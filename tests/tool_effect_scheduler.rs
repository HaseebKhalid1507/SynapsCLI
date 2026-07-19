//! Task 24 — `ToolEffect` metadata and the effect-aware scheduler
//! (spec §8.2), end-to-end through the real `Runtime` stream loop against
//! a loopback Anthropic SSE stub.
//!
//! - two writes to the SAME canonical concurrency key execute serially in
//!   model order under deliberate reordering pressure;
//! - independent read-only tools OVERLAP (observed via a barrier);
//! - unclassified tools serialize by default;
//! - tool_result blocks always match model request order regardless of
//!   completion order;
//! - built-in classification: read/grep/find/ls read-only, write/edit
//!   idempotent-write keyed by canonical path, bash non-idempotent, and
//!   dynamic (MCP/extension) tools default non-idempotent.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};
use synaps_cli::tools::catalog::{ToolEffect, ToolId};
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};
use synaps_cli::{Result, Value};

// ── SSE fixtures ────────────────────────────────────────────────────────────

fn sse_two_calls(name_a: &str, id_a: &str, name_b: &str, id_b: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_e1\",\"type\":\"message\",",
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

// ── fixtures ────────────────────────────────────────────────────────────────

/// Event log recording execution interleaving: `start:<tag>` / `end:<tag>`.
type EventLog = Arc<Mutex<Vec<String>>>;

/// Mutating fixture with a FIXED concurrency key: the first call sleeps
/// (reordering pressure), so interleaved execution would show
/// start:A,start:B before end:A.
struct KeyedWriteFixture {
    name: String,
    tag: String,
    log: EventLog,
    delay_ms: u64,
}

#[async_trait]
impl Tool for KeyedWriteFixture {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "keyed write fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::IdempotentWrite
    }
    fn concurrency_key(&self, _input: &Value) -> Option<String> {
        Some("/same/canonical/target".to_string())
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.log.lock().unwrap().push(format!("start:{}", self.tag));
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        self.log.lock().unwrap().push(format!("end:{}", self.tag));
        Ok(format!("wrote:{}", self.tag))
    }
}

/// Cancellation-safe overlap detector shared by sibling calls.
///
/// `current` counts calls presently inside `execute`; `peak` is the
/// monotonic maximum ever observed. Because `peak` never decreases, a real
/// overlap (two calls in-flight at once) is observed deterministically no
/// matter which sibling exits first — unlike `tokio::sync::Barrier`, whose
/// `wait()` slot is NOT released when a `timeout` drops the future, so a
/// serialized second caller would falsely trip a stale barrier and report
/// OVERLAP. Under genuine serialization `peak` stays at 1 (siblings never
/// coexist), yielding a truthful SERIAL.
#[derive(Default)]
struct ConcurrencyGauge {
    current: AtomicUsize,
    peak: AtomicUsize,
}

/// Overlap fixture: reports OVERLAP if a sibling is concurrently inside
/// `execute` (peak reaches 2), or SERIAL if the observation window elapses
/// with no concurrent sibling (serialized execution).
struct OverlapFixture {
    name: String,
    effect: ToolEffect,
    gauge: Arc<ConcurrencyGauge>,
    log: EventLog,
}

#[async_trait]
impl Tool for OverlapFixture {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "overlap fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        self.effect
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        self.log.lock().unwrap().push(format!("start:{}", self.name));
        let now = self.gauge.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.gauge.peak.fetch_max(now, Ordering::SeqCst);
        // Bounded observation window: wait for a concurrent sibling to
        // raise the shared peak. Under serialization the sibling cannot
        // enter until we exit, so the peak stays at 1 and we report SERIAL;
        // a real overlap raises the peak to 2 and is seen deterministically.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
        while self.gauge.peak.load(Ordering::SeqCst) < 2
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let overlapped = self.gauge.peak.load(Ordering::SeqCst) >= 2;
        self.gauge.current.fetch_sub(1, Ordering::SeqCst);
        self.log.lock().unwrap().push(format!("end:{}", self.name));
        Ok(if overlapped { "OVERLAP" } else { "SERIAL" }.to_string())
    }
}

