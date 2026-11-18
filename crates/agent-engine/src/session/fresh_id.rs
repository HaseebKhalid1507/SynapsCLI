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
/// while the one it holds is taken: its lock is held, it was compacted
/// into a successor, or a journal with that id (`.json`, `.journal` or a
/// `.turn` draft) is already on disk. Runs before anything publishes or
/// writes the id (the `--name` save, the registry card and socket, the
/// START record).
///
/// The lock comes first and the disk check second: every cooperating writer
/// of a journal holds its lock, so no journal can appear between the two.
/// A draw that fails leaves no lock file of its own: a held lock's file is
/// its holder's, a compacted id is refused before its file is opened, and
/// over a journal on disk the file stays where that journal's own sessions
/// keep theirs (unlinking one that another process may be opening would
/// reopen the unlink race).
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
        let taken = match SessionLock::try_acquire(&dir, &id, holder.clone()) {
            Ok(lock) => match journal_on_disk(&dir, &id) {
                None => return Ok(Some(lock)),
                Some(file) => {
                    drop(lock);
                    format!("{file} is on disk")
                }
            },
            Err(e @ (SessionLockError::Held { .. } | SessionLockError::CompactedInto { .. })) => {
                taken_reason(&e)
            }
            Err(e) => {
                tracing::warn!(session = %id, "session lock: {e}");
                return Ok(None);
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

/// The journal file already under `id`, if any.
fn journal_on_disk(dir: &std::path::Path, id: &str) -> Option<String> {
    ["json", "journal", "turn"]
        .iter()
        .map(|ext| format!("{id}.{ext}"))
        .find(|file| dir.join(file).exists())
}

/// Why a fresh session cannot have the id it drew. Not the lock error's own
/// text: that tells the user to attach to the session, a stranger's here.
fn taken_reason(e: &SessionLockError) -> String {
    match e {
        SessionLockError::Held {
            holder: Some(h), ..
        } => {
            format!("its lock is held by pid {} ({})", h.pid, h.kind)
        }
        SessionLockError::Held { holder: None, .. } => "its lock is held".to_string(),
        SessionLockError::CompactedInto { successor_id, .. } => {
            format!("it was compacted into {successor_id}")
        }
        SessionLockError::Io(e, _) => format!("lock I/O error: {e}"),
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

#[cfg(test)]
mod tests {
    use super::{journal_on_disk, taken_reason};
    use agent_core::session_lock::{LockHolder, SessionLockError};

    #[test]
    fn a_taken_reason_never_points_at_the_strangers_session() {
        let held = SessionLockError::Held {
            session_id: "20261117-015328-de08".into(),
            holder: Some(LockHolder {
                pid: 42,
                kind: "daemon".into(),
            }),
        };
        assert_eq!(taken_reason(&held), "its lock is held by pid 42 (daemon)");
        let anon = SessionLockError::Held {
            session_id: "20261117-015328-de08".into(),
            holder: None,
        };
        let compacted = SessionLockError::CompactedInto {
            session_id: "20261117-015328-de08".into(),
            successor_id: "20261117-015329-0e0d".into(),
        };
        for e in [&held, &anon, &compacted] {
            let r = taken_reason(e);
            assert!(!r.contains("attach") && !r.contains("de08"), "{r}");
        }
    }

    #[test]
    fn any_journal_file_takes_an_id() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(journal_on_disk(dir.path(), "a"), None);
        std::fs::write(dir.path().join("a.lock"), "").unwrap();
        assert_eq!(
            journal_on_disk(dir.path(), "a"),
            None,
            "a lock is not a journal"
        );
        for ext in ["turn", "journal", "json"] {
            std::fs::write(dir.path().join(format!("a.{ext}")), "").unwrap();
            assert_eq!(journal_on_disk(dir.path(), "a"), Some(format!("a.{ext}")));
        }
    }
}
