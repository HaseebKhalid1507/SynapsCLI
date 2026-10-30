//! Staged, re-checked commit of the planned file contents: every file is
//! planned and checked before any is published; publication then rolls back
//! best-effort, and a failed rollback is reported.
//!
//! Every step goes through a handle on each target's parent directory,
//! opened during planning by a no-follow walk of the canonical parent path
//! that was scope-checked ([`super::dir`]); no step traverses a parent path
//! again.
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
//! 3. Finalize: rename each temp onto its target. A new file is published
//!    only by a primitive that never replaces an existing name: an
//!    exclusive rename (`renameat2(RENAME_NOREPLACE)`, Linux) or else a
//!    hard link. With neither, the create is refused; there is no
//!    check-then-rename. If file k fails, files 1..k-1 are restored from
//!    their in-memory originals and the error names exactly what happened
//!    to each file. A temp an earlier file left over is tried once more
//!    (identity-checked) and named in the error, removed or still there.
//!
//! No fsync, matching `edit`/`write`. In normal operation a file is never
//! rewritten in place: its new content is published whole, by a rename (or
//! a link, for a new file), so a call never publishes a partial file.
//! Nothing is claimed about a crash or power loss: without fsync, the
//! filesystem decides what survives. The re-check-to-rename window is not
//! locked.

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
pub(crate) enum Primitive {
    RenameNoReplace,
    Link,
    /// Removing the temp name after a new file was published by a hard
    /// link (declared unsupported, the removal fails).
    UnlinkTemp,
}

