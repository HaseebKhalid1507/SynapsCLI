use super::helpers::HelperMethods;
use super::net_retry::{show_transport_retry_notice, warn_transport, TransportDiag};
use super::sse_types::{AnthropicEvent, ContentBlock, Delta};
use super::types::{AuthState, LlmEvent, SessionEvent, StreamEvent};
use crate::runtime::telemetry::{self, TelemetryLevel};
use crate::{Result, RuntimeError, ToolRegistry};
#[cfg(test)]
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

/// Trace the outgoing Anthropic request. Metadata only — raw payload content
/// (message text, system prompt, tool schemas/arguments) must NEVER reach any
/// log sink at any level. Guarded by `outgoing_request_trace_is_metadata_only`.
fn trace_outgoing_request(model: &str, body_bytes: &[u8], message_count: usize, tool_count: usize) {
    // Per-process monotonic sequence: correlates this trace line with any
    // later log lines for the same request without embedding content.
    static REQUEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let correlation_id = REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Count prompt-cache markers by scanning the serialized bytes — reports
    // marker placement (a count) without deserializing or echoing content.
    const NEEDLE: &[u8] = b"\"cache_control\"";
    let cache_marker_count = body_bytes
        .windows(NEEDLE.len())
        .filter(|w| *w == NEEDLE)
        .count();
    tracing::trace!(
        provider = "anthropic",
        model = %model,
        payload_bytes = body_bytes.len(),
        message_count,
        tool_count,
        cache_marker_count,
        correlation_id,
        "Outgoing API request (metadata only)"
    );
}

/// Parse accumulated tool input JSON. On failure, returns a JSON object with
/// `__parse_error` key so the tool executor can report it back to the model.
fn parse_tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => json!({ "__parse_error": format!("invalid tool input JSON: {}", e) }),
    }
}

/// All mutable state for one SSE stream parse. Mutated exclusively through
/// `process_event()` + `finalize()` — single write path makes duplicate-site
/// drift structurally impossible.
struct ParseState {
    // ── Output accumulation (stays Value — outgoing message format) ──
    accumulated_content: Vec<Value>,
    current_text: String,
    // ── Tool-use block accumulation ──
    current_tool_name: String,
    current_tool_id: String,
    current_tool_input_json: String,
    in_tool_use: bool,
    // ── Thinking block accumulation ──
    current_thinking: String,
    current_thinking_signature: String,
    in_thinking: bool,
    /// Set by `finalize()` when it flushed a `tool_use` whose input was still
    /// streaming (no `content_block_stop`). Always the LAST accumulated block.
    /// Only `take_outcome_content(cancelled = true)` acts on it.
    flushed_partial_tool: bool,
    // ── Telemetry captures ──
    telem_msg_id: Option<String>,
    telem_stop_reason: Option<String>,
    telem_usage: telemetry::UsageRecord,
    first_event_seen: bool,
    /// Liveness, counts and timings only: the progress deadlines and the
    /// end-of-stream DEBUG line.
    progress: super::http_phases::StreamProgress,
    // ── Usage captured from message_start (single-emission protocol) ──
    // Live API shape (probed 2024+): message_start carries the full
    // `cache_creation` sub-object; message_delta carries ONLY the aggregate
    // (but repeats input/cache_read/cache_create and brings the final
    // output count). The start arm CAPTURES ONLY — the delta arm records
    // the effective counts, falling back to these values for anything the
    // delta doesn't carry, and THE single authoritative Usage event is sent
    // once, at message_stop or at the stream's end (`emit_final_usage`).
    // Emitting from both arms double-counts input/cache in every
    // accumulating consumer (the pre-cache-ttl double-billing bug).
    msg_start_input: Option<u64>,
    msg_start_output: Option<u64>,
    msg_start_cache_read: Option<u64>,
    msg_start_cache_create: Option<u64>,
    msg_start_cache_5m: Option<u64>,
    msg_start_cache_1h: Option<u64>,
    /// Set once the authoritative Usage event has been emitted
    /// (`emit_final_usage`): exactly one emission per request in all paths.
    usage_emitted: bool,
    /// The last counted message_delta's effective counts. A stream can carry
    /// more than one message_delta and their counts are cumulative, so each
    /// counted delta replaces the last, and the one Usage emission sends
    /// these. `None` until a delta reports a count.
    final_usage: Option<FinalUsage>,
    /// Captured from an in-stream Anthropic `error` event. When set, the stream
    /// produced a failure (e.g. `overloaded_error`, `request_too_large`) the API
    /// delivered inside a 200 response. `classify_stream_outcome` turns this into
    /// either a retry (transient) or a terminal `Err` so the turn never silently
    /// ends with empty content (the "stopping" bug, task #130).
    stream_error: Option<StreamError>,
    /// True once a `message_delta` carrying a stop_reason was seen — captured
    /// UNCONDITIONALLY (independent of the telemetry flag) so
    /// `classify_stream_outcome` can tell a valid empty `end_turn` from a silent
    /// stop even in the default (telemetry-off) config.
    stop_reason_seen: bool,
    /// True when the captured stop_reason is exactly "refusal" — the model
    /// declined the request. Used by `classify_stream_outcome` to gate the
    /// refusal-retry path independently of the telemetry flag.
    stop_reason_is_refusal: bool,
    // ── Trace captures (Task 8) — unconditional, metadata-only ──
    /// Offset of the first parsed model event from `ctx.request_start`
    /// (post-headers), in ms. Feeds the trace `first_model_event` bucket.
    first_event_elapsed_ms: Option<u64>,
    /// Normalized stop reason for the trace record (raw wire string is
    /// mapped immediately, never retained).
    trace_stop_reason: Option<crate::runtime::trace::StopReason>,
    /// Provider-reported usage for the trace record; `None` until observed.
    trace_usage: Option<crate::runtime::trace::UsageMeta>,
    /// Cache-diagnosis verdict from message_start (only when this request
    /// sent `diagnostics`); `None` = no divergence / not requested.
    server_diagnosis: Option<crate::runtime::trace::ServerCacheDiagnosis>,
    /// The same usage with presence kept (absent counters are `None`, not
    /// zero): the warm rolling summary's evidence and booking.
    wire_usage: super::warm_summary::WireUsage,
}

/// One request's effective usage: what the Usage event carries and the
/// root prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FinalUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    cache_create_5m: Option<u64>,
    cache_create_1h: Option<u64>,
}

/// A failure surfaced as an in-stream Anthropic `error` event under a 200
/// response. `retryable` follows Anthropic's error taxonomy: transient classes
/// (`overloaded_error`, `api_error`, `rate_limit_error`) are retried with
/// backoff; everything else (`request_too_large`, `invalid_request_error`,
/// auth/permission) is terminal — retrying a context-overflow never helps.
#[derive(Debug, Clone)]
struct StreamError {
    /// Anthropic wire error type, retained separately from display text so
    /// retry policy never depends on parsing a human-facing message.
    error_type: Option<String>,
    /// Human-facing, actionable message. Built from vetted static text only —
    /// the provider's error message is untrusted (it can echo the request)
    /// and is never copied into this field.
    message: String,
    /// True for transient classes that a backoff retry can clear.
    retryable: bool,
}

impl StreamError {
    /// Anthropic error `type`s that a retry can plausibly clear. Mirrors the
    /// HTTP-status retry set (429 rate_limit, 500 api_error, 529 overloaded).
    fn is_retryable_type(error_type: &str) -> bool {
        matches!(
            error_type,
            "overloaded_error" | "api_error" | "rate_limit_error"
        )
    }
}

/// The decided fate of a finished stream — pure, unit-tested (task #130).
#[derive(Debug)]
enum StreamOutcome {
    /// A valid turn — hand the assistant content envelope back to the caller.
    Done(Value),
    /// A transient in-stream error — caller should back off and re-send.
    Retry(StreamError),
    /// A terminal failure — surface as a visible `Err`, never a silent stop.
    Fail(String),
    /// The model set stop_reason=refusal — discard all partial content and
    /// retry the identical request (up to the refusal_retries budget).
    Refusal,
}

impl ParseState {
    fn new() -> Self {
        Self {
            accumulated_content: Vec::new(),
            current_text: String::new(),
            current_tool_name: String::new(),
            current_tool_id: String::new(),
            current_tool_input_json: String::new(),
            in_tool_use: false,
            current_thinking: String::new(),
            current_thinking_signature: String::new(),
            in_thinking: false,
            flushed_partial_tool: false,
            telem_msg_id: None,
            telem_stop_reason: None,
            telem_usage: telemetry::UsageRecord::default(),
            first_event_seen: false,
            progress: super::http_phases::StreamProgress::new(),
            msg_start_input: None,
            msg_start_output: None,
            msg_start_cache_read: None,
            msg_start_cache_create: None,
            msg_start_cache_5m: None,
            msg_start_cache_1h: None,
            usage_emitted: false,
            final_usage: None,
            stream_error: None,
            stop_reason_seen: false,
            stop_reason_is_refusal: false,
            first_event_elapsed_ms: None,
            trace_stop_reason: None,
            trace_usage: None,
            server_diagnosis: None,
            wire_usage: Default::default(),
        }
    }

    /// End-of-stream flush of any partial thinking/tool/text block.
    /// Idempotent: clears `in_*` and `current_text` so a second call is a no-op.
    fn finalize(&mut self) {
        if self.in_thinking {
            // Never emit a block with neither text nor signature — Anthropic
            // rejects it on the next turn (see content_block_stop arm).
            if keeps_thinking_block(&self.current_thinking, &self.current_thinking_signature) {
                self.accumulated_content.push(json!({
                    "type": "thinking",
                    "thinking": self.current_thinking,
                    "signature": self.current_thinking_signature
                }));
            }
            self.in_thinking = false;
        } else if self.in_tool_use {
            let input = parse_tool_input(&self.current_tool_input_json);
            self.accumulated_content.push(json!({
                "type": "tool_use",
                "id": self.current_tool_id,
                "name": self.current_tool_name,
                "input": input
            }));
            self.in_tool_use = false;
            self.flushed_partial_tool = true;
        } else if !self.current_text.is_empty() {
            self.accumulated_content.push(json!({
                "type": "text",
                "text": self.current_text
            }));
        }
        self.current_text.clear();
    }

    /// The content handed to `classify_stream_outcome`.
    ///
    /// Every stream drops unsigned thinking: an EOF before signature_delta
    /// cannot leave a replayable thinking block, even without cancellation.
    /// Otherwise uncancelled streams retain the existing content semantics.
    ///
    /// Cancelled streams keep only what the model verifiably produced, so the
    /// interrupted assistant turn can stay in history as a REAL message
    /// (never a user-side recap — see `session::actor::cancel_turn`):
    /// - partial text is kept verbatim (it is what the model said);
    /// - thinking without a signature is dropped — a block cut off before its
    ///   `signature_delta` cannot be replayed to the provider;
    /// - a `tool_use` whose input was still streaming is dropped — the model
    ///   never finished making that call, and its truncated JSON would be
    ///   recorded as `{"__parse_error": …}`. Completed calls stay (the tool
    ///   loop pairs them with a canceled result).
    fn take_outcome_content(&mut self, cancelled: bool) -> Vec<Value> {
        let mut content = std::mem::take(&mut self.accumulated_content);
        if cancelled
            && self.flushed_partial_tool
            && content.last().and_then(|b| b["type"].as_str()) == Some("tool_use")
        {
            content.pop();
        }
        content.retain(|b| {
            b["type"].as_str() != Some("thinking")
                || b["signature"].as_str().is_some_and(|s| !s.is_empty())
        });
        self.flushed_partial_tool = false;
        content
    }
}

/// Whether a streamed thinking block goes into the history: it carries text
/// or a signature (an omitted-display block is empty text plus signature).
fn keeps_thinking_block(thinking: &str, signature: &str) -> bool {
    !thinking.is_empty() || !signature.is_empty()
}

/// Immutable per-stream context — not state. `tx` deliberately lives here and
/// not in `ParseState`: read/write separation is the point.
struct EventCtx<'t> {
    model: &'t str,
    tx: &'t mpsc::UnboundedSender<StreamEvent>,
    suppress_stream_deltas: bool,
    telemetry_level: TelemetryLevel,
    request_start: std::time::Instant,
    /// Requested cache TTL — used by the silent-downgrade detector.
    cache_ttl: crate::core::config::CacheTtl,
    /// Once-per-session latch for the downgrade notice (shared via Runtime).
    ttl_downgrade_notified: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Session-scoped latch: set once any response shows a nonzero 1h cache
    /// write. A healthy Hybrid session writes the 1h prefix on turn 1 and
    /// then only the 5m tail on later turns — without this latch that
    /// signature is indistinguishable from a genuine downgrade (spec §3.4.1).
    saw_1h_honored: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// True iff at least one 1h cache marker was actually emitted in THIS
    /// request. Degenerate case (hybrid + API key + no system prompt + zero
    /// tools): no 1h marker exists anywhere in the payload, so a 1h bucket of
    /// zero proves nothing — the detector must stay disarmed.
    request_has_1h_marker: bool,
    /// Optional Task 23 turn-budget usage counters, recorded at the single
    /// authoritative Usage emission (and its residual twin) only.
    usage_counters: Option<std::sync::Arc<crate::runtime::budget::UsageCounters>>,
    /// This request sent `diagnostics`: capture the message id (chain) and
    /// the server's verdict from message_start.
    cache_diagnosis: bool,
    /// This attempt's spend guard (child spend), observe-only: it sees the
    /// message_start capture and the Usage emission's values. Inert unless
    /// the request carries a worker's spend tap.
    spend: super::child_spend::AttemptObserver,
    /// A ledgered attempt's identity (§6): its usage is reported as the
    /// attempt's (`UsageAttempt`) in place of the plain `Usage`. `None`
    /// (every request without a ledger) emits exactly the plain `Usage`.
    usage_request: Option<agent_core::UsageRequest>,
}

/// THE TEST SEAM. Strips SSE framing, skips non-data lines and the `[DONE]`
/// marker, parses JSON, dispatches to `process_event`. Both the main loop and
/// the tail-flush path call this — one write path for every parse site.
fn process_data_line(line: &str, state: &mut ParseState, ctx: &EventCtx) {
    let Some(data_part) = line.strip_prefix("data: ") else {
        return;
    };
    if data_part.trim() == "[DONE]" {
        return;
    }
    let event = match serde_json::from_str::<AnthropicEvent>(data_part) {
        Ok(e) => e,
        Err(_) => return, // malformed JSON: skip the line, never panic
    };
    process_event(event, data_part, state, ctx);
}

/// A usage object's counters with presence kept.
fn usage_presence(
    usage: Option<super::sse_types::UsagePresence>,
) -> Option<super::warm_summary::UsageCounts> {
    let usage = usage?;
    let split = usage.cache_creation.as_ref();
    Some(super::warm_summary::UsageCounts {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        cache_write_5m: split.and_then(|c| c.ephemeral_5m_input_tokens),
        cache_write_1h: split.and_then(|c| c.ephemeral_1h_input_tokens),
    })
}

/// True for Anthropic's keep-alive `ping` frame (it parses as `Unknown`).
fn is_ping(raw: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Tag<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    serde_json::from_str::<Tag>(raw).is_ok_and(|tag| tag.kind == "ping")
}

/// Handle one parsed SSE event. The TTFT capture rides along so the main and
/// tail paths are uniform. `raw` is the un-parsed data line — `#[serde(other)]`
/// discards the tag on Unknown events, so the raw line is the only forensics.
fn process_event(event: AnthropicEvent<'_>, raw: &str, state: &mut ParseState, ctx: &EventCtx) {
    // First-event capture (Task 8), unconditional. Feeds the attempt clock,
    // which both the trace and the telemetry `ttft_ms` read.
    if !state.first_event_seen {
        state.first_event_seen = true;
        state.first_event_elapsed_ms = Some(ctx.request_start.elapsed().as_millis() as u64);
    }
    // Liveness: every parsed event but a ping is progress.
    match &event {
        AnthropicEvent::Unknown if is_ping(raw) => state.progress.ping(),
        event => state
            .progress
            .event(matches!(event, AnthropicEvent::MessageStart { .. })),
    }

    match event {
        AnthropicEvent::ContentBlockStart { content_block } => match content_block {
            ContentBlock::Thinking => {
                state.current_thinking.clear();
                state.current_thinking_signature.clear();
                state.in_thinking = true;
            }
            ContentBlock::ToolUse { id, name } => {
                // Start accumulating a tool_use block
                state.current_tool_name = name.into_owned();
                state.current_tool_id = id.into_owned();
                state.current_tool_input_json.clear();
                state.in_tool_use = true;
                let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::ToolUseStart {
                    tool_name: state.current_tool_name.clone(),
                    tool_id: state.current_tool_id.clone(),
                }));
            }
            ContentBlock::Text => {
                if !state.current_text.is_empty() {
                    state.accumulated_content.push(json!({
                        "type": "text",
                        "text": state.current_text
                    }));
                    state.current_text.clear();
                }
            }
            // Unknown block type: no state change, mirrors the old `_ => {}`.
            ContentBlock::Unknown => {}
        },
        AnthropicEvent::ContentBlockDelta { delta } => match delta {
            Delta::TextDelta { text } => {
                state.current_text.push_str(&text);
                if !ctx.suppress_stream_deltas {
                    let _ = ctx
                        .tx
                        .send(StreamEvent::Llm(LlmEvent::Text(text.into_owned())));
                }
            }
            Delta::ThinkingDelta { thinking } => {
                // Anthropic sends thinking text in delta.thinking
                state.current_thinking.push_str(&thinking);
                if !ctx.suppress_stream_deltas {
                    let _ = ctx
                        .tx
                        .send(StreamEvent::Llm(LlmEvent::Thinking(thinking.into_owned())));
                }
            }
            Delta::SignatureDelta { signature } => {
                state.current_thinking_signature = signature.into_owned();
            }
            Delta::InputJsonDelta { partial_json } => {
                state.current_tool_input_json.push_str(&partial_json);
                if !ctx.suppress_stream_deltas {
                    let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::ToolUseDelta {
                        tool_id: state.current_tool_id.clone(),
                        delta: partial_json.into_owned(),
                    }));
                }
            }
            // Unknown delta subtype: no state change, mirrors the old `_ => {}`.
            Delta::Unknown => {}
        },
        AnthropicEvent::ContentBlockStop => {
            if state.in_thinking {
                // Flush thinking block with signature so it's echoed back in tool loops.
                // A block with an empty `thinking` but a signature is kept: that is
                // every block under `display: "omitted"` (and a summarized-mode
                // progress update the model wrote no summary for); the signature
                // carries the reasoning, and the API wants it back unchanged — a
                // tool loop must not partially drop its thinking blocks. Only a
                // block with neither is dropped: Anthropic rejects that one with
                // `messages.N.content.M.thinking: each thinking block must contain
                // thinking` (verified against the live API, 2026-11).
                if keeps_thinking_block(&state.current_thinking, &state.current_thinking_signature) {
                    state.accumulated_content.push(json!({
                        "type": "thinking",
                        "thinking": state.current_thinking,
                        "signature": state.current_thinking_signature
                    }));
                }
                state.in_thinking = false;
            } else if state.in_tool_use {
                // Parse the accumulated JSON input
                let input = parse_tool_input(&state.current_tool_input_json);

                state.accumulated_content.push(json!({
                    "type": "tool_use",
                    "id": state.current_tool_id,
                    "name": state.current_tool_name,
                    "input": input
                }));

                // Emit the tool_use to the UI as soon as it's fully parsed,
                // so the call appears during the assistant's stream — before
                // we hand off to the tool executor. Without this the call
                // only becomes visible immediately prior to its result.
                let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name: state.current_tool_name.clone(),
                    tool_id: state.current_tool_id.clone(),
                    input: input.clone(),
                }));

                state.in_tool_use = false;
            } else if !state.current_text.is_empty() {
                // Flush text block so ordering is preserved
                state.accumulated_content.push(json!({
                    "type": "text",
                    "text": state.current_text
                }));
                state.current_text.clear();
            }
        }
        AnthropicEvent::MessageDelta { delta, usage } => {
            if let Some(counts) = usage_presence(
                serde_json::from_str::<super::sse_types::DeltaUsagePresence>(raw)
                    .ok()
                    .and_then(|d| d.usage),
            ) {
                let earlier = state.wire_usage.delta.unwrap_or_default();
                state.wire_usage.delta = Some(earlier.overlay(counts));
            }
            // Capture stop_reason UNCONDITIONALLY — classify_stream_outcome relies
            // on it to tell a valid empty end_turn from a silent stop, and that
            // decision must NOT depend on whether telemetry is enabled (default off).
            if let Some(sr) = delta.and_then(|d| d.stop_reason) {
                state.stop_reason_seen = true;
                if sr.as_ref() == "refusal" {
                    state.stop_reason_is_refusal = true;
                }
                // Trace capture: map to the normalized enum immediately —
                // the raw wire string is never retained (Task 8).
                state.trace_stop_reason = Some(
                    crate::runtime::trace::anthropic::stop_reason_from_wire(sr.as_ref()),
                );
                if ctx.telemetry_level.enabled() {
                    state.telem_stop_reason = Some(sr.into_owned());
                }
            }
            if let Some(usage) = usage {
                // Effective values: the delta's own fields where nonzero,
                // falling back to the message_start capture. Live deltas
                // repeat all aggregates (and carry the final output count),
                // but be defensive — a sparse delta must not zero out what
                // the start already reported.
                let input_t = if usage.input_tokens > 0 {
                    usage.input_tokens
                } else {
                    state.msg_start_input.unwrap_or(0)
                };
                let output_t = if usage.output_tokens > 0 {
                    usage.output_tokens
                } else {
                    state.msg_start_output.unwrap_or(0)
                };
                let cache_read = if usage.cache_read_input_tokens > 0 {
                    usage.cache_read_input_tokens
                } else {
                    state.msg_start_cache_read.unwrap_or(0)
                };
                let cache_create = if usage.cache_creation_input_tokens > 0 {
                    usage.cache_creation_input_tokens
                } else {
                    state.msg_start_cache_create.unwrap_or(0)
                };
                // TTL breakdown: prefer the delta's own cache_creation
                // sub-object (future-proof), but in live traffic message_delta
                // carries ONLY the aggregate — the split arrives on
                // message_start. Fall back to the values captured there.
                let cache_create_5m = usage
                    .cache_creation
                    .as_ref()
                    .and_then(|cc| cc.ephemeral_5m_input_tokens)
                    .or(state.msg_start_cache_5m);
                let cache_create_1h = usage
                    .cache_creation
                    .as_ref()
                    .and_then(|cc| cc.ephemeral_1h_input_tokens)
                    .or(state.msg_start_cache_1h);

                // ═══ Silent-downgrade detector (spec §3.4.1) ═══
                // The failure mode that doesn't 400: the API accepts the
                // request but quietly honors only 5m. Fire ONE notice per
                // session and keep requesting what the user configured —
                // auto-downgrade would change pricing behavior behind the
                // user's back and mask the account-level problem.
                //
                // The saw_1h_honored latch prevents the Hybrid false
                // positive: turn 2+ of a healthy Hybrid session has
                // 1h == 0 (prefix cached) and 5m > 0 (tail rewrite) — the
                // exact downgrade signature. Turn 1's prefix write sets the
                // latch (in the message_start arm, so a stream that dies
                // before its delta still latches); a genuinely downgraded
                // account never does.
                //
                // cache_read == 0 guard: a fresh process against a still-warm
                // 1h prefix sees cache_read > 0, 1h == 0 (prefix CACHED, not
                // written), 5m > 0 (tail) — healthy, not a downgrade. A
                // genuinely downgraded account goes cold within 5m, so real
                // downgrades still fire on the first cold turn.
                //
                // request_has_1h_marker: if this request carried no 1h marker
                // at all, a zero 1h bucket proves nothing — stay silent.
                if cache_create_1h.unwrap_or(0) > 0 {
                    ctx.saw_1h_honored
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if ctx.cache_ttl != crate::core::config::CacheTtl::FiveMinutes
                    && ctx.request_has_1h_marker
                    && cache_create_1h.unwrap_or(0) == 0
                    && cache_create_5m.unwrap_or(0) > 0
                    && cache_read == 0
                    && !ctx
                        .saw_1h_honored
                        .load(std::sync::atomic::Ordering::Relaxed)
                    && !ctx
                        .ttl_downgrade_notified
                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        "1h cache TTL not honored — check account/beta support (cache_ttl config)"
                    );
                    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Notice(
                        "⚠ 1h cache TTL not honored — check account/beta support (cache_ttl config)".to_string(),
                    )));
                }

                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                    // ═══ TELEMETRY: accumulate usage (message_delta carries final counts) ═══
                    if ctx.telemetry_level.enabled() {
                        state.telem_usage.input = input_t;
                        state.telem_usage.output = output_t;
                        state.telem_usage.cache_read = cache_read;
                        state.telem_usage.cache_write = cache_create;
                        state.telem_usage.cache_write_5m = cache_create_5m;
                        state.telem_usage.cache_write_1h = cache_create_1h;
                        state.telem_usage.compute_hit_pct();
                    }
                    // A stream carries "one or more message_delta events",
                    // and "the token counts shown in the usage field of the
                    // message_delta event are cumulative" (Anthropic,
                    // Streaming Messages). So this delta's counts replace any
                    // earlier delta's, and THE single authoritative Usage
                    // emission for the request is sent once, at message_stop
                    // or at the stream's end (`emit_final_usage`). Every
                    // downstream consumer is an accumulator, so an emission
                    // per delta double-bills.
                    state.final_usage = Some(FinalUsage {
                        input: input_t,
                        output: output_t,
                        cache_read,
                        cache_create,
                        cache_create_5m,
                        cache_create_1h,
                    });
                    state.trace_usage = Some(crate::runtime::trace::UsageMeta {
                        provenance: crate::runtime::trace::UsageProvenance::ProviderReported,
                        input_tokens: Some(input_t),
                        output_tokens: Some(output_t),
                        cache_read_tokens: Some(cache_read),
                        cache_write_tokens: Some(cache_create),
                    });
                    ctx.spend.observe(&super::child_spend::ReportedUsage {
                        input: Some(input_t),
                        output: Some(output_t),
                        cache_read: Some(cache_read),
                        cache_creation: Some(cache_create),
                        cache_creation_5m: cache_create_5m,
                        cache_creation_1h: cache_create_1h,
                    });
                }
            }
        }
        AnthropicEvent::MessageStart { message } => {
            state.wire_usage.start = usage_presence(
                serde_json::from_str::<super::sse_types::StartUsagePresence>(raw)
                    .ok()
                    .and_then(|s| s.message)
                    .and_then(|m| m.usage),
            );
            // ═══ TELEMETRY: capture msg_id (also the diagnostics chain) ═══
            if ctx.telemetry_level.enabled() || ctx.cache_diagnosis {
                if let Some(id) = message.id {
                    state.telem_msg_id = Some(id.into_owned());
                }
            }
            if ctx.cache_diagnosis {
                state.server_diagnosis =
                    super::cache_diagnosis::parse_server_diagnosis(message.diagnostics.as_ref());
            }
            if let Some(usage) = message.usage {
                // CAPTURE ONLY — no Usage emission from this arm. Live
                // message_delta repeats all aggregates and carries the final
                // output count, so emitting here too double-counts
                // input/cache_read/cache_create in every accumulating
                // consumer (TUI, engine, server, RPC, subagents). The one
                // authoritative event is sent at message_stop or at the
                // stream's end; for a stream that dies before its delta,
                // from these values.
                state.msg_start_input = Some(usage.input_tokens);
                state.msg_start_output = Some(usage.output_tokens);
                state.msg_start_cache_read = Some(usage.cache_read_input_tokens);
                state.msg_start_cache_create = Some(usage.cache_creation_input_tokens);
                state.msg_start_cache_5m = usage
                    .cache_creation
                    .as_ref()
                    .and_then(|cc| cc.ephemeral_5m_input_tokens);
                state.msg_start_cache_1h = usage
                    .cache_creation
                    .as_ref()
                    .and_then(|cc| cc.ephemeral_1h_input_tokens);
                // The spend guard retains the start's counts, so an attempt
                // that ends before its delta (a cancel, a dropped future)
                // still books what the provider reported. A start that
                // states only zeros is "never saw usage" here, as it is for
                // the root's residual below: nothing is retained.
                if usage.input_tokens > 0
                    || usage.output_tokens > 0
                    || usage.cache_read_input_tokens > 0
                    || usage.cache_creation_input_tokens > 0
                {
                    ctx.spend.observe(&super::child_spend::ReportedUsage {
                        input: state.msg_start_input,
                        output: state.msg_start_output,
                        cache_read: state.msg_start_cache_read,
                        cache_creation: state.msg_start_cache_create,
                        cache_creation_5m: state.msg_start_cache_5m,
                        cache_creation_1h: state.msg_start_cache_1h,
                    });
                }
                // Latch 1h-honored HERE, not only in the delta arm: a stream
                // that dies between start and delta on turn 1 must not lose
                // the latch — that would arm a false downgrade notice on a
                // later healthy turn (1h == 0 because the prefix is cached).
                if state.msg_start_cache_1h.unwrap_or(0) > 0 {
                    ctx.saw_1h_honored
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        // The stream's last event: the request's one Usage emission.
        AnthropicEvent::MessageStop => emit_final_usage(state, ctx),
        AnthropicEvent::Error { error } => {
            // Anthropic delivers some failures (overloaded, context-overflow)
            // as an in-stream `error` event under a 200 response. Capture +
            // CLASSIFY it so the stream either retries (transient) or surfaces
            // a visible, accurate error — never the silent "stopping" swallow
            // (task #130). First error wins — later frames don't clobber it.
            if state.stream_error.is_none() {
                let (kind, _body) = match &error {
                    Some(e) => (
                        e.error_type.as_deref().unwrap_or("error"),
                        e.message.as_deref(),
                    ),
                    None => ("error", None),
                };
                let retryable = StreamError::is_retryable_type(kind);
                // SECURITY (spec §5.1): the frame — error type AND message —
                // is untrusted; a hostile provider can echo the entire request
                // through it. Surface only a vetted static class label; never
                // the message or the raw frame (bounded truncation is not
                // redaction).
                let vetted =
                    crate::core::error::sanitize_error_type(kind).unwrap_or("unrecognized_error");
                let message = format!(
                    "API stream error ({vetted}). Provider error details withheld — they can echo request content."
                );
                tracing::warn!(
                    retryable,
                    error_type = vetted,
                    frame_bytes = raw.len(),
                    "SSE error event (payload withheld — untrusted)"
                );
                state.stream_error = Some(StreamError {
                    error_type: Some(kind.to_string()),
                    message,
                    retryable,
                });
            }
        }
        AnthropicEvent::Unknown => {
            // #[serde(other)] discarded the tag. The raw line is untrusted
            // provider output — log only its size, never its content
            // (spec §5.1: no response-derived text in logs at any level).
            tracing::trace!(
                payload_bytes = raw.len(),
                "Unknown SSE event type (payload withheld — untrusted)"
            );
        }
    }
}

/// End a ledgered attempt that failed (§6) by the transports' typed
/// classification: released only when it provably cost nothing; otherwise
/// its liability stays open. The conversation client follows redirects, so
/// this route is `Unverified`: no error status releases an attempt, and the
/// attempt is reserved after every local check, so only the send can fail.
fn settle_failed_attempt(
    report: crate::tools::request_ledger::AttemptReport,
    stage: super::api_sync::FailureStage,
) {
    match super::api_sync::failure_spend(super::api_sync::SettlementRoute::Unverified, stage) {
        super::api_sync::Spend::NotDispatched => {
            report.release();
        }
        _ => {
            report.finish();
        }
    }
}

/// An attempt's spend as its final report states it (§6): output on a
/// `message_delta` (the start's is a running count), input and cache reads
/// and writes on the delta or else on `message_start`, explicit zeros kept.
/// `None` (the attempt stays open) when any of them was not reported. Read
/// from the presence-kept capture, never from the display emission, which
/// zero-fills absent counts and falls back to the start's for a zero.
fn settled_usage(wire: &super::warm_summary::WireUsage) -> Option<agent_core::UsageCounts> {
    let usage = wire.complete()?;
    Some(agent_core::UsageCounts {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_input_tokens: usage.cache_read,
        cache_creation_input_tokens: usage.cache_write,
        cache_creation_5m: usage.cache_write_5m,
        cache_creation_1h: usage.cache_write_1h,
    })
}

/// The request's one Usage emission. It sends the last counted
/// message_delta's counts; a stream that ended before any counted delta (a
/// cancel, a transport death after message_start) sends what message_start
/// reported, since the API already processed the input. Called at
/// message_stop and at every stream end, so a stream cut after its delta
/// still bills it. Idempotent via `usage_emitted`: exactly one emission per
/// request in all paths.
fn emit_final_usage(state: &mut ParseState, ctx: &EventCtx) {
    if state.usage_emitted {
        return;
    }
    if let Some(usage) = state.final_usage {
        // The delta arm already set the telemetry, the trace usage and the
        // spend observation from these counts.
        state.usage_emitted = true;
        tracing::debug!(
            "Token Usage: {} input | {} output | {} cache_read | {} cache_create",
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_create
        );
        send_usage(ctx, usage);
        return;
    }
    let usage = FinalUsage {
        input: state.msg_start_input.unwrap_or(0),
        output: state.msg_start_output.unwrap_or(0),
        cache_read: state.msg_start_cache_read.unwrap_or(0),
        cache_create: state.msg_start_cache_create.unwrap_or(0),
        cache_create_5m: state.msg_start_cache_5m,
        cache_create_1h: state.msg_start_cache_1h,
    };
    if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 && usage.cache_create == 0
    {
        return; // never saw usage (or all-zero) — nothing to bill
    }
    tracing::debug!("Token Usage (residual, dead stream): {} input | {} output | {} cache_read | {} cache_create", usage.input, usage.output, usage.cache_read, usage.cache_create);
    if ctx.telemetry_level.enabled() {
        state.telem_usage.input = usage.input;
        state.telem_usage.output = usage.output;
        state.telem_usage.cache_read = usage.cache_read;
        state.telem_usage.cache_write = usage.cache_create;
        state.telem_usage.cache_write_5m = usage.cache_create_5m;
        state.telem_usage.cache_write_1h = usage.cache_create_1h;
        state.telem_usage.compute_hit_pct();
    }
    state.usage_emitted = true;
    state.trace_usage = Some(crate::runtime::trace::UsageMeta {
        provenance: crate::runtime::trace::UsageProvenance::ProviderReported,
        input_tokens: Some(usage.input),
        output_tokens: Some(usage.output),
        cache_read_tokens: Some(usage.cache_read),
        cache_write_tokens: Some(usage.cache_create),
    });
    ctx.spend.observe(&super::child_spend::ReportedUsage {
        input: Some(usage.input),
        output: Some(usage.output),
        cache_read: Some(usage.cache_read),
        cache_creation: Some(usage.cache_create),
        cache_creation_5m: usage.cache_create_5m,
        cache_creation_1h: usage.cache_create_1h,
    });
    send_usage(ctx, usage);
}

/// The emission itself: the usage log, the root's priced booking and the
/// one Usage event.
fn send_usage(ctx: &EventCtx, usage: FinalUsage) {
    HelperMethods::log_usage(usage.input, usage.cache_read, usage.cache_create, usage.output);
    if let Some(counters) = &ctx.usage_counters {
        counters.record_priced(
            ctx.model,
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_create,
            usage.cache_create_5m,
            usage.cache_create_1h,
        );
    }
    // A ledgered attempt's usage goes out as the attempt's (§6), never also
    // as the plain usage; without a ledger this is exactly the plain `Usage`.
    let counts = agent_core::UsageCounts {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_input_tokens: usage.cache_read,
        cache_creation_input_tokens: usage.cache_create,
        cache_creation_5m: usage.cache_create_5m,
        cache_creation_1h: usage.cache_create_1h,
    };
    let _ = ctx.tx.send(StreamEvent::Session(
        crate::tools::request_ledger::usage_event(ctx.usage_request.as_ref(), counts, None),
    ));
}

/// Decide the fate of a finished stream. Pure (no I/O) so it is unit tested
/// directly — the silent-stop fix (task #130) lives here.
///
///   * An in-stream `error` event → `Retry` (transient class) or `Fail`
///     (terminal class, e.g. `request_too_large` context overflow). A user
///     cancellation downgrades any error to a clean (empty) `Done`.
///   * stop_reason=refusal → `Refusal`: ALL accumulated content discarded,
///     caller retries up to `refusal_retries` budget with a short delay.
///     Refusal on a cancelled stream downgrades to `Done` (don't fight cancel).
///   * A degenerate empty stream (no content AND no stop_reason) that was NOT
///     cancelled → `Fail`. An empty streamed response with no stop reason is
///     never valid; swallowing it as an empty assistant turn is exactly what
///     made the agent silently stop mid-flow.
///   * Otherwise → `Done` with the assistant content envelope. A cancelled
///     stream legitimately yields empty content, so it passes through.
const OAUTH_OVERLOAD_RETRIES: u32 = 10;
const OAUTH_OVERLOAD_BASE_DELAY_MS: u64 = 500;
const GENERIC_STREAM_BASE_DELAY_MS: u64 = 1000;

fn stream_retry_policy(
    auth_type: &str,
    error_type: Option<&str>,
    configured_retries: u32,
) -> (u32, u64) {
    if auth_type == "oauth" && error_type == Some("overloaded_error") {
        (OAUTH_OVERLOAD_RETRIES, OAUTH_OVERLOAD_BASE_DELAY_MS)
    } else {
        (configured_retries, GENERIC_STREAM_BASE_DELAY_MS)
    }
}

fn classify_stream_outcome(
    stream_error: Option<StreamError>,
    content: Vec<Value>,
    has_stop_reason: bool,
    stop_reason_is_refusal: bool,
    cancelled: bool,
) -> StreamOutcome {
    if let Some(e) = stream_error {
        if cancelled {
            // The user pulled the plug — don't surface a scary error or burn a
            // retry. Return whatever (likely empty) content we have.
            return StreamOutcome::Done(json!({ "content": content }));
        }
        return if e.retryable {
            StreamOutcome::Retry(e)
        } else {
            StreamOutcome::Fail(e.message)
        };
    }
    // Refusal: model ended the stream cleanly but declined the request.
    // DISCARD all partial content — nothing partial may enter history.
    if stop_reason_is_refusal && !cancelled {
        return StreamOutcome::Refusal;
    }
    if !cancelled && content.is_empty() && !has_stop_reason {
        return StreamOutcome::Fail(
            "empty response from API — the model returned no content and no stop \
             reason. This usually means the context window was exceeded or the \
             API was overloaded. Try /compact or start a fresh session."
                .to_string(),
        );
    }
    StreamOutcome::Done(json!({ "content": content }))
}

/// How a streamed 200 failed, as a static label: the vetted type of an
/// in-stream error frame (else `unrecognized_error`), a broken body
/// (`transport`), a body that went quiet (`idle_timeout`), or no content and
/// no stop reason (`empty_response`).
fn stream_failure_label(
    stream_error: Option<&StreamError>,
    transport_error: Option<&RuntimeError>,
) -> &'static str {
    match (stream_error, transport_error) {
        (_, Some(RuntimeError::NetworkTimeout(_))) => "idle_timeout",
        (_, Some(_)) => "transport",
        (Some(error), None) => error
            .error_type
            .as_deref()
            .and_then(crate::core::error::sanitize_error_type)
            .unwrap_or("unrecognized_error"),
        (None, None) => "empty_response",
    }
}

