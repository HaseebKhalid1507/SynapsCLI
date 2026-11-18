//! In-process write checks on opened file objects.
//!
//! The write, edit and multi_edit tools run inside the engine process, which
//! is not under Landlock. When a session is confined they open their target
//! through [`open_target`] instead: the parent directory is resolved
//! (symlinks followed once, up front), checked against the session's policy,
//! and then *opened component by component with `O_NOFOLLOW`* from `/`, so
//! the directory the tool writes into is exactly the one the check saw, with
//! no window to swap a component for a symlink. Reads and the atomic replace
//! go through that directory's fd (`openat`, `renameat`); the final path
//! component is never followed.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use super::confinement::Confinement;
use super::policy::{canon_lossy, lexical_normalize};

/// Symlinks followed on the target's last component before giving up.
const MAX_LINK_HOPS: usize = 8;

/// Why a guarded open or write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// The policy refuses this path; the text says why.
    Refused(String),
    Io(io::Error),
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Refused(why) => f.write_str(why),
            WriteError::Io(e) => write!(f, "{e}"),
        }
    }
}

fn cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn openat_dir(parent: libc::c_int, name: &OsStr) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    // SAFETY: `c` is a valid C string; the fd it returns is owned below.
    let fd = cvt(unsafe {
        libc::openat(parent, c.as_ptr(), libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY)
    })?;
    // SAFETY: openat returned a fresh fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_root() -> io::Result<OwnedFd> {
    openat_dir(libc::AT_FDCWD, OsStr::new("/"))
}

fn mkdirat(parent: libc::c_int, name: &OsStr) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: valid C string and fd.
    cvt(unsafe { libc::mkdirat(parent, c.as_ptr(), 0o777) }).map(|_| ())
}

/// A write target whose parent directory has been checked and opened.
#[derive(Debug)]
pub struct GuardedTarget {
    parent: OwnedFd,
    tail: OsString,
    real: PathBuf,
}

impl GuardedTarget {
    /// The checked path: the real parent directory plus the final name.
    pub fn real_path(&self) -> &Path {
        &self.real
    }

    fn stat(&self) -> io::Result<libc::stat> {
        let c = cstr(&self.tail)?;
        // SAFETY: zeroed stat is a valid out-parameter.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd, C string and out pointer.
        cvt(unsafe { libc::fstatat(self.parent.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(st)
    }

    /// Whether the final name exists (a symlink counts).
    pub fn exists(&self) -> bool {
        self.stat().is_ok()
    }

    /// The permission bits of an existing regular file, `None` when there is
    /// none (or it is not a regular file).
    pub fn mode(&self) -> Option<u32> {
        let st = self.stat().ok()?;
        ((st.st_mode & libc::S_IFMT) == libc::S_IFREG).then_some(st.st_mode & 0o7777)
    }

    /// Read the file (never following a symlink on the final name).
    pub fn read_to_string(&self) -> io::Result<String> {
        use std::io::Read;
        let c = cstr(&self.tail)?;
        // SAFETY: valid fd and C string.
        let fd = cvt(unsafe {
            libc::openat(self.parent.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        })?;
        // SAFETY: fresh fd, owned by the File.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }

    /// Write `bytes` to a new sibling `tmp_name` (`O_EXCL|O_NOFOLLOW`), apply
    /// `mode` when given, and rename it over the target. The old file, if
    /// any, is left alone when anything fails. Replacing swaps the directory
    /// entry itself, so a symlink at the target is replaced, not followed.
    pub fn replace(&self, tmp_name: &str, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
        use std::io::Write;
        if tmp_name.is_empty() || tmp_name.contains('/') || tmp_name.contains('\0') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad temporary file name"));
        }
        let tmp = cstr(OsStr::new(tmp_name))?;
        let create_mode = mode.unwrap_or(0o666) & 0o7777;
        // SAFETY: valid fd and C string.
        let fd = cvt(unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                create_mode as libc::c_uint,
            )
        })?;
        // SAFETY: fresh fd, owned by the File.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let finish = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            if let Some(mode) = mode {
                // SAFETY: valid fd.
                cvt(unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })?;
            }
            file.sync_all()?;
            let to = cstr(&self.tail)?;
            // SAFETY: valid fds and C strings.
            cvt(unsafe {
                libc::renameat(self.parent.as_raw_fd(), tmp.as_ptr(), self.parent.as_raw_fd(), to.as_ptr())
            })?;
            Ok(())
        })();
        if finish.is_err() {
            // SAFETY: valid fd and C string; best effort.
            unsafe { libc::unlinkat(self.parent.as_raw_fd(), tmp.as_ptr(), 0) };
        }
        finish
    }

    /// Remove the final name (a file or symlink), when present.
    pub fn remove(&self) -> io::Result<()> {
        let c = cstr(&self.tail)?;
        // SAFETY: valid fd and C string.
        cvt(unsafe { libc::unlinkat(self.parent.as_raw_fd(), c.as_ptr(), 0) }).map(|_| ())
    }
}

