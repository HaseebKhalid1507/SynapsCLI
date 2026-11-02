use super::super::{resolve_agent_prompt, Tool, ToolContext, NEXT_SUBAGENT_ID};
pub use crate::runtime::subagent::SubagentResult;
use crate::{AgentEvent, LlmEvent, Result, RuntimeError, SessionEvent};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;

mod reap;

pub struct SubagentTool;

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent"
    }

    /// Each blocking subagent is its own actor: several dispatched in one
    /// batch run side by side (the parent waits for all of them either way)
    /// instead of one after another in the shared serial lane.
    fn runs_independently(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Dispatch a one-shot subagent with a specific system prompt to perform a task. The subagent gets its own tool suite (bash, read, write, edit, grep, find, ls) and runs autonomously until done. Use this when you need the result before continuing. Blocks until done. For parallel work, use subagent_start instead. Provide either an agent name (resolves from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly; give an inline subagent a short name so it is recognisable in the subagent panel."
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
                "model": {"type": "string", "description": "Omit to inherit the session foreground qualified identity. Explicit values must be one of subagent_models' listed exact choices."},
                "role": {"type": "string", "enum": ["planner", "implementer", "tester", "reviewer", "researcher", "debugger"]},
                "write_policy": {
                    "oneOf": [
                        {"type": "object", "properties": {"mode": {"const": "read_only"}}, "required": ["mode"]},
                        {"type": "object", "properties": {"mode": {"const": "isolated_worktree"}}, "required": ["mode"]},
                        {"type": "object", "properties": {"mode": {"const": "non_overlapping_paths"}, "scopes": {"type": "array", "items": {"type": "string"}}}, "required": ["mode", "scopes"]}
                    ]
                },
                "thinking": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh", "max"],
                    "description": "Worker effort override. Omit for subagent_thinking (default xhigh), independent of foreground thinking. Unsupported model levels are refused."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Wall-clock limit in seconds. Omit for the session default (config `subagent_timeout`); 0 = no limit."
                }
            },
            "required": ["task"]
        }), super::launch::WORKER_ROLLOVER)
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        self.dispatch(params, ctx, |wanted| wanted.then(RecordSite::live))
            .await
    }
}

