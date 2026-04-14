use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct FindTool;

#[async_trait::async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str { "find" }

    fn description(&self) -> &str {
        "Find files by name using glob patterns. Searches recursively from the given path. Excludes .git directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match file names (e.g. \"*.rs\", \"Cargo.*\")"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "type": {
                    "type": "string",
                    "description": "Filter by type: \"f\" for files, \"d\" for directories"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
        let path = expand_path(params["path"].as_str().unwrap_or("."));
        let file_type = params["type"].as_str();

        let mut cmd = Command::new("find");
        cmd.arg(&path);

        cmd.args(["-not", "-path", "*/.git/*"]);
        cmd.args(["-not", "-path", "*/node_modules/*"]);
        cmd.args(["-not", "-path", "*/target/*"]);

        if let Some(t) = file_type {
            cmd.arg("-type").arg(t);
        }

        cmd.arg("-name").arg(pattern);

        let output = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await
            .map_err(|_| RuntimeError::Tool("Find timed out after 10s".to_string()))?
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute find: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            Ok("No files found.".to_string())
        } else {
            Ok(stdout.trim().to_string())
        }
    }
}