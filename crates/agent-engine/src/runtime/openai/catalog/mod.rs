//! Normalized model catalog types and provider-specific parsers.
//!
//! This module is intentionally parser-first: unit tests exercise static JSON
//! fixtures only. Live network fetches are thin wrappers around these parsers.
//!
//! ## Provider research notes (spec appendix)
//!
//! **OpenRouter** `GET https://openrouter.ai/api/v1/models` — no auth required.
//! Metadata: id, name, context_length, supported_parameters, pricing, top_provider,
//! architecture.input_modalities. Reasoning detected from supported_parameters:
//!   - "reasoning"/"include_reasoning" => OpenRouter reasoning request
//!   - "reasoning_effort"              => effort-style (o-series via OR)
//!   - "verbosity"                     => Anthropic-style through OR
//!   - pricing.internal_reasoning      => Gemini thinking-token pricing
//!
//! **Groq** `GET https://api.groq.com/openai/v1/models` — Bearer auth.
//! Fields: id, active, context_window, owned_by. No reasoning in wire.
//!
//! **NVIDIA NIM** `GET https://integrate.api.nvidia.com/v1/models` — no auth for list.
//! Minimal: id, object, created, owned_by. Thinking via system-prompt injection.
//!
//! **Anthropic** `GET https://api.anthropic.com/v1/models` — paginated, Bearer/x-api-key.
//! Optional capabilities.thinking / capabilities.effort.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ANTHROPIC_MODELS_MAX_PAGES: usize = 20;

mod anthropic;
pub mod capability_cache;
mod codex;
mod generic;
mod github_copilot;
mod google_gemini;
mod groq;
pub mod kimi;
mod nvidia;
mod openrouter;
pub mod validation;
mod xai;

pub use anthropic::{
    anthropic_mode_capabilities, anthropic_models_url, anthropic_static_capability,
    merge_catalog_pages, parse_anthropic_catalog_models, parse_anthropic_catalog_page,
    plan_anthropic_execution, plan_standard_anthropic_transport, AnthropicCatalogPage,
    AnthropicExecutionMode, AnthropicExecutionPlan, AnthropicPlanError, AnthropicPlanErrorCode,
    AnthropicPlanPrerequisites, AnthropicWireEffort, AnthropicWorkflowPlan,
};
pub use codex::{
    codex_models_path, codex_models_url, codex_static_capability, codex_static_catalog_models,
    parse_codex_catalog_models, plan_codex_execution, validate_codex_level, CodexCapabilitySource,
    CodexExecutionMode, CodexExecutionPlan, CodexMultiAgentMode, CodexPlanError,
    CodexPlanErrorCode, CodexRequestRole, CodexWireEffort, ExecutionRole,
    PROVIDER_KEY as CODEX_PROVIDER_KEY, PROVIDER_NAME as CODEX_PROVIDER_NAME,
};
pub use generic::parse_generic_catalog_models;
pub use github_copilot::{
    copilot_model, copilot_static_catalog_models, models_request_headers,
    parse_copilot_catalog_entries, parse_copilot_catalog_models,
    preferred_wire_protocol_from_endpoints, runtime_wire_protocol as github_copilot_runtime_model,
    selectable_copilot_entries, validate_models_endpoint, CopilotCatalogEntry, CopilotEndpoint,
    CopilotModelDescriptor, CopilotPolicyState, CopilotWire, COPILOT_API_VERSION,
    COPILOT_FALLBACK_MODELS, MAX_MODELS_BODY_BYTES, MODELS_BASE_URL, MODELS_PATH, MODELS_URL,
    PROVIDER_KEY as COPILOT_PROVIDER_KEY, PROVIDER_NAME as COPILOT_PROVIDER_NAME,
};
pub use google_gemini::{
    google_gemini_model, google_gemini_static_catalog_models, GoogleGeminiModelDescriptor,
    GOOGLE_GEMINI_TEXT_MODELS, PROVIDER_KEY as GOOGLE_GEMINI_PROVIDER_KEY,
    PROVIDER_NAME as GOOGLE_GEMINI_PROVIDER_NAME,
};
pub use groq::{infer_groq_reasoning, parse_groq_catalog_models};
pub use kimi::{apply_kimi_reasoning_params, kimi_static_capability, KimiReasoningCapability};
pub use nvidia::{infer_nvidia_reasoning, parse_nvidia_catalog_models};
pub use openrouter::parse_openrouter_catalog_models;
pub use xai::{
    xai_model, xai_static_capability, xai_static_catalog_models, XaiModelDescriptor,
    XaiReasoningCapability, XAI_TEXT_MODELS,
};

// ─── Modality ────────────────────────────────────────────────────────────────

/// Input/output modality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    File,
    Other(String),
}

impl Modality {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "text" => Modality::Text,
            "image" => Modality::Image,
            "audio" => Modality::Audio,
            "video" => Modality::Video,
            "file" => Modality::File,
            other => Modality::Other(other.to_string()),
        }
    }
}

// ─── PricingSummary ───────────────────────────────────────────────────────────

/// Pricing metadata. Stored as decimal-string USD/token as returned by OpenRouter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PricingSummary {
    /// USD per prompt token, decimal string.
    pub prompt: Option<String>,
    /// USD per completion token, decimal string.
    pub completion: Option<String>,
    /// Separate Gemini internal-reasoning token cost (OpenRouter).
    pub internal_reasoning: Option<String>,
}

impl PricingSummary {
    /// True when a non-zero internal_reasoning price is present.
    pub fn has_internal_reasoning_cost(&self) -> bool {
        self.internal_reasoning
            .as_deref()
            .map(|s| s != "0" && !s.trim().is_empty())
            .unwrap_or(false)
    }
}

// ─── ReasoningSupport ─────────────────────────────────────────────────────────

/// Multi-agent protocol version advertised by the exact OpenAI Codex model.
///
/// Unknown server strings are retained only as this sanitized sentinel; raw
/// catalog values never enter diagnostics or authorization decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexMultiAgentVersion {
    V1,
    V2,
    Unknown,
}

impl CodexMultiAgentVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
            Self::Unknown => "unknown",
        }
    }
}

/// Normalized reasoning/thinking capability for a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningSupport {
    /// No reasoning/thinking support confirmed.
    None,
    /// Anthropic adaptive: thinking:{type:"adaptive"} ± effort param.
    AnthropicAdaptive { adaptive: bool },
    /// OpenRouter: reasoning/include_reasoning/reasoning_effort/verbosity params.
    OpenRouter {
        include_reasoning: bool,
        effort: bool,
        verbosity: bool,
        internal_reasoning_priced: bool,
    },
    /// Groq family-based reasoning (reasoning_format/reasoning_effort).
    GroqReasoning,
    /// NVIDIA inline thinking via system-prompt; <think> in content.
    NvidiaInlineThinking,
    /// Generic OpenAI-compatible (capability unknown).
    GenericOpenAi,
    /// OpenAI Codex (ChatGPT OAuth): named effort levels, exact set from catalog.
    CodexNamed {
        /// Ordered list of supported named reasoning effort strings from the
        /// live catalog's `supported_reasoning_levels[].effort` field.
        supported: Vec<agent_core::reasoning::ReasoningLevel>,
        /// Default level from catalog's `default_reasoning_level`, if present.
        default_level: Option<agent_core::reasoning::ReasoningLevel>,
        /// Exact model's collaboration protocol. Ultra requires V2.
        multi_agent_version: Option<CodexMultiAgentVersion>,
    },
    /// Not yet classified.
    Unknown,
}

