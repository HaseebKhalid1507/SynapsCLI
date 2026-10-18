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

pub(crate) fn finalize(_tmp: &Path, _pending: &Pending) -> std::io::Result<()> {
    Err(std::io::Error::other("not implemented"))
}

pub(crate) fn write_new(_tmp: &Path, _bytes: &[u8], _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn identity_of(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

pub(crate) fn commit(_files: &[Pending], _hooks: &mut dyn Hooks) -> Result<(), String> {
    Err("not implemented".into())
}
