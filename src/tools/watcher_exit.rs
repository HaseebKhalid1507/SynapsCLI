use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use crate::watcher_types::HandoffState;
use super::{Tool, ToolContext};

pub struct WatcherExitTool;

#[async_trait::async_trait]
impl Tool for WatcherExitTool {
    fn name(&self) -> &str { "watcher_exit" }

    fn description(&self) -> &str {
        "Signal that you've completed your work. Call this when you're done or at a natural stopping point. Provide a handoff summary for your next session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "why you're exiting"
                },
                "summary": {
                    "type": "string",
                    "description": "what you accomplished this session"
                },
                "pending": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "tasks still pending"
                },
                "context": {
                    "type": "object",
                    "description": "any structured data for next session"
                }
            },
            "required": ["reason", "summary"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let reason = params["reason"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing reason parameter".to_string()))?;
        let summary = params["summary"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing summary parameter".to_string()))?;
        
        let pending = params["pending"].as_array()
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>())
            .unwrap_or_default();
        
        let context = if params["context"].is_null() {
            serde_json::Value::Null
        } else {
            params["context"].clone()
        };

        let handoff = HandoffState {
            summary: summary.to_string(),
            pending,
            context,
        };

        // Write handoff state to the specified path if provided
        if let Some(ref path) = ctx.capabilities.watcher_exit_path {
            let json_content = serde_json::to_string_pretty(&handoff)
                .map_err(|e| RuntimeError::Tool(format!("Failed to serialize handoff: {}", e)))?;
            
            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await
                        .map_err(|e| RuntimeError::Tool(format!("Failed to create directories: {}", e)))?;
                }
            }

            // Atomic write for handoff
            let tmp_path = path.with_extension("tmp");
            tokio::fs::write(&tmp_path, &json_content).await
                .map_err(|e| RuntimeError::Tool(format!("Failed to write handoff temp file: {}", e)))?;
            tokio::fs::rename(&tmp_path, &path).await
                .map_err(|e| RuntimeError::Tool(format!("Failed to rename handoff file: {}", e)))?;
        }

        Ok(format!("Shutdown acknowledged. Handoff saved. Reason: {}", reason))
    }
}