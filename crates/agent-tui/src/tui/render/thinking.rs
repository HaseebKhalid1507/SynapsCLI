//! The thinking block: a light open rail at the message margin.
//!
//! ```text
//!    ╭─ ◇ thought for 12s
//!    │  Understanding the bug
//!    │  The user says the line cache drops the last row when the viewport
//!    │  is resized mid-stream. Let me reason about where the height gets
//!    │  computed: `sync_cache` rebuilds on a width change, but …   (faded)
//!    ╰─ +9 lines · ctrl+o to expand
//! ```
//!
//! No panel, no fill: the rail sits on the canvas at the margin column the
//! agent's `◈` and the user's `❯` use, and the text hangs two cells off it,
//! under the header's `◇` (the answer's `◈` before it fills in). Three
//! tones of `thinking_color`, each a step up (`Theme::thinking_colors`):
//! the rail glyphs, the meta text, the body — all below the answer, all
//! measured so they read on every palette, paper included.
//!
//! The body is the agent's own markdown renderer (`render_markdown_spans`,
//! so the copy provenance is the agent text's), recoloured into that
//! hierarchy: prose in the ink, headings and bold a step brighter, bullets
//! in the meta tone, code spans on a faint pill, fenced code without its
//! island.
//!
//! - **Live** (the placeholder, and a streamed block until the model moves
//!   on): `thinking` shimmers with the header block's sweep, the clock
//!   beside it; the rail stays open (no `╰`) and the body follows the
//!   newest rows, the oldest one faded. Only the tail of the source is
//!   rendered, so a long thought costs the same per delta as a short one.
//! - **Done**: "thought for 12s" (or a word count when the block has no
//!   timing, e.g. rebuilt from history); the first rows, the last faded
//!   into a `╰─ +N lines` footer, or the whole thought closed by `╰` on its
//!   last row. Ctrl+O (the transcript's compact/full toggle) shows it all.

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::super::header::elapsed;
use super::super::highlight::clamp_line;
use super::super::markdown::render_markdown_spans;
use super::super::text_metrics::width as display_width;
use super::super::theme::{lift_to_contrast, mix, Theme, THEME};
use super::super::transcript::{LineMeta, RenderCtx, ThoughtClock, THINKING_PLACEHOLDER};
use super::LineSink;

/// Body rows a collapsed thought shows (a live one shows one more, so the
/// block keeps its height when the thought ends and the footer appears).
pub(super) const PREVIEW_ROWS: usize = 6;
/// A body at most this many rows past the preview shows whole: a "+1 lines"
/// footer would cost the very row it saves.
pub(super) const PREVIEW_SLACK: usize = 2;
/// While live, only this many trailing source lines render — enough to fill
/// the preview however short the lines — so a delta's cost does not grow
/// with the thought.
const TAIL_SOURCE_LINES: usize = 24;
/// Widest text measure in cells (indent included): past it, prose on a wide
/// terminal stops reading as lines.
pub(super) const MAX_MEASURE: usize = 110;
/// Margin before the rail: the agent's `◈` column.
const MARGIN: &str = "   ";
/// What `render_markdown_spans` gets as its prefix: the margin plus the
/// rail's cell. It indents content two more cells, so the text lands two
/// cells off the rail; the rail glyph then takes the prefix's last cell.
const MD_PREFIX: &str = "    ";
const ICON: &str = "\u{25c7}"; // ◇
/// The live label's sweep: cells per animation frame, and the off-label
/// run (cells) between passes.
const SWEEP_SPEED: f32 = 0.2;
const SWEEP_REST: f32 = 8.0;

/// The block's colours, resolved once per render.
struct Palette {
    canvas: Color,
    ink: Color,
    strong: Color,
    meta: Color,
    rail: Color,
    pill: Color,
    pill_ink: Color,
    // The agent-text colours the markdown renderer emits, to recolour.
    text: Color,
    bullet: Color,
    code_fg: Color,
    code_bg: Color,
    border: Color,
    muted: Color,
}

