//! Opt-in, local repository identity; legacy JSONL discovery is unchanged.
//!
//! Git's common directory, NOT remotes, defines repository membership. A private
//! `synaps-memory-identity.json` there retains a random repository key
//! across moves. Schema (unknown fields rejected): `{"version":2,"key":
//! "p<16 lowercase hex>","initial_root":"/original/main","roots":["/original/main"]}`.
//! `key` is `p` plus 16 random lowercase hexadecimal digits, never a path hash.
//! The reserved all-zero key and unreleased path-key version 1 are refused.
//! `roots` retains verified canonical historical paths
//! (including initial_root), bounded to 4096 entries and the marker to 1 MiB.
//! Readers must verify the current common Git directory and private marker/lock,
//! historical path hashes are migration candidates only, not canonical identity.
//! Registered worktrees are verified in both directions before
//! their path scopes are retained as migration candidates. No records are merged.
//! An explicit `SYNAPS_PROJECT_ROOT` bypasses Git entirely, just as in legacy
//! discovery. Without a discoverable main checkout (e.g. a bare/separate gitdir),
//! the common directory is the reported repository root. Concurrent workers
//! share the random key durably published under the common-directory lock.
//!
//! Persistence requires Unix ownership/mode checks, pinned directory descriptors,
//! an advisory lock, atomic publication and file/directory fsync. Git processes
//! and marker lock acquisition each have a five-second timeout. Unsupported
//! platforms fail closed for Git identity, not for legacy/non-Git scopes. A
//! same-UID process can deliberately rewrite private state; this is not a sandbox
//! against the host user. Copying a private marker copies identity; ordinary clones
//! do not copy Git-private files. Never initialize identity in a live repo in tests.

use super::store::{MemoryError, ProjectScope};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MARKER: &str = "synaps-memory-identity.json";
const LOCK: &str = "synaps-memory-identity.lock";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_ROOTS: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Git processes spawned by `git()` on this thread (tests only).
    static GIT_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Force the one-option-per-process `rev-parse` path (tests only).
    static FORCE_SINGLE_REV_PARSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Wait operations performed by `git()` on this thread (tests only).
    static GIT_WAITS: std::cell::RefCell<Vec<GitWait>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Reads of Git's stdout to report as WouldBlock before reading (tests only).
    static FORCE_WOULD_BLOCK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How `git()` waited (tests only): polling for output, or sleeping before
/// or after stdout EOF.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitWait {
    PollOutput,
    Sleep { after_eof: bool, duration: Duration },
}

#[cfg(test)]
fn record_wait(wait: GitWait) {
    GIT_WAITS.with(|w| w.borrow_mut().push(wait));
}

/// Read Git's nonblocking stdout. Tests can force the next reads to report
/// WouldBlock, so the not-yet-ready path runs regardless of scheduling.
#[cfg(unix)]
fn read_pipe(pipe: &mut impl std::io::Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    #[cfg(test)]
    if FORCE_WOULD_BLOCK.with(|n| n.replace(n.get().saturating_sub(1))) > 0 {
        return Err(std::io::ErrorKind::WouldBlock.into());
    }
    pipe.read(buffer)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryIdentity {
    /// Canonical repository key, with the currently verified repository root.
    pub scope: ProjectScope,
    /// Exactly the scope legacy discovery would return for this invocation.
    pub legacy_scope: ProjectScope,
    /// Deduplicated migration candidates, including `scope` itself. Historical
    /// roots were verified when recorded; they need not still exist. This list
    /// is evidence for an operator, NOT permission to merge or query aliases.
    pub aliases: Vec<ProjectScope>,
    /// Current main checkout when verifiable, otherwise the common directory.
    pub repository_root: PathBuf,
    /// Current checkout (or explicit override/non-Git legacy root).
    pub worktree_root: PathBuf,
    pub common_dir: Option<PathBuf>,
}

impl ProjectScope {
    /// Opt-in repository discovery. The explicit `SYNAPS_PROJECT_ROOT` override
    /// wins and disables Git sharing; malformed Git/private state is an error,
    /// never a fallback to an unrelated or newly generated scope.
    pub fn discover_repository(start: &Path) -> Result<RepositoryIdentity, MemoryError> {
        let root = std::env::var_os("SYNAPS_PROJECT_ROOT").map(PathBuf::from);
        Self::discover_repository_with_override(start, root.as_deref())
    }

    /// Environment-free counterpart; `Some(root)` preserves the legacy override
    /// semantics even if root is a subdirectory/worktree of a Git repository.
    pub fn discover_repository_with_override(
        start: &Path,
        override_root: Option<&Path>,
    ) -> Result<RepositoryIdentity, MemoryError> {
        let legacy = Self::discover_with_override(start, override_root)?;
        Self::validate_repository_key(legacy.key())?;
        let root = legacy.root().to_path_buf();
        let has_git = match fs::symlink_metadata(root.join(".git")) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if override_root.is_some() || !has_git {
            return Ok(RepositoryIdentity {
                scope: legacy.clone(),
                legacy_scope: legacy.clone(),
                aliases: vec![legacy],
                repository_root: root.clone(),
                worktree_root: root,
                common_dir: None,
            });
        }
        resolve(legacy)
    }
}

fn invalid(message: impl Into<String>) -> MemoryError {
    MemoryError::InvalidProjectRoot(message.into())
}

// Only read-only Git plumbing, no shell/hooks/network. Ignore inherited Git
// routing/config injection and user/system config. Local repository config is
// needed for real worktree layouts; Git's own safe.directory check still applies.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, MemoryError> {
    git_output(root, args)?.ok_or_else(|| invalid("Git repository verification failed"))
}

