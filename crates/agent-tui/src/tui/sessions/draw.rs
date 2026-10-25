//! Drawing for the `/sessions` modal: Settings' look (modal_kit), its own
//! layout — tabs, the Live list (LIVE / PARKED sections), a detail strip for
//! the selection, an inline notice line and the hint footer.

use super::super::modal_kit::{centered, hint_line, open_modal, open_popup, pad_to, row_line, Palette};
use super::super::text_metrics::width as text_w;
use super::super::theme::{ModalKind, Theme, THEME};
use super::{LiveSource, RowState, SessionsState, Tab};
use agent_engine::session::SessionMeta;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Truncate `s` to `w` display cells, ending in `…` when cut.
pub(crate) fn fit(s: &str, w: usize) -> String {
    if text_w(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = text_w(ch.encode_utf8(&mut [0; 4]));
        if used + cw + 1 > w {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('\u{2026}');
    out
}

/// Word-wrap `s` into at most two `w`-cell lines (the second fitted with `…`).
fn wrap2(s: &str, w: usize) -> Vec<String> {
    if text_w(s) <= w || w < 10 {
        return vec![fit(s, w)];
    }
    let mut first = String::new();
    let mut rest = s;
    for (i, _) in s.match_indices(' ') {
        if text_w(&s[..i]) > w {
            break;
        }
        first = s[..i].to_string();
        rest = &s[i + 1..];
    }
    if first.is_empty() {
        return vec![fit(s, w)];
    }
    vec![first, fit(rest, w)]
}

/// `s` fitted then right-padded to exactly `w` cells.
fn cell(s: &str, w: usize) -> String {
    let f = fit(s, w);
    let pad = w.saturating_sub(text_w(&f));
    format!("{f}{}", " ".repeat(pad))
}

fn rcell(s: &str, w: usize) -> String {
    let f = fit(s, w);
    let pad = w.saturating_sub(text_w(&f));
    format!("{}{f}", " ".repeat(pad))
}

/// Which optional columns fit a Live row of `avail` cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Columns {
    pub(crate) state: usize,
    pub(crate) clients: bool,
    pub(crate) cost: bool,
    pub(crate) model: bool,
    pub(crate) id: bool,
    /// cwd column width (0 = hidden).
    pub(crate) cwd: usize,
}

const GLYPH_W: usize = 2;
const PIN_W: usize = 2;
const NAME_W: usize = 12;
const ID_W: usize = 4;
const STATE_W: usize = 28;
const STATE_MIN: usize = 16;
const CLIENTS_W: usize = 8;
const MODEL_W: usize = 10;
const COST_W: usize = 7;
const CWD_MIN: usize = 6;
const GAP: usize = 2;

impl Columns {
    pub(crate) fn for_width(avail: usize) -> Self {
        // Always: glyph, pin, name, state (shrinkable to STATE_MIN).
        let base = GLYPH_W + PIN_W + NAME_W + GAP;
        let mut c = Columns { state: STATE_W, clients: false, cost: false, model: false, id: false, cwd: 0 };
        let mut used = base + STATE_W;
        if used > avail {
            c.state = avail.saturating_sub(base).max(STATE_MIN.min(avail.saturating_sub(base)));
            return c;
        }
        // Optional, in priority order.
        for (w, flag) in [
            (CLIENTS_W, 0u8),
            (COST_W, 1),
            (MODEL_W, 2),
            (ID_W, 3),
        ] {
            if used + GAP + w <= avail {
                used += GAP + w;
                match flag {
                    0 => c.clients = true,
                    1 => c.cost = true,
                    2 => c.model = true,
                    _ => c.id = true,
                }
            }
        }
        if used + GAP + CWD_MIN <= avail {
            c.cwd = avail - used - GAP;
        }
        c
    }
}

pub(crate) fn render(frame: &mut Frame, area: Rect, st: &SessionsState) {
    render_with(frame, area, st, &THEME.load());
}

/// [`render`] against an explicit theme (tests sweep the built-ins without
/// touching the process-global one).
pub(crate) fn render_with(frame: &mut Frame, area: Rect, st: &SessionsState, theme: &Theme) {
    let w = (area.width.saturating_mul(9) / 10).max(60).min(area.width);
    let h = (area.height.saturating_mul(8) / 10).max(18).min(area.height);
    let p = Palette::for_modal(theme, Some(ModalKind::Sessions));
    let summary = summary(st);
    let body = open_modal(frame, area, centered(area, w, h), &p, "Sessions", &summary);
    if body.height < 4 || body.width < 20 {
        return;
    }
    let row = |y: u16| Rect { y, height: 1, ..body };
    frame.render_widget(Paragraph::new(tabs_line(st, &p, body.width)), row(body.y));
    let footer = row(body.bottom() - 1);
    // A long notice (a daemon refusal) wraps to a second row, taken from the list.
    let bw = usize::from(body.width);
    let notice_lines = st.notice.as_ref().map(|n| wrap2(&n.text, bw)).unwrap_or_default();
    let notice_h = notice_lines.len().max(1) as u16;
    let notice_area = Rect { y: body.bottom() - 1 - notice_h, height: notice_h, ..body };
    // List | separator + 3-line detail strip (Live only, when there's room).
    let list_top = body.y + 2;
    let list_bottom = body.bottom().saturating_sub(1 + notice_h).max(list_top);
    let show_detail = st.tab == Tab::Live && list_bottom.saturating_sub(list_top) >= 8;
    let detail_h = if show_detail { 4 } else { 0 };
    let list = Rect {
        y: list_top,
        height: list_bottom.saturating_sub(list_top).saturating_sub(detail_h),
        ..body
    };
    match st.tab {
        Tab::Live => {
            render_live(frame, list, st, &p);
            if show_detail {
                let strip = Rect { y: list.bottom(), height: detail_h, ..body };
                render_detail(frame, strip, st, &p);
            }
        }
        Tab::Past => super::past::render(frame, list, &st.past, st.now, &p),
    }
    if let Some(n) = &st.notice {
        let fg = if n.error { p.error } else { p.value };
        let lines: Vec<Line<'static>> = notice_lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(fg))))
            .collect();
        frame.render_widget(Paragraph::new(lines), notice_area);
    } else if let Some(op) = &st.pending {
        let what = match op {
            super::SessionsOp::Pin { on: true, .. } => "pinning\u{2026}".to_string(),
            super::SessionsOp::Pin { on: false, .. } => "unpinning\u{2026}".to_string(),
            super::SessionsOp::Close { label, .. } => format!("closing {label}\u{2026}"),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(what, Style::default().fg(p.dim)))),
            notice_area,
        );
    }
    frame.render_widget(
        Paragraph::new(hint_line(&p, &super::input::hint(st), footer.width)),
        footer,
    );
    if let Some(c) = &st.confirm {
        render_confirm(frame, list, c, &p);
    }
}

