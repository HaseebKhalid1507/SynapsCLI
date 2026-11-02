//! Spawned-child identity ledger: which daemon session (and which worker)
//! spawned each tool child process.
//!
//! - **Identity** is `(pid, instance)`; the instance is the process's
//!   [`InstanceKey`] (on Linux the boot id plus `start_ticks`, field 22 of
//!   `/proc/<pid>/stat`, raw clock ticks since boot; on macOS the boot
//!   session plus the kernel's unique id), and one of a kind this build
//!   doesn't know matches nothing. A pid alone is never an identity.
//!   `sid` and `pgid` come along for the inspector's attribution rule.
//! - **Who reports.** The `bash` tool and the `shell_start` PTY report each
//!   child they spawn, right after `spawn()` returns (the child is still
//!   unreaped, so its stat can't race a pid reuse), and again when they reap
//!   it. Extension and MCP stdio processes go on the host list (labels only).
//! - **The ledger is the authority** for the daemon's `children` control
//!   frame. The `tool_child` event is sent only after the record is in the
//!   ledger, so a client that queries on receipt of the event finds it
//!   unless the record was refused at a cap (the event is still sent) or a
//!   short child was swept before the query. Events are for display; the
//!   query is the authority.
//! - **Fresh check.** Every `children` reply reads each listed running
//!   leader's stat now (on the blocking pool). A leader found gone, reused
//!   or a zombie turns its record ended in the ledger, under the lock and
//!   only if it is still that process, before the reply says anything. A
//!   record never turns back to running. With a sid filter only the
//!   matching records are read. The reply's global flags
//!   (`refused_records`, `ledger_full`) are read after the check, under the
//!   same lock, from the ledger as the check left it.
//! - **Bounds.** At most [`MAX_PER_SESSION`] records per session and
//!   [`MAX_TOTAL`] in all, for live spawns and restored records alike (one
//!   bounded insertion). Over a cap the oldest *ended* record goes first; a
//!   running leader is never dropped. When only running records remain, the
//!   new record is refused and counted (`refused_records`). Either way the
//!   session reads `truncated: true`, also when it has no record left (then
//!   it lists as an empty group until the next sweep, at most
//!   [`MAX_TRUNCATED_EMPTY`] such sessions). A truncated group, empty or
//!   not, says the listing is incomplete: it is no evidence that nothing
//!   (more) was spawned. `ledger_full` says the total cap is reached with no
//!   ended record left to evict. It is a global diagnostic of stored
//!   admission, also under a filter: `false` does not promise that a given
//!   session's next record is kept, since that session's own cap can still
//!   refuse it. Members are listed up to [`MAX_MEMBERS_PER_SID`] per sid and
//!   [`MAX_MEMBERS_PER_REPLY`] per reply; a group with a list cut short
//!   reads truncated.
//! - **States.** A record is `running` while its leader (the spawned
//!   process itself) is alive with the same start ticks. It turns `ended`
//!   when the tool reaps the leader (`tool_child_ended`), or when a sweep, a
//!   reply's fresh check or a restore finds the leader's pid gone, reused,
//!   or a zombie (stat state `Z`/`X`: exited but never reaped; exit code and
//!   signal unknown until the tool reaps it). Other processes left in its
//!   sid only keep an ended record; nothing turns a record back to running.
//! - **Retention.** A record is kept while its leader runs. After it ends,
//!   it is kept until a sweep finds no live (non-zombie) process left in its
//!   session id (sid). A sweep is one `/proc` walk, run lazily on a spawn or
//!   a query at most once per [`SWEEP_EVERY`], on a blocking thread, with
//!   its own walk (never the member queries' shared one). It
//!   judges only records (and host entries) inserted before it began: each
//!   carries an insert sequence, and a record added during or after the walk
//!   is never ended or dropped by it. A sweep drops only records that had
//!   ended before it was claimed: one that ends during it (by the sweep
//!   itself, or reaped or found gone while the walk runs) is judged by a
//!   later sweep, since a walk never lists a pid forked after it passed
//!   that pid's place. A leader pid found reused still drops the record at
//!   once: its session is gone. Records outlive their daemon session.
//! - **Reload.** [`snapshot_for_reload`] / [`restore_after_reload`] carry the
//!   records across the daemon's exec. On restore a record is dropped when
//!   its boot id differs, or when its pid is alive with other start ticks
//!   (reused); a record whose pid is gone or a zombie is kept as ended. Each
//!   goes through the same bounded insertion as a live spawn.
//!
//! Linux and macOS ([`supported`]): elsewhere nothing is recorded and lists
//! say `supported: false`. On macOS an exited, unreaped leader reads as
//! gone, not as a zombie.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::proc_identity::{BootKey, InstanceKey, ProcessIdentity, StartKey};

/// Records kept per daemon session.
pub const MAX_PER_SESSION: usize = 256;
/// Records kept in all.
pub const MAX_TOTAL: usize = 4096;
/// Host-list entries kept (extension and MCP processes).
pub const MAX_HOST: usize = 256;
/// Least time between two sweeps.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// `(pid, instance)` plus the process group and session ids, and on Linux
/// the instance's `start_ticks` and `boot_id` again. No `Eq` here or in what holds it: an `instance` of a kind this build
/// doesn't know equals nothing ([`InstanceKey::Unknown`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ChildIdentityWire")]
pub struct ChildIdentity {
    pub pid: u32,
    /// The Linux start ticks, as the instance holds them, for older
    /// readers; absent for an instance of another kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ticks: Option<u64>,
    /// The Linux boot id, likewise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    pub sid: u32,
    pub pgid: u32,
    /// The process instance, tagged by platform; on Linux it repeats
    /// `boot_id` and `start_ticks`. The ledger matches a process on `pid`
    /// and this alone.
    pub instance: InstanceKey,
}

impl ChildIdentity {
    /// `pid` as the process `instance` names, with the older fields filled
    /// from it.
    pub fn new(pid: u32, sid: u32, pgid: u32, instance: InstanceKey) -> Self {
        let (start_ticks, boot_id) = match instance.linux_fields() {
            Some((ticks, boot)) => (Some(ticks), Some(boot.to_string())),
            None => (None, None),
        };
        Self { pid, start_ticks, boot_id, sid, pgid, instance }
    }
}

/// [`ChildIdentity`] as read: a record or frame written before `instance`
/// existed gets the Linux key its other fields name; one with neither is
/// refused.
#[derive(Deserialize)]
struct ChildIdentityWire {
    pid: u32,
    #[serde(default)]
    start_ticks: Option<u64>,
    #[serde(default)]
    boot_id: Option<String>,
    sid: u32,
    pgid: u32,
    #[serde(default)]
    instance: Option<InstanceKey>,
}

impl TryFrom<ChildIdentityWire> for ChildIdentity {
    type Error = &'static str;

    fn try_from(wire: ChildIdentityWire) -> Result<Self, Self::Error> {
        let ChildIdentityWire { pid, start_ticks, boot_id, sid, pgid, instance } = wire;
        let instance = match (instance, start_ticks, &boot_id) {
            (Some(instance), _, _) => instance,
            (None, Some(start_ticks), Some(boot_id)) => InstanceKey::Linux { boot_id: boot_id.clone(), start_ticks },
            (None, _, _) => return Err("a child needs an instance, or start_ticks and boot_id"),
        };
        Ok(Self { pid, start_ticks, boot_id, sid, pgid, instance })
    }
}

/// Whether this platform records children at all ([`ProcessIdentity::supported`]).
pub fn supported() -> bool {
    ProcessIdentity::supported()
}

/// The identity of `pid`, read now. Call it while the child is unreaped.
pub fn identify(pid: u32) -> Option<ChildIdentity> {
    let process = ProcessIdentity::identify(pid).ok()?;
    Some(ChildIdentity::new(pid, process.sid, process.pgid, process.instance()?))
}

/// The process at `pid` now (a zombie included on Linux); `None` when it is
/// gone or unreadable, or where processes can't be identified.
fn read_now(pid: u32) -> Option<ProcessIdentity> {
    ProcessIdentity::identify(pid).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildState {
    /// Not reaped yet.
    Running,
    /// Reaped; members of its sid may remain.
    Ended,
}

/// What a tool reports about one spawned child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildSpawn {
    /// The `tool_use` id of the call that spawned it.
    pub tool_id: String,
    /// `"bash"` or `"shell_start"`.
    pub tool_name: String,
    /// The worker's handle id (`"sa_12"`) for a worker call; `None` for the
    /// foreground.
    pub handle_id: Option<String>,
    /// The shell session id, for `shell_start`.
    pub shell_session_id: Option<String>,
    pub child: ChildIdentity,
}

/// One ledger record. Also the reload-state form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildRecord {
    pub session_id: String,
    pub tool_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub handle_id: Option<String>,
    #[serde(default)]
    pub shell_session_id: Option<String>,
    pub child: ChildIdentity,
    pub state: ChildState,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
}

/// A live process in a record's sid (`members: true`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildMember {
    pub pid: u32,
    /// The Linux start ticks, for older readers; absent elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ticks: Option<u64>,
    /// As [`ChildIdentity::instance`]; always set where processes are
    /// identified, absent from a reply of an image that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceKey>,
}

/// One child in a `child_list` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildEntry {
    pub tool_id: String,
    pub tool_name: String,
    pub handle_id: Option<String>,
    pub shell_session_id: Option<String>,
    pub child: ChildIdentity,
    pub state: ChildState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<ChildMember>>,
}

/// One session's children in a `child_list` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionChildren {
    pub session_id: String,
    pub truncated: bool,
    pub children: Vec<ChildEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostOwnerKind {
    Extension,
    McpServer,
}

/// A host-wide process (extension or MCP stdio server): labels only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostChild {
    pub owner_kind: HostOwnerKind,
    pub owner_id: String,
    pub child: ChildIdentity,
}

