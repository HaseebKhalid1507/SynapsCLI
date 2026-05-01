use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use synaps_cli::extensions::runtime::process::{
    complete_provider_with_tools,
    ProviderCompleteParams,
    ProviderCompleteResult,
};
use synaps_cli::extensions::runtime::{ExtensionHandler, ExtensionHealth};
use synaps_cli::tools::{Tool, ToolContext, ToolRegistry};

struct ToolThenTextProvider;

#[async_trait]
impl ExtensionHandler for ToolThenTextProvider {
    fn id(&self) -> &str { "provider" }

    async fn provider_complete(&self, params: ProviderCompleteParams) -> Result<ProviderCompleteResult, String> {
        let has_tool_result = params.messages.iter().any(|message| {
            message.get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| blocks.iter().any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")))
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

    async fn handle(&self, _event: &synaps_cli::extensions::hooks::events::HookEvent) -> synaps_cli::extensions::hooks::events::HookResult {
        synaps_cli::extensions::hooks::events::HookResult::Continue
    }

    async fn shutdown(&self) {}

    async fn health(&self) -> ExtensionHealth { ExtensionHealth::Healthy }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo_test" }
    fn description(&self) -> &str { "echo test" }
    fn parameters(&self) -> Value { json!({"type": "object"}) }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        Ok(params["message"].as_str().unwrap_or_default().to_string())
    }
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

#[tokio::test]
async fn provider_tool_loop_returns_final_text_after_tool_result_turn() {
    let mut registry = ToolRegistry::empty();
    registry.register(Arc::new(EchoTool));
    let handler: Arc<dyn ExtensionHandler> = Arc::new(ToolThenTextProvider);
    let params = ProviderCompleteParams {
        provider_id: "p".to_string(),
        model_id: "m".to_string(),
        model: "plugin:p:m".to_string(),
        messages: vec![json!({"role": "user", "content": "use a tool"})],
        system_prompt: None,
        tools: registry.tools_schema().as_ref().clone(),
        temperature: None,
        max_tokens: None,
        thinking_budget: 0,
    };

    let result = complete_provider_with_tools(
        handler,
        params,
        &registry,
        &Arc::new(synaps_cli::extensions::hooks::HookBus::new()),
        || test_context(),
        1000,
        4,
    ).await.expect("provider loop succeeds");

    assert_eq!(result.content, vec![json!({"type": "text", "text": "done"})]);
}
