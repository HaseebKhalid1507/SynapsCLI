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
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_ls_tool_schema() {
        let tool = LsTool;
        assert_eq!(tool.name(), "ls");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_ls_tool_execution() {
        let tool = LsTool;
        let ctx = create_tool_context();

        // Use a dedicated temp dir to avoid races with other tests
        let dir = std::env::temp_dir().join("synaps_test_ls");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hi").unwrap();

        let params = json!({
            "path": dir.to_str().unwrap()
        });

        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("hello.txt"));

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }
}
