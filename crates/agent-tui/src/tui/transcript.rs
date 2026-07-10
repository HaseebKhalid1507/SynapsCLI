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

pub(crate) mod estimate;

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
    Event {
        source: String,
        severity: String,
        text: String,
    },
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
            ChatMessage::ToolUseStart { partial_input, .. } => {
                Cow::Borrowed(partial_input.as_str())
            }
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
    Content {
        range: std::ops::Range<usize>,
        content_col: u16,
    },
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

// T241 Slice 1: HeightState + meta: Option<Vec<LineMeta>>.
// All Slice-1 slots are Exact with meta: Some(_). Estimated slots with
// meta: None are introduced in Slice 3 (the Missing-arm rewrite).

/// Per-slot height: exact (from a full render) or estimated (from the cheap
/// source-byte estimator in `estimate.rs`). Use `.value()` for the coordinate.
///
/// Introduced in T241 Slice 1. All code paths still produce `Exact` here;
/// `Estimated` slots are first created in Slice 3 (the Missing-arm rewrite).
#[allow(dead_code)] // Estimated variant used from Slice 3 onward
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeightState {
    /// Height from a full `render_message_lines` call at `LineCache::width`.
    /// The slot's `meta` is `Some(_)` and parallel to `lines`.
    Exact(usize),
    /// Cheap estimate from `estimate::estimate_message_height`. The slot's
    /// `meta` is `None`; exact row provenance is not yet known.
    Estimated(usize),
}

impl HeightState {
    /// The numeric height value (rows), regardless of exactness.
    #[inline]
    pub(crate) fn value(&self) -> usize {
        match self {
            HeightState::Exact(n) | HeightState::Estimated(n) => *n,
        }
    }

    /// `true` when the height came from a full render.
    #[allow(dead_code)] // used in Slice 3+
    #[inline]
    pub(crate) fn is_exact(&self) -> bool {
        matches!(self, HeightState::Exact(_))
    }
}

pub(crate) struct MsgSlot {
    /// Present only while the message intersects the viewport +/-
    /// the retention halo (design §3 step 7).
    pub(crate) lines: Option<Vec<ratatui::text::Line<'static>>>,
    /// Row provenance -- `Some` when this slot has been exactly rendered;
    /// `None` for `Estimated` slots that have never been through
    /// `render_message_lines`. When `lines` is `Some`, `meta` is `Some` and
    /// `meta.as_ref().unwrap().len() == lines.as_ref().unwrap().len()`.
    ///
    /// **Slice 1:** all slots are produced by `render_message_lines` and
    /// therefore always `Some`. Callers that access `meta` use
    /// `.as_deref().unwrap_or(&[])` so that Slice 3 (Estimated slots) compiles
    /// without further churn.
    pub(crate) meta: Option<Vec<LineMeta>>,
    /// The row height of this message -- exact or estimated. Use `.value()` to
    /// get the usize coordinate; match the enum when correctness matters.
    pub(crate) height: HeightState,
}

impl MsgSlot {
    /// The message's height in display rows -- valid for both Exact and
    /// Estimated slots. Replaces the old `self.meta.len()` derivation.
    #[inline]
    pub(crate) fn height(&self) -> usize {
        self.height.value()
    }

    /// The rendered rows. Panics on a demoted slot -- callers must hold the
    /// "promoted first" invariant (`visible_window` promotes window slots
    /// before assembly; the sync paths only touch freshly rendered slots).
    /// Callers that can legitimately see a demoted slot must match on the
    /// `Option` explicitly (lock L3) instead.
    #[track_caller]
    pub(crate) fn lines(&self) -> &[ratatui::text::Line<'static>] {
        self.lines
            .as_deref()
            .expect("MsgSlot demoted -- promote (re-render) before reading lines")
    }

    /// Row provenance as a slice -- empty for Estimated slots (meta is None).
    /// Used by callers that need `meta` but can tolerate an empty slice when
    /// the slot is estimated. Meta survives demotion (lines=None) unchanged.
    #[inline]
    pub(crate) fn meta_slice(&self) -> &[LineMeta] {
        self.meta.as_deref().unwrap_or(&[])
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
        let mut cache = LineCache {
            width,
            per_msg,
            cum_heights: Vec::new(),
        };
        cache.rebuild_cum_from(0);
        cache
    }

    /// Rebuild `cum_heights` from message index `k` — the cumulative-offset
    /// cache is invalidated with the same watermark as the slots it sums.
    /// Returns the number of entries written (perf probe, lock L4).
    pub(crate) fn rebuild_cum_from(&mut self, k: usize) -> usize {
        let k = k
            .min(self.per_msg.len())
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
    #[allow(dead_code)] // test helper, kept for future cache tests
    pub(crate) fn mark_clean(&mut self) {
        if matches!(self, CacheState::Dirty(..)) {
            let CacheState::Dirty(c, _) = std::mem::replace(self, CacheState::Missing) else {
                unreachable!()
            };
            *self = CacheState::Clean(c);
        }
    }
}

// ── T241 Slice 2: ScrollAnchor data model ────────────────────────────────────
//
// **Scroll representation audit (Slice 2 — was ⚠️UNVERIFIED in scope §1.8):**
//
// `TranscriptStore` stores scroll position as `scroll_back: u16` —
// **offset-from-bottom**, counting rows from the last content row upward.
//   - `scroll_back == 0` means pinned to the bottom (latest content visible).
//   - The visible window in height-space is:
//       end   = total_height − scroll_back
//       start = end − content_height          (clamped to 0)
//       S_top = start                          (top visible content row)
//   - Therefore: `S_top = total_height − scroll_back − content_height` (clamped).
//   - And inversely: `scroll_back = total_height − content_height − S_top` (clamped).
//
// This is the representation used throughout `visible_window` (transcript.rs
// lines 990–991 before this edit). `ScrollAnchor` is representation-independent:
// conversions go through `S_top` as an intermediate, then convert back to
// `scroll_back` for the live field.
//
// **Pinned mode vs. anchored mode:**
// `None` (absent) = pinned to bottom — `S_top` is recomputed each frame from
// `total_height` and `content_height`. This is the default state and is NOT
// stored as a coordinate, because any height correction above the fold that
// changed `total_height` would require no update to a pinned view.
//
// `Some(ScrollAnchor { msg_idx, row_in_msg })` = anchored away from bottom —
// the user has scrolled up. `msg_idx` is the message whose first content row
// is closest to the top of the viewport, and `row_in_msg` is how many rows
// into that message the top of the viewport is (0 = the very first row of
// the message).
//
// **The no-jump theorem (§4.3):** a height correction Δ to message j:
//   j > anchor.msg_idx  → cum[anchor.msg_idx] unchanged → S_top unchanged → no visual motion
//   j < anchor.msg_idx  → cum[anchor.msg_idx] += Δ      → S_top += Δ → anchor msg stays
//                          at same screen row; content coordinate shifted, not visual
//   j == anchor.msg_idx → clamp row_in_msg to min(row_in_msg, new_height − 1) → bounded motion
//
// **Slice 2 shadow-only policy:**
// The `scroll_anchor` field is kept in sync with every `scroll_back` mutation
// but does NOT drive rendering yet. All rendering still uses the raw
// `scroll_back` field (eager behavior preserved, screens byte-identical).
// Slice 3 rewrites the Missing-arm and `promote_window`, at which point the
// anchor will be used as the coordinate source for S_top recomputation.

/// A viewport anchor away from the bottom of the transcript.
///
/// When the user scrolls up, we capture the message + intra-message row that
/// is at the top of the visible window. Future height corrections (Slice 3)
/// use this to recompute `S_top` without visual jumps (§4.3 correction
/// theorem).
///
/// Absent (`None` on `TranscriptStore.scroll_anchor`) means the view is
/// **pinned to the bottom** — `S_top` is recomputed from `total_height` each
/// frame and the anchor is not a coordinate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScrollAnchor {
    /// Index into `TranscriptStore.messages` of the message whose rows
    /// contain the topmost visible content row.
    pub(crate) msg_idx: usize,
    /// Row offset **from the top of the anchor message** to the topmost
    /// visible content row. `0` means the very first row of the message
    /// is at the top of the viewport. Always `< h_i` where `h_i` is the
    /// height of the anchor message.
    pub(crate) row_in_msg: usize,
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
/// Slice 2 (T241): `scroll_anchor` shadow field (§4.3); kept in sync with
/// `scroll_back` mutations but not yet used for rendering (Slice 3 activates).
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
    /// T241 Slice 2: anchor in message-space for use by Slice 3 height-
    /// correction (§4.3 no-jump theorem).
    ///
    /// `None`  = pinned to bottom (default). S_top is recomputed from
    ///           `total_height` and `content_height` each frame.
    /// `Some`  = anchored away from bottom. The anchor msg/row identifies
    ///           the content coordinate of the top-of-viewport. Slice 3
    ///           uses it as the coordinate source after height corrections.
    ///
    /// **Shadow-only in Slice 2**: kept in sync with every `scroll_back`
    /// mutation but does NOT yet drive rendering. Slice 3 activates it.
    scroll_anchor: Option<ScrollAnchor>,

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

