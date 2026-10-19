//! Pinned sessions across daemon restarts.
//!
//! The pin set lives in `<active config dir>/daemon-pins.json` (one per
//! profile), not in the run dir: an older `synaps` unlinks run-dir `*.json`
//! files it can't parse as session cards, and `SYNAPS_RUNTIME_DIR` may be a
//! tmpfs a reboot wipes. Not in the journal either: an older binary
//! re-saving it would drop the flag, and the non-persisted knobs
//! (`settings_replay`, `/keepwarm`) have no home there.
//!
//! **One writer.** The keeper task is the only code that writes the file
//! (tmp + rename, 0600, and only while this image holds the daemon flock).
//! It writes "live pinned sessions + carried entries" on change and on a
//! 2 s tick. At shutdown [`super::Daemon::wait`] lets it take ONE final
//! snapshot while every pinned session is still live, freezes it, and only
//! then ends the sessions: `daemon stop`, SIGTERM and idle exit keep the
//! set; unpin, `/close`, `daemon close` and `/quit` from `synaps attach`
//! remove a session while the daemon is serving, so the next snapshot
//! drops it; a crash leaves the last snapshot.
//!
//! **Restore** runs on a fresh start only (a reload rehydrates its own
//! sessions), after the ready byte (a slow `create` must not push the
//! spawner past its ready timeout), as a background task that holds the
//! idle exit (`DaemonState::restoring`). Entries go one at a time.
//!
//! **Crash loops.** Release builds are `panic = "abort"`: one actor panic
//! kills the daemon and every session in it, and a restore that panics
//! would do it again on every start. So each attempt is counted and the
//! count is written to the file BEFORE `create`; it resets once the session
//! has been up for [`STABLE_AFTER`], and after [`MAX_RESTORE_ATTEMPTS`] the
//! pin is dropped and listed under `dropped` (shown by `synaps daemon
//! sessions` / `status`). A crash in the stability window counts against
//! every entry restored in that run, not just the one at fault.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{registry, DaemonOpts, DaemonState};
use crate::session::wire::PIN_RECORD_QUERY_ID;
use crate::session::{
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionId, SessionQuery,
    SessionReloadRecord, SessionSetting,
};

pub const PINS_FILE_NAME: &str = "daemon-pins.json";
pub const PINS_FILE_VERSION: u32 = 1;
/// `0`/`false`/`off`/`no` = don't restore at start (the file is kept and its
/// entries carried). Wins over `daemon.restore_pinned`; `--no-restore` wins
/// over both.
pub const RESTORE_PINNED_ENV: &str = "SYNAPS_DAEMON_RESTORE_PINNED";
/// Attempts before a pin is dropped (each counted before its `create`).
pub const MAX_RESTORE_ATTEMPTS: u32 = 3;
/// A restored session up this long resets its attempt count.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
/// The keeper's write tick (a crash loses at most this much of the set).
pub const KEEPER_TICK: Duration = Duration::from_secs(2);
/// How often the keeper retries the journal lock of a pinned session that
/// came back as a lock-held placeholder.
pub const PLACEHOLDER_RETRY: Duration = Duration::from_secs(10);
/// Before settling for a placeholder, the lock is re-probed this often for
/// [`HELD_REPROBE_TRIES`]: a holder that is exiting (a daemon just stopped)
/// lets go within moments.
const HELD_REPROBE: Duration = Duration::from_millis(250);
const HELD_REPROBE_TRIES: u32 = 8;
/// How old a session's record may get before the keeper asks again (a crash
/// loses at most this much of `/model`, `/system`, `Set` changes).
pub const RECORD_REFRESH: Duration = Duration::from_secs(30);
/// One `Query{Record}`; past it the entry is built from the handle alone.
pub const RECORD_QUERY_BUDGET: Duration = Duration::from_secs(2);
/// One restore `create` (extensions-ready wait, `on_session_start` hooks).
pub const RESTORE_CREATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Shutdown waits this long for a restore in flight before aborting it.
pub const RESTORE_SHUTDOWN_WAIT: Duration = Duration::from_secs(10);
/// Shutdown waits this long for the keeper's final snapshot.
pub const KEEPER_FINAL_BUDGET: Duration = Duration::from_secs(5);
const NOTIFY_DEBOUNCE: Duration = Duration::from_millis(200);
const WRITE_ACK_BUDGET: Duration = Duration::from_secs(10);
const DROPPED_TTL_DAYS: i64 = 7;
const _: () = assert!(KEEPER_FINAL_BUDGET.as_secs() > RECORD_QUERY_BUDGET.as_secs());

/// `daemon-pins.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinFile {
    #[serde(default)]
    pub version: u32,
    /// The profile that wrote it; another profile's file is never restored
    /// or overwritten.
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub daemon_pid: u32,
    #[serde(default)]
    pub written_at: Option<DateTime<Utc>>,
    /// The keeper's final snapshot at a graceful stop (false = the daemon
    /// was running, or died, when this was written).
    #[serde(default)]
    pub clean_shutdown: bool,
    /// The subagent handle counter (`sa_N`) when written: restored sessions'
    /// models still remember old handles, so new workers start past it.
    #[serde(default)]
    pub next_subagent_id: u64,
    #[serde(default)]
    pub pins: Vec<PinEntry>,
    #[serde(default)]
    pub dropped: Vec<DroppedPin>,
}

/// One pinned session. `config` is what `create` gets back:
/// `continue_session` = the journal, the current model, `keep_warm`; never
/// `env` (the journal carries it; secrets stay out of a second file) or
/// `name` (the journal carries the current one).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinEntry {
    pub id: String,
    pub journal_id: String,
    /// The journal existed when this was written: if it is gone at restore
    /// the conversation is lost, and an empty session must not take its
    /// place.
    #[serde(default)]
    pub has_journal: bool,
    pub config: SessionConfig,
    #[serde(default)]
    pub settings_replay: Vec<SessionSetting>,
    #[serde(default)]
    pub cache_keepwarm: Option<String>,
    /// Workers running at the snapshot: they die with the daemon, and the
    /// restored conversation is told so.
    #[serde(default)]
    pub workers_running: Vec<PinnedWorker>,
    #[serde(default)]
    pub restore: RestoreState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorker {
    pub id: String,
    pub agent: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreState {
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub restored_at: Option<DateTime<Utc>>,
}

/// A pin given up on; listed for [`DROPPED_TTL_DAYS`] or until that journal
/// is pinned again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DroppedPin {
    pub id: String,
    pub journal_id: String,
    pub reason: String,
    pub at: DateTime<Utc>,
}

