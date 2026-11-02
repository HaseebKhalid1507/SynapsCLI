//! The event loop shared by reactive workers (`subagent_start`, `subagent_resume`).
//!
//! It used to be copied into both tools. One copy now also:
//! - keeps the worker's real message history (every `MessageHistory` event), so
//!   the finalizer can archive it and `subagent_resume` can continue it;
//! - tracks where the latest model response starts, so completion previews and
//!   timeout reports lead with the worker's latest word, not its first;
//! - on timeout, cancels the worker and drains its stream briefly, because the
//!   runtime only sends the final history when the turn ends (cancel included);
//! - treats `timeout_secs == 0` as "no time limit";
//! - books the usage the worker reports while it drains after a timeout;
//! - on a run clock (`docs/specs/subagent-context-rollover.md` §5), stops at
//!   the run's one deadline, and treats the runtime's own stop on it as the
//!   run timing out;
//! - with record persistence (a v2 run), drains its stream the same bounded
//!   way after a provider error, so what settles in the run's request ledger
//!   and the runtime's last history reach the final (§6); a v1 run still
//!   ends at the error;
//! - with record persistence, keeps the runtime's notices in the run's
//!   bounded notice ring, sends them as live updates at most one a second
//!   (the latest wins; [`NoticePace`]), and keeps the run's window counters
//!   at its last durable head (§8); a v1 run drops notices, as before;
//! - keeps the legacy totals to the worker's conversation usage: a ledgered
//!   attempt (`UsageAttempt`, v2 only) counts only for purpose
//!   `conversation`; untagged `Usage` counts as before;
//! - words its result in a [`DriveShape`]: a reactive worker's, or (a
//!   oneshot's run with a plan, slice 8c) the oneshot's own.

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::runtime::subagent::{
    bounded_notice, SubagentResult, SubagentState, SubagentStatus, WindowCounters,
};
use crate::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// After a timeout the worker is cancelled and its stream drained at most this
/// long, so the runtime's final `MessageHistory` reaches the archive.
const CANCEL_DRAIN: Duration = Duration::from_secs(30);

/// [`CANCEL_DRAIN`] for a oneshot (slice 8c), whose caller waits for its
/// result: it still returns its timed-out result on time when its stopped
/// worker does not end. Whatever the worker reports after this is not
/// counted, as a oneshot without a plan counts nothing after its timer.
const ONESHOT_DRAIN: Duration = Duration::from_millis(500);

/// The least time between two live notice updates of a run ([`NoticePace`]).
const NOTICE_EVERY: Duration = Duration::from_secs(1);

/// A v2 run's live notice updates (§8), at most one each [`NOTICE_EVERY`]
/// however fast its runtime sends notices: the first goes out at once; one
/// that comes sooner after the last is held, a later one replacing it (the
/// latest wins), and goes out once its time is up (the drive's timer arm)
/// or when the drive ends. Every notice still enters the run's notice ring
/// as it comes. Nothing waits on the consumer: a send is an unbounded
/// channel's, and at most one notice is held.
#[derive(Debug, Default)]
struct NoticePace {
    /// When the last live notice update went out.
    last: Option<tokio::time::Instant>,
    /// The latest notice not sent yet.
    held: Option<String>,
}

impl NoticePace {
    /// When the held notice goes out (`None`: nothing is held).
    fn due(&self) -> Option<tokio::time::Instant> {
        self.held
            .as_ref()
            .map(|_| self.last.map_or_else(tokio::time::Instant::now, |last| last + NOTICE_EVERY))
    }

    /// A kept notice at `now`: to send now (`Some`), or held (`None`).
    fn offer(&mut self, notice: String, now: tokio::time::Instant) -> Option<String> {
        if self.last.is_some_and(|last| now < last + NOTICE_EVERY) {
            self.held = Some(notice);
            return None;
        }
        self.last = Some(now);
        self.held = None;
        Some(notice)
    }

    /// The held notice, to send at `now`.
    fn take(&mut self, now: tokio::time::Instant) -> Option<String> {
        let held = self.held.take()?;
        self.last = Some(now);
        Some(held)
    }
}

pub(super) type WorkerStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// How a drive words its run's result: as a reactive worker's
/// (`subagent_start`, `subagent_resume`) or as a oneshot's (`subagent`,
/// whose run with a plan keeps what its own loop returns; slice 8c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DriveShape {
    /// Responses separated by a blank line; a timeout report that leads
    /// with the latest response and says whether the run can be resumed;
    /// a provider error by its category.
    Reactive,
    /// The oneshot's words: responses separated by a blank line, as the
    /// oneshot's own loop does (`append_response_text`); its
    /// timed-out partial result ([`super::oneshot::timed_out_text`]); a
    /// provider error with its message and category. Its stopped worker is
    /// drained at most [`ONESHOT_DRAIN`]: its caller is waiting.
    Oneshot,
}

pub(super) struct WorkerDrive {
    pub state: Arc<RwLock<SubagentState>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<StreamEvent>>,
    pub subagent_id: u64,
    pub label: String,
    pub model: String,
    /// Wall-clock limit in seconds; 0 = none.
    pub timeout_secs: u64,
    /// The run clock of a run with a plan (§5), shared with its runtime;
    /// `None` for a v1 run, which times out after `timeout_secs` from the
    /// drive's start.
    pub run_clock: Option<crate::runtime::budget::RunClock>,
    /// The worker's cancellation token (cancelled on timeout).
    pub cancel: crate::CancellationToken,
    /// The text that started this run, for the legacy `conversation_state`.
    pub task: String,
    /// Logs the provider's count for the run's first request next to its
    /// admission figure (see `request_size::FirstRequest`).
    pub first_request: Option<super::request_size::FirstRequest>,
    /// A watch worker: its lease for this run and its usage before it. The
    /// run's time and completed rounds are added to the archive metadata
    /// when it ends, on every path. `None`: an ordinary worker.
    pub lease: Option<(
        super::watch_admission::WatchLease,
        super::watch_admission::WatchUsage,
    )>,
    /// The lease's rounds are this run's round bound (`WatchLease::apply_to`);
    /// false when an operator's lower worker cap is, and for an ordinary worker.
    pub lease_binds_rounds: bool,
    /// How the run's result is worded.
    pub shape: DriveShape,
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    // TTL split: None only if no turn ever reported one; otherwise summed.
    cache_5m: Option<u64>,
    cache_1h: Option<u64>,
}

