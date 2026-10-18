//! Spawned-child identity ledger: which daemon session (and which worker)
//! spawned each tool child process.
//!
//! - **Identity** is `(boot_id, pid, start_ticks)`; `start_ticks` is field 22
//!   of `/proc/<pid>/stat`, raw clock ticks since boot. A pid alone is never
//!   an identity. `sid` and `pgid` (fields 6 and 5) come along for the
//!   inspector's attribution rule.
//! - **Who reports.** The `bash` tool and the `shell_start` PTY report each
//!   child they spawn, right after `spawn()` returns (the child is still
//!   unreaped, so its stat can't race a pid reuse), and again when they reap
//!   it. Extension and MCP stdio processes go on the host list (labels only).
//! - **The ledger is the authority** for the daemon's `children` control
//!   frame. The `tool_child` event is sent only after the record is in the
//!   ledger, so a client that queries on receipt of the event finds it.
//! - **Bounds.** At most [`MAX_PER_SESSION`] records per session and
//!   [`MAX_TOTAL`] in all. Over a cap the oldest *ended* record goes first;
//!   a running leader is never dropped. When only running records remain,
//!   the new record is not kept. Either way the session reads
//!   `truncated: true`.
//! - **Retention.** A record is kept while its leader runs. After it ends,
//!   it is kept until a sweep finds no live process left in its session id
//!   (sid). A sweep is one `/proc` walk, run lazily on a spawn or a query at
//!   most once per [`SWEEP_EVERY`], on a blocking thread. Records outlive
//!   their daemon session.
//! - **Reload.** [`snapshot_for_reload`] / [`restore_after_reload`] carry the
//!   records across the daemon's exec. On restore a record is dropped when
//!   its boot id differs, or when its pid is alive with other start ticks
//!   (reused); a record whose pid is gone is kept as ended.
//!
//! Linux only: elsewhere nothing is recorded and lists say
//! `supported: false`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Records kept per daemon session.
pub const MAX_PER_SESSION: usize = 256;
/// Records kept in all.
pub const MAX_TOTAL: usize = 4096;
/// Host-list entries kept (extension and MCP processes).
pub const MAX_HOST: usize = 256;
/// Least time between two sweeps.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// `(boot_id, pid, start_ticks)` plus the process group and session ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub boot_id: String,
    pub sid: u32,
    pub pgid: u32,
}

/// The `/proc/<pid>/stat` fields the ledger uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFields {
    pub ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub start_ticks: u64,
}

/// Parse one `/proc/<pid>/stat` line. The comm (field 2) can hold spaces
/// and `)`, so the fields are split after the *last* `)`: field N is then
/// index N-3.
pub fn parse_stat(text: &str) -> Option<StatFields> {
    let rest = text.get(text.rfind(')')? + 1..)?;
    let mut fields = rest.split_ascii_whitespace();
    let _state = fields.next()?; // field 3
    let ppid = fields.next()?.parse().ok()?; // 4
    let pgid = fields.next()?.parse().ok()?; // 5
    let sid = fields.next()?.parse().ok()?; // 6
    let start_ticks = fields.nth(15)?.parse().ok()?; // 22
    Some(StatFields { ppid, pgid, sid, start_ticks })
}

/// The stat fields of a live (or zombie) pid; `None` when it is gone or
/// unreadable, or off Linux.
pub fn read_stat(pid: u32) -> Option<StatFields> {
    #[cfg(target_os = "linux")]
    {
        parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// `/proc/sys/kernel/random/boot_id`, trimmed; read once per process.
pub fn boot_id() -> Option<&'static str> {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    BOOT.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    })
    .as_deref()
}

/// Clock ticks per second (`sysconf(_SC_CLK_TCK)`), for display only.
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

/// Whether this platform records children at all.
pub fn supported() -> bool {
    cfg!(target_os = "linux") && boot_id().is_some()
}

/// The identity of `pid`, read now. Call it while the child is unreaped.
pub fn identify(pid: u32) -> Option<ChildIdentity> {
    let boot_id = boot_id()?.to_string();
    let stat = read_stat(pid)?;
    Some(ChildIdentity { pid, start_ticks: stat.start_ticks, boot_id, sid: stat.sid, pgid: stat.pgid })
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildMember {
    pub pid: u32,
    pub start_ticks: u64,
}

/// One child in a `child_list` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostChild {
    pub owner_kind: HostOwnerKind,
    pub owner_id: String,
    pub child: ChildIdentity,
}

/// The ledger's part of a `child_list` reply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildListing {
    pub sessions: Vec<SessionChildren>,
    pub host: Vec<HostChild>,
}