/// Header right side: `2 streaming  1 waiting  1 idle  1 parked`.
fn summary(st: &SessionsState) -> String {
    let (s, wt, i, pk) = st.counts();
    let mut parts = Vec::new();
    for (n, what) in [(s, "streaming"), (wt, "waiting"), (i, "idle"), (pk, "parked")] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    parts.join("  ")
}

fn tabs_line(st: &SessionsState, p: &Palette, width: u16) -> Line<'static> {
    let live = format!("Live {}", st.live.len());
    let past = match st.past.count() {
        Some(n) => format!("Past {n}"),
        None => "Past \u{2026}".to_string(),
    };
    let tab = |label: String, active: bool| -> Vec<Span<'static>> {
        if active {
            vec![
                Span::styled("\u{2503}", Style::default().fg(p.accent).bg(p.selected)),
                Span::styled(
                    format!("{label} "),
                    Style::default().fg(p.text).bg(p.selected).add_modifier(Modifier::BOLD),
                ),
            ]
        } else {
            vec![Span::styled(format!(" {label} "), Style::default().fg(p.dim))]
        }
    };
    let mut spans = tab(live, st.tab == Tab::Live);
    spans.push(Span::raw("  "));
    spans.extend(tab(past, st.tab == Tab::Past));
    pad_to(&mut spans, width, p.panel);
    Line::from(spans)
}

fn section(p: &Palette, title: &str, width: u16) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {title}"),
        Style::default().fg(p.dim).add_modifier(Modifier::BOLD),
    )];
    pad_to(&mut spans, width, p.panel);
    Line::from(spans)
}

