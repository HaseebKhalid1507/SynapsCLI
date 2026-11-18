//! Shared test helpers for tool unit tests.

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
            session_allow_all: None,
            launch_cancel: None,
            memory_backend: None,
            telemetry: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: Some(std::sync::Arc::new(
                crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                    .expect("test foreground is routable"),
            )),
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            cwd: None,
            env: None,
            env_stripped: Vec::new(),
            env_warned: Default::default(),
            session_id: None,
            hook_bus: None,
            continuation: None,
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

/// The memory binding of a chat that runs in `dir`. Building it writes
/// nothing.
pub(crate) fn memory_binding(dir: &std::path::Path) -> crate::memory_backend::MemoryBinding {
    crate::memory_backend::MemoryBinding::for_cwd(Some(dir.to_path_buf()))
}

/// A tool context carrying `binding` as its memory backend.
pub(crate) fn context_with_memory(binding: crate::memory_backend::MemoryBinding) -> ToolContext {
    let mut ctx = create_tool_context();
    ctx.capabilities.memory_backend = Some(binding);
    ctx
}
