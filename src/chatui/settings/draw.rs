use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, BorderType, Clear, Paragraph};
use super::{SettingsState, Focus, RuntimeSnapshot, ActiveEditor};
use super::schema::{SettingDef, EditorKind, visible_categories};
use super::super::theme::THEME;

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
) {
    let w = (area.width.saturating_mul(8) / 10).max(60).min(area.width);
    let h = (area.height.saturating_mul(7) / 10).max(20).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(1)])
        .split(outer[0]);

    render_categories(frame, panes[0], state, snap);
    render_settings(frame, panes[1], state, snap);
    render_footer(frame, outer[1], state, snap);

    if let Some(ActiveEditor::PluginCustom { render, .. }) = &state.edit_mode {
        render_plugin_custom_editor(frame, panes[1], render);
    }
}

fn render_categories(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let mut lines = Vec::new();
    let cats = visible_categories(&snap.lifecycle_claims);
    let n_builtin = cats.len();
    for (i, cat) in cats.iter().enumerate() {
        let marker = if i == state.category_idx { "▸ " } else { "  " };
        let style = if i == state.category_idx && state.focus == Focus::Left {
            Style::default().fg(THEME.load().claude_label)
        } else if i == state.category_idx {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}{}", marker, cat.label()), style),
        ]));
    }
    for (i, pcat) in snap.plugin_categories.iter().enumerate() {
        let abs = n_builtin + i;
        let marker = if abs == state.category_idx { "▸ " } else { "  " };
        let style = if abs == state.category_idx && state.focus == Focus::Left {
            Style::default().fg(THEME.load().claude_label)
        } else if abs == state.category_idx {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        // Source label: "<Label> (<plugin>)" so users can tell who owns it.
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(
                format!("{}{} ({})", marker, pcat.label, pcat.plugin),
                style,
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_settings(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    if state.is_plugin_category(snap) {
        render_plugin_category(frame, area, state, snap);
        return;
    }
    let current_cat = visible_categories(&snap.lifecycle_claims)[state.category_idx];
    if current_cat == super::schema::Category::Plugins {
        render_plugins_list(frame, area, state, snap);
        return;
    }
    if current_cat == super::schema::Category::Providers {
        render_providers_list(frame, area, state, snap);
        return;
    }
    let settings = state.current_settings();
    let selected_key = settings.get(state.setting_idx).map(|d| d.key);
    let mut lines = Vec::new();
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        let current_value = current_value_for(def, snap);
        let value_display = if selected {
            match (&state.edit_mode, &def.editor) {
                (Some(ActiveEditor::Text { buffer, setting_key, error, .. }), _)
                    if *setting_key == def.key => {
                    let mut s = format!("[{}_]", buffer);
                    if let Some(err) = error {
                        s.push_str(&format!("  ! {}", err));
                    }
                    s
                }
                (Some(ActiveEditor::CustomModel { buffer, setting_key }), _)
                    if *setting_key == def.key => {
                    format!("[{}_]", buffer)
                }
                (None, EditorKind::Cycler(_)) => {
                    format!("◀ {} ▶", current_value)
                }
                _ => current_value,
            }
        } else {
            current_value
        };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("  {:<20} {}", def.label, value_display), style),
        ]));
        if let Some((key, msg)) = &state.row_error {
            if selected_key == Some(key.as_str()) && i == state.setting_idx {
                let is_note = msg.starts_with("saved");
                let color = if is_note { THEME.load().help_fg } else { THEME.load().error_color };
                lines.push(ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!("    {}", msg), Style::default().fg(color)),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);

    if let Some(ActiveEditor::Picker { options, cursor, .. }) = &state.edit_mode {
        render_picker(frame, area, options, *cursor);
    }
}

