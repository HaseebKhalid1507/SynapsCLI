//! A worker run's request ledger at the transport
//! (`docs/specs/subagent-context-rollover.md` §6; slice 6, dark).
//!
//! One [`RequestLedgerHandle`] per v2 run, shared by everything that sends
//! the run's billable requests (its conversation rounds; its rollover
//! summaries, under their own purpose). Each upstream attempt is one ledger
//! request, so a round that retries has one entry per attempt:
//!
//! - **Reserve** ([`RequestLedgerHandle::reserve`]) right before the attempt
//!   is sent: a fresh host request ID, recorded at once as an open liability
//!   of the attempt's estimated cost. A run at [`Ledger::MAX_ENTRIES`]
//!   refuses the attempt before anything is sent ([`ReserveError::RunFull`]);
//!   the estimate itself never refuses anything.
//! - **Observe** ([`Attempt::observe`]) each usage report the route gives for
//!   the attempt, by the route's [`UsageReports`] semantics.
//! - **Finish** ([`Attempt::finish`]) at the attempt's end: its measured
//!   usage replaces the liability. Without a report, or on a route whose
//!   report semantics are not established, the liability stays open.
//! - **Release** ([`Attempt::release`]) only when the attempt provably cost
//!   nothing: it was never sent, or the transport's typed classification
//!   says the provider charges nothing for its answer. Nothing else (a
//!   proxy's status, a transport failure after the send) is proof.
//! - An attempt dropped unsettled (an error path, a panic, a dropped
//!   producer) keeps its liability open: unknown spend is never lost.
//!
//! Records snapshot the ledger ([`RequestLedgerHandle::snapshot`]), open
//! liabilities included. Only a v2 run gets a handle (worker rollover,
//! enabled since slice 9): runs without one dispatch exactly as before.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::{Arc, Mutex, PoisonError};

use agent_core::{UsageCounts, UsagePurpose, UsageRequest};

use super::record::{
    Amount, Ledger, LedgerEntry, MeasuredUsage, RequestId, RequestPurpose, SettleError,
};

#[derive(Debug, Default)]
struct Shared {
    ledger: Ledger,
    /// Request IDs minted so far: each attempt's is the next, never reused.
    minted: u64,
}

/// How a route reports one attempt's usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageReports {
    /// Each report states the attempt's usage so far: the last one is the
    /// attempt's.
    Cumulative,
    /// Each report states usage since the previous one: the attempt's is
    /// their sum.
    Deltas,
    /// Not established: whatever is reported, the liability stays open.
    Unknown,
}

/// Why an attempt was not reserved; nothing was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReserveError {
    /// The run's ledger is full ([`Ledger::MAX_ENTRIES`] requests).
    RunFull,
    /// The ledger refused the new request (never expected: each request ID
    /// is fresh).
    Refused(SettleError),
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RunFull => write!(
                f,
                "this run reached {} provider requests; no further request is sent",
                Ledger::MAX_ENTRIES
            ),
            Self::Refused(e) => write!(f, "the run's request ledger refused a request: {e:?}"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// How an attempt left the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptEnd {
    /// Its measured usage replaced its liability.
    Measured,
    /// Its liability stays open (unknown spend).
    Open,
    /// Its liability was released.
    Released,
}

/// A run's request ledger, shared by every sender of the run's requests;
/// each clone attributes its requests to its own model and purpose.
#[derive(Clone, Debug)]
pub(crate) struct RequestLedgerHandle {
    shared: Arc<Mutex<Shared>>,
    model: String,
    purpose: RequestPurpose,
}