/// The ledger itself. The process-wide instance sits behind [`ledger`];
/// tests build their own.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Oldest first, all sessions.
    records: VecDeque<ChildRecord>,
    /// Sessions that lost a record to a cap.
    truncated: HashSet<String>,
    host: Vec<HostChild>,
    last_sweep: Option<Instant>,
    sweeping: bool,
}

fn same_process(a: &ChildIdentity, b: &ChildIdentity) -> bool {
    a.pid == b.pid && a.start_ticks == b.start_ticks && a.boot_id == b.boot_id
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

    fn session_len(&self, session_id: &str) -> usize {
        self.records.iter().filter(|r| r.session_id == session_id).count()
    }

    /// Drop the oldest ended record (of `session_id` when given). The session
    /// that lost it reads truncated. False when there is none to drop.
    fn evict_oldest_ended(&mut self, session_id: Option<&str>) -> bool {
        let found = self.records.iter().position(|r| {
            r.state == ChildState::Ended && session_id.is_none_or(|s| r.session_id == s)
        });
        match found.and_then(|i| self.records.remove(i)) {
            Some(gone) => {
                self.truncated.insert(gone.session_id);
                true
            }
            None => false,
        }
    }

    /// Record a spawn. False when the caps left no room (only running
    /// records remain); the session then reads truncated.
    pub fn insert(&mut self, session_id: &str, spawn: ChildSpawn) -> bool {
        if self.session_len(session_id) >= MAX_PER_SESSION && !self.evict_oldest_ended(Some(session_id)) {
            self.truncated.insert(session_id.to_string());
            return false;
        }
        if self.records.len() >= MAX_TOTAL && !self.evict_oldest_ended(None) {
            self.truncated.insert(session_id.to_string());
            return false;
        }
        self.records.push_back(ChildRecord {
            session_id: session_id.to_string(),
            tool_id: spawn.tool_id,
            tool_name: spawn.tool_name,
            handle_id: spawn.handle_id,
            shell_session_id: spawn.shell_session_id,
            child: spawn.child,
            state: ChildState::Running,
            exit_code: None,
            signal: None,
        });
        true
    }

    /// Mark the record of `child` ended. False when there is none.
    pub fn mark_ended(&mut self, child: &ChildIdentity, exit_code: Option<i32>, signal: Option<i32>) -> bool {
        match self.records.iter_mut().find(|r| same_process(&r.child, child)) {
            Some(record) => {
                record.state = ChildState::Ended;
                record.exit_code = exit_code;
                record.signal = signal;
                true
            }
            None => false,
        }
    }

    /// Apply one `/proc` scan: a running record whose process is gone (or
    /// reused) becomes ended, and an ended record with no live process left
    /// in its sid is dropped.
    pub fn apply_sweep(&mut self, scan: &[(u32, StatFields)]) {
        let live_sids: HashSet<u32> = scan.iter().map(|(_, s)| s.sid).collect();
        let live: HashMap<u32, u64> = scan.iter().map(|(pid, s)| (*pid, s.start_ticks)).collect();
        for record in self.records.iter_mut() {
            if record.state == ChildState::Running && live.get(&record.child.pid) != Some(&record.child.start_ticks) {
                record.state = ChildState::Ended;
            }
        }
        // The kernel never reuses a pid while it still names a session, so a
        // leader's pid alive with other start_ticks means its session is gone
        // and the live sid of that number belongs to someone else.
        let session_alive = |r: &ChildRecord| {
            let leader_reused = r.child.sid == r.child.pid
                && live.get(&r.child.pid).is_some_and(|t| *t != r.child.start_ticks);
            !leader_reused && live_sids.contains(&r.child.sid)
        };
        self.records.retain(|r| r.state == ChildState::Running || session_alive(r));
        self.host.retain(|h| live.get(&h.child.pid) == Some(&h.child.start_ticks));
    }

    /// The records of `session_id` (all when `None`), filtered to `sid` when
    /// given, grouped by session in order of first record, plus the host
    /// list under the same sid filter.
    pub fn list(&self, session_id: Option<&str>, sid: Option<u32>) -> ChildListing {
        let mut sessions: Vec<SessionChildren> = Vec::new();
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
            match sessions.iter_mut().find(|s| s.session_id == record.session_id) {
                Some(group) => group.children.push(entry),
                None => sessions.push(SessionChildren {
                    session_id: record.session_id.clone(),
                    truncated: self.truncated.contains(&record.session_id),
                    children: vec![entry],
                }),
            }
        }
        let host = if session_id.is_some() {
            Vec::new()
        } else {
            self.host.iter().filter(|h| sid.is_none_or(|s| s == h.child.sid)).cloned().collect()
        };
        ChildListing { sessions, host }
    }

    /// Add a host-wide process (bounded; the oldest goes first).
    pub fn add_host(&mut self, owner_kind: HostOwnerKind, owner_id: &str, child: ChildIdentity) {
        self.host.retain(|h| !same_process(&h.child, &child));
        if self.host.len() >= MAX_HOST {
            self.host.remove(0);
        }
        self.host.push(HostChild { owner_kind, owner_id: owner_id.to_string(), child });
    }

    /// Remove a host-wide process.
    pub fn remove_host(&mut self, child: &ChildIdentity) {
        self.host.retain(|h| !same_process(&h.child, child));
    }

    /// The records to carry across a reload (already within the caps).
    pub fn snapshot(&self) -> Vec<ChildRecord> {
        self.records.iter().cloned().collect()
    }

    /// Restore carried records: drop those from another boot or whose pid is
    /// alive with other start ticks (reused); keep the rest, ended when the
    /// pid is gone; then apply the caps, oldest ended first.
    pub fn restore(
        &mut self,
        carried: Vec<ChildRecord>,
        current_boot: &str,
        stat_of: impl Fn(u32) -> Option<StatFields>,
    ) {
        for mut record in carried {
            if record.child.boot_id != current_boot {
                continue;
            }
            match stat_of(record.child.pid) {
                Some(stat) if stat.start_ticks != record.child.start_ticks => continue,
                Some(_) => {}
                None => record.state = ChildState::Ended,
            }
            if self.records.iter().any(|r| same_process(&r.child, &record.child)) {
                continue;
            }
            self.records.push_back(record);
        }
        self.enforce_caps();
    }

    fn enforce_caps(&mut self) {
        let mut per_session: HashMap<String, usize> = HashMap::new();
        for record in &self.records {
            *per_session.entry(record.session_id.clone()).or_default() += 1;
        }
        for (session, count) in per_session {
            for _ in MAX_PER_SESSION..count {
                if !self.evict_oldest_ended(Some(&session)) {
                    break;
                }
            }
        }
        while self.records.len() > MAX_TOTAL && self.evict_oldest_ended(None) {}
    }

    /// Whether a sweep is due now (and claim it).
    fn claim_sweep(&mut self, now: Instant) -> bool {
        if self.sweeping || self.last_sweep.is_some_and(|t| now.duration_since(t) < SWEEP_EVERY) {
            return false;
        }
        self.sweeping = true;
        true
    }

    fn finish_sweep(&mut self, scan: Option<&[(u32, StatFields)]>, now: Instant) {
        if let Some(scan) = scan {
            self.apply_sweep(scan);
        }
        self.sweeping = false;
        self.last_sweep = Some(now);
    }
}

