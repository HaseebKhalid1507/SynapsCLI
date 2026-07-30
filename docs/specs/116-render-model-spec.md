# Issue #116 — Dedicated Render Thread: `RenderModel` Design Spec

**Branch:** `refactor/a3-crate-split`
**Crate:** `crates/agent-tui`
**Status:** Spec only. No source modified. Implementation contract for the follow-up PR.

> **The bug in one sentence.** `tui::draw::draw()` runs on the main tokio task and ends in `CrosstermBackend::flush()` doing a blocking `write(2)` to stdout; if the PTY's read side stalls, the entire async runtime is wedged, and only the `signals.rs` watchdog (`std::process::exit(1)` after ~10s — `crates/agent-tui/src/tui/signals.rs:62`) gets us out.

> **The fix in one sentence.** Move the `Terminal` onto a dedicated `std::thread`; the main task builds an owned, send-able `RenderModel` snapshot per frame and ships it over a channel; the render thread owns the `Terminal` and does all writes.

> **The whole risk.** `draw()` today receives `&mut App` and reads ~36 distinct field/method accessors mid-render. You cannot send `&mut App` across a thread. The snapshot must be *complete* (nothing read live), *owned* (no borrows back into App), and *cheap enough* at 60fps (the throttle in `mod.rs:164`).

---

## 1. Complete `draw()` input inventory

### 1.1 Signature (current — `draw.rs:392-401`)

```rust
pub(crate) fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    effect: &mut Option<Effect>,
    exit_effect: &mut Option<Effect>,
    elapsed: std::time::Duration,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    secret_prompts: &synaps_cli::tools::SecretPromptQueue,
) -> io::Result<()>
```

### 1.2 What each parameter contributes

| Parameter | Used for | Owner after refactor |
|---|---|---|
| `terminal` | `terminal.size()?` (line 409), `terminal.draw(\|frame\| {...})?` (431,1146), and the implicit `flush()` inside `draw()`. Also: `viewport::scrub_crossterm_terminal_edges(terminal, …)` (425-429) emits raw `MoveTo`/`Print` directly to the backend. | **Render thread only.** Both `terminal.draw` and `scrub_crossterm_terminal_edges` must move there — they own physical I/O. |
| `app: &mut App` | The god-struct. Read mostly; also **mutated** in five places (1.4 below). | Main side only. Snapshot extracted; never crosses the channel. |
| `runtime: &Runtime` | `runtime.model()` → `&str` (402), `runtime.thinking_level()` → some Display type (403). Also passed verbatim into `settings::RuntimeSnapshot::from_runtime_with_health` (1134) and `models::render` (1138) for modal overlays. | Snapshot the *cheap projections* (`model: String`, `thinking: String`). For the settings modal: `RuntimeSnapshot` is already a snapshot type — build it on the main side and put it in `RenderModel`. |
| `effect`, `exit_effect` | `fx.process(elapsed.into(), frame.buffer_mut(), area)` (1091, 1098). **Stateful**: `process` mutates internal animation state, and `fx.done()` (1092) drives `*effect = None`. See §4. | Render thread owns them. See §4. |
| `elapsed: Duration` | Passed straight into `fx.process(elapsed.into(), …)`. | Render thread computes its own `elapsed` from its local clock (see §4). |
| `registry: &Arc<CommandRegistry>` | `sidecar_pill_spans(app, registry)` → `registry.lifecycle_claims()` (94) for sidecar ordering; `commands::all_commands_with_skills(registry)` (900) for slash-command ghost hints; passed into the settings modal builder (1134). | `Arc<CommandRegistry>` is already `Send + Sync`. **Clone the `Arc` into the `RenderModel`** — zero cost. Or pre-compute the only two things actually read (lifecycle claims, all commands list) on the main side. Recommend the latter: keeps the render thread free of registry semantics. |
| `secret_prompts: &SecretPromptQueue` | `secret_prompts.is_active()` (573) and `secret_prompts.active()` → `&SecretPrompt` (1101) for the modal body. | Snapshot the active prompt into `Option<SecretPromptSnap>` (title, prompt, masked-buffer length) on the main side. |

### 1.3 EXHAUSTIVE `app.*` accessor table

Each row: every distinct `app.<field>` / `app.<method>(…)` access in `draw.rs`, its **type** from `app.rs`, the read sites, and how it's used. (Method-call sites also include callees inside `draw.rs` helpers — notably `sidecar_pill_spans` at lines 87–106 and the closure body 431–1146.)

