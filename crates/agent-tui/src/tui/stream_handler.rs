//! Stream event handling — processes StreamEvent variants from the runtime.


use serde_json::json;
use synaps_cli::{CancellationToken, Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent};

use super::app::{App, ChatMessage, SubagentState, THINKING_PLACEHOLDER};
use super::draw::build_render_model;
use super::render_thread::RenderHandle;

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
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
            app.on_tool_use_start(tool_id, tool_name);
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
            app.on_tool_use_delta(&tool_id, &delta);
        }
        StreamEvent::Llm(LlmEvent::ToolUse { tool_name, tool_id, input }) => {
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            app.on_tool_use_finalized(tool_id, tool_name, input_str);
            return StreamAction::Continue;
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => {
            app.on_tool_result_delta(tool_id, delta);
            return StreamAction::Continue;
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => {
            app.on_tool_result(tool_id, result);
            return StreamAction::Continue;
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
                start_time: app.clock.now(),
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
            app.transcript.scroll_to_bottom();
            app.invalidate();
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model: usage_model,
        }) => {
            let model_for_pricing = usage_model.as_deref().unwrap_or(runtime.model());
            app.add_usage(
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model_for_pricing,
                Some(runtime.context_window()),
            );
        }
        StreamEvent::Session(SessionEvent::Notice(text)) => {
            // Display-only retry/status notice — system line, not transcript.
            // Notice text can echo API error bodies; strip control chars
            // (esp. ESC) so a hostile payload can't inject terminal escapes.
            app.push_msg(ChatMessage::System(sanitize_notice(&text)));
        }
        StreamEvent::Session(SessionEvent::Done) => {
            app.streaming = false;
            app.drop_empty_thinking();
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
            app.drop_empty_thinking();
            app.push_msg(ChatMessage::Error(sanitize_notice(&err)));
            app.streaming = false;
            app.subagents.clear();
            // Restore a valid trailing state — drop unmatched trailing messages
            if let Some(last) = app.api_messages.last() {
                let role = last["role"].as_str().unwrap_or("");
                let is_text_user = role == "user" && last["content"].is_string();
                let is_assistant = role == "assistant";
                if is_text_user || is_assistant {
                    // If we're dropping the user's own message (stream died
                    // before any assistant content), recover it into the input
                    // box instead of silently losing what they typed.
                    if is_text_user && app.input.is_empty() {
                        if let Some(text) = last["content"].as_str() {
                            app.input = text.to_string();
                            app.cursor_pos = app.input.chars().count();
                            app.push_msg(ChatMessage::System(
                                "your message was restored to the input box — press Enter to retry".to_string(),
                            ));
                        }
                    }
                    app.api_messages.pop();
                }
            }
        }
    }
    StreamAction::Continue
}


// ── P12.4: stream-lifecycle select! arms — pure code-motion from run(). ──
// The select! arm EXPRESSIONS (the event-queue `notified()` wake and the
// `stream.next()` polling future) stay inline in mod.rs; only the arm
// BODIES moved here, verbatim, apart from `*`-derefs for the loop-owned
// locals now borrowed via `&mut` and dropping the `stream_handler::` path
// prefix at the new scope. Behavior byte-identical — streaming lifecycle
// (delta → tool_use → done/abort) is the product's hot path.

/// The in-flight response stream, owned by the `run()` loop and lent to the
/// arm handlers below so they can clear/replace it exactly as the inline
/// arms did.
pub(super) type ActiveStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// Event-bus wake arm body: drain queued engine events into the transcript,
/// steer them into an active stream (or buffer), and auto-trigger a model
/// turn when idle.
pub(super) async fn handle_event_queue_arm(
    app: &mut App,
    runtime: &Runtime,
    secret_prompt_handle: &synaps_cli::tools::SecretPromptHandle,
    stream: &mut Option<ActiveStream>,
    cancel_token: &mut Option<CancellationToken>,
    steer_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
                let mut event_received = false;
                while let Some(event) = runtime.event_queue().pop() {
                    event_received = true;
                    let formatted = synaps_cli::events::format_event_for_agent(&event);
                    let severity_str = event.content.severity
                        .as_ref()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "medium".to_string());
                    app.push_msg(ChatMessage::Event {
                        source: event.source.source_type.clone(),
                        severity: severity_str,
                        text: event.content.text.clone(),
                    });

                    if app.streaming || app.compact_task.is_some() {
                        // Steer into active stream if possible, otherwise buffer
                        let steered = steer_tx.as_ref()
                            .map(|tx| tx.send(formatted.clone()).is_ok())
                            .unwrap_or(false);
                        if !steered {
                            app.pending_events.push(formatted);
                        }
                    } else {
                        app.api_messages.push(serde_json::json!({
                            "role": "user",
                            "content": formatted
                        }));
                    }
                    app.invalidate();
                }

                // Auto-trigger model turn when idle — only if we actually received events
                if event_received && !app.streaming && stream.is_none() && app.compact_task.is_none() && !app.api_messages.is_empty() {
                    if let Some(last) = app.api_messages.last() {
                        if last["role"].as_str() == Some("user") {
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.streaming = true;
                            app.spinner_frame = 0;
                            *stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                            *cancel_token = Some(ct);
                            *steer_tx = Some(s_tx);
                        }
                    }
                }
}

