//! Google Gemini (Code Assist) OAuth: authorization-code + PKCE.
//!
//! Experimental. The user-facing endpoints (`accounts.google.com/o/oauth2/v2/auth`,
//! `oauth2.googleapis.com/token`) are official Google OAuth 2.0 surfaces. The
//! Code Assist runtime host (`cloudcode-pa.googleapis.com/v1internal`) that
//! consumes the resulting access token is a product-client-observed integration
//! surface and is treated as **experimental** — it is not described as a stable
//! public third-party API.
//!
//! Credential mapping:
//! - `OAuthCredentials.refresh` = long-lived Google refresh token (broker-owned only)
//! - `OAuthCredentials.access`  = short-lived Google access token
//! - `OAuthCredentials.expires` = access-token expiry (ms, with skew)
//!
//! The long-lived refresh token must never be vended, logged, or placed in
//! broker wire types. This module operates only inside the auth boundary.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::oneshot;
use url::Url;

use super::{
    generate_code_challenge, generate_code_verifier, generate_state, now_millis,
    save_provider_auth, start_callback_server_at, CallbackOutcome, OAuthCredentials,
};

// ── Pinned endpoints ─────────────────────────────────────────────────────────

/// Canonical storage / broker id.
pub const PROVIDER: &str = "google-gemini";

/// Google installed-app OAuth authorization endpoint (RFC 6749).
pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google OAuth 2.0 token endpoint.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google userinfo endpoint (for optional post-auth account labeling).
pub const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Space-separated scopes. Same values as the official Gemini CLI reference:
/// cloud-platform (Code Assist), userinfo.email, userinfo.profile.
pub const SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/cloud-platform",
    " https://www.googleapis.com/auth/userinfo.email",
    " https://www.googleapis.com/auth/userinfo.profile",
);

/// Environment variable containing the Synaps-owned Google Desktop OAuth
/// client registration used for Gemini Code Assist.
pub const CLIENT_ID_ENV: &str = "SYNAPS_GOOGLE_GEMINI_CLIENT_ID";

/// Optional installed-app client value paired with `CLIENT_ID_ENV`. Desktop
/// registrations are public clients; this value is configuration, not a
/// confidential secret, but it still must not be logged or committed here.
pub const CLIENT_SECRET_ENV: &str = "SYNAPS_GOOGLE_GEMINI_CLIENT_SECRET";

/// Loopback callback host; RFC 8252 § 7.3 mandates a literal IP for installed
/// apps. Google explicitly rejects `localhost` for some client types.
pub const CALLBACK_HOST: &str = "127.0.0.1";

/// Callback path — pinned distinct from the shared `/callback` to avoid state
/// bleed with the other loopback providers.
pub const CALLBACK_PATH: &str = "/oauth2callback";

/// Default loopback port for the Gemini callback listener.
pub const CALLBACK_PORT: u16 = 45289;

/// Expiry safety-margin subtracted from `now + expires_in`.
pub const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;

/// Connect timeout for auth HTTP.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Full request timeout for auth HTTP.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum response body bytes we will accept from Google OAuth endpoints.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

// ── Login / refresh — production entrypoints ─────────────────────────────────

/// Interactive login against the production Google OAuth endpoints.
pub async fn login() -> Result<OAuthCredentials, String> {
    let registration = GeminiRegistration::production()?;
    login_with_registration(
        CALLBACK_PORT,
        TOKEN_URL,
        /* allow_http_token_endpoint = */ false,
        registration,
        |auth_url| {
            eprintln!(
                "\n\x1b[1mOpening browser for Google Gemini (Code Assist) sign-in...\x1b[0m\n"
            );
            if let Err(e) = super::open_browser(auth_url) {
                eprintln!("Could not open browser automatically: {e}");
            }
            eprintln!("\x1b[2mIf the browser didn't open, visit:\x1b[0m");
            eprintln!("\x1b[36m{auth_url}\x1b[0m\n");
            Ok(())
        },
    )
    .await
}

pub struct GeminiRegistration {
    client_id: String,
    client_secret: Option<String>,
}

impl std::fmt::Debug for GeminiRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiRegistration")
            .field("client_id", &"[configured]")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[configured]"),
            )
            .finish()
    }
}