/// The ledger's part of a `child_list` reply.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChildListing {
    pub sessions: Vec<SessionChildren>,
    pub host: Vec<HostChild>,
    /// Records refused at a cap since this daemon image started (live
    /// inserts and restore).
    pub refused_records: u64,
    /// The total cap is reached and no ended record is left to evict: the
    /// next spawn's record will be refused.
    pub ledger_full: bool,
}

/// Sessions kept as `truncated` with no record left (a session refused its
/// first record, or one whose records were all evicted). They expire at the
/// next sweep; past this many, the oldest go first.
pub const MAX_TRUNCATED_EMPTY: usize = 256;
/// Members listed per sid in one reply; a sid with more reads truncated.
pub const MAX_MEMBERS_PER_SID: usize = 256;
/// Members listed in all in one reply; past it, entries read truncated.
pub const MAX_MEMBERS_PER_REPLY: usize = 4096;

/// A kept record and its insert sequence (this image only; not carried).
#[derive(Debug, Clone)]
struct Kept {
    seq: u64,
    /// The sequence taken when it turned ended (its insert sequence while
    /// running or when it came in ended): a sweep claimed at or before it
    /// keeps the record.
    ended_at: u64,
    record: ChildRecord,
}

impl Kept {
    /// Turn it ended at sequence `at`; one already ended keeps its first end.
    fn end(&mut self, at: u64) {
        if self.record.state == ChildState::Running {
            self.record.state = ChildState::Ended;
            self.ended_at = at;
        }
    }
}

impl std::ops::Deref for Kept {
    type Target = ChildRecord;
    fn deref(&self) -> &ChildRecord {
        &self.record
    }
}

impl std::ops::DerefMut for Kept {
    fn deref_mut(&mut self) -> &mut ChildRecord {
        &mut self.record
    }
}

/// A host-list entry and its insert sequence.
#[derive(Debug, Clone)]
struct KeptHost {
    seq: u64,
    host: HostChild,
}

/// The ledger itself. The process-wide instance sits behind [`ledger`];
/// tests build their own.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Oldest first, all sessions.
    records: VecDeque<Kept>,
    /// Sessions that lost a record to a cap, with the sequence at the time.
    /// Bounded: a session with records keeps its entry while it has any;
    /// one with none expires at the next sweep (see [`MAX_TRUNCATED_EMPTY`]).
    truncated: HashMap<String, u64>,
    host: Vec<KeptHost>,
    /// Next insert sequence. A sweep applies its scan only to what was
    /// inserted before the scan began.
    next_seq: u64,
    /// Records refused at a cap since this image started.
    refused: u64,
    last_sweep: Option<Instant>,
    sweeping: bool,
}

/// The same process instance: the same pid and an equal `instance` (one of
/// an unknown kind is the same as nothing).
fn same_process(a: &ChildIdentity, b: &ChildIdentity) -> bool {
    a.pid == b.pid && a.instance == b.instance
}

/// `child` started as the process `now` names (an unknown kind never did).
fn started_as(child: &ChildIdentity, now: StartKey) -> bool {
    child.instance.start() == Some(now)
}

/// The leader of `child` runs: its pid is present with the same start key
/// and it is not a zombie.
fn leader_runs(child: &ChildIdentity, now: Option<ProcessIdentity>) -> bool {
    now.is_some_and(|p| p.is_live() && started_as(child, p.start))
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn session_len(&self, session_id: &str) -> usize {
        self.records.iter().filter(|r| r.session_id == session_id).count()
    }

    /// Mark `session_id` truncated, keeping the set bounded: past
    /// [`MAX_TRUNCATED_EMPTY`] sessions with no record, the oldest of those
    /// goes.
    fn mark_truncated(&mut self, session_id: &str) {
        let seq = self.next_seq();
        self.truncated.insert(session_id.to_string(), seq);
        if self.truncated.len() <= MAX_TRUNCATED_EMPTY {
            return;
        }
        let with_records: HashSet<&str> = self.records.iter().map(|r| r.session_id.as_str()).collect();
        let mut empty: Vec<(u64, String)> = self
            .truncated
            .iter()
            .filter(|(s, _)| !with_records.contains(s.as_str()))
            .map(|(s, seq)| (*seq, s.clone()))
            .collect();
        if empty.len() > MAX_TRUNCATED_EMPTY {
            empty.sort_unstable();
            for (_, gone) in empty.iter().take(empty.len() - MAX_TRUNCATED_EMPTY) {
                self.truncated.remove(gone);
            }
        }
    }

    /// Drop the oldest ended record (of `session_id` when given). The session
    /// that lost it reads truncated. False when there is none to drop.
    fn evict_oldest_ended(&mut self, session_id: Option<&str>) -> bool {
        let found = self.records.iter().position(|r| {
            r.state == ChildState::Ended && session_id.is_none_or(|s| r.session_id == s)
        });
        match found.and_then(|i| self.records.remove(i)) {
            Some(gone) => {
                self.mark_truncated(&gone.record.session_id);
                true
            }
            None => false,
        }
    }

    /// The one bounded way in, for live inserts and restore alike: over a
    /// cap the oldest ended record goes first; with none to evict the
    /// arrival is refused, counted, and its session reads truncated.
    fn admit(&mut self, record: ChildRecord) -> bool {
        let fits_session = self.session_len(&record.session_id) < MAX_PER_SESSION
            || self.evict_oldest_ended(Some(&record.session_id));
        let fits_total = fits_session && (self.records.len() < MAX_TOTAL || self.evict_oldest_ended(None));
        if !fits_total {
            self.refused = self.refused.saturating_add(1);
            self.mark_truncated(&record.session_id);
            return false;
        }
        let seq = self.next_seq();
        self.records.push_back(Kept { seq, ended_at: seq, record });
        true
    }

    /// Record a spawn. False when the caps left no room (only running
    /// records remain); the session then reads truncated.
    pub fn insert(&mut self, session_id: &str, spawn: ChildSpawn) -> bool {
        self.admit(ChildRecord {
            session_id: session_id.to_string(),
            tool_id: spawn.tool_id,
            tool_name: spawn.tool_name,
            handle_id: spawn.handle_id,
            shell_session_id: spawn.shell_session_id,
            child: spawn.child,
            state: ChildState::Running,
            exit_code: None,
            signal: None,
        })
    }

    /// Mark the record of `child` ended. False when there is none.
    pub fn mark_ended(&mut self, child: &ChildIdentity, exit_code: Option<i32>, signal: Option<i32>) -> bool {
        let at = self.next_seq();
        match self.records.iter_mut().find(|r| same_process(&r.child, child)) {
            Some(record) => {
                record.end(at);
                record.exit_code = exit_code;
                record.signal = signal;
                true
            }
            None => false,
        }
    }

    /// A fresh check saw the leader of `child` gone, reused or a zombie: its
    /// record, if still that process and running, turns ended (code and
    /// signal unknown). Never the other way. Returns the record's state now.
    fn observe_gone(&mut self, child: &ChildIdentity) -> Option<(ChildState, Option<i32>, Option<i32>)> {
        let at = self.next_seq();
        let record = self.records.iter_mut().find(|r| same_process(&r.child, child))?;
        if record.state == ChildState::Running {
            record.end(at);
            record.exit_code = None;
            record.signal = None;
        }
        Some((record.state, record.exit_code, record.signal))
    }

    /// Apply one `/proc` scan, begun when the next insert sequence was
    /// `watermark`: only records (and host entries) inserted before that are
    /// judged, so a child spawned during or after the walk is never ended or
    /// dropped by a scan that could not see it. A running record whose
    /// leader is gone, reused or a zombie becomes ended. A record that had
    /// ended before the claim is dropped when no live process is left in its
    /// sid; one that ended since (now, or while the walk ran) waits for the
    /// next sweep, as the walk may have missed a pid forked during it. A
    /// reused leader pid drops either. Zombies count as gone throughout.
    /// Sessions truncated with no record left expire.
    pub fn apply_sweep(&mut self, scan: &[ProcessIdentity], watermark: u64) {
        // No scan is claimed after now (tests pass u64::MAX for everything).
        let watermark = watermark.min(self.next_seq);
        let ended_now = self.next_seq();
        let live_sids: HashSet<u32> = scan.iter().filter(|p| p.is_live()).map(|p| p.sid).collect();
        let live: HashMap<u32, StartKey> = scan.iter().filter(|p| p.is_live()).map(|p| (p.pid, p.start)).collect();
        // Every pid in the scan, zombies included: for the reuse check.
        let present: HashMap<u32, StartKey> = scan.iter().map(|p| (p.pid, p.start)).collect();
        let runs = |child: &ChildIdentity| live.get(&child.pid).is_some_and(|now| started_as(child, *now));
        for record in self.records.iter_mut().filter(|r| r.seq < watermark) {
            if record.state == ChildState::Running && !runs(&record.child) {
                record.end(ended_now);
            }
        }
        // The kernel never reuses a pid while it still names a session, so a
        // leader's pid alive with another start key means its session is gone
        // and the live sid of that number belongs to someone else. An empty
        // sid is no such proof for a record ended since the claim.
        let session_alive = |r: &Kept| {
            let leader_reused =
                r.child.sid == r.child.pid && present.get(&r.child.pid).is_some_and(|now| !started_as(&r.child, *now));
            !leader_reused && (r.ended_at >= watermark || live_sids.contains(&r.child.sid))
        };
        self.records.retain(|r| r.seq >= watermark || r.state == ChildState::Running || session_alive(r));
        self.host.retain(|h| h.seq >= watermark || runs(&h.host.child));
        let with_records: HashSet<&str> = self.records.iter().map(|r| r.session_id.as_str()).collect();
        self.truncated.retain(|s, seq| *seq >= watermark || with_records.contains(s.as_str()));
    }

    /// The records of `session_id` (all when `None`), filtered to `sid` when
    /// given, grouped by session in order of first record, plus the host
    /// list under the same sid filter. Without a sid filter, a session
    /// truncated with no record left is a group with no children.
    pub fn list(&self, session_id: Option<&str>, sid: Option<u32>) -> ChildListing {
        let mut sessions: Vec<SessionChildren> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for record in &self.records {
            if session_id.is_some_and(|s| s != record.session_id) || sid.is_some_and(|s| s != record.child.sid) {
                continue;
            }
            let entry = ChildEntry {
                tool_id: record.tool_id.clone(),
                tool_name: record.tool_name.clone(),
                handle_id: record.handle_id.clone(),
                shell_session_id: record.shell_session_id.clone(),
                child: record.child.clone(),
                state: record.state,
                exit_code: record.exit_code,
                signal: record.signal,
                members: None,
            };
            match index.get(record.session_id.as_str()) {
                Some(&at) => sessions[at].children.push(entry),
                None => {
                    index.insert(record.session_id.as_str(), sessions.len());
                    sessions.push(SessionChildren {
                        session_id: record.session_id.clone(),
                        truncated: self.truncated.contains_key(&record.session_id),
                        children: vec![entry],
                    });
                }
            }
        }
        if sid.is_none() {
            let mut empty: Vec<(u64, &String)> = self
                .truncated
                .iter()
                // With no sid filter every session with records is in `index`.
                .filter(|(s, _)| !index.contains_key(s.as_str()) && session_id.is_none_or(|want| want == s.as_str()))
                .map(|(s, seq)| (*seq, s))
                .collect();
            empty.sort_unstable();
            for (_, s) in empty {
                sessions.push(SessionChildren { session_id: s.clone(), truncated: true, children: Vec::new() });
            }
        }
        let host = if session_id.is_some() {
            Vec::new()
        } else {
            self.host.iter().filter(|h| sid.is_none_or(|s| s == h.host.child.sid)).map(|h| h.host.clone()).collect()
        };
        ChildListing { sessions, host, refused_records: self.refused, ledger_full: self.full() }
    }

    /// The total cap is reached and no ended record is left to evict.
    fn full(&self) -> bool {
        self.records.len() >= MAX_TOTAL && !self.records.iter().any(|r| r.state == ChildState::Ended)
    }

    /// Add a host-wide process (bounded; the oldest goes first).
    pub fn add_host(&mut self, owner_kind: HostOwnerKind, owner_id: &str, child: ChildIdentity) {
        self.host.retain(|h| !same_process(&h.host.child, &child));
        if self.host.len() >= MAX_HOST {
            self.host.remove(0);
        }
        let seq = self.next_seq();
        self.host.push(KeptHost { seq, host: HostChild { owner_kind, owner_id: owner_id.to_string(), child } });
    }

    /// Remove a host-wide process.
    pub fn remove_host(&mut self, child: &ChildIdentity) {
        self.host.retain(|h| !same_process(&h.host.child, child));
    }

    /// The records to carry across a reload (already within the caps).
    pub fn snapshot(&self) -> Vec<ChildRecord> {
        self.records.iter().map(|r| r.record.clone()).collect()
    }

    /// Restore carried records: drop those from another boot (or of an
    /// unknown kind) or whose pid is alive with another start key (reused);
    /// keep the rest, ended when the pid is gone or a zombie. Each goes through the same bounded insertion
    /// as a live spawn, oldest first: over a cap the oldest ended goes, and
    /// with none to evict the arrival is refused and counted.
    pub fn restore(
        &mut self,
        carried: Vec<ChildRecord>,
        current_boot: &BootKey,
        stat_of: impl Fn(u32) -> Option<ProcessIdentity>,
    ) {
        for mut record in carried {
            if record.child.instance.boot() != *current_boot {
                continue;
            }
            match stat_of(record.child.pid) {
                Some(now) if !started_as(&record.child, now.start) => continue,
                Some(now) if now.is_live() => {}
                // Gone, or a zombie nobody will reap (its tool died with the
                // old image): ended, code and signal unknown.
                Some(_) | None => record.state = ChildState::Ended,
            }
            if self.records.iter().any(|r| same_process(&r.child, &record.child)) {
                continue;
            }
            self.admit(record);
        }
    }

    /// Whether a sweep is due now; when it is, claim it and return the
    /// watermark its scan applies to (the next insert sequence).
    fn claim_sweep(&mut self, now: Instant) -> Option<u64> {
        if self.sweeping || self.last_sweep.is_some_and(|t| now.duration_since(t) < SWEEP_EVERY) {
            return None;
        }
        self.sweeping = true;
        Some(self.next_seq)
    }

    fn finish_sweep(&mut self, scan: Option<&[ProcessIdentity]>, watermark: u64, now: Instant) {
        if let Some(scan) = scan {
            self.apply_sweep(scan, watermark);
        }
        self.sweeping = false;
        self.last_sweep = Some(now);
    }
}

