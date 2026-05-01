//! OpenRouter `/models` parser and live fetch.

use serde::Deserialize;

use super::{CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary, ReasoningSupport};

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