/// Completion-order fixture: slow vs fast read-only calls.
struct TimedReadFixture {
    name: String,
    delay_ms: u64,
    completion_order: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for TimedReadFixture {
    fn name(&self) -> &str {
        &self.name
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "timed read fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, _p: Value, _c: ToolContext) -> Result<String> {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        self.completion_order
            .lock()
            .unwrap()
            .push(self.name.clone());
        Ok(format!("done:{}", self.name))
    }
}

async fn runtime_with(tools: Vec<Arc<dyn Tool>>) -> Runtime {
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    {
        let shared = rt.tools_shared();
        let mut registry = shared.write().await;
        for tool in tools {
            registry.register(tool);
        }
    }
    rt
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

fn ordered_tool_results(history: &[synaps_cli::SharedMessage]) -> Vec<(String, String)> {
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

/// Two mutating calls with the SAME canonical key run serially in model
/// order even when the first is slow (reordering pressure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn same_key_writes_execute_serially_in_model_order() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("keyed_write_a", "toolu_w1", "keyed_write_b", "toolu_w2").into_boxed_str(),
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let rt = runtime_with(vec![
        Arc::new(KeyedWriteFixture {
            name: "keyed_write_a".into(),
            tag: "A".into(),
            log: Arc::clone(&log),
            delay_ms: 300,
        }),
        Arc::new(KeyedWriteFixture {
            name: "keyed_write_b".into(),
            tag: "B".into(),
            log: Arc::clone(&log),
            delay_ms: 0,
        }),
    ])
    .await;
    let events = drive_runtime_turn(&rt, "write twice", false).await;

    let observed = log.lock().unwrap().clone();
    assert_eq!(
        observed,
        vec!["start:A", "end:A", "start:B", "end:B"],
        "same-key mutations must serialize in model order"
    );
    let results = ordered_tool_results(&final_history(&events));
    assert_eq!(results[0].0, "toolu_w1");
    assert_eq!(results[1].0, "toolu_w2");
}

/// Independent read-only tools OVERLAP: both sides reach the shared
/// barrier while the other is still executing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn independent_read_only_tools_overlap() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("ro_alpha", "toolu_r1", "ro_beta", "toolu_r2").into_boxed_str(),
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let gauge = Arc::new(ConcurrencyGauge::default());
    let rt = runtime_with(vec![
        Arc::new(OverlapFixture {
            name: "ro_alpha".into(),
            effect: ToolEffect::ReadOnly,
            gauge: Arc::clone(&gauge),
            log: Arc::clone(&log),
        }),
        Arc::new(OverlapFixture {
            name: "ro_beta".into(),
            effect: ToolEffect::ReadOnly,
            gauge: Arc::clone(&gauge),
            log: Arc::clone(&log),
        }),
    ])
    .await;
    let events = drive_runtime_turn(&rt, "read twice", false).await;

    let results = ordered_tool_results(&final_history(&events));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].1, "OVERLAP", "read-only calls may overlap");
    assert_eq!(results[1].1, "OVERLAP", "read-only calls may overlap");
}

/// Unclassified tools (default effect) serialize by default: the barrier
/// is never satisfied concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn unclassified_tools_serialize_by_default() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("unc_alpha", "toolu_u1", "unc_beta", "toolu_u2").into_boxed_str(),
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let gauge = Arc::new(ConcurrencyGauge::default());
    let rt = runtime_with(vec![
        Arc::new(OverlapFixture {
            name: "unc_alpha".into(),
            effect: ToolEffect::NonIdempotent,
            gauge: Arc::clone(&gauge),
            log: Arc::clone(&log),
        }),
        Arc::new(OverlapFixture {
            name: "unc_beta".into(),
            effect: ToolEffect::NonIdempotent,
            gauge: Arc::clone(&gauge),
            log: Arc::clone(&log),
        }),
    ])
    .await;
    let events = drive_runtime_turn(&rt, "unclassified twice", false).await;

    let results = ordered_tool_results(&final_history(&events));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].1, "SERIAL", "unclassified must serialize");
    assert_eq!(results[1].1, "SERIAL", "unclassified must serialize");
    let observed = log.lock().unwrap().clone();
    assert_eq!(
        observed,
        vec![
            "start:unc_alpha",
            "end:unc_alpha",
            "start:unc_beta",
            "end:unc_beta"
        ],
        "no interleaving for unclassified tools"
    );
}