    // ── Injectable clock (P6.2) ──────────────────────────────────────────────
    /// Real in production, Test in the harness. Backs the per-tool start
    /// timestamps so tool-timer state is deterministic under test.
    clock: super::clock::TuiClock,

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
    pub(crate) fn new(clock: super::clock::TuiClock) -> Self {
        Self {
            clock,
            messages: Vec::new(),
            scroll_back: 0,
            scroll_pinned: true,
            last_line_count: 0,
            scroll_anchor: None,
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
        self.probe
            .renders
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cumulative-offset entry writes since the last [`Self::probe_reset`].
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_cum_write_count(&self) -> usize {
        self.probe
            .cum_writes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Zero both perf counters — call after a warm-up frame, before the
    /// frame under measurement.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_reset(&self) {
        self.probe
            .renders
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.probe
            .cum_writes
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Internal bump seam for `render_message_lines` (render.rs — a sibling
    /// module; store fields are sealed, so the bump routes through a method).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn probe_note_render(&self) {
        self.probe
            .renders
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // ── Scroll API ────────────────────────────────────────────────────────────
    //
    // These methods do NOT touch selection state — see the module-level
    // scroll/selection note (lock L4): wheel scroll preserves selection,
    // keypresses clear it in input.rs::handle_key.

    // ── T241 Slice 2: anchor conversion / capture / restore / clamp ──────────
    //
    // These are PURE functions of their arguments — no side effects, no
    // `self` mutation. They are the mathematical kernel for §4.3; Slice 3
    // will call them from `promote_window` and the Missing-arm rewrite.
    //
    // All functions use `S_top` (top visible content row, 0-based in
    // cumulative-height space) as the intermediate representation.
    //
    // Relation to `scroll_back` (offset-from-bottom):
    //   scroll_back = (total_height - content_height - S_top).clamp(0, max_back)
    //   S_top       = (total_height - scroll_back - content_height).max(0)
    //   where max_back = total_height.saturating_sub(content_height)

    /// Convert a cumulative `scroll_back` offset + known totals to `S_top`.
    ///
    /// `total_height`: `cum_heights[n]` — current total rows in the cache.
    /// `content_height`: viewport rows (inner, sans borders).
    /// Returns 0 if the view is over-scrolled or the transcript is empty.
    #[inline]
    pub(crate) fn scroll_back_to_stop(
        scroll_back: usize,
        total_height: usize,
        content_height: usize,
    ) -> usize {
        total_height
            .saturating_sub(scroll_back)
            .saturating_sub(content_height)
    }

    /// Convert `S_top` to a `scroll_back` offset, clamped to valid range.
    ///
    /// `total_height`: `cum_heights[n]`.
    /// `content_height`: viewport rows (inner, sans borders).
    /// The result is always `≤ total_height.saturating_sub(content_height)`.
    #[allow(dead_code)] // Slice 3 uses this; tests use it via cfg(test)
    #[inline]
    pub(crate) fn stop_to_scroll_back(
        s_top: usize,
        total_height: usize,
        content_height: usize,
    ) -> usize {
        let max_back = total_height.saturating_sub(content_height);
        total_height
            .saturating_sub(content_height)
            .saturating_sub(s_top)
            .min(max_back)
    }

    /// Capture a [`ScrollAnchor`] from the current scroll position and cache.
    ///
    /// Returns `None` when:
    /// - the cache is absent (no heights yet),
    /// - the transcript is empty,
    /// - `scroll_back == 0` (view is pinned to bottom — caller uses pinned mode,
    ///   not an anchor coordinate),
    /// - or the cache is empty after drain.
    ///
    /// The anchor identifies the message whose range in height-space straddles
    /// `s_top`, and the intra-message row offset.
    pub(crate) fn capture_anchor(
        cache: &LineCache,
        scroll_back: usize,
        content_height: usize,
    ) -> Option<ScrollAnchor> {
        if cache.per_msg.is_empty() || scroll_back == 0 {
            return None;
        }
        let total = cache.total_height();
        let s_top = Self::scroll_back_to_stop(scroll_back, total, content_height);

        // Binary search: find the message `i` such that
        //   cum_heights[i] <= s_top < cum_heights[i+1]
        // `partition_point(|c| c <= s_top)` gives the first index k where
        // cum_heights[k] > s_top, so our message is at k-1 (clamped to 0).
        let k = cache
            .cum_heights
            .partition_point(|&c| c <= s_top)
            .saturating_sub(1)
            .min(cache.per_msg.len() - 1);

        let row_in_msg = s_top.saturating_sub(cache.cum_heights[k]);
        // Clamp against the slot's actual height (safety for zero-height slots).
        let h = cache.per_msg[k].height().max(1);
        let row_in_msg = row_in_msg.min(h - 1);

        Some(ScrollAnchor {
            msg_idx: k,
            row_in_msg,
        })
    }

    /// Restore `scroll_back` from a [`ScrollAnchor`] and the current cache.
    ///
    /// Applies the no-jump theorem (§4.3): the anchor is used to recompute
    /// `S_top` even after height corrections to messages other than the
    /// anchor, producing a corrected `scroll_back` that keeps the anchor
    /// message at the same screen row.
    ///
    /// Edge cases handled:
    /// - Empty cache / zero viewport: returns 0.
    /// - Stale `msg_idx` (past end of messages after cap/drain): clamps to
    ///   the last message.
    /// - Anchor message height shrank below `row_in_msg`: clamps `row_in_msg`.
    /// - Pinned bottom (`anchor == None`): recomputes `scroll_back = 0`
    ///   (caller's responsibility; this function handles `Some` only).
    #[allow(dead_code)] // Slice 3 activates; tests use via cfg(test)
    pub(crate) fn anchor_to_scroll_back(
        anchor: &ScrollAnchor,
        cache: &LineCache,
        content_height: usize,
    ) -> usize {
        if cache.per_msg.is_empty() || content_height == 0 {
            return 0;
        }
        let total = cache.total_height();
        if total == 0 {
            return 0;
        }

        // Clamp stale msg_idx (e.g. after cap/drain).
        let msg_idx = anchor.msg_idx.min(cache.per_msg.len() - 1);

        // Clamp row_in_msg to the anchor message's current height.
        let h = cache.per_msg[msg_idx].height().max(1);
        let row_in_msg = anchor.row_in_msg.min(h - 1);

        // S_top = cum_heights[msg_idx] + row_in_msg, clamped.
        let s_top = cache.cum_heights[msg_idx].saturating_add(row_in_msg);

        // Convert back to scroll_back (clamped via stop_to_scroll_back).
        Self::stop_to_scroll_back(s_top, total, content_height)
    }

    /// Clamp `scroll_back` to the valid range `[0, max_back]` given the
    /// current total height and content height. Also updates `scroll_pinned`.
    ///
    /// Called after any correction that may shift `total_height` (Slice 3).
    /// Idempotent: safe to call even when already clamped.
    #[allow(dead_code)] // Slice 3 activates; tests use via cfg(test)
    #[inline]
    pub(crate) fn clamp_scroll_back(scroll_back: usize, total_height: usize, content_height: usize) -> usize {
        total_height.saturating_sub(content_height).min(scroll_back)
    }

    /// Sync the shadow anchor to match the current `scroll_back` + cache.
    ///
    /// Called at the end of every scroll mutation that changes `scroll_back`.
    /// In Slice 2 this is a shadow-only operation: it keeps `scroll_anchor`
    /// consistent with `scroll_back` so Slice 3 can rely on it being correct.
    ///
    /// When `scroll_back == 0` the anchor is cleared (`None` = pinned bottom).
    fn sync_anchor_from_scroll_back(&mut self) {
        if self.scroll_back == 0 {
            self.scroll_anchor = None;
            return;
        }
        // Only capture if we have a live cache with heights.
        let Some(cache) = self.cache.line_cache() else {
            // No cache yet — can't capture a meaningful anchor.
            // The anchor will be captured on the next `visible_window` call
            // that populates the cache.
            self.scroll_anchor = None;
            return;
        };
        // Use a placeholder content_height for the shadow capture.
        // This is fine because the anchor's (msg_idx, row_in_msg) is
        // content-height-independent; only the scroll_back↔S_top conversion
        // needs a content_height, and Slice 3 will always supply the live one.
        //
        // We use the viewport stored from the last visible_window call.
        // If no viewport yet, use 40 as a safe placeholder (the anchor will
        // be re-captured on the next frame with the real height).
        let content_height = self
            .viewport
            .map(|r| r.height as usize)
            .unwrap_or(40);
        self.scroll_anchor =
            Self::capture_anchor(cache, self.scroll_back as usize, content_height);
    }

    /// Scroll up (away from bottom) by `lines`. Unpins the viewport.
    pub(crate) fn scroll_up(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_add(lines);
        self.scroll_pinned = false;
        // Slice 2: keep shadow anchor in sync.
        self.sync_anchor_from_scroll_back();
    }

    /// Scroll down (toward bottom) by `lines`. Re-pins at 0.
    pub(crate) fn scroll_down(&mut self, lines: u16) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
        if self.scroll_back == 0 {
            self.scroll_pinned = true;
        }
        // Slice 2: keep shadow anchor in sync.
        self.sync_anchor_from_scroll_back();
    }

    /// Reset scroll to bottom and pin.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll_back = 0;
        self.scroll_pinned = true;
        // Slice 2: pinned bottom → clear anchor.
        self.scroll_anchor = None;
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
    pub(crate) fn push_tool_result(
        &mut self,
        tool_id: String,
        content: String,
        elapsed_ms: Option<u64>,
    ) {
        let use_idx = if tool_id.is_empty() {
            None
        } else {
            // Invariant: each tool_id should appear at most once. Assert in
            // debug builds so duplicate IDs surface immediately.
            debug_assert!(
                self.messages
                    .iter()
                    .filter(|m| matches!(
                        &m.msg,
                        ChatMessage::ToolUse { tool_id: tid, .. }
                        | ChatMessage::ToolUseStart { tool_id: tid, .. }
                            if tid == &tool_id
                    ))
                    .count()
                    <= 1,
                "push_tool_result: duplicate ToolUse/ToolUseStart for tool_id={tool_id:?}"
            );
            self.messages.iter().position(|m| {
                matches!(
                    &m.msg,
                    ChatMessage::ToolUse { tool_id: tid, .. }
                    | ChatMessage::ToolUseStart { tool_id: tid, .. }
                        if tid == &tool_id
                )
            })
        };
        let msg = ChatMessage::ToolResult {
            tool_id,
            content,
            elapsed_ms,
        };
        match use_idx {
            Some(i) => {
                let at = (i + 1).min(self.messages.len());
                self.messages.insert(
                    at,
                    TimestampedMsg {
                        msg,
                        time: chrono::Local::now().format("%H:%M").to_string(),
                    },
                );
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
        let needs_full_rebuild = self.line_cache().map_or(true, |c| c.width != content_width);

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
            self.probe.cum_writes.fetch_add(
                cache.cum_heights.len(),
                std::sync::atomic::Ordering::Relaxed,
            );
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
        // Slice 2: re-sync shadow anchor after the growth-adjust/clamp pass.
        // viewport field not yet set at this point, so supply content_height directly.
        if self.scroll_back == 0 {
            self.scroll_anchor = None;
        } else if let Some(cache) = self.cache.line_cache() {
            self.scroll_anchor = Self::capture_anchor(cache, self.scroll_back as usize, content_height);
        }
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
                    (f..=l)
                        .filter(|&mi| c.per_msg[mi].lines.is_none())
                        .collect()
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
        let first = cache
            .cum_heights
            .partition_point(|&c| c <= start)
            .saturating_sub(1);
        let last = cache
            .cum_heights
            .partition_point(|&c| c < end)
            .saturating_sub(1);
        Some((first, last))
    }

    /// Assemble the visible window `[start..end)` from per-slot lines
    /// (design §3 steps 4–6): partial slices of the first/last slots plus
    /// whole middles. A message straddling an edge is rendered fully and
    /// sliced — line-granular windows require it, bounded by one message.
    fn assemble_window(&self, start: usize, end: usize) -> Vec<ratatui::text::Line<'static>> {
        let Some(cache) = self.line_cache() else {
            return Vec::new();
        };
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
                    fresh.height(),
                    slot.height(),
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
        Some(SelPos {
            msg_idx,
            line_in_msg,
            col,
            src_byte,
        })
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
        let LineMeta::Content { range, content_col } = entry.meta_slice().get(line_in_msg)? else {
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
        let (sf, sc) = if sf < vis_start {
            (vis_start, 0)
        } else {
            (sf, s.col.min(max_col))
        };
        let (ef, ec) = if ef >= vis_end {
            (vis_end - 1, max_col)
        } else {
            (ef, e.col.min(max_col))
        };
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
            col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
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
            let Some(entry) = cache.per_msg.get(mi) else {
                continue;
            };
            let source = self.source_text(mi);
            let src: &str = &source;
            if src.is_empty() || entry.meta_slice().is_empty() {
                continue;
            }
            // Middle messages contribute their full source (§1.4 step 2).
            if mi != s.msg_idx && mi != e.msg_idx {
                parts.push(src.to_string());
                continue;
            }

            let last_row = entry.meta_slice().len().saturating_sub(1);
            let row_lo = if mi == s.msg_idx {
                s.line_in_msg.min(last_row)
            } else {
                0
            };
            let row_hi = if mi == e.msg_idx {
                e.line_in_msg.min(last_row)
            } else {
                last_row
            };
            if row_lo > row_hi {
                continue;
            }
            // Endpoints on chrome snap inward to the nearest content row.
            let is_content = |r: usize| !matches!(entry.meta_slice()[r], LineMeta::Chrome);
            let Some(first) = (row_lo..=row_hi).find(|&r| is_content(r)) else {
                continue; // chrome end-to-end within this message (D5)
            };
            let last = (row_lo..=row_hi)
                .rev()
                .find(|&r| is_content(r))
                .unwrap_or(first);
            // The message's final content row — the D4 tail-rule trigger.
            let last_content_in_msg = entry
                .meta_slice()
                .iter()
                .rposition(|m| !matches!(m, LineMeta::Chrome));

            let lo = if mi == s.msg_idx {
                match &entry.meta_slice()[first] {
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
                match &entry.meta_slice()[last] {
                    LineMeta::Content { range, .. } if last == e.line_in_msg => {
                        e.src_byte.unwrap_or(range.end)
                    }
                    LineMeta::Content { range, .. } => range.end,
                    // D4: selecting through the card's last content row means
                    // "I want this output" — copy through the end of source
                    // (recovers renderer-truncated tails).
                    LineMeta::ContentLine { .. } if Some(last) == last_content_in_msg => src.len(),
                    LineMeta::ContentLine { src_line, .. } => source_line_range(src, *src_line).end,
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
        self.messages
            .iter()
            .enumerate()
            .any(|(idx, msg)| match &msg.msg {
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
            Some(ChatMessage::ToolResult {
                tool_id,
                elapsed_ms: None,
                ..
            }) => self.tool_start_times.contains_key(tool_id),
            _ => false,
        }
    }

    /// Find the file extension from the ToolUse message preceding a ToolResult at index `idx`.
    pub(crate) fn find_preceding_read_extension(&self, idx: usize) -> String {
        // Prefer matching by tool_id when the result carries one — under
        // parallel tool calls a `ToolResult` may not be positionally
        // adjacent to its matching `ToolUse`.
        let target_id: Option<String> = match self.messages.get(idx).map(|m| &m.msg) {
            Some(ChatMessage::ToolResult { tool_id, .. }) if !tool_id.is_empty() => {
                Some(tool_id.clone())
            }
            _ => None,
        };
        if let Some(id) = target_id {
            for m in self.messages.iter() {
                if let ChatMessage::ToolUse {
                    tool_id,
                    tool_name,
                    input,
                } = &m.msg
                {
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
        if idx == 0 {
            return String::new();
        }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse {
                ref tool_name,
                ref input,
                ..
            } = self.messages[i].msg
            {
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
        if let Some(ChatMessage::ToolResult { tool_id, .. }) =
            self.messages.get(idx).map(|m| &m.msg)
        {
            if !tool_id.is_empty() {
                for m in self.messages.iter() {
                    match &m.msg {
                        ChatMessage::ToolUse {
                            tool_id: tid,
                            tool_name,
                            ..
                        }
                        | ChatMessage::ToolUseStart {
                            tool_id: tid,
                            tool_name,
                            ..
                        } if tid == tool_id => {
                            return Some(tool_name.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        if idx == 0 {
            return None;
        }
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
        self.messages.iter().rposition(|m| {
            matches!(
                &m.msg,
                ChatMessage::ToolUseStart { tool_id: tid, .. } if tid == tool_id
            )
        })
    }

    /// Locate the latest `ToolResult` block for this `tool_id`.
    pub(crate) fn find_tool_result_idx(&self, tool_id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| {
            matches!(
                &m.msg,
                ChatMessage::ToolResult { tool_id: tid, .. } if tid == tool_id
            )
        })
    }

    /// Begin streaming a new tool call. Records start time per-tool so
    /// elapsed-ms is correct under parallel execution.
    pub(crate) fn on_tool_use_start(&mut self, tool_id: String, tool_name: String) {
        self.drop_empty_thinking();
        let now = self.clock.now();
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
            self.messages
                .iter()
                .rposition(|m| matches!(&m.msg, ChatMessage::ToolUseStart { .. }))
        };
        if let Some(idx) = target_idx {
            if let ChatMessage::ToolUseStart {
                ref mut partial_input,
                ..
            } = self.messages[idx].msg
            {
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
    pub(crate) fn on_tool_use_finalized(
        &mut self,
        tool_id: String,
        tool_name: String,
        input_str: String,
    ) {
        self.drop_empty_thinking();
        // Track start time even if we never saw a ToolUseStart (some
        // providers go straight to a finalized tool_use).
        let now = self.clock.now();
        if !tool_id.is_empty() {
            self.tool_start_times.entry(tool_id.clone()).or_insert(now);
        }
        self.tool_start_time = Some(now);

        if let Some(idx) = self.find_tool_use_start_idx(&tool_id) {
            self.messages[idx].msg = ChatMessage::ToolUse {
                tool_id,
                tool_name,
                input: input_str,
            };
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
        self.push_msg(ChatMessage::ToolUse {
            tool_id,
            tool_name,
            input: input_str,
        });
    }

    /// Stream a chunk of tool output. Appends to the matching
    /// `ToolResult` if one exists, otherwise creates a new placeholder.
    pub(crate) fn on_tool_result_delta(&mut self, tool_id: String, delta: String) {
        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult {
                ref mut content,
                elapsed_ms,
                ..
            } = self.messages[idx].msg
            {
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
            self.messages.iter().position(|m| {
                matches!(
                    &m.msg,
                    ChatMessage::ToolUse { tool_id: tid, .. }
                    | ChatMessage::ToolUseStart { tool_id: tid, .. }
                        if tid == &tool_id
                )
            })
        };

        if let Some(idx) = self.find_tool_result_idx(&tool_id) {
            if let ChatMessage::ToolResult {
                ref mut content,
                elapsed_ms,
                ..
            } = self.messages[idx].msg
            {
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
        if let Some(TimestampedMsg {
            msg: ChatMessage::Text(ref mut existing),
            ..
        }) = self.messages.last_mut()
        {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.invalidate_last();
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg {
            msg: ChatMessage::Thinking(ref mut existing),
            ..
        }) = self.messages.last_mut()
        {
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
        let candidate_idx = self
            .messages
            .iter()
            .rposition(|m| !matches!(&m.msg, ChatMessage::System(_)));
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
    #[allow(dead_code)] // test helper, kept for future cache tests
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
    #[allow(dead_code)] // test helper, kept for future cache tests
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

    // ── Scroll anchor query / mutation (test + Slice 3) ──────────────────────

    /// Read the current shadow scroll anchor.
    ///
    /// `None` = pinned to bottom. `Some(a)` = anchored at message `a.msg_idx`,
    /// row `a.row_in_msg` from the top of that message.
    ///
    /// Public for Slice 3's `promote_window` and for tests.
    #[allow(dead_code)] // Slice 3 activates production use
    pub(crate) fn scroll_anchor(&self) -> Option<&ScrollAnchor> {
        self.scroll_anchor.as_ref()
    }

    /// Directly set the shadow anchor — for tests that need to place the
    /// anchor at a known position before calling conversion functions.
    #[cfg(test)]
    #[allow(dead_code)] // test helper; used when anchor-mutation tests land
    pub(crate) fn test_set_scroll_anchor(&mut self, anchor: Option<ScrollAnchor>) {
        self.scroll_anchor = anchor;
    }
}

impl Default for TranscriptStore {
    fn default() -> Self {
        Self::new(super::clock::TuiClock::real())
    }
}

#[cfg(test)]
mod visible_window_tests {
    use super::*;

    fn test_ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "synaps",
        }
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
        let mut store = TranscriptStore::new(super::super::clock::TuiClock::real());

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

        let total = store
            .line_cache()
            .expect("line cache populated after layout")
            .total_height();
        assert!(total >= 20, "sanity: need >= 20 flat lines, got {total}");
        let end = total; // scroll_back = 0
        let start = end.saturating_sub(content_height);

        let to_str = |sl: &[ratatui::text::Line<'static>]| -> Vec<String> {
            sl.iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
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
        assert_eq!(
            vw.lines_width, content_width,
            "lines_width must be the content width"
        );
        assert_eq!(
            vw.scroll_back, 0,
            "pinned → post-clamp scroll_back must be 0"
        );
        assert!(!vw.is_empty, "20 messages → is_empty must be false");

        // 2. Content must match the reference render's [start..end] window
        //    (post flat-kill: render_lines is the oracle, not a live buffer).
        let oracle = store.render_lines(content_width, &test_ctx());
        assert_eq!(
            oracle.len(),
            total,
            "oracle render must reproduce total_height"
        );
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
        assert_eq!(
            vw2.lines.len(),
            content_height,
            "mid-scroll window must also have viewport length"
        );
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
            src, "{\n  \"command\": \"ls -la\",\n  \"timeout\": 30\n}",
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
        assert_eq!(
            ChatMessage::Error("boom\ntail".into()).source_text(),
            "boom\ntail"
        );
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
        let mut store = TranscriptStore::new(super::super::clock::TuiClock::real());
        store.push_msg(ChatMessage::User("u".into()));
        store.push_msg(ChatMessage::Text("t".into()));
        assert_eq!(store.source_text(0), "u");
        assert_eq!(store.source_text(1), "t");
    }
}

// ── Slice 0: Baseline pathology tests (T241 §5 / §8) ─────────────────────────
//
// These tests document the CURRENT eager behaviour as a measured baseline.
// They are `#[ignore]` so CI doesn't pay the syntect init cost on every run;
// invoke them explicitly:
//
//   cargo test -p synaps-tui --test '*' -- --ignored baseline_slice0
//   cargo test -p synaps-tui -- --ignored baseline_slice0
//
// INTENTIONAL REWRITE TARGET: Slice 4 of T241 (lazy-estimator landing) will
// flip the assertions in these tests.  The test names and the comment
// "BASELINE — rewrite in Slice 4" serve as grep-able markers so the
// implementer knows exactly which assertions invert.
//
// Counter semantics (recap):
//   probe_render_count()  — render_message_lines calls since last probe_reset()
//   highlight_call_count() — syntect highlight_line sessions since last reset
//   syntax_set_was_touched() — whether SYNTAX_SET LazyLock init fired since reset
#[cfg(test)]
mod slice0_baseline_pathology {
    use super::super::highlight;
    use super::*;
    use ratatui::layout::Rect;

    const VIEWPORT_ROWS: usize = 40;
    const VIEWPORT_COLS: usize = 120;
    // Outer rect adds 2 border rows/cols each side.
    fn msg_area(rows: usize, cols: usize) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: (cols + 2) as u16,
            height: (rows + 2) as u16,
        }
    }

    fn test_ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "synaps",
        }
    }

    /// Build 1,000 deterministic synthetic messages:
    ///   - 800 plain-text assistant messages (off-screen in a 40-row viewport)
    ///   - 200 messages containing a fenced Rust code block (off-screen)
    ///
    /// No real session data; purely synthetic and public-safe.
    fn make_synthetic_store(n_plain: usize, n_code: usize) -> TranscriptStore {
        let mut store = TranscriptStore::new(super::super::clock::TuiClock::real());
        for i in 0..n_plain {
            store.push_msg(ChatMessage::Text(format!(
                "Synthetic assistant message {i}.\n\
                 This is a second line of text to give the message some height.\n\
                 Third line: the quick brown fox jumps over the lazy dog."
            )));
        }
        for i in 0..n_code {
            store.push_msg(ChatMessage::Text(format!(
                "Message with off-screen fenced code block {i}.\n\
                 ```rust\n\
                 fn synthetic_{i}() -> usize {{\n\
                     let x = {i};\n\
                     x * x\n\
                 }}\n\
                 ```\n\
                 After the code block."
            )));
        }
        // Push a few visible messages at the tail so the viewport has something
        // to show (these are the ones that will be on-screen).
        for i in 0..5 {
            store.push_msg(ChatMessage::User(format!("User tail message {i}")));
        }
        store
    }

    // ─────────────────────────────────────────────────────────────────────────
    // BASELINE — rewrite in Slice 4
    //
    // T241 §5 T1 (inverted): current eager path renders ALL N messages on the
    // first Missing→sync_cache.  After Slice 3 the ratchet flips to ≤ 72 for
    // a 40-row viewport.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    #[ignore = "slow: loads syntect defaults; run explicitly with --ignored"]
    fn baseline_slice0_cold_missing_renders_all_messages() {
        // BASELINE — rewrite in Slice 4:
        //   after lazy estimation lands, assert renders <= VIEWPORT_ROWS + 2*HALO (≤72)
        const N_PLAIN: usize = 800;
        const N_CODE: usize = 200;
        const TOTAL: usize = N_PLAIN + N_CODE + 5; // +5 tail messages

        let mut store = make_synthetic_store(N_PLAIN, N_CODE);

        // Reset counters right before the cold frame.
        store.probe_reset();
        highlight::highlight_reset_counters();

        let area = msg_area(VIEWPORT_ROWS, VIEWPORT_COLS);
        let _vw = store.visible_window(area, &test_ctx());

        let renders = store.probe_render_count();
        let hl_calls = highlight::highlight_call_count();
        let ss_touched = highlight::syntax_set_was_touched();

        eprintln!(
            "[baseline_slice0_cold_missing_renders_all_messages]\n  \
             renders={renders}  (expected today: == {TOTAL}; target after Slice3: ≤72)\n  \
             highlight_calls={hl_calls}\n  \
             syntax_set_touched={ss_touched}"
        );

        // BASELINE assertion: today every message is rendered on the first frame.
        // This will be INVERTED in Slice 4.
        assert_eq!(
            renders, TOTAL,
            "BASELINE: cold Missing sync renders all {TOTAL} messages. \
             Slice 4 rewrites this assertion to renders ≤ 72."
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // BASELINE — rewrite in Slice 4
    //
    // T241 §5 T3 (inverted): an off-screen fenced code block currently forces
    // SYNTAX_SET init (highlight_call_count > 0, syntax_set_was_touched == true)
    // on the first frame.  After Slice 3 the ratchet flips to == 0 / false.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    #[ignore = "slow: loads syntect defaults; run explicitly with --ignored"]
    fn baseline_slice0_offscreen_code_fence_triggers_syntect_init() {
        // BASELINE — rewrite in Slice 4:
        //   after lazy estimation, assert highlight_calls == 0 && !syntax_set_was_touched
        const N_PLAIN: usize = 800;
        const N_CODE: usize = 200; // off-screen code fences

        let mut store = make_synthetic_store(N_PLAIN, N_CODE);

        store.probe_reset();
        highlight::highlight_reset_counters();

        let area = msg_area(VIEWPORT_ROWS, VIEWPORT_COLS);
        let _vw = store.visible_window(area, &test_ctx());

        let hl_calls = highlight::highlight_call_count();
        let ss_touched = highlight::syntax_set_was_touched();

        eprintln!(
            "[baseline_slice0_offscreen_code_fence_triggers_syntect_init]\n  \
             highlight_calls={hl_calls}  (expected today: >0; target after Slice3: ==0)\n  \
             syntax_set_touched={ss_touched}  \
             (note: may be false if SYNTAX_SET pre-warmed by another test in this process)"
        );

        // BASELINE assertion: today off-screen fences trigger syntect on first frame.
        // This is INVERTED in Slice 4 (hl_calls == 0).
        assert!(
            hl_calls > 0,
            "BASELINE: off-screen code fences trigger syntect (got {hl_calls} calls). \
             Slice 4 rewrites this to == 0."
        );
        // Note: SYNTAX_SET_TOUCHED is a process-global latch that fires at
        // most once per process lifetime. It is only reliable in isolated runs
        // (the integration test tests/mem_transcript.rs).  In shared-process
        // runs another test may pre-warm SYNTAX_SET, leaving ss_touched=false
        // here even though hl_calls > 0 (syntect IS executing, just not
        // initializing again).  We assert hl_calls only; the integration test
        // asserts ss_touched independently.
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Non-pathological steady-state: warm cache, no renders, counter deltas zero.
    // This test does NOT get rewritten in Slice 4 — it should pass before and
    // after the lazy refactor.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    #[ignore = "slow: loads syntect defaults; run explicitly with --ignored"]
    fn baseline_slice0_warm_cache_second_frame_zero_renders() {
        let mut store = make_synthetic_store(100, 20);

        let area = msg_area(VIEWPORT_ROWS, VIEWPORT_COLS);
        let ctx = test_ctx();

        // First (cold) frame — burns the cache.
        let _vw = store.visible_window(area, &ctx);

        // Reset after cold frame, measure the second.
        store.probe_reset();
        highlight::highlight_reset_counters();

        let _vw2 = store.visible_window(area, &ctx);

        let renders = store.probe_render_count();
        let hl_calls = highlight::highlight_call_count();

        eprintln!(
            "[baseline_slice0_warm_cache_second_frame_zero_renders]\n  \
             renders={renders}  (expected: 0)\n  \
             highlight_calls={hl_calls}  (expected: 0)"
        );

        assert_eq!(
            renders, 0,
            "second frame on a clean cache must trigger zero renders"
        );
        assert_eq!(
            hl_calls, 0,
            "second frame on a clean cache must trigger zero highlight calls"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// T241 Slice 2 — ScrollAnchor unit tests
//
// Mathematical theorem tests (§4.3 correction theorem):
//   TA-1  correction below anchor: S_top unchanged
//   TA-2  correction above anchor: coordinate shifts, anchor msg at same screen row
//   TA-3  anchor-message shrink: row_in_msg clamps; motion bounded to msg
//   TA-4  empty / stale index: no panic, valid result
//   TA-5  pinned bottom: anchor is None; recomputes from total
//   TA-6  round-trip capture → restore is idempotent with exact cache
//   TA-7  clamp_scroll_back covers empty transcript and zero viewport
//   TA-8  scroll mutations keep anchor in sync
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod scroll_anchor_tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Build a `LineCache` with `n` slots of height `h` each.
    fn uniform_cache(n: usize, h: usize) -> LineCache {
        let per_msg: Vec<MsgSlot> = (0..n)
            .map(|_| MsgSlot {
                lines: None,
                meta: None,
                height: HeightState::Exact(h),
            })
            .collect();
        LineCache::new(80, per_msg)
    }

    /// Build a `LineCache` with per-message heights from a slice.
    fn cache_from_heights(heights: &[usize]) -> LineCache {
        let per_msg: Vec<MsgSlot> = heights
            .iter()
            .map(|&h| MsgSlot {
                lines: None,
                meta: None,
                height: HeightState::Exact(h),
            })
            .collect();
        LineCache::new(80, per_msg)
    }

    fn test_ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: false,
            agent_name: "synaps",
        }
    }

    // ── TA-1: correction BELOW anchor leaves S_top unchanged ─────────────────
    //
    // Layout: 10 messages × 5 rows each = total 50. Viewport = 10.
    // Anchor at msg 3, row_in_msg 2  →  S_top = 3*5 + 2 = 17.
    // scroll_back = 50 - 10 - 17 = 23.
    //
    // Apply correction +10 to message 7 (below anchor 3).
    // New total = 60. Anchor cum[3] = 15 (unchanged), so S_top = 17 still.
    // New scroll_back = 60 - 10 - 17 = 33.
    //
    // Test: restore from anchor with new cache → scroll_back == 33.
    #[test]
    fn ta1_correction_below_anchor_s_top_unchanged() {
        let cache_before = uniform_cache(10, 5); // total=50
        let content_height = 10usize;
        let scroll_back_before = 23usize; // S_top = 50-10-23 = 17
        let s_top_before = TranscriptStore::scroll_back_to_stop(
            scroll_back_before,
            cache_before.total_height(),
            content_height,
        );
        assert_eq!(s_top_before, 17);

        let anchor = TranscriptStore::capture_anchor(
            &cache_before,
            scroll_back_before,
            content_height,
        )
        .expect("capture must succeed");
        assert_eq!(anchor.msg_idx, 3);   // cum[3]=15, 17-15=2
        assert_eq!(anchor.row_in_msg, 2);

        // Apply +10 to message 7 (index 7 > anchor.msg_idx=3 → below anchor).
        let mut heights: Vec<usize> = vec![5; 10];
        heights[7] = 15; // was 5, now 15: delta +10
        let cache_after = cache_from_heights(&heights); // total=60

        let sb_after = TranscriptStore::anchor_to_scroll_back(
            &anchor,
            &cache_after,
            content_height,
        );

        // S_top must still be 17: scroll_back = 60 - 10 - 17 = 33
        let s_top_after = TranscriptStore::scroll_back_to_stop(
            sb_after,
            cache_after.total_height(),
            content_height,
        );
        assert_eq!(
            s_top_after, s_top_before,
            "correction below anchor must not change S_top (was {s_top_before}, got {s_top_after})"
        );
        assert_eq!(sb_after, 33);
    }

    // ── TA-2: correction ABOVE anchor shifts coordinate, anchor row on screen unchanged ──
    //
    // Same layout. Apply +10 to message 1 (above anchor 3).
    // New cum[3] = 5+5+15+5 = 30 (was 15). New S_top = 30 + 2 = 32.
    // New total = 60. New scroll_back = 60 - 10 - 32 = 18.
    //
    // Key: anchor.row_in_msg == 2 is unchanged; the anchor MESSAGE is still
    // at the same screen row (the screen row of msg 3's row 2 hasn't moved).
    // The COORDINATE shifted, but the visual row is the same.
    #[test]
    fn ta2_correction_above_anchor_shifts_coordinate_anchor_screen_row_fixed() {
        let cache_before = uniform_cache(10, 5);
        let content_height = 10usize;
        let scroll_back_before = 23usize;

        let anchor = TranscriptStore::capture_anchor(
            &cache_before,
            scroll_back_before,
            content_height,
        )
        .expect("capture must succeed");

        // Apply +10 to message 1 (index 1 < anchor.msg_idx=3 → above anchor).
        let mut heights: Vec<usize> = vec![5; 10];
        heights[1] = 15; // delta +10
        let cache_after = cache_from_heights(&heights); // total=60

        let sb_after = TranscriptStore::anchor_to_scroll_back(
            &anchor,
            &cache_after,
            content_height,
        );

        // anchor msg still at same distance from top of viewport.
        // new cum[3] = 5+15+5+5 = 25 (accumulated: cum[0]=0,cum[1]=5,cum[2]=20,cum[3]=25).
        // S_top = cum[3] + row_in_msg = 25 + 2 = 27.
        // scroll_back = 60 - 10 - 27 = 23.
        let s_top_after = TranscriptStore::scroll_back_to_stop(
            sb_after,
            cache_after.total_height(),
            content_height,
        );
        assert_eq!(
            s_top_after, 27,
            "correction above anchor: S_top must shift by delta (expected 27 = cum[3]+row_in_msg, got {s_top_after})"
        );
        // The anchor message's SCREEN ROW is 0 (it's at the top): that hasn't changed.
        // What changed is the content coordinate S_top, not the screen position.
        // We verify: (S_top - cum[anchor.msg_idx]) == row_in_msg.
        let cum_anchor = cache_after.cum_heights[anchor.msg_idx];
        assert_eq!(
            s_top_after.saturating_sub(cum_anchor),
            anchor.row_in_msg,
            "anchor msg row_in_msg must still point to the top-of-viewport row"
        );
    }

    // ── TA-3: anchor-message shrinks → row_in_msg clamps, motion bounded ─────
    //
    // Anchor at msg 3, row_in_msg 2 (height was 5). Shrink msg 3 to height 2.
    // row_in_msg must clamp to 1 (h-1). S_top = cum[3] + 1 = 15+1 = 16.
    // scroll_back = 50 - 10 - 16 = 24.
    //
    // Motion: S_top changed from 17 to 16 — bounded to within the anchor message.
    #[test]
    fn ta3_anchor_message_shrink_clamps_row_in_msg() {
        let cache_before = uniform_cache(10, 5);
        let content_height = 10usize;
        let scroll_back_before = 23usize; // S_top = 17

        let anchor = TranscriptStore::capture_anchor(
            &cache_before,
            scroll_back_before,
            content_height,
        )
        .expect("capture must succeed");
        assert_eq!(anchor.msg_idx, 3);
        assert_eq!(anchor.row_in_msg, 2);

        // Shrink message 3 from 5 to 2 rows.
        let mut heights: Vec<usize> = vec![5; 10];
        heights[3] = 2; // shrink: total becomes 47
        let cache_after = cache_from_heights(&heights);

        let sb_after = TranscriptStore::anchor_to_scroll_back(
            &anchor,
            &cache_after,
            content_height,
        );
        let s_top_after = TranscriptStore::scroll_back_to_stop(
            sb_after,
            cache_after.total_height(),
            content_height,
        );
        // row_in_msg clamped to min(2, 2-1) = 1. S_top = 15+1 = 16.
        assert_eq!(s_top_after, 16, "shrunk anchor msg: S_top must clamp to 16 (got {s_top_after})");
        // Motion is exactly 1 row — bounded to within the anchor message.
        assert!(
            (17i64 - s_top_after as i64).abs() <= 5,
            "shrink motion must be bounded to one anchor-message height"
        );
    }

    // ── TA-4: empty transcript + stale msg_idx → no panic, valid result ───────
    #[test]
    fn ta4_empty_and_stale_no_panic() {
        // Empty cache.
        let empty = cache_from_heights(&[]);
        let anchor_result = TranscriptStore::capture_anchor(&empty, 0, 40);
        assert!(anchor_result.is_none(), "empty cache capture must return None");

        // Stale anchor (msg_idx past the end after drain).
        let small = cache_from_heights(&[5, 5]); // only 2 msgs
        let stale = ScrollAnchor { msg_idx: 99, row_in_msg: 3 };
        // Must not panic; clamps to last msg.
        let _ = TranscriptStore::anchor_to_scroll_back(&stale, &small, 10);

        // Zero viewport.
        let cache = uniform_cache(5, 5);
        let anchor = ScrollAnchor { msg_idx: 2, row_in_msg: 0 };
        let sb = TranscriptStore::anchor_to_scroll_back(&anchor, &cache, 0);
        assert_eq!(sb, 0, "zero viewport → scroll_back 0");

        // Zero-height slot in cache.
        let cache_zero = cache_from_heights(&[0, 5, 5]);
        let _ = TranscriptStore::capture_anchor(&cache_zero, 5, 5); // must not panic
    }

    // ── TA-5: pinned bottom — anchor is None, recomputes from total ───────────
    #[test]
    fn ta5_pinned_bottom_anchor_none_recomputes() {
        let cache = uniform_cache(10, 5); // total=50
        let content_height = 10usize;
        // scroll_back == 0 → capture returns None (pinned mode, not a coordinate).
        let anchor = TranscriptStore::capture_anchor(&cache, 0, content_height);
        assert!(
            anchor.is_none(),
            "scroll_back==0 must return None (pinned mode, not a coordinate)"
        );
        // scroll_back > 0 (actually scrolled up) returns Some even if s_top==0.
        // total=50, content=10, scroll_back=40 → s_top=0.
        let anchor_sb40 = TranscriptStore::capture_anchor(&cache, 40, content_height);
        assert!(
            anchor_sb40.is_some(),
            "scroll_back>0 with s_top==0 must still return Some (anchored, not pinned)"
        );
        // scroll_back > max_back (over-scrolled) still returns Some.
        let anchor_over = TranscriptStore::capture_anchor(&cache, 99, content_height);
        assert!(
            anchor_over.is_some(),
            "over-scrolled (sb>max_back) must still return Some"
        );
    }

    // ── TA-6: round-trip capture → restore is idempotent with exact cache ─────
    #[test]
    fn ta6_round_trip_capture_restore_idempotent() {
        let cache = uniform_cache(20, 5); // total=100
        let content_height = 10usize;

        // Test several scroll positions.
        for sb in [10usize, 25, 50, 70, 85] {
            let anchor = TranscriptStore::capture_anchor(
                &cache,
                sb,
                content_height,
            );
            match anchor {
                None => {
                    // Only valid when sb == 0.
                    assert_eq!(sb, 0, "capture returns None only for sb==0");
                }
                Some(ref a) => {
                    let sb_restored = TranscriptStore::anchor_to_scroll_back(
                        a,
                        &cache,
                        content_height,
                    );
                    // The restored scroll_back must equal the original when
                    // the cache is unchanged (no height corrections).
                    assert_eq!(
                        sb_restored, sb,
                        "round-trip must be idempotent: sb={sb} → anchor={a:?} → {sb_restored}"
                    );
                }
            }
        }
    }

    // ── TA-7: clamp_scroll_back covers edge cases ─────────────────────────────
    #[test]
    fn ta7_clamp_scroll_back_edge_cases() {
        // Empty transcript: max_back = 0.
        assert_eq!(TranscriptStore::clamp_scroll_back(99, 0, 10), 0);
        // total < viewport: no scrollable space.
        assert_eq!(TranscriptStore::clamp_scroll_back(50, 5, 10), 0);
        // scroll_back within range: unchanged.
        assert_eq!(TranscriptStore::clamp_scroll_back(10, 50, 10), 10);
        // scroll_back exceeds max: clamped.
        assert_eq!(TranscriptStore::clamp_scroll_back(100, 50, 10), 40);
        // Zero viewport: total - 0 - scroll_back.
        assert_eq!(TranscriptStore::clamp_scroll_back(30, 50, 0), 30);
    }

    // ── TA-8: scroll mutations keep anchor in sync ────────────────────────────
    //
    // Build a store with a live cache, perform scroll mutations via the
    // public scroll_up/scroll_down/scroll_to_bottom API, and assert that
    // the shadow anchor is consistent with scroll_back after each.
    #[test]
    fn ta8_scroll_mutations_keep_anchor_in_sync() {
        let mut store = TranscriptStore::new(super::super::clock::TuiClock::real());
        // Push 30 text messages — each renders to a few rows.
        for i in 0..30 {
            store.push_msg(ChatMessage::Text(format!("Message {i}: some content here.")));
        }

        // Warm the cache via visible_window so heights are available.
        let msg_area = ratatui::layout::Rect {
            x: 0, y: 0, width: 82, height: 12, // content: 80 wide, 10 high
        };
        let _ = store.visible_window(msg_area, &test_ctx());

        // Initially pinned → anchor is None.
        assert!(
            store.scroll_anchor().is_none(),
            "initially pinned: scroll_anchor must be None"
        );

        // Scroll up → anchor must be Some.
        store.scroll_up(5);
        assert!(
            store.scroll_anchor().is_some(),
            "after scroll_up: anchor must be Some"
        );

        // The anchor's msg_idx must be within range.
        let n_msgs = store.message_count();
        if let Some(a) = store.scroll_anchor() {
            assert!(a.msg_idx < n_msgs, "anchor.msg_idx must be in range");
        }

        // Scroll down to 0 → re-pin → anchor is None.
        store.scroll_down(9999); // saturates to 0
        assert!(
            store.scroll_anchor().is_none(),
            "after scroll_down to 0: anchor must be None"
        );
        assert!(store.is_pinned(), "scroll_back 0 → must be pinned");

        // scroll_to_bottom → anchor is None.
        store.scroll_up(3);
        assert!(store.scroll_anchor().is_some(), "after scroll_up: anchor Some");
        store.scroll_to_bottom();
        assert!(
            store.scroll_anchor().is_none(),
            "scroll_to_bottom must clear anchor"
        );
    }
}
