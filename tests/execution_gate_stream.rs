//! Task 16 — `ExecutionGate` wired into the stream tool loop (spec §7.1).
//!
//! End-to-end through the real `Runtime` stream loop against a loopback
//! Anthropic stub:
//! - a forged call to a registered-but-unverified (non-core) tool is denied
//!   BEFORE implementation execution and BEFORE `before_tool_call` hook
//!   emission, with a typed static tool_result;
//! - an unknown wire name still yields the bounded typed denial;
//! - default registered core tools (e.g. `ls`) still execute — behavior
//!   preservation for the default core set;
//! - tool-loop result ordering is preserved when denials and successes mix.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serial_test::serial;
use support::*;
use synaps_cli::extensions::hooks::events::HookKind;
use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
use synaps_cli::extensions::permissions::PermissionSet;
use synaps_cli::extensions::runtime::ExtensionHandler;
use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin};
use synaps_cli::{Result, Value};

// ── SSE fixtures ────────────────────────────────────────────────────────────

/// Turn requesting the forged non-core fixture tool.
const SSE_FORGED_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_forged\",\"name\":\"gate_forged_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Turn requesting a wire name no live tool has.
const SSE_UNKNOWN_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_unknown\",\"name\":\"totally_unknown_tool\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Parallel round: core `ls` first, forged fixture second — ordering of the
/// tool_result blocks must match the tool_use order.
const SSE_MIXED_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g3\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_ls\",\"name\":\"ls\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_forged2\",\"name\":\"gate_forged_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A registered tool with NO verifiable origin (`ToolOrigin::Unknown`) —
/// cataloged, live, but excluded from the default core set. Executing it
/// flips the flag, which the gate must prevent.
struct ForgedFixtureTool {
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for ForgedFixtureTool {
    fn name(&self) -> &str {
        "gate_forged_fixture"
    }
    fn description(&self) -> &str {
        "deliberately non-core fixture tool"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Unknown
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        self.executed.store(true, Ordering::SeqCst);
        Ok("MUST_NEVER_RUN".to_string())
    }
}

/// Hook spy: records every tool name seen by `before_tool_call`.
struct HookSpy {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExtensionHandler for HookSpy {
    fn id(&self) -> &str {
        "gate-hook-spy"
    }
    async fn handle(&self, event: &HookEvent) -> HookResult {
        if let Some(name) = &event.tool_name {
            self.seen.lock().unwrap().push(name.clone());
        }
        HookResult::Continue
    }
    async fn shutdown(&self) {}
}

async fn install_hook_spy(rt: &Runtime) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    rt.hook_bus()
        .subscribe(
            HookKind::BeforeToolCall,
            Arc::new(HookSpy {
                seen: Arc::clone(&seen),
            }),
            None,
            None,
            PermissionSet::from_strings(&["tools.intercept".to_string()]),
        )
        .await
        .expect("spy subscription");
    seen
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

fn tool_results(history: &[synaps_cli::SharedMessage]) -> Vec<(String, String)> {
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

// ── Scenarios ───────────────────────────────────────────────────────────────

/// Forged known-but-non-core call: typed denial BEFORE implementation
/// execution and BEFORE before_tool_call hook emission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn forged_non_core_tool_denied_before_execution_and_hooks() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[SSE_FORGED_TOOL_USE, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    let executed = Arc::new(AtomicBool::new(false));
    rt.tools_shared()
        .write()
        .await
        .register(Arc::new(ForgedFixtureTool {
            executed: Arc::clone(&executed),
        }));
    let seen_hooks = install_hook_spy(&rt).await;

    let ev = drive_runtime_turn(&rt, "forged call fixture", false).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "denial still continues the loop"
    );

    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "toolu_forged");
    assert!(
        results[0].1.contains("Tool call denied"),
        "denial must be the typed static gate error, got: {}",
        results[0].1
    );
    assert!(
        !results[0].1.contains("MUST_NEVER_RUN"),
        "implementation output must never appear"
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "forged call must be denied BEFORE implementation execution"
    );
    assert!(
        seen_hooks.lock().unwrap().is_empty(),
        "denial must happen BEFORE before_tool_call hook emission, saw: {:?}",
        seen_hooks.lock().unwrap()
    );
}

/// Unknown wire names stay bounded typed denials.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn unknown_wire_name_yields_bounded_typed_denial() {
    let _guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::SeqSse(&[SSE_UNKNOWN_TOOL_USE, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    let seen_hooks = install_hook_spy(&rt).await;

    let ev = drive_runtime_turn(&rt, "unknown tool fixture", false).await;
    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "toolu_unknown");
    assert_eq!(results[0].1, "Unknown tool: totally_unknown_tool");
    assert!(seen_hooks.lock().unwrap().is_empty());
}

/// Default registered core tool still executes (behavior preservation) and
/// its before_tool_call hook still fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn default_core_tool_still_executes_through_gate() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[ANTHROPIC_SSE_TOOL_USE, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    let seen_hooks = install_hook_spy(&rt).await;

    let ev = drive_runtime_turn(&rt, "core tool fixture", false).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "toolu_ph2");
    assert!(
        !results[0].1.contains("denied") && !results[0].1.contains("Unknown tool"),
        "core tool must execute, got: {}",
        results[0].1
    );
    assert_eq!(
        seen_hooks.lock().unwrap().as_slice(),
        ["ls"],
        "authorized core tool still emits before_tool_call"
    );
}

/// Mixed parallel round: denial and success keep the tool_use ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mixed_round_preserves_result_ordering() {
    let _guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::SeqSse(&[SSE_MIXED_TOOL_USE, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    let executed = Arc::new(AtomicBool::new(false));
    rt.tools_shared()
        .write()
        .await
        .register(Arc::new(ForgedFixtureTool {
            executed: Arc::clone(&executed),
        }));

    let ev = drive_runtime_turn(&rt, "mixed round fixture", false).await;
    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 2, "one result per tool_use");
    assert_eq!(results[0].0, "toolu_ls", "ordering follows tool_use order");
    assert_eq!(results[1].0, "toolu_forged2");
    assert!(results[1].1.contains("Tool call denied"));
    assert!(!executed.load(Ordering::SeqCst));
}
