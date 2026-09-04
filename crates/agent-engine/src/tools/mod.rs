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
mod memory_context;
mod powershell;
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
pub use memory_context::MemoryContextTool;
pub use powershell::PowerShellTool;
pub use read::ReadTool;
pub use registry::{DroppedSessionMember, SessionSchemaProjection, ToolRegistry};
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
    /// Session-scoped continuous-memory context capability (task A4, spec
    /// §7.2): shared per-session memory state + host-owned grant scope.
    /// `None` (default for non-stream contexts) means the `memory_context`
    /// builtin can commit nothing — lease-granting actions fail typed,
    /// while `status`/`disable` still answer deterministically `Off`
    /// (memory off requires no infrastructure).
    pub memory_context: Option<crate::runtime::memory_context::MemoryContextCapability>,
    /// Per-session working directory (Phase 2 daemons serve sessions from N
    /// directories in one process). `None` = inherit the process cwd — the
    /// only value Phase 1 ever sets, so behaviour is byte-identical.
    pub cwd: Option<PathBuf>,
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

/// Structured tool output. `Text` is the legacy contract (what `execute`
/// returns). `Blocks` carries Anthropic-shaped content blocks destined for
/// `tool_result.content`, plus a plain-text `summary` used everywhere a
/// `String` is expected today: UI preview, `after_tool_call` hooks, trace
/// ledger byte counts, headless logs, and providers that cannot carry the
/// blocks. INVARIANT: `blocks[0]` MUST be a `{"type":"text"}` block whose
/// `text` == `summary` (compaction reads `blocks[0].text`; non-Anthropic
/// translators join only text sub-blocks).
#[derive(Debug, Clone)]
pub enum ToolOutput {
    Text(String),
    Blocks { blocks: Vec<Value>, summary: String },
}

impl ToolOutput {
    pub fn summary(&self) -> &str {
        match self {
            Self::Text(s) => s,
            Self::Blocks { summary, .. } => summary,
        }
    }

    /// `(summary, Some(blocks))` for rich output, `(text, None)` for plain.
    /// This is the one choke point every rich result passes through, so the
    /// `blocks[0]` text-first invariant is ENFORCED here in every build: a
    /// `Blocks` value whose first block is not `{"type":"text"}` gets a text
    /// block built from `summary` prepended. Debug builds additionally
    /// assert that an existing leading text block matches `summary`.
    pub fn into_parts(self) -> (String, Option<Vec<Value>>) {
        match self {
            Self::Text(s) => (s, None),
            Self::Blocks {
                mut blocks,
                summary,
            } => {
                let text_first = blocks.first().is_some_and(|b| b["type"] == "text");
                if !text_first {
                    blocks.insert(0, serde_json::json!({"type": "text", "text": summary}));
                }
                debug_assert!(
                    blocks[0]["text"] == summary,
                    "ToolOutput::Blocks invariant: blocks[0] text must equal summary"
                );
                (summary, Some(blocks))
            }
        }
    }

    pub fn into_summary(self) -> String {
        self.into_parts().0
    }
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

    /// Structured variant of [`Tool::execute`]. DEFAULTED — existing tools
    /// keep returning `String` via `execute` and never see this. Tools that
    /// need to place non-text content blocks (images) into the model history
    /// override this and make `execute` delegate to it.
    ///
    /// RECURSION TRAP: the default delegates to `execute`. If you make
    /// `execute` delegate here (as `ReadTool` does) you MUST override this
    /// method too, or the pair recurses until the stack blows.
    async fn execute_rich(&self, params: Value, ctx: ToolContext) -> Result<ToolOutput> {
        self.execute(params, ctx).await.map(ToolOutput::Text)
    }

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

#[cfg(test)]
mod tool_output_tests {
    use super::ToolOutput;
    use serde_json::json;

    #[test]
    fn into_parts_prepends_summary_text_when_blocks_not_text_first() {
        let img = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
        let out = ToolOutput::Blocks {
            blocks: vec![img.clone()],
            summary: "[image 1x1]".into(),
        };
        let (summary, blocks) = out.into_parts();
        let blocks = blocks.unwrap();
        assert_eq!(summary, "[image 1x1]");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], json!({"type": "text", "text": "[image 1x1]"}));
        assert_eq!(blocks[1], img);
    }

    #[test]
    fn into_parts_prepends_summary_text_when_blocks_empty() {
        let out = ToolOutput::Blocks {
            blocks: vec![],
            summary: "s".into(),
        };
        let (_, blocks) = out.into_parts();
        assert_eq!(blocks.unwrap(), vec![json!({"type": "text", "text": "s"})]);
    }

    #[test]
    fn into_parts_leaves_well_formed_blocks_alone() {
        let blocks = vec![
            json!({"type": "text", "text": "ok"}),
            json!({"type": "image"}),
        ];
        let out = ToolOutput::Blocks {
            blocks: blocks.clone(),
            summary: "ok".into(),
        };
        let (_, got) = out.into_parts();
        assert_eq!(got.unwrap(), blocks);
    }
}
