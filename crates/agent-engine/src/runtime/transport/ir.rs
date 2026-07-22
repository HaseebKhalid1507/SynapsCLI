//! Provider-neutral normalized request IR (Task 9, spec §6.3).
//!
//! [`NormalizedRequest`] is the canonical in-memory representation of one
//! outgoing model request: ordered system segments followed by conversation
//! messages whose blocks are text, reasoning/thinking metadata, tool calls,
//! tool results (including error state), media/attachments, or unknown
//! opaque provider blocks.
//!
//! CONTENT POLICY: unlike the trace envelope (`runtime::trace`, metadata
//! only), the IR **does carry content** — it is the request representation
//! itself. Content must never derive authority and must never leak through
//! logging: every content-bearing field is wrapped in [`IrText`] /
//! [`IrPayload`], whose `Debug` impls print only byte lengths. Identifier
//! fields (tool-call IDs, tool names, provider tags) are deliberately plain —
//! they are provider identifiers, not content.
//!
//! BORROWING: production normalization ([`NormalizedRequest::from_anthropic_history`])
//! borrows straight out of the `Arc<Value>`-backed [`crate::SharedMessage`]
//! history via `Cow::Borrowed` — no second full-history deep copy. Fixture
//! deserialization produces `Cow::Owned` data through the same types.
//!
//! SCOPE (documented compatibility boundary): today the Anthropic adapter
//! (`transport::anthropic`) serializes the wire body from the original
//! `SharedMessage` slice through the byte-identity-gated
//! `runtime::request::RequestBody` compatibility serializer; the IR built
//! here is the analysis source for the [`super::report::TranslationReport`].
//! Full canonicalization — serializing provider wire bodies *from* the IR —
//! arrives with the non-Anthropic provider adapters (plan Task 10+), which
//! is when cache-marker placement and provider-specific annotations gain IR
//! representations. `cache_control` annotations are transport metadata, not
//! conversation semantics, and are deliberately absent from the IR.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

/// Redaction sentinel used by the custom `Debug` impls below.
const REDACTED: &str = "<content redacted>";

/// Content-bearing text. `Debug` prints only the byte length — never the
/// text itself — so accidental `{:?}` logging cannot leak content.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IrText<'a>(pub Cow<'a, str>);

impl std::fmt::Debug for IrText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{REDACTED}[{}B]", self.0.len())
    }
}

impl<'a> IrText<'a> {
    pub fn borrowed(s: &'a str) -> Self {
        IrText(Cow::Borrowed(s))
    }

    /// Explicit content accessor — reserved for provider adapters that
    /// canonicalize from the IR (Task 10+); never for logging.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Content-bearing structured payload (tool input, tool result content,
/// media source, opaque provider block). `Debug` prints only the serialized
/// byte length.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IrPayload<'a>(pub Cow<'a, Value>);

impl std::fmt::Debug for IrPayload<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = serde_json::to_vec(self.0.as_ref())
            .map(|v| v.len())
            .unwrap_or(0);
        write!(f, "{REDACTED}[{len}B]")
    }
}

impl<'a> IrPayload<'a> {
    pub fn borrowed(v: &'a Value) -> Self {
        IrPayload(Cow::Borrowed(v))
    }

    /// Explicit content accessor — reserved for provider adapters that
    /// canonicalize from the IR (Task 10+); never for logging.
    #[allow(dead_code)]
    pub fn as_value(&self) -> &Value {
        self.0.as_ref()
    }
}

/// Ordered system segment classification (mirrors the trace vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemSegmentKind {
    Primary,
    Orchestration,
    Memory,
    Skill,
    Other,
}

/// One ordered system segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemSegment<'a> {
    pub kind: SystemSegmentKind,
    pub text: IrText<'a>,
}

/// Message author role in the normalized conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedRole {
    User,
    Assistant,
    /// In-conversation system message (some providers). Anthropic has no
    /// wire representation for this role — see the adapter report rules.
    System,
    /// Dedicated tool role (OpenAI-style). Anthropic has no wire
    /// representation for this role — see the adapter report rules.
    Tool,
}

/// Media/attachment classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Document,
    /// Anything else (audio, video, provider-specific attachments).
    Other,
}

/// One normalized conversation block. Content-bearing fields are redacted
/// in `Debug`; identifier fields (ids, names, provider tags) are plain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedBlock<'a> {
    /// Plain text.
    Text { text: IrText<'a> },
    /// Reasoning/thinking metadata. `redacted: true` marks an opaque
    /// provider-encrypted reasoning block whose payload stays on the
    /// original wire value (never copied into the IR).
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<IrText<'a>>,
        /// Provider integrity signature (identifier-like, but treated as
        /// content-adjacent: redacted in `Debug`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<IrText<'a>>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    /// Tool invocation issued by the model.
    ToolCall {
        id: Cow<'a, str>,
        name: Cow<'a, str>,
        input: IrPayload<'a>,
    },
    /// Tool result returned to the model, including error state.
    ToolResult {
        call_id: Cow<'a, str>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        content: IrPayload<'a>,
    },
    /// Media / attachment.
    Media {
        media_kind: MediaKind,
        source: IrPayload<'a>,
    },
    /// Unknown opaque provider block: retained verbatim, never logged.
    /// `provider` tags which provider's wire vocabulary the payload belongs
    /// to; adapters for other providers must report it (Unsupported/Dropped)
    /// rather than silently losing it.
    Unknown {
        provider: Cow<'a, str>,
        payload: IrPayload<'a>,
    },
}

