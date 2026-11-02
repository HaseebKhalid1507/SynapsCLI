//! Task 23 — per-turn budgets (spec §8.1).
//!
//! Every stream session runs under one [`TurnBudget`]: provider rounds,
//! tool calls, elapsed wall-clock, accumulated tool-result bytes, and the
//! optional context-token / USD-cost dimensions. Budgets differ by role
//! (foreground, autonomous/watcher, worker) but share ONE enforcement
//! mechanism — the [`TurnBudgetMeter`] consulted by the stream loop.
//!
//! Composition note: the chat frontend's reactor auto-turn cap
//! (`events.auto_turn_cap`, default `engine::reactor::AUTO_TURN_CAP`) limits consecutive auto-triggered
//! turns ACROSS turns; this budget bounds work WITHIN one turn. They
//! compose — neither replaces nor duplicates the other.

use std::time::{Duration, Instant};

use agent_core::BudgetDimension;

/// Start of every synthetic tool_result for a call the engine did not run
/// because a turn budget was used up. Hosts that count executed calls (the
/// watcher agent) skip results that start with it.
pub const TOOL_CALL_NOT_EXECUTED: &str = "Tool call not executed";

/// The synthetic tool_result for a call refused by the turn's tool-call
/// allowance.
pub const TOOL_CALL_BUDGET_REFUSAL: &str =
    "Tool call not executed: turn tool-call budget exhausted";

/// Per-turn limits (spec §8.1).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnBudget {
    /// Provider rounds per turn (per renewal). `None` means no limit — the
    /// foreground default. Autonomous (24) and worker (64, x3 with renewals)
    /// turns are bounded by default. Set with
    /// `turn_budget.<role>.max_provider_rounds`.
    pub max_provider_rounds: Option<u32>,
    /// Tool calls per turn. `None` means no limit — the foreground and worker
    /// default; autonomous turns default to 96. Set with
    /// `turn_budget.<role>.max_tool_calls`.
    pub max_tool_calls: Option<u32>,
    /// Wall-clock limit for one turn. `None` (the default for every role)
    /// means no limit: a turn that is still making progress is never cut off
    /// for taking long. Opt in with `turn_budget.<role>.max_elapsed_secs` or
    /// `/budget time`.
    pub max_elapsed: Option<Duration>,
    /// Accumulated tool-result bytes per turn. `None` means no limit — the
    /// foreground and worker default; autonomous turns default to 8 MiB. Set
    /// with `turn_budget.<role>.max_accumulated_tool_result_bytes`.
    pub max_accumulated_tool_result_bytes: Option<usize>,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    /// Uncached input + output tokens summed over the turn's requests (cache
    /// reads and writes are priced into `max_cost_usd`, not counted here).
    /// No role sets it by default; the watcher agent uses it to carry its
    /// remaining `max_session_tokens` into the turn.
    pub max_total_tokens: Option<u64>,
    /// Bounded auto-renewals of the provider-round allowance (spec §8.1).
    /// On exhaustion the stream loop may reset the round counter this many
    /// times so a long, legitimate task continues instead of hard-failing.
    /// Round renewal never resets wall-clock (durable context successors do); a
    /// value of `0` means no graceful continuation (immediate hard stop). Only
    /// matters when `max_provider_rounds` is configured.
    pub max_round_renewals: u32,
    /// A worker run's fixed wall clock (`None` for every other runtime).
    /// With one, every turn checks the run's single deadline as well as its
    /// own `max_elapsed`. The worker's configured `max_elapsed` is already
    /// part of the clock, so the turn's own limit only ends a turn first when
    /// a budget was narrowed below the clock (an event turn, via
    /// [`TurnBudget::tighter`]).
    pub run_clock: Option<RunClock>,
}

/// A worker run's fixed wall clock (`docs/specs/subagent-context-rollover.md`
/// §5): one monotonic run start, and the run's absolute deadline, the earlier
/// of that start plus the worker's timeout and that start plus its
/// configured `max_elapsed`. A context rollover never renews it and a
/// wall-clock expiry is never a time checkpoint: the run stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunClock {
    started: tokio::time::Instant,
    deadline: Option<tokio::time::Instant>,
    /// Test seam: a controlled clock's key ([`Self::controlled`]); 0 reads
    /// the real time.
    #[cfg(test)]
    seam: u64,
}

/// Test seam: the time a controlled [`RunClock`] reads is the real time
/// plus what its test advanced it by, so a test decides when its deadline
/// passes (an explicit phase) instead of sleeping past it.
#[cfg(test)]
mod clock_seam {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(1);
    static SKEW: Mutex<BTreeMap<u64, Duration>> = Mutex::new(BTreeMap::new());

    pub(super) fn key() -> u64 {
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn skew(key: u64) -> Duration {
        if key == 0 {
            return Duration::ZERO;
        }
        SKEW.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn advance(key: u64, by: Duration) {
        *SKEW
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key)
            .or_default() += by;
    }
}

impl RunClock {
    /// Start a run's clock now. `timeout` `None` or zero is no timeout;
    /// `max_elapsed` zero is exhausted at once, as for a turn.
    pub fn start(timeout: Option<Duration>, max_elapsed: Option<Duration>) -> Self {
        Self::started_at(tokio::time::Instant::now(), timeout, max_elapsed)
    }

    /// [`Self::start`] at `started`. A limit too far ahead to represent is
    /// no limit.
    pub fn started_at(
        started: tokio::time::Instant,
        timeout: Option<Duration>,
        max_elapsed: Option<Duration>,
    ) -> Self {
        let limit = [timeout.filter(|d| !d.is_zero()), max_elapsed]
            .into_iter()
            .flatten()
            .min();
        Self {
            started,
            deadline: limit.and_then(|d| started.checked_add(d)),
            #[cfg(test)]
            seam: 0,
        }
    }

    /// The time this clock reads: the monotonic now (in tests, plus what a
    /// controlled clock was advanced by).
    fn now(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        #[cfg(test)]
        let now = now + clock_seam::skew(self.seam);
        now
    }

    /// Test seam: this clock, controlled. Its copies (the runtime's, the
    /// meter's) all read the same advanced time.
    #[cfg(test)]
    pub(crate) fn controlled(mut self) -> Self {
        self.seam = clock_seam::key();
        self
    }

    /// Test seam: move a [`Self::controlled`] clock's time forward by `by`.
    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        assert_ne!(self.seam, 0, "advance needs a controlled clock");
        clock_seam::advance(self.seam, by);
    }

    pub fn started(&self) -> tokio::time::Instant {
        self.started
    }

    /// The run deadline; `None` = no limit.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    /// The run deadline on the monotonic clock that request timers and
    /// body gates read (`tokio::time::Instant::now()`): exactly
    /// [`Self::deadline`], the run's own instant, never rebuilt from the
    /// time remaining. A controlled test clock converts the time it was
    /// advanced by (as of this call) into an earlier instant.
    pub fn timer_deadline(&self) -> Option<tokio::time::Instant> {
        let deadline = self.deadline;
        #[cfg(test)]
        let deadline = deadline.map(|d| {
            d.checked_sub(clock_seam::skew(self.seam))
                .unwrap_or_else(tokio::time::Instant::now)
        });
        deadline
    }

    /// The run's time limit, from its start to its deadline.
    pub fn limit(&self) -> Option<Duration> {
        self.deadline.map(|d| d.duration_since(self.started))
    }

    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| self.now() >= d)
    }

    /// The time left until the deadline (zero once it has passed); `None`
    /// = no limit.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(self.now()))
    }

    pub fn elapsed(&self) -> Duration {
        self.now().saturating_duration_since(self.started)
    }
}

/// Execution role a turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// Interactive foreground session.
    Foreground,
    /// Watcher agent sessions (`synaps agent`; no human in the loop).
    Autonomous,
    /// Turns a session starts by itself because events arrived (inbox
    /// messages, subagent completions). Nobody typed them.
    Event,
    /// Delegated worker/subagent sessions.
    Worker,
}

impl TurnBudget {
    /// Compiled per-role defaults. No role has a wall-clock limit unless
    /// configured. Foreground (a human is watching and can interrupt) has no
    /// round, tool-call or tool-result-byte limit either. Unattended roles
    /// stay bounded: autonomous (watcher) and event-started turns hard-stop
    /// at 24 rounds / 96 tool calls / 8 MiB (nobody is watching), and a
    /// worker stops after 64 rounds x (1 + 2 renewals) = 192 rounds per run
    /// (a `timeout: 0` worker has no other bound).
    pub fn for_role(role: TurnRole) -> Self {
        match role {
            TurnRole::Foreground => Self {
                max_provider_rounds: None,
                max_tool_calls: None,
                max_elapsed: None,
                max_accumulated_tool_result_bytes: None,
                max_context_tokens: None,
                max_cost_usd: None,
                max_total_tokens: None,
                // A human is watching and can interrupt; let long agentic
                // tasks continue through several checkpoints.
                max_round_renewals: 8,
                run_clock: None,
            },
            // Same numbers as the watcher; configured separately.
            TurnRole::Autonomous | TurnRole::Event => Self {
                max_provider_rounds: Some(24),
                max_tool_calls: Some(96),
                max_elapsed: None,
                max_accumulated_tool_result_bytes: Some(8 * 1024 * 1024),
                max_context_tokens: None,
                max_cost_usd: None,
                max_total_tokens: None,
                // Nobody is watching: no graceful extension — the round cap
                // is a hard stop.
                max_round_renewals: 0,
                run_clock: None,
            },
            TurnRole::Worker => Self {
                max_provider_rounds: Some(64),
                max_tool_calls: None,
                max_elapsed: None,
                max_accumulated_tool_result_bytes: None,
                max_context_tokens: None,
                max_cost_usd: None,
                max_total_tokens: None,
                // Bounded delegation under a supervising turn: 64 x (1 + 2)
                // = 192 rounds.
                max_round_renewals: 2,
                run_clock: None,
            },
        }
    }

