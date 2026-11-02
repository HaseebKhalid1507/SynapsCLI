//! Worker record publication and run ownership
//! (`docs/specs/subagent-context-rollover.md` §3.2–3.3, slice 4 durability).
//!
//! [`publish`] writes one record file the only way a reader may trust:
//! a fresh private temp file in the process directory, `fsync`, `rename`
//! over the record name, then `fsync` of the directory, all relative to the
//! directory handle without following links. Its [`Publication`] keeps the
//! three outcomes apart: durable; proven not published (failure before the
//! rename); and uncertain (the rename or the directory sync failed, so the
//! new file may or may not be, or stay, on disk). [`RecordDir::open_or_create`]
//! syncs the root after creating (or finding) the process directory, so its
//! name is durable before anything is published into it; the root and its
//! ancestors are host-configured and assumed durable already. Temp files are
//! removed on failure, best effort: a crash (or a failed unlink) can leave
//! `.tmp-*` residue, which readers ignore and directory-age pruning removes.
//!
//! [`RunOwnership`] is a live run's exclusive claim: an in-process lock keyed
//! by run ID plus `<handle>.lock` in the process directory, held with
//! `flock` for as long as the owner (and every persistence task it started)
//! lives. [`writer_probe`] (resume) and [`dir_has_live_writer`] (pruning)
//! test that lock; the kernel drops it when the process ends, so after a
//! crash no writer of that process is assumed. Lock files must be private
//! (no group/other bits), single-linked regular files of this user; any
//! other lock file fails closed. Every lock is released with an explicit
//! `LOCK_UN` before its descriptor closes: a child forked meanwhile shares
//! the open file description until it execs (`O_CLOEXEC` applies only at
//! exec), and its copy would otherwise keep the lock held. That cannot help
//! when this process dies abruptly while such a child is still before its
//! exec: the lock then stays held until the child execs or exits, and
//! resume and pruning treat the run as live meanwhile (fail closed).
//!
//! A [`RecordDir`] keeps the root's canonical pathname from when it was
//! opened and writes through its open process-directory handle; hosts must
//! not relocate or replace an active root (§3.3 "Root relocation"): after a
//! move, the locations it reports no longer reach what it wrote.
//!
//! A probe takes a shared lock for an instant: an acquire racing with it
//! fails with [`OwnershipError::LockHeld`] (fail closed: never two owners).
//! The root's lifecycle lock (`.lifecycle.lock`) is never waited for
//! unboundedly. Pruning tries it exclusively once, without waiting
//! ([`try_lifecycle_exclusive`]), and holds it for its whole scan and
//! delete; when it is held elsewhere, nothing is pruned.
//! [`RunOwnership::acquire`] polls it shared without blocking for at most
//! [`STARTUP_WAIT`] (cancel-aware; a blocking call, for a blocking worker),
//! fails with [`OwnershipError::LifecycleBusy`] at the deadline (never
//! starting without it), and then checks that its process directory is
//! still the one named in the root, so no owner starts in a directory
//! pruning is removing. A lifecycle lock file that is not a private,
//! single-linked regular file of this user is refused and never repaired:
//! pruning warns and prunes nothing, ownership fails (`Io`); the host
//! repairs it by removing the file while no process uses the root. v1 users
//! get the lock file (0600) on the first use of the root already. Unix only; on other
//! platforms no v2 writer exists, ownership cannot be taken, a probe reports
//! a possible writer, and pruning keeps its legacy behaviour. Worker
//! rollover is enabled since slice 9: a v2 run publishes v2 records and
//! takes ownership.

// Off Unix the tests are compiled out too (`mod tests` below is Unix only).
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::checkpoint::{PendingOp, PublicationState};
use super::record::{RecordKind, RunId, WriterState};
use super::record_file::{RecordFileError, RecordLocation};

/// What one publication achieved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publication {
    /// Renamed into place and the directory synced.
    Durable,
    /// Failed before the rename: the record name was not touched.
    NotPublished(String),
    /// The rename or the directory sync failed: the new contents may or may
    /// not be (or stay) under the record name.
    Uncertain(String),
}

