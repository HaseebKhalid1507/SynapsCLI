use super::{
    generate_code_challenge, generate_code_verifier, generate_state, now_millis, open_browser,
    save_provider_auth, start_callback_server_at, CallbackOutcome, OAuthCredentials,
};
use reqwest::Client;
use serde::Deserialize;
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
pub async fn discover(client: &Client) -> Result<Discovery, String> {
    let response = client
        .get(DISCOVERY_URL)
        .send()
        .await
        .map_err(|e| format!("xAI discovery failed: {e}"))?;
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
    client: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
    previous: Option<&str>,
    require: bool,
) -> Result<OAuthCredentials, String> {
    validate_endpoint(endpoint)?;
    let response = client
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
        start_callback_server_at(state, "127.0.0.1", CALLBACK_PORT, "/callback").await?;
    let _ = open_browser(&url);
    eprintln!("Open this xAI sign-in URL if needed:\n{url}");
    let outcome = rx.await.map_err(|_| "xAI callback canceled".to_string())?;
    handle.shutdown().await;
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
