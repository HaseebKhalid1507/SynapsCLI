//! Past-tab drawing: search line, facet sidebar | day-grouped virtual list,
//! preview strip, rename popup. Only the visible list lines are built.

use super::{FacetItem, Focus, ListLine, PastState};
use crate::tui::modal_kit::{editor_spans, open_popup, pad_to, row_line, Palette};
use crate::tui::sessions::draw::{cell, fit, rcell};
use crate::tui::sessions::{cost_text, short_dur, short_id, SessionsState};
use crate::tui::text_metrics::width as text_w;
use agent_core::session_catalog::{CatalogEntry, DeepHit, DeepRole};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::sync::atomic::Ordering;

const NAME_W: usize = 10;
/// Preview strip: separator + 4 lines.
const PREVIEW_H: u16 = 5;

fn sidebar_w(width: u16) -> u16 {
    match width {
        0..=59 => 0,
        60..=99 => 20,
        _ => 26,
    }
}

pub(crate) fn render(frame: &mut Frame, area: Rect, st: &SessionsState, p: &Palette) {
    if area.height == 0 || area.width < 20 {
        return;
    }
    let past = &st.past;
    let row = |y: u16| Rect { y, height: 1, ..area };
    frame.render_widget(Paragraph::new(search_line(past, p, area.width)), row(area.y));
    if area.height < 3 {
        return;
    }
    let body = Rect { y: area.y + 2, height: area.height - 2, ..area };
    if past.loading() {
        let msg = Span::styled("  loading saved sessions\u{2026}", Style::default().fg(p.dim));
        frame.render_widget(Paragraph::new(Line::from(msg)), body);
        return;
    }
    if let Some(e) = &past.error {
        let msg = Span::styled(fit(&format!("  {e}"), usize::from(body.width)), Style::default().fg(p.error));
        frame.render_widget(Paragraph::new(Line::from(msg)), body);
        return;
    }
    let preview_h = if body.height >= 14 { PREVIEW_H } else { 0 };
    let panes = Rect { height: body.height - preview_h, ..body };
    let sw = sidebar_w(area.width);
    let list = if sw > 0 {
        let side = Rect { width: sw, ..panes };
        render_sidebar(frame, side, past, st.home.as_deref(), p);
        let sep = Rect { x: panes.x + sw, width: 1, ..panes };
        let bar: Vec<Line<'static>> =
            (0..sep.height).map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(p.dim)))).collect();
        frame.render_widget(Paragraph::new(bar), sep);
        Rect { x: panes.x + sw + 2, width: panes.width.saturating_sub(sw + 2), ..panes }
    } else {
        panes
    };
    past.viewport.store(list.height, Ordering::Relaxed);
    match &past.deep {
        Some(_) => render_hits(frame, list, past, p),
        None => render_list(frame, list, past, p),
    }
    if preview_h > 0 {
        let strip = Rect { y: panes.bottom(), height: preview_h, ..body };
        render_preview(frame, strip, past, st.home.as_deref(), p);
    }
    if past.rename.is_some() {
        render_rename(frame, list, past, p);
    }
}

