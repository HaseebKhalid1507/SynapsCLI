use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use serde_json::{json, Value};
use reqwest::Client;
use crate::{Result, RuntimeError, ToolRegistry};
use super::types::{AuthState, StreamEvent};
use super::helpers::HelperMethods;
use super::api::ApiMethods;

pub(super) struct StreamMethods;

impl StreamMethods {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_stream_internal(
        auth: Arc<RwLock<AuthState>>,
        client: Client,
        model: String,
        tools: Arc<RwLock<ToolRegistry>>,
        system_prompt: Option<String>,
        thinking_budget: u32,
        initial_messages: Vec<Value>,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
        mut steering_rx: Option<mpsc::UnboundedReceiver<String>>,
        watcher_exit_path: Option<PathBuf>,
    ) -> Result<()> {
        let mut messages = initial_messages;

        loop {
            // Check for cancellation before each API call
            if cancel.is_cancelled() {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Ok(());
            }

            // Refresh token before each API call in the tool loop — this is
            // the fix for stale tokens in long-running agentic sessions.
            {
                let auth_state = auth.read().await;
                if auth_state.auth_type == "oauth" {
                    let expired = match auth_state.token_expires {
                        Some(exp) => {
                            let now = crate::epoch_millis();
                            now >= exp
                        }
                        None => false,
                    };
                    if expired {
                        // Drop read lock before acquiring write
                        drop(auth_state);

                        tracing::info!("Refreshing token mid-stream");
                        let creds = crate::auth::ensure_fresh_token(&client)
                            .await
                            .map_err(|e| RuntimeError::Auth(format!(
                                "Token refresh failed mid-stream: {}. Run `login` to re-authenticate.", e
                            )))?;

                        let mut auth_w = auth.write().await;
                        auth_w.auth_token = creds.access;
                        auth_w.refresh_token = Some(creds.refresh);
                        auth_w.token_expires = Some(creds.expires);
                    }
                }
            }

            let tools_snapshot = tools.read().await.clone();
            let response = match ApiMethods::call_api_stream_inner(
                &auth, &client, &model, &tools_snapshot, &system_prompt, thinking_budget,
                &messages, tx.clone(), &cancel,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    // Send whatever history we have so far, so context isn't lost
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Err(e);
                }
            };

            // Check if Claude wants to use tools
            if let Some(content) = response["content"].as_array() {
                let mut tool_uses = Vec::new();

                // Process response content
                for item in content {
                    if item["type"].as_str() == Some("tool_use") {
                        tool_uses.push(item.clone());
                    }
                }

                // Add assistant's response to conversation
                messages.push(json!({
                    "role": "assistant",
                    "content": content
                }));

                // If no tool uses, check for steering messages before finishing.
                // Steering can redirect the model even when it has no more tool calls.
                if tool_uses.is_empty() {
                    let steered = HelperMethods::drain_steering(&mut steering_rx, &mut messages, &tx);
                    if !steered {
                        // No steering, truly done
                        let _ = tx.send(StreamEvent::MessageHistory(messages));
                        return Ok(());
                    }
                    // Steering message injected — continue the loop for another LLM call
                    continue;
                }

                // Execute tools and add results. We must always produce a tool_result for
                // every tool_use we just pushed onto the assistant message — otherwise the
                // next API call will fail with "tool_use ids were found without tool_result

                // Channel for dynamic tool registration (MCP connect uses this)
                let (tool_reg_tx, mut tool_reg_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Arc<dyn crate::Tool>>>();
                // blocks". On cancellation we synthesize a "Cancelled by user" result for any
                // remaining tools so message history stays valid.
                let mut tool_results = Vec::new();
                let mut cancelled = false;

                if cancel.is_cancelled() {
                    // Already cancelled before tool execution — fill all with cancel results
                    for tool_use in &tool_uses {
                        let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                        if !tool_id.is_empty() {
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": "Cancelled by user"
                            }));
                        }
                    }
                    cancelled = true;
                } else if tool_uses.len() == 1 {
                    // Single tool — run inline with delta streaming + cancellation
                    let tool_use = &tool_uses[0];
                    let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                    let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                    let input = tool_use["input"].clone();

                    if !tool_id.is_empty() && !tool_name.is_empty() {
                        let result = match tools.read().await.get(&tool_name).cloned() {
                            Some(tool) => {
                                let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                let tx_k = tx.clone();
                                let t_id = tool_id.clone();
                                tokio::spawn(async move {
                                    while let Some(msg) = rx_d.recv().await {
                                        let _ = tx_k.send(StreamEvent::ToolResultDelta {
                                            tool_id: t_id.clone(),
                                            delta: msg,
                                        });
                                    }
                                });

                                tokio::select! {
                                    res = tool.execute(input, crate::ToolContext { tx_delta: Some(tx_d), tx_events: Some(tx.clone()), watcher_exit_path: watcher_exit_path.clone(), tool_register_tx: Some(tool_reg_tx.clone()) }) => {
                                        match res {
                                            Ok(output) => output,
                                            Err(e) => format!("Tool execution failed: {}", e),
                                        }
                                    }
                                    _ = cancel.cancelled() => {
                                        cancelled = true;
                                        "Cancelled by user".to_string()
                                    }
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };

                        let _ = tx.send(StreamEvent::ToolResult {
                            tool_id: tool_id.clone(),
                            result: result.clone(),
                        });

                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": HelperMethods::truncate_tool_result(&result)
                        }));
                    }
                } else {
                    // Multiple tools — run in parallel with JoinSet
                    // Delta streaming is per-tool so each gets its own channel
                    let mut join_set = tokio::task::JoinSet::new();

                    for tool_use in &tool_uses {
                        let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                        let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                        let input = tool_use["input"].clone();

                        if tool_id.is_empty() || tool_name.is_empty() {
                            continue;
                        }

                        let tool = tools.read().await.get(&tool_name).cloned();
                        let tx_stream = tx.clone();
                        let cancel_token = cancel.clone();
                        let exit_path = watcher_exit_path.clone();
                        let tool_reg_tx_inner = tool_reg_tx.clone();

                        join_set.spawn(async move {
                            let result = match tool {
                                Some(t) => {
                                    let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    let tx_k = tx_stream.clone();
                                    let t_id = tool_id.clone();
                                    tokio::spawn(async move {
                                        while let Some(msg) = rx_d.recv().await {
                                            let _ = tx_k.send(StreamEvent::ToolResultDelta {
                                                tool_id: t_id.clone(),
                                                delta: msg,
                                            });
                                        }
                                    });

                                    tokio::select! {
                                        res = t.execute(input, crate::ToolContext { tx_delta: Some(tx_d), tx_events: Some(tx_stream.clone()), watcher_exit_path: exit_path.clone(), tool_register_tx: Some(tool_reg_tx_inner.clone()) }) => {
                                            match res {
                                                Ok(output) => (false, output),
                                                Err(e) => (false, format!("Tool execution failed: {}", e)),
                                            }
                                        }
                                        _ = cancel_token.cancelled() => {
                                            (true, "Cancelled by user".to_string())
                                        }
                                    }
                                }
                                None => (false, format!("Unknown tool: {}", tool_name)),
                            };

                            let _ = tx_stream.send(StreamEvent::ToolResult {
                                tool_id: tool_id.clone(),
                                result: result.1.clone(),
                            });

                            (tool_id, result.0, result.1)
                        });
                    }

                    // Collect results
                    let mut results_map = std::collections::HashMap::new();
                    while let Some(res) = join_set.join_next().await {
                        match res {
                            Ok((tool_id, was_cancelled, result)) => {
                                if was_cancelled { cancelled = true; }
                                results_map.insert(tool_id, result);
                            }
                            Err(e) => {
                                tracing::error!("Parallel tool task panicked: {}", e);
                            }
                        }
                    }

                    // Build tool_results in original order
                    for tool_use in &tool_uses {
                        if let Some(tool_id) = tool_use["id"].as_str() {
                            let result = results_map.remove(tool_id)
                                .unwrap_or_else(|| "Cancelled by user".to_string());
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": HelperMethods::truncate_tool_result(&result)
                            }));
                        }
                    }
                }

                // Drain dynamic tool registrations (e.g. from MCP connect)
                drop(tool_reg_tx); // close sender so recv returns None
                while let Ok(new_tools) = tool_reg_rx.try_recv() {
                    let mut registry = tools.write().await;
                    for tool in new_tools {
                        registry.register(tool);
                    }
                }

                // Add tool results to conversation — always, so the assistant's tool_use
                // blocks have matching tool_result blocks even on cancellation.
                messages.push(json!({
                    "role": "user",
                    "content": tool_results
                }));

                if cancelled {
                    // Send final history on cancellation so session can be saved
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Ok(());
                }

                // Check for steering messages between tool rounds.
                // These get injected as user messages before the next LLM call,
                // allowing the user to redirect the agent mid-work.
                HelperMethods::drain_steering(&mut steering_rx, &mut messages, &tx);

                // Continue the loop to get Claude's response with tool results
            } else {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }
}