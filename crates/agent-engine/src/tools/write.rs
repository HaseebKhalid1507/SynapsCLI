use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct WriteTool;

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates parent directories if needed. Use this for creating new files or completely rewriting existing ones."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let content = params["content"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing content parameter".to_string()))?;

        let path = expand_path(raw_path);

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| RuntimeError::Tool(format!("Failed to create directories: {}", e)))?;
            }
        }

        // Preserve permissions if overwriting an existing file
        let original_perms = tokio::fs::metadata(&path).await
            .map(|m| m.permissions())
            .ok();

        let tmp_path = path.with_extension("agent-tmp");
        tokio::fs::write(&tmp_path, content).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;

        if let Some(perms) = original_perms {
            let _ = tokio::fs::set_permissions(&tmp_path, perms).await;
        }

        tokio::fs::rename(&tmp_path, &path).await
            .map_err(|e| {
                let tmp = tmp_path.clone();
                tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
                RuntimeError::Tool(format!("Failed to finalize write: {}", e))
            })?;

        let line_count = content.lines().count();
        Ok(format!("Wrote {} lines ({} bytes) to {}", line_count, content.len(), path.display()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_write_tool_schema() {
        let tool = WriteTool;
        assert_eq!(tool.name(), "write");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_write_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("write_tool_test.txt");

        let tool = WriteTool;
        let ctx = create_tool_context();

        let content = "Hello, world!\nThis is a test file.";
        let params = json!({
            "path": test_file.to_string_lossy(),
            "content": content
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // Verify success message
        assert!(result.contains("Wrote 2 lines"));
        assert!(result.contains("bytes"));

        // Verify file was created with correct content
        let written_content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(written_content, content);

        // Test parent directory creation
        let nested_file = temp_dir.join("nested").join("dir").join("test.txt");
        let ctx = create_tool_context();
        let params = json!({
            "path": nested_file.to_string_lossy(),
            "content": "nested content"
        });

        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("Wrote"));

        let nested_content = std::fs::read_to_string(&nested_file).unwrap();
        assert_eq!(nested_content, "nested content");

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
        let _ = std::fs::remove_dir_all(temp_dir.join("nested"));
    }
}
