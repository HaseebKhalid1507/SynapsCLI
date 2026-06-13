# Silverhand — Concurrency & Race-Breaking Review

**Scope:** `crates/agent-tui/src/tui/render_thread.rs` + how `tui/mod.rs` drives it.
**Commit:** dev @ `17051f2` ("merge: A3 crate split + #116 render thread").
**Lens:** lost-wakeups, deadlocks, ordering, panic/teardown safety.

I broke this. The headline question — *can main publish a frame and the render thread
miss the wake and park forever?* — the answer is **no**. The publish-then-unpark
ordering is correct and `std::thread`'s unpark-token semaphore semantics mean
even the worst interleaving (token deposited milliseconds before park()) still
returns from park immediately. But there ARE three real holes worth fixing, and
one of them is a textbook "drop-without-shutdown leaves a terminal in raw mode"
sitting right in the new code.

---

## Findings, ranked

### 1. `RenderHandle::Drop` does NOT wake the parked render thread → HIGH
**File:** `crates/agent-tui/src/tui/render_thread.rs:177-185`

```rust
impl Drop for RenderHandle {
    fn drop(&mut self) {
        drop(self.join_handle.take());
    }
}
```

**Exploit path (panic unwind):**
1. Anything between `spawn_render_thread()` (mod.rs:143) and the explicit
   `render_handle.teardown(...)` call (mod.rs:1999) panics — say, the streaming
   handler, the gamba child reaper, an `unwrap()` in markdown layout, etc.
2. Stack unwind drops `render_handle`. Drop runs.
3. Drop *takes* the `JoinHandle` and drops it → the render thread is detached.
   `cmd_tx` is implicitly dropped after Drop returns → render thread's `cmd_rx`
   becomes `Disconnected`.
4. **But the render thread is asleep in `thread::park()` and nothing wakes it.**
   `mpsc::Receiver` disconnection does NOT wake a parked thread. The
   `try_recv()` that would observe `Disconnected` (line 287) is never reached.
5. The thread sleeps until the OS reaps it on process exit. It never runs
   `do_teardown()` → raw mode + alt screen + cursor-hide persist. The user's
   shell is wedged.

There's no `panic::set_hook` registered anywhere in the crate that calls
`emergency_teardown_terminal()` (I grepped). On the happy path everything is
fine because `teardown()` is always reached. On panic-unwind, terminal state
leaks. This is exactly the failure the bounded-teardown story was supposed to
*prevent*, dodged via a back-door.

**Repro:** insert `panic!("test")` anywhere in the event loop after spawn.
Observe the shell remains in raw mode after the binary exits.

**Fix (minimal):**
```rust
impl Drop for RenderHandle {
    fn drop(&mut self) {
        // Best-effort: tell the thread to tear down, then unpark it.
        let (ack_tx, _ack_rx) = mpsc::sync_channel(1);
        let _ = self.cmd_tx.send(RenderCmd::Teardown { ack: ack_tx });
        self.slot.render_thread.unpark();
        // Detach (don't join — Drop must be non-blocking).
        drop(self.join_handle.take());
    }
}
```
Even better: also install a `panic::set_hook` in `run_tui` that calls
`lifecycle::emergency_teardown_terminal()` directly, so the terminal is restored
*synchronously* during unwind without depending on the detached thread winning
a race.

---

### 2. Backstop `emergency_teardown_terminal()` on non-acked path can contend with crossterm's internal mutex → MEDIUM-HIGH
**File:** `crates/agent-tui/src/tui/mod.rs:2005`, cross-referenced
`crates/agent-tui/src/tui/signals.rs:140-142`.

The signal-listener comment is explicit:
> crossterm holds a `parking_lot::Mutex` (`TERMINAL_MODE_PRIOR_RAW_MODE`)
> during draw operations; taking it from a signal thread deadlocks.

The deadlock the comment describes is *same-thread reentrance*, but the lock
exists for cross-thread contention too. Scenario:

1. Render thread receives `Teardown`, enters `do_teardown()` →
   `emergency_teardown_terminal()` → `crossterm::terminal::disable_raw_mode()`
   acquires `TERMINAL_MODE_PRIOR_RAW_MODE`.
2. Wedge: the `write!` that follows is blocked on a dead PTY consumer (the
   exact case the design anticipates). Mutex is still held.
3. Main's `recv_timeout` expires → `acked = false` → falls through to
   `lifecycle::emergency_teardown_terminal()` on line 2005.
4. Main's call tries to take the same `parking_lot::Mutex`. It blocks
   forever (parking_lot has no built-in timeout on `lock()`). **Shutdown
   hangs** — defeating the whole point of the bounded teardown.

