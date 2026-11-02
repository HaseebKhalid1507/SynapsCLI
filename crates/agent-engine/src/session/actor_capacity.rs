//! The capacity resume's actor side (A3 phase 2; the rules are in
//! [`super::capacity_resume`]): a turn that ends on a reset-based capacity
//! stop arms ONE resume; the crash-record entry carries it; a parked session
//! is held from eviction while it is armed; any turn start, and the owner's
//! idle Esc, `/new` or close, clear it; at its reset (plus the jitter) it
//! starts one turn with the resume text, unless the stopped turn left work
//! queued (the owner's message, buffered events): that work's turn goes in
//! its place, as the turn's end would have sent it, with no resume chain.
//!
//! AT MOST ONCE. The fire consumes the stop with a write of the record
//! without it that the crash record's writer acknowledges
//! ([`super::live::LiveSlot::await_durable`]) before it dispatches. A
//! consumption the writer does not acknowledge dispatches nothing and is
//! armed again for the next look. After the acknowledgement, work that
//! arrived meanwhile (commands, events, a reload's fence) wins and the stop
//! is armed again; the record's admission ([`super::live::RecordLink::admit`],
//! against a stop's or a reload's seal) is the last check, and nothing
//! awaits between it and the turn start's publish. A crash after the
//! consumption and before the dispatch loses that resume (the stop's
//! notice was shown); it is never given twice.
//!
//! The owner's clears are written and acknowledged the same way before they
//! are announced. A clear the writer does not acknowledge (one WARN) still
//! holds while this actor lives and rides the writer's next write; a crash
//! before that write can bring the stop back (the documented limit).
//!
//! Without a crash record (no record link on the host) nothing is armed,
//! held or dispatched.
//!
//! A reload's or a crash restore's rehydrate hands the new actor the stop
//! its record carried (`ArmCapacityResume`), after the resume decision and
//! the carried workers, before any Park. It is taken once, and only while
//! nothing has continued the session (no turn start, no owner's Esc, `/new`
//! or close, no checkpoint): a turn the restore itself started (a cut turn's
//! resume, a lost workers' note) or a client's work spends it. It is
//! validated before it is armed or holds an eviction; the history it pins
//! is checked at its time, once the session is live.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::actor::SessionActor;
use super::capacity_resume::{self as rules, Provenance, LOG_TARGET};
use super::types::*;

/// The actor's capacity-resume state.
#[derive(Debug, Default)]
pub(crate) struct CapacityState {
    /// The armed stop; the crash-record entry carries it.
    pub(crate) stop: Option<CapacityStop>,
    /// When it fires: its reset plus the jitter, or a later look.
    due: Option<DateTime<Utc>>,
    /// The loop's next wake toward `due`, at most [`rules::TIMER_CAP`] away
    /// (the wall clock is read again at each wake).
    pub(crate) timer: Option<tokio::time::Instant>,
    /// The resume that started the running (or last) turn: a stop of that
    /// turn arms in its chain.
    chain: Option<Provenance>,
    /// The resume being dispatched; the turn start it causes takes it.
    dispatching: Option<Provenance>,
    /// The restore's window for the stop its record carried
    /// (`ArmCapacityResume`) has closed. It is open from the actor's
    /// creation until the first arm (taken or not), turn start, owner's
    /// Esc, `/new` or close, or checkpoint: from then on the session is its
    /// own, and a late arm could bring back a resume nobody owes any more.
    restore_closed: bool,
}

/// The loop's capacity-resume wake.
pub(super) async fn capacity_timer(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Once per process: capacity resumes are off because this daemon keeps no
/// crash record.
fn note_suppressed() {
    static NOTED: AtomicBool = AtomicBool::new(false);
    if !NOTED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            target: LOG_TARGET,
            event = "capacity_resume",
            phase = "suppressed",
            why = "no_crash_record",
            "capacity resume: off (no crash record)"
        );
    }
}

fn later(by: Duration) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(by).unwrap_or_else(|_| chrono::Duration::zero())
}

