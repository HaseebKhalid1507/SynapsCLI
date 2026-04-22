use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxSendTool;

#[async_trait::async_trait]
impl Tool for TmuxSendTool {
    fn name(&self) -> &str { "tmux_send" }

    fn description(&self) -> &str {
        "Send keys or commands to a tmux pane. Only available in tmux mode. Use to type commands into visible panes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pane_id": {
                    "type": "string",
                    "description": "Target pane ID (e.g. '%5'). Required."
                },
                "keys": {
                    "type": "string",
                    "description": "Keys to send. Use 'Enter' for newline, 'C-c' for Ctrl-C. Required."
                },
                "literal": {
                    "type": "boolean",
                    "description": "Send all keys literally (no key name lookup). Default: true"
                }
            },
            "required": ["pane_id", "keys"]
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
        let keys = params["keys"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing keys parameter".to_string()))?;
        let literal = params["literal"].as_bool().unwrap_or(true);

        let mut args: Vec<&str> = vec!["-t", pane_id];
        if literal {
            args.push("-l");
        }
        args.push(keys);

        controller.execute("send-keys", &args).await
            .map_err(|e| RuntimeError::Tool(format!("tmux send-keys failed: {}", e)))?;

        Ok(json!({ "sent": true, "pane_id": pane_id }).to_string())
    }
}
