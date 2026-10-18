//! Watcher agent (`synaps agent`) session accounting and turn driving.
//!
//! The agent's `[limits]` (session tokens, cost, time, tool calls, and the
//! daily cost cap) are enforced by the ENGINE inside each turn: before every
//! turn the role budget is narrowed to what the session has left
//! ([`SessionAllowance::turn_budget`]), so the engine stops the turn at a
//! round boundary with a valid history and a typed `BudgetExceeded`. Calls
//! past the tool-call allowance get the engine's synthetic refusal result,
//! not a cancel; in-budget calls of the same parallel batch still run.
//!
//! [`drive_turn`] meters one turn's stream into [`SessionTotals`]: usage is
//! priced with the runtime's pricing table (cache reads and writes
//! included), tool calls are counted when they produce a result (a refused
//! call is not counted), and a clean exit is recorded only when the
//! `watcher_exit` tool reports that the handoff was saved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::runtime::budget::{TurnBudget, TOOL_CALL_NOT_EXECUTED};
use crate::runtime::{CancelCause, TurnCompletion};
use crate::tools::WATCHER_EXIT_ACK;
use crate::{
    LlmEvent, Runtime, SessionEvent, SessionLimits, SharedMessage, StreamEvent, TurnError,
};

/// What one watcher session has used so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionTotals {
    /// Uncached input + output tokens (what `max_session_tokens` limits).
    pub tokens: u64,
    /// Priced spend, cache reads and writes included.
    pub cost_usd: f64,
    /// Tool calls that ran (refused calls are not counted).
    pub tool_calls: u64,
}

/// A watcher session limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLimit {
    Tokens,
    Cost,
    DailyCost,
    Time,
    ToolCalls,
}

impl SessionLimit {
    /// The exit reason recorded in the session log and stats.
    pub fn exit_reason(self) -> &'static str {
        match self {
            SessionLimit::Tokens => "token_limit",
            SessionLimit::Cost => "cost_limit",
            SessionLimit::DailyCost => "daily_cost_limit",
            SessionLimit::Time => "time_limit",
            SessionLimit::ToolCalls => "tool_limit",
        }
    }
}

/// A session's limits plus what was already spent today before it started.
#[derive(Debug, Clone, Copy)]
pub struct SessionAllowance<'a> {
    pub limits: &'a SessionLimits,
    /// Cost booked to today's stats by earlier sessions.
    pub daily_spent_before: f64,
}

impl SessionAllowance<'_> {
    fn max_duration(&self) -> Duration {
        Duration::from_secs(self.limits.max_session_duration_mins.saturating_mul(60))
    }

    /// When a turn still running is cancelled: the session time limit plus
    /// [`TURN_DEADLINE_GRACE`].
    pub fn turn_deadline(&self, session_start: Instant) -> Instant {
        session_start + self.max_duration() + TURN_DEADLINE_GRACE
    }

    /// The first limit `totals` has reached after `elapsed`, if any.
    pub fn limit_reached(&self, totals: &SessionTotals, elapsed: Duration) -> Option<SessionLimit> {
        if totals.tokens >= self.limits.max_session_tokens {
            Some(SessionLimit::Tokens)
        } else if totals.cost_usd >= self.limits.max_session_cost_usd {
            Some(SessionLimit::Cost)
        } else if self.daily_spent_before + totals.cost_usd >= self.limits.max_daily_cost_usd {
            Some(SessionLimit::DailyCost)
        } else if elapsed >= self.max_duration() {
            Some(SessionLimit::Time)
        } else if totals.tool_calls >= self.limits.max_tool_calls {
            Some(SessionLimit::ToolCalls)
        } else {
            None
        }
    }

    /// The engine budget for the next turn: `role` narrowed to what the
    /// session has left (tool calls, cost against both the session and the
    /// daily cap, time, tokens), the stricter limit winning per dimension.
    pub fn turn_budget(
        &self,
        role: &TurnBudget,
        totals: &SessionTotals,
        elapsed: Duration,
    ) -> TurnBudget {
        let limits = self.limits;
        let session_cost_left = limits.max_session_cost_usd - totals.cost_usd;
        let daily_cost_left =
            limits.max_daily_cost_usd - (self.daily_spent_before + totals.cost_usd);
        let remaining = TurnBudget {
            max_provider_rounds: None,
            max_tool_calls: Some(
                u32::try_from(limits.max_tool_calls.saturating_sub(totals.tool_calls))
                    .unwrap_or(u32::MAX),
            ),
            max_elapsed: Some(self.max_duration().saturating_sub(elapsed)),
            max_accumulated_tool_result_bytes: None,
            max_context_tokens: None,
            max_cost_usd: Some(session_cost_left.min(daily_cost_left).max(0.0)),
            max_total_tokens: Some(limits.max_session_tokens.saturating_sub(totals.tokens)),
            max_round_renewals: u32::MAX,
        };
        role.tighter(&remaining)
    }
}

