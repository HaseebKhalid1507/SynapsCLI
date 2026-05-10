//! Process signal handling for the chat TUI.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Interrupt,
    Terminate,
    Hangup,
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
}