impl GeminiRegistration {
    pub fn production() -> Result<Self, String> {
        let client_id = std::env::var(CLIENT_ID_ENV).unwrap_or_default();
        let client_secret = std::env::var(CLIENT_SECRET_ENV).ok();
        Self::validated(client_id, client_secret)
    }

    #[doc(hidden)]
    pub fn test(client_id: &str, client_secret: Option<&str>) -> Result<Self, String> {
        Self::validated(client_id.to_owned(), client_secret.map(str::to_owned))
    }

    fn validated(client_id: String, client_secret: Option<String>) -> Result<Self, String> {
        let client_id = client_id.trim().to_owned();
        if client_id.is_empty() {
            return Err(format!(
                "registration_required: configure {CLIENT_ID_ENV} with a Synaps-owned Google Desktop OAuth client ID"
            ));
        }
        if client_id.len() > 512 || client_id.chars().any(char::is_control) {
            return Err("google-gemini: invalid OAuth client registration".into());
        }
        let client_secret = client_secret
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if client_secret
            .as_ref()
            .is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control))
        {
            return Err("google-gemini: invalid OAuth client registration".into());
        }
        Ok(Self {
            client_id,
            client_secret,
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn client_secret(&self) -> Option<&str> {
        self.client_secret.as_deref()
    }
}

/// Interactive login with injectable seams — pinned to production URLs by
/// default; a test harness may override the token endpoint (and set
/// `allow_http_token_endpoint = true`) to talk to a loopback OAuth fake.
///
/// The `browser` closure is invoked with the full authorization URL. In
/// production it opens the user's browser; in tests it drives the loopback
/// callback directly. Returning `Err` cancels the flow immediately.
pub async fn login_with_registration<F>(
    port: u16,
    token_endpoint: &str,
    allow_http_token_endpoint: bool,
    registration: GeminiRegistration,
    browser: F,
) -> Result<OAuthCredentials, String>
where
    F: FnOnce(&str) -> Result<(), String> + Send + 'static,
{
    // Endpoint policy: production callers pass TOKEN_URL and must satisfy the
    // Google-host allowlist. Tests set `allow_http_token_endpoint` to permit a
    // loopback http endpoint; that path is never reachable through login().
    if allow_http_token_endpoint {
        let url = Url::parse(token_endpoint)
            .map_err(|_| GeminiAuthError::UntrustedEndpoint.into_secret_safe())?;
        let host = url.host_str().unwrap_or("");
        if !(url.scheme() == "http" && (host == "127.0.0.1" || host == "localhost")) {
            return Err(GeminiAuthError::UntrustedEndpoint.into_secret_safe());
        }
    } else {
        validate_google_https_endpoint(token_endpoint)
            .map_err(GeminiAuthError::into_secret_safe)?;
    }

    let verifier = generate_code_verifier();
    let challenge = generate_code_challenge(&verifier);
    let state = generate_state();

    let (rx, handle) = start_callback_server_at(state.clone(), CALLBACK_HOST, port, CALLBACK_PATH)
        .await
        .map_err(|e| format!("google-gemini: callback server failed: {e}"))?;

    let auth_url = build_authorize_url(&registration, &challenge, &state, port);
    if let Err(e) = browser(&auth_url) {
        handle.shutdown().await;
        return Err(format!("google-gemini: browser step failed: {e}"));
    }

    let outcome = wait_for_callback(rx, &state).await;
    handle.shutdown().await;
    let callback = match outcome? {
        CallbackOutcome::Authorized(c) => c,
        CallbackOutcome::Denied {
            error,
            description: _,
        } => {
            // Never surface `description` — it's attacker-controlled content.
            return Err(format!("google-gemini: OAuth denied ({error})"));
        }
        CallbackOutcome::Invalid => {
            return Err("google-gemini: invalid OAuth callback state".into());
        }
    };

    // Defensive: the callback server itself enforces state equality, but a
    // regression there must still be caught here (mirrors the Anthropic and
    // xAI patterns).
    if callback.state != state {
        return Err("google-gemini: OAuth state mismatch — possible CSRF".into());
    }

    let redirect = redirect_uri(port);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", callback.code.as_str()),
        ("client_id", registration.client_id()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect.as_str()),
    ];
    if let Some(client_secret) = registration.client_secret() {
        form.push(("client_secret", client_secret));
    }
    let creds = token_post(
        &Client::new(),
        token_endpoint,
        allow_http_token_endpoint,
        &form,
        None,
        /* require_refresh = */ true,
    )
    .await?;

    save_provider_auth(PROVIDER, &creds)
        .map_err(|e| format!("google-gemini: failed to persist credentials: {e}"))?;
    Ok(creds)
}

