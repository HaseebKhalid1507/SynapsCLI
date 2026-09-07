//! OpenAI-compatible wire types. Ported from the prototype `openai-runtime` crate.
//!
//! Note: the prototype's `StreamEvent` is renamed to `OaiEvent` to avoid clashing
//! with `crate::runtime::types::StreamEvent`.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use std::fmt;

// ─── Tool definitions (request side) ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: name.into(),
                description: Some(description.into()),
                parameters,
            },
        }
    }
}

// ─── ToolChoice ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    None,
    Auto,
    Required,
    Function(String),
}

impl Serialize for ToolChoice {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolChoice::None => ser.serialize_str("none"),
            ToolChoice::Auto => ser.serialize_str("auto"),
            ToolChoice::Required => ser.serialize_str("required"),
            ToolChoice::Function(name) => {
                #[derive(Serialize)]
                struct Named<'a> {
                    name: &'a str,
                }
                let mut s = ser.serialize_struct("ToolChoice", 2)?;
                s.serialize_field("type", "function")?;
                s.serialize_field("function", &Named { name })?;
                s.end()
            }
        }
    }
}

// ─── Tool calls (response side) ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON string. Do NOT parse mid-stream — only after
    /// `ToolCallsComplete { truncated: false }`.
    pub arguments: String,
}

// ─── ChatMessage ─────────────────────────────────────────────────────────────

/// Chat Completions content parts. Canonical attachment bytes are lowered only
/// at the provider boundary; Responses maps these same parts to its input types.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatImageUrl },
    File { file: ChatFile },
}

impl ChatContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatFile {
    pub filename: String,
    pub file_data: String,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: String,

    /// Existing text-only API. `None` serializes as JSON `null` when there are
    /// no content parts — required for assistant-with-tool-calls.
    pub content: Option<String>,

    /// Complete ordered multipart content, including any accompanying text.
    /// When present this replaces `content` on the wire, not an extra wire key.
    /// Keep attachment data out of the legacy text accessor as well.
    pub content_parts: Option<Vec<ChatContentPart>>,

    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

// Provider wire values may contain entire private documents, data URIs, and
// untrusted filenames. Debug is metadata-only even when nested in ChatRequest.
impl fmt::Debug for ChatContentPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => f
                .debug_struct("Text")
                .field("text_bytes", &text.len())
                .finish(),
            Self::ImageUrl { image_url } => f.debug_tuple("ImageUrl").field(image_url).finish(),
            Self::File { file } => f.debug_tuple("File").field(file).finish(),
        }
    }
}

impl fmt::Debug for ChatImageUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatImageUrl")
            .field("url_bytes", &self.url.len())
            .field("has_detail", &self.detail.is_some())
            .finish()
    }
}

impl fmt::Debug for ChatFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatFile")
            .field("filename_bytes", &self.filename.len())
            .field("file_data_bytes", &self.file_data.len())
            .finish()
    }
}

impl fmt::Debug for ChatMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let role = match self.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            "system" => "system",
            "developer" => "developer",
            "tool" => "tool",
            _ => "other",
        };
        f.debug_struct("ChatMessage")
            .field("role", &role)
            .field("content_bytes", &self.content.as_ref().map(String::len))
            .field("content_parts", &self.content_parts)
            .field("tool_call_count", &self.tool_calls.as_ref().map(Vec::len))
            .field("has_tool_call_id", &self.tool_call_id.is_some())
            .field("has_name", &self.name.is_some())
            .finish()
    }
}

impl Serialize for ChatMessage {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut s = ser.serialize_struct(
            "ChatMessage",
            2 + usize::from(self.tool_calls.is_some())
                + usize::from(self.tool_call_id.is_some())
                + usize::from(self.name.is_some()),
        )?;
        s.serialize_field("role", &self.role)?;
        if let Some(parts) = &self.content_parts {
            s.serialize_field("content", parts)?;
        } else {
            s.serialize_field("content", &self.content)?;
        }
        if let Some(calls) = &self.tool_calls {
            s.serialize_field("tool_calls", calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            s.serialize_field("tool_call_id", id)?;
        }
        if let Some(name) = &self.name {
            s.serialize_field("name", name)?;
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Content {
            Text(String),
            Parts(Vec<ChatContentPart>),
        }
        #[derive(Deserialize)]
        struct WireMessage {
            role: String,
            content: Option<Content>,
            tool_calls: Option<Vec<ToolCall>>,
            tool_call_id: Option<String>,
            name: Option<String>,
        }
        let wire = WireMessage::deserialize(de)?;
        let (content, content_parts) = match wire.content {
            Some(Content::Text(text)) => (Some(text), None),
            Some(Content::Parts(parts)) => (None, Some(parts)),
            None => (None, None),
        };
        Ok(Self {
            role: wire.role,
            content,
            content_parts,
            tool_calls: wire.tool_calls,
            tool_call_id: wire.tool_call_id,
            name: wire.name,
        })
    }
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            content_parts: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn user_parts(parts: Vec<ChatContentPart>) -> Self {
        Self {
            role: "user".into(),
            content: None,
            content_parts: Some(parts),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            content_parts: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            content_parts: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            content_parts: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        }
    }
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            content_parts: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
        }
    }

    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }
}

