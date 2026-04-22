//! `shell_send` tool — send input to an active shell session.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use crate::tools::{Tool, ToolContext};

pub struct ShellSendTool;

#[async_trait::async_trait]
impl Tool for ShellSendTool {
    fn name(&self) -> &str { "shell_send" }

    fn description(&self) -> &str {
        "Send input to an active shell session. Returns the output produced after sending the input. The input is sent exactly as received after JSON parsing — a JSON string containing \\n will send an actual newline (Enter key). Use \\x03 for Ctrl-C, \\x04 for Ctrl-D."
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

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let mgr = ctx.capabilities.session_manager.as_ref()
            .ok_or_else(|| RuntimeError::Tool("Shell sessions not available".into()))?;
        
        let session_id = params["session_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing session_id parameter".into()))?;
        let input = params["input"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing input parameter".into()))?;
        let timeout_ms = params["timeout_ms"].as_u64();
        
        let result = mgr.send_input(session_id, input, timeout_ms, ctx.channels.tx_delta.as_ref()).await?;
        
        if result.status == "active" {
            Ok(result.output)
        } else {
            let mut out = result.output;
            out.push_str(&format!("\n[{}]", result.status));
            Ok(out)
        }
    }
}