impl SubagentTool {
    /// Run the oneshot. `site` is told whether the run wants worker
    /// rollover ([`super::launch::wants_rollover`]) and says where a run
    /// with a plan keeps its records; `None` runs it without one, as
    /// before. In production the run gets a plan only when it wants one
    /// (`execute`: at the live site); a test may give it one.
    pub(super) async fn dispatch(
        &self,
        params: Value,
        ctx: ToolContext,
        site: impl FnOnce(bool) -> Option<RecordSite>,
    ) -> Result<String> {
        // One config snapshot for the whole spawn (`spawn_config`).
        let config = super::spawn_config();
        let thinking = super::resolve_subagent_thinking(&params, None, &config)?;
        let task = params["task"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();
        // The caller's opt-out of worker rollover (§2), checked before
        // anything else happens once worker rollover is built in.
        let rollover = super::launch::context_rollover_param(&params, super::launch::WORKER_ROLLOVER)
            .map_err(RuntimeError::Tool)?;

        // Treat blank / whitespace / control-char strings as absent — see
        // subagent_start.rs for full rationale. Models occasionally pass `agent: ""`
        // (or "\u{0}", or " ") alongside a real `system_prompt`; without this filter
        // we'd try to resolve an empty agent name and fail, instead of falling
        // through to the inline prompt.
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
        let system_prompt = match (&agent_name, &inline_prompt) {
            (Some(name), _) => resolve_agent_prompt(name).map_err(RuntimeError::Tool)?,
            (None, Some(prompt)) => prompt.clone(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). Got neither."
                        .to_string(),
                ));
            }
        };
        // Context admission (request_size): measured and reported, never
        // refused; before any id or grant exists.
        let admitted = super::request_size::admit_fresh(
            super::request_size::Fresh::Oneshot,
            &ctx,
            requested_model,
            &system_prompt,
            &[std::sync::Arc::new(
                json!({"role": "user", "content": task.clone()}),
            )],
            &super::subagent_label(agent_name.as_deref(), params["name"].as_str()),
        )
        .await;
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let orchestration_id = format!("sa_{}", subagent_id);
        let decision = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?
            .resolve_and_authorize(&orchestration_id, requested_model)
            .map_err(|error| RuntimeError::Tool(error.to_string()))?;
        let model = decision.model.as_str().to_owned();
        let label = super::subagent_label(agent_name.as_deref(), params["name"].as_str());
        // Small-watcher admission, as in subagent_start. A one-shot has no
        // resume, so its lease is one run's.
        let lease = match super::watch_admission::admit_dispatch(
            "subagent",
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
            Ok(admitted) => admitted.watch.map(|(lease, _)| lease),
            Err(refusal) => {
                if let Some(orchestration) = ctx.capabilities.orchestration.as_ref() {
                    orchestration.rollback(&orchestration_id);
                }
                return Ok(refusal);
            }
        };
        let codex_parent_plan = ctx.capabilities.codex_parent_plan.clone();
        let memory_backend = ctx.capabilities.memory_backend.clone();
        let session_allow_all = ctx.capabilities.session_allow_all.clone();
        let telemetry = ctx.capabilities.telemetry.clone();
        let workspace = super::WorkerWorkspace::from_capabilities(&ctx.capabilities);
        let worker_hooks = super::WorkerHooks::of(&ctx.capabilities, &orchestration_id);
        let requested_timeout = params["timeout"]
            .as_u64()
            .unwrap_or(ctx.limits.subagent_timeout);
        let timeout_secs = lease.map_or(requested_timeout, |l| l.clamp_timeout(requested_timeout));
        let timeout_header = timeout_header(
            timeout_secs,
            matches!(lease, Some(l) if requested_timeout == 0 || requested_timeout >= l.secs),
        );

        let telemetry = telemetry.map(|t| t.for_worker(&label, &orchestration_id));
        let task_preview: String = task.chars().take(80).collect();
        // Child spend: the run's lease. With one, the worker's requests book
        // through its tap as they are made, so no aggregate Usage is sent at
        // the end. Refused before launch: undo the authorization.
        let spend_lease = super::spend_lease(&ctx, &orchestration_id, &label).map_err(|refused| {
            if let Some(policy) = &ctx.capabilities.orchestration {
                policy.rollback(&orchestration_id);
            }
            RuntimeError::Tool(refused.to_string())
        })?;
        let books_spend = spend_lease.is_some();
        let seal = spend_lease.clone();
        // Worker rollover (on since slice 9, when the run wants it): the
        // run's records, on the binding the worker runs on, with the
        // parent's live context settings at this call (§2), as for a
        // reactive worker.
        let worker_context = ctx.capabilities.worker_context.clone();
        let site = site(super::launch::wants_rollover(worker_context.as_ref(), rollover));
        #[cfg(test)]
        let test_wrap = site.as_ref().and_then(|site| site.test_wrap.clone());
        let plan = site.map(|site| {
            super::launch::LaunchPlan {
                root: site.root,
                process_dir: site.process_dir,
                handle_id: orchestration_id.clone(),
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
                    // A watch oneshot's lease is one run's: nothing used
                    // before it.
                    thinking: Some(thinking.as_str().into()),
                    watch_usage: lease
                        .map(|_| crate::runtime::subagent::WatchUsage::default().into()),
                },
                lineage: None,
                ledger: Default::default(),
                context: super::launch::worker_context(worker_context.as_ref(), &model),
                grant: None,
                tools: None,
                evicted_windows: 0,
            }
        });

        tracing::info!(
            "Dispatching subagent '{}' (id={}) with model {}",
            label,
            subagent_id,
            model
        );

        if let Some(ref tx) = ctx.channels.tx_events {
            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            }));
        }

        let start_time = std::time::Instant::now();

        let (result_tx, result_rx) =
            tokio::sync::oneshot::channel::<std::result::Result<SubagentResult, String>>();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.channels.tx_events.clone();
        let child_route = crate::session::child_events::worker_route(ctx.channels.tx_delta.as_ref(), &orchestration_id);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        // Where a run with a plan is with its terminal publication (§5); a
        // run without one never moves it.
        let terminal_phase = std::sync::Arc::new(super::launch::TerminalPhase::default());
        let terminal_phase_inner = std::sync::Arc::clone(&terminal_phase);
        // The v2 run whose ledger counted the result, set by a run with a
        // plan as it returns (8f): its aggregate goes out once. It travels
        // next to the result, not on it, so a run without worker rollover
        // returns the result it always did.
        let counted_run: std::sync::Arc<std::sync::OnceLock<String>> = Default::default();
        let counted_run_inner = std::sync::Arc::clone(&counted_run);
        let orchestration = ctx.capabilities.orchestration.as_ref().unwrap();
        if let Err(error) = orchestration.mark_starting(&orchestration_id) {
            orchestration.rollback(&orchestration_id);
            return Err(RuntimeError::Tool(error));
        }

        // Names this worker's rolled-over context windows (`worker_context`).
        let transcript_stem = orchestration_id.clone();
        let first_request = admitted.first_request(
            super::request_size::Kind::Oneshot,
            &orchestration_id,
            &model,
        );
        // Who ends the handle: the worker thread's last act ends it when it
        // was handed the end (its run's final record outlasted the wait).
        let worker_end = std::sync::Arc::new(reap::WorkerEnd::default());
        let worker_end_inner = std::sync::Arc::clone(&worker_end);
        let policy_inner = std::sync::Arc::clone(orchestration);
        let id_inner = orchestration_id.clone();

        let thread_handle = match std::thread::Builder::new().spawn(move || {
            let mut terminal = agent_core::orchestration::WorkerTerminal::Failed;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => {
                        let _ = result_tx.send(Err("runtime initialization failed".into()));
                        return;
                    }
                };

                // A run with a plan whose terminal publication outlasted the
                // bound its caller waits leaves it here; the thread drives it
                // to its end after sending the result.
                let settling: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
                    Default::default();
                let settling_inner = std::sync::Arc::clone(&settling);
                let result = rt.block_on(async move {
                    use futures::StreamExt;

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
                    super::apply_subagent_runtime_policy(&mut runtime, &config, memory_backend.as_ref(), session_allow_all.as_ref(), telemetry.as_ref(), &workspace);
                    if let Some(lease) = lease {
                        let mut budget = runtime.turn_budget().clone();
                        lease.apply_to(&mut budget);
                        runtime.set_turn_budget(budget);
                    }
                    // After the policy: its transform view (or the
                    // `subagent_transform_hooks = off` detach) is kept.
                    worker_hooks.apply(&mut runtime, &config);
                    runtime.set_system_prompt(super::compose_system_prompt(system_prompt, runtime.memory_backend_is_axel()));
                    runtime.set_model(model);
                    super::fit_subagent_context_window(&mut runtime);
                    super::apply_codex_worker_reasoning(&mut runtime, codex_parent_plan.as_ref());
                    super::apply_subagent_thinking(&mut runtime, thinking)?;
                    // Rolled-over context goes to the worker archive directory.
                    super::attach_worker_transcripts(
                        &mut runtime,
                        super::archive::process_dir(),
                        &transcript_stem,
                        None,
                        &[],
                        tx_events_inner.clone(),
                        subagent_id,
                        &label_inner,
                    );
                    runtime.set_child_route(child_route);
                    super::install_spend(&mut runtime, spend_lease);

                    // A run with a plan: the shared launch and drive, on
                    // one clock, in the oneshot's words (slice 8c).
                    if let Some(mut plan) = plan {
                        // A v2 run retrieves its own history only (8g):
                        // its registry as configured, for its own
                        // grant's tools to join.
                        super::launch::arm_owned_history(Some(&mut plan), &runtime, &config)
                            .await;
                        return run_planned(
                            runtime,
                            plan,
                            Planned {
                                task,
                                timeout_secs,
                                tx_events: tx_events_inner,
                                subagent_id,
                                label: label_inner,
                                model: model_inner,
                                shutdown: shutdown_rx,
                                phase: terminal_phase_inner,
                                settling: settling_inner,
                                counted_run: counted_run_inner,
                                first_request,
                                // A watch oneshot's lease is one run's.
                                lease: lease.map(|lease| (lease, Default::default())),
                                #[cfg(test)]
                                test_wrap,
                            },
                        )
                        .await;
                    }

                    let cancel = crate::CancellationToken::new();
                    let cancel_inner = cancel.clone();

                    tokio::spawn(async move {
                        let _ = shutdown_rx.await;
                        cancel_inner.cancel();
                    });

                    let mut stream = runtime.run_stream(task, cancel).await;

                let mut final_text = String::new();
                let mut tool_count = 0u32;
                let mut tool_log: Vec<String> = Vec::new();
                let mut response_baseline: (usize, usize, u32) = (0, 0, 0);
                // A new response's first text starts a new paragraph.
                let mut new_response = false;
                let mut total_input_tokens = 0u64;
                let mut total_output_tokens = 0u64;
                let mut total_cache_read = 0u64;
                let mut total_cache_creation = 0u64;
                // TTL split: None only if no turn ever reported one; otherwise summed.
                let mut total_cache_5m: Option<u64> = None;
                let mut total_cache_1h: Option<u64> = None;

                // 0 = no limit (tokio clamps Duration::MAX to its far-future instant).
                let timeout_fut = tokio::time::sleep(if timeout_secs == 0 {
                    Duration::MAX
                } else {
                    Duration::from_secs(timeout_secs)
                });
                tokio::pin!(timeout_fut);

                loop {
                    tokio::select! {
                        event = stream.next() => {
                            let Some(event) = event else { break };
                            crate::runtime::openai::codex_wire::forward_worker_web_search(
                                &label_inner,
                                &event,
                                tx_events_inner.as_ref(),
                            );
                            match event {
                                crate::StreamEvent::Llm(LlmEvent::Thinking(_)) => {
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: "💭 thinking...".to_string(),
                                        }));
                                    }
                                }
                                crate::StreamEvent::Llm(LlmEvent::ResponseStart) => {
                                    response_baseline = (final_text.len(), tool_log.len(), tool_count);
                                    new_response = true;
                                }
                                crate::StreamEvent::Llm(LlmEvent::ResponseReset) => {
                                    final_text.truncate(response_baseline.0);
                                    tool_log.truncate(response_baseline.1);
                                    tool_count = response_baseline.2;
                                    new_response = true;
                                }
                                crate::StreamEvent::Llm(LlmEvent::Text(text)) => {
                                    append_response_text(&mut final_text, &mut new_response, &text);
                                }
                                crate::StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name: name, .. }) => {
                                    tool_count += 1;
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: format!("⚙ {} (tool #{})", name, tool_count),
                                        }));
                                    }
                                }
                                crate::StreamEvent::Llm(LlmEvent::ToolUse { tool_name, input, .. }) => {
                                    let input_str = input.to_string();
                                    let input_preview: String = input_str.chars().take(200).collect();
                                    tool_log.push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                                    // Build a rich status from the tool input
                                    let detail = match tool_name.as_str() {
                                        "bash" => {
                                            let cmd = input["command"].as_str().unwrap_or("");
                                            let preview: String = cmd.chars().take(60).collect();
                                            format!("$ {}", preview)
                                        }
                                        "read" => {
                                            let path = input["path"].as_str().unwrap_or("?");
                                            let short = path.rsplit('/').next().unwrap_or(path);
                                            format!("reading {}", short)
                                        }
                                        "write" => {
                                            let path = input["path"].as_str().unwrap_or("?");
                                            let short = path.rsplit('/').next().unwrap_or(path);
                                            format!("writing {}", short)
                                        }
                                        "edit" => {
                                            let path = input["path"].as_str().unwrap_or("?");
                                            let short = path.rsplit('/').next().unwrap_or(path);
                                            format!("editing {}", short)
                                        }
                                        "multi_edit" => crate::tools::multi_edit_label(&input),
                                        "grep" => {
                                            let pat = input["pattern"].as_str().unwrap_or("?");
                                            let preview: String = pat.chars().take(30).collect();
                                            format!("grep /{}/", preview)
                                        }
                                        "find" => {
                                            let pat = input["pattern"].as_str().unwrap_or("?");
                                            format!("find {}", pat)
                                        }
                                        "ls" => {
                                            let path = input["path"].as_str().unwrap_or(".");
                                            let short = path.rsplit('/').next().unwrap_or(path);
                                            format!("ls {}", short)
                                        }
                                        "subagent" => {
                                            let agent = input["agent"].as_str()
                                                .filter(|s| !s.trim().is_empty());
                                            let name = super::subagent_label(agent, input["name"].as_str());
                                            format!("spawning {}", name)
                                        }
                                        other => {
                                            // MCP or unknown tools — show tool name + first param
                                            let short_name = if other.starts_with("ext__") {
                                                other.splitn(3, "__").last().unwrap_or(other)
                                            } else {
                                                other
                                            };
                                            short_name.to_string()
                                        }
                                    };
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: detail,
                                        }));
                                    }
                                }
                                crate::StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                                    let preview: String = result.chars().take(300).collect();
                                    tool_log.push(format!("[tool_result]: {}", preview));
                                }
                                crate::StreamEvent::Session(SessionEvent::Usage {
                                    input_tokens, output_tokens,
                                    cache_read_input_tokens, cache_creation_input_tokens,
                                    cache_creation_5m, cache_creation_1h,
                                    model: _,
                                }) => {
                                    first_request.on_usage(input_tokens, cache_read_input_tokens, cache_creation_input_tokens);
                                    total_input_tokens += input_tokens;
                                    total_output_tokens += output_tokens;
                                    total_cache_read += cache_read_input_tokens;
                                    total_cache_creation += cache_creation_input_tokens;
                                    crate::core::rpc_dispatch::merge_split(&mut total_cache_5m, cache_creation_5m);
                                    crate::core::rpc_dispatch::merge_split(&mut total_cache_1h, cache_creation_1h);
                                }
                                crate::StreamEvent::Session(SessionEvent::Error(e)) => {
                                    // Typed outcome from the engine — keep the
                                    // terminal category + correlation ID in the
                                    // subagent's failure state (spec §5.2).
                                    return Err(format!("{} [{}]", e.message, e.category_label()));
                                }
                                crate::StreamEvent::Session(SessionEvent::Done) => break,
                                _ => {}
                            }
                        }
                        _ = &mut timeout_fut => {
                            // Return partial work instead of just an error
                            let partial = timed_out_text(&timeout_header, &tool_log, &final_text);
                            return Ok(SubagentResult {
                                text: partial,
                                model: model_inner,
                                input_tokens: total_input_tokens,
                                output_tokens: total_output_tokens,
                                cache_read: total_cache_read,
                                cache_creation: total_cache_creation,
                                cache_creation_5m: total_cache_5m,
                                cache_creation_1h: total_cache_1h,
                                tool_count,
                                timed_out: true,
                            });
                        }
                    }
                }

                Ok(SubagentResult {
                    text: final_text,
                    model: model_inner,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    cache_read: total_cache_read,
                    cache_creation: total_cache_creation,
                    cache_creation_5m: total_cache_5m,
                    cache_creation_1h: total_cache_1h,
                    tool_count,
                    timed_out: false,
                })
            });

                terminal = terminal_of(&result);
                let _ = result_tx.send(result);
                // The worker's own lifecycle finishes its run's terminal
                // publication; its caller already has the result.
                let pending = settling.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(mut publication) = pending {
                    rt.block_on(async {
                        if tokio::time::timeout(reap::REAP_BOUND, &mut publication).await.is_err() {
                            tracing::warn!(
                                "subagent {id_inner}: its final record is still being written after {}s; its handle ends when the write does",
                                reap::REAP_BOUND.as_secs()
                            );
                            let _ = publication.await;
                        }
                    });
                }
            }));

            if let Err(panic_info) = result {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("Subagent thread panicked: {}", msg);
                // result_tx is consumed inside the closure, so we can't send here —
                // the oneshot receiver will see a RecvError, handled below.
            }
            // The thread's last act: it ends the handle if it was handed the
            // end, once its run's final record has been written (§5).
            worker_end_inner.exit(&policy_inner, &id_inner, terminal);
        }) {
            Ok(handle) => handle,
            Err(_) => {
                orchestration.rollback(&orchestration_id);
                return Err(RuntimeError::Tool("worker thread initialization failed".into()));
            }
        };
        if let Err(error) = orchestration.mark_running(&orchestration_id) {
            drop(shutdown_tx);
            let _ = thread_handle.join();
            orchestration.rollback(&orchestration_id);
            return Err(RuntimeError::Tool(error));
        }

        // A parent whose turn is cancelled drops this call while it waits:
        // the guard then ends the worker's handle once the worker stops, so
        // a cancelled call never keeps its delegation slot.
        let reap = reap::DropReap::arm(
            std::sync::Arc::clone(orchestration),
            orchestration_id.clone(),
            thread_handle,
            worker_end,
            seal,
        )
        .with_terminal(std::sync::Arc::clone(&terminal_phase));
        let result = result_rx.await;
        let elapsed = start_time.elapsed().as_secs_f64();

        drop(shutdown_tx);
        let terminal = match &result {
            Ok(result) => terminal_of(result),
            Err(_) => agent_core::orchestration::WorkerTerminal::Failed,
        };
        if terminal_phase.detached() {
            // A run with a plan whose final record is still being written
            // (§5): its worker thread ends the handle once the write has
            // ended. Until then the handle stays pending.
            reap.hand_over(terminal);
        } else if reap.disarm().join_and_claim() {
            // One-shot workers have no separate collect call: every outcome must pass
            // through terminal, collected, and reconciled before this tool exits.
            if let Some(policy) = &ctx.capabilities.orchestration {
                policy
                    .finish_one_shot(&orchestration_id, terminal)
                    .map_err(RuntimeError::Tool)?;
            }
        }

        let log_dir = crate::config::base_dir().join("logs").join("subagents");
        let _ = tokio::fs::create_dir_all(&log_dir).await;
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

        match result {
            Ok(Ok(sa_result)) => {
                let preview: String = sa_result.text.chars().take(120).collect();

                if let Some(ref tx) = ctx.channels.tx_events {
                    let v2_run = counted_run.get().map(String::as_str);
                    if let Some(usage) = aggregate_usage(&sa_result, v2_run, books_spend) {
                        let _ = tx.send(crate::StreamEvent::Session(usage));
                    }
                    let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: preview,
                        duration_secs: elapsed,
                    }));
                }

                let log_content = format!(
                    "# Subagent: {}\nDate: {}\nModel: {}\nTask: {}\nDuration: {:.1}s\nTokens: {}in/{}out ({}cr/{}cw)\n{}\nTools used: {}\n\n## Result\n\n{}\n",
                    label, timestamp, params["model"].as_str().unwrap_or("sonnet"),
                    task_preview, elapsed,
                    sa_result.input_tokens, sa_result.output_tokens,
                    sa_result.cache_read, sa_result.cache_creation,
                    admitted.log_line(), sa_result.tool_count, sa_result.text,
                );
                let log_path = super::subagent_log_path(&log_dir, &timestamp, &label, false);
                let _ = tokio::fs::write(&log_path, &log_content).await;

                Ok(admitted.with_notice(format!("[subagent:{}] {}", label, sa_result.text)))
            }
            Ok(Err(e)) => {
                if let Some(ref tx) = ctx.channels.tx_events {
                    let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: format!("ERROR: {}", e),
                        duration_secs: elapsed,
                    }));
                }
                let log_path = super::subagent_log_path(&log_dir, &timestamp, &label, true);
                let _ = tokio::fs::write(
                    &log_path,
                    format!(
                        "# Subagent ERROR: {}\nTask: {}\n{}\nError: {}\n",
                        label,
                        task_preview,
                        admitted.log_line(),
                        e
                    ),
                )
                .await;
                Ok(admitted.with_notice(format!("[subagent:{} ERROR] {}", label, e)))
            }
            Err(_) => {
                if let Some(ref tx) = ctx.channels.tx_events {
                    let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: "Task panicked or dropped".to_string(),
                        duration_secs: elapsed,
                    }));
                }
                Ok(format!(
                    "[subagent:{} ERROR] Subagent task panicked or was dropped",
                    label
                ))
            }
        }
    }
}

