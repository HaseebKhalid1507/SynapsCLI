//! Dedicated render `std::thread` for the TUI.
//!
//! ## Design (spec §5)
//!
//! The render thread owns the `Terminal<CrosstermBackend<Stdout>>` end-to-end,
//! including final teardown.  The main tokio task never touches the terminal
//! after handing it off at startup.
//!
//! ### Publish protocol (latest-wins mailbox)
//!
//! A `FrameSlot` wraps `Arc<parking_lot::Mutex<Option<Arc<RenderModel>>>>`.
//! The main task calls `FrameSlot::publish(model)` which:
//!
//! 1. Locks the inner mutex and stores the new model (replacing any unread
//!    frame — latest-wins).
//! 2. Calls `Thread::unpark()` on the render thread to wake it.
//!
//! The render thread loops:
//!
//! ```text
//! loop {
//!     thread::park();                       // sleep until a frame arrives
//!     // drain commands …
//!     while let Some(model) = inner.lock().take() { render_frame(…); }
//!     if disconnected { break; }
//! }
//! ```
//!
//! Spurious wakeups from `park()` are safe — the inner `while let` re-checks
//! the slot; if it's empty the inner loop exits and we re-park.
//!
//! ### Sideband command channel
//!
//! A `std::sync::mpsc::Sender<RenderCmd>` carries out-of-band commands.
//! Currently `Teardown { ack }`, `SpawnBootFx`, and `SpawnExitFx` are
//! defined.  Main sends `Teardown`, then waits on the ack `Receiver<()>`
//! inside the existing bounded-teardown budget.  The render thread performs
//! the full terminal cleanup sequence, then sends `()` on the ack channel.
//!
//! ### Terminal size on the main side (spec §5.4, choice documented here)
//!
//! **We call `crossterm::terminal::size()` directly on the main side.**
//! The spec lists this as the simplest option — it reads the TTY fd directly
//! and does not need the `Terminal` object.  No shared `AtomicU16` needed.
//! Worst-case: one stale frame on resize (ratatui re-layouts on the next
//! frame using the terminal's actual size anyway).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tachyonfx::{Effect, Shader};

use super::draw::render_frame;
use super::lifecycle;
use super::render_model::RenderModel;

// ── Public command enum ───────────────────────────────────────────────────────

/// Out-of-band commands sent from the main task to the render thread via the
/// sideband `mpsc` channel.
pub(crate) enum RenderCmd {
    /// Request clean teardown.  The render thread will restore the terminal,
    /// send `()` on `ack`, then exit its loop.  Main waits on `ack` inside
    /// the existing bounded-teardown budget.
    Teardown { ack: mpsc::SyncSender<()> },

    /// Spawn the boot (entry) tachyonfx effect.  Sent once at startup.
    SpawnBootFx { fx: Effect },

    /// Spawn the exit tachyonfx effect.  Sent when the user invokes `/quit`.
    SpawnExitFx { fx: Effect },

    /// Force a full terminal clear on the next render pass.  Sent after gamba
    /// exits and similar full-screen takeover events where ratatui's diff
    /// cannot know the screen is dirty.
    Clear,
}

// ── Latest-wins frame slot ────────────────────────────────────────────────────

/// Single-slot latest-wins mailbox.  Shared between the main task (publisher)
/// and the render thread (consumer).
#[derive(Clone)]
pub(crate) struct FrameSlot {
    inner:         Arc<parking_lot::Mutex<Option<Arc<RenderModel>>>>,
    render_thread: std::thread::Thread,
}

impl FrameSlot {
    fn new(
        inner:         Arc<parking_lot::Mutex<Option<Arc<RenderModel>>>>,
        render_thread: std::thread::Thread,
    ) -> Self {
        FrameSlot { inner, render_thread }
    }

    /// Publish a new frame snapshot.  Replaces any unread frame (latest-wins)
    /// and wakes the render thread via `unpark()`.
    pub(crate) fn publish(&self, model: Arc<RenderModel>) {
        *self.inner.lock() = Some(model);
        self.render_thread.unpark();
    }
}

// ── Public handle returned to the caller ─────────────────────────────────────

/// Handle to the render thread, held by the main task.
pub(crate) struct RenderHandle {
    pub(crate) slot:   FrameSlot,
    pub(crate) cmd_tx: mpsc::Sender<RenderCmd>,
    join_handle:       Option<std::thread::JoinHandle<()>>,
}

