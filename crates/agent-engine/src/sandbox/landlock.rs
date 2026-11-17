//! Landlock primitives for the per-session write sandbox (Linux only).
//!
//! A ruleset handles every write-type filesystem right ([`WRITE_RIGHTS`],
//! checked against the running ABI by [`handled_for_abi`]); read, execute and
//! device ioctls stay unhandled, so they stay unrestricted. Each allowed path
//! gets one path-beneath rule. A domain is applied to a child in one of two
//! ways:
//!
//! * [`confine_command`]: a `pre_exec` hook on a [`std::process::Command`]
//!   (bash tool children). The hook owns a duplicate of the ruleset fd, so
//!   nothing has to outlive the call but the `Command` itself.
//! * [`run_confined`]: a fresh, dedicated thread restricts itself and then
//!   runs a closure that spawns the child (PTY children: `portable-pty` has
//!   no `pre_exec` hook). `PR_SET_NO_NEW_PRIVS` and `landlock_restrict_self`
//!   without `TSYNC` apply to the calling thread only, and fork copies the
//!   calling thread's credentials, so no other thread of the process is
//!   touched, and the thread ends when the closure returns.
//!
//! Never restrict a thread that runs anything else afterwards (an async
//! runtime worker, a blocking-pool thread, the main thread): a Landlock
//! domain can't be removed from a thread once applied.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::Command;

pub const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub const ACCESS_FS_REFER: u64 = 1 << 13;
pub const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

/// The handled set: every write-type right. REFER is ABI 2, TRUNCATE ABI 3,
/// the rest ABI 1. Rights added by later ABIs are deliberately not handled,
/// so a newer kernel can never start denying something new.
pub const WRITE_RIGHTS: u64 = ACCESS_FS_WRITE_FILE
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_CHAR
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_BLOCK
    | ACCESS_FS_MAKE_SYM
    | ACCESS_FS_REFER
    | ACCESS_FS_TRUNCATE;

/// The handled rights that can apply to a non-directory (a file rule).
pub const FILE_WRITE_RIGHTS: u64 = ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE;

/// ABI 1 refuses every cross-directory rename and link; ABI 2 can't stop
/// truncation outside the set. Below 3 the sandbox is "unavailable".
pub const MIN_ABI: u32 = 3;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

/// Why no Landlock domain can be built on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The kernel has no Landlock syscalls (ENOSYS).
    NoLandlock,
    /// Landlock is built in but not enabled at boot (EOPNOTSUPP).
    Disabled,
    /// The running ABI is older than [`MIN_ABI`].
    AbiTooOld(u32),
    /// Any other error from the version query (errno).
    Error(i32),
}

impl Unavailable {
    /// Short fixed reason for log lines.
    pub fn reason(&self) -> &'static str {
        match self {
            Unavailable::NoLandlock => "no-landlock",
            Unavailable::Disabled => "disabled",
            Unavailable::AbiTooOld(_) => "abi-too-old",
            Unavailable::Error(_) => "error",
        }
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::AbiTooOld(abi) => write!(f, "abi-too-old (abi {abi}, need {MIN_ABI})"),
            Unavailable::Error(errno) => write!(f, "error (errno {errno})"),
            other => f.write_str(other.reason()),
        }
    }
}

/// The handled set for a running ABI.
pub fn handled_for_abi(abi: u32) -> Result<u64, Unavailable> {
    if abi < MIN_ABI {
        return Err(Unavailable::AbiTooOld(abi));
    }
    Ok(WRITE_RIGHTS)
}

/// The rights one rule grants: `requested` within `handled`, and for a
/// non-directory only the file rights (the kernel rejects others).
pub fn rule_access(handled: u64, requested: u64, is_dir: bool) -> u64 {
    let access = requested & handled;
    if is_dir {
        access
    } else {
        access & FILE_WRITE_RIGHTS
    }
}

/// The running Landlock ABI version.
pub fn abi_version() -> Result<u32, Unavailable> {
    // SAFETY: the version query takes no attribute (NULL, size 0).
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if v >= 0 {
        return Ok(v as u32);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOSYS) => Err(Unavailable::NoLandlock),
        Some(libc::EOPNOTSUPP) => Err(Unavailable::Disabled),
        Some(e) => Err(Unavailable::Error(e)),
        None => Err(Unavailable::Error(0)),
    }
}

