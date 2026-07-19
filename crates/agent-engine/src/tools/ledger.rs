//! Task 25 — typed tool-call ledger (spec §8.3).
//!
//! Each tool call advances through a MONOTONIC lifecycle:
//!
//! ```text
//! planned -> authorized -> started -> committed -> result_recorded
//! ```
//!
//! Transitions are single-step-forward only; any skip or backward move is
//! rejected and never applied (the ledger state is therefore monotonic —
//! invalid transitions cannot be represented in a committed ledger). When a
//! turn is interrupted (cancellation or transport failure) with a call that
//! has STARTED but not yet recorded a result — i.e. a side effect is
//! possible but its commit status is unknown — a `NonIdempotent` call must
//! surface [`TurnOutcome::InterruptedAfterSideEffect`] and must NEVER be
//! automatically rerun. Read-only and idempotent calls stay retryable.

use crate::tools::catalog::ToolEffect;
use agent_core::TurnOutcome;

/// One monotonic phase of a tool call's lifecycle (spec §8.3). The derived
/// ordering matches the pipeline order, so `<`/`>` express "earlier/later
/// phase" directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallPhase {
    Planned,
    Authorized,
    Started,
    Committed,
    ResultRecorded,
}

impl CallPhase {
    /// The immediately following phase, or `None` at the terminal phase.
    fn successor(self) -> Option<CallPhase> {
        match self {
            CallPhase::Planned => Some(CallPhase::Authorized),
            CallPhase::Authorized => Some(CallPhase::Started),
            CallPhase::Started => Some(CallPhase::Committed),
            CallPhase::Committed => Some(CallPhase::ResultRecorded),
            CallPhase::ResultRecorded => None,
        }
    }
}

/// A rejected non-monotonic transition. The ledger is left UNCHANGED when
/// this is returned, so no invalid state is ever observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonMonotonic {
    pub from: CallPhase,
    pub to: CallPhase,
}

impl std::fmt::Display for NonMonotonic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "non-monotonic tool-call ledger transition {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for NonMonotonic {}

/// Disposition of a call when its turn is interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptDisposition {
    /// Typed terminal outcome to surface when the interruption straddled a
    /// possible-but-unconfirmed side effect that must not be silently
    /// retried. `None` when the interruption is an ordinary cancellation.
    pub outcome: Option<TurnOutcome>,
    /// Whether the runtime may AUTOMATICALLY rerun this call. Always `false`
    /// for a `NonIdempotent` call with an unknown commit status.
    pub auto_rerun_permitted: bool,
}

/// Per-call ledger. Cloneable/inspectable, but the phase can only ever be
/// advanced one monotonic step at a time through [`CallLedger::advance_to`]
/// (or its named helpers).
#[derive(Debug, Clone)]
pub struct CallLedger {
    call_id: String,
    effect: ToolEffect,
    phase: CallPhase,
}

