//! Anthropic ↔ OpenAI translation layer.

use super::types::{
    ChatMessage, FunctionCall, FunctionDefinition, OaiEvent, ToolCall, ToolDefinition,
};
use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct ToolNameMap {
    original_to_oai: HashMap<String, String>,
    oai_to_original: HashMap<String, String>,
}

impl ToolNameMap {
    pub fn to_oai<'a>(&'a self, name: &'a str) -> &'a str {
        self.original_to_oai
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }

    pub fn to_original<'a>(&'a self, name: &'a str) -> &'a str {
        self.oai_to_original
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }

    fn insert(&mut self, original: &str, oai: &str) {
        if original != oai {
            self.original_to_oai
                .insert(original.to_string(), oai.to_string());
            self.oai_to_original
                .insert(oai.to_string(), original.to_string());
        }
    }
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]+$`. Synaps/MCP names may
/// contain namespace separators like `:` or `.`, so sanitize only for the
/// OpenAI wire format and map back before tool execution.
fn sanitize_oai_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "tool".to_string()
    } else {
        if out.len() > 128 {
            out.truncate(128);
        }
        out
    }
}

/// OpenAI's function-schema validator rejects any array schema without
/// `items` ("array schema missing items"), which turns one loosely-specified
/// MCP/extension tool into a hard 400 on *every* request of the session
/// (observed live: `ext__lsrf-manager__managerApi_cloudfrontInvalidate`,
/// session 20260429-235559-094c). Backfill `"items": {}` — the
/// accept-anything schema — recursively, preserving all author intent.
fn sanitize_oai_parameters(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    let is_array_type = match obj.get("type") {
        Some(Value::String(t)) => t == "array",
        Some(Value::Array(ts)) => ts.iter().any(|t| t.as_str() == Some("array")),
        _ => false,
    };
    if is_array_type && !obj.contains_key("items") {
        obj.insert("items".into(), json!({}));
    }

    for key in [
        "items",
        "additionalProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(sub) = obj.get_mut(key) {
            sanitize_oai_parameters(sub);
        }
    }
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(Value::Object(map)) = obj.get_mut(key) {
            for sub in map.values_mut() {
                sanitize_oai_parameters(sub);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list)) = obj.get_mut(key) {
            for sub in list.iter_mut() {
                sanitize_oai_parameters(sub);
            }
        }
    }
}

/// Convert Anthropic tool schema entries to OpenAI ToolDefinitions.
///
/// Anthropic shape: `{"name", "description", "input_schema", optional cache_control}`.
/// OpenAI shape:    `{"type": "function", "function": {"name", "description", "parameters"}}`.
pub fn tools_to_oai(schema: &[Value]) -> (Vec<ToolDefinition>, ToolNameMap) {
    let mut name_map = ToolNameMap::default();
    let mut used_names: HashMap<String, String> = HashMap::new();

    let tools = schema
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            // Skip empty names and internal-only tools
            if name.is_empty()
                || name == "respond"
                || name == "send_channel"
                || name == "watcher_exit"
            {
                return None;
            }
            let mut oai_name = sanitize_oai_tool_name(&name);
            if let Some(existing) = used_names.get(&oai_name) {
                if existing != &name {
                    // Truncate base to leave room for suffix (e.g. "_99" = 3 chars)
                    let max_base = 128_usize.saturating_sub(4);
                    let base = if oai_name.len() > max_base {
                        oai_name[..max_base].to_string()
                    } else {
                        oai_name.clone()
                    };
                    let mut suffix = 2;
                    while used_names.contains_key(&oai_name) {
                        oai_name = format!("{base}_{suffix}");
                        suffix += 1;
                    }
                }
            }
            used_names.insert(oai_name.clone(), name.clone());
            name_map.insert(&name, &oai_name);

            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let parameters = t
                .get("input_schema")
                .cloned()
                .map(|mut schema| {
                    sanitize_oai_parameters(&mut schema);
                    schema
                })
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(ToolDefinition {
                kind: "function".to_string(),
                function: FunctionDefinition {
                    name: oai_name,
                    description,
                    parameters,
                },
            })
        })
        .collect();

    (tools, name_map)
}

/// Convert Anthropic-shaped message list + optional system prompt into
/// an OpenAI ChatMessage stream.
pub fn messages_to_oai(
    anthropic_messages: &[crate::SharedMessage],
    system_prompt: &Option<String>,
    name_map: &ToolNameMap,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();

    if let Some(sp) = system_prompt.as_ref() {
        if !sp.is_empty() {
            out.push(ChatMessage::system(sp.clone()));
        }
    }

    // Build a map of tool_use_id → tool_name from assistant messages
    // so we can populate the name field on tool result messages.
    let mut tool_name_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in anthropic_messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let (Some(id), Some(name)) = (
                            block.get("id").and_then(|v| v.as_str()),
                            block.get("name").and_then(|v| v.as_str()),
                        ) {
                            tool_name_map.insert(id.to_string(), name_map.to_oai(name).to_string());
                        }
                    }
                }
            }
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
                                                b.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(String::from)
                                            })
                                            .collect::<Vec<_>>()
                                            .join(""),
                                        Some(other) => other.to_string(),
                                        None => String::new(),
                                    };
                                    let tool_name =
                                        tool_name_map.get(&tool_id).cloned().unwrap_or_default();
                                    out.push(ChatMessage::tool_result(
                                        tool_id,
                                        tool_name,
                                        result_content,
                                    ));
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
                                    .map(|n| name_map.to_oai(n).to_string())
                                    .unwrap_or_default();
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
        if msg.role == "user"
            && fixed
                .last()
                .map(|m: &ChatMessage| m.role == "tool")
                .unwrap_or(false)
        {
            fixed.push(ChatMessage::assistant(" ".to_string()));
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
        OaiEvent::ToolCallStart { name, id, .. } => {
            Some(StreamEvent::Llm(LlmEvent::ToolUseStart {
                tool_name: name.clone(),
                tool_id: id.clone(),
            }))
        }
        OaiEvent::ToolCallArgumentsDelta { delta, id, .. } => {
            Some(StreamEvent::Llm(LlmEvent::ToolUseDelta {
                tool_id: id.clone(),
                delta: delta.clone(),
            }))
        }
        OaiEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        } => Some(StreamEvent::Session(SessionEvent::Usage {
            input_tokens: *prompt_tokens as u64,
            output_tokens: *completion_tokens as u64,
            cache_read_input_tokens: *cached_tokens as u64,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: None,
        })),
        OaiEvent::Warning(s) => {
            tracing::warn!("openai stream warning: {}", s);
            None
        }
        OaiEvent::RoleStart(_) | OaiEvent::Done | OaiEvent::ToolCallsComplete { .. } => None,
    }
}

/// Convert an OpenAI tool-call list into Anthropic-shaped `tool_use` content blocks.
pub fn tool_calls_to_content_blocks(calls: &[ToolCall], name_map: &ToolNameMap) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            let input: Value =
                serde_json::from_str(&c.function.arguments).unwrap_or_else(|_| json!({}));
            json!({
                "type": "tool_use",
                "id": c.id,
                "name": name_map.to_original(&c.function.name),
                "input": input,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression fixture from session 20260429-235559-094c: the lsrf-manager
    /// extension exposed `managerApi_cloudfrontInvalidate` whose `paths`
    /// property was `{"type":"array"}` with no `items`. The Codex Responses
    /// API rejects every request in such a session:
    ///   400 "Invalid schema for function 'ext__lsrf-manager__managerApi_cloudfrontInvalidate':
    ///        In context=('properties', 'paths'), array schema missing items."
    #[test]
    fn array_property_missing_items_is_backfilled_for_oai_wire() {
        let schema = vec![json!({
            "name": "ext__lsrf-manager__managerApi_cloudfrontInvalidate",
            "description": "Invalidate CloudFront paths",
            "input_schema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array" },
                    "distribution_id": { "type": "string" }
                }
            }
        })];
        let (tools, _map) = tools_to_oai(&schema);
        let params = &tools[0].function.parameters;
        assert_eq!(
            params["properties"]["paths"]["items"],
            json!({}),
            "array schema without items must be backfilled: {params}"
        );
        // Non-array sibling untouched.
        assert_eq!(
            params["properties"]["distribution_id"],
            json!({"type": "string"})
        );
    }

    #[test]
    fn nested_and_composed_array_schemas_are_sanitized() {
        let schema = vec![json!({
            "name": "t",
            "input_schema": {
                "type": "object",
                "properties": {
                    "matrix": {
                        "type": "array",
                        "items": { "type": "array" }
                    },
                    "either": {
                        "anyOf": [
                            { "type": "array" },
                            { "type": "string" }
                        ]
                    },
                    "nullable": { "type": ["array", "null"] },
                    "obj": {
                        "type": "object",
                        "additionalProperties": { "type": "array" },
                        "properties": {
                            "inner": { "type": "array" }
                        }
                    }
                },
                "$defs": {
                    "aliasList": { "type": "array" }
                }
            }
        })];
        let (tools, _map) = tools_to_oai(&schema);
        let p = &tools[0].function.parameters;
        assert_eq!(p["properties"]["matrix"]["items"]["items"], json!({}));
        assert_eq!(p["properties"]["either"]["anyOf"][0]["items"], json!({}));
        assert!(p["properties"]["either"]["anyOf"][1].get("items").is_none());
        assert_eq!(p["properties"]["nullable"]["items"], json!({}));
        assert_eq!(
            p["properties"]["obj"]["additionalProperties"]["items"],
            json!({})
        );
        assert_eq!(
            p["properties"]["obj"]["properties"]["inner"]["items"],
            json!({})
        );
        assert_eq!(p["$defs"]["aliasList"]["items"], json!({}));
    }

    #[test]
    fn array_schema_with_existing_items_is_untouched() {
        let schema = vec![json!({
            "name": "t",
            "input_schema": {
                "type": "object",
                "properties": {
                    "ops": { "type": "array", "items": { "type": "object" } }
                }
            }
        })];
        let (tools, _map) = tools_to_oai(&schema);
        assert_eq!(
            tools[0].function.parameters["properties"]["ops"]["items"],
            json!({"type": "object"})
        );
    }

    /// Regression for session 20260427-235907-2185:
    ///   400 "Invalid 'tools[18].name': string does not match pattern.
    ///        Expected a string that matches the pattern '^[a-zA-Z0-9_-]+$'."
    /// MCP/extension names carry `.`/`:` separators; the OAI wire name must
    /// be sanitized and must round-trip back to the original for execution.
    #[test]
    fn mcp_tool_names_are_sanitized_for_oai_wire_and_round_trip() {
        let schema = vec![json!({
            "name": "ext__lsrf-manager__managerApi.cloudfrontInvalidate",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let (tools, map) = tools_to_oai(&schema);
        let wire = &tools[0].function.name;
        assert!(
            wire.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "wire name must match ^[a-zA-Z0-9_-]+$: {wire}"
        );
        assert_eq!(
            map.to_original(wire),
            "ext__lsrf-manager__managerApi.cloudfrontInvalidate",
            "sanitized wire name must map back to the executable tool name"
        );
    }
}
