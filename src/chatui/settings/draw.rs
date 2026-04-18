use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, BorderType, Clear, Paragraph};
use super::{SettingsState, Focus, RuntimeSnapshot};
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
    let settings = state.current_settings();
    let mut lines = Vec::new();
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.claude_label)
        } else {
            Style::default().fg(THEME.claude_text)
        };
        let current_value = current_value_for(def, snap);
        let value_display = if selected && matches!(def.editor, EditorKind::Cycler(_)) {
            format!("◀ {} ▶", current_value)
        } else {
            current_value
        };
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("  {:<20} {}", def.label, value_display), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
        "skills" => snap.skills.as_ref()
            .map(|s| if s.is_empty() { "(none)".to_string() } else { s.join(",") })
            .unwrap_or_else(|| "(none)".into()),
        "api_retries" => snap.api_retries.to_string(),
        "subagent_timeout" => format!("{}s", snap.subagent_timeout),
        "max_tool_output" => snap.max_tool_output.to_string(),
        "bash_timeout" => format!("{}s", snap.bash_timeout),
        "bash_max_timeout" => format!("{}s", snap.bash_max_timeout),
        "theme" => snap.theme_name.clone(),
        _ => "?".into(),
    }
}
