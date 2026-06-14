# Spec Conformance & API Design Review
**Reviewer:** Zero (architect lens)
**Commit:** `17051f2` — merge: A3 crate split + #116 render thread
**Branch:** `dev`
**Date:** 2026-06-13
**Scope:** `docs/116-render-model-spec.md` + `docs/A3-spec.md` versus merged implementation

---

## Verdict at a Glance

The merge is **substantially conformant**. Both specs were executed faithfully; the
architecture is sound and the implementation is largely cleaner than the spec required.
The deviations identified below are either justified improvements or minor gaps — none
constitute a correctness bug. There is one medium-severity API hygiene issue and two
gaps in the spec's own risk-register mitigations that the implementation silently
resolved (both in its favour).

---

## 1. `docs/116-render-model-spec.md` — conformance

### §3 `RenderModel` struct

#### DEVIATION D-1 — `term_size` type: `Rect` in spec vs `Size` in implementation
**Severity: LOW**
**Spec §6 `build_render_model` signature:** declares `term_size: ratatui::layout::Rect` (a full rect with x/y/width/height).
**Implementation `draw.rs:497`:** uses `term_size: ratatui::layout::Size` (width/height only).

*Is it sound?* **Yes, and the implementation is correct.** A terminal's logical size has no meaningful `(x, y)` origin — passing a `Rect` here was a spec error. `Size` is the right type. The implementation correctly saturates subtraction on `term_size.width` and `term_size.height`. **No action needed; spec was wrong.**

---

#### DEVIATION D-2 — `active_tasks` field: `Arc<ActiveTasks>` in spec vs plain `ActiveTasks` clone
**Severity: LOW**
**Spec §3 + §2 row #4:** "Recommended: `Arc<ActiveTasks>` in App … Until then, `clone()` is fine."
**Spec §7 Step 5:** migrate to `Arc` storage as a perf polish step.
**Implementation `render_model.rs:66`:** `pub(crate) active_tasks: synaps_cli::extensions::active_tasks::ActiveTasks` — plain clone.
**Implementation `draw.rs:709`:** `active_tasks: app.active_tasks.clone()`.

*Is it sound?* **Yes.** The spec explicitly blessed this bridge. `ActiveTasks` derives `Clone` (it wraps a `HashMap<String, TaskState>`). At typical task counts (≤handful) the clone is under a microsecond. The spec's Step 5 migration note captures the outstanding work cleanly.

*Outstanding:* Step 5 (`Arc<ActiveTasks>`, `Arc<[Line<'static>]>` native on App) is unimplemented. Neither the spec nor the code marks this tracked anywhere (no task, no TODO comment). Low risk but should be captured.

---

#### DEVIATION D-3 — `HelpFindSlot` enum: not implemented
**Severity: LOW** (justified)
**Spec §3 (end of struct):**
```rust
pub(crate) enum HelpFindSlot {
    Snap(synaps_cli::help::HelpFindState),
    Shared(Arc<Mutex<HelpFindState>>),
}
```
**Implementation `render_model.rs:96`:** collapses this to:
```rust
pub(crate) help_find: Option<synaps_cli::help::HelpFindState>,
```
The `HelpFindSlot` enum — with its `Snap` / `Shared` bridge variants — was never created.

*Is it sound?* **Yes.** The spec's "preferred" path was a plain clone into `Snap`; the `Shared` variant was a fallback if `help_find::render` could not be changed. The implementation chose the clean path directly: clone per-frame and let the render closure's local mutation operate on the copy (`draw.rs:1591` — `model.help_find.clone()`). This skips the enum entirely, which is the right call. The double-clone (`help_find.clone()` in `build_render_model` **and** `model.help_find.clone()` in the render closure) is an artefact of `render_frame` receiving `model: &RenderModel` but `help_find::render` taking `&mut`. See D-8 / §7 Step 4.

---

#### DEVIATION D-4 — `msg_inner_rect` field is `#[allow(dead_code)]`
**Severity: MEDIUM**
**Spec §2 row #13:** "the render thread ships the rect back over a small `watch<LayoutHint>` channel" OR "replicate the layout computation on the main side" (recommended). The spec explicitly says this rect is **consumed by `input.rs` on next event** for mouse-coordinate mapping.
**Implementation `render_model.rs:54-55`:**
```rust
#[allow(dead_code)]
pub(crate) msg_inner_rect: Rect,
```
**The field is computed, stored in the model, written back to `app.msg_area_rect` in `build_render_model` (`draw.rs:580-583`), but the field in `RenderModel` itself is dead.**

