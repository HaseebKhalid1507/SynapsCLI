//! Settings modal — full-screen overlay opened via /settings.
//! Persists changes to ~/.synaps-cli/config and mutates Runtime where possible.

pub(crate) mod schema;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};

use schema::{Category, SettingDef};

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

// Silence unused-field warnings until later tasks wire these up.
#[allow(dead_code)]
fn _keep_category_used(_: Category) {}
