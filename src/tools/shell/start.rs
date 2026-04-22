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

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let mgr = ctx.capabilities.session_manager.as_ref()
            .ok_or_else(|| RuntimeError::Tool("Shell sessions not available".into()))?;
        
        let command = params["command"].as_str().map(|s| s.to_string());
        let working_directory = params["working_directory"].as_str().map(|s| s.to_string());
        let rows = params["rows"].as_u64().map(|r| r as u16);
        let cols = params["cols"].as_u64().map(|c| c as u16);
        let readiness_timeout_ms = params["readiness_timeout_ms"].as_u64();
        let idle_timeout = params["idle_timeout"].as_u64();
        
        let env = params["env"].as_object()
            .map(|obj| obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect())
            .unwrap_or_default();
        
        let opts = super::SessionOpts {
            command, working_directory, env, rows, cols,
            readiness_timeout_ms, idle_timeout,
        };
        
        let (session_id, output, status) = mgr.create_session(opts, ctx.channels.tx_delta.as_ref()).await?;
        
        let mut result = format!("[Session {} | {}]\n", session_id, status);
        if !output.is_empty() {
            result.push_str(&output);
        }
        Ok(result)
    }
}
