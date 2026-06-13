//! Groq `/models` parser and reasoning inference.

use serde::Deserialize;

use super::{CatalogModel, CatalogProviderKind, CatalogSource, ReasoningSupport};

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
