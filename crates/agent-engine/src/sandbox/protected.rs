//! Owner rule 27c: what no agent may write, whatever its policy grants.
//!
//! JR's Allow/Deny answers and the sandbox-mode switch live in
//! `$HOME/.synaps-cli/memory/jev/` (written only by the Pria Desktop app and
//! the Jev guard), the Jev guard's config (it holds the Jev API key) is
//! `$HOME/.synaps-cli/plugins/jev/config`, and its legacy fallback is the
//! whole Synaps config, `$HOME/.synaps-cli/config`. These roots are
//! `$HOME`-based, not base-dir-based: they are protected whatever the
//! daemon's base dir is.
//!
//! Two layers use them:
//! - the Landlock layer ([`grant_roots`]): a policy grant equal to, under,
//!   or an ancestor of a root, by name or by identity, refuses the policy;
//! - the in-process layer ([`ToolGuard`]): in an enforcing session the
//!   `write`, `edit` and `multi_edit` tools refuse a target that is or is
//!   inside a root, or the base dir's `config` or `sandbox/` (the policy
//!   files and the kill switch, which owner rule 27b leaves to JR).
//!
//! The in-process check is made on opened objects, not path strings: the
//! target's parent is opened `O_PATH` (following symlinks), the kernel's
//! name for it (`/proc/self/fd/N`) and the identity (dev, ino) of it and of
//! every directory above it (by `..`, so a bind mount of a root, or of a
//! directory above one, is seen by its identity) are checked, and the write
//! then goes through that same descriptor (a temp file made with
//! `openat(O_EXCL | O_NOFOLLOW)` and a `renameat`), so a symlink swapped in
//! after the check can't redirect it.
//!
//! Trust assumption, as for the policy: the protected trees and the mount
//! namespace aren't rearranged by unconfined processes. A bind mount of a
//! directory below a root, made elsewhere, isn't chased: walking `..` from
//! inside it need not pass the root.
//!
//! Off Linux there is no `O_PATH` and no `/proc`: directories are opened
//! `O_RDONLY | O_DIRECTORY` (so they need read permission, and a directory
//! that can't be read fails the check: refused, never written), and on
//! macOS an open object is named by `fcntl(F_GETPATH)`, a name that must
//! still lead to that very object. Any other OS can't name one, so every
//! checked write there fails closed.
//!
//! Off Unix (Windows) there are no directory descriptors at all: the
//! descriptor checks are compiled out and every checked write fails closed
//! (`open_target` refuses with [`WriteError::Check`]).

#[cfg(unix)]
use std::ffi::{CString, OsString};
use std::ffi::OsStr;
use std::io;
#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

#[cfg(not(unix))]
use super::nonunix::MetadataExt;

use super::policy::{PolicyEnv, SANDBOX_DIR};

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// `$HOME`-relative: JR's Allow/Deny answers and the 27b switch (a directory).
pub const JEV_STATE_DIR: &str = ".synaps-cli/memory/jev";
/// `$HOME`-relative: the Jev guard's config (it holds the Jev API key).
pub const JEV_CONFIG_FILE: &str = ".synaps-cli/plugins/jev/config";
/// `$HOME`-relative: the whole Synaps config (the Jev guard's legacy fallback).
pub const SYNAPS_CONFIG_FILE: &str = ".synaps-cli/config";
/// Base-dir-relative: the Synaps config of this base dir (in-process layer).
pub const BASE_CONFIG_FILE: &str = "config";

/// The `$HOME`-based roots both layers protect.
pub const HOME_ROOTS: &[&str] = &[JEV_STATE_DIR, JEV_CONFIG_FILE, SYNAPS_CONFIG_FILE];

/// Directories walked up by `..` at most (a deeper tree fails the check).
const MAX_DEPTH: usize = 4096;

/// The text a refused write returns to the agent.
pub fn refusal(path: &Path) -> String {
    format!(
        "Refused by owner rule 27c: `{}` is protected. It is written only by JR's app or by JR, never by an agent's write, edit or multi_edit. Nothing was changed.",
        path.display()
    )
}

