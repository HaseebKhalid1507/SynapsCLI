//! Message rendering — converts ChatMessage variants into styled ratatui Lines.
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(crate) mod paths;
mod subagent_card;
mod thinking;

use super::app::SPINNER_FRAMES;
use super::draw::{bash_trace, format_tool_name, tool_accent};
use super::highlight::{
    clamp_line, clamp_line_ellipsis, highlight_bash_output, highlight_read_output, highlight_tool_code,
    is_read_tool_output, try_highlight_grep_line,
};
use super::markdown::{render_markdown_spans, wrap_text, wrap_text_spans};
use super::text_metrics::width as display_width;
use super::theme::THEME;
use super::transcript::{ChatMessage, LineMeta, MsgSlot, RenderCtx, TranscriptStore};

/// Replace column 0 of a card row (always a space: transcript rows start
/// with the 3-space margin) with a heavy `┃` in `accent`, flush with the
/// card's left edge. Width and every other column are untouched; a row that
/// doesn't start with a space is returned as is.
fn with_bar(line: Line<'static>, accent: Color, band: Color) -> Line<'static> {
    let mut spans = line.spans;
    let Some(first) = spans.first_mut() else {
        return Line::from(spans);
    };
    let text = first.content.to_string();
    let Some(rest) = text.strip_prefix(' ') else {
        return Line::from(spans);
    };
    *first = Span::styled(rest.to_string(), first.style);
    spans.insert(
        0,
        Span::styled("\u{2503}", Style::default().fg(accent).bg(band)),
    );
    Line::from(spans)
}

/// Input-panel background: the theme's `tool_input_bg`, or derived from the
/// palette when it's `Color::Reset` (see `Theme::tool_input_background`).
/// The header marker of a tool card whose call waits behind another in its
/// scheduler lane: static and muted, so nothing on the card moves until
/// the call starts.
fn queued_marker() -> Span<'static> {
    Span::styled(" \u{25cb} queued", Style::default().fg(THEME.load().muted))
}

fn input_panel_bg() -> Color {
    THEME.load().tool_input_background()
}

/// Output-panel background: the theme's `tool_output_bg`, or derived from the
/// palette (see `Theme::tool_output_background`).
fn output_panel_bg() -> Color {
    THEME.load().tool_output_background()
}

/// Tool panels span ~90% of the terminal, leaving ~5% margin on each side.
fn tool_panel_width(viewport: usize) -> usize {
    (viewport * 9 / 10).clamp(1, viewport.saturating_sub(2).max(1))
}

/// Left margin before a tool panel (~5% of the viewport).
fn tool_panel_margin(viewport: usize) -> usize {
    viewport / 20
}

/// Half-row lip above a tool card, so the card starts half a row above its
/// header instead of on the header row (which read as chopped): `▗` carries
/// the gutter bar up in `accent`, then `▄` in the card's `bg`, both over the
/// pane. Spans exactly the panel as [`panel_block`] lays it out (`margin`
/// transparent, the gutter cell, `width` inner cells). It replaces the blank
/// spacer row above the card, so the transcript's row count does not change.
fn panel_top_edge(accent: Color, bg: Color, width: usize, margin: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    if margin > 0 {
        spans.push(Span::raw(" ".repeat(margin)));
    }
    spans.push(Span::styled("\u{2597}", Style::default().fg(accent))); // ▗
    spans.push(Span::styled("\u{2584}".repeat(width.max(4)), Style::default().fg(bg))); // ▄
    Line::from(spans)
}

/// Render a tool block as a panel: an inset, subtle-background card (`bg` fills
/// text and padding) fronted by a coloured `▐` gutter bar in `accent`. The left
/// `margin` stays transparent so the panel reads as inset; content is padded to
/// `width` for a clean rectangle.
fn panel_block(
    inner: Vec<Line<'static>>,
    accent: Color,
    bg: Color,
    width: usize,
    margin: usize,
) -> Vec<Line<'static>> {
    if inner.is_empty() {
        return inner;
    }
    let inner_w = width.max(4);
    let fill = Style::default().bg(bg);
    // The bar is the card's outer edge: `▐` over the pane, flush against the
    // fill. A cell holds two colours, so this (not a `▎` on the fill) is what
    // lets the half-row lip carry the bar to the card's top (`▗`).
    let gutter = Style::default().fg(accent);
    inner
        .into_iter()
        .map(|l| {
            let mut line = clamp_line(l, inner_w);
            for span in line.spans.iter_mut() {
                span.style = span.style.bg(bg);
            }
            let pad = inner_w.saturating_sub(line.width());
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 3);
            if margin > 0 {
                spans.push(Span::raw(" ".repeat(margin)));
            }
            spans.push(Span::styled("\u{2590}", gutter)); // ▐ gutter bar
            spans.append(&mut line.spans);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), fill));
            }
            Line::from(spans)
        })
        .collect()
}

/// Line + provenance accumulator for `render_message_lines` (P10 slice (a)).
///
/// `push` records [`LineMeta::Chrome`] by default so the pre-P10 decoration
/// pushes stay chrome without churn; content rows opt in via `push_meta`.
/// The parallel-vec invariant (`lines.len() == meta.len()`) holds by
/// construction, including through the tool arms' `split_off` →
/// `panel_block` → re-extend dance (red-team 2a: meta must move in lockstep
/// with the card split).
#[derive(Default)]
struct LineSink {
    lines: Vec<Line<'static>>,
    meta: Vec<LineMeta>,
}

impl LineSink {
    /// Put the heavy `┃` bar (in `accent`, on `band`) at column 0 of every
    /// row from `start` on — a card's left edge.
    fn bar_rows(&mut self, start: usize, accent: Color, band: Color) {
        for line in self.lines.iter_mut().skip(start) {
            let taken = std::mem::take(line);
            *line = with_bar(taken, accent, band);
        }
    }

    /// Push a decoration row (Chrome). The default on purpose: misclassified
    /// chrome fails loud (missing text on copy); misattributed ranges fail
    /// quiet — so quiet requires opting in.
    fn push(&mut self, line: Line<'static>) {
        self.push_meta(line, LineMeta::Chrome);
    }

    fn push_meta(&mut self, line: Line<'static>, meta: LineMeta) {
        self.lines.push(line);
        self.meta.push(meta);
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    /// Split off rows `[at..]` — lines and meta together (panel_block seam).
    fn split_off(&mut self, at: usize) -> (Vec<Line<'static>>, Vec<LineMeta>) {
        (self.lines.split_off(at), self.meta.split_off(at))
    }

    /// Re-attach transformed card lines with their original meta.
    /// `panel_block` maps rows 1:1 (or empty→empty), so the pairing is exact.
    fn extend_zipped(&mut self, lines: Vec<Line<'static>>, meta: Vec<LineMeta>) {
        debug_assert_eq!(
            lines.len(),
            meta.len(),
            "panel_block must map card rows 1:1 — meta would desync"
        );
        self.lines.extend(lines);
        self.meta.extend(meta);
    }

    fn into_entry(self) -> MsgSlot {
        debug_assert_eq!(self.lines.len(), self.meta.len());
        // Always materialized at render time; demotion (lines → None) is a
        // separate second-half pass over slots leaving the viewport window.
        MsgSlot {
            lines: Some(self.lines),
            height: super::transcript::HeightState::Exact(self.meta.len()),
            meta: Some(self.meta),
        }
    }
}

/// Iterate a message source's lines with their (line_idx, byte_offset)
/// coordinates — the composition step of design §1.3: per-row ranges from
/// `wrap_text_spans` are message-local once offset by the source line's
/// byte position. Matches `str::lines` semantics exactly (the render arms
/// iterate with `text.lines()`).
fn source_lines(text: &str) -> impl Iterator<Item = (usize, usize, &str)> + '_ {
    let base = text.as_ptr() as usize;
    text.lines()
        .enumerate()
        .map(move |(i, line)| (i, line.as_ptr() as usize - base, line))
}

