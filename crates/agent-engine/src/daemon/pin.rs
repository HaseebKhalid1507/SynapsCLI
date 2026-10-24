//! `ClientFrame::Pin` (`synaps daemon pin <ID|NAME> [--off]`): pin or unpin
//! one session from a control connection, without attaching to it.
//!
//! The daemon sends the session `KeepWarm{on}` with host authority
//! (`from: None`), so the actor's own handling runs unchanged: pinning a
//! parked session unparks it, unpinning one leaves it parked. The answer
//! comes from the handle's pin cell, which the actor writes when it applies
//! the command; no reply event to match.

use std::time::Duration;

use super::DaemonState;
use crate::session::wire::{DaemonFrame, RefuseReason};
use crate::session::{SessionCommand, SessionId, SessionLifecycle, TransportError};

/// How long the daemon waits for the actor to apply the pin (queue latency).
pub const PIN_BUDGET: Duration = Duration::from_secs(2);
/// Pinning a parked session rebuilds its runtime first (MCP servers,
/// extensions), which can take longer than [`PIN_BUDGET`].
pub const PIN_UNPARK_BUDGET: Duration = Duration::from_secs(10);
/// How often the pin cell is polled while waiting.
const PIN_POLL: Duration = Duration::from_millis(20);

/// Serve `Pin{session_id, on}`. Always answers with one frame: `Pinned`,
/// `Refused{Busy|UnknownSession}` or `Error`.
pub async fn pin_session(state: &DaemonState, id: &SessionId, on: bool) -> DaemonFrame {
    // The actor's reload fence swallows a host command, so the pin would
    // never land.
    if state.reloading.load(std::sync::atomic::Ordering::SeqCst) {
        return DaemonFrame::Refused { reason: RefuseReason::Busy, message: "daemon reloading; retry in a moment".into() };
    }
    let Some(handle) = state.attach(id) else {
        return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("no live session {id}") };
    };
    let sid = handle.id.clone();
    if let Some(holder) = handle.meta().locked_by.clone() {
        return DaemonFrame::Refused {
            reason: RefuseReason::Busy,
            // Usually an in-process TUI, which has no pin (`/pin` there only
            // says it applies to daemon sessions): name the way into the
            // daemon instead of sending the user back to it.
            message: format!(
                "session {sid} is live in another process ({holder}), not in this daemon, so there is nothing \
                 to pin here. Quit it there, then load it into the daemon with `synaps --attach --continue {sid}` \
                 and pin it with /pin or `synaps daemon pin {sid}`"
            ),
        };
    }

    let budget = if on && handle.lifecycle() == SessionLifecycle::Parked { PIN_UNPARK_BUDGET } else { PIN_BUDGET };
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match handle.send(SessionCommand::KeepWarm { on }).await {
            Ok(()) => break,
            Err(TransportError::Backpressure) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(PIN_POLL).await;
            }
            Err(TransportError::Backpressure) => {
                return DaemonFrame::Error {
                    session_id: Some(sid),
                    message: "pin: the session's command queue stayed full; retry".into(),
                };
            }
            Err(_) => {
                return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("session {sid} has ended") };
            }
        }
    }
    while handle.pinned() != on {
        if !handle.is_alive() {
            return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("session {sid} has ended") };
        }
        if tokio::time::Instant::now() >= deadline {
            return DaemonFrame::Error {
                session_id: Some(sid),
                message: format!("pin: the session did not apply it within {} s", budget.as_secs()),
            };
        }
        tokio::time::sleep(PIN_POLL).await;
    }
    DaemonFrame::Pinned { session_id: sid, on }
}
