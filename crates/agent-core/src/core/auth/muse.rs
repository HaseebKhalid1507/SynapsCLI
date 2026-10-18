//! Meta Muse Code OAuth (RFC 8628 device authorization + Model API key mint).
//!
//! Experimental. Flow verified against the official Muse Code CLI
//! (`muse 1.4.6-R6000.1`, installed via `dev.meta.ai/install.sh`) by driving
//! it against local mock servers and reading its embedded constants. The
//! endpoints are not published as a stable third-party API; do not describe
//! them as officially supported.
//!
//! Flow shape (differences from Kimi Code / GitHub Copilot):
//! - Device authorization at `auth.meta.com/oidc/device/{authorization,token}/`
//!   with a public client id, no PKCE and no `scope` parameter.
//! - The OAuth access token is NOT the inference bearer. It is exchanged once
//!   at `POST api.meta.ai/muse-code/key` (`{"onboard":false}`) for a Model
//!   API key, and that key is the `Authorization: Bearer` for
//!   `api.meta.ai/v1/responses` (OpenAI Responses wire). An API key created
//!   at dev.meta.ai is the same kind of credential (`META_API_KEY`).
//! - The CLI never refreshes the OAuth token and never proactively re-mints:
//!   the key is used until the API answers 401, then re-minted ONCE with the
//!   stored OAuth access token and the request retried once. A mint 401 means
//!   the login itself is dead — the user must log in again. We mirror this.
//!
//! Credential mapping:
//! - `OAuthCredentials.refresh` = Meta OAuth access token (broker-owned; used
//!   only to re-mint; never vended, logged, or sent to the inference host)
//! - `OAuthCredentials.access`  = minted Model API key (the inference bearer)
//! - `OAuthCredentials.expires` = nominal re-validation horizon (see
//!   [`API_KEY_NOMINAL_LIFETIME_MS`]); real invalidation is 401-driven.
//!
//! The OAuth refresh token is deliberately not stored: the official client
//! never uses it, so keeping it would only widen what a leaked auth.json
//! exposes.

use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::{now_millis, open_browser, save_provider_auth, OAuthCredentials};

// ── Public constants (evidence: muse 1.4.6-R6000.1 binary + mock capture) ────

/// Canonical storage / broker / model-prefix id.
pub const PROVIDER: &str = "muse";

/// Public native-client id shipped in the official Muse Code CLI. Not a
/// client secret.
pub const CLIENT_ID: &str = "1031625952748946";

/// Meta OAuth host pinned by the official CLI (`TBH_AUTH_BASE_URL` default).
pub const OAUTH_HOST: &str = "auth.meta.com";

/// Device-authorization endpoint (RFC 8628 §3.1). Trailing slash is part of
/// the path the official CLI sends.
pub const DEVICE_AUTH_URL: &str = "https://auth.meta.com/oidc/device/authorization/";

/// Device-code token endpoint (RFC 8628 §3.4).
pub const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";

/// Device-code grant type (RFC 8628).
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Inference/API host (`TBH_MINT_BASE_URL` default).
pub const API_HOST: &str = "api.meta.ai";

/// Origin the broker pins for every Muse request. Paths are allowlisted
/// broker-side (`/v1/responses`, `/muse-code/models`).
pub const API_ORIGIN: &str = "https://api.meta.ai";

/// OpenAI Responses base (`{API_BASE_URL}/responses`).
pub const API_BASE_URL: &str = "https://api.meta.ai/v1";

/// Model-API-key mint endpoint (bearer = OAuth access token).
pub const MINT_URL: &str = "https://api.meta.ai/muse-code/key";

/// `x-api-version` the official CLI sends on the mint request.
pub const MINT_API_VERSION: &str = "1.0.0";

/// Where a Meta account finishes Model API onboarding / billing.
pub const ONBOARDING_URL: &str = "https://dev.meta.ai";

/// Environment override honoured by the official CLI: a Model API key here
/// takes precedence over the account login.
pub const API_KEY_ENV: &str = "META_API_KEY";

/// Upstream error code returned by the mint when the account has not
/// finished Model API onboarding.
pub const ONBOARDING_REQUIRED_CODE: &str = "MODEL_API__ONBOARDING_REQUIRED";

/// Honest User-Agent.
pub const USER_AGENT: &str = concat!("SynapsCLI/", env!("CARGO_PKG_VERSION"));

/// Minted keys carry no expiry. The official CLI uses a key until the API
/// answers 401 and never re-mints proactively; this horizon only keeps the
/// repo-wide expiry check from ever forcing a needless re-mint (each mint is
/// a server-side side effect). Invalidation is 401-driven via the broker's
/// rejected-token recovery.
pub const API_KEY_NOMINAL_LIFETIME_MS: u64 = 365 * 24 * 60 * 60 * 1000;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Hard ceiling so `slow_down` cannot grow unbounded.
const MAX_POLL_INTERVAL_SECS: u64 = 30;
/// Seconds added on each `slow_down` (RFC 8628 §3.5).
const SLOW_DOWN_STEP_SECS: u64 = 5;
/// Local wall-clock budget when the server omits `expires_in`.
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 600;

/// Connect timeout for production device/poll/mint HTTP.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Full request timeout for production device/poll/mint HTTP.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum response body bytes accepted from device/poll/mint endpoints.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;
/// Logical sleep quantum for cancel checks (production wall-clock chunks).
const CANCEL_POLL_QUANTUM: Duration = Duration::from_millis(200);

// ── Secret-safe errors ───────────────────────────────────────────────────────

