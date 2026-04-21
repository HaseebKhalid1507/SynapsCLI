//! SubagentStatusTool — poll the current state of a running or completed reactive subagent.
//!
//! Returns a lightweight snapshot: lifecycle status, last 500 chars of output,
//! elapsed wall-clock seconds, and tool-use count. Non-blocking — always returns
//! immediately regardless of the subagent's progress.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};

pub struct SubagentStatusTool;

#[async_trait::async_trait]
impl Tool for SubagentStatusTool {
    fn name(&self) -> &str { "subagent_status" }

    fn description(&self) -> &str {
        "Poll the current state of a reactive subagent. Returns status \
         (running/finished/timed_out/failed), the last 500 characters of output \
         produced so far, elapsed time in seconds, and the number of tool calls \
         made. Non-blocking — returns immediately."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        // ── Registry lookup ────────────────────────────────────────────────────

        let registry = ctx.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?;

        let reg = registry.lock().unwrap();

        let handle = reg.get(&handle_id)
            .ok_or_else(|| RuntimeError::Tool(
                format!("No subagent found with handle_id '{}'", handle_id)
            ))?;

        // ── Build response ─────────────────────────────────────────────────────

        let full = handle.partial_output();
        let partial_output: String = if full.chars().count() > 500 {
            full.chars().skip(full.chars().count() - 500).collect()
        } else {
            full
        };

        Ok(json!({
            "handle_id":      handle_id,
            "agent_name":     handle.agent_name,
            "status":         handle.status().as_str(),
            "partial_output": partial_output,
            "elapsed_secs":   (handle.elapsed_secs() * 10.0).round() / 10.0,
            "tool_count":     handle.tool_log().len()
        }).to_string())
    }
}