fn render_plugin_category(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
) {
    use super::input::plugin_field_current_value;
    let cat = match state.current_plugin_category(snap) {
        Some(c) => c,
        None => return,
    };
    let mut lines: Vec<ratatui::text::Line> = Vec::new();
    // Header — makes the source explicit so users can audit.
    lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
        format!("  Plugin: {}", cat.plugin),
        Style::default().fg(THEME.load().help_fg),
    )]));
    for (i, field) in cat.fields.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        let current = plugin_field_current_value(&cat.plugin, field);
        use synaps_cli::skills::registry::PluginSettingsEditor as PE;
        let display = if selected {
            match (&state.edit_mode, &field.editor) {
                (Some(super::ActiveEditor::PluginText { plugin_id, key, buffer, error, .. }), _)
                    if *plugin_id == cat.plugin && *key == field.key =>
                {
                    let mut s = format!("[{}_]", buffer);
                    if let Some(err) = error {
                        s.push_str(&format!("  ! {}", err));
                    }
                    s
                }
                (None, PE::Cycler { .. }) => format!("◀ {} ▶", current),
                (Some(super::ActiveEditor::PluginCustom { plugin_id, field: active_field, .. }), PE::Custom)
                    if *plugin_id == cat.plugin && *active_field == field.key =>
                {
                    "(custom editor open)".to_string()
                }
                (_, PE::Custom) => "(custom — Enter to edit)".to_string(),
                _ => current.clone(),
            }
        } else if matches!(field.editor, PE::Custom) {
            "(custom)".to_string()
        } else {
            current.clone()
        };
        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            format!("  {:<20} {}", field.label, display),
            style,
        )]));
        if let Some((rk, msg)) = &state.row_error {
            let want = format!("plugin.{}.{}", cat.plugin, field.key);
            if selected && rk == &want {
                let is_note = msg.starts_with("saved");
                let color = if is_note { THEME.load().help_fg } else { THEME.load().error_color };
                lines.push(ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!("    {}", msg), Style::default().fg(color)),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_plugins_list(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let mut lines = Vec::new();

    // Row 0 — action row. Styled distinctly so it reads as a button, not a plugin.
    let action_selected = state.setting_idx == 0 && state.focus == Focus::Right;
    let action_style = if action_selected {
        Style::default()
            .fg(THEME.load().claude_label)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        Style::default()
            .fg(THEME.load().claude_text)
            .add_modifier(ratatui::style::Modifier::BOLD)
    };
    lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
        "  + Open Plugin Marketplace…",
        action_style,
    )]));

    // Surface load errors / notes attached to the action row.
    if let Some((key, msg)) = &state.row_error {
        if key == "plugins" {
            let is_note = msg.starts_with("saved");
            let color = if is_note { THEME.load().help_fg } else { THEME.load().error_color };
            lines.push(ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(format!("    {}", msg), Style::default().fg(color)),
            ]));
        }
    }

    // Rows 1..=n — installed plugins at snap.plugins[idx - 1].
    for (i, p) in snap.plugins.iter().enumerate() {
        let row_idx = i + 1;
        let disabled = snap.disabled_plugins.iter().any(|d| d == &p.name);
        let status = if disabled { "✗ disabled" } else { "✓ enabled" };
        let skills_part = if p.skill_count > 0 {
            format!("  ({} skills)", p.skill_count)
        } else {
            String::new()
        };
        let selected = row_idx == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            format!("  {:<20} {}{}", p.name, status, skills_part),
            style,
        )]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_providers_list(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let providers = synaps_cli::runtime::openai::registry::providers();
    let total_rows = providers.len() + 1; // +1 for Local
    let visible_height = area.height as usize;
    let selected = if state.focus == Focus::Right { state.setting_idx } else { usize::MAX };

    // Scroll offset — keep selected row in view (no scroll when focus is on left pane)
    let scroll_offset = if selected == usize::MAX {
        0
    } else if selected >= visible_height {
        selected.saturating_sub(visible_height - 1)
    } else {
        0
    };

    let mut lines = Vec::new();

    // Row 0: Local models
    if scroll_offset == 0 {
        let is_selected = 0 == selected;
        let style = if is_selected {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        let local_url = snap.provider_keys.get("local.url")
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| std::env::var("LOCAL_ENDPOINT").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "localhost:11434".to_string());

        let local_status = if snap.provider_keys.contains_key("local.url")
            || std::env::var("LOCAL_ENDPOINT").is_ok_and(|s| !s.is_empty())
        {
            format!("✅ {}", local_url)
        } else {
            format!("⬚ default ({})", local_url)
        };

        // Show editor if active on this row
        let display = if let Some(ActiveEditor::ApiKey { provider_id, buffer }) = &state.edit_mode {
            if provider_id == "local.url" {
                format!("[{}_]", buffer)
            } else {
                local_status
            }
        } else {
            local_status
        };

        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            format!("  {:<20} {}", "Local (Ollama/etc)", display),
            style,
        )]));

        if let Some((key, msg)) = &state.row_error {
            if key == "provider.local.url" && is_selected {
                let is_note = msg.starts_with("saved");
                let color = if is_note { THEME.load().help_fg } else { THEME.load().error_color };
                lines.push(ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!("    {}", msg), Style::default().fg(color)),
                ]));
            }
        }
    }

    // Rows 1..=N: Registry providers
    for (i, p) in providers.iter().enumerate() {
        let row_idx = i + 1; // offset by 1 for Local row
        if row_idx < scroll_offset || row_idx >= scroll_offset + visible_height {
            continue;
        }
        let is_selected = row_idx == selected;
        let style = if is_selected {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };

        let status = if let Some(ActiveEditor::ApiKey { provider_id, buffer }) = &state.edit_mode {
            if provider_id == p.key {
                let masked: String = "*".repeat(buffer.len().min(32));
                format!("[{}_]", masked)
            } else {
                provider_status(p, snap)
            }
        } else {
            provider_status(p, snap)
        };

        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            format!("  {:<20} {}", p.name, status),
            style,
        )]));

        if let Some((key, msg)) = &state.row_error {
            if key == &format!("provider.{}", p.key) && is_selected {
                let is_note = msg.starts_with("saved");
                let color = if is_note { THEME.load().help_fg } else { THEME.load().error_color };
                lines.push(ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(format!("    {}", msg), Style::default().fg(color)),
                ]));
            }
        }
    }

    // Scroll indicators
    if scroll_offset > 0 {
        lines.insert(0, ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            "  ▲ more", Style::default().fg(THEME.load().help_fg),
        )]));
    }
    if scroll_offset + visible_height < total_rows {
        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            "  ▼ more", Style::default().fg(THEME.load().help_fg),
        )]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn provider_status(p: &synaps_cli::runtime::openai::registry::ProviderSpec, snap: &RuntimeSnapshot) -> String {
    let key_status = if let Some(k) = snap.provider_keys.get(p.key).filter(|s| !s.is_empty()) {
        format!("✅ {}", mask_key(k))
    } else if p.env_vars.iter().any(|v| std::env::var(v).is_ok()) {
        "✅ (from env)".to_string()
    } else {
        return "⬚ not set".to_string(); // No key = no ping data relevant
    };

    // Append ping summary if available — count online/total models for this provider
    let models: Vec<_> = p.models.iter()
        .filter_map(|(id, _, _)| {
            let full_key = format!("{}/{}", p.key, id);
            snap.model_health.get(&full_key).map(|(s, ms)| (s, *ms))
        })
        .collect();

    if models.is_empty() {
        return key_status;
    }

    let online = models.iter().filter(|(s, _)| {
        matches!(s, synaps_cli::runtime::openai::ping::PingStatus::Online)
    }).count();
    let total = models.len();
    let fastest = models.iter()
        .filter(|(s, _)| matches!(s, synaps_cli::runtime::openai::ping::PingStatus::Online))
        .map(|(_, ms)| *ms)
        .min();

    let ping_str = if let Some(ms) = fastest {
        format!("  ({}/{} online, fastest {}ms)", online, total, ms)
    } else {
        format!("  (0/{} online)", total)
    };

    format!("{}{}", key_status, ping_str)
}