The window is narrow (only when the wedge happens specifically *during*
`disable_raw_mode`'s critical section, not during the bulk `write()`), but
it's reachable.

**Fix:** wrap the backstop in `try_lock_for` semantics, or skip it entirely
when not acked and rely solely on the OS process exit to flush. Or have the
render thread perform mode changes *before* the suspected-to-block writes
(reorder inside `lifecycle::emergency_teardown_terminal()`).

---

### 3. Render-thread panic during normal rendering is invisible to main → MEDIUM
**File:** `render_thread.rs:158-174` (`teardown` body) + thread body 246-333.

If `render_frame()` panics (markdown highlight overflow, ratatui buffer math,
tachyonfx shader bug), the render thread unwinds out of `render_thread_body`
without sending `ack`. Main sees:
- `recv_timeout` expiry → `acked = false`
- **`join_handle` is NEVER consumed**, even though the thread is now `Finished`
  and join would be instantaneous and would surface the panic payload.

The current code says (line 170) "If NOT acked, the render thread is wedged —
skip join". That's true *most* of the time but conflates "wedged" with
"panicked-and-already-dead". You silently lose the panic; only the warn log at
2001 fires. No `tracing::error!` with the panic message; no backtrace.

**Repro:** insert `panic!()` inside `render_frame` for one tick. Observe the
backstop emergency teardown runs but no panic info reaches the logs.

**Fix:** before the `acked == false` branch returns, do a non-blocking
`handle.is_finished()` (stable Rust 1.61+) check; if finished, join + log the
panic payload via `JoinHandle::join().unwrap_err()`.

---

### 4. `exit_done` reset uses `Relaxed`, read uses `Acquire` — inconsistent ordering → LOW/NIT
**File:** `render_thread.rs:275` vs `mod.rs:520`.

```rust
// render thread:
Ok(RenderCmd::SpawnExitFx { fx }) => {
    exit_fx = Some(fx);
    exit_done.store(false, Ordering::Relaxed);   // <-- Relaxed
}
// ...
exit_done.store(true, Ordering::Release);        // <-- Release
```

Main loads with `Acquire` and breaks the event loop on `true`. Within a single
atomic, the modification order is global, so this is *not* a data race. But
it's a footgun if exit-fx is ever sent twice in one session (currently it
isn't — but the API allows it). The `Relaxed` store of `false` is not ordered
against the surrounding stores; mix that with future code adding side-effect
state next to `exit_done` and you have a real visibility bug waiting.

Same nit for `boot_done` — it's never reset, so a hypothetical "reboot fx"
would observe a stale `true` immediately.

**Fix:** make both stores `Release`. Cost: nothing on x86, one fence on aarch64.

---

### 5. `Clear` between two frames produces a one-frame black flash → LOW
**File:** `render_thread.rs:296-300`.

Sequence: main publishes frame A → render thread renders A → main calls
`send_clear()` → render thread wakes, drains cmd (sets `pending_clear`),
applies `terminal.clear()`, drains slot (empty), loops back to park. The
screen is now blank until the next `publish`. On a slow tick (16ms) the user
sees a visible flash.

**Fix:** defer the `terminal.clear()` until *immediately before* the next
`render_frame()` (i.e., move the `if pending_clear` check inside the
`while let Some(model) = …` loop, just before `render_frame`). That coalesces
clear + redraw atomically from the user's POV.

---

## Things I tried to break and could NOT

These are clean:

- **Lost wakeup between publish and park.** The publish protocol
  (`*lock = Some; unpark()`) is ordered correctly against the consumer
  (`park(); drain;`). `unpark()` deposits a one-bit token; `park()` consumes
  it if present, sleeps otherwise. Every interleaving I walked produces
  either an immediate park-return or a properly-blocked park that gets woken.
  No lost wakeup.
- **Spurious park wakeup.** The outer `loop`/`while let` re-checks both the
  cmd channel (via `try_recv`) and the slot (via `take()`). Empty case →
  bottom of loop → re-park. Idempotent. ✓
- **Latest-wins drain (`while let Some = lock().take()`).** The lock is held
  only across `take()`; rendering happens outside the lock. Main can publish
  N times during one render and the consumer picks up only the last one. By
  design. ✓
- **Cmd channel + unpark race.** Sender side is unbounded `mpsc::Sender`
  (line 209 — `mpsc::channel()`), so `send` cannot block. Each cmd send is
  followed by `wake()`. Render thread always drains *all* pending cmds
  before rendering. Teardown is always observed. ✓
- **`teardown(self)` double-drop / use-after-move.** `self` is consumed by
  value; `join_handle.take()` is conditional on `acked`; Drop runs after and
  only drops what's left. No double-join, no second `Teardown` send. ✓
- **`parking_lot::Mutex` poisoning across panic.** parking_lot is poison-free;
  even if the render thread panics holding the slot lock, the lock is just
  released. No deadlock on the slot itself. ✓
- **`recv()` of `thread::current()` handle (line 232).** Bootstrap is
  serialised before any event-loop iteration. If the thread panics before
  sending its handle, `recv()` returns Err and `expect` aborts before any
  user state is created. Acceptable. ✓
- **Teardown ordering with pending Clear.** Cmd drain processes both in
  order; Teardown short-circuits with `return` so leftover `pending_clear`
  is harmlessly discarded. ✓

---

## Severity summary

| # | Issue | Severity |
|---|---|---|
| 1 | `RenderHandle::Drop` doesn't unpark → terminal in raw mode after panic | **HIGH** |
| 2 | Backstop `emergency_teardown_terminal()` can hang on crossterm mode mutex | **MEDIUM-HIGH** |
| 3 | Render-thread panic during render is silently swallowed by skip-join | **MEDIUM** |
| 4 | `exit_done`/`boot_done` ordering inconsistency (Relaxed reset) | **LOW/NIT** |
| 5 | `Clear` causes one-frame black flash between cmd and next publish | **LOW** |

No CRITICAL. The hot-path synchronisation (publish/park, drain, mailbox) is
**correct**. The issues are all in the *edges*: panic-unwind, render-thread
death, and one ordering footgun that doesn't bite today but will bite
tomorrow's contributor.

— Silverhand
