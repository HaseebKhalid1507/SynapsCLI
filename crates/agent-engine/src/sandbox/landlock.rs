//! Landlock primitives for the per-session write sandbox (Linux only).
//!
//! A ruleset handles every write-type filesystem right of the design set
//! ([`WRITE_RIGHTS`], checked against the running ABI by [`handled_for_abi`]);
//! read, execute and device ioctls stay unhandled, so they stay unrestricted.
//! Each allowed path gets one path-beneath rule. A domain is applied to a
//! child in one of two ways:
//!
//! * [`confine_command`]: a `pre_exec` hook on a [`std::process::Command`]
//!   (bash tool children). The hook owns a duplicate of the ruleset fd and the
//!   tag, so nothing has to outlive the call but the `Command` itself.
//! * [`run_confined`]: a fresh, dedicated thread restricts itself and then
//!   runs a closure that spawns the child (PTY children: portable-pty has no
//!   pre_exec hook). `PR_SET_NO_NEW_PRIVS` and `landlock_restrict_self`
//!   without `TSYNC` apply to the calling thread only, and fork copies the
//!   calling thread's credentials, so no other thread of the process is
//!   touched, and the thread ends when the closure returns.
//!
//! Both set the restricting thread's comm to a [`Tag`] (`sbx-…`): the kernel's
//! audit record for a domain (`LANDLOCK_DOMAIN`, ABI 7+) names it, which maps
//! a denial back to the spawn that caused it.
//!
//! Never restrict a thread that runs anything else afterwards (an async
//! runtime worker, a blocking-pool thread, the main thread): a Landlock
//! domain can't be removed from a thread once applied.
//!
//! Off Linux only the rights, [`Unavailable`] and the pure helpers are
//! compiled: [`abi_version`] answers [`Unavailable::UnsupportedOs`] there,
//! and nothing that needs the kernel exists.

#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

pub const ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub const ACCESS_FS_READ_DIR: u64 = 1 << 3;
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
pub const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

/// The handled set: every write-type right of the design. REFER is ABI 2,
/// TRUNCATE ABI 3, the rest ABI 1. IOCTL_DEV (ABI 5) and rights added by
/// later ABIs are deliberately not handled, so a newer kernel can never start
/// denying something new.
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

#[cfg(target_os = "linux")]
const CREATE_RULESET_VERSION: u32 = 1 << 0;
/// UAPI `LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON` (ABI 7).
const RESTRICT_SELF_LOG_NEW_EXEC_ON: u32 = 1 << 1;
/// The only `landlock_restrict_self` flags [`restrict_current`] passes on.
/// In particular never `LANDLOCK_RESTRICT_SELF_TSYNC` (bit 3), which would
/// restrict every thread of the process.
const ALLOWED_RESTRICT_FLAGS: u32 = RESTRICT_SELF_LOG_NEW_EXEC_ON;
#[cfg(target_os = "linux")]
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
    /// This OS has no Landlock at all (every OS but Linux). Unlike the
    /// kernel cases above, an enforcing policy is never run unsandboxed
    /// for it, whatever `required` says ([`Unavailable::always_refuses`]).
    UnsupportedOs,
}

