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
/// Slice (b′): content mutations + tool timers + invalidate family +
/// `line_cache`/`dirty_from`. The invalidate family here mutates ONLY store
/// state — redraw signaling (`needs_redraw`) stays on App via the thin
/// delegating wrappers in app.rs. Fields are `pub(crate)` — sealing is
/// slice (f); the cache tri-state → `CacheState` enum conversion is slice (d).
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

    // ── Render cache (moved in slice b′) ─────────────────────────────────────
    /// Cached wrapped+highlighted message lines.
    /// `None` means "stale — rebuild on next draw". `Some(cache)` means
    /// "valid at cache.width". Tri-state with `dirty_from`:
    /// None + None = full rebuild; Some + None = clean; Some + Some(k) =
    /// incremental from k. Slice (d) converts this to an explicit enum.
    pub(crate) line_cache: Option<LineCache>,
    /// Lowest message index whose rendered lines are stale. `None` = fully clean.
    /// Set to `Some(k)` to trigger partial re-render from message k on next draw.
    pub(crate) dirty_from: Option<usize>,

    // ── Tool timing (moved in slice b′; locked decision #2) ──────────────────
    /// Tracks when the current tool started executing (for elapsed time display)
    pub(crate) tool_start_time: Option<std::time::Instant>,
    /// Per-tool start times keyed by `tool_id`. Lets parallel tool calls
    /// each show their own elapsed-time on the result block, instead of
    /// sharing a single timer that the last-started tool clobbers.
    pub(crate) tool_start_times: std::collections::HashMap<String, std::time::Instant>,
}

impl TranscriptStore {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            scroll_back: 0,
            scroll_pinned: true,
            last_line_count: 0,
            line_cache: None,
            dirty_from: None,
            tool_start_time: None,
            tool_start_times: std::collections::HashMap::new(),
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

    // ── Content mutations (moved in slice b′) ────────────────────────────────
    //
    // Bodies verbatim from app.rs. The invalidate family mutates ONLY
    // line_cache/dirty_from here — `needs_redraw` signaling lives in the
    // App-side wrappers (locked decision #3).

    pub(crate) fn push_msg(&mut self, msg: ChatMessage) {
        self.messages.push(TimestampedMsg {
            msg,
            time: chrono::Local::now().format("%H:%M").to_string(),
        });
        // Auto-scroll only when pinned to bottom
        if self.scroll_pinned {
            self.scroll_back = 0;
        }
        // New tail message — mark last slot dirty (append to per_msg on next rebuild).
        self.invalidate_last();
    }

    /// Insert a freshly created `ToolResult` directly beneath its matching
    /// `ToolUse` / `ToolUseStart` block (matched by `tool_id`) so parallel tool
    /// calls render as **input → its output** pairs, instead of all inputs
    /// stacked then all outputs stacked. Falls back to appending at the end
    /// when no matching tool_use exists (legacy providers without tool_ids).
    pub(crate) fn push_tool_result(&mut self, tool_id: String, content: String, elapsed_ms: Option<u64>) {
        let use_idx = if tool_id.is_empty() {
            None
        } else {
            // Invariant: each tool_id should appear at most once. Assert in
            // debug builds so duplicate IDs surface immediately.
            debug_assert!(
                self.messages.iter().filter(|m| matches!(
                    &m.msg,
                    ChatMessage::ToolUse { tool_id: tid, .. }
                    | ChatMessage::ToolUseStart { tool_id: tid, .. }
                        if tid == &tool_id
                )).count() <= 1,
                "push_tool_result: duplicate ToolUse/ToolUseStart for tool_id={tool_id:?}"
            );
            self.messages.iter().position(|m| matches!(
                &m.msg,
                ChatMessage::ToolUse { tool_id: tid, .. }
                | ChatMessage::ToolUseStart { tool_id: tid, .. }
                    if tid == &tool_id
            ))
        };
        let msg = ChatMessage::ToolResult { tool_id, content, elapsed_ms };
        match use_idx {
            Some(i) => {
                let at = (i + 1).min(self.messages.len());
                self.messages.insert(at, TimestampedMsg {
                    msg,
                    time: chrono::Local::now().format("%H:%M").to_string(),
                });
                if self.scroll_pinned {
                    self.scroll_back = 0;
                }
                // Insert mid-list — invalidate_from(at) so draw re-renders from insert point.
                self.invalidate_from(at);
            }
            None => self.push_msg(msg),
        }
    }
    ///
    /// `render_lines` markdown-renders + syntax-highlights EVERY display message
    /// on the first frame. On `--continue` of a long session (hundreds of
    /// messages) that made boot crawl. This keeps only the most recent `cap`
    /// display messages and prepends a notice — the FULL history is untouched in
    /// `api_messages`, so the model still sees everything; only the visible
    /// scrollback is trimmed. (Proper fix is viewport virtualization — #98.)
    pub(crate) fn cap_resumed_display(&mut self, cap: usize) {
        if self.messages.len() <= cap {
            return;
        }
        let omitted = self.messages.len() - cap;
        self.messages.drain(0..omitted);
        self.messages.insert(
            0,
            TimestampedMsg {
                msg: ChatMessage::System(format!(
                    "… {omitted} earlier message(s) hidden to speed resume — full history is still in the model's context"
                )),
                time: chrono::Local::now().format("%H:%M").to_string(),
            },
        );
    }

