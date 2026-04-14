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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_client_message_message_roundtrip() {
        let msg = ClientMessage::Message {
            content: "Hello, world!".to_string(),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "message");
        assert_eq!(json_value["content"], "Hello, world!");

        // Deserialize back and verify
        let deserialized: ClientMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ClientMessage::Message { content } => {
                assert_eq!(content, "Hello, world!");
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_client_message_cancel_roundtrip() {
        let msg = ClientMessage::Cancel;

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "cancel");

        // Deserialize back and verify
        let deserialized: ClientMessage = serde_json::from_value(json_value).unwrap();
        matches!(deserialized, ClientMessage::Cancel);
    }

    #[test]
    fn test_client_message_command_roundtrip() {
        let msg = ClientMessage::Command {
            name: "clear".to_string(),
            args: "".to_string(),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "command");
        assert_eq!(json_value["name"], "clear");
        assert_eq!(json_value["args"], "");

        // Deserialize back and verify
        let deserialized: ClientMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ClientMessage::Command { name, args } => {
                assert_eq!(name, "clear");
                assert_eq!(args, "");
            }
            _ => panic!("Expected Command variant"),
        }
    }

    #[test]
    fn test_server_message_text_roundtrip() {
        let msg = ServerMessage::Text {
            content: "Hello from server!".to_string(),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "text");
        assert_eq!(json_value["content"], "Hello from server!");

        // Deserialize back and verify
        let deserialized: ServerMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ServerMessage::Text { content } => {
                assert_eq!(content, "Hello from server!");
            }
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_server_message_done_roundtrip() {
        let msg = ServerMessage::Done;

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "done");

        // Deserialize back and verify
        let deserialized: ServerMessage = serde_json::from_value(json_value).unwrap();
        matches!(deserialized, ServerMessage::Done);
    }

    #[test]
    fn test_server_message_error_roundtrip() {
        let msg = ServerMessage::Error {
            message: "Something went wrong".to_string(),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "error");
        assert_eq!(json_value["message"], "Something went wrong");

        // Deserialize back and verify
        let deserialized: ServerMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ServerMessage::Error { message } => {
                assert_eq!(message, "Something went wrong");
            }
            _ => panic!("Expected Error variant"),
        }
    }

    #[test]
    fn test_server_message_tool_use_roundtrip() {
        let msg = ServerMessage::ToolUse {
            tool_name: "execute_bash".to_string(),
            tool_id: "tool_123".to_string(),
            input: json!({"command": "ls -la", "timeout": 30}),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "tool_use");
        assert_eq!(json_value["tool_name"], "execute_bash");
        assert_eq!(json_value["tool_id"], "tool_123");
        assert_eq!(json_value["input"]["command"], "ls -la");
        assert_eq!(json_value["input"]["timeout"], 30);

        // Deserialize back and verify
        let deserialized: ServerMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ServerMessage::ToolUse { tool_name, tool_id, input } => {
                assert_eq!(tool_name, "execute_bash");
                assert_eq!(tool_id, "tool_123");
                assert_eq!(input["command"], "ls -la");
                assert_eq!(input["timeout"], 30);
            }
            _ => panic!("Expected ToolUse variant"),
        }
    }

    #[test]
    fn test_server_message_usage_roundtrip() {
        let msg = ServerMessage::Usage {
            input_tokens: 150,
            output_tokens: 75,
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&msg).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["type"], "usage");
        assert_eq!(json_value["input_tokens"], 150);
        assert_eq!(json_value["output_tokens"], 75);

        // Deserialize back and verify
        let deserialized: ServerMessage = serde_json::from_value(json_value).unwrap();
        match deserialized {
            ServerMessage::Usage { input_tokens, output_tokens } => {
                assert_eq!(input_tokens, 150);
                assert_eq!(output_tokens, 75);
            }
            _ => panic!("Expected Usage variant"),
        }
    }

    #[test]
    fn test_history_entry_user_roundtrip() {
        let entry = HistoryEntry::User {
            content: "User message".to_string(),
            time: "2023-10-01T12:00:00Z".to_string(),
        };

        // Serialize to JSON
        let json_value = serde_json::to_value(&entry).unwrap();

        // Verify JSON structure
        assert_eq!(json_value["role"], "user");
        assert_eq!(json_value["content"], "User message");
        assert_eq!(json_value["time"], "2023-10-01T12:00:00Z");

        // Deserialize back and verify
        let deserialized: HistoryEntry = serde_json::from_value(json_value).unwrap();
        match deserialized {
            HistoryEntry::User { content, time } => {
                assert_eq!(content, "User message");
                assert_eq!(time, "2023-10-01T12:00:00Z");
            }
            _ => panic!("Expected User variant"),
        }
    }
}
