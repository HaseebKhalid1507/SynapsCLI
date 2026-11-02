//! Capacity wait: a turn whose automatic Anthropic account selection finds
//! no capacity, for a reason that ends by itself, waits in the turn instead
//! of ending, then selects again and carries on.
//!
//! Two sites use it, both inside the spawned turn task and under the turn's
//! cancel (Esc or stop ends the wait at once, with no request sent):
//! - a failed automatic selection at turn start or at a round top
//!   ([`super::auth::AuthMethods::refresh_or_wait`]);
//! - a window-limit 429 with a long reset and no other account, before any
//!   output or tool call streamed (the Anthropic request loop in `api.rs`).
//!
//! What waits, and for how long:
//! - a limited account whose reset (or cooldown end) falls inside
//!   [`RESET_BOUND`] of the wait's start, in a failure whose kind waits
//!   (`all_cooling_down`, `usage_read_failed`, `quota_unknown`, and
//!   `credential_rejected` / `action_required`, which name the accounts that
//!   are not limited: limited ones can sit beside them): the turn waits for
//!   that reset ("reset-based"), re-checking at the held usage reads' retry
//!   time and at least every [`CHECK_CAP`], so an account that recovers
//!   sooner is taken sooner. Its notice also names every account that needs
//!   a login or an action, with S1's remedy, so a needed login is never
//!   hidden behind the wait;
//! - capacity unproven (`usage_read_failed`, `quota_unknown`) with no reset
//!   inside the bound: it re-checks at the held reads' retry time (at least
//!   every [`CHECK_CAP`]) for at most [`UNPROVEN_BOUND`] in a row, then the
//!   turn ends as it did before;
//! - never: `credential_store`, `no_credentials`, `seat_churn`,
//!   `remote_no_capacity`, a remote credential source, an explicit account,
//!   and every request the session makes on its own (keep-warm pings,
//!   summaries, compaction); nor `credential_rejected` / `action_required`
//!   with no reset inside the bound.
//!
//! Each wake-up adds a random 0..=[`MAX_JITTER`] delay, so sessions waiting
//! on the same reset don't all select in one burst. The jitter stays inside
//! the wait's envelope: where adding it would pass the 10 min cadence, it
//! comes off the cadence instead, and no wake-up passes the last check's
//! start just before the bound. The bound is hard: no wake-up
//! after it, a check still running at it is cut (the turn ends as before),
//! and a wall clock that jumps past it (a host suspend) ends the wait at the
//! next wake-up without another check. Each check is one selection through
//! the normal broker path (its single-flight and held usage reads apply).
//! Nothing holds the auth lock while it sleeps.
//!
//! The config key `capacity_wait` (default true) turns it off: `false`
//! ends the turn at once, as before.

use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::auth::{SelectionFailure, SelectionFailureKind};
use crate::{SessionEvent, StreamEvent};

/// A waiting turn checks at least this often, so a new login or a cleared
/// cooldown is noticed.
pub(crate) const CHECK_CAP: Duration = Duration::from_secs(10 * 60);
/// A turn waits only for a reset that falls inside this bound of the wait's
/// start (a 5 h window always does; a weekly limit days away doesn't).
pub(crate) const RESET_BOUND: Duration = Duration::from_secs(5 * 3600 + 15 * 60);
/// Unproven capacity with no reset inside the bound waits at most this long.
pub(crate) const UNPROVEN_BOUND: Duration = Duration::from_secs(30 * 60);
/// Upper bound of the random delay added to every wake-up.
pub(crate) const MAX_JITTER: Duration = Duration::from_secs(60);
/// No check comes sooner than this after the previous one, and the last
/// check starts at least this long before the bound.
const MIN_CHECK: Duration = Duration::from_secs(5);
/// Unproven capacity with no known retry time is checked this often.
const UNPROVEN_DEFAULT_CHECK: Duration = Duration::from_secs(60);

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The random delay added to each wake-up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Jitter {
    /// Uniform in `0..=max`.
    Random(Duration),
    /// Always exactly this (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Duration),
}

/// Present when capacity waits are on (config `capacity_wait`, default
/// true); `None` keeps the turn ending at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapacityWaitPolicy {
    pub(crate) jitter: Jitter,
    /// Tests only: a shift added to every wall-clock reading.
    #[cfg(test)]
    pub(crate) wall_shift: WallShift,
    /// Tests only: the wall clock starts at this time (epoch ms) and then
    /// ticks with tokio's (paused) clock, so a scripted reset lands exactly.
    #[cfg(test)]
    pub(crate) wall_start_ms: Option<u64>,
}

impl Default for CapacityWaitPolicy {
    fn default() -> Self {
        Self {
            jitter: Jitter::Random(MAX_JITTER),
            #[cfg(test)]
            wall_shift: WallShift::default(),
            #[cfg(test)]
            wall_start_ms: None,
        }
    }
}

/// Tests only: milliseconds added to the wait's wall-clock readings, set
/// while the wait runs to play a host suspend (forward) or a clock stepped
/// back. Two shifts always compare equal (it is not part of the policy's
/// meaning).
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct WallShift(pub(crate) std::sync::Arc<std::sync::atomic::AtomicI64>);

