//! `shell_start` tool — create a new interactive shell session.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use crate::tools::{Tool, ToolContext};

pub struct ShellStartTool;

#[async_trait::async_trait]
impl Tool for ShellStartTool {
    fn name(&self) -> &str { "shell_start" }

    fn description(&self) -> &str {
        "Start a new interactive shell session with a PTY. Returns a session ID and the initial output. Use shell_send to interact and shell_end to close."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to run (default: user's default shell). Examples: 'bash', 'python3', 'ssh user@host'"
                },
                "working_directory": {
                    "type": "string",
                    "description": "Working directory for the session (default: current directory)"
                },
                "env": {
                    "type": "object",
                    "description": "Additional environment variables as key-value pairs",
                    "additionalProperties": { "type": "string" }
                },
                "rows": {
                    "type": "integer",
                    "description": "Terminal rows (default: from config, fallback 24)"
                },
                "cols": {
                    "type": "integer",
                    "description": "Terminal columns (default: from config, fallback 80)"
                },
                "readiness_timeout_ms": {
                    "type": "integer",
                    "description": "Override output readiness timeout for this session (ms)"
                },
                "idle_timeout": {
                    "type": "integer",
                    "description": "Override idle timeout for this session (seconds)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<String> {
        Err(RuntimeError::Tool("shell_start not yet implemented".into()))
    }
}
