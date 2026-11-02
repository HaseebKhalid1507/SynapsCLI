use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::core::config::base_dir;

/// Discriminator written into every registration so the registry can tell
/// its own files from other `*.json` sharing the run dir (`daemon.json`,
/// tooling drops). Files missing it (older binaries) are treated as sessions.
pub const REGISTRATION_KIND: &str = "session";

fn default_kind() -> String {
    REGISTRATION_KIND.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRegistration {
    /// Always `"session"`; see [`REGISTRATION_KIND`].
    #[serde(default = "default_kind")]
    pub kind: String,
    pub session_id: String,
    pub name: Option<String>,
    pub socket_path: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    /// The receipt protocol version the endpoint answers with
    /// ([`super::socket::RECEIPTS_VERSION`]); 0 on cards written before
    /// receipts existed, whose endpoints close without a reply.
    #[serde(default)]
    pub receipts: u32,
    /// The registering process's start time (field 22 of its
    /// `/proc/<pid>/stat`, in clock ticks since boot), so a receiver can
    /// tell this process from a later one that reuses its pid. `None` on
    /// cards from older builds and where /proc is not readable; such a
    /// card never verifies a sender. Not part of the card's identity
    /// (socket name, owner check): those keep the fields they always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ticks: Option<u64>,
}

/// Returns the session runtime directory, creating it mode 0700 if needed.
///
/// `SYNAPS_RUNTIME_DIR` deliberately controls only ephemeral Unix sockets and
/// registration files. Persistent state and plugin discovery remain rooted at
/// `SYNAPS_BASE_DIR`; this avoids the 108-byte `sun_path` limit when the base
/// directory is an EFS session path.
pub fn registry_dir() -> PathBuf {
    let dir = run_dir_path();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("registry: failed to create run dir {:?}: {}", dir, e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// The session runtime directory's path, without creating it (see
/// [`registry_dir`]).
pub(crate) fn run_dir_path() -> PathBuf {
    std::env::var_os("SYNAPS_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| base_dir().join("run"))
}

/// Sanitize a session ID for safe use in filenames and socket paths.
/// Rejects path separators, `..`, and non-printable characters.
/// Returns the sanitized string (replaces unsafe chars with `_`).
pub fn sanitize_session_id(raw: &str) -> String {
    // Only allow alphanumeric, hyphens, and underscores. Dots are not needed
    // in session IDs (format is {name}-{timestamp}-{pid}) and allowing them
    // complicates path traversal prevention (single-pass ".." replace is
    // incomplete for "..." inputs).
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
}

/// Returns the legacy Unix socket path for a session.
/// Active endpoint clients must read `SessionRegistration::socket_path`: new
/// endpoints use bounded generation names, not this pre-publication helper.
/// Sockets live in the registry dir — `$SYNAPS_RUNTIME_DIR` when set (e.g.
/// `/run/user/<uid>/synaps`), otherwise `$SYNAPS_BASE_DIR/run` (default
/// `~/.synaps-cli/run/`) — which is user-owned and mode 0700, avoiding /tmp
/// symlink squatting and TOCTOU races.
pub fn socket_path_for_session(session_id: &str) -> String {
    socket_path_in_dir(&registry_dir(), session_id)
}

fn socket_path_in_dir(dir: &std::path::Path, session_id: &str) -> String {
    let safe_id = sanitize_session_id(session_id);
    dir.join(format!("{}.sock", safe_id))
        .to_string_lossy()
        .into_owned()
}

/// A permanent lock inode: never unlink this file, including during cleanup.
/// Bounded acquisition keeps Drop and synchronous boot fail-soft under contention.
pub(crate) fn lock_dir(dir: &std::path::Path) -> Result<std::fs::File, String> {
    lock_registry(dir, LockMode::Writer).map_err(|f| f.reason)
}

/// How [`lock_registry`] opens and holds the registry lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LockMode {
    /// Read-write, created when missing, held exclusive: may publish or
    /// prune cards.
    Writer,
    /// Read-only, never created, held shared: lists cards, changes nothing.
    /// For a process that may not write the run dir (a session under a
    /// write sandbox): flock needs no write access.
    Reader,
}

struct LockFailure {
    reason: String,
    /// A `Writer` open was refused for lack of write access (EACCES, EPERM
    /// or EROFS), as it is for a session under a write sandbox.
    write_denied: bool,
}

fn lock_registry(dir: &std::path::Path, mode: LockMode) -> Result<std::fs::File, LockFailure> {
    let fail = |reason: String| LockFailure { reason, write_denied: false };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if mode == LockMode::Writer {
        options.write(true).create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(dir.join(".session-registry.lock"))
        .map_err(|e| LockFailure {
            write_denied: mode == LockMode::Writer && is_write_denial(&e),
            reason: e.to_string(),
        })?;
    if !file.metadata().map_err(|e| fail(e.to_string()))?.is_file() {
        return Err(fail("registry lock is not a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().map_err(|e| fail(e.to_string()))?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.nlink() != 1
            || meta.mode() & 0o777 != 0o600
        {
            return Err(fail("unsafe registry lock owner, links or permissions".into()));
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        let locked = match mode {
            LockMode::Writer => fs4::fs_std::FileExt::try_lock_exclusive(&file),
            LockMode::Reader => fs4::fs_std::FileExt::try_lock_shared(&file),
        };
        if locked.map_err(|e| fail(e.to_string()))? {
            return Ok(file);
        }
        if std::time::Instant::now() >= deadline {
            return Err(fail("registry lock busy".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// An open refused for lack of write access rather than for any other reason.
fn is_write_denial(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        if e.raw_os_error() == Some(libc::EROFS) {
            return true;
        }
    }
    e.kind() == std::io::ErrorKind::PermissionDenied
}

fn same_owner(a: &SessionRegistration, b: &SessionRegistration) -> bool {
    a.session_id == b.session_id
        && a.pid == b.pid
        && a.started_at == b.started_at
        && a.socket_path == b.socket_path
}

fn generated_endpoint_missing(dir: &std::path::Path, reg: &SessionRegistration) -> bool {
    #[cfg(unix)]
    {
        let path = dir.join(generated_basename(reg));
        std::path::Path::new(&reg.socket_path) == path
            && std::fs::symlink_metadata(path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, reg);
        false
    }
}

fn generated_basename(reg: &SessionRegistration) -> String {
    use sha2::{Digest, Sha256};
    let identity = format!("{}\0{}\0{}", reg.session_id, reg.pid, reg.started_at);
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("s-{hex}.sock")
}

fn regular_card(path: &std::path::Path) -> bool {
    !is_reserved_json(path)
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

fn valid_card(path: &std::path::Path, reg: &SessionRegistration) -> bool {
    reg.kind == REGISTRATION_KIND
        && !is_reserved_json(path)
        && path.file_name()
            == Some(std::ffi::OsStr::new(&format!(
                "{}.json",
                sanitize_session_id(&reg.session_id)
            )))
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

fn remove_socket(dir: &std::path::Path, reg: &SessionRegistration) {
    let sock = std::path::Path::new(&reg.socket_path);
    // Exact immediate child only; never follow a claimed subdirectory/symlink.
    let legacy = format!("{}.sock", sanitize_session_id(&reg.session_id));
    if (sock == dir.join(generated_basename(reg)) || sock == dir.join(legacy))
        && sock.parent() == Some(dir)
    {
        super::socket::cleanup_socket(&reg.socket_path);
    }
}

/// Owns a bound, published generation and its captured runtime directory.
/// A timestamp/hash collision fails binding without replacing the prior socket.
/// Unix lock-contention leftovers are repaired on the next successful scan;
/// Windows card cleanup under contention requires retry or own-PID reload sweep.
/// No asynchronous task can bind after this guard is dropped.
pub struct SessionEndpoint {
    pub registration: SessionRegistration,
    dir: PathBuf,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    socket: super::socket::SocketIdentity,
}

impl SessionEndpoint {
    pub fn start(
        id: &str,
        name: Option<String>,
        queue: std::sync::Arc<super::queue::EventQueue>,
    ) -> Result<Self, String> {
        let dir = std::fs::canonicalize(registry_dir()).map_err(|e| e.to_string())?;
        Self::start_in(&dir, id, name, queue)
    }

    pub(crate) fn start_in(
        dir: &std::path::Path,
        id: &str,
        name: Option<String>,
        queue: std::sync::Arc<super::queue::EventQueue>,
    ) -> Result<Self, String> {
        let _lock = lock_dir(dir)?;
        scan_registrations_locked(dir, None);
        let mut registration = SessionRegistration {
            kind: REGISTRATION_KIND.into(),
            session_id: id.into(),
            name,
            socket_path: String::new(),
            pid: std::process::id(),
            started_at: Utc::now(),
            receipts: super::socket::RECEIPTS_VERSION,
            start_ticks: super::peer::own_start_ticks(),
        };
        registration.socket_path = dir
            .join(generated_basename(&registration))
            .to_string_lossy()
            .into_owned();
        let (listener, socket) = super::socket::bind_session_socket(&registration.socket_path)
            .map_err(|e| e.to_string())?;
        if let Err(e) = write_registration(&registration, dir) {
            socket.remove();
            return Err(e);
        }
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = super::socket::serve_session_socket(
            listener,
            registration.socket_path.clone(),
            dir.to_path_buf(),
            queue,
            shutdown.clone(),
        );
        Ok(Self {
            registration,
            dir: dir.to_path_buf(),
            shutdown,
            task,
            socket,
        })
    }

    /// Rename only this published generation in the captured directory.
    pub fn update_name(&self, name: Option<&str>) -> Result<(), String> {
        let _lock = lock_dir(&self.dir)?;
        let path = self.dir.join(format!(
            "{}.json",
            sanitize_session_id(&self.registration.session_id)
        ));
        if !regular_card(&path) {
            return Err("session card missing".into());
        }
        let mut disk: SessionRegistration =
            serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if !valid_card(&path, &disk) || !same_owner(&disk, &self.registration) {
            return Err("session generation replaced".into());
        }
        disk.name = name.map(str::to_owned);
        write_registration(&disk, &self.dir)
    }

    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.task.abort();
        // This unique generation cannot be reused by another successful bind.
        // Remove only the captured inode even if card cleanup cannot get the lock.
        self.socket.remove();
        let Ok(_lock) = lock_dir(&self.dir) else {
            // Unix scans repair missing generated endpoints, including live-PID
            // cards. Windows has no filesystem endpoint: card cleanup remains
            // best effort until explicit shutdown retry / own-PID reload sweep.
            return;
        };
        unregister_owned_locked(&self.registration, &self.dir, false);
    }
}

impl Drop for SessionEndpoint {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Write `{session_id}.json` atomically (tmp + rename). Chmod 0600 on Unix.
pub fn register_session(reg: &SessionRegistration) -> Result<(), String> {
    register_session_in(reg, &registry_dir())
}

fn register_session_in(reg: &SessionRegistration, dir: &std::path::Path) -> Result<(), String> {
    let _lock = lock_dir(dir)?;
    write_registration(reg, dir)
}

fn write_registration(reg: &SessionRegistration, dir: &std::path::Path) -> Result<(), String> {
    use std::io::Write;
    let safe_id = sanitize_session_id(&reg.session_id);
    let path = dir.join(format!("{}.json", safe_id));
    if is_reserved_json(&path) {
        return Err("reserved session id".into());
    }
    // Do not overwrite foreign JSON, symlinks or special files sharing run/.
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err("not a regular session card".into());
            }
            let old: SessionRegistration =
                serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if !valid_card(&path, &old) || old.session_id != reg.session_id {
                return Err("refusing to replace a foreign session card".into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let tmp = dir.join(format!(".session-{}.tmp", uuid::Uuid::new_v4().simple()));

    let json = serde_json::to_string(reg).map_err(|e| format!("serialize error: {}", e))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(json.as_bytes())?;
        drop(file);
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e: std::io::Error| e.to_string())?;

    Ok(())
}

/// Rewrite `{session_id}.json` with a new `name` (rename / `saveas`). The
/// rest of the record (socket, pid, started_at) is kept. Errors when there
/// is no registration to update.
pub fn update_session_name(session_id: &str, name: Option<&str>) -> Result<(), String> {
    update_session_name_in(session_id, name, &registry_dir())
}

fn update_session_name_in(
    session_id: &str,
    name: Option<&str>,
    dir: &std::path::Path,
) -> Result<(), String> {
    let _lock = lock_dir(dir)?;
    let safe_id = sanitize_session_id(session_id);
    let path = dir.join(format!("{}.json", safe_id));
    if !regular_card(&path) {
        return Err("not a regular session card".into());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut reg: SessionRegistration =
        serde_json::from_str(&content).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    if !valid_card(&path, &reg) {
        return Err("not a session card".into());
    }
    reg.name = name.map(str::to_string);
    write_registration(&reg, dir)
}

/// Operator removal by ID (not owner-scoped). No runtime teardown uses this.
/// Remove only a valid matching registration. Best-effort — never panics.
/// Also removes the socket file at `socket_path` if it exists.
pub fn unregister_session(session_id: &str) {
    unregister_session_in(session_id, &registry_dir());
}

fn unregister_session_in(session_id: &str, dir: &std::path::Path) {
    let Ok(_lock) = lock_dir(dir) else {
        return;
    };
    let path = dir.join(format!("{}.json", sanitize_session_id(session_id)));
    if !regular_card(&path) {
        return;
    }
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(reg) = serde_json::from_str::<SessionRegistration>(&content) {
            if valid_card(&path, &reg) {
                remove_registration_files(dir, &path, &reg);
            }
        }
    }
}

/// Remove `reg`'s registration and socket, but only while the file on disk
/// is still this exact registration (same owner pid, start time and socket).
///
/// This is what a session calls when it ends. The check matters because the
/// same id can be registered again while an older owner is still winding
/// down (a second actor on one id, a rehydrated session after a reload): the
/// old owner must not delete the newer registration. Best-effort and
/// idempotent: a missing or foreign file is left alone.
pub fn unregister_owned(reg: &SessionRegistration) {
    unregister_owned_in(reg, &registry_dir());
}

fn unregister_owned_in(reg: &SessionRegistration, dir: &std::path::Path) {
    let Ok(_lock) = lock_dir(dir) else {
        return;
    };
    unregister_owned_locked(reg, dir, true);
}

fn unregister_owned_locked(
    reg: &SessionRegistration,
    dir: &std::path::Path,
    remove_claimed_socket: bool,
) {
    let safe_id = sanitize_session_id(&reg.session_id);
    let path = dir.join(format!("{}.json", safe_id));
    if !regular_card(&path) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(on_disk) = serde_json::from_str::<SessionRegistration>(&content) else {
        return;
    };
    if !valid_card(&path, &on_disk)
        || on_disk.session_id != reg.session_id
        || on_disk.pid != reg.pid
        || on_disk.started_at != reg.started_at
        || on_disk.socket_path != reg.socket_path
    {
        tracing::debug!(session = %reg.session_id, "registry: registration was replaced; leaving the newer one");
        return;
    }
    if remove_claimed_socket {
        remove_socket(dir, &on_disk);
    }
    let _ = std::fs::remove_file(path);
}

/// Unlink a registration file and its socket. The socket is only removed
/// when it lies inside the registry dir, so a crafted file can never delete
/// anything elsewhere.
fn remove_registration_files(
    dir: &std::path::Path,
    path: &std::path::Path,
    reg: &SessionRegistration,
) {
    if !valid_card(path, reg) {
        return;
    }
    remove_socket(dir, reg);
    let _ = std::fs::remove_file(path);
}

/// Remove registrations that no running session can own and return how
/// many were removed. Always: an owner pid that no longer exists. With
/// `own_pid` (the daemon, at startup): also registrations naming that pid,
/// which can only be leftovers. A reload re-execs the same pid without
/// running any session's shutdown, and the new image has not registered
/// anything yet when it sweeps.
pub fn sweep_stale_registrations(own_pid: Option<u32>) -> usize {
    sweep_stale_registrations_in(&registry_dir(), own_pid)
}

fn sweep_stale_registrations_in(dir: &std::path::Path, own_pid: Option<u32>) -> usize {
    scan_registrations(dir, own_pid).1
}

/// Returns true if a process with `pid` is alive (Unix: `kill(pid, 0)`;
/// Windows: the process object is not yet signalled).
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 never sends a signal; it only checks
        // whether the process exists and we have permission to signal it.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        crate::daemon::win::pid_is_alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Files in the run dir that are never session registrations, whatever
/// their contents: the daemon's own `daemon.json` / `daemon-<profile>.json`
/// and its `daemon[-P].<kind>.json` state files (the reload state, the
/// crash record).
fn is_reserved_json(path: &std::path::Path) -> bool {
    path.file_stem().and_then(|s| s.to_str()).is_some_and(|stem| {
        stem == "daemon" || stem.starts_with("daemon-") || stem.starts_with("daemon.")
    })
}

/// Read all registration files, prune stale ones (dead PID), return live set.
///
/// Only files this registry wrote are ever unlinked: a `*.json` that is not
/// a `SessionRegistration` (parse failure, or `kind != "session"`) is logged
/// and skipped — `daemon.json` lives in the same dir and `synaps send` must
/// never destroy it.
pub fn list_active_sessions() -> Vec<SessionRegistration> {
    list_active_sessions_in(&registry_dir())
}

fn list_active_sessions_in(dir: &std::path::Path) -> Vec<SessionRegistration> {
    scan_registrations(dir, None).0
}

/// Read every session registration in `dir`, unlink the stale ones (dead
/// owner pid, or `own_pid` when given) and return `(live, removed)`.
///
/// A process that may not write `dir` (a session under a write sandbox:
/// `synaps send`, fleet tools) can't take the writer lock. It still gets the
/// live set, under the shared reader lock, and unlinks nothing.
fn scan_registrations(
    dir: &std::path::Path,
    own_pid: Option<u32>,
) -> (Vec<SessionRegistration>, usize) {
    match lock_registry(dir, LockMode::Writer) {
        Ok(_lock) => scan_registrations_locked(dir, own_pid),
        Err(f) if f.write_denied => match lock_registry(dir, LockMode::Reader) {
            Ok(_lock) => scan_registrations_read_only(dir, own_pid),
            Err(_) => (Vec::new(), 0),
        },
        Err(_) => (Vec::new(), 0),
    }
}

fn scan_registrations_locked(
    dir: &std::path::Path,
    own_pid: Option<u32>,
) -> (Vec<SessionRegistration>, usize) {
    scan_cards(dir, own_pid, true)
}

/// The live set without unlinking anything (the shared reader lock is held).
fn scan_registrations_read_only(
    dir: &std::path::Path,
    own_pid: Option<u32>,
) -> (Vec<SessionRegistration>, usize) {
    scan_cards(dir, own_pid, false)
}

fn scan_cards(
    dir: &std::path::Path,
    own_pid: Option<u32>,
    prune: bool,
) -> (Vec<SessionRegistration>, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), 0);
    };

    let mut live = Vec::new();
    let mut removed = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") || is_reserved_json(&path) {
            continue;
        }
        if !regular_card(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let reg = match serde_json::from_str::<SessionRegistration>(&content) {
            Ok(reg) if reg.kind == REGISTRATION_KIND => reg,
            Ok(reg) => {
                tracing::debug!(path = %path.display(), kind = %reg.kind, "registry: not a session registration — skipped");
                continue;
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "registry: unparseable json — skipped, not ours");
                continue;
            }
        };

        if !valid_card(&path, &reg) {
            continue;
        }
        if own_pid != Some(reg.pid)
            && pid_is_alive(reg.pid)
            && !generated_endpoint_missing(dir, &reg)
        {
            live.push(reg);
        } else if prune {
            remove_registration_files(dir, &path, &reg);
            removed += 1;
        }
    }

    (live, removed)
}

/// The most card files one receiver scan reads (see [`live_cards_in`]): a
/// run dir full of cards costs a bounded amount of reading.
pub(crate) const MAX_SCANNED_CARDS: usize = 256;

/// The largest card file a receiver scan reads; a bigger one is skipped,
/// never trusted. Real cards are a few hundred bytes.
pub(crate) const MAX_CARD_BYTES: u64 = 64 * 1024;

/// The cards a receiver scan read, and whether it read every card file in
/// the dir.
#[derive(Debug, Clone, Default)]
pub(crate) struct CardScan {
    pub(crate) cards: Vec<SessionRegistration>,
    /// False when the scan stopped at [`MAX_SCANNED_CARDS`], or a card file
    /// or the dir listing couldn't be read: a card it never saw may exist.
    pub(crate) complete: bool,
}

impl CardScan {
    /// A scan that saw every card: exactly `cards`.
    pub(crate) fn whole(cards: Vec<SessionRegistration>) -> Self {
        CardScan {
            cards,
            complete: true,
        }
    }
}

/// The live cards in `dir`, read without the registry lock and without
/// removing anything: what an event receiver matches a connecting process
/// against. Cards are replaced by rename, so each one read is whole.
///
/// Bounded: at most [`MAX_SCANNED_CARDS`] card files are opened (in
/// directory order, so beyond that some cards are never seen), and only
/// regular files of at most [`MAX_CARD_BYTES`] are read ([`read_card_bounded`]).
/// The scan says whether it saw every card file: not when it stopped at the
/// limit, or when the listing or a card file couldn't be read. A file that
/// is no card (another type, too large, not a card's JSON, gone before it was
/// opened) is skipped and leaves the scan complete.
pub(crate) fn live_cards_in(dir: &std::path::Path) -> CardScan {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CardScan::whole(Vec::new()),
        Err(_) => return CardScan::default(),
    };
    let mut scan = CardScan::whole(Vec::new());
    let mut opened = 0;
    for entry in entries {
        let Ok(entry) = entry else {
            scan.complete = false;
            break;
        };
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") || is_reserved_json(&path) {
            continue;
        }
        if opened == MAX_SCANNED_CARDS {
            scan.complete = false;
            break;
        }
        opened += 1;
        match read_card_bounded(&path) {
            CardRead::Card(reg) => {
                if valid_card(&path, &reg) && pid_is_alive(reg.pid) {
                    scan.cards.push(reg);
                }
            }
            CardRead::NotACard => {}
            CardRead::Unreadable => scan.complete = false,
        }
    }
    scan
}

/// What reading one card file gave.
enum CardRead {
    Card(SessionRegistration),
    /// No card: another file type, larger than [`MAX_CARD_BYTES`], not a
    /// card's JSON, or gone before it was opened.
    NotACard,
    /// The file couldn't be opened or read: it may be a card.
    Unreadable,
}

/// One card file, if it is a regular file (not followed through a symlink,
/// never blocking on a FIFO) of at most [`MAX_CARD_BYTES`] that parses. The
/// type and size come from the opened file itself, and at most one byte past
/// the limit is read, so a card that grows after the check is still skipped.
fn read_card_bounded(path: &std::path::Path) -> CardRead {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if !regular_card(path) {
        return CardRead::NotACard;
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CardRead::NotACard,
        // A symlink (O_NOFOLLOW) or a socket: no card.
        #[cfg(unix)]
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENXIO)) => {
            return CardRead::NotACard
        }
        Err(_) => return CardRead::Unreadable,
    };
    let Ok(meta) = file.metadata() else {
        return CardRead::Unreadable;
    };
    if !meta.is_file() || meta.len() > MAX_CARD_BYTES {
        return CardRead::NotACard;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    if file
        .take(MAX_CARD_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return CardRead::Unreadable;
    }
    if bytes.len() as u64 > MAX_CARD_BYTES {
        return CardRead::NotACard;
    }
    match serde_json::from_slice(&bytes) {
        Ok(reg) => CardRead::Card(reg),
        Err(_) => CardRead::NotACard,
    }
}

/// The longest card a strict send reads; real ones are a few hundred bytes.
const CARD_MAX: u64 = 64 * 1024;

/// A card's text, from one no-follow, non-blocking open of a regular file,
/// at most [`CARD_MAX`] bytes.
fn read_strict_card(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(CARD_MAX + 1).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= CARD_MAX).then_some(text)
}

/// A card value or query for a message: at most 80 characters, escaped.
fn shown(s: &str) -> String {
    s.chars()
        .take(80)
        .collect::<String>()
        .escape_debug()
        .to_string()
}

/// The one live session whose id or name is exactly `query`, for a strict
/// send ([`super::send`]). Read only: no lock, and nothing is pruned,
/// created or written.
///
/// Every card whose session id or name is `query` and whose owner is alive
/// counts; a card with a dead owner is only counted for the message. Exactly
/// one must count. Its socket must then be directly in the run dir (as the
/// card writes it, compared as a path), be a socket (Unix; a missing one is
/// left for the connect to report) and answer with receipts.
pub(crate) fn resolve_exact_in(
    query: &str,
    dir: &std::path::Path,
) -> Result<SessionRegistration, (super::send::Why, String)> {
    use super::send::Why;
    if query.is_empty() {
        return Err((
            Why::NoLiveSession,
            "no session has an empty id or name".into(),
        ));
    }
    let unreadable = |e: std::io::Error| {
        let why = if e.kind() == std::io::ErrorKind::NotFound {
            Why::NoLiveSession
        } else {
            Why::RegistryUnreadable
        };
        (
            why,
            format!("cannot read the session registry {}: {e}", dir.display()),
        )
    };
    let run_dir = std::fs::canonicalize(dir).map_err(unreadable)?;
    let entries = std::fs::read_dir(&run_dir).map_err(unreadable)?;

    let mut live = Vec::new();
    let mut stale = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") || is_reserved_json(&path) {
            continue;
        }
        let Some(text) = read_strict_card(&path) else {
            continue;
        };
        let Ok(reg) = serde_json::from_str::<SessionRegistration>(&text) else {
            continue;
        };
        if !valid_card(&path, &reg)
            || (reg.session_id != query && reg.name.as_deref() != Some(query))
        {
            continue;
        }
        if pid_is_alive(reg.pid) {
            live.push(reg);
        } else {
            stale += 1;
        }
    }

    let reg = match live.len() {
        0 => {
            let note = match stale {
                0 => String::new(),
                1 => " (1 stale registration)".to_string(),
                n => format!(" ({n} stale registrations)"),
            };
            return Err((
                Why::NoLiveSession,
                format!(
                    "no live session has the id or name \"{}\"{note}",
                    shown(query)
                ),
            ));
        }
        1 => live.remove(0),
        n => {
            let mut ids: Vec<String> = live.iter().map(|r| shown(&r.session_id)).collect();
            ids.sort();
            return Err((
                Why::Ambiguous,
                format!(
                    "{n} live sessions have the id or name \"{}\": {}",
                    shown(query),
                    ids.join(", ")
                ),
            ));
        }
    };

    let socket = std::path::Path::new(&reg.socket_path);
    if socket.parent() != Some(run_dir.as_path()) || socket.file_name().is_none() {
        return Err((
            Why::SocketOutsideRunDir,
            format!(
                "session {}'s socket \"{}\" is not in the run dir {}",
                shown(&reg.session_id),
                shown(&reg.socket_path),
                run_dir.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = std::fs::symlink_metadata(socket) {
            if !meta.file_type().is_socket() {
                return Err((
                    Why::NotASocket,
                    format!(
                        "session {}'s socket \"{}\" is not a socket",
                        shown(&reg.session_id),
                        shown(&reg.socket_path)
                    ),
                ));
            }
        }
    }
    if reg.receipts < super::socket::RECEIPTS_VERSION {
        return Err((
            Why::ReceiverTooOld,
            format!(
                "session {} runs a build without receipts, so a send to it can't be confirmed",
                shown(&reg.session_id)
            ),
        ));
    }
    Ok(reg)
}

/// Resolve a query to a registration. Resolution order:
/// 1. Exact session_id
/// 2. Name match
/// 3. Partial session_id prefix (unambiguous)
pub fn find_session_registration(query: &str) -> Option<SessionRegistration> {
    find_session_registration_in(query, &registry_dir())
}

fn find_session_registration_in(query: &str, dir: &std::path::Path) -> Option<SessionRegistration> {
    let sessions = list_active_sessions_in(dir);

    // 1. Exact ID
    if let Some(reg) = sessions.iter().find(|r| r.session_id == query) {
        return Some(reg.clone());
    }

    // 2. Name match
    if let Some(reg) = sessions.iter().find(|r| r.name.as_deref() == Some(query)) {
        return Some(reg.clone());
    }

    // 3. Partial prefix — only if unambiguous
    let matches: Vec<_> = sessions
        .iter()
        .filter(|r| r.session_id.starts_with(query))
        .collect();

    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    /// The card for `id` in `dir`, as JSON.
    fn card_json(dir: &std::path::Path, id: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn new_cards_advertise_the_receipt_protocol() {
        let tmp = tmp_registry();
        let _endpoint = SessionEndpoint::start_in(
            tmp.path(),
            "receipts",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        assert_eq!(card_json(tmp.path(), "receipts")["receipts"], 1);
    }

    #[tokio::test]
    async fn renaming_a_session_keeps_the_receipt_flag() {
        let tmp = tmp_registry();
        let endpoint = SessionEndpoint::start_in(
            tmp.path(),
            "renamed",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        endpoint.update_name(Some("new name")).unwrap();
        let card = card_json(tmp.path(), "renamed");
        assert_eq!(card["name"], "new name");
        assert_eq!(card["receipts"], 1);
    }

    // A card written by a build without receipts reads as version 0, so a
    // client that needs receipts can tell it apart.
    #[test]
    fn a_card_from_before_receipts_reads_as_version_0() {
        let old = r#"{"kind":"session","session_id":"old","name":null,"socket_path":"/x.sock","pid":1,"started_at":"2024-01-01T00:00:00Z"}"#;
        let reg: SessionRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_value(&reg).unwrap()["receipts"], 0);
    }

    /// This process's start time, read from /proc here rather than through
    /// the code under test: field 22 of /proc/self/stat.
    #[cfg(target_os = "linux")]
    fn start_ticks_from_proc() -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let after_comm = &stat[stat.rfind(')').unwrap() + 1..];
        after_comm.split_whitespace().nth(19).unwrap().parse().unwrap()
    }

    // A new card carries the registering process's start time, and a
    // rename keeps it: a receiver matches a pid only together with it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn new_cards_carry_this_processs_start_time_through_a_rename() {
        let tmp = tmp_registry();
        let endpoint = SessionEndpoint::start_in(
            tmp.path(),
            "started",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        let want = start_ticks_from_proc();
        assert_eq!(
            card_json(tmp.path(), "started")["start_ticks"],
            want,
            "a new card carries its process's start time"
        );
        endpoint.update_name(Some("renamed")).unwrap();
        let card = card_json(tmp.path(), "started");
        assert_eq!(card["name"], "renamed");
        assert_eq!(card["start_ticks"], want, "a rename keeps the start time");
    }

    // A card from an older build has no start time: it reads as None, a
    // None is not written back as a key, and the start time is not part of
    // a card's identity (its socket name, the owner check), so cards
    // written before and after it existed name the same socket and owner.
    #[test]
    fn a_card_without_a_start_time_reads_as_none_and_keeps_its_identity() {
        let old = r#"{"kind":"session","session_id":"old","name":null,"socket_path":"/x.sock","pid":1,"started_at":"2024-01-01T00:00:00Z","receipts":1}"#;
        let reg: SessionRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(reg.start_ticks, None);
        assert!(serde_json::to_value(&reg)
            .unwrap()
            .get("start_ticks")
            .is_none());
        let newer = SessionRegistration {
            start_ticks: Some(12345),
            ..reg.clone()
        };
        assert_eq!(generated_basename(&reg), generated_basename(&newer));
        assert!(same_owner(&reg, &newer));
        let back: SessionRegistration =
            serde_json::from_value(serde_json::to_value(&newer).unwrap()).unwrap();
        assert_eq!(back.start_ticks, Some(12345));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn locked_drop_removes_endpoint_and_next_scan_repairs_live_pid_card() {
        let tmp = tmp_registry();
        let endpoint = SessionEndpoint::start_in(
            tmp.path(),
            "locked",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        let path = endpoint.registration.socket_path.clone();
        let lock = lock_dir(tmp.path()).unwrap();
        drop(endpoint);
        assert!(!std::path::Path::new(&path).exists());
        assert!(tmp.path().join("locked.json").exists());
        drop(lock);
        assert_eq!(sweep_stale_registrations_in(tmp.path(), None), 1);
        assert!(!tmp.path().join("locked.json").exists());
    }

    #[tokio::test]
    async fn owned_rename_refuses_replacement_and_uses_captured_root() {
        let tmp = tmp_registry();
        let other = tmp_registry();
        let queue = std::sync::Arc::new(super::super::queue::EventQueue::new(10));
        let old = SessionEndpoint::start_in(tmp.path(), "same", None, queue.clone()).unwrap();
        let new = SessionEndpoint::start_in(tmp.path(), "same", None, queue.clone()).unwrap();
        let unrelated = SessionEndpoint::start_in(other.path(), "same", None, queue).unwrap();
        assert!(old.update_name(Some("wrong")).is_err());
        new.update_name(Some("right")).unwrap();
        assert_eq!(
            list_active_sessions_in(tmp.path())[0].name.as_deref(),
            Some("right")
        );
        assert!(list_active_sessions_in(other.path())[0].name.is_none());
        drop(new);
        assert!(other.path().join("same.json").exists());
        drop(unrelated);
    }

    #[tokio::test]
    async fn publication_failure_rolls_back_bound_endpoint() {
        let tmp = tmp_registry();
        assert!(SessionEndpoint::start_in(
            tmp.path(),
            "daemon",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10))
        )
        .is_err());
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(".session-registry.lock")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_survives_old_task_ending_and_immediate_drop() {
        let tmp = tmp_registry();
        let queue = std::sync::Arc::new(super::super::queue::EventQueue::new(10));
        let old = SessionEndpoint::start_in(tmp.path(), "same", None, queue.clone()).unwrap();
        let old_path = old.registration.socket_path.clone();
        let new = SessionEndpoint::start_in(tmp.path(), "same", None, queue).unwrap();
        assert_ne!(old_path, new.registration.socket_path);
        update_session_name_in("same", Some("renamed"), tmp.path()).unwrap();
        // No await since either start: neither listener task has been polled.
        drop(old);
        assert!(!std::path::Path::new(&old_path).exists());
        tokio::task::yield_now().await;
        let _client = tokio::net::UnixStream::connect(&new.registration.socket_path)
            .await
            .unwrap();
        let new_path = new.registration.socket_path.clone();
        assert_eq!(
            list_active_sessions_in(tmp.path())[0].name.as_deref(),
            Some("renamed")
        );
        drop(new);
        assert!(!std::path::Path::new(&new_path).exists());
        assert!(!tmp.path().join("same.json").exists());
    }

    #[test]
    fn lock_is_permanent_and_contention_is_bounded() {
        let tmp = tmp_registry();
        let lock = lock_dir(tmp.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let dir = tmp.path().to_path_buf();
        let contender = std::thread::spawn(move || {
            ready.wait();
            assert!(lock_dir(&dir).is_err());
        });
        barrier.wait();
        contender.join().unwrap();
        drop(lock);
        assert!(tmp.path().join(".session-registry.lock").exists());
        assert!(lock_dir(tmp.path()).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_sweep_and_registration_are_serialized() {
        let tmp = tmp_registry();
        reg_with_socket(tmp.path(), "same", 0);
        let lock = lock_dir(tmp.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let dir = tmp.path().to_path_buf();
        let sweep = std::thread::spawn(move || {
            ready.wait();
            // The other actor holds the publication lock: no stale deletion.
            assert_eq!(
                sweep_stale_registrations_in(&dir, Some(std::process::id())),
                0
            );
        });
        barrier.wait();
        sweep.join().unwrap();
        drop(lock);
        let endpoint = SessionEndpoint::start_in(
            tmp.path(),
            "same",
            None,
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        assert_eq!(sweep_stale_registrations_in(tmp.path(), None), 0);
        let _client = tokio::net::UnixStream::connect(&endpoint.registration.socket_path)
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn malicious_socket_claims_leave_user_files_and_other_sockets() {
        let tmp = tmp_registry();
        let outside = tmp_registry();
        let outside_path = outside.path().join("victim.sock");
        let _outside = std::os::unix::net::UnixListener::bind(&outside_path).unwrap();
        let unrelated = tmp.path().join("other.sock");
        let _other = std::os::unix::net::UnixListener::bind(&unrelated).unwrap();
        let regular = tmp.path().join("victim.sock");
        std::fs::write(&regular, b"user data").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        for path in [
            outside_path.clone(),
            unrelated.clone(),
            regular.clone(),
            tmp.path().join("link/victim.sock"),
            tmp.path().join("../victim.sock"),
        ] {
            let mut reg = make_reg("victim", None, 0);
            reg.socket_path = path.to_string_lossy().into_owned();
            register_session_in(&reg, tmp.path()).unwrap();
            sweep_stale_registrations_in(tmp.path(), None);
        }
        assert!(outside_path.exists());
        assert!(unrelated.exists());
        assert_eq!(std::fs::read(regular).unwrap(), b"user data");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn replacement_pipe_is_bound_before_publication() {
        let tmp = tmp_registry();
        let queue = std::sync::Arc::new(super::super::queue::EventQueue::new(10));
        let old = SessionEndpoint::start_in(tmp.path(), "same", None, queue.clone()).unwrap();
        let new = SessionEndpoint::start_in(tmp.path(), "same", None, queue).unwrap();
        drop(old);
        let pipe = super::super::socket::pipe_name_for(&new.registration.socket_path);
        let _client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(pipe)
            .unwrap();
    }

    fn tmp_registry() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    fn make_reg(id: &str, name: Option<&str>, pid: u32) -> SessionRegistration {
        SessionRegistration {
            kind: REGISTRATION_KIND.to_string(),
            session_id: id.to_string(),
            name: name.map(|s| s.to_string()),
            socket_path: format!("/unused/{id}.sock"),
            pid,
            started_at: Utc::now(),
            receipts: 0,
            start_ticks: None,
        }
    }

    fn dir_buf(tmp: &TempDir) -> PathBuf {
        tmp.path().to_path_buf()
    }

    // What an event receiver reads: live cards only, and nothing removed
    // (not the dead owner's card, not a foreign file).
    #[test]
    fn live_cards_are_read_without_removing_anything() {
        let tmp = tmp_registry();
        let live = make_reg("s-live", Some("alive"), std::process::id());
        register_session_in(&live, tmp.path()).unwrap();
        let dead = make_reg("s-dead", None, u32::MAX);
        register_session_in(&dead, tmp.path()).unwrap();
        std::fs::write(tmp.path().join("notes.json"), "{}").unwrap();
        let scan = live_cards_in(tmp.path());
        assert!(
            scan.complete,
            "a file that is no card leaves the scan complete"
        );
        let cards = scan.cards;
        let ids: Vec<&str> = cards.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, ["s-live"]);
        assert_eq!(cards[0].name.as_deref(), Some("alive"));
        assert!(
            tmp.path().join("s-dead.json").exists(),
            "nothing is removed"
        );
        assert!(tmp.path().join("notes.json").exists());
    }

    // A receiver's scan reads at most MAX_SCANNED_CARDS card files, and says
    // it didn't see them all.
    #[test]
    fn a_scan_reads_at_most_256_cards() {
        let tmp = tmp_registry();
        for i in 0..300 {
            let reg = make_reg(&format!("s-{i:03}"), None, std::process::id());
            register_session_in(&reg, tmp.path()).unwrap();
        }
        let scan = live_cards_in(tmp.path());
        let n = scan.cards.len();
        assert_eq!(n, MAX_SCANNED_CARDS, "a scan of 300 cards read {n}");
        assert!(
            !scan.complete,
            "a scan that stopped at the limit says it is complete"
        );
    }

    // B1b r2 fix: exactly MAX_SCANNED_CARDS cards are a complete scan; one
    // more card file, even one that is no live card, makes it incomplete.
    #[test]
    fn a_scan_is_complete_up_to_256_cards_and_not_beyond() {
        let tmp = tmp_registry();
        for i in 0..MAX_SCANNED_CARDS {
            let reg = make_reg(&format!("s-{i:03}"), None, std::process::id());
            register_session_in(&reg, tmp.path()).unwrap();
        }
        std::fs::write(tmp.path().join("daemon.json"), "{}").unwrap();
        let scan = live_cards_in(tmp.path());
        assert_eq!(scan.cards.len(), MAX_SCANNED_CARDS);
        assert!(
            scan.complete,
            "256 cards and the daemon's file: the scan saw them all"
        );

        let dead = make_reg("s-dead", None, u32::MAX);
        register_session_in(&dead, tmp.path()).unwrap();
        let scan = live_cards_in(tmp.path());
        assert!(
            !scan.complete,
            "a 257th card file was never read, yet the scan says complete"
        );
    }

    /// A card for `id`, owned by this process, whose file is exactly
    /// `bytes` long (padded through its name).
    fn card_of_size(dir: &std::path::Path, id: &str, bytes: usize) {
        let mut reg = make_reg(id, Some(""), std::process::id());
        let base = serde_json::to_string(&reg).unwrap().len();
        reg.name = Some("x".repeat(bytes - base));
        register_session_in(&reg, dir).unwrap();
        let len = std::fs::metadata(dir.join(format!("{id}.json")))
            .unwrap()
            .len();
        assert_eq!(len, bytes as u64, "the card for {id} is not {bytes} bytes");
    }

    // A card file over MAX_CARD_BYTES is skipped, never trusted; one of
    // exactly that size is read.
    #[test]
    fn a_card_over_64_kib_is_skipped_and_one_at_the_limit_is_read() {
        let tmp = tmp_registry();
        let limit = MAX_CARD_BYTES as usize;
        card_of_size(tmp.path(), "s-limit", limit);
        card_of_size(tmp.path(), "s-over", limit + 1);
        let scan = live_cards_in(tmp.path());
        assert!(
            scan.complete,
            "a card file over the limit is no card; the scan saw it"
        );
        let cards = scan.cards;
        let ids: Vec<&str> = cards.iter().map(|c| c.session_id.as_str()).collect();
        assert!(!ids.contains(&"s-over"), "a card over 64 KiB was read: {ids:?}");
        assert!(ids.contains(&"s-limit"), "a card of exactly 64 KiB was skipped: {ids:?}");
    }

    // A FIFO named like a card never blocks a scan (nothing writes to it):
    // only regular files are read. Bounded: the scan runs on a thread, and a
    // stuck one is released by opening the FIFO's other end.
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_a_card_never_blocks_the_scan() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tmp_registry();
        let live = make_reg("s-live", None, std::process::id());
        register_session_in(&live, tmp.path()).unwrap();
        let fifo = tmp.path().join("s-fifo.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path inside the test's own TempDir.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let dir = tmp.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(live_cards_in(&dir));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        if got.is_err() {
            let _writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo);
        }
        let scan = got.expect("a FIFO named like a card blocked the scan for 5 s");
        assert!(scan.complete, "a FIFO is no card; the scan saw it");
        let ids: Vec<&str> = scan.cards.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, ["s-live"]);
    }

    // B1b r2 fix: a card file that can't be read may be a card, so the scan
    // says it didn't see every card. The file is in the test's own TempDir.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_card_file_makes_the_scan_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmp_registry();
        let live = make_reg("s-live", None, std::process::id());
        register_session_in(&live, tmp.path()).unwrap();
        let locked = tmp.path().join("s-locked.json");
        std::fs::write(&locked, "{}").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked).is_ok() {
            println!("EACCES not real here (root?): case skipped");
            return;
        }
        let scan = live_cards_in(tmp.path());
        assert!(
            !scan.complete,
            "an unreadable card file left the scan complete"
        );
        let ids: Vec<&str> = scan.cards.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, ["s-live"]);
    }

    #[test]
    fn register_creates_file() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("abc-1234", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();
        assert!(dir.join("abc-1234.json").exists());
    }

    #[test]
    fn list_returns_live_sessions() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let pid = std::process::id();
        let reg = make_reg("live-0001", Some("my-agent"), pid);
        register_session_in(&reg, &dir).unwrap();

        let sessions = list_active_sessions_in(&dir);
        assert!(sessions.iter().any(|r| r.session_id == "live-0001"));
    }

    #[test]
    fn find_by_exact_id() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("find-exact-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("find-exact-01", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "find-exact-01");
    }

    #[test]
    fn find_by_name() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("named-session-01", Some("prod-agent"), std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("prod-agent", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "named-session-01");
    }

    #[test]
    fn find_by_partial_prefix() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("prefix-abcdef-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("prefix-abc", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "prefix-abcdef-01");
    }

    #[test]
    fn ambiguous_prefix_returns_none() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let pid = std::process::id();
        register_session_in(&make_reg("dup-aaaa-01", None, pid), &dir).unwrap();
        register_session_in(&make_reg("dup-aaaa-02", None, pid), &dir).unwrap();

        let found = find_session_registration_in("dup-aaaa", &dir);
        assert!(found.is_none(), "ambiguous prefix should return None");
    }

    /// The daemon's own run-dir files are never read as session cards.
    #[test]
    fn the_daemons_state_files_are_reserved_and_session_ids_are_not() {
        let p = std::path::Path::new;
        for reserved in [
            "/r/daemon.json",
            "/r/daemon-work.json",
            "/r/daemon.live.json",
            "/r/daemon.reload.json",
            "/r/daemon-work.live.json",
        ] {
            assert!(is_reserved_json(p(reserved)), "{reserved}");
        }
        for card in ["/r/20261019-120800-ab12.json", "/r/daemonic.json", "/r/my.daemon.json"] {
            assert!(!is_reserved_json(p(card)), "{card}");
        }
    }

    /// Bug: `synaps send` used to unlink every `run/*.json` it could not
    /// parse as a registration — `daemon.json` included.
    #[test]
    fn foreign_json_is_never_unlinked() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let daemon = dir.join("daemon.json");
        std::fs::write(
            &daemon,
            r#"{"pid":1,"version":"0.9.0","exe":"/usr/bin/synaps","socket":"/x/daemon.sock","protocol":2}"#,
        )
        .unwrap();
        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "this is not json {").unwrap();
        // Parses as a registration structurally but declares another kind.
        let other = dir.join("other-kind.json");
        std::fs::write(
            &other,
            r#"{"kind":"widget","session_id":"x","name":null,"socket_path":"/x.sock","pid":1,"started_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        // A legacy registration without `kind` still counts as a session.
        let legacy = dir.join("legacy-0001.json");
        std::fs::write(
            &legacy,
            format!(
                r#"{{"session_id":"legacy-0001","name":"old","socket_path":"{}","pid":{},"started_at":"2024-01-01T00:00:00Z"}}"#,
                dir.join("legacy-0001.sock").display(),
                std::process::id()
            ),
        )
        .unwrap();
        let reg = make_reg("live-0002", Some("ambient"), std::process::id());
        register_session_in(&reg, &dir).unwrap();

        for _ in 0..2 {
            let found = find_session_registration_in("ambient", &dir).expect("session resolved");
            assert_eq!(found.session_id, "live-0002");
            assert!(
                find_session_registration_in("old", &dir).is_some(),
                "legacy reg counts"
            );
            let ids: Vec<_> = list_active_sessions_in(&dir)
                .into_iter()
                .map(|r| r.session_id)
                .collect();
            assert!(!ids.contains(&"x".to_string()), "foreign kind not listed");
            assert!(daemon.exists(), "daemon.json must survive");
            assert!(garbage.exists(), "garbage.json must survive");
            assert!(other.exists(), "other-kind.json must survive");
            assert!(legacy.exists());
        }
        assert!(std::fs::read_to_string(&daemon).unwrap().contains("0.9.0"));
    }

    /// Bug: the name was frozen at create — `saveas` never reached the
    /// registry, so `synaps send --session <name>` missed until a
    /// `--continue`.
    #[test]
    fn update_name_rewrites_registration() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("rename-0001", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();
        assert!(find_session_registration_in("ambient", &dir).is_none());

        update_session_name_in("rename-0001", Some("ambient"), &dir).unwrap();
        let found = find_session_registration_in("ambient", &dir).expect("resolves by new name");
        assert_eq!(found.session_id, "rename-0001");
        assert_eq!(
            found.socket_path, reg.socket_path,
            "rest of the record kept"
        );

        update_session_name_in("rename-0001", None, &dir).unwrap();
        assert!(
            find_session_registration_in("ambient", &dir).is_none(),
            "cleared"
        );
        assert!(
            update_session_name_in("ghost", Some("x"), &dir).is_err(),
            "no reg → error"
        );
    }

    #[test]
    fn unregister_removes_file() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("unreg-0001", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let path = dir.join("unreg-0001.json");
        assert!(path.exists());

        unregister_session_in("unreg-0001", &dir);
        assert!(!path.exists());
    }

    #[test]
    fn unregister_is_idempotent() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        // Should not panic even if the file was never registered
        unregister_session_in("ghost-session-99", &dir);
    }

    #[cfg(unix)]
    #[test]
    fn stale_pid_pruned() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        // PID 999999 is effectively guaranteed to not exist
        let reg = make_reg("stale-dead-pid", None, 999999);
        register_session_in(&reg, &dir).unwrap();

        let sessions = list_active_sessions_in(&dir);
        assert!(
            !sessions.iter().any(|r| r.session_id == "stale-dead-pid"),
            "stale registration should have been pruned"
        );

        // File should also be gone
        assert!(!dir.join("stale-dead-pid.json").exists());
    }

    /// A registration whose socket lives in `dir`, with a real socket file,
    /// so removal of both can be observed.
    #[cfg(unix)]
    fn reg_with_socket(dir: &std::path::Path, id: &str, pid: u32) -> SessionRegistration {
        let reg = SessionRegistration {
            kind: REGISTRATION_KIND.to_string(),
            session_id: id.to_string(),
            name: None,
            socket_path: socket_path_in_dir(dir, id),
            pid,
            started_at: Utc::now(),
            receipts: 0,
            start_ticks: None,
        };
        let _ = std::fs::remove_file(&reg.socket_path);
        let _socket = std::os::unix::net::UnixListener::bind(&reg.socket_path).unwrap();
        register_session_in(&reg, dir).unwrap();
        reg
    }

    #[cfg(unix)]
    #[test]
    fn unregister_owned_removes_its_own_registration_and_socket() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = reg_with_socket(&dir, "owned", std::process::id());
        unregister_owned_in(&reg, &dir);
        assert!(!dir.join("owned.json").exists());
        assert!(!dir.join("owned.sock").exists());
        // Idempotent: shutdown() and Drop both call it.
        unregister_owned_in(&reg, &dir);
    }

    #[cfg(unix)]
    #[test]
    fn unregister_owned_leaves_a_newer_registration_of_the_same_id() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let old = reg_with_socket(&dir, "same-id", std::process::id());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newer = reg_with_socket(&dir, "same-id", std::process::id());
        assert_ne!(old.started_at, newer.started_at);

        unregister_owned_in(&old, &dir);
        assert!(dir.join("same-id.json").exists(), "newer registration kept");
        assert!(dir.join("same-id.sock").exists(), "newer socket kept");
        let live = list_active_sessions_in(&dir);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].started_at, newer.started_at);
    }

    #[cfg(unix)]
    #[test]
    fn a_renamed_registration_is_still_owned_and_removed() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = reg_with_socket(&dir, "renamed", std::process::id());
        update_session_name_in("renamed", Some("ambient"), &dir).unwrap();
        unregister_owned_in(&reg, &dir);
        assert!(
            !dir.join("renamed.json").exists(),
            "a rename keeps ownership"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_start_sweep_removes_dead_and_own_pid_and_keeps_other_live_owners() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let own = std::process::id();
        // SAFETY: getppid has no preconditions; the parent (the test runner's
        // parent shell) is alive and owned by the same user.
        let other_live = unsafe { libc::getppid() } as u32;
        reg_with_socket(&dir, "dead-owner", 999_999);
        reg_with_socket(&dir, "pre-reload", own);
        reg_with_socket(&dir, "other-process", other_live);
        std::fs::write(dir.join("daemon.json"), r#"{"pid":999999}"#).unwrap();

        assert_eq!(sweep_stale_registrations_in(&dir, Some(own)), 2);
        for gone in ["dead-owner", "pre-reload"] {
            assert!(
                !dir.join(format!("{gone}.json")).exists(),
                "{gone} card removed"
            );
            assert!(
                !dir.join(format!("{gone}.sock")).exists(),
                "{gone} socket removed"
            );
        }
        assert!(dir.join("other-process.json").exists());
        assert!(dir.join("other-process.sock").exists());
        assert!(
            dir.join("daemon.json").exists(),
            "daemon.json is never a session"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sweep_without_own_pid_keeps_registrations_of_this_process() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        reg_with_socket(&dir, "mine", std::process::id());
        reg_with_socket(&dir, "dead", 999_999);
        assert_eq!(sweep_stale_registrations_in(&dir, None), 1);
        assert!(dir.join("mine.json").exists());
        assert!(!dir.join("dead.json").exists());
    }

    #[test]
    // Keyed to the same lock as every HOME/SYNAPS_BASE_DIR mutator (it pins
    // the base dir itself): unkeyed `#[serial]` and
    // `#[serial(synaps_base_dir)]` do NOT exclude each other.
    #[serial(synaps_base_dir)]
    fn socket_path_format() {
        // Pin the roots: an ambient SYNAPS_BASE_DIR (often itself under
        // /tmp or /var/tmp) or SYNAPS_RUNTIME_DIR must not decide the result.
        let base = crate::test_env::BaseDirGuard::new();
        let _runtime = crate::test_env::EnvVarGuard::remove("SYNAPS_RUNTIME_DIR");
        let path = socket_path_for_session("20240101-120000-ab12");
        // Sockets live in the registry's run dir under the base dir, not in
        // the system temp dir.
        let run = std::fs::canonicalize(base.path().join("run")).unwrap();
        assert_eq!(
            std::path::PathBuf::from(&path),
            run.join("20240101-120000-ab12.sock"),
            "got: {}",
            path
        );
    }

    // Regression: an EFS-backed SYNAPS_BASE_DIR can exceed Linux's 108-byte
    // sockaddr_un.sun_path limit. Runtime sockets must be independently rooted.
    #[cfg(unix)]
    #[test]
    fn short_runtime_dirs_keep_long_session_socket_bindable_and_isolated() {
        use std::os::unix::net::UnixListener;
        let long_session = "s".repeat(80);
        // These model deterministic per-UID runtime roots created by guest-agent.
        let path_a = socket_path_in_dir(std::path::Path::new("/tmp/a"), &long_session);
        let path_b = socket_path_in_dir(std::path::Path::new("/tmp/b"), &long_session);
        assert!(path_a.len() < 108, "Unix socket path too long: {path_a}");
        assert_ne!(
            path_a, path_b,
            "per-user runtime roots must isolate same session ids"
        );

        // Bind under SHORT, /tmp-rooted dirs. The whole point is short socket
        // paths; some platforms' tempdir() (e.g. macOS /var/folders/...) is long
        // enough to blow the sun_path limit and flake this exact assertion, so we
        // control the root length explicitly. Two sibling dirs model two per-uid
        // runtime roots.
        let pid = std::process::id();
        let a = std::path::PathBuf::from(format!("/tmp/sa{pid}"));
        let b = std::path::PathBuf::from(format!("/tmp/sb{pid}"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let _a = UnixListener::bind(socket_path_in_dir(&a, &long_session)).unwrap();
        let _b = UnixListener::bind(socket_path_in_dir(&b, &long_session)).unwrap();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[cfg(unix)]
    #[test]
    fn registration_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("perms-check-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let path = dir.join("perms-check-01.json");
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600, "registry file should be 0600");
    }

    /// Under a real Landlock domain that refuses every write (LOCAL kernel
    /// only, opt in with SYNAPS_TEST_LANDLOCK=1), as for a session under a
    /// write sandbox: lookups still find and reach the live peer, and the
    /// scan unlinks nothing. Without the reader lock, `synaps send` found no
    /// session there and fell back to an inbox it couldn't write either.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn registry_local_confined_reader_finds_live_peers_and_unlinks_nothing() {
        use crate::sandbox::landlock::{abi_version, run_confined, Ruleset, Tag, MIN_ABI};
        if std::env::var("SYNAPS_TEST_LANDLOCK").as_deref() != Ok("1") {
            return;
        }
        match abi_version() {
            Ok(abi) if abi >= MIN_ABI => {}
            other => panic!("SYNAPS_TEST_LANDLOCK=1 but Landlock is unusable here: {other:?}"),
        }
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let peer = SessionEndpoint::start_in(
            &dir,
            "peer-01",
            Some("peer".into()),
            std::sync::Arc::new(super::super::queue::EventQueue::new(10)),
        )
        .unwrap();
        register_session_in(&make_reg("stale-01", None, 999999), &dir).unwrap();
        let stale = dir.join("stale-01.json");
        assert!(stale.exists());
        let socket = peer.registration.socket_path.clone();
        let rs = Ruleset::new().expect("ruleset");
        let got = run_confined(&rs, &Tag::next(), || {
            let writer = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join(".session-registry.lock"))
                .map(|_| ())
                .map_err(|e| e.kind());
            let by_id = find_session_registration_in("peer-01", &dir).map(|r| r.session_id);
            let by_name = find_session_registration_in("peer", &dir).map(|r| r.session_id);
            let live: Vec<String> = list_active_sessions_in(&dir)
                .into_iter()
                .map(|r| r.session_id)
                .collect();
            let connect = std::os::unix::net::UnixStream::connect(&socket)
                .map(|_| ())
                .map_err(|e| e.kind());
            (writer, by_id, by_name, live, connect)
        })
        .expect("confined thread");
        let (writer, by_id, by_name, live, connect) = got;
        assert_eq!(writer, Err(std::io::ErrorKind::PermissionDenied), "the domain refuses the writer lock");
        assert_eq!(by_id.as_deref(), Some("peer-01"));
        assert_eq!(by_name.as_deref(), Some("peer-01"));
        assert_eq!(live, ["peer-01"], "the stale card is not live");
        assert_eq!(connect, Ok(()), "the peer's socket takes the connection");
        assert!(stale.exists(), "the confined scan unlinked nothing");
        // Control: an unconfined scan prunes it.
        assert_eq!(list_active_sessions_in(&dir).len(), 1);
        assert!(!stale.exists(), "an unconfined scan prunes the stale card");
        drop(peer);
    }
}

// ── daemon paths (Phase 2 B) ──────────────────────────────────────────────────

/// Files the `synaps daemon` keeps under `registry_dir()` (0700). One daemon
/// per profile: `daemon.{sock,lock,json,pid}` or `daemon-<P>.{…}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub sock: PathBuf,
    pub lock: PathBuf,
    pub json: PathBuf,
    pub pid: PathBuf,
}

