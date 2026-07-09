//! # TranscriptStore — message pane ownership
//!
//! This module owns everything the message pane shows and nothing else:
//! the message list, the per-message render cache, the viewport position, and
//! the selection quad. External code reads and mutates the transcript
//! **exclusively through the method surface defined here** — no code outside
//! this file may index into the flat cache or touch store fields directly.
//! That invariant is enforced by slice (f); see
//! `~/Jawz/notes/tech/synaps-p9-transcriptstore-seam-design.md` §3.6.
//!
//! **Flat-cache rule (design §3.6), post-P11:** the flat BUFFER is dead —
//! `cum_heights` carries the flat COORDINATE system (same numbers, §0
//! identity), `visible_window` assembles O(visible) frames from per-slot
//! lines, and copy reads message SOURCE + `LineMeta` provenance. The
//! quarantined `flat_index_to_content`/`content_to_flat_index` pair is the
//! only flat-index arithmetic left. This rule enables the inline-mode
//! accommodation described on [`TranscriptStore`] without touching any call
//! site.
//!
//! # Scroll / selection interaction (P10, DECISION LOCK L4)
//! `scroll_up`/`scroll_down` do **not** touch selection state, and since P10
//! slice (b) neither does mouse-wheel scroll (the wheel arms in `input.rs` no
//! longer clear): selection endpoints are content-relative ([`SelPos`]), so
//! scrolled or streaming-grown content carries its selection along and the
//! overlay simply clamps to the visible window. Any KEYPRESS still clears the
//! selection at the top of `input.rs::handle_key` — typing dismisses
//! selection, and that includes Shift+Up/Down keyboard scroll. Structural
//! changes remap or clear per lock L3: an insert/invalidate at index
//! `k <= max(endpoint msg_idx)` shifts endpoints (pure insert below them) or
//! clears the selection; width changes clear it explicitly in `sync_cache`.

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

impl ChatMessage {
    /// The canonical "source" string this message contributes to copy — what
    /// the model/user actually wrote, per the P10 decision lock (design §2
    /// table). [`LineMeta`] ranges and `src_line` indices index into THIS
    /// string; `selected_text` reconstructs from it verbatim. No prompt
    /// prefixes, no timestamps, no tool-card chrome.
    pub(crate) fn source_text(&self) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match self {
            // Raw input as submitted. Pasted messages copy their stored
            // display form incl. "[Pasted N lines]" placeholders (D1 as
            // locked: the full paste lives App-side in api_messages; P10
            // does not reach across that boundary).
            ChatMessage::User(t) => Cow::Borrowed(t.as_str()),
            // Raw markdown the model wrote — markers, fences, tables intact.
            ChatMessage::Text(t) => Cow::Borrowed(t.as_str()),
            // Raw markdown; the spinner sentinel is chrome, not content.
            ChatMessage::Thinking(t) if t == THINKING_PLACEHOLDER => Cow::Borrowed(""),
            ChatMessage::Thinking(t) => Cow::Borrowed(t.as_str()),
            // Transient accumulated fragment (design §2: not over-engineered;
            // finalize replaces the message).
            ChatMessage::ToolUseStart { partial_input, .. } => Cow::Borrowed(partial_input.as_str()),
            // DECISION LOCK L5 — the flip point: ToolUse copies as the
            // PRETTY-PRINTED input JSON (line-mappable, valid JSON, readable
            // pasted). To flip to the raw compact form, replace this arm's
            // body with `Cow::Borrowed(input.as_str())` — nothing else
            // depends on the choice (it's copy-time only).
            ChatMessage::ToolUse { input, .. } => {
                match serde_json::from_str::<serde_json::Value>(input) {
                    Ok(v) => Cow::Owned(
                        serde_json::to_string_pretty(&v).unwrap_or_else(|_| input.clone()),
                    ),
                    // Unparseable input: raw bytes are the only truth we have.
                    Err(_) => Cow::Borrowed(input.as_str()),
                }
            }
            // Raw stored tool output — un-truncated, un-highlighted.
            ChatMessage::ToolResult { content, .. } => Cow::Borrowed(content.as_str()),
            ChatMessage::Error(t) => Cow::Borrowed(t.as_str()),
            ChatMessage::System(t) => Cow::Borrowed(t.as_str()),
            // `[source]` tag + severity are routing metadata — chrome, same
            // class as a timestamp (design §2, resolved as locked).
            ChatMessage::Event { text, .. } => Cow::Borrowed(text.as_str()),
        }
    }
}

pub(crate) struct TimestampedMsg {
    pub(crate) msg: ChatMessage,
    pub(crate) time: String,
}

/// Provenance of one display row: which bytes of the owning message's
/// canonical source it presents (P10 design §1.2, variant shapes per the
/// DECISION LOCK L1/L2/L6). All coordinates are message-local; nothing here
/// references flat/global line indices (P11-proof).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LineMeta {
    /// Decoration: headers, timestamps, padding, separators, panel chrome,
    /// "+N lines" footers, ok/timeout footers, spinner rows. Copies nothing.
    Chrome,
    /// This row presents `source[range]` verbatim. `content_col` is the
    /// display column (within the row, 0-based, display-width cells) where
    /// source content begins — everything left of it is injected
    /// prefix/margin/indent. Emitted ONLY under the lock L1 rule: the row's
    /// rendered text carries the source slice byte-identically (no inline-md
    /// transform, no `\t` in the source line, no post-pass that disturbs the
    /// range↔column correspondence). Soft-vs-hard breaks are DERIVED between
    /// consecutive Content rows of one message:
    ///   gap bytes whitespace-only, no '\n'  ⇒ soft wrap
    ///   gap bytes contain '\n'              ⇒ hard break
    Content { range: std::ops::Range<usize>, content_col: u16 },
    /// This row presents a *transformed* view of source line `src_line`
    /// (clamped code, highlighted tool output, pretty-printed JSON row,
    /// thinking excerpt) — line-level mapping is sound but column-level is
    /// not. Copy at line granularity only (locks L1 fallback, L2 tool cards,
    /// L6 tab-bearing lines).
    ContentLine { msg_idx: usize, src_line: usize },
}

/// Byte range of source line `idx` (`str::lines` numbering — matching the
/// render arms and `ContentLine::src_line`). An index one past the last line
/// resolves to the empty range at source end (defensive; the D4 "+N lines"
/// row anchors at the first HIDDEN line, which exists whenever it renders).
fn source_line_range(source: &str, idx: usize) -> std::ops::Range<usize> {
    let base = source.as_ptr() as usize;
    for (i, l) in source.lines().enumerate() {
        if i == idx {
            let start = l.as_ptr() as usize - base;
            return start..start + l.len();
        }
    }
    source.len()..source.len()
}