// ─── CatalogSource ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogSource {
    /// From a live provider API call.
    Live,
    /// Bundled static/seed data.
    StaticFallback,
    /// Static seed enriched with live fields.
    StaticWithLive,
    /// Capability inferred heuristically.
    Inferred,
}

// ─── CatalogProviderKind ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogProviderKind {
    Anthropic,
    OpenRouter,
    Groq,
    NvidiaNim,
    OpenAiCodex,
    Generic { key: String },
    Local,
}

// ─── CatalogModel ─────────────────────────────────────────────────────────────

/// Normalized model catalog entry. Every provider handler produces these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// Provider key (e.g. "openrouter", "groq").
    pub provider_key: String,
    /// Human-readable provider name.
    pub provider_name: String,
    /// Provider kind for routing/capability dispatch.
    pub provider_kind: CatalogProviderKind,
    /// Model id as used in API requests (no provider prefix).
    pub id: String,
    /// Human-readable label.
    pub label: Option<String>,
    /// Input context window in tokens.
    pub context_tokens: Option<u64>,
    /// Maximum output tokens.
    pub max_output_tokens: Option<u64>,
    /// Input modalities.
    pub input_modalities: Vec<Modality>,
    /// Pricing summary.
    pub pricing: PricingSummary,
    /// Reasoning/thinking capability.
    pub reasoning: ReasoningSupport,
    /// Data provenance.
    pub source: CatalogSource,
}

impl CatalogModel {
    /// Construct a minimal entry, returning `None` if the id is blank.
    pub fn new(
        provider_key: impl Into<String>,
        provider_name: impl Into<String>,
        id: impl Into<String>,
    ) -> Option<Self> {
        let id = id.into();
        if id.trim().is_empty() {
            return None;
        }
        let pk = provider_key.into();
        Some(Self {
            provider_kind: CatalogProviderKind::Generic { key: pk.clone() },
            provider_name: provider_name.into(),
            provider_key: pk,
            id,
            label: None,
            context_tokens: None,
            max_output_tokens: None,
            input_modalities: vec![Modality::Text],
            pricing: PricingSummary::default(),
            reasoning: ReasoningSupport::Unknown,
            source: CatalogSource::Live,
        })
    }

    /// Synaps runtime id. Provider identity is always explicit, including Anthropic.
    pub fn runtime_id(&self) -> String {
        format!("{}/{}", self.provider_key, self.id)
    }

    /// Label if present, id otherwise.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// For Codex models: returns the exact ordered set of supported named levels,
    /// or `None` if this model has no Codex-named capability data.
    pub fn codex_supported_levels(&self) -> Option<&[agent_core::reasoning::ReasoningLevel]> {
        match &self.reasoning {
            ReasoningSupport::CodexNamed { supported, .. } => Some(supported),
            _ => None,
        }
    }

    /// Exact Codex collaboration protocol, if the authoritative capability
    /// record advertised one.
    pub fn codex_multi_agent_version(&self) -> Option<CodexMultiAgentVersion> {
        match &self.reasoning {
            ReasoningSupport::CodexNamed {
                multi_agent_version,
                ..
            } => *multi_agent_version,
            _ => None,
        }
    }

    /// For Codex models: true iff the given level is in the supported set.
    /// Always returns false for models without Codex-named capability data.
    pub fn codex_supports_level(&self, level: agent_core::reasoning::ReasoningLevel) -> bool {
        self.codex_supported_levels()
            .is_some_and(|levels| levels.contains(&level))
    }
}

// ─── Static seed helper ───────────────────────────────────────────────────────

/// Build a static-fallback CatalogModel from a (id, label) pair.
pub fn from_static_seed(
    provider_key: &str,
    provider_name: &str,
    id: &str,
    label: &str,
) -> Option<CatalogModel> {
    let mut m = CatalogModel::new(provider_key, provider_name, id)?;
    m.label = if label.trim().is_empty() {
        None
    } else {
        Some(label.to_string())
    };
    m.source = CatalogSource::StaticFallback;
    m.reasoning = ReasoningSupport::Unknown;
    Some(m)
}

/// Convert all static seeds in a ProviderSpec to CatalogModel entries.
pub fn static_seeds_from_spec(spec: &super::registry::ProviderSpec) -> Vec<CatalogModel> {
    spec.models
        .iter()
        .filter_map(|(id, label, _tier)| from_static_seed(spec.key, spec.name, id, label))
        .collect()
}

// ─── Live fetch helpers ───────────────────────────────────────────────────────

pub trait ModelCatalogProvider: Sync {
    fn provider_key(&self) -> &'static str;

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>>;
}

pub struct OpenRouterCatalogProvider;
pub struct GroqCatalogProvider;
pub struct NvidiaCatalogProvider;
pub struct AnthropicCatalogProvider;
pub struct CodexCatalogProvider;
pub struct XaiCatalogProvider;
pub struct GitHubCopilotCatalogProvider;
pub struct GoogleGeminiCatalogProvider;
pub struct GenericCatalogProvider;

pub fn catalog_provider_for(provider_key: &str) -> &'static dyn ModelCatalogProvider {
    match provider_key {
        "openrouter" => &OpenRouterCatalogProvider,
        "groq" => &GroqCatalogProvider,
        "nvidia" => &NvidiaCatalogProvider,
        "claude" | "anthropic" => &AnthropicCatalogProvider,
        "openai-codex" => &CodexCatalogProvider,
        "xai-auth" => &XaiCatalogProvider,
        "github-copilot" => &GitHubCopilotCatalogProvider,
        "google-gemini" => &GoogleGeminiCatalogProvider,
        _ => &GenericCatalogProvider,
    }
}

fn catalog_get(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client.get(url).timeout(CATALOG_REQUEST_TIMEOUT)
}

async fn read_catalog_response(resp: reqwest::Response) -> Result<String, String> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("model list failed: HTTP {status}"));
    }
    Ok(body)
}

