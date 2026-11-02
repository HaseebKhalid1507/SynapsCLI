//! Child spend: each paid request a worker makes reaches its root session
//! once, in order, as it is reported.
//!
//! - [`SpendSink`]: one per root session, owned by the session. It holds the
//!   publication gate (phase and live producer count) and the sending half of
//!   an unbounded channel; the session reads the other half.
//! - [`RunLease`]: one per launch of a worker (a start, each resume, a
//!   oneshot). Clones share one run. The run publishes its reports with a
//!   sequence number that starts at 0 and has no gaps, then exactly one
//!   terminal message: [`SinkMessage::RunSealed`] when it is sealed (its
//!   publication cut off while its worker may still run), or
//!   [`SinkMessage::RunEnded`] when its last clone is dropped.
//!
//! Locks: a run's mutex first, then the sink's gate; nothing takes them in
//! the other order, and closing the sink takes only the gate. Under the
//! locks nothing awaits, logs or does I/O: the only thing done under the
//! gate is a non-blocking send on the unbounded channel. So the gate check,
//! the sequence number and the enqueue are one step, and a close can't slip
//! between them. Outcomes are logged after both locks are released. A
//! poisoned lock is recovered (the state it guards is updated in single
//! steps), and nothing here panics in `Drop`.
//!
//! Nothing books through this yet.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;

/// The longest agent label a report carries, in characters.
pub const MAX_AGENT_CHARS: usize = 64;

/// What a request was paid for. An open set on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendKind {
    /// A conversation request of the worker: every attempt, retries and
    /// failover included.
    Turn,
    /// The worker's rollover summary (warm, screened or cold).
    Summary,
}

impl SpendKind {
    /// The wire name of the kind.
    pub fn wire_name(self) -> &'static str {
        match self {
            SpendKind::Turn => "turn",
            SpendKind::Summary => "summary",
        }
    }
}

/// One request's reported tokens, in the Anthropic split on every route:
/// `input` excludes cache reads and writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    /// The TTL split of `cache_creation`, when the route reports one.
    pub cache_creation_5m: Option<u64>,
    pub cache_creation_1h: Option<u64>,
}

/// One paid request, priced.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendReport {
    pub kind: SpendKind,
    /// The qualified model the request was priced at.
    pub model: String,
    pub tokens: SpendTokens,
    /// The price in integer nanodollars, rounded up the same way the
    /// session's usage counters round.
    pub cost_nanousd: u64,
}

impl SpendReport {
    /// Price one request at `model`, with the pricing table the session's
    /// own bookings use (an unknown model keeps the table's fallback).
    pub fn priced(kind: SpendKind, model: impl Into<String>, tokens: SpendTokens) -> Self {
        let model = model.into();
        let usd = agent_core::pricing::calculate_cost_optional_split(
            &model,
            tokens.input,
            tokens.output,
            tokens.cache_read,
            tokens.cache_creation,
            tokens.cache_creation_5m,
            tokens.cache_creation_1h,
        );
        Self {
            kind,
            model,
            tokens,
            cost_nanousd: crate::runtime::budget::UsageCounters::nanodollars(usd),
        }
    }

    /// The price in USD, for display.
    pub fn cost_usd(&self) -> f64 {
        self.cost_nanousd as f64 / 1_000_000_000.0
    }
}

/// Who a run's reports belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    /// The worker's handle as status and collect show it.
    pub handle: String,
    /// Fresh per launch, so a resume is a new run.
    pub run_nonce: u64,
    /// The worker's agent label, flattened and bounded.
    pub agent: String,
    /// The worker that delegated this one; `None` when the session's own
    /// turn did.
    pub parent_handle: Option<String>,
}

impl RunIdentity {
    fn new(handle: &str, agent: &str, parent_handle: Option<String>) -> Self {
        Self {
            handle: handle.to_string(),
            run_nonce: rand::random::<u64>(),
            agent: bounded_agent(agent),
            parent_handle,
        }
    }

    /// The run nonce as 16 lowercase hex digits.
    pub fn run_hex(&self) -> String {
        format!("{:016x}", self.run_nonce)
    }
}

/// Control characters and line separators become spaces; then trimmed and
/// cut to [`MAX_AGENT_CHARS`] characters.
fn bounded_agent(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    flat.trim().chars().take(MAX_AGENT_CHARS).collect()
}

