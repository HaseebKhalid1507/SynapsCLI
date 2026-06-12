use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use serde_json::{json, Value};
use reqwest::Client;
use futures::StreamExt;
use crate::{Result, RuntimeError, ToolRegistry};
use crate::runtime::telemetry::{self, TelemetryLevel};
use super::sse_types::{AnthropicEvent, ContentBlock, Delta};
use super::types::{AuthState, StreamEvent, LlmEvent, SessionEvent};
use super::helpers::HelperMethods;

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
    // ── Cache-TTL split captured from message_start ──
    // Live API shape (probed 2024+): message_start carries the full
    // `cache_creation` sub-object; message_delta carries ONLY the aggregate.
    // The delta arm falls back to these when its own sub-object is absent.
    msg_start_cache_5m: Option<u64>,
    msg_start_cache_1h: Option<u64>,
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
            msg_start_cache_5m: None,
            msg_start_cache_1h: None,
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
                let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::Text(text.into_owned())));
            }
            Delta::ThinkingDelta { thinking } => {
                // Anthropic sends thinking text in delta.thinking
                state.current_thinking.push_str(&thinking);
                let _ = ctx.tx.send(StreamEvent::Llm(LlmEvent::Thinking(thinking.into_owned())));
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
            // ═══ TELEMETRY: capture stop_reason from delta ═══
            if ctx.telemetry_level.enabled() {
                if let Some(sr) = delta.and_then(|d| d.stop_reason) {
                    state.telem_stop_reason = Some(sr.into_owned());
                }
            }
            if let Some(usage) = usage {
                let input_t = usage.input_tokens;
                let output_t = usage.output_tokens;
                let cache_read = usage.cache_read_input_tokens;
                let cache_create = usage.cache_creation_input_tokens;
                // TTL breakdown: prefer the delta's own cache_creation
                // sub-object (future-proof), but in live traffic message_delta
                // carries ONLY the aggregate — the split arrives on
                // message_start. Fall back to the values captured there.
                let cache_create_5m = usage.cache_creation.as_ref()
                    .and_then(|cc| cc.ephemeral_5m_input_tokens)
                    .or(state.msg_start_cache_5m);
                let cache_create_1h = usage.cache_creation.as_ref()
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
                // latch; a genuinely downgraded account never does.
                if cache_create_1h.unwrap_or(0) > 0 {
                    ctx.saw_1h_honored.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if ctx.cache_ttl != crate::core::config::CacheTtl::FiveMinutes
                    && cache_create_1h.unwrap_or(0) == 0
                    && cache_create_5m.unwrap_or(0) > 0
                    && !ctx.saw_1h_honored.load(std::sync::atomic::Ordering::Relaxed)
                    && !ctx.ttl_downgrade_notified.swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Notice(
                        "⚠ 1h cache TTL not honored — check account/beta support (cache_ttl config)".to_string(),
                    )));
                }

                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
                    tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
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
                let input_t = usage.input_tokens;
                let output_t = usage.output_tokens;
                let cache_read = usage.cache_read_input_tokens;
                let cache_create = usage.cache_creation_input_tokens;
                let cache_create_5m = usage.cache_creation.as_ref().and_then(|cc| cc.ephemeral_5m_input_tokens);
                let cache_create_1h = usage.cache_creation.as_ref().and_then(|cc| cc.ephemeral_1h_input_tokens);
                // Capture the split for the message_delta arm: live deltas
                // carry only the aggregate, so this is the only place the
                // 5m/1h breakdown exists in streaming traffic.
                state.msg_start_cache_5m = cache_create_5m;
                state.msg_start_cache_1h = cache_create_1h;
                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
                    tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
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
        AnthropicEvent::MessageStop => {}
        AnthropicEvent::Unknown => {
            // #[serde(other)] discarded the tag — the raw line is the only
            // forensics. Covers `ping`, `error`, and future event types.
            // TODO(follow-up): promote Anthropic `error` events to a real arm
            // surfacing SessionEvent::Notice.
            tracing::trace!(
                "Unknown SSE event type: {}",
                truncate_at_char_boundary(raw, 200)
            );
        }
    }
}