#[cfg(test)]
impl WallShift {
    pub(crate) fn set(&self, ms: i64) {
        self.0.store(ms, std::sync::atomic::Ordering::SeqCst);
    }
    fn get(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl PartialEq for WallShift {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[cfg(test)]
impl Eq for WallShift {}

impl CapacityWaitPolicy {
    /// The policy for config `capacity_wait`.
    pub(crate) fn from_config(enabled: bool) -> Option<Self> {
        enabled.then(Self::default)
    }

    fn jitter(&self) -> Duration {
        match &self.jitter {
            Jitter::Fixed(d) => *d,
            Jitter::Random(max) => {
                let max_ms = ms(*max);
                if max_ms == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(rand::random::<u64>() % (max_ms + 1))
                }
            }
        }
    }
}

/// Wall time (epoch ms, what resets are stated in) that never runs behind
/// the monotonic clock: the later of the wall clock and the wall at start
/// plus the monotonic time since (so tokio's paused test clock moves it).
/// A host suspend (the monotonic clock stops, the wall one doesn't) moves it
/// ahead; a wall clock stepped back never takes it below the monotonic
/// reading, so a bound measured on it holds in monotonic time too.
#[derive(Debug)]
struct Clock {
    wall0: u64,
    mono0: tokio::time::Instant,
    #[cfg(test)]
    shift: WallShift,
}

impl Clock {
    fn start(_policy: &CapacityWaitPolicy) -> Self {
        #[cfg(test)]
        let wall0 = _policy.wall_start_ms.unwrap_or_else(crate::epoch_millis);
        #[cfg(not(test))]
        let wall0 = crate::epoch_millis();
        Self {
            wall0,
            mono0: tokio::time::Instant::now(),
            #[cfg(test)]
            shift: _policy.wall_shift.clone(),
        }
    }

    /// The wall clock (epoch ms). Tests: a wall clock that ticks with
    /// tokio's (paused) clock from the start, plus the test's shift (a
    /// suspend or a clock stepped back), never the host's own clock, so the
    /// host's real time passing under load can't move a paused-clock wait.
    fn wall(&self) -> u64 {
        #[cfg(test)]
        {
            self.wall0
                .saturating_add(ms(self.mono0.elapsed()))
                .saturating_add_signed(self.shift.get())
        }
        #[cfg(not(test))]
        {
            crate::epoch_millis()
        }
    }

    fn now_ms(&self) -> u64 {
        let mono = self.wall0.saturating_add(ms(self.mono0.elapsed()));
        self.wall().max(mono)
    }
}

/// What a wait plans from: one failed selection (or a window limit) in the
/// planner's terms. Labels only, never an identity or a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Waitable {
    /// Earliest known time a limited account is usable again (epoch ms).
    pub(crate) reset_ms: Option<u64>,
    /// When held usage reads go to the network again (epoch ms).
    pub(crate) retry_ms: Option<u64>,
    /// Capacity unproven (`usage_read_failed`, `quota_unknown`): no account
    /// is known to be used up.
    pub(crate) unproven: bool,
    /// The failure kind, for the log.
    pub(crate) kind: &'static str,
    /// The notice's head, in S1's selection-failure words.
    pub(crate) head: String,
    /// What the user can do meanwhile: S1's sentences for the accounts
    /// that need a login or an action outside synaps (each starts with a
    /// space; empty when none does).
    pub(crate) remedy: String,
    /// `label=kind` per account, for the log.
    pub(crate) accounts: String,
}

impl Waitable {
    /// A failed automatic selection as something a wait plans from, or
    /// `None` when its kind never waits (a credential-store,
    /// no-credentials, seat-churn or remote failure: the user must act, or
    /// the broker host decides). A refused login (`credential_rejected`) or
    /// an action outside synaps (`action_required`) waits only for a
    /// limited account's reset beside it (its earliest reset already skips
    /// accounts that need a login or an action), never as unproven.
    pub(crate) fn from_selection(f: &SelectionFailure, now_ms: u64) -> Option<Self> {
        use SelectionFailureKind as K;
        let unproven = match f.kind {
            K::AllCoolingDown | K::CredentialRejected | K::ActionRequired => false,
            K::UsageReadFailed | K::QuotaUnknown => true,
            K::NoCredentials
            | K::SeatChurn
            | K::RemoteNoCapacity
            | K::CredentialStore
            // A kind from a newer daemon: nothing to plan a wait from.
            | K::Unknown => return None,
        };
        Some(Self {
            reset_ms: f.earliest_reset_ms,
            retry_ms: f.usage_read_retry_ms,
            unproven,
            kind: f.kind.as_str(),
            head: super::auth::selection_failure_head(f, now_ms),
            remedy: super::auth::selection_failure_remedies(f),
            accounts: accounts_field(f),
        })
    }

    /// The request's own account hit a window limit that resets at
    /// `own_reset_ms`, and no other account was found: `failure` is the
    /// broker's reason, when it gave one. The earlier of the two resets is
    /// what the wait plans from.
    pub(crate) fn window_limit(
        own_reset_ms: u64,
        failure: Option<&SelectionFailure>,
        now_ms: u64,
    ) -> Option<Self> {
        match failure {
            Some(f) => {
                let mut w = Self::from_selection(f, now_ms)?;
                w.reset_ms = Some(w.reset_ms.map_or(own_reset_ms, |r| r.min(own_reset_ms)));
                Some(w)
            }
            None => Some(Self {
                reset_ms: Some(own_reset_ms),
                retry_ms: None,
                unproven: false,
                kind: "window_limit",
                head: "No Anthropic account has capacity now".to_string(),
                remedy: String::new(),
                accounts: String::new(),
            }),
        }
    }
}

/// How far ahead a capacity stop's resume time may lie: a weekly limit
/// resets within 7 days, so a later "reset" is bad data and is never
/// marked.
pub(crate) const RESUME_HORIZON: Duration = Duration::from_secs(8 * 24 * 3600);

tokio::task_local! {
    /// The running turn's completion handle (`run_stream_tracked` scopes
    /// its turn task with it): where a reset-based capacity stop is noted.
    pub(crate) static TURN_COMPLETION: super::types::TurnCompletion;
}

/// The resume point a turn ending on `w` at `now_ms` is marked with, when
/// it is a reset-based stop: a limited account resets after `now_ms` and
/// within [`RESUME_HORIZON`]. Unproven capacity is reset-based only beside
/// a reset inside the wait's [`RESET_BOUND`] (the mixed case); without one
/// it has no resume time and is never marked. A reset past the horizon is
/// bad data: one WARN, no marker.
pub(crate) fn stop_at(w: &Waitable, now_ms: u64) -> Option<super::types::CapacityStopAt> {
    let reset = w.reset_ms.filter(|r| *r > now_ms)?;
    if w.unproven && reset > now_ms.saturating_add(ms(RESET_BOUND)) {
        return None;
    }
    if reset > now_ms.saturating_add(ms(RESUME_HORIZON)) {
        tracing::warn!(
            event = "capacity_stop",
            phase = "beyond_horizon",
            kind = w.kind,
            reset_in_h = (reset - now_ms) / 3_600_000,
            horizon_h = RESUME_HORIZON.as_secs() / 3600,
            "capacity_stop: the reset lies beyond the resume horizon; not marked"
        );
        return None;
    }
    Some(super::types::CapacityStopAt {
        resume_at_ms: reset,
        kind: w.kind,
    })
}

/// Note the running turn's capacity stop, when ending on `w` is one
/// (outside a turn task: nothing).
pub(crate) fn note_stop(w: &Waitable, now_ms: u64) {
    if let Some(stop) = stop_at(w, now_ms) {
        let _ = TURN_COMPLETION.try_with(|c| c.note_capacity_stop(stop));
    }
}

fn accounts_field(f: &SelectionFailure) -> String {
    f.accounts
        .iter()
        .map(|a| format!("{}={}", a.account, a.kind))
        .collect::<Vec<_>>()
        .join(",")
}

/// Why the turn is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Basis {
    /// A limited account resets inside the bound.
    Reset,
    /// Capacity is unproven and nothing resets inside the bound.
    Unproven,
}

impl Basis {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Unproven => "unproven",
        }
    }
}

/// One step of a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) basis: Basis,
    /// When the turn expects to continue: the reset, or (unproven) the
    /// next usage read.
    pub(crate) until_ms: u64,
    /// The next check: `until_ms`, the held reads' retry time or one
    /// [`CHECK_CAP`] from now, whichever comes first, and never sooner than
    /// [`MIN_CHECK`].
    pub(crate) check_ms: u64,
}

