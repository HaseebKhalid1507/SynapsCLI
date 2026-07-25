use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use synaps_cli::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::permissions::PermissionSet;
use synaps_cli::extensions::runtime::process::{
    complete_provider_with_tools, ProviderCompleteParams, ProviderCompleteResult,
};
use synaps_cli::extensions::runtime::{ExtensionHandler, ExtensionHealth};
use synaps_cli::tools::activation::{SessionId, SessionToolSet};
use synaps_cli::tools::{Tool, ToolContext, ToolOrigin, ToolRegistry};

struct ToolThenTextProvider;

#[async_trait]
impl ExtensionHandler for ToolThenTextProvider {
    fn id(&self) -> &str {
        "provider"
    }

    async fn provider_complete(
        &self,
        params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        let has_tool_result = params.messages.iter().any(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                })
        });
        if has_tool_result {
            Ok(ProviderCompleteResult {
                content: vec![json!({"type": "text", "text": "done"})],
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            })
        } else {
            Ok(ProviderCompleteResult {
                content: vec![json!({
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "echo_test",
                    "input": {"message": "hello"}
                })],
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            })
        }
    }

    async fn handle(
        &self,
        _event: &synaps_cli::extensions::hooks::events::HookEvent,
    ) -> synaps_cli::extensions::hooks::events::HookResult {
        synaps_cli::extensions::hooks::events::HookResult::Continue
    }

    async fn shutdown(&self) {}

    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Running
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo_test"
    }
    fn description(&self) -> &str {
        "echo test"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    /// Verified-core fixture: builtin origin so the Task 16 execution gate
    /// classifies it as verified default core (the success path under test).
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        Ok(params["message"].as_str().unwrap_or_default().to_string())
    }
}

fn test_context() -> ToolContext {
    ToolContext {
        channels: synaps_cli::tools::ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: synaps_cli::tools::ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
        },
        limits: synaps_cli::tools::ToolLimits {
            max_tool_output: 1000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 1,
            bash_max_timeout: 1,
            subagent_timeout: 1,
        },
    }
}

/// Default per-session set: exactly the currently verified cataloged tools
/// as core, zero activations — what the runtime threads into this loop.
fn session_set_for(registry: &ToolRegistry) -> SessionToolSet {
    SessionToolSet::default_core_for_catalog(
        SessionId::parse("provider-loop-test-session").expect("valid session id"),
        registry.catalog(),
    )
}

#[tokio::test]
async fn provider_tool_loop_returns_final_text_after_tool_result_turn() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(EchoTool));
    let session_tools = session_set_for(&registry);
    let handler: Arc<dyn ExtensionHandler> = Arc::new(ToolThenTextProvider);
    let params = ProviderCompleteParams {
        provider_id: "p".to_string(),
        model_id: "m".to_string(),
        model: "plugin:p:m".to_string(),
        messages: vec![std::sync::Arc::new(
            json!({"role": "user", "content": "use a tool"}),
        )],
        system_prompt: None,
        tools: registry.tools_schema().as_ref().clone(),
        temperature: None,
        max_tokens: None,
        thinking_budget: 0,
    };

    let mut tools_requested: u32 = 0;
    let result = complete_provider_with_tools(
        handler,
        params,
        &registry,
        &session_tools,
        &Arc::new(synaps_cli::extensions::hooks::HookBus::new()),
        test_context,
        1000,
        4,
        &mut tools_requested,
    )
    .await
    .expect("provider loop succeeds");

    assert_eq!(
        result.content,
        vec![json!({"type": "text", "text": "done"})]
    );
    // Honest audit metric: this provider requested exactly one tool before
    // its final text turn, so the count must be 1 — not the hardcoded 0 the
    // audit record used to report for every tool-loop turn.
    assert_eq!(
        tools_requested, 1,
        "the tool-use the provider actually requested must be counted"
    );
}

// ── Task 16 review fix: the interior extension-provider tool loop is gated ──

