# Shady Smells — Adversarial Review
**Branch:** `dev` @ `17051f2` — merge commit: A3 crate split + #116 render thread  
**Reviewer:** Shady (adversarial lens)  
**Date:** 2025-07-14  
**Scope:** New/changed files only. Pure renames excluded.

---

## Verdict Up Front

Look, I'm not gonna lie — the architecture here is actually good. The render-thread separation is clean, the mailbox design (park/unpark + latest-wins slot) is solid, and the crate split was done without wrecking existing callers. That said, there are real problems in here, and a few of them are the kind that come back to bite you at 3am. Let's get into it.

**Overall rating: 6.5/10** — solid skeleton, but the connective tissue is messy. The big fish isn't a bug; it's `mod.rs` being a 2,000-line `run()` function that nobody is ever going to refactor. Everything else is smaller. But smaller doesn't mean harmless.

---

## 🔥 What's Actually Good (credit where it's due)

- **`render_thread.rs`** — The park/unpark mailbox is exactly the right primitive here. No spin, no condvar complexity, spurious wakeups handled, teardown is bounded. The ack-channel teardown pattern is clean.
- **`render_model.rs`** — Zero `&App` invariant is a smart constraint, proven by compilation. Snapshot completeness enforced structurally, not by convention.
- **Watchdog removal** — The old signal-watchdog-as-duct-tape is gone and the comments explaining *why* it's gone are genuinely good documentation. `signals.rs` is well-thought-out and has unit tests.
- **`#[allow(dead_code)]` discipline** — Every suppression has a reason comment. That's better than most codebases.
- **The three lib.rs facades** — The `extern crate self as synaps_cli` trick unblocks 300+ call sites without touching them. Clever, if a little dark magic.

---

## 💀 CRITICAL

### C1 — `run()` in `mod.rs` is **~1,964 lines long** (`mod.rs:45` → `mod.rs:2009`)
**File:** `crates/agent-tui/src/tui/mod.rs:45`

This function has been growing for a while, and this PR did nothing to stop it — it added ~500 more lines to it. `run()` currently handles: engine boot, session resume, extension loading, signal setup, render-thread spawn, a 15-arm `select!` loop, teardown, the boot/exit animation state machine, gamba integration, sidecar multiplexing, compaction, and secret prompt queueing.

That's not a function. That's a `main.rs` with delusions of grandeur.

The `select!` block alone has 15 arms. You can't reason about what happens when two of those arms fire simultaneously (you can't — `select!` doesn't give you that). Every new feature has been bolted onto this loop, and every one of those bolts is another place the invariants can break silently.

This wasn't introduced in *this* PR but this PR added the render-thread coordination (boot_done/exit_done atomics, fx_sent flags, the size-via-syscall pattern) directly into the loop body without extracting anything. The opportunity to split this during the crate extract was missed.

**What it should be:** Split into at minimum `run_event_loop()` (takes pre-built `AppState`), `setup()` (terminal + render thread), and `teardown()`. The `select!` arms should each call a named handler, not inline hundreds of lines.

**Severity: CRITICAL** — readability, maintainability, testability. You cannot write a regression test for any behavior inside this function.

---

## 💀 HIGH

### H1 — Stale "Step 1 / Step 2" migration comments throughout `render_model.rs` and `draw.rs`
**Files:**
- `crates/agent-tui/src/tui/render_model.rs:4–6` ("In Step 1... Step 2 will ship it")
- `crates/agent-tui/src/tui/render_model.rs:41` ("for diagnostic / assert use in Step 2")
- `crates/agent-tui/src/tui/render_model.rs:94` ("in Step 1 we clone per-frame")
- `crates/agent-tui/src/tui/render_model.rs:112` ("retained for Step 2 ordering / debug")
- `crates/agent-tui/src/tui/draw.rs:477` ("Step 1 split")
- `crates/agent-tui/src/tui/draw.rs:734` ("In Step 2 it runs on the dedicated render thread")
- `crates/agent-tui/src/tui/mod.rs:200` ("Step 2: terminal lives on the render thread")

Step 2 **is done**. The render thread exists. These comments describe the *past*, not the present design. A new reader hitting `render_model.rs:5` ("Step 2 will ship it over a latest-wins slot") is going to think Step 2 hasn't landed yet — and then waste 20 minutes figuring out it has.

The module-level doc of `render_model.rs` reads like a migration ticket, not a design document. Needs rewriting to describe what exists, not what was planned.

**Severity: HIGH** — comments actively mislead about current architecture.

---

