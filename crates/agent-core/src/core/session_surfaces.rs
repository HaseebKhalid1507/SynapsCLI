//! Agent cards' durability (`agent-ui/architecture.md` v3.2, C2): the
//! sidecar `sessions/<id>.ui`, the store that writes it, and recovery.
//!
//! Inert: nothing here is wired to a session, an actor, the wire or a tool.
//! C4a wires it. What is here:
//!
//! - [`Sidecar`]: the format (`{"v":1, …}`), its bounds and its decoding.
//! - [`UiStore`]: one writer thread, a monotonic `seq`, one commit in
//!   flight, and [`write_durable`]'s outcome classes (§3.4): a commit is
//!   [`Outcome::Durable`] only once the file is renamed and its directory
//!   synced. A writer that died is read back.
//! - [`Images`]: the durable image, the in-flight commit and the later
//!   transitions; clients see only [`Images::projection`] (R3-H1).
//! - Identity (`session_id`, `lineage`, `base`) and [`recover`], a pure
//!   function from the sidecar and the loaded history to a [`Plan`]
//!   (§3.11).
//! - The compaction handoff as functions ([`prepare`], [`finish_in_place`],
//!   [`finish_successor`], [`moved`], [`rollback`]).
//! - Settling: [`SaveReceipts`] (per-save tickets) and the ledger ops.
//!
//! Every file operation goes through [`UiFs`]. [`ConfinedUiFs`] is the real
//! one (Unix: directory sync is what makes a rename durable); [`ModelFs`] is
//! the fault-injecting model the C2 exit gate runs on: a crash drops every
//! rename not followed by a directory sync, and failpoints fail, stall or
//! kill the writer on either side of the rename.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::ui_types::{
    is_host_id, is_session_id, Answer, AnswerState, AnswerStatus, Delivery, Downgrade,
    LedgerRecord, LogEntry, Phase, StopKind, SurfaceState, Tombstone, UiMark, Verb, WaitMode,
    ACTION_LOG_MAX, ATTACHED_MAX_STATES, LEDGER_MAX_RECORDS, LEDGER_MAX_VALUE_BYTES, MARK_KEY,
    SIDECAR_MAX_BYTES, SURFACE_STATE_MAX_BYTES, TOMBSTONES_MAX, TOMBSTONE_MAX_BYTES,
};

/// The sidecar format's version.
pub const SIDECAR_VERSION: u32 = 1;
/// A sidecar file over this is rejected unread (§4.4).
pub const SIDECAR_READ_MAX_BYTES: usize = 2 * 1024 * 1024;
/// `base` of a history no compaction has replaced.
pub const ROOT_BASE: &str = "root";
/// How long the actor waits for a commit's outcome before it answers
/// `pending` (§3.4).
pub const COMMIT_BUDGET: Duration = Duration::from_secs(2);
/// How often the writer retries a failed directory sync, and how far apart.
pub const SYNC_RETRIES: u32 = 3;
pub const SYNC_RETRY_GAP: Duration = Duration::from_secs(1);
/// An open card raised longer ago than this expires (§3.11 step 5).
pub const OPEN_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// A tombstone lasts this long, or until [`TOMBSTONES_MAX`] newer ones
/// push it out (§3.7).
pub const TOMBSTONE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Answered or skipped cards kept revisable (§4.4).
pub const RETAINED_MAX: usize = 8;
/// A `base`: `root` or a compaction's `source_range_digest`.
pub const BASE_MAX_BYTES: usize = 128;

/// `sessions/<id>.ui`.
pub fn sidecar_name(session_id: &str) -> String {
    format!("{session_id}.ui")
}

/// Where an unprovable prepared sidecar goes (§3.11 step 0): never served.
pub fn quarantine_name(session_id: &str) -> String {
    format!("{session_id}.ui.quarantine")
}

/// 128 random bits as 32 lowercase hex: a lineage or a handoff id.
pub fn mint_hex128() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub fn is_hex128(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_base(s: &str) -> bool {
    (1..=BASE_MAX_BYTES).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic())
}

/// `hex128(sha256(canonical payload))` of an action (§3.7, v3.2 #10): the
/// verb, `expect_rev`, `epoch` and values, so they are not stored again.
pub fn action_hash(verb: Verb, expect_rev: u32, epoch: u32, values: &Answer) -> String {
    let payload = serde_json::json!({
        "verb": verb,
        "expect_rev": expect_rev,
        "epoch": epoch,
        "values": values,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

// ── The format (§3.11) ───────────────────────────────────────────────────

/// A handoff's source: the parent's id, its base and the cut's `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffFrom {
    pub session_id: String,
    pub base: String,
    pub seq: u64,
}

/// A handoff's target: the same id in place, the successor's otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffTo {
    pub session_id: String,
    pub base: String,
}

/// A compaction handoff in progress on the parent (`compacting`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compacting {
    pub hx: String,
    pub from: HandoffFrom,
    pub to: HandoffTo,
}

/// A successor's sidecar not yet active (`prepared`, R3-H3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub hx: String,
    pub parent: String,
    pub from_seq: u64,
}

/// The parent's last word after a successor handoff finished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovedTo {
    pub to: String,
    pub hx: String,
}

/// A retained card and its action log. The log (`{action_id, hash, rev}`)
/// is sidecar-only: [`SurfaceState::actions`] is what clients see of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardRecord {
    pub state: SurfaceState,
    #[serde(default)]
    pub log: Vec<LogEntry>,
}

/// The whole UI image of one session: what one commit writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub v: u32,
    pub session_id: String,
    pub lineage: String,
    pub base: String,
    pub seq: u64,
    pub epoch: u32,
    /// Written by a successful reload checkpoint; the next commit clears it.
    #[serde(default)]
    pub checkpointed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting: Option<Compacting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared: Option<Prepared>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<MovedTo>,
    #[serde(default)]
    pub cards: Vec<CardRecord>,
    #[serde(default)]
    pub ledger: Vec<LedgerRecord>,
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    /// The newest settled record since `base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_settled: Option<String>,
}

impl Sidecar {
    /// An empty image: no cards, epoch 0, `seq` 0 (the store sets it).
    pub fn new(session_id: &str, lineage: &str, base: &str) -> Self {
        Self {
            v: SIDECAR_VERSION,
            session_id: session_id.to_string(),
            lineage: lineage.to_string(),
            base: base.to_string(),
            seq: 0,
            epoch: 0,
            checkpointed: false,
            compacting: None,
            prepared: None,
            moved_to: None,
            cards: Vec::new(),
            ledger: Vec::new(),
            tombstones: Vec::new(),
            newest_settled: None,
        }
    }

    pub fn card(&self, id: &str) -> Option<&SurfaceState> {
        self.cards.iter().map(|c| &c.state).find(|s| s.id == id)
    }

    pub fn record(&self, answer_id: &str) -> Option<&LedgerRecord> {
        self.ledger.iter().find(|r| r.answer_id == answer_id)
    }

    /// The durable action-log entry with this id, on a card or a tombstone.
    pub fn logged(&self, action_id: &str) -> Option<&LogEntry> {
        self.cards
            .iter()
            .flat_map(|c| c.log.iter())
            .chain(self.tombstones.iter().flat_map(|t| t.log.iter()))
            .find(|e| e.action_id == action_id)
    }