/// Per-message render cache slot (P11 design §1.2). This is the unit P11
/// caches and (in the second-half slices) evicts.
///
/// Two deliberate choices, per the decision lock:
/// - **`meta.len()` IS the height** — no separate `height` field to desync
///   (and no `u16` truncation concern). `meta` is always present after
///   measurement and parallels the rendered rows.
/// - **`meta` survives demotion; only `lines` dies.** `selected_text` reads
///   `meta` + `source_text()` exclusively (P10 §7), so copy of off-screen —
///   even evicted — selections works with zero re-render. `lines: None`
///   means "measured but demoted": height/meta retained, pixels evicted.
///   Demotion is live: `visible_window` evicts slots outside the viewport ±
///   halo each frame and promotes (re-renders) on demand.
pub(crate) struct MsgSlot {
    /// Present only while the message intersects the viewport ± the
    /// retention halo (design §3 step 7).
    pub(crate) lines: Option<Vec<ratatui::text::Line<'static>>>,
    /// Parallel to the rendered rows. Invariant: when `lines` is `Some`,
    /// `lines.len() == meta.len()`.
    pub(crate) meta: Vec<LineMeta>,
}

impl MsgSlot {
    /// The message's exact height in display rows — valid whether or not
    /// the pixels are currently materialized.
    pub(crate) fn height(&self) -> usize {
        self.meta.len()
    }

    /// The rendered rows. Panics on a demoted slot — callers must hold the
    /// "promoted first" invariant (`visible_window` promotes window slots
    /// before assembly; the sync paths only touch freshly rendered slots).
    /// Callers that can legitimately see a demoted slot must match on the
    /// `Option` explicitly (lock L3) instead.
    #[track_caller]
    pub(crate) fn lines(&self) -> &[ratatui::text::Line<'static>] {
        self.lines
            .as_deref()
            .expect("MsgSlot demoted — promote (re-render) before reading lines")
    }
}

/// Per-message render cache. Parallel to `TranscriptStore.messages`: each slot
/// holds the rendered [`MsgSlot`] for that message. There is NO flat
/// materialization (killed in the P11 flat-kill slice): `cum_heights` is the
/// flat coordinate system, and `visible_window` assembles the O(visible)
/// window straight from per-slot lines. The `width` at which these were
/// rendered is stored so stale entries can be detected on terminal resize.
pub(crate) struct LineCache {
    pub(crate) width: usize,
    /// Rendered lines + meta per message — index parallel to TranscriptStore.messages.
    pub(crate) per_msg: Vec<MsgSlot>,
    /// Cumulative height offsets (P11 §1.2): `cum_heights[i]` = summed
    /// heights of messages `[0..i)`; `cum_heights[n]` = total. Always
    /// `per_msg.len() + 1` entries. Rebuilt from the dirty watermark k in
    /// O(n−k) usize writes. Because `height(i) == lines.len()`, these ARE
    /// flat-line indices (the design's identity claim, §0) — every scroll,
    /// selection, and growth-adjust number is unchanged from the flat era.
    pub(crate) cum_heights: Vec<usize>,
}

impl LineCache {
    /// Build a cache from rendered slots, computing `cum_heights` from
    /// scratch.
    pub(crate) fn new(width: usize, per_msg: Vec<MsgSlot>) -> Self {
        let mut cache = LineCache { width, per_msg, cum_heights: Vec::new() };
        cache.rebuild_cum_from(0);
        cache
    }

    /// Rebuild `cum_heights` from message index `k` — the cumulative-offset
    /// cache is invalidated with the same watermark as the slots it sums.
    /// Returns the number of entries written (perf probe, lock L4).
    pub(crate) fn rebuild_cum_from(&mut self, k: usize) -> usize {
        let k = k.min(self.per_msg.len())
            .min(self.cum_heights.len().saturating_sub(1));
        self.cum_heights.truncate(k + 1);
        if self.cum_heights.is_empty() {
            debug_assert_eq!(k, 0);
            self.cum_heights.push(0);
        }
        let mut acc = self.cum_heights[k];
        let mut written = 0usize;
        for slot in &self.per_msg[k..] {
            acc += slot.height();
            self.cum_heights.push(acc);
            written += 1;
        }
        written
    }

    /// Total height in rows — `cum_heights[n]`. The sole total: this IS
    /// what `flat.len()` used to be (§0 identity).
    pub(crate) fn total_height(&self) -> usize {
        *self.cum_heights.last().unwrap_or(&0)
    }
}

/// Test-only perf probe (P11 design §5.2, lock L4). Count-based — no
/// wall-clock flake: the perf gate asserts *how many* message renders and
/// cumulative-offset writes a frame performed, not how long it took. Same
/// class of test-only seam as `test_take_cache`. Interior-mutable
/// (`AtomicUsize`) because `render_message_lines` takes `&self`; per-store
/// (not a global) so parallel tests can't cross-contaminate counts.
///
/// Compiled only under `test`/the `testing` feature — production builds
/// carry neither the field nor the fetch_adds.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub(crate) struct PerfProbe {
    /// Bumped once per `render_message_lines` call (measure == render).
    pub(crate) renders: std::sync::atomic::AtomicUsize,
    /// Bumped once per cumulative-offset entry written. Zero on a Clean
    /// frame is the lock-L4 "no O(n) re-sum per frame" invariant. Wired up
    /// when `cum_heights` lands (P11 MsgSlot slice); until then it stays 0.
    pub(crate) cum_writes: std::sync::atomic::AtomicUsize,
}

/// Cache lifecycle for the per-message line cache. Replaces the old
/// `line_cache: Option<LineCache>` + `dirty_from: Option<usize>` tri-state
/// with the same semantics made explicit (slice d):
///
/// - `Missing`      = full rebuild on next sync (old `None` + `None`)
/// - `Clean(c)`     = serve as-is                (old `Some` + `None`)
/// - `Dirty(c, k)`  = incremental re-render from message index `k`
///   (old `Some` + `Some(k)`)
///
/// Width mismatch (`c.width != content_width`) still forces a full rebuild
/// regardless of Clean/Dirty — that check lives in `sync_cache`.
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
    /// `line_cache.as_mut()`. Production consumer since the demotion slice:
    /// promote/demote flips a slot's `lines` in place.
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
/// delegating wrappers in app.rs. All fields are private; access is
/// exclusively through the method surface below.
/// Slice (d): the cache tri-state became [`CacheState`]; the renderer
/// (`render_message_lines`, render.rs) moved into this impl with
/// [`RenderCtx`] threading; `sync_cache` folds in the draw.rs cache-sync
/// block; `show_full_output` is store-owned (locked decision #1).
///
/// # Inline-mode accommodation (design §3.6)
///
/// The future `committed_upto: usize` field for inline-mode (Option C) will
/// land entirely here. The change is: add the field, bound the cache-sync
/// loops in `sync_cache` to `committed_upto..n` instead of `0..n`, add a
/// `commit_through(idx) -> Vec<Line>` drain, and clamp `visible_window`'s
/// scroll computation to the uncommitted tail. Because `per_msg` is already
/// message-indexed and `visible_window` is the sole cache consumer, nothing
/// outside this file changes — which is precisely why the P9 spike gates
/// inline mode on this extraction.
pub(crate) struct TranscriptStore {
    messages: Vec<TimestampedMsg>,

    // ── Scroll state (moved in slice c) ──────────────────────────────────────
    /// Viewport offset from the bottom (0 = pinned to latest line).
    scroll_back: u16,
    /// When `true`, viewport stays pinned to the bottom (auto-scroll).
    /// Cleared when the user scrolls up; restored when they reach bottom.
    scroll_pinned: bool,
    /// Previous flat-line total — used to stabilise `scroll_back` when
    /// unpinned during streaming growth. See draw.rs §4 (growth-adjust block).
    last_line_count: usize,

