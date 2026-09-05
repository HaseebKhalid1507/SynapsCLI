//! Task 28 — Phase 4 headless acceptance harness (spec §8).
//!
//! Every acceptance bullet maps to a named test that exercises the
//! PRODUCTION path: the real `Runtime` stream loop against a loopback
//! Anthropic SSE stub, the production effect-aware scheduler, the
//! production bash channel intermediary, the production delegation tree
//! (including a LIVE worker cancellation), and the production
//! trace/ledger/budget surfaces. Synthetic generators only — never a real
//! 1 GiB file.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use synaps_cli::orchestration::{DelegationTreeBudget, DelegationTreeDenied, OrchestrationRuntime};
use synaps_cli::runtime::budget::{TurnBudget, TurnRole};
use synaps_cli::runtime::trace::{
    CollectingTraceSink, ExecutionCommitStatus, ExecutionCorrelation, ExecutionPhase, TraceContext,
};
use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};
use synaps_cli::tools::activation::ActivationBasis;
use synaps_cli::tools::catalog::{ToolEffect, ToolId};
use synaps_cli::tools::ledger::CallLedger;
use synaps_cli::tools::output::{
    active_ui_forwarder_count, delta_channel_with_budgets, spawn_ui_forwarder, OutputBudgets,
};
use synaps_cli::tools::{
    bash_intermediary_snapshot, BashTool, ConcurrencyKey, SubagentRegistry, SubagentStartTool,
    Tool, ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolOrigin,
};
use synaps_cli::{BudgetDimension, Result, TurnOutcome, Value};
use tokio_util::sync::CancellationToken;

fn model(id: &str) -> agent_core::prompt::QualifiedModelId {
    agent_core::prompt::QualifiedModelId::parse(id).unwrap()
}

// ── SSE fixtures ────────────────────────────────────────────────────────────

/// One tool-use turn requesting the counting fixture tool. Served by
/// `Script::SeqSse` as the LAST body it repeats forever — the "model
/// requesting tools forever" stub.
const SSE_TOOL_LOOP: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_p1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_loop\",\"name\":\"phase_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// One turn with THREE parallel tool_use blocks (exact-call-budget case).
const SSE_THREE_TOOL_USES: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_p3\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_p1\",\"name\":\"phase_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_p2\",\"name\":\"phase_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_p3\",\"name\":\"phase_fixture_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// One turn with two tool_use blocks calling `name_a` then `name_b`.
fn sse_two_calls(name_a: &str, id_a: &str, name_b: &str, id_b: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_p2\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id_a}\",\"name\":\"{name_a}\"}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":1,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id_b}\",\"name\":\"{name_b}\"}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":1}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        id_a = id_a,
        name_a = name_a,
        id_b = id_b,
        name_b = name_b,
    )
}

// ── fixture tools (probes only — scheduling/budgeting stays production) ─────

/// Deterministic builtin-origin fixture counting real executions.
struct CountingFixtureTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingFixtureTool {
    fn name(&self) -> &str {
        "phase_fixture_tool"
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "phase 4 counting fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("ok".into())
    }
}

/// Read-only probe that trips the caller-owned cancellation token and then
/// stalls, so the PRODUCTION loop observes cancellation mid-execution.
struct StallCancelProbe {
    name: String,
    cancel: CancellationToken,
}

#[async_trait]
impl Tool for StallCancelProbe {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "stall-cancel probe"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.cancel.cancel();
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok("finished".into())
    }
}

/// Interleaving log recording `start:<tag>` / `end:<tag>` probe events.
type EventLog = Arc<Mutex<Vec<String>>>;

/// Mutating probe with a FIXED canonical concurrency key: the first call
/// sleeps (reordering pressure), so interleaved execution would show
/// `start:A,start:B` before `end:A`.
struct KeyedWriteProbe {
    name: String,
    tag: String,
    log: EventLog,
    delay_ms: u64,
}

