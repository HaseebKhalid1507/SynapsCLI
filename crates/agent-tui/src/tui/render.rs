//! Message rendering — converts ChatMessage variants into styled ratatui Lines.
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::app::SPINNER_FRAMES;
use super::transcript::{ChatMessage, LineMeta, MsgSlot, RenderCtx, TranscriptStore, THINKING_PLACEHOLDER};
use super::theme::THEME;
use super::highlight::{highlight_tool_code, highlight_bash_output, highlight_read_output, try_highlight_grep_line, is_read_tool_output, clamp_line};
use super::markdown::{render_markdown_spans, wrap_text, wrap_text_spans};
use super::draw::{bash_trace, format_tool_name, tool_accent};

/// Lighten (or darken, with negative `amt`) an RGB colour additively per
/// channel, clamped. Used to derive subtle panel backgrounds from the theme.
fn lighten(c: Color, amt: i16) -> Color {
    if let Color::Rgb(r, g, b) = c {
        let f = |v: u8| (v as i16 + amt).clamp(0, 255) as u8;
        Color::Rgb(f(r), f(g), f(b))
    } else {
        c
    }
}

/// Input-panel background: the theme's `tool_input_bg`, or a subtle tint
/// auto-derived from `bg` when it's left as `Color::Reset` (the default).
fn input_panel_bg() -> Color {
    let t = THEME.load();
    match t.tool_input_bg {
        Color::Reset => lighten(t.bg, 8),
        c => c,
    }
}

/// Output-panel background: the theme's `tool_output_bg`, or an auto-derived
/// (slightly lighter) tint when left as `Color::Reset`.
fn output_panel_bg() -> Color {
    let t = THEME.load();
    match t.tool_output_bg {
        Color::Reset => lighten(t.bg, 16),
        c => c,
    }
}

/// Tool panels span ~90% of the terminal, leaving ~5% margin on each side.
fn tool_panel_width(viewport: usize) -> usize {
    (viewport * 9 / 10).clamp(1, viewport.saturating_sub(2).max(1))
}

/// Left margin before a tool panel (~5% of the viewport).
fn tool_panel_margin(viewport: usize) -> usize {
    viewport / 20
}

