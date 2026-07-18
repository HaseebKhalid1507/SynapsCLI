//! Task 10C: trace wiring for extension-hosted providers (completing the
//! Task 10 transport coverage). Mirrors the seams in `trace::anthropic` /
//! `trace::openai` / `trace::google` with extension-specific honesty rules:
//!
//! - **Trace only actual IPC.** A record begins only after every routing gate
//!   has passed — provider existence, per-provider trust, trust-state
//!   readability, and the pre-IPC cancellation check — and immediately before
//!   the `provider_stream`/`provider_complete` call crosses the extension
//!   process boundary. Blocked, untrusted, unavailable, or
//!   cancelled-before-start requests emit **no** request-attempt record.
//! - **Wire is always `None`.** JSON-RPC framing and serialization for the
//!   sidecar transport are owned by the extension process transport, not this
//!   routing layer — there is no buffer of exact bytes here, so no wire
//!   digest is ever claimed (same rule as the remote-broker paths).
//! - **Attempt rule (documented invariant):** one outer extension turn is one
//!   transport attempt — one record per `provider_stream` call or per outer
//!   `provider_complete`/`complete_provider_with_tools` turn. The tool-loop
//!   helper may invoke `provider.complete` several times inside one turn;
//!   those interior tool-use iterations are *not* separate request attempts
//!   and are deliberately not counted as retries (they are successful calls,
//!   not failures — the `attempt`/`retries` invariant would be false for
//!   them). The record therefore never claims an interior call count.
//! - **Reserved static endpoint identity.** The endpoint host is the
//!   RFC 2606-style reserved name `extension.invalid` with static paths
//!   `/provider/stream` / `/provider/complete`; the exact qualified
//!   `plugin:provider/model` identity travels in the validated model field.
//!   Plugin file paths, spawn commands, params, messages, tools, errors and
//!   credentials never enter a record.
//! - **Extension-controlled data is untrusted.** Error text from the
//!   extension is replaced by the static code
//!   [`EXTENSION_PROVIDER_ERROR_CODE`]; stop reasons pass a closed mapping;
//!   usage numbers are accepted only as plain unsigned integers under known
//!   keys (never copied as raw values of any other shape).

use super::emit::{AttemptClock, RequestStructure, RequestTracer, TraceContext};
use super::types::{
    EndpointMeta, StopReason, TraceId, TranslationAction, TranslationElement, TranslationLoss,
    TransportKind, UsageMeta, UsageProvenance,
};
use agent_core::prompt::QualifiedModelId;
use agent_core::TurnOutcome;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Reserved, never-routable endpoint host for extension provider records.
/// `.invalid` is an RFC 2606 reserved TLD — this can never collide with or
/// leak a real network endpoint.
pub const EXTENSION_ENDPOINT_HOST: &str = "extension.invalid";

/// Static endpoint path for `provider.stream` IPC.
pub const EXTENSION_STREAM_PATH: &str = "/provider/stream";

/// Static endpoint path for `provider.complete` IPC (including the tool-loop
/// outer turn).
pub const EXTENSION_COMPLETE_PATH: &str = "/provider/complete";

/// The only failure code an extension-controlled error may produce in a
/// trace record — extension error text is untrusted and never copied.
pub const EXTENSION_PROVIDER_ERROR_CODE: &str = "extension_provider_error";

/// Normalize an extension-reported stop reason through a closed mapping.
/// Unknown values collapse to `Other`; the raw string is never stored.
pub fn stop_reason_from_extension(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::Other,
    }
}

/// Extract provider-reported usage from the extension's free-shape `usage`
/// value. Only plain unsigned integers under the known keys are accepted;
/// anything else stays `None` — numbers are never fabricated and non-numeric
/// extension content is never copied. Returns `None` when no metric parses.
pub fn usage_from_extension_value(usage: Option<&Value>) -> Option<UsageMeta> {
    let usage = usage?.as_object()?;
    let field = |k: &str| usage.get(k).and_then(Value::as_u64);
    let meta = UsageMeta {
        provenance: UsageProvenance::ProviderReported,
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
        cache_write_tokens: field("cache_creation_input_tokens"),
    };
    if meta.input_tokens.is_none()
        && meta.output_tokens.is_none()
        && meta.cache_read_tokens.is_none()
        && meta.cache_write_tokens.is_none()
    {
        return None;
    }
    Some(meta)
}

/// Capability-driven translation report for the extension route, structural
/// only. Known losses today:
///
/// - Tools are exposed to a model whose capabilities do not include
///   `tool_use` (this is exactly the condition that selects the streaming
///   path when tools exist — streaming tool-use events are ignored by the
///   current forwarder): each tool is `Unsupported`. Element IDs are the
///   tool names only when they fit the bounded safe grammar; otherwise the
///   entry stands without an ID — unvalidated names are never copied.
/// - The non-streaming display path joins the response's text blocks into a
///   single live text event: reported as one `Merged` message-block entry.
pub fn extension_capability_losses(
    tools_schema: &[Value],
    model_tool_use: bool,
    nonstreaming_display_merge: bool,
) -> Vec<TranslationLoss> {
    let mut losses = Vec::new();
    if !tools_schema.is_empty() && !model_tool_use {
        for tool in tools_schema {
            losses.push(TranslationLoss {
                action: TranslationAction::Unsupported,
                element: TranslationElement::Tool,
                element_id: tool
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|name| TraceId::new(name).ok()),
            });
        }
    }
    if nonstreaming_display_merge {
        losses.push(TranslationLoss {
            action: TranslationAction::Merged,
            element: TranslationElement::MessageBlock,
            element_id: None,
        });
    }
    losses
}

