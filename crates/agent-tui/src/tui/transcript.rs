//! Transcript vocabulary types and the [`TranscriptStore`] shell.
//!
//! **P9 Slice (a):** types moved from `app.rs`; `TranscriptStore` holds only
//! `messages` for now. All other fields migrate in later slices per the P9
//! seam design. The field is `pub(crate)` — pure passthrough this slice.

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

/// Shell for the transcript store. Slice (a): holds only `messages`.
/// Remaining fields migrate in slices (b)–(f) per the P9 seam design.
pub(crate) struct TranscriptStore {
    pub(crate) messages: Vec<TimestampedMsg>,
}

impl TranscriptStore {
    pub(crate) fn new() -> Self {
        Self { messages: Vec::new() }
    }
}

impl Default for TranscriptStore {
    fn default() -> Self {
        Self::new()
    }
}