/// Fill `members` for every entry: the live (non-zombie) processes in its
/// sid, from one walk. Only the listed sids are expanded, each at most
/// [`MAX_MEMBERS_PER_SID`], and at most [`MAX_MEMBERS_PER_REPLY`] in all; a
/// group with a list cut short reads truncated. A member's `instance` is in
/// the boot `boot`.
pub fn attach_members(listing: &mut ChildListing, scan: &[ProcessIdentity], boot: Option<&BootKey>) {
    let wanted: HashSet<u32> = listing.sessions.iter().flat_map(|g| g.children.iter().map(|c| c.child.sid)).collect();
    let mut by_sid: HashMap<u32, (Vec<ChildMember>, bool)> = HashMap::new();
    let live = scan.iter().filter(|p| p.is_live() && wanted.contains(&p.sid));
    for p in live {
        let (members, cut) = by_sid.entry(p.sid).or_default();
        if members.len() < MAX_MEMBERS_PER_SID {
            let instance = boot.and_then(|boot| InstanceKey::new(boot, p.start));
            members.push(ChildMember { pid: p.pid, start_ticks: p.start_ticks(), instance });
        } else {
            *cut = true;
        }
    }
    let mut budget = MAX_MEMBERS_PER_REPLY;
    for group in &mut listing.sessions {
        for entry in &mut group.children {
            let (members, cut) = by_sid.get(&entry.child.sid).map_or((&[][..], false), |(m, cut)| (m.as_slice(), *cut));
            let take = members.len().min(budget);
            budget -= take;
            entry.members = Some(members[..take].to_vec());
            if cut || take < members.len() {
                group.truncated = true;
            }
        }
    }
}

/// Check each running entry's leader now (`stat_of` reads it fresh): when
/// it is gone, reused or a zombie, its record turns ended in `ledger` (only
/// if it is still that process and running), and the entry shows the record
/// as it then is. Never ended to running. Then the listing's global flags
/// (`refused_records`, `ledger_full`) are read again, under the same lock,
/// from the ledger as the check left it.
fn validate_leaders_in(
    ledger: &Mutex<Ledger>,
    listing: &mut ChildListing,
    stat_of: impl Fn(u32) -> Option<ProcessIdentity>,
) {
    let mut gone: Vec<&mut ChildEntry> = listing
        .sessions
        .iter_mut()
        .flat_map(|g| g.children.iter_mut())
        .filter(|e| e.state == ChildState::Running && !leader_runs(&e.child, stat_of(e.child.pid)))
        .collect();
    let mut ledger = ledger.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for entry in gone.iter_mut() {
        let (state, exit_code, signal) = ledger.observe_gone(&entry.child).unwrap_or((ChildState::Ended, None, None));
        entry.state = state;
        entry.exit_code = exit_code;
        entry.signal = signal;
    }
    drop(gone);
    listing.refused_records = ledger.refused;
    listing.ledger_full = ledger.full();
}

/// The process-wide ledger.
pub fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::new()))
}

