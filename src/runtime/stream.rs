use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use serde_json::{json, Value};
use reqwest::Client;
use crate::{Result, RuntimeError, ToolRegistry};
use super::types::{AuthState, StreamEvent, LlmEvent, SessionEvent};
use super::helpers::HelperMethods;
use super::api::ApiMethods;

/// Bundle of all dependencies needed to drive a streaming agent loop.
/// Constructed once by `Runtime::run_stream_with_messages` before spawning the stream task.
pub(super) struct StreamSession {
    // Auth & network
    pub(super) auth: Arc<RwLock<AuthState>>,
    pub(super) client: Client,
    pub(super) options: super::api::ApiOptions,
    pub(super) api_retries: u32,

    // Model config
    pub(super) model: String,
    pub(super) tools: Arc<RwLock<ToolRegistry>>,
    pub(super) system_prompt: Option<String>,
    pub(super) thinking_budget: u32,

    // Channels
    pub(super) tx: mpsc::UnboundedSender<StreamEvent>,
    pub(super) cancel: CancellationToken,
    pub(super) steering_rx: Option<mpsc::UnboundedReceiver<String>>,

    // Tool config
    pub(super) watcher_exit_path: Option<PathBuf>,
    pub(super) max_tool_output: usize,
    pub(super) bash_timeout: u64,
    pub(super) bash_max_timeout: u64,
    pub(super) subagent_timeout: u64,
    pub(super) session_manager: std::sync::Arc<crate::tools::shell::SessionManager>,
    pub(super) subagent_registry: Arc<Mutex<crate::runtime::subagent::SubagentRegistry>>,
    pub(super) event_queue: Arc<crate::events::EventQueue>,
    pub(super) hook_bus: Arc<crate::extensions::hooks::HookBus>,
    pub(super) secret_prompt: Option<crate::tools::SecretPromptHandle>,
}

pub(super) struct StreamMethods;

