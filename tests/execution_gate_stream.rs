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

// ── Retained per-session set semantics (Task 16 review fixes) ───────────────

/// Round 1: TWO parallel `ls` calls. Round 2: one `ls`. Round 3: final text.
const SSE_PARALLEL_LS_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g4\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_s1\",\"name\":\"ls\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_s2\",\"name\":\"ls\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Round 1 (parallel): the dynamic-registration fixture AND the tool it
/// registers, in the SAME model response.
const SSE_DYNREG_ROUND: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g5\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_d1\",\"name\":\"gate_dyn_reg_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_d2\",\"name\":\"gate_late_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Round 2: the freshly registered tool alone.
const SSE_LATE_TOOL_ROUND: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_g6\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_d3\",\"name\":\"gate_late_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Builtin-origin fixture registered mid-round by the `before_message`
/// mutator: its registration bumps the catalog generation.
struct MidRoundRegisteredTool;

#[async_trait]
impl Tool for MidRoundRegisteredTool {
    fn name(&self) -> &str {
        "gate_mid_round_fixture"
    }
    fn description(&self) -> &str {
        "registered mid-round to bump the catalog generation"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("MID_OK".to_string())
    }
}

/// `before_message` handler that mutates the live registry exactly once —
/// AFTER the round-top session-set rebuild, BEFORE tool authorization.
struct RegistryMutator {
    tools: Arc<tokio::sync::RwLock<synaps_cli::ToolRegistry>>,
    fired: Arc<AtomicBool>,
}

#[async_trait]
impl ExtensionHandler for RegistryMutator {
    fn id(&self) -> &str {
        "gate-registry-mutator"
    }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        if !self.fired.swap(true, Ordering::SeqCst) {
            self.tools
                .write()
                .await
                .register(Arc::new(MidRoundRegisteredTool));
        }
        HookResult::Continue
    }
    async fn shutdown(&self) {}
}

/// Builtin-origin fixture that dynamically registers `gate_late_fixture`
/// through the stream's `tool_register_tx` channel (the `connect_mcp_server`
/// seam), which is drained only AFTER the round completes.
struct DynRegFixtureTool;

#[async_trait]
impl Tool for DynRegFixtureTool {
    fn name(&self) -> &str {
        "gate_dyn_reg_fixture"
    }
    fn description(&self) -> &str {
        "registers gate_late_fixture dynamically"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, ctx: ToolContext) -> Result<String> {
        if let Some(tx) = &ctx.capabilities.tool_register_tx {
            let _ = tx.send(vec![Arc::new(LateFixtureTool) as Arc<dyn Tool>]);
        }
        Ok("REGISTERED".to_string())
    }
}

/// The verified (builtin-origin) tool registered dynamically mid-turn.
struct LateFixtureTool;

#[async_trait]
impl Tool for LateFixtureTool {
    fn name(&self) -> &str {
        "gate_late_fixture"
    }
    fn description(&self) -> &str {
        "late dynamically registered fixture"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Ok("LATE_OK".to_string())
    }
}

/// A catalog mutation AFTER the round-top rebuild makes the retained
/// session set stale: BOTH sibling calls of the same response are denied
/// with the IDENTICAL stale-generation message (one set snapshot, one
/// generation — never per-call silent refresh), no `before_tool_call` hook
/// fires for them, and the NEXT round's explicit rebuild recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mid_round_catalog_mutation_denies_stale_until_next_round_rebuild() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[
        SSE_PARALLEL_LS_TOOL_USE,
        ANTHROPIC_SSE_TOOL_USE,
        ANTHROPIC_SSE,
    ]))
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    let seen_hooks = install_hook_spy(&rt).await;
    rt.hook_bus()
        .subscribe(
            HookKind::BeforeMessage,
            Arc::new(RegistryMutator {
                tools: rt.tools_shared(),
                fired: Arc::new(AtomicBool::new(false)),
            }),
            None,
            None,
            PermissionSet::from_strings(&["privacy.llm_content".to_string()]),
        )
        .await
        .expect("mutator subscription");

    let ev = drive_runtime_turn(&rt, "stale round fixture", false).await;
    assert_eq!(hits.load(Ordering::SeqCst), 3, "denial continues the loop");

    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 3, "two round-1 denials plus one round-2 run");
    assert_eq!(results[0].0, "toolu_s1");
    assert_eq!(results[1].0, "toolu_s2");
    assert!(
        results[0].1.contains("stale"),
        "post-rebuild catalog mutation must deny stale, got: {}",
        results[0].1
    );
    assert_eq!(
        results[0].1, results[1].1,
        "sibling calls of one response must be judged against ONE set \
         snapshot at ONE generation"
    );
    assert_eq!(results[2].0, "toolu_ph2");
    assert!(
        !results[2].1.contains("denied") && !results[2].1.contains("stale"),
        "next-round explicit rebuild must recover, got: {}",
        results[2].1
    );
    assert_eq!(
        seen_hooks.lock().unwrap().as_slice(),
        ["ls"],
        "stale denials must not emit before_tool_call; only the recovered \
         round-2 call may"
    );
}

/// Stream dynamic registrations are drained only after a round: a sibling
/// call to the not-yet-drained tool in the SAME response stays a typed
/// Unknown denial, and the NEXT round's explicit rebuild exposes the newly
/// registered verified tool as default core (no inherited activations).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn drained_registration_executes_only_after_next_round_rebuild() {
    let _guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::SeqSse(&[
        SSE_DYNREG_ROUND,
        SSE_LATE_TOOL_ROUND,
        ANTHROPIC_SSE,
    ]))
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.tools_shared()
        .write()
        .await
        .register(Arc::new(DynRegFixtureTool));

    let ev = drive_runtime_turn(&rt, "dyn reg fixture", false).await;
    let results = tool_results(&final_history(&ev));
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, "toolu_d1");
    assert_eq!(results[0].1, "REGISTERED");
    assert_eq!(results[1].0, "toolu_d2");
    assert_eq!(
        results[1].1, "Unknown tool: gate_late_fixture",
        "not-yet-drained registration must stay unknown within the round"
    );
    assert_eq!(results[2].0, "toolu_d3");
    assert_eq!(
        results[2].1, "LATE_OK",
        "next-round rebuild must expose the drained verified tool as \
         default core"
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
