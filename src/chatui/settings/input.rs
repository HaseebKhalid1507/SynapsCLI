use crossterm::event::{KeyCode, KeyModifiers, KeyEvent};
use super::{SettingsState, Focus, RuntimeSnapshot, ActiveEditor};
use super::schema::{CATEGORIES, EditorKind};
use super::draw::current_value_for;

pub(crate) enum InputOutcome {
    None,
    Close,
    Apply { key: &'static str, value: String },
    /// Apply a plugin-declared settings field. Written to the plugin's
    /// own namespaced config (`~/.synaps-cli/plugins/<id>/config`).
    PluginApply { plugin_id: String, key: String, value: String },
    /// User requested to open a plugin-declared custom editor.
    /// The async upper layer calls `settings.editor.open` and installs
    /// `ActiveEditor::PluginCustom` with the returned render payload.
    PluginCustomOpen { plugin_id: String, category: String, key: String },
    SetProviderKey { provider_id: String, value: String },
    TogglePlugin { name: String, enabled: bool },
    PreviewTheme { name: String },
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
                Some(ActiveEditor::Picker { setting_key: "theme", .. })
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
    if state.focus == Focus::Right && state.category_idx < CATEGORIES.len() {
        let cat = super::schema::CATEGORIES[state.category_idx];
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
                        let current_url = snap.provider_keys.get("local.url")
                            .cloned()
                            .unwrap_or_default();
                        state.edit_mode = Some(ActiveEditor::ApiKey {
                            provider_id: "local.url".to_string(),
                            buffer: current_url,
                        });
                        return InputOutcome::None;
                    }
                    KeyCode::Delete | KeyCode::Char('d') => {
                        if snap.provider_keys.contains_key("local.url") {
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
                            let has_key = snap.provider_keys.get(p.key).map(|v| !v.is_empty()).unwrap_or(false);
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
                        KeyCode::Left => if idx > 0 { idx - 1 } else { idx },
                        KeyCode::Right => if idx + 1 < options.len() { idx + 1 } else { idx },
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
                    state.row_error = Some((
                        field.key.clone(),
                        "picker editor not yet wired".to_string(),
                    ));
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
                    let total_categories = CATEGORIES.len() + snap.plugin_categories.len();
                    if state.category_idx + 1 < total_categories {
                        state.category_idx += 1;
                        state.setting_idx = 0;
                    }
                }
                Focus::Right => {
                    let n = row_count(state, snap);
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
                        // Anthropic models
                        let mut opts: Vec<String> = vec!["── Anthropic ──".to_string()];
                        opts.extend(synaps_cli::models::KNOWN_MODELS
                            .iter().map(|(id, desc)| format!("  {}  — {}", id, desc)));

                        // Provider models (only for configured providers)
                        let registry = synaps_cli::runtime::openai::registry::providers();
                        for spec in registry {
                            let has_config_key = snap.provider_keys.contains_key(spec.key);
                            let has_env_key = spec.env_vars.iter()
                                .any(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()));
                            if !has_config_key && !has_env_key { continue; }
                            opts.push(format!("── {} ──", spec.name));
                            for (id, label, tier) in spec.models {
                                let full = format!("{}/{}", spec.key, id);
                                let health = snap.model_health.get(&full)
                                    .map(|(s, ms)| format!("{} {:>6}  ", s.icon(), fmt_latency(*s, *ms)))
                                    .unwrap_or_default();
                                opts.push(format!("  {}{}  — {} [{}]", health, full, label, tier));
                            }
                        }
                        opts.push("Custom…".to_string());

                        let current = current_value_for(def, snap);
                        let cursor = opts.iter()
                            .position(|o| o.trim_start().starts_with(&current))
                            .unwrap_or(0);
                        state.edit_mode = Some(ActiveEditor::Picker {
                            setting_key: def.key,
                            options: opts,
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
                KeyCode::Up => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        // Skip header rows
                        while *cursor > 0 && options[*cursor].starts_with("──") {
                            *cursor -= 1;
                        }
                    }
                    if *setting_key == "theme" {
                        return InputOutcome::PreviewTheme { name: options[*cursor].clone() };
                    }
                    InputOutcome::None
                }
                KeyCode::Down => {
                    if *cursor + 1 < options.len() {
                        *cursor += 1;
                        // Skip header rows
                        while *cursor + 1 < options.len() && options[*cursor].starts_with("──") {
                            *cursor += 1;
                        }
                    }
                    if *setting_key == "theme" {
                        return InputOutcome::PreviewTheme { name: options[*cursor].clone() };
                    }
                    InputOutcome::None
                }
                KeyCode::Enter => {
                    let selection = options[*cursor].clone();
                    // Skip header rows (e.g. "── Groq ──")
                    if selection.starts_with("──") {
                        return InputOutcome::None;
                    }
                    if (*setting_key == "model" || *setting_key == "compaction_model") && selection == "Custom…" {
                        state.edit_mode = Some(ActiveEditor::CustomModel { buffer: String::new(), setting_key });
                        return InputOutcome::None;
                    }
                    let raw = selection.split("  —").next().unwrap_or(&selection).trim();
                    // Strip health prefix (e.g. "✅  79ms  groq/..." or "✅  1304ms  nvidia/...")
                    // Find the model ID by looking for known provider prefixes or "claude-"
                    let value = if let Some(pos) = raw.find("claude-") {
                        raw[pos..].to_string()
                    } else if let Some(pos) = raw.find('/') {
                        // Find start of provider key before the slash (e.g. "groq/", "nvidia/")
                        let before = &raw[..pos];
                        let key_start = before.rfind(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                            .map(|i| i + before[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1))
                            .unwrap_or(0);
                        raw[key_start..].to_string()
                    } else {
                        raw.to_string()
                    };
                    let key = *setting_key;
                    if key == "theme" {
                        state.original_theme_name = None;
                    }
                    InputOutcome::Apply { key, value }
                }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::CustomModel { buffer, setting_key } => {
            match key.code {
                KeyCode::Enter => {
                    if buffer.trim().is_empty() {
                        return InputOutcome::None;
                    }
                    InputOutcome::Apply { key: setting_key, value: buffer.trim().to_string() }
                }
                KeyCode::Backspace => { buffer.pop(); InputOutcome::None }
                KeyCode::Char(c) => { buffer.push(c); InputOutcome::None }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::ApiKey { provider_id, buffer } => {
            match key.code {
                KeyCode::Enter => {
                    InputOutcome::SetProviderKey {
                        provider_id: provider_id.clone(),
                        value: buffer.trim().to_string(),
                    }
                }
                KeyCode::Backspace => { buffer.pop(); InputOutcome::None }
                KeyCode::Char(c) => { buffer.push(c); InputOutcome::None }
                _ => InputOutcome::None,
            }
        }
        ActiveEditor::PluginText { plugin_id, key: field_key, buffer, numeric, error } => {
            match key.code {
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
                KeyCode::Backspace => { buffer.pop(); *error = None; InputOutcome::None }
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
            }
        }
        ActiveEditor::PluginCustom { .. } => InputOutcome::None,
    }
}

fn cycler_current_value(key: &str, snap: &RuntimeSnapshot) -> String {
    match key {
        "thinking" => snap.thinking.clone(),
        "context_window" => snap.context_window.clone(),
        "voice_toggle_key" => synaps_cli::config::read_config_value("voice_toggle_key")
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
    if let Some(v) = synaps_cli::extensions::config_store::read_plugin_config(plugin_id, &field.key) {
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
    let cat = super::schema::CATEGORIES[state.category_idx];
    if cat == super::schema::Category::Plugins {
        snap.plugins.len() + 1
    } else if cat == super::schema::Category::Providers {
        synaps_cli::runtime::openai::registry::providers().len() + 1 // +1 for Local row
    } else {
        state.current_settings().len()
    }
}

fn fmt_latency(status: synaps_cli::runtime::openai::ping::PingStatus, ms: u64) -> String {
    use synaps_cli::runtime::openai::ping::PingStatus;
    match status {
        PingStatus::Online => {
            if ms >= 1000 { format!("{:.1}s", ms as f64 / 1000.0) }
            else { format!("{}ms", ms) }
        }
        other => other.label().to_string(),
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
            plugins: vec![
                super::super::PluginRow { name: "p1".into(), skill_count: 1 },
                super::super::PluginRow { name: "p2".into(), skill_count: 2 },
            ],
            disabled_plugins: vec!["p2".into()],
            provider_keys: std::collections::BTreeMap::new(),
            model_health: std::collections::HashMap::new(),
            plugin_categories: Vec::new(),
        }
    }

    fn plugins_state_at(idx: usize) -> SettingsState {
        let mut state = SettingsState::new();
        state.category_idx = super::super::schema::CATEGORIES
            .iter().position(|c| *c == super::super::schema::Category::Plugins).unwrap();
        state.focus = Focus::Right;
        state.setting_idx = idx;
        state
    }

    #[test]
    fn enter_on_marketplace_row_opens_plugins_marketplace() {
        let mut state = plugins_state_at(0);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap());
        assert!(matches!(out, InputOutcome::OpenPluginsMarketplace));
    }

    #[test]
    fn enter_on_plugin_row_is_noop() {
        // Enter on a plugin row should NOT toggle — only Space does.
        let mut state = plugins_state_at(1);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap());
        assert!(matches!(out, InputOutcome::None));
    }

    #[test]
    fn space_on_plugin_row_toggles_off() {
        // Row 1 is the first plugin (p1).
        let mut state = plugins_state_at(1);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &snap());
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
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap());
        assert!(matches!(out, InputOutcome::None));
    }

    #[test]
    fn space_on_disabled_plugin_toggles_on() {
        // Row 2 is the second plugin (p2, disabled).
        let mut state = plugins_state_at(2);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &snap());
        match out {
            InputOutcome::TogglePlugin { name, enabled } => {
                assert_eq!(name, "p2");
                assert!(enabled);
            }
            _ => panic!("expected TogglePlugin"),
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
        state.focus = Focus::Right;
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
                PluginSettingsEditor::Cycler { options: vec!["slow".into(), "fast".into()] },
            )],
        }]);
        let mut state = SettingsState::new();
        // Down across all built-ins, then once into plugin category.
        for _ in 0..super::super::schema::CATEGORIES.len() {
            handle_event(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &s);
        }
        assert_eq!(state.category_idx, super::super::schema::CATEGORIES.len());
        // One more Down should NOT advance past the last plugin category.
        handle_event(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &s);
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
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &s);
        match out {
            InputOutcome::PluginApply { plugin_id, key, value } => {
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
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &s);
        assert!(matches!(out, InputOutcome::None));
        assert!(matches!(state.edit_mode, Some(ActiveEditor::PluginText { .. })));
        // Type "hi" then Enter.
        handle_event(&mut state, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &s);
        handle_event(&mut state, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &s);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &s);
        match out {
            InputOutcome::PluginApply { plugin_id, key, value } => {
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
            id: "voice".into(),
            label: "Demo".into(),
            fields: vec![plugin_field("body", "Body", PluginSettingsEditor::Custom)],
        }]);
        let mut state = at_first_plugin_cat(&s);
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &s);
        match out {
            InputOutcome::PluginCustomOpen { plugin_id, category, key } => {
                assert_eq!(plugin_id, "demo");
                assert_eq!(category, "voice");
                assert_eq!(key, "body");
            }
            other => panic!("expected PluginCustomOpen, got {:?}",
                std::mem::discriminant(&other)),
        }
        assert!(state.edit_mode.is_none(), "async upper layer opens the editor after RPC returns");
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
