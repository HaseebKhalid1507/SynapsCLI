use super::{
    generate_code_challenge, generate_code_verifier, generate_state, now_millis, open_browser,
    save_provider_auth, start_callback_server_at, CallbackOutcome, OAuthCredentials,
};
use reqwest::Client;
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use url::Url;

pub const ISSUER: &str = "https://auth.x.ai";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
pub const REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
pub const CALLBACK_PORT: u16 = 56121;
const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

pub fn validate_discovery(d: Discovery) -> Result<Discovery, String> {
    if d.issuer != ISSUER {
        return Err("xAI discovery issuer mismatch".into());
    }
    validate_endpoint(&d.authorization_endpoint)?;
    validate_endpoint(&d.token_endpoint)?;
    Ok(d)
}
fn validate_endpoint(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|_| "invalid xAI discovery endpoint")?;
    let host = url.host_str().ok_or("xAI discovery endpoint has no host")?;
    if url.scheme() != "https" || !(host == "x.ai" || host.ends_with(".x.ai")) {
        return Err("untrusted xAI discovery endpoint".into());
    }
    Ok(())
}
pub async fn discover(_client: &Client) -> Result<Discovery, String> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if validate_endpoint(attempt.url().as_str()).is_ok() && attempt.previous().len() < 3 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|e| format!("xAI HTTP client failed: {e}"))?;
    let response = client
        .get(DISCOVERY_URL)
        .send()
        .await
        .map_err(|e| format!("xAI discovery failed: {e}"))?;
    validate_endpoint(response.url().as_str())?;
    if !response.status().is_success() {
        return Err(format!("xAI discovery failed: HTTP {}", response.status()));
    }
    validate_discovery(
        response
            .json()
            .await
            .map_err(|e| format!("invalid xAI discovery: {e}"))?,
    )
}
pub fn authorize_url(
    endpoint: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
) -> Result<String, String> {
    validate_endpoint(endpoint)?;
    let mut url = Url::parse(endpoint).map_err(|_| "invalid authorization endpoint")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("nonce", nonce);
    Ok(url.into())
}
fn credentials(
    token: TokenResponse,
    previous_refresh: Option<&str>,
    require_refresh: bool,
) -> Result<OAuthCredentials, String> {
    if token.access_token.trim().is_empty() {
        return Err("xAI token response omitted access_token".into());
    }
    let refresh = token
        .refresh_token
        .filter(|v| !v.trim().is_empty())
        .or_else(|| previous_refresh.map(str::to_owned));
    if require_refresh && refresh.is_none() {
        return Err("xAI login response omitted refresh_token".into());
    }
    let expires = now_millis()
        .saturating_add(token.expires_in.saturating_mul(1000))
        .saturating_sub(EXPIRY_SKEW_MS);
    Ok(OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: refresh.unwrap_or_default(),
        access: token.access_token,
        expires,
        account_id: None,
    })
}
async fn token_post(
    _client: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
    previous: Option<&str>,
    require: bool,
) -> Result<OAuthCredentials, String> {
    validate_endpoint(endpoint)?;
    // OAuth secrets must never be replayed to a redirect target.
    let no_redirect_client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("xAI HTTP client failed: {e}"))?;
    let response = no_redirect_client
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("xAI token request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "xAI token request failed: HTTP {}",
            response.status()
        ));
    }
    credentials(
        response
            .json()
            .await
            .map_err(|e| format!("invalid xAI token response: {e}"))?,
        previous,
        require,
    )
}
pub async fn refresh_token(client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
    let d = discover(client).await?;
    token_post(
        client,
        &d.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh),
        ],
        Some(refresh),
        false,
    )
    .await
}
fn parse_pasted_callback(input: &str, expected_state: &str) -> Option<super::CallbackResult> {
    let url = Url::parse(input.trim()).ok()?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port_or_known_default() != Some(CALLBACK_PORT)
        || url.path() != "/callback"
        || url.fragment().is_some()
    {
        return None;
    }
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let code = query.get("code")?.to_owned();
    let state = query.get("state")?.to_owned();
    if code.is_empty() || state != expected_state {
        return None;
    }
    Some(super::CallbackResult { code, state })
}

async fn wait_for_callback<R>(
    mut callback: tokio::sync::oneshot::Receiver<CallbackOutcome>,
    input: R,
    expected_state: &str,
) -> Result<CallbackOutcome, String>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = input.lines();
    loop {
        tokio::select! {
            result = &mut callback => {
                return result.map_err(|_| "xAI callback canceled".to_string());
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(callback) = parse_pasted_callback(&line, expected_state) {
                            return Ok(CallbackOutcome::Authorized(callback));
                        }
                        eprintln!("Ignoring invalid callback URL; waiting for browser callback.");
                    }
                    // EOF or an unavailable stdin disables only the manual fallback. The
                    // browser listener remains authoritative and must keep running.
                    Ok(None) | Err(_) => return callback.await.map_err(|_| "xAI callback canceled".to_string()),
                }
            }
        }
    }
}

