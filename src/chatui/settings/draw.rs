use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, BorderType, Clear, Paragraph};
use super::{SettingsState, Focus};
use super::schema::{CATEGORIES, SettingDef};
use crate::theme::THEME;

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &SettingsState) {
    // Centered modal — 80% width, 70% height, min 60x20
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

    // Split into left (categories) / right (settings) with a footer hint line
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(1)])
        .split(outer[0]);

    render_categories(frame, panes[0], state);
    render_settings(frame, panes[1], state);
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

fn render_settings(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let settings = state.current_settings();
    let mut lines = Vec::new();
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && state.focus == Focus::Right;
        let style = if selected {
            Style::default().fg(THEME.claude_label)
        } else {
            Style::default().fg(THEME.claude_text)
        };
        let current_value = current_value_for(def);
        lines.push(ratatui::text::Line::from(vec![
            ratatui::text::Span::styled(format!("  {:<20} {}", def.label, current_value), style),
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

/// Read the persisted value for a setting. Placeholder — filled in Task 12.
fn current_value_for(_def: &SettingDef) -> String {
    String::from("(...)")
}
