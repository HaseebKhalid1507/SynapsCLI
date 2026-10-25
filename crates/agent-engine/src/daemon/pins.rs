//! Pinned sessions come back after the daemon restarts.
//!
//! The **pin book** (`<config dir>/daemon-pins.json`) is the set of pinned
//! journals for this profile. It is written through, never by a timer: the
//! actor calls [`PinBook`] (as the host's [`PinSink`]) when the pin changes,
//! when a pinned session moves to a new journal, and when a pinned session
//! is ended on purpose. A daemon stop, reload or crash leaves it alone, and
//! the next cold start continues every entry that is not live and pins it
//! ([`restore_all`]). `SYNAPS_DAEMON_RESTORE_PINS=0` starts without that.
//!
//! The file lives in the config dir, not the run dir: older binaries unlink
//! run-dir json they cannot parse, and the run dir may be tmpfs. Entries are
//! parsed one at a time, so one bad (or newer) entry never loses the rest;
//! an entry this build can't read is written back untouched, and a file
//! from a newer schema version is read-only here.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use super::DaemonState;
use crate::session::{CompactionPolicyWire, PinSink, SessionCommand, SessionConfig, SessionHandle};

pub const PINS_FILE: &str = "daemon-pins.json";
pub const FILE_VERSION: u64 = 1;
/// `0`/`false`/`off`/`no`: start the daemon without restoring pins.
pub const RESTORE_ENV: &str = "SYNAPS_DAEMON_RESTORE_PINS";
/// Restore attempts the daemon did not live through before an entry is
/// given up as a crash loop (marked failed, not retried).
pub const MAX_ATTEMPTS: u32 = 2;
/// A journal held by an exiting process gets this long, once.
pub const HELD_RECHECK: Duration = Duration::from_secs(2);
/// One restore's `create` budget (MCP servers, extensions).
pub const RESTORE_CREATE_TIMEOUT: Duration = Duration::from_secs(60);

pub const STATE_WAITING: &str = "waiting";
pub const STATE_FAILED: &str = "failed";

/// One pinned journal. Never carries `SessionConfig` or settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PinEntry {
    pub journal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub pinned_at: Option<DateTime<Utc>>,
    /// `waiting` / `failed` after a restore that did not bring it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The session's `/keepwarm` override (`off`/`auto`/`<N>h`), restored
    /// with the pin; one this build can't parse falls back to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_keepwarm: Option<String>,
    /// Crash-loop marker: restores begun and not finished.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    /// Fields from a newer build, kept as they were.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl PinEntry {
    /// `waiting: held by pid 42, tui` / `failed: journal … is missing` /
    /// `not restored yet`.
    pub fn state_text(&self) -> String {
        match (self.state.as_deref(), self.reason.as_deref()) {
            (Some(s), Some(r)) => format!("{s}: {r}"),
            (Some(s), None) => s.to_string(),
            (None, _) => "not restored yet".to_string(),
        }
    }

    fn set_state(&mut self, state: &str, reason: String) {
        self.state = Some(state.to_string());
        self.reason = Some(reason);
    }

    fn clear_state(&mut self) {
        self.state = None;
        self.reason = None;
        self.attempts = 0;
    }
}

/// The pin book of one config dir. Every change is a locked
/// read-modify-write (`<file>.lock`), so the daemon and a CLI clearing an
/// entry never lose each other's writes.
#[derive(Debug, Clone)]
pub struct PinBook {
    path: PathBuf,
}

struct Raw {
    version: u64,
    entries: Vec<PinEntry>,
    /// Entries this build can't parse, written back as they were.
    unknown: Vec<serde_json::Value>,
}

