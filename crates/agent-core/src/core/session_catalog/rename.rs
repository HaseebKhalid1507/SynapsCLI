//! Rename (or un-name) a past session on disk.
//!
//! Same rules as `/saveas`: [`validate_name`], unique across the sessions
//! dir, no clash with a chain name. Safety: the session's own `.lock`
//! (the F23 `SessionLock`) is taken for the whole load → write, so a live
//! session (TUI / daemon / chat holding the flock) is refused, and nobody
//! can open the session mid-rename. The snapshot is rewritten with a
//! durable full save (folding any journal), `updated_at` is left alone,
//! and the snapshot mtime is restored so a rename does not make an old
//! session "latest" for `--continue`.

use crate::core::session::validate_name;
use crate::core::session_journal::{confined_open, SessionPersistence};
use crate::core::session_lock::{lock_path, LockHolder, SessionLock, SessionLockError};
use std::path::Path;

#[derive(Debug)]
pub enum RenameError {
    /// Name fails `[a-z0-9-]{1,40}`.
    InvalidName(String),
    /// Not a plain session id (path separators, empty, …).
    InvalidId,
    NotFound,
    /// Live in another process (holds the session lock).
    Live {
        holder: Option<LockHolder>,
    },
    /// Compacted into `successor` — rename that one instead.
    CompactedInto {
        successor: String,
    },
    /// Another session already carries this name.
    NameTaken {
        other_id: String,
    },
    /// A chain of that name exists.
    ChainConflict,
    Io(std::io::Error),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(e) => f.write_str(e),
            Self::InvalidId => f.write_str("invalid session id"),
            Self::NotFound => f.write_str("no such session"),
            Self::Live { holder: Some(h) } => {
                write!(
                    f,
                    "session is live (pid {}, {}) — rename it there with /saveas",
                    h.pid, h.kind
                )
            }
            Self::Live { holder: None } => {
                f.write_str("session is live — rename it there with /saveas")
            }
            Self::CompactedInto { successor } => {
                write!(
                    f,
                    "session was compacted into {successor} — rename that one"
                )
            }
            Self::NameTaken { other_id } => write!(f, "name already used by session {other_id}"),
            Self::ChainConflict => f.write_str("name conflicts with an existing chain name"),
            Self::Io(e) => write!(f, "rename failed: {e}"),
        }
    }
}

impl std::error::Error for RenameError {}

/// Set (`Some`) or clear (`None`) the name of past session `id` in `dir`.
/// Blocking. Idempotent: setting the current name is `Ok` without a write.
pub fn rename_past_session(dir: &Path, id: &str, name: Option<&str>) -> Result<(), RenameError> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(RenameError::InvalidId);
    }
    if let Some(n) = name {
        validate_name(n).map_err(RenameError::InvalidName)?;
    }
    // Existence first, so a typo never litters a `.lock`.
    let snapshot = format!("{id}.json");
    if confined_open(dir, &snapshot)
        .map_err(RenameError::Io)?
        .is_none()
    {
        return Err(RenameError::NotFound);
    }

    let lock_existed = lock_path(dir, id).exists();
    let holder = LockHolder {
        pid: std::process::id(),
        kind: "rename".into(),
    };
    let lock = match SessionLock::try_acquire(dir, id, holder) {
        Ok(l) => l,
        Err(SessionLockError::Held { holder, .. }) => return Err(RenameError::Live { holder }),
        Err(SessionLockError::CompactedInto { successor_id, .. }) => {
            return Err(RenameError::CompactedInto {
                successor: successor_id,
            })
        }
        Err(SessionLockError::Io(e, _)) => return Err(RenameError::Io(e)),
    };
    let result = rename_locked(dir, id, name);
    if lock_existed {
        drop(lock);
    } else {
        lock.release_and_remove();
    }
    result
}

fn rename_locked(dir: &Path, id: &str, name: Option<&str>) -> Result<(), RenameError> {
    let mut session =
        crate::core::session_journal::load_session_in_dir(dir, id).map_err(RenameError::Io)?;
    if session.name.as_deref() == name {
        return Ok(());
    }
    if let Some(n) = name {
        for entry in
            crate::core::session_journal::session_dir_entries(dir).map_err(RenameError::Io)?
        {
            let Some(other) = entry.name.strip_suffix(".json") else {
                continue;
            };
            if other == id {
                continue;
            }
            if let Some(h) = super::read_header(dir, &entry.name) {
                if h.name.as_deref() == Some(n) {
                    return Err(RenameError::NameTaken { other_id: h.id });
                }
            }
        }
        if crate::core::chain::load_chain(n).is_ok() {
            return Err(RenameError::ChainConflict);
        }
    }
    let mtimes = SessionMtimes::capture(dir, id);

    session.name = name.map(str::to_string);
    save(dir, &session).map_err(RenameError::Io)?;

    // Best effort: keep mtime-ordered "latest" (`--continue`) unchanged.
    mtimes.restore(dir, id);
    Ok(())
}

/// The file mtimes of session `id` (`<id>.json`, and `<id>.journal` when
/// present), so a rename's save can put them back: `latest_session()`
/// (`--continue`) and the `/sessions` order go by mtime, and a rename is not
/// activity in the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionMtimes {
    snapshot: Option<std::time::SystemTime>,
    journal: Option<std::time::SystemTime>,
}

impl SessionMtimes {
    pub fn capture(dir: &Path, id: &str) -> Self {
        let mtime = |name: String| {
            confined_open(dir, &name)
                .ok()
                .flatten()
                .and_then(|f| f.metadata().ok())
                .and_then(|m| m.modified().ok())
        };
        Self {
            snapshot: mtime(format!("{id}.json")),
            journal: mtime(format!("{id}.journal")),
        }
    }

    /// Best effort: set each file that still exists back to its captured
    /// mtime (a file that did not exist before is left alone).
    pub fn restore(&self, dir: &Path, id: &str) {
        for (time, name) in [
            (self.snapshot, format!("{id}.json")),
            (self.journal, format!("{id}.journal")),
        ] {
            if let (Some(t), Ok(Some(f))) = (time, confined_open(dir, &name)) {
                let _ = f.set_modified(t);
            }
        }
    }
}

fn save(dir: &Path, s: &crate::core::session::Session) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::core::session_journal::save_session_durable_in_dir(dir, s, SessionPersistence::Json)
            .map(|_| ())
    }
    #[cfg(not(unix))]
    {
        crate::core::session_journal::save_session_in_dir(dir, s, SessionPersistence::Json)
            .map(|_| ())
    }
}