fn mask_key(key: &str) -> String {
    let n = key.len();
    if n <= 8 { return "*".repeat(n); }
    format!("***...{}", &key[n - 4..])
}


fn render_plugin_custom_editor(
    frame: &mut Frame,
    area: Rect,
    session: &super::plugin_editor::PluginEditorSession,
) {
    let rows = &session.render.rows;
    let cursor = session.render.cursor.unwrap_or(0);
    let footer_lines: u16 = if session.render.footer.is_some() { 1 } else { 0 };
    let w = area.width.saturating_sub(4).clamp(40, 100);
    let needed = rows.len() as u16 + 2 + footer_lines;
    let h = needed.clamp(3, area.height.saturating_sub(2).max(3));
    let rect = Rect { x: area.x + 2, y: area.y + 2, width: w, height: h };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(format!(" {} · {} ", session.plugin_id, session.field))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let (list_area, footer_area) = if footer_lines > 0 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(footer_lines)])
            .split(inner);
        (split[0], Some(split[1]))
    } else {
        (inner, None)
    };

    let visible_height = list_area.height as usize;
    let scroll_offset = if cursor >= visible_height { cursor - visible_height + 1 } else { 0 };
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(scroll_offset).take(visible_height) {
        let mut style = if i == cursor {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        if !row.selectable {
            style = Style::default().fg(THEME.load().help_fg);
        }
        let marker = row.marker.as_deref().unwrap_or(" ");
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}  {}", marker, row.label), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
    if let (Some(area), Some(footer)) = (footer_area, &session.render.footer) {
        frame.render_widget(
            Paragraph::new(footer.clone()).style(Style::default().fg(THEME.load().help_fg)),
            area,
        );
    }
}