/// Daemon file paths for a profile, under `registry_dir()`.
pub fn daemon_paths(profile: Option<&str>) -> DaemonPaths {
    daemon_paths_in(&registry_dir(), profile)
}

/// Same, rooted at an explicit dir (tests; `--socket` overrides only `.sock`).
pub fn daemon_paths_in(dir: &std::path::Path, profile: Option<&str>) -> DaemonPaths {
    let stem = match profile.map(sanitize_session_id).filter(|p| !p.is_empty()) {
        Some(p) => format!("daemon-{p}"),
        None => "daemon".to_string(),
    };
    DaemonPaths {
        dir: dir.to_path_buf(),
        sock: dir.join(format!("{stem}.sock")),
        lock: dir.join(format!("{stem}.lock")),
        json: dir.join(format!("{stem}.json")),
        pid: dir.join(format!("{stem}.pid")),
    }
}

#[cfg(test)]
mod daemon_paths_tests {
    use super::*;

    #[test]
    fn daemon_paths_are_profile_scoped_and_sanitised() {
        let d = std::path::Path::new("/run/x");
        let p = daemon_paths_in(d, None);
        assert_eq!(p.sock, d.join("daemon.sock"));
        assert_eq!(p.lock, d.join("daemon.lock"));
        assert_eq!(p.json, d.join("daemon.json"));
        assert_eq!(p.pid, d.join("daemon.pid"));
        let p = daemon_paths_in(d, Some("work/../evil"));
        assert_eq!(p.sock, d.join("daemon-work____evil.sock"));
    }
}
