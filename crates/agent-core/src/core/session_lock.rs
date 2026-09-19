//! Per-session journal ownership lock.
//!
//! Whoever holds a `Runtime` for session X holds an exclusive advisory
//! `flock` on `<sessions_dir>/<id>.lock`. Two runtimes for the same
//! journal are never allowed — the second caller gets an actionable error
//! pointing at the holder's pid.
//!
//! The lock is released on drop (or when the process dies — `flock`
//! semantics). Stale lock files from dead processes never block.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// Body written into the lock file so the error message can name the holder.
#[derive(Debug, Clone)]
pub struct LockHolder {
    pub pid: u32,
    pub kind: String, // "daemon", "tui", "chat", …
}

impl LockHolder {
    fn serialize(&self) -> String {
        format!("{}\n{}\n", self.pid, self.kind)
    }

    fn deserialize(s: &str) -> Option<Self> {
        let mut lines = s.lines();
        let pid: u32 = lines.next()?.parse().ok()?;
        let kind = lines.next().unwrap_or("unknown").to_string();
        Some(Self { pid, kind })
    }
}

/// Held for the runtime's lifetime; dropping releases the `flock`.
#[derive(Debug)]
pub struct SessionLock {
    _file: File,
    path: PathBuf,
}

impl SessionLock {
    /// Try to acquire an exclusive `flock` on `<dir>/<id>.lock`.
    ///
    /// Returns `Ok(lock)` on success; `Err` with an actionable message if
    /// another process holds it (or on I/O error).
    pub fn try_acquire(dir: &Path, id: &str, holder: LockHolder) -> Result<Self, SessionLockError> {
        let path = dir.join(format!("{}.lock", id));
        std::fs::create_dir_all(dir).map_err(|e| SessionLockError::Io(e, path.clone()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| SessionLockError::Io(e, path.clone()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        if FileExt::try_lock_exclusive(&file).map_err(|e| SessionLockError::Io(e, path.clone()))? {
            // We hold it — write our identity.
            use std::io::Write;
            let _ = file.set_len(0);
            let _ = (&file).write_all(holder.serialize().as_bytes());
            Ok(Self { _file: file, path })
        } else {
            // Someone else holds it — read their identity.
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            let existing = LockHolder::deserialize(&body);
            Err(SessionLockError::Held {
                session_id: id.to_string(),
                holder: existing,
            })
        }
    }

    /// Path of the lock file (for diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Lock-file path for a session (for cleanup).
pub fn lock_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.lock", id))
}

/// The sessions directory (delegates to config).
pub fn sessions_dir() -> PathBuf {
    crate::config::get_active_config_dir().join("sessions")
}

#[derive(Debug)]
pub enum SessionLockError {
    Io(io::Error, PathBuf),
    Held {
        session_id: String,
        holder: Option<LockHolder>,
    },
}

impl std::fmt::Display for SessionLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e, path) => write!(f, "session lock I/O error on {}: {}", path.display(), e),
            Self::Held {
                session_id,
                holder: Some(h),
            } => write!(
                f,
                "session {} is live in another process (pid {}, {}) \
                 — use `synaps --attach {}`, or `synaps daemon sessions`",
                session_id, h.pid, h.kind, session_id
            ),
            Self::Held {
                session_id,
                holder: None,
            } => write!(
                f,
                "session {} is live in another process \
                 — use `synaps --attach {}`, or `synaps daemon sessions`",
                session_id, session_id
            ),
        }
    }
}

impl std::error::Error for SessionLockError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(kind: &str) -> LockHolder {
        LockHolder {
            pid: std::process::id(),
            kind: kind.to_string(),
        }
    }

    #[test]
    fn take_and_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SessionLock::try_acquire(dir.path(), "abc123", holder("tui")).unwrap();
        // Second acquire must fail.
        let err = SessionLock::try_acquire(dir.path(), "abc123", holder("tui")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("abc123"), "error names the session: {msg}");
        assert!(
            msg.contains(&format!("pid {}", std::process::id())),
            "error names the pid: {msg}"
        );
        // Drop releases.
        drop(lock);
        SessionLock::try_acquire(dir.path(), "abc123", holder("tui")).unwrap();
    }

    #[test]
    fn different_sessions_independent() {
        let dir = tempfile::tempdir().unwrap();
        let _a = SessionLock::try_acquire(dir.path(), "aaa", holder("tui")).unwrap();
        let _b = SessionLock::try_acquire(dir.path(), "bbb", holder("tui")).unwrap();
    }

    #[test]
    fn stale_lock_from_dead_process_does_not_block() {
        // flock is released when the fd is closed (process death does that).
        // We simulate by writing a lock file body without holding the flock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.lock");
        std::fs::write(&path, "99999\ndaemon\n").unwrap();
        // Must succeed — no flock held.
        let _lock = SessionLock::try_acquire(dir.path(), "stale", holder("tui")).unwrap();
    }
}