fn render_live(frame: &mut Frame, area: Rect, st: &SessionsState, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(p.dim);
    match &st.source {
        LiveSource::Daemon => {}
        LiveSource::Loading => lines.push(Line::from(Span::styled("  asking the daemon\u{2026}", dim))),
        LiveSource::InProcess => lines.push(Line::from(Span::styled(
            fit("  no daemon (in-process) \u{2014} only this session is listed", usize::from(area.width)),
            dim,
        ))),
        LiveSource::DaemonDown(e) => lines.push(Line::from(Span::styled(
            fit(
                &format!("  daemon unreachable \u{2014} only this session is listed ({e})"),
                usize::from(area.width),
            ),
            dim,
        ))),
    }
    // row_line: bar(2) + "{label:<0} " (1) + value.
    let avail = usize::from(area.width).saturating_sub(3);
    let cols = Columns::for_width(avail);
    let sel = st.selected_index();
    let mut sel_line = 0;
    let mut in_parked = None;
    for (i, m) in st.live.iter().enumerate() {
        let parked = RowState::of(m).is_parked();
        if in_parked != Some(parked) {
            lines.push(section(p, if parked { "PARKED" } else { "LIVE" }, area.width));
            in_parked = Some(parked);
        }
        let selected = sel == Some(i);
        if selected {
            sel_line = lines.len();
        }
        lines.push(row_line(p, area.width, 0, "", live_spans(st, m, &cols, p, selected), selected, true));
    }
    super::super::modal_kit::render_scrolled(frame, area, lines, (sel_line.saturating_sub(1), sel_line + 1));
}

fn live_spans(st: &SessionsState, m: &SessionMeta, cols: &Columns, p: &Palette, selected: bool) -> Vec<Span<'static>> {
    let state = RowState::of(m);
    let own = st.is_own(m);
    let (glyph, gfg) = match state {
        RowState::Streaming => ("\u{25cf}", p.accent),
        RowState::Waiting => ("\u{25c9}", p.value),
        RowState::Idle => ("\u{25cb}", p.text),
        RowState::Transient => ("\u{25cc}", p.dim),
        RowState::Parked => ("\u{25cc}", p.dim),
    };
    let text_fg = if selected { p.value } else if state.is_parked() { p.dim } else { p.text };
    let dim = Style::default().fg(p.dim);
    let gap = " ".repeat(GAP);
    let mut v = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(gfg)),
        Span::styled(
            if m.pinned { "\u{2691} ".to_string() } else { "  ".to_string() },
            Style::default().fg(p.accent),
        ),
        Span::styled(
            cell(&super::label(m), NAME_W),
            Style::default().fg(text_fg).add_modifier(if own { Modifier::BOLD } else { Modifier::empty() }),
        ),
    ];
    if cols.id {
        // Unnamed rows already show the short id as their label.
        let named = m.name.as_deref().is_some_and(|n| !n.is_empty());
        let id = if named { super::short_id(&m.id.0) } else { "" };
        v.push(Span::styled(format!("{gap}{}", cell(id, ID_W)), dim));
    }
    let state_fg = match state {
        RowState::Streaming => p.accent,
        RowState::Waiting => p.value,
        _ => if selected { p.text } else { p.dim },
    };
    v.push(Span::styled(
        format!("{gap}{}", cell(&super::state_text(m, st.now), cols.state)),
        Style::default().fg(state_fg),
    ));
    if cols.clients {
        let fg = if own { p.text } else { p.dim };
        v.push(Span::styled(format!("{gap}{}", cell(&super::clients_text(m, own), CLIENTS_W)), Style::default().fg(fg)));
    }
    if cols.model {
        v.push(Span::styled(format!("{gap}{}", cell(&super::short_model(&m.model), MODEL_W)), dim));
    }
    if cols.cost {
        v.push(Span::styled(format!("{gap}{}", rcell(&super::cost_text(m.cost_usd), COST_W)), Style::default().fg(text_fg)));
    }
    if cols.cwd > 0 {
        v.push(Span::styled(
            format!("{gap}{}", cell(&super::short_cwd(m.cwd.as_deref(), st.home.as_deref()), cols.cwd)),
            dim,
        ));
    }
    v
}