/// Price one request's usage with the runtime's pricing table (the same one
/// the engine's turn cost budget uses), cache reads and writes included.
pub fn usage_cost(
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_5m: Option<u64>,
    cache_write_1h: Option<u64>,
) -> f64 {
    crate::pricing::calculate_cost_optional_split(
        model,
        input,
        output,
        cache_read,
        cache_write,
        cache_write_5m,
        cache_write_1h,
    )
}

/// How one watcher turn ended.
#[derive(Debug, Default)]
pub struct TurnReport {
    /// The turn's final history (valid: every tool_use has a result).
    pub history: Option<Vec<SharedMessage>>,
    /// `watcher_exit` ran and reported the handoff saved.
    pub handoff_saved: bool,
    /// The turn's terminal error (budget stop, provider failure, ...).
    pub error: Option<TurnError>,
}

/// Drain one turn's stream, metering it into `totals`. `model` prices usage
/// events that do not name their model. `on_event` sees every event after
/// it was metered (for logging).
pub async fn drive_turn<S>(
    mut stream: S,
    model: &str,
    totals: &mut SessionTotals,
    mut on_event: impl FnMut(&StreamEvent, &SessionTotals),
) -> TurnReport
where
    S: Stream<Item = StreamEvent> + Unpin,
{
    let mut report = TurnReport::default();
    let mut exit_call: Option<String> = None;
    while let Some(event) = stream.next().await {
        match &event {
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id })
                if tool_name == "watcher_exit" =>
            {
                exit_call = Some(tool_id.clone());
            }
            StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => {
                // Calls a budget refused, and calls a cancel cut short, did
                // not run to completion and are not counted.
                if !result.starts_with(TOOL_CALL_NOT_EXECUTED) && !result.starts_with("Canceled") {
                    totals.tool_calls = totals.tool_calls.saturating_add(1);
                }
                if exit_call.as_deref() == Some(tool_id.as_str())
                    && result.starts_with(WATCHER_EXIT_ACK)
                {
                    report.handoff_saved = true;
                }
            }
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model: event_model,
            }) => {
                totals.tokens = totals
                    .tokens
                    .saturating_add(input_tokens.saturating_add(*output_tokens));
                totals.cost_usd += usage_cost(
                    event_model.as_deref().unwrap_or(model),
                    *input_tokens,
                    *output_tokens,
                    *cache_read_input_tokens,
                    *cache_creation_input_tokens,
                    *cache_creation_5m,
                    *cache_creation_1h,
                );
            }
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                report.history = Some(history.clone());
            }
            StreamEvent::Session(SessionEvent::Error(error)) => {
                report.error = Some(error.clone());
            }
            _ => {}
        }
        on_event(&event, totals);
    }
    report
}

/// Append a user prompt to `messages` without creating two consecutive user
/// messages: a turn that ended on its tool results (a budget stop) already
/// ends with a user message, which then gets the prompt as a text block.
pub fn push_user_prompt(messages: &mut Vec<SharedMessage>, prompt: &str) {
    if let Some(last) = messages.last() {
        if last["role"] == "user" {
            let mut merged = (**last).clone();
            let text = serde_json::json!({"type": "text", "text": prompt});
            match merged["content"].take() {
                serde_json::Value::Array(mut blocks) => {
                    blocks.push(text);
                    merged["content"] = serde_json::Value::Array(blocks);
                }
                serde_json::Value::String(s) => {
                    merged["content"] = serde_json::json!([{"type": "text", "text": s}, text]);
                }
                _ => merged["content"] = serde_json::json!([text]),
            }
            *messages.last_mut().expect("non-empty") = std::sync::Arc::new(merged);
            return;
        }
    }
    messages.push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": prompt}),
    ));
}