/// Every pid in `/proc` with its stat fields (one walk). Empty off Linux.
pub fn scan_proc() -> Vec<(u32, StatFields)> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            if let Some(stat) = read_stat(pid) {
                out.push((pid, stat));
            }
        }
    }
    out
}

/// Fill `members` for every entry: the live processes in its sid.
pub fn attach_members(listing: &mut ChildListing, scan: &[(u32, StatFields)]) {
    let mut by_sid: HashMap<u32, Vec<ChildMember>> = HashMap::new();
    for (pid, stat) in scan {
        by_sid.entry(stat.sid).or_default().push(ChildMember { pid: *pid, start_ticks: stat.start_ticks });
    }
    for group in &mut listing.sessions {
        for entry in &mut group.children {
            entry.members = Some(by_sid.get(&entry.child.sid).cloned().unwrap_or_default());
        }
    }
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
    if !lock().claim_sweep(Instant::now()) {
        return;
    }
    rt.spawn_blocking(|| {
        let scan = scan_proc();
        lock().finish_sweep(Some(&scan), Instant::now());
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

/// A listing from the process-wide ledger (see [`Ledger::list`]).
pub fn list(session_id: Option<&str>, sid: Option<u32>) -> ChildListing {
    let listing = lock().list(session_id, sid);
    maybe_sweep();
    listing
}

/// The records to put in the reload state.
pub fn snapshot_for_reload() -> Vec<ChildRecord> {
    lock().snapshot()
}

/// Restore the records carried in the reload state.
pub fn restore_after_reload(carried: Vec<ChildRecord>) {
    let Some(boot) = boot_id() else {
        return;
    };
    lock().restore(carried, boot, read_stat);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "680bd989-b584-4a03-8f69-995bf66dd28c";

    fn ident(pid: u32, start_ticks: u64, sid: u32) -> ChildIdentity {
        ChildIdentity { pid, start_ticks, boot_id: BOOT.into(), sid, pgid: sid }
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

    fn stat(sid: u32, start_ticks: u64) -> StatFields {
        StatFields { ppid: 1, pgid: sid, sid, start_ticks }
    }

    fn stat_line(comm: &str, ppid: u32, pgid: u32, sid: u32, start: u64) -> String {
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt
        // majflt cmajflt utime stime cutime cstime priority nice threads
        // itrealvalue starttime vsize ...
        format!("4242 ({comm}) S {ppid} {pgid} {sid} 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 {start} 16633856 1726 18446744073709551615")
    }

    #[test]
    fn stat_fields_come_from_after_the_last_paren() {
        let line = stat_line("a b) c) S 9 9 9", 77, 88, 99, 26981644);
        assert_eq!(parse_stat(&line), Some(StatFields { ppid: 77, pgid: 88, sid: 99, start_ticks: 26981644 }));
        assert_eq!(
            parse_stat(&stat_line("cat", 1, 2, 3, 4)),
            Some(StatFields { ppid: 1, pgid: 2, sid: 3, start_ticks: 4 })
        );
    }

    #[test]
    fn a_short_or_broken_stat_line_is_none() {
        assert_eq!(parse_stat("4242 (cat) S 1 2 3"), None);
        assert_eq!(parse_stat("4242 cat S 1 2 3 0 -1"), None);
        assert_eq!(parse_stat(&stat_line("cat", 1, 2, 3, 4).replace(" 4 1663", " x 1663")), None);
        assert_eq!(read_stat(u32::MAX - 7), None, "no such pid");
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
        assert_eq!(id.start_ticks.to_string(), fields[19], "field 22");
        assert_eq!(id.sid.to_string(), fields[3], "field 6");
        assert_eq!(id.pgid.to_string(), fields[2], "field 5");
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        assert_eq!(id.boot_id, boot.trim());
        assert!(supported());
        assert!(clk_tck() > 0);
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
        assert!(l.list(Some("fresh"), None).sessions.is_empty());
        assert!(l.truncated.contains("fresh"));
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
        let scan = vec![(77, stat(21, 9)), (22, stat(22, 5)), (23, stat(23, 99)), (31, stat(31, 5))];
        l.apply_sweep(&scan);
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
        assert!(l.claim_sweep(t0));
        assert!(!l.claim_sweep(t0), "one in flight");
        l.finish_sweep(None, t0);
        assert!(!l.claim_sweep(t0 + SWEEP_EVERY - Duration::from_millis(1)));
        assert!(l.claim_sweep(t0 + SWEEP_EVERY));
    }

    #[test]
    fn members_are_the_live_processes_in_each_records_sid() {
        let mut l = Ledger::new();
        l.insert("s", spawn("t", ident(40, 5, 40)));
        l.mark_ended(&ident(40, 5, 40), Some(0), None);
        let mut listing = l.list(None, None);
        attach_members(&mut listing, &[(41, stat(40, 7)), (42, stat(99, 7))]);
        assert_eq!(
            listing.sessions[0].children[0].members,
            Some(vec![ChildMember { pid: 41, start_ticks: 7 }])
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
        other_boot.boot_id = "another-boot".into();
        old.insert("s", spawn("other-boot", other_boot));
        let carried: Vec<ChildRecord> =
            serde_json::from_str(&serde_json::to_string(&old.snapshot()).unwrap()).unwrap();
        assert_eq!(carried, old.snapshot(), "a JSON round trip");
        let mut new = Ledger::new();
        new.restore(carried, BOOT, |pid| match pid {
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
        l.restore(carried, BOOT, |pid| {
            (![60_001, 60_002, 60_005].contains(&pid)).then(|| stat(pid, 5))
        });
        assert_eq!(l.len(), MAX_PER_SESSION);
        let ids: HashSet<String> = l.records.iter().map(|r| r.tool_id.clone()).collect();
        assert!(!ids.contains("t1") && !ids.contains("t2") && !ids.contains("t5"));
        assert!(l.truncated.contains("s"));
    }
}
