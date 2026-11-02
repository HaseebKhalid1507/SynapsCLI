//! The crash record (`<run dir>/daemon[-P].live.json`): what a fresh daemon
//! needs to put every session back after the previous one died without a
//! reload hand-off (a crash, `kill -9`, an abort).
//!
//! The body is the reload state's per-session record ([`ReloadSession`]),
//! plus whether a turn was running and which workers were running, plus the
//! writer's process identity (`pid`, start time, boot id). No secrets: the
//! same fields the reload state carries.
//!
//! - **Writing.** Each actor publishes its entry into its handle
//!   (`session::live`) at create, at every turn start and end, on park and
//!   unpark and after a settings change; host adopt/remove marks the record
//!   dirty too. The writer task ([`spawn_writer`]) rewrites the whole record
//!   atomically (temp file, `fsync`, rename): at once after a turn start,
//!   otherwise at most once a second. A missing file is written back within
//!   [`HEAL_EVERY`] (an older `synaps` listing may unlink run-dir `*.json`
//!   it can't parse).
//! - **A reload** hands off with its own state file. Just before the exec,
//!   the old image writes the record from that state ([`Recorder::hand_off`],
//!   marked with the reload's generation) and stops writing, so a new image
//!   that dies before its first write still leaves a record the next start
//!   can restore from, with the reload's cut turns resumed as a reload's.
//!   A failed exec lets the writer go on ([`Recorder::resume_writes`]).
//! - **A clean stop** (`Daemon::wait`, before the sessions end) keeps the
//!   record and writes it one last time from checkpoints
//!   ([`record_stop`]): every live session is checkpointed as a reload
//!   does (a running turn is cut, its partial reply and `Restart` marker
//!   kept, its workers stopped resumably), and the record names them,
//!   Live or Parked, marked as a stop with each checkpoint's `turn_cut`.
//!   The next start restores them all and resumes once the turns THIS
//!   stop cut, as after a reload (the stop's own record decides, never the
//!   history alone). `daemon.crash_restore = off` before that start gives
//!   a quiet start (the record is removed, nothing restored).
//! - **A fresh start** (no reload state) with `daemon.crash_restore` on reads
//!   the record before it listens ([`prepare_restore`]: no session touched
//!   yet). A record whose writer still runs (or can't be checked) is moved
//!   aside (`*.live.json.skipped-<time>`); after [`MAX_RESTORE_ATTEMPTS`]
//!   starts that died restoring it, it is moved aside as
//!   `*.live.json.quarantined-<time>`. Otherwise the recorded sessions that
//!   persist and still have their history on disk are put in the restore set
//!   (`DaemonState::restoring`; a parked one past its eviction age is left on
//!   disk), the daemon signals ready and listens, and THEN the restore runs
//!   ([`spawn_restore`]): each session is rehydrated from its journal
//!   through the reload's per-session path, 16 at a time, each bounded by
//!   [`REHYDRATE_CAP`] once its create holds its session's create lock (the
//!   host's per-session create reservation) and by [`REHYDRATE_QUEUE_CAP`]
//!   while it waits for that lock; parked ones come
//!   back parked. Until a session is
//!   back, listings show it from the record with `restoring: true`, and a
//!   request naming it waits on its gate (up to [`GATE_WAIT`], then
//!   "session <id> is still restoring"). A session that fails to load, whose
//!   journal is locked or that runs past either cap is given up: it leaves the
//!   listing, its gate opens with "session <id> could not be restored:
//!   <reason>", and its entry stays carried in the record. A session whose
//!   turn the crash cut is resumed once (`SessionCommand::ResumeAfterCrash`),
//!   except when the same session's last restore resume came less than
//!   [`CRASH_LOOP_WINDOW`] before this crash (a crash loop is suspected).
//!   A start counts as one that died restoring until the restore's summary
//!   line. A stop while the restore runs records the entries it had not
//!   finished unchanged.
//!   `daemon.crash_restore = off`: the record is never read or written, and a
//!   leftover one is removed, so a later `on` can't restore from a stale one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::registry::DaemonPaths;
use super::reload::{self, After, ReloadSession, ReloadState, Rehydrated, LOG_TARGET};
use super::DaemonState;
use crate::proc_identity::{ProcessIdentity, StartKey};
use crate::session::{live, SessionLifecycle};

/// Upper bound for reading a record (the file is the daemon's own, 0600 in
/// its 0700 run dir; anything larger is not restored from).
const MAX_BYTES: u64 = 16 * 1024 * 1024;
/// At most one write a second, except after a turn start.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// How often the writer checks that the file is still there.
pub const HEAL_EVERY: Duration = Duration::from_secs(30);
/// The most sessions rehydrated at once by a crash restore
/// ([`restore_lanes`] bounds it by the host's cores).
pub const RESTORE_CONCURRENCY: usize = 16;

/// The sessions a crash restore rehydrates at once on this host: at most
/// [`RESTORE_CONCURRENCY`], and at most half its cores (at least 2). Each
/// build does blocking work on an async worker (journal loads, the endpoint
/// start), so a small host keeps workers free for the accept loop and its
/// live sessions while it restores.
pub fn restore_lanes() -> usize {
    lanes_for(std::thread::available_parallelism().map_or(2, |n| n.get()))
}

fn lanes_for(cores: usize) -> usize {
    RESTORE_CONCURRENCY.min((cores / 2).max(2))
}
/// How long the restore summary waits for the sessions' resume answers.
const RESUME_REPLY_WAIT: Duration = Duration::from_secs(10);
/// Starts that died while restoring from the same record before a restore
/// stops trying (a session whose restore kills the daemon must not keep
/// every later start down).
const MAX_RESTORE_ATTEMPTS: u32 = 2;
/// Record format version.
const VERSION: u32 = 1;

/// `daemon[-P].live.json` next to `daemon[-P].json`.
pub fn path(paths: &DaemonPaths) -> PathBuf {
    let stem = paths
        .json
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "daemon".into());
    paths.dir.join(format!("{stem}.live.json"))
}

/// The process that wrote a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Writer {
    pub pid: u32,
    /// The writer's start as [`StartKey::number`] gives it: on Linux
    /// `/proc/<pid>/stat` field 22, on macOS the kernel's unique id.
    pub start_ticks: u64,
    #[serde(default)]
    pub boot_id: Option<String>,
}

impl Writer {
    /// This process (`None` where processes can't be identified).
    pub fn current() -> Option<Self> {
        let pid = std::process::id();
        let process = ProcessIdentity::identify(pid).ok()?;
        Some(Self {
            pid,
            start_ticks: process.start.number(),
            boot_id: ProcessIdentity::boot_key().map(|boot| boot.as_str().to_string()),
        })
    }

    /// The key a turn draft carries (`TurnDraft::writer`,
    /// `session::live::process_key`).
    pub fn key(&self) -> String {
        live::key_of(self.pid, self.start_ticks, self.boot_id.as_deref())
    }

    /// Whether the writer still runs. Another boot: no. Gone, a zombie or
    /// the pid reused by another process: no. Anything that can't be read
    /// counts as running, so a doubt never restores sessions a live daemon
    /// may still serve.
    pub fn is_alive(&self) -> bool {
        if let (Some(theirs), Some(ours)) = (self.boot_id.as_deref(), ProcessIdentity::boot_key()) {
            if theirs != ours.as_str() {
                return false;
            }
        }
        let start = StartKey::from_number(self.start_ticks);
        !matches!(ProcessIdentity::exit_of(self.pid, Some(start)), Ok(Some(_)))
    }
}

/// One recorded session: the reload's record plus what a crash leaves open.
/// Written as derived; read through [`LiveSessionWire`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "LiveSessionWire")]
pub struct LiveSession {
    #[serde(flatten)]
    pub session: ReloadSession,
    /// A turn was running at the writer's last write.
    #[serde(default)]
    pub turn_in_flight: bool,
    /// Handle ids of the workers that were running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running_workers: Vec<String>,
    /// An entry a restore had not finished when this record was written,
    /// carried unchanged: the record it came from, which decides its resume
    /// (`None`: the record's own `writer` / `reload` decide).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// When a crash or stop restore last resumed this session's cut turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the session parked (a Parked entry only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_since: Option<chrono::DateTime<chrono::Utc>>,
    /// The session's name and creation time as listings showed them (a
    /// listing shows a session still restoring from these).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// How a [`LiveSession`] is read: the same keys, with the session's
/// `capacity_stop` taken as this entry's own field, outside the flatten. A
/// flatten first buffers every key it does not know as a decoded value, so a
/// recorded resume past the JSON reader's limits (a number out of range,
/// nesting past its depth) would fail the whole record there, before
/// `capacity_resume::lenient_stop` ran; read here, it drops only itself.
/// Writing is unchanged (the flattened session writes `capacity_stop`), and
/// so are the bytes an entry reads from.
#[derive(Deserialize)]
struct LiveSessionWire {
    #[serde(flatten)]
    session: ReloadSession,
    #[serde(default, deserialize_with = "crate::session::capacity_resume::lenient_stop")]
    capacity_stop: Option<crate::session::CapacityStop>,
    #[serde(default)]
    turn_in_flight: bool,
    #[serde(default)]
    running_workers: Vec<String>,
    #[serde(default)]
    origin: Option<Origin>,
    #[serde(default)]
    resumed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    parked_since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<LiveSessionWire> for LiveSession {
    fn from(wire: LiveSessionWire) -> Self {
        let LiveSessionWire {
            mut session,
            capacity_stop,
            turn_in_flight,
            running_workers,
            origin,
            resumed_at,
            parked_since,
            name,
            created_at,
        } = wire;
        // The flatten never sees the key (this entry took it).
        session.capacity_stop = capacity_stop;
        Self { session, turn_in_flight, running_workers, origin, resumed_at, parked_since, name, created_at }
    }
}

/// The record a carried entry came from (see [`LiveSession::origin`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// A crash: the dead daemon's process key and its last write.
    Crash {
        writer: String,
        crashed_at: chrono::DateTime<chrono::Utc>,
    },
    /// A reload's or a clean stop's hand-off.
    HandOff(HandOff),
}

/// A crash within this long of the restore that last resumed a session's
/// cut turn suggests that turn takes the daemon down: the next restore puts
/// the session back without resuming it.
pub const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Whether a crash at `crashed_at` follows the session's last restore
/// resume (`resumed_at`) closely enough to suspect a crash loop.
pub fn crash_loop_suspected(
    resumed_at: Option<chrono::DateTime<chrono::Utc>>,
    crashed_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    let window = chrono::Duration::seconds(CRASH_LOOP_WINDOW.as_secs() as i64);
    resumed_at.is_some_and(|r| crashed_at - r < window)
}

/// Whether a session recorded Parked since `parked_since` is past its
/// eviction age `evict` at `now` (it would have left the map by now, so a
/// restore leaves it on disk; `--continue` still brings it back).
pub fn parked_past_eviction(
    parked_since: Option<chrono::DateTime<chrono::Utc>>,
    evict: Option<Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match (parked_since, evict) {
        (Some(since), Some(evict)) => (now - since).to_std().is_ok_and(|parked| parked >= evict),
        _ => false,
    }
}

/// The record was written by a reload just before its exec, or by a clean
/// stop (`stop`) just before its sessions ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandOff {
    pub generation: u64,
    /// When the reload (or the stop) wrote the record.
    pub reloaded_at: chrono::DateTime<chrono::Utc>,
    /// A clean stop's record, not a reload's.
    #[serde(default)]
    pub stop: bool,
}

/// `daemon[-P].live.json` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashRecord {
    pub version: u32,
    pub writer: Writer,
    pub written_at: chrono::DateTime<chrono::Utc>,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<HandOff>,
    pub sessions: Vec<LiveSession>,
    /// The subagent handle counter at the write.
    #[serde(default)]
    pub next_subagent_id: u64,
    /// Starts that began restoring from this record (and died doing it,
    /// since a start that lives rewrites the record as its own).
    #[serde(default)]
    pub restore_attempts: u32,
}

impl CrashRecord {
    /// The first handle number a restoring daemon may use.
    fn subagent_id_floor(&self) -> u64 {
        let running = self
            .sessions
            .iter()
            .flat_map(|s| s.running_workers.iter().map(String::as_str).chain(s.session.workers.iter().map(|w| w.id.as_str())))
            .filter_map(crate::runtime::subagent::handle_number)
            .max()
            .map_or(0, |n| n + 1);
        self.next_subagent_id.max(running)
    }
}

/// The resume message for a session the crash touched. `turn_cut`: the
/// crash cut its turn; else it was idle while `lost_workers` ran.
pub fn resume_text(
    crashed_at: chrono::DateTime<chrono::Utc>,
    turn_cut: bool,
    lost_workers: &[String],
) -> String {
    let at = crashed_at.format("%H:%M");
    let workers = lost_workers.join(", ");
    if turn_cut {
        let mut text = format!(
            "resume: the daemon restarted after a crash mid-turn ({at} UTC); your last turn was \
             cut, so check your last tool call before trusting it, then continue."
        );
        if !lost_workers.is_empty() {
            text.push_str(&format!(
                " The crash also stopped your running subagents ({workers}); no completion wake \
                 will come for them."
            ));
        }
        text
    } else {
        format!(
            "resume: the daemon restarted after a crash ({at} UTC) while your subagents \
             ({workers}) were running; the crash stopped them, so no completion wake will come \
             for them. Check what they finished before trusting it, then continue."
        )
    }
}

/// The record as this daemon sees its sessions now. Handles that never
/// published (echo actors, lock-held placeholders), ending ones and those
/// that don't persist (nothing to rehydrate from) are left out. While a
/// restore runs, the entries it has not finished are carried unchanged and
/// the record keeps the restore's attempt count (a start that dies before
/// the restore's summary counts as one that died restoring).
fn snapshot(state: &DaemonState, writer: &Writer) -> (CrashRecord, Vec<(live::LiveSlot, u64)>) {
    let mut sessions = Vec::new();
    let mut stamps = Vec::new();
    for h in state.live_sessions() {
        let Some(entry) = h.live_entry() else {
            continue;
        };
        let lifecycle = h.lifecycle();
        if lifecycle == SessionLifecycle::Ending || !h.is_alive() || !entry.record.config.persist {
            continue;
        }
        let mut session = reload::record(&h, Some(entry.record.clone()));
        session.lifecycle = lifecycle;
        // A reload's own notion; a crash record never claims it.
        session.turn_cut = None;
        stamps.push((h.live_slot().clone(), entry.gen));
        sessions.push(LiveSession {
            session,
            turn_in_flight: entry.turn_in_flight,
            running_workers: entry.running_workers.clone(),
            origin: None,
            resumed_at: entry.resumed_at,
            parked_since: entry.parked_since,
            name: h.name(),
            created_at: Some(h.meta().created_at),
        });
    }
    let (carried, attempts) = state.restoring.carried();
    let sessions = with_carried(sessions, carried);
    let record = CrashRecord {
        version: VERSION,
        writer: writer.clone(),
        written_at: chrono::Utc::now(),
        generation: state.generation,
        reload: None,
        sessions,
        next_subagent_id: crate::tools::peek_next_subagent_id(),
        restore_attempts: attempts,
    };
    (record, stamps)
}

