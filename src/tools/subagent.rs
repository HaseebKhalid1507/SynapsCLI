use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;
use crate::{Result, RuntimeError};
use super::{Tool, ToolContext, resolve_agent_prompt, NEXT_SUBAGENT_ID};

pub struct SubagentTool;

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str { "subagent" }

    fn description(&self) -> &str {
        "Dispatch a one-shot subagent with a specific system prompt to perform a task. The subagent gets its own tool suite (bash, read, write, edit, grep, find, ls) and runs autonomously until done. Use for delegation — give a focused task to a specialist agent. Provide either an agent name (resolves from ~/.synaps-cli/agents/<name>.md) or a system_prompt string directly."
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
                    "description": "Model override (default: claude-sonnet-4-20250514). Use claude-opus-4-6 for complex tasks."
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
        let task = params["task"].as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'task' parameter".to_string()))?
            .to_string();

        let agent_name = params["agent"].as_str().map(|s| s.to_string());
        let inline_prompt = params["system_prompt"].as_str().map(|s| s.to_string());
        let model_override = params["model"].as_str().map(|s| s.to_string());
        let timeout_secs = params["timeout"].as_u64().unwrap_or(300);

        let system_prompt = match (&agent_name, &inline_prompt) {
            (Some(name), _) => {
                resolve_agent_prompt(name)
                    .map_err(|e| RuntimeError::Tool(e))?
            }
            (None, Some(prompt)) => prompt.clone(),
            (None, None) => {
                return Err(RuntimeError::Tool(
                    "Must provide either 'agent' (name) or 'system_prompt' (inline). Got neither.".to_string()
                ));
            }
        };

        let label = agent_name.as_deref().unwrap_or("inline").to_string();
        let model = model_override.unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let task_preview: String = task.chars().take(80).collect();
        let subagent_id = NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed);

        tracing::info!("Dispatching subagent '{}' (id={}) with model {}", label, subagent_id, model);

        if let Some(ref tx) = ctx.tx_events {
            let _ = tx.send(crate::StreamEvent::SubagentStart {
                subagent_id,
                agent_name: label.clone(),
                task_preview: task_preview.clone(),
            });
        }

        let start_time = std::time::Instant::now();

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<std::result::Result<SubagentResult, String>>();
        let label_inner = label.clone();
        let model_inner = model.clone();
        let tx_events_inner = ctx.tx_events.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let _thread_handle = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = result_tx.send(Err(format!("Failed to create tokio runtime: {}", e)));
                    return;
                }
            };

            let result = rt.block_on(async move {
                use futures::StreamExt;

                let mut runtime = match crate::Runtime::new().await {
                    Ok(r) => r,
                    Err(e) => return Err(format!("Failed to create subagent runtime: {}", e)),
                };

                runtime.set_system_prompt(system_prompt);
                runtime.set_model(model);
                runtime.set_tools(crate::ToolRegistry::without_subagent());

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
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: "💭 thinking...".to_string(),
                                        });
                                    }
                                }
                                crate::StreamEvent::Text(text) => {
                                    final_text.push_str(&text);
                                }
                                crate::StreamEvent::ToolUseStart(name) => {
                                    tool_count += 1;
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: format!("⚙ {} (tool #{})", name, tool_count),
                                        });
                                    }
                                }
                                crate::StreamEvent::ToolUse { tool_name, input, .. } => {
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
                                            let name = input["agent"].as_str()
                                                .or_else(|| input["system_prompt"].as_str().map(|s| if s.len() > 20 { "inline" } else { s }))
                                                .unwrap_or("?");
                                            format!("spawning {}", name)
                                        }
                                        other => {
                                            // MCP or unknown tools — show tool name + first param
                                            let short_name = if other.starts_with("mcp__") {
                                                other.splitn(3, "__").last().unwrap_or(other)
                                            } else {
                                                other
                                            };
                                            format!("{}", short_name)
                                        }
                                    };
                                    if let Some(ref tx) = tx_events_inner {
                                        let _ = tx.send(crate::StreamEvent::SubagentUpdate {
                                            subagent_id,
                                            agent_name: label_inner.clone(),
                                            status: detail,
                                        });
                                    }
                                }
                                crate::StreamEvent::ToolResult { result, .. } => {
                                    let preview: String = result.chars().take(300).collect();
                                    tool_log.push(format!("[tool_result]: {}", preview));
                                }
                                crate::StreamEvent::Usage {
                                    input_tokens, output_tokens,
                                    cache_read_input_tokens, cache_creation_input_tokens,
                                    model: _,
                                } => {
                                    total_input_tokens += input_tokens;
                                    total_output_tokens += output_tokens;
                                    total_cache_read += cache_read_input_tokens;
                                    total_cache_creation += cache_creation_input_tokens;
                                }
                                crate::StreamEvent::Error(e) => {
                                    return Err(e);
                                }
                                crate::StreamEvent::Done => break,
                                _ => {}
                            }
                        }
                        _ = &mut timeout_fut => {
                            // Return partial work instead of just an error
                            let mut partial = format!("[TIMED OUT after {}s — partial results below]\n\n", timeout_secs);
                            if !tool_log.is_empty() {
                                partial.push_str(&tool_log.join("\n"));
                                partial.push('\n');
                            }
                            if !final_text.is_empty() {
                                partial.push_str("\n[partial response]:\n");
                                partial.push_str(&final_text);
                            }
                            return Ok(SubagentResult {
                                text: partial,
                                model: model_inner,
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
                    text: final_text,
                    model: model_inner,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    cache_read: total_cache_read,
                    cache_creation: total_cache_creation,
                    tool_count,
                })
            });

            let _ = result_tx.send(result);
        });

        let result = result_rx.await;
        let elapsed = start_time.elapsed().as_secs_f64();

        drop(shutdown_tx);

        let log_dir = crate::config::base_dir().join("logs").join("subagents");
        let _ = std::fs::create_dir_all(&log_dir);
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");

        match result {
            Ok(Ok(sa_result)) => {
                let preview: String = sa_result.text.chars().take(120).collect();

                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::Usage {
                        input_tokens: sa_result.input_tokens,
                        output_tokens: sa_result.output_tokens,
                        cache_read_input_tokens: sa_result.cache_read,
                        cache_creation_input_tokens: sa_result.cache_creation,
                        model: Some(sa_result.model),
                    });
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: preview,
                        duration_secs: elapsed,
                    });
                }

                let log_content = format!(
                    "# Subagent: {}\nDate: {}\nModel: {}\nTask: {}\nDuration: {:.1}s\nTokens: {}in/{}out ({}cr/{}cw)\nTools used: {}\n\n## Result\n\n{}\n",
                    label, timestamp, params["model"].as_str().unwrap_or("sonnet"),
                    task_preview, elapsed,
                    sa_result.input_tokens, sa_result.output_tokens,
                    sa_result.cache_read, sa_result.cache_creation,
                    sa_result.tool_count, sa_result.text,
                );
                let log_path = log_dir.join(format!("{}-{}.md", timestamp, label));
                let _ = std::fs::write(&log_path, &log_content);

                Ok(format!("[subagent:{}] {}", label, sa_result.text))
            }
            Ok(Err(e)) => {
                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: format!("ERROR: {}", e),
                        duration_secs: elapsed,
                    });
                }
                let log_path = log_dir.join(format!("{}-{}-error.md", timestamp, label));
                let _ = std::fs::write(&log_path, format!("# Subagent ERROR: {}\nTask: {}\nError: {}\n", label, task_preview, e));
                Ok(format!("[subagent:{} ERROR] {}", label, e))
            }
            Err(_) => {
                if let Some(ref tx) = ctx.tx_events {
                    let _ = tx.send(crate::StreamEvent::SubagentDone {
                        subagent_id,
                        agent_name: label.clone(),
                        result_preview: "Task panicked or dropped".to_string(),
                        duration_secs: elapsed,
                    });
                }
                Ok(format!("[subagent:{} ERROR] Subagent task panicked or was dropped", label))
            }
        }
    }
}

pub struct SubagentResult {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub tool_count: u32,
}