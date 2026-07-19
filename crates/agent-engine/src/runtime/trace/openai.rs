//! Task 10A: trace wiring for the OpenAI-compatible transports — Chat
//! Completions (broker-proxied) and Responses/Codex (broker-proxied xAI and
//! direct ChatGPT-backend). Mirrors the Anthropic seam in `trace::anthropic`
//! and `runtime/api.rs`:
//!
//! - **Exact bytes only.** The wire digest/length always come from the one
//!   buffer serialized by the transport. Broker-backed sends pass that same
//!   buffer through the typed [`ProxyRequest::body_bytes`] handoff, which
//!   `LocalBroker` sends verbatim. On a **remote** broker the daemon
//!   re-serializes the JSON value out of process, so those records carry
//!   `wire: None` and `TransportKind::CloudProxy` — never a digest of bytes
//!   this process did not send.
//! - **One record per actual HTTP send/attempt**, sharing a request ID with
//!   strictly increasing attempt ordinals (rule documented in
//!   [`trace::emit`](super::emit)).
//! - **Validated identity only.** Error codes are static literals or
//!   `http_<status>`; provider request IDs pass [`TraceId`] validation;
//!   normalized stop reasons come from a closed mapping. Raw provider text
//!   never enters a record.
//! - **Correctness firewall.** Everything degrades to no-ops when the
//!   context is disabled or identity is unrepresentable.
//!
//! [`ProxyRequest::body_bytes`]: agent_core::auth::ProxyRequest

use super::emit::{AttemptClock, RequestStructure, RequestTracer, TraceContext};
use super::key::TraceDigestKey;
use super::types::{
    EndpointMeta, RetryClass, StopReason, TraceId, TranslationAction, TranslationElement,
    TranslationLoss, TransportKind, UsageMeta, UsageProvenance,
};
use crate::runtime::openai::translate::ToolNameMap;
use agent_core::prompt::QualifiedModelId;
use agent_core::TurnOutcome;
use serde_json::Value;
use std::time::Duration;

/// Map an OpenAI-compatible HTTP failure status to a coarse retry class.
pub fn retry_class_for_status(status: u16) -> RetryClass {
    match status {
        429 => RetryClass::RateLimited,
        500 | 502 | 503 | 504 | 520 | 529 => RetryClass::ServerError,
        401 | 403 => RetryClass::Auth,
        408 => RetryClass::Timeout,
        _ => RetryClass::Other,
    }
}