| # | Accessor | Type (from `app.rs`) | Sites in `draw.rs` | Use |
|---|---|---|---|---|
| 1 | `app.gamba_child` | `Option<std::process::Child>` (app.rs:121) | 405 | **Early return** if `Some` — casino owns the terminal. |
| 2 | `app.subagents` | `Vec<SubagentState>` (app.rs:101; struct app.rs:185-192: `id:u64, name:String, status:String, start_time:Instant, done:bool, duration_secs:Option<f64>`) | 410, 412, 434, 435, 470, 471, 472, 777, 834, 835 | Panel height, header status span, full subagent panel render (each row clones name/status). |
| 3 | `app.input_wrap_info(w)` → method | `fn(&App, u16) -> (u16, u16, u16)` = (total_lines, cursor_row, cursor_col); pure read, see app.rs:343-384 | 417, 443, 942 | Called **three times per frame** with two different widths. Drives `input_height`, scroll offset, cursor position. |
| 4 | `app.active_tasks` | `synaps_cli::extensions::active_tasks::ActiveTasks` (app.rs:167) | 420, 449, 977, 978 | `is_empty()` for layout; passed to `render_active_tasks_line(&app.active_tasks, w)`. |
| 5 | `app.status_text` | `Option<String>` (app.rs:119) | 464 | Header status span text. |
| 6 | `app.spinner_frame` | `usize` (app.rs:117) | 103 (via `sidecar_pill_spans`), 465, 473, 480, 774 | Spinner index + sinusoidal pulse phase. |
| 7 | `app.streaming` | `bool` (app.rs:54) | 479, 858 | Header status + input border color. |
| 8 | `app.line_cache` | `Option<(usize, Vec<Line<'static>>)>` (app.rs:93) | 522-528 (**read + assign**), 530 | **Width-keyed memo**. `draw` rebuilds it if width changed via `app.render_lines(w)` (#9), then borrows `&app.line_cache.as_ref().unwrap().1`. **Mutates App.** |
| 9 | `app.render_lines(w)` → method | `fn(&App, usize) -> Vec<Line<'static>>` (render.rs:14). Pure, reads `self.messages` + theme. Hundreds of lines of allocation. | 527 | The single most expensive operation in `draw()`. Result is what gets shown in the message pane. |
| 10 | `app.scroll_pinned` | `bool` (app.rs:52) | 535 | Auto-scroll mode. |
| 11 | `app.scroll_back` | `u16` (app.rs:49) | 536 (**assign**), 543 (**assign**), 547, 548 (**assign**), 553, 756, 757 | Viewport offset. **Mutated** during the draw (clamp / growth-compensation arithmetic). |
| 12 | `app.last_line_count` | `usize` (app.rs:99) | 540, 551 (**assign**) | Growth detector for unpinned scroll stabilization. **Mutated.** |
| 13 | `app.msg_area_rect` | `Option<ratatui::layout::Rect>` (app.rs:152) | 582 (**assign**) | Saved post-draw for mouse mapping (consumed by `input.rs` on next event). Pure write-out. |
| 14 | `app.visible_line_range` | `Option<(usize, usize)>` (app.rs:155) | 583 (**assign**) | Same — saved write-out for input.rs. |
| 15 | `app.selection_range()` → method | `fn(&App) -> Option<(u16,u16,u16,u16)>` (app.rs:815). Pure read of `selection_anchor`/`selection_end` (both `Option<(u16,u16)>`, app.rs:148-149), normalized. | 586 | Cell-by-cell selection inversion overlay. |
| 16 | `app.messages` | `Vec<TimestampedMsg>` (app.rs:44) | 617 | `is_empty()` only — controls logo visibility. |
| 17 | `app.logo_dismiss_t` | `Option<f64>` (app.rs:96) | 617, 642 | CRT dismiss animation phase. |
| 18 | `app.logo_build_t` | `Option<f64>` (app.rs:97) | 668 | Build-in animation phase. |
| 19 | `app.input` | `String` (app.rs:45) | 876, 898, 899 | Iterated char-by-char to render the input row; also `starts_with('/')` and slicing for ghost hints. |
| 20 | `app.show_full_output` | `bool` (app.rs:95) | 1007 | Footer keybind label. |
| 21 | `app.session_cost` | `f64` (app.rs:86) | 1017, 1018 | Footer cost segment. |
| 22 | `app.total_input_tokens` | `u64` (app.rs:65) | 1025, 1035, 1038 | Footer token line. |
| 23 | `app.total_output_tokens` | `u64` (app.rs:66) | 1035, 1039 | Footer token line. |
| 24 | `app.total_cache_read_tokens` | `u64` (app.rs:67) | 1025, 1026, 1027 | Cache-hit %. |
| 25 | `app.total_cache_creation_tokens` | `u64` (app.rs:68) | 1025 | Denominator of cache %. |
| 26 | `app.total_cache_write_1h` | `u64` (app.rs:73) | 1029 | "·1h" suffix on cache %. |
| 27 | `app.last_turn_context` | `u64` (app.rs:79) | 1056 | Context-usage bar numerator. |
| 28 | `app.last_turn_context_window` | `u64` (app.rs:84) | 1057 | Bar denominator. |
| 29 | `app.toasts` | `super::toast::ToastProvider` (app.rs:169) | 1131 | Passed by `&` to `render_toasts`, which calls `provider.visible()`. |
| 30 | `app.settings` | `Option<SettingsState>` (app.rs:123) | 1133 | Modal — passed by `&` to `settings::render(frame, area, state, &snap)`. |
| 31 | `app.model_health` | `HashMap<String, (PingStatus, u64)>` (app.rs:135) | 1134 | `.clone()` into the settings `RuntimeSnapshot`. |
| 32 | `app.models` | `Option<ModelsModalState>` (app.rs:127) | 1137 | Modal — passed by `&`. |
| 33 | `app.plugins` | `Option<PluginsModalState>` (app.rs:125) | 1140 | Modal — passed by `&`. |
| 34 | `app.help_find` | `Option<HelpFindState>` (app.rs:129) | 1143 | Modal — passed by **`&mut`** to `help_find::render` (help_find.rs:58 — `state: &mut HelpFindState`). **Mutated by render.** |
| 35 | `app.sidecars` | `HashMap<String, SidecarUiState>` (app.rs:165) | 91, 95, 101, 103 (inside `sidecar_pill_spans`) | Header pill spans. Each `SidecarUiState` owns a child process — **NOT trivially Send** (sticky owner side). |
| 36 | (helper) `sidecar_pill_spans(app, registry)` | free fn — reads (35) + (6) | 500 | Composes `Vec<Span<'static>>` for the header. |

**Mutation tally** (App fields modified by `draw()` itself, not just read):

- `app.line_cache` (528) — memoization rebuild on width change
- `app.scroll_back` (536, 543, 548) — pinned reset + growth compensation
- `app.last_line_count` (551) — write-out for the next frame's growth diff
- `app.msg_area_rect` (582) — write-out for mouse mapping
- `app.visible_line_range` (583) — write-out for selection extraction
- `app.help_find` — mutated **inside** the modal render

These five places of "drawing mutates state" are the chief reason `draw()` currently takes `&mut App` and the chief obstacle to a pure snapshot. They are addressed in §6.

### 1.4 Mutation reachability summary

> "Drawing has side effects" is the wart. Five of the six mutations are bookkeeping that can be lifted into `build_render_model` on the main side (it has `&mut App`); only the `help_find` mutation propagates into a callee on the render thread. See §3.6 and §6.

---

## 2. Classification of each access

Legend:
**(a)** Plain data — snapshot by Clone/Copy.
**(b)** Method call — precompute on main side OR keep pure helper fn.
**(c)** Borrowed collection — Clone or `Arc`-share into snapshot.
**(d)** Behavioral / stateful — needs an explicit decision.

| # | Accessor | Class | Decision |
|---|---|---|---|
| 1 | `gamba_child` | (d) | **Decision: don't ship the frame at all.** `is_some()` early-return is a *main-side* gate before `send`. Render thread never knows about it. |
| 2 | `subagents: Vec<SubagentState>` | (c) | Clone the vec. `SubagentState: Clone` already (app.rs:184). Typically ≤6 elements (cap at line 412). Cost: ~6 × (clone 2 Strings + 6 scalars) per frame = negligible. |
| 3 | `input_wrap_info(w)` | (b) | **Precompute on main side, store result.** It's called 3× per frame with two widths (the outer-rect width and the post-layout `frame.area()` width). In practice these match. But `frame.area()` is only known after `terminal.draw(\|frame\|…)`, so we cannot trivially compute "inside the closure" width on main. **Workaround**: pre-compute against `terminal.size()` on main (same value), or move `input_wrap_info` to a `pub` free fn taking `&str` + `usize` and call it on the render thread (it's pure, doesn't need App). **Recommended**: move to a pure free fn `input::wrap_info(input: &str, cursor_pos: usize, inner_width: u16) -> (u16,u16,u16)` and snapshot `(input: String, cursor_pos: usize)` into the model; render thread calls the fn against the actual frame width. This eliminates the dual-width hazard and matches the input rendering loop which already iterates `app.input.chars()` inside `terminal.draw`. |
| 4 | `active_tasks: ActiveTasks` | (c) | `ActiveTasks::clone()` is shallow (it's a `Vec<Task>` internally — confirm). For ≤a few tasks per frame, cheap. Alternative: `Arc<ActiveTasks>` swapped on update. **Recommended: `Arc<ActiveTasks>` in App** (out of scope for this PR) and `Arc::clone()` into the snapshot. Until then, `clone()` is fine. |
| 5 | `status_text: Option<String>` | (a) | Clone. ≤a few bytes. |
| 6 | `spinner_frame: usize` | (a) | Copy. |
| 7 | `streaming: bool` | (a) | Copy. |
| 8 | `line_cache: Option<(usize, Vec<Line<'static>>)>` | (c) + (d) — *the big one* | Memoization state that today is "rebuild if width changed". Two paths: **(i)** Keep `line_cache` on App; on main side, after determining the frame width, rebuild if stale, then `Arc<Vec<Line<'static>>>` clone-share into the snapshot. **(ii)** Move the cache to the render thread keyed by `(messages_revision, width)`. **Recommended: (i)**. The main side already owns "what changed" (messages mutations), the render thread should not have to invalidate. Wrap the cached vec in `Arc<[Line<'static>]>` so snapshot.clone() is a refcount bump. |
| 9 | `render_lines(w)` → method | (b) | Pure (takes `&self`). Either: precompute on main, or move it. Recommended: **precompute on main** (it's the same function that populates `line_cache`) — that way the snapshot carries an `Arc<[Line<'static>]>` and the render thread does zero markdown/highlight work. |
| 10 | `scroll_pinned: bool` | (a) | Copy. **But note**: `scroll_back` is mutated *based on it* during draw — the mutation must happen main-side after we know `total_lines` and `content_height`. See §6. |
| 11 | `scroll_back: u16` | (a) + (d) | Final value goes into snapshot. Compute it on the main side (see §6). |
| 12 | `last_line_count: usize` | (d) | Pure bookkeeping — mutate on main side, do **not** put in snapshot. |
| 13 | `msg_area_rect: Option<Rect>` | (d) | **Inverted dataflow.** Today: draw computes the inner rect from `msg_block.inner(msg_area)`, then writes back to App so the next mouse event can map coords. After the split, the render thread is the only place that knows the actual layout. **Options**: (a) the main side computes the inner rect deterministically (it can — the layout math is mechanical and doesn't depend on `frame`), or (b) the render thread ships the rect back over a small `watch<LayoutHint>` channel. **Recommended: (a)** — replicate the layout computation on the main side. The math at lines 451-461 + `msg_block.inner(msg_area)` is pure given known constraints; the only input that's "live" is `term_size.width/height` from `terminal.size()`. Cache the last known size on main from a resize notification (see §5). |
| 14 | `visible_line_range: Option<(usize, usize)>` | (d) | Same as #13. Computed deterministically on main: `end = total - scroll_back`, `start = end - content_height`. |
| 15 | `selection_range()` → method | (b) | Pure, reads two `Option<(u16,u16)>`. Call once on main, snapshot the result `Option<(u16,u16,u16,u16)>`. |
| 16 | `messages: Vec<TimestampedMsg>` | (c) | Don't ship the vec. Ship `messages_is_empty: bool` only — that's all `draw.rs:617` reads. (The contents are baked into `line_cache` already.) |
| 17–18 | `logo_dismiss_t`, `logo_build_t: Option<f64>` | (a) | Copy. **Note**: these animate by being mutated by the **main loop's tick handler** (not by draw), so they're naturally fresh per snapshot. |
| 19 | `input: String` | (c) | Clone the string. Sizes are bounded by user typing (typically <1KB). |
| 20–28 | `show_full_output`, `session_cost`, `total_*_tokens`, `last_turn_context*` | (a) | All Copy primitives. |
| 29 | `toasts: ToastProvider` | (c) | `ToastProvider::visible()` returns an iterator of `&Toast`. Snapshot as `Vec<Toast>` (clone). Few toasts on screen at once. |
| 30 | `settings: Option<SettingsState>` | (c) | Clone if Some. Modal state is a moderate struct. Acceptable. |
| 31 | `model_health: HashMap<String,(PingStatus,u64)>` | (c) | Already `.clone()`d at site 1134. Move that clone into `build_render_model`. |
| 32 | `models: Option<ModelsModalState>` | (c) | Clone if Some. |
| 33 | `plugins: Option<PluginsModalState>` | (c) | Clone if Some. |
| 34 | `help_find: Option<HelpFindState>` | (d) — **mutating callee** | `help_find::render` takes `&mut state`. Either: (a) refactor it to `&state` (preferred — the mutations are likely UI-state bookkeeping that can move to event handling), or (b) snapshot a `Mutex<HelpFindState>`/`Arc<Mutex<HelpFindState>>` so the render thread can mutate. (a) is correct long-term; (b) is the bridge if (a) is too invasive in this PR. |
| 35 | `sidecars: HashMap<String, SidecarUiState>` | (c) + (d) | `SidecarUiState` owns a `Child` process — **not cheaply Send-cloneable**. But `draw` only reads `.display_name`, `.status`, `.armed` per entry. **Snapshot a stripped projection**: `Vec<SidecarPillSnap { plugin_id, display_name, status: SidecarUiStatus, armed }>` already ordered. Computed once on main. The original `SidecarUiState` stays on App. |
| 36 | `sidecar_pill_spans` (helper) | (b) | The output is `Vec<Span<'static>>` — ship that directly, or ship the inputs and rebuild on the render thread. The function reads `app.spinner_frame` and `registry.lifecycle_claims()`, both snapshotable. **Recommended**: keep `sidecar_pill_segment` / `sidecar_pill_spans` as pure helpers that consume the projection from #35; call them on the render thread. |

### 2.1 The "(d) call-outs" — stateful items requiring explicit decisions

1. **`gamba_child` early-return** → main-side gate (§3, §5).
2. **`boot_fx` / `exit_fx` (`Effect`)** → see §4.
3. **`line_cache` rebuild + the four write-back fields (`scroll_back`, `last_line_count`, `msg_area_rect`, `visible_line_range`)** → all hoisted to main side in `build_render_model` (§6).
4. **`help_find` `&mut`** → preferred fix: make `help_find::render` take `&state` (it's a 60-line fn; see help_find.rs:58); fallback: `Arc<Mutex<_>>`.

---

## 3. The `RenderModel` struct

Proposed location: `crates/agent-tui/src/tui/render_model.rs`.

```rust
//! Owned, Send snapshot of everything `draw()` reads.
//!
//! Built on the main task each frame in `build_render_model`, shipped over
//! a single-slot latest-wins channel to the render thread. Contains zero
//! borrows back into `App`.

use std::sync::Arc;
use ratatui::layout::Rect;
use ratatui::text::Span;
use tachyonfx::Effect;

use super::sidecar::SidecarUiStatus;
use super::settings::{SettingsState, RuntimeSnapshot};
use super::plugins::PluginsModalState;
use super::models::ModelsModalState;

pub(crate) struct RenderModel {
    // ── Header / status ──────────────────────────────────────────────────
    pub(crate) status_text:   Option<String>,
    pub(crate) streaming:     bool,
    pub(crate) spinner_frame: usize,

    // Pre-computed sidecar pills — main side already ordered & projected
    // out of the live HashMap<String, SidecarUiState>.
    pub(crate) sidecar_pills: Vec<SidecarPillSnap>,

    // Resolved per-frame: model name + thinking level (cheap String).
    pub(crate) runtime_model:    String,
    pub(crate) runtime_thinking: String,

    // ── Messages pane ────────────────────────────────────────────────────
    //
    // The pre-rendered, width-keyed line cache. Arc so snapshot.clone() is
    // a refcount bump, not a deep clone. Width is the content width the
    // cache was built at.
    pub(crate) lines:          Arc<[ratatui::text::Line<'static>]>,
    pub(crate) lines_width:    usize,
    pub(crate) scroll_back:    u16,        // final value, already clamped
    pub(crate) visible_range:  (usize, usize),   // (start, end) into `lines`
    pub(crate) selection:      Option<(u16, u16, u16, u16)>,
    pub(crate) messages_empty: bool,        // drives logo visibility

    // ── Logo / boot animation ────────────────────────────────────────────
    pub(crate) logo_build_t:   Option<f64>,
    pub(crate) logo_dismiss_t: Option<f64>,

    // ── Subagent panel ───────────────────────────────────────────────────
    pub(crate) subagents: Vec<SubagentSnap>,

    // ── Active task progress bar ─────────────────────────────────────────
    pub(crate) active_tasks: Arc<synaps_cli::extensions::active_tasks::ActiveTasks>,

    // ── Input box ────────────────────────────────────────────────────────
    pub(crate) input:        String,
    pub(crate) cursor_pos:   usize,        // char index
    // Slash-command ghost-hint data, precomputed (avoids registry on render thread).
    pub(crate) ghost_hint:   Option<GhostHint>,

    // ── Footer ───────────────────────────────────────────────────────────
    pub(crate) show_full_output:            bool,
    pub(crate) session_cost:                f64,
    pub(crate) total_input_tokens:          u64,
    pub(crate) total_output_tokens:         u64,
    pub(crate) total_cache_read_tokens:     u64,
    pub(crate) total_cache_creation_tokens: u64,
    pub(crate) total_cache_write_1h:        u64,
    pub(crate) last_turn_context:           u64,
    pub(crate) last_turn_context_window:    u64,

    // ── Toasts ───────────────────────────────────────────────────────────
    pub(crate) toasts: Vec<super::toast::Toast>,

    // ── Modals (cloned snapshots) ────────────────────────────────────────
    pub(crate) settings: Option<(SettingsState, RuntimeSnapshot)>,
    pub(crate) plugins:  Option<PluginsModalState>,
    pub(crate) models:   Option<ModelsModalState>,
    // help_find: see §3.6 — either snapshot, or shared via Arc<Mutex<_>>
    pub(crate) help_find: Option<HelpFindSlot>,

    // ── Secret prompt modal ──────────────────────────────────────────────
    pub(crate) secret_prompt: Option<SecretPromptSnap>,

    // ── Edge-scrub geometry ──────────────────────────────────────────────
    // Pre-computed protected_bottom_rows so the render thread can run
    // scrub_crossterm_terminal_edges without recomputing the layout.
    pub(crate) protected_bottom_rows: u16,

    // ── Animation frame timing ───────────────────────────────────────────
    // The render thread maintains its OWN clock for effect timing (see §4),
    // so we do not ship `elapsed`. The effects are owned by the render
    // thread; nothing about them appears in RenderModel.
}

#[derive(Clone)]
pub(crate) struct SidecarPillSnap {
    pub(crate) plugin_id:    String,
    pub(crate) display_name: Option<String>,
    pub(crate) status:       SidecarUiStatus,
    pub(crate) armed:        bool,
}

#[derive(Clone)]
pub(crate) struct SubagentSnap {
    pub(crate) name:          String,
    pub(crate) status:        String,
    pub(crate) elapsed_secs:  f64,   // duration_secs.unwrap_or_else(|| start_time.elapsed())
    pub(crate) done:          bool,
}

#[derive(Clone)]
pub(crate) struct GhostHint {
    /// Either the suffix after the user's partial (prefix match) or
    /// " → /command" form (fuzzy single match).
    pub(crate) ghost_text: String,
    /// Multi-match badge: "N matches · Tab search".
    pub(crate) match_badge: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SecretPromptSnap {
    pub(crate) title:               String,
    pub(crate) prompt:               String,
    pub(crate) masked_buffer_chars: usize,
}

pub(crate) enum HelpFindSlot {
    /// Preferred: full snapshot — assumes help_find::render is refactored to take `&state`.
    Snap(synaps_cli::help::HelpFindState),
    /// Bridge: shared mutable state — render thread locks and mutates.
    Shared(std::sync::Arc<std::sync::Mutex<synaps_cli::help::HelpFindState>>),
}
```

### 3.1 Per-frame cost estimate

At 60fps the snapshot is built ~60× per second during streaming.

| Field | Cost per frame (worst case) | Mitigation |
|---|---|---|
| `lines: Arc<[Line<'static>]>` | Refcount bump on snapshot; **the rebuild itself** (already in `app.render_lines(w)`) is the existing hot path — unchanged | Already memoized by `line_cache`. **No regression.** Switch storage from `Vec` to `Arc<[…]>` so the snapshot doesn't deep-clone. |
| `input: String` | One `String::clone` of typed input (~<1KB typical) | Acceptable. Could go `Arc<str>` if needed. |
| `subagents: Vec<SubagentSnap>` | ≤6 × 2 String clones | <1µs. |
| `sidecar_pills: Vec<SidecarPillSnap>` | ≤handful × few String clones + status clone | <1µs. |
| `toasts: Vec<Toast>` | Few × Toast clone | <1µs. |
| `active_tasks: Arc<ActiveTasks>` | Refcount bump (assuming Arc is adopted) or a small clone | <5µs. |
| Modal snapshots (`settings`, `plugins`, `models`) | Only allocated when modal open; per-frame clone | Acceptable. Could be `Arc<Mutex<_>>` if measured to hurt. |
| All scalars (status flags, counts, tokens) | Free (Copy) | n/a. |

**Net**: a snapshot is **cheaper** than the current `draw` body, because the expensive line build was already happening; we just refcount-share it.

### 3.2 What's deliberately **not** in the model

- `messages: Vec<TimestampedMsg>` — only `is_empty()` is read; flattened to `messages_empty: bool`.
- `gamba_child` — early-return on main side; the render thread never sees a frame in this state.
- `model_health` HashMap directly — collapsed into the `RuntimeSnapshot` built when `settings` is Some.
- `Arc<CommandRegistry>` — only used to pre-compute `ghost_hint` and the sidecar pill order; both projected on main.
- `Runtime` — only `model()` and `thinking_level()` strings shipped.

### 3.3 Sidecar `Child` containment

`SidecarUiState` owns a `std::process::Child` (sidecar process I/O) and is **not safe to clone across threads**. The projection `SidecarPillSnap` strips it entirely. App keeps the live `HashMap<_, SidecarUiState>` and the render thread never touches a `Child`.

### 3.4 `Send` requirements

Everything in `RenderModel` must be `Send`. Audit:

- All primitives, `String`, `Option<…>`, `Vec<…>`, `Arc<…>`, `HashMap<String,…>`: `Send`.
- `ratatui::text::Line<'static>` and `Span<'static>`: `Send` (no borrows; the `'static` is from the source: lines.rs:530's signature already returns `Vec<Line<'static>>`).
- `tachyonfx::Effect`: **not shipped** in the model — owned by the render thread.
- `SettingsState`, `PluginsModalState`, `ModelsModalState`, `Toast`, `HelpFindState`: need a one-line audit. Likely all `Send` (no inner `Rc`/`Cell`). To be confirmed by the implementing change.

### 3.5 Dirty flagging

The main loop already has `app.needs_redraw: bool` (app.rs:94, mod.rs:165). Keep it. The flow becomes:

1. Tokio task wakes (event arrived / tick / stream delta).
2. If `needs_redraw && throttle_ok`: build a `RenderModel` and `try_send` on the channel; clear `needs_redraw`.
3. Channel is **latest-wins** (§5): if the render thread is mid-write, the old pending snapshot is replaced.

### 3.6 The `help_find` mutation

Lines: `help_find::render(frame: &mut Frame, area: Rect, state: &mut HelpFindState)` (help_find.rs:58).

A quick read of help_find.rs suggests the `&mut` is used for scrollback bookkeeping — the same pattern as the messages-pane `scroll_back` write-back. **Strongly preferred**: refactor `help_find::render` to take `&HelpFindState`, lifting any scroll arithmetic into the event handler. If that refactor is out of scope, the `HelpFindSlot::Shared(Arc<Mutex<…>>)` variant is the bridge.

---

## 4. The tachyonfx Effects problem (`boot_fx`, `exit_fx`)

### 4.1 Current behavior

```rust
// draw.rs:1089-1099
if let Some(ref mut fx) = effect {
    let area = frame.area();
    fx.process(elapsed.into(), frame.buffer_mut(), area);
    if fx.done() { *effect = None; }
}
if let Some(ref mut fx) = exit_effect {
    let area = frame.area();
    fx.process(elapsed.into(), frame.buffer_mut(), area);
}
```

`Effect` is stateful: `process(duration, buffer, area)` *mutates* internal animation state (interpolators, elapsed accumulator) AND writes pixels into the buffer. `elapsed` is the per-frame delta (`last_frame.elapsed()` at mod.rs:168) — so the animation's wall-clock progression is driven by **how often draw() is called**.

`exit_effect` is set when the user invokes `/quit` (mod.rs:524, 570, 1415); the loop is expected to keep ticking until the effect finishes, then teardown.

### 4.2 Decision: **Effects move WITH the render thread**

The render thread owns `boot_fx: Option<Effect>` and `exit_fx: Option<Effect>`. Reasoning:

| Option | Verdict |
|---|---|
| **A. Render thread owns effects, ticks them on its own clock.** | ✅ Recommended. |
| B. Main side ticks effects (calls `process` against a side buffer) and ships the buffer to render thread. | ❌ Defeats the whole point — `process` writes into the actual frame buffer that ratatui is mid-rendering. To pre-tick we'd need to replay the effect against a `Buffer`, ship the buffer, and have render thread overlay it. Adds copies, jitter, and a parallel buffer machinery. |
| C. Main side ships only "effect state" and render thread reconstructs. | ❌ tachyonfx's `Effect` is opaque and complex; no clean serialization boundary. |

**Why (A) works**: effects are write-only on the frame buffer with no read-back into App; their *only* input from outside is `elapsed` and the frame `area`. Both are owned naturally by the render thread.

**Mechanics**:

- The render thread maintains `last_frame: Instant` (was on main).
- For each frame it draws, it does `let elapsed = last_frame.elapsed(); last_frame = Instant::now();` before calling `fx.process(elapsed.into(), buf, area)`.
- The main side spawns effects via the channel: a separate small message type, e.g. `RenderCmd { Frame(RenderModel), SpawnBootFx, SpawnExitFx, … }`, or a side `mpsc<EffectCmd>`. Recommended: keep effects "implicit" — main side sends `SpawnBootFx` once at startup and `SpawnExitFx` once on `/quit`. Animation pacing is then completely render-thread-local and unaffected by main-loop hitching.
- When `exit_fx.done()`, the render thread sends an `ExitAnimationDone` signal back to main so main can begin teardown. (Today, mod.rs polls effect state implicitly via continued drawing; the new design needs an explicit ack so main can break the loop.)

### 4.3 The `tachyonfx::Effect: Send` question

`Effect` must be `Send` to live on the render thread. Per tachyonfx's public API (it's expected to be used from background threads in TUI apps), this is almost certainly fine. The implementing PR should add `static_assertions::assert_impl_all!(Effect: Send);` next to the import.

### 4.4 Jitter / timing implications

- Today: animation `elapsed` is tied to draw cadence — if a stream delta lands mid-frame, the next frame's `elapsed` is small, and the one after is large; ratatui's diff write smooths this. No regression in the new model.
- New: render thread's clock is monotonic regardless of main-loop pressure. **Animations get *smoother* under load**, not worse, because the main loop's throttle can no longer compress effect time.
- Edge case: if the channel is full and the render thread is slow, animations still tick at *its* draw rate. They never starve.

---

## 5. Threading architecture

### 5.1 Channel choice

**Recommendation: `tokio::sync::watch::channel::<Arc<RenderModel>>` for the frame, plus a small `tokio::sync::mpsc` for sideband commands.**

| Option | Pros | Cons |
|---|---|---|
| `watch` | Latest-wins built-in: stale frames are silently overwritten. Render thread reads the current value when it's ready to draw. Backpressure is naturally absent — exactly the semantic we want. | Render thread is `std::thread`, not async — has to use `watch::Receiver::changed().await` in a blocking_recv pattern via `tokio::runtime::Handle::block_on` OR poll. Cleaner: use `crossbeam_channel` or `std::sync::mpsc` semantics. |
| `mpsc::channel::<Arc<RenderModel>>(1)` (bounded) with `try_send` + drain-then-take | Simple. Render thread `recv()` blocks on `std::sync::mpsc` if desired. | Requires explicit "drop stale" on `Err(Full)`: `let _ = chan.try_send(model);` |
| `std::sync::mpsc::sync_channel(1)` | Std-only, plain. | Same drain pattern. |

**Final recommendation**: a single-slot `parking_lot::Mutex<Option<Arc<RenderModel>>>` + `std::thread::Thread::unpark` notification. This is **the cleanest "latest-wins" primitive** for cross-runtime hand-off:

```rust
struct FrameSlot {
    inner: parking_lot::Mutex<Option<Arc<RenderModel>>>,
    render_thread: std::thread::Thread,
}

impl FrameSlot {
    fn publish(&self, m: Arc<RenderModel>) {
        *self.inner.lock() = Some(m);
        self.render_thread.unpark();
    }
    fn take(&self) -> Option<Arc<RenderModel>> {
        self.inner.lock().take()
    }
}
```

Render loop:

```rust
loop {
    std::thread::park();                       // sleep until a frame is published
    while let Some(model) = slot.take() {       // drain — only newest survives anyway
        render_frame(&mut terminal, &model, &mut boot_fx, &mut exit_fx)?;
    }
    if shutdown.load(Acquire) { break; }
}
```

No async, no tokio leakage into the render thread, no unbounded queue, "latest wins" is intrinsic. 60fps throttle stays on the main side.

### 5.2 Sideband command channel

A separate `std::sync::mpsc::Sender<RenderCmd>`:

```rust
enum RenderCmd {
    SpawnBootFx,
    SpawnExitFx,
    Resize,           // optional: forces a redraw on next event
    Teardown { ack: std::sync::mpsc::Sender<TerminalReturn> },
}
```

### 5.3 Terminal ownership

**The render thread owns the `Terminal<CrosstermBackend<Stdout>>` for its entire life.** Created on main before the thread starts, then `terminal` is **moved** into the thread via the spawn closure. No `Arc<Mutex<Terminal>>` — that just relocates the blocking write into a contended critical section.

### 5.4 `terminal.size()` and resize

Currently called twice per draw (line 409 in draw.rs, and `scrub_crossterm_terminal_edges` does it again at viewport.rs:94). It hits the kernel via crossterm — quick, but blocking-capable.

- The render thread calls `terminal.size()` as needed; it's local to it.
- The main side needs *some* size info to compute layout for the snapshot (esp. `protected_bottom_rows` and the line-cache width).

**Two-clock approach**:

1. Main starts a crossterm event listener (already exists in mod.rs via `event::poll` / equivalent). On `Event::Resize(w,h)`, main updates a cached `Arc<AtomicU32>` (packed w/h) and sets `needs_redraw`.
2. `build_render_model` reads the cached size. Initial size is queried once at startup before the thread takes the terminal.

There is a brief window after a resize where the main side may build a frame against the previous size; the render thread, when it draws, will use its current `terminal.size()` and ratatui will re-layout. The visible artifact is at most one stale frame, then auto-correction.

### 5.5 Teardown

The render thread owns the `Terminal`, but `mod.rs`'s post-loop teardown calls `disable_raw_mode`, `LeaveAlternateScreen`, etc., on the terminal. Two patterns:

| Pattern | Trade-offs |
|---|---|
| **A. Render thread does teardown.** Main sends `RenderCmd::Teardown` and waits on the ack channel; render thread runs ratatui's cleanup, drops the terminal, sends ack, exits. | Single owner end-to-end. Clean. |
| B. Render thread hands the terminal back. Render thread sends `Terminal` over a oneshot, main does teardown. | Risk: terminal is in some intermediate state. Hand-off race during signal-driven shutdown. |

**Recommendation: (A)**. Main's existing bounded-teardown timeout (`signals::TEARDOWN_TIMEOUT_SECS` = 7s — signals.rs:61) wraps the ack wait; if the render thread is wedged, the watchdog still fires (`WATCHDOG_TIMEOUT_SECS` = 10s — signals.rs:62).

### 5.6 Signal watchdog status

**Keep it, thinned.** Today it exists because a blocked main-loop write means `select!` never observes the signal-channel send (signals.rs:17-32). After this refactor:

- The main task no longer does the terminal write. Its `select!` becomes naturally responsive again, even if the render thread is wedged on stdout.
- The signal handler sends on the shutdown channel; main observes it, breaks the loop, runs bounded teardown (which sends `RenderCmd::Teardown` and waits with a budget).
- The std-thread watchdog (signals.rs:lines 132+) stays as the last-line backstop: if the render thread **itself** is wedged AND teardown ack never arrives within the teardown budget, `std::process::exit(1)` still fires.

So: **the watchdog is no longer the *primary* safety net; it's the residual one.** The "draw blocked the runtime" failure mode is eliminated entirely. We can probably reduce `WATCHDOG_TIMEOUT_SECS` once we have confidence — but defer that to a follow-up.

### 5.7 Architecture diagram

```
┌──────────────────────────────────────────┐         ┌──────────────────────────┐
│ MAIN TOKIO TASK (mod.rs event loop)      │         │ RENDER STD::THREAD       │
│                                          │         │                          │
│  tokio::select! { … }                    │         │  loop {                  │
│      ↓                                   │         │     park();              │
│  app.needs_redraw && throttle_ok         │         │     while let Some(m) =  │
│      ↓                                   │         │         slot.take() {    │
│  build_render_model(&mut App, …) ────────┼─publish─┼──>  render_frame(        │
│      ↓                                   │  +unpark│         &mut terminal,   │
│  app.needs_redraw = false                │         │         &m,              │
│                                          │         │         &mut boot_fx,    │
│                                          │         │         &mut exit_fx)?;  │
│  (signals, stream events, ticks, …)      │         │     }                    │
│                                          │         │  }                       │
│   ↓ shutdown ↓                           │         │                          │
│  RenderCmd::Teardown ─────────────────── │ sideband│  cleanup + ack           │
│  recv ack (bounded by TEARDOWN_TIMEOUT)  │ <───────┼── ack                    │
│      ↓                                   │         │  exit                    │
│  save_session, hooks, std::process::exit │         └──────────────────────────┘
└──────────────────────────────────────────┘
                          ↑
                   signal watchdog (std::thread, retained as residual backstop)
```

---

## 6. The `draw()`-side refactor

Two new functions; the existing `draw()` is removed.

### 6.1 `build_render_model` — runs on main

```rust
pub(crate) fn build_render_model(
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    registry: &Arc<synaps_cli::skills::registry::CommandRegistry>,
    secret_prompts: &synaps_cli::tools::SecretPromptQueue,
    term_size: ratatui::layout::Rect,    // cached from last resize / initial query
) -> Option<Arc<RenderModel>>            // None == "skip frame" (e.g. gamba_child)
```

Body, in order:

1. **gamba gate**: `if app.gamba_child.is_some() { return None; }`
2. **Layout math** (replicated from draw.rs:410-424):
   - `has_subagents = !app.subagents.is_empty();`
   - `subagent_height = …`
   - `input_inner_width = term_size.width.saturating_sub(2);`
   - `(input_lines, cursor_row, cursor_col) = app.input_wrap_info(input_inner_width);`
     (After the refactor: `input::wrap_info(&app.input, app.cursor_pos, input_inner_width)`.)
   - `input_height = …`
   - `download_height = …`
   - `protected_bottom_rows = …`
3. **Line-cache maintenance** (lifted from draw.rs:522-528):
   - Compute `content_width = msg_area.width as usize - 2` from the derived layout.
   - `if app.line_cache.as_ref().map_or(true, |(w,_)| *w != content_width) { app.line_cache = Some((content_width, app.render_lines(content_width))); }`
   - Convert the `Vec<Line<'static>>` to `Arc<[Line<'static>]>` for sharing (or store as `Arc` natively on App going forward).
4. **Scroll bookkeeping** (lifted from draw.rs:535-551). Compute final `scroll_back`, then write back to `app.scroll_back` and `app.last_line_count`.
5. **Visible range + msg_area_rect** (lifted from draw.rs:553-583). Compute `(start, end)`. Compute `msg_inner` deterministically (mechanical from the layout constraints — `msg_area.x+1, msg_area.y+1, w-2, h-2` accounting for top/bottom borders and horizontal padding). Write back `app.msg_area_rect` and `app.visible_line_range`.
6. **Selection range**: `let selection = app.selection_range();`
7. **Subagent snapshots**: `app.subagents.iter().map(SubagentSnap::from).collect()`.
8. **Sidecar pills**: order with `order_sidecar_pills` (already pure — draw.rs:59) and project into `Vec<SidecarPillSnap>`.
9. **Ghost hint**: pure logic from draw.rs:898-936 against `commands::all_commands_with_skills(registry)`.
10. **Toasts**: `app.toasts.visible().cloned().collect()`.
11. **Modals**: `app.settings.clone().map(|s| (s, RuntimeSnapshot::from_runtime_with_health(runtime, registry, app.model_health.clone())))`; `app.plugins.clone()`; `app.models.clone()`; `app.help_find.clone()` (or `HelpFindSlot::Shared(_)`).
12. **Secret prompt**: project from `secret_prompts.active()`.
13. **Runtime strings**: `runtime_model = runtime.model().to_string()`, `runtime_thinking = runtime.thinking_level().to_string()`.
14. Assemble and return `Some(Arc::new(RenderModel { … }))`.

### 6.2 `render_frame` — runs on render thread

```rust
pub(crate) fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &RenderModel,
    boot_fx:  &mut Option<Effect>,
    exit_fx:  &mut Option<Effect>,
    last_frame: &mut std::time::Instant,
) -> io::Result<()>
```

Body:

1. `let elapsed = last_frame.elapsed(); *last_frame = Instant::now();`
2. `viewport::scrub_crossterm_terminal_edges(terminal, model.protected_bottom_rows, …)?;`
3. `terminal.draw(|frame| { … })?;` — the closure body is mechanically lifted from draw.rs:431-1146, with every `app.*` rewritten to `model.*`:
   - Header: build status span from `model.streaming`, `model.spinner_frame`, `model.status_text`, `model.subagents`; append `sidecar_pill_spans_from_snaps(&model.sidecar_pills, model.spinner_frame)`.
   - Messages: render `model.lines[start..end]` directly; apply selection overlay using `model.selection`.
   - Logo: from `model.messages_empty`, `model.logo_build_t`, `model.logo_dismiss_t`.
   - Subagent panel: from `model.subagents`.
   - Input: re-walk `model.input.chars()` with the same wrapping; ghost-hint from `model.ghost_hint`.
   - Footer: all scalars + `model.runtime_model`, `model.runtime_thinking`.
   - Effects: unchanged shape, with `elapsed` from the local clock.
   - Modals & secret prompt: from `model.settings`, `model.plugins`, `model.models`, `model.help_find`, `model.secret_prompt`.
   - Toasts: from `model.toasts`.
4. Return `Ok(())`.

### 6.3 What stays in `draw.rs` (becomes `render_frame.rs`)

All pure helpers — `sidecar_pill_segment`, `sidecar_pill_text`, `order_sidecar_pills`, `bash_trace`, `format_tool_name`, `render_active_tasks_line`, `render_toasts`, `boot_effect`, `quit_effect` — survive unchanged. Helpers that took `&App` (e.g. `sidecar_pill_spans`) gain a sibling that takes the projection.

---

## 7. Migration plan (each step compiles)

### Step 0 — Spec landed (this document). No code change.

### Step 1 — Introduce `RenderModel` skeleton + `build_render_model`. **`render_frame` still runs on the main task synchronously.**

> This is the de-risking step. It proves snapshot completeness *without* threading.

- Add `render_model.rs` with the struct and the projection types.
- Add `build_render_model(&mut App, …) -> Option<Arc<RenderModel>>`.
- Rename current `draw` body's inner closure into `render_frame(&mut Terminal, &RenderModel, &mut Option<Effect>, &mut Option<Effect>, &mut Instant)`.
- Mod.rs's draw call becomes:
  ```rust
  if let Some(model) = build_render_model(&mut app, &runtime, &registry, &secret_prompts, cached_size) {
      render_frame(&mut terminal, &model, &mut boot_fx, &mut exit_fx, &mut last_frame)?;
  }
  ```
- **Behavior must be byte-identical.** Visual regression test (manual smoke + snapshot tests if any).
- The `signal_watchdog` and the blocking-write risk are unchanged at this step. That's fine.

### Step 2 — Move the render thread. **Threading isolated to this PR.**

- Spawn the std::thread at app startup; move `terminal`, `boot_fx`, `exit_fx`, `last_frame` into it.
- Add `FrameSlot` (parking_lot::Mutex + Thread handle) and the `RenderCmd` sideband.
- Mod.rs's draw site becomes:
  ```rust
  if let Some(model) = build_render_model(&mut app, …) {
      frame_slot.publish(model);
  }
  ```
- Implement `RenderCmd::Teardown` ack with a bounded wait inside the existing teardown budget.
- Verify under stress: a slow PTY consumer (`pv -q -L 100 < /dev/null`-style backpressure) must NOT wedge the main task. Signals stay responsive.

### Step 3 — Reduce `signals.rs` watchdog footprint (optional follow-up).

- With Step 2 in place, the "primary" wedge cause is gone. Consider:
  - Lowering `WATCHDOG_TIMEOUT_SECS` (signals.rs:62).
  - Adding a self-test: deliberately stall the render thread, confirm main exits via signals path within budget without the watchdog firing.
- Do **not** delete the watchdog — it remains the last line of defense.

### Step 4 — `help_find::render` to `&state` (clean-up).

- Refactor `help_find::render` to accept `&HelpFindState`; move its mutations to the event handler that owns the modal.
- Replace `HelpFindSlot::Shared` with `HelpFindSlot::Snap` exclusively. Drop the enum.

### Step 5 — Move `ActiveTasks` and `line_cache` to `Arc` storage on App (perf polish).

- Switch `App::active_tasks` and `App::line_cache`'s line vec to `Arc<…>` to make the snapshot true refcount-only.

---

## 8. Risk register (ranked)

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| 1 | **Snapshot incompleteness** — `build_render_model` misses a field draw needed; tearing or stale UI. | **Critical** | Step 1 in the migration plan runs synchronously: any incompleteness is a visual bug, not a race. The §1.3 table is the exhaustive checklist; the implementer ticks each row. Add a debug build assertion that asserts equivalent visual output (snapshot test of a known scene). |
| 2 | **`help_find` mutation requirement** — render thread must `&mut` a piece of App state. | High | Step 1 of refactor: change `help_find::render` to `&state`. Bridge: `HelpFindSlot::Shared(Arc<Mutex<…>>)` with explicit lock per frame (tiny critical section). |
| 3 | **`tachyonfx::Effect: Send` violation** — won't compile, but worse is a subtle runtime panic if it uses interior `Rc`. | High | `static_assertions::assert_impl_all!(Effect: Send);` at module top. If it fails, fall back to "main owns effects, ships pre-tick'd buffer" — implementation cost real but tractable. |
| 4 | **Teardown ordering / hang** — render thread wedged on stdout during `/quit` exit animation; main waits for ack. | High | Bounded teardown timeout (signals.rs:61, already present) wraps the ack wait. Watchdog (signals.rs:62) retained as residual backstop. Document the budget explicitly. |
| 5 | **Resize race** — main builds frame against `(w0,h0)`; render thread draws against `(w1,h1)`. | Medium | ratatui re-layouts against `terminal.size()` at draw time; visible artifact is ≤1 stale frame. Always set `needs_redraw` on `Event::Resize` so the corrected frame is produced immediately after. |
| 6 | **Effect timing/jitter when effects move to render thread** — animations look different. | Medium | They look **smoother** under load (render thread's clock is independent of main-loop pressure). Verify boot animation and quit animation visually. |
| 7 | **`msg_area_rect` write-back race** — main computes & stores rect for mouse mapping; render thread uses a possibly-different rect. | Medium | Layout computation is purely a function of `(term_size, has_subagents, n_subagents, has_tasks, input_height)` — replicate it deterministically in `build_render_model`. Mouse mapping accuracy is unaffected as long as both sides compute identically. Add a unit test for the layout fn. |
| 8 | **Per-frame clone cost regression** — `lines`, modals, toasts cloned at 60fps. | Medium | Hot path (`lines`) goes through `Arc`. Modals are only allocated when open. Toasts are sparse. Benchmark before/after with a deliberately-large transcript. |
| 9 | **"Draw reads App state mutated by the same main loop"** — between `build_render_model` and `frame_slot.publish`, main might mutate App again, and the next snapshot will be built off that newer state. | Low (by construction) | The snapshot is built and published atomically from the main task before yielding to `select!`. No concurrent mutator on App exists. |
| 10 | **Channel-full bookkeeping** — implementation chooses `mpsc(1)` instead of `FrameSlot`; forgets the `try_send → drop on Full` pattern; UI stops updating under load. | Low | The recommended `FrameSlot` design has no notion of "full" — newer publish overwrites older. Either approach works if implemented correctly; FrameSlot is simpler to audit. |
| 11 | **`Toast`, `SettingsState`, `PluginsModalState`, `ModelsModalState`, `HelpFindState` are not `Send`.** | Low | Static-assert each at the spec's module top during Step 1. If any fails, project a Send-only subset (the visible fields). |
| 12 | **Watchdog over-fires** — if a legitimate slow teardown extends past `WATCHDOG_TIMEOUT_SECS`. | Low | The compile-time invariant at signals.rs:65 (`WATCHDOG_TIMEOUT_SECS > TEARDOWN_TIMEOUT_SECS`) prevents drift. Don't reduce the margin without measurement. |

---

## Appendix A — Quick file:line index

- Bug site: `crates/agent-tui/src/tui/draw.rs:392` (signature), `:431` (`terminal.draw`), `:1146` (closure end + implicit flush).
- Edge-scrub (raw backend writes): `crates/agent-tui/src/tui/viewport.rs:86-115`.
- Throttle + draw call: `crates/agent-tui/src/tui/mod.rs:156-180` (main loop), `:174`, `:657`, `:1373`, `:1822`, `:1850` (other draw-call sites that must all switch to `build_render_model` + `frame_slot.publish`).
- Effect spawn sites: `mod.rs:130` (`boot_fx`), `:524`, `:570`, `:1415` (`exit_fx`).
- App struct: `crates/agent-tui/src/tui/app.rs:43-179`.
- `input_wrap_info`: `app.rs:343-384`.
- `selection_range`: `app.rs:815-824`.
- `render_lines`: `crates/agent-tui/src/tui/render.rs:14`.
- `SubagentState` definition: `app.rs:184-192`.
- Watchdog constants & invariant: `crates/agent-tui/src/tui/signals.rs:59-68`.
- `help_find::render` (`&mut state` callee): `crates/agent-tui/src/tui/help_find.rs:58`.

---

*End of spec. This document is the implementation contract for the Step-1 PR; subsequent steps may extend it but should not contradict §3 (the `RenderModel` shape) without a follow-up amendment.*