/// `/ query▏` on the left; hidden/progress status on the right.
fn search_line(past: &PastState, p: &Palette, width: u16) -> Line<'static> {
    let w = usize::from(width);
    let dim = Style::default().fg(p.dim);
    let deep = past.deep.as_ref();
    let mut right = match deep {
        Some(d) => {
            let n = d.hits.len();
            let hits = format!("{n} hit{}", if n == 1 { "" } else { "s" });
            if d.running {
                format!("{hits} \u{00b7} {}/{} files", d.done, d.total)
            } else if d.cancelled {
                format!("{hits} \u{00b7} stopped at {}/{}", d.done, d.total)
            } else if let Some(s) = &d.summary {
                if s.capped {
                    format!("{hits} (cap) \u{00b7} {}/{} files", s.files_done, s.files_total)
                } else {
                    format!("{hits} \u{00b7} {} files", s.files_total)
                }
            } else {
                "enter to search conversation text".to_string()
            }
        }
        None => past.hiding_text(),
    };
    if let Some((done, total)) = past.previews {
        if !right.is_empty() {
            right.push_str("  ");
        }
        right.push_str(&format!("previews {done}/{total}"));
    }
    let prefix = if deep.is_some() { "deep \u{25b8} " } else { "/ " };
    let text = past.search_text();
    let mut spans = vec![Span::styled(prefix.to_string(), Style::default().fg(p.accent))];
    let right_w = text_w(&right);
    let left_room = w.saturating_sub(right_w + 2 + text_w(prefix) + 1);
    if text.is_empty() && !past.editing {
        let ph = if deep.is_some() { "phrase" } else { "search  title, cwd, messages \u{00b7} cwd: model: $>5 msgs>20 after:3d" };
        spans.push(Span::styled(fit(ph, left_room), dim));
    } else {
        // Keep the tail (where the cursor is) visible.
        let shown = tail_fit(text, left_room);
        spans.push(Span::styled(shown, Style::default().fg(p.value)));
        if past.editing {
            spans.push(Span::styled("\u{258f}", Style::default().fg(p.accent)));
        }
    }
    let used: usize = spans.iter().map(|s| text_w(&s.content)).sum();
    if !right.is_empty() && used + 2 + right_w <= w {
        spans.push(Span::raw(" ".repeat(w - used - right_w)));
        spans.push(Span::styled(right, dim));
    }
    Line::from(spans)
}

fn tail_fit(s: &str, w: usize) -> String {
    if text_w(s) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut used = 1; // the leading …
    for c in chars.iter().rev() {
        let cw = text_w(c.encode_utf8(&mut [0; 4]));
        if used + cw > w {
            break;
        }
        out.insert(0, *c);
        used += cw;
    }
    format!("\u{2026}{out}")
}

fn render_sidebar(frame: &mut Frame, area: Rect, past: &PastState, home: Option<&std::path::Path>, p: &Palette) {
    let focused = past.focus == Focus::Sidebar;
    let w = usize::from(area.width);
    let items = &past.derived.sidebar;
    let mut lines = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let (label, count) = match item {
            FacetItem::Header(h) => {
                let mut s = vec![Span::styled(
                    format!("  {h}"),
                    Style::default().fg(p.dim).add_modifier(Modifier::BOLD),
                )];
                pad_to(&mut s, area.width, p.panel);
                lines.push(Line::from(s));
                continue;
            }
            FacetItem::When(wh, n) => (wh.label().to_string(), *n),
            // The modal's `$HOME` (pinned in tests) decides `~/`.
            FacetItem::Project(f) if home.is_some_and(|h| h.as_os_str() == f.key.as_str()) => ("~/".to_string(), f.count),
            FacetItem::Project(f) | FacetItem::Model(f) => (f.label.clone(), f.count),
            FacetItem::Named(n) => ("Named only".to_string(), *n),
        };
        let on = past.facet_selected(item);
        let cursor = focused && i == past.facet_cursor;
        // row_line prefix: bar (2) + empty label (1).
        let count_s = super::thousands(count);
        let mark_w = 2;
        let label_w = w.saturating_sub(3 + mark_w + 1 + text_w(&count_s));
        let fg = if on {
            p.value
        } else if count == 0 {
            p.dim
        } else {
            p.text
        };
        let style = if on { Style::default().fg(fg).add_modifier(Modifier::BOLD) } else { Style::default().fg(fg) };
        let spans = vec![
            Span::styled(if on { "\u{25cf} " } else { "  " }.to_string(), Style::default().fg(p.accent)),
            Span::styled(cell(&label, label_w), style),
            Span::styled(format!(" {count_s}"), Style::default().fg(if on { p.value } else { p.dim })),
        ];
        lines.push(row_line(p, area.width, 0, "", spans, cursor, focused));
    }
    let sel = past.facet_cursor;
    crate::tui::modal_kit::render_scrolled(frame, area, lines, (sel.saturating_sub(1), sel + 1));
}

