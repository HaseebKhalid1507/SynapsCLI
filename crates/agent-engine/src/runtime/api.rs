use super::helpers::HelperMethods;
use super::sse_types::{AnthropicEvent, ContentBlock, Delta};
use super::types::{AuthState, LlmEvent, SessionEvent, StreamEvent};
use crate::runtime::telemetry::{self, TelemetryLevel};
use crate::{Result, RuntimeError, ToolRegistry};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

/// Truncate to at most `max` bytes without slicing mid-UTF-8-codepoint.
/// Used for forensic logging of unknown event lines.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
    // ── Telemetry captures ──
    telem_msg_id: Option<String>,
    telem_ttft: Option<u64>,
    telem_stop_reason: Option<String>,
    telem_usage: telemetry::UsageRecord,
    first_event_seen: bool,
    // ── Usage captured from message_start (single-emission protocol) ──
    // Live API shape (probed 2024+): message_start carries the full
    // `cache_creation` sub-object; message_delta carries ONLY the aggregate
    // (but repeats input/cache_read/cache_create and brings the final
    // output count). The start arm CAPTURES ONLY — the delta arm emits THE
    // single authoritative Usage event, falling back to these values for
    // anything the delta doesn't carry. Emitting from both arms double-counts
    // input/cache in every accumulating consumer (the pre-cache-ttl
    // double-billing bug).
    msg_start_input: Option<u64>,
    msg_start_output: Option<u64>,
    msg_start_cache_read: Option<u64>,
    msg_start_cache_create: Option<u64>,
    msg_start_cache_5m: Option<u64>,
    msg_start_cache_1h: Option<u64>,
    /// Set once the authoritative Usage event has been emitted. The residual
    /// path (stream died between start and delta) checks this so dead streams
    /// still bill what message_start reported — exactly one emission per
    /// request in all paths.
    usage_emitted: bool,
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
}

/// A failure surfaced as an in-stream Anthropic `error` event under a 200
/// response. `retryable` follows Anthropic's error taxonomy: transient classes
/// (`overloaded_error`, `api_error`, `rate_limit_error`) are retried with
/// backoff; everything else (`request_too_large`, `invalid_request_error`,
/// auth/permission) is terminal — retrying a context-overflow never helps.
#[derive(Debug, Clone)]
struct StreamError {
    /// Human-facing, actionable message (already names the error type + body).
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
    Retry(String),
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
            telem_msg_id: None,
            telem_ttft: None,
            telem_stop_reason: None,
            telem_usage: telemetry::UsageRecord::default(),
            first_event_seen: false,
            msg_start_input: None,
            msg_start_output: None,
            msg_start_cache_read: None,
            msg_start_cache_create: None,
            msg_start_cache_5m: None,
            msg_start_cache_1h: None,
            usage_emitted: false,
            stream_error: None,
            stop_reason_seen: false,
            stop_reason_is_refusal: false,
        }
    }

    /// End-of-stream flush of any partial thinking/tool/text block.
    /// Idempotent: clears `in_*` and `current_text` so a second call is a no-op.
    fn finalize(&mut self) {
        if self.in_thinking {
            // Never emit an empty `thinking` field — Anthropic rejects such
            // blocks on the next turn (see content_block_stop arm).
            if !self.current_thinking.is_empty() {
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
        } else if !self.current_text.is_empty() {
            self.accumulated_content.push(json!({
                "type": "text",
                "text": self.current_text
            }));
        }
        self.current_text.clear();
    }
}

/// Immutable per-stream context — not state. `tx` deliberately lives here and
/// not in `ParseState`: read/write separation is the point.
struct EventCtx<'t> {
    tx: &'t mpsc::UnboundedSender<StreamEvent>,
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

