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

/// Read-only App state the transcript renderer needs. Constructed fresh at
/// each cache-sync call site — cheap (a usize, a bool, a &str). This makes
/// the store's one rendering impurity visible in the signature instead of
/// hidden in `self`.
///
/// Exactly three fields (locked decision #1 moved `show_full_output` into
/// the store; locked decision #2 moved the tool timers): verified by grep —
/// the renderer's only remaining App reads are `spinner_frame` (×8),
/// `streaming` (×1) and `agent_name` (×1).
pub(crate) struct RenderCtx<'a> {
    pub(crate) spinner_frame: usize,
    pub(crate) streaming: bool,
    pub(crate) agent_name: &'a str,
}

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

/// Cache lifecycle for the per-message line cache. Replaces the old
/// `line_cache: Option<LineCache>` + `dirty_from: Option<usize>` tri-state
/// with the same semantics made explicit (slice d):
///
/// - `Missing`      = full rebuild on next sync (old `None` + `None`)
/// - `Clean(c)`     = serve as-is                (old `Some` + `None`)
/// - `Dirty(c, k)`  = incremental re-render from message index `k`
///                    (old `Some` + `Some(k)`)
///
/// Width mismatch (`c.width != content_width`) still forces a full rebuild
/// regardless of Clean/Dirty — that check lives in `sync_cache`.
/// `pub(crate)` until slice (f) seals the store.
pub(crate) enum CacheState {
    Missing,
    Clean(LineCache),
    Dirty(LineCache, usize),
}

impl CacheState {
    /// The cache, if one exists (Clean or Dirty). Mirrors the old
    /// `line_cache.as_ref()`.
    pub(crate) fn line_cache(&self) -> Option<&LineCache> {
        match self {
            CacheState::Missing => None,
            CacheState::Clean(c) | CacheState::Dirty(c, _) => Some(c),
        }
    }

    /// Mutable access to the cache, if one exists. Mirrors the old
    /// `line_cache.as_mut()`. Test-side escape hatch until slice (f).
    #[cfg(test)]
    pub(crate) fn line_cache_mut(&mut self) -> Option<&mut LineCache> {
        match self {
            CacheState::Missing => None,
            CacheState::Clean(c) | CacheState::Dirty(c, _) => Some(c),
        }
    }

    /// The incremental watermark, if dirty. Mirrors the old `dirty_from`.
    /// Test-side inspection until slice (f).
    #[cfg(test)]
    pub(crate) fn dirty_from(&self) -> Option<usize> {
        match self {
            CacheState::Dirty(_, k) => Some(*k),
            _ => None,
        }
    }

    /// Dirty → Clean keeping the cache; Missing/Clean unchanged. Mirrors the
    /// old `dirty_from = None` (watermark consumed) transition.
    #[cfg(test)]
    pub(crate) fn mark_clean(&mut self) {
        if matches!(self, CacheState::Dirty(..)) {
            let CacheState::Dirty(c, _) = std::mem::replace(self, CacheState::Missing) else {
                unreachable!()
            };
            *self = CacheState::Clean(c);
        }
    }
}

