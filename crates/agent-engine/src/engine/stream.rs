//! Engine-level stream processing — TUI-agnostic event handling.
//!
//! Processes StreamEvent variants, tracks subagent state and usage,
//! and returns renderer-agnostic actions.

use crate::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// What happened during a stream event — renderer decides how to display.
#[derive(Debug)]
pub enum EngineStreamEvent {
    /// Thinking text arrived.
    Thinking(String),
    /// Response text arrived.
    Text(String),
    /// Tool use started.
    ToolStart { tool_id: String, tool_name: String },
    /// Tool use input delta.
    ToolDelta { tool_id: String, delta: String },
    /// Tool use finalized.
    ///
    /// `input` is the parsed JSON value, not a stringified version. Renderers
    /// that need a string preview (chat.rs, server's HistoryEntry::ToolUse)
    /// can call `serde_json::to_string` themselves; the wire-format ToolUse
    /// in server mode passes the Value through directly.
    ToolFinalized {
        tool_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    /// Tool result delta.
    ToolResultDelta { tool_id: String, delta: String },
    /// Tool result complete.
    ToolResult { tool_id: String, result: String },
    /// Subagent dispatched.
    SubagentStart { id: u64, name: String, task: String },
    /// Subagent status update.
    SubagentUpdate { id: u64, status: String },
    /// Subagent finished.
    SubagentDone {
        id: u64,
        status: String,
        duration_secs: f64,
    },
    /// Steering message was delivered.
    SteeringDelivered { message: String },
    /// Usage stats for this turn.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        /// Cache-write TTL split — `None` when the API omitted the breakdown.
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: Option<String>,
    },
    /// Internal bookkeeping — no visual output.
    Noop,
    /// Display-only status notice (e.g. API retry). Not part of the transcript.
    Notice(String),
    /// Stream completed.
    Done,
    /// Stream errored.
    Error(String),
}

/// Subagent tracking state.
#[derive(Debug, Clone)]
pub struct SubagentTracker {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub start_time: std::time::Instant,
    pub done: bool,
    pub duration_secs: Option<f64>,
}

/// What the caller should do after stream completion.
#[derive(Debug)]
pub enum StreamCompletion {
    /// Stream is still going.
    Continue,
    /// Stream done — auto-send this queued message.
    AutoSendQueued(String),
    /// Stream done — pending events need a new model turn.
    AutoTriggerEvents,
    /// Stream done — nothing special.
    Done,
    /// Stream errored — carries the typed spec §5.2 terminal outcome
    /// (variant + correlation ID) so frontends never re-derive it.
    Error(agent_core::TurnError),
}

/// Repair conversation history after a turn failure.
///
/// Only messages appended by the ACTIVE turn (index >= `turn_baseline`) may
/// be removed, and only while the trailing message is invalid history — an
/// assistant message with unmatched `tool_use` blocks or empty content.
/// Valid partial output (assistant text, completed `tool_result` messages)
/// and everything pre-existing (including a trailing user prompt) survive.
pub fn repair_history_after_failure(
    messages: &mut Vec<crate::SharedMessage>,
    turn_baseline: usize,
) {
    fn trailing_is_valid(msg: &serde_json::Value) -> bool {
        match msg["role"].as_str() {
            // User messages (text prompts or tool_result blocks) are always
            // a valid trailing state.
            Some("user") => true,
            Some("assistant") => match &msg["content"] {
                serde_json::Value::String(text) => !text.is_empty(),
                serde_json::Value::Array(blocks) => {
                    !blocks.is_empty()
                        && !blocks
                            .iter()
                            .any(|b| b["type"].as_str() == Some("tool_use"))
                }
                _ => false,
            },
            _ => false,
        }
    }

    while messages.len() > turn_baseline {
        match messages.last() {
            Some(last) if trailing_is_valid(last) => break,
            _ => {
                messages.pop();
            }
        }
    }
}

/// Convert a raw StreamEvent into an EngineStreamEvent.
/// Also handles message history capture and returns completion signals.
///
/// `messages` — the conversation history (updated in place on MessageHistory)
/// `subagents` — tracked subagent states (updated in place)
/// `queued_message` — message queued during streaming (taken if stream completes)
/// `pending_events` — events buffered during streaming (drained on completion)
/// `turn_baseline` — `messages.len()` when this turn started; failure repair
///   may only remove messages appended at or after this index
pub fn process_stream_event(
    event: StreamEvent,
    messages: &mut Vec<crate::SharedMessage>,
    subagents: &mut Vec<SubagentTracker>,
    queued_message: &mut Option<String>,
    pending_events: &mut Vec<String>,
    turn_baseline: usize,
) -> (EngineStreamEvent, StreamCompletion) {
    process_stream_event_with_terminal_capture(
        event,
        messages,
        subagents,
        queued_message,
        pending_events,
        turn_baseline,
        || {},
    )
}