/// Append streamed text to the one-shot result. A new response's first
/// non-empty text is set off from the previous response's narration by a
/// blank line, as the reactive drive does: "…read them all." then "Found 2
/// files…" used to arrive glued as "…read them all.Found 2 files…".
fn append_response_text(out: &mut String, new_response: &mut bool, text: &str) {
    if text.is_empty() {
        return;
    }
    if std::mem::take(new_response) && !out.is_empty() && !out.ends_with('\n') {
        out.push_str("\n\n");
    }
    out.push_str(text);
}

/// The aggregate `Usage` a oneshot sends its parent when it returns `Ok`
/// (a timeout-partial `Ok` included). None when its worker booked through
/// its spend tap: the tap already counted every request, so it is the one
/// publisher (§6). A v2 run's aggregate (`v2_run`: the run whose ledger
/// counted `result`) is its ledger's and goes out at most once per run
/// ([`super::ledger::claim_aggregate`], 8f); a run without worker rollover
/// (`None`) sends its drive totals, as always.
fn aggregate_usage(
    result: &SubagentResult,
    v2_run: Option<&str>,
    books_spend: bool,
) -> Option<SessionEvent> {
    if books_spend {
        return None;
    }
    if let Some(run) = v2_run {
        if !super::ledger::claim_aggregate(run) {
            return None;
        }
    }
    Some(SessionEvent::Usage {
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
        cache_read_input_tokens: result.cache_read,
        cache_creation_input_tokens: result.cache_creation,
        // TTL split aggregated across the subagent's turns — forwarded so
        // the parent bills 1h writes at 2.0x, not 1.25x.
        cache_creation_5m: result.cache_creation_5m,
        cache_creation_1h: result.cache_creation_1h,
        model: Some(result.model.clone()),
    })
}

