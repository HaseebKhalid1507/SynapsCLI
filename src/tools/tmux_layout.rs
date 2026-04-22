use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct TmuxLayoutTool;

#[async_trait::async_trait]
impl Tool for TmuxLayoutTool {
    fn name(&self) -> &str { "tmux_layout" }

    fn description(&self) -> &str {
        "Change the tmux layout of the current window. Only available in tmux mode. Use to rearrange panes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "preset": {
                    "type": "string",
                    "description": "Layout preset: 'split', 'fullscreen', 'tiled', or 'even-horizontal', 'even-vertical', 'main-horizontal', 'main-vertical'",
                    "enum": ["split", "fullscreen", "tiled", "even-horizontal", "even-vertical", "main-horizontal", "main-vertical"]
                },
                "set_default": {
                    "type": "boolean",
                    "description": "Persist as the default layout. Default: false"
                },
                "scope": {
                    "type": "string",
                    "description": "Where to persist if set_default=true: 'project' or 'system'",
                    "enum": ["project", "system"]
                }
            },
            "required": ["preset"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let controller = ctx.capabilities.tmux_controller
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "tmux mode not active. Launch synaps with --tmux to use tmux tools.".to_string()
            ))?;

        let preset = params["preset"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing preset parameter".to_string()))?;
        let set_default = params["set_default"].as_bool().unwrap_or(false);
        let scope = params["scope"].as_str().unwrap_or("project");

        // Map preset names to tmux layout commands
        match preset {
            "split" | "fullscreen" | "tiled" => {
                let layout_name = match preset {
                    "tiled" => "tiled",
                    "split" => "main-vertical",
                    _ => "even-horizontal", // fullscreen handled differently
                };
                if preset != "fullscreen" {
                    controller.execute("select-layout", &[layout_name]).await
                        .map_err(|e| RuntimeError::Tool(format!("tmux select-layout failed: {}", e)))?;
                }
            }
            layout => {
                controller.execute("select-layout", &[layout]).await
                    .map_err(|e| RuntimeError::Tool(format!("tmux select-layout failed: {}", e)))?;
            }
        }

        // Persist default if requested
        if set_default {
            let config_key = "tmux.default_layout";
            let _ = crate::config::write_config_value(config_key, preset);
        }

        Ok(json!({
            "layout": preset,
            "applied": true,
            "persisted": set_default,
            "scope": scope,
        }).to_string())
    }
}
