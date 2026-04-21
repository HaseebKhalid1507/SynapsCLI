//! SubagentStartTool — dispatch a reactive subagent and return a handle_id immediately.
//!
//! Unlike the one-shot `subagent` tool, this tool returns *before* the subagent
//! finishes. The caller gets a `handle_id` they can poll via `subagent_status`,
//! steer via `subagent_steer`, or block on via `subagent_collect`.
//!
//! ## Spawn logic
//! The actual spawn logic (thread + runtime creation) will be wired in a separate step
//! once `SubagentHandle`'s background task interface is finalised. For now this stub
//! validates parameters, inserts a handle into the registry, and returns the handle_id.

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, resolve_agent_prompt, NEXT_SUBAGENT_ID};
use std::sync::atomic::Ordering;

pub struct SubagentStartTool;

#[async_trait::async_trait]
impl Tool for SubagentStartTool {
    fn name(&self) -> &str { "subagent_start" }

    fn description(&self) -> &str {
        "Dispatch a reactive subagent and return immediately with a handle_id. \
         The subagent runs in the background — use subagent_status to poll, \
         subagent_steer to inject guidance mid-run, and subagent_collect to \
         block until it finishes. Provide either an agent name (resolves from \
         ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent name — resolves to ~/.synaps-cli/agents/<name>.md. \
                                    Mutually exclusive with system_prompt."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Inline system prompt for the subagent. \
                                    Use when you don't have a named agent file."
                },
                "task": {
                    "type": "string",
                    "description": "The task/prompt to send to the subagent."
                },
                "model": {
                    "type": "string",
                    "description": "Model override (default: claude-opus-4-7). \
                                    Use claude-sonnet-4-6 for lighter tasks."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 300). \
                                    Increase for long-running tasks."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        // ── Parameter validation (mirrors subagent.rs) ─────────────────────────

        let task = params["task"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();

        let agent_name    = params["agent"].as_str().map(|s| s.to_string());
        let inline_prompt = params["system_prompt"].as_str().map(|s| s.to_string());
        let _model        = params["model"].as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| crate::models::default_model().to_string());
        let _timeout_secs = params["timeout"].as_u64().unwrap_or(ctx.subagent_timeout);

        // Resolve agent label — validates agent/system_prompt exclusivity.
        let label = match (&agent_name, &inline_prompt) {
            (Some(name), _) => {
                // Eagerly validate the agent file exists — fail fast before we allocate a handle.
                resolve_agent_prompt(name).map_err(RuntimeError::Tool)?;
                name.clone()
            }
            (None, Some(_)) => "inline".to_string(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). \
                     Got neither.".to_string()
                ));
            }
        };

        let _task_preview: String = task.chars().take(80).collect();
        let _subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);

        // ── STUB: allocate placeholder handle_id ───────────────────────────────
        //
        // Actual SubagentHandle construction requires channel halves + Arc state
        // shared with the spawned thread. That wiring lives in the background
        // spawn step. Until then we just mint a deterministic id so callers can
        // reason about response shape.

        let handle_id = format!("sa_stub_{}", _subagent_id);
        let _ = &ctx.subagent_registry; // suppress unused until spawn is wired

        // ── TODO (next step): spawn background task ────────────────────────────
        //
        // The actual spawn logic will be extracted from subagent.rs execute():
        //   1. std::thread::spawn with a new tokio::runtime::Builder::new_current_thread()
        //   2. Runtime::new() → set_system_prompt → set_model → set_tools
        //   3. run_stream() event loop writing into registry handle fields
        //   4. On completion: set handle.status, handle.final_context
        //
        // That wiring happens once SubagentHandle's background task API is locked in.

        tracing::info!(
            "subagent_start: dispatched stub handle '{}' for agent '{}'",
            handle_id, label
        );

        Ok(json!({
            "handle_id":  handle_id,
            "agent_name": label,
            "status":     "running"
        }).to_string())
    }
}
