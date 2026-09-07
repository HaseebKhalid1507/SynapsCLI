//! Session snapshots with opt-in delta journals and durable checkpoints.
//!
//! New journal snapshots carry a storage-only `_journal_generation` UUID;
//! v2 journals replay ONLY against that exact generation. Publishing a new
//! snapshot is the logical commit, even if a crash leaves the old journal.
//! Unmarked legacy snapshots still accept v1 journals. Their pre-migration
//! crash ambiguity cannot be repaired retrospectively. Old binaries can read
//! the Session-shaped snapshot but cannot replay v2 deltas; downgrade/export
//! should first fold the journal with a JSON save using this implementation.
//!
//! Normal JSON saves retain the legacy bytes when no journal/generation has
//! ever been present. Once bound, all snapshot replacements (including JSON
//! saves) rotate the generation so leftover journals cannot resurrect history.
//! Metadata is not a Session field: serde exports/mirrors ignore it. Stripping
//! it loses v2 deltas, rather than applying a potentially unrelated journal.
//!
//! Journal appends write O(delta) bytes; a streaming history hash checks the
//! entire saved prefix for edits (O(history) CPU, no history-sized allocation).
//! Journal-bound snapshot rotation syncs the snapshot directory before removing
//! old deltas; JSON-only normal saves retain their prior best-effort behavior.
//! Durable saves always publish a full snapshot and sync through cleanup.
//! Errors can follow logical commit;
//! callers must stop and reload/retry, not assume rollback. All saves of one
//! session must be externally serialized; no multi-writer transaction is offered.
//!
//! Private 0700/0600, no-symlink handle-relative I/O is preserved on Unix.
//! Normal saves retain the documented non-Unix best effort; durable saves
//! fail closed there rather than pretend directory fsync is supported.

use crate::core::session::Session;
use crate::core::stream_types::SharedMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Journal record schema version (line-level `"v"` field).
pub const JOURNAL_SCHEMA_VERSION: u8 = 2;

/// Journal size floor before a periodic snapshot is forced.
pub const JOURNAL_SNAPSHOT_MIN_BYTES: u64 = 256 * 1024;

/// A snapshot is due when the journal outgrows `snapshot / RATIO` (bounded
/// write amplification: large sessions stretch the threshold proportionally).
pub const JOURNAL_SNAPSHOT_RATIO: u64 = 4;

/// Bounded tail window scanned for the freshest `meta` record.
const META_TAIL_WINDOW: u64 = 64 * 1024;

/// Hard cap on any single persisted-session artifact read (snapshot or
/// journal). Reads are bounded on the OPENED handle; an artifact past the
/// cap is refused rather than slurped.
pub const MAX_PERSISTED_READ_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

// ─── confined resolution (fix1 I1 + fix2) ───────────────────────────────────
//
// STRICT TRUSTED-ROOT SEMANTICS (fix2): the sessions directory path is
// resolved with EVERY component — ancestors AND the final one — opened
// handle-relatively from `/` with symlinks refused
// (`ConfinedDir::{open,create}_absolute_no_symlinks`; Linux uses one atomic
// `openat2 RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` from the root handle).
// Nothing on the path is trusted; there is no check-then-open race — a
// component swapped to a symlink at any moment fails the open itself.
// Artifacts inside the directory are then opened/written/removed relative
// to that ONE handle. Operators whose base dir legitimately sits behind
// ancestor symlinks (e.g. `/home` → `var/home`) must point
// `SYNAPS_BASE_DIR` at the canonical path.
//
// Non-unix keeps the crate's documented best-effort pathname fallback
// (final-component symlink refusal only).

#[cfg(unix)]
type SessionsDirHandle = crate::core::private_fs::ConfinedDir;

