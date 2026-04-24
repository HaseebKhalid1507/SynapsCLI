//! OAuth 2.0 Authorization Code + PKCE flow for Anthropic (Claude Pro/Max).
//!
//! Implements the same flow as Claude Code and Pi coding agent:
//! 1. Generate PKCE verifier + challenge
//! 2. Start localhost callback server
//! 3. Open browser to claude.ai/oauth/authorize
//! 4. Capture redirect with auth code
//! 5. Exchange code for access + refresh tokens
//! 6. Save to ~/.pi/agent/auth.json (shared with Pi)

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

mod pkce;
mod callback;
mod token;
mod storage;
mod browser;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use pkce::{generate_code_verifier, generate_code_challenge, generate_state, build_auth_url};
pub use pkce::build_openai_auth_url;
pub use callback::{CallbackServerHandle, start_callback_server};
pub use token::{exchange_code_for_tokens, refresh_token, ensure_fresh_token};
pub use token::{exchange_code_for_tokens_openai, refresh_openai_token, ensure_fresh_openai_token};
pub use storage::{auth_file_path, load_auth, save_auth};
pub use storage::{save_openai_auth, load_openai_auth};
pub use browser::open_browser;

// ── Constants (match Claude Code / Pi) ──────────────────────────────────────

pub(super) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(super) const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub(super) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(super) const CALLBACK_HOST: &str = "127.0.0.1";
pub(super) const CALLBACK_PORT: u16 = 53692;
pub(super) const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// ── OpenAI Constants ──────────────────────────────────────────────────────
pub(super) const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(super) const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(super) const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(super) const OPENAI_CALLBACK_PORT: u16 = 1455;
pub(super) const OPENAI_CALLBACK_PATH: &str = "/auth/callback";
pub(super) const OPENAI_SCOPES: &str = "openid profile email offline_access";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai: Option<OAuthCredentials>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
}

/// Result from the OAuth callback.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

