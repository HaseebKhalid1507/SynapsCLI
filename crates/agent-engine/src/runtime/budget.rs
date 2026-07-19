//! Task 23 — per-turn budgets (spec §8.1).
//!
//! Every stream session runs under one [`TurnBudget`]: provider rounds,
//! tool calls, elapsed wall-clock, accumulated tool-result bytes, and the
//! optional context-token / USD-cost dimensions. Budgets differ by role
//! (foreground, autonomous/watcher, worker) but share ONE enforcement
//! mechanism — the [`TurnBudgetMeter`] consulted by the stream loop.
//!
//! Composition note: the chat frontend's reactor auto-turn cap
//! (`engine::reactor::AUTO_TURN_CAP`) limits consecutive auto-triggered
//! turns ACROSS turns; this budget bounds work WITHIN one turn. They
//! compose — neither replaces nor duplicates the other.

use std::time::{Duration, Instant};

use agent_core::BudgetDimension;

/// Per-turn limits (spec §8.1).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnBudget {
    pub max_provider_rounds: u32,
    pub max_tool_calls: u32,
    pub max_elapsed: Duration,
    pub max_accumulated_tool_result_bytes: usize,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
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
                max_elapsed: Duration::from_secs(2 * 60 * 60),
                max_accumulated_tool_result_bytes: 32 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
            },
            TurnRole::Autonomous => Self {
                max_provider_rounds: 24,
                max_tool_calls: 96,
                max_elapsed: Duration::from_secs(15 * 60),
                max_accumulated_tool_result_bytes: 8 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
            },
            TurnRole::Worker => Self {
                max_provider_rounds: 64,
                max_tool_calls: 256,
                max_elapsed: Duration::from_secs(60 * 60),
                max_accumulated_tool_result_bytes: 16 * 1024 * 1024,
                max_context_tokens: None,
                max_cost_usd: None,
            },
        }
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
            budget.max_elapsed = Duration::from_secs(v);
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
        budget
    }
}

/// Shared provider-usage counters, updated at the single authoritative
/// Usage emission sites in the transport and read by the stream loop for
/// the optional token/cost dimensions. Metadata only.
#[derive(Debug, Default)]
pub struct UsageCounters {
    input_tokens: std::sync::atomic::AtomicU64,
    output_tokens: std::sync::atomic::AtomicU64,
    cache_read_tokens: std::sync::atomic::AtomicU64,
    cache_write_tokens: std::sync::atomic::AtomicU64,
    /// Latest request's context footprint (input + cache read + cache
    /// write) — the context-token dimension compares against this.
    latest_context_tokens: std::sync::atomic::AtomicU64,
}

impl UsageCounters {
    pub fn record(&self, input: u64, output: u64, cache_read: u64, cache_write: u64) {
        use std::sync::atomic::Ordering;
        self.input_tokens.fetch_add(input, Ordering::Relaxed);
        self.output_tokens.fetch_add(output, Ordering::Relaxed);
        self.cache_read_tokens
            .fetch_add(cache_read, Ordering::Relaxed);
        self.cache_write_tokens
            .fetch_add(cache_write, Ordering::Relaxed);
        self.latest_context_tokens.store(
            input.saturating_add(cache_read).saturating_add(cache_write),
            Ordering::Relaxed,
        );
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
}

impl TurnBudgetMeter {
    pub fn new(budget: TurnBudget) -> Self {
        Self {
            budget,
            started: Instant::now(),
            rounds: 0,
            tool_calls: 0,
            tool_result_bytes: 0,
        }
    }

    pub fn budget(&self) -> &TurnBudget {
        &self.budget
    }

    fn wall_clock_exceeded(&self) -> bool {
        self.started.elapsed() >= self.budget.max_elapsed
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
    /// shared usage counters. `model` feeds the pricing table for the
    /// cost estimate; both checks are no-ops when unconfigured.
    pub fn check_usage(&self, usage: &UsageCounters, model: &str) -> Result<(), BudgetDimension> {
        if let Some(max_context) = self.budget.max_context_tokens {
            if usage.latest_context_tokens() > max_context {
                return Err(BudgetDimension::InputTokens);
            }
        }
        if let Some(max_cost) = self.budget.max_cost_usd {
            let (input, output, cache_read, cache_write) = usage.totals();
            let cost =
                agent_core::pricing::calculate_cost(model, input, output, cache_read, cache_write);
            if cost > max_cost {
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
    fn role_defaults_are_ordered_and_typed() {
        let fg = TurnBudget::for_role(TurnRole::Foreground);
        let auto = TurnBudget::for_role(TurnRole::Autonomous);
        let worker = TurnBudget::for_role(TurnRole::Worker);
        assert!(auto.max_provider_rounds < worker.max_provider_rounds);
        assert!(worker.max_provider_rounds <= fg.max_provider_rounds);
        assert!(auto.max_elapsed < worker.max_elapsed);
        assert!(fg.max_context_tokens.is_none());
        assert!(fg.max_cost_usd.is_none());
    }

    #[test]
    fn config_overlays_only_set_fields() {
        let mut cfg = agent_core::config::TurnBudgetsConfig::default();
        cfg.autonomous.max_provider_rounds = Some(2);
        cfg.autonomous.max_elapsed_secs = Some(9);
        let budget = TurnBudget::from_config(TurnRole::Autonomous, &cfg);
        assert_eq!(budget.max_provider_rounds, 2);
        assert_eq!(budget.max_elapsed, Duration::from_secs(9));
        assert_eq!(
            budget.max_tool_calls,
            TurnBudget::for_role(TurnRole::Autonomous).max_tool_calls
        );
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
            max_elapsed: Duration::ZERO,
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        // Wall clock outranks the round check.
        assert_eq!(stale.begin_round(), Err(BudgetDimension::WallClock));
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
        usage.record(100, 50, 10, 5);
        assert_eq!(usage.latest_context_tokens(), 115);

        let meter = TurnBudgetMeter::new(TurnBudget {
            max_context_tokens: Some(100),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert_eq!(
            meter.check_usage(&usage, "claude-sonnet-4-5"),
            Err(BudgetDimension::InputTokens)
        );

        let cost_meter = TurnBudgetMeter::new(TurnBudget {
            max_cost_usd: Some(0.0),
            ..TurnBudget::for_role(TurnRole::Foreground)
        });
        assert_eq!(
            cost_meter.check_usage(&usage, "claude-sonnet-4-5"),
            Err(BudgetDimension::CostUsd)
        );
        let unlimited = TurnBudgetMeter::new(TurnBudget::for_role(TurnRole::Foreground));
        assert!(unlimited.check_usage(&usage, "claude-sonnet-4-5").is_ok());
    }
}