    /// Every ledger record is settled: a step that drops history may run
    /// (§3.11 "Destructive steps wait for both receipts").
    pub fn all_settled(&self) -> bool {
        self.ledger.iter().all(|r| r.delivery == Delivery::Settled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarError {
    /// Over [`SIDECAR_MAX_BYTES`] (or [`SIDECAR_READ_MAX_BYTES`], unread).
    TooLarge(usize),
    Malformed(String),
    Bounds(String),
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarError::TooLarge(n) => write!(f, "sidecar is {n} bytes, over its bound"),
            SidecarError::Malformed(e) => write!(f, "sidecar is malformed: {e}"),
            SidecarError::Bounds(e) => write!(f, "sidecar is out of bounds: {e}"),
        }
    }
}

impl std::error::Error for SidecarError {}

fn bounds(what: impl Into<String>) -> SidecarError {
    SidecarError::Bounds(what.into())
}

/// The structural bounds of §4.4 and v3.2 #11. Docs are not revalidated
/// here: the engine does that between decode and [`recover`] (v3.2 #12).
pub fn check_bounds(s: &Sidecar) -> Result<(), SidecarError> {
    if s.v != SIDECAR_VERSION {
        return Err(bounds(format!("version {}", s.v)));
    }
    if !is_session_id(&s.session_id) {
        return Err(bounds("session_id"));
    }
    if !is_hex128(&s.lineage) {
        return Err(bounds("lineage"));
    }
    if !is_base(&s.base) {
        return Err(bounds("base"));
    }
    if let Some(c) = &s.compacting {
        let ok = is_hex128(&c.hx)
            && is_session_id(&c.from.session_id)
            && is_session_id(&c.to.session_id)
            && is_base(&c.from.base)
            && is_base(&c.to.base);
        if !ok {
            return Err(bounds("compacting"));
        }
    }
    if let Some(p) = &s.prepared {
        if !is_hex128(&p.hx) || !is_session_id(&p.parent) {
            return Err(bounds("prepared"));
        }
    }
    if let Some(m) = &s.moved_to {
        if !is_hex128(&m.hx) || !is_session_id(&m.to) {
            return Err(bounds("moved_to"));
        }
    }
    if s.cards.len() > ATTACHED_MAX_STATES {
        return Err(bounds("cards"));
    }
    let mut ids = HashSet::new();
    for c in &s.cards {
        let st = &c.state;
        if !is_host_id(&st.id, "s_") || !ids.insert(st.id.as_str()) {
            return Err(bounds("card id"));
        }
        if st.actions.len() > ACTION_LOG_MAX || c.log.len() > ACTION_LOG_MAX {
            return Err(bounds("action log"));
        }
        let n = serde_json::to_vec(st).map_or(usize::MAX, |b| b.len());
        if n > SURFACE_STATE_MAX_BYTES {
            return Err(bounds("card size"));
        }
    }
    if s.ledger.len() > LEDGER_MAX_RECORDS {
        return Err(bounds("ledger"));
    }
    let mut answers = HashSet::new();
    let mut values = 0usize;
    for r in &s.ledger {
        if !is_host_id(&r.answer_id, "a_")
            || !is_host_id(&r.card, "s_")
            || !answers.insert(r.answer_id.as_str())
        {
            return Err(bounds("ledger record"));
        }
        values = values.saturating_add(r.values.to_json().len());
    }
    if values > LEDGER_MAX_VALUE_BYTES {
        return Err(bounds("ledger values"));
    }
    if s.tombstones.len() > TOMBSTONES_MAX {
        return Err(bounds("tombstones"));
    }
    for t in &s.tombstones {
        let n = serde_json::to_vec(t).map_or(usize::MAX, |b| b.len());
        if !is_host_id(&t.id, "s_") || t.log.len() > ACTION_LOG_MAX || n > TOMBSTONE_MAX_BYTES {
            return Err(bounds("tombstone"));
        }
    }
    if let Some(a) = &s.newest_settled {
        if !is_host_id(a, "a_") {
            return Err(bounds("newest_settled"));
        }
    }
    Ok(())
}

/// The bytes one commit writes: bounded, never truncated.
pub fn encode_sidecar(s: &Sidecar) -> Result<Vec<u8>, SidecarError> {
    check_bounds(s)?;
    let bytes = serde_json::to_vec(s).map_err(|e| SidecarError::Malformed(e.to_string()))?;
    if bytes.len() > SIDECAR_MAX_BYTES {
        return Err(SidecarError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Decode and bound-check a sidecar file's bytes.
pub fn decode_sidecar(bytes: &[u8]) -> Result<Sidecar, SidecarError> {
    if bytes.len() > SIDECAR_MAX_BYTES {
        return Err(SidecarError::TooLarge(bytes.len()));
    }
    let s: Sidecar =
        serde_json::from_slice(bytes).map_err(|e| SidecarError::Malformed(e.to_string()))?;
    check_bounds(&s)?;
    Ok(s)
}

/// The `seq` a file holds, read leniently (a writer's read-back, §3.4).
pub fn peek_seq(bytes: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(bytes)
        .ok()?
        .get("seq")?
        .as_u64()
}

// ── The filesystem seam ──────────────────────────────────────────────────

/// A failed atomic write, and whether its rename had already happened.
#[derive(Debug)]
pub struct WriteFailure {
    pub published: bool,
    pub error: io::Error,
}

/// What a read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadBack {
    Missing,
    /// Over the read bound: not read.
    TooLarge(u64),
    Bytes(Vec<u8>),
}

/// The file operations the store and the loader use, so a fault can be
/// injected (the C2 exit gate) and the real one stays confined.
pub trait UiFs: Send + Sync + 'static {
    /// Write `data` to a fsynced temp and rename it over `name`.
    fn write_atomic(&self, name: &str, data: &[u8]) -> Result<(), WriteFailure>;
    /// Sync the directory: a rename is durable only after this succeeds.
    fn sync_dir(&self) -> io::Result<()>;
    /// Read `name`, refusing anything over `max` bytes unread.
    fn read(&self, name: &str, max: usize) -> io::Result<ReadBack>;
    /// Unlink `name`; a missing file is not an error.
    fn remove(&self, name: &str) -> io::Result<()>;
    /// Rename `from` over `to` (a quarantine).
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
}

/// The sessions directory, held open: `ConfinedDir`'s atomic write (a
/// fsynced temp, `renameat`) and its directory sync.
#[cfg(unix)]
pub struct ConfinedUiFs {
    dir: super::private_fs::ConfinedDir,
}

#[cfg(unix)]
impl ConfinedUiFs {
    pub fn new(dir: super::private_fs::ConfinedDir) -> Self {
        Self { dir }
    }
}

#[cfg(unix)]
impl UiFs for ConfinedUiFs {
    fn write_atomic(&self, name: &str, data: &[u8]) -> Result<(), WriteFailure> {
        // `ConfinedDir::write_atomic` returns once `renameat` succeeded, and
        // every error it returns comes before (a failed `renameat` leaves the
        // old file), so none of them is published.
        self.dir
            .write_atomic(name, data)
            .map_err(|error| WriteFailure {
                published: false,
                error,
            })
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.dir.sync_all()
    }

    fn read(&self, name: &str, max: usize) -> io::Result<ReadBack> {
        use std::io::Read;
        let file = match self.dir.open_file(&[name.to_string()]) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ReadBack::Missing),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len > max as u64 {
            return Ok(ReadBack::TooLarge(len));
        }
        let mut bytes = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > max {
            return Ok(ReadBack::TooLarge(bytes.len() as u64));
        }
        Ok(ReadBack::Bytes(bytes))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match self.dir.remove_file(name) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        match self.read(from, SIDECAR_READ_MAX_BYTES)? {
            ReadBack::Bytes(b) => {
                self.dir.write_atomic(to, &b)?;
                self.remove(from)
            }
            ReadBack::TooLarge(_) => self.remove(from),
            ReadBack::Missing => Ok(()),
        }
    }
}

/// The C2 exit gate's filesystem model. Files are kept twice: what a
/// reader sees now, and what survives a crash (the state at the last
/// directory sync). A crash drops every rename and unlink not followed by
/// a directory sync, or keeps them all (the OS happened to flush). Every
/// failpoint fires once per count.
#[derive(Debug, Default)]
pub struct ModelFs {
    state: Mutex<ModelState>,
    gate: Condvar,
}

#[derive(Debug, Default)]
struct ModelState {
    visible: HashMap<String, Vec<u8>>,
    synced: HashMap<String, Vec<u8>>,
    fail_before_rename: u32,
    fail_after_rename: u32,
    fail_sync: u32,
    panic_before_rename: bool,
    panic_after_rename: bool,
    stalled: bool,
    renames: Vec<Rename>,
    sync_calls: u32,
}

/// One rename the model performed: the file, the `seq` it replaced and the
/// `seq` it wrote (read from the bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rename {
    pub name: String,
    pub before: Option<u64>,
    pub after: Option<u64>,
}

