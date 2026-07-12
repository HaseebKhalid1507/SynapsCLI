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
/// Upstream error bodies are diagnostics, not payload: read at most this much.
pub const MAX_UPSTREAM_ERROR_BYTES: usize = 2 * 1024;
/// Character cap for sanitized upstream error snippets surfaced in messages.
const MAX_ERROR_SNIPPET_CHARS: usize = 512;
/// Total time budget for a buffered (non-streaming) broker-executed request.
pub const PROXY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Fixed Anthropic usage endpoint — a typed broker operation, not a general
/// proxy target. Callers cannot vary the URL or path.
const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

// ── Errors ───────────────────────────────────────────────────────────────────

/// Broker failures. Messages never contain credential values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    /// The named provider is not in any broker registry.
    UnknownProvider(String),
    /// The provider is known but has no credential configured.
    NotConfigured(String),
    /// The caller failed broker (machine) authentication.
    Unauthorized,
    /// The request was rejected by broker policy (e.g. malformed proxy path).
    Denied(String),
    /// Transport-level failure talking to the broker or the provider.
    Transport(String),
    /// Credential storage/refresh failure (message is already secret-free).
    Credential(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProvider(p) => write!(f, "unknown provider: {p}"),
            Self::NotConfigured(p) => write!(
                f,
                "no credential configured for '{p}'. Run `synaps login` to add one."
            ),
            Self::Unauthorized => write!(f, "broker rejected machine auth"),
            Self::Denied(msg) => write!(f, "broker denied request: {msg}"),
            Self::Transport(msg) => write!(f, "broker transport error: {msg}"),
            Self::Credential(msg) => write!(f, "credential error: {msg}"),
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
    /// Static provider key (e.g. `groq`) or `local`.
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
}

impl ProxyRequest {
    /// Validate provider identity and path shape. Fail closed on anything
    /// that could redirect a broker-owned credential.
    pub fn validate(&self) -> Result<(), BrokerError> {
        if self.provider != LOCAL_PROVIDER_KEY
            && self.provider != "xai-auth"
            && self.provider != "github-copilot"
            && static_provider(&self.provider).is_none()
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
        if !(self.provider == "xai-auth" && self.path == "/responses")
            && !(self.provider == "github-copilot" && self.path == "/models")
            && !allowed_proxy_paths(&self.provider).contains(&self.path.as_str())
        {
            return Err(BrokerError::Denied(format!(
                "proxy path '{}' is not in the provider's endpoint allowlist",
                self.path
            )));
        }
        if let Some(body) = &self.body {
            let size = serde_json::to_vec(body)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if size > MAX_PROXY_REQUEST_BYTES {
                return Err(BrokerError::Denied(format!(
                    "request body exceeds the {MAX_PROXY_REQUEST_BYTES}-byte broker limit"
                )));
            }
        }
        Ok(())
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
    /// Whether a credential is available through the broker.
    pub configured: bool,
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

/// The typed credential broker protocol. One implementation runs in-process
/// ([`LocalBroker`]) so normal local use needs no daemon; the other talks to a
/// remote `synaps auth-broker` over authenticated HTTP(S) ([`RemoteBroker`]).
#[async_trait]
pub trait CredentialBroker: Send + Sync {
    /// Vend a fresh OAuth access token (token + expiry ONLY).
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError>;

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

    /// Non-secret provider capability/status list.
    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError>;
}

// ── Local (in-process) broker ────────────────────────────────────────────────

/// In-process credential broker. This module is the ONLY place runtime-serving
/// code may read `auth.json`, `provider.<key>` config values, or credential
/// environment variables.
pub struct LocalBroker {
    http: reqwest::Client,
    /// Test seam: overrides the local endpoint URL without env/config.
    local_base_url: Option<String>,
    /// Test seam: overrides the pinned Anthropic usage URL.
    anthropic_usage_url: Option<String>,
    /// Time budget for buffered (non-streaming) requests.
    request_timeout: Duration,
    /// Buffered response size cap.
    max_response_bytes: usize,
}

impl LocalBroker {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            local_base_url: None,
            anthropic_usage_url: None,
            request_timeout: PROXY_REQUEST_TIMEOUT,
            max_response_bytes: MAX_PROXY_RESPONSE_BYTES,
        }
    }

