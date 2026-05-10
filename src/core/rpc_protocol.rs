//! RPC protocol types for the `synaps-bridge` parent↔child IPC channel.
//!
//! # Overview
//!
//! The synaps-bridge RPC protocol enables a parent process to spawn a
//! long-lived "rpc child" process and communicate with it over a pair of
//! `stdio` pipes (child's `stdin` / `stdout`). This module defines every
//! message type exchanged over that channel.
//!
//! See also: `synaps-bridge.SPEC.md §4` (path:
//! `/home/jr/Projects/Maha-Media/synaps-bridge.SPEC.md`).
//!
//! # Framing
//!
//! * **Encoding:** UTF-8, line-delimited JSON (LDJSON / NDJSON).
//! * **One frame per line:** each JSON object is terminated by a single `\n`
//!   (`0x0A`). No `Content-Length` header or other envelope.
//! * **Max frame size:** 1 MiB (1 048 576 bytes). Frames that exceed this
//!   limit are considered malformed. The rpc child must emit an
//!   [`RpcEvent::Error`] with `id: None` and remain alive when it encounters
//!   an oversized inbound frame. Enforcement logic lives in Task 2.
//! * **Direction:** the parent writes [`RpcCommand`] frames to the child's
//!   `stdin`; the child writes [`RpcEvent`] frames to its `stdout`.
//!
//! # Version semantics
//!
//! The current protocol version is [`RPC_PROTOCOL_VERSION`] = `1`. The child
//! emits [`RpcEvent::Ready`] immediately after startup, advertising its
//! `protocol_version`. The parent must refuse to proceed if the version does
//! not match its own expectation.
//!
//! # Correlation
//!
//! Every [`RpcCommand`] variant **except** [`RpcCommand::Shutdown`] carries
//! an `id: String` field. The rpc child echoes the same `id` in the
//! corresponding [`RpcEvent::Response`] frame, allowing the parent to
//! correlate requests and responses. The `id` format is opaque to the child
//! (UUID, monotonic counter, or any other string).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/// Wire-format protocol version.  Both sides must agree on this value;
/// the child advertises it in its [`RpcEvent::Ready`] frame.
pub const RPC_PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Auxiliary types
// ---------------------------------------------------------------------------

/// A file attachment included with a [`RpcCommand::Prompt`] message.
///
/// The rpc child reads the file at `path` from the local filesystem.
/// `name` and `mime` are optional hints; if absent the child falls back to
/// the basename of `path` and MIME auto-detection respectively.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcAttachment {
    /// Local filesystem path the rpc child can read.
    ///
    /// Convention (enforced when Task 10 adds binary attachment support):
    /// MUST be an absolute path; MUST NOT contain `..` segments. Path-traversal
    /// validation will reject relative or `..`-bearing paths at that point.
    pub path: String,
    /// Optional human-meaningful filename (defaults to basename of `path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional MIME hint; rpc child re-detects if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// Token-usage summary for a completed agent turn.
///
/// Mirrors the shape of `runtime::types::SessionEvent::Usage` so that
/// consumers of the RPC protocol have identical fields without depending on
/// the internal runtime type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnUsage {
    /// Prompt tokens sent to the model.
    pub input_tokens: u64,
    /// Completion tokens returned by the model.
    pub output_tokens: u64,
    /// Tokens served from the prompt cache (not billed at full rate).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Tokens written into the prompt cache during this turn.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// The model identifier used for this turn, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands: parent → rpc child
// ---------------------------------------------------------------------------

/// Commands sent from the **parent** process to the **rpc child** over the
/// child's `stdin`.
///
/// All variants except [`RpcCommand::Shutdown`] carry an `id` field that the
/// child echoes back in the matching [`RpcEvent::Response`] frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RpcCommand {
    /// Submit a new user prompt, optionally with file attachments.
    #[serde(rename = "prompt")]
    Prompt {
        /// Correlation id; echoed in the corresponding `Response` event.
        id: String,
        /// The user's message text.
        message: String,
        /// Zero or more file attachments.  Defaults to an empty list when
        /// the field is absent from the JSON frame.
        #[serde(default)]
        attachments: Vec<RpcAttachment>,
    },

    /// Send a follow-up message continuing the current conversation turn.
    #[serde(rename = "follow_up")]
    FollowUp {
        /// Correlation id.
        id: String,
        /// The follow-up message text.
        message: String,
    },

    /// Request in-context compaction of the conversation history.
    #[serde(rename = "compact")]
    Compact {
        /// Correlation id.
        id: String,
    },

    /// Start a fresh conversation session, discarding history.
    #[serde(rename = "new_session")]
    NewSession {
        /// Correlation id.
        id: String,
    },

    /// Retrieve the current conversation message history.
    #[serde(rename = "get_messages")]
    GetMessages {
        /// Correlation id.
        id: String,
    },

    /// Switch the active model for subsequent turns.
    #[serde(rename = "set_model")]
    SetModel {
        /// Correlation id.
        id: String,
        /// The model identifier to activate (e.g. `"claude-opus-4-5"`).
        model: String,
    },

    /// Enumerate models available to the current auth context.
    #[serde(rename = "get_available_models")]
    GetAvailableModels {
        /// Correlation id.
        id: String,
    },

    /// Abort the currently running prompt / agent turn.
    #[serde(rename = "abort")]
    Abort {
        /// Correlation id.
        id: String,
    },

    /// Retrieve aggregated token-usage statistics for the session.
    #[serde(rename = "get_session_stats")]
    GetSessionStats {
        /// Correlation id.
        id: String,
    },

    /// Retrieve the full runtime state snapshot of the rpc child.
    #[serde(rename = "get_state")]
    GetState {
        /// Correlation id.
        id: String,
    },

    /// Instruct the rpc child to exit cleanly.
    ///
    /// No `id` field — the child does not send a `Response` for shutdown.
    #[serde(rename = "shutdown")]
    Shutdown,
}

