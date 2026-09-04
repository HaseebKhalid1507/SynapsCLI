//! Settings modal — full-screen overlay opened via /settings.
//! Persists changes to ~/.synaps-cli/config and mutates Runtime where possible.

pub(crate) mod defs;
pub(crate) mod draw;
pub(crate) mod input;
pub(crate) mod plugin_editor;
pub(crate) mod schema;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};

use crate::tui::focus::FocusRing;

const BUILTIN_THEMES: &[&str] = &[
    "default",
    "night-city",
    "neon-rain",
    "amber",
    "phosphor",
    "solarized-dark",
    "blood",
    "ocean",
    "rose-pine",
    "nord",
    "dracula",
    "monokai",
    "myx",
    "gruvbox",
    "catppuccin",
    "tokyo-night",
    "sunset",
    "ice",
    "forest",
    "lavender",
];

/// Compute the thinking level options for `model_runtime_id`.
///
/// Delegates to the shared engine derivation
/// (`catalog::validation::thinking_options_for_model`): exact capability
/// cache (live catalog) first, then exact static descriptor tables, for
/// `openai-codex/<id>`, `anthropic/<id>` (and bare `claude-*`), and
/// `xai-auth/<id>`. Providers without authoritative metadata keep the
/// conservative set and NEVER gain max/ultra. No substring inference.
pub(crate) fn thinking_options_for_model(model: &str) -> Vec<String> {
    agent_engine::runtime::openai::catalog::validation::thinking_options_for_model(model)
}

/// Human-readable reasoning type for `model_runtime_id` — shared engine
/// derivation (`catalog::validation::reasoning_type_for_model`), same exact
/// capability sources as `thinking_options_for_model`.
pub(crate) fn reasoning_type_for_model(model: &str) -> &'static str {
    agent_engine::runtime::openai::catalog::validation::reasoning_type_for_model(model)
}

pub(crate) fn theme_options() -> Vec<String> {
    let mut opts: Vec<String> = BUILTIN_THEMES.iter().map(|s| s.to_string()).collect();
    if let Some(home) = std::env::var_os("HOME") {
        let themes_dir = std::path::PathBuf::from(home).join(".synaps-cli/themes");
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for e in entries.flatten() {
                if let Some(name) = e.path().file_stem().and_then(|s| s.to_str()) {
                    let s = name.to_string();
                    if !opts.contains(&s) {
                        opts.push(s);
                    }
                }
            }
        }
    }
    opts
}

use schema::SettingDef;

#[derive(Clone)]
pub(crate) struct PluginRow {
    pub name: String,
    pub skill_count: usize,
}

#[derive(Clone)]
/// Snapshot of live runtime + persisted config values, used to display current
/// values in the modal and seed text editors.
pub(crate) struct RuntimeSnapshot {
    pub model: String,
    pub thinking: String,
    pub context_window: String,
    pub compaction_model: String,
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub subagent_timeout: u64,
    pub api_retries: u32,
    pub theme_name: String,
    pub background_opaque: bool,
    pub plugins: Vec<PluginRow>,
    pub disabled_plugins: Vec<String>,
    /// Non-secret static-key status per provider (masked preview / from-env /
    /// not set), sourced from the credential broker. Raw key values never
    /// enter the TUI.
    pub provider_key_status: std::collections::BTreeMap<String, synaps_cli::auth::StaticKeyStatus>,
    /// Explicitly configured local endpoint URL (non-secret), if any.
    pub local_url_explicit: Option<String>,
    /// Cached ping results for models. Key format: "provider/model" (or bare
    /// model id for Anthropic). Empty until `/ping` has been run.
    pub model_health:
        std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    /// Plugin-declared settings categories snapshotted from the registry
    /// at modal-open time. Each entry contributes a category row in the
    /// left pane and a list of fields in the right pane. Path B Phase 4.
    pub plugin_categories: Vec<synaps_cli::skills::registry::PluginSettingsCategory>,
    /// Phase 8 8A.4: lifecycle claims used by `schema::visible_categories`
    /// to hide the legacy global `Sidecar` page when a plugin has staked a
    /// claim with a `settings_category`.
    pub lifecycle_claims: Vec<synaps_cli::skills::registry::LifecycleClaim>,
    /// Dynamic thinking level options for the active model.
    /// For Codex models: the exact ordered supported levels from the catalog.
    /// For all other models: the conservative legacy set.
    pub thinking_options: Vec<String>,
    /// App-level live catalog overrides (provider → (bare id, label) rows),
    /// fetched by the same auto-refresh path the /models modal uses. Feeds
    /// the shared model picker so /settings shows live catalogs too.
    pub catalog_overrides:
        std::collections::BTreeMap<String, crate::tui::models::ProviderCatalogOverride>,
    /// Derived reasoning type for the active model (display-only row).
    pub reasoning_type: String,
}

