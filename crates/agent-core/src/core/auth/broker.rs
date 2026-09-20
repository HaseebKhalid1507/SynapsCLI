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

use super::account::{Account, AccountPolicy, AccountSelector, AccountSummary, CredentialRef};
use super::cloud::{CloudProviderId, InvokeRequest};
use super::provider::OAuthProviderId;
use super::static_providers::{
    allowed_proxy_paths, static_provider, StaticProviderSpec, LOCAL_DEFAULT_BASE_URL,
    LOCAL_PROVIDER_KEY, STATIC_PROVIDERS,
};
use super::{load_provider_auth, storage};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Transport-level failure talking to the broker or the provider.
    Transport(String),
    /// Credential storage/refresh failure (message is already secret-free).
    Credential(String),
    /// An explicitly requested account slot does not exist. Never a fallback.
    UnknownAccount { provider: String, label: String },
    /// This broker implementation cannot address named account slots.
    UnsupportedAccount { provider: String, label: String },
    /// An account label failed validation (CLI/env/config/HTTP boundary).
    InvalidAccount(String),
    /// Automatic selection found no account with proven capacity.
    NoAccountAvailable { provider: String, reason: String },
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
            Self::Unauthorized => write!(f, "broker rejected machine auth"),
            Self::Denied(msg) => write!(f, "broker denied request: {msg}"),
            Self::UnsupportedCapability {
                provider,
                capability,
            } => write!(
                f,
                "cloud provider '{provider}' is text-only: {capability} are not supported yet \
                 (no credential was used and no network request was made)"
            ),
            Self::Transport(msg) => write!(f, "broker transport error: {msg}"),
            Self::Credential(msg) => write!(f, "credential error: {msg}"),
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
            Self::NoAccountAvailable { provider, reason } => {
                write!(f, "no account with proven capacity for '{provider}': {reason}")
            }
        }
    }
}

impl std::error::Error for BrokerError {}

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
    /// Time budget for buffered (non-streaming) requests.
    request_timeout: Duration,
    /// Buffered response size cap.
    max_response_bytes: usize,
    cloud_backend: Option<Arc<dyn CloudBackend>>,
    /// Injected account policy. `None` → resolved from env + config on every
    /// selection (no global mutation, picks up `synaps auth use` changes).
    account_policy: Option<AccountPolicy>,
    /// In-memory cooldowns for automatic selection, keyed by storage key.
    cooldowns: Arc<std::sync::Mutex<BTreeMap<String, Cooldown>>>,
    /// Test seam: overrides the usage endpoint for typed usage snapshots.
    usage_endpoint_override: Option<String>,
    /// Max age of a usage observation that still counts as capacity.
    max_snapshot_age: Duration,
    /// Short-lived cache of the last usage snapshot per storage key so
    /// automatic selection does not re-poll every provider on every token
    /// vend. Entries older than `max_snapshot_age` are never reused.
    snapshots: SharedMap<super::usage::UsageSnapshot>,
    /// Single-flight gates for usage fetches, per storage key.
    snapshot_gates: SharedMap<Arc<tokio::sync::Mutex<()>>>,
    /// Cached account policy keyed by the config file's modification time
    /// (env overlay is applied on every read; it is cheap and has no I/O).
    policy_cache: Arc<std::sync::Mutex<Option<CachedPolicy>>>,
}

/// Process-shared map keyed by storage key.
type SharedMap<T> = Arc<std::sync::Mutex<BTreeMap<String, T>>>;
/// Config-derived policy tagged with the config file mtime it was read at.
type CachedPolicy = (Option<std::time::SystemTime>, AccountPolicy);

/// A reported provider limit for one account (non-secret).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cooldown {
    until_ms: u64,
    reason: String,
}

/// Cooldown applied when a limit is reported without a reset hint.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// Longest cooldown a client report may impose (a weekly window plus slack);
/// anything longer is clamped so a bad report cannot bench a seat forever.
const MAX_COOLDOWN: Duration = Duration::from_secs(8 * 24 * 60 * 60);
/// Default staleness bound for automatic selection. Short on purpose: a
/// selection is a spend decision and should rest on a fresh reading.
pub const DEFAULT_MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(60);

