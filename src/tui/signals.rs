//! Process signal handling for the chat TUI.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Interrupt,
    Terminate,
    Hangup,
}

/// What the event loop should do in response to a shutdown signal.
///
/// `ImmediateExit` — skip the exit animation and break immediately. Used for
/// SIGTERM / SIGHUP where the terminal may already be gone and systemd is
/// counting down. `AnimatedExit` — play the 830ms quit effect (interactive
/// Ctrl-C on a live terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownAction {
    ImmediateExit,
    AnimatedExit,
}

/// Pure policy: map a signal to the shutdown action the event loop should take.
///
/// Extracted as a free function so it can be unit-tested without a real
/// terminal. The loop in `mod.rs` delegates to this instead of hardcoding the
/// decision inline.
pub(crate) fn shutdown_action(signal: ShutdownSignal) -> ShutdownAction {
    match signal {
        // SIGTERM and SIGHUP arrive from systemd, tmux kill-pane, SSH drops,
        // etc. The terminal is likely already gone — exit now, no animation.
        ShutdownSignal::Terminate | ShutdownSignal::Hangup => ShutdownAction::ImmediateExit,
        // Interactive Ctrl-C from a live terminal — show the quit animation.
        ShutdownSignal::Interrupt => ShutdownAction::AnimatedExit,
    }
}

pub(crate) fn signal_label(signal: ShutdownSignal) -> &'static str {
    match signal {
        ShutdownSignal::Interrupt => "interrupt",
        ShutdownSignal::Terminate => "terminate",
        ShutdownSignal::Hangup => "hangup",
    }
}

pub(crate) fn spawn_shutdown_signal_task(
    tx: tokio::sync::mpsc::UnboundedSender<ShutdownSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    super::lifecycle::emergency_teardown_terminal();
                    let _ = tx.send(ShutdownSignal::Interrupt);
                }
                _ = async {
                    if let Some(signal) = sigterm.as_mut() {
                        signal.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    super::lifecycle::emergency_teardown_terminal();
                    let _ = tx.send(ShutdownSignal::Terminate);
                }
                _ = async {
                    if let Some(signal) = sighup.as_mut() {
                        signal.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    super::lifecycle::emergency_teardown_terminal();
                    let _ = tx.send(ShutdownSignal::Hangup);
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            super::lifecycle::emergency_teardown_terminal();
            let _ = tx.send(ShutdownSignal::Interrupt);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(signal_label(ShutdownSignal::Interrupt), "interrupt");
        assert_eq!(signal_label(ShutdownSignal::Terminate), "terminate");
        assert_eq!(signal_label(ShutdownSignal::Hangup), "hangup");
    }

    // FIX B: verify the signal→action policy is correct and stays correct.
    // SIGTERM/SIGHUP must exit immediately (no animation on a dead terminal).
    // Ctrl-C (Interrupt) may play the animation since the terminal is live.
    #[test]
    fn terminate_and_hangup_are_immediate() {
        assert_eq!(shutdown_action(ShutdownSignal::Terminate), ShutdownAction::ImmediateExit);
        assert_eq!(shutdown_action(ShutdownSignal::Hangup), ShutdownAction::ImmediateExit);
    }

    #[test]
    fn interrupt_is_animated() {
        assert_eq!(shutdown_action(ShutdownSignal::Interrupt), ShutdownAction::AnimatedExit);
    }
}