/// A run's terminal as its result reads: the caller's and the worker
/// thread's view of the same outcome.
fn terminal_of(
    result: &std::result::Result<SubagentResult, String>,
) -> agent_core::orchestration::WorkerTerminal {
    match result {
        Ok(result) if result.timed_out => agent_core::orchestration::WorkerTerminal::TimedOut,
        Ok(_) => agent_core::orchestration::WorkerTerminal::Completed,
        Err(_) => agent_core::orchestration::WorkerTerminal::Failed,
    }
}

/// A oneshot's timed-out partial result: its header ([`timeout_header`]),
/// the tool calls it made (`tool_log`), then the text it had written
/// (`text`). The oneshot's own loop and, for a run with a plan, the shared
/// drive word a timeout with it ([`super::drive::DriveShape::Oneshot`]).
pub(super) fn timed_out_text(header: &str, tool_log: &[String], text: &str) -> String {
    let mut partial = format!("[{header} — partial results below]\n\n");
    if !tool_log.is_empty() {
        partial.push_str(&tool_log.join("\n"));
        partial.push('\n');
    }
    if !text.is_empty() {
        partial.push_str("\n[partial response]:\n");
        partial.push_str(text);
    }
    partial
}

/// The header of a oneshot's timed-out result: a watch oneshot whose
/// lease's time bound the run (`lease_expired`) says the lease expired;
/// any other run timed out.
pub(super) fn timeout_header(limit_secs: u64, lease_expired: bool) -> String {
    if lease_expired {
        format!("WATCH LEASE EXPIRED after {limit_secs}s")
    } else {
        format!("TIMED OUT after {limit_secs}s")
    }
}

