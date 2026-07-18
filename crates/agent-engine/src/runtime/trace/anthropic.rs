//! Metadata-only structural description of an Anthropic Messages request
//! (Task 8). Every function here consumes trusted structural inputs — the
//! already-sanitized message array, the caller-supplied system prompt, the
//! registered tool schemas, and the exact serialized wire bytes — and
//! produces only counts, byte lengths, enums, validated IDs, and keyed
//! digests. No raw string is ever retained.

use super::emit::{wire_meta_from_sent_bytes, RequestStructure};
use super::key::{keyed_digest, DigestDomain, TraceDigestKey};
use super::types::{
    BlockKind, BlockMeta, CacheBoundaryLocation, CacheBoundaryMeta, CacheMeta, CacheTtlClass,
    MessageMeta, MessageRole, RequestAnatomy, RetryClass, StopReason, SystemSegmentKind,
    SystemSegmentMeta, ToolMeta, TraceId, WireName,
};
use serde_json::Value;

/// Map an Anthropic HTTP failure status to a coarse retry class.
pub fn retry_class_for_status(status: u16) -> RetryClass {
    match status {
        429 => RetryClass::RateLimited,
        529 => RetryClass::Overloaded,
        500 | 502 | 503 => RetryClass::ServerError,
        401 | 403 => RetryClass::Auth,
        408 => RetryClass::Timeout,
        _ => RetryClass::Other,
    }
}

/// Map an in-stream Anthropic error type (already vetted upstream) to a
/// coarse retry class. `None` means the stream died at transport level.
pub fn retry_class_for_stream_error(error_type: Option<&str>) -> RetryClass {
    match error_type {
        Some("overloaded_error") => RetryClass::Overloaded,
        Some("rate_limit_error") => RetryClass::RateLimited,
        Some("api_error") => RetryClass::ServerError,
        Some(_) => RetryClass::Other,
        None => RetryClass::Network,
    }
}

