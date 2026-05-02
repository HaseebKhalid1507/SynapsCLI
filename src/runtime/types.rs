use serde_json::Value;
use serde::Deserialize;

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
    /// model will reference in its eventual `tool_use` content block —
    /// chat UIs use it to route subsequent `ToolUseDelta` chunks and the
    /// final `ToolUse` finalize event to the correct on-screen block when
    /// multiple tools run in parallel.
    ToolUseStart {
        tool_name: String,
        tool_id: String,
    },
    /// Streaming chunk of a tool's input JSON. `tool_id` matches the
    /// `ToolUseStart` it belongs to; with parallel tool calls the model
    /// may interleave deltas from different tools.
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
    MessageHistory(Vec<Value>),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        model: Option<String>,
    },
    Done,
    Error(String),
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

/// Shared mutable auth state. Lives behind `Arc<RwLock<_>>` so the spawned
/// streaming task and the parent Runtime always see the same (freshest) token.
#[derive(Debug, Clone)]
pub(super) struct AuthState {
    pub(super) auth_token: String,
    pub(super) auth_type: String,
    pub(super) refresh_token: Option<String>,
    pub(super) token_expires: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PiAuth {
    pub(super) anthropic: AnthropicAuth,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(super) struct AnthropicAuth {
    #[serde(rename = "type")]
    pub(super) auth_type: String,
    pub(super) refresh: Option<String>,
    pub(super) access: Option<String>,
    pub(super) expires: Option<u64>,
    pub(super) key: Option<String>,
}