/// One normalized conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedMessage<'a> {
    pub role: NormalizedRole,
    pub blocks: Vec<NormalizedBlock<'a>>,
}

/// Provider-neutral normalized request: ordered system segments plus the
/// conversation. Model/tool/sampling configuration intentionally stays with
/// the provider adapters until multi-provider canonicalization (Task 10+).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedRequest<'a> {
    #[serde(default)]
    pub system: Vec<SystemSegment<'a>>,
    #[serde(default)]
    pub messages: Vec<NormalizedMessage<'a>>,
}

impl<'a> NormalizedRequest<'a> {
    /// Normalize an Anthropic-shaped message history (the sanitized,
    /// cache-annotated `SharedMessage` slice) into the provider-neutral IR,
    /// borrowing every string/value — no deep copy.
    ///
    /// Role strings other than `user`/`assistant` map to `User` (mirroring
    /// the trace builder); block types outside the Anthropic vocabulary
    /// become `Unknown { provider: "anthropic" }` and are retained verbatim
    /// by the Anthropic wire path.
    pub fn from_anthropic_history(
        system_prompt: Option<&'a str>,
        messages: &'a [crate::SharedMessage],
    ) -> Self {
        let system = system_prompt
            .filter(|s| !s.is_empty())
            .map(|s| SystemSegment {
                kind: SystemSegmentKind::Primary,
                text: IrText::borrowed(s),
            })
            .into_iter()
            .collect();
        let messages = messages
            .iter()
            .map(|m| normalize_anthropic_message(m))
            .collect();
        NormalizedRequest { system, messages }
    }
}

fn normalize_anthropic_message(message: &Value) -> NormalizedMessage<'_> {
    let role = match message["role"].as_str() {
        Some("assistant") => NormalizedRole::Assistant,
        _ => NormalizedRole::User,
    };
    let blocks = match &message["content"] {
        Value::String(s) => vec![NormalizedBlock::Text {
            text: IrText::borrowed(s),
        }],
        Value::Array(items) => items.iter().map(normalize_anthropic_block).collect(),
        other => vec![NormalizedBlock::Unknown {
            provider: Cow::Borrowed("anthropic"),
            payload: IrPayload::borrowed(other),
        }],
    };
    NormalizedMessage { role, blocks }
}

fn normalize_anthropic_block(block: &Value) -> NormalizedBlock<'_> {
    match block["type"].as_str() {
        Some("text") => match block["text"].as_str() {
            Some(text) => NormalizedBlock::Text {
                text: IrText::borrowed(text),
            },
            None => opaque_anthropic(block),
        },
        Some("thinking") => NormalizedBlock::Reasoning {
            text: block["thinking"].as_str().map(IrText::borrowed),
            signature: block["signature"].as_str().map(IrText::borrowed),
            redacted: false,
        },
        Some("redacted_thinking") => NormalizedBlock::Reasoning {
            text: None,
            signature: None,
            redacted: true,
        },
        Some("tool_use") => match (block["id"].as_str(), block["name"].as_str()) {
            (Some(id), Some(name)) => NormalizedBlock::ToolCall {
                id: Cow::Borrowed(id),
                name: Cow::Borrowed(name),
                input: IrPayload::borrowed(&block["input"]),
            },
            _ => opaque_anthropic(block),
        },
        Some("tool_result") => match block["tool_use_id"].as_str() {
            Some(call_id) => NormalizedBlock::ToolResult {
                call_id: Cow::Borrowed(call_id),
                is_error: block["is_error"].as_bool().unwrap_or(false),
                content: IrPayload::borrowed(&block["content"]),
            },
            None => opaque_anthropic(block),
        },
        Some("image") => NormalizedBlock::Media {
            media_kind: MediaKind::Image,
            source: IrPayload::borrowed(block),
        },
        Some("document") => NormalizedBlock::Media {
            media_kind: MediaKind::Document,
            source: IrPayload::borrowed(block),
        },
        _ => opaque_anthropic(block),
    }
}

fn opaque_anthropic(block: &Value) -> NormalizedBlock<'_> {
    NormalizedBlock::Unknown {
        provider: Cow::Borrowed("anthropic"),
        payload: IrPayload::borrowed(block),
    }
}
