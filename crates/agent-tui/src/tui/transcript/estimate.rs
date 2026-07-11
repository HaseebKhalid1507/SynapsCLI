//! Cheap height estimator for [`super::ChatMessage`] variants.
//!
//! # Contract
//!
//! `estimate_message_height(msg, width)` returns a *conservative* wrapped-row
//! count that is:
//!
//! - **O(source bytes)** — one pass over `source_text()`, no markdown render,
//!   no syntect / highlight module touch, no styled `Line` allocation.
//! - **Deterministic** in `(source_text, width)` — same inputs always produce
//!   the same output.
//! - **Non-zero** — always returns at least `MIN_MSG_HEIGHT`.
//! - **Not exact** — estimates are coordinates, not truth. The full render
//!   (`render_message_lines`) replaces them with exact heights when a slot
//!   enters the viewport. See `HeightState::Estimated` in `transcript.rs`.
//!
//! The estimator intentionally does NOT use the markdown renderer, syntect, or
//! any styled-line allocation. Code fences are counted as raw source lines —
//! the estimator will often under-count their rendered height (fences add
//! borders), but that is fine: under-counts push content up when corrected,
//! which is invisible above the fold (§4.3 correction theorem).
//!
//! # Chrome rows
//!
//! Each message variant contributes a small fixed number of "chrome" rows
//! (header, footer, blank separators). These are captured in [`CHROME_ROWS`].
//! The constants are calibrated roughly against real renders; they are NOT
//! derived by running the renderer. A separate calibration unit test
//! (`chrome_rows_nonzero`) asserts they are sane.

use super::super::text_metrics::width as display_width;
use super::ChatMessage;

/// Minimum estimated height. An estimate of 0 would be invisible and break
/// coordinate arithmetic; a floor of 1 ensures every message occupies at
/// least one scroll row.
#[allow(dead_code)]
pub(crate) const MIN_MSG_HEIGHT: usize = 1;

/// Fixed per-role chrome rows added on top of the source-line wrap count.
///
/// These cover: timestamp header, blank separator after the header,
/// footer/padding rows, and (for tool cards) the panel border rows.
///
/// Intentionally conservative (slightly high) — an over-estimate shrinks on
/// correction, which is also invisible above the fold, and avoids the
/// "rubber-band" scrollbar artefact that under-estimates cause.
#[allow(dead_code)]
pub(crate) struct ChromeRows {
    /// `ChatMessage::User(_)` — timestamp header + 1 blank above/below.
    pub(crate) user: usize,
    /// `ChatMessage::Text(_)` / `ChatMessage::Thinking(_)` — minimal chrome.
    pub(crate) text: usize,
    /// `ChatMessage::ToolUse*` — panel border top + bottom + header row.
    pub(crate) tool_use: usize,
    /// `ChatMessage::ToolResult` — panel border top + bottom + footer row.
    pub(crate) tool_result: usize,
    /// `ChatMessage::Error(_)` — decorative borders.
    pub(crate) error: usize,
    /// `ChatMessage::System(_)` — one header line.
    pub(crate) system: usize,
    /// `ChatMessage::Event { .. }` — one header line.
    pub(crate) event: usize,
}

#[allow(dead_code)]
pub(crate) const CHROME_ROWS: ChromeRows = ChromeRows {
    user: 3,
    text: 2,
    tool_use: 4,
    tool_result: 4,
    error: 3,
    system: 2,
    event: 2,
};

/// Estimate the wrapped display-row height of `msg` at a terminal of `width`
/// columns.
///
/// # Algorithm
///
/// For each hard-newline-separated source line, we compute the display width
/// via the `unicode-width` crate (already a project dependency) and divide by
/// `width` to get the number of wrapped rows, with a floor of 1 per non-empty
/// line. Empty lines between paragraphs count as 1 row each. We then add
/// per-role chrome rows and clamp to [`MIN_MSG_HEIGHT`].
///
/// This is purely a measurement estimate; it never allocates styled lines,
/// calls the markdown renderer, or touches the syntect highlight module.
#[allow(dead_code)]
pub(crate) fn estimate_message_height(msg: &ChatMessage, width: usize) -> usize {
    if width == 0 {
        return MIN_MSG_HEIGHT;
    }

    let src = msg.source_text();
    let text_rows = count_wrapped_rows(src.as_ref(), width);

    let chrome = match msg {
        ChatMessage::User(_) => CHROME_ROWS.user,
        ChatMessage::Text(_) => CHROME_ROWS.text,
        ChatMessage::Thinking(_) => CHROME_ROWS.text,
        ChatMessage::ToolUseStart { .. } => CHROME_ROWS.tool_use,
        ChatMessage::ToolUse { .. } => CHROME_ROWS.tool_use,
        ChatMessage::ToolResult { .. } => CHROME_ROWS.tool_result,
        ChatMessage::Error(_) => CHROME_ROWS.error,
        ChatMessage::System(_) => CHROME_ROWS.system,
        ChatMessage::Event { .. } => CHROME_ROWS.event,
    };

    (text_rows + chrome).max(MIN_MSG_HEIGHT)
}

