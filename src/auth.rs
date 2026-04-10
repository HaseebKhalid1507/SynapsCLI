//! OAuth 2.0 Authorization Code + PKCE flow for Anthropic (Claude Pro/Max).
//!
//! Implements the same flow as Claude Code and Pi coding agent:
//! 1. Generate PKCE verifier + challenge
//! 2. Start localhost callback server
//! 3. Open browser to claude.ai/oauth/authorize
//! 4. Capture redirect with auth code
//! 5. Exchange code for access + refresh tokens
//! 6. Save to ~/.pi/agent/auth.json (shared with Pi)

use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use rand::Rng;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::path::{Path, PathBuf};
use tokio::sync::oneshot;
use std::sync::Arc;
use tokio::sync::Mutex;

// ── Constants (match Claude Code / Pi) ──────────────────────────────────────

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 53692;
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub refresh: String,
    pub access: String,
    pub expires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    pub anthropic: OAuthCredentials,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

// ── PKCE ────────────────────────────────────────────────────────────────────

/// Generate a cryptographically random code verifier (43-128 chars, base64url).
pub fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compute S256 code challenge from verifier.
pub fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    URL_SAFE_NO_PAD.encode(hash)
}

/// Generate a random state parameter.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

// ── Auth URL ────────────────────────────────────────────────────────────────

/// Build the full authorize URL for the browser.
pub fn build_auth_url(challenge: &str, state: &str, port: u16) -> String {
    let redirect_uri = format!("http://localhost:{}/callback", port);
    let params = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", &redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];

    let query: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{}?{}", AUTHORIZE_URL, query)
}

// ── Callback Server ─────────────────────────────────────────────────────────

/// Result from the OAuth callback.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

const SUCCESS_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Login Successful</title>
<style>
body { background: #09090b; color: #fafafa; font-family: system-ui; display: flex;
       align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
main { text-align: center; max-width: 480px; }
h1 { font-size: 24px; margin-bottom: 8px; }
p { color: #a1a1aa; }
</style></head>
<body><main>
<h1>✓ Authentication successful</h1>
<p>You can close this window and return to your terminal.</p>
</main></body></html>"#;

const ERROR_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Login Failed</title>
<style>
body { background: #09090b; color: #fafafa; font-family: system-ui; display: flex;
       align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
main { text-align: center; max-width: 480px; }
h1 { font-size: 24px; margin-bottom: 8px; color: #ef4444; }
p { color: #a1a1aa; }
</style></head>
<body><main>
<h1>✗ Authentication failed</h1>
<p>Something went wrong. Please try again.</p>
</main></body></html>"#;

/// Start a temporary HTTP server on localhost that captures the OAuth callback.
/// Returns a oneshot receiver that resolves with the auth code + state.
pub async fn start_callback_server(
    expected_state: String,
    port: u16,
) -> std::result::Result<(oneshot::Receiver<CallbackResult>, CallbackServerHandle), String> {
    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let expected = expected_state.clone();
    let tx_clone = tx.clone();

    let app = axum::Router::new().route(
        "/callback",
        axum::routing::get(move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
            let tx = tx_clone.clone();
            let expected = expected.clone();
            async move {
                let code = query.get("code").cloned();
                let state = query.get("state").cloned();
                let error = query.get("error").cloned();

                if let Some(err) = error {
                    eprintln!("OAuth error from provider: {}", err);
                    return axum::response::Html(ERROR_HTML.to_string());
                }

                let (code, state) = match (code, state) {
                    (Some(c), Some(s)) => (c, s),
                    _ => {
                        return axum::response::Html(ERROR_HTML.to_string());
                    }
                };

                if state != expected {
                    eprintln!("State mismatch: expected {}, got {}", expected, state);
                    return axum::response::Html(ERROR_HTML.to_string());
                }

                // Send the result
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(CallbackResult {
                        code,
                        state,
                    });
                }

                axum::response::Html(SUCCESS_HTML.to_string())
            }
        }),
    );

    let addr = format!("{}:{}", CALLBACK_HOST, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Failed to bind callback server on {}: {}", addr, e))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    Ok((
        rx,
        CallbackServerHandle {
            shutdown: Some(shutdown_tx),
            task: Some(server_handle),
        },
    ))
}

/// Handle to shut down the callback server.
pub struct CallbackServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl CallbackServerHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
        }
    }
}

// ── Token Exchange ──────────────────────────────────────────────────────────

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

    let client = Client::new();
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

// ── Auth File I/O ───────────────────────────────────────────────────────────

/// Get the path to auth.json (~/.synaps-cli/auth.json).
pub fn auth_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".synaps-cli/auth.json")
}

/// Load credentials from auth.json.
pub fn load_auth() -> std::result::Result<Option<AuthFile>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let auth: AuthFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok(Some(auth))
}

/// Save credentials to auth.json.
pub fn save_auth(creds: &OAuthCredentials) -> std::result::Result<(), String> {
    let path = auth_file_path();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let auth = AuthFile {
        anthropic: creds.clone(),
    };

    let json = serde_json::to_string_pretty(&auth)
        .map_err(|e| format!("Failed to serialize auth: {}", e))?;

    std::fs::write(&path, &json)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    // Set file permissions to 600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)
            .map_err(|e| format!("Failed to set permissions on {}: {}", path.display(), e))?;
    }

    Ok(())
}

/// Check if the current token is expired (or will expire within 5 minutes).
pub fn is_token_expired(creds: &OAuthCredentials) -> bool {
    now_millis() >= creds.expires
}

// ── Locked Refresh (Pi-style) ───────────────────────────────────────────────