impl PinBook {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The active profile's book.
    pub fn for_active_profile() -> Self {
        Self::at(crate::config::get_active_config_dir().join(PINS_FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry this build can read (missing or unreadable file: none).
    pub fn load(&self) -> Vec<PinEntry> {
        self.read().entries
    }

    fn read(&self) -> Raw {
        let mut raw = Raw { version: FILE_VERSION, entries: Vec::new(), unknown: Vec::new() };
        let Ok(text) = std::fs::read_to_string(&self.path) else { return raw };
        let Ok(serde_json::Value::Object(top)) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!(path = %self.path.display(), "pins: unreadable pin book; leaving it alone");
            raw.version = u64::MAX; // read-only: never overwrite what we can't read
            return raw;
        };
        raw.version = top.get("version").and_then(|v| v.as_u64()).unwrap_or(FILE_VERSION);
        let list = top.get("pins").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for v in list {
            match serde_json::from_value::<PinEntry>(v.clone()) {
                Ok(e) if !e.journal_id.is_empty() => raw.entries.push(e),
                _ => raw.unknown.push(v),
            }
        }
        raw
    }

    /// Locked read-modify-write. Writes only when `f` changed something;
    /// an empty book never creates the file. `Err` when the file is from a
    /// newer schema (or unreadable) and so read-only here.
    pub fn update<R>(&self, f: impl FnOnce(&mut Vec<PinEntry>) -> R) -> io::Result<R> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut lock_name = self.path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_file_name(lock_name))?;
        FileExt::lock_exclusive(&lock)?;
        let mut raw = self.read();
        let before = raw.entries.clone();
        let out = f(&mut raw.entries);
        if raw.entries == before {
            return Ok(out);
        }
        if raw.version > FILE_VERSION {
            return Err(io::Error::other(format!(
                "{} is from a newer synaps (or unreadable); not changing it",
                self.path.display()
            )));
        }
        let mut pins: Vec<serde_json::Value> =
            raw.entries.iter().filter_map(|e| serde_json::to_value(e).ok()).collect();
        pins.extend(raw.unknown);
        let body = serde_json::to_vec_pretty(&serde_json::json!({ "version": FILE_VERSION, "pins": pins }))
            .map_err(io::Error::other)?;
        super::registry::write_private_atomic(&self.path, &body)?;
        Ok(out)
    }

    fn update_logged(&self, what: &str, f: impl FnOnce(&mut Vec<PinEntry>)) {
        if let Err(e) = self.update(f) {
            tracing::warn!(path = %self.path.display(), error = %e, "pins: {what} not written");
        }
    }

    /// Drop the entry for `id` (journal id, exact). `true` if there was one.
    pub fn remove(&self, id: &str) -> io::Result<bool> {
        self.update(|v| {
            let n = v.len();
            v.retain(|e| e.journal_id != id);
            v.len() != n
        })
    }

    fn set(&self, journal_id: &str, f: impl FnOnce(&mut PinEntry)) {
        self.update_logged("state", |v| {
            if let Some(e) = v.iter_mut().find(|e| e.journal_id == journal_id) {
                f(e);
            }
        });
    }
}

impl PinSink for PinBook {
    fn pinned(&self, id: &str, journal_id: &str, name: Option<&str>, cwd: Option<&Path>, cache_keepwarm: Option<&str>) {
        self.update_logged("pin", |v| {
            let at = v.iter().position(|e| e.journal_id == journal_id).or_else(|| v.iter().position(|e| e.journal_id == id));
            let e = match at {
                Some(i) => &mut v[i],
                None => {
                    v.push(PinEntry { pinned_at: Some(Utc::now()), ..Default::default() });
                    v.last_mut().expect("just pushed")
                }
            };
            e.journal_id = journal_id.to_string();
            e.name = name.map(str::to_owned).or(e.name.take());
            if let Some(cwd) = cwd {
                e.cwd = Some(cwd.to_path_buf());
            }
            if let Some(kw) = cache_keepwarm {
                e.cache_keepwarm = Some(kw.to_string());
            }
            e.clear_state();
        });
    }

    fn unpinned(&self, id: &str, journal_id: &str) {
        self.update_logged("unpin", |v| v.retain(|e| e.journal_id != journal_id && e.journal_id != id));
    }

    fn moved(&self, from: &str, to: &str) {
        self.update_logged("move", |v| {
            if let Some(e) = v.iter_mut().find(|e| e.journal_id == from) {
                e.journal_id = to.to_string();
            }
        });
    }
}

/// `SYNAPS_DAEMON_RESTORE_PINS` is not off.
pub fn restore_enabled() -> bool {
    !super::env_is_off(std::env::var(RESTORE_ENV).ok().as_deref())
}

/// The live session for a journal, if this daemon holds it (a lock
/// placeholder does not count).
pub fn live_for(state: &DaemonState, journal_id: &str) -> Option<SessionHandle> {
    state
        .live_sessions()
        .into_iter()
        .filter(|h| h.is_alive() && h.meta().locked_by.is_none())
        .find(|h| h.id.as_str() == journal_id || h.journal_id() == journal_id)
}

/// Entries that are not live in this daemon: what listings show as
/// waiting / failed / not restored yet.
pub fn offline(entries: Vec<PinEntry>, live: &[crate::session::SessionMeta]) -> Vec<PinEntry> {
    entries
        .into_iter()
        .filter(|e| !live.iter().any(|m| m.id.as_str() == e.journal_id || m.journal_id == e.journal_id))
        .collect()
}

