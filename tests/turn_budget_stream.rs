//! Task 23 — `TurnBudget` enforcement in the stream loop (spec §8.1).
//!
//! End-to-end through the real `Runtime` stream loop against a loopback
//! Anthropic SSE stub: a stub model requesting tools forever must stop at
//! EXACTLY the configured budget, every emitted `tool_use` must retain a
//! matching valid `tool_result` (synthetic at exhaustion), the final
//! history must stay valid, and the terminal event must carry the typed
//! `TurnOutcome::BudgetExceeded { dimension }`.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use synaps_cli::runtime::budget::{TurnBudget, TurnRole};
use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin};
use synaps_cli::{BudgetDimension, Result, TurnOutcome, Value};

// ── SSE fixtures ────────────────────────────────────────────────────────────

/// One tool-use turn requesting the budget fixture tool. Served by
/// `Script::SeqSse` as the LAST body it repeats forever — the "model
/// requesting tools forever" stub.
const SSE_TOOL_LOOP: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_loop\",\"name\":\"budget_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// One turn with THREE parallel tool_use blocks (exact-call-budget case).
const SSE_THREE_TOOL_USES: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b3\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_c1\",\"name\":\"budget_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_c2\",\"name\":\"budget_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_c3\",\"name\":\"budget_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

// ── fixtures ────────────────────────────────────────────────────────────────

/// Deterministic-output builtin-origin fixture tool (authorized under the
/// default core in flag-off mode). Counts executions.
struct BudgetFixtureTool {
    executions: Arc<AtomicUsize>,
    output_bytes: usize,
}

#[async_trait]
impl Tool for BudgetFixtureTool {
    fn name(&self) -> &str {
        "budget_fixture_tool"
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "budget fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("B".repeat(self.output_bytes))
    }
}

async fn runtime_with_fixture(
    budget: TurnBudget,
    output_bytes: usize,
) -> (Runtime, Arc<AtomicUsize>) {
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_turn_budget(budget);
    let executions = Arc::new(AtomicUsize::new(0));
    rt.tools_shared()
        .write()
        .await
        .register(Arc::new(BudgetFixtureTool {
            executions: Arc::clone(&executions),
            output_bytes,
        }));
    (rt, executions)
}

fn final_history(events: &[StreamEvent]) -> Vec<synaps_cli::SharedMessage> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            StreamEvent::Session(SessionEvent::MessageHistory(h)) => Some(h.clone()),
            _ => None,
        })
        .expect("turn must surface message history")
}

/// Every tool_use id in assistant messages must have exactly one matching
/// tool_result in a subsequent user message — the validity invariant.
fn assert_history_pairing(history: &[synaps_cli::SharedMessage]) {
    let mut use_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for msg in history {
        if let Some(blocks) = msg["content"].as_array() {
            for b in blocks {
                match b["type"].as_str() {
                    Some("tool_use") => use_ids.push(b["id"].as_str().unwrap_or("").to_string()),
                    Some("tool_result") => {
                        result_ids.push(b["tool_use_id"].as_str().unwrap_or("").to_string())
                    }
                    _ => {}
                }
            }
        }
    }
    use_ids.sort();
    result_ids.sort();
    assert_eq!(
        use_ids, result_ids,
        "every tool_use must retain exactly one matching tool_result"
    );
    assert!(!use_ids.is_empty() || result_ids.is_empty());
}

fn budget_outcome(events: &[StreamEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        StreamEvent::Session(SessionEvent::Error(err)) => match &err.outcome {
            TurnOutcome::BudgetExceeded { .. } => Some(err.outcome.clone()),
            _ => None,
        },
        _ => None,
    })
}

fn tool_result_contents(history: &[synaps_cli::SharedMessage]) -> Vec<(String, String)> {
    history
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flat_map(|blocks| blocks.iter())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            (
                b["tool_use_id"].as_str().unwrap_or("").to_string(),
                b["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

// ── scenarios ───────────────────────────────────────────────────────────────

/// A stub model requesting tools forever stops at EXACTLY the configured
/// provider-round budget: exactly N provider calls, typed
/// BudgetExceeded(ProviderRounds), fully paired history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn infinite_tool_loop_stops_at_exact_round_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_TOOL_LOOP])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_provider_rounds: 3,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let (rt, executions) = runtime_with_fixture(budget, 8).await;
    let events = drive_runtime_turn(&rt, "loop forever", false).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "exactly the configured number of provider rounds"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 3, "one call per round");
    let outcome = budget_outcome(&events).expect("typed budget outcome surfaced");
    assert_eq!(
        outcome,
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ProviderRounds
        }
    );
    assert_history_pairing(&final_history(&events));
}