*Is it sound?* **Mostly.** The write-back to `app.msg_area_rect` in `build_render_model` (`draw.rs:580-583`) is correct — mouse mapping still works because `app.msg_area_rect` is updated on main before the model is published. The `msg_inner_rect` field in `RenderModel` is therefore redundant; it was presumably scaffolded for Step 2 (render thread could potentially feed it back). The `#[allow(dead_code)]` suppression is a minor smell indicating incomplete follow-through — either the field should be removed (it serves no purpose now) or the Step-2 dataflow comment should be updated. As-is, it wastes a few bytes per frame and confuses the reader about why it's there.

**Recommendation:** Remove `msg_inner_rect` from `RenderModel` (the write-back on App covers the use-case) OR add a comment explaining it is reserved for a Step-2 reverse-channel. The bare `#[allow(dead_code)]` without explanation is insufficient.

---

#### DEVIATION D-5 — `lines_width` field is `#[allow(dead_code)]`
**Severity: NIT**
**Spec §3:** `lines_width: usize` — described as "for diagnostic / assert use in Step 2."
**Implementation `render_model.rs:43`:**
```rust
#[allow(dead_code)]
pub(crate) lines_width: usize,
```
Same pattern as D-4 — scaffolded for Step 2, currently unused. Unlike `msg_inner_rect`, this one's purpose is documented in the comment. The `#[allow(dead_code)]` is defensible here. A `#[cfg(debug_assertions)]` assertion using it would be cleaner than a blanket allow.

---

### §4 Effects on the render thread

**CONFORMANT.** The render thread owns `boot_fx: Option<Effect>` and `exit_fx: Option<Effect>` (`render_thread.rs` body, lines 228-229). Effects are **never shipped in `RenderModel`**. The render thread computes its own `elapsed` from `last_frame: Instant` (`draw.rs:752-755`). `boot_done` and `exit_done` `AtomicBool`s are returned from `spawn_render_thread` and used by the main loop to track effect completion without any shared Effect state. Architecture matches spec exactly.

---

### §5 Threading / mailbox / teardown

**CONFORMANT.** The implemented `FrameSlot` design matches the spec's `parking_lot::Mutex<Option<Arc<RenderModel>>>` + `Thread::unpark()` recommendation exactly.

#### DEVIATION D-6 — `RenderCmd::Resize` variant: absent from implementation
**Severity: LOW** (justified absence)
**Spec §5.2 sideband command channel:**
```rust
enum RenderCmd {
    SpawnBootFx,
    SpawnExitFx,
    Resize,           // optional: forces a redraw on next event
    Teardown { ack: … },
}
```
**Implementation `render_thread.rs:66-84`:** implements `SpawnBootFx`, `SpawnExitFx`, `Clear`, and `Teardown`. **`Resize` is absent; `Clear` is present in its place.**

*Is it sound?* **Yes, and the substitution is correct.** The spec's `Resize` variant was "optional" — a hint to force a redraw. The implementation chose the simpler path: `crossterm::terminal::size()` is called fresh on every `build_render_model` invocation (`mod.rs:202`), and `Event::Resize` is implicitly handled because all input events (including resize) call `app.request_redraw()` (`mod.rs:556`), which triggers `build_render_model` on the next tick. The added `Clear` command serves a different and necessary purpose (full-screen takeover recovery post-gamba exit). Net: the resize concern is covered without a dedicated command, which is strictly simpler.

**One gap remains from spec §5.4's "two-clock approach":** The spec recommended caching the terminal size in an `Arc<AtomicU32>` updated on `Event::Resize`. The implementation instead calls `crossterm::terminal::size()` inline every time `build_render_model` runs. This is a **syscall per frame** (up to 60/s). The syscall is fast but it's a kernel round-trip in the hot path. Not flagged as a deviation anywhere in the code; it's an informal "good enough" choice that works but should be documented as a conscious tradeoff.

---