fn render_detail(frame: &mut Frame, area: Rect, st: &SessionsState, p: &Palette) {
    let w = usize::from(area.width);
    let dim = Style::default().fg(p.dim);
    let text = Style::default().fg(p.text);
    let mut lines = vec![Line::from(Span::styled("\u{2500}".repeat(w), dim))];
    if let Some(m) = st.selected_meta() {
        let own = st.is_own(m);
        let created = m.created_at.with_timezone(&chrono::Local).format("%b %d %H:%M");
        let cwd = super::short_cwd(m.cwd.as_deref(), st.home.as_deref());
        let mut l1 = vec![super::label(m), m.id.0.clone(), super::short_model(&m.model)];
        if !cwd.is_empty() {
            l1.push(cwd);
        }
        l1.push(format!("created {created}"));
        lines.push(Line::from(Span::styled(fit(&l1.join(" \u{00b7} "), w), text)));

        let mut l2 = match m.title.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => format!("\u{201c}{t}\u{201d}"),
            None => "(no title yet)".to_string(),
        };
        if !m.journal_id.is_empty() && m.journal_id != m.id.0 {
            l2.push_str(&format!("  \u{00b7} journal {}", m.journal_id));
        }
        lines.push(Line::from(Span::styled(fit(&l2, w), dim)));

        let clients = match (own, m.clients) {
            (false, 0) => "none".to_string(),
            _ => super::clients_text(m, own),
        };
        let mut l3 = vec![format!("clients: {clients}")];
        let cost = super::cost_text(m.cost_usd);
        if !cost.is_empty() {
            l3.push(cost);
        }
        match m.messages {
            0 => {}
            1 => l3.push("1 msg".into()),
            n => l3.push(format!("{n} msgs")),
        }
        l3.push(if m.pinned { "pinned".into() } else { "not pinned".into() });
        if let Some(lock) = &m.locked_by {
            l3.push(format!("journal locked by {lock}"));
        }
        if own {
            l3.push("this TUI".into());
        }
        lines.push(Line::from(Span::styled(fit(&l3.join(" \u{00b7} "), w), dim)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_confirm(frame: &mut Frame, area: Rect, c: &super::ConfirmClose, p: &Palette) {
    let w = 60u16.min(area.width.saturating_sub(4)).max(20);
    // Centred in the list (open_popup's fixed corner offset would cover the
    // very row being closed).
    let centre = centered(area, w + 4, 6.min(area.height) + 2);
    let inner = open_popup(frame, centre, w, 6, p);
    let iw = usize::from(inner.width);
    let why = if c.clients == 0 {
        "it saves and ends; continue it later from Past".to_string()
    } else if super::CLOSE_ATTACHED {
        if c.clients == 1 {
            "its TUI client will exit (1 attached)".to_string()
        } else {
            format!("its TUI clients will exit ({} attached)", c.clients)
        }
    } else {
        format!(
            "{} client{} attached \u{2014} the daemon refuses until {} detach{}",
            c.clients,
            if c.clients == 1 { "" } else { "s" },
            if c.clients == 1 { "it" } else { "they" },
            if c.clients == 1 { "es" } else { "" },
        )
    };
    let bg = p.popup;
    let mut l1 = vec![Span::styled(
        fit(&format!("close {}?", c.label), iw),
        Style::default().fg(p.text).bg(bg).add_modifier(Modifier::BOLD),
    )];
    pad_to(&mut l1, inner.width, bg);
    let mut l2 = vec![Span::styled(fit(&why, iw), Style::default().fg(p.dim).bg(bg))];
    pad_to(&mut l2, inner.width, bg);
    let mut l3: Vec<Span<'static>> = Vec::new();
    pad_to(&mut l3, inner.width, bg);
    let mut l4 = hint_line(p, "y close  n cancel", inner.width).spans;
    for s in &mut l4 {
        s.style = s.style.bg(bg);
    }
    pad_to(&mut l4, inner.width, bg);
    frame.render_widget(
        Paragraph::new(vec![Line::from(l1), Line::from(l2), Line::from(l3), Line::from(l4)]),
        inner,
    );
}
