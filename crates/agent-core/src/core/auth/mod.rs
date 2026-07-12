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

pub mod aws_bedrock;
pub mod azure_openai;
pub mod broker;
mod browser;
mod callback;
pub mod cloud;
pub mod cloud_login;
mod credential_source;
pub mod github_copilot;
pub mod google_gemini;
pub mod google_vertex;
mod openai_codex;
mod pkce;
pub mod provider;
pub mod providers;
pub mod static_providers;
mod storage;
mod token;
mod xai;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use broker::{
    broker_from_source, global_broker, set_global_broker, AccessToken, BrokerError,
    CredentialBroker, CredentialKind, LocalBroker, ProviderStatus, ProxyByteStream, ProxyMethod,
    ProxyRequest, ProxyResponse, RemoteBroker, StaticKeyStatus, MAX_PROXY_REQUEST_BYTES,
    MAX_PROXY_RESPONSE_BYTES, MAX_UPSTREAM_ERROR_BYTES, PROXY_REQUEST_TIMEOUT,
};
pub use browser::open_browser;
pub use callback::{
    start_callback_server, start_callback_server_at, CallbackOutcome, CallbackServerHandle,
};
pub use cloud::{
    AuthIdentity, AwsBedrockConfig, AzureOpenAiConfig, BrokerMessage, BrokerOperation, BrokerTool,
    CloudProviderId, GoogleVertexConfig, InvokeOptions, InvokeRequest, MessageRole, ProviderId,
};
pub use credential_source::{
    is_expired_with_margin, resolve_access_token, resolve_remote, resolve_remote_token,
    BrokerClient, BrokerToken, CredentialSource, TokenCache, TokenFetcher, DEFAULT_MARGIN_MS,
};
pub use github_copilot::login as login_github_copilot;
pub use openai_codex::{
    extract_account_id as extract_codex_account_id, login as login_openai_codex,
};
pub use pkce::{build_auth_url, generate_code_challenge, generate_code_verifier, generate_state};
pub use provider::{
    BrokerCredentialStrategy, OAuthProviderDescriptor, OAuthProviderId, OAuthProviderRegistry,
};
pub use static_providers::{static_provider, StaticProviderSpec, LOCAL_PROVIDER_KEY};
pub use storage::{
    auth_file_path, load_auth, load_cloud_state, load_provider_auth, load_static_key, save_auth,
    save_cloud_state, save_provider_auth, save_static_key,
};
pub use token::{
    ensure_fresh_provider_token, ensure_fresh_token, exchange_code_for_tokens, refresh_token,
};
pub use xai::login as login_xai;

// ── Constants (match Claude Code / Pi) ──────────────────────────────────────

pub(super) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(super) const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub(super) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(super) const CALLBACK_HOST: &str = "127.0.0.1";
pub(super) const CALLBACK_PORT: u16 = 53692;
pub(super) const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    pub anthropic: OAuthCredentials,
    #[serde(
        rename = "openai-codex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_codex: Option<OAuthCredentials>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
}

/// Result from the OAuth callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

/// Check if the current token is expired.
///
/// Note the effective ~5-minute safety margin lives in the stored value, not
/// here: `refresh_token`/`exchange_code_for_tokens` bake a 5-minute buffer into
/// `expires` (`now + expires_in*1000 - 5min`). So a bare `now >= expires` check
/// already fires ~5 minutes before the credential actually dies at the provider.
pub fn is_token_expired(creds: &OAuthCredentials) -> bool {
    now_millis() >= creds.expires
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn now_millis() -> u64 {
    crate::epoch_millis()
}

/// Try to parse manual input as a redirect URL or code#state shorthand.
///
/// Returns `(Some(code), Some(state))` only when both components are present
/// and unambiguous. The `None` state path has been intentionally removed:
/// previously the caller defaulted a missing state to the expected value
/// (`parsed_state.unwrap_or(manual_state)`), which made the downstream
/// `result.state != state` check compare a value against itself — CSRF
/// protection silently nullified. By never returning `Some(code)` without
/// `Some(state)`, the CSRF guard is guaranteed to do real work.
fn parse_manual_input(input: &str) -> (Option<String>, Option<String>) {
    let trimmed = input.trim();

    // Try as full URL (e.g. the redirect from the browser)
    if let Ok(url) = url::Url::parse(trimmed) {
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string());
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string());
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

    // Bare code with no state: REJECTED. Accepting it and defaulting the state
    // to `manual_state` would cause the CSRF check to compare a value to itself.
    // The caller (`manual_paste_to_callback`) will return `None` for this input.
    (None, None)
}