/// How a send that may have reached the provider failed without a
/// response, as a static label.
fn send_failure_label(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::NetworkTimeout(_) => "header_timeout",
        _ => "transport",
    }
}

/// The label a keep-warm request's transport stamps when it takes its
/// refusal exit (stop reason `refusal`, no in-stream error frame before the
/// stream ended, refusal budget spent). The warm summary classifies a
/// refusal on this exit, not on the recorded stop reason alone: an error
/// frame after a refusal stop takes precedence and is stamped with its own
/// label.
pub(crate) const REFUSAL_EXIT: &str = "refusal";

/// Record `label` as how a keep-warm request (ping or warm summary) failed.
fn stamp_failure(options: &ApiOptions, label: &'static str) {
    if let Some(keepwarm) = &options.keepwarm {
        *keepwarm
            .failure_kind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(label);
    }
}

/// Extensible — new flags go here instead of adding parameters to 4 signatures.
#[derive(Debug, Clone, Default)]
pub struct ApiOptions {
    /// Host-set turn provenance; never inferred from messages or event text.
    pub event_triggered: bool,
    /// Immutable host binding for extension-provider interior tool execution.
    /// `None` is only for internal/test callers without a runtime capability.
    pub memory_backend: Option<crate::memory_backend::MemoryBinding>,
    /// The session's write-sandbox binding (`crate::sandbox`): bash/PTY
    /// commands an extension provider's interior tool loop runs are
    /// sandboxed like the session's own. `None`: no sandbox (internal and
    /// test callers without a session), exactly as before.
    pub sandbox: Option<std::sync::Arc<crate::sandbox::SandboxBinding>>,
    /// Opt into the 1M context window beta header.
    pub use_1m_context: bool,
    /// Configured prompt-cache TTL (spec: cache-ttl). Resolved to a wire
    /// [`crate::core::config::CacheTtl`] per request once the credential
    /// kind is known (`auto`: 1h on OAuth, 5m on an API key). A resolved
    /// `FiveMinutes` emits payloads byte-identical to the pre-feature release.
    pub cache_ttl: crate::core::config::CacheTtlSetting,
    /// One-time-per-session latch for the silent-downgrade notice (1h
    /// requested, only 5m honored). Shared via Arc so every request in the
    /// session sees the same latch; the configured mode is NEVER auto-flipped.
    pub ttl_downgrade_notified: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Session-scoped "1h was honored at least once" latch (spec §3.4.1).
    /// Set on any response with a nonzero 1h cache-write bucket; suppresses
    /// the downgrade notice on healthy Hybrid turns where the 1h prefix is
    /// already cached (1h == 0, 5m > 0). Shared via Arc like the notice latch.
    pub saw_1h_honored: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Credential source for provider auth (Local/Remote). Carried here so the
    /// OpenAI/codex routing path can resolve tokens through the broker on
    /// Remote, same as the Anthropic path. (#158 C4)
    pub credential_source: crate::auth::CredentialSource,
    /// Shared broker token cache (Remote only).
    pub token_cache: crate::auth::TokenCache,
    /// Override the Anthropic API base URL. In production always `None`, which
    /// causes the code to read `SYNAPS_ANTHROPIC_BASE_URL` from the environment
    /// and then fall back to `"https://api.anthropic.com"`.
    /// Set by tests to point at a local mock server without touching env vars.
    #[doc(hidden)]
    pub anthropic_base_url: Option<String>,
    /// Dedicated lazy pool: upload and body idle deadlines are independent.
    pub(super) anthropic_stream_client: Arc<std::sync::OnceLock<Client>>,
    /// gzip `Content-Encoding` policy for the request body (see
    /// `request_encoding`). The default compresses large bodies sent straight
    /// to api.anthropic.com and shares the process-wide refusal latch.
    pub(super) request_compression: super::request_encoding::RequestCompression,
    /// Execution authority produced by Runtime preflight. Transports must not
    /// recreate authority for Max/UltraCode from assumed prerequisites.
    pub anthropic_execution_plan: Option<crate::runtime::openai::catalog::AnthropicExecutionPlan>,
    /// Foreground/worker/internal request role.
    pub codex_request_role: crate::runtime::openai::catalog::CodexRequestRole,
    /// The Anthropic request's `thinking.display`
    /// ([`crate::Runtime::wire_thinking_display`]). The default (summarized)
    /// keeps every body byte-identical to before the setting existed.
    pub thinking_display: crate::config::ThinkingDisplay,
    /// Set by the stream loop (and the keep-warm ping) when request-local
    /// image retention was already applied to the messages this request is
    /// given (worker and routed budgets, `image_retention::round_images`):
    /// the `(message, block)` position of that retention's frontier in those
    /// messages. The native transport anchors a cache breakpoint ahead of the
    /// earlier of this and its own replay's frontier. `None` otherwise.
    pub retention_frontier: Option<(usize, usize)>,
    /// Trace emission seam (Task 8). Default is a disabled context (no-op
    /// sink, zero request-path work); tests install a collecting sink and
    /// production installs the Task 11 writer-backed sink via the Runtime.
    pub trace: crate::runtime::trace::TraceContext,
    /// The correlated IDs reserved for this provider request. Set before the
    /// transport starts and consumed by both RequestTracer and the stream's
    /// tool lifecycle recorder.
    pub request_correlation: Option<crate::runtime::trace::RequestCorrelation>,
    /// Journal identity and the runtime's effective window. The window also
    /// sizes the request's output cap (`ApiOptions::effective_context_window`,
    /// B32), so the wire's `max_tokens` is what the context assessment
    /// reserved; `None` falls back to the 1M flag, then the model's window.
    pub diagnostic_session_id: Option<String>,
    pub diagnostic_context_window: Option<u64>,
    /// Internal sync routes can suppress high-volume display-only deltas at
    /// the producer boundary; final text/tool results are still returned in
    /// the normal response value.
    #[doc(hidden)]
    pub suppress_stream_deltas: bool,
    /// Bounded background writer for legacy telemetry records (Task 11).
    /// `None` (default) drops records; production sets the Runtime's shared
    /// session writer whenever the telemetry level is Basic/Full. Enqueue
    /// only — never synchronous file I/O on the request path.
    pub telemetry: Option<crate::runtime::telemetry::TelemetryWriter>,
    /// Runtime-scoped tool-session identity (Task 16). Scopes the execution
    /// gate for extension-provider interior tool loops reached through
    /// `try_route`. Stream turns (and `run_single`) thread the Runtime's
    /// host session; `None` (internal helpers, tests) makes the extension
    /// route fail closed to a locally minted identity with the identical
    /// default-core policy — never to ungated execution.
    pub tool_session_id: Option<crate::tools::activation::SessionId>,
    /// The RETAINED per-stream session tool set handle (Task 17). Stream
    /// turns thread the same shared set the execution gate authorizes
    /// against, so extension-provider interior tool loops observe the
    /// identical set/generation — including exact activations. `None`
    /// (internal helpers, non-stream paths, tests) makes the extension
    /// route fall back to a locally minted default-core set; a STALE
    /// retained set is denied there, never silently replaced.
    pub session_tool_set: Option<crate::tools::activation::SharedSessionToolSet>,
    /// Optional provider-neutral Task 18 schema projection for this request.
    /// `None` preserves the legacy registry-cached Arc and therefore the
    /// flag-off request bytes exactly. Stream rounds set `Some` only when the
    /// progressive-disclosure flag is enabled.
    pub request_tools_schema: Option<std::sync::Arc<Vec<Value>>>,
    /// Anthropic deferred-loading split for this request (progressive
    /// disclosure + `tools.deferred_loading`). `None` keeps the plain tools
    /// array. Only the native Anthropic transport reads it, and only for a
    /// model with tool search; every other transport keeps using
    /// `request_tools_schema`.
    pub anthropic_tool_split: Option<std::sync::Arc<crate::tools::DeferredToolSplit>>,
    /// Optional shared turn-budget usage counters (Task 23). Updated at the
    /// single authoritative Usage emission per request; the stream loop
    /// reads them for the optional token/cost budget dimensions. `None`
    /// (default, non-stream callers) records nothing.
    pub usage_counters: Option<std::sync::Arc<crate::runtime::budget::UsageCounters>>,
    /// "Last accepted Anthropic request" slot (task #444). Stamped with the
    /// send time of every attempt that gets a 2xx — the moment the prefix's
    /// cache entries were written or refreshed. `None` (default) records
    /// nothing; the foreground turn stream and keep-warm pings set it.
    pub cache_anchor: Option<std::sync::Arc<super::keepwarm::CacheAnchorCell>>,
    /// Prompt-cache keep-warm ping (task #444). `None` on every real turn,
    /// which keeps turn requests byte-identical to the pre-feature release.
    /// When set (native Anthropic transport only — any other route fails
    /// closed before a request is built): the conversational cache marker is
    /// stamped on the LAST HISTORY message and the trailing ping message
    /// rides after it unmarked, `max_tokens` is cut to the thinking mode's
    /// minimum, the prefix is fingerprinted (and checked, see
    /// [`KeepWarmMode`]), every retry budget is zero, and the telemetry
    /// record carries `"keepwarm": true`.
    pub keepwarm: Option<KeepWarmRequest>,
    /// Anthropic cache diagnostics chain (`cache_diagnostics = true`).
    /// `None` (default) keeps the wire byte-identical: no header, no
    /// `diagnostics` field. Set on the foreground turn stream (which threads
    /// `previous_message_id`) and on keep-warm pings (header only, so the
    /// beta set matches the turns they keep warm).
    pub cache_diagnosis: Option<super::cache_diagnosis::CacheDiagnosis>,
    /// The context window this request belongs to after an automatic
    /// rollover (worker or foreground), for the telemetry record's
    /// `rollover_window` / `rollover` fields. `None` (default) before any
    /// rollover and on every non-turn request: records stay unchanged.
    pub context_rollover: Option<super::telemetry::RolloverStamp>,
    /// Evidence slot of a recorded conversation request (warm rolling
    /// summary): the native Anthropic transport writes the built prefix's
    /// fingerprint, the authorization's opaque credential scope, the accepted
    /// attempt's send time and the completed response's cache usage here.
    /// Ignored on keep-warm requests. Never changes the request.
    pub prefix_tap: Option<std::sync::Arc<super::warm_summary::PrefixTap>>,
    /// A rollover summary's snapshot gate: the bounded native Anthropic
    /// request is not sent once it closes (checked right before the send).
    pub snapshot_gate: Option<super::warm_summary::SnapshotGate>,
    /// A worker runtime's spend tap (child spend): each paid attempt books
    /// the usage its provider reported through it, once, at the attempt's
    /// end. `None` (default) books nothing: the root session's own requests,
    /// summaries (their settlement books), keep-warm pings and every host
    /// without a sink.
    pub spend_tap: Option<super::child_spend::SpendTap>,
    /// A v2 worker run's request ledger (§6; slice 6, dark), attributed to
    /// this request's model and purpose. `None` (default: every session,
    /// keep-warm ping, one-shot and v1 worker) changes nothing about the
    /// request or its events.
    pub(crate) request_ledger: Option<crate::tools::RequestLedgerHandle>,
    /// The turn's capacity wait (config `capacity_wait`): a window-limit 429
    /// with a long reset and no other account waits in the request for the
    /// reset instead of ending the turn (see `capacity_wait`). `None`
    /// (default): never waits, as for keep-warm pings, summaries and
    /// compaction.
    pub(crate) capacity_wait: Option<super::capacity_wait::CapacityWaitPolicy>,
    /// The `max_tokens` a native Anthropic conversation request was sized to
    /// and admitted on (R1, `stream::size_request`): the transport lowers the
    /// body's static cap to it, so the wire sends exactly the output the
    /// pre-request assessment reserved. Never raises the cap; ignored by
    /// keep-warm pings and warm summaries (their own ceilings) and by every
    /// other transport. `None` (default): the static cap.
    pub(crate) request_output_tokens: Option<u64>,
    /// The stream turn's long-context refusal slot: a request that carried
    /// the 1M beta and was refused with a long-context class (see
    /// [`classify_long_context_refusal`](crate::core::error::classify_long_context_refusal))
    /// is reported here (status and class only, never the body) so the turn
    /// can fall back to the native window. The error itself is unchanged.
    /// `None` (default): nothing is classified, as for one-shots, keep-warm
    /// pings, summaries and compaction.
    pub(crate) long_context_refusal: Option<super::native_window::LongContextSlot>,
}

impl ApiOptions {
    /// The window that sizes this request's output cap
    /// ([`HelperMethods::max_tokens_for_request`](super::helpers::HelperMethods::max_tokens_for_request)):
    /// the runtime's effective window when the caller set it, otherwise 1M
    /// (clamped to `model`'s ceiling) when the 1M context was asked for,
    /// otherwise `model`'s native window.
    pub(crate) fn effective_context_window(&self, model: &str) -> u64 {
        self.diagnostic_context_window.unwrap_or_else(|| {
            if self.use_1m_context {
                crate::core::models::effective_context_window(model, Some(1_000_000))
            } else {
                crate::core::models::context_window_for_model(model)
            }
        })
    }
}

/// What a keep-warm request does once its body is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepWarmMode {
    /// Return `{"keepwarm_prefix": <fingerprint>, "keepwarm_max_tokens": n}`;
    /// send nothing.
    Probe,
    /// Send only if the built prefix still hashes to `expect`; otherwise
    /// return `{"keepwarm_prefix": <actual>}` without sending.
    Send { expect: u64 },
    /// Send unconditionally: the messages before the trailing ping are the
    /// exact request messages of the provider round whose tool calls are
    /// still running (`runtime::tool_keepwarm`), re-sent with that round's
    /// own tools, system, thinking and options, so the prefix is that
    /// request's by construction and there is nothing to fingerprint.
    Replay,
}

/// What a request with a trailing request-local message is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPurpose {
    /// Keep-warm ping: output cut to the thinking mode's minimum.
    Ping,
    /// Warm rolling summary: total output (thinking included) capped at
    /// `max_tokens`, which must exceed any thinking budget; sent only while
    /// the authorization and the resolved endpoint still have the recorded
    /// scopes; never re-sent with a refetched credential.
    Summary {
        max_tokens: u64,
        credential: u64,
        endpoint: u64,
        /// The recorded request's prefix digest
        /// ([`super::request::RequestBody::prefix_digest_over`]); the
        /// summary is sent only when the rebuilt prefix has the same one.
        digest: [u8; 32],
        /// Written to this request's telemetry record as
        /// `rollover_summary`, matching the rollover event's `summary_id`.
        correlation: u64,
    },
}

/// Wire knobs for a keep-warm request (see [`ApiOptions::keepwarm`]).
#[derive(Debug, Clone)]
pub struct KeepWarmRequest {
    pub mode: KeepWarmMode,
    pub purpose: TailPurpose,
    /// The response's stop reason, once one arrived.
    pub stop_reason: std::sync::Arc<std::sync::Mutex<Option<crate::runtime::trace::StopReason>>>,
    /// The response's usage with presence kept, once its stream ended.
    pub usage: std::sync::Arc<std::sync::Mutex<super::warm_summary::WireUsage>>,
    /// Last HTTP status the transport saw (0 = none), so the caller can tell
    /// a 4xx (stop) from a 5xx/429 (bounded retry).
    pub status: std::sync::Arc<std::sync::atomic::AtomicU16>,
    /// Published once the request is actually dispatched: when the HTTP
    /// client, over an established connection with the head written, pulls
    /// the body (see [`gated_body`]), or when `send()` returns a response.
    /// From then on the ping may be billed even if it is aborted. A refused
    /// connection never publishes it; a caller that withdraws it first
    /// keeps the body from ever being produced.
    pub sent: std::sync::Arc<super::keepwarm::Publication>,
    /// Set when `send()` failed to connect (no HTTP status, nothing sent).
    pub connect_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// How the request failed once it may have reached the provider, as a
    /// static label (never provider text), stamped at the error exit: see
    /// [`stream_failure_label`], [`send_failure_label`] and, for an HTTP
    /// error status, the vetted type of its body.
    pub failure_kind: std::sync::Arc<std::sync::Mutex<Option<&'static str>>>,
    /// Warm summary: the right to send, checked at the dispatch boundary
    /// after every await of request preparation; a revoked lease returns
    /// `{"keepwarm_refused":"revoked"}` with nothing sent.
    pub lease: Option<super::warm_summary::WarmLease>,
}