/// Handle one parsed SSE event. The TTFT capture rides along so the main and
/// tail paths are uniform. `raw` is the un-parsed data line — `#[serde(other)]`
/// discards the tag on Unknown events, so the raw line is the only forensics.
fn process_event(event: AnthropicEvent<'_>, raw: &str, state: &mut ParseState, ctx: &EventCtx) {
    // ═══ TELEMETRY: capture TTFT on first event ═══
    if !state.first_event_seen && ctx.telemetry_level.enabled() {
        state.telem_ttft = Some(ctx.request_start.elapsed().as_millis() as u64);
        state.first_event_seen = true;
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
                let _ = ctx
                    .tx
                    .send(StreamEvent::Llm(LlmEvent::Text(text.into_owned())));
            }
            Delta::ThinkingDelta { thinking } => {
                // Anthropic sends thinking text in delta.thinking
                state.current_thinking.push_str(&thinking);
                let _ = ctx
                    .tx
                    .send(StreamEvent::Llm(LlmEvent::Thinking(thinking.into_owned())));
            }
            Delta::SignatureDelta { signature } => {
                state.current_thinking_signature = signature.into_owned();
            }
            Delta::InputJsonDelta { partial_json } => {
                state.current_tool_input_json.push_str(&partial_json);
                let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::ToolUseDelta {
                    tool_id: state.current_tool_id.clone(),
                    delta: partial_json.into_owned(),
                }));
            }
            // Unknown delta subtype: no state change, mirrors the old `_ => {}`.
            Delta::Unknown => {}
        },
        AnthropicEvent::ContentBlockStop => {
            if state.in_thinking {
                // Flush thinking block with signature so it's echoed back in tool loops.
                // CRITICAL: never emit an empty `thinking` field — Anthropic rejects
                // such blocks on the next turn with
                // `messages.N.content.M.thinking: each thinking block must contain thinking`.
                // Empty blocks happen when the stream produced only a signature delta
                // (or none at all) before the block_stop arrived.
                if !state.current_thinking.is_empty() {
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
            // Capture stop_reason UNCONDITIONALLY — classify_stream_outcome relies
            // on it to tell a valid empty end_turn from a silent stop, and that
            // decision must NOT depend on whether telemetry is enabled (default off).
            if let Some(sr) = delta.and_then(|d| d.stop_reason) {
                state.stop_reason_seen = true;
                if sr.as_ref() == "refusal" {
                    state.stop_reason_is_refusal = true;
                }
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
                    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
                    tracing::debug!(
                        "Token Usage: {} input | {} output | {} cache_read | {} cache_create",
                        input_t,
                        output_t,
                        cache_read,
                        cache_create
                    );
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
                    // THE single authoritative Usage emission for this
                    // request. The message_start arm captures only — every
                    // downstream consumer is an accumulator, so a second
                    // emission double-bills input/cache.
                    state.usage_emitted = true;
                    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Usage {
                        input_tokens: input_t,
                        output_tokens: output_t,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_create,
                        cache_creation_5m: cache_create_5m,
                        cache_creation_1h: cache_create_1h,
                        model: None,
                    }));
                }
            }
        }
        AnthropicEvent::MessageStart { message } => {
            // ═══ TELEMETRY: capture msg_id ═══
            if ctx.telemetry_level.enabled() {
                if let Some(id) = message.id {
                    state.telem_msg_id = Some(id.into_owned());
                }
            }
            if let Some(usage) = message.usage {
                // CAPTURE ONLY — no Usage emission from this arm. Live
                // message_delta repeats all aggregates and carries the final
                // output count, so emitting here too double-counts
                // input/cache_read/cache_create in every accumulating
                // consumer (TUI, engine, server, RPC, subagents). The delta
                // arm emits the single authoritative event; the residual
                // path covers streams that die before their delta.
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
        AnthropicEvent::MessageStop => {}
        AnthropicEvent::Error { error } => {
            // Anthropic delivers some failures (overloaded, context-overflow)
            // as an in-stream `error` event under a 200 response. Capture +
            // CLASSIFY it so the stream either retries (transient) or surfaces
            // a visible, accurate error — never the silent "stopping" swallow
            // (task #130). First error wins — later frames don't clobber it.
            if state.stream_error.is_none() {
                let (kind, body) = match &error {
                    Some(e) => (
                        e.error_type.as_deref().unwrap_or("error"),
                        e.message.as_deref(),
                    ),
                    None => ("error", None),
                };
                let retryable = StreamError::is_retryable_type(kind);
                let message = match body {
                    Some(m) => format!("API stream error ({kind}): {m}"),
                    None => format!("API stream error ({kind})"),
                };
                // Gap-1 forensics: the raw frame is the ground truth for the
                // next occurrence — log it once, verbatim (bounded).
                tracing::warn!(
                    retryable,
                    raw = %truncate_at_char_boundary(raw, 400),
                    "SSE error event: {message}"
                );
                state.stream_error = Some(StreamError { message, retryable });
            }
        }
        AnthropicEvent::Unknown => {
            // #[serde(other)] discarded the tag — the raw line is the only
            // forensics. Covers `ping` and future event types (`error` is now
            // a real arm above).
            tracing::trace!(
                "Unknown SSE event type: {}",
                truncate_at_char_boundary(raw, 200)
            );
        }
    }
}

