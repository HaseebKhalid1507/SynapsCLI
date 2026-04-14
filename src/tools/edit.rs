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

        let tmp_path = path.with_extension("agent-tmp");
        tokio::fs::write(&tmp_path, &new_content).await
            .map_err(|e| RuntimeError::Tool(format!("Failed to write file: {}", e)))?;
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