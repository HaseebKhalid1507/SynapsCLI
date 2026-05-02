//! Settings modal — full-screen overlay opened via /settings.
//! Persists changes to ~/.synaps-cli/config and mutates Runtime where possible.

pub(crate) mod defs;
pub(crate) mod schema;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};

const BUILTIN_THEMES: &[&str] = &[
    "default", "neon-rain", "amber", "phosphor", "solarized-dark", "blood",
    "ocean", "rose-pine", "nord", "dracula", "monokai",
    "gruvbox", "catppuccin", "tokyo-night", "sunset", "ice",
    "forest", "lavender",
];

pub(crate) fn theme_options() -> Vec<String> {
    let mut opts: Vec<String> = BUILTIN_THEMES.iter().map(|s| s.to_string()).collect();
    if let Some(home) = std::env::var_os("HOME") {
        let themes_dir = std::path::PathBuf::from(home).join(".synaps-cli/themes");
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for e in entries.flatten() {
                if let Some(name) = e.path().file_stem().and_then(|s| s.to_str()) {
                    let s = name.to_string();
                    if !opts.contains(&s) { opts.push(s); }
                }
            }
        }
    }
    opts
}

/// List `*.bin` files in `~/.synaps-cli/models/whisper/`, sorted by name.
/// Used by the WhisperModelPicker editor.
pub(crate) fn whisper_model_options() -> Vec<String> {
    let home = match std::env::var_os("HOME") {
        Some(h) => h,
        None => return Vec::new(),
    };
    let dir = std::path::PathBuf::from(home).join(".synaps-cli/models/whisper");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut opts: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("bin") {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    opts.sort();
    opts
}

/// One row in the whisper model browser — the catalog entry zipped with
/// installed status against the local models directory.
pub(crate) struct ModelBrowserRow {
    /// Catalog id, e.g. "base.en".
    pub id: String,
    /// Filename, e.g. "ggml-base.en.bin".
    pub filename: String,
    /// Approximate on-disk size in megabytes.
    pub size_mb: u32,
    /// `false` for English-only `*.en` variants.
    pub multilingual: bool,
    /// True iff `models_dir.join(filename)` exists.
    pub installed: bool,
    /// Absolute path the row would be persisted to (whether installed or not).
    pub absolute_path: std::path::PathBuf,
}

/// Build the browser rows by zipping the whisper catalog with installed
/// status against `~/.synaps-cli/models/whisper/`.
pub(crate) fn model_browser_rows() -> Vec<ModelBrowserRow> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home).join(".synaps-cli/models/whisper");
    model_browser_rows_in(&dir)
}

/// Like [`model_browser_rows`] but takes an explicit models directory —
/// used by tests so they don't have to mutate `$HOME`.
pub(crate) fn model_browser_rows_in(dir: &std::path::Path) -> Vec<ModelBrowserRow> {
    synaps_cli::voice::models::CATALOG
        .iter()
        .map(|e| {
            let path = dir.join(e.filename);
            ModelBrowserRow {
                id: e.id.to_string(),
                filename: e.filename.to_string(),
                size_mb: e.size_mb,
                multilingual: e.multilingual,
                installed: path.exists(),
                absolute_path: path,
            }
        })
        .collect()
}

use schema::SettingDef;

pub(crate) struct PluginRow {
    pub name: String,
    pub skill_count: usize,
}

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
    pub plugins: Vec<PluginRow>,
    pub disabled_plugins: Vec<String>,
    pub provider_keys: std::collections::BTreeMap<String, String>,
    /// Cached ping results for models. Key format: "provider/model" (or bare
    /// model id for Anthropic). Empty until `/ping` has been run.
    pub model_health: std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    /// Cached `compiled_backend` from the live voice sidecar (if spawned).
    /// Populated by callers from `app.voice.as_ref().and_then(...)`.
    pub voice_compiled_backend: Option<String>,
    /// Plugin-declared settings categories snapshotted from the registry
    /// at modal-open time. Each entry contributes a category row in the
    /// left pane and a list of fields in the right pane. Path B Phase 4.
    pub plugin_categories: Vec<synaps_cli::skills::registry::PluginSettingsCategory>,
}