    // ── Render cache (moved in slice b′; enum shape since slice d) ───────────
    /// Cached wrapped+highlighted message lines + incremental watermark.
    /// See [`CacheState`] for the lifecycle.
    cache: CacheState,

    // ── Render options (moved in slice d; locked decision #1) ────────────────
    /// Ctrl+O toggle: show full tool output instead of the truncated preview.
    /// Store-owned because it changes cached line content — mutate only via
    /// [`Self::set_show_full_output`], which invalidates internally.
    show_full_output: bool,

    // ── Tool timing (moved in slice b′; locked decision #2) ──────────────────
    /// Tracks when the current tool started executing (for elapsed time display)
    tool_start_time: Option<std::time::Instant>,
    /// Per-tool start times keyed by `tool_id`. Lets parallel tool calls
    /// each show their own elapsed-time on the result block, instead of
    /// sharing a single timer that the last-started tool clobbers.
    tool_start_times: std::collections::HashMap<String, std::time::Instant>,

    // ── Viewport geometry (moved in slice e) ─────────────────────────────────
    /// Inner content rect of the message area as of the last
    /// [`Self::visible_window`] call (was `App.msg_area_rect`; store-side name
    /// per design §1a, locked decision #4). `None` until the first render —
    /// `hit_test` returns `false` before that.
    viewport: Option<ratatui::layout::Rect>,
    /// Flat-cache index range visible in the viewport (was
    /// `App.visible_line_range`). Consumed only by `selected_text`.
    visible_range: Option<(usize, usize)>,

    // ── Selection (moved in slice e; content-relative since P10 slice (b)) ───
    selection_anchor: Option<SelPos>,
    selection_end: Option<SelPos>,

    // ── Perf probe (test-only; P11 §5.2 / lock L4) ───────────────────────────
    #[cfg(any(test, feature = "testing"))]
    probe: PerfProbe,
}

