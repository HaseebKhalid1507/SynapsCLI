use super::*;
use serde::Deserialize;

pub(super) const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
pub(super) const ANTHROPIC_MODELS_PAGE_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicCatalogPage {
    pub models: Vec<CatalogModel>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

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