/// Plan the next step for `w` at `now_ms`, or `None` when nothing it names
/// can change by `reset_deadline_ms` (no wait: the turn ends as before).
pub(crate) fn plan(w: &Waitable, now_ms: u64, reset_deadline_ms: u64) -> Option<Plan> {
    let retry = w.retry_ms.filter(|t| *t > now_ms);
    let (basis, until_ms, floor) = match w.reset_ms.filter(|t| *t <= reset_deadline_ms) {
        Some(reset) if reset > now_ms => (Basis::Reset, reset, MIN_CHECK),
        // A reset that has already passed while the account is still
        // limited: look again in a minute, never in a burst.
        Some(reset) => (Basis::Reset, reset, UNPROVEN_DEFAULT_CHECK),
        None if w.unproven => (
            Basis::Unproven,
            retry.unwrap_or_else(|| now_ms.saturating_add(ms(UNPROVEN_DEFAULT_CHECK))),
            MIN_CHECK,
        ),
        None => return None,
    };
    let mut check_ms = until_ms.min(now_ms.saturating_add(ms(CHECK_CAP)));
    if let Some(r) = retry {
        check_ms = check_ms.min(r);
    }
    let check_ms = check_ms.max(now_ms.saturating_add(ms(floor)));
    Some(Plan {
        basis,
        until_ms,
        check_ms,
    })
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitEnd {
    /// A check found capacity; the turn carries on.
    Resumed,
    /// Esc, stop or a dropped turn.
    Canceled,
    /// The bound ran out (the reset bound, or 30 min of unproven capacity).
    Bound,
    /// The next known reset now falls outside the bound.
    BeyondBound,
    /// A check failed for a reason that never waits.
    Failed,
}

impl WaitEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::Canceled => "canceled",
            Self::Bound => "bound",
            Self::BeyondBound => "beyond_bound",
            Self::Failed => "failed",
        }
    }
}

/// What a wake-up from [`CapacityWaiter::sleep`] calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// Check once, through [`CapacityWaiter::bounded`].
    Check,
    /// Esc, stop or a dropped turn: the turn stops, nothing is sent.
    Canceled,
    /// The bound ran out (no check fits before it, or the host slept
    /// through it): the turn ends as it did before, with no further check.
    Ended,
}

/// One capacity wait, driven by its site: [`Self::begin`], then
/// [`Self::sleep`] and, on [`Wake::Check`], one selection run through
/// [`Self::bounded`], reporting each failed check with [`Self::still`] and
/// success with [`Self::resumed`]. Logs one INFO `capacity_wait` line when
/// it starts and one when it ends (a drop before the end logs
/// `outcome=canceled`).
///
/// The envelope is hard: no wake-up after the bound (the last check starts
/// at least [`MIN_CHECK`] before it), no two checks more than [`CHECK_CAP`]
/// apart (monotonic time), jitter included, and a check still running at
/// the bound is cut.
pub(crate) struct CapacityWaiter {
    policy: CapacityWaitPolicy,
    site: &'static str,
    tx: mpsc::UnboundedSender<StreamEvent>,
    clock: Clock,
    started_ms: u64,
    reset_deadline_ms: u64,
    /// Start of the current run of unproven plans.
    unproven_since_ms: Option<u64>,
    current: Waitable,
    plan: Plan,
    /// When the last selection started (monotonic): the failed one that
    /// began the wait, then each check.
    last_check: tokio::time::Instant,
    checks: u32,
    end: Option<WaitEnd>,
}

impl CapacityWaiter {
    /// Start waiting for `first`, or `None` when nothing it names can change
    /// inside the bound. Starting sends one notice and logs
    /// `capacity_wait phase=start`.
    pub(crate) fn begin(
        policy: &CapacityWaitPolicy,
        site: &'static str,
        first: Waitable,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Option<Self> {
        let clock = Clock::start(policy);
        let now = clock.now_ms();
        let reset_deadline_ms = now.saturating_add(ms(RESET_BOUND));
        let plan = plan(&first, now, reset_deadline_ms)?;
        let waiter = Self {
            policy: policy.clone(),
            site,
            tx: tx.clone(),
            clock,
            started_ms: now,
            reset_deadline_ms,
            unproven_since_ms: (plan.basis == Basis::Unproven).then_some(now),
            current: first,
            plan,
            last_check: tokio::time::Instant::now(),
            checks: 0,
            end: None,
        };
        tracing::info!(
            event = "capacity_wait",
            phase = "start",
            site,
            kind = waiter.current.kind,
            basis = plan.basis.as_str(),
            until = %super::anthropic_quota::describe_reset_utc(plan.until_ms, now),
            until_ms = plan.until_ms,
            accounts = %waiter.current.accounts,
            "capacity_wait"
        );
        waiter.notify(now);
        Some(waiter)
    }

    /// The waiter's clock (epoch ms), for the site's own times.
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The current step.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn plan(&self) -> Plan {
        self.plan
    }

    /// How the wait ended, once it has.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn ended(&self) -> Option<WaitEnd> {
        self.end
    }

    fn unproven_deadline_ms(&self) -> Option<u64> {
        self.unproven_since_ms
            .map(|s| s.saturating_add(ms(UNPROVEN_BOUND)))
    }

    /// The bound in force: the reset bound, or the end of the current
    /// unproven run's 30 min when that comes first.
    fn limit_ms(&self) -> u64 {
        self.unproven_deadline_ms()
            .map_or(self.reset_deadline_ms, |d| d.min(self.reset_deadline_ms))
    }

    /// How long to sleep from `now_ms` before the next check, with
    /// `jitter`; `None` when no check fits before the bound. The planned
    /// check plus the jitter, inside the envelope:
    /// - where that passes the cadence ([`CHECK_CAP`] after the last
    ///   selection), the jitter comes off the cadence instead (but never
    ///   before a planned reset or retry inside it), so waits that keep the
    ///   same cadence still spread out;
    /// - the last check starts [`MIN_CHECK`] before the bound, never later
    ///   (a wake-up the jitter would carry past it is held to it, and the
    ///   wait ends after that check).
    fn next_delay(&self, now_ms: u64, jitter: Duration) -> Option<Duration> {
        let room = Duration::from_millis(self.limit_ms().saturating_sub(now_ms));
        if room <= MIN_CHECK {
            return None;
        }
        let to_last_check = room - MIN_CHECK;
        let to_cadence = CHECK_CAP.saturating_sub(self.last_check.elapsed());
        let planned = Duration::from_millis(self.plan.check_ms.saturating_sub(now_ms)).max(MIN_CHECK);
        let wake = if planned + jitter <= to_cadence {
            planned + jitter
        } else {
            let floor = if planned < to_cadence {
                planned
            } else {
                MIN_CHECK.min(to_cadence)
            };
            to_cadence.saturating_sub(jitter).max(floor)
        };
        Some(wake.min(to_last_check))
    }

    /// The time the notice names: the reset (or the next read), or for
    /// unproven capacity the end of its 30 min.
    fn shown_ms(&self) -> u64 {
        match self.plan.basis {
            Basis::Reset => self.plan.until_ms,
            Basis::Unproven => self.unproven_deadline_ms().unwrap_or(self.plan.until_ms),
        }
    }

    /// The time the notice names, as the notice words it.
    fn shown_at(&self, now_ms: u64) -> String {
        super::anthropic_quota::describe_reset_utc(self.shown_ms(), now_ms)
    }

