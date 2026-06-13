# Case — Lifecycle / Signals / Terminal Safety Review

Repo: `agent-runtime`  •  Branch: `dev` @ `17051f2`
Scope: post-#116 (commit `025c569` retired the signal watchdog).
Lens: process lifecycle, signal handling, terminal restoration on every exit path.

---

## TL;DR

The render-thread refactor is structurally sound and most of the watchdog's job
is now done by the bounded teardown budgets + the render thread's own
`do_teardown` + ack timeout. **But two real holes remain where the terminal
can be left in raw mode / alt screen, and one stale comment lies about a
backstop that no longer exists.** The save-timeout `exit(1)` path is OK
(it does call `emergency_teardown_terminal` first) but it skips the proper
render-thread teardown.

---

## Findings (severity-ranked)

### CRITICAL

#### C1. Panic in `run()` ⇒ render thread parked forever ⇒ terminal left in raw mode + alt screen
**File:** `crates/agent-tui/src/tui/render_thread.rs:177-185`
**Cross-ref:** `crates/agent-tui/src/tui/mod.rs:142-143` (terminal handed off before any panic-protected scope), and the entire `run()` body which has no `catch_unwind`/`set_hook`.

`RenderHandle::Drop` only takes/drops `join_handle`:

```rust
impl Drop for RenderHandle {
    fn drop(&mut self) {
        drop(self.join_handle.take());
    }
}
```

The doc-comment claims:

> "If teardown() was not called (e.g. panic path), drop cmd_tx so the render
>  thread's cmd_rx disconnects and it can exit its loop."