/// Where a worker's run with a plan keeps its records (§3.2): the trusted
/// archive root and this process's directory below it. A oneshot's, a
/// start's and a resume's run take it from here; only a oneshot honors the
/// test fields below.
#[derive(Clone)]
pub(super) struct RecordSite {
    pub root: std::path::PathBuf,
    pub process_dir: String,
    /// Test scaffolding, never set in production: the run's record
    /// directory as its writes see it (a test injects faults).
    #[cfg(test)]
    pub test_wrap: Option<RecordWrap>,
}

/// [`RecordSite::test_wrap`].
#[cfg(test)]
pub(super) type RecordWrap = std::sync::Arc<
    dyn Fn(super::record_store::RecordDir) -> std::sync::Arc<dyn super::record_store::RecordFs>
        + Send
        + Sync,
>;

impl RecordSite {
    /// `<base>/subagent-history/<this process>`, as for reactive workers.
    pub(super) fn live() -> Self {
        Self::at(
            super::archive::archive_root(),
            super::archive::process_dir_name().to_owned(),
        )
    }

    /// The process directory `process_dir` below the archive `root`.
    pub(super) fn at(root: std::path::PathBuf, process_dir: String) -> Self {
        Self {
            root,
            process_dir,
            #[cfg(test)]
            test_wrap: None,
        }
    }
}

