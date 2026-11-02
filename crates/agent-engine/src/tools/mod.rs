//! Tool system — trait, registry, and built-in tool implementations.
//!
//! All tools implement the `Tool` trait and are registered in `ToolRegistry`.
//! Subagents get `ToolRegistry::without_subagent()` to prevent recursion.
use crate::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ── Module declarations ──────────────────────────────────────────────────────────

#[cfg(feature = "command-exec")]
mod bash;
mod confirm_latch;
pub(crate) mod context_checkpoint;
mod edit;
pub(crate) mod extension;
mod find;
pub mod forum;
mod grep;
mod ls;
pub mod memory;
mod memory_context;
mod multi_edit;
#[cfg(feature = "command-exec")]
mod powershell;
pub(crate) mod read;
mod unix_programs;
mod secret_prompt;
mod subagent;
#[cfg(target_os = "linux")]
pub mod sudo_askpass;
pub(crate) use subagent::archive::write_window_transcript;
#[cfg(test)]
pub(crate) use subagent::archive::resume_messages;
pub(crate) use subagent::finalize::{
    build_reload_carry_event, build_reload_left_event, build_reload_resumed_event,
};
/// The worker-record writer table, for a daemon reload's check
/// (`docs/specs/subagent-context-rollover.md` §5).
pub(crate) mod worker_records {
    pub(crate) use super::subagent::record_store::{
        mark_waived, record_waived, writers_snapshot, WriterEntry, WriterVerdict,
    };
    #[cfg(any(test, feature = "testing"))]
    pub(crate) use super::subagent::checkpoint::{PendingOp, PublicationState};
    #[cfg(test)]
    pub(crate) use super::subagent::record::RunId;
    #[cfg(feature = "testing")]
    pub(crate) use super::subagent::record_store::TestWriter;
}
#[doc(hidden)]
pub use subagent::{legacy_fresh_runtime, spawn_runtime};
mod write;

pub mod activation;
mod agent;
pub mod catalog;
pub mod discovery;
pub mod ledger;
pub mod output;
pub mod prompt_catalog;
mod registry;
pub mod respond;
pub mod send_channel;
pub mod shell;
pub(crate) mod util;
pub(crate) mod utf8_stream;
pub mod wait;
pub(crate) use util::expand_path;
pub mod watcher_exit;

// ── Re-exports ──────────────────────────────────────────────────────────────────

pub use crate::runtime::subagent::{
    SubagentDisplayRow, SubagentHandle, SubagentRegistry, SubagentResult, SubagentState,
    SubagentStatus,
};
pub use agent::resolve_agent_prompt;
#[cfg(feature = "command-exec")]
pub use bash::{bash_intermediary_snapshot, BashIntermediarySnapshot, BashTool};
pub use discovery::{ActivateToolsTool, SearchToolsTool};
pub use edit::EditTool;
pub use extension::ExtensionTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use memory_context::MemoryContextTool;
pub use multi_edit::MultiEditTool;
pub(crate) use multi_edit::{label as multi_edit_label, paths as multi_edit_paths};
#[cfg(feature = "command-exec")]
pub use powershell::PowerShellTool;
pub use read::ReadTool;
pub use registry::{
    DeferredToolSplit, DroppedSessionMember, SessionSchemaProjection, ToolRegistry,
};
pub use respond::RespondTool;
pub use confirm_latch::{latch_keys, LatchKey, SessionConfirmLatch};
pub use secret_prompt::SecretPromptQueue;
pub use secret_prompt::{
    ConfirmChoice, ConfirmScope, PromptKind, SecretPromptHandle, SecretPromptRequest,
    CONFIRM_ANSWER_ALLOW_ALL,
};
pub use send_channel::SendChannelTool;

/// Call first thing in `main`: when this process was started by sudo as
/// the bash tool's askpass helper, runs it and returns its exit code (see
/// `sudo_askpass`). Linux only; elsewhere always `None`.
pub fn run_sudo_askpass_helper_if_invoked() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        sudo_askpass::run_helper_if_invoked()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
pub use shell::{ShellEndTool, ShellSendTool, ShellStartTool};
pub use subagent::{
    SubagentCollectTool, SubagentModelAuthorizeTool, SubagentModelsTool, SubagentResumeTool,
    SubagentStartTool, SubagentStatusTool, SubagentSteerTool, SubagentTool,
};
// The parent's context settings a worker inherits (§2, §7.2).
pub use subagent::context_policy::{InheritedWindow, WorkerContextPolicy};
pub(crate) use subagent::context_policy::{same_model_identity, WorkerContext};
pub(crate) use subagent::WORKER_ROLLOVER;
pub(crate) use subagent::lineage::WORKER_LOGICAL_PREFIX;
// A worker run's request ledger, for the runtime and its transports (§6).
pub(crate) use subagent::ledger::{self as request_ledger, LedgerSlot, RequestLedgerHandle};
#[cfg(test)]
pub(crate) use subagent::record as ledger_record;
#[cfg(test)]
pub(crate) use subagent::apply_subagent_runtime_policy;
pub use watcher_exit::{WatcherExitTool, WATCHER_EXIT_ACK};
pub use write::WriteTool;

