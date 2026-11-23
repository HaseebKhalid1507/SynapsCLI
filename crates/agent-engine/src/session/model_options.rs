//! `SessionQuery::ModelOptions` (feature [`super::wire::FEATURE_MODEL_OPTIONS`]):
//! what a client may pick for THIS session's `Set{Model}` and
//! `Set{ReasoningLevel}`, so a thin client (the desktop app) offers the
//! same choices the TUI's pickers do without a list of its own.
//!
//! The model rows are the static rows of every provider this daemon holds a
//! credential for (the TUI's `/models` sections, without its live catalog
//! refresh and favourites), plus extension providers' models. The thinking
//! levels are the ones the session's runtime would accept for its current
//! model right now ([`crate::Runtime::reasoning_level_refusal`]); a level
//! that passes here can still be refused at `Set` time if the session
//! changed in between, and the client shows that refusal.

use agent_core::auth::{broker, OAuthProviderId};
use agent_core::reasoning::ReasoningLevel;
use serde::{Deserialize, Serialize};

use crate::runtime::openai::{catalog, registry};

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

/// The session's options. `prepare_model_capabilities` should have run for
/// the current model first (the actor does), so the level check sees live
/// capability evidence.
pub fn session_model_options(runtime: &crate::Runtime) -> ModelOptions {
    let model = runtime.model().to_string();
    let mut models = available_models();
    if !models.iter().any(|m| same_model(&m.id, &model)) {
        let provider = model.split_once('/').map_or("", |(p, _)| p).to_string();
        models.insert(
            0,
            ModelOption {
                id: model.clone(),
                label: String::new(),
                provider,
            },
        );
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
    }
}

/// A bare id and its `anthropic/` form are the same model.
fn same_model(a: &str, b: &str) -> bool {
    let bare = |m: &str| m.strip_prefix("anthropic/").unwrap_or(m).to_string();
    bare(a) == bare(b)
}

fn rows(
    provider_key: &str,
    provider: &str,
    list: impl IntoIterator<Item = (String, String)>,
) -> Vec<ModelOption> {
    list.into_iter()
        .map(|(id, label)| ModelOption {
            id: format!("{provider_key}/{id}"),
            label,
            provider: provider.to_string(),
        })
        .collect()
}

fn catalog_rows(
    provider_key: &str,
    provider: &str,
    list: Vec<catalog::CatalogModel>,
) -> Vec<ModelOption> {
    rows(
        provider_key,
        provider,
        list.into_iter()
            .map(|m| (m.id, m.label.unwrap_or_default())),
    )
}

/// The static model rows of every provider with a credential here. Non-secret:
/// only the broker's availability answers are read, never a key.
pub fn available_models() -> Vec<ModelOption> {
    let oauth = |p: OAuthProviderId| broker::oauth_provider_logged_in(p);
    let mut out = Vec::new();
    if broker::anthropic_credential_available() {
        out.extend(rows(
            "anthropic",
            "Anthropic",
            agent_core::models::KNOWN_MODELS
                .iter()
                .map(|(id, label)| (id.to_string(), label.to_string())),
        ));
    }
    if oauth(OAuthProviderId::OpenAiCodex) {
        out.extend(catalog_rows(
            "openai-codex",
            "OpenAI Codex",
            catalog::codex_static_catalog_models(),
        ));
    }
    if oauth(OAuthProviderId::Xai) {
        out.extend(catalog_rows(
            "xai-auth",
            "xAI (Grok)",
            catalog::xai_static_catalog_models(),
        ));
    }
    if oauth(OAuthProviderId::GitHubCopilot) {
        out.extend(catalog_rows(
            "github-copilot",
            "GitHub Copilot",
            catalog::copilot_static_catalog_models(),
        ));
    }
    if oauth(OAuthProviderId::GoogleGemini) {
        out.extend(catalog_rows(
            "google-gemini",
            "Google Gemini",
            catalog::google_gemini_static_catalog_models(),
        ));
    }
    if oauth(OAuthProviderId::KimiCode) {
        out.extend(catalog_rows(
            "kimi-code",
            "Kimi Code",
            catalog::kimi_code_static_catalog_models(),
        ));
    }
    if oauth(OAuthProviderId::Muse) || agent_core::auth::muse::env_api_key_present() {
        out.extend(catalog_rows(
            "muse",
            "Meta Muse",
            catalog::muse_static_catalog_models(),
        ));
    }
    let keyed = broker::configured_static_provider_keys();
    for spec in registry::providers()
        .iter()
        .filter(|s| keyed.contains(s.key))
    {
        out.extend(rows(
            spec.key,
            spec.name,
            spec.models
                .iter()
                .map(|(id, label, _)| (id.to_string(), label.to_string())),
        ));
    }
    out.extend(rows(
        "local",
        "Local",
        broker::local_model_ids()
            .into_iter()
            .map(|id| (id, String::new())),
    ));
    if let Some(manager) = crate::runtime::openai::extension_manager_for_routing() {
        if let Ok(manager) = manager.try_read() {
            for p in manager.providers() {
                for m in &p.spec.models {
                    out.push(ModelOption {
                        id: crate::extensions::providers::ProviderRegistry::model_runtime_id(
                            &p.plugin_id,
                            &p.provider_id,
                            &m.id,
                        ),
                        label: m.display_name.clone().unwrap_or_default(),
                        provider: p.spec.display_name.clone(),
                    });
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_cover_every_reasoning_level_once() {
        let names: Vec<&str> = LEVELS.iter().map(|l| l.as_str()).collect();
        for n in &names {
            assert_eq!(ReasoningLevel::parse(n).map(|l| l.as_str()), Some(*n));
        }
        let mut uniq = names.clone();
        uniq.dedup();
        assert_eq!(uniq.len(), names.len());
    }

    #[test]
    fn options_round_trip_and_keep_the_current_model() {
        let o = ModelOptions {
            model: "anthropic/claude-opus-5-5".into(),
            thinking_level: "high".into(),
            models: vec![ModelOption {
                id: "anthropic/claude-opus-5-5".into(),
                label: "Opus".into(),
                provider: "Anthropic".into(),
            }],
            thinking: vec!["off".into(), "high".into()],
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["models"][0]["id"], "anthropic/claude-opus-5-5");
        assert_eq!(serde_json::from_value::<ModelOptions>(v).unwrap(), o);
        assert!(same_model("claude-opus-5-5", "anthropic/claude-opus-5-5"));
        assert!(!same_model("openai-codex/gpt-5.5", "gpt-5.5"));
    }
}
