//! The credential broker — the mandatory boundary between runtimes and
//! provider secrets (Checkpoint 1 decision, `docs/decisions/credential-broker-checkpoint-1.md`).
//!
//! ## Invariants (enforced by construction and tests)
//!
//! * Runtime/TUI code talks to a [`CredentialBroker`] and never reads
//!   `auth.json`, provider-key config, or credential environment variables.
//!   All of that discovery lives inside [`LocalBroker`] — behind the boundary.
//! * OAuth responses vend **access token + expiry only** ([`AccessToken`] has
//!   no refresh field to put one in). Refresh tokens stay broker-owned.
//! * Static API keys are never vended, locally or remotely. They are applied
//!   broker-side via the typed request proxy ([`ProxyRequest`]): the broker
//!   pins the destination URL from its own [`static_providers`] table and
//!   attaches the bearer key itself.
//! * Capability/status queries ([`ProviderStatus`]) expose configured-ness
//!   only — never key material.
//! * Fail closed: a missing credential is an error, never a fallback to a
//!   direct read at the call site.
//!
//! [`static_providers`]: super::static_providers

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use super::account::{
    seat_fingerprint, Account, AccountPolicy, AccountSelector, AccountSummary, CredentialRef,
    MaterialDigest, SeatIdentity,
};
use super::cloud::{CloudProviderId, InvokeRequest};
use super::provider::OAuthProviderId;
use super::static_providers::{
    allowed_proxy_paths, static_provider, StaticProviderSpec, LOCAL_DEFAULT_BASE_URL,
    LOCAL_PROVIDER_KEY, STATIC_PROVIDERS,
};
use super::storage;

// ── Buffering / time limits ──────────────────────────────────────────────────
//
// Streaming (SSE) bodies are never buffered — they flow chunk-by-chunk with
// backpressure. These caps bound everything the broker (or a broker client)
// must hold in memory, so a hostile or broken upstream cannot balloon the
// process or smuggle unbounded content through error strings.

/// Maximum serialized size of a proxy request body.
pub const MAX_PROXY_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Maximum buffered (non-streaming) response body size.
pub const MAX_PROXY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Historical cap for upstream error-body snippets. Upstream/broker error
/// bodies are no longer read at all (spec §5.1: they may echo the request);
/// the constant remains only because it is re-exported from `auth::mod`.
pub const MAX_UPSTREAM_ERROR_BYTES: usize = 2 * 1024;
/// Total time budget for a buffered (non-streaming) broker-executed request.
pub const PROXY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Fixed Anthropic usage endpoint — a typed broker operation, not a general
/// proxy target. Callers cannot vary the URL or path.
const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Pinned Code Assist host for the broker-only Google Gemini proxy runtime.
/// The service is treated as experimental and only the exact `v1internal`
/// method paths reviewed in the spec are permitted (setup + streaming).
pub const GOOGLE_GEMINI_CODE_ASSIST_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";

/// Exhaustive allowlist of `cloudcode-pa` methods the broker may proxy.
/// Deliberately narrow: setup uses loadCodeAssist + onboardUser + operations
/// polling, runtime uses streamGenerateContent. Anything else is denied.
pub(crate) fn is_allowed_google_gemini_path(path: &str) -> bool {
    matches!(
        path,
        "/v1internal:loadCodeAssist"
            | "/v1internal:onboardUser"
            | "/v1internal:streamGenerateContent"
            | "/v1internal:countTokens"
    ) || path.starts_with("/v1internal/operations/")
}

/// Pinned ChatGPT backend host for OpenAI Codex catalog traffic.
pub const OPENAI_CODEX_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Allow only the official Codex model-catalog path:
/// `GET /codex/models?client_version=<non-empty>`.
///
/// Query is constrained to a single non-empty `client_version` key — no extra
/// params, no empty version, no bare `/codex/models`. Same-host inference
/// paths (`/codex/responses`, …) stay out of this catalog-only allowlist, as
/// does the ChatGPT web picker at `/models` (different schema, not a Codex
/// catalog).
pub(crate) const OPENAI_CODEX_MODELS_PATH: &str = "/codex/models";

pub(crate) fn is_allowed_openai_codex_path(path: &str) -> bool {
    let Some((base, query)) = path.split_once('?') else {
        return false;
    };
    if base != OPENAI_CODEX_MODELS_PATH {
        return false;
    }
    let mut client_version: Option<&str> = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            return false;
        };
        if k != "client_version" || client_version.is_some() {
            return false;
        }
        if v.trim().is_empty() {
            return false;
        }
        client_version = Some(v);
    }
    client_version.is_some()
}

/// Allow only Anthropic's paginated models catalog path:
/// `/v1/models` or `/v1/models?limit=…` with optional `after_id=…`.
///
/// No other same-host endpoints (messages, keys, admin, …) are permitted.
pub(crate) fn is_allowed_anthropic_path(path: &str) -> bool {
    if path == "/v1/models" {
        return true;
    }
    let Some((base, query)) = path.split_once('?') else {
        return false;
    };
    if base != "/v1/models" {
        return false;
    }
    if query.is_empty() {
        return false;
    }
    let mut saw_limit = false;
    let mut saw_after_id = false;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            return false;
        };
        if v.is_empty() {
            return false;
        }
        match k {
            "limit" if !saw_limit => saw_limit = true,
            "after_id" if !saw_after_id => saw_after_id = true,
            _ => return false,
        }
    }
    // limit may be omitted only for bare /v1/models; when a query is present
    // at least one allowed key must appear (already enforced by the loop).
    true
}

/// Headers applied by the broker for Anthropic OAuth catalog proxy requests.
/// Bearer is set separately via `bearer_auth`; this list must NOT include
/// `x-api-key` (OAuth tokens are never sent as API keys).
pub(crate) fn anthropic_oauth_catalog_request_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", ANTHROPIC_OAUTH_BETA),
        ("accept", "application/json"),
    ]
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Broker failures. Messages never contain credential values.
/// Non-exhaustive: new failure kinds are added as brokers gain operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrokerError {
    /// The named provider is not in any broker registry.
    UnknownProvider(String),
    /// A required public-client registration is not configured.
    RegistrationRequired {
        provider: String,
        remediation: String,
    },
    /// The provider is known but has no credential configured.
    NotConfigured(String),
    /// The caller failed broker (machine) authentication.
    Unauthorized,
    /// The request was rejected by broker policy (e.g. malformed proxy path).
    Denied(String),
    /// The route honestly lacks a capability the request needs (e.g. tools on
    /// a text-only cloud route). Raised pre-flight, before credentials/network.
    UnsupportedCapability {
        provider: String,
        capability: String,
    },
    /// Transport-level failure talking to the broker or the provider, after
    /// the request may have been sent (or with that unknown).
    Transport(String),
    /// No connection to the broker or the provider could be established:
    /// no byte of the request was sent.
    ConnectFailed(String),
    /// Credential storage/refresh failure (message is already secret-free).
    Credential(String),
    /// An explicitly requested account slot does not exist. Never a fallback.
    UnknownAccount { provider: String, label: String },
    /// This broker implementation cannot address named account slots.
    UnsupportedAccount { provider: String, label: String },
    /// An account label failed validation (CLI/env/config/HTTP boundary).
    InvalidAccount(String),
    /// Automatic selection found no account with proven capacity. `failure`
    /// is the typed explanation (top-level kind plus every account's kind
    /// and HTTP status) when the broker knows it; `reason` its text.
    NoAccountAvailable {
        provider: String,
        reason: String,
        failure: Option<super::quota_policy::SelectionFailure>,
    },
    /// The remote broker predates an operation this client needs; the
    /// payload names what is unavailable. Upgrade the broker host.
    BrokerTooOld(String),
    /// A locked token refresh failed; the kind tells a per-account cooldown,
    /// a busy refresh lock or a transient network/provider failure apart
    /// from an unusable login. Message is secret-free.
    Refresh(super::token::RefreshFailure),
}

impl BrokerError {
    /// True when the broker failed or refused the request before any byte
    /// of it reached the upstream provider (unknown provider, policy, missing
    /// or failed credential, broker machine auth, no connection). An explicit
    /// allowlist: a new variant must be classified here, never free by
    /// default. [`BrokerError::ConnectFailed`] is only produced where no
    /// redirect can have been followed first.
    pub fn request_not_sent(&self) -> bool {
        match self {
            Self::UnknownProvider(_)
            | Self::RegistrationRequired { .. }
            | Self::NotConfigured(_)
            | Self::Unauthorized
            | Self::Denied(_)
            | Self::UnsupportedCapability { .. }
            | Self::ConnectFailed(_)
            | Self::Credential(_) => true,
            // Account resolution and selection (including the remote
            // broker's token vend reporting no capacity) fail before any
            // upstream request exists. A path that recovers after sending
            // (Muse's 401 re-mint) returns the upstream 401 when recovery
            // fails, and its later failures (bearer, retry), these and
            // `Credential` included, as `Transport`.
            Self::UnknownAccount { .. }
            | Self::UnsupportedAccount { .. }
            | Self::InvalidAccount(_)
            | Self::NoAccountAvailable { .. } => true,
            Self::Transport(_) => false,
            // Only `recover_rejected_token` produces it, after the upstream
            // answered 401: the request was sent (as 1e4fa8ac types a Muse
            // recovery failure after a sent request).
            Self::Refresh(_) => false,
            // Dev #253: a remote broker without an account-listing or usage
            // operation answers before any upstream request exists.
            Self::BrokerTooOld(_) => true,
        }
    }
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProvider(p) => write!(f, "unknown provider: {p}"),
            Self::RegistrationRequired {
                provider,
                remediation,
            } => {
                write!(f, "registration required for '{provider}': {remediation}")
            }
            Self::NotConfigured(p) => write!(
                f,
                "no credential configured for '{p}'. Run `synaps login` to add one."
            ),
            Self::Unauthorized => f.write_str(&super::broker_token::unauthorized_hint()),
            Self::Denied(msg) => write!(f, "broker denied request: {msg}"),
            Self::UnsupportedCapability {
                provider,
                capability,
            } => write!(
                f,
                "cloud provider '{provider}' is text-only: {capability} are not supported yet \
                 (no credential was used and no network request was made)"
            ),
            Self::Transport(msg) | Self::ConnectFailed(msg) => {
                write!(f, "broker transport error: {msg}")
            }
            Self::Credential(msg) => write!(f, "credential error: {msg}"),
            Self::Refresh(failure) => {
                use super::token::RefreshFailureKind as K;
                let hint = match failure.kind {
                    K::Transient => " (network or provider error, not a login problem; retry shortly)",
                    K::LockBusy => " (another refresh of this account is still running; retry shortly)",
                    K::Other => " (retry shortly)",
                    K::Credential | K::Cooldown { .. } | K::ActionRequired => "",
                };
                write!(f, "credential refresh failed{hint}: {failure}")
            }
            Self::UnknownAccount { provider, label } => write!(
                f,
                "unknown account '{label}' for provider '{provider}' (no fallback; run \
                 `synaps auth list` or `synaps login --provider {provider} --account {label}`)"
            ),
            Self::UnsupportedAccount { provider, label } => write!(
                f,
                "this credential broker cannot address account '{label}' for provider '{provider}'"
            ),
            Self::InvalidAccount(msg) => write!(f, "invalid account: {msg}"),
            Self::NoAccountAvailable {
                provider,
                reason,
                failure: Some(failure),
            } => write!(
                f,
                "no account with proven capacity for '{provider}' ({}): {reason}",
                failure.kind
            ),
            Self::NoAccountAvailable {
                provider, reason, ..
            } => {
                write!(
                    f,
                    "no account with proven capacity for '{provider}': {reason}"
                )
            }
            Self::BrokerTooOld(what) => write!(
                f,
                "broker is older than this client; {what} unavailable (upgrade the broker host)"
            ),
        }
    }
}

impl std::error::Error for BrokerError {}

#[cfg(test)]
#[path = "broker_remote_compat_tests.rs"]
mod remote_compat_tests;

// ── Vended types ─────────────────────────────────────────────────────────────

/// An OAuth access token vended by the broker.
///
/// Deliberately has **no** refresh-token field — the invariant made
/// structural. There is nowhere to put a refresh token even by mistake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessToken {
    pub token: String,
    /// Absolute expiry, unix-epoch milliseconds.
    pub expires: u64,
}

/// The selected/pinned pair a runtime works with for one attempt: the
/// credential the broker chose and the token vended for exactly that
/// credential. Account-specific headers (e.g. the Codex account id) must be
/// derived from `token`, never from a second lookup.
#[derive(Debug, Clone)]
pub struct PinnedToken {
    pub credential: CredentialRef,
    pub token: AccessToken,
}

impl PinnedToken {
    /// Seat fingerprint derived from THIS token's own identity claim (Codex:
    /// the JWT `chatgpt_account_id`), comparable with
    /// [`AccountSummary::seat_fingerprint`]. `None` when the token carries no
    /// account identity — callers that must spend on exactly one listed seat
    /// treat `None` or a mismatch as "do not send".
    pub fn seat_fingerprint(&self) -> Option<String> {
        match self.credential.provider {
            OAuthProviderId::OpenAiCodex => super::extract_codex_account_id(&self.token.token)
                .and_then(|id| seat_fingerprint(self.credential.provider, &id)),
            _ => None,
        }
    }
}

/// HTTP method subset the proxy protocol supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMethod {
    Get,
    Post,
}

/// A typed, credential-free request the broker executes on the caller's
/// behalf against an OpenAI-compatible provider endpoint.
///
/// The caller names a provider and a relative path; the broker derives the
/// destination from its pinned provider table and applies the (broker-owned)
/// credential. Callers can never supply an absolute URL for a static-key
/// provider, so a key can never be coaxed toward an attacker-chosen host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    /// Static provider key (e.g. `groq`), `local`, or an OAuth-proxied
    /// provider. For OAuth providers an explicit account slot may be pinned
    /// as `<provider>@<label>` or `<provider>@default`; a bare provider means
    /// "the broker's account policy". Validated by [`validate`](Self::validate);
    /// an explicit unknown slot is an error, never a fallback.
    pub provider: String,
    pub method: ProxyMethod,
    /// Relative path joined onto the pinned base URL, e.g. `/chat/completions`.
    pub path: String,
    /// JSON request body (POST only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Request an SSE byte stream (`proxy_stream`) instead of a buffered body.
    #[serde(default)]
    pub stream: bool,
    /// Typed exact-byte handoff (request-trace spec §6.2): when present this
    /// MUST be the exact serialization of `body`, produced once by the
    /// caller. [`LocalBroker`] sends these very bytes upstream verbatim, so
    /// a caller-computed digest over them is a digest of the true wire body.
    /// The field never crosses the remote broker HTTP boundary
    /// (`serde(skip)`): a remote broker daemon re-serializes `body` on its
    /// side, so callers on a remote path must NOT claim exact upstream
    /// bytes. Never logged, never part of the broker wire schema.
    #[serde(skip)]
    pub body_bytes: Option<bytes::Bytes>,
}

impl ProxyRequest {
    /// Validate provider identity and path shape. Fail closed on anything
    /// that could redirect a broker-owned credential.
    pub fn validate(&self) -> Result<(), BrokerError> {
        let (provider, _account) = self.split_provider()?;
        let provider = provider.as_str();
        if provider != LOCAL_PROVIDER_KEY
            && provider != "xai-auth"
            && provider != "github-copilot"
            && provider != "google-gemini"
            && provider != "openai-codex"
            && provider != "anthropic"
            && provider != "kimi-code"
            && provider != "muse"
            && static_provider(provider).is_none()
        {
            return Err(BrokerError::UnknownProvider(self.provider.clone()));
        }
        if !self.path.starts_with('/') {
            return Err(BrokerError::Denied("proxy path must be relative".into()));
        }
        if self.path.contains("://") || self.path.contains("..") {
            return Err(BrokerError::Denied(
                "proxy path is not a plain relative path".into(),
            ));
        }
        // Per-provider endpoint allowlist: a signed proxy request can only
        // reach the cataloged inference/model paths, never other same-host
        // endpoints (key management, billing, admin, …).
        let oauth_path_allowed = provider == "xai-auth" && self.path == "/responses"
            || provider == "github-copilot"
                && matches!(
                    self.path.as_str(),
                    "/models" | "/chat/completions" | "/responses"
                )
            || provider == "google-gemini" && is_allowed_google_gemini_path(&self.path)
            // Managed Kimi Code endpoint allowlist: chat inference plus the
            // read-only catalog/profile/quota surfaces the official CLI uses.
            || provider == "kimi-code"
                && (self.path == "/chat/completions"
                    || self.method == ProxyMethod::Get
                        && matches!(self.path.as_str(), "/models" | "/me" | "/usages"))
            // Meta Muse: Responses inference plus the read-only model catalog.
            // Never the key mint/logout endpoints on the same host.
            || provider == "muse"
                && (self.method == ProxyMethod::Post && self.path == "/v1/responses"
                    || self.method == ProxyMethod::Get && self.path == "/muse-code/models")
            || provider == "openai-codex"
                && self.method == ProxyMethod::Get
                && is_allowed_openai_codex_path(&self.path)
            || provider == "anthropic"
                && self.method == ProxyMethod::Get
                && is_allowed_anthropic_path(&self.path);
        if !oauth_path_allowed && !allowed_proxy_paths(provider).contains(&self.path.as_str()) {
            return Err(BrokerError::Denied(format!(
                "proxy path '{}' is not in the provider's endpoint allowlist",
                self.path
            )));
        }
        // Size cap over the effective wire body. The exact-byte handoff is
        // authoritative whenever present — even if `body` is unset, those
        // bytes are what `LocalBroker` would send upstream, so they must
        // never bypass the cap (fail closed).
        let size = match (&self.body_bytes, &self.body) {
            // Exact-byte handoff: the bytes ARE the wire body.
            (Some(bytes), _) => Some(bytes.len()),
            (None, Some(body)) => Some(
                serde_json::to_vec(body)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX),
            ),
            (None, None) => None,
        };
        if let Some(size) = size {
            if size > MAX_PROXY_REQUEST_BYTES {
                return Err(BrokerError::Denied(format!(
                    "request body exceeds the {MAX_PROXY_REQUEST_BYTES}-byte broker limit"
                )));
            }
        }
        // Coherence invariant (debug/test only — never a release hot-path
        // re-serialization): when both representations are present,
        // `body_bytes` MUST parse back to exactly `body`. A divergence
        // means a caller bypassed [`ProxyRequest::post_json_exact`] and the
        // digested bytes would not describe the JSON the broker validated.
        #[cfg(any(test, debug_assertions))]
        if let (Some(bytes), Some(body)) = (&self.body_bytes, &self.body) {
            let parsed = serde_json::from_slice::<serde_json::Value>(bytes).ok();
            if parsed.as_ref() != Some(body) {
                return Err(BrokerError::Denied(
                    "body_bytes does not match the JSON body (exact-byte handoff incoherent)"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Build a POST proxy request whose JSON body is serialized **exactly
    /// once**: the returned [`bytes::Bytes`] handle shares the same buffer
    /// stored in `body_bytes` (ref-counted clone, no copy), so a
    /// caller-side digest over it is a digest of the true wire body on the
    /// local-broker path. This is the sanctioned way to populate
    /// `body_bytes` — going through it makes a `body`/`body_bytes`
    /// semantic divergence unconstructible, and it never re-serializes on
    /// the release hot path.
    pub fn post_json_exact(
        provider: impl Into<String>,
        path: impl Into<String>,
        body: serde_json::Value,
        stream: bool,
    ) -> Result<(Self, bytes::Bytes), BrokerError> {
        let bytes =
            bytes::Bytes::from(serde_json::to_vec(&body).map_err(|e| {
                BrokerError::Denied(format!("request body failed to serialize: {e}"))
            })?);
        let request = Self {
            provider: provider.into(),
            method: ProxyMethod::Post,
            path: path.into(),
            body: Some(body),
            stream,
            body_bytes: Some(bytes.clone()),
        };
        Ok((request, bytes))
    }

    /// Pin the request to an explicit account slot (`<provider>@<label>` /
    /// `<provider>@default`), replacing any previous pin.
    pub fn with_account(mut self, account: &Account) -> Self {
        let base = self
            .provider
            .split_once('@')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| self.provider.clone());
        self.provider = format!("{base}@{}", account.label_str());
        self
    }

    /// Split `provider` into the base provider key and the explicit account
    /// pin, validating the label. Static/local providers never carry a pin.
    pub fn split_provider(&self) -> Result<(String, Option<Account>), BrokerError> {
        match self.provider.split_once('@') {
            None => Ok((self.provider.clone(), None)),
            Some((base, account)) => {
                if base.parse::<OAuthProviderId>().is_err() {
                    return Err(BrokerError::UnknownProvider(self.provider.clone()));
                }
                let account = Account::parse(account).map_err(BrokerError::InvalidAccount)?;
                Ok((base.to_string(), Some(account)))
            }
        }
    }

    /// Base provider key without any account pin (after validation).
    pub fn base_provider(&self) -> Result<String, BrokerError> {
        Ok(self.split_provider()?.0)
    }

    /// The explicit account pin, if any. `None` → the policy decides.
    pub fn explicit_account(&self) -> Result<Option<Account>, BrokerError> {
        Ok(self.split_provider()?.1)
    }
}

/// Buffered (non-streaming) proxy result: upstream status + body text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyResponse {
    pub status: u16,
    pub body: String,
}

/// Streaming proxy result: raw upstream bytes (SSE), post-authorization.
pub type ProxyByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, BrokerError>> + Send>>;

/// What kind of credential a provider uses. Never carries key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    OAuth,
    StaticKey,
    LocalEndpoint,
}

/// Non-secret capability/status row for one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub key: String,
    pub name: String,
    pub kind: CredentialKind,
    /// Whether a credential is available through the broker (for OAuth rows:
    /// the policy-selected account exists, or any account under `auto`).
    pub configured: bool,
    /// Stored account slots (OAuth providers only; non-secret rows).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountSummary>,
}

/// Non-secret display status for one static key (settings UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticKeyStatus {
    NotSet,
    /// Configured via broker storage or login config; carries a masked,
    /// non-reversible preview like `gsk-…7f2a`.
    Configured {
        masked: String,
    },
    /// Available from the process environment (behind the broker boundary).
    FromEnv,
}

impl StaticKeyStatus {
    pub fn is_configured(&self) -> bool {
        !matches!(self, StaticKeyStatus::NotSet)
    }
}

// ── The broker contract ──────────────────────────────────────────────────────

/// Normalized dynamic cloud model returned across local/remote broker boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudCatalogEntry {
    pub provider: CloudProviderId,
    pub id: String,
    pub display_name: String,
    /// Opaque broker route identifier; never a provider, account, project, or resource name.
    pub context_ref: String,
    pub context_label: String,
    pub stale: bool,
    /// Unix epoch milliseconds when this catalog snapshot was fetched.
    pub fetched_at: u64,
}

/// Credential-free normalized cloud invocation event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CloudEvent {
    TextDelta {
        delta: String,
    },
    ToolArguments {
        id: String,
        name: Option<String>,
        delta: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Done,
}
pub type CloudEventStream = Pin<Box<dyn Stream<Item = Result<CloudEvent, BrokerError>> + Send>>;

/// A failed [`CredentialBroker::usage_or_last_known`] read.
#[derive(Debug)]
pub struct UsageReadFailure {
    /// The error [`CredentialBroker::usage`] gives for the same read.
    pub error: BrokerError,
    /// Set when the usage endpoint rate-limited the read (HTTP 429): this
    /// read's own 429, a held one, or the pause another account's 429
    /// started. Times and a reading only, no token material.
    pub rate_limited: Option<RateLimitedRead>,
}

impl UsageReadFailure {
    /// A failure that carries nothing but its error.
    pub fn plain(error: BrokerError) -> Self {
        Self {
            error,
            rate_limited: None,
        }
    }
}

/// What a rate-limited usage read ([`UsageReadFailure::rate_limited`])
/// can still show.
#[derive(Debug, Clone)]
pub struct RateLimitedRead {
    /// When this process sends the account's next usage request (epoch
    /// ms): the end of the read's hold, or of the pause.
    pub retry_at_ms: u64,
    /// The account's last good reading, only if it was read from the
    /// account stored in the slot now. It keeps the time it was read
    /// (`observed_at`).
    pub last_good: Option<super::usage::UsageSnapshot>,
}

/// The typed credential broker protocol. One implementation runs in-process
/// ([`LocalBroker`]) so normal local use needs no daemon; the other talks to a
/// remote `synaps auth-broker` over authenticated HTTP(S) ([`RemoteBroker`]).
#[async_trait]
pub trait CredentialBroker: Send + Sync {
    /// Vend a fresh OAuth access token (token + expiry ONLY) for the
    /// policy-selected account of `provider`.
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError>;

    /// Effective account selector for `provider` (env > config > injected
    /// policy). No I/O beyond reading configuration; no secrets.
    fn account_selector(&self, provider: OAuthProviderId) -> AccountSelector {
        let _ = provider;
        AccountSelector::Account(Account::Default)
    }

