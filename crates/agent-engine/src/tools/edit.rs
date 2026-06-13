use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct EditTool;

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }

    fn description(&self) -> &str {
        "Make a surgical edit to a file by replacing an exact string match. The old_string must appear exactly once in the file. Provide enough surrounding context to make the match unique."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find and replace. Must match exactly once in the file."
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let old_string = params["old_string"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing old_string parameter".to_string()))?;
        let new_string = params["new_string"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing new_string parameter".to_string()))?;

        let path = expand_path(raw_path);

        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e)))?;

        let count = content.matches(old_string).count();

        if count == 0 {
            return Err(RuntimeError::Tool(format!(
                "old_string not found in '{}'. Make sure it matches exactly, including whitespace and indentation.",
                path.display()
            )));
        }

        if count > 1 {
            return Err(RuntimeError::Tool(format!(
                "old_string found {} times in '{}'. It must be unique — include more surrounding context.",
                count, path.display()
            )));
        }

        let new_content = content.replacen(old_string, new_string, 1);

        // Preserve original file permissions (executable bits, etc.)
        let original_perms = tokio::fs::metadata(&path).await
            .map(|m| m.permissions())
            .ok();

        let tmp_path = path.with_extension("agent-tmp");
        tokio::fs::write(&tmp_path, &new_content).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;

        // Restore original permissions on the temp file before rename
        if let Some(perms) = original_perms {
            let _ = tokio::fs::set_permissions(&tmp_path, perms).await;
        }

        tokio::fs::rename(&tmp_path, &path).await
            .map_err(|e| {
                let tmp = tmp_path.clone();
                tokio::spawn(async move { let _ = tokio::fs::remove_file(tmp).await; });
                RuntimeError::Tool(format!("Failed to finalize edit: {}", e))
            })?;

        let old_lines: Vec<&str> = old_string.lines().collect();
        let new_lines: Vec<&str> = new_string.lines().collect();
        Ok(format!(
            "Edited {} — replaced {} line(s) with {} line(s)",
            path.display(), old_lines.len(), new_lines.len()
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_edit_tool_schema() {
        let tool = EditTool;
        assert_eq!(tool.name(), "edit");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_edit_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("edit_tool_test.txt");

        // Create file with known content
        let initial_content = "Hello world\nThis is a test\nEnd of file";
        std::fs::write(&test_file, initial_content).unwrap();

        let tool = EditTool;

        // Test successful replacement
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "This is a test",
            "new_string": "This is modified"
        });

        let result = tool.execute(params, ctx).await.unwrap();
        assert!(result.contains("Edited"));
        assert!(result.contains("replaced 1 line(s) with 1 line(s)"));

        let modified_content = std::fs::read_to_string(&test_file).unwrap();
        assert!(modified_content.contains("This is modified"));
        assert!(!modified_content.contains("This is a test"));

        // Test old_string not found
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "nonexistent string",
            "new_string": "replacement"
        });

        let result = tool.execute(params, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("old_string not found"));

        // Test old_string found multiple times
        std::fs::write(&test_file, "test\ntest\nother").unwrap();
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "test",
            "new_string": "replacement"
        });

        let result = tool.execute(params, ctx).await;
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("found 2 times"));
        assert!(error_msg.contains("must be unique"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_edit_tool_no_match() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_edit_tool_no_match.txt");

        // Create file with known content
        let content = "some content\nmore content";
        std::fs::write(&test_file, content).unwrap();

        let tool = EditTool;
        let ctx = create_tool_context();

        let params = json!({
            "path": test_file.to_string_lossy(),
            "old_string": "this string does not exist",
            "new_string": "replacement"
        });

        let result = tool.execute(params, ctx).await;

        // Should return error about string not found
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("old_string not found"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }
}