/// Shell for the transcript store.
///
/// Slice (a): `messages`.
/// Slice (c): `scroll_back`, `scroll_pinned`, `last_line_count` + scroll API.
/// Slice (b′): content mutations + tool timers + invalidate family + the
/// render cache. The invalidate family here mutates ONLY store
/// state — redraw signaling (`needs_redraw`) stays on App via the thin
/// delegating wrappers in app.rs. Fields are `pub(crate)` — sealing is
/// slice (f).
/// Slice (d): the cache tri-state became [`CacheState`]; the renderer
/// (`render_message_lines`, render.rs) moved into this impl with
/// [`RenderCtx`] threading; `sync_cache` folds in the draw.rs cache-sync
/// block; `show_full_output` is store-owned (locked decision #1).
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

    // ── Render cache (moved in slice b′; enum shape since slice d) ───────────
    /// Cached wrapped+highlighted message lines + incremental watermark.
    /// See [`CacheState`] for the lifecycle. `pub(crate)` until slice (f).
    pub(crate) cache: CacheState,

    // ── Render options (moved in slice d; locked decision #1) ────────────────
    /// Ctrl+O toggle: show full tool output instead of the truncated preview.
    /// Store-owned because it changes cached line content — mutate only via
    /// [`Self::set_show_full_output`], which invalidates internally.
    show_full_output: bool,

    // ── Tool timing (moved in slice b′; locked decision #2) ──────────────────
    /// Tracks when the current tool started executing (for elapsed time display)
    pub(crate) tool_start_time: Option<std::time::Instant>,
    /// Per-tool start times keyed by `tool_id`. Lets parallel tool calls
    /// each show their own elapsed-time on the result block, instead of
    /// sharing a single timer that the last-started tool clobbers.
    pub(crate) tool_start_times: std::collections::HashMap<String, std::time::Instant>,

    // ── Viewport geometry (moved in slice e) ─────────────────────────────────
    /// Inner content rect of the message area as of the last
    /// [`Self::visible_window`] call (was `App.msg_area_rect`; store-side name
    /// per design §1a, locked decision #4). `None` until the first render —
    /// `hit_test` returns `false` before that.
    pub(crate) viewport: Option<ratatui::layout::Rect>,
    /// Flat-cache index range visible in the viewport (was
    /// `App.visible_line_range`). Consumed only by `selected_text`.
    pub(crate) visible_range: Option<(usize, usize)>,

    // ── Selection (moved in slice e; terminal coords in P9, content-relative in P10) ──
    pub(crate) selection_anchor: Option<(u16, u16)>,
    pub(crate) selection_end: Option<(u16, u16)>,
}

/// Everything `build_render_model` needs from the transcript, computed in one
/// call ([`TranscriptStore::visible_window`]). Fully owned — no borrows into
/// the store reach the render thread (extends render_model.rs's "zero borrows
/// back into App" invariant to the store).
pub(crate) struct VisibleWindow {
    /// O(viewport) clone of the visible slice — same Arc the RenderModel ships.
    pub(crate) lines: std::sync::Arc<[ratatui::text::Line<'static>]>,
    pub(crate) lines_width: usize,
    /// Post-clamp scroll offset — what the scroll indicator shows.
    pub(crate) scroll_back: u16,
    pub(crate) selection: Option<(u16, u16, u16, u16)>,
    /// Drives logo visibility.
    pub(crate) is_empty: bool,
}

impl TranscriptStore {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            scroll_back: 0,
            scroll_pinned: true,
            last_line_count: 0,
            cache: CacheState::Missing,
            show_full_output: false,
            tool_start_time: None,
            tool_start_times: std::collections::HashMap::new(),
            viewport: None,
            visible_range: None,
            selection_anchor: None,
            selection_end: None,
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
        self.cache = CacheState::Missing;
    }

    /// Mark messages from index `idx` onwards as dirty (cheapest granularity).
    /// Coalesces with any existing watermark by taking the minimum. A missing
    /// cache stays missing — the full rebuild subsumes any watermark (same as
    /// the old `None` cache + `Some(k)` state, which the rebuild path wiped).
    pub(crate) fn invalidate_from(&mut self, idx: usize) {
        self.cache = match std::mem::replace(&mut self.cache, CacheState::Missing) {
            CacheState::Missing => CacheState::Missing,
            CacheState::Clean(c) => CacheState::Dirty(c, idx),
            CacheState::Dirty(c, k) => CacheState::Dirty(c, k.min(idx)),
        };
    }

    /// Mark only the tail message dirty. O(1) during streaming.
    pub(crate) fn invalidate_last(&mut self) {
        self.invalidate_from(self.messages.len().saturating_sub(1));
    }

    // ── Cache access + maintenance (slice d) ─────────────────────────────────

    /// The line cache, if one exists (Clean or Dirty). Mirrors the old
    /// `line_cache.as_ref()` reads in draw.rs / `selected_text`.
    pub(crate) fn line_cache(&self) -> Option<&LineCache> {
        self.cache.line_cache()
    }

