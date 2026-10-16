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

/// Per-turn limits (spec §8.1).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnBudget {
    pub max_provider_rounds: u32,
    pub max_tool_calls: u32,
    /// Wall-clock limit for one turn. `None` (the default for every role)
    /// means no limit: a turn that is still making progress is never cut off
    /// for taking long. Opt in with `turn_budget.<role>.max_elapsed_secs` or
    /// `/budget time`. Rounds, tool calls and tool-result bytes stay bounded.
    pub max_elapsed: Option<Duration>,
    pub max_accumulated_tool_result_bytes: usize,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    /// Bounded auto-renewals of the provider-round allowance (spec §8.1).
    /// On exhaustion the stream loop may reset the round counter this many
    /// times so a long, legitimate task continues instead of hard-failing.
    /// Round renewal never resets wall-clock (durable context successors do); a
    /// value of `0` means no graceful continuation (immediate hard stop).
    pub max_round_renewals: u32,
}

/// Execution role a turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// Interactive foreground session.
    Foreground,
    /// Autonomous/watcher sessions (no human in the loop per turn).
    Autonomous,
    /// Delegated worker/subagent sessions.
    Worker,
}

impl TurnBudget {
    /// Compiled per-role defaults. Foreground is the most generous (a
    /// human is watching); autonomous is the tightest (nobody is);
    /// workers sit between (bounded delegation with a supervising turn).
    pub fn for_role(role: TurnRole) -> Self {
        match role {
            TurnRole::Foreground => Self {
                max_provider_rounds: 128,
                max_tool_calls: 512,
                max_elapsed: None,
                max_accumulated_tool_result_bytes: 32 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
                // A human is watching and can interrupt; let long agentic
                // tasks continue through several checkpoints.
                max_round_renewals: 8,
            },
            TurnRole::Autonomous => Self {
                max_provider_rounds: 24,
                max_tool_calls: 96,
                max_elapsed: None,
                max_accumulated_tool_result_bytes: 8 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
                // Nobody is watching: no graceful extension — the round cap is
                // a hard stop.
                max_round_renewals: 0,
            },
            TurnRole::Worker => Self {
                max_provider_rounds: 64,
                max_tool_calls: 256,
                max_elapsed: None,
                max_accumulated_tool_result_bytes: 16 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
                // Bounded delegation under a supervising turn.
                max_round_renewals: 2,
            },
        }
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

    /// Role defaults overlaid with the typed config (unset fields keep
    /// the compiled defaults).
    pub fn from_config(role: TurnRole, config: &agent_core::config::TurnBudgetsConfig) -> Self {
        let overrides = match role {
            TurnRole::Foreground => &config.foreground,
            TurnRole::Autonomous => &config.autonomous,
            TurnRole::Worker => &config.worker,
        };
        let mut budget = Self::for_role(role);
        if let Some(v) = overrides.max_provider_rounds {
            budget.max_provider_rounds = v;
        }
        if let Some(v) = overrides.max_tool_calls {
            budget.max_tool_calls = v;
        }
        if let Some(v) = overrides.max_elapsed_secs {
            // 0 = no wall-clock limit.
            budget.max_elapsed = (v > 0).then(|| Duration::from_secs(v));
        }
        if let Some(v) = overrides.max_accumulated_tool_result_bytes {
            budget.max_accumulated_tool_result_bytes = v;
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
        let units = cost * 1_000_000_000.0;
        let units = if !units.is_finite() || units < 0.0 {
            u64::MAX
        } else {
            units.ceil() as u64
        };
        Self::saturating_add(&self.cost_nanodollars, units);
        Self::saturating_add(&self.input_tokens, input);
        Self::saturating_add(&self.output_tokens, output);
        Self::saturating_add(&self.cache_read_tokens, cache_read);
        Self::saturating_add(&self.cache_write_tokens, cache_write);
        self.latest_context_tokens.store(
            input.saturating_add(cache_read).saturating_add(cache_write),
            Ordering::Relaxed,
        );
        Self::saturating_add(&self.records, 1);
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

    pub(crate) fn wall_clock_exceeded(&self) -> bool {
        self.budget
            .max_elapsed
            .is_some_and(|max| self.started.elapsed() >= max)
    }

    /// A successfully committed durable context successor starts a new time
    /// segment. Never call on an attempted/failed save or a model checkpoint.
    /// Cumulative resource/cost limits and round renewals remain unchanged.
    pub(crate) fn start_context_segment(&mut self) {
        self.started = Instant::now();
    }

    /// Charge one provider round. Checked BEFORE the provider call:
    /// wall-clock first (a stale turn must not spend another request),
    /// then the exact round cap.
    pub fn begin_round(&mut self) -> Result<(), BudgetDimension> {
        if self.wall_clock_exceeded() {
            return Err(BudgetDimension::WallClock);
        }
        if self.rounds >= self.budget.max_provider_rounds {
            return Err(BudgetDimension::ProviderRounds);
        }
        self.rounds += 1;
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

    /// Wall-clock consumed by this turn so far. Diagnostics only — the
    /// enforcement path uses [`Self::wall_clock_exceeded`].
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Metadata-only diagnostic; preserve the typed outcome for every frontend
    /// and autonomous driver. Exhaustion is a local stop, not provider fallback.
    pub fn exhaustion_error(&self, dimension: BudgetDimension) -> agent_core::TurnError {
        let mut error = agent_core::TurnError::budget(dimension);
        if dimension == BudgetDimension::WallClock {
            error.message = format!(
                "turn stopped at its {}s time limit (ran {}s). History is kept; send a prompt to continue. \
                 The limit comes from turn_budget.<role>.max_elapsed_secs or /budget time; /budget time off removes it.",
                self.budget.max_elapsed_secs().unwrap_or(0),
                self.elapsed().as_secs(),
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

    /// Remaining tool-call allowance (for exact-cap batch splitting).
    pub fn remaining_tool_calls(&self) -> u32 {
        self.budget.max_tool_calls.saturating_sub(self.tool_calls)
    }

    /// Charge `n` executed tool calls (the caller split the batch against
    /// [`Self::remaining_tool_calls`], so this never over-charges).
    pub fn charge_tool_calls(&mut self, n: u32) {
        self.tool_calls = self.tool_calls.saturating_add(n);
    }

    /// Charge the bytes of one round's tool results; exceeding the
    /// accumulated cap exhausts the ToolResultBytes dimension.
    pub fn charge_tool_result_bytes(&mut self, bytes: usize) -> Result<(), BudgetDimension> {
        self.tool_result_bytes = self.tool_result_bytes.saturating_add(bytes);
        if self.tool_result_bytes > self.budget.max_accumulated_tool_result_bytes {
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
            if usage.cost_usd() > max_cost {
                return Err(BudgetDimension::CostUsd);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn role_defaults_are_ordered_and_typed() {
        let fg = TurnBudget::for_role(TurnRole::Foreground);
        let auto = TurnBudget::for_role(TurnRole::Autonomous);
        let worker = TurnBudget::for_role(TurnRole::Worker);
        assert!(auto.max_provider_rounds < worker.max_provider_rounds);
        assert!(worker.max_provider_rounds <= fg.max_provider_rounds);
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
    fn config_overlays_only_set_fields() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.autonomous.max_provider_rounds = Some(2);
        cfg.autonomous.max_elapsed_secs = Some(9);
        cfg.autonomous.max_round_renewals = Some(3);
        let budget = TurnBudget::from_config(TurnRole::Autonomous, &cfg);
        assert_eq!(budget.max_provider_rounds, 2);
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
            max_provider_rounds: 2,
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
            max_provider_rounds: 1,
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

    #[test]
    fn wall_clock_still_bounds_a_renewed_turn() {
        let mut meter = TurnBudgetMeter::new(TurnBudget {
            max_provider_rounds: 1,
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
            max_provider_rounds: 1,
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
            max_tool_calls: 3,
            max_accumulated_tool_result_bytes: 10,
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
