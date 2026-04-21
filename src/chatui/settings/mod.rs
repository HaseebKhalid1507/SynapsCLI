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
}

impl RuntimeSnapshot {
    pub fn from_runtime(
        runtime: &synaps_cli::Runtime,
        registry: &synaps_cli::skills::registry::CommandRegistry,
    ) -> Self {
        let config = synaps_cli::config::load_config();
        let plugins: Vec<PluginRow> = registry.plugins().into_iter()
            .map(|p| PluginRow { name: p.name, skill_count: p.skill_count })
            .collect();
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
    CustomModel { buffer: String },
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
        if cat == schema::Category::Plugins { return Vec::new(); }
        schema::ALL_SETTINGS.iter().filter(|s| s.category == cat).collect()
    }

    pub fn current_setting(&self) -> Option<&'static SettingDef> {
        self.current_settings().get(self.setting_idx).copied()
    }
}

