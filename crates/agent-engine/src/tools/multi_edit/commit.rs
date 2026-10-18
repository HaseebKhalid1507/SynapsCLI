//! Staged, re-checked, all-or-nothing commit of the planned file contents.
//!
//! Every step goes through a handle on each target's parent directory,
//! opened during planning from the canonical path the write-scope check saw
//! ([`super::dir`]); no step traverses a parent path again.
//!
//! 1. Stage: every new content goes into a sibling temp file created with
//!    `O_EXCL`. The temp of an existing file (or of a restore) is created
//!    owner-only (`0600`) and gets the file's mode on its descriptor only
//!    after every byte is written, so private content is never staged
//!    under broader permissions. A new file's temp is created `0666` (the
//!    kernel applies the umask, so `umask()` is never called in this
//!    multi-threaded process): the mode it is published with.
//! 2. Re-check, before anything is published: every parent path must still
//!    lead to the directory its handle holds; every temp must still be the
//!    file this call created; every existing target must still be the same
//!    inode with the same bytes we planned against, and every new target
//!    must still be absent. A mismatch removes the temps; nothing was
//!    changed.
//! 3. Finalize: rename each temp onto its target (a new file is linked in
//!    with `link(2)`, which never clobbers). If file k fails, files 1..k-1
//!    are restored from their in-memory originals and the error names
//!    exactly what happened to each file.
//!
//! No fsync, matching `edit`/`write`: a crash can lose the latest edit but
//! never leaves a torn file. The re-check-to-rename window is not locked.

use super::dir::{self, Dir, Kind, OpenError};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One file's planned change.
pub(crate) struct Pending {
    /// The path as the caller first gave it (for messages).
    pub display: String,
    /// The real file written (symlinks resolved).
    pub target: PathBuf,
    /// The verified parent directory of `target`.
    pub dir: Dir,
    /// The file name of `target` in `dir`.
    pub name: OsString,
    /// Bytes read during planning; `None` means the file is being created.
    pub original: Option<Vec<u8>>,
    /// `(dev, ino)` read during planning (Unix).
    pub identity: Option<(u64, u64)>,
    /// Permission bits to keep (Unix), `st_mode & 0o7777`.
    pub mode: Option<u32>,
    pub new: Vec<u8>,
}

/// A temp file this call created in a target's directory.
pub(crate) struct Staged {
    name: OsString,
    /// `(dev, ino)` of the file created (Unix).
    identity: Option<(u64, u64)>,
}

/// A filesystem primitive a test can declare unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Primitive {
    RenameNoReplace,
    Link,
}

/// Test seams. Production uses [`RealHooks`]; the `cfg(test)` methods do not
/// exist in a release build.
pub(crate) trait Hooks {
    /// Runs after every temp is staged and before the re-check.
    fn after_stage(&mut self) {}
    /// Finalizes file `index` (rename, or link for a new file).
    fn finalize(&mut self, _index: usize, staged: &Staged, pending: &Pending) -> std::io::Result<()> {
        #[cfg(test)]
        let mut unsupported = |p: Primitive| self.unsupported(p);
        #[cfg(not(test))]
        let mut unsupported = |_: Primitive| false;
        finalize(staged, pending, &mut unsupported)
    }
    /// Sees each temp after its bytes are written, before its final mode.
    #[cfg(test)]
    fn temp_written(&mut self, _file: &std::fs::File) {}
    /// Pretends `primitive` is not supported by the filesystem.
    #[cfg(test)]
    fn unsupported(&mut self, _primitive: Primitive) -> bool {
        false
    }
    /// Runs just before an existing file's bytes are read.
    #[cfg(test)]
    fn before_read(&mut self) {}
}

pub(crate) struct RealHooks;
impl Hooks for RealHooks {}

pub(crate) fn finalize(
    staged: &Staged,
    pending: &Pending,
    unsupported: &mut dyn FnMut(Primitive) -> bool,
) -> std::io::Result<()> {
    let dir = &pending.dir;
    if pending.original.is_some() {
        return dir.rename(&staged.name, &pending.name);
    }
    let linked = if unsupported(Primitive::Link) {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    } else {
        dir.link(&staged.name, &pending.name)
    };
    match linked {
        Ok(()) => {
            let _ = dir.unlink(&staged.name);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // A filesystem without hard links: fall back to check-then-rename.
        Err(_) => {
            if dir.lstat(&pending.name).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "target appeared before it could be created",
                ));
            }
            dir.rename(&staged.name, &pending.name)
        }
    }
}

/// A fresh temp name next to `p`'s target.
fn temp_name(p: &Pending) -> OsString {
    super::super::util::unique_tmp_path(Path::new(&p.name)).into_os_string()
}

/// Stage `bytes` in a new temp next to `p`'s target, keeping `p`'s mode
/// (or `0666 & ~umask` for a new file).
fn stage(p: &Pending, bytes: &[u8], hooks: &mut dyn Hooks) -> std::io::Result<Staged> {
    write_new(&p.dir, &temp_name(p), bytes, p.mode, hooks)
}

/// Write `bytes` to `name` in `dir`, created exclusively: an existing
/// name, a symlink included, is never followed, reused or removed. With
/// `mode` (an existing file's bits) the file is owner-only until the bytes
/// are written, then gets `mode`; without, it is `0666 & ~umask`
/// throughout. A file this call did create is removed again if writing it
/// fails.
#[cfg_attr(not(test), allow(unused_variables))]
pub(crate) fn write_new(
    dir: &Dir,
    name: &OsStr,
    bytes: &[u8],
    mode: Option<u32>,
    hooks: &mut dyn Hooks,
) -> std::io::Result<Staged> {
    let mut file = dir.create_new(name, if mode.is_some() { 0o600 } else { 0o666 })?;
    let written: std::io::Result<Option<(u64, u64)>> = (|| {
        file.write_all(bytes)?;
        #[cfg(test)]
        hooks.temp_written(&file);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        Ok(dir::meta_of_file(&file)?.identity)
    })();
    match written {
        Ok(identity) => Ok(Staged {
            name: name.to_os_string(),
            identity,
        }),
        Err(e) => {
            let _ = dir.unlink(name);
            Err(e)
        }
    }
}