impl RequestLedgerHandle {
    /// A new run's empty ledger; its requests are `model`'s conversation.
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            shared: Arc::default(),
            model: model.into(),
            purpose: RequestPurpose::Conversation,
        }
    }

    /// The same ledger, for requests to `model` made for `purpose` (a
    /// rollover summary, possibly on the worker's own model).
    pub(crate) fn for_requests(&self, model: impl Into<String>, purpose: RequestPurpose) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            model: model.into(),
            purpose,
        }
    }

    /// The same ledger, for a rollover summary's requests to `model`.
    pub(crate) fn for_summary(&self, model: impl Into<String>) -> Self {
        self.for_requests(model, RequestPurpose::RolloverSummary)
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn purpose(&self) -> RequestPurpose {
        self.purpose
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserve the next attempt, right before it is sent: a fresh request ID
    /// with an open liability of `estimate_micro_usd`. Refused only when the
    /// run's ledger is full; then nothing may be sent.
    pub(crate) fn reserve(
        &self,
        estimate_micro_usd: u64,
        reports: UsageReports,
    ) -> Result<Attempt, ReserveError> {
        let mut shared = self.lock();
        let n = shared.minted + 1;
        let request_id =
            RequestId::parse(&format!("req-{n}")).expect("a minted request ID is valid");
        let entry = LedgerEntry {
            request_id: request_id.clone(),
            purpose: self.purpose,
            model: self.model.clone(),
            amount: Amount::Liability { estimate_micro_usd },
        };
        match shared.ledger.settle(entry) {
            Ok(_) => {
                shared.minted = n;
                Ok(Attempt {
                    shared: Arc::clone(&self.shared),
                    request_id,
                    model: self.model.clone(),
                    purpose: self.purpose,
                    estimate_micro_usd,
                    reports,
                    observed: None,
                })
            }
            Err(SettleError::Full) => Err(ReserveError::RunFull),
            Err(e) => Err(ReserveError::Refused(e)),
        }
    }

    /// The ledger as it stands, open liabilities included.
    pub(crate) fn snapshot(&self) -> Ledger {
        self.lock().ledger.clone()
    }
}

/// Where a worker's runtime finds its run's ledger. The launch arms an
/// empty slot on the runtime before the run (`launch::arm_request_ledger`)
/// and installs the ledger once the run is a v2 run; a run that falls back
/// to rollover off leaves it empty, so its requests dispatch exactly as
/// before. Clones of the runtime share the slot: the stream runs on one.
#[derive(Clone, Debug, Default)]
pub(crate) struct LedgerSlot(Arc<std::sync::OnceLock<RequestLedgerHandle>>);

impl LedgerSlot {
    /// Install the run's ledger; a second install is refused (`false`) and
    /// leaves the first.
    pub(crate) fn install(&self, ledger: RequestLedgerHandle) -> bool {
        self.0.set(ledger).is_ok()
    }

    /// The run's ledger, once installed.
    pub(crate) fn get(&self) -> Option<RequestLedgerHandle> {
        self.0.get().cloned()
    }
}

/// One reserved upstream attempt. Dropped unsettled, its liability stays
/// open.
#[derive(Debug)]
pub(crate) struct Attempt {
    shared: Arc<Mutex<Shared>>,
    request_id: RequestId,
    model: String,
    purpose: RequestPurpose,
    estimate_micro_usd: u64,
    reports: UsageReports,
    observed: Option<MeasuredUsage>,
}

impl Attempt {
    pub(crate) fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub(crate) fn estimate_micro_usd(&self) -> u64 {
        self.estimate_micro_usd
    }

    /// This attempt's identity in the run's ledger events
    /// (`SessionEvent::UsageAttempt` / `UsageLiability`).
    pub(crate) fn usage_request(&self) -> UsageRequest {
        UsageRequest {
            request_id: self.request_id.as_str().to_owned(),
            purpose: match self.purpose {
                RequestPurpose::Conversation => UsagePurpose::Conversation,
                RequestPurpose::RolloverSummary => UsagePurpose::RolloverSummary,
            },
            model: self.model.clone(),
        }
    }

    /// One usage report the route gave for this attempt.
    pub(crate) fn observe(&mut self, usage: MeasuredUsage) {
        self.observed = Some(match (self.reports, self.observed.take()) {
            (UsageReports::Deltas, Some(sum)) => add(sum, usage),
            _ => usage,
        });
    }

    /// The attempt ended after it was sent: its measured usage replaces its
    /// liability, which otherwise stays open (nothing reported, or report
    /// semantics not established).
    pub(crate) fn finish(mut self) -> AttemptEnd {
        let usage = match (self.reports, self.observed.take()) {
            (UsageReports::Unknown, _) | (_, None) => return AttemptEnd::Open,
            (_, Some(usage)) => usage,
        };
        self.settle(Amount::Measured { usage }, AttemptEnd::Measured)
    }

    /// The attempt provably cost nothing (never sent, or a no-charge answer
    /// by the transport's typed classification): its liability is released.
    pub(crate) fn release(self) -> AttemptEnd {
        self.settle(Amount::Released {}, AttemptEnd::Released)
    }

    fn settle(&self, amount: Amount, end: AttemptEnd) -> AttemptEnd {
        let entry = LedgerEntry {
            request_id: self.request_id.clone(),
            purpose: self.purpose,
            model: self.model.clone(),
            amount,
        };
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        match shared.ledger.settle(entry) {
            Ok(_) => end,
            Err(_) => AttemptEnd::Open,
        }
    }
}

/// A ledgered upstream attempt at a transport, with the events that report
/// it on the request's stream (`SessionEvent::UsageAttempt` in place of the
/// plain `Usage`; `SessionEvent::UsageLiability` when its spend is not
/// measured). Reserved right before the attempt is sent
/// ([`AttemptReport::reserve`]); its final usage report is observed
/// ([`AttemptReport::observe`]); it ends measured or open
/// ([`AttemptReport::finish`]) or released ([`AttemptReport::release`]).
/// Dropped before it ends (an error path, a cancel, a panic), its liability
/// stays open and is reported as open.
#[derive(Debug)]
pub(crate) struct AttemptReport {
    attempt: Option<Attempt>,
    request: UsageRequest,
    estimate_micro_usd: u64,
    tx: tokio::sync::mpsc::UnboundedSender<agent_core::StreamEvent>,
}

impl AttemptReport {
    /// Reserve the next attempt of `ledger`'s run, reporting on `tx`.
    pub(crate) fn reserve(
        ledger: &RequestLedgerHandle,
        estimate_micro_usd: u64,
        reports: UsageReports,
        tx: &tokio::sync::mpsc::UnboundedSender<agent_core::StreamEvent>,
    ) -> Result<Self, ReserveError> {
        let attempt = ledger.reserve(estimate_micro_usd, reports)?;
        Ok(Self {
            request: attempt.usage_request(),
            estimate_micro_usd,
            attempt: Some(attempt),
            tx: tx.clone(),
        })
    }

    /// The attempt's identity in its events.
    pub(crate) fn request(&self) -> &UsageRequest {
        &self.request
    }

    /// One usage report of the attempt that states its spend (by the
    /// route's [`UsageReports`] semantics); a running count is not one.
    pub(crate) fn observe(&mut self, usage: &UsageCounts) {
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.observe(measured(usage));
        }
    }

    /// The attempt ended after its send: measured by what was observed, or
    /// open (reported as such).
    pub(crate) fn finish(mut self) -> AttemptEnd {
        let end = match self.attempt.take() {
            Some(attempt) => attempt.finish(),
            None => AttemptEnd::Open,
        };
        if end != AttemptEnd::Measured {
            self.liability(agent_core::LiabilityState::Open);
        }
        end
    }

    /// The attempt provably cost nothing (never sent, or a no-charge answer
    /// by the transport's typed classification).
    pub(crate) fn release(mut self) -> AttemptEnd {
        let end = match self.attempt.take() {
            Some(attempt) => attempt.release(),
            None => AttemptEnd::Open,
        };
        self.liability(match end {
            AttemptEnd::Released => agent_core::LiabilityState::Released,
            _ => agent_core::LiabilityState::Open,
        });
        end
    }

    fn liability(&self, state: agent_core::LiabilityState) {
        let _ = self.tx.send(agent_core::StreamEvent::Session(
            agent_core::SessionEvent::UsageLiability {
                request: self.request.clone(),
                estimate_micro_usd: self.estimate_micro_usd,
                state,
            },
        ));
    }
}

