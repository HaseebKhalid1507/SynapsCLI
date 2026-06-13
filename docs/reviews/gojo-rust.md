# Rust Correctness & Idioms Review
**Reviewer:** Gojo (agent)  
**Branch:** `dev` @ `17051f2`  
**Scope:** `crates/agent-tui/src/tui/{render_thread.rs, render_model.rs, draw.rs, mod.rs, signals.rs}` + `crates/agent-tui/src/lib.rs`  
**Date:** 2025-07-10

---

## Executive Summary (5 lines)

The render-thread split (#116) is architecturally sound and the critical path (park/unpark, latest-wins slot, bounded teardown) is correct.  One render-thread panic path exists: `build_render_model` has an `.unwrap()` after a branch that *should* guarantee `Some` but the invariant is subtle enough to be fragile.  The `exit_done` reset races — it is stored with `Relaxed` ordering while the matching load uses `Acquire`, violating the release/acquire pair requirement for a flag that must synchronise a thread exit.  The `Drop` impl for `RenderHandle` silently drops `join_handle` without first disconnecting `cmd_tx`, so the render thread will never observe `Disconnected` on the panic-drop path; it will park forever until the process exits.  The `tachyonfx` `sendable` feature is used correctly (no `unsafe impl Send`, the crate gates it through trait bounds); no `transmute` or raw pointer concerns.

---

## Findings

### CRITICAL

*(None — no `panic!` / `abort` calls directly in the render-thread hot loop.)*

---

### HIGH

#### H1 — `draw.rs:549` — `.unwrap()` on `line_cache` can fire on the render thread
**File:** `crates/agent-tui/src/tui/draw.rs:549`

```rust
let all_lines_vec: &[ratatui::text::Line<'static>] = &app.line_cache.as_ref().unwrap().1;
```

**The invariant:** Three lines above (line 546–547), `app.line_cache` is assigned `Some(…)` when `needs_rebuild` is true, and when `needs_rebuild` is false it was `Some` already (the `.map_or(true, …)` check).  So the `unwrap` *should* always see `Some`.

**Why it is still HIGH:**
- The invariant is maintained **structurally** (non-obvious) rather than by the type system.  
- Any future refactor that adds an early-return, replaces the rebuild block, or touches the `needs_rebuild` flag path silently makes this `.unwrap()` a runtime panic.  
- `build_render_model` runs on the *main* tokio task, not the render thread — so a panic here would take down the entire async runtime rather than just the TUI rendering.  That is arguably worse than a render-thread panic.

**Fix:** Replace with `expect` carrying a diagnostic, or restructure as:

```rust
let all_lines_vec = match &app.line_cache {
    Some((_, lines)) => lines.as_slice(),
    None => {
        tracing::error!("line_cache missing after rebuild — skipping frame");
        return None;
    }
};
```

---

#### H2 — `render_thread.rs:275` — `Ordering::Relaxed` for `exit_done` reset breaks the Release/Acquire pair
**File:** `crates/agent-tui/src/tui/render_thread.rs:275`

```rust
exit_done.store(false, Ordering::Relaxed);   // ← render thread, SpawnExitFx handler
// ...
exit_done.store(true, Ordering::Release);    // render_thread.rs:328
// ...
if exit_done.load(Ordering::Acquire) {       // mod.rs:520 — main task
```

**Problem:** The `store(false, Relaxed)` at line 275 is the *reset* that fires when the render thread receives `SpawnExitFx`.  It is racing with the `load(Acquire)` on the main task.

The formal requirement for an acquire-release pair is:
- **Writer (render):** must use at least `Release` for the `store` that the reader synchronises on.
- **Reader (main):** must use at least `Acquire`.

The `store(false, Relaxed)` does **not** form a synchronisation point.  The main task might observe the `false` write out-of-order relative to the `SpawnExitFx` cmd being processed, meaning it could (in theory, on a weakly-ordered architecture) see `exit_done == false` *before* the effect even starts, or see a stale `true` *after* the reset because the processor reordered them.

The `store(true, Release)` at line 328 is correct.  Only the reset needs fixing:

```rust
// render_thread.rs:275
exit_done.store(false, Ordering::Release);
```

On x86 this won't manifest (TSO is strong), but on ARM (many CI containers, Apple Silicon) it is a real race.

---

#### H3 — `render_thread.rs:Drop` — `cmd_tx` is NOT dropped before `join_handle`, so the render thread parks forever on panic-drop path
**File:** `crates/agent-tui/src/tui/render_thread.rs:177-184`

```rust
impl Drop for RenderHandle {
    fn drop(&mut self) {
        // If teardown() was not called (e.g. panic path), drop cmd_tx so the
        // render thread's cmd_rx disconnects and it can exit its loop.
        // join_handle is dropped here...
        drop(self.join_handle.take());
    }
}
```

**Problem:** The comment says "drop `cmd_tx` so the render thread's cmd_rx disconnects" — but `cmd_tx` is **not dropped here**.  It is a plain field on `RenderHandle` (`cmd_tx: mpsc::Sender<RenderCmd>`), so it will be dropped *after* this `Drop` impl runs, during the automatic field-drop pass.  But `join_handle` has already been dropped first.

When `join_handle` is dropped (`std::thread::JoinHandle` drop = detach), the render thread is still running, parked, waiting for `cmd_rx`.  The `cmd_tx` drop then disconnects the channel and unblocks `try_recv` — *but* `park()` was called before `try_recv`, so the thread is sleeping and won't wake to observe the disconnect until an unpark arrives.  **No unpark is sent from Drop.**  The render thread will stay parked until the OS destroys it on process exit.

This means: on any panic path that causes `RenderHandle` to drop without an explicit `teardown()`, the render thread is permanently parked and the terminal is never restored.

**Fix:**

```rust
impl Drop for RenderHandle {
    fn drop(&mut self) {
        // IMPORTANT: drop cmd_tx FIRST so cmd_rx disconnects; then unpark
        // the render thread so it wakes up and observes TryRecvError::Disconnected.
        drop(self.cmd_tx);           // explicit field move-out not possible in Drop
        // ↑ Can't move out in Drop. The real fix is to wrap cmd_tx in Option<>:
        // if let Some(tx) = self.cmd_tx.take() { drop(tx); }
        self.slot.render_thread.unpark();
        drop(self.join_handle.take());
    }
}
```

Since you can't move out of `self` in `Drop`, the cleanest fix is to wrap `cmd_tx` in `Option<mpsc::Sender<RenderCmd>>` (mirroring `join_handle`), so `Drop` can `take()` and drop it before the unpark:

```rust
pub(crate) struct RenderHandle {
    pub(crate) slot:   FrameSlot,
    pub(crate) cmd_tx: Option<mpsc::Sender<RenderCmd>>,   // wrap in Option
    join_handle:       Option<std::thread::JoinHandle<()>>,
}

impl Drop for RenderHandle {
    fn drop(&mut self) {
        drop(self.cmd_tx.take());           // disconnect cmd_rx
        self.slot.render_thread.unpark();   // wake the parked thread
        drop(self.join_handle.take());      // detach (don't block)
    }
}
```

All `send` call-sites change from `self.cmd_tx.send(…)` to `self.cmd_tx.as_ref().map(|tx| tx.send(…))`.

---

### MEDIUM

#### M1 — `render_thread.rs:298-299` — `terminal.clear().ok()` on error silently loses terminal corruption
**File:** `crates/agent-tui/src/tui/render_thread.rs:298-299`

```rust
if pending_clear {
    terminal.clear().ok();
    pending_clear = false;
}
```

`terminal.clear()` is an `io::Result`.  Swallowing the error here is safe-ish (we log errors from `render_frame` elsewhere), but if `clear()` fails it means the terminal is in a partially-reset state before the next frame render.  A `tracing::warn!` on the error path would at minimum surface the issue in logs during debugging.

```rust
if let Err(e) = terminal.clear() {
    tracing::warn!(err = %e, "render thread: terminal clear failed");
}
```

---

#### M2 — `render_thread.rs:339` — `terminal.show_cursor().ok()` in teardown silently drops cursor-restore failure
**File:** `crates/agent-tui/src/tui/render_thread.rs:339`

```rust
fn do_teardown(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    lifecycle::emergency_teardown_terminal();
    terminal.show_cursor().ok();
}
```

If the PTY is closed, `show_cursor()` will fail — that's fine and expected.  But if it fails for any *other* reason (e.g., the terminal backend is in a weird state before the PTY actually closes), the cursor stays hidden and the user's shell is left invisible.  At minimum, log the error:

```rust
if let Err(e) = terminal.show_cursor() {
    tracing::debug!(err = %e, "render thread: show_cursor failed during teardown (PTY likely closed)");
}
```

---

#### M3 — `mod.rs:583` — `subagent_registry().lock().unwrap()` — poison-propagating
**File:** `crates/agent-tui/src/tui/mod.rs:583`

```rust
let mut registry = runtime.subagent_registry().lock().unwrap();
```

`std::sync::Mutex::lock().unwrap()` panics if the mutex is **poisoned** — that is, if another thread panicked while holding the lock.  If any subagent thread panics (and they do run user-provided tool code), this `.unwrap()` will propagate the panic into the main event loop, killing the TUI.

**Fix:** Use `lock().unwrap_or_else(|e| e.into_inner())` (recover from poison) or `lock().expect(…)` with a better message, plus a note that poison recovery is intentional.

```rust
let mut registry = runtime.subagent_registry()
    .lock()
    .unwrap_or_else(|e| {
        tracing::warn!("subagent_registry mutex was poisoned — recovering");
        e.into_inner()
    });
```

---

#### M4 — `mod.rs:299` — `unreachable!()` inside a `tokio::select!` arm
**File:** `crates/agent-tui/src/tui/mod.rs:299`

```rust
sidecar_event = async {
    if app.sidecars.is_empty() {
        let _: () = std::future::pending().await;
        unreachable!()                              // ← line 299
    } else {
        // ...
    }
} => {
```

This is a valid Rust idiom for `select!` arms that must be structurally present but are gated by a precondition — `pending().await` never resolves, so `unreachable!()` literally cannot be reached at runtime.  The compiler is unable to prove this because `pending()` returns `Pending` forever, not `!`.

**Classification:** This is a **NIT** in terms of correctness (it truly cannot fire) but MEDIUM in terms of *reviewability* — it looks alarming to anyone who doesn't know the `pending().await; unreachable!()` idiom.  A comment explaining the idiom (or replacing it with `std::convert::Infallible` via a helper) would prevent future readers from being scared.

---

#### M5 — `mod.rs:1268` — `.unwrap()` after `.get_mut()` is a logical double-check that could race
**File:** `crates/agent-tui/src/tui/mod.rs:1262-1268`

```rust
if app.sidecars.contains_key(&target_pid) {
    let label = app.sidecars.get(&target_pid)
        .and_then(|s| s.display_name.as_deref())
        .unwrap_or("sidecar")
        .to_string();
    let v = app.sidecars.get_mut(&target_pid).unwrap();  // ← can this panic?
```

The `contains_key` check and the subsequent `get_mut` are **not atomic** — but since `app.sidecars` is a plain `HashMap` accessed only on the main task, there is no concurrent modification between the two calls.  The `unwrap()` is safe *today*, but only because of the single-threaded access pattern, not the type system.

**Fix:** Replace the double-lookup with a single `if let Some(v) = app.sidecars.get_mut(&target_pid)` to make the safety self-evident to future maintainers and avoid the `.contains_key` + `.get_mut` anti-pattern:

```rust
if let Some(v) = app.sidecars.get_mut(&target_pid) {
    let label = v.display_name.as_deref().unwrap_or("sidecar").to_string();
    // use v directly …
}
```

---

#### M6 — `mod.rs:1361` — `.unwrap()` on `sidecars.values().next()`
**File:** `crates/agent-tui/src/tui/mod.rs:1360-1361`

```rust
} else if app.sidecars.len() == 1 {
    app.sidecars.values().next().unwrap().status_line()
```

The `len() == 1` check and the `.next().unwrap()` are logically paired and safe *today*, but this is the same double-lookup anti-pattern.  Replace with:

```rust
} else if let Some((_, single)) = app.sidecars.iter().next().filter(|_| app.sidecars.len() == 1) {
    single.status_line()
```

Or more idiomatically:

```rust
} else if app.sidecars.len() == 1 {
    app.sidecars.values().next()
        .map(|s| s.status_line())
        .unwrap_or_else(|| "sidecar: unknown state".to_string())
```

---

### LOW

#### L1 — `render_thread.rs:228,232` — `expect()` calls in `spawn_render_thread` on the main task
**File:** `crates/agent-tui/src/tui/render_thread.rs:228,232`

```rust
let join_handle = std::thread::Builder::new()
    .name("agent-tui-render".to_string())
    .spawn(move || { … })
    .expect("failed to spawn render thread");           // line 228

let render_thread = thread_rx.recv()
    .expect("render thread failed to send its Thread handle");  // line 232
```

These `expect()` calls run on the **main tokio task** (not the render thread), so they would panic the tokio runtime rather than the render thread.  Thread spawn failure is extremely rare (OOM, hitting `RLIMIT_NPROC`), but the error message from `expect` is lost in a tokio runtime panic instead of being surfaced as a graceful `io::Error`.

**Recommendation:** Return `io::Result` from `spawn_render_thread` and propagate these as `io::Error::new(io::ErrorKind::Other, …)`, which `run()` already returns via `Result<()>`.

---

#### L2 — `signals.rs:125,154` — `expect()` calls in `spawn_shutdown_signal_task`
**File:** `crates/agent-tui/src/tui/signals.rs:125,154`

```rust
let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT])
    .expect("failed to register signal hooks");   // line 125
// …
std::thread::Builder::new()
    .name("signal-listener".into())
    .spawn(move || { … })
    .expect("failed to spawn signal-listener thread");  // line 154
```

Same pattern as L1. Both call-sites are in the `run()` bootstrap, where returning `Result<()>` is already the convention.  Signal registration failure is unlikely but not impossible (signal mask inherited from a weird parent process, sandbox restrictions).

---

#### L3 — `render_thread.rs:167` — `let _ = handle.join()` silently discards render thread panic
**File:** `crates/agent-tui/src/tui/render_thread.rs:167`

```rust
if let Some(handle) = self.join_handle.take() {
    let _ = handle.join();
}
```

`JoinHandle::join()` returns `Result<(), Box<dyn Any>>` — the `Err` variant means the thread panicked.  If the render thread panics after sending the ack, the join here silently throws that information away.  Add a log:

```rust
if let Err(e) = handle.join() {
    tracing::error!("render thread panicked during teardown: {:?}", e);
}
```

---

#### L4 — `mod.rs:437` — `compact_task.take().unwrap()` — spurious panic risk
**File:** `crates/agent-tui/src/tui/mod.rs:436-437`

```rust
if app.compact_task.as_ref().is_some_and(|t| t.is_finished()) {
    let handle = app.compact_task.take().unwrap();
```

The `is_some_and` check and `take().unwrap()` are safe today (single-threaded event loop).  Convert to a single `if let Some(handle) = app.compact_task.take()` with an `is_finished()` guard, or just chain:

```rust
if let Some(handle) = app.compact_task.take().filter(|t| t.is_finished()) {
    // …
}
```

This also eliminates the (tiny) risk that future restructuring breaks the `take().unwrap()` assumption.

---

#### L5 — `mod.rs:550,600` — `keybind_registry.read().expect("keybind registry poisoned")`
**File:** `crates/agent-tui/src/tui/mod.rs:550,600`

```rust
let kb_guard = keybind_registry.read().expect("keybind registry poisoned");
```

`std::sync::RwLock::read()` can return `Err` only if the write lock is **poisoned** (a writer panicked while holding it).  The `expect` message correctly names the cause.  This is acceptable, but (like M3) poison recovery is safer:

```rust
let kb_guard = keybind_registry.read()
    .unwrap_or_else(|e| e.into_inner());
```

If a plugin thread panics mid-write, the keybind registry content is at worst stale — panicking the event loop on the next keypress is disproportionate.

---

### NIT

#### N1 — `RenderModel`, `RenderHandle`, `RenderCmd`, `FrameSlot` — missing `Debug` derives
**Files:** `render_model.rs`, `render_thread.rs`

None of the new public-facing types derive `Debug`.  This makes tracing and test assertion messages opaque.  For types that contain `Effect` (which may not be `Debug`), use `#[derive(Debug)]` on the outer struct and `#[debug(skip)]` or a manual impl for the `Effect` fields.

For `RenderModel` specifically, a `Debug` impl that at minimum prints the struct name and non-`Effect` fields would be useful.

---

#### N2 — `render_thread.rs:158` — `teardown()` return value should be `#[must_use]`
**File:** `crates/agent-tui/src/tui/render_thread.rs:158`

```rust
pub(crate) fn teardown(mut self, timeout: std::time::Duration) -> bool {
```

Returns `bool` (acked vs. not acked).  Callers must check this to decide whether to call `emergency_teardown_terminal()`.  The existing call-site in `mod.rs` does check it — but future callers might not.  Add `#[must_use]`:

```rust
#[must_use = "callers must call emergency_teardown_terminal() if teardown returns false"]
pub(crate) fn teardown(mut self, timeout: std::time::Duration) -> bool {
```

---

#### N3 — `render_model.rs` — `RenderModel` is not `Debug`; projection types are `Clone` but not `Debug`
**File:** `crates/agent-tui/src/tui/render_model.rs`

`SidecarPillSnap`, `SubagentSnap`, `GhostHint`, `SecretPromptSnap` all derive `Clone` but not `Debug`.  Standard Rust practice: if you derive `Clone`, also derive `Debug` unless the type contains non-`Debug` fields.  None of these do.

---

#### N4 — `draw.rs:365-370` — `format_tool_name` uses `&str` return for `icon` but `String` for `display_name`
**File:** `crates/agent-tui/src/tui/draw.rs:363-385`

```rust
pub(crate) fn format_tool_name(tool_name: &str) -> (&'static str, String, Option<String>) {
```

`display_name` for the non-`ext__` branch is `tool_name.to_string()` — an allocation that immediately clones the input `&str`.  If `format_tool_name` returned `Cow<'_, str>` for `display_name`, the non-ext path would be allocation-free.  This function is called inside `render_lines()` which runs on every frame rebuild — the alloc is in the hot path.

```rust
pub(crate) fn format_tool_name(tool_name: &str) -> (&'static str, Cow<'_, str>, Option<String>) {
    if tool_name.starts_with("ext__") {
        // …
        ("\u{00bb}", Cow::Owned(tool), Some(server))
    } else {
        let icon = match tool_name { /* … */ };
        (icon, Cow::Borrowed(tool_name), None)
    }
}
```

---

#### N5 — `render_thread.rs:88-108` — `FrameSlot::new` is private but could be removed in favour of a direct construction
**File:** `crates/agent-tui/src/tui/render_thread.rs:95-100`

`FrameSlot::new` is only called once (in `spawn_render_thread`).  It doesn't add a meaningful abstraction barrier — `FrameSlot { inner, render_thread }` would be equally clear and keeps the struct construction local to the spawn function.  Minor; keep if you prefer the pattern.

---

#### N6 — `signals.rs` — non-Unix fallback calls `emergency_teardown_terminal()` on the tokio task
**File:** `crates/agent-tui/src/tui/signals.rs:164-167`

```rust
#[cfg(not(unix))]
pub(crate) fn spawn_shutdown_signal_task(…) -> SignalHandle {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        super::lifecycle::emergency_teardown_terminal();  // ← called from tokio task
```

The comment on the Unix path (line 140) explicitly warns: "do NOT call emergency_teardown_terminal() here — crossterm holds a parking_lot::Mutex during draw operations; taking it from a signal thread deadlocks."  The non-Unix path *does* call it from a tokio task — which, in the render-thread world, is now equally problematic: the render thread may be holding crossterm's lock while the tokio task tries to take it.  The non-Unix path should send the signal over the channel and let the event loop handle the teardown, just like the Unix path.

---

## Summary Table

| ID  | Severity | File                              | Line(s)   | Issue                                                                        |
|-----|----------|-----------------------------------|-----------|------------------------------------------------------------------------------|
| H1  | HIGH     | `draw.rs`                         | 549       | `.unwrap()` on `line_cache` — structurally guaranteed but fragile; panics main task |
| H2  | HIGH     | `render_thread.rs`                | 275       | `exit_done.store(false, Relaxed)` — should be `Release` to form correct pair with `load(Acquire)` |
| H3  | HIGH     | `render_thread.rs`                | 177–184   | `Drop` doesn't disconnect `cmd_tx` + unpark before dropping `join_handle` — render thread parks forever on panic-drop |
| M1  | MEDIUM   | `render_thread.rs`                | 298–299   | `terminal.clear().ok()` — error silently swallowed, no log                   |
| M2  | MEDIUM   | `render_thread.rs`                | 339       | `show_cursor().ok()` in teardown — cursor-restore failure silently swallowed |
| M3  | MEDIUM   | `mod.rs`                          | 583       | `subagent_registry().lock().unwrap()` — poison panics main event loop        |
| M4  | MEDIUM   | `mod.rs`                          | 299       | `unreachable!()` after `pending().await` — correct but needs a comment explaining the idiom |
| M5  | MEDIUM   | `mod.rs`                          | 1262–1268 | `contains_key` + `get_mut().unwrap()` double-lookup anti-pattern             |
| M6  | MEDIUM   | `mod.rs`                          | 1360–1361 | `len() == 1` + `.next().unwrap()` double-check                               |
| L1  | LOW      | `render_thread.rs`                | 228, 232  | `expect()` in `spawn_render_thread` — should return `io::Result`             |
| L2  | LOW      | `signals.rs`                      | 125, 154  | `expect()` in `spawn_shutdown_signal_task` — same pattern                    |
| L3  | LOW      | `render_thread.rs`                | 167       | `let _ = handle.join()` — render-thread panic silently discarded             |
| L4  | LOW      | `mod.rs`                          | 436–437   | `compact_task.take().unwrap()` — unnecessary double-check                    |
| L5  | LOW      | `mod.rs`                          | 550, 600  | `keybind_registry.read().expect(…)` — poison should be recovered, not propagated |
| N1  | NIT      | `render_model.rs`, `render_thread.rs` | —     | Missing `#[derive(Debug)]` on new types                                       |
| N2  | NIT      | `render_thread.rs`                | 158       | `teardown()` → `bool` missing `#[must_use]`                                  |
| N3  | NIT      | `render_model.rs`                 | 110–143   | Projection types derive `Clone` but not `Debug`                              |
| N4  | NIT      | `draw.rs`                         | 365–385   | `format_tool_name` allocates on every call; `Cow<'_, str>` would be free     |
| N5  | NIT      | `render_thread.rs`                | 95–100    | `FrameSlot::new` single-use private constructor                              |
| N6  | NIT      | `signals.rs`                      | 164–167   | Non-Unix fallback calls `emergency_teardown_terminal()` from tokio task — contradicts Unix-path warning |

---

## Unsafe / `Send` Audit

No `unsafe impl Send`, `transmute`, or raw pointer code was introduced in the reviewed files.

The `tachyonfx` `sendable` feature is implemented correctly in the upstream crate (`tachyonfx-0.9.3/src/features.rs`): it replaces `Rc<RefCell<T>>` with `Arc<Mutex<T>>` throughout the effect internals and gates the `Shader` trait on `ThreadSafetyMarker` (which requires `Send` when the feature is on).  There is no `unsafe impl Send` — the `Send`-ness flows from the concrete `Arc<Mutex<…>>` types, which are `Send + Sync` by the standard library.  **This is sound.**

The `Effect` type contains `Box<dyn Shader>`, and `Shader: ThreadSafetyMarker` which is `Shader: Send` when `sendable` is active.  `Box<dyn Shader + Send>` is `Send`.  Moving `Effect` values across the `mpsc::channel` into the render thread is therefore safe.

---

## Architecture Notes (non-blocking observations)

1. **Latest-wins slot correctness:** The `while let Some(model) = inner.lock().take()` pattern is correct.  Taking inside the lock and rendering outside it means the main task can publish a new frame concurrently — good.  The `while` (not `if`) is a minor point: since `lock().take()` always empties the slot, the inner loop body runs at most once per `park()` wakeup.  A plain `if let` would be more accurate and slightly clearer, though not wrong.

2. **`Line<'static>` bound satisfaction:** The `'static` is genuinely satisfied.  All `Line` values in `render_lines()` are built from `format!`-produced `String`s, string literals, and `Span::styled(owned_string, style)` calls.  The `into_iter().map(|s| Span::styled(s.to_string(), …))` pattern produces owned spans.  No references into `App` data are embedded.  The `Arc<[Line<'static>]>` snapshot in `RenderModel` is sound.

3. **Parking lot vs std Mutex consistency:** `parking_lot::Mutex` is used for the `FrameSlot` inner (correct — it's the hot publish path and parking_lot is both faster and deadlock-detecting in debug builds).  `std::sync::Mutex` is used for `secret_prompt_rx` and the subagent registry (owned by engine code that predates this PR).  Mixed use is acceptable here; there's no soundness issue.  If consistency is desired for the frame slot path, it's already the right choice.

4. **Teardown budget math:** `TEARDOWN_TIMEOUT_SECS - SAVE_TIMEOUT_SECS` gives the render-thread teardown budget.  The `.max(Duration::from_secs(2))` floor is a good defensive measure.  One observation: if `TEARDOWN_TIMEOUT_SECS == SAVE_TIMEOUT_SECS` (both are 2), the subtraction gives `0` and the floor of 2s kicks in correctly.  Fine.
