# Code Review Synthesis — A3 + #116 (dev @ 17051f2)

**8 reviewers, 8 lenses, S208.** Individual reports in `docs/reviews/{chrollo,silverhand,yoru,case,shady,joestar,gojo,zero}-*.md`.

## Verdict
**Core design is sound. The edges need work.** No lost-wakeup, no deadlock on the hot path
(silverhand cleared it), the crate DAG is clean with no upward refs (chrollo), and both specs
were substantially matched (zero). The findings cluster on **panic/error edge paths**,
**per-frame clone cost**, and **finishing-pass hygiene** — exactly what happy-path live testing
(what we did) misses. That's why we sent eight.

---

## 🔴 P0 — PANIC SAFETY: terminal left in raw mode (CONSENSUS: joestar CRIT, silverhand HIGH, gojo H3, case)
If `render_frame()` panics — tachyonfx, a viewport index, a bad draw closure — the render thread
dies silently. `exit_done` never flips, the `Teardown` ack never arrives, and the user is stuck
staring at a frozen raw-mode screen. Compounding it: `RenderHandle::Drop` detaches the handle
**without** dropping `cmd_tx` or `unpark()`-ing, so any panic-unwind between spawn and `teardown()`
parks the render thread forever. No `catch_unwind`, no `panic::set_hook` backstop.
**Fix:** `catch_unwind` around the render-thread body that ALWAYS restores the terminal; a
`panic::set_hook` that calls `emergency_teardown_terminal`; make `Drop` drop `cmd_tx` + `unpark()`.
*(case suggests a 500ms post-teardown `exit(1)` hard-stop — a tiny, bounded successor to the watchdog.)*

## 🔴 P0 — Gamba/casino races stdout with the render thread (joestar HIGH, case)
`launch_gamba()` calls `disable_raw_mode()` + `LeaveAlternateScreen` directly on the main thread
while the render thread may be mid-`terminal.draw()` on the same fd 1 → interleaved escape
sequences, casino child inherits a broken terminal. **Fix:** route TTY mode changes through
`RenderCmd` (pause/handoff the render thread before takeover).

## 🟠 P1 — Per-frame deep clones (perf regression) (yoru HIGH)
The `Arc` lever was applied to the *cheap* projections but the *heavy* App-owned structures are
deep-cloned EVERY frame (up to 60fps): the settings modal, `app.plugins.clone()`, `Toast::clone`
(rich `Line`s), `active_tasks.clone()`, `help_find.clone()`. The snapshot approach is right; it
just missed the expensive structures. **Fix:** `Arc`-project them (spec §7 Step 5, deferred).

## 🟠 P1 — `crossterm::terminal::size().unwrap_or_default()` → 0×0 model (joestar HIGH)
On PTY close, `size()` errors → `Size{0,0}` → a zero-width `RenderModel` is built and published,
poisoning `msg_area_rect` geometry / mouse hit-testing. **Fix:** gate on `width==0||height==0`.

## 🟠 P1 — Memory ordering: `exit_done.store(Relaxed)` vs `load(Acquire)` (gojo H2, silverhand, joestar, shady)
Breaks the Release/Acquire pair → real visibility bug on ARM. **Fix:** `store(Release)`. One-liner.

## 🟠 P1 — `.unwrap()` panic bombs on the main task (gojo H1)
`draw.rs:549` (and 1591) `.unwrap()` on `line_cache` after a structurally-guaranteed rebuild — a
latent panic one refactor away from firing on the hottest path. **Fix:** graceful fallback.

## 🟠 P1 — Workspace manifest hygiene (chrollo HIGH ×2)
(a) Root `Cargo.toml` is missing `resolver = "2"` → new workspace silently uses resolver 1, leaking
dev-dep features into release and partially defeating the incremental-build win. (b) ~23 unused
direct deps in root `Cargo.toml` (zeroize, libc, syntect, tachyonfx, arboard, tower…) — pulled
transitively anyway; version-skew risk + undermines "bin = glue". **Fix:** add resolver, prune deps.

## 🟡 P2 — Polish cluster
- **help_find scroll mutation thrown away** (joestar/zero/yoru): `draw.rs:1591` clones `help_find`,
  `render(&mut clone)` mutates the throwaway → authoritative `visible_height` never updates → wrong
  scroll window on first modal open at non-default height.
- **Dead code** (shady): `render_toasts()` (no callers), `sidecar_pill_spans()` (superseded by
  `SidecarPillSnap`) — both `#[allow(dead_code)]`'d. Remove.
- **Stale/ghost comments** (shady): "Step 1/Step 2 *will* ship" comments — Step 2 already shipped;
  the "FIX A (draw I/O error → break)" comment at `mod.rs:1793` cites a backstop that doesn't exist
  (render thread logs the error and keeps rendering). Actively misleading.
- **`run()` is ~1964 lines** (shady): the crate extraction was the moment to split it; instead ~500
  lines were added. Split into lifecycle/dispatch/teardown.
- **API leak** (zero): `RenderHandle::slot` is `pub(crate)`; 4 `mod.rs` sites bypass encapsulation
  via `FrameSlot::publish`. Add `RenderHandle::publish`, make `slot` private. `msg_inner_rect` is a
  dead `#[allow(dead_code)]` field. No `static_assertions` Send guards (spec §8 R3/R11).
- **Double-indirection re-export** (shady): `src/lib.rs` has both `pub use agent_core::core` AND
  `pub use core::config` — re-export through a re-export.

## 🟢 P3 — NITs
Clear-command one-frame black flash (silverhand); boot animation fast-forwards if startup >750ms
because `last_frame` is set at spawn (joestar); `impl Clone for Focus` should `derive` (shady);
`&PathBuf`→`&Path` at `commands.rs:302` (gojo/shady); mutex-poison propagation in
`subagent_registry().lock().unwrap()` (gojo); `#[must_use]`/`Debug` omissions (gojo/zero); the
non-Unix signal fallback calls `emergency_teardown_terminal` against the Unix path's own warning (gojo).

---

## Recommended action
1. **Before any v0.3.0 release:** fix the two P0s (panic safety + gamba race) and the P1 one-liners
   (memory ordering, zero-size gate, the `.unwrap()`s, resolver="2"). These are correctness/UX.
2. Per-frame clones (yoru) + dead code + stale comments: a focused follow-up pass.
3. `run()` split + dep prune: opportunistic, lower urgency.

**None of this blocks the merge** — the architecture is right and the happy path is verified. These
are the hardening pass on a high-risk change, which is exactly what the review was for.
