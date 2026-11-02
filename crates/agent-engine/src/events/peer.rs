//! The process on the other end of an event connection, and the session the
//! receiver can match it to, if any.
//!
//! The receiving socket server is the only writer of [`Event::peer`](super::types::Event):
//! whatever a client puts there is replaced. `verified` means only that the
//! receiver matched the connecting process to a session. It does not
//! authenticate the content and grants no authority. The `name` is the
//! session card's name when the event was received and attests nothing
//! (sessions can be renamed); anything that checks who sent an event uses
//! `session_id` only.
//!
//! A verdict takes three steps:
//! 1. [`Credentials`] and [`Pidfd`]: what the kernel reports for the
//!    connection when it is accepted.
//! 2. [`pin`] and [`gather`]: the connecting process's start time and
//!    parents from `/proc`, the session cards, and the resolver's answer.
//! 3. [`decide`]: the verdict, once the event has been read. It reads
//!    nothing itself.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::registry::{CardScan, SessionRegistration};
use super::types::EventSender;
use crate::proc_identity::ProcessIdentity;

/// The most parents the lineage walk reads.
pub const MAX_ANCESTORS: usize = 16;

/// One parent of the connecting process: its pid and its start time
/// (field 22 of `/proc/<pid>/stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: u32,
    pub start_ticks: u64,
}

/// What the receiver established about the connecting process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lineage {
    /// Its start time and its parents (parent first, at most
    /// [`MAX_ANCESTORS`]), each with its own start time.
    Pinned {
        start_ticks: u64,
        ancestors: Vec<Ancestor>,
    },
    /// Not pinned; why.
    Unpinned(Unpinned),
}

impl Default for Lineage {
    fn default() -> Self {
        Lineage::Unpinned(Unpinned::Unsupported)
    }
}

/// Why the connecting process is not pinned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Unpinned {
    /// It had exited.
    Gone,
    /// Its credentials or its `/proc` entry could not be read.
    Unreadable,
    /// The kernel gives no pidfd for socket peers (SO_PEERPIDFD needs Linux 6.5).
    NoPidfd,
    /// It or its parents changed while they were read.
    Changed,
    /// The identity step ran out of time.
    Timeout,
    /// Too many identity steps were already running.
    Busy,
    /// The platform reports no pid, or has no way to pin one.
    #[default]
    Unsupported,
}

impl Unpinned {
    fn reason(self) -> PeerReason {
        match self {
            Unpinned::Gone => PeerReason::PeerGone,
            Unpinned::Unreadable => PeerReason::Unreadable,
            Unpinned::NoPidfd => PeerReason::NoPidfd,
            Unpinned::Changed => PeerReason::EvidenceStale,
            Unpinned::Timeout => PeerReason::Timeout,
            Unpinned::Busy => PeerReason::Busy,
            Unpinned::Unsupported => PeerReason::Unsupported,
        }
    }
}

/// The connecting process, as the receiver could establish it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peer {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub lineage: Lineage,
}

impl Peer {
    /// Its start time, if it is pinned: what a resolver pins it by.
    pub fn start_ticks(&self) -> Option<u64> {
        match &self.lineage {
            Lineage::Pinned { start_ticks, .. } => Some(*start_ticks),
            Lineage::Unpinned(_) => None,
        }
    }

    /// Its parents, parent first; none unless it is pinned.
    pub fn ancestors(&self) -> &[Ancestor] {
        match &self.lineage {
            Lineage::Pinned { ancestors, .. } => ancestors,
            Lineage::Unpinned(_) => &[],
        }
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self.lineage, Lineage::Pinned { .. })
    }

    fn unpinned(creds: Credentials, why: Unpinned) -> Self {
        Peer {
            pid: creds.pid.ok(),
            uid: creds.uid,
            lineage: Lineage::Unpinned(why),
        }
    }
}

/// What the kernel reports for a connection when it is accepted
/// (SO_PEERCRED on Linux, getpeereid and LOCAL_PEERPID on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    pub uid: Option<u32>,
    /// The connecting process's pid, or why there is none.
    pub pid: Result<u32, Unpinned>,
}

impl Default for Credentials {
    fn default() -> Self {
        Credentials {
            uid: None,
            pid: Err(Unpinned::Unsupported),
        }
    }
}

impl Credentials {
    #[cfg(unix)]
    pub fn of_unix_stream(stream: &tokio::net::UnixStream) -> Self {
        match stream.peer_cred() {
            Ok(cred) => Self::from_parts(cred.pid(), Some(cred.uid())),
            Err(_) => Credentials {
                uid: None,
                pid: Err(Unpinned::Unreadable),
            },
        }
    }

    pub fn from_parts(pid: Option<i32>, uid: Option<u32>) -> Self {
        let pid = match pid {
            // The platform gives no pid.
            None => Err(Unpinned::Unsupported),
            // 0: not visible from this pid namespace.
            Some(raw) => u32::try_from(raw)
                .ok()
                .filter(|pid| *pid != 0)
                .ok_or(Unpinned::Unreadable),
        };
        Credentials { uid, pid }
    }
}

/// The kernel's pidfd for the connecting process (SO_PEERPIDFD). It refers
/// to that process for as long as it is held, whatever later holds its pid.
#[derive(Debug, Default)]
pub enum Pidfd {
    #[cfg(unix)]
    Held(std::os::fd::OwnedFd),
    /// The process was gone when the pidfd was asked for.
    Gone,
    /// The kernel doesn't offer SO_PEERPIDFD (before Linux 6.5).
    NotSupported,
    /// The kernel refused for another reason.
    Failed,
    /// The platform has none.
    #[default]
    Absent,
}

impl Pidfd {
    /// The pidfd for the process at the other end of a connected socket.
    #[cfg(target_os = "linux")]
    pub fn of_socket(socket: std::os::fd::BorrowedFd<'_>) -> Self {
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut pidfd: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `pidfd` and `len` are valid for writes of the sizes given,
        // and `socket` is an open descriptor for the duration of the call.
        let rc = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut pidfd as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOPROTOOPT) => Pidfd::NotSupported,
                Some(libc::EINVAL | libc::ESRCH) => Pidfd::Gone,
                _ => Pidfd::Failed,
            };
        }
        if pidfd < 0 {
            return Pidfd::Failed;
        }
        // SAFETY: the kernel just created this descriptor (close-on-exec)
        // for this call, and nothing else owns it.
        Pidfd::Held(unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) })
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    pub fn of_socket(_socket: std::os::fd::BorrowedFd<'_>) -> Self {
        Pidfd::Absent
    }
}