### H2 — `render_toasts()` and `sidecar_pill_spans()` in `draw.rs` are dead production code dressed up as "kept for tests"
**Files:**
- `crates/agent-tui/src/tui/draw.rs:288` — `render_toasts()` (dead, `#[allow(dead_code)]`, "Kept for potential future use")
- `crates/agent-tui/src/tui/draw.rs:95` — `sidecar_pill_spans()` (`#[allow(dead_code)]`, "Used in tests")

`render_toasts()` isn't used in *any* test. It's just dead. The comment "Kept for potential future use" is the classic "I don't want to delete this" excuse. If you want it for future use, that's what git history is for.

`sidecar_pill_spans()` is used once in a test at line 168 — ok, that's legit. But its private helper `sidecar_pill_segment()` (line 14) has `#[allow(dead_code)] // used in tests` and is *only* called from `sidecar_pill_spans()`, which is itself only called from the test. So the production rendering path (which uses `SidecarPillSnap` projections) doesn't use either of these functions. You have a full parallel `App`-coupled rendering path for sidecars sitting in the source tree, suppressed from warnings, that was superseded by the snapshot approach. That's confusion waiting to happen.

**What it should be:** Delete `render_toasts()`. Move `sidecar_pill_spans()`/`sidecar_pill_segment()` into a `#[cfg(test)]` block or test helper module.

**Severity: HIGH** — dead code that will confuse anyone touching the sidecar rendering path.

---

### H3 — `src/lib.rs` double-indirection re-export chain creates a confusing facade
**File:** `src/lib.rs:2–24`

After the crate split, `src/lib.rs` does this:
```rust
pub use agent_core::core;          // line 2: brings in `core` module
// ...
pub use core::config;              // line 16: re-exports FROM the re-export
pub use core::session;
// ...
pub use runtime::{Runtime, ...};   // line 26: `runtime` arrived via agent_engine re-export
```

