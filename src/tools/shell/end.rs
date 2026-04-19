//! `shell_end` tool — close an interactive shell session.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use crate::tools::{Tool, ToolContext};

pub struct ShellEndTool;

#[async_trait::async_trait]
impl Tool for ShellEndTool {
    fn name(&self) -> &str { "shell_end" }

    fn description(&self) -> &str {
        "Close an interactive shell session and clean up resources. Returns the final output if any."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to close"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Err(RuntimeError::Tool("shell_end not yet implemented".into()))
    }
}
