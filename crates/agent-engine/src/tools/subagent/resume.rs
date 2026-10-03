//! SubagentResumeTool — continue a finished subagent with new instructions.
//!
//! The worker's own conversation (from its resume archive, see `archive.rs`) is
//! replayed as the new run's history, with the instructions as the next user
//! turn, so it continues where it stopped rather than starting over. That works
//! for any terminal status and after the handle was collected or reaped; an
//! `archive_path` resumes a worker of an earlier or crashed session. Without an
//! archive (legacy), the prior run's text is pasted into a fresh task. The caller
//! gets a new `handle_id` for the continuation run.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};

use super::super::{Tool, ToolContext, NEXT_SUBAGENT_ID};
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentState, SubagentStatus};
use crate::{AgentEvent, Result, RuntimeError};

pub struct SubagentResumeTool;

fn expired_context_error(handle_id: &str) -> RuntimeError {
    RuntimeError::Tool(format!(
        "Subagent '{}' has expired resumable context; call subagent_collect with reconciled=true to reconcile its retained tombstone.",
        handle_id
    ))
}

/// Identity and opening messages of a resumed run.
#[derive(Debug)]
struct PriorRun {
    agent_name: String,
    model: String,
    system_prompt: String,
    timeout_secs: u64,
    messages: Vec<crate::SharedMessage>,
}

fn prior_run(
    registry: &std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>,
    prior_handle_id: &str,
    explicit_archive: Option<std::path::PathBuf>,
    instructions: &str,
) -> Result<PriorRun> {
    // Where the prior context comes from: the worker's archived conversation
    // (any terminal handle, including collected, reaped and tombstoned ones),
    // or else the legacy text snapshot on a live handle. An explicit
    // archive_path belongs to another session, so the registry is skipped.
    let (live, archive_path, legacy) = if explicit_archive.is_some() {
        (None, explicit_archive, None)
    } else {
        let reg = registry.lock().unwrap_or_else(|p| p.into_inner());
        let handle = reg.get(&prior_handle_id);
        if handle.is_some_and(|h| h.status() == SubagentStatus::Running) {
            return Err(RuntimeError::Tool(format!(
                "Subagent '{}' is still running. Call subagent_collect first, \
                     or wait until it finishes.",
                prior_handle_id
            )));
        }
        let archive_path = reg.archive_path(&prior_handle_id);
        let live = handle.map(|h| {
            (
                h.agent_name.clone(),
                h.model.clone(),
                h.system_prompt.clone(),
                h.timeout_secs,
            )
        });
        let legacy = match handle {
            Some(_) if archive_path.is_some() => None,
            Some(h) if h.is_tombstone() => return Err(expired_context_error(&prior_handle_id)),
            Some(h) => {
                let state = h.conversation_state();
                Some(if state.is_empty() {
                    h.partial_output()
                } else {
                    serde_json::to_string(&state).unwrap_or_else(|_| h.partial_output())
                })
            }
            None if archive_path.is_some() => None,
            None => {
                return Err(RuntimeError::Tool(format!(
                    "No subagent found with handle_id '{}'",
                    prior_handle_id
                )))
            }
        };
        (live, archive_path, legacy)
    };

    let archive = match &archive_path {
        Some(path) => Some(super::archive::read_archive(path).map_err(|e| {
            RuntimeError::Tool(format!(
                "Could not read the resume archive of '{}' ({}): {e}",
                prior_handle_id,
                path.display()
            ))
        })?),
        None => None,
    };
    let (agent_name, model, system_prompt, timeout_secs) = match (live, &archive) {
        (Some(live), _) => live,
        (None, Some(a)) => (
            a.meta.agent_name.clone(),
            a.meta.model.clone(),
            a.meta.system_prompt.clone(),
            a.meta.timeout_secs,
        ),
        (None, None) => {
            return Err(RuntimeError::Tool(format!(
                "No subagent found with handle_id '{}'",
                prior_handle_id
            )))
        }
    };
    let messages: Vec<crate::SharedMessage> = match (archive, legacy) {
        (Some(a), _) => super::archive::resume_messages(a.history, instructions),
        // Legacy: no archived conversation, only the prior run's text.
        (None, Some(prior_context)) => vec![Arc::new(json!({
            "role": "user",
            "content": format!(
                "{instructions}\n\n---\n[Prior conversation context from handle {prior_handle_id}]\n{prior_context}"
            )
        }))],
        (None, None) => unreachable!("either an archive or a legacy snapshot was found"),
    };
    Ok(PriorRun {
        agent_name,
        model,
        system_prompt,
        timeout_secs,
        messages,
    })
}