    /// Vend a token for an explicitly addressed credential. The default
    /// implementation serves only the default slot (via [`access_token`]);
    /// a named slot is an error — NEVER a fallback to another account.
    ///
    /// [`access_token`]: Self::access_token
    async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        match &cred.account {
            Account::Default => self.access_token(cred.provider).await,
            Account::Named(label) => Err(BrokerError::UnsupportedAccount {
                provider: cred.provider.as_str().to_string(),
                label: label.as_str().to_string(),
            }),
        }
    }

    /// Re-vend the exact credential whose access token was rejected. Local
    /// brokers force refresh only if storage still holds that rejected token.
    /// Remote/custom brokers keep their ordinary explicit-slot semantics.
    async fn recover_rejected_token(
        &self,
        cred: &CredentialRef,
        _rejected: &str,
    ) -> Result<AccessToken, BrokerError> {
        self.access_token_for(cred).await
    }

    /// Policy-resolved pair: the credential the broker selected for
    /// `provider` and its token. Default implementation: the default slot.
    async fn access_token_pinned(
        &self,
        provider: OAuthProviderId,
    ) -> Result<PinnedToken, BrokerError> {
        let token = self.access_token(provider).await?;
        Ok(PinnedToken {
            credential: CredentialRef::default_for(provider),
            token,
        })
    }

    /// Model-aware variant of [`access_token_pinned`]: under an `auto`
    /// selector only accounts with proven capacity *for `model`* are
    /// eligible. Default implementation ignores the model.
    ///
    /// [`access_token_pinned`]: Self::access_token_pinned
    async fn access_token_pinned_for(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        let _ = model;
        self.access_token_pinned(provider).await
    }

    /// [`access_token_pinned_for`] that never answers with an account in
    /// `exclude` (the accounts one logical request already used). Default
    /// implementation: the ordinary selection, with an excluded answer
    /// refused as "no account available"; a broker that can pass the
    /// exclusion into its own selection overrides this.
    ///
    /// [`access_token_pinned_for`]: Self::access_token_pinned_for
    async fn access_token_pinned_excluding(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        exclude: &[CredentialRef],
    ) -> Result<PinnedToken, BrokerError> {
        let pinned = self.access_token_pinned_for(provider, model).await?;
        if exclude.contains(&pinned.credential) {
            return Err(BrokerError::NoAccountAvailable {
                provider: provider.as_str().to_string(),
                reason: format!(
                    "the broker chose {}, which this request already used",
                    pinned.credential.account
                ),
                failure: None,
            });
        }
        Ok(pinned)
    }

    /// Non-secret account listing for `provider`. Default: unsupported.
    async fn accounts(&self, provider: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError> {
        Err(BrokerError::UnsupportedCapability {
            provider: provider.as_str().to_string(),
            capability: "accounts".into(),
        })
    }

    /// Typed, read-only usage snapshot for one credential. The token is
    /// resolved and used behind the boundary; callers receive normalized,
    /// secret-free data only. Default: unsupported.
    async fn usage(&self, cred: &CredentialRef) -> Result<super::usage::UsageSnapshot, BrokerError> {
        Err(BrokerError::UnsupportedCapability {
            provider: cred.provider.as_str().to_string(),
            capability: "usage".into(),
        })
    }

    /// [`usage`](Self::usage) as `synaps status` shows it: a broker may
    /// answer with a recent reading it holds instead of a request, and a
    /// read the usage endpoint rate-limited may come with when the next
    /// request goes out and the account's last good reading
    /// ([`UsageReadFailure`]). Default: [`usage`](Self::usage), with a
    /// failure that carries nothing more.
    async fn usage_or_last_known(
        &self,
        cred: &CredentialRef,
    ) -> Result<super::usage::UsageSnapshot, Box<UsageReadFailure>> {
        self.usage(cred)
            .await
            .map_err(|error| Box::new(UsageReadFailure::plain(error)))
    }

    /// Report that `cred` hit a provider limit so automatic selection stops
    /// advertising it until `until_ms` (epoch ms; `None` = a short default).
    /// Best effort; default implementation is a no-op.
    async fn report_cooldown(
        &self,
        cred: &CredentialRef,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let _ = (cred, until_ms, reason);
        Ok(())
    }

    /// When `cred`'s active cooldown ends (epoch ms), if this broker knows
    /// of one. No network request and no token refresh. Default: none.
    fn active_cooldown(&self, cred: &CredentialRef) -> Option<u64> {
        let _ = cred;
        None
    }

    /// A request sent on `cred` at `sent_at_ms` (epoch ms) was accepted:
    /// drop its cooldown if that was reported no later than the request
    /// left (the same millisecond counts: the caller saw the limit first), so
    /// a limit that has passed does not keep moving turns off the account.
    /// A cooldown reported later than `sent_at_ms` stays (that request proves
    /// nothing about it). Memory only. Default: no-op.
    fn clear_cooldown(&self, cred: &CredentialRef, sent_at_ms: u64) {
        let _ = (cred, sent_at_ms);
    }

    /// Re-vend `cred` for automatic selection (a turn keeping the account
    /// it is on). Unlike [`access_token_for`], which serves an account the
    /// user chose, this honours and records a refused refresh's hold, the
    /// same as the automatic choice's own vend. Default:
    /// [`access_token_for`].
    ///
    /// [`access_token_for`]: Self::access_token_for
    async fn access_token_for_auto(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        self.access_token_for(cred).await
    }

    /// Report that `cred` hit a provider limit that applies only to requests
    /// for `model` (a token of the model id the provider names, such as
    /// `fable`), until `until_ms`, so automatic selection stops choosing it
    /// for those requests; other models keep it. Best effort. Default: the
    /// whole account cools down ([`report_cooldown`](Self::report_cooldown)),
    /// which never chooses an account more often than before.
    async fn report_model_cooldown(
        &self,
        cred: &CredentialRef,
        model: &str,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let _ = model;
        self.report_cooldown(cred, until_ms, reason).await
    }

    /// When `cred`'s active cooldown for a request for `model` ends (epoch
    /// ms): the whole account's, or one reported for a model the request
    /// names; with no model, any of them. No network request and no token
    /// refresh. Default: [`active_cooldown`](Self::active_cooldown).
    fn active_cooldown_for(&self, cred: &CredentialRef, model: Option<&str>) -> Option<u64> {
        let _ = model;
        self.active_cooldown(cred)
    }

    /// Execute a non-streaming provider request with the broker-owned key.
    async fn proxy(&self, request: ProxyRequest) -> Result<ProxyResponse, BrokerError>;

    /// Execute a streaming (SSE) provider request with the broker-owned key.
    /// Returns the byte stream only when the upstream response is successful;
    /// error statuses are read (bounded) and surfaced as [`BrokerError`].
    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError>;

    /// Typed operation: fetch the Anthropic account-usage summary.
    ///
    /// The destination URL is pinned broker-side and the OAuth access token
    /// is resolved and attached behind the boundary — callers receive usage
    /// JSON only and never see a token or `auth.json`. This is deliberately
    /// NOT a generic OAuth proxy: one operation, one fixed endpoint.
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError>;

    /// Typed cloud catalog. Credentials and provider authority remain broker-owned.
    async fn cloud_catalog(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        allow_stale: bool,
    ) -> Result<Vec<CloudCatalogEntry>, BrokerError> {
        let _ = (context_ref, allow_stale);
        Err(BrokerError::NotConfigured(provider.to_string()))
    }

    /// Typed cloud invocation. The canonical model and normalized request are the
    /// only caller-controlled inputs; hosts, auth and signing remain broker-owned.
    async fn cloud_invoke(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        model_id: &str,
        request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError> {
        let _ = (context_ref, model_id, request);
        Err(BrokerError::NotConfigured(provider.to_string()))
    }

    /// Non-secret provider capability/status list.
    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError>;
}

// ── Local (in-process) broker ────────────────────────────────────────────────

#[async_trait]
pub trait CloudBackend: Send + Sync {
    async fn catalog(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        allow_stale: bool,
    ) -> Result<Vec<CloudCatalogEntry>, BrokerError>;
    async fn invoke(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        model_id: &str,
        request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError>;
}

#[derive(Clone)]
struct ProductionCloudBackend {
    /// Dedicated credential-bearing client: redirects are always disabled and
    /// both connect and whole-request time are bounded, independent of callers.
    http: reqwest::Client,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}
impl ProductionCloudBackend {
    fn new(_http: reqwest::Client) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("cloud HTTPS client configuration is valid");
        Self {
            http,
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn state(&self, provider: CloudProviderId) -> Result<serde_json::Value, BrokerError> {
        storage::load_cloud_state(provider.as_str())
            .map_err(BrokerError::Credential)?
            .ok_or_else(|| BrokerError::NotConfigured(provider.to_string()))
    }

    fn context_ref(&self, provider: CloudProviderId) -> Result<String, BrokerError> {
        use sha2::{Digest, Sha256};
        let state = self.state(provider)?;
        let public = serde_json::to_vec(&state["config"])
            .map_err(|_| BrokerError::Credential("invalid cloud context".into()))?;
        let digest = Sha256::digest([provider.as_str().as_bytes(), &public].concat());
        Ok(format!(
            "ctx-{}",
            digest[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }

    fn validate_context(
        &self,
        provider: CloudProviderId,
        supplied: &str,
    ) -> Result<String, BrokerError> {
        let opaque = self.context_ref(provider)?;
        // The canonical provider id is accepted only as the catalog bootstrap
        // selector. Every returned entry and persisted model route uses opaque.
        if supplied == provider.as_str() || supplied == opaque {
            Ok(opaque)
        } else {
            Err(BrokerError::Denied(
                "cloud context does not match stored provider".into(),
            ))
        }
    }

    async fn aws(
        &self,
    ) -> Result<super::aws_bedrock::AwsBedrockBroker<super::aws_bedrock::AwsHttpApi>, BrokerError>
    {
        #[derive(Deserialize)]
        struct State {
            config: super::cloud::AwsBedrockConfig,
            access_key: String,
            secret_key: String,
            session_token: String,
            expires_at: u64,
            registered_client: serde_json::Value,
            sso_access_token: String,
            sso_refresh_token: Option<String>,
            sso_expires_at: u64,
        }
        let decode = |value| {
            serde_json::from_value::<State>(value)
                .map_err(|_| BrokerError::Credential("invalid AWS broker state".into()))
        };
        let mut raw = self.state(CloudProviderId::AwsBedrock)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut s = decode(raw.clone())?;
        if s.expires_at <= now + 60_000 {
            // Role credentials are shared mutable broker state. Serialize refresh,
            // then reload so waiters consume the winner's atomic commit.
            let _guard = self.refresh_lock.lock().await;
            raw = self.state(CloudProviderId::AwsBedrock)?;
            s = decode(raw.clone())?;
            if s.expires_at <= now + 60_000 {
                let api =
                    super::aws_bedrock::AwsHttpApi::new(self.http.clone(), &s.config.sso_region);
                if s.sso_expires_at <= now + 60_000 {
                    let refresh = s
                        .sso_refresh_token
                        .as_deref()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            BrokerError::Credential("AWS SSO session expired; login again".into())
                        })?;
                    let client = super::aws_bedrock::RegisteredClient::new(
                        s.registered_client["id"].as_str().ok_or_else(|| {
                            BrokerError::Credential("invalid AWS client registration".into())
                        })?,
                        s.registered_client["secret"].as_str().ok_or_else(|| {
                            BrokerError::Credential("invalid AWS client registration".into())
                        })?,
                        s.registered_client["expires_at"].as_u64().ok_or_else(|| {
                            BrokerError::Credential("invalid AWS client registration".into())
                        })?,
                    );
                    if client.expires_at <= now / 1000 {
                        return Err(BrokerError::Credential(
                            "AWS client registration expired; login again".into(),
                        ));
                    }
                    use super::aws_bedrock::AwsApi;
                    let token = api
                        .create_token(
                            &client,
                            &s.config.sso_region,
                            super::aws_bedrock::TokenGrant::RefreshToken(refresh),
                        )
                        .await
                        .map_err(|_| {
                            BrokerError::Credential("AWS SSO refresh rejected; login again".into())
                        })?;
                    raw["sso_access_token"] = token.access().into();
                    if let Some(rotated) = token.refresh() {
                        raw["sso_refresh_token"] = rotated.into();
                    }
                    raw["sso_expires_at"] = (now + token.expires_in * 1000).into();
                    // A refresh token is single-use and may rotate. Commit the
                    // refreshed SSO session before the independent role fetch;
                    // otherwise a transient GetRoleCredentials failure loses
                    // the only usable token and strands the persisted login.
                    storage::save_cloud_state("aws-bedrock", &raw)
                        .map_err(BrokerError::Credential)?;
                    s = decode(raw.clone())?;
                }
                use super::aws_bedrock::AwsApi;
                let credentials = api
                    .get_role_credentials(
                        &s.config.sso_region,
                        &s.sso_access_token,
                        &s.config.account_id,
                        &s.config.role_name,
                    )
                    .await
                    .map_err(|_| {
                        BrokerError::Credential("AWS role refresh rejected; login again".into())
                    })?;
                raw["access_key"] = credentials.access_key().into();
                raw["secret_key"] = credentials.secret_key().into();
                raw["session_token"] = credentials.session_token().into();
                raw["expires_at"] = credentials.expires_at.into();
                storage::save_cloud_state("aws-bedrock", &raw).map_err(BrokerError::Credential)?;
                s = decode(raw)?;
            }
        }
        let api = super::aws_bedrock::AwsHttpApi::new(self.http.clone(), &s.config.sso_region);
        Ok(super::aws_bedrock::AwsBedrockBroker::from_credentials(
            api,
            s.config,
            super::aws_bedrock::RoleCredentials::new(
                s.access_key,
                s.secret_key,
                s.session_token,
                s.expires_at,
            ),
        ))
    }
    async fn azure_request(
        &self,
        state: &mut serde_json::Value,
        audience: super::azure_openai::AzureAudience,
    ) -> Result<String, BrokerError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let key = match audience {
            super::azure_openai::AzureAudience::Arm => "arm",
            super::azure_openai::AzureAudience::Inference => "inference",
        };
        if state[key]["expires_at"].as_u64().unwrap_or(0) > now + 60_000 {
            return state[key]["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| BrokerError::Credential("invalid Azure token state".into()));
        }
        // Serialize refresh and re-read after waiting so concurrent catalog and
        // invocation calls reuse the single atomically persisted rotation.
        let _refresh = self.refresh_lock.lock().await;
        *state = self.state(CloudProviderId::AzureOpenAi)?;
        if state[key]["expires_at"].as_u64().unwrap_or(0) > now + 60_000 {
            return state[key]["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| BrokerError::Credential("invalid Azure token state".into()));
        }
        let config: super::cloud::AzureOpenAiConfig =
            serde_json::from_value(state["config"].clone())
                .map_err(|_| BrokerError::Credential("invalid Azure broker state".into()))?;
        let client_id = state["client_id"]
            .as_str()
            .ok_or_else(|| BrokerError::Credential("invalid Azure client registration".into()))?;
        let refresh = state["refresh_token"]
            .as_str()
            .ok_or_else(|| BrokerError::Credential("invalid Azure refresh state".into()))?;
        let reg = super::azure_openai::AzureRegistration::production(Some(client_id.into()))
            .map_err(|e| BrokerError::Credential(e.to_string()))?;
        let r = super::azure_openai::refresh_request(&config, &reg, audience, refresh)
            .map_err(|e| BrokerError::Credential(e.to_string()))?;
        let response = self
            .http
            .post(r.url)
            .form(&r.form)
            .send()
            .await
            .map_err(|_| BrokerError::Transport("Azure refresh failed".into()))?;
        if !response.status().is_success() {
            return Err(BrokerError::Credential(
                "Azure refresh rejected; login again".into(),
            ));
        }
        let wire: serde_json::Value = response
            .json()
            .await
            .map_err(|_| BrokerError::Transport("invalid Azure token response".into()))?;
        let access = wire["access_token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| BrokerError::Transport("invalid Azure token response".into()))?
            .to_owned();
        if let Some(r) = wire["refresh_token"].as_str().filter(|v| !v.is_empty()) {
            state["refresh_token"] = r.into();
        }
        state[key] = serde_json::json!({"access_token": access, "expires_at": now + wire["expires_in"].as_u64().unwrap_or(3600) * 1000});
        storage::save_cloud_state("azure-openai", state).map_err(BrokerError::Credential)?;
        Ok(access)
    }

    async fn vertex_request(&self, state: &mut serde_json::Value) -> Result<String, BrokerError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if state["expires_at"].as_u64().unwrap_or(0) > now + 60_000 {
            return state["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| BrokerError::Credential("invalid Vertex token state".into()));
        }
        let _refresh = self.refresh_lock.lock().await;
        *state = self.state(CloudProviderId::GoogleVertex)?;
        if state["expires_at"].as_u64().unwrap_or(0) > now + 60_000 {
            return state["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| BrokerError::Credential("invalid Vertex token state".into()));
        }
        let client_id = state["client_id"]
            .as_str()
            .ok_or_else(|| BrokerError::Credential("invalid Vertex registration".into()))?;
        let refresh = state["refresh_token"]
            .as_str()
            .ok_or_else(|| BrokerError::Credential("invalid Vertex refresh state".into()))?;
        let response = self
            .http
            .post(super::google_vertex::TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
            ])
            .send()
            .await
            .map_err(|_| BrokerError::Transport("Vertex refresh failed".into()))?;
        if !response.status().is_success() {
            return Err(BrokerError::Credential(
                "Vertex refresh rejected; login again".into(),
            ));
        }
        let wire: serde_json::Value = response
            .json()
            .await
            .map_err(|_| BrokerError::Transport("invalid Vertex token response".into()))?;
        let access = wire["access_token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| BrokerError::Transport("invalid Vertex token response".into()))?
            .to_owned();
        if let Some(r) = wire["refresh_token"].as_str().filter(|v| !v.is_empty()) {
            state["refresh_token"] = r.into();
        }
        state["access_token"] = access.clone().into();
        state["expires_at"] = (now + wire["expires_in"].as_u64().unwrap_or(3600) * 1000).into();
        storage::save_cloud_state("google-vertex", state).map_err(BrokerError::Credential)?;
        Ok(access)
    }
}

#[async_trait]
impl CloudBackend for ProductionCloudBackend {
    async fn catalog(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        _allow_stale: bool,
    ) -> Result<Vec<CloudCatalogEntry>, BrokerError> {
        let opaque_context = self.validate_context(provider, context_ref)?;
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        match provider {
            CloudProviderId::AwsBedrock => {
                let broker = self.aws().await?;
                let label = broker.public_context().bedrock_region;
                broker
                    .catalog()
                    .await
                    .map_err(|_| {
                        BrokerError::Transport("AWS catalog failed (details redacted)".into())
                    })
                    .map(|xs| {
                        xs.into_iter()
                            .map(|x| CloudCatalogEntry {
                                provider,
                                id: x.id,
                                display_name: x.display_name,
                                context_ref: opaque_context.clone(),
                                context_label: label.clone(),
                                stale: false,
                                fetched_at,
                            })
                            .collect()
                    })
            }
            CloudProviderId::AzureOpenAi => {
                let mut state = self.state(provider)?;
                let config: super::cloud::AzureOpenAiConfig =
                    serde_json::from_value(state["config"].clone()).map_err(|_| {
                        BrokerError::Credential("invalid Azure broker state".into())
                    })?;
                let token = self
                    .azure_request(&mut state, super::azure_openai::AzureAudience::Arm)
                    .await?;
                let mut discovery =
                    super::azure_openai::DeploymentDiscovery::new(config.clone(), 20, 1000);
                let mut url = Some(discovery.initial_url());
                while let Some(next) = url {
                    let r = self
                        .http
                        .get(next)
                        .bearer_auth(&token)
                        .send()
                        .await
                        .map_err(|_| BrokerError::Transport("Azure catalog failed".into()))?;
                    if !r.status().is_success() {
                        return Err(BrokerError::Transport("Azure catalog rejected".into()));
                    }
                    let body = read_body_capped(r, MAX_CLOUD_CATALOG_BODY_BYTES)
                        .await
                        .map_err(|_| {
                            BrokerError::Transport("Azure catalog exceeded body limit".into())
                        })?;
                    url = discovery
                        .accept_page(&body)
                        .map_err(|_| BrokerError::Transport("invalid Azure catalog".into()))?;
                }
                discovery
                    .finish()
                    .map_err(|_| BrokerError::Transport("Azure catalog unavailable".into()))
                    .map(|xs| {
                        xs.into_iter()
                            .map(|x| CloudCatalogEntry {
                                provider,
                                id: x.id,
                                display_name: x.display_name,
                                context_ref: opaque_context.clone(),
                                context_label: format!(
                                    "{}/{}",
                                    config.resource_name, config.resource_group
                                ),
                                stale: false,
                                fetched_at,
                            })
                            .collect()
                    })
            }
            CloudProviderId::GoogleVertex => {
                let mut state = self.state(provider)?;
                let token = self.vertex_request(&mut state).await?;
                let config: super::cloud::GoogleVertexConfig =
                    serde_json::from_value(state["config"].clone()).map_err(|_| {
                        BrokerError::Credential("invalid Vertex broker state".into())
                    })?;
                let mut page: Option<String> = None;
                let mut out = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for _ in 0..20 {
                    let mut url=format!("https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models",config.location,config.project_id,config.location);
                    if let Some(p) = &page {
                        let mut parsed = url::Url::parse(&url).map_err(|_| {
                            BrokerError::Transport("invalid Vertex catalog URL".into())
                        })?;
                        parsed.query_pairs_mut().append_pair("pageToken", p);
                        url = parsed.into();
                    }
                    let r = self
                        .http
                        .get(url)
                        .bearer_auth(&token)
                        .send()
                        .await
                        .map_err(|_| BrokerError::Transport("Vertex catalog failed".into()))?;
                    if !r.status().is_success() {
                        return Err(BrokerError::Transport("Vertex catalog rejected".into()));
                    }
                    let v = cloud_catalog_json(r).await?;
                    for m in v["publisherModels"].as_array().into_iter().flatten() {
                        if let Some(name) = m["name"]
                            .as_str()
                            .filter(|n| n.starts_with("publishers/google/models/"))
                        {
                            if out.len() >= MAX_CLOUD_CATALOG_ENTRIES {
                                return Err(BrokerError::Transport(
                                    "Vertex catalog exceeded entry limit".into(),
                                ));
                            }
                            if !out.iter().any(|entry: &CloudCatalogEntry| {
                                entry.id == format!("google-vertex/{name}")
                            }) {
                                out.push(CloudCatalogEntry {
                                    provider,
                                    id: format!("google-vertex/{name}"),
                                    display_name: m["displayName"].as_str().unwrap_or(name).into(),
                                    context_ref: opaque_context.clone(),
                                    context_label: format!(
                                        "{}/{}",
                                        config.project_id, config.location
                                    ),
                                    stale: false,
                                    fetched_at,
                                });
                            }
                        }
                    }
                    page = v["nextPageToken"].as_str().map(str::to_owned);
                    match &page {
                        Some(p) if seen.insert(p.clone()) => {}
                        Some(_) => {
                            return Err(BrokerError::Transport("Vertex pagination loop".into()))
                        }
                        None => break,
                    }
                }
                if out.is_empty() {
                    Err(BrokerError::Transport("Vertex catalog is empty".into()))
                } else {
                    Ok(out)
                }
            }
        }
    }
    async fn invoke(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        model_id: &str,
        request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError> {
        self.validate_context(provider, context_ref)?;
        if !request.tools.is_empty() {
            return Err(BrokerError::Denied(
                "tools are not yet supported by cloud providers".into(),
            ));
        }
        let authorized = self.catalog(provider, context_ref, false).await?;
        if !authorized.iter().any(|entry| entry.id == model_id) {
            return Err(BrokerError::Denied(
                "model is not present in the current provider catalog".into(),
            ));
        }
        match provider {
            CloudProviderId::AwsBedrock => {
                let broker = self.aws().await?;
                if request.stream {
                    use futures::StreamExt;
                    let stream = broker
                        .converse_stream(model_id, request)
                        .await
                        .map_err(|_| {
                            BrokerError::Transport(
                                "AWS invocation failed (details redacted)".into(),
                            )
                        })?
                        .map(|event| {
                            event
                                .map_err(|_| {
                                    BrokerError::Transport(
                                        "AWS stream failed (details redacted)".into(),
                                    )
                                })
                                .map(|e| match e {
                                    super::aws_bedrock::ConverseEvent::TextDelta(delta) => {
                                        CloudEvent::TextDelta { delta }
                                    }
                                    super::aws_bedrock::ConverseEvent::ToolArguments {
                                        id,
                                        delta,
                                    } => CloudEvent::ToolArguments {
                                        id,
                                        name: None,
                                        delta,
                                    },
                                    super::aws_bedrock::ConverseEvent::Usage(u) => {
                                        CloudEvent::Usage {
                                            input_tokens: u.input_tokens,
                                            output_tokens: u.output_tokens,
                                        }
                                    }
                                    super::aws_bedrock::ConverseEvent::Done => CloudEvent::Done,
                                })
                        });
                    Ok(Box::pin(stream))
                } else {
                    let o = broker.converse(model_id, request).await.map_err(|_| {
                        BrokerError::Transport("AWS invocation failed (details redacted)".into())
                    })?;
                    Ok(Box::pin(futures::stream::iter(vec![
                        Ok(CloudEvent::TextDelta { delta: o.text }),
                        Ok(CloudEvent::Usage {
                            input_tokens: o.usage.input_tokens,
                            output_tokens: o.usage.output_tokens,
                        }),
                        Ok(CloudEvent::Done),
                    ])))
                }
            }
            CloudProviderId::AzureOpenAi => {
                let mut state = self.state(provider)?;
                let token = self
                    .azure_request(&mut state, super::azure_openai::AzureAudience::Inference)
                    .await?;
                let endpoint = super::azure_openai::AzureEndpoint::parse(
                    state["endpoint"]
                        .as_str()
                        .ok_or_else(|| BrokerError::Credential("invalid Azure endpoint".into()))?,
                )
                .map_err(|_| BrokerError::Credential("invalid Azure endpoint".into()))?;
                let deployment = model_id
                    .strip_prefix("azure-openai/")
                    .ok_or_else(|| BrokerError::Denied("invalid Azure model".into()))?;
                let body = serde_json::json!({"input":request.messages.into_iter().map(|m|serde_json::json!({"role":match m.role{super::cloud::MessageRole::Assistant=>"assistant",super::cloud::MessageRole::System=>"system",_=>"user"},"content":m.content})).collect::<Vec<_>>(),"stream":request.stream});
                let rr = super::azure_openai::responses_request(
                    &endpoint,
                    deployment,
                    &serde_json::to_vec(&body).unwrap(),
                )
                .map_err(|_| BrokerError::Denied("invalid Azure request".into()))?;
                let r = self
                    .http
                    .post(rr.url)
                    .bearer_auth(token)
                    .header("content-type", "application/json")
                    .body(rr.body)
                    .send()
                    .await
                    .map_err(|_| BrokerError::Transport("Azure invocation failed".into()))?;
                if !r.status().is_success() {
                    return Err(BrokerError::Transport("Azure invocation rejected".into()));
                }
                if request.stream {
                    use futures::StreamExt;
                    use std::sync::{
                        atomic::{AtomicBool, Ordering},
                        Arc,
                    };
                    let terminal = Arc::new(AtomicBool::new(false));
                    let seen = terminal.clone();
                    let events = sse_json_stream(r).filter_map(move |item| {
                        let seen = seen.clone();
                        async move {
                            match item {
                                Ok(Some(v)) => v["delta"]
                                    .as_str()
                                    .or_else(|| v["text"].as_str())
                                    .map(|delta| {
                                        Ok(CloudEvent::TextDelta {
                                            delta: delta.into(),
                                        })
                                    }),
                                Ok(None) => {
                                    seen.store(true, Ordering::SeqCst);
                                    Some(Ok(CloudEvent::Done))
                                }
                                Err(error) => Some(Err(error)),
                            }
                        }
                    });
                    let stream = events.chain(futures::stream::unfold(
                        (terminal, false),
                        |(terminal, emitted)| async move {
                            if terminal.load(Ordering::SeqCst) || emitted {
                                None
                            } else {
                                Some((
                                    Err(BrokerError::Transport(
                                        "Azure stream ended without [DONE]".into(),
                                    )),
                                    (terminal, true),
                                ))
                            }
                        },
                    ));
                    Ok(Box::pin(stream))
                } else {
                    let text = read_body_capped(r, MAX_CLOUD_STREAM_EVENT_BYTES).await?;
                    let v: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|_| BrokerError::Transport("invalid Azure response".into()))?;
                    let delta = v["output_text"]
                        .as_str()
                        .ok_or_else(|| {
                            BrokerError::Transport("Azure response omitted output".into())
                        })?
                        .to_owned();
                    Ok(Box::pin(futures::stream::iter(vec![
                        Ok(CloudEvent::TextDelta { delta }),
                        Ok(CloudEvent::Done),
                    ])))
                }
            }
            CloudProviderId::GoogleVertex => {
                let mut state = self.state(provider)?;
                let token = self.vertex_request(&mut state).await?;
                let config: super::cloud::GoogleVertexConfig =
                    serde_json::from_value(state["config"].clone())
                        .map_err(|_| BrokerError::Credential("invalid Vertex state".into()))?;
                let model = model_id
                    .strip_prefix("google-vertex/publishers/google/models/")
                    .filter(|m| {
                        m.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
                    })
                    .ok_or_else(|| BrokerError::Denied("invalid Vertex model".into()))?;
                let action = if request.stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                };
                let url=format!("https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/{}:{}",config.location,config.project_id,config.location,model,action);
                let body = serde_json::json!({"contents":request.messages.into_iter().map(|m|serde_json::json!({"role":if matches!(m.role,super::cloud::MessageRole::Assistant){"model"}else{"user"},"parts":[{"text":m.content}]})).collect::<Vec<_>>()});
                let r = self
                    .http
                    .post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| BrokerError::Transport("Vertex invocation failed".into()))?;
                if !r.status().is_success() {
                    return Err(BrokerError::Transport("Vertex invocation rejected".into()));
                }
                if request.stream {
                    use futures::StreamExt;
                    let events = sse_json_stream(r).map(|item| {
                        item.and_then(|value| {
                            value.ok_or_else(|| {
                                BrokerError::Transport(
                                    "Vertex stream terminated without a provider event".into(),
                                )
                            })
                        })
                    });
                    use std::sync::{
                        atomic::{AtomicBool, Ordering},
                        Arc,
                    };
                    let terminal = Arc::new(AtomicBool::new(false));
                    let terminal_in_events = terminal.clone();
                    let stream = events
                        .flat_map(move |item| {
                            let mut out = Vec::new();
                            match item {
                                Ok(v) => {
                                    if terminal_in_events.load(Ordering::SeqCst) {
                                        out.push(Err(BrokerError::Transport(
                                            "Vertex emitted data after terminal metadata".into(),
                                        )));
                                        return futures::stream::iter(out);
                                    }
                                    for c in v["candidates"].as_array().into_iter().flatten() {
                                        for p in
                                            c["content"]["parts"].as_array().into_iter().flatten()
                                        {
                                            if let Some(delta) = p["text"].as_str() {
                                                out.push(Ok(CloudEvent::TextDelta {
                                                    delta: delta.into(),
                                                }));
                                            }
                                        }
                                        if let Some(reason) = c["finishReason"].as_str() {
                                            const VALID: &[&str] = &[
                                                "STOP",
                                                "MAX_TOKENS",
                                                "SAFETY",
                                                "RECITATION",
                                                "OTHER",
                                                "BLOCKLIST",
                                                "PROHIBITED_CONTENT",
                                                "SPII",
                                                "MALFORMED_FUNCTION_CALL",
                                            ];
                                            if !VALID.contains(&reason) {
                                                out.push(Err(BrokerError::Transport(
                                                    "Vertex returned an invalid finish reason"
                                                        .into(),
                                                )));
                                            } else if terminal_in_events
                                                .swap(true, Ordering::SeqCst)
                                            {
                                                out.push(Err(BrokerError::Transport(
                                                    "Vertex returned duplicate terminal metadata"
                                                        .into(),
                                                )));
                                            } else {
                                                out.push(Ok(CloudEvent::Done));
                                            }
                                        }
                                    }
                                    if let Some(u) = v.get("usageMetadata") {
                                        out.push(Ok(CloudEvent::Usage {
                                            input_tokens: u["promptTokenCount"]
                                                .as_u64()
                                                .unwrap_or(0),
                                            output_tokens: u["candidatesTokenCount"]
                                                .as_u64()
                                                .unwrap_or(0),
                                        }));
                                    }
                                }
                                Err(error) => out.push(Err(error)),
                            }
                            futures::stream::iter(out)
                        })
                        .chain(futures::stream::unfold(
                            (terminal, false),
                            |(terminal, emitted)| async move {
                                if terminal.load(Ordering::SeqCst) || emitted {
                                    None
                                } else {
                                    Some((
                                        Err(BrokerError::Transport(
                                            "Vertex stream ended without finish metadata".into(),
                                        )),
                                        (terminal, true),
                                    ))
                                }
                            },
                        ));
                    Ok(Box::pin(stream))
                } else {
                    let text = read_body_capped(r, MAX_CLOUD_STREAM_EVENT_BYTES).await?;
                    let v: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|_| BrokerError::Transport("invalid Vertex response".into()))?;
                    let mut out = Vec::new();
                    for c in v["candidates"].as_array().into_iter().flatten() {
                        for p in c["content"]["parts"].as_array().into_iter().flatten() {
                            if let Some(delta) = p["text"].as_str() {
                                out.push(Ok(CloudEvent::TextDelta {
                                    delta: delta.into(),
                                }));
                            }
                        }
                    }
                    out.push(Ok(CloudEvent::Done));
                    Ok(Box::pin(futures::stream::iter(out)))
                }
            }
        }
    }
}

const MAX_CLOUD_CATALOG_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_CLOUD_CATALOG_ENTRIES: usize = 10_000;
const MAX_CLOUD_STREAM_EVENT_BYTES: usize = 1024 * 1024;

/// Incremental SSE parser. It retains only one bounded event, yields as soon as
/// an event delimiter arrives, and owns the response so dropping the consumer
/// cancels the upstream request.
fn sse_json_stream(
    resp: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<Option<serde_json::Value>, BrokerError>> + Send>> {
    use futures::StreamExt;
    let chunks = Box::pin(resp.bytes_stream());
    Box::pin(futures::stream::unfold(
        (chunks, Vec::<u8>::new(), false),
        |(mut chunks, mut buf, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                    let frame: Vec<u8> = buf.drain(..end + 2).collect();
                    let text = match std::str::from_utf8(&frame[..end]) {
                        Ok(v) => v,
                        Err(_) => {
                            return Some((
                                Err(BrokerError::Transport("invalid SSE encoding".into())),
                                (chunks, buf, true),
                            ))
                        }
                    };
                    let data = text
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        return Some((Ok(None), (chunks, buf, true)));
                    }
                    let value = serde_json::from_str(&data)
                        .map_err(|_| BrokerError::Transport("invalid SSE event".into()));
                    let terminal = value.is_err();
                    return Some((value.map(Some), (chunks, buf, terminal)));
                }
                match chunks.next().await {
                    Some(Ok(chunk)) if buf.len() + chunk.len() <= MAX_CLOUD_STREAM_EVENT_BYTES => {
                        buf.extend_from_slice(&chunk)
                    }
                    Some(_) => {
                        return Some((
                            Err(BrokerError::Transport(
                                "cloud stream event exceeded limit".into(),
                            )),
                            (chunks, buf, true),
                        ))
                    }
                    None if buf.is_empty() => return None,
                    None => {
                        return Some((
                            Err(BrokerError::Transport("truncated SSE event".into())),
                            (chunks, buf, true),
                        ))
                    }
                }
            }
        },
    ))
}

/// Read one catalog page without allowing an upstream to allocate an unbounded body.
async fn cloud_catalog_json(resp: reqwest::Response) -> Result<serde_json::Value, BrokerError> {
    let body = read_body_capped(resp, MAX_CLOUD_CATALOG_BODY_BYTES).await?;
    serde_json::from_str(&body)
        .map_err(|_| BrokerError::Transport("invalid cloud catalog response".into()))
}

/// In-process credential broker. This module is the ONLY place runtime-serving
/// code may read `auth.json`, `provider.<key>` config values, or credential
/// environment variables.
pub struct LocalBroker {
    http: reqwest::Client,
    /// Test seam: overrides the local endpoint URL without env/config.
    local_base_url: Option<String>,
    /// Test seam: overrides the pinned Anthropic usage URL.
    anthropic_usage_url: Option<String>,
    /// Test seam: overrides the pinned cloudcode-pa base URL.
    google_gemini_base_url: Option<String>,
    /// Test seam: overrides the pinned ChatGPT backend base URL.
    openai_codex_base_url: Option<String>,
    /// Test seam: overrides the pinned Meta Muse API origin.
    muse_base_url: Option<String>,
    /// Test seam: refresh endpoint overrides (the Muse key mint).
    refresh_endpoints: super::provider::RefreshEndpoints,
    /// Idle (between-bytes) timeout for the per-request no-redirect client.
    stream_idle_timeout: Duration,
    /// Time budget for buffered (non-streaming) requests.
    request_timeout: Duration,
    /// Buffered response size cap.
    max_response_bytes: usize,
    cloud_backend: Option<Arc<dyn CloudBackend>>,
    /// Injected account policy. `None` → resolved from env + config on every
    /// selection (no global mutation, picks up `synaps auth use` changes).
    account_policy: Option<AccountPolicy>,
    auto_policy: Option<super::AutoSelectionPolicy>,
    auto_policy_cache: std::sync::Mutex<
        Option<(
            std::path::PathBuf,
            Option<std::time::SystemTime>,
            super::AutoSelectionPolicy,
        )>,
    >,
    /// Successful vends only, scoped by model and immutable seat identity.
    current_accounts: SharedMap<(CredentialRef, SeatIdentity)>,
    /// In-memory cooldowns for automatic selection, keyed by storage key:
    /// the whole account's, and limits of one model's requests.
    cooldowns: Arc<std::sync::Mutex<BTreeMap<String, KeyCooldowns>>>,
    /// Test seam: overrides the usage endpoint for typed usage snapshots.
    usage_endpoint_override: Option<String>,
    /// Max age of a reused reading of an account a selection is about to
    /// switch to, and of the pinned account near its limit.
    max_snapshot_age: Duration,
    /// Max age of a reused reading of any other account, before its share of
    /// `idle_snapshot_spread`.
    idle_snapshot_age: Duration,
    /// Spread of the idle accounts' max age over the provider's listing.
    idle_snapshot_spread: Duration,
    /// Max age of a reused reading of the account a selection is pinned to
    /// while it shows room (near its limit: `max_snapshot_age`).
    pinned_snapshot_age: Duration,
    /// A test clock: when set, automatic selection and its usage cache take
    /// the time from it instead of the wall clock (only a `TestClock` sets
    /// it, with the `testing` feature).
    clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Short-lived cache of the last usage snapshot per *seat key* (credential
    /// file + storage key + [`SeatIdentity`] of the stored credential) so
    /// automatic selection does not re-poll every provider on every token
    /// vend. A re-login, removal or profile switch changes the key, so a
    /// reading can never be attributed to a different seat than the one it
    /// was taken from. An entry past its account's max age, or one with a
    /// reset that has passed since it was read, is never reused, and
    /// neither is one cached before the slot's reading was dropped (its
    /// epoch in the usage-read table moved, through any broker).
    snapshots: SharedMap<CachedReading>,
    /// Process-wide usage-read table (single-flight per slot, held failed
    /// reads, last good readings); see [`super::usage_reads`].
    usage_reads: Arc<super::usage_reads::UsageReads>,
    /// Small per-process number for the usage-read log line (not secret).
    instance: u64,
    /// Cached account policy keyed by the config file's modification time
    /// (env overlay is applied on every read; it is cheap and has no I/O).
    policy_cache: Arc<std::sync::Mutex<Option<CachedPolicy>>>,
    /// `http` is known never to follow redirects ([`Self::without_redirects`]).
    redirect_free: bool,
    /// What `synaps auth state` shows per slot (credential file + storage
    /// key): the last usage read and the last successful one. Diagnostic
    /// only; selection never reads it.
    observations: SharedMap<SlotObservations>,
    /// Last automatic selection per [`Self::current_key`] (provider + model).
    selections: SharedMap<super::auth_state::SelectionState>,
    /// Refused token refreshes held per [`Self::state_key`] (see
    /// [`REFUSED_REFRESH_HOLD`]). Memory only.
    refresh_holds: SharedMap<RefreshHold>,
    /// Stored seats for cooldown lookups, valid for one version of the
    /// credential file ([`Self::cooldown_seat`]).
    seat_cache: Arc<std::sync::Mutex<Option<SeatCache>>>,
    /// Stored-login reads [`Self::cooldown_seat`] made (test seam).
    seat_loads: Arc<std::sync::atomic::AtomicU64>,
}

/// One version of the credential file: modification time, length and
/// inode (writes replace the file by rename, so each one gets a new inode;
/// the coarse mtime alone could miss two writes in one clock tick).
type FileStamp = (std::time::SystemTime, u64, u64);

/// The version of the file at `path`, if it can be stat'ed.
fn file_stamp(path: &std::path::Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((meta.modified().ok()?, meta.len(), inode))
}

/// The stored seat of each slot looked up, read from one version of the
/// credential file.
struct SeatCache {
    path: std::path::PathBuf,
    stamp: FileStamp,
    seats: BTreeMap<String, Option<SeatIdentity>>,
}

/// One slot's recorded usage reads (`synaps auth state`).
#[derive(Debug, Clone, Default)]
struct SlotObservations {
    last_usage: Option<super::auth_state::ObservedOutcome>,
    last_usage_ok_ms: Option<u64>,
}

/// Process-shared map keyed by storage key or seat key.
type SharedMap<T> = Arc<std::sync::Mutex<BTreeMap<String, T>>>;

/// A broker's cached usage reading of a seat, and the slot's epoch in the
/// usage-read table ([`super::usage_reads::UsageReads::epoch`]) when the
/// broker looked it up, before the reading was taken: a drop of the slot's
/// reading since then ends the copy.
#[derive(Clone)]
struct CachedReading {
    snapshot: super::usage::UsageSnapshot,
    epoch: u64,
}

/// The accounts that failed earlier in one automatic selection (storage key
/// → why): a failed refresh of the chosen seat or a failed usage read. They
/// count as failed for the rest of that selection, without a new request.
type FailedThisSelection = BTreeMap<String, super::quota_policy::ObservationFailure>;

/// Config-derived policy tagged with the config file mtime it was read at.
type CachedPolicy = (Option<std::time::SystemTime>, AccountPolicy);

/// A reported provider limit for one account (non-secret).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cooldown {
    until_ms: u64,
    reason: String,
    /// When it was reported (epoch ms): only a request sent after this
    /// clears it ([`CredentialBroker::clear_cooldown`]).
    reported_at_ms: u64,
}

impl Cooldown {
    /// A limit reported until `until_ms`: a missing or past one lasts the
    /// default length from now, and none lasts longer than the maximum. The
    /// reason is bounded.
    fn reported(until_ms: Option<u64>, reason: &str) -> Self {
        let now_ms = crate::epoch_millis();
        let max_until = now_ms.saturating_add(MAX_COOLDOWN.as_millis() as u64);
        Self {
            until_ms: until_ms
                .filter(|t| *t > now_ms)
                .unwrap_or(now_ms + DEFAULT_COOLDOWN.as_millis() as u64)
                .min(max_until),
            reason: crate::truncate_str(reason, 64).to_string(),
            reported_at_ms: now_ms,
        }
    }

    fn active_at(&self, now_ms: u64) -> bool {
        self.until_ms > now_ms
    }
}

/// The limits reported under one cooldown key: the whole account's, and
/// limits of one model's requests by the model token the provider names
/// (`fable`: Fable's weekly limit). In memory only, as every cooldown.
#[derive(Debug, Clone, Default)]
struct KeyCooldowns {
    all: Option<Cooldown>,
    models: BTreeMap<String, Cooldown>,
}

impl KeyCooldowns {
    /// The whole account's cooldown, while active at `now_ms`.
    fn account_at(&self, now_ms: u64) -> Option<&Cooldown> {
        self.all.as_ref().filter(|c| c.active_at(now_ms))
    }

    /// The latest cooldown active at `now_ms` that stops a request for
    /// `model`: the whole account's, or one of a model the request names
    /// ([`super::quota_policy::model_matches`]). With no model (the CLI's
    /// token, the keeper) every one counts: a request of an unknown model
    /// never passes a limit, as for a model's usage window.
    fn for_request_at(&self, model: Option<&str>, now_ms: u64) -> Option<&Cooldown> {
        let models = self
            .models
            .iter()
            .filter(move |(limited, _)| {
                model.is_none_or(|model| {
                    super::quota_policy::model_matches(limited.as_str(), model)
                })
            })
            .map(|(_, cooldown)| cooldown);
        self.all
            .iter()
            .chain(models)
            .filter(|c| c.active_at(now_ms))
            .max_by_key(|c| c.until_ms)
    }
}

/// The keys one account's cooldown is kept under
/// ([`LocalBroker::cooldown_keys`]). Non-secret: the seat part is an
/// identity digest, never a token.
struct CooldownKeys {
    /// The stored seat's key; None when the slot has no readable login.
    seat: Option<String>,
    /// The slot's key.
    slot: String,
    /// The stored seat names a provider account id.
    identified: bool,
}

impl CooldownKeys {
    /// Keys a report writes: the seat's, and the slot's unless the seat
    /// names an account id.
    fn written(&self) -> impl Iterator<Item = &str> + '_ {
        let slot = (!self.identified).then_some(self.slot.as_str());
        self.seat.as_deref().into_iter().chain(slot)
    }

    /// Keys a lookup or a clear reads: the seat's and the slot's.
    fn read(&self) -> impl Iterator<Item = &str> + '_ {
        self.seat.as_deref().into_iter().chain([self.slot.as_str()])
    }
}