/// How the identity step reads `/proc`; a seam, so tests can script, stall
/// or count the reads.
pub trait ProcReader: Send + Sync {
    /// `(ppid, start)` of `pid`, the start as [`StartKey::number`] gives it
    /// (on Linux the start ticks, fields 4 and 22 of `/proc/<pid>/stat`).
    ///
    /// [`StartKey::number`]: crate::proc_identity::StartKey::number
    fn stat(&self, pid: u32) -> Option<(u32, u64)>;
    /// The pid the process behind the open pidfd `fd` holds now, as
    /// `/proc/self/fdinfo/<fd>` reports it: `-1` once it has exited.
    fn pidfd_pid(&self, fd: i32) -> Option<i64>;
}

/// The real process table, read through [`ProcessIdentity`] (`/proc` on
/// Linux); elsewhere it reads nothing.
pub struct SystemProc;

impl ProcReader for SystemProc {
    fn stat(&self, pid: u32) -> Option<(u32, u64)> {
        let process = ProcessIdentity::identify(pid).ok()?;
        Some((process.ppid, process.start.number()))
    }

    fn pidfd_pid(&self, fd: i32) -> Option<i64> {
        #[cfg(target_os = "linux")]
        {
            parse_fdinfo_pid(&std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = fd;
            None
        }
    }
}

/// This process's start, which its session cards record: the number of its
/// [`StartKey`](crate::proc_identity::StartKey) (on Linux the start ticks);
/// `None` where processes can't be identified.
pub fn own_start_ticks() -> Option<u64> {
    Some(ProcessIdentity::current().ok()?.start.number())
}

/// The `Pid:` line of a pidfd's fdinfo.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_fdinfo_pid(fdinfo: &str) -> Option<i64> {
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .and_then(|value| value.trim().parse().ok())
}

/// One read of the connecting process and its parents.
#[derive(Debug, PartialEq, Eq)]
struct Walk {
    start_ticks: u64,
    ppid: u32,
    ancestors: Vec<Ancestor>,
}

/// The process's start time and its readable parents; None if its own
/// stat can't be read.
fn walk(pid: u32, proc: &dyn ProcReader) -> Option<Walk> {
    let (ppid, start_ticks) = proc.stat(pid)?;
    let mut ancestors = Vec::new();
    let mut next = ppid;
    while next != 0 && ancestors.len() < MAX_ANCESTORS {
        let Some((parent_ppid, parent_start)) = proc.stat(next) else {
            break;
        };
        ancestors.push(Ancestor {
            pid: next,
            start_ticks: parent_start,
        });
        next = parent_ppid;
    }
    Some(Walk {
        start_ticks,
        ppid,
        ancestors,
    })
}

/// Pin the connecting process: its start time and parents from `/proc`,
/// tied to the process itself through its pidfd.
///
/// 1. The pidfd: none from the kernel, or one for a process already gone,
///    means it can't be pinned.
/// 2. Two walks of the process and its parents. A parent that exits and
///    has its pid reused during a walk shows up as a difference between
///    them: equal (start time, parent) on both walks means every link held
///    at one instant between them. A difference reads as `Changed`.
/// 3. After the walks, the pidfd must still show the process at `pid`:
///    `-1` means it exited, so what was read may be another process that
///    reused the pid (`Gone`).
#[cfg(unix)]
pub fn pin(creds: Credentials, pidfd: &Pidfd, proc: &dyn ProcReader) -> Peer {
    use std::os::fd::AsRawFd;
    let pid = match creds.pid {
        Ok(pid) => pid,
        Err(why) => return Peer::unpinned(creds, why),
    };
    let fd = match pidfd {
        Pidfd::Held(fd) => fd.as_raw_fd(),
        Pidfd::Gone => return Peer::unpinned(creds, Unpinned::Gone),
        Pidfd::NotSupported => return Peer::unpinned(creds, Unpinned::NoPidfd),
        Pidfd::Failed => return Peer::unpinned(creds, Unpinned::Unreadable),
        Pidfd::Absent => return Peer::unpinned(creds, Unpinned::Unsupported),
    };
    let first = walk(pid, proc);
    let second = first.as_ref().and_then(|_| walk(pid, proc));
    match proc.pidfd_pid(fd) {
        Some(holder) if holder == i64::from(pid) => {}
        Some(-1) => return Peer::unpinned(creds, Unpinned::Gone),
        _ => return Peer::unpinned(creds, Unpinned::Unreadable),
    }
    match (first, second) {
        (None, _) => Peer::unpinned(creds, Unpinned::Unreadable),
        (Some(first), Some(second)) if first == second => Peer {
            pid: Some(pid),
            uid: creds.uid,
            lineage: Lineage::Pinned {
                start_ticks: first.start_ticks,
                ancestors: first.ancestors,
            },
        },
        (Some(_), _) => Peer::unpinned(creds, Unpinned::Changed),
    }
}

/// No platform without Unix sockets can pin a process.
#[cfg(not(unix))]
pub fn pin(creds: Credentials, _pidfd: &Pidfd, _proc: &dyn ProcReader) -> Peer {
    Peer::unpinned(creds, creds.pid.err().unwrap_or(Unpinned::Unsupported))
}

/// The receiving process's effective uid, if the platform has one.
pub fn own_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// A peer running as another user, root included. A uid that can't be
/// read (Windows) is not foreign: it's marked `unsupported` instead.
pub fn is_foreign(uid: Option<u32>, own_uid: Option<u32>) -> bool {
    matches!((uid, own_uid), (Some(theirs), Some(ours)) if theirs != ours)
}

/// How the receiver matched the connecting process to a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerVia {
    /// The sender runs in the receiving process and names a session served there.
    InProcess,
    /// The installed [`PeerResolver`] named the session.
    Resolver,
    /// The connecting process is the session's own process.
    Process,
    /// Not matched; see the reason.
    #[default]
    #[serde(rename = "none")]
    Unverified,
    /// A value this build doesn't know (read only; never written).
    #[serde(other)]
    Unknown,
}

/// Why the receiver did not match the connecting process to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerReason {
    /// In the receiving process, with no sender named.
    NoClaim,
    /// In the receiving process, naming a sender that is no session served there.
    UnknownSender,
    /// The connecting process serves more than one session.
    SharedHost,
    /// The connecting process descends from a session's process (a tool
    /// child). Only a resolver can tell whose call it is.
    ToolChild,
    /// The connecting process is no session's.
    NoCard,
    /// The connecting process had exited.
    PeerGone,
    /// This platform doesn't report the connecting process, or can't pin it.
    Unsupported,
    /// A card that would match was written without a start time (by an
    /// older build), so it can't be tied to a process.
    LegacyCard,
    /// The connecting process's credentials or `/proc` entry couldn't be read.
    Unreadable,
    /// The kernel gives no pidfd for the connection (Linux before 6.5).
    NoPidfd,
    /// The identity step didn't finish in time.
    Timeout,
    /// Too many identity steps were already running.
    Busy,
    /// The card scan didn't see every card (more than it reads, or a card
    /// file couldn't be read), so the process can't be matched to exactly
    /// one session.
    ScanIncomplete,
    // Reasons a resolver gives (see `PeerResolver`):
    /// The process left the call's session.
    LeftTheCall,
    /// No record of a tool call matches the process.
    NoLedgerRecord,
    /// The record of tool calls was truncated.
    LedgerTruncated,
    /// The record of tool calls was full.
    LedgerFull,
    /// More than one recorded call matches the process.
    AmbiguousCaller,
    /// The matching call has ended.
    CallEnded,
    /// The matching call is not a shell call.
    NotABashCall,
    /// The matching call was made by a subagent.
    SubagentCall,
    /// The processes changed while they were checked.
    EvidenceStale,
    /// A reason this build doesn't know (read only; never written).
    #[serde(other)]
    Unknown,
}