/// Open the sessions dir strictly. `Ok(None)` when a path component does
/// not exist; symlinks anywhere are errors.
#[cfg(unix)]
fn open_sessions_dir(dir: &Path) -> std::io::Result<Option<SessionsDirHandle>> {
    match crate::core::private_fs::ConfinedDir::open_absolute_no_symlinks(dir) {
        Ok(handle) => Ok(Some(handle)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Create-or-open the sessions dir strictly (0700 leaf).
#[cfg(unix)]
fn create_sessions_dir(dir: &Path) -> std::io::Result<SessionsDirHandle> {
    crate::core::private_fs::ConfinedDir::create_absolute_no_symlinks(dir)
}

/// Open `<name>` inside an already-strictly-opened sessions dir.
/// `Ok(None)` when the file does not exist; a symlinked artifact errors.
#[cfg(unix)]
fn open_artifact(handle: &SessionsDirHandle, name: &str) -> std::io::Result<Option<std::fs::File>> {
    match handle.open_file(&[name.to_string()]) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// One-shot strict open of `<name>` inside `dir`.
#[cfg(unix)]
pub(crate) fn confined_open(dir: &Path, name: &str) -> std::io::Result<Option<std::fs::File>> {
    let Some(handle) = open_sessions_dir(dir)? else {
        return Ok(None);
    };
    open_artifact(&handle, name)
}

// ── Non-unix best-effort fallbacks (documented, matching `private_fs`) ──

#[cfg(not(unix))]
struct SessionsDirHandle {
    dir: std::path::PathBuf,
}

#[cfg(not(unix))]
fn open_sessions_dir(dir: &Path) -> std::io::Result<Option<SessionsDirHandle>> {
    if !dir.exists() {
        return Ok(None);
    }
    Ok(Some(SessionsDirHandle {
        dir: dir.to_path_buf(),
    }))
}

#[cfg(not(unix))]
fn create_sessions_dir(dir: &Path) -> std::io::Result<SessionsDirHandle> {
    crate::core::private_fs::ensure_private_dir(dir)?;
    Ok(SessionsDirHandle {
        dir: dir.to_path_buf(),
    })
}

#[cfg(not(unix))]
impl SessionsDirHandle {
    fn write_atomic(&self, name: &str, data: &[u8]) -> std::io::Result<()> {
        crate::core::private_fs::write_atomic_private(&self.dir.join(name), data)
            .map_err(std::io::Error::other)
    }
    fn append_file(&self, name: &str) -> std::io::Result<std::fs::File> {
        crate::core::private_fs::open_private_append(&self.dir.join(name))
            .map_err(std::io::Error::other)
    }
    fn remove_file(&self, name: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.dir.join(name))
    }
}

#[cfg(not(unix))]
fn open_artifact(handle: &SessionsDirHandle, name: &str) -> std::io::Result<Option<std::fs::File>> {
    let path = handle.dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::other(format!(
                "refusing symlinked session artifact {name:?}"
            )))
        }
        _ => {}
    }
    match std::fs::File::open(&path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
pub(crate) fn confined_open(dir: &Path, name: &str) -> std::io::Result<Option<std::fs::File>> {
    let Some(handle) = open_sessions_dir(dir)? else {
        return Ok(None);
    };
    open_artifact(&handle, name)
}

/// Bounded read of a whole artifact from an already-confined handle.
fn read_artifact_bytes(handle: &SessionsDirHandle, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let Some(file) = open_artifact(handle, name)? else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    file.take(MAX_PERSISTED_READ_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_PERSISTED_READ_BYTES {
        return Err(std::io::Error::other(format!(
            "session artifact {name:?} exceeds the {MAX_PERSISTED_READ_BYTES}-byte read bound"
        )));
    }
    Ok(Some(buf))
}

/// One strictly-resolved directory entry for session listings.
#[derive(Debug, Clone)]
pub struct SessionDirEntry {
    pub name: String,
    pub mtime: Option<std::time::SystemTime>,
    /// Stored bytes for metadata-only accounting. Obtained from the directory
    /// entry stat; reading it never opens session content.
    pub byte_len: u64,
}

/// Enumerate the sessions dir through the SAME strict handle resolution as
/// every other T35 operation (fix2): handle-relative `readdir` +
/// `fstatat(AT_SYMLINK_NOFOLLOW)` — a symlinked ancestor refuses, a
/// missing dir lists empty, and entry mtimes are of the entries
/// themselves, never symlink targets.
pub fn session_dir_entries(dir: &Path) -> std::io::Result<Vec<SessionDirEntry>> {
    #[cfg(unix)]
    {
        let Some(handle) = open_sessions_dir(dir)? else {
            return Ok(Vec::new());
        };
        Ok(handle
            .entries()?
            .into_iter()
            .filter(|e| e.is_file)
            .map(|e| SessionDirEntry {
                name: e.name,
                mtime: e.mtime_unix_ms.map(|ms| {
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(ms)
                }),
                byte_len: e.byte_len,
            })
            .collect())
    }
    #[cfg(not(unix))]
    {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let byte_len = entry
                .metadata()
                .map(|metadata| metadata.len())
                .unwrap_or_default();
            out.push(SessionDirEntry {
                name,
                mtime: entry.metadata().and_then(|m| m.modified()).ok(),
                byte_len,
            });
        }
        Ok(out)
    }
}

/// Which on-disk persistence strategy `Session::save` uses.
///
/// `Json` (the default) writes a full Session-shaped snapshot. `Journal` is
/// the spec §9.8 opt-in and is only ever selected explicitly via the
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
    /// Full Session-shaped snapshot written (and, in journal mode, the
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
    /// `None` for meta records written before the count was journaled.
    pub message_count: Option<usize>,
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
    Open {
        v: u8,
        base: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history_hash: Option<String>,
    },
    /// Message at ABSOLUTE history index `i`.
    #[serde(rename = "msg")]
    Msg {
        v: u8,
        i: usize,
        m: serde_json::Value,
    },
    /// Full session metadata (the `Session` object minus `api_messages`).
    #[serde(rename = "meta")]
    Meta {
        v: u8,
        meta: Box<SessionMeta>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history_hash: Option<String>,
    },
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
    message_count: Option<usize>,
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
            message_count: Some(s.api_messages.len()),
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

/// A binding is deliberately outside Session/SessionMeta. It is emitted
/// before `api_messages`, so listings and append checks need only the header.
#[derive(Default, Deserialize)]
struct SnapshotBinding {
    #[serde(default, rename = "_journal_generation")]
    generation: Option<String>,
}

fn snapshot_binding(file: std::fs::File) -> std::io::Result<SnapshotBinding> {
    let header = crate::core::session::read_session_header_from_file(file)
        .ok_or_else(|| std::io::Error::other("cannot read session snapshot header"))?;
    serde_json::from_str(&header).map_err(std::io::Error::other)
}

/// v1 is interpretable only against an unmarked legacy snapshot. A bound
/// snapshot must never fall back to v1, even for an empty/torn v2 journal.
fn matching_open(record: &JournalRecord, snapshot_generation: Option<&str>) -> Option<u8> {
    match record {
        JournalRecord::Open {
            v: 1,
            generation: None,
            ..
        } if snapshot_generation.is_none() => Some(1),
        JournalRecord::Open {
            v,
            generation: Some(generation),
            ..
        } if *v == JOURNAL_SCHEMA_VERSION
            && !generation.is_empty()
            && Some(generation.as_str()) == snapshot_generation =>
        {
            Some(*v)
        }
        _ => None,
    }
}

/// Hash the exact ordered prefix, including all message fields. Unlike the
/// old last-message tripwire this detects same-length edits anywhere, even
/// when the journal contains only its open record. Uses the existing sha2 dep.
fn history_hash(messages: &[SharedMessage]) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, messages).map_err(std::io::Error::other)?;
    Ok(format!("{:x}", writer.0.finalize()))
}

