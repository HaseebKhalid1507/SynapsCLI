//! Runtime dispatch for `WireProtocol::GoogleGeminiCodeAssist`.
//!
//! Translates the runtime's Anthropic-shaped `SharedMessage`/tool-schema into
//! Gemini `ChatTurn`/`ToolSpec`, invokes the broker-proxied
//! [`super::stream::stream_gemini`], and forwards decoded events onto the
//! runtime event bus while aggregating a final Anthropic-shaped content Value
//! for the outer agent loop.
//!
//! The broker credential boundary is preserved: this module never touches the
//! OAuth access token, refresh token, or auth.json — it hands the request to
//! `CredentialBroker::proxy_stream` and consumes bytes.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::setup::setup_user;
use super::stream::{build_stream_request, stream_gemini_request, StreamError};
use super::translate::{ChatTurn, GeminiStreamEvent, ToolSpec};
use crate::auth::CredentialBroker;
use crate::runtime::openai::types::ProviderConfig;
use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};

/// Translate tool schemas (Anthropic-shaped: `{name, description, input_schema}`)
/// into Gemini `ToolSpec`s. Internal-only tool names are dropped.
fn tools_to_gemini(schema: &[Value]) -> Vec<ToolSpec> {
    schema
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            if name.is_empty()
                || crate::runtime::trace::google::GEMINI_INTERNAL_TOOLS.contains(&name.as_str())
            {
                return None;
            }
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let parameters_json_schema = t.get("input_schema").cloned();
            Some(ToolSpec {
                name,
                description,
                parameters_json_schema,
            })
        })
        .collect()
}

/// Translate Anthropic-shaped `SharedMessage`s into a flat sequence of Gemini
/// `ChatTurn`s. Text and tool-use/tool-result blocks are preserved; `thinking`
/// blocks and other unrepresentable content are dropped.
fn messages_to_gemini_turns(messages: &[crate::SharedMessage]) -> Vec<ChatTurn> {
    // Build tool_use_id → tool_name map from assistant turns so tool_result
    // blocks can be attached with the correct function name.
    let mut id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let (Some(id), Some(name)) = (
                            block.get("id").and_then(|v| v.as_str()),
                            block.get("name").and_then(|v| v.as_str()),
                        ) {
                            id_to_name.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut turns: Vec<ChatTurn> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        match role {
            "user" => match content {
                Some(Value::String(s)) if !s.is_empty() => {
                    turns.push(ChatTurn::User { text: s.clone() });
                }
                Some(Value::Array(blocks)) => {
                    let mut text_buf = String::new();
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_result" => {
                                if !text_buf.is_empty() {
                                    turns.push(ChatTurn::User {
                                        text: std::mem::take(&mut text_buf),
                                    });
                                }
                                let tool_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = id_to_name.get(&tool_id).cloned().unwrap_or_default();
                                let mut result = match block.get("content") {
                                    Some(Value::String(s)) => json!({ "output": s }),
                                    Some(Value::Array(arr)) => {
                                        let text = arr
                                            .iter()
                                            .filter_map(|b| {
                                                b.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(String::from)
                                            })
                                            .collect::<Vec<_>>()
                                            .join("");
                                        json!({ "output": text })
                                    }
                                    Some(Value::Object(_)) => block["content"].clone(),
                                    Some(other) => json!({ "output": other }),
                                    None => json!({}),
                                };
                                if let Some(is_error) = block.get("is_error") {
                                    result["is_error"] = is_error.clone();
                                }
                                turns.push(ChatTurn::ToolResult { name, result });
                            }
                            _ => {}
                        }
                    }
                    if !text_buf.is_empty() {
                        turns.push(ChatTurn::User { text: text_buf });
                    }
                }
                _ => {}
            },
            "assistant" => match content {
                Some(Value::String(s)) if !s.is_empty() => {
                    turns.push(ChatTurn::Assistant { text: s.clone() });
                }
                Some(Value::Array(blocks)) => {
                    let mut text_buf = String::new();
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_use" => {
                                if !text_buf.is_empty() {
                                    turns.push(ChatTurn::Assistant {
                                        text: std::mem::take(&mut text_buf),
                                    });
                                }
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
                                let thought_signature = block
                                    .get("thought_signature")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_owned);
                                turns.push(ChatTurn::ToolCall {
                                    name,
                                    args,
                                    thought_signature,
                                });
                            }
                            // `thinking` and other unknown block types are not
                            // representable on the Gemini wire — drop.
                            _ => {}
                        }
                    }
                    if !text_buf.is_empty() {
                        turns.push(ChatTurn::Assistant { text: text_buf });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    turns
}