    /// The notice for the current step.
    pub(crate) fn notice(&self, now_ms: u64) -> String {
        let at = self.shown_at(now_ms);
        match self.plan.basis {
            Basis::Reset => format!(
                "⏸ {}; waiting until {at}, then continuing.{} Esc stops.",
                self.current.head, self.current.remedy
            ),
            Basis::Unproven => format!(
                "⏸ {}; checking again for up to {} min (until {at}), then continuing once an \
                 account has proven capacity.{} Esc stops.",
                self.current.head,
                UNPROVEN_BOUND.as_secs() / 60,
                self.current.remedy
            ),
        }
    }

    fn notify(&self, now_ms: u64) {
        let _ = self
            .tx
            .send(StreamEvent::Session(SessionEvent::Notice(self.notice(now_ms))));
    }

    /// Sleep until the next check, jitter included, inside the envelope.
    /// [`Wake::Canceled`] when `cancel` fired first; [`Wake::Ended`] (the
    /// wait has ended at its bound) when no check fits before the bound or
    /// the wall clock passed it meanwhile (a host suspend).
    pub(crate) async fn sleep(&mut self, cancel: &CancellationToken) -> Wake {
        let now = self.clock.now_ms();
        let Some(delay) = self.next_delay(now, self.policy.jitter()) else {
            self.finish(WaitEnd::Bound, now);
            return Wake::Ended;
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Wake::Canceled,
            _ = tokio::time::sleep(delay) => {}
        }
        let now = self.clock.now_ms();
        if now >= self.limit_ms() {
            self.finish(WaitEnd::Bound, now);
            return Wake::Ended;
        }
        self.last_check = tokio::time::Instant::now();
        self.checks += 1;
        Wake::Check
    }

    /// Run the check `check` makes, each one cut at the bound and at the
    /// cadence: the next cadence point is [`CHECK_CAP`] after the check
    /// started, less the jitter. A check still running at the cadence
    /// point is dropped (it can never answer or bind later), logged once as
    /// `capacity_wait phase=check_timeout`, and the next check starts at
    /// once, so no two checks start more than [`CHECK_CAP`] apart. `None`
    /// when a check is still running at the bound, or a cadence cut leaves
    /// no room for another before it (the wait has ended at its bound and
    /// the turn ends as before; whatever the check would have answered is
    /// dropped with it).
    ///
    /// Both cuts run on the monotonic clock. A success that lands after
    /// the wall-clock bound because the host slept, or the wall clock
    /// stepped forward, while the check ran is accepted: an account was
    /// found, and binding it is the good outcome. The monotonic cut still
    /// bounds a check that stalls.
    pub(crate) async fn bounded<T, F>(&mut self, mut check: impl FnMut() -> F) -> Option<T>
    where
        F: Future<Output = T>,
    {
        loop {
            let left = Duration::from_millis(self.limit_ms().saturating_sub(self.clock.now_ms()));
            let to_cadence = CHECK_CAP
                .saturating_sub(self.policy.jitter())
                .saturating_sub(self.last_check.elapsed())
                .max(MIN_CHECK);
            let at_bound = left <= to_cadence;
            match tokio::time::timeout(left.min(to_cadence), check()).await {
                Ok(answer) => return Some(answer),
                Err(_) => {
                    let now = self.clock.now_ms();
                    let room = self.limit_ms().saturating_sub(now);
                    if at_bound || room <= ms(MIN_CHECK) {
                        self.finish(WaitEnd::Bound, now);
                        return None;
                    }
                    tracing::info!(
                        event = "capacity_wait",
                        phase = "check_timeout",
                        site = self.site,
                        kind = self.current.kind,
                        basis = self.plan.basis.as_str(),
                        check = self.checks,
                        "capacity_wait"
                    );
                    self.last_check = tokio::time::Instant::now();
                    self.checks += 1;
                }
            }
        }
    }

    /// A check found no capacity: `next` is its failure in the planner's
    /// terms (`None`: a failure that never waits). `true` keeps waiting
    /// (with a new notice when the time it names changed); `false` ends
    /// the wait and the turn ends as before.
    pub(crate) fn still(&mut self, next: Option<Waitable>) -> bool {
        let now = self.clock.now_ms();
        let Some(next) = next else {
            self.finish(WaitEnd::Failed, now);
            return false;
        };
        let Some(plan) = plan(&next, now, self.reset_deadline_ms) else {
            self.current = next;
            self.finish(WaitEnd::BeyondBound, now);
            return false;
        };
        let shown_before = (self.plan.basis, self.shown_at(now));
        // The unproven bound counts unproven checks in a row: a reset-based
        // step ends the run.
        match plan.basis {
            Basis::Unproven => {
                self.unproven_since_ms.get_or_insert(now);
            }
            Basis::Reset => self.unproven_since_ms = None,
        }
        self.current = next;
        self.plan = plan;
        let out_of_time = now >= self.reset_deadline_ms
            || self.unproven_deadline_ms().is_some_and(|d| now >= d);
        if out_of_time {
            self.finish(WaitEnd::Bound, now);
            return false;
        }
        // A new notice only when what it says changed (not once per check).
        if (self.plan.basis, self.shown_at(now)) != shown_before {
            self.notify(now);
        }
        true
    }

    /// A check found capacity: the wait is over and the turn carries on.
    pub(crate) fn resumed(mut self) {
        let now = self.clock.now_ms();
        self.finish(WaitEnd::Resumed, now);
    }

    fn finish(&mut self, end: WaitEnd, now_ms: u64) {
        if self.end.is_some() {
            return;
        }
        self.end = Some(end);
        tracing::info!(
            event = "capacity_wait",
            phase = "end",
            site = self.site,
            kind = self.current.kind,
            basis = self.plan.basis.as_str(),
            outcome = end.as_str(),
            waited_s = now_ms.saturating_sub(self.started_ms) / 1000,
            checks = self.checks,
            until = %super::anthropic_quota::describe_reset_utc(self.plan.until_ms, now_ms),
            accounts = %self.current.accounts,
            "capacity_wait"
        );
        // A wait that ran out (its bound, or a reset that moved beyond it)
        // ends the turn on its last failure: mark that stop when it is a
        // reset-based one. A resumed, canceled or failed wait marks nothing.
        if matches!(end, WaitEnd::Bound | WaitEnd::BeyondBound) {
            note_stop(&self.current, now_ms);
        }
    }
}

