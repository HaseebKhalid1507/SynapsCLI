use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxWindowTool;

#[async_trait::async_trait]
impl Tool for TmuxWindowTool {
    fn name(&self) -> &str { "tmux_window" }

    fn description(&self) -> &str {
        "Create, switch, close, or rename tmux windows (tabs). Only available in tmux mode."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: 'create', 'switch', 'close', 'rename'",
                    "enum": ["create", "switch", "close", "rename"]
                },
                "name": {
                    "type": "string",
                    "description": "Window name (for create/rename)"
                },
                "target": {
                    "type": "string",
                    "description": "Target window ID or index (for switch/close/rename)"
                },
                "command": {
                    "type": "string",
                    "description": "Command to run in new window (for create)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let controller = ctx.capabilities.tmux_controller
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "tmux mode not active. Launch synaps with --tmux to use tmux tools.".to_string()
            ))?;

        let action = params["action"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing action parameter".to_string()))?;
        let name = params["name"].as_str();
        let target = params["target"].as_str();
        let command = params["command"].as_str();

        match action {
            "create" => {
                let mut args: Vec<&str> = vec!["-P", "-F", "#{window_id}"];
                if let Some(n) = name {
                    args.push("-n");
                    args.push(n);
                }
                if let Some(cmd) = command {
                    args.push(cmd);
                }
                let result = controller.execute("new-window", &args).await
                    .map_err(|e| RuntimeError::Tool(format!("new-window failed: {}", e)))?;
                let window_id = result.lines.first()
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                Ok(json!({ "action": "create", "window_id": window_id }).to_string())
            }
            "switch" => {
                let t = target.ok_or_else(|| RuntimeError::Tool("Missing target for switch".to_string()))?;
                controller.execute("select-window", &["-t", t]).await
                    .map_err(|e| RuntimeError::Tool(format!("select-window failed: {}", e)))?;
                Ok(json!({ "action": "switch", "target": t }).to_string())
            }
            "close" => {
                let t = target.ok_or_else(|| RuntimeError::Tool("Missing target for close".to_string()))?;
                controller.execute("kill-window", &["-t", t]).await
                    .map_err(|e| RuntimeError::Tool(format!("kill-window failed: {}", e)))?;
                Ok(json!({ "action": "close", "target": t }).to_string())
            }
            "rename" => {
                let t = target.ok_or_else(|| RuntimeError::Tool("Missing target for rename".to_string()))?;
                let n = name.ok_or_else(|| RuntimeError::Tool("Missing name for rename".to_string()))?;
                controller.execute("rename-window", &["-t", t, n]).await
                    .map_err(|e| RuntimeError::Tool(format!("rename-window failed: {}", e)))?;
                Ok(json!({ "action": "rename", "target": t, "name": n }).to_string())
            }
            other => Err(RuntimeError::Tool(format!("Unknown action: {}", other))),
        }
    }
}
