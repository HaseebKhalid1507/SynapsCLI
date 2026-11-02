//! Process-local capability cache keyed by provider-qualified model id.
//! Populated when a live Codex catalog is parsed; consulted by validation
//! before falling back to the static table. No credentials stored.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::CatalogModel;

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// The cache's state. The process-wide one sits behind the functions of this
/// module; a test can hold its own.
#[derive(Default)]
pub(crate) struct Cache {
    models: HashMap<String, CatalogModel>,
    /// Each provider's last complete list ([`replace_provider`]): its own
    /// copy of the rows, in the catalog's order. Single inserts and
    /// incremental pages change `models` only, never a complete list.
    snapshots: HashMap<String, Vec<CatalogModel>>,
    generation: u64,
}

impl Cache {
    fn advance(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    pub(crate) fn insert(&mut self, model: CatalogModel) {
        self.models.insert(model.runtime_id(), model);
        self.advance();
    }

    pub(crate) fn populate(&mut self, models: &[CatalogModel]) {
        for m in models {
            self.models.insert(m.runtime_id(), m.clone());
        }
        self.advance();
    }

    pub(crate) fn replace_provider(&mut self, provider_key: &str, models: &[CatalogModel]) {
        let prefix = format!("{provider_key}/");
        let replacements: Vec<_> = models
            .iter()
            .filter(|model| model.provider_key == provider_key)
            .cloned()
            .map(|model| (model.runtime_id(), model))
            .collect();
        // The complete list: each runtime id once, where the catalog first
        // listed it, holding the row `models` keeps for it (the last one).
        let mut rows: Vec<CatalogModel> = Vec::with_capacity(replacements.len());
        let mut at: HashMap<&str, usize> = HashMap::with_capacity(replacements.len());
        for (runtime_id, model) in &replacements {
            match at.get(runtime_id.as_str()) {
                Some(&i) => rows[i] = model.clone(),
                None => {
                    at.insert(runtime_id.as_str(), rows.len());
                    rows.push(model.clone());
                }
            }
        }

        self.models
            .retain(|runtime_id, _| !runtime_id.starts_with(&prefix));
        self.models.extend(replacements);
        self.snapshots.insert(provider_key.to_string(), rows);
        self.advance();
    }

    pub(crate) fn provider_snapshot(&self, provider_key: &str) -> Option<Vec<CatalogModel>> {
        self.snapshots.get(provider_key).cloned()
    }
}

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// Current monotonic catalog generation. Every successful cache mutation
/// advances it, allowing UI snapshots to reject stale capability decisions.
pub fn generation() -> u64 {
    cache().lock().map(|cache| cache.generation).unwrap_or(0)
}

/// Insert or overwrite an entry keyed by `model.runtime_id()`.
pub fn insert(model: CatalogModel) {
    if let Ok(mut cache) = cache().lock() {
        cache.insert(model);
    }
}

/// Bulk-populate from an incremental parsed catalog slice.
pub fn populate(models: &[CatalogModel]) {
    if let Ok(mut cache) = cache().lock() {
        cache.populate(models);
    }
}

/// Atomically replace one provider's complete catalog snapshot.
///
/// Rows for other providers are preserved. Models whose provider identity does
/// not match `provider_key` are ignored rather than crossing provider scopes.
/// The list is also kept as it is, for [`provider_snapshot`].
pub fn replace_provider(provider_key: &str, models: &[CatalogModel]) {
    if let Ok(mut cache) = cache().lock() {
        cache.replace_provider(provider_key, models);
    }
}

/// The provider's last complete list ([`replace_provider`]), exactly as it
/// arrived: the catalog's order, each runtime id once, the rows as they were
/// then. A later [`insert`] or [`populate`] (an incremental page of a
/// refresh that may never complete) does not change it; the next complete
/// list replaces it whole. `None` when this process never received a
/// complete list for the provider.
pub fn provider_snapshot(provider_key: &str) -> Option<Vec<CatalogModel>> {
    cache().lock().ok()?.provider_snapshot(provider_key)
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
            multi_agent_reasoning_effort: None,
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
            multi_agent_reasoning_effort: None,
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

    fn live(provider: &str, id: &str) -> CatalogModel {
        let mut m = CatalogModel::new(provider, provider, id).unwrap();
        m.source = CatalogSource::Live;
        m
    }

    #[test]
    fn a_provider_snapshot_is_the_last_complete_list_in_its_order() {
        // A provider key no other test uses: the cache is process-wide.
        let key = "zz-snapshot-test";
        assert!(provider_snapshot(key).is_none(), "never listed");
        insert(live(key, "single"));
        assert!(provider_snapshot(key).is_none(), "a single insert is not a list");

        let mut b2 = live(key, "b");
        b2.label = Some("B again".into());
        replace_provider(key, &[live(key, "b"), live(key, "a"), b2, live("zz-other", "x")]);
        let rows = provider_snapshot(key).unwrap();
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"], "catalog order, once each, own provider only");
        assert_eq!(rows[0].label.as_deref(), Some("B again"), "the row the cache keeps for b");
        assert_eq!(get(&format!("{key}/b")).and_then(|m| m.label), Some("B again".into()));

        // A later single insert updates the per-model cache only.
        let mut a = live(key, "a");
        a.label = Some("A".into());
        insert(a);
        insert(live(key, "c"));
        assert_eq!(provider_snapshot(key).unwrap(), rows, "the complete list is as it arrived");
        assert_eq!(get(&format!("{key}/a")).and_then(|m| m.label), Some("A".into()));

        replace_provider(key, &[]);
        assert_eq!(provider_snapshot(key).map(|rows| rows.len()), Some(0), "an empty list is a list");
    }

    /// A refresh that fails after an incremental page: the page reaches the
    /// per-model cache (as before), the complete list stays the last one
    /// that completed, and the next complete list replaces it whole.
    #[test]
    fn an_incremental_page_never_changes_the_last_complete_list() {
        let key = "zz-page";
        let labelled = |id: &str, label: Option<&str>| {
            let mut m = live(key, id);
            m.label = label.map(str::to_string);
            m
        };
        let mut cache = Cache::default();
        let a = vec![labelled("a2", Some("A Two")), labelled("a1", None)];
        cache.replace_provider(key, &a);

        // Refresh B, page one: a1 relabelled, a new b3; then B fails.
        cache.populate(&[labelled("a1", Some("Page B")), labelled("b3", Some("B Three"))]);
        cache.insert(labelled("a2", Some("Single")));
        assert_eq!(cache.provider_snapshot(key), Some(a.clone()), "exactly A: order, labels, source");
        assert_eq!(cache.models.get("zz-page/a1").and_then(|m| m.label.clone()), Some("Page B".into()));

        // B completes: the list is B, whole.
        let b = vec![labelled("b3", Some("B Three")), labelled("a1", Some("Page B"))];
        cache.replace_provider(key, &b);
        assert_eq!(cache.provider_snapshot(key), Some(b));
        assert!(!cache.models.contains_key("zz-page/a2"), "a replacement drops the rows it no longer lists");
    }
}
