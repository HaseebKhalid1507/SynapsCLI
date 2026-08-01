//! Kimi Code OAuth (RFC 8628 device authorization; direct bearer, no mint).
//!
//! Experimental. Flow verified against the official Kimi Code CLI (v0.31.1):
//! its embedded `packages/oauth` sources pin the same endpoints, client id,
//! grant type, error codes, and refresh semantics implemented here. The
//! endpoints are not published as a stable third-party API; do not describe
//! them as officially supported.
//!
//! Flow shape (differences from GitHub Copilot):
//! - No session mint: the device-flow `access_token` is used directly as the
//!   API bearer for `https://api.kimi.com/coding/v1`.
//! - Access tokens are short-lived (~15 min) and refresh tokens **rotate on
//!   every refresh** — the rotated pair must always be persisted.
//! - A refresh rejected with HTTP 401/403 or `invalid_grant` means the stored
//!   refresh token is dead; the user must re-login.
//! - Upstream sends `X-Msh-*` device-identity headers on every OAuth and API
//!   request; we mirror that conventional surface with an honest User-Agent.
//!
//! Credential mapping:
//! - `OAuthCredentials.refresh` = rotating Kimi refresh token (broker-owned)
//! - `OAuthCredentials.access`  = short-lived Kimi access token
//! - `OAuthCredentials.expires` = access expiry (ms, with refresh margin)

use std::{
    fmt,
    path::PathBuf,
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

use super::{auth_file_path, now_millis, open_browser, save_provider_auth, OAuthCredentials};

// ── Public constants (evidence: Kimi Code CLI v0.31.1 embedded oauth pkg) ────

/// Canonical storage / broker / model-prefix id.
pub const PROVIDER: &str = "kimi-code";

/// Public native-client id shipped inside the official Kimi Code CLI
/// (`KIMI_CODE_FLOW_CONFIG.clientId`). Not a client secret.
pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

/// OAuth host pinned by the official CLI (`https://auth.kimi.com`).
pub const OAUTH_HOST: &str = "https://auth.kimi.com";

/// Device-authorization endpoint (RFC 8628 §3.1, Kimi path shape).
pub const DEVICE_AUTH_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";

/// Token endpoint — device-code polling AND refresh grants.
pub const TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";

/// Device-code grant type (RFC 8628).
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Managed Kimi Code API base (bearer = OAuth access token).
pub const API_BASE_URL: &str = "https://api.kimi.com/coding/v1";

/// Conventional platform tag sent by the official CLI (`X-Msh-Platform`).
pub const MSH_PLATFORM: &str = "kimi_code_cli";
/// Kimi Code surface version this flow was verified against (`X-Msh-Version`).
pub const MSH_SURFACE_VERSION: &str = "0.31.1";
/// Honest User-Agent (matches the Copilot mint precedent).
pub const USER_AGENT: &str = "SynapsCLI/0.6.0";

const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Hard ceiling so `slow_down` cannot grow unbounded.
const MAX_POLL_INTERVAL_SECS: u64 = 30;
/// Seconds added on each `slow_down` (upstream: +5s).
const SLOW_DOWN_STEP_SECS: u64 = 5;
/// Local wall-clock budget when the server omits `expires_in`
/// (upstream uses a 15-minute device-code timeout).
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;

/// Refresh-margin floor/ratio (upstream: `max(300, expires_in * 0.5)` secs).
const MIN_REFRESH_MARGIN_SECS: u64 = 300;
/// Never bake a margin that would mark a fresh token as already expired.
const MARGIN_HEADROOM_SECS: u64 = 60;

/// Refresh retry policy (upstream parity: 3 attempts, 2^n seconds backoff).
const REFRESH_MAX_ATTEMPTS: u32 = 3;
const REFRESH_BACKOFF_BASE_MS: u64 = 1_000;

/// Connect timeout for production OAuth HTTP.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Full request timeout for production OAuth HTTP.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum response body bytes accepted from OAuth endpoints.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

/// Logical sleep quantum for cancel checks (production wall-clock chunks).
const CANCEL_POLL_QUANTUM: Duration = Duration::from_millis(200);

// ── Secret-safe errors ───────────────────────────────────────────────────────

/// Failures from the Kimi auth chain. Variants intentionally store **no**
/// device codes, user codes, or tokens so Display/Debug cannot leak them.
#[derive(Clone, PartialEq, Eq)]
pub enum KimiAuthError {
    UntrustedEndpoint,
    InvalidDeviceResponse,
    InvalidTokenResponse,
    AccessDenied,
    Expired,
    Cancelled,
    /// Refresh rejected (401/403/invalid_grant) — re-login required.
    Unauthorized,
    HttpStatus(u16),
    Transport,
    ResponseTooLarge,
    Persist,
    Other(&'static str),
}

impl KimiAuthError {
    fn label(&self) -> &'static str {
        match self {
            Self::UntrustedEndpoint => "untrusted endpoint",
            Self::InvalidDeviceResponse => "invalid device authorization response",
            Self::InvalidTokenResponse => "invalid token response",
            Self::AccessDenied => "access denied",
            Self::Expired => "device code expired",
            Self::Cancelled => "login cancelled",
            Self::Unauthorized => "kimi-code refresh token rejected; run `synaps login kimi-code`",
            Self::HttpStatus(_) => "HTTP error",
            Self::Transport => "transport error",
            Self::ResponseTooLarge => "response body too large",
            Self::Persist => "credential persistence failed",
            Self::Other(msg) => msg,
        }
    }
}

impl fmt::Display for KimiAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(code) => write!(f, "HTTP error {code}"),
            other => f.write_str(other.label()),
        }
    }
}