#[async_trait]
impl Tool for KeyedWriteProbe {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "keyed write probe"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::IdempotentWrite
    }
    fn concurrency_key(&self, _input: &Value) -> Option<ConcurrencyKey> {
        Some(ConcurrencyKey::Key("/phase4/same/canonical/target".into()))
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.log.lock().unwrap().push(format!("start:{}", self.tag));
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        self.log.lock().unwrap().push(format!("end:{}", self.tag));
        Ok(format!("wrote:{}", self.tag))
    }
}

/// Cancellation-safe overlap detector shared by sibling calls: `current`
/// counts calls presently inside `execute`; `peak` is the monotonic maximum
/// ever observed. Under genuine serialization `peak` stays at 1.
#[derive(Default)]
struct ConcurrencyGauge {
    current: AtomicUsize,
    peak: AtomicUsize,
}

/// Reports OVERLAP if a sibling is concurrently inside `execute` (peak
/// reaches 2), or SERIAL if the bounded observation window elapses.
struct OverlapProbe {
    name: String,
    gauge: Arc<ConcurrencyGauge>,
}

#[async_trait]
impl Tool for OverlapProbe {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "overlap probe"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        let now = self.gauge.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.gauge.peak.fetch_max(now, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
        while self.gauge.peak.load(Ordering::SeqCst) < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let overlapped = self.gauge.peak.load(Ordering::SeqCst) >= 2;
        self.gauge.current.fetch_sub(1, Ordering::SeqCst);
        Ok(if overlapped { "OVERLAP" } else { "SERIAL" }.to_string())
    }
}

// ── runtime/event helpers ───────────────────────────────────────────────────

async fn runtime_with(budget: Option<TurnBudget>, tools: Vec<Arc<dyn Tool>>) -> Runtime {
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    if let Some(budget) = budget {
        rt.set_turn_budget(budget);
    }
    {
        let shared = rt.tools_shared();
        let mut registry = shared.write().await;
        for tool in tools {
            registry.register(tool);
        }
    }
    rt
}

/// Drive one turn against a caller-owned cancellation token (so a probe can
/// trip it mid-execute), returning every observed event.
async fn drive_with_cancel(
    rt: &Runtime,
    prompt: &str,
    cancel: CancellationToken,
) -> Vec<StreamEvent> {
    use futures::StreamExt;
    let mut stream = rt.run_stream(prompt.to_string(), cancel).await;
    let mut events = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("turn hung beyond 30 s")
    {
        let done = matches!(ev, StreamEvent::Session(SessionEvent::Done));
        events.push(ev);
        if done {
            break;
        }
    }
    events
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
    assert!(!use_ids.is_empty(), "the fixture turn must request tools");
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

// ── §8 acceptance: turn budgets through the production loop ─────────────────

/// A stub model requesting tools forever stops at EXACTLY the configured
/// provider-round budget in the PRODUCTION stream loop: exactly N provider
/// calls, N executions, typed BudgetExceeded, fully paired history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn infinite_tool_loop_stops_at_exact_configured_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_TOOL_LOOP])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_provider_rounds: 3,
        max_round_renewals: 0,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let executions = Arc::new(AtomicUsize::new(0));
    let rt = runtime_with(
        Some(budget),
        vec![Arc::new(CountingFixtureTool {
            executions: Arc::clone(&executions),
        })],
    )
    .await;
    let events = drive_runtime_turn(&rt, "loop forever", false).await;

    assert_eq!(hits.load(Ordering::SeqCst), 3, "exactly N provider rounds");
    assert_eq!(executions.load(Ordering::SeqCst), 3, "one call per round");
    assert_eq!(
        budget_outcome(&events).expect("typed budget outcome surfaced"),
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ProviderRounds
        }
    );
    assert_history_pairing(&final_history(&events));
}