async fn wait_for_callback(
    rx: oneshot::Receiver<CallbackOutcome>,
    _expected_state: &str,
) -> Result<CallbackOutcome, String> {
    rx.await
        .map_err(|_| "google-gemini: callback channel closed".into())
}

/// Refresh grant against the production Google token endpoint.
pub async fn refresh_token(client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
    if refresh.trim().is_empty() {
        return Err(GeminiAuthError::EmptyRefreshToken.into_secret_safe());
    }
    let registration = GeminiRegistration::production()?;
    refresh_with_registration(
        client,
        TOKEN_URL,
        /* allow_http = */ false,
        &registration,
        refresh,
    )
    .await
}

/// Refresh grant with an overridable token endpoint. Public for zero-network
/// harness use only — production callers must go through `refresh_token`.
pub async fn refresh_with_registration(
    client: &Client,
    token_endpoint: &str,
    allow_http_token_endpoint: bool,
    registration: &GeminiRegistration,
    refresh: &str,
) -> Result<OAuthCredentials, String> {
    if refresh.trim().is_empty() {
        return Err(GeminiAuthError::EmptyRefreshToken.into_secret_safe());
    }
    let mut form = vec![
        ("client_id", registration.client_id()),
        ("refresh_token", refresh),
        ("grant_type", "refresh_token"),
    ];
    if let Some(client_secret) = registration.client_secret() {
        form.push(("client_secret", client_secret));
    }
    token_post(
        client,
        token_endpoint,
        allow_http_token_endpoint,
        &form,
        Some(refresh),
        /* require_refresh = */ false,
    )
    .await
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Secret-safe login/refresh errors — variants intentionally store neither
/// tokens, codes, nor client-secret material so Display cannot leak them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiAuthError {
    UntrustedEndpoint,
    TokenRequestFailed(u16),
    InvalidTokenResponse,
    EmptyAccessToken,
    EmptyRefreshToken,
    Transport,
    ResponseTooLarge,
}

impl GeminiAuthError {
    fn into_secret_safe(self) -> String {
        match self {
            Self::UntrustedEndpoint => "google-gemini: untrusted endpoint".into(),
            Self::TokenRequestFailed(code) => {
                format!("google-gemini: token request failed (HTTP {code})")
            }
            Self::InvalidTokenResponse => "google-gemini: invalid token response".into(),
            Self::EmptyAccessToken => "google-gemini: empty access token".into(),
            Self::EmptyRefreshToken => "google-gemini: empty refresh token".into(),
            Self::Transport => "google-gemini: transport error".into(),
            Self::ResponseTooLarge => "google-gemini: response body too large".into(),
        }
    }
}

// ── Pure helpers: URL builder, token parsing, callback parsing ───────────────

/// Validate that `url_str` names an HTTPS endpoint on a Google-owned host.
/// The token/authorize/userinfo endpoints are pinned; this helper is used both
/// for the pinned constants and for post-response URL sanity checks.
pub(crate) fn validate_google_https_endpoint(url_str: &str) -> Result<Url, GeminiAuthError> {
    let url = Url::parse(url_str).map_err(|_| GeminiAuthError::UntrustedEndpoint)?;
    if url.scheme() != "https" {
        return Err(GeminiAuthError::UntrustedEndpoint);
    }
    let host = url.host_str().ok_or(GeminiAuthError::UntrustedEndpoint)?;
    let allowed = host == "accounts.google.com"
        || host == "oauth2.googleapis.com"
        || host == "openidconnect.googleapis.com"
        || host == "cloudcode-pa.googleapis.com";
    if !allowed {
        return Err(GeminiAuthError::UntrustedEndpoint);
    }
    Ok(url)
}

/// Build the loopback redirect URI for the Gemini OAuth flow. Always emits a
/// literal loopback IP (RFC 8252 §7.3) — never `localhost`.
pub fn redirect_uri(port: u16) -> String {
    format!("http://{CALLBACK_HOST}:{port}{CALLBACK_PATH}")
}

