use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::modal_kit::{centered, hint_line, open_modal, pad_to, Palette};
use super::theme::THEME;

pub(crate) enum HelpFindAction {
    None,
    Close,
}

pub(crate) fn handle_event(
    state: &mut synaps_cli::help::HelpFindState,
    key: KeyEvent,
) -> HelpFindAction {
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
    let width = ((area.width as u32 * 8 / 10) as u16)
        .max(50)
        .min(area.width);
    let height = ((area.height as u32 * 8 / 10) as u16)
        .max(14)
        .min(area.height);
    let theme = THEME.load();
    let p = Palette::for_modal(&theme, None);
    let modal = centered(area, width, height);

    if let Some(entry) = state.detail_entry().cloned() {
        let body = open_modal(frame, area, modal, &p, &entry.command, "esc back");
        render_detail(frame, body, &entry, &p);
        return;
    }
    let inner = open_modal(frame, area, modal, &p, "Find help", "esc close");
    if inner.height < 4 {
        return;
    }

    let [search_bar, _, results, _, status_bar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Search field: a raised surface the width of the modal (Noodle's input).
    let mut field = vec![Span::styled(" \u{2315} ", Style::default().fg(p.dim))];
    if state.filter().is_empty() {
        field.push(Span::styled("\u{2588}", Style::default().fg(p.accent)));
        field.push(Span::styled(
            " type to filter",
            Style::default().fg(p.dim).add_modifier(Modifier::ITALIC),
        ));
    } else {
        field.push(Span::styled(
            state.filter().to_string(),
            Style::default().fg(p.text),
        ));
        field.push(Span::styled("\u{2588}", Style::default().fg(p.accent)));
    }
    let field: Vec<Span<'static>> = field
        .into_iter()
        .map(|s| {
            let st = s.style.bg(p.selected);
            s.style(st)
        })
        .collect();
    let mut field = field;
    pad_to(&mut field, search_bar.width, p.selected);
    frame.render_widget(Paragraph::new(Line::from(field)), search_bar);

    let visible_height = results.height as usize;
    state.set_visible_height(visible_height);
    let rows = state.filtered_rows();
    let result_count = state.filtered_entries().len();
    let lines: Vec<Line<'static>> = if rows.is_empty() {
        state
            .no_results_message()
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(p.dim),
                ))
            })
            .collect()
    } else {
        let rendered_rows = render_help_find_rows(&rows, state, results.width, &p);
        let row_heights = rendered_rows.iter().map(Vec::len).collect::<Vec<_>>();
        let start = synaps_cli::help::visible_help_find_window(
            &row_heights,
            state.cursor(),
            state.scroll(),
            visible_height,
        );
        let mut used_height = 0usize;
        rendered_rows
            .into_iter()
            .skip(start)
            .take_while(|row_lines| {
                let next = used_height + row_lines.len().max(1);
                if next <= visible_height {
                    used_height = next;
                    true
                } else {
                    false
                }
            })
            .flatten()
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), results);

    let hint = format!(
        "{} result{}  \u{2191}\u{2193} move  enter details  type filter  esc close",
        result_count,
        if result_count == 1 { "" } else { "s" }
    );
    frame.render_widget(
        Paragraph::new(hint_line(&p, &hint, status_bar.width)),
        status_bar,
    );
}

fn highlighted_spans(text: &str, query: &str, base: Style, matched: Style) -> Vec<Span<'static>> {
    synaps_cli::help::highlight_segments(text, query)
        .into_iter()
        .map(|segment| Span::styled(segment.text, if segment.matched { matched } else { base }))
        .collect()
}

fn render_help_find_rows(
    rows: &[synaps_cli::help::HelpFindRow<'_>],
    state: &synaps_cli::help::HelpFindState,
    width: u16,
    p: &Palette,
) -> Vec<Vec<Line<'static>>> {
    rows.iter()
        .enumerate()
        .map(|(idx, row)| match row {
            synaps_cli::help::HelpFindRow::Category(category) => {
                let spacer = if idx == 0 {
                    Vec::new()
                } else {
                    vec![Line::from("")]
                };
                spacer
                    .into_iter()
                    .chain(std::iter::once(Line::from(Span::styled(
                        category.to_string(),
                        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
                    ))))
                    .collect()
            }
            synaps_cli::help::HelpFindRow::Entry(entry) => {
                let selected = idx == state.cursor();
                let command_style = if selected {
                    Style::default().fg(p.value).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(p.text)
                };
                let summary_style = Style::default().fg(p.dim);
                let match_style = Style::default().fg(p.value).add_modifier(Modifier::BOLD);
                // Two columns for the selection bar.
                let inner_w = usize::from(width).saturating_sub(2).max(10);
                wrapped_entry_lines(
                    selected,
                    entry,
                    state.filter(),
                    inner_w,
                    command_style,
                    summary_style,
                    match_style,
                )
                .into_iter()
                .map(|line| {
                    let bg = if selected { p.selected } else { p.panel };
                    let mut spans = vec![Span::styled(
                        if selected { "\u{2503} " } else { "  " },
                        Style::default().fg(p.accent).bg(bg),
                    )];
                    spans.extend(line.spans.into_iter().map(|s| {
                        let st = s.style.bg(bg);
                        s.style(st)
                    }));
                    pad_to(&mut spans, width, bg);
                    Line::from(spans)
                })
                .collect()
            }
        })
        .collect()
}

