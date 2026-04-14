use reqwest::Client;

use super::{OAuthCredentials, AuthFile, TokenResponse, now_millis, is_token_expired, CLIENT_ID, TOKEN_URL};
use super::storage::auth_file_path;

/// Exchange an authorization code for access + refresh tokens.
pub async fn exchange_code_for_tokens(
    code: &str,
    state: &str,
    verifier: &str,
    port: u16,
) -> std::result::Result<OAuthCredentials, String> {
    let redirect_uri = format!("http://localhost:{}/callback", port);

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });

    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Token exchange request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed ({}): {}", status, text));
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))?;

    // expires_in is seconds; store as epoch millis with 5-minute buffer (matches Pi/Claude Code)
    let expires = now_millis() + (token_resp.expires_in * 1000) - (5 * 60 * 1000);

    Ok(OAuthCredentials {
        auth_type: "oauth".to_string(),
        refresh: token_resp.refresh_token,
        access: token_resp.access_token,
        expires,
    })
}

/// Refresh an expired OAuth token.
pub async fn refresh_token(client: &Client, refresh: &str) -> std::result::Result<OAuthCredentials, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Token refresh request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed ({}): {}", status, text));
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse refresh response: {}", e))?;

    let expires = now_millis() + (token_resp.expires_in * 1000) - (5 * 60 * 1000);

    Ok(OAuthCredentials {
        auth_type: "oauth".to_string(),
        refresh: token_resp.refresh_token,
        access: token_resp.access_token,
        expires,
    })
}

/// Acquire an exclusive lock on auth.json, check token freshness, refresh if
/// needed, and persist the result. Returns the current (possibly refreshed)
/// credentials.
pub async fn ensure_fresh_token(client: &Client) -> std::result::Result<OAuthCredentials, String> {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    let path = auth_file_path();

    // Ensure parent dir exists (first-run case where we're reading before login)
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    if !path.exists() {
        return Err(format!(
            "No credentials at {}. Run `login` to authenticate.",
            path.display()
        ));
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;

    let mut file = tokio::task::spawn_blocking(move || -> std::result::Result<std::fs::File, String> {
        FileExt::lock_exclusive(&file)
            .map_err(|e| format!("Failed to lock auth.json: {}", e))?;
        Ok(file)
    })
    .await
    .map_err(|e| format!("Lock task failed: {}", e))??;

    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("Failed to seek auth.json: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read auth.json: {}", e))?;

    let auth: AuthFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse auth.json: {}", e))?;

    if !is_token_expired(&auth.anthropic) {
        return Ok(auth.anthropic);
    }

    let new_creds = refresh_token(client, &auth.anthropic.refresh).await?;

    let new_auth = AuthFile {
        anthropic: new_creds.clone(),
    };
    let new_json = serde_json::to_string_pretty(&new_auth)
        .map_err(|e| format!("Failed to serialize auth: {}", e))?;

    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("Failed to seek for write: {}", e))?;
    file.set_len(0)
        .map_err(|e| format!("Failed to truncate auth.json: {}", e))?;
    file.write_all(new_json.as_bytes())
        .map_err(|e| format!("Failed to write auth.json: {}", e))?;
    file.sync_all()
        .map_err(|e| format!("Failed to fsync auth.json: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(new_creds)
}