/// A record's sessions: the live actors' own entries, then every carried
/// entry (not restored yet, or given up) that no actor's entry stands for.
/// An actor always wins over a carried entry with its id (a given-up
/// session a client has loaded since is recorded from its actor).
fn with_carried(mut sessions: Vec<LiveSession>, carried: Vec<LiveSession>) -> Vec<LiveSession> {
    let mut seen: std::collections::HashSet<String> =
        sessions.iter().map(|s| s.session.id.clone()).collect();
    sessions.extend(carried.into_iter().filter(|c| seen.insert(c.session.id.clone())));
    sessions
}

/// Write `body` to `path` through `<path>.tmp` (created exclusively, never
/// followed, 0600), `fsync` it, rename it over `path`, then `fsync` the
/// directory (best effort).
fn write_durable(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let open = || {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        options.open(&tmp)
    };
    let mut file = match open() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            open()?
        }
        opened => opened?,
    };
    let written = file.write_all(body).and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return renamed;
    }
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// The record at `path`, if there is a readable regular file of at most
/// [`MAX_BYTES`] there. `Err` names why a file that exists was not used.
pub fn read(path: &Path) -> Result<Option<CrashRecord>, String> {
    use std::io::Read;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(m) if !m.file_type().is_file() => return Err("not a regular file".into()),
        Ok(_) => {}
    }
    let file = reload::open_regular_file(path).ok_or("not a readable regular file")?;
    let mut body = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
    if body.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    serde_json::from_slice(&body).map(Some).map_err(|e| reload::parse_failure(&e))
}

#[derive(Debug, Default)]
struct Gate {
    /// A reload or a clean stop wrote its hand-off: nothing is written
    /// after it (until a reload's exec fails).
    handed_off: bool,
    /// The last write failed (logged once until a write succeeds).
    failing: bool,
    /// The change count (`live::changes_now`) the last written update's
    /// snapshot started at: an update whose snapshot started earlier is
    /// stale (a newer one is on disk) and is never written over it.
    written: u64,
}

/// This daemon's writer side. Every write runs under one mutex, so a
/// periodic write already under way can't replace a reload's or a stop's
/// hand-off once it is written.
#[derive(Debug)]
pub struct Recorder {
    path: PathBuf,
    writer: Writer,
    gate: Mutex<Gate>,
}

impl Recorder {
    /// `None` where this process can't be named (no `/proc`): no record.
    pub fn new(paths: &DaemonPaths) -> Option<Self> {
        Some(Self { path: path(paths), writer: Writer::current()?, gate: Mutex::default() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// The last write failed (the writer retries at its next heal tick).
    fn failing(&self) -> bool {
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).failing
    }

    fn write_gated(&self, body: &[u8], why: &str) {
        self.write_at(body, why, None, &[]);
    }

    /// Write `body` under the gate; `true` when it was written. `started`:
    /// an update's change count, read before its snapshot (a stale one is
    /// skipped); `None`: a hand-off, which always writes and stops updates.
    /// `stamps`: the entries an update holds, acknowledged
    /// (`LiveSlot::stamp_durable`) only when it was written, still under
    /// the gate, so the acknowledgements follow the order of the writes
    /// (a watch send: nothing here waits for an actor).
    fn write_at(
        &self,
        body: &[u8],
        why: &str,
        started: Option<u64>,
        stamps: &[(live::LiveSlot, u64)],
    ) -> bool {
        let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if gate.handed_off && why != "hand-off" {
            return false;
        }
        if let Some(c) = started.filter(|c| *c < gate.written) {
            tracing::debug!(target: LOG_TARGET, started = c, written = gate.written, "crash record: a stale snapshot skipped (a newer one is on disk)");
            return false;
        }
        let written = match write_durable(&self.path, body) {
            Ok(()) => {
                gate.failing = false;
                if let Some(c) = started {
                    gate.written = gate.written.max(c);
                    for (slot, gen) in stamps {
                        slot.stamp_durable(c, *gen);
                    }
                }
                true
            }
            Err(e) => {
                if !gate.failing {
                    tracing::warn!(target: LOG_TARGET, path = %self.path.display(), error = %e, why, "crash record: write failed");
                }
                gate.failing = true;
                false
            }
        };
        if why == "hand-off" {
            gate.handed_off = true;
        }
        written
    }

    /// Write an update whose snapshot started at change count `started` and
    /// holds `stamps` (each session's slot and the entry `gen` it holds);
    /// only a write that succeeded acknowledges them
    /// (`LiveSlot::stamp_durable`). Refused, stale or failed: nothing is
    /// acknowledged.
    fn write_snapshot(&self, body: &[u8], started: u64, stamps: &[(live::LiveSlot, u64)]) -> bool {
        self.write_at(body, "update", Some(started), stamps)
    }

    /// Snapshot `state` and write it (off the runtime threads). The change
    /// count is read BEFORE the snapshot: every entry published up to it is
    /// in the snapshot, so the writer can order snapshots by it.
    pub async fn write_now(self: &Arc<Self>, state: &DaemonState) {
        let started = live::changes_now();
        let (record, stamps) = snapshot(state, &self.writer);
        let Ok(body) = serde_json::to_vec(&record) else {
            return;
        };
        let me = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || me.write_snapshot(&body, started, &stamps)).await;
    }

    /// A reload is about to exec: write the record from its state (the cut
    /// turns as the reload recorded them) and stop writing.
    pub fn hand_off(&self, rs: &ReloadState) {
        let record = CrashRecord {
            version: VERSION,
            writer: self.writer.clone(),
            written_at: rs.written_at,
            generation: rs.generation,
            reload: Some(HandOff { generation: rs.generation, reloaded_at: rs.written_at, stop: false }),
            sessions: rs
                .sessions
                .iter()
                .filter(|s| s.config.persist)
                .map(|s| LiveSession {
                    session: s.clone(),
                    turn_in_flight: false,
                    running_workers: Vec::new(),
                    origin: None,
                    resumed_at: None,
                    parked_since: None,
                    name: None,
                    created_at: None,
                })
                .collect(),
            next_subagent_id: rs.next_subagent_id,
            restore_attempts: 0,
        };
        if let Ok(body) = serde_json::to_vec(&record) {
            self.write_gated(&body, "hand-off");
        }
    }

    /// The reload's exec failed: this image goes on, and so does its record.
    pub fn resume_writes(&self) {
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).handed_off = false;
        live::mark_dirty(true);
    }
}

/// How long a start waits for a stop record's writer to finish exiting
/// (it released the daemon lock as its last step, so it serves nothing any
/// more; waiting only spares its last journal locks a placeholder).
const STOP_EXIT_WAIT: Duration = Duration::from_secs(3);

/// Clean stop ([`super::Daemon::wait`], before the sessions end): keep the
/// record for the next start. Every live session that persists is
/// checkpointed as a reload does: a running turn is cut at once (partial
/// reply and `Restart` marker kept, the checkpoint records `turn_cut`), its
/// workers are stopped resumably and carried. The record is written from
/// those checkpoints, marked as a stop, and nothing is written after it.
/// The next start restores the sessions, Live or Parked as recorded, and
/// resumes once the turns THIS stop cut (its own `turn_cut`, never the
/// history alone). `force`: no checkpoints (a forced stop doesn't wait);
/// the sessions are recorded from their last published entries with no
/// `turn_cut`, so they come back but nothing is resumed. A session whose
/// checkpoint doesn't confirm in time is recorded the same way.
pub async fn record_stop(state: &Arc<DaemonState>, force: bool) {
    let Some(rec) = state.crash_record.clone() else {
        return;
    };
    // Before any entry is read: no capacity resume is dispatched from here
    // on, so the stop record never holds a resume that was also given.
    if let Some(link) = state.host.record_link() {
        link.seal();
    }
    // A restore still running stops first; what it had not finished is
    // carried unchanged (its origin keeps its one resume for next time).
    state.restoring.stop().await;
    let (carried, _) = state.restoring.carried();
    let handles: Vec<_> = state
        .live_sessions()
        .into_iter()
        .filter(|h| {
            h.is_alive()
                && h.lifecycle() != SessionLifecycle::Ending
                && h.live_entry().is_some_and(|e| e.record.config.persist)
        })
        .collect();
    let replies = if force {
        handles.iter().map(|_| None).collect::<Vec<_>>()
    } else {
        // A stop has no drain: every turn keeps its quiesce grace.
        futures::future::join_all(handles.iter().cloned().map(|h| reload::checkpoint_one(h, false))).await
    };
    let mut sessions = Vec::with_capacity(handles.len() + carried.len());
    let (mut cut, mut unconfirmed) = (0usize, 0usize);
    for (h, reply) in handles.iter().zip(replies) {
        let entry = h.live_entry();
        let (resumed_at, parked_since) =
            entry.as_ref().map_or((None, None), |e| (e.resumed_at, e.parked_since));
        let session = match reply {
            // A session the restore was still bringing back when the stop
            // came: unless this stop cut a turn of its own, the carried
            // entry stays (its resume, if not given yet, is given next time;
            // one already given is not repeated). A given-up session that
            // is live is recorded from its actor.
            Some(r) if state.restoring.stop_keeps_carried(h.id.as_str(), r.turn_cut) => continue,
            Some(r) => {
                cut += usize::from(r.turn_cut);
                reload::record(h, Some(r))
            }
            None if state.restoring.stop_keeps_carried(h.id.as_str(), false) => continue,
            None => {
                let Some(entry) = entry else {
                    continue;
                };
                if !force {
                    unconfirmed += 1;
                    tracing::warn!(target: LOG_TARGET, session = %h.id, "crash record: the stop's checkpoint did not confirm; recorded without a turn_cut (not resumed)");
                }
                let mut s = reload::record(h, Some(entry.record.clone()));
                s.lifecycle = h.lifecycle();
                s.turn_cut = None;
                s
            }
        };
        sessions.push(LiveSession {
            session,
            turn_in_flight: false,
            running_workers: Vec::new(),
            origin: None,
            resumed_at,
            parked_since,
            name: h.name(),
            created_at: Some(h.meta().created_at),
        });
    }
    let carried_n = carried.len();
    let sessions = with_carried(sessions, carried);
    let now = chrono::Utc::now();
    let n = sessions.len();
    let record = CrashRecord {
        version: VERSION,
        writer: rec.writer.clone(),
        written_at: now,
        generation: state.generation,
        reload: Some(HandOff { generation: state.generation, reloaded_at: now, stop: true }),
        sessions,
        // After every checkpoint: no session can launch a worker any more.
        next_subagent_id: crate::tools::peek_next_subagent_id(),
        restore_attempts: 0,
    };
    let Ok(body) = serde_json::to_vec(&record) else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || rec.write_gated(&body, "hand-off")).await;
    tracing::info!(
        target: LOG_TARGET,
        sessions = n,
        carried = carried_n,
        turns_cut = cut,
        unconfirmed,
        force,
        "crash record: written for the stop; the next start restores these sessions"
    );
}

/// Wait (bounded) for a stop record's writer to finish exiting.
async fn wait_for_exit(writer: &Writer) {
    let t0 = Instant::now();
    while writer.is_alive() && t0.elapsed() < STOP_EXIT_WAIT {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Move a record aside for diagnosis (`<name>.<tag>-<UTC time>`).
fn move_aside(p: &Path, tag: &str) -> std::io::Result<PathBuf> {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{tag}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ")));
    let q = p.with_file_name(name);
    std::fs::rename(p, &q)?;
    Ok(q)
}

/// [`move_aside`] for a log line: where the record went, or why it stayed.
fn moved_aside(p: &Path, tag: &str) -> String {
    match move_aside(p, tag) {
        Ok(q) => q.display().to_string(),
        Err(e) => format!("(not moved: {e})"),
    }
}

/// Keep the record current until shutdown: a first write at once, then a
/// write per change, at once after a turn start and otherwise at most once
/// a [`DEBOUNCE`]; the file is put back when something removed it.
pub fn spawn_writer(state: Arc<DaemonState>, rec: Arc<Recorder>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = live::subscribe();
        let mut seen = *rx.borrow_and_update();
        rec.write_now(&state).await;
        let mut last_write = Instant::now();
        let mut heal = tokio::time::interval(HEAL_EVERY);
        heal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heal.tick().await;
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => return,
                _ = heal.tick() => {
                    // Put a removed file back; retry a failed write (the
                    // first one also ends a restore's attempt count).
                    if rec.failing() || std::fs::symlink_metadata(rec.path()).is_err() {
                        rec.write_now(&state).await;
                        last_write = Instant::now();
                    }
                    continue;
                }
                changed = rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            let now = *rx.borrow_and_update();
            let mut urgent = now.urgent != seen.urgent;
            seen = now;
            if !urgent {
                let due = tokio::time::Instant::from_std(last_write + DEBOUNCE);
                loop {
                    tokio::select! {
                        _ = state.shutdown.cancelled() => return,
                        _ = tokio::time::sleep_until(due) => break,
                        changed = rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            let now = *rx.borrow_and_update();
                            urgent = now.urgent != seen.urgent;
                            seen = now;
                            if urgent {
                                break;
                            }
                        }
                    }
                }
            }
            rec.write_now(&state).await;
            last_write = Instant::now();
        }
    })
}

/// `daemon.crash_restore = off`: a leftover record is removed (a later `on`
/// must not restore sessions from it), nothing else.
pub fn discard_when_off(paths: &DaemonPaths) {
    let p = path(paths);
    if matches!(std::fs::symlink_metadata(&p), Ok(m) if m.file_type().is_file()) {
        let _ = std::fs::remove_file(&p);
        tracing::info!(target: LOG_TARGET, path = %p.display(), "crash restore: off (daemon.crash_restore = off); removed the old crash record");
    }
}

/// What a restore did (the summary line's fields).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub restored: usize,
    pub parked: usize,
    /// Restored sessions asked to resume a cut turn (or to note the workers
    /// the crash stopped); each one's own line says what it did.
    pub resume_queued: usize,
    /// Given up: a load error, a held journal lock, the per-session cap.
    pub given_up: usize,
    /// In the record but not restored: they don't persist, a repeat id, no
    /// journal on disk, or parked past their eviction age.
    pub skipped: usize,
    pub took_ms: u64,
}

/// How long a request naming a session the restore still holds waits for
/// it ([`Restoring::wait`]).
pub const GATE_WAIT: Duration = Duration::from_secs(30);
/// How long one session's rehydrate may take before the restore gives it
/// up (it stays on disk; `--continue` loads it).
pub const REHYDRATE_CAP: Duration = Duration::from_secs(20);
/// How long one session's create may wait for its session's create lock
/// (the host's per-session create reservation, held by another create of
/// the same session) before the restore gives it up (a create that hangs
/// holding the lock must not keep the restore running for ever). Lanes of
/// different sessions never wait for each other; this is above the 16 ×
/// 20 s they once could.
pub const REHYDRATE_QUEUE_CAP: Duration = Duration::from_secs(600);
const _: () = assert!(
    REHYDRATE_QUEUE_CAP.as_secs() > RESTORE_CONCURRENCY as u64 * REHYDRATE_CAP.as_secs(),
    "a lane queued behind every other lane's capped create is never given up for the queue"
);
/// How long a stopping daemon gives its restore task to stop on its own.
const RESTORE_STOP_WAIT: Duration =
    Duration::from_secs(crate::session::budgets::RESTORE_STOP_WAIT_SECS);