impl fmt::Debug for KimiAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KimiAuthError({self})")
    }
}

impl From<KimiAuthError> for String {
    fn from(value: KimiAuthError) -> Self {
        value.to_string()
    }
}

// ── Injectable boundaries ────────────────────────────────────────────────────

/// Minimal HTTP response used by the device-flow / refresh state machine.
#[derive(Debug, Clone)]
pub struct InjectedHttpResponse {
    pub status: u16,
    pub body: String,
}

/// Injectable HTTP surface so tests never touch the network.
#[async_trait]
pub trait KimiHttp: Send + Sync {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, KimiAuthError>;
}

/// Injectable clock (now + cancellable sleep) for deterministic poll tests.
#[async_trait]
pub trait KimiClock: Send + Sync {
    fn now_millis(&self) -> u64;

    /// Sleep `duration`, returning early with [`KimiAuthError::Cancelled`]
    /// when `cancel` flips.
    async fn sleep_cancellable<X: KimiCancel + ?Sized>(
        &self,
        duration: Duration,
        cancel: &X,
    ) -> Result<(), KimiAuthError>;
}

/// Injectable cancellation signal for device-flow polling.
pub trait KimiCancel: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

impl KimiCancel for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::SeqCst)
    }
}

impl KimiCancel for Arc<AtomicBool> {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::SeqCst)
    }
}

/// Injectable browser opener (production uses `open_browser`).
pub trait KimiBrowser: Send + Sync {
    fn open(&self, url: &str) -> Result<(), KimiAuthError>;
}

// ── Device-flow types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    /// Opaque device code — never logged via public error types.
    pub(crate) device_code: String,
    pub user_code: String,
    /// Plain verification page (may be empty; server contract keeps it
    /// optional as long as the complete URI is present).
    pub verification_uri: String,
    /// Verification URI with the user code embedded — preferred display/open
    /// target (required by the upstream contract).
    pub verification_uri_complete: String,
    pub expires_in_secs: u64,
    pub interval_secs: u64,
    pub issued_at_ms: u64,
}

/// Successful token grant (poll or refresh). Secret-bearing — never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct KimiTokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_secs: u64,
}

impl fmt::Debug for KimiTokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KimiTokenSet")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// Outcome of one device-token poll (secret-safe; tokens only on Authorized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePollOutcome {
    Pending,
    SlowDown,
    Authorized { token: KimiTokenSet },
    Denied,
    Expired,
    OtherError,
}

// ── Endpoint pinning ─────────────────────────────────────────────────────────