impl RuntimeSnapshot {
    #[allow(dead_code)]
    pub fn from_runtime(
        runtime: &impl agent_engine::session::RuntimeRead,
        registry: &synaps_cli::skills::registry::CommandRegistry,
    ) -> Self {
        Self::from_runtime_with_health(runtime, registry, Default::default())
    }

    pub fn from_runtime_with_health(
        runtime: &impl agent_engine::session::RuntimeRead,
        registry: &synaps_cli::skills::registry::CommandRegistry,
        model_health: std::collections::HashMap<
            String,
            (synaps_cli::runtime::openai::ping::PingStatus, u64),
        >,
    ) -> Self {
        let config = synaps_cli::config::load_config();
        // Build the plugin list from *all* discovered plugins on disk (not
        // just the registry, which excludes disabled plugins).  This ensures
        // disabled plugins remain visible in the settings list so the user
        // can re-enable them.
        let registry_map: std::collections::HashMap<String, usize> = registry
            .plugins()
            .into_iter()
            .map(|p| (p.name, p.skill_count))
            .collect();
        let (all_plugins, _all_skills) =
            synaps_cli::skills::loader::load_all(&synaps_cli::skills::loader::default_roots());
        let mut plugins: Vec<PluginRow> = all_plugins
            .into_iter()
            .map(|p| {
                let skill_count = registry_map.get(&p.name).copied().unwrap_or(0);
                PluginRow {
                    name: p.name,
                    skill_count,
                }
            })
            .collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            model: runtime.model().to_string(),
            thinking: runtime.thinking_level().to_string(),
            compaction_model: runtime.compaction_model().to_string(),
            context_window: {
                match config.context_window {
                    Some(200_000) => "200k".to_string(),
                    Some(1_000_000) => "1m".to_string(),
                    Some(v) => v.to_string(),
                    None => "auto".to_string(),
                }
            },
            max_tool_output: runtime.max_tool_output(),
            bash_timeout: runtime.bash_timeout(),
            bash_max_timeout: runtime.bash_max_timeout(),
            subagent_timeout: runtime.subagent_timeout(),
            api_retries: runtime.api_retries(),
            theme_name: config.theme.unwrap_or_else(|| "(default)".to_string()),
            background_opaque: config.tui_background_opaque,
            plugins,
            disabled_plugins: config.disabled_plugins.clone(),
            provider_key_status: synaps_cli::auth::broker::static_key_status_map(),
            local_url_explicit: synaps_cli::auth::broker::local_endpoint_config(),
            model_health,
            plugin_categories: registry.plugin_settings_categories(),
            lifecycle_claims: registry.lifecycle_claims(),
            thinking_options: thinking_options_for_model(runtime.model()),
            catalog_overrides: std::collections::BTreeMap::new(),
            reasoning_type: reasoning_type_for_model(runtime.model()).to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Left,
    Right,
}

#[derive(Clone)]
pub(super) enum ActiveEditor {
    Text {
        buffer: String,
        setting_key: &'static str,
        numeric: bool,
        error: Option<String>,
    },
    Picker {
        setting_key: &'static str,
        options: Vec<String>,
        /// Parallel exact-value column: `values[i]` is applied verbatim when
        /// row `i` is selected. Empty string = non-selectable (header) row.
        values: Vec<String>,
        cursor: usize,
    },
    CustomModel {
        buffer: String,
        setting_key: &'static str,
    },
    ApiKey {
        provider_id: String,
        buffer: String,
    },
    /// Text editor for a plugin-declared `text` field. Path B Phase 4.
    /// Commits via `InputOutcome::PluginApply` to the plugin config namespace.
    PluginText {
        plugin_id: String,
        key: String,
        buffer: String,
        numeric: bool,
        error: Option<String>,
    },
    /// Plugin-owned custom editor render returned by `settings.editor.open`.
    PluginCustom {
        plugin_id: String,
        category: String,
        field: String,
        render: crate::tui::settings::plugin_editor::PluginEditorSession,
    },
}

#[derive(Clone)]
pub(super) struct SettingsState {
    pub category_idx: usize,
    pub setting_idx: usize,
    /// Left/Right focus as read by draw code (`settings/draw.rs` reads it
    /// directly). P7.7: now a synced projection of `focus_ring` — the
    /// authoritative traversal store below.
    pub focus: Focus,
    /// Authoritative Left/Right traversal store: a two-slot [`FocusRing`] from
    /// the FocusManager (slot 0 = Left, slot 1 = Right). Tab / focus moves go
    /// through this ring (P7.7), replacing the old direct `focus` assignments;
    /// `focus` above is re-derived from it after every move. Focus survives
    /// occlusion (e.g. marketplace/PluginEditor pushed on top) because it lives
    /// in the retained `SettingsState` (§4).
    focus_ring: FocusRing,
    pub edit_mode: Option<ActiveEditor>,
    /// Transient error/note shown under a row.
    pub row_error: Option<(String, String)>,
    /// When a theme picker is open, the theme name before previewing began.
    pub original_theme_name: Option<String>,
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            category_idx: 0,
            setting_idx: 0,
            focus: Focus::Left,
            focus_ring: FocusRing::of_len(2),
            edit_mode: None,
            row_error: None,
            original_theme_name: None,
        }
    }

    /// Current Left/Right focus, read from the two-slot [`FocusRing`] backing
    /// store (slot 0 = Left, slot 1 = Right). Equals the synced `focus` field.
    #[cfg(test)]
    pub fn focus(&self) -> Focus {
        Self::focus_from_ring(&self.focus_ring)
    }

    fn focus_from_ring(ring: &FocusRing) -> Focus {
        match ring.current().map(|slot| slot.id()) {
            Some(1) => Focus::Right,
            _ => Focus::Left,
        }
    }

    /// Re-derive the public `focus` projection from the authoritative ring.
    fn sync_focus_field(&mut self) {
        self.focus = Self::focus_from_ring(&self.focus_ring);
    }

    /// Tab: toggle Left <-> Right. On the two-slot ring this is `next()`
    /// (wraps), preserving today's exact toggle semantics.
    pub fn toggle_focus(&mut self) {
        self.focus_ring.next();
        self.sync_focus_field();
    }

    /// Set focus to a specific side. On the two-slot ring, one `next()` flips
    /// to the other slot, so we step only when currently on the wrong side.
    #[cfg(test)]
    pub fn set_focus(&mut self, target: Focus) {
        if self.focus() != target {
            self.focus_ring.next();
        }
        self.sync_focus_field();
    }

    /// Settings in the currently selected category.
    pub fn current_settings(&self, snap: &RuntimeSnapshot) -> Vec<&'static SettingDef> {
        // `category_idx` indexes the *visible* category list (built-ins with any
        // hidden `Sidecar` removed) followed by plugin categories — NOT the
        // static `CATEGORIES` array. Map through the visible list: a plugin
        // position, or a stale index past the list (plugin hot-reloaded while
        // the modal is open), yields no built-in settings instead of
        // mis-mapping onto a static-array neighbour (e.g. returning Sidecar's
        // keybind for a plugin category) or panicking on an out-of-bounds index.
        let visible = schema::visible_categories(&snap.lifecycle_claims);
        let Some(&cat) = visible.get(self.category_idx) else {
            return Vec::new();
        };
        if cat == schema::Category::Plugins || cat == schema::Category::Providers {
            return Vec::new();
        }
        schema::ALL_SETTINGS
            .iter()
            .filter(|s| s.category == cat)
            .collect()
    }

    pub fn current_setting(&self, snap: &RuntimeSnapshot) -> Option<&'static SettingDef> {
        self.current_settings(snap).get(self.setting_idx).copied()
    }

    /// True iff `category_idx` points past the built-in categories at a
    /// plugin-declared category from `snap.plugin_categories`.
    pub fn is_plugin_category(&self, snap: &RuntimeSnapshot) -> bool {
        let n_builtin = schema::visible_categories(&snap.lifecycle_claims).len();
        self.category_idx >= n_builtin
            && self.category_idx - n_builtin < snap.plugin_categories.len()
    }

    /// Plugin-declared category at the current `category_idx`, or None
    /// if the cursor is on a built-in category.
    pub fn current_plugin_category<'a>(
        &self,
        snap: &'a RuntimeSnapshot,
    ) -> Option<&'a synaps_cli::skills::registry::PluginSettingsCategory> {
        if !self.is_plugin_category(snap) {
            return None;
        }
        snap.plugin_categories
            .get(self.category_idx - schema::visible_categories(&snap.lifecycle_claims).len())
    }

    /// Plugin field at `setting_idx` within the current plugin category.
    pub fn current_plugin_field<'a>(
        &self,
        snap: &'a RuntimeSnapshot,
    ) -> Option<&'a synaps_cli::skills::registry::PluginSettingsField> {
        self.current_plugin_category(snap)
            .and_then(|c| c.fields.get(self.setting_idx))
    }
}