/// Who holds `journal_id`'s lock, if anyone (probe: take and drop it).
/// `create` publishes the session endpoint before it locks, so a held
/// journal must be found before `create` runs.
fn lock_holder(dir: &Path, journal_id: &str) -> Option<String> {
    use agent_core::session_lock::{LockHolder, SessionLock, SessionLockError};
    let me = LockHolder { pid: std::process::id(), kind: "daemon".into() };
    match SessionLock::try_acquire(dir, journal_id, me) {
        Ok(lock) => {
            drop(lock);
            None
        }
        Err(SessionLockError::Held { holder, .. }) => {
            Some(holder.map_or_else(|| "another process".to_string(), |h| format!("pid {}, {}", h.pid, h.kind)))
        }
        // Compacted or I/O: `create` follows the chain or reports it.
        Err(_) => None,
    }
}

/// Outcome of one restore, for logs and `daemon pin`.
#[derive(Debug, PartialEq, Eq)]
pub enum Restored {
    Live,
    Waiting(String),
    Failed(String),
}

/// Continue one entry into this daemon, pinned (`synaps --attach
/// --continue <id>`'s create). Never steals a held journal; the attempt
/// marker is on disk before `create`, so a daemon that dies inside it does
/// not try again forever.
pub async fn restore_one(state: &Arc<DaemonState>, entry: &PinEntry) -> Restored {
    let id = entry.journal_id.as_str();
    if let Some(h) = live_for(state, id) {
        if !h.pinned() {
            let _ = h.send(SessionCommand::KeepWarm { on: true }).await;
        }
        return Restored::Live;
    }
    let fail = |why: String| {
        state.pins.set(id, |e| {
            e.set_state(STATE_FAILED, why.clone());
            e.attempts = 0;
        });
        Restored::Failed(why)
    };
    if entry.attempts >= MAX_ATTEMPTS {
        let why = format!(
            "the daemon died {} times while restoring it; not retried (`synaps daemon pin {id}` tries again, --off clears it)",
            entry.attempts
        );
        state.pins.set(id, |e| e.set_state(STATE_FAILED, why.clone()));
        return Restored::Failed(why);
    }
    let dir = agent_core::session_lock::sessions_dir();
    if !dir.join(format!("{id}.json")).is_file() {
        return fail(format!("journal {id} is missing (it never ran a turn, or was deleted)"));
    }
    let mut holder = lock_holder(&dir, id);
    if holder.is_some() {
        tokio::time::sleep(HELD_RECHECK).await;
        holder = lock_holder(&dir, id);
    }
    if let Some(who) = holder {
        let why = format!("held by {who}");
        state.pins.set(id, |e| e.set_state(STATE_WAITING, why.clone()));
        return Restored::Waiting(why);
    }
    state.pins.set(id, |e| e.attempts += 1);
    let cfg = SessionConfig {
        continue_session: Some(Some(id.to_string())),
        cwd: entry.cwd.clone().filter(|c| c.is_dir()),
        compaction_policy: CompactionPolicyWire::LinkedSuccessor,
        await_extensions: false,
        keep_warm: true,
        ..Default::default()
    };
    match tokio::time::timeout(RESTORE_CREATE_TIMEOUT, state.create(cfg)).await {
        // The actor's create wrote the entry back (pinned: state cleared).
        Ok(Ok(h)) => {
            // As reload does. No ping follows: with no request since the
            // restore there is no cache anchor, so a period waits for the
            // first real turn.
            let kw = entry.cache_keepwarm.as_deref().filter(|m| crate::core::config::CacheKeepWarm::parse(m).is_some());
            if let Some(mode) = kw {
                let _ = h
                    .send(SessionCommand::EngineCommand { id: u64::MAX, name: "keepwarm".into(), arg: mode.to_string() })
                    .await;
            }
            tracing::info!(journal = id, session = %h.id, "daemon: pinned session restored");
            Restored::Live
        }
        Ok(Err(e)) if super::reload::is_lock_held_error(&e) => {
            let why = format!("held by {}", super::reload::parse_holder_from_error(&e));
            state.pins.set(id, |e| {
                e.set_state(STATE_WAITING, why.clone());
                e.attempts = 0;
            });
            Restored::Waiting(why)
        }
        Ok(Err(e)) => fail(e),
        Err(_) => fail(format!("create did not finish within {} s", RESTORE_CREATE_TIMEOUT.as_secs())),
    }
}