#[async_trait::async_trait]
impl Tool for SubagentResumeTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_resume"
    }

    fn description(&self) -> &str {
        "Resume a finished, timed-out, failed or cancelled reactive subagent with new \
         instructions. The worker continues its own archived conversation (every tool \
         call and result) with the instructions as the next user turn, so it picks up \
         where it stopped. Works after the handle was collected or reaped. For a worker \
         from an earlier or crashed session, pass archive_path. Returns a new handle_id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle_id": {
                    "type": "string",
                    "description": "Handle ID of the completed subagent to resume (e.g. \"sa_3\")."
                },
                "instructions": {
                    "type": "string",
                    "description": "What to do next. Sent as the next user turn after the \
                                    worker's own prior conversation."
                },
                "archive_path": {
                    "type": "string",
                    "description": "Optional: a resume archive (.json under \
                                    ~/.synaps-cli/subagent-history/) of a worker from an \
                                    earlier or crashed session. handle_id is then only a label."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional wall-clock limit in seconds for the resumed run \
                                    (default: the prior run's); 0 = no limit."
                }
            },
            "required": ["handle_id", "instructions"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let prior_handle_id = params["handle_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let instructions = params["instructions"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'instructions' parameter".to_string()))?
            .to_string();

        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("SubagentRegistry not available on this ToolContext".to_string())
        })?;

        let explicit_archive = match params["archive_path"].as_str() {
            Some(raw) => {
                Some(super::archive::checked_archive_path(raw).map_err(RuntimeError::Tool)?)
            }
            None => None,
        };

        let prior = prior_run(registry, &prior_handle_id, explicit_archive, &instructions)?;
        let (agent_name, model, system_prompt, initial_messages) = (
            prior.agent_name,
            prior.model,
            prior.system_prompt,
            prior.messages,
        );
        let timeout_secs = params["timeout"].as_u64().unwrap_or(prior.timeout_secs);

        // The inherited prior model is still re-authorized as an explicit exact
        // identity; a stale handle cannot bypass current session policy.
        let label = agent_name.clone();
        let task_preview: String = instructions.chars().take(80).collect();
        let task_full = instructions.clone();
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);
        let orchestration = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        orchestration
            .reserve_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref())
            .map_err(|reason| {
                RuntimeError::Tool(format!("delegation tree budget denied: {reason:?}"))
            })?;
        let decision = orchestration
            .resolve_and_authorize(&handle_id, Some(&model))
            .map_err(|error| {
                orchestration
                    .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
                RuntimeError::Tool(error.to_string())
            })?;
        let model = decision.model.as_str().to_owned();
        let codex_parent_plan = ctx.capabilities.codex_parent_plan.clone();
        let memory_backend = ctx.capabilities.memory_backend.clone();

        tracing::info!(
            "subagent_resume: dispatching '{}' (id={}, resumed_from={}) model={}",
            label,
            handle_id,
            prior_handle_id,
            model
        );

        let state = Arc::new(RwLock::new(SubagentState::new()));
        state.write().unwrap().archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: label.clone(),
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            timeout_secs,
        });

        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<SubagentResult>();

        if let Some(ref tx) = ctx.channels.tx_events {
            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            }));
        }

        let state_t = Arc::clone(&state);
        let task_full_a = task_full.clone();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.channels.tx_events.clone();
        let start_time = std::time::Instant::now();
        let parent_queue = ctx.capabilities.event_queue.clone();
        let handle_id_inner = handle_id.clone();
        let prior_handle_for_finalizer = prior_handle_id.clone();
        let child_parent_id = handle_id.clone();

        // ── Build and register handle BEFORE spawning ─────────────────────────
        let system_prompt_for_handle = system_prompt.clone();
        let handle = SubagentHandle::new(
            handle_id.clone(),
            subagent_id,
            label.clone(),
            task_preview,
            model.clone(),
            system_prompt_for_handle,
            timeout_secs,
            Arc::clone(&state),
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
        .with_authorization(&decision);
        {
            let mut reg = registry.lock().unwrap();
            reg.register_with_cancellation(handle, ctx.capabilities.launch_cancel.as_ref());
        }

        let orchestration_for_worker = Arc::clone(orchestration);
        let parent_for_worker = ctx.capabilities.delegation_parent.clone();
        if let Err(error) = orchestration.mark_starting(&handle_id) {
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }
        // ── Spawn subagent thread ──────────────────────────────────────────────
        let thread_handle = std::thread::spawn(move || {
            // Pre-clone for finalizer — catch_unwind moves state_t and label_inner
            let state_for_finalizer = Arc::clone(&state_t);
            let label_for_finalizer = label_inner.clone();
            let orchestration_for_runtime = Arc::clone(&orchestration_for_worker);

            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => {
                        state_t.write().unwrap().status =
                            SubagentStatus::Failed("runtime initialization failed".into());
                        return;
                    }
                };

                let state_a = Arc::clone(&state_t);
                let label_a = label_inner.clone();
                let model_a = model_inner.clone();
                let tx_events_a = tx_events_inner.clone();
                let task_for_timeout = task_full_a.clone();
                let task_for_complete = task_full_a;

                let outcome: std::result::Result<SubagentResult, String> =
                    rt.block_on(async move {
                        // Host-built worker (shared client/creds/token cache, cached
                        // registry) or the legacy fresh runtime — see `spawn_runtime`.
                        let mut runtime = match super::spawn_runtime().await {
                            Ok(r) => r,
                            Err(_) => return Err("subagent runtime initialization failed".into()),
                        };

                        // Apply subagent spawn policy: worker role, 5m cache TTL, worker
                        // turn budget. Subagents are short-lived one-shots — paying the 1h
                        // write premium (~2× input price) on them is unrecoverable waste
                        // (~$0.23 per 10-spawn fan-out). (#110)
                        super::apply_subagent_runtime_policy(&mut runtime, &crate::config::load_config(), memory_backend.as_ref());
                        runtime.set_system_prompt(super::compose_system_prompt(
                            system_prompt,
                            runtime.memory_backend_is_axel(),
                        ));
                        runtime.set_model(model_a.clone());
                        super::apply_codex_worker_reasoning(
                            &mut runtime,
                            codex_parent_plan.as_ref(),
                        );
                        runtime
                            .install_worker_orchestration(Arc::clone(&orchestration_for_runtime));
                        runtime.set_delegation_parent(Some(child_parent_id.clone()));

                        let cancel = crate::CancellationToken::new();
                        let cancel_inner = cancel.clone();
                        tokio::spawn(async move {
                            let _ = shutdown_rx.await;
                            cancel_inner.cancel();
                        });

                        let cancel_on_timeout = cancel.clone();
                        let stream = runtime.run_stream_with_messages(
                                initial_messages,
                                cancel,
                                Some(steer_rx),
                                None,
                                false,
                            )
                            .await;

                        super::drive::WorkerDrive {
                            state: Arc::clone(&state_a),
                            tx_events: tx_events_a.clone(),
                            subagent_id,
                            label: label_a.clone(),
                            model: model_a.clone(),
                            timeout_secs,
                            cancel: cancel_on_timeout,
                            task: task_for_timeout,
                        }
                        .run(stream)
                        .await
                    });

                match outcome {
                    Ok(sa_result) => {
                        {
                            let mut s = state_t.write().unwrap();
                            if matches!(s.status, SubagentStatus::Running) && !s.cancel_requested {
                                s.status = SubagentStatus::Completed;
                                s.conversation_state = vec![
                                    serde_json::json!({"role": "user", "content": task_for_complete.clone()}),
                                    serde_json::json!({"role": "assistant", "content": sa_result.text.clone()}),
                                ];
                            }
                        }
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let preview: String = sa_result.text.chars().take(120).collect();
                        if let Some(ref tx) = tx_events_inner {
                            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                                subagent_id,
                                agent_name: label_inner.clone(),
                                result_preview: preview,
                                duration_secs: elapsed,
                            }));
                        }
                        let _ = result_tx.send(sa_result);
                    }
                    Err(e) => {
                        state_t.write().unwrap().status = SubagentStatus::Failed(e.clone());
                        let elapsed = start_time.elapsed().as_secs_f64();
                        if let Some(ref tx) = tx_events_inner {
                            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                                subagent_id,
                                agent_name: label_inner.clone(),
                                result_preview: format!("ERROR: {}", e),
                                duration_secs: elapsed,
                            }));
                        }
                    }
                }
            }));

            if let Err(panic_info) = panic_result {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("Resumed subagent thread panicked: {}", msg);
                state_t.write().unwrap_or_else(|p| p.into_inner()).status =
                    SubagentStatus::Failed(format!("panic: {}", msg));
            }

            // ── Terminal finalizer — exactly once, outside catch_unwind ────────
            // Covers all paths: Ok, Err, timeout, panic, early tokio-build failure.
            // Sets data.resumed_from so parent can correlate with the prior handle.
            super::finalize::finalize_subagent(
                &state_for_finalizer,
                parent_queue.as_ref(),
                &handle_id_inner,
                subagent_id,
                &label_for_finalizer,
                start_time,
                Some(&prior_handle_for_finalizer),
            );
            orchestration_for_worker
                .release_delegation(&handle_id_inner, parent_for_worker.as_deref());
        });

        // ── Wire thread handle into the already-registered entry ─────────────
        {
            let mut reg = registry.lock().unwrap();
            if let Some(h) = reg.get_mut(&handle_id) {
                h.set_thread_handle(thread_handle);
            }
        }
        if let Err(error) = orchestration.mark_running(&handle_id) {
            let mut handle = registry.lock().unwrap().remove(&handle_id);
            if let Some(handle) = handle.as_mut() {
                handle.cancel();
            }
            if let Some(handle) = handle {
                let _ = handle.collect().await;
            }
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }

        Ok(json!({
            "handle_id":    handle_id,
            "resumed_from": prior_handle_id,
            "agent_name":   label,
            "status":       "running"
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::subagent::{reap_finished_with_ttl, SubagentRegistry};
    use crate::tools::test_helpers::create_tool_context;
    use std::sync::Mutex;
    use std::time::Duration;

    fn finished_handle(
        id: &str,
        status: SubagentStatus,
        archive_path: Option<std::path::PathBuf>,
    ) -> SubagentHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = status;
            s.partial_text = "prior text".into();
            s.conversation_state = vec![json!({"role": "assistant", "content": "prior text"})];
            s.finished_at = Some(std::time::Instant::now());
            s.archive_path = archive_path;
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        SubagentHandle::new(
            id.into(),
            1,
            "live-label".into(),
            "task".into(),
            "anthropic/claude-sonnet-4-6".into(),
            "live system".into(),
            30,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    fn archive_in(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        let meta = crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: "archived-label".into(),
            model: "anthropic/claude-opus-5-5".into(),
            system_prompt: "archived system".into(),
            timeout_secs: 3500,
        };
        let history: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role": "user", "content": "build the plate"})),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "render"}}
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "rendered 300 frames"}
            ]})),
        ];
        super::super::archive::write_archive(dir, id, &meta, "timed_out", "report", &history)
            .unwrap()
    }

    fn assert_continues_archive(prior: &PriorRun, instructions: &str) {
        assert_eq!(
            prior.messages.len(),
            3,
            "archived turns replayed, instructions merged"
        );
        assert_eq!(prior.messages[0]["content"], "build the plate");
        let last = &prior.messages[2]["content"];
        assert_eq!(last[0]["content"], "rendered 300 frames");
        assert!(last[1]["text"].as_str().unwrap().ends_with(instructions));
    }

    #[test]
    fn reaped_worker_resumes_from_its_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = archive_in(dir.path(), "sa_reaped");
        let registry = Mutex::new(SubagentRegistry::new());
        {
            let mut reg = registry.lock().unwrap();
            let mut h = finished_handle("sa_reaped", SubagentStatus::TimedOut, Some(path));
            h.mark_collected();
            reg.register(h);
            reg.cleanup_finished_with_ttl(Duration::ZERO);
            assert!(reg.get("sa_reaped").is_none(), "handle reaped");
        }
        let prior = prior_run(&registry, "sa_reaped", None, "render pass 3").unwrap();
        assert_eq!(prior.agent_name, "archived-label");
        assert_eq!(prior.model, "anthropic/claude-opus-5-5");
        assert_eq!(prior.system_prompt, "archived system");
        assert_eq!(prior.timeout_secs, 3500);
        assert_continues_archive(&prior, "render pass 3");
    }

    #[test]
    fn tombstoned_worker_with_archive_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = archive_in(dir.path(), "sa_tomb");
        let registry = Mutex::new(SubagentRegistry::new());
        {
            let mut reg = registry.lock().unwrap();
            reg.register(finished_handle(
                "sa_tomb",
                SubagentStatus::Failed("x".into()),
                Some(path),
            ));
            reg.release_finished_resources("sa_tomb");
            assert!(reg.get("sa_tomb").unwrap().is_tombstone());
        }
        let prior = prior_run(&registry, "sa_tomb", None, "continue").unwrap();
        // A live handle's identity wins over the archive's.
        assert_eq!(prior.agent_name, "live-label");
        assert_eq!(prior.timeout_secs, 30);
        assert_continues_archive(&prior, "continue");
    }

    #[test]
    fn running_worker_cannot_be_resumed() {
        let registry = Mutex::new(SubagentRegistry::new());
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_run", SubagentStatus::Running, None));
        let err = prior_run(&registry, "sa_run", None, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("still running"), "{err}");
    }

    #[test]
    fn explicit_archive_ignores_the_registry() {
        // An archive from an earlier session can share a handle id with a
        // worker running now; the registry must not be consulted.
        let dir = tempfile::tempdir().unwrap();
        let path = archive_in(dir.path(), "sa_3");
        let registry = Mutex::new(SubagentRegistry::new());
        registry
            .lock()
            .unwrap()
            .register(finished_handle("sa_3", SubagentStatus::Running, None));
        let prior = prior_run(&registry, "sa_3", Some(path), "pick up").unwrap();
        assert_eq!(prior.agent_name, "archived-label");
        assert_continues_archive(&prior, "pick up");
    }

    #[test]
    fn legacy_handle_without_archive_pastes_prior_text() {
        let registry = Mutex::new(SubagentRegistry::new());
        registry.lock().unwrap().register(finished_handle(
            "sa_old",
            SubagentStatus::Completed,
            None,
        ));
        let prior = prior_run(&registry, "sa_old", None, "next").unwrap();
        assert_eq!(prior.messages.len(), 1);
        let text = prior.messages[0]["content"].as_str().unwrap();
        assert!(text.starts_with("next") && text.contains("prior text"));
    }

    #[test]
    fn unknown_handle_without_archive_is_not_found() {
        let registry = Mutex::new(SubagentRegistry::new());
        let err = prior_run(&registry, "sa_none", None, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("No subagent found"), "{err}");
    }

    #[tokio::test]
    async fn tombstone_resume_reports_expired_context_without_side_effects() {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut state = state.write().unwrap();
            state.status = SubagentStatus::Completed;
            state.partial_text = "terminal output".into();
            state.conversation_state = vec![json!({"role": "assistant", "content": "context"})];
            state.finished_at = Some(std::time::Instant::now());
        }
        let (steer_tx, _steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        registry.lock().unwrap().register(SubagentHandle::new(
            "sa_expired".into(),
            1,
            "test".into(),
            "task".into(),
            "anthropic/claude-sonnet-4-6".into(),
            "system".into(),
            30,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        ));
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
        let orchestration = Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64).unwrap(),
        );
        orchestration
            .authorize("sa_expired", "anthropic/claude-sonnet-4-6")
            .unwrap();
        let gate_before = orchestration.completion_gate();
        reap_finished_with_ttl(&registry, Some(orchestration.as_ref()), Duration::ZERO);
        assert!(registry
            .lock()
            .unwrap()
            .get("sa_expired")
            .unwrap()
            .is_tombstone());
        let count_before = registry.lock().unwrap().list_active().len();

        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());
        ctx.capabilities.orchestration = Some(orchestration.clone());
        let error = SubagentResumeTool
            .execute(
                json!({"handle_id": "sa_expired", "instructions": "continue"}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("expired resumable context"));
        assert!(!error.contains("No subagent found"));
        assert_eq!(registry.lock().unwrap().list_active().len(), count_before);
        assert_eq!(orchestration.completion_gate(), gate_before);
    }
}
