use std::sync::Arc;

use serde_json::{json, Value};
use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::runtime::process::{
    execute_provider_tool_use,
    ProviderToolUse,
};
use synaps_cli::tools::{Tool, ToolContext, ToolRegistry};

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo_test" }
    fn description(&self) -> &str { "echo test tool" }
    fn parameters(&self) -> Value { json!({"type": "object"}) }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        Ok(params["message"].as_str().unwrap_or_default().to_string())
    }
}

struct FailingTool;

#[async_trait::async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str { "fail_test" }
    fn description(&self) -> &str { "failing test tool" }
    fn parameters(&self) -> Value { json!({"type": "object"}) }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        Err(synaps_cli::RuntimeError::Tool("boom".to_string()))
    }
}

struct BlockingHook;

#[async_trait::async_trait]
impl synaps_cli::extensions::runtime::ExtensionHandler for BlockingHook {
    fn id(&self) -> &str { "blocking-hook" }
    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Block { reason: "blocked in test".to_string() }
    }
    async fn shutdown(&self) {}
}

fn test_context() -> ToolContext {
    ToolContext {
        channels: synaps_cli::tools::ToolChannels { tx_delta: None, tx_events: None },
        capabilities: synaps_cli::tools::ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            secret_prompt: None,
        },
        limits: synaps_cli::tools::ToolLimits {
            max_tool_output: 1000,
            bash_timeout: 1,
            bash_max_timeout: 1,
            subagent_timeout: 1,
        },
    }
}

#[test]
fn extracts_anthropic_tool_use_blocks_from_provider_content() {
    let content = vec![
        json!({"type": "text", "text": "checking"}),
        json!({
            "type": "tool_use",
            "id": "call-1",
            "name": "read",
            "input": {"path": "Cargo.toml"}
        }),
    ];

    let tool_uses = synaps_cli::extensions::runtime::process::extract_provider_tool_uses(&content)
        .expect("valid tool use blocks");

    assert_eq!(tool_uses, vec![ProviderToolUse {
        id: "call-1".to_string(),
        name: "read".to_string(),
        input: json!({"path": "Cargo.toml"}),
    }]);
}

#[test]
fn rejects_provider_tool_use_without_required_fields() {
    let content = vec![json!({
        "type": "tool_use",
        "id": "call-1",
        "input": {"path": "Cargo.toml"}
    })];

    let err = synaps_cli::extensions::runtime::process::extract_provider_tool_uses(&content)
        .unwrap_err();

    assert!(err.contains("missing name"));
}

#[tokio::test]
async fn executes_provider_requested_tool_through_synaps_registry() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(EchoTool));
    let hook_bus = Arc::new(HookBus::new());
    let tool_use = ProviderToolUse {
        id: "call-1".to_string(),
        name: "echo_test".to_string(),
        input: json!({"message": "hello"}),
    };

    let result = execute_provider_tool_use(
        &registry,
        &hook_bus,
        tool_use,
        test_context(),
        1000,
    ).await;

    assert_eq!(result, json!({
        "type": "tool_result",
        "tool_use_id": "call-1",
        "content": "hello"
    }));
}

#[tokio::test]
async fn provider_requested_tool_execution_failure_is_marked_as_error() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(FailingTool));
    let hook_bus = Arc::new(HookBus::new());
    let tool_use = ProviderToolUse {
        id: "call-1".to_string(),
        name: "fail_test".to_string(),
        input: json!({}),
    };

    let result = execute_provider_tool_use(
        &registry,
        &hook_bus,
        tool_use,
        test_context(),
        1000,
    ).await;

    assert_eq!(result["tool_use_id"], "call-1");
    assert_eq!(result["is_error"], true);
    assert!(result["content"].as_str().unwrap().contains("Tool execution failed"));
    assert!(result["content"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn provider_requested_tool_calls_are_blocked_by_hooks() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(EchoTool));
    let hook_bus = Arc::new(HookBus::new());
    hook_bus.subscribe(
        synaps_cli::extensions::hooks::events::HookKind::BeforeToolCall,
        Arc::new(BlockingHook),
        Some("echo_test".to_string()),
        None,
        synaps_cli::extensions::permissions::PermissionSet::from_strings(&["tools.intercept".to_string()]),
    ).await.unwrap();
    let tool_use = ProviderToolUse {
        id: "call-1".to_string(),
        name: "echo_test".to_string(),
        input: json!({"message": "hello"}),
    };

    let result = execute_provider_tool_use(
        &registry,
        &hook_bus,
        tool_use,
        test_context(),
        1000,
    ).await;

    assert_eq!(result["tool_use_id"], "call-1");
    assert_eq!(result["is_error"], true);
    assert!(result["content"].as_str().unwrap().contains("blocked in test"));
}
