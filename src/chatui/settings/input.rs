use crossterm::event::{KeyCode, KeyModifiers, KeyEvent};
use super::{SettingsState, Focus, RuntimeSnapshot};
use super::schema::{CATEGORIES, EditorKind};

pub(crate) enum InputOutcome {
    None,
    Close,
    Apply { key: &'static str, value: String },
}

pub(crate) fn handle_event(
    state: &mut SettingsState,
    key: KeyEvent,
    snap: &RuntimeSnapshot,
) -> InputOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => InputOutcome::Close,
        (KeyCode::Tab, _) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            state.focus = match state.focus {
                Focus::Left => Focus::Right,
                Focus::Right => Focus::Left,
            };
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Up, _) => {
            match state.focus {
                Focus::Left => {
                    if state.category_idx > 0 {
                        state.category_idx -= 1;
                        state.setting_idx = 0;
                    }
                }
                Focus::Right => {
                    if state.setting_idx > 0 { state.setting_idx -= 1; }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Down, _) => {
            match state.focus {
                Focus::Left => {
                    if state.category_idx + 1 < CATEGORIES.len() {
                        state.category_idx += 1;
                        state.setting_idx = 0;
                    }
                }
                Focus::Right => {
                    let n = state.current_settings().len();
                    if state.setting_idx + 1 < n { state.setting_idx += 1; }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Left, _) | (KeyCode::Right, _) if state.focus == Focus::Right => {
            if let Some(def) = state.current_setting() {
                if let EditorKind::Cycler(options) = def.editor {
                    let current = cycler_current_value(def.key, snap);
                    let idx = options.iter().position(|o| *o == current).unwrap_or(0);
                    let new_idx = match key.code {
                        KeyCode::Left => if idx > 0 { idx - 1 } else { idx },
                        KeyCode::Right => if idx + 1 < options.len() { idx + 1 } else { idx },
                        _ => idx,
                    };
                    if new_idx != idx {
                        state.row_error = None;
                        return InputOutcome::Apply {
                            key: def.key,
                            value: options[new_idx].to_string(),
                        };
                    }
                }
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn cycler_current_value(key: &str, snap: &RuntimeSnapshot) -> String {
    match key {
        "thinking" => snap.thinking.clone(),
        _ => String::new(),
    }
}
