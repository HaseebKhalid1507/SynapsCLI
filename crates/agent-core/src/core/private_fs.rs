//! Private filesystem helpers for sensitive application state.
//!
//! Policy (spec §5.4): directories `0700`, files and temp files `0600`,
//! symlink-safe opens, and atomic create-with-mode → write → rename so no
//! interval exists where a fresh file is broader than policy. Pre-existing
//! broader modes are repaired (chmod) on the next write. On non-Unix
//! platforms the helpers fall back to default platform semantics (symlink
//! refusal is still enforced via `symlink_metadata`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Policy mode for private files and temp files (Unix).
#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;
/// Policy mode for private directories (Unix).
#[cfg(unix)]
pub const DIR_MODE: u32 = 0o700;

/// Typed failure for private filesystem operations.
#[derive(Debug)]
pub enum PrivateFsError {
    /// A symlink was planted at the target path — refusing to write through it.
    SymlinkRefused(PathBuf),
    /// Underlying I/O failure.
    Io(std::io::Error),
}

impl std::fmt::Display for PrivateFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrivateFsError::SymlinkRefused(p) => {
                write!(f, "refusing to write through symlink at {}", p.display())
            }
            PrivateFsError::Io(e) => write!(f, "private fs io error: {e}"),
        }
    }
}

impl std::error::Error for PrivateFsError {}

impl From<std::io::Error> for PrivateFsError {
    fn from(e: std::io::Error) -> Self {
        PrivateFsError::Io(e)
    }
}

impl From<PrivateFsError> for std::io::Error {
    fn from(e: PrivateFsError) -> Self {
        match e {
            PrivateFsError::Io(io) => io,
            other => std::io::Error::new(std::io::ErrorKind::PermissionDenied, other.to_string()),
        }
    }
}

/// Refuse to operate on a path if a symlink is planted there.
fn refuse_symlink(path: &Path) -> Result<(), PrivateFsError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(PrivateFsError::SymlinkRefused(path.to_path_buf()))
        }
        _ => Ok(()),
    }
}

/// Create `dir` (and missing parents) with mode `0700`, and repair the leaf
/// directory to `0700` if a pre-existing one is broader than policy.
pub fn ensure_private_dir(dir: &Path) -> Result<(), PrivateFsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)?;
        // Repair a pre-existing broader-mode leaf dir (spec §5.4).
        let meta = std::fs::metadata(dir)?;
        if meta.permissions().mode() & 0o777 != DIR_MODE {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DIR_MODE))?;
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Open `path` for appending, creating it with mode `0600` (`O_NOFOLLOW` on
/// Unix). A symlink at the target yields [`PrivateFsError::SymlinkRefused`].
/// A pre-existing broader-mode file is repaired to `0600` via `fchmod` on the
/// already-open handle (no path re-resolution).
pub fn open_private_append(path: &Path) -> Result<File, PrivateFsError> {
    refuse_symlink(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    PrivateFsError::SymlinkRefused(path.to_path_buf())
                } else {
                    PrivateFsError::Io(e)
                }
            })?;
        let meta = file.metadata()?;
        if meta.permissions().mode() & 0o777 != FILE_MODE {
            file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        Ok(OpenOptions::new().create(true).append(true).open(path)?)
    }
}

/// Atomically replace `path` with `data`: the temp file is created in the
/// same directory with `create_new` + mode `0600` (never create-then-chmod,
/// so no interval is broader than policy), written, then renamed over the
/// target. A symlink at the target or temp path yields a typed refusal.
pub fn write_atomic_private(path: &Path, data: &[u8]) -> Result<(), PrivateFsError> {
    refuse_symlink(path)?;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| {
            PrivateFsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no file name",
            ))
        })?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    // Clear a stale temp file; `remove_file` unlinks a planted symlink itself,
    // never its target, so `create_new` below cannot be redirected.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let result = (|| -> Result<(), PrivateFsError> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        // Narrow the create→rename TOCTOU window: re-check the target just
        // before rename (rename itself replaces a symlink rather than
        // following it, so this is policy enforcement, not a traversal fix).
        refuse_symlink(path)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Shared umask isolation for tests. umask is process-global, so every test
/// that changes it must (a) hold the `#[serial(umask)]` serial_test key so no
/// two umask-mutating tests overlap, and (b) restore the old mask on drop —
/// even on panic — so sibling tests never observe a permissive mask.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    pub(crate) struct UmaskGuard {
        old: libc::mode_t,
    }

    impl UmaskGuard {
        pub(crate) fn set(mask: libc::mode_t) -> Self {
            Self {
                old: unsafe { libc::umask(mask) },
            }
        }
    }

    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.old);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_support::UmaskGuard;
    use super::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[serial(umask)]
    fn atomic_write_is_0600_and_dir_0700_under_permissive_umask() {
        let _umask = UmaskGuard::set(0);
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("private");
        ensure_private_dir(&dir).unwrap();
        let target = dir.join("state.json");
        write_atomic_private(&target, b"{}").unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    #[serial(umask)]
    fn append_creates_0600_under_permissive_umask() {
        let _umask = UmaskGuard::set(0);
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("log.jsonl");
        let mut f = open_private_append(&target).unwrap();
        f.write_all(b"line\n").unwrap();
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    fn append_refuses_symlink_with_typed_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        match open_private_append(&link) {
            Err(PrivateFsError::SymlinkRefused(p)) => assert_eq!(p, link),
            other => panic!("expected SymlinkRefused, got {other:?}"),
        }
    }

    #[test]
    fn atomic_write_refuses_symlink_with_typed_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "original").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        match write_atomic_private(&link, b"new") {
            Err(PrivateFsError::SymlinkRefused(p)) => assert_eq!(p, link),
            other => panic!("expected SymlinkRefused, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    }

    #[test]
    fn ensure_private_dir_repairs_broad_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("broad");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode_of(&dir), 0o700);
    }
}
