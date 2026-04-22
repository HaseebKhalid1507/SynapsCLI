use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxResizeTool;

#[async_trait::async_trait]
impl Tool for TmuxResizeTool {
    fn name(&self) -> &str { "tmux_resize" }

    fn description(&self) -> &str {
        "Resize a tmux pane or toggle zoom. Only available in tmux mode."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pane_id": {
                    "type": "string",
                    "description": "Target pane ID (e.g. '%5'). Default: current pane"
                },
                "width": {
                    "type": "string",
                    "description": "Width: absolute number or relative '+10'/'-10'"
                },
                "height": {
                    "type": "string",
                    "description": "Height: absolute number or relative '+10'/'-10'"
                },
                "zoom": {
                    "type": "boolean",
                    "description": "Toggle zoom (pane fills entire window). Default: false"
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

        let pane_id = params["pane_id"].as_str();
        let width = params["width"].as_str();
        let height = params["height"].as_str();
        let zoom = params["zoom"].as_bool().unwrap_or(false);

        if zoom {
            let mut args = vec!["-Z"];
            let pane_str;
            if let Some(p) = pane_id {
                args.push("-t");
                pane_str = p.to_string();
                args.push(&pane_str);
            }
            controller.execute("resize-pane", &args).await
                .map_err(|e| RuntimeError::Tool(format!("resize-pane zoom failed: {}", e)))?;
            return Ok(json!({ "zoomed": true }).to_string());
        }

        let mut results = vec![];

        if let Some(w) = width {
            let mut args = vec!["-x", w];
            let pane_str;
            if let Some(p) = pane_id {
                args.push("-t");
                pane_str = p.to_string();
                args.push(&pane_str);
            }
            controller.execute("resize-pane", &args).await
                .map_err(|e| RuntimeError::Tool(format!("resize-pane width failed: {}", e)))?;
            results.push(format!("width={}", w));
        }

        if let Some(h) = height {
            let mut args = vec!["-y", h];
            let pane_str;
            if let Some(p) = pane_id {
                args.push("-t");
                pane_str = p.to_string();
                args.push(&pane_str);
            }
            controller.execute("resize-pane", &args).await
                .map_err(|e| RuntimeError::Tool(format!("resize-pane height failed: {}", e)))?;
            results.push(format!("height={}", h));
        }

        Ok(json!({
            "resized": true,
            "changes": results,
        }).to_string())
    }
}
