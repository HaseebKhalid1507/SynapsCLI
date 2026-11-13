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
mod extension;
mod find;
mod grep;
mod ls;
pub mod memory;
mod multi_edit;
#[cfg(feature = "command-exec")]
mod powershell;
pub(crate) mod read;
mod search;
mod unix_programs;
mod secret_prompt;
mod subagent;
#[cfg(target_os = "linux")]
pub mod sudo_askpass;
pub(crate) use subagent::archive::write_window_transcript;
#[cfg(test)]
pub(crate) use subagent::archive::resume_messages;
pub(crate) use subagent::finalize::build_reload_carry_event;
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
pub(crate) use util::expand_path;

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
pub use write::WriteTool;

// Re-export util items used by sibling tool modules via `super::`
pub(crate) use util::{
    peek_next_subagent_id, resolve_path_in, seed_next_subagent_id, strip_ansi, NEXT_SUBAGENT_ID,
};

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
#[derive(Clone, Default)]
pub struct ToolCapabilities {
    /// Parent-session extension Confirm latch: the set of extension ids the
    /// user chose "Allow all" for. Workers share the Arc (including live
    /// updates and revokes), never a snapshot or host-global approval. This
    /// grants no `activate_tools` authority. None is for contexts without a
    /// parent session.
    pub session_allow_all: Option<Arc<SessionConfirmLatch>>,
    /// Host-owned note storage binding. Every context made for a chat has
    /// its chat's: a turn's ([`TurnToolContext`]) and a slash command's
    /// (`skills::commands::CommandChat`). `None` is left for a context made
    /// for no chat ([`TurnToolContext::no_chat`]) and for the ones tests
    /// build by hand; there the memory tools refuse (they never
    /// derive a project from the process folder).
    pub memory_backend: Option<crate::memory_backend::MemoryBinding>,
    /// Parent-session telemetry for the subagents this context spawns: they
    /// record at the same level through the parent's session writer. `None`
    /// (manual contexts) leaves workers `Off`.
    pub telemetry: Option<crate::runtime::telemetry::WorkerTelemetry>,
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
    /// Per-session working directory (Phase 2 daemons serve sessions from N
    /// directories in one process). `None` = inherit the process cwd — the
    /// only value Phase 1 ever sets, so behaviour is byte-identical.
    pub cwd: Option<PathBuf>,
    /// Per-session environment snapshot (session-identity T1). `None` =
    /// inherit process env. When `Some`, tools call `env_clear().envs()`
    /// so no daemon env leaks into session subprocesses.
    pub env: Option<crate::session::types::ToolEnv>,
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
    /// Extension hook bus, for the notify-only events a builtin tool sends
    /// (`memory_forget`: `on_memory_forgotten`). A worker carries its own
    /// view, which sends nothing. `None` wherever no bus exists; the tool
    /// then works the same and nothing is announced.
    pub hook_bus: Option<Arc<crate::extensions::hooks::HookBus>>,
    /// The calling chat's rollover state, where `context_checkpoint` records
    /// its phase and note. The tool is one instance shared by every chat on
    /// the host and has no state of its own. `None` for a context with no
    /// chat: the tool refuses there. (A worker carries its own state, which
    /// is never enabled, and its registry has no such tool.)
    pub continuation: Option<crate::runtime::continuation::SharedContinuation>,
}

impl ToolCapabilities {
    /// T5: a notice for each stripped secret `pick` selects that this
    /// session was not told about yet (once per name per session). `bash`
    /// picks the names a failed command references; `shell_start` all of
    /// them, up front (an interactive shell's commands never reach a tool
    /// that could tell).
    pub(crate) fn stripped_env_notices(&self, pick: impl Fn(&str) -> bool) -> String {
        let mut notice = String::new();
        if self.env_stripped.is_empty() {
            return notice;
        }
        let mut warned = self.env_warned.lock().unwrap_or_else(|p| p.into_inner());
        for name in self.env_stripped.iter().filter(|n| pick(n)) {
            if warned.insert(name.clone()) {
                notice.push_str(&format!(
                    "\nnote: ${name} was stripped from the session env as a secret; \
                     reattach from a shell that has it \
                     (synaps --system … from that shell) \
                     — the daemon never receives credentials."
                ));
            }
        }
        notice
    }
}

/// Configuration limits and timeouts.
#[derive(Clone)]
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

