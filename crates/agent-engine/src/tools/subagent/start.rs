//! SubagentStartTool — dispatch a reactive subagent and return a handle_id immediately.
//!
//! Unlike the one-shot `subagent` tool, this tool returns *before* the subagent
//! finishes. The caller gets a `handle_id` they can poll via `subagent_status`,
//! steer via `subagent_steer`, or block on via `subagent_collect`.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::{Result, RuntimeError, LlmEvent, SessionEvent, AgentEvent};
use super::super::{Tool, ToolContext, resolve_agent_prompt, NEXT_SUBAGENT_ID};
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentStatus, SubagentState};

pub struct SubagentStartTool;

#[async_trait::async_trait]
impl Tool for SubagentStartTool {
    fn name(&self) -> &str { "subagent_start" }

    fn description(&self) -> &str {
        "Dispatch a reactive subagent and return immediately with a handle_id. \
         The subagent runs in the background — use subagent_status to poll, \
         subagent_steer to inject guidance mid-run, and subagent_collect to poll for the result (non-blocking — call \
         repeatedly until done). Use this for parallel execution or when you \
         want to continue working while the subagent runs. For simple sequential \
         delegation, use subagent instead. Provide either an agent name (resolves \
         from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly."
    }

    fn parameters(&self) -> Value {
        json!({
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
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 300). Increase for long-running tasks."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        // ── Parse params ───────────────────────────────────────────────────────
        let task = params["task"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();

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
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);
        let decision = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?
            .resolve_and_authorize(&handle_id, requested_model)
            .map_err(|error| RuntimeError::Tool(error.to_string()))?;
        let model = decision.model.as_str().to_owned();
        let timeout_secs = params["timeout"]
            .as_u64()
            .unwrap_or(ctx.limits.subagent_timeout);

        let label = agent_name.as_deref().unwrap_or("inline").to_string();
        let task_preview: String = task.chars().take(80).collect();
        let task_full = task.clone();

        tracing::info!("subagent_start: dispatching '{}' (id={}) model={}", label, handle_id, model);

        // ── Shared state ───────────────────────────────────────────────────────
        let state = Arc::new(RwLock::new(SubagentState::new()));

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
        let state_t          = Arc::clone(&state);
        let task_full_a      = task_full.clone();
        let label_inner      = label.clone();
        let model_inner      = model.clone();
        let tx_events_inner  = ctx.channels.tx_events.clone();
        let start_time       = std::time::Instant::now();
        let parent_queue     = ctx.capabilities.event_queue.clone();
        let handle_id_inner  = handle_id.clone();

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
            reg.register(handle);
        }

        let orchestration = ctx.capabilities.orchestration.as_ref().unwrap();
        if let Err(error) = orchestration.mark_starting(&handle_id) {
            orchestration.rollback(&handle_id);
            return Err(RuntimeError::Tool(error));
        }

        // ── Spawn subagent thread (mirrors subagent.rs) ────────────────────────
        let thread_handle = std::thread::spawn(move || {
            // Pre-clone for finalizer — catch_unwind moves state_t and label_inner
            let state_for_finalizer = Arc::clone(&state_t);
            let label_for_finalizer = label_inner.clone();

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
                let state_a        = Arc::clone(&state_t);
                let label_a        = label_inner.clone();
                let model_a        = model_inner.clone();
                let tx_events_a    = tx_events_inner.clone();
                let task_for_timeout = task_full_a.clone();
                let task_for_complete = task_full_a;

                let outcome: std::result::Result<SubagentResult, String> = rt.block_on(async move {
                    use futures::StreamExt;

                    let mut runtime = match crate::Runtime::new().await {
                        Ok(r) => r,
                        Err(_) => return Err("subagent runtime initialization failed".into()),
                    };

                    // Apply subagent spawn policy: inherit credential source AND
                    // unconditionally force cache TTL to 5m. Subagents are short-lived
                    // one-shots — paying the 1h write premium (~2× input price) on them
                    // is unrecoverable waste (~$0.23 per 10-spawn fan-out). (#110)
                    super::apply_subagent_runtime_policy(&mut runtime, &crate::config::load_config());
                    runtime.set_system_prompt(system_prompt);
                    runtime.set_model(model_a.clone());
                    runtime.set_tools(super::subagent_tools().await);

                    let cancel = crate::CancellationToken::new();
                    let cancel_inner = cancel.clone();
                    tokio::spawn(async move {
                        let _ = shutdown_rx.await;
                        cancel_inner.cancel();
                    });

                    let mut stream = runtime.run_stream_with_messages(vec![std::sync::Arc::new(serde_json::json!({"role": "user", "content": task}))], cancel, Some(steer_rx), None, false).await;

                    let mut tool_count = 0u32;
                    let mut total_input_tokens = 0u64;
                    let mut total_output_tokens = 0u64;
                    let mut total_cache_read = 0u64;
                    let mut total_cache_creation = 0u64;
                    // TTL split: None only if no turn ever reported one; otherwise summed.
                    let mut total_cache_5m: Option<u64> = None;
                    let mut total_cache_1h: Option<u64> = None;

                    let timeout_fut = tokio::time::sleep(Duration::from_secs(timeout_secs));
                    tokio::pin!(timeout_fut);

                    loop {
                        tokio::select! {
                            event = stream.next() => {
                                let Some(event) = event else { break };
                                match event {
                                    crate::StreamEvent::Llm(LlmEvent::Thinking(_)) => {
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: "💭 thinking...".to_string(),
                                            }));
                                        }
                                    }
                                    crate::StreamEvent::Llm(LlmEvent::Text(text)) => {
                                        state_a.write().unwrap().partial_text.push_str(&text);
                                    }
                                    crate::StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name: name, .. }) => {
                                        tool_count += 1;
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: format!("⚙ {} (tool #{})", name, tool_count),
                                            }));
                                        }
                                    }
                                    crate::StreamEvent::Llm(LlmEvent::ToolUse { tool_name, input, .. }) => {
                                        let input_str = input.to_string();
                                        let input_preview: String = input_str.chars().take(200).collect();
                                        state_a.write().unwrap().tool_log
                                            .push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                                        let detail = match tool_name.as_str() {
                                            "bash" => {
                                                let cmd = input["command"].as_str().unwrap_or("");
                                                let preview: String = cmd.chars().take(60).collect();
                                                format!("$ {}", preview)
                                            }
                                            "read"  => format!("reading {}", input["path"].as_str().unwrap_or("?").rsplit('/').next().unwrap_or("?")),
                                            "write" => format!("writing {}", input["path"].as_str().unwrap_or("?").rsplit('/').next().unwrap_or("?")),
                                            "edit"  => format!("editing {}", input["path"].as_str().unwrap_or("?").rsplit('/').next().unwrap_or("?")),
                                            "grep"  => format!("grep /{}/", input["pattern"].as_str().unwrap_or("?").chars().take(30).collect::<String>()),
                                            "find"  => format!("find {}", input["pattern"].as_str().unwrap_or("?")),
                                            "ls"    => format!("ls {}", input["path"].as_str().unwrap_or(".").rsplit('/').next().unwrap_or(".")),
                                            other   => {
                                                if other.starts_with("ext__") {
                                                    other.splitn(3, "__").last().unwrap_or(other).to_string()
                                                } else {
                                                    other.to_string()
                                                }
                                            }
                                        };
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::Agent(AgentEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: detail,
                                            }));
                                        }
                                    }
                                    crate::StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                                        let preview: String = result.chars().take(300).collect();
                                        state_a.write().unwrap().tool_log
                                            .push(format!("[tool_result]: {}", preview));
                                    }
                                    crate::StreamEvent::Session(SessionEvent::Usage {
                                        input_tokens, output_tokens,
                                        cache_read_input_tokens, cache_creation_input_tokens,
                                        cache_creation_5m, cache_creation_1h,
                                        model: _,
                                    }) => {
                                        total_input_tokens    += input_tokens;
                                        total_output_tokens   += output_tokens;
                                        total_cache_read      += cache_read_input_tokens;
                                        total_cache_creation  += cache_creation_input_tokens;
                                        crate::core::rpc_dispatch::merge_split(&mut total_cache_5m, cache_creation_5m);
                                        crate::core::rpc_dispatch::merge_split(&mut total_cache_1h, cache_creation_1h);
                                    }
                                    crate::StreamEvent::Session(SessionEvent::Error(_)) => return Err("provider request failed".into()),
                                    crate::StreamEvent::Session(SessionEvent::Done) => break,
                                    _ => {}
                                }
                            }
                            _ = &mut timeout_fut => {
                                let (partial, log) = {
                                    let mut s = state_a.write().unwrap();
                                    s.status = SubagentStatus::TimedOut;
                                    s.conversation_state = vec![
                                        serde_json::json!({"role": "user", "content": task_for_timeout.clone()}),
                                        serde_json::json!({"role": "assistant", "content": &s.partial_text}),
                                    ];
                                    (s.partial_text.clone(), s.tool_log.clone())
                                };
                                let mut text = format!("[TIMED OUT after {}s — partial results below]\n\n", timeout_secs);
                                if !log.is_empty() {
                                    text.push_str(&log.join("\n"));
                                    text.push('\n');
                                }
                                if !partial.is_empty() {
                                    text.push_str("\n[partial response]:\n");
                                    text.push_str(&partial);
                                }
                                state_a.write().unwrap_or_else(|p| p.into_inner()).partial_text = text.clone();
                                return Ok(SubagentResult {
                                    text,
                                    model: model_a.clone(),
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
                        text: state_a.write().unwrap().partial_text.clone(),
                        model: model_a.clone(),
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
                        // drop result_tx — collect() will surface the closed channel
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
                state_t.write().unwrap_or_else(|p| p.into_inner()).status = SubagentStatus::Failed(format!("panic: {}", msg));
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
                None,  // start.rs: not a resume
            );
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
            return Err(RuntimeError::Tool(error));
        }

        Ok(json!({
            "handle_id":  handle_id,
            "agent_name": label,
            "status":     "running"
        }).to_string())
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
        assert!(result.is_err(), "missing registry must return Err before spawn");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("subagent_registry"),
            "error must mention subagent_registry: {msg}"
        );
    }

    #[tokio::test]
    async fn test_subagent_start_blank_agent_uses_system_prompt() {
        let tool = SubagentStartTool;
        let mut ctx = create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::new(Mutex::new(SubagentRegistry::new())));

        let params = json!({
            "agent": "",
            "system_prompt": "You are a concise test subagent. Reply with only: ok",
            "task": "Say ok",
            "model": "anthropic/claude-sonnet-4-6",
            "timeout": 1
        });

        let result = tool.execute(params, ctx).await;
        assert!(result.is_ok(), "blank agent should not be resolved as ~/.synaps-cli/agents/.md: {result:?}");
        let body: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(body["agent_name"], "inline");
        assert!(body["handle_id"].as_str().unwrap_or_default().starts_with("sa_"));
    }
}
