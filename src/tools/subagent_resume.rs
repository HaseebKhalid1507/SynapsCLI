//! SubagentResumeTool — restart a finished or timed-out subagent with new instructions.
//!
//! Takes the completed conversation state stored in the prior `SubagentHandle`,
//! prepends new instructions, and dispatches a fresh subagent via the same flow as
//! `subagent_start`. The caller gets a new `handle_id` for the continuation run.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::{Result, RuntimeError, LlmEvent, SessionEvent, AgentEvent};
use super::{Tool, ToolContext, NEXT_SUBAGENT_ID};
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentStatus, SubagentState};

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
                    "description": "Handle ID of the completed subagent to resume (e.g. \"sa_3\")."
                },
                "instructions": {
                    "type": "string",
                    "description": "New task or context to prepend to the resumed subagent. \
                                    Injected before the prior conversation history."
                }
            },
            "required": ["handle_id", "instructions"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let prior_handle_id = params["handle_id"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'handle_id' parameter".to_string()))?
            .to_string();

        let instructions = params["instructions"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'instructions' parameter".to_string()))?
            .to_string();

        let registry = ctx.capabilities.subagent_registry.as_ref()
            .ok_or_else(|| RuntimeError::Tool(
                "SubagentRegistry not available on this ToolContext".to_string()
            ))?;

        // Extract prior state under the lock, release immediately.
        let (agent_name, model, prior_context) = {
            let reg = registry.lock().unwrap();
            let handle = reg.get(&prior_handle_id)
                .ok_or_else(|| RuntimeError::Tool(
                    format!("No subagent found with handle_id '{}'", prior_handle_id)
                ))?;

            if handle.status() == SubagentStatus::Running {
                return Err(RuntimeError::Tool(format!(
                    "Subagent '{}' is still running. Call subagent_collect first, \
                     or wait until it finishes.",
                    prior_handle_id
                )));
            }

            let prior = {
                let state = handle.conversation_state();
                if state.is_empty() {
                    handle.partial_output()
                } else {
                    serde_json::to_string(&state).unwrap_or_else(|_| handle.partial_output())
                }
            };

            (handle.agent_name.clone(), handle.model.clone(), prior)
        };

        // ── Build resumed task: new instructions → separator → prior context.
        let resumed_task = format!(
            "{instructions}\n\n\
             ---\n\
             [Prior conversation context from handle {prior_handle_id}]\n\
             {prior_context}"
        );

        let system_prompt = "You are continuing a task that was interrupted. \
                             The prior conversation context is included in your task. \
                             Pick up where the previous agent left off.".to_string();

        let label = agent_name.clone();
        let timeout_secs = ctx.limits.subagent_timeout;
        let task_preview: String = resumed_task.chars().take(80).collect();
        let task_full = resumed_task.clone();
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);

        tracing::info!(
            "subagent_resume: dispatching '{}' (id={}, resumed_from={}) model={}",
            label, handle_id, prior_handle_id, model
        );

        let state = Arc::new(RwLock::new(SubagentState::new()));

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

        let state_t         = Arc::clone(&state);
        let task_full_a     = task_full.clone();
        let label_inner     = label.clone();
        let model_inner     = model.clone();
        let tx_events_inner = ctx.channels.tx_events.clone();
        let start_time      = std::time::Instant::now();

        let thread_handle = std::thread::spawn(move || {
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        state_t.write().unwrap().status =
                            SubagentStatus::Failed(format!("tokio runtime: {}", e));
                        return;
                    }
                };

                let state_a           = Arc::clone(&state_t);
                let label_a           = label_inner.clone();
                let model_a           = model_inner.clone();
                let tx_events_a       = tx_events_inner.clone();
                let task_for_timeout  = task_full_a.clone();
                let task_for_complete = task_full_a.clone();
                let task_for_stream   = task_full_a;

                let outcome: std::result::Result<SubagentResult, String> = rt.block_on(async move {
                    use futures::StreamExt;

                    let mut runtime = match crate::Runtime::new().await {
                        Ok(r) => r,
                        Err(e) => return Err(format!("Failed to create subagent runtime: {}", e)),
                    };

                    runtime.set_system_prompt(system_prompt);
                    runtime.set_model(model_a.clone());
                    runtime.set_tools(crate::ToolRegistry::without_subagent());

                    let cancel = crate::CancellationToken::new();
                    let cancel_inner = cancel.clone();
                    tokio::spawn(async move {
                        let _ = shutdown_rx.await;
                        cancel_inner.cancel();
                    });

                    let mut stream = runtime.run_stream_with_messages(
                        vec![serde_json::json!({"role": "user", "content": task_for_stream})],
                        cancel,
                        Some(steer_rx),
                    ).await;

                    let mut tool_count = 0u32;
                    let mut total_input_tokens = 0u64;
                    let mut total_output_tokens = 0u64;
                    let mut total_cache_read = 0u64;
                    let mut total_cache_creation = 0u64;

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
                                    crate::StreamEvent::Llm(LlmEvent::ToolUseStart(name)) => {
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
                                        model: _,
                                    }) => {
                                        total_input_tokens    += input_tokens;
                                        total_output_tokens   += output_tokens;
                                        total_cache_read      += cache_read_input_tokens;
                                        total_cache_creation  += cache_creation_input_tokens;
                                    }
                                    crate::StreamEvent::Session(SessionEvent::Error(e)) => return Err(e),
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
                                return Ok(SubagentResult {
                                    text,
                                    model: model_a.clone(),
                                    input_tokens: total_input_tokens,
                                    output_tokens: total_output_tokens,
                                    cache_read: total_cache_read,
                                    cache_creation: total_cache_creation,
                                    tool_count,
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
                        tool_count,
                    })
                });

                match outcome {
                    Ok(sa_result) => {
                        {
                            let mut s = state_t.write().unwrap();
                            if matches!(s.status, SubagentStatus::Running) {
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
                state_t.write().unwrap().status = SubagentStatus::Failed(format!("panic: {}", msg));
            }
        });

        let handle = SubagentHandle::new(
            handle_id.clone(),
            label.clone(),
            task_preview,
            model,
            timeout_secs,
            state,
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        );

        {
            let mut reg = registry.lock().unwrap();
            reg.register(handle);
            if let Some(h) = reg.get_mut(&handle_id) {
                h.set_thread_handle(thread_handle);
            }
        }

        Ok(json!({
            "handle_id":    handle_id,
            "resumed_from": prior_handle_id,
            "agent_name":   label,
            "status":       "running"
        }).to_string())
    }
}
