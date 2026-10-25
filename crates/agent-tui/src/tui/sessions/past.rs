//! The **Past** tab — placeholder for PR 3's catalog-backed browser.
//!
//! For now: the newest 50 saved sessions (`list_recent_sessions`, loaded on
//! `spawn_blocking` at open), a plain list, and a dim "search & filters
//! coming" note. Everything Past-specific lives in this file — state, keys,
//! drawing — so the PR 3 browser replaces it wholesale.

use super::super::modal_kit::{pad_to, row_line, Palette};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

/// How many saved sessions the placeholder lists.
pub(crate) const PAST_LIMIT: usize = 50;

#[derive(Clone, Debug, Default)]
pub(crate) struct PastState {
    /// `None` while loading.
    pub(crate) rows: Option<Vec<synaps_cli::SessionInfo>>,
    pub(crate) error: Option<String>,
    pub(crate) cursor: usize,
}

impl PastState {
    pub(crate) fn set(&mut self, result: Result<Vec<synaps_cli::SessionInfo>, String>) {
        match result {
            Ok(mut rows) => {
                rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
                rows.truncate(PAST_LIMIT);
                self.cursor = self.cursor.min(rows.len().saturating_sub(1));
                self.rows = Some(rows);
                self.error = None;
            }
            Err(e) => {
                self.rows = Some(Vec::new());
                self.error = Some(e);
            }
        }
    }

    /// The count shown on the tab (`Past 50`); `None` while loading.
    pub(crate) fn count(&self) -> Option<usize> {
        self.rows.as_ref().map(Vec::len)
    }

    /// Keys for the Past tab (navigation only for now). `true` = consumed.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let n = self.rows.as_ref().map_or(0, Vec::len);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < n {
                    self.cursor += 1;
                }
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = n.saturating_sub(1);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn hint(&self) -> &'static str {
        "\u{2191}\u{2193} navigate  tab live  esc close"
    }
}

/// `11h ago`.
fn ago(now: chrono::DateTime<chrono::Utc>, t: chrono::DateTime<chrono::Utc>) -> String {
    format!("{} ago", super::short_dur((now - t).num_seconds()))
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    st: &PastState,
    now: chrono::DateTime<chrono::Utc>,
    p: &Palette,
) {
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(p.dim);
    let mut note = vec![Span::styled(
        "  newest saved sessions \u{00b7} search & filters coming".to_string(),
        dim,
    )];
    pad_to(&mut note, area.width, p.panel);
    lines.push(Line::from(note));
    lines.push(Line::from(""));
    let Some(rows) = &st.rows else {
        lines.push(Line::from(Span::styled("  loading saved sessions\u{2026}", dim)));
        frame.render_widget(ratatui::widgets::Paragraph::new(lines), area);
        return;
    };
    if let Some(e) = &st.error {
        lines.push(Line::from(Span::styled(
            format!("  could not list saved sessions: {e}"),
            Style::default().fg(p.error),
        )));
    } else if rows.is_empty() {
        lines.push(Line::from(Span::styled("  no saved sessions", dim)));
    }
    let w = usize::from(area.width);
    let first = lines.len();
    for (i, s) in rows.iter().enumerate() {
        let selected = i == st.cursor;
        let id = match s.name.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => n.to_string(),
            None => format!("\u{2026}{}", super::short_id(&s.id)),
        };
        let meta = format!(
            "{:>9} {:>8} {:>8}",
            match s.message_count {
                0 => String::new(),
                1 => "1 msg".to_string(),
                n => format!("{n} msgs"),
            },
            super::cost_text(s.session_cost),
            ago(now, s.updated_at),
        );
        // bar(2) + label(12) + space + title + 2 + meta
        let title_w = w.saturating_sub(2 + 13 + 2 + meta.chars().count());
        let title = if s.title.is_empty() { "(no title)".to_string() } else { s.title.clone() };
        let title = super::draw::fit(&title, title_w);
        let fg = if selected { p.value } else { p.text };
        lines.push(row_line(
            p,
            area.width,
            12,
            &super::draw::fit(&id, 12),
            vec![
                Span::styled(format!("{title:<title_w$}  "), Style::default().fg(fg)),
                Span::styled(meta, dim),
            ],
            selected,
            true,
        ));
    }
    let sel = first + st.cursor;
    super::super::modal_kit::render_scrolled(frame, area, lines, (sel, sel + 1));
}