impl ModelFs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn st(&self) -> MutexGuard<'_, ModelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next `n` writes fail before their rename.
    pub fn fail_before_rename(&self, n: u32) {
        self.st().fail_before_rename = n;
    }

    /// The next `n` writes rename, then report a failure.
    pub fn fail_after_rename(&self, n: u32) {
        self.st().fail_after_rename = n;
    }

    /// The next `n` directory syncs fail.
    pub fn fail_sync(&self, n: u32) {
        self.st().fail_sync = n;
    }

    /// The next write panics the writer, before or after its rename.
    pub fn panic_next_write(&self, after_rename: bool) {
        let mut st = self.st();
        if after_rename {
            st.panic_after_rename = true;
        } else {
            st.panic_before_rename = true;
        }
    }

    /// Writes block (before their rename) until [`ModelFs::release`].
    pub fn stall(&self) {
        self.st().stalled = true;
    }

    pub fn release(&self) {
        self.st().stalled = false;
        self.gate.notify_all();
    }

    /// A process or OS crash: `keep_unsynced` keeps every rename since the
    /// last directory sync, else they are all lost. Failpoints reset.
    pub fn crash(&self, keep_unsynced: bool) {
        let mut st = self.st();
        if keep_unsynced {
            st.synced = st.visible.clone();
        } else {
            st.visible = st.synced.clone();
        }
        st.fail_before_rename = 0;
        st.fail_after_rename = 0;
        st.fail_sync = 0;
        st.panic_before_rename = false;
        st.panic_after_rename = false;
        st.stalled = false;
        drop(st);
        self.gate.notify_all();
    }

    /// A file as a reader sees it now.
    pub fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.st().visible.get(name).cloned()
    }

    /// Put a file in place, durably (a test's starting state).
    pub fn put(&self, name: &str, data: &[u8]) {
        let mut st = self.st();
        st.visible.insert(name.to_string(), data.to_vec());
        st.synced.insert(name.to_string(), data.to_vec());
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.st().visible.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn renames(&self) -> Vec<Rename> {
        self.st().renames.clone()
    }

    pub fn sync_calls(&self) -> u32 {
        self.st().sync_calls
    }
}

impl UiFs for ModelFs {
    fn write_atomic(&self, name: &str, data: &[u8]) -> Result<(), WriteFailure> {
        let mut st = self.st();
        while st.stalled {
            st = self.gate.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if st.panic_before_rename {
            st.panic_before_rename = false;
            drop(st);
            panic!("model fs: the writer dies before its rename");
        }
        if st.fail_before_rename > 0 {
            st.fail_before_rename -= 1;
            return Err(WriteFailure {
                published: false,
                error: io::Error::other("model fs: failure before the rename"),
            });
        }
        let before = st.visible.get(name).and_then(|b| peek_seq(b));
        st.renames.push(Rename {
            name: name.to_string(),
            before,
            after: peek_seq(data),
        });
        st.visible.insert(name.to_string(), data.to_vec());
        if st.panic_after_rename {
            st.panic_after_rename = false;
            drop(st);
            panic!("model fs: the writer dies after its rename");
        }
        if st.fail_after_rename > 0 {
            st.fail_after_rename -= 1;
            return Err(WriteFailure {
                published: true,
                error: io::Error::other("model fs: failure after the rename"),
            });
        }
        Ok(())
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut st = self.st();
        st.sync_calls += 1;
        if st.fail_sync > 0 {
            st.fail_sync -= 1;
            return Err(io::Error::other("model fs: directory sync failed"));
        }
        st.synced = st.visible.clone();
        Ok(())
    }

    fn read(&self, name: &str, max: usize) -> io::Result<ReadBack> {
        Ok(match self.st().visible.get(name) {
            None => ReadBack::Missing,
            Some(b) if b.len() > max => ReadBack::TooLarge(b.len() as u64),
            Some(b) => ReadBack::Bytes(b.clone()),
        })
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.st().visible.remove(name);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut st = self.st();
        if let Some(b) = st.visible.remove(from) {
            st.visible.insert(to.to_string(), b);
        }
        Ok(())
    }
}

// ── The publish boundary (§3.4) ──────────────────────────────────────────

/// A write's outcome class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Renamed, and the directory synced.
    Durable,
    /// Failed before the rename: the old file stands, and this image is
    /// never renamed.
    NotPublished(String),
    /// Renamed, but the directory sync failed (after the writer's retries,
    /// from the store: the store has failed).
    Unsynced(String),
}

/// `write_atomic` plus the directory sync, saying which side of the rename
/// a failure fell on.
pub fn write_durable<F: UiFs + ?Sized>(fs: &F, name: &str, data: &[u8]) -> Outcome {
    match fs.write_atomic(name, data) {
        Err(WriteFailure {
            published: false,
            error,
        }) => Outcome::NotPublished(error.to_string()),
        Err(WriteFailure {
            published: true,
            error,
        }) => Outcome::Unsynced(error.to_string()),
        Ok(()) => match fs.sync_dir() {
            Ok(()) => Outcome::Durable,
            Err(e) => Outcome::Unsynced(e.to_string()),
        },
    }
}

/// An unlink plus the directory sync.
pub fn remove_durable<F: UiFs + ?Sized>(fs: &F, name: &str) -> Outcome {
    match fs.remove(name) {
        Err(e) => Outcome::NotPublished(e.to_string()),
        Ok(()) => match fs.sync_dir() {
            Ok(()) => Outcome::Durable,
            Err(e) => Outcome::Unsynced(e.to_string()),
        },
    }
}

// ── The store (§3.4) ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreConfig {
    /// How long a commit may take before the actor answers `pending`.
    pub budget: Duration,
    pub sync_retries: u32,
    pub sync_gap: Duration,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            budget: COMMIT_BUDGET,
            sync_retries: SYNC_RETRIES,
            sync_gap: SYNC_RETRY_GAP,
        }
    }
}

/// Why the store takes nothing now (§3.3 #5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A commit is in flight (or a handoff is fenced): retry.
    Busy,
    /// The store has failed closed.
    Storage(String),
}

/// A commit's progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Nothing is in flight.
    Idle,
    /// No outcome yet: the writer is still running.
    Pending,
    Done(Outcome),
}

enum Job {
    Write {
        seq: u64,
        name: String,
        bytes: Vec<u8>,
    },
    Remove {
        seq: u64,
        name: String,
    },
    Sync {
        seq: u64,
    },
}

struct Writer {
    jobs: Sender<Job>,
    outcomes: Receiver<(u64, Outcome)>,
}

fn retry_sync<F: UiFs>(fs: &F, outcome: Outcome, cfg: &StoreConfig) -> Outcome {
    let Outcome::Unsynced(mut err) = outcome else {
        return outcome;
    };
    for _ in 0..cfg.sync_retries {
        std::thread::sleep(cfg.sync_gap);
        match fs.sync_dir() {
            Ok(()) => return Outcome::Durable,
            Err(e) => err = e.to_string(),
        }
    }
    Outcome::Unsynced(err)
}

/// The writer: jobs in order, one at a time. It never renames an image
/// over a file holding the same or a newer `seq` (`floor`).
fn spawn_writer<F: UiFs>(
    fs: Arc<F>,
    cfg: StoreConfig,
    mut floor: HashMap<String, u64>,
) -> io::Result<Writer> {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (report, outcomes) = mpsc::channel::<(u64, Outcome)>();
    std::thread::Builder::new()
        .name("ui-store".into())
        .spawn(move || {
            for job in inbox {
                let (seq, outcome) = match job {
                    Job::Write { seq, name, bytes } => {
                        if floor.get(&name).is_some_and(|f| *f >= seq) {
                            let why = format!("seq {seq} is not newer than {name}'s");
                            (seq, Outcome::NotPublished(why))
                        } else {
                            let o = write_durable(&*fs, &name, &bytes);
                            if !matches!(o, Outcome::NotPublished(_)) {
                                floor.insert(name, seq);
                            }
                            (seq, retry_sync(&*fs, o, &cfg))
                        }
                    }
                    Job::Remove { seq, name } => {
                        let o = remove_durable(&*fs, &name);
                        if !matches!(o, Outcome::NotPublished(_)) {
                            floor.remove(&name);
                        }
                        (seq, retry_sync(&*fs, o, &cfg))
                    }
                    Job::Sync { seq } => {
                        let o = match fs.sync_dir() {
                            Ok(()) => Outcome::Durable,
                            Err(e) => Outcome::Unsynced(e.to_string()),
                        };
                        (seq, retry_sync(&*fs, o, &cfg))
                    }
                };
                if report.send((seq, outcome)).is_err() {
                    break;
                }
            }
        })?;
    Ok(Writer { jobs, outcomes })
}

#[derive(Clone, Debug)]
struct InFlight {
    seq: u64,
    name: String,
    remove: bool,
}