impl LocalBroker {
    pub fn new(http: reqwest::Client) -> Self {
        let cloud_backend = Arc::new(ProductionCloudBackend::new(http.clone()));
        Self {
            http,
            local_base_url: None,
            anthropic_usage_url: None,
            google_gemini_base_url: None,
            openai_codex_base_url: None,
            request_timeout: PROXY_REQUEST_TIMEOUT,
            max_response_bytes: MAX_PROXY_RESPONSE_BYTES,
            cloud_backend: Some(cloud_backend),
            account_policy: None,
            cooldowns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            usage_endpoint_override: None,
            max_snapshot_age: DEFAULT_MAX_SNAPSHOT_AGE,
            snapshots: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            snapshot_gates: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            policy_cache: Arc::new(std::sync::Mutex::new(None)),
        }
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

    /// Test seam: point the pinned ChatGPT backend host at a loopback fake.
    /// Only relaxes the base URL; allowlists, bearer/header pairing and
    /// redirect denial are unchanged.
    #[doc(hidden)]
    pub fn with_openai_codex_base_url_for_tests(mut self, base_url: impl Into<String>) -> Self {
        self.openai_codex_base_url = Some(base_url.into().trim_end_matches('/').to_string());
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
        let credential = self.select_auto(provider, model).await?;
        let token = self.access_token_for(&credential).await?;
        Ok(PinnedToken { credential, token })
    }

    /// Test seam: point typed usage fetches at a fake server.
    #[doc(hidden)]
    pub fn with_usage_endpoint_override(mut self, url: impl Into<String>) -> Self {
        self.usage_endpoint_override = Some(url.into());
        self
    }

    /// Bound on usage-snapshot age for automatic selection.
    pub fn with_max_snapshot_age(mut self, age: Duration) -> Self {
        self.max_snapshot_age = age;
        self
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

    /// Active cooldown for a storage key (expired entries are dropped).
    fn cooldown_until(&self, storage_key: &str, now_ms: u64) -> Option<u64> {
        let mut map = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(storage_key) {
            Some(c) if c.until_ms > now_ms => Some(c.until_ms),
            Some(_) => {
                map.remove(storage_key);
                None
            }
            None => None,
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

    /// Resolve the credential for a request: explicit account (validated,
    /// never a fallback) or the policy (`auto` → capacity selection).
    async fn resolve_request_credential(
        &self,
        provider: OAuthProviderId,
        explicit: Option<Account>,
        model: Option<&str>,
    ) -> Result<CredentialRef, BrokerError> {
        if let Some(account) = explicit {
            return Ok(CredentialRef::new(provider, account));
        }
        match self.policy().selector(provider) {
            AccountSelector::Account(account) => Ok(CredentialRef::new(provider, account)),
            AccountSelector::Auto => self.select_auto(provider, model).await,
            AccountSelector::Invalid { source, reason } => {
                Err(BrokerError::InvalidAccount(format!("{source}: {reason}")))
            }
        }
    }

    fn cached_snapshot(&self, key: &str) -> Option<super::usage::UsageSnapshot> {
        let max_age_ms = self.max_snapshot_age.as_millis() as u64;
        let now_ms = crate::epoch_millis();
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .filter(|s| !s.is_stale(now_ms, max_age_ms))
            .cloned()
    }

    fn snapshot_gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.snapshot_gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Fresh usage snapshot for `cred`: the short-lived cache is consulted
    /// first (never past `max_snapshot_age`), otherwise ONE read-only fetch
    /// per credential at a time (single-flight; concurrent vends share it).
    async fn fresh_usage(
        &self,
        cred: &CredentialRef,
    ) -> Result<super::usage::UsageSnapshot, BrokerError> {
        let key = cred.storage_key();
        if let Some(cached) = self.cached_snapshot(&key) {
            return Ok(cached);
        }
        let gate = self.snapshot_gate(&key);
        let _held = gate.lock().await;
        if let Some(cached) = self.cached_snapshot(&key) {
            return Ok(cached);
        }
        let snapshot = self.usage(cred).await?;
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, snapshot.clone());
        Ok(snapshot)
    }

    /// Capacity view for one account from a fresh, read-only usage snapshot.
    /// Anything that is not a well-formed, fresh reading is NOT capacity.
    async fn capacity_for(&self, cred: &CredentialRef) -> super::quota_policy::AccountCapacity {
        use super::quota_policy::AccountCapacity;
        match self.fresh_usage(cred).await {
            Ok(snapshot) => capacity_from_snapshot(cred, &snapshot),
            Err(err) => AccountCapacity {
                credential: cred.clone(),
                observed_at_ms: None,
                observation: observation_from_error(&err),
                cooldown_until_ms: None,
            },
        }
    }

    /// Automatic selection: fresh, read-only usage for every stored account
    /// of `provider`, fed to the pure capacity policy. Fails closed — no
    /// account is advertised without proven capacity for `model`.
    async fn select_auto(
        &self,
        provider: OAuthProviderId,
        model: Option<&str>,
    ) -> Result<CredentialRef, BrokerError> {
        use super::quota_policy::{select, Selection, SelectionRequest, Strategy};
        let summaries = storage::list_accounts(provider).map_err(BrokerError::Credential)?;
        if summaries.is_empty() {
            return Err(BrokerError::NoAccountAvailable {
                provider: provider.as_str().to_string(),
                reason: "no stored accounts".into(),
            });
        }
        let mut candidates = Vec::with_capacity(summaries.len());
        for summary in &summaries {
            let Some(cred) = summary.credential_ref() else {
                continue;
            };
            candidates.push(self.capacity_for(&cred).await);
        }
        // Evaluation clock is taken AFTER the (sequential) fetches so the
        // freshest observation is never judged "in the future".
        let now_ms = crate::epoch_millis();
        for capacity in &mut candidates {
            capacity.cooldown_until_ms =
                self.cooldown_until(&capacity.credential.storage_key(), now_ms);
        }
        let preference: Vec<String> = summaries.iter().map(|s| s.label.clone()).collect();
        let request = SelectionRequest {
            model,
            preference: &preference,
            strategy: Strategy::LowestUtilization,
            ..SelectionRequest::new(provider, now_ms, self.max_snapshot_age.as_millis() as u64)
        };
        match select(&request, &candidates) {
            Selection::Selected { credential, .. } => Ok(credential),
            Selection::NoCapacity {
                rejections,
                earliest_reset_ms,
            } => {
                let detail: Vec<String> = rejections
                    .iter()
                    .map(|r| format!("{}: {:?}", r.credential.account, r.reason))
                    .collect();
                let reset = earliest_reset_ms
                    .map(|t| format!("; earliest reset at {t}"))
                    .unwrap_or_default();
                Err(BrokerError::NoAccountAvailable {
                    provider: provider.as_str().to_string(),
                    reason: format!("{}{reset}", detail.join(", ")),
                })
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
        if let Some(provider) = oauth_provider {
            if provider_key != LOCAL_PROVIDER_KEY && static_provider(provider_key).is_none() {
                let cred = self
                    .resolve_request_credential(provider, explicit_account, None)
                    .await?;
                let token = self.access_token_for(&cred).await?;
                pinned = Some(PinnedToken {
                    credential: cred,
                    token,
                });
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
        let url = format!("{base}{}", request.path);
        // Deny redirects for credential-bearing OAuth catalog traffic and
        // google-gemini: a 3xx must not replay the bearer token off-origin.
        let mut builder = if provider_key == "google-gemini"
            || provider_key == "openai-codex"
            || provider_key == "anthropic"
            || provider_key == "kimi-code"
        {
            let no_redirect = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
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
        builder = builder.bearer_auth(&key);
        if provider_key == "openai-codex" {
            // ChatGPT backend requires the account id. Derive it from the SAME
            // credential that produced the bearer: the JWT claim first, then
            // that slot's stored `accountId`. Never another slot, never omitted.
            let cred = pinned
                .as_ref()
                .map(|p| p.credential.clone())
                .unwrap_or_else(|| CredentialRef::default_for(OAuthProviderId::OpenAiCodex));
            let account_id = super::extract_codex_account_id(&key)
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
            if e.is_connect() && provider_key == LOCAL_PROVIDER_KEY {
                BrokerError::Transport(format!(
                    "can't reach local endpoint at {url} — is Ollama/LM Studio running?"
                ))
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
    }
}

/// A usage failure is never capacity; classify it for the policy's report.
fn observation_from_error(err: &BrokerError) -> super::quota_policy::QuotaObservation {
    use super::quota_policy::QuotaObservation;
    match err {
        BrokerError::UnsupportedCapability { .. } | BrokerError::UnsupportedAccount { .. } => {
            QuotaObservation::Unsupported
        }
        BrokerError::Credential(_) | BrokerError::Unauthorized | BrokerError::UnknownAccount { .. } => {
            QuotaObservation::AuthError
        }
        BrokerError::Transport(msg) if msg.contains("malformed") || msg.contains("body_too_large") => {
            QuotaObservation::Malformed
        }
        _ => QuotaObservation::Unknown,
    }
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
/// (OAuth login or `ANTHROPIC_API_KEY`). Non-secret answer for first-run UX.
pub fn anthropic_credential_available() -> bool {
    let oauth = load_provider_auth(OAuthProviderId::Anthropic.as_str())
        .ok()
        .flatten()
        .map(|c| c.auth_type == "oauth" && !c.access.is_empty())
        .unwrap_or(false);
    oauth || std::env::var("ANTHROPIC_API_KEY").is_ok_and(|v| !v.is_empty())
}

/// True if the OAuth provider has stored refreshable credentials. This is a
/// non-secret availability query for UI surfaces; an expired access token is
/// still a valid login when the broker can refresh it on first use.
pub fn oauth_provider_logged_in(provider: OAuthProviderId) -> bool {
    load_provider_auth(provider.as_str())
        .ok()
        .flatten()
        .is_some_and(|creds| {
            creds.auth_type == "oauth" && (!creds.refresh.is_empty() || !creds.access.is_empty())
        })
}

#[async_trait]
impl CredentialBroker for LocalBroker {
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Ok(self.access_token_pinned(provider).await?.token)
    }

    fn account_selector(&self, provider: OAuthProviderId) -> AccountSelector {
        self.policy().selector(provider)
    }

    async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        self.check_slot_exists(cred)?;
        let creds = super::ensure_fresh_credential(&self.http, cred)
            .await
            .map_err(BrokerError::Credential)?;
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
        let credential = self.resolve_request_credential(provider, None, model).await?;
        let token = self.access_token_for(&credential).await?;
        Ok(PinnedToken { credential, token })
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
        use super::usage::{fetch_usage, supports_usage, UsageClient, UsageError, UsageFetchOptions};
        if !supports_usage(cred.provider) {
            return Err(UsageError::UnsupportedProvider {
                provider: cred.provider.as_str().to_string(),
            }
            .into_broker_error());
        }
        // Token resolution happens HERE, behind the boundary, for exactly
        // this credential; the usage helper pairs any account header with
        // the same token.
        let token = self.access_token_for(cred).await?;
        let client = UsageClient::new().map_err(UsageError::into_broker_error)?;
        let opts = UsageFetchOptions {
            endpoint_override: self.usage_endpoint_override.clone(),
            ..UsageFetchOptions::default()
        };
        fetch_usage(
            &client,
            cred.provider,
            cred.account.label_str(),
            &token.token,
            &opts,
        )
        .await
        .map_err(UsageError::into_broker_error)
    }

    async fn report_cooldown(
        &self,
        cred: &CredentialRef,
        until_ms: Option<u64>,
        reason: &str,
    ) -> Result<(), BrokerError> {
        let now_ms = crate::epoch_millis();
        let max_until = now_ms.saturating_add(MAX_COOLDOWN.as_millis() as u64);
        let until_ms = until_ms
            .filter(|t| *t > now_ms)
            .unwrap_or(now_ms + DEFAULT_COOLDOWN.as_millis() as u64)
            .min(max_until);
        let reason = crate::truncate_str(reason, 64).to_string();
        tracing::info!(credential = %cred, until_ms, reason = %reason, "account cooldown reported");
        let key = cred.storage_key();
        self.cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), Cooldown { until_ms, reason });
        // The cached reading predates the limit report; drop it so the next
        // selection re-reads instead of trusting stale headroom.
        self.snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
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
    machine_token: String,
    cache: super::TokenCache,
    /// Injected account policy; `None` → env + config per call.
    account_policy: Option<AccountPolicy>,
}

impl RemoteBroker {
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
        Self {
            http,
            endpoint,
            machine_token: machine_token.into(),
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
        super::BrokerClient::with_client(
            self.endpoint.clone(),
            self.machine_token.clone(),
            self.http.clone(),
        )
    }

    /// Cache scope: endpoint + machine-principal digest (never the token).
    fn scope(&self) -> String {
        super::credential_source::source_scope(&self.endpoint, &self.machine_token)
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
            .http
            .get(format!("{}/token", self.endpoint))
            .query(&query)
            .bearer_auth(&self.machine_token)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => return Err(BrokerError::Unauthorized),
            503 => {
                drop(resp);
                return Err(BrokerError::NoAccountAvailable {
                    provider: provider.as_str().to_string(),
                    reason: "broker reported no account with proven capacity".into(),
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

    async fn post_proxy(&self, request: &ProxyRequest) -> Result<reqwest::Response, BrokerError> {
        request.validate()?;
        let resp = self
            .http
            .post(format!("{}/proxy", self.endpoint))
            .bearer_auth(&self.machine_token)
            .json(request)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
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
        Ok(caps
            .into_iter()
            .find(|c| c.key == provider.as_str())
            .map(|c| c.accounts)
            .unwrap_or_default())
    }

    async fn usage(&self, cred: &CredentialRef) -> Result<super::usage::UsageSnapshot, BrokerError> {
        let resp = self
            .http
            .get(format!("{}/usage/snapshot", self.endpoint))
            .query(&[
                ("provider", cred.provider.as_str()),
                ("account", cred.account.label_str()),
            ])
            .bearer_auth(&self.machine_token)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(format!("broker request failed: {e}")))?;
        match resp.status().as_u16() {
            401 => Err(BrokerError::Unauthorized),
            404 => {
                drop(resp);
                Err(BrokerError::UnknownAccount {
                    provider: cred.provider.as_str().to_string(),
                    label: cred.account.label_str().to_string(),
                })
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
            .http
            .post(format!("{}/accounts/cooldown", self.endpoint))
            .bearer_auth(&self.machine_token)
            .json(&serde_json::json!({
                "provider": cred.provider.as_str(),
                "account": cred.account.label_str(),
                "until_ms": until_ms,
                "reason": crate::truncate_str(reason, 64),
            }))
            .send()
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
            return Err(BrokerError::Transport(format!(
                "broker proxy returned HTTP {}",
                resp.status()
            )));
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
            // Broker-controlled error body: dropped unread (spec §5.1).
            drop(resp);
            return Err(BrokerError::Transport(format!(
                "broker proxy stream failed: {status}"
            )));
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
            .http
            .get(format!("{}/usage", self.endpoint))
            .bearer_auth(&self.machine_token)
            .send()
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
        let resp = self.http.post(format!("{}/cloud/catalog", self.endpoint)).bearer_auth(&self.machine_token).json(&serde_json::json!({"provider":provider,"context_ref":context_ref,"allow_stale":allow_stale})).send().await.map_err(|e| BrokerError::Transport(e.to_string()))?;
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
        let resp = self.http.post(format!("{}/cloud/invoke", self.endpoint)).bearer_auth(&self.machine_token).json(&serde_json::json!({"provider":provider,"context_ref":context_ref,"model_id":model_id,"request":request})).send().await.map_err(|e| BrokerError::Transport(e.to_string()))?;
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
            .http
            .get(format!("{}/capabilities", self.endpoint))
            .bearer_auth(&self.machine_token)
            .send()
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

/// remote transport. There is no third option — and no direct-read fallback.
pub fn broker_from_source(
    source: &super::CredentialSource,
    cache: &super::TokenCache,
    http: reqwest::Client,
) -> Arc<dyn CredentialBroker> {
    match source {
        super::CredentialSource::Local => Arc::new(LocalBroker::new(http)),
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
        assert_eq!(format!("{e}"), "broker rejected machine auth");
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
}