/// A registered-but-unverified tool (`ToolOrigin::Unknown` → `Unverified`
/// provenance): live in the registry, excluded from the default core set.
/// Executing it flips the flag, which the gate must prevent.
struct ShadowTool {
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for ShadowTool {
    fn name(&self) -> &str {
        "shadow_fixture_tool"
    }
    fn description(&self) -> &str {
        "unverified fixture tool"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Unknown
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        self.executed.store(true, Ordering::SeqCst);
        Ok("MUST_NEVER_RUN".to_string())
    }
}

/// Provider (fixture extension handler) that requests the unverified tool
/// on the first interior round and records the tool_result it is handed
/// back on the second.
struct ShadowRequestingProvider {
    seen_result: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl ExtensionHandler for ShadowRequestingProvider {
    fn id(&self) -> &str {
        "shadow-provider"
    }

    async fn provider_complete(
        &self,
        params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        let tool_result = params.messages.iter().find_map(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .and_then(|blocks| {
                    blocks
                        .iter()
                        .find(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                        .cloned()
                })
        });
        if let Some(result) = tool_result {
            *self.seen_result.lock().unwrap() = Some(result);
            Ok(ProviderCompleteResult {
                content: vec![json!({"type": "text", "text": "done"})],
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            })
        } else {
            Ok(ProviderCompleteResult {
                content: vec![json!({
                    "type": "tool_use",
                    "id": "shadow-call-1",
                    "name": "shadow_fixture_tool",
                    "input": {}
                })],
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            })
        }
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }
    async fn shutdown(&self) {}
    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Running
    }
}

/// Hook spy: records every tool name seen by `before_tool_call`.
struct HookSpy {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExtensionHandler for HookSpy {
    fn id(&self) -> &str {
        "loop-hook-spy"
    }
    async fn handle(&self, event: &HookEvent) -> HookResult {
        if let Some(name) = &event.tool_name {
            self.seen.lock().unwrap().push(name.clone());
        }
        HookResult::Continue
    }
    async fn shutdown(&self) {}
}

/// The extension-provider interior tool loop must pass the same execution
/// gate as stream dispatch: a registered-but-unverified tool requested by
/// the provider is denied with a typed, bounded, `is_error` tool_result —
/// the implementation never executes and `before_tool_call` never fires.
#[tokio::test]
async fn provider_loop_denies_unverified_tool_before_hooks_and_execution() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(EchoTool));
    let executed = Arc::new(AtomicBool::new(false));
    registry.register(Arc::new(ShadowTool {
        executed: Arc::clone(&executed),
    }));
    let session_tools = session_set_for(&registry);

    let hook_bus = Arc::new(HookBus::new());
    let seen_hooks = Arc::new(Mutex::new(Vec::new()));
    hook_bus
        .subscribe(
            HookKind::BeforeToolCall,
            Arc::new(HookSpy {
                seen: Arc::clone(&seen_hooks),
            }),
            None,
            None,
            PermissionSet::from_strings(&["tools.intercept".to_string()]),
        )
        .await
        .expect("spy subscription");

    let seen_result = Arc::new(Mutex::new(None));
    let handler: Arc<dyn ExtensionHandler> = Arc::new(ShadowRequestingProvider {
        seen_result: Arc::clone(&seen_result),
    });
    let params = ProviderCompleteParams {
        provider_id: "p".to_string(),
        model_id: "m".to_string(),
        model: "plugin:p:m".to_string(),
        messages: vec![std::sync::Arc::new(
            json!({"role": "user", "content": "use the shadow tool"}),
        )],
        system_prompt: None,
        tools: registry.tools_schema().as_ref().clone(),
        temperature: None,
        max_tokens: None,
        thinking_budget: 0,
    };

    let mut tools_requested: u32 = 0;
    let result = complete_provider_with_tools(
        handler,
        params,
        &registry,
        &session_tools,
        &hook_bus,
        test_context,
        1000,
        4,
        &mut tools_requested,
    )
    .await
    .expect("loop terminates with the final text turn");

    assert_eq!(
        result.content,
        vec![json!({"type": "text", "text": "done"})]
    );
    let denial = seen_result
        .lock()
        .unwrap()
        .clone()
        .expect("provider saw a tool_result");
    assert_eq!(denial["tool_use_id"], "shadow-call-1");
    assert_eq!(denial["is_error"], json!(true), "denial must set is_error");
    let content = denial["content"].as_str().unwrap_or_default().to_string();
    assert!(
        content.contains("Tool call denied"),
        "denial must be the typed static gate error, got: {content}"
    );
    assert!(
        !content.contains("MUST_NEVER_RUN"),
        "implementation output must never appear"
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "unverified tool must be denied BEFORE implementation execution"
    );
    assert!(
        seen_hooks.lock().unwrap().is_empty(),
        "denial must happen BEFORE before_tool_call hook emission, saw: {:?}",
        seen_hooks.lock().unwrap()
    );
}
