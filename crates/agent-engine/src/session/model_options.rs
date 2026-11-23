//! `SessionQuery::ModelOptions` (feature [`super::wire::FEATURE_MODEL_OPTIONS`]):
//! what a client may pick for THIS session's `Set{Model}` and
//! `Set{ReasoningLevel}`, so a thin client (the desktop app) offers the
//! same choices the TUI's pickers do without a list of its own.
//!
//! The model rows are the static rows (the TUI's `/models` sections, without
//! its live catalog refresh and favourites) of every provider THIS session's
//! credential source can serve ([`crate::Runtime::provider_availability`]:
//! its own broker, local or remote, never this machine's files when the
//! source is remote), plus the models of extension providers that are
//! enabled and have a handler, keeping only the ids `Set{Model}` would take
//! as the foreground model ([`crate::Runtime::foreground_model_refusal`]).
//! Static seeds, not entitlement or health: a listed model can still fail
//! its first request. The current model is always a row (the session's
//! state, not a promise that it is usable). The thinking levels are the
//! ones the runtime would accept for its current model right now
//! ([`crate::Runtime::reasoning_level_refusal`]). A choice that passes here
//! can still be refused at `Set` time if the session changed in between,
//! and the client shows that refusal.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use agent_core::reasoning::ReasoningLevel;
use serde::{Deserialize, Serialize};

use crate::extensions::providers::{ProviderRegistry, RegisteredProvider};
use crate::runtime::openai::{catalog, registry};

/// How long a remote credential broker may take to say what it serves.
pub const BROKER_TIMEOUT: Duration = Duration::from_secs(3);

/// One row a client can pass back as `Set{Model{model: id}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// The runtime id (`anthropic/claude-opus-5-5`, `openai-codex/gpt-5.5`).
    pub id: String,
    /// Human label; may be empty.
    #[serde(default)]
    pub label: String,
    /// Provider display name (the picker's section).
    pub provider: String,
    /// The provider's stable key (`anthropic`, `openai-codex`, an extension's
    /// `plugin:provider`): rows group by it. Empty from a daemon before it.
    #[serde(default)]
    pub provider_key: String,
}

/// The `QueryResult.value` of `SessionQuery::ModelOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOptions {
    /// The session's model now (`RuntimeView.model`).
    pub model: String,
    /// The session's thinking level now (`RuntimeView.thinking_level`).
    pub thinking_level: String,
    pub models: Vec<ModelOption>,
    /// Levels (`ReasoningLevel::as_str`) the runtime accepts for `model`.
    pub thinking: Vec<String>,
    /// Why the list is short or no model change would apply now (workers
    /// to reconcile; the credential broker did not answer). `None`: nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Every named level, in the order a picker lists them.
pub const LEVELS: [ReasoningLevel; 9] = [
    ReasoningLevel::Off,
    ReasoningLevel::Adaptive,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
    ReasoningLevel::UltraCode,
];

/// The built-in providers' keys and section names, in picker order.
const BUILT_IN: [(&str, &str); 8] = [
    ("anthropic", "Anthropic"),
    ("openai-codex", "OpenAI Codex"),
    ("xai-auth", "xAI (Grok)"),
    ("github-copilot", "GitHub Copilot"),
    ("google-gemini", "Google Gemini"),
    ("kimi-code", "Kimi Code"),
    ("muse", "Meta Muse"),
    ("local", "Local"),
];

/// The session's options. `prepare_model_capabilities` should have run for
/// the current model first (the actor does), so the level check sees live
/// capability evidence.
pub async fn session_model_options(runtime: &crate::Runtime) -> ModelOptions {
    let available = runtime.provider_availability(BROKER_TIMEOUT).await;
    let extensions = match crate::runtime::openai::extension_manager_for_routing() {
        Some(manager) => {
            let manager = manager.read().await;
            extension_rows(&manager.providers(), &manager.provider_trust_view())
        }
        None => Vec::new(),
    };
    model_options(runtime, available, extensions)
}