/// What a oneshot's run with a plan needs besides its runtime and plan.
struct Planned {
    task: String,
    timeout_secs: u64,
    tx_events: Option<tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>>,
    subagent_id: u64,
    label: String,
    model: String,
    /// Fires when the oneshot's result was taken or its caller dropped it.
    shutdown: tokio::sync::oneshot::Receiver<()>,
    /// Where the run is with its terminal publication.
    phase: std::sync::Arc<super::launch::TerminalPhase>,
    /// Where the run leaves a terminal publication that outlasted the
    /// bound its caller waits, for the worker thread to drive to its end.
    settling: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Where the run leaves its run id once its ledger counted the result
    /// (8f), for the aggregate's once-per-run guard ([`aggregate_usage`]).
    counted_run: std::sync::Arc<std::sync::OnceLock<String>>,
    /// Logs the provider's count for the run's first request next to its
    /// admission figure, as the oneshot's own loop does.
    first_request: super::request_size::FirstRequest,
    /// A watch oneshot's lease for its one run, and its usage before it
    /// (none); `None` for an ordinary worker. The drive heads a stop on the
    /// lease's time as the lease's expiry, as the oneshot's own loop does.
    lease: Option<(
        super::watch_admission::WatchLease,
        super::watch_admission::WatchUsage,
    )>,
    /// [`RecordSite::test_wrap`].
    #[cfg(test)]
    test_wrap: Option<RecordWrap>,
}