// Re-export util items used by sibling tool modules via `super::`
pub(crate) use util::{
    peek_next_subagent_id, resolve_path_in, seed_next_subagent_id, strip_ansi, NEXT_SUBAGENT_ID,
};

// Facade: expose finalize internals for integration tests without making the
// subagent module pub. Tests import `agent_engine::tools::{build_completion_event, finalize_subagent}`.
#[doc(hidden)]
pub use subagent::finalize::{build_completion_event, finalize_subagent};

// The calling turn's before_tool_call route, set by the stream around each
// tool execution and read by the subagent spawn paths.
pub(crate) use subagent::{CallerHooks, CALLER_HOOKS};
pub(crate) use subagent::resume::resume_for_daemon;
// A reload-stopped worker's head left without its final: the adopting
// image marks it cancelled (train30 item 4b).
pub(crate) use subagent::archive::archive_root;
pub(crate) use subagent::crash_head::{mark_left as mark_left_crash_heads, Marked as CrashHeadMarked};
/// Test-only (`cfg(test)` / the `testing` feature): write worker
/// `handle_id`'s v2 head record, status "running" and no final, into this
/// process's archive directory, as a run the reload stopped before its
/// final record was published leaves it, and return its path.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub use subagent::crash_head::write_test_crash_head;

/// Test-only (`cfg(test)` / the `testing` feature): write worker
/// `handle_id`'s v1 resume archive into this process's archive directory,
/// as its finalizer would, and return its path.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn write_test_archive(
    handle_id: &str,
    meta: &crate::runtime::subagent::SubagentArchiveMeta,
    status: &str,
    history: &[serde_json::Value],
) -> std::io::Result<PathBuf> {
    let history: Vec<crate::SharedMessage> = history.iter().cloned().map(Arc::new).collect();
    subagent::archive::write_archive(
        &subagent::archive::process_dir(),
        handle_id,
        meta,
        status,
        None,
        "",
        &history,
    )
}

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
    /// Parent-session extension Confirm latch: the set of extension ids the
    /// user chose "Allow all" for. Workers share the Arc (including live
    /// updates and revokes), never a snapshot or host-global approval. This
    /// grants no `activate_tools` authority. None is for contexts without a
    /// parent session.
    pub session_allow_all: Option<Arc<SessionConfirmLatch>>,
    /// Host-owned note storage binding. Runtime contexts always provide the
    /// same binding; `None` is reserved for manually constructed contexts.
    /// A present binding is exclusive: errors never permit legacy fallback.
    pub memory_backend: Option<crate::memory_backend::MemoryBinding>,
    /// Parent-session telemetry for the subagents this context spawns: they
    /// record at the same level through the parent's session writer. `None`
    /// (manual contexts) leaves workers `Off`.
    pub telemetry: Option<crate::runtime::telemetry::WorkerTelemetry>,
    pub watcher_exit_path: Option<PathBuf>,
    pub tool_register_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<Arc<dyn Tool>>>>,
    pub session_manager: Option<std::sync::Arc<crate::tools::shell::SessionManager>>,
    pub subagent_registry: Option<Arc<Mutex<SubagentRegistry>>>,
    /// Originating turn cancellation, pinned before tool dispatch. A late
    /// registration cannot inherit a later user's renewed launch authority.
    pub launch_cancel: Option<crate::CancellationToken>,
    pub event_queue: Option<Arc<crate::events::EventQueue>>,
    /// Current worker handle when this context belongs to a delegated
    /// runtime. `None` denotes the foreground root.
    pub delegation_parent: Option<String>,
    /// Host-built foreground Codex plan for exact-model Ultra inheritance.
    /// Not a model-supplied tool parameter or an authorization grant.
    pub codex_parent_plan: Option<crate::runtime::openai::catalog::CodexExecutionPlan>,
    /// Host-built snapshot of the calling runtime's live context settings,
    /// for a worker it starts or resumes (subagent rollover §2, §7.2). Not a
    /// model-supplied parameter. `None` (every context today) means no
    /// worker inherits context settings.
    pub worker_context: Option<WorkerContextPolicy>,
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
    /// The calling session's own context-continuation state: the state a
    /// `context_checkpoint` call records into. The stream fills it from its
    /// own session for every tool call; it is never taken from the
    /// (possibly host-shared) tool registry, so sessions sharing one
    /// registry cannot reach each other's state. `None` (manual fixtures,
    /// routes without a session) makes `context_checkpoint` fail typed and
    /// touch nothing.
    pub continuation: Option<crate::runtime::continuation::SharedContinuation>,
    /// Per-session working directory (Phase 2 daemons serve sessions from N
    /// directories in one process). `None` = inherit the process cwd — the
    /// only value Phase 1 ever sets, so behaviour is byte-identical.
    pub cwd: Option<PathBuf>,
    /// Per-session environment snapshot (session-identity T1). `None` =
    /// inherit process env. When `Some`, tools call `env_clear().envs()`
    /// so no daemon env leaks into session subprocesses.
    pub env: Option<crate::session::types::SessionEnv>,
    /// Names of env vars stripped as secrets (T5). Tools use this for
    /// loud notices when a command fails referencing a stripped var.
    pub env_stripped: Vec<String>,
    /// Per-name "already warned" guard (T5): prevents duplicate notices
    /// within a single session. Shared via `Arc<Mutex<…>>` so the same
    /// set survives across tool calls (caps are rebuilt per call in
    /// stream.rs). Poisoned-mutex recovery via `into_inner`.
    pub env_warned: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Owning conversation id of the tool call, as carried on hook events.
    /// `None` for workers/subagents and for host contexts with no
    /// conversation. Forwarded to extension `tool.call` as
    /// `params.session_id` so a daemon-shared plugin knows its caller.
    /// Not the lease scope (`tool_session_id`, a random runtime id).
    pub session_id: Option<String>,
    /// The model's `tool_use` id of this call (the history block's
    /// `tool_use_id`). The stream sets it at both dispatch sites; `None` in
    /// every context outside a provider round (fixtures, the extension
    /// provider tool loop, plugin and skill commands).
    pub tool_use_id: Option<String>,
    /// The stream's per-turn correlation id (spec §5.2), opaque. Set with
    /// `tool_use_id` at both dispatch sites; `None` elsewhere.
    pub turn_correlation_id: Option<String>,
    /// Where a subagent launch from this context takes its run lease (child
    /// spend): the session's sink in a root turn, the worker's own run in a
    /// worker's. `None` in a host without a sink and in contexts that launch
    /// no workers: a oneshot then keeps today's aggregate Usage.
    pub spend: Option<crate::runtime::child_spend::SpendParent>,
    /// The session's write-sandbox binding (`crate::sandbox`), consulted at
    /// every bash/PTY spawn. Bound from the conversation id where there is
    /// one; `None` (workers, fixtures, contexts with no conversation) means
    /// no sandbox, exactly as before.
    pub sandbox: Option<std::sync::Arc<crate::sandbox::SandboxBinding>>,
}

