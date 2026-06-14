use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, expand_path};

pub struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns lines with line numbers. Reads up to 500 lines by default. For large files, use offset and limit to read in sections."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-indexed, default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all lines)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let raw_path = params["path"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing path parameter".to_string()))?;
        let path = expand_path(raw_path);

        // Read raw bytes first to detect binary files
        let bytes = tokio::fs::read(&path).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to read file '{}': {}", path.display(), e)))?;

        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Err(RuntimeError::Tool(format!(
                "File '{}' appears to be binary (not valid UTF-8). Use `bash` with `xxd` or `file` to inspect binary files.",
                path.display()
            ))),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
        let limit = params["limit"].as_u64().map(|l| l as usize).unwrap_or(500.min(total_lines));

        let start = offset.min(total_lines);
        let end = (start + limit).min(total_lines);

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            result.push_str(&format!("{}\t{}\n", start + i + 1, line));
        }

        if total_lines > end {
            result.push_str(&format!("\n... ({} more lines)", total_lines - end));
        }

        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_read_tool_schema() {
        let tool = ReadTool;
        assert_eq!(tool.name(), "read");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    #[tokio::test]
    async fn test_read_tool_execution() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("read_tool_test.txt");

        // Create temp file with known content
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";
        std::fs::write(&test_file, content).unwrap();

        let tool = ReadTool;
        let ctx = create_tool_context();

        // Test basic read
        let params = json!({
            "path": test_file.to_string_lossy()
        });
        let result = tool.execute(params, ctx).await.unwrap();

        // Verify line numbers and content
        assert!(result.contains("1\tline 1"));
        assert!(result.contains("2\tline 2"));
        assert!(result.contains("5\tline 5"));

        // Test with offset and limit
        let ctx = create_tool_context();
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 2,
            "limit": 2
        });
        let result = tool.execute(params, ctx).await.unwrap();

        assert!(result.contains("3\tline 3"));
        assert!(result.contains("4\tline 4"));
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    async fn test_read_tool_offset() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_read_tool_offset.txt");

        // Write 10 lines
        let lines = (1..=10).map(|i| format!("line {}", i)).collect::<Vec<_>>();
        let content = lines.join("\n");
        std::fs::write(&test_file, &content).unwrap();

        let tool = ReadTool;
        let ctx = create_tool_context();

        // Read with offset=5 (0-indexed, so starts at line 6)
        let params = json!({
            "path": test_file.to_string_lossy(),
            "offset": 5
        });

        let result = tool.execute(params, ctx).await.unwrap();

        // First line shown should be line 6 (1-indexed in output)
        assert!(result.contains("6\tline 6"));
        // Should not contain earlier lines
        assert!(!result.contains("1\tline 1"));
        assert!(!result.contains("5\tline 5"));

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }
}
