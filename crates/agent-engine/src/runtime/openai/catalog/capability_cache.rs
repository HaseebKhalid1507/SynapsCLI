//! Process-local capability cache keyed by provider-qualified model id.
//! Populated when a live Codex catalog is parsed; consulted by validation
//! before falling back to the static table. No credentials stored.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::CatalogModel;

static CACHE: OnceLock<Mutex<HashMap<String, CatalogModel>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, CatalogModel>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Insert or overwrite an entry keyed by `model.runtime_id()`.
pub fn insert(model: CatalogModel) {
    if let Ok(mut map) = cache().lock() {
        map.insert(model.runtime_id(), model);
    }
}

/// Bulk-populate from a parsed live catalog slice.
pub fn populate(models: &[CatalogModel]) {
    if let Ok(mut map) = cache().lock() {
        for m in models {
            map.insert(m.runtime_id(), m.clone());
        }
    }
}

/// Look up by provider-qualified id (e.g. `"openai-codex/gpt-5.6-sol"`).
pub fn get(runtime_id: &str) -> Option<CatalogModel> {
    cache().lock().ok()?.get(runtime_id).cloned()
}

/// True if the cache has any entries for the given provider prefix.
#[allow(dead_code)]
pub fn has_provider(provider_key: &str) -> bool {
    let prefix = format!("{provider_key}/");
    cache()
        .lock()
        .ok()
        .map(|m| m.keys().any(|k| k.starts_with(&prefix)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::openai::catalog::{CatalogProviderKind, CatalogSource, ReasoningSupport};
    use agent_core::reasoning::ReasoningLevel;

    fn make_model(runtime_id: &str) -> CatalogModel {
        let (provider, id) = runtime_id.split_once('/').unwrap();
        let mut m = CatalogModel::new(provider, provider, id).unwrap();
        m.provider_kind = CatalogProviderKind::OpenAiCodex;
        m.source = CatalogSource::Live;
        m.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
            multi_agent_version: None,
        };
        m
    }

    #[test]
    fn insert_and_get_round_trip() {
        let m = make_model("openai-codex/gpt-test-cache");
        insert(m.clone());
        let got = get("openai-codex/gpt-test-cache").expect("should be cached");
        assert_eq!(got.id, "gpt-test-cache");
        assert!(matches!(got.reasoning, ReasoningSupport::CodexNamed { .. }));
    }

    #[test]
    fn live_cache_preserves_narrow_capability() {
        let mut live =
            CatalogModel::new("openai-codex", "OpenAI Codex", "gpt-cache-narrow").unwrap();
        live.provider_kind = CatalogProviderKind::OpenAiCodex;
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
            multi_agent_version: None,
        };
        insert(live);
        let cached = get("openai-codex/gpt-cache-narrow").unwrap();
        match cached.reasoning {
            ReasoningSupport::CodexNamed { supported, .. } => {
                assert!(
                    !supported.contains(&ReasoningLevel::Ultra),
                    "live cache must override static — Ultra should be absent"
                );
                assert!(supported.contains(&ReasoningLevel::Low));
            }
            other => panic!("expected CodexNamed, got {other:?}"),
        }
    }
}