/// What the session receives, in the order the runs published it.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkMessage {
    /// One paid request. `seq` is 0, 1, 2… per run.
    Spend {
        run: Arc<RunIdentity>,
        seq: u64,
        report: SpendReport,
    },
    /// The run's publication was cut off while its worker may still run.
    /// `next_seq` is the number of reports it published.
    RunSealed { run: Arc<RunIdentity>, next_seq: u64 },
    /// The run's last lease was dropped. `next_seq` as for `RunSealed`.
    RunEnded { run: Arc<RunIdentity>, next_seq: u64 },
}

/// The session's end of the channel.
pub type SpendReceiver = mpsc::UnboundedReceiver<SinkMessage>;

/// The publication gate's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkPhase {
    /// Reports are published and new runs are admitted.
    Open,
    /// The session is ending: reports are still published, new runs are
    /// refused.
    Closing,
    /// Nothing more is published.
    Closed,
}

/// Why a new run was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRefused {
    /// The delegating run was sealed or has ended.
    ParentSealed,
    /// The session is ending.
    SessionEnding,
}

impl std::fmt::Display for LeaseRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LeaseRefused::ParentSealed => "parent run sealed: no new subagent work",
            LeaseRefused::SessionEnding => "session ending: no new subagent work",
        })
    }
}

impl std::error::Error for LeaseRefused {}

/// What [`RunLease::book`] did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookOutcome {
    /// Published with this sequence number.
    Booked { seq: u64 },
    /// Not published (logged as late spend).
    NotBooked(NotBooked),
    /// The session's receiver is gone; nothing more will be published.
    ReceiverGone,
}

/// Why a report was not published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotBooked {
    /// The run was sealed.
    RunNotLive,
    /// The sink is closed.
    SessionClosed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct Gate {
    phase: SinkPhase,
    /// Live leases: admitted and neither sealed nor ended.
    producers: u64,
    tx: mpsc::UnboundedSender<SinkMessage>,
}

impl Gate {
    /// Enqueue unless closed. When the receiver is gone the gate closes, so
    /// later callers see a closed sink.
    fn send(&mut self, message: SinkMessage) -> SendResult {
        if self.phase == SinkPhase::Closed {
            return SendResult::Closed;
        }
        match self.tx.send(message) {
            Ok(()) => SendResult::Sent,
            Err(_) => {
                self.phase = SinkPhase::Closed;
                SendResult::ReceiverGone
            }
        }
    }
}

enum SendResult {
    Sent,
    Closed,
    ReceiverGone,
}

#[derive(Debug)]
struct SinkInner {
    gate: Mutex<Gate>,
}

/// The session's publication point for its workers' spend.
#[derive(Debug, Clone)]
pub struct SpendSink {
    inner: Arc<SinkInner>,
}

impl SpendSink {
    /// A new open sink and the receiver the session reads.
    pub fn new() -> (Self, SpendReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = Self {
            inner: Arc::new(SinkInner {
                gate: Mutex::new(Gate {
                    phase: SinkPhase::Open,
                    producers: 0,
                    tx,
                }),
            }),
        };
        (sink, rx)
    }

    pub fn phase(&self) -> SinkPhase {
        lock(&self.inner.gate).phase
    }

    /// Live leases (admitted, neither sealed nor ended).
    pub fn producers(&self) -> u64 {
        lock(&self.inner.gate).producers
    }

    /// Open → Closing: new runs are refused, reports still publish. A
    /// closed sink stays closed. Returns the phase before.
    pub fn begin_closing(&self) -> SinkPhase {
        let mut gate = lock(&self.inner.gate);
        let before = gate.phase;
        if before == SinkPhase::Open {
            gate.phase = SinkPhase::Closing;
        }
        before
    }

    /// Nothing more is published. Returns the phase before.
    pub fn close(&self) -> SinkPhase {
        let mut gate = lock(&self.inner.gate);
        std::mem::replace(&mut gate.phase, SinkPhase::Closed)
    }