/// Acquire an exclusive lock on auth.json, check token freshness, refresh if
/// needed, and persist the result. Returns the current (possibly refreshed)
/// credentials.
///
/// This is the Pi-style refresh flow: cross-process safe via flock, with a
/// re-read inside the lock to handle the case where another instance already
/// refreshed while we were waiting for the lock.
///
/// Use this before every API call that needs a valid access token.
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

    // Open for read+write. We need a fresh File handle every call so that
    // flock actually serializes between threads in the same process (flock
    // is per open-file-description on Linux).
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;

    // Acquire exclusive lock. This blocks until available — other processes
    // (and other threads in this process using different File handles) will
    // wait. flock is sync, so run in spawn_blocking.
    let mut file = tokio::task::spawn_blocking(move || -> std::result::Result<std::fs::File, String> {
        FileExt::lock_exclusive(&file)
            .map_err(|e| format!("Failed to lock auth.json: {}", e))?;
        Ok(file)
    })
    .await
    .map_err(|e| format!("Lock task failed: {}", e))??;

    // From here on, we hold the lock until `file` is dropped.
    // Any early return must ensure `file` drops so the lock releases.

    // Re-read the file INSIDE the lock. Another instance may have refreshed
    // between our expiry check and acquiring the lock.
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("Failed to seek auth.json: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read auth.json: {}", e))?;

    let auth: AuthFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse auth.json: {}", e))?;

    // If the token is still fresh (either was never expired, or another
    // instance just refreshed it), return immediately.
    if !is_token_expired(&auth.anthropic) {
        // Lock releases on file drop
        return Ok(auth.anthropic);
    }

    // Token is expired — refresh via API. Hold the lock across the HTTP call
    // so other instances serialize behind us rather than all hitting the
    // token endpoint simultaneously.
    let new_creds = refresh_token(client, &auth.anthropic.refresh).await?;

    // Write new credentials back to the SAME file handle (still locked).
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

    // Preserve 600 permissions (the file was originally chmod'd by save_auth
    // during login, but set_len + write may reset on some filesystems).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    // Lock releases on `file` drop at function return.
    Ok(new_creds)
}

// ── Browser ─────────────────────────────────────────────────────────────────

/// Open a URL in the default browser.
pub fn open_browser(url: &str) -> std::result::Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    Ok(())
}

// ── High-level login flow ───────────────────────────────────────────────────

/// Run the full OAuth login flow. Returns saved credentials.
pub async fn login() -> std::result::Result<OAuthCredentials, String> {
    let port = CALLBACK_PORT;

    // 1. Generate PKCE
    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state = generate_state();

    // 2. Start callback server
    let (rx, server_handle) = start_callback_server(state.clone(), port).await?;

    // 3. Build URL and open browser
    let auth_url = build_auth_url(&challenge, &state, port);

    eprintln!("\n\x1b[1mOpening browser to sign in...\x1b[0m\n");

    if let Err(e) = open_browser(&auth_url) {
        eprintln!("Could not open browser automatically: {}", e);
    }

    eprintln!("\x1b[2mIf the browser didn't open, visit this URL:\x1b[0m");
    eprintln!("\x1b[36m{}\x1b[0m\n", auth_url);

    // Also provide manual paste option
    // Spawn a task to read stdin for manual code entry
    let (manual_tx, manual_rx) = oneshot::channel::<CallbackResult>();
    let manual_state = state.clone();
    let stdin_task = tokio::spawn(async move {
        eprintln!("\x1b[2mOr paste the authorization code here:\x1b[0m");

        let mut line = String::new();
        // Use blocking stdin read in a spawn_blocking
        let result = tokio::task::spawn_blocking(move || {
            std::io::stdin().read_line(&mut line).ok();
            line.trim().to_string()
        })
        .await;

        if let Ok(input) = result {
            if !input.is_empty() {
                // Try to parse as URL or raw code
                let (code, parsed_state) = parse_manual_input(&input);
                if let Some(code) = code {
                    let _ = manual_tx.send(CallbackResult {
                        code,
                        state: parsed_state.unwrap_or(manual_state),
                    });
                }
            }
        }
    });

    // 4. Wait for either callback or manual input
    let result = tokio::select! {
        callback = rx => {
            match callback {
                Ok(result) => result,
                Err(_) => return Err("Callback channel closed".to_string()),
            }
        }
        manual = manual_rx => {
            match manual {
                Ok(result) => result,
                Err(_) => return Err("Manual input channel closed".to_string()),
            }
        }
    };

    stdin_task.abort();

    // 5. Verify state
    if result.state != state {
        server_handle.shutdown().await;
        return Err("OAuth state mismatch — possible CSRF attack".to_string());
    }

    eprintln!("\n\x1b[1mExchanging code for tokens...\x1b[0m");

    // 6. Exchange code for tokens
    let creds = exchange_code_for_tokens(&result.code, &result.state, &verifier, port).await?;

    // 7. Shut down callback server
    server_handle.shutdown().await;

    // 8. Save to auth.json
    save_auth(&creds)?;

    Ok(creds)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Try to parse manual input as a redirect URL or raw code.
fn parse_manual_input(input: &str) -> (Option<String>, Option<String>) {
    let trimmed = input.trim();

    // Try as full URL
    if let Ok(url) = url::Url::parse(trimmed) {
        let code = url.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.to_string());
        let state = url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.to_string());
        if code.is_some() {
            return (code, state);
        }
    }

    // Try as "code#state" format (Claude Code manual flow)
    if trimmed.contains('#') {
        let parts: Vec<&str> = trimmed.splitn(2, '#').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return (Some(parts[0].to_string()), Some(parts[1].to_string()));
        }
    }

    // Treat as raw code
    if !trimmed.is_empty() {
        return (Some(trimmed.to_string()), None);
    }

    (None, None)
}
