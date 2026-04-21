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