// ---------------------------------------------------------------------------
// Events: rpc child → parent
// ---------------------------------------------------------------------------

/// Events emitted by the **rpc child** to the **parent** over the child's
/// `stdout`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RpcEvent {
    /// A streaming update from the assistant (text delta, thinking, tool
    /// call lifecycle, …).  One or more of these frames precede each
    /// [`RpcEvent::AgentEnd`].
    #[serde(rename = "message_update")]
    MessageUpdate {
        /// The granular assistant event payload.
        event: AssistantEvent,
    },

    /// A subagent has been spawned to handle a delegated task.
    #[serde(rename = "subagent_start")]
    SubagentStart {
        /// Opaque monotonic id for this subagent instance.
        subagent_id: u64,
        /// Human-readable agent name.
        agent_name: String,
        /// First few words of the task description.
        task_preview: String,
    },

    /// A running subagent has produced an intermediate status update.
    #[serde(rename = "subagent_update")]
    SubagentUpdate {
        /// Identifies the subagent (matches a prior [`RpcEvent::SubagentStart`]).
        subagent_id: u64,
        /// Human-readable agent name.
        agent_name: String,
        /// Free-form status string (e.g. `"running"`, `"tool_call"`).
        status: String,
    },

    /// A subagent has finished.
    #[serde(rename = "subagent_done")]
    SubagentDone {
        /// Identifies the subagent.
        subagent_id: u64,
        /// Human-readable agent name.
        agent_name: String,
        /// First few words of the result.
        result_preview: String,
        /// Wall-clock seconds the subagent ran for.
        duration_secs: f64,
    },

    /// The agent turn has completed.  Carries final token-usage data.
    #[serde(rename = "agent_end")]
    AgentEnd {
        /// Token usage for the completed turn.
        usage: TurnUsage,
    },

    /// A response to a specific [`RpcCommand`], correlated by `id`.
    ///
    /// The `body` is **flattened** into the enclosing JSON object — its keys
    /// appear at the top level alongside `"type"`, `"id"`, and `"command"`.
    #[serde(rename = "response")]
    Response {
        /// Echoed from the originating [`RpcCommand`]'s `id` field.
        id: String,
        /// The command name this is responding to (e.g. `"get_messages"`).
        command: String,
        /// Arbitrary response payload, flattened into the JSON frame.
        ///
        /// Type-erased to `serde_json::Value` for forward-compat with new
        /// `command` strings. Rust consumers wanting strong typing should
        /// inspect `command` and re-deserialise `body` into a per-command struct.
        #[serde(flatten)]
        body: serde_json::Value,
    },

    /// A protocol-level or runtime error.
    ///
    /// `id` is `None` for errors not attributable to a specific command
    /// (e.g. oversized frame, internal crash).
    #[serde(rename = "error")]
    Error {
        /// Correlation id of the command that caused the error, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Human-readable error description.
        message: String,
    },

    /// Emitted by the rpc child immediately after startup, before any
    /// commands are accepted.
    #[serde(rename = "ready")]
    Ready {
        /// Unique identifier for this session (UUID or similar).
        session_id: String,
        /// The model currently active.
        model: String,
        /// The protocol version implemented by this child.
        /// Must equal [`RPC_PROTOCOL_VERSION`] for the parent to proceed.
        protocol_version: u32,
    },
}

// ---------------------------------------------------------------------------
// Assistant streaming events
// ---------------------------------------------------------------------------

/// Granular events emitted by the assistant during a streaming turn.
///
/// Carried inside [`RpcEvent::MessageUpdate`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AssistantEvent {
    /// An incremental text chunk from the assistant's response.
    #[serde(rename = "text_delta")]
    TextDelta {
        /// The text fragment.
        delta: String,
    },

    /// An incremental thinking/reasoning chunk (extended thinking mode).
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        /// The thinking fragment.
        delta: String,
    },

    /// A tool call has started streaming.  Subsequent
    /// [`AssistantEvent::ToolcallInputDelta`] frames carry the JSON input.
    #[serde(rename = "toolcall_start")]
    ToolcallStart {
        /// Model-assigned opaque identifier for this tool call.
        tool_id: String,
        /// The tool being invoked.
        tool_name: String,
    },

    /// An incremental JSON fragment of a tool call's input.
    #[serde(rename = "toolcall_input_delta")]
    ToolcallInputDelta {
        /// Matches the `tool_id` from [`AssistantEvent::ToolcallStart`].
        tool_id: String,
        /// Raw JSON fragment (not yet a complete object).
        delta: String,
    },

    /// The complete, finalised input for a tool call.
    #[serde(rename = "toolcall_input")]
    ToolcallInput {
        /// Matches the `tool_id` from [`AssistantEvent::ToolcallStart`].
        tool_id: String,
        /// The fully parsed JSON input value.
        input: serde_json::Value,
    },

    /// The result returned by tool execution.
    #[serde(rename = "toolcall_result")]
    ToolcallResult {
        /// Matches the `tool_id` from [`AssistantEvent::ToolcallStart`].
        tool_id: String,
        /// The serialised tool result string.
        result: String,
    },
}