/// `<active config dir>/daemon-pins.json`.
pub fn default_path() -> PathBuf {
    agent_core::config::get_active_config_dir().join(PINS_FILE_NAME)
}

fn sessions_dir() -> PathBuf {
    agent_core::session_lock::sessions_dir()
}

fn journal_exists(dir: &Path, journal_id: &str) -> bool {
    !journal_id.is_empty()
        && (dir.join(format!("{journal_id}.json")).exists() || dir.join(format!("{journal_id}.journal")).exists())
}

fn same_profile(a: Option<&str>, b: Option<&str>) -> bool {
    let norm = |p: Option<&str>| p.filter(|s| !s.is_empty()).map(str::to_owned);
    norm(a) == norm(b)
}

/// Whether `file` was written by a daemon of `profile`.
pub fn same_profile_as(file: &PinFile, profile: Option<&str>) -> bool {
    same_profile(file.profile.as_deref(), profile)
}

/// The file, `None` when there is none. An unparsable one is renamed
/// `<name>.bad` (kept for inspection) and reads as none.
pub fn load(path: &Path) -> Option<PinFile> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "daemon: pins file unreadable; nothing restored");
            return None;
        }
    };
    match serde_json::from_slice::<PinFile>(&raw) {
        Ok(file) => Some(file),
        Err(e) => {
            let mut bad = path.as_os_str().to_owned();
            bad.push(".bad");
            let _ = std::fs::rename(path, &bad);
            tracing::warn!(
                path = %path.display(),
                moved_to = %PathBuf::from(&bad).display(),
                error = %e,
                "daemon: pins file unparsable; set aside, nothing restored"
            );
            None
        }
    }
}

/// `daemon sessions` / `status` footer: one line per dropped pin, read
/// straight from the file (same user, same profile path; nothing renamed).
pub fn dropped_lines(path: &Path) -> Vec<String> {
    let Some(file) = std::fs::read(path).ok().and_then(|raw| serde_json::from_slice::<PinFile>(&raw).ok()) else {
        return Vec::new();
    };
    let cutoff = Utc::now() - chrono::Duration::days(DROPPED_TTL_DAYS);
    file.dropped
        .iter()
        .filter(|d| d.at >= cutoff)
        .map(|d| {
            format!(
                "pin dropped: {} ({}, {})",
                d.id,
                d.reason,
                d.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
            )
        })
        .collect()
}

/// `--no-restore` / the test seam, else the env, else `daemon.restore_pinned`.
pub fn restore_enabled(opts: &DaemonOpts) -> bool {
    if let Some(on) = opts.restore_pinned {
        return on;
    }
    if let Ok(v) = std::env::var(RESTORE_PINNED_ENV) {
        return !super::env_is_off(Some(&v));
    }
    crate::config::load_config().daemon.restore_pinned
}

/// Pinned config for `create`: continue the journal, pinned, the model,
/// no env and no name.
fn pinned_config(config: SessionConfig, journal_id: &str, model: Option<String>) -> SessionConfig {
    let model_override = model.or_else(|| config.model_override.clone());
    SessionConfig {
        continue_session: Some(Some(journal_id.to_string())),
        model_override,
        keep_warm: true,
        name: None,
        env: None,
        env_stripped: Vec::new(),
        ..config
    }
}

type Record = (SessionReloadRecord, Vec<PinnedWorker>);

/// A live session's entry: from its record, or (no reply in time) from
/// what the handle alone knows, like reload's unconfirmed checkpoint.
fn entry_from(h: &SessionHandle, rec: Option<Record>, dir: &Path) -> PinEntry {
    let journal_id = h.journal_id();
    let (config, settings_replay, cache_keepwarm, model, workers_running) = match rec {
        Some((r, workers)) => (r.config, r.settings_replay, r.cache_keepwarm, r.model, workers),
        None => (
            SessionConfig {
                cwd: h.meta().cwd.clone(),
                memory_mode: h.memory_mode(),
                ..SessionConfig::default()
            },
            Vec::new(),
            h.cache_keepwarm_override(),
            h.view().model.clone(),
            Vec::new(),
        ),
    };
    PinEntry {
        id: h.id.as_str().to_string(),
        has_journal: journal_exists(dir, &journal_id),
        config: pinned_config(config, &journal_id, Some(model)),
        journal_id,
        settings_replay,
        cache_keepwarm,
        workers_running,
        restore: RestoreState::default(),
    }
}

