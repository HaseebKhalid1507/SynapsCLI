//! `SessionQuery::CacheExpiry` (arch-review-435-436-437.md Contract v3 §3,
//! v3.1): what this session's own requests show about the provider's prompt
//! cache for its next request, read from the runtime's cache anchor cell
//! (`runtime::keepwarm::CacheAnchorCell`), the one record keep-warm
//! scheduling and cache-preserving compaction read too.
//!
//! The answer is evidence, not provider state. The cell holds the last
//! accepted request of the session's request chain (turn rounds, keep-warm
//! pings, compaction's cached request): when it was sent, what it left in
//! the cache (`CachedPrefix`: its cache identity and its messages), the
//! cache reads and writes the provider reported for it, and the latest
//! request dispatched at all. Nothing older is kept: only the latest
//! request counts.
//!
//! - **Matches**: the next request (the history, assembled by the code that
//!   builds a keep-warm ping, nothing sent) has the same cache identity and
//!   starts with that request's messages. Otherwise `no_matching_request`.
//! - **`warm`** from the anchor for the TTL the server is known to honour
//!   for the history entry, as keep-warm schedules it (`honoured_ttl`: the
//!   request's own 1h/5m write split, else the session's latches; 1h only
//!   with evidence of a 1h write and no downgrade); then
//!   **`may_have_expired`** until the TTL requested for it has run from the
//!   latest dispatch (an unanswered request may still have refreshed the
//!   entry); then **`cold{expired}`**. Ages are the larger of the monotonic
//!   and the wall-clock one.
//! - Its usage not in yet: `unmeasured`; zero reads and writes:
//!   `cold{uncached}`.
//!
//! **Ask (on the loop, no await).** States that need no probe are answered
//! at once. Otherwise the cell is read and the job's inputs captured: a
//! `Runtime` clone sharing the session's cell and TTL latches, the history
//! (`Arc` messages), the keep-warm schedule, and what `finish` compares:
//! `prefix_rev`, the tool registry's and the session injection's change
//! counters and which registry the runtime reads.
//!
//! **Job.** The probe awaits the locks the next request's assembly reads
//! (preflight, system prompt, injection, tool projection, image projection),
//! then both change counters are read again (a write during those reads
//! shows there: each is advanced under its write guard), then the TTL.
//!
//! **Finish (on the loop, the step that emits).** The entry is removed
//! first; then `prefix_rev`, both counters, the registry and the whole cell
//! must be what the job saw, or the reply is `stale`; only then is the
//! answer aged and classified, at emit.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::actor::SessionActor;
use super::query::{JobGen, QueryError, QueryErrorCode};
use super::types::{ClientId, SessionEventWire};
use crate::runtime::keepwarm::{CacheAnchor, CacheAnchorState};

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
    /// `warm`: about how long the last accepted request's entry has left
    /// under the TTL the server is known to honour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_for_secs: Option<u64>,
    /// When this session's last possible refresh of the entry ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lapse_by_secs: Option<u64>,
    /// The TTL `warm_for_secs` counts: the one the server is known to
    /// honour for the history entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_ttl_secs: Option<u64>,
    /// The TTL requested for the history entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ttl_secs: Option<u64>,
    /// Cache reads plus writes the provider reported for that request.
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

/// The recommended rendering (daemon-mode.md, v3.1 §4): the TUI's
/// `/keepwarm status` uses it word for word.
pub fn render_cache_expiry(c: &CacheExpiry) -> String {
    // A lower bound rounds down, an upper bound up.
    let at_least = |s: u64| {
        if s < 60 {
            format!("{s}s")
        } else {
            format!("{}m", s / 60)
        }
    };
    let at_most = |s: u64| {
        if s < 60 {
            format!("{s}s")
        } else {
            format!("{}m", s.div_ceil(60))
        }
    };
    let mut out = match (c.state, c.reason) {
        (CacheState::Warm, _) => match c.warm_for_secs {
            Some(s) => format!("Recent cache activity · about {} left", at_least(s)),
            None => "Recent cache activity".to_string(),
        },
        (CacheState::Unknown, Some(CacheReason::MayHaveExpired)) => {
            let mut s = "May have expired".to_string();
            if let Some(l) = c.lapse_by_secs {
                s.push_str(&format!(
                    " · this session's last possible refresh ends in about {}",
                    at_most(l)
                ));
            }
            s
        }
        (CacheState::Cold, Some(CacheReason::Uncached)) => {
            "Last measured request reported no cache activity".to_string()
        }
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
        out.push_str(&format!(
            " · {tokens} tokens read or written by the last measured request"
        ));
    }
    if let Some(n) = c.next_ping_in_secs {
        out.push_str(&format!(
            " · next keep-warm attempt in about {}",
            at_least(n)
        ));
    }
    out
}