impl CallLedger {
    /// Open a ledger for a freshly PLANNED call.
    pub fn plan(call_id: impl Into<String>, effect: ToolEffect) -> Self {
        Self {
            call_id: call_id.into(),
            effect,
            phase: CallPhase::Planned,
        }
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn effect(&self) -> ToolEffect {
        self.effect
    }

    pub fn phase(&self) -> CallPhase {
        self.phase
    }

    /// Advance to `to`, which MUST be the immediate successor of the current
    /// phase. Any other target (skip, repeat, or backward) is rejected and
    /// leaves the ledger unchanged.
    pub fn advance_to(&mut self, to: CallPhase) -> Result<(), NonMonotonic> {
        if self.phase.successor() == Some(to) {
            self.phase = to;
            Ok(())
        } else {
            Err(NonMonotonic {
                from: self.phase,
                to,
            })
        }
    }

    pub fn authorize(&mut self) -> Result<(), NonMonotonic> {
        self.advance_to(CallPhase::Authorized)
    }

    pub fn start(&mut self) -> Result<(), NonMonotonic> {
        self.advance_to(CallPhase::Started)
    }

    pub fn commit(&mut self) -> Result<(), NonMonotonic> {
        self.advance_to(CallPhase::Committed)
    }

    pub fn record_result(&mut self) -> Result<(), NonMonotonic> {
        self.advance_to(CallPhase::ResultRecorded)
    }

    /// True once the call may have produced a side effect but has not yet
    /// recorded a result: it has STARTED (commit status unknown) or
    /// COMMITTED (side effect definitely occurred), and is not yet
    /// `ResultRecorded`.
    pub fn side_effect_possible(&self) -> bool {
        matches!(self.phase, CallPhase::Started | CallPhase::Committed)
    }

    /// Disposition if the turn is interrupted with this call in its current
    /// phase. When no side effect is possible (not started, or already
    /// recorded), the interruption is an ordinary cancellation and a rerun
    /// is safe. When a side effect is possible, the effect class decides:
    ///
    /// - `NonIdempotent` => `InterruptedAfterSideEffect`, no auto-rerun;
    /// - `IdempotentWrite` / `ReadOnly` => plain cancellation, retryable.
    pub fn on_interrupt(&self) -> InterruptDisposition {
        if !self.side_effect_possible() {
            return InterruptDisposition {
                outcome: None,
                auto_rerun_permitted: true,
            };
        }
        match self.effect {
            ToolEffect::NonIdempotent => InterruptDisposition {
                outcome: Some(TurnOutcome::InterruptedAfterSideEffect {
                    call_id: self.call_id.clone(),
                }),
                auto_rerun_permitted: false,
            },
            ToolEffect::IdempotentWrite | ToolEffect::ReadOnly => InterruptDisposition {
                outcome: None,
                auto_rerun_permitted: true,
            },
        }
    }

    /// Shared policy for the dispatch paths: the interruption disposition of
    /// a call that has STARTED (possible side effect, result not recorded).
    /// This keeps the single- and multi-tool dispatch branches using ONE
    /// definition of the interrupted-side-effect rule.
    pub fn interrupted_started(call_id: &str, effect: ToolEffect) -> InterruptDisposition {
        let mut ledger = Self::plan(call_id, effect);
        // Planned -> Authorized -> Started; these are always valid here.
        let _ = ledger.authorize();
        let _ = ledger.start();
        ledger.on_interrupt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pipeline_advances_monotonically() {
        let mut l = CallLedger::plan("call-1", ToolEffect::NonIdempotent);
        assert_eq!(l.phase(), CallPhase::Planned);
        l.authorize().unwrap();
        l.start().unwrap();
        l.commit().unwrap();
        l.record_result().unwrap();
        assert_eq!(l.phase(), CallPhase::ResultRecorded);
    }

    #[test]
    fn skipping_a_phase_is_rejected_and_leaves_state_unchanged() {
        let mut l = CallLedger::plan("call-1", ToolEffect::NonIdempotent);
        let err = l.advance_to(CallPhase::Started).unwrap_err();
        assert_eq!(
            err,
            NonMonotonic {
                from: CallPhase::Planned,
                to: CallPhase::Started
            }
        );
        // Unchanged after rejection.
        assert_eq!(l.phase(), CallPhase::Planned);
    }

    #[test]
    fn backward_transition_is_rejected() {
        let mut l = CallLedger::plan("call-1", ToolEffect::ReadOnly);
        l.authorize().unwrap();
        l.start().unwrap();
        assert!(l.advance_to(CallPhase::Authorized).is_err());
        assert!(l.advance_to(CallPhase::Planned).is_err());
        assert_eq!(l.phase(), CallPhase::Started);
    }

    #[test]
    fn repeating_the_current_phase_is_rejected() {
        let mut l = CallLedger::plan("call-1", ToolEffect::ReadOnly);
        l.authorize().unwrap();
        assert!(l.advance_to(CallPhase::Authorized).is_err());
        assert_eq!(l.phase(), CallPhase::Authorized);
    }

    #[test]
    fn advancing_past_terminal_is_rejected() {
        let mut l = CallLedger::plan("call-1", ToolEffect::ReadOnly);
        l.authorize().unwrap();
        l.start().unwrap();
        l.commit().unwrap();
        l.record_result().unwrap();
        assert!(l.record_result().is_err());
        assert_eq!(l.phase(), CallPhase::ResultRecorded);
    }

    #[test]
    fn interrupt_before_start_is_a_plain_cancellation() {
        let mut l = CallLedger::plan("call-1", ToolEffect::NonIdempotent);
        l.authorize().unwrap();
        let d = l.on_interrupt();
        assert_eq!(d.outcome, None);
        assert!(d.auto_rerun_permitted);
    }

    #[test]
    fn interrupt_started_nonidempotent_is_interrupted_no_rerun() {
        let d = CallLedger::interrupted_started("call-9", ToolEffect::NonIdempotent);
        assert_eq!(
            d.outcome,
            Some(TurnOutcome::InterruptedAfterSideEffect {
                call_id: "call-9".to_string()
            })
        );
        assert!(!d.auto_rerun_permitted, "NonIdempotent must not auto-rerun");
    }

    #[test]
    fn interrupt_committed_nonidempotent_is_interrupted_no_rerun() {
        let mut l = CallLedger::plan("call-9", ToolEffect::NonIdempotent);
        l.authorize().unwrap();
        l.start().unwrap();
        l.commit().unwrap();
        let d = l.on_interrupt();
        assert_eq!(
            d.outcome,
            Some(TurnOutcome::InterruptedAfterSideEffect {
                call_id: "call-9".to_string()
            })
        );
        assert!(!d.auto_rerun_permitted);
    }

    #[test]
    fn interrupt_started_idempotent_stays_retryable() {
        let d = CallLedger::interrupted_started("call-2", ToolEffect::IdempotentWrite);
        assert_eq!(d.outcome, None);
        assert!(
            d.auto_rerun_permitted,
            "idempotent writes remain safely retryable"
        );
    }

    #[test]
    fn interrupt_started_readonly_stays_retryable() {
        let d = CallLedger::interrupted_started("call-3", ToolEffect::ReadOnly);
        assert_eq!(d.outcome, None);
        assert!(d.auto_rerun_permitted);
    }

    #[test]
    fn interrupt_after_result_recorded_is_not_a_side_effect() {
        let mut l = CallLedger::plan("call-4", ToolEffect::NonIdempotent);
        l.authorize().unwrap();
        l.start().unwrap();
        l.commit().unwrap();
        l.record_result().unwrap();
        let d = l.on_interrupt();
        assert_eq!(d.outcome, None, "a recorded result is not interrupted");
    }
}