/// A oneshot's run with a plan (slice 8c): the launch, records and drive
/// of a reactive worker ([`super::launch::run_worker`]), on one clock (the
/// oneshot's timeout is the run deadline, §5; no timer of its own), worded
/// as the oneshot's own loop words its result
/// ([`super::drive::DriveShape::Oneshot`]). A caller that drops the oneshot
/// cancels the run, which ends as cancelled. What the run shows its caller
/// besides its output follows the output ([`rollover_trailer`]). Its
/// caller waits for its terminal publication at most until
/// [`super::launch::terminal_bound`]; one still running then goes on in
/// [`Planned::settling`].
async fn run_planned(
    mut runtime: crate::Runtime,
    plan: super::launch::LaunchPlan,
    run: Planned,
) -> std::result::Result<SubagentResult, String> {
    use std::sync::{Arc, RwLock};
    let opening: Vec<crate::SharedMessage> =
        vec![Arc::new(json!({"role": "user", "content": run.task.clone()}))];
    let state = Arc::new(RwLock::new(crate::runtime::subagent::SubagentState::new()));
    state.write().unwrap_or_else(|p| p.into_inner()).history = Some(opening.clone());
    let cancel = crate::CancellationToken::new();
    {
        let cancel = cancel.clone();
        let state = Arc::clone(&state);
        let shutdown = run.shutdown;
        tokio::spawn(async move {
            let _ = shutdown.await;
            // Before the run ends this is its caller going away: the run
            // is cancelled, and its final says so.
            state.write().unwrap_or_else(|p| p.into_inner()).cancel_requested = true;
            cancel.cancel();
        });
    }
    let run_clock = super::launch::arm_run_clock(Some(&plan), &mut runtime, run.timeout_secs);
    super::launch::arm_request_ledger(Some(&plan), &mut runtime);
    // Where the run's ledger is once the launch makes it a v2 run (8f).
    let ledger = plan.ledger.clone();
    let runtime = &mut runtime;
    let stream_cancel = cancel.clone();
    // A v2 run's runtime is bound to its run and gets the parent's settings
    // resolved for it (§2, §7.2) before its first inference; a fallback
    // gets nothing.
    let start = move |opening: Vec<crate::SharedMessage>,
                      start: Option<super::launch::WorkerStart>| async move {
        if let Some(start) = start {
            super::launch::start_v2_runtime(runtime, start, &opening);
        }
        runtime
            .run_stream_with_messages(opening, stream_cancel, None, None, false)
            .await
    };
    let drive = super::drive::WorkerDrive {
        state: Arc::clone(&state),
        tx_events: run.tx_events,
        subagent_id: run.subagent_id,
        label: run.label,
        model: run.model,
        timeout_secs: run.timeout_secs,
        run_clock,
        cancel,
        task: run.task,
        first_request: Some(run.first_request),
        lease: run.lease,
        // As the oneshot's own loop: its round bound is its runtime's turn
        // budget (`WatchLease::apply_to`), so a stop there fails in its own
        // words, never as the lease's.
        lease_binds_rounds: false,
        shape: super::drive::DriveShape::Oneshot,
    };
    let real = super::launch::real_fs;
    #[cfg(test)]
    let wrap: &dyn Fn(super::record_store::RecordDir) -> std::sync::Arc<dyn super::record_store::RecordFs> =
        match &run.test_wrap {
            Some(wrap) => &**wrap,
            None => &real,
        };
    #[cfg(not(test))]
    let wrap: &dyn Fn(super::record_store::RecordDir) -> std::sync::Arc<dyn super::record_store::RecordFs> =
        &real;
    let (outcome, settling) =
        super::launch::run_oneshot_worker(drive, opening, plan, start, wrap, run.phase).await;
    if let Some(publication) = settling {
        *run.settling.lock().unwrap_or_else(|p| p.into_inner()) = Some(publication);
    }
    let (trailer, v2_run) = {
        let s = state.read().unwrap_or_else(|p| p.into_inner());
        (rollover_trailer(&s), s.v2_run.clone())
    };
    match outcome {
        Ok(mut result) => {
            if let Some((run_id, ledger)) = v2_run.zip(ledger.get()) {
                count_from_ledger(&mut result, &ledger);
                let _ = run.counted_run.set(run_id);
            }
            result.text.push_str(&trailer);
            Ok(result)
        }
        Err(error) => Err(error + &trailer),
    }
}

