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

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let mgr = ctx.capabilities.session_manager.as_ref()
            .ok_or_else(|| RuntimeError::Tool("Shell sessions not available".into()))?;
        
        let session_id = params["session_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing session_id parameter".into()))?;
        
        let output = mgr.close_session(session_id).await?;
        
        if output.is_empty() {
            Ok(format!("[Session {} closed]", session_id))
        } else {
            Ok(format!("{}\n[Session {} closed]", output, session_id))
        }
    }
}