impl Palette {
    fn new(t: &Theme) -> Self {
        let c = t.thinking_colors();
        // A pill that only just stands off the canvas, its text kept at AA.
        let pill = lift_to_contrast(c.canvas, t.claude_text, c.canvas, 1.18);
        Self {
            canvas: c.canvas,
            ink: c.ink,
            strong: mix(c.ink, t.claude_text, 0.45),
            meta: c.meta,
            rail: c.rail,
            pill,
            pill_ink: lift_to_contrast(mix(t.code_fg, c.ink, 0.6), t.claude_text, pill, 4.5),
            text: t.claude_text,
            bullet: t.list_bullet_color,
            code_fg: t.code_fg,
            code_bg: t.code_bg,
            border: t.border,
            muted: t.muted,
        }
    }

    /// One span of agent-styled markdown, in the block's hierarchy.
    fn recolour(&self, style: &mut Style) {
        let Some(fg) = style.fg else {
            return;
        };
        if style.bg == Some(self.code_bg) {
            if fg == self.code_fg {
                // Inline code: a faint pill instead of the agent's (on light
                // palettes, dark) island.
                style.fg = Some(self.pill_ink);
                style.bg = Some(self.pill);
            } else {
                // Fenced code: no island; its syntax colours ease into the
                // ink.
                style.fg = Some(mix(fg, self.ink, 0.5));
                style.bg = None;
            }
        } else if style.add_modifier.contains(Modifier::BOLD) {
            style.fg = Some(self.strong);
        } else if fg == self.text {
            style.fg = Some(self.ink);
        } else if fg == self.bullet {
            style.fg = Some(self.meta);
        } else if fg == self.border || fg == self.muted {
            // Fence rules, the code gutter, the language chip: structure.
            style.fg = Some(self.rail);
        } else {
            style.fg = Some(mix(fg, self.ink, 0.5));
        }
    }

    /// A row fading into the canvas: the preview's edge row (before it
    /// goes on the rail, so the rail glyph keeps its tone).
    fn fade(&self, line: &mut Line<'static>) {
        for span in line.spans.iter_mut() {
            if let Some(fg) = span.style.fg {
                let ground = span.style.bg.unwrap_or(self.canvas);
                span.style.fg = Some(mix(fg, ground, 0.55));
            }
        }
    }

    fn rail(&self) -> Style {
        Style::default().fg(self.rail)
    }
}

/// Whether a rendered row has no visible text.
fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// `line` (a markdown row, prefixed by [`MD_PREFIX`]) on the rail: its
/// first four cells — always spaces — become the margin and `glyph`. The
/// display width, and so every content column, is unchanged.
fn on_rail(line: Line<'static>, glyph: &'static str, rail: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    spans.push(Span::raw(MARGIN));
    spans.push(Span::styled(glyph, rail));
    let mut strip = MD_PREFIX.len();
    for span in line.spans {
        if strip > 0 {
            let lead = span.content.len() - span.content.trim_start_matches(' ').len();
            let cut = lead.min(strip);
            strip -= cut;
            if cut == span.content.len() {
                continue;
            }
            spans.push(Span::styled(span.content[cut..].to_string(), span.style));
        } else {
            spans.push(span);
        }
    }
    Line::from(spans)
}

/// Byte offset where the last `n` source lines of `text` start, moved back
/// to an opening code fence when that offset falls inside a fenced block
/// (so the slice parses like the whole), and how many source lines precede
/// it.
fn tail_start(text: &str, n: usize) -> (usize, usize) {
    let total = text.lines().count();
    if total <= n {
        return (0, 0);
    }
    let base = text.as_ptr() as usize;
    let mut skip = total - n;
    let mut fence_open: Option<usize> = None;
    for (i, line) in text.lines().enumerate().take(skip) {
        if line.trim_start().starts_with("```") {
            fence_open = match fence_open {
                Some(_) => None,
                None => Some(i),
            };
        }
    }
    if let Some(open) = fence_open {
        skip = open;
    }
    let start = text
        .lines()
        .nth(skip)
        .map_or(text.len(), |l| l.as_ptr() as usize - base);
    (start, skip)
}