/// Cooldown applied when a limit is reported without a reset hint.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// A refused token refresh of one slot. No Debug: `material` is
/// secret-derived.
struct RefreshHold {
    /// The stored login the provider refused.
    material: MaterialDigest,
    failure: super::token::RefreshFailure,
    at_ms: u64,
}

/// How long a refused refresh (`refresh_rejected` /
/// `refresh_action_required`) of an unchanged stored login is held. While
/// held, neither refresh site (the chosen account's vend, the usage read's
/// pre-read refresh) asks the provider again: the account fails with the
/// held kind. The hold ends when the stored login changes (a new login, or a
/// refresh by another process), on `synaps auth state --clear`, on a restart,
/// or after this window; then ONE refresh is tried and a new refusal holds
/// it again. Bounded so that a provider wrongly refusing during an outage
/// cannot leave every account dead until a login or a restart; one probe per
/// account per window is no storm. Retryable refresh failures are never
/// held.
pub const REFUSED_REFRESH_HOLD: Duration = Duration::from_secs(15 * 60);

/// One slot's refused-refresh hold as the token helper sees it
/// ([`super::token::RefusalMemory`]): checked after the stored login is
/// re-read under the slot's refresh gate and lock, and written there from
/// the exchange's answer. So concurrent callers of one slot make one
/// decision: one exchange, and every waiter takes the held refusal. The
/// digest is of the login actually refused. No Debug (the holds are
/// secret-derived).
struct SlotHold {
    holds: SharedMap<RefreshHold>,
    /// [`LocalBroker::state_key`] of the slot.
    key: String,
    provider: OAuthProviderId,
    /// Set when this call was answered from the hold (no exchange).
    answered: std::sync::atomic::AtomicBool,
    /// A refusal drops the slot's usage reading too.
    drop: ReadingDrop,
}

impl SlotHold {
    fn answered(&self) -> bool {
        self.answered.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl super::token::RefusalMemory for SlotHold {
    /// The hold of this slot while it is inside [`REFUSED_REFRESH_HOLD`]
    /// and is for exactly `creds`; anything else ends it.
    fn held(&self, creds: &super::OAuthCredentials) -> Option<super::token::RefreshFailure> {
        let material = MaterialDigest::of(self.provider, creds);
        let window_ms = REFUSED_REFRESH_HOLD.as_millis() as u64;
        let mut holds = self.holds.lock().unwrap_or_else(|e| e.into_inner());
        let hold = holds.get(&self.key)?;
        if crate::epoch_millis() < hold.at_ms.saturating_add(window_ms)
            && hold.material == material
        {
            self.answered.store(true, std::sync::atomic::Ordering::SeqCst);
            return Some(hold.failure.clone());
        }
        holds.remove(&self.key);
        None
    }

    /// Holds a refusal (`Credential` / `ActionRequired`) of `creds`, and
    /// drops the slot's usage reading ([`ReadingDrop`]); a retryable kind is
    /// never held. Store failures never arrive here.
    fn refused(&self, creds: &super::OAuthCredentials, failure: &super::token::RefreshFailure) {
        use super::token::RefreshFailureKind as R;
        if !matches!(failure.kind, R::Credential | R::ActionRequired) {
            return;
        }
        let hold = RefreshHold {
            material: MaterialDigest::of(self.provider, creds),
            failure: failure.clone(),
            at_ms: crate::epoch_millis(),
        };
        self.holds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.key.clone(), hold);
        self.drop.drop_reading();
    }
}

/// Drops one slot's usage reading when the provider refuses its login
/// (p1-r1 B6): the broker's cached readings of the slot, its last good
/// reading in the usage-read table, and, in the writer, its saved copy, so
/// neither a reuse nor the grace outlives the refusal, across a reload too.
/// As a [`super::token::RefusalMemory`] of its own (an explicit account, a
/// rejected token's recovery) it holds nothing: it only drops.
struct ReadingDrop {
    reads: Arc<super::usage_reads::UsageReads>,
    /// [`LocalBroker::read_key`] of the slot.
    key: String,
    scope: super::usage_reads::ReadScope,
    snapshots: SharedMap<CachedReading>,
    /// The slot's part of a [`LocalBroker::seat_key`].
    seat_part: String,
    /// The broker's test clock, if it has one.
    clock: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl ReadingDrop {
    /// Drop the reading. The saved readings are loaded first, so a saved
    /// one is dropped even if this process has not read the slot; the save
    /// is synchronous (the writer's file follows the drop at once).
    fn drop_reading(&self) {
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.contains(&self.seat_part));
        let now_ms = match &self.clock {
            Some(clock) => clock.load(std::sync::atomic::Ordering::SeqCst),
            None => crate::epoch_millis(),
        };
        self.reads.load_saved(&self.scope, now_ms);
        if self.reads.forget_last_good(&self.key, now_ms) {
            self.reads.save(&self.scope, now_ms);
        }
    }
}

impl super::token::RefusalMemory for ReadingDrop {
    fn held(&self, _creds: &super::OAuthCredentials) -> Option<super::token::RefreshFailure> {
        None
    }

    /// A refusal (`Credential` / `ActionRequired`) drops the reading.
    fn refused(&self, _creds: &super::OAuthCredentials, failure: &super::token::RefreshFailure) {
        use super::token::RefreshFailureKind as R;
        if matches!(failure.kind, R::Credential | R::ActionRequired) {
            self.drop_reading();
        }
    }
}

/// Longest cooldown a client report may impose (a weekly window plus slack);
/// anything longer is clamped so a bad report cannot bench a seat forever.
const MAX_COOLDOWN: Duration = Duration::from_secs(8 * 24 * 60 * 60);
/// Default staleness bound for an account automatic selection is about to
/// switch to, and for the account it is pinned to once that one is near its
/// limit ([`DEFAULT_NEAR_LIMIT_PERCENT`]). Short on purpose: a switch is a
/// spend decision and should rest on a fresh reading.
pub const DEFAULT_MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(60);
/// Default age up to which a reading of any other account is reused. Such a
/// reading only ranks the account: before a selection switches to it, the
/// account is read again under [`DEFAULT_MAX_SNAPSHOT_AGE`].
pub const DEFAULT_IDLE_SNAPSHOT_AGE: Duration = Duration::from_secs(10 * 60);
/// Default spread over the idle accounts' re-read clocks: account `i` of `n`
/// in the provider's listing is reused up to
/// `DEFAULT_IDLE_SNAPSHOT_AGE + spread * i / n`, so their re-reads fall on
/// different turn starts instead of in one burst.
pub const DEFAULT_IDLE_SNAPSHOT_SPREAD: Duration = Duration::from_secs(2 * 60);
/// Default age up to which the reading of the Anthropic account automatic
/// selection is pinned to is reused while it shows room (no window of the
/// request at [`DEFAULT_NEAR_LIMIT_PERCENT`] or more). Another provider's
/// pinned account is read again past [`DEFAULT_MAX_SNAPSHOT_AGE`]. A
/// provider limit on the message request itself still cools the account
/// down at once.
pub const DEFAULT_PINNED_SNAPSHOT_AGE: Duration = Duration::from_secs(10 * 60);
/// Utilization (percent) of a window of the request at which the pinned
/// account is near its limit: its reading is then reused only up to
/// [`DEFAULT_NEAR_LIMIT_SNAPSHOT_AGE`], unless it shows the account's paid
/// extra usage off.
pub const DEFAULT_NEAR_LIMIT_PERCENT: f64 = 90.0;
/// Default age up to which the pinned account's reading is reused near its
/// limit (a spent window can be billed as extra usage instead of limited).
/// Not for a reading that shows extra usage off: a spent window then
/// answers the message request with a limit, which cools the account and
/// moves the request on, so it keeps the pinned age.
pub const DEFAULT_NEAR_LIMIT_SNAPSHOT_AGE: Duration = DEFAULT_MAX_SNAPSHOT_AGE;

/// A manual clock for automatic-selection tests
/// ([`LocalBroker::with_test_clock`]): it starts at the wall clock's time
/// and moves only when the test advances it. Only with the `testing`
/// feature; a production build has neither the type nor the setter.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone)]
pub struct TestClock(Arc<std::sync::atomic::AtomicU64>);

#[cfg(any(test, feature = "testing"))]
impl Default for TestClock {
    fn default() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU64::new(
            crate::epoch_millis(),
        )))
    }
}

#[cfg(any(test, feature = "testing"))]
impl TestClock {
    /// The clock's time (epoch ms).
    pub fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Move the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.0
            .fetch_add(by.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Idle timeout between bytes on the broker's per-request no-redirect
/// client (Muse, Kimi, Gemini, Codex/Anthropic catalog). Matches the runtime
/// client's 90s read timeout that xAI streams already get.
pub const BROKER_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Gemini (Code Assist) sends no data chunk while it thinks (thought
/// streaming is off), and a hard prompt can think silently for minutes;
/// its stream has no retry, so it gets a longer idle limit.
pub const GEMINI_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Next per-process broker number for the usage-read log line.
fn next_broker_instance() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl LocalBroker {
    pub fn new(http: reqwest::Client) -> Self {
        let cloud_backend = Arc::new(ProductionCloudBackend::new(http.clone()));
        Self {
            redirect_free: false,
            http,
            local_base_url: None,
            anthropic_usage_url: None,
            google_gemini_base_url: None,
            openai_codex_base_url: None,
            muse_base_url: None,
            refresh_endpoints: Default::default(),
            stream_idle_timeout: BROKER_STREAM_IDLE_TIMEOUT,
            request_timeout: PROXY_REQUEST_TIMEOUT,
            max_response_bytes: MAX_PROXY_RESPONSE_BYTES,
            cloud_backend: Some(cloud_backend),
            account_policy: None,
            auto_policy: None,
            auto_policy_cache: std::sync::Mutex::new(None),
            current_accounts: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            cooldowns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            usage_endpoint_override: None,
            max_snapshot_age: DEFAULT_MAX_SNAPSHOT_AGE,
            idle_snapshot_age: DEFAULT_IDLE_SNAPSHOT_AGE,
            idle_snapshot_spread: DEFAULT_IDLE_SNAPSHOT_SPREAD,
            pinned_snapshot_age: DEFAULT_PINNED_SNAPSHOT_AGE,
            clock: None,
            snapshots: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            usage_reads: super::usage_reads::process_table(),
            instance: next_broker_instance(),
            policy_cache: Arc::new(std::sync::Mutex::new(None)),
            observations: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            selections: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            refresh_holds: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            seat_cache: Arc::new(std::sync::Mutex::new(None)),
            seat_loads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// The same broker on a client that never follows redirects, for
    /// bounded single-attempt requests: a 3xx is returned as an (answered)
    /// error status, and a refused connection can then only mean that no
    /// byte of the request was sent. Without this a refused connection is
    /// not typed as not-sent, because a redirect may already have delivered
    /// the request to an earlier hop.
    ///
    /// Fails closed: when no such client can be built the broker is not
    /// returned at all, so a bounded request is never sent on a client that
    /// follows redirects.
    pub fn without_redirects(self) -> Result<Self, BrokerError> {
        self.with_redirect_free_http(redirect_free_http())
    }

    /// [`Self::without_redirects`] with the result of
    /// [`redirect_free_http`] (`None`: it could not be built). Private: only
    /// that factory's client may be marked redirect-free.
    fn with_redirect_free_http(
        mut self,
        http: Option<reqwest::Client>,
    ) -> Result<Self, BrokerError> {
        let http = http.ok_or_else(redirect_free_unavailable)?;
        self.http = http;
        self.redirect_free = true;
        Ok(self)
    }

    pub fn with_cloud_backend(mut self, backend: Arc<dyn CloudBackend>) -> Self {
        self.cloud_backend = Some(backend);
        self
    }

    /// Pin the account policy instead of reading env + config per call.
    pub fn with_account_policy(mut self, policy: AccountPolicy) -> Self {
        self.account_policy = Some(policy);
        self
    }

    /// Pin the automatic ranking policy (tests/embedding).
    pub fn with_auto_policy(mut self, policy: super::AutoSelectionPolicy) -> Self {
        self.auto_policy = Some(policy);
        self
    }

    fn auto_policy(&self) -> super::AutoSelectionPolicy {
        if let Some(policy) = &self.auto_policy {
            return policy.clone();
        }
        let path = crate::config::resolve_read_path("config");
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut cache = self
            .auto_policy_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((cached_path, cached_time, policy)) = cache.as_ref() {
            if cached_path == &path && *cached_time == mtime {
                return policy.clone();
            }
        }
        let auth = crate::config::load_config().auth;
        let (policy, warnings) = super::AutoSelectionPolicy::from_config_map(&auth.auto);
        let policy = policy.with_order_config(&auth.order);
        for warning in warnings {
            tracing::warn!(%warning, "automatic account policy");
        }
        *cache = Some((path, mtime, policy.clone()));
        policy
    }

    fn current_key(provider: OAuthProviderId, model: Option<&str>) -> String {
        format!(
            "{}|{}|{}",
            storage::auth_file_path().display(),
            provider,
            model.unwrap_or("")
        )
    }

    /// Test seam: point the pinned ChatGPT backend host at a loopback fake.
    /// Only relaxes the base URL; allowlists, bearer/header pairing and
    /// redirect denial are unchanged.
    #[doc(hidden)]
    pub fn with_openai_codex_base_url_for_tests(mut self, base_url: impl Into<String>) -> Self {
        self.openai_codex_base_url = Some(base_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Test seam: point the pinned Meta Muse origin at a loopback fake. Only
    /// relaxes the origin; the path allowlist, bearer resolution, 401
    /// recovery and redirect denial are unchanged.
    #[doc(hidden)]
    pub fn with_muse_base_url_for_tests(mut self, base_url: impl Into<String>) -> Self {
        self.muse_base_url = Some(base_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Idle (between-bytes) limit for `provider_key`'s no-redirect client.
    fn stream_idle_timeout_for(&self, provider_key: &str) -> Duration {
        if provider_key == "google-gemini" {
            GEMINI_STREAM_IDLE_TIMEOUT.max(self.stream_idle_timeout)
        } else {
            self.stream_idle_timeout
        }
    }

    /// Test seam: shorten [`BROKER_STREAM_IDLE_TIMEOUT`] (not Gemini's).
    #[doc(hidden)]
    pub fn with_stream_idle_timeout_for_tests(mut self, idle: Duration) -> Self {
        self.stream_idle_timeout = idle;
        self
    }

    /// Test seam: send Muse key re-mints to a loopback fake mint
    /// (`http://127.0.0.1:<port>/...`) instead of the pinned
    /// `https://api.meta.ai/muse-code/key`. The request itself (headers,
    /// body, OAuth bearer) is built exactly as in production.
    #[doc(hidden)]
    pub fn with_muse_mint_url_for_tests(mut self, mint_url: impl Into<String>) -> Self {
        self.refresh_endpoints =
            std::mem::take(&mut self.refresh_endpoints).with_muse_mint_url_for_tests(mint_url);
        self
    }

    /// Test seam: send Anthropic token refreshes to a loopback fake token
    /// endpoint instead of the real one; a non-loopback URL is refused at
    /// refresh time. The request is built exactly as in production.
    #[doc(hidden)]
    pub fn with_anthropic_token_url_for_tests(mut self, url: impl Into<String>) -> Self {
        self.refresh_endpoints =
            std::mem::take(&mut self.refresh_endpoints).with_anthropic_token_url_for_tests(url);
        self
    }

    /// Capacity-based selection for `provider` regardless of the configured
    /// selector (used by the broker daemon when a remote client's own policy
    /// is `auto`). Fails closed like [`access_token_pinned_for`].
    ///
    /// [`access_token_pinned_for`]: CredentialBroker::access_token_pinned_for
    pub async fn access_token_auto(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        self.vend_auto(provider, model, &[]).await
    }

    /// Test seam: point typed usage fetches at a fake server.
    #[doc(hidden)]
    pub fn with_usage_endpoint_override(mut self, url: impl Into<String>) -> Self {
        self.usage_endpoint_override = Some(url.into());
        self
    }

    /// Bound on usage-snapshot age for automatic selection, the same for
    /// every account (no longer idle bound, no spread).
    pub fn with_max_snapshot_age(mut self, age: Duration) -> Self {
        self.max_snapshot_age = age;
        self.idle_snapshot_age = age;
        self.idle_snapshot_spread = Duration::ZERO;
        self.pinned_snapshot_age = age;
        self
    }

    /// Age up to which the pinned account's reading is reused while it
    /// shows room ([`DEFAULT_PINNED_SNAPSHOT_AGE`]).
    pub fn with_pinned_snapshot_age(mut self, age: Duration) -> Self {
        self.pinned_snapshot_age = age;
        self
    }

    /// Test seam: automatic selection and its usage cache take the time
    /// from `clock` instead of the wall clock, and every usage reading this
    /// broker takes is stamped with the clock's time (the resets a reading
    /// reports stay absolute times). The usage-read table's holds and pauses
    /// take this broker's time too; a last good reading keeps the wall
    /// clock's time of its read. Only with the `testing` feature.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_test_clock(mut self, clock: &TestClock) -> Self {
        self.clock = Some(clock.0.clone());
        self
    }

    /// Test seam: this broker reads usage through a usage-read table of its
    /// own, empty, instead of the process's: what a broker sees after a
    /// daemon reload. Only with the `testing` feature.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_new_usage_table(mut self) -> Self {
        self.usage_reads = Arc::new(super::usage_reads::UsageReads::default());
        self
    }

    /// Test seam: [`with_new_usage_table`](Self::with_new_usage_table), and
    /// that table asks to write the saved readings of this store, as the
    /// daemon's does (it writes them if no other table holds their lease).
    /// Only with the `testing` feature.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_new_saving_usage_table(self) -> Self {
        let broker = self.with_new_usage_table();
        let saved = storage::auth_file_path().with_file_name(super::usage_reads::SAVED_READINGS_FILE);
        broker.usage_reads.write_saved_readings(&saved);
        broker
    }

    /// Test seam: this broker's usage-read table stops writing the saved
    /// readings and frees their lease, as the daemon's does at its shutdown.
    /// Only with the `testing` feature.
    #[cfg(any(test, feature = "testing"))]
    pub fn stop_saving_usage_readings(&self) {
        self.usage_reads.stop_writing();
    }

    /// Test seam: this broker reads usage through `other`'s usage-read
    /// table, as two brokers of one process do. Only with the `testing`
    /// feature.
    #[cfg(any(test, feature = "testing"))]
    pub fn sharing_usage_table_with(mut self, other: &LocalBroker) -> Self {
        self.usage_reads = other.usage_reads.clone();
        self
    }

    /// Now (epoch ms) for automatic selection and its usage cache: the test
    /// clock's time when one is set, else the wall clock's.
    fn now_ms(&self) -> u64 {
        match &self.clock {
            Some(clock) => clock.load(std::sync::atomic::Ordering::SeqCst),
            None => crate::epoch_millis(),
        }
    }

    /// Effective policy: injected, else env > config > default. The config
    /// file is re-parsed only when its modification time changes, so a vend
    /// never re-reads config (and never re-emits warnings) needlessly.
    fn policy(&self) -> AccountPolicy {
        if let Some(policy) = &self.account_policy {
            return policy.clone();
        }
        let mtime = std::fs::metadata(crate::config::resolve_read_path("config"))
            .and_then(|m| m.modified())
            .ok();
        let mut cache = self.policy_cache.lock().unwrap_or_else(|e| e.into_inner());
        let config_policy = match cache.as_ref() {
            Some((cached_mtime, policy)) if *cached_mtime == mtime => policy.clone(),
            _ => {
                let (policy, _warnings) = AccountPolicy::from_config_map(
                    &crate::config::load_config().auth.accounts,
                );
                *cache = Some((mtime, policy.clone()));
                policy
            }
        };
        config_policy.with_env_overlay()
    }

    /// Where `cred`'s cooldown is kept. Limits follow the seat, not an
    /// alias that can be re-used: aliases of one seat share its cooldown.
    /// A login without a provider account id has a seat derived from its
    /// token, which a refresh rotates, so its cooldown is also kept under
    /// its slot and survives the rotation. The slot can't tell that login
    /// gaining an id from another account logging into the label, so any
    /// new login into the slot inherits it too (under-use only; `auth state
    /// --clear` drops it). The cooldown of a login WITH an account id is
    /// kept under that id only: a login with another id does not inherit it.
    fn cooldown_keys(&self, cred: &CredentialRef) -> CooldownKeys {
        let store = storage::auth_file_path().display().to_string();
        let slot = format!("{store}|{}|{}", cred.provider, cred.storage_key());
        let seat = self.cooldown_seat(cred);
        CooldownKeys {
            identified: seat.as_ref().is_some_and(|s| s.seat_fingerprint().is_some()),
            seat: seat.map(|s| format!("{store}|{}|{}", cred.provider, s.as_str())),
            slot,
        }
    }

    /// The seat stored in `cred`'s slot, for cooldown keys. A turn asks for
    /// its account's cooldown before every request while any cooldown is
    /// active, so the stored login is read once per version of the
    /// credential file ([`FileStamp`]), not per lookup:
    /// a later lookup costs one stat. A write by this or another process
    /// (a login, a refresh) changes the version, so a rotated or replaced
    /// login is read again. Never used for spending (vends and usage reads
    /// load the slot themselves).
    fn cooldown_seat(&self, cred: &CredentialRef) -> Option<SeatIdentity> {
        let path = storage::auth_file_path();
        let stamp = file_stamp(&path);
        let key = cred.storage_key();
        let mut cache = self.seat_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(stamp), Some(c)) = (stamp, cache.as_ref()) {
            if c.path == path && c.stamp == stamp {
                if let Some(seat) = c.seats.get(&key) {
                    return seat.clone();
                }
            }
        }
        self.seat_loads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seat = self.stored_seat(cred).ok();
        match stamp {
            Some(stamp) => {
                let fresh = cache
                    .as_ref()
                    .is_some_and(|c| c.path == path && c.stamp == stamp);
                if !fresh {
                    *cache = Some(SeatCache {
                        path,
                        stamp,
                        seats: BTreeMap::new(),
                    });
                }
                if let Some(c) = cache.as_mut() {
                    c.seats.insert(key, seat.clone());
                }
            }
            None => *cache = None,
        }
        seat
    }

    /// Test seam: how many times cooldown lookups read a stored login.
    #[doc(hidden)]
    pub fn cooldown_seat_loads_for_tests(&self) -> u64 {
        self.seat_loads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether any cooldown is active at `now_ms`: a whole account's, or a
    /// limit of one model's requests.
    fn any_cooldown_active(&self, now_ms: u64) -> bool {
        self.cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|key| key.for_request_at(None, now_ms).is_some())
    }

    /// `cred`'s active cooldown of the whole account: the latest one under
    /// its keys. A limit of one model's requests is not one (the listing
    /// and `auth state` show the whole account's only).
    fn active_cooldown_at(&self, cred: &CredentialRef, now_ms: u64) -> Option<Cooldown> {
        self.latest_cooldown(cred, |key| key.account_at(now_ms))
    }

    /// `cred`'s active cooldown for a request for `model`
    /// ([`KeyCooldowns::for_request_at`]): the latest one under its keys.
    fn cooldown_for_at(
        &self,
        cred: &CredentialRef,
        model: Option<&str>,
        now_ms: u64,
    ) -> Option<Cooldown> {
        self.latest_cooldown(cred, |key| key.for_request_at(model, now_ms))
    }

    /// The latest of the cooldowns `pick` takes from `cred`'s keys.
    fn latest_cooldown(
        &self,
        cred: &CredentialRef,
        pick: impl Fn(&KeyCooldowns) -> Option<&Cooldown>,
    ) -> Option<Cooldown> {
        // Nothing cooling, nothing to look up: no read of the stored login
        // (a turn asks before every request).
        if !self
            .cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|key| pick(key).is_some())
        {
            return None;
        }
        self.cooldown_for_keys(&self.cooldown_keys(cred), pick)
    }

    /// The latest of the cooldowns `pick` takes under `keys` (worked out
    /// once per selection: [`Self::selection_plan`]).
    fn cooldown_for_keys(
        &self,
        keys: &CooldownKeys,
        pick: impl Fn(&KeyCooldowns) -> Option<&Cooldown>,
    ) -> Option<Cooldown> {
        let cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        keys.read()
            .filter_map(|key| cooldowns.get(key))
            .filter_map(&pick)
            .max_by_key(|c| c.until_ms)
            .cloned()
    }

    /// Reading an expired cooldown is non-mutating (preview shares this path).
    fn cooldown_until(&self, storage_key: &str, now_ms: u64) -> Option<u64> {
        let cred = CredentialRef::parse_storage_key(storage_key)?;
        self.active_cooldown_at(&cred, now_ms).map(|c| c.until_ms)
    }

    // ── `synaps auth state` ─────────────────────────────────────────────

    /// What this broker remembers about the accounts `query` selects: no
    /// network request, no refresh; the account list comes from the store.
    /// With `query.clear`, their in-memory state is dropped first.
    pub fn auth_state(
        &self,
        query: &super::auth_state::AuthStateQuery,
    ) -> Result<super::auth_state::AuthStateReport, BrokerError> {
        auth_state_report(Some(self), query)
    }

    /// The active cooldown of `cred`'s current seat, with its reason.
    fn cooldown_state(
        &self,
        cred: &CredentialRef,
        now_ms: u64,
    ) -> Option<super::auth_state::CooldownState> {
        self.active_cooldown_at(cred, now_ms)
            .map(|c| super::auth_state::CooldownState {
                until_ms: c.until_ms,
                reason: super::auth_state::CooldownReason::from_reported(&c.reason),
            })
    }

    /// Drop the in-memory state of `creds`: cooldowns, cached usage
    /// readings, held usage-read failures and last good readings in the
    /// process-wide table (its gates stay), `auth state` records, held
    /// refused refreshes and stickiness. With
    /// `whole_providers` (no single account was named), also every cooldown
    /// and selection record of those providers, including ones a previous
    /// occupant of a slot left. Never touches the credential store.
    fn clear_state(&self, creds: &[CredentialRef], whole_providers: &[OAuthProviderId]) {
        let store = storage::auth_file_path().display().to_string();
        let keys: Vec<CooldownKeys> = creds.iter().map(|c| self.cooldown_keys(c)).collect();
        {
            let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
            for key in keys.iter().flat_map(|k| k.read()) {
                cooldowns.remove(key);
            }
            for provider in whole_providers {
                let prefix = format!("{store}|{provider}|");
                cooldowns.retain(|k, _| !k.starts_with(&prefix));
            }
        }
        for cred in creds {
            self.forget_snapshots(cred);
            // The last good reading through the drop path (its saved copy
            // too, so a restart does not bring it back), then the hold.
            self.reading_drop(cred).drop_reading();
            self.usage_reads.forget(&self.read_key(cred));
        }
        {
            let mut observations = self.observations.lock().unwrap_or_else(|e| e.into_inner());
            for cred in creds {
                observations.remove(&Self::state_key(cred));
            }
        }
        {
            let mut holds = self.refresh_holds.lock().unwrap_or_else(|e| e.into_inner());
            for cred in creds {
                holds.remove(&Self::state_key(cred));
            }
            for provider in whole_providers {
                let slot = format!("{store}|{}", provider.as_str());
                let named = format!("{slot}@");
                holds.retain(|k, _| *k != slot && !k.starts_with(&named));
            }
        }
        self.current_accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (current, _)| !creds.contains(current));
        let mut selections = self.selections.lock().unwrap_or_else(|e| e.into_inner());
        for provider in whole_providers {
            let prefix = format!("{store}|{provider}|");
            selections.retain(|k, _| !k.starts_with(&prefix));
        }
    }

    /// Verify an explicitly addressed slot exists. Named slots that are
    /// missing are `UnknownAccount` (never a fallback); the default slot
    /// keeps the historical load-miss `Credential` error from the refresh
    /// path so existing "not logged in" classifiers keep working.
    fn check_slot_exists(&self, cred: &CredentialRef) -> Result<(), BrokerError> {
        if let Account::Named(label) = &cred.account {
            match storage::load_credential(cred) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(BrokerError::UnknownAccount {
                        provider: cred.provider.as_str().to_string(),
                        label: label.as_str().to_string(),
                    })
                }
                Err(e) => return Err(BrokerError::Credential(e)),
            }
        }
        Ok(())
    }

    /// Resolve and vend for a request: explicit account (validated, never a
    /// fallback) or the policy; `auto` goes through capacity selection with
    /// the token paired to the proven seat. `exclude` lists the accounts the
    /// request already used: `auto` selects among the rest, and an explicit
    /// account in it is refused.
    async fn resolve_and_vend(
        &self,
        provider: OAuthProviderId,
        explicit: Option<Account>,
        model: Option<&str>,
        exclude: &[CredentialRef],
    ) -> Result<PinnedToken, BrokerError> {
        let selector = match explicit {
            Some(account) => AccountSelector::Account(account),
            None => self.policy().selector(provider),
        };
        match selector {
            AccountSelector::Account(account) => {
                let credential = CredentialRef::new(provider, account);
                if exclude.contains(&credential) {
                    return Err(BrokerError::NoAccountAvailable {
                        provider: provider.as_str().to_string(),
                        reason: format!(
                            "the selected account {} was already used by this request",
                            credential.account
                        ),
                        failure: None,
                    });
                }
                let token = self.access_token_for(&credential).await?;
                Ok(PinnedToken { credential, token })
            }
            AccountSelector::Auto => self.vend_auto(provider, model, exclude).await,
            AccountSelector::Invalid { source, reason } => {
                Err(BrokerError::InvalidAccount(format!("{source}: {reason}")))
            }
        }
    }

    /// Bound on select → seat-pinned vend rounds when the chosen slot is
    /// replaced underneath us (re-login storm / rotation by another process).
    const MAX_AUTO_VEND_ROUNDS: usize = 2;

    /// `auto`: select on fresh usage, then vend a token for exactly the seat
    /// that usage was read from. If the slot changed in between (re-login,
    /// removal, rotation by another party) the reading is discarded and the
    /// selection re-runs on the new content, at most
    /// [`Self::MAX_AUTO_VEND_ROUNDS`] times; never a token for a seat whose
    /// capacity was not proven.
    ///
    /// A refused refresh of the chosen seat (`refresh_rejected`,
    /// `refresh_action_required`) does not end the selection while another
    /// account has proven capacity: the account counts as failed, with its
    /// refresh kind, for the rest of this selection and the selection runs
    /// again without it. A retryable refresh failure (transient, lock busy,
    /// other) ends the selection with that kind ([`BrokerError::Refresh`]):
    /// it says nothing against the account, and moving would cost the turn
    /// its prompt cache. A failed usage
    /// read counts the same way (a held transient read is answered by the
    /// usage-read table without a request). So each account gets at most one
    /// refresh attempt and one failed read per selection, and the rounds are
    /// bounded by the number of accounts. When none is left, the selection
    /// fails with every account's kind (S1's classified failure). A refused
    /// refresh is also held across selections ([`REFUSED_REFRESH_HOLD`]): a
    /// held account fails with its held kind and no exchange.
    async fn vend_auto(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        exclude: &[CredentialRef],
    ) -> Result<PinnedToken, BrokerError> {
        use super::token::{ensure_fresh_credential_for_seat, SeatVendError};
        let mut failed = FailedThisSelection::new();
        let mut seat_changes = 0;
        loop {
            let (credential, seat) = match self
                .select_auto(provider, model, exclude, &mut failed)
                .await
            {
                Ok(choice) => choice,
                Err(e) => {
                    let failure = match &e {
                        BrokerError::NoAccountAvailable { failure, .. } => failure.clone(),
                        // The account list could not be read (select_auto
                        // logged it).
                        _ => Some(super::quota_policy::SelectionFailure::new(
                            super::quota_policy::SelectionFailureKind::CredentialStore,
                        )),
                    };
                    self.record_selection(provider, model, None, failure, None);
                    return Err(e);
                }
            };
            // A refused refresh of this login is held: no new exchange.
            if let Some(held) = self.held_refresh(&credential) {
                let cause = refresh_failure(&held);
                tracing::info!(
                    credential = %credential,
                    kind = %cause.kind,
                    "automatic choice not vended: its refused token refresh is held until the stored login changes; selecting again"
                );
                failed.insert(credential.storage_key(), cause);
                continue;
            }
            // The hold is checked again, and a refusal recorded, under the
            // slot's refresh gate: concurrent turns make one decision.
            let hold = self.slot_hold(&credential);
            let memory: Arc<dyn super::token::RefusalMemory> = hold.clone();
            match ensure_fresh_credential_for_seat(
                &self.http,
                &credential,
                &seat,
                &self.refresh_endpoints,
                Some(memory),
            )
            .await
            {
                Ok(creds) => {
                    self.current_accounts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            Self::current_key(provider, model),
                            (credential.clone(), seat),
                        );
                    self.record_selection(provider, model, Some(&credential), None, None);
                    return Ok(PinnedToken {
                        credential,
                        token: AccessToken {
                            token: creds.access,
                            expires: creds.expires,
                        },
                    });
                }
                Err(SeatVendError::SeatChanged) => {
                    tracing::info!(credential = %credential, "slot changed after selection; re-selecting");
                    self.forget_snapshots(&credential);
                    seat_changes += 1;
                    if seat_changes >= Self::MAX_AUTO_VEND_ROUNDS {
                        break;
                    }
                }
                // Typed, so a transient refresh failure of the chosen seat
                // is never reported as an unusable login.
                Err(SeatVendError::Other(f)) => {
                    let cause = refresh_failure(&f);
                    // Only a refusal (a login or an action fixes it) moves
                    // the selection on. A retryable failure (the token
                    // endpoint down, the refresh lock busy) says nothing
                    // against this account, and another account would cost
                    // the turn its prompt cache for nothing: the selection
                    // fails with the refresh's own kind, and the next turn
                    // tries again.
                    if !matches!(
                        f.kind,
                        super::token::RefreshFailureKind::Credential
                            | super::token::RefreshFailureKind::ActionRequired
                    ) {
                        tracing::warn!(
                            credential = %credential,
                            kind = %cause.kind,
                            "automatic choice could not be vended: token refresh failed"
                        );
                        self.record_selection(provider, model, Some(&credential), None, Some(cause));
                        return Err(BrokerError::Refresh(auto_vend_failure(&credential, f)));
                    }
                    if hold.answered() {
                        // A refusal another caller just got, under the gate.
                        tracing::info!(
                            credential = %credential,
                            kind = %cause.kind,
                            "automatic choice not vended: its refused token refresh is held until the stored login changes; selecting again"
                        );
                    } else {
                        tracing::warn!(
                            credential = %credential,
                            kind = %cause.kind,
                            "automatic choice could not be vended: token refresh failed; selecting again"
                        );
                    }
                    failed.insert(credential.storage_key(), cause);
                }
            }
        }
        let failure = super::quota_policy::SelectionFailure::new(
            super::quota_policy::SelectionFailureKind::SeatChurn,
        );
        self.record_selection(provider, model, None, Some(failure.clone()), None);
        Err(selection_failed(
            provider,
            model,
            "selected credential kept changing while its capacity was being proven".into(),
            failure,
        ))
    }