/// Resolve and open `path`'s parent for writing under `confinement`.
///
/// The parent is canonicalised as far as it exists; `create_parents` makes
/// the missing directories (each one checked first). A symlink on the final
/// name is followed (up to [`MAX_LINK_HOPS`]) so the policy is applied to
/// where the write would really land.
pub fn open_target(confinement: &Confinement, path: &Path, create_parents: bool) -> Result<GuardedTarget, WriteError> {
    open_target_with(confinement, path, create_parents, &mut || {})
}

/// [`open_target`], calling `between` after the policy check and before the
/// directory is opened: the window a racing process would use to swap a
/// component for a symlink. Production passes nothing; the tests swap there.
fn open_target_with(
    confinement: &Confinement,
    path: &Path,
    create_parents: bool,
    between: &mut dyn FnMut(),
) -> Result<GuardedTarget, WriteError> {
    let mut path = lexical_normalize(path)
        .ok_or_else(|| WriteError::Refused(format!("sandbox: write refused: {} is not an absolute path", path.display())))?;
    for _ in 0..=MAX_LINK_HOPS {
        let target = open_once(confinement, &path, create_parents, between)?;
        // Follow a symlink on the final name, so the check sees its target.
        let is_link = target
            .stat()
            .map(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFLNK)
            .unwrap_or(false);
        if !is_link {
            return Ok(target);
        }
        let c = cstr(&target.tail)?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: valid fd, C string and buffer.
        let n = cvt(unsafe {
            libc::readlinkat(target.parent.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) as libc::c_int
        })?;
        buf.truncate(n as usize);
        let link = PathBuf::from(OsString::from_vec(buf));
        let joined = if link.is_absolute() {
            link
        } else {
            target.real.parent().unwrap_or(Path::new("/")).join(link)
        };
        path = lexical_normalize(&joined).ok_or_else(|| WriteError::Refused("sandbox: write refused: bad link".to_string()))?;
    }
    Err(WriteError::Refused("sandbox: write refused: too many symbolic links".to_string()))
}