/// Classify one wrapped display row per DECISION LOCK L1/L6.
///
/// The wrap input was `prefix + source_line` where `prefix` is `prefix_len`
/// injected ASCII-space-width bytes/columns. `Content` is emitted ONLY when
/// the final rendered row text carries `source_line[local_range]` as a
/// byte-identical suffix — evaluated literally against `rendered_text`, so
/// any transform between wrap and push (re-prefixing, clamping) disqualifies
/// itself. Trailing ASCII-space padding (the User/Event bg fill) is checked
/// past: it sits right of the content and does not disturb the range↔column
/// correspondence that `Content` encodes (D3 as locked: plain wrapped prose
/// gets exact columns). Everything else falls back to line granularity.
pub(crate) fn classify_row(
    rendered_text: &str,
    row: &super::markdown::WrapRow,
    source_line: &str,
    line_off: usize,
    prefix_len: usize,
    msg_idx: usize,
    src_line: usize,
) -> LineMeta {
    let fallback = LineMeta::ContentLine { msg_idx, src_line };
    // L6: tab-bearing source lines are never Content — the copy-time
    // col→byte walk must never meet a tab (char_width('\t') == 0 footgun).
    if source_line.contains('\t') {
        return fallback;
    }
    let Some(src) = row.src.clone() else {
        return fallback;
    };
    // Map the composed-input range to a source-line-local range.
    let (start, content_col) = if src.start == 0 {
        // First row: its range covers the injected prefix too; content
        // begins prefix_len bytes in, at display column prefix_len.
        (0usize, prefix_len as u16)
    } else if src.start >= prefix_len {
        // Continuation row: wrap injected `row.content_col` spaces of indent.
        (src.start - prefix_len, row.content_col)
    } else {
        // Wrap broke inside the injected prefix — degenerate narrow pane.
        return fallback;
    };
    let end = src.end.saturating_sub(prefix_len).max(start);
    let slice = &source_line[start..end];
    // L1, literal: the rendered row must end with the source slice
    // byte-identically (trailing space padding tolerated per above).
    if !rendered_text.ends_with(slice) && !rendered_text.trim_end_matches(' ').ends_with(slice) {
        return fallback;
    }
    LineMeta::Content {
        range: (line_off + start)..(line_off + end),
        content_col,
    }
}