#[cfg(test)]
thread_local! {
    /// Transfer a real local Codex transport result into the runtime completion
    /// path without routing a test through production credential discovery.
    pub(crate) static EVENT_NOOP_RESPONSE_FOR_TEST: std::cell::RefCell<Option<(Value, Vec<StreamEvent>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Live-acceptance seam (test builds only): when set, a warm summary's
    /// body gets this salt in front of its last system text block AFTER the
    /// prefix checks, so the provider sees a prefix it has not cached (a
    /// controlled cache miss). Set on the test's runtime thread only.
    pub(crate) static WARM_BODY_SALT_FOR_TEST: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn salt_summary_body_for_test(body: bytes::Bytes, keepwarm: &Option<KeepWarmRequest>) -> bytes::Bytes {
    let Some(salt) = WARM_BODY_SALT_FOR_TEST.with(std::cell::Cell::get) else {
        return body;
    };
    if !matches!(
        keepwarm,
        Some(KeepWarmRequest {
            purpose: TailPurpose::Summary { .. },
            ..
        })
    ) {
        return body;
    }
    let mut value: Value = serde_json::from_slice(&body).expect("a JSON request body");
    let salted = |text: &str| Value::String(format!("[cache salt {salt:016x}]\n{text}"));
    match &mut value["system"] {
        Value::String(text) => {
            let s = salted(text);
            value["system"] = s;
        }
        Value::Array(blocks) => {
            let block = blocks
                .iter_mut()
                .rev()
                .find(|b| b["text"].is_string())
                .expect("a system text block");
            let s = salted(block["text"].as_str().unwrap_or_default());
            block["text"] = s;
        }
        _ => panic!("a warm summary body has a system prompt"),
    }
    serde_json::to_vec(&value).expect("serializable").into()
}

/// A request body (keep-warm ping, warm or screened rollover summary) that
/// flips `sent` when the HTTP client first pulls it: hyper only polls a
/// request body once the connection is established and the request head
/// is written, so a request that never connected is never marked sent.
/// `Content-Length` is set explicitly so the wire framing matches a
/// buffered body's (no chunked encoding).
///
/// A summary's `gate` (its lease or snapshot and, on a worker run's clock,
/// the run deadline, read last) is checked right before the publication
/// below, before the first (only) body chunk is yielded: the last check that
/// authorizes publishing it, the request's linearization point. Closed there, the body
/// is not produced and the request fails before any body byte is written
/// (the head may already be on the wire, a request the provider never
/// receives in full); after it, the bytes cannot be retracted and the
/// request runs to its end.
///
/// `publication` is published here, by compare-exchange, right before the
/// chunk is yielded (callers also mark it answered when `send()` returns any
/// response). The client may still hold this body on its connection task
/// after the caller dropped the request; a caller that withdrew
/// `publication` first gets no body byte out, here or later.
pub(super) fn gated_body(
    req: reqwest::RequestBuilder,
    body: bytes::Bytes,
    publication: &std::sync::Arc<super::keepwarm::Publication>,
    gate: Option<super::warm_summary::BodyGate>,
) -> reqwest::RequestBuilder {
    let publication = std::sync::Arc::clone(publication);
    let len = body.len();
    let stream = futures::stream::once(async move {
        #[cfg(test)]
        publication.pause(super::keepwarm::SeamPoint::BeforePublish);
        if gate.is_some_and(|gate| !gate.open()) {
            return Err(std::io::Error::other(
                "rollover summary withdrawn before its body",
            ));
        }
        if !publication.publish() {
            return Err(std::io::Error::other("request withdrawn before its body"));
        }
        #[cfg(test)]
        publication.pause(super::keepwarm::SeamPoint::BeforeYield);
        Ok::<bytes::Bytes, std::io::Error>(body)
    });
    req.header(reqwest::header::CONTENT_LENGTH, len)
        .body(reqwest::Body::wrap_stream(stream))
}

/// Validate a provider-assigned request ID from response headers into a
/// bounded [`trace::TraceId`]. Invalid or hostile values are omitted —
/// never copied raw into a trace record.
pub(super) fn provider_request_id_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<crate::runtime::trace::TraceId> {
    crate::runtime::telemetry::request_id_from_headers(headers)
        .and_then(|s| crate::runtime::trace::TraceId::new(s).ok())
}

/// Humanize a terminal (not retried) HTTP failure AND log it. Without the log
/// line the only record of a 400 is the TUI banner, so a session that fails
/// every request leaves nothing in `synaps.log` to diagnose from.
///
/// SECURITY (spec §5.1): `body` is untrusted and can echo the request. Only
/// the numeric status, the provider request id (validated `TraceId`) and OUR
/// humanized text, built from vetted static strings, are logged. Never the body.
pub(super) fn terminal_http_error(
    status: u16,
    body: &str,
    reset_hint: Option<&str>,
    request_id: Option<&crate::runtime::trace::TraceId>,
) -> String {
    let message = crate::core::error::humanize_api_error_with_reset(status, body, reset_hint);
    tracing::warn!(
        status,
        request_id = request_id.map_or("-", |id| id.as_str()),
        error = %message,
        "API request failed (not retried)"
    );
    message
}

/// Begin an Anthropic request tracer (Task 8). Returns `None` when tracing
/// is disabled or any structural identity is unrepresentable — tracing can
/// never fail the request. `body_bytes` MUST be the exact JSON payload handed
/// to reqwest, before any `Content-Encoding` (the server decodes gzip before
/// reading it): it is the sole source of the wire length + digest.
#[allow(clippy::too_many_arguments)]
pub(super) async fn begin_anthropic_tracer(
    options: &ApiOptions,
    cache_ttl: crate::core::config::CacheTtl,
    url: &str,
    bare_model: &str,
    body_bytes: &[u8],
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
    has_tool_marker: bool,
    has_system_marker: bool,
    translation: &crate::runtime::transport::report::TranslationReport,
) -> Option<crate::runtime::trace::RequestTracer> {
    use crate::runtime::trace as tr;
    if !options.trace.enabled() {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let host = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let endpoint = tr::EndpointMeta::new(host, parsed.path()).ok()?;
    let model =
        agent_core::prompt::QualifiedModelId::parse(format!("anthropic/{bare_model}")).ok()?;
    // Lazy, spawn_blocking-backed key load; `None` degrades the record to
    // digest-free sections and is counted, never surfaced as an error.
    let key = options.trace.digest_key().await;
    let prefix_ttl = match cache_ttl {
        crate::core::config::CacheTtl::FiveMinutes => Some(tr::CacheTtlClass::FiveMinutes),
        crate::core::config::CacheTtl::OneHour | crate::core::config::CacheTtl::Hybrid => {
            Some(tr::CacheTtlClass::OneHour)
        }
    };
    let structure = tr::anthropic::anthropic_request_structure(
        key.as_deref(),
        body_bytes,
        messages,
        system_prompt,
        tools_schema,
        prefix_ttl,
        has_tool_marker,
        has_system_marker,
        translation.clone().into_losses(),
        Some(options.trace.cache_snapshots()),
    );
    let tracer = tr::RequestTracer::begin(
        &options.trace,
        options.request_correlation.clone(),
        model,
        tr::TransportKind::AnthropicMessages,
        endpoint,
        structure,
    );
    // One-shot explicit content capture (`/trace next content`): a no-op in
    // every context that does not carry the arm. Body bytes only — headers
    // and credentials structurally never reach this seam.
    if let Some(tracer) = &tracer {
        options
            .trace
            .capture_request_content(tracer.request_id(), body_bytes);
    }
    tracer
}

/// Cancellation-first wait for request preparation, dispatch and retry delays.
/// The future is not polled when cancellation is already observable.
pub(super) async fn await_or_cancel<F: std::future::Future>(
    cancel: &CancellationToken,
    future: F,
) -> Result<F::Output> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(RuntimeError::Canceled),
        output = future => Ok(output),
    }
}

pub(super) struct ApiMethods;

impl ApiMethods {
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(super) async fn call_api_stream(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        tx: mpsc::UnboundedSender<StreamEvent>,
        max_retries: u32,
        refusal_retries: u32,
        options: &ApiOptions,
        telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    ) -> Result<Value> {
        Self::call_api_stream_inner(
            auth,
            client,
            model,
            tools,
            system_prompt,
            thinking_budget,
            reasoning_level,
            messages,
            tx,
            &CancellationToken::new(),
            max_retries,
            refusal_retries,
            options,
            telemetry_level,
        )
        .await
    }

    /// Static inner version — used by both `call_api_stream` (instance) and
    /// `run_stream_internal` (spawned task) so there's one implementation.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::collapsible_match)]
    pub(super) async fn call_api_stream_inner(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: &CancellationToken,
        max_retries: u32,
        refusal_retries: u32,
        options: &ApiOptions,
        telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    ) -> Result<Value> {
        // Do not route, resolve credentials or prepare a request for a revoked turn.
        if cancel.is_cancelled() {
            return Err(RuntimeError::Canceled);
        }
        #[cfg(test)]
        if let Some((response, events)) =
            EVENT_NOOP_RESPONSE_FOR_TEST.with(|slot| slot.borrow_mut().take())
        {
            for event in events {
                let _ = tx.send(event);
            }
            return Ok(response);
        }
        // Keep-warm pings exist only for the native Anthropic transport: every
        // other route would ignore the output cap and the marker placement.
        if options.keepwarm.is_some() && !super::auth::model_is_anthropic(model) {
            return Err(RuntimeError::Config(
                "cache keep-warm ping requires the native Anthropic transport".into(),
            ));
        }
        super::attachments::validate_history_for_request(model, messages).map_err(RuntimeError::Config)?;
        // Host `_synaps_*` keys never reach a provider. The native Anthropic
        // body drops them while serializing (`WireMessages`, no history
        // copy); every other transport strips at its own entry.
        // One provider-neutral schema source for every transport. The opt-in
        // progressive path supplies a per-round session projection; flag-off
        // keeps cloning the registry's existing cached Arc byte-for-byte.
        let tools_schema = options
            .request_tools_schema
            .clone()
            .unwrap_or_else(|| tools.tools_schema());
        // §6 backstop: a ledgered request (a v2 worker run) goes only to a
        // transport that books its attempts; any other route refuses it
        // before anything is sent.
        if options.request_ledger.is_some() && !crate::runtime::openai::books_requests(model) {
            return Err(RuntimeError::Config(
                crate::runtime::openai::unbooked_route(model),
            ));
        }
        // Cloud models always dispatch through the typed credential broker.
        // Text-only pre-flight, invocation, cancellation, and Task 10B trace
        // wiring live in `runtime::cloud_invoke` (extracted for testability).
        let (cloud_model, cloud_context) = crate::auth::cloud::split_model_route(model);
        if let Some((provider_key, _)) = cloud_model.split_once('/') {
            if let Ok(provider) = provider_key.parse::<crate::auth::CloudProviderId>() {
                if cancel.is_cancelled() {
                    return Err(RuntimeError::Canceled);
                }
                let wire_messages = super::continuation::wire_messages(messages);
                let messages = wire_messages.as_deref().unwrap_or(messages);
                let has_tools = !tools_schema.is_empty();
                return with_usage_metering(model, options.usage_counters.clone(), &tx, |tx| async move {
                    crate::runtime::cloud_invoke::cloud_invoke_stream(
                        provider,
                        model,
                        cloud_model,
                        cloud_context,
                        has_tools,
                        || {
                            crate::auth::broker_from_source(
                                &options.credential_source,
                                &options.token_cache,
                                client.clone(),
                            )
                        },
                        system_prompt,
                        messages,
                        &tx,
                        cancel,
                        &options.trace,
                    )
                    .await
                })
                .await;
            }
        }
        // Route to OpenAI-compat provider if the model id resolves to one.
        if cancel.is_cancelled() {
            return Err(RuntimeError::Canceled);
        }
        let routed = {
            let tools_schema = &tools_schema;
            // Native Anthropic models fall through to the transport below,
            // which books its own usage.
            let counters = options
                .usage_counters
                .clone()
                .filter(|_| !super::auth::model_is_anthropic(model));
            let diagnostic_context = crate::runtime::openai::terminal_diagnostics::Context {
                session: options.diagnostic_session_id.clone(),
                window: options.diagnostic_context_window,
            };
            with_usage_metering(model, counters, &tx, |tx| async move {
                crate::runtime::openai::terminal_diagnostics::CONTEXT
                    .scope(
                        diagnostic_context,
                        crate::runtime::openai::try_route_with_request_ledger_for_event(
                            model,
                            client,
                            tools_schema,
                            system_prompt,
                            messages,
                            &tx,
                            None,
                            None,
                            thinking_budget,
                            reasoning_level,
                            cancel,
                            &options.credential_source,
                            &options.token_cache,
                            max_retries,
                            options.codex_request_role,
                            options.tool_session_id.as_ref(),
                            options.session_tool_set.as_ref(),
                            options.memory_backend.as_ref(),
                            options.sandbox.as_ref(),
                            &options.trace,
                            options.suppress_stream_deltas,
                            crate::runtime::openai::AttemptPolicy::Configured,
                            options.spend_tap.as_ref(),
                            options.event_triggered,
                            options.request_ledger.as_ref(),
                        ),
                    )
                    .await
            })
            .await
        };
        if let Some(result) = routed {
            // Classify honestly: transport blips are API failures, not
            // config problems (incident: session 20260714-025948-3dab).
            return result
                .map_err(|e| crate::runtime::openai::net::provider_error_to_runtime_for(model, e));
        }
        // Account failover (G3) crosses the broker boundary through the
        // router seam, exactly like the Codex transport: the broker vends one
        // (credential, access token) pair, never a refresh token, and this
        // path never opens auth.json. Built lazily — nothing is constructed
        // until a 429 proves window exhaustion.
        let router = super::anthropic_quota::BrokerAnthropicRouter::lazy(
            &options.credential_source,
            &options.token_cache,
            client.clone(),
        );
        Self::anthropic_stream_with_router(
            auth,
            client,
            model,
            tools_schema,
            system_prompt,
            thinking_budget,
            reasoning_level,
            messages,
            tx,
            cancel,
            max_retries,
            refusal_retries,
            options,
            telemetry_level,
            &router,
        )
        .await
    }

    /// The Anthropic Messages transport proper: request assembly, the
    /// unified retry loop and SSE parsing. Routing (cloud, OpenAI-compatible
    /// providers) has already happened in [`Self::call_api_stream_inner`];
    /// `messages` is the wire projection and `tools_schema` the resolved
    /// schema source. `router` is the account-failover seam (production:
    /// [`super::anthropic_quota::BrokerAnthropicRouter`]; tests script it).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::collapsible_match)]
    pub(super) async fn anthropic_stream_with_router(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools_schema: Arc<Vec<Value>>,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: &CancellationToken,
        max_retries: u32,
        refusal_retries: u32,
        options: &ApiOptions,
        telemetry_level: crate::runtime::telemetry::TelemetryLevel,
        router: &dyn super::anthropic_quota::AnthropicAccountRouter,
    ) -> Result<Value> {
        use super::anthropic_quota::{
            body_prefix, classify_anthropic_429, failover_permitted, ratelimit_header_summary,
            Anthropic429, AnthropicFailoverGate, FailoverRefusal, LimitText, NoHop,
            QUOTA_PROBE_BODY_CAP,
        };
        // Provider qualification is application identity; Anthropic's wire API
        // still receives its native bare model id.
        let qualified_model = model;
        let execution_plan = options
            .anthropic_execution_plan
            .as_ref()
            .filter(|plan| {
                plan.qualified_model == qualified_model
                    && plan.requested_level == reasoning_level
                    && plan.role == options.codex_request_role
            })
            .cloned()
            .or_else(|| {
                crate::runtime::openai::catalog::plan_standard_anthropic_transport(
                    qualified_model,
                    reasoning_level,
                    options.codex_request_role,
                )
            })
            .ok_or_else(|| {
                RuntimeError::Config(
                    "Anthropic special mode transport requires an authorized execution plan".into(),
                )
            })?;
        let model = model.strip_prefix("anthropic/").unwrap_or(model);

        // A rollover summary is sent once, on the caller's client, which
        // refuses redirects; the conversation pool follows them (a POST
        // 301/302/303 would become a second, body-less GET). The phase
        // deadlines below apply to either client.
        let streaming_client = match &options.keepwarm {
            Some(KeepWarmRequest {
                purpose: TailPurpose::Summary { .. },
                ..
            }) => client,
            _ => super::http_phases::client(&options.anthropic_stream_client)?,
        };

        // Read auth state for this API call
        let (mut auth_header_name, mut auth_header_value, auth_type, mut auth_binding) =
            await_or_cancel(cancel, Self::build_auth_snapshot(auth)).await?;
        // One credential recovery per logical request (local OAuth or remote).
        let mut auth_retried = false;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No Anthropic credentials. Run `synaps login`, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`.".to_string()
            ));
        }

        // Resolve the configured `cache_ttl` against the credential kind
        // this request goes out on (`auto`: 1h OAuth, 5m API key). A pure
        // function of the kind, so failover between accounts of one kind
        // never flips the TTL (which would rewrite the cache).
        let cache_ttl = options.cache_ttl.resolve_for_auth_type(&auth_type);

        tracing::info!(model = %model, cache_ttl = %cache_ttl, "Starting API request");

        // Manual cache breakpoints for optimal prompt caching.
        // Tested vs auto-cache (top-level cache_control) — manual wins: 90% vs 53% hit rate.
        // Arc-shared (#128): to_vec() on &[SharedMessage] is N refcount bumps,
        // not a deep copy; sanitize/annotate clone-on-write only what they edit.
        let mut cleaned_messages = messages.to_vec();
        let retention =
            super::image_retention::plan_request(messages, super::image_retention::Budget::Native)
                .map_err(RuntimeError::Config)?;
        let retired_count = retention.retired;
        // Deferred tool loading: send the session's loaded tools normally and
        // every other activatable tool with `defer_loading`, and resolve the
        // history's `tool_reference` blocks to wire names. Without a split
        // (or for a model without tool search) references are stripped, so
        // they never reach a request that cannot accept them.
        let split = options.anthropic_tool_split.as_deref().filter(|split| {
            !split.loaded.is_empty()
                && !split.deferred.is_empty()
                && crate::core::models::model_supports_tool_search(model)
        });
        let (wire_tools, deferred_tools): (&[Value], &[Value]) = match split {
            Some(split) => (&split.loaded, &split.deferred),
            None => (&tools_schema, &[]),
        };
        // Stable per-image projection precedes cache markers, only on Anthropic.
        // Original preflight above remains authoritative; recheck output budgets.
        await_or_cancel(
            cancel,
            super::image_limits::enforce_image_limits(&mut cleaned_messages, retired_count),
        )
        .await?
        .map_err(RuntimeError::Config)?;
        super::attachments::validate_messages(model, &cleaned_messages)
            .map_err(RuntimeError::Config)?;
        // Warm summary: the trailing instruction is held back while the
        // recorded history is projected exactly as its own request was
        // (retention anchor, sanitize, tool references, cache marker on its
        // last block); then it is appended after that marked block, as a
        // trailing text block of the last user message (the wire merges
        // adjacent user turns).
        let summary_tail = match &options.keepwarm {
            Some(KeepWarmRequest {
                purpose: TailPurpose::Summary { .. },
                ..
            }) if cleaned_messages.len() >= 2 => cleaned_messages.pop(),
            _ => None,
        };
        // The frontier is a position in `messages`: anchor before sanitation
        // merges messages. A worker round arrives already retired upstream
        // (its frontier rides in the options) and replays as a no-op here.
        HelperMethods::annotate_retention_anchor(
            &mut cleaned_messages,
            super::image_retention::earliest(options.retention_frontier, retention.frontier),
            cache_ttl,
        );
        // Strict checks retain original per-source batch boundaries: sanitizing
        // first could merge valid adjacent user batches past eight attachments.
        HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
        HelperMethods::wire_tool_references(&mut cleaned_messages, &|id: &str| {
            split.and_then(|split| split.id_to_api.get(id).cloned())
        });
        // The recorded request's wire messages, for the prefix comparison.
        let mut summary_history: Option<Vec<crate::SharedMessage>> = None;
        let keepwarm_prefix_len = match &options.keepwarm {
            // Warm summary: the recorded history, projected and marked as its
            // own request was, then the held-back instruction as its tail.
            Some(_) if summary_tail.is_some() => {
                HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, cache_ttl);
                // Whole blocks after the marker were request-local (turn
                // context): cut them, then the final protocol guard.
                if !HelperMethods::cut_after_marker(&mut cleaned_messages)
                    || !super::warm_summary::protocol_complete(&cleaned_messages)
                {
                    return Ok(serde_json::json!({ "keepwarm_refused": "protocol" }));
                }
                summary_history = Some(cleaned_messages.clone());
                let prefix_len = cleaned_messages.len();
                if let Some(tail) = summary_tail {
                    HelperMethods::append_tail_message(&mut cleaned_messages, tail);
                }
                prefix_len
            }
            // Tool-round replay (`runtime::tool_keepwarm`): `messages` are the
            // round's own request messages, so they are processed and marked
            // exactly as that round's request was; the ping text is appended
            // only now, after every marker, as an uncached tail.
            Some(k) if k.mode == KeepWarmMode::Replay => {
                HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, cache_ttl);
                let prefix_len = cleaned_messages.len();
                HelperMethods::append_keepwarm_ping(
                    &mut cleaned_messages,
                    Arc::new(serde_json::json!({
                        "role": "user",
                        "content": super::keepwarm::KEEPWARM_PING_TEXT,
                    })),
                );
                prefix_len
            }
            // Keep-warm: the marker goes on the last HISTORY message — the
            // durable prefix the next real turn re-sends verbatim — and the
            // trailing ping message (never persisted) rides after it as an
            // uncached tail, exactly like a real turn's newest user message.
            Some(_) if cleaned_messages.len() >= 2 => {
                let history_len = cleaned_messages.len() - 1;
                HelperMethods::annotate_cache_breakpoint(
                    &mut cleaned_messages[..history_len],
                    cache_ttl,
                );
                history_len
            }
            _ => {
                HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, cache_ttl);
                cleaned_messages.len().saturating_sub(1)
            }
        };

        // Derive the thinking level from the budget for effort mapping.
        // Body assembly (#128 Slice 4): `RequestBody` BORROWS cleaned_messages
        // and the tool schemas instead of the old `json!` assembly that deep-
        // rebuilt the entire history into a `Value` tree (copy C8). Byte-
        // identical output is enforced by `runtime::body_golden`. Task 9: the
        // transport adapter wraps that same serializer and adds the
        // provider-neutral `TranslationReport` (no Value round-trip, no
        // second history copy).
        let parts = crate::runtime::transport::anthropic::build_anthropic_request(
            model,
            &cleaned_messages,
            wire_tools,
            deferred_tools,
            system_prompt,
            &auth_type,
            thinking_budget,
            reasoning_level,
            Some(&execution_plan),
            cache_ttl,
            true,
            options.effective_context_window(model),
        );
        let mut body = parts.body;
        // Before the keep-warm fingerprint below: a ping's prefix is the
        // turn's, `thinking` included.
        body.set_thinking_display(options.thinking_display);
        // A conversation request sized to the room left (R1) sends the
        // output its assessment reserved. Keep-warm pings and warm summaries
        // keep their own ceilings below (applied to the static cap).
        if let (None, Some(reserved)) = (&options.keepwarm, options.request_output_tokens) {
            if !body.apply_reserved_max_tokens(reserved) {
                tracing::warn!(
                    reserved,
                    max_tokens = body.max_tokens(),
                    "reserved output does not fit this request body; static cap kept"
                );
            }
        }
        // A model that rejects input + `max_tokens` over the window gets the
        // room the input leaves (no-op on Claude 4.5 and later). After the
        // reserved output (both only lower the cap) and before the keep-warm
        // cut below, which only lowers it further.
        body.fit_max_tokens_to_window(Self::provider_window(options, model));
        // Cache diagnostics (`cache_diagnostics = true`): the foreground
        // chain only — pings and the setting-off path send no field.
        let mut diagnostics = super::cache_diagnosis::diagnostics_for(options);
        if let Some(d) = &diagnostics {
            body.set_diagnostics(d.clone());
        }
        // Keep-warm: cut the output cap (not part of the cache key), then
        // fingerprint the prefix exactly as it would go on the wire. A probe —
        // or a ping whose prefix no longer matches what is cached — returns
        // here, before any trace, telemetry or network I/O.
        if let Some(keepwarm) = &options.keepwarm {
            match keepwarm.purpose {
                TailPurpose::Ping => body.apply_keepwarm_max_tokens(),
                TailPurpose::Summary {
                    max_tokens,
                    credential,
                    ..
                } => {
                    if !body.apply_summary_max_tokens(max_tokens) {
                        return Ok(serde_json::json!({ "keepwarm_refused": "output_limit" }));
                    }
                    if super::warm_summary::credential_scope(&auth_header_value) != credential {
                        return Ok(serde_json::json!({ "keepwarm_refused": "credential" }));
                    }
                }
            }
            let beta = Self::build_beta_header(&auth_type, options, model);
            let (actual, send) = match (&summary_history, keepwarm.purpose) {
                (Some(history), TailPurpose::Summary { digest, .. }) => {
                    let actual = body.prefix_digest_over(history, beta.as_deref());
                    (
                        body.prefix_fingerprint_over(history, beta.as_deref()),
                        matches!(keepwarm.mode, KeepWarmMode::Send { .. }) && actual == digest,
                    )
                }
                // A summary without its recorded history is never sent.
                (None, TailPurpose::Summary { .. }) => (0, false),
                _ => {
                    let actual = body.prefix_fingerprint(keepwarm_prefix_len, beta.as_deref());
                    let send = match keepwarm.mode {
                        KeepWarmMode::Probe => false,
                        KeepWarmMode::Send { expect } => expect == actual,
                        KeepWarmMode::Replay => true,
                    };
                    (actual, send)
                }
            };
            if !send {
                return Ok(serde_json::json!({
                    "keepwarm_prefix": actual,
                    "keepwarm_max_tokens": body.max_tokens(),
                }));
            }
        }
        if let (None, Some(tap)) = (&options.keepwarm, &options.prefix_tap) {
            let beta = Self::build_beta_header(&auth_type, options, model);
            // The cached prefix ends at the marker: request-local blocks
            // after it are not part of what a warm summary re-sends.
            let mut prefix = cleaned_messages.clone();
            if HelperMethods::cut_after_marker(&mut prefix) {
                tap.built(body.prefix_digest_over(&prefix, beta.as_deref()));
            }
            tap.authorized(super::warm_summary::credential_scope(&auth_header_value));
            tap.resolved_ttl(cache_ttl);
        }
        let translation_report = parts.report;

        // Did THIS request actually carry at least one 1h marker? Arms the
        // silent-downgrade detector. Mirrors cache_control_value(): OneHour
        // puts 1h on every site (message tail, tools, system); Hybrid puts 1h
        // only on stable-prefix sites (tools, system). Degenerate hybrid +
        // API-key + no system prompt + zero tools → no 1h marker anywhere →
        // detector must stay disarmed (zero 1h bucket proves nothing).
        let has_tool_marker = body.has_tool_marker();
        let has_system_marker = body.has_system_marker();
        let request_has_1h_marker = match cache_ttl {
            crate::core::config::CacheTtl::FiveMinutes => false,
            crate::core::config::CacheTtl::OneHour => {
                !cleaned_messages.is_empty() || has_tool_marker || has_system_marker
            }
            crate::core::config::CacheTtl::Hybrid => has_tool_marker || has_system_marker,
        };

        // Serialize the body once up-front; each retry attempt reuses the same
        // bytes via a cheap `Bytes::clone()` (refcount bump, no copy). Previously
        // we called `req.json(&body).send()` per attempt, which re-ran
        // `serde_json::to_vec(&body)` over the entire conversation on every
        // 429 retry — wasted work since the payload is identical.
        let mut body_bytes: bytes::Bytes = serde_json::to_vec(&body)
            .map_err(|e| {
                RuntimeError::ApiStatus(format!("failed to serialize request body: {}", e))
            })?
            .into();
        #[cfg(test)]
        {
            body_bytes = salt_summary_body_for_test(body_bytes, &options.keepwarm);
        }
        // A ledgered request's liability estimate (§6), the same for every
        // attempt: its bytes and output allowance do not change.
        let ledger_estimate = options.request_ledger.as_ref().map(|ledger| {
            crate::tools::request_ledger::request_estimate(
                ledger.model(),
                &body_bytes,
                body.max_tokens(),
            )
        });
        // The derived request body is dead weight for the whole network wait
        // (retries reuse `body_bytes`): release it — and its owned `system`
        // block copy — before the first send (§3.6 f3).
        drop(body);

        trace_outgoing_request(
            model,
            &body_bytes,
            cleaned_messages.len(),
            tools_schema.len(),
        );

        // ═══ UNIFIED RETRY (task #130) ════════════════════════════════════════
        // ONE generic budget governs transient failures whether they surface as
        // an HTTP status (5xx, pre-stream), a transport drop, or an in-stream
        // `api_error` / `rate_limit_error` under a 200 response. Sharing this
        // counter across send and stream phases stops the budget from multiplying
        // (the two-loop smell). The two service-specific exceptions below are
        // 429 rate limits and Anthropic OAuth overload frames.
        //
        // 429 (rate-limit) keeps its own higher budget: OAuth windows can last
        // minutes, so we honour the reset headers rather than dying after a few
        // tries. Other transients (HTTP 5xx + in-stream api/rate-limit errors)
        // share `max_retries`. Anthropic OAuth `overloaded_error` frames use a
        // separate Claude-compatible persistent budget (10 retries), because
        // these 200/SSE overload responses carry no retry headers.
        const MAX_429_RETRIES: u32 = 8;
        // Endpoint resolved once — identical across retry attempts.
        let anthropic_base = options
            .anthropic_base_url
            .clone()
            .or_else(|| std::env::var("SYNAPS_ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        let anthropic_url = format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));
        let endpoint_scope = super::warm_summary::scope_hash(&anthropic_url);
        match (&options.keepwarm, &options.prefix_tap) {
            (
                Some(KeepWarmRequest {
                    purpose: TailPurpose::Summary { endpoint, .. },
                    ..
                }),
                _,
            ) if *endpoint != endpoint_scope => {
                return Ok(serde_json::json!({ "keepwarm_refused": "endpoint" }));
            }
            (None, Some(tap)) => tap.endpoint(endpoint_scope),
            _ => {}
        }

        // ═══ TRACE (Task 8): one record per actual attempt ════════════════════
        // Emission rule documented in `trace::emit`. Tracing is metadata-only
        // and can never fail or delay the request.
        let mut tracer = await_or_cancel(
            cancel,
            begin_anthropic_tracer(
                options,
                cache_ttl,
                &anthropic_url,
                model,
                &body_bytes,
                &cleaned_messages,
                system_prompt.as_deref(),
                &tools_schema,
                has_tool_marker,
                has_system_marker,
                &translation_report,
            ),
        )
        .await?;
        // Placeholder value: every send re-starts the clock immediately
        // before handing bytes to reqwest (per-attempt timing).
        #[allow(unused_assignments)]
        let mut clock = crate::runtime::trace::AttemptClock::start();

        // ═══ WIRE ENCODING (request_encoding) ═════════════════════════════════
        // `body_bytes` stays the JSON: the trace digests it and the
        // diagnostics fail-soft edits it. `wire` is what reqwest uploads,
        // gzip-encoded when the policy allows; retries reuse it.
        let mut wire = await_or_cancel(
            cancel,
            options.request_compression.encode(&anthropic_url, &body_bytes),
        )
        .await?;

        let mut last_err = String::new();
        let mut last_status: Option<u16> = None;
        let mut last_reset_hint: Option<String> = None;
        let mut non_429_attempts: u32 = 0; // shared server-error budget
        let mut overload_attempts: u32 = 0; // Claude-compatible persistent overload budget
        let mut refusal_attempts: u32 = 0; // refusal-retry budget (separate from error budget)
        let mut attempt: u32 = 0; // total attempts (for backoff calc)
        // ═══ ACCOUNT FAILOVER (G3) request-level state ════════════════════════
        // `output_started`: any SSE frame was parsed on an earlier attempt of
        // this request — the strict, fail-closed reading of "nothing has been
        // streamed" that the hop requires (no cross-account replay).
        // `tool_activity`: an earlier attempt produced a tool_use block.
        // `anthropic_failovers`: hops spent on this request (a backstop).
        // `anthropic_attempted`: every account this request used, in order
        // (the pinned one, then each hop's); a hop never goes back to one.
        // `quota_headers_logged`: the one-time schema-capture warn fired.
        let mut output_started = false;
        let mut tool_activity = false;
        let mut anthropic_failovers: u32 = 0;
        // `capacity_resend_left`: this request may still wait for capacity
        // once at a window limit and be sent again; taken when that wait
        // begins, before the resend, so a second window limit ends the turn.
        let mut capacity_resend_left = true;
        let mut anthropic_attempted: Vec<crate::auth::CredentialRef> = Vec::new();
        let mut quota_headers_logged = false;
        // Accounts this request reported exhausted: if one of them then
        // serves (the ordinary retry outlived a short reset), its cooldown
        // goes (`report_served`).
        let mut anthropic_cooled: Vec<crate::auth::CredentialRef> = Vec::new();
        // A cache keep-warm ping: it keeps the cache of the account it is
        // on, so a limit is reported but never switches accounts, and no
        // notice about it reaches the session.
        let keepwarm_ping = options.keepwarm.is_some();
        let mut sent_at_ms;
        // `first_event_relaxed`: an attempt of this request ended at the
        // first-event deadline, so every later attempt waits the progress
        // window for `message_start` (a cold prefill can be that slow).
        let mut first_event_relaxed = false;

        loop {
            // §6: the ledgered attempt in flight, reserved right before each
            // send; it ends with its answer or its stream.
            let mut ledgered: Option<crate::tools::request_ledger::AttemptReport> = None;
            let response = {
                #[allow(unused_assignments)]
                let mut response = None;

                loop {
                    if attempt > 0 {
                        // Sleep was already computed and stored in last_err context;
                        // delay is recomputed here from the empty header map if we
                        // came from a network error path (no headers available).
                        // For header-aware delays we sleep in the error arm below.
                        // Nothing to do here — sleep already happened.
                    }

                    // Rebuild request (consumed on send)
                    let mut req = streaming_client
                        .post(&anthropic_url)
                        .header(auth_header_name.clone(), auth_header_value.clone());
                    for (name, value) in super::request::ANTHROPIC_FIXED_HEADERS {
                        req = req.header(name, value);
                    }
                    // Build the anthropic-beta header. The 1M-context opt-in
                    // (`context-1m-2025-08-07`) is only added when the user
                    // explicitly requested 1M AND the model supports it. Without
                    // this opt-in, all models default to 200k mode — which is the
                    // documented "smarter" inference regime (see
                    // anthropic.com/engineering/effective-context-engineering).
                    if let Some(beta) = Self::build_beta_header(&auth_type, options, model) {
                        req = req.header("anthropic-beta", beta);
                    }

                    // Every attempt (including 401/refusal retries) rechecks at
                    // the dispatch boundary, not merely at the round's start.
                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Canceled);
                    }
                    // A warm summary whose record was replaced, reset or
                    // invalidated, or whose prefix inputs changed, while the
                    // request was prepared is not sent.
                    if let Some(KeepWarmRequest {
                        lease: Some(lease), ..
                    }) = &options.keepwarm
                    {
                        match lease.state() {
                            super::warm_summary::LeaseState::Valid => {}
                            super::warm_summary::LeaseState::PrefixChanged => {
                                return Ok(
                                    serde_json::json!({ "keepwarm_refused": "prefix_policy" }),
                                );
                            }
                            super::warm_summary::LeaseState::Revoked => {
                                return Ok(serde_json::json!({ "keepwarm_refused": "revoked" }));
                            }
                        }
                    }
                    clock = crate::runtime::trace::AttemptClock::start();
                    let sent_at = super::keepwarm::CacheAnchor::now();
                    sent_at_ms = crate::epoch_millis();
                    let req = wire.encoding_header(req);
                    let req = match &options.keepwarm {
                        Some(keepwarm) => gated_body(
                            req,
                            wire.bytes.clone(),
                            &keepwarm.sent,
                            keepwarm
                                .lease
                                .clone()
                                .map(super::warm_summary::BodyGate::Lease),
                        ),
                        None => req.body(wire.bytes.clone()),
                    };
                    // §6: a ledgered attempt is booked before it is sent; a
                    // run at its request cap sends nothing more.
                    if let (Some(ledger), Some(estimate)) =
                        (&options.request_ledger, ledger_estimate)
                    {
                        ledgered = Some(
                            crate::tools::request_ledger::AttemptReport::reserve(
                                ledger,
                                estimate,
                                crate::tools::request_ledger::UsageReports::Cumulative,
                                &tx,
                            )
                            .map_err(|e| RuntimeError::Config(e.to_string()))?,
                        );
                    }
                    // When this attempt was sent: a transport failure's
                    // retry notice counts the time the user already waited.
                    let attempt_started = std::time::Instant::now();
                    match await_or_cancel(
                        cancel,
                        super::http_phases::send(
                            req,
                            super::http_phases::header_deadline(wire.bytes.len()),
                        ),
                    )
                    .await?
                    {
                        Ok(resp) => {
                            let status = resp.status();
                            options
                                .request_compression
                                .settle(&mut wire, status.is_success());
                            if let Some(keepwarm) = &options.keepwarm {
                                keepwarm.sent.answered();
                                keepwarm
                                    .status
                                    .store(status.as_u16(), std::sync::atomic::Ordering::Relaxed);
                            }
                            if status.is_success() {
                                clock.mark_headers();
                                // Accepted: the prefix's cache entries were
                                // read or written by a request that started
                                // no earlier than `sent_at`.
                                if let Some(anchor) = &options.cache_anchor {
                                    anchor.stamp(sent_at);
                                }
                                if !anthropic_cooled.is_empty() {
                                    let served = super::auth::turn_pinned_credential(
                                        &options.credential_source,
                                        auth_binding.as_deref(),
                                    );
                                    if let Some(seat) =
                                        served.filter(|s| anthropic_cooled.contains(s))
                                    {
                                        await_or_cancel(
                                            cancel,
                                            router.report_served(&seat, sent_at_ms),
                                        )
                                        .await?;
                                        anthropic_cooled.retain(|c| c != &seat);
                                    }
                                }
                                if let (None, Some(tap)) = (&options.keepwarm, &options.prefix_tap)
                                {
                                    tap.accepted(sent_at);
                                }
                                response = Some(resp);
                                break;
                            }

                            // §6: an error status from this client (which
                            // follows redirects) is no evidence of zero
                            // spend: the attempt stays open.
                            if let Some(report) = ledgered.take() {
                                settle_failed_attempt(
                                    report,
                                    super::api_sync::FailureStage::HttpError {
                                        status: status.as_u16(),
                                        error_type: None,
                                    },
                                );
                            }

                            // C1: a 401 means the token we sent was rejected —
                            // revoked, or rotated elsewhere (broker, another
                            // `synaps login`, refresh cron, synced auth.json)
                            // before our in-memory expiry said so. Status-level,
                            // so nothing has streamed yet. Invalidate, re-vend,
                            // and retry ONCE. Local API-key mode is excluded:
                            // there is no managed credential to reload.
                            // Never for a keep-warm ping or summary: one request,
                            // and a background request leaves auth to the turns.
                            if status.as_u16() == 401
                                && !auth_retried
                                && (options.credential_source.is_remote() || auth_type == "oauth")
                                && options.keepwarm.is_none()
                            {
                                auth_retried = true;
                                clock.mark_headers();
                                if let Some(t) = tracer.as_mut() {
                                    let rid = provider_request_id_from_headers(resp.headers());
                                    t.attempt_failed(
                                        clock,
                                        crate::runtime::trace::RetryClass::Auth,
                                        Duration::ZERO,
                                        Some(401),
                                        rid,
                                        "http_401",
                                    );
                                }
                                // Drop the rejected response: no unbounded body drain.
                                drop(resp);
                                if !options.credential_source.is_remote() {
                                    let rejected =
                                        auth_header_value.strip_prefix("Bearer ").unwrap_or("");
                                    await_or_cancel(
                                        cancel,
                                        super::auth::AuthMethods::recover_local_rejection(
                                            auth,
                                            client,
                                            &options.credential_source,
                                            &options.token_cache,
                                            auth_binding.as_deref(),
                                            rejected,
                                        ),
                                    )
                                    .await??;
                                } else {
                                    options.token_cache.invalidate("anthropic");
                                    {
                                        let mut g = await_or_cancel(cancel, auth.write()).await?;
                                        g.auth_token.clear();
                                        g.token_expires = None;
                                    }
                                    await_or_cancel(
                                        cancel,
                                        super::auth::AuthMethods::refresh_if_needed(
                                            std::sync::Arc::clone(auth),
                                            client,
                                            &options.credential_source,
                                            &options.token_cache,
                                            Some(model),
                                            super::auth::RefreshPoint::WithinTurn,
                                        ),
                                    )
                                    .await??;
                                }
                                let (n, v, _t, binding) =
                                    await_or_cancel(cancel, Self::build_auth_snapshot(auth))
                                        .await?;
                                auth_header_name = n;
                                auth_header_value = v;
                                auth_binding = binding;
                                if let (None, Some(tap)) = (&options.keepwarm, &options.prefix_tap)
                                {
                                    tap.authorized(super::warm_summary::credential_scope(
                                        &auth_header_value,
                                    ));
                                }
                                continue;
                            }

                            let is_429 = status.as_u16() == 429;
                            let is_retryable =
                                matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);

                            // Capture headers before consuming the body.
                            let (delay, from_hdr) =
                                telemetry::retry_delay_from_headers(resp.headers(), attempt + 1);
                            let reset_hint = if from_hdr {
                                Some(format!("{}s", delay.as_secs()))
                            } else {
                                None
                            };
                            // Headers outlive the body read only for the OAuth
                            // 429 classifier (G3); api-key auth has no seats.
                            let quota_headers = if is_429 && auth_type == "oauth" {
                                Some(resp.headers().clone())
                            } else {
                                None
                            };

                            clock.mark_headers();
                            let trace_rid = provider_request_id_from_headers(resp.headers());
                            // Error bodies may echo prompts or be unbounded.
                            // Retain only the quota probe cap, even before
                            // classification/humanization; never buffer the
                            // full upstream response just to take a prefix.
                            let error_text = await_or_cancel(cancel, async {
                                let mut stream = resp.bytes_stream();
                                let mut bytes = Vec::new();
                                while let Some(Ok(chunk)) = super::http_phases::next(
                                    &mut stream,
                                    super::http_phases::STREAM_IDLE,
                                )
                                .await
                                {
                                    let remaining = QUOTA_PROBE_BODY_CAP - bytes.len();
                                    bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                                    if bytes.len() == QUOTA_PROBE_BODY_CAP {
                                        break;
                                    }
                                }
                                String::from_utf8_lossy(&bytes).into_owned()
                            })
                            .await?;

                            // ═══ ACCOUNT FAILOVER (G3): window exhaustion on a 429 ═══
                            // Recognized ONLY from provider-declared signals (see
                            // `anthropic_quota::classify_anthropic_429`); anything
                            // else keeps the retry path below untouched. The body
                            // is consulted as a bounded prefix and never logged.
                            if let Some(headers) = quota_headers.as_ref() {
                                if !quota_headers_logged {
                                    quota_headers_logged = true;
                                    tracing::warn!(
                                        headers = %ratelimit_header_summary(headers),
                                        "anthropic 429 headers (schema capture)"
                                    );
                                }
                                let now_ms = crate::epoch_millis();
                                if let Anthropic429::QuotaExhausted(evidence) = classify_anthropic_429(
                                    headers,
                                    body_prefix(&error_text, QUOTA_PROBE_BODY_CAP),
                                    now_ms,
                                ) {
                                    // A limit of another model's requests can't be
                                    // what refused this one: the whole seat's.
                                    let evidence = evidence.for_request_model(model);
                                    // The seat this request is pinned to on THIS
                                    // source. `None` = legacy/unbound state: nothing
                                    // to report or switch, but the long-reset rule
                                    // below still applies.
                                    let seat = {
                                        let g = await_or_cancel(cancel, auth.read()).await?;
                                        super::auth::turn_pinned_credential(
                                            &options.credential_source,
                                            g.bound_credential.as_deref(),
                                        )
                                    };
                                    let auto = router.auto_selected();
                                    let label = seat
                                        .as_ref()
                                        .map(|s| s.account.label_str().to_string())
                                        .unwrap_or_else(|| "anthropic".to_string());
                                    // ALWAYS report first — even when the gate then
                                    // refuses to switch — so the next turn boundary's
                                    // AutoRepin selects away from this seat.
                                    match seat.as_ref() {
                                        Some(seat) => {
                                            await_or_cancel(cancel, router.report_exhausted(seat, &evidence))
                                                .await?;
                                            if !anthropic_cooled.contains(seat) {
                                                anthropic_cooled.push(seat.clone());
                                            }
                                        }
                                        None => tracing::warn!(
                                            signal = ?evidence.signal,
                                            "anthropic window exhausted but no seat is bound; cooldown not reported"
                                        ),
                                    }
                                    let gate = AnthropicFailoverGate {
                                        auto_selector: auto,
                                        output_started,
                                        tool_activity,
                                        failovers_so_far: anthropic_failovers,
                                        // Pings and summaries: one request,
                                        // on the session's own seat.
                                        single_attempt: options.keepwarm.is_some(),
                                    };
                                    let mut hop = None;
                                    // Why there is no hop, when there isn't one.
                                    // An unbound request never switches.
                                    let mut no_hop = NoHop::Explicit;
                                    // The broker's reason when no other account
                                    // has capacity (a capacity wait plans from it).
                                    let mut no_hop_failure = None;
                                    if let Some(seat) = seat.as_ref() {
                                        // The exhausted account goes last in the
                                        // attempted set (once).
                                        anthropic_attempted.retain(|c| c != seat);
                                        anthropic_attempted.push(seat.clone());
                                        let permitted = if keepwarm_ping {
                                            Err(FailoverRefusal::KeepWarmPing)
                                        } else {
                                            failover_permitted(&gate)
                                        };
                                        match permitted {
                                            Ok(()) => {
                                                match await_or_cancel(
                                                    cancel,
                                                    router.failover_detailed(
                                                        model,
                                                        &anthropic_attempted,
                                                    ),
                                                )
                                                .await?
                                                {
                                                    Ok(Ok(next))
                                                        if !anthropic_attempted
                                                            .contains(&next.credential) =>
                                                    {
                                                        hop = Some(next);
                                                    }
                                                    Ok(none) => {
                                                        no_hop_failure = none.err().flatten();
                                                        no_hop = NoHop::NoOtherAccount;
                                                        tracing::info!(
                                                            account = %seat,
                                                            attempted = anthropic_attempted.len(),
                                                            "anthropic window exhausted; no account this request hasn't used has capacity"
                                                        )
                                                    }
                                                    Err(e) => {
                                                        no_hop = NoHop::LookupFailed;
                                                        tracing::warn!(
                                                            account = %seat,
                                                            error = %e,
                                                            "anthropic failover candidate resolution failed; not switching"
                                                        )
                                                    }
                                                }
                                            }
                                            Err(refusal) => {
                                                no_hop = NoHop::from_refusal(refusal);
                                                tracing::info!(
                                                    account = %seat,
                                                    reason = refusal.label(),
                                                    "anthropic window exhausted; account failover not permitted"
                                                )
                                            }
                                        }
                                    }
                                    // Every line about this limit comes from one place.
                                    let text = LimitText {
                                        label: &label,
                                        evidence: &evidence,
                                        now_ms,
                                    };
                                    if let Some(next) = hop {
                                        let next_label = next.credential.account.label_str().to_string();
                                        tracing::warn!(
                                            from = %label,
                                            to = %next_label,
                                            hop = anthropic_failovers + 1,
                                            "anthropic window exhausted; switching account (no output replayed)"
                                        );
                                        if let Some(t) = tracer.as_mut() {
                                            t.attempt_failed(
                                                clock,
                                                crate::runtime::trace::RetryClass::RateLimited,
                                                Duration::ZERO,
                                                Some(429),
                                                trace_rid.clone(),
                                                "account_failover",
                                            );
                                        }
                                        // The sanctioned mid-turn re-pin (one per hop):
                                        // bind the new seat exactly as the refresh path
                                        // does, then rebuild the header from it (same as
                                        // the 401 path). The failed seat's token is still
                                        // valid — only its capacity is spent — so no
                                        // cache invalidation is needed.
                                        await_or_cancel(
                                            cancel,
                                            super::auth::AuthMethods::bind_pinned(
                                                auth,
                                                &options.credential_source,
                                                next,
                                            ),
                                        )
                                        .await?;
                                        let (n, v, _t, binding) = await_or_cancel(
                                            cancel,
                                            Self::build_auth_snapshot(auth),
                                        )
                                        .await?;
                                        auth_header_name = n;
                                        auth_header_value = v;
                                        auth_binding = binding;
                                        anthropic_failovers += 1;
                                        let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(
                                            text.hop(&next_label),
                                        )));
                                        // Retry immediately on the new seat WITHOUT
                                        // consuming a 429 retry slot.
                                        continue;
                                    }
                                    // No hop. Say why, then either end the turn now
                                    // (a reset hours away must not burn the whole 429
                                    // budget a minute at a time) or keep the existing
                                    // retry path for short/unknown resets. No text
                                    // claims another account will serve.
                                    if !keepwarm_ping {
                                        let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(
                                            text.no_hop(no_hop),
                                        )));
                                    }
                                    if evidence.reset_is_long(now_ms) {
                                        // Capacity wait: nothing streamed, automatic
                                        // selection, no other account (the gate passed
                                        // and the broker found none), a local source,
                                        // and this request hasn't waited before. Wait
                                        // in the request for the earliest reset (this
                                        // account's or another's), then select again
                                        // with nothing excluded and send the same
                                        // request once on that account.
                                        // The stop in a wait's terms, whether or not
                                        // one may run here: what the turn is marked
                                        // with when it ends on this limit.
                                        let stopped = match (no_hop, evidence.resets_at_ms) {
                                            (NoHop::NoOtherAccount, Some(reset))
                                                if !options.credential_source.is_remote() =>
                                            {
                                                super::capacity_wait::Waitable::window_limit(
                                                    reset,
                                                    no_hop_failure.as_ref(),
                                                    now_ms,
                                                )
                                            }
                                            _ => None,
                                        };
                                        let waitable = stopped.clone().filter(|_| capacity_resend_left);
                                        let waiter = match (options.capacity_wait.as_ref(), waitable) {
                                            (Some(policy), Some(w)) => super::capacity_wait::CapacityWaiter::begin(
                                                policy,
                                                "window_limit",
                                                w,
                                                &tx,
                                            ),
                                            _ => None,
                                        };
                                        // No wait ran (capacity_wait off, this
                                        // request's one wait already used, or
                                        // nothing resets inside the bound): the turn
                                        // ends on this limit. A wait that runs out
                                        // marks its own last failure.
                                        if waiter.is_none() {
                                            if let Some(w) = stopped.as_ref() {
                                                super::capacity_wait::note_stop(w, now_ms);
                                            }
                                        }
                                        if let Some(mut waiter) = waiter {
                                            // One wait (and one resend) per request.
                                            capacity_resend_left = false;
                                            let wait_started = tokio::time::Instant::now();
                                            let resumed = loop {
                                                match waiter.sleep(cancel).await {
                                                    super::capacity_wait::Wake::Check => {}
                                                    super::capacity_wait::Wake::Canceled => {
                                                        return Err(RuntimeError::Canceled)
                                                    }
                                                    super::capacity_wait::Wake::Ended => break None,
                                                }
                                                // Cut at the bound and at the
                                                // cadence: a check still running at
                                                // the next cadence point is dropped
                                                // and the next starts at once; one
                                                // still running at the bound ends
                                                // the turn as before.
                                                let Some(checked) = waiter
                                                    .bounded(|| {
                                                        await_or_cancel(
                                                            cancel,
                                                            router.failover_detailed(model, &[]),
                                                        )
                                                    })
                                                    .await
                                                else {
                                                    break None;
                                                };
                                                let still = match checked? {
                                                    Ok(Ok(next)) => {
                                                        waiter.resumed();
                                                        break Some(next);
                                                    }
                                                    Ok(Err(failure)) => {
                                                        // This account's own reset counts
                                                        // until it passes; then only the
                                                        // broker's reason does.
                                                        let now = waiter.now_ms();
                                                        match evidence.resets_at_ms.filter(|r| *r > now) {
                                                            Some(own) => super::capacity_wait::Waitable::window_limit(
                                                                own,
                                                                failure.as_ref(),
                                                                now,
                                                            ),
                                                            None => failure.as_ref().and_then(|f| {
                                                                super::capacity_wait::Waitable::from_selection(f, now)
                                                            }),
                                                        }
                                                    }
                                                    Err(_) => {
                                                        // A fixed class, never the error's
                                                        // text (it can carry a token or an
                                                        // identity).
                                                        tracing::warn!(
                                                            error_class = "lookup_failed",
                                                            "capacity wait: looking up an account failed; ending the wait"
                                                        );
                                                        None
                                                    }
                                                };
                                                if !waiter.still(still) {
                                                    break None;
                                                }
                                            };
                                            if let Some(next) = resumed {
                                                if let Some(t) = tracer.as_mut() {
                                                    t.attempt_failed(
                                                        clock,
                                                        crate::runtime::trace::RetryClass::RateLimited,
                                                        wait_started.elapsed(),
                                                        Some(429),
                                                        trace_rid.clone(),
                                                        "capacity_wait",
                                                    );
                                                }
                                                // Bind the selected account as a hop does,
                                                // then rebuild the header from it.
                                                await_or_cancel(
                                                    cancel,
                                                    super::auth::AuthMethods::bind_pinned(
                                                        auth,
                                                        &options.credential_source,
                                                        next,
                                                    ),
                                                )
                                                .await?;
                                                let (n, v, _t, binding) = await_or_cancel(
                                                    cancel,
                                                    Self::build_auth_snapshot(auth),
                                                )
                                                .await?;
                                                auth_header_name = n;
                                                auth_header_value = v;
                                                auth_binding = binding;
                                                // Send again at once, without using a
                                                // 429 retry slot.
                                                continue;
                                            }
                                        }
                                        stamp_failure(
                                            options,
                                            super::api_sync::vetted_error_type(&error_text)
                                                .unwrap_or("unrecognized_error"),
                                        );
                                        if let Some(t) = tracer.take() {
                                            let terminal = t.failed_terminal("quota_exhausted");
                                            t.finish(clock, Some(429), trace_rid, None, None, terminal);
                                        }
                                        return Err(RuntimeError::ApiStatus(text.terminal(no_hop)));
                                    }
                                }
                            }

                            // ═══ CACHE DIAGNOSTICS fail-soft ═══
                            // The API refused the beta header or the
                            // `diagnostics` field: latch it off for the session
                            // and re-send this request without either (header
                            // is rebuilt from the latch). Costs no retry budget;
                            // a turn never fails because of diagnostics. The
                            // trace keeps the first attempt's wire metadata.
                            let diag_on_wire = diagnostics.is_some()
                                || super::cache_diagnosis::wants_beta_header(&auth_type, options);
                            if diag_on_wire
                                && super::cache_diagnosis::is_rejection(
                                    status.as_u16(),
                                    &error_text,
                                )
                            {
                                if let Some(chain) = &options.cache_diagnosis {
                                    chain.reject(status.as_u16());
                                }
                                if let Some(d) = diagnostics.take() {
                                    match super::cache_diagnosis::strip_diagnostics(&body_bytes, &d)
                                    {
                                        Some(stripped) => {
                                            body_bytes = stripped.into();
                                            wire = await_or_cancel(
                                                cancel,
                                                options
                                                    .request_compression
                                                    .encode(&anthropic_url, &body_bytes),
                                            )
                                            .await?;
                                        }
                                        // Unreachable by construction (the
                                        // field serializes first); keep the
                                        // normal error path rather than resend
                                        // a body we could not fix.
                                        None => diagnostics = Some(d),
                                    }
                                }
                                if diagnostics.is_none() {
                                    if let Some(t) = tracer.as_mut() {
                                        t.attempt_failed(
                                            clock,
                                            crate::runtime::trace::RetryClass::Other,
                                            Duration::ZERO,
                                            Some(status.as_u16()),
                                            trace_rid,
                                            "cache_diagnosis_rejected",
                                        );
                                    }
                                    continue;
                                }
                            }

                            // ═══ GZIP REFUSAL fail-soft (request_encoding) ═══
                            // A 400/415 to a gzip body: resend it once
                            // uncompressed, costing no retry budget. The
                            // resend's outcome decides whether gzip latches
                            // off (`settle` above), so a genuine 400 just
                            // fails again below and leaves gzip on.
                            if let Some(resend) = options.request_compression.resend_uncompressed(
                                &wire,
                                &body_bytes,
                                status.as_u16(),
                            ) {
                                wire = resend;
                                if let Some(t) = tracer.as_mut() {
                                    t.attempt_failed(
                                        clock,
                                        crate::runtime::trace::RetryClass::Other,
                                        Duration::ZERO,
                                        Some(status.as_u16()),
                                        trace_rid,
                                        "gzip_refused",
                                    );
                                }
                                continue;
                            }

                            // Decide whether we've exhausted retries for this error class.
                            // A keep-warm ping never retries: any error stops it.
                            let retry_exhausted = if options.keepwarm.is_some() {
                                true
                            } else if is_429 {
                                attempt >= MAX_429_RETRIES
                            } else {
                                non_429_attempts >= max_retries
                            };

                            // A refusal of the 1M window (classified from the
                            // body, which stays here) ends the attempt without
                            // retries; the stream turn decides the fallback.
                            let long_context = options
                                .long_context_refusal
                                .as_ref()
                                .filter(|_| {
                                    options.use_1m_context
                                        && crate::core::models::model_supports_1m(model)
                                })
                                .and_then(|slot| {
                                    crate::core::error::classify_long_context_refusal(
                                        status.as_u16(),
                                        &error_text,
                                    )
                                    .map(|class| (slot, class))
                                });

                            if !is_retryable || retry_exhausted || long_context.is_some() {
                                stamp_failure(
                                    options,
                                    super::api_sync::vetted_error_type(&error_text)
                                        .unwrap_or("unrecognized_error"),
                                );
                                let log_rid = trace_rid.clone();
                                if let Some(t) = tracer.take() {
                                    let terminal =
                                        t.failed_terminal(&format!("http_{}", status.as_u16()));
                                    t.finish(
                                        clock,
                                        Some(status.as_u16()),
                                        trace_rid,
                                        None,
                                        None,
                                        terminal,
                                    );
                                }
                                if let Some((slot, class)) = long_context {
                                    slot.report(super::native_window::LongContextFailure {
                                        status: status.as_u16(),
                                        class,
                                    });
                                }
                                let hint = reset_hint.as_deref().or(last_reset_hint.as_deref());
                                return Err(RuntimeError::ApiStatus(terminal_http_error(
                                    status.as_u16(),
                                    &error_text,
                                    hint,
                                    log_rid.as_ref(),
                                )));
                            }

                            last_status = Some(status.as_u16());
                            last_reset_hint = reset_hint.clone();
                            // SECURITY (spec §5.1): `error_text` is untrusted and can
                            // echo the entire request. Retain only the status for
                            // retry state — never the body (it reaches logs and the
                            // exhausted-fallback error text below).
                            last_err = format!("HTTP {}", status.as_u16());

                            if !is_429 {
                                non_429_attempts += 1;
                            }

                            // Emit user-visible notice with specific timing when known.
                            let budget = if is_429 { MAX_429_RETRIES } else { max_retries };
                            let retry_num = if is_429 {
                                attempt + 1
                            } else {
                                non_429_attempts
                            };
                            let notice = if is_429 {
                                if let Some(ref hint) = reset_hint {
                                    format!(
                                        "⚠ Rate limited — resuming in {} ({}/{})",
                                        hint, retry_num, budget
                                    )
                                } else {
                                    format!("⚠ Rate limited — retrying ({}/{})", retry_num, budget)
                                }
                            } else {
                                format!("⏳ API error, retrying ({}/{})…", retry_num, budget)
                            };
                            tracing::warn!(
                                "API retry after {:?}: {} — {}",
                                delay,
                                notice,
                                last_err
                            );
                            let _ = tx.send(StreamEvent::Session(SessionEvent::Retry(notice)));

                            if let Some(t) = tracer.as_mut() {
                                t.attempt_failed(
                                    clock,
                                    crate::runtime::trace::anthropic::retry_class_for_status(
                                        status.as_u16(),
                                    ),
                                    delay,
                                    Some(status.as_u16()),
                                    trace_rid,
                                    &format!("http_{}", status.as_u16()),
                                );
                            }

                            await_or_cancel(cancel, tokio::time::sleep(delay)).await?;

                            if cancel.is_cancelled() {
                                return Err(RuntimeError::Canceled);
                            }
                        }
                        Err(e) => {
                            options.request_compression.settle(&mut wire, false);
                            // §6: this client follows redirects, so even a
                            // refused connection may come after a first hop
                            // that received the request (the broker types it
                            // the same way): the attempt stays open.
                            if let Some(report) = ledgered.take() {
                                settle_failed_attempt(
                                    report,
                                    super::api_sync::FailureStage::AfterSend,
                                );
                            }
                            if let Some(keepwarm) = &options.keepwarm {
                                if matches!(&e, RuntimeError::Api(error) if error.is_connect()) {
                                    keepwarm
                                        .connect_failed
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            non_429_attempts += 1;
                            let diag = TransportDiag::from_runtime(&e, send_failure_label(&e));
                            if non_429_attempts > max_retries {
                                stamp_failure(options, send_failure_label(&e));
                                if let Some(t) = tracer.take() {
                                    let terminal = t.failed_terminal("network");
                                    t.finish(clock, None, None, None, None, terminal);
                                }
                                warn_transport!(
                                    diag,
                                    non_429_attempts,
                                    max_retries,
                                    error = %e.user_message(),
                                    "API network retries exhausted"
                                );
                                return Err(e);
                            }
                            last_err = e.user_message();
                            last_status = None;
                            // No headers on network error — plain exponential back-off.
                            let delay = super::telemetry::network_retry_delay(non_429_attempts);
                            warn_transport!(
                                diag,
                                non_429_attempts,
                                max_retries,
                                "API retry {}/{} after {:?}: {}",
                                non_429_attempts,
                                max_retries,
                                delay,
                                last_err
                            );
                            // Display only: a fast first transport retry
                            // (usually a stale pooled connection) is silent;
                            // the WARN above, the count and the backoff are not.
                            // A slow failure (a header timeout) plus its
                            // backoff counts as a long wait and is shown.
                            if show_transport_retry_notice(
                                non_429_attempts,
                                attempt_started.elapsed().saturating_add(delay),
                            ) {
                                let _ = tx.send(StreamEvent::Session(SessionEvent::Retry(format!(
                                    "⏳ API error, retrying ({}/{})…",
                                    non_429_attempts, max_retries
                                ))));
                            }
                            if let Some(t) = tracer.as_mut() {
                                t.attempt_failed(
                                    clock,
                                    crate::runtime::trace::RetryClass::Network,
                                    delay,
                                    None,
                                    None,
                                    "network",
                                );
                            }
                            await_or_cancel(cancel, tokio::time::sleep(delay)).await?;
                            if cancel.is_cancelled() {
                                return Err(RuntimeError::Canceled);
                            }
                        }
                    }

                    attempt += 1;
                }

                response.ok_or_else(|| {
                    let hint = last_reset_hint.as_deref();
                    let status = last_status.unwrap_or(0);
                    if status == 429 {
                        RuntimeError::ApiStatus(crate::core::error::humanize_api_error_with_reset(
                            429, &last_err, hint,
                        ))
                    } else {
                        RuntimeError::Tool(format!("API failed after retries: {}", last_err))
                    }
                })?
            };

            // ═══ TELEMETRY: capture headers before consuming the response body ═══
            let request_start = std::time::Instant::now();
            // Trace: validated provider request ID (invalid values omitted).
            let trace_rid_stream = if tracer.is_some() {
                provider_request_id_from_headers(response.headers())
            } else {
                None
            };
            let telem_request_id = if telemetry_level.enabled() {
                telemetry::request_id_from_headers(response.headers())
            } else {
                None
            };
            let telem_ratelimit = if telemetry_level == TelemetryLevel::Full {
                let rl = telemetry::ratelimit_from_headers(response.headers());
                if rl.is_empty() {
                    None
                } else {
                    Some(rl)
                }
            } else {
                None
            };

            let mut stream = response.bytes_stream();
            let mut transport_error = None;
            tracing::debug!("Stream opened");

            if !options.suppress_stream_deltas {
                let _ = tx.send(StreamEvent::Llm(LlmEvent::ResponseStart));
            }
            let mut state = ParseState::new();
            if first_event_relaxed {
                state.progress.relax_first_event();
            }
            // Child spend: one guard per accepted attempt (an HTTP error
            // answer has no body usage, so it gets none). Settled below once
            // the body has ended, before any retry, refusal retry or
            // failover dispatches again; its Drop covers a cancel, an error
            // return and a dropped future. Inert without a worker's tap.
            let spend = super::child_spend::AttemptSpend::new(
                options.spend_tap.as_ref(),
                super::child_spend::SpendKind::Turn,
                model,
            );
            let ctx = EventCtx {
                model,
                tx: &tx,
                suppress_stream_deltas: options.suppress_stream_deltas,
                telemetry_level,
                request_start,
                cache_ttl,
                ttl_downgrade_notified: options.ttl_downgrade_notified.clone(),
                saw_1h_honored: options.saw_1h_honored.clone(),
                request_has_1h_marker,
                usage_counters: options.usage_counters.clone(),
                cache_diagnosis: diagnostics.is_some(),
                spend: spend.observer(),
                usage_request: ledgered
                    .as_ref()
                    .map(|report| report.request().clone()),
            };

            // SSE can split across chunk boundaries (even mid-UTF-8-codepoint), so
            // buffer raw bytes and only parse complete lines. Zero-copy: lines are
            // borrowed from the buffer, parsed in place (REVIEW.md P2).
            let mut line_buffer = super::sse::SseLineBuffer::new();

            // Cancellation exits through the normal finalization below: partial
            // content, residual Usage and attempt telemetry must not be dropped.
            // Besides the raw-byte idle timer, a progress deadline (no first
            // event, no progress: pings only) ends a read as a timeout, and
            // the transport arm below retries it like a lost connection.
            loop {
                let read = super::http_phases::next_or_deadline(
                    &mut stream,
                    super::http_phases::STREAM_IDLE,
                    &mut state.progress,
                );
                let Ok(Some(chunk)) = await_or_cancel(cancel, read).await else {
                    break;
                };
                if cancel.is_cancelled() {
                    break;
                }
                // A transport error mid-stream means connection loss. It's the same
                // transient class as an HTTP 5xx or an in-stream overloaded_error, so
                // route it through the unified retry budget instead of hard-failing
                // the turn. Bill what the stream reported first (its last counted
                // delta, else the start's capture): the API already
                // processed the input even if the stream died on us.
                let chunk = match chunk {
                    Ok(c) => {
                        clock.mark_first_byte();
                        c
                    }
                    Err(e) => {
                        if state.stop_reason_seen || state.stream_error.is_some() {
                            break;
                        }
                        emit_final_usage(&mut state, &ctx);
                        state.stream_error = Some(StreamError {
                            error_type: None,
                            message: e.user_message(),
                            retryable: true,
                        });
                        transport_error = Some(e);
                        break;
                    }
                };
                line_buffer.extend(&chunk);

                // Process complete lines from the buffer (zero-copy borrows)
                while let Some(line) = line_buffer.next_line() {
                    process_data_line(line, &mut state, &ctx);
                }
            }

            // Process any remaining buffered data (final line without trailing
            // newline) — same seam as the main loop, so all event types in a
            // partial final line are handled.
            let remaining = line_buffer.take_remaining().unwrap_or_default();
            process_data_line(&remaining, &mut state, &ctx);
            // One line per stream, however it ended: counts and timings only.
            let progress = &state.progress;
            tracing::debug!(
                "Stream ended: chunks={} pings={} events={} message_start={} first_event_ms={} max_event_gap_ms={} deadline={}",
                progress.chunks,
                progress.pings,
                progress.events,
                progress.message_start_seen,
                progress
                    .message_start_after
                    .map_or_else(|| "none".to_owned(), |d| d.as_millis().to_string()),
                progress.max_gap.as_millis(),
                progress
                    .fired
                    .map_or("none", super::http_phases::Deadline::label),
            );
            first_event_relaxed |= progress.fired == Some(super::http_phases::Deadline::FirstEvent);

            // Flush any partial block and return accumulated content
            state.finalize();

            // G3 failover gate inputs: once any SSE frame was parsed on this
            // request, a later 429 must never hop accounts (no cross-account
            // replay); a produced tool_use block is tool activity.
            output_started |= state.first_event_seen;
            tool_activity |= state
                .accumulated_content
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));

            // The one Usage event, if message_stop didn't send it: a stream cut
            // after its last counted delta (cancel, transport death) sends that
            // delta's counts; one cut before any delta sends the message_start
            // capture. No-op once sent.
            emit_final_usage(&mut state, &ctx);
            // The attempt's body has ended: book what it reported, now.
            spend.settle();

            // §6: the attempt ends with its stream: measured by its final
            // report when that states every count, open otherwise (a running
            // count is not its spend, an absent count is not a zero).
            if let Some(mut report) = ledgered.take() {
                if let Some(usage) = settled_usage(&state.wire_usage) {
                    report.observe(&usage);
                }
                report.finish();
            }

            // Capture the failure signals BEFORE the telemetry block — it moves
            // `state.telem_stop_reason` into the record when telemetry is enabled.
            let stream_error = state.stream_error.take();
            let has_stop_reason = state.stop_reason_seen;
            let stop_reason_is_refusal = state.stop_reason_is_refusal;
            let cancelled = cancel.is_cancelled();
            let outcome_content = state.take_outcome_content(cancelled);
            if let Some(keepwarm) = &options.keepwarm {
                *keepwarm
                    .stop_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = state.trace_stop_reason;
                *keepwarm
                    .usage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = state.wire_usage;
            }
            if let (None, Some(tap), None, false) = (
                &options.keepwarm,
                &options.prefix_tap,
                &stream_error,
                cancelled,
            ) {
                tap.completed(state.wire_usage.complete());
            }

            // ═══ TRACE: finish this attempt's monotonic clock + captures ═══
            clock.mark_stream_end();
            if let Some(offset_ms) = state.first_event_elapsed_ms {
                clock.set_first_model_event_after_headers(offset_ms);
            }
            let trace_stop_reason = state.trace_stop_reason;
            let trace_usage = state.trace_usage;
            // Structural failure code for trace terminals: a vetted static
            // error-class label or a fixed literal — never raw provider text.
            let stream_error_code: &str = match stream_error.as_ref() {
                Some(e) => e
                    .error_type
                    .as_deref()
                    .and_then(crate::core::error::sanitize_error_type)
                    .unwrap_or("stream_error"),
                None => "empty_response",
            };

            // ═══ CACHE DIAGNOSTICS: advance the chain, tag the trace ═══
            // The server fingerprints every request that carried
            // `diagnostics`, keyed by its message id — so the next request
            // compares against this one even if its stream later failed.
            if diagnostics.is_some() {
                if let (Some(chain), Some(id)) = (&options.cache_diagnosis, &state.telem_msg_id) {
                    chain.record(id);
                }
            }
            let server_diagnosis = state.server_diagnosis.take();
            if let Some(t) = tracer.as_mut() {
                t.set_server_diagnosis(server_diagnosis);
            }

            // ═══ TELEMETRY: write the record ═══
            if telemetry_level.enabled() {
                // Build context record — what we sent
                let breakpoints: Vec<usize> = cleaned_messages
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| {
                        // ANY block, not just the last: the conversational
                        // marker lands on the last DURABLE block, which sits
                        // before the ephemeral injected tail when per-turn
                        // context is attached.
                        if let Some(arr) = m["content"].as_array() {
                            arr.iter().any(|b| b.get("cache_control").is_some())
                        } else {
                            false
                        }
                    })
                    .map(|(i, _)| i)
                    .collect();

                let system_bytes = system_prompt.as_ref().map(|s| s.len()).unwrap_or(0);

                let mut record = telemetry::TelemetryRecord {
                    ts: telemetry::TelemetryRecord::now_ms(),
                    request_id: telem_request_id,
                    msg_id: state.telem_msg_id,
                    model: model.to_string(),
                    attempt: attempt + 1, // 1-based: 0 = first try, N = (N+1)th send
                    refusal_retries_used: if refusal_attempts > 0 {
                        Some(refusal_attempts)
                    } else {
                        None
                    },
                    // Both from the attempt clock: measured from send start,
                    // like the trace timings. Measuring from `request_start`
                    // (taken once headers arrive) read 0 ms for TTFT.
                    ttft_ms: clock.first_model_event_ms(),
                    total_ms: clock.stream_end_or_now_ms(),
                    stop_reason: state.telem_stop_reason,
                    cost_usd: state.telem_usage.cost_usd(model),
                    usage: state.telem_usage,
                    ratelimit: telem_ratelimit,
                    cache_diag: server_diagnosis.map(|d| telemetry::CacheDiagRecord {
                        miss_reason: d.reason.as_str().to_string(),
                        missed_tokens: d.missed_input_tokens,
                    }),
                    context: telemetry::ContextRecord {
                        messages: cleaned_messages.len(),
                        tools: tools_schema.len(),
                        system_bytes,
                        breakpoints,
                    },
                    keepwarm: matches!(
                        &options.keepwarm,
                        Some(KeepWarmRequest {
                            purpose: TailPurpose::Ping,
                            ..
                        })
                    ),
                    worker: options.codex_request_role
                        == crate::runtime::openai::catalog::CodexRequestRole::Worker,
                    rollover_window: options.context_rollover.map(|r| r.window),
                    rollover: options.context_rollover.is_some_and(|r| r.first_request),
                    rollover_summary: match &options.keepwarm {
                        Some(KeepWarmRequest {
                            purpose: TailPurpose::Summary { correlation, .. },
                            ..
                        }) => Some(telemetry::summary_id(*correlation)),
                        _ => None,
                    },
                    ..Default::default()
                };
                record.set_correlation(
                    options.trace.session_id(),
                    tracer.as_ref().map(|t| t.correlation_ids()),
                );
                // Non-blocking enqueue onto the session's bounded writer
                // (Task 11): overflow or a broken log path is counted by
                // the writer and can never delay or fail the request.
                if let Some(writer) = options.telemetry.as_ref() {
                    writer.enqueue_telemetry(record);
                }
            }

            let failure_label =
                stream_failure_label(stream_error.as_ref(), transport_error.as_ref());
            match classify_stream_outcome(
                stream_error,
                outcome_content,
                has_stop_reason,
                stop_reason_is_refusal,
                cancelled,
            ) {
                StreamOutcome::Done(v) => {
                    if let Some(t) = tracer.take() {
                        let terminal = if cancelled {
                            agent_core::TurnOutcome::Canceled
                        } else {
                            agent_core::TurnOutcome::Completed
                        };
                        t.finish(
                            clock,
                            Some(200),
                            trace_rid_stream,
                            trace_stop_reason,
                            trace_usage,
                            terminal,
                        );
                    }
                    return Ok(v);
                }
                StreamOutcome::Fail(msg) => {
                    stamp_failure(options, failure_label);
                    if let Some(t) = tracer.take() {
                        let terminal = t.failed_terminal(stream_error_code);
                        t.finish(
                            clock,
                            Some(200),
                            trace_rid_stream,
                            trace_stop_reason,
                            trace_usage,
                            terminal,
                        );
                    }
                    return Err(RuntimeError::ApiStatus(msg));
                }
                StreamOutcome::Retry(stream_error) => {
                    let msg = stream_error.message;
                    // Anthropic OAuth overloads need the same persistent retry
                    // posture as Claude Code (10 retries), not the generic
                    // three-attempt server-error budget. Other transient stream
                    // failures continue to use the configured shared budget.
                    let is_overloaded =
                        stream_error.error_type.as_deref() == Some("overloaded_error");
                    let (retry_budget, base_delay_ms) = stream_retry_policy(
                        &auth_type,
                        stream_error.error_type.as_deref(),
                        max_retries,
                    );
                    let retries_used = if auth_type == "oauth" && is_overloaded {
                        overload_attempts
                    } else {
                        non_429_attempts
                    };
                    if retries_used >= retry_budget
                        || cancel.is_cancelled()
                        || options.keepwarm.is_some()
                    {
                        // Budget exhausted (or user cancelled) — surface terminally
                        // rather than silently. Still loud, never the silent stop.
                        stamp_failure(options, failure_label);
                        if let Some(t) = tracer.take() {
                            let terminal = t.failed_terminal(stream_error_code);
                            t.finish(
                                clock,
                                Some(200),
                                trace_rid_stream,
                                trace_stop_reason,
                                trace_usage,
                                terminal,
                            );
                        }
                        if let Some(error) = transport_error {
                            warn_transport!(
                                TransportDiag::from_runtime(&error, failure_label),
                                retries_used.saturating_add(1),
                                retry_budget,
                                error = %error.user_message(),
                                "API stream network retries exhausted"
                            );
                            return Err(error);
                        }
                        return Err(RuntimeError::ApiStatus(msg));
                    }
                    let retry_index = retries_used.saturating_add(1);
                    let delay = Duration::from_millis(
                        base_delay_ms * 2u64.pow(retry_index.saturating_sub(1).min(6)),
                    );
                    if auth_type == "oauth" && is_overloaded {
                        overload_attempts += 1;
                    } else {
                        non_429_attempts += 1;
                    }
                    attempt += 1;
                    last_err = msg.clone();
                    last_status = None;
                    if let Some(error) = transport_error.as_ref() {
                        warn_transport!(
                            TransportDiag::from_runtime(error, failure_label),
                            retry_index,
                            retry_budget,
                            "in-stream API error, retrying {}/{} after {:?}: {}",
                            retry_index,
                            retry_budget,
                            delay,
                            msg
                        );
                    } else {
                        tracing::warn!(
                            "in-stream API error, retrying {}/{} after {:?}: {}",
                            retry_index,
                            retry_budget,
                            delay,
                            msg
                        );
                    }
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Retry(format!(
                        "⏳ API stream error — retrying ({}/{})…",
                        retry_index, retry_budget
                    ))));
                    if let Some(t) = tracer.as_mut() {
                        t.attempt_failed(
                            clock,
                            crate::runtime::trace::anthropic::retry_class_for_stream_error(
                                stream_error.error_type.as_deref(),
                            ),
                            delay,
                            Some(200),
                            trace_rid_stream,
                            stream_error_code,
                        );
                    }
                    if !options.suppress_stream_deltas {
                        let _ = tx.send(StreamEvent::Llm(LlmEvent::ResponseReset));
                    }
                    await_or_cancel(cancel, tokio::time::sleep(delay)).await?;
                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Canceled);
                    }
                    // fall through to the outer `loop` head: rebuild request + re-stream.
                }
                StreamOutcome::Refusal => {
                    // Model declined the request (stop_reason=refusal). Partial
                    // content was already discarded by std::mem::take in classify.
                    // Check budget before sleeping so an immediate exhaustion is
                    // surfaced without a pointless delay.
                    if refusal_attempts >= refusal_retries || cancel.is_cancelled() {
                        stamp_failure(options, REFUSAL_EXIT);
                        if let Some(t) = tracer.take() {
                            let terminal = t.failed_terminal("refusal");
                            t.finish(
                                clock,
                                Some(200),
                                trace_rid_stream,
                                trace_stop_reason,
                                trace_usage,
                                terminal,
                            );
                        }
                        let msg = format!(
                            "⚠ model refused the request ({} attempt{})",
                            refusal_attempts + 1,
                            if refusal_attempts == 0 { "" } else { "s" }
                        );
                        // Surface visible inline notice to TUI (same channel as
                        // rate-limit notices — renders as a turn-end inline message).
                        let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(msg.clone())));
                        // Hard-terminate so the turn never silently ends.
                        return Err(RuntimeError::ApiStatus(msg));
                    }
                    refusal_attempts += 1;
                    attempt += 1;
                    // Fixed 1s delay — refusals are not transient load, exponential
                    // backoff doesn't help. Separate budget from the error budget.
                    let delay = Duration::from_secs(1);
                    tracing::warn!(
                        "stop_reason=refusal — retrying {}/{} after {:?}",
                        refusal_attempts,
                        refusal_retries,
                        delay
                    );
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Retry(format!(
                        "⏳ model refusal — retrying ({}/{})…",
                        refusal_attempts, refusal_retries
                    ))));
                    if let Some(t) = tracer.as_mut() {
                        t.attempt_failed(
                            clock,
                            crate::runtime::trace::RetryClass::Other,
                            delay,
                            Some(200),
                            trace_rid_stream,
                            "refusal",
                        );
                    }
                    if !options.suppress_stream_deltas {
                        let _ = tx.send(StreamEvent::Llm(LlmEvent::ResponseReset));
                    }
                    await_or_cancel(cancel, tokio::time::sleep(delay)).await?;
                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Canceled);
                    }
                    // fall through to outer `loop` head — fresh request, fresh ParseState
                }
            }
        } // end UNIFIED RETRY LOOP (task #130)
    }
}

/// Transports other than native Anthropic (cloud brokers, OpenAI-compatible
/// providers) emit `SessionEvent::Usage` but do not book the turn's
/// [`UsageCounters`](crate::runtime::budget::UsageCounters). Book their Usage
/// events on the way through, priced for `model`, so the turn's cost and
/// token budgets bind on every provider. Events keep their order.
pub(crate) async fn with_usage_metering<T, Fut>(
    model: &str,
    counters: Option<Arc<crate::runtime::budget::UsageCounters>>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    route: impl FnOnce(mpsc::UnboundedSender<StreamEvent>) -> Fut,
) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let Some(counters) = counters else {
        return route(tx.clone()).await;
    };
    let forward = move |counters: &crate::runtime::budget::UsageCounters,
                        model: &str,
                        tx: &mpsc::UnboundedSender<StreamEvent>,
                        event: StreamEvent| {
        if let StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            ..
        }) = &event
        {
            counters.record_priced(
                model,
                *input_tokens,
                *output_tokens,
                *cache_read_input_tokens,
                *cache_creation_input_tokens,
                *cache_creation_5m,
                *cache_creation_1h,
            );
        } else if let StreamEvent::Session(SessionEvent::UsageAttempt { usage, .. }) = &event {
            // A ledgered attempt (§6) reports in place of the plain usage
            // above, never as well: price it the same way.
            counters.record_priced(
                model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_creation_5m,
                usage.cache_creation_1h,
            );
        }
        let _ = tx.send(event);
    };
    let (route_tx, mut route_rx) = mpsc::unbounded_channel();
    let routed = route(route_tx);
    tokio::pin!(routed);
    let result = loop {
        tokio::select! {
            biased;
            Some(event) = route_rx.recv() => forward(&counters, model, tx, event),
            result = &mut routed => break result,
        }
    };
    // Drain what this request sent before returning, so its usage is booked
    // before the stream loop checks the turn budget. Normally the route's
    // sender is gone and this ends at once; a sender clone still held by a
    // provider task winding down gets a short, bounded wait.
    let drained = tokio::time::timeout(Duration::from_millis(200), async {
        while let Some(event) = route_rx.recv().await {
            forward(&counters, model, tx, event);
        }
    })
    .await
    .is_ok();
    if drained {
        return result;
    }
    // A sender that outlives even that must not lose its events.
    let tx = tx.clone();
    let model = model.to_string();
    tokio::spawn(async move {
        while let Some(event) = route_rx.recv().await {
            forward(&counters, &model, &tx, event);
        }
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::telemetry::TelemetryLevel;

    #[tokio::test]
    async fn non_anthropic_usage_is_booked_into_the_turn_counters() {
        let counters = Arc::new(crate::runtime::budget::UsageCounters::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = with_usage_metering("gpt-5", Some(Arc::clone(&counters)), &tx, |route_tx| async move {
            let _ = route_tx.send(StreamEvent::Llm(LlmEvent::Text("hi".into())));
            let _ = route_tx.send(StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 1_000,
                output_tokens: 200,
                cache_read_input_tokens: 50_000,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: None,
            }));
            7
        })
        .await;
        assert_eq!(out, 7);
        assert_eq!(counters.records(), 1);
        assert_eq!(counters.totals(), (1_000, 200, 50_000, 0));
        let expected = agent_core::pricing::calculate_cost_optional_split(
            "gpt-5", 1_000, 200, 50_000, 0, None, None,
        );
        assert!((counters.cost_usd() - expected).abs() < 1e-6);
        // Events still reach the caller, in order.
        assert!(matches!(rx.recv().await, Some(StreamEvent::Llm(LlmEvent::Text(_)))));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Session(SessionEvent::Usage { .. }))
        ));
    }

    /// A failed keep-warm request names its exit with one of our static
    /// labels, never the provider's bytes.
    #[test]
    fn failure_labels_name_the_exit() {
        let frame = |kind: Option<&str>| StreamError {
            error_type: kind.map(str::to_owned),
            message: String::new(),
            retryable: true,
        };
        let idle = RuntimeError::NetworkTimeout("waiting for response body bytes");
        let broken = RuntimeError::Tool("connection reset".into());
        for (error, transport, label) in [
            (Some(frame(Some("overloaded_error"))), None, "overloaded_error"),
            (Some(frame(Some("api_error"))), None, "api_error"),
            (Some(frame(Some("ECHOED-PROVIDER-TEXT"))), None, "unrecognized_error"),
            (Some(frame(None)), None, "unrecognized_error"),
            (Some(frame(None)), Some(&idle), "idle_timeout"),
            (Some(frame(None)), Some(&broken), "transport"),
            (None, None, "empty_response"),
        ] {
            assert_eq!(stream_failure_label(error.as_ref(), transport), label);
        }
        assert_eq!(send_failure_label(&idle), "header_timeout");
        assert_eq!(send_failure_label(&broken), "transport");
    }

    /// Test harness: fresh ParseState + unbounded channel + EventCtx (Full
    /// telemetry so capture paths run). Returns (state, rx, ctx-parts).
    fn harness() -> (
        ParseState,
        mpsc::UnboundedSender<StreamEvent>,
        mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ParseState::new(), tx, rx)
    }

    fn make_ctx(tx: &mpsc::UnboundedSender<StreamEvent>) -> EventCtx<'_> {
        EventCtx {
            model: "claude-sonnet-4-5",
            tx,
            suppress_stream_deltas: false,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::FiveMinutes,
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            request_has_1h_marker: true,
            usage_counters: None,
            cache_diagnosis: false,
            spend: crate::runtime::child_spend::AttemptObserver::inert(),
            usage_request: None,
        }
    }

    #[test]
    fn suppressed_sync_context_accumulates_text_without_emitting_deltas() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = ParseState::new();
        let mut ctx = make_ctx(&tx);
        ctx.suppress_stream_deltas = true;
        process_data_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"large-final-text"}}"#,
            &mut state,
            &ctx,
        );
        assert_eq!(state.current_text, "large-final-text");
        assert!(rx.try_recv().is_err());
    }

    /// Telemetry-OFF ctx — the DEFAULT runtime config. Used to prove parse-time
    /// behavior doesn't silently depend on telemetry being enabled.
    fn make_ctx_telemetry_off(tx: &mpsc::UnboundedSender<StreamEvent>) -> EventCtx<'_> {
        EventCtx {
            model: "claude-sonnet-4-5",
            tx,
            suppress_stream_deltas: false,
            telemetry_level: TelemetryLevel::Off,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::FiveMinutes,
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            request_has_1h_marker: true,
            usage_counters: None,
            cache_diagnosis: false,
            spend: crate::runtime::child_spend::AttemptObserver::inert(),
            usage_request: None,
        }
    }

    /// Harness variant with a configured TTL + shared latches — for the
    /// silent-downgrade detector tests.
    fn make_ctx_ttl<'a>(
        tx: &'a mpsc::UnboundedSender<StreamEvent>,
        ttl: crate::core::config::CacheTtl,
        latch: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        honored: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> EventCtx<'a> {
        EventCtx {
            model: "claude-sonnet-4-5",
            tx,
            suppress_stream_deltas: false,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: ttl,
            ttl_downgrade_notified: latch.clone(),
            saw_1h_honored: honored.clone(),
            // Default armed: most detector tests model a request that DID
            // carry a 1h marker. The no-marker test overrides this.
            request_has_1h_marker: true,
            usage_counters: None,
            cache_diagnosis: false,
            spend: crate::runtime::child_spend::AttemptObserver::inert(),
            usage_request: None,
        }
    }

    fn feed(lines: &[&str], state: &mut ParseState, ctx: &EventCtx) {
        for line in lines {
            process_data_line(line, state, ctx);
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn anthropic_oauth_overload_uses_claude_code_retry_posture() {
        assert_eq!(
            stream_retry_policy("oauth", Some("overloaded_error"), 3),
            (10, 500)
        );
    }

    #[test]
    fn api_key_overload_keeps_configured_retry_posture() {
        assert_eq!(
            stream_retry_policy("api_key", Some("overloaded_error"), 3),
            (3, 1000)
        );
    }

    #[test]
    fn oauth_non_overload_keeps_configured_retry_posture() {
        assert_eq!(
            stream_retry_policy("oauth", Some("api_error"), 3),
            (3, 1000)
        );
    }

    #[test]
    fn text_deltas_accumulate_then_flush_on_block_stop() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, "}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.accumulated_content.len(), 1);
        assert_eq!(
            state.accumulated_content[0],
            json!({"type":"text","text":"Hello, world"})
        );
        assert!(state.current_text.is_empty());
        let events = drain(&mut rx);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello, ", "world"]);
    }

    #[test]
    fn second_text_block_start_flushes_prior_text() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"first"}}"#,
                // New text block starts while current_text is non-empty —
                // the L312–320 branch flushes the prior text block.
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"second"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.accumulated_content.len(), 2);
        assert_eq!(
            state.accumulated_content[0],
            json!({"type":"text","text":"first"})
        );
        assert_eq!(
            state.accumulated_content[1],
            json!({"type":"text","text":"second"})
        );
    }

    #[test]
    fn tool_use_full_lifecycle() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Tokyo\"}"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(!state.in_tool_use, "flag must clear on block_stop");
        assert_eq!(state.accumulated_content.len(), 1);
        assert_eq!(
            state.accumulated_content[0],
            json!({"type":"tool_use","id":"toolu_01","name":"get_weather","input":{"city":"Tokyo"}})
        );
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id })
                if tool_name == "get_weather" && tool_id == "toolu_01"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, .. }) if tool_id == "toolu_01"
        ));
        assert!(matches!(
            events.last().unwrap(),
            StreamEvent::Llm(LlmEvent::ToolUse { tool_name, input, .. })
                if tool_name == "get_weather" && input == &json!({"city":"Tokyo"})
        ));
    }

    #[test]
    fn tool_use_invalid_json_yields_parse_error_object() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_02","name":"run"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\": truncated"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        let input = &state.accumulated_content[0]["input"];
        let err = input["__parse_error"]
            .as_str()
            .expect("__parse_error key present");
        assert!(err.starts_with("invalid tool input JSON:"));
    }

    #[test]
    fn tool_use_empty_input_yields_empty_object() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_03","name":"noop"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.accumulated_content[0]["input"], json!({}));
    }

    #[test]
    fn thinking_lifecycle_with_signature() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(!state.in_thinking);
        assert_eq!(
            state.accumulated_content[0],
            json!({"type":"thinking","thinking":"pondering","signature":"sig_abc"})
        );
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            StreamEvent::Llm(LlmEvent::Thinking(t)) if t == "pondering"
        ));
    }

    /// A block with neither text nor signature is never emitted: Anthropic
    /// rejects it next turn ("each thinking block must contain thinking").
    #[test]
    fn empty_thinking_block_never_emitted() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(state.accumulated_content.is_empty());
        assert!(!state.in_thinking);
    }

    /// `display: "omitted"`: the block streams one empty `thinking_delta`
    /// and its signature. It goes into the history as the API sent it —
    /// empty text, the signature carrying the reasoning — ahead of the text,
    /// and the empty delta still reaches the client (the TUI's cue that a
    /// thought happened). The end-of-stream flush keeps one cut off before
    /// its stop the same way.
    #[test]
    fn omitted_thinking_block_round_trips_with_its_signature() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_only"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            state.accumulated_content,
            vec![
                json!({"type":"thinking","thinking":"","signature":"sig_only"}),
                json!({"type":"text","text":"hi"}),
            ]
        );
        let events = drain(&mut rx);
        assert!(matches!(&events[0], StreamEvent::Llm(LlmEvent::Thinking(t)) if t.is_empty()));

        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_cut"}}"#,
            ],
            &mut state,
            &ctx,
        );
        state.finalize();
        assert_eq!(
            state.accumulated_content,
            vec![json!({"type":"thinking","thinking":"","signature":"sig_cut"})]
        );
    }

    #[test]
    fn message_delta_captures_usage_stop_reason_telemetry() {
        // Future-proof path: if a delta ever carries its own cache_creation
        // sub-object, it takes precedence over the message_start capture.
        // (Live deltas don't — see live_split_from_start_survives_to_delta_emission.)
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":300,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":60,"ephemeral_1h_input_tokens":40}}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.telem_stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(state.telem_usage.input, 100);
        assert_eq!(state.telem_usage.output, 50);
        assert_eq!(state.telem_usage.cache_read, 300);
        assert_eq!(state.telem_usage.cache_write, 100);
        assert_eq!(state.telem_usage.cache_write_5m, Some(60));
        assert_eq!(state.telem_usage.cache_write_1h, Some(40));
        // hit_pct = 300 / (100 + 300 + 100) * 100 = 60.0
        assert_eq!(state.telem_usage.hit_pct, 60.0);
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 300,
                cache_creation_input_tokens: 100,
                cache_creation_5m: Some(60),
                cache_creation_1h: Some(40),
                model: None
            })
        ));
    }

    #[test]
    fn live_split_from_start_survives_to_delta_emission() {
        // LIVE API shape (streaming probe): message_start carries the full
        // cache_creation split; message_delta carries ONLY the aggregate.
        // Regression: reading the split exclusively in the delta arm made it
        // permanently None in live traffic — telemetry lost the 5m/1h keys
        // and the downgrade detector could never latch or fire.
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_live","usage":{"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":1282,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":1277}}}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":4,"output_tokens":42,"cache_read_input_tokens":0,"cache_creation_input_tokens":1282}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
            &mut state,
            &ctx,
        );
        // Telemetry carries the split captured at message_start.
        assert_eq!(state.telem_usage.cache_write, 1282);
        assert_eq!(state.telem_usage.cache_write_5m, Some(5));
        assert_eq!(state.telem_usage.cache_write_1h, Some(1277));
        // The one Usage emission (message_stop, final counts) carries the split too.
        let events = drain(&mut rx);
        let last_usage = events
            .iter()
            .rfind(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .expect("message_stop must emit the Usage event");
        assert!(matches!(
            last_usage,
            StreamEvent::Session(SessionEvent::Usage {
                output_tokens: 42,
                cache_creation_input_tokens: 1282,
                cache_creation_5m: Some(5),
                cache_creation_1h: Some(1277),
                ..
            })
        ));
    }

    #[test]
    fn message_start_captures_msg_id_and_usage() {
        // CAPTURE ONLY: message_start must emit NO Usage event — live deltas
        // repeat all aggregates, so emitting from both arms double-billed
        // input/cache in every accumulating consumer (pre-cache-ttl bug).
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_xyz","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.telem_msg_id.as_deref(), Some("msg_xyz"));
        assert_eq!(state.msg_start_input, Some(10));
        assert_eq!(state.msg_start_output, Some(1));
        assert_eq!(state.msg_start_cache_read, Some(7));
        assert_eq!(state.msg_start_cache_create, Some(3));
        assert!(!state.usage_emitted);
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. }))),
            "message_start must capture only — emitting here double-counts"
        );
    }

    // ── One-emission invariant (the double-billing regression pin) ──────────

    /// Exact live API shapes (probed): message_start carries the split +
    /// aggregates + output placeholder; message_delta repeats ALL aggregates
    /// with the final output and NO split sub-object.
    const PROBED_LIVE_START: &str = r#"data: {"type":"message_start","message":{"id":"msg_probe","usage":{"input_tokens":3,"cache_creation_input_tokens":1103,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":6,"ephemeral_1h_input_tokens":1097},"output_tokens":1}}}"#;
    const PROBED_LIVE_DELTA: &str = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":3,"cache_creation_input_tokens":1103,"cache_read_input_tokens":0,"output_tokens":11}}"#;
    const LIVE_STOP: &str = r#"data: {"type":"message_stop"}"#;

    #[test]
    fn one_usage_emission_per_request_live_shapes() {
        // THE invariant: exactly ONE SessionEvent::Usage per request. Every
        // downstream consumer accumulates — two emissions = ~2x billing on
        // input/cache_read/cache_creation. This bug PREDATES cache-ttl
        // (verified at ffa83a8).
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&[PROBED_LIVE_START, PROBED_LIVE_DELTA], &mut state, &ctx);
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. }))),
            "a delta's counts may be replaced by a later delta's: nothing is emitted before message_stop"
        );
        feed(&[LIVE_STOP], &mut state, &ctx);
        let events = drain(&mut rx);
        let usages: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .collect();
        assert_eq!(usages.len(), 1, "exactly ONE Usage event per request");
        assert!(matches!(
            usages[0],
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 3,
                output_tokens: 11, // delta's FINAL output, not start's placeholder
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 1103,
                cache_creation_5m: Some(6),    // start-captured split
                cache_creation_1h: Some(1097), // survives to the delta emission
                model: None,
            })
        ));
        // The stream-end call must be a no-op after message_stop emitted.
        emit_final_usage(&mut state, &ctx);
        assert!(
            drain(&mut rx).is_empty(),
            "the stream-end emission must be a no-op once message_stop emitted"
        );
    }

    #[test]
    fn priced_usage_books_final_or_residual_once_with_request_model_and_ttl() {
        for final_delta in [false, true] {
            for model in ["claude-opus-4.1", "claude-opus-4.8", "claude-haiku-5.5"] {
                let (mut state, tx, mut rx) = harness();
                let counters =
                    std::sync::Arc::new(crate::runtime::budget::UsageCounters::default());
                let mut ctx = make_ctx_telemetry_off(&tx);
                ctx.model = model;
                ctx.usage_counters = Some(counters.clone());
                feed(&[PROBED_LIVE_START], &mut state, &ctx);
                assert_eq!(counters.cost_usd(), 0.0);
                if final_delta {
                    feed(&[PROBED_LIVE_DELTA], &mut state, &ctx);
                }
                emit_final_usage(&mut state, &ctx);
                emit_final_usage(&mut state, &ctx);
                let output = if final_delta { 11 } else { 1 };
                let expected =
                    agent_core::pricing::calculate_cost_split(model, 3, output, 0, 6, 1097);
                assert!((counters.cost_usd() - expected).abs() < 2e-9);
                assert_eq!(counters.totals(), (3, output, 0, 1103));
                assert_eq!(counters.latest_context_tokens(), 1106);
                assert_eq!(
                    drain(&mut rx)
                        .iter()
                        .filter(|event| matches!(
                            event,
                            StreamEvent::Session(SessionEvent::Usage { .. })
                        ))
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn partial_ttl_final_and_residual_usage_agree_with_display_cost_once() {
        for final_delta in [false, true] {
            for (input, aggregate, known5) in [(0, 100_001, 0), (60_000, 40_001, 10_000)] {
                let (mut state, tx, mut rx) = harness();
                let counters =
                    std::sync::Arc::new(crate::runtime::budget::UsageCounters::default());
                let mut ctx = make_ctx_telemetry_off(&tx);
                ctx.model = "claude-haiku-5.5";
                ctx.usage_counters = Some(counters.clone());
                let start = format!(
                    "data: {}",
                    json!({
                        "type": "message_start", "message": {"id": "msg_partial", "usage": {
                            "input_tokens": input, "output_tokens": 0,
                            "cache_creation_input_tokens": aggregate,
                            "cache_creation": {"ephemeral_5m_input_tokens": known5}
                        }}
                    })
                );
                feed(&[start.as_str()], &mut state, &ctx);
                assert_eq!(counters.cost_usd(), 0.0);
                if final_delta {
                    feed(
                        &[
                            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}}"#,
                        ],
                        &mut state,
                        &ctx,
                    );
                    assert!(
                        state.final_usage.is_some(),
                        "the final delta's counts are kept for the one emission"
                    );
                }
                emit_final_usage(&mut state, &ctx);
                emit_final_usage(&mut state, &ctx);
                let expected = if input == 0 { 0.100001 } else { 0.066251 };
                assert!((counters.cost_usd() - expected).abs() < 2e-9);
                assert_eq!(counters.totals(), (input, 0, 0, aggregate));
                let events = drain(&mut rx);
                let mut usage_count = 0;
                for event in events {
                    if let StreamEvent::Session(SessionEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                        cache_creation_5m,
                        cache_creation_1h,
                        ..
                    }) = event
                    {
                        usage_count += 1;
                        assert_eq!(cache_creation_5m, Some(known5));
                        assert_eq!(cache_creation_1h, None);
                        let display_cost = agent_core::pricing::calculate_cost_optional_split(
                            ctx.model,
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            cache_creation_5m,
                            cache_creation_1h,
                        );
                        assert!((display_cost - expected).abs() < 1e-12);
                    }
                }
                assert_eq!(usage_count, 1);
            }
        }
    }

    #[test]
    fn dead_stream_residual_bills_start_capture() {
        // Stream death between message_start and message_delta: the API
        // already processed the input — bill what start reported, once.
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&[PROBED_LIVE_START], &mut state, &ctx);
        assert!(drain(&mut rx)
            .iter()
            .all(|e| !matches!(e, StreamEvent::Session(SessionEvent::Usage { .. }))));
        emit_final_usage(&mut state, &ctx);
        let events = drain(&mut rx);
        let usages: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .collect();
        assert_eq!(usages.len(), 1, "dead stream bills exactly once");
        assert!(matches!(
            usages[0],
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 3,
                output_tokens: 1, // start's placeholder — all we ever saw
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 1103,
                cache_creation_5m: Some(6),
                cache_creation_1h: Some(1097),
                model: None,
            })
        ));
        // Idempotent: a second residual call emits nothing.
        emit_final_usage(&mut state, &ctx);
        assert!(drain(&mut rx).is_empty(), "residual must be idempotent");
    }

    #[test]
    fn dead_stream_before_delta_still_latches_1h_honored() {
        // B3: turn 1 writes the 1h prefix, stream dies before the delta. The
        // latch must be set at message_start, or a later healthy turn
        // (1h == 0, prefix cached) triggers a false downgrade notice.
        let (mut state, tx, _rx) = harness();
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(
            &tx,
            crate::core::config::CacheTtl::Hybrid,
            &notified,
            &honored,
        );
        feed(&[HONORED_START], &mut state, &ctx); // no delta — stream died
        assert!(
            honored.load(std::sync::atomic::Ordering::Relaxed),
            "saw_1h_honored must latch at message_start, not only at the delta"
        );
    }

    #[test]
    fn all_zero_usage_emits_no_event() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_zero","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
            ],
            &mut state,
            &ctx,
        );
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. }))),
            "all-zero usage must not emit a Usage event"
        );
        // stop_reason still captured — the gate only guards the Usage emit.
        assert_eq!(state.telem_stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn ttft_set_once_on_first_event() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        assert!(state.first_event_elapsed_ms.is_none());
        feed(
            &[r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#],
            &mut state,
            &ctx,
        );
        let first = state.first_event_elapsed_ms;
        assert!(first.is_some());
        assert!(state.first_event_seen);
        std::thread::sleep(std::time::Duration::from_millis(5));
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            state.first_event_elapsed_ms, first,
            "TTFT must not be overwritten by later events"
        );
    }

    /// A lone ping is a parsed frame (a later 429 must not hop accounts)
    /// but never progress: no event, no message_start.
    #[test]
    fn ping_sets_first_event_seen_but_is_not_progress() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&[r#"data: {"type":"ping"}"#], &mut state, &ctx);
        assert!(state.first_event_seen);
        assert_eq!(state.progress.pings, 1);
        assert_eq!(state.progress.events, 0);
        assert!(!state.progress.message_start_seen);
    }

    /// Regression test for the double-emit bug fixed in the slice-2 pre-work
    /// micro-commit: a content_block_stop arriving via the tail path (partial
    /// final line) must clear in_tool_use so finalize() cannot re-push.
    #[test]
    fn tail_path_then_finalize_no_double_emit() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        // Main loop: tool_use opened and input streamed.
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_tail","name":"ls"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            ],
            &mut state,
            &ctx,
        );
        // Tail path: final content_block_stop arrives as a partial last line
        // (no trailing newline) — same seam, same call.
        process_data_line(
            r#"data: {"type":"content_block_stop","index":0}"#,
            &mut state,
            &ctx,
        );
        // End-of-stream flush.
        state.finalize();

        let tool_blocks: Vec<&Value> = state
            .accumulated_content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .collect();
        assert_eq!(
            tool_blocks.len(),
            1,
            "tool_use block must be emitted exactly once"
        );
        let tool_events = drain(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, StreamEvent::Llm(LlmEvent::ToolUse { .. })))
            .count();
        assert_eq!(tool_events, 1);
    }

    #[test]
    fn finalize_flushes_partial_text() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"dangling"}}"#,
            ],
            &mut state,
            &ctx,
        );
        state.finalize();
        assert_eq!(
            state.accumulated_content,
            vec![json!({"type":"text","text":"dangling"})]
        );
    }

    #[test]
    fn finalize_flushes_partial_thinking() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"cut off"}}"#,
            ],
            &mut state,
            &ctx,
        );
        state.finalize();
        assert_eq!(
            state.accumulated_content,
            vec![json!({"type":"thinking","thinking":"cut off","signature":""})]
        );

        // Empty-thinking suppression in finalize too: open block, no text.
        let (mut state2, tx2, _rx2) = harness();
        let ctx2 = make_ctx(&tx2);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ],
            &mut state2,
            &ctx2,
        );
        state2.finalize();
        assert!(
            state2.accumulated_content.is_empty(),
            "empty thinking must be suppressed"
        );
    }

    #[test]
    fn finalize_flushes_partial_tool() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_cut","name":"grep"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"x\"}"}}"#,
            ],
            &mut state,
            &ctx,
        );
        state.finalize();
        assert_eq!(
            state.accumulated_content,
            vec![json!({"type":"tool_use","id":"toolu_cut","name":"grep","input":{"pattern":"x"}})]
        );
    }

    // ── take_outcome_content: cancel keeps only verifiable model output ──

    const THINK_START: &str = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#;
    const THINK_DELTA: &str = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing it"}}"#;
    const THINK_SIG: &str = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_ok"}}"#;
    const STOP0: &str = r#"data: {"type":"content_block_stop","index":0}"#;
    const TEXT_START1: &str = r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#;
    const TEXT_DELTA1: &str = r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me check"}}"#;
    const STOP1: &str = r#"data: {"type":"content_block_stop","index":1}"#;
    const TOOL_START2: &str = r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_done","name":"grep"}}"#;
    const TOOL_DELTA2: &str = r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"x\"}"}}"#;
    const STOP2: &str = r#"data: {"type":"content_block_stop","index":2}"#;
    const TOOL_START3: &str = r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_cut","name":"bash"}}"#;
    const TOOL_DELTA3: &str = r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"rm -"}}"#;

    fn outcome_after(lines: &[&str], cancelled: bool) -> Vec<Value> {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(lines, &mut state, &ctx);
        state.finalize();
        state.take_outcome_content(cancelled)
    }

    #[test]
    fn unsigned_thinking_replay_is_not_retained_after_uncancelled_eof() {
        let content = outcome_after(&[THINK_START, THINK_DELTA], false);
        assert!(content.is_empty());
        assert!(matches!(
            classify_stream_outcome(None, content, false, false, false),
            StreamOutcome::Fail(_)
        ));
    }

    #[test]
    fn unsigned_thinking_replay_removes_only_unsigned_blocks_at_eof() {
        let content = outcome_after(
            &[
                THINK_START,
                THINK_DELTA,
                THINK_SIG,
                STOP0,
                TEXT_START1,
                TEXT_DELTA1,
                STOP1,
                TOOL_START2,
                TOOL_DELTA2,
                STOP2,
                THINK_START,
                THINK_DELTA,
            ],
            false,
        );
        assert_eq!(
            content,
            vec![
                json!({"type":"thinking","thinking":"weighing it","signature":"sig_ok"}),
                json!({"type":"text","text":"Let me check"}),
                json!({"type":"tool_use","id":"toolu_done","name":"grep","input":{"pattern":"x"}}),
            ]
        );
    }

    #[test]
    fn cancelled_outcome_keeps_partial_text_verbatim() {
        let content = outcome_after(&[TEXT_START1, TEXT_DELTA1], true);
        assert_eq!(content, vec![json!({"type":"text","text":"Let me check"})]);
    }

    #[test]
    fn cancelled_outcome_drops_unsigned_thinking() {
        let content = outcome_after(&[THINK_START, THINK_DELTA], true);
        assert!(
            content.is_empty(),
            "unsigned thinking must not survive a cancel: {content:?}"
        );
    }

    #[test]
    fn cancelled_outcome_keeps_signed_thinking_and_completed_blocks() {
        let content = outcome_after(
            &[
                THINK_START,
                THINK_DELTA,
                THINK_SIG,
                STOP0,
                TEXT_START1,
                TEXT_DELTA1,
                STOP1,
            ],
            true,
        );
        assert_eq!(
            content,
            vec![
                json!({"type":"thinking","thinking":"weighing it","signature":"sig_ok"}),
                json!({"type":"text","text":"Let me check"}),
            ]
        );
    }

    #[test]
    fn cancelled_outcome_drops_only_the_in_flight_tool_call() {
        let content = outcome_after(
            &[
                TEXT_START1,
                TEXT_DELTA1,
                STOP1,
                TOOL_START2,
                TOOL_DELTA2,
                STOP2,
                TOOL_START3,
                TOOL_DELTA3,
            ],
            true,
        );
        assert_eq!(
            content,
            vec![
                json!({"type":"text","text":"Let me check"}),
                json!({"type":"tool_use","id":"toolu_done","name":"grep","input":{"pattern":"x"}}),
            ],
            "the completed call stays; the half-streamed one (truncated JSON) is dropped"
        );
    }

    #[test]
    fn uncancelled_outcome_keeps_signed_thinking_and_existing_tool_semantics() {
        // Replayable thinking and the existing non-cancel partial-tool
        // semantics stay unchanged; unsigned thinking is tested separately.
        let lines = [
            THINK_START, THINK_DELTA, THINK_SIG, STOP0, TOOL_START3, TOOL_DELTA3,
        ];
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&lines, &mut state, &ctx);
        state.finalize();
        let expected = state.accumulated_content.clone();
        assert_eq!(
            expected.len(),
            2,
            "fixture: signed thinking + partial tool"
        );
        assert_eq!(state.take_outcome_content(false), expected);
    }

    #[test]
    fn finalize_is_idempotent() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_i","name":"once"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            ],
            &mut state,
            &ctx,
        );
        state.finalize();
        let after_first = state.accumulated_content.clone();
        state.finalize();
        assert_eq!(
            state.accumulated_content, after_first,
            "second finalize must be a no-op"
        );

        // Same for partial text.
        let (mut state2, tx2, _rx2) = harness();
        let ctx2 = make_ctx(&tx2);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"t"}}"#,
            ],
            &mut state2,
            &ctx2,
        );
        state2.finalize();
        state2.finalize();
        assert_eq!(state2.accumulated_content.len(), 1);
    }

    #[test]
    fn done_marker_and_non_data_lines_skipped() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                "data: [DONE]",
                ": keepalive",
                "event: foo",
                "",
                "data: not json at all",
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.current_text, "ok");
        let events = drain(&mut rx);
        assert_eq!(
            events.len(),
            1,
            "only the valid data line may produce events"
        );
    }

    // ───────────────────── slice 3: typed-path additions ─────────────────────

    /// Tuple of every observable ParseState field, in declaration order:
    /// (accumulated_content, current_text, current_tool_name, current_tool_id,
    /// current_tool_input_json, in_tool_use, current_thinking,
    /// current_thinking_signature, in_thinking, telem_msg_id, telem_stop_reason).
    type StateSnapshot = (
        Vec<Value>,
        String,
        String,
        String,
        String,
        bool,
        String,
        String,
        bool,
        Option<String>,
        Option<String>,
    );

    /// Snapshot of every observable ParseState field, for bit-identical
    /// no-state-change assertions.
    fn snapshot(s: &ParseState) -> StateSnapshot {
        (
            s.accumulated_content.clone(),
            s.current_text.clone(),
            s.current_tool_name.clone(),
            s.current_tool_id.clone(),
            s.current_tool_input_json.clone(),
            s.in_tool_use,
            s.current_thinking.clone(),
            s.current_thinking_signature.clone(),
            s.in_thinking,
            s.telem_msg_id.clone(),
            s.telem_stop_reason.clone(),
        )
    }

    #[test]
    fn unknown_event_type_no_state_change() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        // Establish some non-trivial state first.
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pre"}}"#,
            ],
            &mut state,
            &ctx,
        );
        drain(&mut rx);
        let before = snapshot(&state);
        feed(
            &[
                r#"data: {"type":"ping"}"#,
                r#"data: {"type":"fnord","payload":[1,2,3]}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            snapshot(&state),
            before,
            "Unknown events must not mutate state"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "Unknown events must emit zero events"
        );
        assert!(
            state.stream_error.is_none(),
            "ping/unknown must not set a stream error"
        );
    }

    #[test]
    fn error_event_sets_stream_error() {
        // An in-stream Anthropic `error` event (200-status soft failure:
        // overloaded, context overflow) MUST be captured — not silently
        // dropped as an Unknown event. This is the root of task #130: a
        // dropped error event left the stream empty and the turn ended
        // silently. The captured message must name the (vetted) error type so
        // the surfaced error is actionable — but never the provider's raw
        // message, which is untrusted (spec §5.1).
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ],
            &mut state,
            &ctx,
        );
        let err = state
            .stream_error
            .expect("error event must set stream_error");
        assert!(
            err.message.contains("overloaded_error"),
            "must carry the error type: {}",
            err.message
        );
        assert!(
            !err.message.contains("Overloaded"),
            "must NOT carry the raw provider message (untrusted): {}",
            err.message
        );
        assert!(
            err.retryable,
            "overloaded_error is a transient/retryable class"
        );
    }

    #[test]
    fn error_event_without_payload_still_sets_stream_error() {
        // A bare `{"type":"error"}` with no inner payload must still register
        // a failure — an error event is never a no-op.
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&[r#"data: {"type":"error"}"#], &mut state, &ctx);
        assert!(
            state.stream_error.is_some(),
            "error event with no payload must still set a stream error"
        );
    }

    /// Phase 1 holdout regression — RED at HEAD: the SSE `error` event arm
    /// logged the raw frame (bounded 400 bytes) at WARN and copied the
    /// provider message verbatim into `stream_error.message` (→ retry
    /// warnings and the terminal error). GREEN: neither the captured message
    /// nor any log line carries the hostile body; the vetted error type
    /// survives for retry policy.
    #[test]
    fn hostile_sse_error_event_never_leaks_request_content() {
        const HOSTILE_SENTINEL: &str = "HOLDOUT-SENTINEL-http500-77aa";

        /// `std::io::Write` sink appending to a shared buffer.
        struct Cap(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Cap {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || Cap(sink.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        let hostile = format!(
            r#"data: {{"type":"error","error":{{"type":"overloaded_error","message":"ECHOED:{HOSTILE_SENTINEL} {{\"system\":[...],\"tools\":[{{\"input_schema\":{{}}}}]}}"}}}}"#
        );
        feed(&[hostile.as_str()], &mut state, &ctx);

        let err = state
            .stream_error
            .take()
            .expect("error event must be captured");
        {
            let (text, ctx_name) = (err.message.as_str(), "stream_error message");
            assert!(
                !text.contains(HOSTILE_SENTINEL),
                "{ctx_name}: sentinel leaked: {text}"
            );
            assert!(
                !text.contains("ECHOED") && !text.contains("input_schema"),
                "{ctx_name}: echoed request content leaked: {text}"
            );
        }
        assert_eq!(
            err.error_type.as_deref(),
            Some("overloaded_error"),
            "vetted type must survive for retry policy"
        );
        assert!(err.retryable, "overloaded_error stays retryable");

        // Hostile unknown SSE events must not leak via the forensics trace.
        let unknown = format!(r#"data: {{"type":"fnord","echo":"{HOSTILE_SENTINEL}"}}"#);
        feed(&[unknown.as_str()], &mut state, &ctx);

        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!logs.is_empty(), "expected SSE error warn to be logged");
        assert!(
            !logs.contains(HOSTILE_SENTINEL),
            "log sink leaked the sentinel: {logs}"
        );
        assert!(
            !logs.contains("ECHOED") && !logs.contains("input_schema"),
            "log sink leaked echoed request content: {logs}"
        );
    }

    #[test]
    fn stop_reason_seen_captured_even_with_telemetry_off() {
        // Regression (board review HIGH): classify_stream_outcome's
        // `has_stop_reason` must NOT depend on the telemetry flag. With telemetry
        // OFF (the default config), a message_delta carrying a stop_reason must
        // still set `stop_reason_seen` — otherwise a valid empty `end_turn` would
        // misclassify as a silent-stop Fail.
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx_telemetry_off(&tx);
        feed(
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(
            state.stop_reason_seen,
            "stop_reason must be captured with telemetry OFF"
        );
        assert!(
            state.telem_stop_reason.is_none(),
            "telem_stop_reason stays None when telemetry off — only the unconditional flag is set"
        );
    }

    #[test]
    fn context_overflow_error_is_terminal_not_retryable() {
        // `request_too_large` / `invalid_request_error` is the context-overflow
        // (#130) class — retrying never helps, so it must classify terminal.
        for kind in [
            "request_too_large",
            "invalid_request_error",
            "authentication_error",
        ] {
            let (mut state, tx, _rx) = harness();
            let ctx = make_ctx(&tx);
            let line =
                format!(r#"data: {{"type":"error","error":{{"type":"{kind}","message":"nope"}}}}"#);
            feed(&[line.as_str()], &mut state, &ctx);
            let err = state.stream_error.expect("error captured");
            assert!(!err.retryable, "{kind} must be terminal, not retryable");
        }
    }

    // ── classify_stream_outcome: the silent-stop decision (task #130) ──

    #[test]
    fn outcome_retries_on_transient_error_event() {
        let r = classify_stream_outcome(
            Some(StreamError {
                error_type: None,
                message: "overloaded".to_string(),
                retryable: true,
            }),
            vec![],
            false,
            false,
            false,
        );
        assert!(
            matches!(r, StreamOutcome::Retry(_)),
            "transient error must retry, got {r:?}"
        );
    }

    #[test]
    fn outcome_fails_on_terminal_error_event() {
        let r = classify_stream_outcome(
            Some(StreamError {
                error_type: None,
                message: "request_too_large".to_string(),
                retryable: false,
            }),
            vec![],
            false,
            false,
            false,
        );
        match r {
            StreamOutcome::Fail(m) => assert!(m.contains("request_too_large")),
            other => panic!("terminal error must Fail, got {other:?}"),
        }
    }

    #[test]
    fn outcome_fails_on_degenerate_empty_stream() {
        // Empty content + no stop_reason + not cancelled = the silent stop.
        let r = classify_stream_outcome(None, vec![], false, false, false);
        match r {
            StreamOutcome::Fail(m) => assert!(
                m.contains("context window") || m.contains("overloaded"),
                "error must be actionable: {m}"
            ),
            other => panic!("empty+no-stop must Fail, got {other:?}"),
        }
    }

    #[test]
    fn outcome_done_when_empty_but_cancelled() {
        // User cancellation legitimately yields empty content — not an error,
        // and never a retry (don't fight the user's cancel).
        let r = classify_stream_outcome(None, vec![], false, false, true);
        assert!(
            matches!(r, StreamOutcome::Done(_)),
            "cancellation is a clean Done"
        );

        // A cancel during a retryable error must ALSO downgrade to Done.
        let r2 = classify_stream_outcome(
            Some(StreamError {
                error_type: None,
                message: "overloaded".to_string(),
                retryable: true,
            }),
            vec![],
            false,
            false,
            true,
        );
        assert!(
            matches!(r2, StreamOutcome::Done(_)),
            "cancel beats a retryable error"
        );
    }

    #[test]
    fn outcome_done_when_empty_but_has_stop_reason() {
        // A real end_turn with no content (rare but valid — e.g. a pause turn)
        // carries a stop_reason and must pass through.
        let r = classify_stream_outcome(None, vec![], true, false, false);
        assert!(
            matches!(r, StreamOutcome::Done(_)),
            "empty-but-stop_reason is valid"
        );
    }

    #[test]
    fn outcome_done_passes_content_through() {
        let content = vec![json!({"type":"text","text":"hi"})];
        let r = classify_stream_outcome(None, content.clone(), true, false, false);
        match r {
            StreamOutcome::Done(v) => assert_eq!(v["content"], json!(content)),
            other => panic!("non-empty content is always Done, got {other:?}"),
        }
    }

    #[test]
    fn outcome_error_event_beats_nonempty_content() {
        // If the stream errored, surface/retry it even if partial content
        // accumulated — a partial answer after an error frame is not a turn.
        let content = vec![json!({"type":"text","text":"partial"})];
        let r = classify_stream_outcome(
            Some(StreamError {
                error_type: None,
                message: "boom".to_string(),
                retryable: false,
            }),
            content,
            true,
            false,
            false,
        );
        assert!(
            matches!(r, StreamOutcome::Fail(_)),
            "error event wins over partial content"
        );
    }

    #[test]
    fn malformed_json_line_skipped() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        let before = snapshot(&state);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_de"#, // truncated mid-string
                r#"data: {"#,
                r#"data: }{"#,
                "data: \u{1f4a5}", // raw non-JSON multi-byte
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            snapshot(&state),
            before,
            "malformed lines must be skipped without state change"
        );
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn multibyte_utf8_text_delta_end_to_end() {
        // Raw multi-byte (borrow fast path) and \uXXXX-escaped (owned path)
        // variants of the same text must produce byte-identical output
        // through the full seam.
        let expected = "✨ héllo";
        for data_line in [
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"✨ héllo\"}}",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"\u2728 h\u00e9llo"}}"#,
        ] {
            let (mut state, tx, mut rx) = harness();
            let ctx = make_ctx(&tx);
            feed(
                &[data_line, r#"data: {"type":"content_block_stop","index":0}"#],
                &mut state,
                &ctx,
            );
            assert_eq!(
                state.accumulated_content,
                vec![json!({"type":"text","text":expected})],
                "accumulated text must be byte-identical for {data_line}"
            );
            let events = drain(&mut rx);
            assert!(
                matches!(
                    &events[0],
                    StreamEvent::Llm(LlmEvent::Text(t)) if t.as_bytes() == expected.as_bytes()
                ),
                "emitted text must be byte-identical for {data_line}"
            );
        }
    }

    #[test]
    fn event_with_unknown_delta_subtype_ignored_gracefully() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"keep"}}"#,
            ],
            &mut state,
            &ctx,
        );
        drain(&mut rx);
        let before = snapshot(&state);
        feed(
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"x":1}}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            snapshot(&state),
            before,
            "unknown delta subtype must not mutate state"
        );
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn tail_partial_line_typed_parse() {
        // Tail path shape: take_remaining() yields an owned String the typed
        // event borrows from — the lifetime path slice 3 had to keep sound.
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ],
            &mut state,
            &ctx,
        );
        let remaining: String =
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tail ✨"}}"#
                .to_string();
        process_data_line(&remaining, &mut state, &ctx);
        drop(remaining); // event Cow must not outlive this — compile-time proof it didn't
        state.finalize();
        assert_eq!(
            state.accumulated_content,
            vec![json!({"type":"text","text":"tail ✨"})]
        );
        let events = drain(&mut rx);
        assert!(matches!(
            events.last().unwrap(),
            StreamEvent::Llm(LlmEvent::Text(t)) if t == "tail ✨"
        ));
    }

    // ── Silent-downgrade detector (spec §3.4.1) ─────────────────────────────
    //
    // Fixtures mirror the LIVE API shape (streaming probe): message_start
    // carries the full `cache_creation` sub-object; message_delta carries
    // ONLY the aggregate. The detector runs in the delta arm and must work
    // off the split captured at message_start.

    /// Aggregate-only delta — what message_delta actually looks like live.
    const LIVE_DELTA: &str = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":100}}"#;

    /// Downgraded turn: 1h bucket = 0, all writes landed in 5m.
    const DOWNGRADE_START: &str = r#"data: {"type":"message_start","message":{"id":"msg_dg","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}"#;

    fn count_downgrade_notices(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> usize {
        drain(rx)
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Notice(t)) if t.contains("1h cache TTL not honored")))
            .count()
    }

    /// A turn where the 1h bucket is honored (healthy turn 1: prefix write).
    const HONORED_START: &str = r#"data: {"type":"message_start","message":{"id":"msg_ok","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":20,"ephemeral_1h_input_tokens":80}}}}"#;

    #[test]
    fn downgrade_detector_silent_for_healthy_hybrid_session() {
        // The false positive the saw_1h_honored latch exists to kill: a
        // healthy Hybrid session's turn 2+ has 1h == 0 (prefix cached) and
        // 5m > 0 (tail rewrite) — the exact downgrade signature. Turn 1's
        // prefix write must latch saw_1h_honored and keep the detector
        // silent for the rest of the session.
        let (mut state, tx, mut rx) = harness();
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(
            &tx,
            crate::core::config::CacheTtl::Hybrid,
            &notified,
            &honored,
        );
        // Turn 1: 1h prefix written (split on message_start) → latch set, no notice.
        feed(&[HONORED_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(count_downgrade_notices(&mut rx), 0, "turn 1 (1h honored)");
        assert!(
            honored.load(std::sync::atomic::Ordering::Relaxed),
            "latch set on 1h write"
        );
        // Turn 2: prefix cached → 1h == 0, 5m > 0. Healthy. SILENCE.
        let (mut state_t2, tx_t2, mut rx_t2) = harness();
        let ctx_t2 = make_ctx_ttl(
            &tx_t2,
            crate::core::config::CacheTtl::Hybrid,
            &notified,
            &honored,
        );
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state_t2, &ctx_t2);
        assert_eq!(
            count_downgrade_notices(&mut rx_t2),
            0,
            "turn 2 (healthy hybrid signature)"
        );
        // Later request in the same session (new ctx, same latches): still silent.
        let (mut state2, tx2, mut rx2) = harness();
        let ctx2 = make_ctx_ttl(
            &tx2,
            crate::core::config::CacheTtl::Hybrid,
            &notified,
            &honored,
        );
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state2, &ctx2);
        assert_eq!(
            count_downgrade_notices(&mut rx2),
            0,
            "later request, same session"
        );
    }

    #[test]
    fn downgrade_detector_fires_once_when_1h_never_honored() {
        // Genuinely downgraded account: the 1h bucket never goes nonzero —
        // the notice fires on turn 1, exactly once per session, in both modes.
        for ttl in [
            crate::core::config::CacheTtl::OneHour,
            crate::core::config::CacheTtl::Hybrid,
        ] {
            let (mut state, tx, mut rx) = harness();
            let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let ctx = make_ctx_ttl(&tx, ttl, &notified, &honored);
            // First occurrence: 1h bucket = 0, 5m bucket > 0 → exactly one Notice.
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state, &ctx);
            assert_eq!(
                count_downgrade_notices(&mut rx),
                1,
                "first occurrence under {ttl:?}"
            );
            // Second occurrence (same session/latch): nothing.
            let (mut state_b, tx_b, mut rx_b) = harness();
            let ctx_b = make_ctx_ttl(&tx_b, ttl, &notified, &honored);
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state_b, &ctx_b);
            assert_eq!(
                count_downgrade_notices(&mut rx_b),
                0,
                "second occurrence under {ttl:?}"
            );
            // Latch persists across requests in the session (new ctx, same latches).
            let (mut state2, tx2, mut rx2) = harness();
            let ctx2 = make_ctx_ttl(&tx2, ttl, &notified, &honored);
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state2, &ctx2);
            assert_eq!(
                count_downgrade_notices(&mut rx2),
                0,
                "next request, same session"
            );
            // Mode is never auto-flipped — ctx still carries the configured TTL.
            assert_eq!(ctx2.cache_ttl, ttl);
        }
    }

    #[test]
    fn downgrade_detector_silent_under_default_5m() {
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx); // FiveMinutes
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(count_downgrade_notices(&mut rx), 0, "5m mode never warns");
    }

    #[test]
    fn downgrade_detector_silent_when_1h_honored() {
        let (mut state, tx, mut rx) = harness();
        let latch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(
            &tx,
            crate::core::config::CacheTtl::OneHour,
            &latch,
            &honored,
        );
        feed(&[HONORED_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(count_downgrade_notices(&mut rx), 0);
        assert!(
            !latch.load(std::sync::atomic::Ordering::Relaxed),
            "latch untouched when honored"
        );
    }

    #[test]
    fn downgrade_detector_silent_when_split_absent() {
        // cache_creation sub-object missing entirely → no basis to judge; stay quiet.
        let (mut state, tx, mut rx) = harness();
        let latch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(
            &tx,
            crate::core::config::CacheTtl::OneHour,
            &latch,
            &honored,
        );
        feed(
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":100}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(count_downgrade_notices(&mut rx), 0);
    }

    #[test]
    fn downgrade_detector_silent_on_warm_restart() {
        // Fresh process + still-warm 1h prefix: turn 1 has cache_read > 0
        // (prefix CACHED, not written), 1h == 0, 5m > 0 (tail) — healthy,
        // but fresh latches make it look like a downgrade. The
        // cache_read == 0 guard keeps it silent. A genuinely downgraded
        // account goes cold within 5m, so real downgrades still fire on the
        // first cold turn.
        let (mut state, tx, mut rx) = harness();
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(
            &tx,
            crate::core::config::CacheTtl::Hybrid,
            &notified,
            &honored,
        );
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_warm","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":5000,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":5000,"cache_creation_input_tokens":100}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(
            count_downgrade_notices(&mut rx),
            0,
            "warm restart is healthy — no notice"
        );
        assert!(
            !notified.load(std::sync::atomic::Ordering::Relaxed),
            "notice latch untouched"
        );
    }

    #[test]
    fn downgrade_detector_silent_without_1h_marker() {
        // Degenerate request (hybrid + API key + no system prompt + zero
        // tools): no 1h marker anywhere in the payload → a zero 1h bucket
        // proves nothing. Detector must stay disarmed — even on the exact
        // cold downgrade signature.
        let (mut state, tx, mut rx) = harness();
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = EventCtx {
            model: "claude-sonnet-4-5",
            tx: &tx,
            suppress_stream_deltas: false,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::Hybrid,
            ttl_downgrade_notified: notified.clone(),
            saw_1h_honored: honored.clone(),
            request_has_1h_marker: false,
            usage_counters: None,
            cache_diagnosis: false,
            spend: crate::runtime::child_spend::AttemptObserver::inert(),
            usage_request: None,
        };
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(
            count_downgrade_notices(&mut rx),
            0,
            "no 1h marker in request → silent"
        );
        assert!(!notified.load(std::sync::atomic::Ordering::Relaxed));
    }

    // ── refusal-retry: the 8 spec tests (§8) ──

    #[test]
    fn refusal_stop_reason_detected_in_parse_state() {
        // Test 1: feed a message_delta with stop_reason="refusal"
        // → both stop_reason_seen and stop_reason_is_refusal must be true.
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":1}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(
            state.stop_reason_seen,
            "stop_reason_seen must be true on refusal"
        );
        assert!(
            state.stop_reason_is_refusal,
            "stop_reason_is_refusal must be true when stop_reason=refusal"
        );
    }

    #[test]
    fn non_refusal_stop_reason_does_not_set_refusal_flag() {
        // Test 2: feed a message_delta with stop_reason="end_turn"
        // → stop_reason_seen true, stop_reason_is_refusal false.
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(state.stop_reason_seen, "stop_reason_seen must be true");
        assert!(
            !state.stop_reason_is_refusal,
            "stop_reason_is_refusal must be false for end_turn"
        );
    }

    #[test]
    fn outcome_refusal_when_stop_reason_is_refusal() {
        // Test 3: classify returns Refusal when stop_reason_is_refusal=true and not cancelled.
        let content = vec![json!({"type": "text", "text": "some content"})];
        let r = classify_stream_outcome(None, content, true, true, false);
        assert!(
            matches!(r, StreamOutcome::Refusal),
            "classify must return Refusal when stop_reason_is_refusal=true, got {r:?}"
        );
    }

    #[test]
    fn outcome_refusal_discards_partial_content() {
        // Test 4: even with non-empty accumulated content, classify returns
        // Refusal (not Done) — content is consumed by take() and dropped.
        let content = vec![json!({"type": "text", "text": "partial answer"})];
        let r = classify_stream_outcome(None, content, true, true, false);
        assert!(
            matches!(r, StreamOutcome::Refusal),
            "partial content must not produce Done when refusal flag set, got {r:?}"
        );
    }

    #[test]
    fn outcome_refusal_cancelled_downgrades_to_done() {
        // Test 5: a cancelled stream with refusal flag must downgrade to Done,
        // not Refusal — don't burn retry budget after user cancels.
        let r = classify_stream_outcome(None, vec![], true, true, true);
        assert!(
            matches!(r, StreamOutcome::Done(_)),
            "cancelled refusal must downgrade to Done, got {r:?}"
        );
    }

    #[test]
    fn config_refusal_retries_default_is_2() {
        // Test 6: SynapsConfig default must set refusal_retries = 2.
        use crate::core::config::SynapsConfig;
        let config = SynapsConfig::default();
        assert_eq!(
            config.refusal_retries, 2,
            "default refusal_retries must be 2"
        );
    }

    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn config_refusal_retries_parsed_from_file() {
        // Test 7: load_config() parses "refusal_retries = 5" from the config file.
        // Uses SYNAPS_BASE_DIR to point load_config at a temp dir (no HOME mutation).
        use agent_core::config::load_config;
        let dir = tempfile::tempdir().expect("tempdir");
        // Config file is <base_dir>/config (no extension).
        std::fs::write(dir.path().join("config"), b"refusal_retries = 5\n").expect("write");
        let prev = std::env::var("SYNAPS_BASE_DIR").ok();
        std::env::set_var("SYNAPS_BASE_DIR", dir.path().to_str().unwrap());
        let config = load_config();
        match prev {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
        assert_eq!(
            config.refusal_retries, 5,
            "refusal_retries must parse from config file"
        );
    }

    #[test]
    fn refusal_in_stream_sets_flag_end_to_end() {
        // Test 8: full SSE fixture through process_data_line.
        // After finalize(): stop_reason_is_refusal == true.
        // classify_stream_outcome returns Refusal.
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","content":[],"model":"claude-opus-4-5","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I cannot"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":3}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(
            state.stop_reason_is_refusal,
            "end-to-end: refusal flag must survive full event sequence"
        );
        let content = std::mem::take(&mut state.accumulated_content);
        let r = classify_stream_outcome(None, content, true, true, false);
        assert!(
            matches!(r, StreamOutcome::Refusal),
            "end-to-end: classify must return Refusal after realistic fixture, got {r:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// on-401 refetch integration tests  (task #157, subtask 9)
// ─────────────────────────────────────────────────────────────────────────────
//
// Strategy: spin up two tiny axum servers — one that pretends to be Anthropic,
// one that pretends to be the credential broker.  Point ApiOptions at them via
// `anthropic_base_url` (test-only seam) + a Remote CredentialSource.
//
// This module tests the C1 guard in call_api_stream_inner directly.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod on401_tests {
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get as axum_get,
        routing::post as axum_post,
        Router,
    };
    use reqwest::Client;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, RwLock};

    use super::{ApiMethods, ApiOptions};
    use crate::auth::{CredentialSource, TokenCache};
    use crate::runtime::telemetry::TelemetryLevel;
    use crate::runtime::types::AuthState;
    use crate::{StreamEvent, ToolRegistry};

    // ── minimal SSE body Anthropic would return for a "hello" text turn ──────
    const SSE_SUCCESS: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    /// Spawn a mock Anthropic endpoint. Returns (base_url, call_counter).
    /// Behaviour: first `fail_count` POSTs → 401; subsequent → SSE success.
    async fn spawn_mock_anthropic(fail_count: usize) -> (String, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move || {
                let counter = Arc::clone(&counter_clone);
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n < fail_count {
                        (
                            StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\"}}"
                                .to_string(),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            SSE_SUCCESS.to_string(),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), counter)
    }

    /// Spawn a mock broker that always vends a fresh token.
    async fn spawn_mock_broker(token: &'static str) -> String {
        let app = Router::new().route(
            "/token",
            axum_get(move |_headers: HeaderMap| async move {
                // Accept any machine-token for test simplicity
                (
                    StatusCode::OK,
                    format!("{{\"access_token\":\"{token}\",\"expires\":9999999999999}}"),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Build an AuthState with the given token pre-loaded (simulates a Remote
    /// client that already ran refresh_if_needed and cached a token).
    fn auth_with_token(token: &str) -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: token.to_string(),
            auth_type: "api_key".to_string(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    fn make_options(base_url: String, source: CredentialSource, cache: TokenCache) -> ApiOptions {
        ApiOptions {
            anthropic_base_url: Some(base_url),
            credential_source: source,
            token_cache: cache,
            ..Default::default()
        }
    }

    async fn drive_call(
        auth: Arc<RwLock<AuthState>>,
        options: ApiOptions,
    ) -> crate::error::Result<serde_json::Value> {
        let client = Client::new();
        let tools = ToolRegistry::new();
        let messages = vec![std::sync::Arc::new(
            json!({"role": "user", "content": "hi"}),
        )];
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        ApiMethods::call_api_stream(
            &auth,
            &client,
            "claude-haiku-4-5", // fast/cheap model name; routing check just does prefix match
            &tools,
            &None, // system prompt
            0,     // thinking_budget
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages,
            tx,
            0, // max_retries (surface errors fast)
            0, // refusal_retries
            &options,
            TelemetryLevel::Off,
        )
        .await
    }

    // ─── T1: Happy path — first call 401, second call succeeds ───────────────
    // RED: before C1 guard is restored this MUST fail (no retry → error).
    // GREEN: with C1 guard it retries once with the new token and succeeds.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn on401_remote_retries_once_and_succeeds() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let broker_url = spawn_mock_broker("sk-fresh-token").await;
        let (anthropic_url, counter) = spawn_mock_anthropic(1).await; // 1 × 401, then SSE

        let source = CredentialSource::Remote {
            endpoint: broker_url.clone(),
            machine_token: "machine-tok".into(),
        };
        // Pre-seed cache with the STALE token (the one that gets the 401)
        let cache = TokenCache::new();
        cache.put(
            "anthropic",
            crate::auth::BrokerToken {
                access_token: "sk-stale".into(),
                expires: 9_999_999_999_999,
                ttl_ms: None,
            },
        );

        let auth = auth_with_token("sk-stale");
        let options = make_options(anthropic_url, source, cache);

        let result = drive_call(auth, options).await;

        // Call must succeed
        assert!(
            result.is_ok(),
            "expected success after retry, got: {:?}",
            result.err()
        );
        // Anthropic endpoint was hit exactly twice: once 401, once 200
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "expected exactly 2 calls to Anthropic (1×401 + 1×200)"
        );
    }

    // ─── T2: Persistent 401 — no infinite loop ────────────────────────────────
    // Guard fires once: 2nd 401 must surface as a terminal error.
    // Anthropic endpoint must be called exactly 2 times total (no loop).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn on401_remote_persistent_401_is_terminal_not_loop() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let broker_url = spawn_mock_broker("sk-fresh-but-useless").await;
        // All calls → 401 (fail_count=999 is effectively "always")
        let (anthropic_url, counter) = spawn_mock_anthropic(999).await;

        let source = CredentialSource::Remote {
            endpoint: broker_url,
            machine_token: "machine-tok".into(),
        };
        let cache = TokenCache::new();
        cache.put(
            "anthropic",
            crate::auth::BrokerToken {
                access_token: "sk-stale".into(),
                expires: 9_999_999_999_999,
                ttl_ms: None,
            },
        );

        let auth = auth_with_token("sk-stale");
        let options = make_options(anthropic_url, source, cache);

        let result = drive_call(auth, options).await;

        // Must be an error (not a success or an infinite retry)
        assert!(result.is_err(), "expected terminal error on persistent 401");
        // Exactly 2 upstream calls: original + single retry
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "persistent 401 must cause exactly 2 upstream calls, got {}",
            counter.load(Ordering::SeqCst)
        );
    }

    // ─── T3: Local source — 401 is NOT retried (regression guard) ────────────
    // CredentialSource::Local must NOT trigger the C1 refetch path.
    // With max_retries=0 the error surfaces immediately after 1 call.
    #[tokio::test]
    async fn on401_local_api_key_does_not_retry() {
        // Always 401
        let (anthropic_url, counter) = spawn_mock_anthropic(999).await;

        let source = CredentialSource::Local;
        let cache = TokenCache::new();
        let auth = auth_with_token("sk-local-key");
        let options = make_options(anthropic_url, source, cache);

        let result = drive_call(auth, options).await;

        assert!(result.is_err(), "401 with Local source must be an error");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "Local source must never retry on 401, got {} calls",
            counter.load(Ordering::SeqCst)
        );
    }

    // ─── Local OAuth source: 401 → reload auth.json → retry once ──────────
    //
    // Models the reported bug: auth.json was rotated externally (another
    // `synaps login`, refresh-auth cron, lab-laptop sync) while this session
    // still holds the old, unexpired token in memory.

    /// Mock Anthropic that 401s any request whose bearer is not `good_bearer`,
    /// recording every Authorization header it sees.
    async fn spawn_mock_anthropic_accepting(
        good_bearer: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move |headers: HeaderMap| {
                let seen = Arc::clone(&seen_clone);
                async move {
                    let got = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let ok = got == format!("Bearer {good_bearer}");
                    seen.lock().unwrap().push(got);
                    if ok {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            SSE_SUCCESS.to_string(),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\"}}"
                                .to_string(),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn write_local_anthropic_creds(base: &std::path::Path, access: &str) {
        let body = json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "rt-test",
                "access": access,
                "expires": 9_999_999_999_999u64,
            }
        });
        std::fs::write(base.join("auth.json"), body.to_string()).unwrap();
    }

    /// Held token bound to the default local Anthropic seat — the state a
    /// session is in after its pre-stream refresh, which is what 401
    /// recovery (`recover_local_rejection`) re-vends for.
    fn local_oauth_auth(token: &str) -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: token.to_string(),
            auth_type: "oauth".to_string(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: Some(crate::runtime::auth::anthropic_binding(
                &CredentialSource::Local,
                &crate::auth::CredentialRef::default_for(crate::auth::OAuthProviderId::Anthropic),
            )),
        }))
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn on401_local_oauth_reloads_rotated_auth_json_and_retries_once() {
        let base = crate::test_env::BaseDirGuard::new();
        write_local_anthropic_creds(base.path(), "sk-rotated-on-disk");
        let (url, seen) = spawn_mock_anthropic_accepting("sk-rotated-on-disk").await;

        let auth = local_oauth_auth("sk-stale-in-memory");
        let options = make_options(url, CredentialSource::Local, TokenCache::new());
        let result = drive_call(Arc::clone(&auth), options).await;

        assert!(
            result.is_ok(),
            "expected success after reload: {:?}",
            result.err()
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "Bearer sk-stale-in-memory".to_string(),
                "Bearer sk-rotated-on-disk".to_string()
            ],
            "exactly one retry, and it must carry the reloaded token"
        );
        assert_eq!(auth.read().await.auth_token, "sk-rotated-on-disk");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn on401_local_oauth_persistent_401_is_terminal_not_loop() {
        let base = crate::test_env::BaseDirGuard::new();
        write_local_anthropic_creds(base.path(), "sk-also-rejected");
        let (url, seen) = spawn_mock_anthropic_accepting("sk-never-valid").await;

        let auth = local_oauth_auth("sk-stale-in-memory");
        let options = make_options(url, CredentialSource::Local, TokenCache::new());
        let result = drive_call(auth, options).await;

        let err = result.expect_err("second 401 must be terminal");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "persistent 401 must cause exactly 2 upstream calls"
        );
        let msg = err.to_string();
        for secret in ["sk-stale-in-memory", "sk-also-rejected", "rt-test"] {
            assert!(
                !msg.contains(secret),
                "error must not surface token values: {msg}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn on401_local_oauth_adopts_peer_token_and_retries_only_once() {
        let _config = crate::test_env::BaseDirGuard::new();
        let credential =
            crate::auth::CredentialRef::default_for(crate::auth::OAuthProviderId::Anthropic);
        crate::auth::save_provider_auth(
            "anthropic",
            &crate::auth::OAuthCredentials {
                auth_type: "oauth".into(),
                access: "peer-token".into(),
                refresh: "never-used".into(),
                expires: crate::epoch_millis() + 3_600_000,
                account_id: None,
            },
        )
        .unwrap();
        for failures in [1, 999] {
            let (url, counter) = spawn_mock_anthropic(failures).await;
            let auth = auth_with_token("rejected-token");
            {
                let mut state = auth.write().await;
                state.auth_type = "oauth".into();
                state.bound_credential = Some(crate::runtime::auth::anthropic_binding(
                    &CredentialSource::Local,
                    &credential,
                ));
            }
            let result = drive_call(
                auth.clone(),
                make_options(url, CredentialSource::Local, TokenCache::new()),
            )
            .await;
            assert_eq!(result.is_ok(), failures == 1);
            assert_eq!(counter.load(Ordering::SeqCst), 2);
            assert_eq!(auth.read().await.auth_token, "peer-token");
        }
    }

    #[tokio::test]
    async fn exhausted_send_and_body_disconnect_are_network_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for partial_headers in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 8192];
                let _ = socket.read(&mut buf).await;
                if partial_headers {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\n\r\ndata: ").await.unwrap();
                }
            });
            let error = drive_call(
                auth_with_token("test"),
                make_options(url, CredentialSource::Local, TokenCache::new()),
            )
            .await
            .unwrap_err();
            server.await.unwrap();
            assert!(matches!(error, crate::RuntimeError::Api(_)), "{error:?}");
            let human = crate::runtime::helpers::turn_error_for(&error, "test-turn");
            assert!(human.category_label().contains("network_error"));
            assert!(!human.message.contains("http://"));
            assert!(!human.message.is_empty());
        }
    }

    // ─── Request-lifecycle T1: outgoing-request trace is metadata-only ──────
    // A request body containing a unique sentinel must never surface in
    // tracing output at ANY level (TRACE included); only metadata fields
    // (provider, model, byte length, counts, correlation id) may appear.

    /// `std::io::Write` sink that appends to a shared buffer, for capturing
    /// formatted tracing output in-process.
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn outgoing_request_trace_is_metadata_only() {
        let sentinel = "SENTINEL_5f2a9c_do_not_log";
        let body = serde_json::json!({
            "model": "claude-test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": sentinel,
                    "cache_control": {"type": "ephemeral"}
                }]
            }],
            "system": sentinel,
            "tools": [],
        });
        let body_bytes = serde_json::to_vec(&body).unwrap();

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || CaptureWriter(sink.clone()))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            super::trace_outgoing_request("claude-test", &body_bytes, 1, 0);
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!output.is_empty(), "expected a trace line to be emitted");
        assert!(
            !output.contains(sentinel),
            "raw payload content leaked into trace output: {output}"
        );
        for field in [
            "provider",
            "model",
            "payload_bytes",
            "message_count",
            "tool_count",
            "cache_marker_count",
            "correlation_id",
        ] {
            assert!(
                output.contains(field),
                "metadata field `{field}` missing from trace output: {output}"
            );
        }
    }

    // ─── Phase 1 holdout regression: hostile provider echoes the request ─────
    // A hostile loopback provider returns HTTP 500 whose `error.message` is
    // "ECHOED:" + the full request body (user message, system prompts, tool
    // schemas). spec §5.1 / Task 1: no raw-content sentinel may appear in
    // logs at any level, in retry notices, or in the surfaced error.

    const HOSTILE_SENTINEL: &str = "HOLDOUT-SENTINEL-http500-77aa";

    /// Stub provider that echoes the entire request body inside a 500 JSON
    /// error envelope — the exact hostile shape from the preserved probe.
    async fn spawn_echoing_500_provider() -> String {
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move |body: String| async move {
                let envelope = serde_json::json!({
                    "type": "error",
                    "error": { "type": "api_error",
                               "message": format!("ECHOED:{body}") }
                });
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-type", "application/json")],
                    envelope.to_string(),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn assert_no_hostile_leak(text: &str, ctx: &str) {
        assert!(
            !text.contains(HOSTILE_SENTINEL),
            "{ctx}: sentinel leaked: {text}"
        );
        assert!(
            !text.contains("ECHOED:"),
            "{ctx}: echoed request body leaked: {text}"
        );
        assert!(
            !text.contains("input_schema"),
            "{ctx}: tool schema leaked: {text}"
        );
    }

    /// RED at HEAD: the retry `tracing::warn!` prints `last_err` (status +
    /// raw body) and the exhausted path humanizes the raw body into the
    /// user-visible error — both carried the sentinel. GREEN: neither logs
    /// (any level), notices, nor the typed error contain any body-derived
    /// text.
    #[tokio::test]
    async fn hostile_500_retry_and_exhaustion_never_leak_request_content() {
        let anthropic_url = spawn_echoing_500_provider().await;

        // Capture ALL tracing output (TRACE included) on this thread.
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || CaptureWriter(sink.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let options = make_options(anthropic_url, CredentialSource::Local, TokenCache::new());
        let auth = auth_with_token("sk-local-key");
        let client = Client::new();
        let tools = ToolRegistry::new();
        let messages = vec![std::sync::Arc::new(json!({
            "role": "user",
            "content": format!("{HOSTILE_SENTINEL} tell me things"),
        }))];
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        // max_retries=1 → exercises BOTH the retry-warning path (attempt 1)
        // and the retries-exhausted error path (attempt 2).
        let result = ApiMethods::call_api_stream(
            &auth,
            &client,
            "claude-haiku-4-5",
            &tools,
            &Some(format!("system secret {HOSTILE_SENTINEL}")),
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages,
            tx,
            1, // max_retries
            0, // refusal_retries
            &options,
            TelemetryLevel::Off,
        )
        .await;

        // 1) Surfaced typed error must not carry any body-derived text.
        let err = result.expect_err("hostile 500 must fail").to_string();
        assert_no_hostile_leak(&err, "typed error");
        assert!(
            err.contains("500") || err.contains("server error"),
            "error must stay actionable (status class): {err}"
        );

        // 2) User-visible notices (retry banners) must not leak.
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Session(
                crate::SessionEvent::Notice(n) | crate::SessionEvent::Retry(n),
            ) = ev
            {
                assert_no_hostile_leak(&n, "notice");
            }
        }

        // 3) Log sink at every level must not leak.
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!logs.is_empty(), "expected retry warnings to be logged");
        assert_no_hostile_leak(&logs, "tracing output");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// G3 — Anthropic window exhaustion → bounded account failover (loop tests)
// ─────────────────────────────────────────────────────────────────────────────
//
// Strategy: a scripted loopback "Anthropic" whose replies are consumed in
// order and which records the `authorization` header of every call, plus a
// scripted `AnthropicAccountRouter` (no broker, no env, no config, no
// auth.json). The loop is driven through `anthropic_stream_with_router`,
// the same seam production reaches through `call_api_stream_inner`.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod anthropic_failover_tests {
    // Every test in this module drives the OAuth 429 path, so each one can be
    // the first to hit the "schema capture" `warn!` callsite. tracing-core
    // caches a callsite's interest globally, and while only one scoped
    // subscriber is registered a callsite first hit on ANOTHER thread is
    // cached as `never` (that thread has no subscriber). That silently hid
    // the line `schema_capture_logs_headers_once_and_never_the_body` counts
    // and failed CI once. Serializing the module removes the race.
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post as axum_post,
        Router,
    };
    use reqwest::Client;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, RwLock};

    use super::{ApiMethods, ApiOptions};
    use crate::auth::{
        AccessToken, Account, CredentialRef, CredentialSource, OAuthProviderId, PinnedToken,
        TokenCache,
    };
    use crate::runtime::anthropic_quota::{
        AnthropicAccountRouter, AnthropicQuotaEvidence, AnthropicQuotaSignal,
    };
    use crate::runtime::auth::anthropic_binding;
    use crate::runtime::telemetry::TelemetryLevel;
    use crate::runtime::types::AuthState;
    use crate::{StreamEvent, ToolRegistry};

    /// Sentinel inside every 429 body: must never reach logs, notices or
    /// the surfaced error.
    const BODY_SENTINEL: &str = "G3-BODY-SENTINEL-9f1c";

    const SSE_SUCCESS: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    /// Partial output, then an in-stream transient error: the loop retries
    /// the request, but output has now started on this request.
    const SSE_PARTIAL_THEN_OVERLOADED: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );

    #[derive(Clone, Copy)]
    enum Reply {
        /// `anthropic-ratelimit-unified-status: rejected` + unified reset this
        /// many seconds from now, `rate_limit_error` body.
        Rejected429 { reset_in_secs: u64 },
        /// Ordinary 429: `retry-after` only, no exhaustion signal.
        Plain429 { retry_after: u64 },
        /// Only Fable's weekly limit (`7d_oi`) rejected, until this many
        /// seconds from now; the five-hour and weekly windows allowed.
        FableOnly429 { reset_in_secs: u64 },
        /// [`Reply::FableOnly429`] with a unified reset five days out, and
        /// the header `name` set to the raw bytes `value`: replacing one
        /// (or added), or, with `append`, sent once more after it.
        FableOnlyWith429 {
            reset_in_secs: u64,
            name: &'static str,
            value: &'static [u8],
            append: bool,
        },
        SseOk,
        SsePartialThenOverloaded,
        /// 529 `overloaded_error` with `retry-after: 0`.
        Overloaded529,
    }

    struct MockUpstream {
        base_url: String,
        /// `authorization` header of every call, in order.
        auth_seen: Arc<Mutex<Vec<String>>>,
        /// Request body of every call, in order.
        bodies: Arc<Mutex<Vec<String>>>,
    }

    impl MockUpstream {
        fn calls(&self) -> usize {
            self.auth_seen.lock().unwrap().len()
        }
        fn auth_seen(&self) -> Vec<String> {
            self.auth_seen.lock().unwrap().clone()
        }
        fn bodies(&self) -> Vec<String> {
            self.bodies.lock().unwrap().clone()
        }
    }

    async fn spawn_upstream(script: Vec<Reply>) -> MockUpstream {
        let script = Arc::new(Mutex::new(script.into_iter()));
        let auth_seen = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&auth_seen);
        let bodies_seen = Arc::clone(&bodies);
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move |headers: HeaderMap, body: String| {
                let script = Arc::clone(&script);
                let seen = Arc::clone(&seen);
                let bodies_seen = Arc::clone(&bodies_seen);
                async move {
                    bodies_seen.lock().unwrap().push(body);
                    seen.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    let reply = script
                        .lock()
                        .unwrap()
                        .next()
                        .expect("upstream called more times than scripted");
                    let now_secs = crate::epoch_millis() / 1000;
                    let body_429 = json!({
                        "type": "error",
                        "error": {"type": "rate_limit_error", "message": BODY_SENTINEL}
                    })
                    .to_string();
                    match reply {
                        Reply::FableOnly429 { reset_in_secs } => (
                            StatusCode::TOO_MANY_REQUESTS,
                            [
                                ("content-type", "application/json".to_string()),
                                ("anthropic-ratelimit-unified-status", "rejected".to_string()),
                                ("anthropic-ratelimit-unified-5h-status", "allowed".to_string()),
                                (
                                    "anthropic-ratelimit-unified-5h-utilization",
                                    "0.36".to_string(),
                                ),
                                ("anthropic-ratelimit-unified-7d-status", "allowed".to_string()),
                                (
                                    "anthropic-ratelimit-unified-7d-utilization",
                                    "0.5".to_string(),
                                ),
                                (
                                    "anthropic-ratelimit-unified-7d_oi-status",
                                    "rejected".to_string(),
                                ),
                                (
                                    "anthropic-ratelimit-unified-7d_oi-reset",
                                    (now_secs + reset_in_secs).to_string(),
                                ),
                            ],
                            body_429,
                        )
                            .into_response(),
                        Reply::FableOnlyWith429 {
                            reset_in_secs,
                            name,
                            value,
                            append,
                        } => {
                            let mut response = (
                                StatusCode::TOO_MANY_REQUESTS,
                                [("content-type", "application/json".to_string())],
                                body_429,
                            )
                                .into_response();
                            let unified_reset = (now_secs + 5 * 86400).to_string();
                            let fable_reset = (now_secs + reset_in_secs).to_string();
                            for (header, header_value) in [
                                ("anthropic-ratelimit-unified-status", "rejected"),
                                ("anthropic-ratelimit-unified-reset", unified_reset.as_str()),
                                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                                ("anthropic-ratelimit-unified-5h-utilization", "0.36"),
                                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                                ("anthropic-ratelimit-unified-7d-utilization", "0.5"),
                                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                                ("anthropic-ratelimit-unified-7d_oi-reset", fable_reset.as_str()),
                            ] {
                                response.headers_mut().insert(
                                    header,
                                    axum::http::HeaderValue::from_str(header_value).unwrap(),
                                );
                            }
                            let value = axum::http::HeaderValue::from_bytes(value).unwrap();
                            if append {
                                response.headers_mut().append(name, value);
                            } else {
                                response.headers_mut().insert(name, value);
                            }
                            response
                        }
                        Reply::Rejected429 { reset_in_secs } => (
                            StatusCode::TOO_MANY_REQUESTS,
                            [
                                ("content-type", "application/json".to_string()),
                                ("anthropic-ratelimit-unified-status", "rejected".to_string()),
                                (
                                    "anthropic-ratelimit-unified-reset",
                                    (now_secs + reset_in_secs).to_string(),
                                ),
                            ],
                            body_429,
                        )
                            .into_response(),
                        Reply::Plain429 { retry_after } => (
                            StatusCode::TOO_MANY_REQUESTS,
                            [
                                ("content-type", "application/json".to_string()),
                                ("retry-after", retry_after.to_string()),
                            ],
                            body_429,
                        )
                            .into_response(),
                        Reply::SseOk => (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            SSE_SUCCESS.to_string(),
                        )
                            .into_response(),
                        Reply::SsePartialThenOverloaded => (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            SSE_PARTIAL_THEN_OVERLOADED.to_string(),
                        )
                            .into_response(),
                        Reply::Overloaded529 => (
                            StatusCode::from_u16(529).unwrap(),
                            [
                                ("content-type", "application/json".to_string()),
                                ("retry-after", "0".to_string()),
                            ],
                            json!({
                                "type": "error",
                                "error": {"type": "overloaded_error", "message": "Overloaded"}
                            })
                            .to_string(),
                        )
                            .into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockUpstream {
            base_url: format!("http://{addr}"),
            auth_seen,
            bodies,
        }
    }

    fn seat(label: &str) -> CredentialRef {
        CredentialRef::new(OAuthProviderId::Anthropic, Account::parse(label).unwrap())
    }

    fn pinned(label: &str) -> PinnedToken {
        PinnedToken {
            credential: seat(label),
            token: AccessToken {
                token: format!("tok-{label}"),
                expires: u64::MAX,
            },
        }
    }

    /// Scripted router: fixed selector mode, scripted failover answers (one
    /// per lookup; the last one repeats), records every report and every
    /// lookup with the accounts it was told to avoid.
    struct ScriptedRouter {
        auto: bool,
        answers: Mutex<Vec<Result<Option<PinnedToken>, String>>>,
        reports: Mutex<Vec<(CredentialRef, AnthropicQuotaEvidence)>>,
        failovers: Mutex<Vec<(String, Vec<CredentialRef>)>>,
        served: Mutex<Vec<CredentialRef>>,
    }

    impl ScriptedRouter {
        fn new(auto: bool, answer: Result<Option<PinnedToken>, String>) -> Self {
            Self::scripted(auto, vec![answer])
        }
        fn scripted(auto: bool, answers: Vec<Result<Option<PinnedToken>, String>>) -> Self {
            assert!(!answers.is_empty(), "a router needs at least one answer");
            Self {
                auto,
                answers: Mutex::new(answers),
                reports: Mutex::new(Vec::new()),
                failovers: Mutex::new(Vec::new()),
                served: Mutex::new(Vec::new()),
            }
        }
        fn reports(&self) -> Vec<(CredentialRef, AnthropicQuotaEvidence)> {
            self.reports.lock().unwrap().clone()
        }
        fn failovers(&self) -> Vec<(String, Vec<CredentialRef>)> {
            self.failovers.lock().unwrap().clone()
        }
        fn served(&self) -> Vec<CredentialRef> {
            self.served.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AnthropicAccountRouter for ScriptedRouter {
        fn auto_selected(&self) -> bool {
            self.auto
        }
        async fn report_exhausted(
            &self,
            failed: &CredentialRef,
            evidence: &AnthropicQuotaEvidence,
        ) {
            self.reports
                .lock()
                .unwrap()
                .push((failed.clone(), *evidence));
        }
        async fn report_served(&self, seat: &CredentialRef, _sent_at_ms: u64) {
            self.served.lock().unwrap().push(seat.clone());
        }
        async fn failover(
            &self,
            model: &str,
            attempted: &[CredentialRef],
        ) -> Result<Option<PinnedToken>, String> {
            self.failovers
                .lock()
                .unwrap()
                .push((model.to_string(), attempted.to_vec()));
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            }
        }
    }

    /// OAuth state pinned to `label` on the Local source — what
    /// `refresh_if_needed` leaves behind before the request loop runs.
    fn oauth_auth_pinned_to(label: &str) -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: format!("tok-{label}"),
            auth_type: "oauth".to_string(),
            refresh_token: None,
            token_expires: Some(u64::MAX),
            auth_file_mtime: None,
            bound_credential: Some(anthropic_binding(&CredentialSource::Local, &seat(label))),
        }))
    }

    struct Outcome {
        result: crate::error::Result<serde_json::Value>,
        notices: Vec<String>,
        /// The subset that came as typed retries (`SessionEvent::Retry`).
        retries: Vec<String>,
    }

    async fn drive(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &ScriptedRouter,
        max_retries: u32,
    ) -> Outcome {
        drive_with(upstream, auth, router, max_retries, ApiOptions::default()).await
    }

    /// [`drive`] with `base` options (any router, e.g. the real broker's).
    async fn drive_with(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &dyn AnthropicAccountRouter,
        max_retries: u32,
        base: ApiOptions,
    ) -> Outcome {
        drive_options(upstream, auth, router, max_retries, base, "claude-haiku-4-5").await
    }

    /// [`drive`] for a request for `model`.
    async fn drive_model(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &ScriptedRouter,
        max_retries: u32,
        model: &str,
    ) -> Outcome {
        drive_request(upstream, auth, router, max_retries, None, None, model).await
    }

    /// [`drive`], as a keep-warm request when `keepwarm` is given.
    async fn drive_keepwarm(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &ScriptedRouter,
        max_retries: u32,
        keepwarm: Option<super::KeepWarmRequest>,
    ) -> Outcome {
        drive_router(upstream, auth, router, max_retries, keepwarm, None).await
    }

    /// [`drive_keepwarm`] through any router, with the turn's capacity wait.
    async fn drive_router(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &dyn AnthropicAccountRouter,
        max_retries: u32,
        keepwarm: Option<super::KeepWarmRequest>,
        capacity_wait: Option<crate::runtime::capacity_wait::CapacityWaitPolicy>,
    ) -> Outcome {
        drive_request(
            upstream,
            auth,
            router,
            max_retries,
            keepwarm,
            capacity_wait,
            "claude-haiku-4-5",
        )
        .await
    }

    /// [`drive_router`] for a request for `model`.
    async fn drive_request(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &dyn AnthropicAccountRouter,
        max_retries: u32,
        keepwarm: Option<super::KeepWarmRequest>,
        capacity_wait: Option<crate::runtime::capacity_wait::CapacityWaitPolicy>,
        model: &str,
    ) -> Outcome {
        let base = ApiOptions {
            keepwarm,
            capacity_wait,
            ..Default::default()
        };
        drive_options(upstream, auth, router, max_retries, base, model).await
    }

    /// The one driver: a request for `model` with `base` options, sent to
    /// `upstream` through `router`.
    async fn drive_options(
        upstream: &MockUpstream,
        auth: Arc<RwLock<AuthState>>,
        router: &dyn AnthropicAccountRouter,
        max_retries: u32,
        base: ApiOptions,
        model: &str,
    ) -> Outcome {
        let options = ApiOptions {
            anthropic_base_url: Some(upstream.base_url.clone()),
            credential_source: CredentialSource::Local,
            token_cache: TokenCache::new(),
            ..base
        };
        let client = Client::new();
        let tools = ToolRegistry::new();
        let messages = vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = ApiMethods::anthropic_stream_with_router(
            &auth,
            &client,
            model,
            tools.tools_schema(),
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages,
            tx,
            &cancel,
            max_retries,
            0,
            &options,
            TelemetryLevel::Off,
            router,
        )
        .await;
        let (mut notices, mut retries) = (Vec::new(), Vec::new());
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::Session(crate::SessionEvent::Notice(n)) => notices.push(n),
                StreamEvent::Session(crate::SessionEvent::Retry(n)) => {
                    notices.push(n.clone());
                    retries.push(n);
                }
                _ => {}
            }
        }
        Outcome { result, notices, retries }
    }

    /// The request after the partial stream starts no output of its own,
    /// but output already streamed on this request: a limit on it can't
    /// switch accounts and the ordinary retry runs. The reset is short (the
    /// retry is not given up) but outlives the test: a cooldown that is
    /// gone was cleared, not expired.
    fn short_limit_after_output() -> Vec<Reply> {
        vec![
            Reply::SsePartialThenOverloaded,
            Reply::Rejected429 { reset_in_secs: 120 },
            Reply::SseOk,
        ]
    }

    /// Review fix 1: a short limit the ordinary retry outlives. The
    /// account that then serves is reported served, once, so its cooldown
    /// goes; nothing else is.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn an_account_that_serves_after_its_limit_is_reported_served() {
        let upstream = spawn_upstream(short_limit_after_output()).await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");
        let out = drive(&upstream, Arc::clone(&auth), &router, 3).await;
        out.result.expect("the retry succeeds on the same account");
        assert_eq!(upstream.calls(), 3);
        assert_eq!(router.reports().len(), 1);
        assert!(router.failovers().is_empty(), "output streamed: no lookup");
        assert_eq!(router.served(), vec![seat("claude4")]);
        assert_eq!(auth.read().await.auth_token, "tok-claude4");

        // A request that met no limit reports nothing.
        let upstream = spawn_upstream(vec![Reply::SseOk]).await;
        let router = ScriptedRouter::new(true, Ok(None));
        drive(&upstream, Arc::clone(&auth), &router, 0)
            .await
            .result
            .expect("ok");
        assert!(router.served().is_empty());
    }

    /// Review fix 2: a keep-warm ping that meets a recognised limit reports
    /// the cooldown but never looks up, switches or re-binds an account and
    /// sends no notice to the session.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_keepwarm_ping_at_a_limit_never_switches_accounts() {
        let upstream = spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }]).await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");
        let ping = ApiOptions {
            keepwarm: Some(super::KeepWarmRequest {
                mode: super::KeepWarmMode::Replay,
                purpose: super::TailPurpose::Ping,
                stop_reason: Default::default(),
                usage: Default::default(),
                status: Default::default(),
                sent: Default::default(),
                connect_failed: Default::default(),
                failure_kind: Default::default(),
                lease: None,
            }),
            ..Default::default()
        };
        let out = drive_with(&upstream, Arc::clone(&auth), &router, 2, ping).await;
        out.result.expect_err("the ping fails on its own account");
        assert_eq!(upstream.calls(), 1, "no request on another account");
        assert_eq!(router.reports().len(), 1, "the cooldown is still reported");
        assert!(router.failovers().is_empty(), "no account lookup");
        assert!(out.notices.is_empty(), "no notice from a ping: {:?}", out.notices);
        assert_eq!(auth.read().await.auth_token, "tok-claude4");
        assert_eq!(
            auth.read().await.bound_credential.as_deref(),
            Some(anthropic_binding(&CredentialSource::Local, &seat("claude4")).as_str())
        );
    }

    /// End to end through the real `LocalBroker` (auth.json, usage reads,
    /// cooldown memory): two automatic accounts with room, a bound to the
    /// turn.
    mod local_broker {
        use super::*;
        use crate::auth::{AccountPolicy, AccountSelector, CredentialBroker, LocalBroker};
        use crate::runtime::anthropic_quota::BrokerAnthropicRouter;
        use crate::runtime::auth::{AuthMethods, RefreshPoint};

        fn store(home: &std::path::Path, labels: &[&str]) {
            let mut root = serde_json::Map::new();
            for label in labels {
                root.insert(
                    format!("anthropic@{label}"),
                    json!({
                        "type": "oauth",
                        "access": format!("tok-{label}"),
                        "refresh": format!("refresh-{label}"),
                        "expires": 4_102_444_800_000u64,
                    }),
                );
            }
            std::fs::write(home.join("auth.json"), serde_json::Value::Object(root).to_string())
                .unwrap();
        }

        /// Usage endpoint: room in every window for every account.
        async fn spawn_usage() -> String {
            let reset = "2099-01-01T00:00:00Z";
            let app = Router::new().route(
                "/usage",
                axum::routing::get(move || async move {
                    axum::Json(json!({
                        "five_hour": {"utilization": 10.0, "resets_at": reset},
                        "seven_day": {"utilization": 10.0, "resets_at": reset}
                    }))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}/usage")
        }

        async fn auto_broker(home: &std::path::Path, labels: &[&str]) -> Arc<LocalBroker> {
            store(home, labels);
            Arc::new(
                LocalBroker::new(Client::new())
                    .with_account_policy(
                        AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
                    )
                    .with_usage_endpoint_override(spawn_usage().await),
            )
        }

        fn bound_to(auth: &Arc<RwLock<AuthState>>) -> Option<String> {
            auth.try_read().unwrap().bound_credential.clone()
        }

        /// The next round's refresh, mid-turn.
        async fn next_round(auth: &Arc<RwLock<AuthState>>, broker: &LocalBroker) {
            AuthMethods::refresh_with_broker(
                Arc::clone(auth),
                &CredentialSource::Local,
                broker,
                Some("claude-haiku-4-5"),
                RefreshPoint::WithinTurn,
            )
            .await
            .expect("the next round has an account");
        }

        /// Review fix 1: a short limit on a, the ordinary retry succeeds on
        /// a, and the next round keeps a (its cache) with a's cooldown gone:
        /// with one account and with two (no needless switch to b).
        #[tokio::test]
        #[serial_test::serial(synaps_base_dir, anthropic_429_log_capture)]
        async fn an_account_that_serves_after_a_short_limit_keeps_the_turn() {
            for labels in [&["a"][..], &["a", "b"][..]] {
                let base = crate::test_env::BaseDirGuard::new();
                let broker = auto_broker(base.path(), labels).await;
                let router = BrokerAnthropicRouter::new(broker.clone());
                let upstream = spawn_upstream(short_limit_after_output()).await;
                let auth = oauth_auth_pinned_to("a");
                let out =
                    drive_with(&upstream, Arc::clone(&auth), &router, 3, ApiOptions::default())
                        .await;
                out.result.expect("the retry succeeds on a");
                assert_eq!(upstream.calls(), 3, "{labels:?}");
                assert_eq!(
                    broker.active_cooldown(&seat("a")),
                    None,
                    "{labels:?}: a served after its limit was reported"
                );
                next_round(&auth, &broker).await;
                assert_eq!(
                    bound_to(&auth).as_deref(),
                    Some(anthropic_binding(&CredentialSource::Local, &seat("a")).as_str()),
                    "{labels:?}: the turn stays on a"
                );
            }
        }

        /// Review fix 1 (belt and braces): a is cooling down and is the only
        /// account. Mid-turn the round keeps a instead of failing the turn
        /// with `all_cooling_down` after its tools ran.
        #[tokio::test]
        #[serial_test::serial(synaps_base_dir, anthropic_429_log_capture)]
        async fn the_only_account_cooling_mid_turn_keeps_the_turn() {
            let base = crate::test_env::BaseDirGuard::new();
            let broker = auto_broker(base.path(), &["a"]).await;
            let until = crate::epoch_millis() + 3_600_000;
            broker
                .report_cooldown(&seat("a"), Some(until), "anthropic_quota_exhausted")
                .await
                .unwrap();
            let auth = oauth_auth_pinned_to("a");
            auth.try_write().unwrap().auth_token = "stale".into();
            next_round(&auth, &broker).await;
            let g = auth.try_read().unwrap();
            assert_eq!(g.auth_token, "tok-a", "a is re-vended");
            assert_eq!(
                g.bound_credential.as_deref(),
                Some(anthropic_binding(&CredentialSource::Local, &seat("a")).as_str())
            );
        }

        /// Review fix 4: mid-turn, the re-vend of the pinned seat honours a
        /// refused refresh's hold, like the automatic choice's own vend. a's
        /// token is stale and its refresh is refused (once, held); the turn
        /// moves to b, and a later round asks the token endpoint nothing.
        #[tokio::test]
        #[serial_test::serial(synaps_base_dir, anthropic_429_log_capture)]
        async fn a_held_refusal_is_honoured_by_the_mid_turn_revend() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let base = crate::test_env::BaseDirGuard::new();
            store(base.path(), &["a", "b"]);
            // a's token expired in 1970: any use refreshes first.
            let path = base.path().join("auth.json");
            let mut root: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            root["anthropic@a"]["expires"] = json!(1_000u64);
            std::fs::write(&path, root.to_string()).unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            let app = Router::new().route(
                "/v1/oauth/token",
                axum_post(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        (
                            StatusCode::BAD_REQUEST,
                            [("content-type", "application/json")],
                            r#"{"error":"invalid_grant"}"#,
                        )
                            .into_response()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let broker = LocalBroker::new(Client::new())
                .with_account_policy(
                    AccountPolicy::new().with(OAuthProviderId::Anthropic, AccountSelector::Auto),
                )
                .with_usage_endpoint_override(spawn_usage().await)
                .with_anthropic_token_url_for_tests(format!("http://{addr}/v1/oauth/token"));
            let b = anthropic_binding(&CredentialSource::Local, &seat("b"));
            for round in 0..2 {
                let auth = oauth_auth_pinned_to("a");
                auth.try_write().unwrap().token_expires = Some(1);
                next_round(&auth, &broker).await;
                assert_eq!(bound_to(&auth).as_deref(), Some(b.as_str()), "round {round}");
                assert_eq!(hits.load(Ordering::SeqCst), 1, "round {round}: one refused refresh");
            }
        }

        /// The PR's mid-turn re-selection through the real broker: a is
        /// cooling down (another session met its limit) and b has room, so
        /// the next round moves to b.
        #[tokio::test]
        #[serial_test::serial(synaps_base_dir, anthropic_429_log_capture)]
        async fn a_cooling_account_mid_turn_moves_the_turn_to_one_with_room() {
            let base = crate::test_env::BaseDirGuard::new();
            let broker = auto_broker(base.path(), &["a", "b"]).await;
            let until = crate::epoch_millis() + 3_600_000;
            broker
                .report_cooldown(&seat("a"), Some(until), "anthropic_quota_exhausted")
                .await
                .unwrap();
            let auth = oauth_auth_pinned_to("a");
            next_round(&auth, &broker).await;
            let g = auth.try_read().unwrap();
            assert_eq!(g.auth_token, "tok-b");
            assert_eq!(
                g.bound_credential.as_deref(),
                Some(anthropic_binding(&CredentialSource::Local, &seat("b")).as_str())
            );
        }
    }

    fn assert_no_body_leak(text: &str, ctx: &str) {
        assert!(
            !text.contains(BODY_SENTINEL),
            "{ctx}: 429 body leaked: {text}"
        );
    }

    /// Every `cache_control` value in a request body, in document order.
    fn cache_markers(v: &serde_json::Value) -> Vec<serde_json::Value> {
        match v {
            serde_json::Value::Object(map) => map
                .iter()
                .flat_map(|(k, v)| {
                    if k == "cache_control" {
                        vec![v.clone()]
                    } else {
                        cache_markers(v)
                    }
                })
                .collect(),
            serde_json::Value::Array(items) => items.iter().flat_map(cache_markers).collect(),
            _ => Vec::new(),
        }
    }

    /// `cache_ttl = auto` (the `ApiOptions` default) resolves from the
    /// credential KIND only. An account failover between two subscription
    /// seats keeps the hop's retry and the next request on the same 1h
    /// markers (a flip would rewrite the cache); only an actual change of
    /// kind, here to an API key, changes the TTL.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn auto_cache_ttl_is_stable_across_account_failover() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 { reset_in_secs: 3 * 3600 },
            Reply::SseOk, // the same request, retried on seat 2
            Reply::SseOk, // the next request, still on seat 2
            Reply::SseOk, // a request on an API key
        ])
        .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");
        assert_eq!(
            ApiOptions::default().cache_ttl,
            crate::core::config::CacheTtlSetting::Auto
        );

        let hop = drive(&upstream, Arc::clone(&auth), &router, 0).await;
        hop.result.expect("the hop succeeds on seat 2");
        let next = drive(&upstream, Arc::clone(&auth), &router, 0).await;
        next.result.expect("the next request stays on seat 2");
        assert_eq!(
            upstream.auth_seen(),
            ["Bearer tok-claude4", "Bearer tok-claude2", "Bearer tok-claude2"],
            "control: the failover really switched seats"
        );
        {
            let mut g = auth.write().await;
            g.auth_type = "api_key".to_string();
            g.auth_token = "sk-test".to_string();
            g.bound_credential = None;
        }
        let key = drive(&upstream, Arc::clone(&auth), &router, 0).await;
        key.result.expect("the API-key request succeeds");

        let bodies = upstream.bodies();
        assert_eq!(bodies.len(), 4);
        assert_eq!(
            bodies[0], bodies[1],
            "the failover retry re-sends the identical body (same 1h markers)"
        );
        let markers: Vec<_> = bodies
            .iter()
            .map(|b| cache_markers(&serde_json::from_str(b).unwrap()))
            .collect();
        for (i, m) in markers[..3].iter().enumerate() {
            assert!(!m.is_empty(), "request {i} has no markers");
            assert!(
                m.iter().all(|m| m["ttl"] == "1h"),
                "request {i} on a subscription seat: {m:?}"
            );
        }
        assert!(!markers[3].is_empty());
        assert!(
            markers[3].iter().all(|m| m.get("ttl").is_none()),
            "a change of kind (API key) resolves auto to the bare 5m marker: {:?}",
            markers[3]
        );
    }

    /// Read one HTTP/1.1 request (head, then its `content-length` body) in
    /// full, so a close afterwards is a clean FIN rather than a reset.
    async fn read_one_request(socket: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut request: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let body = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + body {
                    return;
                }
            }
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// A raw HTTP/1.1 provider that reads each request in full, then closes
    /// the connection without a reply for the first `drops` requests (hyper:
    /// "connection closed before message completed", a transport failure
    /// with no HTTP status) and answers every later one with the SSE
    /// success and `connection: close`. Returns it and its request count.
    async fn spawn_dropping_upstream(drops: usize) -> (MockUpstream, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let counted = Arc::clone(&counted);
                tokio::spawn(async move {
                    read_one_request(&mut socket).await;
                    if counted.fetch_add(1, Ordering::SeqCst) < drops {
                        return;
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        SSE_SUCCESS.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(SSE_SUCCESS.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        let upstream = MockUpstream {
            base_url: format!("http://{addr}"),
            auth_seen: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::new(Mutex::new(Vec::new())),
        };
        (upstream, requests)
    }

    /// [`drive`] (retry budget 3) against [`spawn_dropping_upstream`] under
    /// a fresh log capture; with `legacy`, inside the seam that shows every
    /// transport retry notice (the display before the first one went
    /// silent). Returns the outcome, the request count and the logs.
    async fn drive_dropping(drops: usize, legacy: bool) -> (Outcome, usize, String) {
        let (upstream, requests) = spawn_dropping_upstream(drops).await;
        let router = ScriptedRouter::new(true, Ok(None));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || LogCapture(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let run = drive(&upstream, oauth_auth_pinned_to("claude4"), &router, 3);
        let out = if legacy {
            crate::runtime::net_retry::LEGACY_TRANSPORT_NOTICES_FOR_TEST
                .scope(true, run)
                .await
        } else {
            run.await
        };
        drop(guard);
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (out, requests.load(Ordering::SeqCst), logs)
    }

    /// The transport retry WARN lines, from the message on (the timestamp
    /// before it differs per run).
    fn retry_warns(logs: &str) -> Vec<String> {
        logs.lines()
            .filter_map(|line| line.find("API retry ").map(|at| line[at..].to_string()))
            .collect()
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn pre_output_rejected_429_hops_once_and_succeeds_on_seat_2() {
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }, Reply::SseOk])
                .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let value = out.result.expect("success on the second seat");
        assert_eq!(value["content"][0]["text"], "hi");
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude2".to_string()],
            "the retry must carry the new seat's bearer"
        );
        // Exactly one cooldown report, for the failed seat, with the reset.
        let reports = router.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, seat("claude4"));
        assert_eq!(
            reports[0].1.signal,
            AnthropicQuotaSignal::UnifiedStatusRejected
        );
        assert!(reports[0].1.resets_at_ms.is_some());
        // One failover request, for the bare model id, told the failed seat.
        assert_eq!(
            router.failovers(),
            vec![("claude-haiku-4-5".to_string(), vec![seat("claude4")])]
        );
        // The turn is now pinned to the new seat (mid-turn rounds keep it).
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-claude2");
        assert_eq!(
            g.bound_credential.as_deref(),
            Some(anthropic_binding(&CredentialSource::Local, &seat("claude2")).as_str())
        );
        assert!(
            out.notices
                .iter()
                .any(|n| n.starts_with("⚠ claude4 is at a usage limit until ")
                    && n.ends_with(" — switching to claude2")),
            "notices: {:?}",
            out.notices
        );
        for n in &out.notices {
            assert_no_body_leak(n, "notice");
        }
    }

    /// A4: a Fable limit (only 7d_oi rejected) on a Fable request is
    /// reported as a limit of Fable's requests, and the request fails over
    /// for the same model. On a request for another model it can't be what
    /// refused it: the report cools the whole account (the 7d_oi window
    /// named), with the same reset.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_fable_limit_is_reported_for_fable_requests_and_seat_wide_for_another_model() {
        for (model, signal) in [
            (
                "claude-fable-5-1",
                AnthropicQuotaSignal::ModelWindowRejected {
                    window: "7d_oi",
                    model: "fable",
                },
            ),
            (
                "claude-haiku-4-5",
                AnthropicQuotaSignal::WindowStatusRejected { window: "7d_oi" },
            ),
        ] {
            let upstream = spawn_upstream(vec![
                Reply::FableOnly429 {
                    reset_in_secs: 2 * 86400,
                },
                Reply::SseOk,
            ])
            .await;
            let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
            let auth = oauth_auth_pinned_to("claude4");
            let out = drive_model(&upstream, Arc::clone(&auth), &router, 0, model).await;
            let reports = router.reports();
            assert_eq!(reports.len(), 1, "{model}");
            assert_eq!(reports[0].0, seat("claude4"), "{model}");
            assert_eq!(reports[0].1.signal, signal, "{model}");
            assert!(reports[0].1.resets_at_ms.is_some(), "{model}");
            out.result.expect("success on the second seat");
            assert_eq!(
                router.failovers(),
                vec![(model.to_string(), vec![seat("claude4")])],
                "{model}"
            );
        }
    }

    /// A4 r2/r3: a Fable request refused with an unknown five-hour status,
    /// a claim that names neither a shared window nor the 7d_oi bucket, a
    /// claim that can't be read (byte 0xff), or the five-hour status sent
    /// twice (`allowed`, then `rejected`) is reported as a limit of the
    /// whole account, with the unified reset, and the request fails over
    /// for Fable.
    ///
    /// Coverage is COMPOSED, not end to end: this test drives the production
    /// classifier, the request-model guard and the report on the request
    /// path (the router here is scripted, so no Opus selection runs);
    /// broker_capacity's `a_whole_account_limit_stops_opus_requests_too`
    /// pins that the local broker keeps an account with a whole-account
    /// cooldown from Opus requests.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_fable_request_refused_with_unknown_headers_is_a_limit_of_the_whole_account() {
        for (name, value, append) in [
            ("anthropic-ratelimit-unified-5h-status", &b"garbage"[..], false),
            ("anthropic-ratelimit-unified-representative-claim", &b"overage"[..], false),
            (
                "anthropic-ratelimit-unified-representative-claim",
                &b"seven_day_oi\xff"[..],
                false,
            ),
            ("anthropic-ratelimit-unified-5h-status", &b"rejected"[..], true),
        ] {
            let case = format!("{name}: {value:?}, append {append}");
            let upstream = spawn_upstream(vec![
                Reply::FableOnlyWith429 {
                    reset_in_secs: 2 * 86400,
                    name,
                    value,
                    append,
                },
                Reply::SseOk,
            ])
            .await;
            let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
            let auth = oauth_auth_pinned_to("claude4");
            let model = "claude-fable-5-1";
            let out = drive_model(&upstream, Arc::clone(&auth), &router, 0, model).await;
            let reports = router.reports();
            assert_eq!(reports.len(), 1, "{case}");
            assert_eq!(reports[0].0, seat("claude4"), "{case}");
            assert_eq!(
                reports[0].1.signal,
                AnthropicQuotaSignal::UnifiedStatusRejected,
                "{case}"
            );
            assert_eq!(reports[0].1.model_scope(), None, "{case}: the whole account");
            let reset = reports[0].1.resets_at_ms.expect("the unified reset");
            assert!(
                reset > crate::epoch_millis() + 4 * 86400 * 1000,
                "{case}: the unified reset, not 7d_oi's"
            );
            out.result.expect("success on the second seat");
            assert_eq!(
                router.failovers(),
                vec![(model.to_string(), vec![seat("claude4")])],
                "{case}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn post_output_rejected_429_reports_cooldown_but_never_hops() {
        // Attempt 1 streams partial text then dies transiently; the retry
        // (attempt 2) is refused with a rejected window.
        let upstream = spawn_upstream(vec![
            Reply::SsePartialThenOverloaded,
            Reply::Rejected429 { reset_in_secs: 3 * 3600 },
        ])
        .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 1).await;

        let err = out.result.expect_err("no hop after output; long reset is terminal");
        let msg = err.to_string();
        assert!(
            msg.contains("usage limit reached on account 'claude4'"),
            "{msg}"
        );
        // True text: nothing claims another account will serve.
        assert!(
            msg.contains(
                "Output had already streamed, so this request could not switch accounts; \
                 send again to select an account for the next turn."
            ),
            "{msg}"
        );
        assert!(!msg.contains("will select another account"), "{msg}");
        assert_no_body_leak(&msg, "error");
        assert_eq!(upstream.calls(), 2, "no third call on another seat");
        assert_eq!(router.reports().len(), 1, "cooldown still reported");
        assert_eq!(router.reports()[0].0, seat("claude4"));
        assert!(router.failovers().is_empty(), "gate must veto the lookup");
        // Seat unchanged.
        assert_eq!(auth.read().await.auth_token, "tok-claude4");
        assert!(
            out.notices
                .iter()
                .any(|n| n.starts_with("⚠ claude4 is at a usage limit until ")
                    && n.ends_with(
                        "; output already streamed, so this request can't switch accounts"
                    )),
            "{:?}",
            out.notices
        );
    }

    /// Each account at most once per logical request: after claude4 and then
    /// claude2 are exhausted, the second lookup (told both) names claude2
    /// again. That is refused, and the turn ends saying no other account has
    /// proven capacity, not that the next turn will find one.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn two_exhausted_accounts_and_no_other_end_the_turn_saying_so() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 { reset_in_secs: 3 * 3600 },
            Reply::Rejected429 { reset_in_secs: 5 * 3600 },
        ])
        .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let msg = out.result.expect_err("second exhaustion is terminal").to_string();
        assert!(msg.contains("account 'claude2'"), "{msg}");
        assert!(
            msg.contains(
                "No other account has proven capacity now; \
                 `synaps auth state --provider anthropic` shows each account's state."
            ),
            "{msg}"
        );
        assert!(!msg.contains("will select another account"), "{msg}");
        assert_eq!(upstream.calls(), 2);
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude2".to_string()]
        );
        let reported: Vec<CredentialRef> = router.reports().into_iter().map(|r| r.0).collect();
        assert_eq!(reported, vec![seat("claude4"), seat("claude2")]);
        assert_eq!(
            router.failovers(),
            vec![
                ("claude-haiku-4-5".to_string(), vec![seat("claude4")]),
                (
                    "claude-haiku-4-5".to_string(),
                    vec![seat("claude4"), seat("claude2")]
                ),
            ],
            "a second lookup, told every account this request used"
        );
        assert!(
            out.notices
                .iter()
                .any(|n| n.starts_with("⚠ claude2 is at a usage limit until ")
                    && n.ends_with("; no other account has proven capacity now")),
            "{:?}",
            out.notices
        );
    }

    /// Each account at most once per logical request, under the same
    /// output/tool gates: claude4 and then claude5 are exhausted before any
    /// output, and claude2 serves. Two hops, a notice for each, and each
    /// lookup is told every account already tried.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn pre_output_exhaustion_hops_through_each_account_once_until_one_serves() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 {
                reset_in_secs: 3 * 3600,
            },
            Reply::Rejected429 {
                reset_in_secs: 5 * 3600,
            },
            Reply::SseOk,
        ])
        .await;
        let router = ScriptedRouter::scripted(
            true,
            vec![Ok(Some(pinned("claude5"))), Ok(Some(pinned("claude2")))],
        );
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let value = out.result.expect("claude2 serves the request");
        assert_eq!(value["content"][0]["text"], "hi");
        assert_eq!(
            upstream.auth_seen(),
            vec![
                "Bearer tok-claude4".to_string(),
                "Bearer tok-claude5".to_string(),
                "Bearer tok-claude2".to_string()
            ]
        );
        let reported: Vec<CredentialRef> = router.reports().into_iter().map(|r| r.0).collect();
        assert_eq!(reported, vec![seat("claude4"), seat("claude5")]);
        assert_eq!(
            router.failovers(),
            vec![
                ("claude-haiku-4-5".to_string(), vec![seat("claude4")]),
                (
                    "claude-haiku-4-5".to_string(),
                    vec![seat("claude4"), seat("claude5")]
                ),
            ]
        );
        assert_eq!(auth.read().await.auth_token, "tok-claude2");
        for (from, to) in [("claude4", "claude5"), ("claude5", "claude2")] {
            assert!(
                out.notices
                    .iter()
                    .any(|n| n.starts_with(&format!("⚠ {from} is at a usage limit until "))
                        && n.ends_with(&format!(" — switching to {to}"))),
                "{from} -> {to}: {:?}",
                out.notices
            );
        }
    }

    /// A lookup that names an account this request already used is refused:
    /// no request goes back to it, and the turn ends on the account that was
    /// exhausted last.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_hop_never_returns_to_an_account_already_tried_on_this_request() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 {
                reset_in_secs: 3 * 3600,
            },
            Reply::Rejected429 {
                reset_in_secs: 5 * 3600,
            },
            Reply::SseOk,
        ])
        .await;
        let router = ScriptedRouter::scripted(
            true,
            vec![Ok(Some(pinned("claude5"))), Ok(Some(pinned("claude4")))],
        );
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let msg = out
            .result
            .expect_err("claude4 was already tried; no other account")
            .to_string();
        assert!(msg.contains("account 'claude5'"), "{msg}");
        assert!(
            msg.contains("No other account has proven capacity now"),
            "{msg}"
        );
        assert_eq!(upstream.calls(), 2, "never back to claude4");
        assert_eq!(router.failovers().len(), 2);
        assert_eq!(
            router.failovers()[1].1,
            vec![seat("claude4"), seat("claude5")]
        );
        assert_eq!(auth.read().await.auth_token, "tok-claude5");
    }

    /// A router that keeps naming accounts never tried before still stops:
    /// the backstop bounds the hops of one request.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_router_naming_new_accounts_forever_stops_at_the_hop_backstop() {
        let max = crate::runtime::anthropic_quota::MAX_ANTHROPIC_ACCOUNT_FAILOVERS as usize;
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 {
                reset_in_secs: 3 * 3600
            };
            max + 1
        ])
        .await;
        let answers = (0..max + 4)
            .map(|i| Ok(Some(pinned(&format!("spare{i}")))))
            .collect();
        let router = ScriptedRouter::scripted(true, answers);
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let msg = out
            .result
            .expect_err("the backstop ends the turn")
            .to_string();
        assert!(
            msg.contains(&format!(
                "Stopped after {max} account switches on this request; \
                 `synaps auth state --provider anthropic` shows each account's state."
            )),
            "{msg}"
        );
        assert_eq!(upstream.calls(), max + 1);
        assert_eq!(router.failovers().len(), max);
        assert!(
            out.notices
                .iter()
                .any(|n| n.ends_with("; no more account switches on this request")),
            "{:?}",
            out.notices
        );
    }

    /// The hop rules don't loosen the output gate: once this request has
    /// streamed output, a later exhaustion reports the cooldown but never
    /// switches accounts.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_later_hop_is_refused_once_output_streamed_on_this_request() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 {
                reset_in_secs: 3 * 3600,
            },
            Reply::SsePartialThenOverloaded,
            Reply::Rejected429 {
                reset_in_secs: 3 * 3600,
            },
        ])
        .await;
        let router = ScriptedRouter::scripted(
            true,
            vec![Ok(Some(pinned("claude5"))), Ok(Some(pinned("claude2")))],
        );
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 1).await;

        let msg = out.result.expect_err("no hop after output").to_string();
        assert!(
            msg.contains("usage limit reached on account 'claude5'"),
            "{msg}"
        );
        assert!(msg.contains("Output had already streamed"), "{msg}");
        assert_eq!(upstream.calls(), 3, "no fourth call on claude2");
        assert_eq!(
            router.failovers().len(),
            1,
            "the gate vetoed the second lookup"
        );
        let reported: Vec<CredentialRef> = router.reports().into_iter().map(|r| r.0).collect();
        assert_eq!(reported, vec![seat("claude4"), seat("claude5")]);
        assert_eq!(auth.read().await.auth_token, "tok-claude5");
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn explicit_selector_reports_cooldown_and_ends_without_lookup() {
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }]).await;
        let router = ScriptedRouter::new(false, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let msg = out.result.expect_err("explicit account: terminal").to_string();
        assert!(msg.contains("synaps auth use"), "{msg}");
        assert_eq!(upstream.calls(), 1);
        assert_eq!(router.reports().len(), 1);
        assert!(router.failovers().is_empty());
        assert_eq!(auth.read().await.auth_token, "tok-claude4");
        assert!(out
            .notices
            .iter()
            .any(|n| n.starts_with("⚠ account limit reached until ")));
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn same_seat_back_or_no_candidate_never_loops() {
        for (answer, advice, notice) in [
            (
                Ok(None),
                "No other account has proven capacity now",
                "; no other account has proven capacity now",
            ),
            (
                Ok(Some(pinned("claude4"))),
                "No other account has proven capacity now",
                "; no other account has proven capacity now",
            ),
            (
                Err("broker down".to_string()),
                "Looking up another account failed; \
                 `synaps auth state --provider anthropic` shows each account's state.",
                "; looking up another account failed",
            ),
        ] {
            let upstream =
                spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }]).await;
            let router = ScriptedRouter::new(true, answer);
            let auth = oauth_auth_pinned_to("claude4");

            let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

            let msg = out.result.expect_err("no other account").to_string();
            assert!(msg.contains(advice), "{msg}");
            assert!(!msg.contains("will select another account"), "{msg}");
            assert!(
                out.notices.iter().any(|n| n.ends_with(notice)),
                "{notice}: {:?}",
                out.notices
            );
            assert_eq!(upstream.calls(), 1, "no retry on the exhausted seat");
            assert_eq!(router.reports().len(), 1);
            assert_eq!(router.failovers().len(), 1);
            assert_eq!(auth.read().await.auth_token, "tok-claude4");
        }
    }

    /// A keep-warm ping on an exhausted seat fails as a ping does: one
    /// request, no account hop, and the session's seat binding (token and
    /// bound credential) is untouched. The seat's exhaustion is still
    /// reported, as for any request, so the next turn boundary selects away.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_keepwarm_ping_on_an_exhausted_seat_never_hops() {
        use super::{KeepWarmMode, KeepWarmRequest, TailPurpose};
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }, Reply::SseOk])
                .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");
        let binding = |g: &AuthState| (g.auth_token.clone(), g.bound_credential.clone());
        let before = binding(&*auth.read().await);
        let ping = |mode| KeepWarmRequest {
            mode,
            purpose: TailPurpose::Ping,
            stop_reason: Default::default(),
            usage: Default::default(),
            status: Default::default(),
            sent: Default::default(),
            connect_failed: Default::default(),
            failure_kind: Default::default(),
            lease: None,
        };
        let probe = drive_keepwarm(
            &upstream,
            Arc::clone(&auth),
            &router,
            0,
            Some(ping(KeepWarmMode::Probe)),
        )
        .await;
        let expect = probe.result.expect("probe")["keepwarm_prefix"]
            .as_u64()
            .expect("the probe returns the prefix");
        assert_eq!(upstream.calls(), 0, "a probe sends nothing");

        let out = drive_keepwarm(
            &upstream,
            Arc::clone(&auth),
            &router,
            0,
            Some(ping(KeepWarmMode::Send { expect })),
        )
        .await;

        assert!(out.result.is_err(), "the ping fails: {:?}", out.result);
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string()],
            "one request, on the session's seat"
        );
        assert!(router.failovers().is_empty(), "no candidate lookup");
        assert_eq!(router.reports().len(), 1, "the exhaustion is reported");
        assert_eq!(binding(&*auth.read().await), before, "binding untouched");
        assert!(
            !out.notices.iter().any(|n| n.contains("switching to")),
            "{:?}",
            out.notices
        );
    }

    /// A long wait on a 429 with no usage-limit header is not called a usage
    /// limit: the notice and the error say it is a rate limit, and how we
    /// know.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_long_wait_without_a_usage_limit_header_is_called_a_rate_limit() {
        let upstream = spawn_upstream(vec![Reply::Plain429 {
            retry_after: 3 * 3600,
        }])
        .await;
        let router = ScriptedRouter::new(true, Ok(None));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        let msg = out.result.expect_err("a long wait is terminal").to_string();
        assert!(
            msg.contains("Anthropic rate limit on account 'claude4' until "),
            "{msg}"
        );
        assert!(
            msg.contains(
                "(HTTP 429 with a long wait, not a declared usage limit). \
                 No other account has proven capacity now"
            ),
            "{msg}"
        );
        assert!(!msg.contains("usage limit reached"), "{msg}");
        assert_no_body_leak(&msg, "error");
        assert_eq!(upstream.calls(), 1);
        let reports = router.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].1.signal,
            AnthropicQuotaSignal::LongResetWithRateLimitBody
        );
        assert!(
            out.notices.iter().any(|n| n.starts_with("⚠ claude4 is rate-limited until ")
                && n.ends_with(
                    "(HTTP 429, no usage-limit header); no other account has proven capacity now"
                )),
            "{:?}",
            out.notices
        );
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn ordinary_429_keeps_the_existing_retry_path() {
        let upstream =
            spawn_upstream(vec![Reply::Plain429 { retry_after: 1 }, Reply::SseOk]).await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude4".to_string()],
            "same seat, plain backoff"
        );
        assert!(router.reports().is_empty());
        assert!(router.failovers().is_empty());
        // 04-tui I4: a typed retry (the header's "retrying"), not just text.
        assert!(
            out.retries.iter().any(|n| n.starts_with("⚠ Rate limited — resuming in ")),
            "{:?}",
            out.notices
        );
    }

    /// One transport failure (no HTTP status), then success: the first
    /// retry, after today's 1 s backoff, shows no notice. Display only: the
    /// same failure under the legacy seam shows it, and both runs make the
    /// same attempts and log the same retry WARN.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_fast_first_transport_retry_is_silent_and_nothing_else_changes() {
        let (out, requests, logs) = drive_dropping(1, false).await;
        let (legacy, legacy_requests, legacy_logs) = drive_dropping(1, true).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert!(legacy.result.is_ok(), "{:?}", legacy.result.err());
        assert!(out.retries.is_empty(), "{:?}", out.notices);
        assert_eq!(
            legacy.retries,
            vec!["⏳ API error, retrying (1/3)…".to_string()],
            "{:?}",
            legacy.notices
        );
        assert_eq!((requests, legacy_requests), (2, 2), "one failed send, one retry");
        let warns = retry_warns(&logs);
        assert_eq!(warns.len(), 1, "{logs}");
        assert!(
            warns[0].starts_with("API retry 1/3 after 1s: Network request failed"),
            "{}",
            warns[0]
        );
        for field in [
            "attempt=1",
            "max_attempts=3",
            "class=\"incomplete_message\"",
            "reused=\"unknown\"",
        ] {
            assert!(warns[0].contains(field), "{field} missing: {}", warns[0]);
        }
        assert_eq!(retry_warns(&legacy_logs), warns);
    }

    /// A second consecutive transport failure shows its notice; every retry
    /// still logs its WARN.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_second_consecutive_transport_failure_shows_its_notice() {
        let (out, requests, logs) = drive_dropping(2, false).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(
            out.retries,
            vec!["⏳ API error, retrying (2/3)…".to_string()],
            "{:?}",
            out.notices
        );
        assert_eq!(requests, 3);
        let warns = retry_warns(&logs);
        assert_eq!(warns.len(), 2, "a WARN on every retry: {logs}");
        assert!(warns[0].starts_with("API retry 1/3 after 1s: "), "{}", warns[0]);
        assert!(warns[1].starts_with("API retry 2/3 after 2s: "), "{}", warns[1]);
    }

    /// An HTTP-status retry keeps its first notice: only transport retries
    /// went quiet.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn an_overloaded_529_first_retry_keeps_its_notice() {
        let upstream = spawn_upstream(vec![Reply::Overloaded529, Reply::SseOk]).await;
        let router = ScriptedRouter::new(true, Ok(None));

        let out = drive(&upstream, oauth_auth_pinned_to("claude4"), &router, 3).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(upstream.calls(), 2);
        assert_eq!(
            out.retries,
            vec!["⏳ API error, retrying (1/3)…".to_string()],
            "{:?}",
            out.notices
        );
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn short_reset_exhaustion_without_hop_falls_through_to_retry() {
        // Explicit account, window rejected but the reset is under the
        // long-reset threshold: report, notify, then today's backoff retry.
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 30 }, Reply::SseOk]).await;
        let router = ScriptedRouter::new(false, Ok(None));
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(upstream.calls(), 2);
        assert_eq!(router.reports().len(), 1);
        assert!(router.failovers().is_empty());
    }

    // ── Capacity wait on a window-limit 429 (site 2) ─────────────────────

    type Lookup = Result<Result<PinnedToken, Option<crate::auth::SelectionFailure>>, String>;

    /// Automatic-selection router for capacity waits: answers lookup N with
    /// answer N (the last repeats) and records each lookup's attempted set
    /// and time. With `pause_on_no_account`, the first lookup that finds no
    /// account pauses tokio's clock (the wait then sleeps on the paused
    /// clock) and a lookup that finds one resumes it (the resend is real
    /// loopback I/O, never under a paused clock).
    struct WaitRouter {
        answers: Mutex<Vec<Lookup>>,
        lookups: Mutex<Vec<(String, Vec<CredentialRef>, tokio::time::Instant)>>,
        reports: Mutex<usize>,
        pause_on_no_account: bool,
        paused: Mutex<bool>,
        /// A wait's re-check (nothing attempted) hangs this long first.
        stall_rechecks: Option<std::time::Duration>,
    }

    impl WaitRouter {
        fn new(answers: Vec<Lookup>, pause_on_no_account: bool) -> Self {
            Self {
                answers: Mutex::new(answers),
                lookups: Mutex::new(Vec::new()),
                reports: Mutex::new(0),
                pause_on_no_account,
                paused: Mutex::new(false),
                stall_rechecks: None,
            }
        }
        fn attempted(&self) -> Vec<(String, Vec<CredentialRef>)> {
            self.lookups
                .lock()
                .unwrap()
                .iter()
                .map(|(m, a, _)| (m.clone(), a.clone()))
                .collect()
        }
        fn times(&self) -> Vec<tokio::time::Instant> {
            self.lookups.lock().unwrap().iter().map(|(_, _, t)| *t).collect()
        }
    }

    #[async_trait::async_trait]
    impl AnthropicAccountRouter for WaitRouter {
        fn auto_selected(&self) -> bool {
            true
        }
        async fn report_exhausted(&self, _failed: &CredentialRef, _evidence: &AnthropicQuotaEvidence) {
            *self.reports.lock().unwrap() += 1;
        }
        async fn report_served(&self, _seat: &CredentialRef, _sent_at_ms: u64) {}
        async fn failover(
            &self,
            model: &str,
            attempted: &[CredentialRef],
        ) -> Result<Option<PinnedToken>, String> {
            Ok(self.failover_detailed(model, attempted).await?.ok())
        }
        async fn failover_detailed(&self, model: &str, attempted: &[CredentialRef]) -> Lookup {
            self.lookups.lock().unwrap().push((
                model.to_string(),
                attempted.to_vec(),
                tokio::time::Instant::now(),
            ));
            if let (Some(hang), true) = (self.stall_rechecks, attempted.is_empty()) {
                tokio::time::sleep(hang).await;
            }
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.len() > 1 {
                    answers.remove(0)
                } else {
                    answers[0].clone()
                }
            };
            if self.pause_on_no_account {
                let mut paused = self.paused.lock().unwrap();
                match (&answer, *paused) {
                    (Ok(Err(_)), false) => {
                        tokio::time::pause();
                        *paused = true;
                    }
                    (Ok(Ok(_)) | Err(_), true) => {
                        tokio::time::resume();
                        *paused = false;
                    }
                    _ => {}
                }
            }
            answer
        }
    }

    fn wait_on() -> Option<crate::runtime::capacity_wait::CapacityWaitPolicy> {
        Some(crate::runtime::capacity_wait::CapacityWaitPolicy {
            jitter: crate::runtime::capacity_wait::Jitter::Fixed(std::time::Duration::ZERO),
            ..Default::default()
        })
    }

    /// `drive` must end within 30 s of real time: a request that must not
    /// wait fails here (rather than sleeping for its reset) if it waits.
    async fn no_wait(drive: impl std::future::Future<Output = Outcome>) -> Outcome {
        tokio::time::timeout(std::time::Duration::from_secs(30), drive)
            .await
            .expect("the request waited; it must end at once")
    }

    /// Every other account is cooling down too, resetting in `secs`.
    fn others_cooling(secs: u64) -> crate::auth::SelectionFailure {
        let until = crate::epoch_millis() + secs * 1000;
        crate::auth::SelectionFailure {
            kind: crate::auth::SelectionFailureKind::AllCoolingDown,
            accounts: vec![crate::auth::AccountFailure {
                account: "claude2".into(),
                kind: crate::auth::AccountFailureKind::Exhausted,
                http_status: None,
                retry_after_s: None,
                until_ms: Some(until),
                cooling_down: true,
            }],
            earliest_reset_ms: Some(until),
            usage_read_retry_ms: None,
        }
    }

    /// Design test 5: a window limit 20 min from now on the request's
    /// account, nothing streamed, no other account. The request waits for
    /// the reset (checking every 10 min), selects again with nothing
    /// excluded, and sends the same request once more on the account found,
    /// without spending a 429 retry.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_window_limit_20_min_away_waits_for_the_reset_then_resends_once() {
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 20 * 60 }, Reply::SseOk]).await;
        let router = WaitRouter::new(
            vec![
                Ok(Err(Some(others_cooling(40 * 60)))),
                Ok(Err(Some(others_cooling(40 * 60)))),
                Ok(Ok(pinned("claude2"))),
            ],
            true,
        );
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait_on()).await;

        let value = out.result.expect("the request carries on after the reset");
        assert_eq!(value["content"][0]["text"], "hi");
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude2".to_string()],
            "one resend, on the account found"
        );
        let model = "claude-haiku-4-5".to_string();
        assert_eq!(
            router.attempted(),
            vec![
                (model.clone(), vec![seat("claude4")]),
                (model.clone(), vec![]),
                (model.clone(), vec![]),
            ],
            "the failover lookup, then a check every 10 min with nothing excluded"
        );
        let t = router.times();
        let waited = t[2] - t[0];
        assert!(
            waited >= std::time::Duration::from_secs(20 * 60 - 1)
                && waited <= std::time::Duration::from_secs(20 * 60 + 1),
            "waited {waited:?}"
        );
        assert!(t[1] - t[0] <= std::time::Duration::from_secs(10 * 60 + 1), "{t:?}");
        assert_eq!(*router.reports.lock().unwrap(), 1);
        let waits: Vec<_> = out.notices.iter().filter(|n| n.starts_with("⏸ ")).collect();
        assert_eq!(waits.len(), 1, "{:?}", out.notices);
        assert!(
            waits[0].starts_with("⏸ Every Anthropic account is cooling down or at a usage limit [claude2: "),
            "{}",
            waits[0]
        );
        assert!(waits[0].contains("; waiting until "), "{}", waits[0]);
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-claude2");
        assert_eq!(
            g.bound_credential.as_deref(),
            Some(anthropic_binding(&CredentialSource::Local, &seat("claude2")).as_str())
        );
        for n in &out.notices {
            assert_no_body_leak(n, "notice");
        }
    }

    /// Pin: a window limit 4 min away (under the long-reset threshold)
    /// stays on the 429 retry path, with no capacity wait.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_window_limit_4_min_away_keeps_the_retry_path_and_never_waits() {
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 4 * 60 }, Reply::SseOk]).await;
        let router = WaitRouter::new(vec![Ok(Err(Some(others_cooling(40 * 60))))], false);
        let auth = oauth_auth_pinned_to("claude4");

        let out = no_wait(drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait_on())).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude4".to_string()],
            "today's backoff retry, on the same account"
        );
        assert_eq!(router.attempted().len(), 1, "the failover lookup only");
        assert!(!out.notices.iter().any(|n| n.starts_with("⏸ ")), "{:?}", out.notices);
    }

    /// A request that already streamed output never waits (and never
    /// resends): its window-limit 429 ends it as today.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_window_limit_after_output_streamed_never_waits() {
        let upstream = spawn_upstream(vec![
            Reply::SsePartialThenOverloaded,
            Reply::Rejected429 { reset_in_secs: 20 * 60 },
        ])
        .await;
        let router = WaitRouter::new(vec![Ok(Ok(pinned("claude2")))], false);
        let auth = oauth_auth_pinned_to("claude4");

        let out = no_wait(drive_router(&upstream, Arc::clone(&auth), &router, 1, None, wait_on())).await;

        let msg = out.result.expect_err("ends as today").to_string();
        assert!(msg.contains("usage limit reached on account 'claude4'"), "{msg}");
        assert_eq!(upstream.calls(), 2, "no resend after the 429");
        assert!(router.attempted().is_empty(), "no lookup at all");
        assert!(!out.notices.iter().any(|n| n.starts_with("⏸ ")), "{:?}", out.notices);
        assert_eq!(auth.read().await.auth_token, "tok-claude4");
    }

    /// A keep-warm ping never waits, even with the wait on: one request, on
    /// the session's seat.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_keepwarm_ping_on_a_window_limit_never_waits() {
        use super::{KeepWarmMode, KeepWarmRequest, TailPurpose};
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 20 * 60 }, Reply::SseOk]).await;
        let router = WaitRouter::new(vec![Ok(Ok(pinned("claude2")))], false);
        let auth = oauth_auth_pinned_to("claude4");
        let ping = |mode| KeepWarmRequest {
            mode,
            purpose: TailPurpose::Ping,
            stop_reason: Default::default(),
            usage: Default::default(),
            status: Default::default(),
            sent: Default::default(),
            connect_failed: Default::default(),
            failure_kind: Default::default(),
            lease: None,
        };
        let probe =
            drive_router(&upstream, Arc::clone(&auth), &router, 0, Some(ping(KeepWarmMode::Probe)), wait_on())
                .await;
        let expect = probe.result.expect("probe")["keepwarm_prefix"]
            .as_u64()
            .expect("the probe returns the prefix");
        let out = no_wait(drive_router(
            &upstream,
            Arc::clone(&auth),
            &router,
            0,
            Some(ping(KeepWarmMode::Send { expect })),
            wait_on(),
        ))
        .await;

        assert!(out.result.is_err(), "the ping fails: {:?}", out.result);
        assert_eq!(upstream.calls(), 1);
        assert!(router.attempted().is_empty(), "no lookup");
        assert!(!out.notices.iter().any(|n| n.starts_with("⏸ ")), "{:?}", out.notices);
    }

    /// Review B2: one capacity-wait resend per request. After the wait, the
    /// resend (on the same seat, found again by the re-check) meets another
    /// window limit, and no other account has capacity: the request ends as
    /// today, with no second wait. Two sends with the same body, no failover
    /// counted, no 429 retry slot used (none are left: `max_retries = 0`).
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_second_window_limit_after_the_capacity_resend_ends_the_request_as_today() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 { reset_in_secs: 20 * 60 },
            Reply::Rejected429 { reset_in_secs: 20 * 60 },
        ])
        .await;
        let router = WaitRouter::new(
            vec![
                Ok(Err(Some(others_cooling(40 * 60)))),
                Ok(Ok(pinned("claude4"))),
                Ok(Err(Some(others_cooling(40 * 60)))),
                Ok(Ok(pinned("claude4"))),
            ],
            true,
        );
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait_on()).await;

        let msg = out.result.expect_err("ends as today").to_string();
        assert!(msg.contains("usage limit reached on account 'claude4'"), "{msg}");
        assert_eq!(upstream.calls(), 2, "the request and one resend, no more");
        let bodies = upstream.bodies();
        assert_eq!(bodies[0], bodies[1], "the resend is the same request");
        assert_eq!(
            upstream.auth_seen(),
            vec!["Bearer tok-claude4".to_string(), "Bearer tok-claude4".to_string()]
        );
        let model = "claude-haiku-4-5".to_string();
        assert_eq!(
            router.attempted(),
            vec![
                (model.clone(), vec![seat("claude4")]),
                (model.clone(), vec![]),
                (model.clone(), vec![seat("claude4")]),
            ],
            "the lookup, one re-check, then the second 429's lookup and no wait"
        );
        let waits: Vec<_> = out.notices.iter().filter(|n| n.starts_with("⏸ ")).collect();
        assert_eq!(waits.len(), 1, "one wait: {:?}", out.notices);
        assert!(!out.notices.iter().any(|n| n.contains("switching to")), "no failover: {:?}", out.notices);
        assert!(out.retries.is_empty(), "no retry slot used: {:?}", out.retries);
    }

    /// Phase 2, E2: a window limit with no other account, beyond the wait's
    /// bound (3 days), or one the wait would reach (20 min) with
    /// `capacity_wait` off: the request ends as today, and the turn's
    /// completion holds the stop with the earlier reset (here the request's
    /// own; every other account resets in 4 days).
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_window_limit_stop_is_marked_with_its_reset() {
        for (reset_in_secs, wait) in [(3 * 24 * 3600, wait_on()), (20 * 60, None)] {
            let upstream = spawn_upstream(vec![Reply::Rejected429 { reset_in_secs }]).await;
            let router = WaitRouter::new(vec![Ok(Err(Some(others_cooling(4 * 24 * 3600))))], false);
            let auth = oauth_auth_pinned_to("claude4");
            let completion = crate::runtime::TurnCompletion::new();

            let out = no_wait(crate::runtime::capacity_wait::TURN_COMPLETION.scope(
                completion.clone(),
                drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait),
            ))
            .await;

            let msg = out.result.expect_err("ends as today").to_string();
            assert!(msg.contains("usage limit reached on account 'claude4'"), "{msg}");
            assert_eq!(upstream.calls(), 1, "no resend");
            assert!(!out.notices.iter().any(|n| n.starts_with("⏸ ")), "{:?}", out.notices);
            let stop = completion.take_capacity_stop().expect("the stop is marked");
            let want = crate::epoch_millis() + reset_in_secs * 1000;
            assert!(stop.resume_at_ms.abs_diff(want) <= 5_000, "{stop:?} vs {want}");
            assert_eq!(stop.kind, "all_cooling_down");
        }
    }

    /// Phase 2, E3 at site 2: a request that already streamed output ends
    /// on its window limit as today, and is no capacity stop to resume.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_window_limit_after_output_streamed_is_not_marked() {
        let upstream = spawn_upstream(vec![
            Reply::SsePartialThenOverloaded,
            Reply::Rejected429 { reset_in_secs: 3 * 24 * 3600 },
        ])
        .await;
        let router = WaitRouter::new(vec![Ok(Ok(pinned("claude2")))], false);
        let auth = oauth_auth_pinned_to("claude4");
        let completion = crate::runtime::TurnCompletion::new();

        let out = no_wait(crate::runtime::capacity_wait::TURN_COMPLETION.scope(
            completion.clone(),
            drive_router(&upstream, Arc::clone(&auth), &router, 1, None, None),
        ))
        .await;

        assert!(out.result.is_err(), "ends as today");
        assert_eq!(upstream.calls(), 2, "no resend after the 429");
        assert_eq!(completion.take_capacity_stop(), None);
    }

    /// Review B1 at site 2, and r2 B1b: re-checks that hang are each dropped
    /// at the next cadence point and the next starts at once, so they start
    /// 10 min apart; the last is cut at the wait's bound (5 h 15 min) and
    /// the request ends as today, with no resend.
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_stalled_window_limit_check_is_cut_at_the_bound() {
        let upstream = spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 20 * 60 }]).await;
        let router = WaitRouter {
            stall_rechecks: Some(std::time::Duration::from_secs(6 * 3600)),
            ..WaitRouter::new(
                vec![Ok(Err(Some(others_cooling(40 * 60)))), Ok(Ok(pinned("claude2")))],
                true,
            )
        };
        let auth = oauth_auth_pinned_to("claude4");

        let out = drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait_on()).await;

        let msg = out.result.expect_err("ends as today").to_string();
        assert!(msg.contains("usage limit reached on account 'claude4'"), "{msg}");
        assert_eq!(upstream.calls(), 1, "no resend after the cut");
        // The lookup, then a re-check every 10 min from 10 min to 5 h 10 min
        // (each dropped while it hangs); the one at 5 h 10 min is cut at the
        // bound.
        let times = router.times();
        assert_eq!(router.attempted().len(), 32, "the lookup and 31 stalled re-checks");
        let ten_min = std::time::Duration::from_secs(600);
        for pair in times.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap <= ten_min + std::time::Duration::from_secs(1), "a gap of {gap:?}");
        }
        for pair in times[1..].windows(2) {
            // Tokio's timers fire on whole milliseconds: a cut can land up
            // to 1 ms after its deadline.
            let gap = pair[1] - pair[0];
            assert!(
                gap >= ten_min && gap <= ten_min + std::time::Duration::from_millis(1),
                "re-checks start 10 min apart: {gap:?}"
            );
        }
        assert_eq!(auth.read().await.auth_token, "tok-claude4", "no late answer is bound");
    }

    /// Review S2: a re-check that fails logs a fixed class, never the
    /// broker's error text (which may carry anything).
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn a_failed_window_limit_check_logs_a_fixed_class_and_never_the_error_text() {
        const SECRET: &str =
            "sk-ant-oat01-SECRETTOKEN0123 for alice@example.com acct_9f8e7d seat 4c1d0a9b";
        let upstream = spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 20 * 60 }]).await;
        let router = WaitRouter::new(
            vec![Ok(Err(Some(others_cooling(40 * 60)))), Err(SECRET.to_string())],
            true,
        );
        let auth = oauth_auth_pinned_to("claude4");

        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || LogCapture(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let out = drive_router(&upstream, Arc::clone(&auth), &router, 0, None, wait_on()).await;
        drop(guard);

        let msg = out.result.expect_err("ends as today").to_string();
        assert!(msg.contains("usage limit reached on account 'claude4'"), "{msg}");
        assert_eq!(upstream.calls(), 1);
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let line = logs
            .lines()
            .find(|l| l.contains("capacity wait: looking up an account failed"))
            .unwrap_or_else(|| panic!("the failed check is logged: {logs}"));
        assert!(line.contains("error_class=\"lookup_failed\"") || line.contains("error_class=lookup_failed"), "{line}");
        for leak in ["sk-ant", "SECRETTOKEN", "alice@example.com", "acct_9f8e7d", "4c1d0a9b"] {
            assert!(!logs.contains(leak), "{leak} reached the log: {logs}");
        }
        for n in &out.notices {
            assert!(!n.contains("sk-ant") && !n.contains('@'), "{n}");
        }
        assert!(!msg.contains("sk-ant") && !msg.contains('@'), "{msg}");
    }

    /// `std::io::Write` sink appending to a shared buffer (in-process
    /// capture of formatted tracing output).
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn schema_capture_logs_headers_once_and_never_the_body() {
        let upstream = spawn_upstream(vec![
            Reply::Rejected429 { reset_in_secs: 3 * 3600 },
            Reply::Rejected429 { reset_in_secs: 3 * 3600 },
        ])
        .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = oauth_auth_pinned_to("claude4");

        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || LogCapture(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;
        assert!(out.result.is_err());
        drop(_guard);

        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("anthropic 429 headers (schema capture)").count(),
            1,
            "one schema-capture line per turn, even with two 429s: {logs}"
        );
        assert!(
            logs.contains("anthropic-ratelimit-unified-status=rejected"),
            "{logs}"
        );
        assert!(
            logs.contains("anthropic-ratelimit-unified-reset="),
            "{logs}"
        );
        assert_no_body_leak(&logs, "tracing output");
        assert!(!logs.contains("tok-claude"), "bearer material in logs: {logs}");
        for n in &out.notices {
            assert_no_body_leak(n, "notice");
        }
        assert_no_body_leak(&out.result.unwrap_err().to_string(), "error");
    }

    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn api_key_auth_never_classifies_or_reports() {
        // Same rejected headers, but api-key auth has no seats: the loop
        // must take today's plain backoff path and touch no seat logic.
        let upstream =
            spawn_upstream(vec![Reply::Rejected429 { reset_in_secs: 3 * 3600 }, Reply::SseOk])
                .await;
        let router = ScriptedRouter::new(true, Ok(Some(pinned("claude2"))));
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-key".to_string(),
            auth_type: "api_key".to_string(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
            bound_credential: None,
        }));

        let out = drive(&upstream, Arc::clone(&auth), &router, 0).await;

        assert!(out.result.is_ok(), "{:?}", out.result.err());
        assert_eq!(upstream.calls(), 2);
        assert!(router.reports().is_empty());
        assert!(router.failovers().is_empty());
        assert!(out
            .notices
            .iter()
            .all(|n| !n.contains("exhausted") && !n.contains("account limit")));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 5 — honest cloud tool capability (spec §5.5).
//
// Cloud broker routes are text-only until full tool translation exists. A
// mode that requires tools must fail with a typed unsupported-capability
// error BEFORE any credential lookup or network access. The counting stub
// broker below receives every credential/catalog/invoke request a Remote
// source could make; the pre-flight is proven by the counter staying at 0.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod cloud_capability_tests {
    use axum::{http::StatusCode, Router};
    use reqwest::Client;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, RwLock};

    use super::{ApiMethods, ApiOptions};
    use crate::auth::{CredentialSource, TokenCache};
    use crate::runtime::telemetry::TelemetryLevel;
    use crate::runtime::types::AuthState;
    use crate::{StreamEvent, ToolRegistry};

    /// Stub broker that counts EVERY inbound request (any method, any path):
    /// token vend, cloud catalog, and cloud invoke all land here for a Remote
    /// credential source, so `hits == 0` proves zero credential use and zero
    /// network activity.
    async fn spawn_counting_stub() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let app = Router::new().fallback(move || {
            let hits = Arc::clone(&hits_clone);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::INTERNAL_SERVER_ERROR
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    async fn drive_cloud(tools: ToolRegistry) -> (crate::error::Result<serde_json::Value>, usize) {
        let (endpoint, hits) = spawn_counting_stub().await;
        let options = ApiOptions {
            credential_source: CredentialSource::Remote {
                endpoint,
                machine_token: "machine-tok".into(),
            },
            token_cache: TokenCache::new(),
            ..Default::default()
        };
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "unused".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let client = Client::new();
        let messages = vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        let result = ApiMethods::call_api_stream(
            &auth,
            &client,
            "aws-bedrock/anthropic.claude-3-haiku#synaps-context=ctx-test",
            &tools,
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages,
            tx,
            0,
            0,
            &options,
            TelemetryLevel::Off,
        )
        .await;
        (result, hits.load(Ordering::SeqCst))
    }

    /// RED at HEAD: no pre-flight exists, so the request reaches the stub
    /// broker (hits > 0) and fails with a transport-ish error instead of the
    /// typed unsupported-capability error. GREEN once the pre-flight fires
    /// before `broker_from_source`.
    #[tokio::test]
    async fn tool_requiring_cloud_route_fails_before_credentials_or_network() {
        // Default chat registry: tools are required.
        let (result, hits) = drive_cloud(ToolRegistry::new()).await;
        let msg = result
            .expect_err("tool-requiring cloud route must fail")
            .to_string();
        assert!(
            msg.contains("text-only") && msg.contains("tools"),
            "expected typed unsupported-capability error, got: {msg}"
        );
        assert_eq!(
            hits, 0,
            "cloud pre-flight must reject tools before any credential or network use"
        );
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use axum::{body::Body, http::Response, routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_auth() -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: "offline-test-key".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    async fn call(
        auth: &Arc<RwLock<AuthState>>,
        cancel: &CancellationToken,
        tx: mpsc::UnboundedSender<StreamEvent>,
        options: &ApiOptions,
    ) -> Result<Value> {
        ApiMethods::call_api_stream_inner(
            auth,
            &Client::builder().no_proxy().build().unwrap(),
            "claude-haiku-4-5-20251001",
            &ToolRegistry::new(),
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &[Arc::new(json!({"role":"user", "content":"offline test"}))],
            tx,
            cancel,
            3,
            1,
            options,
            TelemetryLevel::Off,
        )
        .await
    }

    #[derive(Clone, Copy)]
    enum Stall {
        Headers,
        ErrorBody,
        Retry,
        Sse,
        /// [`SSE_AFTER_DELTA`], then the stream stays open.
        SseAfterDelta,
    }

    /// message_start (12/1, cache 5/3), a counted message_delta (cumulative
    /// 12/7, cache 5/3, no stop_reason), then the text "partial"; no
    /// message_stop. The delta is sent before the text so that receiving the
    /// text proves the delta was parsed, however the bytes are chunked.
    const SSE_AFTER_DELTA: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"offline\",",
        "\"usage\":{\"input_tokens\":12,\"output_tokens\":1,",
        "\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":3}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null},",
        "\"usage\":{\"input_tokens\":12,\"output_tokens\":7,",
        "\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":3}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    );

    /// Loopback only, no credentials/config/environment changes. All streams
    /// deliberately stay open so cancellation, not EOF/client timeout, wins.
    async fn stalled_server(
        stall: Stall,
    ) -> (
        ApiOptions,
        Arc<AtomicUsize>,
        mpsc::UnboundedReceiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let (seen, rx) = mpsc::unbounded_channel();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let hits = counter.clone();
                let seen = seen.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = seen.send(());
                    if matches!(stall, Stall::Headers) {
                        return std::future::pending::<Response<Body>>().await;
                    }
                    if matches!(stall, Stall::Retry) {
                        return Response::builder()
                            .status(429)
                            .header("retry-after", "60")
                            .body(Body::from("{}"))
                            .unwrap();
                    }
                    let prefix = if matches!(stall, Stall::SseAfterDelta) {
                        SSE_AFTER_DELTA
                    } else if matches!(stall, Stall::Sse) {
                        concat!(
                            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"offline\",",
                            "\"usage\":{\"input_tokens\":12,\"output_tokens\":1,",
                            "\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":3}}}\n\n",
                            "data: {\"type\":\"content_block_start\",\"index\":0,",
                            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                            "data: {\"type\":\"content_block_delta\",\"index\":0,",
                            "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                        )
                    } else {
                        "{"
                    };
                    let chunks = futures::stream::once(async move {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(prefix.as_bytes()))
                    })
                    .chain(futures::stream::pending());
                    Response::builder()
                        .status(if matches!(stall, Stall::Sse | Stall::SseAfterDelta) {
                            200
                        } else {
                            500
                        })
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            ApiOptions {
                anthropic_base_url: Some(url),
                ..Default::default()
            },
            hits,
            rx,
            task,
        )
    }

    #[tokio::test]
    async fn pre_cancelled_call_never_waits_for_auth_or_dispatches() {
        let auth = test_auth();
        let _held = auth.write().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            call(&auth, &cancel, tx, &ApiOptions::default()),
        )
        .await
        .expect("pre-cancel must not wait for auth");
        assert!(matches!(result, Err(RuntimeError::Canceled)));
    }

    #[tokio::test]
    async fn cancellation_interrupts_auth_lock_before_dispatch() {
        let auth = test_auth();
        let _held = auth.write().await;
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let options = ApiOptions::default();
        let call = call(&auth, &cancel, tx, &options);
        tokio::pin!(call);
        assert!(futures::poll!(&mut call).is_pending());
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .expect("cancel must release the auth wait");
        assert!(matches!(result, Err(RuntimeError::Canceled)));
    }

    #[tokio::test]
    async fn cancellation_interrupts_send_and_error_body_waits() {
        for stall in [Stall::Headers, Stall::ErrorBody] {
            let (options, hits, mut seen, server) = stalled_server(stall).await;
            let auth = test_auth();
            let cancel = CancellationToken::new();
            let (tx, _rx) = mpsc::unbounded_channel();
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                let (result, ()) = tokio::join!(call(&auth, &cancel, tx, &options), async {
                    seen.recv().await.unwrap();
                    // Allow the error headers to reach the client in ErrorBody mode.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    cancel.cancel();
                });
                result
            })
            .await;
            server.abort();
            assert!(matches!(
                result.expect("cancel must not wait for response"),
                Err(RuntimeError::Canceled)
            ));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_retry_after_without_second_dispatch() {
        let (options, hits, _seen, server) = stalled_server(Stall::Retry).await;
        let auth = test_auth();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let (result, ()) = tokio::join!(call(&auth, &cancel, tx, &options), async {
                while let Some(event) = rx.recv().await {
                    if matches!(event, StreamEvent::Session(SessionEvent::Retry(_))) {
                        cancel.cancel();
                        return;
                    }
                }
                panic!("expected retry notice");
            });
            result
        })
        .await;
        server.abort();
        assert!(matches!(
            result.expect("cancel must interrupt retry-after"),
            Err(RuntimeError::Canceled)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_stalled_sse_preserves_partial_content_and_residual_usage_once() {
        let (mut options, hits, _seen, server) = stalled_server(Stall::Sse).await;
        let counters = Arc::new(crate::runtime::budget::UsageCounters::default());
        options.usage_counters = Some(counters.clone());
        let auth = test_auth();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(call(&auth, &cancel, tx, &options), async {
                let mut usage_events = 0;
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::Llm(LlmEvent::Text(text)) => {
                            assert_eq!(text, "partial");
                            cancel.cancel();
                        }
                        StreamEvent::Session(SessionEvent::Usage {
                            input_tokens,
                            output_tokens,
                            ..
                        }) => {
                            assert_eq!((input_tokens, output_tokens), (12, 1));
                            usage_events += 1;
                        }
                        _ => {}
                    }
                }
                usage_events
            })
        })
        .await;
        server.abort();
        let (response, usage_events) = result.expect("cancel must finish a stalled SSE stream");
        assert_eq!(response.unwrap()["content"][0]["text"], "partial");
        assert_eq!(usage_events, 1);
        assert_eq!(counters.totals(), (12, 1, 5, 3));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A user cancel after a counted message_delta and before message_stop
    /// (the token, as cancel_turn sends it): the read loop ends and the end of
    /// the stream sends the one Usage, with the delta's counts, not the
    /// start's. Only a dropped future (an abort) skips it.
    #[tokio::test]
    async fn cancelled_sse_after_a_counted_delta_sends_the_deltas_usage_once() {
        let (mut options, hits, _seen, server) = stalled_server(Stall::SseAfterDelta).await;
        let counters = Arc::new(crate::runtime::budget::UsageCounters::default());
        options.usage_counters = Some(counters.clone());
        let auth = test_auth();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(call(&auth, &cancel, tx, &options), async {
                let mut usage = Vec::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::Llm(LlmEvent::Text(text)) => {
                            assert_eq!(text, "partial");
                            assert!(usage.is_empty(), "nothing is sent before the end");
                            cancel.cancel();
                        }
                        StreamEvent::Session(SessionEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            ..
                        }) => usage.push((
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                        )),
                        _ => {}
                    }
                }
                usage
            })
        })
        .await;
        server.abort();
        let (response, usage) = result.expect("cancel must finish a stalled SSE stream");
        assert_eq!(response.unwrap()["content"][0]["text"], "partial");
        assert_eq!(usage, vec![(12, 7, 5, 3)], "one Usage, the delta's counts");
        assert_eq!(counters.totals(), (12, 7, 5, 3), "the root counted it once");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    // ── Progress deadlines: a stream that only pings is retried ──────────

    /// Anthropic's keep-alive frame.
    const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";

    /// A complete reply: the text "second attempt", end_turn.
    const COMPLETE: &str = concat!(
        r#"data: {"type":"message_start","message":{"id":"offline","usage":{"input_tokens":12,"output_tokens":1}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"second attempt"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    /// Text, then a tool_use whose input is cut off mid-JSON.
    const PARTIAL_TOOL: &str = concat!(
        r#"data: {"type":"message_start","message":{"id":"offline","usage":{"input_tokens":12,"output_tokens":1}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"first attempt"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_partial","name":"bash"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"rm"}}"#,
        "\n\n",
    );

    const THINKING_START: &str = concat!(
        r#"data: {"type":"message_start","message":{"id":"offline","usage":{"input_tokens":12,"output_tokens":1}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        "\n\n",
    );

    const THINKING_DELTA: &str = concat!(
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        "\n\n",
    );

    const THINKING_END: &str = concat!(
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    /// One attempt's body: each frame after its delay in ms; then, when
    /// `pings` is set, a ping every `pings` ms forever, else the body's end.
    fn scripted(frames: Vec<(u64, &'static str)>, pings: Option<u64>) -> Body {
        let script = futures::stream::iter(frames).then(|(ms, frame)| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(frame.as_bytes()))
        });
        let pings = futures::stream::unfold((), move |()| async move {
            let every = match pings {
                Some(every) => every,
                None => return None,
            };
            tokio::time::sleep(Duration::from_millis(every)).await;
            Some((Ok(bytes::Bytes::from_static(PING.as_bytes())), ()))
        });
        Body::from_stream(script.chain(pings))
    }

    /// Loopback only: answers attempt `n` (from 0) with a 200 SSE body,
    /// `script(n)`.
    async fn scripted_server(
        script: fn(usize) -> Body,
    ) -> (ApiOptions, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let body = script(counter.fetch_add(1, Ordering::SeqCst));
                async move {
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(body)
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            ApiOptions {
                anthropic_base_url: Some(url),
                ..Default::default()
            },
            hits,
            task,
        )
    }

    /// `call` under shrunk (first event, progress) deadlines: its result
    /// and every event it sent, in order.
    async fn call_within(
        options: &ApiOptions,
        limits: (Duration, Duration),
    ) -> (Result<Value>, Vec<StreamEvent>) {
        let auth = test_auth();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            crate::runtime::http_phases::STREAM_PROGRESS_FOR_TEST
                .scope(limits, call(&auth, &cancel, tx, options)),
        )
        .await
        .expect("the request must end well within 10 s");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    /// The live symptom: headers, then only pings, which keep the raw-byte
    /// idle timer alive. The first-event deadline (the only limit in reach:
    /// progress is 60 s) ends the attempt, the request is retried, and the
    /// result is the second attempt's reply.
    #[tokio::test]
    async fn ping_only_stream_is_retried_after_the_first_event_deadline() {
        let (options, hits, server) = scripted_server(|attempt| match attempt {
            0 => scripted(Vec::new(), Some(20)),
            _ => scripted(vec![(0, COMPLETE)], None),
        })
        .await;
        let limits = (Duration::from_millis(300), Duration::from_secs(60));
        let (result, events) = call_within(&options, limits).await;
        server.abort();
        assert_eq!(
            result.unwrap()["content"],
            json!([{"type": "text", "text": "second attempt"}])
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Llm(LlmEvent::ResponseReset))));
    }

    /// The first-event deadline is spent once per request: a cold prefill of
    /// a huge prompt may outlast it, so later attempts wait the progress
    /// window for message_start. Attempt 2 pings past the first-event limit,
    /// under the progress one, then replies: two requests, one retry.
    #[tokio::test]
    async fn first_event_deadline_relaxes_after_it_fires_once() {
        let (options, hits, server) = scripted_server(|attempt| match attempt {
            0 => scripted(Vec::new(), Some(20)),
            _ => {
                let mut frames = vec![(100, PING); 7];
                frames.push((0, COMPLETE));
                scripted(frames, None)
            }
        })
        .await;
        let limits = (Duration::from_millis(300), Duration::from_secs(2));
        let (result, events) = call_within(&options, limits).await;
        server.abort();
        assert_eq!(
            result.unwrap()["content"],
            json!([{"type": "text", "text": "second attempt"}])
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let retries = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Retry(_))))
            .count();
        assert_eq!(retries, 1, "no second deadline");
    }

    /// Text and a tool_use cut off mid-input, then only pings: the progress
    /// deadline (the only limit in reach once message_start came) ends the
    /// attempt and it is retried. The result is the second attempt's reply
    /// alone, the previews are reset before the retry, and the partial call
    /// never completed: no ToolUse event and no tool_use block for the tool
    /// loop to run.
    #[tokio::test]
    async fn partial_tool_use_then_pings_is_retried_after_the_progress_deadline() {
        let (options, hits, server) = scripted_server(|attempt| match attempt {
            0 => scripted(vec![(0, PARTIAL_TOOL)], Some(20)),
            _ => scripted(vec![(0, COMPLETE)], None),
        })
        .await;
        let limits = (Duration::from_secs(60), Duration::from_millis(300));
        let (result, events) = call_within(&options, limits).await;
        server.abort();
        assert_eq!(
            result.unwrap()["content"],
            json!([{"type": "text", "text": "second attempt"}])
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let at = |wanted: fn(&StreamEvent) -> bool| events.iter().position(wanted).unwrap();
        let first =
            at(|e| matches!(e, StreamEvent::Llm(LlmEvent::Text(t)) if t == "first attempt"));
        let tool_start = at(
            |e| matches!(e, StreamEvent::Llm(LlmEvent::ToolUseStart { tool_id, .. }) if tool_id == "toolu_partial"),
        );
        let reset = at(|e| matches!(e, StreamEvent::Llm(LlmEvent::ResponseReset)));
        let second =
            at(|e| matches!(e, StreamEvent::Llm(LlmEvent::Text(t)) if t == "second attempt"));
        assert!(first < tool_start && tool_start < reset && reset < second);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Llm(LlmEvent::ToolUse { .. }))),
            "the partial call never completed"
        );
    }

    /// No false fire: thinking deltas 700 ms apart with pings between them,
    /// under a 1 s progress limit, for over 2 s in all. Each delta is
    /// progress, so the stream completes on its first attempt.
    #[tokio::test]
    async fn spaced_thinking_deltas_with_pings_never_reach_the_progress_deadline() {
        let (options, hits, server) = scripted_server(|_| {
            let mut frames = vec![(0, THINKING_START)];
            for _ in 0..3 {
                frames.extend([(100, PING); 6]);
                frames.push((100, THINKING_DELTA));
            }
            frames.push((0, THINKING_END));
            scripted(frames, None)
        })
        .await;
        let limit = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let (result, events) = call_within(&options, (limit, limit)).await;
        server.abort();
        assert!(started.elapsed() > limit, "the stream outlasted the limit");
        assert_eq!(
            result.unwrap()["content"],
            json!([
                {"type": "thinking", "thinking": "hmhmhm", "signature": "sig"},
                {"type": "text", "text": "done"}
            ])
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!events.iter().any(|e| matches!(
            e,
            StreamEvent::Llm(LlmEvent::ResponseReset)
                | StreamEvent::Session(SessionEvent::Retry(_))
        )));
    }
}
