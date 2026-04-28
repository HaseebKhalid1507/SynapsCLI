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

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use serde::Deserialize;

pub const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_MODELS_PAGE_LIMIT: usize = 100;
const ANTHROPIC_MODELS_MAX_PAGES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicCatalogPage {
    pub models: Vec<CatalogModel>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

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
    pub fn from_str(s: &str) -> Self {
        match s {
            "text"  => Modality::Text,
            "image" => Modality::Image,
            "audio" => Modality::Audio,
            "video" => Modality::Video,
            "file"  => Modality::File,
            other   => Modality::Other(other.to_string()),
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

    /// Synaps runtime id: bare for Anthropic/Claude, "provider/id" otherwise.
    pub fn runtime_id(&self) -> String {
        match &self.provider_kind {
            CatalogProviderKind::Anthropic => self.id.clone(),
            _ => format!("{}/{}", self.provider_key, self.id),
        }
    }

    /// Label if present, id otherwise.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
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
    m.label = if label.trim().is_empty() { None } else { Some(label.to_string()) };
    m.source = CatalogSource::StaticFallback;
    m.reasoning = ReasoningSupport::Unknown;
    Some(m)
}

/// Convert all static seeds in a ProviderSpec to CatalogModel entries.
pub fn static_seeds_from_spec(
    spec: &super::registry::ProviderSpec,
) -> Vec<CatalogModel> {
    spec.models
        .iter()
        .filter_map(|(id, label, _tier)| {
            from_static_seed(spec.key, spec.name, id, label)
        })
        .collect()
}

// ─── Generic OpenAI-compatible parser ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GenericModelsResponse {
    data: Vec<GenericModelItem>,
}

#[derive(Debug, Deserialize)]
struct GenericModelItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

/// Parse a `{data:[{id,name?}]}` response.
/// Identical filtering behaviour to `registry::parse_provider_models_response`.
pub fn parse_generic_catalog_models(
    body: &str,
    provider_key: &str,
    provider_name: &str,
) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: GenericModelsResponse = serde_json::from_str(body)?;
    Ok(resp
        .data
        .into_iter()
        .filter_map(|item| {
            let mut m = CatalogModel::new(provider_key, provider_name, item.id)?;
            m.label = item.name.filter(|n| !n.trim().is_empty());
            m.reasoning = ReasoningSupport::GenericOpenAi;
            Some(m)
        })
        .collect())
}

// ─── OpenRouter parser ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterModelsResponse {
    data: Vec<OpenRouterModelItem>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    internal_reasoning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

/// Parse an OpenRouter `/models` JSON body into CatalogModel entries.
/// Pure function — no network I/O — safe to unit-test with fixtures.
pub fn parse_openrouter_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: OpenRouterModelsResponse = serde_json::from_str(body)?;
    Ok(resp
        .data
        .into_iter()
        .filter_map(|item| {
            let mut m = CatalogModel::new("openrouter", "OpenRouter", item.id)?;
            m.provider_kind = CatalogProviderKind::OpenRouter;
            m.label = item.name.filter(|n| !n.trim().is_empty());
            m.context_tokens = item.context_length;
            m.max_output_tokens = item.top_provider.as_ref().and_then(|p| p.max_completion_tokens);

            let mods: Vec<Modality> = item
                .architecture
                .map(|a| a.input_modalities.iter().map(|s| Modality::from_str(s)).collect())
                .unwrap_or_else(|| vec![Modality::Text]);
            m.input_modalities = mods;

            let pricing = item.pricing.map(|p| PricingSummary {
                prompt:             p.prompt.filter(|v| !v.trim().is_empty()),
                completion:         p.completion.filter(|v| !v.trim().is_empty()),
                internal_reasoning: p.internal_reasoning.filter(|v| !v.trim().is_empty()),
            }).unwrap_or_default();

            let has = |param: &str| item.supported_parameters.iter().any(|p| p == param);
            let internal_priced = pricing.has_internal_reasoning_cost();

            m.reasoning = if has("verbosity") {
                // Anthropic models routed through OpenRouter
                ReasoningSupport::AnthropicAdaptive { adaptive: true }
            } else if has("reasoning") || has("include_reasoning") || has("reasoning_effort") || internal_priced {
                ReasoningSupport::OpenRouter {
                    include_reasoning: has("include_reasoning"),
                    effort:            has("reasoning_effort"),
                    verbosity:         false,
                    internal_reasoning_priced: internal_priced,
                }
            } else {
                ReasoningSupport::None
            };

            m.pricing = pricing;
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect())
}


// ─── Groq parser / inference ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GroqModelsResponse {
    data: Vec<GroqModelItem>,
}

