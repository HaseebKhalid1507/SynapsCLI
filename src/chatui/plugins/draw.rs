use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use super::PluginsModalState;
use super::state::{Focus, LeftRow, RightMode, RightRow};
use crate::theme::THEME;

const OVERLAY_MAX_WIDTH: u16 = 70;
const OVERLAY_HEIGHT: u16 = 7;

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let w = (area.width.saturating_mul(8) / 10).max(60).min(area.width);
    let h = (area.height.saturating_mul(7) / 10).max(20).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" Plugins ")
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

    render_left(frame, panes[0], state);
    render_right(frame, panes[1], state);
    render_footer(frame, outer[1], state);
}

fn render_left(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let rows = state.left_rows();
    let installed_count = state.file.installed.len();
    let mut lines = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.selected_left;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected && matches!(state.focus, Focus::Left) {
            Style::default().fg(THEME.load().claude_label)
        } else if selected {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        let label = match row {
            LeftRow::Installed => {
                if installed_count > 0 {
                    format!("Installed ({})", installed_count)
                } else {
                    "Installed".to_string()
                }
            }
            LeftRow::Marketplace(name) => {
                let count = state.file.marketplaces.iter()
                    .find(|m| &m.name == name)
                    .map(|m| m.cached_plugins.len())
                    .unwrap_or(0);
                if count > 0 {
                    format!("{} ({})", name, count)
                } else {
                    name.clone()
                }
            }
            LeftRow::AddMarketplace => "+ Add Marketplace…".to_string(),
        };
        lines.push(Line::from(vec![Span::styled(format!("{}{}", marker, label), style)]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_right(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    // Always render the list behind overlays so users see context.
    render_right_list(frame, area, state);
    match &state.mode {
        RightMode::List => {}
        RightMode::Detail { row_idx } => render_right_detail(frame, area, state, *row_idx),
        RightMode::AddMarketplaceEditor { buffer, error } => {
            render_add_editor(frame, area, buffer, error.as_deref())
        }
        RightMode::TrustPrompt { plugin_name, host, .. } => {
            render_trust_prompt(frame, area, plugin_name, host)
        }
        RightMode::Confirm { prompt, .. } => render_confirm(frame, area, prompt),
    }
}

fn render_right_list(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let rows = state.right_rows();
    if rows.is_empty() {
        let hint = match state.left_rows().get(state.selected_left) {
            Some(LeftRow::AddMarketplace) => "  Press Enter to add a marketplace.",
            Some(LeftRow::Installed) => "  No plugins installed.",
            Some(LeftRow::Marketplace(_)) => "  No cached plugins. Press r to refresh.",
            None => "",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(THEME.load().help_fg),
            ))),
            area,
        );
        return;
    }

    let mut lines = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.selected_right && matches!(state.focus, Focus::Right);
        let style = if selected {
            Style::default().fg(THEME.load().claude_label)
        } else if i == state.selected_right {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        let (name, status) = match row {
            RightRow::Installed(ip) => {
                let mut s = String::from("installed");
                let up_to_date = matches!(&ip.latest_commit, Some(c) if c == &ip.installed_commit);
                if !up_to_date {
                    s.push_str(" (update)");
                }
                (ip.name.clone(), s)
            }
            RightRow::Browseable { plugin, installed } => {
                let status = if *installed { "installed" } else { "available" };
                (plugin.name.clone(), status.to_string())
            }
        };
        lines.push(Line::from(vec![Span::styled(
            format!("  {:<20} {}", name, status),
            style,
        )]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_right_detail(frame: &mut Frame, area: Rect, state: &PluginsModalState, row_idx: usize) {
    // Inset overlay panel for detail content.
    let rect = inset_rect(area, 2, 1);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = state.right_rows();
    let Some(row) = rows.get(row_idx) else {
        frame.render_widget(
            Paragraph::new("(no selection)")
                .style(Style::default().fg(THEME.load().help_fg)),
            inner,
        );
        return;
    };

    let label_style = Style::default().fg(THEME.load().help_fg);
    let value_style = Style::default().fg(THEME.load().claude_text);
    let mut lines: Vec<Line> = Vec::new();
    match row {
        RightRow::Installed(ip) => {
            lines.push(Line::from(vec![
                Span::styled("name:        ", label_style),
                Span::styled(ip.name.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("source:      ", label_style),
                Span::styled(ip.source_url.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("marketplace: ", label_style),
                Span::styled(
                    ip.marketplace.clone().unwrap_or_else(|| "(direct)".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("commit:      ", label_style),
                Span::styled(ip.installed_commit.clone(), value_style),
            ]));
            let latest = ip.latest_commit.clone().unwrap_or_else(|| "?".to_string());
            let up_to_date = matches!(&ip.latest_commit, Some(c) if c == &ip.installed_commit);
            let mut latest_line = latest;
            if !up_to_date {
                latest_line.push_str("  (update available)");
            }
            lines.push(Line::from(vec![
                Span::styled("latest:      ", label_style),
                Span::styled(latest_line, value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("installed:   ", label_style),
                Span::styled(ip.installed_at.clone(), value_style),
            ]));
        }
        RightRow::Browseable { plugin, installed } => {
            lines.push(Line::from(vec![
                Span::styled("name:        ", label_style),
                Span::styled(plugin.name.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("source:      ", label_style),
                Span::styled(plugin.source.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("version:     ", label_style),
                Span::styled(
                    plugin.version.clone().unwrap_or_else(|| "?".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("description: ", label_style),
                Span::styled(
                    plugin.description.clone().unwrap_or_else(|| "no description".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("status:      ", label_style),
                Span::styled(
                    if *installed { "installed" } else { "available" }.to_string(),
                    value_style,
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn centered_overlay(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let w = area.width.saturating_sub(4).clamp(24, OVERLAY_MAX_WIDTH);
    let h = OVERLAY_HEIGHT;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect { x, y, width: w, height: h.min(area.height) };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn render_add_editor(frame: &mut Frame, area: Rect, buffer: &str, error: Option<&str>) {
    let inner = centered_overlay(frame, area, " Add Marketplace ");

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Enter marketplace URL:",
        Style::default().fg(THEME.load().help_fg),
    )));
    lines.push(Line::from(Span::styled(
        format!("[{}_]", buffer),
        Style::default().fg(THEME.load().claude_label),
    )));
    if let Some(err) = error {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            format!("! {}", err),
            Style::default().fg(THEME.load().error_color),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_trust_prompt(frame: &mut Frame, area: Rect, plugin_name: &str, host: &str) {
    let inner = centered_overlay(frame, area, " Trust Host ");

    let lines = vec![
        Line::from(Span::styled(
            format!("Trust source {} for plugin {}?", host, plugin_name),
            Style::default().fg(THEME.load().claude_text),
        )),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            "  [y]es  [n]o",
            Style::default().fg(THEME.load().help_fg),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_confirm(frame: &mut Frame, area: Rect, prompt: &str) {
    let inner = centered_overlay(frame, area, " Confirm ");

    let lines = vec![
        Line::from(Span::styled(
            prompt.to_string(),
            Style::default().fg(THEME.load().claude_text),
        )),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            "  [y]es  [n]o",
            Style::default().fg(THEME.load().help_fg),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let hint = match (&state.focus, &state.mode) {
        (_, RightMode::Detail { .. }) => "Esc back",
        (_, RightMode::AddMarketplaceEditor { .. }) => "Type URL  Enter submit  Esc cancel",
        (_, RightMode::TrustPrompt { .. }) => "y trust  n cancel",
        (_, RightMode::Confirm { .. }) => "y yes  n no  Esc cancel",
        (Focus::Left, RightMode::List) => {
            "↑↓ nav  Tab switch  Enter select  r refresh  R remove  Esc close"
        }
        (Focus::Right, RightMode::List) => {
            "↑↓ nav  Tab switch  Enter detail  i install  e enable  d disable  u update  U uninstall  Esc close"
        }
    };

    if let Some(err) = &state.row_error {
        let spans = vec![
            Span::styled(format!("! {}  ", err), Style::default().fg(THEME.load().error_color)),
            Span::styled(hint.to_string(), Style::default().fg(THEME.load().help_fg)),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else {
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(THEME.load().help_fg)),
            area,
        );
    }
}

fn inset_rect(area: Rect, dx: u16, dy: u16) -> Rect {
    let w = area.width.saturating_sub(dx * 2);
    let h = area.height.saturating_sub(dy * 2);
    Rect {
        x: area.x + dx.min(area.width),
        y: area.y + dy.min(area.height),
        width: w,
        height: h,
    }
}