/// What the tool calls of one turn are given, whichever loop runs them.
///
/// A turn's tool calls are run by the loop that received the model's
/// answer: the stream loop (its serial and its parallel lane) or, for a
/// model an extension provides, the loop inside the provider round
/// (`runtime::openai::extension_route`). The host builds one of these per
/// turn and each loop makes every call's [`ToolContext`] from it
/// ([`Self::for_call`]). So what one loop's tools are given, the others'
/// are given too: there is no second list of fields to keep in step.
///
/// Two capabilities belong to one dispatch of the stream loop and are not
/// part of the turn's context: `tool_activation` (the catalog snapshot of
/// one provider round) and `tool_register_tx` (a channel that dispatch
/// drains). The stream loop adds them with [`Self::for_dispatch`]. The
/// extension model's loop does not have them, on purpose: its tool list is
/// fixed for the whole provider round, so a tool activated inside it could
/// not be offered or called there, and nothing would drain a registration.
///
/// The event channel is the turn's too (`tx_events` of every call): the
/// stream's own sender, which outlives a provider round. Only the delta
/// lane is made per call.
///
/// One pointer wide: it rides in `ApiOptions` and in the stream loop's
/// state, where a copy of every capability per clone is stack the loop does
/// not have to spare in a debug build.
#[derive(Clone)]
pub struct TurnToolContext(Arc<TurnTools>);

struct TurnTools {
    capabilities: ToolCapabilities,
    limits: ToolLimits,
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
}

impl TurnToolContext {
    /// `capabilities` as the host has them for this turn. Its
    /// `tool_activation` and `tool_register_tx` are per dispatch
    /// ([`Self::for_dispatch`]) and are dropped here. `events` is the
    /// turn's event channel (`None`: the turn has no frontend to tell).
    pub fn new(
        mut capabilities: ToolCapabilities,
        limits: ToolLimits,
        events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
    ) -> Self {
        capabilities.tool_activation = None;
        capabilities.tool_register_tx = None;
        Self(Arc::new(TurnTools {
            capabilities,
            limits,
            events,
        }))
    }

    /// For a call that belongs to no chat (the public routing facade, used
    /// by tests): no capability at all, so the tools run in the process
    /// folder with the process environment, for no conversation, and the
    /// memory tools refuse.
    pub fn no_chat() -> Self {
        Self(Arc::new(TurnTools {
            capabilities: ToolCapabilities::default(),
            limits: ToolLimits {
                max_tool_output: 30000,
                max_tool_buffer: 256 * 1024,
                bash_timeout: 30,
                bash_max_timeout: 300,
                subagent_timeout: 300,
            },
            events: None,
        }))
    }

    /// This turn's context for one dispatch of the stream loop: with the
    /// round's activation capability and the dispatch's registration channel.
    pub fn for_dispatch(
        &self,
        tool_register_tx: tokio::sync::mpsc::UnboundedSender<Vec<Arc<dyn Tool>>>,
        tool_activation: crate::tools::discovery::ActivationCapability,
    ) -> Self {
        let mut capabilities = self.0.capabilities.clone();
        capabilities.tool_register_tx = Some(tool_register_tx);
        capabilities.tool_activation = Some(tool_activation);
        Self(Arc::new(TurnTools {
            capabilities,
            limits: self.0.limits.clone(),
            events: self.0.events.clone(),
        }))
    }

    /// The context of one tool call. `tx_delta` is the call's own lane for
    /// partial output (`None` where the call is not shown as it runs).
    pub fn for_call(&self, tx_delta: Option<crate::tools::output::DeltaSender>) -> ToolContext {
        ToolContext {
            channels: ToolChannels {
                tx_delta,
                tx_events: self.0.events.clone(),
            },
            capabilities: self.0.capabilities.clone(),
            limits: self.0.limits.clone(),
        }
    }

    pub fn capabilities(&self) -> &ToolCapabilities {
        &self.0.capabilities
    }

    pub fn limits(&self) -> &ToolLimits {
        &self.0.limits
    }
}

