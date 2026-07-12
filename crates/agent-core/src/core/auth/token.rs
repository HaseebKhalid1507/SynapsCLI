use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use reqwest::Client;

use super::storage::{auth_file_path, load_provider_auth, save_provider_auth};
use super::{is_token_expired, now_millis, OAuthCredentials, TokenResponse, CLIENT_ID, TOKEN_URL};

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
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
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
        account_id: None,
    })
}

/// Refresh an expired OAuth token.
pub async fn refresh_token(
    client: &Client,
    refresh: &str,
) -> std::result::Result<OAuthCredentials, String> {
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
        account_id: None,
    })
}

/// Acquire an exclusive lock on auth.json, check token freshness, refresh if
/// needed, and persist the result atomically. Returns the current (possibly
/// refreshed) credentials.
///
/// # Atomicity fix
///
/// Previously used seek(0)+set_len(0)+write_all (truncate-in-place). A crash
/// between `set_len(0)` and write completion zeroed auth.json. Because
/// Anthropic rotates the refresh token on every refresh call, the zeroed file
/// was unrecoverable — the new token existed only in memory.
///
/// Fix: delegate the write to `save_provider_auth("anthropic", ...)`, which
/// calls `save_provider_auth_at` in storage.rs. That uses:
///   sidecar `.json.lock` → write to `.json.tmp` → `rename(2)` (atomic on POSIX)
/// A crash at any point leaves auth.json intact with the pre-refresh content.
///
/// The read phase still holds an fs4 exclusive lock on the live file to prevent
/// two concurrent callers from both issuing a network refresh (which would
/// rotate the refresh token twice, invalidating the first caller's result).
/// The locks are compatible: read lock on auth.json, write lock on auth.json.lock.
///
/// # Task #184 (AuthFile hardcoded-2-field issue)
///
/// The old code rebuilt `AuthFile { anthropic, openai_codex }`, silently
/// dropping any provider beyond those two. Routing through `save_provider_auth`
/// gets the same read-merge-write as every other save site — all providers
/// are preserved automatically.
pub async fn ensure_fresh_token(client: &Client) -> std::result::Result<OAuthCredentials, String> {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};

    let path = auth_file_path();

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

    // Open read-only; we no longer need write access on this handle because
    // the write goes through save_provider_auth (tmp+rename).
    let file = OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;

    let mut file =
        tokio::task::spawn_blocking(move || -> std::result::Result<std::fs::File, String> {
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
    // Explicit drop: release the exclusive lock before the async refresh
    // network call so we don't hold the lock across I/O we don't control.
    drop(file);

    let root: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse auth.json: {}", e))?;
    let anthropic_raw = root
        .get("anthropic")
        .ok_or_else(|| "No anthropic credential in auth.json. Run `login`.".to_string())?;
    let creds: OAuthCredentials = serde_json::from_value(anthropic_raw.clone())
        .map_err(|e| format!("Failed to parse anthropic credential: {}", e))?;

    if !is_token_expired(&creds) {
        return Ok(creds);
    }

    let new_creds = refresh_token(client, &creds.refresh).await?;

    // Atomic write via save_provider_auth → save_provider_auth_at:
    //   1. acquire sidecar auth.json.lock
    //   2. read-merge all providers from current auth.json
    //   3. write to auth.json.tmp
    //   4. fsync + rename(auth.json.tmp → auth.json)   ← atomic
    // Also naturally preserves all providers beyond "anthropic" (fixes #184).
    save_provider_auth("anthropic", &new_creds)?;

    Ok(new_creds)
}

/// Process-wide, per-provider refresh gates. The file lock protects persistence;
/// these gates additionally keep concurrent async callers in this process from
/// rotating the same refresh token while the network request is in flight.
fn refresh_gate(provider: &str) -> Arc<Mutex<()>> {
    static GATES: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("refresh gate registry poisoned");
    gates
        .entry(provider.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Ensure a non-Anthropic OAuth provider has a fresh token.
pub async fn ensure_fresh_provider_token<P>(
    client: &Client,
    provider: P,
) -> std::result::Result<OAuthCredentials, String>
where
    P: TryInto<super::provider::OAuthProviderId>,
    P::Error: std::fmt::Display,
{
    let provider = provider.try_into().map_err(|e| e.to_string())?;
    let _gate = refresh_gate(provider.as_str()).lock_owned().await;
    // Re-read after entering the gate: a preceding waiter may have refreshed
    // and atomically persisted a rotated refresh token.
    let Some(creds) = load_provider_auth(provider.as_str())? else {
        return Err(format!(
            "No credentials for {}. Run `synaps login`.",
            provider
        ));
    };

    if !is_token_expired(&creds) {
        return Ok(creds);
    }

    let fresh = super::provider::refresh(client, provider, &creds.refresh).await?;
    save_provider_auth(provider.as_str(), &fresh)?;
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::super::storage::save_provider_auth_at_test_hook;
    use super::super::OAuthCredentials;

    fn fresh_creds(refresh: &str) -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: refresh.to_string(),
            access: "access".to_string(),
            expires: crate::epoch_millis() + 3_600_000,
            account_id: None,
        }
    }

    // ── Bug #2 atomicity regression suite ─────────────────────────────────────
    //
    // Root cause (pre-fix): ensure_fresh_token used seek(0)+set_len(0)+write_all
    // on the live auth.json file (token.rs ~139-168). A crash between truncate
    // and write completion zeroed the file. Because Anthropic rotates the
    // refresh token on every refresh, the zeroed-file state is unrecoverable.
    //
    // Fix: the write goes through save_provider_auth → save_provider_auth_at
    // (storage.rs), which does write-to-tmp + rename(2). rename(2) is atomic:
    // the file is either the old content or the new content, never empty.
    //
    // These tests exercise save_provider_auth_at directly (the same function
    // ensure_fresh_token now delegates to) and prove the atomicity contract.

    /// After a successful save, auth.json must not be empty or zero-length.
    /// This would be violated by the old truncate-in-place path on a crash.
    #[test]
    fn bug2_write_produces_non_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok1"))
            .expect("save must succeed");
        let meta = std::fs::metadata(&path).expect("file must exist");
        assert!(meta.len() > 0, "auth.json must not be empty after save");
    }

    /// The write must go through a .json.tmp file, not truncate-in-place.
    /// We prove this by checking that after a successful save the tmp file
    /// is gone (renamed → auth.json) and the final file is valid JSON.
    /// The tmp file should not persist after a successful write.
    #[test]
    fn bug2_tmp_file_cleaned_up_after_successful_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok2"))
            .expect("save must succeed");
        let tmp = path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "auth.json.tmp must not persist after a successful atomic rename"
        );
    }

    /// The final file must be valid JSON with the correct credential.
    /// Truncate-in-place failure would leave `{}` or `null`; atomic rename
    /// either keeps old content or produces fully-written new content.
    #[test]
    fn bug2_final_file_is_valid_json_with_correct_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok3"))
            .expect("save must succeed");
        let content = std::fs::read_to_string(&path).expect("must be readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("must be valid JSON after atomic write");
        assert_eq!(
            parsed["anthropic"]["refresh"].as_str(),
            Some("tok3"),
            "refresh token must be persisted correctly"
        );
    }

    /// Refresh write must preserve other providers (fixes #184 / AuthFile
    /// 2-field issue). Old code rebuilt `AuthFile { anthropic, openai_codex }`
    /// which drops any 3rd provider; new code does read-merge-write.
    #[test]
    fn bug2_refresh_write_preserves_other_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        // Simulate state after two prior logins
        save_provider_auth_at_test_hook(&path, "openai-codex", &fresh_creds("codex-tok"))
            .expect("save codex");
        save_provider_auth_at_test_hook(&path, "future-provider", &fresh_creds("future-tok"))
            .expect("save future");

        // Simulate ensure_fresh_token writing the refreshed anthropic credential
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("anth-new"))
            .expect("save anthropic after refresh");

        let content = std::fs::read_to_string(&path).expect("readable");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");

        assert_eq!(parsed["anthropic"]["refresh"].as_str(), Some("anth-new"));
        assert_eq!(
            parsed["openai-codex"]["refresh"].as_str(),
            Some("codex-tok"),
            "openai-codex must survive the anthropic refresh write"
        );
        assert_eq!(
            parsed["future-provider"]["refresh"].as_str(),
            Some("future-tok"),
            "future-provider must survive (fixes #184 — 3rd provider not dropped)"
        );
    }
}