/// Accept only the exact pinned HTTPS auth.kimi.com OAuth endpoints.
pub fn validate_oauth_endpoint(url: &str) -> Result<(), KimiAuthError> {
    if url != DEVICE_AUTH_URL && url != TOKEN_URL {
        return Err(KimiAuthError::UntrustedEndpoint);
    }
    let parsed = Url::parse(url).map_err(|_| KimiAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("auth.kimi.com") {
        return Err(KimiAuthError::UntrustedEndpoint);
    }
    Ok(())
}

/// Accept only an HTTPS kimi.com / *.kimi.com verification destination with
/// no userinfo, no fragment, and no explicit port. The exact path is
/// server-assigned (not a documented constant), so the host is the pin.
pub fn validate_verification_uri(uri: &str) -> Result<(), KimiAuthError> {
    let parsed = Url::parse(uri).map_err(|_| KimiAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" {
        return Err(KimiAuthError::UntrustedEndpoint);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(KimiAuthError::UntrustedEndpoint);
    }
    if parsed.port().is_some() || parsed.fragment().is_some() {
        return Err(KimiAuthError::UntrustedEndpoint);
    }
    match parsed.host_str() {
        Some(host) if host == "kimi.com" || host.ends_with(".kimi.com") => Ok(()),
        _ => Err(KimiAuthError::UntrustedEndpoint),
    }
}

// ── Device identity headers ──────────────────────────────────────────────────

/// Strip to printable ASCII, mirroring the upstream `asciiHeader` helper.
fn ascii_header(value: &str, fallback: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Path of the persistent Kimi device id (kept beside auth.json, mode 0600).
pub fn device_id_path() -> PathBuf {
    auth_file_path().with_file_name("kimi_device_id")
}

/// Load the stable device id, creating (and best-effort persisting) a fresh
/// UUIDv4 on first use. Upstream parity: `createKimiDeviceId`.
pub fn load_or_create_device_id() -> String {
    let path = device_id_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    id
}

/// Device-identity headers sent on every OAuth and managed-API request.
///
/// Upstream (`createKimiDeviceHeaders`) sends this exact `X-Msh-*` surface;
/// the User-Agent identifies Synaps honestly (Copilot mint precedent).
/// No header value is secret.
pub fn identity_request_headers(device_id: &str) -> Vec<(&'static str, String)> {
    let host_name = std::env::var("HOSTNAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "synaps".to_string());
    vec![
        ("User-Agent", USER_AGENT.to_string()),
        ("Accept", "application/json".to_string()),
        ("X-Msh-Platform", MSH_PLATFORM.to_string()),
        ("X-Msh-Version", MSH_SURFACE_VERSION.to_string()),
        ("X-Msh-Device-Id", ascii_header(device_id, "unknown")),
        ("X-Msh-Device-Name", ascii_header(&host_name, "synaps")),
        (
            "X-Msh-Device-Model",
            ascii_header(
                &format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                "unknown",
            ),
        ),
        ("X-Msh-Os-Version", std::env::consts::OS.to_string()),
    ]
}

// ── Parsing (pure) ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DeviceCodeJson {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenJson {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
}

pub fn parse_device_code_response(
    body: &str,
    now_ms: u64,
) -> Result<DeviceAuthorization, KimiAuthError> {
    let raw: DeviceCodeJson =
        serde_json::from_str(body).map_err(|_| KimiAuthError::InvalidDeviceResponse)?;
    if raw.device_code.trim().is_empty()
        || raw.user_code.trim().is_empty()
        || raw.verification_uri_complete.trim().is_empty()
    {
        return Err(KimiAuthError::InvalidDeviceResponse);
    }
    validate_verification_uri(&raw.verification_uri_complete)?;
    let verification_uri = raw.verification_uri.unwrap_or_default();
    if !verification_uri.is_empty() {
        validate_verification_uri(&verification_uri)?;
    }
    let expires_in = match raw.expires_in {
        Some(0) => return Err(KimiAuthError::InvalidDeviceResponse),
        Some(v) => v,
        // Upstream tolerates a missing expires_in and relies on its local
        // 15-minute wall-clock budget; mirror that.
        None => DEFAULT_DEVICE_EXPIRES_IN_SECS,
    };
    let interval = raw
        .interval
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .clamp(1, MAX_POLL_INTERVAL_SECS);
    Ok(DeviceAuthorization {
        device_code: raw.device_code,
        user_code: raw.user_code,
        verification_uri,
        verification_uri_complete: raw.verification_uri_complete,
        expires_in_secs: expires_in,
        interval_secs: interval,
        issued_at_ms: now_ms,
    })
}

/// Parse a successful token grant. The upstream contract requires all three
/// of access_token / refresh_token / expires_in — enforce the same here so a
/// non-rotating or non-expiring credential can never be persisted silently.
fn parse_token_success(raw: TokenJson) -> Result<KimiTokenSet, KimiAuthError> {
    let access = raw
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or(KimiAuthError::InvalidTokenResponse)?;
    let refresh = raw
        .refresh_token
        .filter(|t| !t.trim().is_empty())
        .ok_or(KimiAuthError::InvalidTokenResponse)?;
    let expires_in = match raw.expires_in {
        Some(v) if v > 0 => v,
        _ => return Err(KimiAuthError::InvalidTokenResponse),
    };
    Ok(KimiTokenSet {
        access_token: access,
        refresh_token: refresh,
        expires_in_secs: expires_in,
    })
}

pub fn parse_device_poll_response(body: &str) -> Result<DevicePollOutcome, KimiAuthError> {
    let raw: TokenJson =
        serde_json::from_str(body).map_err(|_| KimiAuthError::InvalidTokenResponse)?;
    if raw.access_token.is_some() {
        return Ok(DevicePollOutcome::Authorized {
            token: parse_token_success(raw)?,
        });
    }
    Ok(match raw.error.as_deref().unwrap_or("") {
        "authorization_pending" => DevicePollOutcome::Pending,
        "slow_down" => DevicePollOutcome::SlowDown,
        "access_denied" => DevicePollOutcome::Denied,
        "expired_token" => DevicePollOutcome::Expired,
        _ => DevicePollOutcome::OtherError,
    })
}

/// Map a granted token set onto broker credentials.
///
/// The stored `expires` bakes in the upstream refresh margin
/// (`max(300, expires_in / 2)` seconds, capped so a fresh token is never
/// instantly "expired"), so the repo-wide bare `now >= expires` check
/// refreshes on the same schedule as the official CLI.
pub fn credentials_from_token(token: KimiTokenSet, now_ms: u64) -> OAuthCredentials {
    let margin_secs = MIN_REFRESH_MARGIN_SECS
        .max(token.expires_in_secs / 2)
        .min(token.expires_in_secs.saturating_sub(MARGIN_HEADROOM_SECS));
    let expires = now_ms
        .saturating_add(token.expires_in_secs.saturating_mul(1000))
        .saturating_sub(margin_secs.saturating_mul(1000));
    OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: token.refresh_token,
        access: token.access_token,
        expires,
        account_id: None,
    }
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
) -> Result<String, KimiAuthError> {
    use futures::StreamExt;
    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| KimiAuthError::Transport)?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(KimiAuthError::ResponseTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| KimiAuthError::Transport)
}

// ── Device-flow state machine ────────────────────────────────────────────────

/// Start device authorization (POST /api/oauth/device_authorization).
pub async fn start_device_authorization<H, C>(
    http: &H,
    clock: &C,
) -> Result<DeviceAuthorization, KimiAuthError>
where
    H: KimiHttp,
    C: KimiClock,
{
    validate_oauth_endpoint(DEVICE_AUTH_URL)?;
    let resp = http
        .post_form(DEVICE_AUTH_URL, &[("client_id", CLIENT_ID)])
        .await?;
    if !(200..300).contains(&resp.status) {
        return Err(KimiAuthError::HttpStatus(resp.status));
    }
    parse_device_code_response(&resp.body, clock.now_millis())
}

