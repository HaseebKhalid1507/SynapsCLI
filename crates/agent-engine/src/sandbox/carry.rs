//! Carrying a session's policy file to its linked successor at compaction.
//!
//! A linked-successor compaction gives the conversation a new journal id, and
//! policy files are named by that id. `apply_compaction` carries
//! `<prev>.conf` to `<new>.conf` BEFORE the successor's journal is saved, so a
//! successor never exists without its predecessor's policy; if the carry
//! fails, the compaction fails and the session stays on its predecessor,
//! still under its own policy. A later failure of the transition rolls back
//! the file this carry created, and only that file.
//!
//! The predecessor's file is read the way a spawn reads it (one open, no
//! symlink, a regular file with one name, capped) and validated with the same
//! parse; the copy has the same bytes and permission bits (rwx only), is
//! written to a temp name (`.`-prefixed, so never read as a policy), fsynced,
//! and renamed into place with `renameat2(RENAME_NOREPLACE)`: an existing
//! `<new>.conf` is kept, never replaced. The new file's identity (dev/ino) is
//! taken before the rename; a step that fails after it removes the new file
//! again (if it is still this one) before the error returns. Each result logs
//! one `sandbox.carry` line, which never quotes the policy text; a
//! predecessor without a file carries nothing and logs nothing.
//!
//! Off Linux there is no `renameat2`: the temp file is hard-linked to the
//! new name (`linkat`, which never replaces either) and then unlinked.
//!
//! Off Unix (Windows) a policy file is refused unread, so there is never
//! one to carry; the write step is compiled out and fails closed.

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Write as _;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[cfg(not(unix))]
use super::nonunix::MetadataExt;
use super::policy::{self, PolicyEnv};

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// The temp file's mode at creation; the predecessor's mode is set after.
#[cfg(unix)]
const TMP_MODE: libc::c_uint = 0o600;

/// What a carry did.
#[derive(Debug)]
pub enum Carried {
    /// The predecessor has no policy file (or is the same session): nothing
    /// to carry, and no line is logged.
    Nothing,
    /// The successor already had a policy file: kept as it is.
    Kept,
    /// The policy file this carry created, for a rollback.
    Copied(Created),
}

/// A policy file created by a carry: its path and identity.
#[derive(Debug)]
pub struct Created {
    path: PathBuf,
    dev: u64,
    ino: u64,
    prev: String,
    new: String,
}

/// A session id as it appears in a log line: ids that can't name a policy
/// file are not printed.
fn shown(id: &str) -> &str {
    if policy::valid_session_id(id) {
        id
    } else {
        "<invalid>"
    }
}

/// The `sandbox.carry` line for one result (`result` is fixed text; `reason`
/// is free text and comes last).
pub fn carry_line(new: &str, prev: &str, result: &str, reason: Option<&str>) -> String {
    let mut line = format!("sandbox.carry session={} from={} result={result}", shown(new), shown(prev));
    if let Some(r) = reason {
        line.push_str(" reason=");
        line.push_str(&r.replace(['\n', '\r'], " "));
    }
    line
}

/// Carry `prev`'s policy file to `new`, logging the result with `log`.
/// `Err` (already logged) means nothing was created: the caller must not
/// proceed as if `new` had the predecessor's policy. The error and the line
/// never quote the policy file's text (`PolicyError::summary`).
pub fn carry(env: &PolicyEnv, prev: &str, new: &str, log: &mut dyn FnMut(String)) -> Result<Carried, String> {
    carry_inner(env, prev, new, log, &mut || {}, &mut || {})
}

/// [`carry`] with hooks run just before and just after the rename (tests: a
/// racing or replacing file).
pub(crate) fn carry_inner(
    env: &PolicyEnv,
    prev: &str,
    new: &str,
    log: &mut dyn FnMut(String),
    before_rename: &mut dyn FnMut(),
    after_rename: &mut dyn FnMut(),
) -> Result<Carried, String> {
    if prev == new {
        return Ok(Carried::Nothing);
    }
    // An id that can't name a file has no policy to carry.
    let Some(prev_path) = env.policy_path(prev) else {
        return Ok(Carried::Nothing);
    };
    let result = carry_file(env, prev, new, &prev_path, before_rename, after_rename);
    match &result {
        Ok(Carried::Nothing) => {}
        Ok(Carried::Kept) => log(carry_line(new, prev, "kept", None)),
        Ok(Carried::Copied(_)) => log(carry_line(new, prev, "copied", None)),
        Err(reason) => log(carry_line(new, prev, "failed", Some(reason))),
    }
    result
}

