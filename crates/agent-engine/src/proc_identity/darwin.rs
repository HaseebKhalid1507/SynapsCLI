//! Darwin: what the macOS reads (`macos.rs`) give, as bytes, and every rule
//! read from them: the reply's layout and size, the state letters, the
//! sid's bracket and the parent link. No FFI here: this module is compiled
//! and tested on every platform, from byte fixtures built off the declared
//! layout; only macOS reads through it.
//!
//! A process is read with `proc_pidinfo(pid, PROC_PIDT_BSDINFOWITHUNIQID,
//! 0, buf, 192)`. Flavor 18 is private SPI (absent from the public SDK), so
//! its reply is declared here per xnu `bsd/sys/proc_info_private.h`: the
//! public `struct proc_bsdinfo` (136 bytes) then `struct
//! proc_uniqidentifierinfo` (56 bytes). A reply of any other size is
//! refused, so a kernel whose layout moved fails closed.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io;
use std::mem::{offset_of, size_of};

use super::{BootKey, Parent, ProcessIdentity, StartKey};

/// `PROC_PIDT_BSDINFOWITHUNIQID`.
pub(super) const FLAVOR: i32 = 18;
/// The only reply size taken.
pub(super) const SIZE: usize = 192;

/// `struct proc_bsdinfo` (`<sys/proc_info.h>`). The layout only: replies
/// are read by offset, never through this type.
#[allow(dead_code)]
#[repr(C)]
pub(super) struct BsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [u8; 16],
    pbi_name: [u8; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

/// `struct proc_uniqidentifierinfo` (xnu `proc_info_private.h`). The
/// layout only. `p_idversion` (the pid version) is never read: every exec
/// bumps it.
#[allow(dead_code)]
#[repr(C)]
pub(super) struct UniqIdInfo {
    p_uuid: [u8; 16],
    p_uniqueid: u64,
    p_puniqueid: u64,
    p_idversion: i32,
    p_orig_ppidversion: i32,
    p_reserve2: u64,
    p_reserve3: u64,
}

/// `struct proc_bsdinfowithuniqid`: the flavor-18 reply.
#[allow(dead_code)]
#[repr(C)]
pub(super) struct BsdInfoWithUniqId {
    pbsd: BsdInfo,
    p_uniqidentifier: UniqIdInfo,
}

const _: () = assert!(size_of::<BsdInfo>() == 136);
const _: () = assert!(size_of::<UniqIdInfo>() == 56);
const _: () = assert!(size_of::<BsdInfoWithUniqId>() == SIZE);

const UNIQ: usize = offset_of!(BsdInfoWithUniqId, p_uniqidentifier);
const STATUS: usize = offset_of!(BsdInfo, pbi_status);
const PID: usize = offset_of!(BsdInfo, pbi_pid);
const PPID: usize = offset_of!(BsdInfo, pbi_ppid);
const PGID: usize = offset_of!(BsdInfo, pbi_pgid);
const UNIQUE_ID: usize = UNIQ + offset_of!(UniqIdInfo, p_uniqueid);
const PARENT_UNIQUE_ID: usize = UNIQ + offset_of!(UniqIdInfo, p_puniqueid);

// The offsets the probe on macOS 26.3.1 (arm64) read.
const _: () = assert!(STATUS == 4 && PID == 12 && PPID == 16 && PGID == 100);
const _: () = assert!(UNIQ == 136 && UNIQUE_ID == 152 && PARENT_UNIQUE_ID == 160);

/// `pbi_status` values (`<sys/proc.h>`).
const SIDL: u32 = 1;
const SRUN: u32 = 2;
const SSLEEP: u32 = 3;
const SSTOP: u32 = 4;
const SZOMB: u32 = 5;

/// The ps(1) letter of a `pbi_status`, as macOS ps prints it: being created
/// (`SIDL`) and runnable (`SRUN`) are `R`, sleeping `S`, stopped `T`, a
/// zombie `Z`. `None` for any other value: such a process is never taken
/// as running or as gone.
pub(super) fn state_letter(status: u32) -> Option<char> {
    match status {
        SIDL | SRUN => Some('R'),
        SSLEEP => Some('S'),
        SSTOP => Some('T'),
        SZOMB => Some('Z'),
        _ => None,
    }
}

/// One flavor-18 reply, read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BsdRead {
    pub pid: u32,
    pub state: char,
    pub ppid: u32,
    pub pgid: u32,
    /// `p_uniqueid`: this process, for as long as the host runs.
    pub unique_id: u64,
    /// `p_puniqueid`: the `p_uniqueid` of the parent it was started by.
    pub parent_unique_id: u64,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("proc_pidinfo: {what}"))
}

fn u32_at(reply: &[u8; SIZE], at: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&reply[at..at + 4]);
    u32::from_ne_bytes(bytes)
}

fn u64_at(reply: &[u8; SIZE], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&reply[at..at + 8]);
    u64::from_ne_bytes(bytes)
}

