//! macOS descriptor anchoring. There is no procfs; the anchored pathname is the
//! volfs name `/.vol/<fsid>/<file id>/<basename>`, so Axel's own existence
//! probe on it works unmodified. XNU re-resolves a volfs name through the
//! directory's *current* ancestry on every lookup, which is not descriptor
//! anchoring, so SQLite never performs that lookup: every path-taking entry of
//! SQLite's Unix syscall table is routed to the matching `*at()` call on a held,
//! verified directory descriptor. Unheld or malformed `/.vol` paths fail closed
//! with EACCES; all other paths (temp files, /dev/urandom) pass through.
use rusqlite::ffi::{sqlite3_syscall_ptr, sqlite3_vfs, SQLITE_OK};
use std::ffi::{c_char, c_int, CStr, CString};
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// (fsid, file id, held directory descriptor) of live `PrivatePath`s.
static HELD: Mutex<Vec<(u32, u64, RawFd)>> = Mutex::new(Vec::new());
fn held() -> MutexGuard<'static, Vec<(u32, u64, RawFd)>> {
    HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
// dev_t is i32 on macOS; `as u32` undoes MetadataExt's sign extension.
pub(super) fn anchor(meta: &std::fs::Metadata) -> String {
    format!("/.vol/{}/{}", meta.dev() as u32, meta.ino())
}
pub(super) fn hold(meta: &std::fs::Metadata, fd: RawFd) {
    held().push((meta.dev() as u32, meta.ino(), fd));
}
/// Called before the descriptor closes; waits for any in-flight routed call.
pub(super) fn release(fd: RawFd) {
    held().retain(|entry| entry.2 != fd);
}
/// Canonical `/.vol/<fsid>/<file id>[/<basename>]`.
pub(super) fn parse(bytes: &[u8]) -> Option<(u32, u64, Option<&[u8]>)> {
    fn number<T: std::str::FromStr>(digits: &[u8]) -> Option<T> {
        let canonical = matches!(digits, [b'0'] | [b'1'..=b'9', ..]);
        if !canonical || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    }
    let mut parts = bytes.strip_prefix(b"/.vol/")?.split(|b| *b == b'/');
    let dev = number(parts.next()?)?;
    let ino = number(parts.next()?)?;
    let name = parts.next();
    if parts.next().is_some() || matches!(name, Some(b"" | b"." | b"..")) {
        return None;
    }
    Some((dev, ino, name))
}
/// Run `at(dirfd, basename)` for a held volfs path, `native()` for any
/// non-volfs path. The registry lock spans the call so the descriptor
/// cannot be released (and its number reused) underneath it.
unsafe fn route<T: From<i8>>(
    path: *const c_char,
    at: impl FnOnce(RawFd, *const c_char) -> T,
    native: impl FnOnce() -> T,
) -> T {
    if path.is_null() {
        return native();
    }
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    if !bytes.starts_with(b"/.vol/") {
        return native();
    }
    let held = held();
    if let Some((dev, ino, name)) = parse(bytes) {
        if let Some(&(_, _, fd)) = held.iter().find(|h| h.0 == dev && h.1 == ino) {
            if let Ok(name) = CString::new(name.unwrap_or(b".")) {
                return at(fd, name.as_ptr());
            }
        }
    }
    drop(held);
    unsafe { *libc::__error() = libc::EACCES };
    T::from(-1)
}

type Syscall = unsafe extern "C" fn();
type Open = unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int;
type Access = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type Stat = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
type Unlink = unsafe extern "C" fn(*const c_char) -> c_int;
type Mkdir = unsafe extern "C" fn(*const c_char, libc::mode_t) -> c_int;
type Readlink = unsafe extern "C" fn(*const c_char, *mut c_char, libc::size_t) -> libc::ssize_t;
#[derive(Default)]
struct Native {
    open: Option<Open>,
    access: Option<Access>,
    stat: Option<Stat>,
    lstat: Option<Stat>,
    unlink: Option<Unlink>,
    mkdir: Option<Mkdir>,
    rmdir: Option<Unlink>,
    readlink: Option<Readlink>,
}
static NATIVE: OnceLock<Native> = OnceLock::new();
fn native() -> &'static Native {
    NATIVE.get().expect("installed before any routed call")
}
fn missing<T: From<i8>>() -> T {
    unsafe { *libc::__error() = libc::ENOSYS };
    T::from(-1)
}

unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::openat(fd, name, flags, mode as libc::c_uint),
            || native().open.map_or_else(missing, |f| f(path, flags, mode)),
        )
    }
}
unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::faccessat(fd, name, mode, 0),
            || native().access.map_or_else(missing, |f| f(path, mode)),
        )
    }
}
unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::fstatat(fd, name, buf, 0),
            || native().stat.map_or_else(missing, |f| f(path, buf)),
        )
    }
}
unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::fstatat(fd, name, buf, libc::AT_SYMLINK_NOFOLLOW),
            || native().lstat.map_or_else(missing, |f| f(path, buf)),
        )
    }
}
unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::unlinkat(fd, name, 0),
            || native().unlink.map_or_else(missing, |f| f(path)),
        )
    }
}
unsafe extern "C" fn mkdir(path: *const c_char, mode: libc::mode_t) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::mkdirat(fd, name, mode),
            || native().mkdir.map_or_else(missing, |f| f(path, mode)),
        )
    }
}
unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    unsafe {
        route(
            path,
            |fd, name| libc::unlinkat(fd, name, libc::AT_REMOVEDIR),
            || native().rmdir.map_or_else(missing, |f| f(path)),
        )
    }
}
unsafe extern "C" fn readlink(
    path: *const c_char,
    buf: *mut c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    unsafe {
        route(
            path,
            |fd, name| libc::readlinkat(fd, name, buf, size),
            || {
                native()
                    .readlink
                    .map_or_else(missing, |f| f(path, buf, size))
            },
        )
    }
}

/// Replace the path-taking entries of SQLite's (process-global) Unix
/// syscall table. Runs once, before the anchored VFS is registered.
pub(super) unsafe fn install(vfs: *mut sqlite3_vfs) -> bool {
    let (Some(get), Some(set)) = (unsafe { (*vfs).xGetSystemCall }, unsafe {
        (*vfs).xSetSystemCall
    }) else {
        return false;
    };
    let current = |name: &CStr| unsafe { get(vfs, name.as_ptr()) };
    // SAFETY: each entry has exactly the C signature SQLite casts it to.
    let native = unsafe {
        Native {
            open: current(c"open").map(|f| std::mem::transmute::<Syscall, Open>(f)),
            access: current(c"access").map(|f| std::mem::transmute::<Syscall, Access>(f)),
            stat: current(c"stat").map(|f| std::mem::transmute::<Syscall, Stat>(f)),
            lstat: current(c"lstat").map(|f| std::mem::transmute::<Syscall, Stat>(f)),
            unlink: current(c"unlink").map(|f| std::mem::transmute::<Syscall, Unlink>(f)),
            mkdir: current(c"mkdir").map(|f| std::mem::transmute::<Syscall, Mkdir>(f)),
            rmdir: current(c"rmdir").map(|f| std::mem::transmute::<Syscall, Unlink>(f)),
            readlink: current(c"readlink").map(|f| std::mem::transmute::<Syscall, Readlink>(f)),
        }
    };
    if native.open.is_none() || NATIVE.set(native).is_err() {
        return false;
    }
    let routed: [(&CStr, sqlite3_syscall_ptr); 8] = unsafe {
        [
            (c"open", Some(std::mem::transmute::<Open, Syscall>(open))),
            (
                c"access",
                Some(std::mem::transmute::<Access, Syscall>(access)),
            ),
            (c"stat", Some(std::mem::transmute::<Stat, Syscall>(stat))),
            (c"lstat", Some(std::mem::transmute::<Stat, Syscall>(lstat))),
            (
                c"unlink",
                Some(std::mem::transmute::<Unlink, Syscall>(unlink)),
            ),
            (c"mkdir", Some(std::mem::transmute::<Mkdir, Syscall>(mkdir))),
            (
                c"rmdir",
                Some(std::mem::transmute::<Unlink, Syscall>(rmdir)),
            ),
            (
                c"readlink",
                Some(std::mem::transmute::<Readlink, Syscall>(readlink)),
            ),
        ]
    };
    routed.into_iter().all(|(name, routed)| {
        // An entry SQLite was built without is never called; nothing to route.
        current(name).is_none() || unsafe { set(vfs, name.as_ptr(), routed) } == SQLITE_OK
    })
}