/// Only clean, generation-matched v2 journals can be appended to. Legacy
/// saves migrate through a full snapshot; torn/gapped tails resnapshot too.
struct JournalState {
    durable_len: usize,
    history_hash: String,
    bytes: u64,
}

fn read_journal_state(
    handle: &SessionsDirHandle,
    id: &str,
    generation: Option<&str>,
) -> std::io::Result<Option<JournalState>> {
    let Some(raw) = read_artifact_bytes(handle, &format!("{id}.journal"))? else {
        return Ok(None);
    };
    if !raw.ends_with(b"\n") {
        return Ok(None); // appending after an unterminated line would hide data
    }
    let Ok(text) = std::str::from_utf8(&raw) else {
        return Ok(None);
    };
    let mut lines = text.lines();
    let Some(open) = lines
        .next()
        .and_then(|s| serde_json::from_str::<JournalRecord>(s).ok())
    else {
        return Ok(None);
    };
    if matching_open(&open, generation) != Some(JOURNAL_SCHEMA_VERSION) {
        return Ok(None);
    }
    let JournalRecord::Open {
        base,
        history_hash: Some(mut hash),
        ..
    } = open
    else {
        return Ok(None);
    };
    let mut durable_len = base;
    let mut hashed_len = base;
    for line in lines {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(JournalRecord::Msg { v, i, .. })
                if v == JOURNAL_SCHEMA_VERSION && i == durable_len =>
            {
                durable_len += 1;
            }
            Ok(JournalRecord::Meta {
                v,
                meta,
                history_hash: Some(next_hash),
            }) if v == JOURNAL_SCHEMA_VERSION
                && meta.id == id
                && meta.message_count == Some(durable_len) =>
            {
                hash = next_hash;
                hashed_len = durable_len;
            }
            _ => return Ok(None),
        }
    }
    if hashed_len != durable_len {
        return Ok(None); // messages without a complete committing meta record
    }
    Ok(Some(JournalState {
        durable_len,
        history_hash: hash,
        bytes: raw.len() as u64,
    }))
}

// ─── save ────────────────────────────────────────────────────────────────────

/// Persist under the configured mode. Journal-bound replacement syncs the
/// snapshot directory before discarding old deltas; normal JSON-only saves do
/// not promise directory durability. Use [`save_session_durable_in_dir`] for a
/// full context-head barrier. Snapshot replacements are generation-safe in both.
pub fn save_session_in_dir(
    dir: &Path,
    session: &Session,
    mode: SessionPersistence,
) -> std::io::Result<SaveReceipt> {
    let handle = create_sessions_dir(dir)?;
    match mode {
        SessionPersistence::Json => {
            // Keep virgin JSON-only saves byte-compatible. Once a journal or
            // generation exists, never publish an unmarked snapshot again.
            let journal = open_artifact(&handle, &format!("{}.journal", session.id))?;
            let bound =
                open_artifact(&handle, &format!("{}.json", session.id))?.is_some_and(|file| {
                    // An unreadable/oversized header must not prevent repair
                    // by a full save; conservatively rotate the generation.
                    snapshot_binding(file).map_or(true, |b| b.generation.is_some())
                });
            publish_snapshot(&handle, session, mode, false, journal.is_some() || bound)
        }
        SessionPersistence::Journal => save_journal_mode(&handle, session),
    }
}

