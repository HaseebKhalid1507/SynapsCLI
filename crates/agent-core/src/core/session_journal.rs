//! Task 35 — opt-in session journal + periodic atomic snapshots (spec §9.8;
//! decision record: docs/decisions/T35-session-journal-opt-in.md).
//!
//! The DEFAULT persistence (`SessionPersistence::Json`) is byte-for-byte the
//! legacy path: every save atomically rewrites `sessions/<id>.json`. The
//! opt-in `Journal` mode is purely ADDITIVE: the snapshot file keeps the
//! unchanged legacy `Session` schema, and an append-only `sessions/<id>.journal`
//! (JSONL, schema v1) carries the deltas since the snapshot, so a steady-state
//! save costs O(delta) instead of O(total history).
//!
//! Replay is IDEMPOTENT by construction — `msg` records carry absolute
//! history indices (`i == len` appends, `i < len` skips, `i > len` stops at
//! the last consistent prefix) and `meta` records only apply when their
//! `updated_at` is not older than the loaded state. Torn tails (kill during
//! append), stale journals (kill between snapshot and journal reset), and
//! manual journal deletion therefore all recover to a consistent session.
//!
//! Everything here writes through the T4 `private_fs` helpers: 0700 dirs,
//! 0600 files, symlink-refusing, atomic snapshot replacement, synced appends.

use crate::core::session::Session;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Journal record schema version (line-level `"v"` field).
pub const JOURNAL_SCHEMA_VERSION: u8 = 1;

/// Journal size floor before a periodic snapshot is forced.
pub const JOURNAL_SNAPSHOT_MIN_BYTES: u64 = 256 * 1024;

/// A snapshot is due when the journal outgrows `snapshot / RATIO` (bounded
/// write amplification: large sessions stretch the threshold proportionally).
pub const JOURNAL_SNAPSHOT_RATIO: u64 = 4;

/// Bounded tail window scanned for the freshest `meta` record.
const META_TAIL_WINDOW: u64 = 64 * 1024;

/// Which on-disk persistence strategy `Session::save` uses.
///
/// `Json` (the default) is the unchanged legacy behavior. `Journal` is the
/// spec §9.8 opt-in and is only ever selected explicitly via the
/// `session_persistence = journal` config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPersistence {
    #[default]
    Json,
    Journal,
}

impl SessionPersistence {
    /// Parse a config value; anything unrecognized yields `None` so the
    /// caller keeps the safe default and surfaces a typed warning.
    pub fn parse(val: &str) -> Option<Self> {
        match val.trim() {
            "json" => Some(Self::Json),
            "journal" => Some(Self::Journal),
            _ => None,
        }
    }
}

/// How a save landed on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// Full legacy-schema snapshot written (and, in journal mode, the
    /// journal reset to a lone `open` record).
    FullSnapshot,
    /// Delta append: `messages` new history entries plus one meta record.
    Append { messages: usize },
}

/// Machine-readable receipt for benchmarks and delta-proportionality tests.
#[derive(Debug, Clone, Copy)]
pub struct SaveReceipt {
    pub mode: SaveMode,
    pub bytes_written: u64,
}

/// Freshest journaled metadata, read from a bounded tail window — lets
/// session listings stay accurate without parsing the full journal.
#[derive(Debug, Clone)]
pub struct JournalMetaTail {
    pub updated_at: DateTime<Utc>,
    pub session_cost: f64,
}

/// `sessions/<id>.journal` — single-extension name so the session id stays
/// the file stem (chain-head protection and retention pair on the stem).
pub fn journal_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.journal"))
}

/// Snapshot threshold: journal bytes ≥ max(256 KiB, snapshot / 4).
pub fn snapshot_due(journal_bytes: u64, snapshot_bytes: u64) -> bool {
    journal_bytes >= JOURNAL_SNAPSHOT_MIN_BYTES.max(snapshot_bytes / JOURNAL_SNAPSHOT_RATIO)
}

// ─── journal records ─────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(tag = "k")]
enum JournalRecord {
    /// First line of every journal: the snapshot held `base` messages.
    #[serde(rename = "open")]
    Open { v: u8, base: usize },
    /// Message at ABSOLUTE history index `i`.
    #[serde(rename = "msg")]
    Msg {
        v: u8,
        i: usize,
        m: serde_json::Value,
    },
    /// Full session metadata (the `Session` object minus `api_messages`).
    #[serde(rename = "meta")]
    Meta { v: u8, meta: SessionMeta },
}

/// Mirror of [`Session`] WITHOUT `api_messages`, with identical serde
/// attributes. `deny_unknown_fields` + the `session_meta_stays_in_sync_…`
/// test make silent drift between the two types impossible.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionMeta {
    id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    model: String,
    thinking_level: String,
    system_prompt: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    session_cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abort_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compacted_into: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_provenance: Option<crate::prompt::PromptProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction: Option<crate::core::compaction::CompactionRecord>,
}