/// Build a Google installed-app authorization URL with PKCE (S256), offline
/// access, and forced consent so a refresh token is always issued.
pub fn build_authorize_url(
    registration: &GeminiRegistration,
    challenge: &str,
    state: &str,
    port: u16,
) -> String {
    let mut url = Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", registration.client_id())
        .append_pair("redirect_uri", &redirect_uri(port))
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "true");
    url.into()
}

/// Parse a manually pasted Google OAuth loopback callback URL. Strict:
/// requires http/127.0.0.1/exact-port/exact-path, both `code` and `state`,
/// and exact state equality — never defaults a missing state.
pub fn parse_pasted_callback(
    input: &str,
    expected_state: &str,
    port: u16,
) -> Option<super::CallbackResult> {
    let url = Url::parse(input.trim()).ok()?;
    if url.scheme() != "http"
        || url.host_str() != Some(CALLBACK_HOST)
        || url.port_or_known_default() != Some(port)
        || url.path() != CALLBACK_PATH
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

// ── Token exchange wire types (kept private) ─────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    /// Some Google flows include a `scope` we can echo back for parity with the
    /// original grant; not enforced structurally to keep parsing lenient.
    #[allow(dead_code)]
    scope: Option<String>,
    #[allow(dead_code)]
    token_type: Option<String>,
}

pub fn credentials_from_token_response(
    body: &str,
    previous_refresh: Option<&str>,
    require_refresh: bool,
) -> Result<OAuthCredentials, GeminiAuthError> {
    let token: TokenResponse =
        serde_json::from_str(body).map_err(|_| GeminiAuthError::InvalidTokenResponse)?;

    let access = token
        .access_token
        .filter(|v| !v.trim().is_empty())
        .ok_or(GeminiAuthError::EmptyAccessToken)?;

    let refresh = token
        .refresh_token
        .filter(|v| !v.trim().is_empty())
        .or_else(|| previous_refresh.map(str::to_owned));
    if require_refresh && refresh.is_none() {
        return Err(GeminiAuthError::EmptyRefreshToken);
    }

    let expires_in = token.expires_in.unwrap_or(3600);
    let expires = now_millis()
        .saturating_add(expires_in.saturating_mul(1000))
        .saturating_sub(EXPIRY_SKEW_MS);

    Ok(OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: refresh.unwrap_or_default(),
        access,
        expires,
        account_id: None,
    })
}