/// One session's sidecar writer: one commit in flight, `seq` order, and
/// the outcome classes of §3.4. Its commits may name another file (a
/// successor's, during a handoff), so one writer orders both.
pub struct UiStore<F: UiFs> {
    fs: Arc<F>,
    session_id: String,
    cfg: StoreConfig,
    last_seq: u64,
    inflight: Option<InFlight>,
    failed: Option<String>,
    writer: Writer,
}

impl<F: UiFs> UiStore<F> {
    /// The store for `session_id`; `seq` continues after the file's.
    pub fn open(fs: Arc<F>, session_id: &str, cfg: StoreConfig) -> io::Result<Self> {
        let name = sidecar_name(session_id);
        let last_seq = match fs.read(&name, SIDECAR_READ_MAX_BYTES)? {
            ReadBack::Bytes(b) => peek_seq(&b).unwrap_or(0),
            _ => 0,
        };
        let floor = HashMap::from([(name, last_seq)]);
        let writer = spawn_writer(fs.clone(), cfg, floor)?;
        Ok(Self {
            fs,
            session_id: session_id.to_string(),
            cfg,
            last_seq,
            inflight: None,
            failed: None,
            writer,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The store now speaks for `session_id` (a successor after Finish).
    pub fn rebind(&mut self, session_id: &str) {
        self.session_id = session_id.to_string();
    }

    pub fn config(&self) -> StoreConfig {
        self.cfg
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn in_flight(&self) -> Option<u64> {
        self.inflight.as_ref().map(|f| f.seq)
    }

    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// §3.3 #5, the store's half: one commit in flight, a failed store.
    pub fn admit(&self) -> Result<(), Refusal> {
        if let Some(e) = &self.failed {
            return Err(Refusal::Storage(e.clone()));
        }
        if self.inflight.is_some() {
            return Err(Refusal::Busy);
        }
        Ok(())
    }

    fn next_seq(&mut self) -> Result<u64, Refusal> {
        self.admit()?;
        let seq = self
            .last_seq
            .checked_add(1)
            .ok_or_else(|| Refusal::Storage("seq overflow".into()))?;
        Ok(seq)
    }

    fn send(&mut self, job: Job, flight: InFlight) -> Result<u64, Refusal> {
        let seq = flight.seq;
        if let Err(mpsc::SendError(job)) = self.writer.jobs.send(job) {
            // The writer died between commits: a fresh one, which never
            // writes below what is on disk.
            self.respawn(&flight.name)?;
            self.writer
                .jobs
                .send(job)
                .map_err(|_| Refusal::Storage("the writer is gone".into()))?;
        }
        self.last_seq = seq;
        self.inflight = Some(flight);
        Ok(seq)
    }

    fn respawn(&mut self, name: &str) -> Result<Option<u64>, Refusal> {
        let on_disk = match self.fs.read(name, SIDECAR_READ_MAX_BYTES) {
            Ok(ReadBack::Bytes(b)) => peek_seq(&b),
            _ => None,
        };
        let floor = on_disk
            .map(|s| HashMap::from([(name.to_string(), s)]))
            .unwrap_or_default();
        match spawn_writer(self.fs.clone(), self.cfg, floor) {
            Ok(w) => {
                self.writer = w;
                Ok(on_disk)
            }
            Err(e) => {
                let e = format!("couldn't start the writer: {e}");
                self.failed = Some(e.clone());
                Err(Refusal::Storage(e))
            }
        }
    }

    /// Issue a commit of `image` (its `seq` is set here), to the file its
    /// `session_id` names. One at a time.
    pub fn commit(&mut self, image: &mut Sidecar) -> Result<u64, Refusal> {
        let seq = self.next_seq()?;
        image.seq = seq;
        let bytes = encode_sidecar(image).map_err(|e| Refusal::Storage(e.to_string()))?;
        let name = sidecar_name(&image.session_id);
        let flight = InFlight {
            seq,
            name: name.clone(),
            remove: false,
        };
        self.send(Job::Write { seq, name, bytes }, flight)
    }

    /// Issue the removal of `<session_id>.ui`, then a directory sync, as a
    /// commit (the `/new` fallback, a successor's rollback).
    pub fn remove(&mut self, session_id: &str) -> Result<u64, Refusal> {
        let seq = self.next_seq()?;
        let name = sidecar_name(session_id);
        let flight = InFlight {
            seq,
            name: name.clone(),
            remove: true,
        };
        self.send(Job::Remove { seq, name }, flight)
    }

    /// Wait up to `timeout` for the in-flight commit's outcome. A writer
    /// that died is read back (§3.4): the file holds the in-flight `seq`,
    /// so it is synced from a fresh writer; it doesn't, so it was never
    /// published.
    pub fn poll(&mut self, timeout: Duration) -> Poll {
        let Some(flight) = self.inflight.clone() else {
            return Poll::Idle;
        };
        loop {
            match self.writer.outcomes.recv_timeout(timeout) {
                Ok((seq, outcome)) if seq == flight.seq => {
                    if let Outcome::Unsynced(e) = &outcome {
                        self.failed = Some(e.clone());
                    }
                    self.inflight = None;
                    return Poll::Done(outcome);
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => return Poll::Pending,
                Err(RecvTimeoutError::Disconnected) => return self.after_writer_death(&flight),
            }
        }
    }

    /// Join the writer (a checkpoint, `/new`, a Prepare): the in-flight
    /// outcome, or `Pending` if it is still unknown at `budget`.
    pub fn join(&mut self, budget: Duration) -> Poll {
        self.poll(budget)
    }

    fn after_writer_death(&mut self, flight: &InFlight) -> Poll {
        let on_disk = match self.respawn(&flight.name) {
            Ok(s) => s,
            Err(Refusal::Storage(e)) => {
                self.inflight = None;
                return Poll::Done(Outcome::Unsynced(e));
            }
            Err(Refusal::Busy) => None,
        };
        let published = if flight.remove {
            on_disk.is_none()
        } else {
            on_disk == Some(flight.seq)
        };
        if !published {
            self.inflight = None;
            return Poll::Done(Outcome::NotPublished(
                "the writer died before publishing".into(),
            ));
        }
        if self
            .writer
            .jobs
            .send(Job::Sync { seq: flight.seq })
            .is_err()
        {
            self.inflight = None;
            self.failed = Some("the writer is gone".into());
            return Poll::Done(Outcome::Unsynced("the writer is gone".into()));
        }
        self.poll(self.cfg.budget)
    }

    /// Read back and decode `<session_id>.ui` (a loader's read).
    pub fn read(&self, session_id: &str) -> io::Result<Loaded> {
        load(&*self.fs, session_id)
    }
}

/// A sidecar as the loader found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    Missing,
    /// It fails bounds or schema (a class, not an I/O failure).
    Unreadable(String),
    Image(Box<Sidecar>),
}

/// Read `<session_id>.ui`. An I/O error is `Err`: the store then fails
/// closed (§3.4), unlike a file that fails bounds or schema.
pub fn load<F: UiFs + ?Sized>(fs: &F, session_id: &str) -> io::Result<Loaded> {
    Ok(
        match fs.read(&sidecar_name(session_id), SIDECAR_READ_MAX_BYTES)? {
            ReadBack::Missing => Loaded::Missing,
            ReadBack::TooLarge(n) => {
                Loaded::Unreadable(SidecarError::TooLarge(n as usize).to_string())
            }
            ReadBack::Bytes(b) => match decode_sidecar(&b) {
                Ok(s) => Loaded::Image(Box::new(s)),
                Err(e) => Loaded::Unreadable(e.to_string()),
            },
        },
    )
}

// ── Transitions and the three images (§3.2, §3.4, R3-H1) ─────────────────

/// An accepted answer, skip or revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub record: LedgerRecord,
    pub action: LogEntry,
    /// The roster label for other clients' receipts.
    pub by: String,
    pub at: String,
    /// What the card offers after it (the actor decides: C4c adds revise).
    pub offers: Vec<Verb>,
}

/// One change to the UI image. A commit carries at most one operation
/// that can be refused ([`Op::Create`], [`Op::Accept`], [`Op::Close`],
/// [`Op::Clear`]); the rest are transitions that ride the next commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Create(Box<SurfaceState>),
    Accept(Box<Accepted>),
    /// The agent's `close`: an open card is withdrawn, a receipt closed.
    Close {
        card: String,
    },
    /// `/new`: no card, answer or tombstone survives it.
    Clear,
    /// T15/T16 (and T17, T21): the epoch rises.
    Stop(StopKind),
    /// T13/T14: every waiting card goes live.
    Downgrade(Downgrade),
    Expire {
        card: String,
    },
    /// Delivery progress of one record.
    Deliver {
        answer_id: String,
        to: Delivery,
    },
    /// T22: the record is settled; `newest_settled` moves to it.
    Settle {
        answer_id: String,
    },
    /// A settled record leaves the ledger.
    Release {
        answer_id: String,
    },
    /// The reload barrier's image (§3.10).
    Checkpoint,
    /// A compaction handoff's state on this image (§3.11).
    Handoff(Box<HandoffState>),
}

/// The handoff fields one commit sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffState {
    pub base: String,
    pub compacting: Option<Compacting>,
    pub prepared: Option<Prepared>,
    pub moved_to: Option<MovedTo>,
    pub newest_settled: Option<String>,
}

impl Op {
    /// Recovery reproduces it from the previous durable image, so a client
    /// may see it before it is durable (§3.11's write rule).
    pub fn recovery_equivalent(&self) -> bool {
        matches!(self, Op::Downgrade(_))
    }
}

fn sync_view(sc: &mut Sidecar, answer_id: &str) {
    let Some(r) = sc.ledger.iter().find(|r| r.answer_id == answer_id).cloned() else {
        return;
    };
    for c in &mut sc.cards {
        if let Some(a) = c.state.answer.as_mut().filter(|a| a.answer_id == answer_id) {
            a.delivery = r.delivery;
            a.wake = r.wake;
        }
    }
}

/// Apply one operation to an image.
pub fn apply(sc: &mut Sidecar, op: &Op) {
    match op {
        Op::Create(s) => {
            if sc.card(&s.id).is_none() {
                sc.cards.push(CardRecord {
                    state: (**s).clone(),
                    log: Vec::new(),
                });
            }
        }
        Op::Accept(a) => {
            let r = &a.record;
            let Some(c) = sc.cards.iter_mut().find(|c| c.state.id == r.card) else {
                return;
            };
            c.state.rev = c.state.rev.saturating_add(1);
            c.state.phase = match r.status {
                AnswerStatus::Skipped => Phase::Skipped,
                AnswerStatus::Answered | AnswerStatus::Revised => Phase::Answered,
            };
            c.state.mode = WaitMode::Live;
            c.state.offers.clone_from(&a.offers);
            c.state.actions.push(a.action.action_id.clone());
            c.log.push(a.action.clone());
            c.state.answer = Some(AnswerState {
                rev: r.rev,
                action_id: a.action.action_id.clone(),
                answer_id: r.answer_id.clone(),
                status: r.status,
                values: r.values.clone(),
                delivery: r.delivery,
                wake: r.wake,
                by: a.by.clone(),
                at: a.at.clone(),
            });
            sc.ledger.push(r.clone());
        }
        Op::Close { card } => {
            if let Some(c) = sc.cards.iter_mut().find(|c| &c.state.id == card) {
                c.state.rev = c.state.rev.saturating_add(1);
                c.state.phase = if c.state.phase == Phase::Open {
                    Phase::Withdrawn
                } else {
                    Phase::Closed
                };
                c.state.mode = WaitMode::Live;
                c.state.offers.clear();
            }
        }
        Op::Clear => {
            sc.cards.clear();
            sc.ledger.clear();
            sc.tombstones.clear();
            sc.newest_settled = None;
            ROOT_BASE.clone_into(&mut sc.base);
        }
        Op::Stop(kind) => {
            sc.epoch = sc.epoch.saturating_add(1);
            let epoch = sc.epoch;
            for c in &mut sc.cards {
                let st = &mut c.state;
                st.epoch = epoch;
                st.rev = st.rev.saturating_add(1);
                let waiting = st.mode == WaitMode::Waiting;
                match kind {
                    StopKind::User | StopKind::System if waiting => {
                        st.phase = Phase::Expired;
                        st.mode = WaitMode::Live;
                        st.offers.clear();
                    }
                    StopKind::System => {
                        // Open cards expire; receipts are final (no revise).
                        if st.phase == Phase::Open {
                            st.phase = Phase::Expired;
                        }
                        st.offers.clear();
                    }
                    StopKind::Reload | StopKind::Crash if waiting => {
                        st.mode = WaitMode::Live;
                        st.downgraded = Some(if *kind == StopKind::Reload {
                            Downgrade::Reload
                        } else {
                            Downgrade::Crash
                        });
                        st.resume = true;
                    }
                    _ => {
                        if st.phase == Phase::Open || st.offers.contains(&Verb::Revise) {
                            st.resume = true;
                        }
                    }
                }
                if st.phase == Phase::Open {
                    st.origin.stopped = Some(*kind);
                }
            }
        }
        Op::Downgrade(d) => {
            for c in &mut sc.cards {
                if c.state.mode == WaitMode::Waiting {
                    c.state.mode = WaitMode::Live;
                    c.state.downgraded = Some(*d);
                    c.state.rev = c.state.rev.saturating_add(1);
                }
            }
        }
        Op::Expire { card } => {
            if let Some(c) = sc.cards.iter_mut().find(|c| &c.state.id == card) {
                if c.state.phase == Phase::Open {
                    c.state.phase = Phase::Expired;
                    c.state.mode = WaitMode::Live;
                    c.state.offers.clear();
                    c.state.rev = c.state.rev.saturating_add(1);
                }
            }
        }
        Op::Deliver { answer_id, to } => {
            if let Some(r) = sc.ledger.iter_mut().find(|r| &r.answer_id == answer_id) {
                if r.delivery != Delivery::Settled {
                    r.delivery = *to;
                }
            }
            sync_view(sc, answer_id);
        }
        Op::Settle { answer_id } => {
            if let Some(r) = sc.ledger.iter_mut().find(|r| &r.answer_id == answer_id) {
                r.delivery = Delivery::Settled;
                sc.newest_settled = Some(answer_id.clone());
            }
            sync_view(sc, answer_id);
        }
        Op::Release { answer_id } => {
            sc.ledger
                .retain(|r| &r.answer_id != answer_id || r.delivery != Delivery::Settled);
        }
        Op::Checkpoint => sc.checkpointed = true,
        Op::Handoff(h) => {
            sc.base.clone_from(&h.base);
            sc.compacting.clone_from(&h.compacting);
            sc.prepared.clone_from(&h.prepared);
            sc.moved_to.clone_from(&h.moved_to);
            sc.newest_settled.clone_from(&h.newest_settled);
        }
    }
}

struct InFlightCommit {
    op: Option<Op>,
    riding: Vec<Op>,
    image: Sidecar,
}

/// The actor's three images (§3.4, R3-H1): the durable image, the
/// in-flight commit (its frozen image, its one operation and the
/// transitions riding it) and the transitions noted since.
pub struct Images {
    durable: Sidecar,
    inflight: Option<InFlightCommit>,
    later: Vec<Op>,
}

/// §3.3 #4–#10 for one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRequest {
    pub card: String,
    pub action_id: String,
    pub hash: String,
    pub verb: Verb,
    pub contract: String,
    pub epoch: u32,
    pub expect_rev: u32,
}