/// Test seams. Production uses [`RealHooks`]; the `cfg(test)` methods do not
/// exist in a release build.
pub(crate) trait Hooks {
    /// Runs after every temp is staged and before the re-check.
    fn after_stage(&mut self) {}
    /// Finalizes file `index` (rename, or link for a new file). `Some` is a
    /// note for the result: published, but something was left over.
    fn finalize(&mut self, _index: usize, staged: &Staged, pending: &Pending) -> Published {
        #[cfg(test)]
        let mut unsupported = |p: Primitive| self.unsupported(p);
        #[cfg(not(test))]
        let mut unsupported = |_: Primitive| false;
        finalize(staged, pending, &mut unsupported)
    }
    /// Sees each temp after its bytes are written, before its final mode;
    /// an error fails the write.
    #[cfg(test)]
    fn temp_written(&mut self, _file: &std::fs::File) -> std::io::Result<()> {
        Ok(())
    }
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

/// A file's finalize: `Ok(Some(note))` when it is published but something
/// was left over (named in the result).
pub(crate) type Published = std::io::Result<Option<String>>;

pub(crate) fn finalize(
    staged: &Staged,
    pending: &Pending,
    unsupported: &mut dyn FnMut(Primitive) -> bool,
) -> Published {
    if pending.original.is_some() {
        return pending.dir.rename(&staged.name, &pending.name).map(|()| None);
    }
    create(staged, pending, unsupported)
}

/// Publish a new file without ever replacing a name that appeared in the
/// meantime: an exclusive rename where the system has one, else a hard
/// link (the temp name is then removed; if it can't be, the file is still
/// published and the note names the temp left over). With neither, refuse.
fn create(
    staged: &Staged,
    pending: &Pending,
    unsupported: &mut dyn FnMut(Primitive) -> bool,
) -> Published {
    let dir = &pending.dir;
    let not_here = || std::io::Error::from(std::io::ErrorKind::Unsupported);
    let renamed = if unsupported(Primitive::RenameNoReplace) {
        Err(not_here())
    } else {
        dir.rename_noreplace(&staged.name, &pending.name)
    };
    match renamed {
        Ok(()) => return Ok(None),
        Err(e) if !dir::rename_unsupported(&e) => return Err(e),
        Err(_) => {}
    }
    let linked = if unsupported(Primitive::Link) {
        Err(not_here())
    } else {
        dir.link(&staged.name, &pending.name)
    };
    match linked {
        Ok(()) => {
            let removed = if unsupported(Primitive::UnlinkTemp) {
                Err(not_here().to_string())
            } else {
                remove_if_ours(dir, &staged.name, staged.identity)
            };
            Ok(removed.err().map(|why| {
                format!(
                    "{} published; could not remove temp {}: {why}",
                    pending.display,
                    pending.target.with_file_name(&staged.name).display()
                )
            }))
        }
        Err(e) if dir::link_unsupported(&e) => Err(std::io::Error::other(format!(
            "cannot create {} without risking an overwrite on this filesystem",
            pending.display
        ))),
        Err(e) => Err(e),
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
/// throughout. If writing fails, the file is removed again, but only while
/// `name` still holds the file this call created.
#[cfg_attr(not(test), allow(unused_variables))]
pub(crate) fn write_new(
    dir: &Dir,
    name: &OsStr,
    bytes: &[u8],
    mode: Option<u32>,
    hooks: &mut dyn Hooks,
) -> std::io::Result<Staged> {
    let mut file = dir.create_new(name, if mode.is_some() { 0o600 } else { 0o666 })?;
    // What was created, so a failed write removes only that file.
    let created = dir::meta_of_file(&file).map(|meta| meta.identity);
    let written: std::io::Result<Option<(u64, u64)>> = (|| {
        file.write_all(bytes)?;
        #[cfg(test)]
        hooks.temp_written(&file)?;
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
            // Without its identity the file can't be told apart from one
            // renamed over it: it is left in place.
            if let Ok(identity) = created {
                let _ = remove_if_ours(dir, name, identity);
            }
            Err(e)
        }
    }
}

/// A target read back: its bytes and its identity.
type ReadBack = (Vec<u8>, Option<(u64, u64)>);

/// Read `p`'s target back through its directory handle (a regular file
/// only, never waiting on the open), at most `cap` bytes, with its
/// identity.
fn read_target(p: &Pending, cap: usize) -> Result<ReadBack, OpenError> {
    let (file, meta) = p.dir.open_regular(&p.name)?;
    let bytes = dir::read_capped(file, cap).map_err(OpenError::Io)?;
    Ok((bytes, meta.identity))
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
            let (now, identity) = read_target(p, original.len() + 1).map_err(|e| match e {
                OpenError::NotRegular | OpenError::Symlink => {
                    format!("{} was replaced since it was read", p.display)
                }
                OpenError::Io(e) => format!("{} can no longer be read: {e}", p.display),
            })?;
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
    let (current, _) =
        read_target(p, p.new.len() + 1).map_err(|e| format!("cannot read it back: {e}"))?;
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
/// by someone else is left alone, outside the window between
/// [`remove_if_ours`]'s check and its unlink.
fn remove_temp(p: &Pending, staged: &Staged) {
    let _ = remove_if_ours(&p.dir, &staged.name, staged.identity);
}

/// Remove `name` from `dir` only while it is the regular file with
/// `identity` (the file this call created). `Err` says why it is still
/// there; a name that is already gone is `Ok`. The check and the unlink are
/// separate steps: a file renamed onto `name` between them is removed.
fn remove_if_ours(dir: &Dir, name: &OsStr, identity: Option<(u64, u64)>) -> Result<(), String> {
    match dir.lstat(name) {
        Ok(meta) if meta.kind == Kind::File && meta.identity == identity => {
            dir.unlink(name).map_err(|e| e.to_string())
        }
        Ok(_) => Err("another file has taken its name; left as is".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn remove_all(files: &[Pending], temps: &[Staged]) {
    for (p, staged) in files.iter().zip(temps) {
        remove_temp(p, staged);
    }
}

/// Creates `name` in `parent` the way staging creates a temp (tests only).
#[cfg(all(test, unix))]
pub(crate) fn create_temp_for_test(parent: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let dir = Dir::open(&std::fs::canonicalize(parent)?)?;
    write_new(&dir, OsStr::new(name), bytes, None, &mut RealHooks).map(drop)
}

/// Publish every file (see the module docs): every file is planned and
/// checked before any is published; publication then rolls back
/// best-effort, and a failed rollback is reported. `Ok` holds a note per
/// file that was published with something left over.
pub(crate) fn commit(files: &[Pending], hooks: &mut dyn Hooks) -> Result<Vec<String>, String> {
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
    let mut notes: Vec<(usize, String)> = Vec::new();
    for (k, (p, staged)) in files.iter().zip(&temps).enumerate() {
        let e = match hooks.finalize(k, staged, p) {
            Ok(note) => {
                notes.extend(note.map(|note| (k, note)));
                continue;
            }
            Err(e) => e,
        };
        remove_all(&files[k..], &temps[k..]);
        let mut restored = Vec::new();
        let mut failed = Vec::new();
        for done in &files[..k] {
            match restore(done, hooks) {
                Ok(()) => restored.push(done.display.clone()),
                Err(why) => failed.push(format!("{} ({why})", done.display)),
            }
        }
        // A temp an earlier file left over is tried again, identity-checked
        // like every cleanup, and reported either way.
        let leftovers: String = notes
            .iter()
            .map(|(j, note)| {
                let temp = &temps[*j];
                let outcome = match remove_if_ours(&files[*j].dir, &temp.name, temp.identity) {
                    Ok(()) => "removed on retry".to_string(),
                    Err(why) => format!("still there: {why}"),
                };
                format!(" Left over earlier: {note}; {outcome}.")
            })
            .collect();
        let never: Vec<String> = files[k..].iter().map(|f| f.display.clone()).collect();
        let list = |v: &[String]| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.join(", ")
            }
        };
        return Err(format!(
            "writing {} failed: {e}. Committed then restored: {}. Restore FAILED: {}. Never written: {}.{leftovers}",
            p.display,
            list(&restored),
            list(&failed),
            list(&never)
        ));
    }
    Ok(notes.into_iter().map(|(_, note)| note).collect())
}
