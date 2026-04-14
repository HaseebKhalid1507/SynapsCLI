use serde_json::Value;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Thinking(String),
    Text(String),
    ToolUseStart(String),
    ToolUseDelta(String),
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
    /// Subagent lifecycle events — rendered as a live status panel in the TUI
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
    /// A steering message was delivered mid-stream (between tool rounds)
    SteeringDelivered {
        message: String,
    },
    /// Full message history after the tool loop completes, for multi-turn context
    MessageHistory(Vec<Value>),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        /// Model that generated this usage. None = use parent runtime's model.
        model: Option<String>,
    },
    Done,
    Error(String),
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