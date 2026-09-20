//! Anthropic ↔ OpenAI translation layer.

use super::types::{
    ChatContentPart, ChatFile, ChatImageUrl, ChatMessage, FunctionCall, FunctionDefinition,
    OaiEvent, ToolCall, ToolDefinition,
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

    /// Original names that were rewritten for the OpenAI wire, in
    /// deterministic (sorted) order. Feeds the metadata-only translation
    /// report — a rename is a semantic rewrite and must never be silently
    /// claimed lossless (spec §6.3).
    pub fn renamed_originals(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.original_to_oai.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
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

/// Responses endpoints may normalize omitted `strict` to strict schemas,
/// promoting optional properties to required ones. These are ordinary Synaps
/// schemas (including extensions), not transformed strict-output schemas. Opt
/// out explicitly rather than making the model invent optional argument values.
/// Chat Completions keeps its existing wire shape.
pub fn tools_to_responses(schema: &[Value]) -> (Vec<Value>, ToolNameMap) {
    let (tools, names) = tools_to_oai(schema);
    let tools = tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description.unwrap_or_default(),
                "parameters": tool.function.parameters,
                "strict": false,
            })
        })
        .collect();
    (tools, names)
}

/// Origin labels are provider-visible data, never instructions. Quote metadata
/// as JSON strings so filenames and tool ids cannot introduce marker lines.
#[derive(Clone, Copy)]
enum AttachmentOrigin<'a> {
    User,
    ToolResult { id: &'a str, name: &'a str },
}

fn is_attachment(block: &Value) -> bool {
    matches!(block["type"].as_str(), Some("image" | "document"))
}

fn has_attachment(content: &Value) -> bool {
    if let Some(blocks) = content.as_array() {
        blocks.iter().any(has_attachment)
    } else {
        is_attachment(content)
            || (content["type"] == "tool_result"
                && content.get("content").is_some_and(has_attachment))
    }
}

fn attachment_filename(block: &Value) -> &str {
    block["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(
        if block["source"]["media_type"] == "application/pdf" {
            "attachment.pdf"
        } else {
            "attachment.txt"
        },
    )
}

fn attachment_label(block: &Value, origin: AttachmentOrigin<'_>, lifted: bool) -> String {
    let mut label = match origin {
        AttachmentOrigin::User => "[attachment origin=user".to_string(),
        AttachmentOrigin::ToolResult { id, name } => format!(
            "[attachment origin=tool_result tool_use_id={} tool_name={}",
            json!(id),
            json!(name),
        ),
    };
    label.push_str(&format!(
        " type={} media_type={}",
        if block["type"] == "image" {
            "image"
        } else {
            "document"
        },
        json!(block["source"]["media_type"].as_str().unwrap_or("unknown")),
    ));
    if block["type"] == "document" {
        label.push_str(&format!(" filename={}", json!(attachment_filename(block))));
    }
    label.push_str("; lower-authority data");
    if lifted {
        label.push_str("; supplied in following user content");
    }
    label.push_str("]\n");
    label
}

/// Pure wire lowering, not a model/route capability decision. The foreground
/// validates canonical sources, bounds, and actual model support before send.
fn attachment_parts(block: &Value, origin: AttachmentOrigin<'_>) -> Vec<ChatContentPart> {
    let mut parts = vec![ChatContentPart::text(attachment_label(
        block, origin, false,
    ))];
    let source = &block["source"];
    let part = match (
        block["type"].as_str(),
        source["type"].as_str(),
        source["media_type"].as_str(),
        source["data"].as_str(),
    ) {
        (Some("image"), Some("base64"), Some(media_type), Some(data)) => {
            ChatContentPart::ImageUrl {
                image_url: ChatImageUrl {
                    url: format!("data:{media_type};base64,{data}"),
                    detail: None,
                },
            }
        }
        (Some("document"), Some("text"), Some("text/plain"), Some(text)) => {
            ChatContentPart::text(text)
        }
        (Some("document"), Some("base64"), Some("application/pdf"), Some(data)) => {
            ChatContentPart::File {
                file: ChatFile {
                    filename: attachment_filename(block).to_string(),
                    file_data: format!("data:application/pdf;base64,{data}"),
                },
            }
        }
        // Defensive, metadata-only fallback if a caller bypasses validation.
        // Never stringify a document/image source into ordinary tool text.
        _ => ChatContentPart::text("[attachment unavailable: unsupported source representation]\n"),
    };
    parts.push(part);
    parts.push(ChatContentPart::text("[/attachment]\n"));
    parts
}