    /// Record the outcome of an automatic selection for `synaps auth state`.
    /// `vend_failure`: the chosen account's token refresh failed for a
    /// retryable reason, which ended the selection. (A refused refresh is
    /// no outcome: the selection runs again without that account.)
    fn record_selection(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        chosen: Option<&CredentialRef>,
        failure: Option<super::quota_policy::SelectionFailure>,
        vend_failure: Option<super::quota_policy::ObservationFailure>,
    ) {
        let state = super::auth_state::SelectionState {
            provider: provider.as_str().to_string(),
            model: model.map(str::to_string),
            at_ms: crate::epoch_millis(),
            chosen: chosen.map(|c| c.account.label_str().to_string()),
            failure,
            vend_failure,
        };
        self.selections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Self::current_key(provider, model), state);
    }

    /// Cache key for one stored seat: credential file + slot + identity of
    /// the material stored there. Never contains token material.
    fn seat_key(cred: &CredentialRef, seat: &SeatIdentity) -> String {
        format!(
            "{}|{}|{}",
            storage::auth_file_path().display(),
            cred.storage_key(),
            seat.as_str()
        )
    }

    /// Identity of the credential currently stored in `cred`'s slot.
    fn stored_seat(&self, cred: &CredentialRef) -> Result<SeatIdentity, BrokerError> {
        match storage::load_credential(cred).map_err(BrokerError::Credential)? {
            Some(creds) => Ok(SeatIdentity::of(cred.provider, &creds)),
            None => Err(BrokerError::UnknownAccount {
                provider: cred.provider.as_str().to_string(),
                label: cred.account.label_str().to_string(),
            }),
        }
    }

    /// The cached reading for `key` while it is no older than `max_age` and
    /// no reset it reports has passed since it was read (that window has
    /// started again, so the reading is out of date). A reading cached at
    /// another epoch than the slot's `epoch` now (its reading was dropped
    /// since, through any broker of the process) is removed, never used.
    fn cached_snapshot(
        &self,
        key: &str,
        max_age: Duration,
        epoch: u64,
    ) -> Option<super::usage::UsageSnapshot> {
        let max_age_ms = max_age.as_millis() as u64;
        let now_ms = self.now_ms();
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        if snapshots.get(key).is_some_and(|cached| cached.epoch != epoch) {
            snapshots.remove(key);
            return None;
        }
        snapshots
            .get(key)
            .map(|cached| &cached.snapshot)
            .filter(|s| !s.is_stale(now_ms, max_age_ms) && !s.reset_since_observed(now_ms))
            .cloned()
    }

    /// Usage-read table key for `cred`'s slot: credential file + storage key
    /// + usage endpoint. Stable across token rotation; no token material.
    fn read_key(&self, cred: &CredentialRef) -> String {
        format!(
            "{}|{}|{}",
            storage::auth_file_path().display(),
            cred.storage_key(),
            self.usage_endpoint_override.as_deref().unwrap_or("")
        )
    }

    /// Where `cred`'s readings are saved (beside its credential store), and
    /// which reads a usage 429 on it pauses: every Anthropic slot of the
    /// store at the same usage endpoint. No other provider has the pause.
    fn read_scope(&self, cred: &CredentialRef) -> super::usage_reads::ReadScope {
        self.store_scope(cred.provider)
    }

    /// [`Self::read_scope`] of any slot of `provider` in the credential store.
    fn store_scope(&self, provider: OAuthProviderId) -> super::usage_reads::ReadScope {
        let store = storage::auth_file_path();
        let endpoint = self.usage_endpoint_override.as_deref().unwrap_or("");
        let prefix = format!("{}|", store.display());
        let anthropic = provider == OAuthProviderId::Anthropic;
        super::usage_reads::ReadScope {
            saved: store.with_file_name(super::usage_reads::SAVED_READINGS_FILE),
            pause: anthropic.then(|| format!("{prefix}{}|{endpoint}", provider.as_str())),
            store: prefix,
            persisted: anthropic,
        }
    }

    /// Prune the usage readings of `provider`'s slots (at this broker's
    /// usage endpoint) that the store's listing `listed` no longer has, so
    /// the writer's next saves leave them out (p1-r1 SF4). The store's saved
    /// readings are loaded first (p1-r2 SF1), so a saved reading of a slot
    /// that left the store is pruned too; a reading such a slot gets after
    /// this (a direct usage read, or a first read in flight) stays until a
    /// later selection prunes it (see `usage_reads::UsageReads::keep_listed`).
    /// Providers without saved readings are left alone.
    fn keep_listed_readings(&self, provider: OAuthProviderId, listed: &[AccountSummary]) {
        if provider != OAuthProviderId::Anthropic {
            return;
        }
        let keys: std::collections::HashSet<String> = listed
            .iter()
            .filter_map(AccountSummary::credential_ref)
            .map(|cred| self.read_key(&cred))
            .collect();
        let prefix = format!("{}|", storage::auth_file_path().display());
        let suffix = format!("|{}", self.usage_endpoint_override.as_deref().unwrap_or(""));
        let covers = |key: &str| {
            key.strip_prefix(prefix.as_str())
                .and_then(|rest| rest.strip_suffix(suffix.as_str()))
                .and_then(CredentialRef::parse_storage_key)
                .is_some_and(|cred| cred.provider == provider)
        };
        self.usage_reads
            .keep_listed(&self.store_scope(provider), covers, &keys, self.now_ms());
    }

    /// Drop every cached reading for `cred`'s slot (any seat). The
    /// single-flight gate lives in the process-wide usage-read table and is
    /// never dropped, so this cannot open a second flight.
    fn forget_snapshots(&self, cred: &CredentialRef) {
        let prefix = format!("|{}|", cred.storage_key());
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.contains(&prefix));
    }

    /// Fresh usage snapshot for `cred` plus the seat it was read from: the
    /// cache is consulted first (never past `max_age` or a passed reset,
    /// and only for the seat currently stored in the slot), otherwise the
    /// process-wide usage-read table: ONE read per slot at a time across
    /// every broker in the process, and a held failed read is answered
    /// without a request. The token for the read and the reported seat come
    /// from the same load, so the pair is exact even if the slot changes
    /// concurrently.
    async fn fresh_usage(
        &self,
        cred: &CredentialRef,
        max_age: Duration,
    ) -> Result<(super::usage::UsageSnapshot, SeatIdentity, u64), super::usage_reads::ReadError> {
        if let Some(held) = self.held_refresh(cred) {
            return Err(Self::held_read(cred, held).into_read_error());
        }
        let seat = self
            .stored_seat(cred)
            .map_err(|e| UsageReadError::slot(e).into_read_error())?;
        let key = Self::seat_key(cred, &seat);
        // The slot's epoch before anything is looked up or read: a cached
        // copy is used, and the reading this returns is cached and handed
        // back, only at it. A drop between here and a read's admission
        // costs at most one extra read later (the copy is cached at the
        // older epoch), and a selection holding the reading treats it as
        // superseded.
        let read_key = self.read_key(cred);
        let epoch = self.usage_reads.epoch(&read_key);
        if let Some(cached) = self.cached_snapshot(&key, max_age, epoch) {
            return Ok((cached, seat, epoch));
        }
        // A reading saved before this process started (a restart, a
        // reload) counts as a cached one, at its original age, until this
        // process reads the slot.
        let now_ms = self.now_ms();
        self.usage_reads.load_saved(&self.read_scope(cred), now_ms);
        if let Some(saved) = self
            .usage_reads
            .saved_reading(&read_key, &seat)
            .filter(|s| {
                !s.is_stale(now_ms, max_age.as_millis() as u64) && !s.reset_since_observed(now_ms)
            })
        {
            self.snapshots.lock().unwrap_or_else(|e| e.into_inner()).insert(
                key,
                CachedReading {
                    snapshot: saved.clone(),
                    epoch,
                },
            );
            return Ok((saved, seat, epoch));
        }
        let (mut snapshot, used) = self.shared_read(cred).await?;
        if self.clock.is_some() {
            // Under a test clock, a reading is taken at the clock's time.
            snapshot.observed_at = self.now_ms();
        }
        // One slot holds one seat: a reading for its current content
        // supersedes anything cached for a previous occupant.
        self.forget_snapshots(cred);
        let used_key = Self::seat_key(cred, &used);
        self.snapshots.lock().unwrap_or_else(|e| e.into_inner()).insert(
            used_key,
            CachedReading {
                snapshot: snapshot.clone(),
                epoch,
            },
        );
        Ok((snapshot, used, epoch))
    }

    /// Read-only usage for `cred` through the process-wide usage-read table,
    /// reporting the seat whose token made the request. A failure keeps its
    /// typed cause. A held refused refresh answers without a request.
    async fn usage_with_seat(
        &self,
        cred: &CredentialRef,
    ) -> Result<(super::usage::UsageSnapshot, SeatIdentity), UsageReadError> {
        if let Some(held) = self.held_refresh(cred) {
            return Err(Self::held_read(cred, held));
        }
        self.shared_read(cred)
            .await
            .map_err(|e| UsageReadError::from_read(&e))
    }

    /// [`CredentialBroker::usage_or_last_known`]: what `synaps status`
    /// shows of `cred`'s usage. A saved reading of the account stored in
    /// the slot now, at most [`Self::status_reuse_age`] old (the inclusive
    /// bound of [`UsageSnapshot::is_stale`]) and with no reset passed
    /// since, is shown without a request, unless the slot's failed read is
    /// held or the reads are paused. Otherwise one read as
    /// [`CredentialBroker::usage`] does, through the process-wide
    /// usage-read table, which sends nothing while the slot's failed read
    /// is held or the reads are paused. A read the usage endpoint
    /// rate-limited (its own 429, a held one, or the pause) also gives when
    /// this process reads the slot next and the slot's last good reading,
    /// only if it was read from the account stored there now: during the
    /// pause, a saved reading recent enough to show is that last good
    /// reading, so the row doesn't turn on the reading's age. Writes no
    /// saved readings itself: only the table holding their lease (the
    /// daemon's) writes them.
    ///
    /// [`UsageSnapshot::is_stale`]: super::usage::UsageSnapshot::is_stale
    async fn last_known_usage(
        &self,
        cred: &CredentialRef,
    ) -> Result<super::usage::UsageSnapshot, Box<UsageReadFailure>> {
        use super::usage_reads::ReadError;
        let plain = |error: BrokerError| Box::new(UsageReadFailure::plain(error));
        if let Some(held) = self.held_refresh(cred) {
            return Err(plain(Self::held_read(cred, held).error));
        }
        let key = self.read_key(cred);
        let scope = self.read_scope(cred);
        let now_ms = self.now_ms();
        self.usage_reads.load_saved(&scope, now_ms);
        // While the slot's failed read is held or the reads are paused, no
        // saved reading is shown as a success: the read below sends nothing
        // and gives the hold's or the pause's failure (a usage 429: the
        // rate-limited form, with the saved reading as the last good one).
        // An unreadable or empty slot is not reused; its read below fails
        // as `usage` does.
        let held = self.usage_reads.held_at(&key, now_ms).is_some()
            || self.usage_reads.paused_at(&scope, now_ms).is_some();
        let recent = if held {
            None
        } else {
            self.stored_seat(cred).ok().and_then(|seat| {
                self.usage_reads.saved_reading(&key, &seat).filter(|s| {
                    !s.is_stale(now_ms, self.status_reuse_age(cred, s).as_millis() as u64)
                        && !s.reset_since_observed(now_ms)
                })
            })
        };
        if let Some(saved) = recent {
            tracing::debug!(
                credential = %cred,
                "usage read not sent: the saved reading is recent enough to show"
            );
            return Ok(saved);
        }
        match self.shared_read(cred).await {
            Ok((snapshot, _)) => Ok(snapshot),
            Err(ReadError::Transient {
                failure,
                retry_at_ms,
            }) if failure.is_rate_limited() => {
                // The account stored now (it may have changed during the
                // read): never another account's reading.
                let last_good = self
                    .stored_seat(cred)
                    .ok()
                    .and_then(|seat| self.usage_reads.last_good_for(&key, &seat));
                Err(Box::new(UsageReadFailure {
                    error: failure.to_broker_error(),
                    rate_limited: Some(RateLimitedRead {
                        retry_at_ms,
                        last_good,
                    }),
                }))
            }
            Err(e) => Err(plain(e.to_broker_error())),
        }
    }

    /// How old a saved reading of `cred` may be when `synaps status` shows
    /// it instead of a request: the age at which automatic selection reads
    /// the account it is pinned to again. For an Anthropic account that is
    /// [`DEFAULT_PINNED_SNAPSHOT_AGE`] while the reading shows room, or
    /// shows the account's paid extra usage off; near its limit
    /// ([`DEFAULT_NEAR_LIMIT_PERCENT`] on any window) with extra usage on
    /// or unknown, and for any other provider's account,
    /// `max_snapshot_age`.
    fn status_reuse_age(
        &self,
        cred: &CredentialRef,
        snapshot: &super::usage::UsageSnapshot,
    ) -> Duration {
        let capacity = capacity_from_snapshot(cred, snapshot);
        if near_its_limit(&capacity, None) && capacity.extra_usage_enabled != Some(false) {
            self.max_snapshot_age
        } else {
            self.pinned_age_for(cred.provider)
        }
    }

    /// Key of a slot's `auth state` records: credential file + storage key.
    /// Never contains token material.
    fn state_key(cred: &CredentialRef) -> String {
        format!(
            "{}|{}",
            storage::auth_file_path().display(),
            cred.storage_key()
        )
    }

    /// Digest of the login stored in `cred`'s slot now; `None` when the
    /// slot is empty or the store can't be read.
    fn stored_material(cred: &CredentialRef) -> Option<MaterialDigest> {
        storage::load_credential(cred)
            .ok()
            .flatten()
            .map(|creds| MaterialDigest::of(cred.provider, &creds))
    }

    /// The held refused refresh of `cred`'s slot, if the provider refused
    /// the login stored there now and [`REFUSED_REFRESH_HOLD`] has not
    /// passed. A changed login or an ended window drops the hold, so the
    /// next refresh asks the provider again. Without a hold for the slot
    /// nothing is loaded.
    fn held_refresh(&self, cred: &CredentialRef) -> Option<super::token::RefreshFailure> {
        let key = Self::state_key(cred);
        let (material, failure, at_ms) = {
            let holds = self.refresh_holds.lock().unwrap_or_else(|e| e.into_inner());
            let hold = holds.get(&key)?;
            (hold.material.clone(), hold.failure.clone(), hold.at_ms)
        };
        let window_ms = REFUSED_REFRESH_HOLD.as_millis() as u64;
        let in_window = crate::epoch_millis() < at_ms.saturating_add(window_ms);
        if in_window && Self::stored_material(cred).as_ref() == Some(&material) {
            return Some(failure);
        }
        let mut holds = self.refresh_holds.lock().unwrap_or_else(|e| e.into_inner());
        if holds.get(&key).is_some_and(|h| h.at_ms == at_ms) {
            holds.remove(&key);
        }
        None
    }

    /// The refused-refresh hold of `cred`'s slot, for the token helper to
    /// check and write under the slot's refresh gate ([`SlotHold`]).
    fn slot_hold(&self, cred: &CredentialRef) -> Arc<SlotHold> {
        Arc::new(SlotHold {
            holds: self.refresh_holds.clone(),
            key: Self::state_key(cred),
            provider: cred.provider,
            answered: std::sync::atomic::AtomicBool::new(false),
            drop: self.reading_drop(cred),
        })
    }

    /// What drops `cred`'s usage reading when its login is refused
    /// ([`ReadingDrop`]).
    fn reading_drop(&self, cred: &CredentialRef) -> ReadingDrop {
        ReadingDrop {
            reads: self.usage_reads.clone(),
            key: self.read_key(cred),
            scope: self.read_scope(cred),
            snapshots: self.snapshots.clone(),
            seat_part: format!("|{}|", cred.storage_key()),
            clock: self.clock.clone(),
        }
    }

    /// A usage read of an account whose refused refresh is held: no refresh
    /// and no request, so not recorded as a read; the failure is the held
    /// refusal (it names the login command or remedy).
    fn held_read(cred: &CredentialRef, held: super::token::RefreshFailure) -> UsageReadError {
        let err = UsageReadError::refresh(held);
        tracing::debug!(
            credential = %cred,
            kind = %err.failure.kind,
            "usage read not sent: the refused token refresh is held until the stored login changes"
        );
        err
    }

    /// What `auth state` shows of `cred`'s held refused refresh (the same
    /// test as [`Self::held_refresh`], dropping nothing). Never the digest.
    fn refresh_hold_state(
        &self,
        cred: &CredentialRef,
        now_ms: u64,
    ) -> Option<super::auth_state::RefreshHoldState> {
        let (material, failure, at_ms) = {
            let holds = self.refresh_holds.lock().unwrap_or_else(|e| e.into_inner());
            let hold = holds.get(&Self::state_key(cred))?;
            (hold.material.clone(), hold.failure.clone(), hold.at_ms)
        };
        let until_ms = at_ms.saturating_add(REFUSED_REFRESH_HOLD.as_millis() as u64);
        (now_ms < until_ms && Self::stored_material(cred).as_ref() == Some(&material)).then(|| {
            super::auth_state::RefreshHoldState {
                failure: refresh_failure(&failure),
                refused_at_ms: at_ms,
                until_ms,
            }
        })
    }

    /// Test seam: move every held refused refresh `by` into the past (the
    /// hold window's injected clock).
    #[doc(hidden)]
    pub fn age_refresh_holds_for_tests(&self, by: Duration) {
        let by_ms = by.as_millis() as u64;
        for hold in self
            .refresh_holds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            hold.at_ms = hold.at_ms.saturating_sub(by_ms);
        }
    }

    /// Test seam: the digest a held refusal of `cred` keeps, so a test can
    /// check that it appears in no output.
    #[doc(hidden)]
    pub fn refresh_hold_digest_for_tests(&self, cred: &CredentialRef) -> Option<String> {
        self.refresh_holds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::state_key(cred))
            .map(|h| h.material.expose_for_tests().to_string())
    }

    fn record_usage(
        &self,
        cred: &CredentialRef,
        failure: Option<super::quota_policy::ObservationFailure>,
    ) {
        let at_ms = crate::epoch_millis();
        let mut observations = self.observations.lock().unwrap_or_else(|e| e.into_inner());
        let slot = observations.entry(Self::state_key(cred)).or_default();
        if failure.is_none() {
            slot.last_usage_ok_ms = Some(at_ms);
        }
        slot.last_usage = Some(super::auth_state::ObservedOutcome { at_ms, failure });
    }

    /// One usage read for `cred` through the process-wide table, with its
    /// INFO line (account label, outcome, failure kind/status; no token).
    /// A read that went to the network (or joined one in flight) is
    /// recorded for `synaps auth state`; a held read sends nothing and is
    /// not a new read, and a superseded one (a good reading that landed
    /// after the slot's reading was dropped) is no observation: its line
    /// says `outcome=superseded`, and nothing is recorded.
    async fn shared_read(&self, cred: &CredentialRef) -> super::usage_reads::ReadResult {
        use super::usage_reads::{ReadError, ReadPath, USAGE_READ_TARGET};
        let key = self.read_key(cred);
        let (result, path) = self
            .usage_reads
            .read(
                &key,
                &self.read_scope(cred),
                || self.now_ms(),
                || self.network_read(cred),
            )
            .await;
        let superseded = matches!(result, Err(ReadError::Superseded));
        if !superseded && !matches!(path, ReadPath::NegativeCacheHit | ReadPath::Paused) {
            self.record_usage(cred, result.as_ref().err().map(ReadError::cause));
        }
        let outcome = match (path, &result) {
            (_, Err(ReadError::Superseded)) => "superseded",
            (ReadPath::Network, Ok(_)) => "ok",
            (ReadPath::Network, Err(_)) => "failed",
            (ReadPath::Joined, _) => "joined",
            (ReadPath::NegativeCacheHit, _) => "negative_cache_hit",
            (ReadPath::Paused, _) => "paused",
        };
        let now_ms = self.now_ms();
        let (kind, status, retry_in_s) = match &result {
            Ok(_) => ("none", String::new(), 0),
            Err(ReadError::Transient {
                failure,
                retry_at_ms,
            }) => (
                failure.kind,
                failure.status.map(|s| s.to_string()).unwrap_or_default(),
                retry_at_ms.saturating_sub(now_ms) / 1000,
            ),
            Err(read @ (ReadError::Other { .. } | ReadError::Superseded)) => {
                let cause = read.cause();
                (
                    cause.kind.as_str(),
                    cause.http_status.map(|s| s.to_string()).unwrap_or_default(),
                    0,
                )
            }
        };
        tracing::info!(
            target: USAGE_READ_TARGET,
            broker = self.instance,
            account = %cred.account.label_str(),
            outcome = %outcome,
            kind = %kind,
            status = %status,
            retry_in_s,
            "usage read"
        );
        result
    }

    /// One network usage read for `cred`, reporting the seat whose token
    /// made the request. Token resolution happens HERE, behind the boundary,
    /// for exactly this credential; the usage helper pairs any account
    /// header with the same token. Only [`Self::shared_read`] calls it.
    async fn network_read(&self, cred: &CredentialRef) -> super::usage_reads::Fetched {
        use super::usage::{fetch_usage, supports_usage, UsageClient, UsageError, UsageFetchOptions};
        use super::usage_reads::Fetched;
        if !supports_usage(cred.provider) {
            return Fetched::Usage(UsageError::UnsupportedProvider {
                provider: cred.provider.as_str().to_string(),
            });
        }
        if let Err(e) = self.check_slot_exists(cred) {
            return UsageReadError::slot(e).into_fetched();
        }
        // The hold is checked again, and a refusal recorded, under the
        // slot's refresh gate ([`SlotHold`]).
        let memory: Arc<dyn super::token::RefusalMemory> = self.slot_hold(cred);
        let creds = match super::token::ensure_fresh_credential_via(
            &self.http,
            cred,
            &self.refresh_endpoints,
            Some(memory),
        )
        .await
        {
            Ok(creds) => creds,
            Err(f) => return UsageReadError::refresh(f).into_fetched(),
        };
        let seat = SeatIdentity::of(cred.provider, &creds);
        let client = match UsageClient::new() {
            Ok(client) => client,
            Err(e) => return Fetched::Usage(e),
        };
        let opts = UsageFetchOptions {
            endpoint_override: self.usage_endpoint_override.clone(),
            ..UsageFetchOptions::default()
        };
        // p1-r1 B4: the pause is checked again at the dispatch, after the
        // token refresh above: a usage 429 on another slot while this read
        // waited for its token sends nothing here.
        if let Some((failure, retry_at_ms)) = self
            .usage_reads
            .paused_at(&self.read_scope(cred), self.now_ms())
        {
            return Fetched::Paused {
                failure,
                retry_at_ms,
            };
        }
        match fetch_usage(
            &client,
            cred.provider,
            cred.account.label_str(),
            &creds.access,
            &opts,
        )
        .await
        {
            Ok(snapshot) => Fetched::Ok(snapshot, seat),
            Err(e) => Fetched::Usage(e),
        }
    }

    /// Capacity view for one account from a read-only usage snapshot no
    /// older than `max_age`,
    /// with the seat it was read from. Anything that is not a well-formed,
    /// fresh reading is NOT capacity; a failed read keeps its typed cause. A
    /// read that failed transiently also reports that failure (and when
    /// reads resume) and, when the slot has one read from the account stored
    /// there now, its last good reading for the policy's grace, paired with
    /// that stored seat.
    async fn capacity_for(&self, cred: &CredentialRef, max_age: Duration) -> CandidateRead {
        use super::quota_policy::AccountCapacity;
        use super::usage_reads::ReadError;
        match self.fresh_usage(cred, max_age).await {
            Ok((snapshot, seat, epoch)) => CandidateRead {
                capacity: capacity_from_snapshot(cred, &snapshot),
                seat: Some(seat),
                transient: None,
                grace: None,
                epoch: Some(epoch),
            },
            Err(read_err) => {
                let err = UsageReadError::from_read(&read_err);
                if err.failure.kind.needs_action() {
                    // Only the user can fix this, outside synaps; the
                    // selection failure names just the kind, so say here
                    // what and where.
                    tracing::warn!(
                        credential = %cred,
                        kind = %err.failure.kind,
                        detail = %action_detail(cred, &err),
                        "usage read needs action"
                    );
                } else {
                    tracing::debug!(credential = %cred, cause = %err.failure, "usage read failed");
                }
                let capacity = AccountCapacity::failed(cred.clone(), err.failure);
                let ReadError::Transient {
                    failure,
                    retry_at_ms,
                } = read_err
                else {
                    return CandidateRead {
                        capacity,
                        seat: None,
                        transient: None,
                        grace: None,
                        epoch: None,
                    };
                };
                // Grace only on a reading of the account in the slot now: a
                // re-login to another account id never inherits it. The
                // slot's epoch is taken before the last good reading, so a
                // drop after this ends the grace for the selection too.
                let read_key = self.read_key(cred);
                let epoch = self.usage_reads.epoch(&read_key);
                let graced = self.stored_seat(cred).ok().and_then(|seat| {
                    let good = self.usage_reads.last_good_for(&read_key, &seat)?;
                    Some((good, seat))
                });
                let (grace, seat, epoch) = match graced {
                    Some((good, seat)) => (
                        Some(capacity_from_snapshot(cred, &good)),
                        Some(seat),
                        Some(epoch),
                    ),
                    None => (None, None, None),
                };
                CandidateRead {
                    capacity,
                    seat,
                    transient: Some((failure, retry_at_ms)),
                    grace,
                    epoch,
                }
            }
        }
    }

    /// Ends this selection's copies of readings dropped since the selection
    /// took them (p1-r3 B1). Each read in `reads` (a reading, or the last
    /// good one offered to grace) carries the slot's epoch it was taken at;
    /// when that epoch has moved (a reported limit, a refused login), the
    /// account counts as failed for the rest of the selection exactly as a
    /// [`ReadError::Superseded`](super::usage_reads::ReadError::Superseded)
    /// read: no grace, no hold, no cooldown, and not read again in this
    /// selection (a re-read queued for it is dropped unsent). True when a
    /// copy ended.
    fn revalidate(&self, reads: &mut SelectionReads, failed: &mut FailedThisSelection) -> bool {
        let ended: Vec<String> = reads
            .reads
            .iter()
            .filter(|(key, read)| !failed.contains_key(*key) && self.is_superseded(key, read))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &ended {
            self.end_copy(key, reads, failed);
        }
        !ended.is_empty()
    }

    /// [`Self::revalidate`] for `key` alone, right before the selection
    /// reuses its read or takes the re-read queued for it (p1-r4 B1): a
    /// drop since the copy was taken ends it there, so neither a reuse nor
    /// a re-read can replace the only evidence of the drop. True when the
    /// copy ended (the account is then failed for this selection).
    fn revalidate_key(
        &self,
        key: &str,
        reads: &mut SelectionReads,
        failed: &mut FailedThisSelection,
    ) -> bool {
        let ended = !failed.contains_key(key)
            && reads
                .reads
                .get(key)
                .is_some_and(|read| self.is_superseded(key, read));
        if ended {
            self.end_copy(key, reads, failed);
        }
        ended
    }

    /// Whether `read`, this selection's copy of `key`'s evidence, was taken
    /// at an epoch of the slot that has moved since.
    fn is_superseded(&self, key: &str, read: &CandidateRead) -> bool {
        read.epoch.is_some_and(|epoch| {
            CredentialRef::parse_storage_key(key)
                .is_some_and(|cred| self.usage_reads.epoch(&self.read_key(&cred)) != epoch)
        })
    }

    /// End this selection's copy of `key`'s evidence: the account counts as
    /// failed with a superseded read's cause for the rest of the selection,
    /// and any re-read queued for it is dropped unsent.
    fn end_copy(&self, key: &str, reads: &mut SelectionReads, failed: &mut FailedThisSelection) {
        use super::usage_reads::{ReadError, USAGE_READ_TARGET};
        reads.forget(key);
        failed.insert(key.to_string(), ReadError::Superseded.cause());
        let account = CredentialRef::parse_storage_key(key)
            .map(|cred| cred.account.label_str().to_string())
            .unwrap_or_default();
        tracing::debug!(
            target: USAGE_READ_TARGET,
            broker = self.instance,
            account = %account,
            "selection evidence superseded"
        );
    }

    /// Preview the ranking without vending inference credentials or
    /// changing stickiness/cooldowns. This is one ranking pass on the
    /// readings a selection starts from: a vend may then read again the
    /// account it would choose (its reading being over `max_snapshot_age`),
    /// or, before it reports no capacity, the accounts rejected on such
    /// readings, so its final choice can differ from this preview. Usage
    /// reads may refresh expired OAuth tokens through the ordinary
    /// rotation-safe credential path.
    pub async fn plan(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<super::SelectionPlan, BrokerError> {
        self.selection_plan(
            provider,
            model,
            &[],
            &mut FailedThisSelection::new(),
            &mut SelectionReads::new(self.now_ms()),
        )
        .await
        .map(|planned| planned.plan)
    }

    /// Read every account's capacity and rank them; an account whose
    /// cooldown is active is not read. `exclude` lists the
    /// accounts the request already used. An account in `failed` failed
    /// earlier in this selection: it counts as failed with that kind and is
    /// not read again; a read that fails now (other than a held transient
    /// read, which the usage-read table answers without a request) is added.
    /// `reads` holds this selection's reads: an account read in an earlier
    /// ranking pass of the same selection is not read again, except for the
    /// one re-read under `max_snapshot_age` that `reads` asks for.
    /// A cached reading is reused up to its account's max age: the pinned
    /// age for the account this provider/model is pinned to (a reading near
    /// the limit is read again past `max_snapshot_age` before it is used:
    /// [`Self::old_choice`]), else `idle_snapshot_age` plus the account's
    /// share of `idle_snapshot_spread` by its place in the listing. A reading
    /// saved before this process started counts, at its original age. The
    /// spread only separates the accounts' due times: a first selection with
    /// no saved readings, or one after every account's max age has passed,
    /// still reads every account (one usage 429 pauses the rest).
    async fn selection_plan(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        exclude: &[CredentialRef],
        failed: &mut FailedThisSelection,
        reads: &mut SelectionReads,
    ) -> Result<PlannedSelection, BrokerError> {
        use super::quota_policy::{AccountCapacity, SelectionRequest};
        let listed = storage::list_accounts(provider).map_err(BrokerError::Credential)?;
        self.keep_listed_readings(provider, &listed);
        let summaries = reads.keep_inventory(listed);
        let mut candidates = Vec::with_capacity(summaries.len());
        let mut seats = BTreeMap::new();
        let mut transient = BTreeMap::new();
        let mut grace = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut exclude = exclude.to_vec();
        let previous = self
            .current_accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::current_key(provider, model))
            .cloned();
        let pinned = previous.as_ref().map(|(cred, _)| cred.storage_key());
        // Each account's cooldown keys, worked out once for both lookups
        // below (and only while some cooldown is active: they read the
        // stored login).
        let cooling_any = self.any_cooldown_active(self.now_ms());
        let mut keys = BTreeMap::new();
        // First this pass's evidence: read (or reused from an earlier pass)
        // for each account not already failed in this selection. `None`
        // marks an account that failed earlier in this selection.
        let mut gathered = Vec::with_capacity(summaries.len());
        for (index, summary) in summaries.iter().enumerate() {
            let Some(cred) = summary.credential_ref() else {
                continue;
            };
            let key = cred.storage_key();
            if cooling_any {
                keys.insert(key.clone(), self.cooldown_keys(&cred));
            }
            // Failed earlier in this selection, or its copy of the account's
            // evidence predates a drop: checked right before the copy is
            // reused or a re-read queued for it is taken (p1-r4 B1), so the
            // account is not read again in this selection.
            if failed.contains_key(&key) || self.revalidate_key(&key, reads, failed) {
                gathered.push((cred, None));
                continue;
            }
            // An account whose cooldown is active (the whole account's, or
            // a limit of this request's model) can't be chosen, so its
            // usage is not read (no usage request, no token refresh). It
            // keeps its stored seat, for stickiness and the duplicate-seat
            // check; the policy rejects it as cooling down.
            let now_ms = self.now_ms();
            let read = if keys
                .get(&key)
                .and_then(|k| self.cooldown_for_keys(k, |c| c.for_request_at(model, now_ms)))
                .is_some()
            {
                CandidateRead {
                    capacity: AccountCapacity::unobserved(cred.clone()),
                    seat: self.stored_seat(&cred).ok(),
                    transient: None,
                    grace: None,
                    epoch: None,
                }
            } else if let Some(read) = reads.reuse(&key) {
                // Read earlier in this selection: not read again.
                read
            } else {
                let max_age = if reads.take_reread(&key) {
                    self.max_snapshot_age
                } else {
                    self.reuse_age(&cred, pinned.as_deref(), index, summaries.len())
                };
                // A re-read never replaces a copy taken at an older epoch:
                // a drop between the check above and the re-read's own
                // epoch still fails the account for this selection.
                let replaced = reads.epoch_of(&key);
                let read = self.capacity_for(&cred, max_age).await;
                if replaced.is_some_and(|old| read.epoch.is_some_and(|new| new != old)) {
                    self.end_copy(&key, reads, failed);
                    gathered.push((cred, None));
                    continue;
                }
                reads.record(key, &read);
                read
            };
            gathered.push((cred, Some(read)));
        }
        // The reads above awaited: a reading dropped meanwhile (a reported
        // limit, a refused login) ends this selection's copy of it too.
        self.revalidate(reads, failed);
        for (cred, read) in gathered {
            let read = match (read, failed.get(&cred.storage_key())) {
                (Some(read), None) => read,
                (_, failure) => {
                    // Failed earlier in this selection (not read), or its
                    // evidence was superseded just now: it counts as failed
                    // with that kind.
                    candidates.extend(
                        failure.map(|failure| AccountCapacity::failed(cred, failure.clone())),
                    );
                    continue;
                }
            };
            let (mut capacity, seat) = (read.capacity, read.seat);
            match (read.transient, &capacity.failure) {
                (Some(held), _) => {
                    transient.insert(cred.storage_key(), held);
                }
                (None, Some(failure)) => {
                    failed.insert(cred.storage_key(), failure.clone());
                }
                (None, None) => {}
            }
            // The same entitlement guard for a fresh reading and for a last
            // good reading standing in for a failed read.
            require_requested_model(provider, model, &mut capacity);
            grace.extend(read.grace.map(|mut good| {
                require_requested_model(provider, model, &mut good);
                good
            }));
            if let Some(seat) = seat {
                if !seen.insert(seat.as_str().to_string()) {
                    exclude.push(cred.clone());
                }
                seats.insert(cred.storage_key(), seat);
            }
            candidates.push(capacity);
        }
        let now_ms = self.now_ms();
        // A cooldown reported while the reads ran counts too.
        if !cooling_any && self.any_cooldown_active(now_ms) {
            for capacity in &candidates {
                let cred = &capacity.credential;
                keys.insert(cred.storage_key(), self.cooldown_keys(cred));
            }
        }
        for capacity in &mut candidates {
            capacity.cooldown_until_ms = keys
                .get(&capacity.credential.storage_key())
                .and_then(|k| self.cooldown_for_keys(k, |c| c.for_request_at(model, now_ms)))
                .map(|c| c.until_ms);
        }
        let current = previous
            .filter(|(cred, seat)| seats.get(&cred.storage_key()) == Some(seat))
            .map(|(cred, _)| cred);
        let policy = self.auto_policy();
        // `auth.order.<provider>` first (labels that exist), then the rest in
        // listing order; the policy is re-read when the config changes.
        let listed: Vec<_> = summaries.iter().map(|s| s.label.clone()).collect();
        let preference = policy.preference_for(provider, &listed);
        let request = SelectionRequest {
            model,
            preference: &preference,
            exclude: &exclude,
            strategy: policy.strategy_for(provider),
            current: current.as_ref(),
            urgent_horizon_ms: policy.urgent_horizon_ms,
            sticky: policy.sticky,
            grace: &grace,
            // Each reading was reused only up to its own account's max age;
            // the policy's bound is the widest of them.
            ..SelectionRequest::new(provider, now_ms, self.widest_reuse_age().as_millis() as u64)
        };
        let resets_ahead = candidates
            .iter()
            .filter(|capacity| waits_for_reset(&request, capacity))
            .map(|capacity| capacity.credential.storage_key())
            .collect();
        Ok(PlannedSelection {
            plan: super::SelectionPlan::from_candidates(&request, &candidates),
            seats,
            transient,
            excluded: exclude.iter().map(CredentialRef::storage_key).collect(),
            resets_ahead,
            pinned,
        })
    }

    /// How old a cached reading of `cred` (at `index` of `listed` in the
    /// provider's listing) may be when this selection reuses it.
    fn reuse_age(
        &self,
        cred: &CredentialRef,
        pinned: Option<&str>,
        index: usize,
        listed: usize,
    ) -> Duration {
        if pinned == Some(cred.storage_key().as_str()) {
            return self.pinned_age_for(cred.provider);
        }
        let share = self.idle_snapshot_spread.mul_f64(index as f64 / listed.max(1) as f64);
        self.idle_snapshot_age.max(self.max_snapshot_age) + share
    }

    /// How old the pinned account's reading may be while it shows room.
    fn pinned_age(&self) -> Duration {
        self.pinned_snapshot_age.max(self.max_snapshot_age)
    }

    /// [`Self::pinned_age`] for an Anthropic account; `max_snapshot_age`
    /// for any other provider's (p1-r1 LOW2: the pinned 10 minutes, the
    /// pause and the saved readings are Anthropic's only).
    fn pinned_age_for(&self, provider: OAuthProviderId) -> Duration {
        if provider == OAuthProviderId::Anthropic {
            self.pinned_age()
        } else {
            self.max_snapshot_age
        }
    }

    /// The oldest any account's reading may be when a selection reuses it.
    fn widest_reuse_age(&self) -> Duration {
        (self.idle_snapshot_age.max(self.max_snapshot_age) + self.idle_snapshot_spread)
            .max(self.pinned_age())
    }

    /// The chosen account's storage key when the plan chose it on a reading
    /// that is old at `now_ms` ([`SelectionReads::is_old`]) and that this
    /// selection has not read again. A graced account (its read failed
    /// transiently and is held) is not read again here. Old means over
    /// `max_snapshot_age` for an account the selection would switch to;
    /// for the account it is pinned to (or the first choice of a selection
    /// pinned to none), over the pinned age while the reading shows room on
    /// every window of the request or shows the account's paid extra usage
    /// off, and over `max_snapshot_age` near the limit
    /// ([`DEFAULT_NEAR_LIMIT_PERCENT`]) otherwise.
    fn old_choice(
        &self,
        planned: &PlannedSelection,
        reads: &SelectionReads,
        model: Option<&str>,
        now_ms: u64,
    ) -> Option<String> {
        let chosen = planned.plan.selection.selected()?;
        let key = chosen.storage_key();
        if planned.transient.contains_key(&key) {
            return None;
        }
        let stays = planned.pinned.as_deref().is_none_or(|pinned| pinned == key);
        let max_age = if stays && (!reads.near_limit(&key, model) || reads.extra_usage_off(&key)) {
            self.pinned_age_for(chosen.provider)
        } else {
            self.max_snapshot_age
        };
        reads.is_old(&key, now_ms, max_age).then_some(key)
    }

    async fn select_auto(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        exclude: &[CredentialRef],
        failed: &mut FailedThisSelection,
    ) -> Result<(CredentialRef, SeatIdentity), BrokerError> {
        use super::quota_policy::{Selection, SelectionFailure, SelectionFailureKind};
        use super::usage_reads::USAGE_READ_TARGET;
        let mut reads = SelectionReads::new(self.now_ms());
        let mut rechecked = false;
        let PlannedSelection {
            plan,
            mut seats,
            transient,
            ..
        } = loop {
            let planned = match self
                .selection_plan(provider, model, exclude, failed, &mut reads)
                .await
            {
                Ok(planned) => planned,
                // The account list itself could not be read (the only error
                // `selection_plan` returns): still exactly one WARN for this
                // failed selection, and the caller gets the store's own error.
                Err(e) => {
                    log_selection_failure(
                        provider,
                        model,
                        &SelectionFailure::new(SelectionFailureKind::CredentialStore),
                    );
                    return Err(e);
                }
            };
            // A reading older than `max_snapshot_age` only ranks an account;
            // it never decides the choice: the chosen account is read again
            // (once per selection) and the ranking redone on this
            // selection's reads (no other account is read again for it).
            // Ages are judged now, after this pass's reads: a cache hit that
            // was young when the pass took it is old by now if another
            // account's read was slow.
            let now_ms = self.now_ms();
            if let Some(key) = self.old_choice(&planned, &reads, model, now_ms) {
                if reads.ask_reread(key) {
                    continue;
                }
            }
            // Before it reports no capacity, the selection reads again, once,
            // each account it rejected on an old reading (taken before the
            // selection began and over `max_snapshot_age` old now) that
            // names no reset still ahead for a window it is used up on (no
            // reset, or one already past when read): one read each at most,
            // never an excluded or failed account (a cooling one is not
            // read, and a held read is answered without a request). An
            // account used up until a reset ahead is not read: it can't have
            // room before that reset, and its reading is read again once the
            // reset has passed. Only a selection in which every account
            // looks unusable gets here.
            if !rechecked && planned.plan.selection.selected().is_none() {
                rechecked = true;
                let old = reads.old_readings(now_ms, self.max_snapshot_age, |key| {
                    planned.excluded.contains(key)
                        || failed.contains_key(key)
                        || planned.resets_ahead.contains(key)
                });
                if !old.is_empty() {
                    for key in old {
                        reads.ask_reread(key);
                    }
                    continue;
                }
            }
            // Nothing above awaited since the plan's last revalidation, but
            // another task can still drop a reading the plan rests on: the
            // selection returns only on evidence still current, else it
            // ranks again without it (each time one more account fails, so
            // this ends).
            if self.revalidate(&mut reads, failed) {
                continue;
            }
            break planned;
        };
        match plan.selection {
            Selection::Selected {
                credential,
                tier,
                budget_reset_ms,
                utilization,
                ..
            } => {
                // Every account whose usage was read has its seat, so a
                // selected account always has one.
                let seat = seats.remove(&credential.storage_key());
                debug_assert!(
                    seat.is_some(),
                    "selected account has no paired seat identity"
                );
                let Some(seat) = seat else {
                    return Err(selection_failed(
                        provider,
                        model,
                        "selected account has no paired seat identity".into(),
                        SelectionFailure::new(SelectionFailureKind::SeatChurn),
                    ));
                };
                // A chosen account whose read failed was chosen on its last
                // good reading (the policy's grace).
                if let Some((failure, retry_at_ms)) = transient.get(&credential.storage_key()) {
                    tracing::info!(
                        target: USAGE_READ_TARGET,
                        broker = self.instance,
                        account = %credential.account.label_str(),
                        outcome = %"grace",
                        kind = %failure.kind,
                        status = %failure.status.map(|s| s.to_string()).unwrap_or_default(),
                        retry_in_s = retry_at_ms.saturating_sub(self.now_ms()) / 1000,
                        "usage read"
                    );
                }
                tracing::info!(%credential, ?plan.strategy, tier, ?budget_reset_ms, ?utilization, "automatic subscription choice");
                Ok((credential, seat))
            }
            Selection::NoCapacity {
                rejections,
                earliest_reset_ms,
            } => {
                let detail: Vec<_> = rejections
                    .iter()
                    .map(|r| format!("{}: {}", r.credential.account, r.describe()))
                    .collect();
                let reset = earliest_reset_ms
                    .map(|t| format!("; earliest reset at {t}"))
                    .unwrap_or_default();
                let mut failure = super::quota_policy::SelectionFailure::from_rejections(
                    &rejections,
                    plan.now_ms,
                );
                // A held usage read goes to the network again at the next
                // selection after its hold ends: say when.
                if failure.kind == SelectionFailureKind::UsageReadFailed {
                    failure.usage_read_retry_ms = rejections
                        .iter()
                        .filter_map(|r| transient.get(&r.credential.storage_key()))
                        .map(|(_, at)| *at)
                        .min();
                }
                let reason = if detail.is_empty() {
                    format!("no {provider} account is logged in")
                } else {
                    format!("{}{reset}", detail.join(", "))
                };
                Err(selection_failed(provider, model, reason, failure))
            }
        }
    }

    /// Test/embedding seam: pin the `local` provider endpoint explicitly.
    pub fn with_local_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            local_base_url: Some(base_url.into()),
            ..Self::new(http)
        }
    }

    /// Test seam: point the pinned Google Code Assist host at a loopback fake.
    /// Production code must never call this — it only relaxes the base URL,
    /// not any of the path allowlist / bearer / redirect-denial invariants.
    #[doc(hidden)]
    pub fn with_google_gemini_base_url_for_tests(
        http: reqwest::Client,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            google_gemini_base_url: Some(base_url.into().trim_end_matches('/').to_string()),
            ..Self::new(http)
        }
    }

    /// Test seam: point the pinned Anthropic usage operation at a fake server.
    #[doc(hidden)]
    pub fn with_anthropic_usage_url(mut self, url: impl Into<String>) -> Self {
        self.anthropic_usage_url = Some(url.into());
        self
    }

    /// Test seam: shrink the buffered-request time budget.
    #[doc(hidden)]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Test seam: shrink the buffered-response size cap.
    #[doc(hidden)]
    pub fn with_max_response_bytes(mut self, cap: usize) -> Self {
        self.max_response_bytes = cap;
        self
    }

    /// Resolve (and migrate) a static provider key. Broker-owned storage wins;
    /// login config and env are legacy discovery surfaces that are migrated
    /// into broker storage on first use.
    fn resolve_static_key(&self, provider: &str) -> Result<String, BrokerError> {
        if provider == LOCAL_PROVIDER_KEY {
            return Ok(self.resolve_local_key());
        }
        let spec = static_provider(provider)
            .ok_or_else(|| BrokerError::UnknownProvider(provider.to_string()))?;
        if let Ok(Some(key)) = storage::load_static_key(provider) {
            return Ok(key);
        }
        if let Some(key) = discover_legacy_static_key(spec) {
            // Migration: persist into broker-owned storage. Best-effort — a
            // read-only home dir must not break the request itself.
            if let Err(e) = storage::save_static_key(provider, &key) {
                tracing::warn!(provider, "static key migration failed: {e}");
            }
            return Ok(key);
        }
        Err(BrokerError::NotConfigured(provider.to_string()))
    }

    fn resolve_local_key(&self) -> String {
        storage::load_static_key(LOCAL_PROVIDER_KEY)
            .ok()
            .flatten()
            .or_else(|| {
                crate::config::get_provider_keys()
                    .get(LOCAL_PROVIDER_KEY)
                    .filter(|s| !s.is_empty())
                    .cloned()
            })
            .or_else(|| {
                std::env::var("LOCAL_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "local".to_string())
    }

    fn base_url_for(&self, provider: &str) -> Result<String, BrokerError> {
        if provider == LOCAL_PROVIDER_KEY {
            if let Some(url) = &self.local_base_url {
                return Ok(url.trim_end_matches('/').to_string());
            }
            return Ok(local_endpoint_url());
        }
        static_provider(provider)
            .map(|s| s.base_url.trim_end_matches('/').to_string())
            .ok_or_else(|| BrokerError::UnknownProvider(provider.to_string()))
    }

    async fn send(&self, request: &ProxyRequest) -> Result<reqwest::Response, BrokerError> {
        request.validate()?;
        let (provider_key, explicit_account) = request.split_provider()?;
        let provider_key = provider_key.as_str();
        // OAuth-proxied providers: resolve ONE credential (explicit account
        // or policy) and derive both the bearer and any account-specific
        // header from that same credential.
        let oauth_provider: Option<OAuthProviderId> = provider_key.parse().ok();
        let mut pinned: Option<PinnedToken> = None;
        // Muse honours the official CLI's `META_API_KEY` override: a Model
        // API key from the environment wins over the account login (only for
        // policy-selected requests — an explicit account stays explicit).
        let muse_env_key = (provider_key == "muse" && explicit_account.is_none())
            .then(super::muse::api_key_from_env)
            .flatten();
        if let Some(provider) = oauth_provider.filter(|_| muse_env_key.is_none()) {
            if provider_key != LOCAL_PROVIDER_KEY && static_provider(provider_key).is_none() {
                pinned = Some(
                    self.resolve_and_vend(provider, explicit_account, None, &[])
                        .await?,
                );
            }
        }
        let bearer = |pinned: &Option<PinnedToken>| -> Result<String, BrokerError> {
            pinned
                .as_ref()
                .map(|p| p.token.token.clone())
                .ok_or_else(|| BrokerError::UnknownProvider(request.provider.clone()))
        };
        let (key, base) = if provider_key == "xai-auth" {
            (bearer(&pinned)?, "https://api.x.ai/v1".to_string())
        } else if provider_key == "github-copilot" {
            // Catalog-only OAuth proxy: short-lived Copilot session token only.
            // Never attach the GitHub user token (stored as OAuth refresh).
            (
                bearer(&pinned)?,
                super::github_copilot_models_base_url().to_string(),
            )
        } else if provider_key == "google-gemini" {
            // Google Gemini (Code Assist) is broker-proxy-only. Refresh stays
            // broker-owned; runtime never receives it.
            let base = self
                .google_gemini_base_url
                .clone()
                .unwrap_or_else(|| GOOGLE_GEMINI_CODE_ASSIST_BASE_URL.to_string());
            (bearer(&pinned)?, base)
        } else if provider_key == "openai-codex" {
            // Catalog-only OAuth proxy for ChatGPT backend models. Access token
            // never leaves the broker; account header is derived broker-side.
            let base = self
                .openai_codex_base_url
                .clone()
                .unwrap_or_else(|| OPENAI_CODEX_BACKEND_BASE_URL.to_string());
            (bearer(&pinned)?, base)
        } else if provider_key == "kimi-code" {
            // Managed Kimi Code OAuth proxy: short-lived (~15 min) access
            // token resolved broker-side; the rotating refresh token never
            // leaves the boundary. Base is pinned to the managed endpoint.
            (bearer(&pinned)?, super::kimi_code::API_BASE_URL.to_string())
        } else if provider_key == "muse" {
            // Meta Muse: the bearer is a Model API key (env override, or the
            // key minted from the broker-owned Meta OAuth login). The OAuth
            // token itself never leaves the boundary. Origin is pinned; the
            // allowlist pins the paths.
            let key = match &muse_env_key {
                Some(key) => key.clone(),
                None => bearer(&pinned)?,
            };
            let base = self
                .muse_base_url
                .clone()
                .unwrap_or_else(|| super::muse::API_ORIGIN.to_string());
            (key, base)
        } else if provider_key == "anthropic" {
            // Catalog-only OAuth proxy for Anthropic /v1/models pagination.
            // Keeps the access token broker-owned (no runtime token vending).
            (bearer(&pinned)?, "https://api.anthropic.com".to_string())
        } else {
            (
                self.resolve_static_key(provider_key)?,
                self.base_url_for(provider_key)?,
            )
        };
        let response = self
            .dispatch(request, provider_key, &key, &base, &pinned)
            .await?;
        // Muse mirrors the official CLI: a 401 on a minted key re-mints ONCE
        // from the stored OAuth login (under the credential's refresh locks;
        // a peer's fresh key is adopted instead) and the request is retried
        // once. An env-supplied key is never re-minted.
        //
        // If the re-mint is refused (forced-refresh cooldown) or the mint
        // rejects the stored login, the caller gets the ORIGINAL upstream 401
        // — status and body untouched, exactly like the env-key path — not a
        // broker error (the authenticator-gives-up convention). A transient,
        // lock-busy or local failure is a retryable `BrokerError::Refresh`
        // instead. Only the status has been read, so `response` is still
        // unconsumed here. This covers `proxy` and `proxy_stream`, which both
        // go through `send`.
        if provider_key == "muse" && response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(rejected) = pinned.as_ref() {
                tracing::info!(credential = %rejected.credential, "muse key rejected (401); re-minting once");
                match self.recover_rejected_token(&rejected.credential, &key).await {
                    Ok(fresh) => {
                        // The rejected request was sent: a failure from here
                        // on (the bearer, the retry) fails after a dispatch,
                        // so it is never a not-sent error.
                        let after_dispatch = |e: BrokerError| {
                            BrokerError::Transport(format!("after a sent request: {e}"))
                        };
                        let pinned = Some(PinnedToken {
                            credential: rejected.credential.clone(),
                            token: fresh,
                        });
                        let key = bearer(&pinned).map_err(after_dispatch)?;
                        return self
                            .dispatch(request, provider_key, &key, &base, &pinned)
                            .await
                            .map_err(after_dispatch);
                    }
                    // A transient mint failure, a busy refresh lock, a local
                    // failure or something the user must fix elsewhere (a
                    // corrupt store) says nothing about the login: surface
                    // the refresh error, not the upstream 401 (which the
                    // runtime shows as "sign in again").
                    Err(BrokerError::Refresh(f))
                        if f.kind.is_retryable()
                            || f.kind == super::token::RefreshFailureKind::ActionRequired =>
                    {
                        tracing::warn!(
                            credential = %rejected.credential,
                            reason = %f,
                            "muse key re-mint failed; returning a retryable error"
                        );
                        return Err(BrokerError::Refresh(f));
                    }
                    Err(e) => {
                        // Token-free: recovery errors carry sanitized reasons
                        // (cooldown, MuseAuthError labels, lock timeouts).
                        tracing::warn!(
                            credential = %rejected.credential,
                            reason = %e,
                            "muse key re-mint refused or rejected; returning the upstream 401"
                        );
                    }
                }
            }
        }
        Ok(response)
    }

    /// Build and send one upstream request with an already-resolved bearer.
    async fn dispatch(
        &self,
        request: &ProxyRequest,
        provider_key: &str,
        key: &str,
        base: &str,
        pinned: &Option<PinnedToken>,
    ) -> Result<reqwest::Response, BrokerError> {
        let url = format!("{base}{}", request.path);
        // Deny redirects for credential-bearing OAuth catalog traffic and
        // google-gemini: a 3xx must not replay the bearer token off-origin.
        let pinned_no_redirect = provider_key == "google-gemini"
            || provider_key == "openai-codex"
            || provider_key == "anthropic"
            || provider_key == "kimi-code"
            || provider_key == "muse";
        // A refused connection proves nothing was sent only when no redirect
        // can have been followed first (an earlier hop may have received the
        // request).
        let redirect_free = pinned_no_redirect || self.redirect_free;
        let mut builder = if pinned_no_redirect {
            // `read_timeout` is an IDLE timer (reset on every read), so a
            // long SSE stream that keeps sending is never cut; a stalled one
            // (silent NAT drop, hung edge) fails instead of hanging the turn.
            let no_redirect = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(self.stream_idle_timeout_for(provider_key))
                .build()
                .map_err(|e| BrokerError::Transport(format!("client build failed: {e}")))?;
            match request.method {
                ProxyMethod::Get => no_redirect.get(&url),
                ProxyMethod::Post => no_redirect.post(&url),
            }
        } else {
            match request.method {
                ProxyMethod::Get => self.http.get(&url),
                ProxyMethod::Post => self.http.post(&url),
            }
        };
        builder = builder.bearer_auth(key);
        if provider_key == "openai-codex" {
            // ChatGPT backend requires the account id. Derive it from the SAME
            // credential that produced the bearer: the JWT claim first, then
            // that slot's stored `accountId`. Never another slot, never omitted.
            let cred = pinned
                .as_ref()
                .map(|p| p.credential.clone())
                .unwrap_or_else(|| CredentialRef::default_for(OAuthProviderId::OpenAiCodex));
            let account_id = super::extract_codex_account_id(key)
                .or_else(|| {
                    storage::load_credential(&cred)
                        .ok()
                        .flatten()
                        .and_then(|c| c.account_id)
                        .filter(|s| !s.trim().is_empty())
                })
                .ok_or_else(|| {
                    BrokerError::Credential(
                        "openai-codex credential is missing chatgpt account id".into(),
                    )
                })?;
            builder = builder
                .header("chatgpt-account-id", account_id)
                .header("originator", "synaps")
                .header("OpenAI-Beta", "responses=experimental")
                .header("accept", "application/json");
        }
        if provider_key == "anthropic" {
            // OAuth catalog: Bearer + anthropic-version + anthropic-beta.
            // Do NOT send the OAuth token as x-api-key.
            for (name, value) in anthropic_oauth_catalog_request_headers() {
                builder = builder.header(*name, *value);
            }
        }
        if provider_key == "github-copilot" {
            for (name, value) in super::github_copilot_models_request_headers() {
                builder = builder.header(*name, *value);
            }
            if request.path != "/models" {
                builder = builder
                    .header("Openai-Intent", "conversation-edits")
                    .header("X-Initiator", "agent");
            }
        }
        if provider_key == "google-gemini" {
            // The Code Assist reference client uses `?alt=sse` for streaming.
            // Match that so upstream returns line-delimited SSE frames rather
            // than JSON-in-one-response.
            if request.stream && request.path == "/v1internal:streamGenerateContent" {
                builder = builder.query(&[("alt", "sse")]);
            }
            builder = builder.header("user-agent", "SynapsCLI/0.6.0 (google-gemini)");
        }
        if provider_key == "muse" {
            // API-key requests need only Authorization (verified); identify
            // Synaps honestly instead of mimicking the official client.
            builder = builder.header("user-agent", super::muse::USER_AGENT);
        }
        if provider_key == "kimi-code" {
            // Conventional Kimi device-identity surface (mirrors the official
            // CLI's `X-Msh-*` headers). Values are non-secret; the User-Agent
            // identifies Synaps honestly.
            let device_id = super::kimi_code::load_or_create_device_id();
            for (name, value) in super::kimi_code::identity_request_headers(&device_id) {
                builder = builder.header(name, value);
            }
        }
        if let Some(bytes) = &request.body_bytes {
            // Exact-byte handoff (request-trace spec §6.2): send the very
            // buffer the caller serialized (and digested) — never a
            // re-serialization of the JSON value.
            builder = builder
                .header("content-type", "application/json")
                .body(bytes.clone());
        } else if let Some(body) = &request.body {
            builder = builder.json(body);
        }
        if request.stream {
            builder = builder.header("accept", "text/event-stream");
        } else {
            // Buffered requests get an explicit total time budget; streaming
            // responses are open-ended by design (SSE) and rely on the
            // client's connect timeout plus consumer-side backpressure.
            builder = builder.timeout(self.request_timeout);
        }
        builder.send().await.map_err(|e| {
            let connect_failure = |message: String| {
                if redirect_free {
                    BrokerError::ConnectFailed(message)
                } else {
                    BrokerError::Transport(message)
                }
            };
            if e.is_connect() && provider_key == LOCAL_PROVIDER_KEY {
                connect_failure(format!(
                    "can't reach local endpoint at {url} — is Ollama/LM Studio running?"
                ))
            } else if e.is_connect() {
                // reqwest errors do not include the bearer header.
                connect_failure(format!("request to {} failed: {e}", request.provider))
            } else {
                // reqwest errors do not include the bearer header.
                BrokerError::Transport(format!("request to {} failed: {e}", request.provider))
            }
        })
    }
}

// ── Usage → capacity mapping (fail closed) ───────────────────────────────────

/// Map a typed usage snapshot onto the pure capacity policy's input. Only
/// account-wide and model-scoped quota windows constrain inference;
/// feature-scoped windows (e.g. code review) are not inference limits.
pub fn capacity_from_snapshot(
    cred: &CredentialRef,
    snapshot: &super::usage::UsageSnapshot,
) -> super::quota_policy::AccountCapacity {
    use super::quota_policy::{
        AccountCapacity, ModelAvailability, ModelState, QuotaObservation, WindowLimit,
    };
    use super::usage::{Availability, WindowScope};
    let mut windows: Vec<WindowLimit> = snapshot
        .windows
        .iter()
        .filter_map(|w| {
            let models = match &w.scope {
                WindowScope::Account => None,
                WindowScope::Model { model } => Some(vec![model.clone()]),
                WindowScope::Feature { .. } => return None,
            };
            Some(WindowLimit {
                id: w.id.clone(),
                duration_ms: w.duration_secs.map(|s| s.saturating_mul(1000)),
                used_percent: w.used_percent.valid(),
                limit_reached: w.limit_reached,
                resets_at_ms: w.reset_at,
                models,
            })
        })
        .collect();
    if snapshot.limit_reached == Some(true) {
        // Provider-asserted overall exhaustion applies to every model.
        windows.push(WindowLimit {
            id: "limit_reached".into(),
            duration_ms: None,
            used_percent: None,
            limit_reached: Some(true),
            resets_at_ms: snapshot.earliest_reset_at(),
            models: None,
        });
    }
    let models = if snapshot.model_availability.is_empty() {
        None
    } else {
        Some(
            snapshot
                .model_availability
                .iter()
                .map(|m| ModelAvailability {
                    model: m.model.clone(),
                    state: match m.availability {
                        Availability::Available => ModelState::Available,
                        Availability::Exhausted => ModelState::Exhausted,
                        Availability::Unknown => ModelState::Unknown,
                    },
                })
                .collect(),
        )
    };
    AccountCapacity {
        credential: cred.clone(),
        observed_at_ms: Some(snapshot.observed_at),
        observation: QuotaObservation::Ok { windows, models },
        cooldown_until_ms: None,
        failure: None,
        extra_usage_enabled: snapshot.extra_usage_enabled,
    }
}

/// A failed usage read: the broker error `usage()` callers see (unchanged
/// vocabulary) plus the typed cause the selection report keeps.
#[derive(Debug)]
struct UsageReadError {
    error: BrokerError,
    failure: super::quota_policy::ObservationFailure,
}

/// One automatic selection's ranking and what the broker needs to act on it.
struct PlannedSelection {
    plan: super::SelectionPlan,
    /// The seat each read account's reading belongs to, by storage key.
    seats: BTreeMap<String, SeatIdentity>,
    /// Accounts whose read failed transiently: the failure and when reads
    /// resume, by storage key.
    transient: BTreeMap<String, (super::usage_reads::TransientReadFailure, u64)>,
    /// Storage keys the selection may not choose: the accounts the request
    /// already used, and a second login to a seat listed earlier.
    excluded: std::collections::BTreeSet<String>,
    /// Storage keys of the accounts used up, on a window that applies to
    /// the request, until a reset still ahead ([`waits_for_reset`]).
    resets_ahead: std::collections::BTreeSet<String>,
    /// Storage key of the account the selection is pinned to, if any.
    pinned: Option<String>,
}

/// Whether `capacity`'s reading has a window that applies to `request`,
/// used up until a reset after `request.now_ms`. No reading can show room
/// on that window before the reset, and once the reset has passed the
/// cached reading is out of date and read again
/// ([`UsageSnapshot::reset_since_observed`](super::usage::UsageSnapshot::reset_since_observed)).
fn waits_for_reset(
    request: &super::quota_policy::SelectionRequest<'_>,
    capacity: &super::quota_policy::AccountCapacity,
) -> bool {
    use super::quota_policy::{QuotaObservation, WindowVerdict};
    let QuotaObservation::Ok { windows, .. } = &capacity.observation else {
        return false;
    };
    windows
        .iter()
        .filter(|w| w.applies_to(request.model))
        .any(|w| {
            matches!(
                w.verdict(request.exhausted_at_percent),
                WindowVerdict::Exhausted { resets_at_ms: Some(at) } if at > request.now_ms
            )
        })
}

/// Whether `capacity`'s reading has a window that applies to the request
/// (`model`; `None`: every window) at [`DEFAULT_NEAR_LIMIT_PERCENT`] or more.
fn near_its_limit(capacity: &super::quota_policy::AccountCapacity, model: Option<&str>) -> bool {
    use super::quota_policy::{QuotaObservation, WindowVerdict};
    let QuotaObservation::Ok { windows, .. } = &capacity.observation else {
        return false;
    };
    windows.iter().filter(|w| w.applies_to(model)).any(|w| {
        matches!(
            w.verdict(DEFAULT_NEAR_LIMIT_PERCENT),
            WindowVerdict::Exhausted { .. }
        )
    })
}

/// One automatic selection's usage reads, by storage key. Each account is
/// read at most once per selection: a later ranking pass of the same
/// selection reuses its read, except for one re-read under
/// `max_snapshot_age` of an account whose reading is old
/// ([`SelectionReads::is_old`]) when a pass would rest a decision on it.
/// The selection keeps to the accounts of its first listing.
struct SelectionReads {
    /// When the selection began, on the broker's clock: a reading observed
    /// since then was taken in this selection.
    started_ms: u64,
    /// The storage keys of the selection's first listing.
    inventory: Option<std::collections::BTreeSet<String>>,
    reads: BTreeMap<String, CandidateRead>,
    /// Accounts to read again under `max_snapshot_age` at the next pass.
    pending: std::collections::BTreeSet<String>,
    /// Accounts read again under `max_snapshot_age` in this selection.
    reread: std::collections::BTreeSet<String>,
}

impl SelectionReads {
    fn new(started_ms: u64) -> Self {
        Self {
            started_ms,
            inventory: None,
            reads: BTreeMap::new(),
            pending: Default::default(),
            reread: Default::default(),
        }
    }

    /// The accounts of `listed` this selection considers: the first listing
    /// sets them, and a later pass keeps to them, so an account added while
    /// the selection runs waits for the next selection (one removed meanwhile
    /// drops out). With a fixed set of accounts the passes are bounded: each
    /// account is read again at most once, and the check before no capacity
    /// runs once. An entry that names no account is kept (the plan skips
    /// it). The set is kept by storage key, not by position: the places and
    /// the count that spread the reuse ages ([`LocalBroker::reuse_age`]) are
    /// those of the kept entries. They match the first listing only while
    /// nothing in it is removed: a removal moves the later entries up one
    /// place and lowers the count, and an entry that names no account is kept
    /// whether or not the first listing had it.
    fn keep_inventory(&mut self, mut listed: Vec<AccountSummary>) -> Vec<AccountSummary> {
        let key = |s: &AccountSummary| s.credential_ref().map(|c| c.storage_key());
        match &self.inventory {
            Some(keys) => listed.retain(|s| match key(s) {
                Some(k) => keys.contains(&k),
                None => true,
            }),
            None => self.inventory = Some(listed.iter().filter_map(key).collect()),
        }
        listed
    }

    /// This selection's read of `key`, unless its re-read is due.
    fn reuse(&self, key: &str) -> Option<CandidateRead> {
        if self.pending.contains(key) {
            return None;
        }
        self.reads.get(key).cloned()
    }

    /// Whether `key`'s re-read is due now; it then counts as re-read.
    fn take_reread(&mut self, key: &str) -> bool {
        let due = self.pending.remove(key);
        if due {
            self.reread.insert(key.to_string());
        }
        due
    }

    /// Keep `read` of `key`.
    fn record(&mut self, key: String, read: &CandidateRead) {
        self.reads.insert(key, read.clone());
    }

    /// The slot epoch of this selection's copy of `key`'s evidence, if any.
    fn epoch_of(&self, key: &str) -> Option<u64> {
        self.reads.get(key).and_then(|read| read.epoch)
    }

    /// Forget `key`'s read and any re-read queued for it: its evidence was
    /// superseded, and the account is failed for the rest of the selection.
    fn forget(&mut self, key: &str) {
        self.reads.remove(key);
        self.pending.remove(key);
    }

    /// Whether a decision on `key` at `now_ms` would rest on an old reading:
    /// a good reading (not a failed or held read) observed before this
    /// selection began and over `max_age` old now, that this selection has
    /// not read again. Judged when the decision is made, not when the pass
    /// took the reading, since another account's slow read can age it. A
    /// reading taken in this selection is never old here.
    fn is_old(&self, key: &str, now_ms: u64, max_age: Duration) -> bool {
        let max_age_ms = max_age.as_millis() as u64;
        !self.reread.contains(key)
            && self.reads.get(key).is_some_and(|read| {
                read.transient.is_none()
                    && read.capacity.failure.is_none()
                    && read.capacity.observed_at_ms.is_some_and(|at| {
                        at < self.started_ms && now_ms.saturating_sub(at) > max_age_ms
                    })
            })
    }

    /// Ask for `key`'s one re-read at the next pass; false when it already
    /// had it or is waiting for it.
    fn ask_reread(&mut self, key: String) -> bool {
        !self.reread.contains(&key) && self.pending.insert(key)
    }

    /// Whether `key`'s reading in this selection shows the account's paid
    /// extra usage switched off (unknown counts as on).
    fn extra_usage_off(&self, key: &str) -> bool {
        self.reads
            .get(key)
            .is_some_and(|read| read.capacity.extra_usage_enabled == Some(false))
    }

    /// Whether `key`'s reading in this selection has a window that applies
    /// to the request (`model`) at [`DEFAULT_NEAR_LIMIT_PERCENT`] or more.
    fn near_limit(&self, key: &str, model: Option<&str>) -> bool {
        self.reads
            .get(key)
            .is_some_and(|read| near_its_limit(&read.capacity, model))
    }

    /// The accounts whose reading is old at `now_ms`
    /// ([`is_old`](Self::is_old)), except those `skip` names.
    fn old_readings(
        &self,
        now_ms: u64,
        max_age: Duration,
        skip: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        self.reads
            .keys()
            .filter(|key| self.is_old(key.as_str(), now_ms, max_age) && !skip(key.as_str()))
            .cloned()
            .collect()
    }
}

/// One account's read for automatic selection.
#[derive(Clone)]
struct CandidateRead {
    capacity: super::quota_policy::AccountCapacity,
    /// Seat the reading belongs to (for a graced account: the seat stored now).
    seat: Option<SeatIdentity>,
    /// The read failed transiently: the failure and when reads resume.
    transient: Option<(super::usage_reads::TransientReadFailure, u64)>,
    /// Last good reading offered to the policy's grace.
    grace: Option<super::quota_policy::AccountCapacity>,
    /// The slot's epoch ([`UsageReads::epoch`](super::usage_reads::UsageReads::epoch))
    /// the evidence (the reading, or the last good one offered to grace)
    /// was taken at; `None` when there is none. A selection uses the
    /// evidence only while the epoch stays the same (p1-r3 B1).
    epoch: Option<u64>,
}

impl UsageReadError {
    /// A read answered by the process-wide usage-read table (a network,
    /// joined or held read): the same error and cause the read itself had.
    fn from_read(e: &super::usage_reads::ReadError) -> Self {
        Self {
            error: e.to_broker_error(),
            failure: e.cause(),
        }
    }

    /// This failure as the table's error for a read that never reached the
    /// usage request (slot check).
    fn into_read_error(self) -> super::usage_reads::ReadError {
        super::usage_reads::ReadError::Other {
            error: self.error,
            cause: self.failure,
        }
    }

    /// This failure as a network read's outcome (slot check, refresh).
    fn into_fetched(self) -> super::usage_reads::Fetched {
        super::usage_reads::Fetched::Failed {
            error: self.error,
            cause: self.failure,
        }
    }

    /// The `usage()` text stays the historical credential error; the cause
    /// keeps the refresh kind (transient vs rejected login).
    fn refresh(f: super::token::RefreshFailure) -> Self {
        Self {
            failure: refresh_failure(&f),
            error: BrokerError::Credential(f.into()),
        }
    }

    /// Slot checks: a slot that vanished vs a store that can't be read.
    fn slot(error: BrokerError) -> Self {
        use super::quota_policy::{AccountFailureKind as K, ObservationFailure};
        let kind = match &error {
            BrokerError::UnknownAccount { .. } => K::CredentialMissing,
            _ => K::CredentialStore,
        };
        Self {
            failure: ObservationFailure::new(kind),
            error,
        }
    }

    /// A failed usage request as the selection sees it: through the
    /// usage-read table, which keeps the request's own error and cause.
    #[cfg(test)]
    fn usage(e: super::usage::UsageError) -> Self {
        let table = super::usage_reads::UsageReads::default();
        match table.record_at("slot", super::usage_reads::Fetched::Usage(e), 0) {
            Err(read) => Self::from_read(&read),
            Ok(_) => unreachable!("a usage error is never a reading"),
        }
    }
}

/// A generic Codex window (including a free 30-day plan) does not prove
/// entitlement to Astra or any other requested model: a requested model the
/// reading does not list counts as unknown. Applied to every reading the
/// policy judges, fresh or graced.
fn require_requested_model(
    provider: OAuthProviderId,
    model: Option<&str>,
    capacity: &mut super::quota_policy::AccountCapacity,
) {
    use super::quota_policy::{model_matches, ModelAvailability, ModelState, QuotaObservation};
    if provider != OAuthProviderId::OpenAiCodex {
        return;
    }
    if let (Some(model), QuotaObservation::Ok { models, .. }) = (model, &mut capacity.observation) {
        let availability = models.get_or_insert_with(Vec::new);
        if !availability.iter().any(|m| model_matches(&m.model, model)) {
            availability.push(ModelAvailability {
                model: model.to_string(),
                state: ModelState::Unknown,
            });
        }
    }
}

/// What a usage read that needs action asks of the user. A refresh that
/// needs action carries our own instructions (paths, what to move back);
/// an unreadable stored login names the store, not the parser's message
/// (which can quote stored values).
fn action_detail(cred: &CredentialRef, err: &UsageReadError) -> String {
    use super::quota_policy::AccountFailureKind as K;
    match err.failure.kind {
        K::CredentialStore => format!(
            "the stored login of '{}' could not be read from {}; repair or restore the \
             credential store",
            cred.account.label_str(),
            storage::auth_file_path().display()
        ),
        _ => err.error.to_string(),
    }
}

/// The observation-failure kind of a token refresh failure, with the token
/// endpoint's HTTP status when it answered with one.
fn refresh_failure(f: &super::token::RefreshFailure) -> super::quota_policy::ObservationFailure {
    let failure = refresh_kind_failure(f.kind);
    match f.http_status() {
        Some(status) => failure.with_status(status),
        None => failure,
    }
}

/// The observation-failure kind of a token refresh failure kind.
fn refresh_kind_failure(
    kind: super::token::RefreshFailureKind,
) -> super::quota_policy::ObservationFailure {
    use super::quota_policy::{AccountFailureKind as K, ObservationFailure};
    use super::token::RefreshFailureKind as R;
    match kind {
        R::Transient => ObservationFailure::new(K::RefreshTransient),
        R::LockBusy => ObservationFailure::new(K::RefreshLockBusy),
        R::Other => ObservationFailure::new(K::RefreshOther),
        R::Cooldown { retry_in_secs } => {
            ObservationFailure::new(K::RefreshOther).with_retry_after(Some(retry_in_secs))
        }
        R::Credential => ObservationFailure::new(K::RefreshRejected),
        R::ActionRequired => ObservationFailure::new(K::RefreshActionRequired),
    }
}

/// The refresh failure of an automatically chosen account, naming it (and,
/// for a refused login, the command that logs it in again).
fn auto_vend_failure(
    credential: &CredentialRef,
    f: super::token::RefreshFailure,
) -> super::token::RefreshFailure {
    let label = credential.account.label_str();
    let mut message = format!("account '{label}': {}", f.message());
    if f.kind == super::token::RefreshFailureKind::Credential && !message.contains("synaps login") {
        let account = if credential.account.is_default() {
            String::new()
        } else {
            format!(" --account {label}")
        };
        message.push_str(&format!(
            "; log it in again with `synaps login --provider {}{account}`",
            credential.provider
        ));
    }
    super::token::RefreshFailure::new(f.kind, message).with_http_status(f.http_status())
}

/// A failed automatic selection: ONE WARN (see [`log_selection_failure`]),
/// then the typed error.
fn selection_failed(
    provider: OAuthProviderId,
    model: Option<&str>,
    reason: String,
    failure: super::quota_policy::SelectionFailure,
) -> BrokerError {
    log_selection_failure(provider, model, &failure);
    BrokerError::NoAccountAvailable {
        provider: provider.to_string(),
        reason,
        failure: Some(failure),
    }
}

/// The one `auth_selection_failed` WARN of a failed automatic selection:
/// the top-level kind and every account's `label:kind[:status][:cooling]`
/// (labels only, never identities or token material).
fn log_selection_failure(
    provider: OAuthProviderId,
    model: Option<&str>,
    failure: &super::quota_policy::SelectionFailure,
) {
    tracing::warn!(
        provider = %provider,
        model = model.unwrap_or("any"),
        kind = %failure.kind,
        accounts = %failure.accounts_field(),
        earliest_reset_ms = ?failure.earliest_reset_ms,
        "auth_selection_failed"
    );
}

// ── Bounded body handling ────────────────────────────────────────────────────

/// Read a response body up to `cap` bytes; fail closed (no truncated JSON
/// masquerading as a full payload) if the upstream exceeds the cap.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Result<String, BrokerError> {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| BrokerError::Transport(format!("failed to read response: {e}")))?;
        if buf.len() + chunk.len() > cap {
            return Err(BrokerError::Transport(format!(
                "response body exceeded the {cap}-byte broker buffering limit"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf)
        .map_err(|_| BrokerError::Transport("response body was not valid UTF-8".into()))
}

/// Cap on how much of a non-2xx upstream body is read for *classification*
/// only (spec §5.1). Error envelopes are tiny; anything larger is cut off
/// and simply fails to classify.
const MAX_PROXY_ERROR_CLASSIFY_BYTES: usize = 8 * 1024;

/// Read at most `cap` bytes of an untrusted error body for classification.
/// Never fails: overflow truncates (so JSON no longer parses → no label),
/// transport errors and non-UTF-8 yield `None`. The returned text must only
/// ever be fed to a vetted classifier — never to an error string or log.
async fn read_error_body_for_classification(resp: reqwest::Response, cap: usize) -> Option<String> {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(1024));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        let room = cap.saturating_sub(buf.len());
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            // Drop the rest unread: truncated JSON cannot classify, which is
            // the intended fail-closed outcome for oversized bodies.
            return String::from_utf8(buf).ok();
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).ok()
}

/// Legacy discovery for a static key: login config first, then env vars.
/// Only callable from inside the broker boundary.
fn discover_legacy_static_key(spec: &StaticProviderSpec) -> Option<String> {
    if let Some(v) = crate::config::get_provider_keys().get(spec.key) {
        if !v.is_empty() {
            return Some(v.clone());
        }
    }
    spec.env_vars
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
}

/// True if a static key is available through the broker (storage, login
/// config, or env). Never returns the key.
pub fn static_key_configured(provider: &str) -> bool {
    if provider == LOCAL_PROVIDER_KEY {
        return true; // the local endpoint never requires a key
    }
    let Some(spec) = static_provider(provider) else {
        return false;
    };
    storage::load_static_key(provider).ok().flatten().is_some()
        || discover_legacy_static_key(spec).is_some()
}

/// Non-secret display status for a static key: configured (with masked
/// preview), from-env, or not set. The mask keeps at most the first 4 and
/// last 4 characters and is safe to render.
pub fn static_key_status(provider: &str) -> StaticKeyStatus {
    let stored = storage::load_static_key(provider)
        .ok()
        .flatten()
        .or_else(|| {
            crate::config::get_provider_keys()
                .get(provider)
                .filter(|s| !s.is_empty())
                .cloned()
        });
    if let Some(key) = stored {
        return StaticKeyStatus::Configured {
            masked: mask_key(&key),
        };
    }
    if let Some(spec) = static_provider(provider) {
        if spec
            .env_vars
            .iter()
            .any(|var| std::env::var(var).is_ok_and(|v| !v.is_empty()))
        {
            return StaticKeyStatus::FromEnv;
        }
    }
    StaticKeyStatus::NotSet
}

/// Masked, non-reversible preview of a key (`gsk-…7f2a`).
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// The set of static providers with an available credential. Non-secret.
pub fn configured_static_provider_keys() -> std::collections::BTreeSet<String> {
    STATIC_PROVIDERS
        .iter()
        .filter(|s| static_key_configured(s.key))
        .map(|s| s.key.to_string())
        .collect()
}

/// The local endpoint URL (non-secret configuration: `provider.local.url`
/// config → `LOCAL_ENDPOINT` env → default).
pub fn local_endpoint_url() -> String {
    local_endpoint_config()
        .unwrap_or_else(|| LOCAL_DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The explicitly-configured local endpoint, if any (None → default in use).
pub fn local_endpoint_config() -> Option<String> {
    crate::config::get_provider_keys()
        .get("local.url")
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| {
            std::env::var("LOCAL_ENDPOINT")
                .ok()
                .filter(|s| !s.is_empty())
        })
}

/// Comma-separated local model ids from non-secret config (`provider.local.models`).
pub fn local_model_ids() -> Vec<String> {
    crate::config::get_provider_keys()
        .get("local.models")
        .map(|value| {
            value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// True if any Anthropic credential is available through the broker
/// (OAuth login under the active account policy, or `ANTHROPIC_API_KEY`).
/// Non-secret answer for first-run UX.
pub fn anthropic_credential_available() -> bool {
    oauth_provider_logged_in(OAuthProviderId::Anthropic)
        || std::env::var("ANTHROPIC_API_KEY").is_ok_and(|v| !v.is_empty())
}

/// True if the OAuth provider has a stored, refreshable credential the
/// active account policy (env > config > default) can reach. Non-secret
/// availability query for UI surfaces; an expired access token is still a
/// valid login when the broker can refresh it on first use.
///
/// * An explicitly selected named account counts only if THAT slot exists
///   (explicit selection never falls back, so neither does this answer).
/// * Otherwise any usable slot of the provider counts — a named-only
///   installation is logged in, not hidden. Which slot a vend uses, and
///   whether a malformed selector fails it, is reported by the vend itself.
pub fn oauth_provider_logged_in(provider: OAuthProviderId) -> bool {
    oauth_provider_logged_in_at(
        &storage::auth_file_path(),
        &AccountPolicy::from_environment(),
        provider,
    )
}

/// Path/policy-explicit core of [`oauth_provider_logged_in`].
pub(crate) fn oauth_provider_logged_in_at(
    path: &std::path::Path,
    policy: &AccountPolicy,
    provider: OAuthProviderId,
) -> bool {
    let Ok(inventory) = storage::list_accounts_detailed_at(path, Some(provider)) else {
        return false;
    };
    match policy.selector(provider) {
        AccountSelector::Account(Account::Named(label)) => inventory
            .accounts
            .iter()
            .any(|row| row.label == label.as_str()),
        AccountSelector::Account(Account::Default)
        | AccountSelector::Auto
        | AccountSelector::Invalid { .. } => !inventory.accounts.is_empty(),
    }
}

#[async_trait]
impl CredentialBroker for LocalBroker {
    async fn recover_rejected_token(
        &self,
        cred: &CredentialRef,
        rejected: &str,
    ) -> Result<AccessToken, BrokerError> {
        self.check_slot_exists(cred)?;
        // Never held (a rejected token's recovery); a refusal of the login
        // still drops its usage reading ([`ReadingDrop`]).
        let memory: Arc<dyn super::token::RefusalMemory> = Arc::new(self.reading_drop(cred));
        let creds = super::token::recover_rejected_credential(
            &self.http,
            cred,
            rejected,
            &self.refresh_endpoints,
            Some(memory),
        )
        .await
        .map_err(BrokerError::Refresh)?;
        Ok(AccessToken {
            token: creds.access,
            expires: creds.expires,
        })
    }

    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Ok(self.access_token_pinned(provider).await?.token)
    }

    fn account_selector(&self, provider: OAuthProviderId) -> AccountSelector {
        self.policy().selector(provider)
    }

    async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        self.check_slot_exists(cred)?;
        // An account the user chose is never held; a refusal of its login
        // still drops its usage reading ([`ReadingDrop`]).
        let memory: Arc<dyn super::token::RefusalMemory> = Arc::new(self.reading_drop(cred));
        let creds = super::token::ensure_fresh_credential_via(
            &self.http,
            cred,
            &self.refresh_endpoints,
            Some(memory),
        )
        .await
        .map_err(|f| match f.kind {
            // Typed so the runtime doesn't send the user to a login
            // that would fail on the same store.
            super::token::RefreshFailureKind::ActionRequired => BrokerError::Refresh(f),
            _ => BrokerError::Credential(f.into()),
        })?;
        // Strip to token + expiry: the refresh token stays behind the boundary.
        Ok(AccessToken {
            token: creds.access,
            expires: creds.expires,
        })
    }

    async fn access_token_pinned(
        &self,
        provider: OAuthProviderId,
    ) -> Result<PinnedToken, BrokerError> {
        self.access_token_pinned_for(provider, None).await
    }

    async fn access_token_pinned_for(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        self.resolve_and_vend(provider, None, model, &[]).await
    }

    /// The exclusion goes into the selection itself: `auto` selects among
    /// the accounts the request hasn't used (one that is cooling down still
    /// reads as cooling down in a failure).
    async fn access_token_pinned_excluding(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
        exclude: &[CredentialRef],
    ) -> Result<PinnedToken, BrokerError> {
        self.resolve_and_vend(provider, None, model, exclude).await
    }

    async fn accounts(&self, provider: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError> {
        let mut rows = storage::list_accounts(provider).map_err(BrokerError::Credential)?;
        let selector = self.policy().selector(provider);
        let now_ms = crate::epoch_millis();
        for row in &mut rows {
            row.selected = matches!(&selector, AccountSelector::Account(a) if a.label_str() == row.label);
            if let Some(cred) = row.credential_ref() {
                row.cooldown_until = self.cooldown_until(&cred.storage_key(), now_ms);
            }
        }
        Ok(rows)
    }

    async fn usage(&self, cred: &CredentialRef) -> Result<super::usage::UsageSnapshot, BrokerError> {
        self.usage_with_seat(cred)
            .await
            .map(|(snapshot, _seat)| snapshot)
            .map_err(|e| e.error)
    }

    async fn usage_or_last_known(
        &self,
        cred: &CredentialRef,
    ) -> Result<super::usage::UsageSnapshot, Box<UsageReadFailure>> {
        self.last_known_usage(cred).await
    }

    fn active_cooldown(&self, cred: &CredentialRef) -> Option<u64> {
        self.active_cooldown_at(cred, crate::epoch_millis())
            .map(|c| c.until_ms)
    }

    fn active_cooldown_for(&self, cred: &CredentialRef, model: Option<&str>) -> Option<u64> {
        self.cooldown_for_at(cred, model, crate::epoch_millis())
            .map(|c| c.until_ms)
    }

    /// Clears the whole account's cooldown only: a limit of one model's
    /// requests (Fable's weekly limit) stays, since the accepted request
    /// (whose model this call does not name) proves nothing about it.
    fn clear_cooldown(&self, cred: &CredentialRef, sent_at_ms: u64) {
        let now_ms = crate::epoch_millis();
        let account_cooling = self
            .cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|key| key.account_at(now_ms).is_some());
        if !account_cooling {
            return;
        }
        let keys = self.cooldown_keys(cred);
        let mut cleared = None;
        {
            let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
            for key in keys.read() {
                let Some(entry) = cooldowns.get_mut(key) else {
                    continue;
                };
                if entry
                    .all
                    .as_ref()
                    .is_some_and(|c| c.active_at(now_ms) && c.reported_at_ms <= sent_at_ms)
                {
                    cleared = entry.all.take().or(cleared);
                    if entry.models.is_empty() {
                        cooldowns.remove(key);
                    }
                }
            }
        }
        if let Some(c) = cleared {
            tracing::info!(
                credential = %cred,
                until_ms = c.until_ms,
                reason = %c.reason,
                "account cooldown cleared: a request sent after it was reported was accepted"
            );
        }
    }

    async fn access_token_for_auto(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        self.check_slot_exists(cred)?;
        // A refused refresh of this login is held: no new exchange.
        if let Some(held) = self.held_refresh(cred) {
            return Err(BrokerError::Refresh(auto_vend_failure(cred, held)));
        }
        let memory: Arc<dyn super::token::RefusalMemory> = self.slot_hold(cred);
        let creds = super::token::ensure_fresh_credential_via(
            &self.http,
            cred,
            &self.refresh_endpoints,
            Some(memory),
        )
        .await
        .map_err(|f| BrokerError::Refresh(auto_vend_failure(cred, f)))?;
        Ok(AccessToken {
            token: creds.access,
            expires: creds.expires,
        })
    }

    async fn report_cooldown(
        &self,
        cred: &CredentialRef,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let cooldown = Cooldown::reported(until_ms, reason);
        tracing::info!(
            credential = %cred,
            until_ms = cooldown.until_ms,
            reason = %cooldown.reason,
            "account cooldown reported"
        );
        let keys = self.cooldown_keys(cred);
        {
            let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
            for key in keys.written() {
                cooldowns.entry(key.to_string()).or_default().all = Some(cooldown.clone());
            }
        }
        // The cached reading predates the limit report; drop it so the next
        // selection re-reads instead of trusting stale headroom, and drop
        // the slot's last good reading so grace can never outlive the limit
        // (its saved copy too: loaded first, so a saved one is dropped even
        // if this process has not read the slot).
        self.reading_drop(cred).drop_reading();
        Ok(())
    }

    /// A limit of `model`'s requests only (Fable's weekly limit), under the
    /// same keys as a whole-account report: automatic selection stops
    /// choosing the account for that model's requests (and for a request
    /// with no model) until `until_ms`; other models keep it. The reading
    /// is kept: its shared windows are still true, and this cooldown stops
    /// the model's requests whatever room the reading shows for it. A model
    /// that is not one plain word cools the whole account instead.
    async fn report_model_cooldown(
        &self,
        cred: &CredentialRef,
        model: &str,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let model = model.trim().to_ascii_lowercase();
        if model.is_empty()
            || model.len() > 32
            || !model
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return self.report_cooldown(cred, until_ms, reason).await;
        }
        let cooldown = Cooldown::reported(until_ms, reason);
        tracing::info!(
            credential = %cred,
            model = %model,
            until_ms = cooldown.until_ms,
            reason = %cooldown.reason,
            "account cooldown reported"
        );
        let keys = self.cooldown_keys(cred);
        {
            let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
            for key in keys.written() {
                cooldowns
                    .entry(key.to_string())
                    .or_default()
                    .models
                    .insert(model.clone(), cooldown.clone());
            }
        }
        Ok(())
    }

    async fn proxy(&self, request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        let resp = self.send(&request).await?;
        let status = resp.status().as_u16();
        let body = read_body_capped(resp, self.max_response_bytes).await?;
        Ok(ProxyResponse { status, body })
    }

    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        let mut request = request;
        request.stream = true;
        let resp = self.send(&request).await?;
        let status = resp.status();
        if !status.is_success() {
            // Spec §5.1: the upstream may echo the full request (prompts,
            // tool schemas, credentials) in its error body. The body is read
            // (bounded) for *classification only*: it selects one of OUR
            // static labels or nothing — no provider bytes reach the error
            // or the log. The stable `provider request failed: {status}`
            // prefix (with canonical reason phrase, e.g. `429 Too Many
            // Requests`) is the transport contract engine-side classifiers
            // parse; the optional ` [label]` suffix follows it and contains
            // no ':' so `redact_provider_proxy_error` keeps it intact.
            let label = read_error_body_for_classification(resp, MAX_PROXY_ERROR_CLASSIFY_BYTES)
                .await
                .as_deref()
                .and_then(crate::error::vetted_proxy_error_label);
            tracing::warn!(
                provider = %request.provider,
                status = status.as_u16(),
                label = label.unwrap_or("-"),
                "provider request failed"
            );
            let label_suffix = label.map(|l| format!(" [{l}]")).unwrap_or_default();
            return Err(BrokerError::Transport(format!(
                "provider request failed: {status}{label_suffix}"
            )));
        }
        use futures::StreamExt;
        let stream = resp
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| BrokerError::Transport(format!("stream error: {e}"))));
        Ok(Box::pin(stream))
    }

    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        // Token resolution happens HERE, behind the boundary — the caller
        // never touches auth.json or sees the access token.
        let token = self.access_token(OAuthProviderId::Anthropic).await?;
        let url = self
            .anthropic_usage_url
            .as_deref()
            .unwrap_or(ANTHROPIC_USAGE_URL);
        let resp = self
            .http
            .get(url)
            .timeout(self.request_timeout)
            .bearer_auth(&token.token)
            .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(format!("usage request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            // The error body is upstream-controlled and may echo the request;
            // it never enters the error (spec §5.1).
            drop(resp);
            return Err(BrokerError::Transport(format!(
                "usage request failed: {status}"
            )));
        }
        let body = read_body_capped(resp, self.max_response_bytes).await?;
        serde_json::from_str(&body)
            .map_err(|e| BrokerError::Transport(format!("invalid usage response: {e}")))
    }

    async fn cloud_catalog(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        allow_stale: bool,
    ) -> Result<Vec<CloudCatalogEntry>, BrokerError> {
        self.cloud_backend
            .as_ref()
            .ok_or_else(|| BrokerError::NotConfigured(provider.to_string()))?
            .catalog(provider, context_ref, allow_stale)
            .await
    }

    async fn cloud_invoke(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        model_id: &str,
        request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError> {
        self.cloud_backend
            .as_ref()
            .ok_or_else(|| BrokerError::NotConfigured(provider.to_string()))?
            .invoke(provider, context_ref, model_id, request)
            .await
    }

    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        let mut out = Vec::new();
        let policy = self.policy();
        for descriptor in super::provider::registry().iter() {
            let accounts = self.accounts(descriptor.id).await.unwrap_or_default();
            let configured = match policy.selector(descriptor.id) {
                AccountSelector::Account(account) => accounts
                    .iter()
                    .any(|a| a.label == account.label_str()),
                AccountSelector::Auto => !accounts.is_empty(),
                AccountSelector::Invalid { .. } => false,
            };
            out.push(ProviderStatus {
                key: descriptor.id.as_str().to_string(),
                name: descriptor.display_name.to_string(),
                kind: CredentialKind::OAuth,
                configured,
                accounts,
            });
        }
        for spec in STATIC_PROVIDERS {
            out.push(ProviderStatus {
                key: spec.key.to_string(),
                name: spec.name.to_string(),
                kind: CredentialKind::StaticKey,
                configured: static_key_configured(spec.key),
                accounts: Vec::new(),
            });
        }
        out.push(ProviderStatus {
            key: LOCAL_PROVIDER_KEY.to_string(),
            name: "Local endpoint".to_string(),
            kind: CredentialKind::LocalEndpoint,
            configured: true,
            accounts: Vec::new(),
        });
        Ok(out)
    }
}