/// Normalize an OpenAI Chat Completions `finish_reason` into the trace enum.
/// Unknown values collapse to `Other` — the raw string is never stored.
pub fn stop_reason_from_finish_reason(raw: &str) -> StopReason {
    match raw {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

/// Extract the upstream HTTP status from a redacted broker proxy error
/// (`… provider request failed: <status> <reason>`). Returns only a parsed
/// numeric status — never any provider-authored text.
pub fn broker_error_status(message: &str) -> Option<u16> {
    const MARKER: &str = "provider request failed: ";
    let rest = &message[message.find(MARKER)? + MARKER.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|s| (100..=599).contains(s))
}

/// Validate a provider-assigned request ID from response headers into a
/// bounded [`TraceId`]. Invalid or hostile values are omitted — never copied
/// raw into a trace record.
pub fn provider_request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<TraceId> {
    crate::runtime::telemetry::request_id_from_headers(headers).and_then(|s| TraceId::new(s).ok())
}

/// Provider-reported usage for a trace record. `None` inputs stay absent —
/// zeros are only recorded when the provider actually reported them.
pub fn provider_usage(input: u64, output: u64, cached: u64) -> UsageMeta {
    UsageMeta {
        provenance: UsageProvenance::ProviderReported,
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_tokens: Some(cached),
        cache_write_tokens: None,
    }
}

/// Translation-report entries for the rewrites the OpenAI adapter is known
/// to perform today: tool names sanitized for the OpenAI wire grammar.
/// Element IDs are the *original* names when they fit the bounded safe
/// grammar; otherwise the entry stands without an ID (never raw text). The
/// full per-element OpenAI IR report is incremental follow-up work (Task 9
/// allows per-provider reports to land incrementally) — but a known rename
/// is never silently claimed lossless.
pub fn renamed_tool_losses(name_map: &ToolNameMap) -> Vec<TranslationLoss> {
    name_map
        .renamed_originals()
        .into_iter()
        .map(|original| TranslationLoss {
            action: TranslationAction::Renamed,
            element: TranslationElement::Tool,
            element_id: TraceId::new(original).ok(),
        })
        .collect()
}

/// Begin tracing one OpenAI-compatible request. Returns `None` when tracing
/// is disabled or any structural identity is unrepresentable — tracing can
/// never fail the request.
///
/// `exact_sent_bytes` MUST be the exact buffer this process hands to the
/// transport (reqwest body or `ProxyRequest::body_bytes`); pass `None` on
/// remote-broker paths where the upstream bytes are serialized out of
/// process — the record then carries no wire digest rather than a false one.
#[allow(clippy::too_many_arguments)]
pub async fn begin_openai_tracer(
    trace: &TraceContext,
    provider: &str,
    model: &str,
    transport: TransportKind,
    url: &str,
    exact_sent_bytes: Option<&[u8]>,
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
    translation: Vec<TranslationLoss>,
) -> Option<RequestTracer> {
    if !trace.enabled() {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let host = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let endpoint = EndpointMeta::new(host, parsed.path()).ok()?;
    let model = QualifiedModelId::parse(format!("{provider}/{model}")).ok()?;
    // Lazy, spawn_blocking-backed key load; `None` degrades the record to
    // digest-free sections and is counted, never surfaced as an error.
    let key = trace.digest_key().await;
    let structure = openai_request_structure(
        key.as_deref(),
        exact_sent_bytes,
        messages,
        system_prompt,
        tools_schema,
        translation,
    );
    RequestTracer::begin(trace, None, model, transport, endpoint, structure)
}

/// Structural, metadata-only description of one OpenAI-compatible request.
/// The walker operates on the internal normalized (Anthropic-shaped)
/// message array shared by every transport; prompt-cache markers are not
/// representable on the OpenAI wire, so the cache section is always empty.
fn openai_request_structure(
    key: Option<&TraceDigestKey>,
    exact_sent_bytes: Option<&[u8]>,
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
    translation: Vec<TranslationLoss>,
) -> RequestStructure {
    let mut structure = super::anthropic::anthropic_request_structure(
        key,
        exact_sent_bytes.unwrap_or(&[]),
        messages,
        system_prompt,
        tools_schema,
        None,
        false,
        false,
        translation,
        None,
    );
    // No exact bytes → no wire claim, ever (remote broker serializes the
    // upstream body out of process).
    if exact_sent_bytes.is_none() {
        structure.wire = None;
    }
    // cache_control annotations on the internal messages are NOT sent on
    // the OpenAI wire — reporting them would fabricate cache boundaries.
    structure.cache = Default::default();
    structure
}

/// Per-attempt trace state for one OpenAI-compatible request: the optional
/// tracer plus this attempt's monotonic clock. Every method is a no-op when
/// tracing is disabled; nothing here can fail the request.
pub struct StreamAttempt {
    tracer: Option<RequestTracer>,
    clock: AttemptClock,
}

impl StreamAttempt {
    /// Start the first attempt's clock. Call immediately before the send.
    pub fn new(tracer: Option<RequestTracer>) -> Self {
        Self {
            tracer,
            clock: AttemptClock::start(),
        }
    }

    /// Restart the per-attempt clock (retry loops call this right before
    /// each re-send — retry clocks reset per attempt).
    pub fn restart_clock(&mut self) {
        self.clock = AttemptClock::start();
    }

    /// The trace request ID, when a tracer began (for content-capture
    /// correlation). `None` when tracing is disabled for this request.
    pub fn request_id(&self) -> Option<&super::TraceId> {
        self.tracer.as_ref().map(|t| t.request_id())
    }

    pub fn mark_headers(&mut self) {
        self.clock.mark_headers();
    }

    pub fn mark_first_byte(&mut self) {
        self.clock.mark_first_byte();
    }

    pub fn mark_first_model_event(&mut self) {
        self.clock.mark_first_model_event();
    }

    /// Record a failed attempt that WILL be retried (one record per actual
    /// send). `code` must be a static literal or `http_<status>`.
    pub fn attempt_failed(
        &mut self,
        class: RetryClass,
        delay: Duration,
        http_status: Option<u16>,
        provider_request_id: Option<TraceId>,
        code: &str,
    ) {
        if let Some(t) = self.tracer.as_mut() {
            t.attempt_failed(
                self.clock,
                class,
                delay,
                http_status,
                provider_request_id,
                code,
            );
        }
    }

    /// Final record: request completed. Subsequent finish calls are no-ops
    /// (the tracer is taken), so error paths can never double-emit.
    pub fn finish_success(
        &mut self,
        http_status: Option<u16>,
        provider_request_id: Option<TraceId>,
        stop_reason: Option<StopReason>,
        usage: Option<UsageMeta>,
    ) {
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            t.finish(
                self.clock,
                http_status,
                provider_request_id,
                stop_reason,
                usage,
                TurnOutcome::Completed,
            );
        }
    }

    /// Final record: request canceled mid-attempt.
    pub fn finish_canceled(&mut self, http_status: Option<u16>, usage: Option<UsageMeta>) {
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            t.finish(
                self.clock,
                http_status,
                None,
                None,
                usage,
                TurnOutcome::Canceled,
            );
        }
    }

    /// Final record: terminal provider/transport failure. `code` must be a
    /// static literal or `http_<status>` — never raw provider text.
    pub fn finish_failed(
        &mut self,
        code: &str,
        http_status: Option<u16>,
        provider_request_id: Option<TraceId>,
    ) {
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            let terminal = t.failed_terminal(code);
            t.finish(
                self.clock,
                http_status,
                provider_request_id,
                None,
                None,
                terminal,
            );
        }
    }
}
