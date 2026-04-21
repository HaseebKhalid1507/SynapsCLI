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

// Re-export util items used by sibling tool modules via `super::`
pub(crate) use util::{NEXT_SUBAGENT_ID, strip_ansi, expand_path};

// ── Tool Trait ──────────────────────────────────────────────────────────────────

/// Context passed to tool execution — channels for streaming output and events.
pub struct ToolContext {
    pub tx_delta: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
    pub watcher_exit_path: Option<PathBuf>,
    /// Channel for tools that need to register new tools at runtime (e.g. MCP).
    /// Breaks the circular Arc — tools send registrations, runtime applies them.
    pub tool_register_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<Arc<dyn Tool>>>>,
    pub session_manager: Option<std::sync::Arc<crate::tools::shell::SessionManager>>,
    /// Shared registry of reactive subagent handles — populated by SubagentStartTool,
    /// read by SubagentStatusTool, and mutated by SubagentSteerTool / SubagentCollectTool.
    /// `None` in contexts that don't support reactive subagents (e.g. subagent runtimes).
    pub subagent_registry: Option<Arc<Mutex<SubagentRegistry>>>,
    /// Event queue for tools that need to interact with the event bus.
    /// `None` in contexts without an event bus (e.g. subagent runtimes, tests).
    pub event_queue: Option<Arc<crate::events::EventQueue>>,
    // Configuration parameters
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub subagent_timeout: u64,
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
