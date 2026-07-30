use super::draw::current_value_for;
use super::schema::{visible_categories, EditorKind};
use super::{ActiveEditor, Focus, RuntimeSnapshot, SettingsState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) enum InputOutcome {
    None,
    Close,
    Apply {
        key: &'static str,
        value: String,
    },
    /// Apply a plugin-declared settings field. Written to the plugin's
    /// own namespaced config (`~/.synaps-cli/plugins/<id>/config`).
    PluginApply {
        plugin_id: String,
        key: String,
        value: String,
    },
    /// User requested to open a plugin-declared custom editor.
    /// The async upper layer calls `settings.editor.open` and installs
    /// `ActiveEditor::PluginCustom` with the returned render payload.
    PluginCustomOpen {
        plugin_id: String,
        category: String,
        key: String,
    },
    SetProviderKey {
        provider_id: String,
        value: String,
    },
    TogglePlugin {
        name: String,
        enabled: bool,
    },
    PreviewTheme {
        name: String,
    },
    RevertTheme,
    OpenPluginsMarketplace,
    PingModels,
}

pub(crate) fn handle_event(
    state: &mut SettingsState,
    key: KeyEvent,
    snap: &RuntimeSnapshot,
) -> InputOutcome {
    // If an editor is open, route keys to it (Esc closes editor only, not modal).
    if state.edit_mode.is_some() {
        if key.code == KeyCode::Esc {
            let revert = matches!(
                &state.edit_mode,
                Some(ActiveEditor::Picker {
                    setting_key: "theme",
                    ..
                })
            ) && state.original_theme_name.is_some();
            state.edit_mode = None;
            if revert {
                state.original_theme_name = None;
                return InputOutcome::RevertTheme;
            }
            return InputOutcome::None;
        }
        return handle_editor_key(state, key);
    }
    let visible = visible_categories(&snap.lifecycle_claims);
    if state.focus == Focus::Right && state.category_idx < visible.len() {
        let cat = visible[state.category_idx];
        if cat == super::schema::Category::Providers {
            // 'p' key — ping all models from any row
            if matches!(key.code, KeyCode::Char('p')) && state.edit_mode.is_none() {
                return InputOutcome::PingModels;
            }
            // Row 0 = Local (edits URL), Rows 1+ = registry providers (edit API key)
            if state.setting_idx == 0 {
                // Local provider — edit URL
                match key.code {
                    KeyCode::Enter => {
                        state.row_error = None;
                        let current_url = snap.local_url_explicit.clone().unwrap_or_default();
                        state.edit_mode = Some(ActiveEditor::ApiKey {
                            provider_id: "local.url".to_string(),
                            buffer: current_url,
                        });
                        return InputOutcome::None;
                    }
                    KeyCode::Delete | KeyCode::Char('d') => {
                        if snap.local_url_explicit.is_some() {
                            state.row_error = None;
                            return InputOutcome::SetProviderKey {
                                provider_id: "local.url".to_string(),
                                value: String::new(),
                            };
                        }
                        return InputOutcome::None;
                    }
                    _ => {}
                }
            } else {
                let providers = synaps_cli::runtime::openai::registry::providers();
                if let Some(p) = providers.get(state.setting_idx - 1) {
                    match key.code {
                        KeyCode::Enter => {
                            state.row_error = None;
                            state.edit_mode = Some(ActiveEditor::ApiKey {
                                provider_id: p.key.to_string(),
                                buffer: String::new(),
                            });
                            return InputOutcome::None;
                        }
                        KeyCode::Delete | KeyCode::Char('d') => {
                            let has_key = snap
                                .provider_key_status
                                .get(p.key)
                                .is_some_and(|status| status.is_configured());
                            if has_key {
                                state.row_error = None;
                                return InputOutcome::SetProviderKey {
                                    provider_id: p.key.to_string(),
                                    value: String::new(),
                                };
                            }
                            return InputOutcome::None;
                        }
                        _ => {}
                    }
                }
            }
        }
        if cat == super::schema::Category::Plugins {
            // Row 0 is the "Open Plugin Marketplace…" action row.
            // Rows 1..=n map to snap.plugins[idx - 1].
            let toggle_at = |idx: usize| -> InputOutcome {
                if let Some(row) = snap.plugins.get(idx) {
                    let was_disabled = snap.disabled_plugins.iter().any(|d| d == &row.name);
                    // Toggle polarity: if was disabled, the new state is enabled.
                    InputOutcome::TogglePlugin {
                        name: row.name.clone(),
                        enabled: was_disabled,
                    }
                } else {
                    InputOutcome::None
                }
            };
            match key.code {
                KeyCode::Enter if state.setting_idx == 0 => {
                    return InputOutcome::OpenPluginsMarketplace;
                }
                KeyCode::Char(' ') if state.setting_idx == 0 => {
                    return InputOutcome::None;
                }
                // Only Space toggles — Enter is reserved for drill-down (future).
                KeyCode::Char(' ') => {
                    return toggle_at(state.setting_idx - 1);
                }
                KeyCode::Enter => {
                    // TODO: drill into plugin detail view
                    return InputOutcome::None;
                }
                _ => {}
            }
        }
    }
    // Plugin-declared categories — right-pane handling. Path B Phase 4.
    // Only handled when focus is on the right pane; Up/Down/Tab fall
    // through to the generic match below so navigation is uniform.
    if state.focus == Focus::Right && state.is_plugin_category(snap) {
        if let Some(field) = state.current_plugin_field(snap).cloned() {
            let plugin_id = state
                .current_plugin_category(snap)
                .map(|c| c.plugin.clone())
                .unwrap_or_default();
            use synaps_cli::skills::registry::PluginSettingsEditor as PE;
            match (key.code, &field.editor) {
                (KeyCode::Left | KeyCode::Right, PE::Cycler { options }) if !options.is_empty() => {
                    let current = plugin_field_current_value(&plugin_id, &field);
                    let idx = options.iter().position(|o| *o == current).unwrap_or(0);
                    let new_idx = match key.code {
                        KeyCode::Left => {
                            if idx > 0 {
                                idx - 1
                            } else {
                                idx
                            }
                        }
                        KeyCode::Right => {
                            if idx + 1 < options.len() {
                                idx + 1
                            } else {
                                idx
                            }
                        }
                        _ => idx,
                    };
                    if new_idx != idx {
                        state.row_error = None;
                        return InputOutcome::PluginApply {
                            plugin_id,
                            key: field.key.clone(),
                            value: options[new_idx].clone(),
                        };
                    }
                    return InputOutcome::None;
                }
                (KeyCode::Enter, PE::Text { numeric }) => {
                    state.row_error = None;
                    let buffer = plugin_field_current_value(&plugin_id, &field);
                    state.edit_mode = Some(ActiveEditor::PluginText {
                        plugin_id,
                        key: field.key.clone(),
                        buffer,
                        numeric: *numeric,
                        error: None,
                    });
                    return InputOutcome::None;
                }
                (KeyCode::Enter, PE::Picker) => {
                    // Picker options are not declarable in the manifest
                    // today (only Cycler carries inline options); show a
                    // note rather than opening an empty picker.
                    state.row_error =
                        Some((field.key.clone(), "picker editor not yet wired".to_string()));
                    return InputOutcome::None;
                }
                (KeyCode::Enter, PE::Cycler { options }) if !options.is_empty() => {
                    // Same as Right: advance one step, wrapping at the end.
                    let current = plugin_field_current_value(&plugin_id, &field);
                    let idx = options.iter().position(|o| *o == current).unwrap_or(0);
                    let new_idx = if idx + 1 < options.len() { idx + 1 } else { 0 };
                    state.row_error = None;
                    return InputOutcome::PluginApply {
                        plugin_id,
                        key: field.key.clone(),
                        value: options[new_idx].clone(),
                    };
                }
                (KeyCode::Enter, PE::Custom) => {
                    let category = state
                        .current_plugin_category(snap)
                        .map(|c| c.id.clone())
                        .unwrap_or_default();
                    return InputOutcome::PluginCustomOpen {
                        plugin_id,
                        category,
                        key: field.key.clone(),
                    };
                }
                _ => {}
            }
        }
    }
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => InputOutcome::Close,
        (KeyCode::Tab, _) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            // P7.7: focus toggle now goes through the FocusManager ring
            // (two-slot next()); `state.focus` is re-derived from it.
            state.toggle_focus();
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
                    if state.setting_idx > 0 {
                        state.setting_idx -= 1;
                    }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Down, _) => {
            match state.focus {
                Focus::Left => {
                    let total_categories = visible_categories(&snap.lifecycle_claims).len()
                        + snap.plugin_categories.len();
                    if state.category_idx + 1 < total_categories {
                        state.category_idx += 1;
                        state.setting_idx = 0;
                    }
                }
                Focus::Right => {
                    let n = row_count(state, snap);
                    if state.setting_idx + 1 < n {
                        state.setting_idx += 1;
                    }
                }
            }
            state.row_error = None;
            InputOutcome::None
        }
        (KeyCode::Left, _) | (KeyCode::Right, _) if state.focus == Focus::Right => {
            if let Some(def) = state.current_setting(snap) {
                let dyn_opts: Vec<String>;
                let opts_ref: &[&str];
                let dyn_strs: Vec<&str>;
                let options: &[&str] = match &def.editor {
                    EditorKind::Cycler(opts) => opts,
                    EditorKind::DynamicCycler => {
                        dyn_opts = snap.thinking_options.clone();
                        dyn_strs = dyn_opts.iter().map(|s| s.as_str()).collect();
                        opts_ref = &dyn_strs;
                        opts_ref
                    }
                    _ => return InputOutcome::None,
                };
                let current = cycler_current_value(def.key, snap);
                let idx = options.iter().position(|o| *o == current).unwrap_or(0);
                let new_idx = match key.code {
                    KeyCode::Left => {
                        if idx > 0 {
                            idx - 1
                        } else {
                            idx
                        }
                    }
                    KeyCode::Right => {
                        if idx + 1 < options.len() {
                            idx + 1
                        } else {
                            idx
                        }
                    }
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
            InputOutcome::None
        }
        (KeyCode::Enter, _) if state.focus == Focus::Right => {
            if let Some(def) = state.current_setting(snap) {
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
                        // Shared row source with the /models modal: same
                        // section builder, availability/login data, and live
                        // catalog overrides — exact provider-qualified values.
                        let rows = crate::tui::models::settings_model_picker_rows(
                            &snap.model,
                            &snap.catalog_overrides,
                            &snap.model_health,
                        );
                        let mut opts: Vec<String> = Vec::with_capacity(rows.len() + 1);
                        let mut values: Vec<String> = Vec::with_capacity(rows.len() + 1);
                        for (display, value) in rows {
                            opts.push(display);
                            values.push(value);
                        }
                        opts.push("Custom…".to_string());
                        values.push(String::new());

                        let current = current_value_for(def, snap);
                        let cursor = values
                            .iter()
                            .position(|v| !v.is_empty() && *v == current)
                            .unwrap_or(0);
                        state.edit_mode = Some(ActiveEditor::Picker {
                            setting_key: def.key,
                            options: opts,
                            values,
                            cursor,
                        });
                    }
                    EditorKind::ThemePicker => {
                        state.row_error = None;
                        let opts = super::theme_options();
                        let cursor = opts.iter().position(|o| o == &snap.theme_name).unwrap_or(0);
                        state.original_theme_name = Some(snap.theme_name.clone());
                        state.edit_mode = Some(ActiveEditor::Picker {
                            setting_key: "theme",
                            values: opts.clone(),
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
        ActiveEditor::Text {
            buffer,
            setting_key,
            numeric,
            error,
        } => match key.code {
            KeyCode::Enter => {
                if *numeric && buffer.parse::<u64>().is_err() {
                    *error = Some("must be a number".to_string());
                    return InputOutcome::None;
                }
                InputOutcome::Apply {
                    key: setting_key,
                    value: buffer.clone(),
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                *error = None;
                InputOutcome::None
            }
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
        },
        ActiveEditor::Picker {
            setting_key,
            options,
            values,
            cursor,
        } => {
            match key.code {
                KeyCode::Up => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        // Skip header rows
                        while *cursor > 0 && options[*cursor].starts_with("──") {
                            *cursor -= 1;
                        }
                    }
                    if *setting_key == "theme" {
                        return InputOutcome::PreviewTheme {
                            name: options[*cursor].clone(),
                        };
                    }
                    InputOutcome::None
                }
                KeyCode::Down => {
                    if *cursor + 1 < options.len() {
                        *cursor += 1;
                        // Skip header rows
                        while *cursor + 1 < options.len() && options[*cursor].starts_with("──")
                        {
                            *cursor += 1;
                        }
                    }
                    if *setting_key == "theme" {
                        return InputOutcome::PreviewTheme {
                            name: options[*cursor].clone(),
                        };
                    }
                    InputOutcome::None
                }
                KeyCode::Enter => {
                    let selection = options[*cursor].clone();
                    // Skip header rows (e.g. "── Groq ──")
                    if selection.starts_with("──") {
                        return InputOutcome::None;
                    }
                    if (*setting_key == "model" || *setting_key == "compaction_model")
                        && selection == "Custom…"
                    {
                        state.edit_mode = Some(ActiveEditor::CustomModel {
                            buffer: String::new(),
                            setting_key,
                        });
                        return InputOutcome::None;
                    }
                    // Exact-value application: the parallel `values` column
                    // carries the provider-qualified id verbatim — no display
                    // string parsing. Empty value = non-selectable row.
                    let value = values.get(*cursor).cloned().unwrap_or_default();
                    if value.is_empty() {
                        return InputOutcome::None;
                    }
                    let key = *setting_key;
                    if key == "theme" {
                        state.original_theme_name = None;
                    }
                    InputOutcome::Apply { key, value }
                }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::CustomModel {
            buffer,
            setting_key,
        } => match key.code {
            KeyCode::Enter => {
                if buffer.trim().is_empty() {
                    return InputOutcome::None;
                }
                InputOutcome::Apply {
                    key: setting_key,
                    value: buffer.trim().to_string(),
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                InputOutcome::None
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                InputOutcome::None
            }
            _ => InputOutcome::None,
        },
        ActiveEditor::ApiKey {
            provider_id,
            buffer,
        } => match key.code {
            KeyCode::Enter => InputOutcome::SetProviderKey {
                provider_id: provider_id.clone(),
                value: buffer.trim().to_string(),
            },
            KeyCode::Backspace => {
                buffer.pop();
                InputOutcome::None
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                InputOutcome::None
            }
            _ => InputOutcome::None,
        },
        ActiveEditor::PluginText {
            plugin_id,
            key: field_key,
            buffer,
            numeric,
            error,
        } => match key.code {
            KeyCode::Enter => {
                if *numeric && buffer.parse::<i64>().is_err() {
                    *error = Some("must be a number".to_string());
                    return InputOutcome::None;
                }
                InputOutcome::PluginApply {
                    plugin_id: plugin_id.clone(),
                    key: field_key.clone(),
                    value: buffer.clone(),
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                *error = None;
                InputOutcome::None
            }
            KeyCode::Char(c) => {
                if *numeric && !(c.is_ascii_digit() || c == '-') {
                    *error = Some("digits only".to_string());
                    return InputOutcome::None;
                }
                buffer.push(c);
                *error = None;
                InputOutcome::None
            }
            _ => InputOutcome::None,
        },
        ActiveEditor::PluginCustom { .. } => InputOutcome::None,
    }
}

fn cycler_current_value(key: &str, snap: &RuntimeSnapshot) -> String {
    match key {
        "thinking" => snap.thinking.clone(),
        "context_window" => snap.context_window.clone(),
        "tui_background_opaque" => {
            if snap.background_opaque {
                "opaque".to_string()
            } else {
                "invisible".to_string()
            }
        }
        "sidecar_toggle_key" => synaps_cli::config::read_config_value("sidecar_toggle_key")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "F8".to_string()),
        _ => String::new(),
    }
}

/// Read the current value for a plugin field. Falls back to the manifest
/// `default` (when present) or the empty string. Path B Phase 4.
pub(crate) fn plugin_field_current_value(
    plugin_id: &str,
    field: &synaps_cli::skills::registry::PluginSettingsField,
) -> String {
    if let Some(v) = synaps_cli::extensions::config_store::read_plugin_config(plugin_id, &field.key)
    {
        return v;
    }
    match &field.default {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn row_count(state: &SettingsState, snap: &RuntimeSnapshot) -> usize {
    if state.is_plugin_category(snap) {
        return state
            .current_plugin_category(snap)
            .map(|c| c.fields.len())
            .unwrap_or(0);
    }
    let visible = visible_categories(&snap.lifecycle_claims);
    // Guard against a stale `category_idx` parked past the list (e.g. a plugin
    // category hot-reloaded away while the modal is open): no category => no rows.
    let Some(&cat) = visible.get(state.category_idx) else {
        return 0;
    };
    if cat == super::schema::Category::Plugins {
        snap.plugins.len() + 1
    } else if cat == super::schema::Category::Providers {
        synaps_cli::runtime::openai::registry::providers().len() + 1 // +1 for Local row
    } else {
        state.current_settings(snap).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn snap() -> RuntimeSnapshot {
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
            plugins: vec![
                super::super::PluginRow {
                    name: "p1".into(),
                    skill_count: 1,
                },
                super::super::PluginRow {
                    name: "p2".into(),
                    skill_count: 2,
                },
            ],
            disabled_plugins: vec!["p2".into()],
            provider_key_status: std::collections::BTreeMap::new(),
            local_url_explicit: None,
            model_health: std::collections::HashMap::new(),
            plugin_categories: Vec::new(),
            lifecycle_claims: Vec::new(),
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

    fn plugins_state_at(idx: usize) -> SettingsState {
        let mut state = SettingsState::new();
        state.category_idx = super::super::schema::CATEGORIES
            .iter()
            .position(|c| *c == super::super::schema::Category::Plugins)
            .unwrap();
        state.set_focus(Focus::Right);
        state.setting_idx = idx;
        state
    }

    #[test]
    fn enter_on_marketplace_row_opens_plugins_marketplace() {
        let mut state = plugins_state_at(0);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snap(),
        );
        assert!(matches!(out, InputOutcome::OpenPluginsMarketplace));
    }

    #[test]
    fn enter_on_plugin_row_is_noop() {
        // Enter on a plugin row should NOT toggle — only Space does.
        let mut state = plugins_state_at(1);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snap(),
        );
        assert!(matches!(out, InputOutcome::None));
    }

    #[test]
    fn space_on_plugin_row_toggles_off() {
        // Row 1 is the first plugin (p1).
        let mut state = plugins_state_at(1);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &snap(),
        );
        match out {
            InputOutcome::TogglePlugin { name, enabled } => {
                assert_eq!(name, "p1");
                assert!(!enabled);
            }
            _ => panic!("expected TogglePlugin"),
        }
    }

    #[test]
    fn enter_on_disabled_plugin_is_noop() {
        // Enter on a disabled plugin row should NOT toggle.
        let mut state = plugins_state_at(2);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snap(),
        );
        assert!(matches!(out, InputOutcome::None));
    }

    #[test]
    fn space_on_disabled_plugin_toggles_on() {
        // Row 2 is the second plugin (p2, disabled).
        let mut state = plugins_state_at(2);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &snap(),
        );
        match out {
            InputOutcome::TogglePlugin { name, enabled } => {
                assert_eq!(name, "p2");
                assert!(enabled);
            }
            _ => panic!("expected TogglePlugin"),
        }
    }

    #[test]
    fn settings_anthropic_picker_emits_provider_qualified_id() {
        let mut state = SettingsState::new();
        state.edit_mode = Some(ActiveEditor::Picker {
            setting_key: "model",
            options: vec!["  anthropic/claude-sonnet-4-6  — Claude Sonnet".to_string()],
            values: vec!["anthropic/claude-sonnet-4-6".to_string()],
            cursor: 0,
        });
        assert!(matches!(
            handle_editor_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputOutcome::Apply { key: "model", value } if value == "anthropic/claude-sonnet-4-6"
        ));
    }

    #[test]
    fn settings_copilot_claude_picker_keeps_copilot_provider() {
        let mut state = SettingsState::new();
        state.edit_mode = Some(ActiveEditor::Picker {
            setting_key: "model",
            options: vec!["  github-copilot/claude-sonnet-4.6  — Claude Sonnet".to_string()],
            values: vec!["github-copilot/claude-sonnet-4.6".to_string()],
            cursor: 0,
        });
        assert!(matches!(
            handle_editor_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputOutcome::Apply { key: "model", value } if value == "github-copilot/claude-sonnet-4.6"
        ));
    }

    // ---- Slice A: settings model picker reuses /models section data ------

    /// Enter must apply the EXACT provider-qualified value carried in the
    /// parallel `values` column — no display-string parsing.
    #[test]
    fn model_picker_enter_emits_exact_value_from_values_column() {
        let mut state = SettingsState::new();
        state.edit_mode = Some(ActiveEditor::Picker {
            setting_key: "model",
            options: vec!["  ✅  79ms  confusing claude-like display text".to_string()],
            values: vec!["openai-codex/gpt-5.6-sol".to_string()],
            cursor: 0,
        });
        assert!(matches!(
            handle_editor_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputOutcome::Apply { key: "model", value } if value == "openai-codex/gpt-5.6-sol"
        ));
    }

    /// Header rows carry an empty value and must never apply.
    #[test]
    fn model_picker_enter_on_header_row_is_noop() {
        let mut state = SettingsState::new();
        state.edit_mode = Some(ActiveEditor::Picker {
            setting_key: "model",
            options: vec!["── OpenAI Codex ──".to_string()],
            values: vec![String::new()],
            cursor: 0,
        });
        assert!(matches!(
            handle_editor_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputOutcome::None
        ));
    }

    /// Enter on the Model row must open a picker whose rows come from the
    /// shared /models section builder: parallel display/value columns of the
    /// same length, with the trailing "Custom…" escape hatch preserved.
    #[test]
    fn enter_on_model_row_builds_picker_from_shared_sections() {
        let mut state = SettingsState::new();
        state.set_focus(Focus::Right);
        state.setting_idx = 0; // "model" is the first Model-category setting
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snap(),
        );
        assert!(matches!(out, InputOutcome::None));
        match &state.edit_mode {
            Some(ActiveEditor::Picker {
                setting_key,
                options,
                values,
                ..
            }) => {
                assert_eq!(*setting_key, "model");
                assert_eq!(
                    options.len(),
                    values.len(),
                    "display and value columns must stay parallel"
                );
                assert_eq!(options.last().unwrap(), "Custom…");
                // Every non-header, non-custom row must carry a non-empty
                // exact value; headers carry the empty string.
                for (display, value) in options.iter().zip(values.iter()) {
                    if display.starts_with("──") {
                        assert!(value.is_empty(), "header row must have empty value");
                    }
                }
            }
            other => panic!("expected model Picker edit mode, got {:?}", other.is_some()),
        }
    }

    // ---- Slice B: dynamic reasoning type row (display-only) --------------

    /// The Model category exposes a display-only "Reasoning" row whose value
    /// comes straight from the snapshot (recomputed per event from the exact
    /// active model). It must not open an editor or emit Apply.
    #[test]
    fn reasoning_type_row_is_display_only() {
        let mut s = snap();
        s.reasoning_type = "effort".into();
        let mut state = SettingsState::new();
        state.set_focus(Focus::Right);
        let idx = state
            .current_settings(&s)
            .iter()
            .position(|d| d.key == "reasoning_type")
            .expect("reasoning_type row must exist in the Model category");
        state.setting_idx = idx;
        let def = state.current_setting(&s).unwrap();
        assert!(matches!(def.editor, EditorKind::Display));
        assert_eq!(current_value_for(def, &s), "effort");

        // Enter must not open any editor.
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
        );
        assert!(matches!(out, InputOutcome::None));
        assert!(state.edit_mode.is_none());

        // Left/Right must not cycle/apply anything.
        for code in [KeyCode::Left, KeyCode::Right] {
            let out = handle_event(&mut state, KeyEvent::new(code, KeyModifiers::NONE), &s);
            assert!(matches!(out, InputOutcome::None));
        }
    }

    // ---- Path B Phase 4 — plugin-declared category wiring ----------------

    use synaps_cli::skills::registry::{
        PluginSettingsCategory, PluginSettingsEditor, PluginSettingsField,
    };

    fn plugin_field(key: &str, label: &str, editor: PluginSettingsEditor) -> PluginSettingsField {
        PluginSettingsField {
            key: key.to_string(),
            label: label.to_string(),
            editor,
            help: None,
            default: None,
        }
    }

    fn snap_with_plugin_cats(cats: Vec<PluginSettingsCategory>) -> RuntimeSnapshot {
        let mut s = snap();
        s.plugin_categories = cats;
        s
    }

    fn at_first_plugin_cat(s: &RuntimeSnapshot) -> SettingsState {
        let mut state = SettingsState::new();
        state.category_idx = super::super::schema::CATEGORIES.len();
        state.set_focus(Focus::Right);
        state.setting_idx = 0;
        // sanity
        assert!(state.is_plugin_category(s));
        state
    }

    #[test]
    fn plugin_categories_extend_left_pane_navigation() {
        let s = snap_with_plugin_cats(vec![PluginSettingsCategory {
            plugin: "demo".into(),
            id: "demo.main".into(),
            label: "Demo".into(),
            fields: vec![plugin_field(
                "speed",
                "Speed",
                PluginSettingsEditor::Cycler {
                    options: vec!["slow".into(), "fast".into()],
                },
            )],
        }]);
        let mut state = SettingsState::new();
        // Down across all built-ins, then once into plugin category.
        for _ in 0..super::super::schema::CATEGORIES.len() {
            handle_event(
                &mut state,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &s,
            );
        }
        assert_eq!(state.category_idx, super::super::schema::CATEGORIES.len());
        // One more Down should NOT advance past the last plugin category.
        handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &s,
        );
        assert_eq!(state.category_idx, super::super::schema::CATEGORIES.len());
    }

    #[test]
    fn cycler_right_emits_plugin_apply_with_next_option() {
        let s = snap_with_plugin_cats(vec![PluginSettingsCategory {
            plugin: "demo".into(),
            id: "demo.main".into(),
            label: "Demo".into(),
            fields: vec![plugin_field(
                "speed",
                "Speed",
                PluginSettingsEditor::Cycler {
                    options: vec!["slow".into(), "fast".into()],
                },
            )],
        }]);
        let mut state = at_first_plugin_cat(&s);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &s,
        );
        match out {
            InputOutcome::PluginApply {
                plugin_id,
                key,
                value,
            } => {
                assert_eq!(plugin_id, "demo");
                assert_eq!(key, "speed");
                assert_eq!(value, "fast");
            }
            _ => panic!("expected PluginApply, got something else"),
        }
    }

    #[test]
    fn enter_on_plugin_text_opens_editor_and_applies() {
        let s = snap_with_plugin_cats(vec![PluginSettingsCategory {
            plugin: "demo".into(),
            id: "demo.main".into(),
            label: "Demo".into(),
            fields: vec![plugin_field(
                "label",
                "Label",
                PluginSettingsEditor::Text { numeric: false },
            )],
        }]);
        let mut state = at_first_plugin_cat(&s);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
        );
        assert!(matches!(out, InputOutcome::None));
        assert!(matches!(
            state.edit_mode,
            Some(ActiveEditor::PluginText { .. })
        ));
        // Type "hi" then Enter.
        handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &s,
        );
        handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            &s,
        );
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
        );
        match out {
            InputOutcome::PluginApply {
                plugin_id,
                key,
                value,
            } => {
                assert_eq!(plugin_id, "demo");
                assert_eq!(key, "label");
                assert_eq!(value, "hi");
            }
            _ => panic!("expected PluginApply"),
        }
    }

    #[test]
    fn enter_on_plugin_custom_field_requests_plugin_editor_open() {
        let s = snap_with_plugin_cats(vec![PluginSettingsCategory {
            plugin: "demo".into(),
            id: "capture".into(),
            label: "Demo".into(),
            fields: vec![plugin_field("body", "Body", PluginSettingsEditor::Custom)],
        }]);
        let mut state = at_first_plugin_cat(&s);
        let out = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &s,
        );
        match out {
            InputOutcome::PluginCustomOpen {
                plugin_id,
                category,
                key,
            } => {
                assert_eq!(plugin_id, "demo");
                assert_eq!(category, "capture");
                assert_eq!(key, "body");
            }
            other => panic!(
                "expected PluginCustomOpen, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(
            state.edit_mode.is_none(),
            "async upper layer opens the editor after RPC returns"
        );
    }

    #[test]
    fn cycler_current_value_uses_plugin_default_when_unset() {
        // Default is honoured before we've ever written a value.
        let field = PluginSettingsField {
            key: "speed".into(),
            label: "Speed".into(),
            editor: PluginSettingsEditor::Cycler {
                options: vec!["slow".into(), "fast".into()],
            },
            help: None,
            default: Some(serde_json::Value::String("fast".into())),
        };
        // Use a plugin id that does not exist on disk so read returns None.
        let v = super::plugin_field_current_value("__nonexistent_plugin_xyz__", &field);
        assert_eq!(v, "fast");
    }
}