/// Streamed Gemini turn: forwards text/tool events onto `tx` and returns an
/// Anthropic-shaped `{content, stop_reason, usage}` Value for the outer loop.
///
/// The broker owns the OAuth token and pins the upstream host; this function
/// never touches secrets directly.
///
/// Trace wiring (Task 10B): one `synaps-request-trace/1` record per actual
/// broker stream attempt (`proxy_stream` call). `exact_wire_bytes` must be
/// `true` only on the local-broker path, where the `post_json_exact` buffer
/// is provably the wire body; remote-broker records carry `wire: None` and
/// `TransportKind::CloudProxy`. Project setup (`setup_user`) precedes the
/// generateContent attempt and emits no attempt record.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_google_gemini_stream_inner(
    cfg: &ProviderConfig,
    broker: &Arc<dyn CredentialBroker>,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    trace: &crate::runtime::trace::TraceContext,
    exact_wire_bytes: bool,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    use crate::runtime::trace::google as trg;
    use crate::runtime::trace::openai as tro;

    let turns = messages_to_gemini_turns(messages);
    let tools = tools_to_gemini(tools_schema);

    // Resolve the Code Assist project id through the broker before streaming.
    // Code Assist rejects `streamGenerateContent` without a project on the
    // envelope; the broker owns the OAuth token so `setup_user` never touches
    // secrets directly. We honor GOOGLE_CLOUD_PROJECT / GOOGLE_CLOUD_PROJECT_ID
    // as an override, matching the reference client.
    let env_project = gemini_project_env();
    let user = setup_user(broker.as_ref(), env_project)
        .await
        .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("{e}")))?;
    let project_id = user.project_id;

    tracing::debug!(
        provider = %cfg.provider,
        model = %cfg.model,
        project = %project_id,
        "google-gemini stream request via broker proxy"
    );

    // Serialize the envelope ONCE; every retry attempt sends this same
    // request, so the digested bytes describe each attempt's wire body on
    // the local-broker path.
    let (proxy_request, body_bytes) = build_stream_request(
        cfg.model.clone(),
        Some(project_id.clone()),
        system_prompt.clone(),
        &turns,
        &tools,
    )?;
    let tracer = trg::begin_gemini_tracer(
        trace,
        &cfg.provider,
        &cfg.model,
        if exact_wire_bytes {
            crate::runtime::trace::TransportKind::GeminiGenerateContent
        } else {
            crate::runtime::trace::TransportKind::CloudProxy
        },
        &format!(
            "{}/v1internal:streamGenerateContent",
            cfg.base_url.trim_end_matches('/')
        ),
        exact_wire_bytes.then_some(body_bytes.as_ref()),
        messages,
        system_prompt.as_deref(),
        tools_schema,
    )
    .await;
    let mut attempt = tro::StreamAttempt::new(tracer);

    let mut stream = {
        // ═══ RESET-HONORING 429 RETRY ═══════════════════════════════════════
        // Code Assist enforces small per-model windows and reports the reset
        // delay in the 429 body. Failing fast here made every agentic step die
        // on tiny-quota accounts; retrying immediately would burn the window
        // and extend the reset. So: honor the reported reset (plus a small
        // buffer), bounded by MAX_GEMINI_429_RETRIES / MAX_GEMINI_429_WAIT.
        // Non-429 errors keep failing fast — they are not capacity.
        let mut attempt_no: u32 = 0;
        loop {
            match stream_gemini_request(broker.as_ref(), proxy_request.clone(), cancel.clone())
                .await
            {
                Ok(stream) => break stream,
                Err(e) => {
                    // Full text is used ONLY for 429/reset classification —
                    // the broker flattens the upstream status and rate-limit
                    // hint into it. Anything surfaced must be the redacted
                    // form: the snippet is provider-controlled and may echo
                    // the request (spec §5.1).
                    let text = format!("{e}");
                    let redacted = crate::runtime::openai::net::redact_provider_proxy_error(&text);
                    let status = tro::broker_error_status(&text);
                    let Some(reset_secs) = code_assist_429_reset(&text) else {
                        let code = status
                            .map(|s| format!("http_{s}"))
                            .unwrap_or_else(|| "transport_error".to_string());
                        attempt.finish_failed(&code, status, None);
                        return Err(Box::<dyn std::error::Error + Send + Sync>::from(redacted));
                    };
                    if attempt_no >= MAX_GEMINI_429_RETRIES {
                        attempt.finish_failed("http_429", Some(429), None);
                        return Err(Box::<dyn std::error::Error + Send + Sync>::from(redacted));
                    }
                    attempt_no += 1;
                    let wait = reset_secs
                        .map(|s| Duration::from_secs(s.saturating_add(1)))
                        .unwrap_or(DEFAULT_GEMINI_429_WAIT)
                        .min(MAX_GEMINI_429_WAIT);
                    // One record per actual attempt: this failed try is
                    // recorded now; a cancel during the backoff sleep (no
                    // send in flight) emits no additional record.
                    attempt.attempt_failed(
                        crate::runtime::trace::RetryClass::RateLimited,
                        wait,
                        Some(429),
                        None,
                        "http_429",
                    );
                    let notice = format!(
                        "⚠ Gemini rate limited — resuming in {}s ({}/{})",
                        wait.as_secs(),
                        attempt_no,
                        MAX_GEMINI_429_RETRIES
                    );
                    tracing::warn!(
                        provider = %cfg.provider,
                        model = %cfg.model,
                        wait_secs = wait.as_secs(),
                        attempt = attempt_no,
                        "google-gemini 429 rate limited; honoring reported reset"
                    );
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(notice)));
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel.cancelled() => {
                            return Err("operation canceled".into());
                        }
                    }
                    attempt.restart_clock();
                }
            }
        }
    };

    let mut assembled_text = String::new();
    let mut content_blocks: Vec<Value> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut trace_stop: Option<crate::runtime::trace::StopReason> = None;
    let mut trace_usage: Option<crate::runtime::trace::UsageMeta> = None;
    let mut tool_seq: u64 = 0;

    while let Some(event) = stream.next().await {
        attempt.mark_first_byte();
        match event {
            Ok(GeminiStreamEvent::TextDelta(delta)) => {
                attempt.mark_first_model_event();
                assembled_text.push_str(&delta);
                let _ = tx.send(StreamEvent::Llm(LlmEvent::Text(delta)));
            }
            Ok(GeminiStreamEvent::ToolCall(call)) => {
                attempt.mark_first_model_event();
                // Flush any buffered text as a `text` block before the tool_use.
                if !assembled_text.is_empty() {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": std::mem::take(&mut assembled_text),
                    }));
                }
                tool_seq += 1;
                // Gemini function calls have no vendor tool-call id, so we
                // synthesize a stable per-turn id for the downstream loop.
                let tool_id = format!("gemini_call_{tool_seq}");
                let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolUseStart {
                    tool_name: call.name.clone(),
                    tool_id: tool_id.clone(),
                }));
                let input = call.args.clone();
                let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name: call.name.clone(),
                    tool_id: tool_id.clone(),
                    input: input.clone(),
                }));
                let mut tool_block = json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": call.name,
                    "input": input,
                });
                if let Some(signature) = call.thought_signature {
                    tool_block["thought_signature"] = Value::String(signature);
                }
                content_blocks.push(tool_block);
            }
            Ok(GeminiStreamEvent::Finish { reason }) => {
                if let Some(r) = reason {
                    trace_stop = Some(trg::stop_reason_from_gemini(&r));
                    stop_reason = Some(map_finish_reason(&r));
                }
            }
            Ok(GeminiStreamEvent::Usage(usage)) => {
                // Provider-reported accounting; last observation wins.
                trace_usage = Some(trg::usage_from_gemini(&usage));
            }
            Ok(GeminiStreamEvent::Ignored) => {}
            Err(StreamError::Cancelled) => {
                attempt.finish_canceled(None, trace_usage);
                return Err("operation canceled".into());
            }
            Err(e) => {
                // Mid-stream broker errors are transport-level, but redact
                // defensively: any proxy-flattened upstream body snippet is
                // provider-controlled (spec §5.1).
                let text = e.to_string();
                attempt.finish_failed("stream_error", tro::broker_error_status(&text), None);
                return Err(format!(
                    "google-gemini: {}",
                    crate::runtime::openai::net::redact_provider_proxy_error(&text)
                )
                .into());
            }
        }
    }

    if !assembled_text.is_empty() {
        content_blocks.push(json!({
            "type": "text",
            "text": std::mem::take(&mut assembled_text),
        }));
    }

    attempt.finish_success(None, None, trace_stop, trace_usage);

    Ok(json!({
        "content": content_blocks,
        "stop_reason": stop_reason.unwrap_or_else(|| "end_turn".to_string()),
        "usage": {},
    }))
}