impl Drop for CapacityWaiter {
    fn drop(&mut self) {
        let now = self.clock.now_ms();
        self.finish(WaitEnd::Canceled, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SelectionFailureKind as K;
    use agent_core::auth::{AccountFailure, AccountFailureKind as AK};
    use tokio::time::Instant;

    const MIN: u64 = 60_000;
    const HOUR: u64 = 60 * MIN;
    /// A fixed planner clock (epoch ms).
    const NOW: u64 = 1_800_000_000_000;

    fn deadline() -> u64 {
        NOW + ms(RESET_BOUND)
    }

    fn fixed(secs: u64) -> CapacityWaitPolicy {
        CapacityWaitPolicy {
            jitter: Jitter::Fixed(Duration::from_secs(secs)),
            ..Default::default()
        }
    }

    /// [`fixed`] with the wait's wall clock starting at `base` and ticking
    /// with the paused clock (no real-time drift).
    fn fixed_at(base: u64, secs: u64) -> CapacityWaitPolicy {
        CapacityWaitPolicy {
            wall_start_ms: Some(base),
            ..fixed(secs)
        }
    }

    fn limited(label: &str, until_ms: u64) -> AccountFailure {
        AccountFailure {
            account: label.into(),
            kind: AK::Exhausted,
            http_status: None,
            retry_after_s: None,
            until_ms: Some(until_ms),
            cooling_down: true,
        }
    }

    fn failure(kind: SelectionFailureKind, reset: Option<u64>, retry: Option<u64>) -> SelectionFailure {
        SelectionFailure {
            kind,
            accounts: reset.map(|r| vec![limited("claude4", r)]).unwrap_or_default(),
            earliest_reset_ms: reset,
            usage_read_retry_ms: retry,
        }
    }

    fn waitable(kind: SelectionFailureKind, reset: Option<u64>, retry: Option<u64>, now: u64) -> Waitable {
        Waitable::from_selection(&failure(kind, reset, retry), now).expect("this kind waits")
    }

    fn notices(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Session(SessionEvent::Notice(n)) = ev {
                out.push(n);
            }
        }
        out
    }

    // ── The planner ──────────────────────────────────────────────────────

    #[test]
    fn a_reset_inside_the_bound_waits_for_it_checking_at_least_every_10_min() {
        let p = plan(&waitable(K::AllCoolingDown, Some(NOW + 2 * HOUR), None, NOW), NOW, deadline())
            .expect("a reset in 2 h waits");
        assert_eq!(p.basis, Basis::Reset);
        assert_eq!(p.until_ms, NOW + 2 * HOUR);
        assert_eq!(p.check_ms, NOW + 10 * MIN, "checks at least every 10 min");

        let soon = plan(&waitable(K::AllCoolingDown, Some(NOW + 2 * MIN), None, NOW), NOW, deadline())
            .unwrap();
        assert_eq!(soon.check_ms, NOW + 2 * MIN, "a nearer reset is checked at the reset");

        // The bound's edge waits; a millisecond past it doesn't.
        let edge = waitable(K::AllCoolingDown, Some(deadline()), None, NOW);
        assert!(plan(&edge, NOW, deadline()).is_some());
        let past = waitable(K::AllCoolingDown, Some(deadline() + 1), None, NOW);
        assert_eq!(plan(&past, NOW, deadline()), None);
    }

    #[test]
    fn a_reset_beyond_the_bound_or_no_reset_at_all_never_waits() {
        let weekly = waitable(K::AllCoolingDown, Some(NOW + 3 * 24 * HOUR), None, NOW);
        assert_eq!(plan(&weekly, NOW, deadline()), None);
        let unknown = waitable(K::AllCoolingDown, None, None, NOW);
        assert_eq!(plan(&unknown, NOW, deadline()), None);
    }

    /// The mixed case (ruling): unproven capacity that also names a limited
    /// account resetting inside the bound is reset-based, and the held
    /// reads' retry time is checked first.
    #[test]
    fn unproven_capacity_with_a_reset_inside_the_bound_is_reset_based() {
        for kind in [K::UsageReadFailed, K::QuotaUnknown] {
            let w = waitable(kind, Some(NOW + 2 * HOUR), Some(NOW + 90_000), NOW);
            assert!(w.unproven);
            let p = plan(&w, NOW, deadline()).expect("waits");
            assert_eq!(p.basis, Basis::Reset, "{kind:?}");
            assert_eq!(p.until_ms, NOW + 2 * HOUR);
            assert_eq!(p.check_ms, NOW + 90_000, "the retry time comes first");
        }
    }

    #[test]
    fn unproven_capacity_alone_plans_from_the_held_reads_retry_time() {
        let p = plan(&waitable(K::UsageReadFailed, None, Some(NOW + 90_000), NOW), NOW, deadline())
            .expect("unproven waits");
        assert_eq!((p.basis, p.until_ms, p.check_ms), (Basis::Unproven, NOW + 90_000, NOW + 90_000));

        let p = plan(&waitable(K::QuotaUnknown, None, None, NOW), NOW, deadline()).unwrap();
        assert_eq!((p.basis, p.check_ms), (Basis::Unproven, NOW + MIN), "no retry time: a minute");

        let p = plan(&waitable(K::UsageReadFailed, None, Some(NOW + 25 * MIN), NOW), NOW, deadline())
            .unwrap();
        assert_eq!(p.check_ms, NOW + 10 * MIN, "never more than 10 min apart");

        // A reset beyond the bound doesn't make it reset-based.
        let p = plan(&waitable(K::UsageReadFailed, Some(NOW + 3 * 24 * HOUR), None, NOW), NOW, deadline())
            .unwrap();
        assert_eq!(p.basis, Basis::Unproven);
    }

    #[test]
    fn a_reset_that_passed_while_still_limited_is_checked_once_a_minute() {
        let p = plan(&waitable(K::AllCoolingDown, Some(NOW - 1_000), None, NOW), NOW, deadline())
            .expect("waits");
        assert_eq!((p.basis, p.check_ms), (Basis::Reset, NOW + MIN));
    }

    /// Ruling: credential-store, no-credentials, seat-churn and remote
    /// failures never wait, even when they also name a limited account
    /// resetting inside the bound.
    #[test]
    fn only_the_kinds_that_end_by_themselves_wait() {
        for kind in [
            K::NoCredentials,
            K::SeatChurn,
            K::RemoteNoCapacity,
            K::CredentialStore,
        ] {
            let f = failure(kind, Some(NOW + 2 * MIN), Some(NOW + MIN));
            assert_eq!(Waitable::from_selection(&f, NOW), None, "{kind:?} must never wait");
            assert_eq!(Waitable::window_limit(NOW + 20 * MIN, Some(&f), NOW), None, "{kind:?}");
        }
        for kind in [
            K::AllCoolingDown,
            K::UsageReadFailed,
            K::QuotaUnknown,
            K::CredentialRejected,
            K::ActionRequired,
        ] {
            let w = Waitable::from_selection(&failure(kind, Some(NOW + 2 * MIN), None), NOW)
                .unwrap_or_else(|| panic!("{kind:?} waits"));
            assert_eq!(w.kind, kind.as_str());
            assert_eq!(w.accounts, "claude4=exhausted");
        }
    }

    /// Ruling (flip): a refused login or an action outside synaps names the
    /// accounts that are not limited; limited accounts can sit beside them,
    /// and their reset (the earliest reset skips accounts that need a login
    /// or an action) is waited for when it falls inside the bound, like any
    /// reset. With no reset inside the bound they end at once, and they are
    /// never unproven (no 30 min wait).
    #[test]
    fn a_login_or_action_failure_waits_only_for_a_reset_inside_the_bound() {
        for kind in [K::CredentialRejected, K::ActionRequired] {
            let w = waitable(kind, Some(NOW + 2 * MIN), None, NOW);
            assert!(!w.unproven, "{kind:?}");
            let p = plan(&w, NOW, deadline()).unwrap_or_else(|| panic!("{kind:?} waits"));
            assert_eq!((p.basis, p.until_ms), (Basis::Reset, NOW + 2 * MIN), "{kind:?}");

            let weekly = waitable(kind, Some(NOW + 3 * 24 * HOUR), Some(NOW + MIN), NOW);
            assert_eq!(plan(&weekly, NOW, deadline()), None, "{kind:?}: a reset days away");
            let none = waitable(kind, None, Some(NOW + MIN), NOW);
            assert_eq!(plan(&none, NOW, deadline()), None, "{kind:?}: no reset at all");

            // At a window limit, the request's own reset counts too.
            let f = failure(kind, None, None);
            let w = Waitable::window_limit(NOW + 20 * MIN, Some(&f), NOW)
                .unwrap_or_else(|| panic!("{kind:?} at a window limit waits"));
            assert_eq!(w.reset_ms, Some(NOW + 20 * MIN), "{kind:?}");
        }
    }

    #[test]
    fn a_window_limit_plans_from_the_earlier_reset() {
        let own = NOW + 20 * MIN;
        let w = Waitable::window_limit(own, None, NOW).unwrap();
        assert_eq!((w.reset_ms, w.kind), (Some(own), "window_limit"));
        assert_eq!(w.head, "No Anthropic account has capacity now");

        let other = failure(K::AllCoolingDown, Some(NOW + 12 * MIN), None);
        let w = Waitable::window_limit(own, Some(&other), NOW).unwrap();
        assert_eq!((w.reset_ms, w.kind), (Some(NOW + 12 * MIN), "all_cooling_down"));
        let later = failure(K::AllCoolingDown, Some(NOW + 40 * MIN), None);
        assert_eq!(Waitable::window_limit(own, Some(&later), NOW).unwrap().reset_ms, Some(own));
    }

    #[test]
    fn random_jitter_stays_within_60_s_and_varies() {
        let policy = CapacityWaitPolicy::default();
        assert_eq!(policy.jitter, Jitter::Random(MAX_JITTER));
        assert_eq!(MAX_JITTER, Duration::from_secs(60));
        let draws: Vec<_> = (0..200).map(|_| policy.jitter()).collect();
        assert!(draws.iter().all(|d| *d <= MAX_JITTER), "{draws:?}");
        let distinct: std::collections::BTreeSet<_> = draws.iter().collect();
        assert!(distinct.len() > 20, "the jitter must vary: {distinct:?}");
        assert_eq!(CapacityWaitPolicy::from_config(true), Some(policy));
        assert_eq!(CapacityWaitPolicy::from_config(false), None);
    }

    // ── The waiter (paused clock) ────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn each_wake_up_adds_the_jitter() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::AllCoolingDown, Some(now + 2 * MIN), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 37), "turn_start", w, &tx).unwrap();
        let t0 = Instant::now();
        assert_eq!(waiter.sleep(&CancellationToken::new()).await, Wake::Check);
        assert_eq!(t0.elapsed(), Duration::from_secs(120 + 37));
    }