impl SessionMeta {
    fn of(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            title: s.title.clone(),
            name: s.name.clone(),
            model: s.model.clone(),
            thinking_level: s.thinking_level.clone(),
            system_prompt: s.system_prompt.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            total_input_tokens: s.total_input_tokens,
            total_output_tokens: s.total_output_tokens,
            session_cost: s.session_cost,
            abort_context: s.abort_context.clone(),
            parent_session: s.parent_session.clone(),
            compacted_into: s.compacted_into.clone(),
            prompt_provenance: s.prompt_provenance.clone(),
            compaction: s.compaction.clone(),
        }
    }

    /// Apply onto a loaded session. Only metadata moves — never history.
    fn apply(self, s: &mut Session) {
        s.title = self.title;
        s.name = self.name;
        s.model = self.model;
        s.thinking_level = self.thinking_level;
        s.system_prompt = self.system_prompt;
        s.created_at = self.created_at;
        s.updated_at = self.updated_at;
        s.total_input_tokens = self.total_input_tokens;
        s.total_output_tokens = self.total_output_tokens;
        s.session_cost = self.session_cost;
        s.abort_context = self.abort_context;
        s.parent_session = self.parent_session;
        s.compacted_into = self.compacted_into;
        s.prompt_provenance = self.prompt_provenance;
        s.compaction = self.compaction;
    }
}

/// Parsed view of an on-disk journal: the durable message count implied by
/// the contiguous valid prefix, the last durable message (tripwire), and
/// whether every line parsed cleanly (a torn tail forces a resnapshot so
/// later appends are never shadowed behind an unparseable line).
struct JournalState {
    durable_len: usize,
    last_msg: Option<(usize, serde_json::Value)>,
    bytes: u64,
    clean: bool,
}

fn read_journal_state(path: &Path) -> std::io::Result<Option<JournalState>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bytes = raw.len() as u64;
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();

    let Some(first) = lines.next() else {
        return Ok(Some(JournalState {
            durable_len: 0,
            last_msg: None,
            bytes,
            clean: false,
        }));
    };
    let base = match serde_json::from_str::<JournalRecord>(first) {
        Ok(JournalRecord::Open { base, .. }) => base,
        _ => {
            // No/invalid open record: the journal is unusable as an append
            // target — resnapshot.
            return Ok(Some(JournalState {
                durable_len: 0,
                last_msg: None,
                bytes,
                clean: false,
            }));
        }
    };

    let mut durable_len = base;
    let mut last_msg = None;
    let mut clean = true;
    for line in lines {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(JournalRecord::Msg { i, m, .. }) => {
                if i == durable_len {
                    durable_len += 1;
                    last_msg = Some((i, m));
                } else if i > durable_len {
                    clean = false; // gap — inconsistent suffix
                    break;
                }
                // i < durable_len: stale duplicate, ignore.
            }
            Ok(JournalRecord::Meta { .. }) => {}
            Ok(JournalRecord::Open { .. }) | Err(_) => {
                clean = false; // torn tail or nested open
                break;
            }
        }
    }
    Ok(Some(JournalState {
        durable_len,
        last_msg,
        bytes,
        clean,
    }))
}

// ─── save ────────────────────────────────────────────────────────────────────

/// Persist `session` into `dir` under the given persistence mode.
///
/// `Json` (default): the unchanged legacy path — one atomic private
/// `<id>.json` — plus rollback folding: any journal left over from a
/// previous opt-in is deleted, because the fresh snapshot supersedes it.
///
/// `Journal`: append the delta since the last durable state; write a fresh
/// snapshot instead when the journal is missing/unusable, the history
/// shrank or its durable tail was edited (append-only tripwires), or the
/// journal outgrew [`snapshot_due`].
pub fn save_session_in_dir(
    dir: &Path,
    session: &Session,
    mode: SessionPersistence,
) -> std::io::Result<SaveReceipt> {
    crate::core::private_fs::ensure_private_dir(dir)?;
    match mode {
        SessionPersistence::Json => {
            let json = serde_json::to_string(session).map_err(std::io::Error::other)?;
            crate::core::session::save_json_in_dir(dir, &session.id, json.as_bytes())?;
            // Rollback fold: the snapshot now holds everything; a stale
            // journal must not shadow future legacy-only readers.
            remove_if_exists(&journal_path(dir, &session.id))?;
            Ok(SaveReceipt {
                mode: SaveMode::FullSnapshot,
                bytes_written: json.len() as u64,
            })
        }
        SessionPersistence::Journal => save_journal_mode(dir, session),
    }
}