/// The WARN line of a refused write: no path, no content.
pub fn protected_line(session: &str, tool: &str) -> String {
    format!("sandbox.protected session={session} tool={tool}")
}

/// `p` as the kernel would name it now: its deepest existing ancestor
/// resolved (symlinks followed), with the rest appended as written. A root
/// that doesn't exist yet is then still known by the name it would get.
pub fn resolved_name(p: &Path) -> PathBuf {
    for anc in p.ancestors() {
        if anc.as_os_str().is_empty() {
            break;
        }
        if let Ok(c) = std::fs::canonicalize(anc) {
            return match p.strip_prefix(anc) {
                Ok(rest) if !rest.as_os_str().is_empty() => c.join(rest),
                _ => c,
            };
        }
    }
    p.to_path_buf()
}

/// `true` for "doesn't exist" (also a non-directory on the way).
fn missing(e: &io::Error) -> bool {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ENOTDIR) {
        return true;
    }
    e.kind() == io::ErrorKind::NotFound
}

/// What the Landlock layer protects: every name of every root, and the
/// identity of each existing root and of every existing directory above it.
#[derive(Debug, Default)]
pub struct GrantRoots {
    pub names: Vec<PathBuf>,
    pub ids: Vec<(u64, u64)>,
}

/// The roots under `home` for the Landlock layer. Fails closed: a root or
/// a directory above one that exists but can't be statted is an error (the
/// caller refuses the policy). A root that doesn't exist has nothing to
/// alias; its name is still protected.
pub fn grant_roots(home: &Path) -> Result<GrantRoots, String> {
    let mut out = GrantRoots::default();
    for rel in HOME_ROOTS {
        let p = home.join(rel);
        let resolved = resolved_name(&p);
        for name in [resolved.clone(), p] {
            if !out.names.contains(&name) {
                out.names.push(name);
            }
        }
        for anc in resolved.ancestors() {
            match std::fs::metadata(anc) {
                Ok(m) => out.ids.push((m.dev(), m.ino())),
                Err(e) if missing(&e) => {}
                Err(e) => return Err(format!("cannot stat the protected `{}`: {e}", anc.display())),
            }
        }
    }
    Ok(out)
}

/// One root of the in-process layer: its names now, and its identity if it
/// exists.
#[derive(Clone, Debug)]
struct Root {
    names: Vec<PathBuf>,
    id: Option<(u64, u64)>,
}

/// The in-process layer of one enforcing session (made per tool call, so a
/// root created or moved since is seen as it is now).
#[derive(Clone, Debug)]
pub struct ToolGuard {
    roots: Vec<Root>,
    /// `tools = confined`: where the target may be written.
    confine: Option<Confine>,
}

/// The write set of a confined session's file tools.
#[derive(Clone, Debug)]
struct Confine {
    /// The identity of every directory a target may be in or below
    /// (`super::confine::write_dirs`).
    dirs: Vec<(u64, u64)>,
    /// The policy's contact sentence, for the refusal.
    contact: String,
}

/// Why a checked write didn't happen. Each tool maps these to its own
/// texts, the same texts it gives without a sandbox.
#[derive(Debug)]
pub enum WriteError {
    /// The target is protected (owner rule 27c).
    Protected,
    /// The target is outside a confined session's write set
    /// (`tools = confined`).
    Confined,
    /// The check itself failed (the target couldn't be named or statted).
    Check(io::Error),
    /// Creating a missing parent failed.
    CreateDirs(io::Error),
    /// The parent couldn't be opened (no parents are created).
    Open(io::Error),
    /// Writing the temp file failed.
    Write(io::Error),
    /// Renaming the temp file onto the target failed.
    Finalize(io::Error),
}