/// Why a request naming a session the restore holds isn't served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// Still restoring after [`GATE_WAIT`].
    StillRestoring,
    /// The restore gave the session up (the reason).
    GivenUp(String),
}

impl GateError {
    /// The client-facing error text (`DaemonFrame::Error.message`).
    pub fn message(&self, id: &str) -> String {
        match self {
            Self::StillRestoring => format!("session {id} is still restoring"),
            Self::GivenUp(why) => format!("session {id} could not be restored: {why}"),
        }
    }
}

struct Pending {
    entry: LiveSession,
    /// `true` once the restore is done with the session (back, or given up).
    done: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct RestoringInner {
    /// While the restore runs: the record's restore count, written into
    /// every record until the summary line (a start that dies before then
    /// died restoring).
    attempts: Option<u32>,
    /// The record's `written_at` (a listed entry's `created_at`).
    recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Recorded id → not restored yet: listed `restoring: true`, carried
    /// unchanged into every record.
    pending: std::collections::HashMap<String, Pending>,
    /// Recorded id → given up (the entry, the reason): not listed, carried
    /// into every record (a later start tries it again), without the
    /// capacity resume it had ([`drop_capacity_resume`]).
    given_up: std::collections::HashMap<String, (LiveSession, String)>,
}

impl RestoringInner {
    /// The recorded id `id` names: itself, the session whose journal id it
    /// is (after a compaction the two differ), or the one session recorded
    /// under that name (a client's `--continue <name>`; a name two recorded
    /// sessions share names neither).
    fn key_of(&self, id: &str) -> String {
        if self.pending.contains_key(id) || self.given_up.contains_key(id) {
            return id.to_string();
        }
        let entries = || {
            self.pending
                .iter()
                .map(|(k, p)| (k, &p.entry))
                .chain(self.given_up.iter().map(|(k, (e, _))| (k, e)))
        };
        if let Some((k, _)) = entries().find(|(_, e)| e.session.journal_id == id) {
            return k.clone();
        }
        let named = |e: &LiveSession| {
            e.name.as_deref().or(e.session.config.name.as_deref()) == Some(id)
        };
        let mut by_name = entries().filter(|&(_, e)| named(e));
        match (by_name.next(), by_name.next()) {
            (Some((k, _)), None) => k.clone(),
            _ => id.to_string(),
        }
    }
}

/// A session the restore gives up has no live actor to carry its capacity
/// resume: the resume goes with it (one INFO line), so no later record, stop
/// or client load brings it back. Whether there was one.
fn drop_capacity_resume(id: &str, mut entry: LiveSession) -> (LiveSession, bool) {
    match entry.session.capacity_stop.take() {
        Some(stop) => {
            crate::session::capacity_resume::note_given_up(id, &stop);
            (entry, true)
        }
        None => (entry, false),
    }
}

/// A fresh start's restore from the crash record (`DaemonState::restoring`):
/// per recorded session a gate that requests naming it wait on, and the
/// entries the restore has not finished. Empty when no restore ran.
#[derive(Default)]
pub struct Restoring {
    inner: Mutex<RestoringInner>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Restoring {
    fn inner(&self) -> std::sync::MutexGuard<'_, RestoringInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The restore is still going (until its summary line).
    pub fn running(&self) -> bool {
        self.inner().attempts.is_some()
    }

    fn begin(&self, attempts: u32, recorded_at: chrono::DateTime<chrono::Utc>, entries: &[LiveSession]) {
        let mut inner = self.inner();
        inner.attempts = Some(attempts);
        inner.recorded_at = Some(recorded_at);
        for e in entries {
            let (done, _) = tokio::sync::watch::channel(false);
            inner.pending.insert(e.session.id.clone(), Pending { entry: e.clone(), done });
        }
    }

    /// The restore is done with `id`: back (`Ok`), or given up (`Err`, the
    /// reason; it leaves the listing in this same step). Its waiters are
    /// released after the state changed.
    fn settle(&self, id: &str, outcome: Result<(), String>) {
        let mut inner = self.inner();
        let Some(p) = inner.pending.remove(id) else {
            return;
        };
        let mut dropped = false;
        if let Err(why) = outcome {
            let (entry, had) = drop_capacity_resume(id, p.entry);
            dropped = had;
            inner.given_up.insert(id.to_string(), (entry, why));
        }
        p.done.send_replace(true);
        drop(inner);
        if dropped {
            live::mark_dirty(true);
        }
    }

    /// The summary line is out: the records from here on hold no restore
    /// count (written at once).
    fn finish(&self) {
        self.inner().attempts = None;
        live::mark_dirty(true);
    }

    /// The restore task failed (a panic): every session it had not finished
    /// is given up with `why` (its waiters get the error, it stays carried
    /// in the record, `--continue` loads it with its one resume), and the
    /// restore is over (reloads, the idle exit and the gates go on as
    /// after a summary). Returns the ids given up.
    fn abandon(&self, why: &str) -> Vec<String> {
        let mut inner = self.inner();
        let pending: Vec<(String, Pending)> = inner.pending.drain().collect();
        let mut ids = Vec::with_capacity(pending.len());
        for (id, p) in pending {
            let (entry, _) = drop_capacity_resume(&id, p.entry);
            inner.given_up.insert(id.clone(), (entry, why.to_string()));
            p.done.send_replace(true);
            ids.push(id);
        }
        inner.attempts = None;
        drop(inner);
        live::mark_dirty(true);
        ids
    }

    /// The entries every record written now carries unchanged (not
    /// restored yet, or given up), and the restore count to write.
    pub fn carried(&self) -> (Vec<LiveSession>, u32) {
        let inner = self.inner();
        let entries = inner
            .pending
            .values()
            .map(|p| p.entry.clone())
            .chain(inner.given_up.values().map(|(e, _)| e.clone()))
            .collect();
        (entries, inner.attempts.unwrap_or(0))
    }

    /// Wait (up to `bound`) until the restore is done with `id` (a recorded
    /// session id, or its journal id). `Ok`: it is back, or it was never the
    /// restore's. A session given up says why.
    pub async fn wait(&self, id: &str, bound: Duration) -> Result<(), GateError> {
        let (key, mut rx) = {
            let inner = self.inner();
            let key = inner.key_of(id);
            if let Some((_, why)) = inner.given_up.get(&key) {
                return Err(GateError::GivenUp(why.clone()));
            }
            match inner.pending.get(&key) {
                Some(p) => (key, p.done.subscribe()),
                None => return Ok(()),
            }
        };
        // The watch guard `wait_for` returns is dropped inside: `settle`
        // takes the watch's lock while it holds `inner`, so a guard must
        // never be held while taking `inner`.
        let done = async move { rx.wait_for(|d| *d).await.is_ok() };
        if tokio::time::timeout(bound, done).await.is_err() {
            return Err(GateError::StillRestoring);
        }
        match self.inner().given_up.get(&key) {
            Some((_, why)) => Err(GateError::GivenUp(why.clone())),
            None => Ok(()),
        }
    }

    /// The sessions still restoring, as listings show them: the record's
    /// fields, `lifecycle` as recorded (live or parked), `restoring: true`.
    pub fn metas(&self) -> Vec<crate::session::SessionMeta> {
        let inner = self.inner();
        let at = inner.recorded_at.unwrap_or_else(chrono::Utc::now);
        inner.pending.values().map(|p| restoring_meta(&p.entry, at)).collect()
    }

    /// A session the restore gave up is live now (a client's create loaded
    /// it, by id, journal id, name or `--continue` latest, or a client
    /// attached to it after the cap ran out past its create): its entry
    /// leaves the restore's keeping (from now on its actor is recorded) and
    /// comes back for its one resume decision. Matched by the LOADED
    /// session's id or journal id, never by what the client typed: a name
    /// or a chain could resolve to another session than the entry that
    /// carries it. `None`: no given-up entry for it, or already taken.
    fn take_given_up_loaded(&self, id: &str, journal_id: &str) -> Option<LiveSession> {
        let mut inner = self.inner();
        if inner.given_up.is_empty() {
            return None;
        }
        let ids = [id, journal_id];
        let key = inner
            .given_up
            .iter()
            .find(|(k, (e, _))| {
                ids.iter()
                    .any(|i| k.as_str() == *i || e.session.id == *i || e.session.journal_id == *i)
            })
            .map(|(k, _)| k.clone())?;
        inner.given_up.remove(&key).map(|(e, _)| e)
    }

    /// Whether a stop records `id` from the restore's carried entry rather
    /// than from its live actor: only for an entry the restore had not
    /// finished (its one resume is still owed), and only when the stop cut
    /// no turn of the actor's own. A given-up entry whose session is live
    /// is the actor's from then on: the actor is recorded, as the crash
    /// record does ([`with_carried`]).
    fn stop_keeps_carried(&self, id: &str, turn_cut: bool) -> bool {
        !turn_cut && self.inner().pending.contains_key(id)
    }

    /// The daemon is stopping (`shutdown` is cancelled, which the restore
    /// task watches): give the task a moment to stop on its own, then abort
    /// it, so no session comes back after the stop's record is written.
    pub async fn stop(&self) {
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(mut task) = task {
            if tokio::time::timeout(RESTORE_STOP_WAIT, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

/// A session the restore still holds, as a listing shows it.
fn restoring_meta(e: &LiveSession, at: chrono::DateTime<chrono::Utc>) -> crate::session::SessionMeta {
    let s = &e.session;
    let parked = matches!(s.lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking);
    crate::session::SessionMeta {
        id: crate::session::SessionId::from(s.id.as_str()),
        name: e.name.clone().or_else(|| s.config.name.clone()),
        model: s.config.model_override.clone().unwrap_or_default(),
        cwd: s.config.cwd.clone(),
        created_at: e.created_at.unwrap_or(at),
        continued: true,
        continue_info: None,
        host_pid: std::process::id(),
        lifecycle: if parked { SessionLifecycle::Parked } else { SessionLifecycle::Live },
        clients: 0,
        input_owner: None,
        input_owner_kind: None,
        awaiting_input: 0,
        journal_id: if s.journal_id.is_empty() { s.id.clone() } else { s.journal_id.clone() },
        locked_by: None,
        restoring: true,
        // Upstream's listing fields (790d R1): their defaults while the
        // restore holds it (no actor publishes them yet).
        roster: Vec::new(),
        client_input: None,
        pinned: false,
        busy: false,
        state_since: None,
        park_due: None,
        evict_due: None,
        title: None,
        cost_usd: 0.0,
        messages: 0,
    }
}

/// A listing: the live sessions, then each restoring entry no live one
/// already shows (by id or by journal id: a session with an actor is listed
/// from it). The caller reads the restore set BEFORE the live sessions, so
/// a session that settles in between is listed from its actor, never from
/// neither (an actor is registered before its entry settles).
pub(crate) fn merge_listing(
    mut live: Vec<crate::session::SessionMeta>,
    restoring: Vec<crate::session::SessionMeta>,
) -> Vec<crate::session::SessionMeta> {
    if restoring.is_empty() {
        return live;
    }
    let listed: std::collections::HashSet<String> = live
        .iter()
        .flat_map(|m| [m.id.as_str().to_string(), m.journal_id.clone()])
        .filter(|k| !k.is_empty())
        .collect();
    live.extend(
        restoring
            .into_iter()
            .filter(|m| !listed.contains(m.id.as_str()) && !listed.contains(&m.journal_id)),
    );
    live
}

/// Whether the session's history is on disk (its `<id>.json` or its
/// `<id>.journal`): without it there is nothing to restore.
fn on_disk(journal_id: &str) -> bool {
    let dir = crate::config::resolve_write_path("sessions");
    dir.join(format!("{journal_id}.json")).is_file()
        || agent_core::session_journal::journal_path(&dir, journal_id).is_file()
}

/// What [`prepare_restore`] found; [`spawn_restore`] runs it.
pub struct Plan {
    entries: Vec<LiveSession>,
    /// The record's own origin (for entries without one of their own).
    origin: Origin,
    how: &'static str,
    writer_key: String,
    /// A stop record's writer: it released the daemon lock as its last
    /// step; the restore waits (bounded) for it to finish exiting.
    wait_for: Option<Writer>,
    summary: Summary,
}

/// Whether the restore takes entry `s` as far as its parking goes. First its
/// recorded capacity resume is validated, before it earns anything: one that
/// fails is stripped with one WARN of a fixed class (the loaded text never
/// reaches the log). Then a session recorded Parked past its eviction age is
/// left on disk (one INFO line), unless an armed capacity resume that
/// validates holds it (its resume unparks it), as it did in the daemon that
/// recorded it; nothing else does.
fn screen_entry(
    s: &mut LiveSession,
    evict: Option<Duration>,
    capacity_resume: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let id = s.session.id.as_str();
    let invalid = s
        .session
        .capacity_stop
        .as_ref()
        .and_then(|stop| crate::session::capacity_resume::validate(stop, now).err());
    if let Some(invalid) = invalid {
        tracing::warn!(
            target: crate::session::capacity_resume::LOG_TARGET,
            session = %id,
            event = "capacity_resume",
            phase = "dropped",
            class = invalid.as_str(),
            "capacity resume: the recorded stop is invalid; dropped"
        );
        s.session.capacity_stop = None;
    }
    let parked = matches!(s.session.lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking);
    if !parked || !parked_past_eviction(s.parked_since, evict, now) {
        return true;
    }
    if !crate::session::capacity_resume::spares_parked(s.session.capacity_stop.as_ref(), capacity_resume, now) {
        tracing::info!(target: LOG_TARGET, session = %id, "crash restore: session={id} was parked past its eviction age; left on disk");
        return false;
    }
    tracing::info!(target: LOG_TARGET, session = %id, "crash restore: session={id} was parked past its eviction age; restored for its capacity resume");
    true
}

/// Fresh start, `daemon.crash_restore` on, BEFORE the daemon listens (fast:
/// no session is touched). Read the record and decide whether to restore
/// from it: a record whose writer still runs (or can't be checked) is moved
/// aside (`.skipped-<time>`), one that earlier starts died restoring is
/// quarantined. Otherwise count this attempt in the record, seed the
/// subagent ids, and put the sessions to restore into `state.restoring`, so
/// the first connection already finds them listed and gated. Sessions with
/// no history on disk, and parked ones past their eviction age, are left
/// out. `None`: nothing to restore.
pub fn prepare_restore(state: &DaemonState) -> Option<Plan> {
    let p = path(&state.paths);
    let mut record = match read(&p) {
        Ok(Some(r)) => r,
        Ok(None) => return None,
        Err(why) => {
            tracing::warn!(target: LOG_TARGET, path = %p.display(), why = %why, "crash restore: the crash record is unusable; nothing restored");
            return None;
        }
    };
    let crash = record.writer.key();
    let writer = record.writer.clone();
    let stopped = record.reload.as_ref().is_some_and(|h| h.stop);
    if state.crash_record.as_ref().is_some_and(|r| r.writer().key() == crash) {
        tracing::info!(target: LOG_TARGET, writer = %crash, "crash restore: the record is this daemon's own; nothing restored");
        return None;
    }
    // A stop record's writer released the daemon lock (which this start
    // holds) as its last step: it serves nothing any more, whether or not
    // its process is gone yet.
    if !stopped && writer.is_alive() {
        let moved_to = moved_aside(&p, "skipped");
        tracing::warn!(
            target: LOG_TARGET,
            writer = %crash,
            moved_to = %moved_to,
            "crash restore: the record's writer still runs (or can't be checked); moved the record aside, nothing restored"
        );
        return None;
    }
    if record.restore_attempts >= MAX_RESTORE_ATTEMPTS {
        let moved_to = moved_aside(&p, "quarantined");
        tracing::warn!(
            target: LOG_TARGET,
            writer = %crash,
            attempts = record.restore_attempts,
            moved_to = %moved_to,
            "crash restore: earlier starts died restoring this record; moved it aside, not restoring from it"
        );
        return None;
    }
    // Before any session is touched: a start that dies before the
    // restore's summary line counts against the record.
    record.restore_attempts += 1;
    let counted = serde_json::to_vec(&record)
        .map_err(|e| e.to_string())
        .and_then(|body| write_durable(&p, &body).map_err(|e| e.to_string()));
    if let Err(e) = counted {
        tracing::warn!(target: LOG_TARGET, error = %e, "crash restore: could not count the restore attempt; restoring anyway");
    }
    crate::tools::seed_next_subagent_id(record.subagent_id_floor());
    let origin = match &record.reload {
        Some(h) => Origin::HandOff(h.clone()),
        None => Origin::Crash { writer: crash.clone(), crashed_at: record.written_at },
    };
    let how = match &record.reload {
        Some(h) if h.stop => "stop",
        Some(_) => "handoff",
        None => "crash",
    };
    let evict = crate::session::actor::parked_evict_after();
    let now = chrono::Utc::now();
    let mut summary = Summary::default();
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for mut s in std::mem::take(&mut record.sessions) {
        let id = s.session.id.clone();
        if !s.session.config.persist || !seen.insert(id.clone()) {
            summary.skipped += 1;
            continue;
        }
        let journal = if s.session.journal_id.is_empty() { id.as_str() } else { s.session.journal_id.as_str() };
        if !on_disk(journal) {
            summary.skipped += 1;
            tracing::warn!(target: LOG_TARGET, session = %id, "crash restore: session={id} has no history on disk; skipped");
            continue;
        }
        if !screen_entry(&mut s, evict, state.host.config().capacity_resume, now) {
            summary.skipped += 1;
            continue;
        }
        if s.origin.is_none() {
            s.origin = Some(origin.clone());
        }
        entries.push(s);
    }
    state.restoring.begin(record.restore_attempts, record.written_at, &entries);
    tracing::info!(
        target: LOG_TARGET,
        how = %how,
        writer = %crash,
        sessions = entries.len(),
        skipped = summary.skipped,
        attempt = record.restore_attempts,
        "crash restore: restoring {} sessions after the {how}, once the daemon listens",
        entries.len()
    );
    Some(Plan { entries, origin, how, writer_key: crash, wait_for: stopped.then_some(writer), summary })
}

/// Start the restore [`prepare_restore`] planned, right after the daemon
/// listens: 16 sessions at a time, each bounded by [`REHYDRATE_CAP`] once
/// its create holds its session's create lock and by [`REHYDRATE_QUEUE_CAP`]
/// before.
pub fn spawn_restore(state: &Arc<DaemonState>, plan: Plan) {
    let task = tokio::spawn(run_restore(Arc::clone(state), plan));
    *state.restoring.task.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
}

/// What a restored session is told (see [`After`]), and whether the crash
/// loop cap held its resume back.
fn after_for(e: &LiveSession, default: &Origin, reload_resume: bool) -> (After, bool) {
    match e.origin.as_ref().unwrap_or(default) {
        // A reload hand-off's cut turns follow `daemon.reload_resume`, as
        // after the reload itself; a stop's follow `daemon.crash_restore`
        // (on, or this would not run).
        Origin::HandOff(h) => (
            After::Reload {
                resume: h.stop || reload_resume,
                generation: h.generation,
                reloaded_at: h.reloaded_at,
                stopped: h.stop,
            },
            false,
        ),
        Origin::Crash { writer, crashed_at } => {
            let looping = e.turn_in_flight && crash_loop_suspected(e.resumed_at, *crashed_at);
            (
                After::Crash {
                    resume: !looping,
                    crash: writer.clone(),
                    crashed_at: *crashed_at,
                    turn_in_flight: e.turn_in_flight,
                    lost_workers: e.running_workers.clone(),
                },
                looping,
            )
        }
    }
}

type ResumeReply = Option<tokio::sync::oneshot::Receiver<&'static str>>;

/// Why [`capped_after_lock`] gave a lane up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capped {
    /// Its create never got its session's create lock within the queue cap.
    Queued,
    /// Its create held the lock past the cap.
    Ran,
}

/// `work`, bounded at `cap` counted from when `locked` fires (the lane's
/// create holds its session's create lock, or found the session live), and
/// at `queue_cap` until then: a
/// create that hangs holding the lock (another lane's is capped, a
/// client's is not) must not keep the restore running for ever (no idle
/// exit, reloads refused, gates saying "still restoring"). `Err`: which
/// cap ran out (`work` is dropped, which is cancel-safe: a create
/// registers its session with no await after the actor exists, one that
/// holds the lock gives it back when dropped, and one still waiting for it
/// holds nothing).
async fn capped_after_lock<T>(
    locked: &tokio::sync::Notify,
    queue_cap: Duration,
    cap: Duration,
    work: impl std::future::Future<Output = T>,
) -> Result<T, Capped> {
    let timer = async {
        if tokio::time::timeout(queue_cap, locked.notified()).await.is_err() {
            return Capped::Queued;
        }
        tokio::time::sleep(cap).await;
        Capped::Ran
    };
    tokio::pin!(work);
    tokio::pin!(timer);
    tokio::select! {
        biased;
        out = &mut work => Ok(out),
        why = &mut timer => Err(why),
    }
}

/// Whether the restore asks this session to resume a cut turn (or to note
/// the workers the crash stopped): the summary's "resume queued". The actor
/// still skips one already answered or re-prompted (the per-session line
/// says what it did).
fn resume_due(e: &LiveSession, after: &After) -> bool {
    match after {
        After::Crash { resume: true, turn_in_flight, lost_workers, .. } => *turn_in_flight || !lost_workers.is_empty(),
        After::Reload { resume: true, .. } => e.session.turn_cut == Some(true),
        _ => false,
    }
}

/// A panic in the restore task (the guard is dropped while unwinding) must
/// not leave the restore running for the life of the process (reloads
/// refused, the idle exit blocked, every gate waiting out its 30 s): the
/// sessions not finished are given up, each with its WARN line, and the
/// restore ends. A task aborted by a stop is dropped without a panic: then
/// nothing changes, and the stop's record carries those entries unchanged.
/// Only builds that unwind get here: the release profile aborts on a panic,
/// and the next start counts the dead one as a start that died restoring.
struct FinishOnPanic<'a>(&'a Restoring);

impl Drop for FinishOnPanic<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let why = "the restore failed; it stays on disk";
        let ids = self.0.abandon(why);
        for id in &ids {
            give_up_line(id, why);
        }
        tracing::warn!(target: LOG_TARGET, given_up = ids.len(), "crash restore: the restore failed; gave up {} sessions", ids.len());
    }
}

async fn run_restore(state: Arc<DaemonState>, plan: Plan) {
    let _finish_on_panic = FinishOnPanic(&state.restoring);
    let Plan { entries, origin, how, writer_key, wait_for, mut summary } = plan;
    if let Some(w) = &wait_for {
        wait_for_exit(w).await;
    }
    let reload_resume = state.host.config().daemon.reload_resume;
    let t0 = Instant::now();
    let rehydrates = entries.into_iter().map(|e| {
        let state = Arc::clone(&state);
        let (after, looping) = after_for(&e, &origin, reload_resume);
        let due = resume_due(&e, &after);
        async move {
            let id = e.session.id.clone();
            let journal_id = e.session.journal_id.clone();
            // Stopping: not started; the entry stays carried for the
            // stop's record (the ones already running finish, bounded by
            // `Restoring::stop`).
            if state.shutdown.is_cancelled() {
                return (id, journal_id, due, None);
            }
            if looping {
                tracing::warn!(target: LOG_TARGET, session = %id, "crash restore: crash loop suspected: session={id} not resumed");
            }
            // The cap counts from the moment this lane's create holds its
            // session's create lock: time queued behind another create of
            // the same session (a client's, waiting on its hooks) never
            // gives a session up.
            let probe = Arc::new(crate::host::CreateProbe::default());
            let work = crate::host::CREATE_PROBE.scope(
                Arc::clone(&probe),
                reload::rehydrate_one(&state, &e.session, &after, Some(e.parked_since)),
            );
            let back = capped_after_lock(&probe.locked, REHYDRATE_QUEUE_CAP, REHYDRATE_CAP, work).await;
            (id, journal_id, due, Some(back))
        }
    });
    let mut stream = Box::pin(futures::stream::iter(rehydrates).buffer_unordered(restore_lanes()));
    let mut replies: Vec<(String, ResumeReply)> = Vec::new();
    let mut left = 0usize;
    while let Some((id, journal_id, due, back)) = stream.next().await {
        let Some(back) = back else {
            left += 1;
            continue;
        };
        match back {
            Ok(Rehydrated::Restored { parked, resume }) => {
                summary.restored += 1;
                summary.parked += usize::from(parked);
                summary.resume_queued += usize::from(due && resume.is_some());
                state.restoring.settle(&id, Ok(()));
                replies.push((id, resume));
            }
            // Only a reload's rehydrate registers a placeholder.
            Ok(Rehydrated::Placeholder) => state.restoring.settle(&id, Ok(())),
            Ok(Rehydrated::Failed(why)) => give_up(&state, &id, why, &mut summary),
            Err(capped) => {
                give_up(&state, &id, capped_why(capped), &mut summary);
                // The cap ran out after the create had finished: the
                // session is live but given up. It gets its one resume
                // decision (and its carried workers) now, as a client's
                // load would give it, so a stop or the crash snapshot,
                // which record a live session from its actor, can't drop
                // the resume it is owed. Spawned: the actor may be what
                // stalled. A client's load that took the entry first, or
                // takes it later, makes the other one a no-op.
                if capped == Capped::Ran {
                    if let Some(h) = live_session(&state, &id, &journal_id) {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move { loaded_on_attach(&state, &h).await });
                    }
                }
            }
        }
    }
    drop(stream);
    summary.took_ms = t0.elapsed().as_millis() as u64;
    let interrupted = left > 0;
    log_summary(&summary, how, &writer_key, interrupted);
    if !interrupted {
        state.restoring.finish();
        tokio::spawn(log_resumes(replies, how));
    }
}

/// The restore gives `id` up: it leaves the listing, its waiters get the
/// reason, and its entry stays carried in the record.
fn give_up(state: &DaemonState, id: &str, why: String, summary: &mut Summary) {
    summary.given_up += 1;
    give_up_line(id, &why);
    state.restoring.settle(id, Err(why));
}

/// A recorded session's live actor, if it has one: by its recorded id (a
/// restore's create keeps it), else by its journal id. Never a lock-held
/// placeholder.
fn live_session(state: &DaemonState, id: &str, journal_id: &str) -> Option<crate::session::SessionHandle> {
    let real = |h: &crate::session::SessionHandle| h.meta().locked_by.is_none();
    state.attach(&crate::session::SessionId::from(id)).filter(real).or_else(|| {
        state
            .host
            .session_handles()
            .into_iter()
            .find(|h| h.journal_id() == journal_id && real(h))
    })
}

/// The give-up reason for a lane [`capped_after_lock`] stopped.
fn capped_why(capped: Capped) -> String {
    match capped {
        Capped::Ran => format!("not restored within {} s; it stays on disk", REHYDRATE_CAP.as_secs()),
        Capped::Queued => format!(
            "its create waited {} s for the create lock; it stays on disk",
            REHYDRATE_QUEUE_CAP.as_secs()
        ),
    }
}

fn give_up_line(id: &str, why: &str) {
    tracing::warn!(target: LOG_TARGET, session = %id, reason = %why, "crash restore: session={id} given up: {why}");
}

/// A client's create or attach made `h` live (or the restore's own create
/// did, and the cap ran out after it), and the restore gave its session up
/// (it failed to load, its journal was locked, it ran past the cap, or the
/// cap ran out after its create): it gets the one resume
/// decision the restore owed it (the same [`After`] from the record it came
/// from; the actor keeps it once per crash or stop, and skips a turn a user
/// message already answered), and its carried workers, before the client
/// attaches. However the client named it (id, journal id, name, chain,
/// `--continue` latest): matched by the loaded session itself. Nothing to
/// do for any other session, or on a second load. One INFO line once the
/// actor answered:
/// `crash restore: session=<id> loaded on attach, resume queued|not needed|unknown`.
pub async fn loaded_on_attach(state: &DaemonState, h: &crate::session::SessionHandle) {
    let Some(e) = state.restoring.take_given_up_loaded(h.id.as_str(), &h.journal_id()) else {
        return;
    };
    // The live session is recorded from its actor from now on.
    live::mark_dirty(true);
    let Some(origin) = e.origin.clone() else {
        return;
    };
    let reload_resume = state.host.config().daemon.reload_resume;
    let (after, looping) = after_for(&e, &origin, reload_resume);
    let id = e.session.id.clone();
    if looping {
        tracing::warn!(target: LOG_TARGET, session = %id, "crash restore: crash loop suspected: session={id} not resumed");
    }
    let parked = matches!(e.session.lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking);
    let reply = if parked { None } else { reload::ask_resume(h, &e.session, &after).await };
    if !e.session.workers.is_empty() {
        // The crash restore leaves reload-stopped workers to the model (a
        // daemon resume here is a follow-up, not built yet).
        let _ = h
            .send(crate::session::SessionCommand::AdoptWorkers {
                workers: e.session.workers.clone(),
                resume: crate::session::WorkerResume::Never,
            })
            .await;
    }
    tokio::spawn(async move {
        let resume = match reply {
            None => "not needed",
            Some(rx) => match tokio::time::timeout(RESUME_REPLY_WAIT, rx).await {
                Ok(Ok("resumed")) => "queued",
                Ok(Ok(_)) => "not needed",
                _ => "unknown",
            },
        };
        log_loaded_on_attach(&id, resume);
    });
}

fn log_loaded_on_attach(id: &str, resume: &str) {
    tracing::info!(
        target: LOG_TARGET,
        session = %id,
        resume = %resume,
        "crash restore: session={id} loaded on attach, resume {resume}"
    );
}

/// One INFO line per restored session once its resume decision is known
/// (bounded by [`RESUME_REPLY_WAIT`]; `unknown` after it).
async fn log_resumes(replies: Vec<(String, ResumeReply)>, how: &'static str) {
    let deadline = tokio::time::Instant::now() + RESUME_REPLY_WAIT;
    let mut waits = futures::stream::FuturesUnordered::new();
    for (id, reply) in replies {
        match reply {
            None => log_restored(&id, "no", how),
            Some(rx) => waits.push(async move { (tokio::time::timeout_at(deadline, rx).await, id) }),
        }
    }
    while let Some((answer, id)) = waits.next().await {
        let resumed = match answer {
            Ok(Ok("resumed")) => "yes",
            Ok(Ok(_)) => "no",
            _ => "unknown",
        };
        log_restored(&id, resumed, how);
    }
}

fn log_restored(id: &str, resumed: &str, how: &str) {
    tracing::info!(
        target: LOG_TARGET,
        session = %id,
        resumed = %resumed,
        how = %how,
        "crash restore: session={id} restored, resumed={resumed}"
    );
}

/// The one summary line a restore leaves on [`LOG_TARGET`], as soon as the
/// last session is back or given up (it does not wait for resume replies).
fn log_summary(s: &Summary, how: &str, writer: &str, interrupted: bool) {
    tracing::info!(
        target: LOG_TARGET,
        how = %how,
        writer = %writer,
        restored = s.restored,
        parked = s.parked,
        resume_queued = s.resume_queued,
        given_up = s.given_up,
        skipped = s.skipped,
        took_ms = s.took_ms,
        interrupted,
        "crash restore: restored {} sessions, resume queued {}, given up {}, skipped {}, took {} ms",
        s.restored,
        s.resume_queued,
        s.given_up,
        s.skipped,
        s.took_ms
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half the cores, at least 2, at most RESTORE_CONCURRENCY.
    #[test]
    fn restore_lanes_are_half_the_cores_between_two_and_sixteen() {
        for (cores, lanes) in [(1, 2), (2, 2), (4, 2), (6, 3), (8, 4), (16, 8), (32, 16), (64, 16), (256, 16)] {
            assert_eq!(lanes_for(cores), lanes, "{cores} cores");
        }
        assert!((2..=RESTORE_CONCURRENCY).contains(&restore_lanes()));
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn session(id: &str) -> ReloadSession {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "journal_id": id,
            "config": {},
            "keep_warm": false,
            "lifecycle": "live",
            "input_owner_kind": null
        }))
        .unwrap()
    }