/// A target read back: its bytes and identity, or `None` when it is not a
/// regular file any more.
type ReadBack = Option<(Vec<u8>, Option<(u64, u64)>)>;

/// Read `p`'s target back through its directory handle, at most `cap`
/// bytes.
fn read_target(p: &Pending, cap: usize) -> Result<ReadBack, OpenError> {
    let (file, meta) = p.dir.open_read(&p.name)?;
    if meta.kind != Kind::File {
        return Ok(None);
    }
    let bytes = dir::read_capped(file, cap).map_err(OpenError::Io)?;
    Ok(Some((bytes, meta.identity)))
}

/// Whether everything is still as staged: the parent path still leads to
/// the directory held, the temp is still the file created, and the target
/// still holds what planning read (or is still absent).
fn unchanged(p: &Pending, staged: &Staged) -> Result<(), String> {
    if !p.dir.still_there() {
        return Err(format!("the parent directory of {} was replaced", p.display));
    }
    match p.dir.lstat(&staged.name) {
        Ok(meta) if meta.kind == Kind::File && meta.identity == staged.identity => {}
        _ => return Err(format!("the staged temp of {} was replaced", p.display)),
    }
    match &p.original {
        None => match p.dir.lstat(&p.name) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(format!("{} was created by something else during the call", p.display)),
        },
        Some(original) => {
            let replaced = || format!("{} was replaced since it was read", p.display);
            let (now, identity) = read_target(p, original.len() + 1)
                .map_err(|e| match e {
                    OpenError::Symlink => replaced(),
                    OpenError::Io(e) => format!("{} can no longer be read: {e}", p.display),
                })?
                .ok_or_else(replaced)?;
            if identity != p.identity {
                return Err(format!("{} was replaced since it was read", p.display));
            }
            if &now != original {
                return Err(format!("{} changed since it was read", p.display));
            }
            Ok(())
        }
    }
}

/// Put back a file this call already committed. Only if it still holds the
/// bytes we wrote (otherwise someone else changed it after us: leave it).
fn restore(p: &Pending, hooks: &mut dyn Hooks) -> Result<(), String> {
    let (current, _) = read_target(p, p.new.len() + 1)
        .map_err(|e| format!("cannot read it back: {e}"))?
        .ok_or("it is not a regular file any more; left as is")?;
    if current != p.new {
        return Err("it changed again after this call wrote it; left as is".into());
    }
    match &p.original {
        None => p.dir.unlink(&p.name).map_err(|e| format!("cannot remove it: {e}")),
        Some(original) => {
            let tmp = stage(p, original, hooks).map_err(|e| format!("staging: {e}"))?;
            p.dir.rename(&tmp.name, &p.name).map_err(|e| {
                remove_temp(p, &tmp);
                format!("rename: {e}")
            })
        }
    }
}

/// Remove a temp, but only the file this call created: a name taken over
/// by someone else is left alone.
fn remove_temp(p: &Pending, staged: &Staged) {
    if p.dir
        .lstat(&staged.name)
        .is_ok_and(|meta| meta.kind == Kind::File && meta.identity == staged.identity)
    {
        let _ = p.dir.unlink(&staged.name);
    }
}

fn remove_all(files: &[Pending], temps: &[Staged]) {
    for (p, staged) in files.iter().zip(temps) {
        remove_temp(p, staged);
    }
}

/// Creates `name` in `parent` the way staging creates a temp (tests only).
#[cfg(test)]
pub(crate) fn create_temp_for_test(parent: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let dir = Dir::open(&std::fs::canonicalize(parent)?)?;
    write_new(&dir, OsStr::new(name), bytes, None, &mut RealHooks).map(drop)
}

pub(crate) fn commit(files: &[Pending], hooks: &mut dyn Hooks) -> Result<(), String> {
    let mut temps: Vec<Staged> = Vec::with_capacity(files.len());
    for p in files {
        match stage(p, &p.new, hooks) {
            Ok(staged) => temps.push(staged),
            Err(e) => {
                remove_all(files, &temps);
                return Err(format!("staging {}: {e}; no file was changed", p.display));
            }
        }
    }
    hooks.after_stage();
    for (p, staged) in files.iter().zip(&temps) {
        if let Err(e) = unchanged(p, staged) {
            remove_all(files, &temps);
            return Err(format!("{e}; no file was changed"));
        }
    }
    for (k, (p, staged)) in files.iter().zip(&temps).enumerate() {
        if let Err(e) = hooks.finalize(k, staged, p) {
            remove_all(&files[k..], &temps[k..]);
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for done in &files[..k] {
                match restore(done, hooks) {
                    Ok(()) => restored.push(done.display.clone()),
                    Err(why) => failed.push(format!("{} ({why})", done.display)),
                }
            }
            let never: Vec<String> = files[k..].iter().map(|f| f.display.clone()).collect();
            let list = |v: &[String]| {
                if v.is_empty() {
                    "none".to_string()
                } else {
                    v.join(", ")
                }
            };
            return Err(format!(
                "writing {} failed: {e}. Committed then restored: {}. Restore FAILED: {}. Never written: {}.",
                p.display,
                list(&restored),
                list(&failed),
                list(&never)
            ));
        }
    }
    Ok(())
}