/// The receiver's verdict on who sent an event. Built only by the socket
/// server, through [`decide`].
///
/// Wire shape: `{"verified":true,"session_id":"…","name":"…","via":"process"}`
/// or `{"verified":false,"session_id":null,"name":null,"via":"none","reason":"no_card"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPeer {
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    via: PeerVia,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<PeerReason>,
}

impl EventPeer {
    fn verified(card: &SessionRegistration, via: PeerVia) -> Self {
        EventPeer {
            verified: true,
            session_id: Some(card.session_id.clone()),
            name: card.name.clone(),
            via,
            reason: None,
        }
    }

    pub fn unverified(reason: PeerReason) -> Self {
        EventPeer {
            verified: false,
            session_id: None,
            name: None,
            via: PeerVia::Unverified,
            reason: Some(reason),
        }
    }

    /// True only when the receiver matched the sender to a session: a
    /// decoded peer that says `verified` without a session id (or with
    /// `via: none`) reads as unverified.
    pub fn is_verified(&self) -> bool {
        self.verified
            && self.via != PeerVia::Unverified
            && self.session_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    /// The verified sender's session id.
    pub fn session_id(&self) -> Option<&str> {
        self.is_verified()
            .then_some(self.session_id.as_deref())
            .flatten()
    }

    /// The verified sender's card name at receive time. It attests nothing.
    pub fn name(&self) -> Option<&str> {
        self.is_verified().then_some(self.name.as_deref()).flatten()
    }

    pub fn via(&self) -> PeerVia {
        self.via
    }

    pub fn reason(&self) -> Option<PeerReason> {
        self.reason
    }
}

/// A resolver's answer for one connecting process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The process acts for this session (it must still be a live card).
    Session(String),
    /// The process is not to be verified; final, the generic rules don't run.
    Refused(PeerReason),
    /// No answer; the generic rules decide.
    Pass,
}

/// Maps a connecting process to a session where the generic rules can't,
/// for example from a host's own record of the tool calls it runs. It runs
/// in the identity step, off the listener's tasks, so it may read files,
/// but it must answer quickly: the step has one deadline.
pub trait PeerResolver: Send + Sync {
    fn resolve(&self, peer: &Peer) -> Resolution;
}

static RESOLVER: OnceLock<Box<dyn PeerResolver>> = OnceLock::new();

/// Install the process's resolver. Once only; none is installed by default.
pub fn set_peer_resolver(resolver: Box<dyn PeerResolver>) -> Result<(), String> {
    RESOLVER
        .set(resolver)
        .map_err(|_| "a peer resolver is already installed".to_string())
}

pub(crate) fn installed_resolver() -> Option<&'static dyn PeerResolver> {
    RESOLVER.get().map(|resolver| resolver.as_ref())
}

/// A resolver's answer, with the start time read for the process of the
/// session it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverAnswer {
    pub resolution: Resolution,
    pub host_start_ticks: Option<u64>,
}

/// What the identity step gathered for one connection.
#[derive(Debug, Clone)]
pub struct Gathered {
    pub peer: Peer,
    pub cards: Vec<SessionRegistration>,
    /// Whether the card scan saw every card. If not, a card it never saw may
    /// share the process, so a match by process alone isn't trusted.
    pub cards_complete: bool,
    pub answer: Option<ResolverAnswer>,
}

impl Gathered {
    /// Nothing gathered: the process is not pinned, and `why`.
    pub fn unpinned(creds: Credentials, why: Unpinned) -> Self {
        Gathered {
            peer: Peer::unpinned(creds, why),
            cards: Vec::new(),
            cards_complete: false,
            answer: None,
        }
    }
}

/// Everything a verdict needs that reads the system, besides the pinning:
/// the card scan (read by the caller) and the resolver's answer. The
/// resolver is asked only about a pinned process that isn't this one.
pub(crate) fn gather(
    peer: Peer,
    own_pid: u32,
    scan: CardScan,
    resolver: Option<&dyn PeerResolver>,
    proc: &dyn ProcReader,
) -> Gathered {
    let CardScan { cards, complete } = scan;
    let answer = match (peer.pid, resolver) {
        (Some(pid), Some(resolver)) if pid != own_pid && peer.is_pinned() => {
            let resolution = resolver.resolve(&peer);
            let host_start_ticks = match &resolution {
                Resolution::Session(id) => cards
                    .iter()
                    .find(|card| &card.session_id == id)
                    .and_then(|card| proc.stat(card.pid))
                    .map(|(_, start)| start),
                _ => None,
            };
            Some(ResolverAnswer {
                resolution,
                host_start_ticks,
            })
        }
        _ => None,
    };
    Gathered {
        peer,
        cards,
        cards_complete: complete,
        answer,
    }
}

