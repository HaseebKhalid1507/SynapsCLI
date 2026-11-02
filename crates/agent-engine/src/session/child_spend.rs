//! Child spend in the session actor (P1): what the session's subagents
//! book, at any depth, reaches this session through its spend sink, with or
//! without a turn. Each report is deduplicated, booked into the inclusive
//! totals and the child share (never as the foreground's own request).
//! Each pass that booked any is then shown and queued for a save once.
//! A session does not park or end idle while one of its workers still
//! reports spend or reports are queued (rule A).
//!
//! The queue is unbounded, on purpose: a producer publishes from `Drop` and
//! under the sink's gate lock, so it can neither block nor wait for room,
//! and dropping an accepted paid report would lose spend that was billed.
//! Nothing bounds the backlog. It is the arrival rate (at most one report
//! per settled paid attempt per live run, and live runs are at most the
//! delegation tree's concurrency limit) times the time the actor is not
//! draining (one long command, a cancel drain, a save). So the loop's arm
//! takes at most [`SPEND_BACKLOG_WARN`] messages per pass and fires again
//! while any remain. Returning to the select between passes gives other
//! commands a chance (it picks among ready arms at random); it does not
//! guarantee that a waiting command is served between any two passes. A
//! settle and an end's last drain take everything. A settle has asked
//! every worker to stop (cancellation requested: a worker's thread may
//! still finish its attempt) and admits no new run, so each live run adds
//! at most one more report per settled attempt; the end's last drain runs
//! after the sink closed. So what is left is finite.

use crate::runtime::child_spend::{RunIdentity, SeqVerdict, SinkMessage, SpendReport};

use super::actor::SessionActor;

/// When a checkpoint or an end stops waiting for its workers' spend: its
/// deadline less the save it still does (never before now).
pub(crate) fn spend_deadline(deadline: tokio::time::Instant) -> tokio::time::Instant {
    deadline
        .checked_sub(super::budgets::SPEND_SAVE_RESERVE)
        .unwrap_or_else(tokio::time::Instant::now)
}

/// The most messages one pass of the actor loop's spend arm takes before
/// the loop turns again, and the warning and load-test threshold: a pass
/// (or a settle) that takes this many logs a WARN. It is not a bound on the
/// queue (see the module doc); every report is still booked.
pub(crate) const SPEND_BACKLOG_WARN: usize = 4096;

/// What one pass over the spend queue took.
#[derive(Debug, Default, Clone, Copy)]
struct SpendPass {
    /// Messages taken from the queue (reports and run ends).
    messages: usize,
    /// Reports booked into the totals.
    booked: usize,
}

impl SpendPass {
    fn took(&mut self, booked: bool) {
        self.messages += 1;
        self.booked += usize::from(booked);
    }
}

impl SessionActor {
    /// Rule A: no worker of this session still holds a live run lease, and
    /// nothing it published is waiting to be booked.
    pub(crate) fn spend_quiet(&self) -> bool {
        self.spend_sink.producers() == 0 && self.spend_rx.is_empty()
    }

    /// The status query, plus `spend_reporting_workers` (live run leases,
    /// rule A) while there is one. Absent at 0, so a session without
    /// workers answers exactly as before.
    pub(crate) fn with_spend_status(&self, mut status: serde_json::Value) -> serde_json::Value {
        let producers = self.spend_sink.producers();
        if producers > 0 {
            if let Some(map) = status.as_object_mut() {
                map.insert("spend_reporting_workers".into(), producers.into());
            }
        }
        status
    }