/// First visible line: the stored scroll, nudged so the cursor line is in
/// view if the viewport changed since input last scrolled.
fn first_line(scroll: usize, cursor_line: usize, h: usize, total: usize) -> usize {
    let mut s = scroll;
    if cursor_line >= s + h {
        s = cursor_line + 1 - h;
    }
    if cursor_line < s {
        s = cursor_line;
    }
    s.min(total.saturating_sub(h))
}

fn render_list(frame: &mut Frame, area: Rect, past: &PastState, p: &Palette) {
    let d = &past.derived;
    let h = usize::from(area.height);
    if d.view.rows.is_empty() {
        let hidden = d.view.hidden.total;
        let msg = if past.entries.is_empty() {
            "  no saved sessions".to_string()
        } else if hidden > 0 && !past.show_hidden {
            format!("  no sessions match \u{00b7} h shows {} hidden", super::thousands(hidden))
        } else {
            "  no sessions match".to_string()
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(p.dim)))), area);
        return;
    }
    let clock = past.clock();
    let cur_line = past.cursor_line();
    let first = first_line(past.scroll, cur_line, h, d.lines.len());
    let focused = past.focus == Focus::List && past.rename.is_none();
    let mut lines = Vec::with_capacity(h);
    for line in d.lines.iter().skip(first).take(h) {
        match *line {
            ListLine::Header(g) => {
                let mut s = vec![Span::styled(
                    format!("  {}", d.groups[g].label.to_uppercase()),
                    Style::default().fg(p.dim).add_modifier(Modifier::BOLD),
                )];
                pad_to(&mut s, area.width, p.panel);
                lines.push(Line::from(s));
            }
            ListLine::Row(pos) => {
                let e = &past.entries[d.view.rows[pos]];
                let selected = pos == past.cursor;
                let spans = row_spans(e, past.is_pinned(&e.id), area.width, selected, p, &clock);
                lines.push(row_line(p, area.width, 0, "", spans, selected, focused));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn entry_label(e: &CatalogEntry) -> String {
    match e.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("\u{2026}{}", short_id(&e.id)),
    }
}

fn entry_title(e: &CatalogEntry) -> String {
    if !e.title.trim().is_empty() {
        e.title.clone()
    } else if let Some(f) = &e.first_user {
        f.clone()
    } else {
        "(no title)".to_string()
    }
}

fn msgs_text(n: Option<usize>) -> String {
    match n {
        None | Some(0) => String::new(),
        Some(1) => "1 msg".into(),
        Some(n) => format!("{n} msgs"),
    }
}

/// The ⚑ column (as on the Live tab).
const PIN_W: usize = 2;

fn row_spans(
    e: &CatalogEntry,
    pinned: bool,
    width: u16,
    selected: bool,
    p: &Palette,
    clock: &agent_core::session_catalog::Clock,
) -> Vec<Span<'static>> {
    // row_line prefix = 3 cells.
    let avail = usize::from(width).saturating_sub(3);
    let fg = if selected { p.value } else { p.text };
    let dim = Style::default().fg(p.dim);
    let ago = short_dur((clock.now - e.updated_at).num_seconds());
    // Optional columns, kept by priority (age, msgs, cost), shown as
    // msgs · cost · age. The title takes the rest (at least 12 cells).
    let cols = [(ago, 4usize), (msgs_text(e.message_count), 9), (cost_text(e.cost), 7)];
    let mut show = [false; 3];
    let mut used = PIN_W + NAME_W + 1 + 12;
    for (i, (_, w)) in cols.iter().enumerate() {
        if used + 2 + w <= avail {
            used += 2 + w;
            show[i] = true;
        }
    }
    let title_w = avail.saturating_sub(used - 12);
    let mut v = vec![
        Span::styled(if pinned { "\u{2691} " } else { "  " }, Style::default().fg(p.accent)),
        Span::styled(
            cell(&entry_label(e), NAME_W),
            Style::default()
                .fg(fg)
                .add_modifier(if e.name.is_some() { Modifier::BOLD } else { Modifier::empty() }),
        ),
        Span::raw(" "),
        Span::styled(cell(&entry_title(e), title_w), Style::default().fg(fg)),
    ];
    for i in [1usize, 2, 0] {
        if show[i] {
            let (text, w) = &cols[i];
            v.push(Span::styled(format!("  {}", rcell(text, *w)), dim));
        }
    }
    v
}

fn render_hits(frame: &mut Frame, area: Rect, past: &PastState, p: &Palette) {
    let Some(d) = &past.deep else { return };
    let h = usize::from(area.height);
    let dim = Style::default().fg(p.dim);
    if d.hits.is_empty() {
        let msg = if d.running {
            "  searching\u{2026}".to_string()
        } else if d.summary.is_some() || d.cancelled {
            "  no matches in conversation text".to_string()
        } else {
            format!(
                "  type a phrase, enter searches {} sessions (user + assistant text)",
                super::thousands(past.derived.view.rows.len())
            )
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(msg, dim))), area);
        return;
    }
    let first = first_line(d.scroll, d.cursor, h, d.hits.len());
    let focused = past.rename.is_none();
    let mut lines = Vec::with_capacity(h);
    for (i, hit) in d.hits.iter().enumerate().skip(first).take(h) {
        let selected = i == d.cursor;
        let label = past
            .entries
            .iter()
            .find(|e| e.id == hit.id)
            .map(entry_label)
            .unwrap_or_else(|| format!("\u{2026}{}", short_id(&hit.id)));
        lines.push(row_line(p, area.width, 0, "", hit_spans(hit, &label, area.width, selected, p), selected, focused));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn hit_spans(hit: &DeepHit, label: &str, width: u16, selected: bool, p: &Palette) -> Vec<Span<'static>> {
    let avail = usize::from(width).saturating_sub(3);
    let role = match hit.role {
        DeepRole::User => "you",
        DeepRole::Assistant => "ai",
        DeepRole::ToolResult => "tool",
        DeepRole::Host => "host",
    };
    let fg = if selected { p.value } else { p.text };
    let snip_w = avail.saturating_sub(NAME_W + 1 + 5);
    let (before, matched, after) = split_snippet(hit, snip_w);
    vec![
        Span::styled(cell(label, NAME_W), Style::default().fg(fg)),
        Span::raw(" "),
        Span::styled(cell(role, 4), Style::default().fg(p.dim)),
        Span::raw(" "),
        Span::styled(before, Style::default().fg(p.dim)),
        Span::styled(matched, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled(after, Style::default().fg(p.dim)),
    ]
}

/// The snippet cut to `w` cells with the match kept in view (about a third
/// of the way in when the whole snippet does not fit).
fn split_snippet(hit: &DeepHit, w: usize) -> (String, String, String) {
    let s = &hit.snippet;
    let r = hit.matched.clone();
    let (b, m, a) = (&s[..r.start], &s[r.start..r.end], &s[r.end..]);
    if text_w(s) <= w {
        return (b.to_string(), m.to_string(), a.to_string());
    }
    let m = fit(m, w);
    let room = w.saturating_sub(text_w(&m));
    let lead = (room / 3).min(text_w(b));
    let before = if lead == 0 {
        String::new()
    } else if lead >= text_w(b) {
        b.to_string()
    } else {
        tail_fit(b, lead)
    };
    let after = fit(a, room.saturating_sub(text_w(&before)));
    (before, m, after)
}

fn render_preview(frame: &mut Frame, area: Rect, past: &PastState, home: Option<&std::path::Path>, p: &Palette) {
    let w = usize::from(area.width);
    let dim = Style::default().fg(p.dim);
    let text = Style::default().fg(p.text);
    let mut lines = vec![Line::from(Span::styled("\u{2500}".repeat(w), dim))];
    let Some(e) = past.selected_entry() else {
        frame.render_widget(Paragraph::new(lines), area);
        return;
    };
    let clock = past.clock();
    let mut l1 = Vec::new();
    if let Some(n) = &e.name {
        l1.push(n.clone());
    }
    l1.push(e.id.clone());
    l1.push(e.model_label().to_string());
    if e.cwd.is_some() {
        l1.push(crate::tui::sessions::short_cwd(e.cwd.as_deref(), home));
    }
    if e.title.trim().is_empty() {
        l1.push("(no title)".into());
    } else {
        l1.push(format!("\u{201c}{}\u{201d}", e.title));
    }
    lines.push(Line::from(Span::styled(fit(&l1.join(" \u{00b7} "), w), text)));
    let quote = |label: &str, t: &Option<String>| -> Line<'static> {
        let body = match t {
            Some(t) => format!("\u{201c}{t}\u{201d}"),
            None => "\u{2014}".to_string(),
        };
        Line::from(vec![
            Span::styled(label.to_string(), dim),
            Span::styled(fit(&body, w.saturating_sub(text_w(label))), text),
        ])
    };
    lines.push(quote("first: ", &e.first_user));
    lines.push(quote("last:  ", &e.last_user));
    let mut l4 = Vec::new();
    let msgs = msgs_text(e.message_count);
    if !msgs.is_empty() {
        l4.push(msgs);
    }
    let cost = cost_text(e.cost);
    if !cost.is_empty() {
        l4.push(cost);
    }
    l4.push(span_text(e.span().num_seconds()));
    let (c, u) = (clock.local(e.created_at), clock.local(e.updated_at));
    let created = c.format("%b %-d %H:%M").to_string();
    let updated = if c.date() == u.date() { u.format("%H:%M").to_string() } else { u.format("%b %-d %H:%M").to_string() };
    l4.push(format!("{created} \u{2192} {updated}"));
    if let Some(t) = e.turns {
        l4.push(format!("{t} turns"));
    }
    lines.push(Line::from(Span::styled(fit(&l4.join(" \u{00b7} "), w), dim)));
    frame.render_widget(Paragraph::new(lines), area);
}