    /// Closing → Open: a session that stopped admitting runs for a reload
    /// checkpoint continues in this process after all (the exec failed). A
    /// closed sink stays closed. Returns the phase before.
    pub fn reopen(&self) -> SinkPhase {
        let mut gate = lock(&self.inner.gate);
        let before = gate.phase;
        if before == SinkPhase::Closing {
            gate.phase = SinkPhase::Open;
        }
        before
    }

    /// A lease for a worker the session's own turn delegated.
    pub fn lease(&self, handle: &str, agent: &str) -> Result<RunLease, LeaseRefused> {
        admit(&self.inner, RunIdentity::new(handle, agent, None))
    }

    /// Whether two sinks are the same session's.
    pub fn same_sink(&self, other: &SpendSink) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Count a new producer under the gate, unless the sink is past Open.
/// A caller admitting a child holds the parent's run mutex (run → gate).
fn admit(sink: &Arc<SinkInner>, identity: RunIdentity) -> Result<RunLease, LeaseRefused> {
    {
        let mut gate = lock(&sink.gate);
        if gate.phase != SinkPhase::Open {
            return Err(LeaseRefused::SessionEnding);
        }
        gate.producers = gate.producers.saturating_add(1);
    }
    Ok(RunLease {
        inner: Arc::new(LeaseInner {
            sink: Arc::clone(sink),
            identity: Arc::new(identity),
            run: Mutex::new(Run {
                state: RunState::Live,
                next_seq: 0,
            }),
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Live,
    Sealed,
    Ended,
}

#[derive(Debug)]
struct Run {
    state: RunState,
    /// The sequence number of the next report.
    next_seq: u64,
}

#[derive(Debug)]
struct LeaseInner {
    sink: Arc<SinkInner>,
    identity: Arc<RunIdentity>,
    run: Mutex<Run>,
}

/// One launch's right to publish spend. Clones share the run.
#[derive(Debug, Clone)]
pub struct RunLease {
    inner: Arc<LeaseInner>,
}

impl RunLease {
    pub fn identity(&self) -> &RunIdentity {
        &self.inner.identity
    }

    /// The sink this run publishes to.
    pub fn sink(&self) -> SpendSink {
        SpendSink {
            inner: Arc::clone(&self.inner.sink),
        }
    }

    /// Whether the run still publishes.
    pub fn is_live(&self) -> bool {
        lock(&self.inner.run).state == RunState::Live
    }

    /// A lease for a worker this run delegates. Refused once this run is
    /// sealed (its parent may already have been allowed to stop waiting
    /// for it) or the session is ending.
    pub fn child(&self, handle: &str, agent: &str) -> Result<RunLease, LeaseRefused> {
        let run = lock(&self.inner.run);
        if run.state != RunState::Live {
            return Err(LeaseRefused::ParentSealed);
        }
        let identity = RunIdentity::new(handle, agent, Some(self.inner.identity.handle.clone()));
        let lease = admit(&self.inner.sink, identity);
        drop(run);
        lease
    }

    /// Publish one report. A sealed run or a closed sink publishes
    /// nothing and logs the report as late spend.
    pub fn book(&self, report: SpendReport) -> BookOutcome {
        let kind = report.kind;
        let cost = report.cost_usd();
        let outcome = {
            let mut run = lock(&self.inner.run);
            if run.state != RunState::Live {
                BookOutcome::NotBooked(NotBooked::RunNotLive)
            } else {
                let mut gate = lock(&self.inner.sink.gate);
                let seq = run.next_seq;
                let message = SinkMessage::Spend {
                    run: Arc::clone(&self.inner.identity),
                    seq,
                    report,
                };
                match gate.send(message) {
                    SendResult::Sent => {
                        run.next_seq = seq.saturating_add(1);
                        BookOutcome::Booked { seq }
                    }
                    SendResult::Closed => BookOutcome::NotBooked(NotBooked::SessionClosed),
                    SendResult::ReceiverGone => BookOutcome::ReceiverGone,
                }
            }
        };
        let who = &self.inner.identity;
        match outcome {
            BookOutcome::Booked { .. } => {}
            BookOutcome::NotBooked(reason) => tracing::warn!(
                handle = %who.handle,
                run = %who.run_hex(),
                kind = kind.wire_name(),
                cost_usd = cost,
                "late spend not booked: {} ${:.6} ({})",
                who.handle,
                cost,
                match reason {
                    NotBooked::RunNotLive => "run sealed",
                    NotBooked::SessionClosed => "session closed",
                },
            ),
            BookOutcome::ReceiverGone => tracing::error!(
                handle = %who.handle,
                run = %who.run_hex(),
                kind = kind.wire_name(),
                cost_usd = cost,
                "child spend not delivered: the session's spend receiver is gone ({} ${:.6})",
                who.handle,
                cost,
            ),
        }
        outcome
    }

    /// Cut the run's publication off while its worker may still run. The
    /// session gets `RunSealed` after every report already published.
    /// Idempotent: `true` only for the call that sealed it.
    pub fn seal(&self) -> bool {
        let gone = {
            let mut run = lock(&self.inner.run);
            if run.state != RunState::Live {
                return false;
            }
            run.state = RunState::Sealed;
            let mut gate = lock(&self.inner.sink.gate);
            gate.producers = gate.producers.saturating_sub(1);
            let message = SinkMessage::RunSealed {
                run: Arc::clone(&self.inner.identity),
                next_seq: run.next_seq,
            };
            matches!(gate.send(message), SendResult::ReceiverGone)
        };
        if gone {
            receiver_gone(&self.inner.identity, "seal");
        }
        true
    }
}

fn receiver_gone(who: &RunIdentity, what: &str) {
    tracing::error!(
        handle = %who.handle,
        run = %who.run_hex(),
        "child spend {what} not delivered: the session's spend receiver is gone ({})",
        who.handle,
    );
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        let run = self.run.get_mut().unwrap_or_else(PoisonError::into_inner);
        if run.state != RunState::Live {
            return;
        }
        run.state = RunState::Ended;
        let next_seq = run.next_seq;
        let gone = {
            let mut gate = lock(&self.sink.gate);
            gate.producers = gate.producers.saturating_sub(1);
            let message = SinkMessage::RunEnded {
                run: Arc::clone(&self.identity),
                next_seq,
            };
            matches!(gate.send(message), SendResult::ReceiverGone)
        };
        if gone {
            receiver_gone(&self.identity, "end");
        }
    }
}

// ═══ Per-attempt booking ═══════════════════════════════════════════════════

/// A worker runtime's right to book its own paid requests: its run lease.
/// Only worker runtimes in a host with a sink have one.
#[derive(Debug, Clone)]
pub struct SpendTap {
    lease: RunLease,
}

impl SpendTap {
    pub fn new(lease: RunLease) -> Self {
        Self { lease }
    }

    pub fn lease(&self) -> &RunLease {
        &self.lease
    }

    /// Price one request at `model` (outside every lock) and publish it.
    pub fn book(&self, kind: SpendKind, model: &str, tokens: SpendTokens) -> BookOutcome {
        self.lease.book(SpendReport::priced(kind, model, tokens))
    }
}

/// What a routed transport needs to guard its attempts: the runtime's tap
/// (none for the root, summaries, keep-warm and hosts without a sink) and
/// the qualified model the request was made for, which its reports are
/// priced at.
#[derive(Debug, Clone, Copy)]
pub struct RouteSpend<'a> {
    pub tap: Option<&'a SpendTap>,
    pub model: &'a str,
}

impl RouteSpend<'static> {
    /// No tap: every attempt's guard is inert.
    pub const NONE: Self = Self {
        tap: None,
        model: "",
    };
}

impl RouteSpend<'_> {
    /// A guard for one of the route's attempts (a request turn).
    pub fn attempt(&self) -> AttemptSpend {
        AttemptSpend::new(self.tap, SpendKind::Turn, self.model)
    }

    /// Book one attempt that has already returned with `usage` (Anthropic
    /// field names); an absent or count-less report books nothing.
    pub fn book_returned(&self, usage: Option<&serde_json::Value>) {
        let attempt = self.attempt();
        if let Some(usage) = usage {
            attempt.observe(&ReportedUsage::from_anthropic_json(usage));
        }
        attempt.settle();
    }
}

/// One usage report as a route parsed it: each count only if the report
/// stated it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportedUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub cache_creation_5m: Option<u64>,
    pub cache_creation_1h: Option<u64>,
}