fn lock() -> std::sync::MutexGuard<'static, Ledger> {
    ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run a sweep on a blocking thread when one is due (at most once per
/// [`SWEEP_EVERY`]); never blocks the caller. Outside a Tokio runtime it
/// does nothing.
pub fn maybe_sweep() {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Some(watermark) = lock().claim_sweep(Instant::now()) else {
        return;
    };
    rt.spawn_blocking(move || {
        let scan = ProcessIdentity::all();
        lock().finish_sweep(Some(&scan), watermark, Instant::now());
    });
}

/// Record a spawn in the process-wide ledger. Returns false when the caps
/// left no room. Call it before sending the `tool_child` event.
pub fn record_spawn(session_id: &str, spawn: ChildSpawn) -> bool {
    let kept = lock().insert(session_id, spawn);
    maybe_sweep();
    kept
}

/// Mark a reaped child ended in the process-wide ledger.
pub fn record_end(child: &ChildIdentity, exit_code: Option<i32>, signal: Option<i32>) -> bool {
    lock().mark_ended(child, exit_code, signal)
}

/// Add a host-wide process to the process-wide ledger (no-op off Linux).
pub fn record_host(owner_kind: HostOwnerKind, owner_id: &str, pid: u32) {
    if let Some(child) = identify(pid) {
        lock().add_host(owner_kind, owner_id, child);
    }
}

/// Remove a host-wide process from the process-wide ledger.
pub fn forget_host(child: &ChildIdentity) {
    lock().remove_host(child);
}

/// A listing of the stored state from the process-wide ledger (see
/// [`Ledger::list`]); no fresh check. The `children` reply uses
/// [`list_checked`].
pub fn list(session_id: Option<&str>, sid: Option<u32>) -> ChildListing {
    let listing = lock().list(session_id, sid);
    maybe_sweep();
    listing
}

/// The `children` reply's listing. Blocking (run it on the blocking pool):
/// each listed running leader is read now, and one found gone, reused or a
/// zombie turns its record ended before the reply says anything (only the
/// listed records are checked); host entries are listed only while their
/// process runs; with `scan`, members are attached within their caps.
pub fn list_checked(session_id: Option<&str>, sid: Option<u32>, scan: Option<&[ProcessIdentity]>) -> ChildListing {
    let mut listing = lock().list(session_id, sid);
    validate_leaders_in(ledger(), &mut listing, read_now);
    listing.host.retain(|h| leader_runs(&h.child, read_now(h.child.pid)));
    if let Some(scan) = scan {
        attach_members(&mut listing, scan, ProcessIdentity::boot_key());
    }
    listing
}

type Scan = std::sync::Arc<Vec<ProcessIdentity>>;

/// One `/proc` walk for concurrent member queries: a query reuses a walk
/// that began after it asked, else runs one itself (on the blocking pool).
/// One walk at a time among queries that run to completion; queries waiting
/// meanwhile hold no thread. A cancelled query can't stop a walk already
/// started on the blocking pool, so a query after it may start another
/// while that one still runs. Sweeps never use this cache.
pub async fn shared_scan() -> Scan {
    static CACHE: OnceLock<tokio::sync::Mutex<Option<(Instant, Scan)>>> = OnceLock::new();
    shared_scan_in(CACHE.get_or_init(Default::default), ProcessIdentity::all).await
}

async fn shared_scan_in(
    cache: &tokio::sync::Mutex<Option<(Instant, Scan)>>,
    walk: fn() -> Vec<ProcessIdentity>,
) -> Scan {
    let asked = Instant::now();
    let mut slot = cache.lock().await;
    if let Some((began, scan)) = slot.as_ref() {
        if *began >= asked {
            return scan.clone();
        }
    }
    let began = Instant::now();
    let scan: Scan = std::sync::Arc::new(tokio::task::spawn_blocking(walk).await.unwrap_or_default());
    *slot = Some((began, scan.clone()));
    scan
}

/// The records to put in the reload state.
pub fn snapshot_for_reload() -> Vec<ChildRecord> {
    lock().snapshot()
}

/// Restore the records carried in the reload state.
pub fn restore_after_reload(carried: Vec<ChildRecord>) {
    let Some(boot) = ProcessIdentity::boot_key() else {
        return;
    };
    lock().restore(carried, boot, read_now);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "680bd989-b584-4a03-8f69-995bf66dd28c";

    fn ident(pid: u32, start_ticks: u64, sid: u32) -> ChildIdentity {
        ChildIdentity::new(pid, sid, sid, InstanceKey::Linux { boot_id: BOOT.into(), start_ticks })
    }

    /// The member `pid` started at `start_ticks`, in the boot [`BOOT`].
    fn member(pid: u32, start_ticks: u64) -> ChildMember {
        ChildMember {
            pid,
            start_ticks: Some(start_ticks),
            instance: Some(InstanceKey::Linux { boot_id: BOOT.into(), start_ticks }),
        }
    }

    fn boot() -> BootKey {
        BootKey::Linux { boot_id: BOOT.into() }
    }

    fn spawn(tool_id: &str, child: ChildIdentity) -> ChildSpawn {
        ChildSpawn {
            tool_id: tool_id.into(),
            tool_name: "bash".into(),
            handle_id: None,
            shell_session_id: None,
            child,
        }
    }

    /// A live process `sid` leads (pid = sid), started at `start_ticks`.
    fn stat(sid: u32, start_ticks: u64) -> ProcessIdentity {
        ProcessIdentity { pid: sid, state: 'S', ppid: 1, pgid: sid, sid, start: StartKey::Linux { start_ticks } }
    }

    /// `p`, read under `pid`.
    fn at(pid: u32, p: ProcessIdentity) -> ProcessIdentity {
        ProcessIdentity { pid, ..p }
    }

    fn boot_id() -> Option<&'static str> {
        ProcessIdentity::boot_key().map(BootKey::as_str)
    }

    /// The ledger's shapes as text, byte for byte: the reload state carries
    /// `ChildRecord`, replies and events the rest. An older record without
    /// the optional fields (or without `instance`) still loads. The old
    /// fields are as before `instance` came; it only adds to them.
    #[test]
    fn the_ledger_shapes_are_pinned_as_text() {
        const INSTANCE: &str =
            r#","instance":{"kind":"linux","boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","start_ticks":26981644}"#;
        let child = ident(4242, 26981644, 4242);
        let text = serde_json::to_string(&child).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","sid":4242,"pgid":4242,"#,
                r#""instance":{"kind":"linux","boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","start_ticks":26981644}}"#
            )
        );
        assert_eq!(
            text.replace(INSTANCE, ""),
            r#"{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","sid":4242,"pgid":4242}"#,
            "the old fields, byte for byte"
        );
        let mac = ChildIdentity::new(
            4242,
            4242,
            4242,
            InstanceKey::Darwin { boot_session: "9F1C".into(), unique_id: 4_000_000_123 },
        );
        let mac_text = serde_json::to_string(&mac).unwrap();
        assert_eq!(
            mac_text,
            r#"{"pid":4242,"sid":4242,"pgid":4242,"instance":{"kind":"darwin","boot_session":"9F1C","unique_id":4000000123}}"#,
            "no Linux fields beside a darwin instance"
        );
        assert_eq!(serde_json::from_str::<ChildIdentity>(&mac_text).unwrap(), mac, "a present instance is kept");
        let mixed = ChildIdentity { instance: mac.instance.clone(), ..child.clone() };
        let mixed_back: ChildIdentity = serde_json::from_str(&serde_json::to_string(&mixed).unwrap()).unwrap();
        assert_eq!(mixed_back, mixed, "an instance wins over the old fields");
        let bare = r#"{"pid":4242,"sid":4242,"pgid":4242}"#;
        assert!(serde_json::from_str::<ChildIdentity>(bare).is_err(), "no instance and no old fields: refused");
        assert!(
            serde_json::from_str::<ChildIdentity>(r#"{"pid":4242,"start_ticks":1,"sid":4242,"pgid":4242}"#).is_err(),
            "half the old fields: refused"
        );
        let mac_member =
            ChildMember { pid: 4243, start_ticks: None, instance: Some(mac.instance.clone()) };
        assert_eq!(
            serde_json::to_string(&mac_member).unwrap(),
            r#"{"pid":4243,"instance":{"kind":"darwin","boot_session":"9F1C","unique_id":4000000123}}"#
        );
        let record = ChildRecord {
            session_id: "s1".into(),
            tool_id: "toolu_1".into(),
            tool_name: "shell_start".into(),
            handle_id: None,
            shell_session_id: Some("shell_3".into()),
            child: child.clone(),
            state: ChildState::Ended,
            exit_code: None,
            signal: Some(15),
        };
        let text = serde_json::to_string(&record).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"session_id":"s1","tool_id":"toolu_1","tool_name":"shell_start","handle_id":null,"#,
                r#""shell_session_id":"shell_3","#,
                r#""child":{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","#,
                r#""sid":4242,"pgid":4242,"#,
                r#""instance":{"kind":"linux","boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","start_ticks":26981644}},"#,
                r#""state":"ended","exit_code":null,"signal":15}"#
            )
        );
        assert_eq!(
            text.replace(INSTANCE, ""),
            concat!(
                r#"{"session_id":"s1","tool_id":"toolu_1","tool_name":"shell_start","handle_id":null,"#,
                r#""shell_session_id":"shell_3","#,
                r#""child":{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","#,
                r#""sid":4242,"pgid":4242},"state":"ended","exit_code":null,"signal":15}"#
            ),
            "the old fields, byte for byte"
        );
        let old: ChildRecord = serde_json::from_str(concat!(
            r#"{"session_id":"s1","tool_id":"toolu_1","tool_name":"shell_start","#,
            r#""child":{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","#,
            r#""sid":4242,"pgid":4242},"state":"ended","signal":15}"#
        ))
        .unwrap();
        assert_eq!(old, ChildRecord { shell_session_id: None, ..record.clone() }, "instance from the old fields");
        assert_eq!(serde_json::from_str::<ChildRecord>(&text).unwrap(), record);
        let entry = ChildEntry {
            tool_id: "toolu_1".into(),
            tool_name: "bash".into(),
            handle_id: Some("sa_2".into()),
            shell_session_id: None,
            child: child.clone(),
            state: ChildState::Running,
            exit_code: None,
            signal: None,
            members: None,
        };
        assert_eq!(
            serde_json::to_string(&entry).unwrap().replace(INSTANCE, ""),
            concat!(
                r#"{"tool_id":"toolu_1","tool_name":"bash","handle_id":"sa_2","shell_session_id":null,"#,
                r#""child":{"pid":4242,"start_ticks":26981644,"boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","#,
                r#""sid":4242,"pgid":4242},"state":"running","exit_code":null,"signal":null}"#
            ),
            "no members key without members"
        );
        let one = member(4243, 26981700);
        let text = serde_json::to_string(&one).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"pid":4243,"start_ticks":26981700,"#,
                r#""instance":{"kind":"linux","boot_id":"680bd989-b584-4a03-8f69-995bf66dd28c","start_ticks":26981700}}"#
            )
        );
        assert_eq!(text.replace(&INSTANCE.replace("26981644", "26981700"), ""), r#"{"pid":4243,"start_ticks":26981700}"#);
        let old: ChildMember = serde_json::from_str(r#"{"pid":4243,"start_ticks":26981700}"#).unwrap();
        assert_eq!(old, ChildMember { instance: None, ..one });
        assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"pid":4243,"start_ticks":26981700}"#, "none: no key");
    }

    /// A child, member or record whose `instance` is of a kind this build
    /// doesn't know (a newer image's reload state or frame) still decodes,
    /// and that instance matches nothing, itself included.
    #[test]
    fn a_future_instance_kind_decodes_and_never_matches() {
        const FUTURE: &str = r#"{"kind":"future","boot_epoch":"680bd989","serial":26981644,"extra":{"a":[1,2]}}"#;
        let child: ChildIdentity = serde_json::from_str(&format!(
            r#"{{"pid":4242,"start_ticks":26981644,"boot_id":"{BOOT}","sid":4242,"pgid":4242,"instance":{FUTURE}}}"#
        ))
        .unwrap();
        assert_eq!(
            (child.pid, child.start_ticks, child.boot_id.as_deref(), child.sid),
            (4242, Some(26981644), Some(BOOT), 4242)
        );
        assert!(matches!(child.instance, InstanceKey::Unknown), "{child:?}");
        assert_ne!(child.instance, child.instance.clone(), "not even itself");
        assert_ne!(child, child.clone());
        assert_ne!(child.instance, ident(4242, 26981644, 4242).instance, "nor the key its old fields name");
        let member: ChildMember =
            serde_json::from_str(&format!(r#"{{"pid":4243,"start_ticks":26981700,"instance":{FUTURE}}}"#)).unwrap();
        assert!(matches!(member.instance, Some(InstanceKey::Unknown)), "{member:?}");
        assert_ne!(member.instance, member.instance.clone());
        let record: ChildRecord = serde_json::from_str(&format!(
            r#"{{"session_id":"s1","tool_id":"t","tool_name":"bash","child":{},"state":"running"}}"#,
            serde_json::to_string(&child).unwrap()
        ))
        .unwrap();
        assert!(matches!(record.child.instance, InstanceKey::Unknown));
        // Written back, the unknown kind keeps its tag only.
        assert!(serde_json::to_string(&member).unwrap().ends_with(r#","instance":{"kind":"unknown"}}"#));
    }

    /// The ledger matches a process by its `instance`, never by the older
    /// `start_ticks`/`boot_id` fields: a child whose instance is of an
    /// unknown kind is matched by nothing (it can't be ended by identity, its
    /// leader never runs for a checked reply, a sweep ends and drops it, and
    /// a restore drops it), and one whose instance names another start is
    /// told apart even with the same old fields.
    #[test]
    fn the_ledger_matches_on_instance_and_an_unknown_kind_fails_closed() {
        let unknown = |pid: u32| ChildIdentity { instance: InstanceKey::Unknown, ..ident(pid, 5, pid) };
        let mut l = Ledger::new();
        l.insert("s", spawn("unknown", unknown(90)));
        l.insert("s", spawn("known", ident(91, 5, 91)));
        assert!(!l.mark_ended(&unknown(90), Some(0), None), "an unknown instance names no record");
        assert!(!leader_runs(&unknown(90), Some(stat(90, 5))), "nor a running leader");
        assert!(leader_runs(&ident(91, 5, 91), Some(stat(91, 5))));
        let mut moved = ident(91, 5, 91);
        moved.instance = InstanceKey::Linux { boot_id: BOOT.into(), start_ticks: 6 };
        assert!(!l.mark_ended(&moved, Some(0), None), "the same old fields, another instance");
        assert!(!leader_runs(&moved, Some(stat(91, 5))));
        let carried = l.snapshot();
        l.apply_sweep(&[stat(90, 5), stat(91, 5)], u64::MAX);
        let got: Vec<(String, ChildState)> = l.records.iter().map(|r| (r.tool_id.clone(), r.state)).collect();
        assert_eq!(
            got,
            vec![("known".to_string(), ChildState::Running)],
            "a sweep ends the unknown one though its pid runs, then drops it: its pid reads as another process"
        );
        let mut new = Ledger::new();
        new.restore(carried, &boot(), |pid| Some(stat(pid, 5)));
        let kept: Vec<&str> = new.records.iter().map(|r| r.tool_id.as_str()).collect();
        assert_eq!(kept, vec!["known"], "a restore drops the unknown one");
        let mut h = Ledger::new();
        h.add_host(HostOwnerKind::Extension, "x", unknown(92));
        h.remove_host(&unknown(92));
        assert_eq!(h.host.len(), 1, "an unknown host entry is never removed by identity");
        h.apply_sweep(&[stat(92, 5)], u64::MAX);
        assert!(h.host.is_empty(), "a sweep drops it");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identify_reads_this_process_as_the_kernel_prints_it() {
        let me = std::process::id();
        let id = identify(me).expect("own identity");
        let text = std::fs::read_to_string("/proc/self/stat").unwrap();
        let after = &text[text.rfind(')').unwrap() + 1..];
        let fields: Vec<&str> = after.split_whitespace().collect();
        assert_eq!(id.pid, me);
        let start_ticks = id.start_ticks.expect("Linux start ticks");
        assert_eq!(start_ticks.to_string(), fields[19], "field 22");
        assert_eq!(id.sid.to_string(), fields[3], "field 6");
        assert_eq!(id.pgid.to_string(), fields[2], "field 5");
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        assert_eq!(id.boot_id.as_deref(), Some(boot.trim()));
        assert_eq!(id.instance, InstanceKey::Linux { boot_id: boot.trim().into(), start_ticks });
        assert!(supported());
        assert!(ProcessIdentity::clk_tck() > 0);
    }

    #[test]
    fn insert_end_and_list() {
        let mut l = Ledger::new();
        assert!(l.insert("s1", spawn("t1", ident(10, 100, 10))));
        assert!(l.mark_ended(&ident(10, 100, 10), Some(0), None));
        assert!(!l.mark_ended(&ident(10, 101, 10), Some(0), None), "other start ticks is another process");
        let got = l.list(Some("s1"), None);
        assert_eq!(got.sessions.len(), 1);
        let c = &got.sessions[0].children[0];
        assert_eq!((c.state, c.exit_code, c.signal), (ChildState::Ended, Some(0), None));
        assert_eq!(c.tool_id, "t1");
        assert!(!got.sessions[0].truncated);
        assert!(l.list(Some("nope"), None).sessions.is_empty(), "an unknown session is an empty list");
    }

    #[test]
    fn the_sid_filter_spans_sessions_and_only_filters() {
        let mut l = Ledger::new();
        l.insert("s1", spawn("a", ident(10, 1, 500)));
        l.insert("s1", spawn("b", ident(11, 1, 600)));
        l.insert("s2", spawn("c", ident(12, 1, 500)));
        l.add_host(HostOwnerKind::Extension, "ext", ident(13, 1, 500));
        l.add_host(HostOwnerKind::McpServer, "mcp", ident(14, 1, 700));
        let got = l.list(None, Some(500));
        let ids: Vec<(&str, &str)> = got
            .sessions
            .iter()
            .flat_map(|s| s.children.iter().map(move |c| (s.session_id.as_str(), c.tool_id.as_str())))
            .collect();
        assert_eq!(ids, vec![("s1", "a"), ("s2", "c")]);
        assert_eq!(got.host.len(), 1);
        assert_eq!(got.host[0].owner_id, "ext");
        let none = l.list(None, Some(999));
        assert!(none.sessions.is_empty() && none.host.is_empty());
        let both = l.list(Some("s2"), Some(500));
        assert_eq!(both.sessions.len(), 1);
        assert!(both.host.is_empty(), "a session-scoped list has no host part");
    }

    #[test]
    fn the_session_cap_drops_the_oldest_ended_and_never_a_running_leader() {
        let mut l = Ledger::new();
        for i in 0..MAX_PER_SESSION as u32 {
            assert!(l.insert("s1", spawn(&format!("t{i}"), ident(1000 + i, 1, 1000 + i))));
        }
        // All running: the next one isn't kept, and the session reads truncated.
        assert!(!l.insert("s1", spawn("over", ident(5000, 1, 5000))));
        assert_eq!(l.len(), MAX_PER_SESSION);
        assert!(l.list(Some("s1"), None).sessions[0].truncated);
        assert!(l.records.iter().all(|r| r.state == ChildState::Running));
        // Two ended: the older of them goes.
        l.mark_ended(&ident(1007, 1, 1007), Some(0), None);
        l.mark_ended(&ident(1003, 1, 1003), Some(0), None);
        assert!(l.insert("s1", spawn("next", ident(5001, 1, 5001))));
        assert_eq!(l.len(), MAX_PER_SESSION);
        assert!(!l.records.iter().any(|r| r.child.pid == 1003), "the oldest ended went");
        assert!(l.records.iter().any(|r| r.child.pid == 1007));
        // Another session is not affected by s1's cap.
        assert!(l.insert("s2", spawn("x", ident(6000, 1, 6000))));
        assert!(!l.list(Some("s2"), None).sessions[0].truncated);
    }

    #[test]
    fn the_total_cap_drops_the_oldest_ended_across_sessions() {
        let mut l = Ledger::new();
        let mut pid = 10_000u32;
        for s in 0..(MAX_TOTAL / MAX_PER_SESSION) {
            for _ in 0..MAX_PER_SESSION {
                assert!(l.insert(&format!("s{s}"), spawn("t", ident(pid, 1, pid))));
                pid += 1;
            }
        }
        assert_eq!(l.len(), MAX_TOTAL);
        assert!(!l.insert("fresh", spawn("t", ident(pid, 1, pid))), "all running: not kept");
        let fresh = l.list(Some("fresh"), None);
        assert_eq!(fresh.sessions.len(), 1, "a session refused its first record still shows");
        assert!(fresh.sessions[0].truncated && fresh.sessions[0].children.is_empty());
        assert!(l.truncated.contains_key("fresh"));
        l.mark_ended(&ident(10_300, 1, 10_300), None, Some(9)); // in s1
        assert!(l.insert("fresh", spawn("t", ident(pid + 1, 1, pid + 1))));
        assert_eq!(l.len(), MAX_TOTAL);
        assert!(!l.records.iter().any(|r| r.child.pid == 10_300));
        assert!(l.list(Some("s1"), None).sessions[0].truncated, "s1 lost a record");
    }

    #[test]
    fn a_sweep_ends_vanished_leaders_and_drops_ended_records_with_an_empty_sid() {
        let mut l = Ledger::new();
        l.insert("s", spawn("gone", ident(20, 5, 20)));
        l.insert("s", spawn("bg", ident(21, 5, 21)));
        l.insert("s", spawn("live", ident(22, 5, 22)));
        l.insert("s", spawn("reused", ident(23, 5, 23)));
        l.mark_ended(&ident(20, 5, 20), Some(0), None);
        l.mark_ended(&ident(21, 5, 21), Some(0), None);
        l.add_host(HostOwnerKind::Extension, "dead-ext", ident(30, 5, 30));
        l.add_host(HostOwnerKind::Extension, "live-ext", ident(31, 5, 31));
        // pid 77 is a background job still in sid 21; 22 runs; 23 was reused.
        let scan = vec![at(77, stat(21, 9)), stat(22, 5), stat(23, 99), stat(31, 5)];
        l.apply_sweep(&scan, u64::MAX);
        let got = l.list(Some("s"), None);
        let kept: Vec<(&str, ChildState)> =
            got.sessions[0].children.iter().map(|c| (c.tool_id.as_str(), c.state)).collect();
        assert_eq!(
            kept,
            vec![("bg", ChildState::Ended), ("live", ChildState::Running)],
            "'gone' had an empty sid; 'reused' became ended and its leader pid now names another session"
        );
        let all = l.list(None, None);
        let host: Vec<&str> = all.host.iter().map(|h| h.owner_id.as_str()).collect();
        assert_eq!(host, vec!["live-ext"]);
    }

    #[test]
    fn sweeps_are_claimed_at_most_once_per_interval() {
        let mut l = Ledger::new();
        let t0 = Instant::now();
        assert!(l.claim_sweep(t0).is_some());
        assert!(l.claim_sweep(t0).is_none(), "one in flight");
        l.finish_sweep(None, 0, t0);
        assert!(l.claim_sweep(t0 + SWEEP_EVERY - Duration::from_millis(1)).is_none());
        assert!(l.claim_sweep(t0 + SWEEP_EVERY).is_some());
    }

    #[test]
    fn members_are_the_live_processes_in_each_records_sid() {
        let mut l = Ledger::new();
        l.insert("s", spawn("t", ident(40, 5, 40)));
        l.mark_ended(&ident(40, 5, 40), Some(0), None);
        let mut listing = l.list(None, None);
        attach_members(&mut listing, &[at(41, stat(40, 7)), at(42, stat(99, 7))], Some(&boot()));
        assert_eq!(
            listing.sessions[0].children[0].members,
            Some(vec![member(41, 7)])
        );
        let plain = l.list(None, None);
        assert_eq!(plain.sessions[0].children[0].members, None, "absent unless asked for");
        let json = serde_json::to_value(&plain.sessions[0].children[0]).unwrap();
        assert!(json.get("members").is_none());
    }

    #[test]
    fn restore_drops_other_boots_and_reused_pids_and_ends_gone_ones() {
        let mut old = Ledger::new();
        old.insert("s", spawn("alive", ident(50, 5, 50)));
        old.insert("s", spawn("gone", ident(51, 5, 51)));
        old.insert("s", spawn("reused", ident(52, 5, 52)));
        let mut other_boot = ident(53, 5, 53);
        other_boot.boot_id = Some("another-boot".into());
        other_boot.instance = InstanceKey::Linux { boot_id: "another-boot".into(), start_ticks: 5 };
        old.insert("s", spawn("other-boot", other_boot));
        let carried: Vec<ChildRecord> =
            serde_json::from_str(&serde_json::to_string(&old.snapshot()).unwrap()).unwrap();
        assert_eq!(carried, old.snapshot(), "a JSON round trip");
        let mut new = Ledger::new();
        new.restore(carried, &boot(), |pid| match pid {
            50 => Some(stat(50, 5)),
            52 => Some(stat(52, 6)),
            53 => Some(stat(53, 5)),
            _ => None,
        });
        let got: Vec<(String, ChildState)> = new.records.iter().map(|r| (r.tool_id.clone(), r.state)).collect();
        assert_eq!(
            got,
            vec![("alive".to_string(), ChildState::Running), ("gone".to_string(), ChildState::Ended)]
        );
    }

    /// A leader that exited but was never reaped still has a stat line with
    /// its start ticks. It reads ended, on a sweep and on a restore, and a
    /// zombie in a sid keeps no ended record alive and is no member.
    #[test]
    fn a_zombie_leader_reads_ended_and_zombies_count_as_gone() {
        let zombie = |sid: u32, ticks: u64| ProcessIdentity { state: 'Z', ..stat(sid, ticks) };
        let mut l = Ledger::new();
        l.insert("s", spawn("zombie-leader", ident(70, 5, 70)));
        l.insert("s", spawn("zombie-member", ident(71, 5, 71)));
        l.mark_ended(&ident(71, 5, 71), Some(0), None);
        l.insert("s", spawn("live-member", ident(72, 5, 72)));
        l.mark_ended(&ident(72, 5, 72), Some(0), None);
        // 70 is a zombie leader with a live process (83) left in its sid;
        // 71's sid holds only a zombie; 72's a live process.
        let scan = [zombie(70, 5), at(83, stat(70, 9)), at(81, zombie(71, 9)), at(82, stat(72, 9))];
        l.apply_sweep(&scan, u64::MAX);
        let got: Vec<(String, ChildState)> = l.records.iter().map(|r| (r.tool_id.clone(), r.state)).collect();
        assert_eq!(
            got,
            vec![("zombie-leader".to_string(), ChildState::Ended), ("live-member".to_string(), ChildState::Ended)],
            "the zombie leader reads ended; a sid with only a zombie keeps nothing"
        );
        let mut listing = l.list(None, None);
        attach_members(&mut listing, &scan, Some(&boot()));
        let members: Vec<Option<Vec<ChildMember>>> = listing.sessions[0].children.iter().map(|c| c.members.clone()).collect();
        assert_eq!(
            members,
            vec![Some(vec![member(83, 9)]), Some(vec![member(82, 9)])],
            "zombies are no members"
        );

        let mut carried = Ledger::new();
        carried.insert("s", spawn("carried-zombie", ident(73, 5, 73)));
        let mut new = Ledger::new();
        new.restore(carried.snapshot(), &boot(), |pid| (pid == 73).then(|| zombie(73, 5)));
        assert_eq!(new.records[0].state, ChildState::Ended, "restored as ended");
        assert_eq!((new.records[0].exit_code, new.records[0].signal), (None, None), "code and signal unknown");
    }

    /// The same with a real zombie: a child that exits and isn't reaped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_unreaped_child_reads_ended_after_a_sweep_and_a_restore() {
        let Some(boot) = ProcessIdentity::boot_key() else { return };
        let mut child = std::process::Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        let identity = identify(pid).expect("readable while unreaped");
        let t0 = Instant::now();
        while read_now(pid).is_some_and(|s| s.is_live()) {
            assert!(t0.elapsed() < Duration::from_secs(10), "never became a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(read_now(pid).map(|s| (s.state, s.start_ticks())), Some(('Z', identity.start_ticks)));

        let mut l = Ledger::new();
        l.insert("s", spawn("zombie", identity.clone()));
        l.apply_sweep(&ProcessIdentity::all(), u64::MAX);
        assert_eq!(l.records[0].state, ChildState::Ended, "after a sweep");
        let mut restored = Ledger::new();
        let mut carried = Ledger::new();
        carried.insert("s", spawn("zombie", identity));
        restored.restore(carried.snapshot(), boot, read_now);
        assert_eq!(restored.records[0].state, ChildState::Ended, "after a restore");
        child.wait().unwrap();
    }

    #[test]
    fn restore_applies_the_caps_oldest_ended_first() {
        let mut carried = Vec::new();
        for i in 0..(MAX_PER_SESSION as u32 + 3) {
            carried.push(ChildRecord {
                session_id: "s".into(),
                tool_id: format!("t{i}"),
                tool_name: "bash".into(),
                handle_id: None,
                shell_session_id: None,
                child: ident(60_000 + i, 5, 60_000 + i),
                state: ChildState::Running,
                exit_code: None,
                signal: None,
            });
        }
        let mut l = Ledger::new();
        // t1, t2 and t5 are gone (ended); everything else is alive.
        l.restore(carried, &boot(), |pid| {
            (![60_001, 60_002, 60_005].contains(&pid)).then(|| stat(pid, 5))
        });
        assert_eq!(l.len(), MAX_PER_SESSION);
        let ids: HashSet<String> = l.records.iter().map(|r| r.tool_id.clone()).collect();
        assert!(!ids.contains("t1") && !ids.contains("t2") && !ids.contains("t5"));
        assert!(l.truncated.contains_key("s"));
    }
    fn carried(session: &str, i: u32) -> ChildRecord {
        ChildRecord {
            session_id: session.into(),
            tool_id: format!("t{i}"),
            tool_name: "bash".into(),
            handle_id: None,
            shell_session_id: None,
            child: ident(100_000 + i, 5, 100_000 + i),
            state: ChildState::Running,
            exit_code: None,
            signal: None,
        }
    }

    fn fill_running(l: &mut Ledger) {
        let mut pid = 200_000u32;
        for s in 0..(MAX_TOTAL / MAX_PER_SESSION) {
            for _ in 0..MAX_PER_SESSION {
                assert!(l.insert(&format!("full{s}"), spawn("t", ident(pid, 1, pid))));
                pid += 1;
            }
        }
        assert_eq!(l.len(), MAX_TOTAL);
    }

    /// The sweep race: a scan taken before a spawn must not end or drop the
    /// spawn's record (or a host entry added after it), and a later scan that
    /// sees the leader alive keeps it running.
    #[test]
    fn a_sweep_applies_only_to_what_was_there_before_its_scan() {
        let mut l = Ledger::new();
        l.insert("s", spawn("before", ident(90, 5, 90)));
        let t0 = Instant::now();
        let watermark = l.claim_sweep(t0).expect("due");
        // The walk ran here: 90 was gone (a member, 95, held its sid), and
        // 91 did not exist yet.
        let old_scan: Vec<ProcessIdentity> = vec![stat(1, 1), at(95, stat(90, 9))];
        l.insert("s", spawn("after", ident(91, 5, 91)));
        // Spawned and reaped during the walk: its sid looks empty to the walk.
        l.insert("s", spawn("after-ended", ident(93, 5, 93)));
        l.mark_ended(&ident(93, 5, 93), Some(0), None);
        l.add_host(HostOwnerKind::Extension, "late-ext", ident(92, 5, 92));
        l.finish_sweep(Some(&old_scan), watermark, t0);
        let got: Vec<(String, ChildState)> = l.records.iter().map(|r| (r.tool_id.clone(), r.state)).collect();
        assert_eq!(
            got,
            vec![
                ("before".to_string(), ChildState::Ended),
                ("after".to_string(), ChildState::Running),
                ("after-ended".to_string(), ChildState::Ended),
            ],
            "only the record the scan could have seen is judged"
        );
        assert_eq!(l.list(None, None).host.len(), 1, "the host entry added after the scan stays");
        // The next sweep sees 91 and 92 alive: still running, still listed.
        let next = l.claim_sweep(t0 + SWEEP_EVERY).expect("due again");
        l.finish_sweep(Some(&[stat(91, 5), stat(92, 5)]), next, t0 + SWEEP_EVERY);
        assert_eq!(l.records.iter().find(|r| r.child.pid == 91).map(|r| r.state), Some(ChildState::Running));
        assert_eq!(l.list(None, None).host.len(), 1);
    }

    /// A walk never lists a pid forked after it passed that pid's place. A
    /// leader that forks a background job and exits while a sweep's walk
    /// runs leaves a sid that walk reads empty: the sweep ends the record but
    /// keeps it, as it keeps one the tool reaped meanwhile. The next walk
    /// lists the jobs and keeps both; one that finds their sids empty drops
    /// them.
    #[test]
    fn a_sweep_keeps_what_ended_during_it_until_a_later_walk_judges_its_sid() {
        let mut l = Ledger::new();
        l.insert("s", spawn("exited", ident(90, 5, 90)));
        l.insert("s", spawn("reaped", ident(91, 5, 91)));
        let states = |l: &Ledger| l.records.iter().map(|r| (r.tool_id.clone(), r.state)).collect::<Vec<_>>();
        let both_ended = vec![("exited".to_string(), ChildState::Ended), ("reaped".to_string(), ChildState::Ended)];
        let t0 = Instant::now();
        let watermark = l.claim_sweep(t0).expect("due");
        // The walk ran here: 90 and 91 forked 95 and 96 past its last batch
        // and exited, so it lists none of them; the tool reaped 91.
        l.mark_ended(&ident(91, 5, 91), Some(0), None);
        l.finish_sweep(Some(&[stat(1, 1)]), watermark, t0);
        assert_eq!(states(&l), both_ended, "ended during the sweep: kept");
        let t1 = t0 + SWEEP_EVERY;
        let next = l.claim_sweep(t1).expect("due again");
        l.finish_sweep(Some(&[stat(1, 1), at(95, stat(90, 9)), at(96, stat(91, 9))]), next, t1);
        assert_eq!(states(&l), both_ended, "a later walk lists the jobs");
        let t2 = t1 + SWEEP_EVERY;
        let last = l.claim_sweep(t2).expect("due again");
        l.finish_sweep(Some(&[stat(1, 1)]), last, t2);
        assert!(l.records.is_empty(), "ended before this sweep, with an empty sid: dropped");
        // The same for a scan applied with no claim.
        let mut m = Ledger::new();
        m.insert("s", spawn("exited", ident(90, 5, 90)));
        m.apply_sweep(&[], u64::MAX);
        assert_eq!(states(&m), vec![("exited".to_string(), ChildState::Ended)]);
        m.apply_sweep(&[], u64::MAX);
        assert!(m.records.is_empty());
    }

    /// A reply checks each listed running leader now: a zombie (or gone)
    /// leader reads ended in the reply and in the ledger, with a live member
    /// left in its sid; an ended record never reads running again; a code
    /// the tool already recorded shows.
    #[test]
    fn a_reply_ends_a_leader_found_gone_or_a_zombie_and_never_the_reverse() {
        let zombie = |sid: u32, ticks: u64| ProcessIdentity { state: 'Z', ..stat(sid, ticks) };
        let ledger = Mutex::new(Ledger::new());
        {
            let mut l = ledger.lock().unwrap();
            l.insert("s", spawn("zombie", ident(60, 5, 60)));
            l.insert("s", spawn("live", ident(61, 5, 61)));
            l.insert("s", spawn("gone", ident(62, 5, 62)));
            l.insert("s", spawn("reused", ident(63, 5, 63)));
            l.insert("s", spawn("ended", ident(64, 5, 64)));
            l.mark_ended(&ident(64, 5, 64), Some(3), None);
        }
        let stat_of = |pid: u32| match pid {
            60 => Some(zombie(60, 5)),
            61 => Some(stat(61, 5)),
            63 => Some(stat(63, 6)),
            64 => Some(stat(64, 5)),
            _ => None,
        };
        let mut listing = ledger.lock().unwrap().list(None, None);
        validate_leaders_in(&ledger, &mut listing, stat_of);
        attach_members(&mut listing, &[zombie(60, 5), at(70, stat(60, 9))], Some(&boot()));
        let reply: Vec<(&str, ChildState, Option<i32>)> =
            listing.sessions[0].children.iter().map(|c| (c.tool_id.as_str(), c.state, c.exit_code)).collect();
        assert_eq!(
            reply,
            vec![
                ("zombie", ChildState::Ended, None),
                ("live", ChildState::Running, None),
                ("gone", ChildState::Ended, None),
                ("reused", ChildState::Ended, None),
                ("ended", ChildState::Ended, Some(3)),
            ]
        );
        assert_eq!(listing.sessions[0].children[0].members, Some(vec![member(70, 9)]));
        let stored: Vec<ChildState> = ledger.lock().unwrap().records.iter().map(|r| r.state).collect();
        assert_eq!(
            stored,
            vec![ChildState::Ended, ChildState::Running, ChildState::Ended, ChildState::Ended, ChildState::Ended],
            "the ledger holds what the reply said"
        );
    }

    /// The global flags come from the ledger as the fresh check left it: a
    /// full ledger whose leaders are all gone is no longer full once the
    /// reply has ended them.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_checked_reply_at_the_cap_reads_its_flags_after_the_check() {
        let Some(boot) = boot_id() else { return };
        let ledger = Mutex::new(Ledger::new());
        {
            let mut l = ledger.lock().unwrap();
            let mut i = 0u32;
            for s in 0..(MAX_TOTAL / MAX_PER_SESSION) {
                for _ in 0..MAX_PER_SESSION {
                    let pid = u32::MAX - 1 - i; // above any pid_max: never exists
                    let instance = InstanceKey::Linux { boot_id: boot.to_string(), start_ticks: 1 };
                    let child = ChildIdentity::new(pid, pid, pid, instance);
                    assert!(l.insert(&format!("cap{s}"), spawn("t", child)));
                    i += 1;
                }
            }
            assert_eq!(l.len(), MAX_TOTAL);
            assert!(l.list(None, None).ledger_full, "full before the check");
        }
        let mut listing = ledger.lock().unwrap().list(None, None);
        validate_leaders_in(&ledger, &mut listing, read_now);
        assert!(listing.sessions.iter().flat_map(|g| &g.children).all(|c| c.state == ChildState::Ended));
        assert_eq!(listing.sessions.iter().map(|g| g.children.len()).sum::<usize>(), MAX_TOTAL);
        assert!(!listing.ledger_full, "the reply reads the flag after the check");
        assert_eq!(listing.refused_records, 0);
        let stored = ledger.lock().unwrap().list(None, None);
        assert!(stored.sessions.iter().flat_map(|g| &g.children).all(|c| c.state == ChildState::Ended));
        assert!(!stored.ledger_full, "and so does the ledger");
    }

    /// The fresh check never overrides what the tool recorded meanwhile, and
    /// with a sid filter only the listed leaders are read.
    #[test]
    fn a_reply_checks_only_the_listed_leaders_and_keeps_a_reaped_code() {
        let ledger = Mutex::new(Ledger::new());
        {
            let mut l = ledger.lock().unwrap();
            l.insert("s", spawn("a", ident(80, 5, 500)));
            l.insert("s", spawn("b", ident(81, 5, 600)));
        }
        let mut listing = ledger.lock().unwrap().list(None, Some(500));
        // The tool reaps 80 between the listing and the check.
        ledger.lock().unwrap().mark_ended(&ident(80, 5, 500), Some(7), None);
        let read = std::cell::RefCell::new(Vec::new());
        validate_leaders_in(&ledger, &mut listing, |pid| {
            read.borrow_mut().push(pid);
            None
        });
        assert_eq!(*read.borrow(), vec![80], "only the sid's records are checked");
        let c = &listing.sessions[0].children[0];
        assert_eq!((c.state, c.exit_code), (ChildState::Ended, Some(7)), "the reaped code shows");
        assert_eq!(ledger.lock().unwrap().records[1].state, ChildState::Running, "81 was not judged");
    }

    /// Restore goes through the same bounded insertion: 4097 running carried
    /// records keep both caps, the refused one is counted and its session
    /// reads truncated.
    #[test]
    fn restore_of_4097_running_records_holds_the_caps() {
        let per = 241u32; // 17 sessions of 241 = 4097, each under the session cap.
        let records: Vec<ChildRecord> =
            (0..17u32).flat_map(|s| (0..per).map(move |i| carried(&format!("r{s}"), s * per + i))).collect();
        assert_eq!(records.len(), MAX_TOTAL + 1);
        let mut l = Ledger::new();
        l.restore(records, &boot(), |pid| Some(stat(pid, 5)));
        assert_eq!(l.len(), MAX_TOTAL);
        let listing = l.list(None, None);
        assert_eq!(listing.refused_records, 1);
        assert!(listing.ledger_full);
        let last = listing.sessions.iter().find(|g| g.session_id == "r16").expect("r16");
        assert!(last.truncated && last.children.len() == per as usize - 1);
        assert!(listing.sessions.iter().filter(|g| g.truncated).count() == 1, "only the session that lost one");

        // Over the session cap, all running: refused and counted too.
        let mut one = Ledger::new();
        one.restore((0..MAX_PER_SESSION as u32 + 2).map(|i| carried("solo", i)).collect(), &boot(), |pid| {
            Some(stat(pid, 5))
        });
        assert_eq!(one.len(), MAX_PER_SESSION);
        assert_eq!(one.list(None, None).refused_records, 2);
    }

    /// refused_records counts every refusal; ledger_full holds only while the
    /// total cap is reached with nothing ended to evict.
    #[test]
    fn refusals_are_counted_and_a_full_ledger_says_so() {
        let mut l = Ledger::new();
        let empty = l.list(None, None);
        assert_eq!((empty.refused_records, empty.ledger_full), (0, false));
        fill_running(&mut l);
        assert!(l.list(None, None).ledger_full, "at the cap with nothing ended");
        assert!(!l.insert("late", spawn("t", ident(1, 1, 1))));
        assert!(!l.insert("late", spawn("t", ident(2, 1, 2))));
        let full = l.list(None, None);
        assert_eq!((full.refused_records, full.ledger_full), (2, true));
        // One ends: there is something to evict again.
        l.mark_ended(&ident(200_000, 1, 200_000), Some(0), None);
        assert!(!l.list(None, None).ledger_full);
        // Taking its place fills the ledger again, with no refusal.
        assert!(l.insert("late", spawn("t", ident(3, 1, 3))));
        let again = l.list(None, None);
        assert_eq!((again.refused_records, again.ledger_full), (2, true));
        // A sweep that drops ended records clears it too.
        l.mark_ended(&ident(200_001, 1, 200_001), Some(0), None);
        l.mark_ended(&ident(200_002, 1, 200_002), Some(0), None);
        let all_live: Vec<ProcessIdentity> = l
            .records
            .iter()
            .filter(|r| r.state == ChildState::Running)
            .map(|r| at(r.child.pid, stat(r.child.sid, r.child.start_ticks.unwrap())))
            .collect();
        l.apply_sweep(&all_live, u64::MAX);
        assert_eq!(l.len(), MAX_TOTAL - 2);
        assert!(!l.list(None, None).ledger_full);
    }

    /// The truncated set is bounded: sessions with no record left keep their
    /// mark until the next sweep, at most MAX_TRUNCATED_EMPTY of them, and
    /// they show as empty truncated groups (not under a sid filter).
    #[test]
    fn empty_truncated_sessions_are_bounded_and_expire() {
        let mut l = Ledger::new();
        fill_running(&mut l);
        let extra = MAX_TRUNCATED_EMPTY as u32 + 44;
        for i in 0..extra {
            assert!(!l.insert(&format!("e{i}"), spawn("t", ident(i + 1, 1, i + 1))));
        }
        let listing = l.list(None, None);
        let empty: Vec<&str> =
            listing.sessions.iter().filter(|g| g.children.is_empty()).map(|g| g.session_id.as_str()).collect();
        assert_eq!(empty.len(), MAX_TRUNCATED_EMPTY);
        assert_eq!(empty.first().copied(), Some("e44"), "the oldest marks went first");
        assert_eq!(empty.last().copied(), Some(format!("e{}", extra - 1).as_str()));
        assert!(listing.sessions.iter().filter(|g| g.children.is_empty()).all(|g| g.truncated));
        assert!(l.list(None, Some(1)).sessions.is_empty(), "a sid filter lists records only");
        let one = l.list(Some("e299"), None);
        assert_eq!((one.sessions.len(), one.sessions[0].truncated), (1, true));
        // A sweep after the marks expires them; sessions with records keep theirs.
        l.mark_ended(&ident(200_000, 1, 200_000), Some(0), None);
        assert!(l.insert("full0", spawn("t", ident(9, 1, 9))));
        let live: Vec<ProcessIdentity> = l.records.iter().map(|r| at(r.child.pid, stat(r.child.sid, 1))).collect();
        let watermark = l.next_seq;
        l.apply_sweep(&live, watermark);
        let after = l.list(None, None);
        assert!(after.sessions.iter().all(|g| !g.children.is_empty()), "no empty group left");
        assert!(after.sessions.iter().find(|g| g.session_id == "full0").is_some_and(|g| g.truncated));
        assert!(l.truncated.len() <= MAX_TOTAL / MAX_PER_SESSION);
    }

    /// Members: only listed sids are expanded, each up to its cap, and in all
    /// up to the reply cap; a list cut short marks its group truncated.
    #[test]
    fn members_are_capped_per_sid_and_per_reply() {
        let mut l = Ledger::new();
        l.insert("big", spawn("t", ident(1, 1, 1_000_000)));
        l.insert("small", spawn("t", ident(2, 1, 2_000_000)));
        let mut scan: Vec<ProcessIdentity> =
            (0..MAX_MEMBERS_PER_SID as u32 + 5).map(|i| at(10 + i, stat(1_000_000, 1))).collect();
        scan.push(at(5, stat(2_000_000, 1)));
        let mut listing = l.list(None, None);
        attach_members(&mut listing, &scan, Some(&boot()));
        let big = &listing.sessions[0];
        assert_eq!(big.children[0].members.as_ref().map(Vec::len), Some(MAX_MEMBERS_PER_SID));
        assert!(big.truncated, "a cut list marks the group");
        assert!(!listing.sessions[1].truncated);
        assert_eq!(listing.sessions[1].children[0].members, Some(vec![member(5, 1)]));

        // The reply cap: 17 records each with a full sid.
        let mut many = Ledger::new();
        let mut scan = Vec::new();
        for s in 0..17u32 {
            let sid = 3_000_000 + s;
            many.insert(&format!("m{s}"), spawn("t", ident(sid, 1, sid)));
            scan.extend((0..MAX_MEMBERS_PER_SID as u32).map(|i| at(sid * 1000 + i, stat(sid, 1))));
        }
        let mut listing = many.list(None, None);
        attach_members(&mut listing, &scan, Some(&boot()));
        let total: usize =
            listing.sessions.iter().flat_map(|g| &g.children).map(|c| c.members.as_ref().map_or(0, Vec::len)).sum();
        assert_eq!(total, MAX_MEMBERS_PER_REPLY);
        assert!(!listing.sessions[15].truncated && listing.sessions[16].truncated);
        assert_eq!(listing.sessions[16].children[0].members, Some(Vec::new()));
    }

    static WALKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn slow_walk() -> Vec<ProcessIdentity> {
        WALKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        vec![stat(1, 1)]
    }

    /// Concurrent member queries share walks: one runs at a time, and a
    /// query reuses a walk that began after it asked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_member_queries_share_one_walk() {
        let cache: &'static tokio::sync::Mutex<Option<(Instant, Scan)>> = Box::leak(Box::default());
        let first = tokio::spawn(shared_scan_in(cache, slow_walk));
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Eight more arrive while the first walk runs.
        let rest: Vec<_> = (0..8).map(|_| tokio::spawn(shared_scan_in(cache, slow_walk))).collect();
        assert_eq!(first.await.unwrap().len(), 1);
        for r in rest {
            assert_eq!(r.await.unwrap().len(), 1);
        }
        assert_eq!(WALKS.load(std::sync::atomic::Ordering::SeqCst), 2, "the first, then one for all who came after it");
        // A later query never reuses an older walk.
        shared_scan_in(cache, slow_walk).await;
        assert_eq!(WALKS.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// The real case: `bash -c 'sleep … & exit 0'` in its own session. The
    /// leader exits at once (a zombie until reaped) while the background
    /// sleep holds the sid. The reply reads it ended, with the sleep listed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_zombie_leader_with_a_live_member_reads_ended_in_the_reply() {
        use std::os::unix::process::CommandExt;
        if boot_id().is_none() {
            return;
        }
        let mut cmd = std::process::Command::new("bash");
        cmd.args(["-c", "sleep 30 >/dev/null 2>&1 & exit 0"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn bash");
        let pid = child.id();
        let identity = identify(pid).expect("readable while unreaped");
        let session = format!("real-zombie-{pid}");
        record_spawn(&session, spawn("bash", identity.clone()));
        let t0 = Instant::now();
        while read_now(pid).is_some_and(|s| s.is_live()) {
            assert!(t0.elapsed() < Duration::from_secs(10), "the leader never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        let scan = ProcessIdentity::all();
        let listing = list_checked(Some(&session), None, Some(&scan));
        let entry = &listing.sessions[0].children[0];
        assert_eq!(entry.state, ChildState::Ended, "a zombie leader is not running");
        let members = entry.members.clone().unwrap_or_default();
        let stored = list(Some(&session), None).sessions.first().map(|g| g.children[0].state);
        // SAFETY: kill(2) on the child's own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        child.wait().unwrap();
        assert!(!members.is_empty(), "the background sleep is listed as a member");
        assert!(members.iter().all(|m| m.pid != pid));
        assert_eq!(stored, Some(ChildState::Ended), "the ledger says so too");
    }
}
