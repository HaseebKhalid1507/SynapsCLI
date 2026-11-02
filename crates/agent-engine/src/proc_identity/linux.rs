//! Linux: `/proc`. Compiled everywhere so the parser's tests run on every
//! platform; only Linux reads through it.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io;

use super::{BootKey, Parent, ProcessIdentity, StartKey};

pub(super) fn supported() -> bool {
    true
}

/// A start key from its number: the start ticks.
pub(super) fn start_key(number: u64) -> StartKey {
    StartKey::Linux { start_ticks: number }
}

/// Parse the `/proc/<pid>/stat` line of `pid`. The command name (field 2)
/// is in parentheses and may itself hold spaces and `)`, so the fields are
/// split after the LAST `)`: field N is then index N-3.
pub(super) fn parse_stat(pid: u32, line: &str) -> Option<ProcessIdentity> {
    let rest = line.get(line.rfind(')')? + 1..)?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?; // field 3
    let ppid = fields.next()?.parse().ok()?; // 4
    let pgid = fields.next()?.parse().ok()?; // 5
    let sid = fields.next()?.parse().ok()?; // 6
    let start_ticks = fields.nth(15)?.parse().ok()?; // 22
    Some(ProcessIdentity { pid, state, ppid, pgid, sid, start: StartKey::Linux { start_ticks } })
}

/// Read and parse one stat file. `NotFound` when there is no such pid.
fn read(pid: u32, path: &str) -> io::Result<ProcessIdentity> {
    let line = std::fs::read_to_string(path).map_err(|error| {
        // A pid that exits mid-read can answer ESRCH.
        if error.raw_os_error() == Some(3) {
            io::Error::new(io::ErrorKind::NotFound, error)
        } else {
            error
        }
    })?;
    parse_stat(pid, &line).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unparsable /proc stat line"))
}

pub(super) fn identify(pid: u32) -> io::Result<ProcessIdentity> {
    read(pid, &format!("/proc/{pid}/stat"))
}

pub(super) fn current() -> io::Result<ProcessIdentity> {
    read(std::process::id(), "/proc/self/stat")
}

/// The kernel's boot id, trimmed; `None` when unreadable or empty.
pub(super) fn boot_key() -> Option<BootKey> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| BootKey::Linux { boot_id: id.to_string() })
}

/// The child's stat names its parent's pid only, so the parent is read
/// apart. A parent that exits reparents the child before its pid can be
/// reused, so a child that still names the same parent after the parent's
/// read had that parent all along.
pub(super) fn parent(pid: u32) -> io::Result<Option<Parent>> {
    let child = identify(pid)?;
    if child.ppid == 0 {
        return Ok(None);
    }
    let parent = identify(child.ppid)?;
    let again = identify(pid)?;
    if again.ppid != child.ppid || !again.same_instance(&child) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "the parent changed while it was read"));
    }
    Ok(Some(Parent { pid: child.ppid, start: parent.start }))
}

/// Every numeric entry of `/proc` whose stat reads.
pub(super) fn all() -> Vec<ProcessIdentity> {
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            if let Ok(process) = identify(pid) {
                out.push(process);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub(super) fn pidfd(pid: u32, start: Option<StartKey>) -> io::Result<Result<std::os::fd::OwnedFd, super::Exit>> {
    use std::os::fd::{FromRawFd, OwnedFd};

    use super::Exit;
    if let Some(exit) = ProcessIdentity::exit_of(pid, start)? {
        return Ok(Err(exit));
    }
    let pid_t =
        libc::pid_t::try_from(pid).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_t, 0) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(Err(Exit::Gone)) } else { Err(error) };
    }
    // SAFETY: a non-negative return is a new fd this process owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    // Revalidate: the pid could have been reused before the open.
    if let Some(exit) = ProcessIdentity::exit_of(pid, start)? {
        return Ok(Err(exit));
    }
    Ok(Ok(fd))
}
