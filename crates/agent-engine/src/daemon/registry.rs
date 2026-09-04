//! `daemon.json` / `daemon.lock` / `daemon.pid` under `registry_dir()`
//! (PLAN-phase2 §2.11). The flock on `daemon.lock` is THE liveness oracle
//! (jcode `socket.rs:89-139`): a daemon is alive iff someone holds the lock.
//! A socket file with no lock holder is stale and gets unlinked. Nobody
//! unlinks on ECONNREFUSED alone.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

pub use crate::events::registry::{daemon_paths, daemon_paths_in, DaemonPaths};

/// `daemon.json` — pid + profile + paths only. Never credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonInfo {
    pub pid: u32,
    pub protocol_version: u32,
    pub daemon_version: String,
    pub profile: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub socket: String,
    /// Executable to re-exec on `reload` (argv[0] canonicalised at first
    /// start; `--exe` overrides and is recorded). C3.
    #[serde(default)]
    pub exe: Option<std::path::PathBuf>,
    /// Reload counter (starts at 1; `Welcome.generation`). C3.
    #[serde(default = "one")]
    pub generation: u64,
}

fn one() -> u64 {
    1
}

#[cfg(unix)]
fn chmod_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn chmod_600(_path: &Path) {}

/// Write `daemon.json` + `daemon.pid` atomically (tmp + rename), 0600.
pub fn write_daemon_json(paths: &DaemonPaths, info: &DaemonInfo) -> io::Result<()> {
    let body = serde_json::to_vec_pretty(info).map_err(io::Error::other)?;
    write_atomic(&paths.json, &body)?;
    write_atomic(&paths.pid, format!("{}\n", info.pid).as_bytes())
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    chmod_600(&tmp);
    std::fs::rename(&tmp, path)
}

/// 0600 tmp + rename (reload-state).
pub fn write_private_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    write_atomic(path, body)
}

pub fn read_daemon_json(paths: &DaemonPaths) -> Option<DaemonInfo> {
    let s = std::fs::read_to_string(&paths.json).ok()?;
    serde_json::from_str(&s).ok()
}

/// Held for the daemon's lifetime; dropping releases the flock.
pub struct DaemonLock {
    file: File,
}

impl DaemonLock {
    /// `flock(LOCK_EX|LOCK_NB)` on `daemon.lock` (0600). `None` if another
    /// daemon holds it.
    pub fn try_acquire(paths: &DaemonPaths) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock)?;
        chmod_600(&paths.lock);
        if FileExt::try_lock_exclusive(&file)? {
            Ok(Some(Self { file }))
        } else {
            Ok(None)
        }
    }
}

impl DaemonLock {
    /// Adopt an already-locked fd inherited across `execv` (reload). The
    /// flock travels with the open file description, so the new image
    /// holds the SAME lock without a release/acquire gap.
    #[cfg(unix)]
    pub fn adopt(fd: i32) -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;
        if fd < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative lock fd"));
        }
        // SAFETY: the parent image cleared CLOEXEC on exactly this fd and
        // handed it to us via SYNAPS_DAEMON_LOCK_FD; we take ownership.
        let file = unsafe { File::from_raw_fd(fd) };
        // Restore CLOEXEC so tool subprocesses never inherit the lock.
        // SAFETY: fcntl on our own fd.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
        Ok(Self { file })
    }

    #[cfg(unix)]
    pub fn raw_fd(&self) -> i32 {
        use std::os::unix::io::AsRawFd;
        self.file.as_raw_fd()
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Liveness probe: try to take the lock; success means nobody holds it
/// (dead or never started) — release immediately. Never unlinks anything.
pub fn is_alive(paths: &DaemonPaths) -> bool {
    if !paths.lock.exists() {
        return false;
    }
    match DaemonLock::try_acquire(paths) {
        Ok(Some(_guard)) => false,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Unlink `sock`/`json`/`pid` if — and only if — no daemon holds the lock.
/// Returns `true` when something stale was removed.
pub fn reap_stale(paths: &DaemonPaths) -> bool {
    if is_alive(paths) {
        return false;
    }
    let mut reaped = false;
    for p in [&paths.sock, &paths.json, &paths.pid] {
        if p.exists() {
            crate::events::socket::cleanup_socket(&p.to_string_lossy());
            reaped |= !p.exists();
        }
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, DaemonPaths) {
        let d = tempfile::tempdir().unwrap();
        let p = daemon_paths_in(d.path(), None);
        (d, p)
    }

    #[test]
    fn lock_is_the_liveness_oracle() {
        let (_d, p) = paths();
        assert!(!is_alive(&p));
        let guard = DaemonLock::try_acquire(&p).unwrap().expect("first holder");
        assert!(is_alive(&p));
        assert!(DaemonLock::try_acquire(&p).unwrap().is_none(), "second holder refused");
        drop(guard);
        assert!(!is_alive(&p));
    }

    #[test]
    fn stale_files_reaped_only_without_holder() {
        let (_d, p) = paths();
        std::fs::write(&p.sock, b"").unwrap();
        let info = DaemonInfo {
            pid: 4_000_000,
            protocol_version: 1,
            daemon_version: "t".into(),
            profile: None,
            started_at: chrono::Utc::now(),
            socket: p.sock.to_string_lossy().into_owned(),
            exe: None,
            generation: 1,
        };
        write_daemon_json(&p, &info).unwrap();
        assert_eq!(read_daemon_json(&p).unwrap(), info);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p.json).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let guard = DaemonLock::try_acquire(&p).unwrap().unwrap();
        assert!(!reap_stale(&p), "live holder: nothing reaped");
        assert!(p.sock.exists());
        drop(guard);
        assert!(reap_stale(&p));
        assert!(!p.sock.exists() && !p.json.exists() && !p.pid.exists());
    }
}
