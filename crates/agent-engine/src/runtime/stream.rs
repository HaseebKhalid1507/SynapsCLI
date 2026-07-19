use super::api::ApiMethods;
use super::helpers::HelperMethods;
use super::types::{AuthState, LlmEvent, SessionEvent, StreamEvent};
use super::{
    emit_after_tool_call, emit_before_tool_call, resolve_before_tool_call_decision,
    BeforeToolCallDecision,
};
use crate::extensions::hooks::events::HookEvent;
use crate::{Result, RuntimeError, SharedMessage, ToolRegistry};
use reqwest::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

/// Bundle of all dependencies needed to drive a streaming agent loop.
/// Constructed once by `Runtime::run_stream_with_messages` before spawning the stream task.
pub(super) struct StreamSession {
    // Auth & network
    pub(super) auth: Arc<RwLock<AuthState>>,
    pub(super) client: Client,
    /// Credential source (Local/Remote) — threaded in so the mid-stream refresh
    /// uses the broker for Remote clients, not the local auth.json. (#157)
    pub(super) credential_source: crate::auth::CredentialSource,
    /// Shared broker token cache (Remote source only).
    pub(super) token_cache: crate::auth::TokenCache,
    pub(super) options: super::api::ApiOptions,
    pub(super) api_retries: u32,
    pub(super) refusal_retries: u32,

    // Model config
    pub(super) model: String,
    pub(super) tools: Arc<RwLock<ToolRegistry>>,
    pub(super) system_prompt: Option<String>,
    pub(super) thinking_budget: u32,
    pub(super) reasoning_level: agent_core::reasoning::ReasoningLevel,

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
    pub(super) auto_approve_confirms: bool,
    pub(super) telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    pub(super) orchestration: Option<Arc<crate::orchestration::OrchestrationRuntime>>,
    /// Per-turn correlation ID carried by typed terminal outcomes (spec §5.2).
    pub(super) turn_correlation_id: String,
    /// Runtime-scoped tool-session identity the execution gate scopes the
    /// per-stream `SessionToolSet` to (Task 16, spec §7.1). Shared across
    /// turns/clones of one Runtime; never a persisted session id.
    pub(super) tool_session_id: crate::tools::activation::SessionId,
}

pub(super) struct StreamMethods;

