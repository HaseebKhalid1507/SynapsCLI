//! Task 30 — typed compaction summary provenance (spec §9.3).
//!
//! A compaction summary is a typed context artifact: it carries provenance
//! (where it came from, who produced it, under which prompt stack and
//! disclosure classes) and enters model context through ONE canonical,
//! sanitized rendering. It is neither ordinary user text nor immutable
//! system policy:
//!
//! - wrapper/escaping injection inside a summary body is neutralized before
//!   the summary is rendered into context, so hostile summary text can
//!   never close its `<context-summary>` data boundary or forge
//!   system-prompt blocks;
//! - the old system prompt survives as TYPED metadata (the session's
//!   `system_prompt` field and [`CompactionRecord::prior_system_prompt`]),
//!   never as a plain user message.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::stream_types::SharedMessage;

/// Version of the persisted summary artifact schema.
pub const COMPACTION_SUMMARY_SCHEMA_VERSION: u32 = 1;

/// Content classes source material may belong to. Shared vocabulary between
/// summary provenance (spec §9.3) and the compaction disclosure policy
/// (spec §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    UserText,
    AssistantText,
    Thinking,
    ToolCalls,
    ToolResults,
    /// STRUCTURED path-bearing surfaces only: the file-operations record
    /// derived from tool calls, and — conservatively — entire tool-call
    /// argument payloads (withheld wholesale on exclusion, because paths
    /// inside nested/positional/unrecognized argument shapes cannot be
    /// reliably identified). Paths mentioned inside free text belong to
    /// their text class (`UserText`, `AssistantText`, `ToolResults`,
    /// `EventData`); excluding those classes withholds such mentions.
    FilePaths,
    EventData,
}

impl ContentClass {
    /// Every known class, in stable order.
    pub const ALL: [ContentClass; 7] = [
        ContentClass::UserText,
        ContentClass::AssistantText,
        ContentClass::Thinking,
        ContentClass::ToolCalls,
        ContentClass::ToolResults,
        ContentClass::FilePaths,
        ContentClass::EventData,
    ];

    /// Canonical snake_case name (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentClass::UserText => "user_text",
            ContentClass::AssistantText => "assistant_text",
            ContentClass::Thinking => "thinking",
            ContentClass::ToolCalls => "tool_calls",
            ContentClass::ToolResults => "tool_results",
            ContentClass::FilePaths => "file_paths",
            ContentClass::EventData => "event_data",
        }
    }

    /// Parse a canonical snake_case name. Unknown names return `None` so
    /// config surfaces can warn instead of silently dropping policy.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

/// Where compaction summarization runs (spec §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// Summarize through the configured remote provider (default).
    #[default]
    Remote,
    /// Summarize locally — no HTTP request may be constructed.
    LocalOnly,
}

/// Redaction posture applied while serializing the source range for
/// summarization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionPolicy {
    /// Legacy behavior: tool results and thinking are truncated but no
    /// content class is excluded.
    TruncationOnly,
    /// Configured content classes were excluded per the session disclosure
    /// policy (spec §9.4).
    PolicyExclusions,
}

/// Typed provenance persisted with every compaction summary (spec §9.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    /// [`COMPACTION_SUMMARY_SCHEMA_VERSION`] at write time.
    pub schema_version: u32,
    /// Session the summarized range came from.
    pub source_session: String,
    /// Number of messages in the summarized range.
    pub source_message_count: usize,
    /// SHA-256 digest binding the exact summarized message range.
    pub source_range_digest: String,
    /// Provider that produced the summary.
    pub summary_provider: String,
    /// Model that produced the summary.
    pub summary_model: String,
    /// When the summary was produced.
    pub created_at: DateTime<Utc>,
    /// SHA-256 digest of the prompt stack the summarizer ran under.
    pub prompt_stack_digest: String,
    /// Content classes included in the summarization input.
    pub included_classes: Vec<ContentClass>,
    /// Content classes excluded from the summarization input.
    pub excluded_classes: Vec<ContentClass>,
    /// Redaction posture applied to the source range.
    pub redaction_policy: RedactionPolicy,
    /// The predecessor's system prompt as typed metadata (spec §9.3: never
    /// embedded as a plain user message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_system_prompt: Option<String>,
}

/// SHA-256 digest over the canonical serialization of a message range.
/// Order-sensitive: the digest binds both content and sequence.
pub fn message_range_digest(messages: &[SharedMessage]) -> String {
    let mut hasher = Sha256::new();
    for msg in messages {
        // Length-prefix each message so concatenation cannot collide across
        // message boundaries.
        let bytes = serde_json::to_vec(&**msg).unwrap_or_default();
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    hex_digest(hasher)
}

/// SHA-256 digest over the ordered prompt-stack parts used for
/// summarization (system prompt, instruction template, custom focus).
pub fn prompt_stack_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex_digest(hasher)
}

fn hex_digest(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Wrapper tags a summary body must never be able to open or close.
/// Matched case-insensitively on the tag STEM so `</Context-Summary>` and
/// `<CONTEXT-SUMMARY>` are both neutralized.
const PROTECTED_TAG_STEMS: [&str; 2] = ["context-summary", "system-prompt"];

/// Neutralize wrapper/escaping injection inside summary text (spec §9.3:
/// escaping cannot elevate content). Any opening or closing form of a
/// protected tag has its `<` replaced with the HTML entity, which keeps the
/// text legible while breaking the markup.
pub fn sanitize_summary_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            let mut j = i + 1;
            if j < chars.len() && chars[j] == '/' {
                j += 1;
            }
            let tail: String = chars[j..(j + 16).min(chars.len())]
                .iter()
                .collect::<String>()
                .to_lowercase();
            if PROTECTED_TAG_STEMS
                .iter()
                .any(|stem| tail.starts_with(stem))
            {
                out.push_str("&lt;");
                i += 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The ONE canonical rendering of a compaction summary into model context:
/// a sanitized `<context-summary>` user message plus the assistant
/// acknowledgement. Every frontend and both transition policies (linked
/// successor and in-place) share this shape — that is what makes their
/// logical histories equivalent.
pub fn compaction_context_messages(summary_text: &str) -> Vec<SharedMessage> {
    let sanitized = sanitize_summary_text(summary_text);
    let user = format!(
        "The conversation history before this point was compacted into the \
         following summary:\n\n<context-summary>\n{}\n</context-summary>\n\n\
         Continue from where we left off. The summary and the system prompt \
         contain all the context you need.",
        sanitized
    );
    vec![
        SharedMessage::new(serde_json::json!({"role": "user", "content": user})),
        SharedMessage::new(serde_json::json!({
            "role": "assistant",
            "content": "I've loaded the conversation summary. Ready to continue."
        })),
    ]
}
