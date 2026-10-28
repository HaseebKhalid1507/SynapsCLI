//! `ClientFrame::Rename` (`synaps daemon rename <ID|NAME> <NEW_NAME> |
//! --clear [--force]`): name or unname one session from a control
//! connection, without attaching.
//!
//! Renaming is metadata, not input: it needs no input ownership, and the
//! connection never attaches, so presence (clients, owner, the shared-inbox
//! claim) does not change. The daemon sends the session the host-only
//! engine command `rename` (JSON args: name + the compare-and-set `expect`),
//! handled by `SessionActor::host_rename`: `/saveas`'s rules under a
//! process-wide name lock, a durable save that keeps the file mtimes, and a
//! `Conversation` to every attached client. A parked session is renamed on
//! disk under its journal lock (`session_catalog::rename_past_session`)
//! without being woken; it picks the name up from disk when it unparks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

use super::DaemonState;
use crate::session::actor::RENAME_COMMAND;
use crate::session::actor_cmds::RenameArgs;
use crate::session::wire::{DaemonFrame, RefuseReason, RenameRefusal, RENAME_QUERY_ID_BASE};
use crate::session::{SessionCommand, SessionEventWire, SessionId, TransportError};

/// How long a full command queue is retried before giving up.
pub const RENAME_SEND_RETRY: Duration = Duration::from_secs(2);
/// How long the actor may take to answer: queue latency plus one durable
/// save (a parked session's save folds its journal, which can be large).
pub const RENAME_REPLY_BUDGET: Duration = Duration::from_secs(15);

fn next_rename_query_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    RENAME_QUERY_ID_BASE + NEXT.fetch_add(1, Ordering::Relaxed)
}

fn failed(session_id: SessionId, reason: RenameRefusal, message: String) -> DaemonFrame {
    DaemonFrame::RenameFailed { session_id, reason, message, taken_by: None, current_name: None }
}

/// Serve `Rename{session_id, name, expect}`. `None` or blank clears the
/// name. Always answers with one frame: `Renamed`, `RenameFailed`, or
/// `Refused{UnknownSession}`.
pub async fn rename_session(
    state: &DaemonState,
    id: &SessionId,
    name: Option<&str>,
    expect: Option<Option<String>>,
) -> DaemonFrame {
    if state.reloading.load(Ordering::SeqCst) {
        return failed(id.clone(), RenameRefusal::Busy, "daemon reloading; retry in a moment".into());
    }
    let Some(handle) = state.attach(id) else {
        return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("no live session {id}") };
    };
    let sid = handle.id.clone();
    if let Some(holder) = handle.meta().locked_by.clone() {
        return failed(
            sid.clone(),
            RenameRefusal::Locked,
            format!("session {sid} is live in another process ({holder}), not in this daemon; rename it there with /saveas"),
        );
    }
    let arg = serde_json::to_string(&RenameArgs { name: name.map(str::to_string), expect })
        .expect("rename args serialize");

    // Subscribe before sending: the reply can land before we would look.
    let mut rx = handle.subscribe();
    let query_id = next_rename_query_id();
    let deadline = tokio::time::Instant::now() + RENAME_SEND_RETRY;
    loop {
        let cmd = SessionCommand::EngineCommand { id: query_id, name: RENAME_COMMAND.into(), arg: arg.clone() };
        match handle.send(cmd).await {
            Ok(()) => break,
            Err(TransportError::Backpressure) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(TransportError::Backpressure) => {
                return failed(sid, RenameRefusal::Busy, "the session's command queue stayed full; retry".into());
            }
            Err(_) => {
                return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("session {sid} has ended") };
            }
        }
    }

    let reply = tokio::time::timeout(RENAME_REPLY_BUDGET, async {
        loop {
            match rx.recv().await {
                Ok(env) => match env.event {
                    SessionEventWire::QueryResult { id, value } if id == query_id => return Some(value),
                    SessionEventWire::Ended { .. } => return None,
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    match reply {
        Ok(Some(value)) => reply_frame(sid, &value),
        Ok(None) => DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("session {sid} has ended") },
        Err(_) => failed(
            sid,
            RenameRefusal::Unknown,
            format!(
                "no reply from the session in {} s; the rename may still apply. Re-list (`synaps daemon sessions`) before retrying",
                RENAME_REPLY_BUDGET.as_secs()
            ),
        ),
    }
}

/// The actor's `rename` reply → the wire answer.
fn reply_frame(session_id: SessionId, value: &serde_json::Value) -> DaemonFrame {
    let text = value.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string();
    if value.get("kind").and_then(|k| k.as_str()) == Some("error") {
        let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("failed");
        let taken_by = value.get("taken_by").and_then(|t| t.as_str()).map(str::to_string);
        let current_name = value.get("current_name").and_then(|t| t.as_str()).map(str::to_string);
        return DaemonFrame::RenameFailed {
            session_id,
            reason: RenameRefusal::from_code(code),
            message: text,
            taken_by,
            current_name,
        };
    }
    let name = value.get("name").and_then(|n| n.as_str()).map(str::to_string);
    let in_memory_only = value.get("in_memory_only").and_then(|b| b.as_bool()).unwrap_or(false);
    DaemonFrame::Renamed { session_id, name, in_memory_only }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_replies_map_to_typed_frames() {
        let ok = serde_json::json!({ "kind": "output", "text": "session named 'axon'", "name": "axon" });
        assert!(matches!(reply_frame("s".into(), &ok), DaemonFrame::Renamed { name: Some(ref n), in_memory_only: false, .. } if n == "axon"));
        let cleared = serde_json::json!({ "kind": "output", "text": "x", "name": null, "in_memory_only": true });
        assert!(matches!(reply_frame("s".into(), &cleared), DaemonFrame::Renamed { name: None, in_memory_only: true, .. }));
        let named = serde_json::json!({ "kind": "error", "code": "named", "text": "x", "current_name": "kite" });
        assert!(matches!(
            reply_frame("s".into(), &named),
            DaemonFrame::RenameFailed { reason: RenameRefusal::Named, current_name: Some(ref c), .. } if c == "kite"
        ));
        let taken = serde_json::json!({
            "kind": "error", "code": "taken", "text": "name 'axon' already used by session t", "taken_by": "t",
        });
        assert!(matches!(
            reply_frame("s".into(), &taken),
            DaemonFrame::RenameFailed { reason: RenameRefusal::Taken, taken_by: Some(ref t), ref message, .. }
                if t == "t" && message.contains("already used")
        ));
        let odd = serde_json::json!({ "kind": "error", "text": "x" });
        assert!(matches!(reply_frame("s".into(), &odd), DaemonFrame::RenameFailed { reason: RenameRefusal::Failed, .. }));
    }
}