/// Calls beyond the exact tool-call allowance never execute and still get
/// SYNTHETIC valid results in model order from the PRODUCTION loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn every_tool_use_gets_a_valid_result_at_budget_exhaustion() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_THREE_TOOL_USES, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let budget = TurnBudget {
        max_tool_calls: 1,
        ..TurnBudget::for_role(TurnRole::Foreground)
    };
    let executions = Arc::new(AtomicUsize::new(0));
    let rt = runtime_with(
        Some(budget),
        vec![Arc::new(CountingFixtureTool {
            executions: Arc::clone(&executions),
        })],
    )
    .await;
    let events = drive_runtime_turn(&rt, "three calls", false).await;

    assert_eq!(hits.load(Ordering::SeqCst), 1, "no further provider round");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "exactly the configured tool-call budget executes"
    );
    assert_eq!(
        budget_outcome(&events).expect("typed budget outcome surfaced"),
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ToolCalls
        }
    );
    let history = final_history(&events);
    assert_history_pairing(&history);
    let results = tool_result_contents(&history);
    assert_eq!(
        results
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["toolu_p1", "toolu_p2", "toolu_p3"],
        "model order preserved"
    );
    assert!(!results[0].1.contains("budget"), "the first call executed");
    for (_, content) in &results[1..] {
        assert!(
            content.contains("budget"),
            "over-budget synthetic result names the budget: {content}"
        );
    }
}

/// Mid-execution cancellation still leaves every emitted tool_use paired
/// with a valid result ("Canceled by user") in the PRODUCTION loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn every_tool_use_gets_a_valid_result_on_mid_execution_cancellation() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("stall_ro_a", "toolu_x1", "stall_ro_b", "toolu_x2").into_boxed_str(),
    );
    let (url, _hits, _) = spawn_stub(Script::SeqSse(Box::leak(Box::new([body])))).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let cancel = CancellationToken::new();
    let rt = runtime_with(
        None,
        vec![
            Arc::new(StallCancelProbe {
                name: "stall_ro_a".into(),
                cancel: cancel.clone(),
            }),
            Arc::new(StallCancelProbe {
                name: "stall_ro_b".into(),
                cancel: cancel.clone(),
            }),
        ],
    )
    .await;
    let events = drive_with_cancel(&rt, "stall twice", cancel).await;

    let history = final_history(&events);
    assert_history_pairing(&history);
    let results = tool_result_contents(&history);
    assert_eq!(results.len(), 2);
    for (id, content) in &results {
        assert!(
            content.contains("Canceled"),
            "canceled call {id} must carry a valid canceled result: {content}"
        );
    }
}

// ── §8 acceptance: bounded output through the production channel ────────────

#[tokio::test]
async fn synthetic_one_gib_slow_consumer_stays_under_fixed_retention_ceiling() {
    const GIB: u64 = 1024 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    let channel = delta_channel_with_budgets(
        OutputBudgets {
            ui_preview_bytes: 1024,
            model_history_bytes: 4096,
        },
        None,
    );
    let output = channel.output_handle();
    let chunk = "x".repeat(CHUNK);
    // This is an exact retained-byte oracle over the production channel, not
    // a process-RSS measurement. The allocator, Tokio, and test harness add
    // nondeterministic resident memory outside this component invariant.
    // Only one fixed 64 KiB chunk is allocated and reused; no 1 GiB file or
    // String is ever materialized.
    for _ in 0..GIB / CHUNK as u64 {
        channel.sender.send(chunk.clone());
    }
    let history = output.model_history();
    assert_eq!(history.original_bytes as u64, GIB);
    assert_eq!(history.retained_bytes, 4096);
    assert!(
        output.counters().snapshot().retained_bytes() <= OutputBudgets::max_ui_retained_bytes()
    );
}

