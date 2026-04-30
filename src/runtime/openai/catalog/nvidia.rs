//! NVIDIA NIM `/models` parser, context inference, and reasoning inference.

use serde::Deserialize;

use super::{CatalogModel, CatalogProviderKind, CatalogSource, ReasoningSupport};

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