/// Begin tracing one extension provider IPC turn. Returns `None` when
/// tracing is disabled or the qualified identity is unrepresentable —
/// tracing can never fail or alter the provider request.
#[allow(clippy::too_many_arguments)]
pub async fn begin_extension_tracer(
    trace: &TraceContext,
    plugin_id: &str,
    provider_id: &str,
    model_id: &str,
    streaming: bool,
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
    translation: Vec<TranslationLoss>,
) -> Option<RequestTracer> {
    if !trace.enabled() {
        return None;
    }
    // The qualified identity is `plugin:provider/model`. A `/` inside the
    // plugin or provider segment would shift the provider boundary of the
    // QualifiedModelId — refuse rather than record a misattributed identity.
    if plugin_id.contains('/') || provider_id.contains('/') {
        return None;
    }
    let model = QualifiedModelId::parse(format!("{plugin_id}:{provider_id}/{model_id}")).ok()?;
    let path = if streaming {
        EXTENSION_STREAM_PATH
    } else {
        EXTENSION_COMPLETE_PATH
    };
    let endpoint = EndpointMeta::new(EXTENSION_ENDPOINT_HOST, path).ok()?;
    let key = trace.digest_key().await;
    let mut structure: RequestStructure = super::anthropic::anthropic_request_structure(
        key.as_deref(),
        &[],
        messages,
        system_prompt,
        tools_schema,
        None,
        false,
        false,
        translation,
    );
    // The JSON-RPC sidecar transport owns framing/serialization — this
    // process never holds the exact bytes, so no wire digest is ever claimed.
    structure.wire = None;
    // cache_control annotations on the internal messages are not a wire
    // contract with the extension; reporting boundaries would fabricate them.
    structure.cache = Default::default();
    RequestTracer::begin(trace, model, TransportKind::Extension, endpoint, structure)
}

/// Sentinel meaning "no first model event observed yet".
const FIRST_EVENT_UNSET: u64 = u64::MAX;

/// Cheap cloneable handle the stream forwarder uses to mark the first model
/// event (first `TextDelta`/`ThinkingDelta`/`ToolUse`/`Usage`). Set-once via
/// compare-exchange; a `None` interior (tracing disabled) makes `mark()` a
/// no-op with zero shared state.
#[derive(Clone)]
pub struct FirstEventMark(Option<(Instant, Arc<AtomicU64>)>);

impl FirstEventMark {
    pub fn mark(&self) {
        if let Some((started, cell)) = &self.0 {
            let _ = cell.compare_exchange(
                FIRST_EVENT_UNSET,
                started.elapsed().as_millis() as u64,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

/// Per-turn trace state for one extension provider IPC invocation: the
/// optional tracer, this turn's monotonic clock, and the shared first-event
/// cell. Every method is a no-op when tracing is disabled; nothing here can
/// fail the request. Exactly one `finish_*` record is emitted — the tracer
/// is taken on finish, so later calls can never double-emit.
pub struct ExtensionAttempt {
    tracer: Option<RequestTracer>,
    clock: AttemptClock,
    started: Instant,
    first_event: Arc<AtomicU64>,
}

impl ExtensionAttempt {
    /// Start the turn clock. Call immediately before the IPC send.
    pub fn new(tracer: Option<RequestTracer>) -> Self {
        Self {
            tracer,
            clock: AttemptClock::start(),
            started: Instant::now(),
            first_event: Arc::new(AtomicU64::new(FIRST_EVENT_UNSET)),
        }
    }

    /// Handle for the forwarder task to mark the first model event.
    pub fn first_event_mark(&self) -> FirstEventMark {
        if self.tracer.is_none() {
            return FirstEventMark(None);
        }
        FirstEventMark(Some((self.started, self.first_event.clone())))
    }

    fn apply_first_event(&mut self) {
        let observed = self.first_event.load(Ordering::Acquire);
        if observed != FIRST_EVENT_UNSET {
            self.clock.set_first_model_event_offset(observed);
        }
    }

    /// Final record: turn completed. Stop reason/usage stay `None` when the
    /// extension reported nothing — never fabricated.
    pub fn finish_success(&mut self, stop_reason: Option<StopReason>, usage: Option<UsageMeta>) {
        self.apply_first_event();
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            t.finish(
                self.clock,
                None,
                None,
                stop_reason,
                usage,
                TurnOutcome::Completed,
            );
        }
    }

    /// Final record: canceled while the IPC turn was active.
    pub fn finish_canceled(&mut self) {
        self.apply_first_event();
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            t.finish(self.clock, None, None, None, None, TurnOutcome::Canceled);
        }
    }

    /// Final record: the extension turn failed. `code` must be a static
    /// literal ([`EXTENSION_PROVIDER_ERROR_CODE`]) — never extension text.
    pub fn finish_failed(&mut self, code: &str) {
        self.apply_first_event();
        self.clock.mark_stream_end();
        if let Some(t) = self.tracer.take() {
            let terminal = t.failed_terminal(code);
            t.finish(self.clock, None, None, None, None, terminal);
        }
    }
}