#[derive(Debug, Deserialize)]
struct GroqModelItem {
    id: String,
    #[serde(default = "default_true")]
    active: bool,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    owned_by: Option<String>,
}

fn default_true() -> bool { true }

pub fn infer_groq_reasoning(model_id: &str) -> ReasoningSupport {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("openai/gpt-oss-") || id.starts_with("qwen/qwen3-") || id.starts_with("groq/compound") {
        ReasoningSupport::GroqReasoning
    } else {
        ReasoningSupport::None
    }
}

pub fn parse_groq_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: GroqModelsResponse = serde_json::from_str(body)?;
    Ok(resp
        .data
        .into_iter()
        .filter(|item| item.active)
        .filter_map(|item| {
            let mut m = CatalogModel::new("groq", "Groq", item.id)?;
            m.provider_kind = CatalogProviderKind::Groq;
            m.label = item.owned_by.as_ref().map(|owner| format!("{} — {}", m.id, owner));
            m.context_tokens = item.context_window;
            m.reasoning = infer_groq_reasoning(&m.id);
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect())
}

// ─── NVIDIA NIM parser / inference ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct NvidiaModelsResponse {
    data: Vec<NvidiaModelItem>,
}

#[derive(Debug, Deserialize)]
struct NvidiaModelItem {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
}

fn infer_nvidia_context_tokens(model_id: &str) -> Option<u64> {
    let id = model_id.to_ascii_lowercase();
    if id.contains("kimi-k2-thinking") { Some(256_000) }
    else if id.contains("nemotron-ultra")
        || id.contains("nemotron-super")
        || id.contains("qwen3-next")
        || id.contains("deepseek-v3.1")
        || id.contains("llama-3.1-405b")
        || id.contains("llama-3.2-90b-vision")
        || id.contains("mistral-large-2")
    { Some(128_000) }
    else { None }
}

pub fn infer_nvidia_reasoning(model_id: &str) -> ReasoningSupport {
    let id = model_id.to_ascii_lowercase();
    if id.contains("thinking")
        || id.contains("cosmos-reason")
        || id.contains("nemotron-ultra")
        || id.contains("nemotron-super")
        || id.contains("mistral-nemotron")
        || id.contains("magistral")
        || id.contains("deepseek-v4")
        || id.contains("kimi")
        || id.contains("reasoning")
    {
        ReasoningSupport::NvidiaInlineThinking
    } else {
        ReasoningSupport::None
    }
}

pub fn parse_nvidia_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: NvidiaModelsResponse = serde_json::from_str(body)?;
    let mut seen = std::collections::BTreeSet::new();
    Ok(resp
        .data
        .into_iter()
        .filter_map(|item| {
            let id = item.id;
            if id.trim().is_empty() || !seen.insert(id.clone()) {
                return None;
            }
            let mut m = CatalogModel::new("nvidia", "NVIDIA NIM", id)?;
            m.provider_kind = CatalogProviderKind::NvidiaNim;
            m.label = item.owned_by.as_ref().map(|owner| format!("{} — {}", m.id, owner));
            m.context_tokens = infer_nvidia_context_tokens(&m.id);
            m.reasoning = infer_nvidia_reasoning(&m.id);
            m.source = if m.context_tokens.is_some() || m.reasoning != ReasoningSupport::None {
                CatalogSource::Inferred
            } else {
                CatalogSource::Live
            };
            Some(m)
        })
        .collect())
}


// ─── Anthropic parser and Codex static handler ──────────────────────────────