/// What admission says, when it doesn't admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// #4: this action is durably logged with this payload: its original
    /// outcome, no effect.
    Duplicate {
        rev: u32,
    },
    /// #4: it is the in-flight action: the final result follows.
    Pending,
    /// #4: the id is logged with another payload.
    Reused,
    Busy,
    Storage(String),
    Resolved,
    Gone,
    Stale,
    Stopped,
    Conflict,
}

/// A lifecycle operation's report (create, close, `/new`, `/ui off`):
/// only `NotPublished` is a definitive failure; an outcome still unknown
/// is unconfirmed, never a refusal (§3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Done,
    Failed(String),
    Unconfirmed,
}

impl Lifecycle {
    pub fn of(poll: &Poll) -> Self {
        match poll {
            Poll::Idle | Poll::Done(Outcome::Durable) => Lifecycle::Done,
            Poll::Done(Outcome::NotPublished(e)) => Lifecycle::Failed(e.clone()),
            Poll::Pending | Poll::Done(Outcome::Unsynced(_)) => Lifecycle::Unconfirmed,
        }
    }
}

impl Images {
    pub fn new(durable: Sidecar) -> Self {
        Self {
            durable,
            inflight: None,
            later: Vec::new(),
        }
    }

    pub fn durable(&self) -> &Sidecar {
        &self.durable
    }