/// The prompt for the turn that asks for a handoff after a session limit.
pub const HANDOFF_PROMPT: &str = "You're being shut down (resource limit reached). Call watcher_exit NOW with your handoff state — summarize what you did, what's pending, and any context your next session needs.";

/// How long the post-limit handoff turn may run.
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);

/// Watches one streaming turn: cancels it on SIGINT/SIGTERM, or once
/// `deadline` passes, as a host stop (canceled tool results say so, not
/// "by user"). Exits when the turn ends. Session limits are otherwise
/// enforced by the engine inside the turn.
pub async fn watch_turn(
    cancel: CancellationToken,
    completion: TurnCompletion,
    interrupted: Arc<AtomicBool>,
    turn_over: Arc<AtomicBool>,
    deadline: Option<Instant>,
    poll: Duration,
) {
    while !turn_over.load(Ordering::Acquire) && !cancel.is_cancelled() {
        if interrupted.load(Ordering::Acquire) || deadline.is_some_and(|d| Instant::now() >= d) {
            completion.note_cancel_cause(CancelCause::Host);
            cancel.cancel();
            break;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Run one turn of `runtime` over `messages` under `budget`, metered into
/// `totals`; `messages` becomes the turn's final history. A signal cancels
/// the turn; so does `deadline` (the handoff turn's backstop).
pub async fn run_turn(
    runtime: &mut Runtime,
    messages: &mut Vec<SharedMessage>,
    budget: TurnBudget,
    totals: &mut SessionTotals,
    interrupted: &Arc<AtomicBool>,
    deadline: Option<Instant>,
    on_event: impl FnMut(&StreamEvent, &SessionTotals),
) -> TurnReport {
    runtime.set_turn_budget(budget);
    let cancel = CancellationToken::new();
    let completion = TurnCompletion::new();
    let turn_over = Arc::new(AtomicBool::new(false));
    tokio::spawn(watch_turn(
        cancel.clone(),
        completion.clone(),
        Arc::clone(interrupted),
        Arc::clone(&turn_over),
        deadline,
        Duration::from_millis(250),
    ));
    let model = runtime.model().to_string();
    let stream = runtime
        .run_stream_tracked(messages.clone(), cancel, None, None, false, completion)
        .await;
    let report = drive_turn(stream, &model, totals, on_event).await;
    turn_over.store(true, Ordering::Release);
    if let Some(history) = &report.history {
        *messages = history.clone();
    }
    report
}

/// Extra time past the session time limit before a turn still running is
/// cancelled. The engine stops a turn at the next provider call once the
/// session time is used up; this deadline only catches a turn stuck in a
/// long tool round (a slow command, a `timeout: 0` subagent).
pub const TURN_DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// One turn of a watcher session, as `synaps agent` runs it: the role
/// budget narrowed to what the session has left.
#[allow(clippy::too_many_arguments)]
pub async fn run_session_turn(
    runtime: &mut Runtime,
    messages: &mut Vec<SharedMessage>,
    allowance: &SessionAllowance<'_>,
    role: &TurnBudget,
    totals: &mut SessionTotals,
    session_start: Instant,
    interrupted: &Arc<AtomicBool>,
    on_event: impl FnMut(&StreamEvent, &SessionTotals),
) -> TurnReport {
    let budget = allowance.turn_budget(role, totals, session_start.elapsed());
    let deadline = allowance.turn_deadline(session_start);
    run_turn(
        runtime,
        messages,
        budget,
        totals,
        interrupted,
        Some(deadline),
        on_event,
    )
    .await
}

/// After a session limit: ask for the handoff in one more turn, bounded by
/// [`HANDOFF_TIMEOUT`] and the role's per-turn limits, and metered into
/// `totals` like every other turn (it is real spend).
pub async fn run_handoff_turn(
    runtime: &mut Runtime,
    messages: &mut Vec<SharedMessage>,
    role: &TurnBudget,
    totals: &mut SessionTotals,
    interrupted: &Arc<AtomicBool>,
    on_event: impl FnMut(&StreamEvent, &SessionTotals),
) -> TurnReport {
    push_user_prompt(messages, HANDOFF_PROMPT);
    let budget = role.tighter(&TurnBudget {
        max_elapsed: Some(HANDOFF_TIMEOUT),
        max_round_renewals: u32::MAX,
        ..TurnBudget::for_role(crate::runtime::budget::TurnRole::Foreground)
    });
    run_turn(
        runtime,
        messages,
        budget,
        totals,
        interrupted,
        Some(Instant::now() + HANDOFF_TIMEOUT),
        on_event,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::budget::{TurnRole, TOOL_CALL_BUDGET_REFUSAL};

    fn limits() -> SessionLimits {
        SessionLimits {
            max_session_tokens: 1_000,
            max_session_duration_mins: 60,
            max_session_cost_usd: 0.50,
            max_daily_cost_usd: 10.0,
            max_tool_calls: 10,
            ..SessionLimits::default()
        }
    }

    #[test]
    fn limits_are_reached_at_their_configured_values() {
        let limits = limits();
        let allowance = SessionAllowance {
            limits: &limits,
            daily_spent_before: 0.0,
        };
        let none = SessionTotals::default();
        assert_eq!(allowance.limit_reached(&none, Duration::ZERO), None);
        for (totals, elapsed, expected) in [
            (
                SessionTotals {
                    tokens: 1_000,
                    ..none
                },
                Duration::ZERO,
                SessionLimit::Tokens,
            ),
            (
                SessionTotals {
                    cost_usd: 0.50,
                    ..none
                },
                Duration::ZERO,
                SessionLimit::Cost,
            ),
            (
                SessionTotals {
                    tool_calls: 10,
                    ..none
                },
                Duration::ZERO,
                SessionLimit::ToolCalls,
            ),
            (none, Duration::from_secs(3600), SessionLimit::Time),
        ] {
            assert_eq!(allowance.limit_reached(&totals, elapsed), Some(expected));
        }
        let late_in_the_day = SessionAllowance {
            limits: &limits,
            daily_spent_before: 9.80,
        };
        let spent = SessionTotals {
            cost_usd: 0.20,
            ..none
        };
        assert_eq!(
            late_in_the_day.limit_reached(&spent, Duration::ZERO),
            Some(SessionLimit::DailyCost)
        );
    }

    #[test]
    fn the_turn_budget_is_the_remaining_allowance_capped_by_the_role() {
        let limits = limits();
        let allowance = SessionAllowance {
            limits: &limits,
            daily_spent_before: 9.90,
        };
        let role = TurnBudget::for_role(TurnRole::Autonomous);
        let totals = SessionTotals {
            tokens: 400,
            cost_usd: 0.05,
            tool_calls: 8,
        };
        let budget = allowance.turn_budget(&role, &totals, Duration::from_secs(50 * 60));
        assert_eq!(budget.max_tool_calls, Some(2));
        assert_eq!(budget.max_total_tokens, Some(600));
        assert_eq!(budget.max_elapsed, Some(Duration::from_secs(10 * 60)));
        // The daily cap (0.05 left) is tighter than the session's 0.45.
        assert!((budget.max_cost_usd.unwrap() - 0.05).abs() < 1e-9);
        // Role limits still apply where they are tighter.
        assert_eq!(budget.max_provider_rounds, Some(24));
        assert_eq!(budget.max_round_renewals, 0);

        let fresh = allowance.turn_budget(&role, &SessionTotals::default(), Duration::ZERO);
        assert_eq!(fresh.max_tool_calls, Some(10));
        let roomy = SessionLimits {
            max_tool_calls: 500,
            ..limits
        };
        let roomy = SessionAllowance {
            limits: &roomy,
            daily_spent_before: 0.0,
        };
        assert_eq!(
            roomy
                .turn_budget(&role, &SessionTotals::default(), Duration::ZERO)
                .max_tool_calls,
            Some(96),
            "the role's per-turn cap wins when the session has more left"
        );
    }

    #[test]
    fn cache_heavy_usage_is_priced_with_the_runtime_table() {
        let model = "claude-opus-4-1";
        let cost = usage_cost(model, 2_000, 1_000, 400_000, 100_000, None, None);
        let uncached_only = usage_cost(model, 2_000, 1_000, 0, 0, None, None);
        assert_eq!(
            cost,
            crate::pricing::calculate_cost_optional_split(
                model, 2_000, 1_000, 400_000, 100_000, None, None
            )
        );
        assert!(
            cost > uncached_only * 5.0,
            "cache reads/writes dominate a long watcher context: {cost} vs {uncached_only}"
        );
    }

    #[tokio::test]
    async fn drive_turn_meters_cache_tokens_and_counts_only_executed_calls() {
        let events = vec![
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "ls".into(),
                tool_id: "t1".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: "ls".into(),
                tool_id: "t2".into(),
            }),
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 200_000,
                cache_creation_input_tokens: 20_000,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: None,
            }),
            StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: "t1".into(),
                result: "ok".into(),
            }),
            StreamEvent::Llm(LlmEvent::ToolResult {
                tool_id: "t2".into(),
                result: TOOL_CALL_BUDGET_REFUSAL.into(),
            }),
        ];
        let mut totals = SessionTotals::default();
        let report = drive_turn(
            futures::stream::iter(events),
            "claude-sonnet-4-5",
            &mut totals,
            |_, _| {},
        )
        .await;
        assert_eq!(totals.tokens, 150);
        assert_eq!(totals.tool_calls, 1, "the refused call is not counted");
        let expected = usage_cost("claude-sonnet-4-5", 100, 50, 200_000, 20_000, None, None);
        assert!((totals.cost_usd - expected).abs() < 1e-12);
        assert!(!report.handoff_saved);
    }

    #[tokio::test]
    async fn only_a_successful_watcher_exit_result_counts_as_a_saved_handoff() {
        let start = StreamEvent::Llm(LlmEvent::ToolUseStart {
            tool_name: "watcher_exit".into(),
            tool_id: "x1".into(),
        });
        for (result, saved) in [
            (format!("{WATCHER_EXIT_ACK} Reason: done"), true),
            (TOOL_CALL_BUDGET_REFUSAL.to_string(), false),
            ("Canceled by user".to_string(), false),
            ("Error: Failed to rename handoff file".to_string(), false),
        ] {
            let mut totals = SessionTotals::default();
            let report = drive_turn(
                futures::stream::iter(vec![
                    start.clone(),
                    StreamEvent::Llm(LlmEvent::ToolResult {
                        tool_id: "x1".into(),
                        result,
                    }),
                ]),
                "claude-sonnet-4-5",
                &mut totals,
                |_, _| {},
            )
            .await;
            assert_eq!(report.handoff_saved, saved);
        }
        // The call starting is not enough.
        let mut totals = SessionTotals::default();
        let report = drive_turn(
            futures::stream::iter(vec![start]),
            "claude-sonnet-4-5",
            &mut totals,
            |_, _| {},
        )
        .await;
        assert!(!report.handoff_saved);
    }

    #[tokio::test]
    async fn turn_watcher_cancels_on_signal_and_at_the_deadline_only() {
        let poll = Duration::from_millis(5);
        // Signal.
        let cancel = CancellationToken::new();
        let watcher = tokio::spawn(watch_turn(
            cancel.clone(),
            TurnCompletion::new(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            None,
            poll,
        ));
        tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
            .await
            .expect("signal cancels the turn");
        watcher.await.unwrap();
        // Deadline.
        let cancel = CancellationToken::new();
        let watcher = tokio::spawn(watch_turn(
            cancel.clone(),
            TurnCompletion::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Some(Instant::now()),
            poll,
        ));
        tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
            .await
            .expect("deadline cancels the turn");
        watcher.await.unwrap();
        // Turn over: exits without cancelling.
        let cancel = CancellationToken::new();
        let turn_over = Arc::new(AtomicBool::new(false));
        let watcher = tokio::spawn(watch_turn(
            cancel.clone(),
            TurnCompletion::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&turn_over),
            None,
            poll,
        ));
        turn_over.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(5), watcher)
            .await
            .expect("watcher exits when the turn ends")
            .unwrap();
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn a_prompt_after_tool_results_joins_the_last_user_message() {
        let mut messages: Vec<SharedMessage> = vec![
            std::sync::Arc::new(serde_json::json!({"role": "user", "content": "boot"})),
            std::sync::Arc::new(serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "ls", "input": {}}
            ]})),
            std::sync::Arc::new(serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "x"}
            ]})),
        ];
        push_user_prompt(&mut messages, "write your handoff");
        assert_eq!(messages.len(), 3);
        let blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["text"], "write your handoff");

        messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "assistant", "content": "ok"}),
        ));
        push_user_prompt(&mut messages, "next");
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[4]["content"], "next");
    }
}