/// The tool-call budget stops at EXACTLY the configured call count: calls
/// beyond the remaining allowance receive SYNTHETIC valid tool_results in
/// model order, nothing over-budget executes, and the typed outcome is
/// ToolCalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn tool_call_budget_synthesizes_results_beyond_exact_allowance() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_THREE_TOOL_USES])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_tool_calls: 2,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let (rt, executions) = runtime_with_fixture(budget, 8).await;
    let events = drive_runtime_turn(&rt, "three calls", false).await;

    assert_eq!(hits.load(Ordering::SeqCst), 1, "no further provider round");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "exactly the configured tool-call budget executes"
    );
    let outcome = budget_outcome(&events).expect("typed budget outcome surfaced");
    assert_eq!(
        outcome,
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ToolCalls
        }
    );
    let history = final_history(&events);
    assert_history_pairing(&history);
    let results = tool_result_contents(&history);
    assert_eq!(results.len(), 3);
    // Model order preserved; the third is the synthetic budget result.
    assert_eq!(results[0].0, "toolu_c1");
    assert_eq!(results[1].0, "toolu_c2");
    assert_eq!(results[2].0, "toolu_c3");
    assert!(
        results[2].1.contains("budget"),
        "synthetic result names the budget: {}",
        results[2].1
    );
    assert!(!results[0].1.contains("budget"));
}

/// Accumulated tool-result bytes: exceeding the byte budget after a round
/// finalizes valid history (results kept) and stops with ToolResultBytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn accumulated_tool_result_bytes_budget_stops_turn() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_TOOL_LOOP])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_accumulated_tool_result_bytes: 1024,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let (rt, executions) = runtime_with_fixture(budget, 4096).await;
    let events = drive_runtime_turn(&rt, "big output", false).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "stops after the first round"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let outcome = budget_outcome(&events).expect("typed budget outcome surfaced");
    assert_eq!(
        outcome,
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ToolResultBytes
        }
    );
    assert_history_pairing(&final_history(&events));
}

/// Wall-clock exhaustion is checked BEFORE a provider call: a zero budget
/// stops pre-flight with zero provider hits and valid (unchanged) history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn wall_clock_budget_stops_before_any_provider_call() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_TOOL_LOOP])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_elapsed: Duration::ZERO,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let (rt, executions) = runtime_with_fixture(budget, 8).await;
    let events = drive_runtime_turn(&rt, "never call", false).await;

    assert_eq!(hits.load(Ordering::SeqCst), 0, "no provider call");
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let outcome = budget_outcome(&events).expect("typed budget outcome surfaced");
    assert_eq!(
        outcome,
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::WallClock
        }
    );
    assert_history_pairing(&final_history(&events));
}

/// Per-role defaults are typed, distinct, and config-overridable; the
/// chat auto-turn cap (reactor-level, ACROSS turns) composes with — and is
/// not duplicated by — the engine per-turn budget.
#[test]
fn per_role_defaults_and_auto_turn_composition() {
    let fg = TurnBudget::for_role(TurnRole::Foreground);
    let auto = TurnBudget::for_role(TurnRole::Autonomous);
    let worker = TurnBudget::for_role(TurnRole::Worker);
    assert!(fg.max_provider_rounds > auto.max_provider_rounds);
    assert!(worker.max_provider_rounds <= fg.max_provider_rounds);
    assert!(auto.max_elapsed < fg.max_elapsed);
    assert!(fg.max_context_tokens.is_none() && fg.max_cost_usd.is_none());

    // Typed config overrides per role.
    let mut cfg = synaps_cli::config::TurnBudgetsConfig::default();
    cfg.worker.max_provider_rounds = Some(3);
    cfg.worker.max_cost_usd = Some(0.25);
    let from_cfg = TurnBudget::from_config(TurnRole::Worker, &cfg);
    assert_eq!(from_cfg.max_provider_rounds, 3);
    assert_eq!(from_cfg.max_cost_usd, Some(0.25));
    // Unset fields keep the role defaults.
    assert_eq!(from_cfg.max_tool_calls, worker.max_tool_calls);

    // Composition, not duplication: the reactor auto-turn cap is a
    // separate ACROSS-turn mechanism and remains enforced on its own
    // counter regardless of the per-turn budget.
    use synaps_cli::engine::reactor::{claim_auto_turn, AUTO_TURN_CAP};
    let mut consecutive = AUTO_TURN_CAP - 1;
    assert!(claim_auto_turn(&mut consecutive), "under the cap: allowed");
    assert!(
        !claim_auto_turn(&mut consecutive),
        "at the cap: denied independent of any TurnBudget"
    );
}
