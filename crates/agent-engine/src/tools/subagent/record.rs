//! Worker record v2: schema and a pure resolver (dark; see
//! `docs/specs/subagent-context-rollover.md` §3 and slice 1).
//!
//! A rollover-capable worker run publishes a *head* record while it runs and a
//! *final* record when it ends. Both carry a random run ID, a generation and a
//! digest of the history they hold. [`resolve`] decides which supplied, already
//! parsed candidate is authoritative. It performs no I/O and no recovery: the
//! caller loads sibling files derived from a validated handle, reports whether
//! an earlier writer of the run may still publish, and acts on the decision.
//!
//! Nothing in the live archive, collect or resume paths uses this module yet.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use sha2::{Digest as _, Sha256};

use super::archive::SubagentArchive;
use crate::runtime::subagent::{SubagentArchiveMeta, WatchUsage};

/// Schema version of worker records written by rollover-capable runs.
pub const RECORD_VERSION: u32 = 2;
/// Version of the pre-rollover archive (`archive.rs`), accepted as legacy.
pub const LEGACY_VERSION: u32 = 1;
/// Total own + inherited window references a record may list. Each archive
/// appears once, so this bounds the serialized vectors as well.
pub const MAX_LINEAGE_REFS: usize = 128;

/// Why a record or a value inside it was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    UnsupportedVersion(u64),
    KindMismatch {
        expected: RecordKind,
        found: RecordKind,
    },
    InvalidHandle,
    InvalidRunId,
    InvalidArchiveId,
    InvalidRequestId,
    InvalidDigest,
    InvalidBinding,
    DigestMismatch,
    /// The history holds a value outside the `c1` encoding domain.
    HistoryNotEncodable,
    DuplicateWindow(u64),
    InvalidWindow,
    LineageCapExceeded(usize),
    Lineage(LineageError),
    Malformed(String),
}

/// A lineage that contradicts itself (§4, "Lineage structure").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineageError {
    /// One archive listed twice (exactly or under another owner/ordinal).
    DuplicateArchive(ArchiveId),
    /// Ordinals are not 1, 2, … in list order, inherited before own.
    OutOfOrder(u64),
    /// The run lists itself as predecessor or as an inherited owner.
    SelfReference,
    /// Inherited windows without a predecessor to inherit them from.
    MissingPredecessor,
    /// An inherited run reappears after a later run's windows.
    ChainOrder,
    /// `rollovers_this_run` differs from the number of own windows.
    RolloverCount,
    /// `window_ordinal` is not the ordinal after the last sealed window.
    Ordinal,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported worker record version {v}"),
            Self::KindMismatch { expected, found } => {
                write!(f, "expected a {expected:?} record, found {found:?}")
            }
            Self::InvalidHandle => f.write_str("invalid worker handle id"),
            Self::InvalidRunId => f.write_str("invalid run id"),
            Self::InvalidArchiveId => f.write_str("invalid archive id"),
            Self::InvalidRequestId => f.write_str("invalid request id"),
            Self::InvalidDigest => f.write_str("invalid history digest"),
            Self::InvalidBinding => f.write_str("invalid memory binding"),
            Self::DigestMismatch => f.write_str("history does not match its digest"),
            Self::HistoryNotEncodable => f.write_str("history is outside the c1 encoding"),
            Self::DuplicateWindow(w) => write!(f, "window {w} is listed twice"),
            Self::InvalidWindow => f.write_str("window numbers start at 1"),
            Self::LineageCapExceeded(n) => {
                write!(
                    f,
                    "{n} lineage references exceed the cap of {MAX_LINEAGE_REFS}"
                )
            }
            Self::Lineage(e) => write!(f, "contradictory lineage: {e:?}"),
            Self::Malformed(e) => write!(f, "malformed worker record: {e}"),
        }
    }
}

impl std::error::Error for RecordError {}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `sa_<n>` with `n` the canonical decimal spelling of a u64 in
/// `1..=u64::MAX` (no sign, no leading zeros): the form worker tools mint from
/// their counter, which starts at 1.
pub fn is_valid_handle_id(handle: &str) -> bool {
    handle.strip_prefix("sa_").is_some_and(|n| {
        !n.is_empty()
            && !n.starts_with('0')
            && n.bytes().all(|b| b.is_ascii_digit())
            && n.parse::<u64>().is_ok()
    })
}

