//! Settings modal — full-screen overlay opened via /settings.
//! Persists changes to ~/.synaps-cli/config and mutates Runtime where possible.

pub(crate) mod schema;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};

const BUILTIN_THEMES: &[&str] = &[
    "neon-rain", "amber", "phosphor", "solarized-dark", "blood",
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

use schema::{Category, SettingDef};

/// Snapshot of live runtime + persisted config values, used to display current
/// values in the modal and seed text editors.
pub(crate) struct RuntimeSnapshot {
    pub model: String,
    pub thinking: String,
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub subagent_timeout: u64,
    pub api_retries: u32,
    pub skills: Option<Vec<String>>,
    pub theme_name: String,
}

impl RuntimeSnapshot {
    pub fn from_runtime(runtime: &synaps_cli::Runtime) -> Self {
        let config = synaps_cli::config::load_config();
        Self {
            model: runtime.model().to_string(),
            thinking: runtime.thinking_level().to_string(),
            max_tool_output: runtime.max_tool_output(),
            bash_timeout: runtime.bash_timeout(),
            bash_max_timeout: runtime.bash_max_timeout(),
            subagent_timeout: runtime.subagent_timeout(),
            api_retries: runtime.api_retries(),
            skills: config.skills,
            theme_name: config.theme.unwrap_or_else(|| "(default)".to_string()),
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
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            category_idx: 0,
            setting_idx: 0,
            focus: Focus::Left,
            edit_mode: None,
            row_error: None,
        }
    }

    /// Settings in the currently selected category.
    pub fn current_settings(&self) -> Vec<&'static SettingDef> {
        let cat = schema::CATEGORIES[self.category_idx];
        schema::ALL_SETTINGS.iter().filter(|s| s.category == cat).collect()
    }

    pub fn current_setting(&self) -> Option<&'static SettingDef> {
        self.current_settings().get(self.setting_idx).copied()
    }
}