/// What a probe job found; aged and classified only at emit (v3.1 §2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CacheEval {
    /// Decided without the cell (`rollover_pending`, `probe_failed`).
    Fixed(CacheReason),
    /// The cell as the probe was compared against it.
    Read {
        cell: CacheAnchorState,
        /// The next request could read what the cell's request left.
        reads_cache: bool,
        /// The TTL the server is known to honour for the history entry.
        honoured: Duration,
        /// The TTL requested for it.
        requested: Duration,
        next_ping_at: Option<Instant>,
    },
}

/// How long ago `then` was at `now`: the larger of the two clocks' (the
/// provider's TTL keeps running while this machine sleeps).
fn age(then: CacheAnchor, now: CacheAnchor) -> Duration {
    let mono = now.mono.saturating_duration_since(then.mono);
    let wall = now.wall.duration_since(then.wall).unwrap_or_default();
    mono.max(wall)
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

impl CacheEval {
    /// The cell this answer was read from, if it read it.
    fn cell(&self) -> Option<CacheAnchorState> {
        match self {
            Self::Fixed(_) => None,
            Self::Read { cell, .. } => Some(*cell),
        }
    }

    /// The reply, aged and classified at `now`.
    pub(crate) fn at(&self, now: CacheAnchor) -> CacheExpiry {
        let (cell, reads_cache, honoured, requested, next_ping_at) = match *self {
            Self::Fixed(reason) => return CacheExpiry::unknown(reason),
            Self::Read {
                cell,
                reads_cache,
                honoured,
                requested,
                next_ping_at,
            } => (cell, reads_cache, honoured, requested, next_ping_at),
        };
        let next_ping_in_secs =
            next_ping_at.map(|t| t.saturating_duration_since(now.mono).as_secs());
        let answer = |state, reason| CacheExpiry {
            state,
            reason: Some(reason),
            next_ping_in_secs,
            ..CacheExpiry::unknown(reason)
        };
        let (Some(anchor), true) = (cell.anchor, reads_cache) else {
            return answer(CacheState::Unknown, CacheReason::NoMatchingRequest);
        };
        let tokens = match cell.usage.map(|u| u.read.saturating_add(u.write)) {
            None => return answer(CacheState::Unknown, CacheReason::Unmeasured),
            Some(0) => return answer(CacheState::Cold, CacheReason::Uncached),
            Some(n) => n,
        };
        let latest = cell
            .dispatched
            .filter(|d| d.mono > anchor.mono)
            .unwrap_or(anchor);
        let warm_left = honoured.saturating_sub(age(anchor, now));
        let lapse_left = requested.max(honoured).saturating_sub(age(latest, now));
        let (state, reason) = if !warm_left.is_zero() {
            (CacheState::Warm, None)
        } else if !lapse_left.is_zero() {
            (CacheState::Unknown, Some(CacheReason::MayHaveExpired))
        } else {
            (CacheState::Cold, Some(CacheReason::Expired))
        };
        CacheExpiry {
            state,
            reason,
            warm_for_secs: (state == CacheState::Warm).then_some(warm_left.as_secs()),
            lapse_by_secs: (state != CacheState::Cold).then(|| ceil_secs(lapse_left)),
            proof_ttl_secs: Some(honoured.as_secs()),
            requested_ttl_secs: Some(requested.as_secs()),
            proof_cache_tokens: Some(tokens),
            next_ping_in_secs,
        }
    }
}

/// What a `CacheExpiry` answer must still match when it is emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpiryAsk {
    prefix_rev: u64,
    inputs: (usize, u64, u64),
}

pub(crate) type ExpiryJobs = super::query::QueryJobs<CacheEval, ExpiryAsk>;

impl SessionActor {
    fn reply_value(&mut self, from: Option<ClientId>, id: u64, value: serde_json::Value) {
        self.reply(from, SessionEventWire::QueryResult { id, value });
    }

    fn reply_expiry(&mut self, from: Option<ClientId>, id: u64, answer: CacheExpiry) {
        let value = serde_json::to_value(answer).unwrap_or_default();
        self.reply_value(from, id, value);
    }