    /// The stricter of two budgets, dimension by dimension (`None` = no
    /// limit loses to any limit). Used to narrow a role budget to what a
    /// session has left, or to combine a session's budget with the event
    /// role's.
    pub fn tighter(&self, other: &TurnBudget) -> TurnBudget {
        fn min<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(if b < a { b } else { a }),
                (a, None) => a,
                (None, b) => b,
            }
        }
        TurnBudget {
            max_provider_rounds: min(self.max_provider_rounds, other.max_provider_rounds),
            max_tool_calls: min(self.max_tool_calls, other.max_tool_calls),
            max_elapsed: min(self.max_elapsed, other.max_elapsed),
            max_accumulated_tool_result_bytes: min(
                self.max_accumulated_tool_result_bytes,
                other.max_accumulated_tool_result_bytes,
            ),
            max_context_tokens: min(self.max_context_tokens, other.max_context_tokens),
            max_cost_usd: min(self.max_cost_usd, other.max_cost_usd),
            max_total_tokens: min(self.max_total_tokens, other.max_total_tokens),
            max_round_renewals: self.max_round_renewals.min(other.max_round_renewals),
            // Only a worker's runtime has a run clock (§5), and an event
            // role's budget never does: whichever budget carries one keeps it,
            // and the narrowed `max_elapsed` above still bounds the turn.
            run_clock: self.run_clock.or(other.run_clock),
        }
    }

    /// The budget for a turn the session starts by itself because events
    /// arrived: this (session) budget narrowed by the event role's
    /// (`turn_budget.event.*`), so a `/budget` the user set still applies.
    pub fn for_event_turn(&self, config: &agent_core::config::TurnBudgetsConfig) -> TurnBudget {
        self.tighter(&TurnBudget::from_config(TurnRole::Event, config))
    }

    /// Whether a wall-clock limit can end (or checkpoint) a turn. A zero
    /// limit counts as exhausted at once and never as a time checkpoint.
    pub fn has_time_limit(&self) -> bool {
        self.max_elapsed.is_some_and(|d| !d.is_zero())
    }

    /// The wall-clock limit in seconds, for diagnostics (`None` = no limit).
    pub fn max_elapsed_secs(&self) -> Option<u64> {
        self.max_elapsed.map(|d| d.as_secs())
    }

    /// Role defaults overlaid with the typed config. Unset fields keep the
    /// compiled defaults; `unlimited` clears a round, tool-call or
    /// tool-result-byte limit (`0` means none allowed, deprecated); `0`
    /// seconds means no wall-clock limit.
    pub fn from_config(role: TurnRole, config: &agent_core::config::TurnBudgetsConfig) -> Self {
        let overrides = match role {
            TurnRole::Foreground => &config.foreground,
            TurnRole::Autonomous => &config.autonomous,
            TurnRole::Event => &config.event,
            TurnRole::Worker => &config.worker,
        };
        let mut budget = Self::for_role(role);
        if let Some(v) = overrides.max_provider_rounds {
            budget.max_provider_rounds = v.limit();
        }
        if let Some(v) = overrides.max_tool_calls {
            budget.max_tool_calls = v.limit();
        }
        if let Some(v) = overrides.max_elapsed_secs {
            budget.max_elapsed = (v > 0).then(|| Duration::from_secs(v));
        }
        if let Some(v) = overrides.max_accumulated_tool_result_bytes {
            budget.max_accumulated_tool_result_bytes = v.limit();
        }
        if overrides.max_context_tokens.is_some() {
            budget.max_context_tokens = overrides.max_context_tokens;
        }
        if overrides.max_cost_usd.is_some() {
            budget.max_cost_usd = overrides.max_cost_usd;
        }
        if let Some(v) = overrides.max_round_renewals {
            budget.max_round_renewals = v;
        }
        budget
    }
}

/// Moves an event-started turn onto the session's budget once the owner's
/// own message is delivered into it (a steer): from then on someone is
/// watching. Shared by the session actor (which sends the owner's steer) and
/// the turn's stream (which drains steering). Sending an owner steer and
/// draining steering hold the same lock, so the stream takes the budget in
/// the very drain that delivers the owner's message, never before it and
/// never for an event or note steered into the same channel.
#[derive(Debug, Default)]
pub struct OwnerSteerBudget {
    pending: std::sync::Mutex<Option<TurnBudget>>,
}

impl OwnerSteerBudget {
    /// Session side: `send` the owner's message into the turn; if it went in,
    /// the turn takes `budget` when the stream delivers it.
    pub fn send_owner_steer(&self, budget: TurnBudget, send: impl FnOnce() -> bool) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let sent = send();
        if sent {
            *pending = Some(budget);
        }
        sent
    }

    /// Stream side: run `drain` (the steering drain) and return the budget
    /// an owner message delivered by it, or by an earlier drain, carries.
    pub fn drain<R>(&self, drain: impl FnOnce() -> R) -> (R, Option<TurnBudget>) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let drained = drain();
        (drained, pending.take())
    }
}

/// Shared provider-usage counters, updated at the single authoritative
/// Usage emission sites in the transport and read by the stream loop for
/// the optional token/cost dimensions. Metadata only.
#[derive(Debug, Default)]
pub struct UsageCounters {
    /// Sum of individually priced requests, rounded UP to nanodollars.
    /// Saturation is fail-closed for any finite configured budget.
    cost_nanodollars: std::sync::atomic::AtomicU64,
    input_tokens: std::sync::atomic::AtomicU64,
    output_tokens: std::sync::atomic::AtomicU64,
    cache_read_tokens: std::sync::atomic::AtomicU64,
    cache_write_tokens: std::sync::atomic::AtomicU64,
    /// Latest request's context footprint (input + cache read + cache
    /// write) — the context-token dimension compares against this.
    latest_context_tokens: std::sync::atomic::AtomicU64,
    /// Number of requests booked; lets a caller tell whether the latest
    /// footprint belongs to the request it just sent.
    records: std::sync::atomic::AtomicU64,
    /// Cost held for side requests whose spend is not (yet) measured: an
    /// allowance reserved before sending, released when the provider reports
    /// usage, kept as a conservative charge when it never does. Distinct
    /// from the measured `cost_nanodollars`.
    reserved_nanodollars: std::sync::atomic::AtomicU64,
    /// Context footprint of the first request booked since
    /// [`Self::watch_first_context`] (telemetry: a request's first reported
    /// attempt, even if that attempt then failed or was retried).
    first_context: std::sync::Mutex<FirstContext>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum FirstContext {
    #[default]
    Idle,
    Watching,
    Seen(u64),
}

/// A cost allowance held in [`UsageCounters`]; see
/// [`UsageCounters::reserve`].
#[derive(Debug)]
#[must_use = "settle or keep the reservation explicitly"]
pub struct CostReservation {
    units: u64,
}

impl UsageCounters {
    /// Book exactly one request, using its model and optional cache TTL split.
    /// Unknown models retain the pricing table's nonzero Sonnet fallback.
    #[allow(clippy::too_many_arguments)]
    pub fn record_priced(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cache_write_5m: Option<u64>,
        cache_write_1h: Option<u64>,
    ) {
        use std::sync::atomic::Ordering;
        let cost = agent_core::pricing::calculate_cost_optional_split(
            model,
            input,
            output,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
        );
        self.book_cost_and_tokens(cost, input, output, cache_read, cache_write);
        let context = input.saturating_add(cache_read).saturating_add(cache_write);
        self.latest_context_tokens.store(context, Ordering::Relaxed);
        Self::saturating_add(&self.records, 1);
        let mut first = self
            .first_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *first == FirstContext::Watching {
            *first = FirstContext::Seen(context);
        }
    }

    /// Start recording the footprint of the next booked request.
    pub fn watch_first_context(&self) {
        *self
            .first_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = FirstContext::Watching;
    }

    /// The footprint of the first request booked since the last
    /// [`Self::watch_first_context`], if any; stops watching.
    pub fn take_first_context(&self) -> Option<u64> {
        match std::mem::take(
            &mut *self
                .first_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) {
            FirstContext::Seen(tokens) => Some(tokens),
            FirstContext::Idle | FirstContext::Watching => None,
        }
    }