/// Read `GOOGLE_CLOUD_PROJECT` (or the `_ID` alias) if set, matching the
/// reference client. Empty values are treated as unset. The runtime forwards
/// this to `setup_user`, which keeps the setup module env-free.
fn gemini_project_env() -> Option<String> {
    for key in ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"] {
        if let Ok(v) = std::env::var(key) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Map Gemini's `finishReason` values onto Anthropic-style stop reasons the
/// outer agent loop already knows how to interpret.
fn map_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "end_turn".to_string(),
        "MAX_TOKENS" => "max_tokens".to_string(),
        // Tool-call-driven stop maps to Anthropic's `tool_use`.
        "TOOL_CALL" | "FUNCTION_CALL" => "tool_use".to_string(),
        other => other.to_string(),
    }
}

/// Additional attempts allowed after a Code Assist 429 rate-limit rejection.
/// OAuth Code Assist quotas reset on short windows (observed 14–53s), so a
/// bounded reset-honoring retry converts tiny-quota accounts from hard
/// failure into slow-but-working sessions.
const MAX_GEMINI_429_RETRIES: u32 = 4;
/// Cap on any single reset wait so a hostile/garbled hint cannot stall us.
const MAX_GEMINI_429_WAIT: Duration = Duration::from_secs(120);
/// Wait used when a 429 carries no parsable reset hint.
const DEFAULT_GEMINI_429_WAIT: Duration = Duration::from_secs(20);

/// Detect a Code Assist rate-limit rejection in a broker transport error.
///
/// Returns `None` when the error is not a 429 rate limit; `Some(reset_secs)`
/// when it is, with the upstream-reported reset delay parsed from the
/// `"Your quota will reset after Ns"` message text when present. The broker
/// deliberately flattens upstream status to text (`provider request failed:
/// 429 Too Many Requests: …`), so this is the transport contract we parse —
/// a status marker alone is not enough, a rate-limit marker must accompany it.
fn code_assist_429_reset(err: &str) -> Option<Option<u64>> {
    let is_429 = err.contains("429");
    let has_rate_limit_marker = err.contains("RESOURCE_EXHAUSTED")
        || err.contains("RATE_LIMIT_EXCEEDED")
        || err.contains("Too Many Requests");
    if !is_429 || !has_rate_limit_marker {
        return None;
    }
    let secs = err.split("reset after").nth(1).and_then(|rest| {
        let rest = rest.trim_start();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if rest[digits.len()..].starts_with('s') {
            digits.parse::<u64>().ok()
        } else {
            None
        }
    });
    Some(secs)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
