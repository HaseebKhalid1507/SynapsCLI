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
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_find_tool_schema() {
        let tool = FindTool;
        assert_eq!(tool.name(), "find");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_find_tool_execution() {
        let temp_dir = std::env::temp_dir().join("test_find_tool_execution");
        std::fs::create_dir_all(&temp_dir).unwrap();

        let test_file = temp_dir.join("test_find_me.txt");
        std::fs::write(&test_file, "test content").unwrap();

        let tool = FindTool;
        let ctx = create_tool_context();

        let params = json!({
            "pattern": "test_find_me*",
            "path": temp_dir.to_string_lossy()
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // Should contain the filename
        assert!(result.contains("test_find_me.txt"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