impl ReportedUsage {
    /// Read a usage object in the Anthropic field names, which every route
    /// normalizes to (`input_tokens`, `output_tokens`,
    /// `cache_read_input_tokens`, `cache_creation_input_tokens`, and the
    /// optional `cache_creation.ephemeral_5m_input_tokens` /
    /// `ephemeral_1h_input_tokens` split). A count that is absent, null or
    /// not an unsigned integer is not stated.
    pub fn from_anthropic_json(usage: &serde_json::Value) -> Self {
        let count = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64);
        let split = |key: &str| {
            usage
                .get("cache_creation")
                .and_then(|c| c.get(key))
                .and_then(serde_json::Value::as_u64)
        };
        Self {
            input: count("input_tokens"),
            output: count("output_tokens"),
            cache_read: count("cache_read_input_tokens"),
            cache_creation: count("cache_creation_input_tokens"),
            cache_creation_5m: split("ephemeral_5m_input_tokens"),
            cache_creation_1h: split("ephemeral_1h_input_tokens"),
        }
    }

    /// Whether the report states at least one of the four counts.
    pub fn has_counts(&self) -> bool {
        self.input.is_some()
            || self.output.is_some()
            || self.cache_read.is_some()
            || self.cache_creation.is_some()
    }

    /// The counts as booked: a count the report did not state is 0; the TTL
    /// split only when the report stated it.
    pub fn tokens(&self) -> SpendTokens {
        SpendTokens {
            input: self.input.unwrap_or(0),
            output: self.output.unwrap_or(0),
            cache_read: self.cache_read.unwrap_or(0),
            cache_creation: self.cache_creation.unwrap_or(0),
            cache_creation_5m: self.cache_creation_5m,
            cache_creation_1h: self.cache_creation_1h,
        }
    }
}

