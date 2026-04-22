//! Anthropic ↔ OpenAI translation layer.

use super::types::{ChatMessage, FunctionCall, FunctionDefinition, OaiEvent, ToolCall, ToolDefinition};
use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};
use serde_json::{json, Value};

/// Convert Anthropic tool schema entries to OpenAI ToolDefinitions.
///
/// Anthropic shape: `{"name", "description", "input_schema", optional cache_control}`.
/// OpenAI shape:    `{"type": "function", "function": {"name", "description", "parameters"}}`.
pub fn tools_to_oai(schema: &[Value]) -> Vec<ToolDefinition> {
    schema
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            // Skip empty names and internal-only tools
            if name.is_empty() || name == "respond" || name == "send_channel" || name == "watcher_exit" {
                return None;
            }
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let parameters = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(ToolDefinition {
                kind: "function".to_string(),
                function: FunctionDefinition {
                    name,
                    description,
                    parameters,
                },
            })
        })
        .collect()
}

/// Convert Anthropic-shaped message list + optional system prompt into
/// an OpenAI ChatMessage stream.
pub fn messages_to_oai(
    anthropic_messages: &[Value],
    system_prompt: &Option<String>,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();

    if let Some(sp) = system_prompt.as_ref() {
        if !sp.is_empty() {
            out.push(ChatMessage::system(sp.clone()));
        }
    }

    for msg in anthropic_messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        match role {
            "user" => {
                match content {
                    Some(Value::String(s)) => out.push(ChatMessage::user(s.clone())),
                    Some(Value::Array(blocks)) => {
                        let mut text_buf = String::new();
                        for block in blocks {
                            let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match btype {
                                "text" => {
                                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                        text_buf.push_str(t);
                                    }
                                }
                                "tool_result" => {
                                    // Flush pending text first
                                    if !text_buf.is_empty() {
                                        out.push(ChatMessage::user(std::mem::take(&mut text_buf)));
                                    }
                                    let tool_id = block
                                        .get("tool_use_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let result_content = match block.get("content") {
                                        Some(Value::String(s)) => s.clone(),
                                        Some(Value::Array(arr)) => arr
                                            .iter()
                                            .filter_map(|b| {
                                                b.get("text").and_then(|t| t.as_str()).map(String::from)
                                            })
                                            .collect::<Vec<_>>()
                                            .join(""),
                                        Some(other) => other.to_string(),
                                        None => String::new(),
                                    };
                                    out.push(ChatMessage::tool_result(tool_id, "", result_content));
                                }
                                _ => {}
                            }
                        }
                        if !text_buf.is_empty() {
                            out.push(ChatMessage::user(text_buf));
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let mut text_buf = String::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();

                if let Some(Value::Array(blocks)) = content {
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_use" => {
                                let id = block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let arguments = block
                                    .get("input")
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "{}".to_string());
                                tool_calls.push(ToolCall {
                                    id,
                                    kind: "function".to_string(),
                                    function: FunctionCall { name, arguments },
                                });
                            }
                            "thinking" => {
                                // Not representable in OpenAI — drop.
                            }
                            _ => {}
                        }
                    }
                } else if let Some(Value::String(s)) = content {
                    text_buf.push_str(s);
                }

                let has_text = !text_buf.is_empty();
                let has_tools = !tool_calls.is_empty();
                match (has_text, has_tools) {
                    (true, false) => out.push(ChatMessage::assistant(text_buf)),
                    (false, true) => out.push(ChatMessage::assistant_tool_calls(tool_calls)),
                    (true, true) => out.push(ChatMessage {
                        role: "assistant".into(),
                        content: Some(text_buf),
                        tool_calls: Some(tool_calls),
                        tool_call_id: None,
                        name: None,
                    }),
                    (false, false) => {}
                }
            }
            _ => {}
        }
    }

    // Fixup: OpenAI requires that a 'tool' message is never followed
    // directly by a 'user' message. Insert an empty assistant message if needed.
    let mut fixed = Vec::with_capacity(out.len());
    for msg in out {
        if msg.role == "user" && fixed.last().map(|m: &ChatMessage| m.role == "tool").unwrap_or(false) {
            fixed.push(ChatMessage::assistant("".to_string()));
        }
        fixed.push(msg);
    }
    fixed
}

/// Translate an [`OaiEvent`] into a synaps [`StreamEvent`].
///
/// Returns `None` for events that are handled at a higher level
/// (`Done`, `ToolCallsComplete`, `RoleStart`) or purely informational
/// (`Warning` — logged via tracing).
pub fn oai_event_to_llm(event: &OaiEvent) -> Option<StreamEvent> {
    match event {
        OaiEvent::TextDelta(t) => Some(StreamEvent::Llm(LlmEvent::Text(t.clone()))),
        OaiEvent::ToolCallStart { name, .. } => {
            Some(StreamEvent::Llm(LlmEvent::ToolUseStart(name.clone())))
        }
        OaiEvent::ToolCallArgumentsDelta { delta, .. } => {
            Some(StreamEvent::Llm(LlmEvent::ToolUseDelta(delta.clone())))
        }
        OaiEvent::Usage { prompt_tokens, completion_tokens } => {
            Some(StreamEvent::Session(SessionEvent::Usage {
                input_tokens: *prompt_tokens as u64,
                output_tokens: *completion_tokens as u64,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                model: None,
            }))
        }
        OaiEvent::Warning(s) => {
            tracing::warn!("openai stream warning: {}", s);
            None
        }
        OaiEvent::RoleStart(_) | OaiEvent::Done | OaiEvent::ToolCallsComplete { .. } => None,
    }
}

/// Convert an OpenAI tool-call list into Anthropic-shaped `tool_use` content blocks.
pub fn tool_calls_to_content_blocks(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            let input: Value = serde_json::from_str(&c.function.arguments).unwrap_or_else(|_| json!({}));
            json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.function.name,
                "input": input,
            })
        })
        .collect()
}