    /// A checkpoint's or an end's wait (admissions already closed): ask
    /// every worker of the session still running to stop (cancellation is
    /// requested; a worker's thread may still finish its attempt and
    /// report), then book what they report until none still reports or
    /// `deadline`. A worker still running then is logged; whether its
    /// later spend is booked is the caller's (an end closes the sink, a
    /// checkpoint reopens it).
    ///
    /// The deadline bounds the wait for producers only. Every report
    /// already accepted is booked, also past the deadline: accepted paid
    /// spend is never dropped to meet a timer. What the settle booked is
    /// shown and queued for a save once, at its end.
    pub(crate) async fn settle_child_spend(&mut self, deadline: tokio::time::Instant) {
        if self.runtime.is_live() {
            let stopped = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel_running(crate::runtime::subagent::WorkerCancelScope::All);
            if !stopped.is_empty() {
                tracing::info!(session = %self.id, workers = ?stopped, "stopping the session's workers");
            }
        }
        let mut pass = SpendPass::default();
        loop {
            self.drain_child_spend(&mut pass, usize::MAX);
            let producers = self.spend_sink.producers();
            if producers == 0 {
                break;
            }
            match tokio::time::timeout_at(deadline, self.spend_rx.recv()).await {
                Ok(Some(message)) => pass.took(self.book_child_message(message)),
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(
                        session = %self.id,
                        workers = producers,
                        "worker(s) still reporting spend at the deadline"
                    );
                    break;
                }
            }
        }
        self.after_child_bookings(pass);
    }

    /// Book every message already queued, without waiting (an end's last
    /// drain, after the sink closed); show and queue a save once.
    pub(crate) fn book_queued_spend(&mut self) {
        let mut pass = SpendPass::default();
        self.drain_child_spend(&mut pass, usize::MAX);
        self.after_child_bookings(pass);
    }

    /// The actor loop's arm, one pass: `message` and those queued behind
    /// it, at most [`SPEND_BACKLOG_WARN`] messages in all, are booked, then
    /// shown and queued for a save once. What is left fires the arm again
    /// after the loop turns, which gives other commands a chance between
    /// passes (not a guarantee: the select picks a ready arm at random).
    pub(crate) fn on_child_spend(&mut self, message: SinkMessage) {
        let mut pass = SpendPass::default();
        pass.took(self.book_child_message(message));
        self.drain_child_spend(&mut pass, SPEND_BACKLOG_WARN);
        self.after_child_bookings(pass);
        #[cfg(any(test, feature = "testing"))]
        if pass.messages >= SPEND_BACKLOG_WARN {
            testing::full_pass(&self.journal_id.load());
        }
    }

    /// Book what is already queued, without waiting, until `pass` has
    /// taken `limit` messages.
    fn drain_child_spend(&mut self, pass: &mut SpendPass, limit: usize) {
        while pass.messages < limit {
            match self.spend_rx.try_recv() {
                Ok(message) => pass.took(self.book_child_message(message)),
                Err(_) => break,
            }
        }
    }

    /// After a pass booked its reports into the totals: one Conversation
    /// snapshot (it carries the totals, so the last one says all the
    /// earlier ones would) and one queued save (latest wins anyway;
    /// building each snapshot costs a copy of the history). A pass that
    /// took [`SPEND_BACKLOG_WARN`] messages or more is logged; every report
    /// it took was booked, and the arm books the rest on its next passes.
    fn after_child_bookings(&mut self, pass: SpendPass) {
        if pass.messages >= SPEND_BACKLOG_WARN {
            tracing::warn!(
                session = %self.id,
                messages = pass.messages,
                reports = pass.booked,
                still_queued = self.spend_rx.len(),
                threshold = SPEND_BACKLOG_WARN,
                "child spend backlog at its warning threshold: every report is booked"
            );
        }
        if pass.booked == 0 {
            return;
        }
        self.emit_conversation();
        // Booked spend must survive a crash while idle, as keep-warm
        // spend does: the ordinary save, queued, never awaited.
        self.request_save();
    }

    /// One message from the session's spend sink: whether it booked a
    /// report into the totals (not shown or saved yet; see
    /// [`Self::after_child_bookings`]).
    fn book_child_message(&mut self, message: SinkMessage) -> bool {
        match message {
            SinkMessage::Spend { run, seq, report } => {
                match self.spend_seqs.accept(&run, seq) {
                    SeqVerdict::Book => {}
                    SeqVerdict::Duplicate { expected } => {
                        tracing::warn!(
                            session = %self.id,
                            handle = %run.handle,
                            run = %run.run_hex(),
                            seq,
                            expected,
                            "child spend already booked: dropped"
                        );
                        return false;
                    }
                    SeqVerdict::Gap { expected } => tracing::error!(
                        session = %self.id,
                        handle = %run.handle,
                        run = %run.run_hex(),
                        seq,
                        expected,
                        "spend sequence gap: booked, expecting the next"
                    ),
                }
                if !self.conv.is_live() {
                    // Rule A keeps a session with live producers or queued
                    // reports from parking, so this is not expected.
                    tracing::warn!(
                        session = %self.id,
                        handle = %run.handle,
                        cost_usd = report.cost_usd(),
                        "child spend while parked: not booked"
                    );
                    return false;
                }
                self.conv.book_child_spend(&report);
                // Its frame now, with the totals right after this booking;
                // the pass's one snapshot follows its last report.
                let event = child_spend_event(
                    &run,
                    seq,
                    &report,
                    self.conv.session_cost,
                    self.conv.child_cost,
                );
                self.emit(crate::session::SessionEventWire::ChildSpend(event));
                true
            }
            SinkMessage::RunSealed { run, next_seq } | SinkMessage::RunEnded { run, next_seq } => {
                if let Err(accepted) = self.spend_seqs.end(&run, next_seq) {
                    tracing::error!(
                        session = %self.id,
                        handle = %run.handle,
                        run = %run.run_hex(),
                        published = next_seq,
                        accepted,
                        "child spend count differs at the run's end"
                    );
                }
                self.rearm_park();
                false
            }
        }
    }
}