    /// Test/embedding seam: pin the `local` provider endpoint explicitly.
    pub fn with_local_base_url(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            local_base_url: Some(base_url.into()),
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
        let (key, base) = if request.provider == "xai-auth" {
            let token = self.access_token(OAuthProviderId::Xai).await?;
            (token.token, "https://api.x.ai/v1".to_string())
        } else if request.provider == "github-copilot" {
            // Catalog-only OAuth proxy: short-lived Copilot session token only.
            // Never attach the GitHub user token (stored as OAuth refresh).
            let token = self
                .access_token(OAuthProviderId::GitHubCopilot)
                .await?;
            (
                token.token,
                super::github_copilot_models_base_url().to_string(),
            )
        } else {
            (
                self.resolve_static_key(&request.provider)?,
                self.base_url_for(&request.provider)?,
            )
        };
        let url = format!("{base}{}", request.path);
        let mut builder = match request.method {
            ProxyMethod::Get => self.http.get(&url),
            ProxyMethod::Post => self.http.post(&url),
        };
        builder = builder.bearer_auth(&key);
        if request.provider == "github-copilot" {
            for (name, value) in super::github_copilot_models_request_headers() {
                builder = builder.header(*name, *value);
            }
        }
        if let Some(body) = &request.body {
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
            if e.is_connect() && request.provider == LOCAL_PROVIDER_KEY {
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

/// Bounded, sanitized snippet of an upstream error body. Reads at most
/// [`MAX_UPSTREAM_ERROR_BYTES`], then truncates/sanitizes — arbitrary
/// upstream content is never propagated at full size into error messages.
async fn upstream_error_snippet(resp: reqwest::Response) -> String {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = MAX_UPSTREAM_ERROR_BYTES.saturating_sub(buf.len());
        if room == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    sanitize_error_text(&String::from_utf8_lossy(&buf))
}

/// Strip control characters and cap the length of upstream error text so a
/// hostile body cannot inject terminal escapes or flood logs/UI.
fn sanitize_error_text(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_ERROR_SNIPPET_CHARS)
        .collect();
    if s.chars().count() > MAX_ERROR_SNIPPET_CHARS {
        out.push('…');
    }
    out
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

/// True if the OAuth provider has a stored, unexpired credential. Non-secret
/// login-status query for UI surfaces; the credential itself stays behind the
/// broker boundary.
pub fn oauth_provider_logged_in(provider: OAuthProviderId) -> bool {
    load_provider_auth(provider.as_str())
        .ok()
        .flatten()
        .is_some_and(|creds| !super::is_token_expired(&creds))
}

#[async_trait]
impl CredentialBroker for LocalBroker {
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        let creds = super::ensure_fresh_provider_token(&self.http, provider)
            .await
            .map_err(BrokerError::Credential)?;
        // Strip to token + expiry: the refresh token stays behind the boundary.
        Ok(AccessToken {
            token: creds.access,
            expires: creds.expires,
        })
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
            let snippet = upstream_error_snippet(resp).await;
            return Err(BrokerError::Transport(format!(
                "provider request failed: {status}: {snippet}"
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
            let snippet = upstream_error_snippet(resp).await;
            return Err(BrokerError::Transport(format!(
                "usage request failed: {status}: {snippet}"
            )));
        }
        let body = read_body_capped(resp, self.max_response_bytes).await?;
        serde_json::from_str(&body)
            .map_err(|e| BrokerError::Transport(format!("invalid usage response: {e}")))
    }

    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        let mut out = Vec::new();
        for descriptor in super::provider::registry().iter() {
            out.push(ProviderStatus {
                key: descriptor.id.as_str().to_string(),
                name: descriptor.display_name.to_string(),
                kind: CredentialKind::OAuth,
                configured: load_provider_auth(descriptor.id.as_str())
                    .ok()
                    .flatten()
                    .is_some(),
            });
        }
        for spec in STATIC_PROVIDERS {
            out.push(ProviderStatus {
                key: spec.key.to_string(),
                name: spec.name.to_string(),
                kind: CredentialKind::StaticKey,
                configured: static_key_configured(spec.key),
            });
        }
        out.push(ProviderStatus {
            key: LOCAL_PROVIDER_KEY.to_string(),
            name: "Local endpoint".to_string(),
            kind: CredentialKind::LocalEndpoint,
            configured: true,
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
}

impl RemoteBroker {
    pub fn new(
        endpoint: impl Into<String>,
        machine_token: impl Into<String>,
        http: reqwest::Client,
        cache: super::TokenCache,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            machine_token: machine_token.into(),
            cache,
        }
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
            400 | 403 => {
                let body = upstream_error_snippet(resp).await;
                Err(BrokerError::Denied(sanitize_broker_error(&body)))
            }
            _ => Ok(resp),
        }
    }
}

/// Extract the `error` field from a broker JSON error body; the broker only
/// ever puts static, secret-free strings there.
fn sanitize_broker_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| "request rejected".to_string())
}

#[async_trait]
impl CredentialBroker for RemoteBroker {
    async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        let fetcher = super::BrokerClient::with_client(
            self.endpoint.clone(),
            self.machine_token.clone(),
            self.http.clone(),
        );
        let tok = super::resolve_remote(
            &fetcher,
            &self.cache,
            provider.as_str(),
            super::DEFAULT_MARGIN_MS,
        )
        .await
        .map_err(BrokerError::Transport)?;
        Ok(AccessToken {
            token: tok.access_token,
            expires: tok.expires,
        })
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
            let body = upstream_error_snippet(resp).await;
            return Err(BrokerError::Transport(format!(
                "broker proxy stream failed: {status}: {}",
                sanitize_broker_error(&body)
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
                let body = upstream_error_snippet(resp).await;
                Err(BrokerError::Transport(format!(
                    "broker usage returned HTTP {s}: {}",
                    sanitize_broker_error(&body)
                )))
            }
            _ => {
                let body = read_body_capped(resp, MAX_PROXY_RESPONSE_BYTES).await?;
                serde_json::from_str(&body)
                    .map_err(|e| BrokerError::Transport(format!("invalid usage response: {e}")))
            }
        }
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

/// Install the process-wide broker (called once from runtime configuration).
pub fn set_global_broker(broker: Arc<dyn CredentialBroker>) {
    *GLOBAL_BROKER.write().expect("broker registry poisoned") = Some(broker);
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
        };
        match req.validate() {
            Err(BrokerError::Denied(msg)) => assert!(msg.contains("byte"), "got: {msg}"),
            other => panic!("oversize body must be denied, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_error_text_strips_controls_and_caps_length() {
        let hostile = format!("\x1b[2Jevil{}\x07", "A".repeat(4096));
        let out = sanitize_error_text(&hostile);
        assert!(!out.contains('\x1b') && !out.contains('\x07'));
        assert!(out.chars().count() <= MAX_ERROR_SNIPPET_CHARS + 1);
        assert!(out.ends_with('…'), "truncation must be visible");
    }

    #[test]
    fn proxy_request_validation_fails_closed() {
        let ok = ProxyRequest {
            provider: "groq".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
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

    /// OAuth provider keys are NOT valid proxy targets — static-key proxying
    /// and OAuth vending stay separate strategies (cross-provider isolation).
    #[test]
    fn proxy_rejects_oauth_providers() {
        for provider in ["anthropic", "openai-codex", "claude"] {
            let req = ProxyRequest {
                provider: provider.into(),
                method: ProxyMethod::Get,
                path: "/models".into(),
                body: None,
                stream: false,
            };
            assert!(
                matches!(req.validate(), Err(BrokerError::UnknownProvider(_))),
                "{provider} must not be proxyable"
            );
        }
    }

    /// C2 catalog-only: github-copilot may proxy GET /models (session token),
    /// but not inference paths yet.
    #[test]
    fn proxy_allows_github_copilot_models_only() {
        let models = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
        };
        assert!(models.validate().is_ok());

        let chat = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: "/chat/completions".into(),
            body: None,
            stream: false,
        };
        assert!(
            matches!(chat.validate(), Err(BrokerError::Denied(_))),
            "chat must remain deny-listed until inference slice"
        );
        let responses = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: "/responses".into(),
            body: None,
            stream: false,
        };
        assert!(matches!(responses.validate(), Err(BrokerError::Denied(_))));
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
            })
            .await
            .expect_err("oversize body must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("limit"), "got: {msg}");
    }

    /// Upstream error bodies are truncated and sanitized — a hostile provider
    /// cannot flood the caller or inject terminal escapes via error text.
    #[tokio::test]
    async fn local_broker_stream_error_body_is_truncated_and_sanitized() {
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
