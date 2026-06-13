//! Process signal handling for the chat TUI.
//!
//! # Why signal-hook instead of tokio's async signal streams?
//!
//! Tokio's `tokio::signal::unix::signal` / `ctrl_c` streams install the OS
//! handler correctly (proven: SigCgt mask shows all three signals caught,
//! SigPnd clears on delivery) but their `.recv().await` **never resolves** in
//! synaps's TUI runtime.  The exact same pattern works in a standalone binary,
//! so some aspect of this process's runtime state breaks tokio's async signal
//! driver.  Rather than chase that, we sidestep it entirely.
//!
//! A **dedicated `std::thread`** runs `signal_hook::iterator::Signals::forever()`
//! in a plain blocking loop.  When a signal arrives the thread sends on the
//! existing tokio `UnboundedSender`.  Sending from a std thread is safe and
//! wakes the tokio receiver — **when the event loop is free to receive it**.
//!
//! # Second problem: blocked write() in draw()
//!
//! The main tokio task calls `draw()` synchronously (no `spawn_blocking`).
//! If the terminal's read side stops draining output (e.g. the PTY master
//! closes while the kernel buffer is full), the `write()` inside crossterm
//! blocks indefinitely, preventing the `select!` from ever running.  The
//! channel send from the signal thread is correct, but the receiver is never
//! polled.
//!
//! Fix: after sending on the channel, the signal thread also spawns a
//! **watchdog** std thread that calls `std::process::exit(1)` after
//! `WATCHDOG_TIMEOUT_SECS` if the process hasn't exited via the normal loop
//! path.  The watchdog does NOT call `emergency_teardown_terminal()` —
//! crossterm holds a `parking_lot::Mutex` (`TERMINAL_MODE_PRIOR_RAW_MODE`)
//! during draw; calling it from a concurrent thread deadlocks.  Terminal
//! state resets on process exit anyway.
//!
//! # Watchdog / teardown timeout invariant
//!
//! The post-loop teardown in `mod.rs` is split into two sequential budgets:
//!   - `SAVE_TIMEOUT_SECS`  : save_session + append_record (data safety first)
//!   - `HOOKS_TIMEOUT_SECS` : on_session_end hook emit (concurrent, fail-open)
//!
//! `TEARDOWN_TIMEOUT_SECS` = their sum (total teardown budget).
//! `WATCHDOG_TIMEOUT_SECS` = teardown budget + a margin so the watchdog only
//! fires when the process is genuinely stuck — never during a legitimate slow
//! teardown.
//!
//! A compile-time assert enforces `WATCHDOG_TIMEOUT_SECS > TEARDOWN_TIMEOUT_SECS`
//! so the invariant cannot be silently violated by a future edit.

// ── Teardown / watchdog timing constants ─────────────────────────────────────
//
// These constants are the single source of truth for all shutdown budgets.
// Both sites (watchdog sleep below, teardown timeout in mod.rs) reference them
// to prevent the kind of drift that previously violated the invariant.
//
// Breakdown:
//   SAVE_TIMEOUT_SECS  — budget for save_session() + append_record()
//   HOOKS_TIMEOUT_SECS — budget for concurrent on_session_end hook emit
//   TEARDOWN_TIMEOUT_SECS — sum of the above; total teardown budget for mod.rs
//   WATCHDOG_TIMEOUT_SECS — must exceed TEARDOWN_TIMEOUT_SECS by a margin
pub(crate) const SAVE_TIMEOUT_SECS:     u64 = 2;
pub(crate) const HOOKS_TIMEOUT_SECS:    u64 = 5;
pub(crate) const TEARDOWN_TIMEOUT_SECS: u64 = SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS;
pub(crate) const WATCHDOG_TIMEOUT_SECS: u64 = TEARDOWN_TIMEOUT_SECS + 3; // margin

// Compile-time invariant: watchdog must always outlast the full teardown.
const _: () = assert!(
    WATCHDOG_TIMEOUT_SECS > TEARDOWN_TIMEOUT_SECS,
    "WATCHDOG_TIMEOUT_SECS must exceed TEARDOWN_TIMEOUT_SECS"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Interrupt,
    Terminate,
    Hangup,
}

/// What the event loop should do in response to a shutdown signal.
///
/// All OS signals map to `ImmediateExit`: break out of the event loop
/// immediately and fall through to the bounded teardown path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownAction {
    ImmediateExit,
}

/// Pure policy: map a signal to the shutdown action the event loop should take.
///
/// Extracted as a free function so it can be unit-tested without a real
/// terminal.  The loop in `mod.rs` delegates to this instead of hardcoding
/// the decision inline.
pub(crate) fn shutdown_action(_signal: ShutdownSignal) -> ShutdownAction {
    // All OS signals exit immediately — no animation.  For SIGTERM/SIGHUP
    // the terminal may already be gone.  For Ctrl-C (Interrupt) we exit
    // cleanly via the event loop rather than relying on animation timing,
    // which is unreliable relative to the watchdog.
    ShutdownAction::ImmediateExit
}

pub(crate) fn signal_label(signal: ShutdownSignal) -> &'static str {
    match signal {
        ShutdownSignal::Interrupt => "interrupt",
        ShutdownSignal::Terminate => "terminate",
        ShutdownSignal::Hangup    => "hangup",
    }
}