/// The source line a [`LineMeta`] row presents, if any.
fn src_line_of(meta: &LineMeta, text: &str) -> Option<usize> {
    match meta {
        LineMeta::Chrome => None,
        LineMeta::ContentLine { src_line, .. } => Some(*src_line),
        LineMeta::Content { range, .. } => {
            Some(text[..range.start.min(text.len())].matches('\n').count())
        }
    }
}

/// What a collapsed preview left out: `hidden` non-blank rows, from source
/// line `src_line` on.
struct Cut {
    hidden: usize,
    src_line: usize,
}

/// The body rows (not yet on the rail): the agent markdown renderer over
/// `text` (or, while live and collapsed, its tail), recoloured, at most one
/// blank row in a row and none at either end, with provenance into the
/// whole `text`; trimmed to the preview unless `expanded`.
fn body_rows(
    text: &str,
    live: bool,
    expanded: bool,
    msg_idx: usize,
    measure: usize,
    pal: &Palette,
) -> (Vec<(Line<'static>, LineMeta)>, Option<Cut>) {
    let (start, lines_before) = if live && !expanded {
        tail_start(text, TAIL_SOURCE_LINES)
    } else {
        (0, 0)
    };
    let mut rows: Vec<(Line<'static>, LineMeta)> = Vec::new();
    for (mut line, meta) in render_markdown_spans(&text[start..], MD_PREFIX, measure, msg_idx) {
        for span in &mut line.spans {
            pal.recolour(&mut span.style);
        }
        let meta = match meta {
            LineMeta::Content { range, content_col } => LineMeta::Content {
                range: range.start + start..range.end + start,
                content_col,
            },
            LineMeta::ContentLine { msg_idx, src_line } => LineMeta::ContentLine {
                msg_idx,
                src_line: src_line + lines_before,
            },
            LineMeta::Chrome => LineMeta::Chrome,
        };
        // One blank row of air at most (a heading's injected spacer plus the
        // source's own blank line would be two), none at the top. Of a run,
        // the row kept is a source line's (ContentLine) over a Chrome spacer.
        if is_blank(&line) {
            match rows.last_mut() {
                None => continue,
                Some((prev, prev_meta)) if is_blank(prev) => {
                    if matches!(prev_meta, LineMeta::Chrome) {
                        *prev_meta = meta;
                    }
                    continue;
                }
                _ => {}
            }
        }
        rows.push((line, meta));
    }
    while rows.last().is_some_and(|(l, _)| is_blank(l)) {
        rows.pop();
    }
    if expanded {
        return (rows, None);
    }
    if live {
        // Follow the newest rows, the oldest fading out at the top.
        let window = PREVIEW_ROWS + 1;
        if rows.len() > window {
            let mut from = rows.len() - window;
            while from + 1 < rows.len() && is_blank(&rows[from].0) {
                from += 1;
            }
            rows.drain(..from);
            if let Some((first, _)) = rows.first_mut() {
                pal.fade(first);
            }
        }
        return (rows, None);
    }
    if rows.len() <= PREVIEW_ROWS + PREVIEW_SLACK {
        return (rows, None);
    }
    // Collapsed: the first rows, the last fading into the footer. The
    // footer stands in for the hidden tail (D4, as the tool cards' "+N
    // lines"): ContentLine at the first hidden source line, so a selection
    // through it copies the whole thought.
    let mut keep = PREVIEW_ROWS;
    while keep > 1 && is_blank(&rows[keep - 1].0) {
        keep -= 1;
    }
    let src_line = rows[keep..]
        .iter()
        .find_map(|(_, meta)| src_line_of(meta, text))
        .unwrap_or_else(|| text.lines().count().saturating_sub(1));
    let hidden = rows[keep..].iter().filter(|(l, _)| !is_blank(l)).count();
    rows.truncate(keep);
    if let Some((last, _)) = rows.last_mut() {
        pal.fade(last);
    }
    (rows, (hidden > 0).then_some(Cut { hidden, src_line }))
}

/// `╰─ +N lines · ctrl+o to expand`, the hint dropped where it doesn't fit.
fn footer(hidden: usize, width: usize, pal: &Palette) -> Line<'static> {
    let count = format!("+{hidden} line{}", if hidden == 1 { "" } else { "s" });
    let meta = Style::default().fg(pal.meta);
    let mut spans = vec![
        Span::raw(MARGIN),
        Span::styled("\u{2570}\u{2500} ", pal.rail()),
        Span::styled(count.clone(), meta),
    ];
    let hint = " \u{00b7} ctrl+o to expand";
    if MARGIN.len() + 3 + display_width(&count) + display_width(hint) <= width {
        spans.push(Span::styled(" \u{00b7} ", meta));
        // The key in the ink, its label in the meta tone (the footer's
        // keybind hints do the same).
        spans.push(Span::styled("ctrl+o", Style::default().fg(pal.ink)));
        spans.push(Span::styled(" to expand", meta));
    }
    Line::from(spans)
}

/// The header: `╭─ ◇` + the label — `thinking` shimmering while live (the
/// clock beside it once the model has said something), "thought for 12s"
/// (or a word count) once done. `╶─` when no body hangs below it.
fn header(
    text: &str,
    thought: Option<ThoughtClock>,
    live: bool,
    open: bool,
    now: Instant,
    ctx: &RenderCtx<'_>,
    pal: &Palette,
) -> Line<'static> {
    let corner = if open { "\u{256d}\u{2500} " } else { "\u{2576}\u{2500} " };
    let meta = Style::default().fg(pal.meta);
    let mut spans = vec![
        Span::raw(MARGIN),
        Span::styled(corner, pal.rail()),
        Span::styled(format!("{ICON} "), meta),
    ];
    let bold = Modifier::BOLD;
    if live {
        // The header block's sweep (header.rs) over the label's letters, on
        // a shorter loop: a soft band of brighter ink keeps passing through
        // (~1.3s a pass at the 60 fps tick).
        let label = "thinking";
        let w = label.len() as f32;
        let period = w + SWEEP_REST;
        let at = (ctx.spinner_frame as f32 * SWEEP_SPEED) % period - SWEEP_REST / 2.0;
        for (x, ch) in label.chars().enumerate() {
            let d = (x as f32 - at) / 2.0;
            let s = (-d * d).exp();
            let fg = mix(pal.ink, pal.strong, 0.2 + 0.8 * f64::from(s));
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(fg).add_modifier(bold),
            ));
        }
        if let Some(clock) = thought {
            let secs = now.saturating_duration_since(clock.first()).as_secs();
            spans.push(Span::styled(format!(" {}", elapsed(secs)), meta));
        }
        return Line::from(spans);
    }
    spans.push(Span::styled(
        "thought",
        Style::default().fg(pal.ink).add_modifier(bold),
    ));
    let detail = match thought.and_then(|c| c.secs()) {
        Some(secs) => format!(" for {}", elapsed(secs.max(1))),
        None => match text.split_whitespace().count() {
            0 => " \u{00b7} empty".to_string(),
            1 => " \u{00b7} 1 word".to_string(),
            n => format!(" \u{00b7} {n} words"),
        },
    };
    spans.push(Span::styled(detail, meta));
    Line::from(spans)
}