/// Validate user-pasted OAuth authorization input for the Anthropic manual
/// fallback flow.
///
/// Returns `Some(CallbackResult)` only if the input contains BOTH a `code`
/// and a `state`. Previously the caller defaulted a missing `state` to the
/// original CSRF token (`parsed_state.unwrap_or(manual_state)`), which
/// silently bypassed the CSRF check — the state comparison became
/// `manual_state == state` (always true). By requiring an explicit state
/// here, a bare code paste or a URL without `state` is rejected outright.
fn manual_paste_to_callback(input: &str) -> Option<CallbackResult> {
    let (code, state) = parse_manual_input(input);
    Some(CallbackResult {
        code: code?,
        state: state?,
    })
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
    let (manual_tx, manual_rx) = oneshot::channel::<CallbackResult>();
    let stdin_task = tokio::spawn(async move {
        eprintln!("\x1b[2mOr paste the redirect URL here (must include `state`):\x1b[0m");

        let mut line = String::new();
        let result = tokio::task::spawn_blocking(move || {
            std::io::stdin().read_line(&mut line).ok();
            line.trim().to_string()
        })
        .await;

        if let Ok(input) = result {
            match manual_paste_to_callback(&input) {
                Some(callback) => {
                    let _ = manual_tx.send(callback);
                }
                None if !input.is_empty() => {
                    eprintln!(
                        "\x1b[31m✗ Pasted input did not contain both `code` and `state`.\x1b[0m"
                    );
                    eprintln!(
                        "\x1b[2m  Paste the full redirect URL (e.g. http://localhost:53692/callback?code=…&state=…).\x1b[0m"
                    );
                }
                None => {}
            }
        }
    });

    // 4. Wait for either callback or manual input
    let result = tokio::select! {
        callback = rx => {
            match callback {
                Ok(CallbackOutcome::Authorized(result)) => result,
                Ok(CallbackOutcome::Denied { error, description }) => return Err(format!("OAuth denied: {}{}", error, description.map(|d| format!(": {d}")).unwrap_or_default())),
                Ok(CallbackOutcome::Invalid) => return Err("Invalid OAuth callback".to_string()),
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

// ── Anthropic-specific CSRF regression tests ─────────────────────────────────
//
// See also: openai_codex::tests for the parallel codex test suite.
// These mirror the codex tests — same invariants, same naming pattern.

/// Pinned experimental Copilot models base URL for broker catalog proxy.
pub(crate) fn github_copilot_models_base_url() -> &'static str {
    "https://api.githubcopilot.com"
}

/// Headers for the experimental Copilot models GET (no secrets).
pub(crate) fn github_copilot_models_request_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("User-Agent", "SynapsCLI/0.6.0"),
        ("Accept", "application/json"),
        ("Editor-Version", "vscode/1.107.0"),
        ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
        ("Copilot-Integration-Id", "vscode-chat"),
        ("X-Github-Api-Version", "2025-10-01"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    #[test]
    fn test_generate_code_verifier() {
        let verifier = generate_code_verifier();
        assert!(!verifier.is_empty(), "Code verifier should not be empty");
        assert!(
            verifier.len() > 20,
            "Code verifier should be longer than 20 characters"
        );
        let verifier2 = generate_code_verifier();
        assert_ne!(
            verifier, verifier2,
            "Two calls should produce different verifiers"
        );
    }

    #[test]
    fn test_generate_code_challenge() {
        let verifier = "test_verifier_123";
        let challenge = generate_code_challenge(verifier);
        assert!(!challenge.is_empty(), "Code challenge should not be empty");
        let challenge2 = generate_code_challenge(verifier);
        assert_eq!(
            challenge, challenge2,
            "Same verifier should produce same challenge"
        );
        let different_challenge = generate_code_challenge("different_verifier_456");
        assert_ne!(
            challenge, different_challenge,
            "Different verifiers should produce different challenges"
        );
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
            account_id: None,
        };
        assert!(is_token_expired(&expired_creds));

        let future_time = now_millis() + 3600000;
        let fresh_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: future_time,
            account_id: None,
        };
        assert!(!is_token_expired(&fresh_creds));
        assert_eq!(fresh_creds.auth_type, "oauth");
    }

    #[test]
    fn test_pkce_challenge_sha256() {
        let verifier = "test_verifier_string";
        let challenge = generate_code_challenge(verifier);

        use sha2::{Digest, Sha256};
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
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let exactly_now_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: current_time,
            account_id: None,
        };
        assert!(is_token_expired(&exactly_now_creds));

        let one_ms_future_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "test_refresh".to_string(),
            access: "test_access".to_string(),
            expires: current_time + 1,
            account_id: None,
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
            account_id: None,
        };

        let json = serde_json::to_string(&original_creds).expect("Should serialize");
        let deserialized_creds: OAuthCredentials =
            serde_json::from_str(&json).expect("Should deserialize");

        assert_eq!(original_creds.auth_type, deserialized_creds.auth_type);
        assert_eq!(original_creds.refresh, deserialized_creds.refresh);
        assert_eq!(original_creds.access, deserialized_creds.access);
        assert_eq!(original_creds.expires, deserialized_creds.expires);
    }

    // ── manual_paste_to_callback: CSRF guard on the Anthropic manual paste path ──
    //
    // Mirrors the regression suite in openai_codex::tests (manual_paste_to_callback).
    // Pre-fix: parse_manual_input returned (Some(code), None) for bare codes,
    // and the caller did `parsed_state.unwrap_or(manual_state)`, making the
    // CSRF check `manual_state == state` — always true — bypass.
    // Fix: removed bare-code fallback from parse_manual_input + extracted
    // manual_paste_to_callback that requires both code AND state.

    #[test]
    fn anthropic_manual_paste_accepts_full_redirect_url() {
        let result = manual_paste_to_callback("http://localhost:53692/callback?code=abc&state=xyz")
            .expect("URL with code+state must be accepted");
        assert_eq!(result.code, "abc");
        assert_eq!(result.state, "xyz");
    }

    #[test]
    fn anthropic_manual_paste_rejects_bare_code() {
        // CSRF regression: a bare code with no state was previously defaulted
        // to `manual_state`, making the CSRF check trivially true.
        assert!(
            manual_paste_to_callback("abc123_bare_code").is_none(),
            "bare code with no state must be rejected — was the root of the CSRF bypass"
        );
    }

    #[test]
    fn anthropic_manual_paste_rejects_url_without_state() {
        assert!(
            manual_paste_to_callback("http://localhost:53692/callback?code=abc").is_none(),
            "URL missing `state` must be rejected — would bypass CSRF check"
        );
    }

    #[test]
    fn anthropic_manual_paste_accepts_code_hash_state() {
        // "code#state" is the Claude Code shorthand; both components explicit.
        let result = manual_paste_to_callback("mycode#mystate")
            .expect("code#state shorthand must be accepted");
        assert_eq!(result.code, "mycode");
        assert_eq!(result.state, "mystate");
    }

    #[test]
    fn anthropic_manual_paste_rejects_empty_input() {
        assert!(manual_paste_to_callback("").is_none());
        assert!(manual_paste_to_callback("   ").is_none());
    }

    #[test]
    fn anthropic_manual_paste_rejects_url_with_only_state() {
        assert!(
            manual_paste_to_callback("http://localhost:53692/callback?state=xyz").is_none(),
            "URL missing `code` must be rejected"
        );
    }
}