/// `Ok(None)`: Git exited unsuccessfully (the direct child is already reaped).
#[cfg(not(unix))]
fn git_output(_: &Path, _: &[&str]) -> Result<Option<Vec<u8>>, MemoryError> {
    Err(invalid(
        "bounded Git discovery requires Unix nonblocking pipes",
    ))
}

/// `Ok(None)`: Git exited unsuccessfully (the direct child is already reaped).
#[cfg(unix)]
fn git_output(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, MemoryError> {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    let mut cmd = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            cmd.env_remove(name);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let deadline = Instant::now() + WAIT_TIMEOUT;
    #[cfg(test)]
    GIT_SPAWNS.with(|n| n.set(n.get() + 1));
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let timed_out = || invalid("Git discovery timed out after 5 seconds");
    // A reader thread or blocking read_to_end can outlive the timeout when a
    // descendant retains stdout. Read the nonblocking pipe, waiting for it with
    // poll(2) in slices of at most POLL_INTERVAL (no sleep before EOF), and
    // check the direct child after each slice. A child that fails while a
    // descendant still holds the pipe is reported at once; one that succeeds
    // but leaves a descendant holding stdout still waits for EOF until the
    // deadline and then times out, as before. After EOF the child is reaped
    // with a bounded sleep backoff (20 us doubling, capped at POLL_INTERVAL).
    let result = (|| {
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            match read_pipe(&mut stdout, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes.len() + n > MAX_BYTES as usize {
                        return Err(invalid("Git discovery output too large"));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            let slice = POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
            // Round up so a sub-millisecond remainder does not spin.
            let timeout_ms = slice.as_micros().div_ceil(1000) as libc::c_int;
            #[cfg(test)]
            record_wait(GitWait::PollOutput);
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // POLLIN/POLLHUP/POLLERR all mean the next read makes progress or
            // reports EOF/error; either way re-check the child and deadline.
            if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != ErrorKind::Interrupted {
                    return Err(e.into());
                }
                continue;
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Ok(None);
                }
            }
        }
        // EOF: Git normally exits immediately after closing stdout. Reap it
        // with a short exponential backoff, bounded by the same deadline.
        let mut backoff = Duration::from_micros(20);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status.success().then_some(bytes));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timed_out());
            }
            #[cfg(test)]
            record_wait(GitWait::Sleep {
                after_eof: true,
                duration: backoff.min(remaining),
            });
            std::thread::sleep(backoff.min(remaining));
            backoff = (backoff * 2).min(POLL_INTERVAL);
        }
    })();
    if result.is_err() {
        // Always terminate/reap the direct child on timeout/read/wait errors.
        // Never wait for EOF from descendants during cleanup.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn path_bytes(bytes: &[u8]) -> Result<PathBuf, MemoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).map_err(|_| invalid("non-UTF8 Git path"))?,
    ))
}