impl RuntimeSnapshot {
    #[allow(dead_code)]
    pub fn from_runtime(
        runtime: &synaps_cli::Runtime,
        registry: &synaps_cli::skills::registry::CommandRegistry,
    ) -> Self {
        Self::from_runtime_with_health(runtime, registry, Default::default())
    }

    pub fn from_runtime_with_health(
        runtime: &synaps_cli::Runtime,
        registry: &synaps_cli::skills::registry::CommandRegistry,
        model_health: std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
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
                PluginRow { name: p.name, skill_count }
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
            plugins,
            disabled_plugins: config.disabled_plugins.clone(),
            provider_keys: config.provider_keys.clone(),
            model_health,
            voice_compiled_backend: None,
            plugin_categories: registry.plugin_settings_categories(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Left,
    Right,
}

pub(super) enum ActiveEditor {
    Text { buffer: String, setting_key: &'static str, numeric: bool, error: Option<String> },
    Picker { setting_key: &'static str, options: Vec<String>, cursor: usize },
    CustomModel { buffer: String, setting_key: &'static str },
    ApiKey { provider_id: String, buffer: String },
    /// Whisper.cpp catalog browser. Cursor selects a row from `rows`.
    /// Enter on installed → emits Apply for `voice_stt_model_path`.
    /// Enter on uninstalled → emits StartModelDownload.
    ModelBrowser { cursor: usize, rows: Vec<ModelBrowserRow> },
    /// Text editor for a plugin-declared `text` field. Path B Phase 4.
    /// Commits via `InputOutcome::PluginApply` to the plugin config namespace.
    PluginText {
        plugin_id: String,
        key: String,
        buffer: String,
        numeric: bool,
        error: Option<String>,
    },
    /// Picker editor for a plugin-declared `picker` field. Options are
    /// taken from the manifest declaration; selection commits via
    /// `InputOutcome::PluginApply`.
    #[allow(dead_code)] // wired by render path in a follow-up
    PluginPicker {
        plugin_id: String,
        key: String,
        options: Vec<String>,
        cursor: usize,
    },
}

pub(super) struct SettingsState {
    pub category_idx: usize,
    pub setting_idx: usize,
    pub focus: Focus,
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
            edit_mode: None,
            row_error: None,
            original_theme_name: None,
        }
    }

    /// Settings in the currently selected category.
    pub fn current_settings(&self) -> Vec<&'static SettingDef> {
        let cat = schema::CATEGORIES[self.category_idx];
        if cat == schema::Category::Plugins || cat == schema::Category::Providers {
            return Vec::new();
        }
        schema::ALL_SETTINGS.iter().filter(|s| s.category == cat).collect()
    }

    pub fn current_setting(&self) -> Option<&'static SettingDef> {
        self.current_settings().get(self.setting_idx).copied()
    }

    /// True iff `category_idx` points past the built-in categories at a
    /// plugin-declared category from `snap.plugin_categories`.
    pub fn is_plugin_category(&self, snap: &RuntimeSnapshot) -> bool {
        let n_builtin = schema::CATEGORIES.len();
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
        snap.plugin_categories.get(self.category_idx - schema::CATEGORIES.len())
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
mod model_browser_tests {
    use super::*;

    #[test]
    fn model_browser_rows_includes_all_catalog_entries() {
        let dir = tempfile::tempdir().unwrap();
        let rows = model_browser_rows_in(dir.path());
        assert_eq!(rows.len(), synaps_cli::voice::models::CATALOG.len());
        for (row, entry) in rows.iter().zip(synaps_cli::voice::models::CATALOG.iter()) {
            assert_eq!(row.id, entry.id);
            assert_eq!(row.filename, entry.filename);
            assert_eq!(row.size_mb, entry.size_mb);
            assert_eq!(row.multilingual, entry.multilingual);
        }
    }

    #[test]
    fn model_browser_rows_marks_installed_correctly() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create the file for the "base" entry only.
        let base = synaps_cli::voice::models::find_by_id("base").unwrap();
        std::fs::write(dir.path().join(base.filename), b"fake").unwrap();

        let rows = model_browser_rows_in(dir.path());
        for row in &rows {
            if row.id == "base" {
                assert!(row.installed, "base should be marked installed");
            } else {
                assert!(!row.installed, "{} should NOT be installed", row.id);
            }
            assert_eq!(row.absolute_path, dir.path().join(&row.filename));
        }
    }
}