#[derive(Debug)]
struct Slot {
    tap: SpendTap,
    kind: SpendKind,
    model: String,
    retained: Option<SpendTokens>,
    settled: bool,
}

impl Slot {
    /// Take what is to be booked, once; later calls get nothing.
    fn take_for_settlement(&mut self) -> Option<SpendTokens> {
        if self.settled {
            return None;
        }
        self.settled = true;
        self.retained.take()
    }
}

/// One paid attempt's spend, settled exactly once at the attempt's real end.
///
/// - [`observe`](Self::observe): a report that states at least one count
///   replaces what is retained, as a whole (a snapshot, never summed with
///   an earlier one); a report with no counts is ignored; an explicit zero
///   is a report and replaces.
/// - [`settle`](Self::settle) books what is retained, once (nothing when
///   no report was retained). A route calls it before any backoff,
///   failover or next dispatch.
/// - Dropping the guard without settling does the same, synchronously (no
///   await, no I/O), so a cancel, an error return or a dropped future books
///   what was reported.
/// - Without a tap the guard is inert: nothing is retained or booked.
///
/// [`observer`](Self::observer) hands a clone to a task that sees the
/// reports (an extension stream forwarder); once the guard is settled the
/// observer's reports are ignored, so a late report can't re-arm it or be
/// booked twice. Only the guard settles; an observer's drop books nothing.
#[derive(Debug)]
pub struct AttemptSpend {
    slot: Option<Arc<Mutex<Slot>>>,
}

/// A clone of an [`AttemptSpend`]'s slot that can only observe.
#[derive(Debug, Clone)]
pub struct AttemptObserver {
    slot: Option<Arc<Mutex<Slot>>>,
}

fn observe_slot(slot: &Option<Arc<Mutex<Slot>>>, report: &ReportedUsage) {
    let Some(slot) = slot else { return };
    if !report.has_counts() {
        return;
    }
    let mut slot = lock(slot);
    if !slot.settled {
        slot.retained = Some(report.tokens());
    }
}

impl AttemptSpend {
    /// A guard for one attempt at `model`; inert without a tap.
    pub fn new(tap: Option<&SpendTap>, kind: SpendKind, model: &str) -> Self {
        Self {
            slot: tap.map(|tap| {
                Arc::new(Mutex::new(Slot {
                    tap: tap.clone(),
                    kind,
                    model: model.to_string(),
                    retained: None,
                    settled: false,
                }))
            }),
        }
    }

    /// A guard that never books (the root, summary transports, keepwarm,
    /// hosts without a sink).
    pub fn inert() -> Self {
        Self { slot: None }
    }