/// A lock-held placeholder registered by a reload with no pins entry.
fn entry_from_placeholder(h: &SessionHandle, cfg: &SessionConfig, dir: &Path) -> PinEntry {
    let journal_id = h.id.as_str().to_string();
    PinEntry {
        id: journal_id.clone(),
        has_journal: journal_exists(dir, &journal_id),
        config: pinned_config(cfg.clone(), &journal_id, cfg.model_override.clone()),
        journal_id,
        settings_replay: Vec::new(),
        cache_keepwarm: None,
        workers_running: Vec::new(),
        restore: RestoreState::default(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Carry {
    /// From the file, not attempted this run (or restore is off).
    Pending,
    /// The marker is written and `create` is running.
    InFlight,
    /// Failed under the cap: tried again at the next start.
    Failed,
    /// The journal is held by another process; a placeholder stands in.
    Placeholder,
    /// Back as this live session id; the live session now speaks for it.
    Restored,
}

#[derive(Debug, Clone)]
struct Carried {
    entry: PinEntry,
    status: Carry,
}

/// What the keeper knows beyond the live handles. Behind a std mutex: every
/// use is synchronous (never held across an await).
pub struct PinBook {
    path: PathBuf,
    profile: Option<String>,
    /// The file belongs to another profile: never written this run.
    read_only: bool,
    carried: Vec<Carried>,
    /// Last record per live session id, and when it was taken.
    records: HashMap<String, (PinEntry, Instant)>,
    /// Restored this run: the attempt count until it has been up for
    /// [`STABLE_AFTER`].
    restored: HashMap<String, (RestoreState, Instant)>,
    dropped: Vec<DroppedPin>,
    next_subagent_id: u64,
    /// Set by the final snapshot: nothing is written after it.
    frozen: bool,
    on_disk: bool,
    last_key: Option<String>,
}

/// What a snapshot did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    Written,
    Unchanged,
    /// Frozen, shutting down, another profile's file, or no flock.
    Refused,
    Failed,
}

impl PinBook {
    pub fn new(path: PathBuf, profile: Option<String>, file: Option<PinFile>) -> Self {
        let mut book = Self {
            path,
            profile,
            read_only: false,
            carried: Vec::new(),
            records: HashMap::new(),
            restored: HashMap::new(),
            dropped: Vec::new(),
            next_subagent_id: 0,
            frozen: false,
            on_disk: file.is_some(),
            last_key: None,
        };
        let Some(file) = file else {
            return book;
        };
        if !same_profile(file.profile.as_deref(), book.profile.as_deref()) {
            tracing::warn!(
                path = %book.path.display(),
                file_profile = ?file.profile,
                profile = ?book.profile,
                "daemon: pins file belongs to another profile; not restoring it and not writing it this run"
            );
            book.read_only = true;
            return book;
        }
        book.last_key = Some(content_key(&file));
        book.next_subagent_id = file.next_subagent_id;
        book.dropped = file.dropped;
        book.carried = file
            .pins
            .into_iter()
            .map(|entry| Carried { entry, status: Carry::Pending })
            .collect();
        book
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn pending(&self) -> Vec<PinEntry> {
        self.carried
            .iter()
            .filter(|c| c.status == Carry::Pending)
            .map(|c| c.entry.clone())
            .collect()
    }

    fn placeholders(&self) -> Vec<PinEntry> {
        self.carried
            .iter()
            .filter(|c| c.status == Carry::Placeholder)
            .map(|c| c.entry.clone())
            .collect()
    }

    fn set(&mut self, entry: &PinEntry, status: Carry) {
        match self.carried.iter_mut().find(|c| c.entry.id == entry.id) {
            Some(c) => {
                c.entry = entry.clone();
                c.status = status;
            }
            None => self.carried.push(Carried { entry: entry.clone(), status }),
        }
    }

    fn drop_pin(&mut self, entry: &PinEntry, reason: String) {
        self.carried.retain(|c| c.entry.id != entry.id);
        self.dropped.retain(|d| d.id != entry.id);
        self.dropped.push(DroppedPin {
            id: entry.id.clone(),
            journal_id: entry.journal_id.clone(),
            reason,
            at: Utc::now(),
        });
    }

    fn mark_restored(&mut self, entry: &PinEntry, h: &SessionHandle, now: Instant) {
        let live_id = h.id.as_str().to_string();
        let state = RestoreState {
            restored_at: Some(Utc::now()),
            last_error: None,
            ..entry.restore.clone()
        };
        self.restored.insert(live_id.clone(), (state, now));
        let record = PinEntry {
            id: live_id.clone(),
            journal_id: h.journal_id(),
            // They died with the previous daemon; the conversation was told.
            workers_running: Vec::new(),
            restore: RestoreState::default(),
            ..entry.clone()
        };
        self.records.insert(live_id, (record, now));
        self.set(entry, Carry::Restored);
    }

    /// A restore aborted mid-`create` (shutdown): the attempt never ran to
    /// an outcome, so it is not counted.
    fn rollback_in_flight(&mut self) {
        for c in self.carried.iter_mut().filter(|c| c.status == Carry::InFlight) {
            c.entry.restore.attempts = c.entry.restore.attempts.saturating_sub(1);
            c.status = Carry::Pending;
        }
    }

    fn needs_record(&self, h: &SessionHandle, refresh: Refresh, now: Instant) -> bool {
        match refresh {
            Refresh::No => false,
            Refresh::All => true,
            Refresh::Stale => match self.records.get(h.id.as_str()) {
                None => true,
                Some((e, at)) => {
                    now.saturating_duration_since(*at) >= RECORD_REFRESH || e.journal_id != h.journal_id()
                }
            },
        }
    }

    /// The set as it stands: every live pinned session, plus the carried
    /// entries no live session speaks for. A live session (pinned or not)
    /// decides for its own entry: unpinning it drops the entry.
    pub fn compose(&mut self, live: &[SessionHandle], dir: &Path, now: Instant, clean_shutdown: bool) -> PinFile {
        let live: Vec<&SessionHandle> = live.iter().filter(|h| h.is_alive()).collect();
        let real: Vec<&SessionHandle> = live.iter().copied().filter(|h| h.placeholder_config().is_none()).collect();
        let placeholders: HashSet<&str> =
            live.iter().filter(|h| h.placeholder_config().is_some()).map(|h| h.id.as_str()).collect();
        let mut live_keys: HashSet<String> = HashSet::new();
        for h in &real {
            live_keys.insert(h.id.as_str().to_string());
            live_keys.insert(h.journal_id());
        }
        let real_ids: HashSet<String> = real.iter().map(|h| h.id.as_str().to_string()).collect();
        self.records.retain(|id, _| real_ids.contains(id));
        self.restored
            .retain(|id, (_, since)| real_ids.contains(id) && now.saturating_duration_since(*since) < STABLE_AFTER);

        let mut pins: Vec<PinEntry> = Vec::new();
        for h in real.iter().filter(|h| h.pinned()) {
            let id = h.id.as_str();
            let journal_id = h.journal_id();
            // A restore whose `create` just returned: its entry carries the
            // settings and the attempt count until a record and the
            // stability window take over.
            let carried = self.carried.iter().find(|c| {
                matches!(c.status, Carry::InFlight | Carry::Restored) && (c.entry.id == id || c.entry.journal_id == journal_id)
            });
            let mut entry = match (self.records.get(id), carried) {
                (Some((e, _)), _) => e.clone(),
                (None, Some(c)) => c.entry.clone(),
                (None, None) => entry_from(h, None, dir),
            };
            entry.id = id.to_string();
            entry.config.continue_session = Some(Some(journal_id.clone()));
            entry.has_journal = journal_exists(dir, &journal_id);
            entry.journal_id = journal_id;
            entry.restore = match (self.restored.get(id), carried) {
                (Some((st, _)), _) => st.clone(),
                (None, Some(c)) if c.status == Carry::InFlight => c.entry.restore.clone(),
                _ => RestoreState::default(),
            };
            pins.push(entry);
        }

        self.carried.retain_mut(|c| {
            if c.status == Carry::InFlight {
                return true; // the restore task settles it
            }
            if live_keys.contains(&c.entry.id) || live_keys.contains(&c.entry.journal_id) {
                return false;
            }
            if placeholders.contains(c.entry.id.as_str()) {
                c.status = Carry::Placeholder;
                return true;
            }
            // A placeholder that was closed, or a restored session that
            // already ended: the user's end decides.
            !matches!(c.status, Carry::Placeholder | Carry::Restored)
        });
        for c in &self.carried {
            if !(live_keys.contains(&c.entry.id) || live_keys.contains(&c.entry.journal_id)) {
                pins.push(c.entry.clone());
            }
        }
        for h in live.iter().filter(|h| placeholders.contains(h.id.as_str())) {
            let Some(cfg) = h.placeholder_config() else { continue };
            if cfg.keep_warm && !pins.iter().any(|p| p.id == h.id.as_str()) {
                pins.push(entry_from_placeholder(h, cfg, dir));
            }
        }

        let cutoff = Utc::now() - chrono::Duration::days(DROPPED_TTL_DAYS);
        self.dropped
            .retain(|d| d.at >= cutoff && !pins.iter().any(|p| p.id == d.id || p.journal_id == d.journal_id));
        self.next_subagent_id = self.next_subagent_id.max(crate::tools::peek_next_subagent_id());

        PinFile {
            version: PINS_FILE_VERSION,
            profile: self.profile.clone(),
            daemon_pid: std::process::id(),
            written_at: Some(Utc::now()),
            clean_shutdown,
            next_subagent_id: self.next_subagent_id,
            pins,
            dropped: self.dropped.clone(),
        }
    }

    /// Write `file` if it changed. Never once frozen, never another
    /// profile's file, never without the daemon flock (`lock_held`): after
    /// `Daemon::wait` drops it, nothing may write. No file is created for
    /// an empty set.
    pub fn persist(&mut self, file: &PinFile, lock_held: bool, final_snapshot: bool) -> std::io::Result<Snap> {
        if self.read_only || self.frozen || !lock_held {
            return Ok(Snap::Refused);
        }
        let key = content_key(file);
        let changed = self.last_key.as_deref() != Some(key.as_str());
        if !(changed || final_snapshot && self.on_disk) {
            return Ok(Snap::Unchanged);
        }
        if !self.on_disk && file.pins.is_empty() && file.dropped.is_empty() {
            self.last_key = Some(key);
            return Ok(Snap::Unchanged);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
        registry::write_private_atomic(&self.path, &body)?;
        self.on_disk = true;
        self.last_key = Some(key);
        Ok(Snap::Written)
    }

    pub fn freeze(&mut self) {
        self.frozen = true;
    }
}

/// What a write is compared on: the set, the dropped list, and whether it
/// was a clean stop (not the pid, timestamp or worker counter).
fn content_key(file: &PinFile) -> String {
    serde_json::to_string(&(&file.pins, &file.dropped, file.clean_shutdown)).unwrap_or_default()
}

/// The daemon's half: the book, the keeper's wake-up and its write queue.
pub struct Pins {
    book: Mutex<PinBook>,
    notify: tokio::sync::Notify,
    write_tx: mpsc::UnboundedSender<oneshot::Sender<bool>>,
    write_rx: Mutex<Option<mpsc::UnboundedReceiver<oneshot::Sender<bool>>>>,
}

impl Pins {
    pub fn new(book: PinBook) -> Self {
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        Self {
            book: Mutex::new(book),
            notify: tokio::sync::Notify::new(),
            write_tx,
            write_rx: Mutex::new(Some(write_rx)),
        }
    }

    pub fn book(&self) -> MutexGuard<'_, PinBook> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A pin may have changed (pin/unpin, a pinned create, a close): the
    /// keeper snapshots soon instead of at its next tick.
    pub fn changed(&self) {
        self.notify.notify_one();
    }

    fn freeze(&self) {
        self.book().freeze();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// Cached records or the handle (restore markers, `--force` stops).
    No,
    /// Records older than [`RECORD_REFRESH`] or missing.
    Stale,
    /// Every record (the final snapshot of a graceful stop).
    All,
}

/// Ask a session for its record (non-fencing, unlike `Checkpoint{Reload}`).
async fn query_record(h: SessionHandle) -> Option<Record> {
    let mut rx = h.subscribe();
    h.send(SessionCommand::Query { id: PIN_RECORD_QUERY_ID, query: SessionQuery::Record }).await.ok()?;
    let wait = async {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let SessionEventWire::QueryResult { id, value } = env.event {
                        if id == PIN_RECORD_QUERY_ID {
                            let rec: SessionReloadRecord = serde_json::from_value(value["record"].clone()).ok()?;
                            let workers: Vec<PinnedWorker> =
                                serde_json::from_value(value["workers_running"].clone()).unwrap_or_default();
                            return Some((rec, workers));
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    };
    tokio::time::timeout(RECORD_QUERY_BUDGET, wait).await.ok().flatten()
}

/// Compose and write. Not after shutdown began, except the final snapshot:
/// once sessions start ending, a snapshot would drop them.
async fn snapshot(state: &Arc<DaemonState>, refresh: Refresh, final_snapshot: bool) -> Snap {
    if !final_snapshot && state.shutdown.is_cancelled() {
        return Snap::Refused;
    }
    let dir = sessions_dir();
    let now = Instant::now();
    let live = state.live_sessions();
    let wanted: Vec<SessionHandle> = {
        let book = state.pins.book();
        let wanted: Vec<SessionHandle> = live
            .into_iter()
            .filter(|h| h.placeholder_config().is_none() && h.pinned() && book.needs_record(h, refresh, now))
            .collect();
        wanted
    };
    if !wanted.is_empty() {
        let got = futures::future::join_all(wanted.iter().cloned().map(query_record)).await;
        let mut book = state.pins.book();
        for (h, rec) in wanted.iter().zip(got) {
            if let Some(rec) = rec {
                book.records.insert(h.id.as_str().to_string(), (entry_from(h, Some(rec), &dir), Instant::now()));
            }
        }
    }
    // Book, then flock: the file is written under both.
    let mut book = state.pins.book();
    let file = book.compose(&state.live_sessions(), &dir, Instant::now(), final_snapshot);
    let lock = state.lock_fd.lock().unwrap_or_else(|e| e.into_inner());
    let result = book.persist(&file, lock.is_some(), final_snapshot);
    drop(lock);
    match result {
        Ok(snap) => snap,
        Err(e) => {
            tracing::warn!(path = %book.path().display(), error = %e, "daemon: pins file not written");
            Snap::Failed
        }
    }
}

/// The only writer of the pins file.
async fn keeper(state: Arc<DaemonState>, final_token: CancellationToken) {
    let writes = state.pins.write_rx.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(mut writes) = writes else {
        return;
    };
    let mut tick = tokio::time::interval(KEEPER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut retry = tokio::time::interval(PLACEHOLDER_RETRY);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    retry.tick().await; // the first tick is immediate; restore just probed
    loop {
        tokio::select! {
            biased;
            _ = final_token.cancelled() => break,
            Some(ack) = writes.recv() => {
                let snap = snapshot(&state, Refresh::No, false).await;
                let _ = ack.send(matches!(snap, Snap::Written | Snap::Unchanged));
            }
            _ = state.pins.notify.notified() => {
                tokio::time::sleep(NOTIFY_DEBOUNCE).await;
                snapshot(&state, Refresh::Stale, false).await;
            }
            _ = tick.tick() => {
                snapshot(&state, Refresh::Stale, false).await;
            }
            _ = retry.tick() => {
                // Not while the restore runs: it waits on this task for its
                // markers. A shutdown meanwhile waits for the retry (or the
                // final budget aborts it; the file still has the entry).
                let busy = state.shutdown.is_cancelled()
                    || state.reloading.load(Ordering::SeqCst)
                    || state.restoring.load(Ordering::SeqCst);
                if !busy {
                    retry_placeholders(&state).await;
                }
            }
        }
    }
    // Every pinned session is still live here (`Daemon::wait` ends them
    // after this returns). `--force` stops get no queries.
    let refresh = if state.force_shutdown.load(Ordering::SeqCst) { Refresh::No } else { Refresh::All };
    let snap = snapshot(&state, refresh, true).await;
    tracing::debug!(?snap, "daemon: pins final snapshot");
    state.pins.freeze();
    writes.close();
    while let Ok(ack) = writes.try_recv() {
        let _ = ack.send(false);
    }
}

/// Ask the keeper to write now and wait for it (the restore marker).
async fn request_write(state: &DaemonState) -> bool {
    let (tx, rx) = oneshot::channel();
    if state.pins.write_tx.send(tx).is_err() {
        return false;
    }
    matches!(tokio::time::timeout(WRITE_ACK_BUDGET, rx).await, Ok(Ok(true)))
}

/// Clears `DaemonState::restoring` however the restore task ends (an abort
/// at shutdown drops it mid-await).
struct RestoringGuard(Arc<DaemonState>);

impl Drop for RestoringGuard {
    fn drop(&mut self) {
        self.0.restoring.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Restored,
    Held,
    Failed,
    Dropped,
    Aborted,
}

async fn restore_all(state: Arc<DaemonState>) {
    let _guard = RestoringGuard(Arc::clone(&state));
    let (entries, path) = {
        let book = state.pins.book();
        (book.pending(), book.path().to_path_buf())
    };
    tracing::info!(n = entries.len(), path = %path.display(), "daemon: restoring pinned session(s)");
    let (mut ok, mut held, mut failed, mut dropped) = (0, 0, 0, 0);
    for entry in entries {
        if state.shutdown.is_cancelled() {
            break;
        }
        match restore_one(&state, entry).await {
            Outcome::Restored => ok += 1,
            Outcome::Held => held += 1,
            Outcome::Failed => failed += 1,
            Outcome::Dropped => dropped += 1,
            Outcome::Aborted => break,
        }
    }
    tracing::info!(ok, held, failed, dropped, "daemon: restore done");
}

async fn drop_entry(state: &DaemonState, entry: &PinEntry, reason: String) -> Outcome {
    tracing::warn!(session = %entry.id, reason = %reason, "daemon: pin dropped");
    state.pins.book().drop_pin(entry, reason);
    request_write(state).await;
    Outcome::Dropped
}

async fn fail_entry(state: &DaemonState, mut entry: PinEntry, error: String) -> Outcome {
    tracing::warn!(
        session = %entry.id,
        attempt = entry.restore.attempts,
        max = MAX_RESTORE_ATTEMPTS,
        error = %error,
        "daemon: restore failed"
    );
    if entry.restore.attempts >= MAX_RESTORE_ATTEMPTS {
        let reason = format!("restore failed {} times: {error}", entry.restore.attempts);
        return drop_entry(state, &entry, reason).await;
    }
    entry.restore.last_error = Some(error);
    state.pins.book().set(&entry, Carry::Failed);
    request_write(state).await;
    Outcome::Failed
}

/// F23 (as reload's rehydrate): the journal is live in another process. A
/// placeholder stands in under the same id, pinned, so the first attach
/// after the holder lets go recreates it pinned. No attempt is counted.
fn register_placeholder(state: &DaemonState, entry: &PinEntry, holder: String) -> Outcome {
    tracing::warn!(session = %entry.id, holder = %holder, "daemon: pinned session's journal is held by another process; registering a placeholder");
    let cfg = SessionConfig {
        continue_session: Some(Some(entry.journal_id.clone())),
        keep_warm: true,
        ..entry.config.clone()
    };
    let (handle, _task) =
        crate::session::handle::locked_placeholder::spawn(SessionId::from(entry.id.as_str()), cfg, holder);
    state.insert(handle);
    state.pins.book().set(entry, Carry::Placeholder);
    state.pins.changed();
    Outcome::Held
}

async fn restore_one(state: &Arc<DaemonState>, mut entry: PinEntry) -> Outcome {
    // A client brought it back while restore ran: only the pin is missing.
    let live = state.attach(&SessionId::from(entry.id.as_str())).or_else(|| {
        (entry.journal_id != entry.id)
            .then(|| state.attach(&SessionId::from(entry.journal_id.as_str())))
            .flatten()
    });
    if let Some(h) = live.filter(|h| h.placeholder_config().is_none()) {
        let _ = h.send(SessionCommand::KeepWarm { on: true }).await;
        tracing::info!(session = %h.id, "daemon: pinned session already live; pinned it");
        state.pins.changed();
        return Outcome::Restored;
    }
    if entry.restore.attempts >= MAX_RESTORE_ATTEMPTS {
        let why = entry.restore.last_error.clone().unwrap_or_else(|| "the daemon died while restoring it".into());
        return drop_entry(state, &entry, format!("restore failed {} times: {why}", entry.restore.attempts)).await;
    }
    if let Some(cwd) = entry.config.cwd.as_ref().filter(|c| !c.is_dir()) {
        let reason = format!("cwd {} no longer exists", cwd.display());
        return drop_entry(state, &entry, reason).await;
    }

    let dir = sessions_dir();
    let journal = journal_exists(&dir, &entry.journal_id);
    if journal {
        // `create` publishes the session endpoint before it takes the
        // journal lock: probe first so a live holder is never disturbed.
        let mut tries = 0;
        while let Some(desc) = lock_holder(&dir, &entry.journal_id) {
            tries += 1;
            if tries >= HELD_REPROBE_TRIES || state.shutdown.is_cancelled() {
                return register_placeholder(state, &entry, desc);
            }
            tokio::time::sleep(HELD_REPROBE).await;
        }
    }

    // The marker: on disk BEFORE `create`, so an abort inside it (release
    // builds are panic = abort) still counts.
    entry.restore.attempts += 1;
    entry.restore.last_attempt_at = Some(Utc::now());
    state.pins.book().set(&entry, Carry::InFlight);
    if !request_write(state).await {
        entry.restore.attempts -= 1;
        state.pins.book().set(&entry, Carry::Pending);
        if state.shutdown.is_cancelled() {
            return Outcome::Aborted;
        }
        tracing::warn!(session = %entry.id, "daemon: pins file not writable; not restoring without a crash-loop marker");
        return Outcome::Failed;
    }
    if !journal && entry.has_journal {
        return fail_entry(state, entry.clone(), format!("journal {} is missing", entry.journal_id)).await;
    }

    let cfg = SessionConfig {
        // Never journaled (no turn yet): a fresh session, as rehydrate does.
        continue_session: if journal { Some(Some(entry.journal_id.clone())) } else { None },
        keep_warm: true,
        ..entry.config.clone()
    };
    match tokio::time::timeout(RESTORE_CREATE_TIMEOUT, state.create(cfg)).await {
        Ok(Ok(h)) => {
            super::reload::apply_record(&h, &entry.settings_replay, true, entry.cache_keepwarm.as_deref()).await;
            let lost: Vec<(String, String)> =
                entry.workers_running.iter().map(|w| (w.id.clone(), w.agent.clone())).collect();
            let _ = h.send(SessionCommand::Restored { lost_workers: lost }).await;
            if h.id.as_str() != entry.id {
                state
                    .reload_aliases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(entry.id.clone(), h.id.as_str().to_string());
            }
            state.pins.book().mark_restored(&entry, &h, Instant::now());
            state.pins.changed();
            tracing::info!(old = %entry.id, session = %h.id, "daemon: restored (pinned)");
            Outcome::Restored
        }
        Ok(Err(e)) if super::reload::is_lock_held_error(&e) => {
            entry.restore.attempts -= 1;
            register_placeholder(state, &entry, super::reload::parse_holder_from_error(&e))
        }
        Ok(Err(e)) => fail_entry(state, entry, e).await,
        Err(_) => {
            let secs = RESTORE_CREATE_TIMEOUT.as_secs();
            fail_entry(state, entry, format!("create did not finish within {secs} s")).await
        }
    }
}

/// Who holds `journal_id`'s lock, if anyone. A compacted journal or an I/O
/// error reads as free: `create` follows the chain or reports the error.
fn lock_holder(dir: &Path, journal_id: &str) -> Option<String> {
    let holder = agent_core::session_lock::LockHolder { pid: std::process::id(), kind: "daemon".into() };
    match agent_core::session_lock::SessionLock::try_acquire(dir, journal_id, holder) {
        Ok(lock) => {
            drop(lock);
            None
        }
        Err(agent_core::session_lock::SessionLockError::Held { holder, .. }) => {
            Some(holder.map_or_else(|| "unknown holder".to_string(), |h| format!("pid {}, {}", h.pid, h.kind)))
        }
        Err(_) => None,
    }
}

/// A pinned session whose journal another process held at restore stands
/// in as a placeholder. A pinned session often has no client to attach and
/// bring it back, so the keeper retries the lock itself and, once it is
/// free, recreates the session pinned with its record. No attempt is
/// counted for a held lock; a failed recreate keeps the placeholder.
async fn retry_placeholders(state: &Arc<DaemonState>) {
    let entries = state.pins.book().placeholders();
    if entries.is_empty() {
        return;
    }
    let dir = sessions_dir();
    for entry in entries {
        if state.shutdown.is_cancelled() {
            break;
        }
        let Some(placeholder) =
            state.attach(&SessionId::from(entry.id.as_str())).filter(|h| h.placeholder_config().is_some())
        else {
            continue;
        };
        if lock_holder(&dir, &entry.journal_id).is_some() {
            continue;
        }
        let h = state.retry_locked_placeholder(placeholder).await;
        if h.placeholder_config().is_some() {
            continue; // still locked after all: next round
        }
        super::reload::apply_record(&h, &entry.settings_replay, true, entry.cache_keepwarm.as_deref()).await;
        let lost: Vec<(String, String)> =
            entry.workers_running.iter().map(|w| (w.id.clone(), w.agent.clone())).collect();
        let _ = h.send(SessionCommand::Restored { lost_workers: lost }).await;
        if h.id.as_str() != entry.id {
            state
                .reload_aliases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(entry.id.clone(), h.id.as_str().to_string());
        }
        state.pins.book().mark_restored(&entry, &h, Instant::now());
        state.pins.changed();
        tracing::info!(old = %entry.id, session = %h.id, "daemon: pinned session's journal was released; restored (pinned)");
    }
}

/// The keeper and the restore task of one daemon image.
pub struct PinTasks {
    keeper: tokio::task::JoinHandle<()>,
    restore: Option<tokio::task::JoinHandle<()>>,
    final_token: CancellationToken,
}

/// Start the keeper and, on a fresh start with restore enabled, the
/// restore. Called after `signal_ready`.
pub fn start(state: &Arc<DaemonState>, restore: bool) -> PinTasks {
    let final_token = CancellationToken::new();
    let keeper = tokio::spawn(keeper(Arc::clone(state), final_token.clone()));
    let pending = state.pins.book().pending().len();
    let restore = if pending == 0 {
        None
    } else if restore {
        // Before the task runs, so the idle monitor never sees a gap.
        state.restoring.store(true, Ordering::SeqCst);
        Some(tokio::spawn(restore_all(Arc::clone(state))))
    } else {
        tracing::info!(n = pending, "daemon: pinned sessions not restored this run (restore disabled); kept in the pins file");
        None
    };
    PinTasks { keeper, restore, final_token }
}

impl PinTasks {
    /// Shutdown, before any session is ended: let the restore stop between
    /// entries (bounded; an aborted one is not counted), then the keeper's
    /// final snapshot, then freeze.
    pub async fn finish(self, state: &DaemonState) {
        if let Some(mut restore) = self.restore {
            if tokio::time::timeout(RESTORE_SHUTDOWN_WAIT, &mut restore).await.is_err() {
                tracing::warn!(wait = ?RESTORE_SHUTDOWN_WAIT, "daemon: restore still running at shutdown; aborted");
                restore.abort();
                let _ = restore.await;
                state.pins.book().rollback_in_flight();
            }
        }
        self.final_token.cancel();
        let mut keeper = self.keeper;
        if tokio::time::timeout(KEEPER_FINAL_BUDGET, &mut keeper).await.is_err() {
            tracing::warn!("daemon: pins final snapshot did not finish; the file keeps the last one");
            keeper.abort();
            let _ = keeper.await;
        }
        state.pins.freeze();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::handle::echo;
    use crate::session::handle::SessionEndpoints;

    fn entry(id: &str, cwd: &Path) -> PinEntry {
        PinEntry {
            id: id.into(),
            journal_id: id.into(),
            has_journal: true,
            config: pinned_config(
                SessionConfig { cwd: Some(cwd.to_path_buf()), ..SessionConfig::default() },
                id,
                Some("claude-sonnet-4-5".into()),
            ),
            settings_replay: vec![SessionSetting::ApiRetries { n: 7 }],
            cache_keepwarm: None,
            workers_running: Vec::new(),
            restore: RestoreState::default(),
        }
    }

    fn file_with(pins: Vec<PinEntry>) -> PinFile {
        PinFile { version: PINS_FILE_VERSION, pins, ..PinFile::default() }
    }

    /// A live handle whose pin cell is `pinned`; alive while the endpoints are.
    fn live(id: &str, pinned: bool) -> (SessionHandle, SessionEndpoints) {
        let mut meta = echo::meta_for(&SessionId::from(id));
        meta.pinned = pinned;
        SessionHandle::new(meta, echo::view_for())
    }

    fn ids(file: &PinFile) -> Vec<&str> {
        file.pins.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn a_pins_file_round_trips_and_an_older_file_without_new_fields_parses() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(vec![entry("s1", dir.path())]);
        f.dropped.push(DroppedPin { id: "s0".into(), journal_id: "s0".into(), reason: "gone".into(), at: Utc::now() });
        let back: PinFile = serde_json::from_slice(&serde_json::to_vec(&f).unwrap()).unwrap();
        assert_eq!(ids(&back), ["s1"]);
        assert!(back.pins[0].config.keep_warm);
        assert!(back.pins[0].config.env.is_none(), "env is never persisted");
        assert_eq!(back.pins[0].config.continue_session, Some(Some("s1".into())));
        assert_eq!(back.dropped.len(), 1);

        let old: PinFile = serde_json::from_value(serde_json::json!({
            "pins": [{ "id": "s2", "journal_id": "s2", "config": {} }]
        }))
        .unwrap();
        assert_eq!(old.version, 0);
        assert_eq!(old.pins[0].restore.attempts, 0);
        assert!(!old.pins[0].has_journal);
    }

    #[test]
    fn an_unparsable_pins_file_is_set_aside_as_bad_and_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINS_FILE_NAME);
        assert!(load(&path).is_none(), "missing");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_none());
        assert!(!path.exists());
        assert!(dir.path().join(format!("{PINS_FILE_NAME}.bad")).exists());
    }

    #[test]
    fn another_profiles_file_is_neither_restored_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINS_FILE_NAME);
        let mut f = file_with(vec![entry("s1", dir.path())]);
        f.profile = Some("work".into());
        let mut book = PinBook::new(path.clone(), None, Some(f));
        assert!(book.pending().is_empty(), "nothing to restore");
        let out = book.compose(&[], dir.path(), Instant::now(), false);
        assert_eq!(book.persist(&out, true, false).unwrap(), Snap::Refused);
        assert!(!path.exists());
        // `None` and `Some("")` are both the default profile.
        assert!(same_profile(Some(""), None));
    }

    #[test]
    fn nothing_is_written_without_the_daemon_flock_and_no_file_is_created_for_an_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINS_FILE_NAME);
        let mut book = PinBook::new(path.clone(), None, None);
        let empty = book.compose(&[], dir.path(), Instant::now(), false);
        assert_eq!(book.persist(&empty, true, false).unwrap(), Snap::Unchanged);
        assert!(!path.exists(), "an empty set creates no file");

        let (h, _ep) = live("s1", true);
        let out = book.compose(std::slice::from_ref(&h), dir.path(), Instant::now(), false);
        assert_eq!(ids(&out), ["s1"]);
        assert_eq!(book.persist(&out, false, false).unwrap(), Snap::Refused, "no flock, no write");
        assert!(!path.exists());
        assert_eq!(book.persist(&out, true, false).unwrap(), Snap::Written);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(book.persist(&out, true, false).unwrap(), Snap::Unchanged);
    }

    #[test]
    fn a_frozen_book_never_writes_so_a_snapshot_after_shutdown_cannot_empty_the_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINS_FILE_NAME);
        let (h, _ep) = live("s1", true);
        let mut book = PinBook::new(path.clone(), None, None);
        let out = book.compose(std::slice::from_ref(&h), dir.path(), Instant::now(), true);
        assert_eq!(ids(&out), ["s1"]);
        assert_eq!(book.persist(&out, true, true).unwrap(), Snap::Written);
        book.freeze();
        // After `shutdown_all` the session is gone from the map.
        let after = book.compose(&[], dir.path(), Instant::now(), true);
        assert!(after.pins.is_empty());
        assert_eq!(book.persist(&after, true, true).unwrap(), Snap::Refused);
        let on_disk = load(&path).unwrap();
        assert_eq!(ids(&on_disk), ["s1"], "the final snapshot stands");
        assert!(on_disk.clean_shutdown);
    }

    #[test]
    fn the_set_is_the_live_pinned_sessions_plus_carried_entries_a_live_session_does_not_speak_for() {
        let dir = tempfile::tempdir().unwrap();
        let carried = vec![entry("old-a", dir.path()), entry("old-b", dir.path())];
        let mut book = PinBook::new(dir.path().join(PINS_FILE_NAME), None, Some(file_with(carried)));
        let (pinned, _e1) = live("new-p", true);
        let (unpinned, _e2) = live("new-u", false);
        // `old-b` is live again, unpinned: the live session decides.
        let (old_b, _e3) = live("old-b", false);
        let out = book.compose(&[pinned, unpinned, old_b], dir.path(), Instant::now(), false);
        let mut got = ids(&out);
        got.sort_unstable();
        assert_eq!(got, ["new-p", "old-a"]);
        assert!(out.pins.iter().all(|p| p.config.keep_warm && p.config.env.is_none()));
        // The carried entry is gone for good, not just hidden this once.
        let again = book.compose(&[], dir.path(), Instant::now(), false);
        assert_eq!(ids(&again), ["old-a"]);
    }

    #[tokio::test]
    async fn a_placeholder_entry_lives_as_long_as_its_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry("held", dir.path());
        let mut book = PinBook::new(dir.path().join(PINS_FILE_NAME), None, Some(file_with(vec![e.clone()])));
        let (ph, task) = crate::session::handle::locked_placeholder::spawn(
            SessionId::from("held"),
            e.config.clone(),
            "pid 1, tui".into(),
        );
        book.set(&e, Carry::Placeholder);
        let out = book.compose(std::slice::from_ref(&ph), dir.path(), Instant::now(), false);
        assert_eq!(ids(&out), ["held"]);
        assert_eq!(out.pins[0].settings_replay.len(), 1, "the carried entry, settings and all");
        assert_eq!(out.pins[0].restore.attempts, 0);

        // `daemon close` on the placeholder: it leaves the map.
        ph.send(SessionCommand::End { reason: crate::session::EndReason::ClientQuit }).await.unwrap();
        let _ = task.await;
        let out = book.compose(&[], dir.path(), Instant::now(), false);
        assert!(out.pins.is_empty(), "{:?}", ids(&out));
    }

    #[test]
    fn a_restored_session_keeps_its_attempt_count_until_it_has_been_up_for_a_minute() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = entry("s1", dir.path());
        e.restore.attempts = 2;
        let mut book = PinBook::new(dir.path().join(PINS_FILE_NAME), None, Some(file_with(vec![e.clone()])));
        let (h, _ep) = live("s1", true);
        let t0 = Instant::now();
        book.set(&e, Carry::InFlight);
        let out = book.compose(std::slice::from_ref(&h), dir.path(), t0, false);
        assert_eq!(out.pins.len(), 1, "live and carried are one entry");
        assert_eq!(out.pins[0].restore.attempts, 2, "in flight: the marker stands");

        book.mark_restored(&e, &h, t0);
        let out = book.compose(std::slice::from_ref(&h), dir.path(), t0 + Duration::from_secs(30), false);
        assert_eq!(out.pins[0].restore.attempts, 2, "not stable yet");
        assert_eq!(out.pins[0].settings_replay.len(), 1, "settings from the restored entry");
        let out = book.compose(std::slice::from_ref(&h), dir.path(), t0 + STABLE_AFTER, false);
        assert_eq!(out.pins.len(), 1);
        assert_eq!(out.pins[0].restore.attempts, 0, "stable: reset");
    }

    #[test]
    fn an_aborted_restore_does_not_count_its_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = entry("s1", dir.path());
        let mut book = PinBook::new(dir.path().join(PINS_FILE_NAME), None, Some(file_with(vec![e.clone()])));
        e.restore.attempts = 1;
        book.set(&e, Carry::InFlight);
        book.rollback_in_flight();
        assert_eq!(book.pending()[0].restore.attempts, 0);
    }

    #[test]
    fn a_dropped_pin_is_listed_until_that_journal_is_pinned_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PINS_FILE_NAME);
        let e = entry("s1", dir.path());
        let mut book = PinBook::new(path.clone(), None, Some(file_with(vec![e.clone()])));
        book.drop_pin(&e, "cwd /nope no longer exists".into());
        let out = book.compose(&[], dir.path(), Instant::now(), false);
        assert!(out.pins.is_empty());
        assert_eq!(book.persist(&out, true, false).unwrap(), Snap::Written);
        let lines = dropped_lines(&path);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("pin dropped: s1 (cwd /nope no longer exists, "), "{lines:?}");

        let (h, _ep) = live("s1", true);
        let out = book.compose(std::slice::from_ref(&h), dir.path(), Instant::now(), false);
        assert!(out.dropped.is_empty(), "pinned again");
    }
}
