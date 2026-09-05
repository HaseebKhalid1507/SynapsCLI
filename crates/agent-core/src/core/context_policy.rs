//! Pure, task-aware context pressure policy. No I/O, clocks, prompts, or tools.
//!
//! Pressure is preparation, not a percentage-based stop. Existing execution
//! can continue through the warning band, but a new task or execution after a
//! plan should start in fresh context. At rollover only a bounded Plan/WrapUp
//! extension is allowed; provider capacity always takes precedence.
//!
//! The host owns token accounting and safe session replacement. Assess before
//! each model round, then persist `assessment.next_state` at that admission
//! checkpoint (not for UI-only polling). It charges one round for each bounded
//! extension. Keep that state across phase reports and mode changes; reset it
//! only after successful context replacement. Neither phase reports nor these
//! decisions authorize tools, disclosures, or mutations of session history.

use crate::core::config::{ContextManagementConfig, ContextManagementMode};

/// Advisory task phase. A report is not evidence of authorization or capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkPhase {
    #[default]
    Unknown,
    /// Execution already in progress may continue through the pressure band.
    Execute,
    /// Finish a bounded piece of existing work; do not begin another task.
    WrapUp,
    /// Produce a plan/specification, not execute the resulting large plan.
    Plan,
    NewTask,
}

impl WorkPhase {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "execute" => Some(Self::Execute),
            "wrap_up" | "wrapup" | "wrap-up" => Some(Self::WrapUp),
            "plan" => Some(Self::Plan),
            "new_task" | "newtask" | "new-task" => Some(Self::NewTask),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Execute => "execute",
            Self::WrapUp => "wrap_up",
            Self::Plan => "plan",
            Self::NewTask => "new_task",
        }
    }
}

/// Per-context state. Private counters prevent phase reports from replenishing
/// the allowance. The host must preserve this state across model rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextState {
    phase: WorkPhase,
    plan_pending_execution: bool,
    boundary_pending: bool,
    rollover_due: bool,
    rollover_required: bool,
    finish_rounds_used: u32,
}

impl ContextState {
    pub fn phase(&self) -> WorkPhase {
        self.phase
    }

    pub fn finish_rounds_used(&self) -> u32 {
        self.finish_rounds_used
    }

    /// Record advisory progress. Intermediate Unknown/WrapUp reports cannot
    /// hide a Plan -> Execute transition, nor can reports reset an extension.
    pub fn report_phase(&mut self, phase: WorkPhase) {
        match phase {
            WorkPhase::Plan => self.plan_pending_execution = true,
            WorkPhase::Execute if self.plan_pending_execution => {
                self.boundary_pending = true;
                self.plan_pending_execution = false;
            }
            WorkPhase::NewTask => self.boundary_pending = true,
            _ => {}
        }
        self.phase = phase;
    }

