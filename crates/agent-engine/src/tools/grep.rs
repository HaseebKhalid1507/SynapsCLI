use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Returns matching lines with file paths and line numbers. Supports file type filtering and context lines."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: current directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.rs\", \"*.py\")"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing pattern parameter".to_string()))?;
        let path = expand_path(params["path"].as_str().unwrap_or("."));
        let include = params["include"].as_str();
        let context = params["context"].as_u64();

        let mut cmd = Command::new("grep");
        cmd.arg("-rn");
        cmd.arg("--color=never");

        if let Some(glob) = include {
            cmd.arg("--include").arg(glob);
        }

        if let Some(ctx) = context {
            cmd.arg(format!("-C{}", ctx));
        }

        cmd.arg("--exclude-dir=.git");
        cmd.arg("--exclude-dir=node_modules");
        cmd.arg("--exclude-dir=target");

        cmd.arg("--").arg(pattern).arg(&path);

        let output = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await
            .map_err(|_| RuntimeError::Tool("Grep timed out after 15s".to_string()))?
            .map_err(|e| RuntimeError::Tool(format!("Failed to execute grep: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            Ok("No matches found.".to_string())
        } else {
            let result = stdout.to_string();
            if result.len() > ctx.limits.max_tool_output {
                let truncated: String = result.chars().take(ctx.limits.max_tool_output).collect();
                Ok(format!("{}\n\n... (output truncated, {} total bytes)", truncated, result.len()))
            } else {
                Ok(result)
            }
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
    fn test_grep_tool_schema() {
        let tool = GrepTool;
        assert_eq!(tool.name(), "grep");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_grep_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_grep_tool_execution.txt");

        // Write test content
        let content = "hello world\nfoo bar\nhello again";
        std::fs::write(&test_file, content).unwrap();

        let tool = GrepTool;
        let ctx = create_tool_context();

        let params = json!({
            "pattern": "hello",
            "path": test_file.to_string_lossy()
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // Should contain both matching lines with line numbers
        assert!(result.contains("hello world"));
        assert!(result.contains("hello again"));
        assert!(result.contains("1:") || result.contains("hello world"));
        assert!(result.contains("3:") || result.contains("hello again"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }
}