/// Sibling file name of a record, derived only from a validated handle.
pub fn record_file_name(handle: &str, kind: RecordKind) -> Result<String, RecordError> {
    if !is_valid_handle_id(handle) {
        return Err(RecordError::InvalidHandle);
    }
    Ok(match kind {
        RecordKind::Head => format!("{handle}.head.json"),
        RecordKind::Final => format!("{handle}.json"),
    })
}

macro_rules! hex_id {
    ($(#[$doc:meta])* $name:ident, $len:expr, $err:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(raw: &str) -> Result<Self, RecordError> {
                if is_lower_hex(raw, $len) {
                    Ok(Self(raw.to_owned()))
                } else {
                    Err($err)
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = RecordError;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::parse(&raw)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

hex_id!(
    /// Random identity of one worker execution (128 bits, lowercase hex).
    /// Handle IDs are process-local and never identity.
    RunId,
    32,
    RecordError::InvalidRunId
);
hex_id!(
    /// A sealed eligible-archive segment (`ctx-<id>` without the prefix).
    ArchiveId,
    32,
    RecordError::InvalidArchiveId
);

impl RunId {
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }
}

// ── History digest: canonical encoding c1 ───────────────────────────────────

/// Domain prefix of the `c1` history encoding.
pub const C1_DOMAIN: &[u8] = b"synaps.worker-history.c1\0";
/// Nesting limit of the `c1` encoding (the JSON parser stops at 128).
pub const C1_MAX_DEPTH: usize = 256;

/// The `c1` encoding of a history (§3.1): `C1_DOMAIN` followed by the
/// history encoded as an array. Values are tag-length-value, all lengths and
/// counts u64 big-endian:
///
/// - `n` null; `f` false; `t` true;
/// - `u` + 8 bytes: an integer in `0..=u64::MAX`;
/// - `i` + 8 bytes: a negative integer in `i64::MIN..0` (two's complement);
/// - `d` + 8 bytes: any other number, as its IEEE-754 binary64 bits (finite
///   only; `-0.0`, `0.0`, `1.0` and `1` all differ);
/// - `s` + length + UTF-8 bytes, verbatim (no escaping or normalization);
/// - `a` + count + elements;
/// - `o` + count + entries sorted by the key's UTF-8 bytes, each key encoded
///   as length + bytes followed by its value.
///
/// It is independent of serde_json's map order or text formatting; only the
/// parsed value matters (with the engine's `float_roundtrip` parsing).
pub fn canonical_history_bytes(history: &[Value]) -> Result<Vec<u8>, RecordError> {
    let mut out = C1_DOMAIN.to_vec();
    out.push(b'a');
    put_len(&mut out, history.len());
    for value in history {
        encode_c1(value, 1, &mut out)?;
    }
    Ok(out)
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_be_bytes());
}

fn encode_c1(value: &Value, depth: usize, out: &mut Vec<u8>) -> Result<(), RecordError> {
    if depth > C1_MAX_DEPTH {
        return Err(RecordError::HistoryNotEncodable);
    }
    match value {
        Value::Null => out.push(b'n'),
        Value::Bool(false) => out.push(b'f'),
        Value::Bool(true) => out.push(b't'),
        Value::Number(n) => encode_number(n, out)?,
        Value::String(s) => {
            out.push(b's');
            put_len(out, s.len());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            out.push(b'a');
            put_len(out, items.len());
            for item in items {
                encode_c1(item, depth + 1, out)?;
            }
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'o');
            put_len(out, entries.len());
            for (key, item) in entries {
                put_len(out, key.len());
                out.extend_from_slice(key.as_bytes());
                encode_c1(item, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

fn encode_number(n: &Number, out: &mut Vec<u8>) -> Result<(), RecordError> {
    if !n.is_f64() {
        if let Some(u) = n.as_u64() {
            out.push(b'u');
            out.extend_from_slice(&u.to_be_bytes());
            return Ok(());
        }
        if let Some(i) = n.as_i64() {
            out.push(b'i');
            out.extend_from_slice(&i.to_be_bytes());
            return Ok(());
        }
    }
    match n.as_f64() {
        Some(f) if f.is_finite() => {
            out.push(b'd');
            out.extend_from_slice(&f.to_bits().to_be_bytes());
            Ok(())
        }
        _ => Err(RecordError::HistoryNotEncodable),
    }
}

/// `sha256-c1:<64 lowercase hex>`: SHA-256 of [`canonical_history_bytes`] of
/// a record's `history`, and of nothing else. An integrity check, not
/// authentication. A new encoding gets a new prefix.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HistoryDigest(String);

impl HistoryDigest {
    const PREFIX: &'static str = "sha256-c1:";

    pub fn of(history: &[Value]) -> Result<Self, RecordError> {
        let bytes = canonical_history_bytes(history)?;
        Ok(Self(format!(
            "{}{}",
            Self::PREFIX,
            hex(&Sha256::digest(&bytes))
        )))
    }
    pub fn parse(raw: &str) -> Result<Self, RecordError> {
        match raw.strip_prefix(Self::PREFIX) {
            Some(h) if is_lower_hex(h, 64) => Ok(Self(raw.to_owned())),
            _ => Err(RecordError::InvalidDigest),
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl TryFrom<String> for HistoryDigest {
    type Error = RecordError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<HistoryDigest> for String {
    fn from(d: HistoryDigest) -> String {
        d.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// Published while the run is live; a crash checkpoint afterwards.
    Head,
    /// Written once when the run ends.
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Legacy,
    Axel,
}

/// The memory backend and repository scope the run's windows were sealed in.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub backend: BackendKind,
    /// `sha256:<64 lowercase hex>` of the host-resolved repository scope key
    /// (never the raw path). The key's derivation is fixed with lineage
    /// validation (slice 3); records compare digests as exact strings.
    pub scope_digest: String,
}

impl Binding {
    fn validate(&self) -> Result<(), RecordError> {
        let ok = self
            .scope_digest
            .strip_prefix("sha256:")
            .is_some_and(|h| is_lower_hex(h, 64));
        ok.then_some(()).ok_or(RecordError::InvalidBinding)
    }
}

/// A window this run sealed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRef {
    pub window: u64,
    pub archive_id: ArchiveId,
}

/// A window sealed by a run in the validated predecessor chain, copied by value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InheritedRef {
    pub run_id: RunId,
    pub window: u64,
    pub archive_id: ArchiveId,
}

/// The validated record a resumed run continues (a head is a crash checkpoint).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predecessor {
    pub run_id: RunId,
    pub kind: RecordKind,
    pub generation: u64,
    /// The predecessor's `history_digest`: it commits to that record's
    /// history only, not to its lineage, binding, ledger or metadata.
    pub digest: HistoryDigest,
}

/// Strict v2 copy of [`SubagentArchiveMeta`]: unknown fields are rejected here,
/// while the shared v1 type stays permissive for old archives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerMeta {
    pub agent_name: String,
    pub model: String,
    pub system_prompt: String,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// A watch worker's lease used so far, across its runs (dev's
    /// `SubagentArchiveMeta::watch_usage`, `subagent.watch.*`); `None` for an
    /// ordinary worker. Carried so a resume from a record continues the same
    /// lease. Additive: a record without it reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_usage: Option<WorkerWatchUsage>,
}

/// Strict v2 copy of [`WatchUsage`] (unknown fields rejected, as at every
/// typed level of a record); the shared type stays permissive for archives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerWatchUsage {
    pub secs: u64,
    pub rounds: u32,
}

impl From<WatchUsage> for WorkerWatchUsage {
    fn from(u: WatchUsage) -> Self {
        Self {
            secs: u.secs,
            rounds: u.rounds,
        }
    }
}

impl From<WorkerWatchUsage> for WatchUsage {
    fn from(u: WorkerWatchUsage) -> Self {
        Self {
            secs: u.secs,
            rounds: u.rounds,
        }
    }
}

impl From<SubagentArchiveMeta> for WorkerMeta {
    fn from(m: SubagentArchiveMeta) -> Self {
        Self {
            agent_name: m.agent_name,
            model: m.model,
            system_prompt: m.system_prompt,
            timeout_secs: m.timeout_secs,
            thinking: m.thinking,
            watch_usage: m.watch_usage.map(Into::into),
        }
    }
}

impl From<WorkerMeta> for SubagentArchiveMeta {
    fn from(m: WorkerMeta) -> Self {
        Self {
            agent_name: m.agent_name,
            model: m.model,
            system_prompt: m.system_prompt,
            timeout_secs: m.timeout_secs,
            // A v2 record carries no directory (#317 records it in v1
            // archives and live state only).
            cwd: None,
            thinking: m.thinking,
            watch_usage: m.watch_usage.map(Into::into),
        }
    }
}

// ── Ledger identity ─────────────────────────────────────────────────────────

/// Host-assigned identity of one upstream request (stable across redelivery).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    pub fn parse(raw: &str) -> Result<Self, RecordError> {
        let ok = (1..=128).contains(&raw.len())
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        ok.then(|| Self(raw.to_owned()))
            .ok_or(RecordError::InvalidRequestId)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RequestId {
    type Error = RecordError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<RequestId> for String {
    fn from(id: RequestId) -> String {
        id.0
    }
}

/// Why a request was made: same-model summary usage stays attributable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Conversation,
    RolloverSummary,
}

/// Provider-reported usage of one request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_write_5m: Option<u64>,
    #[serde(default)]
    pub cache_write_1h: Option<u64>,
}

/// Measured usage, or an estimated cost held while the spend is unknown.
/// A liability is an estimate in micro-USD, never provider token usage.
/// `Released` carries nothing: it ends the liability it replaces, whose
/// estimate is not kept, so a release can never restate an amount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Amount {
    Measured { usage: MeasuredUsage },
    Liability { estimate_micro_usd: u64 },
    Released {},
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    pub request_id: RequestId,
    pub purpose: RequestPurpose,
    /// Model the request was priced against (older events without a model
    /// are attributed to the worker's model by the caller).
    pub model: String,
    pub amount: Amount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    Recorded,
    /// Exact redelivery of a settled entry: nothing changed.
    Duplicate,
    /// A held liability became measured or was released.
    Transitioned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettleError {
    /// Same request, different model or purpose.
    IdentityMismatch,
    /// A terminal entry cannot change, the transition is not allowed, or a
    /// new request starts released.
    Conflict,
    /// A new request at [`Ledger::MAX_ENTRIES`]. Dispatch reserves a slot
    /// before sending, so this is an admission refusal, not lost usage.
    Full,
}

/// Per-request usage of one run. Settlement is idempotent; a rejected update
/// leaves the ledger unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<LedgerEntry>", into = "Vec<LedgerEntry>")]
pub struct Ledger {
    entries: BTreeMap<RequestId, LedgerEntry>,
}

impl Ledger {
    pub const MAX_ENTRIES: usize = 4096;

    pub fn settle(&mut self, entry: LedgerEntry) -> Result<Settled, SettleError> {
        let Some(existing) = self.entries.get_mut(&entry.request_id) else {
            if matches!(entry.amount, Amount::Released {}) {
                return Err(SettleError::Conflict);
            }
            if self.entries.len() >= Self::MAX_ENTRIES {
                return Err(SettleError::Full);
            }
            self.entries.insert(entry.request_id.clone(), entry);
            return Ok(Settled::Recorded);
        };
        if existing.purpose != entry.purpose || existing.model != entry.model {
            return Err(SettleError::IdentityMismatch);
        }
        if existing.amount == entry.amount {
            return Ok(Settled::Duplicate);
        }
        match (&existing.amount, &entry.amount) {
            (Amount::Liability { .. }, Amount::Measured { .. } | Amount::Released { .. }) => {
                existing.amount = entry.amount;
                Ok(Settled::Transitioned)
            }
            _ => Err(SettleError::Conflict),
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl TryFrom<Vec<LedgerEntry>> for Ledger {
    type Error = RecordError;
    fn try_from(list: Vec<LedgerEntry>) -> Result<Self, Self::Error> {
        if list.len() > Self::MAX_ENTRIES {
            return Err(RecordError::Malformed(
                "ledger exceeds its entry cap".into(),
            ));
        }
        let mut entries = BTreeMap::new();
        for entry in list {
            let id = entry.request_id.clone();
            if entries.insert(id, entry).is_some() {
                return Err(RecordError::Malformed("duplicate ledger request id".into()));
            }
        }
        Ok(Self { entries })
    }
}

impl From<Ledger> for Vec<LedgerEntry> {
    fn from(ledger: Ledger) -> Self {
        ledger.entries.into_values().collect()
    }
}

// ── Records ─────────────────────────────────────────────────────────────────

/// A v2 worker record (head or final).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRecord {
    pub version: u32,
    pub run_id: RunId,
    pub handle_id: String,
    pub kind: RecordKind,
    /// Head: publications so far (0 = spawn head). Final: the last committed
    /// head generation it supersedes.
    pub generation: u64,
    pub history_digest: HistoryDigest,
    pub binding: Binding,
    #[serde(default)]
    pub predecessor: Option<Predecessor>,
    #[serde(default)]
    pub windows: Vec<WindowRef>,
    #[serde(default)]
    pub inherited_windows: Vec<InheritedRef>,
    /// Lineage-wide ordinal of the current window (1 = never rolled over).
    pub window_ordinal: u64,
    pub rollovers_this_run: u64,
    #[serde(default)]
    pub ledger: Ledger,
    pub meta: WorkerMeta,
    pub status: String,
    pub output: String,
    /// The current window verbatim, including any `_synaps_context` marker.
    pub history: Vec<Value>,
}

impl WorkerRecord {
    /// Own plus inherited references as serialized (a valid record lists
    /// each archive once).
    pub fn lineage_ref_count(&self) -> usize {
        self.windows.len() + self.inherited_windows.len()
    }

    /// Structural and integrity checks of one record against the kind its
    /// locator implies. Ownership and liveness of refs are checked later by
    /// the host against its archive store (§4), not here.
    pub fn validate(&self, expected: RecordKind) -> Result<(), RecordError> {
        if self.version != RECORD_VERSION {
            return Err(RecordError::UnsupportedVersion(u64::from(self.version)));
        }
        if self.kind != expected {
            return Err(RecordError::KindMismatch {
                expected,
                found: self.kind,
            });
        }
        if !is_valid_handle_id(&self.handle_id) {
            return Err(RecordError::InvalidHandle);
        }
        self.binding.validate()?;
        self.validate_lineage()?;
        if HistoryDigest::of(&self.history)? != self.history_digest {
            return Err(RecordError::DigestMismatch);
        }
        Ok(())
    }

    /// §4 "Lineage structure": every sealed window of the lineage appears
    /// once, inherited (oldest run first) then own, with ordinals 1, 2, … in
    /// list order; the current window is the next ordinal.
    fn validate_lineage(&self) -> Result<(), RecordError> {
        use LineageError as L;
        let refs = self.lineage_ref_count();
        if refs > MAX_LINEAGE_REFS {
            return Err(RecordError::LineageCapExceeded(refs));
        }
        if let Some(p) = &self.predecessor {
            if p.run_id == self.run_id {
                return Err(RecordError::Lineage(L::SelfReference));
            }
        } else if !self.inherited_windows.is_empty() {
            return Err(RecordError::Lineage(L::MissingPredecessor));
        }
        let mut finished_runs = BTreeSet::new();
        let mut owner: Option<&RunId> = None;
        for w in &self.inherited_windows {
            if w.run_id == self.run_id {
                return Err(RecordError::Lineage(L::SelfReference));
            }
            if owner != Some(&w.run_id) {
                if let Some(done) = owner {
                    finished_runs.insert(done);
                }
                if finished_runs.contains(&w.run_id) {
                    return Err(RecordError::Lineage(L::ChainOrder));
                }
                owner = Some(&w.run_id);
            }
        }
        let ordered = self
            .inherited_windows
            .iter()
            .map(|w| (w.window, &w.archive_id))
            .chain(self.windows.iter().map(|w| (w.window, &w.archive_id)));
        let mut ordinals = BTreeSet::new();
        let mut archives = BTreeSet::new();
        for (expected, (window, archive)) in (1u64..).zip(ordered) {
            if window == 0 {
                return Err(RecordError::InvalidWindow);
            }
            if !ordinals.insert(window) {
                return Err(RecordError::DuplicateWindow(window));
            }
            if window != expected {
                return Err(RecordError::Lineage(L::OutOfOrder(window)));
            }
            if !archives.insert(archive) {
                return Err(RecordError::Lineage(L::DuplicateArchive(archive.clone())));
            }
        }
        if self.window_ordinal == 0 {
            return Err(RecordError::InvalidWindow);
        }
        if self.window_ordinal != refs as u64 + 1 {
            return Err(RecordError::Lineage(L::Ordinal));
        }
        if self.rollovers_this_run != self.windows.len() as u64 {
            return Err(RecordError::Lineage(L::RolloverCount));
        }
        Ok(())
    }
}

/// A final-record file: the legacy v1 archive or a v2 record. Both variants
/// are re-checked by [`resolve`], so a hand-built value gains nothing.
#[derive(Debug)]
pub enum FinalRecord {
    V1(Box<SubagentArchive>),
    V2(Box<WorkerRecord>),
}

fn version_of(bytes: &[u8]) -> Result<u64, RecordError> {
    #[derive(Deserialize)]
    struct Probe {
        version: u64,
    }
    serde_json::from_slice::<Probe>(bytes)
        .map(|p| p.version)
        .map_err(|e| RecordError::Malformed(e.to_string()))
}

/// Parse a final-record file of either version. Parsing is syntactic: typed
/// fields are checked, but nothing is admitted until [`resolve`] validates the
/// record against its locator and expectations.
pub fn parse_final(bytes: &[u8]) -> Result<FinalRecord, RecordError> {
    match version_of(bytes)? {
        v if v == u64::from(LEGACY_VERSION) => serde_json::from_slice(bytes)
            .map(|a| FinalRecord::V1(Box::new(a)))
            .map_err(|e| RecordError::Malformed(e.to_string())),
        v if v == u64::from(RECORD_VERSION) => {
            parse_v2(bytes).map(|r| FinalRecord::V2(Box::new(r)))
        }
        v => Err(RecordError::UnsupportedVersion(v)),
    }
}

/// Parse a head-record file (v2 only). Syntactic like [`parse_final`]: a final
/// record in a head's place still parses and is refused by [`resolve`].
pub fn parse_head(bytes: &[u8]) -> Result<WorkerRecord, RecordError> {
    match version_of(bytes)? {
        v if v == u64::from(RECORD_VERSION) => parse_v2(bytes),
        v => Err(RecordError::UnsupportedVersion(v)),
    }
}

fn parse_v2(bytes: &[u8]) -> Result<WorkerRecord, RecordError> {
    serde_json::from_slice(bytes).map_err(|e| RecordError::Malformed(e.to_string()))
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// One supplied candidate file.
#[derive(Debug)]
pub enum Candidate<T> {
    Absent,
    /// Present but could not be read or parsed.
    Unreadable(String),
    Present(T),
}

/// Whether an earlier writer of this run may still publish. Only the owned
/// writer's completion (or the end of its process) proves quiescence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterState {
    Quiescent,
    MayPublish,
}

/// What the locator promises about the run.
#[derive(Clone, Debug)]
pub struct Expectations {
    pub handle_id: String,
    pub run_id: Option<RunId>,
    pub binding: Option<Binding>,
}

#[derive(Debug)]
pub struct ResolveInput {
    pub head: Candidate<WorkerRecord>,
    pub final_record: Candidate<FinalRecord>,
    pub writer: WriterState,
    pub expect: Expectations,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unresolved {
    WriterMayPublish,
    HeadUnreadable(String),
    FinalUnreadable(String),
    /// A v2 head next to a v1 final.
    MixedVersions,
    RunIdMismatch,
    HandleMismatch,
    BindingMismatch,
    /// The head is newer than the final that should supersede it.
    HeadNewerThanFinal {
        head: u64,
        final_generation: u64,
    },
    Invalid {
        kind: RecordKind,
        error: RecordError,
    },
}

#[derive(Debug)]
pub enum Resolution {
    NotFound,
    /// A pre-rollover archive: full history, no lineage.
    Legacy(Box<SubagentArchive>),
    Authoritative {
        kind: RecordKind,
        record: Box<WorkerRecord>,
        /// Set for a head: a crash checkpoint that may predate later work.
        loss_warning: bool,
    },
    Unresolved(Unresolved),
}

/// Decide which supplied candidate is authoritative (§3.4). Pure: never reads,
/// writes, republishes or recovers.
///
/// `Authoritative` selects a disk candidate; it never certifies durability. A
/// final whose rename succeeded but whose fsync failed is selectable here, so
/// the checkpoint owner keeps that uncertainty itself and durably republishes
/// an ambiguous final before acknowledging it or deleting the head (slice 4).
/// `Legacy` satisfies no v2 expectation: an expected run ID or binding refuses
/// a v1 archive.
pub fn resolve(input: ResolveInput) -> Resolution {
    let ResolveInput {
        head,
        final_record,
        writer,
        expect,
    } = input;
    // A snapshot is not a completion barrier: while an earlier writer can
    // still rename, nothing on disk may be adopted.
    if writer == WriterState::MayPublish {
        return Resolution::Unresolved(Unresolved::WriterMayPublish);
    }
    // Atomic renames never leave a torn record, so an unreadable one is
    // abnormal and may be the newer state.
    let head = match head {
        Candidate::Unreadable(e) => return Resolution::Unresolved(Unresolved::HeadUnreadable(e)),
        Candidate::Absent => None,
        Candidate::Present(r) => Some(r),
    };
    let final_record = match final_record {
        Candidate::Unreadable(e) => return Resolution::Unresolved(Unresolved::FinalUnreadable(e)),
        Candidate::Absent => None,
        Candidate::Present(r) => Some(r),
    };
    let checked = |record: &WorkerRecord, kind: RecordKind| -> Result<(), Unresolved> {
        record
            .validate(kind)
            .map_err(|error| Unresolved::Invalid { kind, error })?;
        if record.handle_id != expect.handle_id {
            return Err(Unresolved::HandleMismatch);
        }
        if expect
            .run_id
            .as_ref()
            .is_some_and(|id| *id != record.run_id)
        {
            return Err(Unresolved::RunIdMismatch);
        }
        if expect
            .binding
            .as_ref()
            .is_some_and(|b| *b != record.binding)
        {
            return Err(Unresolved::BindingMismatch);
        }
        Ok(())
    };
    let outcome = match (head, final_record) {
        (None, None) => return Resolution::NotFound,
        (None, Some(FinalRecord::V1(archive))) => {
            let version = u64::from(archive.version);
            return Resolution::Unresolved(if version != u64::from(LEGACY_VERSION) {
                Unresolved::Invalid {
                    kind: RecordKind::Final,
                    error: RecordError::UnsupportedVersion(version),
                }
            } else if expect.run_id.is_some() {
                Unresolved::RunIdMismatch
            } else if expect.binding.is_some() {
                Unresolved::BindingMismatch
            } else {
                return Resolution::Legacy(archive);
            });
        }
        (Some(_), Some(FinalRecord::V1(_))) => Err(Unresolved::MixedVersions),
        (Some(head), None) => checked(&head, RecordKind::Head).map(|()| (RecordKind::Head, head)),
        (None, Some(FinalRecord::V2(fin))) => {
            checked(&fin, RecordKind::Final).map(|()| (RecordKind::Final, *fin))
        }
        (Some(head), Some(FinalRecord::V2(fin))) => checked(&head, RecordKind::Head)
            .and_then(|()| checked(&fin, RecordKind::Final))
            .and_then(|()| {
                if head.run_id != fin.run_id {
                    Err(Unresolved::RunIdMismatch)
                } else if head.binding != fin.binding {
                    Err(Unresolved::BindingMismatch)
                } else if head.generation > fin.generation {
                    Err(Unresolved::HeadNewerThanFinal {
                        head: head.generation,
                        final_generation: fin.generation,
                    })
                } else {
                    Ok((RecordKind::Final, *fin))
                }
            }),
    };
    match outcome {
        Ok((kind, record)) => Resolution::Authoritative {
            kind,
            record: Box::new(record),
            loss_warning: kind == RecordKind::Head,
        },
        Err(reason) => Resolution::Unresolved(reason),
    }
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