/// One poll against the token endpoint.
pub async fn poll_device_token<H>(
    http: &H,
    device_code: &str,
) -> Result<DevicePollOutcome, KimiAuthError>
where
    H: KimiHttp,
{
    validate_oauth_endpoint(TOKEN_URL)?;
    let resp = http
        .post_form(
            TOKEN_URL,
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", DEVICE_GRANT_TYPE),
            ],
        )
        .await?;
    // Upstream contract: pending/denied/expired arrive as OAuth error bodies
    // with 4xx statuses — classify by body first, status second.
    if resp.status >= 500 {
        return Err(KimiAuthError::HttpStatus(resp.status));
    }
    match parse_device_poll_response(&resp.body) {
        Ok(DevicePollOutcome::OtherError) if !(200..300).contains(&resp.status) => {
            Err(KimiAuthError::HttpStatus(resp.status))
        }
        Ok(outcome) => Ok(outcome),
        Err(err) if !(200..300).contains(&resp.status) => {
            let _ = err;
            Err(KimiAuthError::HttpStatus(resp.status))
        }
        Err(err) => Err(err),
    }
}

/// Poll until authorized, denied, expired, cancelled, or deadline.
pub async fn wait_for_device_authorization<H, C, X>(
    http: &H,
    clock: &C,
    authz: &DeviceAuthorization,
    cancel: &X,
) -> Result<KimiTokenSet, KimiAuthError>
where
    H: KimiHttp,
    C: KimiClock,
    X: KimiCancel + ?Sized,
{
    let deadline_ms = authz
        .issued_at_ms
        .saturating_add(authz.expires_in_secs.saturating_mul(1000));
    let mut interval_secs = authz.interval_secs.clamp(1, MAX_POLL_INTERVAL_SECS);

    loop {
        if cancel.is_cancelled() {
            return Err(KimiAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(KimiAuthError::Expired);
        }

        clock
            .sleep_cancellable(Duration::from_secs(interval_secs), cancel)
            .await?;

        if cancel.is_cancelled() {
            return Err(KimiAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(KimiAuthError::Expired);
        }

        match poll_device_token(http, &authz.device_code).await? {
            DevicePollOutcome::Pending => continue,
            DevicePollOutcome::SlowDown => {
                interval_secs = apply_slow_down(interval_secs);
            }
            DevicePollOutcome::Authorized { token } => return Ok(token),
            DevicePollOutcome::Denied => return Err(KimiAuthError::AccessDenied),
            DevicePollOutcome::Expired => return Err(KimiAuthError::Expired),
            DevicePollOutcome::OtherError => return Err(KimiAuthError::InvalidTokenResponse),
        }
    }
}

// ── Refresh (rotating; retry transient failures) ─────────────────────────────

/// Classify one refresh response. Pure; drives the retry loop.
pub fn classify_refresh_response(
    status: u16,
    body: &str,
) -> Result<KimiTokenSet, RefreshDisposition> {
    let raw: Option<TokenJson> = serde_json::from_str(body).ok();
    if (200..300).contains(&status) {
        if let Some(raw) = raw {
            if raw.access_token.is_some() {
                return parse_token_success(raw).map_err(RefreshDisposition::Fatal);
            }
        }
        return Err(RefreshDisposition::Fatal(
            KimiAuthError::InvalidTokenResponse,
        ));
    }
    let error_code = raw.and_then(|r| r.error).unwrap_or_default();
    if status == 401 || status == 403 || error_code == "invalid_grant" {
        return Err(RefreshDisposition::Fatal(KimiAuthError::Unauthorized));
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return Err(RefreshDisposition::Retry(KimiAuthError::HttpStatus(status)));
    }
    Err(RefreshDisposition::Fatal(KimiAuthError::HttpStatus(status)))
}

/// Whether one refresh attempt should be retried or surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshDisposition {
    Retry(KimiAuthError),
    Fatal(KimiAuthError),
}

/// Refresh with upstream retry semantics: up to 3 attempts, exponential
/// backoff on 429/5xx, immediate failure on 401/403/`invalid_grant`.
///
/// The caller (`ensure_fresh_provider_token`) persists the returned
/// credentials — including the **rotated** refresh token — atomically.
pub async fn refresh_with<H, C>(
    http: &H,
    clock: &C,
    refresh_token: &str,
) -> Result<OAuthCredentials, KimiAuthError>
where
    H: KimiHttp,
    C: KimiClock,
{
    if refresh_token.trim().is_empty() {
        return Err(KimiAuthError::Unauthorized);
    }
    validate_oauth_endpoint(TOKEN_URL)?;
    let never_cancelled = AtomicBool::new(false);
    let mut last_err = KimiAuthError::Transport;
    for attempt in 0..REFRESH_MAX_ATTEMPTS {
        if attempt > 0 {
            let backoff_ms = REFRESH_BACKOFF_BASE_MS.saturating_mul(1 << (attempt - 1));
            clock
                .sleep_cancellable(Duration::from_millis(backoff_ms), &never_cancelled)
                .await?;
        }
        let resp = match http
            .post_form(
                TOKEN_URL,
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ],
            )
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                // Transport-level failures are retryable (upstream parity).
                last_err = err;
                continue;
            }
        };
        match classify_refresh_response(resp.status, &resp.body) {
            Ok(token) => return Ok(credentials_from_token(token, clock.now_millis())),
            Err(RefreshDisposition::Retry(err)) => {
                last_err = err;
            }
            Err(RefreshDisposition::Fatal(err)) => return Err(err),
        }
    }
    Err(last_err)
}