impl ToolGuard {
    /// The roots for `env`: the three `$HOME` roots, the base dir's
    /// `config`, and its `sandbox/`.
    pub fn new(env: &PolicyEnv) -> ToolGuard {
        let mut paths: Vec<PathBuf> = HOME_ROOTS.iter().map(|rel| env.home.join(rel)).collect();
        paths.push(env.base_dir.join(BASE_CONFIG_FILE));
        paths.push(env.base_dir.join(SANDBOX_DIR));
        let roots = paths
            .into_iter()
            .map(|p| {
                let resolved = resolved_name(&p);
                let mut names = vec![resolved];
                if !names.contains(&p) {
                    names.push(p.clone());
                }
                let id = std::fs::metadata(&p).ok().map(|m| (m.dev(), m.ino()));
                Root { names, id }
            })
            .collect();
        ToolGuard { roots, confine: None }
    }

    /// This guard for a `tools = confined` session: a target must also be in
    /// or below one of `dirs` (directory identities), checked after owner rule
    /// 27c's check, so a protected target keeps 27c's refusal.
    pub fn confined(mut self, dirs: Vec<(u64, u64)>, contact: &str) -> ToolGuard {
        self.confine = Some(Confine { dirs, contact: contact.to_string() });
        self
    }

    /// The contact sentence of a confined session's policy (`None` when the
    /// session's tools aren't confined).
    pub fn confine_contact(&self) -> Option<&str> {
        self.confine.as_ref().map(|c| c.contact.as_str())
    }

    /// `tools = confined`: the opened directory `fd`, where the target is or
    /// will be written, must be one of the write set's directories or below
    /// one, by identity, walking `..` as Landlock's path-beneath rules do
    /// (which also climbs out of a bind mount). Nothing to check when the
    /// session's tools aren't confined.
    #[cfg(unix)]
    fn check_allowed(&self, fd: BorrowedFd<'_>) -> Result<(), WriteError> {
        let Some(c) = &self.confine else {
            return Ok(());
        };
        let mut cur = openat(fd.as_raw_fd(), OsStr::new("."), DIR_PATH, 0).map_err(WriteError::Check)?;
        for _ in 0..MAX_DEPTH {
            let id = fstat(cur.as_raw_fd()).map_err(WriteError::Check)?.id;
            if c.dirs.contains(&id) {
                return Ok(());
            }
            let up = openat(cur.as_raw_fd(), OsStr::new(".."), DIR_PATH, 0).map_err(WriteError::Check)?;
            if fstat(up.as_raw_fd()).map_err(WriteError::Check)?.id == id {
                // The root, and no write-set directory on the way.
                return Err(WriteError::Confined);
            }
            cur = up;
        }
        Err(WriteError::Confined)
    }

    /// Whether `p` (an absolute, resolved name) is or is inside a root.
    pub fn covers(&self, p: &Path) -> bool {
        self.roots.iter().flat_map(|r| r.names.iter()).any(|n| p.starts_with(n))
    }

    fn covers_id(&self, id: (u64, u64)) -> bool {
        self.roots.iter().any(|r| r.id == Some(id))
    }

    /// An opened directory: refused if the kernel's name for it is or is
    /// inside a root, or if it or any directory above it (walked by `..`,
    /// which also climbs out of a bind mount) is a root. Returns its name.
    #[cfg(unix)]
    fn check_dir(&self, fd: BorrowedFd<'_>) -> Result<PathBuf, WriteError> {
        let name = fd_name(fd).map_err(WriteError::Check)?;
        if self.covers(&name) {
            return Err(WriteError::Protected);
        }
        let mut cur = openat(fd.as_raw_fd(), OsStr::new("."), DIR_PATH, 0).map_err(WriteError::Check)?;
        for _ in 0..MAX_DEPTH {
            let id = fstat(cur.as_raw_fd()).map_err(WriteError::Check)?.id;
            if self.covers_id(id) {
                return Err(WriteError::Protected);
            }
            let up = openat(cur.as_raw_fd(), OsStr::new(".."), DIR_PATH, 0).map_err(WriteError::Check)?;
            if fstat(up.as_raw_fd()).map_err(WriteError::Check)?.id == id {
                return Ok(name);
            }
            cur = up;
        }
        Err(WriteError::Check(io::Error::other("the directory tree is too deep to check")))
    }