impl TranscriptStore {
    /// Render the lines for a single message at `idx`, in isolation, plus
    /// per-row source provenance (P10 slice (a): `MsgSlot` carries a
    /// [`LineMeta`] per line; nothing consumes the meta yet).
    /// The rendered lines are identical to the contribution that message[idx]
    /// would make inside `render_lines`, assuming the same prev-message context.
    /// Ephemeral App state (spinner frame, streaming flag, agent name) crosses
    /// the seam via `ctx` — see [`RenderCtx`].
    pub(crate) fn render_message_lines(
        &self,
        idx: usize,
        width: usize,
        ctx: &RenderCtx<'_>,
    ) -> MsgSlot {
        // P11 perf probe (§5.2 / lock L4): measurement IS the render, so
        // counting calls here counts both. Compiled out of production.
        #[cfg(any(test, feature = "testing"))]
        self.probe_note_render();
        let mut lines = LineSink::default();
        let m = "   "; // margin

        let tmsg = &self.messages()[idx];
        let i = idx;
        let ts = &tmsg.time;
        match &tmsg.msg {
            ChatMessage::User(text) => {
                let bg = Style::default().bg(THEME.load().user_bg);
                // Top margin — extra <br> for breathing room before the pill
                lines.push(Line::from(""));
                lines.push(Line::from(""));
                // Top padding
                lines.push(Line::from(Span::styled(
                    format!("{:<width$}", "", width = width),
                    bg,
                )));
                // Header: chevron + name + timestamp right-aligned
                let label = format!("{}\u{276f} you", m);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", label, " ".repeat(gap)),
                        Style::default()
                            .fg(THEME.load().user_color)
                            .bg(THEME.load().user_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        ts_str,
                        Style::default()
                            .fg(THEME.load().muted)
                            .bg(THEME.load().user_bg),
                    ),
                ]));
                // Content — just render the text (pasted messages already contain "[Pasted N lines]")
                let style = Style::default()
                    .fg(THEME.load().user_color)
                    .bg(THEME.load().user_bg);
                for (src_line, line_off, line) in source_lines(text) {
                    let composed = format!("{}  {}", m, line);
                    let prefix_len = m.len() + 2;
                    for row in wrap_text_spans(&composed, width) {
                        let rendered = format!("{:<width$}", row.text, width = width);
                        let meta =
                            classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                        lines.push_meta(Line::from(Span::styled(rendered, style)), meta);
                    }
                }
                // Bottom padding
                lines.push(Line::from(Span::styled(
                    format!("{:<width$}", "", width = width),
                    bg,
                )));
                // Bottom margin
                lines.push(Line::from(""));
            }

            ChatMessage::Thinking(text) => {
                // The light open rail at the message margin
                // (render/thinking.rs). Slice (c): the body is
                // `render_markdown_spans` over the raw source, so its rows
                // carry the agent text's provenance (re-verified on the final
                // row, lock L1); the placeholder's source is empty
                // (`source_text`), so it is all Chrome. Collapsed rows are
                // reachable through the footer's ContentLine (D4 whole-card
                // rule).
                let prev_ends_blank = i > 0
                    && matches!(
                        &self.messages()[i - 1].msg,
                        ChatMessage::User(_)
                            | ChatMessage::Text(_)
                            | ChatMessage::Event { .. }
                            | ChatMessage::Thinking(_)
                    );
                thinking::render(
                    &mut lines,
                    text,
                    tmsg.thought,
                    i,
                    width,
                    self.now(),
                    ctx,
                    prev_ends_blank,
                    self.show_full_output(),
                );
            }

            ChatMessage::Text(text) => {
                // Turn boundary: two blank lines of breathing room between the
                // previous block and the agent header. Replaces the old
                // ──── · ──── divider — the extra <br> carries the beat. A
                // thought is the answer's own prelude: it already ends in a
                // blank row, and the answer hangs right under it.
                let after_thought =
                    i > 0 && matches!(&self.messages()[i - 1].msg, ChatMessage::Thinking(_));
                if i > 0 && !after_thought {
                    lines.push(Line::from(""));
                    lines.push(Line::from(""));
                }
                // Header
                let label = format!("{}\u{25c8} {}", m, ctx.agent_name);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                // Pulse the agent label when streaming (same sin-wave as header dot)
                let label_color = if ctx.streaming && self.streaming_text_idx() == Some(i) {
                    let pulse = ((ctx.spinner_frame as f64 / 20.0).sin() * 0.3 + 0.7).max(0.4);
                    if let Color::Rgb(r, g, b) = THEME.load().claude_label {
                        Color::Rgb(
                            (r as f64 * pulse) as u8,
                            (g as f64 * pulse) as u8,
                            (b as f64 * pulse) as u8,
                        )
                    } else {
                        THEME.load().claude_label
                    }
                } else {
                    THEME.load().claude_label
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", label, " ".repeat(gap)),
                        Style::default()
                            .fg(label_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(ts_str, Style::default().fg(THEME.load().muted)),
                ]));
                // Body
                if text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("{}   \u{2026}", m),
                        Style::default().fg(THEME.load().muted),
                    )));
                } else {
                    // Slice (c): render_markdown_spans classifies per lock L1 —
                    // untransformed wrapped prose gets char-precise Content;
                    // inline-md paragraphs/lists/tables/code/fence chrome map
                    // ContentLine to their source lines; injected spacing
                    // blanks stay Chrome.
                    for (line, meta) in render_markdown_spans(text, m, width, i) {
                        lines.push_meta(line, meta);
                    }
                }
                // Bottom margin — <br> after the agent message
                lines.push(Line::from(""));
            }

            ChatMessage::ToolUseStart {
                tool_id,
                tool_name,
                partial_input,
            } => {
                let margin = tool_panel_margin(width);
                let width = tool_panel_width(width);
                // Breathing room before the tool block; its lower half is the
                // card's top lip.
                lines.push(panel_top_edge(tool_accent(tool_name), input_panel_bg(), width, margin));
                let block_start = lines.len();
                let (icon, display_name, server_tag) = format_tool_name(tool_name);
                let accent = tool_accent(tool_name);
                let mut header = vec![
                    Span::styled(m.to_string(), Style::default().fg(accent)),
                    Span::styled(format!("{} ", icon), Style::default().fg(accent)),
                    Span::styled(
                        display_name,
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(tag) = server_tag {
                    header.push(Span::styled(
                        format!(" [{}]", tag),
                        Style::default().fg(THEME.load().muted),
                    ));
                }
                // Show elapsed time while tool is running
                let elapsed_str = if let Some(start) = self.tool_started_at(tool_id) {
                    let secs = self.now().saturating_duration_since(start).as_secs_f64();
                    if secs >= 1.0 {
                        format!(" {:.1}s", secs)
                    } else {
                        format!(" {}ms", (secs * 1000.0) as u64)
                    }
                } else {
                    String::new()
                };
                let spinner_idx = (ctx.spinner_frame / 3) % SPINNER_FRAMES.len();
                // Only while it runs (wherever the card is); the result row
                // carries the clock once one is beneath it.
                let runs = self.tool_header_runs(i, tool_id);
                // Bash gets a special animated execution trace
                if runs && tool_name == "bash" {
                    let (trace, color) = bash_trace(ctx.spinner_frame);
                    header.push(Span::styled(
                        format!(" {}{}", trace, elapsed_str),
                        Style::default().fg(color),
                    ));
                } else if runs {
                    header.push(Span::styled(
                        format!(" {} running{}", SPINNER_FRAMES[spinner_idx], elapsed_str),
                        Style::default()
                            .fg(THEME.load().status_streaming)
                            .add_modifier(Modifier::DIM),
                    ));
                } else if self.tool_queued(tool_id) {
                    header.push(queued_marker());
                }
                lines.push(Line::from(header));
                // Show accumulated partial input with newlines rendered
                // Slice (c): the tail preview is a transformed view (unescape
                // + "content" scan) of the transient `partial_input` fragment
                // — never Content (lock L2). Display lines don't correspond
                // to raw fragment lines (the unescape is what CREATES them),
                // so all preview rows anchor at source line 0: coarse
                // whole-fragment granularity, per design §2 ("not
                // over-engineered — it's transient; finalize replaces the
                // message"). The D4 tail rule extends a selection over the
                // card's last row to the fragment's end.
                if !partial_input.is_empty() {
                    let preview_meta = LineMeta::ContentLine {
                        msg_idx: i,
                        src_line: 0,
                    };
                    let param_style = Style::default().fg(THEME.load().tool_param);
                    // Unescape \n in JSON string to real newlines for display
                    let unescaped = partial_input.replace("\\n", "\n").replace("\\t", "  ");

                    // Try to extract just the content value if this is a write tool
                    let display = if let Some(idx) = unescaped.find("\"content\": \"") {
                        let content_start = idx + "\"content\": \"".len();
                        &unescaped[content_start..]
                    } else if let Some(idx) = unescaped.find("\"content\":\"") {
                        let content_start = idx + "\"content\":\"".len();
                        &unescaped[content_start..]
                    } else {
                        &unescaped
                    };

                    let content_lines: Vec<&str> = display.lines().collect();
                    let total = content_lines.len();
                    let max_show = 12;
                    // Show last N lines (tail) so you see what's being written now
                    let skip = total.saturating_sub(max_show);
                    if skip > 0 {
                        let omit = format!("{}     … {} lines above", m, skip);
                        lines.push_meta(
                            Line::from(Span::styled(omit, Style::default().fg(THEME.load().muted))),
                            preview_meta.clone(),
                        );
                    }
                    for cline in content_lines.iter().skip(skip) {
                        let line_str = format!("{}       {}", m, cline);
                        for wline in wrap_text(&line_str, width) {
                            lines.push_meta(
                                Line::from(Span::styled(wline, param_style)),
                                preview_meta.clone(),
                            );
                        }
                    }
                }
                lines.push(Line::from("")); // bottom padding of input block
                let (card_lines, card_meta) = lines.split_off(block_start);
                lines.extend_zipped(
                    panel_block(card_lines, accent, input_panel_bg(), width, margin),
                    card_meta,
                );
            }

            ChatMessage::ToolUse {
                tool_id,
                tool_name,
                input,
            } => {
                let margin = tool_panel_margin(width);
                let width = tool_panel_width(width);
                // Breathing room before the tool block; its lower half is the
                // card's top lip.
                lines.push(panel_top_edge(tool_accent(tool_name), input_panel_bg(), width, margin));
                // Compact tool header
                let block_start = lines.len();
                let (icon, display_name, server_tag) = format_tool_name(tool_name);
                let accent = tool_accent(tool_name);
                let mut header = vec![
                    Span::styled(m.to_string(), Style::default().fg(accent)),
                    Span::styled(format!("{} ", icon), Style::default().fg(accent)),
                    Span::styled(
                        display_name,
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(tag) = server_tag {
                    header.push(Span::styled(
                        format!(" [{}]", tag),
                        Style::default().fg(THEME.load().muted),
                    ));
                }
                // While the tool runs, the header carries its clock — wherever
                // the card is (messages may have landed below it), until its
                // result row takes over.
                if self.tool_header_runs(i, tool_id) {
                    let elapsed_str = if let Some(start) = self.tool_started_at(tool_id) {
                        let secs = self.now().saturating_duration_since(start).as_secs_f64();
                        if secs >= 1.0 {
                            format!(" {:.1}s", secs)
                        } else {
                            format!(" {}ms", (secs * 1000.0) as u64)
                        }
                    } else {
                        String::new()
                    };

                    if tool_name == "bash" {
                        let (trace, color) = bash_trace(ctx.spinner_frame);
                        header.push(Span::styled(
                            format!(" {}{}", trace, elapsed_str),
                            Style::default().fg(color),
                        ));
                    } else {
                        let spinner_idx = (ctx.spinner_frame / 3) % SPINNER_FRAMES.len();
                        header.push(Span::styled(
                            format!(" {} running{}", SPINNER_FRAMES[spinner_idx], elapsed_str),
                            Style::default()
                                .fg(THEME.load().status_streaming)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                } else if self.tool_queued(tool_id) {
                    // Waiting behind another call in its lane: a calm,
                    // static marker — no sweep, no clock (it isn't running).
                    header.push(queued_marker());
                }
                lines.push(Line::from(header));
                // Params — key:value on one line each, dimmed
                // Slice (c), locks L2 + L5: this card's canonical source is
                // the PRETTY-PRINTED input JSON (`source_text`); each key's
                // rendered rows (kv line, code-preview rows, diff markers,
                // "+N more") map ContentLine onto the pretty line that opens
                // that key. The search cursor keeps the mapping monotonic if
                // a nested value line happens to look like a later key.
                // Never Content — panel_block re-clamps make char precision
                // fake (L2). The "{" / "}" brace lines carry no rendered row.
                let param_style = Style::default().fg(THEME.load().tool_param);
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                    let pretty = tmsg.msg.source_text();
                    let pretty_lines: Vec<&str> = pretty.lines().collect();
                    let mut search_from = 0usize;
                    let key_line = |k: &str, from: &mut usize| -> usize {
                        let needle = format!(
                            "{}:",
                            serde_json::to_string(k).unwrap_or_else(|_| format!("\"{k}\""))
                        );
                        match pretty_lines
                            .iter()
                            .skip(*from)
                            .position(|l| l.trim_start().starts_with(&needle))
                        {
                            Some(p) => {
                                *from += p;
                                *from
                            }
                            None => 0,
                        }
                    };
                    if let Some(obj) = parsed.as_object() {
                        // Extract file extension from "path" param if present (for syntax highlighting)
                        let file_ext = obj
                            .get("path")
                            .and_then(|v| v.as_str())
                            .and_then(|p| std::path::Path::new(p).extension())
                            .map(|e| e.to_string_lossy().to_string())
                            .unwrap_or_default();

                        for (k, v) in obj {
                            let km = LineMeta::ContentLine {
                                msg_idx: i,
                                src_line: key_line(k, &mut search_from),
                            };
                            if let Some(s) = v.as_str() {
                                if s.contains('\n') {
                                    // Multi-line content: syntax highlight if we know the language
                                    let content_lines: Vec<&str> = s.lines().collect();
                                    let total = content_lines.len();
                                    let max_preview = 12;
                                    let show = total.min(max_preview);

                                    // Diff-style markers for edit tool
                                    let (marker, marker_color) = match k.as_str() {
                                        "old_string" => ("\u{2212}", THEME.load().error_color),
                                        "new_string" => ("+", THEME.load().tool_result_ok),
                                        _ => ("\u{258f}", THEME.load().muted),
                                    };

                                    let label = match k.as_str() {
                                        "old_string" => "old",
                                        "new_string" => "new",
                                        _ => k.as_str(),
                                    };
                                    let header = format!("{}     {}: ({} lines)", m, label, total);
                                    lines.push_meta(
                                        Line::from(Span::styled(header, param_style)),
                                        km.clone(),
                                    );

                                    // Syntax highlight the code
                                    let is_code_param =
                                        k == "content" || k == "old_string" || k == "new_string";
                                    if is_code_param && !file_ext.is_empty() {
                                        let hl_lines = highlight_tool_code(
                                            &content_lines[..show],
                                            &file_ext,
                                            m,
                                            marker,
                                            marker_color,
                                        );
                                        for hl_line in hl_lines {
                                            lines.push_meta(clamp_line(hl_line, width), km.clone());
                                        }
                                    } else {
                                        for (ci, cline) in
                                            content_lines.iter().take(show).enumerate()
                                        {
                                            lines.push_meta(
                                                clamp_line(
                                                    Line::from(vec![
                                                        Span::styled(
                                                            format!(
                                                                "{}    {:>3} {} ",
                                                                m,
                                                                ci + 1,
                                                                marker
                                                            ),
                                                            Style::default().fg(marker_color),
                                                        ),
                                                        Span::styled(
                                                            cline.to_string(),
                                                            param_style,
                                                        ),
                                                    ]),
                                                    width,
                                                ),
                                                km.clone(),
                                            );
                                        }
                                    }
                                    if total > max_preview {
                                        let omit = format!(
                                            "{}       … +{} more lines",
                                            m,
                                            total - max_preview
                                        );
                                        lines.push_meta(
                                            Line::from(Span::styled(
                                                omit,
                                                Style::default().fg(THEME.load().muted),
                                            )),
                                            km.clone(),
                                        );
                                    }
                                } else {
                                    let val = if s.len() > 120 {
                                        let p: String = s.chars().take(120).collect();
                                        format!("{}\u{2026}", p)
                                    } else {
                                        s.to_string()
                                    };
                                    let line_str = format!("{}     {}: {}", m, k, val);
                                    for wline in wrap_text(&line_str, width) {
                                        lines.push_meta(
                                            Line::from(Span::styled(wline, param_style)),
                                            km.clone(),
                                        );
                                    }
                                }
                            } else {
                                let val = v.to_string();
                                let line_str = format!("{}     {}: {}", m, k, val);
                                for wline in wrap_text(&line_str, width) {
                                    lines.push_meta(
                                        Line::from(Span::styled(wline, param_style)),
                                        km.clone(),
                                    );
                                }
                            }
                        }
                    }
                }
                lines.push(Line::from("")); // bottom padding of input block
                let (card_lines, card_meta) = lines.split_off(block_start);
                lines.extend_zipped(
                    panel_block(card_lines, accent, input_panel_bg(), width, margin),
                    card_meta,
                );
            }

            ChatMessage::ToolResult {
                ref tool_id,
                ref content,
                elapsed_ms,
            } => {
                let margin = tool_panel_margin(width);
                let width = tool_panel_width(width);
                let result = content;
                let block_start = lines.len();
                let is_error = result.starts_with("Tool execution failed")
                    || result.starts_with("Unknown tool");
                let is_timeout = result.contains("[TIMED OUT");
                let style = if is_error {
                    Style::default().fg(THEME.load().error_color)
                } else if is_timeout {
                    Style::default().fg(THEME.load().warning_color)
                } else {
                    Style::default().fg(THEME.load().tool_result_color)
                };

                let result_lines: Vec<&str> = result.lines().collect();
                let show = if self.show_full_output() {
                    result_lines.len()
                } else {
                    let max_show = if result_lines.len() > 30 { 15 } else { 12 };
                    result_lines.len().min(max_show)
                };

                // Detect which tool produced this result (bound once for reuse below)
                let preceding_tool = self.find_preceding_tool_name(i);

                // subagent_* results are model-facing JSON: show the user a
                // human card instead (display-only; `content` is untouched).
                let human_card = if is_timeout || is_error {
                    None
                } else {
                    preceding_tool.as_deref().and_then(|tool| {
                        subagent_card::tool_result_card(tool, result, m, width, i, |h| {
                            self.subagent_facts(h)
                        })
                    })
                };

                // Check if this is read tool output (line-numbered) and try syntax highlighting
                // Skip fancy highlighting for timeouts — render everything in warning style
                let highlighted_lines = if is_timeout || is_error || human_card.is_some() {
                    None
                } else if is_read_tool_output(&result_lines) {
                    let ext = self.find_preceding_read_extension(i);
                    highlight_read_output(&result_lines[..show], &ext, m)
                } else if preceding_tool.as_deref() == Some("bash") {
                    Some(highlight_bash_output(&result_lines[..show], m))
                } else {
                    None
                };

                // Content rows: lock L2 — everything through panel_block is
                // line-granular. `content` is the canonical source (§2, raw
                // tool output verbatim) and both highlight paths emit exactly
                // one row per source line, so src_line == the enumeration
                // index; wrapped plain rows all map to their source line.
                if let Some(card) = human_card.as_ref() {
                    // A transformed view of the whole JSON result: every row
                    // maps ContentLine onto it (lock L2: never Content).
                    let cap = if self.show_full_output() { card.len() } else { 12 };
                    for l in card.iter().take(cap) {
                        lines.push_meta(
                            clamp_line(l.clone(), width),
                            LineMeta::ContentLine {
                                msg_idx: i,
                                src_line: 0,
                            },
                        );
                    }
                    if card.len() > cap {
                        lines.push_meta(
                            Line::from(Span::styled(
                                format!("{}       +{} lines", m, card.len() - cap),
                                Style::default().fg(THEME.load().muted),
                            )),
                            LineMeta::ContentLine {
                                msg_idx: i,
                                src_line: 0,
                            },
                        );
                    }
                } else if let Some(hl_lines) = highlighted_lines {
                    if !is_error && !is_timeout {
                        for (li, hl_line) in hl_lines.into_iter().enumerate() {
                            let dimmed_spans: Vec<Span> = hl_line
                                .spans
                                .into_iter()
                                .map(|span| {
                                    Span::styled(
                                        span.content,
                                        span.style.add_modifier(Modifier::DIM),
                                    )
                                })
                                .collect();
                            lines.push_meta(
                                clamp_line(Line::from(dimmed_spans), width),
                                LineMeta::ContentLine {
                                    msg_idx: i,
                                    src_line: li,
                                },
                            );
                        }
                    } else {
                        for (li, hl_line) in hl_lines.into_iter().enumerate() {
                            lines.push_meta(
                                clamp_line(hl_line, width),
                                LineMeta::ContentLine {
                                    msg_idx: i,
                                    src_line: li,
                                },
                            );
                        }
                    }
                } else {
                    for (li, line) in result_lines[..show].iter().enumerate() {
                        // Try to detect and highlight grep output (skip for timeout/error)
                        if !is_timeout && !is_error {
                            // Display-only: paths under the session cwd
                            // shown relative; a row too wide ends in `…`.
                            let shorten = |p: &str| self.path_base().shorten(p).into_owned();
                            if let Some(grep_spans) = try_highlight_grep_line(line, m, &shorten) {
                                lines.push_meta(
                                    clamp_line_ellipsis(Line::from(grep_spans), width),
                                    LineMeta::ContentLine {
                                        msg_idx: i,
                                        src_line: li,
                                    },
                                );
                                continue;
                            }
                        }
                        let line = if preceding_tool.as_deref() == Some("find") {
                            self.path_base().shorten(line)
                        } else {
                            std::borrow::Cow::Borrowed(*line)
                        };
                        let full = format!("{}       {}", m, line);
                        for wline in wrap_text(&full, width) {
                            let body_style = if is_error || is_timeout {
                                style
                            } else {
                                style.add_modifier(Modifier::DIM)
                            };
                            lines.push_meta(
                                Line::from(Span::styled(wline, body_style)),
                                LineMeta::ContentLine {
                                    msg_idx: i,
                                    src_line: li,
                                },
                            );
                        }
                    }
                }
                if human_card.is_none() && result_lines.len() > show {
                    // D4 (as locked): the "+N lines" truncation row stands in
                    // for the hidden tail — ContentLine anchored at the first
                    // hidden source line; slice (d)'s copy path reads a
                    // selection over it as "from src_line to end of content".
                    lines.push_meta(
                        Line::from(Span::styled(
                            format!("{}       +{} lines", m, result_lines.len() - show),
                            Style::default().fg(THEME.load().muted),
                        )),
                        LineMeta::ContentLine {
                            msg_idx: i,
                            src_line: show,
                        },
                    );
                }

                // Footer: timeout indicator is shown unconditionally (even when is_error);
                // success/active footers are only shown when not an error.
                if is_timeout && show > 0 {
                    let elapsed_str = match elapsed_ms {
                        Some(ms) if *ms >= 1000 => format!(" {:.1}s", *ms as f64 / 1000.0),
                        Some(ms) => format!(" {}ms", ms),
                        None => String::new(),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(
                                "{}     \u{2514}\u{2500} \u{26a0} timed out ({} lines)",
                                m,
                                result_lines.len()
                            ),
                            Style::default().fg(THEME.load().warning_color),
                        ),
                        Span::styled(elapsed_str, Style::default().fg(THEME.load().subagent_time)),
                    ]));
                } else if !is_error && (show > 0 || result_lines.is_empty()) {
                    if self.is_active_tool_result(i) {
                        // Tool still executing — show animation only for the active result.
                        let elapsed_str = if let Some(start) = self.tool_started_at(tool_id) {
                            let secs = self.now().saturating_duration_since(start).as_secs_f64();
                            if secs >= 1.0 {
                                format!(" {:.1}s", secs)
                            } else {
                                format!(" {}ms", (secs * 1000.0) as u64)
                            }
                        } else {
                            String::new()
                        };

                        if preceding_tool.as_deref() == Some("bash") {
                            let (trace, color) = bash_trace(ctx.spinner_frame);
                            lines.push(Line::from(vec![
                                Span::styled(format!("{}     ", m), Style::default()),
                                Span::styled(
                                    format!("{}{}", trace, elapsed_str),
                                    Style::default().fg(color),
                                ),
                            ]));
                        } else {
                            let spinner_idx = (ctx.spinner_frame / 3) % SPINNER_FRAMES.len();
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "{}     {} running{}",
                                    m, SPINNER_FRAMES[spinner_idx], elapsed_str
                                ),
                                Style::default()
                                    .fg(THEME.load().status_streaming)
                                    .add_modifier(Modifier::DIM),
                            )));
                        }
                    } else {
                        let elapsed_str = match elapsed_ms {
                            Some(ms) if *ms >= 1000 => format!(" {:.1}s", *ms as f64 / 1000.0),
                            Some(ms) => format!(" {}ms", ms),
                            None => String::new(),
                        };
                        // A tool that printed nothing still ends: say so
                        // rather than leave its card open-ended (#287).
                        let summary = if human_card.is_some() {
                            // The card already says what happened.
                            String::new()
                        } else if result_lines.is_empty() {
                            " (no output)".to_string()
                        } else {
                            format!(" ({} lines)", result_lines.len())
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}     \u{2514}\u{2500} ok{}", m, summary),
                                Style::default().fg(THEME.load().tool_result_ok),
                            ),
                            Span::styled(
                                elapsed_str,
                                Style::default().fg(THEME.load().subagent_time),
                            ),
                        ]));
                    }
                }
                // One blank line of bottom padding inside the card (filled
                // with the panel bg + gutter by panel_block).
                lines.push(Line::from(""));
                let (card_lines, card_meta) = lines.split_off(block_start);
                let accent = if is_error {
                    THEME.load().error_color
                } else if is_timeout {
                    THEME.load().warning_color
                } else {
                    preceding_tool
                        .map(|n| tool_accent(&n))
                        .unwrap_or_else(|| tool_accent("_generic"))
                };
                lines.extend_zipped(
                    panel_block(card_lines, accent, output_panel_bg(), width, margin),
                    card_meta,
                );
            }

            ChatMessage::Error(err) => {
                // Newline-aware AND wrap-aware. The ✘ glyph appears on
                // the first row only; continuation rows (whether from a
                // hard \n or from soft-wrap) use blank padding aligned
                // under the message body.
                let err_style = Style::default().fg(THEME.load().error_color);
                let mut first_row = true;
                let strip = format!("{}    ", m);
                let prefix_len = m.len() + 4;
                for (src_line, line_off, line) in source_lines(err) {
                    for row in wrap_text_spans(&format!("{}    {}", m, line), width) {
                        // wrap_text emits the prefix on every output row;
                        // strip ours so we can re-add the glyph or padding
                        // exactly once at the head.
                        let body = row
                            .text
                            .strip_prefix(&strip)
                            .unwrap_or(&row.text)
                            .to_string();
                        let prefix = if first_row {
                            format!("{}  \u{2718} ", m)
                        } else {
                            format!("{}    ", m)
                        };
                        first_row = false;
                        // Classify against the final composed row: the glyph
                        // prefix is 7 display cells, same as the wrap prefix,
                        // so Content columns stay true; the ends_with check
                        // (L1) verifies the source suffix survived the
                        // strip/re-prefix dance byte-for-byte.
                        let rendered = format!("{}{}", prefix, body);
                        let meta =
                            classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                        lines.push_meta(
                            Line::from(vec![
                                Span::styled(prefix, err_style),
                                Span::styled(body, err_style),
                            ]),
                            meta,
                        );
                    }
                }
            }

            ChatMessage::System(msg) => {
                if should_separate_system_messages(
                    self.messages().get(i.saturating_sub(1)).map(|msg| &msg.msg),
                    &tmsg.msg,
                ) {
                    lines.push(Line::from(""));
                }
                // Newline-aware AND wrap-aware: split on '\n' first so
                // explicit line breaks always render as separate rows,
                // then wrap each line on word boundaries to fit `width`.
                // Mirrors the User/Text pattern using wrap_text() so all
                // chat content wraps consistently.
                let style = Style::default()
                    .fg(THEME.load().system_msg);
                for (src_line, line_off, line) in source_lines(msg) {
                    let prefix_len = m.len() + 2;
                    for row in wrap_text_spans(&format!("{}  {}", m, line), width) {
                        let meta =
                            classify_row(&row.text, &row, line, line_off, prefix_len, i, src_line);
                        let text = row.text;
                        lines.push_meta(Line::from(Span::styled(text, style)), meta);
                    }
                }
            }

            ChatMessage::Event {
                source,
                severity,
                text,
            } => {
                let theme = THEME.load();
                // A one-cell glyph in the palette's colour (colour emoji
                // ignore the theme and are two cells wide).
                let icon = match severity.as_str() {
                    "critical" => "✖",
                    "low" => "◇",
                    _ => "◆",
                };
                let sev_color = theme.event_accent(severity);
                // Theme surface (was a fixed slate), with the severity
                // colour as a heavy bar down the card's left edge.
                let event_bg = theme.raised_surface(1.18);
                let bg = Style::default().bg(event_bg);
                // Top spacing
                lines.push(Line::from(""));
                let event_start = lines.len();
                // Top padding
                lines.push(Line::from(Span::styled(
                    format!("{:<width$}", "", width = width),
                    bg,
                )));
                // Header: icon + source (severity is not rendered as a span)
                let header = format!("{}  {} [{}]", m, icon, source);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(header.chars().count() + ts_str.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}  {} ", m, icon),
                        Style::default().fg(sev_color).bg(event_bg),
                    ),
                    Span::styled(
                        format!("[{}]", source),
                        Style::default()
                            .fg(theme.event_label())
                            .bg(event_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" ".repeat(gap).to_string(), Style::default().bg(event_bg)),
                    Span::styled(ts_str, Style::default().fg(theme.muted).bg(event_bg)),
                ]));
                // Content
                let text_style = Style::default().fg(theme.event_body()).bg(event_bg);
                // A subagent completion carries a model-facing sentence
                // ("… Call subagent_collect with handle_id …"); show the user
                // a compact summary + preview. Display-only: `text` (and the
                // model's prompt-cached copy) is untouched.
                let completion = (source == "subagent")
                    .then(|| subagent_card::parse_completion(text))
                    .flatten();
                if let Some(c) = completion {
                    let meta = LineMeta::ContentLine {
                        msg_idx: i,
                        src_line: 0,
                    };
                    let (glyph, _, color) = subagent_card::status_badge(c.status);
                    let verb = match c.status {
                        "completed" => "finished".to_string(),
                        other => subagent_card::status_badge(other).1,
                    };
                    let head = format!(
                        "{}  {} {} {} \u{00B7} {}",
                        m,
                        glyph,
                        c.name,
                        verb,
                        subagent_card::fmt_secs(c.secs)
                    );
                    let gap = width.saturating_sub(display_width(head.as_str()));
                    let glyph_at = m.len() + 2;
                    lines.push_meta(
                        clamp_line(
                            Line::from(vec![
                                Span::styled(head[..glyph_at].to_string(), text_style),
                                Span::styled(
                                    format!("{} ", glyph),
                                    Style::default().fg(color).bg(event_bg),
                                ),
                                Span::styled(
                                    head[glyph_at + glyph.len() + 1..].to_string(),
                                    text_style.add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(" ".repeat(gap), text_style),
                            ]),
                            width,
                        ),
                        meta.clone(),
                    );
                    let mut body: Vec<(String, Style)> = Vec::new();
                    if let Some(r) = c.reason {
                        body.push((
                            format!("reason: {}", r),
                            Style::default().fg(theme.error_color).bg(event_bg),
                        ));
                    }
                    for pl in super::markdown::strip_md_markers(c.preview).lines() {
                        body.push((pl.to_string(), text_style));
                    }
                    for (line, st) in body {
                        for wline in wrap_text(&format!("{}    {}", m, line), width) {
                            let rendered = format!("{:<width$}", wline, width = width);
                            lines.push_meta(Line::from(Span::styled(rendered, st)), meta.clone());
                        }
                    }
                } else {
                    for (src_line, line_off, line) in source_lines(text) {
                        let prefix_len = m.len() + 2;
                        for row in wrap_text_spans(&format!("{}  {}", m, line), width) {
                            let rendered = format!("{:<width$}", row.text, width = width);
                            let meta =
                                classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                            lines.push_meta(Line::from(Span::styled(rendered, text_style)), meta);
                        }
                    }
                }
                // Bottom padding
                lines.push(Line::from(Span::styled(
                    format!("{:<width$}", "", width = width),
                    bg,
                )));
                lines.bar_rows(event_start, sev_color, event_bg);
                lines.push(Line::from(""));
            }
        }

        lines.into_entry()
    }

    /// Render the entire transcript by concatenating every message's lines.
    /// Test-only: the live path (`build_render_model`) uses the incremental
    /// per-message cache and never rebuilds the whole transcript at once.
    /// Retained as the reference oracle that the cache tests assert against.
    #[cfg(test)]
    pub(crate) fn render_lines(&self, width: usize, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for i in 0..self.messages().len() {
            lines.extend(
                self.render_message_lines(i, width, ctx)
                    .lines
                    .expect("freshly rendered slot has lines"),
            );
        }
        lines
    }
}

