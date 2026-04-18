use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, BorderType, Clear, Paragraph};
use super::{SettingsState, Focus, RuntimeSnapshot, ActiveEditor};
use super::schema::{CATEGORIES, SettingDef, EditorKind};
use crate::theme::THEME;

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
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
        .border_style(Style::default().fg(THEME.border_active))
        .style(Style::default().bg(THEME.bg));
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

    render_categories(frame, panes[0], state);
    render_settings(frame, panes[1], state, snap);
    render_footer(frame, outer[1]);
}

fn render_categories(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let mut lines = Vec::new();
    for (i, cat) in CATEGORIES.iter().enumerate() {
        let marker = if i == state.category_idx { "▸ " } else { "  " };
        let style = if i == state.category_idx && state.focus == Focus::Left {
            Style::default().fg(THEME.claude_label)
        } else if i == state.category_idx {
            Style::default().fg(THEME.claude_text)
        } else {
            Style::default().fg(THEME.help_fg)
        };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}{}", marker, cat.label()), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_settings(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let current_cat = super::schema::CATEGORIES[state.category_idx];
    if current_cat == super::schema::Category::Plugins {
        render_plugins_list(frame, area, state, snap);
        return;
    }
    let settings = state.current_settings();
    let selected_key = settings.get(state.setting_idx).map(|d| d.key);
    let mut lines = Vec::new();
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.claude_label)
        } else {
            Style::default().fg(THEME.claude_text)
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
                (Some(ActiveEditor::CustomModel { buffer }), _)
                    if def.key == "model" => {
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
                let color = if is_note { THEME.help_fg } else { THEME.error_color };
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

fn render_plugins_list(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let mut lines = Vec::new();
    if snap.plugins.is_empty() {
        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            "  No plugins installed. Open /plugins to add a marketplace.",
            Style::default().fg(THEME.help_fg),
        )]));
    } else {
        for (i, p) in snap.plugins.iter().enumerate() {
            let disabled = snap.disabled_plugins.iter().any(|d| d == &p.name);
            let status = if disabled { "✗ disabled" } else { "✓ enabled" };
            let skills_part = if p.skill_count > 0 {
                format!("  ({} skills)", p.skill_count)
            } else {
                String::new()
            };
            let selected = i == state.setting_idx && state.focus == Focus::Right;
            let style = if selected {
                Style::default().fg(THEME.claude_label)
            } else {
                Style::default().fg(THEME.claude_text)
            };
            lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
                format!("  {:<20} {}{}", p.name, status, skills_part),
                style,
            )]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_picker(frame: &mut Frame, area: Rect, options: &[String], cursor: usize) {
    let w = area.width.saturating_sub(4).min(60).max(20);
    let h = (options.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let x = area.x + 2;
    let y = area.y + 2;
    let rect = Rect { x, y, width: w, height: h };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.border_active))
        .style(Style::default().bg(THEME.bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = Vec::new();
    for (i, opt) in options.iter().enumerate() {
        let style = if i == cursor {
            Style::default().fg(THEME.claude_label)
        } else {
            Style::default().fg(THEME.claude_text)
        };
        let marker = if i == cursor { "▸ " } else { "  " };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("{}{}", marker, opt), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let hint = "↑↓ navigate  Tab switch pane  Enter edit  Esc close";
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(THEME.help_fg)),
        area,
    );
}

pub(crate) fn current_value_for(def: &SettingDef, snap: &RuntimeSnapshot) -> String {
    match def.key {
        "model" => snap.model.clone(),
        "thinking" => snap.thinking.clone(),
        "api_retries" => snap.api_retries.to_string(),
        "subagent_timeout" => format!("{}s", snap.subagent_timeout),
        "max_tool_output" => snap.max_tool_output.to_string(),
        "bash_timeout" => format!("{}s", snap.bash_timeout),
        "bash_max_timeout" => format!("{}s", snap.bash_max_timeout),
        "theme" => snap.theme_name.clone(),
        _ => "?".into(),
    }
}
