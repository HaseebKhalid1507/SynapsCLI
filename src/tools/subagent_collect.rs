//! SubagentCollectTool — check if a reactive subagent is done and return its result.
//!
//! Non-blocking — checks the registry once and returns immediately.
//! If the subagent is still running, returns status + partial output.
//! If done, returns the full result. The natural pair to `subagent_start` —
//! start async, check when you want the answer.
//!

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};
use crate::runtime::subagent::SubagentStatus;


pub struct SubagentCollectTool;

#[async_trait::async_trait]
impl Tool for SubagentCollectTool {
    fn name(&self) -> &str { "subagent_collect" }

    fn description(&self) -> &str {
        "Check if a reactive subagent is done and return its result. Non-blocking — \
         returns immediately. If still running, returns status and partial output. \
         If finished, returns the full result. Call repeatedly to poll for completion."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Additional seconds to wait beyond the subagent's own \
                                    timeout before giving up. Optional — defaults to waiting \
                                    indefinitely (bounded only by the subagent's own timeout)."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let registry = ctx.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?;

        let reg = registry.lock().unwrap();
        let handle = reg.get(&handle_id)
            .ok_or_else(|| RuntimeError::Tool(
                format!("No subagent found with handle_id '{}'", handle_id)
            ))?;

        let status = handle.status();
        let output = handle.partial_output();

        if status == SubagentStatus::Running {
            // Still going — return current state, don't block
            return Ok(json!({
                "handle_id":    handle_id,
                "status":       "running",
                "elapsed_secs": (handle.elapsed_secs() * 10.0).round() / 10.0,
                "output_so_far": if output.chars().count() > 500 {
                    output.chars().skip(output.chars().count() - 500).collect::<String>()
                } else {
                    output.clone()
                }
            }).to_string());
        }

        // Done — return full result
        Ok(json!({
            "handle_id": handle_id,
            "status":    status.as_str(),
            "output":    output
        }).to_string())
    }
}
