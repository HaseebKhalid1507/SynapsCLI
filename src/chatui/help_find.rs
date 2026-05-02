use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Clear, Paragraph},
    Frame,
};

use super::theme::THEME;

pub(crate) enum HelpFindAction {
    None,
    Close,
}

pub(crate) fn handle_event(state: &mut synaps_cli::help::HelpFindState, key: KeyEvent) -> HelpFindAction {
    if state.detail_entry().is_some() {
        match key.code {
            KeyCode::Esc => {
                state.close_detail();
                return HelpFindAction::None;
            }
            _ => return HelpFindAction::None,
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => HelpFindAction::Close,
        (KeyCode::Enter, _) => {
            state.open_selected();
            HelpFindAction::None
        }
        (KeyCode::Up, _) => {
            state.move_up();
            HelpFindAction::None
        }
        (KeyCode::Down, _) => {
            state.move_down();
            HelpFindAction::None
        }
        (KeyCode::Backspace, _) => {
            state.backspace();
            HelpFindAction::None
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            state.clear_filter();
            HelpFindAction::None
        }
        (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
            state.push_char(ch);
            HelpFindAction::None
        }
        _ => HelpFindAction::None,
    }
}

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut synaps_cli::help::HelpFindState) {
    let width = ((area.width as u32 * 8 / 10) as u16).max(50).min(area.width);
    let height = ((area.height as u32 * 8 / 10) as u16).max(14).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let modal = Rect::new(x, y, width, height);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(" Find help ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    if let Some(entry) = state.detail_entry().cloned() {
        render_detail(frame, modal, &entry);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let search = Paragraph::new(Line::from(vec![
        Span::styled("Search: ", Style::default().fg(THEME.load().muted)),
        Span::styled(state.filter().to_string(), Style::default().fg(THEME.load().input_fg)),
    ]));
    frame.render_widget(search, chunks[0]);

    let visible_height = chunks[1].height as usize;
    state.set_visible_height(visible_height);
    let rows = state.filtered_rows();
    let start = state.scroll().min(rows.len());
    let end = (start + visible_height).min(rows.len());
    let result_count = state.filtered_entries().len();
    let lines: Vec<Line<'static>> = if rows.is_empty() {
        state
            .no_results_message()
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), Style::default().fg(THEME.load().muted))))
            .collect()
    } else {
        rows[start..end]
            .iter()
            .enumerate()
            .map(|(offset, row)| {
                let idx = start + offset;
                match row {
                    synaps_cli::help::HelpFindRow::Category(category) => Line::from(Span::styled(
                        format!("  {}", category),
                        Style::default().fg(THEME.load().muted).add_modifier(Modifier::BOLD),
                    )),
                    synaps_cli::help::HelpFindRow::Entry(entry) => {
                        let selected = idx == state.cursor();
                        let marker = if selected { "›" } else { " " };
                        let command_style = if selected {
                            Style::default().fg(THEME.load().claude_label).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(THEME.load().input_fg)
                        };
                        let summary_style = Style::default().fg(THEME.load().muted);
                        let match_style = Style::default().fg(THEME.load().claude_label).add_modifier(Modifier::BOLD);
                        let mut spans = vec![Span::styled(marker.to_string(), command_style), Span::raw(" ")];
                        spans.extend(highlighted_spans(
                            &format!("{:<18}", entry.command),
                            state.filter(),
                            command_style,
                            match_style,
                        ));
                        spans.extend(highlighted_spans(&entry.summary, state.filter(), summary_style, match_style));
                        Line::from(spans)
                    }
                }
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    let footer = format!("{} result{}  ↑↓ move  Enter details  type filter  Esc close", result_count, if result_count == 1 { "" } else { "s" });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, Style::default().fg(THEME.load().muted)))),
        chunks[2],
    );
}

fn highlighted_spans(text: &str, query: &str, base: Style, matched: Style) -> Vec<Span<'static>> {
    synaps_cli::help::highlight_segments(text, query)
        .into_iter()
        .map(|segment| Span::styled(segment.text, if segment.matched { matched } else { base }))
        .collect()
}

fn render_detail(frame: &mut Frame, modal: Rect, entry: &synaps_cli::help::HelpEntry) {
    let block = Block::default()
        .title(format!(" {} ", entry.command))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active));
    let inner = block.inner(modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);

    let mut lines = vec![
        Line::from(Span::styled(entry.title.clone(), Style::default().fg(THEME.load().claude_label).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(entry.summary.clone(), Style::default().fg(THEME.load().input_fg))),
    ];
    if let Some(source) = synaps_cli::help::source_display(entry) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Source: {}", source),
            Style::default().fg(THEME.load().muted),
        )));
    }
    lines.push(Line::from(""));
    lines.extend(entry.lines.iter().map(|line| Line::from(Span::raw(line.clone()))));
    if let Some(usage) = entry.usage.as_ref().filter(|usage| !usage.trim().is_empty()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Usage", Style::default().fg(THEME.load().claude_label).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(Span::raw(format!("  {}", usage))));
    }
    if !entry.examples.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Examples", Style::default().fg(THEME.load().claude_label).add_modifier(Modifier::BOLD))));
        for example in &entry.examples {
            let rendered = if example.description.trim().is_empty() {
                format!("  {}", example.command)
            } else {
                format!("  {:<16} {}", example.command, example.description)
            };
            lines.push(Line::from(Span::raw(rendered)));
        }
    }
    lines.push(Line::from(""));
    if !entry.related.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Related: {}", entry.related.join(", ")),
            Style::default().fg(THEME.load().muted),
        )));
    }
    lines.push(Line::from(Span::styled("Esc back", Style::default().fg(THEME.load().muted))));
    frame.render_widget(Paragraph::new(lines), inner);
}