// ── Remote broker client ─────────────────────────────────────────────────────

/// Client for a remote `synaps auth-broker`. Presents the machine's own
/// bearer token; receives access tokens (OAuth) and proxied responses
/// (static-key providers). Never receives refresh tokens or raw keys.
pub struct RemoteBroker {
    http: reqwest::Client,
    endpoint: String,
    /// Reloadable from the local broker token file after a 401 when the
    /// endpoint is loopback (prA I2).
    machine_token: super::MachineToken,
    cache: super::TokenCache,
    /// Injected account policy; `None` → env + config per call.
    account_policy: Option<AccountPolicy>,
}

/// A 404 from a route the broker does not have: 0.10.2's axum router
/// answers an empty, untyped body, while a current broker's errors are JSON.
/// Only emptiness and the content type are inspected, never the content.
async fn route_missing(resp: reqwest::Response) -> bool {
    let json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    !json
        && read_body_capped(resp, 1024)
            .await
            .is_ok_and(|body| body.trim().is_empty())
}

impl RemoteBroker {
    /// True if the broker has no `/usage/snapshot` (0.10.2 and older). The
    /// probe sends an empty account, which a current broker rejects with a
    /// 400 before touching any credential.
    async fn predates_usage_snapshot(&self) -> Result<bool, BrokerError> {
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                    .get(format!("{}/usage/snapshot", self.endpoint))
                    .query(&[("provider", "anthropic"), ("account", "")])
                    .bearer_auth(t)
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            404 => Ok(route_missing(resp).await),
            _ => Ok(false),
        }
    }

    pub fn new(
        endpoint: impl Into<String>,
        machine_token: impl Into<String>,
        http: reqwest::Client,
        cache: super::TokenCache,
    ) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        // Never trust a caller-supplied redirect policy for credential-bearing
        // broker RPC. A redirect could otherwise replay machine auth off-origin.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .unwrap_or(http);
        let machine_token = super::MachineToken::for_endpoint(&endpoint, machine_token);
        Self {
            http,
            endpoint,
            machine_token,
            cache,
            account_policy: None,
        }
    }

    /// Pin the account policy instead of reading env + config per call.
    pub fn with_account_policy(mut self, policy: AccountPolicy) -> Self {
        self.account_policy = Some(policy);
        self
    }

    fn policy(&self) -> AccountPolicy {
        self.account_policy
            .clone()
            .unwrap_or_else(AccountPolicy::from_environment)
    }

    fn fetcher(&self) -> super::BrokerClient {
        super::BrokerClient::with_machine_token(
            self.endpoint.clone(),
            self.machine_token.clone(),
            self.http.clone(),
        )
    }

    /// Cache scope: endpoint + machine-principal digest (never the token).
    fn scope(&self) -> String {
        super::credential_source::source_scope(&self.endpoint, &self.machine_token.get())
    }

    /// Resolve the credential for a token request on the client side. An
    /// `auto` selector is delegated to the broker daemon (`account=auto`),
    /// which holds the usage/cooldown state; the daemon reports which slot
    /// it chose.
    async fn resolve_pinned(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        match self.policy().selector(provider) {
            AccountSelector::Account(account) => {
                let credential = CredentialRef::new(provider, account);
                let token = self.access_token_for(&credential).await?;
                Ok(PinnedToken { credential, token })
            }
            AccountSelector::Auto => self.fetch_auto(provider, model).await,
            AccountSelector::Invalid { source, reason } => {
                Err(BrokerError::InvalidAccount(format!("{source}: {reason}")))
            }
        }
    }

    /// `GET /token?provider=X&account=auto[&model=M]` — the daemon selects;
    /// the response's `account` field names the chosen slot. Never cached
    /// (the selection is the daemon's, per call).
    async fn fetch_auto(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        let mut query: Vec<(&str, &str)> = vec![
            ("provider", provider.as_str()),
            ("account", super::account::AUTO_ACCOUNT_NAME),
        ];
        if let Some(model) = model {
            query.push(("model", model));
        }
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .get(format!("{}/token", self.endpoint))
                .query(&query)
                .bearer_auth(t)
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => return Err(BrokerError::Unauthorized),
            503 => {
                drop(resp);
                return Err(BrokerError::NoAccountAvailable {
                    provider: provider.as_str().to_string(),
                    reason: "broker reported no account with proven capacity".into(),
                    // The broker host's own log has the per-account kinds.
                    failure: Some(super::quota_policy::SelectionFailure::new(
                        super::quota_policy::SelectionFailureKind::RemoteNoCapacity,
                    )),
                });
            }
            s if !(200..300).contains(&s) => {
                drop(resp);
                return Err(BrokerError::Transport(format!("broker returned HTTP {s}")));
            }
            _ => {}
        }
        #[derive(Deserialize)]
        struct AutoToken {
            access_token: String,
            expires: u64,
            #[serde(default)]
            ttl_ms: Option<u64>,
            account: Option<String>,
        }
        let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
        let tok: AutoToken = serde_json::from_str(&body)
            .map_err(|e| BrokerError::Transport(format!("invalid broker token response: {e}")))?;
        let account = tok
            .account
            .as_deref()
            .ok_or_else(|| BrokerError::Transport("broker did not report the selected account".into()))
            .and_then(|a| Account::parse(a).map_err(BrokerError::InvalidAccount))?;
        if tok.access_token.is_empty() {
            return Err(BrokerError::Transport("broker returned an empty access_token".into()));
        }
        let expires = match tok.ttl_ms {
            Some(ttl) => crate::epoch_millis().saturating_add(ttl),
            None => tok.expires,
        };
        Ok(PinnedToken {
            credential: CredentialRef::new(provider, account),
            token: AccessToken {
                token: tok.access_token,
                expires,
            },
        })
    }

    /// A non-2xx `/proxy` answer. A 502 whose `error` is a retryable
    /// `BrokerError::Refresh` from the daemon (it maps those to 502) becomes
    /// the same typed error, so the client keeps its "retry shortly" hint.
    /// Only the KIND is read from the body, by matching the exact prefix our
    /// own Display produces for that kind; the message is static, so no
    /// broker-controlled byte reaches the Display (spec §5.1).
    async fn proxy_failure(resp: reqwest::Response, fallback: String) -> BrokerError {
        if resp.status() != reqwest::StatusCode::BAD_GATEWAY {
            return BrokerError::Transport(fallback);
        }
        let Ok(body) = read_body_capped(resp, 4096).await else {
            return BrokerError::Transport(fallback);
        };
        let text = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_default();
        use super::token::{RefreshFailure, RefreshFailureKind as K};
        for kind in [K::Transient, K::LockBusy, K::Other] {
            let prefix = BrokerError::Refresh(RefreshFailure::new(kind, "")).to_string();
            if text.starts_with(&prefix) {
                return BrokerError::Refresh(RefreshFailure::new(
                    kind,
                    "the remote credential broker could not refresh this credential",
                ));
            }
        }
        BrokerError::Transport(fallback)
    }

    async fn post_proxy(&self, request: &ProxyRequest) -> Result<reqwest::Response, BrokerError> {
        request.validate()?;
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .post(format!("{}/proxy", self.endpoint))
                .bearer_auth(t)
                .json(request)
            })
            .await
            .map_err(|e| {
                let message = format!("broker request failed: {e}");
                if e.is_connect() {
                    BrokerError::ConnectFailed(message)
                } else {
                    BrokerError::Transport(message)
                }
            })?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            s @ (400 | 403) => {
                // A compromised broker controls every byte of its response —
                // including JSON `error` fields. Static status-only message;
                // the body is dropped unread (spec §5.1).
                drop(resp);
                Err(BrokerError::Denied(format!(
                    "broker rejected the request (HTTP {s})"
                )))
            }
            _ => Ok(resp),
        }
    }
}

