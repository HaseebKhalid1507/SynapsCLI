use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxCaptureTool;

#[async_trait::async_trait]
impl Tool for TmuxCaptureTool {
    fn name(&self) -> &str { "tmux_capture" }

    fn description(&self) -> &str {
        "Read/capture the content of a tmux pane. Only available in tmux mode. Use to see what's displayed in a visible pane."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pane_id": {
                    "type": "string",
                    "description": "Target pane ID (e.g. '%5'). Required."
                },
                "start_line": {
                    "type": "integer",
                    "description": "Start line (negative = history). Default: visible area start"
                },
                "end_line": {
                    "type": "integer",
                    "description": "End line. Default: visible area end"
                }
            },
            "required": ["pane_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let controller = ctx.capabilities.tmux_controller
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "tmux mode not active. Launch synaps with --tmux to use tmux tools.".to_string()
            ))?;

        let pane_id = params["pane_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing pane_id parameter".to_string()))?;

        let mut args: Vec<String> = vec![
            "-t".to_string(), pane_id.to_string(), "-p".to_string(),
        ];

        if let Some(start) = params["start_line"].as_i64() {
            args.push("-S".to_string());
            args.push(start.to_string());
        }
        if let Some(end) = params["end_line"].as_i64() {
            args.push("-E".to_string());
            args.push(end.to_string());
        }

        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let result = controller.execute("capture-pane", &args_ref).await
            .map_err(|e| RuntimeError::Tool(format!("tmux capture-pane failed: {}", e)))?;

        let content = result.lines.join("\n");

        Ok(json!({
            "pane_id": pane_id,
            "content": content,
        }).to_string())
    }
}
