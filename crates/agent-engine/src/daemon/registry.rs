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

/// Write `daemon.json` + `daemon.pid` atomically (each through its own
/// temp file + rename), 0600.
pub fn write_daemon_json(paths: &DaemonPaths, info: &DaemonInfo) -> io::Result<()> {
    let body = serde_json::to_vec_pretty(info).map_err(io::Error::other)?;
    write_atomic(&paths.json, &body)?;
    write_atomic(&paths.pid, format!("{}\n", info.pid).as_bytes())
}

/// `<file name>.tmp` next to `path`: one temp file per target, so writes of
/// `daemon.json` and `daemon.pid` never share one.
fn temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Create `path` as a new private file: `O_CREAT | O_EXCL` (never opens an
/// existing file, and so never a symlink), `O_NOFOLLOW`, mode 0600 at
/// creation.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_private(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Exactly 0600, whatever the umask took away at creation.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}
#[cfg(not(unix))]
fn set_private(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Write `body` to `<path>.tmp`, created exclusively (a leftover or a
/// planted file there is unlinked first, never followed), 0600, then rename
/// it over `path`. The temp file is removed when anything fails.
fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = temp_path(path);
    let mut file = match create_private(&tmp) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            create_private(&tmp)?
        }
        opened => opened?,
    };
    let written = file.write_all(body).and_then(|()| set_private(&file));
    drop(file);
    #[cfg(test)]
    test_pause::at(&test_pause::BEFORE_RENAME);
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// 0600 temp file + rename (reload-state), as [`write_atomic`].
pub fn write_private_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    write_atomic(path, body)
}

pub fn read_daemon_json(paths: &DaemonPaths) -> Option<DaemonInfo> {
    let s = std::fs::read_to_string(&paths.json).ok()?;
    serde_json::from_str(&s).ok()
}

/// The lock holder puts its own `daemon.json` (and `daemon.pid`) back when
/// the file is missing, unreadable, or names another pid: an older `synaps`
/// that lists sessions unlinks every run-dir `*.json` it cannot parse as a
/// session card. `info` is the record the daemon holds in memory; the write
/// is [`write_daemon_json`]'s (tmp + rename, 0600). Anything at the path
/// that is not a regular file (a symlink, a directory, a socket) is left
/// alone, as [`cleanup_metadata`] does. Returns whether it wrote.
pub fn heal_daemon_json(paths: &DaemonPaths, info: &DaemonInfo, _lock: &DaemonLock) -> io::Result<bool> {
    match std::fs::symlink_metadata(&paths.json) {
        Ok(m) if !m.file_type().is_file() => return Ok(false),
        Ok(_) if read_daemon_json(paths).is_some_and(|on_disk| on_disk.pid == info.pid) => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    #[cfg(test)]
    test_pause::at(&test_pause::HEAL_BEFORE_WRITE);
    write_daemon_json(paths, info)?;
    Ok(true)
}

/// At most one `daemon.json rewritten` line per this interval; heals in
/// between are counted in the next line.
pub const JSON_HEAL_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// The daemon's `daemon.json` record as this image holds it, and the writes
/// of it: the start write, [`Self::publish`] (reload, and the restore after
/// a failed exec), [`Self::heal`] (Ping) and [`Self::cleanup`] (shutdown).
///
/// One metadata lock serializes them all: each changes the held record and
/// writes the files inside the same critical section, so a heal can never
/// write a record older than the one held, and after the cleanup nothing
/// writes the files back. Lock order: a caller that also holds the daemon's
/// `lock_fd` mutex (heal, cleanup) takes that first. The lock is never held
/// across an await.
pub struct Metadata {
    paths: DaemonPaths,
    inner: std::sync::Mutex<MetaInner>,
}

struct MetaInner {
    info: DaemonInfo,
    /// The heal's INFO-line rate limit: when it was last logged, and the
    /// heals since then.
    heals_logged_at: Option<std::time::Instant>,
    heals_since_log: u64,
    /// Shutdown removed the files: no later write puts them back.
    cleaned: bool,
}

/// What [`Metadata::heal`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Healed {
    /// The file was in place (or is not a regular file), or the metadata
    /// was cleaned up: nothing written.
    No,
    /// The record was written back. `log` is `Some(heals)` when this heal
    /// is due its INFO line (`heals` counted since the last line).
    Yes { info: DaemonInfo, log: Option<u64> },
}