#### §5.5 / §5.6 — Watchdog removal
**CONFORMANT with spec §7 Step 3, but more aggressive.** The watchdog (`std::process::exit(1)` after timeout) was retired. `signals.rs` no longer contains `WATCHDOG_TIMEOUT_SECS` or a watchdog thread. The spec called this an optional follow-up (Step 3: "Do not delete the watchdog — it remains the last line of defense"), but commit `025c569` deliberately retired it with the rationale that the root cause is now eliminated. This is a justified forward move — the watchdog was the symptom of the bug that was fixed.

---

### §6 `build_render_model` + `render_frame` signatures

**CONFORMANT.** Both function signatures match the spec's §6.1 and §6.2 exactly, with one justified deviation already captured (D-1: `Size` vs `Rect`). The 14-step body of `build_render_model` matches the spec's prescribed order precisely. All five App mutations (line_cache, scroll_back, last_line_count, msg_area_rect, visible_line_range) are hoisted to the main side as specified in §6.1.

---

### §7 Migration Steps 1→2

**CONFORMANT.** The spec described Step 1 as "synchronous — same task, no threading" and Step 2 as "spawn std::thread, FrameSlot, teardown ack." Both steps were merged together in this branch. The comments in render_model.rs (`// In Step 1 … Step 2 will ship it`) and render_thread.rs confirm the implementer was aware of the plan and collapsed the two steps. This is not a deviation — it's a valid compression that works because the Step-1 synchronous proof-of-concept validated the snapshot before threading was added.

---

### §8 Risk Register — resolved open items

| Risk | Spec resolution | Actual resolution |
|---|---|---|
| R2 — `help_find` `&mut` | `HelpFindSlot::Shared` bridge | Clone per-frame, local mutation — cleaner ✓ |
| R3 — `Effect: Send` | `static_assertions::assert_impl_all!(Effect: Send)` | Compiles (implicitly Send), but **no static assert** — see F-3 |
| R5 — Resize race | `Arc<AtomicU32>` cached size | `crossterm::terminal::size()` per-frame — acceptable but undocumented |
| R8 — Clone cost | `Arc<[Line<'static>]>` hot path | `lines` is per-frame `.to_vec().into()` (one deep copy then Arc-wrapped) — not a refcount bump from a cached Arc. Step 5 deferred. |

---

## 2. `docs/A3-spec.md` — conformance

**CONFORMANT.** The 3-crate layout (`agent-core → agent-engine → agent-tui → bin(synaps)`) is implemented and matches the spec's target DAG.

Verified from Cargo.toml:
- `crates/agent-core` — no in-repo deps ✓
- `crates/agent-engine` — depends only on `agent-core` ✓
- `crates/agent-tui` — depends on `agent-core` + `agent-engine` ✓
- Root `synaps` depends on all three ✓
- `members = ["crates/agent-core", "crates/agent-engine", "crates/agent-tui"]` ✓

#### DEVIATION D-7 — Bin NOT relocated to `crates/agent-runtime/`
**Severity: LOW** (documented, justified)
**Spec A3 "Deliberately NOT done":** explicitly documents this non-action. The root `synaps` package remains the bin/glue crate. Rationale (cargo-dist config, published binary name, `synaps_cli` lib name) is sound.

#### A3 `extern crate self as synaps_cli` pattern
The `lib.rs` of `agent-tui` uses `extern crate self as synaps_cli` to allow ~300 internal `synaps_cli::` references in `tui/` to resolve without mass renaming. Documented in lib.rs comment. Technically correct and the right constraint-respecting choice. No deviation.

#### A3 Open question — engine split if R3 manifests
**EMPIRICALLY RESOLVED:** 2.79s warm incremental check on a hot TUI edit; engine does not rebuild. Spec closed this risk. ✓

---

## 3. API / Module Design Review

### Public surface of `render_thread.rs`

```rust
pub(crate) enum RenderCmd { Teardown { ack }, SpawnBootFx { fx }, SpawnExitFx { fx }, Clear }
pub(crate) struct FrameSlot { … }               // .publish(&self, Arc<RenderModel>)
pub(crate) struct RenderHandle { slot, cmd_tx } // helpers: send_boot_fx, send_exit_fx, send_clear, teardown
pub(crate) fn spawn_render_thread(terminal) -> (RenderHandle, Arc<AtomicBool>, Arc<AtomicBool>)
```