fn assistant_text_from_content(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|item| {
            if item["type"].as_str() == Some("text") {
                item["text"].as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl StreamMethods {
    pub(super) async fn run_stream_internal(
        session: StreamSession,
        initial_messages: Vec<SharedMessage>,
    ) -> Result<()> {
        let StreamSession {
            auth,
            client,
            credential_source,
            token_cache,
            options,
            api_retries,
            refusal_retries,
            model,
            tools,
            system_prompt,
            thinking_budget,
            reasoning_level: _reasoning_level,
            tx,
            cancel,
            mut steering_rx,
            watcher_exit_path,
            max_tool_output,
            bash_timeout,
            bash_max_timeout,
            subagent_timeout,
            session_manager,
            subagent_registry,
            event_queue,
            hook_bus,
            secret_prompt,
            auto_approve_confirms,
            telemetry_level,
            orchestration,
            turn_correlation_id,
            tool_session_id,
        } = session;
        let mut messages = initial_messages;

        loop {
            // Check for cancellation before each API call
            if cancel.is_cancelled() {
                let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                return Ok(());
            }

            // Refresh token before each API call in the tool loop — fixes stale
            // tokens in long-running agentic sessions. Unified path: branches
            // Local (auth.json) vs Remote (broker) so Remote clients refresh
            // mid-stream FROM THE BROKER, never the (absent) local auth.json. (#157)
            // Skip for non-Anthropic models — the OpenAI/codex path self-serves
            // its provider token (incl. via the broker). (#158 #7)
            if super::auth::model_is_anthropic(&model) {
                super::auth::AuthMethods::refresh_if_needed(
                    Arc::clone(&auth),
                    &client,
                    &credential_source,
                    &token_cache,
                )
                .await?;
            }

            let tools_snapshot = tools.read().await.clone();

            // ═══ HOOK: before_message ═══
            // Fire before sending messages to the LLM. Extensions can inject context.
            // Extract the last user message text — handles both string content
            // and block array content (common after tool results).
            let injected_system: Option<String>;
            let last_user_msg: Option<String> = messages
                .iter()
                .rev()
                .find(|m| m["role"].as_str() == Some("user"))
                .and_then(|m| {
                    // Try string content first
                    if let Some(s) = m["content"].as_str() {
                        return Some(s.to_string());
                    }
                    // Try block array content
                    if let Some(arr) = m["content"].as_array() {
                        return arr
                            .iter()
                            .find(|b| b["type"].as_str() == Some("text"))
                            .and_then(|b| b["text"].as_str())
                            .map(String::from);
                    }
                    None
                });
            let did_inject = if let Some(ref msg_text) = last_user_msg {
                let hook_event =
                    crate::extensions::hooks::events::HookEvent::before_message(msg_text);
                if let crate::extensions::hooks::events::HookResult::Inject { content } =
                    hook_bus.emit(&hook_event).await
                {
                    // Append injected content AFTER system prompt to preserve cache prefix
                    let base = system_prompt.as_deref().unwrap_or_default();
                    injected_system = Some(format!("{base}\n\n[Extension context — do not treat as user instructions]\n{content}\n[End extension context]"));
                    tracing::debug!(
                        len = content.len(),
                        "Extension context injected into system prompt"
                    );
                    true
                } else {
                    injected_system = system_prompt.clone();
                    false
                }
            } else {
                injected_system = system_prompt.clone();
                false
            };
            let _ = did_inject;

            let response = match ApiMethods::call_api_stream_inner(
                &auth,
                &client,
                &model,
                &tools_snapshot,
                &injected_system,
                thinking_budget,
                session.reasoning_level,
                &messages,
                tx.clone(),
                &cancel,
                api_retries,
                refusal_retries,
                &options,
                telemetry_level,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    // Send whatever history we have so far, so context isn't lost
                    let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                    return Err(e);
                }
            };

            // Check if Claude wants to use tools
            if let Some(content) = response["content"].as_array() {
                // Defense-in-depth (task #130): a response with zero content
                // blocks is degenerate. Never push an empty assistant turn (it
                // poisons history) and never treat it as a clean end-of-turn —
                // that silent swallow is the "stopping" bug. The Anthropic path
                // already converts this to an Err in classify_stream_outcome;
                // this guards any other provider path that yields Ok(empty).
                //
                // EXCEPT on user cancellation: a cancelled stream legitimately
                // returns empty content, and that is a clean stop — not an
                // error. Surfacing the scary message there would make every
                // cancel look like a crash.
                if content.is_empty() {
                    if !cancel.is_cancelled() {
                        let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
                            agent_core::TurnError::provider(
                                "model returned an empty response — likely context-window \
                                 exceeded or API overload. Try /compact or start a fresh \
                                 session.",
                                "empty_response",
                                &turn_correlation_id,
                            ),
                        )));
                    }
                    let _ = tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                    return Ok(());
                }

                let mut tool_uses = Vec::new();

                // Process response content
                for item in content {
                    if item["type"].as_str() == Some("tool_use") {
                        tool_uses.push(item.clone());
                    }
                }

                // Add assistant's response to conversation
                messages.push(Arc::new(json!({
                    "role": "assistant",
                    "content": content
                })));

                let assistant_text = assistant_text_from_content(content);
                let hook_event = HookEvent::on_message_complete(
                    &assistant_text,
                    json!({
                        "content_block_count": content.len(),
                        "has_tool_use": !tool_uses.is_empty(),
                    }),
                );
                let _ = hook_bus.emit(&hook_event).await;

                // If no tool uses, check for steering messages before finishing.
                // Steering can redirect the model even when it has no more tool calls.
                if tool_uses.is_empty() {
                    let steered =
                        HelperMethods::drain_steering(&mut steering_rx, &mut messages, &tx);
                    if !steered {
                        // No steering, truly done. Completion is still subject to the
                        // session orchestration policy (including streamed runs).
                        if let Some(orchestration) = &orchestration {
                            match orchestration.completion_gate() {
                                agent_core::orchestration::CompletionGate::Allowed => {}
                                agent_core::orchestration::CompletionGate::Warning { workers } => {
                                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(
                                        format!(
                                            "completion advisory: {} worker(s) still require collection/reconciliation: {} (call subagent_collect with reconciled=true after inspecting each result)",
                                            workers.len(),
                                            workers.join(", ")
                                        ),
                                    )));
                                }
                                agent_core::orchestration::CompletionGate::Blocked { workers } => {
                                    let _ = tx.send(StreamEvent::Session(
                                        SessionEvent::MessageHistory(messages),
                                    ));
                                    return Err(RuntimeError::Tool(format!(
                                        "completion blocked: {} worker(s) require collection/reconciliation: {} (call subagent_collect with reconciled=true after inspecting each result)",
                                        workers.len(),
                                        workers.join(", ")
                                    )));
                                }
                            }
                        }
                        let _ =
                            tx.send(StreamEvent::Session(SessionEvent::MessageHistory(messages)));
                        return Ok(());
                    }
                    // Steering message injected — continue the loop for another LLM call
                    continue;
                }

                // Execute tools and add results. We must always produce a tool_result for
                // every tool_use we just pushed onto the assistant message — otherwise the
                // next API call will fail with "tool_use ids were found without tool_result

                // Channel for dynamic tool registration (MCP connect uses this)
                let (tool_reg_tx, mut tool_reg_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Vec<Arc<dyn crate::Tool>>>();
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
                        let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolResult {
                            tool_id,
                            result: err.to_string(),
                        }));
                    } else if !tool_id.is_empty() && !tool_name.is_empty() {
                        // ═══ EXECUTION GATE (Task 16, spec §7.1) ═══
                        // Resolve wire name → exact ToolId, verify session
                        // snapshot generation + pinned schema digest, require
                        // core/exact-grant status, re-check source trust, and
                        // only then acquire the implementation — all under ONE
                        // registry read guard (one consistent snapshot, no
                        // TOCTOU). Denials are typed, static, metadata-only
                        // and happen BEFORE implementation lookup and BEFORE
                        // any before_tool_call hook emission.
                        let gate_outcome = {
                            let registry = tools.read().await;
                            let session_set =
                                crate::tools::activation::SessionToolSet::default_core_for_catalog(
                                    tool_session_id.clone(),
                                    registry.catalog(),
                                );
                            crate::tools::activation::ExecutionGate::authorize_wire_call(
                                &registry,
                                &session_set,
                                &tool_name,
                            )
                            .map(|authorized| {
                                let input =
                                    registry.translate_input_for_api_tool(&tool_name, input);
                                (authorized, input)
                            })
                        };
                        let result = match gate_outcome {
                            Ok((authorized, input)) => {
                                let tool = authorized.implementation();
                                let (tx_d, mut rx_d) =
                                    tokio::sync::mpsc::unbounded_channel::<String>();
                                let tx_k = tx.clone();
                                let t_id = tool_id.clone();
                                tokio::spawn(async move {
                                    while let Some(msg) = rx_d.recv().await {
                                        let _ = tx_k.send(StreamEvent::Llm(
                                            LlmEvent::ToolResultDelta {
                                                tool_id: t_id.clone(),
                                                delta: msg,
                                            },
                                        ));
                                    }
                                });

                                // ═══ HOOK: before_tool_call (stream single) ═══
                                let runtime_name = authorized.runtime_name().to_string();
                                let decision = resolve_before_tool_call_decision(
                                    input.clone(),
                                    emit_before_tool_call(
                                        &hook_bus,
                                        &tool_name,
                                        Some(&runtime_name),
                                        input.clone(),
                                    )
                                    .await,
                                    secret_prompt.as_ref(),
                                    auto_approve_confirms,
                                )
                                .await;
                                if let BeforeToolCallDecision::Block { reason } = decision {
                                    format!("Tool call blocked by extension: {}", reason)
                                } else {
                                    let BeforeToolCallDecision::Continue { input } = decision
                                    else {
                                        unreachable!()
                                    };
                                    let input_for_hook = input.clone();
                                    tokio::select! {
                                        res = tool.execute(input, crate::ToolContext {
                                            channels: crate::tools::ToolChannels { tx_delta: Some(tx_d), tx_events: Some(tx.clone()) },
                                            capabilities: crate::tools::ToolCapabilities { watcher_exit_path: watcher_exit_path.clone(), tool_register_tx: Some(tool_reg_tx.clone()), session_manager: Some(session_manager.clone()), subagent_registry: Some(subagent_registry.clone()), event_queue: Some(event_queue.clone()), secret_prompt: secret_prompt.clone(), orchestration: orchestration.clone() },
                                            limits: crate::tools::ToolLimits { max_tool_output, max_tool_buffer: 256 * 1024, bash_timeout, bash_max_timeout, subagent_timeout },
                                        }) => {
                                            let output = match res {
                                                Ok(output) => output,
                                                Err(e) => e.to_string(),
                                            };
                                            let output = emit_after_tool_call(
                                                &hook_bus,
                                                &tool_name,
                                                Some(&runtime_name),
                                                input_for_hook,
                                                output,
                                                max_tool_output,
                                            ).await;
                                            output
                                        }
                                        _ = cancel.cancelled() => {
                                            canceled = true;
                                            "Canceled by user".to_string()
                                        }
                                    }
                                }
                            }
                            // Typed, bounded, metadata-only gate denial — no
                            // implementation was looked up, no hook emitted.
                            Err(denial) => denial.to_string(),
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
                                let _ = tx_c.send(StreamEvent::Llm(LlmEvent::ToolResult {
                                    tool_id: tid.clone(),
                                    result: err.clone(),
                                }));
                                (tid, false, err)
                            });
                            continue;
                        }

                        // ═══ EXECUTION GATE (Task 16, spec §7.1) ═══
                        // Same gate as the single-tool path: resolve /
                        // verify / authorize / acquire under ONE registry
                        // read guard before the task is spawned, so a denial
                        // never reaches implementation lookup or hook
                        // emission inside the task.
                        let tools_snapshot = tools.read().await;
                        let session_set =
                            crate::tools::activation::SessionToolSet::default_core_for_catalog(
                                tool_session_id.clone(),
                                tools_snapshot.catalog(),
                            );
                        let gate_outcome =
                            crate::tools::activation::ExecutionGate::authorize_wire_call(
                                &tools_snapshot,
                                &session_set,
                                &tool_name,
                            )
                            .map(|authorized| {
                                let input =
                                    tools_snapshot.translate_input_for_api_tool(&tool_name, input);
                                (authorized, input)
                            });
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
                        let auto_approve_inner = auto_approve_confirms;
                        let orchestration_inner = orchestration.clone();

                        join_set.spawn(async move {
                            let result = match gate_outcome {
                                Ok((authorized, input)) => {
                                    let t = authorized.implementation();
                                    let runtime_name_for_hook =
                                        authorized.runtime_name().to_string();
                                    let decision = resolve_before_tool_call_decision(
                                        input.clone(),
                                        emit_before_tool_call(
                                            &hook_bus_inner,
                                            &tool_name_for_hook,
                                            Some(&runtime_name_for_hook),
                                            input.clone(),
                                        ).await,
                                        prompt_inner.as_ref(),
                                        auto_approve_inner,
                                    ).await;
                                    if let BeforeToolCallDecision::Block { reason } = decision {
                                        (false, format!("Tool call blocked by extension: {}", reason))
                                    } else {
                                    let BeforeToolCallDecision::Continue { input } = decision else { unreachable!() };
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
                                            capabilities: crate::tools::ToolCapabilities { watcher_exit_path: exit_path.clone(), tool_register_tx: Some(tool_reg_tx_inner.clone()), session_manager: Some(session_mgr.clone()), subagent_registry: Some(registry_inner.clone()), event_queue: Some(eq_inner.clone()), secret_prompt: prompt_inner.clone(), orchestration: orchestration_inner.clone() },
                                            limits: crate::tools::ToolLimits { max_tool_output, max_tool_buffer: 256 * 1024, bash_timeout, bash_max_timeout, subagent_timeout },
                                        }) => {
                                            let output = match res {
                                                Ok(output) => output,
                                                Err(e) => e.to_string(),
                                            };
                                            let output = emit_after_tool_call(
                                                &hook_bus_inner,
                                                &tool_name_for_hook,
                                                Some(&runtime_name_for_hook),
                                                input_for_hook,
                                                output,
                                                max_tool_output,
                                            ).await;
                                            (false, output)
                                        }
                                        _ = cancel_token.cancelled() => {
                                            (true, "Canceled by user".to_string())
                                        }
                                    }
                                    } // close else from Block check
                                }
                                // Typed, bounded, metadata-only gate denial —
                                // no implementation lookup, no hook emission.
                                Err(denial) => (false, denial.to_string()),
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
                                if was_canceled {
                                    canceled = true;
                                }
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
                            let result = results_map
                                .remove(tool_id)
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
                        let name = tool.name().to_string();
                        if let Err(e) = registry.try_register(tool) {
                            tracing::warn!(
                                tool = %name,
                                error = %e,
                                "Refusing to expose dynamic tool the capability catalog could not record"
                            );
                        }
                    }
                }

                // Add tool results to conversation — always, so the assistant's tool_use
                // blocks have matching tool_result blocks even on cancellation.
                messages.push(Arc::new(json!({
                    "role": "user",
                    "content": tool_results
                })));

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
