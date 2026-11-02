//! macOS: the reads, and only the reads. Each FFI call sits in one small
//! function here; what its bytes mean (layout, size, state letters, the
//! sid's bracket, the parent link, the boot key's text) is in `darwin.rs`,
//! which every platform compiles and tests.
//!
//! - The boot key: `sysctlbyname("kern.bootsessionuuid")`.
//! - A process: `proc_pidinfo(pid, PROC_PIDT_BSDINFOWITHUNIQID, 0, buf,
//!   192)`, exactly 192 bytes or a failure, then `getsid(pid)`, then the
//!   same read again (see [`darwin::identity`]). The unique id
//!   (`p_uniqueid`) survives every exec; the pid version does not, so no
//!   key holds it. With `arg` 0 the kernel doesn't look up zombies: an
//!   exited, unreaped process reads as gone (`NotFound`). Another user's
//!   process the kernel won't describe (`EPERM`) is an error, never a
//!   guess.
//! - The parent: the process at `pbi_ppid`, taken only while its unique id
//!   is the child's `p_puniqueid` (see [`darwin::parent_of`]).
//! - Every process: `proc_listallpids`.

use std::ffi::c_void;
use std::io;
use std::sync::OnceLock;

use super::darwin::{self, BsdRead};
use super::{BootKey, Parent, ProcessIdentity, StartKey};

// The public half of the declared reply is libc's `proc_bsdinfo`, field for
// field where it is read.
const _: () = assert!(std::mem::size_of::<libc::proc_bsdinfo>() == 136);
const _: () = assert!(std::mem::offset_of!(libc::proc_bsdinfo, pbi_status) == 4);
const _: () = assert!(std::mem::offset_of!(libc::proc_bsdinfo, pbi_pid) == 12);
const _: () = assert!(std::mem::offset_of!(libc::proc_bsdinfo, pbi_ppid) == 16);
const _: () = assert!(std::mem::offset_of!(libc::proc_bsdinfo, pbi_pgid) == 100);

/// Processes can be identified here: a read of this process gives exactly
/// the 192 bytes the declared layout expects. Checked once.
pub(super) fn supported() -> bool {
    static SELF_READ: OnceLock<bool> = OnceLock::new();
    *SELF_READ.get_or_init(|| read(std::process::id()).is_ok())
}

pub(super) fn boot_key() -> Option<BootKey> {
    darwin::boot_key(&boot_session_bytes()?)
}

/// A start key from its number: the unique id.
pub(super) fn start_key(number: u64) -> StartKey {
    StartKey::Darwin { unique_id: number }
}

pub(super) fn identify(pid: u32) -> io::Result<ProcessIdentity> {
    let first = read(pid)?;
    let sid = sid_of(pid)?;
    let again = read(pid)?;
    darwin::identity(first, sid, again)
}

pub(super) fn current() -> io::Result<ProcessIdentity> {
    identify(std::process::id())
}

pub(super) fn parent(pid: u32) -> io::Result<Option<Parent>> {
    let child = read(pid)?;
    if child.ppid == 0 {
        return Ok(None);
    }
    darwin::parent_of(&child, &read(child.ppid)?).map(Some)
}

/// Every pid the kernel lists whose reads succeed.
pub(super) fn all() -> Vec<ProcessIdentity> {
    list_pids().into_iter().filter_map(|pid| identify(pid).ok()).collect()
}

/// One flavor-18 read of `pid`, parsed.
fn read(pid: u32) -> io::Result<BsdRead> {
    let mut reply = [0u8; darwin::SIZE];
    let len = pidinfo(pid, &mut reply)?;
    darwin::parse(pid, &reply[..len])
}

/// The errno of the call that just failed: no such process is `NotFound`;
/// `EPERM` stays `PermissionDenied`.
fn os_error() -> io::Error {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => io::Error::new(io::ErrorKind::NotFound, error),
        Some(0) | None => io::Error::new(io::ErrorKind::InvalidData, "the call failed without an errno"),
        Some(_) => error,
    }
}

/// `pid` as the kernel's type; one past its range names no process.
fn pid_t(pid: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(pid).map_err(|_| io::Error::new(io::ErrorKind::NotFound, "pid out of range"))
}

/// `proc_pidinfo(pid, PROC_PIDT_BSDINFOWITHUNIQID, 0, reply, 192)`: the
/// byte count the kernel wrote into `reply`.
fn pidinfo(pid: u32, reply: &mut [u8; darwin::SIZE]) -> io::Result<usize> {
    let pid = pid_t(pid)?;
    // SAFETY: `reply` is a live, writable buffer of the size passed; the
    // kernel writes at most that many bytes into it and keeps no pointer.
    let got = unsafe {
        libc::proc_pidinfo(pid, darwin::FLAVOR, 0, reply.as_mut_ptr().cast::<c_void>(), darwin::SIZE as libc::c_int)
    };
    if got <= 0 {
        return Err(os_error());
    }
    Ok((got as usize).min(darwin::SIZE))
}

/// `getsid(pid)`.
fn sid_of(pid: u32) -> io::Result<u32> {
    let pid = pid_t(pid)?;
    // SAFETY: getsid takes a pid and returns a sid or -1; no memory is
    // involved.
    let sid = unsafe { libc::getsid(pid) };
    if sid < 0 {
        return Err(os_error());
    }
    Ok(sid as u32)
}

/// `sysctlbyname("kern.bootsessionuuid")`, as the kernel wrote it.
fn boot_session_bytes() -> Option<Vec<u8>> {
    let mut buf = [0u8; 128];
    let mut len: libc::size_t = buf.len();
    // SAFETY: the name is a NUL-terminated C string; `buf` is a live,
    // writable buffer of `len` bytes, into which the kernel writes at most
    // `len` bytes and then stores the count in `len`; nothing is set (null
    // new value, length 0).
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| buf[..len.min(buf.len())].to_vec())
}

/// `proc_listallpids` into `pids`: how many it wrote. A null buffer asks
/// for the count only.
fn list_into(pids: Option<&mut [libc::c_int]>) -> usize {
    let (ptr, bytes) = match pids {
        Some(pids) => (
            pids.as_mut_ptr().cast::<c_void>(),
            libc::c_int::try_from(std::mem::size_of_val(pids)).unwrap_or(libc::c_int::MAX),
        ),
        None => (std::ptr::null_mut(), 0),
    };
    // SAFETY: `ptr` is null with size 0, or a live, writable buffer of
    // `bytes` bytes (rounded down to whole pids by the kernel); the kernel
    // writes at most that and keeps no pointer.
    let got = unsafe { libc::proc_listallpids(ptr, bytes) };
    usize::try_from(got).unwrap_or(0)
}

/// Every pid now. A list that fills its buffer may have been cut short, so
/// it is read again with more room.
fn list_pids() -> Vec<u32> {
    let mut room = list_into(None).saturating_add(64);
    let mut pids: Vec<libc::c_int> = Vec::new();
    for _ in 0..4 {
        pids = vec![0; room];
        let got = list_into(Some(pids.as_mut_slice())).min(room);
        pids.truncate(got);
        if got < room {
            break;
        }
        room = room.saturating_mul(2);
    }
    pids.into_iter().filter_map(|pid| u32::try_from(pid).ok()).collect()
}
