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
        Arc,
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
/// not a first-class constant on docs.github.com. See docs/specs/github-copilot-oauth-spec.md.
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

/// Default verification page (official docs.github.com device flow).
pub const DEFAULT_VERIFICATION_URI: &str = "https://github.com/login/device";

const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Hard ceiling so `slow_down` cannot grow unbounded.
const MAX_POLL_INTERVAL_SECS: u64 = 30;
/// Seconds added on each `slow_down` (GitHub docs: +5s).
const SLOW_DOWN_STEP_SECS: u64 = 5;

/// Connect timeout for production device/poll/mint HTTP.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Full request timeout for production device/poll/mint HTTP.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum response body bytes accepted from device/poll/mint endpoints.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

// Session-mint request headers (community-observed; required to avoid 403 on
// /copilot_internal/v2/token after a successful device-flow authorization).
// User-Agent identifies Synaps honestly; Copilot-Integration-Id / Editor-* match
// the conventional vscode-chat integration surface used by public clients.
// See docs/specs/github-copilot-oauth-spec.md §C5 / U4.
pub const MINT_USER_AGENT: &str = "SynapsCLI/0.6.0";
pub const MINT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub const MINT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub const MINT_COPILOT_INTEGRATION_ID: &str = "vscode-chat";
/// Logical sleep quantum for cancel checks (production wall-clock chunks).
const CANCEL_POLL_QUANTUM: Duration = Duration::from_millis(200);

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
    AccessDenied,
    Expired,
    IncorrectDeviceCode,
    DeviceFlowDisabled,
    Cancelled,
    EmptyGitHubToken,
    EmptySessionToken,
    HttpStatus(u16),
    /// Session mint endpoint rejected the request (often missing integration headers).
    SessionMintRejected(u16),
    Transport,
    ResponseTooLarge,
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
            Self::AccessDenied => "access denied",
            Self::Expired => "device code expired",
            Self::IncorrectDeviceCode => "incorrect device code",
            Self::DeviceFlowDisabled => "device flow disabled",
            Self::Cancelled => "login cancelled",
            Self::EmptyGitHubToken => "empty GitHub user token",
            Self::EmptySessionToken => "empty Copilot session token",
            Self::HttpStatus(_) => "HTTP error",
            Self::SessionMintRejected(_) => "Copilot session mint rejected",
            Self::Transport => "transport error",
            Self::ResponseTooLarge => "response body too large",
            Self::Persist => "credential persistence failed",
            Self::Other(msg) => msg,
        }
    }
}

impl fmt::Display for CopilotAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(code) => write!(f, "HTTP error {code}"),
            Self::SessionMintRejected(code) => {
                write!(f, "Copilot session mint rejected (HTTP {code})")
            }
            other => f.write_str(other.label()),
        }
    }
}

impl fmt::Debug for CopilotAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CopilotAuthError({self})")
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

    /// GET with bearer auth and extra request headers (mint path attaches GitHub/Copilot headers).
    async fn get_bearer(
        &self,
        url: &str,
        bearer: &str,
        headers: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError>;
}

/// Injectable clock (now + cancellable sleep) for deterministic poll tests.
#[async_trait]
pub trait CopilotClock: Send + Sync {
    fn now_millis(&self) -> u64;

    /// Sleep `duration`, returning early with [`CopilotAuthError::Cancelled`]
    /// when `cancel` flips. Implementations must check cancellation at least
    /// every quantum (not only after the full sleep).
    async fn sleep_cancellable<X: CopilotCancel + ?Sized>(
        &self,
        duration: Duration,
        cancel: &X,
    ) -> Result<(), CopilotAuthError>;
}

/// Injectable cancellation signal for device-flow polling.
pub trait CopilotCancel: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

impl CopilotCancel for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::SeqCst)
    }
}

impl CopilotCancel for Arc<AtomicBool> {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::SeqCst)
    }
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