impl Totals {
    /// Add a usage event's counts (anything else is ignored). The legacy
    /// totals are the worker's conversation usage (§6): a ledgered
    /// attempt counts only for purpose `conversation` (a rollover summary's
    /// requests are in the run's ledger, not here); an untagged `Usage` is
    /// counted as it always was (an older event is the worker's
    /// conversation).
    fn book(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model: _,
            }) => self.add(
                *input_tokens,
                *output_tokens,
                *cache_read_input_tokens,
                *cache_creation_input_tokens,
                *cache_creation_5m,
                *cache_creation_1h,
            ),
            StreamEvent::Session(SessionEvent::UsageAttempt { request, usage })
                if request.purpose == agent_core::UsagePurpose::Conversation =>
            {
                self.add(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_creation_5m,
                    usage.cache_creation_1h,
                )
            }
            _ => {}
        }
    }

    fn add(
        &mut self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_5m: Option<u64>,
        cache_1h: Option<u64>,
    ) {
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_creation += cache_creation;
        crate::core::rpc_dispatch::merge_split(&mut self.cache_5m, cache_5m);
        crate::core::rpc_dispatch::merge_split(&mut self.cache_1h, cache_1h);
    }
}

/// Per-response bookkeeping (a provider round can be reset and retried).
#[derive(Default)]
struct ResponseMarks {
    /// (partial_text len, tool_log len, tool_count) when the response started.
    baseline: (usize, usize, u32),
    /// `last_response_start` before this response, restored on reset.
    prev_last_start: usize,
    /// Set at ResponseStart; consumed by the response's first text.
    pending: bool,
}

enum Flow {
    Continue,
    Done,
    Failed(String),
    /// The runtime stopped on the run clock's deadline: the run timed out.
    RunDeadline,
    /// A watch worker's turn stopped at its provider-round lease.
    LeaseRounds,
}

/// Which parts of a watch worker's lease this run's stop used up. Atomics:
/// the drive borrows it across awaits.
#[derive(Default)]
struct LeaseSpent {
    time: std::sync::atomic::AtomicBool,
    rounds: std::sync::atomic::AtomicBool,
}

impl WorkerDrive {
    pub async fn run(self, stream: WorkerStream) -> Result<SubagentResult, String> {
        self.run_with(stream, None).await
    }

    /// [`Self::run`] with the run's record persistence (dark: `None` in
    /// production until worker rollover is enabled). With persistence its
    /// run deadline replaces `timeout_secs`, and each context-head
    /// checkpoint is persisted before it is acknowledged; without, a
    /// checkpoint is refused explicitly (the runtime stops before further
    /// inference, as when nobody answers).
    pub async fn run_with(
        self,
        stream: WorkerStream,
        persistence: Option<&mut super::persist::WorkerPersistence>,
    ) -> Result<SubagentResult, String> {
        let deadline_at = match (&self.run_clock, persistence.as_deref()) {
            (Some(clock), _) => clock.deadline(),
            (None, Some(p)) => p.deadline(),
            (None, None) => (self.timeout_secs > 0)
                .then(|| tokio::time::Instant::now() + Duration::from_secs(self.timeout_secs)),
        };
        self.run_until(stream, persistence, deadline_at).await
    }

    /// [`Self::run_with`] until the run deadline `deadline_at` (`None`: no
    /// limit), taken by the caller before the launch: a run that fell back
    /// to rollover off keeps it rather than starting a fresh one. A watch
    /// worker's run is charged to its lease on every path, and with record
    /// persistence the charge is handed to it for the run's final record.
    pub(super) async fn run_until(
        self,
        stream: WorkerStream,
        mut persistence: Option<&mut super::persist::WorkerPersistence>,
        deadline_at: Option<tokio::time::Instant>,
    ) -> Result<SubagentResult, String> {
        let started = std::time::Instant::now();
        let rounds_before = self.completed_rounds();
        let spent = LeaseSpent::default();
        let mut pace = NoticePace::default();
        let outcome = self
            .drive(stream, persistence.as_deref_mut(), deadline_at, &spent, &mut pace)
            .await;
        // A notice still held goes out before the run's outcome does.
        if let Some(held) = pace.take(tokio::time::Instant::now()) {
            let tools = self.state.read().unwrap_or_else(|p| p.into_inner()).tools;
            self.update(held, tools);
        }
        let used = self.record_watch_usage(started, rounds_before, &spent);
        if let (Some(used), Some(p)) = (used, persistence) {
            p.charge_watch(used);
        }
        outcome
    }

