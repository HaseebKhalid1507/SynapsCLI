//! SubagentStartTool — dispatch a reactive subagent and return a handle_id immediately.
//!
//! Unlike the one-shot `subagent` tool, this tool returns *before* the subagent
//! finishes. The caller gets a `handle_id` they can poll via `subagent_status`,
//! steer via `subagent_steer`, or block on via `subagent_collect`.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};

use super::super::{resolve_agent_prompt, Tool, ToolContext, NEXT_SUBAGENT_ID};
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentState, SubagentStatus};
use crate::{AgentEvent, Result, RuntimeError};

pub struct SubagentStartTool;

#[async_trait::async_trait]
impl Tool for SubagentStartTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_start"
    }

    fn description(&self) -> &str {
        "Dispatch a reactive subagent and return immediately with a handle_id. \
         The subagent runs in the background and is REACTIVE: when it finishes, a \
         completion event is pushed into your queue and you are woken with its \
         status — do NOT sleep, busy-wait, or poll in a loop waiting for it. After \
         dispatching, end your turn or keep doing other work; on the completion \
         event call subagent_collect once with reconciled=true. subagent_status is \
         for an occasional progress peek, and subagent_steer injects guidance \
         mid-run. Use this for parallel execution or when you want to continue \
         working while the subagent runs. For simple sequential delegation that \
         must block, use subagent instead. Provide either an agent name (resolves \
         from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly; \
         give an inline subagent a short name so it is recognisable in the \
         subagent panel."
    }

    fn parameters(&self) -> Value {
        super::launch::with_rollover_param(json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent name — resolves to ~/.synaps-cli/agents/<name>.md. Mutually exclusive with system_prompt."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Inline system prompt for the subagent. Use when you don't have a named agent file."
                },
                "name": {
                    "type": "string",
                    "description": "Short display name for an inline (system_prompt) subagent, e.g. \"gif-recorder\". Shown in the subagent panel and in status/collect output. Letters, digits, - and _ only, max 32. Ignored when agent is set; defaults to \"inline\"."
                },
                "task": {
                    "type": "string",
                    "description": "The task/prompt to send to the subagent."
                },
                "model": {
                    "type": "string",
                    "description": "Omit to inherit the session foreground qualified identity. Explicit values must be one of subagent_models' listed exact choices."
                },
                "role": {
                    "type": "string",
                    "enum": ["planner", "implementer", "tester", "reviewer", "researcher", "debugger"],
                    "description": "Typed orchestration role."
                },
                "write_policy": {
                    "oneOf": [
                        {"type": "object", "properties": {"mode": {"const": "read_only"}}, "required": ["mode"]},
                        {"type": "object", "properties": {"mode": {"const": "isolated_worktree"}}, "required": ["mode"]},
                        {"type": "object", "properties": {"mode": {"const": "non_overlapping_paths"}, "scopes": {"type": "array", "items": {"type": "string"}}}, "required": ["mode", "scopes"]}
                    ],
                    "description": "Declared write isolation and path scopes."
                },
                "thinking": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh", "max"],
                    "description": "Worker effort override. Omit for subagent_thinking (default xhigh), independent of foreground thinking. Unsupported model levels are refused."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Wall-clock limit in seconds. Omit for the session default (config `subagent_timeout`); 0 = no limit. A timed-out worker keeps its full conversation: subagent_resume continues it where it stopped."
                }
            },
            "required": ["task"]
        }), super::launch::WORKER_ROLLOVER)
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        self.dispatch(params, ctx, |wanted| wanted.then(super::oneshot::RecordSite::live))
            .await
    }
}