    // ── Invalidate family (moved in slice b′) ────────────────────────────────

    /// Mark the cached message lines stale — they'll be rebuilt on the next draw.
    /// Call this after any mutation that changes how `messages` renders.
    /// Use for structural changes (theme, width, message list reshuffle). For
    /// streaming deltas prefer `invalidate_last()` which is O(1).
    pub(crate) fn invalidate(&mut self) {
        self.line_cache = None;
        self.dirty_from = None;
    }

    /// Mark messages from index `idx` onwards as dirty (cheapest granularity).
    /// Coalesces with any existing dirty_from by taking the minimum.
    pub(crate) fn invalidate_from(&mut self, idx: usize) {
        self.dirty_from = Some(match self.dirty_from {
            Some(k) => k.min(idx),
            None => idx,
        });
    }

    /// Mark only the tail message dirty. O(1) during streaming.
    pub(crate) fn invalidate_last(&mut self) {
        self.invalidate_from(self.messages.len().saturating_sub(1));
    }

    // ── Tool state queries + find_* helpers (moved in slice b′) ──────────────

    /// Returns true when the chat message at `idx` is the tool result currently
    /// being streamed/executed. Completed historical tool results must render as
    /// done even while a later tool call is active.
    pub(crate) fn is_active_tool_result(&self, idx: usize) -> bool {
        if self.tool_start_time.is_none() {
            return false;
        }
        match self.messages.get(idx).map(|m| &m.msg) {
            Some(ChatMessage::ToolResult { tool_id, elapsed_ms: None, .. }) => {
                self.tool_start_times.contains_key(tool_id)
            }
            _ => false,
        }
    }