/// The `child_spend` frame for one booked report, with the session's totals
/// right after the booking.
fn child_spend_event(
    run: &RunIdentity,
    seq: u64,
    report: &SpendReport,
    session_cost: f64,
    child_cost: f64,
) -> crate::session::ChildSpendEvent {
    let t = &report.tokens;
    crate::session::ChildSpendEvent {
        handle: run.handle.clone(),
        run: run.run_hex(),
        spend_seq: seq,
        agent: run.agent.clone(),
        parent_handle: run.parent_handle.clone(),
        kind: report.kind.wire_name().to_string(),
        model: report.model.clone(),
        input_tokens: t.input,
        output_tokens: t.output,
        cache_read_input_tokens: t.cache_read,
        cache_creation_input_tokens: t.cache_creation,
        cache_creation_5m: t.cache_creation_5m,
        cache_creation_1h: t.cache_creation_1h,
        cost_usd: report.cost_usd(),
        cost_nanousd: report.cost_nanousd,
        session_cost_usd: session_cost,
        child_cost_usd: child_cost,
    }
}

/// Test-only controls (`cfg(test)` / the `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::Mutex;

    /// The most messages one pass of the actor loop's spend arm takes.
    pub const PASS_CAP: usize = super::SPEND_BACKLOG_WARN;

    type Hook = Box<dyn FnOnce() + Send>;

    static AFTER_A_FULL_PASS: Mutex<Vec<(String, Hook)>> = Mutex::new(Vec::new());

    /// Run `hook` once, on the actor's thread, after the next pass of the
    /// actor loop's spend arm in session `session` (its journal id) that
    /// took the per-pass cap, before the loop turns again. It may block
    /// (a handshake with the test).
    pub fn after_a_full_pass(session: &str, hook: impl FnOnce() + Send + 'static) {
        AFTER_A_FULL_PASS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((session.to_string(), Box::new(hook)));
    }

    pub(super) fn full_pass(session: &str) {
        let hook = {
            let mut hooks = AFTER_A_FULL_PASS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hooks
                .iter()
                .position(|(s, _)| s == session)
                .map(|i| hooks.remove(i).1)
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}
