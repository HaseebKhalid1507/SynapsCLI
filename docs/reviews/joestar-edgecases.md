# Edge-Case Review — #116 Render Thread
**Reviewer:** Joestar (subagent)  
**Branch:** `dev` @ `17051f2`  
**Scope:** `crates/agent-tui/src/tui/{render_thread.rs, render_model.rs, draw.rs, mod.rs}`  
**Mode:** Read-only, no builds or test runs.  

---

## Findings (severity-ranked)

---

### 1. `CRITICAL` — Render thread panic silently hangs `exit_done` forever

**File:** `render_thread.rs:262–332` (thread body loop) / `mod.rs:520`

**Trigger:** Any `panic!` originating inside `render_frame()` (e.g. from a tachyonfx `Shader::process()` call, a viewport out-of-bounds index, or any `unwrap()` inside the `terminal.draw()` closure) unwinds the render thread *without* sending the `Teardown` ack and *without* ever setting `exit_done = true`.

**What breaks:**
- The render thread's `JoinHandle` is stored in `RenderHandle::join_handle: Option<JoinHandle<()>>`. When the thread panics, Rust marks the `JoinHandle` as `Err`. Nobody on the main side polls that handle proactively — the main event loop only exits via `exit_done.load(Ordering::Acquire)` (line 520) or `break` on signal/PTY close. Neither path fires after a render thread panic.
- The `cmd_tx` side is still alive (main holds `render_handle`). `cmd_rx` on the thread side is gone (thread panicked). Sending `Teardown` to `cmd_tx` will silently succeed (mpsc channel buffers it), the ack never comes, and `teardown()` on line 1999 eventually times out.
- **Main does NOT hang** thanks to the bounded teardown budget — it times out and calls `lifecycle::emergency_teardown_terminal()`. BUT: the terminal is *left in raw mode with alternate screen* for the full teardown timeout duration (up to `TEARDOWN_TIMEOUT_SECS - SAVE_TIMEOUT_SECS`, floored to 2 s) before recovery. The user sees a frozen screen with no visible error.

**Deeper concern:** `terminal.draw(|frame| { … })` swallows the inner closure's panic *only* if `catch_unwind` is in play. Ratatui's `Terminal::draw` does **not** wrap the render closure in `catch_unwind` — the panic propagates up through `render_frame` and kills the thread.

**Mitigation missing:** There is no `std::panic::catch_unwind` guard around the `render_thread_body` loop, no `Arc<AtomicBool> render_alive` flag checked by main, and no watchdog polling the `JoinHandle`.

**Fix direction:**
```rust
// Wrap the entire body:
let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    render_thread_body(terminal, inner, cmd_rx, boot_done, exit_done_thread);
}));
if result.is_err() {
    lifecycle::emergency_teardown_terminal();
    // Optionally set a shared error flag for main.
}
```
Or: poll `join_handle.is_finished()` once per tick on the main side and treat it as a fatal signal.

---

### 2. `HIGH` — Gamba (`launch_gamba`) tears down terminal from main thread while render thread may be mid-frame

**File:** `gamba.rs:44–74` / `render_thread.rs:305–316` / `mod.rs:610–618`

**Trigger:** User types `/casino`. `CommandAction::LaunchGamba` calls `app.launch_gamba()`, which on the main (Tokio) thread directly calls:
```rust
crossterm::terminal::disable_raw_mode().ok();
crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen).ok();
```
These are raw TTY `ioctl`/escape-sequence writes to `stdout`.

The render thread **also owns `stdout`** (via `Terminal<CrosstermBackend<io::Stdout>>`). At the moment `launch_gamba()` runs, the render thread may be *inside* `scrub_crossterm_terminal_edges` or `terminal.draw()`, both of which write to the same `stdout` fd.

