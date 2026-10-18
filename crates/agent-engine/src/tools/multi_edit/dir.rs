//! Directory handles for `multi_edit`.
//!
//! On Unix, every file operation after planning goes relative to a handle on
//! the target's parent directory. The handle is opened by walking the
//! canonical parent path that was scope-checked from `/`, one component at
//! a time, without following any symlink, so it is the directory that path
//! named; the walk captures its identity (dev, ino). The scope check itself
//! only compares path strings (write scopes are defined by paths): the walk
//! and the re-walk before publishing are what tie the handle to the checked
//! path. Staging, the re-check, publishing, restoring and cleanup never
//! traverse the parent path again; before anything is published, the same
//! walk must still reach the same directory.
//!
//! Elsewhere the handle is just the path, as before.

use std::fs::File;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What `stat` says about one file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Meta {
    pub kind: Kind,
    pub size: u64,
    /// Permission bits, `st_mode & 0o7777` (Unix).
    pub mode: Option<u32>,
    pub nlink: u64,
    /// `(dev, ino)` (Unix).
    pub identity: Option<(u64, u64)>,
}

/// Why [`Dir::open_regular`] refused.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// A FIFO, device, socket or directory.
    NotRegular,
    /// The name is a symlink now.
    Symlink,
    Io(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotRegular => f.write_str("not a regular file"),
            OpenError::Symlink => f.write_str("it is a symlink now"),
            OpenError::Io(e) => e.fmt(f),
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::{Kind, Meta, OpenError};
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};

    // A directory is only ever used as the base of `*at` calls, so on Linux
    // it needs search permission only (O_PATH), not read permission.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const DIR_FLAGS: libc::c_int =
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const DIR_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
    }

    fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    /// `mode` is the variadic `unsigned int` argument (used with O_CREAT).
    fn openat(dir: RawFd, name: &OsStr, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
        let name = cstr(name)?;
        loop {
            // SAFETY: `name` is NUL-terminated; `dir` is an open descriptor
            // or AT_FDCWD.
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

    // The stat field types differ between platforms.
    #[allow(clippy::unnecessary_cast)]
    fn meta_of(st: &libc::stat) -> Meta {
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => Kind::File,
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        Meta {
            kind,
            size: st.st_size as u64,
            mode: Some((st.st_mode & 0o7777) as u32),
            nlink: st.st_nlink as u64,
            identity: Some((st.st_dev as u64, st.st_ino as u64)),
        }
    }

    fn fstat(fd: RawFd) -> io::Result<Meta> {
        let mut st = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `st` is a valid out-pointer for one stat struct.
        cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
        // SAFETY: fstat succeeded, so `st` is initialised.
        Ok(meta_of(unsafe { &st.assume_init() }))
    }

    /// A verified parent directory.
    pub(crate) struct Dir {
        fd: OwnedFd,
        path: PathBuf,
        identity: (u64, u64),
    }

    impl Dir {
        /// Opens the absolute, canonical `path` by walking it from `/`
        /// without following symlinks. A component that is not a directory
        /// (a symlink included) fails.
        pub(crate) fn open(path: &Path) -> io::Result<Dir> {
            if !path.has_root() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not an absolute path"));
            }
            let mut fd = openat(libc::AT_FDCWD, OsStr::new("/"), DIR_FLAGS, 0)?;
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        let next = openat(fd.as_raw_fd(), name, DIR_FLAGS, 0)?;
                        if fstat(next.as_raw_fd())?.kind != Kind::Dir {
                            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                        }
                        fd = next;
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "not a canonical path",
                        ))
                    }
                }
            }
            let (dev, ino) = fstat(fd.as_raw_fd())?.identity.unwrap_or_default();
            Ok(Dir {
                fd,
                path: path.to_path_buf(),
                identity: (dev, ino),
            })
        }

        /// Whether walking the same path again still reaches this directory.
        pub(crate) fn still_there(&self) -> bool {
            Dir::open(&self.path).is_ok_and(|now| now.identity == self.identity)
        }

        /// `fstatat(name, AT_SYMLINK_NOFOLLOW)`.
        pub(crate) fn lstat(&self, name: &OsStr) -> io::Result<Meta> {
            let name = cstr(name)?;
            let mut st = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `name` is NUL-terminated and `st` a valid out-pointer.
            cvt(unsafe {
                libc::fstatat(self.fd.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
            })?;
            // SAFETY: fstatat succeeded, so `st` is initialised.
            Ok(meta_of(unsafe { &st.assume_init() }))
        }

        /// Opens `name` for reading. The open never waits (a FIFO or device
        /// is never blocked on) and never follows a symlink; the type is
        /// checked on the descriptor itself, and only a regular file is
        /// returned, switched back to blocking reads.
        pub(crate) fn open_regular(&self, name: &OsStr) -> Result<(File, Meta), OpenError> {
            let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY;
            let fd = openat(self.fd.as_raw_fd(), name, flags, 0).map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    OpenError::Symlink
                } else {
                    OpenError::Io(e)
                }
            })?;
            let meta = fstat(fd.as_raw_fd()).map_err(OpenError::Io)?;
            if meta.kind != Kind::File {
                return Err(OpenError::NotRegular);
            }
            // SAFETY: plain fcntl calls on a descriptor we own.
            let flags = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) }).map_err(OpenError::Io)?;
            // SAFETY: as above.
            cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) })
                .map_err(OpenError::Io)?;
            Ok((File::from(fd), meta))
        }

        /// Creates `name` exclusively (never following or reusing anything
        /// already there) with `mode`, which the umask still narrows.
        pub(crate) fn create_new(&self, name: &OsStr, mode: u32) -> io::Result<File> {
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            openat(self.fd.as_raw_fd(), name, flags, mode).map(File::from)
        }

        /// `renameat(from, to)` within this directory; replaces `to`.
        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            let (from, to) = (cstr(from)?, cstr(to)?);
            let fd = self.fd.as_raw_fd();
            // SAFETY: both names are NUL-terminated; `fd` is open.
            cvt(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) }).map(drop)
        }

        /// Renames `from` to `to` only if `to` does not exist (Linux
        /// `renameat2(RENAME_NOREPLACE)`); `Unsupported` elsewhere.
        pub(crate) fn rename_noreplace(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            #[cfg(target_os = "linux")]
            {
                let (from, to) = (cstr(from)?, cstr(to)?);
                let fd = self.fd.as_raw_fd();
                // SAFETY: renameat2(int, const char *, int, const char *,
                // unsigned) with NUL-terminated names and an open descriptor.
                let r = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        fd,
                        from.as_ptr(),
                        fd,
                        to.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                };
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (from, to);
                Err(io::Error::from(io::ErrorKind::Unsupported))
            }
        }

        /// `linkat(from, to)` within this directory; never replaces `to`.
        pub(crate) fn link(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            let (from, to) = (cstr(from)?, cstr(to)?);
            let fd = self.fd.as_raw_fd();
            // SAFETY: both names are NUL-terminated; `fd` is open.
            cvt(unsafe { libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0) }).map(drop)
        }

        /// `unlinkat(name)` within this directory.
        pub(crate) fn unlink(&self, name: &OsStr) -> io::Result<()> {
            let name = cstr(name)?;
            // SAFETY: `name` is NUL-terminated; the descriptor is open.
            cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
        }
    }

    /// The permission bits and identity of an open file.
    pub(crate) fn meta_of_file(file: &File) -> io::Result<Meta> {
        fstat(file.as_raw_fd())
    }

    /// Whether a failed exclusive rename means "not supported here".
    pub(crate) fn rename_unsupported(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Unsupported
            || matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
    }

    /// Whether a failed hard link means "not supported here".
    pub(crate) fn link_unsupported(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Unsupported
            || matches!(e.raw_os_error(), Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP))
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Kind, Meta, OpenError};
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::{Path, PathBuf};

    fn meta_of(meta: &std::fs::Metadata) -> Meta {
        let t = meta.file_type();
        let kind = if t.is_symlink() {
            Kind::Symlink
        } else if t.is_file() {
            Kind::File
        } else if t.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        Meta {
            kind,
            size: meta.len(),
            mode: None,
            nlink: 1,
            identity: None,
        }
    }

    /// The parent directory as a path (no directory handles here).
    pub(crate) struct Dir {
        path: PathBuf,
    }

    impl Dir {
        pub(crate) fn open(path: &Path) -> io::Result<Dir> {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::Other, "not a directory"));
            }
            Ok(Dir {
                path: path.to_path_buf(),
            })
        }

        pub(crate) fn still_there(&self) -> bool {
            true
        }

        pub(crate) fn lstat(&self, name: &OsStr) -> io::Result<Meta> {
            std::fs::symlink_metadata(self.path.join(name)).map(|m| meta_of(&m))
        }

        pub(crate) fn open_regular(&self, name: &OsStr) -> Result<(File, Meta), OpenError> {
            let file = File::open(self.path.join(name)).map_err(OpenError::Io)?;
            let meta = meta_of(&file.metadata().map_err(OpenError::Io)?);
            if meta.kind != Kind::File {
                return Err(OpenError::NotRegular);
            }
            Ok((file, meta))
        }

        pub(crate) fn create_new(&self, name: &OsStr, _mode: u32) -> io::Result<File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(name))
        }

        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            std::fs::rename(self.path.join(from), self.path.join(to))
        }

        pub(crate) fn rename_noreplace(&self, _from: &OsStr, _to: &OsStr) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }

        pub(crate) fn link(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            std::fs::hard_link(self.path.join(from), self.path.join(to))
        }

        pub(crate) fn unlink(&self, name: &OsStr) -> io::Result<()> {
            std::fs::remove_file(self.path.join(name))
        }
    }

    pub(crate) fn meta_of_file(file: &File) -> io::Result<Meta> {
        file.metadata().map(|m| meta_of(&m))
    }

    pub(crate) fn rename_unsupported(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Unsupported
    }

    pub(crate) fn link_unsupported(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::Unsupported
    }
}