/// Read the flavor-18 reply for `pid` (the bytes the kernel wrote, in its
/// own byte order). Refused unless it is exactly [`SIZE`] bytes, names
/// `pid`, and holds a known state.
pub(super) fn parse(pid: u32, reply: &[u8]) -> io::Result<BsdRead> {
    let reply: &[u8; SIZE] =
        reply.try_into().map_err(|_| invalid(&format!("{} bytes, not {SIZE}", reply.len())))?;
    if u32_at(reply, PID) != pid {
        return Err(invalid("the reply names another pid"));
    }
    let status = u32_at(reply, STATUS);
    let state = state_letter(status).ok_or_else(|| invalid(&format!("unknown pbi_status {status}")))?;
    Ok(BsdRead {
        pid,
        state,
        ppid: u32_at(reply, PPID),
        pgid: u32_at(reply, PGID),
        unique_id: u64_at(reply, UNIQUE_ID),
        parent_unique_id: u64_at(reply, PARENT_UNIQUE_ID),
    })
}

/// The process from a read, the sid `getsid` gave, and a second read
/// after it: the sid is taken only when both reads name the same process
/// (the same `p_uniqueid`), so it is never that of another process that
/// reused the pid in between. The first read's fields are kept.
pub(super) fn identity(first: BsdRead, sid: u32, again: BsdRead) -> io::Result<ProcessIdentity> {
    if again.pid != first.pid || again.unique_id != first.unique_id {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "the pid changed while it was read"));
    }
    Ok(ProcessIdentity {
        pid: first.pid,
        state: first.state,
        ppid: first.ppid,
        pgid: first.pgid,
        sid,
        start: StartKey::Darwin { unique_id: first.unique_id },
    })
}

/// The parent of `child`, read as `parent` (the process at its `ppid`):
/// taken only when that is the process it was started by, its
/// `p_uniqueid` equal to the child's `p_puniqueid`. An orphan (adopted by
/// launchd, still naming its first parent) or a parent whose pid was
/// reused fails closed.
pub(super) fn parent_of(child: &BsdRead, parent: &BsdRead) -> io::Result<Parent> {
    if parent.pid != child.ppid || parent.unique_id != child.parent_unique_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the process at its ppid is not the parent it was started by",
        ));
    }
    Ok(Parent { pid: parent.pid, start: StartKey::Darwin { unique_id: parent.unique_id } })
}