/// Accept only the documented GitHub device verification destination:
/// `https://github.com/login/device` optionally with a single `user_code` query
/// (verification_uri_complete). Rejects userinfo, fragments, alternate ports,
/// deceptive paths, and arbitrary query keys.
pub fn validate_verification_uri(uri: &str) -> Result<(), CopilotAuthError> {
    let parsed = Url::parse(uri).map_err(|_| CopilotAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    if parsed.host_str() != Some("github.com") {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    if parsed.port().is_some() {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    if parsed.fragment().is_some() {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    // Normalize trailing slash only; reject any other path (including `..` forms
    // after URL parsing).
    let path = parsed.path().trim_end_matches('/');
    if path != "/login/device" {
        return Err(CopilotAuthError::UntrustedEndpoint);
    }
    // Query: none, or only user_code=... (complete URI form).
    let mut saw_user_code = false;
    for (key, value) in parsed.query_pairs() {
        if key != "user_code" || value.is_empty() || saw_user_code {
            return Err(CopilotAuthError::UntrustedEndpoint);
        }
        saw_user_code = true;
    }
    Ok(())
}

// ── Session-mint headers ─────────────────────────────────────────────────────

/// Headers attached only to the pinned session-mint request.
///
/// GitHub rejects API calls without a User-Agent. The Copilot internal mint
/// endpoint additionally requires the conventional integration/editor headers
/// (community-observed); missing them yields HTTP 403 after a successful device
/// authorization.
pub fn session_mint_request_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("Accept", "application/json"),
        ("User-Agent", MINT_USER_AGENT),
        ("Editor-Version", MINT_EDITOR_VERSION),
        ("Editor-Plugin-Version", MINT_EDITOR_PLUGIN_VERSION),
        ("Copilot-Integration-Id", MINT_COPILOT_INTEGRATION_ID),
    ]
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

pub fn apply_slow_down(interval_secs: u64) -> u64 {
    interval_secs
        .saturating_add(SLOW_DOWN_STEP_SECS)
        .min(MAX_POLL_INTERVAL_SECS)
}

// ── Bounded body read ────────────────────────────────────────────────────────

/// Read a response body with a hard byte cap. Fail closed if exceeded.
pub async fn read_body_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<String, CopilotAuthError> {
    use futures::StreamExt;
    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CopilotAuthError::Transport)?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(CopilotAuthError::ResponseTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| CopilotAuthError::Transport)
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
    if !(200..300).contains(&resp.status) {
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
///
/// Sleeps are cancellable: `cancel` is checked before each wait and during
/// the wait via [`CopilotClock::sleep_cancellable`].
pub async fn wait_for_device_authorization<H, C, X>(
    http: &H,
    clock: &C,
    authz: &DeviceAuthorization,
    cancel: &X,
) -> Result<String, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
    X: CopilotCancel + ?Sized,
{
    let deadline_ms = authz
        .issued_at_ms
        .saturating_add(authz.expires_in_secs.saturating_mul(1000));
    let mut interval_secs = authz.interval_secs.clamp(1, MAX_POLL_INTERVAL_SECS);

    loop {
        if cancel.is_cancelled() {
            return Err(CopilotAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(CopilotAuthError::Expired);
        }

        clock
            .sleep_cancellable(Duration::from_secs(interval_secs), cancel)
            .await?;

        if cancel.is_cancelled() {
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
    let resp = http
        .get_bearer(
            SESSION_MINT_URL,
            github_token,
            session_mint_request_headers(),
        )
        .await?;
    if !(200..300).contains(&resp.status) {
        // Stage-specific: device flow already succeeded; mint is the failing step.
        return Err(CopilotAuthError::SessionMintRejected(resp.status));
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

pub type UserCodeHook<'a> = &'a dyn Fn(&str, &str);

pub struct LoginHooks<'a, B> {
    pub browser: &'a B,
    /// When set, the user_code + verification_uri are written here (tests / TUI).
    pub on_user_code: Option<UserCodeHook<'a>>,
}

/// Device start → user prompt → poll → mint → atomic persist.
pub async fn login_with<H, C, B, X>(
    http: &H,
    clock: &C,
    hooks: LoginHooks<'_, B>,
    cancel: &X,
    persist: bool,
) -> Result<OAuthCredentials, CopilotAuthError>
where
    H: CopilotHttp,
    C: CopilotClock,
    B: CopilotBrowser,
    X: CopilotCancel + ?Sized,
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
    /// No-redirect client with connect + request timeouts for secret-bearing calls.
    client: Client,
}

impl ProductionHttp {
    fn new() -> Result<Self, CopilotAuthError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
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
        let body = read_body_capped(response, MAX_RESPONSE_BODY_BYTES).await?;
        Ok(InjectedHttpResponse { status, body })
    }

    async fn get_bearer(
        &self,
        url: &str,
        bearer: &str,
        headers: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError> {
        // Pin again immediately before attaching the GitHub token.
        validate_session_mint_endpoint(url)?;
        let mut req = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {bearer}"));
        for (name, value) in headers {
            // Never let a caller-supplied Authorization override the bearer we set.
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(*name, *value);
        }
        let response = req.send().await.map_err(|_| CopilotAuthError::Transport)?;
        let status = response.status().as_u16();
        // Redirects are disabled; a 3xx means fail closed without following.
        if (300..400).contains(&status) {
            return Err(CopilotAuthError::UntrustedEndpoint);
        }
        let body = read_body_capped(response, MAX_RESPONSE_BODY_BYTES).await?;
        Ok(InjectedHttpResponse { status, body })
    }
}

struct ProductionClock;

#[async_trait]
impl CopilotClock for ProductionClock {
    fn now_millis(&self) -> u64 {
        now_millis()
    }

    async fn sleep_cancellable<X: CopilotCancel + ?Sized>(
        &self,
        duration: Duration,
        cancel: &X,
    ) -> Result<(), CopilotAuthError> {
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if cancel.is_cancelled() {
                return Err(CopilotAuthError::Cancelled);
            }
            let chunk = if remaining > CANCEL_POLL_QUANTUM {
                CANCEL_POLL_QUANTUM
            } else {
                remaining
            };
            tokio::time::sleep(chunk).await;
            remaining = remaining.saturating_sub(chunk);
        }
        if cancel.is_cancelled() {
            return Err(CopilotAuthError::Cancelled);
        }
        Ok(())
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

// ── Test doubles ─────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeClock {
        pub now: Mutex<u64>,
        pub sleeps: Mutex<Vec<u64>>,
        /// When true (default), advance `now` by each quantum during sleep.
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

        async fn sleep_cancellable<X: CopilotCancel + ?Sized>(
            &self,
            duration: Duration,
            cancel: &X,
        ) -> Result<(), CopilotAuthError> {
            // Quantum = 1s of logical time so cancel can interrupt mid-interval.
            let total_secs = duration
                .as_secs()
                .max(if duration.is_zero() { 0 } else { 1 });
            if total_secs == 0 {
                if cancel.is_cancelled() {
                    return Err(CopilotAuthError::Cancelled);
                }
                return Ok(());
            }
            let mut slept = 0u64;
            for _ in 0..total_secs {
                if cancel.is_cancelled() {
                    if slept > 0 {
                        self.sleeps.lock().unwrap().push(slept);
                    }
                    return Err(CopilotAuthError::Cancelled);
                }
                slept += 1;
                if *self.auto_advance.lock().unwrap() {
                    let mut now = self.now.lock().unwrap();
                    *now = now.saturating_add(1000);
                }
                // Tiny real sleep so a concurrent canceller can land mid-interval.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            self.sleeps.lock().unwrap().push(slept);
            if cancel.is_cancelled() {
                return Err(CopilotAuthError::Cancelled);
            }
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    pub enum ScriptedResponse {
        Ok(String),
        Status(u16, String),
    }

    #[derive(Default)]
    pub struct FakeHttp {
        pub post_queue: Mutex<Vec<ScriptedResponse>>,
        pub get_queue: Mutex<Vec<ScriptedResponse>>,
        pub posts: Mutex<Vec<(String, Vec<(String, String)>)>>,
        /// (url, bearer, headers)
        pub gets: Mutex<Vec<(String, String, Vec<(String, String)>)>>,
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
                ScriptedResponse::Status(status, body) => Ok(InjectedHttpResponse { status, body }),
            }
        }

        async fn get_bearer(
            &self,
            url: &str,
            bearer: &str,
            headers: &[(&str, &str)],
        ) -> Result<InjectedHttpResponse, CopilotAuthError> {
            validate_session_mint_endpoint(url)?;
            self.gets.lock().unwrap().push((
                url.to_string(),
                bearer.to_string(),
                headers
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            ));
            let next = {
                let mut q = self.get_queue.lock().unwrap();
                if q.is_empty() {
                    return Err(CopilotAuthError::Transport);
                }
                q.remove(0)
            };
            match next {
                ScriptedResponse::Ok(body) => Ok(InjectedHttpResponse { status: 200, body }),
                ScriptedResponse::Status(status, body) => Ok(InjectedHttpResponse { status, body }),
            }
        }
    }

    #[derive(Default)]
    pub struct RecordingBrowser {
        pub opened: Mutex<Vec<String>>,
    }

    impl CopilotBrowser for RecordingBrowser {
        fn open(&self, url: &str) -> Result<(), CopilotAuthError> {
            validate_verification_uri(url)?;
            self.opened.lock().unwrap().push(url.to_string());
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
        assert_eq!(DEFAULT_VERIFICATION_URI, "https://github.com/login/device");
        validate_device_endpoint(DEVICE_CODE_URL).unwrap();
        validate_device_endpoint(ACCESS_TOKEN_URL).unwrap();
        validate_session_mint_endpoint(SESSION_MINT_URL).unwrap();
        validate_verification_uri(DEFAULT_VERIFICATION_URI).unwrap();
        assert!(CONNECT_TIMEOUT.as_secs() > 0);
        assert!(REQUEST_TIMEOUT.as_secs() > 0);
        assert!(MAX_RESPONSE_BODY_BYTES >= 1024);
    }

    #[test]
    fn github_copilot_rejects_non_pinned_endpoints() {
        for bad in [
            "http://github.com/login/device/code",
            "https://evil.com/login/device/code",
            "https://github.com.evil/login/device/code",
            "https://api.github.com/copilot_internal/v2/token",
        ] {
            assert!(
                validate_device_endpoint(bad).is_err(),
                "should reject {bad}"
            );
        }
        for bad in [
            "http://api.github.com/copilot_internal/v2/token",
            "https://evil.com/copilot_internal/v2/token",
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
    fn github_copilot_verification_uri_is_strictly_device_page() {
        // Allowed: plain and complete (user_code only).
        validate_verification_uri("https://github.com/login/device").unwrap();
        validate_verification_uri("https://github.com/login/device/").unwrap();
        validate_verification_uri("https://github.com/login/device?user_code=ABCD-1234").unwrap();

        for bad in [
            "http://github.com/login/device",
            "https://evil.com/login/device",
            "https://github.com.evil/login/device",
            "https://github.com/settings",
            "https://github.com/login",
            "https://github.com/login/device/extra",
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/device/../../../etc/passwd",
            "https://user:pass@github.com/login/device",
            "https://github.com/login/device#fragment",
            "https://github.com:8443/login/device",
            "https://github.com/login/device?foo=bar",
            "https://github.com/login/device?user_code=A&user_code=B",
            "https://github.com/login/device?user_code=",
            // Deceptive open-redirect style query
            "https://github.com/login/device?user_code=x&return_to=https://evil",
        ] {
            assert!(
                validate_verification_uri(bad).is_err(),
                "should reject verification uri {bad}"
            );
        }
    }

    #[test]
    fn github_copilot_no_client_secret_in_production_surface() {
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
    fn github_copilot_production_http_configures_timeouts_and_body_cap() {
        let src = include_str!("github_copilot.rs");
        let production = src
            .split("// ── Unit tests ─")
            .next()
            .expect("production section");
        assert!(production.contains("connect_timeout(CONNECT_TIMEOUT)"));
        assert!(production.contains(".timeout(REQUEST_TIMEOUT)"));
        assert!(production.contains("read_body_capped(response, MAX_RESPONSE_BODY_BYTES)"));
        assert!(!production.contains(".text()"));
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

        let deceptive = r#"{
            "device_code":"x","user_code":"Y",
            "verification_uri":"https://github.com/login/device?user_code=A&next=https://evil",
            "expires_in":100,"interval":5
        }"#;
        assert!(parse_device_code_response(deceptive, 0).is_err());
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
        assert!(credentials_from_session(
            "",
            SessionToken {
                token: "x".into(),
                expires_at_ms: 1
            }
        )
        .is_err());
        assert!(credentials_from_session(
            "g",
            SessionToken {
                token: "".into(),
                expires_at_ms: 1
            }
        )
        .is_err());
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
            CopilotAuthError::SessionMintRejected(403),
            CopilotAuthError::Transport,
            CopilotAuthError::ResponseTooLarge,
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

        // cancel before poll starts
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
        clock.set_now(authz.issued_at_ms + authz.expires_in_secs * 1000 + 1);
        let cancel = AtomicBool::new(false);
        let err = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::Expired);
    }

    #[tokio::test]
    async fn github_copilot_cancellation_interrupts_pending_sleep() {
        let http = FakeHttp::default();
        // Long interval so cancel lands mid-sleep, not after poll starts.
        let body = serde_json::json!({
            "device_code": "device-secret-code-xxxxx",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 30
        })
        .to_string();
        http.push_post(ScriptedResponse::Ok(body));
        // No poll responses: cancel must interrupt the first sleep.
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);

        let waiter = tokio::spawn(async move {
            wait_for_device_authorization(&http, &clock, &authz, cancel_flag.as_ref()).await
        });

        // Wait until the first sleep quantum is underway, then cancel.
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.store(true, Ordering::SeqCst);

        let err = waiter.await.unwrap().unwrap_err();
        assert_eq!(err, CopilotAuthError::Cancelled);
        // Must not have attempted a poll (queue empty would be Transport).
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
        assert!(matches!(err, CopilotAuthError::SessionMintRejected(401)));
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

    #[test]
    fn github_copilot_session_mint_headers_include_required_integration_fields() {
        let headers = session_mint_request_headers();
        let map: std::collections::HashMap<_, _> = headers.iter().copied().collect();
        assert_eq!(map.get("Accept"), Some(&"application/json"));
        assert_eq!(map.get("User-Agent"), Some(&MINT_USER_AGENT));
        assert_eq!(map.get("Editor-Version"), Some(&MINT_EDITOR_VERSION));
        assert_eq!(
            map.get("Editor-Plugin-Version"),
            Some(&MINT_EDITOR_PLUGIN_VERSION)
        );
        assert_eq!(
            map.get("Copilot-Integration-Id"),
            Some(&MINT_COPILOT_INTEGRATION_ID)
        );
        // Authorization is never part of the static header table (bearer is separate).
        assert!(!map.contains_key("Authorization"));
        // Synaps identifies itself honestly in User-Agent.
        assert!(MINT_USER_AGENT.contains("Synaps"));
    }

    #[tokio::test]
    async fn github_copilot_session_mint_sends_integration_headers_or_maps_403() {
        // Live failure mode: device flow succeeds, mint returns 403 without
        // Copilot-Integration-Id / editor headers. Regression: mint must attach
        // session_mint_request_headers() and stage-map non-2xx as SessionMintRejected.
        let http = FakeHttp::default();
        let now = 1_700_000_000_000u64;
        http.push_get(ScriptedResponse::Status(
            403,
            r#"{"message":"token not authorized for this integration"}"#.into(),
        ));
        let clock = FakeClock::new(now);
        let err = mint_session_token(&http, &clock, "gho_after_device_ok")
            .await
            .unwrap_err();
        assert!(
            matches!(err, CopilotAuthError::SessionMintRejected(403)),
            "mint 403 must be staged as SessionMintRejected, not generic HttpStatus: {err:?}"
        );
        // Error surface must not echo body secrets / raw tokens.
        let rendered = err.to_string();
        assert!(!rendered.contains("gho_"));
        assert!(!rendered.contains("authorized for this integration"));
        assert!(rendered.contains("403"));

        // Successful mint path must send the full header set.
        let http = FakeHttp::default();
        http.push_get(ScriptedResponse::Ok(session_body(
            "tid=ok",
            now / 1000 + 1800,
        )));
        let creds = mint_credentials(&http, &clock, "gho_after_device_ok")
            .await
            .unwrap();
        assert_eq!(creds.access, "tid=ok");
        let gets = http.gets.lock().unwrap().clone();
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0].0, SESSION_MINT_URL);
        assert_eq!(gets[0].1, "gho_after_device_ok");
        let hdrs: std::collections::HashMap<_, _> = gets[0].2.iter().cloned().collect();
        assert_eq!(
            hdrs.get("Copilot-Integration-Id").map(String::as_str),
            Some(MINT_COPILOT_INTEGRATION_ID)
        );
        assert_eq!(
            hdrs.get("Editor-Version").map(String::as_str),
            Some(MINT_EDITOR_VERSION)
        );
        assert_eq!(
            hdrs.get("User-Agent").map(String::as_str),
            Some(MINT_USER_AGENT)
        );
        assert_eq!(
            hdrs.get("Accept").map(String::as_str),
            Some("application/json")
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
        assert!(
            http.posts.lock().unwrap().is_empty(),
            "refresh must not re-run device flow"
        );
        assert_eq!(http.gets.lock().unwrap().len(), 1);
        assert_eq!(http.gets.lock().unwrap()[0].0, SESSION_MINT_URL);
    }

    #[tokio::test]
    async fn github_copilot_login_persist_is_atomic_and_preserves_siblings() {
        use super::super::storage::save_provider_auth_at_test_hook;
        use super::super::OAuthCredentials;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
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
        assert_eq!(
            parsed["github-copilot"]["refresh"],
            "gho_long_lived_never_vend"
        );
        assert_eq!(parsed["anthropic"]["refresh"], "anth-refresh");
        assert_eq!(parsed["xai-auth"]["access"], "xai-a");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn github_copilot_broker_access_token_shape_excludes_refresh() {
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
        http.push_get(ScriptedResponse::Status(302, "redirected".into()));
        let clock = FakeClock::new(0);
        let err = mint_session_token(&http, &clock, "gho_x")
            .await
            .unwrap_err();
        assert!(matches!(err, CopilotAuthError::SessionMintRejected(302)));
    }
}
