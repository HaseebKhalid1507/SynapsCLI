//! Process identity: which process instance a pid names, so a reused pid
//! is never taken for the process a caller meant.
//!
//! [`ProcessIdentity`] is the one type for it, with one interface on every
//! platform. The reads behind it are per platform, inside this module only:
//! - Linux (`linux.rs`): `/proc/<pid>/stat` (state, ppid, pgid, sid, and the
//!   start time in clock ticks since boot, field 22) and
//!   `/proc/sys/kernel/random/boot_id`.
//! - macOS (`macos.rs`, the FFI only, and `darwin.rs`, what its bytes
//!   mean): `proc_pidinfo(PROC_PIDT_BSDINFOWITHUNIQID)` (state, ppid, pgid
//!   and `p_uniqueid`, which no exec changes), `getsid`,
//!   `proc_listallpids` and `sysctl kern.bootsessionuuid`. An exited,
//!   unreaped process reads as gone there, not as a zombie.
//! - Elsewhere: `Unsupported`.
//!
//! A read gives a [`StartKey`], which tells a process apart from every other
//! that held or will hold its pid in this boot. Together with the
//! [`BootKey`] it makes the [`InstanceKey`], which tells processes of
//! different boots apart too.
//!
//! The child ledger (`session::children`), the event socket's peer pinning
//! (`events::peer`), the crash record's writer, the live process key and the
//! `wait` tool's pid conditions all read processes through here. (The
//! daemon's reaper and the extension runtime still read `/proc` on their own.)
//!
//! These functions do blocking file I/O: async callers run them on the
//! blocking pool.

use std::io;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

mod darwin;
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use other as platform;

/// What tells one process apart from every other that held or will hold
/// its pid within one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartKey {
    /// Field 22 of `/proc/<pid>/stat`: the start time in clock ticks since
    /// boot.
    Linux { start_ticks: u64 },
    /// `p_uniqueid` from `proc_pidinfo` (`PROC_PIDT_BSDINFOWITHUNIQID`),
    /// which no exec changes and no other process of the boot shares. The
    /// pid version is left out: every exec bumps it.
    Darwin { unique_id: u64 },
}

impl StartKey {
    /// The key as one number: the start ticks on Linux, the unique id on
    /// macOS. Older wire and disk shapes carry it in a field named
    /// `start_ticks`, which on Linux is exactly what it held before.
    pub fn number(self) -> u64 {
        match self {
            StartKey::Linux { start_ticks } => start_ticks,
            StartKey::Darwin { unique_id } => unique_id,
        }
    }

    /// The key this platform's [`StartKey::number`] gave `number` (a Linux
    /// key where processes can't be identified, which nothing matches).
    pub fn from_number(number: u64) -> Self {
        platform::start_key(number)
    }
}

/// What tells one boot of this host from another. On the wire (the
/// `child_list` reply's `host_instance`): `{"kind":"linux","boot_id":…}` or
/// `{"kind":"darwin","boot_session":…}`.
///
/// Equality is a match, so [`BootKey::Unknown`] equals nothing, itself
/// included (hence no `Eq`, like a float).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BootKey {
    /// `/proc/sys/kernel/random/boot_id`, trimmed.
    Linux { boot_id: String },
    /// `kern.bootsessionuuid`, up to its NUL and trimmed.
    Darwin { boot_session: String },
    /// A kind this build doesn't know (from a newer image or peer), read
    /// with its other fields dropped; written back as `{"kind":"unknown"}`.
    /// It never matches anything, so whatever it names fails closed.
    #[serde(other)]
    Unknown,
}

impl PartialEq for BootKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (BootKey::Linux { boot_id: a }, BootKey::Linux { boot_id: b }) => a == b,
            (BootKey::Darwin { boot_session: a }, BootKey::Darwin { boot_session: b }) => a == b,
            _ => false,
        }
    }
}

impl BootKey {
    /// The boot's id as text, whichever platform gave it; empty for an
    /// unknown kind (a host's own key is never one).
    pub fn as_str(&self) -> &str {
        match self {
            BootKey::Linux { boot_id } => boot_id,
            BootKey::Darwin { boot_session } => boot_session,
            BootKey::Unknown => "",
        }
    }

    /// The Linux boot id, which older shapes carry as `boot_id`; `None` for
    /// any other kind.
    pub fn linux_boot_id(&self) -> Option<&str> {
        match self {
            BootKey::Linux { boot_id } => Some(boot_id),
            BootKey::Darwin { .. } | BootKey::Unknown => None,
        }
    }
}

