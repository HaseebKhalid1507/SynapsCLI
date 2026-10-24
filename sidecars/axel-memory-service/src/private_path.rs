//! Unix private-path boundary. No environment defaults and no symlink traversal.
use crate::contract::{Error, Result};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub struct PrivatePath {
    pub path: PathBuf,
    directory: File,
}
impl PrivatePath {
    pub fn acquire(path: &Path) -> Result<Self> {
        if !path.is_absolute() || path.extension().and_then(|s| s.to_str()) != Some("r8") {
            return Err(Error::UnsafePath);
        }
        let parent = path.parent().ok_or(Error::UnsafePath)?;
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")?;
        for component in parent.components() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) => {
                    let name = CString::new(name.as_bytes()).map_err(|_| Error::UnsafePath)?;
                    // SAFETY: owned descriptor and NUL-terminated component; no symlinks followed.
                    let fd = unsafe {
                        libc::openat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0 {
                        return Err(Error::UnsafePath);
                    }
                    // Sync each parent even on reopen/retry. A previously failed
                    // fsync must not be mistaken for durable directory ancestry.
                    directory.sync_all()?;
                    directory = unsafe { File::from_raw_fd(fd) };
                }
                _ => return Err(Error::UnsafePath),
            }
        }
        let meta = directory.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(Error::UnsafePath);
        }
        // Serialize before SQLite open/schema creation. Wait only for an
        // uncommitted lock acquisition, never retry a database operation. Keep
        // this below the host's 15-second process deadline. The CLOEXEC-owned
        // descriptor closes on cancellation; no lock files or helper children.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK | libc::EINTR))
                || std::time::Instant::now() >= deadline
            {
                return Err(Error::Storage);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let anchor = anchor_directory(&directory, &meta)?;
        let anchored_meta = fs::metadata(&anchor)?;
        if anchored_meta.dev() != meta.dev() || anchored_meta.ino() != meta.ino() {
            return Err(Error::UnsafePath);
        }
        let this = Self {
            path: anchor.join(path.file_name().ok_or(Error::UnsafePath)?),
            directory,
        };
        #[cfg(target_os = "macos")]
        volfs::hold(&meta, this.directory.as_raw_fd());
        install_anchored_vfs()?;
        this.check_files()?;
        Ok(this)
    }
    /// `<brain><suffix>` inside the held directory.
    fn name(&self, suffix: &str) -> std::io::Result<CString> {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        CString::new(name.as_bytes()).map_err(|_| std::io::ErrorKind::InvalidInput.into())
    }
    /// `fstatat` relative to the held directory; never follows a symlink.
    fn lstat(&self, suffix: &str) -> std::io::Result<libc::stat> {
        let name = self.name(suffix)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: held directory descriptor, NUL-terminated basename, owned buffer.
        let rc = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stat)
    }
    /// Read-only `openat` relative to the held directory; never follows a symlink.
    pub fn open(&self, suffix: &str) -> std::io::Result<File> {
        let name = self.name(suffix)?;
        // SAFETY: held directory descriptor and NUL-terminated basename.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn try_exists(&self) -> Result<bool> {
        match self.lstat("") {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn check_files(&self) -> Result<()> {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            match self.lstat(suffix) {
                Ok(m)
                    if m.st_mode & libc::S_IFMT == libc::S_IFREG
                        && m.st_nlink == 1
                        && m.st_uid == unsafe { libc::geteuid() }
                        && m.st_mode & 0o077 == 0 => {}
                Ok(_) => return Err(Error::UnsafePath),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err(Error::UnsafePath),
            }
        }
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        self.check_files()?;
        self.open("")?.sync_all()?;
        self.directory.sync_all()?;
        Ok(())
    }
}
#[cfg(target_os = "macos")]
impl Drop for PrivatePath {
    fn drop(&mut self) {
        volfs::release(self.directory.as_raw_fd());
    }
}