#[cfg(test)]
mod wireup_tests {
    use super::*;
    use synaps_cli::skills::registry::LifecycleClaim;

    fn snap_with_claims(claims: Vec<LifecycleClaim>) -> RuntimeSnapshot {
        RuntimeSnapshot {
            model: "m".into(),
            thinking: "medium".into(),
            context_window: "auto".into(),
            compaction_model: "m".into(),
            max_tool_output: 0,
            bash_timeout: 0,
            bash_max_timeout: 0,
            subagent_timeout: 0,
            api_retries: 0,
            theme_name: "t".into(),
            background_opaque: true,
            plugins: Vec::new(),
            disabled_plugins: Vec::new(),
            provider_key_status: std::collections::BTreeMap::new(),
            local_url_explicit: None,
            model_health: std::collections::HashMap::new(),
            plugin_categories: Vec::new(),
            lifecycle_claims: claims,
            thinking_options: vec![
                "low".into(),
                "medium".into(),
                "high".into(),
                "xhigh".into(),
                "adaptive".into(),
            ],
            catalog_overrides: std::collections::BTreeMap::new(),
            reasoning_type: "budget (legacy)".into(),
        }
    }

    fn claim(settings_category: Option<&str>) -> LifecycleClaim {
        LifecycleClaim {
            plugin: "p".into(),
            command: "capture".into(),
            settings_category: settings_category.map(|s| s.into()),
            display_name: "Sample".into(),
            importance: 0,
        }
    }

