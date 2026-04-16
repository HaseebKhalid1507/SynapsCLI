//! Agent event handling — processes AgentEvent variants from the bus.

use synaps_cli::transport::{AgentEvent, ToolEvent, SubagentEvent, MetaEvent};

use super::app::{App, ChatMessage, SubagentState};

/// What the event loop should do after processing an agent event.
pub(super) enum StreamAction {
    /// Continue processing — no special action needed.
    Continue,
    /// Turn completed and a queued message should be auto-sent.
    AutoSendQueued(String),
}

/// Returns true if the event should trigger an immediate redraw.
pub(super) fn needs_immediate_draw(event: &AgentEvent) -> bool {
    matches!(event,
        AgentEvent::Tool(ToolEvent::Invoke { .. })
        | AgentEvent::Tool(ToolEvent::Complete { .. })
        | AgentEvent::Subagent(_)
        | AgentEvent::Meta(MetaEvent::Steered { .. })
        | AgentEvent::MessageHistory(_)
        | AgentEvent::TurnComplete
        | AgentEvent::Error(_)
    )
}

/// Process an AgentEvent, update app state, return what the loop should do.
pub(super) fn handle_agent_event(
    event: AgentEvent,
    app: &mut App,
) -> StreamAction {
    match event {
        AgentEvent::Thinking(text) => {
            app.append_or_update_thinking(&text);
        }
        AgentEvent::Text(text) => {
            app.append_or_update_text(&text);
        }
        AgentEvent::Tool(ToolEvent::Start { tool_name, .. }) => {
            app.tool_start_time = Some(std::time::Instant::now());
            app.push_msg(ChatMessage::ToolUseStart(tool_name, String::new()));
        }
        AgentEvent::Tool(ToolEvent::ArgsDelta(delta)) => {
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolUseStart(_, ref mut partial) = last.msg {
                    partial.push_str(&delta);
                    app.dirty = true;
                    app.line_cache.clear();
                }
            }
        }
        AgentEvent::Tool(ToolEvent::Invoke { tool_name, input, .. }) => {
            app.tool_start_time = Some(std::time::Instant::now());
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolUseStart(name, _) = &last.msg {
                    if name == &tool_name {
                        last.msg = ChatMessage::ToolUse { tool_name, input: input_str };
                        app.dirty = true;
                        app.line_cache.clear();
                        return StreamAction::Continue;
                    }
                }
            }
            app.push_msg(ChatMessage::ToolUse { tool_name, input: input_str });
        }
        AgentEvent::Tool(ToolEvent::OutputDelta { delta, .. }) => {
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolResult { ref mut content, .. } = last.msg {
                    content.push_str(&delta);
                    app.dirty = true;
                    app.line_cache.clear();
                    return StreamAction::Continue;
                }
            }
            app.push_msg(ChatMessage::ToolResult { content: delta, elapsed_ms: None });
        }
        AgentEvent::Tool(ToolEvent::Complete { result, elapsed_ms, .. }) => {
            if let Some(last) = app.messages.last_mut() {
                if let ChatMessage::ToolResult { ref mut content, elapsed_ms: ref mut el, .. } = last.msg {
                    *content = result;
                    *el = elapsed_ms;
                    app.dirty = true;
                    app.line_cache.clear();
                    return StreamAction::Continue;
                }
            }
            app.push_msg(ChatMessage::ToolResult { content: result, elapsed_ms });
        }
        AgentEvent::Subagent(SubagentEvent::Start { id, agent_name, task_preview }) => {
            app.subagents.push(SubagentState {
                id,
                name: agent_name,
                status: format!("starting: {}", task_preview),
                start_time: std::time::Instant::now(),
                done: false,
                duration_secs: None,
            });
            app.dirty = true;
            app.line_cache.clear();
        }
        AgentEvent::Subagent(SubagentEvent::Update { id, status, .. }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == id) {
                sa.status = status;
            }
            app.dirty = true;
            app.line_cache.clear();
        }
        AgentEvent::Subagent(SubagentEvent::Done { id, result_preview, duration_secs, .. }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == id) {
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
            app.dirty = true;
            app.line_cache.clear();
        }
        AgentEvent::Meta(MetaEvent::Steered { message }) => {
            app.push_msg(ChatMessage::User(message.clone()));
            if app.queued_message.as_ref() == Some(&message) {
                app.queued_message = None;
            }
            app.scroll_back = 0;
            app.scroll_pinned = true;
            app.dirty = true;
            app.line_cache.clear();
        }
        AgentEvent::Meta(MetaEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            cost_usd,
            ..
        }) => {
            app.add_usage(input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd);
        }
        AgentEvent::Meta(MetaEvent::SessionStats { .. }) => {
            // Stats tracked by driver; TUI doesn't need these
        }
        AgentEvent::Meta(MetaEvent::Shutdown { .. }) => {
            // Driver is shutting down — TUI will exit on its own
        }
        AgentEvent::MessageHistory(h) => {
            app.api_messages = h;
        }
        AgentEvent::TurnComplete => {
            app.streaming = false;
            app.subagents.clear();

            // Check for queued message to auto-send
            if let Some(queued) = app.queued_message.take() {
                return StreamAction::AutoSendQueued(queued);
            }
        }
        AgentEvent::Error(err) => {
            app.push_msg(ChatMessage::Error(err));
            app.streaming = false;
            app.subagents.clear();
        }
    }
    StreamAction::Continue
}