/// Stream-event arm body: route one `StreamEvent` through
/// [`handle_stream_event`] and act on the returned [`StreamAction`] —
/// continue (incl. Done/Error stream-state teardown + gamba reclaim),
/// auto-send a queued message, or auto-trigger buffered events.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_stream_arm(
    maybe_event: Option<StreamEvent>,
    app: &mut App,
    runtime: &Runtime,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &RenderHandle,
    secret_prompt_handle: &synaps_cli::tools::SecretPromptHandle,
    stream: &mut Option<ActiveStream>,
    cancel_token: &mut Option<CancellationToken>,
    steer_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
                if let Some(event) = maybe_event {
                    let do_draw = needs_immediate_draw(&event);
                    let action = handle_stream_event(event, app, runtime).await;

                    match action {
                        StreamAction::Continue => {
                            // For Done/Error, clear stream state
                            if !app.streaming {
                                *stream = None;
                                *cancel_token = None;
                                *steer_tx = None;
                                // Reclaim gamba if running — resume render thread
                                // after reclaim restores the terminal.
                                if let Some(msg) = app.reclaim_gamba() {
                                    render_handle.resume();
                                    app.push_msg(ChatMessage::System(msg));
                                    app.invalidate();
                                }
                            }
                        }
                        StreamAction::AutoSendQueued(queued) => {
                            // Drop old stream state (important for cleanup)
                            drop(stream.take());
                            drop(cancel_token.take());
                            drop(steer_tx.take());
                            // Reclaim gamba if running — resume render thread
                            // after reclaim restores the terminal.
                            if let Some(msg) = app.reclaim_gamba() {
                                render_handle.resume();
                                app.push_msg(ChatMessage::System(msg));
                                app.invalidate();
                            }
                            // Auto-send the queued message
                            app.push_msg(ChatMessage::User(queued.clone()));
                            app.transcript.scroll_to_bottom();
                            let api_content = if let Some(ref ctx) = app.abort_context {
                                let combined = format!("{}\n\n{}", ctx, queued);
                                app.abort_context = None;
                                combined
                            } else {
                                queued
                            };
                            app.api_messages.push(json!({"role": "user", "content": api_content}));
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.status_text = Some("connecting…".to_string());
                            app.streaming = true;
                            app.spinner_frame = 0;
                            let term_size = crossterm::terminal::size().map(|(w, h)| ratatui::layout::Size { width: w, height: h }).unwrap_or_default();
                            if let Some(model) = build_render_model(app, runtime, registry, term_size) {
                                render_handle.publish(model);
                            }
                            *stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.status_text = None;
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                            *cancel_token = Some(ct);
                            *steer_tx = Some(s_tx);
                        }
                        StreamAction::AutoTriggerEvents => {
                            drop(stream.take());
                            drop(cancel_token.take());
                            drop(steer_tx.take());
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.streaming = true;
                            app.spinner_frame = 0;
                            *stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                            *cancel_token = Some(ct);
                            *steer_tx = Some(s_tx);
                        }
                    }

                    if do_draw {
                        let term_size = crossterm::terminal::size().map(|(w, h)| ratatui::layout::Size { width: w, height: h }).unwrap_or_default();
                        if let Some(model) = build_render_model(app, runtime, registry, term_size) {
                            render_handle.publish(model);
                        }
                    }
                }
}

/// Strip ASCII control characters (except `\n` and `\t`) from notice text
/// before it reaches the render path. Notices can carry raw API error bodies;
/// an embedded ESC (0x1b) would otherwise inject terminal escape sequences.
fn sanitize_notice(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize_notice;

    #[test]
    fn test_sanitize_notice_strips_ansi_escape_payload() {
        let payload = "API retry \x1b[2J\x1b]0;pwned\x07 in 2s\nline two\tend";
        let clean = sanitize_notice(payload);
        assert_eq!(clean, "API retry [2J]0;pwned in 2s\nline two\tend");
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
    }

    #[test]
    fn test_sanitize_notice_passes_normal_text() {
        let s = "retrying (attempt 2/5) — overloaded";
        assert_eq!(sanitize_notice(s), s);
    }
}