impl Metadata {
    /// Hold `info` and write it (the start write).
    pub fn start(paths: DaemonPaths, info: DaemonInfo) -> io::Result<Self> {
        let meta = Self {
            paths,
            inner: std::sync::Mutex::new(MetaInner {
                info,
                heals_logged_at: None,
                heals_since_log: 0,
                cleaned: false,
            }),
        };
        {
            let inner = meta.inner();
            write_daemon_json(&meta.paths, &inner.info)?;
        }
        Ok(meta)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, MetaInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The record this image holds (a copy).
    pub fn info(&self) -> DaemonInfo {
        self.inner().info.clone()
    }

    /// Hold `info` and write it, under the metadata lock. After the cleanup
    /// it is only held.
    pub fn publish(&self, info: DaemonInfo) -> io::Result<()> {
        let mut inner = self.inner();
        inner.info = info;
        if inner.cleaned {
            return Ok(());
        }
        write_daemon_json(&self.paths, &inner.info)
    }

    /// [`heal_daemon_json`] with the held record, under the metadata lock;
    /// counts the heal for the INFO line's rate limit.
    pub fn heal(&self, lock: &DaemonLock) -> io::Result<Healed> {
        let mut inner = self.inner();
        if inner.cleaned || !heal_daemon_json(&self.paths, &inner.info, lock)? {
            return Ok(Healed::No);
        }
        inner.heals_since_log += 1;
        // `map_or`, not `is_none_or`: the crate's MSRV is 1.80.
        let log = if inner.heals_logged_at.is_none_or(|at| at.elapsed() >= JSON_HEAL_LOG_EVERY) {
            let heals = inner.heals_since_log;
            inner.heals_logged_at = Some(std::time::Instant::now());
            inner.heals_since_log = 0;
            Some(heals)
        } else {
            None
        };
        Ok(Healed::Yes { info: inner.info.clone(), log })
    }

    /// [`cleanup_metadata`] (shutdown), under the metadata lock; no later
    /// publish or heal writes the files.
    pub fn cleanup(&self, lock: &DaemonLock) -> bool {
        let mut inner = self.inner();
        inner.cleaned = true;
        cleanup_metadata(&self.paths, lock)
    }
}

/// Test-only pause points (thread-local, so a test pauses only its own
/// thread): a hook set here runs at that point of the write path.
#[cfg(test)]
pub(crate) mod test_pause {
    use std::cell::RefCell;
    use std::thread::LocalKey;

    pub(crate) type Hook = RefCell<Option<Box<dyn Fn()>>>;

    thread_local! {
        /// In [`super::heal_daemon_json`], after it found the file missing
        /// or foreign and before it writes the record it was given.
        pub(crate) static HEAL_BEFORE_WRITE: Hook = const { RefCell::new(None) };
        /// In `write_atomic`, after the temp file is written and before it
        /// is renamed into place.
        pub(crate) static BEFORE_RENAME: Hook = const { RefCell::new(None) };
    }