fn save_journal_mode(dir: &Path, session: &Session) -> std::io::Result<SaveReceipt> {
    let snap_path = dir.join(format!("{}.json", session.id));
    let jpath = journal_path(dir, &session.id);

    let state = match read_journal_state(&jpath)? {
        Some(state) if snap_path.exists() => state,
        // First journal-mode save of a new or legacy session (or the
        // snapshot vanished out-of-band): full snapshot + fresh journal.
        _ => return full_snapshot_reset(dir, session),
    };

    // Append-only tripwires — anything the journal cannot express safely
    // becomes a fresh atomic snapshot instead.
    let rewrite_needed = !state.clean
        || session.api_messages.len() < state.durable_len
        || state.last_msg.as_ref().is_some_and(|(i, m)| {
            session
                .api_messages
                .get(*i)
                .map_or(true, |live| live.as_ref() != m)
        });
    if rewrite_needed {
        return full_snapshot_reset(dir, session);
    }

    // Delta append: new messages (absolute indices) + one meta record.
    let mut buf = Vec::new();
    for (offset, msg) in session.api_messages[state.durable_len..].iter().enumerate() {
        let record = JournalRecord::Msg {
            v: JOURNAL_SCHEMA_VERSION,
            i: state.durable_len + offset,
            m: msg.as_ref().clone(),
        };
        serde_json::to_writer(&mut buf, &record).map_err(std::io::Error::other)?;
        buf.push(b'\n');
    }
    let meta = JournalRecord::Meta {
        v: JOURNAL_SCHEMA_VERSION,
        meta: SessionMeta::of(session),
    };
    serde_json::to_writer(&mut buf, &meta).map_err(std::io::Error::other)?;
    buf.push(b'\n');

    let appended = session.api_messages.len() - state.durable_len;
    let mut file = crate::core::private_fs::open_private_append(&jpath)?;
    file.write_all(&buf)?;
    file.sync_data()?;
    drop(file);

    // Periodic snapshot: fold an oversized journal back into the atomic
    // snapshot. Crash between the two steps leaves a stale journal whose
    // records replay idempotently (see module docs).
    let journal_bytes = state.bytes + buf.len() as u64;
    let snapshot_bytes = std::fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);
    if snapshot_due(journal_bytes, snapshot_bytes) {
        let reset = full_snapshot_reset(dir, session)?;
        return Ok(SaveReceipt {
            mode: SaveMode::FullSnapshot,
            bytes_written: buf.len() as u64 + reset.bytes_written,
        });
    }

    Ok(SaveReceipt {
        mode: SaveMode::Append { messages: appended },
        bytes_written: buf.len() as u64,
    })
}

/// Atomic full snapshot (unchanged legacy schema) followed by an atomic
/// journal reset to a lone `open` record. Snapshot strictly first: a crash
/// between the two leaves a stale-but-idempotent journal, never data loss.
fn full_snapshot_reset(dir: &Path, session: &Session) -> std::io::Result<SaveReceipt> {
    let json = serde_json::to_string(session).map_err(std::io::Error::other)?;
    crate::core::session::save_json_in_dir(dir, &session.id, json.as_bytes())?;

    let open = JournalRecord::Open {
        v: JOURNAL_SCHEMA_VERSION,
        base: session.api_messages.len(),
    };
    let mut line = serde_json::to_vec(&open).map_err(std::io::Error::other)?;
    line.push(b'\n');
    crate::core::private_fs::write_atomic_private(&journal_path(dir, &session.id), &line)?;

    Ok(SaveReceipt {
        mode: SaveMode::FullSnapshot,
        bytes_written: json.len() as u64 + line.len() as u64,
    })
}

// ─── load ────────────────────────────────────────────────────────────────────

/// Load `<id>.json` and, when a journal exists, replay it idempotently.
/// Old-format sessions (no journal) load exactly as before; a session with
/// a torn or stale journal recovers to its last consistent state.
pub fn load_session_in_dir(dir: &Path, id: &str) -> std::io::Result<Session> {
    let content = std::fs::read_to_string(dir.join(format!("{id}.json")))?;
    let mut session: Session = serde_json::from_str(&content).map_err(std::io::Error::other)?;

    let raw = match std::fs::read(journal_path(dir, id)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(session),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    // The first line must be an open record; otherwise the whole journal is
    // untrusted and the snapshot alone is the consistent state.
    match lines.next().map(serde_json::from_str::<JournalRecord>) {
        Some(Ok(JournalRecord::Open { .. })) => {}
        _ => return Ok(session),
    }
    for line in lines {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(JournalRecord::Msg { i, m, .. }) => {
                if i == session.api_messages.len() {
                    session.api_messages.push(std::sync::Arc::new(m));
                } else if i > session.api_messages.len() {
                    break; // gap — stop at the consistent prefix
                }
                // i < len: already in the snapshot — idempotent skip.
            }
            Ok(JournalRecord::Meta { meta, .. }) => {
                if meta.id == session.id && meta.updated_at >= session.updated_at {
                    meta.apply(&mut session);
                }
            }
            Ok(JournalRecord::Open { .. }) | Err(_) => break, // torn tail
        }
    }
    Ok(session)
}

