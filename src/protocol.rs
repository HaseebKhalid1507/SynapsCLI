use serde::{Serialize, Deserialize};
use serde_json::Value;

/// Messages sent from client → server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Send a user message to the conversation
    #[serde(rename = "message")]
    Message { content: String },

    /// Execute a slash command
    #[serde(rename = "command")]
    Command { name: String, args: String },

    /// Cancel the current streaming response
    #[serde(rename = "cancel")]
    Cancel,

    /// Request current session state
    #[serde(rename = "status")]
    Status,

    /// Request conversation history (for late-joining clients)
    #[serde(rename = "history")]
    History,
}

/// Messages sent from server → client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Thinking tokens (streamed incrementally)
    #[serde(rename = "thinking")]
    Thinking { content: String },

    /// Text tokens (streamed incrementally)
    #[serde(rename = "text")]
    Text { content: String },

    /// A tool is ABOUT to be invoked (streaming JSON args)
    #[serde(rename = "tool_use_start")]
    ToolUseStart { tool_name: String },

    /// Streaming chunk of the tool's JSON arguments
    #[serde(rename = "tool_use_delta")]
    ToolUseDelta(String),

    /// A tool was invoked (JSON finished)
    #[serde(rename = "tool_use")]
    ToolUse {
        tool_name: String,
        tool_id: String,
        input: Value,
    },

    /// Tool execution result
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_id: String,
        result: String,
    },

    /// Tool execution incremental delta
    #[serde(rename = "tool_result_delta")]
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },

    /// Token usage update
    #[serde(rename = "usage")]
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },

    /// Streaming complete for this turn
    #[serde(rename = "done")]
    Done,

    /// Error occurred
    #[serde(rename = "error")]
    Error { message: String },

    /// System/info message (command responses, status)
    #[serde(rename = "system")]
    System { message: String },

    /// Full conversation history (response to History request)
    #[serde(rename = "history")]
    HistoryResponse { messages: Vec<HistoryEntry> },

    /// Server status
    #[serde(rename = "status")]
    StatusResponse {
        model: String,
        thinking: String,
        streaming: bool,
        session_id: String,
        total_input_tokens: u64,
        total_output_tokens: u64,
        session_cost: f64,
        connected_clients: usize,
    },
}

/// A single entry in the conversation history (display-friendly)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum HistoryEntry {
    #[serde(rename = "user")]
    User { content: String, time: String },
    #[serde(rename = "thinking")]
    Thinking { content: String, time: String },
    #[serde(rename = "text")]
    Text { content: String, time: String },
    #[serde(rename = "tool_use")]
    ToolUse { tool_name: String, input: String, time: String },
    #[serde(rename = "tool_result")]
    ToolResult { result: String, time: String },
    #[serde(rename = "system")]
    System { content: String, time: String },
    #[serde(rename = "error")]
    Error { content: String, time: String },
}