/// The file operations of a publication, relative to one process directory.
/// The real implementation is [`RecordDir`]; tests wrap it to inject faults
/// between the steps.
pub trait RecordFs: Send + Sync {
    /// Create a fresh private temp file, write `bytes` and `fsync` it;
    /// returns its name. On error its temp file is removed, best effort.
    fn write_temp(&self, bytes: &[u8]) -> std::io::Result<String>;
    /// Rename `from` over `to` (both in the directory).
    fn rename(&self, from: &str, to: &str) -> std::io::Result<()>;
    /// `fsync` the directory.
    fn sync_dir(&self) -> std::io::Result<()>;
    /// Remove `name`; absent is not an error.
    fn remove(&self, name: &str) -> std::io::Result<()>;
    /// Where `handle`'s record of `kind` is read back from, without
    /// filesystem access (callers may be on the executor).
    fn location(&self, handle: &str, kind: RecordKind) -> Result<RecordLocation, RecordFileError>;
    /// The open directory, for ownership locks.
    fn dir(&self) -> &RecordDir;
}

/// Write `bytes` under `name`: temp, sync, rename, directory sync.
pub fn publish(fs: &dyn RecordFs, name: &str, bytes: &[u8]) -> Publication {
    let temp = match fs.write_temp(bytes) {
        Ok(temp) => temp,
        Err(e) => return Publication::NotPublished(format!("temp record: {e}")),
    };
    if let Err(e) = fs.rename(&temp, name) {
        // Most rename errors leave the target untouched, but not all of
        // them prove it (e.g. EIO): the caller re-reads before trusting.
        let _ = fs.remove(&temp);
        return Publication::Uncertain(format!("rename: {e}"));
    }
    match fs.sync_dir() {
        Ok(()) => Publication::Durable,
        Err(e) => Publication::Uncertain(format!("directory sync: {e}")),
    }
}

/// A process directory below the trusted root, open without following
/// links and created private (0700) if absent.
pub struct RecordDir {
    root: PathBuf,
    process_dir: String,
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

impl std::fmt::Debug for RecordDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordDir")
            .field("root", &self.root)
            .field("process_dir", &self.process_dir)
            .finish()
    }
}

impl RecordDir {
    /// Open (creating if needed) `process_dir` below `root`. The root must
    /// exist and be private to this user like the reader requires.
    pub fn open_or_create(root: &Path, process_dir: &str) -> Result<Self, RecordFileError> {
        // Validates the directory name and canonicalizes the root.
        let probe = super::record_file::location_of(root, process_dir, "sa_1", RecordKind::Final)?;
        #[cfg(unix)]
        {
            let fd = unix::open_or_create_dir(probe.root(), process_dir)?;
            Ok(Self {
                root: probe.root().to_path_buf(),
                process_dir: process_dir.to_owned(),
                fd,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = probe;
            Err(RecordFileError::Unsupported)
        }
    }
}

#[cfg(unix)]
impl RecordFs for RecordDir {
    fn write_temp(&self, bytes: &[u8]) -> std::io::Result<String> {
        unix::write_temp(&self.fd, bytes)
    }
    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        unix::rename_at(&self.fd, from, to)
    }
    fn sync_dir(&self) -> std::io::Result<()> {
        unix::sync_fd(&self.fd)
    }
    fn remove(&self, name: &str) -> std::io::Result<()> {
        unix::remove_at(&self.fd, name)
    }
    fn location(&self, handle: &str, kind: RecordKind) -> Result<RecordLocation, RecordFileError> {
        // The root was canonicalized when the directory was opened (on a
        // blocking worker); this touches no filesystem.
        super::record_file::location_in(&self.root, &self.process_dir, handle, kind)
    }
    fn dir(&self) -> &RecordDir {
        self
    }
}

/// The lock file name of `handle`'s run.
pub fn lock_file_name(handle: &str) -> String {
    format!("{handle}.lock")
}

/// The non-authoritative report name: evidence the owner could not commit.
pub fn report_file_name(handle: &str) -> String {
    format!("{handle}.report.json")
}

/// How long [`RunOwnership::acquire`] waits while pruning holds the root's
/// lifecycle lock.
pub const STARTUP_WAIT: Duration = Duration::from_secs(2);
/// The interval of its non-blocking attempts.
const LIFECYCLE_POLL: Duration = Duration::from_millis(10);

/// Why ownership could not be taken. Nothing was claimed in any case; the
/// drive runs such a worker with rollover off (slice 4 wiring).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnershipError {
    /// This process already owns the run.
    AlreadyOwned,
    /// Another owner (in this or another process) holds the run's lock.
    LockHeld,
    /// Pruning held the root's lifecycle lock past the startup deadline.
    LifecycleBusy,
    /// Cancelled while waiting for the root's lifecycle lock.
    Cancelled,
    Io(String),
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyOwned => f.write_str("the run is already owned in this process"),
            Self::LockHeld => f.write_str("another writer holds the run's lock"),
            Self::LifecycleBusy => f.write_str(
                "pruning held the history root's lifecycle lock past the startup deadline",
            ),
            Self::Cancelled => {
                f.write_str("cancelled while waiting for the history root's lifecycle lock")
            }
            Self::Io(e) => write!(f, "run lock: {e}"),
        }
    }
}

