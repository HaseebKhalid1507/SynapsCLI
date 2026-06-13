use super::*;

pub fn codex_static_catalog_models() -> Vec<CatalogModel> {
    [
        ("gpt-5.5", "GPT-5.5"),
        ("gpt-5.4", "GPT-5.4"),
        ("gpt-5.4-mini", "GPT-5.4 Mini"),
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
