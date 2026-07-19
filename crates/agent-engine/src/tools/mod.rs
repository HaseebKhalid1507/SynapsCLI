//! Tool system — trait, registry, and built-in tool implementations.
//!
//! All tools implement the `Tool` trait and are registered in `ToolRegistry`.
//! Subagents get `ToolRegistry::without_subagent()` to prevent recursion.
use crate::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ── Module declarations ──────────────────────────────────────────────────────────

mod bash;
mod edit;
mod extension;
mod find;
mod grep;
mod ls;
pub mod memory;
mod read;
mod secret_prompt;
mod subagent;
mod write;

pub mod activation;
mod agent;
pub mod catalog;
pub mod discovery;
pub mod ledger;
pub mod output;
mod registry;
pub mod respond;
pub mod send_channel;
pub mod shell;
pub(crate) mod util;
pub mod watcher_exit;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use crate::runtime::subagent::{
    SubagentDisplayRow, SubagentHandle, SubagentRegistry, SubagentResult, SubagentState,
    SubagentStatus,
};
pub use agent::resolve_agent_prompt;
pub use bash::{bash_intermediary_snapshot, BashIntermediarySnapshot, BashTool};
pub use discovery::{ActivateToolsTool, SearchToolsTool};
pub use edit::EditTool;
pub use extension::ExtensionTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use registry::ToolRegistry;
pub use respond::RespondTool;
pub use secret_prompt::SecretPromptQueue;
pub use secret_prompt::{SecretPromptHandle, SecretPromptRequest};
pub use send_channel::SendChannelTool;
pub use shell::{ShellEndTool, ShellSendTool, ShellStartTool};
pub use subagent::{
    SubagentCollectTool, SubagentModelAuthorizeTool, SubagentModelsTool, SubagentResumeTool,
    SubagentStartTool, SubagentStatusTool, SubagentSteerTool, SubagentTool,
};
pub use watcher_exit::WatcherExitTool;
pub use write::WriteTool;

// Re-export util items used by sibling tool modules via `super::`
pub(crate) use util::{expand_path, strip_ansi, NEXT_SUBAGENT_ID};

// Facade: expose finalize internals for integration tests without making the
// subagent module pub. Tests import `agent_engine::tools::{build_completion_event, finalize_subagent}`.
#[doc(hidden)]
pub use subagent::finalize::{build_completion_event, finalize_subagent};

// ── Tool Trait ──────────────────────────────────────────────────────────────────

/// Streaming channels — carry partial tool output and stream events.
///
/// `tx_delta` is the Task 26 bounded delta lane (spec §8.4): a non-blocking
/// coalesce-then-drop producer handle whose consumer enforces the UI-preview
/// byte budget. It replaced the previous unbounded `mpsc` sender so a fast
/// producer with a slow consumer can no longer grow memory without bound.
pub struct ToolChannels {
    pub tx_delta: Option<crate::tools::output::DeltaSender>,
    pub tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
}

/// Runtime capability handles — shared services a tool may require.
pub struct ToolCapabilities {
    pub watcher_exit_path: Option<PathBuf>,
    pub tool_register_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<Arc<dyn Tool>>>>,
    pub session_manager: Option<std::sync::Arc<crate::tools::shell::SessionManager>>,
    pub subagent_registry: Option<Arc<Mutex<SubagentRegistry>>>,
    pub event_queue: Option<Arc<crate::events::EventQueue>>,
    /// Current worker handle when this context belongs to a delegated
    /// runtime. `None` denotes the foreground root.
    pub delegation_parent: Option<String>,
    pub secret_prompt: Option<SecretPromptHandle>,
    /// Runtime-enforced delegation/lifecycle policy. When present, every spawn
    /// path must authorize before creating channels, threads, or provider runtimes.
    pub orchestration: Option<Arc<crate::orchestration::OrchestrationRuntime>>,
    /// Discovery/activation capability context (Task 17): passive catalog
    /// snapshot + retained session-set handle + host-supplied activation
    /// authority. `None` (default for manual fixtures and non-stream
    /// contexts) means the discovery/activation builtins fail typed and no
    /// model-initiated activation is possible.
    pub tool_activation: Option<crate::tools::discovery::ActivationCapability>,
    /// Session-scoped exact MCP lease capability (Task 19): exact session
    /// identity + shared runtime manager. `None` (default for non-stream
    /// contexts) means deferred MCP tools fail typed and start nothing.
    pub mcp_leases: Option<crate::mcp::McpLeaseCapability>,
    /// Session-scoped exact EXTENSION lease capability (Task 20): exact
    /// session identity + shared extension runtime manager. `None`
    /// (default for non-stream contexts) means deferred extension tools
    /// fail typed and start nothing.
    pub extension_leases: Option<crate::extensions::lease::ExtensionLeaseCapability>,
}

/// Configuration limits and timeouts.
pub struct ToolLimits {
    /// Context-budget ceiling — final tool output entering history is truncated
    /// to this. Applied AFTER the `after_tool_call` hook so compression
    /// extensions see the FULL buffered output and can decide what to keep.
    pub max_tool_output: usize,
    /// Memory/pipe safety bound — the maximum bytes a tool may buffer
    /// internally before truncating with a marker. Sized well above
    /// `max_tool_output` so transform extensions are never starved.
    pub max_tool_buffer: usize,
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

/// Explicit runtime-origin identity of a registered tool implementation.
///
/// This is the metadata boundary the capability catalog trusts for
/// provenance. The default is conservative: a tool that declares nothing is
/// [`ToolOrigin::Unknown`] (or [`ToolOrigin::Extension`] when it already
/// declares an owning extension via [`Tool::extension_id`]) — never builtin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOrigin {
    /// Compiled into this binary.
    Builtin,
    /// Registered by a locally installed extension (`<extension_id>:<tool>`
    /// runtime names).
    Extension { extension_id: String },
    /// Bridged from an MCP server; identifies the server and the tool name
    /// as the server knows it (not the prefixed runtime name).
    Mcp {
        server_id: String,
        server_tool_name: String,
    },
    /// Declared by a plugin definition.
    Plugin {
        plugin_id: String,
        tool_name: String,
    },
    /// No declared origin. Cataloged conservatively, never as builtin.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConcurrencyKey {
    Key(String),
    Serialize,
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

    /// Owning extension id for tools registered by an extension. Built-in tools return `None`.
    fn extension_id(&self) -> Option<&str> {
        None
    }

    /// Runtime-origin identity used for catalog provenance. Conservative by
    /// default: tools that declare nothing are `Unknown` (fail-closed source
    /// trust), and tools that declare an owning extension classify as
    /// extension-provided. Built-in implementations override this explicitly.
    fn origin(&self) -> ToolOrigin {
        match self.extension_id() {
            Some(extension_id) => ToolOrigin::Extension {
                extension_id: extension_id.to_string(),
            },
            None => ToolOrigin::Unknown,
        }
    }

    /// Conservative effect class (Task 24, spec §8.2). The default keeps
    /// unknown/dynamic tools `NonIdempotent` — serialized execution;
    /// implementations opt IN to weaker classes explicitly.
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::NonIdempotent
    }

    /// Key resolution derived from validated input. `Serialize` explicitly
    /// fails closed into the global mutation lane; `None` is the conservative
    /// default for tools with no key support.
    fn concurrency_key(&self, _validated_input: &Value) -> Option<ConcurrencyKey> {
        None
    }
}

#[cfg(test)]
mod test_helpers;