/// `2h14m`, `45m`, `3d4h`, `30s`.
pub(crate) fn span_text(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86_400, (s % 86_400) / 3600)
    }
}

fn render_rename(frame: &mut Frame, area: Rect, past: &PastState, p: &Palette) {
    let Some(r) = &past.rename else { return };
    let w = 56u16.min(area.width.saturating_sub(4)).max(20);
    let centre = crate::tui::modal_kit::centered(area, w + 4, 7.min(area.height) + 2);
    let inner = open_popup(frame, centre, w, 7, p);
    let iw = usize::from(inner.width);
    let bg = p.popup;
    let line = |mut spans: Vec<Span<'static>>| {
        for s in &mut spans {
            s.style = s.style.bg(bg);
        }
        pad_to(&mut spans, inner.width, bg);
        Line::from(spans)
    };
    let label = past.entries.iter().find(|e| e.id == r.id).map(entry_label).unwrap_or_else(|| r.id.clone());
    let mut lines = vec![
        line(vec![Span::styled(
            fit(&format!("rename {label}"), iw),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )]),
        line(vec![Span::styled(fit("a-z 0-9 -, up to 40 \u{00b7} empty clears the name", iw), Style::default().fg(p.dim))]),
        line(Vec::new()),
        line(editor_spans(p, &tail_fit(&r.buffer, iw.saturating_sub(2)), None)),
    ];
    let status = if r.pending {
        Span::styled("renaming\u{2026}".to_string(), Style::default().fg(p.dim))
    } else if let Some(e) = &r.error {
        Span::styled(fit(e, iw), Style::default().fg(p.error))
    } else {
        Span::raw(String::new())
    };
    lines.push(line(vec![status]));
    frame.render_widget(Paragraph::new(lines), inner);
}