    /// The target `tail` in the opened directory `parent`: the directory,
    /// the name the target has or would get, the object there now (its
    /// identity: a second name of a protected file), and, for a symlink or a
    /// name that isn't a plain file name, the object it leads to.
    #[cfg(unix)]
    fn check_target(&self, parent: BorrowedFd<'_>, tail: &OsStr, file_name: Option<&OsStr>) -> Result<(), WriteError> {
        let dir = self.check_dir(parent)?;
        if let Some(n) = file_name {
            if self.covers(&dir.join(n)) {
                return Err(WriteError::Protected);
            }
        }
        // Nothing there (or nothing that can be statted): the name is all.
        let Ok(st) = fstatat(parent.as_raw_fd(), tail, libc::AT_SYMLINK_NOFOLLOW) else {
            return Ok(());
        };
        if self.covers_id(st.id) {
            return Err(WriteError::Protected);
        }
        if st.kind == libc::S_IFLNK || file_name.is_none() {
            // Where it leads now. (`write` and `edit` replace a final
            // symlink rather than write through it; refusing one that leads
            // into a root costs nothing.) A dangling one leads nowhere.
            if let Ok(fd) = openat(parent.as_raw_fd(), tail, ANY_PATH, 0) {
                let to = fstat(fd.as_raw_fd()).map_err(WriteError::Check)?;
                if self.covers_id(to.id) {
                    return Err(WriteError::Protected);
                }
                let to_name = fd_name(fd.as_fd()).map_err(WriteError::Check)?;
                if self.covers(&to_name) {
                    return Err(WriteError::Protected);
                }
                if to.kind == libc::S_IFDIR {
                    self.check_dir(fd.as_fd())?;
                } else if let Some(up) = to_name.parent() {
                    if let Ok(up) = open_dir(up) {
                        self.check_dir(up.as_fd())?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The parent of a target, created as `std::fs::create_dir_all` would
    /// (same modes, same errors), but one directory at a time from an
    /// opened, checked one. Before the first directory is made, the whole
    /// target as it would be named (the opened directory's name, every
    /// missing name, and `tail`) is checked, so a refusal makes nothing;
    /// when a `.` or `..` among them makes that name unknowable ahead, only
    /// the per-step checks apply: each new name is checked before it is
    /// made, and each directory after it is opened.
    #[cfg(unix)]
    fn ensure_dir(&self, parent: &Path, tail: &OsStr) -> Result<OwnedFd, WriteError> {
        let mut pending: Vec<OsString> = Vec::new();
        let mut cur = parent.to_path_buf();
        let mut fd = loop {
            match open_dir(&cur) {
                Ok(fd) => break fd,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let (up, last) = split(&cur);
                    if last.is_empty() || last == "." || pending.len() >= MAX_DEPTH || up == cur {
                        return Err(WriteError::CreateDirs(e));
                    }
                    pending.push(last);
                    cur = up;
                }
                // create_dir_all: an existing non-directory is "File exists".
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) && std::fs::metadata(&cur).is_ok_and(|m| !m.is_dir()) => {
                    return Err(WriteError::CreateDirs(io::Error::from_raw_os_error(libc::EEXIST)));
                }
                Err(e) => return Err(WriteError::CreateDirs(e)),
            }
        };
        let opened = self.check_dir(fd.as_fd())?;
        if !pending.is_empty() {
            let names = pending.iter().rev().map(OsString::as_os_str).chain([tail]);
            if let Some(whole) = prospective(opened, names) {
                if self.covers(&whole) {
                    return Err(WriteError::Protected);
                }
            }
            // Confined: nothing is made outside the write set. (With nothing
            // to make, `open_target` checks after 27c's target checks.)
            self.check_allowed(fd.as_fd())?;
        }
        while let Some(name) = pending.pop() {
            let dir = fd_name(fd.as_fd()).map_err(WriteError::Check)?;
            if let Some(Component::Normal(n)) = Path::new(&name).components().next_back() {
                if self.covers(&dir.join(n)) {
                    return Err(WriteError::Protected);
                }
            }
            let existed = match mkdirat(fd.as_raw_fd(), &name, 0o777) {
                Ok(()) => false,
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => true,
                Err(e) => return Err(WriteError::CreateDirs(e)),
            };
            let next = match openat(fd.as_raw_fd(), &name, DIR_PATH, 0) {
                Ok(next) => next,
                // It was there but isn't a directory (or leads nowhere).
                Err(_) if existed => return Err(WriteError::CreateDirs(io::Error::from_raw_os_error(libc::EEXIST))),
                Err(e) => return Err(WriteError::CreateDirs(e)),
            };
            self.check_dir(next.as_fd())?;
            // A name that appeared meanwhile can lead elsewhere.
            self.check_allowed(next.as_fd())?;
            fd = next;
        }
        Ok(fd)
    }

    /// Open and check the target `path` (absolute, or relative to this
    /// process's directory, as the tools give it). `create_parents`: make
    /// missing parent directories (`write`); otherwise a missing parent is
    /// [`WriteError::Open`] (`edit`).
    #[cfg(unix)]
    pub fn open_target(&self, path: &Path, create_parents: bool) -> Result<Target, WriteError> {
        let (parent, tail) = split(path);
        let fd = if create_parents {
            self.ensure_dir(&parent, &tail)?
        } else {
            open_dir(&parent).map_err(WriteError::Open)?
        };
        let file_name = path.file_name().map(OsStr::to_os_string);
        self.check_target(fd.as_fd(), &tail, file_name.as_deref())?;
        self.check_allowed(fd.as_fd())?;
        Ok(Target { parent: fd, tail })
    }

    /// [`Self::open_target`] off Unix (Windows): no directory descriptor to
    /// check the target through, so the check fails and nothing is written.
    #[cfg(not(unix))]
    pub fn open_target(&self, path: &Path, create_parents: bool) -> Result<Target, WriteError> {
        let _ = (path, create_parents);
        Err(WriteError::Check(io::Error::new(
            io::ErrorKind::Unsupported,
            "this OS has no directory descriptors to check a write through",
        )))
    }

    /// A target opened another way: `multi_edit`'s verified parent handle,
    /// the file name in it, and `target`, the resolved path they name. The
    /// same checks as [`Self::open_target`]; only `Protected` or `Check`.
    #[cfg(unix)]
    pub fn check_opened(&self, parent: BorrowedFd<'_>, name: &OsStr, target: &Path) -> Result<(), WriteError> {
        if self.covers(target) {
            return Err(WriteError::Protected);
        }
        self.check_target(parent, name, Some(name))?;
        self.check_allowed(parent)
    }
}

/// A checked target: its parent's handle and its name in it, exactly as the
/// path ended (a trailing `/` or a final `..` keeps the kernel's own result).
#[cfg(unix)]
#[derive(Debug)]
pub struct Target {
    parent: OwnedFd,
    tail: OsString,
}

/// Off Unix no target is ever opened ([`ToolGuard::open_target`] refuses).
#[cfg(not(unix))]
#[derive(Debug)]
pub struct Target {
    never: std::convert::Infallible,
}

#[cfg(not(unix))]
impl Target {
    pub fn read_to_string(&self) -> io::Result<String> {
        match self.never {}
    }

    pub fn mode(&self) -> Option<u32> {
        match self.never {}
    }

    pub fn replace(&self, tmp_name: &OsStr, bytes: &[u8], mode: Option<u32>) -> Result<(), WriteError> {
        let _ = (tmp_name, bytes, mode);
        match self.never {}
    }
}

#[cfg(unix)]
impl Target {
    /// Read the target as text, through a final symlink (as
    /// `std::fs::read_to_string` does).
    pub fn read_to_string(&self) -> io::Result<String> {
        let fd = openat(self.parent.as_raw_fd(), &self.tail, libc::O_RDONLY | libc::O_CLOEXEC, 0)?;
        let mut text = String::new();
        std::fs::File::from(fd).read_to_string(&mut text)?;
        Ok(text)
    }

    /// The permission bits of what the target leads to now, if anything
    /// (as `std::fs::metadata` follows a final symlink).
    pub fn mode(&self) -> Option<u32> {
        fstatat(self.parent.as_raw_fd(), &self.tail, 0).ok().map(|st| st.mode & 0o7777)
    }

    /// Replace the target: `bytes` into a new temp file `tmp_name` in the
    /// same directory, `mode` on it (ignored if it fails, as the tools do),
    /// then a rename onto the target, which replaces a final symlink rather
    /// than following it. The temp file is removed if the rename fails.
    pub fn replace(&self, tmp_name: &OsStr, bytes: &[u8], mode: Option<u32>) -> Result<(), WriteError> {
        let dir = self.parent.as_raw_fd();
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut file = std::fs::File::from(openat(dir, tmp_name, flags, 0o666).map_err(WriteError::Write)?);
        file.write_all(bytes).map_err(WriteError::Write)?;
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        drop(file);
        renameat(dir, tmp_name, &self.tail).map_err(|e| {
            let _ = unlinkat(dir, tmp_name);
            WriteError::Finalize(e)
        })
    }
}

/// `path` split into the directory its last component is resolved in and
/// that component as written (with any trailing `/`). An empty directory is
/// `.`; a path with no parent (`/`) is resolved as `/` + `.`.
#[cfg(unix)]
fn split(path: &Path) -> (PathBuf, OsString) {
    let all = path.as_os_str().as_bytes();
    let Some(parent) = path.parent() else {
        let dir = if all.is_empty() { PathBuf::from(".") } else { path.to_path_buf() };
        return (dir, OsString::from("."));
    };
    let pb = parent.as_os_str().as_bytes();
    let tail = match all.strip_prefix(pb) {
        Some(rest) => {
            let start = rest.iter().position(|b| *b != b'/').unwrap_or(rest.len());
            OsStr::from_bytes(&rest[start..]).to_os_string()
        }
        None => path.file_name().map(OsStr::to_os_string).unwrap_or_else(|| OsString::from(".")),
    };
    let dir = if pb.is_empty() { PathBuf::from(".") } else { parent.to_path_buf() };
    (dir, tail)
}

#[cfg(target_os = "linux")]
const DIR_PATH: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
/// No `O_PATH` off Linux: a directory handle needs read permission there.
#[cfg(all(unix, not(target_os = "linux")))]
const DIR_PATH: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

/// A handle on whatever a name leads to (following symlinks), to stat and
/// name it.
#[cfg(target_os = "linux")]
const ANY_PATH: libc::c_int = libc::O_PATH | libc::O_CLOEXEC;
/// Off Linux a read-only open that never waits and never takes a terminal;
/// what can't be opened so is left to the name checks, as a dangling link.
#[cfg(all(unix, not(target_os = "linux")))]
const ANY_PATH: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;

/// `dir` with each of `names` (as written, `/`-separated) appended: the
/// name a target would get once its missing directories exist. `None` if
/// one of them is `.` or `..` (that name can't be known ahead).
#[cfg(unix)]
fn prospective<'a>(dir: PathBuf, names: impl IntoIterator<Item = &'a OsStr>) -> Option<PathBuf> {
    let mut out = dir;
    for name in names {
        for seg in name.as_bytes().split(|b| *b == b'/').filter(|s| !s.is_empty()) {
            if seg == b"." || seg == b".." {
                return None;
            }
            out.push(OsStr::from_bytes(seg));
        }
    }
    Some(out)
}

#[cfg(unix)]
struct Stat {
    kind: libc::mode_t,
    mode: u32,
    id: (u64, u64),
}

#[cfg(unix)]
fn cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

#[cfg(unix)]
fn cvt(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `openat`, retried on EINTR. `mode` is used with `O_CREAT` only.
#[cfg(unix)]
fn openat(dir: RawFd, name: &OsStr, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
    let name = cstr(name)?;
    loop {
        // SAFETY: `name` is NUL-terminated; `dir` is an open descriptor or AT_FDCWD.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags, mode) };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by openat and has no other owner.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Open the directory `p` as a handle only, following symlinks.
#[cfg(unix)]
fn open_dir(p: &Path) -> io::Result<OwnedFd> {
    openat(libc::AT_FDCWD, p.as_os_str(), DIR_PATH, 0)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn stat_of(st: &libc::stat) -> Stat {
    Stat { kind: st.st_mode & libc::S_IFMT, mode: st.st_mode as u32, id: (st.st_dev as u64, st.st_ino as u64) }
}

#[cfg(unix)]
fn fstat(fd: RawFd) -> io::Result<Stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `st` is a valid out-pointer for one stat struct.
    cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
    // SAFETY: fstat succeeded, so `st` is initialised.
    Ok(stat_of(unsafe { &st.assume_init() }))
}

#[cfg(unix)]
fn fstatat(dir: RawFd, name: &OsStr, flags: libc::c_int) -> io::Result<Stat> {
    let name = cstr(name)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and `st` a valid out-pointer.
    cvt(unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), flags) })?;
    // SAFETY: fstatat succeeded, so `st` is initialised.
    Ok(stat_of(unsafe { &st.assume_init() }))
}

#[cfg(unix)]
fn mkdirat(dir: RawFd, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    let name = cstr(name)?;
    // SAFETY: `name` is NUL-terminated; `dir` is open.
    cvt(unsafe { libc::mkdirat(dir, name.as_ptr(), mode) })
}

#[cfg(unix)]
fn renameat(dir: RawFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let (from, to) = (cstr(from)?, cstr(to)?);
    // SAFETY: both names are NUL-terminated; `dir` is open.
    cvt(unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) })
}