fn carry_file(
    env: &PolicyEnv,
    prev: &str,
    new: &str,
    prev_path: &Path,
    before_rename: &mut dyn FnMut(),
    after_rename: &mut dyn FnMut(),
) -> Result<Carried, String> {
    let Some((text, meta)) = policy::read_policy_file_meta(prev_path).map_err(|e| e.summary())? else {
        return Ok(Carried::Nothing);
    };
    policy::parse(&text, prev_path, env).map_err(|e| format!("the predecessor's policy is invalid: {}", e.summary()))?;
    let new_path = env.policy_path(new).ok_or_else(|| "the new session id can't name a policy file".to_string())?;
    match std::fs::symlink_metadata(&new_path) {
        Ok(_) => return Ok(Carried::Kept),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot check `{}`: {e}", new_path.display())),
    }
    let target = Target { new_path: &new_path, prev, new };
    match write_new(env, &target, text.as_bytes(), meta.mode() & 0o777, before_rename, after_rename)? {
        Some(created) => Ok(Carried::Copied(created)),
        None => Ok(Carried::Kept),
    }
}

/// Where a carry writes: the successor's file and both ids (for its line).
struct Target<'a> {
    new_path: &'a Path,
    prev: &'a str,
    new: &'a str,
}

/// A test seam: one injected failure at a fallible step of [`write_new`], on
/// this thread only (scratch fault injection; nothing else sets it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The temp file's fstat (before the rename).
    TempStat,
    /// The sessions dir's fsync (after the rename).
    DirSync,
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
}

/// Arm (or clear) [`Fault`] for the next carry on this thread.
#[cfg(test)]
pub(crate) fn inject(fault: Option<Fault>) {
    FAULT.with(|f| f.set(fault));
}

/// `Err` once if `at` is armed on this thread (tests); always `Ok` otherwise.
fn fault(at: Fault) -> io::Result<()> {
    #[cfg(test)]
    if FAULT.with(|f| f.get()) == Some(at) {
        FAULT.with(|f| f.set(None));
        return Err(io::Error::other(format!("injected {at:?} failure")));
    }
    let _ = at;
    Ok(())
}

/// Write `bytes` with `mode` to a temp file in the sessions dir, fsync it and
/// rename it to `new_path` without replacing anything. `Some` = the file this
/// carry created, `None` = a file appeared at `new_path` first (kept). The
/// temp file never survives. The new file's identity is taken from the temp
/// file before the rename; from the rename on, a failed step removes the new
/// file (if it is still this one) before returning the error, which says so.
#[cfg(unix)]
fn write_new(
    env: &PolicyEnv,
    target: &Target<'_>,
    bytes: &[u8],
    mode: u32,
    before_rename: &mut dyn FnMut(),
    after_rename: &mut dyn FnMut(),
) -> Result<Option<Created>, String> {
    let (new, new_path) = (target.new, target.new_path);
    let dir_path = env.sessions_dir();
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&dir_path)
        .map_err(|e| format!("cannot open `{}`: {e}", dir_path.display()))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = CString::new(format!(".{new}.conf.carry-{}-{nanos}", std::process::id()))
        .map_err(|_| "bad temp file name".to_string())?;
    let name = CString::new(format!("{new}.conf")).map_err(|_| "bad file name".to_string())?;
    // SAFETY: `dir` is an open directory fd and `tmp` a NUL-terminated name;
    // on success the returned fd is owned by the File below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            TMP_MODE,
        )
    };
    if fd < 0 {
        return Err(format!("cannot create a temp file in `{}`: {}", dir_path.display(), io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just returned by openat and nothing else owns it.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let unlink_tmp = || {
        // SAFETY: `dir` is open and `tmp` is NUL-terminated.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
    };
    let written = file
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        unlink_tmp();
        return Err(format!("cannot write the copy in `{}`: {e}", dir_path.display()));
    }
    // The identity the rollback checks: the temp file's, before it has the
    // successor's name.
    let meta = match fault(Fault::TempStat).and_then(|()| file.metadata()) {
        Ok(m) => m,
        Err(e) => {
            unlink_tmp();
            return Err(format!("cannot stat the copy in `{}`: {e}", dir_path.display()));
        }
    };
    before_rename();
    if let Err(e) = rename_noreplace(dir.as_raw_fd(), &tmp, &name) {
        unlink_tmp();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(None);
        }
        return Err(format!("cannot rename the copy to `{}`: {e}", new_path.display()));
    }
    // From here the successor's file exists, and `created` owns it.
    let created = Created {
        path: new_path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        prev: target.prev.to_string(),
        new: new.to_string(),
    };
    after_rename();
    if let Err(e) = fault(Fault::DirSync).and_then(|()| dir.sync_all()) {
        let cleanup = match created.remove_if_ours() {
            Ok(()) => "the copy was removed".to_string(),
            Err(why) => format!("the copy was not removed: {why}"),
        };
        return Err(format!("cannot sync `{}`: {e}; {cleanup}", dir_path.display()));
    }
    Ok(Some(created))
}

