//! Envelope metadata types for `synaps-request-trace/1`.
//!
//! Every content-derived value is a count, byte length, bounded validated
//! ID, enum, or keyed digest. See the module docs in `trace/mod.rs` for the
//! full invariant statement.

use super::key::ComponentDigest;
use agent_core::prompt::QualifiedModelId;
use agent_core::TurnOutcome;
use serde::{Deserialize, Serialize};

/// The exact version tag of this envelope schema.
pub const TRACE_SCHEMA: &str = "synaps-request-trace/1";

// --- Schema version tag ---

/// Validated schema tag — serializes as the string `synaps-request-trace/1`
/// and refuses to deserialize any other value, so a reader can never silently
/// accept a record from a different (past or future) schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TraceSchemaVersion;

impl TryFrom<String> for TraceSchemaVersion {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value == TRACE_SCHEMA {
            Ok(TraceSchemaVersion)
        } else {
            Err(format!(
                "unsupported trace schema tag (expected {TRACE_SCHEMA})"
            ))
        }
    }
}

impl From<TraceSchemaVersion> for String {
    fn from(_: TraceSchemaVersion) -> Self {
        TRACE_SCHEMA.to_string()
    }
}

// --- Validated bounded identifiers ---

/// Maximum byte length of a [`TraceId`].
pub const TRACE_ID_MAX_BYTES: usize = 256;

/// Maximum byte length of a [`WireName`].
pub const WIRE_NAME_MAX_BYTES: usize = 128;

fn validate_trace_id(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("trace id must be nonempty".to_string());
    }
    if s.len() > TRACE_ID_MAX_BYTES {
        return Err(format!(
            "trace id exceeds {TRACE_ID_MAX_BYTES} bytes ({} given)",
            s.len()
        ));
    }
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'[' | b']');
        if !ok {
            return Err(format!(
                "trace id contains forbidden byte 0x{b:02x} (allowed: [A-Za-z0-9._/:\\[\\]-])"
            ));
        }
    }
    Ok(())
}

fn validate_wire_name(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("wire name must be nonempty".to_string());
    }
    if s.len() > WIRE_NAME_MAX_BYTES {
        return Err(format!(
            "wire name exceeds {WIRE_NAME_MAX_BYTES} bytes ({} given)",
            s.len()
        ));
    }
    for b in s.bytes() {
        if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(format!(
                "wire name contains forbidden byte 0x{b:02x} (allowed: [A-Za-z0-9_-])"
            ));
        }
    }
    Ok(())
}

/// Bounded, safe-alphabet identifier — serialized as a plain JSON string.
///
/// Invariant: nonempty, at most [`TRACE_ID_MAX_BYTES`] bytes, ASCII from
/// `[A-Za-z0-9._/:\[\]-]` only (the `[`/`]` support positional paths like
/// `messages[3].blocks[1]`). Whitespace, control chars, quotes, backslashes
/// and non-ASCII are rejected on both construction and deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TraceId(String);

impl TraceId {
    /// Validated constructor.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_trace_id(&value)?;
        Ok(TraceId(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TraceId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        TraceId::new(value)
    }
}

impl TryFrom<&str> for TraceId {
    type Error = String;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        TraceId::new(value)
    }
}

impl From<TraceId> for String {
    fn from(value: TraceId) -> Self {
        value.0
    }
}

/// Wire-level tool name — stricter than [`TraceId`]: nonempty, at most
/// [`WIRE_NAME_MAX_BYTES`] bytes, `[A-Za-z0-9_-]` only (the intersection of
/// provider tool-name grammars). Serialized as a plain JSON string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WireName(String);

impl WireName {
    /// Validated constructor.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_wire_name(&value)?;
        Ok(WireName(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WireName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for WireName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        WireName::new(value)
    }
}

impl TryFrom<&str> for WireName {
    type Error = String;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        WireName::new(value)
    }
}

impl From<WireName> for String {
    fn from(value: WireName) -> Self {
        value.0
    }
}

// --- Envelope metadata types (no content-bearing fields) ---

/// Which wire protocol carried the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    GeminiGenerateContent,
    VertexGenerateContent,
    CloudProxy,
    Extension,
}

