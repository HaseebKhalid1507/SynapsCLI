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
    let models: Vec<CatalogModel> = page
        .data
        .into_iter()
        .filter_map(|item| {
            let mut m = CatalogModel::new("anthropic", "Anthropic", item.id)?;
            m.provider_kind = CatalogProviderKind::Anthropic;
            m.label = item.display_name.filter(|name| !name.trim().is_empty());
            m.context_tokens = item.max_input_tokens;
            m.max_output_tokens = item.max_tokens;
            m.reasoning = match item.capabilities {
                Some(caps) => match caps.thinking.as_ref() {
                    Some(thinking) if thinking.supported => ReasoningSupport::AnthropicAdaptive {
                        adaptive: caps.effort.as_ref().is_some_and(|c| c.supported),
                    },
                    // Explicit evidence that thinking is unsupported → None
                    // (named reasoning fails closed for this model).
                    Some(_) => ReasoningSupport::None,
                    // Capabilities present but thinking omitted → Unknown.
                    None => ReasoningSupport::Unknown,
                },
                _ => ReasoningSupport::Unknown,
            };
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect();

    // Populate the process-local capability cache (keyed by "anthropic/<id>")
    // so validation and dynamic option derivation see live data.
    super::capability_cache::populate(&models);

    Ok(AnthropicCatalogPage {
        models,
        has_more: page.has_more,
        last_id: page.last_id.filter(|id| !id.trim().is_empty()),
    })
}

pub fn parse_anthropic_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    parse_anthropic_catalog_page(body).map(|page| page.models)
}

/// Conservative exact static fallback descriptors for the source-controlled
/// known-model list (`agent_core::models::KNOWN_MODELS`). Exact ids only —
/// never substring-based. Evidence: adaptive-thinking notes in
/// `crates/agent-core/src/core/models.rs` (Opus 4.7+ / Fable 5 adaptive+effort;
/// Sonnet 4.6, Opus 4.6, Haiku 4.5 fixed-budget thinking).
pub fn anthropic_static_capability(model_id: &str) -> Option<ReasoningSupport> {
    match model_id {
        "claude-opus-4-7" | "claude-fable-5" => {
            Some(ReasoningSupport::AnthropicAdaptive { adaptive: true })
        }
        "claude-sonnet-4-6" | "claude-opus-4-6" | "claude-haiku-4-5-20251001" => {
            Some(ReasoningSupport::AnthropicAdaptive { adaptive: false })
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Exact static fallback descriptors (spec: anthropic-xai-reasoning-modes) ──

    #[test]
    fn static_capability_covers_exact_known_models_only() {
        for id in ["claude-opus-4-7", "claude-fable-5"] {
            assert_eq!(
                anthropic_static_capability(id),
                Some(ReasoningSupport::AnthropicAdaptive { adaptive: true }),
                "{id}"
            );
        }
        for id in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(
                anthropic_static_capability(id),
                Some(ReasoningSupport::AnthropicAdaptive { adaptive: false }),
                "{id}"
            );
        }
        // No substring inference: near-miss ids fail closed.
        for id in [
            "claude-opus-4-7-preview",
            "claude-haiku-4-5",
            "opus-4-7",
            "",
        ] {
            assert_eq!(anthropic_static_capability(id), None, "{id}");
        }
    }

    #[test]
    fn live_parse_maps_explicit_unsupported_thinking_to_none() {
        let body = r#"{
            "data": [
                {"id": "claude-test-thinker",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": true}}},
                {"id": "claude-test-fixed",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": false}}},
                {"id": "claude-test-nothink",
                 "capabilities": {"thinking": {"supported": false}}},
                {"id": "claude-test-nocaps"}
            ],
            "has_more": false
        }"#;
        let models = parse_anthropic_catalog_models(body).expect("parse");
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(
            by_id("claude-test-thinker").reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: true }
        );
        assert_eq!(
            by_id("claude-test-fixed").reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: false }
        );
        // Explicit evidence of no thinking support fails closed as None.
        assert_eq!(
            by_id("claude-test-nothink").reasoning,
            ReasoningSupport::None
        );
        // Absent capabilities stay Unknown (conservative, backward compatible).
        assert_eq!(
            by_id("claude-test-nocaps").reasoning,
            ReasoningSupport::Unknown
        );
    }

    #[test]
    fn live_parse_populates_capability_cache_with_qualified_ids() {
        let body = r#"{
            "data": [
                {"id": "claude-test-cache-entry",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": true}}}
            ],
            "has_more": false
        }"#;
        parse_anthropic_catalog_models(body).expect("parse");
        let cached = super::super::capability_cache::get("anthropic/claude-test-cache-entry")
            .expect("live anthropic parse must populate the capability cache");
        assert_eq!(
            cached.reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: true }
        );
    }
}