/// [`write_new`] off Unix (Windows): never reached (a policy file is
/// refused unread there, so there is nothing to carry); fails closed.
#[cfg(not(unix))]
fn write_new(
    env: &PolicyEnv,
    target: &Target<'_>,
    bytes: &[u8],
    mode: u32,
    before_rename: &mut dyn FnMut(),
    after_rename: &mut dyn FnMut(),
) -> Result<Option<Created>, String> {
    let _ = (env, bytes, mode, before_rename, after_rename);
    Err(format!("cannot write `{}`: sandbox policy files need a Unix host", target.new_path.display()))
}

/// Rename `from` to `to` in the directory `dir`, never replacing an
/// existing `to` (`EEXIST`).
#[cfg(target_os = "linux")]
fn rename_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and both fds are this open
    // directory; renameat2 takes no other pointers.
    let rc = unsafe { libc::syscall(libc::SYS_renameat2, dir, from.as_ptr(), dir, to.as_ptr(), libc::RENAME_NOREPLACE) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn rename_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    link_noreplace(dir, from, to)
}

/// [`rename_noreplace`] without `renameat2`: hard-link `from` to `to`
/// (`EEXIST` if `to` exists), then unlink `from`. Between the two both names
/// exist; `from` is a `.`-prefixed temp name, never read as a policy. A
/// failed unlink leaves only that temp name behind.
#[cfg(all(unix, any(test, not(target_os = "linux"))))]
fn link_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and `dir` is an open directory.
    if unsafe { libc::linkat(dir, from.as_ptr(), dir, to.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    unsafe { libc::unlinkat(dir, from.as_ptr(), 0) };
    Ok(())
}

impl Created {
    /// Remove the created file if it is still the one this carry made (same
    /// device and inode, a regular file); anything else at that path is kept.
    /// Logs `rolled-back` or `rollback-kept`.
    pub fn rollback(self, log: &mut dyn FnMut(String)) {
        match self.remove_if_ours() {
            Ok(()) => log(carry_line(&self.new, &self.prev, "rolled-back", None)),
            Err(why) => log(carry_line(&self.new, &self.prev, "rollback-kept", Some(&why))),
        }
    }

    /// Remove the file if it is still this carry's (`lstat` then `unlink`:
    /// not atomic, so an unconfined process replacing the file in between
    /// can still lose its file; see docs/sandbox.md). `Err` = kept, and why.
    fn remove_if_ours(&self) -> Result<(), String> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_file() && m.dev() == self.dev && m.ino() == self.ino => {
                std::fs::remove_file(&self.path).map_err(|e| format!("cannot remove: {e}"))
            }
            Ok(_) => Err("replaced since the carry".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err("already gone".into()),
            Err(e) => Err(format!("cannot check: {e}")),
        }
    }
}