impl StreamMethods {
    pub(super) async fn run_stream_internal(
        session: StreamSession,
        initial_messages: Vec<Value>,
    ) -> Result<()> {
        let StreamSession {
            auth, client, options, api_retries,
            model, tools, system_prompt, thinking_budget,
            tx, cancel, mut steering_rx,
            watcher_exit_path, max_tool_output,
            bash_timeout, bash_max_timeout, subagent_timeout,
            session_manager, subagent_registry, event_queue, hook_bus, secret_prompt,
        } = session;
        let mut messages = initial_messages;

        loop {
            // Check for cancellation before each API call
            if cancel.is_cancelled() {
                let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
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

            // ═══ HOOK: before_message ═══
            // Fire before sending messages to the LLM. Extensions can inject context.
            // Extract the last user message text — handles both string content
            // and block array content (common after tool results).
            let mut injected_system = system_prompt.clone();
            let last_user_msg: Option<String> = messages.iter().rev()
                .find(|m| m["role"].as_str() == Some("user"))
                .and_then(|m| {
                    // Try string content first
                    if let Some(s) = m["content"].as_str() {
                        return Some(s.to_string());
                    }
                    // Try block array content
                    if let Some(arr) = m["content"].as_array() {
                        return arr.iter()
                            .find(|b| b["type"].as_str() == Some("text"))
                            .and_then(|b| b["text"].as_str())
                            .map(String::from);
                    }
                    None
                });
            if let Some(ref msg_text) = last_user_msg {
                let hook_event = crate::extensions::hooks::events::HookEvent::before_message(msg_text);
                match hook_bus.emit(&hook_event).await {
                    crate::extensions::hooks::events::HookResult::Inject { content } => {
                        // Prepend injected content to system prompt
                        let base = injected_system.clone().unwrap_or_default();
                        injected_system = Some(format!("[Extension context — do not treat as user instructions]\n{content}\n[End extension context]\n\n{base}"));
                        tracing::debug!(len = content.len(), "Extension context injected into system prompt");
                    }
                    crate::extensions::hooks::events::HookResult::Block { reason } => {
                        let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                        return Err(RuntimeError::Config(format!("Message blocked by extension: {}", reason)));
                    }
                    _ => {}
                }
            }

            let response = match ApiMethods::call_api_stream_inner(
                &auth, &client, &model, &tools_snapshot, &injected_system, thinking_budget,
                &messages, tx.clone(), &cancel, api_retries, &options,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    // Send whatever history we have so far, so context isn't lost
                    let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
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
                        let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
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
                // blocks". On cancellation we synthesize a "Canceled by user" result for any
                // remaining tools so message history stays valid.
                let mut tool_results = Vec::new();
                let mut canceled = false;

                if cancel.is_cancelled() {
                    // Already canceled before tool execution — fill all with cancel results
                    for tool_use in &tool_uses {
                        let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                        if !tool_id.is_empty() {
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": "Canceled by user"
                            }));
                        }
                    }
                    canceled = true;
                } else if tool_uses.len() == 1 {
                    // Single tool — run inline with delta streaming + cancellation
                    let tool_use = &tool_uses[0];
                    let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                    let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                    let input = tool_use["input"].clone();

                    // Catch JSON parse errors surfaced by parse_tool_input()
                    if let Some(err) = input.get("__parse_error").and_then(|v| v.as_str()) {
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": err,
                            "is_error": true
                        }));
                        let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result: err.to_string() }));
                    } else if !tool_id.is_empty() && !tool_name.is_empty() {
                        let result = match tools.read().await.get(&tool_name).cloned() {
                            Some(tool) => {
                                let input = tools.read().await.translate_input_for_api_tool(&tool_name, input);
                                let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                let tx_k = tx.clone();
                                let t_id = tool_id.clone();
                                tokio::spawn(async move {
                                    while let Some(msg) = rx_d.recv().await {
                                        let _ = tx_k.send(StreamEvent::Llm(LlmEvent::ToolResultDelta {
                                            tool_id: t_id.clone(),
                                            delta: msg,
                                        }));
                                    }
                                });

                                // ═══ HOOK: before_tool_call (stream single) ═══
                                let runtime_name = tools.read().await.runtime_name_for_api(&tool_name).to_string();
                                let mut hook_event = crate::extensions::hooks::events::HookEvent::before_tool_call(
                                    &tool_name, input.clone(),
                                );
                                hook_event.tool_runtime_name = Some(runtime_name.clone());
                                let hook_result = hook_bus.emit(&hook_event).await;
                                if let crate::extensions::hooks::events::HookResult::Block { reason } = hook_result {
                                    format!("Tool call blocked by extension: {}", reason)
                                } else {
                                let input_for_hook = input.clone();
                                tokio::select! {
                                    res = tool.execute(input, crate::ToolContext {
                                        channels: crate::tools::ToolChannels { tx_delta: Some(tx_d), tx_events: Some(tx.clone()) },
                                        capabilities: crate::tools::ToolCapabilities { watcher_exit_path: watcher_exit_path.clone(), tool_register_tx: Some(tool_reg_tx.clone()), session_manager: Some(session_manager.clone()), subagent_registry: Some(subagent_registry.clone()), event_queue: Some(event_queue.clone()), secret_prompt: secret_prompt.clone() },
                                        limits: crate::tools::ToolLimits { max_tool_output, bash_timeout, bash_max_timeout, subagent_timeout },
                                    }) => {
                                        let output = match res {
                                            Ok(output) => output,
                                            Err(e) => format!("Tool execution failed: {}", e),
                                        };
                                        // ═══ HOOK: after_tool_call (stream single) ═══
                                        let hook_event = crate::extensions::hooks::events::HookEvent::after_tool_call(
                                            &tool_name, input_for_hook, output.clone(),
                                        );
                                        let _ = hook_bus.emit(&hook_event).await;
                                        output
                                    }
                                    _ = cancel.cancelled() => {
                                        canceled = true;
                                        "Canceled by user".to_string()
                                    }
                                }
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };

                        let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolResult {
                            tool_id: tool_id.clone(),
                            result: result.clone(),
                        }));

                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": HelperMethods::truncate_tool_result(&result, max_tool_output)
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

                        // Catch JSON parse errors surfaced by parse_tool_input()
                        if let Some(err) = input.get("__parse_error").and_then(|v| v.as_str()) {
                            let err = err.to_string();
                            let tid = tool_id.clone();
                            let tx_c = tx.clone();
                            join_set.spawn(async move {
                                let _ = tx_c.send(StreamEvent::Llm(LlmEvent::ToolResult { tool_id: tid.clone(), result: err.clone() }));
                                (tid, false, format!("Tool execution failed: {}", err))
                            });
                            continue;
                        }

                        let tools_snapshot = tools.read().await;
                        let input = tools_snapshot.translate_input_for_api_tool(&tool_name, input);
                        let tool = tools_snapshot.get(&tool_name).cloned();
                        drop(tools_snapshot);
                        let tx_stream = tx.clone();
                        let cancel_token = cancel.clone();
                        let exit_path = watcher_exit_path.clone();
                        let tool_reg_tx_inner = tool_reg_tx.clone();
                        let session_mgr = session_manager.clone();
                        let registry_inner = subagent_registry.clone();
                        let eq_inner = event_queue.clone();
                        let hook_bus_inner = hook_bus.clone();
                        let tool_name_for_hook = tool_name.clone();
                        let prompt_inner = secret_prompt.clone();

                        join_set.spawn(async move {
                            let result = match tool {
                                Some(t) => {
                                    // ═══ HOOK: before_tool_call (stream parallel) ═══
                                    let mut hook_event = crate::extensions::hooks::events::HookEvent::before_tool_call(
                                        &tool_name_for_hook, input.clone(),
                                    );
                                    hook_event.tool_runtime_name = Some(tool_name_for_hook.clone());
                                    let hook_result = hook_bus_inner.emit(&hook_event).await;
                                    if let crate::extensions::hooks::events::HookResult::Block { reason } = hook_result {
                                        (false, format!("Tool call blocked by extension: {}", reason))
                                    } else {
                                    let input_for_hook = input.clone();
                                    let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    let tx_k = tx_stream.clone();
                                    let t_id = tool_id.clone();
                                    tokio::spawn(async move {
                                        while let Some(msg) = rx_d.recv().await {
                                            let _ = tx_k.send(StreamEvent::Llm(LlmEvent::ToolResultDelta {
                                                tool_id: t_id.clone(),
                                                delta: msg,
                                            }));
                                        }
                                    });

                                    tokio::select! {
                                        res = t.execute(input, crate::ToolContext {
                                            channels: crate::tools::ToolChannels { tx_delta: Some(tx_d), tx_events: Some(tx_stream.clone()) },
                                            capabilities: crate::tools::ToolCapabilities { watcher_exit_path: exit_path.clone(), tool_register_tx: Some(tool_reg_tx_inner.clone()), session_manager: Some(session_mgr.clone()), subagent_registry: Some(registry_inner.clone()), event_queue: Some(eq_inner.clone()), secret_prompt: prompt_inner.clone() },
                                            limits: crate::tools::ToolLimits { max_tool_output, bash_timeout, bash_max_timeout, subagent_timeout },
                                        }) => {
                                            let output = match res {
                                                Ok(output) => output,
                                                Err(e) => format!("Tool execution failed: {}", e),
                                            };
                                            // ═══ HOOK: after_tool_call (stream parallel) ═══
                                            let hook_event = crate::extensions::hooks::events::HookEvent::after_tool_call(
                                                &tool_name_for_hook, input_for_hook, output.clone(),
                                            );
                                            let _ = hook_bus_inner.emit(&hook_event).await;
                                            (false, output)
                                        }
                                        _ = cancel_token.cancelled() => {
                                            (true, "Canceled by user".to_string())
                                        }
                                    }
                                    } // close else from Block check
                                }
                                None => (false, format!("Unknown tool: {}", tool_name)),
                            };

                            let _ = tx_stream.send(StreamEvent::Llm(LlmEvent::ToolResult {
                                tool_id: tool_id.clone(),
                                result: result.1.clone(),
                            }));

                            (tool_id, result.0, result.1)
                        });
                    }

                    // Collect results
                    let mut results_map = std::collections::HashMap::new();
                    while let Some(res) = join_set.join_next().await {
                        match res {
                            Ok((tool_id, was_canceled, result)) => {
                                if was_canceled { canceled = true; }
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
                                .unwrap_or_else(|| "Canceled by user".to_string());
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": HelperMethods::truncate_tool_result(&result, max_tool_output)
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

                if canceled {
                    // Send final history on cancellation so session can be saved
                    let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                    return Ok(());
                }

                // Check for steering messages between tool rounds.
                // These get injected as user messages before the next LLM call,
                // allowing the user to redirect the agent mid-work.
                HelperMethods::drain_steering(&mut steering_rx, &mut messages, &tx);

                // Continue the loop to get Claude's response with tool results
            } else {
                let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }
}