/// Maximum byte length of an endpoint host (RFC 1035 name ceiling + port).
const ENDPOINT_HOST_MAX_BYTES: usize = 262;

/// Maximum byte length of an endpoint path.
const ENDPOINT_PATH_MAX_BYTES: usize = 1024;

fn validate_endpoint_host(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("endpoint host must be nonempty".to_string());
    }
    if s.len() > ENDPOINT_HOST_MAX_BYTES {
        return Err("endpoint host too long".to_string());
    }
    // No control chars, whitespace, non-ASCII, userinfo, query, fragment,
    // or path separators anywhere in the host.
    for b in s.bytes() {
        if !(0x21..=0x7e).contains(&b) {
            return Err("endpoint host contains whitespace/control/non-ASCII".to_string());
        }
        if matches!(b, b'@' | b'?' | b'#' | b'/' | b'\\' | b'"' | b'\'') {
            return Err(format!(
                "endpoint host contains forbidden char {:?}",
                b as char
            ));
        }
    }
    fn validate_port(p: &str) -> Result<(), String> {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err("endpoint host port must be 1-5 digits".to_string());
        }
        Ok(())
    }
    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6 literal, optionally with a numeric port.
        let end = rest
            .find(']')
            .ok_or_else(|| "unterminated bracketed IPv6 host".to_string())?;
        let inner = &rest[..end];
        if inner.is_empty()
            || !inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return Err("invalid bracketed IPv6 host".to_string());
        }
        let after = &rest[end + 1..];
        if !after.is_empty() {
            let port = after
                .strip_prefix(':')
                .ok_or_else(|| "unexpected trailing bytes after IPv6 bracket".to_string())?;
            validate_port(port)?;
        }
        return Ok(());
    }
    // DNS name or IPv4 literal, optionally with a numeric port. At most one
    // colon, and only as the port separator.
    let (name, port) = match s.split_once(':') {
        Some((name, port)) => {
            if port.contains(':') {
                return Err("endpoint host contains multiple colons".to_string());
            }
            (name, Some(port))
        }
        None => (s, None),
    };
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err("endpoint host name must be [A-Za-z0-9.-]".to_string());
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Ok(())
}

fn validate_endpoint_path(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("endpoint path must be nonempty".to_string());
    }
    if !s.starts_with('/') {
        return Err("endpoint path must begin with '/'".to_string());
    }
    if s.len() > ENDPOINT_PATH_MAX_BYTES {
        return Err("endpoint path too long".to_string());
    }
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'-' | b'_' | b'.' | b'~' | b'%' | b':' | b'=' | b'+' | b','
            );
        if !ok {
            return Err(format!(
                "endpoint path contains forbidden byte 0x{b:02x} (no query/fragment/whitespace)"
            ));
        }
    }
    Ok(())
}

/// Serde shadow for [`EndpointMeta`] — keeps the wire shape `{host, path}`
/// while routing every read through validation.
#[derive(Serialize, Deserialize)]
struct RawEndpoint {
    host: String,
    path: String,
}

/// Endpoint identity — validated host and path only; never query strings,
/// fragments, userinfo, or headers. Fields are private: construction is only
/// via [`EndpointMeta::new`] (or deserialization, which applies the same
/// validation). Serializes as `{host, path}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawEndpoint", into = "RawEndpoint")]
pub struct EndpointMeta {
    host: String,
    path: String,
}

