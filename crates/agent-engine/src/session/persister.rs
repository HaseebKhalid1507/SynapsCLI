//! Background persistence for one session actor.
//!
//! Session saves (`Session::save`: full snapshot rewrite + fsync) and turn
//! draft writes (`agent_core::core::session_draft`) run on ONE task per
//! actor instead of inside the actor's turn machine, so a slow disk never
//! delays Esc, a round, or a client. Requests are synchronous and never
//! block; the task applies them in batches:
//!
//! - **latest wins, per session id**: a snapshot requested while an older
//!   one for the same id is still queued replaces it (only the newest
//!   history is worth writing); same for draft operations. Different ids
//!   (`/resume`, compaction, a new session) are all kept, in request order.
//! - **save, then draft**: within a batch every snapshot is written before
//!   any draft operation, so a draft removal can never land before the
//!   history that ends its turn (a crash in between leaves a draft next to
//!   a concluded history, which recovery treats as stale).
//! - **a removal waits for a good save**: while the latest save of an id
//!   has failed, removing that id's draft is deferred until a later save of
//!   the id succeeds — across batches, not just within one. The draft is
//!   the only record that a turn was open.
//! - **ordered with every other in-process writer**: `Session::save` holds
//!   the per-session save-order guard (`session_save_order`) for its I/O.
//!
//! `flush` is the barrier: it resolves once everything requested so far has
//! been applied, and reports whether every session's latest save succeeded
//! (a failure stays reported until that session saves cleanly). The actor
//! flushes before anything that needs the disk to be current: parking
//! (drops the in-memory conversation), a durable context-head checkpoint,
//! compaction, `/resume`, checkpoint and teardown.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_core::core::session_draft::TurnDraft;
use tokio::sync::{watch, Notify};

use crate::core::session::Session;

enum DraftOp {
    Write(TurnDraft),
    Remove,
}

#[derive(Default)]
struct Pending {
    /// Latest snapshot per session id, in first-request order.
    saves: Vec<Session>,
    /// Latest draft operation per session id, in first-request order.
    drafts: Vec<(String, DraftOp)>,
    /// Bumped on every request; `Progress::done` catches up to it.
    requested: u64,
    closed: bool,
}

#[derive(Clone, Copy, Debug)]
struct Progress {
    /// The `requested` value the last applied batch covered.
    done: u64,
    /// No session's latest save attempt has failed.
    ok: bool,
}

struct Shared {
    drafts_dir: PathBuf,
    pending: Mutex<Pending>,
    wake: Notify,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Identity of a queued snapshot: everything but `updated_at`, with the
/// messages compared by `Arc` identity (the actor's history only ever
/// shares or appends messages, so equal pointers mean an equal history —
/// no deep comparison, no serialisation of the history).
pub(crate) struct SnapshotKey {
    meta: serde_json::Value,
    messages: Vec<crate::SharedMessage>,
}

impl SnapshotKey {
    pub(crate) fn of(session: &Session) -> Self {
        let mut meta = session.clone();
        meta.api_messages = Vec::new();
        meta.updated_at = chrono::DateTime::<chrono::Utc>::MIN_UTC;
        Self {
            meta: serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null),
            messages: session.api_messages.clone(),
        }
    }
}

impl PartialEq for SnapshotKey {
    fn eq(&self, other: &Self) -> bool {
        self.messages.len() == other.messages.len()
            && self
                .messages
                .iter()
                .zip(&other.messages)
                .all(|(a, b)| Arc::ptr_eq(a, b))
            && self.meta == other.meta
    }
}

pub(crate) struct Persister {
    shared: Arc<Shared>,
    progress: watch::Receiver<Progress>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Persister {
    /// Spawns the task. `drafts_dir` = the sessions directory the drafts
    /// live in (snapshots go wherever `Session::save` puts them).
    pub(crate) fn new(drafts_dir: PathBuf) -> Self {
        let shared = Arc::new(Shared {
            drafts_dir,
            pending: Mutex::new(Pending::default()),
            wake: Notify::new(),
        });
        let (tx, progress) = watch::channel(Progress { done: 0, ok: true });
        let task = tokio::spawn(run(Arc::clone(&shared), tx));
        Self {
            shared,
            progress,
            task: Some(task),
        }
    }

    /// Queue `session` for saving (replaces a queued snapshot of the same id).
    pub(crate) fn save(&self, session: Session) {
        {
            let mut p = self.shared.lock();
            match p.saves.iter_mut().find(|s| s.id == session.id) {
                Some(slot) => *slot = session,
                None => p.saves.push(session),
            }
            p.requested += 1;
        }
        self.shared.wake.notify_one();
    }

    /// Queue a write of `id`'s turn draft.
    pub(crate) fn write_draft(&self, id: &str, draft: TurnDraft) {
        self.draft(id, DraftOp::Write(draft));
    }

    /// Queue the removal of `id`'s turn draft (after any queued save of `id`).
    pub(crate) fn remove_draft(&self, id: &str) {
        self.draft(id, DraftOp::Remove);
    }

    fn draft(&self, id: &str, op: DraftOp) {
        {
            let mut p = self.shared.lock();
            match p.drafts.iter_mut().find(|(i, _)| i == id) {
                Some(slot) => slot.1 = op,
                None => p.drafts.push((id.to_string(), op)),
            }
            p.requested += 1;
        }
        self.shared.wake.notify_one();
    }

    /// Wait until everything requested so far is applied. `true` when every
    /// session's latest save succeeded; `false` while any has failed (until
    /// a later save of it succeeds) or when the task is dead. Unbounded:
    /// callers wrap it in their own budget.
    pub(crate) async fn flush(&self) -> bool {
        let target = self.shared.lock().requested;
        let mut rx = self.progress.clone();
        loop {
            let p = *rx.borrow_and_update();
            if p.done >= target {
                return p.ok;
            }
            if rx.changed().await.is_err() {
                // The task is gone: whatever it had not applied never will be.
                let p = *rx.borrow();
                return p.done >= target && p.ok;
            }
        }
    }

    /// No session's latest save attempt has failed (as of the last applied
    /// batch; `true` before any).
    pub(crate) fn saves_ok(&self) -> bool {
        self.progress.borrow().ok
    }

    /// Nothing queued or in flight.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.progress.borrow().done >= self.shared.lock().requested
    }
}

