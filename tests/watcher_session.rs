//! Watcher agent (`synaps agent`) turns, end to end through the real
//! `Runtime` stream loop against a loopback Anthropic SSE stub.
//!
//! The session limits bind inside a turn through the engine budget
//! (`SessionAllowance::turn_budget`), never by cancelling the stream:
//! - a parallel batch that crosses `max_tool_calls` runs its in-budget
//!   calls, the rest get the engine's refusal, and refused calls are not
//!   counted;
//! - a round whose cache-heavy usage crosses the cost limit while calling
//!   `watcher_exit` stops the turn with a valid history, is NOT taken as a
//!   clean exit, and the metered handoff turn that follows writes the
//!   handoff;
//! - a signal-cancelled turn leaves a valid history.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use synaps_cli::runtime::budget::{TurnBudget, TurnRole, TOOL_CALL_BUDGET_REFUSAL};
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin};
use synaps_cli::watcher_session::{
    run_handoff_turn, run_turn, SessionAllowance, SessionLimit, SessionTotals,
};
use synaps_cli::{
    BudgetDimension, Result, Runtime, SessionLimits, SharedMessage, TurnOutcome, Value,
};

const MODEL: &str = "claude-sonnet-4-5";

/// Three parallel calls to the fixture tool.
const SSE_THREE_TOOL_USES: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_w3\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_w1\",\"name\":\"watcher_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_w2\",\"name\":\"watcher_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_w3\",\"name\":\"watcher_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A `watcher_exit` call in a round that reads 2M cached tokens: ~$0.60 at
/// Sonnet's cache-read price, over a $0.50 session limit, while the
/// uncached input + output stays tiny.
const SSE_EXPENSIVE_WATCHER_EXIT: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":50,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":2000000}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_x1\",\"name\":\"watcher_exit\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"reason\\\":\\\"done\\\",\\\"summary\\\":\\\"first try\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":50,\"output_tokens\":20,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":2000000}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The handoff turn: a cheap `watcher_exit` call.
const SSE_CHEAP_WATCHER_EXIT: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":30,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_x2\",\"name\":\"watcher_exit\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"reason\\\":\\\"limit\\\",\\\"summary\\\":\\\"handoff after limit\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":30,\"output_tokens\":10,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// After the handoff the model ends the turn.
const SSE_DONE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x3\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"bye\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

struct FixtureTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        "watcher_fixture_tool"
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "watcher fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("ran".into())
    }
}

/// A runtime set up like `synaps agent` sets it up: watcher_exit with a
/// handoff path, the autonomous role budget.
async fn watcher_runtime(handoff: &std::path::Path) -> (Runtime, Arc<AtomicUsize>) {
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model(MODEL.to_string());
    rt.set_turn_budget(TurnBudget::for_role(TurnRole::Autonomous));
    rt.watcher_exit_path = Some(handoff.to_path_buf());
    let executions = Arc::new(AtomicUsize::new(0));
    {
        let tools = rt.tools_shared();
        let mut tools = tools.write().await;
        tools.register(Arc::new(synaps_cli::tools::WatcherExitTool));
        tools.register(Arc::new(FixtureTool {
            executions: Arc::clone(&executions),
        }));
    }
    (rt, executions)
}

fn limits() -> SessionLimits {
    SessionLimits {
        max_session_tokens: 100_000,
        max_session_duration_mins: 60,
        max_session_cost_usd: 0.50,
        max_daily_cost_usd: 10.0,
        max_tool_calls: 10,
        ..SessionLimits::default()
    }
}

fn prompt() -> Vec<SharedMessage> {
    vec![Arc::new(
        serde_json::json!({"role":"user","content":"boot"}),
    )]
}