    /// Book a side request (a rollover summary) into the cost and token
    /// totals only. The latest context footprint and the request count
    /// describe the conversation's own requests (context-token budget,
    /// calibration), so a side request leaves them unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn record_priced_auxiliary(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cache_write_5m: Option<u64>,
        cache_write_1h: Option<u64>,
    ) {
        let cost = agent_core::pricing::calculate_cost_optional_split(
            model,
            input,
            output,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
        );
        self.book_cost_and_tokens(cost, input, output, cache_read, cache_write);
    }

    fn book_cost_and_tokens(
        &self,
        cost: f64,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        Self::saturating_add(&self.cost_nanodollars, Self::nanodollars(cost));
        Self::saturating_add(&self.input_tokens, input);
        Self::saturating_add(&self.output_tokens, output);
        Self::saturating_add(&self.cache_read_tokens, cache_read);
        Self::saturating_add(&self.cache_write_tokens, cache_write);
    }

    /// Hold `usd` of the cost budget before sending a request whose spend
    /// is only known afterwards.
    pub fn reserve(&self, usd: f64) -> CostReservation {
        let units = Self::nanodollars(usd);
        Self::saturating_add(&self.reserved_nanodollars, units);
        CostReservation { units }
    }

    /// Hold `usd` only if measured plus reserved cost stays within
    /// `max_usd` (no limit: always): the check and the hold are one atomic
    /// step, so two callers can never both pass on the same remainder.
    /// `None`: not affordable (or `usd` is not a finite non-negative cost);
    /// nothing is held.
    ///
    /// What it guarantees: the caller's own worst case fits against the
    /// measured and held cost known when it reserves. It is atomic among
    /// reservers only, not against [`Self::record_priced`] and the other
    /// measured writers: unreserved spend booked meanwhile (an ordinary
    /// turn's usage) is not held off and can still take the total past the
    /// limit, as it could without reservations; the next `try_reserve`
    /// counts it. Settle by booking the measured spend first, then
    /// [`Self::release`] (which saturates at zero).
    pub fn try_reserve(&self, usd: f64, max_usd: Option<f64>) -> Option<CostReservation> {
        use std::sync::atomic::Ordering;
        if !usd.is_finite() || usd < 0.0 {
            return None;
        }
        let units = Self::nanodollars(usd);
        let Some(max) = max_usd else {
            Self::saturating_add(&self.reserved_nanodollars, units);
            return Some(CostReservation { units });
        };
        // Floor the limit: a remainder is never rounded up in the caller's
        // favour.
        let limit = if max.is_finite() && max >= 0.0 {
            (max * 1_000_000_000.0).floor() as u64
        } else if max == f64::INFINITY {
            u64::MAX
        } else {
            return None;
        };
        self.reserved_nanodollars
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                let measured = self.cost_nanodollars.load(Ordering::Acquire);
                let committed = measured.checked_add(held)?.checked_add(units)?;
                (committed <= limit).then_some(held + units)
            })
            .ok()
            .map(|_| CostReservation { units })
    }

    /// The provider reported the spend (booked separately): release the
    /// held allowance.
    pub fn release(&self, reservation: CostReservation) {
        use std::sync::atomic::Ordering;
        let _ =
            self.reserved_nanodollars
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                    Some(held.saturating_sub(reservation.units))
                });
    }

    /// The spend is unknown (canceled, timed out, failed without usage):
    /// the held allowance stays charged for the rest of the turn.
    pub fn keep(&self, reservation: CostReservation) {
        let _ = reservation;
    }

    /// Cost still held by reservations, in USD.
    pub fn reserved_usd(&self) -> f64 {
        Self::usd(
            self.reserved_nanodollars
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Measured plus reserved cost: what the cost budget is checked against.
    pub fn committed_cost_usd(&self) -> f64 {
        let measured = self
            .cost_nanodollars
            .load(std::sync::atomic::Ordering::Relaxed);
        let reserved = self
            .reserved_nanodollars
            .load(std::sync::atomic::Ordering::Relaxed);
        Self::usd(measured.saturating_add(reserved))
    }

    /// USD to nanodollars, rounded up; not finite or negative saturates
    /// (fail closed against any finite budget).
    pub(crate) fn nanodollars(usd: f64) -> u64 {
        let units = usd * 1_000_000_000.0;
        if !units.is_finite() || units < 0.0 {
            u64::MAX
        } else {
            units.ceil() as u64
        }
    }

    fn usd(units: u64) -> f64 {
        if units == u64::MAX {
            f64::MAX
        } else {
            units as f64 / 1_000_000_000.0
        }
    }

    pub fn records(&self) -> u64 {
        self.records.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn saturating_add(counter: &std::sync::atomic::AtomicU64, value: u64) {
        use std::sync::atomic::Ordering;
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old.saturating_add(value))
        });
    }

    pub fn cost_usd(&self) -> f64 {
        let units = self
            .cost_nanodollars
            .load(std::sync::atomic::Ordering::Relaxed);
        if units == u64::MAX {
            f64::MAX
        } else {
            units as f64 / 1_000_000_000.0
        }
    }

    pub fn latest_context_tokens(&self) -> u64 {
        self.latest_context_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Accumulated (input, output, cache_read, cache_write) totals.
    pub fn totals(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.input_tokens.load(Ordering::Relaxed),
            self.output_tokens.load(Ordering::Relaxed),
            self.cache_read_tokens.load(Ordering::Relaxed),
            self.cache_write_tokens.load(Ordering::Relaxed),
        )
    }
}

/// One turn's running budget meter. NOT thread-shared: owned by the
/// stream loop; the only cross-task input is the [`UsageCounters`] cell.
#[derive(Debug)]
pub struct TurnBudgetMeter {
    budget: TurnBudget,
    started: Instant,
    rounds: u32,
    tool_calls: u32,
    tool_result_bytes: usize,
    round_renewals: u32,
}

impl TurnBudgetMeter {
    pub fn new(budget: TurnBudget) -> Self {
        Self {
            budget,
            started: Instant::now(),
            rounds: 0,
            tool_calls: 0,
            tool_result_bytes: 0,
            round_renewals: 0,
        }
    }

    pub fn budget(&self) -> &TurnBudget {
        &self.budget
    }

    /// Switch the turn to `budget` mid-turn. Usage so far (rounds, tool
    /// calls, tool-result bytes, renewals, elapsed time) keeps counting.
    pub fn switch_budget(&mut self, budget: TurnBudget) {
        self.budget = budget;
    }

    /// Whether this turn (segment) has used its `max_elapsed`, or, on a run
    /// clock, the run's deadline has passed: both deadlines hold (strict r1
    /// A-SF). A worker turn starts no earlier than its run and the clock
    /// already holds the worker's `max_elapsed`, so on a run clock the
    /// turn's own limit comes first only when its budget was narrowed below
    /// the clock (a worker's event turn).
    pub(crate) fn wall_clock_exceeded(&self) -> bool {
        self.turn_time_exceeded()
            || self
                .budget
                .run_clock
                .as_ref()
                .is_some_and(RunClock::expired)
    }

    /// Whether this turn (segment) has used its own `max_elapsed`.
    fn turn_time_exceeded(&self) -> bool {
        self.budget
            .max_elapsed
            .is_some_and(|max| self.started.elapsed() >= max)
    }

    /// A successfully committed durable context successor starts a new time
    /// segment. Never call on an attempted/failed save or a model checkpoint.
    /// Cumulative resource/cost limits and round renewals remain unchanged.
    /// On a run clock nothing changes: a worker's rollover never renews its
    /// run deadline.
    pub(crate) fn start_context_segment(&mut self) {
        if self.budget.run_clock.is_none() {
            self.started = Instant::now();
        }
    }

    /// Whether a wall-clock expiry may become a context time checkpoint
    /// (archive the head, continue in a fresh segment) instead of a stop.
    /// Never on a run clock: the run stops at its deadline.
    pub(crate) fn time_checkpoints(&self) -> bool {
        self.budget.run_clock.is_none() && self.budget.has_time_limit()
    }

    /// Whether this meter reads a worker run's fixed clock.
    pub(crate) fn on_run_clock(&self) -> bool {
        self.budget.run_clock.is_some()
    }

    /// Charge one provider round. Checked BEFORE the provider call:
    /// wall-clock first (a stale turn must not spend another request),
    /// then the exact round cap (when one is configured).
    pub fn begin_round(&mut self) -> Result<(), BudgetDimension> {
        if self.wall_clock_exceeded() {
            return Err(BudgetDimension::WallClock);
        }
        if self
            .budget
            .max_provider_rounds
            .is_some_and(|max| self.rounds >= max)
        {
            return Err(BudgetDimension::ProviderRounds);
        }
        self.rounds = self.rounds.saturating_add(1);
        Ok(())
    }