#[async_trait]
impl CredentialBroker for RemoteBroker {
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Ok(self.access_token_pinned(provider).await?.token)
    }

    fn account_selector(&self, provider: OAuthProviderId) -> AccountSelector {
        self.policy().selector(provider)
    }

    async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        let fetcher = self.fetcher();
        let tok = super::credential_source::resolve_remote_credential(
            &fetcher,
            &self.cache,
            &self.scope(),
            cred,
            super::DEFAULT_MARGIN_MS,
        )
        .await
        .map_err(|e| e.into_broker_error(cred))?;
        Ok(AccessToken {
            token: tok.access_token,
            expires: tok.expires,
        })
    }

    async fn access_token_pinned(
        &self,
        provider: OAuthProviderId,
    ) -> Result<PinnedToken, BrokerError> {
        self.resolve_pinned(provider, None).await
    }

    async fn access_token_pinned_for(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        self.resolve_pinned(provider, model).await
    }

    async fn accounts(&self, provider: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError> {
        let caps = self.capabilities().await?;
        let accounts = caps
            .into_iter()
            .find(|c| c.key == provider.as_str())
            .map(|c| c.accounts)
            .unwrap_or_default();
        // A 0.10.2 broker lists no accounts at all; don't report that as
        // "nothing logged in".
        if accounts.is_empty() && self.predates_usage_snapshot().await? {
            return Err(BrokerError::BrokerTooOld("account listing".into()));
        }
        Ok(accounts)
    }

    async fn usage(&self, cred: &CredentialRef) -> Result<super::usage::UsageSnapshot, BrokerError> {
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .get(format!("{}/usage/snapshot", self.endpoint))
                .query(&[
                    ("provider", cred.provider.as_str()),
                    ("account", cred.account.label_str()),
                ])
                .bearer_auth(t)
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            404 => {
                if !route_missing(resp).await {
                    return Err(BrokerError::UnknownAccount {
                        provider: cred.provider.as_str().to_string(),
                        label: cred.account.label_str().to_string(),
                    });
                }
                if cred.provider != OAuthProviderId::Anthropic || !cred.account.is_default() {
                    return Err(BrokerError::BrokerTooOld("per-account usage".into()));
                }
                // The old broker's `/usage` serves its default Anthropic slot.
                let body = self.anthropic_usage().await?.to_string();
                super::usage::parse_anthropic_usage(
                    &body,
                    cred.account.label_str(),
                    crate::epoch_millis(),
                )
                .map_err(super::usage::UsageError::into_broker_error)
            }
            s if !(200..300).contains(&s) => {
                // Broker-controlled error body: dropped unread (spec §5.1).
                drop(resp);
                Err(BrokerError::Transport(format!(
                    "broker usage snapshot returned HTTP {s}"
                )))
            }
            _ => {
                let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
                let snapshot: super::usage::UsageSnapshot = serde_json::from_str(&body)
                    .map_err(|e| BrokerError::Transport(format!("invalid usage snapshot: {e}")))?;
                if snapshot.provider != cred.provider.as_str()
                    || snapshot.account != cred.account.label_str()
                {
                    return Err(BrokerError::Transport(
                        "broker returned a usage snapshot for a different account".into(),
                    ));
                }
                Ok(snapshot)
            }
        }
    }

    async fn report_cooldown(
        &self,
        cred: &CredentialRef,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .post(format!("{}/accounts/cooldown", self.endpoint))
                .bearer_auth(t)
                .json(&serde_json::json!({
                    "provider": cred.provider.as_str(),
                    "account": cred.account.label_str(),
                    "until_ms": until_ms,
                    "reason": crate::truncate_str(reason, 64),
                }))
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            s if !(200..300).contains(&s) => {
                Err(BrokerError::Transport(format!("broker returned HTTP {s}")))
            }
            _ => Ok(()),
        }
    }

    async fn proxy(&self, request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        let mut request = request;
        request.stream = false;
        let resp = self.post_proxy(&request).await?;
        if !resp.status().is_success() {
            let fallback = format!("broker proxy returned HTTP {}", resp.status());
            return Err(Self::proxy_failure(resp, fallback).await);
        }
        let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
        serde_json::from_str::<ProxyResponse>(&body)
            .map_err(|e| BrokerError::Transport(format!("invalid broker proxy response: {e}")))
    }

    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        let mut request = request;
        request.stream = true;
        let resp = self.post_proxy(&request).await?;
        let status = resp.status();
        if !status.is_success() {
            // Broker-controlled error body: only a refresh-failure kind is
            // recovered from it, never its text (spec §5.1).
            let fallback = format!("broker proxy stream failed: {status}");
            return Err(Self::proxy_failure(resp, fallback).await);
        }
        use futures::StreamExt;
        let stream = resp
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| BrokerError::Transport(format!("stream error: {e}"))));
        Ok(Box::pin(stream))
    }

    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        // Machine-authenticated typed operation: the remote broker resolves
        // the OAuth token on its side and returns usage JSON only.
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .get(format!("{}/usage", self.endpoint))
                .bearer_auth(t)
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            s if !(200..300).contains(&s) => {
                // Broker-controlled error body: dropped unread (spec §5.1).
                drop(resp);
                Err(BrokerError::Transport(format!(
                    "broker usage returned HTTP {s}"
                )))
            }
            _ => {
                let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
                serde_json::from_str(&body)
                    .map_err(|e| BrokerError::Transport(format!("invalid usage response: {e}")))
            }
        }
    }

    async fn cloud_catalog(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        allow_stale: bool,
    ) -> Result<Vec<CloudCatalogEntry>, BrokerError> {
        let resp = self.machine_token.send(|t| self.http.post(format!("{}/cloud/catalog", self.endpoint)).bearer_auth(t).json(&serde_json::json!({"provider":provider,"context_ref":context_ref,"allow_stale":allow_stale}))).await.map_err(|e| BrokerError::Transport(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BrokerError::Unauthorized);
        }
        if !resp.status().is_success() {
            return Err(BrokerError::Transport(format!(
                "cloud catalog returned HTTP {}",
                resp.status()
            )));
        }
        let body = read_body_capped(resp, MAX_CLOUD_CATALOG_BODY_BYTES).await?;
        let entries: Vec<CloudCatalogEntry> = serde_json::from_str(&body)
            .map_err(|e| BrokerError::Transport(format!("invalid cloud catalog: {e}")))?;
        if entries.len() > MAX_CLOUD_CATALOG_ENTRIES
            || entries.iter().any(|e| {
                e.provider != provider || !e.context_ref.starts_with("ctx-") || e.id.len() > 512
            })
        {
            return Err(BrokerError::Transport(
                "broker returned an invalid cloud catalog".into(),
            ));
        }
        Ok(entries)
    }

    async fn cloud_invoke(
        &self,
        provider: CloudProviderId,
        context_ref: &str,
        model_id: &str,
        request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError> {
        let resp = self.machine_token.send(|t| self.http.post(format!("{}/cloud/invoke", self.endpoint)).bearer_auth(t).json(&serde_json::json!({"provider":provider,"context_ref":context_ref,"model_id":model_id,"request":request}))).await.map_err(|e| BrokerError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                BrokerError::Unauthorized
            } else {
                BrokerError::Transport(format!("cloud invoke returned HTTP {}", resp.status()))
            });
        }
        use futures::StreamExt;
        let chunks = Box::pin(resp.bytes_stream());
        let stream = futures::stream::unfold(
            (chunks, Vec::<u8>::new(), false, false),
            |(mut chunks, mut buffer, done, failed)| async move {
                if failed {
                    return None;
                }
                loop {
                    if let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=end).collect();
                        let parsed = serde_json::from_slice::<CloudEvent>(&line[..end])
                            .map_err(|_| BrokerError::Transport("invalid cloud event".into()));
                        let event = match parsed {
                            Ok(CloudEvent::Done) if done => Err(BrokerError::Transport(
                                "duplicate cloud terminal event".into(),
                            )),
                            Ok(CloudEvent::Done) => Ok(CloudEvent::Done),
                            Ok(_) if done => Err(BrokerError::Transport(
                                "cloud data followed terminal event".into(),
                            )),
                            other => other,
                        };
                        let next_done = done || matches!(event, Ok(CloudEvent::Done));
                        let next_failed = event.is_err();
                        return Some((event, (chunks, buffer, next_done, next_failed)));
                    }
                    match chunks.next().await {
                        Some(Ok(chunk))
                            if buffer.len() + chunk.len() <= MAX_CLOUD_STREAM_EVENT_BYTES =>
                        {
                            buffer.extend_from_slice(&chunk)
                        }
                        Some(Ok(_)) => {
                            return Some((
                                Err(BrokerError::Transport("cloud event exceeded limit".into())),
                                (chunks, Vec::new(), done, true),
                            ))
                        }
                        Some(Err(_)) => {
                            return Some((
                                Err(BrokerError::Transport("broker stream failed".into())),
                                (chunks, Vec::new(), done, true),
                            ))
                        }
                        None if buffer.is_empty() && done => return None,
                        None if buffer.is_empty() => {
                            return Some((
                                Err(BrokerError::Transport(
                                    "cloud stream ended without terminal event".into(),
                                )),
                                (chunks, Vec::new(), done, true),
                            ))
                        }
                        None => {
                            return Some((
                                Err(BrokerError::Transport("truncated cloud event".into())),
                                (chunks, Vec::new(), done, true),
                            ))
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }

    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        let resp = self
            .machine_token
            .send(|t| {
                self.http
                .get(format!("{}/capabilities", self.endpoint))
                .bearer_auth(t)
            })
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            s if !(200..300).contains(&s) => {
                Err(BrokerError::Transport(format!("broker returned HTTP {s}")))
            }
            _ => {
                let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
                serde_json::from_str::<Vec<ProviderStatus>>(&body).map_err(|e| {
                    BrokerError::Transport(format!("invalid capabilities response: {e}"))
                })
            }
        }
    }
}