// ── Full login (injectable) ──────────────────────────────────────────────────

pub type UserCodeHook<'a> = &'a dyn Fn(&str, &str);

pub struct LoginHooks<'a, B> {
    pub browser: &'a B,
    /// When set, the user_code + verification URI are written here (tests / TUI).
    pub on_user_code: Option<UserCodeHook<'a>>,
}

/// Device start → user prompt → poll → atomic persist.
pub async fn login_with<H, C, B, X>(
    http: &H,
    clock: &C,
    hooks: LoginHooks<'_, B>,
    cancel: &X,
    persist: bool,
) -> Result<OAuthCredentials, KimiAuthError>
where
    H: KimiHttp,
    C: KimiClock,
    B: KimiBrowser,
    X: KimiCancel + ?Sized,
{
    let authz = start_device_authorization(http, clock).await?;
    if let Some(cb) = hooks.on_user_code {
        cb(&authz.user_code, &authz.verification_uri_complete);
    } else {
        eprintln!("\n\x1b[1mKimi Code device login\x1b[0m");
        eprintln!("  Confirm code: \x1b[36m{}\x1b[0m", authz.user_code);
        eprintln!(
            "  At:           \x1b[36m{}\x1b[0m\n",
            authz.verification_uri_complete
        );
    }
    // Open only the validated verification URI (never an arbitrary redirect).
    validate_verification_uri(&authz.verification_uri_complete)?;
    let _ = hooks.browser.open(&authz.verification_uri_complete);

    let token = wait_for_device_authorization(http, clock, &authz, cancel).await?;
    let creds = credentials_from_token(token, clock.now_millis());
    if persist {
        save_provider_auth(PROVIDER, &creds).map_err(|_| KimiAuthError::Persist)?;
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

/// Broker refresh: rotate the token pair (no browser). The caller persists
/// the rotated credentials via the provider single-flight gate.
pub async fn refresh_token(_client: &Client, refresh: &str) -> Result<OAuthCredentials, String> {
    let http = ProductionHttp::new().map_err(|e| e.to_string())?;
    let clock = ProductionClock;
    refresh_with(&http, &clock, refresh)
        .await
        .map_err(|e| e.to_string())
}

// ── Production adapters ──────────────────────────────────────────────────────

struct ProductionHttp {
    /// No-redirect client with connect + request timeouts for secret-bearing calls.
    client: Client,
    /// Stable device id resolved once per flow.
    device_id: String,
}

impl ProductionHttp {
    fn new() -> Result<Self, KimiAuthError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| KimiAuthError::Transport)?;
        Ok(Self {
            client,
            device_id: load_or_create_device_id(),
        })
    }
}

#[async_trait]
impl KimiHttp for ProductionHttp {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, KimiAuthError> {
        validate_oauth_endpoint(url)?;
        let mut req = self.client.post(url);
        for (name, value) in identity_request_headers(&self.device_id) {
            req = req.header(name, value);
        }
        let response = req
            .form(form)
            .send()
            .await
            .map_err(|_| KimiAuthError::Transport)?;
        let status = response.status().as_u16();
        // Redirects are disabled; a 3xx means fail closed without following.
        if (300..400).contains(&status) {
            return Err(KimiAuthError::UntrustedEndpoint);
        }
        let body = read_body_capped(response, MAX_RESPONSE_BODY_BYTES).await?;
        Ok(InjectedHttpResponse { status, body })
    }
}

struct ProductionClock;

#[async_trait]
impl KimiClock for ProductionClock {
    fn now_millis(&self) -> u64 {
        now_millis()
    }

    async fn sleep_cancellable<X: KimiCancel + ?Sized>(
        &self,
        duration: Duration,
        cancel: &X,
    ) -> Result<(), KimiAuthError> {
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if cancel.is_cancelled() {
                return Err(KimiAuthError::Cancelled);
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
            return Err(KimiAuthError::Cancelled);
        }
        Ok(())
    }
}

struct ProductionBrowser;