    /// The operation of the in-flight commit, if any.
    pub fn pending_op(&self) -> Option<&Op> {
        self.inflight.as_ref().and_then(|f| f.op.as_ref())
    }

    pub fn in_flight(&self) -> bool {
        self.inflight.is_some()
    }

    /// Something is in memory that isn't durable yet.
    pub fn has_uncommitted(&self) -> bool {
        self.inflight.is_some() || !self.later.is_empty()
    }

    /// Admission and routing decide on this: durable, then in flight, then
    /// later, so a stop during a commit is never missed.
    pub fn live(&self) -> Sidecar {
        let mut s = match &self.inflight {
            Some(f) => f.image.clone(),
            None => self.durable.clone(),
        };
        for op in &self.later {
            apply(&mut s, op);
        }
        s
    }

    /// What clients and the model see: the durable image plus the
    /// recovery-equivalent transitions. Never an in-flight operation.
    pub fn projection(&self) -> Sidecar {
        let mut s = self.durable.clone();
        let riding = self.inflight.iter().flat_map(|f| f.riding.iter());
        for op in riding.chain(&self.later) {
            if op.recovery_equivalent() {
                apply(&mut s, op);
            }
        }
        s
    }

    /// A transition that can't be refused: memory at once, the next
    /// commit carries it.
    pub fn note(&mut self, op: Op) {
        self.later.push(op);
    }

    /// Issue the next commit: everything noted since the last one, then
    /// `op`. Refused by a failed store, and while one is in flight.
    pub fn commit<F: UiFs>(
        &mut self,
        store: &mut UiStore<F>,
        op: Option<Op>,
    ) -> Result<u64, Refusal> {
        store.admit()?;
        if self.inflight.is_some() {
            return Err(Refusal::Busy);
        }
        let riding = std::mem::take(&mut self.later);
        let mut image = self.durable.clone();
        image.checkpointed = false;
        for o in riding.iter().chain(op.iter()) {
            apply(&mut image, o);
        }
        match store.commit(&mut image) {
            Ok(seq) => {
                self.inflight = Some(InFlightCommit { op, riding, image });
                Ok(seq)
            }
            Err(r) => {
                self.later = riding;
                Err(r)
            }
        }
    }

    /// The in-flight commit's outcome. `NotPublished` removes only its own
    /// operation: the transitions it carried are kept and commit next.
    /// `Unsynced` from the store means it is published but the store has
    /// failed: the change is kept, still pending.
    pub fn resolve(&mut self, outcome: &Outcome) {
        let Some(f) = self.inflight.take() else {
            return;
        };
        match outcome {
            Outcome::Durable => self.durable = f.image,
            Outcome::NotPublished(_) => {
                let mut later = f.riding;
                later.append(&mut self.later);
                self.later = later;
            }
            Outcome::Unsynced(_) => self.inflight = Some(f),
        }
    }

    /// Poll the store and resolve what it says.
    pub fn poll<F: UiFs>(&mut self, store: &mut UiStore<F>, timeout: Duration) -> Poll {
        let p = store.poll(timeout);
        if let Poll::Done(o) = &p {
            self.resolve(o);
        }
        p
    }

    /// §3.3 #4–#10, in order.
    pub fn admit<F: UiFs>(&self, store: &UiStore<F>, req: &ActionRequest) -> Result<(), Admission> {
        // #4: the durable log first, then the in-flight action.
        if let Some(e) = self.durable.logged(&req.action_id) {
            return Err(if e.hash == req.hash {
                Admission::Duplicate { rev: e.rev }
            } else {
                Admission::Reused
            });
        }
        if let Some(Op::Accept(a)) = self.pending_op() {
            if a.action.action_id == req.action_id {
                return Err(if a.action.hash == req.hash {
                    Admission::Pending
                } else {
                    Admission::Busy
                });
            }
        }
        // #5: the store, and a handoff's fence.
        match store.admit() {
            Err(Refusal::Storage(e)) => return Err(Admission::Storage(e)),
            Err(Refusal::Busy) => return Err(Admission::Busy),
            Ok(()) => {}
        }
        let live = self.live();
        if self.inflight.is_some() || live.compacting.is_some() {
            return Err(Admission::Busy);
        }
        // #6–#10 on the live state.
        let Some(card) = live.card(&req.card) else {
            return Err(if live.tombstones.iter().any(|t| t.id == req.card) {
                Admission::Resolved
            } else {
                Admission::Gone
            });
        };
        if !card.offers.contains(&req.verb) {
            return Err(Admission::Resolved);
        }
        if card.contract != req.contract {
            return Err(Admission::Stale);
        }
        if req.epoch != live.epoch {
            return Err(Admission::Stopped);
        }
        let rev = card.answer.as_ref().map_or(0, |a| a.rev);
        if req.expect_rev != rev {
            return Err(Admission::Conflict);
        }
        Ok(())
    }
}

// ── Identity and recovery (§3.11) ────────────────────────────────────────

/// What the loaded history says, for recovery. Built from the session's
/// messages and record; the journal generation and the save mode play no
/// part, so an ordinary save never makes a good sidecar look suspect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryFacts {
    /// The file stem.
    pub session_id: String,
    /// The compaction the history starts from, or [`ROOT_BASE`].
    pub base: String,
    pub compacted_into: Option<String>,
    /// Every well-formed `_synaps_ui` mark, in order.
    pub marks: Vec<UiMark>,
}

impl HistoryFacts {
    /// From in-memory messages, whose marks sit under [`MARK_KEY`].
    pub fn from_messages<'a>(
        session_id: &str,
        base: Option<&str>,
        compacted_into: Option<&str>,
        messages: impl IntoIterator<Item = &'a Value>,
    ) -> Self {
        let marks = messages
            .into_iter()
            .filter_map(|m| m.get(MARK_KEY))
            .filter_map(|v| serde_json::from_value::<UiMark>(v.clone()).ok())
            .filter(UiMark::is_valid)
            .collect();
        Self {
            session_id: session_id.to_string(),
            base: base.unwrap_or(ROOT_BASE).to_string(),
            compacted_into: compacted_into.map(str::to_string),
            marks,
        }
    }

    /// A history mark names this answer: its `<ui_answer>` message, or a
    /// `ui` result that delivered it.
    pub fn has_answer(&self, answer_id: &str) -> bool {
        self.marks.iter().any(|m| match m {
            UiMark::Answer(a) => a.answer == answer_id,
            UiMark::Results(r) => r
                .results
                .iter()
                .any(|x| x.answer.as_deref() == Some(answer_id)),
        })
    }
}

/// How a sidecar relates to its history (§3.11 step 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Related,
    Restored,
    Foreign,
    Missing,
    /// The history was compacted into a successor (`compacted_into`): the
    /// sidecar is ignored and serves no card.
    Moved,
}

/// The parent's files, for a prepared sidecar (step 0), read without
/// writing either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParentFacts {
    Unreadable,
    Read {
        compacted_into: Option<String>,
        sidecar: Option<Box<Sidecar>>,
    },
}

/// A file change recovery asks for, before its commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileAction {
    /// Unlink this sidecar.
    Remove(String),
    /// Unlink a successor's sidecar if it is still prepared with `hx`.
    RemoveIfPrepared { session_id: String, hx: String },
    /// Never served again.
    Quarantine { from: String, to: String },
}

/// What recovery does (T21). It never starts a turn (decision 11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub class: Class,
    /// Step 0 found the parent's link: this successor is now active.
    pub activated: bool,
    /// File changes first, in order.
    pub files: Vec<FileAction>,
    /// One commit, Durable, before the first attach snapshot.
    pub commit: Option<Sidecar>,
    /// Records the history lacks: back into context once, in ledger order,
    /// after any interruption marker, with `wake` false.
    pub deliver: Vec<String>,
    /// Records the history holds: `Recorded`, settled by a later commit.
    pub recorded: Vec<String>,
}

impl Plan {
    fn new(class: Class) -> Self {
        Self {
            class,
            activated: false,
            files: Vec::new(),
            commit: None,
            deliver: Vec::new(),
            recorded: Vec::new(),
        }
    }