    /// The states a probe cannot change, decided at ask.
    fn cache_expiry_at_ask(&self) -> Option<CacheReason> {
        if self.is_parked() {
            return Some(CacheReason::Parked);
        }
        if self.runtime.cache_keepwarm_ineligibility().is_some() {
            return Some(CacheReason::NotAnthropic);
        }
        if self.streaming
            || self.compact.is_some()
            || !self.pending_prompts.is_empty()
            || self.driver_pending.is_some()
        {
            return Some(CacheReason::Busy);
        }
        if self.runtime.context_head_unresolved() {
            return Some(CacheReason::HeadUnresolved);
        }
        let history = &self.conv.api_messages;
        if history.is_empty() {
            // No request this session sent can be a prefix of nothing.
            return Some(CacheReason::NoMatchingRequest);
        }
        if !crate::runtime::keepwarm::history_ready(history) {
            return Some(CacheReason::HistoryUnsettled);
        }
        None
    }

    /// `Query{CacheExpiry}` from `from` under `id`.
    pub(crate) fn ask_cache_expiry(&mut self, from: Option<ClientId>, id: u64) {
        // A newer ask replaces its requester's older one: that one is
        // answered `superseded` first, then aborted.
        for old in self.expiry_jobs.supersede(from, id) {
            self.reply_value(
                old.from,
                old.id,
                QueryError::new(QueryErrorCode::Superseded).to_value(),
            );
            old.abort();
        }
        if let Some(reason) = self.cache_expiry_at_ask() {
            return self.reply_expiry(from, id, CacheExpiry::unknown(reason));
        }
        // No accepted request whose cached prefix is known (none in this
        // runtime yet, or one that changed accounts mid-request).
        let cell = self.runtime.cache_anchor_state();
        let Some(expect) = cell.prefix else {
            return self.reply_expiry(
                from,
                id,
                CacheExpiry::unknown(CacheReason::NoMatchingRequest),
            );
        };
        if self.expiry_jobs.full() {
            return self.reply_value(from, id, QueryError::new(QueryErrorCode::Busy).to_value());
        }
        let ask = ExpiryAsk {
            prefix_rev: self.prefix_rev,
            inputs: self.runtime.request_input_generations(),
        };
        // A clone sharing the session's cell and TTL latches; it sends
        // nothing.
        let mut runtime = (*self.runtime).clone();
        runtime.share_ttl_latches(&self.runtime);
        let history = self.conv.api_messages.clone();
        let next_ping_at = self
            .cache_keepwarm
            .period
            .as_ref()
            .and_then(|p| p.deadline(Instant::now(), SystemTime::now()));
        let inputs = ask.inputs;
        self.expiry_jobs.spawn(from, id, ask, async move {
            use super::cache_keepwarm::{governing_ttl, honoured_ttl, TtlEvidence};
            #[cfg(any(test, feature = "testing"))]
            testing::job_started().await;
            let probe = crate::runtime::image_limits::without_waiting(
                runtime.cache_expiry_probe(&history, expect),
            )
            .await;
            let probe = match probe {
                Ok(p) => p,
                Err(e)
                    if e.to_string()
                        .contains(crate::runtime::image_limits::IMAGE_GATE_BUSY) =>
                {
                    return Err(QueryError::with_message(
                        QueryErrorCode::Busy,
                        crate::runtime::image_limits::IMAGE_GATE_BUSY,
                    ));
                }
                Err(e) => {
                    tracing::debug!("cache expiry probe failed: {e}");
                    return Ok(CacheEval::Fixed(CacheReason::ProbeFailed));
                }
            };
            // A write during the reads above advanced a counter.
            if runtime.request_input_generations() != inputs {
                return Err(QueryError::new(QueryErrorCode::Stale));
            }
            if probe.rollover {
                return Ok(CacheEval::Fixed(CacheReason::RolloverPending));
            }
            // The TTL keep-warm schedules by: the request's own 1h/5m
            // write split, else the session's latches.
            let configured = runtime.resolved_cache_ttl().await;
            let usage = cell.usage.unwrap_or_default();
            let evidence = TtlEvidence {
                downgraded: runtime.cache_ttl_downgraded(),
                saw_1h: runtime.cache_1h_honoured(),
                write_1h: usage.write_1h.unwrap_or(0),
                write_5m: usage.write_5m.unwrap_or(0),
            };
            let eval = CacheEval::Read {
                cell,
                reads_cache: probe.reads_cache,
                honoured: governing_ttl(honoured_ttl(configured, evidence)),
                requested: governing_ttl(configured),
                next_ping_at,
            };
            #[cfg(any(test, feature = "testing"))]
            testing::job_selected().await;
            Ok(eval)
        });
    }