/// Blocking body of `Session::save_durable`: a full generation-bound snapshot
/// in either mode. Fsync the file, rename it, fsync the directory, then clean
/// up the journal and fsync the directory again. Snapshot rename is the logical
/// commit; any later error is returned without rollback. Retry/reload is safe
/// with stale journals. Concurrent saves of the same session are NOT supported.
pub fn save_session_durable_in_dir(
    dir: &Path,
    session: &Session,
    mode: SessionPersistence,
) -> std::io::Result<SaveReceipt> {
    #[cfg(unix)]
    {
        let handle = SessionsDirHandle::create_absolute_no_symlinks_durable(dir)?;
        publish_snapshot(&handle, session, mode, true, true)
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, session, mode);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable session checkpoints require Unix directory fsync",
        ))
    }
}

fn save_journal_mode(
    handle: &SessionsDirHandle,
    session: &Session,
) -> std::io::Result<SaveReceipt> {
    let journal_name = format!("{}.journal", session.id);

    let snapshot = open_artifact(handle, &format!("{}.json", session.id))?;
    let Some(snapshot) = snapshot else {
        return full_snapshot_reset(handle, session);
    };
    let snapshot_bytes = snapshot.metadata()?.len();
    let Ok(binding) = snapshot_binding(snapshot) else {
        return full_snapshot_reset(handle, session);
    };
    let Some(state) = read_journal_state(handle, &session.id, binding.generation.as_deref())?
    else {
        return full_snapshot_reset(handle, session);
    };

    if session.api_messages.len() < state.durable_len
        || history_hash(&session.api_messages[..state.durable_len])? != state.history_hash
    {
        return full_snapshot_reset(handle, session);
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
        meta: Box::new(SessionMeta::of(session)),
        history_hash: Some(history_hash(&session.api_messages)?),
    };
    serde_json::to_writer(&mut buf, &meta).map_err(std::io::Error::other)?;
    buf.push(b'\n');

    let appended = session.api_messages.len() - state.durable_len;
    let mut file = handle.append_file(&journal_name)?;
    file.write_all(&buf)?;
    file.sync_data()?;
    drop(file);

    // Periodic snapshot: its fresh generation invalidates the old journal
    // at publication, before the journal reset can occur.
    let journal_bytes = state.bytes + buf.len() as u64;
    if snapshot_due(journal_bytes, snapshot_bytes) {
        let reset = full_snapshot_reset(handle, session)?;
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

/// Borrowing mirror of [`Session`] used ONLY for snapshot serialization:
/// identical field order and serde attributes, with `message_count`
/// computed from `api_messages.len()` at write time instead of read from
/// the (non-authoritative) in-memory field. Field order matters —
/// `message_count` must precede `api_messages` for `read_session_header`.
/// The `snapshot_json_matches_session_schema` test guards drift.
#[derive(Serialize)]
struct SessionSnapshotRef<'a> {
    // Storage-only metadata, omitted for legacy JSON-only saves. Session
    // serde deliberately ignores it; it is not journaled as SessionMeta.
    #[serde(skip_serializing_if = "Option::is_none")]
    _journal_generation: Option<&'a str>,
    id: &'a str,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: &'a Option<String>,
    model: &'a str,
    thinking_level: &'a str,
    system_prompt: &'a Option<String>,
    created_at: &'a DateTime<Utc>,
    updated_at: &'a DateTime<Utc>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    session_cost: f64,
    message_count: usize,
    api_messages: &'a [SharedMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    abort_context: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compacted_into: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_provenance: &'a Option<crate::prompt::PromptProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: &'a Option<crate::core::compaction::CompactionRecord>,
}

impl<'a> SessionSnapshotRef<'a> {
    fn of(s: &'a Session, generation: Option<&'a str>) -> Self {
        Self {
            _journal_generation: generation,
            id: &s.id,
            title: &s.title,
            name: &s.name,
            model: &s.model,
            thinking_level: &s.thinking_level,
            system_prompt: &s.system_prompt,
            created_at: &s.created_at,
            updated_at: &s.updated_at,
            total_input_tokens: s.total_input_tokens,
            total_output_tokens: s.total_output_tokens,
            session_cost: s.session_cost,
            message_count: s.api_messages.len(),
            api_messages: &s.api_messages,
            abort_context: &s.abort_context,
            parent_session: &s.parent_session,
            compacted_into: &s.compacted_into,
            prompt_provenance: &s.prompt_provenance,
            compaction: &s.compaction,
        }
    }
}

/// Full-snapshot JSON with fresh count and optional storage-only binding.
fn snapshot_json(session: &Session, generation: Option<&str>) -> std::io::Result<String> {
    serde_json::to_string(&SessionSnapshotRef::of(session, generation))
        .map_err(std::io::Error::other)
}

fn full_snapshot_reset(
    handle: &SessionsDirHandle,
    session: &Session,
) -> std::io::Result<SaveReceipt> {
    publish_snapshot(handle, session, SessionPersistence::Journal, false, true)
}

fn publish_snapshot(
    handle: &SessionsDirHandle,
    session: &Session,
    mode: SessionPersistence,
    durable: bool,
    bind: bool,
) -> std::io::Result<SaveReceipt> {
    let generation = bind.then(|| uuid::Uuid::new_v4().to_string());
    let json = snapshot_json(session, generation.as_deref())?;
    // Prepare all serialization BEFORE the logical commit.
    let mut journal = Vec::new();
    if mode == SessionPersistence::Journal {
        let open = JournalRecord::Open {
            v: JOURNAL_SCHEMA_VERSION,
            base: session.api_messages.len(),
            generation,
            history_hash: Some(history_hash(&session.api_messages)?),
        };
        serde_json::to_writer(&mut journal, &open).map_err(std::io::Error::other)?;
        journal.push(b'\n');
    }
    // Preflight journal symlinks in either mode, even JSON cleanup (which
    // otherwise only unlinks). This does not claim multi-writer isolation.
    open_artifact(handle, &format!("{}.journal", session.id))?;
    handle.write_atomic(&format!("{}.json", session.id), json.as_bytes())?;
    // Before destroying the old journal, the new snapshot must be durable.
    // Binding prevents new-snapshot/old-journal replay, but without this sync
    // a power loss could recover old-snapshot/new-journal and lose deltas.
    #[cfg(unix)]
    if durable || bind {
        sync_sessions_dir(handle)?;
    }
    if mode == SessionPersistence::Journal {
        handle.write_atomic(&format!("{}.journal", session.id), &journal)?;
    } else {
        remove_artifact_if_exists(handle, &format!("{}.journal", session.id))?;
    }
    if durable {
        sync_sessions_dir(handle)?;
    }
    Ok(SaveReceipt {
        mode: SaveMode::FullSnapshot,
        bytes_written: json.len() as u64 + journal.len() as u64,
    })
}

fn sync_sessions_dir(handle: &SessionsDirHandle) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        handle.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = handle;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable session checkpoints require Unix directory fsync",
        ))
    }
}

