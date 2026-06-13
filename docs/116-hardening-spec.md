# #116/A3 Hardening Spec — review fixes (S208)

Branch `fix/116-review-hardening` off dev. Fixes the 8-agent review findings
(docs/reviews/SYNTHESIS.md). Each SLICE is built + tested + committed independently.
NO `cargo fmt` (repo never rustfmt'd). Verify each slice; live-test the terminal-affecting ones.

## SLICE 1 — P0 Panic safety (#121)  [render_thread.rs, mod.rs]
Goal: a crash NEVER leaves the terminal in raw mode.
1. Wrap the render-thread body in `std::panic::catch_unwind(AssertUnwindSafe(...))`. On panic
   (or normal return), ALWAYS call `do_teardown()` so the terminal is restored, then set
   `exit_done = true` so the main loop stops waiting.
2. Install a `std::panic::set_hook` (once, at spawn or run start) that calls
   `lifecycle::emergency_teardown_terminal()` before the default hook — backstop for a panic on
   ANY thread (main task included). Chain to the previous hook.
3. Fix `RenderHandle::Drop`: drop `cmd_tx` (disconnect) AND `unpark()` the thread so a parked
   render thread wakes, sees the disconnect, tears down, and exits. (Currently only detaches.)
Verify: build; `panic!` injected in render path (temp) → terminal restored on exit (revert probe).
Live: clean /quit + Ctrl-C still work; no regression.

## SLICE 2 — P1 one-liners (#123)  [render_thread.rs, draw.rs, mod.rs, Cargo.toml]
1. `exit_done.store(Relaxed)` → `store(Release)` (render_thread.rs ~275) to pair with load(Acquire).
   Audit boot_done too.
2. `crossterm::terminal::size().unwrap_or_default()` (mod.rs): if width==0||height==0, SKIP
   build/publish this frame (like the gamba gate) — don't publish a 0×0 model.
3. `draw.rs:549` (+1591) `.unwrap()` on line_cache → graceful fallback (rebuild or empty slice),
   no panic on the main task.
4. Root `Cargo.toml`: add `resolver = "2"` to `[workspace]`.
5. Root `Cargo.toml`: prune unused direct deps (verify each is genuinely unused by `src/` before
   removing — grep src/ for the crate; remove only if zero hits AND build stays green).
Verify: build green after EACH; `cargo tree` sanity after dep prune.

## SLICE 3 — P0 Gamba TTY race (#122)  [gamba.rs, render_thread.rs, mod.rs]
Goal: the casino child and the render thread never write the same fd concurrently.
- Add `RenderCmd::Pause { ack }` + `RenderCmd::Resume` (or reuse Teardown-style ack). Before
  `launch_gamba()` does any raw-mode/alt-screen change, send Pause + wait ack so the render thread
  is guaranteed OUT of `terminal.draw()` and idle. After the casino child exits, send Resume +
  a Clear so ratatui repaints.
- The render thread, on Pause: finish any in-flight draw, ack, then park until Resume (ignore
  publishes while paused, or drain-and-drop).
Verify: build; live — launch casino (/gamba or whatever), play, exit → terminal intact, no escape
garbage; render resumes cleanly.

## SLICE 4 — P1 per-frame clones (#124)  [draw.rs build_render_model, render_model.rs, app.rs]
Goal: stop deep-cloning heavy App structures every frame (spec §7 Step 5).
- Change App storage of the heavy structures to `Arc<...>` so the snapshot is a refcount bump:
  the settings modal state, `app.plugins`, the `Toast` list (or its Lines), `active_tasks`.
- help_find FIX: the modal scroll mutation is currently thrown away (draw.rs:1591 clones, render
  mutates the clone). Either store help_find behind `Arc<Mutex<>>` and render through it, or write
  the mutated `visible_height` back to `App::help_find`. Pick the cleaner one.
Verify: build; tests; live — open settings/plugins/help modals, scroll, confirm state persists;
re-measure streaming CPU (should drop vs the deep-clone baseline).

## SLICE 5 — P2 quick polish (#120 subset; DEFER run() split)
1. Delete dead `render_toasts()` + `sidecar_pill_spans()` (+ their `#[allow(dead_code)]`).
2. Fix stale/lying comments: the "FIX A (draw I/O error → break)" comment at mod.rs:~1793 (render
   thread logs+keeps rendering, doesn't break); the "Step 1/Step 2 will ship" comments now that
   Step 2 shipped.
3. API: add `RenderHandle::publish`, make `slot` private; remove dead `msg_inner_rect`.
4. NITs (if cheap): `impl Clone for Focus`→derive; `&PathBuf`→`&Path` commands.rs:302.
DEFER to a separate task: the run() 1964-line split (big, risky, own effort).
Verify: build green, 0 new warnings.

## Final
Full workspace test serial (all crates) + live smoke (boot/stream/quit/SIGTERM) before merge.
Then merge fix/116-review-hardening → dev.
