use serde_json::{json, Value};
use tokio::process::Command;
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct LsTool;

#[async_trait::async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str { "ls" }

    fn description(&self) -> &str {
        "List directory contents with details (permissions, size, modification date). Defaults to current directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let path = expand_path(params["path"].as_str().unwrap_or("."));

        let output = Command::new("ls")
            .arg("-lah")
            .arg(&path)
            .output()
            .await
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute ls: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            Err(RuntimeError::Tool(format!("ls failed: {}", stderr)))
        } else if stdout.is_empty() {
            Ok("Directory is empty.".to_string())
        } else {
            Ok(stdout.to_string())
        }
    }
}