/// Check if the current token is expired (or will expire within 5 minutes).
pub fn is_token_expired(creds: &OAuthCredentials) -> bool {
    now_millis() >= creds.expires
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn now_millis() -> u64 {
    crate::epoch_millis()
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

// ── High-level login flow ───────────────────────────────────────────────────

/// Run the full OAuth login flow. Returns saved credentials.
pub async fn login() -> std::result::Result<OAuthCredentials, String> {
    let port = CALLBACK_PORT;

    // 1. Generate PKCE
    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state = generate_state();

    // 2. Start callback server
    let (rx, server_handle) = start_callback_server(state.clone(), port, "/callback").await?;

    // 3. Build URL and open browser
    let auth_url = build_auth_url(&challenge, &state, port);

    eprintln!("\n\x1b[1mOpening browser to sign in...\x1b[0m\n");

    if let Err(e) = open_browser(&auth_url) {
        eprintln!("Could not open browser automatically: {}", e);
    }

    eprintln!("\x1b[2mIf the browser didn't open, visit this URL:\x1b[0m");
    // Use OSC 8 hyperlink so the entire URL is one clickable link in the terminal
    eprintln!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\n", auth_url, auth_url);

    // Also provide manual paste option
    let (manual_tx, manual_rx) = oneshot::channel::<CallbackResult>();
    let manual_state = state.clone();
    let stdin_task = tokio::spawn(async move {
        eprintln!("\x1b[2mOr paste the authorization code here:\x1b[0m");

        let mut line = String::new();
        let result = tokio::task::spawn_blocking(move || {
            std::io::stdin().read_line(&mut line).ok();
            line.trim().to_string()
        })
        .await;

        if let Ok(input) = result {
            if !input.is_empty() {
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

/// Run the full OAuth login flow for OpenAI (Sign in with ChatGPT).
pub async fn login_openai() -> std::result::Result<OAuthCredentials, String> {
    let port = OPENAI_CALLBACK_PORT;

    // 1. Generate PKCE
    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state = generate_state();

    // 2. Start callback server (with OpenAI callback path)
    let (rx, server_handle) = start_callback_server(state.clone(), port, OPENAI_CALLBACK_PATH).await?;

    // 3. Build URL and open browser
    let auth_url = build_openai_auth_url(&challenge, &state, port);

    eprintln!("\n\x1b[1mOpening browser to sign in with ChatGPT...\x1b[0m\n");

    if let Err(e) = open_browser(&auth_url) {
        eprintln!("Could not open browser automatically: {}", e);
    }

    eprintln!("\x1b[2mIf the browser didn't open, visit this URL:\x1b[0m");
    // Use OSC 8 hyperlink so the entire URL is one clickable link in the terminal
    eprintln!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\n", auth_url, auth_url);

    // Also provide manual paste option
    let (manual_tx, manual_rx) = oneshot::channel::<CallbackResult>();
    let manual_state = state.clone();
    let stdin_task = tokio::spawn(async move {
        eprintln!("\x1b[2mOr paste the authorization code here:\x1b[0m");

        let mut line = String::new();
        let result = tokio::task::spawn_blocking(move || {
            std::io::stdin().read_line(&mut line).ok();
            line.trim().to_string()
        })
        .await;

        if let Ok(input) = result {
            if !input.is_empty() {
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
    let creds = exchange_code_for_tokens_openai(&result.code, &result.state, &verifier, port).await?;

    // 7. Shut down callback server
    server_handle.shutdown().await;

    // 8. Save to auth.json (under the openai key)
    save_openai_auth(&creds)?;

    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    #[test]
    fn test_generate_code_verifier() {
        let verifier = generate_code_verifier();
        assert!(!verifier.is_empty(), "Code verifier should not be empty");
        assert!(verifier.len() > 20, "Code verifier should be longer than 20 characters");
        let verifier2 = generate_code_verifier();
        assert_ne!(verifier, verifier2, "Two calls should produce different verifiers");
    }

    #[test]
    fn test_generate_code_challenge() {
        let verifier = "test_verifier_123";
        let challenge = generate_code_challenge(verifier);
        assert!(!challenge.is_empty(), "Code challenge should not be empty");
        let challenge2 = generate_code_challenge(verifier);
        assert_eq!(challenge, challenge2, "Same verifier should produce same challenge");
        let different_challenge = generate_code_challenge("different_verifier_456");
        assert_ne!(challenge, different_challenge, "Different verifiers should produce different challenges");
    }

    #[test]
    fn test_generate_state() {
        let state = generate_state();
        assert!(!state.is_empty(), "State should not be empty");
        let state2 = generate_state();
        assert_ne!(state, state2, "Two calls should produce different states");
    }

    #[test]
    fn test_build_auth_url() {
        let challenge = "test_challenge";
        let state = "test_state";
        let port = 8080;
        let url = build_auth_url(challenge, state, port);
        assert!(url.contains("claude.ai/oauth/authorize"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains(&format!("code_challenge={}", challenge)));
        assert!(url.contains(&format!("state={}", state)));
        assert!(url.contains("localhost"));
        assert!(url.contains(&port.to_string()));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn test_is_token_expired() {
        let expired_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: 0,
        };
        assert!(is_token_expired(&expired_creds));

        let future_time = now_millis() + 3600000;
        let fresh_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: future_time,
        };
        assert!(!is_token_expired(&fresh_creds));
        assert_eq!(fresh_creds.auth_type, "oauth");
    }

    #[test]
    fn test_pkce_challenge_sha256() {
        let verifier = "test_verifier_string";
        let challenge = generate_code_challenge(verifier);
        
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let expected = URL_SAFE_NO_PAD.encode(hash);
        
        assert_eq!(challenge, expected);
    }

    #[test]
    fn test_code_verifier_length() {
        let verifier = generate_code_verifier();
        assert_eq!(verifier.len(), 43);
    }

    #[test]
    fn test_state_length() {
        let state = generate_state();
        assert_eq!(state.len(), 43);
    }

    #[test]
    fn test_build_auth_url_required_params() {
        let url = build_auth_url("test_challenge", "test_state", 8080);
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope="));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("8080"));
    }

    #[test]
    fn test_is_token_expired_edge_cases() {
        let current_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        
        let exactly_now_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: current_time,
        };
        assert!(is_token_expired(&exactly_now_creds));
        
        let one_ms_future_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: current_time + 1,
        };
        assert!(!is_token_expired(&one_ms_future_creds));
    }

    #[test]
    fn test_auth_file_path() {
        let path = auth_file_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with("auth.json"));
    }

    #[test]
    fn test_oauth_credentials_serialization_roundtrip() {
        let original_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh_token".to_string(),
            access: "test_access_token".to_string(),
            expires: 1234567890,
        };
        
        let json = serde_json::to_string(&original_creds).expect("Should serialize");
        let deserialized_creds: OAuthCredentials = serde_json::from_str(&json).expect("Should deserialize");
        
        assert_eq!(original_creds.auth_type, deserialized_creds.auth_type);
        assert_eq!(original_creds.refresh, deserialized_creds.refresh);
        assert_eq!(original_creds.access, deserialized_creds.access);
        assert_eq!(original_creds.expires, deserialized_creds.expires);
    }
}