impl EndpointMeta {
    /// Validated constructor. `host` may be a DNS name, an IPv4 literal, or a
    /// bracketed IPv6 literal, each optionally with a numeric `:port` (for
    /// local fixtures/proxies). `path` must begin with `/`. Query strings
    /// (`?`), fragments (`#`), userinfo (`@`), whitespace and control chars
    /// are rejected outright.
    pub fn new(host: impl Into<String>, path: impl Into<String>) -> Result<Self, String> {
        let host = host.into();
        let path = path.into();
        validate_endpoint_host(&host)?;
        validate_endpoint_path(&path)?;
        Ok(EndpointMeta { host, path })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl TryFrom<RawEndpoint> for EndpointMeta {
    type Error = String;
    fn try_from(value: RawEndpoint) -> Result<Self, Self::Error> {
        EndpointMeta::new(value.host, value.path)
    }
}

impl From<EndpointMeta> for RawEndpoint {
    fn from(value: EndpointMeta) -> Self {
        RawEndpoint {
            host: value.host,
            path: value.path,
        }
    }
}

/// Normalized request anatomy: counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestAnatomy {
    pub system_segment_count: u32,
    pub message_count: u32,
    pub block_count: u32,
    pub tool_count: u32,
}

/// Exact-wire metadata, computed from the very bytes handed to the transport
/// (Task 8). `None` until a transport populates it — never re-serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMeta {
    pub byte_len: u64,
    pub digest: ComponentDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemSegmentKind {
    Primary,
    Orchestration,
    Memory,
    Skill,
    Other,
}

/// One system segment: type, size, keyed digest. No text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemSegmentMeta {
    pub kind: SystemSegmentKind,
    pub byte_len: u64,
    pub digest: ComponentDigest,
}

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

/// Content-block category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    Media,
    Unknown,
}

/// One block within a message: kind + size only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMeta {
    pub kind: BlockKind,
    pub byte_len: u64,
}

/// One conversation message: role and per-block metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub role: MessageRole,
    pub blocks: Vec<BlockMeta>,
}

/// One exposed tool: stable ID, wire name, schema size + keyed digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMeta {
    pub stable_id: TraceId,
    pub wire_name: WireName,
    pub schema_byte_len: u64,
    pub schema_digest: ComponentDigest,
}

/// Where a cache boundary marker sits in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBoundaryLocation {
    Tools,
    System,
    Messages,
}

/// Cache TTL class of one boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtlClass {
    FiveMinutes,
    OneHour,
}

/// One declared cache boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBoundaryMeta {
    pub location: CacheBoundaryLocation,
    /// Zero-based index of the element the boundary is attached to, within
    /// its location (tool index, system segment index, or message index).
    pub index: u32,
    pub ttl: CacheTtlClass,
}

/// Stable-prefix metadata for cache diagnosis: bytes + keyed digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixMeta {
    pub byte_len: u64,
    pub digest: ComponentDigest,
}

/// Previous-turn comparison state for one cache segment (spec §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentChange {
    /// Keyed digest identical to the previous emitted request in this
    /// session — the provider can reuse its cached prefix.
    Unchanged,
    /// Keyed digest differs from the previous emitted request.
    Changed,
    /// No previous request in this session (or the segment appeared/vanished).
    New,
}

/// Per-segment previous-turn change report plus changed-tool detail and
/// reuse estimates (spec §6.6). Every field is bounded metadata: enums,
/// validated tool IDs, and byte counts — never content. All fields are
/// optional/defaulted so `synaps-request-trace/1` records written before
/// this struct existed still deserialize.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSegmentDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<SegmentChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SegmentChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_tail: Option<SegmentChange>,
    /// Stable IDs of tools whose schema digest changed, or that were added
    /// or removed, relative to the previous emitted request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_tool_ids: Vec<TraceId>,
    /// True when the same tool set was sent in a different order — an
    /// intentional-looking prefix invalidation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tool_order_changed: bool,
    /// Estimated bytes the provider can reuse from its cached prefix
    /// (sum of unchanged-segment canonical byte lengths). An estimate over
    /// canonical component bytes, not provider-internal token accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_reused_bytes: Option<u64>,
    /// Estimated bytes the provider must recompute (changed/new segments +
    /// the history tail when it changed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_recomputed_bytes: Option<u64>,
}

/// Cache boundaries, stable-prefix digests, and previous-turn segment
/// deltas (spec §6.6). The prefix/tail digests are keyed HMAC over
/// **canonical component bytes** (see `trace::diagnostics` for the exact
/// canonicalization and its documented approximations) — never a
/// re-serialization passed off as exact wire bytes. All diagnostic fields
/// are optional for backward-compatible deserialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheMeta {
    pub boundaries: Vec<CacheBoundaryMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_prefix: Option<PrefixMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prefix: Option<PrefixMeta>,
    /// History tail: the canonical bytes of the messages *after* the last
    /// message-level cache boundary (the segment the provider recomputes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_tail: Option<PrefixMeta>,
    /// Previous-turn per-segment comparison, when a session snapshot was
    /// available to compare against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<CacheSegmentDelta>,
}