impl KimiBrowser for ProductionBrowser {
    fn open(&self, url: &str) -> Result<(), KimiAuthError> {
        validate_verification_uri(url)?;
        let _ = open_browser(url);
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── Test doubles ─────────────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeClock {
        now: Mutex<u64>,
        sleeps: Mutex<Vec<u64>>,
    }

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self {
                now: Mutex::new(now),
                sleeps: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl KimiClock for FakeClock {
        fn now_millis(&self) -> u64 {
            *self.now.lock().unwrap()
        }
        async fn sleep_cancellable<X: KimiCancel + ?Sized>(
            &self,
            duration: Duration,
            cancel: &X,
        ) -> Result<(), KimiAuthError> {
            if cancel.is_cancelled() {
                return Err(KimiAuthError::Cancelled);
            }
            let ms = duration.as_millis() as u64;
            self.sleeps.lock().unwrap().push(ms);
            *self.now.lock().unwrap() += ms;
            Ok(())
        }
    }

    /// Scripted HTTP: pops responses front-to-back, recording each form body.
    type RecordedForm = (String, Vec<(String, String)>);

    #[derive(Default)]
    struct FakeHttp {
        responses: Mutex<Vec<InjectedHttpResponse>>,
        requests: Mutex<Vec<RecordedForm>>,
    }

    impl FakeHttp {
        fn scripted(responses: Vec<InjectedHttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl KimiHttp for FakeHttp {
        async fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<InjectedHttpResponse, KimiAuthError> {
            self.requests.lock().unwrap().push((
                url.to_string(),
                form.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(KimiAuthError::Transport);
            }
            Ok(responses.remove(0))
        }
    }

    struct NoopBrowser;
    impl KimiBrowser for NoopBrowser {
        fn open(&self, _url: &str) -> Result<(), KimiAuthError> {
            Ok(())
        }
    }

    fn ok(body: &str) -> InjectedHttpResponse {
        InjectedHttpResponse {
            status: 200,
            body: body.to_string(),
        }
    }

    fn err(status: u16, body: &str) -> InjectedHttpResponse {
        InjectedHttpResponse {
            status,
            body: body.to_string(),
        }
    }

    const DEVICE_OK: &str = r#"{
        "device_code": "dev-123",
        "user_code": "ABCD-EFGH",
        "verification_uri": "https://www.kimi.com/code/device",
        "verification_uri_complete": "https://www.kimi.com/code/device?user_code=ABCD-EFGH",
        "expires_in": 300,
        "interval": 5
    }"#;

    const TOKEN_OK: &str = r#"{
        "access_token": "acc-1",
        "refresh_token": "ref-1",
        "expires_in": 900,
        "scope": "kimi-code",
        "token_type": "Bearer"
    }"#;

    // ── Endpoint pinning ─────────────────────────────────────────────────────

    #[test]
    fn oauth_endpoint_pinning_accepts_only_exact_urls() {
        assert!(validate_oauth_endpoint(DEVICE_AUTH_URL).is_ok());
        assert!(validate_oauth_endpoint(TOKEN_URL).is_ok());
        for bad in [
            "https://auth.kimi.com/api/oauth/other",
            "http://auth.kimi.com/api/oauth/token",
            "https://auth.kimi.com.evil.example/api/oauth/token",
            "https://api.kimi.com/api/oauth/token",
        ] {
            assert_eq!(
                validate_oauth_endpoint(bad),
                Err(KimiAuthError::UntrustedEndpoint),
                "must reject {bad}"
            );
        }
    }

    #[test]
    fn verification_uri_pinning() {
        assert!(validate_verification_uri("https://www.kimi.com/code/device?user_code=X").is_ok());
        assert!(validate_verification_uri("https://kimi.com/device").is_ok());
        for bad in [
            "http://www.kimi.com/device",
            "https://kimi.com.evil.example/device",
            "https://evilkimi.com/device",
            "https://user@www.kimi.com/device",
            "https://www.kimi.com:8443/device",
            "https://www.kimi.com/device#frag",
        ] {
            assert_eq!(
                validate_verification_uri(bad),
                Err(KimiAuthError::UntrustedEndpoint),
                "must reject {bad}"
            );
        }
    }

    // ── Parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn parses_device_code_response() {
        let authz = parse_device_code_response(DEVICE_OK, 1_000).expect("parse");
        assert_eq!(authz.user_code, "ABCD-EFGH");
        assert_eq!(authz.device_code, "dev-123");
        assert_eq!(authz.expires_in_secs, 300);
        assert_eq!(authz.interval_secs, 5);
        assert_eq!(authz.issued_at_ms, 1_000);
        assert!(authz.verification_uri_complete.contains("user_code"));
    }

    #[test]
    fn device_response_defaults_missing_expires_and_interval() {
        let body = r#"{
            "device_code": "d",
            "user_code": "u",
            "verification_uri_complete": "https://www.kimi.com/code"
        }"#;
        let authz = parse_device_code_response(body, 0).expect("parse");
        assert_eq!(authz.expires_in_secs, DEFAULT_DEVICE_EXPIRES_IN_SECS);
        assert_eq!(authz.interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(authz.verification_uri, "");
    }

    #[test]
    fn device_response_requires_complete_uri_and_codes() {
        for body in [
            r#"{"device_code":"", "user_code":"u", "verification_uri_complete":"https://kimi.com/x"}"#,
            r#"{"device_code":"d", "user_code":" ", "verification_uri_complete":"https://kimi.com/x"}"#,
            r#"{"device_code":"d", "user_code":"u", "verification_uri_complete":""}"#,
            r#"{"device_code":"d", "user_code":"u"}"#,
            "not-json",
        ] {
            assert!(parse_device_code_response(body, 0).is_err(), "body: {body}");
        }
    }

    #[test]
    fn device_response_rejects_foreign_verification_host() {
        let body = r#"{
            "device_code": "d",
            "user_code": "u",
            "verification_uri_complete": "https://phish.example/kimi"
        }"#;
        assert_eq!(
            parse_device_code_response(body, 0).unwrap_err(),
            KimiAuthError::UntrustedEndpoint
        );
    }

    #[test]
    fn poll_outcomes_map_oauth_error_codes() {
        for (code, expected) in [
            ("authorization_pending", DevicePollOutcome::Pending),
            ("slow_down", DevicePollOutcome::SlowDown),
            ("access_denied", DevicePollOutcome::Denied),
            ("expired_token", DevicePollOutcome::Expired),
            ("some_other", DevicePollOutcome::OtherError),
        ] {
            let body = format!(r#"{{"error":"{code}"}}"#);
            assert_eq!(parse_device_poll_response(&body).unwrap(), expected);
        }
    }

    #[test]
    fn poll_success_requires_rotating_refresh_and_expiry() {
        let ok = parse_device_poll_response(TOKEN_OK).unwrap();
        match ok {
            DevicePollOutcome::Authorized { token } => {
                assert_eq!(token.access_token, "acc-1");
                assert_eq!(token.refresh_token, "ref-1");
                assert_eq!(token.expires_in_secs, 900);
            }
            other => panic!("expected Authorized, got {other:?}"),
        }
        // Missing refresh_token → invalid (Kimi always rotates).
        assert!(parse_device_poll_response(r#"{"access_token":"a","expires_in":900}"#).is_err());
        // Missing/zero expires_in → invalid (upstream contract).
        assert!(parse_device_poll_response(r#"{"access_token":"a","refresh_token":"r"}"#).is_err());
        assert!(parse_device_poll_response(
            r#"{"access_token":"a","refresh_token":"r","expires_in":0}"#
        )
        .is_err());
    }

    #[test]
    fn credentials_bake_upstream_refresh_margin() {
        // 900s token, margin = max(300, 450) = 450s → expires = now + 450s.
        let creds = credentials_from_token(
            KimiTokenSet {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_in_secs: 900,
            },
            1_000_000,
        );
        assert_eq!(creds.expires, 1_000_000 + 450 * 1000);
        assert_eq!(creds.auth_type, "oauth");
        // Short-lived token: margin must leave headroom, not zero the expiry.
        let short = credentials_from_token(
            KimiTokenSet {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_in_secs: 120,
            },
            0,
        );
        assert_eq!(short.expires, (120 - 60) * 1000);
    }

    #[test]
    fn slow_down_grows_and_caps() {
        assert_eq!(apply_slow_down(5), 10);
        assert_eq!(apply_slow_down(28), 30);
        assert_eq!(apply_slow_down(30), 30);
    }

    // ── Refresh classification ───────────────────────────────────────────────

    #[test]
    fn refresh_classification_matrix() {
        assert!(classify_refresh_response(200, TOKEN_OK).is_ok());
        assert_eq!(
            classify_refresh_response(401, "{}").unwrap_err(),
            RefreshDisposition::Fatal(KimiAuthError::Unauthorized)
        );
        assert_eq!(
            classify_refresh_response(403, "{}").unwrap_err(),
            RefreshDisposition::Fatal(KimiAuthError::Unauthorized)
        );
        assert_eq!(
            classify_refresh_response(400, r#"{"error":"invalid_grant"}"#).unwrap_err(),
            RefreshDisposition::Fatal(KimiAuthError::Unauthorized)
        );
        for status in [429u16, 500, 502, 503, 504] {
            assert_eq!(
                classify_refresh_response(status, "{}").unwrap_err(),
                RefreshDisposition::Retry(KimiAuthError::HttpStatus(status)),
                "status {status} must be retryable"
            );
        }
        assert_eq!(
            classify_refresh_response(418, "{}").unwrap_err(),
            RefreshDisposition::Fatal(KimiAuthError::HttpStatus(418))
        );
        // 200 with garbage body must not be accepted.
        assert_eq!(
            classify_refresh_response(200, "{}").unwrap_err(),
            RefreshDisposition::Fatal(KimiAuthError::InvalidTokenResponse)
        );
    }

    // ── State machine (async, deterministic) ─────────────────────────────────

    #[tokio::test]
    async fn login_happy_path_persist_disabled() {
        let http = FakeHttp::scripted(vec![
            ok(DEVICE_OK),
            err(400, r#"{"error":"authorization_pending"}"#),
            ok(TOKEN_OK),
        ]);
        let clock = FakeClock::new(0);
        let cancel = AtomicBool::new(false);
        let creds = login_with(
            &http,
            &clock,
            LoginHooks {
                browser: &NoopBrowser,
                on_user_code: Some(&|code, uri| {
                    assert_eq!(code, "ABCD-EFGH");
                    assert!(uri.starts_with("https://www.kimi.com/"));
                }),
            },
            &cancel,
            false,
        )
        .await
        .expect("login");
        assert_eq!(creds.access, "acc-1");
        assert_eq!(creds.refresh, "ref-1");

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, DEVICE_AUTH_URL);
        assert_eq!(requests[1].0, TOKEN_URL);
        // Poll must carry the device grant type + device code.
        let poll_form = &requests[1].1;
        assert!(poll_form.contains(&("grant_type".into(), DEVICE_GRANT_TYPE.into())));
        assert!(poll_form.contains(&("device_code".into(), "dev-123".into())));
    }

    #[tokio::test]
    async fn slow_down_extends_poll_interval() {
        let http = FakeHttp::scripted(vec![
            ok(DEVICE_OK),
            err(400, r#"{"error":"slow_down"}"#),
            ok(TOKEN_OK),
        ]);
        let clock = FakeClock::new(0);
        let cancel = AtomicBool::new(false);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .expect("authorized");
        let sleeps = clock.sleeps.lock().unwrap();
        assert_eq!(*sleeps, vec![5_000, 10_000]);
    }

    #[tokio::test]
    async fn denied_and_expired_terminate() {
        for (body, expected) in [
            (r#"{"error":"access_denied"}"#, KimiAuthError::AccessDenied),
            (r#"{"error":"expired_token"}"#, KimiAuthError::Expired),
        ] {
            let http = FakeHttp::scripted(vec![ok(DEVICE_OK), err(400, body)]);
            let clock = FakeClock::new(0);
            let cancel = AtomicBool::new(false);
            let authz = start_device_authorization(&http, &clock).await.unwrap();
            let got = wait_for_device_authorization(&http, &clock, &authz, &cancel)
                .await
                .unwrap_err();
            assert_eq!(got, expected);
        }
    }

    #[tokio::test]
    async fn deadline_expires_without_server_grant() {
        // expires_in 300s; each pending poll advances 5s of fake time.
        let mut responses = vec![ok(DEVICE_OK)];
        for _ in 0..70 {
            responses.push(err(400, r#"{"error":"authorization_pending"}"#));
        }
        let http = FakeHttp::scripted(responses);
        let clock = FakeClock::new(0);
        let cancel = AtomicBool::new(false);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let got = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(got, KimiAuthError::Expired);
    }

    #[tokio::test]
    async fn cancellation_stops_polling() {
        let http = FakeHttp::scripted(vec![ok(DEVICE_OK)]);
        let clock = FakeClock::new(0);
        let cancel = AtomicBool::new(true);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let got = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(got, KimiAuthError::Cancelled);
    }

    #[tokio::test]
    async fn server_5xx_during_poll_is_fatal() {
        let http = FakeHttp::scripted(vec![ok(DEVICE_OK), err(502, "bad gateway")]);
        let clock = FakeClock::new(0);
        let cancel = AtomicBool::new(false);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let got = wait_for_device_authorization(&http, &clock, &authz, &cancel)
            .await
            .unwrap_err();
        assert_eq!(got, KimiAuthError::HttpStatus(502));
    }

    #[tokio::test]
    async fn refresh_retries_transient_then_succeeds() {
        let http = FakeHttp::scripted(vec![err(503, "{}"), err(429, "{}"), ok(TOKEN_OK)]);
        let clock = FakeClock::new(0);
        let creds = refresh_with(&http, &clock, "old-refresh")
            .await
            .expect("refresh");
        assert_eq!(creds.access, "acc-1");
        // Rotated refresh token must replace the old one.
        assert_eq!(creds.refresh, "ref-1");
        // Backoff: 1s then 2s.
        assert_eq!(*clock.sleeps.lock().unwrap(), vec![1_000, 2_000]);
        // Refresh form carries the refresh grant, never the device grant.
        let requests = http.requests.lock().unwrap();
        for (_, form) in requests.iter() {
            assert!(form.contains(&("grant_type".into(), "refresh_token".into())));
        }
    }

    #[tokio::test]
    async fn refresh_unauthorized_is_immediate_no_retry() {
        let http = FakeHttp::scripted(vec![err(401, "{}"), ok(TOKEN_OK)]);
        let clock = FakeClock::new(0);
        let got = refresh_with(&http, &clock, "dead-refresh")
            .await
            .unwrap_err();
        assert_eq!(got, KimiAuthError::Unauthorized);
        // Exactly one request — the scripted 200 must remain unconsumed.
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_exhausts_retries() {
        let http = FakeHttp::scripted(vec![err(500, "{}"), err(500, "{}"), err(500, "{}")]);
        let clock = FakeClock::new(0);
        let got = refresh_with(&http, &clock, "r").await.unwrap_err();
        assert_eq!(got, KimiAuthError::HttpStatus(500));
        assert_eq!(http.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn refresh_rejects_empty_refresh_token() {
        let http = FakeHttp::scripted(vec![ok(TOKEN_OK)]);
        let clock = FakeClock::new(0);
        let got = refresh_with(&http, &clock, "  ").await.unwrap_err();
        assert_eq!(got, KimiAuthError::Unauthorized);
        assert!(http.requests.lock().unwrap().is_empty());
    }

    // ── Identity headers ─────────────────────────────────────────────────────

    #[test]
    fn identity_headers_cover_msh_surface_without_secrets() {
        let headers = identity_request_headers("4d2b0725-9e2e-4b8f-b57b-6011ecf1c3eb");
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        for required in [
            "User-Agent",
            "X-Msh-Platform",
            "X-Msh-Version",
            "X-Msh-Device-Id",
            "X-Msh-Device-Name",
            "X-Msh-Device-Model",
            "X-Msh-Os-Version",
        ] {
            assert!(names.contains(&required), "missing header {required}");
        }
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("X-Msh-Platform"), MSH_PLATFORM);
        assert_eq!(get("User-Agent"), USER_AGENT);
        assert_eq!(
            get("X-Msh-Device-Id"),
            "4d2b0725-9e2e-4b8f-b57b-6011ecf1c3eb"
        );
        for (_, value) in &headers {
            assert!(
                value.chars().all(|c| (' '..='~').contains(&c)),
                "header values must be printable ASCII"
            );
        }
    }

    #[test]
    fn ascii_header_sanitizes_and_falls_back() {
        assert_eq!(ascii_header("  héllo wörld  ", "x"), "hllo wrld");
        assert_eq!(ascii_header("\u{7f}\n\t", "fallback"), "fallback");
        assert_eq!(ascii_header("plain", "x"), "plain");
    }

    #[test]
    fn error_display_never_contains_token_material() {
        // Compile-time shape guarantee: no variant stores a String, so no
        // token can transit. Spot-check Display output anyway.
        for err in [
            KimiAuthError::Unauthorized,
            KimiAuthError::InvalidTokenResponse,
            KimiAuthError::HttpStatus(500),
        ] {
            let text = format!("{err} / {err:?}");
            assert!(!text.contains("acc-"), "{text}");
            assert!(!text.contains("ref-"), "{text}");
        }
    }
}