/// A handle that can stop the signal-listener thread.
///
/// Dropping or calling `.close()` unregisters the signal hooks and causes the
/// blocking `Signals::forever()` iterator to return, letting the thread exit
/// cleanly.
pub(crate) struct SignalHandle {
    #[cfg(unix)]
    inner: signal_hook::iterator::Handle,
}

impl SignalHandle {
    pub(crate) fn close(self) {
        #[cfg(unix)]
        self.inner.close();
    }
}

/// Spawn a **std::thread** that delivers OS signals over the existing tokio
/// mpsc channel.
///
/// Returns a `SignalHandle` whose `.close()` method stops the thread.  The
/// caller in `mod.rs` should call `handle.close()` instead of `.abort()`-ing
/// a tokio `JoinHandle`.
///
/// On non-Unix targets a minimal tokio `ctrl_c` fallback is used instead.
#[cfg(unix)]
pub(crate) fn spawn_shutdown_signal_task(
    tx: tokio::sync::mpsc::UnboundedSender<ShutdownSignal>,
) -> SignalHandle {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT])
        .expect("failed to register signal hooks");
    let handle = signals.handle();

    std::thread::Builder::new()
        .name("signal-listener".into())
        .spawn(move || {
            for sig in signals.forever() {
                tracing::debug!(sig, "signal-listener: received signal");
                let shutdown = match sig {
                    SIGINT  => ShutdownSignal::Interrupt,
                    SIGTERM => ShutdownSignal::Terminate,
                    SIGHUP  => ShutdownSignal::Hangup,
                    _       => continue,
                };

                // IMPORTANT: do NOT call emergency_teardown_terminal() here.
                // crossterm holds a parking_lot::Mutex (TERMINAL_MODE_PRIOR_RAW_MODE)
                // during draw operations; taking it from a signal thread deadlocks.

                // Send on the tokio channel — wakes the event loop when it is free.
                let _ = tx.send(shutdown);

                // Fallback: if the main async task is blocked in a synchronous
                // write() to the PTY (happens when the terminal read-side stops
                // draining output and the buffer fills), the select! branch above
                // never gets scheduled.  Spawn a watchdog that forces exit after
                // WATCHDOG_TIMEOUT_SECS if the process hasn't already exited via
                // normal teardown.
                //
                // WATCHDOG_TIMEOUT_SECS > TEARDOWN_TIMEOUT_SECS (compile-time
                // asserted above), so the watchdog never races a valid teardown.
                //
                // exit(1) — not exit(0) — because a watchdog-forced exit means
                // something was stuck; reporting clean exit (0) to systemd/supervisors
                // would hide data-loss and obscure the root cause.
                std::thread::Builder::new()
                    .name("signal-watchdog".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(WATCHDOG_TIMEOUT_SECS));
                        // Do NOT call emergency_teardown_terminal() here — it also
                        // acquires crossterm's TERMINAL_MODE_PRIOR_RAW_MODE mutex,
                        // which the main thread may hold during draw().  Just force
                        // exit; the terminal will reset on process death anyway.
                        tracing::warn!(
                            watchdog_secs = WATCHDOG_TIMEOUT_SECS,
                            "signal watchdog: clean exit timed out, forcing exit(1)"
                        );
                        std::process::exit(1);
                    })
                    .ok();

                // One signal is enough; the event loop handles the rest.
                break;
            }
        })
        .expect("failed to spawn signal-listener thread");

    SignalHandle { inner: handle }
}

#[cfg(not(unix))]
pub(crate) fn spawn_shutdown_signal_task(
    tx: tokio::sync::mpsc::UnboundedSender<ShutdownSignal>,
) -> SignalHandle {
    // Non-Unix: fall back to tokio ctrl_c (the only signal available).
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        super::lifecycle::emergency_teardown_terminal();
        let _ = tx.send(ShutdownSignal::Interrupt);
    });
    SignalHandle {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(signal_label(ShutdownSignal::Interrupt), "interrupt");
        assert_eq!(signal_label(ShutdownSignal::Terminate), "terminate");
        assert_eq!(signal_label(ShutdownSignal::Hangup),    "hangup");
    }

    // Verify the signal→action policy is correct and stays correct.
    // All OS signals map to ImmediateExit — the event loop breaks immediately
    // without playing an animation.  This is reliable regardless of system
    // load and never races the watchdog.
    #[test]
    fn all_signals_are_immediate_exit() {
        assert_eq!(shutdown_action(ShutdownSignal::Terminate), ShutdownAction::ImmediateExit);
        assert_eq!(shutdown_action(ShutdownSignal::Hangup),    ShutdownAction::ImmediateExit);
        assert_eq!(shutdown_action(ShutdownSignal::Interrupt), ShutdownAction::ImmediateExit);
    }

    #[test]
    fn all_signals_have_labels() {
        for sig in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate, ShutdownSignal::Hangup] {
            assert!(!signal_label(sig).is_empty());
        }
    }

    #[test]
    fn watchdog_exceeds_teardown_budget() {
        // Runtime version of the compile-time assert — belt-and-suspenders.
        assert!(WATCHDOG_TIMEOUT_SECS > TEARDOWN_TIMEOUT_SECS);
        assert_eq!(TEARDOWN_TIMEOUT_SECS, SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS);
    }

    #[test]
    fn teardown_budget_is_sum_of_parts() {
        assert_eq!(TEARDOWN_TIMEOUT_SECS, SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS);
    }
}