#[derive(Debug, Deserialize)]
struct AnthropicModelsPage {
    data: Vec<AnthropicModelItem>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelItem {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<AnthropicCapabilities>,
}

#[derive(Debug, Deserialize)]
struct AnthropicCapabilities {
    #[serde(default)]
    thinking: Option<CapabilitySupported>,
    #[serde(default)]
    effort: Option<AnthropicEffortCapability>,
}

#[derive(Debug, Deserialize)]
struct CapabilitySupported {
    #[serde(default)]
    supported: bool,
}

#[derive(Debug, Deserialize)]
struct AnthropicEffortCapability {
    #[serde(default)]
    supported: bool,
}

pub fn parse_anthropic_catalog_page(body: &str) -> Result<AnthropicCatalogPage, serde_json::Error> {
    let page: AnthropicModelsPage = serde_json::from_str(body)?;
    let models = page
        .data
        .into_iter()
        .filter_map(|item| {
            let mut m = CatalogModel::new("anthropic", "Anthropic", item.id)?;
            m.provider_kind = CatalogProviderKind::Anthropic;
            m.label = item.display_name.filter(|name| !name.trim().is_empty());
            m.context_tokens = item.max_input_tokens;
            m.max_output_tokens = item.max_tokens;
            m.reasoning = match item.capabilities {
                Some(caps) if caps.thinking.as_ref().is_some_and(|c| c.supported) => {
                    ReasoningSupport::AnthropicAdaptive {
                        adaptive: caps.effort.as_ref().is_some_and(|c| c.supported),
                    }
                }
                _ => ReasoningSupport::Unknown,
            };
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect();

    Ok(AnthropicCatalogPage {
        models,
        has_more: page.has_more,
        last_id: page.last_id.filter(|id| !id.trim().is_empty()),
    })
}

pub fn parse_anthropic_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    parse_anthropic_catalog_page(body).map(|page| page.models)
}

pub fn anthropic_models_url(after_id: Option<&str>) -> String {
    let mut url = format!("{ANTHROPIC_MODELS_URL}?limit={ANTHROPIC_MODELS_PAGE_LIMIT}");
    if let Some(after_id) = after_id.filter(|id| !id.trim().is_empty()) {
        url.push_str("&after_id=");
        url.push_str(after_id);
    }
    url
}

pub fn merge_catalog_pages(pages: Vec<Vec<CatalogModel>>) -> Vec<CatalogModel> {
    let mut seen = std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for page in pages {
        for model in page {
            if seen.insert(model.id.clone()) {
                merged.push(model);
            }
        }
    }
    merged
}

pub fn codex_static_catalog_models() -> Vec<CatalogModel> {
    [
        ("gpt-5.5", "GPT-5.5"),
        ("gpt-5.1-codex-mini", "GPT-5.1 Codex Mini"),
    ]
    .into_iter()
    .filter_map(|(id, label)| {
        let mut m = CatalogModel::new("openai-codex", "OpenAI Codex", id)?;
        m.provider_kind = CatalogProviderKind::OpenAiCodex;
        m.label = Some(label.to_string());
        m.reasoning = ReasoningSupport::Unknown;
        m.source = CatalogSource::StaticFallback;
        Some(m)
    })
    .collect()
}

// ─── Live fetch helpers ───────────────────────────────────────────────────────

pub trait ModelCatalogProvider: Sync {
    fn provider_key(&self) -> &'static str;

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>>;
}

pub struct OpenRouterCatalogProvider;
pub struct GroqCatalogProvider;
pub struct NvidiaCatalogProvider;
pub struct AnthropicCatalogProvider;
pub struct CodexCatalogProvider;
pub struct GenericCatalogProvider;

pub fn catalog_provider_for(provider_key: &str) -> &'static dyn ModelCatalogProvider {
    match provider_key {
        "openrouter" => &OpenRouterCatalogProvider,
        "groq" => &GroqCatalogProvider,
        "nvidia" => &NvidiaCatalogProvider,
        "claude" | "anthropic" => &AnthropicCatalogProvider,
        "openai-codex" => &CodexCatalogProvider,
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
    client: &reqwest::Client,
) -> Result<Vec<CatalogModel>, String> {
    let creds = crate::auth::ensure_fresh_token(client)
        .await
        .map_err(|e| format!("Anthropic is not configured: {e}"))?;
    let mut pages = Vec::new();
    let mut after_id: Option<String> = None;

    for _ in 0..ANTHROPIC_MODELS_MAX_PAGES {
        let url = anthropic_models_url(after_id.as_deref());
        let resp = catalog_get(client, &url)
            .bearer_auth(&creds.access)
            .header("x-api-key", &creds.access)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let body = read_catalog_response(resp).await?;
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

impl ModelCatalogProvider for OpenRouterCatalogProvider {
    fn provider_key(&self) -> &'static str { "openrouter" }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { fetch_openrouter_catalog_models(client).await })
    }
}

impl ModelCatalogProvider for GroqCatalogProvider {
    fn provider_key(&self) -> &'static str { "groq" }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move {
            let spec = super::registry::providers()
                .iter()
                .find(|s| s.key == "groq")
                .ok_or_else(|| "unknown provider: groq".to_string())?;
            let api_key = super::registry::resolve_provider("groq", overrides)
                .map(|(cfg, _)| cfg.api_key)
                .ok_or_else(|| format!("{} is not configured", spec.name))?;
            let url = format!("{}/models", spec.base_url.trim_end_matches('/'));
            let resp = catalog_get(client, &url)
                .bearer_auth(api_key)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let body = read_catalog_response(resp).await?;
            parse_groq_catalog_models(&body).map_err(|e| format!("parse failed: {e}"))
        })
    }
}

