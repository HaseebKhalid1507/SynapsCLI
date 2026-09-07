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
        let proc_dir = PathBuf::from(format!("/proc/{}/fd", std::process::id()));
        let proc_c =
            CString::new(proc_dir.as_os_str().as_bytes()).map_err(|_| Error::UnsafePath)?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(proc_c.as_ptr(), &mut stat) } != 0
            || stat.f_type != libc::PROC_SUPER_MAGIC
        {
            return Err(Error::UnsafePath);
        }
        let anchor = proc_dir.join(directory.as_raw_fd().to_string());
        let anchored_meta = fs::metadata(&anchor)?;
        if anchored_meta.dev() != meta.dev() || anchored_meta.ino() != meta.ino() {
            return Err(Error::UnsafePath);
        }
        let this = Self {
            path: anchor.join(path.file_name().ok_or(Error::UnsafePath)?),
            directory,
        };
        install_anchored_vfs()?;
        this.check_files()?;
        Ok(this)
    }
    pub fn check_files(&self) -> Result<()> {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = self.path.as_os_str().to_os_string();
            name.push(suffix);
            match fs::symlink_metadata(Path::new(&name)) {
                Ok(m)
                    if m.is_file()
                        && m.nlink() == 1
                        && m.uid() == unsafe { libc::geteuid() }
                        && m.mode() & 0o077 == 0 => {}
                Ok(_) => return Err(Error::UnsafePath),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err(Error::UnsafePath),
            }
        }
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        self.check_files()?;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)?
            .sync_all()?;
        self.directory.sync_all()?;
        Ok(())
    }
}

// SQLite's default xFullPathname resolves symlinks, including proc fd handles,
// undoing descriptor anchoring. Preserve ONLY our process's proc-fd paths.
unsafe extern "C" fn anchored_full_path(
    _: *mut rusqlite::ffi::sqlite3_vfs,
    input: *const libc::c_char,
    size: libc::c_int,
    output: *mut libc::c_char,
) -> libc::c_int {
    let bytes = unsafe { std::ffi::CStr::from_ptr(input) }.to_bytes();
    let prefix = format!("/proc/{}/fd/", std::process::id());
    let valid = bytes.strip_prefix(prefix.as_bytes()).is_some_and(|rest| {
        let mut parts = rest.split(|b| *b == b'/');
        let fd = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        !fd.is_empty()
            && fd.iter().all(u8::is_ascii_digit)
            && !name.is_empty()
            && name != b"."
            && name != b".."
            && parts.next().is_none()
    });
    if !valid || size <= 0 || bytes.len() >= size as usize {
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
        let original = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
        if original.is_null() {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_anchor_survives_ancestor_replacement() {
        let root = tempfile::tempdir().unwrap();
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
}