/// Options that modify API request behavior beyond the core parameters.
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
        messages: &[Value],
        tx: mpsc::UnboundedSender<StreamEvent>,
        max_retries: u32,
        options: &ApiOptions,
        telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    ) -> Result<Value> {
        Self::call_api_stream_inner(auth, client, model, tools, system_prompt, thinking_budget, messages, tx, &CancellationToken::new(), max_retries, options, telemetry_level).await
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
        messages: &[Value],
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: &CancellationToken,
        max_retries: u32,
        options: &ApiOptions,
        telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    ) -> Result<Value> {
        // Route to OpenAI-compat provider if the model id resolves to one.
        let tools_schema = tools.tools_schema();
        if let Some(result) = crate::runtime::openai::try_route(
            model, client, &tools_schema, system_prompt, messages, &tx,
            None, None, thinking_budget, cancel,
        ).await {
            return result.map_err(|e| RuntimeError::Config(format!("openai provider: {e}")));
        }

        // Read auth state for this API call
        let (auth_header_name, auth_header_value, auth_type) = Self::build_auth_header(auth).await;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No Anthropic credentials. Run `synaps login` or set ANTHROPIC_API_KEY, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`.".to_string()
            ));
        }

        tracing::info!(model = %model, "Starting API request");

        // Manual cache breakpoints for optimal prompt caching.
        // Tested vs auto-cache (top-level cache_control) — manual wins: 90% vs 53% hit rate.
        let mut cleaned_messages = messages.to_vec();
        // Strip empty/invalid thinking blocks before they hit the API. See
        // `sanitize_thinking_blocks` for the failure mode this guards against.
        HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, options.cache_ttl);

        // Derive the thinking level from the budget for effort mapping.
        let thinking_level = crate::core::models::thinking_level_for_budget(thinking_budget);

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
            "messages": cleaned_messages,
            "tools": &*tools_schema,
            "stream": true,
            "thinking": if crate::core::models::model_supports_adaptive_thinking(model) {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                // Legacy path requires budget_tokens >= 1024 (Anthropic enforced).
                // If user picked "adaptive" (sentinel 0) on a legacy model, fall back
                // to "high" (16384) — the model's effective thinking depth without
                // the deprecated-but-functional adaptive shape it doesn't support.
                let budget = if thinking_budget == 0 { crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK } else { thinking_budget };
                json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                    "display": "summarized"
                })
            }
        });

        // For adaptive models, control thinking depth via effort (GA, no beta).
        // "adaptive" level = omit effort entirely (model decides).
        if crate::core::models::model_supports_adaptive_thinking(model) {
            if let Some(effort) = crate::core::models::effort_for_thinking_level(thinking_level) {
                body["output_config"] = json!({"effort": effort});
            }
        }

        // Prompt caching: mark the last tool so all tool schemas are cached
        HelperMethods::mark_last_tool(&mut body, options.cache_ttl);

        if let Some(system) = HelperMethods::build_system_blocks(&auth_type, system_prompt, options.cache_ttl) {
            body["system"] = system;
        }

        tracing::trace!("Outgoing API Request Payload:\n{}", serde_json::to_string_pretty(&body).unwrap_or_default());

        // Retry loop for transient API errors (429, 529, 500, 502, 503)
        let response = {
            let mut last_err = String::new();
            let mut response = None;

            for attempt in 0..=max_retries {
                if attempt > 0 {
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt - 1)); // 1s, 2s, 4s
                    tracing::warn!("API retry {}/{} after {:?}: {}", attempt, max_retries, delay, last_err);
                    // Display-only notice — never lands in message history.
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(format!("⏳ API error, retrying ({}/{})…", attempt, max_retries))));
                    tokio::time::sleep(delay).await;

                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Canceled);
                    }
                }

                // Rebuild request (consumed on send)
                let mut req = client
                    .post("https://api.anthropic.com/v1/messages")
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

                match req.json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            response = Some(resp);
                            break;
                        }
                        let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                        let error_text = resp.text().await.unwrap_or_default();
                        if !is_retryable || attempt == max_retries {
                            return Err(RuntimeError::ApiStatus(crate::core::error::humanize_api_error(status.as_u16(), &error_text)));
                        }
                        last_err = format!("{}: {}", status, error_text);
                    }
                    Err(e) => {
                        if attempt == max_retries {
                            return Err(RuntimeError::ApiStatus(crate::core::error::humanize_network_error(&e)));
                        }
                        last_err = e.to_string();
                    }
                }
            }

            response.ok_or_else(|| RuntimeError::Tool(format!("API failed after {} retries: {}", max_retries, last_err)))?
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
            if rl.is_empty() { None } else { Some(rl) }
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
        };

        // SSE can split across chunk boundaries (even mid-UTF-8-codepoint), so
        // buffer raw bytes and only parse complete lines. Zero-copy: lines are
        // borrowed from the buffer, parsed in place (REVIEW.md P2).
        let mut line_buffer = super::sse::SseLineBuffer::new();

        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                break;
            }
            // A transport error mid-stream means connection loss — translate
            // to an actionable message instead of a raw reqwest debug string.
            let chunk = chunk.map_err(|e| RuntimeError::ApiStatus(crate::core::error::humanize_network_error(&e)))?;
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


        // ═══ TELEMETRY: write the record ═══
        if telemetry_level.enabled() {
            // Build context record — what we sent
            let breakpoints: Vec<usize> = cleaned_messages.iter().enumerate()
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
                attempt: 1, // TODO: thread attempt number from retry loop
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

        Ok(json!({
            "content": state.accumulated_content
        }))
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
        assert_eq!(state.accumulated_content[0], json!({"type":"text","text":"Hello, world"}));
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
        assert_eq!(state.accumulated_content[0], json!({"type":"text","text":"first"}));
        assert_eq!(state.accumulated_content[1], json!({"type":"text","text":"second"}));
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
        let err = input["__parse_error"].as_str().expect("__parse_error key present");
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
            StreamEvent::Session(SessionEvent::Usage { input_tokens: 100, output_tokens: 50, cache_read_input_tokens: 300, cache_creation_input_tokens: 100, cache_creation_5m: Some(60), cache_creation_1h: Some(40), model: None })
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
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .next_back()
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
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_xyz","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(state.telem_msg_id.as_deref(), Some("msg_xyz"));
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            StreamEvent::Session(SessionEvent::Usage { input_tokens: 10, output_tokens: 1, .. })
        ));
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
            !events.iter().any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. }))),
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
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#],
            &mut state,
            &ctx,
        );
        assert_eq!(state.telem_ttft, first, "TTFT must not be overwritten by later events");
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
        process_data_line(r#"data: {"type":"content_block_stop","index":0}"#, &mut state, &ctx);
        // End-of-stream flush.
        state.finalize();

        let tool_blocks: Vec<&Value> = state
            .accumulated_content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .collect();
        assert_eq!(tool_blocks.len(), 1, "tool_use block must be emitted exactly once");
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
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"dangling"}}"#],
            &mut state,
            &ctx,
        );
        state.finalize();
        assert_eq!(state.accumulated_content, vec![json!({"type":"text","text":"dangling"})]);
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
            &[r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#],
            &mut state2,
            &ctx2,
        );
        state2.finalize();
        assert!(state2.accumulated_content.is_empty(), "empty thinking must be suppressed");
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
        assert_eq!(state.accumulated_content, after_first, "second finalize must be a no-op");

        // Same for partial text.
        let (mut state2, tx2, _rx2) = harness();
        let ctx2 = make_ctx(&tx2);
        feed(
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"t"}}"#],
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
        assert_eq!(events.len(), 1, "only the valid data line may produce events");
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
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pre"}}"#],
            &mut state,
            &ctx,
        );
        drain(&mut rx);
        let before = snapshot(&state);
        feed(
            &[
                r#"data: {"type":"ping"}"#,
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                r#"data: {"type":"fnord","payload":[1,2,3]}"#,
            ],
            &mut state,
            &ctx,
        );
        assert_eq!(snapshot(&state), before, "Unknown events must not mutate state");
        assert!(drain(&mut rx).is_empty(), "Unknown events must emit zero events");
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
        assert_eq!(snapshot(&state), before, "malformed lines must be skipped without state change");
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
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"keep"}}"#],
            &mut state,
            &ctx,
        );
        drain(&mut rx);
        let before = snapshot(&state);
        feed(
            &[r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"x":1}}}"#],
            &mut state,
            &ctx,
        );
        assert_eq!(snapshot(&state), before, "unknown delta subtype must not mutate state");
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn tail_partial_line_typed_parse() {
        // Tail path shape: take_remaining() yields an owned String the typed
        // event borrows from — the lifetime path slice 3 had to keep sound.
        let (mut state, tx, mut rx) = harness();
        let ctx = make_ctx(&tx);
        feed(
            &[r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#],
            &mut state,
            &ctx,
        );
        let remaining: String =
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tail ✨"}}"#
                .to_string();
        process_data_line(&remaining, &mut state, &ctx);
        drop(remaining); // event Cow must not outlive this — compile-time proof it didn't
        state.finalize();
        assert_eq!(state.accumulated_content, vec![json!({"type":"text","text":"tail ✨"})]);
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
        let ctx = make_ctx_ttl(&tx, crate::core::config::CacheTtl::Hybrid, &notified, &honored);
        // Turn 1: 1h prefix written (split on message_start) → latch set, no notice.
        feed(&[HONORED_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(count_downgrade_notices(&mut rx), 0, "turn 1 (1h honored)");
        assert!(honored.load(std::sync::atomic::Ordering::Relaxed), "latch set on 1h write");
        // Turn 2: prefix cached → 1h == 0, 5m > 0. Healthy. SILENCE.
        let (mut state_t2, tx_t2, mut rx_t2) = harness();
        let ctx_t2 = make_ctx_ttl(&tx_t2, crate::core::config::CacheTtl::Hybrid, &notified, &honored);
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state_t2, &ctx_t2);
        assert_eq!(count_downgrade_notices(&mut rx_t2), 0, "turn 2 (healthy hybrid signature)");
        // Later request in the same session (new ctx, same latches): still silent.
        let (mut state2, tx2, mut rx2) = harness();
        let ctx2 = make_ctx_ttl(&tx2, crate::core::config::CacheTtl::Hybrid, &notified, &honored);
        feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state2, &ctx2);
        assert_eq!(count_downgrade_notices(&mut rx2), 0, "later request, same session");
    }

    #[test]
    fn downgrade_detector_fires_once_when_1h_never_honored() {
        // Genuinely downgraded account: the 1h bucket never goes nonzero —
        // the notice fires on turn 1, exactly once per session, in both modes.
        for ttl in [crate::core::config::CacheTtl::OneHour, crate::core::config::CacheTtl::Hybrid] {
            let (mut state, tx, mut rx) = harness();
            let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let ctx = make_ctx_ttl(&tx, ttl, &notified, &honored);
            // First occurrence: 1h bucket = 0, 5m bucket > 0 → exactly one Notice.
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state, &ctx);
            assert_eq!(count_downgrade_notices(&mut rx), 1, "first occurrence under {ttl:?}");
            // Second occurrence (same session/latch): nothing.
            let (mut state_b, tx_b, mut rx_b) = harness();
            let ctx_b = make_ctx_ttl(&tx_b, ttl, &notified, &honored);
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state_b, &ctx_b);
            assert_eq!(count_downgrade_notices(&mut rx_b), 0, "second occurrence under {ttl:?}");
            // Latch persists across requests in the session (new ctx, same latches).
            let (mut state2, tx2, mut rx2) = harness();
            let ctx2 = make_ctx_ttl(&tx2, ttl, &notified, &honored);
            feed(&[DOWNGRADE_START, LIVE_DELTA], &mut state2, &ctx2);
            assert_eq!(count_downgrade_notices(&mut rx2), 0, "next request, same session");
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
        let ctx = make_ctx_ttl(&tx, crate::core::config::CacheTtl::OneHour, &latch, &honored);
        feed(&[HONORED_START, LIVE_DELTA], &mut state, &ctx);
        assert_eq!(count_downgrade_notices(&mut rx), 0);
        assert!(!latch.load(std::sync::atomic::Ordering::Relaxed), "latch untouched when honored");
    }

    #[test]
    fn downgrade_detector_silent_when_split_absent() {
        // cache_creation sub-object missing entirely → no basis to judge; stay quiet.
        let (mut state, tx, mut rx) = harness();
        let latch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let honored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = make_ctx_ttl(&tx, crate::core::config::CacheTtl::OneHour, &latch, &honored);
        feed(
            &[r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":100}}"#],
            &mut state,
            &ctx,
        );
        assert_eq!(count_downgrade_notices(&mut rx), 0);
    }
}
