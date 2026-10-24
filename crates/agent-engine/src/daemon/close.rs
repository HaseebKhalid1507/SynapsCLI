//! `ClientFrame::Close` (`synaps daemon close <ID|NAME> [--force]`): end one
//! session from a control connection, without attaching to it.
//!
//! The daemon sends the session the engine command `close` with host
//! authority (`from: None`), so the actor's own `/close` runs unchanged: the
//! same blockers (and `force`), the same parked exemption (a parked session
//! is never rebuilt), the same reply text. Host authority is no more power
//! than `daemon stop`, which already ends every session from any same-uid
//! shell, but this command is targeted (and the target can be a prefix), so
//! a session with clients attached is refused unless the caller passes
//! `attached` (`--attached`). `force` does not imply it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

use super::DaemonState;
use crate::session::actor::CLOSE_COMMAND;
use crate::session::wire::{DaemonFrame, RefuseReason, CLOSE_QUERY_ID_BASE};
use crate::session::{EndReason, SessionCommand, SessionEventWire, SessionHandle, SessionId, SessionMeta, TransportError};

/// How long a full command queue is retried before giving up (the actor
/// drains it between commands).
pub const CLOSE_SEND_RETRY: Duration = Duration::from_secs(2);
/// How long the actor may take to answer `close`. The command waits in the
/// queue behind whatever the actor is doing (an inline compaction, a
/// preflight), so this is queue latency, not the end itself.
pub const CLOSE_REPLY_BUDGET: Duration = Duration::from_secs(5);
/// How long the session may take to end once it accepted: `finish`'s worst
/// case plus a margin.
pub const CLOSE_END_BUDGET: Duration =
    Duration::from_secs(super::lifecycle::SESSION_END_BUDGET.as_secs() + 1);