/// Normalize an Anthropic wire `stop_reason` into the trace enum. Unknown
/// values collapse to `Other` — the raw string is never stored.
pub fn stop_reason_from_wire(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

/// Build the structural description of one Anthropic request.
///
/// `sent_bytes` MUST be the exact buffer handed to reqwest — wire length and
/// digest come from it directly, never from re-serialization. When `key` is
/// `None` (digest key unavailable) all digest-bearing sections (`wire`,
/// `system_segments`, `tools`) are omitted; counts in `anatomy` remain.
pub fn anthropic_request_structure(
    key: Option<&TraceDigestKey>,
    sent_bytes: &[u8],
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
    prefix_marker_ttl: Option<CacheTtlClass>,
    has_tool_marker: bool,
    has_system_marker: bool,
) -> RequestStructure {
    let message_meta: Vec<MessageMeta> = messages.iter().map(|m| message_meta(m)).collect();
    let block_count: u32 = message_meta.iter().map(|m| m.blocks.len() as u32).sum();

    let system_segment_count = u32::from(system_prompt.is_some_and(|s| !s.is_empty()));
    let anatomy = RequestAnatomy {
        system_segment_count,
        message_count: messages.len() as u32,
        block_count,
        tool_count: tools_schema.len() as u32,
    };

    let system_segments = match (key, system_prompt) {
        (Some(key), Some(s)) if !s.is_empty() => vec![SystemSegmentMeta {
            kind: SystemSegmentKind::Primary,
            byte_len: s.len() as u64,
            digest: keyed_digest(key, DigestDomain::SystemSegment, s.as_bytes()),
        }],
        _ => Vec::new(),
    };

    let tools = key
        .map(|key| {
            tools_schema
                .iter()
                .filter_map(|t| tool_meta(key, t))
                .collect()
        })
        .unwrap_or_default();

    RequestStructure {
        anatomy,
        wire: key.map(|key| wire_meta_from_sent_bytes(key, sent_bytes)),
        system_segments,
        messages: message_meta,
        tools,
        cache: cache_meta(
            messages,
            tools_schema.len(),
            prefix_marker_ttl,
            has_tool_marker,
            has_system_marker,
        ),
    }
}

fn message_meta(message: &Value) -> MessageMeta {
    let role = match message["role"].as_str() {
        Some("assistant") => MessageRole::Assistant,
        Some("system") => MessageRole::System,
        Some("tool") => MessageRole::Tool,
        _ => MessageRole::User,
    };
    let blocks = match &message["content"] {
        Value::String(s) => vec![BlockMeta {
            kind: BlockKind::Text,
            byte_len: s.len() as u64,
        }],
        Value::Array(items) => items.iter().map(block_meta).collect(),
        other => vec![BlockMeta {
            kind: BlockKind::Unknown,
            byte_len: serialized_len(other),
        }],
    };
    MessageMeta { role, blocks }
}

fn block_meta(block: &Value) -> BlockMeta {
    let kind = match block["type"].as_str() {
        Some("text") => BlockKind::Text,
        Some("thinking") | Some("redacted_thinking") => BlockKind::Thinking,
        Some("tool_use") => BlockKind::ToolUse,
        Some("tool_result") => BlockKind::ToolResult,
        Some("image") | Some("document") => BlockKind::Media,
        _ => BlockKind::Unknown,
    };
    BlockMeta {
        kind,
        byte_len: serialized_len(block),
    }
}

fn serialized_len(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn tool_meta(key: &TraceDigestKey, tool: &Value) -> Option<ToolMeta> {
    let name = tool["name"].as_str()?;
    // Validated identifiers: a tool whose name does not fit the safe
    // grammar is omitted from the digest section (still counted in
    // anatomy) — the raw name is never copied into the record.
    let wire_name = WireName::new(name).ok()?;
    let stable_id = TraceId::new(name).ok()?;
    let schema_bytes = serde_json::to_vec(&tool["input_schema"]).ok()?;
    Some(ToolMeta {
        stable_id,
        wire_name,
        schema_byte_len: schema_bytes.len() as u64,
        schema_digest: keyed_digest(key, DigestDomain::ToolSchema, &schema_bytes),
    })
}

/// Cache boundaries: message-tail markers are read from the annotated
/// message array (structural, trusted — the annotation this process applied);
/// tools/system prefix markers are reported from the request-builder's own
/// marker flags with the configured prefix TTL class.
fn cache_meta(
    messages: &[crate::SharedMessage],
    tool_count: usize,
    prefix_marker_ttl: Option<CacheTtlClass>,
    has_tool_marker: bool,
    has_system_marker: bool,
) -> CacheMeta {
    let mut boundaries = Vec::new();
    if let (Some(ttl), true) = (prefix_marker_ttl, has_tool_marker) {
        boundaries.push(CacheBoundaryMeta {
            location: CacheBoundaryLocation::Tools,
            index: tool_count.saturating_sub(1) as u32,
            ttl,
        });
    }
    if let (Some(ttl), true) = (prefix_marker_ttl, has_system_marker) {
        boundaries.push(CacheBoundaryMeta {
            location: CacheBoundaryLocation::System,
            index: 0,
            ttl,
        });
    }
    for (index, message) in messages.iter().enumerate() {
        let marker = message["content"]
            .as_array()
            .and_then(|blocks| blocks.last())
            .and_then(|b| b.get("cache_control"));
        if let Some(marker) = marker {
            let ttl = if marker.get("ttl").and_then(Value::as_str) == Some("1h") {
                CacheTtlClass::OneHour
            } else {
                CacheTtlClass::FiveMinutes
            };
            boundaries.push(CacheBoundaryMeta {
                location: CacheBoundaryLocation::Messages,
                index: index as u32,
                ttl,
            });
        }
    }
    CacheMeta {
        boundaries,
        tools_prefix: None,
        system_prefix: None,
    }
}