fn absolute_path(bytes: &[u8]) -> Result<PathBuf, MemoryError> {
    let path = path_bytes(bytes)?;
    if !path.is_absolute() {
        return Err(invalid("Git returned a relative path"));
    }
    Ok(path.canonicalize()?)
}

fn git_path(root: &Path, arg: &str) -> Result<PathBuf, MemoryError> {
    let bytes = git(root, &["rev-parse", "--path-format=absolute", arg])?;
    let bytes = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| invalid("invalid Git path output"))?;
    absolute_path(bytes)
}

const CHECKOUT_PATHS: [&str; 3] = ["--show-toplevel", "--absolute-git-dir", "--git-common-dir"];

/// Exactly three newline-terminated lines, else `None`. A successful
/// rev-parse prints one line per option, so any other count means a path
/// contains a newline and the lines cannot be attributed unambiguously.
fn three_lines(bytes: &[u8]) -> Option<[&[u8]; 3]> {
    let body = bytes.strip_suffix(b"\n")?;
    let lines: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
    lines.try_into().ok()
}

/// The `CHECKOUT_PATHS` of `root` in one rev-parse. Unsuccessful Git or an
/// ambiguous (newline-containing) output repeats the per-option calls, which
/// keeps their exact results, errors and checking order.
fn checkout_paths(root: &Path) -> Result<Option<[Vec<u8>; 3]>, MemoryError> {
    #[cfg(test)]
    if FORCE_SINGLE_REV_PARSE.with(|f| f.get()) {
        return Ok(None);
    }
    let mut args = vec!["rev-parse", "--path-format=absolute"];
    args.extend(CHECKOUT_PATHS);
    let Some(bytes) = git_output(root, &args)? else {
        return Ok(None);
    };
    Ok(three_lines(&bytes).map(|lines| lines.map(<[u8]>::to_vec)))
}

#[derive(PartialEq, Eq)]
struct Checkout {
    root: PathBuf,
    git_dir: PathBuf,
    common: PathBuf,
}

fn checkout(root: &Path) -> Result<Checkout, MemoryError> {
    let entry = fs::symlink_metadata(root.join(".git"))?;
    if entry.file_type().is_symlink() || !(entry.is_file() || entry.is_dir()) {
        return Err(invalid("unsafe .git entry"));
    }
    let batched = checkout_paths(root)?;
    let path = |i: usize| match &batched {
        Some(lines) => absolute_path(&lines[i]),
        None => git_path(root, CHECKOUT_PATHS[i]),
    };
    let top = path(0)?;
    if top != root {
        return Err(invalid("Git checkout root mismatch"));
    }
    let git_dir = path(1)?;
    let common = path(2)?;
    if git_dir != common {
        if git_dir.parent() != Some(common.join("worktrees").as_path()) {
            return Err(invalid("unregistered Git worktree directory"));
        }
        let back = fs::read(git_dir.join("gitdir"))?;
        let back = back.strip_suffix(b"\n").unwrap_or(&back);
        let back = path_bytes(back)?;
        let back = if back.is_absolute() {
            back
        } else {
            git_dir.join(back)
        };
        if back.canonicalize()? != root.join(".git").canonicalize()? {
            return Err(invalid("Git worktree backlink mismatch"));
        }
    }
    Ok(Checkout {
        root: top,
        git_dir,
        common,
    })
}