pub async fn login() -> Result<OAuthCredentials, String> {
    let client = Client::new();
    let d = discover(&client).await?;
    let verifier = generate_code_verifier();
    let state = generate_state();
    let nonce = generate_state();
    let url = authorize_url(
        &d.authorization_endpoint,
        &generate_code_challenge(&verifier),
        &state,
        &nonce,
    )?;
    let (rx, handle) =
        start_callback_server_at(state.clone(), "127.0.0.1", CALLBACK_PORT, "/callback").await?;
    let _ = open_browser(&url);
    eprintln!("Complete xAI sign-in in your browser. If the callback cannot connect, paste the full callback URL here.");
    let outcome =
        wait_for_callback(rx, tokio::io::BufReader::new(tokio::io::stdin()), &state).await;
    handle.shutdown().await;
    let outcome = outcome?;
    let callback = match outcome {
        CallbackOutcome::Authorized(v) => v,
        CallbackOutcome::Denied { error, description } => {
            return Err(format!(
                "OAuth denied: {}{}",
                error,
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ))
        }
        CallbackOutcome::Invalid => return Err("Invalid OAuth callback state".into()),
    };
    let creds = token_post(
        &client,
        &d.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &callback.code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", &verifier),
        ],
        None,
        true,
    )
    .await?;
    save_provider_auth("xai-auth", &creds)?;
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_rejects_http_and_foreign_hosts() {
        for endpoint in [
            "http://auth.x.ai/token",
            "https://evil.example/token",
            "https://x.ai.evil/token",
        ] {
            assert!(validate_discovery(Discovery {
                issuer: ISSUER.into(),
                authorization_endpoint: "https://auth.x.ai/authorize".into(),
                token_endpoint: endpoint.into()
            })
            .is_err());
        }
    }
    #[test]
    fn authorize_has_exact_metadata() {
        let u = Url::parse(
            &authorize_url("https://auth.x.ai/authorize", "challenge", "state", "nonce").unwrap(),
        )
        .unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["scope"], SCOPES);
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "state");
        assert_eq!(q["nonce"], "nonce");
    }
    #[test]
    fn pasted_callback_is_strict_and_state_bound() {
        assert!(parse_pasted_callback("bare-code", "s").is_none());
        assert!(
            parse_pasted_callback("http://localhost:56121/callback?code=c&state=s", "s").is_none()
        );
        assert!(
            parse_pasted_callback("http://127.0.0.1:56121/callback?code=c&state=wrong", "s")
                .is_none()
        );
        assert_eq!(
            parse_pasted_callback("http://127.0.0.1:56121/callback?code=c&state=s", "s")
                .unwrap()
                .code,
            "c"
        );
    }

    #[tokio::test]
    async fn eof_and_invalid_paste_do_not_cancel_browser_callback() {
        for input in ["", "bare-code\n"] {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let input = tokio::io::BufReader::new(std::io::Cursor::new(input.as_bytes()));
            let waiter = tokio::spawn(async move { wait_for_callback(rx, input, "s").await });
            tokio::task::yield_now().await;
            tx.send(CallbackOutcome::Authorized(super::super::CallbackResult {
                code: "browser".into(),
                state: "s".into(),
            }))
            .unwrap();
            assert!(matches!(
                waiter.await.unwrap().unwrap(),
                CallbackOutcome::Authorized(result) if result.code == "browser"
            ));
        }
    }

    #[tokio::test]
    async fn valid_pasted_url_still_completes_and_drops_callback_receiver() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pasted = "http://127.0.0.1:56121/callback?code=pasted&state=s\n";
        let outcome = wait_for_callback(
            rx,
            tokio::io::BufReader::new(std::io::Cursor::new(pasted.as_bytes())),
            "s",
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CallbackOutcome::Authorized(result) if result.code == "pasted"));
        assert!(tx.send(CallbackOutcome::Invalid).is_err());
    }

    #[test]
    fn refresh_omission_preserves_previous() {
        let c = credentials(
            TokenResponse {
                access_token: "new".into(),
                refresh_token: None,
                expires_in: 3600,
            },
            Some("old"),
            false,
        )
        .unwrap();
        assert_eq!(c.refresh, "old");
    }
    #[test]
    fn login_requires_refresh_and_access() {
        assert!(credentials(
            TokenResponse {
                access_token: "a".into(),
                refresh_token: None,
                expires_in: 1
            },
            None,
            true
        )
        .is_err());
        assert!(credentials(
            TokenResponse {
                access_token: "".into(),
                refresh_token: Some("r".into()),
                expires_in: 1
            },
            None,
            true
        )
        .is_err());
    }
}