impl SessionActor {
    /// The capacity stop the ending turn noted on its completion. Taken
    /// before `clear_stream` drops the completion.
    pub(crate) fn take_capacity_stop(&self) -> Option<crate::runtime::CapacityStopAt> {
        self.turn_completion.as_ref().and_then(|c| c.take_capacity_stop())
    }

    /// The ending turn stopped on a reset-based capacity stop: arm its
    /// resume (the turn end's `clear_stream` publishes it). Returns the
    /// notice to show once the turn's end is out. A turn a checkpoint cut
    /// (`cut`) is the reload's to resume, never also this one's.
    pub(crate) fn capacity_arm(
        &mut self,
        noted: crate::runtime::CapacityStopAt,
        cut: bool,
    ) -> Option<String> {
        let kind = noted.kind;
        let not_armed = |session: &SessionId, why: &str| {
            tracing::info!(
                target: LOG_TARGET,
                session = %session,
                event = "capacity_resume",
                phase = "not_armed",
                kind,
                why,
                "capacity resume: the stop is not armed"
            );
        };
        if self.host.record_link().is_none() {
            note_suppressed();
            return None;
        }
        let why = if !self.host.config().capacity_resume {
            Some("switch_off")
        } else if cut {
            Some("checkpoint_cut")
        } else if !self.config.persist {
            Some("not_persisted")
        } else if self.driver.is_some() || self.driver_pending.is_some() {
            Some("driver")
        } else {
            None
        };
        if let Some(why) = why {
            not_armed(&self.id, why);
            return None;
        }
        let Some(resume_at) = i64::try_from(noted.resume_at_ms)
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            not_armed(&self.id, "bad_time");
            return None;
        };
        let attempt = match rules::next_attempt(resume_at, self.capacity.chain) {
            Ok(attempt) => attempt,
            Err(no) => {
                not_armed(&self.id, no.as_str());
                return None;
            }
        };
        let now = Utc::now();
        let stop = CapacityStop {
            resume_at,
            stopped_at: now,
            kind: kind.to_string(),
            history_len: u32::try_from(self.conv.api_messages.len()).unwrap_or(u32::MAX),
            last_digest: rules::last_digest(&self.conv.api_messages),
            attempt,
        };
        let valid = match rules::validate(&stop, now) {
            Ok(valid) => valid,
            Err(invalid) => {
                not_armed(&self.id, invalid.as_str());
                return None;
            }
        };
        let due = resume_at
            + chrono::Duration::from_std(rules::random_jitter())
                .unwrap_or_else(|_| chrono::Duration::zero());
        self.capacity.stop = Some(valid.stop);
        self.capacity_schedule(due);
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "armed",
            kind = valid.kind.as_str(),
            attempt,
            resume_at = %resume_at.to_rfc3339(),
            due = %due.to_rfc3339(),
            "capacity resume: armed for the stop's reset"
        );
        Some(rules::armed_notice(resume_at))
    }

    /// A rehydrate's `ArmCapacityResume`: the capacity resume the record
    /// carried. Taken once, only while the restore's window is open, only
    /// with a crash record and the switch on, and only a stop that
    /// validates. It reads nothing a parked session dropped: the history it
    /// pins is checked at its time, once the session is live
    /// (`on_capacity_timer`). An overdue one fires right away (plus the
    /// jitter), behind the same guards.
    pub(crate) fn capacity_arm_restored(&mut self, stop: CapacityStop) {
        let not_armed = |session: &SessionId, why: &str| {
            tracing::info!(
                target: LOG_TARGET,
                session = %session,
                event = "capacity_resume",
                phase = "not_armed",
                why,
                "capacity resume: the restored stop is not armed"
            );
        };
        if std::mem::replace(&mut self.capacity.restore_closed, true) {
            return not_armed(&self.id, "superseded");
        }
        if self.host.record_link().is_none() {
            return note_suppressed();
        }
        if !self.host.config().capacity_resume {
            return not_armed(&self.id, "switch_off");
        }
        let now = Utc::now();
        let valid = match rules::validate(&stop, now) {
            Ok(valid) => valid,
            Err(invalid) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    session = %self.id,
                    event = "capacity_resume",
                    phase = "not_armed",
                    class = invalid.as_str(),
                    "capacity resume: the restored stop is invalid; dropped"
                );
                return;
            }
        };
        let due = valid.stop.resume_at.max(now)
            + chrono::Duration::from_std(rules::random_jitter())
                .unwrap_or_else(|_| chrono::Duration::zero());
        let (kind, attempt, resume_at) = (valid.kind, valid.stop.attempt, valid.stop.resume_at);
        self.capacity.stop = Some(valid.stop);
        self.capacity_schedule(due);
        self.publish_live(false);
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "armed",
            why = "restored",
            kind = kind.as_str(),
            attempt,
            resume_at = %resume_at.to_rfc3339(),
            due = %due.to_rfc3339(),
            "capacity resume: armed from the record"
        );
    }

    /// The restore's arm window closes (a checkpoint).
    pub(crate) fn capacity_close_restore(&mut self) {
        self.capacity.restore_closed = true;
    }

    fn capacity_schedule(&mut self, due: DateTime<Utc>) {
        self.capacity.due = Some(due);
        let left = (due - Utc::now()).to_std().unwrap_or_default();
        self.capacity.timer = Some(tokio::time::Instant::now() + left.min(rules::TIMER_CAP));
    }

    fn capacity_disarm(&mut self) -> Option<CapacityStop> {
        self.capacity.due = None;
        self.capacity.timer = None;
        self.capacity.stop.take()
    }

    /// Armed again (it is still owed) for `due`, and published.
    fn capacity_rearm(&mut self, stop: CapacityStop, due: DateTime<Utc>) {
        self.capacity.stop = Some(stop);
        self.capacity_schedule(due);
        self.publish_live(false);
    }

    /// A turn starts, whatever started it: it continues the session, so an
    /// armed stop is spent. The turn's own stop arms in a chain only when a
    /// capacity resume started it. Called before the turn start's publish,
    /// which carries the clear.
    pub(crate) fn capacity_turn_started(&mut self) {
        self.capacity.restore_closed = true;
        self.capacity.chain = self.capacity.dispatching.take();
        if let Some(stop) = self.capacity_disarm() {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "cleared",
                why = "turn_started",
                kind = %stop.kind,
                "capacity resume: cleared"
            );
        }
    }

    /// The owner moved on (an idle Esc, `/new`, a close): clear the armed
    /// stop with a write the writer acknowledges, before the caller says so.
    /// Returns whether a stop was armed.
    pub(crate) async fn capacity_owner_clear(&mut self, why: &'static str) -> bool {
        self.capacity.restore_closed = true;
        let Some(stop) = self.capacity_disarm() else {
            return false;
        };
        self.publish_live(true);
        let (after, gen) = (super::live::changes_now(), self.live_gen);
        if self.live.await_durable(after, gen, rules::ack_wait()).await {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "cleared",
                why,
                kind = %stop.kind,
                "capacity resume: cleared"
            );
        } else {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "cleared",
                class = "clear_unacked",
                why,
                "capacity resume: the crash record did not acknowledge the clear; it holds while \
                 this session runs and rides the record's next write"
            );
        }
        self.rearm_parked_eviction();
        true
    }

    /// The owner's Esc while idle (live or parked): cancel an armed resume,
    /// then say so.
    pub(crate) async fn capacity_owner_cancel(&mut self) {
        if self.capacity_owner_clear("owner_cancel").await {
            self.emit(SessionEventWire::SystemNotice(rules::canceled_notice()));
        }
    }

    /// The armed stop is no longer owed (`why`, a fixed class): drop it.
    fn capacity_drop(&mut self, why: &'static str) {
        let Some(stop) = self.capacity_disarm() else {
            return;
        };
        self.publish_live(false);
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "dropped",
            why,
            kind = %stop.kind,
            "capacity resume: dropped"
        );
        self.rearm_parked_eviction();
    }

    /// A due resume that can't fire yet (`why`) looks again later.
    fn capacity_defer(&mut self, why: &'static str) {
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "deferred",
            why,
            "capacity resume: deferred"
        );
        self.capacity_schedule(later(rules::defer()));
    }

    /// A parked session's eviction is due. An armed, valid stop holds it
    /// (the resume unparks the session at its time); a stop that is invalid,
    /// switched off or without a crash record is dropped, and the eviction
    /// runs.
    pub(crate) fn capacity_holds_eviction(&mut self) -> bool {
        let Some(stop) = &self.capacity.stop else {
            return false;
        };
        let dropped = if self.host.record_link().is_none() {
            Some("no_crash_record")
        } else if !self.host.config().capacity_resume {
            Some("switch_off")
        } else {
            rules::validate(stop, Utc::now()).err().map(rules::Invalid::as_str)
        };
        if let Some(why) = dropped {
            self.capacity_drop(why);
            return false;
        }
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "eviction_held",
            "capacity resume: the parked session is kept for its resume"
        );
        true
    }

    /// Nothing holds a parked session's eviction any more: it runs again,
    /// from when the session parked (`rearm_park` leaves a parked session
    /// alone).
    fn rearm_parked_eviction(&mut self) {
        if !self.is_parked() || !self.attached.is_empty() || self.park_deadline.is_some() {
            return;
        }
        let since = self.parked_since.unwrap_or_else(Utc::now);
        let parked_for = (Utc::now() - since).to_std().unwrap_or_default();
        self.park_deadline = super::actor::parked_evict_after()
            .map(|d| tokio::time::Instant::now() + d.saturating_sub(parked_for));
    }

    /// A compaction rewrote the history the armed stop pins: pin the new
    /// one (a crash before the record's next write drops the stop at its
    /// time, as a changed history).
    pub(crate) fn capacity_refingerprint(&mut self) {
        if self.capacity.stop.is_none() {
            return;
        }
        let len = u32::try_from(self.conv.api_messages.len()).unwrap_or(u32::MAX);
        let digest = rules::last_digest(&self.conv.api_messages);
        if let Some(stop) = self.capacity.stop.as_mut() {
            stop.history_len = len;
            stop.last_digest = digest;
        }
        self.publish_live(false);
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "refingerprinted",
            "capacity resume: the stop follows the compacted history"
        );
    }

    /// Work that runs or waits here: a due resume waits behind it. Queued
    /// commands and events win (the owner's, a sender's, a worker's). A
    /// session whose `on_session_start` has not run yet is not busy: the
    /// turn's own wait holds its request (`start_turn`), as for a queue
    /// wake, so a parked session's hook, which runs only once it is
    /// unparked, never defers its resume for good. A message queued during
    /// the stopped turn, and events it left, start no turn by themselves
    /// after an error, so they don't defer the resume (they would starve
    /// it): once it is consumed they go first, in place of its text
    /// (`send_work_queued_behind_stop`).
    fn capacity_busy(&self) -> Option<&'static str> {
        if self.streaming {
            Some("turn")
        } else if self.compact.is_some() {
            Some("compaction")
        } else if self.driver.is_some() || self.driver_pending.is_some() {
            Some("driver")
        } else if !self.cmd_rx.is_empty() {
            Some("commands")
        } else if !self.event_queue.is_empty() {
            Some("events")
        } else {
            None
        }
    }

    /// The capacity timer woke: fire the armed resume if it is due and
    /// nothing stands in its way.
    ///
    /// The order: guards that read nothing a parked session dropped (the
    /// crash record, the switch, the stop's own fields, a reload's fence or
    /// a checkpoint's grace, the record's seal) -> unpark -> busy -> the
    /// history the stop pins -> consume (an acknowledged write) -> what
    /// arrived meanwhile -> admission -> dispatch.
    pub(crate) async fn on_capacity_timer(&mut self) {
        self.capacity.timer = None;
        let (Some(stop), Some(due)) = (self.capacity.stop.clone(), self.capacity.due) else {
            return;
        };
        let now = Utc::now();
        if now < due {
            self.capacity_schedule(due);
            return;
        }
        let Some(link) = self.host.record_link() else {
            return self.capacity_drop("no_crash_record");
        };
        if !self.host.config().capacity_resume {
            return self.capacity_drop("switch_off");
        }
        if let Err(invalid) = rules::validate(&stop, now) {
            return self.capacity_drop(invalid.as_str());
        }
        if self.reload_fence.is_some() || self.checkpoint_grace.is_some() {
            return self.capacity_defer("checkpoint");
        }
        if !link.admit() {
            return self.capacity_defer("sealed");
        }
        if self.ensure_live().await.is_err() {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "dropped",
                class = "unpark_failed",
                "capacity resume: the parked session could not be restored"
            );
            return self.capacity_drop("unpark_failed");
        }
        if let Some(why) = self.capacity_busy() {
            return self.capacity_defer(why);
        }
        let history_len = u32::try_from(self.conv.api_messages.len()).unwrap_or(u32::MAX);
        if history_len != stop.history_len
            || rules::last_digest(&self.conv.api_messages) != stop.last_digest
        {
            return self.capacity_drop("history_changed");
        }
        if self.conv.context_head.is_blocked(&self.conv.session) {
            return self.capacity_drop("context_head_blocked");
        }

        // Consume: the record without the stop, acknowledged, before any
        // dispatch.
        self.capacity_disarm();
        self.publish_live(true);
        let (after, gen) = (super::live::changes_now(), self.live_gen);
        if !self.live.await_durable(after, gen, rules::ack_wait()).await {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "rearmed",
                class = "consume_unacked",
                "capacity resume: the crash record did not acknowledge the consumption; nothing \
                 is dispatched and it is tried again later"
            );
            return self.capacity_rearm(stop, later(rules::retry()));
        }
        // Tests: the crash window between the acknowledgement and the
        // dispatch.
        #[cfg(any(test, feature = "testing"))]
        rules::testing::pause_after_ack().await;

        // Acknowledged. What arrived while it waited wins; admission against
        // a stop's or a reload's seal is the last check.
        if !self.host.config().capacity_resume {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "dropped",
                why = "switch_off",
                kind = %stop.kind,
                "capacity resume: dropped"
            );
            self.rearm_parked_eviction();
            return;
        }
        let refused = if self.reload_fence.is_some() || self.checkpoint_grace.is_some() {
            Some("checkpoint")
        } else if let Some(why) = self.capacity_busy() {
            Some(why)
        } else if !link.admit() {
            Some("sealed")
        } else {
            None
        };
        if let Some(why) = refused {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "rearmed",
                why,
                "capacity resume: consumed but not dispatched; armed again"
            );
            return self.capacity_rearm(stop, later(rules::defer()));
        }
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "consumed",
            kind = %stop.kind,
            attempt = stop.attempt,
            "capacity resume: consumed"
        );
        // Work the stopped turn left queued (the owner's message, events
        // buffered during it) goes first, in place of the resume text; its
        // turn carries no resume chain.
        if let Some(action) = self.send_work_queued_behind_stop().await {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                event = "capacity_resume",
                phase = "superseded",
                action,
                "capacity resume: consumed; queued work goes first and the resume text is not sent"
            );
            return;
        }
        self.capacity.dispatching = Some(Provenance {
            dispatched: stop.attempt.saturating_add(1),
            prev_resume_at: stop.resume_at,
        });
        let action = self
            .queue_resume(rules::resume_text(stop.stopped_at, stop.resume_at))
            .await;
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            event = "capacity_resume",
            phase = "dispatched",
            action,
            dispatched = stop.attempt.saturating_add(1),
            "capacity resume: dispatched"
        );
    }
}