/// [`session_model_options`] from what it looked up: the providers the
/// session's credential source serves (or why it could not say) and the
/// usable extension providers' rows.
pub fn model_options(
    runtime: &crate::Runtime,
    available: Result<BTreeSet<String>, String>,
    extensions: Vec<ModelOption>,
) -> ModelOptions {
    let model = runtime.model().to_string();
    let (keys, mut notice) = match available {
        Ok(keys) => (keys, None),
        Err(e) => (BTreeSet::new(), Some(format!("only the current model is listed: {e}"))),
    };
    let names: BTreeMap<String, String> =
        extensions.iter().map(|m| (m.provider_key.clone(), m.provider.clone())).collect();
    let mut models = built_in_rows(&keys);
    models.extend(extensions);
    let mut seen = BTreeSet::new();
    models.retain(|m| seen.insert(m.id.clone()) && runtime.foreground_model_refusal(&m.id).is_ok());
    if !models.iter().any(|m| same_model(&m.id, &model)) {
        let key = provider_key_of(&model);
        let provider = provider_name(&key, &names);
        models.insert(
            0,
            ModelOption {
                id: model.clone(),
                label: String::new(),
                provider,
                provider_key: key,
            },
        );
    }
    if let Some(blocked) = runtime.model_switch_blocked() {
        notice = Some(blocked);
    }
    let thinking = LEVELS
        .iter()
        .filter(|l| runtime.reasoning_level_refusal(**l).is_ok())
        .map(|l| l.as_str().to_string())
        .collect();
    ModelOptions {
        thinking_level: runtime.thinking_level().to_string(),
        model,
        models,
        thinking,
        notice,
    }
}

/// A bare id and its `anthropic/` form are the same model.
fn same_model(a: &str, b: &str) -> bool {
    let bare = |m: &str| m.strip_prefix("anthropic/").unwrap_or(m).to_string();
    bare(a) == bare(b)
}

/// The provider key a model id routes to: `key/model` → `key`; an extension
/// provider's `plugin:provider:model` → `plugin:provider`; a bare id is a
/// legacy Anthropic one.
pub fn provider_key_of(id: &str) -> String {
    if let Some((key, _)) = id.split_once('/') {
        return key.to_string();
    }
    match id.rsplit_once(':') {
        Some((provider, _)) if provider.contains(':') => provider.to_string(),
        _ => "anthropic".to_string(),
    }
}

/// A provider key's section name: the built-in names, the static
/// providers' registry names, the extension providers' display names (in
/// `extensions`); else the key itself.
fn provider_name(key: &str, extensions: &BTreeMap<String, String>) -> String {
    BUILT_IN
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, name)| name.to_string())
        .or_else(|| registry::providers().iter().find(|s| s.key == key).map(|s| s.name.to_string()))
        .or_else(|| extensions.get(key).cloned())
        .unwrap_or_else(|| key.to_string())
}

fn rows(key: &str, list: impl IntoIterator<Item = (String, String)>) -> Vec<ModelOption> {
    let provider = provider_name(key, &BTreeMap::new());
    list.into_iter()
        .map(|(id, label)| ModelOption {
            id: format!("{key}/{id}"),
            label,
            provider: provider.clone(),
            provider_key: key.to_string(),
        })
        .collect()
}

fn catalog_rows(key: &str, list: Vec<catalog::CatalogModel>) -> Vec<ModelOption> {
    rows(key, list.into_iter().map(|m| (m.id, m.label.unwrap_or_default())))
}

/// The static model rows of every built-in provider in `keys` (the ones the
/// session's credential source serves). Non-secret: no key is read.
pub fn built_in_rows(keys: &BTreeSet<String>) -> Vec<ModelOption> {
    let mut out = Vec::new();
    for (key, _) in BUILT_IN {
        if !keys.contains(key) {
            continue;
        }
        out.extend(match key {
            "anthropic" => rows(
                key,
                agent_core::models::KNOWN_MODELS
                    .iter()
                    .map(|(id, label)| (id.to_string(), label.to_string())),
            ),
            "openai-codex" => catalog_rows(key, catalog::codex_static_catalog_models()),
            "xai-auth" => catalog_rows(key, catalog::xai_static_catalog_models()),
            "github-copilot" => catalog_rows(key, catalog::copilot_static_catalog_models()),
            "google-gemini" => catalog_rows(key, catalog::google_gemini_static_catalog_models()),
            "kimi-code" => catalog_rows(key, catalog::kimi_code_static_catalog_models()),
            "muse" => catalog_rows(key, catalog::muse_static_catalog_models()),
            _ => rows(
                key,
                agent_core::auth::broker::local_model_ids()
                    .into_iter()
                    .map(|id| (id, String::new())),
            ),
        });
    }
    for spec in registry::providers().iter().filter(|s| keys.contains(s.key)) {
        out.extend(rows(
            spec.key,
            spec.models
                .iter()
                .map(|(id, label, _)| (id.to_string(), label.to_string())),
        ));
    }
    out
}