// ── Construction and process-wide handle ─────────────────────────────────────

/// Build the right broker for a credential source. Local sources get the
/// in-process broker (no daemon needed); remote sources get the authenticated
/// Pre-flight capability check for a cloud route (spec §5.5). Pure function:
/// callers MUST run it before constructing a broker, looking up credentials,
/// or opening any connection. The invoke-time guard inside the broker remains
/// in place as defense in depth.
pub fn preflight_cloud_capability(
    provider: CloudProviderId,
    needs_tools: bool,
) -> Result<(), BrokerError> {
    if needs_tools && !provider.supports_tools() {
        return Err(BrokerError::UnsupportedCapability {
            provider: provider.to_string(),
            capability: "tools".into(),
        });
    }
    Ok(())
}

/// `synaps auth state` for the local broker `cache` holds for the current
/// credential store and config (the one requests use) or, when no request
/// has built one yet, the account list with nothing recorded. `live`: the
/// caller is the process that serves requests (the daemon). No network
/// request, no refresh; `query.clear` never writes the store.
pub fn local_auth_state(
    cache: &super::TokenCache,
    query: &super::auth_state::AuthStateQuery,
    live: bool,
) -> Result<super::auth_state::AuthStateReport, BrokerError> {
    let key = (
        storage::auth_file_path(),
        crate::config::resolve_read_path("config"),
    );
    let broker = cache
        .local_brokers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    let mut report = auth_state_report(broker.as_deref(), query)?;
    report.live = live;
    Ok(report)
}

fn auth_state_report(
    broker: Option<&LocalBroker>,
    query: &super::auth_state::AuthStateQuery,
) -> Result<super::auth_state::AuthStateReport, BrokerError> {
    use super::auth_state::{
        AccountState, AuthStateReport, IdentityKind, ObservedOutcome, AUTH_STATE_SCHEMA_VERSION,
    };
    let provider = match query.provider.as_deref() {
        Some(p) => Some(
            p.parse::<OAuthProviderId>()
                .map_err(|_| BrokerError::UnknownProvider(p.to_string()))?,
        ),
        None => None,
    };
    let mut summaries = match provider {
        Some(p) => storage::list_accounts(p),
        None => storage::list_all_accounts(),
    }
    .map_err(BrokerError::Credential)?;
    if let Some(label) = query.account.as_deref() {
        summaries.retain(|s| s.label == label);
        if summaries.is_empty() {
            return Err(BrokerError::UnknownAccount {
                provider: provider.map_or("any provider", |p| p.as_str()).to_string(),
                label: label.to_string(),
            });
        }
    }
    let creds: Vec<CredentialRef> = summaries
        .iter()
        .filter_map(AccountSummary::credential_ref)
        .collect();
    let mut cleared = Vec::new();
    if query.clear {
        if let Some(broker) = broker {
            let whole: Vec<OAuthProviderId> = match (&query.account, provider) {
                (Some(_), _) => Vec::new(),
                (None, Some(p)) => vec![p],
                (None, None) => super::provider::DESCRIPTORS.iter().map(|d| d.id).collect(),
            };
            broker.clear_state(&creds, &whole);
        }
        cleared = creds
            .iter()
            .map(|c| format!("{}:{}", c.provider, c.account.label_str()))
            .collect();
        tracing::info!(
            accounts = %cleared.join(","),
            broker_active = broker.is_some(),
            "auth_state_cleared"
        );
    }
    let store = storage::auth_file_path();
    let now_ms = crate::epoch_millis();
    let mut accounts = Vec::with_capacity(summaries.len());
    for summary in &summaries {
        let Some(cred) = summary.credential_ref() else {
            continue;
        };
        let observed = broker
            .and_then(|b| {
                b.observations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&LocalBroker::state_key(&cred))
                    .cloned()
            })
            .unwrap_or_default();
        accounts.push(AccountState {
            provider: summary.provider.clone(),
            label: summary.label.clone(),
            identity: if summary.seat_fingerprint.is_some() {
                IdentityKind::Stable
            } else {
                IdentityKind::Rotating
            },
            access_expires_ms: (summary.expires != 0).then_some(summary.expires),
            cooldown: broker.and_then(|b| b.cooldown_state(&cred, now_ms)),
            last_usage: observed.last_usage,
            last_usage_ok_ms: observed.last_usage_ok_ms,
            last_refresh: super::token::last_refresh_exchange(&store, &cred.storage_key()).map(
                |x| ObservedOutcome {
                    at_ms: x.at_ms,
                    failure: x.failure.map(|kind| {
                        let failure = refresh_kind_failure(kind);
                        match x.http_status {
                            Some(status) => failure.with_status(status),
                            None => failure,
                        }
                    }),
                },
            ),
            refresh_hold: broker.and_then(|b| b.refresh_hold_state(&cred, now_ms)),
        });
    }
    let store_prefix = format!("{}|", store.display());
    let mut selections: Vec<_> = broker
        .map(|b| {
            b.selections
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(k, s)| {
                    k.starts_with(&store_prefix)
                        && provider.is_none_or(|p| s.provider == p.as_str())
                        && query
                            .account
                            .as_deref()
                            .is_none_or(|label| selection_involves(s, label))
                })
                .map(|(_, s)| s.clone())
                .collect()
        })
        .unwrap_or_default();
    selections.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    Ok(AuthStateReport {
        schema_version: AUTH_STATE_SCHEMA_VERSION,
        live: false,
        broker_active: broker.is_some(),
        generated_at_ms: now_ms,
        store: store.display().to_string(),
        accounts,
        selections,
        cleared,
    })
}

/// `auth state --account LABEL` keeps a selection that chose `label` or
/// lists it among the accounts that failed.
fn selection_involves(s: &super::auth_state::SelectionState, label: &str) -> bool {
    s.chosen.as_deref() == Some(label)
        || s.failure
            .as_ref()
            .is_some_and(|f| f.accounts.iter().any(|a| a.account == label))
}

/// remote transport. There is no third option — and no direct-read fallback.
pub fn broker_from_source(
    source: &super::CredentialSource,
    cache: &super::TokenCache,
    http: reqwest::Client,
) -> Arc<dyn CredentialBroker> {
    match source {
        super::CredentialSource::Local => {
            // Runtime refresh and failover build adapters repeatedly. Keep
            // their local authority on the runtime-owned TokenCache, not a
            // global broker (which may belong to another remote principal).
            let key = (
                storage::auth_file_path(),
                crate::config::resolve_read_path("config"),
            );
            let mut brokers = cache
                .local_brokers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            brokers
                .entry(key)
                .or_insert_with(|| Arc::new(LocalBroker::new(http)))
                .clone()
        }
        super::CredentialSource::Remote {
            endpoint,
            machine_token,
        } => Arc::new(RemoteBroker::new(
            endpoint.clone(),
            machine_token.clone(),
            http,
            cache.clone(),
        )),
    }
}

/// [`broker_from_source`] for bounded single-attempt requests: a local
/// broker never follows redirects ([`LocalBroker::without_redirects`]), and
/// fails closed when that cannot be guaranteed. The remote broker's own RPC
/// refuses redirects, but its daemon dispatches upstream with an ordinary
/// broker, so callers must not send bounded inference through its proxy.
pub fn bounded_broker_from_source(
    source: &super::CredentialSource,
    cache: &super::TokenCache,
    http: reqwest::Client,
) -> Result<Arc<dyn CredentialBroker>, BrokerError> {
    match source {
        super::CredentialSource::Local => Ok(Arc::new(LocalBroker::new(http).without_redirects()?)),
        super::CredentialSource::Remote { .. } => Ok(broker_from_source(source, cache, http)),
    }
}

/// The process's HTTP client that never follows redirects, for bounded
/// single-attempt requests; `None` when it cannot be built (then no bounded
/// request may be sent).
pub fn redirect_free_http() -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .inspect_err(
                    |error| tracing::warn!(%error, "redirect-free broker client unavailable"),
                )
                .ok()
        })
        .clone()
}

fn redirect_free_unavailable() -> BrokerError {
    BrokerError::UnsupportedCapability {
        provider: "broker".into(),
        capability: "an HTTP client that refuses redirects".into(),
    }
}

static GLOBAL_BROKER: std::sync::RwLock<Option<Arc<dyn CredentialBroker>>> =
    std::sync::RwLock::new(None);

static GLOBAL_BROKER_INSTALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Install the process-wide broker (called once from runtime configuration).
/// Every call is counted ([`global_broker_install_count`]) and, when
/// `SYNAPS_MEM_TRACE=1`, logged — the daemon-mode S7 gate asserts the count
/// stays at 1 across subagent spawns.
pub fn set_global_broker(broker: Arc<dyn CredentialBroker>) {
    *GLOBAL_BROKER.write().expect("broker registry poisoned") = Some(broker);
    let n = GLOBAL_BROKER_INSTALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if crate::core::memstat::mem_trace_enabled() {
        tracing::info!(target: "agent_core::memstat", installs = n, "global broker installed");
    }
}

/// How many times [`set_global_broker`] has run in this process.
pub fn global_broker_install_count() -> u64 {
    GLOBAL_BROKER_INSTALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The process-wide broker. Defaults to the in-process [`LocalBroker`] so
/// normal local use never requires a separately launched daemon.
pub fn global_broker() -> Arc<dyn CredentialBroker> {
    if let Some(b) = GLOBAL_BROKER
        .read()
        .expect("broker registry poisoned")
        .clone()
    {
        return b;
    }
    let default: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::new(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_default(),
    ));
    let mut guard = GLOBAL_BROKER.write().expect("broker registry poisoned");
    if let Some(b) = guard.clone() {
        return b;
    }
    *guard = Some(default.clone());
    default
}

/// Legacy signature bridge: resolve an access token via the appropriate
/// broker for `source`. Kept for CLI paths (`synaps status`).
pub async fn broker_access_token(
    provider: OAuthProviderId,
    source: &super::CredentialSource,
    cache: &super::TokenCache,
    http: &reqwest::Client,
) -> Result<AccessToken, BrokerError> {
    broker_from_source(source, cache, http.clone())
        .access_token(provider)
        .await
}

