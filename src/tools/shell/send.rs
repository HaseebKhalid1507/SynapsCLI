//! `shell_send` tool — send input to an active shell session.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use crate::tools::{Tool, ToolContext};

pub struct ShellSendTool;

#[async_trait::async_trait]
impl Tool for ShellSendTool {
    fn name(&self) -> &str { "shell_send" }

    fn description(&self) -> &str {
        "Send input to an active shell session. Returns the output produced after sending the input. The input is sent exactly as provided — include \\n for Enter, \\x03 for Ctrl-C, \\x04 for Ctrl-D."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID from shell_start"
                },
                "input": {
                    "type": "string",
                    "description": "Text to send to the shell. Use \\n for Enter, \\x03 for Ctrl-C, \\x04 for Ctrl-D"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Override readiness timeout for this send (ms)"
                }
            },
            "required": ["session_id", "input"]
        })
    }

    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Err(RuntimeError::Tool("shell_send not yet implemented".into()))
    }
}