/// The boot key from `kern.bootsessionuuid` as read (a C string, maybe
/// with its NUL); `None` when empty or not text.
pub(super) fn boot_key(raw: &[u8]) -> Option<BootKey> {
    let text = raw.split(|b| *b == 0).next()?;
    let session = std::str::from_utf8(text).ok()?.trim();
    (!session.is_empty()).then(|| BootKey::Darwin { boot_session: session.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flavor-18 reply as the kernel lays it out, with every field this
    /// module reads set and the rest filled with noise.
    fn reply(pid: u32, status: u32, ppid: u32, pgid: u32, unique_id: u64, parent_unique_id: u64) -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..SIZE).map(|i| (i * 7 + 3) as u8).collect();
        let mut put = |at: usize, value: &[u8]| bytes[at..at + value.len()].copy_from_slice(value);
        put(offset_of!(BsdInfo, pbi_status), &status.to_ne_bytes());
        put(offset_of!(BsdInfo, pbi_pid), &pid.to_ne_bytes());
        put(offset_of!(BsdInfo, pbi_ppid), &ppid.to_ne_bytes());
        put(offset_of!(BsdInfo, pbi_pgid), &pgid.to_ne_bytes());
        put(offset_of!(BsdInfo, pbi_comm), b"sleep\0\0\0\0\0\0\0\0\0\0\0");
        let uniq = offset_of!(BsdInfoWithUniqId, p_uniqidentifier);
        put(uniq + offset_of!(UniqIdInfo, p_uniqueid), &unique_id.to_ne_bytes());
        put(uniq + offset_of!(UniqIdInfo, p_puniqueid), &parent_unique_id.to_ne_bytes());
        put(uniq + offset_of!(UniqIdInfo, p_idversion), &1234i32.to_ne_bytes());
        bytes
    }

    fn read(pid: u32, unique_id: u64, ppid: u32, parent_unique_id: u64) -> BsdRead {
        BsdRead { pid, state: 'S', ppid, pgid: pid, unique_id, parent_unique_id }
    }

    #[test]
    fn the_layout_is_xnus() {
        assert_eq!((size_of::<BsdInfo>(), size_of::<UniqIdInfo>(), size_of::<BsdInfoWithUniqId>()), (136, 56, 192));
        assert_eq!(
            (offset_of!(BsdInfo, pbi_comm), offset_of!(BsdInfo, pbi_nfiles), offset_of!(BsdInfo, pbi_start_tvsec)),
            (48, 96, 120)
        );
        let uniq = offset_of!(BsdInfoWithUniqId, p_uniqidentifier);
        assert_eq!(uniq + offset_of!(UniqIdInfo, p_idversion), 168);
        assert_eq!(uniq + offset_of!(UniqIdInfo, p_reserve3), 184);
    }

    #[test]
    fn a_reply_reads_its_fields() {
        let got = parse(4242, &reply(4242, SSLEEP, 1, 4240, 4_000_000_123, 77)).unwrap();
        assert_eq!(
            got,
            BsdRead { pid: 4242, state: 'S', ppid: 1, pgid: 4240, unique_id: 4_000_000_123, parent_unique_id: 77 }
        );
    }

    #[test]
    fn each_status_maps_to_its_letter_and_any_other_fails_closed() {
        let letters: Vec<Option<char>> = (0..=6).map(state_letter).collect();
        assert_eq!(letters, vec![None, Some('R'), Some('R'), Some('S'), Some('T'), Some('Z'), None]);
        for (status, letter) in [(SIDL, 'R'), (SRUN, 'R'), (SSLEEP, 'S'), (SSTOP, 'T'), (SZOMB, 'Z')] {
            assert_eq!(parse(9, &reply(9, status, 1, 9, 5, 6)).unwrap().state, letter);
        }
        let zombie = parse(9, &reply(9, SZOMB, 1, 9, 5, 6)).unwrap();
        assert!(identity(zombie, 9, zombie).unwrap().exited(), "a zombie has exited");
        let stopped = parse(9, &reply(9, SSTOP, 1, 9, 5, 6)).unwrap();
        assert!(identity(stopped, 9, stopped).unwrap().is_live(), "a stopped process runs");
        for status in [0, 6, 7, u32::MAX] {
            let error = parse(9, &reply(9, status, 1, 9, 5, 6)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{status}: {error}");
        }
    }

    #[test]
    fn only_a_reply_of_exactly_192_bytes_for_the_pid_is_taken() {
        let full = reply(9, SRUN, 1, 9, 5, 6);
        for len in [0, 1, 136, 191] {
            let error = parse(9, &full[..len]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{len}: {error}");
        }
        let mut long = full.clone();
        long.push(0);
        assert!(parse(9, &long).is_err(), "193 bytes");
        assert!(parse(10, &full).is_err(), "a reply naming another pid");
        assert!(parse(9, &full).is_ok());
    }

    #[test]
    fn the_sid_is_taken_only_between_two_reads_of_one_process() {
        let first = read(42, 1000, 1, 7);
        let process = identity(first, 40, first).unwrap();
        assert_eq!(
            process,
            ProcessIdentity { pid: 42, state: 'S', ppid: 1, pgid: 42, sid: 40, start: StartKey::Darwin { unique_id: 1000 } }
        );
        assert_eq!(process.start_ticks(), None, "no Linux start ticks");
        let reused = read(42, 1001, 1, 7);
        assert_eq!(identity(first, 40, reused).unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    /// Accepted only while the parent is the one the child was started by;
    /// an orphan keeps its dead parent's unique id and fails closed.
    #[test]
    fn the_parent_link_holds_only_for_the_parent_it_was_started_by() {
        let parent = read(100, 5000, 1, 1);
        let child = read(200, 6000, 100, 5000);
        assert_eq!(parent_of(&child, &parent).unwrap(), Parent { pid: 100, start: StartKey::Darwin { unique_id: 5000 } });
        let reused = read(100, 5001, 1, 1);
        assert_eq!(parent_of(&child, &reused).unwrap_err().kind(), io::ErrorKind::InvalidData, "pid reused");
        let launchd = read(1, 1, 0, 0);
        let orphan = read(200, 6000, 1, 5000);
        assert_eq!(parent_of(&orphan, &launchd).unwrap_err().kind(), io::ErrorKind::InvalidData, "an orphan");
        assert!(parent_of(&child, &launchd).is_err(), "not the process at its ppid");
    }

    #[test]
    fn the_boot_session_is_the_text_before_the_nul() {
        let key = BootKey::Darwin { boot_session: "6B1E3E7C-24C1-4C4B-9D2A-0B5C1E2F3A4B".into() };
        assert_eq!(boot_key(b"6B1E3E7C-24C1-4C4B-9D2A-0B5C1E2F3A4B\0"), Some(key.clone()));
        assert_eq!(boot_key(b"6B1E3E7C-24C1-4C4B-9D2A-0B5C1E2F3A4B\0\0garbage"), Some(key.clone()));
        assert_eq!(boot_key(b"6B1E3E7C-24C1-4C4B-9D2A-0B5C1E2F3A4B"), Some(key));
        assert_eq!(boot_key(b"\0"), None);
        assert_eq!(boot_key(b""), None);
        assert_eq!(boot_key(b" \n"), None);
        assert_eq!(boot_key(&[0xff, 0xfe, 0]), None, "not text");
    }
}