impl Drop for Persister {
    /// The task finishes what is queued, then exits: a request is never
    /// lost because the actor went away (the actor flushes first when it
    /// needs to know the outcome).
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.wake.notify_one();
        drop(self.task.take());
    }
}

async fn run(shared: Arc<Shared>, progress: watch::Sender<Progress>) {
    // Sessions whose LATEST save attempt failed. Sticky across batches: a
    // later batch with no save of them (a draft write, a removal) must not
    // report success or release their draft removal.
    let mut failed: HashSet<String> = HashSet::new();
    // Draft removals held back because their session's save failed.
    let mut deferred_removes: HashSet<String> = HashSet::new();
    loop {
        let batch = {
            let mut p = shared.lock();
            if p.saves.is_empty() && p.drafts.is_empty() {
                if p.closed {
                    return;
                }
                None
            } else {
                Some((
                    std::mem::take(&mut p.saves),
                    std::mem::take(&mut p.drafts),
                    p.requested,
                ))
            }
        };
        let Some((saves, drafts, covered)) = batch else {
            // `notify_one` stores a permit when nobody waits, so a request
            // made between the check above and here is not missed.
            shared.wake.notified().await;
            continue;
        };

        let mut saved: HashSet<String> = HashSet::new();
        for session in saves {
            match session.save().await {
                Ok(()) => {
                    failed.remove(&session.id);
                    saved.insert(session.id.clone());
                }
                Err(e) => {
                    tracing::error!(session = %session.id, "failed to save session: {e}");
                    failed.insert(session.id.clone());
                }
            }
        }

        let mut ops: Vec<(String, DraftOp)> = Vec::new();
        // A later good save releases a removal that was held back.
        for id in &saved {
            if deferred_removes.remove(id) {
                ops.push((id.clone(), DraftOp::Remove));
            }
        }
        for (id, op) in drafts {
            // A newer operation for the id supersedes a held-back removal.
            deferred_removes.remove(&id);
            ops.retain(|(i, _)| i != &id);
            if matches!(op, DraftOp::Remove) && failed.contains(&id) {
                tracing::warn!(
                    session = %id,
                    "session save failed; keeping the turn draft until a save succeeds"
                );
                deferred_removes.insert(id);
                continue;
            }
            ops.push((id, op));
        }
        for (id, op) in ops {
            let dir = shared.drafts_dir.clone();
            let label = if matches!(op, DraftOp::Write(_)) { "write" } else { "remove" };
            let applied = tokio::task::spawn_blocking(move || match op {
                DraftOp::Write(draft) => {
                    agent_core::core::session_draft::write_turn_draft(&dir, &id, &draft)
                }
                DraftOp::Remove => agent_core::core::session_draft::remove_turn_draft(&dir, &id),
            })
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r);
            if let Err(e) = applied {
                tracing::warn!("turn draft {label}: {e}");
            }
        }

        progress.send_replace(Progress {
            done: covered,
            ok: failed.is_empty(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::core::session_draft::{read_turn_draft, write_turn_draft};
    use serde_json::json;

    fn session_with(n: usize) -> Session {
        let mut s = Session::new("claude-sonnet-4-5", "low", None);
        s.api_messages = (0..n)
            .map(|i| Arc::new(json!({"role": "user", "content": format!("m{i}")})))
            .collect();
        s
    }

    fn draft(base_len: usize) -> TurnDraft {
        TurnDraft {
            base_len,
            partial_text: "partial".into(),
        }
    }

    fn on_disk(id: &str) -> usize {
        Session::load(id).map(|s| s.api_messages.len()).unwrap_or(0)
    }

    /// Many requests, one outcome: the disk ends in the LAST requested
    /// state, and `flush` waits for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn the_disk_ends_in_the_last_requested_state() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let p = Persister::new(dir.clone());
        let mut s = session_with(1);
        for n in 1..=50 {
            s.api_messages = session_with(n).api_messages;
            p.save(s.clone());
            p.write_draft(&s.id, draft(n));
        }
        assert!(p.flush().await);
        assert!(p.is_idle());
        assert_eq!(on_disk(&s.id), 50);
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), Some(draft(50)));

        p.save(s.clone());
        p.remove_draft(&s.id);
        assert!(p.flush().await);
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), None);
    }

    /// Latest-wins is per session id: switching sessions keeps both
    /// sessions' operations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn operations_for_different_sessions_are_all_applied() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let a = session_with(2);
        let b = session_with(3);
        write_turn_draft(&dir, &a.id, &draft(2)).unwrap();
        let p = Persister::new(dir.clone());
        p.save(a.clone());
        p.remove_draft(&a.id);
        p.save(b.clone());
        p.write_draft(&b.id, draft(3));
        assert!(p.flush().await);
        assert_eq!((on_disk(&a.id), on_disk(&b.id)), (2, 3));
        assert_eq!(read_turn_draft(&dir, &a.id).unwrap(), None, "A's removal kept");
        assert_eq!(read_turn_draft(&dir, &b.id).unwrap(), Some(draft(3)));
    }

    /// A draft removal never follows a FAILED save: it waits for the next
    /// good save of that session, then lands.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_removal_waits_for_a_good_save() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let s = session_with(2);
        write_turn_draft(&dir, &s.id, &draft(1)).unwrap();
        // Block the snapshot: a non-empty directory where `<id>.json` goes.
        let snapshot = dir.join(format!("{}.json", s.id));
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("occupied"), b"x").unwrap();

        let p = Persister::new(dir.clone());
        p.save(s.clone());
        p.remove_draft(&s.id);
        assert!(!p.flush().await, "the failed save is reported");
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), Some(draft(1)), "draft kept");

        std::fs::remove_dir_all(&snapshot).unwrap();
        p.save(s.clone());
        assert!(p.flush().await);
        assert_eq!(on_disk(&s.id), 2);
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), None, "removed after the good save");
    }

    /// A failure is sticky: after a failed save, a batch with no save of
    /// that session (a draft write, then a lone removal) neither reports
    /// success nor releases the removal. (Review finding: `failed` used to
    /// be per batch, so the removal went through and `flush` said `true`.)
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_failed_save_stays_failed_until_a_save_succeeds() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let s = session_with(2);
        let snapshot = dir.join(format!("{}.json", s.id));
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("occupied"), b"x").unwrap();
        let p = Persister::new(dir.clone());
        p.save(s.clone());
        assert!(!p.flush().await);
        p.write_draft(&s.id, draft(2));
        assert!(!p.flush().await, "a draft-only batch does not clear the failure");
        assert!(!p.saves_ok());
        p.remove_draft(&s.id);
        assert!(!p.flush().await);
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), Some(draft(2)), "draft kept");

        std::fs::remove_dir_all(&snapshot).unwrap();
        p.save(s.clone());
        assert!(p.flush().await, "the good save clears it");
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), None, "and releases the removal");
    }

    /// A new turn's draft supersedes a removal that was held back.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_new_draft_supersedes_a_held_back_removal() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let s = session_with(2);
        let snapshot = dir.join(format!("{}.json", s.id));
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("occupied"), b"x").unwrap();
        let p = Persister::new(dir.clone());
        p.save(s.clone());
        p.remove_draft(&s.id);
        assert!(!p.flush().await);

        std::fs::remove_dir_all(&snapshot).unwrap();
        p.write_draft(&s.id, draft(9));
        p.save(s.clone());
        assert!(p.flush().await);
        assert_eq!(read_turn_draft(&dir, &s.id).unwrap(), Some(draft(9)));
    }

    /// The actor skips re-queueing an identical snapshot; anything that
    /// matters on disk makes it differ.
    #[test]
    fn snapshot_key_ignores_only_the_save_time() {
        let s = session_with(2);
        let mut later = s.clone();
        later.updated_at = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert!(SnapshotKey::of(&s) == SnapshotKey::of(&later));

        let mut appended = s.clone();
        appended.api_messages.push(Arc::new(json!({"role": "user", "content": "more"})));
        assert!(SnapshotKey::of(&s) != SnapshotKey::of(&appended));

        let mut renamed = s.clone();
        renamed.model = "claude-opus-4-6".into();
        assert!(SnapshotKey::of(&s) != SnapshotKey::of(&renamed));

        let mut costed = s.clone();
        costed.session_cost += 0.01;
        assert!(SnapshotKey::of(&s) != SnapshotKey::of(&costed));

        // Same content, rebuilt message: conservatively a different state.
        let mut rebuilt = s.clone();
        rebuilt.api_messages[1] = Arc::new((*s.api_messages[1]).clone());
        assert!(SnapshotKey::of(&s) != SnapshotKey::of(&rebuilt));
    }

    /// Dropping the persister still applies what was queued.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn queued_work_survives_the_drop() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let s = session_with(4);
        let p = Persister::new(dir);
        p.save(s.clone());
        drop(p);
        for _ in 0..200 {
            if on_disk(&s.id) == 4 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the queued save never landed");
    }
}