impl Unavailable {
    /// Short fixed reason for log lines.
    pub fn reason(&self) -> &'static str {
        match self {
            Unavailable::NoLandlock => "no-landlock",
            Unavailable::Disabled => "disabled",
            Unavailable::AbiTooOld(_) => "abi-too-old",
            Unavailable::Error(_) => "error",
            Unavailable::UnsupportedOs => "unsupported-os",
        }
    }

    /// Whether an enforcing policy refuses every spawn for this reason
    /// even without `required = true`: only [`Unavailable::UnsupportedOs`]
    /// (fail closed where no Landlock can ever exist). Linux never sees it.
    pub fn always_refuses(&self) -> bool {
        matches!(self, Unavailable::UnsupportedOs)
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::AbiTooOld(abi) => write!(f, "abi-too-old (abi {abi}, need {MIN_ABI})"),
            Unavailable::Error(errno) => write!(f, "error (errno {errno})"),
            Unavailable::UnsupportedOs => write!(f, "unsupported-os (no Landlock on {})", std::env::consts::OS),
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

/// `landlock_restrict_self` flags for a running ABI: from ABI 7 on, ask the
/// kernel to audit denials after exec too (the default audits only the
/// process that called restrict_self, before its exec).
pub fn restrict_flags_for_abi(abi: u32) -> u32 {
    if abi >= 7 {
        RESTRICT_SELF_LOG_NEW_EXEC_ON
    } else {
        0
    }
}

/// Whether the restrict step accepts `flags`: only the exec-audit bit
/// (`LOG_NEW_EXEC_ON`), so never TSYNC.
pub fn restrict_flags_ok(flags: u32) -> bool {
    flags & !ALLOWED_RESTRICT_FLAGS == 0
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

/// Whether `s` is a valid tag: `sbx-` followed by 1 to 11 of `[0-9a-z]`, so
/// at most 15 bytes (the kernel's comm limit: `PR_SET_NAME` never truncates
/// it, and two different tags never look the same in an audit record).
pub fn valid_tag(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("sbx-") else {
        return false;
    };
    (1..=11).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// A validated comm tag for one restricted spawn (see [`valid_tag`]). Built
/// and checked in the parent, before any fork; NUL-terminated.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tag(CString);

/// This process's tag counter, seeded once from the OS RNG ([`fresh_seed`]),
/// in the parent: (the seed, the next tag number).
#[cfg(target_os = "linux")]
static NEXT_TAG: OnceLock<(u64, AtomicU64)> = OnceLock::new();

#[cfg(target_os = "linux")]
fn next_tag() -> &'static (u64, AtomicU64) {
    NEXT_TAG.get_or_init(|| {
        let seed = fresh_seed();
        (seed, AtomicU64::new(tag_seed(seed)))
    })
}

#[cfg(target_os = "linux")]
impl Tag {
    /// A tag from a string; refuses anything [`valid_tag`] refuses.
    pub fn new(s: &str) -> io::Result<Tag> {
        if !valid_tag(s) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid sandbox tag {s:?}")));
        }
        CString::new(s).map(Tag).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// A fresh tag, unique within this process; two starts (an exec reload
    /// keeps the pid, so the audit join can't tell two images' spawns apart
    /// by pid alone) draw random seeds ([`tag_seed`]) and share a block only
    /// 1 time in 36^5.
    pub fn next() -> Tag {
        tag_for(next_tag().1.fetch_add(1, Ordering::Relaxed))
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().expect("a validated tag is ASCII")
    }

    pub fn as_cstr(&self) -> &CStr {
        &self.0
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first tag number of a process seeded with `seed`: each seed value
/// owns a block of 36^6 numbers (6 base-36 digits), and the seed is taken
/// mod 36^5, so a tag is at most `sbx-` + 11 digits, 15 bytes, the most
/// comm holds (`sbx-zzzzzzzzzzz`).
pub fn tag_seed(seed: u64) -> u64 {
    (seed % 36u64.pow(5)) * 36u64.pow(6)
}

/// The value this process's tags were seeded from.
#[cfg(target_os = "linux")]
pub fn tag_seed_base() -> u64 {
    next_tag().0
}

/// A fresh seed value for [`tag_seed`], below 36^5: 8 bytes of the OS RNG
/// (getrandom, else /dev/urandom); the clock's seconds only if both fail.
#[cfg(target_os = "linux")]
pub fn fresh_seed() -> u64 {
    let mut buf = [0u8; 8];
    // SAFETY: getrandom writes at most buf.len() bytes into buf.
    let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    let got = usize::try_from(n).is_ok_and(|n| n == buf.len()) || {
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok()
    };
    let raw = if got {
        u64::from_ne_bytes(buf)
    } else {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    };
    raw % 36u64.pow(5)
}

/// `sbx-` + `n` in base 36, wrapped to 11 digits (15 bytes in all).
#[cfg(target_os = "linux")]
pub fn tag_for(n: u64) -> Tag {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = n % 36u64.pow(11);
    let mut buf = Vec::with_capacity(11);
    loop {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    buf.reverse();
    let s = format!("sbx-{}", String::from_utf8(buf).expect("ascii digits"));
    Tag::new(&s).expect("tag_for always builds a valid tag")
}

/// The running Landlock ABI version: always [`Unavailable::UnsupportedOs`]
/// off Linux.
#[cfg(not(target_os = "linux"))]
pub fn abi_version() -> Result<u32, Unavailable> {
    Err(Unavailable::UnsupportedOs)
}

/// The running Landlock ABI version.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[cfg(target_os = "linux")]
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// A Landlock ruleset under construction, ready to apply to a child.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    abi: u32,
    handled: u64,
    flags: u32,
    rules: usize,
}

#[cfg(target_os = "linux")]
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
        // SAFETY: the kernel returned a new fd (O_CLOEXEC) that we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        Ok(Ruleset { fd, abi, handled, flags: restrict_flags_for_abi(abi), rules: 0 })
    }

    /// Allow `requested` rights beneath `path` (a directory), or the file
    /// rights on `path` (a non-directory). Opens `path` (following symlinks)
    /// and calls [`Ruleset::allow_fd`]. A missing path is an error
    /// (NotFound); the caller decides whether to skip it.
    pub fn allow(&mut self, path: &Path, requested: u64) -> io::Result<bool> {
        use std::os::unix::fs::OpenOptionsExt;
        // O_PATH: a handle for the rule only, no read permission needed.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)?;
        self.allow_fd(file.as_fd(), requested)
    }

    /// Allow `requested` rights on the object `fd` refers to (best opened
    /// `O_PATH`): beneath it for a directory, the file rights for anything
    /// else. The object's type comes from the same fd, so a caller that
    /// validated this fd's target grants exactly that object. Rights outside
    /// the handled set are dropped; a rule that would grant nothing is skipped
    /// (returns false).
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

    pub fn abi(&self) -> u32 {
        self.abi
    }
    pub fn handled(&self) -> u64 {
        self.handled
    }
    pub fn flags(&self) -> u32 {
        self.flags
    }
    pub fn rules(&self) -> usize {
        self.rules
    }
}

/// Restrict the calling thread: refuse unknown flags, set its comm to `tag`,
/// set no_new_privs, then `landlock_restrict_self(fd, flags)`. Only raw
/// syscalls and no allocation, so it is usable in a `pre_exec` closure.
/// Flags other than `LOG_NEW_EXEC_ON` (TSYNC included) are refused with
/// EINVAL before anything changes.
///
/// # Safety
/// The caller guarantees:
/// * `fd` is an open Landlock ruleset fd for the whole call;
/// * the calling thread is a forked child before exec (a `pre_exec` hook) or
///   a dedicated thread that runs nothing else afterwards: never an async
///   runtime worker, a blocking-pool thread or any other reusable thread;
/// * in a `pre_exec` hook, `tag` and the ruleset fd were created before the
///   fork and stay alive until the spawn returns (the usual post-fork rules).
///
/// Use [`confine_command`] or [`run_confined`], which uphold all of this.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn restrict_current(fd: RawFd, flags: u32, tag: &Tag) -> io::Result<()> {
    if !restrict_flags_ok(flags) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let (zero, one): (libc::c_ulong, libc::c_ulong) = (0, 1);
    // Best effort, result ignored on purpose. The validated tag (<= 15 bytes,
    // NUL-terminated) is never truncated, but an inherited seccomp filter can
    // still refuse prctl. Then only the audit record's tag is lost: the
    // containment below doesn't depend on the name.
    libc::prctl(libc::PR_SET_NAME, tag.as_cstr().as_ptr() as libc::c_ulong, zero, zero, zero);
    // Required: landlock_restrict_self refuses (EPERM) without it, and it
    // keeps setuid binaries (sudo) from gaining privileges inside.
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) != 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::syscall(libc::SYS_landlock_restrict_self, fd, flags) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Make `cmd`'s child restrict itself with `ruleset` before exec (a
/// `pre_exec` hook, added after any hooks already registered). The hook owns
/// a duplicate of the ruleset fd (O_CLOEXEC) and `tag`, so the caller may drop
/// `ruleset` before spawning. If the restrict fails, the spawn fails with its
/// errno and nothing is exec'd: there is no unrestricted fallback.
#[cfg(target_os = "linux")]
pub fn confine_command(cmd: &mut Command, ruleset: &Ruleset, tag: Tag) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let fd = ruleset.fd.try_clone()?;
    let flags = ruleset.flags;
    // SAFETY: the hook only calls restrict_current, which makes raw syscalls
    // and doesn't allocate. `fd` and `tag` were created here, before any fork,
    // and are owned by the hook, so they live as long as `cmd` (through every
    // spawn). The forked child is single-threaded and execs right after.
    unsafe {
        cmd.pre_exec(move || restrict_current(fd.as_raw_fd(), flags, &tag));
    }
    Ok(())
}

/// Run `f` on a fresh, dedicated, named thread that first restricts itself
/// with `ruleset`; whatever `f` spawns inherits the domain. The caller's
/// thread (and every other thread) stays unrestricted. The thread is joined
/// before this returns and is never reused. If the restrict fails, `f` does
/// not run and its errno is returned.
#[cfg(target_os = "linux")]
pub fn run_confined<T, F>(ruleset: &Ruleset, tag: &Tag, f: F) -> io::Result<T>
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    let fd = ruleset.fd.as_raw_fd();
    let flags = ruleset.flags;
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name(tag.as_str().to_string())
            .spawn_scoped(s, move || -> io::Result<T> {
                // SAFETY: `ruleset` is borrowed for the whole scope, so `fd`
                // stays open until this thread is joined; `tag` is validated
                // and borrowed likewise. This thread is dedicated: it runs `f`
                // and ends.
                unsafe { restrict_current(fd, flags, tag) }?;
                Ok(f())
            })?;
        match handle.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
