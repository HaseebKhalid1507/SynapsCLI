//! Stream event handling — processes StreamEvent variants from the runtime.


use synaps_cli::{Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent};

use super::app::{App, ChatMessage, SubagentState};

/// What the event loop should do after processing a stream event.
pub(super) enum StreamAction {
    /// Continue processing — no special action needed.
    Continue,
    /// Stream completed and a queued message should be auto-sent.
    AutoSendQueued(String),
    /// Stream completed and buffered events need a model turn.
    AutoTriggerEvents,
}

/// Returns true if the event should trigger an immediate redraw.
pub(super) fn needs_immediate_draw(event: &StreamEvent) -> bool {
    matches!(event,
        StreamEvent::Llm(LlmEvent::ToolUse { .. })
        | StreamEvent::Llm(LlmEvent::ToolResult { .. })
        | StreamEvent::Agent(AgentEvent::SubagentStart { .. })
        | StreamEvent::Agent(AgentEvent::SubagentUpdate { .. })
        | StreamEvent::Agent(AgentEvent::SubagentDone { .. })
        | StreamEvent::Agent(AgentEvent::SteeringDelivered { .. })
        | StreamEvent::Session(SessionEvent::Done)
        | StreamEvent::Session(SessionEvent::Error(_))
    )
}

/// Process a StreamEvent, update app state, return what the loop should do.
pub(super) async fn handle_stream_event(
    event: StreamEvent,
    app: &mut App,
    runtime: &Runtime,
) -> StreamAction {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(text)) => {
            app.append_or_update_thinking(&text);
        }
        StreamEvent::Llm(LlmEvent::Text(text)) => {
            app.append_or_update_text(&text);
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart(name)) => {
            app.tool_start_time = Some(std::time::Instant::now());
            app.push_msg(ChatMessage::ToolUseStart(name, String::new()));
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta(delta)) => {
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolUseStart(_, ref mut partial) = last.msg {
                    partial.push_str(&delta);
                    app.invalidate();
                }
            }
        }
        StreamEvent::Llm(LlmEvent::ToolUse { tool_name, input, .. }) => {
            app.tool_start_time = Some(std::time::Instant::now());
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolUseStart(name, _) = &last.msg {
                    if name == &tool_name {
                        last.msg = ChatMessage::ToolUse { tool_name, input: input_str };
                        app.invalidate();
                        return StreamAction::Continue;
                    }
                }
            }
            app.push_msg(ChatMessage::ToolUse { tool_name, input: input_str });
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, .. }) => {
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolResult { ref mut content, .. } = last.msg {
                    content.push_str(&delta);
                    app.invalidate();
                    return StreamAction::Continue;
                }
            }
            app.push_msg(ChatMessage::ToolResult { content: delta, elapsed_ms: None });
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
            let elapsed = app.tool_start_time.take()
                .map(|t| t.elapsed().as_millis() as u64);
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolResult { ref mut content, elapsed_ms: ref mut el, .. } = last.msg {
                    *content = result;
                    *el = elapsed;
                    app.invalidate();
                    return StreamAction::Continue;
                }
            }
            app.push_msg(ChatMessage::ToolResult { content: result, elapsed_ms: elapsed });
        }
        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
            app.api_messages = history;
            app.save_session().await;
        }
        StreamEvent::Agent(AgentEvent::SubagentStart { subagent_id, agent_name, task_preview }) => {
            app.subagents.push(SubagentState {
                id: subagent_id,
                name: agent_name,
                status: format!("starting: {}", task_preview),
                start_time: std::time::Instant::now(),
                done: false,
                duration_secs: None,
            });
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SubagentUpdate { subagent_id, status, .. }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.status = status;
            }
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SubagentDone { subagent_id, result_preview, duration_secs, .. }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.done = true;
                sa.duration_secs = Some(duration_secs);
                let preview: String = result_preview.chars().take(40).collect();
                if result_preview.starts_with("[TIMED OUT") {
                    sa.status = "\u{26a0} timed out".to_string();
                } else if result_preview.starts_with("ERROR") {
                    sa.status = format!("\u{2718} {}", preview);
                } else {
                    sa.status = format!("\u{2714} {}", preview);
                }
            }
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            app.push_msg(ChatMessage::User(message.clone()));
            if app.queued_message.as_ref() == Some(&message) {
                app.queued_message = None;
            }
            app.scroll_back = 0;
            app.scroll_pinned = true;
            app.invalidate();
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            model: usage_model,
        }) => {
            let model_for_pricing = usage_model.as_deref().unwrap_or(runtime.model());
            app.add_usage(
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                model_for_pricing,
                Some(runtime.context_window()),
            );
        }
        StreamEvent::Session(SessionEvent::Done) => {
            app.streaming = false;
            app.subagents.clear();
            // Clean up finished reactive subagent handles
            if let Some(registry) = runtime.subagent_registry().lock().ok().as_mut() {
                registry.cleanup_finished();
            }

            // Flush events that arrived during streaming into api_messages
            let had_pending = !app.pending_events.is_empty();
            for formatted in app.pending_events.drain(..) {
                app.api_messages.push(serde_json::json!({
                    "role": "user",
                    "content": formatted
                }));
            }

            // Check for queued message to auto-send
            if let Some(queued) = app.queued_message.take() {
                return StreamAction::AutoSendQueued(queued);
            }

            // If events arrived during streaming, trigger a new model turn
            if had_pending {
                app.save_session().await;
                return StreamAction::AutoTriggerEvents;
            }
        }
        StreamEvent::Session(SessionEvent::Error(err)) => {
            app.push_msg(ChatMessage::Error(err));
            app.streaming = false;
            app.subagents.clear();
            // Restore a valid trailing state — drop unmatched trailing messages
            if let Some(last) = app.api_messages.last() {
                let role = last["role"].as_str().unwrap_or("");
                let is_text_user = role == "user" && last["content"].is_string();
                let is_assistant = role == "assistant";
                if is_text_user || is_assistant {
                    app.api_messages.pop();
                }
            }
        }
    }
    StreamAction::Continue
}