#### FINDING F-1 — `RenderHandle::slot` is `pub(crate)` — leaked internal
**Severity: MEDIUM**
**File:** `render_thread.rs:114`
```rust
pub(crate) struct RenderHandle {
    pub(crate) slot:   FrameSlot,                    // ← exposed
    pub(crate) cmd_tx: mpsc::Sender<RenderCmd>,      // ← exposed
}
```
`slot` is accessed directly at four `mod.rs` call sites as `render_handle.slot.publish(model)`. This forces callers to know about `FrameSlot` as a type and reach into `RenderHandle`'s internals. By contrast, `cmd_tx` is correctly hidden behind `send_boot_fx`, `send_exit_fx`, `send_clear`, and `teardown` helpers. The design is internally inconsistent: publishing a frame — the most frequent operation — is the one that bypasses the encapsulation.

**Recommendation:** Add `pub(crate) fn publish(&self, model: Arc<RenderModel>)` to `RenderHandle`; make `slot` private. The four `render_handle.slot.publish(model)` sites in `mod.rs` become `render_handle.publish(model)`. `FrameSlot` can then be `pub(crate)` within the module only, with no need for external visibility.

---

#### FINDING F-2 — `spawn_render_thread` returns an anonymous tuple
**Severity: LOW**
```rust
pub(crate) fn spawn_render_thread(terminal: …)
    -> (RenderHandle, Arc<AtomicBool>, Arc<AtomicBool>)
```
The two `AtomicBool` outputs are positionally ambiguous. Callers pattern-match by variable naming (`let (render_handle, boot_done, exit_done) = …`). A named struct or type aliases (`type EffectDoneFlag = Arc<AtomicBool>`) would make the contract self-describing. Low severity: single call site, `pub(crate)`.

---

#### FINDING F-3 — Missing `Send` static assertions from spec §8 R3 + R11
**Severity: MEDIUM**
The spec's risk register explicitly required compile-time Send guards:
- R3: `static_assertions::assert_impl_all!(Effect: Send)`
- R11: `static_assertions::assert_impl_all!(Toast: Send, SettingsState: Send, PluginsModalState: Send, ModelsModalState: Send, HelpFindState: Send)`

Neither exists in the implementation. The code compiled (so all types are `Send`), but the gates that **keep them Send** across future refactors are absent. If someone introduces an `Rc<_>` or thread-local inside `Effect`, `Toast`, or any modal state, `Arc<RenderModel>: Send` will fail silently at a distant call site — not at the type definition.

**Recommendation:** Add to `render_model.rs` top-level:
```rust
const _: fn() = || {
    fn assert_send<T: Send + Sync>() {}
    assert_send::<RenderModel>();
    // assert_send::<tachyonfx::Effect>(); // verify in render_thread.rs
};
```
Zero runtime cost, directly satisfies the spec's demanded contract.

---

#### FINDING F-4 — `lines` arc-wraps a fresh `.to_vec()` per frame, not a cached Arc
**Severity: LOW** (perf note)
**draw.rs:549-552:**
```rust
let all_lines_vec: &[ratatui::text::Line<'static>] = &app.line_cache.as_ref().unwrap().1;
let total = all_lines_vec.len();
// Wrap in Arc for zero-copy clone into the model.
let lines: std::sync::Arc<[ratatui::text::Line<'static>]> = all_lines_vec.to_vec().into();
```
The comment says "zero-copy clone into the model" but **the `to_vec().into()` is itself a full deep copy** of `line_cache.1` every frame, even on cache hits (i.e., when the cache was NOT rebuilt). The Arc is useful for preventing a second deep copy when the model is eventually cloned (e.g. for Step-2 multi-consumer), but the *first* copy happens unconditionally here.