    /// Attempt a bounded graceful extension after the provider-round cap is
    /// hit. Resets the round counter and returns the number of renewals still
    /// remaining, or `None` when the renewal budget is exhausted. Wall-clock
    /// is deliberately untouched — [`Self::begin_round`] re-checks it, so a
    /// stale turn cannot extend past `max_elapsed`. Only meaningful right
    /// after `begin_round` returned [`BudgetDimension::ProviderRounds`].
    pub fn try_renew_rounds(&mut self) -> Option<u32> {
        if self.round_renewals >= self.budget.max_round_renewals {
            return None;
        }
        self.round_renewals += 1;
        self.rounds = 0;
        Some(self.budget.max_round_renewals - self.round_renewals)
    }

    /// Number of graceful round renewals already consumed this turn.
    pub fn round_renewals_used(&self) -> u32 {
        self.round_renewals
    }

    /// Wall-clock consumed by this turn so far (on a run clock: since the
    /// run's start). Diagnostics only — the enforcement path uses
    /// [`Self::wall_clock_exceeded`].
    pub fn elapsed(&self) -> Duration {
        match &self.budget.run_clock {
            Some(clock) => clock.elapsed(),
            None => self.started.elapsed(),
        }
    }

    /// Metadata-only diagnostic; preserve the typed outcome for every frontend
    /// and autonomous driver. Exhaustion is a local stop, not provider fallback.
    pub fn exhaustion_error(&self, dimension: BudgetDimension) -> agent_core::TurnError {
        let mut error = agent_core::TurnError::budget(dimension);
        // The run's deadline, unless only the turn's own (narrower) limit
        // has run out: that stop is worded as the turn's.
        let run_stop = self
            .budget
            .run_clock
            .filter(|clock| clock.expired() || !self.turn_time_exceeded());
        if let (BudgetDimension::WallClock, Some(clock)) = (dimension, &run_stop) {
            error.message = format!(
                "worker run stopped at its {}s deadline (ran {}s). A context rollover does not extend it; \
                 the deadline is the earlier of the worker's timeout and turn_budget.worker.max_elapsed_secs.",
                clock.limit().map_or(0, |d| d.as_secs()),
                self.elapsed().as_secs(),
            );
        } else if dimension == BudgetDimension::WallClock {
            error.message = format!(
                "turn stopped at its {}s time limit (ran {}s). History is kept; send a prompt to continue. \
                 The limit comes from turn_budget.<role>.max_elapsed_secs or /budget time; /budget time off removes it.",
                self.budget.max_elapsed_secs().unwrap_or(0),
                self.started.elapsed().as_secs(),
            );
        }
        error
    }

    /// Provider rounds charged since the last renewal (reset by
    /// [`Self::try_renew_rounds`]). Diagnostics only.
    pub fn rounds_used(&self) -> u32 {
        self.rounds
    }

    /// Tool calls charged this turn. Diagnostics only.
    pub fn tool_calls_used(&self) -> u32 {
        self.tool_calls
    }

    /// Accumulated tool-result bytes charged this turn. Diagnostics only.
    pub fn tool_result_bytes_used(&self) -> usize {
        self.tool_result_bytes
    }

    /// Remaining tool-call allowance (for exact-cap batch splitting);
    /// `u32::MAX` when no tool-call limit is configured.
    pub fn remaining_tool_calls(&self) -> u32 {
        self.budget
            .max_tool_calls
            .map_or(u32::MAX, |max| max.saturating_sub(self.tool_calls))
    }

    /// Charge `n` executed tool calls (the caller split the batch against
    /// [`Self::remaining_tool_calls`], so this never over-charges).
    pub fn charge_tool_calls(&mut self, n: u32) {
        self.tool_calls = self.tool_calls.saturating_add(n);
    }

    /// Charge the bytes of one round's tool results; exceeding a configured
    /// accumulated cap exhausts the ToolResultBytes dimension.
    pub fn charge_tool_result_bytes(&mut self, bytes: usize) -> Result<(), BudgetDimension> {
        self.tool_result_bytes = self.tool_result_bytes.saturating_add(bytes);
        if self
            .budget
            .max_accumulated_tool_result_bytes
            .is_some_and(|max| self.tool_result_bytes > max)
        {
            return Err(BudgetDimension::ToolResultBytes);
        }
        Ok(())
    }

    /// Optional dimensions, consulted after each provider round from the
    /// shared usage counters. Costs are summed per request, never repriced
    /// from token totals; both checks are no-ops when unconfigured.
    pub fn check_usage(&self, usage: &UsageCounters) -> Result<(), BudgetDimension> {
        if let Some(max_context) = self.budget.max_context_tokens {
            if usage.latest_context_tokens() > max_context {
                return Err(BudgetDimension::InputTokens);
            }
        }
        if let Some(max_cost) = self.budget.max_cost_usd {
            // Reservations held for unmeasured side requests count too.
            if usage.committed_cost_usd() > max_cost {
                return Err(BudgetDimension::CostUsd);
            }
        }
        if let Some(max_tokens) = self.budget.max_total_tokens {
            let (input, output, _, _) = usage.totals();
            if input.saturating_add(output) > max_tokens {
                return Err(BudgetDimension::TotalTokens);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::config::TurnLimit;

    #[test]
    fn an_owner_steer_switches_the_budget_and_keeps_the_usage() {
        let event = TurnBudget::from_config(TurnRole::Event, &Default::default());
        let session = TurnBudget::for_role(TurnRole::Foreground);
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: Some(2),
            ..event
        });
        meter.begin_round().unwrap();
        meter.begin_round().unwrap();
        meter.charge_tool_calls(3);
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));

        let lift = OwnerSteerBudget::default();
        // Nothing sent: the drain carries no budget.
        assert_eq!(lift.drain(|| ()).1, None);
        // A send that fails (turn over) arms nothing.
        assert!(!lift.send_owner_steer(session.clone(), || false));
        assert_eq!(lift.drain(|| ()).1, None);
        assert!(lift.send_owner_steer(session.clone(), || true));
        let (_, budget) = lift.drain(|| ());
        meter.switch_budget(budget.expect("the owner's steer carries the session budget"));
        assert_eq!(lift.drain(|| ()).1, None, "taken once");