fn tool_results(history: &[SharedMessage]) -> Vec<(String, String)> {
    history
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            (
                b["tool_use_id"].as_str().unwrap_or("").to_string(),
                b["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Every tool_use has exactly one tool_result: no dangling tool_use.
fn assert_history_pairing(history: &[SharedMessage]) {
    let mut uses: Vec<String> = Vec::new();
    for msg in history {
        for b in msg["content"].as_array().into_iter().flatten() {
            if b["type"] == "tool_use" {
                uses.push(b["id"].as_str().unwrap_or("").to_string());
            }
        }
    }
    let mut results: Vec<String> = tool_results(history)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    uses.sort();
    results.sort();
    assert_eq!(
        uses, results,
        "every tool_use keeps exactly one tool_result"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parallel_batch_crossing_the_tool_limit_runs_its_in_budget_calls() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_THREE_TOOL_USES])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let dir = tempfile::tempdir().unwrap();
    let (mut rt, executions) = watcher_runtime(&dir.path().join("handoff.json")).await;

    let limits = limits();
    let allowance = SessionAllowance {
        limits: &limits,
        daily_spent_before: 0.0,
    };
    let mut totals = SessionTotals {
        tool_calls: 8,
        ..SessionTotals::default()
    };
    let role = rt.turn_budget().clone();
    let budget = allowance.turn_budget(&role, &totals, Duration::ZERO);
    let mut messages = prompt();
    let interrupted = Arc::new(AtomicBool::new(false));
    let report = run_turn(
        &mut rt,
        &mut messages,
        budget,
        &mut totals,
        &interrupted,
        None,
        |_, _| {},
    )
    .await;

    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "the two in-budget calls ran"
    );
    assert_eq!(totals.tool_calls, 10, "the refused call is not counted");
    assert_eq!(
        report.error.map(|e| e.outcome),
        Some(TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ToolCalls
        })
    );
    assert_history_pairing(&messages);
    let results = tool_results(&messages);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].1, "ran");
    assert_eq!(results[1].1, "ran");
    assert_eq!(results[2].1, TOOL_CALL_BUDGET_REFUSAL);
    assert!(
        results.iter().all(|(_, c)| !c.contains("Canceled by user")),
        "a limit is not a user cancel: {results:?}"
    );
    assert_eq!(
        allowance.limit_reached(&totals, Duration::ZERO),
        Some(SessionLimit::ToolCalls)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cost_limit_crossed_while_calling_watcher_exit_still_ends_with_a_handoff() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[
        SSE_EXPENSIVE_WATCHER_EXIT,
        SSE_CHEAP_WATCHER_EXIT,
        SSE_DONE,
    ]))
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let dir = tempfile::tempdir().unwrap();
    let handoff = dir.path().join("handoff.json");
    std::fs::write(
        &handoff,
        r#"{"summary":"previous session","pending":[],"context":null}"#,
    )
    .unwrap();
    let (mut rt, _) = watcher_runtime(&handoff).await;

    let limits = limits();
    let allowance = SessionAllowance {
        limits: &limits,
        daily_spent_before: 0.0,
    };
    let mut totals = SessionTotals::default();
    let role = rt.turn_budget().clone();
    let interrupted = Arc::new(AtomicBool::new(false));
    let mut messages = prompt();

    // The expensive round crosses the cost limit: the engine stops the turn
    // before running its tools, so watcher_exit never ran.
    let budget = allowance.turn_budget(&role, &totals, Duration::ZERO);
    let report = run_turn(
        &mut rt,
        &mut messages,
        budget,
        &mut totals,
        &interrupted,
        None,
        |_, _| {},
    )
    .await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        report.error.map(|e| e.outcome),
        Some(TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::CostUsd
        })
    );
    assert!(
        !report.handoff_saved,
        "a started watcher_exit is not a clean exit"
    );
    assert_history_pairing(&messages);
    assert!(
        totals.cost_usd > 0.50,
        "cache reads are priced into the session cost: {}",
        totals.cost_usd
    );
    assert!(totals.tokens < 1_000, "cache tokens are not session tokens");
    assert_eq!(
        allowance.limit_reached(&totals, Duration::ZERO),
        Some(SessionLimit::Cost)
    );
    let unchanged = std::fs::read_to_string(&handoff).unwrap();
    assert!(unchanged.contains("previous session"));

    // The handoff turn runs, writes the handoff, and is metered.
    let cost_before = totals.cost_usd;
    let report = run_handoff_turn(
        &mut rt,
        &mut messages,
        &role,
        &mut totals,
        &interrupted,
        |_, _| {},
    )
    .await;
    assert!(report.handoff_saved, "the handoff turn saved the handoff");
    let written = std::fs::read_to_string(&handoff).unwrap();
    assert!(written.contains("handoff after limit"), "{written}");
    assert!(totals.cost_usd > cost_before, "the handoff turn is metered");
    assert_eq!(totals.tool_calls, 1);
    assert_history_pairing(&messages);
    // The handoff prompt joined the user message that ended the stopped
    // turn instead of adding a second consecutive user message.
    for pair in messages.windows(2) {
        assert!(
            !(pair[0]["role"] == "user" && pair[1]["role"] == "user"),
            "no consecutive user messages"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_signal_cancelled_turn_leaves_a_valid_history() {
    let _guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let dir = tempfile::tempdir().unwrap();
    let (mut rt, _) = watcher_runtime(&dir.path().join("handoff.json")).await;

    let limits = limits();
    let allowance = SessionAllowance {
        limits: &limits,
        daily_spent_before: 0.0,
    };
    let mut totals = SessionTotals::default();
    let role = rt.turn_budget().clone();
    let budget = allowance.turn_budget(&role, &totals, Duration::ZERO);
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        flag.store(true, Ordering::Release);
    });
    let mut messages = prompt();
    let report = tokio::time::timeout(
        Duration::from_secs(20),
        run_turn(
            &mut rt,
            &mut messages,
            budget,
            &mut totals,
            &interrupted,
            None,
            |_, _| {},
        ),
    )
    .await
    .expect("a signal ends the turn");
    assert!(!report.handoff_saved);
    assert_eq!(messages[0]["content"], "boot");
    assert_history_pairing(&messages);
}
