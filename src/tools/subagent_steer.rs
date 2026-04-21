//! SubagentSteerTool — inject a guidance message into a running reactive subagent.
//!
//! Sends a message over the subagent's steering channel. The message is surfaced
//! to the subagent's event loop as additional context mid-run, allowing the
//! orchestrating agent to correct course without killing and restarting.
//!
//! Fails (non-fatally) if the subagent has already finished or the steering
//! channel is unavailable — the caller receives a structured error payload.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};
use crate::runtime::subagent::SubagentStatus;

pub struct SubagentSteerTool;

#[async_trait::async_trait]
impl Tool for SubagentSteerTool {
    fn name(&self) -> &str { "subagent_steer" }

    fn description(&self) -> &str {
        "Inject a guidance message into a running reactive subagent. Use this to \
         correct course, provide new context, or impose constraints mid-run without \
         stopping the subagent. Returns {\"acknowledged\": true} on success or an \
         error payload if the subagent is no longer running."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID returned by subagent_start (e.g. \"sa_3\")."
                },
                "message": {
                    "type": "string",
                    "description": "Guidance message to inject into the subagent's context. \
                                    Keep it concise — the subagent sees this as a mid-run \
                                    user message."
                }
            },
            "required": ["handle_id", "message"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let message = params["message"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'message' parameter".to_string()))?
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

        // ── Guard: only steer a running subagent ───────────────────────────────

        if handle.status() != SubagentStatus::Running {
            return Ok(json!({
                "acknowledged": false,
                "error": format!(
                    "Subagent '{}' is '{}' — steering is only possible while running.",
                    handle_id,
                    handle.status().as_str()
                )
            }).to_string());
        }

        // ── Send over steering channel ─────────────────────────────────────────
        //
        // The `steer_tx` is connected by the background task in subagent_start.
        // Until that wiring is done the channel will be None — return a clear
        // stub response so callers know the shape of the success path.

        match handle.steer(&message) {
            Ok(()) => Ok(json!({ "acknowledged": true }).to_string()),
            Err(e) => {
                tracing::debug!(
                    "subagent_steer: steer failed for handle '{}': {}",
                    handle_id, e
                );
                Ok(json!({
                    "acknowledged": false,
                    "error": e
                }).to_string())
            }
        }
    }
}
