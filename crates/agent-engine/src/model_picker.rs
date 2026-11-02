//! The model picker's provider sections: which providers are usable, the
//! rows each one offers, the favorites, and their order.
//!
//! The TUI's `/models` modal and `/settings` model picker build their rows
//! here (moved from agent-tui unchanged in behaviour), so every surface that
//! lists models gives the same answer from the same function.
//!
//! Plugin providers come from an [`ExtensionManager`] the caller reads. The
//! read is taken only for the plugin rows, after the built-in rows, and
//! released at once: [`build_sections`] takes it through an accessor (the
//! TUI's non-blocking read), and an async caller builds
//! [`builtin_sections`] first and then [`plugin_sections`] under its own
//! read.

use std::collections::{BTreeMap, BTreeSet};

use crate::extensions::manager::ExtensionManager;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevProviderSelection {
    pub key: &'static str,
    pub name: &'static str,
    pub auth_kind: DevProviderAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevProviderAuth {
    OAuth(&'static str),
    Cloud,
    ApiKey,
    LocalModels,
}

pub fn dev_model_providers() -> Vec<DevProviderSelection> {
    let mut providers = vec![
        DevProviderSelection {
            key: "anthropic",
            name: "Anthropic",
            auth_kind: DevProviderAuth::OAuth("anthropic"),
        },
        DevProviderSelection {
            key: "openai-codex",
            name: "OpenAI Codex",
            auth_kind: DevProviderAuth::OAuth("openai-codex"),
        },
        DevProviderSelection {
            key: "xai-auth",
            name: "xAI (Grok)",
            auth_kind: DevProviderAuth::OAuth("xai-auth"),
        },
        DevProviderSelection {
            key: "github-copilot",
            name: "GitHub Copilot",
            auth_kind: DevProviderAuth::OAuth("github-copilot"),
        },
        DevProviderSelection {
            key: "google-gemini",
            name: "Google Gemini (Code Assist)",
            auth_kind: DevProviderAuth::OAuth("google-gemini"),
        },
        DevProviderSelection {
            key: "kimi-code",
            name: "Kimi Code",
            auth_kind: DevProviderAuth::OAuth("kimi-code"),
        },
        DevProviderSelection {
            key: "muse",
            name: "Meta Muse",
            auth_kind: DevProviderAuth::OAuth("muse"),
        },
        DevProviderSelection {
            key: "azure-openai",
            name: "Azure OpenAI",
            auth_kind: DevProviderAuth::Cloud,
        },
        DevProviderSelection {
            key: "aws-bedrock",
            name: "AWS Bedrock",
            auth_kind: DevProviderAuth::Cloud,
        },
        DevProviderSelection {
            key: "google-vertex",
            name: "Google Vertex",
            auth_kind: DevProviderAuth::Cloud,
        },
    ];

    providers.extend(
        crate::runtime::openai::registry::providers()
            .iter()
            .map(|spec| DevProviderSelection {
                key: spec.key,
                name: spec.name,
                auth_kind: DevProviderAuth::ApiKey,
            }),
    );

    providers.push(DevProviderSelection {
        key: "local",
        name: "Local",
        auth_kind: DevProviderAuth::LocalModels,
    });

    providers
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub display_id: String,
    pub label: String,
    pub tier: String,
    pub provider_key: String,
    pub provider_name: String,
    pub favorite_id: String,
    pub configured: bool,
    pub is_current: bool,
    pub is_favorite: bool,
    pub order: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSection {
    pub provider_key: String,
    pub provider_name: String,
    pub configured: bool,
    pub entries: Vec<ModelEntry>,
}

/// Live catalog rows for a provider that should replace static main-section seeds.
pub type ProviderCatalogOverride = Vec<(String, String)>; // (model_id, label)

/// A live catalog's rows as this process holds them: (model id, the
/// catalog's own display name, `None` when it gave none).
pub type HeldCatalogRows = Vec<(String, Option<String>)>;

pub fn normalize_favorite_id(model: &str) -> String {
    if let Some(id) = model.strip_prefix("claude/") {
        format!("anthropic/{id}")
    } else if model.contains('/') {
        model.to_string()
    } else if model.starts_with("claude-") {
        format!("anthropic/{model}")
    } else {
        model.to_string()
    }
}

/// What the picker is showing: the search text, the view (favorites only or
/// all), the favorites, and the live catalog rows fetched so far.
#[derive(Clone, Copy, Debug)]
pub struct PickerQuery<'a> {
    pub search: &'a str,
    pub favorites_only: bool,
    pub favorites: &'a BTreeSet<String>,
    pub catalog_overrides: &'a BTreeMap<String, ProviderCatalogOverride>,
}

/// Every section the picker shows. `plugins` is called once, after the
/// built-in sections are built, and its read is dropped once the plugin
/// rows are taken; `None` (no manager, or busy) means no plugin rows.
pub fn build_sections<G>(
    current_model: &str,
    query: &PickerQuery<'_>,
    plugins: impl FnOnce() -> Option<G>,
) -> Vec<ModelSection>
where
    G: std::ops::Deref<Target = ExtensionManager>,
{
    let availability = provider_availability();
    let logged_in_oauth = logged_in_oauth_providers();
    build_sections_from_parts(current_model, query, &availability, &logged_in_oauth, plugins)
}

/// Non-secret availability data from the credential broker boundary — this
/// module never reads provider keys or credential env vars.
pub fn provider_availability() -> ProviderAvailability {
    ProviderAvailability {
        configured_static: crate::auth::broker::configured_static_provider_keys(),
        local_models: crate::auth::broker::local_model_ids(),
    }
}

/// Non-secret provider availability: which static providers have a
/// broker-held credential, and the configured local model list.
#[derive(Clone, Debug, Default)]
pub struct ProviderAvailability {
    pub configured_static: BTreeSet<String>,
    pub local_models: Vec<String>,
}

pub(crate) fn logged_in_oauth_providers() -> BTreeSet<&'static str> {
    // Non-secret login-status queries via the broker boundary.
    [
        crate::auth::OAuthProviderId::Anthropic,
        crate::auth::OAuthProviderId::OpenAiCodex,
        crate::auth::OAuthProviderId::Xai,
        crate::auth::OAuthProviderId::GitHubCopilot,
        crate::auth::OAuthProviderId::GoogleGemini,
        crate::auth::OAuthProviderId::KimiCode,
        crate::auth::OAuthProviderId::Muse,
    ]
    .into_iter()
    .filter(|id| {
        crate::auth::broker::oauth_provider_logged_in(*id)
            // Muse also runs on the official `META_API_KEY` override alone.
            || (*id == crate::auth::OAuthProviderId::Muse
                && crate::auth::muse::env_api_key_present())
    })
    .map(|id| id.as_str())
    .collect()
}

/// [`build_sections`] with the availability and logins given.
pub fn build_sections_from_parts<G>(
    current_model: &str,
    picker: &PickerQuery<'_>,
    availability: &ProviderAvailability,
    logged_in_oauth: &BTreeSet<&'static str>,
    plugins: impl FnOnce() -> Option<G>,
) -> Vec<ModelSection>
where
    G: std::ops::Deref<Target = ExtensionManager>,
{
    let mut sections = builtin_sections(current_model, picker, availability, logged_in_oauth);
    // The manager is read only now, for the plugin rows, and released at once.
    if let Some(manager) = plugins() {
        sections.extend(plugin_sections(current_model, picker, &manager));
    }
    sections
}

/// The sections of the built-in providers that are usable, in provider order.
pub fn builtin_sections(
    current_model: &str,
    picker: &PickerQuery<'_>,
    availability: &ProviderAvailability,
    logged_in_oauth: &BTreeSet<&'static str>,
) -> Vec<ModelSection> {
    let query = picker.search.trim().to_lowercase();
    let favorites_only = picker.favorites_only;
    let current_fav = normalize_favorite_id(current_model);
    let mut sections = Vec::new();

    for provider in dev_model_providers() {
        if !dev_provider_is_logged_in(&provider, availability, logged_in_oauth) {
            continue;
        }

        let mut entries: Vec<ModelEntry> =
            provider_model_selections(&provider, availability, picker.catalog_overrides)
                .into_iter()
                .map(|model| {
                    let (runtime_id, favorite_id) = if provider.key == "claude" {
                        (model.id.clone(), format!("claude/{}", model.id))
                    } else {
                        let full_id = format!("{}/{}", provider.key, model.id);
                        (full_id.clone(), full_id)
                    };
                    ModelEntry {
                        id: runtime_id.clone(),
                        display_id: model.id.clone(),
                        label: model.label.clone(),
                        tier: model.tier.clone(),
                        provider_key: provider.key.to_string(),
                        provider_name: provider.name.to_string(),
                        configured: true,
                        is_current: current_model == runtime_id || current_fav == favorite_id,
                        is_favorite: picker
                            .favorites
                            .iter()
                            .any(|id| normalize_favorite_id(id) == favorite_id),
                        favorite_id,
                        order: model.order,
                    }
                })
                .filter(|m| model_matches(m, &query, favorites_only))
                .collect();
        pin_favorites_first(&mut entries);
        if !entries.is_empty() || provider.auth_kind == DevProviderAuth::Cloud {
            sections.push(ModelSection {
                provider_key: provider.key.to_string(),
                provider_name: provider.name.to_string(),
                configured: true,
                entries,
            });
        }
    }

    sections
}

/// The sections of the manager's plugin providers, in the manager's order
/// (by runtime id). Plugin favorites match exactly, never normalized.
pub fn plugin_sections(
    current_model: &str,
    picker: &PickerQuery<'_>,
    manager: &ExtensionManager,
) -> Vec<ModelSection> {
    let query = picker.search.trim().to_lowercase();
    let favorites_only = picker.favorites_only;
    let mut sections = Vec::new();
    for provider in manager.providers() {
        let mut entries: Vec<ModelEntry> = plugin_model_rows(provider)
            .into_iter()
            .map(|row| {
                let label = row.label();
                ModelEntry {
                    id: row.runtime_id.clone(),
                    display_id: row.model_id,
                    label,
                    tier: "extension".to_string(),
                    provider_key: provider.runtime_id.clone(),
                    provider_name: provider.spec.display_name.clone(),
                    configured: true,
                    is_current: current_model == row.runtime_id,
                    is_favorite: picker.favorites.contains(&row.runtime_id),
                    favorite_id: row.runtime_id,
                    order: row.order,
                }
            })
            .filter(|m| model_matches(m, &query, favorites_only))
            .collect();
        pin_favorites_first(&mut entries);
        if !entries.is_empty() {
            sections.push(ModelSection {
                provider_key: provider.runtime_id.clone(),
                provider_name: provider.spec.display_name.clone(),
                configured: true,
                entries,
            });
        }
    }

    sections
}

/// One model of a plugin provider, in the provider's own order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginModelRow {
    /// `plugin:provider:model`, the id a session's model setting takes.
    pub runtime_id: String,
    /// The model's id within its provider.
    pub model_id: String,
    /// The model's display name, if the provider gave one.
    pub display_name: Option<String>,
    /// "tool-use" and/or "streaming", from the model's capabilities.
    pub badges: Vec<&'static str>,
    pub order: usize,
}

