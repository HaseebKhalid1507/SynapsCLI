//! Phase 2 provider-neutral observability conformance harness
//! (request-lifecycle hardening, Task 13). Fully headless: loopback stubs,
//! in-process runtimes, and the real `synaps` binary for CLI surfaces — no
//! human, no TTY, no non-loopback network. Shared fixtures/drivers live in
//! `tests/support/phase2/mod.rs` (not a standalone test target).
//!
//! # Spec §6 "Phase 2 acceptance criteria" → test mapping
//!
//! | §6 acceptance bullet | Test(s) |
//! |----------------------|---------|
//! | All supported providers emit one schema-valid trace record for success, failure, retry, and cancellation fixtures | `s1_anthropic_success_retry_failure_cancel_records_persist_and_validate` (real `Runtime` → persisted log), `s1_openai_chat_matrix_all_provider_ids_success_failure_cancel` (every registry provider ID), `s1_openai_responses_success_failure_cancel`, `s1_gemini_success_failure_retry_cancel`, `s1_cloud_invoke_success_failure_cancel_wire_none`, `s1_extension_provider_success_and_gate_honesty`, `s1_transport_kind_table_strict_reader_accepts_all_and_fails_closed` |
//! | Trace wire digests match sent bytes | `s2_…` half of `s1_anthropic_…` (exact bytes the loopback server received, local HTTP path); wire-`None` honesty asserts in the cloud/extension/remote-broker tests |
//! | Default traces contain no raw content or credentials | sentinel scans inside every S1 test (`assert_record_conformant`) + `s5_trace_secret_exfiltration_probe` |
//! | Translation fixtures either preserve normalized meaning or report each loss/rewrite | `s4_translation_losses_explicit_or_semantics_preserved` |
//! | Timing tests independently delay headers and SSE bytes and validate the correct timing buckets | `s3_timing_buckets_headers_first_byte_model_event_are_ordered_and_distinct` (fragmented SSE included) |
//! | Slow or broken trace storage does not delay or fail a model turn | `s6_slow_storage_never_delays_turn_and_overflow_is_counted`, `s6_broken_storage_never_fails_turn_and_warns_once` |
//! | `/context` explains system, tools, history, loaded skills/memories, and changed cache component without exposing content by default | `s7_context_report_is_content_free_and_names_every_section`, `s7_intentional_tool_order_change_is_flagged` |
//! | (§6.1 controls/export) `/trace next` covers exactly one logical request incl. retries; metadata export is private + schema-valid; content export is double-opt-in | `s8_trace_next_one_shot_covers_exactly_one_logical_request_including_retries`, export-CLI half of `s1_anthropic_…` |
//! | Default workspace unchanged, no real provider calls | `s9_default_telemetry_off_persists_nothing_and_touches_loopback_only`; every stub asserts loopback hit counts and the env guard removes all provider keys |
//!
//! # Historical red evidence (documented, not re-executed)
//!
//! At the Phase 2 base `d20e03f` **no trace system existed**: no
//! `synaps-request-trace/1` envelope (added `073a7b7`), no exact-wire
//! digests or Anthropic attempt records (`b2e0f82`), no request IR
//! (`1de6426`), no OpenAI-compatible transport traces (`6e1c3dc`), no
//! Google/cloud traces (`0d5c46a`), no extension-provider traces
//! (`2831ec5`), no bounded non-blocking writer (`3e7378a`), and no
//! diagnostics/controls/export (`2c381b4`). Every test here fails against
//! `d20e03f` (missing modules, empty persisted logs); per-test headers note
//! the commit that turned each behavior green.
//!
//! # Documented limitations
//!
//! - **Codex Responses** (`openai-codex/*`) sends directly to a pinned
//!   `chatgpt.com` endpoint with no override reachable from an integration
//!   test, so its live emission cannot be driven without real egress. Its
//!   schema surface is covered by the strict transport-kind table test
//!   (Codex records share `open_ai_responses`); its emission wiring stays
//!   covered in-crate by `runtime::trace::openai_wiring_tests` (not re-run
//!   here).
//! - Chat/Responses transports define no transport-internal retry loop
//!   (the engine issues a **new logical request** instead), so the
//!   retry-fixture bullet is proven on the transports that do: Anthropic
//!   (S1/S8) and Gemini (S1).
//! - The exact-wire digest (§6.2) is asserted on the local HTTP provider
//!   path (Anthropic). Remote-broker/cloud/extension paths must NOT claim
//!   wire bytes (serialized out of process) — asserted as `wire: None`,
//!   and remote-broker sends are honestly labeled `CloudProxy` (the wire
//!   family is asserted via the endpoint path). The provider-direct
//!   transport labels for local-broker sends stay covered by the in-crate
//!   wiring tests, which can construct a `LocalBroker` with a loopback
//!   base URL (crate-private constructor).
//! - `/context` and `/trace` slash commands are thin TUI views over
//!   `Runtime::context_report` / `trace_status` / `trace_arm_next`; S7/S8
//!   drive those engine surfaces headlessly.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serial_test::serial;
use support::*;
use synaps_cli::auth::CredentialSource;
use synaps_cli::runtime::telemetry::{
    TelemetryLevel, TelemetryWriter, WriterOptions, WriterTraceSink,
};
use synaps_cli::runtime::trace::{
    keyed_digest, load_or_create_digest_key_at, CollectingTraceSink, DigestDomain, RequestTrace,
    TraceContext, TransportKind,
};
use synaps_cli::runtime::Runtime;

include!("support/phase2/cases/providers.rs");
include!("support/phase2/cases/privacy.rs");
include!("support/phase2/cases/surfaces.rs");
