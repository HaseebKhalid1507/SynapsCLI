//! Versioned, provider-neutral request trace envelope (spec §6.1, §6.4).
//!
//! `synaps-request-trace/1` carries **bounded, safe-alphabet metadata only**:
//! no field can hold free-form text. Every content-derived value is a count,
//! byte length, enum, keyed digest ([`ComponentDigest`]), or a validated
//! bounded identifier ([`TraceId`] / [`WireName`] — ≤256 bytes, restricted
//! ASCII alphabet, no whitespace/control/quotes/backslashes). Short strings
//! spelled from the ID alphabet remain representable — the enforced invariant
//! is *bounded + safe*, which structurally excludes prompt text, message
//! content, tool results, headers, query strings, and credentials of any
//! realistic shape. Digests are HMAC-SHA256, keyed by a random
//! per-installation key stored as a private regular file (`0600`, parent
//! `0700`, symlink- and special-file-refusing) under the Synaps base
//! directory — keying prevents offline dictionary confirmation of prompt
//! contents from a leaked trace log. Neither the key nor digest preimages are
//! ever logged.
//!
//! Task 7 delivered the types + serde + key/digest primitives; Task 8 wires
//! the Anthropic transports (`runtime/api.rs`, `runtime/api_sync.rs`) into
//! the emission seam in [`emit`], with structural builders in [`anthropic`]
//! (the normalized IR / `TranslationReport` arrive in Task 9; the bounded
//! persistence writer in Task 11). Task 10A extends the seam to the
//! OpenAI-compatible transports — Chat Completions and Responses/Codex —
//! via [`openai`]; Task 10B covers Gemini/Vertex and the cloud-invoke
//! routes via [`google`]; Task 10C completes the set with extension-hosted
//! providers via [`extension`].
//! Serde contract (deliberate): optional metrics serialize as **absent** —
//! never fabricated zeros — and both absent and explicit `null` deserialize
//! to `None`. Struct field order is the deterministic serialization order.

pub mod anthropic;
pub mod emit;
pub mod extension;
pub mod google;
mod key;
pub mod openai;
mod types;

pub use emit::{
    wire_meta_from_sent_bytes, AttemptClock, CollectingTraceSink, NoopTraceSink, RequestStructure,
    RequestTracer, TraceContext, TraceSink,
};
pub use key::{
    default_digest_key_path, keyed_digest, load_or_create_digest_key, load_or_create_digest_key_at,
    ComponentDigest, DigestDomain, TraceDigestKey, TraceKeyError,
};
pub use types::{
    BlockKind, BlockMeta, CacheBoundaryLocation, CacheBoundaryMeta, CacheMeta, CacheTtlClass,
    EndpointMeta, MessageMeta, MessageRole, PrefixMeta, RequestAnatomy, RequestTrace, RetryClass,
    RetryMeta, StopReason, SystemSegmentKind, SystemSegmentMeta, TimingStages, ToolMeta, TraceId,
    TraceSchemaVersion, TranslationAction, TranslationElement, TranslationLoss, TransportKind,
    TransportOutcome, UsageMeta, UsageProvenance, WireMeta, WireName, TRACE_ID_MAX_BYTES,
    TRACE_SCHEMA, WIRE_NAME_MAX_BYTES,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wiring_tests;

#[cfg(test)]
mod openai_wiring_tests;

#[cfg(test)]
mod google_wiring_tests;

#[cfg(test)]
mod extension_wiring_tests;