/// Kernel pathname for the held directory that does not traverse its
/// replaceable ancestors. Linux: the descriptor's procfs magic link.
#[cfg(target_os = "linux")]
fn anchor_directory(directory: &File, _: &fs::Metadata) -> Result<PathBuf> {
    let proc_dir = PathBuf::from(format!("/proc/{}/fd", std::process::id()));
    let proc_c = CString::new(proc_dir.as_os_str().as_bytes()).map_err(|_| Error::UnsafePath)?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(proc_c.as_ptr(), &mut stat) } != 0
        || stat.f_type != libc::PROC_SUPER_MAGIC
    {
        return Err(Error::UnsafePath);
    }
    Ok(proc_dir.join(directory.as_raw_fd().to_string()))
}
/// macOS: the volfs file-id name `/.vol/<fsid>/<file id>`. See [`volfs`].
#[cfg(target_os = "macos")]
fn anchor_directory(_: &File, meta: &fs::Metadata) -> Result<PathBuf> {
    Ok(PathBuf::from(volfs::anchor(meta)))
}

// SQLite's default xFullPathname resolves symlinks, including proc fd handles,
// undoing descriptor anchoring. Preserve ONLY anchored paths.
#[cfg(target_os = "linux")]
fn anchored(bytes: &[u8]) -> bool {
    let prefix = format!("/proc/{}/fd/", std::process::id());
    bytes.strip_prefix(prefix.as_bytes()).is_some_and(|rest| {
        let mut parts = rest.split(|b| *b == b'/');
        let fd = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        !fd.is_empty()
            && fd.iter().all(u8::is_ascii_digit)
            && !name.is_empty()
            && name != b"."
            && name != b".."
            && parts.next().is_none()
    })
}
#[cfg(target_os = "macos")]
fn anchored(bytes: &[u8]) -> bool {
    matches!(volfs::parse(bytes), Some((_, _, Some(_))))
}
unsafe extern "C" fn anchored_full_path(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    input: *const libc::c_char,
    size: libc::c_int,
    output: *mut libc::c_char,
) -> libc::c_int {
    let bytes = unsafe { std::ffi::CStr::from_ptr(input) }.to_bytes();
    if !anchored(bytes) || size <= 0 || bytes.len() >= size as usize {
        return rusqlite::ffi::SQLITE_CANTOPEN;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(input, output, bytes.len() + 1);
    }
    rusqlite::ffi::SQLITE_OK
}
fn install_anchored_vfs() -> Result<()> {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ok = INSTALLED.get_or_init(|| unsafe {
        // macOS: "unix-posix" is the stock Unix VFS with POSIX locks. The
        // default Apple "unix" VFS would first statfs() the database path
        // directly (outside the syscall table) to pick a locking style.
        #[cfg(target_os = "linux")]
        let original = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
        #[cfg(target_os = "macos")]
        let original = rusqlite::ffi::sqlite3_vfs_find(c"unix-posix".as_ptr());
        if original.is_null() {
            return false;
        }
        #[cfg(target_os = "macos")]
        if !volfs::install(original) {
            return false;
        }
        let mut vfs = Box::new(std::ptr::read(original));
        vfs.pNext = std::ptr::null_mut();
        vfs.zName = c"synaps-anchored".as_ptr();
        vfs.xFullPathname = Some(anchored_full_path);
        // SQLite retains this pointer for the process lifetime. All remaining
        // callbacks/pAppData belong to its static built-in Unix VFS.
        rusqlite::ffi::sqlite3_vfs_register(Box::into_raw(vfs), 1) == rusqlite::ffi::SQLITE_OK
    });
    if *ok {
        Ok(())
    } else {
        Err(Error::UnsafePath)
    }
}