fn wrapped_entry_lines(
    selected: bool,
    entry: &synaps_cli::help::HelpEntry,
    query: &str,
    width: usize,
    command_style: Style,
    summary_style: Style,
    match_style: Style,
) -> Vec<Line<'static>> {
    // The `┃` bar marks the selection, so the wrapper's own `›` marker is
    // not wanted (`selected: false`).
    let _ = selected;
    synaps_cli::help::wrap_help_find_entry_lines(&entry.command, &entry.summary, false, width)
        .into_iter()
        .map(|(command_part, summary)| {
            // Drop the wrapper's 2-column marker gutter: the bar gutter
            // replaces it.
            let command_part = command_part
                .strip_prefix("  ")
                .unwrap_or(&command_part)
                .to_string();
            let mut spans = highlighted_spans(&command_part, query, command_style, match_style);
            if !summary.is_empty() {
                if !command_part.trim().is_empty() {
                    spans.push(Span::raw("  "));
                }
                spans.extend(highlighted_spans(
                    &summary,
                    query,
                    summary_style,
                    match_style,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn wrapped_styled_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    synaps_cli::help::wrap_help_text(text, width)
        .into_iter()
        .map(|line| Line::from(Span::styled(line, style)))
        .collect()
}

fn render_detail(frame: &mut Frame, inner: Rect, entry: &synaps_cli::help::HelpEntry, p: &Palette) {
    let w = inner.width as usize;
    let heading = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            Style::default().fg(p.title).add_modifier(Modifier::BOLD),
        ))
    };
    let text = Style::default().fg(p.text);
    let dim = Style::default().fg(p.dim);
    let mut lines = vec![
        Line::from(Span::styled(
            entry.title.clone(),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    lines.extend(wrapped_styled_lines(&entry.summary, w, text));
    if let Some(source) = synaps_cli::help::source_display(entry) {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("from ", dim),
            Span::styled(source, text),
        ]));
    }
    lines.push(Line::from(""));
    lines.extend(
        entry
            .lines
            .iter()
            .flat_map(|line| wrapped_styled_lines(line, w, text)),
    );
    if let Some(usage) = entry
        .usage
        .as_ref()
        .filter(|usage| !usage.trim().is_empty())
    {
        lines.push(Line::from(""));
        lines.push(heading("Usage"));
        lines.extend(wrapped_styled_lines(
            &format!("  {}", usage),
            w,
            Style::default().fg(p.value),
        ));
    }
    if !entry.examples.is_empty() {
        lines.push(Line::from(""));
        lines.push(heading("Examples"));
        for example in &entry.examples {
            if example.description.trim().is_empty() {
                lines.extend(wrapped_styled_lines(
                    &format!("  {}", example.command),
                    w,
                    Style::default().fg(p.value),
                ));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:<16} ", example.command),
                        Style::default().fg(p.value),
                    ),
                    Span::styled(example.description.clone(), dim),
                ]));
            }
        }
    }
    let related_in_body = entry
        .lines
        .iter()
        .any(|l| l.trim_start().starts_with("Related:"));
    if !entry.related.is_empty() && !related_in_body {
        lines.push(Line::from(""));
        lines.extend(wrapped_styled_lines(
            &format!("Related: {}", entry.related.join(", ")),
            w,
            dim,
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod noodle_tests {
    //! Help is a borderless modal (after Noodle): no frame, a search field
    //! on a raised surface, selection as a `┃` bar.
    use crate::tui::testing::TestHarness;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Frame glyphs in the modal rows (the header's `│` separator is
    /// app chrome, above the modal).
    fn frame_glyphs(s: &str) -> Vec<char> {
        s.lines()
            .skip(2)
            .flat_map(str::chars)
            .filter(|c| "╭╮╰╯│┌┐└┘".contains(*c))
            .collect()
    }

    #[test]
    fn help_find_is_borderless_with_a_bar_selection() {
        let mut h = TestHarness::boot_with_size(100, 30);
        h.run_slash_command("help", "find");
        let frame = h.snapshot();
        assert!(frame.contains("Find help"), "{frame}");
        assert!(frame_glyphs(&frame).is_empty(), "no frame: {frame}");
        assert!(
            frame.contains("\u{2503} /"),
            "bar on the selected entry: {frame}"
        );
        assert!(!frame.contains("\u{203a} /"), "no › marker next to the bar");

        h.key(KeyCode::Enter, KeyModifiers::empty());
        let detail = h.snapshot();
        assert!(detail.contains("esc back"), "{detail}");
        assert!(
            frame_glyphs(&detail).is_empty(),
            "detail has no frame either"
        );
        assert!(
            detail.matches("Related:").count() <= 1,
            "related listed once"
        );
    }
}