    /// Assistant messages in the worker's history: its completed provider
    /// rounds, whichever provider ran them.
    fn completed_rounds(&self) -> u32 {
        let s = self.state.read().unwrap_or_else(|p| p.into_inner());
        let n = s
            .history
            .as_ref()
            .map_or(0, |h| h.iter().filter(|m| m["role"] == "assistant").count());
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// A watch worker's run is charged to its lease: wall-clock seconds
    /// (rounded up) and the rounds it completed, carried in the archive
    /// metadata for the next resume. A stop on the lease spends that part
    /// whole, so the next resume is refused, as the stop's report says.
    /// Returns the charge (the lease used so far), `None` for no lease.
    fn record_watch_usage(
        &self,
        started: std::time::Instant,
        rounds_before: u32,
        spent: &LeaseSpent,
    ) -> Option<super::watch_admission::WatchUsage> {
        let (lease, prior) = self.lease?;
        let mut secs = (started.elapsed().as_secs_f64().ceil() as u64).max(1);
        let mut rounds = self.completed_rounds().saturating_sub(rounds_before);
        if spent.time.load(std::sync::atomic::Ordering::Relaxed) {
            secs = secs.max(lease.secs);
        }
        if spent.rounds.load(std::sync::atomic::Ordering::Relaxed) {
            rounds = rounds.max(lease.rounds);
        }
        let used = prior.plus(secs, rounds);
        let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
        if let Some(meta) = s.archive_meta.as_mut() {
            meta.watch_usage = Some(used);
        }
        tracing::info!(
            target: super::watch_admission::LOG_TARGET,
            subagent_id = self.subagent_id,
            used_secs = used.secs,
            used_rounds = used.rounds,
            "watch lease charged"
        );
        Some(used)
    }

    /// Why a round stop is the lease's: the rounds it left this run.
    fn lease_rounds_why(&self) -> String {
        let rounds = self.lease.map_or(0, |(lease, _)| lease.rounds);
        format!("{rounds} provider rounds left at the start of this run")
    }

    /// Why a timeout is the lease's, when the lease's time (not a shorter
    /// limit of the caller's) bound this run.
    fn lease_time_why(&self) -> Option<String> {
        let (lease, _) = self.lease?;
        (self.timeout_secs >= lease.secs)
            .then(|| format!("{}s left at the start of this run", lease.secs))
    }

    /// The run deadline passed: a stop on the lease's time when the lease
    /// bound this run (spending that part whole), else a plain timeout.
    async fn stop_at_deadline(
        &self,
        stream: WorkerStream,
        tool_count: u32,
        totals: Totals,
        spent: &LeaseSpent,
    ) -> SubagentResult {
        let why = self.lease_time_why();
        if why.is_some() {
            spent.time.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.stop_spent(stream, tool_count, totals, why).await
    }

    async fn drive(
        &self,
        mut stream: WorkerStream,
        mut persistence: Option<&mut super::persist::WorkerPersistence>,
        deadline_at: Option<tokio::time::Instant>,
        spent: &LeaseSpent,
        pace: &mut NoticePace,
    ) -> Result<SubagentResult, String> {
        let mut totals = Totals::default();
        let mut tool_count = 0u32;
        let mut marks = ResponseMarks::default();
        let mut deadline = deadline_at.map(|at| Box::pin(tokio::time::sleep_until(at)));

        loop {
            let notice_due = pace.due();
            tokio::select! {
                event = stream.next() => {
                    let Some(event) = event else { break };
                    if let StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                        session_id,
                        messages,
                        receipt,
                    }) = event
                    {
                        let Some(p) = persistence.as_deref_mut() else {
                            receipt.complete(Err(std::io::Error::other(
                                super::persist::ROLLOVER_OFF,
                            )));
                            continue;
                        };
                        let step = p.on_head(&session_id, &messages, &receipt, &self.cancel).await;
                        // The counters follow what is durable (§8).
                        self.show_windows(p.windows());
                        if step == super::persist::HeadStep::DeadlineBarrier {
                            return Ok(self
                                .stop_at_deadline(stream, tool_count, totals, spent)
                                .await);
                        }
                        continue;
                    }
                    // A v2 run keeps its runtime's notices for its caller
                    // (§8); a v1 run drops them, as before.
                    if persistence.is_some() {
                        if let StreamEvent::Session(SessionEvent::Notice(text)) = &event {
                            self.notice(text, tool_count, pace);
                            continue;
                        }
                    }
                    match self.on_event(event, &mut tool_count, &mut marks, &mut totals) {
                        Flow::Continue => {}
                        Flow::Done => break,
                        Flow::Failed(reason) => {
                            if persistence.is_some() {
                                // A v2 run's final waits for what settles
                                // after the error (§6): stop the worker and
                                // drain it, bounded.
                                self.cancel.cancel();
                                self.drain(
                                    stream,
                                    &mut totals,
                                    "the worker is stopping after a provider error",
                                )
                                .await;
                            }
                            return Err(reason);
                        }
                        Flow::LeaseRounds => {
                            spent.rounds.store(true, std::sync::atomic::Ordering::Relaxed);
                            let why = self.lease_rounds_why();
                            return Ok(self.stop_spent(stream, tool_count, totals, Some(why)).await);
                        }
                        Flow::RunDeadline => {
                            return Ok(self
                                .stop_at_deadline(stream, tool_count, totals, spent)
                                .await);
                        }
                    }
                }
                _ = async {
                    match deadline.as_mut() {
                        Some(sleep) => sleep.as_mut().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return Ok(self
                        .stop_at_deadline(stream, tool_count, totals, spent)
                        .await);
                }
                // The trailing flush: a held notice goes out once its
                // time is up, while the worker runs on.
                _ = async {
                    match notice_due {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if let Some(held) = pace.take(tokio::time::Instant::now()) {
                        self.update(held, tool_count);
                    }
                }
            }
        }

        let text = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .partial_text
            .clone();
        Ok(self.result(text, tool_count, totals, false))
    }

    fn on_event(
        &self,
        event: StreamEvent,
        tool_count: &mut u32,
        marks: &mut ResponseMarks,
        totals: &mut Totals,
    ) -> Flow {
        // Everything the worker shows, for clients watching its thread.
        self.state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .watch
            .record(&event);
        // Hosted web_search is egress: show worker searches in the parent.
        crate::runtime::openai::codex_wire::forward_worker_web_search(
            &self.label,
            &event,
            self.tx_events.as_ref(),
        );
        match event {
            StreamEvent::Llm(LlmEvent::Thinking(_)) => {
                self.update("💭 thinking...".to_string(), *tool_count)
            }
            StreamEvent::Llm(LlmEvent::ResponseStart) => {
                let s = self.state.read().unwrap_or_else(|p| p.into_inner());
                marks.baseline = (s.partial_text.len(), s.tool_log.len(), *tool_count);
                marks.prev_last_start = s.last_response_start;
                marks.pending = true;
            }
            StreamEvent::Llm(LlmEvent::ResponseReset) => {
                let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
                s.partial_text.truncate(marks.baseline.0);
                s.tool_log.truncate(marks.baseline.1);
                s.last_response_start = marks.prev_last_start.min(s.partial_text.len());
                *tool_count = marks.baseline.2;
                s.tools = *tool_count;
                marks.pending = true;
            }
            StreamEvent::Llm(LlmEvent::Text(text)) => {
                let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
                if std::mem::take(&mut marks.pending) && !text.is_empty() {
                    // Separate this response from the previous one's
                    // narration, in both shapes: a oneshot's own loop does
                    // too since upstream #364 (`append_response_text`), and
                    // a run with a plan must answer as one without.
                    if !s.partial_text.is_empty() && !s.partial_text.ends_with('\n') {
                        s.partial_text.push_str("\n\n");
                    }
                    s.last_response_start = s.partial_text.len();
                } else if text.is_empty() {
                    marks.pending |= s.partial_text.len() == marks.baseline.0;
                }
                s.partial_text.push_str(&text);
            }
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
                *tool_count += 1;
                self.update(
                    format!("⚙ {} (tool #{})", tool_name, tool_count),
                    *tool_count,
                );
            }
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name, input, ..
            }) => {
                let input_preview: String = input.to_string().chars().take(200).collect();
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .tool_log
                    .push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                self.update(tool_detail(&tool_name, &input), *tool_count);
            }
            StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                let preview: String = result.chars().take(300).collect();
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .tool_log
                    .push(format!("[tool_result]: {}", preview));
            }
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                self.state
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .history = Some(history);
            }
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            }) => {
                if let Some(first) = &self.first_request {
                    first.on_usage(
                        input_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                    );
                }
                totals.book(&event);
            }
            // A ledgered attempt stands in for the plain usage (§6); like
            // `Totals::book`, only the conversation's own requests count.
            StreamEvent::Session(SessionEvent::UsageAttempt {
                ref request,
                ref usage,
            }) => {
                if request.purpose == agent_core::UsagePurpose::Conversation {
                    if let Some(first) = &self.first_request {
                        first.on_usage(
                            usage.input_tokens,
                            usage.cache_read_input_tokens,
                            usage.cache_creation_input_tokens,
                        );
                    }
                }
                totals.book(&event);
            }
            StreamEvent::Session(SessionEvent::Error(e)) => {
                // A watch worker's round bound is its lease (`WatchLease::apply_to`).
                if self.lease.is_some()
                    && self.lease_binds_rounds
                    && matches!(
                        e.outcome,
                        crate::TurnOutcome::BudgetExceeded {
                            dimension: crate::BudgetDimension::ProviderRounds
                        }
                    )
                {
                    return Flow::LeaseRounds;
                }
                // The runtime checks the same run deadline before each
                // request (§5); its stop on it is this run timing out.
                if self.run_clock.is_some()
                    && e.outcome
                        == (agent_core::TurnOutcome::BudgetExceeded {
                            dimension: agent_core::BudgetDimension::WallClock,
                        })
                {
                    return Flow::RunDeadline;
                }
                return Flow::Failed(match self.shape {
                    DriveShape::Reactive => format!(
                        "provider request failed [{}]: {}",
                        e.category_label(),
                        e.message
                    ),
                    DriveShape::Oneshot => format!("{} [{}]", e.message, e.category_label()),
                });
            }
            StreamEvent::Session(SessionEvent::Done) => return Flow::Done,
            _ => {}
        }
        Flow::Continue
    }

    /// A run whose deadline passed before its first inference: timed out
    /// without a stream (nothing to cancel or drain). A watch worker's lease
    /// is charged as on any other stop at the deadline, and handed to the
    /// run's record persistence as in [`Self::run_until`].
    pub(super) async fn expire_before_start(
        self,
        persistence: Option<&mut super::persist::WorkerPersistence>,
    ) -> SubagentResult {
        let started = std::time::Instant::now();
        let rounds_before = self.completed_rounds();
        let spent = LeaseSpent::default();
        let result = self
            .stop_at_deadline(
                Box::pin(futures::stream::empty()),
                0,
                Totals::default(),
                &spent,
            )
            .await;
        let used = self.record_watch_usage(started, rounds_before, &spent);
        if let (Some(used), Some(p)) = (used, persistence) {
            p.charge_watch(used);
        }
        result
    }

    /// Stop a worker that ran out of time, or a watch worker whose lease ran
    /// out (`lease_why` says which part): cancel it, collect its final
    /// history, and replace the output with a report that leads with its
    /// latest progress. Both are `TimedOut`. While draining, the usage the
    /// worker reports is booked, and a context-head checkpoint is refused
    /// before anything is written.
    async fn stop_spent(
        &self,
        stream: WorkerStream,
        tool_count: u32,
        mut totals: Totals,
        lease_why: Option<String>,
    ) -> SubagentResult {
        self.state.write().unwrap_or_else(|p| p.into_inner()).status = SubagentStatus::TimedOut;
        self.cancel.cancel();
        self.drain(
            stream,
            &mut totals,
            "the run deadline passed; the worker is stopping",
        )
        .await;

        let text = {
            let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
            let report = match (&lease_why, self.shape) {
                (Some(why), DriveShape::Reactive) => {
                    super::archive::watch_lease_report(why, tool_count, &s)
                }
                (None, DriveShape::Reactive) => super::archive::timeout_report(
                    self.limit_secs(),
                    tool_count,
                    &s,
                    s.history.is_some(),
                ),
                // A oneshot keeps its own loop's words: a stop on its
                // lease's time is headed as the lease's expiry, any other as
                // a timeout (`oneshot::timeout_header`). Its rounds stop is
                // never the lease's here (`lease_binds_rounds` is off).
                (why, DriveShape::Oneshot) => super::oneshot::timed_out_text(
                    &super::oneshot::timeout_header(self.limit_secs(), why.is_some()),
                    &s.tool_log,
                    &s.partial_text,
                ),
            };
            s.conversation_state = vec![
                serde_json::json!({"role": "user", "content": self.task.clone()}),
                serde_json::json!({"role": "assistant", "content": s.partial_text.clone()}),
            ];
            s.partial_text = report.clone();
            s.last_response_start = 0;
            report
        };
        self.result(text, tool_count, totals, true)
    }

    /// Drain a stopping (cancelled) worker's stream, at most
    /// [`CANCEL_DRAIN`] ([`ONESHOT_DRAIN`] for a oneshot): its history is
    /// kept, the usage it reports is booked, and a context-head checkpoint
    /// it asks for is refused with `why` before anything is written.
    async fn drain(&self, mut stream: WorkerStream, totals: &mut Totals, why: &str) {
        let limit = match self.shape {
            DriveShape::Reactive => CANCEL_DRAIN,
            DriveShape::Oneshot => ONESHOT_DRAIN,
        };
        let _ = tokio::time::timeout(limit, async {
            while let Some(event) = stream.next().await {
                // What the worker shows while it stops still reaches its
                // watch journal.
                let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
                s.watch.record(&event);
                match event {
                    StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                        s.history = Some(history);
                    }
                    StreamEvent::Session(
                        SessionEvent::Usage { .. } | SessionEvent::UsageAttempt { .. },
                    ) => {
                        totals.book(&event);
                    }
                    StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                        receipt, ..
                    }) => {
                        receipt.complete(Err(std::io::Error::other(why.to_owned())));
                    }
                    _ => {}
                }
            }
        })
        .await;
        drop(stream);
    }

    /// The run's time limit in seconds, for its timeout report: on a run
    /// clock its start-to-deadline span, otherwise `timeout_secs`.
    fn limit_secs(&self) -> u64 {
        self.run_clock
            .and_then(|clock| clock.limit())
            .map_or(self.timeout_secs, |limit| limit.as_secs())
    }

    fn result(&self, text: String, tool_count: u32, t: Totals, timed_out: bool) -> SubagentResult {
        SubagentResult {
            text,
            model: self.model.clone(),
            input_tokens: t.input,
            output_tokens: t.output,
            cache_read: t.cache_read,
            cache_creation: t.cache_creation,
            cache_creation_5m: t.cache_5m,
            cache_creation_1h: t.cache_1h,
            tool_count,
            timed_out,
        }
    }

    /// A v2 run's runtime notice: kept, bounded, in its notice ring and
    /// sent as a live update, paced by `pace`.
    fn notice(&self, text: &str, tools: u32, pace: &mut NoticePace) {
        let kept = {
            let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
            match s.visibility.as_mut() {
                Some(visibility) => visibility.push_notice(text),
                None => Some(bounded_notice(text)).filter(|kept| !kept.is_empty()),
            }
        };
        // A notice blank once flattened sends nothing and keeps the
        // progress line.
        if let Some(send) = kept.and_then(|kept| pace.offer(kept, tokio::time::Instant::now())) {
            self.update(send, tools);
        }
    }

    /// A v2 run's window counters, from its last durable head.
    fn show_windows(&self, windows: Option<WindowCounters>) {
        let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
        if let (Some(visibility), Some(windows)) = (s.visibility.as_mut(), windows) {
            visibility.windows = windows;
        }
    }

    /// Publish the worker's progress: kept in its state (clients read it once
    /// the turn that started the worker has ended) and sent as a live update.
    fn update(&self, status: String, tools: u32) {
        self.state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .note_progress(&status, tools);
        if let Some(tx) = &self.tx_events {
            let _ = tx.send(StreamEvent::Agent(AgentEvent::SubagentUpdate {
                subagent_id: self.subagent_id,
                agent_name: self.label.clone(),
                status,
            }));
        }
    }
}

