//! Runtime glue for plugin-provided custom settings editors.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use synaps_cli::extensions::settings_editor::{
    SettingsEditorCloseParams, SettingsEditorCommitParams, SettingsEditorKeyParams,
    SettingsEditorOpenParams, SettingsEditorRenderParams,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PluginEditorSession {
    pub plugin_id: String,
    pub category: String,
    pub field: String,
    pub render: SettingsEditorRenderParams,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum PluginEditorEffect {
    None,
    ConfigWrite { plugin_id: String, key: String, value: String },
    InvokeCommand { plugin_id: String, command: String, args: Vec<String> },
}

#[allow(dead_code)]
pub(crate) fn open_params(category: &str, field: &str) -> SettingsEditorOpenParams {
    SettingsEditorOpenParams {
        category: category.to_string(),
        field: field.to_string(),
    }
}

#[allow(dead_code)]
pub(crate) fn key_params(key: KeyEvent) -> SettingsEditorKeyParams {
    SettingsEditorKeyParams { key: key_to_wire(key) }
}

pub(crate) fn key_to_wire(key: KeyEvent) -> String {
    let base = match key.code {
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(c) => format!("Char({c})"),
        KeyCode::Null => "Null".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::CapsLock => "CapsLock".to_string(),
        KeyCode::ScrollLock => "ScrollLock".to_string(),
        KeyCode::NumLock => "NumLock".to_string(),
        KeyCode::PrintScreen => "PrintScreen".to_string(),
        KeyCode::Pause => "Pause".to_string(),
        KeyCode::Menu => "Menu".to_string(),
        KeyCode::KeypadBegin => "KeypadBegin".to_string(),
        KeyCode::Media(_) => "Media".to_string(),
        KeyCode::Modifier(_) => "Modifier".to_string(),
    };
    if key.modifiers == KeyModifiers::NONE {
        base
    } else {
        let mut mods = Vec::new();
        if key.modifiers.contains(KeyModifiers::CONTROL) { mods.push("Ctrl"); }
        if key.modifiers.contains(KeyModifiers::ALT) { mods.push("Alt"); }
        if key.modifiers.contains(KeyModifiers::SHIFT) { mods.push("Shift"); }
        format!("{}+{}", mods.join("+"), base)
    }
}

pub(crate) fn render_from_open_result(value: Value) -> Result<SettingsEditorRenderParams, String> {
    let render = value
        .get("render")
        .cloned()
        .unwrap_or(value);
    serde_json::from_value(render).map_err(|e| format!("invalid settings.editor.open render: {e}"))
}

pub(crate) fn render_from_key_result(value: Value) -> Result<Option<SettingsEditorRenderParams>, String> {
    if value.get("committed").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }
    let Some(render) = value.get("render").cloned() else {
        return Ok(None);
    };
    serde_json::from_value(render)
        .map(Some)
        .map_err(|e| format!("invalid settings.editor.key render: {e}"))
}

#[allow(dead_code)]
pub(crate) fn close_note(params: SettingsEditorCloseParams) -> Option<String> {
    params.reason.filter(|s| !s.trim().is_empty())
}

pub(crate) fn effect_from_commit(
    plugin_id: &str,
    field: &str,
    commit: SettingsEditorCommitParams,
) -> PluginEditorEffect {
    effect_from_commit_reply(plugin_id, field, commit.value)
}

pub(crate) fn effect_from_commit_reply(
    plugin_id: &str,
    field: &str,
    reply: Value,
) -> PluginEditorEffect {
    if reply.get("ok").and_then(Value::as_bool) == Some(false) {
        return PluginEditorEffect::None;
    }
    if let Some(intent) = reply.get("intent") {
        if let Some(kind) = intent.get("kind").and_then(Value::as_str) {
            match kind {
                "download" => {
                    let command = intent
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("voice")
                        .to_string();
                    let args: Vec<String> = intent
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .filter(|items: &Vec<String>| !items.is_empty())
                        .or_else(|| {
                            intent
                                .get("model_id")
                                .and_then(Value::as_str)
                                .map(|id| vec!["download".to_string(), id.to_string()])
                        })
                        .unwrap_or_default();
                    return PluginEditorEffect::InvokeCommand {
                        plugin_id: plugin_id.to_string(),
                        command,
                        args,
                    };
                }
                "select" => {
                    let key = intent
                        .get("config_key")
                        .and_then(Value::as_str)
                        .and_then(|k| k.rsplit('.').next())
                        .filter(|k| !k.is_empty())
                        .unwrap_or(field)
                        .to_string();
                    let selected = intent
                        .get("model_path")
                        .or_else(|| intent.get("path"))
                        .or_else(|| intent.get("value"))
                        .or_else(|| intent.get("model_id"))
                        .and_then(Value::as_str);
                    if let Some(selected) = selected {
                        return PluginEditorEffect::ConfigWrite {
                            plugin_id: plugin_id.to_string(),
                            key,
                            value: selected.to_string(),
                        };
                    }
                }
                _ => {}
            }
        }
    }

    if reply.get("kind").and_then(Value::as_str).is_some() {
        return effect_from_commit_reply(
            plugin_id,
            field,
            serde_json::json!({"ok": true, "intent": reply}),
        );
    }

    let value = reply.get("value").cloned().unwrap_or(reply);
    if let Some(s) = value.as_str() {
        if let Some(rest) = s.strip_prefix("download:") {
            return PluginEditorEffect::InvokeCommand {
                plugin_id: plugin_id.to_string(),
                command: "voice".to_string(),
                args: vec!["download".to_string(), rest.to_string()],
            };
        }
        if let Some(rest) = s.strip_prefix("model:") {
            return PluginEditorEffect::ConfigWrite {
                plugin_id: plugin_id.to_string(),
                key: field.to_string(),
                value: rest.to_string(),
            };
        }
        return PluginEditorEffect::ConfigWrite {
            plugin_id: plugin_id.to_string(),
            key: field.to_string(),
            value: s.to_string(),
        };
    }
    PluginEditorEffect::ConfigWrite {
        plugin_id: plugin_id.to_string(),
        key: field.to_string(),
        value: value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    #[test]
    fn key_to_wire_encodes_navigation_and_modifiers() {
        assert_eq!(key_to_wire(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)), "Down");
        assert_eq!(key_to_wire(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)), "Ctrl+Char(j)");
    }

    #[test]
    fn parses_render_from_open_result_wrapper() {
        let render = render_from_open_result(json!({
            "category": "voice",
            "field": "model_path",
            "render": {"rows": [{"label": "tiny", "data": "download:tiny"}], "cursor": 0}
        })).unwrap();
        assert_eq!(render.rows[0].label, "tiny");
        assert_eq!(render.cursor, Some(0));
    }

    #[test]
    fn download_commit_routes_to_voice_download_command() {
        let effect = effect_from_commit(
            "local-voice",
            "model_path",
            SettingsEditorCommitParams { value: json!("download:base.en") },
        );
        assert_eq!(effect, PluginEditorEffect::InvokeCommand {
            plugin_id: "local-voice".into(),
            command: "voice".into(),
            args: vec!["download".into(), "base.en".into()],
        });
    }

    #[test]
    fn select_commit_routes_to_plugin_config_write() {
        let effect = effect_from_commit(
            "local-voice",
            "model_path",
            SettingsEditorCommitParams { value: json!({"kind":"select", "path":"/tmp/ggml.bin"}) },
        );
        assert_eq!(effect, PluginEditorEffect::ConfigWrite {
            plugin_id: "local-voice".into(),
            key: "model_path".into(),
            value: "/tmp/ggml.bin".into(),
        });
    }
}
