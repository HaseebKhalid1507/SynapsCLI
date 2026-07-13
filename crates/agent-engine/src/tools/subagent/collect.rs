//! SubagentCollectTool — check if a reactive subagent is done and return its result.
//!
//! Non-blocking — checks the registry once and returns immediately.
//! If the subagent is still running, returns status + partial output.
//! If done, returns the full result. The natural pair to `subagent_start` —
//! start async, check when you want the answer.
//!

use serde_json::{json, Value};
use crate::{Result, RuntimeError};
use super::super::{Tool, ToolContext};
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
                "reconciled": {
                    "type": "boolean",
                    "default": false,
                    "description": "Confirm the collected result was inspected and reconciled with foreground work."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let registry = ctx.capabilities.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?;

        let mut reg = registry.lock().unwrap();
        let handle = reg.get_mut(&handle_id)
            .ok_or_else(|| RuntimeError::Tool(
                format!(
                    "No subagent found with handle_id '{}'. \
                     Finished handles are retained for {} minutes after completion \
                     and then garbage-collected.",
                    handle_id,
                    crate::runtime::subagent::FINISHED_HANDLE_TTL.as_secs() / 60
                )
            ))?;

        let status = handle.status();
        let output: String = handle.partial_output();
        let elapsed = handle.elapsed_secs();

        // Mark collected on any terminal read so the reaper knows it's safe to GC.
        let already_collected = handle.is_collected();
        if status != SubagentStatus::Running {
            handle.mark_collected();
        }
        drop(reg);

        if status == SubagentStatus::Running {
            // Still going — return current state, don't block
            let char_count = output.chars().count();
            let output_so_far: String = if char_count > 500 {
                output.chars().skip(char_count - 500).collect()
            } else {
                output
            };
            return Ok(json!({
                "handle_id":    handle_id,
                "status":       "running",
                "elapsed_secs": (elapsed * 10.0).round() / 10.0,
                "output_so_far": output_so_far
            }).to_string());
        }

        if let Some(orchestration) = &ctx.capabilities.orchestration {
            let terminal = match status {
                SubagentStatus::Completed => agent_core::orchestration::WorkerTerminal::Completed,
                SubagentStatus::TimedOut => agent_core::orchestration::WorkerTerminal::TimedOut,
                _ => agent_core::orchestration::WorkerTerminal::Failed,
            };
            orchestration
                .terminal_and_collect(&handle_id, terminal)
                .map_err(RuntimeError::Tool)?;
            if params["reconciled"].as_bool().unwrap_or(false) {
                orchestration
                    .reconcile(&handle_id)
                    .map_err(RuntimeError::Tool)?;
            }
        }

        // Done — return full result including collected flag for idempotency signaling
        let mut body = json!({
            "handle_id": handle_id,
            "status":    status.as_str(),
            "output":    output,
            "collected": already_collected,
        });
        if let Some(reason) = status.failure_reason() {
            body["error"] = json!(reason);
        }
        Ok(body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use crate::runtime::subagent::{SubagentRegistry, SubagentHandle, SubagentState, SubagentStatus};
    use serde_json::json;
    use std::sync::{Arc, Mutex, RwLock};
    use tokio::sync::{mpsc, oneshot};

    fn make_finished_handle(id: &str, output: &str) -> SubagentHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.partial_text = output.to_string();
            s.finished_at = Some(std::time::Instant::now());
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        SubagentHandle::new(
            id.to_string(),
            0,
            "test-agent".to_string(),
            "test task".to_string(),
            "claude-sonnet-4-6".to_string(),
            "system prompt".to_string(),
            300,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    fn make_ctx_with_registry(
        registry: Arc<Mutex<SubagentRegistry>>,
    ) -> crate::tools::ToolContext {
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry);
        ctx
    }

    // U5: collect marks collected on first read; second read shows collected=true
    #[tokio::test]
    async fn collect_marks_collected_and_is_idempotent() {
        let tool = SubagentCollectTool;

        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        let handle = make_finished_handle("sa_42", "result text");
        registry.lock().unwrap().register(handle);

        // First collect — should return collected=false (not yet collected before this call)
        let result1 = tool.execute(
            json!({"handle_id": "sa_42"}),
            make_ctx_with_registry(registry.clone()),
        ).await.unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["status"], "completed");
        assert_eq!(body1["output"], "result text");
        assert_eq!(body1["collected"], false, "first collect should report collected=false");

        // Second collect — same handle still in registry (not yet reaped), collected=true
        let result2 = tool.execute(
            json!({"handle_id": "sa_42"}),
            make_ctx_with_registry(registry.clone()),
        ).await.unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["status"], "completed");
        assert_eq!(body2["collected"], true, "second collect should report collected=true");

        // After collect, the registry should have the handle marked collected
        let reg = registry.lock().unwrap();
        let h = reg.get("sa_42").unwrap();
        assert!(h.is_collected(), "handle must be marked collected in registry");
    }
}