impl PluginModelRow {
    /// The picker's label: the display name (else the model id), followed by
    /// the badges as " [tool-use] [streaming]".
    pub fn label(&self) -> String {
        let base = self
            .display_name
            .clone()
            .unwrap_or_else(|| self.model_id.clone());
        if self.badges.is_empty() {
            base
        } else {
            let suffix = self
                .badges
                .iter()
                .map(|b| format!("[{}]", b))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} {}", base, suffix)
        }
    }
}

/// The rows of one registered plugin provider.
pub fn plugin_model_rows(
    provider: &crate::extensions::providers::RegisteredProvider,
) -> Vec<PluginModelRow> {
    provider
        .spec
        .models
        .iter()
        .enumerate()
        .map(|(order, model)| {
            let runtime_id = crate::extensions::providers::ProviderRegistry::model_runtime_id(
                &provider.plugin_id,
                &provider.provider_id,
                &model.id,
            );
            let mut badges: Vec<&'static str> = Vec::new();
            if model
                .capabilities
                .get("tool_use")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                badges.push("tool-use");
            }
            if model
                .capabilities
                .get("streaming")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                badges.push("streaming");
            }
            PluginModelRow {
                runtime_id,
                model_id: model.id.clone(),
                display_name: model.display_name.clone(),
                badges,
                order,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelectionItem {
    pub id: String,
    pub label: String,
    pub tier: String,
    pub order: usize,
}

pub fn provider_model_selections(
    provider: &DevProviderSelection,
    availability: &ProviderAvailability,
    catalog_overrides: &BTreeMap<String, ProviderCatalogOverride>,
) -> Vec<ModelSelectionItem> {
    if provider.auth_kind == DevProviderAuth::LocalModels {
        return availability
            .local_models
            .clone()
            .into_iter()
            .enumerate()
            .map(|(order, id)| ModelSelectionItem {
                label: local_model_label(&id),
                id,
                tier: "local".to_string(),
                order,
            })
            .collect();
    }

    // OpenAI Codex OAuth picker rows are HARDCODED to the seven
    // source-controlled slugs (`codex_static_catalog_models`). Live catalog
    // overrides — however they got injected — must never replace them. This
    // single choke point covers both /models and /settings (shared sections).
    if provider.key != "openai-codex" {
        if let Some(live) = catalog_overrides.get(provider.key) {
            if !live.is_empty() {
                return live
                    .iter()
                    .enumerate()
                    .map(|(order, (id, label))| ModelSelectionItem {
                        id: id.clone(),
                        label: label.clone(),
                        tier: provider_model_tier(provider.key, id).to_string(),
                        order,
                    })
                    .collect();
            }
        }
    }

    provider_static_model_seeds(provider)
        .into_iter()
        .enumerate()
        .map(|(order, (id, label, tier))| ModelSelectionItem {
            id,
            label,
            tier,
            order,
        })
        .collect()
}

pub(crate) fn provider_model_tier(provider_key: &str, model_id: &str) -> &'static str {
    match (provider_key, model_id) {
        ("openai-codex", "gpt-6.1-sol" | "gpt-5.6-sol") => "S+",
        ("openai-codex", "gpt-5.6-terra" | "gpt-5.5") => "A",
        ("openai-codex", "gpt-5.6-luna") => "B+",
        _ => "",
    }
}

pub fn provider_static_model_seeds(provider: &DevProviderSelection) -> Vec<(String, String, String)> {
    match provider.key {
        "anthropic" => crate::models::KNOWN_MODELS
            .iter()
            .enumerate()
            .map(|(idx, (id, label))| {
                let tier = match idx {
                    0 => "S+",
                    1 | 2 => "S",
                    _ => "A",
                };
                ((*id).to_string(), (*label).to_string(), tier.to_string())
            })
            .collect(),
        "openai-codex" => crate::runtime::openai::catalog::codex_static_catalog_models()
            .into_iter()
            .map(|model| {
                let tier = provider_model_tier(provider.key, &model.id);
                (model.id, model.label.unwrap_or_default(), tier.to_string())
            })
            .collect(),
        "xai-auth" => crate::runtime::openai::catalog::xai_static_catalog_models()
            .into_iter()
            .map(|model| (model.id, model.label.unwrap_or_default(), String::new()))
            .collect(),
        // Server-driven catalog: only the documented seed ships statically;
        // the live list replaces it once fetched (`e` / auto-refresh).
        "muse" => crate::runtime::openai::catalog::muse_static_catalog_models()
            .into_iter()
            .map(|model| (model.id, model.label.unwrap_or_default(), String::new()))
            .collect(),
        "github-copilot" => crate::runtime::openai::catalog::copilot_static_catalog_models()
            .into_iter()
            .map(|model| {
                // Tiers are UI-only hints. Every id here is fixture-selectable
                // (see `COPILOT_FALLBACK_MODELS`); disabled/endpointless rows
                // never reach this list.
                let tier = match model.id.as_str() {
                    "gpt-5.3-codex" | "gpt-5.6-luna" | "gpt-5.6-terra" => "S+",
                    "gpt-5.4" | "gpt-5.4-mini" | "claude-sonnet-4.6" | "claude-sonnet-5"
                    | "claude-haiku-4.5" => "S",
                    _ => "",
                };
                (model.id, model.label.unwrap_or_default(), tier.to_string())
            })
            .collect(),
        "google-gemini" => {
            crate::runtime::openai::catalog::google_gemini_static_catalog_models()
                .into_iter()
                .map(|model| {
                    let tier = match model.id.as_str() {
                        "gemini-2.5-pro" => "S",
                        "gemini-2.5-flash" => "A",
                        _ => "",
                    };
                    (model.id, model.label.unwrap_or_default(), tier.to_string())
                })
                .collect()
        }
        "kimi-code" => crate::runtime::openai::catalog::kimi_code_static_catalog_models()
            .into_iter()
            .map(|model| {
                // UI-only tier hints for the managed Kimi Code catalog.
                let tier = match model.id.as_str() {
                    "k3" | "k3-256k" => "S+",
                    "kimi-for-coding" => "S",
                    _ => "",
                };
                (model.id, model.label.unwrap_or_default(), tier.to_string())
            })
            .collect(),
        key => crate::runtime::openai::registry::providers()
            .iter()
            .find(|spec| spec.key == key)
            .map(|spec| {
                spec.models
                    .iter()
                    .map(|(id, label, tier)| {
                        ((*id).to_string(), (*label).to_string(), (*tier).to_string())
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

pub(crate) fn local_model_label(id: &str) -> String {
    id.split(':')
        .next()
        .unwrap_or(id)
        .replace('-', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn dev_provider_is_logged_in(
    provider: &DevProviderSelection,
    availability: &ProviderAvailability,
    logged_in_oauth: &BTreeSet<&'static str>,
) -> bool {
    match provider.auth_kind {
        DevProviderAuth::LocalModels => !availability.local_models.is_empty(),
        DevProviderAuth::Cloud => true,
        DevProviderAuth::ApiKey => availability.configured_static.contains(provider.key),
        DevProviderAuth::OAuth(storage_key) => logged_in_oauth.contains(storage_key),
    }
}

pub(crate) fn model_matches(model: &ModelEntry, query: &str, favorites_only: bool) -> bool {
    if favorites_only && !model.is_favorite {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {}",
        model.id, model.display_id, model.label, model.provider_name
    )
    .to_lowercase();
    haystack.contains(query)
}

pub(crate) fn pin_favorites_first(entries: &mut [ModelEntry]) {
    entries.sort_by(|a, b| {
        b.is_favorite
            .cmp(&a.is_favorite)
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.display_id.cmp(&b.display_id))
    });
}

// ── the Favorites view (`model_favorites`, `synaps models --favorites`) ───────

/// One row of the picker's Favorites view.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FavoriteModel {
    /// Exactly the string a session's model setting takes: what the picker
    /// applies when the row is chosen (`plugin:provider:model` for a plugin).
    /// A setting string, not a permission: a session's own policy (an
    /// orchestration session with workers running, say) may still refuse
    /// the switch, as it refuses the picker's.
    pub id: String,
    /// The plain display name, as its source gives it. `None` only when the
    /// model has none (no display name, or an empty one); the picker then
    /// shows the model id in its place (a static row: the id alone).
    #[serde(default)]
    pub label: Option<String>,
    pub provider_key: String,
    pub provider_name: String,
    /// A plugin model's capabilities, "tool-use" then "streaming" (the
    /// picker prints them after the label); empty for every other row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
}

/// The Favorites view: the favorites of every usable provider, in the
/// picker's order (built-in providers in provider order, then the plugin
/// providers by runtime id; within a provider, its own model order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FavoritesView {
    pub models: Vec<FavoriteModel>,
    /// Usable providers whose rows came from a live catalog this process
    /// already held, in provider order.
    pub live_providers: Vec<String>,
}

/// What a Favorites answer reads from files: the favorites, the provider
/// availability and the OAuth logins (non-secret; blocking reads).
#[derive(Clone, Debug, Default)]
pub struct FavoritesInputs {
    /// Normalized as the picker holds them ([`configured_favorites`]).
    pub favorites: BTreeSet<String>,
    pub availability: ProviderAvailability,
    pub logged_in_oauth: BTreeSet<&'static str>,
}

impl FavoritesInputs {
    /// Read now: `favorite_models` from the config, the broker's availability
    /// and the logins.
    pub fn read() -> Self {
        Self {
            favorites: configured_favorites(),
            availability: provider_availability(),
            logged_in_oauth: logged_in_oauth_providers(),
        }
    }
}

/// `favorite_models` from the config, normalized as the picker does
/// (`claude/x` and a bare `claude-x` are `anthropic/...`).
pub fn configured_favorites() -> BTreeSet<String> {
    crate::config::load_config()
        .favorite_models
        .into_iter()
        .map(|id| normalize_favorite_id(&id))
        .collect()
}

/// The built-in providers' part of the Favorites view, with `held` as the
/// live catalog rows. The rows and their order are the picker's; a live
/// row's label is its catalog's own display name (`None` stays `None`,
/// where the picker shows the id instead).
pub fn builtin_favorites(
    inputs: &FavoritesInputs,
    held: &BTreeMap<String, HeldCatalogRows>,
) -> FavoritesView {
    let overrides = picker_overrides(held);
    let live_providers: Vec<String> = dev_model_providers()
        .into_iter()
        .filter(|provider| {
            dev_provider_is_logged_in(provider, &inputs.availability, &inputs.logged_in_oauth)
                && takes_live_rows(provider, &overrides)
        })
        .map(|provider| provider.key.to_string())
        .collect();
    let picker = favorites_query(&inputs.favorites, &overrides);
    let models = builtin_sections("", &picker, &inputs.availability, &inputs.logged_in_oauth)
        .into_iter()
        .flat_map(|section| section.entries)
        .map(|entry| {
            let label = if live_providers.contains(&entry.provider_key) {
                held_label(held, &entry.provider_key, &entry.display_id)
            } else {
                Some(entry.label)
            };
            FavoriteModel {
                id: entry.id,
                label: label.filter(|label| !label.is_empty()),
                provider_key: entry.provider_key,
                provider_name: entry.provider_name,
                badges: Vec::new(),
            }
        })
        .collect();
    FavoritesView {
        models,
        live_providers,
    }
}

/// The plugin providers' part of the Favorites view. Plugin ids match the
/// favorites exactly. The label is the plain display name (`None` when the
/// provider gave none or an empty one); the badges are their own field.
pub fn plugin_favorites(
    favorites: &BTreeSet<String>,
    manager: &ExtensionManager,
) -> Vec<FavoriteModel> {
    let none = BTreeMap::new();
    let picker = favorites_query(favorites, &none);
    let details: std::collections::HashMap<String, PluginModelRow> = manager
        .providers()
        .into_iter()
        .flat_map(plugin_model_rows)
        .map(|row| (row.runtime_id.clone(), row))
        .collect();
    plugin_sections("", &picker, manager)
        .into_iter()
        .flat_map(|section| section.entries)
        .map(|entry| {
            let row = details.get(&entry.id);
            FavoriteModel {
                label: row
                    .and_then(|row| row.display_name.clone())
                    .filter(|label| !label.is_empty()),
                badges: row
                    .map(|row| row.badges.iter().map(|b| (*b).to_string()).collect())
                    .unwrap_or_default(),
                id: entry.id,
                provider_key: entry.provider_key,
                provider_name: entry.provider_name,
            }
        })
        .collect()
}

/// The Favorites view this process can give without extensions or a live
/// catalog: the CLI's answer when no daemon answers. No network request,
/// no token refresh.
pub fn local_favorites_view() -> FavoritesView {
    builtin_favorites(&FavoritesInputs::read(), &BTreeMap::new())
}

/// The live catalog rows this process already holds: each built-in
/// provider's last complete snapshot when every row of it is live, with the
/// catalog's own labels. Never `openai-codex` (its rows are
/// source-controlled) or local (its rows are the configured list). An empty
/// list gives no rows, so the picker keeps its static seeds, as it does for
/// an empty fetch. Reads memory only.
pub fn held_catalog_rows() -> BTreeMap<String, HeldCatalogRows> {
    catalog_rows_from(crate::runtime::openai::catalog::capability_cache::provider_snapshot)
}

/// [`held_catalog_rows`] over `snapshot` (a provider's complete list).
fn catalog_rows_from(
    snapshot: impl Fn(&str) -> Option<Vec<crate::runtime::openai::catalog::CatalogModel>>,
) -> BTreeMap<String, HeldCatalogRows> {
    use crate::runtime::openai::catalog::CatalogSource;
    dev_model_providers()
        .into_iter()
        .filter(|provider| provider.auth_kind != DevProviderAuth::LocalModels && provider.key != "openai-codex")
        .filter_map(|provider| {
            let snapshot = snapshot(provider.key)?;
            if snapshot.is_empty() || snapshot.iter().any(|model| model.source != CatalogSource::Live) {
                return None;
            }
            // The ids as the TUI turns a fetched list into override rows.
            let prefix = format!("{}/", provider.key);
            let rows: HeldCatalogRows = snapshot
                .iter()
                .filter_map(|model| {
                    let runtime_id = model.runtime_id();
                    let bare = runtime_id.strip_prefix(&prefix).unwrap_or(&runtime_id).to_string();
                    (!bare.trim().is_empty()).then(|| (bare, model.label.clone()))
                })
                .collect();
            (!rows.is_empty()).then(|| (provider.key.to_string(), rows))
        })
        .collect()
}

/// The picker's override rows for `held`: as the TUI shows a fetched row
/// (`CatalogModel::display_label`), a missing display name shows the id.
fn picker_overrides(held: &BTreeMap<String, HeldCatalogRows>) -> BTreeMap<String, ProviderCatalogOverride> {
    held.iter()
        .map(|(key, rows)| {
            let rows = rows
                .iter()
                .map(|(id, label)| (id.clone(), label.clone().unwrap_or_else(|| id.clone())))
                .collect();
            (key.clone(), rows)
        })
        .collect()
}

/// The catalog's own label for `model_id` in `held`'s rows of `provider_key`.
fn held_label(held: &BTreeMap<String, HeldCatalogRows>, provider_key: &str, model_id: &str) -> Option<String> {
    held.get(provider_key)?
        .iter()
        .find(|(id, _)| id == model_id)
        .and_then(|(_, label)| label.clone())
}

fn favorites_query<'a>(
    favorites: &'a BTreeSet<String>,
    overrides: &'a BTreeMap<String, ProviderCatalogOverride>,
) -> PickerQuery<'a> {
    PickerQuery {
        search: "",
        favorites_only: true,
        favorites,
        catalog_overrides: overrides,
    }
}

/// True when [`provider_model_selections`] takes this provider's rows from
/// `overrides` (the same three conditions).
fn takes_live_rows(
    provider: &DevProviderSelection,
    overrides: &BTreeMap<String, ProviderCatalogOverride>,
) -> bool {
    provider.auth_kind != DevProviderAuth::LocalModels
        && provider.key != "openai-codex"
        && overrides.get(provider.key).is_some_and(|rows| !rows.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::extensions::hooks::HookBus;
    use crate::extensions::runtime::process::{RegisteredProviderModelSpec, RegisteredProviderSpec};
    use std::sync::Arc;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|id| normalize_favorite_id(id)).collect()
    }

    /// Inputs with the given favorites, keyed providers and logins; local
    /// models llama3:8b and qwen3:8b.
    pub(crate) fn inputs(favorites: &[&str], configured: &[&str], logins: &[&'static str]) -> FavoritesInputs {
        FavoritesInputs {
            favorites: set(favorites),
            availability: ProviderAvailability {
                configured_static: configured.iter().map(|k| k.to_string()).collect(),
                local_models: vec!["llama3:8b".into(), "qwen3:8b".into()],
            },
            logged_in_oauth: logins.iter().copied().collect(),
        }
    }

    fn ids(view: &[FavoriteModel]) -> Vec<String> {
        view.iter().map(|m| m.id.clone()).collect()
    }

    fn known(i: usize) -> (&'static str, &'static str) {
        crate::models::KNOWN_MODELS[i]
    }

    fn codex_first() -> crate::runtime::openai::catalog::CatalogModel {
        crate::runtime::openai::catalog::codex_static_catalog_models().remove(0)
    }

    fn registry_first() -> (&'static str, &'static str, &'static str) {
        let spec = &crate::runtime::openai::registry::providers()[0];
        (spec.key, spec.name, spec.models[0].0)
    }

    /// A manager holding one plugin provider `plug:prov` with three models:
    /// m1 (a display name, tool use and streaming), m2 (neither) and m3 (an
    /// empty display name).
    pub(crate) fn manager() -> crate::extensions::manager::ExtensionManager {
        let mut mgr = crate::extensions::manager::ExtensionManager::new(Arc::new(HookBus::new()));
        let spec = RegisteredProviderSpec {
            id: "prov".into(),
            display_name: "Plug Provider".into(),
            description: "test".into(),
            models: vec![
                RegisteredProviderModelSpec {
                    id: "m1".into(),
                    display_name: Some("Model One".into()),
                    capabilities: serde_json::json!({"streaming": true, "tool_use": true}),
                    context_window: None,
                },
                RegisteredProviderModelSpec {
                    id: "m2".into(),
                    display_name: None,
                    capabilities: serde_json::Value::Null,
                    context_window: None,
                },
                RegisteredProviderModelSpec {
                    id: "m3".into(),
                    display_name: Some(String::new()),
                    capabilities: serde_json::Value::Null,
                    context_window: None,
                },
            ],
            config_schema: None,
        };
        mgr.register_provider_for_tests("plug", spec).unwrap();
        mgr
    }

    #[test]
    fn favorites_follow_the_picker_order_and_hide_unusable_providers() {
        let (k0, l0) = known(0);
        let (k1, l1) = known(1);
        let codex = codex_first();
        let codex_id = codex.runtime_id();
        let (reg_key, reg_name, reg_model) = registry_first();
        let reg_id = format!("{reg_key}/{reg_model}");
        let claude_alias = format!("claude/{k1}");
        let favorites = [
            "local/qwen3:8b",
            reg_id.as_str(),
            claude_alias.as_str(), // the legacy spelling of anthropic/<k1>
            k0,                    // a bare claude- id: anthropic/<k0>
            codex_id.as_str(),
            "xai-auth/grok-4.5", // xAI is not logged in: hidden
            "local/not-configured",
        ];
        let got = builtin_favorites(&inputs(&favorites, &[reg_key], &["anthropic", "openai-codex"]), &BTreeMap::new());
        assert_eq!(
            ids(&got.models),
            [format!("anthropic/{k0}"), format!("anthropic/{k1}"), codex_id.clone(), reg_id.clone(), "local/qwen3:8b".into()],
            "OAuth providers, then the API-key registry, then local; each in its own model order"
        );
        let anthropic = &got.models[0];
        assert_eq!((anthropic.label.as_deref(), anthropic.provider_name.as_str()), (Some(l0), "Anthropic"));
        assert_eq!(got.models[1].label.as_deref(), Some(l1));
        assert_eq!(got.models[3].provider_name, reg_name);
        assert_eq!((got.models[4].label.as_deref(), got.models[4].provider_key.as_str()), (Some("Qwen3"), "local"));
        assert!(got.live_providers.is_empty());
        assert!(got.models.iter().all(|m| m.badges.is_empty()));

        // Without the logins and the key, only local is left.
        let got = builtin_favorites(&inputs(&favorites, &[], &[]), &BTreeMap::new());
        assert_eq!(ids(&got.models), ["local/qwen3:8b"]);
        // No favorites: an empty view.
        assert!(builtin_favorites(&inputs(&[], &[reg_key], &["anthropic"]), &BTreeMap::new()).models.is_empty());
    }

    #[test]
    fn live_rows_replace_seeds_for_usable_providers_but_never_for_codex() {
        let codex = codex_first();
        let codex_id = codex.runtime_id();
        let some = |label: &str| Some(label.to_string());
        let mut held = BTreeMap::new();
        held.insert(
            "xai-auth".to_string(),
            vec![("grok-live".to_string(), some("Grok Live")), ("grok-blank".to_string(), some("")), ("grok-none".to_string(), None)],
        );
        held.insert("kimi-code".to_string(), vec![("k-live".to_string(), some("K Live"))]);
        held.insert("openai-codex".to_string(), vec![("o-live".to_string(), some("O Live"))]);
        let favorites = ["xai-auth/grok-live", "xai-auth/grok-blank", "xai-auth/grok-none", "kimi-code/k-live", codex_id.as_str(), "openai-codex/o-live"];
        let given = inputs(&favorites, &[], &["openai-codex", "xai-auth"]);
        let got = builtin_favorites(&given, &held);
        assert_eq!(
            ids(&got.models),
            [codex_id.clone(), "xai-auth/grok-live".into(), "xai-auth/grok-blank".into(), "xai-auth/grok-none".into()],
            "codex keeps its static rows; kimi is not logged in"
        );
        assert_eq!(got.models[0].label.as_deref(), codex.label.as_deref(), "a static row keeps its label");
        assert_eq!(got.models[1].label.as_deref(), Some("Grok Live"));
        assert_eq!(got.models[2].label, None, "an empty display name is no label");
        assert_eq!(got.models[3].label, None, "no display name stays no label, never the id");
        assert_eq!(got.live_providers, ["xai-auth"]);

        // The picker itself still shows the id for a live row with no name.
        let overrides = picker_overrides(&held);
        let picker = favorites_query(&given.favorites, &overrides);
        let shown: Vec<(String, String)> = builtin_sections("", &picker, &given.availability, &given.logged_in_oauth)
            .into_iter()
            .filter(|section| section.provider_key == "xai-auth")
            .flat_map(|section| section.entries)
            .map(|entry| (entry.display_id, entry.label))
            .collect();
        let pair = |id: &str, label: &str| (id.to_string(), label.to_string());
        assert_eq!(shown, [pair("grok-live", "Grok Live"), pair("grok-blank", ""), pair("grok-none", "grok-none")]);
    }

    #[test]
    fn plugin_favorites_match_exactly_and_keep_the_badges_apart() {
        let mgr = manager();
        let favorites = set(&["plug:prov:m3", "plug:prov:m2", "plug:prov:m1", "PLUG:prov:m1", "plug/prov/m1"]);
        let got = plugin_favorites(&favorites, &mgr);
        assert_eq!(ids(&got), ["plug:prov:m1", "plug:prov:m2", "plug:prov:m3"], "the provider's own order, exact ids only");
        assert_eq!(got[2].label, None, "an empty display name is no label");
        assert_eq!(got[0].label.as_deref(), Some("Model One"), "the plain name, no badges");
        assert_eq!(got[0].badges, ["tool-use", "streaming"]);
        assert_eq!((got[0].provider_key.as_str(), got[0].provider_name.as_str()), ("plug:prov", "Plug Provider"));
        assert_eq!(got[1].label, None, "no display name: the picker shows the model id");
        assert!(got[1].badges.is_empty());
        // The picker's own row for m1 still carries the badges in its label.
        let none = BTreeMap::new();
        let sections = plugin_sections("", &favorites_query(&favorites, &none), &mgr);
        assert_eq!(sections[0].entries[0].label, "Model One [tool-use] [streaming]");
        assert!(plugin_favorites(&set(&["plug:prov:zz"]), &mgr).is_empty());
    }

    #[test]
    fn build_sections_reads_the_manager_after_the_builtin_rows_only() {
        let mgr = manager();
        let favorites = set(&["plug:prov:m1", "local/qwen3:8b"]);
        let none = BTreeMap::new();
        let picker = favorites_query(&favorites, &none);
        let given = inputs(&[], &[], &[]);
        let mut asked = 0;
        let sections = build_sections_from_parts("", &picker, &given.availability, &given.logged_in_oauth, || {
            asked += 1;
            Some(&mgr)
        });
        assert_eq!(asked, 1);
        let keys: Vec<&str> = sections.iter().map(|s| s.provider_key.as_str()).collect();
        assert!(keys.ends_with(&["local", "plug:prov"]), "{keys:?}");
        let without = build_sections_from_parts("", &picker, &given.availability, &given.logged_in_oauth, || None::<&crate::extensions::manager::ExtensionManager>);
        assert_eq!(without.len() + 1, sections.len(), "no manager: no plugin rows");
    }

    /// Every kind of row gives an id the session's model setting takes as
    /// is: the picker applies `ModelEntry::id` through
    /// `SessionSetting::Model`, which ends in `Runtime::try_set_model`.
    #[test]
    fn each_row_id_is_what_the_model_setting_takes_unchanged() {
        let (k0, _) = known(0);
        let (reg_key, _, reg_model) = registry_first();
        let reg_id = format!("{reg_key}/{reg_model}");
        let favorites = [k0, reg_id.as_str(), "local/qwen3:8b", "plug:prov:m1"];
        let given = inputs(&favorites, &[reg_key], &["anthropic"]);
        let mut rows = builtin_favorites(&given, &BTreeMap::new()).models;
        rows.extend(plugin_favorites(&given.favorites, &manager()));
        let kinds: Vec<&str> = rows.iter().map(|m| m.provider_key.as_str()).collect();
        assert_eq!(kinds, ["anthropic", reg_key, "local", "plug:prov"], "one row of each kind");
        for row in &rows {
            let mut runtime = crate::runtime::Runtime::new_headless();
            runtime.try_set_model(row.id.clone()).unwrap_or_else(|e| panic!("{}: {e}", row.id));
            assert_eq!(runtime.model(), row.id, "stored unchanged");
            if let Some((plugin, provider, model)) = crate::extensions::providers::ProviderRegistry::parse_model_id(&row.id) {
                assert_eq!((plugin, provider, model), ("plug", "prov", "m1"));
            } else {
                let route = crate::runtime::openai::resolve_route(&row.id).unwrap_or_else(|| panic!("{} routes", row.id));
                assert_eq!(route.provider, row.provider_key, "{}", row.id);
                assert_eq!(format!("{}/{}", row.provider_key, route.model), row.id);
            }
        }
    }

    #[test]
    fn a_favorite_row_serializes_with_a_label_on_every_row() {
        let row = FavoriteModel {
            id: "plug:prov:m2".into(),
            label: None,
            provider_key: "plug:prov".into(),
            provider_name: "Plug Provider".into(),
            badges: vec![],
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({"id": "plug:prov:m2", "label": null, "provider_key": "plug:prov", "provider_name": "Plug Provider"})
        );
        let with = FavoriteModel { label: Some("M".into()), badges: vec!["tool-use".into()], ..row };
        let back: FavoriteModel = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
        assert_eq!(back, with);
    }

    /// The rows come from a snapshot fixture, not the process-wide cache,
    /// so no other test's cache rows move.
    #[test]
    fn held_rows_take_complete_live_snapshots_only() {
        use crate::runtime::openai::catalog::{CatalogModel, CatalogSource};
        let row = |provider: &str, id: &str, label: Option<&str>, source: CatalogSource| {
            let mut m = CatalogModel::new(provider, provider, id).unwrap();
            m.label = label.map(str::to_string);
            m.source = source;
            m
        };
        let snapshot = |key: &str| match key {
            "kimi-code" => Some(vec![row(key, "k2", Some("K Two"), CatalogSource::Live), row(key, "k1", None, CatalogSource::Live)]),
            "github-copilot" => Some(vec![row(key, "c1", None, CatalogSource::Live), row(key, "c2", None, CatalogSource::StaticFallback)]),
            "openai-codex" | "local" => Some(vec![row(key, "o1", None, CatalogSource::Live)]),
            "muse" => Some(Vec::new()),
            _ => None,
        };
        let held = catalog_rows_from(snapshot);
        assert_eq!(
            held.get("kimi-code"),
            Some(&vec![("k2".to_string(), Some("K Two".to_string())), ("k1".to_string(), None)]),
            "catalog order; the catalog's own labels, a missing one kept missing"
        );
        assert_eq!(held.keys().collect::<Vec<_>>(), ["kimi-code"], "a partly static list, codex, local and an empty list give none");
        // As picker rows, a missing label shows the id, as in the TUI.
        assert_eq!(
            picker_overrides(&held).get("kimi-code"),
            Some(&vec![("k2".to_string(), "K Two".to_string()), ("k1".to_string(), "k1".to_string())])
        );
        // The real cache: a provider never listed gives nothing.
        assert!(!held_catalog_rows().contains_key("zz-never-listed"));
    }

    /// A catalog refresh that fails after an incremental page never reaches
    /// the Favorites: the rows stay exactly the last complete list until
    /// the next complete list replaces them whole. Codex stays static. A
    /// cache of its own, so no other test's rows move.
    #[test]
    fn a_failed_refresh_never_reaches_the_favorites() {
        use crate::runtime::openai::catalog::capability_cache::Cache;
        use crate::runtime::openai::catalog::{CatalogModel, CatalogSource};
        let row = |provider: &str, id: &str, label: Option<&str>| {
            let mut m = CatalogModel::new(provider, provider, id).unwrap();
            m.label = label.map(str::to_string);
            m.source = CatalogSource::Live;
            m
        };
        let codex_id = codex_first().runtime_id();
        let favorites = ["kimi-code/k1", "kimi-code/k2", "kimi-code/k9", codex_id.as_str(), "openai-codex/o-live"];
        let given = inputs(&favorites, &[], &["kimi-code", "openai-codex"]);
        let kimi = |cache: &Cache| {
            let view = builtin_favorites(&given, &catalog_rows_from(|key| cache.provider_snapshot(key)));
            let codex: Vec<String> = view.models.iter().filter(|m| m.provider_key == "openai-codex").map(|m| m.id.clone()).collect();
            assert_eq!(codex, std::slice::from_ref(&codex_id), "codex keeps its static rows");
            let rows: Vec<(String, Option<String>)> =
                view.models.iter().filter(|m| m.provider_key == "kimi-code").map(|m| (m.id.clone(), m.label.clone())).collect();
            (rows, view.live_providers)
        };

        let mut cache = Cache::default();
        cache.replace_provider("kimi-code", &[row("kimi-code", "k2", Some("K Two")), row("kimi-code", "k1", Some("K One"))]);
        cache.replace_provider("openai-codex", &[row("openai-codex", "o-live", Some("O Live"))]);
        let a = vec![("kimi-code/k2".to_string(), Some("K Two".to_string())), ("kimi-code/k1".to_string(), Some("K One".to_string()))];
        assert_eq!(kimi(&cache), (a.clone(), vec!["kimi-code".to_string()]));

        // Refresh B: one incremental page, then it fails (no replacement).
        cache.populate(&[row("kimi-code", "k1", Some("Page B")), row("kimi-code", "k9", Some("K Nine"))]);
        assert_eq!(kimi(&cache), (a, vec!["kimi-code".to_string()]), "still exactly A");

        // B completes: the Favorites follow B, whole.
        cache.replace_provider("kimi-code", &[row("kimi-code", "k9", Some("K Nine")), row("kimi-code", "k1", Some("Page B"))]);
        let b = vec![("kimi-code/k9".to_string(), Some("K Nine".to_string())), ("kimi-code/k1".to_string(), Some("Page B".to_string()))];
        assert_eq!(kimi(&cache), (b, vec!["kimi-code".to_string()]));
    }
}