    /// Recovery never starts a turn, whatever `wake` says.
    pub fn starts_turn(&self) -> bool {
        false
    }

    /// This sidecar is the session's card authority: its open cards are
    /// answerable and its ledger delivers.
    pub fn is_authority(&self) -> bool {
        self.class == Class::Related && self.commit.is_some()
    }
}

fn names_handoff(parent: &Sidecar, hx: &str, to: &str) -> bool {
    parent
        .compacting
        .as_ref()
        .is_some_and(|c| c.hx == hx && c.to.session_id == to)
        || parent
            .moved_to
            .as_ref()
            .is_some_and(|m| m.hx == hx && m.to == to)
}

fn older_than(at: &str, now: DateTime<Utc>, age: Duration) -> bool {
    let Ok(t) = DateTime::parse_from_rfc3339(at) else {
        return false;
    };
    let age = chrono::Duration::from_std(age).unwrap_or(chrono::Duration::MAX);
    now.signed_duration_since(t.with_timezone(&Utc)) > age
}

/// Re-apply the limits (T20, §3.11 step 5): open cards past 24 h expire;
/// past [`RETAINED_MAX`] closed or answered cards, the oldest become
/// tombstones; tombstones past 7 days or [`TOMBSTONES_MAX`] go.
pub fn enforce_limits(sc: &mut Sidecar, now: DateTime<Utc>) {
    for c in &mut sc.cards {
        let st = &mut c.state;
        if st.phase == Phase::Open && older_than(&st.raised_at, now, OPEN_MAX_AGE) {
            st.phase = Phase::Expired;
            st.mode = WaitMode::Live;
            st.offers.clear();
            st.rev = st.rev.saturating_add(1);
        }
    }
    let unsettled: HashSet<String> = sc
        .ledger
        .iter()
        .filter(|r| r.delivery != Delivery::Settled)
        .map(|r| r.card.clone())
        .collect();
    let at = now.to_rfc3339();
    while sc
        .cards
        .iter()
        .filter(|c| c.state.phase != Phase::Open)
        .count()
        > RETAINED_MAX
    {
        let Some(i) = sc
            .cards
            .iter()
            .position(|c| c.state.phase != Phase::Open && !unsettled.contains(&c.state.id))
        else {
            break;
        };
        let c = sc.cards.remove(i);
        sc.tombstones.push(Tombstone {
            id: c.state.id,
            at: at.clone(),
            log: c.log,
        });
    }
    sc.tombstones
        .retain(|t| !older_than(&t.at, now, TOMBSTONE_MAX_AGE));
    let over = sc.tombstones.len().saturating_sub(TOMBSTONES_MAX);
    sc.tombstones = sc.tombstones.split_off(over);
}

/// A fresh sidecar for a Restored, Foreign or Missing history (step 3):
/// nothing is delivered; open cards and cards with unsettled records
/// become terminal `Unknown` entries (Copy to composer). It takes the
/// lineage of the history's marks (or a new one) and the history's base,
/// so the next load is Related. Nothing else of the old sidecar is kept.
fn fresh(history: &HistoryFacts, old: Option<&Sidecar>) -> Sidecar {
    let lineage = history
        .marks
        .first()
        .map(|m| m.lineage().to_string())
        .filter(|l| is_hex128(l))
        .unwrap_or_else(mint_hex128);
    let mut sc = Sidecar::new(&history.session_id, &lineage, &history.base);
    let Some(old) = old else {
        return sc;
    };
    sc.epoch = old.epoch;
    sc.seq = old.seq;
    let unsettled: HashSet<&str> = old
        .ledger
        .iter()
        .filter(|r| r.delivery != Delivery::Settled)
        .map(|r| r.card.as_str())
        .collect();
    for c in &old.cards {
        if c.state.phase != Phase::Open && !unsettled.contains(c.state.id.as_str()) {
            continue;
        }
        let mut st = c.state.clone();
        st.phase = Phase::Unknown;
        st.mode = WaitMode::Live;
        st.offers.clear();
        st.resume = false;
        st.rev = st.rev.saturating_add(1);
        if let Some(a) = st.answer.as_mut() {
            if unsettled.contains(st.id.as_str()) {
                a.delivery = Delivery::Unknown;
            }
            a.wake = false;
        }
        sc.cards.push(CardRecord {
            state: st,
            log: c.log.clone(),
        });
    }
    sc
}

/// Recovery at load (T21, §3.11), as a pure function of the sidecar, the
/// loaded history, the parent's files (read only for a prepared sidecar)
/// and the time. Its plan is applied in order: `files`, then `commit`
/// (Durable) before the first attach snapshot, then `deliver`.
pub fn recover(
    loaded: &Loaded,
    history: &HistoryFacts,
    parent: Option<&ParentFacts>,
    now: DateTime<Utc>,
) -> Plan {
    let stem = history.session_id.as_str();
    let mut sc = match loaded {
        Loaded::Image(s) => (**s).clone(),
        Loaded::Missing | Loaded::Unreadable(_) => {
            let mut plan = Plan::new(Class::Missing);
            if !history.marks.is_empty() {
                plan.commit = Some(fresh(history, None));
            }
            return plan;
        }
    };
    let mut plan = Plan::new(Class::Related);

    // A parent with `compacted_into` ignores its sidecar.
    if history.compacted_into.is_some() {
        plan.class = Class::Moved;
        return plan;
    }

    // Step 0: a prepared sidecar is neither served nor classified until it
    // is proven active.
    if let Some(p) = sc.prepared.clone() {
        match parent {
            Some(ParentFacts::Read {
                compacted_into,
                sidecar,
            }) => {
                let linked = compacted_into.as_deref() == Some(stem)
                    && sidecar
                        .as_deref()
                        .is_some_and(|ps| names_handoff(ps, &p.hx, stem));
                if !linked {
                    plan.class = Class::Missing;
                    plan.files.push(FileAction::Remove(sidecar_name(stem)));
                    return plan;
                }
                sc.prepared = None;
                plan.activated = true;
            }
            Some(ParentFacts::Unreadable) | None => {
                plan.class = Class::Missing;
                plan.files.push(FileAction::Quarantine {
                    from: sidecar_name(stem),
                    to: quarantine_name(stem),
                });
                return plan;
            }
        }
    }

    // A handoff in progress on this (parent) sidecar.
    if let Some(c) = sc.compacting.clone() {
        if c.to.session_id == c.from.session_id {
            if history.base == c.to.base {
                // The compaction landed: finish it.
                sc.base.clone_from(&c.to.base);
                sc.compacting = None;
                sc.newest_settled = None;
            } else if history.base == c.from.base {
                sc.compacting = None;
            }
        } else {
            // No `compacted_into` (above): the handoff never activated.
            sc.compacting = None;
            plan.files.push(FileAction::RemoveIfPrepared {
                session_id: c.to.session_id.clone(),
                hx: c.hx.clone(),
            });
        }
    }

    // Step 1: classify.
    let lineage_ok = history.marks.iter().all(|m| m.lineage() == sc.lineage);
    plan.class = if sc.session_id != stem || !lineage_ok {
        Class::Foreign
    } else {
        let settled_ok = sc
            .newest_settled
            .as_deref()
            .is_none_or(|a| history.has_answer(a));
        if sc.base == history.base && settled_ok {
            Class::Related
        } else {
            Class::Restored
        }
    };

    if plan.class == Class::Related {
        // Step 2: each unsettled record.
        let mut ids = Vec::new();
        for r in &mut sc.ledger {
            if r.delivery == Delivery::Settled {
                continue;
            }
            if history.has_answer(&r.answer_id) {
                r.delivery = Delivery::Recorded;
                plan.recorded.push(r.answer_id.clone());
            } else {
                r.delivery = Delivery::InContext;
                plan.deliver.push(r.answer_id.clone());
            }
            r.wake = false;
            ids.push(r.answer_id.clone());
        }
        for id in &ids {
            sync_view(&mut sc, id);
        }
    } else {
        // Step 3.
        sc = fresh(history, Some(&sc));
    }

    // Step 4: waiting cards go live; the epoch rises unless a reload
    // checkpoint wrote this image (it already did).
    let checkpointed = sc.checkpointed;
    sc.checkpointed = false;
    for c in &mut sc.cards {
        if c.state.mode == WaitMode::Waiting {
            c.state.mode = WaitMode::Live;
            c.state.downgraded = Some(Downgrade::Crash);
        }
    }
    if !checkpointed {
        apply(&mut sc, &Op::Stop(StopKind::Crash));
    }

    // Step 5: limits. Step 6: one commit.
    enforce_limits(&mut sc, now);
    plan.commit = Some(sc);
    plan
}