/// [`abi_version`] checked against [`MIN_ABI`]: the version a ruleset can
/// be built with, or why none can.
pub fn usable_abi() -> Result<u32, Unavailable> {
    let abi = abi_version()?;
    handled_for_abi(abi).map(|_| abi)
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// A Landlock ruleset under construction, ready to apply to a child.
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    abi: u32,
    handled: u64,
    rules: usize,
}

impl Ruleset {
    /// A ruleset handling every write right the running ABI supports.
    pub fn new() -> Result<Self, Unavailable> {
        let abi = abi_version()?;
        let handled = handled_for_abi(abi)?;
        let attr = RulesetAttr { handled_access_fs: handled, handled_access_net: 0, scoped: 0 };
        // SAFETY: attr is a valid, fully initialised landlock_ruleset_attr.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(Unavailable::Error(errno));
        }
        // SAFETY: the kernel returned a new fd (created close-on-exec) that
        // we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        Ok(Ruleset { fd, abi, handled, rules: 0 })
    }

    /// Allow `requested` rights on the object `fd` refers to (best opened
    /// `O_PATH`): beneath it for a directory, the file rights for anything
    /// else. The object's type comes from the same fd, so a caller that
    /// validated this fd's target grants exactly that object. Rights outside
    /// the handled set are dropped; a rule that would grant nothing is
    /// skipped (returns false).
    pub fn allow_fd(&mut self, fd: BorrowedFd<'_>, requested: u64) -> io::Result<bool> {
        let is_dir = std::fs::File::from(fd.try_clone_to_owned()?).metadata()?.is_dir();
        let access = rule_access(self.handled, requested, is_dir);
        if access == 0 {
            return Ok(false);
        }
        let attr = PathBeneathAttr { allowed_access: access, parent_fd: fd.as_raw_fd() };
        // SAFETY: attr is a valid landlock_path_beneath_attr; both fds are open.
        let r = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        self.rules += 1;
        Ok(true)
    }

    /// Allow every write right beneath `path` (a directory), or the file
    /// rights on `path` (a non-directory). Opens `path` (following
    /// symlinks, `O_PATH`) and calls [`Ruleset::allow_fd`]. A missing path
    /// is an error (NotFound); the caller decides whether to skip it.
    pub fn allow(&mut self, path: &Path) -> io::Result<bool> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)?;
        self.allow_fd(file.as_fd(), WRITE_RIGHTS)
    }

    pub fn abi(&self) -> u32 {
        self.abi
    }
    pub fn rules(&self) -> usize {
        self.rules
    }
}

/// Restrict the calling thread: set no_new_privs, then
/// `landlock_restrict_self(fd, 0)`. Only raw syscalls and no allocation, so
/// it is usable in a `pre_exec` closure. The flags are fixed at 0: never
/// `LANDLOCK_RESTRICT_SELF_TSYNC`, which would restrict every thread of the
/// process.
///
/// # Safety
/// The caller guarantees:
/// * `fd` is an open Landlock ruleset fd for the whole call;
/// * the calling thread is a forked child before exec (a `pre_exec` hook) or
///   a dedicated thread that runs nothing else afterwards: never an async
///   runtime worker, a blocking-pool thread or any other reusable thread.
///
/// Use [`confine_command`] or [`run_confined`], which uphold all of this.
unsafe fn restrict_current(fd: RawFd) -> io::Result<()> {
    let (zero, one): (libc::c_ulong, libc::c_ulong) = (0, 1);
    // Required: landlock_restrict_self refuses (EPERM) without it, and it
    // keeps setuid binaries (sudo) from gaining privileges inside.
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) != 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Make `cmd`'s child restrict itself with `ruleset` before exec (a
/// `pre_exec` hook, added after any hooks already registered). The hook owns
/// a duplicate of the ruleset fd (close-on-exec), so the caller may drop
/// `ruleset` before spawning. If the restrict fails, the spawn fails with its
/// errno and nothing is exec'd: there is no unrestricted fallback.
pub fn confine_command(cmd: &mut Command, ruleset: &Ruleset) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let fd = ruleset.fd.try_clone()?;
    // SAFETY: the hook only calls restrict_current, which makes raw syscalls
    // and doesn't allocate. `fd` was created here, before any fork, and is
    // owned by the hook, so it lives as long as `cmd` (through every spawn).
    // The forked child is single-threaded and execs right after.
    unsafe {
        cmd.pre_exec(move || restrict_current(fd.as_raw_fd()));
    }
    Ok(())
}

