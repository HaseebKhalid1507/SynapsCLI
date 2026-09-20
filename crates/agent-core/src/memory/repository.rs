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
#[cfg(not(unix))]
fn git(_: &Path, _: &[&str]) -> Result<Vec<u8>, MemoryError> {
    Err(invalid(
        "bounded Git discovery requires Unix nonblocking pipes",
    ))
}

#[cfg(unix)]
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, MemoryError> {
    use std::io::{ErrorKind, Read};
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
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    // A reader thread or blocking read_to_end can outlive the timeout when a
    // descendant retains stdout. Poll the nonblocking pipe and child together.
    let result = (|| {
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(invalid("Git discovery timed out after 5 seconds"));
            }
            let mut received = false;
            if !eof {
                match stdout.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        if bytes.len() + n > MAX_BYTES as usize {
                            return Err(invalid("Git discovery output too large"));
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        received = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(invalid("Git repository verification failed"));
                }
                if eof {
                    return Ok(bytes);
                }
            }
            if !received {
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
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

fn git_path(root: &Path, arg: &str) -> Result<PathBuf, MemoryError> {
    let bytes = git(root, &["rev-parse", "--path-format=absolute", arg])?;
    let bytes = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| invalid("invalid Git path output"))?;
    let path = path_bytes(bytes)?;
    if !path.is_absolute() {
        return Err(invalid("Git returned a relative path"));
    }
    Ok(path.canonicalize()?)
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
    let top = git_path(root, "--show-toplevel")?;
    if top != root {
        return Err(invalid("Git checkout root mismatch"));
    }
    let git_dir = git_path(root, "--absolute-git-dir")?;
    let common = git_path(root, "--git-common-dir")?;
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
                    0o600 as libc::mode_t,
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