#[cfg(unix)]
fn unlinkat(dir: RawFd, name: &OsStr) -> io::Result<()> {
    let name = cstr(name)?;
    // SAFETY: `name` is NUL-terminated; `dir` is open.
    cvt(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) })
}

/// The kernel's current absolute name for an open object. The kernel marks
/// an unlinked object's name with a trailing " (deleted)", but a live name
/// may end so too: such a name is used only if that exact path (not
/// followed) is this very object; otherwise the check fails closed.
#[cfg(target_os = "linux")]
fn fd_name(fd: BorrowedFd<'_>) -> io::Result<PathBuf> {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    if !link.is_absolute() {
        return Err(no_usable_name(&link));
    }
    if link.as_os_str().as_bytes().ends_with(b" (deleted)") {
        let there = std::fs::symlink_metadata(&link).map_err(|_| no_usable_name(&link))?;
        if (there.dev(), there.ino()) != fstat(fd.as_raw_fd())?.id {
            return Err(no_usable_name(&link));
        }
    }
    Ok(link)
}

/// macOS: the name `fcntl(F_GETPATH)` gives (as std's `File` debug output
/// gets it), used only if that exact path (not followed) is still this very
/// object; a stale name of a moved or removed object fails closed.
#[cfg(target_vendor = "apple")]
fn fd_name(fd: BorrowedFd<'_>) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX
    // bytes into `buf`, which holds PATH_MAX bytes; `fd` is open.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) })?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf.truncate(len);
    let name = PathBuf::from(OsString::from_vec(buf));
    if !name.is_absolute() {
        return Err(no_usable_name(&name));
    }
    let there = std::fs::symlink_metadata(&name).map_err(|_| no_usable_name(&name))?;
    if (there.dev(), there.ino()) != fstat(fd.as_raw_fd())?.id {
        return Err(no_usable_name(&name));
    }
    Ok(name)
}

/// Neither Linux nor macOS: no way to name an open object, so every check
/// that needs a name fails (closed: the write is refused).
#[cfg(all(unix, not(any(target_os = "linux", target_vendor = "apple"))))]
fn fd_name(fd: BorrowedFd<'_>) -> io::Result<PathBuf> {
    let _ = fd;
    Err(io::Error::new(io::ErrorKind::Unsupported, "this OS can't name an open directory"))
}

#[cfg(unix)]
fn no_usable_name(link: &Path) -> io::Error {
    io::Error::other(format!("no usable name ({})", link.display()))
}