/// Freshest `meta` record from a bounded journal tail window, for listing
/// freshness without a full journal read. `None` when no journal or no
/// complete meta record exists in the window.
pub fn journal_meta_tail(dir: &Path, id: &str) -> Option<JournalMetaTail> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(journal_path(dir, id)).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(META_TAIL_WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = String::new();
    file.take(META_TAIL_WINDOW).read_to_string(&mut tail).ok()?;

    let mut freshest = None;
    for line in tail.lines() {
        if let Ok(JournalRecord::Meta { meta, .. }) = serde_json::from_str::<JournalRecord>(line) {
            freshest = Some(JournalMetaTail {
                updated_at: meta.updated_at,
                session_cost: meta.session_cost,
            });
        }
    }
    freshest
}

// ─── deletion ────────────────────────────────────────────────────────────────

/// Remove a session's snapshot AND journal (compaction rollback, retention).
/// Idempotent — missing files are not errors.
pub fn delete_session_files_in_dir(dir: &Path, id: &str) -> std::io::Result<()> {
    remove_if_exists(&dir.join(format!("{id}.json")))?;
    remove_if_exists(&journal_path(dir, id))
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: `SessionMeta` must mirror every non-history `Session`
    /// field. Serializing a full `Session`, dropping `api_messages`, and
    /// parsing the rest as `SessionMeta` (deny_unknown_fields) fails the
    /// moment `Session` grows a field this module does not journal.
    #[test]
    fn session_meta_stays_in_sync_with_session_schema() {
        let mut s = Session::new("model-x", "medium", Some("prompt"));
        s.name = Some("named".into());
        s.abort_context = Some("ctx".into());
        s.parent_session = Some("parent".into());
        s.compacted_into = Some("child".into());
        let mut value = serde_json::to_value(&s).unwrap();
        value.as_object_mut().unwrap().remove("api_messages");
        let meta: SessionMeta = serde_json::from_value(value)
            .expect("Session grew a field SessionMeta does not mirror — extend SessionMeta");
        assert_eq!(meta.id, s.id);
        assert_eq!(meta.name.as_deref(), Some("named"));
    }

    #[test]
    fn meta_roundtrip_applies_every_field() {
        let mut a = Session::new("model-a", "high", Some("sys"));
        a.title = "t".into();
        a.total_input_tokens = 7;
        a.session_cost = 0.5;
        a.updated_at = chrono::Utc::now();
        let mut b = Session::new("model-b", "low", None);
        b.id = a.id.clone();
        SessionMeta::of(&a).apply(&mut b);
        assert_eq!(b.model, "model-a");
        assert_eq!(b.title, "t");
        assert_eq!(b.total_input_tokens, 7);
        assert_eq!(b.session_cost, 0.5);
        assert_eq!(b.updated_at, a.updated_at);
    }

    /// Private modes (spec §5.4): journal-mode saves keep the 0700 dir and
    /// 0600 files under a permissive umask, exactly like the legacy path.
    #[cfg(unix)]
    mod private_modes {
        use super::*;
        use crate::core::private_fs::test_support::UmaskGuard;
        use serial_test::serial;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        #[serial(umask)]
        fn journal_files_are_0600_in_0700_dir_under_permissive_umask() {
            let _umask = UmaskGuard::set(0);
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("sessions");
            let mut s = Session::new("m", "medium", None);
            save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
            s.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role":"user","content":"x"}),
            ));
            save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();

            assert_eq!(mode_of(&dir), 0o700, "sessions dir must be 0700");
            assert_eq!(
                mode_of(&dir.join(format!("{}.json", s.id))),
                0o600,
                "snapshot must be 0600"
            );
            assert_eq!(
                mode_of(&journal_path(&dir, &s.id)),
                0o600,
                "journal must be 0600"
            );
        }

        #[test]
        fn journal_append_refuses_symlink_target() {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            let mut s = Session::new("m", "medium", None);
            let victim = tmp.path().join("victim");
            std::fs::write(&victim, "original").unwrap();
            std::os::unix::fs::symlink(&victim, journal_path(&dir, &s.id)).unwrap();
            // Snapshot write succeeds; the journal step must refuse.
            s.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role":"user","content":"x"}),
            ));
            let res = save_session_in_dir(&dir, &s, SessionPersistence::Journal);
            assert!(res.is_err(), "journal write onto a symlink must fail");
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
        }
    }
}