    /// Host-only lifecycle operation after successful context replacement.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Host-supplied, conservative accounting for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub context_window_tokens: u64,
    /// Current full input footprint, including system/tool/summary overhead.
    pub used_tokens: u64,
    /// Remaining usable capacity according to the host/provider. May be less
    /// than window - used; a larger value never enlarges the physical window.
    pub hard_remaining_tokens: u64,
    /// Required headroom for the next round, including output/thinking/tool
    /// growth. Compared with (not added to) the configured minimum reserve.
    pub required_next_round_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextThresholds {
    pub pressure_tokens: u64,
    pub rollover_tokens: u64,
}

impl ContextThresholds {
    /// Anchored absolute soft bands: 140k/180k at 200k, 250k/400k at 1m.
    /// Interpolate between anchors and cap above 1m. Smaller models scale
    /// down to fit their window. A lone override preserves that explicit
    /// value and moves the other default only if ordering requires it.
    pub fn resolve(
        config: &ContextManagementConfig,
        context_window_tokens: u64,
    ) -> Result<Self, &'static str> {
        config.validate_for_window(context_window_tokens)?;
        let window = context_window_tokens;
        let (pressure, rollover) = if window <= 200_000 {
            ((window * 7 / 10).max(1), (window * 9 / 10).max(2))
        } else if window < 1_000_000 {
            let extra = window - 200_000;
            (
                140_000 + extra * 110_000 / 800_000,
                180_000 + extra * 220_000 / 800_000,
            )
        } else {
            (250_000, 400_000)
        };
        let (pressure_tokens, rollover_tokens) =
            match (config.pressure_tokens, config.rollover_tokens) {
                (Some(p), Some(r)) => (p, r),
                (Some(p), None) => (p, rollover.max(p + 1)),
                (None, Some(r)) => (pressure.min(r - 1), r),
                (None, None) => (pressure, rollover),
            };
        Ok(Self {
            pressure_tokens,
            rollover_tokens,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextBand {
    Normal,
    Pressure,
    Rollover,
    HardLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    Disabled,
    Continue,
    /// Preparation only; the current task is not forcibly interrupted.
    Advisory,
    /// Admit one Plan/WrapUp round. Count is the allowance AFTER this round.
    FinishBounded {
        rounds_remaining: u32,
    },
    /// Request host-controlled rollover at a safe boundary, before more work.
    Rollover,
    /// No further model round, even to finish a plan or generate a summary.
    /// The host must recover without exceeding the supplied hard budget.
    HardStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextReason {
    ModeOff,
    BelowPressure,
    PrepareForRollover,
    TaskBoundary,
    RolloverThreshold,
    BoundedFinish,
    FinishAllowanceExhausted,
    RolloverPending,
    HardCapacity,
    InvalidConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextAssessment {
    pub action: ContextAction,
    pub band: ContextBand,
    pub reason: ContextReason,
    /// None when disabled, invalid, or already stopped by the hard budget.
    pub thresholds: Option<ContextThresholds>,
    /// Persist at the host's admission checkpoint, even if the action stops
    /// work. Repeated assessments alone have no side effects.
    pub next_state: ContextState,
}

/// Pure assessment, independent of UI, model prose, and tool authority.
/// Hard capacity wins even in Off mode. Invalid programmatic configuration
/// fails closed; config-file validation normally catches it before this call.
pub fn assess_context(
    config: &ContextManagementConfig,
    state: &ContextState,
    budget: ContextBudget,
) -> ContextAssessment {
    let mut assessment = ContextAssessment {
        action: ContextAction::HardStop,
        band: ContextBand::HardLimit,
        reason: ContextReason::HardCapacity,
        thresholds: None,
        next_state: *state,
    };
    let remaining = budget.hard_remaining_tokens.min(
        budget
            .context_window_tokens
            .saturating_sub(budget.used_tokens),
    );
    let required = budget.required_next_round_tokens.max(config.reserve_tokens);
    if remaining == 0 || remaining < required {
        return assessment;
    }

    // Consume boundary reports at the admission checkpoint, including Off
    // mode, so an old low-pressure transition does not interrupt later work.
    assessment.next_state.boundary_pending = false;
    if config.mode == ContextManagementMode::Off {
        assessment.action = ContextAction::Disabled;
        assessment.band = ContextBand::Normal;
        assessment.reason = ContextReason::ModeOff;
        return assessment;
    }
    let Ok(thresholds) = ContextThresholds::resolve(config, budget.context_window_tokens) else {
        assessment.reason = ContextReason::InvalidConfig;
        return assessment;
    };
    assessment.thresholds = Some(thresholds);
    assessment.band = if budget.used_tokens >= thresholds.rollover_tokens {
        ContextBand::Rollover
    } else if budget.used_tokens >= thresholds.pressure_tokens {
        ContextBand::Pressure
    } else {
        ContextBand::Normal
    };

    if state.rollover_required {
        assessment.action = ContextAction::Rollover;
        assessment.reason = ContextReason::RolloverPending;
        return assessment;
    }
    if budget.used_tokens >= thresholds.pressure_tokens
        && (state.boundary_pending || state.phase == WorkPhase::NewTask)
    {
        assessment.next_state.rollover_required = true;
        assessment.action = ContextAction::Rollover;
        assessment.reason = ContextReason::TaskBoundary;
        return assessment;
    }
    if assessment.band == ContextBand::Rollover || state.rollover_due {
        // Token-estimate decreases, repeated reports, or mode toggles cannot
        // renew an allowance once this context has entered rollover.
        assessment.next_state.rollover_due = true;
        if matches!(state.phase, WorkPhase::Plan | WorkPhase::WrapUp)
            && state.finish_rounds_used < config.finish_rounds
        {
            assessment.next_state.finish_rounds_used += 1;
            assessment.action = ContextAction::FinishBounded {
                rounds_remaining: config.finish_rounds - assessment.next_state.finish_rounds_used,
            };
            assessment.reason = ContextReason::BoundedFinish;
        } else {
            assessment.next_state.rollover_required = true;
            assessment.action = ContextAction::Rollover;
            assessment.reason = if state.finish_rounds_used >= config.finish_rounds {
                ContextReason::FinishAllowanceExhausted
            } else {
                ContextReason::RolloverThreshold
            };
        }
    } else if assessment.band == ContextBand::Pressure {
        assessment.action = ContextAction::Advisory;
        assessment.reason = ContextReason::PrepareForRollover;
    } else {
        assessment.action = ContextAction::Continue;
        assessment.reason = ContextReason::BelowPressure;
    }
    assessment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_config() -> ContextManagementConfig {
        ContextManagementConfig {
            mode: ContextManagementMode::Auto,
            ..Default::default()
        }
    }

    fn budget(window: u64, used: u64) -> ContextBudget {
        ContextBudget {
            context_window_tokens: window,
            used_tokens: used,
            hard_remaining_tokens: window.saturating_sub(used),
            required_next_round_tokens: 8_000,
        }
    }

    #[test]
    fn context_threshold_anchors_and_pressure_are_advisory() {
        let config = auto_config();
        for (window, pressure, rollover) in [
            (200_000, 140_000, 180_000),
            (1_000_000, 250_000, 400_000),
            (2_000_000, 250_000, 400_000),
        ] {
            assert_eq!(
                ContextThresholds::resolve(&config, window).unwrap(),
                ContextThresholds {
                    pressure_tokens: pressure,
                    rollover_tokens: rollover
                }
            );
            let mut state = ContextState::default();
            state.report_phase(WorkPhase::Execute);
            assert_eq!(
                assess_context(&config, &state, budget(window, pressure - 1)).action,
                ContextAction::Continue
            );
            let at_pressure = assess_context(&config, &state, budget(window, pressure));
            assert_eq!(at_pressure.action, ContextAction::Advisory);
            assert_eq!(at_pressure.band, ContextBand::Pressure);
            assert_eq!(
                assess_context(&config, &state, budget(window, rollover - 1)).action,
                ContextAction::Advisory
            );
            assert_eq!(
                assess_context(&config, &state, budget(window, rollover)).action,
                ContextAction::Rollover
            );
        }
    }

    #[test]
    fn context_350k_plan_finishes_before_execute_requests_rollover() {
        let config = auto_config();
        let current_budget = budget(1_000_000, 350_000);
        for phase in [WorkPhase::Execute, WorkPhase::WrapUp, WorkPhase::Plan] {
            let mut state = ContextState::default();
            state.report_phase(phase);
            assert_eq!(
                assess_context(&config, &state, current_budget).action,
                ContextAction::Advisory
            );
        }
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::Plan);
        state = assess_context(&config, &state, current_budget).next_state;
        state.report_phase(WorkPhase::Unknown);
        state.report_phase(WorkPhase::WrapUp);
        state.report_phase(WorkPhase::Execute);
        state.report_phase(WorkPhase::Plan); // cannot hide the boundary
        let assessment = assess_context(&config, &state, current_budget);
        assert_eq!(assessment.action, ContextAction::Rollover);
        assert_eq!(assessment.reason, ContextReason::TaskBoundary);
        state = assessment.next_state;
        state.report_phase(WorkPhase::WrapUp);
        assert_eq!(
            assess_context(&config, &state, current_budget).action,
            ContextAction::Rollover
        );
        state.reset();
        assert_eq!(
            assess_context(&config, &state, budget(1_000_000, 0)).action,
            ContextAction::Continue
        );
    }

    #[test]
    fn context_new_task_rolls_over_at_pressure_not_before() {
        let config = auto_config();
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::NewTask);
        assert_eq!(
            assess_context(&config, &state, budget(1_000_000, 249_999)).action,
            ContextAction::Continue
        );
        assert_eq!(
            assess_context(&config, &state, budget(1_000_000, 250_000)).action,
            ContextAction::Rollover
        );
    }

    #[test]
    fn context_low_pressure_boundary_is_consumed_at_admission() {
        let config = auto_config();
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::Plan);
        state.report_phase(WorkPhase::Execute);
        state = assess_context(&config, &state, budget(1_000_000, 100_000)).next_state;
        assert_eq!(
            assess_context(&config, &state, budget(1_000_000, 350_000)).action,
            ContextAction::Advisory
        );
    }

    #[test]
    fn context_400k_finish_is_bounded_across_reports_and_estimate_decreases() {
        let config = auto_config();
        for phase in [WorkPhase::Plan, WorkPhase::WrapUp] {
            let mut state = ContextState::default();
            state.report_phase(phase);
            let first = assess_context(&config, &state, budget(1_000_000, 400_000));
            assert_eq!(
                first.action,
                ContextAction::FinishBounded {
                    rounds_remaining: 1
                }
            );
            assert_eq!(state.finish_rounds_used(), 0); // assessment itself is pure
            assert_eq!(
                first,
                assess_context(&config, &state, budget(1_000_000, 400_000))
            );
            state = first.next_state;
            state.report_phase(WorkPhase::Unknown);
            state.report_phase(phase);
            let second = assess_context(&config, &state, budget(1_000_000, 350_000));
            assert_eq!(
                second.action,
                ContextAction::FinishBounded {
                    rounds_remaining: 0
                }
            );
            state = second.next_state;
            assert_eq!(state.finish_rounds_used(), 2);
            state.report_phase(phase);
            let exhausted = assess_context(&config, &state, budget(1_000_000, 100_000));
            assert_eq!(exhausted.action, ContextAction::Rollover);
            assert_eq!(exhausted.reason, ContextReason::FinishAllowanceExhausted);
            state = exhausted.next_state;
            state.report_phase(WorkPhase::Plan);
            assert_eq!(
                assess_context(&config, &state, budget(1_000_000, 100_000)).action,
                ContextAction::Rollover
            );
            state.reset();
            assert_eq!(state.finish_rounds_used(), 0);
            assert_eq!(state.phase(), WorkPhase::Unknown);
        }
    }

    #[test]
    fn context_finish_override_is_bounded_including_zero() {
        for limit in [0, 1, ContextManagementConfig::MAX_FINISH_ROUNDS] {
            let config = ContextManagementConfig {
                finish_rounds: limit,
                ..auto_config()
            };
            let mut state = ContextState::default();
            state.report_phase(WorkPhase::WrapUp);
            for used in 0..limit {
                let assessment = assess_context(&config, &state, budget(1_000_000, 400_000));
                assert_eq!(
                    assessment.action,
                    ContextAction::FinishBounded {
                        rounds_remaining: limit - used - 1
                    }
                );
                state = assessment.next_state;
            }
            assert_eq!(
                assess_context(&config, &state, budget(1_000_000, 400_000)).action,
                ContextAction::Rollover
            );
        }
    }

    #[test]
    fn context_off_is_default_and_mode_toggles_do_not_replenish_finish() {
        let off = ContextManagementConfig::default();
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::WrapUp);
        assert_eq!(
            assess_context(&off, &state, budget(1_000_000, 400_000)).action,
            ContextAction::Disabled
        );
        state = assess_context(&auto_config(), &state, budget(1_000_000, 400_000)).next_state;
        state = assess_context(&off, &state, budget(1_000_000, 410_000)).next_state;
        assert_eq!(state.finish_rounds_used(), 1);
        let second = assess_context(&auto_config(), &state, budget(1_000_000, 420_000));
        assert_eq!(
            second.action,
            ContextAction::FinishBounded {
                rounds_remaining: 0
            }
        );
        assert_eq!(
            assess_context(
                &auto_config(),
                &second.next_state,
                budget(1_000_000, 420_000)
            )
            .action,
            ContextAction::Rollover
        );
    }

    #[test]
    fn context_hard_capacity_wins_for_every_phase_and_mode() {
        for mode in [ContextManagementMode::Off, ContextManagementMode::Auto] {
            let config = ContextManagementConfig {
                mode,
                ..Default::default()
            };
            for phase in [
                WorkPhase::Unknown,
                WorkPhase::Execute,
                WorkPhase::WrapUp,
                WorkPhase::Plan,
                WorkPhase::NewTask,
            ] {
                let mut state = ContextState::default();
                state.report_phase(phase);
                for hard_budget in [
                    ContextBudget {
                        hard_remaining_tokens: 15_999,
                        ..budget(1_000_000, 350_000)
                    },
                    ContextBudget {
                        required_next_round_tokens: 650_001,
                        ..budget(1_000_000, 350_000)
                    },
                    ContextBudget {
                        hard_remaining_tokens: u64::MAX,
                        ..budget(200_000, 190_000)
                    },
                    budget(200_000, 200_001),
                    budget(0, 0),
                ] {
                    let assessment = assess_context(&config, &state, hard_budget);
                    assert_eq!(
                        assessment.action,
                        ContextAction::HardStop,
                        "{mode:?} {phase:?} {hard_budget:?}"
                    );
                    assert_eq!(assessment.band, ContextBand::HardLimit);
                    assert_eq!(assessment.reason, ContextReason::HardCapacity);
                    assert_eq!(assessment.next_state, state);
                }
            }
        }
    }

    #[test]
    fn context_exact_reserve_is_allowed_and_next_round_can_require_more() {
        let config = auto_config();
        let state = ContextState::default();
        let exact = ContextBudget {
            hard_remaining_tokens: 16_000,
            ..budget(200_000, 140_000)
        };
        assert_eq!(
            assess_context(&config, &state, exact).action,
            ContextAction::Advisory
        );
        let larger = ContextBudget {
            required_next_round_tokens: 16_001,
            ..exact
        };
        assert_eq!(
            assess_context(&config, &state, larger).action,
            ContextAction::HardStop
        );
    }

    #[test]
    fn context_threshold_overrides_preserve_explicit_values() {
        for (pressure, rollover, expected) in [
            (Some(350_000), Some(500_000), (350_000, 500_000)),
            (Some(450_000), None, (450_000, 450_001)),
            (None, Some(200_000), (199_999, 200_000)),
        ] {
            let config = ContextManagementConfig {
                pressure_tokens: pressure,
                rollover_tokens: rollover,
                ..auto_config()
            };
            let thresholds = ContextThresholds::resolve(&config, 1_000_000).unwrap();
            assert_eq!(
                (thresholds.pressure_tokens, thresholds.rollover_tokens),
                expected
            );
        }
        assert!(ContextThresholds::resolve(&auto_config(), u64::MAX).is_ok());
        assert_eq!(
            ContextThresholds::resolve(&auto_config(), 100_000)
                .unwrap()
                .pressure_tokens,
            70_000
        );
        assert_eq!(
            ContextThresholds::resolve(&auto_config(), 600_000)
                .unwrap()
                .rollover_tokens,
            290_000
        );
    }

    #[test]
    fn context_invalid_programmatic_config_fails_closed() {
        for config in [
            ContextManagementConfig {
                pressure_tokens: Some(0),
                ..auto_config()
            },
            ContextManagementConfig {
                pressure_tokens: Some(u64::MAX),
                ..auto_config()
            },
            ContextManagementConfig {
                rollover_tokens: Some(1_000_001),
                ..auto_config()
            },
            ContextManagementConfig {
                finish_rounds: 9,
                ..auto_config()
            },
            ContextManagementConfig {
                reserve_tokens: 0,
                ..auto_config()
            },
        ] {
            let assessment = assess_context(
                &config,
                &ContextState::default(),
                budget(1_000_000, 350_000),
            );
            assert_eq!(assessment.action, ContextAction::HardStop);
            assert_eq!(assessment.reason, ContextReason::InvalidConfig);
        }
    }

    #[test]
    fn context_work_phase_roundtrips_and_rejects_unknown_reports() {
        for phase in [
            WorkPhase::Unknown,
            WorkPhase::Execute,
            WorkPhase::WrapUp,
            WorkPhase::Plan,
            WorkPhase::NewTask,
        ] {
            assert_eq!(WorkPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(WorkPhase::parse(" PLAN "), Some(WorkPhase::Plan));
        assert_eq!(WorkPhase::parse("wrap-up"), Some(WorkPhase::WrapUp));
        assert_eq!(WorkPhase::parse("new-task"), Some(WorkPhase::NewTask));
        assert_eq!(WorkPhase::parse("finish_forever"), None);
    }
}
