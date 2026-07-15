//! SubagentCollectTool — check if a reactive subagent is done and return its result.
//!
//! Non-blocking — checks the registry once and returns immediately.
//! If the subagent is still running, returns status + partial output.
//! If done, returns the full result. The natural pair to `subagent_start` —
//! start async, check when you want the answer.
//!

use super::super::{Tool, ToolContext};
use crate::runtime::subagent::SubagentStatus;
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentCollectTool;

#[async_trait::async_trait]
impl Tool for SubagentCollectTool {
    fn name(&self) -> &str {
        "subagent_collect"
    }

    fn description(&self) -> &str {
        "Check if a reactive subagent is done and return its result. Non-blocking — \
         returns immediately. If still running, returns status and partial output. \
         If finished, returns the full result. Call repeatedly to poll for completion. \
         After inspecting a finished result, call again with reconciled=true to attest \
         reconciliation and clear the completion gate; reconciled defaults to false so \
         inspection and attestation stay intentional (first terminal collect always \
         collects; any terminal collect with reconciled=true reconciles, including repeats)."
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
                    "description": "After inspecting the collected result, set true to attest reconciliation with foreground work and unblock completion. Defaults to false; a later collect with reconciled=true still reconciles (idempotent)."
                }
            },
            "required": ["handle_id"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        let mut reg = registry.lock().unwrap();
        let Some(handle) = reg.get_mut(&handle_id) else {
            drop(reg);
            if let Some(orchestration) = &ctx.capabilities.orchestration {
                if orchestration.is_unreconciled(&handle_id) {
                    orchestration
                        .terminal_and_collect(
                            &handle_id,
                            agent_core::orchestration::WorkerTerminal::Failed,
                        )
                        .map_err(RuntimeError::Tool)?;
                    if params["reconciled"].as_bool().unwrap_or(false) {
                        orchestration
                            .reconcile(&handle_id)
                            .map_err(RuntimeError::Tool)?;
                    }
                    return Ok(json!({
                        "handle_id": handle_id,
                        "status": "expired",
                        "note": "Subagent output expired; orchestration lifecycle remains recoverable.",
                        "collected": false
                    })
                    .to_string());
                }
            }
            return Err(RuntimeError::Tool(format!(
                "No subagent found with handle_id '{}'. Finished handles are retained for {} minutes after completion and then garbage-collected.",
                handle_id,
                crate::runtime::subagent::FINISHED_HANDLE_TTL.as_secs() / 60
            )));
        };

        let status = handle.status();
        let output: String = handle.partial_output();
        let elapsed = handle.elapsed_secs();
        let model = handle.model.clone();
        let terminal = handle.terminal_diagnostic();
        let authorization = handle.authorization.clone();

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
            })
            .to_string());
        }

        // Lifecycle (intentional caller attestation — never auto-reconcile):
        // - First terminal collect always drives terminal_and_collect.
        // - Any terminal collect with reconciled=true must reconcile, including
        //   a repeated collect after an earlier unreconciled read. Gating
        //   reconcile on !already_collected left workers stuck in Collected
        //   forever and blocked completion.
        // - terminal_and_collect stays first-collect-only so diagnostic
        //   re-reads remain read-many idempotent at the handle layer.
        if let Some(orchestration) = &ctx.capabilities.orchestration {
            if !already_collected {
                let terminal = match status {
                    SubagentStatus::Completed => {
                        agent_core::orchestration::WorkerTerminal::Completed
                    }
                    SubagentStatus::TimedOut => agent_core::orchestration::WorkerTerminal::TimedOut,
                    _ => agent_core::orchestration::WorkerTerminal::Failed,
                };
                orchestration
                    .terminal_and_collect(&handle_id, terminal)
                    .map_err(RuntimeError::Tool)?;
            }
            if params["reconciled"].as_bool().unwrap_or(false) {
                orchestration
                    .reconcile(&handle_id)
                    .map_err(RuntimeError::Tool)?;
            }
        }

        // Done — return full result. The registry retains this record, making
        // repeated collection diagnostically idempotent; `collected` signals
        // idempotency to the caller.
        let mut body = json!({
            "handle_id": handle_id,
            "status":    status.as_str(),
            "output":    output,
            "model":     model,
            "terminal_cause": terminal,
            "authorization": authorization,
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
    use crate::runtime::subagent::{
        SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
    };
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::Tool;
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

    fn make_ctx_with_registry(registry: Arc<Mutex<SubagentRegistry>>) -> crate::tools::ToolContext {
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry);
        // This suite exercises the registry-level collected/idempotency
        // contract in isolation. The handle is registered directly (never
        // dispatched through orchestration), so the policy registry must not
        // participate — dispatch-path enforcement is covered by the
        // orchestration and lifecycle suites.
        ctx.capabilities.orchestration = None;
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
        let result1 = tool
            .execute(
                json!({"handle_id": "sa_42"}),
                make_ctx_with_registry(registry.clone()),
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["status"], "completed");
        assert_eq!(body1["output"], "result text");
        assert_eq!(
            body1["collected"], false,
            "first collect should report collected=false"
        );

        // Second collect — same handle still in registry (not yet reaped), collected=true
        let result2 = tool
            .execute(
                json!({"handle_id": "sa_42"}),
                make_ctx_with_registry(registry.clone()),
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["status"], "completed");
        assert_eq!(
            body2["collected"], true,
            "second collect should report collected=true"
        );

        // After collect, the registry should have the handle marked collected
        let reg = registry.lock().unwrap();
        let h = reg.get("sa_42").unwrap();
        assert!(
            h.is_collected(),
            "handle must be marked collected in registry"
        );
    }

    /// Dispatch a finished handle through orchestration so collect can drive
    /// the terminal → collected → reconciled lifecycle. Returns (registry, orch).
    fn make_orch_ctx(
        handle_id: &str,
        output: &str,
    ) -> (
        Arc<Mutex<SubagentRegistry>>,
        Arc<crate::orchestration::OrchestrationRuntime>,
        crate::tools::ToolContext,
    ) {
        use crate::orchestration::OrchestrationRuntime;
        use agent_core::orchestration::CompletionGate;

        let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6")
            .expect("test foreground is qualified");
        let orch =
            Arc::new(OrchestrationRuntime::baseline(foreground, 8, 64).expect("baseline runtime"));
        orch.authorize(handle_id, "anthropic/claude-sonnet-4-6")
            .expect("authorize worker");
        assert!(
            matches!(orch.completion_gate(), CompletionGate::Blocked { .. }),
            "authorized worker must block completion until reconciled"
        );

        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry
            .lock()
            .unwrap()
            .register(make_finished_handle(handle_id, output));

        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        ctx.capabilities.orchestration = Some(orch.clone());
        (registry, orch, ctx)
    }

    // (a) first collect without reconciled leaves gate blocked; second collect
    // with reconciled=true advances Collected → Reconciled and allows completion.
    #[tokio::test]
    async fn deferred_reconcile_on_repeat_collect_unblocks_completion() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_deferred", "result text");

        let result1 = tool
            .execute(
                json!({"handle_id": "sa_deferred", "reconciled": false}),
                ctx,
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["collected"], false);
        match orch.completion_gate() {
            CompletionGate::Blocked { workers } => {
                assert_eq!(workers, vec!["sa_deferred".to_string()]);
                assert!(
                    workers.iter().all(|id| id.starts_with("sa_")),
                    "blocked IDs must be subagent_collect handles, got {workers:?}"
                );
                assert!(
                    workers.iter().all(|id| !id.starts_with("worker-")),
                    "blocked IDs must never leak policy WorkerHandle, got {workers:?}"
                );
            }
            other => panic!(
                "first collect without reconcile must leave completion blocked, got {other:?}"
            ),
        }

        // Rebuild ctx so the same shared orch/registry participate on the
        // second call (create_tool_context would allocate a fresh baseline).
        let mut ctx2 = create_tool_context();
        ctx2.capabilities.subagent_registry = Some(registry.clone());
        ctx2.capabilities.orchestration = Some(orch.clone());

        let result2 = tool
            .execute(
                json!({"handle_id": "sa_deferred", "reconciled": true}),
                ctx2,
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["collected"], true);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "repeat collect with reconciled=true must unblock completion"
        );
    }

    // (b) first collect with reconciled=true allows completion immediately.
    #[tokio::test]
    async fn first_collect_with_reconcile_allows_completion() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (_registry, orch, ctx) = make_orch_ctx("sa_first_true", "result text");

        let result = tool
            .execute(
                json!({"handle_id": "sa_first_true", "reconciled": true}),
                ctx,
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(body["collected"], false);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "first collect with reconciled=true must allow completion"
        );
    }

    #[tokio::test]
    async fn forced_expiry_remains_collectible_and_clears_gate() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_expired", &"result".repeat(10_000));
        crate::runtime::subagent::reap_finished_with_ttl(
            &registry,
            Some(orch.as_ref()),
            std::time::Duration::ZERO,
        );
        assert!(registry
            .lock()
            .unwrap()
            .get("sa_expired")
            .unwrap()
            .is_tombstone());

        let result = tool
            .execute(json!({"handle_id": "sa_expired", "reconciled": true}), ctx)
            .await
            .unwrap();
        assert_ne!(
            serde_json::from_str::<Value>(&result).unwrap()["status"],
            "expired"
        );
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);
    }

    #[tokio::test]
    async fn missing_mapped_handle_has_degraded_collect_not_404() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_missing", "lost");
        registry.lock().unwrap().remove("sa_missing");
        let result = tool
            .execute(json!({"handle_id": "sa_missing", "reconciled": true}), ctx)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(body["status"], "expired");
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);
    }

    // (c) repeated collect with reconciled=true remains read-many idempotent.
    #[tokio::test]
    async fn repeated_reconcile_true_is_idempotent() {
        use agent_core::orchestration::CompletionGate;

        let tool = SubagentCollectTool;
        let (registry, orch, ctx) = make_orch_ctx("sa_repeat_true", "result text");

        let result1 = tool
            .execute(
                json!({"handle_id": "sa_repeat_true", "reconciled": true}),
                ctx,
            )
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_str(&result1).unwrap();
        assert_eq!(body1["collected"], false);
        assert_eq!(orch.completion_gate(), CompletionGate::Allowed);

        let mut ctx2 = create_tool_context();
        ctx2.capabilities.subagent_registry = Some(registry.clone());
        ctx2.capabilities.orchestration = Some(orch.clone());

        let result2 = tool
            .execute(
                json!({"handle_id": "sa_repeat_true", "reconciled": true}),
                ctx2,
            )
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_str(&result2).unwrap();
        assert_eq!(body2["collected"], true);
        assert_eq!(
            orch.completion_gate(),
            CompletionGate::Allowed,
            "repeat reconciled collect must stay Allowed (idempotent)"
        );
        // Registry handle remains readable and marked collected.
        let reg = registry.lock().unwrap();
        assert!(reg.get("sa_repeat_true").unwrap().is_collected());
    }
}