/// Failures from the Muse auth chain. Variants store **no** device codes,
/// user codes, tokens, keys, or upstream bodies, so Display/Debug cannot leak
/// them.
#[derive(Clone, PartialEq, Eq)]
pub enum MuseAuthError {
    UntrustedEndpoint,
    InvalidDeviceResponse,
    InvalidTokenResponse,
    InvalidMintResponse,
    AccessDenied,
    Expired,
    Cancelled,
    EmptyAccessToken,
    EmptyApiKey,
    /// The Meta account has not finished Model API onboarding.
    OnboardingRequired,
    /// The account must add a payment method / subscription first.
    PaymentRequired,
    /// The mint rejected the stored OAuth login (HTTP 401/403): re-login.
    LoginRejected(u16),
    HttpStatus(u16),
    Transport,
    ResponseTooLarge,
    Persist,
}

impl fmt::Display for MuseAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedEndpoint => f.write_str("untrusted endpoint"),
            Self::InvalidDeviceResponse => f.write_str("invalid device authorization response"),
            Self::InvalidTokenResponse => f.write_str("invalid device token response"),
            Self::InvalidMintResponse => f.write_str("invalid Muse key mint response"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::Expired => f.write_str("device code expired"),
            Self::Cancelled => f.write_str("login cancelled"),
            Self::EmptyAccessToken => f.write_str("empty Meta access token"),
            Self::EmptyApiKey => f.write_str("Muse key mint returned an empty API key"),
            Self::OnboardingRequired => write!(
                f,
                "your Meta account has not finished Model API onboarding — finish it at \
                 {ONBOARDING_URL}, then run `synaps login --provider muse` again"
            ),
            Self::PaymentRequired => write!(
                f,
                "your Meta account needs an active Model API plan or payment method — set it \
                 up at {ONBOARDING_URL}, then run `synaps login --provider muse` again"
            ),
            Self::LoginRejected(code) => write!(
                f,
                "your saved Meta login is no longer valid (HTTP {code}); run \
                 `synaps login --provider muse` again"
            ),
            Self::HttpStatus(code) => write!(f, "HTTP error {code}"),
            Self::Transport => f.write_str("transport error"),
            Self::ResponseTooLarge => f.write_str("response body too large"),
            Self::Persist => f.write_str("credential persistence failed"),
        }
    }
}

impl fmt::Debug for MuseAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MuseAuthError({self})")
    }
}

impl From<MuseAuthError> for String {
    fn from(value: MuseAuthError) -> Self {
        value.to_string()
    }
}

// ── Injectable boundaries ────────────────────────────────────────────────────

/// Minimal HTTP response used by the device-flow / mint state machine.
#[derive(Clone)]
pub struct MuseHttpResponse {
    pub status: u16,
    pub body: String,
}

impl fmt::Debug for MuseHttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MuseHttpResponse")
            .field("status", &self.status)
            .field(
                "body",
                &format_args!("[REDACTED {} bytes]", self.body.len()),
            )
            .finish()
    }
}

/// Injectable HTTP surface so tests never touch the network.
#[async_trait]
pub trait MuseHttp: Send + Sync {
    /// Form POST to a pinned `auth.meta.com` device endpoint.
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<MuseHttpResponse, MuseAuthError>;

    /// JSON POST with bearer auth to the pinned mint endpoint.
    async fn post_json_bearer(
        &self,
        url: &str,
        bearer: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<MuseHttpResponse, MuseAuthError>;
}

/// Injectable clock (now + cancellable sleep) for deterministic poll tests.
#[async_trait]
pub trait MuseClock: Send + Sync {
    fn now_millis(&self) -> u64;

    /// Sleep `duration`, returning early with [`MuseAuthError::Cancelled`]
    /// when `cancel` flips.
    async fn sleep_cancellable(
        &self,
        duration: Duration,
        cancel: &AtomicBool,
    ) -> Result<(), MuseAuthError>;
}

/// Injectable browser opener (production uses `open_browser`).
pub trait MuseBrowser: Send + Sync {
    fn open(&self, url: &str) -> Result<(), MuseAuthError>;
}

// ── Device-flow types ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DeviceAuthorization {
    /// Opaque device code — never logged via public error types.
    pub(crate) device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// `verification_uri_complete` when the server sent a valid one.
    pub verification_uri_complete: Option<String>,
    pub expires_in_secs: u64,
    pub interval_secs: u64,
    pub issued_at_ms: u64,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuthorization")
            .field("device_code", &"[REDACTED]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in_secs", &self.expires_in_secs)
            .field("interval_secs", &self.interval_secs)
            .field("issued_at_ms", &self.issued_at_ms)
            .finish()
    }
}

impl DeviceAuthorization {
    /// URI to open in the browser: the complete form (code pre-filled) when
    /// available, otherwise the bare verification page.
    pub fn browser_uri(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// Outcome of one device-token poll (secret-safe; token only on Authorized).
#[derive(Clone, PartialEq, Eq)]
pub enum DevicePollOutcome {
    Pending,
    SlowDown,
    Authorized { access_token: String },
    Denied,
    Expired,
    OtherError,
}

impl fmt::Debug for DevicePollOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => f.write_str("Pending"),
            Self::SlowDown => f.write_str("SlowDown"),
            Self::Authorized { .. } => f
                .debug_struct("Authorized")
                .field("access_token", &"[REDACTED]")
                .finish(),
            Self::Denied => f.write_str("Denied"),
            Self::Expired => f.write_str("Expired"),
            Self::OtherError => f.write_str("OtherError"),
        }
    }
}

/// A minted Model API key plus the non-secret account facts the mint echoes.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedKey {
    pub api_key: String,
    pub user_email: Option<String>,
    pub subs_active: Option<bool>,
}

impl fmt::Debug for MintedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedKey")
            .field("api_key", &"[REDACTED]")
            .field("user_email", &self.user_email)
            .field("subs_active", &self.subs_active)
            .finish()
    }
}

// ── Endpoint pinning ─────────────────────────────────────────────────────────