fn render_picker(frame: &mut Frame, area: Rect, options: &[String], cursor: usize) {
    let w = area.width.saturating_sub(4).clamp(20, 100);
    let h = (options.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let x = area.x + 2;
    let y = area.y + 2;
    let rect = Rect { x, y, width: w, height: h };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let visible_height = inner.height as usize;
    let scroll_offset = if cursor >= visible_height {
        cursor - visible_height + 1
    } else {
        0
    };

    let mut lines = Vec::new();
    for (i, opt) in options.iter().enumerate().skip(scroll_offset).take(visible_height) {
        let style = if i == cursor {
            Style::default().fg(THEME.load().claude_label)
        } else {
            Style::default().fg(THEME.load().claude_text)
        };
        let marker = if i == cursor { "▸ " } else { "  " };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}{}", marker, opt), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let cats = visible_categories(&snap.lifecycle_claims);
    let cat = cats.get(state.category_idx).copied().unwrap_or(super::schema::Category::Plugins);
    let on_plugins_right = cat == super::schema::Category::Plugins
        && state.focus == Focus::Right;
    let on_providers_right = cat == super::schema::Category::Providers
        && state.focus == Focus::Right;
    let in_api_key_editor = matches!(state.edit_mode, Some(ActiveEditor::ApiKey { .. }));
    let hint = if in_api_key_editor {
        "type key  Enter save  Esc cancel"
    } else if on_plugins_right && state.setting_idx == 0 {
        "↑↓ navigate  Tab switch pane  Enter open marketplace  Esc close"
    } else if on_plugins_right && state.setting_idx > 0 {
        "↑↓ navigate  Tab switch pane  Space toggle  Esc close"
    } else if on_providers_right {
        "↑↓ navigate  Tab switch pane  Enter set key  d/Del clear  p ping  Esc close"
    } else {
        "↑↓ navigate  Tab switch pane  Enter edit  Esc close"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(THEME.load().help_fg)),
        area,
    );
}

pub(crate) fn current_value_for(def: &SettingDef, snap: &RuntimeSnapshot) -> String {
    match def.key {
        "model" => snap.model.clone(),
        "thinking" => snap.thinking.clone(),
        "context_window" => snap.context_window.clone(),
        "compaction_model" => snap.compaction_model.clone(),
        "api_retries" => snap.api_retries.to_string(),
        "subagent_timeout" => format!("{}s", snap.subagent_timeout),
        "max_tool_output" => snap.max_tool_output.to_string(),
        "bash_timeout" => format!("{}s", snap.bash_timeout),
        "bash_max_timeout" => format!("{}s", snap.bash_max_timeout),
        "theme" => snap.theme_name.clone(),
        "sidecar_toggle_key" => synaps_cli::config::read_config_value("sidecar_toggle_key")
            .or_else(|| synaps_cli::config::read_config_value("voice_toggle_key"))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "F8".to_string()),
        _ => "?".into(),
    }
}
