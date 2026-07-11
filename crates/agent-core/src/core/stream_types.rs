//! Core streaming event types shared across agent-core (rpc_dispatch) and
//! the engine layer (runtime, engine). Defined here so `agent-core` is a
//! clean leaf with no upward dependencies into the engine layer.

use serde_json::Value;
use std::sync::Arc;

/// Reference-counted shared message payload. Introduced at the runtime's
/// inner boundary (T128 Slice 2) to enable clone-free fan-out of message
/// history through the stream loop and `SessionEvent::MessageHistory`.
/// Outer state (App/Session/ConversationState) remains `Vec<Value>` and
/// shims at the boundary — see Slice 5 for full conversion.
pub type SharedMessage = Arc<Value>;

/// Top-level stream event — grouped into concern-specific sub-enums.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Llm(LlmEvent),
    Session(SessionEvent),
    Agent(AgentEvent),
}

#[derive(Debug, Clone)]
pub enum LlmEvent {
    Thinking(String),
    Text(String),
    /// A tool-use block has begun streaming. `tool_id` is the call id the
    /// model will reference in its eventual `tool_use` content block.
    ToolUseStart {
        tool_name: String,
        tool_id: String,
    },
    /// Streaming chunk of a tool's input JSON.
    ToolUseDelta {
        tool_id: String,
        delta: String,
    },
    ToolUse {
        tool_name: String,
        tool_id: String,
        input: Value,
    },
    ToolResult {
        tool_id: String,
        result: String,
    },
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    MessageHistory(Vec<SharedMessage>),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        /// Cache-write TTL split. `None` when the API omits the breakdown.
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: Option<String>,
    },
    Done,
    Error(String),
    /// Transient status line — display-only, never persisted.
    Notice(String),
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    SubagentStart {
        subagent_id: u64,
        agent_name: String,
        task_preview: String,
    },
    SubagentUpdate {
        subagent_id: u64,
        agent_name: String,
        status: String,
    },
    SubagentDone {
        subagent_id: u64,
        agent_name: String,
        result_preview: String,
        duration_secs: f64,
    },
    SteeringDelivered {
        message: String,
    },
}