    /// Review B1: where the jitter would carry a wake-up past the 10 min
    /// cadence or the last check's start before the bound, it comes off
    /// that cap instead of being added.
    #[tokio::test(start_paused = true)]
    async fn the_jitter_never_carries_a_wake_up_past_the_cadence_or_the_bound() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let cancel = CancellationToken::new();

        // A reset in 2 h is checked every 10 min: 10 min - 60 s.
        let w = waitable(K::AllCoolingDown, Some(now + 2 * HOUR), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 60), "turn_start", w, &tx).unwrap();
        let t0 = Instant::now();
        assert_eq!(waiter.sleep(&cancel).await, Wake::Check);
        assert_eq!(t0.elapsed(), CHECK_CAP - MAX_JITTER);
        drop(waiter);

        // Unproven capacity whose reads are held for 29 min 50 s: the
        // cadence comes first (10 min - 60 s each time); then the retry
        // time plus the jitter would pass the last check's start, which
        // holds it (MIN_CHECK before the 30 min), and the wait ends.
        let now = crate::epoch_millis();
        let retry = now + ms(UNPROVEN_BOUND) - 10_000;
        let w = waitable(K::UsageReadFailed, None, Some(retry), now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 60), "turn_start", w, &tx).unwrap();
        let t0 = Instant::now();
        let mut at = Vec::new();
        loop {
            if waiter.sleep(&cancel).await == Wake::Ended {
                break;
            }
            at.push(t0.elapsed());
            let next = waitable(K::UsageReadFailed, None, Some(retry), waiter.now_ms());
            if !waiter.still(Some(next)) {
                break;
            }
        }
        let m = |s: u64| Duration::from_secs(s);
        assert_eq!(at, vec![m(540), m(1080), m(1620), m(1795)], "{at:?}");
        assert_eq!(waiter.ended(), Some(WaitEnd::Bound));
        assert_eq!(t0.elapsed(), m(1795));
    }

    #[tokio::test(start_paused = true)]
    async fn unproven_capacity_waits_at_most_30_min_with_one_notice() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::UsageReadFailed, None, None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        let started = waiter.now_ms();
        let cancel = CancellationToken::new();
        let t0 = Instant::now();
        let mut checks = 0;
        loop {
            if waiter.sleep(&cancel).await == Wake::Ended {
                break;
            }
            checks += 1;
            assert!(
                t0.elapsed() <= UNPROVEN_BOUND - MIN_CHECK,
                "unproven capacity waited {:?}",
                t0.elapsed()
            );
            let next = waitable(K::UsageReadFailed, None, None, waiter.now_ms());
            if !waiter.still(Some(next)) {
                break;
            }
        }
        assert_eq!(waiter.ended(), Some(WaitEnd::Bound));
        assert_eq!(t0.elapsed(), UNPROVEN_BOUND - MIN_CHECK, "the last check fits before the bound");
        assert_eq!(checks, 30, "a check a minute with no retry time");
        let notices = notices(&mut rx);
        assert_eq!(notices.len(), 1, "one notice, not one per check: {notices:?}");
        let until =
            super::super::anthropic_quota::describe_reset_utc(started + ms(UNPROVEN_BOUND), started);
        assert!(
            notices[0].contains(&format!("checking again for up to 30 min (until {until})")),
            "{}",
            notices[0]
        );
    }

    /// The mixed case keeps waiting past 30 min, to the reset, checking at
    /// least every 10 min.
    #[tokio::test(start_paused = true)]
    async fn unproven_capacity_with_a_reset_inside_the_bound_waits_for_the_reset() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let reset = now + 2 * HOUR;
        let w = waitable(K::UsageReadFailed, Some(reset), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        let cancel = CancellationToken::new();
        let t0 = Instant::now();
        let (mut last, mut max_gap, mut checks) = (Duration::ZERO, Duration::ZERO, 0);
        loop {
            assert_eq!(waiter.sleep(&cancel).await, Wake::Check);
            checks += 1;
            let e = t0.elapsed();
            max_gap = max_gap.max(e - last);
            last = e;
            if waiter.now_ms() >= reset {
                break;
            }
            let next = waitable(K::UsageReadFailed, Some(reset), None, waiter.now_ms());
            assert!(waiter.still(Some(next)), "still waiting after {e:?}");
        }
        waiter.resumed();
        assert_eq!(t0.elapsed(), Duration::from_secs(2 * 3600));
        assert!(max_gap <= CHECK_CAP, "a gap of {max_gap:?}");
        assert!(checks >= 12, "{checks} checks in 2 h");
        let notices = notices(&mut rx);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("; waiting until "), "{}", notices[0]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_reset_bound_ends_the_wait_at_5_h_15_min_with_a_last_check() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        // Still limited after its reset (+30 min): checked once a minute
        // until the bound; the last check starts MIN_CHECK before it.
        let w = waitable(K::AllCoolingDown, Some(now + 30 * MIN), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        let cancel = CancellationToken::new();
        let t0 = Instant::now();
        let mut last_check = Duration::ZERO;
        loop {
            if waiter.sleep(&cancel).await == Wake::Ended {
                break;
            }
            last_check = t0.elapsed();
            assert!(last_check <= RESET_BOUND - MIN_CHECK, "{last_check:?}");
            let next = waitable(K::AllCoolingDown, Some(now + 30 * MIN), None, waiter.now_ms());
            if !waiter.still(Some(next)) {
                break;
            }
        }
        assert_eq!(waiter.ended(), Some(WaitEnd::Bound));
        assert_eq!(last_check, RESET_BOUND - MIN_CHECK);
        assert_eq!(t0.elapsed(), RESET_BOUND - MIN_CHECK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reset_that_moves_beyond_the_bound_ends_the_wait() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::AllCoolingDown, Some(now + 2 * MIN), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        assert_eq!(waiter.sleep(&CancellationToken::new()).await, Wake::Check);
        let inside = waitable(K::AllCoolingDown, Some(now + 5 * HOUR + 10 * MIN), None, waiter.now_ms());
        assert!(waiter.still(Some(inside)), "a later reset inside the bound keeps waiting");
        assert_eq!(waiter.plan().until_ms, now + 5 * HOUR + 10 * MIN);
        let beyond = waitable(K::AllCoolingDown, Some(now + 5 * HOUR + 20 * MIN), None, waiter.now_ms());
        assert!(!waiter.still(Some(beyond)));
        assert_eq!(waiter.ended(), Some(WaitEnd::BeyondBound));
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_fails_for_a_reason_that_never_waits_ends_the_wait() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::AllCoolingDown, Some(now + 2 * MIN), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        assert_eq!(waiter.sleep(&CancellationToken::new()).await, Wake::Check);
        assert!(!waiter.still(None));
        assert_eq!(waiter.ended(), Some(WaitEnd::Failed));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_ends_the_sleep_at_once() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::AllCoolingDown, Some(now + 2 * HOUR), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
        let cancel = CancellationToken::new();
        let stopper = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                cancel.cancel();
            })
        };
        let t0 = Instant::now();
        assert_eq!(waiter.sleep(&cancel).await, Wake::Canceled, "a cancel ends the wait");
        assert_eq!(t0.elapsed(), Duration::from_secs(30));
        assert_eq!(waiter.ended(), None);
        stopper.await.unwrap();
    }

    /// Review r2 B1b, at the waiter: a check that stalls is dropped at the
    /// next cadence point (10 min after it started, less the jitter) and the
    /// next check starts at once; the dropped one never answers.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_check_is_dropped_at_the_cadence_and_the_next_starts_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let w = waitable(K::AllCoolingDown, Some(now + 2 * HOUR), None, now);
        let mut waiter = CapacityWaiter::begin(&fixed_at(now, 60), "turn_start", w, &tx).unwrap();
        let t0 = Instant::now();
        assert_eq!(waiter.sleep(&CancellationToken::new()).await, Wake::Check);
        assert_eq!(t0.elapsed(), CHECK_CAP - MAX_JITTER);
        let started = std::sync::Mutex::new(Vec::new());
        let answered = AtomicUsize::new(0);
        let answered_ref = &answered;
        let answer = waiter
            .bounded(|| {
                let n = {
                    let mut s = started.lock().unwrap();
                    s.push(t0.elapsed());
                    s.len()
                };
                async move {
                    if n == 1 {
                        tokio::time::sleep(Duration::from_secs(2 * 3600)).await;
                    }
                    answered_ref.fetch_add(1, Ordering::SeqCst);
                    n
                }
            })
            .await;
        assert_eq!(answer, Some(2), "the check after the dropped one answers");
        let m = Duration::from_secs;
        assert_eq!(*started.lock().unwrap(), vec![m(540), m(1080)]);
        assert_eq!(answered.load(Ordering::SeqCst), 1, "the dropped check never answered");
        assert_eq!(waiter.ended(), None, "the wait goes on");
    }

    /// The notice is S1's selection-failure head (labels only) and the
    /// reset time; no identity, storage key or token.
    #[tokio::test]
    async fn the_notice_names_the_failure_in_labels_and_the_reset_time() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let reset = now + 2 * HOUR;
        let f = failure(K::AllCoolingDown, Some(reset), None);
        let w = Waitable::from_selection(&f, now).unwrap();
        let waiter = CapacityWaiter::begin(&fixed(0), "turn_start", w, &tx).unwrap();
        let notices = notices(&mut rx);
        assert_eq!(notices.len(), 1);
        let at = super::super::anthropic_quota::describe_reset_utc(reset, now);
        assert_eq!(
            notices[0],
            format!(
                "⏸ {}; waiting until {at}, then continuing. Esc stops.",
                super::super::auth::selection_failure_head(&f, now)
            )
        );
        assert!(notices[0].starts_with("⏸ Every Anthropic account is cooling down"), "{}", notices[0]);
        assert!(notices[0].contains("claude4"), "{}", notices[0]);
        assert!(!notices[0].contains('@'), "{}", notices[0]);
        drop(waiter);
    }

    /// Ruling (flip): a wait beside accounts that need a login or an action
    /// says so in its notice, with S1's labels, remedies and login command,
    /// so a needed login is never hidden behind the wait.
    #[tokio::test]
    async fn the_notice_names_the_accounts_that_need_a_login_or_an_action() {
        let now = crate::epoch_millis();
        let reset = now + 2 * MIN;
        let at = super::super::anthropic_quota::describe_reset_utc(reset, now);
        let login = AccountFailure {
            kind: AK::RefreshRejected,
            until_ms: None,
            cooling_down: false,
            ..limited("claude7", 0)
        };
        let action = AccountFailure {
            kind: AK::RefreshActionRequired,
            ..login.clone()
        };
        let cases = [
            (
                K::CredentialRejected,
                login,
                " claude7 needs a new login: `synaps login --provider anthropic --account claude7`.",
            ),
            (
                K::ActionRequired,
                action,
                " claude7: the token refresh needs an action outside synaps; the `usage read needs \
                 action` line in the synaps log says what to do.",
            ),
        ];
        for (kind, other, remedy) in cases {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let f = SelectionFailure {
                kind,
                accounts: vec![limited("claude4", reset), other],
                earliest_reset_ms: Some(reset),
                usage_read_retry_ms: None,
            };
            let w = Waitable::from_selection(&f, now).unwrap_or_else(|| panic!("{kind:?} waits"));
            let waiter = CapacityWaiter::begin(&fixed(0), "turn_start", w, &tx)
                .unwrap_or_else(|| panic!("{kind:?} waits for the reset"));
            let notices = notices(&mut rx);
            assert_eq!(
                notices,
                vec![format!(
                    "⏸ {}; waiting until {at}, then continuing.{remedy} Esc stops.",
                    super::super::auth::selection_failure_head(&f, now)
                )],
                "{kind:?}"
            );
            drop(waiter);
        }
    }

    // ── Phase 2: the capacity-stop marker ────────────────────────────────

    const DAY: u64 = 24 * HOUR;

    fn stop(resume_at_ms: u64, kind: &'static str) -> Option<crate::runtime::CapacityStopAt> {
        Some(crate::runtime::CapacityStopAt { resume_at_ms, kind })
    }

    /// A reset-based stop is marked with its reset, inside the 8-day
    /// horizon (its edge included). A reset past the horizon is bad data;
    /// a reset that passed, or none at all, is no resume point.
    #[test]
    fn a_reset_based_stop_is_marked_with_its_reset_inside_the_horizon() {
        let w = |reset| waitable(K::AllCoolingDown, reset, None, NOW);
        assert_eq!(stop_at(&w(Some(NOW + 3 * DAY)), NOW), stop(NOW + 3 * DAY, "all_cooling_down"));
        assert_eq!(
            stop_at(&w(Some(NOW + 2 * HOUR)), NOW),
            stop(NOW + 2 * HOUR, "all_cooling_down"),
            "a reset inside the wait's bound too (a stop with capacity_wait off)"
        );
        assert_eq!(stop_at(&w(Some(NOW + 8 * DAY)), NOW), stop(NOW + 8 * DAY, "all_cooling_down"));
        assert_eq!(stop_at(&w(Some(NOW + 8 * DAY + 1)), NOW), None, "beyond the horizon");
        assert_eq!(stop_at(&w(Some(NOW)), NOW), None, "a reset now is no future resume");
        assert_eq!(stop_at(&w(Some(NOW - MIN)), NOW), None);
        assert_eq!(stop_at(&w(None), NOW), None);
    }

    /// A refused login or an action outside synaps beside a limited account
    /// (the flip ruling) stops on that account's reset: marked with it.
    #[test]
    fn a_login_or_action_failure_beside_a_limited_account_is_marked() {
        for kind in [K::CredentialRejected, K::ActionRequired] {
            let w = waitable(kind, Some(NOW + 3 * DAY), None, NOW);
            assert_eq!(stop_at(&w, NOW), stop(NOW + 3 * DAY, kind.as_str()), "{kind:?}");
        }
    }

    /// Unproven capacity is reset-based only in the mixed case: a limited
    /// account resetting inside the wait's bound (its edge included). With
    /// no reset, or a reset beyond the bound, it is no reset-based stop:
    /// no marker (ruling).
    #[test]
    fn unproven_capacity_is_marked_only_beside_a_reset_inside_the_bound() {
        for kind in [K::UsageReadFailed, K::QuotaUnknown] {
            let at = |reset| stop_at(&waitable(kind, reset, Some(NOW + MIN), NOW), NOW);
            assert_eq!(at(Some(NOW + 2 * HOUR)), stop(NOW + 2 * HOUR, kind.as_str()), "{kind:?}");
            assert_eq!(at(Some(deadline())), stop(deadline(), kind.as_str()), "{kind:?}");
            assert_eq!(at(Some(deadline() + 1)), None, "{kind:?}");
            assert_eq!(at(Some(NOW + 3 * DAY)), None, "{kind:?}");
            assert_eq!(at(None), None, "{kind:?}");
        }
    }

    /// A window limit is marked with the earlier of the request's own reset
    /// and the broker's.
    #[test]
    fn a_window_limit_stop_is_marked_with_the_earlier_reset() {
        let own = Waitable::window_limit(NOW + 3 * DAY, None, NOW).unwrap();
        assert_eq!(stop_at(&own, NOW), stop(NOW + 3 * DAY, "window_limit"));
        let f = failure(K::AllCoolingDown, Some(NOW + 2 * DAY), None);
        let both = Waitable::window_limit(NOW + 3 * DAY, Some(&f), NOW).unwrap();
        assert_eq!(stop_at(&both, NOW), stop(NOW + 2 * DAY, "all_cooling_down"));
    }

    /// A wait that runs out at its bound, still limited after the reset it
    /// was told (now passed), notes nothing: there is no future resume
    /// point to mark.
    #[tokio::test(start_paused = true)]
    async fn a_wait_that_ends_at_the_bound_after_its_reset_passed_notes_nothing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let completion = crate::runtime::TurnCompletion::new();
        TURN_COMPLETION
            .scope(completion.clone(), async {
                let w = waitable(K::AllCoolingDown, Some(now + 30 * MIN), None, now);
                let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
                let cancel = CancellationToken::new();
                loop {
                    if waiter.sleep(&cancel).await == Wake::Ended {
                        break;
                    }
                    let next = waitable(K::AllCoolingDown, Some(now + 30 * MIN), None, waiter.now_ms());
                    if !waiter.still(Some(next)) {
                        break;
                    }
                }
                assert_eq!(waiter.ended(), Some(WaitEnd::Bound));
            })
            .await;
        assert_eq!(completion.take_capacity_stop(), None);
    }

    /// A wait whose reset moves beyond the bound notes the new reset.
    #[tokio::test(start_paused = true)]
    async fn a_wait_whose_reset_moves_beyond_the_bound_notes_the_new_reset() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let completion = crate::runtime::TurnCompletion::new();
        let beyond = TURN_COMPLETION
            .scope(completion.clone(), async {
                let w = waitable(K::AllCoolingDown, Some(now + 2 * MIN), None, now);
                let mut waiter = CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap();
                assert_eq!(waiter.sleep(&CancellationToken::new()).await, Wake::Check);
                let beyond = waiter.now_ms() + 3 * DAY;
                assert!(!waiter.still(Some(waitable(K::AllCoolingDown, Some(beyond), None, waiter.now_ms()))));
                assert_eq!(waiter.ended(), Some(WaitEnd::BeyondBound));
                beyond
            })
            .await;
        assert_eq!(completion.take_capacity_stop(), stop(beyond, "all_cooling_down"));
        assert_eq!(completion.take_capacity_stop(), None, "taken once");
    }

    /// A wait that ends any other way notes nothing: it resumed, it was
    /// canceled, or a check failed for a reason that never waits. Outside
    /// a turn task nothing is noted either.
    #[tokio::test(start_paused = true)]
    async fn a_wait_that_resumes_is_canceled_or_fails_notes_nothing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let now = crate::epoch_millis();
        let begin = || {
            let w = waitable(K::AllCoolingDown, Some(now + 2 * MIN), None, now);
            CapacityWaiter::begin(&fixed_at(now, 0), "turn_start", w, &tx).unwrap()
        };
        let completion = crate::runtime::TurnCompletion::new();
        TURN_COMPLETION
            .scope(completion.clone(), async {
                begin().resumed();
                drop(begin());
                let mut failed = begin();
                assert!(!failed.still(None));
            })
            .await;
        assert_eq!(completion.take_capacity_stop(), None);
        note_stop(&waitable(K::AllCoolingDown, Some(now + DAY), None, now), now);
    }
}