    /// The job `gen` ended with `outcome`: its one reply.
    pub(crate) fn finish_cache_expiry(
        &mut self,
        gen: JobGen,
        outcome: Result<CacheEval, QueryError>,
    ) {
        let Some(job) = self.expiry_jobs.take(gen) else {
            return;
        };
        let value = match outcome {
            Err(e) => e.to_value(),
            Ok(eval) => {
                let unchanged = self.runtime.is_live()
                    && job.ask.prefix_rev == self.prefix_rev
                    && job.ask.inputs == self.runtime.request_input_generations()
                    && eval
                        .cell()
                        .is_none_or(|cell| cell == self.runtime.cache_anchor_state());
                if unchanged {
                    serde_json::to_value(eval.at(CacheAnchor::now())).unwrap_or_default()
                } else {
                    QueryError::new(QueryErrorCode::Stale).to_value()
                }
            }
        };
        self.reply_value(job.from, job.id, value);
    }
}

/// Test seams: hold a job where it reads its inputs, and know where it is.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::{watch, Semaphore};

    /// Where a job is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum At {
        /// Its probe is about to build the system prompt, whose first read
        /// is the tool registry's.
        SystemPrompt,
        /// Its probe is about to read the session injection.
        Injection,
        /// Its probe has read the session injection.
        InjectionRead,
        /// Its outcome is in (the work and its deadline are over) and not
        /// yet handed to the actor.
        Done,
    }

    #[derive(Clone)]
    struct Gates {
        start: Arc<Semaphore>,
        selected: Arc<Semaphore>,
        reached: Arc<AtomicUsize>,
        seen: Arc<watch::Sender<Vec<At>>>,
        parks: Arc<Mutex<HashMap<At, Arc<Semaphore>>>>,
    }

    static GATES: Mutex<Option<Gates>> = Mutex::new(None);

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// While held, every `CacheExpiry` job waits for a `start` permit
    /// before its probe, and for a `selected` permit once it has its answer
    /// (one each per job). [`Hold::open`] lets jobs through `start`. Jobs
    /// report each [`At`] they reach, and stop at a [`Hold::park`]ed one.
    pub struct Hold {
        pub start: Arc<Semaphore>,
        pub selected: Arc<Semaphore>,
        gates: Gates,
    }

    impl Hold {
        pub fn new() -> Self {
            let gates = Gates {
                start: Arc::new(Semaphore::new(0)),
                selected: Arc::new(Semaphore::new(0)),
                reached: Arc::new(AtomicUsize::new(0)),
                seen: Arc::new(watch::channel(Vec::new()).0),
                parks: Arc::default(),
            };
            *lock(&GATES) = Some(gates.clone());
            Self {
                start: gates.start.clone(),
                selected: gates.selected.clone(),
                gates,
            }
        }

        /// How many jobs have their answer and wait at `selected` (or
        /// passed it).
        pub fn reached(&self) -> usize {
            self.gates.reached.load(Ordering::SeqCst)
        }

        /// Jobs pass `start` freely (they still stop at `selected`).
        pub fn open(self) -> Self {
            self.start.add_permits(Semaphore::MAX_PERMITS / 2);
            self
        }

        /// Jobs that reach `at` from now on stop there, having reported
        /// it, until [`Hold::release`].
        pub fn park(&self, at: At) {
            lock(&self.gates.parks).insert(at, Arc::new(Semaphore::new(0)));
        }

        /// One job parked at `at` goes on.
        pub fn release(&self, at: At) {
            if let Some(park) = lock(&self.gates.parks).get(&at) {
                park.add_permits(1);
            }
        }

        /// A job has reached `at`.
        pub fn has_seen(&self, at: At) -> bool {
            self.gates.seen.borrow().contains(&at)
        }

        /// Until a job reaches `at`.
        pub async fn seen(&self, at: At) {
            let mut seen = self.gates.seen.subscribe();
            let _ = seen.wait_for(|s| s.contains(&at)).await;
        }
    }

    impl Default for Hold {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            *lock(&GATES) = None;
            self.start.close();
            self.selected.close();
            for park in lock(&self.gates.parks).values() {
                park.close();
            }
        }
    }

    fn gates() -> Option<Gates> {
        lock(&GATES).clone()
    }

    pub(super) async fn job_started() {
        if let Some(g) = gates() {
            if let Ok(p) = g.start.acquire().await {
                p.forget();
            }
        }
    }

    pub(super) async fn job_selected() {
        if let Some(g) = gates() {
            g.reached.fetch_add(1, Ordering::SeqCst);
            if let Ok(p) = g.selected.acquire().await {
                p.forget();
            }
        }
    }

    /// The job is at `at`: reported, then parked there if the hold says so.
    pub(crate) async fn at(at: At) {
        let Some(g) = gates() else {
            return;
        };
        let park = lock(&g.parks).get(&at).cloned();
        g.seen.send_modify(|seen| seen.push(at));
        if let Some(park) = park {
            if let Ok(p) = park.acquire().await {
                p.forget();
            }
        }
    }
}

#[cfg(test)]
mod tests;