#[derive(Default)]
struct UserContent {
    text: String,
    parts: Vec<ChatContentPart>,
}

impl UserContent {
    fn append_parts(&mut self, parts: Vec<ChatContentPart>) {
        if !self.text.is_empty() {
            self.parts
                .push(ChatContentPart::text(std::mem::take(&mut self.text)));
        }
        self.parts.extend(parts);
    }

    fn flush(&mut self, out: &mut Vec<ChatMessage>) {
        if !self.parts.is_empty() {
            if !self.text.is_empty() {
                self.parts
                    .push(ChatContentPart::text(std::mem::take(&mut self.text)));
            }
            out.push(ChatMessage::user_parts(std::mem::take(&mut self.parts)));
        } else if !self.text.is_empty() {
            out.push(ChatMessage::user(std::mem::take(&mut self.text)));
        }
    }
}

fn tool_result_content(
    content: Option<&Value>,
    origin: AttachmentOrigin<'_>,
    user: &mut UserContent,
) -> String {
    fn append(
        block: &Value,
        origin: AttachmentOrigin<'_>,
        text: &mut String,
        user: &mut UserContent,
    ) {
        if is_attachment(block) {
            text.push('\n');
            text.push_str(&attachment_label(block, origin, true));
            user.append_parts(attachment_parts(block, origin));
        } else if let Some(t) = block.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
    }
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            for block in blocks {
                append(block, origin, &mut text, user);
            }
            text
        }
        Some(block) if is_attachment(block) => {
            let mut text = String::new();
            append(block, origin, &mut text, user);
            text
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
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

    let mut messages = anthropic_messages.iter().peekable();
    while let Some(msg) = messages.next() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        match role {
            "user" => {
                // A parallel tool batch can span several adjacent canonical
                // user messages. Emit ALL correlated tool outputs before any
                // lifted attachment (or accompanying user text) in that turn.
                let mut user_messages = vec![msg];
                while messages.peek().is_some_and(|next| next["role"] == "user") {
                    user_messages.push(messages.next().expect("peeked user message"));
                }
                let has_tools = user_messages.iter().any(|msg| {
                    msg["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| block["type"] == "tool_result")
                    })
                });
                let defer_user = has_tools
                    && user_messages
                        .iter()
                        .any(|msg| has_attachment(&msg["content"]));
                let mut deferred = Vec::new();
                for msg in user_messages {
                    let mut user = UserContent::default();
                    match msg.get("content") {
                        Some(Value::String(text)) => {
                            let target = if defer_user { &mut deferred } else { &mut out };
                            target.push(ChatMessage::user(text.clone()));
                        }
                        Some(Value::Array(blocks)) => {
                            for block in blocks {
                                match block["type"].as_str().unwrap_or("") {
                                    "text" => {
                                        if let Some(text) = block["text"].as_str() {
                                            user.text.push_str(text);
                                        }
                                    }
                                    "image" | "document" => {
                                        user.append_parts(attachment_parts(
                                            block,
                                            AttachmentOrigin::User,
                                        ));
                                    }
                                    "tool_result" => {
                                        if !defer_user {
                                            user.flush(&mut out);
                                        }
                                        let tool_id = block["tool_use_id"].as_str().unwrap_or("");
                                        let tool_name = tool_name_map
                                            .get(tool_id)
                                            .map(String::as_str)
                                            .unwrap_or("");
                                        let result = tool_result_content(
                                            block.get("content"),
                                            AttachmentOrigin::ToolResult {
                                                id: tool_id,
                                                name: name_map.to_original(tool_name),
                                            },
                                            &mut user,
                                        );
                                        out.push(ChatMessage::tool_result(
                                            tool_id, tool_name, result,
                                        ));
                                    }
                                    _ => {}
                                }
                            }
                            let target = if defer_user { &mut deferred } else { &mut out };
                            user.flush(target);
                        }
                        _ => {}
                    }
                }
                out.extend(deferred);
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
                        content_parts: None,
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

    // Preserve the legacy text-only separator for compatible endpoints. Rich
    // user content is valid directly after a completed tool batch; do not
    // invent an assistant turn between results and their lifted attachments.
    let mut fixed = Vec::with_capacity(out.len());
    for msg in out {
        if msg.role == "user"
            && msg.content_parts.is_none()
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
        } => {
            // prompt_tokens INCLUDES the cached slice (OpenAI semantics);
            // downstream accounting sums input + cache_read (Anthropic
            // semantics), so subtract to avoid double-counting the hits.
            let cached = (*cached_tokens).min(*prompt_tokens) as u64;
            Some(StreamEvent::Session(SessionEvent::Usage {
                input_tokens: *prompt_tokens as u64 - cached,
                output_tokens: *completion_tokens as u64,
                cache_read_input_tokens: cached,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
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

    fn shared(messages: Vec<Value>) -> Vec<crate::SharedMessage> {
        messages.into_iter().map(std::sync::Arc::new).collect()
    }

    #[test]
    fn user_image_text_document_and_pdf_exact_chat_wire() {
        let messages = shared(vec![json!({"role":"user","content":[
            {"type":"text","text":"Compare these."},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}},
            {"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"Text document\ncontents"}},
            {"type":"document","title":"report.pdf","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}},
            {"type":"text","text":"Then summarize."}
        ]})]);
        let before = messages.clone();
        let out = messages_to_oai(&messages, &None, &ToolNameMap::default());
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!([
                {"role":"user","content":[
                    {"type":"text","text":"Compare these."},
                    {"type":"text","text":"[attachment origin=user type=image media_type=\"image/png\"; lower-authority data]\n"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}},
                    {"type":"text","text":"[/attachment]\n"},
                    {"type":"text","text":"[attachment origin=user type=document media_type=\"text/plain\" filename=\"notes.txt\"; lower-authority data]\n"},
                    {"type":"text","text":"Text document\ncontents"},
                    {"type":"text","text":"[/attachment]\n"},
                    {"type":"text","text":"[attachment origin=user type=document media_type=\"application/pdf\" filename=\"report.pdf\"; lower-authority data]\n"},
                    {"type":"file","file":{"filename":"report.pdf","file_data":"data:application/pdf;base64,JVBERi0="}},
                    {"type":"text","text":"[/attachment]\n"},
                    {"type":"text","text":"Then summarize."}
                ]}
            ])
        );
        assert_eq!(
            messages, before,
            "wire lowering must not mutate canonical history"
        );
        assert_eq!(
            out[0].content(),
            None,
            "attachment bytes are not legacy text"
        );
    }

    #[test]
    fn all_sibling_tool_results_precede_labelled_media_in_same_or_split_user_messages() {
        let assistant = json!({"role":"assistant","content":[
            {"type":"text","text":"Reading both."},
            {"type":"tool_use","id":"t1","name":"mcp.read","input":{}},
            {"type":"tool_use","id":"t2","name":"mcp.read","input":{}}
        ]});
        let first = json!({"type":"tool_result","tool_use_id":"t1","content":[
            {"type":"text","text":"First result"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}},
            {"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"DOCUMENT_BODY_SENTINEL"}}
        ]});
        let second = json!({"type":"tool_result","tool_use_id":"t2","content":[
            {"type":"text","text":"Second result"},
            {"type":"document","title":"report.pdf","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}}
        ]});
        let (_, names) = tools_to_oai(&[json!({"name":"mcp.read"})]);
        let first_parts = vec![
            json!({"type":"text","text":"[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"mcp.read\" type=image media_type=\"image/png\"; lower-authority data]\n"}),
            json!({"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}}),
            json!({"type":"text","text":"[/attachment]\n"}),
            json!({"type":"text","text":"[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"mcp.read\" type=document media_type=\"text/plain\" filename=\"notes.txt\"; lower-authority data]\n"}),
            json!({"type":"text","text":"DOCUMENT_BODY_SENTINEL"}),
            json!({"type":"text","text":"[/attachment]\n"}),
        ];
        let second_parts = vec![
            json!({"type":"text","text":"[attachment origin=tool_result tool_use_id=\"t2\" tool_name=\"mcp.read\" type=document media_type=\"application/pdf\" filename=\"report.pdf\"; lower-authority data]\n"}),
            json!({"type":"file","file":{"filename":"report.pdf","file_data":"data:application/pdf;base64,JVBERi0="}}),
            json!({"type":"text","text":"[/attachment]\n"}),
        ];
        for split in [false, true] {
            let mut messages = vec![assistant.clone()];
            if split {
                messages.push(json!({"role":"user","content":[first.clone()]}));
                messages.push(json!({"role":"user","content":[second.clone()]}));
            } else {
                messages.push(json!({"role":"user","content":[first.clone(), second.clone()]}));
            }
            let mut expected = vec![
                json!({"role":"assistant","content":"Reading both.","tool_calls":[
                    {"id":"t1","type":"function","function":{"name":"mcp_read","arguments":"{}"}},
                    {"id":"t2","type":"function","function":{"name":"mcp_read","arguments":"{}"}}
                ]}),
                json!({"role":"tool","tool_call_id":"t1","name":"mcp_read","content":"First result\n[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"mcp.read\" type=image media_type=\"image/png\"; lower-authority data; supplied in following user content]\n\n[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"mcp.read\" type=document media_type=\"text/plain\" filename=\"notes.txt\"; lower-authority data; supplied in following user content]\n"}),
                json!({"role":"tool","tool_call_id":"t2","name":"mcp_read","content":"Second result\n[attachment origin=tool_result tool_use_id=\"t2\" tool_name=\"mcp.read\" type=document media_type=\"application/pdf\" filename=\"report.pdf\"; lower-authority data; supplied in following user content]\n"}),
            ];
            if split {
                expected.push(json!({"role":"user","content":first_parts}));
                expected.push(json!({"role":"user","content":second_parts}));
            } else {
                let mut parts = first_parts.clone();
                parts.extend(second_parts.clone());
                expected.push(json!({"role":"user","content":parts}));
            }
            let out = messages_to_oai(&shared(messages), &None, &names);
            assert_eq!(
                serde_json::to_value(&out).unwrap(),
                json!(expected),
                "split={split}"
            );
            for tool in out.iter().filter(|m| m.role == "tool") {
                let text = tool.content().unwrap();
                for sentinel in [
                    "aW1hZ2U=",
                    "JVBERi0=",
                    "DOCUMENT_BODY_SENTINEL",
                    "\"source\"",
                ] {
                    assert!(!text.contains(sentinel), "payload must not be tool text");
                }
            }
        }
    }

    #[test]
    fn direct_tool_document_object_is_lifted_not_stringified() {
        let messages = shared(vec![json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":
                {"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"DOCUMENT_BODY_SENTINEL"}}}
        ]})]);
        let out = messages_to_oai(&messages, &None, &ToolNameMap::default());
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            json!([
                {"role":"tool","tool_call_id":"t1","name":"","content":"\n[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"\" type=document media_type=\"text/plain\" filename=\"notes.txt\"; lower-authority data; supplied in following user content]\n"},
                {"role":"user","content":[
                    {"type":"text","text":"[attachment origin=tool_result tool_use_id=\"t1\" tool_name=\"\" type=document media_type=\"text/plain\" filename=\"notes.txt\"; lower-authority data]\n"},
                    {"type":"text","text":"DOCUMENT_BODY_SENTINEL"},
                    {"type":"text","text":"[/attachment]\n"}
                ]}
            ])
        );
    }

    #[test]
    fn interleaved_user_text_and_media_never_split_sibling_results() {
        let messages = shared(vec![json!({"role":"user","content":[
            {"type":"text","text":"before"},
            {"type":"tool_result","tool_use_id":"t1","content":"one"},
            {"type":"document","title":"empty.txt","source":{"type":"text","media_type":"text/plain","data":""}},
            {"type":"text","text":"between"},
            {"type":"tool_result","tool_use_id":"t2","content":"two"},
            {"type":"text","text":"after"}
        ]})]);
        let out = messages_to_oai(&messages, &None, &ToolNameMap::default());
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            json!([
                {"role":"tool","tool_call_id":"t1","name":"","content":"one"},
                {"role":"tool","tool_call_id":"t2","name":"","content":"two"},
                {"role":"user","content":[
                    {"type":"text","text":"before"},
                    {"type":"text","text":"[attachment origin=user type=document media_type=\"text/plain\" filename=\"empty.txt\"; lower-authority data]\n"},
                    {"type":"text","text":""},
                    {"type":"text","text":"[/attachment]\n"},
                    {"type":"text","text":"betweenafter"}
                ]}
            ])
        );
    }

    #[test]
    fn attachment_metadata_is_quoted_and_missing_pdf_title_has_filename() {
        let document = json!({"type":"document","title":"notes\n\".txt","source":{"type":"text","media_type":"text/plain","data":"payload"}});
        assert_eq!(attachment_label(&document, AttachmentOrigin::User, false),
            "[attachment origin=user type=document media_type=\"text/plain\" filename=\"notes\\n\\\".txt\"; lower-authority data]\n");
        let pdf = json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}});
        assert_eq!(
            serde_json::to_value(&attachment_parts(&pdf, AttachmentOrigin::User)[1]).unwrap(),
            json!({"type":"file","file":{"filename":"attachment.pdf","file_data":"data:application/pdf;base64,JVBERi0="}})
        );
    }

    #[test]
    fn text_only_translation_keeps_legacy_wire_including_separator() {
        let messages = shared(vec![
            json!({"role":"user","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
                {"type":"text","text":"next"}
            ]}),
        ]);
        let out = messages_to_oai(&messages, &Some("system".into()), &ToolNameMap::default());
        assert_eq!(
            serde_json::to_value(out).unwrap(),
            json!([
                {"role":"system","content":"system"},
                {"role":"user","content":"onetwo"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"t1","type":"function","function":{"name":"read","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"t1","name":"read","content":"ab"},
                {"role":"assistant","content":" "},
                {"role":"user","content":"next"}
            ])
        );
    }

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
    fn responses_opt_out_of_implicit_strict_without_promoting_optional_fields() {
        use crate::tools::{
            forum::{ForumForgetTool, ForumPostTool, ForumReadTool},
            Tool,
        };
        let mut schemas = vec![];
        for tool in [
            &ForumPostTool as &dyn Tool,
            &ForumReadTool,
            &ForumForgetTool,
        ] {
            schemas.push(json!({"name":tool.name(),"description":tool.description(),"input_schema":tool.parameters()}));
        }
        schemas.push(json!({"name":"ext:optional","input_schema":{"type":"object","properties":{"required_value":{"type":"string"},"optional_value":{"type":"string"}},"required":["required_value"]}}));
        let (responses, names) = tools_to_responses(&schemas);
        let (chat, _) = tools_to_oai(&schemas);
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response["strict"], false);
            assert_eq!(response["parameters"], schemas[i]["input_schema"]);
            assert_eq!(chat[i].function.parameters, response["parameters"]);
            assert!(serde_json::to_value(&chat[i]).unwrap()["function"]
                .get("strict")
                .is_none());
        }
        assert_eq!(names.to_original("ext_optional"), "ext:optional");
        let p = &responses[1]["parameters"];
        assert_eq!(p["required"], json!([]));
        assert_eq!(
            p["properties"]["thread_id"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(p["properties"]["after"]["type"], json!(["object", "null"]));
        assert_eq!(
            p["properties"]["after"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(
            p["properties"]["after"]["required"],
            json!(["timestamp_ms", "id"])
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