The spec's §7 Step 5 recommended storing `Arc<[Line<'static>]>` natively in `app.line_cache` so that a cache hit is a pure refcount bump. That would make the hot path truly zero-copy. As-is, a 1000-line conversation = ~1000 `Line` clones per frame regardless of whether content changed.

This is a known deferred optimization (Step 5), but the comment is misleading. The comment should read "cloned once here; Arc allows subsequent snapshot clones to be zero-copy."

---

#### FINDING F-5 — `RenderHandle::wake` is private with no doc
**Severity: NIT**
`render_thread.rs:122`: `fn wake(&self)` is private and called by all helper methods. Correct visibility. But adding a command that forgets to call `wake()` would silently work (the park/unpark loop will still eventually wake from a future event) but could cause visible delay. A doc comment noting "must call after send" on the helpers would prevent future mistakes.

---

### `agent-tui/src/lib.rs` — crate public surface

The lib.rs is a pure re-export façade. The entire TUI is accessible via `pub mod tui`. This is correct and expected: the bin accesses `agent_tui::tui::run(…)`. The `extern crate self as synaps_cli` trick is the only unusual element and is properly documented. No issues.

---

## 4. Open Questions from Specs — Silent Resolution Map

| Spec Section | Open Question | How Implementation Resolved It |
|---|---|---|
| §2 row #3 / §6.1 step 2 | `input_wrap_info` — spec proposed free fn `input::wrap_info(&str, usize, u16)` | Kept as method call on main; render frame re-walks `model.input.chars()` inline. Dual-width hazard avoided by using `frame.area().width` in render vs `term_size.width` in build. Refactor skipped; mild duplication remains. |
| §3.4 `Send` audit | "to be confirmed by implementing change" | Confirmed implicitly (compiles). No static assertions added. See F-3. |
| §5.2 `RenderCmd::Resize` | "optional: forces a redraw on next event" | Absent. Covered by per-frame `crossterm::terminal::size()` + all-events `request_redraw()`. Undocumented substitution. |
| §5.6 Watchdog | "keep it, thinned" / "do not delete" | **Deleted** in `025c569`. More aggressive than spec recommended; justified because the root cause is gone. |
| §7 Step 3 | Signal watchdog thinning — "optional follow-up" | Done in same branch. |
| §7 Step 4 | `help_find::render` refactor to `&state` | Skipped. Double-clone workaround in place (`draw.rs:1591`). Left as future cleanup. |
| §7 Step 5 | `Arc<ActiveTasks>`, `Arc<[Line<'static>]>` native on App | Fully deferred. `lines` gets a per-frame deep copy. No tracking issue created. |
| A3 §5 Benchmark | "Confirm engine does NOT rebuild on TUI edit" | Confirmed: 2.79s hot TUI edit, engine stays cached. ✓ |

---

## 5. Notable Deviations Summary (ranked by severity)

| ID | Severity | Location | Finding |
|---|---|---|---|
| F-1 | **MEDIUM** | `render_thread.rs:114` | `RenderHandle::slot` is `pub(crate)` — callers bypass encapsulation. Should be wrapped behind `RenderHandle::publish()`. |
| F-3 | **MEDIUM** | `render_model.rs` / `render_thread.rs` | Missing `Send` static assertions demanded by spec §8 R3+R11. Types are `Send` now but no compile-time guard exists against regressions. |
| D-4 | **MEDIUM** | `render_model.rs:54` | `msg_inner_rect: Rect` is `#[allow(dead_code)]` with no explanation. Field is redundant (write-back to App covers the use-case). Remove or document the Step-2 intent. |
| F-4 | LOW | `draw.rs:552` | `lines` is a full deep copy per frame even on cache hits; comment says "zero-copy" which is misleading. Step 5 deferred but comment should be corrected now. |
| D-1 | LOW | `draw.rs:497` | `term_size: Size` vs spec's `Rect`. Justified — `Size` is the correct type; spec was wrong. |
| D-2 | LOW | `render_model.rs:66` | `active_tasks` plain clone vs spec's `Arc<ActiveTasks>`. Spec blessed this bridge; Step 5 not yet tracked. |
| D-3 | LOW | `render_model.rs` | `HelpFindSlot` enum absent; replaced by direct clone. Cleaner outcome. |
| D-6 | LOW | `render_thread.rs` | `RenderCmd::Resize` absent; `Clear` substituted. Justified but undocumented. |
| F-2 | LOW | `render_thread.rs:200` | `spawn_render_thread` returns positional tuple of 3. Minor ergonomic issue, single call site. |
| D-5 | NIT | `render_model.rs:43` | `lines_width` is `#[allow(dead_code)]` — comment explains intent but no assertion uses it. |
| F-5 | NIT | `render_thread.rs:122` | `wake()` is undocumented; "call wake after send" contract is implicit. |

---

*End of review. Zero out.*
