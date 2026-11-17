//! The journal lock of a fresh session, and a new id when the one it drew
//! is taken.
//!
//! A fresh session's id is `YYYYMMDD-HHMMSS-` plus a few random hex
//! characters ([`agent_core::session::mint_session_id`]). Two sessions
//! started in the same second can draw the same id. The second one's lock
//! attempt then finds the lock held, and it used to carry on without it:
//! two writers on one journal, and in the daemon the second replaced the
//! first in the session map (the first kept running unlisted, its registry
//! card and per-session socket taken over). A fresh session's id is new by
//! definition, so a taken one is never its own: it draws another.

use agent_core::session_lock::{LockHolder, SessionLock, SessionLockError};

use crate::engine::setup::SessionBootResult;

/// Draws before a fresh session gives up. A taken id is already a rare
/// event; eight in a row is no free id at all.
const FRESH_ID_DRAWS: usize = 8;

/// Take the journal lock for the fresh session in `sb`, giving it a new id
/// while the one it holds is taken: its lock is held, or a journal with
/// that id (`.json` or `.journal`) is already on disk. Runs before anything publishes or writes the
/// id (the `--name` save, the registry card and socket, the START record).
///
/// `Ok(None)` is the old best-effort case only: the lock could not be taken
/// for another reason (an unwritable sessions dir), which is logged.
pub(crate) fn lock_fresh_session(
    runtime: &mut crate::Runtime,
    sb: &mut SessionBootResult,
    holder: LockHolder,
) -> crate::Result<Option<SessionLock>> {
    let dir = agent_core::session_lock::sessions_dir();
    #[cfg(any(test, feature = "testing"))]
    if let Some(id) = testing::take_forced_id() {
        redraw(runtime, sb, id);
    }
    let mut draws = 1;
    loop {
        let id = sb.session.id.clone();
        let on_disk = ["json", "journal"]
            .iter()
            .any(|ext| dir.join(format!("{id}.{ext}")).exists());
        let taken = if on_disk {
            "a journal with this id is on disk".to_string()
        } else {
            match SessionLock::try_acquire(&dir, &id, holder.clone()) {
                Ok(lock) => return Ok(Some(lock)),
                Err(
                    e @ (SessionLockError::Held { .. } | SessionLockError::CompactedInto { .. }),
                ) => e.to_string(),
                Err(e) => {
                    tracing::warn!(session = %id, "session lock: {e}");
                    return Ok(None);
                }
            }
        };
        if draws == FRESH_ID_DRAWS {
            return Err(crate::RuntimeError::Session(format!(
                "no free session id after {FRESH_ID_DRAWS} draws (last {id}: {taken})"
            )));
        }
        draws += 1;
        redraw(
            runtime,
            sb,
            agent_core::session::mint_session_id(chrono::Utc::now()),
        );
        tracing::warn!(taken = %id, session = %sb.session.id, "fresh session id taken ({taken}); drew another");
    }
}

/// Move the fresh session in `sb` to `id`, with the runtime state
/// `resolve_session_and_prompt` keyed by the old one.
fn redraw(runtime: &mut crate::Runtime, sb: &mut SessionBootResult, id: String) {
    sb.session.id = id;
    runtime.set_session_id(Some(sb.session.id.clone()));
    runtime.reset_context_continuation(&sb.session.id, &sb.api_messages);
}

/// Test seam: make the next fresh sessions draw given ids.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    static FORCED: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

    /// The next fresh sessions this process creates draw `ids`, one each,
    /// in order, before their lock is taken (a taken one still gets a new
    /// id). An empty list clears what is left.
    pub fn force_fresh_session_ids(ids: impl IntoIterator<Item = String>) {
        let mut q = FORCED.lock().unwrap_or_else(|p| p.into_inner());
        q.clear();
        q.extend(ids);
    }

    pub(super) fn take_forced_id() -> Option<String> {
        FORCED.lock().unwrap_or_else(|p| p.into_inner()).pop_front()
    }
}