/// tool_result order always matches model request order, even when the
/// second call completes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn result_order_matches_model_order_despite_completion_order() {
    let _guard = HomeGuard::new();
    let body: &'static str = Box::leak(
        sse_two_calls("slow_read", "toolu_s1", "fast_read", "toolu_s2").into_boxed_str(),
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([body, ANTHROPIC_SSE]));
    let (url, _hits, _) = spawn_stub(Script::SeqSse(bodies)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let completion_order = Arc::new(Mutex::new(Vec::new()));
    let rt = runtime_with(vec![
        Arc::new(TimedReadFixture {
            name: "slow_read".into(),
            delay_ms: 400,
            completion_order: Arc::clone(&completion_order),
        }),
        Arc::new(TimedReadFixture {
            name: "fast_read".into(),
            delay_ms: 0,
            completion_order: Arc::clone(&completion_order),
        }),
    ])
    .await;
    let events = drive_runtime_turn(&rt, "race", false).await;

    let completions = completion_order.lock().unwrap().clone();
    assert_eq!(
        completions,
        vec!["fast_read", "slow_read"],
        "completion order genuinely inverted (non-vacuous)"
    );
    let results = ordered_tool_results(&final_history(&events));
    assert_eq!(results[0].0, "toolu_s1", "model order preserved");
    assert_eq!(results[1].0, "toolu_s2");
}

/// Built-in classification + catalog recording + dynamic defaults.
#[tokio::test]
async fn builtin_classification_and_dynamic_defaults() {
    use synaps_cli::tools::{
        BashTool, EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool,
    };
    let write = WriteTool;
    let edit = EditTool;
    assert_eq!(ReadTool.effect(), ToolEffect::ReadOnly);
    assert_eq!(GrepTool.effect(), ToolEffect::ReadOnly);
    assert_eq!(FindTool.effect(), ToolEffect::ReadOnly);
    assert_eq!(LsTool.effect(), ToolEffect::ReadOnly);
    assert_eq!(write.effect(), ToolEffect::IdempotentWrite);
    assert_eq!(edit.effect(), ToolEffect::IdempotentWrite);
    assert_eq!(BashTool.effect(), ToolEffect::NonIdempotent);

    // write/edit concurrency keys: canonical path — same target, same key,
    // regardless of lexical spelling; different targets differ.
    let a = write.concurrency_key(&serde_json::json!({"path": "/tmp/x/../t24/file.txt"}));
    let b = edit.concurrency_key(&serde_json::json!({"path": "/tmp/t24/file.txt"}));
    assert!(a.is_some());
    assert_eq!(a, b, "same canonical target, same key across write/edit");
    let c = write.concurrency_key(&serde_json::json!({"path": "/tmp/t24/other.txt"}));
    assert_ne!(a, c);
    // bash exposes no key (serialized by effect).
    assert_eq!(
        BashTool.concurrency_key(&serde_json::json!({"command": "ls"})),
        None
    );

    // Catalog records the effect; dynamic/unknown-origin tools default to
    // NonIdempotent.
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(WriteTool));
    registry.register(Arc::new(TimedReadFixture {
        name: "cat_ro".into(),
        delay_ms: 0,
        completion_order: Arc::new(Mutex::new(Vec::new())),
    }));
    assert_eq!(
        registry
            .catalog()
            .get(&ToolId::builtin("write"))
            .unwrap()
            .effect(),
        ToolEffect::IdempotentWrite
    );
    assert_eq!(
        registry
            .catalog()
            .get(&ToolId::builtin("cat_ro"))
            .unwrap()
            .effect(),
        ToolEffect::ReadOnly
    );

    // Dormant extension tools (dynamic) stay NonIdempotent by default.
    let manifest: synaps_cli::extensions::manifest::ExtensionManifest =
        serde_json::from_value(serde_json::json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["tools.register"],
            "deferred": {"tools": [{"name": "dyn_tool", "description": "d",
                "input_schema": {"type":"object"}}]}
        }))
        .unwrap();
    let dynamic = synaps_cli::extensions::lifecycle::dormant_extension_tools("plug", &manifest);
    assert_eq!(dynamic[0].effect(), ToolEffect::NonIdempotent);
}