`pub use core::config` resolves `core` as the module just re-exported from `agent_core` — a re-export of a re-export. This works, but if line 2 is ever removed or reordered, the downstream `pub use core::config` silently breaks. It also means `core` is reachable as both `synaps_cli::core` (via `pub use agent_core::core`) and `synaps_cli::core` (via `pub use core::config`'s parent) — the same path, but through two different chains.

Additionally, `epoch_millis` and `truncate_str` are re-exported twice: implicitly via the chain and explicitly at lines 41 and 44 with redundant `///` doc comments.

**Severity: HIGH** — maintenance trap; duplicate re-exports add noise.

---

### H4 — "FIX A" is a dangling reference with no implementation
**File:** `crates/agent-tui/src/tui/mod.rs:1793`

```rust
// yielding Err/None on a dead PTY (the confirmed busy-loop bug). FIX A
// (draw() I/O error → break) is the backstop for that case and fires
// on the very next render regardless of EventStream behaviour.
```

"FIX A (draw() I/O error → break)" is referenced as a named safety backstop. There is no "FIX A" label anywhere in the codebase. The render thread handles terminal write failures at `render_thread.rs:313` by logging a warning and *continuing* — it does **not** break the main loop. The comment claims a break-on-I/O-error path exists; it doesn't. The render thread eats the error and keeps rendering.

The main loop does break on `EventStream` returning None/Err (FIX C), but that's not what "FIX A" describes. This comment is a lie about a safety-critical path.

**Severity: HIGH** — incorrect description of dead PTY handling behavior.

---

## 🤡 MEDIUM

### M1 — `plugins/state.rs`: manual `impl Clone for Focus` when `#[derive(Clone, Copy)]` works
**File:** `crates/agent-tui/src/tui/plugins/state.rs:25–37`

```rust
pub enum Focus {
    Left,
    Right,
}

impl Clone for Focus {
    fn clone(&self) -> Self {
        match self {
            Self::Left => Self::Left,
            Self::Right => Self::Right,
        }
    }
}
```

The old code had `pub enum Focus { Left, Right }` — no `Clone`, no ceremony. The new code expands it to 7 lines and hand-writes a `Clone` implementation that's byte-for-byte what `#[derive(Clone)]` generates. This is rustfmt churn + an unnecessary manual impl in the same commit. Just `#[derive(Clone, Copy)]`. It's two unit variants. `Copy` is free.

**Severity: MEDIUM**

---

### M2 — `settings/mod.rs`: `use schema::SettingDef` buried mid-file after a function body
**File:** `crates/agent-tui/src/tui/settings/mod.rs:52`

```rust
pub(crate) fn theme_options() -> Vec<String> {
    // ... 15 lines ...
}

use schema::SettingDef;  // ← mid-file use, after a function definition
```

`use` statements belong at the top of the file. Every Rust programmer scanning this file will expect imports at the top and get confused when they hit one mid-body. This is either a crate-copy artifact or deliberate — neither is acceptable.

**Severity: MEDIUM**

---

### M3 — `draw.rs`: `use` statements at lines 1, 282, and 480 — mid-file imports in the main draw file
**File:** `crates/agent-tui/src/tui/draw.rs:282–284` and `draw.rs:480–483`

```
Line   1: use ratatui::{...}; use std::io; use tachyonfx::...
Line 282: use super::app::{App, SPINNER_FRAMES};
          use super::markdown::format_tokens;
          use super::theme::THEME;
Line 480: use super::render_model::{GhostHint, RenderModel, ...};
```

Three separate `use` blocks scattered across a 1,636-line file. The ones at 282 and 480 were written inline during development and never hoisted. This violates the codebase's stated hand-aligned style.

**Severity: MEDIUM**

---

### M4 — `commands.rs:302`: `&PathBuf` instead of `&Path`
**File:** `crates/agent-tui/src/tui/commands.rs:302`

```rust
system_prompt_path: &PathBuf,
```

Classic `clippy::ptr_arg` violation. Should be `&Path`. Not a bug, but it'll bite in CI and "we'll fix it later" never comes.

**Severity: MEDIUM**

---

### M5 — `commands.rs:761`: `TODO(phase 8 8B)` left in merged code + dead `StartStream` variant
**File:** `crates/agent-tui/src/tui/commands.rs:38,605,761,1463`

`CommandAction::StartStream` has `#[allow(dead_code)]`, is described as "reserved for future use," and both match arms that hit it are `=> {} // reserved for future use`. A dead enum variant with suppressed warnings + two empty match arms is a task-tracker item masquerading as code.

The `TODO(phase 8 8B)` comment at line 761 is untracked forward work left in merged source.

**What it should be:** Delete `StartStream` until the feature is implemented. Track the 8B work in the issue tracker.

**Severity: MEDIUM**

---

### M6 — `render_thread.rs:275`: `Ordering::Relaxed` store paired with `Ordering::Acquire` load on `exit_done`
**File:** `crates/agent-tui/src/tui/render_thread.rs:275`

```rust
exit_done.store(false, Ordering::Relaxed);  // on SpawnExitFx re-arm
// ...
exit_done.store(true, Ordering::Release);   // on effect completion
// ...
// Main side:
if exit_done.load(Ordering::Acquire) { break; }
```

The Release/Acquire pair on the `store(true)` / `load` is correct — that's the happens-before needed to see effect completion. But the `store(false, Relaxed)` on re-arm is inconsistent: a relaxed store paired with an acquire load doesn't guarantee visibility ordering. In practice this only matters if the exit effect is re-triggered (unusual in normal flow), but mixing Relaxed/Release stores on the same atomic is a smell that will confuse the next person who touches this.

**Severity: MEDIUM** — should be `Ordering::Release` for consistency.

---

### M7 — `plugins/state.rs`: file-level `#![allow(dead_code)]` with a no-context "Task 14/15" reference
**File:** `crates/agent-tui/src/tui/plugins/state.rs:1`

```rust
// Task 14/15 will use these variants/fields; keep them declared now for API stability.
#![allow(dead_code)]
```

Pre-existing, not introduced here — but not cleaned up during extraction either. A file-wide `#![allow(dead_code)]` is a compiler muzzle. "Task 14/15" is opaque unless you know the project's task numbering. Use a task tracker.

**Severity: MEDIUM**

---

## NIT

### N1 — `render_model.rs`: `#[allow(dead_code)]` on `lines_width` and `msg_inner_rect` fields
**File:** `crates/agent-tui/src/tui/render_model.rs:42,54`

`lines_width` is suppressed with "for diagnostic / assert use in Step 2" — Step 2 is done, no asserts were added. `msg_inner_rect` is described as "Consumed by `input.rs`" but is still dead_code suppressed. If it's consumed, why is the suppression still there?

**Severity: NIT**

---

### N2 — `render_model.rs:112`: `SidecarPillSnap.plugin_id` is dead weight allocated every frame
**File:** `crates/agent-tui/src/tui/render_model.rs:112–114`

The field comment says "retained for Step 2 ordering / debug; not read by renderer." Step 2 is done. The field is allocated and cloned into every frame snapshot, never consumed. Either wire it up or delete it.

**Severity: NIT**

---

### N3 — `mod.rs:76`, `sse.rs:7`, `api.rs:793`: dangling `REVIEW.md` document references
**Files:**
- `crates/agent-tui/src/tui/mod.rs:76` — "P5 in REVIEW.md"
- `crates/agent-engine/src/runtime/sse.rs:7` — "REVIEW.md P2"
- `crates/agent-engine/src/runtime/api.rs:793` — "REVIEW.md P2"

`REVIEW.md` does not exist at the repo root. The closest file is `docs/REVIEW-S205.md`. These references are unresolvable — anyone following up cannot look up P2/P5.

**Severity: NIT**

---

### N4 — `app.rs:398`: `eprintln!` with ANSI escape codes inside TUI code
**File:** `crates/agent-tui/src/tui/app.rs:398`

```rust
eprintln!("\x1b[31m[ERROR] Failed to save session: {}\x1b[0m", e);
```

Raw escape codes to stderr while the terminal is in raw mode = display corruption. Should be `tracing::error!()` or a toast. Pre-existing, not introduced here.

**Severity: NIT**

---

### N5 — `sidecar_event` arm: `select_all` rebuilds futures Vec every loop iteration
**File:** `crates/agent-tui/src/tui/mod.rs:295–318`

Every `select!` re-evaluation allocates a new `Vec`, boxes every future, and `select_all` drops the non-winning futures (discarding partial poll progress). For 1-2 sidecars: irrelevant. For the "Phase 8 8B" multi-sidecar world this comment hints at: visible allocation churn per iteration.

**Severity: NIT** (acceptable now, design debt for 8B)

---

### N6 — `plugins/state.rs:423`: `std::env::set_var` in async test
**File:** `crates/agent-tui/src/tui/plugins/state.rs:423`

```rust
std::env::set_var("SYNAPS_INSTALL_MIN_DISPLAY_MS", "200");
```

`set_var` is not thread-safe in multi-test environments. `current_thread` flavor reduces the risk but doesn't eliminate it if other tests in the crate run concurrently. Use a proper injection mechanism.

**Severity: NIT**

---

## Summary Table

| ID | File | Severity | Issue |
|----|------|----------|-------|
| C1 | `tui/mod.rs:45` | **CRITICAL** | `run()` is ~1,964 lines; 15-arm select loop; untestable monolith |
| H1 | `render_model.rs:4`, `draw.rs:477,734`, `mod.rs:200` | **HIGH** | Stale "Step 1/Step 2" migration comments — Step 2 is done |
| H2 | `draw.rs:288,95,14` | **HIGH** | Dead production code: `render_toasts` + pre-snapshot sidecar path |
| H3 | `src/lib.rs:2–24` | **HIGH** | Double-indirection re-export chain; duplicate `epoch_millis`/`truncate_str` exports |
| H4 | `mod.rs:1793` | **HIGH** | "FIX A" cited as safety backstop but doesn't exist; dead PTY break path misrepresented |
| M1 | `plugins/state.rs:25` | MEDIUM | Manual `Clone` for unit-variant enum; should `#[derive(Clone, Copy)]` |
| M2 | `settings/mod.rs:52` | MEDIUM | `use schema::SettingDef` mid-file after a function body |
| M3 | `draw.rs:282,480` | MEDIUM | Mid-file `use` blocks violate hand-aligned codebase convention |
| M4 | `commands.rs:302` | MEDIUM | `&PathBuf` should be `&Path` (clippy::ptr_arg) |
| M5 | `commands.rs:761,38` | MEDIUM | `TODO(phase 8 8B)` untracked + dead `StartStream` variant with suppressed warning |
| M6 | `render_thread.rs:275` | MEDIUM | `Ordering::Relaxed` store on `exit_done` — should be `Ordering::Release` |
| M7 | `plugins/state.rs:1` | MEDIUM | File-wide `#![allow(dead_code)]` with opaque "Task 14/15" reference |
| N1 | `render_model.rs:42,54` | NIT | `#[allow(dead_code)]` on fields with claimed-but-absent consumers |
| N2 | `render_model.rs:112` | NIT | `plugin_id` allocated+cloned every frame, never read post-Step-2 |
| N3 | `mod.rs:76`, `sse.rs:7`, `api.rs:793` | NIT | Dangling `REVIEW.md` references (file doesn't exist) |
| N4 | `app.rs:398` | NIT | `eprintln!` with ANSI escapes in raw-mode TUI context |
| N5 | `mod.rs:295` | NIT | `select_all` with per-iteration Vec allocation |
| N6 | `plugins/state.rs:423` | NIT | `std::env::set_var` in async test |