impl Drop for AttemptReport {
    fn drop(&mut self) {
        if self.attempt.take().is_some() {
            self.liability(agent_core::LiabilityState::Open);
        }
    }
}

/// The usage event of one report: the attempt's `UsageAttempt` when the
/// request is ledgered, otherwise exactly the plain `Usage` the transport
/// has always sent (`model` as before).
pub(crate) fn usage_event(
    request: Option<&UsageRequest>,
    usage: UsageCounts,
    model: Option<String>,
) -> agent_core::SessionEvent {
    match request {
        Some(request) => agent_core::SessionEvent::UsageAttempt {
            request: request.clone(),
            usage,
        },
        None => agent_core::SessionEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_creation_5m: usage.cache_creation_5m,
            cache_creation_1h: usage.cache_creation_1h,
            model,
        },
    }
}

/// A transport's stream event for a ledgered attempt (§6): its plain
/// `Usage` becomes the attempt's `UsageAttempt` (never both); every other
/// event, and every event of a request without a ledger, is unchanged.
pub(crate) fn attempt_event(
    event: agent_core::StreamEvent,
    request: Option<&UsageRequest>,
) -> agent_core::StreamEvent {
    match (request, event) {
        (
            Some(request),
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model: _,
            }),
        ) => agent_core::StreamEvent::Session(agent_core::SessionEvent::UsageAttempt {
            request: request.clone(),
            usage: UsageCounts {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
            },
        }),
        (_, event) => event,
    }
}

