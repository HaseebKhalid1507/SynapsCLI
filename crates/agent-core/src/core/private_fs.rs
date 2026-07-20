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

// ─── CP-13 fix2: directory-handle-relative confined creation ─────────────────

/// A held `O_DIRECTORY` handle beneath a trusted root. Every operation is
/// RELATIVE to this handle (`openat`/`mkdirat`/`unlinkat`), each component
/// is opened `O_NOFOLLOW`, and no path is ever re-resolved after a check —
/// the check IS the open, so ancestor-symlink plants and concurrent
/// component swaps fail closed (`ELOOP`/`ENOTDIR`) instead of escaping.
///
/// On Linux, multi-component descents additionally try `openat2` with
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` for kernel-side atomic
/// resolution, falling back to the component-by-component walk when the
/// syscall is unavailable (`ENOSYS`). Unix-only by design: confined export
/// fails closed on other platforms.
#[cfg(unix)]
#[derive(Debug)]
pub struct ConfinedDir {
    handle: File,
}

#[cfg(unix)]
impl ConfinedDir {
    /// Open the TRUSTED export root itself, creating it 0700 if missing.
    /// The final component must not be a symlink (`O_NOFOLLOW`); the
    /// root's own ancestors are the caller's trusted input.
    pub fn create_root(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let dir = Self::open_dir_nofollow_at_path(path)?;
        dir.fchmod_private()?;
        Ok(dir)
    }

    /// Open an EXISTING trusted root without creating it.
    pub fn open_root(path: &Path) -> std::io::Result<Self> {
        Self::open_dir_nofollow_at_path(path)
    }

    /// Open an EXISTING absolute directory with EVERY component — ancestors
    /// AND the final one — resolved handle-relatively and symlink-refusing
    /// (fix2). Unlike [`Self::open_root`], whose pathname open lets the
    /// kernel follow symlinked ANCESTORS, this walks from `/` (which cannot
    /// be a symlink): Linux tries one atomic `openat2` with
    /// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` from the root handle, and
    /// the fallback opens each component `O_NOFOLLOW | O_DIRECTORY`. There
    /// is no check-then-open race — a component swapped to a symlink at any
    /// point fails the open itself (`ELOOP`/`ENOTDIR`).
    ///
    /// TRUSTED-ROOT SEMANTICS: nothing on the path is trusted; every
    /// component must be a real directory. Operators whose base dir
    /// legitimately sits behind ancestor symlinks (e.g. `/home` →
    /// `var/home`) must point `SYNAPS_BASE_DIR` at the canonical path.
    pub fn open_absolute_no_symlinks(path: &Path) -> std::io::Result<Self> {
        let components = absolute_real_components(path)?;
        let root = Self::open_dir_nofollow_at_path(Path::new("/"))?;
        if components.is_empty() {
            return Ok(root);
        }
        #[cfg(target_os = "linux")]
        {
            let joined = components.join("/");
            match root.openat2_beneath(&joined, libc::O_RDONLY | libc::O_DIRECTORY) {
                Ok(handle) => return Ok(Self { handle }),
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {} // fall back
                Err(e) => return Err(confinement_error(&joined, &e)),
            }
        }
        let mut dir = root;
        for component in &components {
            dir = dir.open_child_dir_nofollow(component)?;
        }
        Ok(dir)
    }

    /// Like [`Self::open_absolute_no_symlinks`], but creates missing
    /// directory components (mode 0700) during the walk and repairs the
    /// LEAF to 0700 via `fchmod` on the opened handle — the handle-relative
    /// counterpart of [`ensure_private_dir`]. Every existing component,
    /// ancestor or final, must be a real (non-symlink) directory.
    pub fn create_absolute_no_symlinks(path: &Path) -> std::io::Result<Self> {
        let components = absolute_real_components(path)?;
        let mut dir = Self::open_dir_nofollow_at_path(Path::new("/"))?;
        for component in &components {
            dir = match dir.open_child_dir_nofollow(component) {
                Ok(next) => next,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let c_name = validated_component_cstring(component)?;
                    let rc = unsafe { libc::mkdirat(dir.fd(), c_name.as_ptr(), 0o700) };
                    if rc != 0 {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() != Some(libc::EEXIST) {
                            return Err(err);
                        }
                    }
                    dir.open_child_dir_nofollow(component)?
                }
                Err(e) => return Err(e),
            };
        }
        dir.fchmod_private()?;
        Ok(dir)
    }

    /// Open one existing DIRECT child directory, `O_NOFOLLOW | O_DIRECTORY`.
    fn open_child_dir_nofollow(&self, name: &str) -> std::io::Result<Self> {
        let c_name = validated_component_cstring(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(confinement_error(name, &err));
        }
        use std::os::unix::io::FromRawFd;
        Ok(Self {
            handle: unsafe { File::from_raw_fd(fd) },
        })
    }

    fn open_dir_nofollow_at_path(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| confinement_error(path.to_string_lossy().as_ref(), &e))?;
        Ok(Self { handle })
    }

    /// Repair this directory's mode to 0700 via `fchmod` on the OPENED
    /// handle (no path re-resolution; immune to umask and to pre-existing
    /// broad modes).
    fn fchmod_private(&self) -> std::io::Result<()> {
        let rc = unsafe { libc::fchmod(self.fd(), 0o700) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn fd(&self) -> libc::c_int {
        use std::os::unix::io::AsRawFd;
        self.handle.as_raw_fd()
    }

    /// Open-or-create one DIRECT child directory (validated single
    /// component), 0700, refusing symlinks even when swapped in between
    /// operations.
    pub fn child_dir(&self, name: &str) -> std::io::Result<Self> {
        let c_name = validated_component_cstring(name)?;
        // mkdirat: EEXIST is fine — the subsequent O_NOFOLLOW open decides
        // whether the existing entry is an acceptable real directory.
        let rc = unsafe { libc::mkdirat(self.fd(), c_name.as_ptr(), 0o700) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EEXIST) {
                return Err(err);
            }
        }
        let fd = unsafe {
            libc::openat(
                self.fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(confinement_error(name, &err));
        }
        use std::os::unix::io::FromRawFd;
        let dir = Self {
            handle: unsafe { File::from_raw_fd(fd) },
        };
        dir.fchmod_private()?;
        Ok(dir)
    }

    /// Descend through validated relative directory components, creating
    /// each as needed (0700). Component-by-component `O_NOFOLLOW` opens.
    pub fn create_dirs(&self, components: &[String]) -> std::io::Result<Self> {
        let mut dir = self.try_clone()?;
        for component in components {
            dir = dir.child_dir(component)?;
        }
        Ok(dir)
    }

    /// Descend through EXISTING validated relative directory components
    /// without creating anything. Tries Linux `openat2` with
    /// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` for the whole descent;
    /// falls back to the component walk when unavailable.
    pub fn open_dirs(&self, components: &[String]) -> std::io::Result<Self> {
        if components.is_empty() {
            return self.try_clone();
        }
        for component in components {
            let _ = validated_component_cstring(component)?;
        }
        #[cfg(target_os = "linux")]
        {
            let joined = components.join("/");
            match self.openat2_beneath(&joined, libc::O_RDONLY | libc::O_DIRECTORY) {
                Ok(handle) => return Ok(Self { handle }),
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {} // fall back
                Err(e) => return Err(confinement_error(&joined, &e)),
            }
        }
        let mut dir = self.try_clone()?;
        for component in components {
            let c_name = validated_component_cstring(component)?;
            let fd = unsafe {
                libc::openat(
                    dir.fd(),
                    c_name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                return Err(confinement_error(component, &err));
            }
            use std::os::unix::io::FromRawFd;
            dir = Self {
                handle: unsafe { File::from_raw_fd(fd) },
            };
        }
        Ok(dir)
    }

    #[cfg(target_os = "linux")]
    fn openat2_beneath(&self, rel: &str, flags: libc::c_int) -> std::io::Result<File> {
        let c_rel = std::ffi::CString::new(rel)
            .map_err(|_| std::io::Error::other("NUL in confined path"))?;
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (flags | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.fd(),
                c_rel.as_ptr(),
                &mut how as *mut libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        use std::os::unix::io::FromRawFd;
        Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
    }

    /// Open a descendant FILE for reading via handle-relative resolution
    /// (CP-13 fix3): Linux tries `openat2` with `RESOLVE_BENEATH |
    /// RESOLVE_NO_SYMLINKS` over the whole validated relative path, falling
    /// back to the component `O_NOFOLLOW` directory walk plus a final
    /// `openat(O_RDONLY|O_NOFOLLOW)`. The opened handle must be a regular
    /// file by handle metadata. No full-path re-open ever happens — an
    /// ancestor swapped to a symlink after discovery is refused here.
    pub fn open_file(&self, components: &[String]) -> std::io::Result<File> {
        let (name, dirs) = components
            .split_last()
            .ok_or_else(|| std::io::Error::other("empty confined file path"))?;
        for component in components {
            let _ = validated_component_cstring(component)?;
        }
        #[cfg(target_os = "linux")]
        {
            let joined = components.join("/");
            match self.openat2_beneath(&joined, libc::O_RDONLY | libc::O_NOFOLLOW) {
                Ok(file) => {
                    let meta = file.metadata()?;
                    if !meta.is_file() {
                        return Err(std::io::Error::other(format!(
                            "confined source {joined:?} is not a regular file"
                        )));
                    }
                    return Ok(file);
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {} // fall back
                Err(e) => return Err(confinement_error(&joined, &e)),
            }
        }
        let dir = self.open_dirs(dirs)?;
        let c_name = validated_component_cstring(name)?;
        let fd = unsafe {
            libc::openat(
                dir.fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(confinement_error(name, &err));
        }
        use std::os::unix::io::FromRawFd;
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other(format!(
                "confined source {name:?} is not a regular file"
            )));
        }
        Ok(file)
    }

    /// Create a file (validated single component) `O_CREAT|O_EXCL|
    /// O_NOFOLLOW` 0600 relative to this handle. An existing entry —
    /// including any symlink, even one swapped in concurrently — fails.
    pub fn create_file(&self, name: &str) -> std::io::Result<File> {
        let c_name = validated_component_cstring(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd(),
                c_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(confinement_error(name, &err));
        }
        use std::os::unix::io::FromRawFd;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Unlink a file (validated single component) relative to this handle.
    /// Handle-relative atomic private write (fix2): `<name>.tmp` is created
    /// `O_CREAT|O_EXCL|O_NOFOLLOW` 0600 relative to THIS handle, written,
    /// fsynced, and `renameat`ed over `<name>` — `renameat` never follows
    /// the target, so a planted symlink is replaced as a link, never
    /// written through. A pre-existing symlink at the target is refused
    /// with a typed error (policy parity with [`write_atomic_private`]);
    /// one planted after the check merely gets atomically replaced.
    pub fn write_atomic(&self, name: &str, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let c_final = validated_component_cstring(name)?;
        // Refuse a pre-existing symlink target (typed policy refusal).
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                self.fd(),
                c_final.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            return Err(std::io::Error::other(format!(
                "confinement violation at {name:?}: refusing symlink write target"
            )));
        }

        let tmp_name = format!("{name}.tmp");
        // Clear a stale temp (unlinkat removes a planted symlink itself,
        // never its target), then create fresh with O_EXCL.
        match self.remove_file(&tmp_name) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = self.create_file(&tmp_name)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);

        let c_tmp = validated_component_cstring(&tmp_name)?;
        let rc = unsafe { libc::renameat(self.fd(), c_tmp.as_ptr(), self.fd(), c_final.as_ptr()) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            let _ = self.remove_file(&tmp_name);
            return Err(err);
        }
        Ok(())
    }

    /// Handle-relative private append (fix2): open `<name>` for appending
    /// relative to THIS handle with `O_NOFOLLOW`, creating it 0600; a
    /// pre-existing broader-mode file is repaired via the opened handle —
    /// parity with [`open_private_append`], minus the pathname resolution.
    pub fn append_file(&self, name: &str) -> std::io::Result<File> {
        let c_name = validated_component_cstring(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd(),
                c_name.as_ptr(),
                libc::O_WRONLY
                    | libc::O_APPEND
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(confinement_error(name, &err));
        }
        use std::os::unix::io::FromRawFd;
        let file = unsafe { File::from_raw_fd(fd) };
        use std::os::unix::fs::PermissionsExt;
        let meta = file.metadata()?;
        if meta.permissions().mode() & 0o777 != FILE_MODE {
            file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
        }
        Ok(file)
    }

    pub fn remove_file(&self, name: &str) -> std::io::Result<()> {
        let c_name = validated_component_cstring(name)?;
        let rc = unsafe { libc::unlinkat(self.fd(), c_name.as_ptr(), 0) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            handle: self.handle.try_clone()?,
        })
    }
}

/// One entry discovered from a held directory handle. Discovery never
/// re-resolves the directory by path; callers still perform the authoritative
/// open relative to the same handle, so entry replacement races fail closed.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedEntry {
    pub name: String,
    pub is_file: bool,
    pub is_dir: bool,
    /// Modification time (ms since epoch) from the handle-relative
    /// `fstatat(AT_SYMLINK_NOFOLLOW)` — of the entry ITSELF, never a
    /// symlink target.
    pub mtime_unix_ms: Option<u64>,
    /// File length from the same handle-relative stat.
    pub byte_len: u64,
}