        assert_eq!(meter.budget(), &session);
        assert_eq!(meter.rounds_used(), 2, "usage keeps counting");
        assert_eq!(meter.tool_calls_used(), 3);
        meter.begin_round().unwrap();
        assert_eq!(meter.rounds_used(), 3);
    }

    #[test]
    fn reservations_count_against_the_cost_budget_but_stay_unmeasured() {
        let usage = UsageCounters::default();
        let mut budget = TurnBudget::for_role(TurnRole::Foreground);
        budget.max_cost_usd = Some(1.0);
        let meter = TurnBudgetMeter::new(budget);
        let held = usage.reserve(0.6);
        assert_eq!(usage.cost_usd(), 0.0, "a reservation is not measured spend");
        assert!((usage.committed_cost_usd() - 0.6).abs() < 1e-9);
        assert_eq!(meter.check_usage(&usage), Ok(()));
        let kept = usage.reserve(0.6);
        assert_eq!(meter.check_usage(&usage), Err(BudgetDimension::CostUsd));
        usage.release(held);
        assert!((usage.reserved_usd() - 0.6).abs() < 1e-9);
        usage.keep(kept);
        assert!(
            (usage.committed_cost_usd() - 0.6).abs() < 1e-9,
            "unknown spend stays held"
        );
        assert_eq!(usage.totals(), (0, 0, 0, 0));
    }

    #[test]
    fn a_reservation_is_checked_and_taken_in_one_step() {
        let usage = UsageCounters::default();
        // Without a cost limit every reservation is taken.
        let free = usage.try_reserve(5.0, None).expect("no limit");
        usage.release(free);
        // Earlier reservations count, and the limit is inclusive.
        let first = usage.try_reserve(0.5, Some(1.0)).expect("fits");
        assert!(
            usage.try_reserve(0.6, Some(1.0)).is_none(),
            "over the limit"
        );
        assert!(
            (usage.reserved_usd() - 0.5).abs() < 1e-9,
            "a refusal holds nothing"
        );
        let second = usage.try_reserve(0.5, Some(1.0)).expect("exactly fits");
        usage.release(first);
        usage.release(second);
        // Measured spend counts too (10k Sonnet output tokens: about $0.15).
        usage.record_priced_auxiliary("claude-sonnet-4-6", 0, 10_000, 0, 0, None, None);
        assert!(usage.cost_usd() > 0.1);
        assert!(usage.try_reserve(0.9, Some(1.0)).is_none());
        usage.release(usage.try_reserve(0.8, Some(1.0)).expect("fits"));
        assert!(usage.try_reserve(f64::NAN, Some(1.0)).is_none());
        assert!(usage.try_reserve(-1.0, Some(1.0)).is_none());

        // Concurrent callers: exactly as many as fit get one.
        let usage = std::sync::Arc::new(UsageCounters::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let taken: Vec<_> = (0..8)
            .map(|_| {
                let usage = std::sync::Arc::clone(&usage);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    usage.try_reserve(0.3, Some(1.0))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(taken.iter().filter(|r| r.is_some()).count(), 3);
        assert!((usage.reserved_usd() - 0.9).abs() < 1e-9);
        for reservation in taken.into_iter().flatten() {
            usage.release(reservation);
        }
    }

    /// What a reservation guarantees, and what it does not. Settling
    /// books, then releases: the held total drops to the other holders'
    /// and never below zero. A measured write by another thread while a
    /// reservation is held counts in the next `try_reserve`. Unreserved
    /// spend is not held off: measured plus held can pass the limit, and
    /// then nothing more is reserved.
    #[test]
    fn a_settled_reservation_books_then_releases_and_measured_spend_counts_next() {
        let usage = std::sync::Arc::new(UsageCounters::default());
        let limit = Some(1.0);
        let a = usage.try_reserve(0.4, limit).expect("fits");
        let b = usage.try_reserve(0.2, limit).expect("fits");
        // Settle `a`: book its measured spend (10k Sonnet output tokens),
        // then release its hold.
        usage.record_priced_auxiliary("claude-sonnet-4-6", 0, 10_000, 0, 0, None, None);
        let measured = usage.cost_usd();
        assert!(measured > 0.1);
        usage.release(a);
        assert!((usage.reserved_usd() - 0.2).abs() < 1e-9, "only b is held");
        assert!((usage.committed_cost_usd() - (measured + 0.2)).abs() < 1e-9);
        usage.release(b);
        assert_eq!(usage.reserved_usd(), 0.0);
        usage.release(CostReservation { units: u64::MAX });
        assert_eq!(usage.reserved_usd(), 0.0, "the held total never goes below zero");

        // Another writer books while a reservation is held.
        let held = usage.try_reserve(0.5, limit).expect("fits");
        let writer = {
            let usage = std::sync::Arc::clone(&usage);
            std::thread::spawn(move || {
                usage.record_priced_auxiliary("claude-sonnet-4-6", 0, 20_000, 0, 0, None, None)
            })
        };
        writer.join().unwrap();
        let measured = usage.cost_usd();
        let remainder = 1.0 - measured - 0.5;
        assert!(remainder > 0.02, "{remainder}");
        assert!(
            usage.try_reserve(remainder + 0.01, limit).is_none(),
            "the measured write counts"
        );
        usage.release(usage.try_reserve(remainder - 0.01, limit).expect("the rest fits"));

        // Unreserved spend is not held off by the reservation.
        usage.record_priced_auxiliary("claude-sonnet-4-6", 0, 10_000, 0, 0, None, None);
        assert!(usage.committed_cost_usd() > 1.0, "the limit can be passed");
        assert!(usage.try_reserve(0.0, limit).is_none(), "then nothing more is reserved");
        usage.release(held);
        assert_eq!(usage.reserved_usd(), 0.0);
    }

    #[test]
    fn partial_ttl_metadata_cannot_drop_cost_or_lower_tier() {
        for (input, aggregate, c5, c1, expected) in [
            (0, 100_001, Some(0), None, 0.100001),
            (0, 100_001, None, Some(0), 0.100001),
            (60_000, 40_000, Some(10_000), None, 0.01325),
            (60_000, 40_001, Some(10_000), None, 0.066251),
            (60_000, 40_001, None, Some(10_000), 0.070001),
            (60_000, 40_001, Some(10_000), Some(10_000), 0.066251),
            (60_000, 1, Some(20_000), Some(20_001), 0.062501),
            (60_000, 1, Some(40_001), None, 0.055000625),
            (60_000, 40_001, Some(40_001), None, 0.055000625),
            (60_000, 40_001, None, Some(40_001), 0.070001),
            (60_000, 40_001, Some(10_000), Some(30_001), 0.066251),
            (60_000, 40_001, None, None, 0.055000625),
        ] {
            let usage = UsageCounters::default();
            usage.record_priced("claude-haiku-5.5", input, 0, 0, aggregate, c5, c1);
            assert!(
                (usage.cost_usd() - expected).abs() < 2e-9,
                "input={input}, aggregate={aggregate}, split={c5:?}/{c1:?}"
            );
            let mut budget = TurnBudget::for_role(TurnRole::Foreground);
            budget.max_cost_usd = Some(expected / 2.0);
            assert_eq!(
                TurnBudgetMeter::new(budget).check_usage(&usage),
                Err(BudgetDimension::CostUsd)
            );
        }
        for (aggregate, c5, c1) in [
            (u64::MAX, Some(1), None),
            (1, Some(u64::MAX), Some(u64::MAX)),
        ] {
            let usage = UsageCounters::default();
            usage.record_priced("claude-haiku-5.5", 0, 0, 0, aggregate, c5, c1);
            assert_eq!(usage.cost_usd(), f64::MAX);
        }
    }

    #[test]
    fn costs_are_per_request_not_cumulative_tiers_or_current_model() {
        let usage = UsageCounters::default();
        for _ in 0..2 {
            usage.record_priced("claude-haiku-5.5", 60_000, 1_000, 0, 0, None, None);
        }
        assert!((usage.cost_usd() - 0.013).abs() < 3e-9);
        let mut budget = TurnBudget::for_role(TurnRole::Foreground);
        budget.max_cost_usd = Some(0.03);
        let meter = TurnBudgetMeter::new(budget);
        assert_eq!(meter.check_usage(&usage), Ok(()));
        usage.record_priced("claude-haiku-5-5", 100_001, 1_000, 0, 0, None, None);
        assert!((usage.cost_usd() - 0.0655005).abs() < 4e-9);
        assert_eq!(meter.check_usage(&usage), Err(BudgetDimension::CostUsd));
        usage.record_priced("claude-opus-4.1", 1_000_000, 1_000_000, 0, 0, None, None);
        usage.record_priced("claude-opus-4.8", 1_000_000, 1_000_000, 0, 0, None, None);
        assert!((usage.cost_usd() - 120.0655005).abs() < 6e-9);
        assert_eq!(usage.latest_context_tokens(), 1_000_000);
    }

    #[test]
    fn request_tier_includes_reads_and_both_write_ttls() {
        for extra in [0, 1] {
            let usage = UsageCounters::default();
            usage.record_priced(
                "claude-haiku-5-5",
                60_000,
                1_000,
                20_000 + extra,
                20_000,
                Some(5_000),
                Some(15_000),
            );
            let expected = (0.006 + 0.0002 + 0.000625 + 0.003 + 0.0005 + extra as f64 * 0.00000001)
                * if extra == 0 { 1.0 } else { 5.0 };
            assert!((usage.cost_usd() - expected).abs() < 2e-9);
            assert_eq!(usage.latest_context_tokens(), 100_000 + extra);
        }
    }

    #[test]
    fn unknown_models_and_saturation_cannot_silently_zero_cost() {
        let usage = UsageCounters::default();
        usage.record_priced("unknown-model", 1_000_000, 0, 0, 0, None, None);
        assert!((usage.cost_usd() - 3.0).abs() < 2e-9);
        for _ in 0..2 {
            usage.record_priced(
                "claude-opus-4.1",
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                None,
                None,
            );
        }
        assert_eq!(usage.totals(), (u64::MAX, u64::MAX, u64::MAX, u64::MAX));
        assert_eq!(usage.cost_usd(), f64::MAX);
        assert_eq!(usage.latest_context_tokens(), u64::MAX);
    }

    #[test]
    fn role_defaults_are_typed_and_only_foreground_is_unlimited() {
        let fg = TurnBudget::for_role(TurnRole::Foreground);
        let auto = TurnBudget::for_role(TurnRole::Autonomous);
        let worker = TurnBudget::for_role(TurnRole::Worker);
        // Interactive turns have no round, tool-call or tool-result-byte
        // limit unless one is configured (#219).
        assert_eq!(fg.max_provider_rounds, None);
        assert_eq!(fg.max_tool_calls, None);
        assert_eq!(fg.max_accumulated_tool_result_bytes, None);
        // No role has a wall-clock limit unless one is configured.
        for budget in [&fg, &auto, &worker] {
            assert_eq!(budget.max_elapsed, None);
            assert!(!budget.has_time_limit());
        }
        assert!(fg.max_context_tokens.is_none());
        assert!(fg.max_cost_usd.is_none());
        // Graceful continuation is most generous for foreground (human
        // watching) and disabled for autonomous (nobody watching).
        assert_eq!(auto.max_round_renewals, 0);
        assert!(worker.max_round_renewals > 0);
        assert!(fg.max_round_renewals >= worker.max_round_renewals);
    }

    #[test]
    fn autonomous_role_keeps_finite_per_turn_defaults() {
        // Nobody watches a watcher turn and its session limits are checked
        // between turns, so one turn must stay bounded by default.
        let auto = TurnBudget::for_role(TurnRole::Autonomous);
        assert_eq!(auto.max_provider_rounds, Some(24));
        assert_eq!(auto.max_tool_calls, Some(96));
        assert_eq!(
            auto.max_accumulated_tool_result_bytes,
            Some(8 * 1024 * 1024)
        );
        assert_eq!(auto.max_round_renewals, 0);

        let mut meter = TurnBudgetMeter::new(auto);
        for _ in 0..24 {
            assert_eq!(meter.begin_round(), Ok(()));
        }
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), None);
        assert_eq!(meter.remaining_tool_calls(), 96);
    }

    #[test]
    fn event_role_uses_the_watcher_numbers_and_its_own_config() {
        // Turns started by events have nobody typing; they get the watcher's
        // per-turn numbers by default, configurable under turn_budget.event.
        let event = TurnBudget::for_role(TurnRole::Event);
        assert_eq!(event, TurnBudget::for_role(TurnRole::Autonomous));

        let cfg = agent_core::config::load_config_from_str(
            "turn_budget.event.max_provider_rounds = unlimited\n\
             turn_budget.event.max_tool_calls = 300\n\
             turn_budget.autonomous.max_tool_calls = 5\n",
        )
        .turn_budgets;
        let event = TurnBudget::from_config(TurnRole::Event, &cfg);
        assert_eq!(event.max_provider_rounds, None);
        assert_eq!(event.max_tool_calls, Some(300));
        assert_eq!(
            event.max_accumulated_tool_result_bytes,
            Some(8 * 1024 * 1024)
        );
        // The watcher role is configured separately.
        assert_eq!(
            TurnBudget::from_config(TurnRole::Autonomous, &cfg).max_tool_calls,
            Some(5)
        );
    }

    #[test]
    fn tighter_takes_the_smaller_limit_per_dimension() {
        let session = TurnBudget {
            max_elapsed: Some(Duration::from_secs(600)),
            max_cost_usd: Some(2.0),
            max_tool_calls: Some(500),
            ..TurnBudget::for_role(TurnRole::Foreground)
        };
        let event = TurnBudget {
            max_cost_usd: Some(5.0),
            max_total_tokens: Some(1_000),
            ..TurnBudget::for_role(TurnRole::Event)
        };
        let combined = session.tighter(&event);
        assert_eq!(combined.max_provider_rounds, Some(24));
        assert_eq!(combined.max_tool_calls, Some(96));
        assert_eq!(
            combined.max_accumulated_tool_result_bytes,
            Some(8 * 1024 * 1024)
        );
        assert_eq!(combined.max_elapsed, Some(Duration::from_secs(600)));
        assert_eq!(combined.max_cost_usd, Some(2.0));
        assert_eq!(combined.max_total_tokens, Some(1_000));
        assert_eq!(combined.max_context_tokens, None);
        assert_eq!(combined.max_round_renewals, 0);
        assert_eq!(combined, event.tighter(&session));
    }

    #[test]
    fn total_tokens_limit_counts_uncached_input_and_output() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_total_tokens: Some(100),
            ..TurnBudget::for_role(TurnRole::Autonomous)
        });
        let usage = UsageCounters::default();
        // Cache traffic does not count toward the token limit.
        usage.record_priced("claude-sonnet-4-5", 40, 60, 50_000, 50_000, None, None);
        assert_eq!(meter.check_usage(&usage), Ok(()));
        usage.record_priced("claude-sonnet-4-5", 1, 0, 0, 0, None, None);
        assert_eq!(meter.check_usage(&usage), Err(BudgetDimension::TotalTokens));
        meter.charge_tool_calls(0);
    }

    #[test]
    fn worker_role_keeps_a_finite_round_cap_of_192() {
        // 64 rounds per allowance x (1 + 2 renewals) = the pre-#219 cap.
        let worker = TurnBudget::for_role(TurnRole::Worker);
        assert_eq!(worker.max_provider_rounds, Some(64));
        assert_eq!(worker.max_round_renewals, 2);
        let mut meter = TurnBudgetMeter::new(worker);
        let mut rounds = 0u32;
        loop {
            match meter.begin_round() {
                Ok(()) => rounds += 1,
                Err(BudgetDimension::ProviderRounds) => {
                    if meter.try_renew_rounds().is_none() {
                        break;
                    }
                }
                Err(other) => panic!("unexpected dimension {other:?}"),
            }
            assert!(rounds <= 10_000, "worker round cap must be finite");
        }
        assert_eq!(rounds, 192);
    }

    #[test]
    fn config_overlays_only_set_fields() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.autonomous.max_provider_rounds = Some(TurnLimit::Max(2));
        cfg.autonomous.max_elapsed_secs = Some(9);
        cfg.autonomous.max_round_renewals = Some(3);
        let budget = TurnBudget::from_config(TurnRole::Autonomous, &cfg);
        assert_eq!(budget.max_provider_rounds, Some(2));
        assert_eq!(budget.max_elapsed, Some(Duration::from_secs(9)));
        assert_eq!(budget.max_round_renewals, 3);
        assert_eq!(
            budget.max_tool_calls,
            TurnBudget::for_role(TurnRole::Autonomous).max_tool_calls
        );
    }

    #[test]
    fn zero_seconds_in_config_means_no_time_limit() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.foreground.max_elapsed_secs = Some(0);
        let budget = TurnBudget::from_config(TurnRole::Foreground, &cfg);
        assert_eq!(budget.max_elapsed, None);
    }

    #[test]
    fn config_sets_round_tool_call_and_byte_limits_and_unlimited_clears_them() {
        for role in [
            TurnRole::Foreground,
            TurnRole::Autonomous,
            TurnRole::Event,
            TurnRole::Worker,
        ] {
            let mut cfg = agent_core::config::TurnBudgetsConfig::default();
            let overrides = match role {
                TurnRole::Foreground => &mut cfg.foreground,
                TurnRole::Autonomous => &mut cfg.autonomous,
                TurnRole::Event => &mut cfg.event,
                TurnRole::Worker => &mut cfg.worker,
            };
            overrides.max_provider_rounds = Some(TurnLimit::Max(5));
            overrides.max_tool_calls = Some(TurnLimit::Max(7));
            overrides.max_accumulated_tool_result_bytes = Some(TurnLimit::Max(11));
            let budget = TurnBudget::from_config(role, &cfg);
            assert_eq!(budget.max_provider_rounds, Some(5));
            assert_eq!(budget.max_tool_calls, Some(7));
            assert_eq!(budget.max_accumulated_tool_result_bytes, Some(11));

            let overrides = match role {
                TurnRole::Foreground => &mut cfg.foreground,
                TurnRole::Autonomous => &mut cfg.autonomous,
                TurnRole::Event => &mut cfg.event,
                TurnRole::Worker => &mut cfg.worker,
            };
            overrides.max_provider_rounds = Some(TurnLimit::Unlimited);
            overrides.max_tool_calls = Some(TurnLimit::Unlimited);
            overrides.max_accumulated_tool_result_bytes = Some(TurnLimit::Unlimited);
            let budget = TurnBudget::from_config(role, &cfg);
            assert_eq!(budget.max_provider_rounds, None);
            assert_eq!(budget.max_tool_calls, None);
            assert_eq!(budget.max_accumulated_tool_result_bytes, None);
        }
    }

    #[test]
    fn configured_limits_are_enforced_exactly() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.worker.max_provider_rounds = Some(TurnLimit::Max(2));
        cfg.worker.max_round_renewals = Some(1);
        cfg.worker.max_tool_calls = Some(TurnLimit::Max(3));
        cfg.worker.max_accumulated_tool_result_bytes = Some(TurnLimit::Max(100));
        let mut meter = TurnBudgetMeter::new(TurnBudget::from_config(TurnRole::Worker, &cfg));
        for _ in 0..2 {
            assert_eq!(meter.begin_round(), Ok(()));
        }
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), Some(0));
        for _ in 0..2 {
            assert_eq!(meter.begin_round(), Ok(()));
        }
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), None);

        assert_eq!(meter.remaining_tool_calls(), 3);
        meter.charge_tool_calls(3);
        assert_eq!(meter.remaining_tool_calls(), 0);

        assert_eq!(meter.charge_tool_result_bytes(100), Ok(()));
        assert_eq!(
            meter.charge_tool_result_bytes(1),
            Err(BudgetDimension::ToolResultBytes)
        );
    }

    #[test]
    fn zero_in_config_means_none_allowed_for_every_role() {
        let cfg = agent_core::config::load_config_from_str(
            "turn_budget.foreground.max_provider_rounds = 0\n\
             turn_budget.foreground.max_tool_calls = 0\n\
             turn_budget.foreground.max_accumulated_tool_result_bytes = 0\n\
             turn_budget.autonomous.max_provider_rounds = 0\n\
             turn_budget.autonomous.max_tool_calls = 0\n\
             turn_budget.autonomous.max_accumulated_tool_result_bytes = 0\n\
             turn_budget.worker.max_provider_rounds = 0\n\
             turn_budget.worker.max_tool_calls = 0\n\
             turn_budget.worker.max_accumulated_tool_result_bytes = 0\n\
             turn_budget.event.max_provider_rounds = 0\n\
             turn_budget.event.max_tool_calls = 0\n\
             turn_budget.event.max_accumulated_tool_result_bytes = 0\n",
        )
        .turn_budgets;
        for role in [
            TurnRole::Foreground,
            TurnRole::Autonomous,
            TurnRole::Event,
            TurnRole::Worker,
        ] {
            let budget = TurnBudget::from_config(role, &cfg);
            assert_eq!(budget.max_provider_rounds, Some(0), "{role:?}");
            assert_eq!(budget.max_tool_calls, Some(0), "{role:?}");
            assert_eq!(
                budget.max_accumulated_tool_result_bytes,
                Some(0),
                "{role:?}"
            );
            let mut meter = TurnBudgetMeter::new(budget);
            assert_eq!(meter.remaining_tool_calls(), 0);
            assert_eq!(
                meter.charge_tool_result_bytes(1),
                Err(BudgetDimension::ToolResultBytes)
            );
            // Renewals cannot turn a zero round allowance into rounds.
            loop {
                match meter.begin_round() {
                    Ok(()) => panic!("{role:?}: a zero round limit allows no rounds"),
                    Err(BudgetDimension::ProviderRounds) => {
                        if meter.try_renew_rounds().is_none() {
                            break;
                        }
                    }
                    Err(other) => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    #[test]
    fn configured_unlimited_never_exhausts() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.autonomous.max_provider_rounds = Some(TurnLimit::Unlimited);
        cfg.autonomous.max_tool_calls = Some(TurnLimit::Unlimited);
        cfg.autonomous.max_accumulated_tool_result_bytes = Some(TurnLimit::Unlimited);
        let mut meter = TurnBudgetMeter::new(TurnBudget::from_config(TurnRole::Autonomous, &cfg));
        for _ in 0..1_000 {
            assert_eq!(meter.begin_round(), Ok(()));
        }
        assert_eq!(meter.remaining_tool_calls(), u32::MAX);
        meter.charge_tool_calls(1_000);
        assert_eq!(meter.remaining_tool_calls(), u32::MAX);
        assert_eq!(meter.charge_tool_result_bytes(1 << 20), Ok(()));
    }

    #[test]
    fn an_unlimited_meter_never_exhausts_but_still_counts() {
        let mut unlimited = agent_core::config::TurnBudgetsConfig::default();
        unlimited.autonomous.max_provider_rounds = Some(TurnLimit::Unlimited);
        unlimited.autonomous.max_tool_calls = Some(TurnLimit::Unlimited);
        unlimited.autonomous.max_accumulated_tool_result_bytes = Some(TurnLimit::Unlimited);
        unlimited.worker.max_provider_rounds = Some(TurnLimit::Unlimited);
        for budget in [
            TurnBudget::for_role(TurnRole::Foreground),
            TurnBudget::from_config(TurnRole::Autonomous, &unlimited),
            TurnBudget::from_config(TurnRole::Worker, &unlimited),
        ] {
            let mut meter = TurnBudgetMeter::new(budget);
            for _ in 0..10_000 {
                assert_eq!(meter.begin_round(), Ok(()));
                assert_eq!(meter.remaining_tool_calls(), u32::MAX);
                meter.charge_tool_calls(1);
                assert_eq!(meter.charge_tool_result_bytes(64 * 1024), Ok(()));
            }
            assert_eq!(meter.rounds_used(), 10_000);
            assert_eq!(meter.tool_calls_used(), 10_000);
            assert_eq!(meter.tool_result_bytes_used(), 10_000 * 64 * 1024);
            assert_eq!(meter.round_renewals_used(), 0);
            assert_eq!(meter.check_usage(&UsageCounters::default()), Ok(()));
        }
    }

    #[test]
    fn a_turn_without_a_time_limit_never_runs_out_of_time() {
        let mut meter = TurnBudgetMeter::new(TurnBudget::for_role(TurnRole::Foreground));
        meter.started = Instant::now() - Duration::from_secs(30 * 24 * 60 * 60);
        assert!(!meter.wall_clock_exceeded());
        assert_eq!(meter.begin_round(), Ok(()));
    }

    #[test]
    fn committed_context_segment_resets_only_time() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_elapsed: Some(Duration::from_secs(7200)),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        meter.started = Instant::now() - Duration::from_secs(7201);
        meter.charge_tool_calls(3);
        meter.charge_tool_result_bytes(1234).unwrap();
        meter.try_renew_rounds();
        let original = meter.budget().clone();
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        meter.start_context_segment();
        assert!(meter.begin_round().is_ok());
        assert_eq!(meter.budget(), &original);
        assert_eq!(meter.tool_calls_used(), 3);
        assert_eq!(meter.tool_result_bytes_used(), 1234);
        assert_eq!(meter.round_renewals_used(), 1);
    }

    #[test]
    fn exhaustion_diagnostic_keeps_typed_outcome_and_reports_metadata() {
        let meter = TurnBudgetMeter::new(TurnBudget {
            max_elapsed: Some(Duration::from_secs(7200)),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        let error = meter.exhaustion_error(BudgetDimension::WallClock);
        assert_eq!(
            error.outcome,
            agent_core::TurnOutcome::BudgetExceeded {
                dimension: BudgetDimension::WallClock
            }
        );
        assert!(error.message.contains("its 7200s time limit (ran "));
        assert!(error.message.contains("History is kept"));
        assert!(error.message.contains("/budget time off"));
        // Other dimensions keep the existing diagnostic and exact typed label.
        for dimension in [
            BudgetDimension::ToolCalls,
            BudgetDimension::ProviderRounds,
            BudgetDimension::ToolResultBytes,
            BudgetDimension::InputTokens,
            BudgetDimension::OutputTokens,
            BudgetDimension::CostUsd,
        ] {
            assert_eq!(
                meter.exhaustion_error(dimension),
                agent_core::TurnError::budget(dimension)
            );
        }
    }

    #[test]
    fn meter_enforces_exact_rounds_and_wall_clock_order() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: Some(2),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert!(meter.begin_round().is_ok());
        assert!(meter.begin_round().is_ok());
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));

        let mut stale = TurnBudgetMeter::new(TurnBudget {
            max_elapsed: Some(Duration::ZERO),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        // Wall clock outranks the round check.
        assert_eq!(stale.begin_round(), Err(BudgetDimension::WallClock));
    }

    #[test]
    fn round_renewals_extend_a_bounded_number_of_times() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: Some(1),
            max_round_renewals: 2,
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        // First round of the base allowance.
        assert!(meter.begin_round().is_ok());
        // Exhausted → renew #1, then a fresh round is available.
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), Some(1));
        assert!(meter.begin_round().is_ok());
        // Exhausted again → renew #2 (last one).
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), Some(0));
        assert!(meter.begin_round().is_ok());
        // Exhausted a third time → no renewals left: hard stop.
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        assert_eq!(meter.try_renew_rounds(), None);
        assert_eq!(meter.round_renewals_used(), 2);
    }

    /// D (§5): a worker run's deadline is the earlier of its timeout and its
    /// configured `max_elapsed`, both from one run start (timeout zero: none).
    #[test]
    fn d1_a_run_clock_ends_at_the_earlier_of_timeout_and_max_elapsed() {
        let t0 = tokio::time::Instant::now();
        let s = Duration::from_secs;
        let at = |timeout, max| RunClock::started_at(t0, timeout, max).deadline();
        assert_eq!(at(Some(s(10)), Some(s(3))), Some(t0 + s(3)));
        assert_eq!(at(Some(s(3)), Some(s(10))), Some(t0 + s(3)));
        assert_eq!(at(Some(s(7)), None), Some(t0 + s(7)));
        assert_eq!(at(None, Some(s(5))), Some(t0 + s(5)));
        assert_eq!(at(Some(s(0)), Some(s(5))), Some(t0 + s(5)), "timeout 0 is none");
        assert_eq!(at(Some(s(0)), None), None);
        assert_eq!(at(None, None), None);
        assert_eq!(at(None, Some(Duration::ZERO)), Some(t0), "zero max_elapsed: at once");
        assert_eq!(at(Some(Duration::MAX), None), None, "unrepresentable: no limit");
        let clock = RunClock::started_at(t0, Some(s(10)), Some(s(3)));
        assert_eq!((clock.started(), clock.limit()), (t0, Some(s(3))));
    }

    /// D (§5): a worker run's clock is never renewed by a committed
    /// rollover, never turns into a time checkpoint, and resets no counter.
    #[test]
    fn d2_a_run_clock_is_never_renewed_and_never_a_time_checkpoint() {
        let started = tokio::time::Instant::now() - Duration::from_secs(10);
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            // Part of the clock when it is started; the meter reads the clock.
            max_elapsed: Some(Duration::from_secs(3600)),
            run_clock: Some(RunClock::started_at(started, Some(Duration::from_secs(5)), None)),
            ..TurnBudget::for_role(TurnRole::Worker)
        });
        meter.charge_tool_calls(3);
        meter.charge_tool_result_bytes(1234).unwrap();
        meter.try_renew_rounds();
        let original = meter.budget().clone();
        assert!(meter.wall_clock_exceeded(), "the run's deadline, not a turn clock");
        assert!(!meter.time_checkpoints());
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        meter.start_context_segment();
        assert!(meter.wall_clock_exceeded(), "a committed rollover renews nothing");
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        assert_eq!(meter.budget(), &original);
        assert_eq!(meter.tool_calls_used(), 3);
        assert_eq!(meter.tool_result_bytes_used(), 1234);
        assert_eq!(meter.round_renewals_used(), 1);
        assert!(meter.elapsed() >= Duration::from_secs(10), "elapsed since the run start");
        let error = meter.exhaustion_error(BudgetDimension::WallClock);
        assert_eq!(
            error.outcome,
            agent_core::TurnOutcome::BudgetExceeded {
                dimension: BudgetDimension::WallClock
            }
        );
        assert!(error.message.contains("its 5s deadline"), "{}", error.message);

        // A run clock without a deadline never runs out by itself and never
        // checkpoints on time; a turn's own `max_elapsed` (a budget narrowed
        // below the clock) still ends that turn (strict r1 A-SF).
        let mut open = TurnBudgetMeter::new(TurnBudget {
            max_elapsed: Some(Duration::from_secs(1)),
            run_clock: Some(RunClock::started_at(started, None, None)),
            ..TurnBudget::for_role(TurnRole::Worker)
        });
        assert!(!open.wall_clock_exceeded());
        assert!(!open.time_checkpoints());
        assert_eq!(open.begin_round(), Ok(()));
        open.started = Instant::now() - Duration::from_secs(2);
        assert!(open.wall_clock_exceeded(), "the turn's own limit");
        assert!(!open.time_checkpoints());
    }

    /// Strict r1 A-SF: a clock-bearing worker's event turn keeps BOTH
    /// deadlines. `tighter` keeps the run clock and narrows `max_elapsed` to
    /// the event limit; whichever comes first ends the turn, and the stop is
    /// worded by the deadline that passed.
    #[test]
    fn a_run_clock_turn_narrowed_below_its_clock_stops_at_the_shorter_limit() {
        let hour = Duration::from_secs(3600);
        let minute = Duration::from_secs(60);
        let worker = TurnBudget {
            max_elapsed: Some(hour),
            run_clock: Some(RunClock::start(Some(hour), Some(hour))),
            ..TurnBudget::for_role(TurnRole::Worker)
        };
        let event = TurnBudget {
            max_elapsed: Some(minute),
            ..TurnBudget::for_role(TurnRole::Event)
        };
        let narrowed = worker.tighter(&event);
        assert_eq!(narrowed.run_clock, worker.run_clock, "the run clock is kept");
        assert_eq!(narrowed.max_elapsed, Some(minute), "narrowed to the event limit");

        // The event turn's minute passes long before the run's hour.
        let mut meter = TurnBudgetMeter::new(narrowed.clone());
        assert_eq!(meter.begin_round(), Ok(()));
        meter.started = Instant::now() - Duration::from_secs(61);
        assert!(meter.wall_clock_exceeded(), "the event turn's own limit");
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        assert!(!meter.time_checkpoints(), "still never a checkpoint on a run clock");
        let message = meter.exhaustion_error(BudgetDimension::WallClock).message;
        assert!(message.contains("turn stopped at its 60s time limit (ran 61s)"), "{message}");

        // The run's deadline still ends the turn when it comes first.
        let clock = RunClock::start(Some(hour), Some(hour)).controlled();
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            run_clock: Some(clock),
            ..narrowed
        });
        assert_eq!(meter.begin_round(), Ok(()));
        clock.advance(hour);
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        let message = meter.exhaustion_error(BudgetDimension::WallClock).message;
        assert!(message.contains("worker run stopped at its 3600s deadline"), "{message}");
    }

    /// The test seam: a controlled clock's deadline passes when its test
    /// advances it, in every copy (the meter's included), and only there.
    #[test]
    fn a_controlled_run_clock_moves_only_when_advanced() {
        let t0 = tokio::time::Instant::now();
        let hour = Duration::from_secs(3600);
        let real = RunClock::started_at(t0, Some(hour), None);
        let clock = real.controlled();
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            run_clock: Some(clock),
            ..TurnBudget::for_role(TurnRole::Worker)
        });
        assert!(!clock.expired());
        assert_eq!(meter.begin_round(), Ok(()));
        clock.advance(hour);
        assert!(clock.expired(), "at its deadline");
        assert!(meter.wall_clock_exceeded(), "the meter's copy moved too");
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
        assert!(meter.elapsed() >= hour);
        assert!(!real.expired(), "an uncontrolled clock reads the real time");
        assert!(!real.controlled().expired(), "each controlled clock moves alone");
    }

    /// D (§5): request timers and body gates get the run's own deadline
    /// instant. A controlled clock converts the time it was advanced by
    /// into an earlier instant on the real monotonic clock.
    #[test]
    fn the_timer_deadline_is_the_run_deadline() {
        let t0 = tokio::time::Instant::now();
        let s = Duration::from_secs;
        let clock = RunClock::started_at(t0, Some(s(60)), None);
        assert_eq!(clock.timer_deadline(), clock.deadline(), "unchanged");
        assert_eq!(RunClock::started_at(t0, None, None).timer_deadline(), None);
        let controlled = clock.controlled();
        assert_eq!(controlled.timer_deadline(), clock.deadline(), "not advanced yet");
        controlled.advance(s(45));
        assert_eq!(controlled.timer_deadline(), Some(t0 + s(15)));
        assert_eq!(clock.timer_deadline(), clock.deadline(), "each clock alone");
    }

    #[test]
    fn wall_clock_still_bounds_a_renewed_turn() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: Some(1),
            max_round_renewals: 5,
            max_elapsed: Some(Duration::ZERO),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        // Renewal resets the round counter, but begin_round re-checks the
        // wall clock — a stale turn cannot extend past max_elapsed.
        assert_eq!(meter.try_renew_rounds(), Some(4));
        assert_eq!(meter.begin_round(), Err(BudgetDimension::WallClock));
    }

    #[test]
    fn autonomous_role_has_no_graceful_renewals() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: Some(1),
            ..TurnBudget::for_role(TurnRole::Autonomous)
        });
        assert!(meter.begin_round().is_ok());
        assert_eq!(meter.begin_round(), Err(BudgetDimension::ProviderRounds));
        // Nobody watching: the round cap is a hard stop.
        assert_eq!(meter.try_renew_rounds(), None);
    }

    #[test]
    fn meter_tool_calls_and_bytes_are_exact() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_tool_calls: Some(3),
            max_accumulated_tool_result_bytes: Some(10),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert_eq!(meter.remaining_tool_calls(), 3);
        meter.charge_tool_calls(2);
        assert_eq!(meter.remaining_tool_calls(), 1);
        meter.charge_tool_calls(1);
        assert_eq!(meter.remaining_tool_calls(), 0);

        assert!(meter.charge_tool_result_bytes(10).is_ok());
        assert_eq!(
            meter.charge_tool_result_bytes(1),
            Err(BudgetDimension::ToolResultBytes)
        );
    }

    #[test]
    fn first_context_is_the_first_booking_after_watch_and_side_requests_do_not_count() {
        let usage = UsageCounters::default();
        usage.record_priced("claude-sonnet-4-5", 1, 0, 0, 0, None, None);
        assert_eq!(usage.take_first_context(), None, "not watching");
        usage.watch_first_context();
        assert_eq!(usage.take_first_context(), None, "nothing booked yet");
        usage.watch_first_context();
        usage.record_priced_auxiliary("claude-sonnet-4-5", 9, 0, 0, 0, None, None);
        usage.record_priced("claude-sonnet-4-5", 100, 5, 10, 5, None, None);
        usage.record_priced("claude-sonnet-4-5", 200, 5, 0, 0, None, None);
        assert_eq!(usage.take_first_context(), Some(115), "first attempt wins");
        assert_eq!(usage.take_first_context(), None, "taken once");
        assert_eq!(usage.latest_context_tokens(), 200);
    }

    #[test]
    fn usage_counters_drive_optional_dimensions() {
        let usage = UsageCounters::default();
        usage.record_priced("claude-sonnet-4-5", 100, 50, 10, 5, None, None);
        assert_eq!(usage.latest_context_tokens(), 115);

        let meter = TurnBudgetMeter::new(TurnBudget {
            max_context_tokens: Some(100),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert_eq!(meter.check_usage(&usage), Err(BudgetDimension::InputTokens));

        let cost_meter = TurnBudgetMeter::new(TurnBudget {
            max_cost_usd: Some(0.0),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert_eq!(
            cost_meter.check_usage(&usage),
            Err(BudgetDimension::CostUsd)
        );
        let unlimited = TurnBudgetMeter::new(TurnBudget::for_role(TurnRole::Foreground));
        assert!(unlimited.check_usage(&usage).is_ok());
    }
}