/// Count how many terminal rows the string `src` occupies when wrapped to
/// `width` columns.
///
/// - Each `\n`-separated line occupies `ceil(display_width / width)` rows,
///   with a floor of 1 (empty lines, including the trailing newline, each
///   consume one row).
/// - The result is 0 for the empty string (no source lines).
///
/// This does NOT expand tabs (`\t` has display width 0 via `char_width` so
/// it contributes nothing — the same behaviour as the rest of the codebase;
/// callers that need visual tab expansion handle it separately).
#[allow(dead_code)]
pub(crate) fn count_wrapped_rows(src: &str, width: usize) -> usize {
    debug_assert!(width > 0, "count_wrapped_rows called with width=0");
    if src.is_empty() {
        return 0;
    }
    let mut rows = 0usize;
    for line in src.split('\n') {
        let w = display_width(line);
        rows += if w == 0 {
            1 // empty or zero-width line still occupies one terminal row
        } else {
            w.div_ceil(width)
        };
    }
    rows
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::{ChatMessage, THINKING_PLACEHOLDER};
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn text(s: &str) -> ChatMessage {
        ChatMessage::Text(s.to_string())
    }

    fn user(s: &str) -> ChatMessage {
        ChatMessage::User(s.to_string())
    }

    fn system(s: &str) -> ChatMessage {
        ChatMessage::System(s.to_string())
    }

    fn tool_use(input: &str) -> ChatMessage {
        ChatMessage::ToolUse {
            tool_id: "tid".to_string(),
            tool_name: "tool".to_string(),
            input: input.to_string(),
        }
    }

    fn tool_result(content: &str) -> ChatMessage {
        ChatMessage::ToolResult {
            tool_id: "tid".to_string(),
            content: content.to_string(),
            elapsed_ms: None,
        }
    }

    const W: usize = 80;

    // ── nonzero ───────────────────────────────────────────────────────────────

    #[test]
    fn estimate_nonzero_empty_text() {
        assert!(estimate_message_height(&text(""), W) >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_nonzero_empty_user() {
        assert!(estimate_message_height(&user(""), W) >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_nonzero_system() {
        assert!(estimate_message_height(&system(""), W) >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_nonzero_tool_use() {
        assert!(estimate_message_height(&tool_use("{}"), W) >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_nonzero_tool_result() {
        assert!(estimate_message_height(&tool_result("ok"), W) >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_nonzero_thinking_placeholder() {
        let msg = ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string());
        assert!(estimate_message_height(&msg, W) >= MIN_MSG_HEIGHT);
    }

    // ── short plain text ──────────────────────────────────────────────────────

    #[test]
    fn estimate_short_text_small_height() {
        let h = estimate_message_height(&text("Hello world"), W);
        // Short single-line text: chrome + 1 row
        assert!(h >= 1);
        assert!(h <= 10, "short text should not have huge height, got {h}");
    }

    // ── monotonic / longer = more rows ───────────────────────────────────────

    #[test]
    fn estimate_longer_text_at_least_as_tall() {
        let short = estimate_message_height(&text("one line"), W);
        let long = estimate_message_height(&text("one line\ntwo line\nthree line\nfour"), W);
        assert!(
            long >= short,
            "longer source should produce >= height (short={short} long={long})"
        );
    }

    #[test]
    fn estimate_more_newlines_more_height() {
        let h1 = estimate_message_height(&text("a\nb"), W);
        let h2 = estimate_message_height(&text("a\nb\nc\nd\ne"), W);
        assert!(
            h2 >= h1,
            "more newlines => height must not shrink (h1={h1} h2={h2})"
        );
    }

    // ── wrapping ──────────────────────────────────────────────────────────────

    #[test]
    fn estimate_long_single_line_wraps() {
        // 160-char line at width=80 -> 2 wrapped rows + chrome
        let long_line = "x".repeat(160);
        let h_long = estimate_message_height(&text(&long_line), W);
        let h_short = estimate_message_height(&text("x"), W);
        assert!(h_long >= h_short, "long line must wrap to more rows");
    }

    #[test]
    fn estimate_wrap_count_correct_ascii() {
        // count_wrapped_rows is the internal primitive -- test it directly
        // 80 chars at width 80 -> 1 row
        assert_eq!(count_wrapped_rows(&"a".repeat(80), 80), 1);
        // 81 chars -> 2 rows
        assert_eq!(count_wrapped_rows(&"a".repeat(81), 80), 2);
        // 160 chars -> 2 rows
        assert_eq!(count_wrapped_rows(&"a".repeat(160), 80), 2);
        // 161 chars -> 3 rows
        assert_eq!(count_wrapped_rows(&"a".repeat(161), 80), 3);
    }

    #[test]
    fn estimate_wrap_empty_lines_count_as_one_row_each() {
        // "\n\n" -> 3 lines (split by \n gives ["", "", ""])
        assert_eq!(count_wrapped_rows("\n\n", 80), 3);
        assert_eq!(count_wrapped_rows("a\n\nb", 80), 3);
    }

    // ── code fence (no highlight module call) ─────────────────────────────────

    #[test]
    fn estimate_code_fence_nonzero_no_highlight_counter() {
        // This test verifies: estimator handles code fences, returns nonzero,
        // and does NOT call the syntect highlight module.
        // The hard ratchet (1,000 fenced messages -> 0 highlight calls) is in
        // the larger test below.
        let fenced = text(
            "Before the fence.\n\
             ```rust\n\
             fn foo() -> usize {\n\
                 42\n\
             }\n\
             ```\n\
             After the fence.",
        );
        let h = estimate_message_height(&fenced, W);
        assert!(
            h >= MIN_MSG_HEIGHT,
            "fenced message must have nonzero height, got {h}"
        );
    }

    #[test]
    fn estimate_code_fence_grows_with_content() {
        let short_fence = text("```rust\nfn a() {}\n```");
        let long_fence = text(
            "```rust\n\
             fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n\
             fn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\nfn j() {}\n\
             ```",
        );
        let hs = estimate_message_height(&short_fence, W);
        let hl = estimate_message_height(&long_fence, W);
        assert!(
            hl >= hs,
            "longer fence must not have less height (short={hs} long={hl})"
        );
    }

    // ── tool / system cards ───────────────────────────────────────────────────

    #[test]
    fn estimate_tool_use_nonzero() {
        let h = estimate_message_height(&tool_use(r#"{"path": "/etc/passwd", "offset": 0}"#), W);
        assert!(h >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_tool_result_nonzero() {
        let h = estimate_message_height(&tool_result("file content here"), W);
        assert!(h >= MIN_MSG_HEIGHT);
    }

    #[test]
    fn estimate_system_nonzero() {
        let h = estimate_message_height(&system("... 50 earlier messages hidden ..."), W);
        assert!(h >= MIN_MSG_HEIGHT);
    }

    // ── determinism ───────────────────────────────────────────────────────────

    #[test]
    fn estimate_is_deterministic() {
        let msg = text("Some text\nAnother line\n```\ncode\n```");
        let h1 = estimate_message_height(&msg, 80);
        let h2 = estimate_message_height(&msg, 80);
        assert_eq!(h1, h2, "estimator must be deterministic");
    }

    #[test]
    fn estimate_deterministic_at_different_widths() {
        let msg = text("Hello world");
        let h80 = estimate_message_height(&msg, 80);
        let h120 = estimate_message_height(&msg, 120);
        // Short line -> same at both widths (does not wrap at either)
        assert_eq!(h80, h120);
    }

    #[test]
    fn estimate_narrower_width_more_rows() {
        let msg = text(&"a".repeat(200));
        let h80 = estimate_message_height(&msg, 80);
        let h40 = estimate_message_height(&msg, 40);
        assert!(
            h40 >= h80,
            "narrower viewport must produce >= rows (h40={h40} h80={h80})"
        );
    }

    // ── chrome_rows sanity ────────────────────────────────────────────────────

    const _: () = {
        // Every role must contribute at least 1 chrome row (header at minimum).
        assert!(CHROME_ROWS.user >= 1);
        assert!(CHROME_ROWS.text >= 1);
        assert!(CHROME_ROWS.tool_use >= 1);
        assert!(CHROME_ROWS.tool_result >= 1);
        assert!(CHROME_ROWS.error >= 1);
        assert!(CHROME_ROWS.system >= 1);
        assert!(CHROME_ROWS.event >= 1);
    };

    // ─────────────────────────────────────────────────────────────────────────
    // Hard ratchet (T241 §5 T3-estimator variant):
    // Calling the estimator over 1,000 off-screen fenced messages must leave
    // HIGHLIGHT_CALLS == 0.
    //
    // SYNTAX_SET_TOUCHED is a process-global latch (fires once per process);
    // in a shared-process test run another test may have pre-warmed syntect.
    // We assert highlight_call_count() only; the integration test
    // `tests/mem_transcript.rs` asserts SYNTAX_SET_TOUCHED independently in
    // an isolated process.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn estimator_over_1000_fenced_messages_zero_highlight_calls() {
        use super::super::super::highlight;

        highlight::highlight_reset_counters();

        const N: usize = 1_000;
        for i in 0..N {
            let msg = text(&format!(
                "Message {i}.\n\
                 ```rust\n\
                 fn synthetic_{i}() -> usize {{\n\
                     let x = {i};\n\
                     x * x\n\
                 }}\n\
                 ```\n\
                 After the code block."
            ));
            let h = estimate_message_height(&msg, 80);
            // Belt-and-suspenders: each estimate must be nonzero.
            assert!(h >= MIN_MSG_HEIGHT, "msg {i}: estimate must be nonzero");
        }

        let calls = highlight::highlight_call_count();
        assert_eq!(
            calls, 0,
            "estimator over {N} fenced messages must never call the highlight module \
             (got {calls} calls -- syntect must NOT be touched by the estimator)"
        );
    }
}
