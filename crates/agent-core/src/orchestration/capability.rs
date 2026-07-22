//! Typed capability identities shared across catalog/activation surfaces
//! (spec §4.1, §12).
//!
//! External identifiers become typed values before policy checks. Parsing is
//! strict and canonical: no sanitization or aliasing happens here, so two
//! distinct raw spellings can never collapse into one identity. Malformed and
//! oversized input fails closed with typed errors.

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Maximum total byte length of a serialized `ToolId` (`namespace:name`).
pub const TOOL_ID_MAX_BYTES: usize = 200;
/// Maximum byte length of the namespace segment.
pub const TOOL_ID_NAMESPACE_MAX_BYTES: usize = 64;
/// Maximum byte length of the name segment.
pub const TOOL_ID_NAME_MAX_BYTES: usize = 128;
/// Maximum byte length of a session identifier inside an activation grant.
pub const SESSION_ID_MAX_BYTES: usize = 256;

/// Typed failure for boundary parsing of [`ToolId`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ToolIdError {
    #[error("tool id is empty")]
    Empty,
    #[error("tool id is oversized: {actual} bytes exceeds limit {limit}")]
    Oversized { actual: usize, limit: usize },
    #[error("tool id is missing a `namespace:name` separator")]
    MissingNamespace,
    #[error("tool id namespace segment is empty, oversized, or non-canonical")]
    InvalidNamespace,
    #[error("tool id name segment is empty, oversized, or non-canonical")]
    InvalidName,
}

/// Stable, validated capability identity of the form `namespace:name`
/// (e.g. `builtin:bash`, `mcp.server-1:list_issues`).
///
/// Only canonical lowercase segments of `[a-z0-9_.-]` starting with an
/// alphanumeric character are accepted, which removes case/sanitization
/// alias ambiguity by construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolId(String);