async fn fetch_anthropic_catalog_models(
    _client: &reqwest::Client,
) -> Result<Vec<CatalogModel>, String> {
    // Paginated live discovery through the broker-owned Anthropic credential.
    // Tokens never enter this module — each page is a allowlisted ProxyRequest
    // for `/v1/models` (+ limit/after_id query).
    let mut pages = Vec::new();
    let mut after_id: Option<String> = None;

    for _ in 0..ANTHROPIC_MODELS_MAX_PAGES {
        let path = anthropic_models_proxy_path(after_id.as_deref());
        let body = broker_proxy_catalog_body("anthropic", &path).await?;
        let page = parse_anthropic_catalog_page(&body).map_err(|e| format!("parse failed: {e}"))?;
        let next_after_id = page.last_id.clone();
        let has_more = page.has_more && next_after_id.is_some();
        pages.push(page.models);
        if !has_more {
            return Ok(merge_catalog_pages(pages));
        }
        after_id = next_after_id;
    }

    Ok(merge_catalog_pages(pages))
}

/// Relative Anthropic models path for the broker allowlist (no host).
pub fn anthropic_models_proxy_path(after_id: Option<&str>) -> String {
    // Keep query shape identical to `anthropic_models_url` without the host.
    let absolute = anthropic_models_url(after_id);
    absolute
        .strip_prefix("https://api.anthropic.com")
        .unwrap_or(&absolute)
        .to_string()
}

impl ModelCatalogProvider for OpenRouterCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "openrouter"
    }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { fetch_openrouter_catalog_models(client).await })
    }
}

impl ModelCatalogProvider for GroqCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "groq"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move {
            let body = broker_catalog_models_body("groq").await?;
            parse_groq_catalog_models(&body).map_err(|e| format!("parse failed: {e}"))
        })
    }
}

impl ModelCatalogProvider for NvidiaCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "nvidia"
    }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move {
            let resp = catalog_get(client, "https://integrate.api.nvidia.com/v1/models")
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let body = read_catalog_response(resp).await?;
            parse_nvidia_catalog_models(&body).map_err(|e| format!("parse failed: {e}"))
        })
    }
}

impl ModelCatalogProvider for AnthropicCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "claude"
    }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { fetch_anthropic_catalog_models(client).await })
    }
}

/// True when a broker catalog error means "no credential / not logged in",
/// so static seeds are an acceptable offline fallback.
///
/// Transport, HTTP status, parse, denial, and credential-shape failures must
/// NOT fall through to static seeds — they are real errors.
pub fn is_missing_credential_catalog_error(err: &str) -> bool {
    // Prefer structured BrokerError Display forms when present.
    // Note: broker_proxy_catalog_body wraps broker errors as
    // `request failed: {BrokerError}` — so "request failed" alone is NOT
    // a transport signal.
    let lower = err.to_lowercase();

    // Hard excludes: real operational failures must never fall back to seeds.
    if lower.contains("model list failed")
        || lower.contains("parse failed")
        || lower.contains("broker denied")
        || lower.contains("broker transport error")
        || lower.contains("missing chatgpt account id")
        || lower.contains("http 4")
        || lower.contains("http 5")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
    {
        return false;
    }

    // Stable BrokerError::Credential prefix from token.rs load-miss:
    // "credential error: No credentials for {provider} at {path}. Run `synaps login`."
    // Match only the stable prefix so account-shape / other Credential variants
    // (e.g. missing chatgpt account id) remain hard errors above.
    lower.contains("credential error: no credentials for ")
        || lower.contains("no credential configured")
        || lower.contains("unknown provider:")
        || lower.contains("not logged")
        || lower.contains("registration required")
}

impl ModelCatalogProvider for CodexCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "openai-codex"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        // Prefer broker-proxied live discovery against the ChatGPT backend
        // models endpoint. Static seeds are offline / not-configured fallback
        // only — never the normal successful result when auth is available.
        Box::pin(async move {
            let path = codex_models_path(env!("CARGO_PKG_VERSION"));
            match broker_proxy_catalog_body("openai-codex", &path).await {
                Ok(body) => {
                    let models = parse_codex_catalog_models(&body)
                        .map_err(|e| format!("parse failed: {e}"))?;
                    capability_cache::replace_provider(self.provider_key(), &models);
                    Ok(models)
                }
                // Typed decision (fix1 I2a): only a no-live-session outcome
                // falls back to static seeds; operational failures surface.
                Err(CatalogBrokerError::NotAuthenticated(_)) => Ok(codex_static_catalog_models()),
                Err(CatalogBrokerError::Failed(err)) => Err(err),
            }
        })
    }
}

impl ModelCatalogProvider for XaiCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "xai-auth"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(xai_static_catalog_models()) })
    }
}

impl ModelCatalogProvider for GitHubCopilotCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "github-copilot"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        // Prefer broker-proxied live discovery (session token never enters this
        // module as a caller-supplied secret). Fall back to curated static seeds
        // exactly when the TYPED broker outcome says there is no live session
        // (fix1 I2a) — deterministic for every ambient credential state.
        Box::pin(async move {
            match broker_catalog_models_body("github-copilot").await {
                Ok(body) => {
                    parse_copilot_catalog_models(&body).map_err(|e| format!("parse failed: {e}"))
                }
                Err(CatalogBrokerError::NotAuthenticated(_)) => Ok(copilot_static_catalog_models()),
                Err(CatalogBrokerError::Failed(err)) => Err(err),
            }
        })
    }
}

impl ModelCatalogProvider for GoogleGeminiCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "google-gemini"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        // The Code Assist model-discovery surface is not documented as a
        // stable third-party API and there is no reviewed live-listing method
        // in the broker allowlist yet. Fail closed to the conservative static
        // catalog: text + tool-capable IDs whose provenance is the official
        // Gemini CLI reference source.
        Box::pin(async move { Ok(google_gemini_static_catalog_models()) })
    }
}

impl ModelCatalogProvider for GenericCatalogProvider {
    fn provider_key(&self) -> &'static str {
        "generic"
    }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move {
            Err("generic catalog fetch requires provider key; use fetch_generic_catalog_provider_models".to_string())
        })
    }
}

async fn fetch_generic_catalog_provider_models(
    provider_key: &str,
) -> Result<Vec<CatalogModel>, String> {
    let specs = super::registry::providers();
    let spec = specs
        .iter()
        .find(|s| s.key == provider_key)
        .ok_or_else(|| format!("unknown provider: {provider_key}"))?;
    if !crate::auth::broker::static_key_configured(provider_key) {
        return Err(format!("{} is not configured", spec.name));
    }
    let body = broker_catalog_models_body(provider_key).await?;
    parse_generic_catalog_models(&body, provider_key, spec.name)
        .map_err(|e| format!("parse failed: {e}"))
}

/// GET `/models` through the credential broker (the key never enters this
/// module). Returns the body on 2xx, a typed failure otherwise.
async fn broker_catalog_models_body(provider_key: &str) -> Result<String, CatalogBrokerError> {
    broker_proxy_catalog_body(provider_key, "/models").await
}

/// Typed broker catalog failure (fix1 I2a): the static-fallback decision is
/// made on TYPED broker variants, never by probing Display strings after
/// the fact — so the outcome is deterministic for every credential state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogBrokerError {
    /// No usable live session for the provider (not configured, unknown to
    /// the registries, registration required, or a stored-credential miss).
    /// Providers with curated seeds fall back to them.
    NotAuthenticated(String),
    /// A real operational failure (transport, denial, HTTP status,
    /// account-shape credential errors): fail closed.
    Failed(String),
}