/// The verdict on a connecting process. It reads nothing itself.
///
/// A process that isn't pinned is unverified with its reason, before any
/// rule below. A card matches a process only with the start time read for
/// it; a card without one (an older build) never verifies. Then the first
/// rule that answers wins:
/// 1. In the receiving process (`own_pid`): verified only if `sender` names
///    a card this process wrote.
/// 2. The resolver: `Session` (if its card carries the start time read now
///    for that card's process), or `Refused`, which is final.
///
/// Rules 1 and 2 find a card by its session id. The rules below match by
/// process, which needs every card: after a scan that didn't see them all
/// (`cards_complete` false) the verdict is unverified (`scan_incomplete`),
/// since an unseen card could share the process.
///
/// 3. The process of exactly one card. A process serving several sessions
///    can't say which one sent.
/// 4. Otherwise unverified: a descendant of a card's process (`tool_child`),
///    or no card's.
pub fn decide(gathered: &Gathered, own_pid: u32, sender: Option<&EventSender>) -> EventPeer {
    let Gathered {
        peer,
        cards,
        cards_complete,
        answer,
    } = gathered;
    let (pid, start) = match (&peer.lineage, peer.pid) {
        (Lineage::Pinned { start_ticks, .. }, Some(pid)) => (pid, *start_ticks),
        (Lineage::Unpinned(why), _) => return EventPeer::unverified(why.reason()),
        (Lineage::Pinned { .. }, None) => return EventPeer::unverified(PeerReason::Unsupported),
    };

    if pid == own_pid {
        let Some(sender) = sender else {
            return EventPeer::unverified(PeerReason::NoClaim);
        };
        return match cards
            .iter()
            .find(|card| card.pid == own_pid && card.session_id == sender.id)
        {
            Some(card) if card.start_ticks == Some(start) => {
                EventPeer::verified(card, PeerVia::InProcess)
            }
            Some(card) if card.start_ticks.is_none() => {
                EventPeer::unverified(PeerReason::LegacyCard)
            }
            _ => EventPeer::unverified(PeerReason::UnknownSender),
        };
    }

    if let Some(answer) = answer {
        match &answer.resolution {
            Resolution::Session(id) => {
                return match cards.iter().find(|card| &card.session_id == id) {
                    Some(card) if card.start_ticks.is_none() => {
                        EventPeer::unverified(PeerReason::LegacyCard)
                    }
                    Some(card)
                        if answer.host_start_ticks.is_some()
                            && card.start_ticks == answer.host_start_ticks =>
                    {
                        EventPeer::verified(card, PeerVia::Resolver)
                    }
                    _ => EventPeer::unverified(PeerReason::NoCard),
                };
            }
            Resolution::Refused(reason) => return EventPeer::unverified(*reason),
            Resolution::Pass => {}
        }
    }

    if !*cards_complete {
        return EventPeer::unverified(PeerReason::ScanIncomplete);
    }

    let on_pid: Vec<&SessionRegistration> =
        cards.iter().filter(|card| card.pid == pid).collect();
    if on_pid.iter().any(|card| card.start_ticks.is_none()) {
        return EventPeer::unverified(PeerReason::LegacyCard);
    }
    let mut ours = on_pid
        .iter()
        .filter(|card| card.start_ticks == Some(start));
    match (ours.next(), ours.next()) {
        (Some(card), None) => return EventPeer::verified(card, PeerVia::Process),
        (Some(_), Some(_)) => return EventPeer::unverified(PeerReason::SharedHost),
        _ => {}
    }

    let tool_child = peer.ancestors().iter().any(|parent| {
        cards
            .iter()
            .any(|card| card.pid == parent.pid && card.start_ticks == Some(parent.start_ticks))
    });
    EventPeer::unverified(if tool_child {
        PeerReason::ToolChild
    } else {
        PeerReason::NoCard
    })
}