// Git enumerates the common-dir worktrees registry (NUL format handles spaces,
// quotes and newlines). Re-resolving every candidate rejects stale/prunable
// paths, reused paths and forged one-way links, without touching any records.
fn registered(current: &Checkout) -> Result<(PathBuf, Vec<PathBuf>), MemoryError> {
    let bytes = git(&current.common, &["worktree", "list", "--porcelain", "-z"])?;
    let mut roots = Vec::new();
    let mut main = None;
    for field in bytes.split(|b| *b == 0) {
        let Some(bytes) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        if roots.len() >= MAX_ROOTS {
            return Err(invalid("too many registered worktrees"));
        }
        let path = path_bytes(bytes)?;
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        let Ok(candidate) = checkout(&path) else {
            continue;
        };
        if candidate.common != current.common {
            continue;
        }
        if candidate.git_dir == current.common {
            if main.as_ref().is_some_and(|other| other != &path) {
                return Err(invalid("ambiguous main checkout"));
            }
            main = Some(path.clone());
        }
        roots.push(path);
    }
    if current.git_dir == current.common {
        if main.as_ref().is_some_and(|other| other != &current.root) {
            return Err(invalid("main checkout disagrees with registry"));
        }
        // Separate-git-dir repositories may not list their main checkout. Do
        // not report it as the repository root: a worker cannot discover that path.
        if main.is_some() {
            roots.push(current.root.clone());
        }
    } else if !roots.contains(&current.root) {
        return Err(invalid("current worktree missing from verified registry"));
    }
    roots.push(current.root.clone());
    roots.sort();
    roots.dedup();
    Ok((main.unwrap_or_else(|| current.common.clone()), roots))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u32,
    key: String,
    initial_root: PathBuf,
    roots: Vec<PathBuf>,
}

impl Marker {
    fn validate(&self) -> Result<(), MemoryError> {
        ProjectScope::validate_repository_key(&self.key)?;
        if self.version != 2 || self.roots.len() > MAX_ROOTS || self.roots.is_empty() {
            return Err(invalid(
                "unsupported or malformed repository identity marker",
            ));
        }
        for root in std::iter::once(&self.initial_root).chain(&self.roots) {
            if !root.is_absolute()
                || root
                    .components()
                    .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
            {
                return Err(invalid("noncanonical historical repository path"));
            }
            ProjectScope::validate_repository_key(
                ProjectScope::for_canonical_root(root.clone()).key(),
            )?;
        }
        if !self.roots.contains(&self.initial_root) {
            return Err(invalid("repository marker roots omit its initial root"));
        }
        Ok(())
    }
}

// Randomness is independent of checkout paths: an unrelated repository may
// occupy a moved repository's old path without inheriting its canonical DB key.
fn new_repository_key() -> String {
    loop {
        let key = format!("p{:016x}", rand::random::<u64>());
        if ProjectScope::validate_repository_key(&key).is_ok() {
            return key;
        }
    }
}

#[cfg(not(unix))]
fn resolve(_: ProjectScope) -> Result<RepositoryIdentity, MemoryError> {
    Err(invalid(
        "private repository identity requires Unix filesystem ownership checks",
    ))
}

#[cfg(unix)]
fn acquire_lock(lock: &fs::File) -> Result<(), MemoryError> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(invalid("repository marker lock timed out after 5 seconds"));
        }
        match fs4::fs_std::FileExt::try_lock_exclusive(lock) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(unix)]
fn resolve(legacy: ProjectScope) -> Result<RepositoryIdentity, MemoryError> {
    let current = checkout(legacy.root())?;
    let dir = private::Directory::open(&current.common)?;
    let lock = dir.open_file(LOCK, libc::O_RDWR | libc::O_CREAT)?;
    acquire_lock(&lock)?;
    dir.check_file(LOCK, &lock)?;
    let (repository_root, roots) = registered(&current)?;
    let old = match dir.open_file(MARKER, libc::O_RDONLY) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut marker = if let Some(file) = &old {
        use std::io::Read;
        let mut data = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_BYTES {
            return Err(invalid("repository identity marker too large"));
        }
        serde_json::from_slice::<Marker>(&data)?
    } else {
        Marker {
            version: 2,
            key: new_repository_key(),
            initial_root: repository_root.clone(),
            roots: vec![repository_root.clone()],
        }
    };
    marker.validate()?;
    let before = marker.roots.clone();
    marker.roots.extend(roots);
    marker.roots.sort();
    marker.roots.dedup();
    marker.validate()?;
    // Revalidate routing and pinned names before publishing/returning. A Git
    // repair/move racing discovery must be retried, not bind the wrong scope.
    if checkout(legacy.root())? != current {
        return Err(invalid("Git layout changed during discovery"));
    }
    dir.check_path(&current.common)?;
    dir.check_file(LOCK, &lock)?;
    if let Some(file) = &old {
        dir.check_file(MARKER, file)?;
    }
    if old.is_none() || before != marker.roots {
        let bytes = serde_json::to_vec(&marker)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("repository identity marker too large"));
        }
        dir.publish(&bytes, old.is_some())?;
    }
    let published = dir.open_file(MARKER, libc::O_RDONLY)?;
    {
        use std::io::Read;
        let mut bytes = Vec::new();
        (&published).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        let expected = serde_json::to_value(&marker)?;
        if bytes.len() as u64 > MAX_BYTES
            || serde_json::from_slice::<serde_json::Value>(&bytes)? != expected
        {
            return Err(invalid("repository marker changed during discovery"));
        }
    }
    // fsync even on read: a previous writer may have died just after rename.
    published.sync_all()?;
    dir.sync_all()?;
    dir.check_file(LOCK, &lock)?;
    dir.check_file(MARKER, &published)?;
    dir.check_path(&current.common)?;
    let scope = ProjectScope::from_key(&repository_root, &marker.key)?;
    let mut aliases = vec![scope.clone()];
    for root in marker.roots {
        let alias = ProjectScope::for_canonical_root(root);
        if !aliases.iter().any(|s| s.key() == alias.key()) {
            aliases.push(alias);
        }
    }
    Ok(RepositoryIdentity {
        scope,
        legacy_scope: legacy.clone(),
        aliases,
        repository_root,
        worktree_root: legacy.root().to_path_buf(),
        common_dir: Some(current.common),
    })
}