/// Accept only the exact pinned HTTPS `auth.meta.com` device endpoints.
pub fn validate_device_endpoint(url: &str) -> Result<(), MuseAuthError> {
    if url != DEVICE_AUTH_URL && url != TOKEN_URL {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    let parsed = Url::parse(url).map_err(|_| MuseAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some(OAUTH_HOST) {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    Ok(())
}

/// Accept only the pinned mint host+path (no query, no alternate host).
pub fn validate_mint_endpoint(url: &str) -> Result<(), MuseAuthError> {
    if url != MINT_URL {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    let parsed = Url::parse(url).map_err(|_| MuseAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some(API_HOST)
        || parsed.path() != "/muse-code/key"
        || parsed.query().is_some()
    {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    Ok(())
}

/// Verification destinations must be plain HTTPS pages on a Meta-owned host
/// (`meta.com` / `*.meta.com`, `meta.ai` / `*.meta.ai`). Rejects userinfo,
/// explicit ports, fragments, and look-alike hosts. Path/query are left to
/// the server (the device page shape is not part of the pinned contract).
pub fn validate_verification_uri(uri: &str) -> Result<(), MuseAuthError> {
    let parsed = Url::parse(uri).map_err(|_| MuseAuthError::UntrustedEndpoint)?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.fragment().is_some()
    {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    let host = parsed.host_str().ok_or(MuseAuthError::UntrustedEndpoint)?;
    let meta_owned = ["meta.com", "meta.ai"]
        .iter()
        .any(|root| host == *root || host.ends_with(&format!(".{root}")));
    if !meta_owned {
        return Err(MuseAuthError::UntrustedEndpoint);
    }
    Ok(())
}

// ── Mint request headers ─────────────────────────────────────────────────────

/// Non-secret headers attached to the pinned mint request (bearer is set by
/// the HTTP adapter and can never be overridden through this list).
pub fn mint_request_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("x-api-version", MINT_API_VERSION),
        ("User-Agent", USER_AGENT),
    ]
}

/// Exact mint body the official CLI sends for a returning login.
pub const MINT_BODY: &str = r#"{"onboard":false}"#;

// ── Parsing (pure) ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeJson {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

impl fmt::Debug for DeviceCodeJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCodeJson")
            .field("device_code", &"[REDACTED]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

#[derive(Deserialize)]
struct DeviceTokenJson {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl fmt::Debug for DeviceTokenJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceTokenJson")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Deserialize)]
struct MintJson {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    is_subs_active: Option<bool>,
    #[serde(default)]
    require_payment: Option<bool>,
}

pub fn parse_device_code_response(
    body: &str,
    now_ms: u64,
) -> Result<DeviceAuthorization, MuseAuthError> {
    let raw: DeviceCodeJson =
        serde_json::from_str(body).map_err(|_| MuseAuthError::InvalidDeviceResponse)?;
    if raw.device_code.trim().is_empty() || raw.user_code.trim().is_empty() {
        return Err(MuseAuthError::InvalidDeviceResponse);
    }
    validate_verification_uri(&raw.verification_uri)?;
    // A malformed/foreign complete URI is dropped (we fall back to the bare
    // page + displayed code) rather than failing an otherwise valid login.
    let verification_uri_complete = raw
        .verification_uri_complete
        .filter(|uri| validate_verification_uri(uri).is_ok());
    let expires_in = match raw.expires_in {
        Some(0) => return Err(MuseAuthError::InvalidDeviceResponse),
        Some(secs) => secs,
        None => DEFAULT_DEVICE_EXPIRES_IN_SECS,
    };
    let interval = raw
        .interval
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .clamp(1, MAX_POLL_INTERVAL_SECS);
    Ok(DeviceAuthorization {
        device_code: raw.device_code,
        user_code: raw.user_code,
        verification_uri: raw.verification_uri,
        verification_uri_complete,
        expires_in_secs: expires_in,
        interval_secs: interval,
        issued_at_ms: now_ms,
    })
}

pub fn parse_device_poll_response(body: &str) -> Result<DevicePollOutcome, MuseAuthError> {
    let raw: DeviceTokenJson =
        serde_json::from_str(body).map_err(|_| MuseAuthError::InvalidTokenResponse)?;
    if let Some(token) = raw.access_token {
        if token.trim().is_empty() {
            return Err(MuseAuthError::EmptyAccessToken);
        }
        return Ok(DevicePollOutcome::Authorized {
            access_token: token,
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

/// Classify a mint response. Upstream bytes never enter the error: the
/// onboarding code is matched as a fixed token, everything else maps to a
/// static variant.
pub fn parse_mint_response(status: u16, body: &str) -> Result<MintedKey, MuseAuthError> {
    if body.contains(ONBOARDING_REQUIRED_CODE) {
        return Err(MuseAuthError::OnboardingRequired);
    }
    if status == 401 || status == 403 {
        return Err(MuseAuthError::LoginRejected(status));
    }
    if !(200..300).contains(&status) {
        return Err(MuseAuthError::HttpStatus(status));
    }
    let raw: MintJson =
        serde_json::from_str(body).map_err(|_| MuseAuthError::InvalidMintResponse)?;
    match raw.api_key.filter(|key| !key.trim().is_empty()) {
        Some(api_key) => Ok(MintedKey {
            api_key,
            user_email: raw.user_email.filter(|e| !e.trim().is_empty()),
            subs_active: raw.is_subs_active,
        }),
        None if raw.require_payment == Some(true) => Err(MuseAuthError::PaymentRequired),
        None => Err(MuseAuthError::EmptyApiKey),
    }
}

/// OAuth access token → `refresh`; minted key → `access` (the bearer).
pub fn credentials_from_mint(
    oauth_access_token: &str,
    minted: &MintedKey,
    now_ms: u64,
) -> Result<OAuthCredentials, MuseAuthError> {
    if oauth_access_token.trim().is_empty() {
        return Err(MuseAuthError::EmptyAccessToken);
    }
    if minted.api_key.trim().is_empty() {
        return Err(MuseAuthError::EmptyApiKey);
    }
    Ok(OAuthCredentials {
        auth_type: "oauth".into(),
        refresh: oauth_access_token.to_string(),
        access: minted.api_key.clone(),
        expires: now_ms.saturating_add(API_KEY_NOMINAL_LIFETIME_MS),
        account_id: None,
    })
}

pub fn apply_slow_down(interval_secs: u64) -> u64 {
    interval_secs
        .saturating_add(SLOW_DOWN_STEP_SECS)
        .min(MAX_POLL_INTERVAL_SECS)
}

/// Model API key from the environment override, if set and non-blank.
pub fn api_key_from_env() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// Presence-only check for the `META_API_KEY` override (the value never
/// leaves this module). Used by pickers: Muse is usable without a stored
/// login when the override is set.
pub fn env_api_key_present() -> bool {
    api_key_from_env().is_some()
}

// ── Bounded body read ────────────────────────────────────────────────────────

/// Read a response body with a hard byte cap. Fail closed if exceeded.
pub async fn read_body_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<String, MuseAuthError> {
    use futures::StreamExt;
    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| MuseAuthError::Transport)?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(MuseAuthError::ResponseTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| MuseAuthError::Transport)
}

// ── Device-flow state machine ────────────────────────────────────────────────

/// Start device authorization (POST device/authorization/).
pub async fn start_device_authorization<H, C>(
    http: &H,
    clock: &C,
) -> Result<DeviceAuthorization, MuseAuthError>
where
    H: MuseHttp,
    C: MuseClock,
{
    validate_device_endpoint(DEVICE_AUTH_URL)?;
    let resp = http
        .post_form(DEVICE_AUTH_URL, &[("client_id", CLIENT_ID)])
        .await?;
    if !(200..300).contains(&resp.status) {
        return Err(MuseAuthError::HttpStatus(resp.status));
    }
    parse_device_code_response(&resp.body, clock.now_millis())
}

/// One poll against the token endpoint.
pub async fn poll_device_token<H: MuseHttp>(
    http: &H,
    device_code: &str,
) -> Result<DevicePollOutcome, MuseAuthError> {
    validate_device_endpoint(TOKEN_URL)?;
    let resp = http
        .post_form(
            TOKEN_URL,
            &[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", device_code),
                ("client_id", CLIENT_ID),
            ],
        )
        .await?;
    let parsed = parse_device_poll_response(&resp.body);
    if (200..300).contains(&resp.status) {
        return parsed;
    }
    // RFC 8628 errors arrive as HTTP 400 with an `error` body; only those
    // recognised outcomes are meaningful on a non-2xx status.
    match parsed {
        Ok(
            outcome @ (DevicePollOutcome::Pending
            | DevicePollOutcome::SlowDown
            | DevicePollOutcome::Denied
            | DevicePollOutcome::Expired),
        ) => Ok(outcome),
        _ => Err(MuseAuthError::HttpStatus(resp.status)),
    }
}

/// Poll until authorized, denied, expired, cancelled, or deadline. Returns
/// the OAuth access token.
pub async fn wait_for_device_authorization<H, C>(
    http: &H,
    clock: &C,
    authz: &DeviceAuthorization,
    cancel: &AtomicBool,
) -> Result<String, MuseAuthError>
where
    H: MuseHttp,
    C: MuseClock,
{
    let deadline_ms = authz
        .issued_at_ms
        .saturating_add(authz.expires_in_secs.saturating_mul(1000));
    let mut interval_secs = authz.interval_secs.clamp(1, MAX_POLL_INTERVAL_SECS);
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(MuseAuthError::Cancelled);
        }
        if clock.now_millis() >= deadline_ms {
            return Err(MuseAuthError::Expired);
        }
        clock
            .sleep_cancellable(Duration::from_secs(interval_secs), cancel)
            .await?;
        if clock.now_millis() >= deadline_ms {
            return Err(MuseAuthError::Expired);
        }
        match poll_device_token(http, &authz.device_code).await? {
            DevicePollOutcome::Pending => continue,
            DevicePollOutcome::SlowDown => interval_secs = apply_slow_down(interval_secs),
            DevicePollOutcome::Authorized { access_token } => return Ok(access_token),
            DevicePollOutcome::Denied => return Err(MuseAuthError::AccessDenied),
            DevicePollOutcome::Expired => return Err(MuseAuthError::Expired),
            DevicePollOutcome::OtherError => return Err(MuseAuthError::InvalidTokenResponse),
        }
    }
}

// ── Key mint (pinned, no-redirect at production client) ──────────────────────

/// Exchange the Meta OAuth access token for a Model API key. The OAuth token
/// is sent only to the pinned mint URL. Callers must never log it.
pub async fn mint_api_key<H: MuseHttp>(
    http: &H,
    oauth_access_token: &str,
) -> Result<MintedKey, MuseAuthError> {
    if oauth_access_token.trim().is_empty() {
        return Err(MuseAuthError::EmptyAccessToken);
    }
    validate_mint_endpoint(MINT_URL)?;
    let resp = http
        .post_json_bearer(
            MINT_URL,
            oauth_access_token,
            mint_request_headers(),
            MINT_BODY,
        )
        .await?;
    parse_mint_response(resp.status, &resp.body)
}

/// Mint and build the stored credential.
pub async fn mint_credentials<H, C>(
    http: &H,
    clock: &C,
    oauth_access_token: &str,
) -> Result<(OAuthCredentials, MintedKey), MuseAuthError>
where
    H: MuseHttp,
    C: MuseClock,
{
    let minted = mint_api_key(http, oauth_access_token).await?;
    let creds = credentials_from_mint(oauth_access_token, &minted, clock.now_millis())?;
    Ok((creds, minted))
}

// ── Full login (injectable) ──────────────────────────────────────────────────

pub type UserCodeHook<'a> = &'a dyn Fn(&str, &str);

pub struct LoginHooks<'a, B> {
    pub browser: &'a B,
    /// When set, the user_code + verification URI are reported here (tests /
    /// TUI) instead of being printed.
    pub on_user_code: Option<UserCodeHook<'a>>,
}

/// Device start → user prompt → poll → mint → persist into `persist_key`
/// (`muse` or `muse@<label>`) when given; `None` returns it unsaved.
pub async fn login_with_into<H, C, B>(
    http: &H,
    clock: &C,
    hooks: LoginHooks<'_, B>,
    cancel: &AtomicBool,
    persist_key: Option<&str>,
) -> Result<OAuthCredentials, MuseAuthError>
where
    H: MuseHttp,
    C: MuseClock,
    B: MuseBrowser,
{
    let authz = start_device_authorization(http, clock).await?;
    if let Some(cb) = hooks.on_user_code {
        cb(&authz.user_code, authz.browser_uri());
    } else {
        eprintln!("\n\x1b[1mMeta Muse device login\x1b[0m");
        eprintln!("  Enter code: \x1b[36m{}\x1b[0m", authz.user_code);
        eprintln!("  At:         \x1b[36m{}\x1b[0m\n", authz.verification_uri);
    }
    let _ = hooks.browser.open(authz.browser_uri());

    let oauth_access_token = wait_for_device_authorization(http, clock, &authz, cancel).await?;
    let (creds, minted) = mint_credentials(http, clock, &oauth_access_token).await?;
    if hooks.on_user_code.is_none() {
        if let Some(email) = &minted.user_email {
            eprintln!("  Signed in to Meta as \x1b[36m{email}\x1b[0m");
        }
        if minted.subs_active == Some(false) {
            eprintln!(
                "  Note: no active Muse plan on this account; usage may be limited \
                 (manage it at {ONBOARDING_URL})."
            );
        }
    }
    if let Some(key) = persist_key {
        save_provider_auth(key, &creds).map_err(|_| MuseAuthError::Persist)?;
    }
    Ok(creds)
}

/// Production login entry (real network + system browser + auth.json).
pub async fn login() -> Result<OAuthCredentials, String> {
    login_into(Some(PROVIDER)).await
}

/// Production login; `Some(key)` persists ONLY into that slot, `None`
/// returns the credential unsaved.
pub async fn login_into(persist_key: Option<&str>) -> Result<OAuthCredentials, String> {
    let http = ProductionHttp::new().map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    login_with_into(
        &http,
        &ProductionClock,
        LoginHooks {
            browser: &ProductionBrowser,
            on_user_code: None,
        },
        &cancel,
        persist_key,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Broker refresh: re-mint a Model API key from the stored OAuth access
/// token (no browser). The OAuth token is carried over unchanged, so the
/// compare-and-swap save in the token store matches.
pub async fn refresh_token(
    _client: &Client,
    oauth_access_token: &str,
) -> Result<OAuthCredentials, String> {
    let http = ProductionHttp::new().map_err(|e| e.to_string())?;
    mint_credentials(&http, &ProductionClock, oauth_access_token)
        .await
        .map(|(creds, _)| creds)
        .map_err(|e| e.to_string())
}

// ── Production adapters ──────────────────────────────────────────────────────

struct ProductionHttp {
    /// No-redirect client with connect + request timeouts for secret-bearing calls.
    client: Client,
}

impl ProductionHttp {
    fn new() -> Result<Self, MuseAuthError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| MuseAuthError::Transport)?;
        Ok(Self { client })
    }

    async fn finish(response: reqwest::Response) -> Result<MuseHttpResponse, MuseAuthError> {
        let status = response.status().as_u16();
        // Redirects are disabled; a 3xx means fail closed without following.
        if (300..400).contains(&status) {
            return Err(MuseAuthError::UntrustedEndpoint);
        }
        let body = read_body_capped(response, MAX_RESPONSE_BODY_BYTES).await?;
        Ok(MuseHttpResponse { status, body })
    }
}

#[async_trait]
impl MuseHttp for ProductionHttp {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<MuseHttpResponse, MuseAuthError> {
        validate_device_endpoint(url)?;
        let response = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .form(form)
            .send()
            .await
            .map_err(|_| MuseAuthError::Transport)?;
        Self::finish(response).await
    }

    async fn post_json_bearer(
        &self,
        url: &str,
        bearer: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<MuseHttpResponse, MuseAuthError> {
        // Pin again immediately before attaching the OAuth token.
        validate_mint_endpoint(url)?;
        let mut req = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {bearer}"));
        for (name, value) in headers {
            // Never let a caller-supplied Authorization override the bearer.
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(*name, *value);
        }
        let response = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| MuseAuthError::Transport)?;
        Self::finish(response).await
    }
}

struct ProductionClock;

#[async_trait]
impl MuseClock for ProductionClock {
    fn now_millis(&self) -> u64 {
        now_millis()
    }

    async fn sleep_cancellable(
        &self,
        duration: Duration,
        cancel: &AtomicBool,
    ) -> Result<(), MuseAuthError> {
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if cancel.load(Ordering::SeqCst) {
                return Err(MuseAuthError::Cancelled);
            }
            let chunk = remaining.min(CANCEL_POLL_QUANTUM);
            tokio::time::sleep(chunk).await;
            remaining = remaining.saturating_sub(chunk);
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(MuseAuthError::Cancelled);
        }
        Ok(())
    }
}

struct ProductionBrowser;

impl MuseBrowser for ProductionBrowser {
    fn open(&self, url: &str) -> Result<(), MuseAuthError> {
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

    #[derive(Default)]
    struct FakeClock {
        now: Mutex<u64>,
        sleeps: Mutex<Vec<u64>>,
    }

    impl FakeClock {
        fn at(now: u64) -> Self {
            Self {
                now: Mutex::new(now),
                sleeps: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MuseClock for FakeClock {
        fn now_millis(&self) -> u64 {
            *self.now.lock().unwrap()
        }
        async fn sleep_cancellable(
            &self,
            duration: Duration,
            cancel: &AtomicBool,
        ) -> Result<(), MuseAuthError> {
            if cancel.load(Ordering::SeqCst) {
                return Err(MuseAuthError::Cancelled);
            }
            self.sleeps.lock().unwrap().push(duration.as_secs());
            *self.now.lock().unwrap() += duration.as_millis() as u64;
            Ok(())
        }
    }

    type Recorded = (String, Option<String>, Vec<(String, String)>, String);

    #[derive(Default)]
    struct FakeHttp {
        queue: Mutex<Vec<(u16, String)>>,
        /// (url, bearer, form-or-headers, body)
        calls: Mutex<Vec<Recorded>>,
    }

    impl FakeHttp {
        fn script(responses: &[(u16, &str)]) -> Self {
            let http = Self::default();
            *http.queue.lock().unwrap() = responses
                .iter()
                .rev()
                .map(|(s, b)| (*s, b.to_string()))
                .collect();
            http
        }
        fn next(&self) -> Result<MuseHttpResponse, MuseAuthError> {
            let (status, body) = self
                .queue
                .lock()
                .unwrap()
                .pop()
                .expect("unscripted request");
            Ok(MuseHttpResponse { status, body })
        }
    }

    #[async_trait]
    impl MuseHttp for FakeHttp {
        async fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<MuseHttpResponse, MuseAuthError> {
            validate_device_endpoint(url)?;
            self.calls.lock().unwrap().push((
                url.into(),
                None,
                form.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                String::new(),
            ));
            self.next()
        }
        async fn post_json_bearer(
            &self,
            url: &str,
            bearer: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Result<MuseHttpResponse, MuseAuthError> {
            validate_mint_endpoint(url)?;
            self.calls.lock().unwrap().push((
                url.into(),
                Some(bearer.into()),
                headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body.into(),
            ));
            self.next()
        }
    }

    #[derive(Default)]
    struct FakeBrowser(Mutex<Vec<String>>);
    impl MuseBrowser for FakeBrowser {
        fn open(&self, url: &str) -> Result<(), MuseAuthError> {
            validate_verification_uri(url)?;
            self.0.lock().unwrap().push(url.into());
            Ok(())
        }
    }

    const DEVICE_OK: &str = r#"{"device_code":"DEV-SECRET","user_code":"ABCD-EFGH",
        "verification_uri":"https://auth.meta.com/device",
        "verification_uri_complete":"https://auth.meta.com/device?user_code=ABCD-EFGH",
        "expires_in":600,"interval":2}"#;
    const PENDING: &str = r#"{"error":"authorization_pending"}"#;
    const SLOW: &str = r#"{"error":"slow_down"}"#;
    const TOKEN_OK: &str = r#"{"access_token":"OAUTH-AT","token_type":"Bearer",
        "expires_in":3600,"refresh_token":"OAUTH-RT","scope":"openid"}"#;
    const MINT_OK: &str = r#"{"api_key":"MODEL-KEY-1","base_url":"https://api.meta.ai/v1",
        "user_full_name":"Test User","user_email":"t@example.com","is_subs_active":true}"#;

    #[test]
    fn constants_are_pinned_https_meta_hosts() {
        for url in [DEVICE_AUTH_URL, TOKEN_URL] {
            assert!(validate_device_endpoint(url).is_ok(), "{url}");
            assert!(
                url.ends_with('/'),
                "official CLI sends trailing slash: {url}"
            );
        }
        assert!(validate_mint_endpoint(MINT_URL).is_ok());
        assert_eq!(CLIENT_ID, "1031625952748946");
        assert_eq!(API_BASE_URL, format!("{API_ORIGIN}/v1"));
        assert_eq!(PROVIDER, "muse");
    }

    #[test]
    fn rejects_non_pinned_endpoints() {
        for url in [
            "http://auth.meta.com/oidc/device/token/",
            "https://auth.meta.com.evil.test/oidc/device/token/",
            "https://auth.meta.com/oidc/device/token",
            "https://evil.test/oidc/device/token/",
        ] {
            assert_eq!(
                validate_device_endpoint(url),
                Err(MuseAuthError::UntrustedEndpoint),
                "{url}"
            );
        }
        for url in [
            "http://api.meta.ai/muse-code/key",
            "https://api.meta.ai/muse-code/key?x=1",
            "https://api.meta.ai.evil.test/muse-code/key",
            "https://api.meta.ai/muse-code/logout",
        ] {
            assert_eq!(
                validate_mint_endpoint(url),
                Err(MuseAuthError::UntrustedEndpoint),
                "{url}"
            );
        }
    }

    #[test]
    fn verification_uri_must_be_plain_https_on_meta_hosts() {
        for ok in [
            "https://auth.meta.com/device",
            "https://www.meta.com/device?user_code=X",
            "https://meta.ai/activate",
        ] {
            assert!(validate_verification_uri(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://auth.meta.com/device",
            "https://user@auth.meta.com/device",
            "https://auth.meta.com:8443/device",
            "https://auth.meta.com/device#frag",
            "https://notmeta.com/device",
            "https://meta.com.evil.test/device",
            "javascript:alert(1)",
        ] {
            assert!(validate_verification_uri(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn device_code_parse_defaults_and_drops_foreign_complete_uri() {
        let authz = parse_device_code_response(DEVICE_OK, 1_000).unwrap();
        assert_eq!(authz.user_code, "ABCD-EFGH");
        assert_eq!(authz.interval_secs, 2);
        assert_eq!(authz.expires_in_secs, 600);
        assert_eq!(
            authz.browser_uri(),
            "https://auth.meta.com/device?user_code=ABCD-EFGH"
        );

        let foreign = r#"{"device_code":"d","user_code":"u",
            "verification_uri":"https://auth.meta.com/device",
            "verification_uri_complete":"https://evil.test/device"}"#;
        let authz = parse_device_code_response(foreign, 0).unwrap();
        assert_eq!(authz.browser_uri(), "https://auth.meta.com/device");
        assert_eq!(authz.expires_in_secs, DEFAULT_DEVICE_EXPIRES_IN_SECS);
        assert_eq!(authz.interval_secs, DEFAULT_POLL_INTERVAL_SECS);

        let bad_page = r#"{"device_code":"d","user_code":"u",
            "verification_uri":"https://evil.test/device"}"#;
        assert_eq!(
            parse_device_code_response(bad_page, 0).unwrap_err(),
            MuseAuthError::UntrustedEndpoint
        );
        let empty = r#"{"device_code":" ","user_code":"u",
            "verification_uri":"https://auth.meta.com/device"}"#;
        assert_eq!(
            parse_device_code_response(empty, 0).unwrap_err(),
            MuseAuthError::InvalidDeviceResponse
        );
    }

    #[test]
    fn poll_outcomes_follow_rfc8628() {
        assert_eq!(
            parse_device_poll_response(PENDING).unwrap(),
            DevicePollOutcome::Pending
        );
        assert_eq!(
            parse_device_poll_response(SLOW).unwrap(),
            DevicePollOutcome::SlowDown
        );
        assert_eq!(
            parse_device_poll_response(r#"{"error":"access_denied"}"#).unwrap(),
            DevicePollOutcome::Denied
        );
        assert_eq!(
            parse_device_poll_response(r#"{"error":"expired_token"}"#).unwrap(),
            DevicePollOutcome::Expired
        );
        assert_eq!(
            parse_device_poll_response(TOKEN_OK).unwrap(),
            DevicePollOutcome::Authorized {
                access_token: "OAUTH-AT".into()
            }
        );
        assert_eq!(
            parse_device_poll_response(r#"{"access_token":"  "}"#).unwrap_err(),
            MuseAuthError::EmptyAccessToken
        );
    }

    #[test]
    fn slow_down_is_bounded() {
        let mut interval = 5;
        for _ in 0..20 {
            interval = apply_slow_down(interval);
        }
        assert_eq!(interval, MAX_POLL_INTERVAL_SECS);
    }

    #[test]
    fn mint_classification_is_static_and_secret_free() {
        let minted = parse_mint_response(200, MINT_OK).unwrap();
        assert_eq!(minted.api_key, "MODEL-KEY-1");
        assert_eq!(minted.user_email.as_deref(), Some("t@example.com"));
        assert!(!format!("{minted:?}").contains("MODEL-KEY-1"));

        let onboarding = r#"{"error":{"code":"MODEL_API__ONBOARDING_REQUIRED",
            "message":"<html>echo OAUTH-AT</html>"}}"#;
        for status in [200, 400, 403] {
            let err = parse_mint_response(status, onboarding).unwrap_err();
            assert_eq!(err, MuseAuthError::OnboardingRequired);
            assert!(err.to_string().contains("dev.meta.ai"));
            assert!(!err.to_string().contains("OAUTH-AT"));
        }
        assert_eq!(
            parse_mint_response(401, r#"{"error":"bad token OAUTH-AT"}"#).unwrap_err(),
            MuseAuthError::LoginRejected(401)
        );
        assert_eq!(
            parse_mint_response(500, "boom").unwrap_err(),
            MuseAuthError::HttpStatus(500)
        );
        assert_eq!(
            parse_mint_response(200, r#"{"api_key":"","require_payment":true}"#).unwrap_err(),
            MuseAuthError::PaymentRequired
        );
        assert_eq!(
            parse_mint_response(200, r#"{"api_key":""}"#).unwrap_err(),
            MuseAuthError::EmptyApiKey
        );
        assert_eq!(
            parse_mint_response(200, "not json").unwrap_err(),
            MuseAuthError::InvalidMintResponse
        );
    }

    #[test]
    fn credentials_map_oauth_to_refresh_and_key_to_access() {
        let minted = parse_mint_response(200, MINT_OK).unwrap();
        let creds = credentials_from_mint("OAUTH-AT", &minted, 1_000).unwrap();
        assert_eq!(creds.refresh, "OAUTH-AT");
        assert_eq!(creds.access, "MODEL-KEY-1");
        assert_eq!(creds.auth_type, "oauth");
        assert_eq!(creds.expires, 1_000 + API_KEY_NOMINAL_LIFETIME_MS);
        assert!(creds.account_id.is_none());
        assert!(credentials_from_mint(" ", &minted, 0).is_err());
    }

    #[tokio::test]
    async fn full_login_polls_then_mints_without_persisting() {
        let http = FakeHttp::script(&[
            (200, DEVICE_OK),
            (400, PENDING),
            (400, SLOW),
            (200, TOKEN_OK),
            (200, MINT_OK),
        ]);
        let clock = FakeClock::at(1_000);
        let browser = FakeBrowser::default();
        let seen = Mutex::new(Vec::new());
        let hook = |code: &str, uri: &str| seen.lock().unwrap().push((code.into(), uri.into()));
        let creds = login_with_into(
            &http,
            &clock,
            LoginHooks {
                browser: &browser,
                on_user_code: Some(&hook),
            },
            &AtomicBool::new(false),
            None,
        )
        .await
        .unwrap();
        assert_eq!(creds.access, "MODEL-KEY-1");
        assert_eq!(creds.refresh, "OAUTH-AT");
        // interval 2s, then +5s after slow_down
        assert_eq!(*clock.sleeps.lock().unwrap(), vec![2, 2, 7]);
        assert_eq!(
            seen.lock().unwrap()[0],
            (
                "ABCD-EFGH".to_string(),
                "https://auth.meta.com/device?user_code=ABCD-EFGH".to_string()
            )
        );
        assert_eq!(browser.0.lock().unwrap().len(), 1);

        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        // Device start: client_id only (no scope, no PKCE).
        assert_eq!(calls[0].0, DEVICE_AUTH_URL);
        assert_eq!(
            calls[0].2,
            vec![("client_id".to_string(), CLIENT_ID.to_string())]
        );
        // Poll: RFC 8628 grant with the device code.
        assert_eq!(calls[1].0, TOKEN_URL);
        assert!(calls[1]
            .2
            .contains(&("grant_type".to_string(), DEVICE_GRANT_TYPE.to_string())));
        assert!(calls[1]
            .2
            .contains(&("device_code".to_string(), "DEV-SECRET".to_string())));
        // Mint: OAuth token as bearer, exact body + version header.
        let mint = &calls[4];
        assert_eq!(mint.0, MINT_URL);
        assert_eq!(mint.1.as_deref(), Some("OAUTH-AT"));
        assert_eq!(mint.3, MINT_BODY);
        assert!(mint
            .2
            .contains(&("x-api-version".to_string(), "1.0.0".to_string())));
        assert!(!mint
            .2
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
    }

    #[tokio::test]
    async fn denial_expiry_and_cancel_stop_polling() {
        let http = FakeHttp::script(&[(400, r#"{"error":"access_denied"}"#)]);
        let authz = parse_device_code_response(DEVICE_OK, 0).unwrap();
        let err = wait_for_device_authorization(
            &http,
            &FakeClock::at(0),
            &authz,
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
        assert_eq!(err, MuseAuthError::AccessDenied);

        // Deadline passes before the first poll.
        let http = FakeHttp::script(&[]);
        let err = wait_for_device_authorization(
            &http,
            &FakeClock::at(600_001),
            &authz,
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
        assert_eq!(err, MuseAuthError::Expired);

        let err =
            wait_for_device_authorization(&http, &FakeClock::at(0), &authz, &AtomicBool::new(true))
                .await
                .unwrap_err();
        assert_eq!(err, MuseAuthError::Cancelled);
    }

    #[tokio::test]
    async fn login_fails_closed_when_mint_requires_onboarding() {
        let http = FakeHttp::script(&[
            (200, DEVICE_OK),
            (200, TOKEN_OK),
            (403, r#"{"code":"MODEL_API__ONBOARDING_REQUIRED"}"#),
        ]);
        let err = login_with_into(
            &http,
            &FakeClock::at(0),
            LoginHooks {
                browser: &FakeBrowser::default(),
                on_user_code: Some(&|_, _| {}),
            },
            &AtomicBool::new(false),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err, MuseAuthError::OnboardingRequired);
    }

    #[tokio::test]
    async fn remint_uses_stored_oauth_token_and_keeps_it_as_refresh() {
        let http = FakeHttp::script(&[(200, r#"{"api_key":"MODEL-KEY-2"}"#)]);
        let (creds, _) = mint_credentials(&http, &FakeClock::at(5), "OAUTH-AT")
            .await
            .unwrap();
        assert_eq!(creds.access, "MODEL-KEY-2");
        // Unchanged refresh slot → the token store's CAS save matches.
        assert_eq!(creds.refresh, "OAUTH-AT");
        let http = FakeHttp::script(&[(401, "{}")]);
        assert_eq!(
            mint_credentials(&http, &FakeClock::at(5), "OAUTH-AT")
                .await
                .unwrap_err(),
            MuseAuthError::LoginRejected(401)
        );
    }

    #[test]
    fn errors_debug_display_are_secret_free() {
        for err in [
            MuseAuthError::LoginRejected(401),
            MuseAuthError::HttpStatus(500),
            MuseAuthError::OnboardingRequired,
            MuseAuthError::PaymentRequired,
            MuseAuthError::EmptyApiKey,
        ] {
            let shown = format!("{err} {err:?}");
            assert!(!shown.contains("OAUTH-AT") && !shown.contains("MODEL-KEY"));
        }
    }

    #[test]
    fn production_http_builds_with_no_redirects() {
        assert!(ProductionHttp::new().is_ok());
        assert_eq!(MAX_RESPONSE_BODY_BYTES, 64 * 1024);
        assert!(USER_AGENT.starts_with("SynapsCLI/"));
    }

    #[test]
    fn device_poll_outcome_debug_redacts_token() {
        let o = DevicePollOutcome::Authorized {
            access_token: "oauth-secret-123".into(),
        };
        let dbg = format!("{o:?}");
        assert!(!dbg.contains("oauth-secret-123"), "{dbg}");
        assert!(
            dbg.contains("Authorized") && dbg.contains("REDACTED"),
            "{dbg}"
        );
        assert_eq!(format!("{:?}", DevicePollOutcome::Pending), "Pending");
    }

    #[test]
    fn device_flow_debug_redacts_secrets() {
        let r = MuseHttpResponse {
            status: 200,
            body: "{\"access_token\":\"body-secret\"}".into(),
        };
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("body-secret") && dbg.contains("200"), "{dbg}");

        let a = DeviceAuthorization {
            device_code: "device-secret".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://example.invalid".into(),
            verification_uri_complete: None,
            expires_in_secs: 600,
            interval_secs: 5,
            issued_at_ms: 0,
        };
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("device-secret") && dbg.contains("ABCD-EFGH"),
            "{dbg}"
        );

        let c = DeviceCodeJson {
            device_code: "code-json-secret".into(),
            user_code: "WXYZ-1234".into(),
            verification_uri: "https://example.invalid".into(),
            verification_uri_complete: None,
            expires_in: Some(600),
            interval: Some(5),
        };
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("code-json-secret") && dbg.contains("WXYZ-1234"),
            "{dbg}"
        );

        let t = DeviceTokenJson {
            access_token: Some("token-secret".into()),
            error: Some("slow_down".into()),
        };
        let dbg = format!("{t:?}");
        assert!(
            !dbg.contains("token-secret") && dbg.contains("slow_down"),
            "{dbg}"
        );
    }
}