#[cfg(unix)]
impl ConfinedDir {
    /// Enumerate direct children via `fdopendir(dup(dirfd))` + `readdir` and
    /// classify each with handle-relative `fstatat(AT_SYMLINK_NOFOLLOW)`.
    /// Symlinks are returned as neither file nor directory. Names that are
    /// non-UTF8 or invalid confined components are omitted.
    pub fn entries(&self) -> std::io::Result<Vec<ConfinedEntry>> {
        let duplicated = unsafe { libc::dup(self.fd()) };
        if duplicated < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let directory = unsafe { libc::fdopendir(duplicated) };
        if directory.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(duplicated) };
            return Err(error);
        }

        let mut entries = Vec::new();
        loop {
            let raw = unsafe { libc::readdir(directory) };
            if raw.is_null() {
                break;
            }
            let name_bytes = unsafe { std::ffi::CStr::from_ptr((*raw).d_name.as_ptr()) };
            let Ok(name) = name_bytes.to_str() else {
                continue;
            };
            if name == "." || name == ".." || validated_component_cstring(name).is_err() {
                continue;
            }
            let c_name = validated_component_cstring(name)?;
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::fstatat(
                    self.fd(),
                    c_name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc != 0 {
                // Entry disappeared or changed during enumeration. The
                // authoritative later open decides; there is nothing safe to
                // report from this stale directory record.
                continue;
            }
            let kind = stat.st_mode & libc::S_IFMT;
            #[allow(clippy::useless_conversion)]
            let mtime_unix_ms = u64::try_from(stat.st_mtime)
                .ok()
                .map(|secs| secs * 1_000 + (stat.st_mtime_nsec as u64) / 1_000_000);
            entries.push(ConfinedEntry {
                name: name.to_string(),
                is_file: kind == libc::S_IFREG,
                is_dir: kind == libc::S_IFDIR,
                mtime_unix_ms,
                byte_len: u64::try_from(stat.st_size).unwrap_or_default(),
            });
        }
        let close_rc = unsafe { libc::closedir(directory) }; // owns duplicated fd
        if close_rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

/// Split an ABSOLUTE path into validated real components for the strict
/// no-symlink walks: rejects relative paths, `.`/`..`, and non-UTF8.
#[cfg(unix)]
fn absolute_real_components(path: &Path) -> std::io::Result<Vec<String>> {
    if !path.is_absolute() {
        return Err(std::io::Error::other(format!(
            "strict no-symlink resolution requires an absolute path, got {path:?}"
        )));
    }
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(os) => {
                let name = os.to_str().ok_or_else(|| {
                    std::io::Error::other(format!("non-UTF8 path component in {path:?}"))
                })?;
                let _ = validated_component_cstring(name)?;
                out.push(name.to_string());
            }
            other => {
                return Err(std::io::Error::other(format!(
                    "refusing non-normal path component {other:?} in {path:?}"
                )));
            }
        }
    }
    Ok(out)
}