**This is wrong.** The render thread spends its life **parked**
(`render_thread.rs:265`: `std::thread::park()`). It only polls `cmd_rx.try_recv()`
**after** an `unpark()`. Dropping `cmd_tx` (or the `FrameSlot`'s shared `Arc`)
does **not** unpark a parked thread. So on a panic-unwind of the main task:

1. `RenderHandle::drop` runs — `cmd_tx` is dropped as a field after `Drop::drop`.
2. The render thread is still parked. Nothing wakes it.
3. The main thread/runtime returns out of `run()`; process exits.
4. The OS reaps the parked render thread without ever running `do_teardown`.
5. **Terminal is left in raw mode + alt screen + mouse capture + kbd-enhance.**

There is **no `panic::set_hook`** anywhere in the crate (`grep -rn set_hook
crates/` returns nothing). There is **no `catch_unwind`** around the event
loop. Any panic from any `select!` arm — `app.save_session()` futures, the
sidecar `select_all`, widget handlers, the giant input arm, ratatui itself
panicking on the main-thread `crossterm::terminal::size()` call — bypasses
all cleanup.

This is the classic "scary user-visible failure": the user's shell looks dead,
no echo, no line discipline, garbage from mouse-capture escapes.

**Fix sketch:**
- Install a `std::panic::set_hook` immediately after `setup_terminal()` that
  calls `lifecycle::emergency_teardown_terminal()` (safe: it only does
  `disable_raw_mode()` via ioctl + a best-effort `execute!` on stdout).
- Make `RenderHandle::Drop` also `unpark()` the render thread (store the
  `Thread` handle in `RenderHandle`, not just inside `FrameSlot`). With that
  + cmd_tx drop, the parked thread wakes, sees `Disconnected`, runs the
  existing `do_teardown(&mut terminal)` path (`render_thread.rs:287-292`).
  This is the path the code already pretends to take.

---

### HIGH

#### H1. Wedged-render fallback can hang on the stdout mutex
**File:** `crates/agent-tui/src/tui/mod.rs:1999-2006`

```rust
let acked = render_handle.teardown(teardown_budget);
if !acked {
    tracing::warn!("render thread did not ack teardown within budget — watchdog is backstop");
    lifecycle::emergency_teardown_terminal();
}
```

When the render thread is wedged in `terminal.draw()`'s `write()` syscall on a
slow-but-alive PTY consumer, it is **holding `io::stdout()`'s internal mutex**
(Rust's `Stdout` locks per-write). The fallback's `execute!(stdout, …)`
re-enters that mutex and blocks **forever**. The process never exits.

Mitigation that exists: `disable_raw_mode()` runs **first** in
`lifecycle.rs:60` and uses an ioctl on the tty fd directly (no stdout lock).
So raw mode does get cleared. **But alt screen, mouse capture, bracketed
paste, and kbd-enhance flags do not** — the `execute!` block deadlocks before
emitting their disable sequences. User ends up staring at a frozen
alt-screen with cursor hidden and mouse escapes still being interpreted.

This is the case that used to be backstopped by the watchdog `exit(1)`. It
is **no longer backstopped by anything.** The comment "watchdog is backstop"
on line 2001 is stale — there is no watchdog (see M1).

**Fix sketch:** if `!acked`, spawn a *short* `exit(1)` watchdog (e.g. 500ms)
after attempting `emergency_teardown_terminal`, so worst case is a
half-restored terminal instead of a hung process. The render thread being
wedged on a dead PTY almost always means the user can't see anything anyway,
so an immediate `exit(1)` after a best-effort cleanup is correct.

#### H2. No teardown on render-thread panic
**File:** `crates/agent-tui/src/tui/render_thread.rs:219-228, 246-333`

The render-thread body is a plain closure inside `thread::Builder::spawn`.
No `catch_unwind`. If `render_frame` panics (ratatui buffer overrun, theme
arithmetic, anything `unwrap`s in `draw.rs`), the thread unwinds, drops
`terminal` (no terminal restoration — `Drop` for `ratatui::Terminal` does
not leave alt screen / disable raw mode), and dies.

The main task then either:
- Publishes a frame → `unpark()` on a dead thread is a no-op; the slot just
  fills up. No deadlock.
- Eventually calls `render_handle.teardown(…)` → `cmd_tx.send` succeeds but
  is never consumed → `ack_rx.recv_timeout` exhausts the budget → falls into
  the H1 path (which itself may hang).

Net result: terminal left in raw mode + alt screen, possibly with the
process hung waiting on the (now meaningless) ack.

**Fix sketch:** wrap the body in `std::panic::catch_unwind` and call
`do_teardown(&mut terminal)` from the unwind arm before re-panicking (or
just exiting the thread). A panic hook (see C1) would also cover this.

---

### MEDIUM

#### M1. Stale "watchdog is backstop" comment misrepresents the safety net
**File:** `crates/agent-tui/src/tui/mod.rs:2001`

```rust
tracing::warn!("render thread did not ack teardown within budget — watchdog is backstop");
```

The watchdog was retired in `025c569`. There is no backstop. Either delete
the claim or, better, add the backstop the comment promises (see H1 fix).

This is technically a doc bug but I'm filing it MEDIUM because it is
actively misleading future maintainers and the absence of the backstop is
the H1 hole.

#### M2. Save-timeout `exit(1)` skips render-thread teardown
**File:** `crates/agent-tui/src/tui/mod.rs:1923-1930`

```rust
Err(_elapsed) => {
    tracing::warn!(budget_secs = signals::SAVE_TIMEOUT_SECS, "session save timed out — data may be incomplete");
    lifecycle::emergency_teardown_terminal();
    std::process::exit(1);
}
```

Good: calls `emergency_teardown_terminal` before exiting. ✓
Less good: the render thread is still alive at this moment, possibly
mid-`draw()`. Two concerns:

1. **Stdout-lock race**: main's `execute!` on stdout contends with the render
   thread's `write()`. Rust's `Stdout` mutex serialises them, so they don't
   interleave bytes, but if the render thread is currently holding the lock
   *and is wedged*, main hangs (same root cause as H1). On a healthy PTY
   it's a brief wait and harmless.
2. **No clean ack from render thread**: alt-screen leave / mouse-disable
   escapes from main may race the render thread's last `draw()` writes;
   in practice ordering ends up "main's disable-escape lands first or last,
   depending on lock timing." Crossterm's disable sequences are
   idempotent/visual-only so the final state is correct as long as main's
   escapes are emitted *at all*.

**Fix sketch:** before this `exit(1)`, do a best-effort
`render_handle.teardown(Duration::from_millis(500))` so the render thread
stops drawing, releases the stdout lock, and restores the terminal itself.
Then `emergency_teardown_terminal` + `exit(1)` as a belt-and-braces fallback.

#### M3. `RenderHandle::Drop` does not unpark — the doc comment lies
**File:** `crates/agent-tui/src/tui/render_thread.rs:177-185`

See C1 for the consequence. Filing separately at MEDIUM because the
*comment* is the lie (it states cmd_tx drop causes the thread to exit; it
doesn't, because the thread is parked). The code is consistent with itself
— it just doesn't do what it claims. Fix together with C1.

---

### LOW

#### L1. `boot_done` / `exit_done` are not signalled on render-thread death
**File:** `crates/agent-tui/src/tui/render_thread.rs:323-331`

If the render thread exits via `Disconnected` (current code path L287-292)
or panics (H2) while `exit_fx_sent == true` on the main side, `exit_done`
never flips. Main waits for `exit_done` at `mod.rs:520` and `break`s the
loop only on it. So a /quit during a render-thread crash hangs the loop
until something else (a signal) breaks it.

Not a terminal-leak risk per se, but a UX-cliff: /quit appears to hang.

**Fix sketch:** store both `Arc<AtomicBool>`s in `RenderHandle` so its
`Drop` can `exit_done.store(true)` + `boot_done.store(true)` to unblock the
event loop.

#### L2. Signal-listener thread cleanup is fine, but worth noting
**File:** `crates/agent-tui/src/tui/signals.rs:128-156`, `mod.rs:1978`

`shutdown_signal_task.close()` calls `signal_hook::iterator::Handle::close()`,
which causes `Signals::forever()` to return `None` and the thread to exit
cleanly. ✓ Not joined, but it's purely a blocking I/O thread and the OS
reaps it on exit. Fine. Documented this here so it isn't re-flagged later.

One nit: in the signal-listener thread, after sending on `tx`, the loop
`break`s after the first signal. If `tx.send` fails (receiver dropped — the
main task is gone), the signal is dropped silently and the thread exits.
That's intentional and correct now that there's no watchdog, but it means
**a SIGTERM that arrives after `shutdown_signal_rx` is dropped is
swallowed**. In practice the runtime is also being torn down at that point,
so it doesn't matter.

#### L3. PTY-already-closed path: render thread logs and keeps looping
**File:** `crates/agent-tui/src/tui/render_thread.rs:312-316`

```rust
if let Err(e) = render_frame(...) {
    tracing::warn!(err = %e, "render thread: terminal write failed — PTY likely closed");
    // Do NOT teardown here — stay alive so main's bounded-teardown
    // can send the Teardown command and get a clean exit sequence.
}
```

Correct choice (proactive teardown here would race main's bounded teardown).
But every subsequent frame from main re-enters this and re-logs. On a SIGHUP
storm where main is also being signalled, this is fine — main breaks out
fast. On a "PTY closed but no signal" scenario (does this exist? — only if
the parent terminal emulator closes the slave fd without SIGHUP, which is
unusual) the loop spins until something else happens. Probably fine.

---

### NIT

#### N1. `do_teardown` calls `terminal.show_cursor()` after `emergency_teardown_terminal()` has already left the alt screen
**File:** `crates/agent-tui/src/tui/render_thread.rs:337-342`

`emergency_teardown_terminal()` does `LeaveAlternateScreen`. Then we call
`terminal.show_cursor()`, which writes a `Show` cursor sequence to the
**primary** screen. That's fine — we *want* the cursor visible on the
primary screen — but it's a subtle order-dependence. If the order were
reversed, `show_cursor` would target the alt screen and the user would see
a hidden cursor in their shell. Current order is correct; add a one-line
comment so a future refactor doesn't flip it.

#### N2. `TEARDOWN_TIMEOUT_SECS` is computed but the actual render-thread budget uses `saturating_sub` + `.max(2s)` (mod.rs:1995-1998)
The constant is "single source of truth" per the signals.rs comment, but
the consumer fudges it. Minor — the `.max(2s)` floor is sensible.

---

## Per-path teardown matrix

| Exit path | Terminal restored? | Notes |
|---|---|---|
| `/quit` clean | ✅ | Animation → exit_done → bounded teardown → render_handle.teardown → ack → do_teardown |
| SIGINT / SIGTERM / SIGHUP | ✅ | select! → break → bounded teardown → render_handle.teardown ack path |
| save_session timeout (mod.rs:1928) | ✅ (with caveats — see M2) | `emergency_teardown_terminal()` runs before `exit(1)`. Stdout-lock contention with render thread is benign on healthy PTY. |
| Render-thread panic | ❌ | H2 — no catch_unwind, terminal dropped without restoration |
| Render-thread Disconnected (cmd_tx dropped) | ✅ *if* unparked | Code path exists at render_thread.rs:287-292; but the panic-in-main case (C1) never unparks → never reached |
| Main-task panic | ❌ | C1 — no panic hook, render thread parked forever |
| PTY closed mid-render, no signal | ⚠️ | L3 — render thread loops; cleanup happens when main does something |
| Render-thread wedged on slow PTY consumer | ⚠️ | H1 — raw mode disabled (ioctl), but alt screen / mouse / paste may not be (stdout-lock deadlock) |
| `process::exit` from anywhere else (e.g. extension panic in detached task) | ❌ | No `atexit`-style hook; out of scope of #116 but worth noting |

---

## Recommended minimum patch set

1. **(C1 + H2 + L1)** Install a `std::panic::set_hook` immediately after
   `setup_terminal()` that calls `lifecycle::emergency_teardown_terminal()`.
   This single fix covers main-task panic AND render-thread panic (since the
   process-wide hook fires on any thread's panic before its frames unwind).
2. **(C1 + M3)** Store the render thread's `Thread` handle in `RenderHandle`
   (in addition to inside `FrameSlot`) so `Drop` can `unpark()` it after
   dropping `cmd_tx`. Then the existing `Disconnected` arm at
   `render_thread.rs:287-292` actually runs on the panic path.
3. **(H1 + M1)** Replace the stale "watchdog is backstop" comment with a
   real 500ms `exit(1)` watchdog spawned only on the `!acked` branch. Same
   shape as the old signal watchdog, just narrower scope (post-teardown
   only, not for the whole shutdown budget).
4. **(M2)** Before the save-timeout `exit(1)`, do a best-effort
   `render_handle.teardown(Duration::from_millis(500))`. Cheap; usually
   completes well within budget and yields a cleaner terminal state.

With (1) and (2), the matrix above goes all-green for the "terminal restored?"
column except H1's wedged-PTY case, which (3) caps with a 500ms timer.

---

*The sky above the port was the color of television, tuned to a dead
channel. The terminal, you hope, is not.*