// ─── load ────────────────────────────────────────────────────────────────────

/// Load `<id>.json` and, when a journal exists, replay it idempotently.
/// Old-format sessions (no journal) load exactly as before; a session with
/// a torn or stale journal recovers to its last consistent state. Both
/// reads are confined nofollow bounded handle reads (fix1 I1).
pub fn load_session_in_dir(dir: &Path, id: &str) -> std::io::Result<Session> {
    // ONE strict resolution per load (fix2); both artifact reads below are
    // relative to this handle.
    let handle = open_sessions_dir(dir)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no sessions directory for '{id}'"),
        )
    })?;
    let snapshot = read_artifact_bytes(&handle, &format!("{id}.json"))?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session snapshot for '{id}'"),
        )
    })?;
    let binding: SnapshotBinding =
        serde_json::from_slice(&snapshot).map_err(std::io::Error::other)?;
    let mut session: Session = serde_json::from_slice(&snapshot).map_err(std::io::Error::other)?;

    let Some(raw) = read_artifact_bytes(&handle, &format!("{id}.journal"))? else {
        return Ok(session);
    };
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let version = match lines
        .next()
        .and_then(|s| serde_json::from_str::<JournalRecord>(s).ok())
        .and_then(|open| matching_open(&open, binding.generation.as_deref()))
    {
        Some(version) => version,
        None => return Ok(session),
    };
    for line in lines {
        match serde_json::from_str::<JournalRecord>(line) {
            // fix1 M1: an unknown-version record ends the valid prefix.
            Ok(JournalRecord::Msg { v, .. }) | Ok(JournalRecord::Meta { v, .. })
                if v != version =>
            {
                break;
            }
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
/// freshness without a full journal read. `None` when no journal, no
/// complete supported-version meta record, or a refused (non-confined)
/// artifact — a symlinked journal discloses nothing.
pub fn journal_meta_tail(dir: &Path, id: &str) -> Option<JournalMetaTail> {
    let handle = open_sessions_dir(dir).ok()??;
    let binding = snapshot_binding(open_artifact(&handle, &format!("{id}.json")).ok()??).ok()?;
    journal_meta_tail_from_handle(&handle, id, binding.generation.as_deref())
}

/// Listings already read a snapshot header: use THAT generation rather than
/// reopening a possibly replaced snapshot and mixing two checkpoint heads.
pub(crate) fn journal_meta_tail_for_generation(
    dir: &Path,
    id: &str,
    generation: Option<&str>,
) -> Option<JournalMetaTail> {
    let handle = open_sessions_dir(dir).ok()??;
    journal_meta_tail_from_handle(&handle, id, generation)
}

fn journal_meta_tail_from_handle(
    handle: &SessionsDirHandle,
    id: &str,
    generation: Option<&str>,
) -> Option<JournalMetaTail> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    let mut file = open_artifact(handle, &format!("{id}.journal")).ok()??;
    let mut first = String::new();
    BufReader::new((&mut file).take(4096))
        .read_line(&mut first)
        .ok()?;
    let open: JournalRecord = serde_json::from_str(&first).ok()?;
    let version = matching_open(&open, generation)?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(META_TAIL_WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = String::new();
    file.take(META_TAIL_WINDOW).read_to_string(&mut tail).ok()?;

    let mut freshest = None;
    for line in tail.lines() {
        if let Ok(JournalRecord::Meta { v, meta, .. }) = serde_json::from_str::<JournalRecord>(line)
        {
            if v == version && meta.id == id {
                freshest = Some(JournalMetaTail {
                    updated_at: meta.updated_at,
                    session_cost: meta.session_cost,
                    message_count: meta.message_count,
                });
            }
        }
    }
    freshest
}

// ─── deletion ────────────────────────────────────────────────────────────────

/// Remove a session's snapshot AND journal (compaction rollback, retention).
/// Idempotent — missing files are not errors.
pub fn delete_session_files_in_dir(dir: &Path, id: &str) -> std::io::Result<()> {
    let Some(handle) = open_sessions_dir(dir)? else {
        return Ok(()); // no directory — idempotently nothing to delete
    };
    remove_artifact_if_exists(&handle, &format!("{id}.json"))?;
    remove_artifact_if_exists(&handle, &format!("{id}.journal"))
}

fn remove_artifact_if_exists(handle: &SessionsDirHandle, name: &str) -> std::io::Result<()> {
    match handle.remove_file(name) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Strict-handle snapshot write shared with the legacy
/// `session::save_json_in_dir` path (fix2): same root resolution and the
/// same handle-relative atomic private write as every journal operation.
#[cfg(test)]
pub(crate) fn write_json_snapshot(dir: &Path, id: &str, json: &[u8]) -> std::io::Result<()> {
    let handle = create_sessions_dir(dir)?;
    handle.write_atomic(&format!("{id}.json"), json)
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
    fn snapshot_json_matches_session_schema() {
        let mut s = Session::new("model-x", "medium", Some("prompt"));
        s.name = Some("named".into());
        s.abort_context = Some("ctx".into());
        s.parent_session = Some("parent".into());
        s.compacted_into = Some("child".into());
        s.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": "hi"}),
        ));
        s.message_count = 0; // stale in memory — snapshot must not trust it
                             // Expected = the old clone-and-refresh path, byte for byte.
        let mut expected = s.clone();
        expected.message_count = expected.api_messages.len();
        let expected = serde_json::to_string(&expected).unwrap();
        assert_eq!(snapshot_json(&s, None).unwrap(), expected);
        let back: Session = serde_json::from_str(&snapshot_json(&s, None).unwrap()).unwrap();
        assert_eq!(back.message_count, 1);
        // Same check with every optional absent.
        let s = Session::new("model-x", "medium", None);
        let mut expected = s.clone();
        expected.message_count = 0;
        assert_eq!(
            snapshot_json(&s, None).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }

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

    fn with_messages(n: usize) -> Session {
        let mut s = Session::new("checkpoint-test", "medium", None);
        for i in 0..n {
            push_message(&mut s, &format!("old {i}"));
        }
        s
    }

    fn push_message(s: &mut Session, text: &str) {
        s.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": text}),
        ));
    }

    fn assert_saved(dir: &Path, expected: &Session) {
        let loaded = load_session_in_dir(dir, &expected.id).unwrap();
        let mut expected = expected.clone();
        expected.message_count = expected.api_messages.len();
        // message_count is a snapshot hint, not authoritative after replay.
        let mut loaded = loaded;
        loaded.message_count = loaded.api_messages.len();
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    fn disk_generation(dir: &Path, id: &str) -> String {
        let snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{id}.json"))).unwrap())
                .unwrap();
        snapshot["_journal_generation"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn bound_snapshot_is_session_shaped_and_storage_metadata_is_not_mirrored() {
        let s = with_messages(2);
        let json = snapshot_json(&s, Some("test-generation")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value
                .as_object_mut()
                .unwrap()
                .remove("_journal_generation")
                .unwrap(),
            "test-generation"
        );
        let mut expected = s.clone();
        expected.message_count = 2;
        assert_eq!(value, serde_json::to_value(&expected).unwrap());
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), value);
        value.as_object_mut().unwrap().remove("api_messages");
        serde_json::from_value::<SessionMeta>(value).unwrap();
    }

    /// Recreate the exact two-file state of a crash after snapshot rename but
    /// before journal reset/unlink: keep the newly published snapshot and put
    /// back the old journal bytes. No clocks, processes, config, or real data.
    fn replacement_crash_case(mode: SessionPersistence, durable: bool, shorten: bool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut s = with_messages(2);
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        for _ in 0..4 {
            push_message(&mut s, "obsolete journal message");
        }
        s.title = "obsolete metadata".into();
        s.updated_at += chrono::Duration::hours(1);
        s.session_cost = 99.0;
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let old = std::fs::read(journal_path(&dir, &s.id)).unwrap();
        let old_generation = disk_generation(&dir, &s.id);

        // Even same-length edits with an unchanged final message must commit.
        s.api_messages[0] =
            std::sync::Arc::new(serde_json::json!({"role":"user","content":"new head"}));
        if shorten {
            s.api_messages.truncate(3);
        }
        s.title = "candidate".into();
        s.updated_at -= chrono::Duration::hours(2); // old metadata would win by timestamp alone
        s.session_cost = 1.0;
        let receipt = if durable {
            save_session_durable_in_dir(&dir, &s, mode).unwrap()
        } else {
            save_session_in_dir(&dir, &s, mode).unwrap()
        };
        assert_eq!(receipt.mode, SaveMode::FullSnapshot);
        assert_ne!(disk_generation(&dir, &s.id), old_generation);
        std::fs::write(journal_path(&dir, &s.id), old).unwrap();
        assert_saved(&dir, &s);
        assert!(journal_meta_tail(&dir, &s.id).is_none());

        // A subsequent normal save must not append into the stale generation.
        push_message(&mut s, "post-checkpoint");
        let receipt = save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        assert_eq!(receipt.mode, SaveMode::FullSnapshot);
        assert_saved(&dir, &s);
        push_message(&mut s, "new generation delta");
        assert_eq!(
            save_session_in_dir(&dir, &s, SessionPersistence::Journal)
                .unwrap()
                .mode,
            SaveMode::Append { messages: 1 }
        );
        assert_saved(&dir, &s);
    }

    #[test]
    fn normal_replacements_ignore_stale_journal_after_snapshot_publication() {
        for mode in [SessionPersistence::Json, SessionPersistence::Journal] {
            for shorten in [false, true] {
                replacement_crash_case(mode, false, shorten);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn durable_replacements_ignore_stale_journal_after_snapshot_publication() {
        for mode in [SessionPersistence::Json, SessionPersistence::Journal] {
            for shorten in [false, true] {
                replacement_crash_case(mode, true, shorten);
            }
        }
    }

    #[test]
    fn legacy_v1_reads_then_migrates_and_cannot_replay_over_replacement() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut s = with_messages(1);
        std::fs::write(
            dir.join(format!("{}.json", s.id)),
            snapshot_json(&s, None).unwrap(),
        )
        .unwrap();
        let mut legacy = vec![JournalRecord::Open {
            v: 1,
            base: 1,
            generation: None,
            history_hash: None,
        }];
        for i in 1..5 {
            push_message(&mut s, "legacy delta");
            legacy.push(JournalRecord::Msg {
                v: 1,
                i,
                m: s.api_messages[i].as_ref().clone(),
            });
        }
        s.title = "legacy title".into();
        s.updated_at += chrono::Duration::hours(1);
        legacy.push(JournalRecord::Meta {
            v: 1,
            meta: Box::new(SessionMeta::of(&s)),
            history_hash: None,
        });
        let mut bytes = Vec::new();
        for record in legacy {
            serde_json::to_writer(&mut bytes, &record).unwrap();
            bytes.push(b'\n');
        }
        std::fs::write(journal_path(&dir, &s.id), &bytes).unwrap();
        assert_saved(&dir, &s);
        assert_eq!(
            journal_meta_tail(&dir, &s.id).unwrap().message_count,
            Some(5)
        );
        let migrated = load_session_in_dir(&dir, &s.id).unwrap();
        assert_eq!(
            save_session_in_dir(&dir, &migrated, SessionPersistence::Journal)
                .unwrap()
                .mode,
            SaveMode::FullSnapshot
        );
        assert_saved(&dir, &s);
        assert!(!disk_generation(&dir, &s.id).is_empty());

        s.api_messages.truncate(2);
        s.title = "new window".into();
        s.updated_at -= chrono::Duration::hours(2);
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        std::fs::write(journal_path(&dir, &s.id), &bytes).unwrap();
        assert_saved(&dir, &s);
        assert!(journal_meta_tail(&dir, &s.id).is_none());
    }

    #[test]
    fn same_length_edits_in_snapshot_or_earlier_journal_message_resnapshot() {
        for appended in [false, true] {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().canonicalize().unwrap();
            let mut s = with_messages(3);
            save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
            let edited = if appended {
                push_message(&mut s, "editable");
                push_message(&mut s, "unchanged last message");
                save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
                3
            } else {
                0
            };
            let old_generation = disk_generation(&dir, &s.id);
            std::sync::Arc::make_mut(&mut s.api_messages[edited])["content"] = "replaced".into();
            assert_eq!(
                save_session_in_dir(&dir, &s, SessionPersistence::Journal)
                    .unwrap()
                    .mode,
                SaveMode::FullSnapshot
            );
            assert_ne!(disk_generation(&dir, &s.id), old_generation);
            assert_saved(&dir, &s);
        }
    }

    #[test]
    fn v2_never_replays_without_binding_or_across_record_versions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut s = with_messages(1);
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let first = std::fs::read(journal_path(&dir, &s.id)).unwrap();
        push_message(&mut s, "v2 delta");
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let snapshot: Session =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{}.json", s.id))).unwrap())
                .unwrap();
        // A generic Session serde mirror drops only the storage metadata.
        std::fs::write(
            dir.join(format!("{}.json", s.id)),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        assert_saved(&dir, &snapshot); // never apply an unbound v2 delta
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let bound_snapshot = s.clone();
        let mut journal = std::fs::read(journal_path(&dir, &s.id)).unwrap();
        journal.extend_from_slice(
            b"{\"v\":1,\"k\":\"msg\",\"i\":2,\"m\":{\"content\":\"wrong version\"}}\n",
        );
        std::fs::write(journal_path(&dir, &s.id), &journal).unwrap();
        assert_saved(&dir, &bound_snapshot);
        assert_eq!(
            save_session_in_dir(&dir, &s, SessionPersistence::Journal)
                .unwrap()
                .mode,
            SaveMode::FullSnapshot
        );
        // Restoring an older *v2* open also invalidates the whole overlay.
        std::fs::write(journal_path(&dir, &s.id), first).unwrap();
        assert_saved(&dir, &s);
    }

    #[test]
    fn json_saves_keep_binding_even_after_journal_deletion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut s = with_messages(2);
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        push_message(&mut s, "old delta");
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let journal = std::fs::read(journal_path(&dir, &s.id)).unwrap();
        save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
        let generation = disk_generation(&dir, &s.id);
        s.api_messages.truncate(2);
        save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
        assert_ne!(generation, disk_generation(&dir, &s.id));
        std::fs::write(journal_path(&dir, &s.id), journal).unwrap();
        assert_saved(&dir, &s);
    }

    #[cfg(unix)]
    #[test]
    fn durable_error_after_rename_keeps_candidate_and_retry_is_safe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut s = with_messages(2);
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        push_message(&mut s, "old delta");
        push_message(&mut s, "more old history");
        save_session_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        let old_journal = std::fs::read(journal_path(&dir, &s.id)).unwrap();
        let old_generation = disk_generation(&dir, &s.id);
        s.api_messages.truncate(2);
        s.title = "durable candidate".into();
        // Snapshot fsync/rename/dir fsync succeeds; journal temp cleanup fails.
        let blocker = dir.join(format!("{}.journal.tmp", s.id));
        std::fs::create_dir(&blocker).unwrap();
        let error = save_session_durable_in_dir(&dir, &s, SessionPersistence::Journal).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert_ne!(old_generation, disk_generation(&dir, &s.id));
        assert_eq!(
            std::fs::read(journal_path(&dir, &s.id)).unwrap(),
            old_journal
        );
        assert_saved(&dir, &s); // Err did NOT roll back the published head
        std::fs::remove_dir(blocker).unwrap();
        save_session_durable_in_dir(&dir, &s, SessionPersistence::Journal).unwrap();
        assert_saved(&dir, &s);
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
        #[serial(umask)]
        fn durable_files_are_private_in_both_modes_and_repair_leaf_modes() {
            let _umask = UmaskGuard::set(0);
            for mode in [SessionPersistence::Json, SessionPersistence::Journal] {
                let tmp = tempfile::TempDir::new().unwrap();
                let dir = tmp
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("new/profile/sessions");
                let s = with_messages(2);
                save_session_durable_in_dir(&dir, &s, mode).unwrap();
                assert_eq!(mode_of(dir.parent().unwrap()), 0o700);
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
                let snapshot = dir.join(format!("{}.json", s.id));
                std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o666))
                    .unwrap();
                save_session_durable_in_dir(&dir, &s, mode).unwrap();
                assert_eq!(mode_of(&dir), 0o700);
                assert_eq!(mode_of(&snapshot), 0o600);
                if mode == SessionPersistence::Journal {
                    assert_eq!(mode_of(&journal_path(&dir, &s.id)), 0o600);
                }
                assert_saved(&dir, &s);
            }
        }

        #[test]
        fn durable_saves_refuse_artifact_and_ancestor_symlinks_in_both_modes() {
            for mode in [SessionPersistence::Json, SessionPersistence::Journal] {
                for extension in ["json", "journal"] {
                    let tmp = tempfile::TempDir::new().unwrap();
                    let root = tmp.path().canonicalize().unwrap();
                    let dir = root.join("sessions");
                    std::fs::create_dir(&dir).unwrap();
                    let victim = root.join("victim");
                    std::fs::write(&victim, "original").unwrap();
                    let s = with_messages(1);
                    std::os::unix::fs::symlink(
                        &victim,
                        dir.join(format!("{}.{}", s.id, extension)),
                    )
                    .unwrap();
                    assert!(save_session_durable_in_dir(&dir, &s, mode).is_err());
                    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
                }
                let tmp = tempfile::TempDir::new().unwrap();
                let root = tmp.path().canonicalize().unwrap();
                let victim_dir = root.join("victim-dir");
                std::fs::create_dir(&victim_dir).unwrap();
                let link = root.join("link");
                std::os::unix::fs::symlink(&victim_dir, &link).unwrap();
                let s = with_messages(1);
                for dir in [link.clone(), link.join("profile/sessions")] {
                    assert!(save_session_durable_in_dir(&dir, &s, mode).is_err());
                }
                assert_eq!(std::fs::read_dir(&victim_dir).unwrap().count(), 0);
            }
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
            // Preflight refuses the journal before publishing a snapshot.
            s.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role":"user","content":"x"}),
            ));
            let res = save_session_in_dir(&dir, &s, SessionPersistence::Journal);
            assert!(res.is_err(), "journal write onto a symlink must fail");
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
        }
    }
}