/// Render a tool block as a panel: an inset, subtle-background card (`bg` fills
/// text and padding) fronted by a coloured gutter bar in `accent`. The left
/// `margin` stays transparent so the panel reads as inset; content is padded to
/// `width` for a clean rectangle.
fn panel_block(inner: Vec<Line<'static>>, accent: Color, bg: Color, width: usize, margin: usize) -> Vec<Line<'static>> {
    if inner.is_empty() {
        return inner;
    }
    let inner_w = width.max(4);
    let fill = Style::default().bg(bg);
    let gutter = Style::default().fg(accent).bg(bg);
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
            spans.push(Span::styled("\u{258E}", gutter)); // ▎ gutter bar
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
        MsgSlot { lines: Some(self.lines), meta: self.meta }
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
    if !rendered_text.ends_with(slice)
        && !rendered_text.trim_end_matches(' ').ends_with(slice)
    {
        return fallback;
    }
    LineMeta::Content { range: (line_off + start)..(line_off + end), content_col }
}

impl TranscriptStore {
    /// Render the lines for a single message at `idx`, in isolation, plus
    /// per-row source provenance (P10 slice (a): `MsgSlot` carries a
    /// [`LineMeta`] per line; nothing consumes the meta yet).
    /// The rendered lines are identical to the contribution that message[idx]
    /// would make inside `render_lines`, assuming the same prev-message context.
    /// Ephemeral App state (spinner frame, streaming flag, agent name) crosses
    /// the seam via `ctx` — see [`RenderCtx`].
    pub(crate) fn render_message_lines(&self, idx: usize, width: usize, ctx: &RenderCtx<'_>) -> MsgSlot {
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
                // Top margin
                lines.push(Line::from(""));
                // Top padding
                lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
                // Header: chevron + name + timestamp right-aligned
                let label = format!("{}\u{276f} you", m);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", label, " ".repeat(gap)),
                        Style::default().fg(THEME.load().user_color).bg(THEME.load().user_bg).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(ts_str, Style::default().fg(THEME.load().muted).bg(THEME.load().user_bg)),
                ]));
                // Content — just render the text (pasted messages already contain "[Pasted N lines]")
                let style = Style::default().fg(THEME.load().user_color).bg(THEME.load().user_bg);
                for (src_line, line_off, line) in source_lines(text) {
                    let composed = format!("{}  {}", m, line);
                    let prefix_len = m.len() + 2;
                    for row in wrap_text_spans(&composed, width) {
                        let rendered = format!("{:<width$}", row.text, width = width);
                        let meta = classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                        lines.push_meta(Line::from(Span::styled(rendered, style)), meta);
                    }
                }
                // Bottom padding
                lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
                // Bottom margin
                lines.push(Line::from(""));
            }

            ChatMessage::Thinking(text) => {
                // Only add spacing if previous message wasn't a User block
                // (User blocks already have bottom margin)
                let prev_was_user = i > 0 && matches!(&self.messages()[i - 1].msg, ChatMessage::User(_));
                if !prev_was_user {
                    lines.push(Line::from(""));
                }
                let dim = Style::default().fg(THEME.load().thinking_color);
                let dim_italic = dim.add_modifier(Modifier::ITALIC);
                // Header
                let thinking_label = if text == THINKING_PLACEHOLDER {
                    let braille = ['\u{28fe}','\u{28f7}','\u{28ef}','\u{28df}','\u{287f}','\u{28bf}','\u{28fb}','\u{28fd}'];
                    let idx = (ctx.spinner_frame / 4) % braille.len();
                    let wave: String = (0..3).map(|i| braille[(idx + i) % braille.len()]).collect();
                    format!("{} thinking", wave)
                } else {
                    "thinking".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{}╭─ ", m), dim),
                    Span::styled(thinking_label, dim.add_modifier(Modifier::DIM)),
                ]));
                // Body — structured with visual hierarchy
                // Slice (c): rows are trimmed + char-chunked — transformed
                // views, so they map to their untrimmed source lines at
                // ContentLine granularity only (design §2 A2, lock L1
                // fallback). The spinner placeholder has empty source
                // (`source_text`), so its rows stay Chrome. Hidden tail
                // lines (>8) are reachable via the D4 whole-card rule.
                let is_placeholder = text == THINKING_PLACEHOLDER;
                let non_empty: Vec<(usize, &str)> = text
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| !l.trim().is_empty())
                    .collect();
                let show = non_empty.len().min(8);
                // Calculate usable width for thinking content
                let prefix_len = m.len() + 4; // margin + "│ · " or "│ "
                let content_width = width.saturating_sub(prefix_len);

                for (k, (src_line, line)) in non_empty[..show].iter().enumerate() {
                    let meta = if is_placeholder {
                        LineMeta::Chrome
                    } else {
                        LineMeta::ContentLine { msg_idx: i, src_line: *src_line }
                    };
                    let trimmed = line.trim();
                    let is_last = k == show - 1 && non_empty.len() <= 8;
                    let connector = if is_last { "╰" } else { "│" };
                    let continuation = "│";

                    // Detect structure in thinking
                    let (prefix_char, line_style) = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                        ("· ", dim_italic)
                    } else if trimmed.ends_with(':') || trimmed.starts_with('#') {
                        ("", dim.add_modifier(Modifier::BOLD))
                    } else if trimmed.starts_with("```") {
                        ("", dim.add_modifier(Modifier::DIM))
                    } else {
                        ("", dim_italic)
                    };

                    // Wrap manually to preserve connector on each line
                    let first_prefix = format!("{}{} {}", m, connector, prefix_char);
                    let cont_prefix = format!("{}{} {}", m, continuation, " ".repeat(prefix_char.len()));

                    if content_width > 10 {
                        let chars: Vec<char> = trimmed.chars().collect();
                        let mut pos = 0;
                        let mut is_first = true;
                        while pos < chars.len() {
                            let chunk_len = content_width.min(chars.len() - pos);
                            let chunk: String = chars[pos..pos + chunk_len].iter().collect();
                            let prefix = if is_first { &first_prefix } else { &cont_prefix };
                            lines.push_meta(Line::from(Span::styled(
                                format!("{}{}", prefix, chunk),
                                line_style,
                            )), meta.clone());
                            pos += chunk_len;
                            is_first = false;
                        }
                    } else {
                        lines.push_meta(Line::from(Span::styled(
                            format!("{}{}", first_prefix, trimmed),
                            line_style,
                        )), meta);
                    }
                }
                if non_empty.len() > 8 {
                    lines.push(Line::from(Span::styled(
                        format!("{}╰ +{} lines", m, non_empty.len() - 8), dim,
                    )));
                }
            }

            ChatMessage::Text(text) => {
                // Separator between user block and agent response
                // After thinking: just a single blank line (no separator)
                let prev_was_thinking = i > 0 && matches!(&self.messages()[i - 1].msg, ChatMessage::Thinking(_));
                if prev_was_thinking {
                    lines.push(Line::from(""));
                } else if i > 0 {
                    lines.push(Line::from(""));
                    let sep_total = width.min(40);
                    let sep_half = sep_total / 2;
                    let sep_left: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                    let sep_right: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                    let sep_content_width = sep_left.chars().count() + 3 + sep_right.chars().count();
                    let pad_left = width.saturating_sub(sep_content_width) / 2;
                    lines.push(Line::from(vec![
                        Span::styled(" ".repeat(pad_left), Style::default()),
                        Span::styled(sep_left, Style::default().fg(THEME.load().separator)),
                        Span::styled(" \u{00b7} ", Style::default().fg(Color::Rgb(35, 55, 75))),
                        Span::styled(sep_right, Style::default().fg(THEME.load().separator)),
                    ]));
                    lines.push(Line::from(""));
                }
                // Header
                let label = format!("{}\u{25c8} {}", m, ctx.agent_name);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                // Pulse the agent label when streaming (same sin-wave as header dot)
                let label_color = if ctx.streaming && i == self.messages().len() - 1 {
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
                        Style::default().fg(label_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(ts_str, Style::default().fg(THEME.load().muted)),
                ]));
                // Body
                if text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("{}   \u{2026}", m), Style::default().fg(THEME.load().muted),
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
            }

            ChatMessage::ToolUseStart { tool_name, partial_input, .. } => {
                let margin = tool_panel_margin(width);
                let width = tool_panel_width(width);
                // Breathing room before tool block
                lines.push(Line::from(""));
                let block_start = lines.len();
                let (icon, display_name, server_tag) = format_tool_name(tool_name);
                let accent = tool_accent(tool_name);
                let mut header = vec![
                    Span::styled(m.to_string(), Style::default().fg(accent)),
                    Span::styled(format!("{} ", icon), Style::default().fg(accent)),
                    Span::styled(display_name, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                ];
                if let Some(tag) = server_tag {
                    header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.load().muted)));
                }
                // Show elapsed time while tool is running
                let elapsed_str = if let Some(start) = self.tool_start_time() {
                    let secs = start.elapsed().as_secs_f64();
                    if secs >= 1.0 {
                        format!(" {:.1}s", secs)
                    } else {
                        format!(" {}ms", (secs * 1000.0) as u64)
                    }
                } else {
                    String::new()
                };
                let spinner_idx = (ctx.spinner_frame / 3) % SPINNER_FRAMES.len();
                // Bash gets a special animated execution trace
                if tool_name == "bash" {
                    let (trace, color) = bash_trace(ctx.spinner_frame);
                    header.push(Span::styled(
                        format!(" {}{}", trace, elapsed_str),
                        Style::default().fg(color),
                    ));
                } else {
                    header.push(Span::styled(
                        format!(" {} running{}", SPINNER_FRAMES[spinner_idx], elapsed_str),
                        Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
                    ));
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
                    let preview_meta = LineMeta::ContentLine { msg_idx: i, src_line: 0 };
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

            ChatMessage::ToolUse { tool_name, input, .. } => {
                let margin = tool_panel_margin(width);
                let width = tool_panel_width(width);
                // Breathing room before tool block
                lines.push(Line::from(""));
                // Compact tool header
                let block_start = lines.len();
                let (icon, display_name, server_tag) = format_tool_name(tool_name);
                let accent = tool_accent(tool_name);
                let mut header = vec![
                    Span::styled(m.to_string(), Style::default().fg(accent)),
                    Span::styled(format!("{} ", icon), Style::default().fg(accent)),
                    Span::styled(display_name, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                ];
                if let Some(tag) = server_tag {
                    header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.load().muted)));
                }
                // If this is the last message and a tool is executing, show animation
                let is_last = i == self.messages().len() - 1;
                if is_last && self.tool_start_time().is_some() {
                    let elapsed_str = if let Some(start) = self.tool_start_time() {
                        let secs = start.elapsed().as_secs_f64();
                        if secs >= 1.0 { format!(" {:.1}s", secs) }
                        else { format!(" {}ms", (secs * 1000.0) as u64) }
                    } else { String::new() };

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
                            Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
                        ));
                    }
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
                        let file_ext = obj.get("path")
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
                                        "old_string" => ("−", Color::Rgb(200, 60, 60)),
                                        "new_string" => ("+", Color::Rgb(60, 200, 80)),
                                        _ => ("│", THEME.load().muted),
                                    };

                                    let label = match k.as_str() {
                                        "old_string" => "old",
                                        "new_string" => "new",
                                        _ => k.as_str(),
                                    };
                                    let header = format!("{}     {}: ({} lines)", m, label, total);
                                    lines.push_meta(Line::from(Span::styled(header, param_style)), km.clone());

                                    // Syntax highlight the code
                                    let is_code_param = k == "content" || k == "old_string" || k == "new_string";
                                    if is_code_param && !file_ext.is_empty() {
                                        let hl_lines = highlight_tool_code(&content_lines[..show], &file_ext, m, marker, marker_color);
                                        for hl_line in hl_lines {
                                            lines.push_meta(clamp_line(hl_line, width), km.clone());
                                        }
                                    } else {
                                        for (ci, cline) in content_lines.iter().take(show).enumerate() {
                                            lines.push_meta(clamp_line(Line::from(vec![
                                                Span::styled(format!("{}    {:>3} {} ", m, ci + 1, marker), Style::default().fg(marker_color)),
                                                Span::styled(cline.to_string(), param_style),
                                            ]), width), km.clone());
                                        }
                                    }
                                    if total > max_preview {
                                        let omit = format!("{}       … +{} more lines", m, total - max_preview);
                                        lines.push_meta(Line::from(Span::styled(omit, Style::default().fg(THEME.load().muted))), km.clone());
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
                                        lines.push_meta(Line::from(Span::styled(wline, param_style)), km.clone());
                                    }
                                }
                            } else {
                                let val = v.to_string();
                                let line_str = format!("{}     {}: {}", m, k, val);
                                for wline in wrap_text(&line_str, width) {
                                    lines.push_meta(Line::from(Span::styled(wline, param_style)), km.clone());
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

            ChatMessage::ToolResult { ref content, elapsed_ms, .. } => {
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

                // Check if this is read tool output (line-numbered) and try syntax highlighting
                // Skip fancy highlighting for timeouts — render everything in warning style
                let highlighted_lines = if is_timeout || is_error {
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
                if let Some(hl_lines) = highlighted_lines {
                    if !is_error && !is_timeout {
                        for (li, hl_line) in hl_lines.into_iter().enumerate() {
                            let dimmed_spans: Vec<Span> = hl_line.spans.into_iter().map(|span| {
                                Span::styled(span.content, span.style.add_modifier(Modifier::DIM))
                            }).collect();
                            lines.push_meta(
                                clamp_line(Line::from(dimmed_spans), width),
                                LineMeta::ContentLine { msg_idx: i, src_line: li },
                            );
                        }
                    } else {
                        for (li, hl_line) in hl_lines.into_iter().enumerate() {
                            lines.push_meta(
                                clamp_line(hl_line, width),
                                LineMeta::ContentLine { msg_idx: i, src_line: li },
                            );
                        }
                    }
                } else {
                    for (li, line) in result_lines[..show].iter().enumerate() {
                        // Try to detect and highlight grep output (skip for timeout/error)
                        if !is_timeout && !is_error {
                            if let Some(grep_spans) = try_highlight_grep_line(line, m) {
                                lines.push_meta(
                                    clamp_line(Line::from(grep_spans), width),
                                    LineMeta::ContentLine { msg_idx: i, src_line: li },
                                );
                                continue;
                            }
                        }
                        let full = format!("{}       {}", m, line);
                        for wline in wrap_text(&full, width) {
                            let body_style = if is_error || is_timeout { style } else { style.add_modifier(Modifier::DIM) };
                            lines.push_meta(
                                Line::from(Span::styled(wline, body_style)),
                                LineMeta::ContentLine { msg_idx: i, src_line: li },
                            );
                        }
                    }
                }
                if result_lines.len() > show {
                    // D4 (as locked): the "+N lines" truncation row stands in
                    // for the hidden tail — ContentLine anchored at the first
                    // hidden source line; slice (d)'s copy path reads a
                    // selection over it as "from src_line to end of content".
                    lines.push_meta(
                        Line::from(Span::styled(
                            format!("{}       +{} lines", m, result_lines.len() - show),
                            Style::default().fg(THEME.load().muted),
                        )),
                        LineMeta::ContentLine { msg_idx: i, src_line: show },
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
                            format!("{}     \u{2514}\u{2500} \u{26a0} timed out ({} lines)", m, result_lines.len()),
                            Style::default().fg(THEME.load().warning_color),
                        ),
                        Span::styled(
                            elapsed_str,
                            Style::default().fg(THEME.load().subagent_time),
                        ),
                    ]));
                } else if !is_error && show > 0 {
                    if self.is_active_tool_result(i) {
                        // Tool still executing — show animation only for the active result.
                        let elapsed_str = if let Some(start) = self.tool_start_time() {
                            let secs = start.elapsed().as_secs_f64();
                            if secs >= 1.0 { format!(" {:.1}s", secs) }
                            else { format!(" {}ms", (secs * 1000.0) as u64) }
                        } else { String::new() };

                        if preceding_tool.as_deref() == Some("bash") {
                            let (trace, color) = bash_trace(ctx.spinner_frame);
                            lines.push(Line::from(vec![
                                Span::styled(format!("{}     ", m), Style::default()),
                                Span::styled(format!("{}{}", trace, elapsed_str), Style::default().fg(color)),
                            ]));
                        } else {
                            let spinner_idx = (ctx.spinner_frame / 3) % SPINNER_FRAMES.len();
                            lines.push(Line::from(Span::styled(
                                format!("{}     {} running{}", m, SPINNER_FRAMES[spinner_idx], elapsed_str),
                                Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
                            )));
                        }
                    } else {
                        let elapsed_str = match elapsed_ms {
                            Some(ms) if *ms >= 1000 => format!(" {:.1}s", *ms as f64 / 1000.0),
                            Some(ms) => format!(" {}ms", ms),
                            None => String::new(),
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}     \u{2514}\u{2500} ok ({} lines)", m, result_lines.len()),
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
                        let body = row.text
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
                        let meta = classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                        lines.push_meta(Line::from(vec![
                            Span::styled(prefix, err_style),
                            Span::styled(body, err_style),
                        ]), meta);
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
                let style = Style::default().fg(THEME.load().muted).add_modifier(Modifier::DIM);
                for (src_line, line_off, line) in source_lines(msg) {
                    let prefix_len = m.len() + 2;
                    for row in wrap_text_spans(&format!("{}  {}", m, line), width) {
                        let meta = classify_row(&row.text, &row, line, line_off, prefix_len, i, src_line);
                        let text = row.text;
                        lines.push_meta(Line::from(Span::styled(text, style)), meta);
                    }
                }
            }

            ChatMessage::Event { source, severity, text } => {
                let theme = THEME.load();
                let (icon, sev_color) = match severity.as_str() {
                    "critical" => ("🔴", theme.event_critical),
                    "high"     => ("🟠", theme.event_icon),
                    "medium"   => ("🟡", theme.event_icon),
                    "low"      => ("🔵", theme.event_source),
                    _          => ("📨", theme.event_icon),
                };
                let event_bg = Color::Rgb(30, 35, 45);
                let bg = Style::default().bg(event_bg);
                // Top spacing
                lines.push(Line::from(""));
                // Top padding
                lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
                // Header: icon + source (severity is not rendered as a span)
                let header = format!("{}  {} [{}]", m, icon, source);
                let ts_str = format!("{} ", ts);
                let gap = width.saturating_sub(header.chars().count() + ts_str.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(format!("{}  {} ", m, icon), Style::default().fg(sev_color).bg(event_bg)),
                    Span::styled(format!("[{}]", source), Style::default().fg(theme.event_source).bg(event_bg).add_modifier(Modifier::BOLD)),
                    Span::styled(" ".repeat(gap).to_string(), Style::default().bg(event_bg)),
                    Span::styled(ts_str, Style::default().fg(theme.muted).bg(event_bg)),
                ]));
                // Content
                let text_style = Style::default().fg(theme.event_text).bg(event_bg);
                for (src_line, line_off, line) in source_lines(text) {
                    let prefix_len = m.len() + 2;
                    for row in wrap_text_spans(&format!("{}  {}", m, line), width) {
                        let rendered = format!("{:<width$}", row.text, width = width);
                        let meta = classify_row(&rendered, &row, line, line_off, prefix_len, i, src_line);
                        lines.push_meta(Line::from(Span::styled(rendered, text_style)), meta);
                    }
                }
                // Bottom padding
                lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
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
            lines.extend(self.render_message_lines(i, width, ctx).lines.expect("freshly rendered slot has lines"));
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
        RenderCtx { spinner_frame: 0, streaming: false, agent_name: "agent" }
    }

    fn flatten(line: &ratatui::text::Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
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
        let mut store = TranscriptStore::new();
        store.push_msg(ChatMessage::User(
            "hello there\nsecond user line that is long enough to wrap at forty columns easily".into(),
        ));
        store.push_msg(ChatMessage::Thinking("pondering the request at hand".into()));
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
        store.push_msg(ChatMessage::Error("boom happened\nwith a second error line".into()));
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
                    entry.meta.len(),
                    "meta must stay parallel to lines (msg {idx}, width {width})"
                );
                let mut prev_end = 0usize;
                for (row, meta) in entry.meta.iter().enumerate() {
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
                                source.is_char_boundary(range.start) && source.is_char_boundary(range.end),
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
        for (idx, wants_content) in [(0usize, true), (6, true), (5, true)] {
            let entry = store.render_message_lines(idx, 80, &ctx());
            assert_eq!(
                entry.meta.iter().any(|m| matches!(m, LineMeta::Content { .. })),
                wants_content,
                "msg {idx} should emit Content rows for its plain wrapped body"
            );
        }
        // ToolResult rows are line-mapped (L2).
        let entry = store.render_message_lines(4, 80, &ctx());
        assert!(
            entry.meta.iter().any(|m| matches!(m, LineMeta::ContentLine { .. })),
            "ToolResult content rows must be ContentLine-mapped"
        );
    }
}