/// A selection endpoint in content space (P10 slice (b) — design §3.2).
/// Captured at mouse-event time by mapping (col,row) through the viewport +
/// the LIVE scroll offset + per_msg prefix sums (red-team 3e: the
/// event→content window is computed from current `scroll_back`, not the
/// stored `visible_range` tuple).
///
/// Deliberately redundant: `(msg_idx, line_in_msg, col)` drives the highlight
/// inverse-map; `src_byte` drives the slice-(d) copy path. All coordinates
/// are message-local — nothing here survives on flat indices (P11-proof).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SelPos {
    msg_idx: usize,
    /// Index into `per_msg[msg_idx].lines` — for highlight painting.
    line_in_msg: usize,
    /// Display column within that row (viewport-relative) — for highlight
    /// painting and the copy-time column walk.
    col: u16,
    /// Resolved source byte for `Content` rows (via [`LineMeta`]); `None` on
    /// Chrome/ContentLine rows — copy snaps to line granularity there
    /// (locks L1 fallback / L2 / L6).
    src_byte: Option<usize>,
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
            #[cfg(any(test, feature = "testing"))]
            probe: PerfProbe::default(),
        }
    }

    // ── Perf probe surface (test-only; P11 §5.2 / lock L4) ───────────────────

    /// Message renders since the last [`Self::probe_reset`].
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_render_count(&self) -> usize {
        self.probe.renders.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cumulative-offset entry writes since the last [`Self::probe_reset`].
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_cum_write_count(&self) -> usize {
        self.probe.cum_writes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Zero both perf counters — call after a warm-up frame, before the
    /// frame under measurement.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_reset(&self) {
        self.probe.renders.store(0, std::sync::atomic::Ordering::Relaxed);
        self.probe.cum_writes.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Internal bump seam for `render_message_lines` (render.rs — a sibling
    /// module; store fields are sealed, so the bump routes through a method).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_note_render(&self) {
        self.probe.renders.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // ── Scroll API ────────────────────────────────────────────────────────────
    //
    // These methods do NOT touch selection state — see the module-level
    // scroll/selection note (lock L4): wheel scroll preserves selection,
    // keypresses clear it in input.rs::handle_key.

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
                // L3: structural insert — shift selection endpoints past the
                // insertion point (or clear when the selection spans it)
                // BEFORE the watermark. Must bypass the selection-clearing
                // `invalidate_from`, which would nuke a selection the remap
                // just preserved: the re-render from `at` writes identical
                // content into the shifted slots.
                self.selection_remap_insert(at);
                // Insert mid-list — watermark at the matched ToolUse index
                // `i`, not the insert point `at` (lock L2): the arriving
                // result flips the use's "⠋ running…" header to done, and
                // with the tool-delta full invalidates narrowed this is the
                // only thing left that repaints it. Same watermark-only seam
                // (selection handled by the remap above — the re-render
                // writes identical content rows into the shifted slots; the
                // use's own change is header chrome, not content).
                self.invalidate_watermark(i);
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
    ///
    /// Lock L3: a full invalidate is an invalidate at k = 0 — any active
    /// selection is cleared (content anywhere may have changed; no remap is
    /// sound).
    pub(crate) fn invalidate(&mut self) {
        self.selection_on_invalidate(0);
        self.cache = CacheState::Missing;
    }

    /// Mark messages from index `idx` onwards as dirty (cheapest granularity).
    /// Coalesces with any existing watermark by taking the minimum. A missing
    /// cache stays missing — the full rebuild subsumes any watermark (same as
    /// the old `None` cache + `Some(k)` state, which the rebuild path wiped).
    ///
    /// Lock L3: content changed at `idx..` — a selection with any endpoint at
    /// `msg_idx >= idx` is cleared (its content coordinates may no longer
    /// denote the same bytes). Streaming appends invalidate only the NEW tail
    /// index, so selections over earlier messages survive growth — the point
    /// of the content-relative migration.
    pub(crate) fn invalidate_from(&mut self, idx: usize) {
        self.selection_on_invalidate(idx);
        self.invalidate_watermark(idx);
    }

    /// Watermark-only invalidate — no selection interaction. Internal seam
    /// for `push_tool_result`, whose insert remaps endpoints itself.
    fn invalidate_watermark(&mut self, idx: usize) {
        self.cache = match std::mem::replace(&mut self.cache, CacheState::Missing) {
            CacheState::Missing => CacheState::Missing,
            CacheState::Clean(c) => CacheState::Dirty(c, idx),
            CacheState::Dirty(c, k) => CacheState::Dirty(c, k.min(idx)),
        };
    }

    /// Lock L3 (generalized D6(i)): clear the selection when an invalidate at
    /// `k` reaches it — i.e. `k <= max(endpoint msg_idx)`. Mid-drag anchors
    /// (no end yet) count.
    fn selection_on_invalidate(&mut self, k: usize) {
        let max_idx = match (&self.selection_anchor, &self.selection_end) {
            (Some(a), Some(e)) => a.msg_idx.max(e.msg_idx),
            (Some(a), None) => a.msg_idx,
            _ => return,
        };
        if k <= max_idx {
            self.clear_selection();
        }
    }

    /// Lock L3, insert case: a message inserted at `at` shifts everything at
    /// `>= at` down one slot. Endpoints entirely below the insertion point
    /// are untouched; endpoints entirely at/after it shift by +1 (their
    /// content is unchanged, just re-slotted); a selection SPANNING the
    /// insertion point is cleared — it would now cover the inserted message.
    fn selection_remap_insert(&mut self, at: usize) {
        let (min_idx, max_idx) = match (&self.selection_anchor, &self.selection_end) {
            (Some(a), Some(e)) => (a.msg_idx.min(e.msg_idx), a.msg_idx.max(e.msg_idx)),
            (Some(a), None) => (a.msg_idx, a.msg_idx),
            _ => return,
        };
        if at > max_idx {
            // Insert strictly below the selection — nothing moves.
        } else if at <= min_idx {
            for p in [&mut self.selection_anchor, &mut self.selection_end] {
                if let Some(p) = p.as_mut() {
                    p.msg_idx += 1;
                }
            }
        } else {
            self.clear_selection();
        }
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

        // Width change ⇒ rewrap ⇒ every content coordinate is re-derived;
        // clear the selection explicitly — honest and cheap (design §3.3).
        // (The invariant "width change invalidates selection" transfers to
        // P11 as a rule, not this line of code — see design §7.)
        if self.line_cache().is_some_and(|c| c.width != content_width) {
            self.clear_selection();
        }

        if needs_full_rebuild {
            // Width changed or no cache: full rebuild
            let per_msg: Vec<MsgSlot> = (0..self.messages.len())
                .map(|i| self.render_message_lines(i, content_width, ctx))
                .collect();
            let cache = LineCache::new(content_width, per_msg);
            #[cfg(any(test, feature = "testing"))]
            self.probe
                .cum_writes
                .fetch_add(cache.cum_heights.len(), std::sync::atomic::Ordering::Relaxed);
            self.cache = CacheState::Clean(cache);
        } else if let CacheState::Dirty(cache, k) = &self.cache {
            // Incremental rebuild: only re-render messages[k..]
            // Render all dirty slots first (immutable borrow of self), then apply.
            let k = *k;
            let n = self.messages.len();
            let needs_resize = cache.per_msg.len() != n;

            // Render fresh slots for [k..n] — the lock-L1 re-render window:
            // k..n, NOT k..=k+1 (the i−1-only dependency premise is falsified
            // by whole-list reads in the render arms; see the P11 lock).
            let fresh: Vec<MsgSlot> = (k..n)
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
            // Splice the cumulative-offset cache from the same watermark.
            let _cum_written = cache.rebuild_cum_from(k);
            #[cfg(any(test, feature = "testing"))]
            self.probe
                .cum_writes
                .fetch_add(_cum_written, std::sync::atomic::Ordering::Relaxed);
            self.cache = CacheState::Clean(cache);
        }
        // Paranoia fallback: guarantee a cache exists (should never fire —
        // the enum makes this provably dead, but deleting it is a later
        // commit, not this one; design §6).
        if matches!(self.cache, CacheState::Missing) {
            let per_msg: Vec<MsgSlot> = (0..self.messages.len())
                .map(|i| self.render_message_lines(i, content_width, ctx))
                .collect();
            self.cache = CacheState::Clean(LineCache::new(content_width, per_msg));
        }
        self.debug_assert_cum_identity();
    }

    /// The P11 §0 identity, executable: summed exact per-message heights and
    /// flat-line indices are the SAME numbers. Debug builds verify it after
    /// every cache sync; the whole pin-equivalence argument (21/22/23/24)
    /// leans on it. O(n) usize compares, debug-only — compiled out of
    /// release.
    fn debug_assert_cum_identity(&self) {
        #[cfg(debug_assertions)]
        if let Some(c) = self.line_cache() {
            debug_assert_eq!(
                c.cum_heights.len(),
                c.per_msg.len() + 1,
                "cum_heights must have per_msg.len()+1 entries"
            );
            debug_assert_eq!(c.cum_heights.first(), Some(&0));
            let mut acc = 0usize;
            for (i, slot) in c.per_msg.iter().enumerate() {
                if let Some(lines) = &slot.lines {
                    debug_assert_eq!(
                        lines.len(),
                        slot.height(),
                        "slot {i}: meta must stay parallel to rendered rows"
                    );
                }
                acc += slot.height();
                debug_assert_eq!(
                    c.cum_heights[i + 1],
                    acc,
                    "cum_heights[{}] inconsistent with slot heights",
                    i + 1
                );
            }
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
        // P11 (design §2.1): `total` is sourced from the cumulative-offset
        // cache — the same numbers the flat buffer's len() used to be (§0
        // identity). The growth-adjust/clamp code below is untouched, only
        // re-sourced.
        let total = self.line_cache().map_or(0, |c| c.total_height());

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

        // ── Ensure rendered (design §3 step 5) ──
        // Promote any demoted slot intersecting the visible window: heights
        // are already known, so scroll math above never waited on this; the
        // re-render is debug-asserted to reproduce the measured height.
        let to_promote: Vec<usize> = self
            .line_cache()
            .and_then(|c| {
                Self::window_msg_range(c, start, end).map(|(f, l)| {
                    (f..=l).filter(|&mi| c.per_msg[mi].lines.is_none()).collect()
                })
            })
            .unwrap_or_default();
        for mi in to_promote {
            self.promote_slot(mi, content_width, ctx);
        }

        // ── Window assembly (design §3 steps 4–6) ──
        // Resolve the visible message range by binary search over
        // `cum_heights` and copy partial first/last slices plus whole middles
        // into a fresh Vec → Arc. O(log n) search + O(viewport) clone — the
        // exact same fresh O(viewport) publish the flat slice produced
        // (nothing changes downstream; the Arc story holds).
        let assembled = self.assemble_window(start, end);
        let lines: std::sync::Arc<[ratatui::text::Line<'static>]> = assembled.into();

        // ── Demote (design §3 step 7, lock L3) ──
        // Near-viewport retention: slots outside the window ± a halo drop
        // their pixels (`lines = None`); meta/heights survive, so copy,
        // selection painting, and scroll math are unaffected. The halo is
        // one viewport of rows each side plus the locked ±1-message overscan
        // floor — wide enough that a wheel notch on a Clean cache promotes
        // nothing (perf: scroll tick = 0 renders); a jump past the halo
        // re-renders on demand next frame, bounded by O(visible).
        if let Some(cache) = self.cache.line_cache_mut() {
            let n = cache.per_msg.len();
            if n > 0 && total > 0 {
                let halo = content_height.max(1);
                let keep_lo = start.saturating_sub(halo);
                let keep_hi = (end + halo).min(total).max(keep_lo + 1);
                let (kf, kl) =
                    Self::window_msg_range(cache, keep_lo, keep_hi).unwrap_or((0, n - 1));
                let keep_first = kf.saturating_sub(1);
                let keep_last = (kl + 1).min(n - 1);
                for (mi, slot) in cache.per_msg.iter_mut().enumerate() {
                    if mi < keep_first || mi > keep_last {
                        slot.lines = None;
                    }
                }
            }
        }

        VisibleWindow {
            lines,
            lines_width: content_width,
            scroll_back,
            selection: self.selection_range(), // old draw.rs §6
            is_empty: self.messages.is_empty(),
        }
    }

    /// Resolve height-space rows `[start..end)` to a message range via
    /// binary search over `cum_heights` (design §3 step 4):
    /// `cum[first] <= start < cum[first+1]` and `cum[last] < end <= cum[last+1]`.
    /// Zero-height slots at the boundary resolve past themselves (they
    /// contribute no rows). `None` when the window is empty.
    fn window_msg_range(cache: &LineCache, start: usize, end: usize) -> Option<(usize, usize)> {
        if start >= end || cache.per_msg.is_empty() {
            return None;
        }
        let first = cache.cum_heights.partition_point(|&c| c <= start).saturating_sub(1);
        let last = cache.cum_heights.partition_point(|&c| c < end).saturating_sub(1);
        Some((first, last))
    }

    /// Assemble the visible window `[start..end)` from per-slot lines
    /// (design §3 steps 4–6): partial slices of the first/last slots plus
    /// whole middles. A message straddling an edge is rendered fully and
    /// sliced — line-granular windows require it, bounded by one message.
    fn assemble_window(&self, start: usize, end: usize) -> Vec<ratatui::text::Line<'static>> {
        let Some(cache) = self.line_cache() else { return Vec::new() };
        let Some((first, last)) = Self::window_msg_range(cache, start, end) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(end - start);
        for mi in first..=last {
            let slot = &cache.per_msg[mi];
            let base = cache.cum_heights[mi];
            let lo = start.saturating_sub(base);
            let hi = (end - base).min(slot.height());
            out.extend(slot.lines()[lo..hi].iter().cloned());
        }
        out
    }

    /// Re-materialize a demoted slot's pixels (design §3 step 5). The height
    /// is already known — the render is debug-asserted to reproduce it (the
    /// measure-IS-render guarantee; a mismatch means a height-affecting
    /// render input changed without an invalidate — the §1.4 rule violation).
    fn promote_slot(&mut self, msg_idx: usize, width: usize, ctx: &RenderCtx<'_>) {
        let needs = self
            .line_cache()
            .and_then(|c| c.per_msg.get(msg_idx))
            .is_some_and(|s| s.lines.is_none());
        if !needs {
            return;
        }
        let fresh = self.render_message_lines(msg_idx, width, ctx);
        if let Some(cache) = self.cache.line_cache_mut() {
            if let Some(slot) = cache.per_msg.get_mut(msg_idx) {
                debug_assert_eq!(
                    fresh.meta.len(),
                    slot.meta.len(),
                    "promoted slot {msg_idx} must re-render to its measured height"
                );
                *slot = fresh;
            }
        }
    }

    // ── Selection (content-relative since P10 slice (b)) ─────────────────────
    //
    // Same signatures as P9; the internals map terminal coords to content
    // space at event time and project back to a terminal quad per frame.

    /// flat index → (msg_idx, line_in_msg) via per_msg prefix sums.
    ///
    /// Together with [`Self::content_to_flat_index`] this is THE transitional
    /// flat-index dependency of the selection path — P11 swaps these two
    /// helpers for summed `desired_height` arithmetic; content-space
    /// endpoints survive unchanged (design §7). Do not add further flat math
    /// outside this pair.
    fn flat_index_to_content(&self, flat_idx: usize) -> Option<(usize, usize)> {
        let cache = self.line_cache()?;
        let mut off = flat_idx;
        for (mi, slot) in cache.per_msg.iter().enumerate() {
            if off < slot.height() {
                return Some((mi, off));
            }
            off -= slot.height();
        }
        None
    }

    /// (msg_idx, line_in_msg) → flat index. Inverse of
    /// [`Self::flat_index_to_content`]; `None` when the endpoint no longer
    /// exists in the cache (L3 clears should prevent this — treated as
    /// "paint nothing" rather than a panic).
    fn content_to_flat_index(&self, msg_idx: usize, line_in_msg: usize) -> Option<usize> {
        let cache = self.line_cache()?;
        let slot = cache.per_msg.get(msg_idx)?;
        if line_in_msg >= slot.height() {
            return None;
        }
        let prefix: usize = cache.per_msg[..msg_idx].iter().map(|e| e.height()).sum();
        Some(prefix + line_in_msg)
    }

    /// Map a terminal (col,row) to a content-space endpoint.
    ///
    /// The row window is computed from LIVE `scroll_back` (red-team 3e), not
    /// the stored `visible_range`, so a wheel event followed by a click
    /// before the next frame maps through post-scroll coordinates. Rows
    /// above/below the content clamp to the first/last content row; cols
    /// clamp into the viewport. `None` before the first render (no viewport)
    /// or on an empty transcript.
    fn map_event_to_selpos(&self, col: u16, row: u16) -> Option<SelPos> {
        let rect = self.viewport?;
        if rect.width == 0 {
            return None;
        }
        let (msg_idx, line_in_msg) = self.event_row_to_content(row)?;
        let col = col.saturating_sub(rect.x).min(rect.width - 1);
        let src_byte = self.resolve_src_byte(msg_idx, line_in_msg, col);
        Some(SelPos { msg_idx, line_in_msg, col, src_byte })
    }

    /// Terminal row → (msg_idx, line_in_msg) through the viewport + LIVE
    /// scroll offset (red-team 3e). Shared by event→SelPos mapping and
    /// promote-on-demand; `total` is sourced from the cumulative offsets.
    fn event_row_to_content(&self, row: u16) -> Option<(usize, usize)> {
        let rect = self.viewport?;
        let total = self.line_cache()?.total_height();
        if total == 0 || rect.width == 0 {
            return None;
        }
        let content_height = rect.height as usize;
        let end = total - (self.scroll_back as usize).min(total.saturating_sub(1));
        let start = end.saturating_sub(content_height);
        let row_off = row.saturating_sub(rect.y) as usize;
        let flat_idx = (start + row_off).min(end - 1);
        self.flat_index_to_content(flat_idx)
    }

    /// Frame-lagged demotion fix (lock L3 / red-team 2b): a wheel event and
    /// a Down/Drag in the same input batch — no draw between them, and wheel
    /// does not clear an in-flight selection — can map into rows that were
    /// demoted as of the last frame. Re-render such a slot on demand so
    /// `src_byte` resolution stays char-precise; the line-granularity degrade
    /// in [`Self::resolve_src_byte`] remains the backstop — never silently
    /// wrong bytes.
    fn promote_for_event(&mut self, row: u16, ctx: &RenderCtx<'_>) {
        let Some((msg_idx, _)) = self.event_row_to_content(row) else {
            return;
        };
        let Some(width) = self.line_cache().map(|c| c.width) else {
            return;
        };
        self.promote_slot(msg_idx, width, ctx);
    }

    /// Resolve a display column on a rendered row to a source byte via the
    /// row's `Content` meta. The rendered row carries `source[range]`
    /// verbatim starting at display column `content_col` (the slice-(a)
    /// invariant), so walking the RENDERED suffix by display width IS walking
    /// the source — CJK-safe, and a click on the second cell of a wide char
    /// snaps down (red-team 1d). Tab-bearing rows never get here (L6:
    /// they're ContentLine). `None` on Chrome/ContentLine rows.
    fn resolve_src_byte(&self, msg_idx: usize, line_in_msg: usize, col: u16) -> Option<usize> {
        use super::text_metrics::char_width;
        let entry = self.line_cache()?.per_msg.get(msg_idx)?;
        let LineMeta::Content { range, content_col } = entry.meta.get(line_in_msg)? else {
            return None;
        };
        // Lock L3: a demoted slot (lines == None) resolves to no src_byte —
        // the copy path degrades to line granularity (ContentLine-style),
        // never silently-wrong bytes. The event path promotes on demand
        // first (`promote_for_event`); this is the defensive backstop.
        let lines = entry.lines.as_ref()?;
        let text: String = lines[line_in_msg]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // Locate the content start byte by walking display columns.
        let mut byte = 0usize;
        let mut w: u16 = 0;
        for ch in text.chars() {
            if w >= *content_col {
                break;
            }
            w += char_width(ch) as u16;
            byte += ch.len_utf8();
        }
        if w != *content_col {
            return None; // defensive: content start not on a column boundary
        }
        let slice = text.get(byte..byte + range.len())?;
        // Walk the source suffix from content_col up to the clicked column.
        let mut off = 0usize;
        let mut cur = *content_col;
        for ch in slice.chars() {
            let cw = char_width(ch) as u16;
            if cur + cw > col {
                break;
            }
            cur += cw;
            off += ch.len_utf8();
        }
        Some(range.start + off)
    }

    /// Start a selection at `(col, row)` — Down(Left) inside the message
    /// area. Clears any previous end point. No-op (anchor cleared) when the
    /// event can't be mapped to content — empty transcript or pre-render.
    /// Promotes a demoted slot under the event first (lock L3): char-precise
    /// `src_byte` capture needs the rendered row.
    pub(crate) fn selection_begin(&mut self, col: u16, row: u16, ctx: &RenderCtx<'_>) {
        self.promote_for_event(row, ctx);
        self.selection_anchor = self.map_event_to_selpos(col, row);
        self.selection_end = None;
    }

    /// Extend the selection to `(col, row)` — Drag(Left). No-op when no
    /// anchor exists (drag without a preceding in-area down). Promotes a
    /// demoted slot under the event on demand (lock L3).
    pub(crate) fn selection_drag(&mut self, col: u16, row: u16, ctx: &RenderCtx<'_>) {
        if self.selection_anchor.is_some() {
            self.promote_for_event(row, ctx);
            if let Some(pos) = self.map_event_to_selpos(col, row) {
                self.selection_end = Some(pos);
            }
        }
    }

    /// Finalize the selection at `(col, row)` — Up(Left). A release at the
    /// anchor position was a click, not a drag — clears the selection.
    pub(crate) fn selection_release(&mut self, col: u16, row: u16, ctx: &RenderCtx<'_>) {
        if let Some(anchor) = self.selection_anchor.clone() {
            self.promote_for_event(row, ctx);
            let Some(end) = self.map_event_to_selpos(col, row) else {
                return;
            };
            // If start == end (in content space), it was a click not a drag.
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

    /// Get the normalized selection as a terminal quad
    /// (start_col, start_row, end_col, end_row), start <= end in reading
    /// order — now READING ORDER OVER CONTENT COORDS, projected through the
    /// last-rendered window (design §4.1 asterisk 1). Endpoints outside the
    /// window clamp to its edge rows; a selection entirely off-window returns
    /// `None` and the overlay simply doesn't paint. Returns None if no selection.
    pub(crate) fn selection_range(&self) -> Option<(u16, u16, u16, u16)> {
        let a = self.selection_anchor.as_ref()?;
        let b = self.selection_end.as_ref()?;
        // Normalize: start is the earlier position in content reading order.
        let (s, e) = if (a.msg_idx, a.line_in_msg, a.col) <= (b.msg_idx, b.line_in_msg, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        let rect = self.viewport?;
        let (vis_start, vis_end) = self.visible_range?;
        if vis_end <= vis_start || rect.width == 0 {
            return None;
        }
        let sf = self.content_to_flat_index(s.msg_idx, s.line_in_msg)?;
        let ef = self.content_to_flat_index(e.msg_idx, e.line_in_msg)?;
        if ef < vis_start || sf >= vis_end {
            return None;
        }
        let max_col = rect.width - 1;
        let (sf, sc) = if sf < vis_start { (vis_start, 0) } else { (sf, s.col.min(max_col)) };
        let (ef, ec) = if ef >= vis_end { (vis_end - 1, max_col) } else { (ef, e.col.min(max_col)) };
        Some((
            rect.x + sc,
            rect.y + (sf - vis_start) as u16,
            rect.x + ec,
            rect.y + (ef - vis_start) as u16,
        ))
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

    /// The clipboard-bound text for the current selection — SOURCE
    /// reconstruction (P10 slice (d), design §1.4). Walks the
    /// content-relative endpoints over per-row [`LineMeta`] provenance and
    /// emits bytes of each message's canonical source (`source_text`)
    /// verbatim:
    ///
    /// - `Content` rows: exact source bytes — endpoint columns resolve
    ///   through the `src_byte` captured at event time (char-precise).
    /// - `ContentLine` rows: whole source lines (locks L1 fallback / L2
    ///   tool cards / L6 tabs). A selection reaching a message's LAST
    ///   content row extends to the end of its source — the D4 whole-card
    ///   rule: truncated tool output, hidden thinking lines, and clamped
    ///   code tails come back in full.
    /// - `Chrome` rows contribute nothing; a chrome-only selection returns
    ///   `None` (D5: no clipboard write, no toast).
    ///
    /// Soft wraps vanish and hard breaks reappear for free because the
    /// emitted slice IS the original text — no joins, no indent heuristics,
    /// no viewport dependency (off-screen selections copy fine; design §3.3).
    /// Messages join with "\n\n" (§1.4 step 4); middle messages contribute
    /// their full source.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let a = self.selection_anchor.as_ref()?;
        let b = self.selection_end.as_ref()?;
        // Normalize to content reading order (same rule as selection_range).
        let (s, e) = if (a.msg_idx, a.line_in_msg, a.col) <= (b.msg_idx, b.line_in_msg, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        let cache = self.line_cache()?;

        let mut parts: Vec<String> = Vec::new();
        for mi in s.msg_idx..=e.msg_idx {
            let Some(entry) = cache.per_msg.get(mi) else { continue };
            let source = self.source_text(mi);
            let src: &str = &source;
            if src.is_empty() || entry.meta.is_empty() {
                continue;
            }
            // Middle messages contribute their full source (§1.4 step 2).
            if mi != s.msg_idx && mi != e.msg_idx {
                parts.push(src.to_string());
                continue;
            }

            let last_row = entry.meta.len() - 1;
            let row_lo = if mi == s.msg_idx { s.line_in_msg.min(last_row) } else { 0 };
            let row_hi = if mi == e.msg_idx { e.line_in_msg.min(last_row) } else { last_row };
            if row_lo > row_hi {
                continue;
            }
            // Endpoints on chrome snap inward to the nearest content row.
            let is_content = |r: usize| !matches!(entry.meta[r], LineMeta::Chrome);
            let Some(first) = (row_lo..=row_hi).find(|&r| is_content(r)) else {
                continue; // chrome end-to-end within this message (D5)
            };
            let last = (row_lo..=row_hi).rev().find(|&r| is_content(r)).unwrap_or(first);
            // The message's final content row — the D4 tail-rule trigger.
            let last_content_in_msg = entry.meta.iter().rposition(|m| !matches!(m, LineMeta::Chrome));

            let lo = if mi == s.msg_idx {
                match &entry.meta[first] {
                    LineMeta::Content { range, .. } if first == s.line_in_msg => {
                        s.src_byte.unwrap_or(range.start)
                    }
                    LineMeta::Content { range, .. } => range.start,
                    LineMeta::ContentLine { src_line, .. } => {
                        source_line_range(src, *src_line).start
                    }
                    LineMeta::Chrome => unreachable!("first is a content row"),
                }
            } else {
                0
            };
            let hi = if mi == e.msg_idx {
                match &entry.meta[last] {
                    LineMeta::Content { range, .. } if last == e.line_in_msg => {
                        e.src_byte.unwrap_or(range.end)
                    }
                    LineMeta::Content { range, .. } => range.end,
                    // D4: selecting through the card's last content row means
                    // "I want this output" — copy through the end of source
                    // (recovers renderer-truncated tails).
                    LineMeta::ContentLine { .. } if Some(last) == last_content_in_msg => src.len(),
                    LineMeta::ContentLine { src_line, .. } => {
                        source_line_range(src, *src_line).end
                    }
                    LineMeta::Chrome => unreachable!("last is a content row"),
                }
            } else {
                src.len()
            };

            let (lo, hi) = (lo.min(src.len()), hi.min(src.len()));
            if lo < hi {
                parts.push(src[lo..hi].to_string());
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
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
                // P11 narrowing (design §1.3 prerequisite): the delta only
                // changes messages[idx..] (idx itself, plus any later message
                // whose render reads back to it — covered by the k..n
                // re-render window, lock L1). A full invalidate here cost an
                // O(total) rebuild PER DELTA during tool streaming.
                self.invalidate_from(idx);
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
            // P11 narrowing + lock L2: idx IS the matched use index, so
            // dirtying from idx covers the stale-header trap directly. The
            // in-place ToolUseStart→ToolUse swap can also change the render
            // MODE of a matching ToolResult at any later index
            // (find_preceding_read_extension matches ToolUse only, not
            // ToolUseStart) — covered because the k..n re-render window
            // (lock L1) re-renders everything at idx.. on the next frame.
            self.invalidate_from(idx);
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
                    // P11 narrowing: output deltas change messages[idx..]
                    // only — the matched ToolUse's running header is keyed
                    // on the tool-timer maps, which this path doesn't touch.
                    self.invalidate_from(idx);
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

        // Lock L2 — the stale-header trap: stamping elapsed_ms (and dropping
        // the tool timer above) flips the matched ToolUse's header from
        // "⠋ running…" to done. The full invalidate this path used to issue
        // repainted it by accident; narrowed, we must dirty
        // min(use_idx, result_idx) explicitly or the header freezes at
        // "running" forever. Resolved by tool_id before the replace below.
        let use_idx = if tool_id.is_empty() {
            None
        } else {
            self.messages.iter().position(|m| matches!(
                &m.msg,
                ChatMessage::ToolUse { tool_id: tid, .. }
                | ChatMessage::ToolUseStart { tool_id: tid, .. }
                    if tid == &tool_id
            ))
        };

        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult { ref mut content, elapsed_ms, .. } = self.messages[idx].msg {
                if elapsed_ms.is_none() {
                    *content = result;
                    self.messages[idx].msg = ChatMessage::ToolResult {
                        tool_id,
                        content: std::mem::take(content),
                        elapsed_ms: elapsed,
                    };
                    self.invalidate_from(use_idx.map_or(idx, |u| u.min(idx)));
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

    // ── Query surface (§3.4) ─────────────────────────────────────────────

    /// Read-only slice of all messages. Use this for iteration and assertions;
    /// mutation must go through the push/on_tool_*/append_or_update_* API.
    pub(crate) fn messages(&self) -> &[TimestampedMsg] {
        &self.messages
    }

    /// Canonical copy source for message `idx` — see
    /// [`ChatMessage::source_text`] (design §2; the string [`LineMeta`]
    /// provenance indexes into).
    pub(crate) fn source_text(&self, idx: usize) -> std::borrow::Cow<'_, str> {
        self.messages[idx].msg.source_text()
    }

    /// True when the transcript contains no messages.
    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Number of messages in the transcript.
    ///
    /// Part of the designed query surface (seam design §3.4) — no production
    /// caller yet; P10 (selection/copy) and P11 (virtualization) consume it.
    #[allow(dead_code)]
    pub(crate) fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Clear all messages and fully invalidate the cache.
    /// Equivalent to `messages.clear()` + `invalidate()` in prior slices.
    pub(crate) fn clear(&mut self) {
        self.messages.clear();
        self.clear_selection();
        self.cache = CacheState::Missing;
    }

    /// Current scroll-back offset (0 = pinned to the latest line).
    /// Named `scroll_back_pos` to avoid colliding with the private field
    /// and the `scroll_back` field on [`VisibleWindow`].
    /// Query surface (seam design §3.4) — P10/P11 consumers pending.
    #[allow(dead_code)]
    pub(crate) fn scroll_back_pos(&self) -> u16 {
        self.scroll_back
    }

    /// Whether the viewport is pinned to the bottom (auto-scroll active).
    /// Query surface (seam design §3.4) — P10/P11 consumers pending.
    #[allow(dead_code)]
    pub(crate) fn is_pinned(&self) -> bool {
        self.scroll_pinned
    }

    /// Returns the elapsed time since the current tool started, if one is active.
    /// Used by render.rs (in `impl TranscriptStore`) to format elapsed displays —
    /// must go through this accessor because render.rs is in a sibling module.
    pub(crate) fn tool_start_time(&self) -> Option<std::time::Instant> {
        self.tool_start_time
    }

    /// Returns the per-tool start time for `tool_id`, if present.
    /// Query surface (seam design §3.4) — P10/P11 consumers pending.
    #[allow(dead_code)]
    pub(crate) fn tool_start_time_for(&self, tool_id: &str) -> Option<std::time::Instant> {
        self.tool_start_times.get(tool_id).copied()
    }

    /// Returns the dirty watermark index if the cache is in the `Dirty` state.
    /// `None` means either clean or missing. Tests use this to assert that
    /// `invalidate_last`/`invalidate_from` set the correct watermark.
    #[cfg(test)]
    pub(crate) fn cache_dirty_from(&self) -> Option<usize> {
        self.cache.dirty_from()
    }

    // ── Test-only seam (#[cfg(test)]) ────────────────────────────────────
    //
    // These methods give unit tests fine-grained control over internal state
    // without leaking that surface to production callers. They are the only
    // sanctioned back-doors through the sealed field boundary.

    /// Set the cache to `Clean` with the given `LineCache`. Tests use this to
    /// install a pre-built cache so they can assert incremental-rebuild logic
    /// without going through `visible_window`. Recomputes `cum_heights` so a
    /// hand-spliced cache can't violate the §0 identity assertion.
    #[cfg(test)]
    pub(crate) fn test_set_cache_clean(&mut self, mut cache: LineCache) {
        cache.rebuild_cum_from(0);
        self.cache = CacheState::Clean(cache);
    }

    /// Set the cache to `Dirty(cache, watermark)`. Used by tests that simulate
    /// the `invalidate_from` path without calling `visible_window`.
    #[cfg(test)]
    pub(crate) fn test_set_cache_dirty(&mut self, mut cache: LineCache, from: usize) {
        cache.rebuild_cum_from(0);
        self.cache = CacheState::Dirty(cache, from);
    }

    /// Consume and return the current `CacheState`, replacing it with `Missing`.
    /// Tests use this to implement incremental-rebuild helpers inline.
    #[cfg(test)]
    pub(crate) fn test_take_cache(&mut self) -> CacheState {
        std::mem::replace(&mut self.cache, CacheState::Missing)
    }

    /// Put a `CacheState` back after a test manipulation.
    #[cfg(test)]
    pub(crate) fn test_put_cache(&mut self, cs: CacheState) {
        self.cache = cs;
    }

    /// Mutable access to the last message for test-side content updates
    /// (e.g. simulating a streaming delta that `append_or_update_text` would
    /// do through the real API in production).
    #[cfg(test)]
    pub(crate) fn test_last_msg_mut(&mut self) -> Option<&mut TimestampedMsg> {
        self.messages.last_mut()
    }

    /// Insert a `TimestampedMsg` at `idx` for structural tests.
    #[cfg(test)]
    pub(crate) fn test_insert_at(&mut self, idx: usize, msg: TimestampedMsg) {
        self.messages.insert(idx, msg);
    }

    /// Set `tool_start_time` for tests that exercise the spinner / elapsed path.
    #[cfg(test)]
    pub(crate) fn test_set_tool_start_time(&mut self, t: Option<std::time::Instant>) {
        self.tool_start_time = t;
    }

    /// Insert a per-tool start-time entry for tests that exercise parallel-tool
    /// elapsed rendering.
    #[cfg(test)]
    pub(crate) fn test_insert_tool_start_time(&mut self, id: String, t: std::time::Instant) {
        self.tool_start_times.insert(id, t);
    }

    /// Directly set `scroll_back` for scroll unit tests that need to start from
    /// a known offset without going through `scroll_up`.
    #[cfg(test)]
    pub(crate) fn test_set_scroll_back(&mut self, v: u16) {
        self.scroll_back = v;
        self.scroll_pinned = v == 0;
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
    ///   2. `lines` content == the reference render's [start..end] window
    ///      (`render_lines` is the surviving §4 oracle).
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

        let total = store.line_cache().expect("line cache populated after layout").total_height();
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

        // 2. Content must match the reference render's [start..end] window
        //    (post flat-kill: render_lines is the oracle, not a live buffer).
        let oracle = store.render_lines(content_width, &test_ctx());
        assert_eq!(oracle.len(), total, "oracle render must reproduce total_height");
        assert_eq!(
            to_str(&vw.lines),
            to_str(&oracle[start..end]),
            "published window content must equal the reference render's [start..end]"
        );

        // 3. Render-thread side: visible = model.lines.to_vec() (no re-slice).
        let visible_render: Vec<ratatui::text::Line> = vw.lines.to_vec();
        assert_eq!(
            to_str(&visible_render),
            to_str(&oracle[start..end]),
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

#[cfg(test)]
mod source_text_tests {
    //! P10 slice (c): per-variant source extraction — the design §2 table,
    //! made executable. `source_text` is the string copy emits; every
    //! variant's canonical form is pinned here.
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn user_is_raw_input_verbatim() {
        let m = ChatMessage::User("hello  world\nwith a second line".into());
        assert_eq!(m.source_text(), "hello  world\nwith a second line");
    }

    #[test]
    fn user_paste_placeholder_copies_as_stored_display_form() {
        // D1 as locked: the transcript stores the display form; copy emits it.
        let m = ChatMessage::User("[Pasted 42 lines]".into());
        assert_eq!(m.source_text(), "[Pasted 42 lines]");
    }

    #[test]
    fn text_is_raw_markdown_with_markers_fences_and_tables() {
        let src = "some **bold** text\n\n```rust\nlet x = 1;\n```\n| a | b |\n|---|---|";
        let m = ChatMessage::Text(src.into());
        assert_eq!(m.source_text(), src);
    }

    #[test]
    fn thinking_is_raw_markdown() {
        let m = ChatMessage::Thinking("- consider\n- decide".into());
        assert_eq!(m.source_text(), "- consider\n- decide");
    }

    #[test]
    fn thinking_placeholder_is_empty_source() {
        // The spinner sentinel is chrome, not content (design §2 table).
        let m = ChatMessage::Thinking(THINKING_PLACEHOLDER.into());
        assert_eq!(m.source_text(), "");
    }

    #[test]
    fn tool_use_start_is_the_raw_partial_fragment() {
        let m = ChatMessage::ToolUseStart {
            tool_id: "t".into(),
            tool_name: "write".into(),
            partial_input: r#"{"path": "a.rs", "content": "fn ma"#.into(),
        };
        assert_eq!(m.source_text(), r#"{"path": "a.rs", "content": "fn ma"#);
    }

    #[test]
    fn tool_use_is_pretty_printed_input_json_lock_l5() {
        let m = ChatMessage::ToolUse {
            tool_id: "t".into(),
            tool_name: "bash".into(),
            input: r#"{"command":"ls -la","timeout":30}"#.into(),
        };
        let src = m.source_text();
        // Valid, pretty-printed, key/value line-mappable JSON.
        assert_eq!(
            src,
            "{\n  \"command\": \"ls -la\",\n  \"timeout\": 30\n}",
            "L5: ToolUse source must be serde_json pretty form"
        );
        // Owned (computed at copy time) — the raw compact form is one flip away.
        assert!(matches!(src, Cow::Owned(_)));
    }

    #[test]
    fn tool_use_unparseable_input_falls_back_to_raw_bytes() {
        let m = ChatMessage::ToolUse {
            tool_id: "t".into(),
            tool_name: "bash".into(),
            input: "not json {".into(),
        };
        assert_eq!(m.source_text(), "not json {");
        assert!(matches!(m.source_text(), Cow::Borrowed(_)));
    }

    #[test]
    fn tool_result_is_raw_stored_output_untruncated() {
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let m = ChatMessage::ToolResult {
            tool_id: "t".into(),
            content: long.clone(),
            elapsed_ms: Some(3),
        };
        // Un-truncated even though the renderer shows 12–15 lines.
        assert_eq!(m.source_text(), long.as_str());
    }

    #[test]
    fn error_system_are_verbatim() {
        assert_eq!(ChatMessage::Error("boom\ntail".into()).source_text(), "boom\ntail");
        assert_eq!(ChatMessage::System("notice".into()).source_text(), "notice");
    }

    #[test]
    fn event_text_only_source_tag_is_chrome() {
        let m = ChatMessage::Event {
            source: "mail".into(),
            severity: "high".into(),
            text: "inbox message arrived".into(),
        };
        // Routing metadata ([source], severity) never copies (design §2).
        assert_eq!(m.source_text(), "inbox message arrived");
    }

    #[test]
    fn store_source_text_delegates_per_index() {
        let mut store = TranscriptStore::new();
        store.push_msg(ChatMessage::User("u".into()));
        store.push_msg(ChatMessage::Text("t".into()));
        assert_eq!(store.source_text(0), "u");
        assert_eq!(store.source_text(1), "t");
    }
}
