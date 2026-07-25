//! Provider-neutral request transport layer (Task 9, spec §6.3, §11).
//!
//! - [`ir`] — the normalized request IR: ordered system segments +
//!   conversation blocks (text, reasoning metadata, tool call, tool result
//!   with error state, media, unknown opaque provider block). Content-bearing
//!   with redacted `Debug`; borrows the live history (no deep copy).
//! - [`report`] — [`report::TranslationReport`]: typed
//!   dropped/merged/renamed/synthesized/downgraded/unsupported entries with
//!   bounded positional IDs and no content. Populates the trace envelope's
//!   `translation_losses` (schema `synaps-request-trace/1`, unchanged).
//! - [`anthropic`] — the reference adapter: returns wire request + report,
//!   byte-identical to the legacy body for all golden fixtures.
//!
//! Cross-provider IR fixtures live under `tests/fixtures/request_ir/` at the
//! workspace root; they encode normalized semantics (not provider wire JSON)
//! and the expected per-provider translation actions.

pub mod anthropic;
pub mod ir;
pub mod report;

#[cfg(test)]
mod tests;