impl std::error::Error for OwnershipError {}

/// The runs this process owns (the writer table), keyed by run ID: an entry
/// lives exactly as long as its [`RunOwnership`] (and so every persistence
/// task holding it), and says where the run's publications stand, for a
/// daemon reload's check (spec §5).
fn owned_runs() -> &'static std::sync::Mutex<std::collections::HashMap<RunId, WriterEntry>> {
    static OWNED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<RunId, WriterEntry>>,
    > = std::sync::OnceLock::new();
    OWNED.get_or_init(Default::default)
}

/// A run this process owns, as a daemon reload sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterEntry {
    pub run_id: RunId,
    pub handle: String,
    /// The conversation that launched the run; `None` until labelled
    /// ([`RunOwnership::label_session`]).
    pub session: Option<String>,
    /// The generation of the run's last durably committed head.
    pub committed: Option<u64>,
    pub state: PublicationState,
}

/// Whether a reload may go ahead with a run this process owns (spec §5):
/// it may when the run has a durable record and no write of it is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterVerdict {
    /// A durable record (a committed head, or the final) and no record
    /// write in flight; a pending report or head removal (after a durable
    /// final) is not a record write.
    Durable,
    /// A head, republication or final is being written.
    Publishing,
    /// The spawn head is not committed yet: the launch is still deciding it.
    Starting,
    /// The disk head matches neither the committed head nor the attempted
    /// one: the run has no record this process can vouch for.
    Unresolved,
}

impl WriterVerdict {
    /// Why the run stops a normal reload (`None`: it does not).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Durable => None,
            Self::Publishing => Some("a record write is still in flight"),
            Self::Starting => Some("its spawn record is not written yet"),
            Self::Unresolved => Some("no durable record: its publication is unresolved"),
        }
    }

    /// The machine-readable reason (`--json`'s `why`).
    pub fn code(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Publishing => "publishing",
            Self::Starting => "starting",
            Self::Unresolved => "unresolved",
        }
    }
}

impl WriterEntry {
    pub fn verdict(&self) -> WriterVerdict {
        match &self.state {
            PublicationState::Pending(
                PendingOp::Head(_) | PendingOp::Republish(_) | PendingOp::Final,
            ) => WriterVerdict::Publishing,
            PublicationState::Unresolved => WriterVerdict::Unresolved,
            PublicationState::Final => WriterVerdict::Durable,
            // A head is removed only after a durable final, so a run with a
            // committed head keeps a record on disk whatever its final's
            // fate; an uncertain later head left either it or its successor.
            _ if self.committed.is_some() => WriterVerdict::Durable,
            // Only a launch is ever without a committed head: it commits the
            // spawn head or gives the run up (`WorkerPersistence::begin`).
            _ => WriterVerdict::Starting,
        }
    }
}

fn waived_handles() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static WAIVED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    WAIVED.get_or_init(Default::default)
}

/// A forced reload goes ahead without these handles' records (spec §5):
/// the checkpoint that carries them says so in their wake
/// (`CarriedWorker::record_waived`). Replaces any earlier set.
pub fn mark_waived(handles: impl IntoIterator<Item = String>) {
    *waived_handles().lock().unwrap_or_else(|p| p.into_inner()) = handles.into_iter().collect();
}

/// Whether the reload under way waived `handle`'s record.
pub fn record_waived(handle: &str) -> bool {
    waived_handles()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(handle)
}