/// Run `f` on a fresh, dedicated, named thread that first restricts itself
/// with `ruleset`; whatever `f` spawns inherits the domain. The caller's
/// thread (and every other thread) stays unrestricted. The thread is joined
/// before this returns and is never reused. If the restrict fails, `f` does
/// not run and its errno is returned.
pub fn run_confined<T, F>(ruleset: &Ruleset, f: F) -> io::Result<T>
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    let fd = ruleset.fd.as_raw_fd();
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name("sandboxed-spawn".to_string())
            .spawn_scoped(s, move || -> io::Result<T> {
                // SAFETY: `ruleset` is borrowed for the whole scope, so `fd`
                // stays open until this thread is joined. This thread is
                // dedicated: it runs `f` and ends.
                unsafe { restrict_current(fd) }?;
                Ok(f())
            })?;
        match handle.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_are_write_only() {
        // No read, execute or device-ioctl right is ever handled.
        for bit in [0u64, 2, 3, 15] {
            let right = 1u64 << bit;
            if right == ACCESS_FS_WRITE_FILE {
                continue;
            }
            assert_eq!(WRITE_RIGHTS & right, 0, "bit {bit} must stay unhandled");
        }
        assert_eq!(WRITE_RIGHTS & ACCESS_FS_WRITE_FILE, ACCESS_FS_WRITE_FILE);
    }

    #[test]
    fn handled_set_needs_abi_3() {
        assert_eq!(handled_for_abi(2), Err(Unavailable::AbiTooOld(2)));
        assert_eq!(handled_for_abi(0), Err(Unavailable::AbiTooOld(0)));
        assert_eq!(handled_for_abi(MIN_ABI), Ok(WRITE_RIGHTS));
        assert_eq!(handled_for_abi(99), Ok(WRITE_RIGHTS));
    }

    #[test]
    fn a_file_rule_grants_only_file_rights() {
        assert_eq!(rule_access(WRITE_RIGHTS, WRITE_RIGHTS, false), FILE_WRITE_RIGHTS);
        assert_eq!(rule_access(WRITE_RIGHTS, WRITE_RIGHTS, true), WRITE_RIGHTS);
        assert_eq!(rule_access(ACCESS_FS_WRITE_FILE, WRITE_RIGHTS, true), ACCESS_FS_WRITE_FILE);
        assert_eq!(rule_access(WRITE_RIGHTS, ACCESS_FS_MAKE_DIR, false), 0);
    }

    #[test]
    fn a_confined_child_writes_inside_and_not_outside() {
        if usable_abi().is_err() {
            eprintln!("skipped: no usable Landlock on this host");
            return;
        }
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut rs = Ruleset::new().unwrap();
        // Executing a shell needs no write right; /dev/null for redirects.
        assert!(rs.allow(inside.path()).unwrap());
        assert!(rs.allow(Path::new("/dev/null")).unwrap());
        let script = format!(
            "echo ok > '{}/in.txt'; echo no > '{}/out.txt'",
            inside.path().display(),
            outside.path().display()
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        confine_command(&mut cmd, &rs).unwrap();
        drop(rs);
        let out = cmd.output().unwrap();
        assert!(inside.path().join("in.txt").exists(), "write inside the grant works");
        assert!(!outside.path().join("out.txt").exists(), "write outside is refused");
        assert!(!out.status.success());
    }

    #[test]
    fn run_confined_restricts_only_its_own_thread() {
        if usable_abi().is_err() {
            eprintln!("skipped: no usable Landlock on this host");
            return;
        }
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut rs = Ruleset::new().unwrap();
        assert!(rs.allow(inside.path()).unwrap());
        let target = outside.path().join("t.txt");
        let wrote_inside = run_confined(&rs, || std::fs::write(inside.path().join("a"), b"x").is_ok()).unwrap();
        let wrote_outside = run_confined(&rs, || std::fs::write(&target, b"x").is_ok()).unwrap();
        assert!(wrote_inside);
        assert!(!wrote_outside);
        // This thread was never restricted.
        std::fs::write(&target, b"x").unwrap();
    }

    /// Prints what this host's kernel offers, so a run with `--nocapture`
    /// records the Landlock ABI the real-kernel tests ran against. Never
    /// fails: a host without usable Landlock is reported, not faked.
    #[test]
    fn reports_the_kernels_landlock_abi() {
        match abi_version() {
            Ok(abi) => eprintln!("landlock-abi: {abi} (minimum {MIN_ABI}, usable: {})", abi >= MIN_ABI),
            Err(why) => eprintln!("landlock-abi: none ({why})"),
        }
    }
}