impl RenderHandle {
    /// Wake the render thread (in addition to an unpark from publish).
    /// Used after sending a command so the thread wakes and processes it.
    fn wake(&self) {
        self.slot.render_thread.unpark();
    }

    /// Send the `SpawnBootFx` command (best-effort; ignore if thread is gone).
    pub(crate) fn send_boot_fx(&self, fx: Effect) {
        let _ = self.cmd_tx.send(RenderCmd::SpawnBootFx { fx });
        self.wake();
    }

    /// Send the `SpawnExitFx` command (best-effort).
    pub(crate) fn send_exit_fx(&self, fx: Effect) {
        let _ = self.cmd_tx.send(RenderCmd::SpawnExitFx { fx });
        self.wake();
    }

    /// Send a `Clear` command so the render thread calls `terminal.clear()`
    /// before its next render pass.  Used after full-screen takeover events
    /// (gamba exit, etc.) where ratatui's diff does not know the screen is
    /// dirty.
    pub(crate) fn send_clear(&self) {
        let _ = self.cmd_tx.send(RenderCmd::Clear);
        self.wake();
    }

    /// Perform a clean, bounded teardown of the render thread.
    ///
    /// Sends `Teardown`, waits for the ack (up to `timeout`).  If the ack
    /// arrives the thread is exiting cleanly, so we join it (quick).  If it
    /// does NOT arrive within the budget the thread is wedged — almost always
    /// because its own teardown `write()` is blocked on a dead PTY consumer —
    /// so we deliberately do **not** join (that would hang the process on
    /// exit).  The wedged thread is reaped when the process exits.  This
    /// self-bounding behaviour is what replaced the old signal watchdog (#116).
    ///
    /// Returns `true` if the ack arrived within the timeout.
    pub(crate) fn teardown(mut self, timeout: std::time::Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::sync_channel::<()>(1);
        let _ = self.cmd_tx.send(RenderCmd::Teardown { ack: ack_tx });
        self.wake();
        let acked = ack_rx.recv_timeout(timeout).is_ok();
        if acked {
            if let Some(handle) = self.join_handle.take() {
                // Acked → the thread restored the terminal and is returning;
                // join is quick.
                let _ = handle.join();
            }
        }
        // If NOT acked, the render thread is wedged (e.g. blocked on a dead
        // PTY). Skip the join — blocking here would hang shutdown forever.
        // Dropping the handle detaches the thread; it dies on process exit.
        acked
    }
}

impl Drop for RenderHandle {
    fn drop(&mut self) {
        // If teardown() was not called (e.g. panic path), drop cmd_tx so the
        // render thread's cmd_rx disconnects and it can exit its loop.
        // join_handle is dropped here; if the thread is still running it
        // becomes a detached thread — the OS cleans it up on process exit.
        drop(self.join_handle.take());
    }
}

// ── Thread spawn ─────────────────────────────────────────────────────────────

/// Spawn the dedicated render `std::thread`.
///
/// Moves `terminal` into the thread — the main task must NOT use `terminal`
/// after this call.
///
/// Returns:
/// - A [`RenderHandle`] for publishing frames and sending teardown.
/// - An `Arc<AtomicBool>` (`boot_done`) that becomes `true` when the boot
///   effect finishes — the main task clears its `boot_fx_sent` guard on this.
/// - An `Arc<AtomicBool>` (`exit_done`) that becomes `true` when the exit
///   effect finishes — the main task breaks the event loop on this.
pub(crate) fn spawn_render_thread(
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
) -> (RenderHandle, Arc<AtomicBool>, Arc<AtomicBool>) {
    // Shared inner slot — same Arc goes into the FrameSlot (main) and the
    // thread closure (render side).  No circular dependency.
    let inner: Arc<parking_lot::Mutex<Option<Arc<RenderModel>>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let inner_thread = Arc::clone(&inner);

    let (cmd_tx, cmd_rx) = mpsc::channel::<RenderCmd>();
    let boot_done = Arc::new(AtomicBool::new(false));
    let boot_done_thread = Arc::clone(&boot_done);
    let exit_done = Arc::new(AtomicBool::new(false));
    let exit_done_thread = Arc::clone(&exit_done);

    // Oneshot to receive the thread's own `Thread` handle so we can build
    // the FrameSlot and call `unpark()` later.
    let (thread_tx, thread_rx) = mpsc::sync_channel::<std::thread::Thread>(1);

    let join_handle = std::thread::Builder::new()
        .name("agent-tui-render".to_string())
        .spawn(move || {
            // Send our Thread handle to the main side immediately — this is
            // the bootstrap for the unpark/park synchronisation.
            let _ = thread_tx.send(std::thread::current());

            render_thread_body(terminal, inner_thread, cmd_rx, boot_done_thread, exit_done_thread);
        })
        .expect("failed to spawn render thread");

    // Block (briefly) until the thread has sent its handle.  This completes
    // before any event-loop iteration on the main side.
    let render_thread = thread_rx.recv().expect("render thread failed to send its Thread handle");

    let slot = FrameSlot::new(inner, render_thread);
    let handle = RenderHandle {
        slot,
        cmd_tx,
        join_handle: Some(join_handle),
    };

    (handle, boot_done, exit_done)
}