    /// Find the file extension from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_read_extension(&self, idx: usize) -> String {
        // Prefer matching by tool_id when the result carries one — under
        // parallel tool calls a `ToolResult` may not be positionally
        // adjacent to its matching `ToolUse`.
        let target_id: Option<String> = match self.messages.get(idx).map(|m| &m.msg) {
            Some(ChatMessage::ToolResult { tool_id, .. }) if !tool_id.is_empty() => Some(tool_id.clone()),
            _ => None,
        };
        if let Some(id) = target_id {
            for m in self.messages.iter() {
                if let ChatMessage::ToolUse { tool_id, tool_name, input } = &m.msg {
                    if tool_id == &id && tool_name == "read" {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                            if let Some(path) = parsed["path"].as_str() {
                                if let Some(ext) = std::path::Path::new(path).extension() {
                                    return ext.to_string_lossy().to_string();
                                }
                            }
                        }
                        return String::new();
                    }
                }
            }
        }
        // Fallback: walk backwards from idx to find the preceding ToolUse
        if idx == 0 { return String::new(); }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, ref input, .. } = self.messages[i].msg {
                if tool_name == "read" {
                    // Extract path from the JSON input
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(path) = parsed["path"].as_str() {
                            // Get the extension
                            if let Some(ext) = std::path::Path::new(path).extension() {
                                return ext.to_string_lossy().to_string();
                            }
                        }
                    }
                }
                break; // Stop at first ToolUse regardless
            }
        }
        String::new()
    }

    /// Find the tool name from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_tool_name(&self, idx: usize) -> Option<String> {
        // Prefer matching by tool_id when the result carries one — this
        // is the only way to render parallel-tool outputs correctly,
        // since results may not be positionally adjacent to their
        // matching tool_use.
        if let Some(ChatMessage::ToolResult { tool_id, .. }) = self.messages.get(idx).map(|m| &m.msg) {
            if !tool_id.is_empty() {
                for m in self.messages.iter() {
                    match &m.msg {
                        ChatMessage::ToolUse { tool_id: tid, tool_name, .. }
                        | ChatMessage::ToolUseStart { tool_id: tid, tool_name, .. }
                            if tid == tool_id =>
                        {
                            return Some(tool_name.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        if idx == 0 { return None; }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
            if let ChatMessage::ToolUseStart { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
        }
        None
    }

    // ── Tool-event routing (moved in slice b′) ──────────────────────────────
    //
    // Stream events arrive interleaved when the model fans out parallel
    // tool calls. The chat UI keeps a flat `Vec<ChatMessage>`, so to keep
    // each on-screen tool block correct we must route every delta /
    // finalize / result event back to the block whose `tool_id` matches.
    //
    // The earlier "always update last message" approach worked for
    // sequential tool calls but corrupts state under parallelism — input
    // deltas from tool A would land on tool B's `ToolUseStart`, and the
    // first arriving result would be silently overwritten by the second.

    /// Locate the index of a `ToolUseStart` block with this `tool_id`.
    pub(crate) fn find_tool_use_start_idx(&self, tool_id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| matches!(
            &m.msg,
            ChatMessage::ToolUseStart { tool_id: tid, .. } if tid == tool_id
        ))
    }

    /// Locate the latest `ToolResult` block for this `tool_id`.
    pub(crate) fn find_tool_result_idx(&self, tool_id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| matches!(
            &m.msg,
            ChatMessage::ToolResult { tool_id: tid, .. } if tid == tool_id
        ))
    }

    /// Begin streaming a new tool call. Records start time per-tool so
    /// elapsed-ms is correct under parallel execution.
    pub(crate) fn on_tool_use_start(&mut self, tool_id: String, tool_name: String) {
        self.drop_empty_thinking();
        let now = std::time::Instant::now();
        self.tool_start_time = Some(now);
        if !tool_id.is_empty() {
            self.tool_start_times.insert(tool_id.clone(), now);
        }
        self.push_msg(ChatMessage::ToolUseStart {
            tool_id,
            tool_name,
            partial_input: String::new(),
        });
    }

    /// Append a chunk of the tool's input JSON to the matching
    /// `ToolUseStart` block. Falls back to "last ToolUseStart" only when
    /// the event lacks a tool_id (legacy paths).
    pub(crate) fn on_tool_use_delta(&mut self, tool_id: &str, delta: &str) {
        let target_idx = if !tool_id.is_empty() {
            self.find_tool_use_start_idx(tool_id)
        } else {
            self.messages.iter().rposition(|m| matches!(&m.msg, ChatMessage::ToolUseStart { .. }))
        };
        if let Some(idx) = target_idx {
            if let ChatMessage::ToolUseStart { ref mut partial_input, .. } = self.messages[idx].msg {
                partial_input.push_str(delta);
                self.invalidate();
            }
        }
    }

    /// Finalize a streaming tool call. Replaces the matching
    /// `ToolUseStart` in place — keeping its position so on-screen order
    /// matches the order the model emitted the calls.
    pub(crate) fn on_tool_use_finalized(&mut self, tool_id: String, tool_name: String, input_str: String) {
        self.drop_empty_thinking();
        // Track start time even if we never saw a ToolUseStart (some
        // providers go straight to a finalized tool_use).
        if !tool_id.is_empty() {
            self.tool_start_times.entry(tool_id.clone()).or_insert_with(std::time::Instant::now);
        }
        self.tool_start_time = Some(std::time::Instant::now());

        if let Some(idx) = self.find_tool_use_start_idx(&tool_id) {
            self.messages[idx].msg = ChatMessage::ToolUse { tool_id, tool_name, input: input_str };
            self.invalidate();
            return;
        }
        // No matching start (e.g. provider only emits finalized blocks) —
        // append a new finalized block at the end.
        self.push_msg(ChatMessage::ToolUse { tool_id, tool_name, input: input_str });
    }

    /// Stream a chunk of tool output. Appends to the matching
    /// `ToolResult` if one exists, otherwise creates a new placeholder.
    pub(crate) fn on_tool_result_delta(&mut self, tool_id: String, delta: String) {
        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult { ref mut content, elapsed_ms, .. } = self.messages[idx].msg {
                if elapsed_ms.is_none() {
                    content.push_str(&delta);
                    self.invalidate();
                    return;
                }
            }
        }
        self.push_tool_result(tool_id, delta, None);
    }

    /// Finalize a tool result. Replaces any in-flight `ToolResult` for
    /// this `tool_id` (including a delta-buffered one) and stamps the
    /// elapsed time using the per-tool start time.
    pub(crate) fn on_tool_result(&mut self, tool_id: String, result: String) {
        let elapsed = self
            .tool_start_times
            .remove(&tool_id)
            .map(|t| t.elapsed().as_millis() as u64);
        // Clear the shared "active tool" timer once the *latest* tool
        // finishes — otherwise the bash trace animation lingers.
        if self.tool_start_times.is_empty() {
            self.tool_start_time = None;
        }

        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult { ref mut content, elapsed_ms, .. } = self.messages[idx].msg {
                if elapsed_ms.is_none() {
                    *content = result;
                    self.messages[idx].msg = ChatMessage::ToolResult {
                        tool_id,
                        content: std::mem::take(content),
                        elapsed_ms: elapsed,
                    };
                    self.invalidate();
                    return;
                }
            }
        }
        self.push_tool_result(tool_id, result, elapsed);
    }

    // ── Streaming text mutations (moved in slice b′) ─────────────────────────

    pub(crate) fn append_or_update_text(&mut self, text: &str) {
        // Model produced real output — clear any empty thinking placeholder
        // so its spinner stops.
        self.drop_empty_thinking();
        if let Some(TimestampedMsg { msg: ChatMessage::Text(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.invalidate_last();
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            // First real delta replaces the sentinel placeholder rather than
            // appending to it (otherwise content reads "…​<thinking>").
            if existing == THINKING_PLACEHOLDER {
                *existing = text.to_string();
            } else {
                existing.push_str(text);
            }
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
        self.invalidate_last();
    }

    /// Remove a trailing thinking block that never received content — the
    /// sentinel placeholder, or one left empty. Called when the model starts
    /// producing real output or the turn ends, so the thinking spinner can't
    /// run forever on an empty thinking step.
    ///
    /// Scans backward past trailing System/Notice messages so a placeholder
    /// stranded mid-list (e.g. when a Notice arrives on top of it) is still
    /// found and removed (FIX 4).
    pub(crate) fn drop_empty_thinking(&mut self) {
        // Walk from the tail, skipping System messages, to find the first
        // non-System message. If it's an empty/placeholder Thinking, drop it.
        let candidate_idx = self.messages.iter().rposition(|m| {
            !matches!(&m.msg, ChatMessage::System(_))
        });
        if let Some(idx) = candidate_idx {
            if let ChatMessage::Thinking(t) = &self.messages[idx].msg {
                if t == THINKING_PLACEHOLDER || t.is_empty() {
                    self.messages.remove(idx);
                    // Structural change (remove) — full invalidate to resync per_msg lengths.
                    self.invalidate();
                }
            }
        }
    }
}

impl Default for TranscriptStore {
    fn default() -> Self {
        Self::new()
    }
}