/// A reply id no other in-flight close uses.
fn next_close_query_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    CLOSE_QUERY_ID_BASE + NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Serve `Close{session_id, force, attached}`. Always answers with one frame:
/// `Closed`, `Refused{Busy|UnknownSession}` or `Error`.
pub async fn close_session(state: &DaemonState, id: &SessionId, force: bool, attached: bool) -> DaemonFrame {
    // The actor's reload fence swallows a host engine command (it only
    // emits a notice), so no reply would ever come.
    if state.reloading.load(Ordering::SeqCst) {
        return DaemonFrame::Refused { reason: RefuseReason::Busy, message: "daemon reloading; retry in a moment".into() };
    }
    let Some(handle) = state.attach(id) else {
        return DaemonFrame::Refused { reason: RefuseReason::UnknownSession, message: format!("no live session {id}") };
    };
    let sid = handle.id.clone();
    if let Some(holder) = handle.meta().locked_by.clone() {
        return close_placeholder(state, &handle, &holder).await;
    }
    // Someone is looking at it: closing would exit their TUI mid-draft.
    // `force` is about work in flight, not people, so it does not cover this.
    let presence = handle.presence();
    if presence.clients > 0 && !attached {
        let n = presence.clients;
        let owner = presence.input_owner.map(|c| format!(", input owned by client #{}", c.0)).unwrap_or_default();
        return DaemonFrame::Refused {
            reason: RefuseReason::Busy,
            message: format!(
                "session {sid} has {n} client{} attached{owner}; closing it exits their TUI and drops unsent input. \
                 Close it anyway with --attached",
                if n == 1 { "" } else { "s" }
            ),
        };
    }

    // Subscribe before sending: the reply can land before we would look.
    let mut rx = handle.subscribe();
    let query_id = next_close_query_id();
    let cmd = || SessionCommand::EngineCommand {
        id: query_id,
        name: CLOSE_COMMAND.into(),
        arg: if force { "force".into() } else { String::new() },
    };
    let deadline = tokio::time::Instant::now() + CLOSE_SEND_RETRY;
    loop {
        match handle.send(cmd()).await {
            Ok(()) => break,
            Err(TransportError::Backpressure) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(TransportError::Backpressure) => {
                return DaemonFrame::Error {
                    session_id: Some(sid),
                    message: "close: the session's command queue stayed full; retry".into(),
                };
            }
            Err(_) => {
                state.remove(&sid);
                return DaemonFrame::Closed { session_id: sid.clone(), text: format!("session {sid} had already ended") };
            }
        }
    }

    let reply = tokio::time::timeout(CLOSE_REPLY_BUDGET, async {
        loop {
            match rx.recv().await {
                Ok(env) => match env.event {
                    SessionEventWire::QueryResult { id, value } if id == query_id => {
                        let text = value.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string();
                        let ok = value.get("kind").and_then(|k| k.as_str()) != Some("error");
                        return Reply::Answered { ok, text };
                    }
                    // An actor without `close` (echo) that ended anyway.
                    SessionEventWire::Ended { .. } => return Reply::EndedSilently,
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Reply::EndedSilently,
            }
        }
    })
    .await;
    let text = match reply {
        Ok(Reply::Answered { ok: false, text }) => {
            return DaemonFrame::Refused { reason: RefuseReason::Busy, message: text };
        }
        Ok(Reply::Answered { ok: true, text }) => text,
        Ok(Reply::EndedSilently) => format!("session {sid} ended"),
        Err(_) => {
            return DaemonFrame::Error {
                session_id: Some(sid),
                message: format!("close: no reply from the session in {} s", CLOSE_REPLY_BUDGET.as_secs()),
            };
        }
    };

    let ended = tokio::time::timeout(CLOSE_END_BUDGET, handle.closed()).await.is_ok();
    // Idempotent with the host task's own removal; makes the very next
    // `daemon sessions` exact.
    state.remove(&sid);
    let text = if ended { text } else { format!("{text} (still ending in the background)") };
    DaemonFrame::Closed { session_id: sid, text }
}

enum Reply {
    Answered { ok: bool, text: String },
    EndedSilently,
}

/// A lock-held placeholder (F23) stands for a session that is live in
/// another process; it holds no state and does not implement `close`.
/// Ending it only drops it from this daemon's list.
async fn close_placeholder(state: &DaemonState, handle: &SessionHandle, holder: &str) -> DaemonFrame {
    let sid = handle.id.clone();
    let _ = handle.send(SessionCommand::End { reason: EndReason::ClientQuit }).await;
    let _ = tokio::time::timeout(CLOSE_REPLY_BUDGET, handle.closed()).await;
    state.remove(&sid);
    DaemonFrame::Closed {
        session_id: sid.clone(),
        text: format!(
            "session {sid} is live in another process ({holder}); removed it from this daemon's list, it keeps running there"
        ),
    }
}

/// Why [`resolve_session`] found no single session.
#[derive(Debug)]
pub enum ResolveError {
    NotFound,
    /// Every session the query matched, so the caller can list them.
    Ambiguous(Vec<SessionMeta>),
    /// The query only matched as a prefix, and it is shorter than `min`.
    TooShort { min: usize },
}

/// Shortest prefix [`resolve_session`] accepts: the date part of an id
/// (`20261021`). Exact ids, journal ids and names are not prefixes.
pub const MIN_PREFIX_LEN: usize = 8;

/// Resolve what a user typed (`synaps daemon close X`) against a session
/// listing, most specific first: exact id, exact journal id, exact name
/// (two sessions with that name = ambiguous), then a prefix of exactly one
/// session's id or journal id, at least [`MIN_PREFIX_LEN`] characters long.
/// Never guesses between candidates.
pub fn resolve_session(sessions: &[SessionMeta], query: &str) -> Result<SessionId, ResolveError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(ResolveError::NotFound);
    }
    if let Some(m) = sessions.iter().find(|m| m.id.as_str() == q) {
        return Ok(m.id.clone());
    }
    if let Some(m) = sessions.iter().find(|m| m.journal_id == q) {
        return Ok(m.id.clone());
    }
    let named: Vec<&SessionMeta> = sessions.iter().filter(|m| m.name.as_deref() == Some(q)).collect();
    match named.as_slice() {
        [one] => return Ok(one.id.clone()),
        [] => {}
        many => return Err(ResolveError::Ambiguous(many.iter().copied().cloned().collect())),
    }
    let prefixed: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|m| m.id.as_str().starts_with(q) || (!m.journal_id.is_empty() && m.journal_id.starts_with(q)))
        .collect();
    if !prefixed.is_empty() && q.chars().count() < MIN_PREFIX_LEN {
        return Err(ResolveError::TooShort { min: MIN_PREFIX_LEN });
    }
    match prefixed.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(ResolveError::NotFound),
        many => Err(ResolveError::Ambiguous(many.iter().copied().cloned().collect())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, journal: &str, name: Option<&str>) -> SessionMeta {
        let mut m = crate::session::handle::echo::meta_for(&SessionId::from(id));
        m.journal_id = journal.into();
        m.name = name.map(Into::into);
        m
    }

    fn ambiguous_ids(r: Result<SessionId, ResolveError>) -> Vec<String> {
        match r {
            Err(ResolveError::Ambiguous(ms)) => {
                let mut ids: Vec<String> = ms.into_iter().map(|m| m.id.0).collect();
                ids.sort();
                ids
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    fn sessions() -> Vec<SessionMeta> {
        vec![
            meta("alpha-1", "alpha-1", Some("work")),
            meta("alpha-2", "20261020-120000-b9b9", Some("play")),
            meta("gamma-3", "gamma-3", Some("work")),
            meta("delta-4", "delta-4", None),
            meta("20261021-093000-cafe", "20261021-093000-cafe", None),
            meta("20261021-100000-beef", "20261021-100000-beef", None),
        ]
    }

    #[test]
    fn an_exact_id_wins_over_everything_else() {
        let s = sessions();
        assert_eq!(resolve_session(&s, "alpha-1").unwrap().as_str(), "alpha-1");
        assert_eq!(resolve_session(&s, " delta-4 ").unwrap().as_str(), "delta-4");
    }

    #[test]
    fn a_journal_id_resolves_to_its_session() {
        assert_eq!(resolve_session(&sessions(), "20261020-120000-b9b9").unwrap().as_str(), "alpha-2");
    }

    #[test]
    fn a_unique_name_resolves_and_a_shared_name_is_ambiguous() {
        let s = sessions();
        assert_eq!(resolve_session(&s, "play").unwrap().as_str(), "alpha-2");
        assert_eq!(ambiguous_ids(resolve_session(&s, "work")), ["alpha-1", "gamma-3"]);
    }

    #[test]
    fn a_unique_prefix_of_an_id_or_journal_id_resolves() {
        let s = sessions();
        assert_eq!(resolve_session(&s, "20261021-0930").unwrap().as_str(), "20261021-093000-cafe");
        assert_eq!(resolve_session(&s, "20261020-1").unwrap().as_str(), "alpha-2");
    }

    #[test]
    fn a_shared_prefix_is_ambiguous_and_lists_every_candidate() {
        assert_eq!(ambiguous_ids(resolve_session(&sessions(), "20261021")), ["20261021-093000-cafe", "20261021-100000-beef"]);
    }

    #[test]
    fn a_prefix_shorter_than_the_minimum_is_refused_even_when_unique() {
        let s = sessions();
        // `2` or `gam` would be a unique prefix, but one keystroke must not
        // pick a session to end.
        for q in ["2", "2026102", "gam", "alpha-"] {
            assert!(matches!(resolve_session(&s, q), Err(ResolveError::TooShort { min: MIN_PREFIX_LEN })), "{q}");
        }
        // Exact ids and names are not prefixes: any length works.
        assert_eq!(resolve_session(&s, "play").unwrap().as_str(), "alpha-2");
        // A short query that matches nothing is still just not found.
        assert!(matches!(resolve_session(&s, "zz"), Err(ResolveError::NotFound)));
    }

    #[test]
    fn no_match_and_an_empty_query_are_not_found() {
        let s = sessions();
        assert!(matches!(resolve_session(&s, "zeta-zeta"), Err(ResolveError::NotFound)));
        assert!(matches!(resolve_session(&s, "  "), Err(ResolveError::NotFound)));
        assert!(matches!(resolve_session(&[], "alpha"), Err(ResolveError::NotFound)));
    }
}
