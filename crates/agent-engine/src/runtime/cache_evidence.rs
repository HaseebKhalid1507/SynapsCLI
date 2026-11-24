//! Prompt-cache evidence: what this session's own requests show about the
//! provider's cache (`SessionQuery::CacheExpiry`, arch-review-435-436-437.md
//! Contract v3 §3, v3.1 and the R4 ruling).
//!
//! The answer is evidence, not provider state. Every attempt a foreground
//! producer dispatches takes a [`CacheTicket`] at the moment its bytes go
//! to the HTTP client; the response's parsed usage is then
//! [`CacheEvidence::observe`]d against that ticket. Nothing else is ever
//! evidence (a booked or estimated cost is not). A probe assembles the next
//! request (the history plus one placeholder user block) through the same
//! builder, sends nothing, and matches the records whose bytes it repeats.
//!
//! - **Namespace** (`ns`): [`agent_core::auth::CacheNamespace`] of the
//!   attempt's provider, URL, auth kind and the credential as sent.
//! - **Coordinates** ([`BlockCoord`]): the final wire `messages`, after image
//!   projection, the retention anchor, thinking sanitation, merging and
//!   marker placement. `ord` counts raw content blocks across messages (a
//!   string content is one block). The provider counts a run of `tool_use`
//!   or `tool_result` blocks as one lookback position, so the 20-position
//!   check under-approximates: it can miss a match, never invent one.
//! - **`stable`**: model, `output_config`, system, thinking, tools (their
//!   markers included) and the beta header.
//! - **`hash_at(c)`**: FNV over `stable`, every message before `c.msg`, and
//!   message `c.msg`'s role and blocks through `c.block`, with history
//!   `cache_control` stripped and string content read as one text block.
//!
//! Bounds (v3.1 §1, R4-M1): `warm_until` is per record (its proof) and may
//! fall; `lapse_by` is the record's group horizon `(ns, stable)` or the
//! store's `floor`, whichever is later, and never shrinks. A new or
//! reintroduced group's `ttl_cap` starts at the longest TTL this store has
//! ever ticketed. The store's `version` advances on every ticket, observe
//! and eviction (R4-M2) and is checked in the query's `finish`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::SharedMessage;

/// Records kept, evicted least recently sent first.
pub const RECORDS: usize = 8;
/// Group horizons kept, evicted least recently ticketed first.
pub const GROUPS: usize = 32;
/// The provider's documented lookback, in content-block positions.
pub const LOOKBACK: usize = 20;

const FIVE_MINUTES: Duration = Duration::from_secs(300);
const ONE_HOUR: Duration = Duration::from_secs(3600);

/// A content block's place in a request's final wire `messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockCoord {
    pub msg: usize,
    pub block: usize,
    /// Raw content blocks before this one, across messages.
    pub ord: usize,
}

/// A history `cache_control` breakpoint and the TTL it was sent with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    pub at: BlockCoord,
    pub ttl: Duration,
}

/// What one request's final wire body says about its cache entries.
#[derive(Debug, Clone, PartialEq)]
pub struct WireEvidence {
    pub stable: u64,
    /// History markers in wire order (the retention anchor, then the tail).
    pub markers: Vec<Marker>,
    /// The last history marker and `hash_at` of it.
    pub tail: Option<(Marker, u64)>,
    /// The longest TTL any marker of the request carried, system and tool
    /// markers included.
    pub max_ttl: Duration,
}

impl WireEvidence {
    /// `messages` are the request's final wire messages; `stable_ttls` the
    /// TTLs of its system and tool markers.
    pub fn of(stable: u64, messages: &[SharedMessage], stable_ttls: impl IntoIterator<Item = Duration>) -> Self {
        let markers = history_markers(messages);
        let tail = markers.last().copied().map(|m| {
            let h = hash_at(stable, messages, &[m.at]);
            (m, h[0].expect("a marker's own coordinate exists"))
        });
        let max_ttl = markers
            .iter()
            .map(|m| m.ttl)
            .chain(stable_ttls)
            .max()
            .unwrap_or(Duration::ZERO);
        Self { stable, markers, tail, max_ttl }
    }
}

/// The TTL a `cache_control` value carries (`"ttl":"1h"`, else 5m).
pub fn marker_ttl(cache_control: &Value) -> Duration {
    match cache_control.get("ttl").and_then(Value::as_str) {
        Some("1h") => ONE_HOUR,
        _ => FIVE_MINUTES,
    }
}

