use super::*;
use serde::Deserialize;

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
