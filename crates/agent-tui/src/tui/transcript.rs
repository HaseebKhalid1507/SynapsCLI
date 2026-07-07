//! Transcript vocabulary types and the [`TranscriptStore`] shell.
//!
//! **P9 Slice (a):** types moved from `app.rs`; `TranscriptStore` holds only
//! `messages` for now. All other fields migrate in later slices per the P9
//! seam design. The field is `pub(crate)` — pure passthrough this slice.
//!
//! **P9 Slice (c):** scroll state (`scroll_back`, `scroll_pinned`,
//! `last_line_count`) moves here; `scroll_up`, `scroll_down`,
//! `scroll_to_bottom` added. Fields are `pub(crate)` — sealing is slice (f).
//!
//! # Scroll / selection asymmetry (red-team finding)
//! `scroll_up`/`scroll_down` do **not** touch selection state. Mouse-wheel
//! scroll clears selection at the call site in `input.rs` (before calling
//! `scroll_up`/`scroll_down`) because that clearing is specific to wheel
//! events — `Shift+Up/Down` keyboard scroll intentionally preserves selection.
//! Do not unify the two call sites by folding `clear_selection` into these
//! methods.

/// Sentinel placeholder pushed into a `Thinking` block while the model is
/// deciding whether to think. Using ellipsis + zero-width space makes it
/// visually identical to "…" but never equal to real model output.
pub(crate) const THINKING_PLACEHOLDER: &str = "\u{2026}\u{200B}";

#[derive(Clone)]
pub(crate) enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    /// Streaming tool-use placeholder. `tool_id` lets the chat UI route
    /// subsequent input deltas and the final finalize event to *this*
    /// block when multiple tools run in parallel — without it, the
    /// "always update last message" hack misroutes deltas/results to
    /// whichever tool block happens to be most recent.
    ToolUseStart {
        tool_id: String,
        tool_name: String,
        partial_input: String,
    },
    ToolUse {
        tool_id: String,
        tool_name: String,
        input: String,
    },
    ToolResult {
        tool_id: String,
        content: String,
        elapsed_ms: Option<u64>,
    },
    Error(String),
    System(String),
    Event { source: String, severity: String, text: String },
}

pub(crate) struct TimestampedMsg {
    pub(crate) msg: ChatMessage,
    pub(crate) time: String,
}

/// Per-message render cache. Parallel to `TranscriptStore.messages`: each slot
/// holds the rendered `Vec<Line>` for that message. `flat` is their
/// concatenation, which is what downstream (draw/selection) consumes. The
/// `width` at which these were rendered is stored so stale entries can be
/// detected on terminal resize.
pub(crate) struct LineCache {
    pub(crate) width: usize,
    /// Rendered lines per message — index parallel to TranscriptStore.messages.
    pub(crate) per_msg: Vec<Vec<ratatui::text::Line<'static>>>,
    /// Concatenation of per_msg; what downstream code consumes.
    pub(crate) flat: Vec<ratatui::text::Line<'static>>,
}

/// Shell for the transcript store.
///
/// Slice (a): `messages`.
/// Slice (c): `scroll_back`, `scroll_pinned`, `last_line_count` + scroll API.
/// Fields are `pub(crate)` — sealing is slice (f).
pub(crate) struct TranscriptStore {
    pub(crate) messages: Vec<TimestampedMsg>,

    // ── Scroll state (moved in slice c) ──────────────────────────────────────
    /// Viewport offset from the bottom (0 = pinned to latest line).
    pub(crate) scroll_back: u16,
    /// When `true`, viewport stays pinned to the bottom (auto-scroll).
    /// Cleared when the user scrolls up; restored when they reach bottom.
    pub(crate) scroll_pinned: bool,
    /// Previous flat-line total — used to stabilise `scroll_back` when
    /// unpinned during streaming growth. See draw.rs §4 (growth-adjust block).
    pub(crate) last_line_count: usize,
}

impl TranscriptStore {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            scroll_back: 0,
            scroll_pinned: true,
            last_line_count: 0,
        }
    }

    // ── Scroll API ────────────────────────────────────────────────────────────
    //
    // These methods do NOT touch selection state — see module-level doc note
    // on the wheel-vs-Shift+Up asymmetry.

    /// Scroll up (away from bottom) by `lines`. Unpins the viewport.
    pub(crate) fn scroll_up(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_add(lines);
        self.scroll_pinned = false;
    }

    /// Scroll down (toward bottom) by `lines`. Re-pins at 0.
    pub(crate) fn scroll_down(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
        if self.scroll_back == 0 {
            self.scroll_pinned = true;
        }
    }

    /// Reset scroll to bottom and pin.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll_back = 0;
        self.scroll_pinned = true;
    }
}

impl Default for TranscriptStore {
    fn default() -> Self {
        Self::new()
    }
}
