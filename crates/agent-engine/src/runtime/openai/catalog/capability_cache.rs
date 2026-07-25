//! Process-local capability cache keyed by provider-qualified model id.
//! Populated when a live Codex catalog is parsed; consulted by validation
//! before falling back to the static table. No credentials stored.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::CatalogModel;

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[derive(Default)]
struct Cache {
    models: HashMap<String, CatalogModel>,
    generation: u64,
}

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn advance(cache: &mut Cache) {
    cache.generation = cache.generation.saturating_add(1);
}

/// Current monotonic catalog generation. Every successful cache mutation
/// advances it, allowing UI snapshots to reject stale capability decisions.
pub fn generation() -> u64 {
    cache().lock().map(|cache| cache.generation).unwrap_or(0)
}

/// Insert or overwrite an entry keyed by `model.runtime_id()`.
pub fn insert(model: CatalogModel) {
    if let Ok(mut cache) = cache().lock() {
        cache.models.insert(model.runtime_id(), model);
        advance(&mut cache);
    }
}

/// Bulk-populate from an incremental parsed catalog slice.
pub fn populate(models: &[CatalogModel]) {
    if let Ok(mut cache) = cache().lock() {
        for m in models {
            cache.models.insert(m.runtime_id(), m.clone());
        }
        advance(&mut cache);
    }
}

/// Atomically replace one provider's complete catalog snapshot.
///
/// Rows for other providers are preserved. Models whose provider identity does
/// not match `provider_key` are ignored rather than crossing provider scopes.
pub fn replace_provider(provider_key: &str, models: &[CatalogModel]) {
    let prefix = format!("{provider_key}/");
    let replacements: Vec<_> = models
        .iter()
        .filter(|model| model.provider_key == provider_key)
        .cloned()
        .map(|model| (model.runtime_id(), model))
        .collect();

    if let Ok(mut cache) = cache().lock() {
        cache
            .models
            .retain(|runtime_id, _| !runtime_id.starts_with(&prefix));
        cache.models.extend(replacements);
        advance(&mut cache);
    }
}

/// Look up by provider-qualified id (e.g. `"openai-codex/gpt-5.6-sol"`).
pub fn get(runtime_id: &str) -> Option<CatalogModel> {
    cache().lock().ok()?.models.get(runtime_id).cloned()
}

/// True if the cache has any entries for the given provider prefix.
#[allow(dead_code)]
pub fn has_provider(provider_key: &str) -> bool {
    let prefix = format!("{provider_key}/");
    cache()
        .lock()
        .ok()
        .map(|m| m.models.keys().any(|k| k.starts_with(&prefix)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::openai::catalog::{CatalogProviderKind, CatalogSource, ReasoningSupport};
    use agent_core::reasoning::ReasoningLevel;

    fn make_model(runtime_id: &str) -> CatalogModel {
        let (provider, id) = runtime_id
            .split_once('/')
            .expect("test runtime_id must be 'provider/model'");
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