    fn record(writer: Writer) -> CrashRecord {
        CrashRecord {
            version: VERSION,
            writer,
            written_at: chrono::Utc::now(),
            generation: 1,
            reload: None,
            sessions: vec![entry("s-cut", true, &["sa_7"]), entry("s-idle", false, &[])],
            next_subagent_id: 3,
            restore_attempts: 0,
        }
    }

    fn entry(id: &str, turn_in_flight: bool, workers: &[&str]) -> LiveSession {
        LiveSession {
            session: session(id),
            turn_in_flight,
            running_workers: workers.iter().map(|w| w.to_string()).collect(),
            origin: None,
            resumed_at: None,
            parked_since: None,
            name: None,
            created_at: None,
        }
    }

    /// The summary line reaches the daemon log through the daemon's own
    /// filter (`with_crate_directives` over a `RUST_LOG`-unset base); a line
    /// at an unrelated target does not (the control).
    #[test]
    fn the_daemon_log_filter_keeps_the_restore_summary() {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        let s = Summary {
            restored: 55,
            parked: 4,
            resume_queued: 2,
            given_up: 1,
            skipped: 1,
            took_ms: 812,
        };
        tracing::subscriber::with_default(subscriber, || {
            log_summary(&s, "crash", "12236:99:boot", false);
            log_restored("s-1", "yes", "crash");
            give_up_line("s-2", "journal locked by pid 7");
            log_loaded_on_attach("s-2", "queued");
            tracing::info!(target: "crash_restore", "crash restore: outside the crate directives");
        });
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        assert!(
            text.contains(
                "crash restore: restored 55 sessions, resume queued 2, given up 1, skipped 1, took 812 ms"
            ),
            "{text:?}"
        );
        assert!(text.contains("crash restore: session=s-1 restored, resumed=yes"), "{text:?}");
        assert!(text.contains("crash restore: session=s-2 given up: journal locked by pid 7"), "{text:?}");
        assert!(text.contains("crash restore: session=s-2 loaded on attach, resume queued"), "{text:?}");
        assert!(text.contains("INFO") && text.contains(LOG_TARGET), "{text:?}");
        assert!(!text.contains("outside the crate directives"), "{text:?}");
    }