/// A message's content as blocks: a string is one text block.
fn blocks(message: &Value) -> std::borrow::Cow<'_, [Value]> {
    match &message["content"] {
        Value::Array(a) => std::borrow::Cow::Borrowed(a.as_slice()),
        Value::String(s) => std::borrow::Cow::Owned(vec![serde_json::json!({"type": "text", "text": s})]),
        _ => std::borrow::Cow::Owned(Vec::new()),
    }
}

/// Every history block carrying `cache_control`, in wire order.
pub fn history_markers(messages: &[SharedMessage]) -> Vec<Marker> {
    let mut out = Vec::new();
    let mut ord = 0;
    for (msg, m) in messages.iter().enumerate() {
        for (block, b) in blocks(m).iter().enumerate() {
            if let Some(cc) = b.get("cache_control") {
                out.push(Marker { at: BlockCoord { msg, block, ord }, ttl: marker_ttl(cc) });
            }
            ord += 1;
        }
    }
    out
}

/// Streaming FNV-1a, as `RequestBody::prefix_fingerprint` hashes.
pub(crate) struct Fnv(pub(crate) u64);

impl Fnv {
    pub(crate) fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl std::io::Write for Fnv {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for b in buf {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `hash_at` of each coordinate in `at` (matched on `msg`/`block`), in one
/// pass; `None` for a coordinate past the end of `messages`.
pub fn hash_at(stable: u64, messages: &[SharedMessage], at: &[BlockCoord]) -> Vec<Option<u64>> {
    use std::io::Write as _;
    let mut out = vec![None; at.len()];
    let last = match at.iter().map(|c| (c.msg, c.block)).max() {
        Some(l) => l,
        None => return out,
    };
    let mut h = Fnv::new();
    let _ = h.write_all(&stable.to_le_bytes());
    for (msg, m) in messages.iter().enumerate() {
        if msg > last.0 {
            break;
        }
        let _ = h.write_all(&[0x1e]);
        let _ = serde_json::to_writer(&mut h, &m["role"]);
        for (block, b) in blocks(m).iter().enumerate() {
            if (msg, block) > last {
                break;
            }
            let _ = h.write_all(&[0x1f]);
            if b.get("cache_control").is_some() {
                let mut stripped = b.clone();
                if let Some(o) = stripped.as_object_mut() {
                    o.remove("cache_control");
                }
                let _ = serde_json::to_writer(&mut h, &stripped);
            } else {
                let _ = serde_json::to_writer(&mut h, b);
            }
            for (i, c) in at.iter().enumerate() {
                if (c.msg, c.block) == (msg, block) {
                    out[i] = Some(h.0);
                }
            }
        }
    }
    out
}

/// One attempt's place in the send order (taken at dispatch).
#[derive(Debug, Clone)]
pub struct CacheTicket {
    ns: Arc<str>,
    tail: Option<(Marker, u64)>,
    seq: u64,
    sent: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Proof {
    seq: u64,
    sent: Instant,
    ttl: Duration,
    /// The tail TTL the proving request asked for.
    requested: Duration,
    /// Read + write over all of its markers.
    tokens: u64,
}

#[derive(Debug, Clone)]
struct Record {
    ns: Arc<str>,
    stable: u64,
    tail: BlockCoord,
    hash: u64,
    last_send: u64,
    /// `(seq, read + write > 0)` of the latest measured send.
    measured: Option<(u64, bool)>,
    proof: Option<Proof>,
}

#[derive(Debug, Clone)]
struct Group {
    ns: Arc<str>,
    stable: u64,
    ttl_cap: Duration,
    until: Instant,
    last_seq: u64,
}

#[derive(Debug, Default)]
struct Store {
    seq: u64,
    version: u64,
    records: Vec<Record>,
    groups: Vec<Group>,
    /// The latest horizon of every evicted group.
    floor: Option<Instant>,
    /// The longest TTL ever ticketed (R4-M1): seeds every new group.
    max_ever_ttl: Duration,
}

/// The session's cache evidence: one per actor lifetime, shared by its
/// producers (`Runtime::share_ttl_latches`); a `Runtime::clone` gets none.
#[derive(Debug, Default)]
pub struct CacheEvidence(Mutex<Store>);

impl CacheEvidence {
    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Changes on every ticket, observe and eviction.
    pub fn version(&self) -> u64 {
        self.store().version
    }

    /// An attempt is being dispatched now. Every attempt counts toward its
    /// group's horizon, whatever becomes of it.
    pub fn ticket(&self, ns: &str, ev: &WireEvidence) -> CacheTicket {
        let mut s = self.store();
        s.seq += 1;
        s.version += 1;
        let (seq, sent) = (s.seq, Instant::now());
        s.max_ever_ttl = s.max_ever_ttl.max(ev.max_ttl);
        let seed = s.max_ever_ttl;
        let ns: Arc<str> = match s.groups.iter().find(|g| &*g.ns == ns) {
            Some(g) => Arc::clone(&g.ns),
            None => Arc::from(ns),
        };
        match s.groups.iter_mut().find(|g| g.ns == ns && g.stable == ev.stable) {
            Some(g) => {
                g.ttl_cap = g.ttl_cap.max(ev.max_ttl);
                g.until = g.until.max(sent + g.ttl_cap);
                g.last_seq = seq;
            }
            None => {
                s.groups.push(Group { ns: Arc::clone(&ns), stable: ev.stable, ttl_cap: seed, until: sent + seed, last_seq: seq });
                if s.groups.len() > GROUPS {
                    let i = (0..s.groups.len()).min_by_key(|&i| s.groups[i].last_seq).expect("non-empty");
                    let g = s.groups.remove(i);
                    s.floor = Some(s.floor.map_or(g.until, |f| f.max(g.until)));
                    s.records.retain(|r| !(r.ns == g.ns && r.stable == g.stable));
                }
            }
        }
        if let Some((tail, hash)) = ev.tail {
            match s.records.iter_mut().find(|r| r.ns == ns && r.hash == hash) {
                Some(r) => r.last_send = seq,
                None => {
                    s.records.push(Record {
                        ns: Arc::clone(&ns),
                        stable: ev.stable,
                        tail: tail.at,
                        hash,
                        last_send: seq,
                        measured: None,
                        proof: None,
                    });
                    if s.records.len() > RECORDS {
                        let i = (0..s.records.len()).min_by_key(|&i| s.records[i].last_send).expect("non-empty");
                        s.records.remove(i);
                    }
                }
            }
        }
        CacheTicket { ns, tail: ev.tail, seq, sent }
    }

    /// The provider's parsed usage for `t`'s request. A measurement older
    /// than the record's latest is ignored; the proof's clock is the send.
    pub fn observe(&self, t: &CacheTicket, read: u64, write: u64, write_5m: Option<u64>, write_1h: Option<u64>) {
        let Some((tail, hash)) = t.tail else {
            return;
        };
        let mut s = self.store();
        let Some(r) = s.records.iter_mut().find(|r| r.ns == t.ns && r.hash == hash) else {
            return;
        };
        if r.measured.is_some_and(|(seq, _)| seq > t.seq) {
            return;
        }
        let active = read.saturating_add(write) > 0;
        r.measured = Some((t.seq, active));
        if active && r.proof.is_none_or(|p| p.seq <= t.seq) {
            let ttl = proof_ttl(tail.ttl, write, write_5m, write_1h, r.proof.map(|p| p.ttl));
            r.proof = Some(Proof { seq: t.seq, sent: t.sent, ttl, requested: tail.ttl, tokens: read.saturating_add(write) });
        }
        s.version += 1;
    }

    /// The probe's candidates, from one consistent read of the store.
    pub fn select(&self, probe: &ProbeEvidence) -> Selection {
        let (version, records, horizon) = {
            let s = self.store();
            if probe.ns.is_empty() {
                // The next request's credential is not known yet.
                return Selection { version: s.version, candidates: Vec::new() };
            }
            let records: Vec<Record> = s
                .records
                .iter()
                .filter(|r| &*r.ns == probe.ns.as_str() && r.stable == probe.stable)
                .cloned()
                .collect();
            let until = s.groups.iter().find(|g| &*g.ns == probe.ns.as_str() && g.stable == probe.stable).map(|g| g.until);
            (s.version, records, until.map(|u| s.floor.map_or(u, |f| f.max(u))))
        };
        let coords: Vec<BlockCoord> = records.iter().map(|r| r.tail).collect();
        let hashes = hash_at(probe.stable, &probe.messages, &coords);
        let candidates = records
            .iter()
            .zip(hashes)
            .filter(|(r, h)| *h == Some(r.hash))
            .filter(|(r, _)| probe.markers.iter().any(|d| r.tail.ord <= d.at.ord && d.at.ord < r.tail.ord + LOOKBACK))
            .map(|(r, _)| Candidate {
                measured: r.measured.map(|(_, a)| a),
                warm_until: r.proof.filter(|p| r.measured == Some((p.seq, true))).map(|p| p.sent + p.ttl),
                lapse_by: horizon,
                ord: r.tail.ord,
                last_send: r.last_send,
                proof: r.proof.map(|p| (p.ttl, p.requested, p.tokens)),
            })
            .collect();
        Selection { version, candidates }
    }
}

/// The TTL a measured request proves for its key (v3 §3.4).
fn proof_ttl(tail: Duration, write: u64, write_5m: Option<u64>, write_1h: Option<u64>, previous: Option<Duration>) -> Duration {
    if tail < ONE_HOUR {
        return FIVE_MINUTES;
    }
    let (w5, w1) = (write_5m.unwrap_or(0), write_1h.unwrap_or(0));
    if w5 > 0 {
        FIVE_MINUTES
    } else if w1 > 0 {
        ONE_HOUR
    } else if write == 0 {
        previous.unwrap_or(FIVE_MINUTES)
    } else {
        // Written, split unreported: the shorter.
        FIVE_MINUTES
    }
}

/// The next request as the probe assembled it, nothing sent.
#[derive(Debug, Clone)]
pub struct ProbeEvidence {
    /// Empty while the runtime holds no credential (it is credential-blind
    /// until its first request): nothing matches.
    pub ns: String,
    pub stable: u64,
    pub markers: Vec<Marker>,
    pub messages: Vec<SharedMessage>,
    /// The next turn would roll the context over first.
    pub rollover: bool,
}

/// One matching record, its deadlines as `Instant`s (classified at emit).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// `None`: never measured; `Some(false)`: the latest measurement was
    /// inactive.
    pub measured: Option<bool>,
    pub warm_until: Option<Instant>,
    /// `None` only if the group is missing (never cold then).
    pub lapse_by: Option<Instant>,
    pub ord: usize,
    pub last_send: u64,
    /// `(proof ttl, requested tail ttl, tokens)`.
    pub proof: Option<(Duration, Duration, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub version: u64,
    pub candidates: Vec<Candidate>,
}

/// `CacheExpiry.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Warm,
    Cold,
    #[serde(other)]
    Unknown,
}

/// `CacheExpiry.reason`. Append-only; an unknown reason reads as `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheReason {
    Expired,
    Uncached,
    MayHaveExpired,
    Unmeasured,
    NoMatchingRequest,
    Busy,
    Parked,
    NotAnthropic,
    HeadUnresolved,
    HistoryUnsettled,
    RolloverPending,
    ProbeFailed,
    #[serde(other)]
    Unknown,
}

/// The `SessionQuery::CacheExpiry` reply. Every field after `state` is
/// optional; durations are whole seconds from the daemon's emit (lower
/// bounds floored, upper bounds ceiled).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheExpiry {
    pub state: CacheState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<CacheReason>,
    /// `warm`: about how long the latest measured matching request's proof
    /// has left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_for_secs: Option<u64>,
    /// When this session's last possible refresh of the matched entry ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lapse_by_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ttl_secs: Option<u64>,
    /// Read + write of the proving request over all its markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_cache_tokens: Option<u64>,
    /// While a keep-warm period is active: the next scheduled attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_ping_in_secs: Option<u64>,
}

impl CacheExpiry {
    pub fn unknown(reason: CacheReason) -> Self {
        Self {
            state: CacheState::Unknown,
            reason: Some(reason),
            warm_for_secs: None,
            lapse_by_secs: None,
            proof_ttl_secs: None,
            requested_ttl_secs: None,
            proof_cache_tokens: None,
            next_ping_in_secs: None,
        }
    }
}

impl Candidate {
    /// What this record says at `now`.
    pub fn classify(&self, now: Instant) -> (CacheState, Option<CacheReason>) {
        match self.measured {
            None => (CacheState::Unknown, Some(CacheReason::Unmeasured)),
            Some(false) => (CacheState::Cold, Some(CacheReason::Uncached)),
            Some(true) => {
                if self.warm_until.is_some_and(|w| now < w) {
                    (CacheState::Warm, None)
                } else if self.lapse_by.is_some_and(|l| now >= l) {
                    (CacheState::Cold, Some(CacheReason::Expired))
                } else {
                    (CacheState::Unknown, Some(CacheReason::MayHaveExpired))
                }
            }
        }
    }
}

fn rank(state: CacheState) -> u8 {
    match state {
        CacheState::Warm => 2,
        CacheState::Unknown => 1,
        CacheState::Cold => 0,
    }
}

fn floor_secs(d: Duration) -> u64 {
    d.as_secs()
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// What a probe job found, deadlines kept as `Instant`s: it is aged and
/// classified only at emit (v3.1 §2).
#[derive(Debug, Clone, PartialEq)]
pub enum CacheEval {
    /// Decided without the store (`rollover_pending`, `probe_failed`).
    Fixed(CacheReason),
    /// Matched against the store at `selection.version`.
    Matched { selection: Selection, next_ping_at: Option<Instant> },
}

impl CacheEval {
    /// The store version this answer was read at, if it read the store.
    pub fn store_version(&self) -> Option<u64> {
        match self {
            Self::Fixed(_) => None,
            Self::Matched { selection, .. } => Some(selection.version),
        }
    }

    /// The reply as `finish` emits it (R4-M2): `None` (`stale`) when the
    /// store moved since the job read it, else [`at`](Self::at) `now`. A
    /// saved deadline is never aged against a store that has newer sends.
    pub fn finalize(&self, store: &CacheEvidence, now: Instant) -> Option<CacheExpiry> {
        if self.store_version().is_some_and(|v| v != store.version()) {
            return None;
        }
        Some(self.at(now))
    }

    /// The reply, classified and aged at `now`. Ranking: warm, then any
    /// unknown, then cold; then the longest prefix; then the latest send.
    pub fn at(&self, now: Instant) -> CacheExpiry {
        let (selection, next_ping_at) = match self {
            Self::Fixed(reason) => return CacheExpiry::unknown(*reason),
            Self::Matched { selection, next_ping_at } => (selection, *next_ping_at),
        };
        let next_ping_in_secs = next_ping_at.map(|t| floor_secs(t.saturating_duration_since(now)));
        let best = selection
            .candidates
            .iter()
            .map(|c| (c.classify(now), c))
            .max_by_key(|((state, _), c)| (rank(*state), c.ord, c.last_send));
        let Some(((state, reason), c)) = best else {
            return CacheExpiry { next_ping_in_secs, ..CacheExpiry::unknown(CacheReason::NoMatchingRequest) };
        };
        let live = matches!(reason, None | Some(CacheReason::MayHaveExpired));
        CacheExpiry {
            state,
            reason,
            warm_for_secs: (state == CacheState::Warm)
                .then(|| c.warm_until.map(|w| floor_secs(w.saturating_duration_since(now))))
                .flatten(),
            lapse_by_secs: if live { c.lapse_by.map(|l| ceil_secs(l.saturating_duration_since(now))) } else { None },
            proof_ttl_secs: c.proof.map(|p| p.0.as_secs()),
            requested_ttl_secs: c.proof.map(|p| p.1.as_secs()),
            proof_cache_tokens: c.proof.map(|p| p.2),
            next_ping_in_secs,
        }
    }
}

/// The recommended rendering (daemon-mode.md, v3.1 §4): the TUI's
/// `/keepwarm status` uses it word for word.
pub fn render_cache_expiry(c: &CacheExpiry) -> String {
    // A lower bound rounds down, an upper bound up.
    let at_least = |s: u64| if s < 60 { format!("{s}s") } else { format!("{}m", s / 60) };
    let at_most = |s: u64| if s < 60 { format!("{s}s") } else { format!("{}m", s.div_ceil(60)) };
    let mut out = match (c.state, c.reason) {
        (CacheState::Warm, _) => match c.warm_for_secs {
            Some(s) => format!("Recent cache activity · about {} left", at_least(s)),
            None => "Recent cache activity".to_string(),
        },
        (CacheState::Unknown, Some(CacheReason::MayHaveExpired)) => {
            let mut s = "May have expired".to_string();
            if let Some(l) = c.lapse_by_secs {
                s.push_str(&format!(" · this session's last possible refresh ends in about {}", at_most(l)));
            }
            s
        }
        (CacheState::Cold, Some(CacheReason::Uncached)) => "Last measured request reported no cache activity".to_string(),
        (CacheState::Cold, _) => "No unexpired evidence from this session".to_string(),
        (CacheState::Unknown, reason) => match reason {
            Some(CacheReason::Parked) => "parked",
            Some(CacheReason::Busy) => "a turn is running",
            Some(CacheReason::HistoryUnsettled) => "last turn didn't finish",
            Some(CacheReason::NoMatchingRequest) => "no matching request yet",
            Some(CacheReason::Unmeasured) => "last request not measured yet",
            Some(CacheReason::RolloverPending) => "next turn starts a new context",
            Some(CacheReason::NotAnthropic) => "not tracked for this provider",
            _ => "couldn't check",
        }
        .to_string(),
    };
    if let Some(tokens) = c.proof_cache_tokens.filter(|_| c.state == CacheState::Warm) {
        out.push_str(&format!(" · {tokens} tokens read or written by the last measured request"));
    }
    if let Some(n) = c.next_ping_in_secs {
        out.push_str(&format!(" · next keep-warm attempt in about {}", at_least(n)));
    }
    out
}

#[cfg(test)]
mod tests;
