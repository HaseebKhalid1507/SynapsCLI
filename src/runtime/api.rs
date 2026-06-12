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
                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
                    tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
                    // ═══ TELEMETRY: accumulate usage (message_delta carries final counts) ═══
                    if ctx.telemetry_level.enabled() {
                        state.telem_usage.input = input_t;
                        state.telem_usage.output = output_t;
                        state.telem_usage.cache_read = cache_read;
                        state.telem_usage.cache_write = cache_create;
                        // TTL breakdown from cache_creation sub-object
                        if let Some(cc) = usage.cache_creation {
                            state.telem_usage.cache_write_5m = cc.ephemeral_5m_input_tokens;
                            state.telem_usage.cache_write_1h = cc.ephemeral_1h_input_tokens;
                        }
                        state.telem_usage.compute_hit_pct();
                    }
                    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Usage {
                        input_tokens: input_t,
                        output_tokens: output_t,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_create,
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
                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                    HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
                    tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
                    let _ = ctx.tx.send(StreamEvent::Session(SessionEvent::Usage {
                        input_tokens: input_t,
                        output_tokens: output_t,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_create,
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
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages);

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
        if let Some(tool_list) = body["tools"].as_array_mut() {
            if let Some(last_tool) = tool_list.last_mut() {
                last_tool["cache_control"] = json!({"type": "ephemeral"});
            }
        }

        if auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": crate::core::config::get_identity()}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            // Prompt caching: mark the last system block so entire system prompt is cached
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["system"] = json!(system_blocks);
        } else if let Some(ref prompt) = system_prompt {
            body["system"] = json!([
                {"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}
            ]);
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
            StreamEvent::Session(SessionEvent::Usage { input_tokens: 100, output_tokens: 50, cache_read_input_tokens: 300, cache_creation_input_tokens: 100, model: None })
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
}