/// Configuration limits and timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl ToolLimits {
    /// The internal buffer bound every production context uses; not a
    /// config key.
    pub const MAX_TOOL_BUFFER: usize = 256 * 1024;

    /// The configured `max_tool_output`, `bash_timeout`, `bash_max_timeout`
    /// and `subagent_timeout`.
    pub fn from_config(config: &crate::SynapsConfig) -> Self {
        Self {
            max_tool_output: config.max_tool_output,
            max_tool_buffer: Self::MAX_TOOL_BUFFER,
            bash_timeout: config.bash_timeout,
            bash_max_timeout: config.bash_max_timeout,
            subagent_timeout: config.subagent_timeout,
        }
    }

    /// The limits for a tool call with no runtime in reach (the
    /// extension-provider tool loop, a plugin command's extension tool): the
    /// installed host's current config, else the config file.
    pub(crate) fn configured() -> Self {
        match crate::EngineHost::current() {
            Some(host) => Self::from_config(&host.config()),
            None => Self::from_config(&crate::config::load_config()),
        }
    }
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
    /// A memory-only result (`_meta["synaps/memory_only"] == true` on an
    /// extension or MCP result). The stream keeps this text in RAM only and
    /// puts a fixed placeholder in the history; see `runtime::memory_only`.
    MemoryOnly(String),
}

impl ToolOutput {
    pub fn summary(&self) -> &str {
        match self {
            Self::Text(s) | Self::MemoryOnly(s) => s,
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
            Self::Text(s) | Self::MemoryOnly(s) => (s, None),
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

    /// The operation name this tool was registered under by its source (an
    /// extension's own tool name, before the `<extension>:` runtime prefix),
    /// when that differs from [`Tool::name`]. Typed metadata for policy that
    /// must not depend on the exposed name alone; `None` when not declared.
    fn source_operation(&self) -> Option<&str> {
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

    /// Each call is its own independent actor (a subagent): sibling calls in
    /// one batch each get their own scheduler lane and run concurrently,
    /// instead of joining the shared serial lane that `NonIdempotent` tools
    /// use. Scheduling only — the effect class, and with it the rule that an
    /// interrupted call is never re-run automatically, is unchanged.
    fn runs_independently(&self) -> bool {
        false
    }

    /// Key resolution derived from validated input. `Serialize` explicitly
    /// fails closed into the global mutation lane; `None` is the conservative
    /// default for tools with no key support.
    fn concurrency_key(&self, _validated_input: &Value) -> Option<ConcurrencyKey> {
        None
    }
}

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(all(test, target_os = "linux"))]
mod protected_tests;

#[cfg(all(test, target_os = "linux"))]
mod confine_tests;

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