/// What a provider adapter did to a non-representable element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationAction {
    Dropped,
    Merged,
    Renamed,
    Synthesized,
    Downgraded,
    Unsupported,
}

/// Which structural element was affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationElement {
    SystemSegment,
    MessageBlock,
    Tool,
    Parameter,
    Other,
}

/// One translation loss / synthetic rewrite: action + element reference only.
/// The full `TranslationReport` (Task 9) will populate these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationLoss {
    pub action: TranslationAction,
    pub element: TranslationElement,
    /// Stable identifier of the affected element **in the normalized
    /// (pre-translation) request**: a tool stable ID, or a positional path
    /// like `messages[3].blocks[1]` — never content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<TraceId>,
}

/// Coarse retry classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    RateLimited,
    Overloaded,
    ServerError,
    Network,
    Timeout,
    Auth,
    Other,
}

/// One retry: attempt ordinal, class, and delay before the next attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryMeta {
    /// 1-based ordinal of the **failed** try this entry describes.
    pub attempt: u32,
    pub class: RetryClass,
    /// Backoff delay before the next try, in milliseconds.
    pub delay_ms: u64,
}

/// Timing stages (spec §6.4). Every stage is optional: a stage that was not
/// observed is `None`, never a fabricated zero. Offsets are milliseconds from
/// `send_start_unix_ms`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingStages {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_start_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_byte_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_model_event_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_end_ms: Option<u64>,
}

/// Where a usage number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    /// Reported verbatim by the provider in the response/stream.
    ProviderReported,
    /// Estimated locally (e.g. byte-based heuristics).
    Estimated,
}

/// Token usage with provenance. Individual metrics the provider did not
/// report are `None` — never zero-filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageMeta {
    pub provenance: UsageProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// Provider-assigned stop reason, normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ContentFilter,
    Other,
}

/// Common transport outcome (spec §6.4): every transport returns this shape.
/// All metrics are optional — absent/unknown is `None`, never a fabricated
/// zero. The terminal [`TurnOutcome`] is the single typed source of truth
/// produced by the engine (spec §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportOutcome {
    pub timings: TimingStages,
    /// Transport-internal retries that preceded the final try of this
    /// record: `retries.len() + 1` equals the envelope's top-level `attempt`.
    pub retries: Vec<RetryMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<TraceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageMeta>,
    pub terminal: TurnOutcome,
}

impl TransportOutcome {
    /// An outcome with no observed metrics — everything `None`/empty except
    /// the required terminal state.
    pub fn unobserved(terminal: TurnOutcome) -> Self {
        Self {
            timings: TimingStages::default(),
            retries: Vec::new(),
            provider_request_id: None,
            http_status: None,
            stop_reason: None,
            usage: None,
            terminal,
        }
    }
}

// --- The envelope ---

/// One `synaps-request-trace/1` record: metadata for a single request
/// attempt. Field order is the canonical, deterministic serialization order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTrace {
    /// Always `synaps-request-trace/1`; validated on read.
    pub schema: TraceSchemaVersion,
    pub session_id: TraceId,
    pub turn_id: TraceId,
    pub request_id: TraceId,
    /// 1-based try ordinal for this record: the number of tries the
    /// transport made, i.e. `outcome.retries.len() + 1`.
    pub attempt: u32,
    /// Exact provider-qualified model identity (`provider/model`).
    pub model: QualifiedModelId,
    pub transport: TransportKind,
    pub endpoint: EndpointMeta,
    pub anatomy: RequestAnatomy,
    /// Exact-wire metadata — populated by transports (Task 8) from the very
    /// bytes sent; `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireMeta>,
    pub system_segments: Vec<SystemSegmentMeta>,
    pub messages: Vec<MessageMeta>,
    pub tools: Vec<ToolMeta>,
    pub cache: CacheMeta,
    pub translation_losses: Vec<TranslationLoss>,
    pub outcome: TransportOutcome,
}

impl RequestTrace {
    /// Provider segment of the qualified model identity.
    pub fn provider(&self) -> &str {
        self.model.provider()
    }
}