impl CatalogBrokerError {
    fn message(&self) -> &str {
        match self {
            Self::NotAuthenticated(msg) | Self::Failed(msg) => msg,
        }
    }
}

impl From<CatalogBrokerError> for String {
    fn from(err: CatalogBrokerError) -> Self {
        err.message().to_string()
    }
}

/// Classify a typed [`BrokerError`] for catalog fetching. The one string
/// dependency left is the stable token-store load-miss prefix inside
/// `BrokerError::Credential` ("No credentials for …"), pinned by a unit
/// test — account-shape credential errors stay hard failures.
fn classify_catalog_broker_error(err: agent_core::auth::BrokerError) -> CatalogBrokerError {
    use agent_core::auth::BrokerError as B;
    let msg = format!("request failed: {err}");
    match &err {
        B::NotConfigured(_) | B::UnknownProvider(_) | B::RegistrationRequired { .. } => {
            CatalogBrokerError::NotAuthenticated(msg)
        }
        B::Credential(detail) if detail.starts_with("No credentials for ") => {
            CatalogBrokerError::NotAuthenticated(msg)
        }
        _ => CatalogBrokerError::Failed(msg),
    }
}

/// GET an allowlisted catalog path through the credential broker.
/// The credential never enters this module — only the provider key + path.
async fn broker_proxy_catalog_body(
    provider_key: &str,
    path: &str,
) -> Result<String, CatalogBrokerError> {
    let resp = crate::auth::broker::global_broker()
        .proxy(crate::auth::ProxyRequest {
            provider: provider_key.to_string(),
            method: crate::auth::ProxyMethod::Get,
            path: path.to_string(),
            body: None,
            stream: false,
            body_bytes: None,
        })
        .await
        .map_err(classify_catalog_broker_error)?;
    if !(200..300).contains(&resp.status) {
        return Err(CatalogBrokerError::Failed(format!(
            "model list failed: HTTP {}",
            resp.status
        )));
    }
    Ok(resp.body)
}

/// Fetch the OpenRouter live model list. Auth not required.
pub async fn fetch_openrouter_catalog_models(
    client: &reqwest::Client,
) -> Result<Vec<CatalogModel>, String> {
    let resp = catalog_get(client, "https://openrouter.ai/api/v1/models")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let body = read_catalog_response(resp).await?;
    parse_openrouter_catalog_models(&body).map_err(|e| format!("parse failed: {e}"))
}