**What breaks:** Two threads simultaneously writing to `stdout` is a data race at the OS level (not UB in Rust because `Stdout` uses a mutex on some platforms, but crossterm's `QueueableCommand` / `execute!` macros use `Write::write_all` which on Linux bypasses the `Stdout` lock by calling `write(2)` directly on fd 1). The result is **interleaved escape sequences** — the casino child inherits a terminal in an undefined state. On most terminals this manifests as:
- Partial alternate-screen exit (smcup/rmcup torn)
- Cursor left hidden
- Raw mode disabled mid-draw, causing echoed keystrokes to appear in the casino child's output

**Note:** `build_render_model` returns `None` when `gamba_child.is_some()`, which *prevents new frames from being published*, but there is no synchronization to drain or halt the render thread's *in-flight* frame before `launch_gamba()` proceeds with teardown. The `FrameSlot` mailbox may have a model queued that the render thread has already taken (popped under the lock at line 305) and is currently rendering.

**No `send_clear` on success path:** After a successful `launch_gamba()`, `send_clear` is only sent on the *error* path (line 615). This means on `check_gamba_exited` / `reclaim_gamba` (lines 430–431, 1820–1821), a `Clear` is sent, but the *transition* into gamba doesn't tell the render thread to stop (it relies on `build_render_model` returning `None`). There is a window of one render cycle.

**Fix direction:** Before calling `launch_gamba()`, send a `RenderCmd::Teardown` (or a new `RenderCmd::Pause`/`Suspend`) and wait for an ack before touching stdout. Alternatively: route all terminal mode changes for gamba through the render thread's command channel.

---

### 3. `HIGH` — `crossterm::terminal::size()` returning `(0,0)` on PTY close propagates a zero-sized layout into the render thread

**File:** `mod.rs:202–204` (and repeated at lines 693, 1407, 1855, 1881)

**Trigger:** The PTY master closes or the terminal emulator is killed/resized to zero (e.g. during tmux pane destruction, or the kernel SIGHUP race). `crossterm::terminal::size()` returns `Err(_)`, which is mapped via `.unwrap_or_default()` to `ratatui::layout::Size { width: 0, height: 0 }`.

**What breaks:**
- `build_render_model` receives `term_size = Size { width: 0, height: 0 }`.
- `input_inner_width = term_size.width.saturating_sub(2)` → `0`.
- `msg_area_height = 0u16.saturating_sub(…).max(1)` → `1` (the `.max(1)` saves the height from going to zero, but `width` is still 0).
- `content_width = msg_area.width.saturating_sub(2)` → `0`.
- `app.input_wrap_info(0)` is called, which may behave oddly with zero-width wrapping.
- `protected_bottom_rows` is computed from subagent/input heights at zero width — may overflow the zero terminal height, resulting in `edge_scrub_area` returning `None` (benign) but also `visible_range = (0, 0)` meaning zero lines rendered.
- A zero-width `RenderModel` is published to the render thread. Inside `render_frame`, `terminal.draw()` → `autoresize()` → `backend.size()` may also return `(0,0)`, causing `Buffer::new(Rect { width:0, height:0 })`, which is valid but renders nothing. However `scrub_crossterm_terminal_edges` writes `MoveTo(0, y)` + `Print(" ")` for each row in an empty area — returns `Ok(())` immediately (correct). The layout in `render_frame` calls `Layout::split(frame.area())` with a zero-area frame — ratatui's layout engine handles this gracefully (splits to zero-sized rects). No panic, but a completely blank frame is dispatched.
- **Real danger:** Line 770 in `render_frame`: `let input_inner_width = frame.area().width.saturating_sub(2)` — safe. But the visible range `(start, end)` from the model snapshot was computed at zero width with a stale `content_height`. If `end > model.lines.len()`, the slice `model.lines[start..end]` would panic. It won't because `end = total.saturating_sub(scroll_back as usize)` and `start = end.saturating_sub(content_height)` — both are bounded by `total`, so slicing is safe.

**Net verdict:** No hard crash, but a zero-size `term_size` silently produces a degenerate frame. The real risk is that `msg_area_rect` and `visible_line_range` on `App` are written back with zero-width geometry that mouse-click coordinate mapping (in `input.rs`) will use for the *next* input event — mapping all mouse clicks to position `(0, 0)`.

**Fix direction:** Guard the publish: if `term_size.width == 0 || term_size.height == 0`, skip `build_render_model` entirely (same as the gamba gate). Add alongside the existing `size()` call:
```rust
if term_size.width == 0 || term_size.height == 0 { /* skip */ continue; }
```

---

### 4. `MEDIUM` — `help_find` scroll/cursor state is silently discarded every frame

**File:** `draw.rs:1591–1592` / `render_model.rs:96` / `help_find.rs:95`

**Trigger:** User opens the help-find modal and types a filter query or moves the cursor with arrow keys. This mutates `App::help_find: Option<HelpFindState>`.

`build_render_model` (line 678) clones `app.help_find` into `model.help_find`. Inside `render_frame`, the render closure at line 1591 does:
```rust
if let Some(ref mut state) = model.help_find.clone() {
    super::help_find::render(frame, frame.area(), state);
}
```
Note the **double indirection**: `model.help_find` is `Option<HelpFindState>`, and `.clone()` here creates a *third* copy just to satisfy the `ref mut` borrow needed by `help_find::render(&mut HelpFindState)`. The mutations `set_visible_height` (line 95 of `help_find.rs`) and the scroll-window computation inside `render` update **this throwaway clone only**. They are never written back.

**What breaks:**
- `state.set_visible_height(visible_height)` — sets the visible height on the local copy. The authoritative `App::help_find` retains the *old* `visible_height`. On the first render the default is `10` (from `HelpFindState::new`). If the actual modal height is larger or smaller, the scroll window calculation uses the stale height until the next user keystroke triggers a real `App`-side mutation.
- **Stale scroll window:** `visible_help_find_window` (line 107 of `help_find.rs`) uses `state.scroll()` — which is the cloned-and-then-mutated scroll from the render copy. Since the authoritative scroll never changes from render-side mutations, rapid arrow-key scrolling can exhibit a one-frame-behind scroll position if the main side hasn't processed the key yet.
- **This is intentional for correctness** (the comment in `render_model.rs:93–95` acknowledges "let render mutate its local copy") but the `set_visible_height` side-effect is *not* cosmetically harmless: if the modal is opened at a non-default terminal height, the first render uses `visible_height=10` regardless, potentially truncating the results list or showing an incorrect scroll indicator.

**Fix direction:** Either (a) set `visible_height` on the App-side state in `build_render_model` using the pre-computed `msg_area_height`; or (b) accept one stale frame and document it.

---

### 5. `MEDIUM` — Resize: layout computed by main with stale `crossterm::size()` disagrees with render thread's `frame.area()` for one frame

**File:** `render_thread.rs` module comment §5.4 / `mod.rs:202–204` / `draw.rs:770`

**Trigger:** User resizes the terminal. The OS delivers `SIGWINCH`. Crossterm's event stream delivers an `Event::Resize(w, h)` (consumed by `input::handle_event`, which sets `app.needs_redraw = true`). The main side then calls `crossterm::terminal::size()` — this is a `TIOCGWINSZ` ioctl on the TTY fd, which *always returns the current kernel-cached size*, not the ratatui terminal's buffered size. So far so good.

However, the render thread calls `terminal.draw()` → `autoresize()` → `backend.size()` (same ioctl) independently. There is **no synchronization** between main's `size()` call and the render thread's `autoresize()` call.

**Race window:** Main reads size `(W1, H1)` → builds `RenderModel` with layout computed at `(W1, H1)` → publishes. Meanwhile a *second* resize arrives: kernel size is now `(W2, H2)`. Render thread calls `autoresize()` → sees `(W2, H2)` → resizes its buffers to `(W2, H2)` → renders the model (which was laid out at `(W1, H1)`) into a `(W2, H2)` frame. The layout constraints in `render_frame` re-run against `frame.area()` which is `(W2, H2)`, so **widgets are re-laid-out at the new size** but **content (visible_range, msg_inner_rect, protected_bottom_rows)** in the snapshot was computed at `(W1, H1)`.

**What breaks:**
- `visible_range = (start, end)` was computed with `content_height` from the old terminal height. If the new height is larger, the render thread renders fewer lines than fit. If smaller, `end` could exceed `model.lines.len()` — *except* `end = total.saturating_sub(scroll_back)` which is bounded by `total`, so no panic. Just visually: wrong number of lines shown for one frame.
- `protected_bottom_rows` in the model vs the real `frame.area().height` — `scrub_crossterm_terminal_edges` uses the render thread's own `terminal.size()` (line 94 of `viewport.rs`) so the scrub area is always correct. No torn scrub.
- **Torn layout:** The model's `msg_inner_rect` written back to `App` reflects old geometry. The next mouse event on the main side uses this stale rect for hit-testing, potentially off by several rows.

**Acknowledged in spec:** The module comment at `render_thread.rs:45` explicitly notes "Worst-case: one stale frame on resize." This is correct and acceptable for most use cases. The `msg_inner_rect` staleness for mouse events is the real sting — it persists until the next `build_render_model` call.

**Fix direction:** After a resize event, clear `app.msg_area_rect = None` to force input.rs to reject stale coordinates on the next mouse event.

---

### 6. `MEDIUM` — Render thread keeps looping (and logging) on every frame after PTY close

**File:** `render_thread.rs:305–317`

**Trigger:** PTY master closed (SSH disconnect, `tmux kill-pane`, terminal emulator quit without `/quit`). `render_frame()` returns `Err(io::Error { kind: BrokenPipe, … })`.

**What happens:**
```rust
if let Err(e) = render_frame(…) {
    tracing::warn!(…, "render thread: terminal write failed — PTY likely closed");
    // Do NOT teardown here …
}
```
The render thread **stays alive** and re-parks. If the main side has any live tick source (streaming, boot_fx, etc.) it will keep publishing frames every 16 ms. The render thread will keep waking, calling `render_frame`, getting `BrokenPipe`, logging a warning, and looping — potentially **thousands of times** before main notices and breaks its own loop.

**Main notices via:** `event_reader.next()` returning `Some(Err(_)) | None` (line 1796 of mod.rs — `// (draw() I/O error → break)`). The comment there says "draw() I/O error" but it actually refers to crossterm event-stream I/O error, which does fire on PTY close. So main *will* break — but only on the next crossterm event, which requires another event loop iteration.

**Gap:** Between PTY close and main's event loop processing `None` from the event stream, the tick arm fires at 16 ms intervals. Each tick publishes a new frame. The render thread gets each frame, tries to write, gets `BrokenPipe`, logs a warning, and loops. On a fast machine this can be **hundreds of warn! log entries** in ~1 second.

**No data corruption** — `BrokenPipe` is handled gracefully. The main teardown path with `render_handle.teardown(budget)` then works correctly (render thread is alive, processes `Teardown`, calls `do_teardown`, acks).

**Fix direction:** Add a consecutive-error counter. After N (e.g. 3) consecutive `render_frame` errors, set a local `pty_dead` flag and skip frame rendering (just drain commands and park). This avoids the log spam while preserving the design invariant of "stay alive for Teardown."

---

### 7. `MEDIUM` — `gamba.restore_terminal()` does NOT send `Clear` to the render thread

**File:** `gamba.rs:34–40` / `mod.rs:430–431`

**Trigger:** `check_gamba_exited()` is polled in the tick arm. When the casino process exits on its own (user quits the casino), `check_gamba_exited` calls `self.restore_terminal()` internally and returns `Some(msg)`. The caller at `mod.rs:430` then calls `render_handle.send_clear()`.

However, `reclaim_gamba()` (called at lines 1820 and 1833 via a different code path — appears to be the `/reclaim` slash-command) *also* calls `self.restore_terminal()` internally. Both callers do send `render_handle.send_clear()` afterwards. ✓ Those paths look correct.

**The real gap:** `restore_terminal()` calls `crossterm::execute!(stdout, EnterAlternateScreen)` — which writes the smcup sequence to `stdout` directly from the main thread. The render thread may be awake and writing to the same `stdout` concurrently (same fd-1 race described in finding #2 above, but in the *reverse direction* — now main writes while the render thread renders). If the render thread is mid-draw when `restore_terminal()` runs, the smcup sequence is interleaved with ratatui's diff output, leaving the terminal in an inconsistent state.

**Fix direction:** Same as #2 — all raw terminal I/O must go through the render thread's command channel. Add a `RenderCmd::RestoreAfterGamba` that the render thread handles by writing `EnterAlternateScreen` + `enable_raw_mode` before clearing and resuming normal rendering.

---

### 8. `LOW` — `exit_fx` timing: `exit_done` set to `false` in `SpawnExitFx` arm risks a missed-completion window

**File:** `render_thread.rs:274–276`

**Trigger:** `SpawnExitFx` command arrives:
```rust
Ok(RenderCmd::SpawnExitFx { fx }) => {
    exit_fx = Some(fx);
    exit_done.store(false, Ordering::Relaxed);
}
```
The store is `Relaxed`. The main side reads `exit_done.load(Ordering::Acquire)` (mod.rs:520). A `Relaxed` store is visible to an `Acquire` load on the same location *eventually*, but not necessarily before the load. In practice on x86 this is fine (TSO), but on ARM the store could be reordered past the subsequent `exit_fx` processing — the main could read `exit_done=true` (from a previous run) between the `SpawnExitFx` processing and the `store(false)`. This would cause the main loop to `break` immediately, before the exit animation completes.

**Probability:** Extremely low in practice — there's a natural delay between `send_exit_fx` on main and the render thread processing it. But the ordering is formally incorrect.

**Fix direction:** Change to `store(false, Ordering::Release)` on line 275 (matching the `store(true, Ordering::Release)` on line 328 for symmetry).

---

### 9. `LOW` — `boot_fx` animation timing: first frame elapsed reads garbage duration

**File:** `render_thread.rs:256–257, 753–754`

**Trigger:** `last_frame` is initialized to `Instant::now()` at thread spawn (line 256). The render thread then calls `std::thread::park()` and waits for the first frame. On a slow startup (extension discovery, auth, large session restore), this wait can be **seconds**. When the first frame arrives, `render_frame` computes:
```rust
let elapsed = last_frame.elapsed(); // e.g. 3.2 seconds
*last_frame = std::time::Instant::now();
```
This `elapsed` is passed to tachyonfx's `Effect::process()` as the frame duration. Most effects clamp or saturate their time parameter, but if the boot effect's total duration is 750 ms and the first `elapsed` is 3200 ms, tachyonfx will fast-forward the effect through all its keyframes in a single frame — the boot animation is **skipped entirely**, jumping to its final state with no visible animation.

**Observed symptom:** Boot sweep-in animation appears to complete instantly (or not at all) on slow machines or when `--continue` loads a large session.

**Fix direction:** Reset `last_frame = Instant::now()` immediately *before* calling `render_frame` (not at thread spawn), or cap `elapsed` to the animation's own expected frame duration:
```rust
let elapsed = last_frame.elapsed().min(std::time::Duration::from_millis(50));
```

---

### 10. `LOW` — First frame before any `publish()`: render thread parks correctly, no issue; but `App::msg_area_rect = None` is used for mouse hit-testing

**File:** `mod.rs:197–213` / `render_model.rs:55`

**Trigger:** Between process start and the first `build_render_model` → `publish()` call, `app.msg_area_rect` is `None`. If a mouse event arrives before the first frame is drawn (fast clicker, accessibility tools, automated tests), `input.rs` would read `None` from `app.msg_area_rect` and skip the click — no crash, just silently ignored. The `FrameSlot` inner `Option<Arc<RenderModel>>` is `None` at startup, so the render thread's `while let Some(model) = inner.lock().take()` inner loop correctly does nothing on the first park/wake cycle if no frame has been published yet.

**Severity:** Benign — the `None` case is handled gracefully. Noting for completeness.

---

### 11. `NIT` — `ref mut state` clone in render is dead code / confusing

**File:** `draw.rs:1591`

```rust
if let Some(ref mut state) = model.help_find.clone() {
```

`model.help_find` is `Option<HelpFindState>` (not `Arc`). `.clone()` creates a temporary `Option<HelpFindState>`, then `ref mut state` borrows it. The clone is immediately discarded after `render` returns. This compiles and works but the `clone()` is redundant — `model.help_find` is already owned. The idiom should be:

```rust
if let Some(ref mut state) = model.help_find {
```

Since `model` is `&RenderModel` (immutable borrow inside the `terminal.draw` closure), `model.help_find` is `&Option<HelpFindState>`. Obtaining `&mut HelpFindState` requires a local copy anyway. The cleanest form:

```rust
if let Some(mut state) = model.help_find.clone() {
    super::help_find::render(frame, frame.area(), &mut state);
}
```

No semantic difference, but the current form is misleading — it looks like the mutation might persist.

---

## Summary Table

| # | Severity | File:Line(s) | Issue |
|---|----------|-------------|-------|
| 1 | CRITICAL | `render_thread.rs:262–332`, `mod.rs:520` | Render thread panic → `exit_done` never set, terminal stuck in raw/alt-screen for full teardown timeout |
| 2 | HIGH | `gamba.rs:44–57`, `render_thread.rs:305–316` | `launch_gamba()` writes raw escape sequences to stdout from main thread while render thread may be mid-draw |
| 3 | HIGH | `mod.rs:202–204` (×4 sites) | `size() → unwrap_or_default()` silently produces zero-width `RenderModel`; corrupts `msg_area_rect` for mouse hit-testing |
| 4 | MEDIUM | `draw.rs:1591–1592`, `help_find.rs:95` | `set_visible_height` mutates a throwaway clone — authoritative `HelpFindState` always has stale `visible_height=10` |
| 5 | MEDIUM | `render_thread.rs:45`, `mod.rs:202–204`, `draw.rs:770` | One-frame layout tear on rapid resize: `msg_inner_rect` written at old size, used by mouse hit-testing until next frame |
| 6 | MEDIUM | `render_thread.rs:313`, `mod.rs:1796` | PTY close → render thread logs hundreds of `BrokenPipe` warnings before main breaks its event loop |
| 7 | MEDIUM | `gamba.rs:34–40`, `mod.rs:430` | `restore_terminal()` writes `EnterAlternateScreen` to stdout from main thread; same fd-1 race as #2 on casino exit |
| 8 | LOW | `render_thread.rs:275` | `exit_done.store(false, Relaxed)` — should be `Release` for ARM correctness; main reads with `Acquire` |
| 9 | LOW | `render_thread.rs:256–257`, `draw.rs:753–754` | First tachyonfx frame gets multi-second `elapsed` from thread-spawn delay; boot animation skips on slow startup |
| 10 | LOW | `mod.rs:197–213` | First frame before publish: `msg_area_rect = None`, mouse clicks silently dropped — benign, handled |
| 11 | NIT | `draw.rs:1591` | `model.help_find.clone()` with `ref mut` is misleading; clone is discarded, mutation does not persist |

---

## 5-Line Summary

1. **CRITICAL (#1):** A panic inside `render_frame` kills the render thread with no recovery signal — `exit_done` never fires, the terminal stays frozen in raw/alt-screen for the full teardown timeout; needs `catch_unwind` or `JoinHandle` polling.
2. **HIGH (#2 & #7):** `launch_gamba()` and `restore_terminal()` write raw escape sequences to `stdout` from the main thread concurrently with the render thread's own `stdout` writes — interleaved sequences corrupt terminal state; all TTY mode changes must be routed through `RenderCmd`.
3. **HIGH (#3):** `crossterm::terminal::size()` failure silently produces a zero-width `RenderModel` via `unwrap_or_default()`, corrupting `msg_area_rect` and mouse coordinates; should gate on `width == 0 || height == 0`.
4. **MEDIUM (#4 & #5):** `help_find::render` mutates a throwaway clone, so `set_visible_height` never persists and the scroll window uses the stale default height; separately, resize produces one torn frame where the snapshot's `visible_range`/`msg_inner_rect` reflect the old size while the render thread layouts against the new one.
5. **LOW (#8 & #9):** `exit_done.store(false, Relaxed)` should be `Release` (ARM ordering); and the first tachyonfx frame clock reads the full thread-park delay as elapsed time, fast-forwarding and invisibilizing the boot animation on slow startup.