/// Whether this kernel gives pidfds for socket peers (Linux 6.5+), asked
/// directly rather than through the code under test, so tests can say
/// what a real connection should read as here.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn kernel_gives_peer_pidfds() -> bool {
    use std::os::fd::AsRawFd;
    let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid pointers for the sizes given; `a` is open.
    let rc = unsafe {
        libc::getsockopt(
            a.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc == 0 && fd >= 0 {
        // SAFETY: the kernel just gave us this descriptor.
        unsafe { libc::close(fd) };
    }
    rc == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const OWN: u32 = 4242;
    /// The start time of every process in these tests, unless a test says otherwise.
    const START: u64 = 7;

    fn card(id: &str, pid: u32, name: Option<&str>) -> SessionRegistration {
        SessionRegistration {
            kind: "session".into(),
            session_id: id.into(),
            name: name.map(Into::into),
            socket_path: format!("/unused/{id}.sock"),
            pid,
            started_at: Utc::now(),
            receipts: 1,
            start_ticks: Some(START),
        }
    }

    /// A card whose process started at `start` (`None`: an older build's card).
    fn card_started(id: &str, pid: u32, start: Option<u64>) -> SessionRegistration {
        SessionRegistration {
            start_ticks: start,
            ..card(id, pid, None)
        }
    }

    /// A pinned peer that started at [`START`], with these parents (parent
    /// first), each also started at [`START`].
    fn peer(pid: u32, ancestors: &[u32]) -> Peer {
        Peer {
            pid: Some(pid),
            uid: Some(1000),
            lineage: Lineage::Pinned {
                start_ticks: START,
                ancestors: ancestors
                    .iter()
                    .map(|&pid| Ancestor {
                        pid,
                        start_ticks: START,
                    })
                    .collect(),
            },
        }
    }

    fn unpinned(pid: Option<u32>, why: Unpinned) -> Peer {
        Peer {
            pid,
            uid: Some(1000),
            lineage: Lineage::Unpinned(why),
        }
    }

    fn sender(id: &str) -> EventSender {
        EventSender {
            id: id.into(),
            name: "claimed".into(),
            sender_type: "session".into(),
        }
    }

    /// A `/proc` where every process started at [`START`], with parent 1.
    struct Steady;
    impl ProcReader for Steady {
        fn stat(&self, pid: u32) -> Option<(u32, u64)> {
            Some((u32::from(pid != 1), START))
        }
        fn pidfd_pid(&self, _fd: i32) -> Option<i64> {
            None
        }
    }

    /// Each pid's scripted stat answers, in the order they are given.
    type StatAnswers = HashMap<u32, VecDeque<Option<(u32, u64)>>>;

    /// A scripted `/proc`: each pid's stat gives its answers in turn (the
    /// last one repeats; a pid not listed can't be read), and the pidfd
    /// shows `holder`.
    struct Script {
        stats: Mutex<StatAnswers>,
        holder: Option<i64>,
    }

    impl Script {
        fn holder(holder: Option<i64>) -> Self {
            Script {
                stats: Mutex::new(HashMap::new()),
                holder,
            }
        }

        fn answers(self, pid: u32, answers: &[Option<(u32, u64)>]) -> Self {
            self.stats
                .lock()
                .unwrap()
                .insert(pid, answers.iter().copied().collect());
            self
        }
    }

    impl ProcReader for Script {
        fn stat(&self, pid: u32) -> Option<(u32, u64)> {
            let mut stats = self.stats.lock().unwrap();
            let answers = stats.get_mut(&pid)?;
            if answers.len() > 1 {
                answers.pop_front().flatten()
            } else {
                answers.front().copied().flatten()
            }
        }
        fn pidfd_pid(&self, _fd: i32) -> Option<i64> {
            self.holder
        }
    }

    struct Answer(Resolution);
    impl PeerResolver for Answer {
        fn resolve(&self, _peer: &Peer) -> Resolution {
            self.0.clone()
        }
    }

    /// Counts how often it is asked.
    struct Counting(Answer, AtomicUsize);
    impl PeerResolver for Counting {
        fn resolve(&self, peer: &Peer) -> Resolution {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.resolve(peer)
        }
    }

    /// The verdict for an already pinned (or unpinned) `peer`, against
    /// `cards`, on a `/proc` where every process started at [`START`].
    fn verdict(
        peer: &Peer,
        sender: Option<&EventSender>,
        cards: &[SessionRegistration],
        resolver: Option<&dyn PeerResolver>,
    ) -> EventPeer {
        verdict_on(peer, sender, cards, resolver, &Steady)
    }

    fn verdict_on(
        peer: &Peer,
        sender: Option<&EventSender>,
        cards: &[SessionRegistration],
        resolver: Option<&dyn PeerResolver>,
        proc: &dyn ProcReader,
    ) -> EventPeer {
        verdict_scanned(
            peer,
            sender,
            CardScan::whole(cards.to_vec()),
            resolver,
            proc,
        )
    }

    /// The verdict after a card scan that may not have seen every card.
    fn verdict_scanned(
        peer: &Peer,
        sender: Option<&EventSender>,
        scan: CardScan,
        resolver: Option<&dyn PeerResolver>,
        proc: &dyn ProcReader,
    ) -> EventPeer {
        let gathered = gather(peer.clone(), OWN, scan, resolver, proc);
        decide(&gathered, OWN, sender)
    }

    /// A scan that stopped before the end: it saw `cards` and missed others.
    fn partial(cards: &[SessionRegistration]) -> CardScan {
        CardScan {
            cards: cards.to_vec(),
            complete: false,
        }
    }

    /// A descriptor standing in for a pidfd; the scripted `/proc` decides
    /// what it shows.
    #[cfg(unix)]
    fn held() -> Pidfd {
        Pidfd::Held(std::fs::File::open("/dev/null").unwrap().into())
    }

    /// The whole path for a connection from `pid`: pin it on `proc`, then
    /// the verdict against `cards`.
    #[cfg(unix)]
    fn connection(
        pid: u32,
        pidfd: Pidfd,
        proc: &dyn ProcReader,
        cards: &[SessionRegistration],
    ) -> EventPeer {
        let creds = Credentials {
            uid: Some(1000),
            pid: Ok(pid),
        };
        let peer = pin(creds, &pidfd, proc);
        decide(
            &gather(peer, OWN, CardScan::whole(cards.to_vec()), None, proc),
            OWN,
            None,
        )
    }

    fn unverified(reason: PeerReason) -> EventPeer {
        EventPeer::unverified(reason)
    }

    // P1: the session's own process is that session.
    #[test]
    fn a_session_process_is_verified_as_that_session() {
        let cards = [card("s-a", 100, Some("alpha")), card("s-b", 200, None)];
        let v = verdict(&peer(100, &[1]), None, &cards, None);
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.session_id(), Some("s-a"));
        assert_eq!(v.name(), Some("alpha"));
        assert_eq!(v.via(), PeerVia::Process);
        assert_eq!(v.reason(), None);
    }

    // P1: a process started by a session (a tool's shell) is not the session.
    #[test]
    fn a_child_of_a_session_process_is_a_tool_child() {
        let cards = [card("s-a", 100, Some("alpha"))];
        let v = verdict(&peer(200, &[100, 1]), Some(&sender("s-a")), &cards, None);
        assert_eq!(v, unverified(PeerReason::ToolChild));
    }

    // S4: nor is anything further down.
    #[test]
    fn a_grandchild_of_a_session_process_is_a_tool_child() {
        let cards = [card("s-a", 100, None)];
        let v = verdict(&peer(300, &[200, 100, 1]), None, &cards, None);
        assert_eq!(v, unverified(PeerReason::ToolChild));
    }

    // P2: a process serving several sessions can't say which one sent.
    #[test]
    fn a_process_serving_two_sessions_verifies_neither() {
        let cards = [card("s-a", 100, None), card("s-b", 100, None)];
        let v = verdict(&peer(100, &[1]), Some(&sender("s-a")), &cards, None);
        assert_eq!(v, unverified(PeerReason::SharedHost));
    }

    #[test]
    fn a_process_that_is_no_sessions_has_no_card() {
        let cards = [card("s-a", 100, None)];
        let v = verdict(&peer(300, &[1]), Some(&sender("s-a")), &cards, None);
        assert_eq!(v, unverified(PeerReason::NoCard));
    }

    #[test]
    fn a_peer_that_was_gone_or_unreported_is_never_verified() {
        let cards = [card("s-a", 100, None)];
        assert_eq!(
            verdict(&unpinned(Some(300), Unpinned::Gone), None, &cards, None),
            unverified(PeerReason::PeerGone)
        );
        // Credentials that couldn't be read, or a pid from another namespace.
        for creds in [
            Credentials {
                uid: None,
                pid: Err(Unpinned::Unreadable),
            },
            Credentials::from_parts(Some(0), Some(1000)),
        ] {
            let g = Gathered::unpinned(creds, creds.pid.unwrap_err());
            assert_eq!(
                decide(&g, OWN, None),
                unverified(PeerReason::Unreadable),
                "{creds:?}"
            );
        }
        let none = Credentials::from_parts(None, Some(1000));
        assert_eq!(
            decide(&Gathered::unpinned(none, none.pid.unwrap_err()), OWN, None),
            unverified(PeerReason::Unsupported)
        );
    }

    // P3: a resolver's session wins over the generic rules.
    #[test]
    fn a_resolvers_session_wins_over_the_generic_rules() {
        let cards = [card("s-a", 100, None), card("s-b", 200, Some("beta"))];
        let hook = Answer(Resolution::Session("s-b".into()));
        let v = verdict(&peer(100, &[1]), None, &cards, Some(&hook));
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.session_id(), Some("s-b"));
        assert_eq!(v.name(), Some("beta"));
        assert_eq!(v.via(), PeerVia::Resolver);
    }

    // P3: a resolver that passes leaves the generic rules to decide.
    #[test]
    fn a_resolver_that_passes_leaves_the_generic_rules() {
        let cards = [card("s-a", 100, None)];
        let hook = Answer(Resolution::Pass);
        let v = verdict(&peer(100, &[1]), None, &cards, Some(&hook));
        assert_eq!(v.via(), PeerVia::Process);
        assert_eq!(v.session_id(), Some("s-a"));
    }

    #[test]
    fn a_resolvers_session_without_a_live_card_is_no_card() {
        let cards = [card("s-a", 100, None)];
        let hook = Answer(Resolution::Session("s-gone".into()));
        let v = verdict(&peer(300, &[100]), None, &cards, Some(&hook));
        assert_eq!(v, unverified(PeerReason::NoCard));
    }

    // S3: a refusal is final. Here the peer is the process of the only card,
    // which the generic rules would verify.
    #[test]
    fn a_resolvers_refusal_is_final() {
        let cards = [card("s-a", 100, None)];
        let hook = Answer(Resolution::Refused(PeerReason::SubagentCall));
        let v = verdict(&peer(100, &[1]), None, &cards, Some(&hook));
        assert_eq!(v, unverified(PeerReason::SubagentCall));
    }

    // P4: in the receiving process, a sender naming a session served here.
    // Both cards share this pid, so rule 3 alone would say shared_host.
    #[test]
    fn in_process_naming_a_session_served_here_is_verified() {
        let cards = [card("s-a", OWN, None), card("s-b", OWN, Some("beta"))];
        let v = verdict(&peer(OWN, &[1]), Some(&sender("s-b")), &cards, None);
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.session_id(), Some("s-b"));
        assert_eq!(v.name(), Some("beta"));
        assert_eq!(v.via(), PeerVia::InProcess);
    }

    // P4: in the receiving process with no sender: nothing to verify.
    #[test]
    fn in_process_without_a_sender_is_no_claim() {
        let cards = [card("s-a", OWN, None)];
        let v = verdict(&peer(OWN, &[1]), None, &cards, None);
        assert_eq!(v, unverified(PeerReason::NoClaim));
    }

    // P4: a sender that is no session served by this process.
    #[test]
    fn in_process_naming_another_processs_session_is_unknown_sender() {
        let cards = [card("s-a", OWN, None), card("s-c", 999, None)];
        for claimed in ["s-c", "s-x"] {
            let v = verdict(&peer(OWN, &[1]), Some(&sender(claimed)), &cards, None);
            assert_eq!(v, unverified(PeerReason::UnknownSender), "{claimed}");
        }
    }

    // The in-process rule applies to this process only: a resolver is not
    // asked about it, and its answer can't change it.
    #[test]
    fn in_process_is_decided_before_any_resolver() {
        let cards = [card("s-a", OWN, None), card("s-b", 200, None)];
        let hook = Answer(Resolution::Session("s-b".into()));
        let v = verdict(&peer(OWN, &[1]), None, &cards, Some(&hook));
        assert_eq!(v, unverified(PeerReason::NoClaim));
    }

    // B1b r2 (1): a card from an earlier process that had this pid is not
    // this process's card, whatever its pid says.
    #[test]
    fn a_card_from_an_earlier_process_with_this_pid_is_no_card() {
        let stale = [card_started("s-a", 100, Some(START - 1))];
        let v = verdict(&peer(100, &[1]), None, &stale, None);
        assert_eq!(
            v,
            unverified(PeerReason::NoCard),
            "a card from an earlier process with this pid: {v:?}"
        );
    }

    // B1b r2 (1): a card written without a start time (by an older build)
    // can't be tied to a process, so it never verifies, even next to a
    // card that does carry this process's start time.
    #[test]
    fn a_card_without_a_start_time_never_verifies() {
        let legacy = card_started("s-a", 100, None);
        let v = verdict(&peer(100, &[1]), None, std::slice::from_ref(&legacy), None);
        assert_eq!(
            v,
            unverified(PeerReason::LegacyCard),
            "a card without a start time (an older build): {v:?}"
        );
        let v = verdict(
            &peer(100, &[1]),
            None,
            &[legacy, card("s-b", 100, None)],
            None,
        );
        assert_eq!(v, unverified(PeerReason::LegacyCard), "{v:?}");
    }

    // B1b r2 (1): in this process, only a card this very process wrote
    // counts; one from an earlier process with this pid, or one without a
    // start time, does not.
    #[test]
    fn in_process_needs_a_card_this_process_wrote() {
        let stale = [card_started("s-a", OWN, Some(START - 1))];
        let v = verdict(&peer(OWN, &[1]), Some(&sender("s-a")), &stale, None);
        assert_eq!(
            v,
            unverified(PeerReason::UnknownSender),
            "a card an earlier process with this pid wrote: {v:?}"
        );
        let legacy = [card_started("s-a", OWN, None)];
        let v = verdict(&peer(OWN, &[1]), Some(&sender("s-a")), &legacy, None);
        assert_eq!(v, unverified(PeerReason::LegacyCard), "{v:?}");
    }

    // B1b r2 (1c): a parent counts as a session's process only with that
    // card's start time.
    #[test]
    fn a_parent_whose_card_is_from_another_process_makes_no_tool_child() {
        let cards = [card_started("s-b", 200, Some(START + 1))];
        let v = verdict(&peer(300, &[200, 1]), None, &cards, None);
        assert_eq!(
            v,
            unverified(PeerReason::NoCard),
            "a parent pid whose card is from another process: {v:?}"
        );
    }

    // B1b r2 (1, 2): the session a resolver names needs a card with a start
    // time, and that time must be its process's as read now.
    #[test]
    fn a_resolvers_session_needs_a_card_with_its_processs_start_time() {
        let hook = Answer(Resolution::Session("s-b".into()));
        let hosts = Script::holder(None).answers(200, &[Some((1, START))]);
        let legacy = [card_started("s-b", 200, None)];
        let v = verdict_on(&peer(300, &[1]), None, &legacy, Some(&hook), &hosts);
        assert_eq!(
            v,
            unverified(PeerReason::LegacyCard),
            "a resolver naming a session whose card has no start time: {v:?}"
        );
        let stale = [card_started("s-b", 200, Some(START + 1))];
        let v = verdict_on(&peer(300, &[1]), None, &stale, Some(&hook), &hosts);
        assert_eq!(v, unverified(PeerReason::NoCard), "{v:?}");
        let live = [card_started("s-b", 200, Some(START))];
        let v = verdict_on(&peer(300, &[1]), None, &live, Some(&hook), &hosts);
        assert_eq!(v.session_id(), Some("s-b"), "{v:?}");
    }

    // B1b r2 (2): missing evidence decides before any rule that verifies.
    // Each peer here has the pid of a live card (in this process, with a
    // sender naming it), and none is verified.
    #[test]
    fn an_unpinned_peer_whose_pid_matches_a_card_is_never_verified() {
        let cards = [card("s-a", 100, None), card("s-own", OWN, None)];
        for (why, reason) in [
            (Unpinned::Gone, PeerReason::PeerGone),
            (Unpinned::Unreadable, PeerReason::Unreadable),
            (Unpinned::NoPidfd, PeerReason::NoPidfd),
            (Unpinned::Changed, PeerReason::EvidenceStale),
            (Unpinned::Timeout, PeerReason::Timeout),
            (Unpinned::Busy, PeerReason::Busy),
            (Unpinned::Unsupported, PeerReason::Unsupported),
        ] {
            let v = verdict(&unpinned(Some(100), why), None, &cards, None);
            assert_eq!(
                v,
                unverified(reason),
                "{why:?}: an unpinned peer whose pid matches a card: {v:?}"
            );
            let v = verdict(
                &unpinned(Some(OWN), why),
                Some(&sender("s-own")),
                &cards,
                None,
            );
            assert_eq!(v, unverified(reason), "{why:?} in this process: {v:?}");
        }
    }

    // B1b r2 (2): a resolver is never asked about a process that isn't
    // pinned, so it can't affirm one.
    #[test]
    fn a_resolver_is_never_asked_about_an_unpinned_peer() {
        let cards = [card("s-a", 100, None)];
        let hook = Counting(Answer(Resolution::Session("s-a".into())), AtomicUsize::new(0));
        let v = verdict(&unpinned(Some(300), Unpinned::Gone), None, &cards, Some(&hook));
        assert_eq!(
            v,
            unverified(PeerReason::PeerGone),
            "a resolver affirming a peer that was gone: {v:?}"
        );
        assert_eq!(hook.1.load(Ordering::SeqCst), 0, "the resolver was asked");
    }

    // B1b r2 (1b): the connecting process exited and its pid now names
    // another process, which is a session's own. The pidfd shows the
    // connector is gone, so nothing is verified.
    #[cfg(unix)]
    #[test]
    fn a_connector_replaced_before_its_proc_read_is_gone() {
        let replacement = Script::holder(Some(-1))
            .answers(300, &[Some((1, 60))])
            .answers(1, &[Some((0, 1))]);
        let cards = [card_started("s-d", 300, Some(60))];
        let v = connection(300, held(), &replacement, &cards);
        assert_eq!(
            v,
            unverified(PeerReason::PeerGone),
            "the pid now names another process: {v:?}"
        );
    }

    // B1b r2 (1b): a connector that has exited is gone; one that is still
    // there but whose stat can't be read is unreadable.
    #[cfg(unix)]
    #[test]
    fn an_exited_connector_is_gone_and_an_unreadable_one_is_unreadable() {
        let cards = [card("s-a", 300, None)];
        let exited = Script::holder(Some(-1));
        assert_eq!(
            connection(300, held(), &exited, &cards),
            unverified(PeerReason::PeerGone),
            "an exited connector"
        );
        let unreadable = Script::holder(Some(300));
        let v = connection(300, held(), &unreadable, &cards);
        assert_eq!(
            v,
            unverified(PeerReason::Unreadable),
            "a live process whose stat can't be read: {v:?}"
        );
    }

    // B1b r2 (1b): the kernel gave no pidfd, so the process can't be pinned.
    #[cfg(unix)]
    #[test]
    fn a_connector_without_a_kernel_pidfd_is_no_pidfd() {
        let proc = Script::holder(Some(100))
            .answers(100, &[Some((1, START))])
            .answers(1, &[Some((0, 1))]);
        let cards = [card("s-a", 100, None)];
        let v = connection(100, Pidfd::NotSupported, &proc, &cards);
        assert_eq!(
            v,
            unverified(PeerReason::NoPidfd),
            "no pidfd from the kernel: {v:?}"
        );
        for (pidfd, reason) in [
            (Pidfd::Gone, PeerReason::PeerGone),
            (Pidfd::Failed, PeerReason::Unreadable),
            (Pidfd::Absent, PeerReason::Unsupported),
        ] {
            let v = connection(100, pidfd, &proc, &cards);
            assert_eq!(v, unverified(reason), "{v:?}");
        }
    }

    // B1b r2 (1c): a parent that is replaced between the two reads of the
    // lineage (pid 200 now started at another time) makes the whole read
    // stale.
    #[cfg(unix)]
    #[test]
    fn parents_replaced_between_the_two_walks_are_evidence_stale() {
        let proc = Script::holder(Some(300))
            .answers(300, &[Some((200, 30))])
            .answers(200, &[Some((1, 20)), Some((1, 21))])
            .answers(1, &[Some((0, 1))]);
        let cards = [card_started("s-b", 200, Some(20))];
        let v = connection(300, held(), &proc, &cards);
        assert_eq!(
            v,
            unverified(PeerReason::EvidenceStale),
            "a parent replaced between the two walks: {v:?}"
        );
    }

    // B1b r2 (1): a live connector that still holds its pid after the reads
    // is pinned, and verified against its own card.
    #[cfg(unix)]
    #[test]
    fn a_live_pinned_connector_is_still_verified() {
        let proc = Script::holder(Some(100))
            .answers(100, &[Some((1, START))])
            .answers(1, &[Some((0, 1))]);
        let cards = [card("s-a", 100, Some("alpha"))];
        let v = connection(100, held(), &proc, &cards);
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.session_id(), Some("s-a"));
        assert_eq!(v.via(), PeerVia::Process);
    }

    // B1b r2 fix: after a scan that missed cards, a match by process isn't
    // trusted: the one card seen for the process may have an unseen twin.
    // A process with no visible card isn't called no_card or tool_child
    // either.
    #[test]
    fn a_match_by_process_after_an_incomplete_scan_is_scan_incomplete() {
        let cards = [card("s-a", 100, Some("alpha"))];
        assert!(verdict(&peer(100, &[1]), None, &cards, None).is_verified());
        let v = verdict_scanned(&peer(100, &[1]), None, partial(&cards), None, &Steady);
        assert_eq!(v, unverified(PeerReason::ScanIncomplete));
        let v = verdict_scanned(&peer(300, &[100, 1]), None, partial(&cards), None, &Steady);
        assert_eq!(v, unverified(PeerReason::ScanIncomplete));
        let hook = Answer(Resolution::Pass);
        let v = verdict_scanned(
            &peer(100, &[1]),
            None,
            partial(&cards),
            Some(&hook),
            &Steady,
        );
        assert_eq!(v, unverified(PeerReason::ScanIncomplete));
    }

    // B1b r2 fix: the rules that find a card by its session id don't need
    // every card, so they still decide after an incomplete scan.
    #[test]
    fn in_process_and_a_resolver_still_decide_after_an_incomplete_scan() {
        let cards = [card("s-own", OWN, None)];
        let v = verdict_scanned(
            &peer(OWN, &[1]),
            Some(&sender("s-own")),
            partial(&cards),
            None,
            &Steady,
        );
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.via(), PeerVia::InProcess);

        let cards = [card("s-b", 200, Some("beta"))];
        let hook = Answer(Resolution::Session("s-b".into()));
        let v = verdict_scanned(
            &peer(300, &[1]),
            None,
            partial(&cards),
            Some(&hook),
            &Steady,
        );
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.session_id(), Some("s-b"));
        assert_eq!(v.via(), PeerVia::Resolver);

        let hook = Answer(Resolution::Refused(PeerReason::CallEnded));
        let v = verdict_scanned(
            &peer(300, &[1]),
            None,
            partial(&cards),
            Some(&hook),
            &Steady,
        );
        assert_eq!(v, unverified(PeerReason::CallEnded));
    }

    // B1b r2 fix, end to end: two cards of one process, split by the scan
    // limit. The scan sees one, stops before its twin, and says so; the
    // process is not taken for the one session it saw (it serves two).
    #[cfg(unix)]
    #[test]
    fn twin_cards_split_by_the_scan_limit_are_scan_incomplete() {
        use super::super::registry::{live_cards_in, MAX_SCANNED_CARDS};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Written in place (truncate and write), so a file keeps its entry.
        let write = |id: &str, pid: u32| {
            let bytes = serde_json::to_vec(&card(id, pid, None)).unwrap();
            std::fs::write(dir.join(format!("{id}.json")), bytes).unwrap();
        };
        // The scan's own order: the directory's.
        let order = || -> Vec<String> {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".json"))
                .map(|name| name.trim_end_matches(".json").to_string())
                .collect()
        };
        let filler = std::process::id();
        for i in 0..=MAX_SCANNED_CARDS {
            write(&format!("s-{i:03}"), filler);
        }
        let ids = order();
        assert_eq!(ids.len(), MAX_SCANNED_CARDS + 1);
        // The twins' process: alive, and neither the receiver nor a filler.
        let host = std::os::unix::process::parent_id();
        assert!(host != OWN && host != filler);
        let (first, last) = (&ids[0], &ids[MAX_SCANNED_CARDS]);
        write(first, host);
        write(last, host);
        assert_eq!(
            order(),
            ids,
            "rewriting two cards changed the directory order"
        );

        let scan = live_cards_in(dir);
        assert!(
            !scan.complete,
            "a scan that stopped at the limit says it is complete"
        );
        let twins: Vec<&str> = scan
            .cards
            .iter()
            .filter(|c| c.pid == host)
            .map(|c| c.session_id.as_str())
            .collect();
        assert_eq!(
            twins,
            [first.as_str()],
            "the scan should see exactly the first twin"
        );
        let v = verdict_scanned(&peer(host, &[1]), None, scan, None, &Steady);
        assert_eq!(v, unverified(PeerReason::ScanIncomplete));
    }

    #[test]
    fn wire_shapes_are_pinned() {
        let cards = [card("s-a", 100, Some("alpha"))];
        let v = verdict(&peer(100, &[1]), None, &cards, None);
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            json!({"verified": true, "session_id": "s-a", "name": "alpha", "via": "process"})
        );
        assert_eq!(
            serde_json::to_value(unverified(PeerReason::NoCard)).unwrap(),
            json!({"verified": false, "session_id": null, "name": null, "via": "none", "reason": "no_card"})
        );
        for (via, text) in [
            (PeerVia::InProcess, "in_process"),
            (PeerVia::Resolver, "resolver"),
            (PeerVia::Process, "process"),
        ] {
            assert_eq!(serde_json::to_value(via).unwrap(), json!(text));
        }
        for (reason, text) in [
            (PeerReason::LegacyCard, "legacy_card"),
            (PeerReason::Unreadable, "unreadable"),
            (PeerReason::NoPidfd, "no_pidfd"),
            (PeerReason::Timeout, "timeout"),
            (PeerReason::Busy, "busy"),
            (PeerReason::ScanIncomplete, "scan_incomplete"),
            (PeerReason::PeerGone, "peer_gone"),
            (PeerReason::EvidenceStale, "evidence_stale"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), json!(text));
        }
    }

    // P12: values from a newer build decode as Unknown.
    #[test]
    fn an_unknown_via_or_reason_decodes_as_unknown() {
        let v: EventPeer = serde_json::from_value(json!({
            "verified": false, "session_id": null, "name": null,
            "via": "telepathy", "reason": "moon_phase"
        }))
        .unwrap();
        assert_eq!(v.via(), PeerVia::Unknown);
        assert_eq!(v.reason(), Some(PeerReason::Unknown));
        assert!(!v.is_verified());
    }

    // P12: `verified: true` without a session id (or with via "none") reads
    // as unverified, and exposes no session or name.
    #[test]
    fn verified_without_a_session_id_reads_as_unverified() {
        for shape in [
            json!({"verified": true, "session_id": null, "name": "alpha", "via": "process"}),
            json!({"verified": true, "session_id": "", "name": "alpha", "via": "process"}),
            json!({"verified": true, "name": "alpha", "via": "process"}),
            json!({"verified": true, "session_id": "s-a", "name": "alpha", "via": "none"}),
            json!({"verified": true, "session_id": "s-a", "name": "alpha"}),
        ] {
            let v: EventPeer = serde_json::from_value(shape.clone()).unwrap();
            assert!(!v.is_verified(), "{shape}");
            assert_eq!(v.session_id(), None, "{shape}");
            assert_eq!(v.name(), None, "{shape}");
        }
        let ok: EventPeer = serde_json::from_value(
            json!({"verified": true, "session_id": "s-a", "name": null, "via": "telepathy"}),
        )
        .unwrap();
        assert!(
            ok.is_verified(),
            "a newer build's way of verifying still counts"
        );
    }

    #[test]
    fn a_peer_running_as_another_user_is_foreign() {
        assert!(!is_foreign(Some(1000), Some(1000)));
        assert!(is_foreign(Some(1001), Some(1000)));
        assert!(is_foreign(Some(0), Some(1000)), "root included");
        assert!(!is_foreign(None, Some(1000)));
        assert!(!is_foreign(Some(1001), None));
    }

    #[test]
    fn the_pidfd_pid_line_is_read_from_fdinfo() {
        let info = "pos:\t0\nflags:\t02000002\nmnt_id:\t15\nino:\t1057\nPid:\t4242\nNSpid:\t4242\n";
        assert_eq!(parse_fdinfo_pid(info), Some(4242));
        assert_eq!(parse_fdinfo_pid("Pid:\t-1\nNSpid:\t-1\n"), Some(-1));
        assert_eq!(parse_fdinfo_pid("pos:\t0\n"), None);
    }

    // The real reader gives what the kernel prints: fields 4 and 22 of
    // `/proc/<pid>/stat`, and the cards' start is field 22 of this process.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_system_reader_gives_the_kernels_ppid_and_start_ticks() {
        let text = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<&str> = text[text.rfind(')').unwrap() + 1..].split_whitespace().collect();
        let (ppid, ticks): (u32, u64) = (fields[1].parse().unwrap(), fields[19].parse().unwrap());
        assert_eq!(SystemProc.stat(std::process::id()), Some((ppid, ticks)));
        assert_eq!(own_start_ticks(), Some(ticks));
        assert_eq!(SystemProc.stat(u32::MAX - 1), None, "no such pid");
    }

    // A real connection to this process, pinned on this kernel: its start
    // time is this process's, and its first parent is this one's parent.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_real_connection_is_pinned_with_its_start_time_and_parents() {
        use std::os::fd::AsFd;
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let creds = Credentials::of_unix_stream(&ours);
        assert_eq!(creds.pid, Ok(std::process::id()));
        let peer = pin(creds, &Pidfd::of_socket(ours.as_fd()), &SystemProc);
        drop(theirs);
        if !super::kernel_gives_peer_pidfds() {
            println!("this kernel gives no pidfds for socket peers: checked no_pidfd only");
            assert_eq!(peer.lineage, Lineage::Unpinned(Unpinned::NoPidfd));
            return;
        }
        assert_eq!(peer.start_ticks(), own_start_ticks(), "{peer:?}");
        assert!(own_start_ticks().is_some());
        let ancestors = peer.ancestors();
        assert_eq!(
            ancestors.first().map(|a| a.pid),
            Some(std::os::unix::process::parent_id())
        );
        assert!(ancestors.len() <= MAX_ANCESTORS);
        assert!(ancestors.iter().all(|a| a.pid != 0));
    }
}
