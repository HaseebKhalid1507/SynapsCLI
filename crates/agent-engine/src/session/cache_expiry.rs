//! `SessionQuery::CacheExpiry` on the actor (arch-review-435-436-437.md
//! Contract v3 §3.6 + v3.1 §2 + R4-M2): the synchronous ask, the probe job,
//! and `finish`, the one place its reply is emitted.
//!
//! **Ask (on the loop, no await).** States that need no probe are answered
//! at once. Otherwise the job's inputs are captured: a `Runtime` clone (its
//! by-value fields and its `Arc` handles; no lock is taken), the history
//! (`Arc` messages), the keep-warm schedule, and what `finish` compares —
//! `prefix_rev`, the tool registry's and the session injection's change
//! counters and which registry the runtime reads. The clone is no producer
//! and no evidence: it carries no cache evidence, and the probe sends and
//! tickets nothing.
//!
//! **Job.** It awaits the locks the next request's assembly reads (the
//! ping's own path: preflight, system prompt, injection, tool projection,
//! image projection), re-reads both change counters (a write during those
//! reads shows there: each is advanced under its write guard), then reads
//! the store. Its result keeps deadlines as `Instant`s.
//!
//! **Finish (on the loop, the step that emits).** The entry is removed
//! first; then `prefix_rev`, both counters, the registry and the store's
//! version (R4-M2) must be what the job saw, or the reply is `stale`; only
//! then is `now` read and the answer classified and converted to seconds.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::time::Instant;

use super::actor::SessionActor;
use super::query::{JobGen, QueryError, QueryErrorCode};
use super::types::{ClientId, SessionEventWire};
use crate::runtime::cache_evidence::{CacheEval, CacheExpiry, CacheReason};

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
        if self.streaming || self.compact.is_some() || !self.pending_prompts.is_empty() || self.driver_pending.is_some() {
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
            self.reply_value(old.from, old.id, QueryError::new(QueryErrorCode::Superseded).to_value());
            old.abort();
        }
        if let Some(reason) = self.cache_expiry_at_ask() {
            return self.reply_expiry(from, id, CacheExpiry::unknown(reason));
        }
        if self.expiry_jobs.full() {
            return self.reply_value(from, id, QueryError::new(QueryErrorCode::Busy).to_value());
        }
        let ask = ExpiryAsk { prefix_rev: self.prefix_rev, inputs: self.runtime.request_input_generations() };
        let runtime = (*self.runtime).clone();
        let history = self.conv.api_messages.clone();
        let store = Arc::clone(&self.cache_evidence);
        let next_ping_at = self
            .cache_keepwarm
            .period
            .as_ref()
            .and_then(|p| p.deadline(Instant::now(), SystemTime::now()));
        let inputs = ask.inputs;
        self.expiry_jobs.spawn(from, id, ask, async move {
            #[cfg(any(test, feature = "testing"))]
            testing::job_started().await;
            let probe = crate::runtime::image_limits::without_waiting(runtime.cache_expiry_probe(&history)).await;
            let probe = match probe {
                Ok(p) => p,
                Err(e) if e.to_string().contains(crate::runtime::image_limits::IMAGE_GATE_BUSY) => {
                    return Err(QueryError::with_message(QueryErrorCode::Busy, crate::runtime::image_limits::IMAGE_GATE_BUSY));
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
            // After assembly: attempts that ticketed meanwhile count.
            let eval = CacheEval::Matched { selection: store.select(&probe), next_ping_at };
            #[cfg(any(test, feature = "testing"))]
            testing::job_selected().await;
            Ok(eval)
        });
    }

    /// The job `gen` ended with `outcome`: its one reply.
    pub(crate) fn finish_cache_expiry(&mut self, gen: JobGen, outcome: Result<CacheEval, QueryError>) {
        let Some(job) = self.expiry_jobs.take(gen) else {
            return;
        };
        let value = match outcome {
            Err(e) => e.to_value(),
            Ok(eval) => {
                let unchanged = self.runtime.is_live()
                    && job.ask.prefix_rev == self.prefix_rev
                    && job.ask.inputs == self.runtime.request_input_generations();
                match unchanged.then(|| eval.finalize(&self.cache_evidence, Instant::now())).flatten() {
                    Some(answer) => serde_json::to_value(answer).unwrap_or_default(),
                    None => QueryError::new(QueryErrorCode::Stale).to_value(),
                }
            }
        };
        self.reply_value(job.from, job.id, value);
    }
}

/// Test seams: hold a job where it reads its inputs.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Semaphore;

    type Gates = (Arc<Semaphore>, Arc<Semaphore>, Arc<AtomicUsize>);

    static GATES: Mutex<Option<Gates>> = Mutex::new(None);

    /// While held, every `CacheExpiry` job waits for a `start` permit
    /// before its probe, and for a `selected` permit after it read the
    /// store (one each per job). [`Hold::open`] lets jobs through `start`.
    pub struct Hold {
        pub start: Arc<Semaphore>,
        pub selected: Arc<Semaphore>,
        reached: Arc<AtomicUsize>,
    }

    impl Hold {
        pub fn new() -> Self {
            let (start, selected) = (Arc::new(Semaphore::new(0)), Arc::new(Semaphore::new(0)));
            let reached = Arc::new(AtomicUsize::new(0));
            *GATES.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((start.clone(), selected.clone(), reached.clone()));
            Self { start, selected, reached }
        }

        /// How many jobs have read the store and wait at `selected`
        /// (or passed it).
        pub fn reached(&self) -> usize {
            self.reached.load(Ordering::SeqCst)
        }

        /// Jobs pass `start` freely (they still stop at `selected`).
        pub fn open(self) -> Self {
            self.start.add_permits(Semaphore::MAX_PERMITS / 2);
            self
        }
    }

    impl Default for Hold {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            *GATES.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.start.close();
            self.selected.close();
        }
    }

    fn gates() -> Option<Gates> {
        GATES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub(super) async fn job_started() {
        if let Some((start, _, _)) = gates() {
            if let Ok(p) = start.acquire().await {
                p.forget();
            }
        }
    }

    pub(super) async fn job_selected() {
        if let Some((_, selected, reached)) = gates() {
            reached.fetch_add(1, Ordering::SeqCst);
            if let Ok(p) = selected.acquire().await {
                p.forget();
            }
        }
    }
}