/// Push the thinking block for message `msg_idx` (`text`, its `thought`
/// clock) at viewport `width`; `now` is the store's clock (the live
/// header's). `prev_ends_blank`: the message above already ends in a blank
/// row. `expanded`: the transcript's Ctrl+O state.
#[allow(clippy::too_many_arguments)]
pub(super) fn render(
    sink: &mut LineSink,
    text: &str,
    thought: Option<ThoughtClock>,
    msg_idx: usize,
    width: usize,
    now: Instant,
    ctx: &RenderCtx<'_>,
    prev_ends_blank: bool,
    expanded: bool,
) {
    let theme = THEME.load();
    let pal = Palette::new(&theme);
    let placeholder = text == THINKING_PLACEHOLDER;
    let live = placeholder || thought.is_some_and(|c| !c.done);

    // One blank row of air above and below, whatever the neighbours (the
    // agent text after a thought adds none of its own: render.rs).
    if msg_idx > 0 && !prev_ends_blank {
        sink.push(Line::from(""));
    }
    let (rows, cut) = if placeholder || text.trim().is_empty() {
        (Vec::new(), None)
    } else {
        let measure = width.saturating_sub(2).clamp(1, MAX_MEASURE);
        body_rows(text, live, expanded, msg_idx, measure, &pal)
    };
    sink.push(clamp_line(
        header(text, thought, live, !rows.is_empty(), now, ctx, &pal),
        width,
    ));
    let last = rows.len().saturating_sub(1);
    let close = !live && cut.is_none();
    for (k, (line, meta)) in rows.into_iter().enumerate() {
        let glyph = if close && k == last { "\u{2570}" } else { "\u{2502}" };
        let line = clamp_line(on_rail(line, glyph, pal.rail()), width);
        // The rail swapped spaces for a glyph of the same width, so content
        // columns hold; a row the clamp cut no longer carries its source
        // slice, so it falls back to line granularity (lock L1).
        let meta = match meta {
            LineMeta::Content { range, content_col } => {
                let row: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                if row.ends_with(&text[range.clone()]) {
                    LineMeta::Content { range, content_col }
                } else {
                    LineMeta::ContentLine {
                        msg_idx,
                        src_line: text[..range.start].matches('\n').count(),
                    }
                }
            }
            other => other,
        };
        sink.push_meta(line, meta);
    }
    if let Some(cut) = cut {
        sink.push_meta(
            clamp_line(footer(cut.hidden, width, &pal), width),
            LineMeta::ContentLine {
                msg_idx,
                src_line: cut.src_line,
            },
        );
    }
    sink.push(Line::from("")); // bottom margin
}

