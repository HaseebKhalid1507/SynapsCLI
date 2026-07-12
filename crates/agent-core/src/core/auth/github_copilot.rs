//! GitHub Copilot OAuth (device authorization + broker-owned session mint).
//!
//! Experimental. Device-flow endpoints are official GitHub OAuth. The session-mint
//! path (`GET /copilot_internal/v2/token`) is community-observed and is **not**
//! documented as a stable general-purpose third-party API. Do not describe it as
//! officially supported.
//!
//! Credential mapping:
//! - `OAuthCredentials.refresh` = long-lived GitHub user token (broker-owned only)
//! - `OAuthCredentials.access`  = short-lived Copilot session token
//! - `OAuthCredentials.expires` = session expiry (ms, with skew)
//!
//! The long-lived GitHub token must never be vended, logged, or placed in broker
//! wire types.

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::{now_millis, open_browser, save_provider_auth, OAuthCredentials};

// ── Public constants (evidence-isolated) ─────────────────────────────────────

/// Canonical storage / broker / model-prefix id.
pub const PROVIDER: &str = "github-copilot";

/// Public native-client id observed in Copilot editor clients (VS Code lineage).
/// Not a client secret. Provenance: community consensus + device-flow examples;
/// not a first-class constant on docs.github.com. See docs/github-copilot-oauth-spec.md.
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Least-privilege scope among common working values (community-observed).
pub const SCOPE: &str = "read:user";

/// Official GitHub device-authorization endpoint.
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// Official GitHub OAuth token endpoint (device-code grant).
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Community-observed Copilot session-mint endpoint (pinned host+path).
pub const SESSION_MINT_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Device-code grant type (RFC 8628).
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Default verification page (official).
#[allow(dead_code)] // referenced by tests and login UX defaults
pub const DEFAULT_VERIFICATION_URI: &str = "https://github.com/login/device";

const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Hard ceiling so `slow_down` cannot grow unbounded.
const MAX_POLL_INTERVAL_SECS: u64 = 30;
/// Seconds added on each `slow_down` (GitHub docs: +5s).
const SLOW_DOWN_STEP_SECS: u64 = 5;

// ── Secret-safe errors ───────────────────────────────────────────────────────

/// Failures from the Copilot auth chain. Variants intentionally store **no**
/// device codes, user codes, tokens, or Authorization headers so Display/Debug
/// cannot leak them.
#[derive(Clone, PartialEq, Eq)]
pub enum CopilotAuthError {
    UntrustedEndpoint,
    InvalidDeviceResponse,
    InvalidTokenResponse,
    InvalidSessionResponse,
    AuthorizationPending, // not surfaced as terminal; reserved
    AccessDenied,
    Expired,
    IncorrectDeviceCode,
    DeviceFlowDisabled,
    Cancelled,
    EmptyGitHubToken,
    EmptySessionToken,
    HttpStatus(u16),
    Transport,
    Persist,
    Other(&'static str),
}

impl CopilotAuthError {
    fn label(&self) -> &'static str {
        match self {
            Self::UntrustedEndpoint => "untrusted endpoint",
            Self::InvalidDeviceResponse => "invalid device authorization response",
            Self::InvalidTokenResponse => "invalid device token response",
            Self::InvalidSessionResponse => "invalid Copilot session response",
            Self::AuthorizationPending => "authorization pending",
            Self::AccessDenied => "access denied",
            Self::Expired => "device code expired",
            Self::IncorrectDeviceCode => "incorrect device code",
            Self::DeviceFlowDisabled => "device flow disabled",
            Self::Cancelled => "login cancelled",
            Self::EmptyGitHubToken => "empty GitHub user token",
            Self::EmptySessionToken => "empty Copilot session token",
            Self::HttpStatus(_) => "HTTP error",
            Self::Transport => "transport error",
            Self::Persist => "credential persistence failed",
            Self::Other(msg) => msg,
        }
    }
}

impl fmt::Display for CopilotAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(code) => write!(f, "HTTP error {code}"),
            other => f.write_str(other.label()),
        }
    }
}

impl fmt::Debug for CopilotAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same surface as Display — never dump internal secret-bearing payloads.
        write!(f, "CopilotAuthError({})", self)
    }
}

impl From<CopilotAuthError> for String {
    fn from(value: CopilotAuthError) -> Self {
        value.to_string()
    }
}

// ── Injectable boundaries ────────────────────────────────────────────────────

/// Minimal HTTP response used by the device-flow / mint state machine.
#[derive(Debug, Clone)]
pub struct InjectedHttpResponse {
    pub status: u16,
    pub body: String,
}

/// Injectable HTTP surface so tests never touch the network.
#[async_trait]
pub trait CopilotHttp: Send + Sync {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError>;

    async fn get_bearer(
        &self,
        url: &str,
        bearer: &str,
    ) -> Result<InjectedHttpResponse, CopilotAuthError>;
}

/// Injectable clock (now + sleep) for deterministic poll tests.
#[async_trait]
pub trait CopilotClock: Send + Sync {
    fn now_millis(&self) -> u64;
    async fn sleep(&self, duration: Duration);
}

/// Injectable browser opener (production uses `open_browser`).
pub trait CopilotBrowser: Send + Sync {
    fn open(&self, url: &str) -> Result<(), CopilotAuthError>;
}