#[cfg(unix)]
mod private {
    use super::{LOCK, MARKER};
    use std::ffi::CString;
    use std::fs::{File, Metadata};
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::path::{Component, Path};

    pub(super) struct Directory(File);

    fn denied() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe or changed repository identity filesystem state",
        )
    }
    fn cstr(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| denied())
    }
    fn opened(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
    fn same(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    fn file_ok(meta: &Metadata) -> io::Result<()> {
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
        {
            return Err(denied());
        }
        Ok(())
    }

    impl Directory {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            if !path.is_absolute() {
                return Err(denied());
            }
            let mut file = File::open("/")?;
            let uid = unsafe { libc::geteuid() };
            for part in path.components() {
                let Component::Normal(name) = part else {
                    if part == Component::RootDir {
                        continue;
                    }
                    return Err(denied());
                };
                let name = cstr(name.as_bytes())?;
                file = opened(unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                })?;
                let meta = file.metadata()?;
                // Root-owned sticky /tmp is safe. Owner-owned group-writable
                // project layouts are supported; the private marker/lock remain
                // 0600. This is not a sandbox against the host user's group.
                // Never chmod or take ownership of Git directories.
                if (meta.uid() != uid && meta.uid() != 0)
                    || (meta.mode() & 0o002 != 0 && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0))
                {
                    return Err(denied());
                }
            }
            let meta = file.metadata()?;
            if meta.uid() != uid || meta.mode() & 0o002 != 0 {
                return Err(denied());
            }
            Ok(Self(file))
        }

        pub(super) fn open_file(&self, name: &str, flags: i32) -> io::Result<File> {
            let name = cstr(name.as_bytes())?;
            let file = opened(unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    // Variadic mode: C promotes it to unsigned int, and Rust
                    // refuses a u16 (`mode_t` on macOS) in a variadic call.
                    0o600 as libc::c_uint,
                )
            })?;
            file_ok(&file.metadata()?)?;
            Ok(file)
        }

        pub(super) fn check_file(&self, name: &str, file: &File) -> io::Result<()> {
            let meta = file.metadata()?;
            file_ok(&meta)?;
            let other = self.open_file(name, libc::O_RDONLY)?;
            if !same(&meta, &other.metadata()?) {
                return Err(denied());
            }
            Ok(())
        }

        pub(super) fn check_path(&self, path: &Path) -> io::Result<()> {
            let other = Self::open(path)?;
            if !same(&self.0.metadata()?, &other.0.metadata()?) {
                return Err(denied());
            }
            Ok(())
        }

        pub(super) fn sync_all(&self) -> io::Result<()> {
            self.0.sync_all()
        }

        pub(super) fn publish(&self, bytes: &[u8], replace: bool) -> io::Result<()> {
            let temp = format!("{MARKER}.{}.tmp", uuid::Uuid::new_v4().simple());
            let mut file = self.open_file(&temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
            let temp_c = cstr(temp.as_bytes())?;
            let final_c = cstr(MARKER.as_bytes())?;
            let fd = self.0.as_raw_fd();
            let result = (|| {
                file.write_all(bytes)?;
                file.sync_all()?;
                self.check_file(&temp, &file)?;
                let rc = if replace {
                    // Under the persistent advisory lock. Refuse unsafe targets
                    // rather than repairing/replacing a planted link or mode.
                    self.open_file(MARKER, libc::O_RDONLY)?;
                    unsafe { libc::renameat(fd, temp_c.as_ptr(), fd, final_c.as_ptr()) }
                } else {
                    // No-clobber atomic first publication: even a noncooperating
                    // creator cannot have its marker silently overwritten.
                    unsafe { libc::linkat(fd, temp_c.as_ptr(), fd, final_c.as_ptr(), 0) }
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })();
            // Never unlink the lock. Unlink only our uniquely named temp entry.
            debug_assert_ne!(temp, LOCK);
            let unlinked = unsafe { libc::unlinkat(fd, temp_c.as_ptr(), 0) };
            result?;
            if unlinked != 0 && !replace {
                return Err(io::Error::last_os_error());
            }
            self.sync_all()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    // Fixture tests create directories (racing the crate's umask-mutating
    // tests) and spawn Git. A forked child briefly inherits other threads'
    // descriptors, including held advisory locks, until it executes Git; tests
    // that require uncontended try-locks join `process_spawn` as `parallel`.
    use serial_test::serial;
    use tempfile::TempDir;

    fn fixture_git(root: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command.env_remove(name);
            }
        }
        let output = command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", root)
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args([
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=test@example.invalid",
            ])
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A synthetic repository with `extra` plain worktrees plus the given
    /// specially named ones. Returns (main, all worktree roots).
    fn fixture(tmp: &TempDir, extra: usize, named: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let main = tmp.path().join("main");
        fs::create_dir(&main).unwrap();
        fixture_git(&main, &["init", "-q"]);
        fixture_git(&main, &["commit", "-q", "--allow-empty", "-m", "synthetic"]);
        let main = main.canonicalize().unwrap();
        let names = (0..extra)
            .map(|i| format!("wt{i}"))
            .chain(named.iter().map(|s| s.to_string()));
        let mut roots = Vec::new();
        for (i, name) in names.enumerate() {
            let path = tmp.path().join(name);
            let branch = format!("b{i}");
            fixture_git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &branch,
                    path.to_str().unwrap(),
                ],
            );
            roots.push(path.canonicalize().unwrap());
        }
        (main, roots)
    }

    fn spawns<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = GIT_SPAWNS.with(|n| n.get());
        let value = f();
        (value, GIT_SPAWNS.with(|n| n.get()) - before)
    }

    fn single<T>(f: impl FnOnce() -> T) -> T {
        FORCE_SINGLE_REV_PARSE.with(|f| f.set(true));
        let value = f();
        FORCE_SINGLE_REV_PARSE.with(|f| f.set(false));
        value
    }

    fn discover(root: &Path) -> RepositoryIdentity {
        ProjectScope::discover_repository_with_override(root, None).unwrap()
    }

    #[test]
    #[serial(umask, process_spawn)]
    fn discovery_spawns_one_git_per_registered_worktree() {
        let tmp = TempDir::new().unwrap();
        let (main, roots) = fixture(&tmp, 40, &[]);
        // N+1 checkouts, one worktree listing, one current-checkout re-check.
        let budget = roots.len() + 1 + 4;
        for start in [&main, &roots[7]] {
            let (identity, count) = spawns(|| discover(start));
            assert_eq!(identity.aliases.len(), roots.len() + 2);
            assert!(count <= budget, "{count} Git spawns > {budget}");
        }
    }

    #[test]
    #[serial(umask, process_spawn)]
    fn checkout_batches_rev_parse_and_newline_paths_fall_back() {
        let tmp = TempDir::new().unwrap();
        let (main, roots) = fixture(&tmp, 1, &["worker a", "worker\n\"b"]);
        for root in [&main, &roots[0], &roots[1]] {
            let (batched, count) = spawns(|| checkout(root).unwrap());
            assert_eq!(count, 1, "{root:?}");
            assert!(batched == single(|| checkout(root).unwrap()));
        }
        // Four output lines: identical result via the per-option fallback.
        let newline = &roots[2];
        let (batched, count) = spawns(|| checkout(newline).unwrap());
        assert_eq!(count, 4);
        assert!(batched == single(|| checkout(newline).unwrap()));
        assert_eq!(&batched.root, newline);
    }

    #[test]
    #[serial(umask, process_spawn)]
    fn batched_and_single_rev_parse_resolve_identical_identity() {
        let tmp = TempDir::new().unwrap();
        let (main, roots) = fixture(&tmp, 3, &["worker a", "worker\n\"b"]);
        for start in std::iter::once(&main).chain(&roots) {
            let slow = single(|| discover(start));
            let fast = discover(start);
            assert_eq!(fast, slow);
            assert_eq!(fast.aliases.len(), roots.len() + 2);
        }
        assert!(single(|| checkout(&main)).unwrap() == checkout(&main).unwrap());
    }

    #[test]
    fn rev_parse_output_needs_exactly_three_terminated_lines() {
        let ok: [&[u8]; 3] = [b"/a", b"/a/.git", b"/a/.git"];
        assert_eq!(three_lines(b"/a\n/a/.git\n/a/.git\n"), Some(ok));
        for bad in [
            &b""[..],
            b"\n",
            b"/a\n/a/.git\n/a/.git",
            b"/a\n/a/.git\n",
            b"/x\ny\n/x\ny/.git\n/x\ny/.git\n",
            b"/a\n/a/.git\n/a/.git\n\n",
        ] {
            assert_eq!(three_lines(bad), None, "{bad:?}");
        }
    }

    // Checks the kind and order of waits, not elapsed time: before EOF git()
    // must wait for its output with poll(2), never with a fixed sleep (the
    // old loop slept 10 ms). Only the bounded post-EOF reaping backoff may
    // sleep, starting far below that. The first read of every call is forced
    // to report WouldBlock, so the not-yet-ready path runs even when Git's
    // output is already available. Elapsed time is measured only by the
    // ignored benchmark.
    #[test]
    #[serial(umask, process_spawn)]
    fn git_waits_for_output_by_poll_not_fixed_sleep() {
        let tmp = TempDir::new().unwrap();
        let (main, _) = fixture(&tmp, 0, &[]);
        for _ in 0..10 {
            GIT_WAITS.with(|w| w.borrow_mut().clear());
            FORCE_WOULD_BLOCK.with(|n| n.set(1));
            let output = git(&main, &["rev-parse", "--git-common-dir"]).unwrap();
            assert_eq!(output, b".git\n");
            assert_eq!(FORCE_WOULD_BLOCK.with(|n| n.get()), 0);
            let waits = GIT_WAITS.with(|w| w.take());
            // At least one pre-EOF wait, all polls; then only reaping sleeps.
            assert_eq!(waits.first(), Some(&GitWait::PollOutput), "{waits:?}");
            let reaping = waits
                .iter()
                .position(|w| matches!(w, GitWait::Sleep { .. }))
                .unwrap_or(waits.len());
            for (i, wait) in waits.iter().enumerate() {
                match *wait {
                    GitWait::PollOutput => assert!(i < reaping, "{waits:?}"),
                    GitWait::Sleep {
                        after_eof,
                        duration,
                    } => {
                        assert!(after_eof, "fixed sleep before EOF: {waits:?}");
                        if i == reaping {
                            assert!(duration < Duration::from_millis(1), "{waits:?}");
                        }
                    }
                }
            }
        }
    }

    /// `cargo test -p synaps-core --lib -- --ignored --nocapture discovery_bench`
    #[test]
    #[serial(umask, process_spawn)]
    #[ignore]
    fn discovery_bench_132_worktrees() {
        let tmp = TempDir::new().unwrap();
        let (main, roots) = fixture(&tmp, 132, &[]);
        discover(&main);
        let mut times = Vec::new();
        let mut count = 0;
        for _ in 0..5 {
            let started = Instant::now();
            count = spawns(|| discover(&roots[50])).1;
            times.push(started.elapsed());
        }
        times.sort();
        println!(
            "discovery 132 worktrees: median {:?} min {:?} max {:?}, {count} Git spawns",
            times[2], times[0], times[4]
        );
    }
}
