//! Engine-level stream processing — TUI-agnostic event handling.
//!
//! Processes StreamEvent variants, tracks subagent state and usage,
//! and returns renderer-agnostic actions.

use crate::{StreamEvent, LlmEvent, SessionEvent, AgentEvent};
use serde_json::Value;

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
    ToolFinalized { tool_id: String, tool_name: String, input: serde_json::Value },
    /// Tool result delta.
    ToolResultDelta { tool_id: String, delta: String },
    /// Tool result complete.
    ToolResult { tool_id: String, result: String },
    /// Subagent dispatched.
    SubagentStart { id: u64, name: String, task: String },
    /// Subagent status update.
    SubagentUpdate { id: u64, status: String },
    /// Subagent finished.
    SubagentDone { id: u64, status: String, duration_secs: f64 },
    /// Steering message was delivered.
    SteeringDelivered { message: String },
    /// Usage stats for this turn.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
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
    /// Stream errored.
    Error(String),
}

/// Convert a raw StreamEvent into an EngineStreamEvent.
/// Also handles message history capture and returns completion signals.
///
/// `messages` — the conversation history (updated in place on MessageHistory)
/// `subagents` — tracked subagent states (updated in place)
/// `queued_message` — message queued during streaming (taken if stream completes)
/// `pending_events` — events buffered during streaming (drained on completion)
pub fn process_stream_event(
    event: StreamEvent,
    messages: &mut Vec<Value>,
    subagents: &mut Vec<SubagentTracker>,
    queued_message: &mut Option<String>,
    pending_events: &mut Vec<String>,
) -> (EngineStreamEvent, StreamCompletion) {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(text)) => {
            (EngineStreamEvent::Thinking(text), StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::Text(text)) => {
            (EngineStreamEvent::Text(text), StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
            (EngineStreamEvent::ToolStart { tool_id, tool_name }, StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
            (EngineStreamEvent::ToolDelta { tool_id, delta }, StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolUse { tool_name, tool_id, input }) => {
            (EngineStreamEvent::ToolFinalized { tool_id, tool_name, input }, StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => {
            (EngineStreamEvent::ToolResultDelta { tool_id, delta }, StreamCompletion::Continue)
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => {
            (EngineStreamEvent::ToolResult { tool_id, result }, StreamCompletion::Continue)
        }
        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
            *messages = history;
            (EngineStreamEvent::Noop, StreamCompletion::Continue)
        }
        StreamEvent::Agent(AgentEvent::SubagentStart { subagent_id, agent_name, task_preview }) => {
            subagents.push(SubagentTracker {
                id: subagent_id,
                name: agent_name.clone(),
                status: format!("starting: {}", task_preview),
                start_time: std::time::Instant::now(),
                done: false,
                duration_secs: None,
            });
            (EngineStreamEvent::SubagentStart { id: subagent_id, name: agent_name, task: task_preview }, StreamCompletion::Continue)
        }
        StreamEvent::Agent(AgentEvent::SubagentUpdate { subagent_id, status, .. }) => {
            if let Some(sa) = subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.status = status.clone();
            }
            (EngineStreamEvent::SubagentUpdate { id: subagent_id, status }, StreamCompletion::Continue)
        }
        StreamEvent::Agent(AgentEvent::SubagentDone { subagent_id, result_preview, duration_secs, .. }) => {
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
            (EngineStreamEvent::SubagentDone { id: subagent_id, status, duration_secs }, StreamCompletion::Continue)
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            if queued_message.as_ref() == Some(&message) {
                *queued_message = None;
            }
            (EngineStreamEvent::SteeringDelivered { message }, StreamCompletion::Continue)
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            model,
        }) => {
            (EngineStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read: cache_read_input_tokens,
                cache_creation: cache_creation_input_tokens,
                model,
            }, StreamCompletion::Continue)
        }
        StreamEvent::Session(SessionEvent::Done) => {
            subagents.clear();

            // Drain pending events into messages
            let had_pending = !pending_events.is_empty();
            for formatted in pending_events.drain(..) {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": formatted
                }));
            }

            // Check for queued message
            if let Some(queued) = queued_message.take() {
                (EngineStreamEvent::Done, StreamCompletion::AutoSendQueued(queued))
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
            // Drop trailing unmatched messages
            if let Some(last) = messages.last() {
                let role = last["role"].as_str().unwrap_or("");
                let is_text_user = role == "user" && last["content"].is_string();
                let is_assistant = role == "assistant";
                if is_text_user || is_assistant {
                    messages.pop();
                }
            }
            (EngineStreamEvent::Error(err.clone()), StreamCompletion::Error(err))
        }
    }
}
