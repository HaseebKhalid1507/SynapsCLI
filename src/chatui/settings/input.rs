use crossterm::event::{KeyCode, KeyModifiers, KeyEvent};
use super::{SettingsState, Focus, RuntimeSnapshot, ActiveEditor};
use super::schema::{CATEGORIES, EditorKind};
use super::draw::current_value_for;

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
    // If an editor is open, route keys to it (Esc closes editor only, not modal).
    if state.edit_mode.is_some() {
        if key.code == KeyCode::Esc {
            state.edit_mode = None;
            return InputOutcome::None;
        }
        return handle_editor_key(state, key);
    }
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
        (KeyCode::Enter, _) if state.focus == Focus::Right => {
            if let Some(def) = state.current_setting() {
                match def.editor {
                    EditorKind::Text { numeric } => {
                        state.row_error = None;
                        state.edit_mode = Some(ActiveEditor::Text {
                            buffer: current_value_for(def, snap),
                            setting_key: def.key,
                            numeric,
                            error: None,
                        });
                    }
                    EditorKind::ModelPicker => {
                        state.row_error = None;
                        let mut opts: Vec<String> = synaps_cli::models::KNOWN_MODELS
                            .iter().map(|(id, desc)| format!("{}  — {}", id, desc)).collect();
                        opts.push("Custom…".to_string());
                        let cursor = opts.iter()
                            .position(|o| o.starts_with(&snap.model))
                            .unwrap_or(0);
                        state.edit_mode = Some(ActiveEditor::Picker {
                            setting_key: "model",
                            options: opts,
                            cursor,
                        });
                    }
                    EditorKind::ThemePicker => {
                        state.row_error = None;
                        let opts = super::theme_options();
                        let cursor = opts.iter().position(|o| o == &snap.theme_name).unwrap_or(0);
                        state.edit_mode = Some(ActiveEditor::Picker {
                            setting_key: "theme",
                            options: opts,
                            cursor,
                        });
                    }
                    _ => {}
                }
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn handle_editor_key(state: &mut SettingsState, key: KeyEvent) -> InputOutcome {
    let editor = state.edit_mode.as_mut().expect("caller checks");
    match editor {
        ActiveEditor::Text { buffer, setting_key, numeric, error } => {
            match key.code {
                KeyCode::Enter => {
                    if *numeric && buffer.parse::<u64>().is_err() {
                        *error = Some("must be a number".to_string());
                        return InputOutcome::None;
                    }
                    InputOutcome::Apply { key: *setting_key, value: buffer.clone() }
                }
                KeyCode::Backspace => { buffer.pop(); *error = None; InputOutcome::None }
                KeyCode::Char(c) => {
                    if *numeric && !c.is_ascii_digit() {
                        *error = Some("digits only".to_string());
                        return InputOutcome::None;
                    }
                    buffer.push(c);
                    *error = None;
                    InputOutcome::None
                }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::Picker { setting_key, options, cursor } => {
            match key.code {
                KeyCode::Up => { if *cursor > 0 { *cursor -= 1; } InputOutcome::None }
                KeyCode::Down => { if *cursor + 1 < options.len() { *cursor += 1; } InputOutcome::None }
                KeyCode::Enter => {
                    let selection = options[*cursor].clone();
                    if *setting_key == "model" && selection == "Custom…" {
                        state.edit_mode = Some(ActiveEditor::CustomModel { buffer: String::new() });
                        return InputOutcome::None;
                    }
                    let value = selection.split("  —").next().unwrap_or(&selection).trim().to_string();
                    InputOutcome::Apply { key: *setting_key, value }
                }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::CustomModel { buffer } => {
            match key.code {
                KeyCode::Enter => {
                    if buffer.trim().is_empty() {
                        return InputOutcome::None;
                    }
                    InputOutcome::Apply { key: "model", value: buffer.trim().to_string() }
                }
                KeyCode::Backspace => { buffer.pop(); InputOutcome::None }
                KeyCode::Char(c) => { buffer.push(c); InputOutcome::None }
                _ => InputOutcome::None,
            }
        }
    }
}

fn cycler_current_value(key: &str, snap: &RuntimeSnapshot) -> String {
    match key {
        "thinking" => snap.thinking.clone(),
        _ => String::new(),
    }
}