#[tokio::test]
async fn production_bash_handoff_conserves_bytes_for_large_generated_output() {
    let before = bash_intermediary_snapshot();
    let tool = BashTool;
    let ctx = ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
        },
        limits: ToolLimits {
            max_tool_output: 4096,
            max_tool_buffer: 64 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 60,
            subagent_timeout: 30,
        },
    };
    let output = tool
        .execute(
            serde_json::json!({
                "command": "python3 -c \"import sys; sys.stdout.write('x' * 1048576)\"",
                "timeout": 30
            }),
            ctx,
        )
        .await
        .expect("production bash path");
    assert!(output.contains("output truncated"));
    let after = bash_intermediary_snapshot();
    let produced = after.produced_bytes - before.produced_bytes;
    let consumed = after.consumed_bytes - before.consumed_bytes;
    let dropped = after.dropped_bytes - before.dropped_bytes;
    assert_eq!(produced, consumed + dropped);
    assert_eq!(after.retained_bytes, before.retained_bytes);
}

// ── §8 acceptance: effect-aware scheduling through the production loop ──────

/// Two mutating calls sharing one canonical concurrency key run serially in
/// model order through the PRODUCTION scheduler, even when the first is
/// slow (reordering pressure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn same_path_writes_are_serialized_in_model_order() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("keyed_write_a", "toolu_w1", "keyed_write_b", "toolu_w2").into_boxed_str(),
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let rt = runtime_with(
        None,
        vec![
            Arc::new(KeyedWriteProbe {
                name: "keyed_write_a".into(),
                tag: "A".into(),
                log: Arc::clone(&log),
                delay_ms: 300,
            }),
            Arc::new(KeyedWriteProbe {
                name: "keyed_write_b".into(),
                tag: "B".into(),
                log: Arc::clone(&log),
                delay_ms: 0,
            }),
        ],
    )
    .await;
    let events = drive_runtime_turn(&rt, "write twice", false).await;

    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:A", "end:A", "start:B", "end:B"],
        "same-key mutations must serialize in model order"
    );
    let results = tool_result_contents(&final_history(&events));
    assert_eq!(results[0].0, "toolu_w1");
    assert_eq!(results[1].0, "toolu_w2");
}

/// Independent read-only calls OVERLAP through the PRODUCTION scheduler:
/// each observes the sibling concurrently inside `execute`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn independent_read_only_calls_overlap() {
    let _guard = HomeGuard::new();
    let body: &'static str =
        Box::leak(sse_two_calls("ro_alpha", "toolu_r1", "ro_beta", "toolu_r2").into_boxed_str());
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let gauge = Arc::new(ConcurrencyGauge::default());
    let rt = runtime_with(
        None,
        vec![
            Arc::new(OverlapProbe {
                name: "ro_alpha".into(),
                gauge: Arc::clone(&gauge),
            }),
            Arc::new(OverlapProbe {
                name: "ro_beta".into(),
                gauge: Arc::clone(&gauge),
            }),
        ],
    )
    .await;
    let events = drive_runtime_turn(&rt, "read twice", false).await;

    let results = tool_result_contents(&final_history(&events));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].1, "OVERLAP", "read-only calls may overlap");
    assert_eq!(results[1].1, "OVERLAP", "read-only calls may overlap");
}

// ── §8 acceptance: ledger disposition (production surface) ──────────────────

#[test]
fn cancel_after_nonidempotent_start_is_not_automatically_duplicated() {
    let disposition =
        CallLedger::interrupted_started("call-side-effect", ToolEffect::NonIdempotent);
    assert!(!disposition.auto_rerun_permitted);
    assert!(disposition.outcome.is_some());
}

// ── §8 acceptance: cancellation releases forwarders and LIVE leases ─────────