/// Validate one path component: non-empty, no separators, not `.`/`..`,
/// no NUL. Returns it as a `CString` for the *at syscalls.
#[cfg(unix)]
fn validated_component_cstring(name: &str) -> std::io::Result<std::ffi::CString> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(std::io::Error::other(format!(
            "invalid confined path component: {name:?}"
        )));
    }
    std::ffi::CString::new(name).map_err(|_| std::io::Error::other("NUL in path component"))
}

/// Split and validate a RELATIVE multi-component path (`a/b/c.jsonl`).
/// Rejects absolute paths, `.`/`..`, empty components, and backslashes.
#[cfg(unix)]
pub fn validated_relative_components(rel: &str) -> std::io::Result<Vec<String>> {
    if rel.is_empty() || rel.starts_with('/') {
        return Err(std::io::Error::other(format!(
            "invalid confined relative path: {rel:?}"
        )));
    }
    let mut out = Vec::new();
    for component in rel.split('/') {
        validated_component_cstring(component).map_err(|_| {
            std::io::Error::other(format!("invalid confined relative path: {rel:?}"))
        })?;
        out.push(component.to_string());
    }
    Ok(out)
}

#[cfg(unix)]
fn confinement_error(component: &str, err: &std::io::Error) -> std::io::Error {
    if matches!(
        err.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR) | Some(libc::EEXIST) | Some(libc::EXDEV)
    ) {
        std::io::Error::other(format!(
            "confinement violation at {component:?}: refusing symlink or \
             non-directory in a confined destination ({err})"
        ))
    } else {
        std::io::Error::new(err.kind(), format!("{component:?}: {err}"))
    }
}