/// Fetch catalog models for any registered provider.
/// OpenRouter uses its rich parser; all others use the generic parser.
/// Compatible shim: callers that previously used `registry::fetch_provider_models`
/// and then mapped to `ExpandedModelEntry` can switch to this.
pub async fn fetch_catalog_models(
    client: &reqwest::Client,
    provider_key: &str,
) -> Result<Vec<CatalogModel>, String> {
    let provider = catalog_provider_for(provider_key);
    if provider.provider_key() == "generic" {
        return fetch_generic_catalog_provider_models(provider_key).await;
    }
    provider.fetch(client).await
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Task 1: Normalized catalog contract ──────────────────────────────────

    #[test]
    fn catalog_model_rejects_empty_ids() {
        assert!(CatalogModel::new("openrouter", "OpenRouter", "").is_none());
        assert!(CatalogModel::new("openrouter", "OpenRouter", "   ").is_none());
    }

    #[test]
    fn static_seed_sets_fallback_source_and_runtime_id() {
        let m = from_static_seed("groq", "Groq", "llama-3.3-70b-versatile", "Llama 3.3 70B")
            .expect("valid seed");
        assert_eq!(m.runtime_id(), "groq/llama-3.3-70b-versatile");
        assert_eq!(m.display_label(), "Llama 3.3 70B");
        assert_eq!(m.source, CatalogSource::StaticFallback);
    }

    #[test]
    fn static_seed_empty_label_stores_none() {
        let m = from_static_seed("groq", "Groq", "model-x", "").expect("valid id");
        assert_eq!(m.label, None);
    }

    #[test]
    fn static_seed_whitespace_label_stores_none() {
        let m = from_static_seed("groq", "Groq", "model-x", "   ").expect("valid id");
        assert_eq!(m.label, None);
    }

    #[test]
    fn static_seeds_from_spec_converts_all_groq_models() {
        let spec = super::super::registry::providers()
            .iter()
            .find(|s| s.key == "groq")
            .expect("groq spec");
        let seeds = static_seeds_from_spec(spec);
        assert_eq!(seeds.len(), spec.models.len());
        assert!(seeds
            .iter()
            .all(|m| m.source == CatalogSource::StaticFallback));
        assert!(seeds.iter().all(|m| !m.id.is_empty()));
        assert!(seeds.iter().all(|m| m.runtime_id().starts_with("groq/")));
    }

    #[test]
    fn anthropic_runtime_id_is_provider_qualified() {
        let mut m = CatalogModel::new("anthropic", "Anthropic", "claude-opus-4-7").unwrap();
        m.provider_kind = CatalogProviderKind::Anthropic;
        assert_eq!(m.runtime_id(), "anthropic/claude-opus-4-7");
    }

    #[test]
    fn pricing_summary_has_internal_reasoning_cost_zero_is_false() {
        let p = PricingSummary {
            prompt: None,
            completion: None,
            internal_reasoning: Some("0".to_string()),
        };
        assert!(!p.has_internal_reasoning_cost());
    }

    #[test]
    fn pricing_summary_has_internal_reasoning_cost_nonzero_is_true() {
        let p = PricingSummary {
            prompt: None,
            completion: None,
            internal_reasoning: Some("0.0000035".to_string()),
        };
        assert!(p.has_internal_reasoning_cost());
    }

    #[tokio::test]
    async fn ui_catalog_fetch_includes_xai_static_models() {
        let models = fetch_catalog_models(&reqwest::Client::new(), "xai-auth")
            .await
            .expect("static xAI catalog");
        assert_eq!(models, xai_static_catalog_models());
        assert!(models
            .iter()
            .all(|model| model.runtime_id().starts_with("xai-auth/")));
    }

    #[tokio::test]
    // Broker-proxied discovery reads ambient credentials via base-dir
    // resolution; racing SYNAPS_BASE_DIR mutators made this flaky.
    #[serial_test::serial(synaps_base_dir)]
    async fn ui_catalog_fetch_github_copilot_returns_prefixed_chat_models() {
        // When the operator has a live session, broker-proxied discovery wins.
        // Otherwise the curated static fallback is returned. Either way runtime
        // ids are github-copilot/<wire-id> and include fixture-established IDs.
        let models = fetch_catalog_models(&reqwest::Client::new(), "github-copilot")
            .await
            .expect("GitHub Copilot catalog");
        assert!(!models.is_empty());
        assert!(models
            .iter()
            .all(|model| model.runtime_id().starts_with("github-copilot/")));
        let ids: std::collections::HashSet<_> = models.iter().map(|m| m.id.as_str()).collect();
        // High-value ids from the curated set must be present for this account
        // or via static fallback.
        for required in ["gpt-5.3-codex", "claude-sonnet-4.6", "gemini-3.5-flash"] {
            assert!(ids.contains(required), "missing {required}");
        }
        assert!(!ids.contains("text-embedding-3-small"));
    }

    /// fix1 I2a: with NO credentials anywhere (fresh empty base dir), the
    /// documented contract — "otherwise the curated static fallback is
    /// returned" — must hold DETERMINISTICALLY. The broker's typed
    /// credential-miss must map to the static seeds, never to an error.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn copilot_catalog_without_credentials_is_deterministic_static_fallback() {
        let _base = crate::test_env::BaseDirGuard::new();
        let models = fetch_catalog_models(&reqwest::Client::new(), "github-copilot")
            .await
            .expect("credential-less fetch must fall back to static seeds");
        assert_eq!(models, copilot_static_catalog_models());
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("github-copilot/")));
    }

    /// fix1 I2a: the fetch outcome must not depend on ambient
    /// SYNAPS_BASE_DIR churn. A current-thread churn task flips the base
    /// dir between two credential-less roots at every await point while
    /// fetches run — every fetch must return the same static catalog.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn copilot_catalog_is_deterministic_under_concurrent_base_dir_churn() {
        let base_a = tempfile::TempDir::new().unwrap();
        let base_b = tempfile::TempDir::new().unwrap();
        let old = std::env::var("SYNAPS_BASE_DIR").ok();
        std::env::set_var("SYNAPS_BASE_DIR", base_a.path());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let churn = {
                    let a = base_a.path().to_path_buf();
                    let b = base_b.path().to_path_buf();
                    // Same-thread churn task: env flips interleave at await
                    // points only (no cross-thread set_var races).
                    tokio::task::spawn_local(async move {
                        loop {
                            std::env::set_var("SYNAPS_BASE_DIR", &a);
                            tokio::task::yield_now().await;
                            std::env::set_var("SYNAPS_BASE_DIR", &b);
                            tokio::task::yield_now().await;
                        }
                    })
                };
                for _ in 0..20 {
                    let models = fetch_catalog_models(&reqwest::Client::new(), "github-copilot")
                        .await
                        .expect("fetch must be deterministic under base-dir churn");
                    assert_eq!(models, copilot_static_catalog_models());
                    tokio::task::yield_now().await;
                }
                churn.abort();
            })
            .await;

        match old {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
    }

    #[test]
    fn github_copilot_static_catalog_is_available_without_network() {
        let models = copilot_static_catalog_models();
        assert_eq!(models.len(), COPILOT_FALLBACK_MODELS.len());
        assert!(models
            .iter()
            .all(|m| m.source == CatalogSource::StaticFallback));
    }

    #[test]
    fn catalog_provider_trait_dispatch_selects_specialized_handlers() {
        assert_eq!(
            catalog_provider_for("openrouter").provider_key(),
            "openrouter"
        );
        assert_eq!(catalog_provider_for("groq").provider_key(), "groq");
        assert_eq!(catalog_provider_for("nvidia").provider_key(), "nvidia");
        assert_eq!(catalog_provider_for("claude").provider_key(), "claude");
        assert_eq!(
            catalog_provider_for("openai-codex").provider_key(),
            "openai-codex"
        );
        assert_eq!(catalog_provider_for("xai-auth").provider_key(), "xai-auth");
        assert_eq!(
            catalog_provider_for("github-copilot").provider_key(),
            "github-copilot"
        );
        assert_eq!(catalog_provider_for("cerebras").provider_key(), "generic");
    }

    #[test]
    fn catalog_request_timeout_is_bounded() {
        assert_eq!(CATALOG_REQUEST_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn anthropic_page_metadata_is_exposed_for_pagination() {
        let page = parse_anthropic_catalog_page(
            r#"{
            "data":[{"id":"claude-opus-4-7"}],
            "has_more": true,
            "last_id": "claude-opus-4-7"
        }"#,
        )
        .expect("parse page");
        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(page.models.len(), 1);
    }

    #[test]
    fn anthropic_pagination_url_adds_after_id_cursor() {
        assert_eq!(
            anthropic_models_url(None),
            "https://api.anthropic.com/v1/models?limit=100"
        );
        assert_eq!(
            anthropic_models_url(Some("claude-opus-4-7")),
            "https://api.anthropic.com/v1/models?limit=100&after_id=claude-opus-4-7"
        );
    }

    #[test]
    fn merge_catalog_pages_dedupes_by_id() {
        let first =
            parse_anthropic_catalog_models(r#"{"data":[{"id":"claude-opus-4-7"}]}"#).unwrap();
        let second = parse_anthropic_catalog_models(
            r#"{"data":[{"id":"claude-opus-4-7"},{"id":"claude-sonnet-4-6"}]}"#,
        )
        .unwrap();
        let merged = merge_catalog_pages(vec![first, second]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "claude-opus-4-7");
        assert_eq!(merged[1].id, "claude-sonnet-4-6");
    }

    // ── Task 2: OpenRouter rich catalog handler ───────────────────────────────

    mod openrouter {
        use super::super::*;

        const RICH_FIXTURE: &str = r#"{
          "data": [
            {
              "id": "qwen/qwen3-coder",
              "name": "Qwen: Qwen3 Coder",
              "context_length": 131072,
              "top_provider": { "max_completion_tokens": 32768 },
              "supported_parameters": ["temperature", "top_p", "max_tokens"],
              "pricing": { "prompt": "0.0000001", "completion": "0.0000004", "internal_reasoning": "0" },
              "architecture": { "input_modalities": ["text"] }
            },
            {
              "id": "anthropic/claude-opus-4-7",
              "name": "Anthropic: Claude Opus 4.7",
              "context_length": 200000,
              "supported_parameters": ["temperature", "verbosity", "max_tokens"],
              "pricing": { "prompt": "0.000015", "completion": "0.000075" }
            },
            {
              "id": "openai/o4-mini",
              "name": "OpenAI: o4-mini",
              "context_length": 128000,
              "supported_parameters": ["reasoning_effort", "max_tokens"],
              "pricing": { "prompt": "0.0000011", "completion": "0.0000044" }
            },
            {
              "id": "google/gemini-2.5-flash",
              "name": "Google: Gemini 2.5 Flash",
              "context_length": 1048576,
              "supported_parameters": ["reasoning", "include_reasoning", "max_tokens"],
              "pricing": { "prompt": "0.00000015", "completion": "0.0000035", "internal_reasoning": "0.0000035" },
              "architecture": { "input_modalities": ["text", "image", "audio", "video"] }
            },
            {
              "id": "",
              "name": "Empty — must be filtered"
            }
          ]
        }"#;

        #[test]
        fn parses_minimal_model() {
            let json = r#"{"data":[{"id":"test/model","name":"Test Model"}]}"#;
            let models = parse_openrouter_catalog_models(json).expect("parse ok");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "test/model");
            assert_eq!(models[0].label.as_deref(), Some("Test Model"));
            assert_eq!(models[0].runtime_id(), "openrouter/test/model");
            assert_eq!(models[0].provider_key, "openrouter");
            assert_eq!(models[0].source, CatalogSource::Live);
        }

        #[test]
        fn parses_context_length_and_max_output() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let qwen = models.iter().find(|m| m.id == "qwen/qwen3-coder").unwrap();
            assert_eq!(qwen.context_tokens, Some(131_072));
            assert_eq!(qwen.max_output_tokens, Some(32_768));
        }

        #[test]
        fn filters_empty_ids() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            assert!(!models.iter().any(|m| m.id.is_empty()));
            assert_eq!(models.len(), 4);
        }

        #[test]
        fn parses_pricing_fields() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let qwen = models.iter().find(|m| m.id == "qwen/qwen3-coder").unwrap();
            assert_eq!(qwen.pricing.prompt.as_deref(), Some("0.0000001"));
            assert_eq!(qwen.pricing.completion.as_deref(), Some("0.0000004"));
            assert!(!qwen.pricing.has_internal_reasoning_cost());
        }

        #[test]
        fn parses_internal_reasoning_cost_flag() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let gemini = models
                .iter()
                .find(|m| m.id == "google/gemini-2.5-flash")
                .unwrap();
            assert!(gemini.pricing.has_internal_reasoning_cost());
        }

        #[test]
        fn no_reasoning_params_maps_to_none() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let qwen = models.iter().find(|m| m.id == "qwen/qwen3-coder").unwrap();
            assert_eq!(qwen.reasoning, ReasoningSupport::None);
        }

        #[test]
        fn verbosity_param_maps_to_anthropic_adaptive() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let claude = models
                .iter()
                .find(|m| m.id == "anthropic/claude-opus-4-7")
                .unwrap();
            assert_eq!(
                claude.reasoning,
                ReasoningSupport::AnthropicAdaptive { adaptive: true }
            );
        }

        #[test]
        fn reasoning_effort_maps_to_openrouter_reasoning() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let o4 = models.iter().find(|m| m.id == "openai/o4-mini").unwrap();
            assert_eq!(
                o4.reasoning,
                ReasoningSupport::OpenRouter {
                    include_reasoning: false,
                    effort: true,
                    verbosity: false,
                    internal_reasoning_priced: false,
                }
            );
        }

        #[test]
        fn reasoning_include_reasoning_maps_correctly() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let gemini = models
                .iter()
                .find(|m| m.id == "google/gemini-2.5-flash")
                .unwrap();
            assert_eq!(
                gemini.reasoning,
                ReasoningSupport::OpenRouter {
                    include_reasoning: true,
                    effort: false,
                    verbosity: false,
                    internal_reasoning_priced: true,
                }
            );
        }

        #[test]
        fn parses_multimodal_input() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let gemini = models
                .iter()
                .find(|m| m.id == "google/gemini-2.5-flash")
                .unwrap();
            assert!(gemini.input_modalities.contains(&Modality::Text));
            assert!(gemini.input_modalities.contains(&Modality::Image));
            assert!(gemini.input_modalities.contains(&Modality::Audio));
            assert!(gemini.input_modalities.contains(&Modality::Video));
        }

        #[test]
        fn missing_modalities_defaults_to_text() {
            let json = r#"{"data":[{"id":"test/model"}]}"#;
            let models = parse_openrouter_catalog_models(json).expect("parse ok");
            assert_eq!(models[0].input_modalities, vec![Modality::Text]);
        }

        #[test]
        fn parses_openrouter_rich_metadata_and_reasoning_flags() {
            // Backward-compatible name used by existing test
            let json = r#"{
              "data": [{
                "id": "anthropic/claude-sonnet-4.6",
                "name": "Anthropic: Claude Sonnet 4.6",
                "context_length": 1000000,
                "architecture": { "input_modalities": ["text", "image"] },
                "pricing": { "prompt": "0.000003", "completion": "0.000015", "internal_reasoning": "0.000012" },
                "top_provider": { "max_completion_tokens": 128000 },
                "supported_parameters": ["reasoning", "include_reasoning", "verbosity", "tools"]
              }]
            }"#;
            let models = parse_openrouter_catalog_models(json).expect("parse ok");
            assert_eq!(models.len(), 1);
            let m = &models[0];
            assert_eq!(m.runtime_id(), "openrouter/anthropic/claude-sonnet-4.6");
            assert_eq!(m.context_tokens, Some(1_000_000));
            assert_eq!(m.max_output_tokens, Some(128_000));
            assert!(m.input_modalities.contains(&Modality::Image));
            // verbosity wins → AnthropicAdaptive
            assert_eq!(
                m.reasoning,
                ReasoningSupport::AnthropicAdaptive { adaptive: true }
            );
        }

        #[test]
        fn parses_openrouter_non_reasoning_model_as_none() {
            let json = r#"{
              "data": [{
                "id": "meta-llama/llama-3.3-70b-instruct",
                "name": "Meta: Llama 3.3 70B",
                "supported_parameters": ["temperature", "tools"]
              }]
            }"#;
            let models = parse_openrouter_catalog_models(json).expect("parse ok");
            assert_eq!(models[0].reasoning, ReasoningSupport::None);
        }

        #[test]
        fn invalid_json_returns_error() {
            assert!(parse_openrouter_catalog_models("{not json}").is_err());
        }

        #[test]
        fn missing_data_key_returns_error() {
            assert!(parse_openrouter_catalog_models(r#"{"models":[]}"#).is_err());
        }
    }

    // ── Task 3: Generic handler / compat with registry ────────────────────────

    // ── Task 5: Anthropic parser and Codex static catalog ───────────────────

    mod anthropic {
        use super::super::*;

        #[test]
        fn parser_reads_optional_capabilities_and_token_limits() {
            let json = r#"{
                "data": [{
                    "id": "claude-opus-4-7",
                    "display_name": "Claude Opus 4.7",
                    "max_input_tokens": 200000,
                    "max_tokens": 32000,
                    "capabilities": {
                        "thinking": { "supported": true },
                        "effort": { "supported": true }
                    }
                }],
                "has_more": false
            }"#;
            let models = parse_anthropic_catalog_models(json).expect("parse anthropic");
            assert_eq!(models.len(), 1);
            let model = &models[0];
            assert_eq!(model.runtime_id(), "anthropic/claude-opus-4-7");
            assert_eq!(model.label.as_deref(), Some("Claude Opus 4.7"));
            assert_eq!(model.context_tokens, Some(200_000));
            assert_eq!(model.max_output_tokens, Some(32_000));
            assert_eq!(
                model.reasoning,
                ReasoningSupport::AnthropicAdaptive { adaptive: true }
            );
        }

        #[test]
        fn parser_tolerates_missing_capabilities_as_unknown() {
            let json =
                r#"{"data":[{"id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku"}]}"#;
            let models = parse_anthropic_catalog_models(json).expect("parse anthropic");
            assert_eq!(models[0].reasoning, ReasoningSupport::Unknown);
        }

        #[test]
        fn parser_filters_empty_ids() {
            let json = r#"{"data":[{"id":""},{"id":"claude-sonnet-4-6"}]}"#;
            let models = parse_anthropic_catalog_models(json).expect("parse anthropic");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "claude-sonnet-4-6");
        }
    }

    mod codex {
        use super::super::*;

        #[test]
        fn static_catalog_uses_fallback_source_and_prefixed_runtime_ids() {
            let models = codex_static_catalog_models();
            assert!(models.iter().any(|m| m.id == "gpt-5.5"));
            assert!(models.iter().any(|m| m.id == "gpt-5.4"));
            assert!(models.iter().any(|m| m.id == "gpt-5.4-mini"));
            assert!(!models.iter().any(|m| m.id == "gpt-5.5-pro"));
            assert!(!models.iter().any(|m| m.id == "gpt-5.4-nano"));
            assert!(!models.iter().any(|m| m.id == "gpt-5.1-codex-mini"));
            assert!(models
                .iter()
                .all(|m| m.source == CatalogSource::StaticFallback));
            assert!(models
                .iter()
                .all(|m| m.runtime_id().starts_with("openai-codex/")));
        }

        #[test]
        fn live_parser_filters_non_list_visibility_from_fixture() {
            let fixture = include_str!("fixtures/openai_codex_models.json");
            let models = parse_codex_catalog_models(fixture).expect("parse");
            let ids: std::collections::HashSet<_> = models.iter().map(|m| m.id.as_str()).collect();
            assert!(ids.contains("gpt-5.6-sol"));
            assert!(ids.contains("gpt-5.3-codex-spark"));
            assert!(!ids.contains("codex-auto-review"));
            assert!(!ids.contains("codex-internal-eval"));
            assert!(models.iter().all(|m| m.source == CatalogSource::Live));
        }

        #[test]
        fn models_path_carries_package_client_version() {
            let path = codex_models_path(env!("CARGO_PKG_VERSION"));
            assert!(path.starts_with("/models?client_version="));
            assert!(path.contains(env!("CARGO_PKG_VERSION")));
        }

        #[test]
        fn missing_credential_fallback_classifies_real_broker_not_configured_text() {
            let not_configured = format!(
                "request failed: {}",
                crate::auth::BrokerError::NotConfigured("openai-codex".into())
            );
            assert!(
                is_missing_credential_catalog_error(&not_configured),
                "got: {not_configured}"
            );
            let unknown = format!(
                "request failed: {}",
                crate::auth::BrokerError::UnknownProvider("openai-codex".into())
            );
            assert!(
                is_missing_credential_catalog_error(&unknown),
                "got: {unknown}"
            );
        }

        #[test]
        fn missing_credential_fallback_classifies_exact_broker_credential_no_credentials_text() {
            // Production path: LocalBroker::access_token maps
            // ensure_fresh_provider_token's load miss through BrokerError::Credential.
            // Display form is exactly:
            //   credential error: No credentials for {provider} at {path}. Run `synaps login`.
            let raw = crate::auth::BrokerError::Credential(format!(
                "No credentials for openai-codex at {}. Run `synaps login`.",
                std::path::Path::new("/tmp/auth.json").display()
            ));
            let wrapped = format!("request failed: {raw}");
            assert!(
                is_missing_credential_catalog_error(&wrapped),
                "exact BrokerError::Credential missing-login text must fall back to static seeds; got: {wrapped}"
            );
            // Prefix-stable form (path/provider vary; Display prefix does not).
            assert!(is_missing_credential_catalog_error(
                "request failed: credential error: No credentials for openai-codex at /home/user/.synaps/auth.json. Run `synaps login`."
            ));
        }

        /// fix1 I2a: the TYPED classifier is the authoritative fallback
        /// decision. No-live-session broker variants map to
        /// NotAuthenticated; everything operational maps to Failed.
        #[test]
        fn typed_classifier_maps_auth_misses_to_fallback_and_operations_to_failed() {
            use crate::auth::BrokerError as B;
            let auth_misses = [
                B::NotConfigured("github-copilot".into()),
                B::UnknownProvider("github-copilot".into()),
                B::RegistrationRequired {
                    provider: "github-copilot".into(),
                    remediation: "run synaps login".into(),
                },
                // Pinned token-store load-miss prefix (the one string
                // dependency, asserted here so token.rs drift breaks THIS
                // test instead of production determinism).
                B::Credential(
                    "No credentials for github-copilot at /tmp/x/auth.json. Run `synaps login`."
                        .into(),
                ),
            ];
            for err in auth_misses {
                assert!(
                    matches!(
                        classify_catalog_broker_error(err.clone()),
                        CatalogBrokerError::NotAuthenticated(_)
                    ),
                    "must classify as no-live-session: {err}"
                );
            }

            let operational = [
                B::Transport("connection reset".into()),
                B::Denied("proxy path not allowlisted".into()),
                B::Unauthorized,
                B::Credential("github-copilot credential is missing chatgpt account id".into()),
                B::UnsupportedCapability {
                    provider: "github-copilot".into(),
                    capability: "tools".into(),
                },
            ];
            for err in operational {
                assert!(
                    matches!(
                        classify_catalog_broker_error(err.clone()),
                        CatalogBrokerError::Failed(_)
                    ),
                    "must classify as operational failure: {err}"
                );
            }
        }

        #[test]
        fn missing_credential_fallback_does_not_hide_transport_http_parse_or_account_errors() {
            for err in [
                "request failed: broker transport error: connection reset",
                "model list failed: HTTP 401",
                "model list failed: HTTP 500",
                "parse failed: missing field `models`",
                "request failed: broker denied request: proxy path '/models' is not in the provider's endpoint allowlist",
                "request failed: credential error: openai-codex credential is missing chatgpt account id",
                "request failed: broker transport error: provider request failed: 403 Forbidden",
            ] {
                assert!(
                    !is_missing_credential_catalog_error(err),
                    "must not fallback for: {err}"
                );
            }
        }
    }

    mod anthropic_paths {
        use super::super::*;

        #[test]
        fn proxy_path_round_trips_limit_and_after_id() {
            assert_eq!(anthropic_models_proxy_path(None), "/v1/models?limit=100");
            assert_eq!(
                anthropic_models_proxy_path(Some("claude-opus-4-7")),
                "/v1/models?limit=100&after_id=claude-opus-4-7"
            );
            assert_eq!(
                anthropic_models_proxy_path(Some("")),
                "/v1/models?limit=100"
            );
        }
    }

    // ── Task 4: Groq and NVIDIA enrichment ──────────────────────────────────

    mod groq {
        use super::super::*;

        #[test]
        fn parser_extracts_context_window_and_filters_inactive() {
            let json = r#"{"data":[
                {"id":"llama-3.3-70b-versatile","active":true,"context_window":131072,"owned_by":"Meta"},
                {"id":"old-model-v1","active":false,"context_window":8192,"owned_by":"Meta"}
            ]}"#;
            let models = parse_groq_catalog_models(json).expect("parse groq");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "llama-3.3-70b-versatile");
            assert_eq!(models[0].context_tokens, Some(131_072));
            assert_eq!(models[0].reasoning, ReasoningSupport::None);
        }

        #[test]
        fn inference_maps_reasoning_families() {
            assert_eq!(
                infer_groq_reasoning("openai/gpt-oss-120b"),
                ReasoningSupport::GroqReasoning
            );
            assert_eq!(
                infer_groq_reasoning("qwen/qwen3-32b"),
                ReasoningSupport::GroqReasoning
            );
            assert_eq!(
                infer_groq_reasoning("groq/compound-mini"),
                ReasoningSupport::GroqReasoning
            );
            assert_eq!(
                infer_groq_reasoning("llama-3.3-70b-versatile"),
                ReasoningSupport::None
            );
        }

        #[test]
        fn parser_filters_empty_ids() {
            let json = r#"{"data":[{"id":"","active":true},{"id":"openai/gpt-oss-20b","active":true,"context_window":131072}]}"#;
            let models = parse_groq_catalog_models(json).expect("parse groq");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "openai/gpt-oss-20b");
        }
    }

    mod nvidia {
        use super::super::*;

        #[test]
        fn parser_dedupes_and_enriches_known_context() {
            let json = r#"{"data":[
                {"id":"nvidia/llama-3.1-nemotron-ultra-253b-v1","owned_by":"nvidia"},
                {"id":"nvidia/llama-3.1-nemotron-ultra-253b-v1","owned_by":"nvidia"},
                {"id":"moonshotai/kimi-k2-thinking","owned_by":"moonshotai"}
            ]}"#;
            let models = parse_nvidia_catalog_models(json).expect("parse nvidia");
            assert_eq!(models.len(), 2);
            let ultra = models.iter().find(|m| m.id.contains("ultra")).unwrap();
            assert_eq!(ultra.context_tokens, Some(128_000));
            assert_eq!(ultra.reasoning, ReasoningSupport::NvidiaInlineThinking);
            let kimi = models.iter().find(|m| m.id.contains("kimi")).unwrap();
            assert_eq!(kimi.context_tokens, Some(256_000));
        }

        #[test]
        fn inference_detects_thinking_and_standard_models() {
            assert_eq!(
                infer_nvidia_reasoning("qwen/qwen3-next-80b-a3b-thinking"),
                ReasoningSupport::NvidiaInlineThinking
            );
            assert_eq!(
                infer_nvidia_reasoning("nvidia/cosmos-reason2-8b"),
                ReasoningSupport::NvidiaInlineThinking
            );
            assert_eq!(
                infer_nvidia_reasoning("meta/llama-3.3-70b-instruct"),
                ReasoningSupport::None
            );
        }

        #[test]
        fn parser_filters_empty_ids() {
            let json = r#"{"data":[{"id":""},{"id":"meta/llama-3.3-70b-instruct"}]}"#;
            let models = parse_nvidia_catalog_models(json).expect("parse nvidia");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "meta/llama-3.3-70b-instruct");
        }
    }

    mod generic_compat {
        use super::super::*;

        #[test]
        fn parses_generic_catalog_models_and_filters_empty_ids() {
            let json = r#"{
                "data": [
                    { "id": "qwen/qwen3-coder", "name": "Qwen: Qwen3 Coder" },
                    { "id": "" },
                    { "id": "openai/gpt-oss-120b" }
                ]
            }"#;
            let models =
                parse_generic_catalog_models(json, "openrouter", "OpenRouter").expect("parse ok");
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].runtime_id(), "openrouter/qwen/qwen3-coder");
            assert_eq!(models[0].display_label(), "Qwen: Qwen3 Coder");
            assert_eq!(models[1].display_label(), "openai/gpt-oss-120b");
        }

        #[test]
        fn whitespace_only_id_is_filtered() {
            let json = r#"{"data":[{"id":"   "},{"id":"valid"}]}"#;
            let models = parse_generic_catalog_models(json, "p", "P").expect("parse ok");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "valid");
        }

        #[test]
        fn whitespace_label_stored_as_none() {
            let json = r#"{"data":[{"id":"m1","name":"   "}]}"#;
            let models = parse_generic_catalog_models(json, "p", "P").expect("parse ok");
            assert_eq!(models[0].label, None);
        }

        #[test]
        fn generic_catalog_source_is_live() {
            let json = r#"{"data":[{"id":"m1","name":"Model One"}]}"#;
            let models =
                parse_generic_catalog_models(json, "testprovider", "Test").expect("parse ok");
            assert_eq!(models[0].source, CatalogSource::Live);
        }

        #[test]
        fn generic_reasoning_is_generic_open_ai() {
            let json = r#"{"data":[{"id":"m1"}]}"#;
            let models = parse_generic_catalog_models(json, "p", "P").expect("parse ok");
            assert_eq!(models[0].reasoning, ReasoningSupport::GenericOpenAi);
        }

        #[test]
        fn generic_parse_matches_legacy_parse_behavior() {
            // parse_generic_catalog_models should produce same ids/labels
            // as the legacy registry::parse_provider_models_response
            let json = r#"{
                "data": [
                    { "id": "qwen/qwen3-coder", "name": "Qwen: Qwen3 Coder" },
                    { "id": "openai/gpt-oss-120b" }
                ]
            }"#;
            let legacy = super::super::super::registry::parse_provider_models_response(json)
                .expect("legacy parse ok");
            let catalog = parse_generic_catalog_models(json, "openrouter", "OpenRouter")
                .expect("catalog parse ok");
            assert_eq!(legacy.len(), catalog.len());
            for (l, c) in legacy.iter().zip(catalog.iter()) {
                assert_eq!(l.id, c.id, "ids must match");
                assert_eq!(l.name, c.label, "labels must match");
            }
        }

        #[test]
        fn static_seeds_from_spec_all_providers() {
            // Every registered provider should produce at least one seed
            for spec in super::super::super::registry::providers() {
                if spec.models.is_empty() {
                    continue;
                }
                let seeds = super::super::static_seeds_from_spec(spec);
                assert!(!seeds.is_empty(), "no seeds for {}", spec.key);
                assert!(
                    seeds
                        .iter()
                        .all(|m| m.runtime_id().starts_with(&format!("{}/", spec.key))),
                    "runtime_id prefix wrong for {}",
                    spec.key
                );
            }
        }
    }
}