fn is_canonical_segment(segment: &str, max_bytes: usize) -> bool {
    if segment.is_empty() || segment.len() > max_bytes {
        return false;
    }
    let mut chars = segment.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Reserved marker for hex-encoded segments. Verbatim segments never start
/// with it, so encoded and verbatim forms cannot collide.
const ENCODED_SEGMENT_PREFIX: &str = "enc-";
/// Reserved marker for digest-compressed oversized segments.
const DIGEST_SEGMENT_PREFIX: &str = "sha-";
/// Hex characters kept from the SHA-256 of an oversized segment (160 bits).
const DIGEST_SEGMENT_HEX_LEN: usize = 40;

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Deterministic alias-safe canonical encoding of one raw runtime segment.
///
/// - Already-canonical segments that do not spell a reserved prefix pass
///   through verbatim, so existing canonical identities are unchanged.
/// - Anything else (uppercase, Unicode, whitespace, `:`, empty, reserved
///   spellings) becomes `enc-<lowercase hex of the raw bytes>`, which is
///   injective: two distinct raw spellings can never collapse.
/// - Segments whose hex form exceeds the byte budget compress to
///   `sha-<truncated sha256 hex>` (deterministic, 160-bit collision
///   resistant, always within budget).
fn encode_segment(raw: &str, max_bytes: usize) -> String {
    debug_assert!(max_bytes >= DIGEST_SEGMENT_PREFIX.len() + DIGEST_SEGMENT_HEX_LEN);
    if is_canonical_segment(raw, max_bytes)
        && !raw.starts_with(ENCODED_SEGMENT_PREFIX)
        && !raw.starts_with(DIGEST_SEGMENT_PREFIX)
    {
        return raw.to_string();
    }
    let encoded = format!("{ENCODED_SEGMENT_PREFIX}{}", hex_lower(raw.as_bytes()));
    if encoded.len() <= max_bytes {
        return encoded;
    }
    let digest = Sha256::digest(raw.as_bytes());
    let mut hex = hex_lower(&digest);
    hex.truncate(DIGEST_SEGMENT_HEX_LEN);
    format!("{DIGEST_SEGMENT_PREFIX}{hex}")
}

impl ToolId {
    /// Parse an untrusted string into a canonical `ToolId`, failing closed on
    /// anything malformed, oversized, or non-canonical.
    pub fn parse(raw: &str) -> Result<Self, ToolIdError> {
        if raw.is_empty() {
            return Err(ToolIdError::Empty);
        }
        if raw.len() > TOOL_ID_MAX_BYTES {
            return Err(ToolIdError::Oversized {
                actual: raw.len(),
                limit: TOOL_ID_MAX_BYTES,
            });
        }
        let (namespace, name) = raw.split_once(':').ok_or(ToolIdError::MissingNamespace)?;
        if !is_canonical_segment(namespace, TOOL_ID_NAMESPACE_MAX_BYTES) {
            return Err(ToolIdError::InvalidNamespace);
        }
        if !is_canonical_segment(name, TOOL_ID_NAME_MAX_BYTES) {
            return Err(ToolIdError::InvalidName);
        }
        Ok(Self(raw.to_string()))
    }

    /// Namespace-family prefixes used by the source-aware constructors below.
    /// They are pairwise non-overlapping, so identities from different
    /// sources cannot collide even for identical raw names.
    const BUILTIN_NAMESPACE: &'static str = "builtin";
    const UNKNOWN_NAMESPACE: &'static str = "unknown";
    const EXTENSION_NAMESPACE_PREFIX: &'static str = "ext.";
    const MCP_NAMESPACE_PREFIX: &'static str = "mcp.";
    const PLUGIN_NAMESPACE_PREFIX: &'static str = "plugin.";

    fn from_source(namespace: String, raw_name: &str) -> Self {
        debug_assert!(namespace.len() <= TOOL_ID_NAMESPACE_MAX_BYTES);
        let name = encode_segment(raw_name, TOOL_ID_NAME_MAX_BYTES);
        Self(format!("{namespace}:{name}"))
    }

    fn from_prefixed_source(prefix: &str, raw_source: &str, raw_name: &str) -> Self {
        let budget = TOOL_ID_NAMESPACE_MAX_BYTES - prefix.len();
        let namespace = format!("{prefix}{}", encode_segment(raw_source, budget));
        Self::from_source(namespace, raw_name)
    }

    /// Identity of a capability compiled into this runtime.
    pub fn builtin(runtime_name: &str) -> Self {
        Self::from_source(Self::BUILTIN_NAMESPACE.to_string(), runtime_name)
    }

    /// Identity of a capability declared by a locally installed extension.
    /// The extension id and tool name are encoded independently, so existing
    /// uppercase/Unicode/colon-bearing runtime identities are representable
    /// exactly without alias collapse.
    pub fn extension(extension_id: &str, tool_name: &str) -> Self {
        Self::from_prefixed_source(Self::EXTENSION_NAMESPACE_PREFIX, extension_id, tool_name)
    }

    /// Identity of a capability served by a configured MCP server, keyed by
    /// the server id and the tool name as the server knows it.
    pub fn mcp(server_id: &str, server_tool_name: &str) -> Self {
        Self::from_prefixed_source(Self::MCP_NAMESPACE_PREFIX, server_id, server_tool_name)
    }

    /// Identity of a capability declared by a plugin definition.
    pub fn plugin(plugin_id: &str, tool_name: &str) -> Self {
        Self::from_prefixed_source(Self::PLUGIN_NAMESPACE_PREFIX, plugin_id, tool_name)
    }

    /// Identity of a dynamically registered capability with no declared
    /// origin. Kept in an explicit `unknown` namespace so it can never be
    /// mistaken for a trusted builtin.
    pub fn unclassified(runtime_name: &str) -> Self {
        Self::from_source(Self::UNKNOWN_NAMESPACE.to_string(), runtime_name)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The source namespace segment (before the `:`).
    pub fn namespace(&self) -> &str {
        self.0.split_once(':').map(|(ns, _)| ns).unwrap_or("")
    }

    /// The capability name segment (after the `:`).
    pub fn name(&self) -> &str {
        self.0.split_once(':').map(|(_, name)| name).unwrap_or("")
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic catalog mutation counter. Every catalog mutation advances the
/// generation so stale activations can be detected and invalidated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CatalogGeneration(u64);

/// Typed failure for exhausting the generation counter. Mutations observing
/// this must fail closed: no mutation may succeed without a new generation.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("catalog generation counter exhausted at u64::MAX")]
pub struct CatalogGenerationOverflow;

impl CatalogGeneration {
    pub const fn initial() -> Self {
        Self(0)
    }

    /// Construct a generation at an explicit value (boundary tests, resuming
    /// persisted counters). Constructing a generation grants nothing.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Fail-closed advancement: at `u64::MAX` there is no new generation, so
    /// the caller must abort its mutation instead of wrapping or sticking
    /// (either of which could let stale grants keep validating).
    #[must_use = "checked_next() returns the advanced generation without mutating self"]
    pub fn checked_next(self) -> Result<Self, CatalogGenerationOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(CatalogGenerationOverflow)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Deterministic SHA-256 digest (lowercase hex) of a tool's full JSON schema.
///
/// The digest hashes a canonical structural encoding — explicit type tags,
/// length framing, and lexicographically sorted object keys — computed
/// iteratively. It is therefore independent of serde_json's `preserve_order`
/// feature and map insertion order, never allocates a serialized copy of the
/// schema, performs no fallible serialization (`expect`-free on external
/// data), and cannot overflow the stack on deeply nested values.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SchemaDigest(String);

const DIGEST_TAG_NULL: u8 = 0;
const DIGEST_TAG_BOOL: u8 = 1;
const DIGEST_TAG_NUMBER: u8 = 2;
const DIGEST_TAG_STRING: u8 = 3;
const DIGEST_TAG_ARRAY: u8 = 4;
const DIGEST_TAG_OBJECT: u8 = 5;
const DIGEST_TAG_KEY: u8 = 6;

impl SchemaDigest {
    pub fn of_schema(schema: &serde_json::Value) -> Self {
        use serde_json::Value;

        enum Task<'a> {
            Value(&'a Value),
            Key(&'a str),
        }

        let mut hasher = Sha256::new();
        let mut stack: Vec<Task<'_>> = vec![Task::Value(schema)];
        while let Some(task) = stack.pop() {
            match task {
                Task::Key(key) => {
                    hasher.update([DIGEST_TAG_KEY]);
                    hasher.update((key.len() as u64).to_le_bytes());
                    hasher.update(key.as_bytes());
                }
                Task::Value(value) => match value {
                    Value::Null => hasher.update([DIGEST_TAG_NULL]),
                    Value::Bool(b) => hasher.update([DIGEST_TAG_BOOL, u8::from(*b)]),
                    Value::Number(n) => {
                        let repr = n.to_string();
                        hasher.update([DIGEST_TAG_NUMBER]);
                        hasher.update((repr.len() as u64).to_le_bytes());
                        hasher.update(repr.as_bytes());
                    }
                    Value::String(s) => {
                        hasher.update([DIGEST_TAG_STRING]);
                        hasher.update((s.len() as u64).to_le_bytes());
                        hasher.update(s.as_bytes());
                    }
                    Value::Array(items) => {
                        hasher.update([DIGEST_TAG_ARRAY]);
                        hasher.update((items.len() as u64).to_le_bytes());
                        for item in items.iter().rev() {
                            stack.push(Task::Value(item));
                        }
                    }
                    Value::Object(map) => {
                        hasher.update([DIGEST_TAG_OBJECT]);
                        hasher.update((map.len() as u64).to_le_bytes());
                        // Sort explicitly: with `preserve_order` enabled the
                        // map iterates in insertion order, which must not
                        // change the digest.
                        let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                        entries.sort_by_key(|(key, _)| *key);
                        for (key, child) in entries.into_iter().rev() {
                            stack.push(Task::Value(child));
                            stack.push(Task::Key(key));
                        }
                    }
                },
            }
        }
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// Typed failure for boundary parsing of [`SessionId`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SessionIdError {
    #[error("session id is empty")]
    Empty,
    #[error("session id is oversized: {actual} bytes exceeds limit {limit}")]
    Oversized { actual: usize, limit: usize },
    #[error("session id contains control characters")]
    ControlCharacters,
}

/// Typed, validated session identity (Task 15). Freely accepted unbounded
/// strings must not name sessions; parse at the boundary instead. The same
/// limits back [`SessionActivationGrant`] session validation, so a grant's
/// session id and a `SessionId` can never diverge in what they accept.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(raw: &str) -> Result<Self, SessionIdError> {
        if raw.is_empty() {
            return Err(SessionIdError::Empty);
        }
        if raw.len() > SESSION_ID_MAX_BYTES {
            return Err(SessionIdError::Oversized {
                actual: raw.len(),
                limit: SESSION_ID_MAX_BYTES,
            });
        }
        // Reject C0/C1/DEL control values (newline, carriage return, NUL,
        // ESC/ANSI, NEL, …): session ids reach `Display`/log output and
        // grant comparisons, so raw control bytes must fail closed at the
        // boundary instead of being smuggled through.
        if raw.chars().any(char::is_control) {
            return Err(SessionIdError::ControlCharacters);
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Typed failure for constructing a [`SessionActivationGrant`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ActivationGrantError {
    #[error("activation grant session id is empty")]
    EmptySessionId,
    #[error("activation grant session id is oversized: {actual} bytes exceeds limit {limit}")]
    OversizedSessionId { actual: usize, limit: usize },
    #[error("activation grant session id contains control characters")]
    ControlCharacterSessionId,
}

/// An exact, session-scoped activation of one catalog capability, pinned to
/// the catalog generation and schema digest it was granted against.
///
/// Introduced in Task 14 as a shared identity type; issuing and enforcing
/// grants is later work (`SessionToolSet` / `ExecutionGate`). No code path
/// issues grants at catalog insertion time.
#[derive(Clone, Debug, Serialize)]
pub struct SessionActivationGrant {
    session_id: String,
    tool_id: ToolId,
    catalog_generation: CatalogGeneration,
    schema_digest: SchemaDigest,
}

impl SessionActivationGrant {
    pub fn new(
        session_id: &str,
        tool_id: ToolId,
        catalog_generation: CatalogGeneration,
        schema_digest: SchemaDigest,
    ) -> Result<Self, ActivationGrantError> {
        // Single source of truth for session identity limits: delegate to
        // `SessionId` so grant and session parsing can never diverge.
        let session_id = SessionId::parse(session_id).map_err(|err| match err {
            SessionIdError::Empty => ActivationGrantError::EmptySessionId,
            SessionIdError::Oversized { actual, limit } => {
                ActivationGrantError::OversizedSessionId { actual, limit }
            }
            SessionIdError::ControlCharacters => ActivationGrantError::ControlCharacterSessionId,
        })?;
        Ok(Self {
            session_id: session_id.0,
            tool_id,
            catalog_generation,
            schema_digest,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }

    pub fn catalog_generation(&self) -> CatalogGeneration {
        self.catalog_generation
    }

    pub fn schema_digest(&self) -> &SchemaDigest {
        &self.schema_digest
    }

    /// True only for the exact (session, tool, generation, digest) tuple the
    /// grant was issued for. Any drift — stale generation, changed schema,
    /// different session or tool — fails closed.
    pub fn covers(
        &self,
        session_id: &str,
        tool_id: &ToolId,
        catalog_generation: CatalogGeneration,
        schema_digest: &SchemaDigest,
    ) -> bool {
        self.session_id == session_id
            && &self.tool_id == tool_id
            && self.catalog_generation == catalog_generation
            && &self.schema_digest == schema_digest
    }
}