/// Apply a plan's file changes (the loader's half of step 0 and a dead
/// handoff's successor).
pub fn apply_files<F: UiFs + ?Sized>(fs: &F, files: &[FileAction]) -> io::Result<()> {
    for f in files {
        match f {
            FileAction::Remove(name) => fs.remove(name)?,
            FileAction::RemoveIfPrepared { session_id, hx } => {
                if let Loaded::Image(s) = load(fs, session_id)? {
                    if s.prepared.as_ref().is_some_and(|p| &p.hx == hx) {
                        fs.remove(&sidecar_name(session_id))?;
                    }
                }
            }
            FileAction::Quarantine { from, to } => fs.rename(from, to)?,
        }
    }
    if !files.is_empty() {
        fs.sync_dir()?;
    }
    Ok(())
}

// ── The compaction handoff (§3.11, R2-H2, R3-H3) ─────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffError {
    /// A record whose message the compaction would drop isn't settled.
    Unsettled,
    /// A handoff is already open on this image.
    Busy,
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffError::Unsettled => f.write_str("card answers not yet saved"),
            HandoffError::Busy => f.write_str("a compaction handoff is already open"),
        }
    }
}

impl std::error::Error for HandoffError {}

fn handoff_state(sc: &Sidecar) -> HandoffState {
    HandoffState {
        base: sc.base.clone(),
        compacting: sc.compacting.clone(),
        prepared: sc.prepared.clone(),
        moved_to: sc.moved_to.clone(),
        newest_settled: sc.newest_settled.clone(),
    }
}

/// Prepare (step 1): the parent's `compacting` op (its commit is the
/// ledger cut) and, for a successor, the successor's prepared image. Every
/// record in context must be settled first; queued ones are carried.
pub fn prepare(
    parent: &Sidecar,
    to: HandoffTo,
    hx: &str,
) -> Result<(Op, Option<Sidecar>), HandoffError> {
    if parent.compacting.is_some() || parent.prepared.is_some() || parent.moved_to.is_some() {
        return Err(HandoffError::Busy);
    }
    let unsettled = parent.ledger.iter().any(|r| {
        matches!(
            r.delivery,
            Delivery::Handed | Delivery::InContext | Delivery::Recorded
        )
    });
    if unsettled {
        return Err(HandoffError::Unsettled);
    }
    let compacting = Compacting {
        hx: hx.to_string(),
        from: HandoffFrom {
            session_id: parent.session_id.clone(),
            base: parent.base.clone(),
            seq: parent.seq,
        },
        to: to.clone(),
    };
    let mut st = handoff_state(parent);
    st.compacting = Some(compacting);
    let successor = (to.session_id != parent.session_id).then(|| {
        let mut s = parent.clone();
        s.session_id.clone_from(&to.session_id);
        s.base.clone_from(&to.base);
        s.compacting = None;
        s.moved_to = None;
        s.checkpointed = false;
        s.newest_settled = None;
        s.prepared = Some(Prepared {
            hx: hx.to_string(),
            parent: parent.session_id.clone(),
            from_seq: parent.seq,
        });
        s
    });
    Ok((Op::Handoff(Box::new(st)), successor))
}

/// Finish in place: the new base, `compacting` cleared.
pub fn finish_in_place(parent: &Sidecar) -> Op {
    let mut st = handoff_state(parent);
    if let Some(c) = st.compacting.take() {
        st.base = c.to.base;
        st.newest_settled = None;
    }
    Op::Handoff(Box::new(st))
}

/// Finish on the successor: `prepared` cleared.
pub fn finish_successor(successor: &Sidecar) -> Op {
    let mut st = handoff_state(successor);
    st.prepared = None;
    Op::Handoff(Box::new(st))
}

/// The parent's last commit after a successor handoff: `moved_to`.
pub fn moved(parent: &Sidecar) -> Op {
    let mut st = handoff_state(parent);
    if let Some(c) = st.compacting.take() {
        st.moved_to = Some(MovedTo {
            to: c.to.session_id,
            hx: c.hx,
        });
    }
    Op::Handoff(Box::new(st))
}

/// Rollback (any failure before the parent link): `compacting` cleared.
/// The caller's writer removes `<successor>.ui` first.
pub fn rollback(parent: &Sidecar) -> Op {
    let mut st = handoff_state(parent);
    st.compacting = None;
    Op::Handoff(Box::new(st))
}

// ── Settling (§3.11, R2-H3) ──────────────────────────────────────────────

/// A save request's ticket: its session and its place in issue order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ticket {
    pub session_id: String,
    pub n: u64,
}

/// What a persister save was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveKind {
    History,
    /// The `.turn` draft: never a receipt.
    Draft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receipt {
    Saved,
    NotYet,
}

/// Per-save receipts: `receipt(ticket)` is `Saved` only once a successful
/// history save of that session, covering the ticket, was applied.
/// Coalescing is fine: a later save of the same id holds the earlier
/// history. The persister's half (issuing tickets, reporting saves) is
/// C4a's wiring.
#[derive(Clone, Debug, Default)]
pub struct SaveReceipts {
    next: u64,
    saved: HashMap<String, u64>,
}

impl SaveReceipts {
    /// `request_save` for `session_id`.
    pub fn request(&mut self, session_id: &str) -> Ticket {
        self.next += 1;
        Ticket {
            session_id: session_id.to_string(),
            n: self.next,
        }
    }

    /// The highest ticket issued so far (a save started now covers it).
    pub fn issued(&self) -> u64 {
        self.next
    }

    /// A save of `session_id` that started after ticket `covers` was issued
    /// finished: `ok` and a history save, it covers every ticket up to it.
    pub fn applied(&mut self, session_id: &str, kind: SaveKind, covers: u64, ok: bool) {
        if ok && kind == SaveKind::History {
            let e = self.saved.entry(session_id.to_string()).or_default();
            *e = (*e).max(covers);
        }
    }

    pub fn receipt(&self, t: &Ticket) -> Receipt {
        if self.saved.get(&t.session_id).is_some_and(|n| *n >= t.n) {
            Receipt::Saved
        } else {
            Receipt::NotYet
        }
    }
}

/// Why the reload barrier failed (§3.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarrierFailure {
    UiStore(String),
    History,
}

/// The checkpoint barrier (§3.10): join the in-flight commit (unknown at
/// `budget` fails), every in-context record's history receipt must be
/// `Saved` (they become `Recorded`), then one `checkpointed` commit must
/// reach Durable. C4a wires a failure to the reload abort.
pub fn checkpoint_barrier<F: UiFs>(
    images: &mut Images,
    store: &mut UiStore<F>,
    tickets: &HashMap<String, Ticket>,
    receipts: &SaveReceipts,
    budget: Duration,
) -> Result<(), BarrierFailure> {
    match images.poll(store, budget) {
        Poll::Pending => return Err(BarrierFailure::UiStore("the writer didn't answer".into())),
        Poll::Done(Outcome::Unsynced(e)) => return Err(BarrierFailure::UiStore(e)),
        Poll::Idle | Poll::Done(_) => {}
    }
    let live = images.live();
    for r in live
        .ledger
        .iter()
        .filter(|r| r.delivery == Delivery::InContext)
    {
        let saved = tickets
            .get(&r.answer_id)
            .is_some_and(|t| receipts.receipt(t) == Receipt::Saved);
        if !saved {
            return Err(BarrierFailure::History);
        }
        images.note(Op::Deliver {
            answer_id: r.answer_id.clone(),
            to: Delivery::Recorded,
        });
    }
    images
        .commit(store, Some(Op::Checkpoint))
        .map_err(|r| BarrierFailure::UiStore(format!("{r:?}")))?;
    match images.poll(store, budget) {
        Poll::Done(Outcome::Durable) => Ok(()),
        Poll::Done(Outcome::NotPublished(e)) | Poll::Done(Outcome::Unsynced(e)) => {
            Err(BarrierFailure::UiStore(e))
        }
        Poll::Pending | Poll::Idle => {
            Err(BarrierFailure::UiStore("the writer didn't answer".into()))
        }
    }
}
