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

impl CatalogGeneration {
    pub const fn initial() -> Self {
        Self(0)
    }

    #[must_use = "next() returns the advanced generation without mutating self"]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Deterministic SHA-256 digest (lowercase hex) of a tool's full JSON schema.
///
/// `serde_json::Value` objects are key-sorted maps in this workspace (the
/// `preserve_order` feature is off), so semantically equal schemas serialize
/// to identical bytes and digest identically.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SchemaDigest(String);

impl SchemaDigest {
    pub fn of_schema(schema: &serde_json::Value) -> Self {
        let encoded =
            serde_json::to_vec(schema).expect("JSON value with string keys is serializable");
        Self(format!("{:x}", Sha256::digest(encoded)))
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// Typed failure for constructing a [`SessionActivationGrant`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ActivationGrantError {
    #[error("activation grant session id is empty")]
    EmptySessionId,
    #[error("activation grant session id is oversized: {actual} bytes exceeds limit {limit}")]
    OversizedSessionId { actual: usize, limit: usize },
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
        if session_id.is_empty() {
            return Err(ActivationGrantError::EmptySessionId);
        }
        if session_id.len() > SESSION_ID_MAX_BYTES {
            return Err(ActivationGrantError::OversizedSessionId {
                actual: session_id.len(),
                limit: SESSION_ID_MAX_BYTES,
            });
        }
        Ok(Self {
            session_id: session_id.to_string(),
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
