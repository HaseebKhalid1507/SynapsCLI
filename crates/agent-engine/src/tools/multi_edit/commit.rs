//! Staged, re-checked, all-or-nothing commit of the planned file contents.
//!
//! 1. Stage: every new content goes into a sibling temp file created with
//!    `O_EXCL` and mode `0666` (the kernel applies the umask, so `umask()`
//!    is never called in this multi-threaded process); an existing file's
//!    mode is copied onto its temp.
//! 2. Re-check: every existing target must still be the same inode with the
//!    same bytes we planned against, and every new target must still be
//!    absent. A mismatch removes the temps; nothing was changed.
//! 3. Finalize: rename each temp onto its target (a new file is linked in
//!    with `link(2)`, which never clobbers). If file k fails, files 1..k-1
//!    are restored from their in-memory originals and the error names
//!    exactly what happened to each file.
//!
//! No fsync, matching `edit`/`write`: a crash can lose the latest edit but
//! never leaves a torn file. The re-check-to-rename window is not locked.

use std::io::Write;
use std::path::{Path, PathBuf};

/// One file's planned change.
pub(crate) struct Pending {
    /// The path as the caller first gave it (for messages).
    pub display: String,
    /// The real file written (symlinks resolved).
    pub target: PathBuf,
    /// Bytes read during planning; `None` means the file is being created.
    pub original: Option<Vec<u8>>,
    /// `(dev, ino)` read during planning (Unix).
    pub identity: Option<(u64, u64)>,
    /// Permission bits to keep (Unix), `st_mode & 0o7777`.
    pub mode: Option<u32>,
    pub new: Vec<u8>,
}

/// Test seams. Production uses [`RealHooks`].
pub(crate) trait Hooks {
    /// Runs after every temp is staged and before the re-check.
    fn after_stage(&mut self) {}
    /// Finalizes file `index` (rename, or link for a new file).
    fn finalize(&mut self, _index: usize, tmp: &Path, pending: &Pending) -> std::io::Result<()> {
        finalize(tmp, pending)
    }
}

pub(crate) struct RealHooks;
impl Hooks for RealHooks {}

pub(crate) fn finalize(tmp: &Path, pending: &Pending) -> std::io::Result<()> {
    if pending.original.is_some() {
        return std::fs::rename(tmp, &pending.target);
    }
    match std::fs::hard_link(tmp, &pending.target) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // A filesystem without hard links: fall back to check-then-rename.
        Err(_) => {
            if std::fs::symlink_metadata(&pending.target).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "target appeared before it could be created",
                ));
            }
            std::fs::rename(tmp, &pending.target)
        }
    }
}

/// Create a sibling temp file of `target` holding `bytes`, with `mode`
/// (or `0666 & ~umask` for a new file).
fn stage(target: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<PathBuf> {
    let tmp = super::super::util::unique_tmp_path(target);
    write_new(&tmp, bytes, mode)?;
    Ok(tmp)
}

/// Write `bytes` to a file created at `tmp` exclusively: an existing path,
/// a symlink included, is never followed, reused or removed. A file this
/// call did create is removed again if writing it fails.
pub(crate) fn write_new(tmp: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o666);
    }
    let mut file = options.open(tmp)?;
    let written: std::io::Result<()> = (|| {
        file.write_all(bytes)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    written
}

#[cfg(unix)]
pub(crate) fn identity_of(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
pub(crate) fn identity_of(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Whether the target still holds what planning read (or is still absent).
fn unchanged(p: &Pending) -> Result<(), String> {
    match &p.original {
        None => match std::fs::symlink_metadata(&p.target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(format!("{} was created by something else during the call", p.display)),
        },
        Some(original) => {
            let meta = std::fs::metadata(&p.target)
                .map_err(|e| format!("{} can no longer be read: {e}", p.display))?;
            if identity_of(&meta) != p.identity {
                return Err(format!("{} was replaced since it was read", p.display));
            }
            let now = std::fs::read(&p.target)
                .map_err(|e| format!("{} can no longer be read: {e}", p.display))?;
            if &now != original {
                return Err(format!("{} changed since it was read", p.display));
            }
            Ok(())
        }
    }
}

/// Put back a file this call already committed. Only if it still holds the
/// bytes we wrote (otherwise someone else changed it after us: leave it).
fn restore(p: &Pending) -> Result<(), String> {
    let current = std::fs::read(&p.target).map_err(|e| format!("cannot read it back: {e}"))?;
    if current != p.new {
        return Err("it changed again after this call wrote it; left as is".into());
    }
    match &p.original {
        None => std::fs::remove_file(&p.target).map_err(|e| format!("cannot remove it: {e}")),
        Some(original) => {
            let tmp = stage(&p.target, original, p.mode).map_err(|e| format!("staging: {e}"))?;
            std::fs::rename(&tmp, &p.target).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("rename: {e}")
            })
        }
    }
}

fn remove_all(temps: &[PathBuf]) {
    for tmp in temps {
        let _ = std::fs::remove_file(tmp);
    }
}

pub(crate) fn commit(files: &[Pending], hooks: &mut dyn Hooks) -> Result<(), String> {
    let mut temps: Vec<PathBuf> = Vec::with_capacity(files.len());
    for p in files {
        match stage(&p.target, &p.new, p.mode) {
            Ok(tmp) => temps.push(tmp),
            Err(e) => {
                remove_all(&temps);
                return Err(format!("staging {}: {e}; no file was changed", p.display));
            }
        }
    }
    hooks.after_stage();
    for p in files {
        if let Err(e) = unchanged(p) {
            remove_all(&temps);
            return Err(format!("{e}; no file was changed"));
        }
    }
    for (k, (p, tmp)) in files.iter().zip(&temps).enumerate() {
        if let Err(e) = hooks.finalize(k, tmp, p) {
            remove_all(&temps[k..]);
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for done in &files[..k] {
                match restore(done) {
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