// ── Thread body ───────────────────────────────────────────────────────────────

fn render_thread_body(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    inner:        Arc<parking_lot::Mutex<Option<Arc<RenderModel>>>>,
    cmd_rx:       mpsc::Receiver<RenderCmd>,
    boot_done:    Arc<AtomicBool>,
    exit_done:    Arc<AtomicBool>,
) {
    // The render thread's own monotonic clock for tachyonfx effect timing.
    // Independent of main-loop pressure: if the main task is busy, animations
    // still advance at the render thread's cadence.
    let mut last_frame = Instant::now();

    let mut boot_fx:      Option<Effect> = None;
    let mut exit_fx:      Option<Effect> = None;
    let mut pending_clear = false;

    loop {
        // Park until the main task publishes a frame or sends a command.
        // Spurious wakeups are safe: the inner loops below re-check state.
        std::thread::park();

        // ── 1. Drain the command channel (higher priority than frame render) ──
        loop {
            match cmd_rx.try_recv() {
                Ok(RenderCmd::SpawnBootFx { fx }) => {
                    boot_fx = Some(fx);
                }
                Ok(RenderCmd::SpawnExitFx { fx }) => {
                    exit_fx = Some(fx);
                    exit_done.store(false, Ordering::Relaxed);
                }
                Ok(RenderCmd::Clear) => {
                    pending_clear = true;
                }
                Ok(RenderCmd::Teardown { ack }) => {
                    // Full terminal restoration — must happen before ack.
                    do_teardown(&mut terminal);
                    let _ = ack.send(());
                    return;   // exit the thread
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Main dropped its sender without sending Teardown —
                    // emergency cleanup and exit.
                    do_teardown(&mut terminal);
                    return;
                }
            }
        }

        // ── 2. Apply pending clear before rendering ───────────────────────────
        if pending_clear {
            terminal.clear().ok();
            pending_clear = false;
        }

        // ── 3. Render the latest pending frame (latest-wins: drain = take once)
        // We take under the lock, then render outside it so the main task can
        // publish the next frame concurrently while we're writing.
        while let Some(model) = inner.lock().take() {
            if let Err(e) = render_frame(
                &mut terminal,
                &model,
                &mut boot_fx,
                &mut exit_fx,
                &mut last_frame,
            ) {
                tracing::warn!(err = %e, "render thread: terminal write failed — PTY likely closed");
                // Do NOT teardown here — stay alive so main's bounded-teardown
                // can send the Teardown command and get a clean exit sequence.
            }
        }

        // ── 4. Effect done checks ────────────────────────────────────────────
        // After every render pass, check if effects have completed.
        // `.done()` is only meaningful once `fx.process()` has been called
        // at least once (i.e. the effect has ticked).
        if boot_fx.as_ref().is_some_and(|fx| fx.done()) {
            boot_done.store(true, Ordering::Release);
            boot_fx = None;  // boot effect is done; release resources
        }
        if exit_fx.as_ref().is_some_and(|fx| fx.done()) {
            exit_done.store(true, Ordering::Release);
            // Don't clear exit_fx — keep it alive so it continues to render
            // on the final frame(s) before teardown.
        }
    }
}

/// Restore the terminal to a sane state.  Called on teardown and on
/// unexpected disconnection of the command channel.
fn do_teardown(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    lifecycle::emergency_teardown_terminal();
    terminal.show_cursor().ok();
    // The Terminal is dropped when the thread exits — that's fine; the
    // crossterm cleanup has already happened above.
}
