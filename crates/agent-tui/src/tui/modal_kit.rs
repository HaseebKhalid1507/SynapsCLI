//! Modal kit — the borderless modal language shared by Settings, Plugins,
//! Models and Help, after Noodle (github.com/wilfredinni/noodle:
//! `src/ui/overlays/Overlay.tsx`, `src/ui/settings/SettingsView.tsx`).
//!
//! - The screen behind a modal dims toward black; the modal is a flat chrome
//!   surface with no frame ([`open_modal`]).
//! - Selection is a raised surface plus a heavy `┃` bar, accent in the focused
//!   pane and muted in the other ([`row_line`]).
//! - Popups (pickers, editors, details) are a brighter surface, no frame.
//! - Hints: key bright, word dim, segments set apart by space ([`hint_line`]).

use super::theme::{ModalKind, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

/// How far the selection surface stands off the chrome.
const SELECTED_STEP: f64 = 1.18;
/// How far popups (pickers, custom editors) stand off the chrome.
const POPUP_STEP: f64 = 1.32;
/// Share of the way each colour behind the modal moves toward black.
const BACKDROP_DIM: f32 = 0.5;

/// The palette a modal draws with (Noodle's surface/text roles).
pub(crate) struct Palette {
    pub(crate) panel: Color,
    pub(crate) selected: Color,
    pub(crate) popup: Color,
    pub(crate) accent: Color,
    pub(crate) title: Color,
    pub(crate) text: Color,
    pub(crate) value: Color,
    pub(crate) dim: Color,
    pub(crate) error: Color,
}

impl Palette {
    /// The palette for a modal. `kind` selects its P19.1 per-part overrides
    /// (`<modal>.border` is the accent — there is no border — and
    /// `<modal>.title` the title); `None` uses the base tokens.
    pub(crate) fn for_modal(t: &Theme, kind: Option<ModalKind>) -> Self {
        Self {
            panel: t.bg,
            selected: t.raised_surface(SELECTED_STEP),
            popup: t.raised_surface(POPUP_STEP),
            accent: kind.map_or(t.border_active, |k| t.modal_border(k)),
            title: kind
                .and_then(|k| t.modal_title(k))
                .unwrap_or(t.claude_label),
            text: t.claude_text,
            value: t.claude_label,
            dim: t.chrome_dim(),
            error: t.error_color,
        }
    }
}

/// Darken every cell of `area` outside `keep` toward black: the modal's
/// backdrop, so the modal reads as in front without a frame around it.
pub(crate) fn dim_backdrop(buf: &mut Buffer, area: Rect, keep: Rect) {
    let dim = |c: Color| match c {
        Color::Rgb(r, g, b) => {
            let f = |v: u8| (f32::from(v) * (1.0 - BACKDROP_DIM)).round() as u8;
            Color::Rgb(f(r), f(g), f(b))
        }
        other => other,
    };
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if keep.contains((x, y).into()) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                let (fg, bg) = (dim(cell.fg), dim(cell.bg));
                cell.set_fg(fg).set_bg(bg);
            }
        }
    }
}

/// Fill `rect` with `bg`.
pub(crate) fn fill(buf: &mut Buffer, rect: Rect, bg: Color) {
    buf.set_style(rect, Style::default().bg(bg));
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
            }
        }
    }
}

/// One selectable row: `┃` bar (selected only), a `label_w`-wide label
/// column, value. Selected rows sit on the raised surface across `width`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn row_line(
    p: &Palette,
    width: u16,
    label_w: usize,
    label: &str,
    value: Vec<Span<'static>>,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let bg = if selected { p.selected } else { p.panel };
    let bar = if selected {
        Span::styled(
            "\u{2503} ",
            Style::default()
                .fg(if focused { p.accent } else { p.dim })
                .bg(bg),
        )
    } else {
        Span::styled("  ", Style::default().bg(bg))
    };
    let label_style = if selected && focused {
        Style::default()
            .fg(p.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(p.text).bg(bg)
    } else {
        Style::default().fg(p.dim).bg(bg)
    };
    let mut spans = vec![
        bar,
        Span::styled(format!("{label:<label_w$} "), label_style),
    ];
    spans.extend(value.into_iter().map(|s| {
        let st = s.style.bg(bg);
        s.style(st)
    }));
    pad_to(&mut spans, width, bg);
    Line::from(spans)
}

/// Right-pad a line's spans with `bg` to `width` cells.
pub(crate) fn pad_to(spans: &mut Vec<Span<'static>>, width: u16, bg: Color) {
    let used: usize = spans
        .iter()
        .map(|s| super::text_metrics::width(&s.content))
        .sum();
    let pad = usize::from(width).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
}

/// A plain value in the value colour (selected) or text colour.
pub(crate) fn value_span(p: &Palette, v: String, selected: bool) -> Vec<Span<'static>> {
    let fg = if selected { p.value } else { p.text };
    vec![Span::styled(v, Style::default().fg(fg))]
}

/// Cycler value while selected: `‹ value ›` with dim arrows.
pub(crate) fn cycler_spans(p: &Palette, v: String) -> Vec<Span<'static>> {
    vec![
        Span::styled("\u{2039} ", Style::default().fg(p.dim)),
        Span::styled(v, Style::default().fg(p.value).add_modifier(Modifier::BOLD)),
        Span::styled(" \u{203a}", Style::default().fg(p.dim)),
    ]
}