    /// Sync the line cache to `content_width`: full rebuild on width change or
    /// missing cache, incremental re-render from the dirty watermark otherwise.
    /// Bodies verbatim from draw.rs §3 (the old lines 517–575), including the
    /// two-phase immutable-render-then-mutable-apply structure — borrow rules
    /// are identical inside the store (design §3.5).
    pub(crate) fn sync_cache(&mut self, content_width: usize, ctx: &RenderCtx<'_>) {
        let needs_full_rebuild = self
            .line_cache()
            .map_or(true, |c| c.width != content_width);

        if needs_full_rebuild {
            // Width changed or no cache: full rebuild
            let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..self.messages.len())
                .map(|i| self.render_message_lines(i, content_width, ctx))
                .collect();
            let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
            self.cache = CacheState::Clean(LineCache { width: content_width, per_msg, flat });
        } else if let CacheState::Dirty(cache, k) = &self.cache {
            // Incremental rebuild: only re-render messages[k..]
            // Render all dirty slots first (immutable borrow of self), then apply.
            let k = *k;
            let n = self.messages.len();
            let needs_resize = cache.per_msg.len() != n;

            // Render fresh slots for [k..n]
            let fresh: Vec<Vec<ratatui::text::Line<'static>>> = (k..n)
                .map(|i| self.render_message_lines(i, content_width, ctx))
                .collect();

            // Apply to cache (now mutable borrow); Dirty(c, k) → Clean(c)
            // mirrors the old `dirty_from.take()`.
            let CacheState::Dirty(mut cache, _) =
                std::mem::replace(&mut self.cache, CacheState::Missing)
            else {
                unreachable!("cache must be Dirty here")
            };
            if needs_resize {
                cache.per_msg.truncate(k);
                cache.per_msg.extend(fresh);
            } else {
                for (offset, rendered) in fresh.into_iter().enumerate() {
                    cache.per_msg[k + offset] = rendered;
                }
            }
            // Rebuild flat from k
            let prefix_line_count: usize = cache.per_msg[..k].iter().map(|v| v.len()).sum();
            cache.flat.truncate(prefix_line_count);
            for slot in &cache.per_msg[k..] {
                cache.flat.extend(slot.iter().cloned());
            }
            self.cache = CacheState::Clean(cache);
        }
        // Paranoia fallback: guarantee a cache exists (should never fire —
        // the enum makes this provably dead, but deleting it is a later
        // commit, not this one; design §6).
        if matches!(self.cache, CacheState::Missing) {
            let per_msg: Vec<Vec<ratatui::text::Line<'static>>> = (0..self.messages.len())
                .map(|i| self.render_message_lines(i, content_width, ctx))
                .collect();
            let flat: Vec<ratatui::text::Line<'static>> = per_msg.iter().flatten().cloned().collect();
            self.cache = CacheState::Clean(LineCache { width: content_width, per_msg, flat });
        }
    }

    // ── Snapshot seam (slice e) ───────────────────────────────────────────────

    /// Sync the line cache to the viewport width (full rebuild on width
    /// change, incremental from the dirty watermark otherwise), apply
    /// pin/growth/clamp scroll bookkeeping, record the viewport geometry +
    /// visible range for selection mapping, and return the visible window.
    ///
    /// Folds the old draw.rs §3–§6 block (design §3.5) so the cache-sync →
    /// scroll-clamp → slice ordering can't be re-interleaved wrongly at call
    /// sites. `&mut self` is honest: scroll bookkeeping legitimately mutates
    /// during model build. Ordering inside the unpinned branch is
    /// load-bearing: growth-adjust THEN clamp THEN `last_line_count` write.
    ///
    /// `msg_area` is the outer body rect; the store derives the inner content
    /// rect (-1 border/pad each side) exactly as `msg_block.inner(msg_area)`
    /// does on the render side.
    pub(crate) fn visible_window(
        &mut self,
        msg_area: ratatui::layout::Rect,
        ctx: &RenderCtx<'_>,
    ) -> VisibleWindow {
        let content_height = msg_area.height.saturating_sub(2) as usize;
        let content_width = msg_area.width.saturating_sub(2) as usize;

        // ── Cache sync (old draw.rs §3) ──
        self.sync_cache(content_width, ctx);
        let total = self.line_cache().map_or(0, |c| c.flat.len());

        // ── Scroll bookkeeping (old draw.rs §4) ──
        // Order is load-bearing: growth-adjust THEN clamp THEN last_line_count write.
        if self.scroll_pinned {
            self.scroll_back = 0;
        } else {
            let prev = self.last_line_count;
            if total > prev && prev > 0 {
                let growth = (total - prev) as u16;
                self.scroll_back = self.scroll_back.saturating_add(growth);
            }
            let max_back = total.saturating_sub(content_height).min(u16::MAX as usize) as u16;
            if self.scroll_back > max_back {
                self.scroll_back = max_back;
            }
        }
        self.last_line_count = total;
        let scroll_back = self.scroll_back;

        // ── Visible range + viewport geometry (old draw.rs §5 write-backs) ──
        let end = total.saturating_sub(scroll_back as usize);
        let start = end.saturating_sub(content_height);
        // Inner rect: TOP+BOTTOM borders (-2 height, +1 y), horizontal padding
        // (-2 width, +1 x).  Matches `msg_block.inner(msg_area)` exactly.
        let msg_inner = ratatui::layout::Rect {
            x: msg_area.x + 1,
            y: msg_area.y + 1,
            width: msg_area.width.saturating_sub(2),
            height: msg_area.height.saturating_sub(2),
        };
        self.viewport = Some(msg_inner);
        self.visible_range = Some((start, end));

        // Clone only the visible window (~viewport height lines) into the Arc —
        // O(viewport) not O(n).  `total` above is the full flat count, kept for
        // scroll bookkeeping; `lines` is only the slice the render thread needs.
        let all_lines: &[ratatui::text::Line<'static>] =
            self.line_cache().map_or(&[], |c| c.flat.as_slice());
        let visible_slice = all_lines.get(start..end).unwrap_or(&[]);
        let lines: std::sync::Arc<[ratatui::text::Line<'static>]> = visible_slice.to_vec().into();

        VisibleWindow {
            lines,
            lines_width: content_width,
            scroll_back,
            selection: self.selection_range(), // old draw.rs §6
            is_empty: self.messages.is_empty(),
        }
    }

    // ── Selection (moved in slice e; bodies verbatim from app.rs) ────────────
    //
    // Terminal coordinates in P9; P10 replaces the internals with
    // content-relative coordinates behind these same signatures.

    /// Start a selection at `(col, row)` — Down(Left) inside the message
    /// area. Clears any previous end point.
    pub(crate) fn selection_begin(&mut self, col: u16, row: u16) {
        self.selection_anchor = Some((col, row));
        self.selection_end = None;
    }

    /// Extend the selection to `(col, row)` — Drag(Left). No-op when no
    /// anchor exists (drag without a preceding in-area down).
    pub(crate) fn selection_drag(&mut self, col: u16, row: u16) {
        if self.selection_anchor.is_some() {
            self.selection_end = Some((col, row));
        }
    }

    /// Finalize the selection at `(col, row)` — Up(Left). A release at the
    /// anchor position was a click, not a drag — clears the selection.
    pub(crate) fn selection_release(&mut self, col: u16, row: u16) {
        if let Some(anchor) = self.selection_anchor {
            let end = (col, row);
            // If start == end, it was a click not a drag — clear selection
            if anchor == end {
                self.clear_selection();
            } else {
                self.selection_end = Some(end);
            }
        }
    }

    /// Returns true if there is an active text selection in the message area.
    pub(crate) fn has_selection(&self) -> bool {
        self.selection_anchor.is_some() && self.selection_end.is_some()
    }

    /// Clear the current text selection.
    pub(crate) fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_end = None;
    }

    /// Get the normalized selection range: (start_col, start_row, end_col, end_row)
    /// where start <= end in reading order. Returns None if no selection.
    pub(crate) fn selection_range(&self) -> Option<(u16, u16, u16, u16)> {
        let (ac, ar) = self.selection_anchor?;
        let (ec, er) = self.selection_end?;
        // Normalize: start is the earlier position in reading order
        if ar < er || (ar == er && ac <= ec) {
            Some((ac, ar, ec, er))
        } else {
            Some((ec, er, ac, ar))
        }
    }

    /// Check if a terminal coordinate is inside the message content area.
    /// `viewport` stores the inner rect (after borders/padding), so no offset
    /// arithmetic is needed. Replaces `input.rs::is_in_msg_area`; returns
    /// `false` until the first render establishes the viewport (pinned
    /// behavior — scenario 22's "Shady observation").
    pub(crate) fn hit_test(&self, col: u16, row: u16) -> bool {
        if let Some(rect) = self.viewport {
            col >= rect.x && col < rect.x + rect.width
                && row >= rect.y && row < rect.y + rect.height
        } else {
            false
        }
    }

    /// Rendering margin used in render.rs for message continuation lines.
    /// 3-char margin + 2-char content indent = 5 chars total.
    const MSG_LINE_INDENT: &'static str = "     ";

    /// Extract the selected text from the visible line cache.
    /// Uses the viewport rect and visible range to map terminal coordinates
    /// back to line content. The viewport stores the inner content rect
    /// (after borders/padding), so no offset arithmetic is needed here.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (sc, sr, ec, er) = self.selection_range()?;
        let rect = self.viewport?;
        let (vis_start, vis_end) = self.visible_range?;
        let all_lines = &self.line_cache()?.flat;

        let content_x = rect.x;
        let content_y = rect.y;
        let content_h = rect.height;

        // Convert terminal y-coordinates to line indices
        let mut result = String::new();
        for term_y in sr..=er {
            if term_y < content_y || term_y >= content_y + content_h {
                continue;
            }
            let line_offset = (term_y - content_y) as usize;
            let line_idx = vis_start + line_offset;
            if line_idx >= vis_end || line_idx >= all_lines.len() {
                continue;
            }
            let line = &all_lines[line_idx];
            // Extract text from the line spans
            let full_text: String = line.spans.iter()
                .map(|s| s.content.as_ref())
                .collect();

            // Determine character range on this line
            let line_start_col = if term_y == sr {
                (sc.saturating_sub(content_x)) as usize
            } else {
                0
            };
            let line_end_col = if term_y == er {
                (ec.saturating_sub(content_x)) as usize
            } else {
                full_text.len()
            };

            let chars: Vec<char> = full_text.chars().collect();
            let start = line_start_col.min(chars.len());
            let end = line_end_col.min(chars.len());
            if start < end {
                let selected: String = chars[start..end].iter().collect();
                let trimmed = selected.trim_end();
                let trimmed = if result.is_empty() {
                    trimmed.trim_start()
                } else {
                    trimmed.strip_prefix(Self::MSG_LINE_INDENT).unwrap_or(trimmed)
                };
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(trimmed);
            }
        }

        if result.is_empty() { None } else { Some(result) }
    }

    // ── Render options (slice d; locked decision #1) ─────────────────────────

    /// Ctrl+O state: show full tool output instead of the truncated preview.
    pub(crate) fn show_full_output(&self) -> bool {
        self.show_full_output
    }

    /// Self-invalidating setter — the toggle changes cached line content, so
    /// the manual `invalidate()` at the call site is impossible to forget
    /// because it no longer exists (red-team overrule of design §6 decision 1).
    pub(crate) fn set_show_full_output(&mut self, v: bool) {
        self.show_full_output = v;
        self.invalidate();
    }

    /// True when any transcript line is spinner-animated, i.e. re-rendering
    /// the message cache on a spinner tick would change output. Was
    /// `App::render_lines_uses_spinner` (design §3.4).
    pub(crate) fn uses_spinner(&self) -> bool {
        self.messages.iter().enumerate().any(|(idx, msg)| match &msg.msg {
            ChatMessage::Thinking(text) => text == THINKING_PLACEHOLDER,
            ChatMessage::ToolUseStart { .. } => true,
            ChatMessage::ToolUse { .. } => {
                idx == self.messages.len().saturating_sub(1) && self.tool_start_time.is_some()
            }
            ChatMessage::ToolResult { .. } => self.is_active_tool_result(idx),
            _ => false,
        })
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

#[cfg(test)]
mod visible_window_tests {
    use super::*;

    fn test_ctx() -> RenderCtx<'static> {
        RenderCtx { spinner_frame: 0, streaming: false, agent_name: "synaps" }
    }

    /// Ported from app.rs with slice (e). The app.rs original *mirrored* the
    /// draw.rs publish path because `build_render_model` needs a live
    /// Runtime + CommandRegistry; now that the path IS a store method, the
    /// test drives `visible_window()` directly and pins the same invariants:
    ///   1. `lines.len()` == content_height (the viewport), NOT `total`.
    ///      This is what makes the publish O(viewport) instead of O(n).
    ///   2. `lines` content == `cache.flat[start..end]` for the scroll position.
    ///   3. the render thread's `model.lines.to_vec()` (no re-slice) equals
    ///      that same window — proving the [start..end] re-slice is redundant.
    ///   4. a different scroll_back yields a different window of the same len.
    #[test]
    fn visible_window_publish_clones_only_viewport_not_full_buffer() {
        let mut store = TranscriptStore::new();

        // 20 Text messages → each renders as 1 flat line at w=80, so total >= 20.
        for i in 0..20 {
            store.push_msg(ChatMessage::Text(format!("line {i}")));
        }

        let content_width: usize = 80;
        let content_height: usize = 5; // viewport is 5 rows — much less than 20
        // Outer body rect: inner content = outer − 2 in each dimension
        // (visible_window derives content_width/height the same way draw.rs did).
        let msg_area = ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: content_width as u16 + 2,
            height: content_height as u16 + 2,
        };

        // Pinned at bottom → scroll_back = 0.
        let vw = store.visible_window(msg_area, &test_ctx());

        let total = store.line_cache().unwrap().flat.len();
        assert!(total >= 20, "sanity: need >= 20 flat lines, got {total}");
        let end = total; // scroll_back = 0
        let start = end.saturating_sub(content_height);

        let to_str = |sl: &[ratatui::text::Line<'static>]| -> Vec<String> {
            sl.iter()
              .map(|l| {
                  l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
              })
              .collect()
        };

        // 1. Published len must equal the viewport, NOT the full buffer.
        assert_eq!(
            vw.lines.len(),
            content_height,
            "vw.lines.len() must equal content_height ({content_height}), \
             got {} (full buffer len = {total}) — full-buffer publish regression",
            vw.lines.len()
        );
        assert_eq!(vw.lines_width, content_width, "lines_width must be the content width");
        assert_eq!(vw.scroll_back, 0, "pinned → post-clamp scroll_back must be 0");
        assert!(!vw.is_empty, "20 messages → is_empty must be false");

        // 2. Content must match cache.flat[start..end].
        assert_eq!(
            to_str(&vw.lines),
            to_str(&store.line_cache().unwrap().flat[start..end]),
            "published window content must equal cache.flat[start..end]"
        );

        // 3. Render-thread side: visible = model.lines.to_vec() (no re-slice).
        let visible_render: Vec<ratatui::text::Line> = vw.lines.to_vec();
        assert_eq!(
            to_str(&visible_render),
            to_str(&store.line_cache().unwrap().flat[start..end]),
            "render-thread .to_vec() on vw.lines must equal the visible window"
        );

        // 4. Sanity: scroll into the middle, check a different window.
        store.scroll_up(10); // unpins, scroll_back = 10
        let vw2 = store.visible_window(msg_area, &test_ctx());
        assert_eq!(vw2.lines.len(), content_height,
            "mid-scroll window must also have viewport length");
        assert_eq!(vw2.scroll_back, 10, "scroll_back 10 is within clamp range");
        assert_ne!(
            to_str(&vw2.lines),
            to_str(&vw.lines),
            "different scroll positions must yield different window content"
        );
    }
}