// ── Device-flow types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    /// Opaque device code — never logged via public error types.
    pub(crate) device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_secs: u64,
    pub interval_secs: u64,
    pub issued_at_ms: u64,
}

/// Outcome of one device-token poll (secret-safe; token only on Authorized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePollOutcome {
    Pending,
    SlowDown,
    Authorized { github_token: String },
    Denied,
    Expired,
    IncorrectDeviceCode,
    DeviceFlowDisabled,
    OtherError,
}

#[derive(Debug, Clone)]
pub struct SessionToken {
    pub token: String,
    pub expires_at_ms: u64,
}

// ── Endpoint pinning ─────────────────────────────────────────────────────────

/// Accept only the exact pinned HTTPS github.com device endpoints.
pub fn validate_device_endpoint(url: &str) -> Result<(), CopilotAuthError> {
    if url != DEVICE_CODE_URL && url != ACCESS_TOKEN_URL {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    let parsed = Url::parse(url).map_err(|_| CopilotAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    Ok(())
}

/// Accept only the pinned session-mint host+path (no query, no alternate host).
pub fn validate_session_mint_endpoint(url: &str) -> Result<(), CopilotAuthError> {
    if url != SESSION_MINT_URL {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    let parsed = Url::parse(url).map_err(|_| CopilotAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("api.github.com")
        || parsed.path() != "/copilot_internal/v2/token"
    {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    Ok(())
}

/// Accept only HTTPS github.com verification URIs (no open redirect to evil hosts).
pub fn validate_verification_uri(uri: &str) -> Result<(), CopilotAuthError> {
    let parsed = Url::parse(uri).map_err(|_| CopilotAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    Ok(())
}

// ── Parsing (pure) ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DeviceCodeJson {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenJson {
    access_token: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionMintJson {
    token: Option<String>,
    /// Unix seconds (community-observed).
    expires_at: Option<u64>,
    expires_in: Option<u64>,
}

pub fn parse_device_code_response(
    body: &str,
    now_ms: u64,
) -> Result<DeviceAuthorization, CopilotAuthError> {
    let raw: DeviceCodeJson =
        serde_json::from_str(body).map_err(|_| CopilotAuthError::InvalidDeviceResponse)?;
    if raw.device_code.trim().is_empty() || raw.user_code.trim().is_empty() {
        return Err(CopilotAuthError::InvalidDeviceResponse);
    }
    if raw.expires_in == 0 {
        return Err(CopilotAuthError::InvalidDeviceResponse);
    }
    validate_verification_uri(&raw.verification_uri)?;
    // Prefer the plain verification URI (user types code). Complete URI is optional
    // and still must stay on github.com if present.
    if let Some(ref complete) = raw.verification_uri_complete {
        validate_verification_uri(complete)?;
    }
    let interval = raw
        .interval
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .clamp(1, MAX_POLL_INTERVAL_SECS);
    Ok(DeviceAuthorization {
        device_code: raw.device_code,
        user_code: raw.user_code,
        verification_uri: raw.verification_uri,
        expires_in_secs: raw.expires_in,
        interval_secs: interval,
        issued_at_ms: now_ms,
    })
}

pub fn parse_device_poll_response(body: &str) -> Result<DevicePollOutcome, CopilotAuthError> {
    // GitHub may return form-urlencoded or JSON; prefer JSON (Accept: application/json).
    if let Ok(raw) = serde_json::from_str::<DeviceTokenJson>(body) {
        if let Some(token) = raw.access_token {
            if token.trim().is_empty() {
                return Err(CopilotAuthError::EmptyGitHubToken);
            }
            return Ok(DevicePollOutcome::Authorized {
                github_token: token,
            });
        }
        return Ok(match raw.error.as_deref().unwrap_or("") {
            "authorization_pending" => DevicePollOutcome::Pending,
            "slow_down" => DevicePollOutcome::SlowDown,
            "access_denied" => DevicePollOutcome::Denied,
            "expired_token" => DevicePollOutcome::Expired,
            "incorrect_device_code" => DevicePollOutcome::IncorrectDeviceCode,
            "device_flow_disabled" => DevicePollOutcome::DeviceFlowDisabled,
            _ => DevicePollOutcome::OtherError,
        });
    }
    // form-urlencoded fallback
    let mut access_token = None;
    let mut error = None;
    for pair in body.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        let v = urlencoding::decode(v)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| v.to_string());
        match k {
            "access_token" => access_token = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    if let Some(token) = access_token {
        if token.trim().is_empty() {
            return Err(CopilotAuthError::EmptyGitHubToken);
        }
        return Ok(DevicePollOutcome::Authorized {
            github_token: token,
        });
    }
    Ok(match error.as_deref().unwrap_or("") {
        "authorization_pending" => DevicePollOutcome::Pending,
        "slow_down" => DevicePollOutcome::SlowDown,
        "access_denied" => DevicePollOutcome::Denied,
        "expired_token" => DevicePollOutcome::Expired,
        "incorrect_device_code" => DevicePollOutcome::IncorrectDeviceCode,
        "device_flow_disabled" => DevicePollOutcome::DeviceFlowDisabled,
        _ => DevicePollOutcome::OtherError,
    })
}

pub fn parse_session_mint_response(
    body: &str,
    now_ms: u64,
) -> Result<SessionToken, CopilotAuthError> {
    let raw: SessionMintJson =
        serde_json::from_str(body).map_err(|_| CopilotAuthError::InvalidSessionResponse)?;
    let token = raw
        .token
        .filter(|t| !t.trim().is_empty())
        .ok_or(CopilotAuthError::EmptySessionToken)?;
    let expires_at_ms = if let Some(secs) = raw.expires_at {
        // Community payloads use unix seconds; treat large values as already-ms.
        if secs > 10_000_000_000 {
            secs
        } else {
            secs.saturating_mul(1000)
        }
    } else if let Some(expires_in) = raw.expires_in {
        now_ms.saturating_add(expires_in.saturating_mul(1000))
    } else {
        return Err(CopilotAuthError::InvalidSessionResponse);
    };
    let expires_at_ms = expires_at_ms.saturating_sub(EXPIRY_SKEW_MS);
    if expires_at_ms <= now_ms {
        return Err(CopilotAuthError::Expired);
    }
    Ok(SessionToken {
        token,
        expires_at_ms,
    })
}

pub fn credentials_from_session(
    github_token: &str,
    session: SessionToken,
) -> Result<OAuthCredentials, CopilotAuthError> {
    if github_token.trim().is_empty() {
        return Err(CopilotAuthError::EmptyGitHubToken);
    }
    if session.token.trim().is_empty() {
        return Err(CopilotAuthError::EmptySessionToken);
    }
    Ok(OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: github_token.to_string(),
        access: session.token,
        expires: session.expires_at_ms,
        account_id: None,
    })
}

// ── Bounded poll interval helper ─────────────────────────────────────────────

pub fn apply_slow_down(interval_secs: u64) -> u64 {
    interval_secs
        .saturating_add(SLOW_DOWN_STEP_SECS)
        .min(MAX_POLL_INTERVAL_SECS)
}

// ── Device-flow state machine ────────────────────────────────────────────────

/// Start device authorization (POST device/code).
pub async fn start_device_authorization<H, C>(
    http: &H,
    clock: &C,
) -> Result<DeviceAuthorization, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
{
    validate_device_endpoint(DEVICE_CODE_URL)?;
    let resp = http
        .post_form(
            DEVICE_CODE_URL,
            &[("client_id", CLIENT_ID), ("scope", SCOPE)],
        )
        .await?;
    if !(200..300).contains(&resp.status) {
        return Err(CopilotAuthError::HttpStatus(resp.status));
    }
    parse_device_code_response(&resp.body, clock.now_millis())
}

/// One poll against the token endpoint.
pub async fn poll_device_token<H>(
    http: &H,
    device_code: &str,
) -> Result<DevicePollOutcome, CopilotAuthError>
where
    H: CopilotHttp,
{
    validate_device_endpoint(ACCESS_TOKEN_URL)?;
    let resp = http
        .post_form(
            ACCESS_TOKEN_URL,
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", DEVICE_GRANT_TYPE),
            ],
        )
        .await?;
    // GitHub returns 200 even for authorization_pending in many clients; treat
    // non-2xx as transport/HTTP failure (no body secrets echoed).
    if !(200..300).contains(&resp.status) {
        // Some servers return 400 with error body — still try parse.
        if let Ok(outcome) = parse_device_poll_response(&resp.body) {
            if !matches!(outcome, DevicePollOutcome::OtherError) {
                return Ok(outcome);
            }
        }
        return Err(CopilotAuthError::HttpStatus(resp.status));
    }
    parse_device_poll_response(&resp.body)
}

/// Poll until authorized, denied, expired, cancelled, or deadline.
pub async fn wait_for_device_authorization<H, C>(
    http: &H,
    clock: &C,
    authz: &DeviceAuthorization,
    cancel: &AtomicBool,
) -> Result<String, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
{
    let deadline_ms = authz
        .issued_at_ms
        .saturating_add(authz.expires_in_secs.saturating_mul(1000));
    let mut interval_secs = authz.interval_secs.clamp(1, MAX_POLL_INTERVAL_SECS);

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CopilotAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(CopilotAuthError::Expired);
        }

        clock.sleep(Duration::from_secs(interval_secs)).await;

        if cancel.load(Ordering::SeqCst) {
            return Err(CopilotAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(CopilotAuthError::Expired);
        }

        match poll_device_token(http, &authz.device_code).await? {
            DevicePollOutcome::Pending => continue,
            DevicePollOutcome::SlowDown => {
                interval_secs = apply_slow_down(interval_secs);
            }
            DevicePollOutcome::Authorized { github_token } => {
                if github_token.trim().is_empty() {
                    return Err(CopilotAuthError::EmptyGitHubToken);
                }
                return Ok(github_token);
            }
            DevicePollOutcome::Denied => return Err(CopilotAuthError::AccessDenied),
            DevicePollOutcome::Expired => return Err(CopilotAuthError::Expired),
            DevicePollOutcome::IncorrectDeviceCode => {
                return Err(CopilotAuthError::IncorrectDeviceCode)
            }
            DevicePollOutcome::DeviceFlowDisabled => {
                return Err(CopilotAuthError::DeviceFlowDisabled)
            }
            DevicePollOutcome::OtherError => return Err(CopilotAuthError::InvalidTokenResponse),
        }
    }
}

// ── Session mint (pinned, no-redirect at production client) ──────────────────

/// Exchange a long-lived GitHub user token for a short-lived Copilot session.
///
/// The GitHub token is sent only to the pinned mint URL. Callers must never log
/// the token argument.
pub async fn mint_session_token<H, C>(
    http: &H,
    clock: &C,
    github_token: &str,
) -> Result<SessionToken, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
{
    if github_token.trim().is_empty() {
        return Err(CopilotAuthError::EmptyGitHubToken);
    }
    validate_session_mint_endpoint(SESSION_MINT_URL)?;
    let resp = http.get_bearer(SESSION_MINT_URL, github_token).await?;
    if !(200..300).contains(&resp.status) {
        return Err(CopilotAuthError::HttpStatus(resp.status));
    }
    parse_session_mint_response(&resp.body, clock.now_millis())
}

/// Build credentials after a successful mint (GitHub token → refresh).
pub async fn mint_credentials<H, C>(
    http: &H,
    clock: &C,
    github_token: &str,
) -> Result<OAuthCredentials, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
{
    let session = mint_session_token(http, clock, github_token).await?;
    credentials_from_session(github_token, session)
}

// ── Full login (injectable) ──────────────────────────────────────────────────

pub struct LoginHooks<'a, B> {
    pub browser: &'a B,
    /// When set, the user_code + verification_uri are written here (tests / TUI).
    pub on_user_code: Option<&'a dyn Fn(&str, &str)>,
}

/// Device start → user prompt → poll → mint → atomic persist.
pub async fn login_with<H, C, B>(
    http: &H,
    clock: &C,
    hooks: LoginHooks<'_, B>,
    cancel: &AtomicBool,
    persist: bool,
) -> Result<OAuthCredentials, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
    B: CopilotBrowser,
{
    let authz = start_device_authorization(http, clock).await?;
    if let Some(cb) = hooks.on_user_code {
        cb(&authz.user_code, &authz.verification_uri);
    } else {
        eprintln!("\n\x1b[1mGitHub Copilot device login\x1b[0m");
        eprintln!("  Enter code: \x1b[36m{}\x1b[0m", authz.user_code);
        eprintln!("  At:         \x1b[36m{}\x1b[0m\n", authz.verification_uri);
    }
    // Open only the validated verification URI (never an arbitrary redirect).
    validate_verification_uri(&authz.verification_uri)?;
    let _ = hooks.browser.open(&authz.verification_uri);

    let github_token = wait_for_device_authorization(http, clock, &authz, cancel).await?;
    let creds = mint_credentials(http, clock, &github_token).await?;
    if persist {
        save_provider_auth(PROVIDER, &creds).map_err(|_| CopilotAuthError::Persist)?;
    }
    Ok(creds)
}

/// Production login entry (real network + system browser + auth.json).
pub async fn login() -> Result<OAuthCredentials, String> {
    let http = ProductionHttp::new().map_err(|e| e.to_string())?;
    let clock = ProductionClock;
    let browser = ProductionBrowser;
    let cancel = AtomicBool::new(false);
    login_with(
        &http,
        &clock,
        LoginHooks {
            browser: &browser,
            on_user_code: None,
        },
        &cancel,
        true,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Broker refresh: re-mint session from stored GitHub user token (no browser).
pub async fn refresh_token(
    _client: &Client,
    github_token: &str,
) -> Result<OAuthCredentials, String> {
    let http = ProductionHttp::new().map_err(|e| e.to_string())?;
    let clock = ProductionClock;
    mint_credentials(&http, &clock, github_token)
        .await
        .map_err(|e| e.to_string())
}

// ── Production adapters ──────────────────────────────────────────────────────

struct ProductionHttp {
    /// No-redirect client for secret-bearing requests.
    client: Client,
}

impl ProductionHttp {
    fn new() -> Result<Self, CopilotAuthError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CopilotAuthError::Transport)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl CopilotHttp for ProductionHttp {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError> {
        validate_device_endpoint(url)?;
        let response = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .form(form)
            .send()
            .await
            .map_err(|_| CopilotAuthError::Transport)?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|_| CopilotAuthError::Transport)?;
        Ok(InjectedHttpResponse { status, body })
    }

    async fn get_bearer(
        &self,
        url: &str,
        bearer: &str,
    ) -> Result<InjectedHttpResponse, CopilotAuthError> {
        // Pin again immediately before attaching the GitHub token.
        validate_session_mint_endpoint(url)?;
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {bearer}"))
            .send()
            .await
            .map_err(|_| CopilotAuthError::Transport)?;
        let status = response.status().as_u16();
        // Redirects are disabled; a 3xx means fail closed without following.
        if (300..400).contains(&status) {
            return Err(CopilotAuthError::UntrustedEndpoint);
        }
        let body = response
            .text()
            .await
            .map_err(|_| CopilotAuthError::Transport)?;
        Ok(InjectedHttpResponse { status, body })
    }
}

struct ProductionClock;

#[async_trait]
impl CopilotClock for ProductionClock {
    fn now_millis(&self) -> u64 {
        now_millis()
    }
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

struct ProductionBrowser;

impl CopilotBrowser for ProductionBrowser {
    fn open(&self, url: &str) -> Result<(), CopilotAuthError> {
        validate_verification_uri(url)?;
        let _ = open_browser(url);
        Ok(())
    }
}

// ── Test doubles (unit tests + e2e harness) ──────────────────────────────────

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeClock {
        pub now: Mutex<u64>,
        pub sleeps: Mutex<Vec<u64>>,
        /// Advance `now` by this many ms on each sleep (default: duration).
        pub auto_advance: Mutex<bool>,
    }

    impl FakeClock {
        pub fn new(now: u64) -> Self {
            Self {
                now: Mutex::new(now),
                sleeps: Mutex::new(Vec::new()),
                auto_advance: Mutex::new(true),
            }
        }
        pub fn set_now(&self, v: u64) {
            *self.now.lock().unwrap() = v;
        }
    }

    #[async_trait]
    impl CopilotClock for FakeClock {
        fn now_millis(&self) -> u64 {
            *self.now.lock().unwrap()
        }
        async fn sleep(&self, duration: Duration) {
            let secs = duration.as_secs();
            self.sleeps.lock().unwrap().push(secs);
            if *self.auto_advance.lock().unwrap() {
                let mut now = self.now.lock().unwrap();
                *now = now.saturating_add(duration.as_millis() as u64);
            }
        }
    }

    #[derive(Clone, Debug)]
    pub enum ScriptedResponse {
        Ok(String),
        Status(u16, String),
        Err,
    }

    #[derive(Default)]
    pub struct FakeHttp {
        pub post_queue: Mutex<Vec<ScriptedResponse>>,
        pub get_queue: Mutex<Vec<ScriptedResponse>>,
        pub posts: Mutex<Vec<(String, Vec<(String, String)>)>>,
        pub gets: Mutex<Vec<(String, String)>>,
    }

    impl FakeHttp {
        pub fn push_post(&self, r: ScriptedResponse) {
            self.post_queue.lock().unwrap().push(r);
        }
        pub fn push_get(&self, r: ScriptedResponse) {
            self.get_queue.lock().unwrap().push(r);
        }
    }

    #[async_trait]
    impl CopilotHttp for FakeHttp {
        async fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<InjectedHttpResponse, CopilotAuthError> {
            validate_device_endpoint(url)?;
            self.posts.lock().unwrap().push((
                url.to_string(),
                form.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            ));
            let next = {
                let mut q = self.post_queue.lock().unwrap();
                if q.is_empty() {
                    return Err(CopilotAuthError::Transport);
                }
                q.remove(0)
            };
            match next {
                ScriptedResponse::Ok(body) => Ok(InjectedHttpResponse { status: 200, body }),
                ScriptedResponse::Status(status, body) => {
                    Ok(InjectedHttpResponse { status, body })
                }
                ScriptedResponse::Err => Err(CopilotAuthError::Transport),
            }
        }

        async fn get_bearer(
            &self,
            url: &str,
            bearer: &str,
        ) -> Result<InjectedHttpResponse, CopilotAuthError> {
            validate_session_mint_endpoint(url)?;
            self.gets
                .lock()
                .unwrap()
                .push((url.to_string(), bearer.to_string()));
            let next = {
                let mut q = self.get_queue.lock().unwrap();
                if q.is_empty() {
                    return Err(CopilotAuthError::Transport);
                }
                q.remove(0)
            };
            match next {
                ScriptedResponse::Ok(body) => Ok(InjectedHttpResponse { status: 200, body }),
                ScriptedResponse::Status(status, body) => {
                    Ok(InjectedHttpResponse { status, body })
                }
                ScriptedResponse::Err => Err(CopilotAuthError::Transport),
            }
        }
    }

    #[derive(Default)]
    pub struct RecordingBrowser {
        pub opened: Mutex<Vec<String>>,
        pub fail: AtomicBool,
    }

    impl CopilotBrowser for RecordingBrowser {
        fn open(&self, url: &str) -> Result<(), CopilotAuthError> {
            validate_verification_uri(url)?;
            self.opened.lock().unwrap().push(url.to_string());
            if self.fail.load(Ordering::SeqCst) {
                return Err(CopilotAuthError::Transport);
            }
            Ok(())
        }
    }

    pub fn device_code_body() -> String {
        serde_json::json!({
            "device_code": "device-secret-code-xxxxx",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        })
        .to_string()
    }

    pub fn pending_body() -> String {
        r#"{"error":"authorization_pending"}"#.into()
    }
    pub fn slow_down_body() -> String {
        r#"{"error":"slow_down"}"#.into()
    }
    pub fn denied_body() -> String {
        r#"{"error":"access_denied"}"#.into()
    }
    pub fn expired_body() -> String {
        r#"{"error":"expired_token"}"#.into()
    }
    pub fn authorized_body(token: &str) -> String {
        serde_json::json!({ "access_token": token, "token_type": "bearer", "scope": "read:user" })
            .to_string()
    }
    pub fn session_body(token: &str, expires_at_secs: u64) -> String {
        serde_json::json!({ "token": token, "expires_at": expires_at_secs }).to_string()
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn github_copilot_constants_are_pinned_https_github() {
        assert_eq!(PROVIDER, "github-copilot");
        assert_eq!(CLIENT_ID, "Iv1.b507a08c87ecfe98");
        assert_eq!(SCOPE, "read:user");
        assert_eq!(DEVICE_CODE_URL, "https://github.com/login/device/code");
        assert_eq!(
            ACCESS_TOKEN_URL,
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(
            SESSION_MINT_URL,
            "https://api.github.com/copilot_internal/v2/token"
        );
        validate_device_endpoint(DEVICE_CODE_URL).unwrap();
        validate_device_endpoint(ACCESS_TOKEN_URL).unwrap();
        validate_session_mint_endpoint(SESSION_MINT_URL).unwrap();
    }

    #[test]
    fn github_copilot_rejects_non_pinned_endpoints() {
        for bad in [
            "http://github.com/login/device/code",
            "https://evil.com/login/device/code",
            "https://github.com.evil/login/device/code",
            "https://api.github.com/copilot_internal/v2/token",
            "https://github.com/login/device/code/../oauth/access_token",
        ] {
            assert!(
                validate_device_endpoint(bad).is_err(),
                "should reject {bad}"
            );
        }
        for bad in [
            "http://api.github.com/copilot_internal/v2/token",
            "https://evil.com/copilot_internal/v2/token",
            "https://api.github.com/copilot_internal/v2/token/../other",
            "https://api.github.com/user",
            "https://github.com/login/device/code",
        ] {
            assert!(
                validate_session_mint_endpoint(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

        #[test]
    fn github_copilot_no_client_secret_constant() {
        // Production surface (everything before unit tests) must not define or
        // post a client secret. The test module may mention the string.
        let src = include_str!("github_copilot.rs");
        let production = src
            .split("// ── Unit tests ─")
            .next()
            .expect("production section");
        assert!(
            !production.contains("CLIENT_SECRET"),
            "device flow must not embed a CLIENT_SECRET constant"
        );
        assert!(
            !production.contains("client_secret"),
            "device flow must not send or store a client_secret"
        );
    }

#[test]
    fn github_copilot_parse_device_code_and_reject_bad_uri() {
        let authz = parse_device_code_response(&device_code_body(), 1_000).unwrap();
        assert_eq!(authz.user_code, "ABCD-1234");
        assert_eq!(authz.interval_secs, 5);
        assert_eq!(authz.verification_uri, DEFAULT_VERIFICATION_URI);

        let evil = r#"{
            "device_code":"x","user_code":"Y","verification_uri":"https://evil.example/device",
            "expires_in":100,"interval":5
        }"#;
        assert!(parse_device_code_response(evil, 0).is_err());
    }

    #[test]
    fn github_copilot_parse_poll_outcomes() {
        assert_eq!(
            parse_device_poll_response(&pending_body()).unwrap(),
            DevicePollOutcome::Pending
        );
        assert_eq!(
            parse_device_poll_response(&slow_down_body()).unwrap(),
            DevicePollOutcome::SlowDown
        );
        assert_eq!(
            parse_device_poll_response(&denied_body()).unwrap(),
            DevicePollOutcome::Denied
        );
        assert_eq!(
            parse_device_poll_response(&expired_body()).unwrap(),
            DevicePollOutcome::Expired
        );
        match parse_device_poll_response(&authorized_body("gho_test_token")).unwrap() {
            DevicePollOutcome::Authorized { github_token } => {
                assert_eq!(github_token, "gho_test_token")
            }
            other => panic!("expected authorized, got {other:?}"),
        }
        // form-urlencoded
        assert_eq!(
            parse_device_poll_response("error=authorization_pending").unwrap(),
            DevicePollOutcome::Pending
        );
    }

    #[test]
    fn github_copilot_slow_down_is_bounded() {
        let mut i = 5u64;
        for _ in 0..20 {
            i = apply_slow_down(i);
        }
        assert_eq!(i, MAX_POLL_INTERVAL_SECS);
        assert!(i <= MAX_POLL_INTERVAL_SECS);
    }

    #[test]
    fn github_copilot_session_mint_parse_applies_skew() {
        let now = 1_700_000_000_000u64;
        let expires_at_secs = now / 1000 + 1800;
        let session =
            parse_session_mint_response(&session_body("tid=session;exp=1", expires_at_secs), now)
                .unwrap();
        assert_eq!(session.token, "tid=session;exp=1");
        assert_eq!(
            session.expires_at_ms,
            expires_at_secs * 1000 - EXPIRY_SKEW_MS
        );
    }

    #[test]
    fn github_copilot_credentials_map_github_to_refresh_session_to_access() {
        let creds = credentials_from_session(
            "gho_long_lived",
            SessionToken {
                token: "tid=short".into(),
                expires_at_ms: 99,
            },
        )
        .unwrap();
        assert_eq!(creds.refresh, "gho_long_lived");
        assert_eq!(creds.access, "tid=short");
        assert_eq!(creds.expires, 99);
        assert_eq!(creds.auth_type, "oauth");
        assert!(credentials_from_session("", SessionToken { token: "x".into(), expires_at_ms: 1 }).is_err());
        assert!(credentials_from_session("g", SessionToken { token: "".into(), expires_at_ms: 1 }).is_err());
    }

    #[test]
    fn github_copilot_errors_debug_display_are_secret_free() {
        let secrets = [
            "gho_SUPER_SECRET_TOKEN",
            "ghu_another_secret",
            "tid=session-secret",
            "device-secret-code-xxxxx",
            "ABCD-1234",
            "Bearer abc",
            "Authorization",
        ];
        let errors = [
            CopilotAuthError::UntrustedEndpoint,
            CopilotAuthError::InvalidDeviceResponse,
            CopilotAuthError::InvalidTokenResponse,
            CopilotAuthError::InvalidSessionResponse,
            CopilotAuthError::AccessDenied,
            CopilotAuthError::Expired,
            CopilotAuthError::IncorrectDeviceCode,
            CopilotAuthError::DeviceFlowDisabled,
            CopilotAuthError::Cancelled,
            CopilotAuthError::EmptyGitHubToken,
            CopilotAuthError::EmptySessionToken,
            CopilotAuthError::HttpStatus(401),
            CopilotAuthError::Transport,
            CopilotAuthError::Persist,
            CopilotAuthError::Other("generic failure"),
        ];
        for e in errors {
            let d = format!("{e:?}");
            let s = e.to_string();
            for secret in secrets {
                assert!(
                    !d.contains(secret) && !s.contains(secret),
                    "leak in {d:?} / {s:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn github_copilot_device_flow_pending_slow_down_then_authorize() {
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        http.push_post(ScriptedResponse::Ok(pending_body()));
        http.push_post(ScriptedResponse::Ok(slow_down_body()));
        http.push_post(ScriptedResponse::Ok(authorized_body("gho_user_token")));

        let clock = FakeClock::new(1_000);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        assert_eq!(authz.user_code, "ABCD-1234");

        let cancel = AtomicBool::new(false);
        let token = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap();
        assert_eq!(token, "gho_user_token");

        let sleeps = clock.sleeps.lock().unwrap().clone();
        // pending @5s, slow_down then next poll @10s (5+5)
        assert!(sleeps.len() >= 3);
        assert_eq!(sleeps[0], 5);
        assert_eq!(sleeps[1], 5);
        assert_eq!(sleeps[2], 10);

        let posts = http.posts.lock().unwrap().clone();
        assert_eq!(posts[0].0, DEVICE_CODE_URL);
        assert!(posts[0]
            .1
            .iter()
            .any(|(k, v)| k == "client_id" && v == CLIENT_ID));
        assert!(posts[0].1.iter().any(|(k, v)| k == "scope" && v == SCOPE));
        assert_eq!(posts[1].0, ACCESS_TOKEN_URL);
        // No client_secret in any form body
        for (_, form) in &posts {
            assert!(!form.iter().any(|(k, _)| k == "client_secret"));
        }
    }

    #[tokio::test]
    async fn github_copilot_device_flow_denial_and_expiry_and_cancel() {
        // denial
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        http.push_post(ScriptedResponse::Ok(denied_body()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let cancel = AtomicBool::new(false);
        let err = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::AccessDenied);

        // cancel before poll completes
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let cancel = AtomicBool::new(true);
        let err = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::Cancelled);

        // expiry by clock
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        // Jump past expires_in (900s)
        clock.set_now(authz.issued_at_ms + authz.expires_in_secs * 1000 + 1);
        let cancel = AtomicBool::new(false);
        let err = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::Expired);
    }

    #[tokio::test]
    async fn github_copilot_session_mint_pins_host_and_maps_credentials() {
        let http = FakeHttp::default();
        let now = 1_700_000_000_000u64;
        let exp = now / 1000 + 1800;
        http.push_get(ScriptedResponse::Ok(session_body("tid=sess", exp)));
        let clock = FakeClock::new(now);
        let creds = mint_credentials(&http, &clock, "gho_refresh_material")
            .await
            .unwrap();
        assert_eq!(creds.access, "tid=sess");
        assert_eq!(creds.refresh, "gho_refresh_material");
        let gets = http.gets.lock().unwrap().clone();
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0].0, SESSION_MINT_URL);
        assert_eq!(gets[0].1, "gho_refresh_material");
    }

    #[tokio::test]
    async fn github_copilot_login_requires_successful_mint_before_return() {
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        http.push_post(ScriptedResponse::Ok(authorized_body("gho_ok")));
        // mint fails
        http.push_get(ScriptedResponse::Status(401, r#"{"message":"bad"}"#.into()));
        let clock = FakeClock::new(0);
        let browser = RecordingBrowser::default();
        let cancel = AtomicBool::new(false);
        let err = login_with(
            &http,
            &clock,
            LoginHooks {
                browser: &browser,
                on_user_code: Some(&|_, _| {}),
            },
            &cancel,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CopilotAuthError::HttpStatus(401)));
        assert_eq!(
            browser.opened.lock().unwrap().as_slice(),
            &[DEFAULT_VERIFICATION_URI.to_string()]
        );
    }

    #[tokio::test]
    async fn github_copilot_full_login_happy_path_without_persist() {
        let http = FakeHttp::default();
        http.push_post(ScriptedResponse::Ok(device_code_body()));
        http.push_post(ScriptedResponse::Ok(pending_body()));
        http.push_post(ScriptedResponse::Ok(authorized_body("gho_live")));
        let now = 1_700_000_000_000u64;
        http.push_get(ScriptedResponse::Ok(session_body(
            "tid=live-session",
            now / 1000 + 2000,
        )));
        let clock = FakeClock::new(now);
        let browser = RecordingBrowser::default();
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(None);
        let creds = login_with(
            &http,
            &clock,
            LoginHooks {
                browser: &browser,
                on_user_code: Some(&|code, uri| {
                    *seen.lock().unwrap() = Some((code.to_string(), uri.to_string()));
                }),
            },
            &cancel,
            false,
        )
        .await
        .unwrap();
        assert_eq!(creds.refresh, "gho_live");
        assert_eq!(creds.access, "tid=live-session");
        assert_eq!(
            seen.lock().unwrap().clone().unwrap(),
            (
                "ABCD-1234".to_string(),
                DEFAULT_VERIFICATION_URI.to_string()
            )
        );
    }

    #[tokio::test]
    async fn github_copilot_empty_github_token_rejected_before_mint() {
        let http = FakeHttp::default();
        let clock = FakeClock::new(0);
        let err = mint_credentials(&http, &clock, "  ").await.unwrap_err();
        assert_eq!(err, CopilotAuthError::EmptyGitHubToken);
        assert!(http.gets.lock().unwrap().is_empty());
    }


    #[tokio::test]
    async fn github_copilot_refresh_remints_without_browser() {
        // refresh_token path is mint-only; no device endpoints contacted.
        let http = FakeHttp::default();
        let now = 1_700_000_000_000u64;
        http.push_get(ScriptedResponse::Ok(session_body(
            "tid=refreshed-session",
            now / 1000 + 1800,
        )));
        let clock = FakeClock::new(now);
        let creds = mint_credentials(&http, &clock, "gho_stored_refresh")
            .await
            .unwrap();
        assert_eq!(creds.access, "tid=refreshed-session");
        assert_eq!(creds.refresh, "gho_stored_refresh");
        assert!(http.posts.lock().unwrap().is_empty(), "refresh must not re-run device flow");
        assert_eq!(http.gets.lock().unwrap().len(), 1);
        assert_eq!(http.gets.lock().unwrap()[0].0, SESSION_MINT_URL);
    }

    #[tokio::test]
    async fn github_copilot_login_persist_is_atomic_and_preserves_siblings() {
        use super::super::storage::save_provider_auth_at_test_hook;
        use super::super::OAuthCredentials;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        // Pre-seed unrelated providers
        let sibling = OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: "anth-refresh".into(),
            access: "anth-access".into(),
            expires: now_millis() + 3_600_000,
            account_id: None,
        };
        save_provider_auth_at_test_hook(&path, "anthropic", &sibling).unwrap();
        save_provider_auth_at_test_hook(
            &path,
            "xai-auth",
            &OAuthCredentials {
                auth_type: "oauth".into(),
                refresh: "xai-r".into(),
                access: "xai-a".into(),
                expires: now_millis() + 3_600_000,
                account_id: None,
            },
        )
        .unwrap();

        // Simulate successful mint+store under github-copilot
        let creds = credentials_from_session(
            "gho_long_lived_never_vend",
            SessionToken {
                token: "tid=session_only".into(),
                expires_at_ms: now_millis() + 1_500_000,
            },
        )
        .unwrap();
        save_provider_auth_at_test_hook(&path, PROVIDER, &creds).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["github-copilot"]["access"], "tid=session_only");
        assert_eq!(parsed["github-copilot"]["refresh"], "gho_long_lived_never_vend");
        assert_eq!(parsed["anthropic"]["refresh"], "anth-refresh");
        assert_eq!(parsed["xai-auth"]["access"], "xai-a");
        // Atomic tmp cleaned up
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn github_copilot_broker_access_token_shape_excludes_refresh() {
        // Structural: AccessToken has no refresh field; only session token is vended.
        let v = serde_json::json!({
            "token": "tid=session",
            "expires": 123u64,
            "refresh": "gho_MUST_NOT_DESERIALIZE",
        });
        let tok: super::super::AccessToken = serde_json::from_value(v).unwrap();
        assert_eq!(tok.token, "tid=session");
        assert_eq!(tok.expires, 123);
        let round = serde_json::to_value(&tok).unwrap();
        assert!(round.get("refresh").is_none());
        assert!(!round.to_string().contains("gho_"));
    }

    #[tokio::test]
    async fn github_copilot_mint_rejects_redirect_status() {
        let http = FakeHttp::default();
        http.push_get(ScriptedResponse::Status(
            302,
            "redirected".into(),
        ));
        let clock = FakeClock::new(0);
        // FakeHttp returns the 302 body to mint_session_token which treats non-2xx as HttpStatus.
        // ProductionHttp additionally maps 3xx -> UntrustedEndpoint; unit-level non-2xx is enough.
        let err = mint_session_token(&http, &clock, "gho_x").await.unwrap_err();
        assert!(matches!(err, CopilotAuthError::HttpStatus(302)));
    }
}