/// One-line status for the TUI's subagent panel.
fn tool_detail(tool_name: &str, input: &serde_json::Value) -> String {
    let base = |key: &str, default: &str| {
        input[key]
            .as_str()
            .unwrap_or(default)
            .rsplit('/')
            .next()
            .unwrap_or(default)
            .to_string()
    };
    match tool_name {
        "bash" => {
            let preview: String = input["command"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            format!("$ {}", preview)
        }
        "read" => format!("reading {}", base("path", "?")),
        "write" => format!("writing {}", base("path", "?")),
        "edit" => format!("editing {}", base("path", "?")),
        "multi_edit" => crate::tools::multi_edit_label(input),
        "grep" => format!(
            "grep /{}/",
            input["pattern"]
                .as_str()
                .unwrap_or("?")
                .chars()
                .take(30)
                .collect::<String>()
        ),
        "find" => format!("find {}", input["pattern"].as_str().unwrap_or("?")),
        "ls" => format!("ls {}", base("path", ".")),
        other if other.starts_with("ext__") => {
            other.splitn(3, "__").last().unwrap_or(other).to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drive(
        timeout_secs: u64,
    ) -> (
        WorkerDrive,
        Arc<RwLock<SubagentState>>,
        crate::CancellationToken,
    ) {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        let cancel = crate::CancellationToken::new();
        (
            WorkerDrive {
                state: Arc::clone(&state),
                tx_events: None,
                subagent_id: 1,
                label: "inline".into(),
                model: "m".into(),
                timeout_secs,
                run_clock: None,
                cancel: cancel.clone(),
                task: "the task".into(),
                first_request: None,
                lease: None,
                lease_binds_rounds: false,
                shape: DriveShape::Reactive,
            },
            state,
            cancel,
        )
    }

    /// [`drive`] worded as a oneshot's (slice 8c).
    fn oneshot_drive(
        timeout_secs: u64,
    ) -> (
        WorkerDrive,
        Arc<RwLock<SubagentState>>,
        crate::CancellationToken,
    ) {
        let (mut d, state, cancel) = drive(timeout_secs);
        d.shape = DriveShape::Oneshot;
        (d, state, cancel)
    }

    fn text(t: &str) -> StreamEvent {
        StreamEvent::Llm(LlmEvent::Text(t.into()))
    }

    fn history(n: usize) -> StreamEvent {
        StreamEvent::Session(SessionEvent::MessageHistory(
            (0..n)
                .map(|i| Arc::new(json!({"role": "user", "content": format!("m{i}")})))
                .collect(),
        ))
    }

    fn attempt(purpose: agent_core::UsagePurpose, input: u64, output: u64) -> StreamEvent {
        StreamEvent::Session(SessionEvent::UsageAttempt {
            request: agent_core::UsageRequest {
                request_id: format!("req-{input}"),
                purpose,
                model: "m".into(),
            },
            usage: agent_core::UsageCounts {
                input_tokens: input,
                output_tokens: output,
                cache_creation_5m: Some(1),
                ..Default::default()
            },
        })
    }

    fn plain_usage(input: u64, output: u64) -> StreamEvent {
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: None,
        })
    }

    /// L: the legacy totals stay the worker's conversation usage. A
    /// ledgered conversation attempt counts; a ledgered rollover summary
    /// (even on the worker's own model) and a liability do not; untagged
    /// usage counts as before. The same holds while a timed-out run drains.
    #[tokio::test]
    async fn legacy_totals_count_conversation_usage_only() {
        use agent_core::UsagePurpose::{Conversation, RolloverSummary};
        let liability = StreamEvent::Session(SessionEvent::UsageLiability {
            request: agent_core::UsageRequest {
                request_id: "req-9".into(),
                purpose: Conversation,
                model: "m".into(),
            },
            estimate_micro_usd: 1_000_000,
            state: agent_core::LiabilityState::Open,
        });
        let (d, _, _) = drive(60);
        let events = vec![
            plain_usage(1, 1),
            attempt(Conversation, 10, 2),
            attempt(RolloverSummary, 500, 50),
            liability.clone(),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!((r.input_tokens, r.output_tokens), (11, 3));
        assert_eq!(r.cache_creation_5m, Some(1), "only the conversation attempt's split");

        let (d, _, cancel) = drive(0);
        let c = cancel.clone();
        let drained = vec![attempt(Conversation, 20, 4), attempt(RolloverSummary, 700, 70), liability];
        let stream = futures::stream::iter(vec![attempt(Conversation, 10, 2)]).chain(
            futures::stream::once(async move { c.cancelled().await })
                .flat_map(move |()| futures::stream::iter(drained.clone())),
        );
        let r = d
            .run_until(Box::pin(stream), None, Some(tokio::time::Instant::now()))
            .await
            .unwrap();
        assert!(r.timed_out);
        assert_eq!((r.input_tokens, r.output_tokens), (30, 6));
    }

    #[tokio::test]
    async fn completes_with_history_and_separated_responses() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Reading the files."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("All done: "),
            text("plate renders."),
            history(4),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert!(!r.timed_out);
        assert_eq!(r.text, "Reading the files.\n\nAll done: plate renders.");
        let s = state.read().unwrap();
        assert_eq!(s.last_response_text(), "All done: plate renders.");
        assert_eq!(s.history.as_ref().map(Vec::len), Some(4));
    }

    #[tokio::test]
    async fn worker_web_search_notices_reach_the_parent() {
        let (mut d, _state, _) = drive(60);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.tx_events = Some(tx);
        let events = vec![
            StreamEvent::Session(SessionEvent::Notice(
                "🔎 Codex web_search (search, completed): rust tokio".into(),
            )),
            StreamEvent::Session(SessionEvent::Notice("retry: upstream body".into())),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        let mut notices = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Session(SessionEvent::Notice(t)) = ev {
                notices.push(t);
            }
        }
        assert_eq!(
            notices,
            vec!["🔎 [worker inline] Codex web_search (search, completed): rust tokio".to_string()]
        );
    }

    #[tokio::test]
    async fn the_watch_journal_records_what_the_worker_showed() {
        use crate::runtime::subagent_watch::WatchEvent as W;
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Llm(LlmEvent::Thinking("plan".into())),
            text("Look"),
            text("ing."),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
                input: json!({"command": "ls"}),
            }),
            StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: "t1".into(),
                result: "a".into(),
                memory_only: false,
                client_tool: None,
            }),
            StreamEvent::Session(SessionEvent::Notice("retry: upstream body".into())),
            history(2),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events))).await.unwrap();
        let page = state.write().unwrap().watch.read(0);
        assert_eq!(
            page.events,
            vec![
                W::ResponseStart,
                W::Thinking { text: "plan".into() },
                W::Text { text: "Looking.".into() },
                W::ToolUse {
                    tool_id: "t1".into(),
                    tool_name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
                W::ToolResult { tool_id: "t1".into(), result: "a".into(), memory_only: false },
                W::Done,
            ],
            "notices and history are not journaled; deltas coalesce"
        );
    }

    #[tokio::test]
    async fn progress_is_kept_in_state_for_background_clients() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "read".into(),
                tool_id: "t2".into(),
            }),
            StreamEvent::Llm(LlmEvent::ResponseReset),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        let s = state.read().unwrap();
        // The retried round's tool start is rolled back; the bash call stays.
        assert_eq!(s.tools, 1);
        assert_eq!(s.step, "⚙ read (tool #2)", "last published step is kept");
    }

    #[tokio::test]
    async fn reset_restores_the_previous_response_mark() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("First."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Half a resp"),
            StreamEvent::Llm(LlmEvent::ResponseReset),
            text("Retried response."),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(r.text, "First.\n\nRetried response.");
        assert_eq!(
            state.read().unwrap().last_response_text(),
            "Retried response."
        );
    }

    #[tokio::test]
    async fn tool_only_rounds_keep_the_last_spoken_response() {
        let (d, state, _) = drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Rendering now."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(state.read().unwrap().last_response_text(), "Rendering now.");
    }

    #[tokio::test]
    async fn error_keeps_history_sent_before_it() {
        let (d, state, _) = drive(60);
        let events = vec![
            history(3),
            StreamEvent::Session(SessionEvent::Error(
                agent_core::TurnError::interrupted_after_side_effect("c1"),
            )),
        ];
        let err = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap_err();
        assert!(err.starts_with("provider request failed"));
        assert_eq!(
            state.read().unwrap().history.as_ref().map(Vec::len),
            Some(3)
        );
    }

    #[tokio::test]
    async fn human_error_survives_completion_and_archive() {
        let (drive, state, _) = drive(600);
        let error = crate::runtime::helpers::turn_error_for(
            &crate::RuntimeError::NetworkTimeout("waiting for response headers"),
            "turn-test",
        );
        let reason = drive
            .run(Box::pin(futures::stream::iter(vec![StreamEvent::Session(
                SessionEvent::Error(error),
            )])))
            .await
            .unwrap_err();
        assert!(reason.contains("network_error"));
        assert!(reason.contains("waiting for response headers"));
        let status = SubagentStatus::Failed(reason.clone());
        let event = super::super::finalize::build_completion_event(
            "sa_test", 1, "test", &status, "", 1.0, None, None, 0,
        );
        assert_eq!(event.content.data.unwrap()["error"], reason);
        {
            let mut s = state.write().unwrap();
            s.status = status;
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "test".into(),
                model: "anthropic/claude-opus-5-5".into(),
                system_prompt: "test".into(),
                timeout_secs: 600,
                cwd: None,
                thinking: None,
                watch_usage: None,
            });
        }
        let dir = tempfile::tempdir().unwrap();
        super::super::archive::archive_finished_worker(&state, "sa_test", dir.path());
        let path = state.read().unwrap().archive_path.clone().unwrap();
        let archive = super::super::archive::read_archive(&path).unwrap();
        assert_eq!(archive.error.as_deref(), Some(reason.as_str()));
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("error");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(super::super::archive::read_archive(&path)
            .unwrap()
            .error
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_cancels_then_collects_the_final_history() {
        let (d, state, cancel) = drive(5);
        // A worker that talks, then hangs until cancelled, then (like the
        // runtime's cancel path) sends its history and ends.
        let stream = futures::stream::iter(vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Rendering pass 2."),
        ])
        .chain(futures::stream::once(async move {
            cancel.cancelled().await;
            history(7)
        }));
        let r = d.run(Box::pin(stream)).await.unwrap();
        assert!(r.timed_out);
        assert!(r.text.starts_with("[TIMED OUT after 5s"), "{}", r.text);
        assert!(r.text.contains("Rendering pass 2."));
        let s = state.read().unwrap();
        assert_eq!(s.status, SubagentStatus::TimedOut);
        assert_eq!(s.history.as_ref().map(Vec::len), Some(7));
        assert_eq!(s.partial_text, r.text);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_drain_barrier_keeps_managed_worker_publicly_running() {
        let (d, state, cancel) = drive(5);
        state.write().unwrap().archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "worker".into(),
            model: "model".into(),
            system_prompt: "system".into(),
            timeout_secs: 5,
            cwd: None,
            thinking: None,
            watch_usage: None,
        });
        let observed = Arc::clone(&state);
        let stream = futures::stream::once(async move {
            cancel.cancelled().await;
            // Timeout already set the raw status, but the history has not arrived.
            {
                let s = observed.read().unwrap();
                assert_eq!(s.status, SubagentStatus::TimedOut);
                assert_eq!(s.public_status(), SubagentStatus::Running);
                assert!(s.finished_at.is_none());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            history(7)
        });
        d.run(Box::pin(stream)).await.unwrap();
        let s = state.read().unwrap();
        assert_eq!(s.public_status(), SubagentStatus::Running);
        assert_eq!(s.history.as_ref().unwrap().len(), 7);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_never_times_out() {
        let (d, _, _) = drive(0);
        let stream = futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(365 * 24 * 3600)).await;
            text("still here")
        })
        .chain(futures::stream::iter(vec![StreamEvent::Session(
            SessionEvent::Done,
        )]));
        let r = d.run(Box::pin(stream)).await.unwrap();
        assert!(!r.timed_out);
        assert_eq!(r.text, "still here");
    }

    use crate::tools::subagent::watch_admission::{WatchLease, WatchUsage};

    fn lease(secs: u64, rounds: u32) -> WatchLease {
        WatchLease {
            secs,
            rounds,
            cost_usd: None,
        }
    }

    fn turns(users: usize, assistants: usize) -> Vec<crate::SharedMessage> {
        let mut h: Vec<crate::SharedMessage> = (0..users)
            .map(|i| Arc::new(json!({"role": "user", "content": format!("u{i}")})))
            .collect();
        h.extend(
            (0..assistants)
                .map(|i| Arc::new(json!({"role": "assistant", "content": format!("a{i}")}))),
        );
        h
    }

    fn archive_meta() -> crate::runtime::subagent::SubagentArchiveMeta {
        crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "lease-watcher".into(),
            model: "muse/muse-wonder-1.0".into(),
            system_prompt: "watch".into(),
            timeout_secs: 0,
            cwd: None,
            thinking: None,
            watch_usage: None,
        }
    }

    fn charged(state: &RwLock<SubagentState>) -> Option<WatchUsage> {
        let s = state.read().unwrap();
        s.archive_meta.as_ref().and_then(|m| m.watch_usage)
    }

    #[tokio::test]
    async fn a_watch_run_charges_its_time_and_completed_rounds_to_the_archive_meta() {
        let (mut d, state, _) = drive(60);
        let prior = WatchUsage {
            secs: 100,
            rounds: 3,
        };
        d.lease = Some((lease(7100, 37), prior));
        {
            let mut s = state.write().unwrap();
            // A resumed worker: one round already in its history.
            s.history = Some(turns(1, 1));
            s.archive_meta = Some(archive_meta());
        }
        let events = vec![
            StreamEvent::Session(SessionEvent::MessageHistory(turns(2, 3))),
            StreamEvent::Session(SessionEvent::Done),
        ];
        d.run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        let used = state
            .read()
            .unwrap()
            .archive_meta
            .as_ref()
            .unwrap()
            .watch_usage;
        assert_eq!(
            used,
            Some(WatchUsage {
                secs: 101,
                rounds: 5
            }),
            "1 s minimum, 2 new rounds"
        );

        // An ordinary worker's meta is left alone.
        let (d, state, _) = drive(60);
        state.write().unwrap().archive_meta = Some(archive_meta());
        d.run(Box::pin(futures::stream::iter(vec![StreamEvent::Session(
            SessionEvent::Done,
        )])))
        .await
        .unwrap();
        assert_eq!(
            state
                .read()
                .unwrap()
                .archive_meta
                .as_ref()
                .unwrap()
                .watch_usage,
            None
        );
    }

    #[tokio::test]
    async fn a_watch_worker_stopped_at_its_round_lease_times_out_and_an_ordinary_one_fails() {
        let events = || {
            vec![
                StreamEvent::Llm(LlmEvent::ResponseStart),
                text("Still waiting for the rc file."),
                history(2),
                StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::budget(
                    agent_core::BudgetDimension::ProviderRounds,
                ))),
            ]
        };
        let (mut d, state, _) = drive(60);
        d.lease = Some((lease(7200, 40), WatchUsage::default()));
        d.lease_binds_rounds = true;
        state.write().unwrap().archive_meta = Some(archive_meta());
        let r = d
            .run(Box::pin(futures::stream::iter(events())))
            .await
            .unwrap();
        assert!(r.timed_out);
        assert!(
            r.text.starts_with(
                "[WATCH LEASE EXPIRED (40 provider rounds left at the start of this run)"
            ),
            "{}",
            r.text
        );
        assert!(r.text.contains("Still waiting for the rc file."));
        assert_eq!(state.read().unwrap().status, SubagentStatus::TimedOut);
        let used = charged(&state);
        assert_eq!(
            used.map(|u| u.rounds),
            Some(40),
            "a stop on the rounds spends them whole, though the history shows none"
        );

        let (d, _, _) = drive(60);
        let err = d
            .run(Box::pin(futures::stream::iter(events())))
            .await
            .unwrap_err();
        assert!(err.starts_with("provider request failed"), "{err}");

        // A watch worker stopped by an operator's lower worker cap: not the lease.
        let (mut d, _, _) = drive(60);
        d.lease = Some((lease(7200, 40), WatchUsage::default()));
        let err = d
            .run(Box::pin(futures::stream::iter(events())))
            .await
            .unwrap_err();
        assert!(err.starts_with("provider request failed"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_the_lease_set_reports_the_spent_lease() {
        let hang = |cancel: crate::CancellationToken| {
            futures::stream::iter(vec![text("Watching.")]).chain(futures::stream::once(
                async move {
                    cancel.cancelled().await;
                    history(3)
                },
            ))
        };
        // The lease's 100 s bound this run (the caller's timeout was clamped).
        let (mut d, state, cancel) = drive(100);
        state.write().unwrap().archive_meta = Some(archive_meta());
        d.lease = Some((
            lease(100, 40),
            WatchUsage {
                secs: 7100,
                rounds: 0,
            },
        ));
        let r = d.run(Box::pin(hang(cancel))).await.unwrap();
        assert!(
            r.text
                .starts_with("[WATCH LEASE EXPIRED (100s left at the start of this run)"),
            "{}",
            r.text
        );
        let used = charged(&state).map(|u| u.secs);
        assert_eq!(used, Some(7200), "the lease's time is spent whole");
        // The caller's own shorter timeout: an ordinary, resumable timeout.
        let (mut d, state, cancel) = drive(30);
        state.write().unwrap().archive_meta = Some(archive_meta());
        d.lease = Some((
            lease(100, 40),
            WatchUsage {
                secs: 7100,
                rounds: 0,
            },
        ));
        let r = d.run(Box::pin(hang(cancel))).await.unwrap();
        assert!(r.text.starts_with("[TIMED OUT after 30s"), "{}", r.text);
        let used = charged(&state);
        assert!(
            used.is_some_and(|u| u.secs < 7200),
            "only the time it ran: {used:?}"
        );
    }

    /// The lease charge line reaches the daemon log: its filter (RUST_LOG
    /// unset) keeps it at INFO under the admission target and drops the
    /// same level at the earlier bare `subagent_admission` target and at an
    /// unrelated one (the controls).
    #[tokio::test]
    async fn the_lease_charge_line_reaches_the_daemon_log() {
        use crate::tools::subagent::watch_admission::LOG_TARGET;
        let (mut d, state, _) = drive(60);
        d.lease = Some((lease(7200, 40), WatchUsage::default()));
        state.write().unwrap().archive_meta = Some(archive_meta());
        let (log, guard) = crate::tools::test_helpers::DaemonLog::capture();
        d.run(Box::pin(futures::stream::iter(vec![StreamEvent::Session(
            SessionEvent::Done,
        )])))
        .await
        .unwrap();
        tracing::info!(target: "subagent_admission", "control: the earlier target");
        tracing::info!(target: "some_dependency", "control: an unrelated target");
        drop(guard);
        let lines = log.find(LOG_TARGET, tracing::Level::INFO, "watch lease charged");
        assert_eq!(lines.len(), 1, "{:?}", log.lines());
        assert!(lines[0].fields.contains_key("used_secs"), "{:?}", lines[0]);
        assert!(
            !log.passed("control:"),
            "the controls are dropped: {:?}",
            log.lines()
        );
    }

    /// 8c: a oneshot's drive joins its responses as the oneshot's own loop
    /// does; a reset still rolls a retried response back.
    ///
    /// PARITY (upstream #364, d90ffd5fc): the oneshot's loop now sets each
    /// response off with a blank line (`append_response_text`), so the
    /// drive does too; the name is kept from when it concatenated.
    #[tokio::test]
    async fn a_oneshot_drive_concatenates_its_responses() {
        let (d, _, _) = oneshot_drive(60);
        let events = vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Reading the files."),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("Half a resp"),
            StreamEvent::Llm(LlmEvent::ResponseReset),
            text("All done: "),
            text("plate renders."),
            StreamEvent::Session(SessionEvent::Done),
        ];
        let r = d
            .run(Box::pin(futures::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(r.text, "Reading the files.\n\nAll done: plate renders.");
    }

    /// 8c: a oneshot's drive that runs out of time returns the oneshot's
    /// timed-out partial result (its tool log, then its text), after the
    /// same cancel and drain as a reactive worker's.
    ///
    /// PARITY (upstream #364): its two responses are set off by a blank
    /// line, as the oneshot's own loop sets them (`append_response_text`).
    #[tokio::test(start_paused = true)]
    async fn a_oneshot_drive_times_out_with_the_oneshot_partial_result() {
        let (d, state, cancel) = oneshot_drive(5);
        let stream = futures::stream::iter(vec![
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("looking. "),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "read".into(),
                tool_id: "t1".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "read".into(),
                tool_id: "t1".into(),
                input: json!({"path": "/x/y.txt"}),
            }),
            StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: "t1".into(),
                result: "no such file".into(),
                memory_only: false,
                client_tool: None,
            }),
            StreamEvent::Llm(LlmEvent::ResponseStart),
            text("partial answer"),
        ])
        .chain(futures::stream::once(async move {
            cancel.cancelled().await;
            history(3)
        }));
        let r = d.run(Box::pin(stream)).await.unwrap();
        assert!(r.timed_out);
        assert_eq!(
            r.text,
            "[TIMED OUT after 5s — partial results below]\n\n\
             [tool_use]: read — {\"path\":\"/x/y.txt\"}\n\
             [tool_result]: no such file\n\
             \n[partial response]:\nlooking. \n\npartial answer"
        );
        let s = state.read().unwrap();
        assert_eq!(s.status, SubagentStatus::TimedOut);
        assert_eq!(s.history.as_ref().map(Vec::len), Some(3));
        assert_eq!(s.partial_text, r.text, "the final's output is the result");
    }

    /// 8c: a oneshot returns its timed-out result on time even when its
    /// stopped worker does not end: its drive drains it at most
    /// [`ONESHOT_DRAIN`], where a reactive worker's, whose caller is not
    /// waiting, drains at most [`CANCEL_DRAIN`].
    #[tokio::test(start_paused = true)]
    async fn a_oneshot_drive_returns_on_time_when_its_worker_does_not_stop() {
        for (shape, drain) in [
            (DriveShape::Oneshot, ONESHOT_DRAIN),
            (DriveShape::Reactive, CANCEL_DRAIN),
        ] {
            let (mut d, _state, _cancel) = drive(5);
            d.shape = shape;
            let stream = futures::stream::iter(vec![
                StreamEvent::Llm(LlmEvent::ResponseStart),
                text("partial answer"),
            ])
            .chain(futures::stream::pending());
            let started = tokio::time::Instant::now();
            let r = d.run(Box::pin(stream)).await.unwrap();
            let took = started.elapsed();
            assert!(r.timed_out, "{shape:?}");
            let limit = Duration::from_secs(5) + drain;
            assert!(
                took >= limit && took < limit + Duration::from_millis(100),
                "{shape:?}: {took:?}"
            );
        }
    }

    /// 8c: a oneshot's drive reports a provider error with its message and
    /// category, as the oneshot did; a reactive worker's keeps its own
    /// words (its category, then the message).
    #[tokio::test]
    async fn a_oneshot_drive_reports_a_provider_error_with_its_message() {
        let error = || {
            StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::provider(
                "Bad request (HTTP 400).",
                "api_status",
                "turn-1-2",
            )))
        };
        let (d, _, _) = oneshot_drive(60);
        let err = d
            .run(Box::pin(futures::stream::iter(vec![error()])))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "Bad request (HTTP 400). [provider_failed code=api_status correlation=turn-1-2]"
        );
        let (d, _, _) = drive(60);
        let err = d
            .run(Box::pin(futures::stream::iter(vec![error()])))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "provider request failed [provider_failed code=api_status correlation=turn-1-2]: \
             Bad request (HTTP 400)."
        );
    }

    /// Port glue (dev's watch lease, f6000024, on a oneshot with a plan): a
    /// watch oneshot's drive words its stops as the oneshot's own loop does.
    /// A stop on the lease's time is headed as the lease's expiry, the
    /// caller's shorter timeout as a timeout, and a stop at its round bound
    /// fails in the oneshot's words, never as a reactive lease report.
    #[tokio::test(start_paused = true)]
    async fn a_watch_oneshot_drive_words_its_lease_stops_as_the_oneshot() {
        let hang = |cancel: crate::CancellationToken| {
            futures::stream::iter(vec![text("Watching.")]).chain(futures::stream::once(
                async move {
                    cancel.cancelled().await;
                    history(3)
                },
            ))
        };
        let (mut d, _, cancel) = oneshot_drive(100);
        d.lease = Some((lease(100, 40), WatchUsage::default()));
        let r = d.run(Box::pin(hang(cancel))).await.unwrap();
        assert!(r.timed_out);
        assert!(
            r.text
                .starts_with("[WATCH LEASE EXPIRED after 100s — partial results below]"),
            "{}",
            r.text
        );
        let (mut d, _, cancel) = oneshot_drive(30);
        d.lease = Some((lease(100, 40), WatchUsage::default()));
        let r = d.run(Box::pin(hang(cancel))).await.unwrap();
        assert!(
            r.text
                .starts_with("[TIMED OUT after 30s — partial results below]"),
            "{}",
            r.text
        );

        let rounds = || {
            StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::budget(
                agent_core::BudgetDimension::ProviderRounds,
            )))
        };
        let (mut d, _, _) = oneshot_drive(60);
        d.lease = Some((lease(7200, 40), WatchUsage::default()));
        let err = d
            .run(Box::pin(futures::stream::iter(vec![
                StreamEvent::Llm(LlmEvent::ResponseStart),
                text("Still waiting."),
                rounds(),
            ])))
            .await
            .unwrap_err();
        let StreamEvent::Session(SessionEvent::Error(e)) = rounds() else {
            unreachable!()
        };
        assert_eq!(err, format!("{} [{}]", e.message, e.category_label()));
    }
}