fn should_separate_system_messages(prev: Option<&ChatMessage>, current: &ChatMessage) -> bool {
    let Some(ChatMessage::System(prev)) = prev else {
        return false;
    };
    let ChatMessage::System(current) = current else {
        return false;
    };
    !is_grouped_system_continuation(prev, current)
}

fn is_grouped_system_continuation(prev: &str, current: &str) -> bool {
    current.starts_with(' ')
        || current.starts_with('\t')
        || prev.trim_end().ends_with(':')
        || prev.trim_end().ends_with('…')
}

#[cfg(test)]
mod meta_tests {
    use super::super::transcript::{ChatMessage, LineMeta, RenderCtx, TranscriptStore};

    fn ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "agent",
        }
    }

    fn flatten(line: &ratatui::text::Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Event cards use a one-cell glyph in the theme's accent (no colour
    /// emoji), the bar takes the same accent, and the header row fills the
    /// card width exactly (a two-cell emoji used to push the timestamp out).
    #[test]
    fn event_card_is_themed_and_aligned() {
        use unicode_width::UnicodeWidthStr;
        let theme = crate::tui::theme::THEME.load();
        for (severity, glyph) in [("high", "◆"), ("critical", "✖"), ("low", "◇")] {
            let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
            store.push_msg(ChatMessage::Event {
                source: "subagent".into(),
                severity: severity.into(),
                text: "Subagent 'spike' finished".into(),
            });
            let entry = store.render_message_lines(0, 60, &ctx());
            let rows: Vec<String> = entry.lines().iter().map(flatten).collect();
            let header = rows
                .iter()
                .position(|r| r.contains("[subagent]"))
                .expect("header row");
            assert!(rows[header].contains(glyph), "{severity}: {:?}", rows[header]);
            for r in &rows {
                assert!(
                    !r.chars().any(|c| ('\u{1F300}'..='\u{1FAFF}').contains(&c)),
                    "no colour emoji: {r:?}"
                );
            }
            let accent = theme.event_accent(severity);
            let header_line = &entry.lines()[header];
            assert!(
                header_line.spans.iter().any(|s| s.content.contains(glyph) && s.style.fg == Some(accent)),
                "{severity}: glyph in the accent colour"
            );
            assert!(
                header_line.spans.iter().any(|s| s.style.fg == Some(accent) || s.style.bg == Some(accent)),
                "{severity}: bar in the accent colour"
            );
            let widths: Vec<usize> = rows
                .iter()
                .filter(|r| !r.is_empty())
                .map(|r| UnicodeWidthStr::width(r.as_str()))
                .collect();
            assert!(widths.windows(2).all(|w| w[0] == w[1]), "{severity}: rows {widths:?}");
        }
    }

    /// Thinking rows word-wrap (no mid-word cuts) on the open rail: the
    /// `╭─ ◇` header, `│` on every body row with the text two cells off it,
    /// and the closing `╰` on the block's last row only.
    #[test]
    fn thinking_wraps_on_words_and_closes_on_the_last_row() {
        let last = "The safest approach is to not collect and just give a brief reply, \
                    since there's no new instruction asking me to collect here.";
        let text = format!("First short thought.\n{last}");
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        store.push_msg(ChatMessage::Thinking(text));
        for width in [40usize, 57, 80, 117] {
            let rows: Vec<String> = store
                .render_message_lines(0, width, &ctx())
                .lines()
                .iter()
                .map(flatten)
                .filter(|r| !r.is_empty())
                .collect();
            assert!(
                rows[0].starts_with("   \u{256d}\u{2500} \u{25c7} thought"),
                "w={width}: {rows:#?}"
            );
            let body = &rows[1..];
            let closers: Vec<usize> = body
                .iter()
                .enumerate()
                .filter(|(_, r)| r.starts_with("   \u{2570}"))
                .map(|(k, _)| k)
                .collect();
            assert_eq!(closers, vec![body.len() - 1], "w={width}: {rows:#?}");
            assert!(
                body[..body.len() - 1].iter().all(|r| r.starts_with("   \u{2502}  ")),
                "w={width}: rail on every row: {rows:#?}"
            );
            let words: Vec<String> = body
                .iter()
                .skip(1)
                .map(|r| r.chars().skip(6).collect::<String>().trim().to_string())
                .collect();
            assert_eq!(words.join(" "), last, "w={width}: words intact: {rows:#?}");
            for r in &rows {
                assert!(unicode_width::UnicodeWidthStr::width(r.as_str()) <= width, "{r:?}");
            }
        }
    }

    /// P10 slice (a) plumbing invariants: every rendered row is classified
    /// (meta parallel to lines through every arm, incl. the tool-card
    /// `split_off` → `panel_block` → re-extend seam); `Content` ranges are
    /// in-bounds, monotonic, non-overlapping, and byte-identical suffixes of
    /// the rendered row (lock L1, checked against the FINAL row text);
    /// `ContentLine` rows carry the right msg_idx and an in-bounds src_line;
    /// tool cards never emit `Content` (lock L2).
    #[test]
    fn msg_entry_meta_parallel_and_content_ranges_sound() {
        let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
        store.push_msg(ChatMessage::User(
            "hello there\nsecond user line that is long enough to wrap at forty columns easily"
                .into(),
        ));
        store.push_msg(ChatMessage::Thinking(
            "pondering the request at hand, in plain prose that wraps at forty\n\
             ## A plan\n- weigh `sync_cache` and **the splice**\n\n```rust\nlet y = 2;\n```"
                .into(),
        ));
        store.push_msg(ChatMessage::Text(
            "Some **markdown** text with `code`\n\n```rust\nlet x = 1;\n```".into(),
        ));
        store.push_msg(ChatMessage::ToolUse {
            tool_id: "t1".into(),
            tool_name: "bash".into(),
            input: r#"{"command":"ls -la"}"#.into(),
        });
        store.push_msg(ChatMessage::ToolResult {
            tool_id: "t1".into(),
            content: "a.txt\nb.txt\nline\twith\ttabs\nmore output text".into(),
            elapsed_ms: Some(5),
        });
        store.push_msg(ChatMessage::Error(
            "boom happened\nwith a second error line".into(),
        ));
        store.push_msg(ChatMessage::System(
            "system notice spanning enough words to wrap at narrow widths for coverage".into(),
        ));
        store.push_msg(ChatMessage::Event {
            source: "mail".into(),
            severity: "low".into(),
            text: "an event text body with several words in it".into(),
        });

        for width in [40usize, 80] {
            for idx in 0..store.message_count() {
                let msg = &store.messages()[idx].msg;
                // Slice (c): meta invariants are checked against the CANONICAL
                // copy source — `source_text` — which is what `selected_text`
                // reconstructs from (for ToolUse that's the pretty-printed
                // input JSON, lock L5).
                let source: String = store.source_text(idx).into_owned();
                let is_tool_card = matches!(
                    msg,
                    ChatMessage::ToolUse { .. }
                        | ChatMessage::ToolUseStart { .. }
                        | ChatMessage::ToolResult { .. }
                );
                let entry = store.render_message_lines(idx, width, &ctx());
                assert_eq!(
                    entry.lines().len(),
                    entry.meta_slice().len(),
                    "meta must stay parallel to lines (msg {idx}, width {width})"
                );
                let mut prev_end = 0usize;
                for (row, meta) in entry.meta_slice().iter().enumerate() {
                    match meta {
                        LineMeta::Chrome => {}
                        LineMeta::Content { range, content_col } => {
                            assert!(
                                !is_tool_card,
                                "lock L2: tool-card rows must never be Content (msg {idx} row {row})"
                            );
                            assert!(
                                range.end <= source.len() && range.start <= range.end,
                                "Content range {range:?} out of bounds (msg {idx} row {row}, width {width})"
                            );
                            assert!(
                                source.is_char_boundary(range.start)
                                    && source.is_char_boundary(range.end),
                                "Content range {range:?} splits a char (msg {idx} row {row})"
                            );
                            assert!(
                                range.start >= prev_end,
                                "Content ranges must be monotonic non-overlapping \
                                 (msg {idx} row {row}: {range:?} after {prev_end})"
                            );
                            prev_end = range.end;
                            // L1, re-verified end-to-end: the final rendered
                            // row carries source[range] byte-identically
                            // (trailing space padding tolerated).
                            let text = flatten(&entry.lines()[row]);
                            let slice = &source[range.clone()];
                            assert!(
                                text.ends_with(slice) || text.trim_end_matches(' ').ends_with(slice),
                                "Content row must carry source[range] as suffix \
                                 (msg {idx} row {row}, width {width}):\n row: {text:?}\n src: {slice:?}"
                            );
                            let _ = content_col;
                        }
                        LineMeta::ContentLine { msg_idx, src_line } => {
                            assert_eq!(*msg_idx, idx, "ContentLine msg_idx must match owner");
                            assert!(
                                *src_line <= source.lines().count(),
                                "ContentLine src_line {src_line} out of bounds (msg {idx} row {row})"
                            );
                        }
                    }
                }
            }
        }

        // Plumbing must actually emit provenance for the plain-wrap paths —
        // an all-Chrome regression would pass the checks above vacuously.
        for (idx, wants_content) in [(0usize, true), (1, true), (6, true), (5, true)] {
            let entry = store.render_message_lines(idx, 80, &ctx());
            assert_eq!(
                entry
                    .meta_slice()
                    .iter()
                    .any(|m| matches!(m, LineMeta::Content { .. })),
                wants_content,
                "msg {idx} should emit Content rows for its plain wrapped body"
            );
        }
        // ToolResult rows are line-mapped (L2).
        let entry = store.render_message_lines(4, 80, &ctx());
        assert!(
            entry
                .meta_slice()
                .iter()
                .any(|m| matches!(m, LineMeta::ContentLine { .. })),
            "ToolResult content rows must be ContentLine-mapped"
        );
    }

    /// Tool cards open with a half-row lip: the row above the header is `▄`
    /// in the card's background over the pane, spanning exactly the panel
    /// (margin left transparent). It replaces the old blank spacer, so the
    /// header is still the second row and the card's height is unchanged.
    /// Same for the streaming card, so nothing jumps when it completes.
    #[test]
    fn tool_card_has_half_row_top_lip() {
        use unicode_width::UnicodeWidthStr;
        let bg = super::input_panel_bg();
        for streaming in [false, true] {
            let mut store = TranscriptStore::new(crate::tui::clock::TuiClock::real());
            store.push_msg(if streaming {
                ChatMessage::ToolUseStart {
                    tool_id: "t1".into(),
                    tool_name: "bash".into(),
                    partial_input: r#"{"command":"ls"#.into(),
                }
            } else {
                ChatMessage::ToolUse {
                    tool_id: "t1".into(),
                    tool_name: "bash".into(),
                    input: r#"{"command":"ls"}"#.into(),
                }
            });
            for width in [40usize, 80, 120] {
                let entry = store.render_message_lines(0, width, &ctx());
                let rows: Vec<String> = entry.lines().iter().map(flatten).collect();
                let header = rows.iter().position(|r| r.contains("bash")).expect("header row");
                assert_eq!(header, 1, "streaming={streaming} width={width}: {rows:?}");
                let lip = &rows[0];
                let body = lip.trim_start_matches(' ');
                assert!(
                    body.starts_with('\u{2597}') && body.chars().skip(1).all(|c| c == '\u{2584}'),
                    "lip is ▗ then ▄: {lip:?}"
                );
                assert!(
                    rows[header].trim_start_matches(' ').starts_with('\u{2590}'),
                    "the bar is ▐ under the lip's ▗: {:?}",
                    rows[header]
                );
                assert_eq!(lip.len() - body.len(), super::tool_panel_margin(width), "margin");
                assert_eq!(
                    UnicodeWidthStr::width(lip.as_str()),
                    UnicodeWidthStr::width(rows[header].as_str()),
                    "lip spans the card exactly (width {width})"
                );
                let accent = super::tool_accent("bash");
                for span in &entry.lines()[0].spans {
                    if span.content.contains('\u{2597}') {
                        assert_eq!(span.style.fg, Some(accent), "lip corner in the bar colour");
                    } else if span.content.contains('\u{2584}') {
                        assert_eq!(span.style.fg, Some(bg), "lip in the card background");
                    }
                    assert_eq!(span.style.bg, None, "upper half shows the pane");
                }
                let bar = entry.lines()[header]
                    .spans
                    .iter()
                    .find(|s| s.content.contains('\u{2590}'))
                    .expect("bar span");
                assert_eq!((bar.style.fg, bar.style.bg), (Some(accent), None), "bar on the pane");
            }
        }
    }
}