/// Non-secret map of static key display statuses for every provider,
/// plus the local endpoint. For settings UI snapshots.
pub fn static_key_status_map() -> BTreeMap<String, StaticKeyStatus> {
    STATIC_PROVIDERS
        .iter()
        .map(|s| (s.key.to_string(), static_key_status(s.key)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A selection keeps the accounts of its first listing by storage key:
    /// an account added later is left out, one removed later drops out (the
    /// later entries move up a place), and an entry that names no account is
    /// kept in every pass.
    #[test]
    fn a_selection_keeps_its_first_listing_by_storage_key_through_churn() {
        let provider = OAuthProviderId::Anthropic;
        let named = |label: &str| AccountSummary::new(provider, &Account::named(label).unwrap());
        let nameless = AccountSummary {
            provider: "no-such-provider".to_string(),
            label: "work".to_string(),
            ..AccountSummary::default()
        };
        assert!(nameless.credential_ref().is_none());
        let mut reads = SelectionReads::new(0);
        let first = reads.keep_inventory(vec![named("a"), named("b"), nameless.clone()]);
        assert_eq!(first, vec![named("a"), named("b"), nameless.clone()]);
        // "b" is removed and "c" is added while the selection runs.
        let later = reads.keep_inventory(vec![named("a"), named("c"), nameless.clone()]);
        assert_eq!(later, vec![named("a"), nameless.clone()]);
        // "b" comes back: it is in the first listing, so it is kept again.
        let back = reads.keep_inventory(vec![named("a"), named("b"), named("c"), nameless.clone()]);
        assert_eq!(back, vec![named("a"), named("b"), nameless]);
    }

    #[test]
    fn login_availability_handles_named_only_and_explicit_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let provider = OAuthProviderId::Anthropic;
        let empty = AccountPolicy::new();
        assert!(!oauth_provider_logged_in_at(&path, &empty, provider));
        std::fs::write(&path, serde_json::json!({
            "anthropic@work": {"type":"oauth", "access":"synthetic-access",
                "refresh":"synthetic-refresh", "expires":1}
        }).to_string()).unwrap();
        // This is login inventory for UI, not an authorization/capacity check.
        assert!(oauth_provider_logged_in_at(&path, &empty, provider));
        for selector in [AccountSelector::Auto,
            AccountSelector::parse_or_invalid("../bad", "test"),
            AccountSelector::Account(Account::named("work").unwrap())] {
            let policy = AccountPolicy::new().with(provider, selector);
            assert!(oauth_provider_logged_in_at(&path, &policy, provider));
        }
        let missing = AccountPolicy::new().with(provider,
            AccountSelector::Account(Account::named("missing").unwrap()));
        assert!(!oauth_provider_logged_in_at(&path, &missing, provider));
        assert!(!oauth_provider_logged_in_at(&path, &empty, OAuthProviderId::OpenAiCodex));
        std::fs::write(&path, "{broken").unwrap();
        assert!(!oauth_provider_logged_in_at(&path, &empty, provider));
    }

    /// Spec §5.5: the pre-flight capability check is a pure function — it can
    /// be (and is) called before any credential lookup or network access. A
    /// tool-requiring request against any text-only cloud route must yield the
    /// typed unsupported-capability error; text-only requests pass.
    #[test]
    fn preflight_rejects_tool_requiring_cloud_routes_with_typed_error() {
        for provider in [
            CloudProviderId::AzureOpenAi,
            CloudProviderId::AwsBedrock,
            CloudProviderId::GoogleVertex,
        ] {
            let err = preflight_cloud_capability(provider, true)
                .expect_err("tool-requiring cloud route must fail pre-flight");
            assert_eq!(
                err,
                BrokerError::UnsupportedCapability {
                    provider: provider.to_string(),
                    capability: "tools".into(),
                }
            );
            assert!(err.to_string().contains("text-only"));
            assert_eq!(preflight_cloud_capability(provider, false), Ok(()));
        }
    }

    async fn spawn_ndjson(body: &'static str) -> String {
        use axum::{body::Body, routing::post, Router};
        use bytes::Bytes;
        let app = Router::new().route(
            "/cloud/invoke",
            post(move || async move {
                let chunks = body
                    .as_bytes()
                    .chunks(3)
                    .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(chunk)));
                axum::response::Response::builder()
                    .header("content-type", "application/x-ndjson")
                    .body(Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn remote_events(body: &'static str) -> Vec<Result<CloudEvent, BrokerError>> {
        use futures::StreamExt;
        let broker = RemoteBroker::new(
            spawn_ndjson(body).await,
            "opaque-machine-token",
            reqwest::Client::new(),
            super::super::TokenCache::new(),
        );
        broker
            .cloud_invoke(
                CloudProviderId::GoogleVertex,
                "ctx-opaque",
                "google-vertex/publishers/google/models/test",
                InvokeRequest {
                    messages: vec![],
                    tools: vec![],
                    stream: true,
                    options: Default::default(),
                },
            )
            .await
            .unwrap()
            .collect()
            .await
    }

    #[tokio::test]
    async fn remote_ndjson_requires_exactly_one_terminal_and_stops_after_error() {
        let valid =
            remote_events("{\"type\":\"text_delta\",\"delta\":\"hi\"}\n{\"type\":\"done\"}\n")
                .await;
        assert_eq!(valid.len(), 2);
        assert!(matches!(valid[1], Ok(CloudEvent::Done)));

        for body in [
            "{\"type\":\"text_delta\",\"delta\":\"hi\"}\n",
            "{\"type\":\"done\"}\n{\"type\":\"done\"}\n",
            "{\"type\":\"done\"}\n{\"type\":\"text_delta\",\"delta\":\"late\"}\n",
        ] {
            let events = remote_events(body).await;
            assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
            assert!(events.last().unwrap().is_err());
        }
    }

    /// The per-provider endpoint allowlist: a signed proxy request cannot be
    /// steered at other same-host endpoints (key management, billing, …).
    #[test]
    fn proxy_rejects_unlisted_same_host_paths() {
        for (provider, path) in [
            ("groq", "/v1/keys"),
            ("groq", "/admin"),
            ("openrouter", "/api/v1/auth/keys"),
            ("local", "/audio/speech"),
        ] {
            let req = ProxyRequest {
                provider: provider.into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            match req.validate() {
                Err(BrokerError::Denied(msg)) => {
                    assert!(msg.contains("allowlist"), "got: {msg}")
                }
                other => panic!("{provider} {path} must be denied, got {other:?}"),
            }
        }
        // The cataloged paths remain reachable.
        for path in ["/models", "/chat/completions"] {
            let req = ProxyRequest {
                provider: "groq".into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(req.validate().is_ok(), "{path} must be allowed");
        }
    }

    /// Request bodies above the broker buffering limit are rejected before
    /// any credential resolution or upstream contact.
    #[test]
    fn proxy_rejects_oversize_request_body() {
        let req = ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: Some(serde_json::json!({
                "blob": "x".repeat(MAX_PROXY_REQUEST_BYTES + 1)
            })),
            stream: false,
            body_bytes: None,
        };
        match req.validate() {
            Err(BrokerError::Denied(msg)) => assert!(msg.contains("byte"), "got: {msg}"),
            other => panic!("oversize body must be denied, got {other:?}"),
        }
    }

    /// Regression: the size cap must hold over the exact-byte handoff even
    /// when the JSON `body` is unset — `body_bytes` alone is what
    /// `LocalBroker` would send upstream, so it can never bypass the limit.
    #[test]
    fn proxy_rejects_oversize_body_bytes_without_json_body() {
        let req = ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
            body_bytes: Some(bytes::Bytes::from(vec![b'x'; MAX_PROXY_REQUEST_BYTES + 1])),
        };
        match req.validate() {
            Err(BrokerError::Denied(msg)) => assert!(msg.contains("byte"), "got: {msg}"),
            other => panic!("oversize body_bytes must be denied, got {other:?}"),
        }
    }

    /// The sanctioned constructor serializes once and keeps `body` and
    /// `body_bytes` coherent: same buffer returned to the caller (for
    /// digesting) and stored on the request, parsing back to the very value.
    #[test]
    fn post_json_exact_sets_coherent_body_and_bytes() {
        let value = serde_json::json!({"model": "m", "stream": true});
        let (req, digest_bytes) =
            ProxyRequest::post_json_exact("groq", "/chat/completions", value.clone(), true)
                .expect("serializable body must construct");
        assert_eq!(req.method, ProxyMethod::Post);
        assert!(req.stream);
        assert_eq!(req.body.as_ref(), Some(&value));
        let stored = req.body_bytes.as_ref().expect("bytes must be set");
        assert_eq!(stored, &digest_bytes, "caller digest bytes == wire bytes");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(stored).unwrap(),
            value
        );
        req.validate().expect("coherent request must validate");
    }

    /// A semantically divergent `body`/`body_bytes` pair (constructed by
    /// hand, bypassing `post_json_exact`) is rejected by validation in
    /// debug/test builds — the digest would not describe the claimed body.
    #[test]
    fn mismatched_body_bytes_rejected_by_debug_validation() {
        let req = ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: Some(serde_json::json!({"model": "claimed"})),
            stream: true,
            body_bytes: Some(bytes::Bytes::from_static(b"{\"model\":\"actually-sent\"}")),
        };
        match req.validate() {
            Err(BrokerError::Denied(msg)) => {
                assert!(msg.contains("body_bytes"), "got: {msg}");
            }
            other => panic!("incoherent handoff must be denied, got {other:?}"),
        }
    }

    #[test]
    fn proxy_request_validation_fails_closed() {
        let ok = ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(ok.validate().is_ok());

        let unknown = ProxyRequest {
            provider: "evil".into(),
            ..ok.clone()
        };
        assert!(matches!(
            unknown.validate(),
            Err(BrokerError::UnknownProvider(_))
        ));

        let absolute = ProxyRequest {
            path: "https://evil.example/x".into(),
            ..ok.clone()
        };
        assert!(matches!(absolute.validate(), Err(BrokerError::Denied(_))));

        let traversal = ProxyRequest {
            path: "/../secrets".into(),
            ..ok.clone()
        };
        assert!(matches!(traversal.validate(), Err(BrokerError::Denied(_))));

        let relative = ProxyRequest {
            path: "chat/completions".into(),
            ..ok
        };
        assert!(matches!(relative.validate(), Err(BrokerError::Denied(_))));
    }

    /// Managed Kimi Code proxy: chat inference plus the read-only catalog,
    /// profile, and quota GETs the official CLI uses — nothing else.
    #[test]
    fn proxy_allows_only_pinned_kimi_code_paths() {
        let chat = ProxyRequest {
            provider: "kimi-code".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: true,
            body_bytes: None,
        };
        assert!(chat.validate().is_ok(), "kimi-code chat must be allowed");

        for path in ["/models", "/me", "/usages"] {
            let get = ProxyRequest {
                provider: "kimi-code".into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(get.validate().is_ok(), "kimi-code GET {path} must pass");
            // The same surface must not accept POST (read-only endpoints).
            let post = ProxyRequest {
                method: ProxyMethod::Post,
                ..get
            };
            assert!(
                matches!(post.validate(), Err(BrokerError::Denied(_))),
                "kimi-code POST {path} must be denied"
            );
        }

        // Arbitrary same-host endpoints (feedback/upload/admin) stay denied.
        for path in ["/feedback", "/search", "/fetch", "/completions"] {
            let denied = ProxyRequest {
                provider: "kimi-code".into(),
                method: ProxyMethod::Post,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(
                matches!(denied.validate(), Err(BrokerError::Denied(_))),
                "kimi-code {path} must be denied"
            );
        }

        // `kimi` stays the static Moonshot API-key provider key — validating
        // it as an OAuth proxy id must keep resolving via static_provider.
        let static_kimi = ProxyRequest {
            provider: "kimi".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(static_kimi.validate().is_ok());
    }

    /// OAuth providers stay fail-closed except reviewed catalog paths.
    /// Claude alias remains unknown; anthropic/openai-codex only accept
    /// their exact catalog allowlists (not bare `/models`).
    #[test]
    fn proxy_rejects_oauth_providers_except_codex_catalog() {
        // Unrecognized OAuth alias is still unknown.
        let claude = ProxyRequest {
            provider: "claude".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(
            matches!(claude.validate(), Err(BrokerError::UnknownProvider(_))),
            "claude alias must not be proxyable"
        );
        // Anthropic bare OpenAI-style /models is denied (only /v1/models…).
        let anth = ProxyRequest {
            provider: "anthropic".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(
            matches!(anth.validate(), Err(BrokerError::Denied(_))),
            "anthropic /models must be denied"
        );
        // openai-codex bare /models (no client_version) must be denied.
        let bare = ProxyRequest {
            provider: "openai-codex".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(
            matches!(bare.validate(), Err(BrokerError::Denied(_))),
            "openai-codex /models without client_version must be denied"
        );
    }

    /// Codex catalog proxy is pinned to GET /codex/models?client_version=…
    /// only. Arbitrary same-host paths and query shapes are denied — including
    /// the ChatGPT web picker at `/models`, which is not a Codex catalog.
    #[test]
    fn proxy_allows_only_pinned_openai_codex_models_path() {
        let allowed = ProxyRequest {
            provider: "openai-codex".into(),
            method: ProxyMethod::Get,
            path: "/codex/models?client_version=0.153.3".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(
            allowed.validate().is_ok(),
            "exact codex models path must be allowed"
        );

        for path in [
            "/codex/models",
            "/codex/models?client_version=",
            "/codex/models?foo=1",
            "/codex/models?client_version=0.153.3&extra=1",
            "/codex/responses",
            "/models",
            "/models?client_version=0.153.3",
            "/backend-api/codex/models?client_version=0.153.3",
            "/v1/models?client_version=0.153.3",
            "/chat/completions",
        ] {
            let req = ProxyRequest {
                provider: "openai-codex".into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(
                matches!(req.validate(), Err(BrokerError::Denied(_))),
                "openai-codex path {path} must be denied"
            );
        }

        // POST is never allowed for the catalog path.
        let post = ProxyRequest {
            provider: "openai-codex".into(),
            method: ProxyMethod::Post,
            path: "/codex/models?client_version=0.153.3".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(
            matches!(post.validate(), Err(BrokerError::Denied(_))),
            "POST openai-codex models must be denied"
        );
    }

    /// Copilot is pinned to its catalog and two reviewed inference paths.
    #[test]
    fn proxy_allows_only_pinned_github_copilot_paths() {
        let models = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(models.validate().is_ok());

        let chat = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(chat.validate().is_ok());
        let responses = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: "/responses".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(responses.validate().is_ok());
        for path in ["/v1/messages", "/models?x=1", "/embeddings"] {
            let request = ProxyRequest {
                provider: "github-copilot".into(),
                method: ProxyMethod::Post,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(matches!(request.validate(), Err(BrokerError::Denied(_))));
        }
    }

    /// Anthropic catalog proxy is pinned to /v1/models (+ limit/after_id) only.
    #[test]
    fn proxy_allows_only_pinned_anthropic_models_path() {
        for path in [
            "/v1/models",
            "/v1/models?limit=100",
            "/v1/models?limit=100&after_id=claude-opus-4-7",
        ] {
            let req = ProxyRequest {
                provider: "anthropic".into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(req.validate().is_ok(), "{path} must be allowed");
        }
        for path in [
            "/v1/messages",
            "/models",
            "/v1/models?foo=1",
            "/v1/models?limit=",
            "/api/oauth/usage",
            "/v1/models?limit=100&evil=1",
            "/v1/models?limit=100&limit=50",
            "/v1/models?after_id=a&after_id=b",
            "/v1/models?limit=100&after_id=a&limit=50",
        ] {
            let req = ProxyRequest {
                provider: "anthropic".into(),
                method: ProxyMethod::Get,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(
                matches!(req.validate(), Err(BrokerError::Denied(_))),
                "anthropic path {path} must be denied"
            );
        }
    }

    #[test]
    fn anthropic_oauth_catalog_headers_are_bearer_version_beta_not_x_api_key() {
        let headers = anthropic_oauth_catalog_request_headers();
        let names: Vec<_> = headers.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"anthropic-version"));
        assert!(names.contains(&"anthropic-beta"));
        assert!(names.contains(&"accept"));
        assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("x-api-key")));
        assert!(!names
            .iter()
            .any(|n| n.eq_ignore_ascii_case("authorization")));
        let beta = headers
            .iter()
            .find(|(n, _)| *n == "anthropic-beta")
            .map(|(_, v)| *v)
            .expect("beta");
        assert_eq!(beta, "oauth-2025-04-20");
    }

    #[test]
    fn anthropic_models_path_round_trip_and_rejects_duplicates() {
        assert!(is_allowed_anthropic_path("/v1/models"));
        assert!(is_allowed_anthropic_path("/v1/models?limit=100"));
        assert!(is_allowed_anthropic_path(
            "/v1/models?limit=100&after_id=claude-opus-4-7"
        ));
        assert!(!is_allowed_anthropic_path("/v1/models?limit=100&limit=50"));
        assert!(!is_allowed_anthropic_path(
            "/v1/models?after_id=a&after_id=b"
        ));
        // Round-trip the catalog helper path shape used by the engine.
        let page0 = "/v1/models?limit=100";
        let page1 = "/v1/models?limit=100&after_id=model-1";
        assert!(is_allowed_anthropic_path(page0));
        assert!(is_allowed_anthropic_path(page1));
    }

    /// google-gemini is pinned to the reviewed cloudcode-pa v1internal methods.
    #[test]
    fn proxy_allows_only_pinned_google_gemini_paths() {
        for path in [
            "/v1internal:loadCodeAssist",
            "/v1internal:onboardUser",
            "/v1internal:streamGenerateContent",
            "/v1internal:countTokens",
            "/v1internal/operations/op-12345",
        ] {
            let req = ProxyRequest {
                provider: "google-gemini".into(),
                method: ProxyMethod::Post,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(req.validate().is_ok(), "{path} must be allowed");
        }
        for path in [
            // No arbitrary same-host methods.
            "/v1internal:listExperiments",
            "/v1internal:fetchAdminControls",
            "/v1internal:setCodeAssistGlobalUserSetting",
            "/v1internal:generateContent",
            // No unrelated versions.
            "/v2/models",
            "/v1beta/models",
            // No path traversal or root probe.
            "/",
            "/v1internal:",
        ] {
            let req = ProxyRequest {
                provider: "google-gemini".into(),
                method: ProxyMethod::Post,
                path: path.into(),
                body: None,
                stream: false,
                body_bytes: None,
            };
            assert!(
                matches!(req.validate(), Err(BrokerError::Denied(_))),
                "{path} must be denied"
            );
        }
    }

    #[test]
    fn access_token_type_has_no_refresh_field() {
        // Structural invariant: deserializing a broker response that includes
        // a refresh token silently drops it — there is no field to hold one.
        let t: AccessToken =
            serde_json::from_str(r#"{"token":"sk-x","expires":123,"refresh":"MUST-NOT-EXIST"}"#)
                .unwrap();
        let round = serde_json::to_value(&t).unwrap();
        assert_eq!(round.get("refresh"), None);
        assert_eq!(round["token"], "sk-x");
    }

    #[test]
    fn broker_error_display_never_echoes_values() {
        // Errors carry provider names and status text only.
        let e = BrokerError::NotConfigured("groq".into());
        assert!(!format!("{e}").contains("sk-"));
        let e = BrokerError::Unauthorized;
        let shown = format!("{e}");
        assert!(shown.starts_with("broker rejected machine auth"), "{shown}");
        // Actionable: names the config keys and the token file location.
        assert!(shown.contains("auth.machine_token") && shown.contains("SYNAPS_MACHINE_TOKEN"), "{shown}");
        assert!(shown.contains("auth-broker.token") && shown.contains("restart"), "{shown}");
    }

    #[test]
    fn mask_key_is_short_and_lossy() {
        assert_eq!(mask_key("gsk-live-1234567890abcdef"), "gsk-…cdef");
        assert_eq!(mask_key("short"), "…");
    }

    #[test]
    fn proxy_request_serde_roundtrip() {
        let req = ProxyRequest {
            provider: "openrouter".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: Some(serde_json::json!({"a": 1})),
            stream: true,
            body_bytes: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ProxyRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider, "openrouter");
        assert!(back.stream);
        assert!(matches!(back.method, ProxyMethod::Get));
    }

    // ── LocalBroker streaming/proxy behavior against a fake upstream ─────────

    async fn spawn_upstream(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Streaming forwarding: SSE bytes from the upstream arrive unmodified
    /// through `proxy_stream`, and the Authorization header applied is the
    /// broker's — the caller never supplied one.
    #[tokio::test]
    async fn local_broker_streams_local_endpoint_and_applies_key() {
        use axum::routing::post;
        let seen_auth = Arc::new(std::sync::Mutex::new(String::new()));
        let seen = seen_auth.clone();
        let app = axum::Router::new().route(
            "/chat/completions",
            post(move |headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"x\":1}\n\ndata: [DONE]\n\n",
                    )
                }
            }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let mut stream = broker
            .proxy_stream(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Post,
                path: "/chat/completions".into(),
                body: Some(serde_json::json!({"model": "m"})),
                stream: true,
                body_bytes: None,
            })
            .await
            .expect("stream must open");
        use futures::StreamExt;
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(collected).unwrap();
        assert!(text.contains("data: {\"x\":1}"));
        assert!(text.contains("[DONE]"));
        // Key applied broker-side (default local key), not by the caller.
        assert_eq!(&*seen_auth.lock().unwrap(), "Bearer local");
    }

    /// A refused connection is known not to have sent the request; an
    /// upstream error status after sending is not.
    #[tokio::test]
    async fn a_refused_connection_is_typed_as_not_sent() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", closed.local_addr().unwrap());
        drop(closed);
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url)
            .without_redirects()
            .unwrap();
        let request = ProxyRequest {
            provider: LOCAL_PROVIDER_KEY.into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: Some(serde_json::json!({"model": "m"})),
            stream: true,
            body_bytes: None,
        };
        let error = match broker.proxy_stream(request).await {
            Ok(_) => panic!("nothing listens there"),
            Err(error) => error,
        };
        assert!(matches!(error, BrokerError::ConnectFailed(_)), "{error:?}");
        assert!(error.request_not_sent());
        assert!(
            error.to_string().starts_with("broker transport error: "),
            "display unchanged: {error}"
        );

        for refused in [
            BrokerError::UnknownProvider("p".into()),
            BrokerError::NotConfigured("p".into()),
            BrokerError::Denied("policy".into()),
            BrokerError::Credential("expired".into()),
            BrokerError::Unauthorized,
        ] {
            assert!(refused.request_not_sent(), "{refused:?}");
        }
        assert!(!BrokerError::Transport("provider request failed: 400".into()).request_not_sent());
    }

    /// A loopback endpoint that reads one request and answers it with a
    /// redirect to `location`. Returns its base URL and the number of
    /// request bytes it received.
    async fn spawn_redirector(location: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = received.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 65_536];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                seen.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
                let reply = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}/v1"), received)
    }

    fn local_stream_request() -> ProxyRequest {
        ProxyRequest {
            provider: LOCAL_PROVIDER_KEY.into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: Some(serde_json::json!({"model": "m"})),
            stream: true,
            body_bytes: None,
        }
    }

    fn closed_port_url() -> String {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            closed.local_addr().unwrap()
        );
        drop(closed);
        url
    }

    /// A refused connection after a followed redirect is not proof that
    /// nothing was sent: the first hop already received the request.
    #[tokio::test]
    async fn a_refused_connection_after_a_redirect_is_not_typed_as_not_sent() {
        let (url, received) = spawn_redirector(closed_port_url()).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let error = match broker.proxy_stream(local_stream_request()).await {
            Ok(_) => panic!("the redirect target refuses"),
            Err(error) => error,
        };
        assert!(
            received.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the first hop received the request"
        );
        assert!(!error.request_not_sent(), "{error:?}");
        assert!(
            error.to_string().starts_with("broker transport error: "),
            "display unchanged: {error}"
        );
    }

    /// A redirect-free broker never follows the redirect: the 3xx is an
    /// answered (sent) request, and a direct refusal is still not-sent.
    #[tokio::test]
    async fn a_redirect_free_broker_types_only_a_direct_refusal_as_not_sent() {
        let (url, received) = spawn_redirector(closed_port_url()).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url)
            .without_redirects()
            .unwrap();
        let error = match broker.proxy_stream(local_stream_request()).await {
            Ok(_) => panic!("a redirect is not a stream"),
            Err(error) => error,
        };
        assert!(received.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert!(matches!(error, BrokerError::Transport(_)), "{error:?}");
        assert!(!error.request_not_sent());

        let refused = closed_port_url();
        let base = refused.trim_end_matches("/chat/completions").to_string();
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), base)
            .without_redirects()
            .unwrap();
        let error = match broker.proxy_stream(local_stream_request()).await {
            Ok(_) => panic!("nothing listens there"),
            Err(error) => error,
        };
        assert!(matches!(error, BrokerError::ConnectFailed(_)), "{error:?}");
        assert!(error.request_not_sent());
    }

    /// Without a redirect-free client there is no bounded broker at all: the
    /// failure is a pre-send refusal, never the original redirect-following
    /// broker.
    #[test]
    fn a_bounded_broker_fails_closed_without_a_redirect_free_client() {
        let error = match LocalBroker::new(reqwest::Client::new()).with_redirect_free_http(None) {
            Ok(_) => panic!("no broker without a redirect-free client"),
            Err(error) => error,
        };
        assert!(
            matches!(error, BrokerError::UnsupportedCapability { .. }),
            "{error:?}"
        );
        assert!(error.request_not_sent());
        // The public path uses the verified factory's client.
        assert!(redirect_free_http().is_some());
        let broker = LocalBroker::new(reqwest::Client::new())
            .without_redirects()
            .expect("the factory's redirect-free client");
        assert!(broker.redirect_free);
    }

    /// `request_not_sent` is an explicit allowlist of pre-send variants.
    #[test]
    fn only_typed_pre_send_failures_are_not_sent() {
        for refused in [
            BrokerError::UnknownProvider("p".into()),
            BrokerError::RegistrationRequired {
                provider: "p".into(),
                remediation: "r".into(),
            },
            BrokerError::NotConfigured("p".into()),
            BrokerError::Unauthorized,
            BrokerError::Denied("policy".into()),
            BrokerError::UnsupportedCapability {
                provider: "p".into(),
                capability: "tools".into(),
            },
            BrokerError::ConnectFailed("refused".into()),
            BrokerError::Credential("expired".into()),
            BrokerError::UnknownAccount {
                provider: "p".into(),
                label: "a".into(),
            },
            BrokerError::UnsupportedAccount {
                provider: "p".into(),
                label: "a".into(),
            },
            BrokerError::InvalidAccount("a".into()),
            BrokerError::NoAccountAvailable {
                provider: "p".into(),
                reason: "none".into(),
                failure: None,
            },
        ] {
            assert!(refused.request_not_sent(), "{refused:?}");
        }
        assert!(!BrokerError::Transport("reset".into()).request_not_sent());
    }

    /// A non-2xx upstream response never yields a stream — it becomes a typed
    /// error whose text is the provider's status/body (no key material).
    #[tokio::test]
    async fn local_broker_stream_error_is_typed_and_keyless() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "{\"error\":\"bad key\"}",
                )
            }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let err = match broker
            .proxy_stream(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Post,
                path: "/chat/completions".into(),
                body: None,
                stream: true,
                body_bytes: None,
            })
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("non-2xx upstream must not yield a stream"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("401"), "got: {msg}");
        assert!(
            !msg.to_lowercase().contains("bearer"),
            "no auth material in errors"
        );
    }

    /// Non-streaming proxy returns upstream status + body verbatim.
    #[tokio::test]
    async fn local_broker_proxy_returns_status_and_body() {
        use axum::routing::get;
        let app =
            axum::Router::new().route("/models", get(|| async { "{\"data\":[{\"id\":\"m1\"}]}" }));
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let resp = broker
            .proxy(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Get,
                path: "/models".into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("m1"));
    }

    /// A buffered response above the broker cap fails closed instead of
    /// ballooning memory or returning silently truncated JSON.
    #[tokio::test]
    async fn local_broker_proxy_rejects_oversize_response() {
        use axum::routing::get;
        let app = axum::Router::new().route("/models", get(|| async { "x".repeat(4096) }));
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url)
            .with_max_response_bytes(64);
        let err = broker
            .proxy(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Get,
                path: "/models".into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .expect_err("oversize body must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("limit"), "got: {msg}");
    }

    /// Upstream error bodies never reach the caller — a hostile provider
    /// cannot flood the caller or inject terminal escapes via error text.
    #[tokio::test]
    async fn local_broker_stream_error_is_status_only() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("\x1b[2J{}", "E".repeat(64 * 1024)),
                )
            }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let err = broker
            .proxy_stream(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Post,
                path: "/chat/completions".into(),
                body: None,
                stream: true,
                body_bytes: None,
            })
            .await
            .err()
            .expect("non-2xx upstream must not yield a stream");
        let msg = format!("{err}");
        assert!(
            msg.len() < 700,
            "error must be bounded, got {} bytes",
            msg.len()
        );
        assert!(!msg.contains('\x1b'), "control chars must be stripped");
        assert!(msg.contains("500"));
    }

    /// Buffered proxy requests carry an explicit time budget: a hung upstream
    /// becomes a typed transport error, not an indefinite stall.
    #[tokio::test]
    async fn local_broker_buffered_request_times_out() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/models",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "too late"
            }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url)
            .with_request_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let err = broker
            .proxy(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Get,
                path: "/models".into(),
                body: None,
                stream: false,
                body_bytes: None,
            })
            .await
            .expect_err("hung upstream must time out");
        assert!(matches!(err, BrokerError::Transport(_)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must be enforced promptly"
        );
    }

    /// Local in-process behavior: the process-wide broker exists without any
    /// daemon being launched and answers capability queries immediately.
    #[tokio::test]
    async fn global_broker_defaults_to_in_process_local() {
        let broker = global_broker();
        let caps = broker
            .capabilities()
            .await
            .expect("in-process broker needs no daemon");
        assert!(caps.iter().any(|c| c.key == LOCAL_PROVIDER_KEY));
        // Idempotent: repeated calls return an installed instance.
        let again = global_broker();
        assert!(again.capabilities().await.is_ok());
    }

    // ── Upstream/broker error-body disclosure (spec §5.1) ────────────────────
    //
    // An upstream may echo the full request (prompts, tool input_schema,
    // credentials) in its error body, and a compromised remote broker controls
    // every byte of its responses including JSON `error` fields. No
    // provider/broker-controlled bytes may reach BrokerError Display.

    /// Unique sentinel: if this appears in any error Display, provider bytes
    /// leaked across the broker boundary.
    const HOSTILE_SENTINEL: &str = "ZX9-HOSTILE-SENTINEL-7Q";

    /// A hostile error body shaped like an echoed request: sentinel, marker,
    /// and request-shaped fields a provider could reflect back.
    fn hostile_body() -> String {
        format!(
            "{{\"error\":{{\"message\":\"ECHOED {HOSTILE_SENTINEL} \
             {{\\\"messages\\\":[{{\\\"content\\\":\\\"my secret prompt\\\"}}],\
             \\\"tools\\\":[{{\\\"input_schema\\\":{{}}}}]}}\"}}}}"
        )
    }

    /// A hostile broker-style JSON body whose top-level `error` field is
    /// attacker-controlled (compromised remote broker).
    fn hostile_broker_json() -> String {
        format!(
            "{{\"error\":\"ECHOED {HOSTILE_SENTINEL} input_schema \
             my secret prompt\"}}"
        )
    }

    fn assert_no_hostile_bytes(msg: &str) {
        assert!(
            !msg.contains(HOSTILE_SENTINEL),
            "sentinel leaked into error: {msg}"
        );
        assert!(!msg.contains("ECHOED"), "echoed body leaked: {msg}");
        assert!(
            !msg.contains("input_schema"),
            "request-shaped data leaked: {msg}"
        );
        assert!(
            !msg.contains("secret prompt"),
            "prompt content leaked: {msg}"
        );
    }

    /// LocalBroker::proxy_stream: a non-2xx upstream body must never enter the
    /// error, while the stable `provider request failed: {status}` prefix and
    /// typed Transport category are retained for engine-side classification
    /// (including Gemini 429 detection on the status reason phrase).
    #[tokio::test]
    async fn local_broker_stream_error_omits_upstream_body() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    format!("\x1b[2J{}", hostile_body()),
                )
            }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let err = broker
            .proxy_stream(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Post,
                path: "/chat/completions".into(),
                body: None,
                stream: true,
                body_bytes: None,
            })
            .await
            .err()
            .expect("non-2xx upstream must not yield a stream");
        assert!(matches!(err, BrokerError::Transport(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("provider request failed: 429 Too Many Requests"),
            "status prefix must survive for engine classification: {msg}"
        );
        assert!(
            !msg.contains('\x1b'),
            "escapes must not reach errors: {msg}"
        );
        assert_no_hostile_bytes(&msg);
    }

    /// Drive `proxy_stream` against an upstream that answers `/chat/completions`
    /// with `status` + `body`, returning the Transport error message.
    async fn stream_error_message(status: axum::http::StatusCode, body: String) -> String {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/chat/completions",
            post(move || async move { (status, body) }),
        );
        let url = spawn_upstream(app).await;
        let broker = LocalBroker::with_local_base_url(reqwest::Client::new(), url);
        let err = broker
            .proxy_stream(ProxyRequest {
                provider: LOCAL_PROVIDER_KEY.into(),
                method: ProxyMethod::Post,
                path: "/chat/completions".into(),
                body: None,
                stream: true,
                body_bytes: None,
            })
            .await
            .err()
            .expect("non-2xx upstream must not yield a stream");
        assert!(matches!(err, BrokerError::Transport(_)));
        format!("{err}")
    }

    /// Everything after the `provider request failed: ` marker.
    fn after_marker(msg: &str) -> &str {
        let marker = "provider request failed: ";
        let idx = msg.find(marker).expect("marker present");
        &msg[idx + marker.len()..]
    }

    /// Kimi Code weekly-quota 403: the vetted `access_terminated_error`
    /// class is surfaced as OUR static label so callers can tell quota
    /// exhaustion from an auth failure — while none of the body's text
    /// reaches the error, and the suffix stays ':'-free so engine-side
    /// `redact_provider_proxy_error` / `broker_error_status` keep parsing.
    #[tokio::test]
    async fn local_broker_stream_403_kimi_quota_surfaces_vetted_label_only() {
        let kimi_body = "{\"error\":{\"type\":\"access_terminated_error\",\"message\":\
                         \"You've reached your weekly (7-day) usage limit. Your quota will \
                         reset when the current 7-day window ends. \"}}";
        let msg = stream_error_message(axum::http::StatusCode::FORBIDDEN, kimi_body.into()).await;
        assert!(
            msg.ends_with("provider request failed: 403 Forbidden [access_terminated_error]"),
            "expected exact labelled prefix: {msg}"
        );
        assert!(!msg.contains("weekly"), "body text leaked: {msg}");
        assert!(!msg.contains("quota"), "body text leaked: {msg}");
        assert!(!msg.contains('{'), "raw JSON leaked: {msg}");
        assert!(
            !after_marker(&msg).contains(':'),
            "suffix must not contain ':' (engine truncates there): {msg}"
        );
    }

    /// Hostile/unvetted `error.type` never becomes a label and its bytes
    /// never reach the error.
    #[tokio::test]
    async fn local_broker_stream_429_unvetted_type_yields_no_label() {
        let body = format!(
            "{{\"error\":{{\"type\":\"<script>ECHO:secret\",\"message\":\"ECHOED {HOSTILE_SENTINEL}\"}}}}"
        );
        let msg = stream_error_message(axum::http::StatusCode::TOO_MANY_REQUESTS, body).await;
        assert!(
            msg.ends_with("provider request failed: 429 Too Many Requests"),
            "no label expected: {msg}"
        );
        assert!(!msg.contains('['), "unexpected label: {msg}");
        assert!(!msg.contains("script"), "hostile type leaked: {msg}");
        assert!(!msg.contains("secret"), "hostile type leaked: {msg}");
        assert!(!after_marker(&msg).contains(':'), "got: {msg}");
        assert_no_hostile_bytes(&msg);
    }

    /// Non-JSON (HTML) error page → no label, no leak.
    #[tokio::test]
    async fn local_broker_stream_html_body_yields_no_label() {
        let body =
            format!("<html><body>ECHOED {HOSTILE_SENTINEL} access_terminated_error</body></html>");
        let msg = stream_error_message(axum::http::StatusCode::BAD_GATEWAY, body).await;
        assert!(
            msg.ends_with("provider request failed: 502 Bad Gateway"),
            "no label expected: {msg}"
        );
        assert!(!msg.contains("html"), "body leaked: {msg}");
        assert!(!msg.contains('['), "unexpected label: {msg}");
        assert_no_hostile_bytes(&msg);
    }

    /// Body larger than the classification cap: truncated read, no label,
    /// no panic, no error from the reader itself.
    #[tokio::test]
    async fn local_broker_stream_oversized_body_yields_no_label_without_panic() {
        // Valid vetted envelope, but padded far past the cap so the read is
        // cut off before the closing braces → cannot parse → no label.
        let pad = "x".repeat(MAX_PROXY_ERROR_CLASSIFY_BYTES * 4);
        let body = format!(
            "{{\"error\":{{\"message\":\"ECHOED {HOSTILE_SENTINEL} {pad}\",\"type\":\"access_terminated_error\"}}}}"
        );
        let msg = stream_error_message(axum::http::StatusCode::FORBIDDEN, body).await;
        assert!(
            msg.ends_with("provider request failed: 403 Forbidden"),
            "no label expected for oversized body: {msg}"
        );
        assert!(!msg.contains('['), "unexpected label: {msg}");
        assert!(
            msg.len() < 200,
            "oversized body leaked: {} bytes",
            msg.len()
        );
        assert_no_hostile_bytes(&msg);
    }

    /// The truncating reader itself: overflow returns the first `cap` bytes
    /// (never an error), and a multibyte char split at the cap yields `None`
    /// rather than panicking.
    #[tokio::test]
    async fn read_error_body_for_classification_truncates_and_never_fails() {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/big", get(|| async { "a".repeat(100) }))
            .route("/utf8", get(|| async { format!("{}é", "a".repeat(9)) }));
        let url = spawn_upstream(app).await;
        let client = reqwest::Client::new();

        let resp = client.get(format!("{url}/big")).send().await.unwrap();
        let got = read_error_body_for_classification(resp, 10).await;
        assert_eq!(got.as_deref(), Some("aaaaaaaaaa"));

        // 'é' is 2 bytes at offset 9..11; cap 10 splits it.
        let resp = client.get(format!("{url}/utf8")).send().await.unwrap();
        let got = read_error_body_for_classification(resp, 10).await;
        assert_eq!(got, None);

        // Under the cap: whole body.
        let resp = client.get(format!("{url}/utf8")).send().await.unwrap();
        let got = read_error_body_for_classification(resp, 1024).await;
        assert_eq!(got.as_deref(), Some("aaaaaaaaaé"));
    }

    /// LocalBroker::anthropic_usage: a non-2xx usage-endpoint body must never
    /// enter the error; operation + status survive. Serial: mutates
    /// `SYNAPS_BASE_DIR`, which other `#[serial]` tests also depend on.
    #[tokio::test]
    #[serial_test::serial]
    async fn local_broker_usage_error_omits_upstream_body() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/usage",
            get(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    hostile_body(),
                )
            }),
        );
        let url = spawn_upstream(app).await;

        // Provide fresh Anthropic OAuth creds behind an isolated base dir so
        // token resolution succeeds without touching the real auth store.
        // Redirect via HOME (not SYNAPS_BASE_DIR): the non-serial
        // `config::tests::test_base_dir` asserts only the `.synaps-cli`
        // suffix, which every HOME-derived base dir preserves.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        let creds = crate::auth::OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: String::new(),
            access: "test-access-token".into(),
            expires: crate::epoch_millis() + 3_600_000,
            account_id: None,
        };
        super::super::storage::save_provider_auth_at_test_hook(
            &base.join("auth.json"),
            "anthropic",
            &creds,
        )
        .unwrap();
        let old_base = std::env::var("SYNAPS_BASE_DIR").ok();
        let old_home = std::env::var("HOME").ok();
        std::env::remove_var("SYNAPS_BASE_DIR");
        std::env::set_var("HOME", dir.path());

        let broker = LocalBroker::new(reqwest::Client::new())
            .with_anthropic_usage_url(format!("{url}/usage"));
        let result = broker.anthropic_usage().await;

        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = old_base {
            std::env::set_var("SYNAPS_BASE_DIR", v);
        }

        let err = result.expect_err("non-2xx usage response must fail");
        assert!(matches!(err, BrokerError::Transport(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("usage request failed") && msg.contains("500"),
            "operation + status must survive: {msg}"
        );
        assert_no_hostile_bytes(&msg);
    }

    async fn spawn_remote_broker_error(status: u16) -> RemoteBroker {
        use axum::routing::{get, post};
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new()
            .route(
                "/proxy",
                post(move || async move { (code, hostile_broker_json()) }),
            )
            .route(
                "/usage",
                get(move || async move { (code, hostile_broker_json()) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        RemoteBroker::new(
            format!("http://{addr}"),
            "opaque-machine-token",
            reqwest::Client::new(),
            super::super::TokenCache::new(),
        )
    }

    fn groq_request(stream: bool) -> ProxyRequest {
        ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream,
            body_bytes: None,
        }
    }

    /// RemoteBroker 400/403: a compromised broker's `error` field must not
    /// reach the Denied Display; the typed Denied category survives.
    #[tokio::test]
    async fn remote_broker_denied_omits_broker_error_body() {
        for status in [400u16, 403] {
            let broker = spawn_remote_broker_error(status).await;
            let err = broker
                .proxy(groq_request(false))
                .await
                .expect_err("4xx broker response must fail");
            assert!(
                matches!(err, BrokerError::Denied(_)),
                "typed category must survive, got {err:?}"
            );
            let msg = format!("{err}");
            assert!(msg.contains(&status.to_string()), "status lost: {msg}");
            assert_no_hostile_bytes(&msg);
        }
    }

    /// R2-I1: Gemini can think silently for minutes before its first data
    /// chunk, so its no-redirect client idles longer than everyone else's.
    #[test]
    fn gemini_gets_the_longer_stream_idle_timeout() {
        let broker = LocalBroker::new(reqwest::Client::new());
        assert_eq!(broker.stream_idle_timeout_for("google-gemini"), Duration::from_secs(300));
        for key in ["muse", "kimi-code", "openai-codex", "anthropic"] {
            assert_eq!(broker.stream_idle_timeout_for(key), Duration::from_secs(90), "{key}");
        }
        let short = LocalBroker::new(reqwest::Client::new())
            .with_stream_idle_timeout_for_tests(Duration::from_millis(10));
        assert_eq!(short.stream_idle_timeout_for("muse"), Duration::from_millis(10));
        assert!(short.stream_idle_timeout_for("google-gemini") >= Duration::from_secs(300));
    }

    async fn spawn_remote_broker_body(status: u16, body: String) -> RemoteBroker {
        use axum::routing::post;
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().route(
            "/proxy",
            post(move || {
                let body = body.clone();
                async move { (code, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        RemoteBroker::new(
            format!("http://{addr}"),
            "opaque-machine-token",
            reqwest::Client::new(),
            super::super::TokenCache::new(),
        )
    }

    /// Round-2 item 4: the daemon's 502 for a retryable refresh failure
    /// (a Muse re-mint that hit the network) reaches a remote client as the
    /// typed retryable refresh error, so the "retry shortly" hint survives.
    /// Only the kind is taken from the body; the text is static, so a
    /// hostile broker still can't put its bytes into the Display.
    #[tokio::test]
    async fn remote_broker_502_keeps_a_retryable_refresh_failure() {
        use super::super::token::{RefreshFailure, RefreshFailureKind as K};
        for kind in [K::Transient, K::LockBusy, K::Other] {
            let daemon_text = BrokerError::Refresh(RefreshFailure::new(
                kind,
                format!("HTTP error 503 {HOSTILE_SENTINEL} ECHOED input_schema"),
            ))
            .to_string();
            let body = serde_json::json!({ "error": daemon_text }).to_string();
            let broker = spawn_remote_broker_body(502, body.clone()).await;
            let stream_err = broker.proxy_stream(groq_request(true)).await.err().unwrap();
            let buffered_err = broker.proxy(groq_request(false)).await.unwrap_err();
            for err in [stream_err, buffered_err] {
                let BrokerError::Refresh(f) = &err else {
                    panic!("{kind:?}: expected Refresh, got {err:?}");
                };
                assert_eq!(f.kind, kind);
                let msg = err.to_string();
                assert!(msg.contains("retry shortly"), "{msg}");
                assert_no_hostile_bytes(&msg);
            }
        }
        // Anything else in a 502 stays an opaque transport error.
        let broker = spawn_remote_broker_body(502, hostile_broker_json()).await;
        let err = broker.proxy_stream(groq_request(true)).await.err().unwrap();
        assert!(matches!(err, BrokerError::Transport(_)), "{err:?}");
        assert_no_hostile_bytes(&err.to_string());
    }

    /// RemoteBroker::proxy_stream: a non-2xx broker body must never enter the
    /// error; operation + status survive.
    #[tokio::test]
    async fn remote_broker_stream_error_omits_broker_body() {
        let broker = spawn_remote_broker_error(500).await;
        let err = broker
            .proxy_stream(groq_request(true))
            .await
            .err()
            .expect("non-2xx broker response must not yield a stream");
        assert!(matches!(err, BrokerError::Transport(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("broker proxy stream failed") && msg.contains("500"),
            "operation + status must survive: {msg}"
        );
        assert_no_hostile_bytes(&msg);
    }

    /// RemoteBroker::anthropic_usage: a non-2xx broker body must never enter
    /// the error; operation + status survive.
    #[tokio::test]
    async fn remote_broker_usage_error_omits_broker_body() {
        let broker = spawn_remote_broker_error(502).await;
        let err = broker
            .anthropic_usage()
            .await
            .expect_err("non-2xx broker response must fail");
        assert!(matches!(err, BrokerError::Transport(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("broker usage returned HTTP 502"),
            "operation + status must survive: {msg}"
        );
        assert_no_hostile_bytes(&msg);
    }

    /// Capability rows carry configured-ness only — the serialized form can
    /// never contain key material because there is no field for it.
    #[tokio::test]
    async fn capabilities_expose_no_secret_fields() {
        let broker = LocalBroker::new(reqwest::Client::new());
        let caps = broker.capabilities().await.unwrap();
        assert!(caps
            .iter()
            .any(|c| c.key == "anthropic" && c.kind == CredentialKind::OAuth));
        assert!(caps
            .iter()
            .any(|c| c.key == "groq" && c.kind == CredentialKind::StaticKey));
        let json = serde_json::to_string(&caps).unwrap();
        for field in ["key\":", "name\":", "kind\":", "configured\":"] {
            assert!(json.contains(field));
        }
        assert!(!json.contains("refresh"));
        assert!(!json.contains("access"));
    }

    /// No Anthropic refresh seam exists, so the refresh-failure → kind map
    /// is pinned here: only a rejected login is a login problem; transient,
    /// lock-busy, local and cooldown failures are "retry", action-required
    /// is neither.
    #[test]
    fn refresh_failure_kinds_map_to_observation_kinds() {
        use super::super::quota_policy::{AccountFailureKind as K, QuotaObservation};
        use super::super::token::{RefreshFailure, RefreshFailureKind as R};
        for (kind, expected, transient, login) in [
            (R::Transient, K::RefreshTransient, true, false),
            (R::LockBusy, K::RefreshLockBusy, true, false),
            (R::Other, K::RefreshOther, true, false),
            (
                R::Cooldown { retry_in_secs: 42 },
                K::RefreshOther,
                true,
                false,
            ),
            (R::Credential, K::RefreshRejected, false, true),
            (R::ActionRequired, K::RefreshActionRequired, false, false),
        ] {
            let read = UsageReadError::refresh(RefreshFailure::new(kind, "why"));
            assert_eq!(read.failure.kind, expected, "{kind:?}");
            assert_eq!(read.failure.kind.is_transient(), transient, "{kind:?}");
            assert_eq!(read.failure.kind.needs_login(), login, "{kind:?}");
            // `usage()` keeps its historical text for every refresh failure.
            assert_eq!(
                read.error,
                BrokerError::Credential("why".into()),
                "{kind:?}"
            );
            let observation = read.failure.observation();
            if login {
                assert_eq!(observation, QuotaObservation::AuthError);
            } else {
                assert_eq!(observation, QuotaObservation::Unknown);
            }
        }
        let cooldown =
            refresh_failure(&RefreshFailure::new(R::Cooldown { retry_in_secs: 42 }, "x"));
        assert_eq!(cooldown.retry_after_s, Some(42));
        // Slot errors: vanished vs unreadable store.
        let gone = UsageReadError::slot(BrokerError::UnknownAccount {
            provider: "anthropic".into(),
            label: "x".into(),
        });
        assert_eq!(gone.failure.kind, K::CredentialMissing);
        assert!(gone.failure.kind.needs_login());
        let store = UsageReadError::slot(BrokerError::Credential("unreadable".into()));
        assert_eq!(store.failure.kind, K::CredentialStore);
        assert!(!store.failure.kind.needs_login() && !store.failure.kind.is_transient());
    }

    /// Every usage error keeps a kind; 429 keeps its Retry-After and the
    /// status codes survive.
    #[test]
    fn usage_errors_map_to_observation_kinds_with_status() {
        use super::super::quota_policy::AccountFailureKind as K;
        use super::super::usage::UsageError as U;
        for (err, kind, status) in [
            (
                U::RateLimited {
                    retry_after_s: Some(9),
                },
                K::UsageRateLimited,
                Some(429),
            ),
            (
                U::Unauthorized { status: 403 },
                K::UsageUnauthorized,
                Some(403),
            ),
            (
                U::UpstreamStatus {
                    status: 503,
                    retry_after_s: None,
                },
                K::UsageUpstream,
                Some(503),
            ),
            (U::Redirected { status: 302 }, K::UsageUpstream, Some(302)),
            (U::Timeout, K::UsageTimeout, None),
            (
                U::Transport {
                    detail: "connect".into(),
                },
                K::UsageTransport,
                None,
            ),
            (U::Malformed { detail: "x".into() }, K::UsageMalformed, None),
            (U::BodyTooLarge { cap: 1 }, K::UsageMalformed, None),
            (U::AccountIdUnavailable, K::UsageUnauthorized, None),
            (
                U::UnsupportedProvider {
                    provider: "p".into(),
                },
                K::Unsupported,
                None,
            ),
        ] {
            let read = UsageReadError::usage(err.clone());
            assert_eq!(read.failure.kind, kind, "{err:?}");
            assert_eq!(read.failure.http_status, status, "{err:?}");
            assert_eq!(read.error, err.clone().into_broker_error(), "{err:?}");
        }
        assert_eq!(
            UsageReadError::usage(U::RateLimited {
                retry_after_s: Some(9)
            })
            .failure
            .retry_after_s,
            Some(9)
        );
    }
}