// ─── Options + Request ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

// ─── Finish reason ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
}

// ─── Stream events (OpenAI-side) ─────────────────────────────────────────────

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OaiEvent {
    RoleStart(String),
    TextDelta(String),
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: u32,
        id: String,
        delta: String,
    },
    ToolCallsComplete {
        calls: Vec<ToolCall>,
        truncated: bool,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        cached_tokens: u32,
    },
    Warning(String),
    Done,
}

// ─── Provider config ─────────────────────────────────────────────────────────

/// Credential-free routing data for one provider/model pair. Deliberately has
/// no API-key field: credentials are broker-owned and applied broker-side at
/// request time (see `agent_core::auth::broker`).
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub provider: String,
}

#[cfg(test)]
mod chat_message_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_text_and_null_serialization_is_byte_stable() {
        let call = ToolCall {
            id: "t1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        let cases = vec![
            (
                ChatMessage::user("hello"),
                r#"{"role":"user","content":"hello"}"#,
            ),
            (
                ChatMessage::system("rules"),
                r#"{"role":"system","content":"rules"}"#,
            ),
            (
                ChatMessage::assistant("answer"),
                r#"{"role":"assistant","content":"answer"}"#,
            ),
            (
                ChatMessage::assistant_tool_calls(vec![call]),
                r#"{"role":"assistant","content":null,"tool_calls":[{"id":"t1","type":"function","function":{"name":"read","arguments":"{}"}}]}"#,
            ),
            (
                ChatMessage::tool_result("t1", "read", "done"),
                r#"{"role":"tool","content":"done","tool_call_id":"t1","name":"read"}"#,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(serde_json::to_string(&message).unwrap(), expected);
            let restored: ChatMessage = serde_json::from_str(expected).unwrap();
            assert_eq!(restored.content(), message.content());
            assert!(restored.content_parts.is_none());
            assert_eq!(serde_json::to_string(&restored).unwrap(), expected);
        }
    }

    #[test]
    fn multipart_serializes_only_as_content_and_round_trips() {
        let wire = json!({"role":"user","content":[
            {"type":"text","text":"private text document"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U=","detail":"high"}},
            {"type":"file","file":{"filename":"report.pdf","file_data":"data:application/pdf;base64,JVBERi0="}}
        ]});
        let mut restored: ChatMessage = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored.content(), None);
        assert_eq!(restored.content_parts.as_ref().unwrap().len(), 3);
        assert_eq!(serde_json::to_value(&restored).unwrap(), wire);
        restored.content = Some("legacy field is not concatenated or duplicated".into());
        assert_eq!(serde_json::to_value(restored).unwrap(), wire);
    }

    #[test]
    fn debug_is_metadata_only_for_all_attachment_types_and_messages() {
        let image = ChatImageUrl {
            url: "data:image/png;base64,IMAGE_PAYLOAD_SENTINEL".into(),
            detail: Some("PRIVATE_DETAIL_SENTINEL".into()),
        };
        let file = ChatFile {
            filename: "PRIVATE_FILENAME_SENTINEL.pdf".into(),
            file_data: "data:application/pdf;base64,PDF_PAYLOAD_SENTINEL".into(),
        };
        let parts = vec![
            ChatContentPart::text("TEXT_DOCUMENT_SENTINEL"),
            ChatContentPart::ImageUrl {
                image_url: image.clone(),
            },
            ChatContentPart::File { file: file.clone() },
        ];
        let mut message = ChatMessage::user_parts(parts.clone());
        message.content = Some("LEGACY_TEXT_SENTINEL".into());
        message.name = Some("PRIVATE_NAME_SENTINEL".into());
        message.tool_call_id = Some("PRIVATE_ID_SENTINEL".into());
        message.tool_calls = Some(vec![ToolCall {
            id: "PRIVATE_ID_SENTINEL".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "PRIVATE_NAME_SENTINEL".into(),
                arguments: "PRIVATE_ARGUMENTS_SENTINEL".into(),
            },
        }]);
        let debug = format!("{image:?} {file:?} {parts:?} {message:?}");
        assert!(
            !debug.contains("SENTINEL"),
            "Debug must not emit private values"
        );
        assert!(!debug.contains("data:"));
        assert!(debug.contains("file_data_bytes"));
        assert!(debug.contains("text_bytes"));
        assert!(debug.contains("tool_call_count: Some(1)"));
    }
}
