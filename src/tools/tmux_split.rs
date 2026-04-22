use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxSplitTool;

#[async_trait::async_trait]
impl Tool for TmuxSplitTool {
    fn name(&self) -> &str { "tmux_split" }

    fn description(&self) -> &str {
        "Create a new tmux pane by splitting the current window. Only available in tmux mode. Use to create visible terminal panes for running commands the user can watch."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": {
                    "type": "string",
                    "description": "Split direction: 'horizontal' (left/right) or 'vertical' (top/bottom). Default: horizontal",
                    "enum": ["horizontal", "vertical"]
                },
                "size": {
                    "type": "string",
                    "description": "Size of new pane as percentage (e.g. '30%') or line count. Default: 50%"
                },
                "target": {
                    "type": "string",
                    "description": "Pane ID to split (e.g. '%0'). Default: current active pane"
                },
                "command": {
                    "type": "string",
                    "description": "Command to run in the new pane"
                },
                "title": {
                    "type": "string",
                    "description": "Title for the new pane"
                },
                "focus": {
                    "type": "boolean",
                    "description": "Switch focus to the new pane. Default: false"
                }
            }
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let controller = ctx.capabilities.tmux_controller
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "tmux mode not active. Launch synaps with --tmux to use tmux tools.".to_string()
            ))?;

        let direction = params["direction"].as_str().unwrap_or("horizontal");
        let size = params["size"].as_str().unwrap_or("50%");
        let target = params["target"].as_str();
        let command = params["command"].as_str();
        let title = params["title"].as_str();
        let focus = params["focus"].as_bool().unwrap_or(false);

        let dir_flag = if direction == "vertical" { "-v" } else { "-h" };

        let mut args: Vec<&str> = vec![dir_flag, "-P", "-F", "#{pane_id}"];

        let size_flag = format!("-l");
        args.push(&size_flag);
        args.push(size);

        if !focus {
            args.push("-d");
        }

        let target_str;
        if let Some(t) = target {
            args.push("-t");
            target_str = t.to_string();
            args.push(&target_str);
        }

        let cmd_str;
        if let Some(cmd) = command {
            cmd_str = cmd.to_string();
            args.push(&cmd_str);
        }

        let result = controller.execute("split-window", &args).await
            .map_err(|e| RuntimeError::Tool(format!("tmux split failed: {}", e)))?;

        let pane_id = result.lines.first()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        if let Some(t) = title {
            let _ = controller.execute("select-pane", &["-t", &pane_id, "-T", t]).await;
        }

        Ok(json!({
            "pane_id": pane_id,
            "direction": direction,
            "size": size,
        }).to_string())
    }
}
