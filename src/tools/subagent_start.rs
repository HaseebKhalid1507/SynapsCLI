//! SubagentStartTool — dispatch a reactive subagent and return a handle_id immediately.
//!
//! Unlike the one-shot `subagent` tool, this tool returns *before* the subagent
//! finishes. The caller gets a `handle_id` they can poll via `subagent_status`,
//! steer via `subagent_steer`, or block on via `subagent_collect`.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, resolve_agent_prompt, NEXT_SUBAGENT_ID};
use crate::runtime::subagent::{SubagentHandle, SubagentResult, SubagentStatus, SubagentState};

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
                    "description": "Model override (default: claude-opus-4-7). Use claude-sonnet-4-6 for lighter tasks."
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

        let agent_name    = params["agent"].as_str().map(|s| s.to_string());
        let inline_prompt = params["system_prompt"].as_str().map(|s| s.to_string());
        let model_override = params["model"].as_str().map(|s| s.to_string());
        let timeout_secs   = params["timeout"].as_u64().unwrap_or(ctx.subagent_timeout);

        let system_prompt = match (&agent_name, &inline_prompt) {
            (Some(name), _) => resolve_agent_prompt(name).map_err(RuntimeError::Tool)?,
            (None, Some(p)) => p.clone(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). Got neither.".to_string()
                ));
            }
        };

        let label = agent_name.as_deref().unwrap_or("inline").to_string();
        let model = model_override.unwrap_or_else(|| crate::models::default_model().to_string());
        let task_preview: String = task.chars().take(80).collect();
        let task_full = task.clone();
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);
        let handle_id = format!("sa_{}", subagent_id);

        tracing::info!("subagent_start: dispatching '{}' (id={}) model={}", label, handle_id, model);

        // ── Shared state ───────────────────────────────────────────────────────
        let state = Arc::new(Mutex::new(SubagentState::new()));

        // ── Channels ───────────────────────────────────────────────────────────
        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<SubagentResult>();

        // ── Forward SubagentStart event to TUI ─────────────────────────────────
        if let Some(ref tx) = ctx.tx_events {
            let _ = tx.send(crate::StreamEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            });
        }

        // ── Clone state for the spawned thread ─────────────────────────────────
        let state_t          = Arc::clone(&state);
        let task_full_a      = task_full.clone();
        let label_inner      = label.clone();
        let model_inner      = model.clone();
        let tx_events_inner  = ctx.tx_events.clone();
        let start_time       = std::time::Instant::now();

        // ── Spawn subagent thread (mirrors subagent.rs) ────────────────────────
        let _thread_handle = std::thread::spawn(move || {
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        state_t.lock().unwrap().status =
                            SubagentStatus::Failed(format!("tokio runtime: {}", e));
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

                    let mut stream = runtime.run_stream_with_messages(vec![serde_json::json!({"role": "user", "content": task})], cancel, Some(steer_rx)).await;

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
                                    crate::StreamEvent::Thinking(_) => {
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: "💭 thinking...".to_string(),
                                            });
                                        }
                                    }
                                    crate::StreamEvent::Text(text) => {
                                        state_a.lock().unwrap().partial_text.push_str(&text);
                                    }
                                    crate::StreamEvent::ToolUseStart(name) => {
                                        tool_count += 1;
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: format!("⚙ {} (tool #{})", name, tool_count),
                                            });
                                        }
                                    }
                                    crate::StreamEvent::ToolUse { tool_name, input, .. } => {
                                        let input_str = input.to_string();
                                        let input_preview: String = input_str.chars().take(200).collect();
                                        state_a.lock().unwrap().tool_log
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
                                                if other.starts_with("mcp__") {
                                                    other.splitn(3, "__").last().unwrap_or(other).to_string()
                                                } else {
                                                    other.to_string()
                                                }
                                            }
                                        };
                                        if let Some(ref tx) = tx_events_a {
                                            let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                                subagent_id,
                                                agent_name: label_a.clone(),
                                                status: detail,
                                            });
                                        }
                                    }
                                    crate::StreamEvent::ToolResult { result, .. } => {
                                        let preview: String = result.chars().take(300).collect();
                                        state_a.lock().unwrap().tool_log
                                            .push(format!("[tool_result]: {}", preview));
                                    }
                                    crate::StreamEvent::Usage {
                                        input_tokens, output_tokens,
                                        cache_read_input_tokens, cache_creation_input_tokens,
                                        model: _,
                                    } => {
                                        total_input_tokens    += input_tokens;
                                        total_output_tokens   += output_tokens;
                                        total_cache_read      += cache_read_input_tokens;
                                        total_cache_creation  += cache_creation_input_tokens;
                                    }
                                    crate::StreamEvent::Error(e) => return Err(e),
                                    crate::StreamEvent::Done => break,
                                    _ => {}
                                }
                            }
                            _ = &mut timeout_fut => {
                                let (partial, log) = {
                                    let mut s = state_a.lock().unwrap();
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
                        text: state_a.lock().unwrap().partial_text.clone(),
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
                        // Only overwrite Running → Completed (don't stomp TimedOut).
                        {
                            let mut s = state_t.lock().unwrap();
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
                            let _ = tx.send(crate::StreamEvent::SubagentDone {
                                subagent_id,
                                agent_name: label_inner.clone(),
                                result_preview: preview,
                                duration_secs: elapsed,
                            });
                        }
                        let _ = result_tx.send(sa_result);
                    }
                    Err(e) => {
                        state_t.lock().unwrap().status = SubagentStatus::Failed(e.clone());
                        let elapsed = start_time.elapsed().as_secs_f64();
                        if let Some(ref tx) = tx_events_inner {
                            let _ = tx.send(crate::StreamEvent::SubagentDone {
                                subagent_id,
                                agent_name: label_inner.clone(),
                                result_preview: format!("ERROR: {}", e),
                                duration_secs: elapsed,
                            });
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
                state_t.lock().unwrap().status = SubagentStatus::Failed(format!("panic: {}", msg));
            }
        });

        // ── Build handle + register ────────────────────────────────────────────
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

        if let Some(registry) = &ctx.subagent_registry {
            registry.lock().unwrap().register(handle);
        } else {
            return Err(RuntimeError::Tool(
                "subagent_start requires a subagent_registry in ToolContext".to_string()
            ));
        }

        Ok(json!({
            "handle_id":  handle_id,
            "agent_name": label,
            "status":     "running"
        }).to_string())
    }
}