/// The models of the extension providers a request could reach now: ones
/// with a handler (a registered spec alone is metadata) that the trust
/// state enables (`trust`: [`crate::extensions::manager::ExtensionManager::provider_trust_view`],
/// all disabled when it is unreadable).
pub fn extension_rows(
    providers: &[&RegisteredProvider],
    trust: &BTreeMap<String, bool>,
) -> Vec<ModelOption> {
    providers
        .iter()
        .filter(|p| p.handler.is_some() && trust.get(&p.runtime_id).copied().unwrap_or(false))
        .flat_map(|p| {
            p.spec.models.iter().map(|m| ModelOption {
                id: ProviderRegistry::model_runtime_id(&p.plugin_id, &p.provider_id, &m.id),
                label: m.display_name.clone().unwrap_or_default(),
                provider: p.spec.display_name.clone(),
                provider_key: p.runtime_id.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::extensions::runtime::process::{RegisteredProviderModelSpec, RegisteredProviderSpec};
    use crate::extensions::runtime::ExtensionHandler;
    use std::sync::Arc;

    /// Every variant of [`ReasoningLevel`], by an exhaustive match: a new
    /// level does not compile here until it is listed.
    fn every_level() -> Vec<ReasoningLevel> {
        let all = [
            ReasoningLevel::Off,
            ReasoningLevel::Adaptive,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
            ReasoningLevel::UltraCode,
        ];
        for l in all {
            match l {
                ReasoningLevel::Off
                | ReasoningLevel::Adaptive
                | ReasoningLevel::Low
                | ReasoningLevel::Medium
                | ReasoningLevel::High
                | ReasoningLevel::XHigh
                | ReasoningLevel::Max
                | ReasoningLevel::Ultra
                | ReasoningLevel::UltraCode => {}
            }
        }
        all.to_vec()
    }

    #[test]
    fn levels_list_every_reasoning_level_exactly_once() {
        for l in every_level() {
            assert_eq!(LEVELS.iter().filter(|x| **x == l).count(), 1, "{l:?}");
            assert_eq!(ReasoningLevel::parse(l.as_str()), Some(l));
        }
        assert_eq!(LEVELS.len(), every_level().len());
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ExtensionHandler for Handler {
        fn id(&self) -> &str {
            "fixture"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }

    fn provider(plugin: &str, id: &str, handler: bool) -> RegisteredProvider {
        RegisteredProvider {
            plugin_id: plugin.into(),
            provider_id: id.into(),
            runtime_id: format!("{plugin}:{id}"),
            spec: RegisteredProviderSpec {
                id: id.into(),
                display_name: format!("{plugin} {id}"),
                description: String::new(),
                models: vec![RegisteredProviderModelSpec {
                    id: "m1".into(),
                    display_name: Some("M one".into()),
                    capabilities: serde_json::Value::Null,
                    context_window: None,
                }],
                config_schema: None,
            },
            handler: handler.then(|| Arc::new(Handler) as Arc<dyn ExtensionHandler>),
        }
    }

    /// A registered spec is metadata: only a provider with a handler that
    /// the trust state enables is listed (disabled, no handler, a corrupt
    /// trust file — all disabled — are not).
    #[test]
    fn only_enabled_extension_providers_with_a_handler_are_listed() {
        let (ok, off, bare) = (provider("p", "ok", true), provider("p", "off", true), provider("p", "bare", false));
        let all = [&ok, &off, &bare];
        let trust: BTreeMap<String, bool> =
            [("p:ok".into(), true), ("p:off".into(), false), ("p:bare".into(), true)].into();
        let rows = extension_rows(&all, &trust);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, "p:ok:m1");
        assert_eq!((rows[0].provider.as_str(), rows[0].provider_key.as_str()), ("p ok", "p:ok"));
        assert_eq!(rows[0].label, "M one");
        // Unreadable trust state: `provider_trust_view` disables every one.
        let corrupt: BTreeMap<String, bool> = all.iter().map(|p| (p.runtime_id.clone(), false)).collect();
        assert!(extension_rows(&all, &corrupt).is_empty());
        // No entry at all is not an enablement.
        assert!(extension_rows(&all, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_model_id_names_its_provider() {
        assert_eq!(provider_key_of("claude-opus-4-6"), "anthropic");
        assert_eq!(provider_key_of("anthropic/claude-opus-4-6"), "anthropic");
        assert_eq!(provider_key_of("openai-codex/gpt-5.5"), "openai-codex");
        assert_eq!(provider_key_of("plug:prov:m1"), "plug:prov");
        assert_eq!(provider_name("openai-codex", &BTreeMap::new()), "OpenAI Codex");
        assert_eq!(provider_name("local", &BTreeMap::new()), "Local");
        let ext: BTreeMap<String, String> = [("plug:prov".into(), "Plug Prov".into())].into();
        assert_eq!(provider_name("plug:prov", &ext), "Plug Prov");
        assert_eq!(provider_name("nobody", &BTreeMap::new()), "nobody");
    }

    async fn runtime(model: &str, orchestrated: bool) -> crate::Runtime {
        let mut rt = crate::Runtime::new().await.expect("runtime");
        rt.set_model(model.to_string());
        assert_eq!(rt.model(), model);
        if orchestrated {
            let fg = crate::orchestration::canonical_foreground_identity(rt.model()).unwrap();
            let limits = rt.subagent_limits();
            rt.install_orchestration(Arc::new(
                crate::orchestration::OrchestrationRuntime::from_limits(fg, limits).unwrap(),
            ));
        }
        rt
    }

    /// The rows are the providers the session's credential source serves,
    /// whatever this machine's own files hold; a source that cannot say
    /// lists only the current model, and says why.
    #[tokio::test]
    async fn rows_follow_the_sessions_credential_source() {
        let rt = runtime("anthropic/claude-sonnet-4-5", false).await;
        let o = model_options(&rt, Ok(["openai-codex".to_string()].into()), Vec::new());
        assert!(o.models.iter().skip(1).all(|m| m.provider_key == "openai-codex"), "{:?}", o.models);
        assert!(o.models.len() > 1);
        assert_eq!(o.models[0].id, "anthropic/claude-sonnet-4-5", "the current model stays");
        assert_eq!(o.notice, None);
        let o = model_options(&rt, Err("broker down".into()), Vec::new());
        assert_eq!(o.models.len(), 1, "{:?}", o.models);
        assert!(o.notice.as_deref().is_some_and(|n| n.contains("broker down")), "{:?}", o.notice);
    }

    /// The current model, when no row has it, sits in its provider's
    /// section (same key and name as the rows), never a section of its own.
    #[tokio::test]
    async fn the_current_model_groups_with_its_provider() {
        let rt = runtime("claude-sonnet-4-5", false).await;
        let o = model_options(&rt, Ok(BTreeSet::new()), Vec::new());
        assert_eq!(o.models.len(), 1);
        assert_eq!((o.models[0].provider.as_str(), o.models[0].provider_key.as_str()), ("Anthropic", "anthropic"));
        let rt = runtime("openai-codex/gpt-5.4-mini", false).await;
        let o = model_options(&rt, Ok(["openai-codex".to_string()].into()), Vec::new());
        let keys: BTreeSet<&str> = o.models.iter().map(|m| m.provider_key.as_str()).collect();
        let names: BTreeSet<&str> = o.models.iter().map(|m| m.provider.as_str()).collect();
        assert_eq!((keys.len(), names.len()), (1, 1), "{:?}", o.models);
    }

    /// A row is listed only if `Set{Model}` would take it as the foreground:
    /// with manifestless orchestration (every daemon session has it) an
    /// extension provider's `plugin:provider:model` is refused, so it is
    /// not offered, however usable its provider is.
    #[tokio::test]
    async fn only_models_set_would_take_are_listed() {
        let ext = vec![ModelOption {
            id: "plug:prov:m1".into(),
            label: String::new(),
            provider: "Plug".into(),
            provider_key: "plug:prov".into(),
        }];
        let rt = runtime("anthropic/claude-sonnet-4-5", true).await;
        let mut probe = rt.clone();
        assert!(probe.try_set_model("plug:prov:m1".into()).is_err(), "Set refuses it");
        assert!(rt.foreground_model_refusal("plug:prov:m1").is_err());
        let o = model_options(&rt, Ok(["anthropic".to_string()].into()), ext);
        assert!(o.models.iter().all(|m| m.id != "plug:prov:m1"), "{:?}", o.models);
        for m in &o.models {
            let mut probe = rt.clone();
            assert!(probe.try_set_model(m.id.clone()).is_ok(), "{} is listed but Set refuses it", m.id);
        }
    }
}