/// A v2 run's counts are its ledger's measured conversation on its
/// worker's model (§6 "Slice 8", 8f): no rollover summary, no open
/// liability, each attempt once. Its aggregate usage and log say these,
/// and the aggregate goes out at most once for its run
/// ([`Planned::counted_run`], [`aggregate_usage`]).
fn count_from_ledger(result: &mut SubagentResult, ledger: &super::ledger::RequestLedgerHandle) {
    let usage = super::ledger::conversation_aggregate(&ledger.snapshot(), &result.model);
    result.input_tokens = usage.input_tokens;
    result.output_tokens = usage.output_tokens;
    result.cache_read = usage.cache_read_tokens;
    result.cache_creation = usage.cache_write_tokens;
    result.cache_creation_5m = usage.cache_write_5m;
    result.cache_creation_1h = usage.cache_write_1h;
}

/// What a oneshot shows its caller about its rollover after its output
/// (§8; a oneshot has no status, collect or completion event): nothing
/// unless something happened, so a run with a plan that made no rollover,
/// kept no notice and did not fall back reads as a run without one.
/// Otherwise a blank line, then one line each, in this order:
/// `[rollover]: <notice>` (it fell back to rollover off, or its thresholds
/// were reset), `[rollover windows]: window <W>; rollovers this run: <R>`
/// (only when `R > 0`), `[rollover notice]: <notice>` for each kept notice,
/// oldest first, and `[rollover notices dropped]: <D>` (only when `D > 0`).
/// Every notice is already one bounded plain line.
pub(super) fn rollover_trailer(state: &crate::runtime::subagent::SubagentState) -> String {
    let mut lines = Vec::new();
    if let Some(notice) = &state.rollover_notice {
        lines.push(format!("[rollover]: {notice}"));
    }
    if let Some(visibility) = &state.visibility {
        let windows = visibility.windows;
        if windows.rollovers_this_run > 0 {
            lines.push(format!(
                "[rollover windows]: window {}; rollovers this run: {}",
                windows.window_ordinal, windows.rollovers_this_run
            ));
        }
        for notice in visibility.notices() {
            lines.push(format!("[rollover notice]: {notice}"));
        }
        if visibility.dropped() > 0 {
            lines.push(format!("[rollover notices dropped]: {}", visibility.dropped()));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_response_starts_a_new_paragraph() {
        let mut out = String::new();
        let mut new_response = true;
        for (start, text) in [
            (true, "Let me find the files"),
            (false, " and read them all."),
            (true, ""),
            (false, "Found 2 files."),
            (true, "Here are the results:\n"),
            (true, "Total: 14"),
        ] {
            new_response |= start;
            append_response_text(&mut out, &mut new_response, text);
        }
        assert_eq!(
            out,
            "Let me find the files and read them all.\n\nFound 2 files.\n\nHere are the results:\nTotal: 14"
        );
    }
    use crate::tools::test_helpers::create_tool_context;
    use crate::tools::Tool;
    use serde_json::json;

    #[test]
    fn test_subagent_tool_schema() {
        let tool = SubagentTool;
        assert_eq!(tool.name(), "subagent");
        assert!(!tool.description().is_empty());

        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].is_object());
        assert!(params["required"].is_array());
    }

    // Runs a real worker: pin an empty base dir so it never reads the real
    // auth.json or writes its error log under the real ~/.synaps-cli, and
    // clear the environment's provider credentials/endpoints, so it fails
    // at auth, offline.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn test_subagent_blank_agent_uses_system_prompt() {
        let _base = crate::test_env::BaseDirGuard::new();
        let _offline = crate::test_env::OfflineProviderEnv::new();
        let tool = SubagentTool;
        let ctx = create_tool_context();
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
    }

    #[tokio::test]
    async fn a_polling_one_shot_on_a_large_model_is_refused_before_it_starts() {
        let ctx = create_tool_context();
        let orchestration = ctx.capabilities.orchestration.clone().expect("policy");
        // A worker dispatched earlier: the refusal must leave it as it is.
        orchestration
            .resolve_and_authorize("sa_earlier", None)
            .expect("an earlier dispatch");
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
        let text = SubagentTool
            .execute(params, ctx)
            .await
            .expect("a refusal is a tool result");
        assert!(
            text.starts_with("subagent refused (polling task)"),
            "{text}"
        );
        assert!(text.contains("/var/tmp/build/out.log"), "{text}");
        // The dispatch it authorized is rolled back (a Dispatched worker is
        // unreconciled until then). A one-shot reserves no tree edge.
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
mod oneshot_gate {
    use super::*;
    include!("oneshot_gate_tests.rs");

    mod dropped {
        use super::*;
        include!("oneshot_drop_tests.rs");
    }

    /// Child spend: a oneshot in a host with a sink books through its tap.
    mod spend {
        use super::*;
        include!("oneshot_spend_tests.rs");
    }
}

#[cfg(all(test, unix))]
mod oneshot_parity {
    use super::*;
    include!("oneshot_parity_tests.rs");
}