/// An inline text editor: the buffer, a block cursor, and an optional error.
pub(crate) fn editor_spans(
    p: &Palette,
    buffer: &str,
    error: Option<&String>,
) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::styled(buffer.to_string(), Style::default().fg(p.value)),
        Span::styled("\u{2588}", Style::default().fg(p.accent)),
    ];
    if let Some(err) = error {
        v.push(Span::styled(
            format!("  {err}"),
            Style::default().fg(p.error),
        ));
    }
    v
}

/// A detail line under the selected row (description, note, error),
/// indented to the label column and wrapped to `width`.
pub(crate) fn detail_lines(text: &str, fg: Color, width: u16, bg: Color) -> Vec<Line<'static>> {
    let indent = "  ";
    let w = usize::from(width).saturating_sub(indent.len() + 1).max(10);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter()
        .map(|l| {
            let mut spans = vec![Span::styled(
                format!("{indent}{l}"),
                Style::default().fg(fg).bg(bg),
            )];
            pad_to(&mut spans, width, bg);
            Line::from(spans)
        })
        .collect()
}

/// Render `lines` into `area`, scrolled so rows `focus.0..focus.1` (the
/// selected row and its details) stay in view.
pub(crate) fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    focus: (usize, usize),
) {
    let h = usize::from(area.height);
    let offset = if focus.1 > h {
        (focus.1 - h).min(focus.0)
    } else {
        0
    };
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

/// Hints in the footer's style: segments separated by two spaces in the
/// source strings; the first word of each is the key (bright), the rest dim.
pub(crate) fn hint_line(p: &Palette, hint: &str, width: u16) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (i, seg) in hint.split("  ").filter(|s| !s.is_empty()).enumerate() {
        let (key, word) = seg.split_once(' ').unwrap_or((seg, ""));
        let w = super::text_metrics::width(seg) + if i > 0 { 3 } else { 0 };
        if used + w > usize::from(width) {
            break; // drop whole segments when narrow
        }
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(p.text)));
        if !word.is_empty() {
            spans.push(Span::styled(format!(" {word}"), Style::default().fg(p.dim)));
        }
        used += w;
    }
    Line::from(spans)
}

/// The category title at the top of the rows pane, and a blank line.
pub(crate) fn section_title(p: &Palette, title: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            title.to_string(),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ]
}

/// A floating surface over a pane (pickers, editors, detail): no border —
/// one step brighter than the selection surface. Offset 2 cells in.
pub(crate) fn popup_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(1);
    let h = height.min(area.height.saturating_sub(1)).max(1);
    Rect {
        x: area.x + 2.min(area.width.saturating_sub(w)),
        y: area.y + 2.min(area.height.saturating_sub(h)),
        width: w,
        height: h,
    }
}

/// Popup rows: selected gets the accent bar and bright text.
pub(crate) fn popup_lines<'a>(
    p: &Palette,
    labels: impl Iterator<Item = (usize, &'a str, bool)>,
    cursor: usize,
    width: u16,
) -> Vec<Line<'static>> {
    labels
        .map(|(i, label, selectable)| {
            let selected = i == cursor;
            let fg = if !selectable {
                p.dim
            } else if selected {
                p.value
            } else {
                p.text
            };
            let mut spans = vec![
                Span::styled(
                    if selected { "\u{2503} " } else { "  " },
                    Style::default().fg(p.accent).bg(p.popup),
                ),
                Span::styled(label.to_string(), Style::default().fg(fg).bg(p.popup)),
            ];
            pad_to(&mut spans, width, p.popup);
            Line::from(spans)
        })
        .collect()
}

/// Centre a `w`×`h` modal in `area`.
pub(crate) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(area.width), h.min(area.height));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Open a modal at `modal`: dim everything else in `area`, clear and fill
/// the modal with the panel surface, draw its header (`title` left, a hint
/// such as `"esc close"` right) and return the padded body (2 columns, 1 row;
/// the header and a blank row taken off the top).
pub(crate) fn open_modal(
    frame: &mut Frame,
    area: Rect,
    modal: Rect,
    p: &Palette,
    title: &str,
    right_hint: &str,
) -> Rect {
    dim_backdrop(frame.buffer_mut(), area, modal);
    frame.render_widget(Clear, modal);
    fill(frame.buffer_mut(), modal, p.panel);
    let inner = Rect {
        x: modal.x + 2,
        y: modal.y + 1,
        width: modal.width.saturating_sub(4),
        height: modal.height.saturating_sub(2),
    };
    if inner.height == 0 || inner.width == 0 {
        return inner;
    }
    let header = Rect { height: 1, ..inner };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            title.to_string(),
            Style::default().fg(p.title).add_modifier(Modifier::BOLD),
        )])),
        header,
    );
    if !right_hint.is_empty() {
        frame.render_widget(
            Paragraph::new(hint_line(p, right_hint, header.width))
                .alignment(ratatui::layout::Alignment::Right),
            header,
        );
    }
    Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    }
}

/// Open a popup over `area`: clear, fill with the popup surface, and return
/// the inner rect (1 cell of padding on every side).
pub(crate) fn open_popup(frame: &mut Frame, area: Rect, w: u16, h: u16, p: &Palette) -> Rect {
    let rect = popup_rect(area, w, h);
    frame.render_widget(Clear, rect);
    fill(frame.buffer_mut(), rect, p.popup);
    Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}