fn open_once(
    confinement: &Confinement,
    path: &Path,
    create_parents: bool,
    between: &mut dyn FnMut(),
) -> Result<GuardedTarget, WriteError> {
    let tail = match path.components().next_back() {
        Some(Component::Normal(name)) => name.to_os_string(),
        _ => {
            return Err(WriteError::Refused(format!(
                "sandbox: write refused: {} has no file name",
                path.display()
            )))
        }
    };
    let parent_lexical = path.parent().unwrap_or(Path::new("/"));
    // The canonical parent as far as it exists, plus what does not yet exist.
    let canon = canon_lossy(parent_lexical);
    let real = canon.join(&tail);
    confinement.allows_write(&real).map_err(WriteError::Refused)?;
    between();

    // Walk from `/` with O_NOFOLLOW; any component that is now a symlink
    // fails the open instead of redirecting the write.
    let mut dir = open_root()?;
    let mut missing = false;
    for component in canon.components() {
        let Component::Normal(name) = component else { continue };
        if missing {
            if !create_parents {
                return Err(io::Error::from(io::ErrorKind::NotFound).into());
            }
            mkdirat(dir.as_raw_fd(), name)?;
            dir = openat_dir(dir.as_raw_fd(), name)?;
            continue;
        }
        match openat_dir(dir.as_raw_fd(), name) {
            Ok(next) => dir = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !create_parents {
                    return Err(e.into());
                }
                missing = true;
                mkdirat(dir.as_raw_fd(), name)?;
                dir = openat_dir(dir.as_raw_fd(), name)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    // The directory the kernel now has open must be the one the check saw:
    // a component renamed since the canonicalisation would show up here.
    let seen = std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd()))
        .map_err(|e| WriteError::Refused(format!("sandbox: write refused: cannot verify the target's directory ({e})")))?;
    if seen != canon {
        return Err(WriteError::Refused(
            "sandbox: write refused: the target's directory changed while it was being opened".to_string(),
        ));
    }
    Ok(GuardedTarget { parent: dir, tail, real })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::testkit::Tree;

    fn refused<T: std::fmt::Debug>(r: Result<T, WriteError>) -> String {
        match r {
            Err(WriteError::Refused(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn write(c: &Confinement, path: &Path, text: &str) -> Result<(), WriteError> {
        let target = open_target(c, path, true)?;
        target.replace(".guard-test.tmp", text.as_bytes(), None)?;
        Ok(())
    }

    #[test]
    fn a_write_inside_the_workspace_lands_and_one_outside_never_does() {
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        write(&c, &t.ws.join("src/a.txt"), "hi").unwrap();
        assert_eq!(std::fs::read_to_string(t.ws.join("src/a.txt")).unwrap(), "hi");
        assert!(!t.ws.join("src/.guard-test.tmp").exists(), "the temp name is renamed over the target");

        for outside in [t.home.join("outside.txt"), t.ws.join("src/../../outside.txt"), t.root.join("elsewhere/x")] {
            refused(write(&c, &outside, "no"));
        }
        assert!(!t.home.join("outside.txt").exists());
        assert!(!t.root.join("elsewhere").exists(), "a refused write makes no directory either");
    }

    #[test]
    fn a_protected_path_inside_the_granted_workspace_is_refused() {
        let t = Tree::new();
        std::fs::create_dir_all(t.ws.join("bin")).unwrap();
        let c = t.confinement(vec![t.ws.join("bin")], vec![]);
        for (what, path) in [
            ("project plugin folder", t.ws.join(".synaps/plugins/x/run.sh")),
            ("a $PATH entry inside the workspace", t.ws.join("bin/tool")),
            ("the engine base dir", t.base.join("config")),
            ("the engine's sandbox dir", t.base.join("sandbox/policy")),
            ("a credential store", t.home.join(".ssh/authorized_keys")),
        ] {
            let why = refused(write(&c, &path, "x"));
            assert!(why.starts_with("sandbox: "), "{what}: {why}");
            assert!(!path.exists(), "{what}: nothing was written");
        }
        assert!(!t.ws.join(".synaps").exists(), "no directory was made on the way to a protected path");
        assert!(!t.home.join(".ssh").exists());
    }

    #[test]
    fn a_symlink_on_the_final_name_is_judged_by_where_it_points() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        std::fs::write(t.home.join("victim.txt"), "keep").unwrap();
        symlink(t.home.join("victim.txt"), t.ws.join("src/out")).unwrap();
        refused(write(&c, &t.ws.join("src/out"), "no"));
        assert_eq!(std::fs::read_to_string(t.home.join("victim.txt")).unwrap(), "keep");

        std::fs::create_dir_all(t.ws.join(".synaps/plugins/x")).unwrap();
        symlink(t.ws.join(".synaps/plugins/x"), t.ws.join("src/plug")).unwrap();
        refused(write(&c, &t.ws.join("src/plug/run.sh"), "no"));
        assert!(!t.ws.join(".synaps/plugins/x/run.sh").exists());

        // A link that stays inside the grant is fine: the write lands on its target.
        std::fs::write(t.ws.join("src/real.txt"), "old").unwrap();
        symlink(t.ws.join("src/real.txt"), t.ws.join("src/alias")).unwrap();
        write(&c, &t.ws.join("src/alias"), "new").unwrap();
        assert_eq!(std::fs::read_to_string(t.ws.join("src/real.txt")).unwrap(), "new");
    }

    #[test]
    fn a_directory_swapped_for_a_link_after_the_check_is_not_followed() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        let outside = t.root.join("outside-dir");
        std::fs::create_dir_all(&outside).unwrap();
        for (what, elsewhere) in [("an unprotected dir", outside.clone()), ("the engine base dir", t.base.clone())] {
            let src = t.ws.join("src");
            let parked = t.ws.join("src.parked");
            let mut swapped = false;
            let result = open_target_with(&c, &src.join("swap.txt"), false, &mut || {
                // The check has passed; now a racing process swaps the directory.
                std::fs::rename(&src, &parked).unwrap();
                symlink(&elsewhere, &src).unwrap();
                swapped = true;
            });
            assert!(swapped, "{what}: the swap ran between the check and the open");
            // Refused by the open itself (`O_NOFOLLOW`), not only by the check
            // after the walk that compares the opened directory with the path.
            assert!(
                matches!(result, Err(WriteError::Io(_))),
                "{what}: the open must not follow the swapped link: {result:?}"
            );
            assert!(!elsewhere.join("swap.txt").exists(), "{what}: nothing landed through the link");
            std::fs::remove_file(&src).unwrap();
            std::fs::rename(&parked, &src).unwrap();
        }
    }

    #[test]
    fn a_temp_name_that_is_a_link_or_exists_is_never_written_through() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        let victim = t.home.join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        let target = open_target(&c, &t.ws.join("src/out.txt"), false).unwrap();
        // A link planted at the temporary name is not followed.
        let tmp = t.ws.join("src/.planted.tmp");
        symlink(&victim, &tmp).unwrap();
        assert!(target.replace(".planted.tmp", b"no", None).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!t.ws.join("src/out.txt").exists());
        std::fs::remove_file(&tmp).unwrap();
        // A file already there is not reused either.
        std::fs::write(&tmp, "mine").unwrap();
        assert!(target.replace(".planted.tmp", b"no", None).is_err());
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "mine");
        std::fs::remove_file(&tmp).unwrap();
        // The name is one plain component.
        for bad in ["", "a/b", "../x", "a\0b"] {
            let err = target.replace(bad, b"no", None).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
        }
        assert!(!t.ws.join("src/out.txt").exists());
        // Control: a plain name works.
        target.replace(".planted.tmp", b"yes", None).unwrap();
        assert_eq!(std::fs::read_to_string(t.ws.join("src/out.txt")).unwrap(), "yes");
    }

    #[test]
    fn a_guarded_read_never_follows_the_final_name() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        let file = t.ws.join("src/read.txt");
        std::fs::write(&file, "mine").unwrap();
        let target = open_target(&c, &file, false).unwrap();
        assert_eq!(target.read_to_string().unwrap(), "mine", "control: a regular file reads");
        // The name is swapped for a link after the target was opened.
        let secret = t.home.join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        std::fs::remove_file(&file).unwrap();
        symlink(&secret, &file).unwrap();
        let read = target.read_to_string();
        assert!(read.is_err(), "the link was followed: {read:?}");
        // Nor is anything but a regular file read.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(target.read_to_string().is_err());
    }

    #[test]
    fn a_chain_of_links_is_followed_up_to_the_bound_and_no_further() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        let real = t.ws.join("src/real.txt");
        std::fs::write(&real, "old").unwrap();
        let chain = |n: usize| -> PathBuf {
            let mut prev = real.clone();
            for i in 0..n {
                let link = t.ws.join(format!("src/chain-{n}-{i}"));
                symlink(&prev, &link).unwrap();
                prev = link;
            }
            prev
        };
        write(&c, &chain(MAX_LINK_HOPS), "new").unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new", "{MAX_LINK_HOPS} links are followed");
        let why = refused(write(&c, &chain(MAX_LINK_HOPS + 1), "no"));
        assert!(why.contains("too many symbolic links"), "{why}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new", "one link too many writes nothing");
    }

    #[test]
    fn parents_are_made_only_when_the_whole_path_is_allowed() {
        let t = Tree::new();
        let c = t.confinement(vec![], vec![]);
        write(&c, &t.ws.join("src/new/deep/f.txt"), "ok").unwrap();
        assert_eq!(std::fs::read_to_string(t.ws.join("src/new/deep/f.txt")).unwrap(), "ok");
        refused(write(&c, &t.home.join("newdir/f.txt"), "no"));
        assert!(!t.home.join("newdir").exists());
        let missing = open_target(&c, &t.ws.join("src/absent/f.txt"), false);
        assert!(matches!(missing, Err(WriteError::Io(_))), "{missing:?}");
        assert!(!t.ws.join("src/absent").exists(), "without create_parents nothing is made");
    }
}