/// One process instance on this host, across boots: a [`BootKey`] plus a
/// [`StartKey`]. Two keys name the same instance when they are of the same
/// platform and every field of it is equal; keys of different platforms
/// never do. On the wire (a child's or member's `instance`):
/// `{"kind":"linux","boot_id":…,"start_ticks":…}` or
/// `{"kind":"darwin","boot_session":…,"unique_id":…}`.
///
/// Equality is that match, so [`InstanceKey::Unknown`] equals nothing,
/// itself included (hence no `Eq`, like a float).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstanceKey {
    Linux { boot_id: String, start_ticks: u64 },
    Darwin { boot_session: String, unique_id: u64 },
    /// A kind this build doesn't know (from a newer image or peer), read
    /// with its other fields dropped; written back as `{"kind":"unknown"}`.
    /// It never matches anything, so whatever it names fails closed.
    #[serde(other)]
    Unknown,
}

impl PartialEq for InstanceKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                InstanceKey::Linux { boot_id: a, start_ticks: s },
                InstanceKey::Linux { boot_id: b, start_ticks: t },
            ) => a == b && s == t,
            (
                InstanceKey::Darwin { boot_session: a, unique_id: s },
                InstanceKey::Darwin { boot_session: b, unique_id: t },
            ) => a == b && s == t,
            _ => false,
        }
    }
}

impl InstanceKey {
    /// The key of the process started at `start` in the boot `boot`; `None`
    /// when they come from different platforms (or the boot's kind is
    /// unknown).
    pub fn new(boot: &BootKey, start: StartKey) -> Option<Self> {
        match (boot, start) {
            (BootKey::Linux { boot_id }, StartKey::Linux { start_ticks }) => {
                Some(InstanceKey::Linux { boot_id: boot_id.clone(), start_ticks })
            }
            (BootKey::Darwin { boot_session }, StartKey::Darwin { unique_id }) => {
                Some(InstanceKey::Darwin { boot_session: boot_session.clone(), unique_id })
            }
            _ => None,
        }
    }

    /// The boot part ([`BootKey::Unknown`] for an unknown kind).
    pub fn boot(&self) -> BootKey {
        match self {
            InstanceKey::Linux { boot_id, .. } => BootKey::Linux { boot_id: boot_id.clone() },
            InstanceKey::Darwin { boot_session, .. } => BootKey::Darwin { boot_session: boot_session.clone() },
            InstanceKey::Unknown => BootKey::Unknown,
        }
    }

    /// The part within the boot; `None` for an unknown kind, so it matches
    /// no process.
    pub fn start(&self) -> Option<StartKey> {
        match self {
            InstanceKey::Linux { start_ticks, .. } => Some(StartKey::Linux { start_ticks: *start_ticks }),
            InstanceKey::Darwin { unique_id, .. } => Some(StartKey::Darwin { unique_id: *unique_id }),
            InstanceKey::Unknown => None,
        }
    }

    /// The Linux fields older shapes carry beside the instance:
    /// `(start_ticks, boot_id)`; `None` for any other kind.
    pub fn linux_fields(&self) -> Option<(u64, &str)> {
        match self {
            InstanceKey::Linux { boot_id, start_ticks } => Some((*start_ticks, boot_id)),
            InstanceKey::Darwin { .. } | InstanceKey::Unknown => None,
        }
    }
}

/// One read of one process: its pid, what tells it apart from other
/// processes under that pid, and its family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// The ps(1) state letter: `R`, `S`, `D`, `Z` (zombie), `X` (dead), ….
    /// On macOS one of `R` (runnable or being created), `S`, `T`, `Z`.
    pub state: char,
    /// The parent's pid; 0 for a process with none.
    pub ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub start: StartKey,
}

/// A process's parent, as [`ProcessIdentity::parent`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parent {
    pub pid: u32,
    pub start: StartKey,
}

impl ProcessIdentity {
    /// Whether this platform identifies processes: the reads work here and
    /// the boot key is known.
    pub fn supported() -> bool {
        platform::supported() && Self::boot_key().is_some()
    }