#[cfg(test)]
mod tests {
    use super::super::super::clock::TuiClock;
    use super::super::super::theme::{wcag_contrast, THEME};
    use super::super::super::transcript::{
        ChatMessage, LineMeta, MsgSlot, RenderCtx, TranscriptStore, THINKING_PLACEHOLDER,
    };
    use super::{MAX_MEASURE, PREVIEW_ROWS, PREVIEW_SLACK};
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    fn ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: true,
            agent_name: "agent",
        }
    }

    fn flat(line: &ratatui::text::Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rows(slot: &MsgSlot) -> Vec<String> {
        slot.lines().iter().map(flat).collect()
    }

    /// A store whose message 0 is a system line (it ends without air, so
    /// the thought at index 1 draws its own).
    fn store(clock: TuiClock) -> TranscriptStore {
        let mut s = TranscriptStore::new(clock);
        s.push_msg(ChatMessage::System("notice".into()));
        s
    }

    /// A finished (untimed) thought — message 1 — rendered at `width`.
    fn done(text: &str, width: usize) -> (TranscriptStore, MsgSlot) {
        let mut s = store(TuiClock::test());
        s.push_msg(ChatMessage::Thinking(text.into()));
        let slot = s.render_message_lines(1, width, &ctx());
        (s, slot)
    }

    /// The `n` source lines of a long markdown thought.
    fn long_thought(n: usize) -> String {
        (0..n)
            .map(|i| format!("Step {i}: weigh the option and note why it matters here."))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rail sits on the canvas: a row of air above and below, no fill
    /// on any cell outside a code pill, and prose held to a readable
    /// measure however wide the pane (render.rs tests the wrap and close).
    #[test]
    fn rail_sits_on_the_canvas_with_air_and_a_measure() {
        let text = format!("{} {}", long_thought(1), "and then some more words".repeat(12));
        for width in [40usize, 100, 200] {
            let (_, slot) = done(&text, width);
            let r = rows(&slot);
            assert_eq!(r.first().map(String::as_str), Some(""), "air above (w={width})");
            assert_eq!(r.last().map(String::as_str), Some(""), "air below (w={width})");
            for line in slot.lines() {
                for span in &line.spans {
                    assert!(span.style.bg.is_none(), "no fill: {span:?}");
                }
            }
            let widest = r.iter().map(|row| UnicodeWidthStr::width(row.as_str())).max();
            assert!(widest.unwrap_or(0) <= MAX_MEASURE.min(width), "w={width}: {r:#?}");
        }
    }

    /// Markdown renders: no literal markers or the old `· -` double
    /// bullets; headings and bold in the brighter ink; code spans on a pill
    /// that is not the agent's code island.
    #[test]
    fn markdown_renders_in_the_rail_hierarchy() {
        let text = "## Plan\n- check `sync_cache` first\n* then **the splice**\n1. ship it";
        let (_, slot) = done(text, 80);
        let all = rows(&slot).join("\n");
        for marker in ["##", "**", "`", "\u{00b7} -", "* "] {
            assert!(!all.contains(marker), "{marker:?} leaked:\n{all}");
        }
        assert!(all.contains("\u{2022} check"), "bullets render:\n{all}");
        assert!(all.contains("1. ship it"), "{all}");
        let t = THEME.load();
        let c = t.thinking_colors();
        let spans: Vec<_> = slot.lines().iter().flat_map(|l| l.spans.iter()).collect();
        let heading = spans.iter().find(|s| s.content.contains("Plan")).expect("heading");
        assert!(heading.style.add_modifier.contains(Modifier::BOLD));
        let code = spans.iter().find(|s| s.content.contains("sync_cache")).expect("pill");
        assert!(code.style.bg.is_some() && code.style.bg != Some(t.code_bg), "{code:?}");
        let prose = spans.iter().find(|s| s.content.contains("check")).expect("prose");
        assert_eq!(prose.style.fg, Some(c.ink), "body in the ink");
        let rail = spans.iter().find(|s| s.content == "\u{2502}").expect("rail");
        assert_eq!(rail.style.fg, Some(c.rail), "rail in its own, quieter tone");
    }

    /// A long thought collapses to the first rows, the last one faded, and
    /// a `╰─ +N lines · ctrl+o to expand` footer; the footer maps to the
    /// first hidden source line (D4: selecting through it copies the whole
    /// thought). One within the slack shows whole. Ctrl+O shows all of it.
    #[test]
    fn collapsed_preview_fades_into_a_footer_and_ctrl_o_expands() {
        let text = long_thought(30);
        let (mut s, slot) = done(&text, 80);
        let r = rows(&slot);
        // air, header, PREVIEW_ROWS body rows, footer, air
        assert_eq!(r.len(), PREVIEW_ROWS + 4, "{r:#?}");
        let footer = &r[r.len() - 2];
        let hidden = 30 - PREVIEW_ROWS;
        assert_eq!(
            footer.trim_end(),
            format!("   \u{2570}\u{2500} +{hidden} lines \u{00b7} ctrl+o to expand"),
        );
        assert_eq!(
            slot.meta_slice()[r.len() - 2],
            LineMeta::ContentLine {
                msg_idx: 1,
                src_line: PREVIEW_ROWS
            },
            "footer anchors the hidden tail"
        );
        let c = THEME.load().thinking_colors();
        let ink_of = |row: usize| {
            slot.lines()[row]
                .spans
                .iter()
                .find(|sp| sp.content.contains("Step"))
                .and_then(|sp| sp.style.fg)
                .expect("text span")
        };
        assert_eq!(ink_of(2), c.ink);
        let faded = ink_of(PREVIEW_ROWS + 1);
        assert!(
            wcag_contrast(faded, c.canvas) < wcag_contrast(c.ink, c.canvas),
            "the edge row fades"
        );
        assert!(!r.iter().any(|row| row.starts_with("   \u{2570} ")), "no close: {r:#?}");

        // Within the slack: whole, closed, no footer.
        let short = long_thought(PREVIEW_ROWS + PREVIEW_SLACK);
        let (_, slot) = done(&short, 80);
        let r = rows(&slot);
        assert!(!r.iter().any(|row| row.contains("ctrl+o")), "{r:#?}");
        assert_eq!(r.len(), PREVIEW_ROWS + PREVIEW_SLACK + 3);

        // Ctrl+O: everything, closed on the last row.
        s.set_show_full_output(true);
        let slot = s.render_message_lines(1, 80, &ctx());
        let r = rows(&slot);
        assert_eq!(r.len(), 30 + 3, "{r:#?}");
        assert!(r[r.len() - 2].starts_with("   \u{2570}  Step 29"), "{r:#?}");
    }

    /// Narrow panes (and degenerate ones) never overrun; the footer drops
    /// its hint before it would wrap.
    #[test]
    fn narrow_panes_never_overrun() {
        let text = format!(
            "## A heading long enough to wrap\n{}\n- a bullet with `code` in it\n\
             a/very/long/path/without/any/break/points/at/all/that/must/hard/break",
            long_thought(12)
        );
        for width in [1usize, 8, 20, 30, 40, 52, 60] {
            let (_, slot) = done(&text, width);
            assert_eq!(slot.lines().len(), slot.meta_slice().len());
            for r in rows(&slot) {
                assert!(UnicodeWidthStr::width(r.as_str()) <= width, "w={width}: {r:?}");
            }
        }
        let (_, slot) = done(&text, 30);
        let footer = rows(&slot).into_iter().find(|r| r.contains("+")).expect("footer");
        assert!(!footer.contains("ctrl+o"), "hint dropped at 30 cols: {footer:?}");
        let (_, slot) = done(&text, 60);
        let footer = rows(&slot).into_iter().find(|r| r.contains(" lines")).expect("footer");
        assert!(footer.contains("ctrl+o to expand"), "{footer:?}");
    }

    /// The placeholder is one quiet `╶─ ◇ thinking` row, all chrome; an
    /// empty or whitespace-only thought says so instead of drawing an empty
    /// rail.
    #[test]
    fn placeholder_and_empty_thoughts_are_a_single_header() {
        let mut s = TranscriptStore::new(TuiClock::test());
        s.push_msg(ChatMessage::User("go".into()));
        s.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
        let slot = s.render_message_lines(1, 80, &ctx());
        let r = rows(&slot);
        assert_eq!(r.len(), 2, "header + air (the user card ends in air): {r:#?}");
        assert_eq!(r[0].trim_end(), "   \u{2576}\u{2500} \u{25c7} thinking");
        assert!(slot.meta_slice().iter().all(|m| *m == LineMeta::Chrome));
        assert!(s.live_slots().contains(&1), "the placeholder animates");

        let (_, slot) = done("  \n\t\n", 80);
        let r = rows(&slot);
        assert_eq!(r[1].trim_end(), "   \u{2576}\u{2500} \u{25c7} thought \u{00b7} empty");
        assert_eq!(r.len(), 3);
    }

    /// Streamed: live while the model thinks (shimmering `thinking`, its
    /// clock, an open rail following the newest rows), then — once the
    /// model moves on — "thought for 12s", the head preview and a closed
    /// rail; it stops being a live slot. History-rebuilt thoughts (no
    /// clock) count words.
    #[test]
    fn streaming_header_then_thought_for() {
        let clock = TuiClock::test();
        let mut s = store(clock.clone());
        s.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
        let lines: Vec<String> = long_thought(20).lines().map(|l| format!("{l}\n")).collect();
        for l in &lines {
            s.append_or_update_thinking(l);
            clock.advance(std::time::Duration::from_millis(600));
        }
        assert_eq!(s.live_slots(), &[1], "a streaming thought is live");
        let live = s.render_message_lines(1, 80, &ctx());
        let r = rows(&live);
        assert!(r[1].contains("\u{25c7} thinking 12s"), "{r:#?}");
        assert!(r.iter().any(|row| row.contains("Step 19")), "follows the tail: {r:#?}");
        assert!(!r.iter().any(|row| row.contains("Step 0:")), "{r:#?}");
        assert!(!r.iter().any(|row| row.starts_with("   \u{2570}")), "rail open: {r:#?}");
        // The shimmer moves with the frame.
        let later = s.render_message_lines(
            1,
            80,
            &RenderCtx {
                spinner_frame: 20,
                ..ctx()
            },
        );
        assert_ne!(live.lines()[1], later.lines()[1], "the label shimmers");
        // The live window is the collapsed preview's height (its footer
        // included): no jump when the thought ends.
        let live_h = live.lines().len();

        s.push_msg(ChatMessage::System("a notice lands".into()));
        s.drop_empty_thinking(); // the model moved on
        assert!(s.live_slots().is_empty(), "sealed: not live");
        let slot = s.render_message_lines(1, 80, &ctx());
        let r = rows(&slot);
        assert!(r[1].contains("\u{25c7} thought for 12s"), "{r:#?}");
        assert!(r.iter().any(|row| row.contains("Step 0:")), "head preview: {r:#?}");
        assert!(r.iter().any(|row| row.contains("ctrl+o")), "{r:#?}");
        assert_eq!(slot.lines().len(), live_h, "same height live and done");

        let (_, slot) = done("one two three", 80);
        assert!(rows(&slot)[1].ends_with("thought \u{00b7} 3 words"));
    }

    /// LineMeta: header, air and footer chrome are Chrome/ContentLine as
    /// specified; plain prose rows are char-precise Content whose range is
    /// the row's suffix starting exactly at `content_col` (the rail swap
    /// keeps columns); a long thought's live tail maps into the WHOLE
    /// source.
    #[test]
    fn line_meta_maps_rows_to_the_whole_source() {
        let text = "plain words here\nsecond plain line with more words to wrap at forty cols";
        for width in [40usize, 80] {
            let (_, slot) = done(text, width);
            let metas = slot.meta_slice();
            assert_eq!(metas[0], LineMeta::Chrome, "air");
            assert_eq!(metas[1], LineMeta::Chrome, "header");
            assert_eq!(*metas.last().unwrap(), LineMeta::Chrome, "air");
            let mut contents = 0;
            for (line, meta) in slot.lines().iter().zip(metas) {
                if let LineMeta::Content { range, content_col } = meta {
                    contents += 1;
                    let row = flat(line);
                    let col = *content_col as usize;
                    let tail: String = row.chars().skip(col).collect();
                    assert_eq!(tail, &text[range.clone()], "w={width}: {row:?}");
                }
            }
            assert!(contents >= 2, "plain prose is Content (w={width})");
        }

        // Live tail: provenance offsets into the whole thought.
        let mut s = store(TuiClock::test());
        s.append_or_update_thinking(&long_thought(60));
        let slot = s.render_message_lines(1, 80, &ctx());
        let src = s.source_text(1).into_owned();
        for (line, meta) in slot.lines().iter().zip(slot.meta_slice()) {
            match meta {
                LineMeta::Content { range, .. } => {
                    assert!(flat(line).ends_with(&src[range.clone()]));
                    assert!(src[range.clone()].starts_with("Step 5"), "tail rows");
                }
                LineMeta::ContentLine { src_line, .. } => assert!(*src_line >= 50),
                LineMeta::Chrome => {}
            }
        }
    }
}