    #[test]
    fn visible_categories_excludes_sidecar_when_claim_present() {
        let snap = snap_with_claims(vec![claim(Some("capture"))]);
        let v = schema::visible_categories(&snap.lifecycle_claims);
        assert!(!v.contains(&schema::Category::Sidecar));
    }

    #[test]
    fn visible_categories_includes_sidecar_when_no_claim_settings_category() {
        let snap_empty = snap_with_claims(Vec::new());
        assert!(schema::visible_categories(&snap_empty.lifecycle_claims)
            .contains(&schema::Category::Sidecar));

        let snap_no_cat = snap_with_claims(vec![claim(None)]);
        assert!(schema::visible_categories(&snap_no_cat.lifecycle_claims)
            .contains(&schema::Category::Sidecar));
    }

    #[test]
    fn current_settings_maps_plugin_category_to_empty_not_static_neighbour() {
        // Regression (PR#60 review). `category_idx` indexes the VISIBLE category
        // list (built-ins minus a hidden Sidecar) followed by plugin categories —
        // NOT the static `CATEGORIES` array. Two failure modes pinned here:
        //  (1) silverhand: an index past the list must not panic.
        //  (2) shady: with Sidecar hidden, the first plugin category sits at
        //      idx == visible_len (6). The OLD `CATEGORIES[6]` returned Sidecar's
        //      keybind def for that plugin category (silent mis-map that let a
        //      keypress rewire the sidecar toggle). It must resolve to empty.
        let mut state = SettingsState::new();

        // --- No plugin claim: all 7 built-ins visible, Sidecar present. ---
        let snap_plain = snap_with_claims(Vec::new());
        // One past the array must not panic and must be empty (silverhand).
        state.category_idx = schema::CATEGORIES.len();
        assert!(state.current_settings(&snap_plain).is_empty());
        assert!(state.current_setting(&snap_plain).is_none());
        state.category_idx = schema::CATEGORIES.len() + 5;
        assert!(state.current_settings(&snap_plain).is_empty());
        assert!(state.current_setting(&snap_plain).is_none());

        // --- Plugin claim present: Sidecar hidden, visible list len 6. ---
        let snap_claimed = snap_with_claims(vec![claim(Some("capture"))]);
        let visible = schema::visible_categories(&snap_claimed.lifecycle_claims);
        assert_eq!(
            visible.len(),
            schema::CATEGORIES.len() - 1,
            "Sidecar should be hidden when a plugin claims a settings_category"
        );

        // idx == visible_len is the first PLUGIN position — must be empty, NOT
        // Sidecar's settings (shady's silent-mis-map blocker).
        state.category_idx = visible.len();
        assert!(
            state.current_settings(&snap_claimed).is_empty(),
            "first plugin category must return NO built-in settings (not Sidecar's)"
        );
        assert!(state.current_setting(&snap_claimed).is_none());

        // Every built-in position still maps to exactly its own settings —
        // proves the visible-list mapping didn't regress in-range lookups.
        for (idx, &cat) in visible.iter().enumerate() {
            state.category_idx = idx;
            let expected = if cat == schema::Category::Plugins || cat == schema::Category::Providers
            {
                0
            } else {
                schema::ALL_SETTINGS
                    .iter()
                    .filter(|s| s.category == cat)
                    .count()
            };
            assert_eq!(
                state.current_settings(&snap_claimed).len(),
                expected,
                "built-in category at idx {idx} mapped to the wrong settings"
            );
        }
    }
}