pub(crate) use imp::{link_unsupported, meta_of_file, rename_unsupported, Dir};

/// Reads at most `cap` bytes of `file`.
pub(crate) fn read_capped(file: File, cap: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        (d, root)
    }

    #[test]
    fn open_never_follows_a_symlinked_component() {
        let (_d, root) = canonical_tmp();
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(Dir::open(&root.join("real/inner")).is_ok());
        assert!(Dir::open(&root.join("link")).is_err());
        assert!(Dir::open(&root.join("link/inner")).is_err());
        assert!(Dir::open(Path::new("relative")).is_err());
    }

    #[test]
    fn still_there_notices_a_replaced_directory() {
        let (_d, root) = canonical_tmp();
        let path = root.join("d");
        std::fs::create_dir(&path).unwrap();
        let dir = Dir::open(&path).unwrap();
        assert!(dir.still_there());
        std::fs::rename(&path, root.join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!dir.still_there());
    }

    #[test]
    fn open_regular_takes_only_a_regular_file() {
        let (_d, root) = canonical_tmp();
        std::fs::write(root.join("f"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("f"), root.join("l")).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let dir = Dir::open(&root).unwrap();
        assert!(dir.open_regular(OsStr::new("f")).is_ok());
        assert!(matches!(dir.open_regular(OsStr::new("l")), Err(OpenError::Symlink)));
        assert!(matches!(dir.open_regular(OsStr::new("sub")), Err(OpenError::NotRegular)));
    }
}