    /// This boot's key; read once per process.
    pub fn boot_key() -> Option<&'static BootKey> {
        static BOOT: OnceLock<Option<BootKey>> = OnceLock::new();
        BOOT.get_or_init(platform::boot_key).as_ref()
    }

    /// The process at `pid` now (on Linux a zombie included; on macOS an
    /// exited, unreaped process reads as no such pid). `NotFound` when there
    /// is no such pid, `Unsupported` where processes can't be identified.
    pub fn identify(pid: u32) -> io::Result<Self> {
        platform::identify(pid)
    }

    /// This process.
    pub fn current() -> io::Result<Self> {
        platform::current()
    }

    /// The parent of the process at `pid`, with its start key; `None` for a
    /// process with no parent. The parent is the one the child had at a
    /// single instant: a parent that exits while it is read is never taken
    /// for whatever reuses its pid (that reads as `Interrupted` on Linux).
    /// On macOS the process at the ppid is taken only when it is the one the
    /// child was started by, else `InvalidData`: an orphan (adopted by
    /// launchd) fails so too.
    pub fn parent(pid: u32) -> io::Result<Option<Parent>> {
        platform::parent(pid)
    }

    /// Every process now, in one walk; those that can't be read are left
    /// out. Empty where processes can't be identified.
    pub fn all() -> Vec<Self> {
        platform::all()
    }

    /// Has the instance `(pid, start)` exited? `None` while it runs. Without
    /// `start` any process under the pid counts. Errors other than "no such
    /// pid" (permission, an unreadable entry) are errors, never taken as an
    /// exit.
    pub fn exit_of(pid: u32, start: Option<StartKey>) -> io::Result<Option<Exit>> {
        match Self::identify(pid) {
            Ok(now) if start.is_some_and(|start| start != now.start) => Ok(Some(Exit::Reused)),
            Ok(now) if now.exited() => Ok(Some(Exit::Zombie)),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(Exit::Gone)),
            Err(error) => Err(error),
        }
    }

    /// A pidfd for the instance `(pid, start)`, readable once it exits. The
    /// identity is checked before AND after the open, so the fd never
    /// refers to a process that reused the pid in between. `Err(exit)` when
    /// the instance already exited (a zombie at entry included: its pidfd
    /// would be readable at once anyway).
    #[cfg(target_os = "linux")]
    pub fn pidfd(pid: u32, start: Option<StartKey>) -> io::Result<Result<std::os::fd::OwnedFd, Exit>> {
        linux::pidfd(pid, start)
    }

    /// Clock ticks per second (`sysconf(_SC_CLK_TCK)`), the unit of Linux
    /// start times; for display only.
    pub fn clk_tck() -> u64 {
        #[cfg(unix)]
        {
            // SAFETY: sysconf has no preconditions.
            let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if ticks > 0 {
                return ticks as u64;
            }
        }
        100
    }

    /// Exited but not reaped (`Z`), or dead (`X`/`x`).
    pub fn exited(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x')
    }

    /// Not exited: a zombie runs nothing and counts as gone.
    pub fn is_live(&self) -> bool {
        !self.exited()
    }

    /// The same process instance as `other` (both read in this boot).
    pub fn same_instance(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start == other.start
    }

    /// The Linux start time (field 22 of `/proc/<pid>/stat`), which the
    /// existing wire shapes carry; `None` elsewhere.
    pub fn start_ticks(&self) -> Option<u64> {
        match self.start {
            StartKey::Linux { start_ticks } => Some(start_ticks),
            StartKey::Darwin { .. } => None,
        }
    }

    /// This process's key across boots; `None` without a boot key.
    pub fn instance(&self) -> Option<InstanceKey> {
        InstanceKey::new(Self::boot_key()?, self.start)
    }
}

/// The answer where processes can't be identified.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "process identity needs Linux or macOS")
}

/// Neither Linux nor macOS: nothing can be identified.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod other {
    use std::io;

    use super::{BootKey, Parent, ProcessIdentity, StartKey};

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn boot_key() -> Option<BootKey> {
        None
    }

    pub(super) fn start_key(number: u64) -> StartKey {
        StartKey::Linux { start_ticks: number }
    }

    pub(super) fn identify(_pid: u32) -> io::Result<ProcessIdentity> {
        Err(super::unsupported())
    }

    pub(super) fn current() -> io::Result<ProcessIdentity> {
        Err(super::unsupported())
    }

    pub(super) fn parent(_pid: u32) -> io::Result<Option<Parent>> {
        Err(super::unsupported())
    }

    pub(super) fn all() -> Vec<ProcessIdentity> {
        Vec::new()
    }
}

/// Why a watched process instance is no longer running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// No such pid.
    Gone,
    /// The pid's process exited and is not reaped yet.
    Zombie,
    /// The pid now names a different process (another start time).
    Reused,
}

impl Exit {
    pub fn as_str(self) -> &'static str {
        match self {
            Exit::Gone => "gone",
            Exit::Zombie => "zombie",
            Exit::Reused => "reused",
        }
    }
}

#[cfg(test)]
mod tests;
