//! Shared test helpers for tool unit tests.
#![cfg(test)]

use super::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

pub(crate) fn create_tool_context() -> ToolContext {
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
            secret_prompt: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}