#[cfg(test)]
mod thinking_options_tests {
    use super::thinking_options_for_model;

    // ---- Slice B: reasoning type + effort recompute on model change ------

    /// Settings derivations key on the runtime's EXACT active model: after a
    /// model change through the checked dispatch path, both the reasoning
    /// type and the effort option set recompute for the new model. (The
    /// snapshot is rebuilt from the runtime on every key event, so these
    /// derivations ARE the recompute path.)
    #[test]
    fn reasoning_type_and_effort_options_recompute_on_model_change() {
        let mut rt = synaps_cli::Runtime::new_headless();
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("m", "medium", None));

        super::defs::apply_setting_dispatch("model", "xai-auth/grok-4.5", &mut rt, &mut app)
            .unwrap();
        assert_eq!(super::reasoning_type_for_model(rt.model()), "effort");
        assert_eq!(
            thinking_options_for_model(rt.model()),
            vec!["adaptive", "low", "medium", "high"]
        );

        super::defs::apply_setting_dispatch("model", "xai-auth/grok-4.3", &mut rt, &mut app)
            .unwrap();
        assert_eq!(super::reasoning_type_for_model(rt.model()), "intrinsic");
        assert_eq!(thinking_options_for_model(rt.model()), vec!["adaptive"]);

        super::defs::apply_setting_dispatch("model", "openai-codex/gpt-5.6-sol", &mut rt, &mut app)
            .unwrap();
        assert_eq!(
            super::reasoning_type_for_model(rt.model()),
            "effort (named)"
        );
        assert!(thinking_options_for_model(rt.model()).contains(&"ultra".to_string()));
    }

    /// The snapshot built from a runtime must carry the derived reasoning
    /// type for the active model (shown by the display-only settings row).
    #[test]
    fn snapshot_carries_reasoning_type_for_active_model() {
        let mut rt = synaps_cli::Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.3".to_string());
        let registry = synaps_cli::skills::registry::CommandRegistry::new(&[], Vec::new());
        let snap = super::RuntimeSnapshot::from_runtime(&rt, &registry);
        assert_eq!(snap.reasoning_type, "intrinsic");
        assert_eq!(snap.thinking_options, vec!["adaptive"]);
    }

    #[test]
    fn sol_includes_off_adaptive_and_ultra_max() {
        let opts = thinking_options_for_model("openai-codex/gpt-5.6-sol");
        assert_eq!(opts[0], "off", "off must be first");
        assert_eq!(opts[1], "adaptive", "adaptive must be second");
        assert!(
            opts.contains(&"ultra".to_string()),
            "sol must include ultra"
        );
        assert!(opts.contains(&"max".to_string()), "sol must include max");
    }

    #[test]
    fn luna_includes_off_adaptive_and_max_not_ultra() {
        let opts = thinking_options_for_model("openai-codex/gpt-5.6-luna");
        assert_eq!(opts[0], "off");
        assert_eq!(opts[1], "adaptive");
        assert!(opts.contains(&"max".to_string()));
        assert!(
            !opts.contains(&"ultra".to_string()),
            "luna must not include ultra"
        );
    }

    #[test]
    fn gpt55_includes_off_adaptive_and_xhigh_not_max_ultra() {
        for model in [
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.4",
            "openai-codex/gpt-5.4-mini",
            "openai-codex/gpt-5.3-codex-spark",
        ] {
            let opts = thinking_options_for_model(model);
            assert_eq!(opts[0], "off", "{model}: off must be first");
            assert_eq!(opts[1], "adaptive", "{model}: adaptive must be second");
            assert!(
                !opts.contains(&"max".to_string()),
                "{model} must not include max"
            );
            assert!(
                !opts.contains(&"ultra".to_string()),
                "{model} must not include ultra"
            );
            assert!(
                opts.contains(&"xhigh".to_string()),
                "{model} must include xhigh"
            );
        }
    }

    #[test]
    fn non_codex_provider_includes_off_adaptive_not_max_ultra() {
        // Unknown xAI ids now fail closed (see
        // xai_options_derive_from_exact_static_capabilities); providers
        // without exact metadata keep the conservative set.
        for model in ["claude-opus-4-7", "groq/llama-3.3-70b"] {
            let opts = thinking_options_for_model(model);
            assert_eq!(opts[0], "off", "{model}: off must be first");
            assert_eq!(opts[1], "adaptive", "{model}: adaptive must be second");
            assert!(
                !opts.contains(&"max".to_string()),
                "{model} must not gain max"
            );
            assert!(
                !opts.contains(&"ultra".to_string()),
                "{model} must not gain ultra"
            );
        }
    }

    #[test]
    fn unknown_codex_model_falls_back_to_conservative_with_off_adaptive() {
        let opts = thinking_options_for_model("openai-codex/gpt-future-unknown");
        assert_eq!(opts[0], "off");
        assert_eq!(opts[1], "adaptive");
        assert!(!opts.contains(&"max".to_string()));
        assert!(!opts.contains(&"ultra".to_string()));
    }

    // ── Dynamic options for anthropic/<id> and xai-auth/<id> (spec:
    //     anthropic-xai-reasoning-modes) ──

    #[test]
    fn xai_options_derive_from_exact_static_capabilities() {
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.5"),
            vec!["adaptive", "low", "medium", "high"],
            "grok-4.5: documented low/medium/high, cannot be disabled → no off, no xhigh"
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.20-multi-agent-0309"),
            vec!["adaptive", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.3"),
            vec!["adaptive"],
            "no documented effort control → provider default only"
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.20-0309-non-reasoning"),
            vec!["off", "adaptive"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-unknown-id"),
            vec!["adaptive"],
            "unknown exact xAI id must fail closed"
        );
    }

    #[test]
    fn anthropic_options_derive_from_capabilities_and_never_gain_max_ultra() {
        for model in ["anthropic/claude-opus-4-7", "anthropic/claude-sonnet-4-6"] {
            let opts = thinking_options_for_model(model);
            assert_eq!(
                opts,
                vec!["off", "adaptive", "low", "medium", "high", "xhigh"],
                "{model}"
            );
        }
    }

    #[test]
    fn anthropic_live_no_thinking_narrows_options() {
        use agent_engine::runtime::openai::catalog::{
            capability_cache, CatalogProviderKind, CatalogSource, ReasoningSupport,
        };
        let unique_id = "claude-test-tui-nothink";
        let mut live = agent_engine::runtime::openai::catalog::CatalogModel::new(
            "anthropic",
            "Anthropic",
            unique_id,
        )
        .unwrap();
        live.provider_kind = CatalogProviderKind::Anthropic;
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::None;
        capability_cache::insert(live);
        assert_eq!(
            thinking_options_for_model(&format!("anthropic/{unique_id}")),
            vec!["off", "adaptive"],
            "explicit no-thinking evidence must narrow the options"
        );
    }

    /// Gap 2: cache-narrower override — live entry with only Low+Medium must
    /// suppress Ultra in the TUI options even when static sol has Ultra.
    #[test]
    fn cache_narrower_than_static_suppresses_ultra_in_tui_options() {
        use agent_core::reasoning::ReasoningLevel;
        use agent_engine::runtime::openai::catalog::{
            capability_cache, CatalogProviderKind, CatalogSource, ReasoningSupport,
        };

        // Unique slug to avoid polluting other tests via the shared cache.
        let unique_id = "gpt-5.6-sol-tui-cache-test";
        let qualified = format!("openai-codex/{unique_id}");

        // Insert a live cache entry that supports only Low+Medium.
        let mut live = agent_engine::runtime::openai::catalog::CatalogModel::new(
            "openai-codex",
            "OpenAI Codex",
            unique_id,
        )
        .unwrap();
        live.provider_kind = CatalogProviderKind::OpenAiCodex;
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
            multi_agent_version: None,
        };
        capability_cache::insert(live);

        let opts = thinking_options_for_model(&qualified);
        assert_eq!(opts[0], "off");
        assert_eq!(opts[1], "adaptive");
        assert!(
            !opts.contains(&"ultra".to_string()),
            "cache narrower: ultra must be suppressed"
        );
        assert!(
            !opts.contains(&"max".to_string()),
            "cache narrower: max must be suppressed"
        );
        assert!(opts.contains(&"low".to_string()));
        assert!(opts.contains(&"medium".to_string()));
    }
}