#[cfg(target_os = "macos")]
mod volfs;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_anchor_survives_ancestor_replacement() {
        // Symlink-free base: macOS temp dirs live under the /var symlink.
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = root.path().join("private");
        fs::create_dir(&parent).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let held = PrivatePath::acquire(&parent.join("notes.r8")).unwrap();
        let moved = root.path().join("moved");
        fs::rename(&parent, &moved).unwrap();
        fs::create_dir(&parent).unwrap();
        let c = rusqlite::Connection::open(&held.path).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE synthetic(x);")
            .unwrap();
        assert!(moved.join("notes.r8").exists());
        assert!(moved.join("notes.r8-wal").exists());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        let actual: String = c
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual, held.path.to_str().unwrap());
    }

    #[cfg(target_os = "macos")]
    fn private_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    /// A volfs name is re-resolved through the directory's current ancestry;
    /// the held descriptor is not. With an unsearchable ancestor only
    /// descriptor-relative SQLite I/O and boundary checks still reach the brain.
    #[cfg(target_os = "macos")]
    #[test]
    fn sqlite_io_does_not_traverse_ancestors() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let gate = root.path().join("gate");
        let parent = gate.join("private");
        private_dir(&parent);
        // Private mode without the service's umask; SQLite copies it to WAL/SHM.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(parent.join("notes.r8"))
            .unwrap();
        let held = PrivatePath::acquire(&parent.join("notes.r8")).unwrap();
        assert!(held.path.starts_with("/.vol/"));
        struct Reopen<'a>(&'a Path); // restore traversal even if an assertion fails
        impl Drop for Reopen<'_> {
            fn drop(&mut self) {
                let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o700));
            }
        }
        let reopen = Reopen(&gate);
        fs::set_permissions(&gate, fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = fs::metadata(held.path.parent().unwrap()).is_err();
        let result = (|| {
            let c = rusqlite::Connection::open(&held.path)?;
            c.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE synthetic(x); INSERT INTO synthetic VALUES(1);",
            )?;
            let rows = c.query_row("SELECT count(*) FROM synthetic", [], |r| r.get::<_, i64>(0))?;
            held.check_files().unwrap();
            held.sync().unwrap();
            assert!(held.try_exists().unwrap());
            Ok::<_, rusqlite::Error>(rows)
        })();
        drop(reopen);
        // Root bypasses search permission; the routed open must work either way.
        assert!(blocked || unsafe { libc::geteuid() } == 0);
        assert_eq!(result.unwrap(), 1);
        assert!(parent.join("notes.r8").exists());
    }
    /// Only held anchors are routed. A well-formed volfs path for any other
    /// directory, or for a released anchor, never reaches the kernel.
    #[cfg(target_os = "macos")]
    #[test]
    fn sqlite_refuses_unheld_volfs_paths() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let (mine, other) = (root.path().join("mine"), root.path().join("other"));
        private_dir(&mine);
        private_dir(&other);
        let held = PrivatePath::acquire(&mine.join("notes.r8")).unwrap();
        let unheld = PathBuf::from(volfs::anchor(&fs::metadata(&other).unwrap())).join("notes.r8");
        assert!(fs::metadata(unheld.parent().unwrap()).unwrap().is_dir()); // kernel volfs resolves it
        assert!(rusqlite::Connection::open(&unheld).is_err());
        assert!(!other.join("notes.r8").exists());
        let released = held.path.clone();
        rusqlite::Connection::open(&released).unwrap();
        drop(held);
        fs::remove_file(mine.join("notes.r8")).unwrap();
        assert!(rusqlite::Connection::open(&released).is_err());
        assert!(!mine.join("notes.r8").exists());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn volfs_paths_are_canonical() {
        assert_eq!(volfs::parse(b"/.vol/1/2"), Some((1, 2, None)));
        assert_eq!(
            volfs::parse(b"/.vol/1/2/a.r8-wal"),
            Some((1, 2, Some(&b"a.r8-wal"[..])))
        );
        for bad in [
            &b"/.vol/1"[..],
            b"/.vol/01/2/a",
            b"/.vol/1/2/",
            b"/.vol/1/2/.",
            b"/.vol/1/2/..",
            b"/.vol/1/2/a/b",
            b"/.vol/x/2/a",
            b"/.vol/4294967296/2/a",
            b"/vol/1/2/a",
        ] {
            assert_eq!(volfs::parse(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }
}