/// Residual Usage emission for dead streams: if message_start reported usage
/// but the stream terminated before message_delta could emit the single
/// authoritative event, bill what the start reported. Idempotent via
/// `usage_emitted` — exactly one emission per request in all paths.
fn emit_residual_usage(state: &mut ParseState, ctx: &EventCtx) {
    if state.usage_emitted {
        return;
    }
    let input_t = state.msg_start_input.unwrap_or(0);
    let output_t = state.msg_start_output.unwrap_or(0);
    let cache_read = state.msg_start_cache_read.unwrap_or(0);
    let cache_create = state.msg_start_cache_create.unwrap_or(0);
    if input_t == 0 && output_t == 0 && cache_read == 0 && cache_create == 0 {
        return; // never saw usage (or all-zero) — nothing to bill
    }
    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
    tracing::debug!("Token Usage (residual, dead stream): {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
    if ctx.telemetry_level.enabled() {
        state.telem_usage.input = input_t;
        state.telem_usage.output = output_t;
        state.telem_usage.cache_read = cache_read;
        state.telem_usage.cache_write = cache_create;
        state.telem_usage.cache_write_5m = state.msg_start_cache_5m;
        state.telem_usage.cache_write_1h = state.msg_start_cache_1h;
        state.telem_usage.compute_hit_pct();
    }
    state.usage_emitted = true;
    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Usage {
        input_tokens: input_t,
        output_tokens: output_t,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_create,
        cache_creation_5m: state.msg_start_cache_5m,
        cache_creation_1h: state.msg_start_cache_1h,
        model: None,
    }));
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
            StreamOutcome::Retry(e.message)
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

