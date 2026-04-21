//! SubagentCollectTool — block until a reactive subagent finishes and return its full result.
//!
//! Polls the registry with a short sleep interval until the subagent's status
//! leaves `Running`, or until an optional additional timeout expires. This is
//! the natural pair to `subagent_start` — start async, collect when you need
//! the answer.
//!
//! ## Design note on blocking
//! We use `tokio::time::sleep` between polls rather than a true oneshot channel
//! because the registry is the authoritative state store. A completion-notify
//! channel will be added to `SubagentHandle` in a later step, at which point this
//! tool can switch to `tokio::select!` with zero poll overhead.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};
use crate::tools::subagent_handle::SubagentStatus;


pub struct SubagentCollectTool;

#[async_trait::async_trait]
impl Tool for SubagentCollectTool {
    fn name(&self) -> &str { "subagent_collect" }

    fn description(&self) -> &str {
        "Block until a reactive subagent finishes and return its full output. \
         Optionally supply an additional timeout (seconds) to cap how long to \
         wait beyond the subagent's own timeout. Use after subagent_start when \
         you need the complete result before continuing."
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
