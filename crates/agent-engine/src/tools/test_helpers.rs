//! Shared test helpers for tool unit tests.
#![cfg(test)]

use super::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

pub(crate) fn create_tool_context() -> ToolContext {
    let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6")
        .expect("test foreground is qualified");
    ToolContext {
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
            secret_prompt: None,
            orchestration: Some(std::sync::Arc::new(
                crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                    .expect("test foreground is routable"),
            )),
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            memory_context: None,
            cwd: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}