/// Every run this process owns, by handle.
pub fn writers_snapshot() -> Vec<WriterEntry> {
    let mut entries: Vec<WriterEntry> = owned_runs()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    entries.sort_by(|a, b| (&a.handle, &a.run_id).cmp(&(&b.handle, &b.run_id)));
    entries
}

/// Test seam (`testing` builds and unit tests only): an entry in the writer
/// table for no real run, removed when dropped.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
pub struct TestWriter(RunId);

#[cfg(any(test, feature = "testing"))]
impl TestWriter {
    pub fn hold(handle: &str, session: Option<&str>, committed: Option<u64>, state: PublicationState) -> Self {
        let run_id = RunId::random();
        owned_runs().lock().unwrap_or_else(|p| p.into_inner()).insert(
            run_id.clone(),
            WriterEntry {
                run_id: run_id.clone(),
                handle: handle.to_owned(),
                session: session.map(str::to_owned),
                committed,
                state,
            },
        );
        Self(run_id)
    }
}

#[cfg(any(test, feature = "testing"))]
impl Drop for TestWriter {
    fn drop(&mut self) {
        owned_runs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// A live run's exclusive claim; released when dropped. Persistence tasks
/// hold it through an `Arc`, so it outlives an owner that stopped waiting.
#[derive(Debug)]
pub struct RunOwnership {
    run_id: RunId,
    handle: String,
    /// Released first on drop (explicit unlock), so the lock file is free
    /// before the run ID is.
    #[cfg(unix)]
    lock: Option<unix::LockGuard>,
}

impl RunOwnership {
    /// Claim `run_id` (in-process) and `<handle>.lock` in `dir` (`flock`,
    /// non-blocking). Fails without side effects if either is taken, or if
    /// `dir` is no longer the process directory named in its root. While
    /// pruning holds the root's lifecycle lock it waits up to
    /// [`STARTUP_WAIT`] and then fails with
    /// [`OwnershipError::LifecycleBusy`]: a blocking call, for a blocking
    /// worker (`spawn_blocking`), never an async executor thread.
    pub fn acquire(dir: &RecordDir, handle: &str, run_id: &RunId) -> Result<Self, OwnershipError> {
        Self::acquire_within(dir, handle, run_id, STARTUP_WAIT, &|| false)
    }

    /// [`Self::acquire`] waiting at most `wait` for the lifecycle lock, by
    /// non-blocking attempts on a monotonic clock. The budget counts from
    /// the call; the first attempt is made at once and every later one only
    /// before the deadline (an interrupted attempt goes back through the
    /// checks). `cancelled` is asked before each attempt; once it says so
    /// the wait ends with [`OwnershipError::Cancelled`].
    pub fn acquire_within(
        dir: &RecordDir,
        handle: &str,
        run_id: &RunId,
        wait: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, OwnershipError> {
        // The budget counts from the call, opening the lock included.
        let deadline = std::time::Instant::now() + wait;
        if !super::record::is_valid_handle_id(handle) {
            return Err(OwnershipError::Io("invalid handle".into()));
        }
        #[cfg(unix)]
        {
            // Shared against pruning until the run lock is held.
            let _startup = unix::startup_lock(dir, deadline, cancelled)?;
            let mut owned = owned_runs().lock().unwrap_or_else(|p| p.into_inner());
            if owned.contains_key(run_id) {
                return Err(OwnershipError::AlreadyOwned);
            }
            let lock = unix::lock_exclusive(&dir.fd, &lock_file_name(handle))?;
            owned.insert(
                run_id.clone(),
                WriterEntry {
                    run_id: run_id.clone(),
                    handle: handle.to_owned(),
                    session: None,
                    committed: None,
                    state: PublicationState::NoHead,
                },
            );
            Ok(Self {
                run_id: run_id.clone(),
                handle: handle.to_owned(),
                lock: Some(lock),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, run_id, deadline, cancelled);
            Err(OwnershipError::Io("worker records require Unix".into()))
        }
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn handle(&self) -> &str {
        &self.handle
    }

    /// Name the conversation that launched the run in its writer-table
    /// entry (a reload's refusal names it).
    pub fn label_session(&self, session: Option<&str>) {
        self.update(|entry| entry.session = session.map(str::to_owned));
    }

    /// Record where the run's publications stand in its writer-table entry
    /// (its checkpoint owner calls it on every change).
    pub fn note_state(&self, committed: Option<u64>, state: PublicationState) {
        self.update(|entry| {
            entry.committed = committed;
            entry.state = state;
        });
    }

    fn update(&self, change: impl FnOnce(&mut WriterEntry)) {
        if let Some(entry) = owned_runs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&self.run_id)
        {
            change(entry);
        }
    }
}

impl Drop for RunOwnership {
    fn drop(&mut self) {
        // Unlock (explicitly, see `unix::LockGuard`) before releasing the
        // in-process entry: an in-process acquire that passes the entry
        // check then finds the lock file free instead of `LockHeld`.
        #[cfg(unix)]
        drop(self.lock.take());
        owned_runs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.run_id);
    }
}

/// Whether a writer may still publish the located run: its lock file is
/// held. Absent directory or lock file ⇒ quiescent; anything that cannot be
/// checked (a link, an unreadable lock) ⇒ may publish (fail closed).
pub fn writer_probe(location: &RecordLocation) -> WriterState {
    #[cfg(unix)]
    {
        unix::probe(location)
    }
    #[cfg(not(unix))]
    {
        let _ = location;
        WriterState::MayPublish
    }
}

/// Whether any run lock in the process directory at `path` is held, or
/// cannot be checked (fail closed: the directory is kept). For pruning.
/// Not Unix: false, keeping legacy pruning where no v2 writer can exist.
pub fn dir_has_live_writer(path: &Path) -> bool {
    #[cfg(unix)]
    {
        unix::dir_has_live_writer(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// One attempt at the root's lifecycle lock (pruning).
#[derive(Debug)]
pub enum LifecycleAttempt {
    /// Held exclusively until the guard drops.
    Held(LifecycleGuard),
    /// Held elsewhere (another pruning, or a starting owner).
    Busy,
    /// There is no history root (nothing to prune).
    NoRoot,
    /// The lock file cannot be used: refused, never repaired.
    Invalid(String),
    /// The root itself cannot be used: it cannot be opened (access,
    /// ownership), or it holds no lock file and forbids creating one.
    RootUnusable(String),
}

/// The root's lifecycle lock held exclusively; released (explicitly) on drop.
#[derive(Debug)]
pub struct LifecycleGuard {
    #[cfg(unix)]
    _lock: unix::LockGuard,
}

/// Try the lifecycle lock of the history `root` exclusively, once, without
/// waiting. Pruning holds it for its whole scan and delete. Not Unix: always
/// granted (legacy pruning).
pub fn try_lifecycle_exclusive(root: &Path) -> LifecycleAttempt {
    #[cfg(unix)]
    {
        unix::try_lifecycle_exclusive(root)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        LifecycleAttempt::Held(LifecycleGuard {})
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::CString;
    use std::io::Write as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::path::Path;
    use std::time::Instant;

    use std::os::unix::fs::MetadataExt as _;

    use super::super::record::WriterState;
    use super::super::record_file::unix::{euid, io, is_symlink_at, open_at, private_dir};
    use super::super::record_file::{RecordFileError, RecordLocation};
    use super::{lock_file_name, OwnershipError, RecordDir};

    /// The root's lifecycle lock file (pruning exclusive, startup shared).
    const LIFECYCLE_LOCK: &str = ".lifecycle.lock";

    /// A held `flock`, released with an explicit `LOCK_UN` before its
    /// descriptor closes: a child forked meanwhile shares the open file
    /// description until it execs, and closing our copy alone would leave
    /// the lock held through its copy. An unlock error is logged; the
    /// descriptor is closed either way, and the kernel drops the lock once
    /// every copy is closed.
    #[derive(Debug)]
    pub(super) struct LockGuard(OwnedFd);

    #[cfg(test)]
    impl LockGuard {
        /// Another descriptor of the same open file description (what a
        /// forked child would hold).
        pub(super) fn duplicate(&self) -> std::io::Result<OwnedFd> {
            self.0.try_clone()
        }
    }

    impl Drop for LockGuard {
        fn drop(&mut self) {
            // SAFETY: valid fd.
            if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } < 0 {
                tracing::warn!(
                    "worker record lock: unlock failed ({}); released once every copy of its descriptor is closed",
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    fn metadata_of(fd: &OwnedFd) -> std::io::Result<std::fs::Metadata> {
        std::fs::File::from(fd.try_clone()?).metadata()
    }

    /// A lock file this user may trust: a private, single-linked regular
    /// file of the effective UID.
    fn private_lock(meta: &std::fs::Metadata) -> bool {
        meta.is_file() && meta.uid() == euid() && meta.mode() & 0o077 == 0 && meta.nlink() == 1
    }

    /// Open (creating, 0600) the lock file `name` in `dir`, no-follow,
    /// close-on-exec, and check it is private.
    fn open_lock(dir: &OwnedFd, name: &str) -> std::io::Result<OwnedFd> {
        let c = cstring(name)?;
        // SAFETY: valid directory fd and NUL-terminated name; owned once.
        let raw = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC
                    | libc::O_NOCTTY,
                0o600 as libc::c_uint,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fresh descriptor owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if !private_lock(&metadata_of(&fd)?) {
            return Err(std::io::Error::other(
                "the lock file is not a private, single-linked regular file",
            ));
        }
        Ok(fd)
    }

    /// After a failed open: is there an entry `name` in `dir`, of any type
    /// (not followed)? Classification only; nothing is opened.
    fn exists_at(dir: &OwnedFd, name: &str) -> bool {
        let Ok(c) = cstring(name) else {
            return false;
        };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid fd, NUL-terminated name and a writable stat buffer.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        rc == 0
    }

    /// The outcome of one non-blocking `flock` attempt.
    enum Try {
        Held,
        /// Held elsewhere.
        Busy,
        /// Interrupted (`EINTR`) before it was decided.
        Interrupted,
    }

    fn flock_once(fd: &OwnedFd, op: libc::c_int) -> std::io::Result<Try> {
        match flock(fd, op) {
            Ok(()) => Ok(Try::Held),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(Try::Busy),
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => Ok(Try::Interrupted),
            Err(e) => Err(e),
        }
    }

    /// One non-blocking `flock` attempt for pruning (`LOCK_EX`): `Ok(false)`
    /// when it is held elsewhere. An interrupted attempt is repeated; it
    /// never waits for the lock.
    fn try_flock(fd: &OwnedFd, op: libc::c_int) -> std::io::Result<bool> {
        loop {
            match flock_once(fd, op)? {
                Try::Held => return Ok(true),
                Try::Busy => return Ok(false),
                Try::Interrupted => continue,
            }
        }
    }

    pub(super) fn try_lifecycle_exclusive(root: &Path) -> super::LifecycleAttempt {
        use super::LifecycleAttempt as Attempt;
        let root = match std::fs::File::open(root) {
            Ok(root) => OwnedFd::from(root),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Attempt::NoRoot,
            Err(e) => return Attempt::RootUnusable(format!("history root: {e}")),
        };
        let fd = match open_lock(&root, LIFECYCLE_LOCK) {
            Ok(fd) => fd,
            // No entry by that name: the root forbids creating it (or
            // looking it up), so there is no lock file to blame or remove.
            Err(e) if !exists_at(&root, LIFECYCLE_LOCK) => {
                return Attempt::RootUnusable(format!(
                    "history root: cannot create {LIFECYCLE_LOCK}: {e}"
                ))
            }
            Err(e) => return Attempt::Invalid(format!("{LIFECYCLE_LOCK}: {e}")),
        };
        match try_flock(&fd, libc::LOCK_EX) {
            Ok(true) => Attempt::Held(super::LifecycleGuard {
                _lock: LockGuard(fd),
            }),
            Ok(false) => Attempt::Busy,
            Err(e) => Attempt::Invalid(format!("{LIFECYCLE_LOCK}: {e}")),
        }
    }

    /// The lifecycle lock held shared, taken by non-blocking attempts until
    /// `deadline`, once `dir` is still the process directory named in its
    /// root (pruning may have removed it before). The first attempt is made
    /// at once; every later one only before the deadline, so a late wake-up
    /// never acquires past it. `cancelled` is asked before every attempt,
    /// an interrupted one included.
    pub(super) fn startup_lock(
        dir: &RecordDir,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<LockGuard, OwnershipError> {
        let failed = |e: std::io::Error| OwnershipError::Io(e.to_string());
        let root = OwnedFd::from(std::fs::File::open(&dir.root).map_err(failed)?);
        let fd = open_lock(&root, LIFECYCLE_LOCK).map_err(failed)?;
        let mut first = true;
        let guard = loop {
            if cancelled() {
                return Err(OwnershipError::Cancelled);
            }
            if !std::mem::take(&mut first) && Instant::now() >= deadline {
                return Err(OwnershipError::LifecycleBusy);
            }
            match flock_once(&fd, libc::LOCK_SH).map_err(failed)? {
                Try::Held => break LockGuard(fd),
                // Back through the cancellation and deadline checks.
                Try::Interrupted => continue,
                Try::Busy => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(OwnershipError::LifecycleBusy);
            }
            std::thread::sleep(left.min(super::LIFECYCLE_POLL));
        };
        let named = open_at(
            &root,
            &dir.process_dir,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .and_then(|fd| metadata_of(&fd));
        let opened = metadata_of(&dir.fd).map_err(failed)?;
        match named {
            Ok(named) if (named.dev(), named.ino()) == (opened.dev(), opened.ino()) => Ok(guard),
            _ => Err(OwnershipError::Io(
                "the process directory was removed or replaced".into(),
            )),
        }
    }

    fn cstring(name: &str) -> std::io::Result<CString> {
        CString::new(name).map_err(std::io::Error::other)
    }

    fn check(rc: libc::c_int) -> std::io::Result<()> {
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn open_or_create_dir(
        root: &Path,
        process_dir: &str,
    ) -> Result<OwnedFd, RecordFileError> {
        let root_file = std::fs::File::open(root).map_err(io)?;
        private_dir(&root_file.metadata().map_err(io)?, 0o022)?;
        let root_fd = OwnedFd::from(root_file);
        let name = cstring(process_dir).map_err(io)?;
        // SAFETY: valid directory fd and NUL-terminated name.
        let rc = unsafe { libc::mkdirat(root_fd.as_raw_fd(), name.as_ptr(), 0o700) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(io(e));
            }
            #[cfg(test)]
            super::seams::op("mkdir (exists)");
        } else {
            #[cfg(test)]
            super::seams::op("mkdir");
        }
        let fd = match open_at(
            &root_fd,
            process_dir,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(fd) => fd,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                return Err(if is_symlink_at(&root_fd, process_dir) {
                    RecordFileError::Symlink
                } else {
                    RecordFileError::NotRegular
                });
            }
            Err(e) => return Err(io(e)),
        };
        #[cfg(test)]
        super::seams::op("open");
        let file = std::fs::File::from(fd);
        private_dir(&file.metadata().map_err(io)?, 0o077)?;
        // The directory's name is durable before anything is published in
        // it; also when found, as its creator may have died before this.
        sync_root(&root_fd).map_err(|e| RecordFileError::Io(format!("archive root sync: {e}")))?;
        Ok(OwnedFd::from(file))
    }

    fn sync_root(root: &OwnedFd) -> std::io::Result<()> {
        #[cfg(test)]
        {
            super::seams::op("sync root");
            if super::seams::root_sync_fails() {
                return Err(std::io::Error::other("injected root sync fault"));
            }
        }
        sync_fd(root)
    }

    pub(super) fn write_temp(dir: &OwnedFd, bytes: &[u8]) -> std::io::Result<String> {
        let name = format!(".tmp-{}", uuid::Uuid::new_v4().simple());
        let c = cstring(&name)?;
        // SAFETY: valid directory fd, NUL-terminated name; the fd is owned
        // exactly once below.
        let raw = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NOCTTY,
                0o600 as libc::c_uint,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor owned by nobody else.
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(e) = written {
            drop(file);
            let _ = remove_at(dir, &name);
            return Err(e);
        }
        Ok(name)
    }

    pub(super) fn rename_at(dir: &OwnedFd, from: &str, to: &str) -> std::io::Result<()> {
        let (from, to) = (cstring(from)?, cstring(to)?);
        // SAFETY: valid directory fd and NUL-terminated names.
        check(unsafe {
            libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr())
        })
    }

    pub(super) fn sync_fd(fd: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: valid fd.
        check(unsafe { libc::fsync(fd.as_raw_fd()) })
    }

    pub(super) fn remove_at(dir: &OwnedFd, name: &str) -> std::io::Result<()> {
        let c = cstring(name)?;
        // SAFETY: valid directory fd and NUL-terminated name.
        match check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) }) {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            other => other,
        }
    }

    fn flock(fd: &OwnedFd, op: libc::c_int) -> std::io::Result<()> {
        // SAFETY: valid fd.
        check(unsafe { libc::flock(fd.as_raw_fd(), op | libc::LOCK_NB) })
    }

    pub(super) fn lock_exclusive(dir: &OwnedFd, name: &str) -> Result<LockGuard, OwnershipError> {
        let fd = open_lock(dir, name).map_err(|e| OwnershipError::Io(e.to_string()))?;
        match flock(&fd, libc::LOCK_EX) {
            Ok(()) => Ok(LockGuard(fd)),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Err(OwnershipError::LockHeld),
            Err(e) => Err(OwnershipError::Io(e.to_string())),
        }
    }

    /// `Some(true)` held, `Some(false)` free, `None` cannot be checked.
    fn lock_held_at(dir: &OwnedFd, name: &str) -> Option<bool> {
        let fd = match open_at(
            dir,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Some(false),
            Err(_) => return None,
        };
        if !private_lock(&metadata_of(&fd).ok()?) {
            return None;
        }
        match flock(&fd, libc::LOCK_SH) {
            Ok(()) => {
                let shared = LockGuard(fd);
                #[cfg(test)]
                super::seams::probe_shared();
                drop(shared);
                Some(false)
            }
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Some(true),
            Err(_) => None,
        }
    }

    pub(super) fn probe(location: &RecordLocation) -> WriterState {
        let dir = match super::super::record_file::unix::open_process_dir(location) {
            Ok(Some(dir)) => dir,
            Ok(None) => return WriterState::Quiescent,
            Err(_) => return WriterState::MayPublish,
        };
        match lock_held_at(&dir, &lock_file_name(location.handle())) {
            Some(false) => WriterState::Quiescent,
            Some(true) | None => WriterState::MayPublish,
        }
    }

    pub(super) fn dir_has_live_writer(path: &Path) -> bool {
        let Ok(dir) = std::fs::File::open(path) else {
            return true;
        };
        let Ok(entries) = std::fs::read_dir(path) else {
            return true;
        };
        let dir = OwnedFd::from(dir);
        for entry in entries {
            let Ok(entry) = entry else {
                return true;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.ends_with(".lock") && lock_held_at(&dir, name) != Some(false) {
                return true;
            }
        }
        false
    }
}

/// Test-only step hooks (thread-local: they apply to calls on the setting
/// test's thread).
#[cfg(test)]
pub(crate) mod seams {
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    type Hook = Box<dyn FnMut()>;
    type PathHook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static PROBE_SHARED: RefCell<Option<Hook>> = RefCell::new(None);
        static PRUNE_DELETE: RefCell<Option<PathHook>> = RefCell::new(None);
        static OPS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        static FAIL_ROOT_SYNC: Cell<bool> = const { Cell::new(false) };
    }

    /// Run `hook` while a probe holds its shared lock.
    pub(crate) fn on_probe_shared(hook: Option<Hook>) {
        PROBE_SHARED.with(|h| *h.borrow_mut() = hook);
    }

    pub(super) fn probe_shared() {
        PROBE_SHARED.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook();
            }
        });
    }

    /// Run `hook` after pruning found a directory free and before it
    /// removes it.
    pub(crate) fn before_prune_delete(hook: Option<PathHook>) {
        PRUNE_DELETE.with(|h| *h.borrow_mut() = hook);
    }

    pub(in crate::tools::subagent) fn prune_delete(path: &Path) {
        PRUNE_DELETE.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(path);
            }
        });
    }

    /// The directory operations of `RecordDir::open_or_create` so far.
    pub(crate) fn take_ops() -> Vec<&'static str> {
        OPS.with(|o| std::mem::take(&mut *o.borrow_mut()))
    }

    pub(super) fn op(name: &'static str) {
        OPS.with(|o| o.borrow_mut().push(name));
    }

    /// Fail the next root sync.
    pub(crate) fn fail_root_sync() {
        FAIL_ROOT_SYNC.with(|f| f.set(true));
    }

    pub(super) fn root_sync_fails() -> bool {
        FAIL_ROOT_SYNC.with(|f| f.replace(false))
    }
}

#[cfg(all(test, unix))]
#[path = "record_store_tests.rs"]
mod tests;