impl ModelCatalogProvider for NvidiaCatalogProvider {
    fn provider_key(&self) -> &'static str { "nvidia" }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _overrides: &'a BTreeMap<String, String>,
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
    fn provider_key(&self) -> &'static str { "claude" }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        _overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { fetch_anthropic_catalog_models(client).await })
    }
}

impl ModelCatalogProvider for CodexCatalogProvider {
    fn provider_key(&self) -> &'static str { "openai-codex" }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(codex_static_catalog_models()) })
    }
}

impl ModelCatalogProvider for GenericCatalogProvider {
    fn provider_key(&self) -> &'static str { "generic" }

    fn fetch<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _overrides: &'a BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CatalogModel>, String>> + Send + 'a>> {
        Box::pin(async move {
            Err("generic catalog fetch requires provider key; use fetch_generic_catalog_provider_models".to_string())
        })
    }
}

async fn fetch_generic_catalog_provider_models(
    client: &reqwest::Client,
    provider_key: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<CatalogModel>, String> {
    let specs = super::registry::providers();
    let spec = specs
        .iter()
        .find(|s| s.key == provider_key)
        .ok_or_else(|| format!("unknown provider: {provider_key}"))?;

    let api_key = super::registry::resolve_provider(provider_key, overrides)
        .map(|(cfg, _)| cfg.api_key)
        .ok_or_else(|| format!("{} is not configured", spec.name))?;

    fetch_generic_catalog_models(
        client,
        provider_key,
        spec.name,
        spec.base_url,
        &api_key,
    ).await
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

/// Fetch a generic provider's `/models` endpoint.
pub async fn fetch_generic_catalog_models(
    client: &reqwest::Client,
    provider_key: &str,
    provider_name: &str,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<CatalogModel>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = catalog_get(client, &url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let body = read_catalog_response(resp).await?;
    parse_generic_catalog_models(&body, provider_key, provider_name)
        .map_err(|e| format!("parse failed: {e}"))
}

/// Fetch catalog models for any registered provider.
/// OpenRouter uses its rich parser; all others use the generic parser.
/// Compatible shim: callers that previously used `registry::fetch_provider_models`
/// and then mapped to `ExpandedModelEntry` can switch to this.
pub async fn fetch_catalog_models(
    client: &reqwest::Client,
    provider_key: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<CatalogModel>, String> {
    let provider = catalog_provider_for(provider_key);
    if provider.provider_key() == "generic" {
        return fetch_generic_catalog_provider_models(client, provider_key, overrides).await;
    }
    provider.fetch(client, overrides).await
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
        assert!(seeds.iter().all(|m| m.source == CatalogSource::StaticFallback));
        assert!(seeds.iter().all(|m| !m.id.is_empty()));
        assert!(seeds.iter().all(|m| m.runtime_id().starts_with("groq/")));
    }

    #[test]
    fn anthropic_runtime_id_is_bare() {
        let mut m = CatalogModel::new("anthropic", "Anthropic", "claude-opus-4-7").unwrap();
        m.provider_kind = CatalogProviderKind::Anthropic;
        assert_eq!(m.runtime_id(), "claude-opus-4-7");
    }

    #[test]
    fn pricing_summary_has_internal_reasoning_cost_zero_is_false() {
        let p = PricingSummary {
            prompt: None, completion: None,
            internal_reasoning: Some("0".to_string()),
        };
        assert!(!p.has_internal_reasoning_cost());
    }

    #[test]
    fn pricing_summary_has_internal_reasoning_cost_nonzero_is_true() {
        let p = PricingSummary {
            prompt: None, completion: None,
            internal_reasoning: Some("0.0000035".to_string()),
        };
        assert!(p.has_internal_reasoning_cost());
    }

    #[test]
    fn catalog_provider_trait_dispatch_selects_specialized_handlers() {
        assert_eq!(catalog_provider_for("openrouter").provider_key(), "openrouter");
        assert_eq!(catalog_provider_for("groq").provider_key(), "groq");
        assert_eq!(catalog_provider_for("nvidia").provider_key(), "nvidia");
        assert_eq!(catalog_provider_for("claude").provider_key(), "claude");
        assert_eq!(catalog_provider_for("openai-codex").provider_key(), "openai-codex");
        assert_eq!(catalog_provider_for("cerebras").provider_key(), "generic");
    }

    #[test]
    fn catalog_request_timeout_is_bounded() {
        assert_eq!(CATALOG_REQUEST_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn anthropic_page_metadata_is_exposed_for_pagination() {
        let page = parse_anthropic_catalog_page(r#"{
            "data":[{"id":"claude-opus-4-7"}],
            "has_more": true,
            "last_id": "claude-opus-4-7"
        }"#).expect("parse page");
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
        let first = parse_anthropic_catalog_models(r#"{"data":[{"id":"claude-opus-4-7"}]}"#).unwrap();
        let second = parse_anthropic_catalog_models(r#"{"data":[{"id":"claude-opus-4-7"},{"id":"claude-sonnet-4-6"}]}"#).unwrap();
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
            let gemini = models.iter().find(|m| m.id == "google/gemini-2.5-flash").unwrap();
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
            let claude = models.iter().find(|m| m.id == "anthropic/claude-opus-4-7").unwrap();
            assert_eq!(claude.reasoning, ReasoningSupport::AnthropicAdaptive { adaptive: true });
        }

        #[test]
        fn reasoning_effort_maps_to_openrouter_reasoning() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let o4 = models.iter().find(|m| m.id == "openai/o4-mini").unwrap();
            assert_eq!(o4.reasoning, ReasoningSupport::OpenRouter {
                include_reasoning: false,
                effort: true,
                verbosity: false,
                internal_reasoning_priced: false,
            });
        }

        #[test]
        fn reasoning_include_reasoning_maps_correctly() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let gemini = models.iter().find(|m| m.id == "google/gemini-2.5-flash").unwrap();
            assert_eq!(gemini.reasoning, ReasoningSupport::OpenRouter {
                include_reasoning: true,
                effort: false,
                verbosity: false,
                internal_reasoning_priced: true,
            });
        }

        #[test]
        fn parses_multimodal_input() {
            let models = parse_openrouter_catalog_models(RICH_FIXTURE).expect("parse ok");
            let gemini = models.iter().find(|m| m.id == "google/gemini-2.5-flash").unwrap();
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
            assert_eq!(m.reasoning, ReasoningSupport::AnthropicAdaptive { adaptive: true });
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
            assert_eq!(model.runtime_id(), "claude-opus-4-7");
            assert_eq!(model.label.as_deref(), Some("Claude Opus 4.7"));
            assert_eq!(model.context_tokens, Some(200_000));
            assert_eq!(model.max_output_tokens, Some(32_000));
            assert_eq!(model.reasoning, ReasoningSupport::AnthropicAdaptive { adaptive: true });
        }

        #[test]
        fn parser_tolerates_missing_capabilities_as_unknown() {
            let json = r#"{"data":[{"id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku"}]}"#;
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
            assert!(models.iter().any(|m| m.id == "gpt-5.1-codex-mini"));
            assert!(models.iter().all(|m| m.source == CatalogSource::StaticFallback));
            assert!(models.iter().all(|m| m.runtime_id().starts_with("openai-codex/")));
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
            assert_eq!(infer_groq_reasoning("openai/gpt-oss-120b"), ReasoningSupport::GroqReasoning);
            assert_eq!(infer_groq_reasoning("qwen/qwen3-32b"), ReasoningSupport::GroqReasoning);
            assert_eq!(infer_groq_reasoning("groq/compound-mini"), ReasoningSupport::GroqReasoning);
            assert_eq!(infer_groq_reasoning("llama-3.3-70b-versatile"), ReasoningSupport::None);
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
            assert_eq!(infer_nvidia_reasoning("qwen/qwen3-next-80b-a3b-thinking"), ReasoningSupport::NvidiaInlineThinking);
            assert_eq!(infer_nvidia_reasoning("nvidia/cosmos-reason2-8b"), ReasoningSupport::NvidiaInlineThinking);
            assert_eq!(infer_nvidia_reasoning("meta/llama-3.3-70b-instruct"), ReasoningSupport::None);
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
            let models = parse_generic_catalog_models(json, "openrouter", "OpenRouter")
                .expect("parse ok");
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
            let models = parse_generic_catalog_models(json, "testprovider", "Test")
                .expect("parse ok");
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
                if spec.models.is_empty() { continue; }
                let seeds = super::super::static_seeds_from_spec(spec);
                assert!(!seeds.is_empty(), "no seeds for {}", spec.key);
                assert!(
                    seeds.iter().all(|m| m.runtime_id().starts_with(&format!("{}/", spec.key))),
                    "runtime_id prefix wrong for {}",
                    spec.key
                );
            }
        }
    }
}