    pub fn is_inert(&self) -> bool {
        self.slot.is_none()
    }

    pub fn observe(&self, report: &ReportedUsage) {
        observe_slot(&self.slot, report);
    }

    pub fn observer(&self) -> AttemptObserver {
        AttemptObserver {
            slot: self.slot.clone(),
        }
    }

    /// Book what is retained, once.
    pub fn settle(mut self) {
        self.settle_now();
    }

    fn settle_now(&mut self) {
        let Some(slot) = self.slot.take() else { return };
        let (tap, kind, model, tokens) = {
            let mut guard = lock(&slot);
            let Some(tokens) = guard.take_for_settlement() else {
                return;
            };
            (guard.tap.clone(), guard.kind, guard.model.clone(), tokens)
        };
        tap.book(kind, &model, tokens);
    }
}

impl Drop for AttemptSpend {
    fn drop(&mut self) {
        self.settle_now();
    }
}

impl AttemptObserver {
    /// An observer of no guard: its reports go nowhere.
    pub fn inert() -> Self {
        Self { slot: None }
    }

    pub fn observe(&self, report: &ReportedUsage) {
        observe_slot(&self.slot, report);
    }
}

/// Where a subagent launch takes its run lease: the session's sink, for a
/// worker the session's own turn delegates, or the delegating worker's run.
#[derive(Debug, Clone)]
pub enum SpendParent {
    Session(SpendSink),
    Run(RunLease),
}

impl SpendParent {
    /// A lease for the launch of worker `handle`. Refused once the
    /// delegating run is sealed or the session is ending.
    pub fn lease(&self, handle: &str, agent: &str) -> Result<RunLease, LeaseRefused> {
        match self {
            SpendParent::Session(sink) => sink.lease(handle, agent),
            SpendParent::Run(run) => run.child(handle, agent),
        }
    }
}

/// The session's dedupe of what its runs published: the sequence number
/// it expects next, per run. Exactly once is the producers' invariant (one
/// lock over the sequence number and the send); this only guards it.
#[derive(Debug, Default)]
pub struct SpendSeqs {
    next: std::collections::HashMap<(String, u64), u64>,
}

/// What the session does with a published report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqVerdict {
    /// The expected report: book it.
    Book,
    /// Already booked (a lower sequence number): drop it.
    Duplicate { expected: u64 },
    /// Reports were skipped (a higher sequence number): book it, and
    /// expect the one after it.
    Gap { expected: u64 },
}

impl SpendSeqs {
    pub fn accept(&mut self, run: &RunIdentity, seq: u64) -> SeqVerdict {
        let next = self
            .next
            .entry((run.handle.clone(), run.run_nonce))
            .or_insert(0);
        let expected = *next;
        if seq < expected {
            return SeqVerdict::Duplicate { expected };
        }
        *next = seq.saturating_add(1);
        if seq == expected {
            SeqVerdict::Book
        } else {
            SeqVerdict::Gap { expected }
        }
    }

    /// A run's terminal: forget the run. `Err(accepted)` when the run says
    /// it published `next_seq` reports and `accepted` were accepted.
    pub fn end(&mut self, run: &RunIdentity, next_seq: u64) -> Result<(), u64> {
        let accepted = self
            .next
            .remove(&(run.handle.clone(), run.run_nonce))
            .unwrap_or(0);
        if accepted == next_seq {
            Ok(())
        } else {
            Err(accepted)
        }
    }

    /// Runs with an accepted report and no terminal yet.
    pub fn open_runs(&self) -> usize {
        self.next.len()
    }
}

/// Test seams (`cfg(test)` / the `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// Send `report` for `lease`'s run with an explicit `seq`, without
    /// stepping the run's next seq: how a duplicate (a seq the session
    /// already booked) reaches the session's dedupe, which a lease never
    /// produces. Whether it was enqueued.
    pub fn publish_at(lease: &RunLease, seq: u64, report: SpendReport) -> bool {
        let mut gate = lock(&lease.inner.sink.gate);
        matches!(
            gate.send(SinkMessage::Spend {
                run: Arc::clone(&lease.inner.identity),
                seq,
                report,
            }),
            SendResult::Sent
        )
    }
}

#[cfg(test)]
#[path = "child_spend_tests.rs"]
mod tests;