/// Cold start: restore every entry not already live, one at a time (so the
/// crash marker names the one entry in flight), after the daemon is ready.
/// Holds the idle exit while it runs.
pub async fn restore_all(state: Arc<DaemonState>) {
    let entries = state.pins.load();
    if entries.is_empty() {
        return;
    }
    state.restoring.store(true, Ordering::SeqCst);
    tracing::info!(pins = entries.len(), "daemon: restoring pinned sessions");
    for entry in entries {
        if state.shutdown.is_cancelled() {
            break;
        }
        match restore_one(&state, &entry).await {
            Restored::Live => {}
            Restored::Waiting(why) | Restored::Failed(why) => {
                tracing::warn!(journal = %entry.journal_id, reason = %why, "daemon: pinned session not restored");
            }
        }
    }
    state.restoring.store(false, Ordering::SeqCst);
}

/// `daemon pin` for a journal that is not live here: `--off` drops its
/// entry, on restores it now (a fresh try: the crash marker is reset).
/// `None` when the book has no such entry.
pub async fn pin_offline(state: &Arc<DaemonState>, id: &str, on: bool) -> Option<Result<(), String>> {
    let entry = state.pins.load().into_iter().find(|e| e.journal_id == id)?;
    if !on {
        return Some(state.pins.remove(id).map(|_| ()).map_err(|e| e.to_string()));
    }
    let entry = PinEntry { attempts: 0, ..entry };
    state.pins.set(id, |e| e.attempts = 0);
    Some(match restore_one(state, &entry).await {
        Restored::Live => Ok(()),
        Restored::Waiting(why) => Err(format!("not restored: {why}")),
        Restored::Failed(why) => Err(format!("restore failed: {why}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> (tempfile::TempDir, PinBook) {
        let dir = tempfile::tempdir().unwrap();
        let book = PinBook::at(dir.path().join(PINS_FILE));
        (dir, book)
    }

    #[test]
    fn pin_unpin_and_move_write_through() {
        let (_d, book) = book();
        book.pinned("a", "a", Some("axon"), Some(Path::new("/tmp")), Some("6h"));
        book.pinned("b", "b", None, None, None);
        let v = book.load();
        assert_eq!(v.iter().map(|e| e.journal_id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(v[0].name.as_deref(), Some("axon"));
        assert!(v[0].pinned_at.is_some());
        assert_eq!(v[0].cache_keepwarm.as_deref(), Some("6h"));
        book.moved("a", "a2");
        book.pinned("a", "a2", Some("axon"), None, None); // refresh: no duplicate
        assert_eq!(book.load().iter().map(|e| e.journal_id.as_str()).collect::<Vec<_>>(), ["a2", "b"]);
        book.unpinned("a", "a2");
        assert_eq!(book.load().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(book.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn an_empty_book_never_creates_the_file() {
        let (_d, book) = book();
        book.unpinned("x", "x");
        assert!(!book.path().exists());
    }

    #[test]
    fn one_bad_entry_never_loses_the_rest_and_is_written_back() {
        let (_d, book) = book();
        std::fs::write(
            book.path(),
            r#"{"version":1,"pins":[
                {"journal_id":"good","future_field":{"x":1}},
                {"journal_id":42},
                "garbage",
                {"journal_id":"also","attempts":"lots"}
            ],"future_top":true}"#,
        )
        .unwrap();
        let v = book.load();
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].extra.get("future_field"), Some(&serde_json::json!({"x":1})));
        book.pinned("new", "new", None, None, None);
        let text = std::fs::read_to_string(book.path()).unwrap();
        assert!(text.contains("future_field") && text.contains("garbage") && text.contains("\"lots\""), "{text}");
        assert_eq!(book.load().len(), 2);
    }

    #[test]
    fn a_newer_or_unreadable_file_is_read_only() {
        let (_d, book) = book();
        std::fs::write(book.path(), r#"{"version":9,"pins":[{"journal_id":"a"}]}"#).unwrap();
        assert_eq!(book.load().len(), 1, "still readable");
        assert!(book.remove("a").is_err());
        assert!(std::fs::read_to_string(book.path()).unwrap().contains("\"version\":9"));
        std::fs::write(book.path(), "{not json").unwrap();
        assert!(book.load().is_empty());
        book.pinned("b", "b", None, None, None);
        assert_eq!(std::fs::read_to_string(book.path()).unwrap(), "{not json", "never overwritten");
    }

    #[test]
    fn state_text_reads_as_listed() {
        let mut e = PinEntry { journal_id: "a".into(), ..Default::default() };
        assert_eq!(e.state_text(), "not restored yet");
        e.set_state(STATE_WAITING, "held by pid 7, tui".into());
        assert_eq!(e.state_text(), "waiting: held by pid 7, tui");
    }
}