    #[test]
    fn the_resume_text_says_crash_and_names_lost_workers() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T15:51:07Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            resume_text(at, true, &[]),
            "resume: the daemon restarted after a crash mid-turn (15:51 UTC); your last turn was \
             cut, so check your last tool call before trusting it, then continue."
        );
        let workers = vec!["sa_3".to_string(), "sa_9".to_string()];
        assert!(resume_text(at, true, &workers).ends_with(
            " The crash also stopped your running subagents (sa_3, sa_9); no completion wake will \
             come for them."
        ));
        let idle = resume_text(at, false, &workers);
        assert!(idle.starts_with("resume: the daemon restarted after a crash (15:51 UTC)"), "{idle}");
        assert!(idle.contains("(sa_3, sa_9)"), "{idle}");
    }

    #[test]
    fn a_record_round_trips_and_keeps_the_reload_session_fields_flat() {
        let w = Writer { pid: 4242, start_ticks: 77, boot_id: Some("b".into()) };
        let r = record(w.clone());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["sessions"][0]["id"], "s-cut");
        assert_eq!(v["sessions"][0]["turn_in_flight"], true);
        assert_eq!(v["sessions"][0]["running_workers"][0], "sa_7");
        assert!(v["sessions"][1].get("running_workers").is_none());
        assert!(v.get("reload").is_none());
        let back: CrashRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.writer, w);
        assert_eq!(back.sessions.len(), 2);
        assert!(back.sessions[0].turn_in_flight && !back.sessions[1].turn_in_flight);
        assert_eq!(back.subagent_id_floor(), 8, "past the running sa_7 and the counter 3");
        assert_eq!(w.key(), "4242:77:b");
    }

    /// The writer as text, byte for byte, and an older writer without a
    /// boot id.
    #[test]
    fn the_writer_shape_is_pinned_as_text() {
        let w = Writer { pid: 4242, start_ticks: 77, boot_id: Some("b".into()) };
        assert_eq!(serde_json::to_string(&w).unwrap(), r#"{"pid":4242,"start_ticks":77,"boot_id":"b"}"#);
        let none = Writer { pid: 4242, start_ticks: 77, boot_id: None };
        assert_eq!(serde_json::to_string(&none).unwrap(), r#"{"pid":4242,"start_ticks":77,"boot_id":null}"#);
        assert_eq!(serde_json::from_str::<Writer>(r#"{"pid":4242,"start_ticks":77}"#).unwrap(), none);
        assert_eq!((w.key(), none.key()), ("4242:77:b".to_string(), "4242:77:-".to_string()));
    }

    /// This process is alive; a pid that can't exist, a pid whose start
    /// time differs and another boot are not.
    #[test]
    fn the_writer_check_tells_a_live_writer_from_a_dead_one() {
        let Some(me) = Writer::current() else {
            return;
        };
        assert!(me.is_alive());
        assert_eq!(Some(me.key().as_str()), live::process_key());
        assert!(!Writer { start_ticks: me.start_ticks + 1, ..me.clone() }.is_alive(), "a reused pid");
        assert!(!Writer { pid: 0x7fff_fff0, ..me.clone() }.is_alive(), "no such pid");
        assert!(!Writer { boot_id: Some("another-boot".into()), ..me.clone() }.is_alive());
    }

    /// On Linux the writer is what the kernel prints, and its record text
    /// is the one it always was: field 22 of the stat line and the boot id.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_writer_on_linux_is_the_kernels_start_ticks_and_boot_id() {
        let me = Writer::current().expect("a writer on Linux");
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let field22 = stat[stat.rfind(')').unwrap() + 1..].split_whitespace().nth(19).unwrap().to_string();
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let pid = std::process::id();
        assert_eq!(
            serde_json::to_string(&me).unwrap(),
            format!(r#"{{"pid":{pid},"start_ticks":{field22},"boot_id":"{}"}}"#, boot.trim())
        );
        assert_eq!(me.key(), format!("{pid}:{field22}:{}", boot.trim()));
    }

    /// Written 0600 through a temp file that doesn't stay; a hand-off
    /// stops writes until the exec fails.
    #[test]
    fn writes_are_private_and_a_hand_off_gates_them() {
        let d = tempfile::tempdir().unwrap();
        let paths = super::super::registry::daemon_paths_in(d.path(), None);
        let Some(rec) = Recorder::new(&paths) else {
            return;
        };
        assert_eq!(rec.path(), d.path().join("daemon.live.json"));
        let body = serde_json::to_vec(&record(rec.writer().clone())).unwrap();
        rec.write_gated(&body, "update");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(rec.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!d.path().join("daemon.live.json.tmp").exists());
        let back = read(rec.path()).unwrap().expect("a record");
        assert_eq!(back.sessions.len(), 2);

        let rs = ReloadState {
            generation: 5,
            written_at: chrono::Utc::now(),
            sessions: vec![ReloadSession { turn_cut: Some(true), ..session("s-cut") }],
            expected_clients: 0,
            next_subagent_id: 11,
            base_umask: None,
            children: Vec::new(),
        };
        rec.hand_off(&rs);
        let handed = read(rec.path()).unwrap().unwrap();
        assert_eq!(handed.reload.as_ref().map(|h| h.generation), Some(5));
        assert_eq!(handed.sessions[0].session.turn_cut, Some(true));
        rec.write_gated(&body, "update");
        assert!(read(rec.path()).unwrap().unwrap().reload.is_some(), "no write after the hand-off");
        rec.resume_writes();
        rec.write_gated(&body, "update");
        assert!(read(rec.path()).unwrap().unwrap().reload.is_none(), "the failed exec's image writes again");
        assert!(!handed.reload.unwrap().stop, "a reload's hand-off is not a stop");
    }

    /// Updates are written in the order their snapshots started: one that
    /// started earlier than the record on disk is stale and never replaces
    /// it (two that started at the same count both may write).
    #[test]
    fn a_stale_snapshot_never_replaces_a_newer_one() {
        let d = tempfile::tempdir().unwrap();
        let paths = super::super::registry::daemon_paths_in(d.path(), None);
        let Some(rec) = Recorder::new(&paths) else {
            return;
        };
        let body = |generation| {
            serde_json::to_vec(&CrashRecord { generation, ..record(rec.writer().clone()) }).unwrap()
        };
        assert!(rec.write_snapshot(&body(5), 5, &[]));
        assert!(!rec.write_snapshot(&body(4), 4, &[]), "an older snapshot is skipped");
        assert_eq!(read(rec.path()).unwrap().unwrap().generation, 5);
        assert!(rec.write_snapshot(&body(6), 5, &[]), "the same start writes");
        assert_eq!(read(rec.path()).unwrap().unwrap().generation, 6);
        assert!(rec.write_snapshot(&body(7), 9, &[]));
        assert!(!rec.write_snapshot(&body(8), 8, &[]));
        assert_eq!(read(rec.path()).unwrap().unwrap().generation, 7);
    }

    /// Two updates whose snapshots started at the same count may both write,
    /// in either order: the mark is always the pair of the record on disk,
    /// the latest written (the stamp is taken with the write). Real writes,
    /// in sequence and raced.
    #[test]
    fn equal_count_writes_leave_the_mark_on_the_record_on_disk() {
        let d = tempfile::tempdir().unwrap();
        let paths = super::super::registry::daemon_paths_in(d.path(), None);
        let Some(rec) = Recorder::new(&paths) else {
            return;
        };
        let rec = Arc::new(rec);
        let body = |generation: u64| {
            serde_json::to_vec(&CrashRecord { generation, ..record(rec.writer().clone()) }).unwrap()
        };
        let slot = live::LiveSlot::default();
        assert!(rec.write_snapshot(&body(2), 7, &[(slot.clone(), 2)]));
        assert!(rec.write_snapshot(&body(1), 7, &[(slot.clone(), 1)]), "the same start writes");
        assert_eq!(read(rec.path()).unwrap().unwrap().generation, 1);
        assert_eq!(slot.durable(), live::Durable { changes: 7, gen: 1 }, "the mark is the file's");

        for round in 0..100u64 {
            let started = 100 + round;
            let go = Arc::new(std::sync::Barrier::new(4));
            let writers: Vec<_> = (1..=4u64)
                .map(|gen| {
                    let (rec, slot, go, b) = (Arc::clone(&rec), slot.clone(), Arc::clone(&go), body(gen));
                    std::thread::spawn(move || {
                        go.wait();
                        rec.write_snapshot(&b, started, &[(slot, gen)])
                    })
                })
                .collect();
            for w in writers {
                assert!(w.join().unwrap());
            }
            let on_disk = read(rec.path()).unwrap().unwrap().generation;
            assert_eq!(slot.durable(), live::Durable { changes: started, gen: on_disk }, "round {round}");
        }
    }

    /// Only a write that succeeded acknowledges the entries it held: a
    /// stale, a failed and a refused (after a hand-off) write acknowledge
    /// nothing, and an update built before a hand-off never replaces it.
    #[test]
    fn only_a_successful_write_acknowledges_its_entries() {
        let d = tempfile::tempdir().unwrap();
        let paths = super::super::registry::daemon_paths_in(d.path(), None);
        let Some(rec) = Recorder::new(&paths) else {
            return;
        };
        let body = serde_json::to_vec(&record(rec.writer().clone())).unwrap();
        let slot = live::LiveSlot::default();
        assert!(rec.write_snapshot(&body, 10, &[(slot.clone(), 3)]));
        assert_eq!(slot.durable(), live::Durable { changes: 10, gen: 3 });
        assert!(!rec.write_snapshot(&body, 9, &[(slot.clone(), 4)]), "stale");
        assert_eq!(slot.durable(), live::Durable { changes: 10, gen: 3 });

        // An update snapshot built (armed) before a hand-off, written after it.
        let prebuilt = serde_json::to_vec(&record(rec.writer().clone())).unwrap();
        let rs = ReloadState {
            generation: 5,
            written_at: chrono::Utc::now(),
            sessions: vec![session("s-cut")],
            expected_clients: 0,
            next_subagent_id: 11,
            base_umask: None,
            children: Vec::new(),
        };
        rec.hand_off(&rs);
        assert!(!rec.write_snapshot(&prebuilt, 20, &[(slot.clone(), 5)]), "refused after the hand-off");
        assert!(read(rec.path()).unwrap().unwrap().reload.is_some(), "the hand-off stays");
        assert_eq!(slot.durable(), live::Durable { changes: 10, gen: 3 }, "nothing acknowledged");

        // A write that fails (the record's directory is a file).
        let f = tempfile::tempdir().unwrap();
        std::fs::write(f.path().join("runtime"), b"not a dir").unwrap();
        let broken = super::super::registry::daemon_paths_in(&f.path().join("runtime"), None);
        let Some(bad) = Recorder::new(&broken) else {
            return;
        };
        let other = live::LiveSlot::default();
        assert!(!bad.write_snapshot(&body, 30, &[(other.clone(), 1)]));
        assert_eq!(other.durable(), live::Durable::default(), "a failed write acknowledges nothing");
    }

    /// A record from before the stop field reads as a reload's hand-off; a
    /// quarantined record keeps its body under a new name and is no longer
    /// found.
    #[test]
    fn old_hand_offs_are_reloads_and_quarantine_moves_the_record_aside() {
        let h: HandOff =
            serde_json::from_str(r#"{"generation":3,"reloaded_at":"2026-10-19T15:51:07Z"}"#).unwrap();
        assert!(!h.stop);
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.live.json");
        std::fs::write(&p, b"{\"body\":1}").unwrap();
        let q = move_aside(&p, "quarantined").unwrap();
        assert!(!p.exists());
        assert_eq!(q.parent(), Some(d.path()));
        let name = q.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("daemon.live.json.quarantined-") && name.ends_with('Z'), "{name}");
        assert_eq!(std::fs::read(&q).unwrap(), b"{\"body\":1}");
        assert!(matches!(read(&p), Ok(None)));
    }

    /// A record that is too big, not a regular file or not JSON is refused
    /// with a reason; a missing one is just absent.
    #[test]
    fn a_bad_record_is_refused_and_a_missing_one_is_none() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.live.json");
        assert!(matches!(read(&p), Ok(None)));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(read(&p).is_err());
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert!(read(&p).is_err());
    }

    #[test]
    fn off_removes_a_leftover_record_only() {
        let d = tempfile::tempdir().unwrap();
        let paths = super::super::registry::daemon_paths_in(d.path(), None);
        let p = path(&paths);
        discard_when_off(&paths);
        std::fs::write(&p, b"{}").unwrap();
        discard_when_off(&paths);
        assert!(!p.exists());
        std::fs::create_dir(&p).unwrap();
        discard_when_off(&paths);
        assert!(p.is_dir(), "only a regular file is removed");
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
    }

    /// A request naming a session the restore holds waits for it; one the
    /// restore never held passes at once; a timeout says "still
    /// restoring"; a given-up session says why and leaves the listing at
    /// once but stays carried for the record; the attempt count is written
    /// until the summary (`finish`).
    #[tokio::test]
    async fn the_gate_holds_requests_until_the_session_is_back_or_given_up() {
        let r = Restoring::default();
        assert!(!r.running());
        let mut parked = entry("s-parked", false, &[]);
        parked.session.lifecycle = SessionLifecycle::Parked;
        parked.name = Some("named".into());
        r.begin(1, at("2026-10-19T15:51:07Z"), &[entry("s-a", true, &[]), entry("s-b", false, &[]), parked]);
        assert!(r.running());
        assert_eq!(r.wait("s-other", Duration::from_millis(1)).await, Ok(()));

        let mut metas = r.metas();
        metas.sort_by(|x, y| x.id.as_str().cmp(y.id.as_str()));
        assert_eq!(metas.len(), 3);
        assert!(metas.iter().all(|m| m.restoring && m.clients == 0));
        assert_eq!(metas[0].lifecycle, SessionLifecycle::Live);
        assert_eq!(metas[2].lifecycle, SessionLifecycle::Parked);
        assert_eq!(metas[2].name.as_deref(), Some("named"));
        assert_eq!(metas[0].created_at, at("2026-10-19T15:51:07Z"), "the record's time without its own");

        let r = Arc::new(r);
        let waiter = {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.wait("s-a", Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        r.settle("s-a", Ok(()));
        assert_eq!(waiter.await.unwrap(), Ok(()));
        assert_eq!(r.wait("s-a", Duration::from_millis(1)).await, Ok(()), "back: no longer held");

        let still = r.wait("s-b", Duration::from_millis(20)).await;
        assert_eq!(still, Err(GateError::StillRestoring));
        assert_eq!(still.unwrap_err().message("s-b"), "session s-b is still restoring");
        r.settle("s-b", Err("journal locked by pid 7 (tui)".into()));
        let gone = r.wait("s-b", Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(gone.message("s-b"), "session s-b could not be restored: journal locked by pid 7 (tui)");
        assert!(r.metas().iter().all(|m| m.id.as_str() == "s-parked"), "given up: not listed");

        let (carried, attempts) = r.carried();
        let mut ids: Vec<_> = carried.iter().map(|c| c.session.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["s-b", "s-parked"], "not back yet, or given up: carried unchanged");
        assert_eq!(attempts, 1);

        // A client loads the given-up one (`create --continue`): its entry
        // comes back once for the resume decision and is no longer carried
        // (its actor is recorded from then on); a second load finds nothing.
        assert_eq!(r.take_given_up_loaded("s-b", "s-b").map(|e| e.session.id), Some("s-b".to_string()));
        assert!(r.take_given_up_loaded("s-b", "s-b").is_none(), "taken once");
        assert!(r.take_given_up_loaded("s-parked", "s-parked").is_none(), "still pending, not given up");
        assert_eq!(r.wait("s-b", Duration::from_millis(1)).await, Ok(()), "no longer the restore's");
        assert_eq!(r.carried().0.len(), 1);
        r.finish();
        assert!(!r.running());
        assert_eq!(r.carried().1, 0, "after the summary the record holds no restore count");
    }

    /// Synaps's own client waits out the daemon's gate (and a parked
    /// session's unpark after it) for a session listed `restoring`, so the
    /// gate's own error reaches it, not a client-side timeout.
    #[test]
    fn the_client_attach_budget_outlasts_the_gate() {
        use crate::session::transport::{ATTACH_TIMEOUT_PARKED, ATTACH_TIMEOUT_RESTORING};
        assert!(ATTACH_TIMEOUT_RESTORING >= GATE_WAIT + ATTACH_TIMEOUT_PARKED);
    }

    /// A restore lane's cap counts from the moment its create holds the
    /// host's create lock: time queued behind another create is not
    /// counted, a create that hangs holding the lock is given up a cap
    /// later, and one that never gets the lock is given up at the queue
    /// cap, never later.
    #[tokio::test(start_paused = true)]
    async fn the_cap_counts_only_from_the_create_lock() {
        let queue_cap = Duration::from_secs(600);
        let cap = Duration::from_secs(20);
        let locked = tokio::sync::Notify::new();
        let queued_then_quick = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            locked.notify_one();
            tokio::time::sleep(Duration::from_secs(1)).await;
            "back"
        };
        assert_eq!(capped_after_lock(&locked, queue_cap, cap, queued_then_quick).await, Ok("back"));

        let locked = tokio::sync::Notify::new();
        let t0 = tokio::time::Instant::now();
        let hangs_holding_the_lock = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            locked.notify_one();
            std::future::pending::<&str>().await
        };
        assert_eq!(capped_after_lock(&locked, queue_cap, cap, hangs_holding_the_lock).await, Err(Capped::Ran));
        let took = t0.elapsed();
        assert!(took >= Duration::from_secs(25) && took < Duration::from_secs(26), "{took:?}");

        // Queued just under the queue cap, then quick: still restored.
        let locked = tokio::sync::Notify::new();
        let queued_long_then_quick = async {
            tokio::time::sleep(Duration::from_secs(599)).await;
            locked.notify_one();
            tokio::time::sleep(Duration::from_secs(19)).await;
            "back"
        };
        assert_eq!(capped_after_lock(&locked, queue_cap, cap, queued_long_then_quick).await, Ok("back"));

        let locked = tokio::sync::Notify::new();
        let t0 = tokio::time::Instant::now();
        let never_locked = capped_after_lock(&locked, queue_cap, cap, std::future::pending::<()>());
        assert_eq!(never_locked.await, Err(Capped::Queued));
        let took = t0.elapsed();
        assert!(took >= queue_cap && took < queue_cap + Duration::from_secs(1), "given up at the queue cap, not a cap later: {took:?}");
        assert!(capped_why(Capped::Queued).contains("600 s for the create lock"));
        assert!(capped_why(Capped::Ran).contains("within 20 s"));
    }

    #[test]
    fn the_crash_loop_and_eviction_checks() {
        let crash = at("2026-10-19T16:00:00Z");
        assert!(crash_loop_suspected(Some(at("2026-10-19T15:40:00Z")), crash));
        assert!(!crash_loop_suspected(Some(at("2026-10-19T15:29:59Z")), crash), "past 30 min");
        assert!(!crash_loop_suspected(None, crash));
        let hour = Some(Duration::from_secs(3600));
        assert!(parked_past_eviction(Some(at("2026-10-19T14:59:00Z")), hour, crash));
        assert!(!parked_past_eviction(Some(at("2026-10-19T15:30:00Z")), hour, crash));
        assert!(!parked_past_eviction(Some(at("2026-10-19T10:00:00Z")), None, crash), "never evicted");
        assert!(!parked_past_eviction(None, hour, crash));
    }

    /// A crash entry is resumed unless a crash loop is suspected (a cut
    /// turn whose last restore resume came within the window); a stop's
    /// hand-off always resumes (crash_restore is on), a reload's follows
    /// `daemon.reload_resume`; a carried entry keeps its own origin.
    #[test]
    fn each_entry_is_told_after_its_own_origin() {
        let crashed_at = at("2026-10-19T16:00:00Z");
        let crash = Origin::Crash { writer: "1:2:b".into(), crashed_at };
        let mut cut = entry("s-cut", true, &["sa_3"]);
        let (after, looping) = after_for(&cut, &crash, true);
        assert!(!looping);
        assert!(matches!(after, After::Crash { resume: true, turn_in_flight: true, ref lost_workers, .. } if lost_workers == &["sa_3".to_string()]));
        cut.resumed_at = Some(at("2026-10-19T15:45:00Z"));
        let (after, looping) = after_for(&cut, &crash, true);
        assert!(looping);
        assert!(matches!(after, After::Crash { resume: false, .. }));
        let mut idle = entry("s-idle", false, &[]);
        idle.resumed_at = cut.resumed_at;
        assert!(!after_for(&idle, &crash, true).1, "only a cut turn counts as a loop");

        let stop = Origin::HandOff(HandOff { generation: 4, reloaded_at: crashed_at, stop: true });
        let reload = Origin::HandOff(HandOff { generation: 4, reloaded_at: crashed_at, stop: false });
        assert!(matches!(after_for(&idle, &stop, false).0, After::Reload { resume: true, stopped: true, .. }));
        assert!(matches!(after_for(&idle, &reload, false).0, After::Reload { resume: false, stopped: false, .. }));
        assert!(matches!(after_for(&idle, &reload, true).0, After::Reload { resume: true, .. }));
        idle.origin = Some(stop);
        assert!(
            matches!(after_for(&idle, &crash, true).0, After::Reload { stopped: true, .. }),
            "a carried entry keeps the record it came from"
        );
    }

    /// The wire contract (the installed app decodes `lifecycle` with no
    /// fallback): a restoring entry is listed with a lifecycle the old
    /// clients know and `restoring: true`; a whole one has no `restoring`
    /// key; the SessionMeta shape before this field decodes both.
    #[test]
    fn a_restoring_listing_entry_decodes_in_an_old_client() {
        /// `SessionMeta` as of 673314e4 (frozen; its lifecycle has exactly
        /// the four values old clients accept).
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct OldMeta {
            id: String,
            name: Option<String>,
            model: String,
            cwd: Option<PathBuf>,
            created_at: chrono::DateTime<chrono::Utc>,
            continued: bool,
            continue_info: Option<serde_json::Value>,
            host_pid: u32,
            #[serde(default)]
            lifecycle: OldLifecycle,
            #[serde(default)]
            clients: usize,
            #[serde(default)]
            input_owner: Option<u64>,
            #[serde(default)]
            awaiting_input: usize,
            #[serde(default)]
            journal_id: String,
            #[serde(default)]
            locked_by: Option<String>,
        }
        #[derive(Debug, Default, PartialEq, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldLifecycle {
            #[default]
            Live,
            Parking,
            Parked,
            Ending,
        }

        let mut parked = entry("s-parked", false, &[]);
        parked.session.lifecycle = SessionLifecycle::Parked;
        let now = chrono::Utc::now();
        for (e, want) in [(entry("s-live", true, &[]), OldLifecycle::Live), (parked, OldLifecycle::Parked)] {
            let v = serde_json::to_value(restoring_meta(&e, now)).unwrap();
            assert_eq!(v["restoring"], true, "{v}");
            let old: OldMeta = serde_json::from_value(v).unwrap();
            assert_eq!(old.lifecycle, want);
            assert_eq!(old.journal_id, e.session.id);
        }

        let mut whole = restoring_meta(&entry("s-whole", false, &[]), now);
        whole.restoring = false;
        let v = serde_json::to_value(&whole).unwrap();
        assert!(v.get("restoring").is_none(), "{v}");
        let _: OldMeta = serde_json::from_value(v.clone()).unwrap();
        let back: crate::session::SessionMeta = serde_json::from_value(v).unwrap();
        assert!(!back.restoring, "absent reads as not restoring");
    }

    /// A request names a recorded session by its id, by its journal id
    /// (after a compaction the two differ), or by a name exactly one
    /// recorded session carries (its own or its config's: a client's
    /// `--continue <name>`). A name two recorded sessions share names
    /// neither: the request passes as one the restore never held.
    #[tokio::test]
    async fn a_recorded_session_is_found_by_id_journal_id_or_its_one_name() {
        let r = Restoring::default();
        let mut compacted = entry("s-new", false, &[]);
        compacted.session.journal_id = "s-old".into();
        let mut named = entry("s-named", false, &[]);
        named.name = Some("alpha".into());
        let mut by_config = entry("s-config", false, &[]);
        by_config.session.config.name = Some("beta".into());
        let mut twin_a = entry("s-twin-a", false, &[]);
        twin_a.name = Some("twin".into());
        let mut twin_b = entry("s-twin-b", false, &[]);
        twin_b.session.config.name = Some("twin".into());
        r.begin(1, at("2026-10-19T15:51:07Z"), &[compacted, named, by_config, twin_a, twin_b]);
        {
            let inner = r.inner();
            assert_eq!(inner.key_of("s-new"), "s-new");
            assert_eq!(inner.key_of("s-old"), "s-new", "by journal id");
            assert_eq!(inner.key_of("alpha"), "s-named", "by the entry's name");
            assert_eq!(inner.key_of("beta"), "s-config", "by the config's name");
            assert_eq!(inner.key_of("twin"), "twin", "a shared name names neither");
            assert_eq!(inner.key_of("s-other"), "s-other");
        }
        // The gate follows the key: a name holds a request as the id does.
        assert_eq!(r.wait("alpha", Duration::from_millis(20)).await, Err(GateError::StillRestoring));
        assert_eq!(r.wait("twin", Duration::from_millis(1)).await, Ok(()), "not the restore's");
        // A given-up session's gate answers by name too (`create --continue
        // <name>`); its entry is then taken by the session that loaded.
        r.settle("s-named", Err("journal locked by pid 7 (tui)".into()));
        assert!(matches!(r.wait("alpha", Duration::from_millis(1)).await, Err(GateError::GivenUp(_))));
        assert_eq!(
            r.take_given_up_loaded("s-named", "s-named").map(|e| e.session.id),
            Some("s-named".to_string())
        );
    }

    /// r3 S1: a given-up session gets its one resume decision whichever
    /// way a client loads it (by name, `--continue` latest, a chain, or an
    /// attach to it live after a late cap): the entry is taken by the
    /// session that LOADED (its id or journal id), never by the client's
    /// words, once. A pending entry is the restore's, never taken.
    #[test]
    fn a_given_up_session_is_taken_by_the_session_that_loaded_it_however_it_was_named() {
        let r = Restoring::default();
        let mut compacted = entry("s-new", false, &[]);
        compacted.session.journal_id = "s-old".into();
        let mut named = entry("s-named", false, &[]);
        named.name = Some("alpha".into());
        r.begin(
            1,
            at("2026-10-19T15:51:07Z"),
            &[compacted, named, entry("s-latest", true, &[]), entry("s-pending", false, &[])],
        );
        assert!(r.take_given_up_loaded("s-pending", "s-pending").is_none(), "pending: the restore's");
        for id in ["s-new", "s-named", "s-latest"] {
            r.settle(id, Err("journal locked by pid 7 (tui)".into()));
        }
        // `--continue alpha` loaded s-named: the name itself takes nothing
        // (a name or chain may resolve elsewhere), the loaded session does.
        assert!(r.take_given_up_loaded("alpha", "alpha").is_none());
        assert_eq!(
            r.take_given_up_loaded("s-named", "s-named").map(|e| e.session.id),
            Some("s-named".to_string())
        );
        // `--continue` (latest) loaded s-latest: no target word at all.
        assert_eq!(
            r.take_given_up_loaded("s-latest", "s-latest").map(|e| e.session.id),
            Some("s-latest".to_string())
        );
        assert!(r.take_given_up_loaded("s-latest", "s-latest").is_none(), "once");
        // A loaded session matched by its journal id.
        assert_eq!(
            r.take_given_up_loaded("s-unrelated", "s-old").map(|e| e.session.id),
            Some("s-new".to_string())
        );
        let (carried, _) = r.carried();
        let ids: Vec<_> = carried.iter().map(|c| c.session.id.as_str()).collect();
        assert_eq!(ids, ["s-pending"], "taken entries are no longer carried");
    }

    /// r3 S1: a stop records a given-up session that is live from its
    /// actor (not the stale carried entry: old knobs, old lifecycle); an
    /// entry the restore had not finished stays carried unless the stop
    /// cut a turn of its own. The crash record's snapshot merges the same
    /// way ([`with_carried`]).
    #[test]
    fn a_stop_records_a_live_given_up_session_from_its_actor() {
        let r = Restoring::default();
        r.begin(1, at("2026-10-19T15:51:07Z"), &[entry("s-given", true, &[]), entry("s-pending", false, &[])]);
        r.settle("s-given", Err("not restored within 20 s; it stays on disk".into()));
        assert!(!r.stop_keeps_carried("s-given", false), "given up and live: the actor");
        assert!(r.stop_keeps_carried("s-pending", false), "not finished: its carried entry");
        assert!(!r.stop_keeps_carried("s-pending", true), "the stop cut its turn: the actor");
        assert!(!r.stop_keeps_carried("s-other", false), "never the restore's");

        let mut actor = entry("s-given", false, &[]);
        actor.session.lifecycle = SessionLifecycle::Parked;
        actor.name = Some("from-the-actor".into());
        let merged = with_carried(vec![actor], r.carried().0);
        let ids: Vec<_> = merged.iter().map(|s| s.session.id.as_str()).collect();
        assert_eq!(ids, ["s-given", "s-pending"], "each once, the actor first");
        assert_eq!(merged[0].name.as_deref(), Some("from-the-actor"));
        assert_eq!(merged[0].session.lifecycle, SessionLifecycle::Parked);
        assert!(!merged[0].turn_in_flight, "not the stale entry's in-flight turn");
    }

    fn capacity_stop(kind: &str) -> crate::session::CapacityStop {
        let now = chrono::Utc::now();
        crate::session::CapacityStop {
            resume_at: now + chrono::Duration::hours(1),
            stopped_at: now,
            kind: kind.into(),
            history_len: 3,
            last_digest: "0f".repeat(32),
            attempt: 0,
        }
    }

    /// D5: a session the restore gives up has no live actor to carry its
    /// capacity resume, so the resume goes with it. A give-up (`settle`
    /// with an error) and an abandoned restore strip it from the carried
    /// entry, one INFO line each (through the daemon's log filter; the
    /// loaded kind text never reaches it). Every record written from then
    /// on, a stop's included, carries the entry without it, and a client's
    /// later load takes the entry without it (`loaded_on_attach` arms
    /// nothing). An entry the restore has not finished keeps its resume
    /// (still owed); a restored one is its actor's.
    #[test]
    fn a_given_up_session_drops_its_capacity_resume() {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let with_stop = |id: &str, kind: &str| {
            let mut e = entry(id, false, &[]);
            e.session.capacity_stop = Some(capacity_stop(kind));
            e
        };
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        let r = Restoring::default();
        tracing::subscriber::with_default(subscriber, || {
            r.begin(
                1,
                at("2026-10-19T15:51:07Z"),
                &[
                    with_stop("s-given", "all_cooling_down"),
                    with_stop("s-pending", "<b>loaded</b>"),
                    with_stop("s-back", "all_cooling_down"),
                ],
            );
            r.settle("s-back", Ok(()));
            r.settle("s-given", Err("journal locked by pid 7 (tui)".into()));
            let (carried, _) = r.carried();
            let armed = |id: &str| {
                carried.iter().find(|c| c.session.id == id).map(|c| c.session.capacity_stop.is_some())
            };
            assert_eq!(armed("s-given"), Some(false), "given up: dropped");
            assert_eq!(armed("s-pending"), Some(true), "not finished: still owed");
            assert_eq!(armed("s-back"), None, "restored: its actor's");
            let stopped = with_carried(Vec::new(), r.carried().0);
            let given = stopped.iter().find(|s| s.session.id == "s-given").expect("still carried");
            assert_eq!(given.session.capacity_stop, None, "a stop's record carries none");
            assert_eq!(
                r.take_given_up_loaded("s-given", "s-given").map(|e| e.session.capacity_stop),
                Some(None),
                "a client's load takes it without the resume"
            );
            assert_eq!(r.abandon("the restore failed; it stays on disk"), ["s-pending"]);
            assert!(
                r.carried().0.iter().all(|c| c.session.capacity_stop.is_none()),
                "abandoned: dropped"
            );
        });
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        assert_eq!(text.matches("capacity resume: dropped").count(), 2, "{text:?}");
        assert!(text.contains("session=s-given") && text.contains("session=s-pending"), "{text:?}");
        assert!(!text.contains("s-back"), "{text:?}");
        assert_eq!(text.matches("given_up").count(), 2, "{text:?}");
        assert!(text.contains("INFO") && text.contains("agent_engine::session::capacity_resume"), "{text:?}");
        assert!(!text.contains("<b>") && text.contains("invalid"), "the loaded kind never reaches the log: {text:?}");
        assert!(!text.contains(&"0f".repeat(32)), "nor the digest: {text:?}");
    }

    /// A panic in the restore task gives up every session it had not
    /// finished: a waiter gets the reason, the entry leaves the listing and
    /// stays carried for the record, and the restore is over (no restore
    /// count, so reloads and the idle exit go on). A task dropped without a
    /// panic (a stop's abort) changes nothing: the stop's record carries the
    /// entries unchanged.
    #[tokio::test]
    async fn a_panicking_restore_gives_up_what_it_had_not_finished() {
        let r = Arc::new(Restoring::default());
        r.begin(2, at("2026-10-19T15:51:07Z"), &[entry("s-a", true, &[]), entry("s-b", false, &[])]);
        {
            let _aborted = FinishOnPanic(r.as_ref());
        }
        assert!(r.running(), "a drop without a panic changes nothing");
        assert_eq!(r.metas().len(), 2);

        r.settle("s-a", Ok(()));
        let waiter = {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.wait("s-b", Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = FinishOnPanic(r.as_ref());
            panic!("a restore lane panicked");
        }));
        assert!(caught.is_err());
        let gone = waiter.await.unwrap().unwrap_err();
        assert_eq!(
            gone.message("s-b"),
            "session s-b could not be restored: the restore failed; it stays on disk"
        );
        assert!(!r.running(), "the restore is over");
        assert!(r.metas().is_empty(), "given up: not listed");
        assert_eq!(r.wait("s-a", Duration::from_millis(1)).await, Ok(()), "already back: untouched");
        let (carried, attempts) = r.carried();
        let ids: Vec<_> = carried.iter().map(|c| c.session.id.as_str()).collect();
        assert_eq!(ids, ["s-b"], "the unfinished entry stays carried");
        assert_eq!(attempts, 0, "no restore count once the restore ended");
    }

    /// A listing shows each session once: from its actor when it has one
    /// (matched by id, or by journal id: a compacted session's actor has a
    /// new id), else from the restore set (`restoring: true`). With nothing
    /// restoring it is the live listing unchanged.
    #[test]
    fn a_listing_shows_a_session_from_its_actor_or_else_from_the_restore() {
        let now = chrono::Utc::now();
        let whole = |id: &str, journal: &str| {
            let mut m = restoring_meta(&entry(id, false, &[]), now);
            m.journal_id = journal.into();
            m.restoring = false;
            m
        };
        let live = vec![whole("s-live", "s-live"), whole("s-compacted-new", "s-compacted")];
        let restoring: Vec<_> = [entry("s-live", false, &[]), entry("s-compacted", false, &[]), entry("s-pending", true, &[])]
            .iter()
            .map(|e| restoring_meta(e, now))
            .collect();
        let listed = |metas: &[crate::session::SessionMeta]| -> Vec<(String, bool)> {
            metas.iter().map(|m| (m.id.as_str().to_string(), m.restoring)).collect()
        };
        assert_eq!(
            listed(&merge_listing(live.clone(), restoring)),
            [
                ("s-live".to_string(), false),
                ("s-compacted-new".to_string(), false),
                ("s-pending".to_string(), true)
            ],
            "the actor wins by id and by journal id; the rest is listed restoring"
        );
        assert_eq!(listed(&merge_listing(live.clone(), Vec::new())), listed(&live));
    }

    /// `f`'s log lines through the daemon's own filter (`with_crate_directives`
    /// over a `RUST_LOG`-unset base).
    fn daemon_log<T>(f: impl FnOnce() -> T) -> (T, String) {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        (result, text)
    }

    /// A1: a recorded capacity resume that does not parse (a wrong type, a
    /// missing field, an attempt out of range, a kind that is no string)
    /// drops only itself. Every session of the record reads, with its other
    /// fields, the valid resume beside them stays, and each dropped one logs
    /// exactly one WARN of a fixed class. Neither the stored value nor
    /// serde's message (which quotes it) reaches a log line.
    #[test]
    fn a_malformed_capacity_resume_drops_only_itself_from_the_crash_record() {
        use crate::session::capacity_resume::fixtures::{all_levels_log, malformed_stops, SENTINEL};
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.live.json");
        let good = capacity_stop("all_cooling_down");
        let mut kept = entry("s-good", false, &[]);
        kept.session.capacity_stop = Some(good.clone());
        let writer = Writer { pid: 4242, start_ticks: 77, boot_id: None };
        let mut v = serde_json::to_value(CrashRecord { sessions: vec![kept], ..record(writer) }).unwrap();
        let malformed = malformed_stops();
        for (id, stop) in &malformed {
            let mut e = serde_json::to_value(entry(id, true, &["sa_9"])).unwrap();
            e["capacity_stop"] = stop.clone();
            v["sessions"].as_array_mut().unwrap().push(e);
        }
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();

        let (back, text) = daemon_log(|| read(&p));
        let back = back.expect("the record reads").expect("a record");
        let ids: Vec<_> = back.sessions.iter().map(|s| s.session.id.as_str()).collect();
        let mut want = vec!["s-good"];
        want.extend(malformed.iter().map(|(id, _)| *id));
        assert_eq!(ids, want, "every session reads");
        assert_eq!(back.sessions[0].session.capacity_stop, Some(good), "the valid resume stays");
        for s in &back.sessions[1..] {
            assert_eq!(s.session.capacity_stop, None, "{}: dropped", s.session.id);
            assert!(s.turn_in_flight && s.running_workers == ["sa_9"], "{}: the rest reads", s.session.id);
        }
        assert_eq!(
            text.matches("capacity resume: a recorded resume does not parse; dropped").count(),
            malformed.len(),
            "one WARN each: {text:?}"
        );
        assert_eq!(text.matches("WARN").count(), malformed.len(), "{text:?}");
        assert_eq!(text.matches("class=\"malformed\"").count(), malformed.len(), "{text:?}");
        assert!(text.contains("agent_engine::session::capacity_resume"), "{text:?}");
        assert!(!text.contains(SENTINEL), "no stored value in the log: {text:?}");
        let (again, all) = all_levels_log(|| read(&p));
        assert_eq!(again.expect("the record reads").expect("a record").sessions.len(), want.len());
        assert_eq!(all.matches("class=\"malformed\"").count(), malformed.len(), "{all:?}");
        assert!(!all.contains(SENTINEL), "no stored value at any level: {all:?}");
    }

    /// AB1: a recorded resume past the JSON reader's limits (a number out
    /// of range, nesting past 128 levels), as raw text in the crash file,
    /// drops only itself through the daemon's own read from bytes: the
    /// entry takes the resume's raw text apart, outside its flatten, before
    /// any typed decoding. Every session reads, with its other fields; a
    /// valid resume beside them stays; each dropped one logs one WARN of a
    /// fixed class; the stored value reaches no line at any level.
    #[test]
    fn a_capacity_resume_past_the_readers_limits_drops_only_itself_from_the_crash_record() {
        use crate::session::capacity_resume::fixtures::{
            all_levels_log, daemon_log, raw_malformed_stops, with_raw_stops, SENTINEL,
        };
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.live.json");
        let good = capacity_stop("all_cooling_down");
        let mut kept = entry("s-good", false, &[]);
        kept.session.capacity_stop = Some(good.clone());
        let raw = raw_malformed_stops();
        let mut sessions = vec![kept];
        sessions.extend(raw.iter().map(|(id, _)| entry(id, true, &["sa_9"])));
        let writer = Writer { pid: 4242, start_ticks: 77, boot_id: None };
        let doc = serde_json::to_value(CrashRecord { sessions, ..record(writer) }).unwrap();
        std::fs::write(&p, with_raw_stops(doc, &raw)).unwrap();

        let (back, text) = daemon_log(|| read(&p));
        let back = back.expect("the record reads").expect("a record");
        let ids: Vec<_> = back.sessions.iter().map(|s| s.session.id.as_str()).collect();
        let mut want = vec!["s-good"];
        want.extend(raw.iter().map(|(id, _)| *id));
        assert_eq!(ids, want, "every session reads");
        assert_eq!(back.sessions[0].session.capacity_stop, Some(good), "the valid resume stays");
        for s in &back.sessions[1..] {
            assert_eq!(s.session.capacity_stop, None, "{}: dropped", s.session.id);
            assert!(s.turn_in_flight && s.running_workers == ["sa_9"], "{}: the rest reads", s.session.id);
        }
        let dropped = "capacity resume: a recorded resume does not parse; dropped";
        assert_eq!(text.matches(dropped).count(), raw.len(), "one WARN each: {text:?}");
        assert_eq!(text.matches("WARN").count(), raw.len(), "{text:?}");
        assert_eq!(text.matches("class=\"malformed\"").count(), raw.len(), "{text:?}");
        assert!(!text.contains(SENTINEL), "no stored value in the log: {text:?}");
        let (again, all) = all_levels_log(|| read(&p));
        assert_eq!(again.expect("the record reads").expect("a record").sessions.len(), want.len());
        assert_eq!(all.matches("class=\"malformed\"").count(), raw.len(), "{all:?}");
        assert!(!all.contains(SENTINEL), "no stored value at any level: {all:?}");
    }

    /// The crash entry's reader takes the resume apart from the flatten
    /// (`LiveSessionWire`), and nothing else about an entry changes: it is
    /// written byte for byte as 25cee355's derive wrote it (`Before` below
    /// is that derive), and today's bytes (with a resume, without one, with
    /// `null`, with keys a later image adds) read back to the same entry. A
    /// number out of range in any OTHER field still fails the entry, as it
    /// did before (the documented limit).
    #[test]
    fn a_crash_entry_reads_and_writes_as_before_with_its_resume_read_apart() {
        #[derive(Serialize)]
        struct Before {
            #[serde(flatten)]
            session: ReloadSession,
            #[serde(default)]
            turn_in_flight: bool,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            running_workers: Vec<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            origin: Option<Origin>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            resumed_at: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            parked_since: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            name: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            created_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let before = |e: &LiveSession| Before {
            session: e.session.clone(),
            turn_in_flight: e.turn_in_flight,
            running_workers: e.running_workers.clone(),
            origin: e.origin.clone(),
            resumed_at: e.resumed_at,
            parked_since: e.parked_since,
            name: e.name.clone(),
            created_at: e.created_at,
        };
        let now = chrono::Utc::now();
        let mut full = entry("s-full", true, &["sa_1", "sa_2"]);
        full.session.capacity_stop = Some(capacity_stop("all_cooling_down"));
        full.session.cut_tail = Some(2);
        full.origin = Some(Origin::Crash { writer: "w-1".into(), crashed_at: now });
        full.resumed_at = Some(now);
        full.parked_since = Some(now);
        full.name = Some("a name".into());
        full.created_at = Some(now);
        let bare = entry("s-bare", false, &[]);
        let bytes = |e: &LiveSession| serde_json::to_vec(e).unwrap();
        for e in [&full, &bare] {
            let id = &e.session.id;
            assert_eq!(bytes(e), serde_json::to_vec(&before(e)).unwrap(), "{id}: written as before");
            let read = |b: &[u8]| -> LiveSession { serde_json::from_slice(b).expect("reads") };
            assert_eq!(bytes(&read(&bytes(e))), bytes(e), "{id}: today's bytes read back the same");
            assert_eq!(read(&bytes(e)).session.capacity_stop, e.session.capacity_stop, "{id}");

            let mut v = serde_json::to_value(e).unwrap();
            v["capacity_stop"] = serde_json::Value::Null;
            let nulled = read(&serde_json::to_vec(&v).unwrap());
            assert_eq!(nulled.session.capacity_stop, None, "{id}: null is none, as before");
            let mut none = e.clone();
            none.session.capacity_stop = None;
            assert_eq!(bytes(&nulled), bytes(&none), "{id}: the rest unchanged");

            let mut v = serde_json::to_value(e).unwrap();
            v["a_later_key"] = serde_json::json!({ "nested": [1, { "x": null }], "n": 1.5 });
            v["capacity_stop_note"] = serde_json::json!("not the resume");
            let later = read(&serde_json::to_vec(&v).unwrap());
            assert_eq!(bytes(&later), bytes(e), "{id}: keys a later image adds are ignored, as before");

            let mut v = serde_json::to_value(e).unwrap();
            v["a_later_key"] = serde_json::json!("capres-raw-other");
            let text = serde_json::to_string(&v).unwrap().replace("\"capres-raw-other\"", "1e400");
            assert!(
                serde_json::from_str::<LiveSession>(&text).is_err(),
                "{id}: out of range in another field still fails the entry (the limit)"
            );
        }
    }

    /// A1: a record that does not parse at all is unusable, and says so with
    /// a fixed class and where (line, column), never with serde's message,
    /// which quotes the stored value.
    #[test]
    fn an_unusable_crash_record_names_no_stored_value() {
        use crate::session::capacity_resume::fixtures::SENTINEL;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.live.json");
        let writer = Writer { pid: 4242, start_ticks: 77, boot_id: None };
        let mut v = serde_json::to_value(record(writer)).unwrap();
        v["generation"] = serde_json::json!(SENTINEL);
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        let why = read(&p).expect_err("unusable");
        assert!(why.starts_with("does not parse (data, line 1, column "), "{why}");
        assert!(!why.contains(SENTINEL), "{why}");
        std::fs::write(&p, b"{\"version\": 1,").unwrap();
        let why = read(&p).expect_err("unusable");
        assert!(why.starts_with("does not parse (eof, line 1, column "), "{why}");
    }

    /// A2: a recorded capacity resume is validated before the parked
    /// eviction check, for every entry the restore screens. One that is well
    /// typed but invalid (an unknown kind, an attempt past the cap) is
    /// stripped with exactly one WARN of a fixed class (the loaded text never
    /// reaches the log) and earns no hold, so a parked entry past its
    /// eviction age is left on disk. A valid one holds it, with no WARN; a
    /// live entry with an invalid one is restored without it.
    #[test]
    fn an_invalid_recorded_capacity_resume_is_dropped_before_the_eviction_check() {
        use crate::session::capacity_resume::fixtures::{all_levels_log, SENTINEL};
        let now = chrono::Utc::now();
        let evict = Some(Duration::from_secs(3600));
        let parked = |id: &str, stop: Option<crate::session::CapacityStop>| {
            let mut e = entry(id, false, &[]);
            e.session.lifecycle = SessionLifecycle::Parked;
            e.parked_since = Some(now - chrono::Duration::hours(2));
            e.session.capacity_stop = stop;
            e
        };
        let bad_attempt = crate::session::CapacityStop { attempt: 3, ..capacity_stop("all_cooling_down") };
        let mut expired_kind = parked("s-expired-kind", Some(capacity_stop(SENTINEL)));
        let mut expired_attempt = parked("s-expired-attempt", Some(bad_attempt));
        let mut expired_valid = parked("s-expired-valid", Some(capacity_stop("all_cooling_down")));
        let mut expired_none = parked("s-expired-none", None);
        let mut live_kind = entry("s-live-kind", false, &[]);
        live_kind.session.capacity_stop = Some(capacity_stop(SENTINEL));
        let again = [
            expired_kind.clone(),
            expired_attempt.clone(),
            expired_valid.clone(),
            expired_none.clone(),
            live_kind.clone(),
        ];

        let ((), text) = daemon_log(|| {
            assert!(!screen_entry(&mut expired_kind, evict, true, now), "invalid: no hold");
            assert!(!screen_entry(&mut expired_attempt, evict, true, now), "invalid: no hold");
            assert!(screen_entry(&mut expired_valid, evict, true, now), "valid: held");
            assert!(!screen_entry(&mut expired_none, evict, true, now), "none: left on disk");
            assert!(screen_entry(&mut live_kind, evict, true, now), "live: restored");
        });
        for e in [&expired_kind, &expired_attempt, &live_kind] {
            assert_eq!(e.session.capacity_stop, None, "{}: stripped", e.session.id);
        }
        assert!(expired_valid.session.capacity_stop.is_some(), "the valid one stays");
        let dropped = "capacity resume: the recorded stop is invalid; dropped";
        assert_eq!(text.matches(dropped).count(), 3, "{text:?}");
        assert_eq!(text.matches("WARN").count(), 3, "{text:?}");
        assert_eq!(text.matches("class=\"invalid_kind\"").count(), 2, "{text:?}");
        assert_eq!(text.matches("class=\"bad_attempt\"").count(), 1, "{text:?}");
        for id in ["s-expired-kind", "s-expired-attempt", "s-live-kind"] {
            let line = text.lines().find(|l| l.contains(dropped) && l.contains(&format!("session={id}")));
            assert!(line.is_some(), "{id}: {text:?}");
        }
        assert!(!text.contains(SENTINEL), "the loaded kind never reaches the log: {text:?}");
        let warned = |id: &str| text.lines().any(|l| l.contains("WARN") && l.contains(&format!("session={id}")));
        assert!(!warned("s-expired-valid") && !warned("s-expired-none"), "{text:?}");
        let ((), all) = all_levels_log(move || {
            for mut e in again {
                screen_entry(&mut e, evict, true, now);
            }
        });
        assert_eq!(all.matches(dropped).count(), 3, "{all:?}");
        assert!(!all.contains(SENTINEL), "the loaded kind reaches no line at any level: {all:?}");
    }

    /// SC-SF1: an OLD hand-off after a consumed resume, composed from the
    /// real parts: the session's slot (`live::LiveSlot`), the record link's
    /// seal, this Recorder's writes and acknowledgements, and the reload
    /// state a hand-off is built from (here from the slot, as a stop's or a
    /// reload's snapshot takes the published entry's resume). A stop or a
    /// reload seals BEFORE it builds its snapshot, so either (a) the seal
    /// came first: the snapshot holds the armed stop; the consumption is
    /// still published, written and acknowledged, but the final admission
    /// refuses, nothing is dispatched, and the old hand-off written after it
    /// carries the stop to the next image (owed once, none given); or (b)
    /// the admission came first (its consumed entry was published before
    /// it): the seal, the snapshot and the hand-off that follow hold no
    /// armed stop. In neither order is a dispatched resume recorded armed.
    #[tokio::test]
    async fn an_old_hand_off_after_a_consumed_resume_never_carries_a_dispatched_one() {
        let stop = capacity_stop("all_cooling_down");
        let reload_record = |stop: Option<crate::session::CapacityStop>| {
            let rec: crate::session::SessionReloadRecord = serde_json::from_value(serde_json::json!({
                "config": {}, "keep_warm": false, "lifecycle": "live", "model": "m", "thinking_level": "low"
            }))
            .unwrap();
            crate::session::SessionReloadRecord { capacity_stop: stop, ..rec }
        };
        let slot_entry = |gen: u64, stop: Option<crate::session::CapacityStop>| {
            Arc::new(live::LiveEntry {
                record: reload_record(stop),
                turn_in_flight: false,
                running_workers: Vec::new(),
                resumed_at: None,
                parked_since: None,
                gen,
            })
        };
        // A stop's or a reload's snapshot of the slot, after its seal.
        let hand_off_state = |slot: &live::LiveSlot| ReloadState {
            generation: 3,
            written_at: chrono::Utc::now(),
            sessions: vec![ReloadSession {
                capacity_stop: slot.load_full().and_then(|e| e.record.capacity_stop.clone()),
                ..session("s-armed")
            }],
            expected_clients: 0,
            next_subagent_id: 0,
            base_umask: None,
            children: Vec::new(),
        };
        // The update writer: the change count read before its snapshot of the
        // slot, the write, and its stamp (only when written).
        let write_update = |rec: &Recorder, slot: &live::LiveSlot| {
            let started = live::changes_now();
            let e = slot.load_full().expect("published");
            let mut s = entry("s-armed", false, &[]);
            s.session.capacity_stop = e.record.capacity_stop.clone();
            let body =
                serde_json::to_vec(&CrashRecord { sessions: vec![s], ..record(rec.writer().clone()) }).unwrap();
            rec.write_snapshot(&body, started, &[(slot.clone(), e.gen)])
        };
        let on_disk = |rec: &Recorder| read(rec.path()).unwrap().unwrap().sessions[0].session.capacity_stop.clone();

        // (a) The seal before the armed snapshot.
        let d = tempfile::tempdir().unwrap();
        let Some(rec) = Recorder::new(&super::super::registry::daemon_paths_in(d.path(), None)) else {
            return;
        };
        let link = live::RecordLink::new();
        let slot = live::LiveSlot::default();
        slot.store(Some(slot_entry(1, Some(stop.clone()))));
        live::mark_dirty(false);
        assert!(write_update(&rec, &slot), "the armed entry is on disk");
        link.seal();
        let old = hand_off_state(&slot);
        assert_eq!(old.sessions[0].capacity_stop, Some(stop.clone()), "the snapshot holds the armed stop");
        // The consumption: the entry without the stop, published, written
        // and acknowledged by a write that started after the publish.
        slot.store(Some(slot_entry(2, None)));
        live::mark_dirty(false);
        let after = live::changes_now();
        assert!(write_update(&rec, &slot));
        assert!(slot.await_durable(after, 2, Duration::from_secs(1)).await, "the consumption is acknowledged");
        assert_eq!(on_disk(&rec), None, "consumed on disk");
        let dispatched = link.admit();
        assert!(!dispatched, "sealed before the snapshot: the final admission refuses");
        rec.hand_off(&old);
        assert_eq!(on_disk(&rec), Some(stop.clone()), "the old hand-off carries the undispatched stop");
        assert!(!write_update(&rec, &slot), "nothing is written after the hand-off");

        // (b) The admission before the seal.
        let d = tempfile::tempdir().unwrap();
        let Some(rec) = Recorder::new(&super::super::registry::daemon_paths_in(d.path(), None)) else {
            return;
        };
        let link = live::RecordLink::new();
        let slot = live::LiveSlot::default();
        slot.store(Some(slot_entry(1, Some(stop.clone()))));
        live::mark_dirty(false);
        assert!(write_update(&rec, &slot));
        slot.store(Some(slot_entry(2, None)));
        live::mark_dirty(false);
        let after = live::changes_now();
        assert!(write_update(&rec, &slot));
        assert!(slot.await_durable(after, 2, Duration::from_secs(1)).await);
        let dispatched = link.admit();
        assert!(dispatched, "nothing sealed: admitted after the consumption");
        link.seal();
        let later = hand_off_state(&slot);
        assert_eq!(later.sessions[0].capacity_stop, None, "the snapshot after the seal holds no armed stop");
        rec.hand_off(&later);
        assert_eq!(on_disk(&rec), None, "the hand-off carries no dispatched resume");
    }
}