    pub(crate) fn set(slot: &'static LocalKey<Hook>, hook: impl Fn() + 'static) {
        slot.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn at(slot: &'static LocalKey<Hook>) {
        slot.with(|h| {
            if let Some(hook) = h.borrow().as_ref() {
                hook();
            }
        });
    }
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

impl DaemonPaths {
    /// `<stem>.spawn.lock` next to the flock: held by the one client that
    /// is spawning the daemon (jcode's spawn lock). Never the liveness oracle.
    pub fn spawn_lock(&self) -> std::path::PathBuf {
        self.lock.with_extension("spawn.lock")
    }
}

/// Held while a client spawns the daemon; dropping releases it.
#[derive(Debug)]
pub struct SpawnLock {
    file: File,
}

impl SpawnLock {
    /// `flock(LOCK_EX|LOCK_NB)` on the spawn lock, retried for up to `wait`
    /// (a loser waits for the winner's spawn to finish, then re-probes the
    /// daemon flock). `TimedOut` if the holder never lets go.
    pub fn acquire(paths: &DaemonPaths, wait: std::time::Duration) -> io::Result<Self> {
        let path = paths.spawn_lock();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        chmod_600(&path);
        let deadline = std::time::Instant::now() + wait;
        loop {
            if FileExt::try_lock_exclusive(&file)? {
                return Ok(Self { file });
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "another client is still spawning the daemon"));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for SpawnLock {
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

/// Remove only the daemon's two metadata files, while its lock is held.
/// These paths come from DaemonPaths, never from a session card or a suffix scan.
/// Refuse symlinks and special files (including directories and sockets).
pub(super) fn cleanup_metadata(paths: &DaemonPaths, _lock: &DaemonLock) -> bool {
    let mut removed = false;
    for path in [&paths.json, &paths.pid] {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
            match std::fs::remove_file(path) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "daemon: metadata cleanup failed"),
            }
        }
    }
    removed
}

/// Unlink `sock`/`json`/`pid` if — and only if — no daemon holds the lock.
/// Returns `true` when something stale was removed.
pub fn reap_stale(paths: &DaemonPaths) -> bool {
    // Retain the probe's lock through deletion: metadata cleanup requires ownership.
    let Ok(Some(lock)) = DaemonLock::try_acquire(paths) else {
        return false;
    };
    let had_socket = paths.sock.exists();
    crate::events::socket::cleanup_socket(&paths.sock.to_string_lossy());
    let reaped_socket = had_socket && !paths.sock.exists();
    cleanup_metadata(paths, &lock) || reaped_socket
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
    fn spawn_lock_is_exclusive_and_waits() {
        let (_d, p) = paths();
        let a = SpawnLock::acquire(&p, std::time::Duration::from_millis(10)).unwrap();
        let err = SpawnLock::acquire(&p, std::time::Duration::from_millis(120)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(a);
        SpawnLock::acquire(&p, std::time::Duration::from_millis(10)).unwrap();
        assert!(!is_alive(&p), "the spawn lock is not the liveness oracle");
    }

    #[test]
    fn metadata_cleanup_is_exact_and_requires_the_held_lock() {
        let (d, p) = paths();
        let other = daemon_paths_in(d.path(), Some("other"));
        for path in [&p.json, &p.pid, &other.json, &other.pid] {
            std::fs::write(path, b"metadata").unwrap();
        }
        let unrelated = d.path().join("session.json");
        let unrelated_pid = d.path().join("session.pid");
        std::fs::write(&unrelated, b"keep").unwrap();
        std::fs::write(&unrelated_pid, b"keep").unwrap();
        // A regular file at the socket path must not be removed either.
        std::fs::write(&p.sock, b"not a socket").unwrap();
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        assert!(cleanup_metadata(&p, &lock));
        assert!(!p.json.exists() && !p.pid.exists());
        assert!(is_alive(&p), "cleanup must not release or unlink the lock");
        assert!(!cleanup_metadata(&p, &lock), "idempotent");
        drop(lock);
        assert!(!reap_stale(&p), "regular socket impostor is preserved");
        for path in [&p.sock, &p.lock, &other.json, &other.pid, &unrelated, &unrelated_pid] {
            assert!(path.exists(), "preserve {}", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_cleanup_preserves_symlinks_and_special_files() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let (d, p) = paths();
        let target = d.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, &p.json).unwrap();
        symlink(d.path().join("missing"), &p.pid).unwrap();
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        assert!(!cleanup_metadata(&p, &lock));
        drop(lock);
        assert!(!reap_stale(&p));
        for path in [&p.json, &p.pid] {
            assert!(std::fs::symlink_metadata(path).unwrap().file_type().is_symlink());
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::create_dir(&p.json).unwrap();
        let _listener = UnixListener::bind(&p.pid).unwrap();
        assert!(!reap_stale(&p));
        assert!(p.json.is_dir() && p.pid.exists());
    }

    fn info_for(p: &DaemonPaths, pid: u32) -> DaemonInfo {
        DaemonInfo {
            pid,
            protocol_version: 3,
            daemon_version: "1.2.3".into(),
            profile: None,
            started_at: chrono::Utc::now(),
            socket: p.sock.to_string_lossy().into_owned(),
            exe: Some("/opt/synaps".into()),
            generation: 4,
        }
    }

    #[test]
    fn the_lock_holder_heals_a_missing_or_foreign_daemon_json() {
        let (_d, p) = paths();
        let ours = info_for(&p, 4_000_001);
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        // Missing: written, with daemon.pid, 0600.
        assert!(heal_daemon_json(&p, &ours, &lock).unwrap());
        assert_eq!(read_daemon_json(&p), Some(ours.clone()));
        assert_eq!(std::fs::read_to_string(&p.pid).unwrap(), "4000001\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p.json).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Ours already: no write (same inode).
        #[cfg(unix)]
        let ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&p.json).unwrap().ino()
        };
        assert!(!heal_daemon_json(&p, &ours, &lock).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&p.json).unwrap().ino(), ino);
        }
        // Another pid, or not a DaemonInfo: replaced.
        write_daemon_json(&p, &info_for(&p, 4_000_002)).unwrap();
        assert!(heal_daemon_json(&p, &ours, &lock).unwrap());
        assert_eq!(read_daemon_json(&p), Some(ours.clone()));
        std::fs::write(&p.json, b"not json").unwrap();
        assert!(heal_daemon_json(&p, &ours, &lock).unwrap());
        assert_eq!(read_daemon_json(&p), Some(ours.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn the_heal_leaves_symlinks_and_directories_alone() {
        let (d, p) = paths();
        let ours = info_for(&p, 4_000_003);
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        let target = d.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &p.json).unwrap();
        assert!(!heal_daemon_json(&p, &ours, &lock).unwrap());
        assert!(std::fs::symlink_metadata(&p.json).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_file(&p.json).unwrap();
        std::fs::create_dir(&p.json).unwrap();
        assert!(!heal_daemon_json(&p, &ours, &lock).unwrap());
        assert!(p.json.is_dir());
    }

    /// Wait (at most 5 s) until `n` threads have arrived here.
    fn rendezvous(n: usize) -> impl Fn() + Clone + Send + Sync + 'static {
        let seen = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
        move || {
            let (count, cv) = &*seen;
            let mut c = count.lock().unwrap();
            *c += 1;
            cv.notify_all();
            let _ = cv.wait_timeout_while(c, std::time::Duration::from_secs(5), |c| *c < n).unwrap();
        }
    }

    /// A heal that took the held record, found the file missing and then
    /// stalls before its write (a pause point) while a reload publishes a newer
    /// record must not write its older copy over the newer one: the newer
    /// record ends on disk and in memory.
    #[test]
    fn a_heal_paused_after_its_snapshot_never_writes_over_a_newer_record() {
        let (_d, p) = paths();
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        let old = info_for(&p, 4_000_010);
        let meta = Metadata::start(p.clone(), old.clone()).unwrap();
        std::fs::remove_file(&p.json).unwrap(); // so the heal writes
        let new = DaemonInfo { generation: old.generation + 1, exe: Some("/opt/synaps-new".into()), ..old.clone() };
        let (snapped_tx, snapped_rx) = std::sync::mpsc::channel::<()>();
        let (published_tx, published_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|s| {
            let healer = s.spawn(|| {
                test_pause::set(&test_pause::HEAL_BEFORE_WRITE, move || {
                    snapped_tx.send(()).unwrap();
                    // Held here until the publish is done, or 500 ms if the
                    // publish waits for this heal.
                    let _ = published_rx.recv_timeout(std::time::Duration::from_millis(500));
                });
                meta.heal(&lock).unwrap()
            });
            snapped_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the heal is about to write");
            meta.publish(new.clone()).unwrap();
            let _ = published_tx.send(());
            assert!(matches!(healer.join().unwrap(), Healed::Yes { .. }));
        });
        assert_eq!(read_daemon_json(&p), Some(new.clone()), "on disk: the newer record");
        assert_eq!(meta.info(), new, "in memory: the newer record");
    }

    /// `daemon.json` and `daemon.pid` written at the same moment (both temp
    /// files written before either is renamed) each end with their own
    /// contents: the two writes never share a temp file.
    #[test]
    fn daemon_json_and_daemon_pid_written_at_once_keep_their_own_contents() {
        let (_d, p) = paths();
        let meet = rendezvous(2);
        let results: Vec<io::Result<()>> = std::thread::scope(|s| {
            let a = s.spawn(|| {
                test_pause::set(&test_pause::BEFORE_RENAME, meet.clone());
                write_atomic(&p.json, b"{\"the\":\"json\"}")
            });
            let b = s.spawn(|| {
                test_pause::set(&test_pause::BEFORE_RENAME, meet.clone());
                write_atomic(&p.pid, b"4000011\n")
            });
            vec![a.join().unwrap(), b.join().unwrap()]
        });
        for r in &results {
            assert!(r.is_ok(), "{results:?}");
        }
        assert_eq!(std::fs::read_to_string(&p.json).unwrap(), "{\"the\":\"json\"}");
        assert_eq!(std::fs::read_to_string(&p.pid).unwrap(), "4000011\n");
    }

    /// A symlink planted where a temp file goes is never followed: the
    /// write replaces the link, the file it points at keeps its contents,
    /// and the written files are 0600.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_a_temp_path_is_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let (d, p) = paths();
        let target = d.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        // The shared temp name of earlier versions and the per-file names.
        for name in ["daemon.tmp", "daemon.json.tmp", "daemon.pid.tmp"] {
            std::os::unix::fs::symlink(&target, d.path().join(name)).unwrap();
        }
        let ours = info_for(&p, 4_000_012);
        write_daemon_json(&p, &ours).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert_eq!(read_daemon_json(&p), Some(ours));
        assert_eq!(std::fs::read_to_string(&p.pid).unwrap(), "4000012\n");
        for path in [&p.json, &p.pid] {
            assert!(std::fs::symlink_metadata(path).unwrap().file_type().is_file());
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    /// After shutdown's cleanup no later write puts the files back: neither
    /// a publish (a reload racing the shutdown) nor a heal.
    #[test]
    fn nothing_writes_the_metadata_back_after_cleanup() {
        let (_d, p) = paths();
        let lock = DaemonLock::try_acquire(&p).unwrap().unwrap();
        let ours = info_for(&p, 4_000_013);
        let meta = Metadata::start(p.clone(), ours.clone()).unwrap();
        assert!(meta.cleanup(&lock));
        let _ = meta.publish(DaemonInfo { generation: 9, ..ours });
        assert_eq!(meta.heal(&lock).unwrap(), Healed::No);
        assert!(!p.json.exists() && !p.pid.exists());
    }

    #[test]
    fn stale_files_reaped_only_without_holder() {
        let (_d, p) = paths();
        #[cfg(unix)]
        let _listener = std::os::unix::net::UnixListener::bind(&p.sock).unwrap();
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
        #[cfg(unix)]
        assert!(p.sock.exists());
        assert!(p.json.exists() && p.pid.exists());
        drop(guard);
        assert!(reap_stale(&p));
        assert!(!p.sock.exists() && !p.json.exists() && !p.pid.exists());
    }
}