async fn token_post(
    _client: &Client,
    token_endpoint: &str,
    allow_http_token_endpoint: bool,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
    require_refresh: bool,
) -> Result<OAuthCredentials, String> {
    if allow_http_token_endpoint {
        let url = Url::parse(token_endpoint)
            .map_err(|_| GeminiAuthError::UntrustedEndpoint.into_secret_safe())?;
        let host = url.host_str().unwrap_or("");
        if !(url.scheme() == "http" && (host == "127.0.0.1" || host == "localhost")) {
            return Err(GeminiAuthError::UntrustedEndpoint.into_secret_safe());
        }
    } else {
        validate_google_https_endpoint(token_endpoint)
            .map_err(GeminiAuthError::into_secret_safe)?;
    }
    // OAuth secrets (refresh tokens, code_verifier, client_secret) must never
    // be replayed to a redirect target. Use a dedicated no-redirect client.
    let no_redirect_client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| GeminiAuthError::Transport.into_secret_safe())?;
    let response = no_redirect_client
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .map_err(|_| GeminiAuthError::Transport.into_secret_safe())?;
    let status = response.status();
    if !status.is_success() {
        return Err(GeminiAuthError::TokenRequestFailed(status.as_u16()).into_secret_safe());
    }
    // Bound the body — never trust upstream to be small.
    let bytes = response
        .bytes()
        .await
        .map_err(|_| GeminiAuthError::Transport.into_secret_safe())?;
    if bytes.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(GeminiAuthError::ResponseTooLarge.into_secret_safe());
    }
    let body = std::str::from_utf8(&bytes)
        .map_err(|_| GeminiAuthError::InvalidTokenResponse.into_secret_safe())?;
    credentials_from_token_response(body, previous_refresh, require_refresh)
        .map_err(GeminiAuthError::into_secret_safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn endpoints_are_https_and_pinned_to_google() {
        for endpoint in [AUTHORIZE_URL, TOKEN_URL, USERINFO_URL] {
            let url = Url::parse(endpoint).expect("valid URL");
            assert_eq!(url.scheme(), "https", "{endpoint} must be https");
            let host = url.host_str().expect("has host");
            assert!(
                host.ends_with("google.com") || host.ends_with("googleapis.com"),
                "{endpoint} must be a google endpoint (got {host})"
            );
        }
    }

    #[test]
    fn scopes_include_cloud_platform_and_userinfo() {
        // Cloud Platform is what unlocks Code Assist; userinfo is required for
        // the eventual account label. Order must be stable so it doesn't drift.
        assert!(SCOPES.contains("https://www.googleapis.com/auth/cloud-platform"));
        assert!(SCOPES.contains("https://www.googleapis.com/auth/userinfo.email"));
        assert!(SCOPES.contains("https://www.googleapis.com/auth/userinfo.profile"));
    }

    #[test]
    fn callback_uses_loopback_ip_literal() {
        // RFC 8252 §7.3: installed apps must use a loopback IP literal, not
        // "localhost", to avoid DNS-based interception.
        assert_eq!(CALLBACK_HOST, "127.0.0.1");
        assert!(CALLBACK_PATH.starts_with('/'));
        assert_ne!(
            CALLBACK_PATH, "/callback",
            "must not collide with other providers"
        );
    }

    #[test]
    fn provider_id_matches_typed_registry_key() {
        assert_eq!(
            PROVIDER,
            super::super::provider::OAuthProviderId::GoogleGemini.as_str()
        );
    }

    #[tokio::test]
    async fn refresh_rejects_empty_refresh_without_network() {
        let err = refresh_token(&Client::new(), "").await.unwrap_err();
        // Must fail before registration lookup or any network call.
        assert!(err.contains("empty refresh token"));
    }

    #[test]
    fn production_registration_fails_closed_without_client_id() {
        std::env::remove_var(CLIENT_ID_ENV);
        std::env::remove_var(CLIENT_SECRET_ENV);
        let err = GeminiRegistration::production().unwrap_err();
        assert!(err.contains("registration_required"));
        assert!(err.contains(CLIENT_ID_ENV));
    }

    #[test]
    fn authorize_url_has_required_installed_app_pkce_params() {
        let registration = GeminiRegistration::test(
            "synaps-google-desktop-client.example",
            Some("public-client-value"),
        )
        .unwrap();
        let url_str = build_authorize_url(&registration, "challenge-123", "state-abc", 45289);
        let url = Url::parse(&url_str).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], registration.client_id());
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:45289/oauth2callback");
        assert_eq!(q["code_challenge"], "challenge-123");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "state-abc");
        // access_type=offline + prompt=consent is what unlocks a refresh token
        // on re-authorization. Both are required or Google may skip it.
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert!(q["scope"].contains("cloud-platform"));
    }

    #[test]
    fn redirect_uri_uses_loopback_ip_literal_not_localhost() {
        let uri = redirect_uri(45289);
        assert_eq!(uri, "http://127.0.0.1:45289/oauth2callback");
        assert!(!uri.contains("localhost"));
    }

    #[test]
    fn pasted_callback_is_strict_and_state_bound() {
        // Wrong host — localhost is NOT accepted.
        assert!(parse_pasted_callback(
            "http://localhost:45289/oauth2callback?code=c&state=s",
            "s",
            45289
        )
        .is_none());
        // Wrong path.
        assert!(parse_pasted_callback(
            "http://127.0.0.1:45289/callback?code=c&state=s",
            "s",
            45289
        )
        .is_none());
        // Wrong port.
        assert!(parse_pasted_callback(
            "http://127.0.0.1:1234/oauth2callback?code=c&state=s",
            "s",
            45289
        )
        .is_none());
        // Wrong scheme.
        assert!(parse_pasted_callback(
            "https://127.0.0.1:45289/oauth2callback?code=c&state=s",
            "s",
            45289
        )
        .is_none());
        // Wrong state — CSRF guard.
        assert!(parse_pasted_callback(
            "http://127.0.0.1:45289/oauth2callback?code=c&state=wrong",
            "s",
            45289
        )
        .is_none());
        // Missing state — must not silently pass.
        assert!(
            parse_pasted_callback("http://127.0.0.1:45289/oauth2callback?code=c", "s", 45289)
                .is_none()
        );
        // Missing code.
        assert!(
            parse_pasted_callback("http://127.0.0.1:45289/oauth2callback?state=s", "s", 45289)
                .is_none()
        );
        // Bare code / garbage.
        assert!(parse_pasted_callback("bare-code", "s", 45289).is_none());
        // Happy path.
        let r = parse_pasted_callback(
            "http://127.0.0.1:45289/oauth2callback?code=goodcode&state=s",
            "s",
            45289,
        )
        .unwrap();
        assert_eq!(r.code, "goodcode");
        assert_eq!(r.state, "s");
    }

    #[test]
    fn validate_google_https_endpoint_rejects_untrusted_hosts_and_http() {
        assert!(
            validate_google_https_endpoint("http://accounts.google.com/o/oauth2/v2/auth").is_err()
        );
        assert!(validate_google_https_endpoint("https://evil.example/token").is_err());
        // Look-alike must fail (host suffix rule is exact-host, not endsWith).
        assert!(validate_google_https_endpoint("https://accounts.google.com.evil/token").is_err());
        assert!(validate_google_https_endpoint("https://oauth2.googleapis.com/token").is_ok());
        assert!(
            validate_google_https_endpoint("https://cloudcode-pa.googleapis.com/v1internal:x")
                .is_ok()
        );
    }

    #[test]
    fn credentials_from_token_response_happy_and_carryover() {
        let body = r#"{"access_token":"aaa","refresh_token":"rrr","expires_in":3600,"token_type":"Bearer"}"#;
        let creds = credentials_from_token_response(body, None, true).unwrap();
        assert_eq!(creds.access, "aaa");
        assert_eq!(creds.refresh, "rrr");
        assert_eq!(creds.auth_type, "oauth");
        // Skew is subtracted; expires must be strictly less than now + expires_in.
        assert!(creds.expires < now_millis() + 3600 * 1000);

        // Refresh omitted, previous provided → carry over (refresh flow behavior).
        let body_no_r = r#"{"access_token":"newaccess","expires_in":1800}"#;
        let carried =
            credentials_from_token_response(body_no_r, Some("old-refresh"), false).unwrap();
        assert_eq!(carried.access, "newaccess");
        assert_eq!(carried.refresh, "old-refresh");
    }

    #[test]
    fn credentials_from_token_response_rejects_missing_or_empty_access_token() {
        assert!(matches!(
            credentials_from_token_response(r#"{"expires_in":10}"#, None, false),
            Err(GeminiAuthError::EmptyAccessToken)
        ));
        assert!(matches!(
            credentials_from_token_response(
                r#"{"access_token":"   ","expires_in":10}"#,
                None,
                false
            ),
            Err(GeminiAuthError::EmptyAccessToken)
        ));
    }

    #[test]
    fn credentials_from_token_response_requires_refresh_on_login_but_not_on_refresh() {
        let body = r#"{"access_token":"a","expires_in":10}"#;
        // Login path (require_refresh=true) with no refresh field and no
        // previous refresh must fail.
        assert!(matches!(
            credentials_from_token_response(body, None, true),
            Err(GeminiAuthError::EmptyRefreshToken)
        ));
        // Refresh path (require_refresh=false) with carry-over is fine.
        assert!(credentials_from_token_response(body, Some("carry"), false).is_ok());
    }

    #[test]
    fn credentials_from_token_response_rejects_non_json_body_secret_safely() {
        // Note: the caller (token_post) never places the raw body into the
        // error surface; this test asserts the parser signals a typed error.
        assert!(matches!(
            credentials_from_token_response("not-json", None, false),
            Err(GeminiAuthError::InvalidTokenResponse)
        ));
    }

    #[test]
    fn gemini_auth_error_display_is_secret_safe() {
        for err in [
            GeminiAuthError::UntrustedEndpoint,
            GeminiAuthError::TokenRequestFailed(400),
            GeminiAuthError::InvalidTokenResponse,
            GeminiAuthError::EmptyAccessToken,
            GeminiAuthError::EmptyRefreshToken,
            GeminiAuthError::Transport,
            GeminiAuthError::ResponseTooLarge,
        ] {
            let msg = err.clone().into_secret_safe();
            assert!(msg.starts_with("google-gemini:"), "{msg}");
            assert!(!msg.contains("configured-client-value"));
        }
    }
}