/// Process one stream event and invoke `terminal_capture` only after a typed,
/// successful terminal turn. The hook is deliberately infallible: capture
/// enqueue/build/provider failures cannot replace the completed outcome.
pub fn process_stream_event_with_terminal_capture(
    event: StreamEvent,
    messages: &mut Vec<crate::SharedMessage>,
    subagents: &mut Vec<SubagentTracker>,
    queued_message: &mut Option<String>,
    pending_events: &mut Vec<String>,
    turn_baseline: usize,
    terminal_capture: impl FnOnce(),
) -> (EngineStreamEvent, StreamCompletion) {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(text)) => (
            EngineStreamEvent::Thinking(text),
            StreamCompletion::Continue,
        ),
        StreamEvent::Llm(LlmEvent::Text(text)) => {
            (EngineStreamEvent::Text(text), StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => (
            EngineStreamEvent::ToolStart { tool_id, tool_name },
            StreamCompletion::Continue,
        ),
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => (
            EngineStreamEvent::ToolDelta { tool_id, delta },
            StreamCompletion::Continue,
        ),
        StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name,
            tool_id,
            input,
        }) => (
            EngineStreamEvent::ToolFinalized {
                tool_id,
                tool_name,
                input,
            },
            StreamCompletion::Continue,
        ),
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => (
            EngineStreamEvent::ToolResultDelta { tool_id, delta },
            StreamCompletion::Continue,
        ),
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => (
            EngineStreamEvent::ToolResult { tool_id, result },
            StreamCompletion::Continue,
        ),
        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
            *messages = history;
            (EngineStreamEvent::Noop, StreamCompletion::Continue)
        }
        StreamEvent::Agent(AgentEvent::SubagentStart {
            subagent_id,
            agent_name,
            task_preview,
        }) => {
            subagents.push(SubagentTracker {
                id: subagent_id,
                name: agent_name.clone(),
                status: format!("starting: {}", task_preview),
                start_time: std::time::Instant::now(),
                done: false,
                duration_secs: None,
            });
            (
                EngineStreamEvent::SubagentStart {
                    id: subagent_id,
                    name: agent_name,
                    task: task_preview,
                },
                StreamCompletion::Continue,
            )
        }
        StreamEvent::Agent(AgentEvent::SubagentUpdate {
            subagent_id,
            status,
            ..
        }) => {
            if let Some(sa) = subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.status = status.clone();
            }
            (
                EngineStreamEvent::SubagentUpdate {
                    id: subagent_id,
                    status,
                },
                StreamCompletion::Continue,
            )
        }
        StreamEvent::Agent(AgentEvent::SubagentDone {
            subagent_id,
            result_preview,
            duration_secs,
            ..
        }) => {
            let status = if result_preview.starts_with("[TIMED OUT") {
                "\u{26a0} timed out".to_string()
            } else if result_preview.starts_with("ERROR") {
                let preview: String = result_preview.chars().take(40).collect();
                format!("\u{2718} {}", preview)
            } else {
                let preview: String = result_preview.chars().take(40).collect();
                format!("\u{2714} {}", preview)
            };
            if let Some(sa) = subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.done = true;
                sa.duration_secs = Some(duration_secs);
                sa.status = status.clone();
            }
            (
                EngineStreamEvent::SubagentDone {
                    id: subagent_id,
                    status,
                    duration_secs,
                },
                StreamCompletion::Continue,
            )
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            if queued_message.as_ref() == Some(&message) {
                *queued_message = None;
            }
            (
                EngineStreamEvent::SteeringDelivered { message },
                StreamCompletion::Continue,
            )
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model,
        }) => (
            EngineStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read: cache_read_input_tokens,
                cache_creation: cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model,
            },
            StreamCompletion::Continue,
        ),
        StreamEvent::Session(SessionEvent::Done) => {
            // The canonical history has reached its typed successful terminal
            // state. Capture is best-effort and cannot alter completion.
            terminal_capture();
            subagents.clear();

            // Drain pending events into messages
            let had_pending = !pending_events.is_empty();
            for formatted in pending_events.drain(..) {
                messages.push(std::sync::Arc::new(serde_json::json!({
                    "role": "user",
                    "content": formatted
                })));
            }

            // Check for queued message
            if let Some(queued) = queued_message.take() {
                (
                    EngineStreamEvent::Done,
                    StreamCompletion::AutoSendQueued(queued),
                )
            } else if had_pending {
                (EngineStreamEvent::Done, StreamCompletion::AutoTriggerEvents)
            } else {
                (EngineStreamEvent::Done, StreamCompletion::Done)
            }
        }
        StreamEvent::Session(SessionEvent::Notice(text)) => {
            // Display-only status (e.g. retry notice) — surface as a system
            // line, never recorded into message history.
            (EngineStreamEvent::Notice(text), StreamCompletion::Continue)
        }
        StreamEvent::Session(SessionEvent::Error(err)) => {
            subagents.clear();
            // Repair history: remove only invalid messages appended by the
            // ACTIVE turn — never a pre-existing trailing message, never
            // valid partial output (spec §5.2).
            repair_history_after_failure(messages, turn_baseline);
            (
                EngineStreamEvent::Error(err.message.clone()),
                StreamCompletion::Error(err),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn msg(v: serde_json::Value) -> crate::SharedMessage {
        Arc::new(v)
    }

    fn run_error_event(
        messages: &mut Vec<crate::SharedMessage>,
        turn_baseline: usize,
    ) -> (EngineStreamEvent, StreamCompletion) {
        let mut subagents = Vec::new();
        let mut queued = None;
        let mut pending = Vec::new();
        process_stream_event(
            StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::provider(
                "provider exploded",
                "api_status",
                "turn-test-0",
            ))),
            messages,
            &mut subagents,
            &mut queued,
            &mut pending,
            turn_baseline,
        )
    }

    #[test]
    fn memory_terminal_capture_failure_cannot_change_completed_engine_turn() {
        let mut messages = vec![msg(json!({"role": "user", "content": "hello"}))];
        let mut subagents = Vec::new();
        let mut queued = None;
        let mut pending = Vec::new();
        let (event, completion) = process_stream_event_with_terminal_capture(
            StreamEvent::Session(SessionEvent::Done),
            &mut messages,
            &mut subagents,
            &mut queued,
            &mut pending,
            1,
            || {
                // The capture boundary absorbs build/enqueue/provider failure.
                let _: Result<(), &'static str> = Err("capture_failed");
            },
        );
        assert!(matches!(event, EngineStreamEvent::Done));
        assert!(matches!(completion, StreamCompletion::Done));
    }

    /// T3 criterion 4: a pre-existing trailing user message (the prompt that
    /// started the turn) must never be removed by failure repair.
    #[test]
    fn error_repair_keeps_preexisting_trailing_user_message() {
        let mut messages = vec![msg(json!({"role": "user", "content": "hello"}))];
        let (_, completion) = run_error_event(&mut messages, 1);
        assert!(matches!(completion, StreamCompletion::Error(_)));
        assert_eq!(
            messages.len(),
            1,
            "pre-existing trailing user message must survive turn failure"
        );
        assert_eq!(messages[0]["content"], "hello");
    }

    /// T3 criterion 2: partial assistant output and completed tool results
    /// appended by the active turn must survive an unrecovered failure.
    #[test]
    fn error_repair_keeps_partial_assistant_output_and_tool_results() {
        let mut messages = vec![
            msg(json!({"role": "user", "content": "do the thing"})),
            msg(json!({"role": "assistant", "content": [
                {"type": "text", "text": "working on it"},
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {}},
            ]})),
            msg(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "ok"},
            ]})),
            msg(json!({"role": "assistant", "content": [
                {"type": "text", "text": "partial answer before the failure"},
            ]})),
        ];
        let (_, completion) = run_error_event(&mut messages, 1);
        assert!(matches!(completion, StreamCompletion::Error(_)));
        assert_eq!(
            messages.len(),
            4,
            "valid partial assistant output and completed tool results must survive"
        );
        assert_eq!(
            messages[3]["content"][0]["text"],
            "partial answer before the failure"
        );
    }

    /// A turn-appended trailing assistant message with unmatched tool_use
    /// blocks is invalid history and must be removed by repair.
    #[test]
    fn error_repair_drops_turn_appended_unmatched_tool_use() {
        let mut messages = vec![
            msg(json!({"role": "user", "content": "do the thing"})),
            msg(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {}},
            ]})),
        ];
        let (_, completion) = run_error_event(&mut messages, 1);
        assert!(matches!(completion, StreamCompletion::Error(_)));
        assert_eq!(
            messages.len(),
            1,
            "unmatched trailing tool_use appended by the turn must be removed"
        );
        assert_eq!(messages[0]["content"], "do the thing");
    }
}
