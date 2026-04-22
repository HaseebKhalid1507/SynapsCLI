//! Tool system — trait, registry, and built-in tool implementations.
//!
//! All tools implement the `Tool` trait and are registered in `ToolRegistry`.
//! Subagents get `ToolRegistry::without_subagent()` to prevent recursion.
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use crate::Result;

// ── Module declarations ──────────────────────────────────────────────────────────

mod bash;
mod read;
mod write;
mod edit;
mod grep;
mod find;
mod ls;
mod subagent;

pub mod watcher_exit;
pub(crate) mod util;
mod agent;
mod registry;
pub mod shell;
pub mod subagent_start;
pub mod subagent_status;
pub mod subagent_steer;
pub mod subagent_collect;
pub mod subagent_resume;
pub mod respond;
pub mod send_channel;
pub mod tmux_split;
pub mod tmux_send;
pub mod tmux_capture;
pub mod tmux_layout;
pub mod tmux_window;
pub mod tmux_resize;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use bash::BashTool;
pub use read::ReadTool;
pub use write::WriteTool;
pub use edit::EditTool;
pub use grep::GrepTool;
pub use find::FindTool;
pub use ls::LsTool;
pub use subagent::SubagentTool;
pub use crate::runtime::subagent::{SubagentResult, SubagentHandle, SubagentRegistry, SubagentStatus, SubagentState};
pub use watcher_exit::WatcherExitTool;
pub use registry::ToolRegistry;
pub use agent::resolve_agent_prompt;
pub use shell::{ShellStartTool, ShellSendTool, ShellEndTool};
pub use subagent_start::SubagentStartTool;
pub use subagent_status::SubagentStatusTool;
pub use subagent_steer::SubagentSteerTool;
pub use subagent_collect::SubagentCollectTool;
pub use subagent_resume::SubagentResumeTool;
pub use respond::RespondTool;
pub use send_channel::SendChannelTool;
pub use tmux_split::TmuxSplitTool;
pub use tmux_send::TmuxSendTool;
pub use tmux_capture::TmuxCaptureTool;
pub use tmux_layout::TmuxLayoutTool;
pub use tmux_window::TmuxWindowTool;
pub use tmux_resize::TmuxResizeTool;

// Re-export util items used by sibling tool modules via `super::`
pub(crate) use util::{NEXT_SUBAGENT_ID, strip_ansi, expand_path};

// ── Tool Trait ──────────────────────────────────────────────────────────────────

/// Streaming channels — carry partial tool output and stream events.
pub struct ToolChannels {
    pub tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
}

/// Runtime capability handles — shared services a tool may require.
pub struct ToolCapabilities {
    pub watcher_exit_path: Option<PathBuf>,
    pub tool_register_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<Arc<dyn Tool>>>>,
    pub session_manager: Option<std::sync::Arc<crate::tools::shell::SessionManager>>,
    pub subagent_registry: Option<Arc<Mutex<SubagentRegistry>>>,
    pub event_queue: Option<Arc<crate::events::EventQueue>>,
    pub tmux_controller: Option<std::sync::Arc<crate::tmux::TmuxController>>,
}

/// Configuration limits and timeouts.
pub struct ToolLimits {
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub subagent_timeout: u64,
}

/// Context passed to tool execution — composition of channels, capabilities, and limits.
pub struct ToolContext {
    pub channels: ToolChannels,
    pub capabilities: ToolCapabilities,
    pub limits: ToolLimits,
}

/// The core trait for all tools. Implement this to add a new tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name as it appears in the API (e.g. "bash", "read").
    fn name(&self) -> &str;

    /// Human-readable description sent to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters(&self) -> Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}

#[cfg(test)]
mod tests;
