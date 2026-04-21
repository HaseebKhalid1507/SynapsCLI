//! SubagentResumeTool — restart a finished or timed-out subagent with new instructions.
//!
//! Takes the completed conversation state stored in `SubagentHandle::final_context`,
//! prepends new instructions, and dispatches a fresh subagent via the same flow as
//! `subagent_start`. The caller gets a new `handle_id` for the continuation run.
//!
//! ## Use cases
//! - A subagent timed out mid-task: resume with "continue from where you left off"
//! - A finished subagent needs a follow-up pass: resume with new refinement instructions
//! - A failed subagent needs retry with corrected context
//!
//! ## Stub note
//! The actual spawn (thread + runtime) is stubbed, matching `subagent_start.rs`.
//! Wiring happens in the same step that finalises background task spawn logic.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext};
use crate::runtime::subagent::SubagentStatus;

pub struct SubagentResumeTool;

#[async_trait::async_trait]
impl Tool for SubagentResumeTool {
    fn name(&self) -> &str { "subagent_resume" }

    fn description(&self) -> &str {
        "Resume a finished or timed-out reactive subagent with new instructions. \
         The previous subagent's conversation state is prepended as context so the \
         new run has full history. Returns a new handle_id — the original handle \
         remains readable for comparison. Only works on subagents in \
         finished/timed_out/failed state."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID of the completed subagent to resume \
                                    (e.g. \"sa_3\")."
                },
                "instructions": {
                    "type": "string",
                    "description": "New task or context to prepend to the resumed subagent. \
                                    This is injected before the prior conversation history, \
                                    so the new run sees: instructions → prior context → continues."
                }
            },
            "required": ["handle_id", "instructions"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let instructions = params["instructions"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'instructions' parameter".to_string()))?
            .to_string();

        // ── Registry lookup ────────────────────────────────────────────────────

        let registry = ctx.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?;

        // Extract what we need under the lock, then release immediately.
        let (agent_name, prior_context) = {
            let reg = registry.lock().unwrap();
            let handle = reg.get(&handle_id)
                .ok_or_else(|| RuntimeError::Tool(
                    format!("No subagent found with handle_id '{}'", handle_id)
                ))?;

            // Only resumable if the subagent has stopped running.
            if handle.status() == SubagentStatus::Running {
                return Err(RuntimeError::Tool(format!(
                    "Subagent '{}' is still running. \
                     Call subagent_collect first, or wait until it finishes.",
                    handle_id
                )));
            }

            (
                handle.agent_name.clone(),
                {
                    let state = handle.conversation_state();
                    if state.is_empty() {
                        handle.partial_output()
                    } else {
                        serde_json::to_string(&state).unwrap_or_else(|_| handle.partial_output())
                    }
                },
            )
        };

        // ── Build resumed task ─────────────────────────────────────────────────
        //
        // Structure: new instructions → separator → prior conversation context.
        // The subagent runtime sees this as a single task string, giving it full
        // continuity without requiring multi-turn session state.

        let resumed_task = format!(
            "{instructions}\n\n\
             ---\n\
             [Prior conversation context from handle {handle_id}]\n\
             {prior_context}"
        );

        // ── Allocate new handle ────────────────────────────────────────────────

        // ── STUB: allocate placeholder new handle_id ───────────────────────────
        let new_handle_id = {
            use std::sync::atomic::Ordering;
            let n = super::NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
            let _ = &registry; // suppress unused until spawn is wired
            format!("sa_stub_{}", n)
        };

        // ── TODO (next step): spawn background task ────────────────────────────
        //
        // Identical to subagent_start.rs spawn path, but using `resumed_task`
        // as the task string. The `final_context` from the prior handle provides
        // the system-prompt-level history prepend.
        //
        // Will be wired in the same pass as subagent_start spawn logic.

        let _ = resumed_task; // suppress unused warning until spawn is wired

        tracing::info!(
            "subagent_resume: stub — '{}' resumed from '{}' → new handle '{}'",
            agent_name, handle_id, new_handle_id
        );

        Ok(json!({
            "handle_id":       new_handle_id,
            "resumed_from":    handle_id,
            "agent_name":      agent_name,
            "status":          "running"
        }).to_string())
    }
}