/// Fields elided: capabilities hold handles, and a session's forwarded
/// secrets must not be printed.
impl std::fmt::Debug for TurnToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnToolContext")
            .field("session_id", &self.0.capabilities.session_id)
            .field("cwd", &self.0.capabilities.cwd)
            .finish_non_exhaustive()
    }
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

    /// [`Self::concurrency_key`] for a call made in `cwd`, the chat's folder
    /// (`None`: the process folder). The scheduler asks this one. A tool
    /// whose key is a path resolves a relative one against `cwd`, as its
    /// `execute` does, so the key names the file the call will touch; a
    /// tool whose key does not depend on the folder has nothing to add.
    fn concurrency_key_in(
        &self,
        validated_input: &Value,
        _cwd: Option<&std::path::Path>,
    ) -> Option<ConcurrencyKey> {
        self.concurrency_key(validated_input)
    }
}

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod stripped_env_notice_tests {
    /// One notice per picked stripped name, once per session (the shared
    /// set), the same text `bash` and `shell_start` print.
    #[test]
    fn notices_name_each_picked_secret_once_per_session() {
        let mut caps = super::test_helpers::create_tool_context().capabilities;
        assert_eq!(caps.stripped_env_notices(|_| true), "", "nothing stripped: nothing to say");
        caps.env_stripped = vec!["GH_TOKEN".into(), "AWS_SECRET_ACCESS_KEY".into()];
        let first = caps.stripped_env_notices(|n| n == "GH_TOKEN");
        assert!(first.contains("note: $GH_TOKEN was stripped from the session env as a secret"), "{first}");
        assert!(!first.contains("AWS_SECRET_ACCESS_KEY"), "{first}");
        let all = caps.stripped_env_notices(|_| true);
        assert!(all.contains("$AWS_SECRET_ACCESS_KEY") && !all.contains("$GH_TOKEN"), "{all}");
        assert_eq!(caps.stripped_env_notices(|_| true), "", "each name once");
    }
}

#[cfg(test)]
mod turn_tool_context_tests {
    use super::*;

    /// A turn's context is carried by pointer. Held by value in
    /// `ApiOptions` and the stream loop's state it overflowed a 2 MiB
    /// worker stack in a debug build (`session_actor_cache_keepwarm`).
    #[test]
    fn a_turns_tool_context_is_one_pointer_wide() {
        eprintln!(
            "ToolCapabilities: {} bytes, ToolContext: {} bytes",
            std::mem::size_of::<ToolCapabilities>(),
            std::mem::size_of::<ToolContext>(),
        );
        assert_eq!(
            std::mem::size_of::<Option<TurnToolContext>>(),
            std::mem::size_of::<usize>()
        );
    }

    /// The two capabilities of one stream dispatch are not a turn's: `new`
    /// drops them, `for_dispatch` adds them to its own copy only, and a
    /// call's context is the turn's otherwise.
    #[test]
    fn dispatch_capabilities_never_reach_the_turns_context() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        let turn = TurnToolContext::new(
            ToolCapabilities {
                tool_register_tx: Some(tx.clone()),
                session_id: Some("chat-A".into()),
                env_stripped: vec!["SECRET".into()],
                ..Default::default()
            },
            TurnToolContext::no_chat().limits().clone(),
            Some(events.clone()),
        );
        assert!(turn.capabilities().tool_register_tx.is_none());

        let set = crate::tools::activation::SessionToolSet::progressive_core_for_catalog(
            crate::tools::activation::SessionId::parse("turn-tools").unwrap(),
            &crate::tools::catalog::ToolCatalog::empty(),
        );
        let dispatch = turn.for_dispatch(
            tx,
            crate::tools::discovery::ActivationCapability::new(
                crate::tools::catalog::ToolCatalog::empty(),
                Arc::new(std::sync::RwLock::new(set)),
                crate::tools::activation::ActivationAuthority::ModelConfirmed,
            ),
        );
        let (of_turn, of_dispatch) = (turn.for_call(None), dispatch.for_call(None));
        assert!(of_turn.capabilities.tool_register_tx.is_none());
        assert!(of_turn.capabilities.tool_activation.is_none());
        assert!(of_dispatch.capabilities.tool_register_tx.is_some());
        assert!(of_dispatch.capabilities.tool_activation.is_some());
        for ctx in [&of_turn, &of_dispatch] {
            assert_eq!(ctx.capabilities.session_id.as_deref(), Some("chat-A"));
            assert_eq!(ctx.capabilities.env_stripped, ["SECRET"]);
            // The turn's event channel, whichever loop makes the call.
            assert!(ctx
                .channels
                .tx_events
                .as_ref()
                .is_some_and(|tx| tx.same_channel(&events)));
            assert!(ctx.channels.tx_delta.is_none());
        }
        let none = TurnToolContext::no_chat().for_call(None);
        assert!(none.channels.tx_events.is_none());
        assert!(none.capabilities.session_id.is_none() && none.capabilities.env.is_none());
        assert!(none.capabilities.memory_backend.is_none());
    }
}

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