/// Extensible — new flags go here instead of adding parameters to 4 signatures.
#[derive(Debug, Clone, Default)]
pub struct ApiOptions {
    /// Opt into the 1M context window beta header.
    pub use_1m_context: bool,
    /// Prompt-cache TTL strategy (spec: cache-ttl). Default `FiveMinutes`
    /// emits payloads byte-identical to the pre-feature release.
    pub cache_ttl: crate::core::config::CacheTtl,
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
    /// Execution authority produced by Runtime preflight. Transports must not
    /// recreate authority for Max/UltraCode from assumed prerequisites.
    pub anthropic_execution_plan: Option<crate::runtime::openai::catalog::AnthropicExecutionPlan>,
    /// Foreground/worker/internal request role.
    pub codex_request_role: crate::runtime::openai::catalog::CodexRequestRole,
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
        // Cloud models always dispatch through the typed credential broker.
        let (cloud_model, cloud_context) = crate::auth::cloud::split_model_route(model);
        if let Some((provider_key, _)) = cloud_model.split_once('/') {
            if let Ok(provider) = provider_key.parse::<crate::auth::CloudProviderId>() {
                use futures::StreamExt;
                let broker = crate::auth::broker_from_source(
                    &options.credential_source,
                    &options.token_cache,
                    client.clone(),
                );
                let mut normalized = Vec::new();
                if let Some(system) = system_prompt.as_ref().filter(|s| !s.is_empty()) {
                    normalized.push(crate::auth::cloud::BrokerMessage {
                        role: crate::auth::cloud::MessageRole::System,
                        content: system.clone(),
                    });
                }
                for message in messages {
                    let role = match message["role"].as_str().unwrap_or("user") {
                        "assistant" => crate::auth::cloud::MessageRole::Assistant,
                        "system" => crate::auth::cloud::MessageRole::System,
                        "tool" => crate::auth::cloud::MessageRole::Tool,
                        _ => crate::auth::cloud::MessageRole::User,
                    };
                    let content = message["content"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| message["content"].to_string());
                    normalized.push(crate::auth::cloud::BrokerMessage { role, content });
                }
                let request = crate::auth::cloud::InvokeRequest {
                    messages: normalized,
                    tools: Vec::new(),
                    stream: true,
                    options: Default::default(),
                };
                let context_ref = cloud_context.unwrap_or(provider.as_str());
                let mut stream = broker
                    .cloud_invoke(provider, context_ref, cloud_model, request)
                    .await
                    .map_err(|e| RuntimeError::Config(e.to_string()))?;
                let mut text = String::new();
                while let Some(event) = tokio::select! { _ = cancel.cancelled() => return Err(RuntimeError::Config("cloud invocation cancelled".into())), event = stream.next() => event }
                {
                    match event.map_err(|e| RuntimeError::Config(e.to_string()))? {
                        crate::auth::broker::CloudEvent::TextDelta { delta } => {
                            text.push_str(&delta);
                            let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                                crate::runtime::types::LlmEvent::Text(delta),
                            ));
                        }
                        crate::auth::broker::CloudEvent::ToolArguments { id, name, delta } => {
                            if let Some(name) = name {
                                let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                                    crate::runtime::types::LlmEvent::ToolUseStart {
                                        tool_name: name,
                                        tool_id: id.clone(),
                                    },
                                ));
                            }
                            let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                                crate::runtime::types::LlmEvent::ToolUseDelta {
                                    tool_id: id,
                                    delta,
                                },
                            ));
                        }
                        crate::auth::broker::CloudEvent::Usage {
                            input_tokens,
                            output_tokens,
                        } => {
                            let _ = tx.send(crate::runtime::types::StreamEvent::Session(
                                crate::runtime::types::SessionEvent::Usage {
                                    input_tokens,
                                    output_tokens,
                                    cache_read_input_tokens: 0,
                                    cache_creation_input_tokens: 0,
                                    cache_creation_5m: None,
                                    cache_creation_1h: None,
                                    model: Some(model.into()),
                                },
                            ));
                        }
                        crate::auth::broker::CloudEvent::Done => break,
                    }
                }
                return Ok(serde_json::json!({"content":[{"type":"text","text":text}]}));
            }
        }
        // Route to OpenAI-compat provider if the model id resolves to one.
        let tools_schema = tools.tools_schema();
        if let Some(result) = crate::runtime::openai::try_route(
            model,
            client,
            &tools_schema,
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
        )
        .await
        {
            // Classify honestly: transport blips are API failures, not
            // config problems (incident: session 20260714-025948-3dab).
            return result.map_err(crate::runtime::openai::net::provider_error_to_runtime);
        }
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
                (!matches!(
                    reasoning_level,
                    agent_core::reasoning::ReasoningLevel::Max
                        | agent_core::reasoning::ReasoningLevel::UltraCode
                ))
                .then(|| {
                    crate::runtime::openai::catalog::plan_anthropic_execution(
                        qualified_model,
                        reasoning_level,
                        options.codex_request_role,
                        crate::runtime::openai::catalog::AnthropicPlanPrerequisites {
                            orchestration_policy: false,
                            builtin_lifecycle_tools: false,
                        },
                        None,
                    )
                    .ok()
                })
                .flatten()
            })
            .ok_or_else(|| {
                RuntimeError::Config(
                    "Anthropic special mode transport requires an authorized execution plan".into(),
                )
            })?;
        let model = model.strip_prefix("anthropic/").unwrap_or(model);

        // Read auth state for this API call
        let (mut auth_header_name, mut auth_header_value, auth_type) =
            Self::build_auth_header(auth).await;
        // C1: allow a single on-401 token refetch+retry for Remote clients.
        let mut auth_retried = false;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No Anthropic credentials. Run `synaps login`, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`.".to_string()
            ));
        }

        tracing::info!(model = %model, "Starting API request");

        // Manual cache breakpoints for optimal prompt caching.
        // Tested vs auto-cache (top-level cache_control) — manual wins: 90% vs 53% hit rate.
        // Arc-shared (#128): to_vec() on &[SharedMessage] is N refcount bumps,
        // not a deep copy; sanitize/annotate clone-on-write only what they edit.
        let mut cleaned_messages = messages.to_vec();
        // Strip empty/invalid thinking blocks before they hit the API. See
        // `sanitize_thinking_blocks` for the failure mode this guards against.
        HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, options.cache_ttl);

        // Derive the thinking level from the budget for effort mapping.
        // Body assembly (#128 Slice 4): `RequestBody` BORROWS cleaned_messages
        // and the tool schemas instead of the old `json!` assembly that deep-
        // rebuilt the entire history into a `Value` tree (copy C8). Byte-
        // identical output is enforced by `runtime::body_golden`.
        let body = super::request::RequestBody::new(
            model,
            &cleaned_messages,
            &tools_schema,
            system_prompt,
            &auth_type,
            thinking_budget,
            reasoning_level,
            Some(&execution_plan),
            options.cache_ttl,
            true,
        );

        // Did THIS request actually carry at least one 1h marker? Arms the
        // silent-downgrade detector. Mirrors cache_control_value(): OneHour
        // puts 1h on every site (message tail, tools, system); Hybrid puts 1h
        // only on stable-prefix sites (tools, system). Degenerate hybrid +
        // API-key + no system prompt + zero tools → no 1h marker anywhere →
        // detector must stay disarmed (zero 1h bucket proves nothing).
        let has_tool_marker = body.has_tool_marker();
        let has_system_marker = body.has_system_marker();
        let request_has_1h_marker = match options.cache_ttl {
            crate::core::config::CacheTtl::FiveMinutes => false,
            crate::core::config::CacheTtl::OneHour => {
                !cleaned_messages.is_empty() || has_tool_marker || has_system_marker
            }
            crate::core::config::CacheTtl::Hybrid => has_tool_marker || has_system_marker,
        };

        tracing::trace!(
            "Outgoing API Request Payload:\n{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );

        // Serialize the body once up-front; each retry attempt reuses the same
        // bytes via a cheap `Bytes::clone()` (refcount bump, no copy). Previously
        // we called `req.json(&body).send()` per attempt, which re-ran
        // `serde_json::to_vec(&body)` over the entire conversation on every
        // 429 retry — wasted work since the payload is identical.
        let body_bytes: bytes::Bytes = serde_json::to_vec(&body)
            .map_err(|e| {
                RuntimeError::ApiStatus(format!("failed to serialize request body: {}", e))
            })?
            .into();

        // ═══ UNIFIED RETRY (task #130) ════════════════════════════════════════
        // ONE budget governs every transient failure — whether it surfaces as an
        // HTTP status (429 / 5xx, pre-stream), a transport drop, OR an in-stream
        // `error` event (overloaded_error / api_error / rate_limit_error,
        // mid-stream under a 200). They are the same class of problem: "re-send
        // the identical request after backoff." Sharing these counters across the
        // send phase AND the stream phase is what stops the budget from
        // multiplying (the two-loop smell) — a request that flaps between an HTTP
        // 5xx and an in-stream error draws down a SINGLE `max_retries` budget.
        //
        // 429 (rate-limit) keeps its own higher budget: OAuth windows can last
        // minutes, so we honour the reset headers rather than dying after a few
        // tries. Other transients (HTTP 5xx + in-stream errors) share `max_retries`.
        const MAX_429_RETRIES: u32 = 8;
        let mut last_err = String::new();
        let mut last_status: Option<u16> = None;
        let mut last_reset_hint: Option<String> = None;
        let mut non_429_attempts: u32 = 0; // shared server-error budget
        let mut refusal_attempts: u32 = 0; // refusal-retry budget (separate from error budget)
        let mut attempt: u32 = 0; // total attempts (for backoff calc)

        loop {
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
                    let anthropic_base = options
                        .anthropic_base_url
                        .clone()
                        .or_else(|| std::env::var("SYNAPS_ANTHROPIC_BASE_URL").ok())
                        .unwrap_or_else(|| "https://api.anthropic.com".into());
                    let anthropic_url =
                        format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));
                    let mut req = client
                        .post(&anthropic_url)
                        .header(auth_header_name.clone(), auth_header_value.clone())
                        .header("anthropic-version", "2023-06-01")
                        .header("content-type", "application/json");
                    // Build the anthropic-beta header. The 1M-context opt-in
                    // (`context-1m-2025-08-07`) is only added when the user
                    // explicitly requested 1M AND the model supports it. Without
                    // this opt-in, all models default to 200k mode — which is the
                    // documented "smarter" inference regime (see
                    // anthropic.com/engineering/effective-context-engineering).
                    if let Some(beta) = Self::build_beta_header(&auth_type, options, model) {
                        req = req.header("anthropic-beta", beta);
                    }

                    match req.body(body_bytes.clone()).send().await {
                        Ok(resp) => {
                            let status = resp.status();
                            if status.is_success() {
                                response = Some(resp);
                                break;
                            }

                            // C1: a 401 with a Remote source means the broker token
                            // was rejected (revoked, or rotated at the broker before
                            // our cache thought it expired). Invalidate, refetch from
                            // the broker, and retry ONCE with the fresh token.
                            if status.as_u16() == 401
                                && options.credential_source.is_remote()
                                && !auth_retried
                            {
                                auth_retried = true;
                                let _ = resp.text().await; // drain body
                                options.token_cache.invalidate("anthropic");
                                {
                                    let mut g = auth.write().await;
                                    g.auth_token.clear();
                                    g.token_expires = None;
                                }
                                super::auth::AuthMethods::refresh_if_needed(
                                    std::sync::Arc::clone(auth),
                                    client,
                                    &options.credential_source,
                                    &options.token_cache,
                                )
                                .await?;
                                let (n, v, _t) = Self::build_auth_header(auth).await;
                                auth_header_name = n;
                                auth_header_value = v;
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

                            let error_text = resp.text().await.unwrap_or_default();

                            // Decide whether we've exhausted retries for this error class.
                            let retry_exhausted = if is_429 {
                                attempt >= MAX_429_RETRIES
                            } else {
                                non_429_attempts >= max_retries
                            };

                            if !is_retryable || retry_exhausted {
                                let hint = reset_hint.as_deref().or(last_reset_hint.as_deref());
                                return Err(RuntimeError::ApiStatus(
                                    crate::core::error::humanize_api_error_with_reset(
                                        status.as_u16(),
                                        &error_text,
                                        hint,
                                    ),
                                ));
                            }

                            last_status = Some(status.as_u16());
                            last_reset_hint = reset_hint.clone();
                            last_err = format!("{}: {}", status, error_text);

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
                            let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(notice)));

                            tokio::time::sleep(delay).await;

                            if cancel.is_cancelled() {
                                return Err(RuntimeError::Canceled);
                            }
                        }
                        Err(e) => {
                            non_429_attempts += 1;
                            if non_429_attempts > max_retries {
                                return Err(RuntimeError::ApiStatus(
                                    crate::core::error::humanize_network_error(&e),
                                ));
                            }
                            last_err = e.to_string();
                            last_status = None;
                            // No headers on network error — plain exponential back-off.
                            let delay = Duration::from_millis(
                                1000 * 2u64.pow(non_429_attempts.saturating_sub(1)),
                            );
                            tracing::warn!(
                                "API retry {}/{} after {:?}: {}",
                                non_429_attempts,
                                max_retries,
                                delay,
                                last_err
                            );
                            let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(format!(
                                "⏳ API error, retrying ({}/{})…",
                                non_429_attempts, max_retries
                            ))));
                            tokio::time::sleep(delay).await;
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
            tracing::debug!("Stream opened");

            let mut state = ParseState::new();
            let ctx = EventCtx {
                tx: &tx,
                telemetry_level,
                request_start,
                cache_ttl: options.cache_ttl,
                ttl_downgrade_notified: options.ttl_downgrade_notified.clone(),
                saw_1h_honored: options.saw_1h_honored.clone(),
                request_has_1h_marker,
            };

            // SSE can split across chunk boundaries (even mid-UTF-8-codepoint), so
            // buffer raw bytes and only parse complete lines. Zero-copy: lines are
            // borrowed from the buffer, parsed in place (REVIEW.md P2).
            let mut line_buffer = super::sse::SseLineBuffer::new();

            while let Some(chunk) = stream.next().await {
                if cancel.is_cancelled() {
                    break;
                }
                // A transport error mid-stream means connection loss. It's the same
                // transient class as an HTTP 5xx or an in-stream overloaded_error, so
                // route it through the unified retry budget instead of hard-failing
                // the turn. Bill any start-captured usage first: the API already
                // processed the input even if the stream died on us.
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        emit_residual_usage(&mut state, &ctx);
                        state.stream_error = Some(StreamError {
                            message: crate::core::error::humanize_network_error(&e),
                            retryable: true,
                        });
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

            // Flush any partial block and return accumulated content
            state.finalize();

            // Dead-stream billing: if the stream terminated before message_delta
            // (cancel, transport death mid-stream), emit the one Usage event from
            // the message_start capture. No-op when the delta already emitted.
            emit_residual_usage(&mut state, &ctx);

            // Capture the failure signals BEFORE the telemetry block — it moves
            // `state.telem_stop_reason` into the record when telemetry is enabled.
            let stream_error = state.stream_error.take();
            let has_stop_reason = state.stop_reason_seen;
            let stop_reason_is_refusal = state.stop_reason_is_refusal;
            let cancelled = cancel.is_cancelled();

            // ═══ TELEMETRY: write the record ═══
            if telemetry_level.enabled() {
                // Build context record — what we sent
                let breakpoints: Vec<usize> = cleaned_messages
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| {
                        if let Some(arr) = m["content"].as_array() {
                            arr.last().and_then(|b| b.get("cache_control")).is_some()
                        } else {
                            false
                        }
                    })
                    .map(|(i, _)| i)
                    .collect();

                let system_bytes = system_prompt.as_ref().map(|s| s.len()).unwrap_or(0);

                let record = telemetry::TelemetryRecord {
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
                    ttft_ms: state.telem_ttft,
                    total_ms: request_start.elapsed().as_millis() as u64,
                    stop_reason: state.telem_stop_reason,
                    usage: state.telem_usage,
                    ratelimit: telem_ratelimit,
                    cache_diag: None, // TODO: wire cache-diagnostics beta in future slice
                    context: telemetry::ContextRecord {
                        messages: cleaned_messages.len(),
                        tools: tools_schema.len(),
                        system_bytes,
                        breakpoints,
                    },
                };
                telemetry::write_record(&record);
            }

            match classify_stream_outcome(
                stream_error,
                std::mem::take(&mut state.accumulated_content),
                has_stop_reason,
                stop_reason_is_refusal,
                cancelled,
            ) {
                StreamOutcome::Done(v) => return Ok(v),
                StreamOutcome::Fail(msg) => return Err(RuntimeError::ApiStatus(msg)),
                StreamOutcome::Retry(msg) => {
                    // Transient in-stream error (overloaded_error / api_error /
                    // rate_limit_error). Draws from the SAME shared server-error
                    // budget as HTTP 5xx — one unified retry policy, no budget
                    // multiplication across the send and stream phases.
                    if non_429_attempts >= max_retries || cancel.is_cancelled() {
                        // Budget exhausted (or user cancelled) — surface terminally
                        // rather than silently. Still loud, never the silent stop.
                        return Err(RuntimeError::ApiStatus(msg));
                    }
                    let delay = Duration::from_millis(1000 * 2u64.pow(non_429_attempts.min(6)));
                    non_429_attempts += 1;
                    attempt += 1;
                    last_err = msg.clone();
                    last_status = None;
                    tracing::warn!(
                        "in-stream API error, retrying {}/{} after {:?}: {}",
                        non_429_attempts,
                        max_retries,
                        delay,
                        msg
                    );
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(format!(
                        "⏳ API stream error — retrying ({}/{})…",
                        non_429_attempts, max_retries
                    ))));
                    tokio::time::sleep(delay).await;
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
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(format!(
                        "⏳ model refusal — retrying ({}/{})…",
                        refusal_attempts, refusal_retries
                    ))));
                    tokio::time::sleep(delay).await;
                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Canceled);
                    }
                    // fall through to outer `loop` head — fresh request, fresh ParseState
                }
            }
        } // end UNIFIED RETRY LOOP (task #130)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::telemetry::TelemetryLevel;

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
            tx,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::FiveMinutes,
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            request_has_1h_marker: true,
        }
    }

    /// Telemetry-OFF ctx — the DEFAULT runtime config. Used to prove parse-time
    /// behavior doesn't silently depend on telemetry being enabled.
    fn make_ctx_telemetry_off(tx: &mpsc::UnboundedSender<StreamEvent>) -> EventCtx<'_> {
        EventCtx {
            tx,
            telemetry_level: TelemetryLevel::Off,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::FiveMinutes,
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            request_has_1h_marker: true,
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
            tx,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: ttl,
            ttl_downgrade_notified: latch.clone(),
            saw_1h_honored: honored.clone(),
            // Default armed: most detector tests model a request that DID
            // carry a 1h marker. The no-marker test overrides this.
            request_has_1h_marker: true,
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

    #[test]
    fn empty_thinking_block_never_emitted() {
        let (mut state, tx, _rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                // Only a signature delta — no thinking text. Anthropic rejects
                // empty thinking blocks; the guard must suppress the push.
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_only"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
            &mut state,
            &ctx,
        );
        assert!(state.accumulated_content.is_empty());
        assert!(!state.in_thinking);
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
            ],
            &mut state,
            &ctx,
        );
        // Telemetry carries the split captured at message_start.
        assert_eq!(state.telem_usage.cache_write, 1282);
        assert_eq!(state.telem_usage.cache_write_5m, Some(5));
        assert_eq!(state.telem_usage.cache_write_1h, Some(1277));
        // The delta-arm Usage emission (final counts) carries the split too.
        let events = drain(&mut rx);
        let last_usage = events
            .iter()
            .rfind(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .expect("delta must emit a Usage event");
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

    #[test]
    fn one_usage_emission_per_request_live_shapes() {
        // THE invariant: exactly ONE SessionEvent::Usage per request. Every
        // downstream consumer accumulates — two emissions = ~2x billing on
        // input/cache_read/cache_creation. This bug PREDATES cache-ttl
        // (verified at ffa83a8).
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(&[PROBED_LIVE_START, PROBED_LIVE_DELTA], &mut state, &ctx);
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
        // Residual path must be a no-op after the delta emitted.
        emit_residual_usage(&mut state, &ctx);
        assert!(
            drain(&mut rx).is_empty(),
            "residual emission must be a no-op when the delta already emitted"
        );
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
        emit_residual_usage(&mut state, &ctx);
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
        emit_residual_usage(&mut state, &ctx);
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
        assert!(state.telem_ttft.is_none());
        feed(
            &[r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#],
            &mut state,
            &ctx,
        );
        let first = state.telem_ttft;
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
            state.telem_ttft, first,
            "TTFT must not be overwritten by later events"
        );
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
        // silently. The captured message must name the error type so the
        // surfaced error is actionable.
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
            err.message.contains("Overloaded"),
            "must carry the error message: {}",
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
            tx: &tx,
            telemetry_level: TelemetryLevel::Full,
            request_start: std::time::Instant::now(),
            cache_ttl: crate::core::config::CacheTtl::Hybrid,
            ttl_downgrade_notified: notified.clone(),
            saw_1h_honored: honored.clone(),
            request_has_1h_marker: false,
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
    async fn on401_remote_retries_once_and_succeeds() {
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
    async fn on401_remote_persistent_401_is_terminal_not_loop() {
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
    async fn on401_local_source_does_not_retry() {
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
}