/// Cancellation closes the bounded UI forwarder AND a LIVE spawned worker
/// releases its delegation lease through the production finalizer path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancellation_closes_forwarder_and_releases_live_delegation_leases() {
    let _guard = HomeGuard::new();
    // Forwarder half: cancel closes the bounded UI lane.
    let baseline = active_ui_forwarder_count();
    let channel = delta_channel_with_budgets(OutputBudgets::for_limits(1024), None);
    let cancel = CancellationToken::new();
    let forwarder = spawn_ui_forwarder(channel.receiver, 128, cancel.clone(), |_| {});
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), forwarder)
        .await
        .expect("bounded cancellation")
        .expect("forwarder");
    assert!(active_ui_forwarder_count() <= baseline);

    // Live delegation half: a REAL worker held open by an endless provider
    // stream is cancelled and must release its lease via the production
    // terminal finalizer (never by test-side bookkeeping).
    let (url, _hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let queue = Arc::new(synaps_cli::events::EventQueue::new(100));
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    let foreground = model("anthropic/claude-sonnet-4-6");
    let orchestration =
        Arc::new(OrchestrationRuntime::baseline(foreground, 8, 64).expect("routable foreground"));
    let ctx = ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: Some(Arc::clone(&registry)),
            event_queue: Some(Arc::clone(&queue)),
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: Some(Arc::clone(&orchestration)),
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 60,
            subagent_timeout: 60,
        },
    };
    let started = SubagentStartTool
        .execute(
            serde_json::json!({
                "system_prompt": "You are a bounded phase-4 fixture worker.",
                "task": "Stream forever until cancelled.",
                "timeout": 60
            }),
            ctx,
        )
        .await
        .expect("live worker start");
    let handle_id = serde_json::from_str::<serde_json::Value>(&started).unwrap()["handle_id"]
        .as_str()
        .expect("handle id")
        .to_string();
    assert_eq!(
        orchestration.delegation_descendants(),
        1,
        "a live worker holds exactly one delegation lease"
    );

    registry
        .lock()
        .unwrap()
        .get_mut(&handle_id)
        .expect("live handle")
        .cancel();
    let deadline = Instant::now() + Duration::from_secs(20);
    while orchestration.delegation_descendants() != 0 {
        assert!(
            Instant::now() < deadline,
            "cancelled live worker must release its delegation lease"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── §8 acceptance: correlated events / delegation tree / role budgets ───────

#[test]
fn correlated_events_share_ids_preserve_model_order_and_have_bounded_previews() {
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink);
    let request = trace.reserve_request_correlation().unwrap();
    let correlation = ExecutionCorrelation::from_request(&trace, &request);
    for (order, call) in ["toolu-1", "toolu-2"].into_iter().enumerate() {
        correlation.record(
            call,
            &ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::ResultRecorded,
            Instant::now(),
            1_000_000,
            64,
            ActivationBasis::Core,
            ToolEffect::NonIdempotent,
            ExecutionCommitStatus::ResultRecorded,
            order,
        );
    }
    let events = trace.execution_events(&request.request_id);
    assert_eq!(
        events
            .iter()
            .map(|event| event.model_order)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(events.iter().all(|event| {
        event.session_id == request.session_id
            && event.turn_id == request.turn_id
            && event.request_id == request.request_id
            && event.preview_bytes <= 64
            && event.truncated
    }));
}

#[test]
fn delegation_tree_depth_and_fanout_are_bounded_before_dispatch() {
    let foreground = model("anthropic/foreground");
    let runtime = OrchestrationRuntime::new(agent_core::orchestration::DelegationPolicy::enforced(
        foreground.clone(),
        [foreground],
        4,
        8,
    ))
    .with_tree_budget(DelegationTreeBudget {
        max_depth: 1,
        max_children_per_worker: 1,
        max_total_descendants: 2,
    })
    .unwrap();
    assert_eq!(runtime.reserve_delegation("root-child", None), Ok(1));
    assert_eq!(
        runtime.reserve_delegation("nested", Some("root-child")),
        Err(DelegationTreeDenied::DepthLimit)
    );
    assert_eq!(
        runtime.reserve_delegation("root-sibling", None),
        Err(DelegationTreeDenied::ChildLimit)
    );
}

#[test]
fn role_defaults_are_finite_for_every_autonomous_role() {
    for role in [TurnRole::Foreground, TurnRole::Autonomous, TurnRole::Worker] {
        let budget = TurnBudget::for_role(role);
        assert!(budget.max_provider_rounds > 0);
        assert!(budget.max_tool_calls > 0);
        assert!(budget.max_elapsed < Duration::from_secs(24 * 60 * 60));
    }
}