impl SubagentStartTool {
    /// Start the worker. `site` is told whether the run wants worker
    /// rollover ([`super::launch::wants_rollover`]) and says where a run
    /// with a plan keeps its records; `None` runs it without one, as
    /// before. In production the run gets a plan only when it wants one
    /// (`execute`: at the live site); a test may give it one. A
    /// start takes only the site's root and process directory.
    pub(super) async fn dispatch(
        &self,
        params: Value,
        ctx: ToolContext,
        site: impl FnOnce(bool) -> Option<super::oneshot::RecordSite>,
    ) -> Result<String> {
        // One config snapshot for the whole spawn (`spawn_config`).
        let config = super::spawn_config();
        let thinking = super::resolve_subagent_thinking(&params, None, &config)?;
        // ── Parse params ───────────────────────────────────────────────────────
        let task = params["task"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();
        // The caller's opt-out of worker rollover (§2), checked before
        // anything else happens once worker rollover is built in.
        let rollover = super::launch::context_rollover_param(&params, super::launch::WORKER_ROLLOVER)
            .map_err(RuntimeError::Tool)?;

        // Treat blank / whitespace / control-char strings as absent.
        // Some model providers serialize "unset" as "" rather than omitting the field,
        // and we must not try to resolve "" (or " ", or "\u{0}") as an agent name —
        // doing so produces a hard "Agent '' not found" error that the model then
        // retries forever with sentinel values instead of falling back to system_prompt.
        let is_blank = |s: &String| s.chars().all(|c| c.is_whitespace() || c.is_control());
        let agent_name = params["agent"]
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !is_blank(s));
        let inline_prompt = params["system_prompt"]
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !is_blank(s));
        let requested_model = params["model"].as_str();
        // Validate the registry before authorization, id allocation, or event emission;
        // the same borrow is reused below when publishing the authorized handle.
        let registry = ctx.capabilities.subagent_registry.as_ref().ok_or_else(|| {
            RuntimeError::Tool("subagent_start requires a subagent_registry in ToolContext".into())
        })?;
        let system_prompt = match (&agent_name, &inline_prompt) {
            (Some(name), _) => resolve_agent_prompt(name).map_err(RuntimeError::Tool)?,
            (None, Some(p)) => p.clone(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). Got neither."
                        .to_string(),
                ));
            }
        };
        // Context admission (request_size): measured and reported, never
        // refused; before any id or reservation exists.
        let admitted = super::request_size::admit_fresh(
            super::request_size::Fresh::Start,
            &ctx,
            requested_model,
            &system_prompt,
            &[Arc::new(json!({"role": "user", "content": task.clone()}))],
            &super::subagent_label(agent_name.as_deref(), params["name"].as_str()),
        )
        .await;
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);
        let orchestration = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        orchestration
            .reserve_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref())
            .map_err(|reason| RuntimeError::Tool(format!("delegation denied: {reason}")))?;
        let decision = orchestration
            .resolve_and_authorize(&handle_id, requested_model)
            .map_err(|error| {
                // Refused before launch: release the edge and drop its owner entry.
                orchestration.abandon_delegation(&handle_id);
                RuntimeError::Tool(error.to_string())
            })?;
        let model = decision.model.as_str().to_owned();
        let label = super::subagent_label(agent_name.as_deref(), params["name"].as_str());
        // Small-watcher admission, the one call here: a polling task on a large
        // model is refused with a ready `wait`; on a small model it runs as a
        // watch worker under a finite lease.
        let watch_admitted = match super::watch_admission::admit_dispatch(
            "subagent_start",
            &super::watch_admission::Dispatch {
                request: super::watch_admission::WatchRequest {
                    name: Some(&label),
                    task: &task,
                    explicit_write_policy: params["write_policy"]["mode"].as_str(),
                },
                requested_model,
                resolved_model: &model,
            },
            None,
            None,
        )
        .await
        {
            Ok(watch_admitted) => watch_admitted,
            Err(refusal) => {
                orchestration.rollback(&handle_id);
                orchestration
                    .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
                return Ok(refusal);
            }
        };
        let lease = watch_admitted.watch;
        let codex_parent_plan = ctx.capabilities.codex_parent_plan.clone();
        let memory_backend = ctx.capabilities.memory_backend.clone();
        let session_allow_all = ctx.capabilities.session_allow_all.clone();
        let telemetry = ctx.capabilities.telemetry.clone();
        let workspace = super::WorkerWorkspace::from_capabilities(&ctx.capabilities);
        let worker_hooks = super::WorkerHooks::of(&ctx.capabilities, &handle_id);
        let timeout_secs = params["timeout"]
            .as_u64()
            .unwrap_or(ctx.limits.subagent_timeout);
        let timeout_secs = lease.map_or(timeout_secs, |(l, _)| l.clamp_timeout(timeout_secs));

        let telemetry = telemetry.map(|t| t.for_worker(&label, &handle_id));
        let task_preview: String = task.chars().take(80).collect();
        let task_full = task.clone();
        // Child spend: the run's lease. Refused before launch (the
        // delegating run was sealed, or the session is ending): undo the
        // authorization and the reservation.
        let spend_lease = super::spend_lease(&ctx, &handle_id, &label).map_err(|refused| {
            orchestration.rollback(&handle_id);
            orchestration.abandon_delegation(&handle_id);
            RuntimeError::Tool(refused.to_string())
        })?;
        // Worker rollover (on since slice 9, when the run wants it): the
        // run's records, on the binding the worker runs on.
        // The parent's live context settings at this call (§2).
        let worker_context = ctx.capabilities.worker_context.clone();
        let site = site(super::launch::wants_rollover(worker_context.as_ref(), rollover));
        let plan = site.map(|site| super::launch::LaunchPlan {
            root: site.root,
            process_dir: site.process_dir,
            handle_id: handle_id.clone(),
            session: ctx.capabilities.session_id.clone(),
            // The binding the worker runs on: the policy's fallback
            // (`apply_subagent_runtime_policy`), from this spawn's one
            // config snapshot and the worker's cwd.
            binding: memory_backend.clone().unwrap_or_else(|| {
                crate::memory_backend::MemoryBinding::from_config_with_cwd(
                    &config.memory_backend,
                    workspace.cwd.clone().or_else(|| std::env::current_dir().ok()),
                )
            }),
            meta: super::record::WorkerMeta {
                agent_name: label.clone(),
                model: model.clone(),
                system_prompt: system_prompt.clone(),
                timeout_secs,
                thinking: Some(thinking.as_str().into()),
                watch_usage: lease.map(|(_, used)| used.into()),
            },
            lineage: None,
            ledger: Default::default(),
            context: super::launch::worker_context(worker_context.as_ref(), &model),
            grant: None,
            tools: None,
            evicted_windows: 0,
        });

        tracing::info!(
            "subagent_start: dispatching '{}' (id={}) model={}",
            label,
            handle_id,
            model
        );

        // ── Shared state ───────────────────────────────────────────────────────
        let state = Arc::new(RwLock::new(SubagentState::new()));
        state.write().unwrap().archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
            agent_name: label.clone(),
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            timeout_secs,
            cwd: workspace
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok()),
            thinking: Some(thinking.as_str().into()),
            watch_usage: lease.map(|(_, used)| used),
        });

        state.write().unwrap().archive_dir = Some(super::archive::process_dir());
        let initial_messages = vec![Arc::new(
            serde_json::json!({"role": "user", "content": task.clone()}),
        )];
        state.write().unwrap().history = Some(initial_messages.clone());

        // ── Channels ───────────────────────────────────────────────────────────
        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<SubagentResult>();

        // ── Forward SubagentStart event to TUI ─────────────────────────────────
        if let Some(ref tx) = ctx.channels.tx_events {
            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            }));
        }

        // ── Clone state for the spawned thread ─────────────────────────────────
        let state_t = Arc::clone(&state);
        let task_full_a = task_full.clone();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.channels.tx_events.clone();
        let start_time = std::time::Instant::now();
        let parent_queue = ctx.capabilities.event_queue.clone();
        let handle_id_inner = handle_id.clone();
        let child_parent_id = handle_id.clone();
        let child_route = crate::session::child_events::worker_route(ctx.channels.tx_delta.as_ref(), &handle_id);

        // ── Build and register handle BEFORE spawning ─────────────────────────
        // This closes the publish-before-register race: finalize_subagent can
        // push a completion event the instant the thread exits, but the registry
        // must already contain the handle so the parent's collect succeeds.
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

        let orchestration = ctx.capabilities.orchestration.as_ref().unwrap();
        let orchestration_for_worker = Arc::clone(orchestration);
        let parent_for_worker = ctx.capabilities.delegation_parent.clone();
        if let Err(error) = orchestration.mark_starting(&handle_id) {
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }

        // ── Spawn subagent thread (mirrors subagent.rs) ────────────────────────
        let first_request =
            admitted.first_request(super::request_size::Kind::Start, &handle_id, &model);
        let thread_handle = std::thread::spawn(move || {
            // Pre-clone for finalizer — catch_unwind moves state_t and label_inner
            let state_for_finalizer = Arc::clone(&state_t);
            let label_for_finalizer = label_inner.clone();
            let orchestration_for_runtime = Arc::clone(&orchestration_for_worker);

            let mut pending_result = None;
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

                // Clones for the async block — the outer closure still needs the originals.
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
                        super::apply_subagent_runtime_policy(
                            &mut runtime,
                            &config,
                            memory_backend.as_ref(),
                            session_allow_all.as_ref(),
                            telemetry.as_ref(),
                            &workspace,
                        );
                        let mut lease_binds_rounds = false;
                        if let Some((lease, _)) = lease {
                            let mut budget = runtime.turn_budget().clone();
                            lease_binds_rounds = lease.apply_to(&mut budget);
                            runtime.set_turn_budget(budget);
                        }
                        // After the policy: its transform view (or the
                        // `subagent_transform_hooks = off` detach) is kept.
                        worker_hooks.apply(&mut runtime, &config);
                        runtime.set_system_prompt(super::compose_system_prompt(
                            system_prompt,
                            runtime.memory_backend_is_axel(),
                        ));
                        runtime.set_model(model_a.clone());
                        super::fit_subagent_context_window(&mut runtime);
                        super::apply_codex_worker_reasoning(
                            &mut runtime,
                            codex_parent_plan.as_ref(),
                        );
                        super::apply_subagent_thinking(&mut runtime, thinking)?;
                        runtime
                            .install_worker_orchestration(Arc::clone(&orchestration_for_runtime));
                        runtime.set_delegation_parent(Some(child_parent_id.clone()));
                        // Rolled-over context goes beside this worker's
                        // resume archive (see `worker_context`).
                        let archive_dir = state_a
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .archive_dir
                            .clone()
                            .unwrap_or_else(super::archive::process_dir);
                        super::attach_worker_transcripts(
                            &mut runtime,
                            archive_dir,
                            &child_parent_id,
                            Some(Arc::clone(&state_a)),
                            &initial_messages,
                            tx_events_a.clone(),
                            subagent_id,
                            &label_a,
                        );
                        runtime.set_child_route(child_route.clone());
                        super::install_spend(&mut runtime, spend_lease);

                        let cancel = crate::CancellationToken::new();
                        let cancel_inner = cancel.clone();
                        tokio::spawn(async move {
                            let _ = shutdown_rx.await;
                            cancel_inner.cancel();
                        });

                        let cancel_on_timeout = cancel.clone();
                        // A run with a plan shares one run clock with its
                        // runtime (§5); without one nothing changes.
                        let run_clock = super::launch::arm_run_clock(
                            plan.as_ref(),
                            &mut runtime,
                            timeout_secs,
                        );
                        // Its requests book into the run's ledger only once
                        // the run is a v2 run (§6).
                        super::launch::arm_request_ledger(plan.as_ref(), &mut runtime);
                        // A v2 run retrieves its own lineage's history
                        // only (8g): its registry as configured, for its
                        // own grant's tools to join.
                        let mut plan = plan;
                        super::launch::arm_owned_history(plan.as_mut(), &runtime, &config).await;
                        let drive_outcome = {
                        let runtime = &mut runtime;
                        // A v2 run's runtime is bound to its run and gets
                        // the parent's settings resolved for it (§2, §7.2)
                        // before its first inference; a run without
                        // rollover gets nothing.
                        let start = move |opening: Vec<crate::SharedMessage>,
                                          start: Option<super::launch::WorkerStart>| async move {
                            if let Some(start) = start {
                                super::launch::start_v2_runtime(runtime, start, &opening);
                            }
                            runtime.run_stream_with_messages(
                                opening,
                                cancel,
                                Some(steer_rx),
                                None,
                                false,
                            )
                            .await
                        };

                        let drive = super::drive::WorkerDrive {
                            state: Arc::clone(&state_a),
                            tx_events: tx_events_a.clone(),
                            subagent_id,
                            label: label_a.clone(),
                            model: model_a.clone(),
                            timeout_secs,
                            run_clock,
                            cancel: cancel_on_timeout,
                            task: task_for_timeout,
                            first_request: Some(first_request),
                            lease,
                            lease_binds_rounds,
                            shape: super::drive::DriveShape::Reactive,
                        };
                        super::launch::run_worker(drive, initial_messages, plan, start).await
                        };
                        // Date this run's cache for a later resume (cold check).
                        super::archive::record_cache_anchor(&state_a, runtime.cache_anchor());
                        drive_outcome
                    });

                match outcome {
                    Ok(sa_result) => {
                        // Only overwrite Running → Completed (don't stomp TimedOut or a cancellation).
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
                        pending_result = Some(sa_result);
                    }
                    Err(e) => {
                        state_t.write().unwrap().status = SubagentStatus::Failed(e);
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
                tracing::error!("Subagent thread panicked: {}", msg);
                state_t.write().unwrap_or_else(|p| p.into_inner()).status =
                    SubagentStatus::Failed(format!("panic: {}", msg));
            }

            // ── Terminal finalizer — exactly once, outside catch_unwind ────────
            // Covers all paths: Ok, Err, timeout, panic, early tokio-build failure.
            super::finalize::finalize_subagent(
                &state_for_finalizer,
                parent_queue.as_ref(),
                &handle_id_inner,
                subagent_id,
                &label_for_finalizer,
                start_time,
                None, // start.rs: not a resume
            );
            // A cancelled worker publishes no wake, so nothing would ever
            // prompt a collect: retire it before any terminal publication.
            super::finalize::retire_if_cancelled(
                &state_for_finalizer,
                &orchestration_for_worker,
                &handle_id_inner,
            );
            // All terminal publications follow the recovery/finalization barrier.
            if let Some(ref tx) = tx_events_inner {
                let s = state_for_finalizer
                    .read()
                    .unwrap_or_else(|p| p.into_inner());
                let preview = match s.status.failure_reason() {
                    Some(reason) => format!("ERROR: {reason}"),
                    None => s.partial_text.chars().take(120).collect(),
                };
                let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                    subagent_id,
                    agent_name: label_for_finalizer.clone(),
                    result_preview: preview,
                    duration_secs: start_time.elapsed().as_secs_f64(),
                }));
            }
            if let Some(result) = pending_result {
                let _ = result_tx.send(result);
            }
            orchestration_for_worker
                .release_delegation(&handle_id_inner, parent_for_worker.as_deref());
        });

        // ── Attach the already-persisted handle to the started thread ──────────
        {
            let mut reg = registry.lock().unwrap();
            if let Some(h) = reg.get_mut(&handle_id) {
                h.set_thread_handle(thread_handle);
            }
        }
        if let Err(error) = ctx
            .capabilities
            .orchestration
            .as_ref()
            .unwrap()
            .mark_running(&handle_id)
        {
            let thread = {
                let mut reg = registry.lock().unwrap();
                if let Some(handle) = reg.get_mut(&handle_id) {
                    handle.cancel();
                }
                reg.remove(&handle_id)
            };
            if let Some(handle) = thread {
                let _ = handle.collect().await;
            }
            orchestration.rollback(&handle_id);
            orchestration
                .release_delegation(&handle_id, ctx.capabilities.delegation_parent.as_deref());
            return Err(RuntimeError::Tool(error));
        }
        // The thread may have been cancelled and finalized before it was
        // marked running; its own retire attempt was then too early.
        super::finalize::retire_if_cancelled(&state, orchestration, &handle_id);

        let mut reply = json!({
            "handle_id":  handle_id,
            "agent_name": label,
            "status":     "running"
        });
        admitted.add_reply_fields(&mut reply);
        Ok(reply.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::{SubagentRegistry, Tool};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // V4: registry=None must return Err immediately, before any thread is spawned.
    #[tokio::test]
    async fn test_subagent_start_no_registry_returns_err() {
        let tool = SubagentStartTool;
        let ctx = create_tool_context(); // subagent_registry is None by default

        let params = json!({
            "system_prompt": "You are a test subagent.",
            "task": "Say ok",
        });

        let result = tool.execute(params, ctx).await;
        assert!(
            result.is_err(),
            "missing registry must return Err before spawn"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("subagent_registry"),
            "error must mention subagent_registry: {msg}"
        );
    }

    /// A test's isolated base directory and the registry of the workers it
    /// dispatches; the one owner of joining them. These tests launch a real
    /// worker thread: run unpinned, it read the developer's real `auth.json`
    /// (and could reach the provider) and wrote `logs/subagents` and
    /// `subagent-history` under the real `~/.synaps-cli` after the test
    /// returned. So the scope pins an empty base directory and clears the
    /// environment's provider credentials and endpoints
    /// (`OfflineProviderEnv`): the worker fails at auth, offline. Dropping it
    /// cancels and joins every worker (bounded), also when the test fails
    /// first, and only then restores the environment and the ambient base
    /// directory. A worker that does not stop in time keeps both (they are
    /// leaked, never restored under a live worker) and is reported.
    struct WorkerScope {
        registry: Arc<Mutex<SubagentRegistry>>,
        base: Option<crate::test_env::BaseDirGuard>,
        offline: Option<crate::test_env::OfflineProviderEnv>,
    }

    impl WorkerScope {
        fn new() -> Self {
            Self {
                base: Some(crate::test_env::BaseDirGuard::new()),
                offline: Some(crate::test_env::OfflineProviderEnv::new()),
                registry: Arc::new(Mutex::new(SubagentRegistry::new())),
            }
        }
    }

    impl Drop for WorkerScope {
        fn drop(&mut self) {
            let threads: Vec<_> = {
                let mut reg = self.registry.lock().unwrap_or_else(|p| p.into_inner());
                reg.iter_mut_handles()
                    .filter_map(|handle| {
                        handle.cancel();
                        handle.thread_handle.take()
                    })
                    .collect()
            };
            let mut outcomes = Vec::new();
            for thread in threads {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = done_tx.send(thread.join().is_ok());
                });
                outcomes.push(done_rx.recv_timeout(std::time::Duration::from_secs(60)));
            }
            if outcomes.iter().any(|o| o.is_err()) {
                // Still running: it may write below the isolated base or
                // read the provider environment.
                std::mem::forget(self.offline.take());
                std::mem::forget(self.base.take());
            }
            if !std::thread::panicking() && outcomes.iter().any(|o| *o != Ok(true)) {
                panic!("a cancelled worker did not stop cleanly: {outcomes:?}");
            }
        }
    }

    /// Stop a dispatched worker and wait until its thread has ended, so
    /// everything it writes (its archive under the base directory read at
    /// spawn) has landed. Joining stays with the [`WorkerScope`].
    async fn stop_and_join(registry: &Mutex<SubagentRegistry>, handle_id: &str) {
        registry
            .lock()
            .unwrap()
            .get_mut(handle_id)
            .expect("registered")
            .cancel();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let finished = registry
                .lock()
                .unwrap()
                .get(handle_id)
                .and_then(|h| h.thread_handle.as_ref().map(|t| t.is_finished()))
                .expect("the thread handle is attached before dispatch returns");
            if finished {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a cancelled worker stops within its 1 s timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn test_subagent_start_blank_agent_uses_system_prompt() {
        let scope = WorkerScope::new();
        let registry = &scope.registry;
        let tool = SubagentStartTool;
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());

        let params = json!({
            "agent": "",
            "system_prompt": "You are a concise test subagent. Reply with only: ok",
            "task": "Say ok",
            "model": "anthropic/claude-sonnet-4-6",
            "timeout": 1
        });

        let result = tool.execute(params, ctx).await;
        assert!(
            result.is_ok(),
            "blank agent should not be resolved as ~/.synaps-cli/agents/.md: {result:?}"
        );
        let body: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(body["agent_name"], "inline");
        let handle_id = body["handle_id"].as_str().unwrap_or_default();
        assert!(handle_id.starts_with("sa_"));
        stop_and_join(registry, handle_id).await;
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn test_subagent_start_inline_name_labels_the_handle() {
        let scope = WorkerScope::new();
        let registry = &scope.registry;
        let tool = SubagentStartTool;
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(registry.clone());

        let params = json!({
            "system_prompt": "You are a concise test subagent. Reply with only: ok",
            "name": "gif recorder",
            "task": "Say ok",
            "model": "anthropic/claude-sonnet-4-6",
            "timeout": 1
        });

        let result = tool.execute(params, ctx).await.expect("dispatch");
        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(body["agent_name"], "gif-recorder");
        let handle_id = body["handle_id"].as_str().unwrap();
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .get(handle_id)
                .expect("registered")
                .agent_name,
            "gif-recorder"
        );
        stop_and_join(registry, handle_id).await;
    }

    /// The worker's rollover notice once its thread has ended.
    fn notice_of(registry: &Mutex<SubagentRegistry>, handle_id: &str) -> Option<String> {
        let reg = registry.lock().unwrap();
        let state = &reg.get(handle_id).expect("registered").state;
        let notice = state.read().unwrap().rollover_notice.clone();
        drop(reg);
        notice
    }

    /// S0 (scaffolding for slice 9): a test site gives a start a plan
    /// whatever the run wants, as the oneshot's does; without the parent's
    /// settings it falls back to rollover off and says why. The production
    /// start gives none while worker rollover is off (mutants: the start
    /// ignores its site; `execute` gives a site unasked).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn s0_a_test_site_gives_a_start_a_plan() {
        let scope = WorkerScope::new();
        let registry = &scope.registry;
        let params = json!({
            "system_prompt": "You are a concise test subagent. Reply with only: ok",
            "task": "Say ok",
            "model": "anthropic/claude-sonnet-4-6",
            "timeout": 1
        });
        let mut notices = Vec::new();
        for planned in [false, true] {
            let mut ctx = create_tool_context();
            ctx.capabilities.subagent_registry = Some(registry.clone());
            assert!(ctx.capabilities.worker_context.is_none());
            let reply = if planned {
                SubagentStartTool
                    .dispatch(params.clone(), ctx, |_| {
                        Some(super::super::oneshot::RecordSite::live())
                    })
                    .await
            } else {
                SubagentStartTool.execute(params.clone(), ctx).await
            }
            .expect("dispatch");
            let body: serde_json::Value = serde_json::from_str(&reply).unwrap();
            let handle_id = body["handle_id"].as_str().unwrap().to_owned();
            stop_and_join(registry, &handle_id).await;
            notices.push(notice_of(registry, &handle_id));
        }
        assert_eq!(
            notices,
            [
                None,
                Some(
                    "rollover disabled for this worker: the parent's context settings are not available"
                        .to_owned()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_polling_start_on_a_large_model_is_refused_and_registers_nothing() {
        let mut ctx = create_tool_context();
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
        ctx.capabilities.subagent_registry = Some(registry.clone());
        let orchestration = ctx.capabilities.orchestration.clone().expect("policy");
        // A worker dispatched earlier, and a parent the refused start would
        // have nested under: the refusal must leave both exactly as they are.
        orchestration
            .resolve_and_authorize("sa_earlier", None)
            .expect("an earlier dispatch");
        orchestration
            .reserve_delegation("sa_parent", None)
            .expect("a parent edge");
        ctx.capabilities.delegation_parent = Some("sa_parent".into());
        let handles_before = orchestration.unreconciled_runtime_handles();
        assert!(handles_before.contains("sa_earlier"), "{handles_before:?}");
        let descendants_before = orchestration.delegation_descendants();
        let gate_before = orchestration.completion_gate();
        let params = json!({
            "system_prompt": "You watch a build.",
            "name": "t-build-wait",
            "task": "sleep 60, then tail -5 /var/tmp/build/out.log and report the last line",
            "model": "anthropic/claude-sonnet-4-6",
        });
        let text = SubagentStartTool
            .execute(params, ctx)
            .await
            .expect("a refusal is a tool result");
        assert!(
            text.starts_with("subagent_start refused (polling task)"),
            "{text}"
        );
        assert!(text.contains("wait"), "{text}");
        assert!(text.contains("/var/tmp/build/out.log"), "{text}");
        assert!(registry.lock().unwrap().list_active().is_empty());
        // The dispatch it authorized is rolled back (a Dispatched worker is
        // unreconciled until then), and its tree edge is released.
        assert_eq!(
            orchestration.unreconciled_runtime_handles(),
            handles_before,
            "the refused dispatch was rolled back"
        );
        assert_eq!(orchestration.delegation_descendants(), descendants_before);
        assert_eq!(orchestration.completion_gate(), gate_before);
    }
}

#[cfg(test)]
mod worker_gate {
    use super::*;
    include!("worker_gate_tests.rs");

    /// Child spend: a reactive worker in a host with a sink books through
    /// its own run's tap, and a resume is a new run.
    mod spend {
        use super::*;
        include!("reactive_spend_tests.rs");
    }
}

#[cfg(test)]
mod reactive_gate {
    use super::*;
    include!("reactive_gate_tests.rs");
}