/// A request's estimated cost in micro-USD (§6): its serialized `body` by
/// the conservative token estimate at `model`'s input rate, plus its whole
/// output allowance at the output rate.
pub(crate) fn request_estimate(model: &str, body: &[u8], max_output_tokens: u64) -> u64 {
    let input = crate::runtime::context::conservative_token_estimate(&String::from_utf8_lossy(body));
    estimate_micro_usd(model, input, max_output_tokens)
}

/// The sum of two usage reports of one attempt, or of two attempts'
/// measured usage (saturating). A cache-write split known in either is
/// known in the sum.
fn add(a: MeasuredUsage, b: MeasuredUsage) -> MeasuredUsage {
    let split = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
    };
    MeasuredUsage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        cache_read_tokens: a.cache_read_tokens.saturating_add(b.cache_read_tokens),
        cache_write_tokens: a.cache_write_tokens.saturating_add(b.cache_write_tokens),
        cache_write_5m: split(a.cache_write_5m, b.cache_write_5m),
        cache_write_1h: split(a.cache_write_1h, b.cache_write_1h),
    }
}

/// A usage event's counts as the ledger's measured usage.
pub(crate) fn measured(counts: &UsageCounts) -> MeasuredUsage {
    MeasuredUsage {
        input_tokens: counts.input_tokens,
        output_tokens: counts.output_tokens,
        cache_read_tokens: counts.cache_read_input_tokens,
        cache_write_tokens: counts.cache_creation_input_tokens,
        cache_write_5m: counts.cache_creation_5m,
        cache_write_1h: counts.cache_creation_1h,
    }
}

/// The ledger's measured usage as a usage event's counts.
pub(crate) fn counts(usage: &MeasuredUsage) -> UsageCounts {
    UsageCounts {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_tokens,
        cache_creation_input_tokens: usage.cache_write_tokens,
        cache_creation_5m: usage.cache_write_5m,
        cache_creation_1h: usage.cache_write_1h,
    }
}

/// An attempt's estimated cost in micro-USD, rounded up: `input_tokens` at
/// the model's full input rate and its whole output allowance
/// `max_output_tokens` at the output rate. A liability amount only: it is
/// never a reason to refuse a request (a user-set `max_cost_usd` is the
/// only cost limit).
pub(crate) fn estimate_micro_usd(model: &str, input_tokens: u64, max_output_tokens: u64) -> u64 {
    micro_usd_ceil(agent_core::pricing::calculate_cost(
        model,
        input_tokens,
        max_output_tokens,
        0,
        0,
    ))
}

/// `usd` in whole micro-USD, rounded up and saturating. An amount that is
/// not finite is never free: positive infinity, or anything that is not a
/// number, is `u64::MAX`, like any amount beyond `u64`; zero or less is
/// zero. Floating-point rounding can put the result one micro-USD above the
/// exact amount, not below it (for token counts where `f64` is exact).
pub(crate) fn micro_usd_ceil(usd: f64) -> u64 {
    if usd.is_nan() {
        return u64::MAX;
    }
    if usd <= 0.0 {
        return 0;
    }
    let micro = (usd * 1_000_000.0).ceil();
    if micro >= u64::MAX as f64 {
        u64::MAX
    } else {
        micro as u64
    }
}

/// A v2 oneshot's aggregate usage (§6 "Slice 8", 8f): the measured
/// conversation entries of the run's `ledger` on the worker's `model`,
/// summed. A rollover summary (on any model), a conversation request on
/// another model, an open liability and a released attempt add nothing;
/// each attempt counts once, however often its report was delivered.
pub(crate) fn conversation_aggregate(ledger: &Ledger, model: &str) -> MeasuredUsage {
    ledger
        .entries()
        .filter(|e| e.purpose == RequestPurpose::Conversation && e.model == model)
        .filter_map(|e| match &e.amount {
            Amount::Measured { usage } => Some(usage.clone()),
            Amount::Liability { .. } | Amount::Released {} => None,
        })
        .fold(MeasuredUsage::default(), add)
}

/// Whether the aggregate usage of the v2 run `run_id` may be published
/// now (8f): yes the first time in this process, never again after. The
/// mark is in-process only (§6): a process that restarts forgets it, and
/// slice 10's recovery adds a durable one.
pub(crate) fn claim_aggregate(run_id: &str) -> bool {
    static CLAIMED: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    CLAIMED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(run_id.to_owned())
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
