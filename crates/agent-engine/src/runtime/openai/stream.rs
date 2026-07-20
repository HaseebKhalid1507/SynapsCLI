//! Streaming path for OpenAI-compatible providers.
//!
//! Mirrors `ApiMethods::call_api_stream_inner` but speaks OpenAI chat/completions
//! and translates back to Anthropic-shaped events for the rest of the runtime.

use super::translate;
use super::types::{ChatMessage, OaiEvent, ProviderConfig, StreamOptions, ToolCall};
use super::wire::StreamDecoder;
use crate::runtime::trace::openai as tr;
use crate::runtime::types::StreamEvent;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Trace capture for one batch of decoded Chat Completions events: marks the
/// first parsed model event and records provider-reported usage. Metadata
/// only — token counts, no content.
fn capture_oai_trace_signals(
    events: &[OaiEvent],
    attempt: &mut tr::StreamAttempt,
    usage: &mut Option<crate::runtime::trace::UsageMeta>,
) {
    for ev in events {
        if !matches!(ev, OaiEvent::Warning(_)) {
            attempt.mark_first_model_event();
        }
        if let OaiEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        } = ev
        {
            *usage = Some(tr::provider_usage(
                u64::from(*prompt_tokens),
                u64::from(*completion_tokens),
                u64::from(*cached_tokens),
            ));
        }
    }
}

/// Persistent retry posture for the Codex transport, mirroring the Anthropic
/// OAuth overload budget (`OAUTH_OVERLOAD_RETRIES` in `runtime/api.rs`).
///
/// chatgpt.com edge turbulence — 503 upstream connect errors, Cloudflare
/// 520s, raw TCP connect/read timeouts (incident: 2026-07-16 log bursts at
/// 14:19, 18:52, 20:21, 20:27, 20:55, 21:47) — clears on its own within a
/// burst; a three-attempt budget risks aborting an autonomous turn mid-burst.
pub(crate) const CODEX_PERSISTENT_RETRIES: u32 = 10;

/// Exponential-backoff exponent cap shared with the Anthropic stream retry
/// path (`stream_retry_policy`): delay = base·2^min(n−1, CAP). Without the
/// cap a 10-deep budget would sleep 1s·2⁹ = 512s on its final attempt.
const RETRY_BACKOFF_EXP_CAP: u32 = 6;

/// Effective Codex transport retry budget: the persistent floor, unless the
/// user configured something even larger. Pure — decided at the dispatch
/// seam (`try_route`), never inside `send_with_retries`, so tests can still
/// inject exact budgets.
pub(crate) fn codex_retry_budget(configured_retries: u32) -> u32 {
    configured_retries.max(CODEX_PERSISTENT_RETRIES)
}

/// Backoff for retry attempt *n* (1-based): 1s·2^(n−1), capped at 64s.
fn retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(
        1000 * 2u64.pow(attempt.saturating_sub(1).min(RETRY_BACKOFF_EXP_CAP)),
    )
}

/// Send a provider streaming request, retrying transient failures.
///
/// Parity fix: the Anthropic path retries transient errors with backoff
/// (`call_api_stream_inner`) but the provider routes were single-shot — one
/// transport blip against e.g. `chatgpt.com` aborted an entire autonomous
/// turn (incident: session 20260714-025948-3dab). Attempt *n* sleeps
/// 1s·2^(n−1) capped at 2^`RETRY_BACKOFF_EXP_CAP`, mirroring the Anthropic
/// budget semantics.
///
/// Retryable: transport-level send failures (timeout / connect / request)
/// and 408 / 429 / 5xx responses. Deterministic client errors (other 4xx)
/// and localhost connection refusals (Ollama/LM Studio not running — a
/// setup problem, not a blip) fail fast. Backoff sleeps are cancel-aware.
///
/// Trace (Task 10A): one record per actual HTTP send — every retried
/// failure is recorded via `attempt_failed` (status + retry class, never
/// provider text) and the per-attempt clock restarts right before each
/// re-send. Terminal failures emit their final record here; on success the
/// caller finishes the attempt after consuming the stream.
async fn send_with_retries(
    label: &str,
    url: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
    max_retries: u32,
    trace_attempt: &mut tr::StreamAttempt,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    let mut attempt: u32 = 0;
    loop {
        match build().send().await {
            Ok(resp) if resp.status().is_success() => {
                trace_attempt.mark_headers();
                return Ok(resp);
            }
            Ok(resp) => {
                // A complete HTTP response was observed even when its status
                // is non-success. Preserve that timing for retried and
                // terminal failure records just as the success branch does.
                trace_attempt.mark_headers();
                let status = resp.status();
                // Provider-assigned request id from validated headers only.
                let trace_rid = tr::provider_request_id_from_headers(resp.headers());
                let retryable =
                    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error();
                // Privacy (spec §5.1): the response body is provider-controlled
                // and may echo the full request (prompts, system text, tool
                // schemas, credentials). Drop it unread — it must never be
                // stored, surfaced, or logged at any level. `status` Display
                // uses the canonical reason phrase, never server bytes.
                drop(resp);
                let code = format!("http_{}", status.as_u16());
                if !retryable || attempt >= max_retries {
                    trace_attempt.finish_failed(&code, Some(status.as_u16()), trace_rid);
                    return Err(format!("{label} request failed: {status}").into());
                }
                attempt += 1;
                let delay = retry_delay(attempt);
                trace_attempt.attempt_failed(
                    tr::retry_class_for_status(status.as_u16()),
                    delay,
                    Some(status.as_u16()),
                    trace_rid,
                    &code,
                );
                tracing::warn!(
                    "{label} API retry {attempt}/{max_retries} after {delay:?}: {status}"
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => {
                        trace_attempt.finish_canceled(None, None);
                        return Err("request canceled".into());
                    }
                }
                // Retry clocks reset per attempt, right before the re-send.
                trace_attempt.restart_clock();
            }
            Err(e) => {
                let localhost_refusal = e.is_connect() && url.contains("localhost");
                let transient = e.is_timeout() || e.is_connect() || e.is_request();
                if localhost_refusal || !transient || attempt >= max_retries {
                    tracing::warn!(
                        "{label} request failed (no retry): {}",
                        crate::core::error::error_chain_string(&e)
                    );
                    trace_attempt.finish_failed("transport_error", None, None);
                    return Err(e.into());
                }
                attempt += 1;
                let delay = retry_delay(attempt);
                trace_attempt.attempt_failed(
                    if e.is_timeout() {
                        crate::runtime::trace::RetryClass::Timeout
                    } else {
                        crate::runtime::trace::RetryClass::Other
                    },
                    delay,
                    None,
                    None,
                    "transport_error",
                );
                tracing::warn!(
                    "{label} transport retry {attempt}/{max_retries} after {delay:?}: {}",
                    crate::core::error::error_chain_string(&e)
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => {
                        trace_attempt.finish_canceled(None, None);
                        return Err("request canceled".into());
                    }
                }
                // Retry clocks reset per attempt, right before the re-send.
                trace_attempt.restart_clock();
            }
        }
    }
}

/// Run a single streaming request against an OpenAI-compatible endpoint.
///
/// Returns the final assistant response as an Anthropic-shaped content Value
/// (`{"content": [..text.., ..tool_use..]}`) so the outer agent loop can keep
/// using the same handling as the native Anthropic path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_oai_stream_inner(
    cfg: &ProviderConfig,
    broker: &std::sync::Arc<dyn crate::auth::CredentialBroker>,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    cancel: &tokio_util::sync::CancellationToken,
    trace: &crate::runtime::trace::TraceContext,
    exact_wire_bytes: bool,
    suppress_stream_deltas: bool,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (oai_tools, name_map) = translate::tools_to_oai(tools_schema);
    let oai_messages = translate::messages_to_oai(messages, system_prompt, &name_map);
    let tools_opt = if oai_tools.is_empty() {
        None
    } else {
        Some(oai_tools)
    };

    // Google's OpenAI-compat endpoint rejects stream_options
    let stream_options = if cfg.base_url.contains("googleapis.com") {
        None
    } else {
        Some(StreamOptions {
            include_usage: true,
        })
    };

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(cfg.model.clone()));
    body.insert("messages".to_string(), serde_json::to_value(oai_messages)?);
    body.insert("stream".to_string(), json!(true));
    if let Some(stream_options) = stream_options {
        body.insert(
            "stream_options".to_string(),
            serde_json::to_value(stream_options)?,
        );
    }
    if let Some(max_tokens) = max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(tools) = tools_opt {
        body.insert("tools".to_string(), serde_json::to_value(tools)?);
    }
    super::reasoning::apply_openai_reasoning_params(
        &mut body,
        super::reasoning::provider_for_key(&cfg.provider),
        &cfg.model,
        thinking_budget,
    );
    let body = Value::Object(body);

    tracing::debug!(provider=%cfg.provider, model=%cfg.model, "openai stream request via broker proxy");

    // Serialize ONCE via the sanctioned constructor: the returned bytes are
    // both digested for the trace and the very buffer stored on the request
    // (`body_bytes`), which `LocalBroker` sends verbatim — the digest is
    // never computed over a re-serialization, and `body`/`body_bytes`
    // cannot diverge.
    let (proxy_request, body_bytes) = crate::auth::ProxyRequest::post_json_exact(
        cfg.provider.clone(),
        "/chat/completions",
        body,
        true,
    )?;
    // ═══ TRACE (Task 10A): one record per actual attempt ═══════════════════
    // A remote broker re-serializes the JSON out of process: honest kind
    // `CloudProxy`, no wire-byte claim. Rule documented in `trace::emit`.
    let tracer = tr::begin_openai_tracer(
        trace,
        &cfg.provider,
        &cfg.model,
        if exact_wire_bytes {
            crate::runtime::trace::TransportKind::OpenAiChatCompletions
        } else {
            crate::runtime::trace::TransportKind::CloudProxy
        },
        &format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
        exact_wire_bytes.then_some(body_bytes.as_ref()),
        messages,
        system_prompt.as_deref(),
        tools_schema,
        tr::renamed_tool_losses(&name_map),
    )
    .await;
    // One-shot explicit content capture (`/trace next content`): a no-op in
    // every context without the arm. `body_bytes` is the serialized request
    // body this process built pre-send (exact wire bytes on the local
    // broker; the same body a remote broker re-serializes) — body only,
    // headers and credentials structurally never reach this seam.
    if let Some(tracer) = &tracer {
        trace.capture_request_content(tracer.request_id(), body_bytes.as_ref());
    }
    let mut attempt = tr::StreamAttempt::new(tracer);

    // The broker owns the API key and executes/signs the request; this path
    // never resolves or attaches a credential.
    let stream = broker
        .proxy_stream(proxy_request)
        .await
        // Privacy (spec §5.1): a broker proxy error may carry an upstream
        // response-body snippet; redact it to status-only before surfacing.
        .map_err(|e| {
            format!(
                "openai request failed: {}",
                super::net::redact_provider_proxy_error(&e.to_string())
            )
        });
    let mut stream = match stream {
        Ok(stream) => {
            attempt.mark_headers();
            stream
        }
        Err(msg) => {
            // Status parsed from the redacted static-prefix message only;
            // codes are `http_<status>` or the static `broker_error`.
            let status = tr::broker_error_status(&msg);
            let code = status.map_or_else(|| "broker_error".to_string(), |s| format!("http_{s}"));
            attempt.finish_failed(&code, status, None);
            return Err(msg.into());
        }
    };

    let mut decoder = StreamDecoder::new();
    let mut accumulated_text = String::new();
    let mut tool_use_blocks: Vec<Value> = Vec::new();
    let mut buf = bytes::BytesMut::with_capacity(8 * 1024);
    let mut sink: Vec<OaiEvent> = Vec::with_capacity(4);
    let mut trace_usage: Option<crate::runtime::trace::UsageMeta> = None;

    while let Some(chunk) = tokio::select! {
        chunk = stream.next() => chunk,
        _ = cancel.cancelled() => {
            attempt.finish_canceled(None, trace_usage);
            return Err("request canceled".into());
        }
    } {
        let chunk = match chunk {
            Ok(chunk) => {
                attempt.mark_first_byte();
                chunk
            }
            Err(e) => {
                attempt.finish_failed("stream_error", None, None);
                return Err(e.into());
            }
        };
        buf.extend_from_slice(&chunk);

        // Scan for newline-delimited SSE lines (SIMD-accelerated via memchr)
        while let Some(nl) = memchr::memchr(b'\n', &buf) {
            let line_bytes = buf.split_to(nl + 1); // O(1) — ref-counted split
            let line = std::str::from_utf8(&line_bytes[..nl]).unwrap_or("");

            sink.clear();
            decoder.push_line(line, &mut sink);
            capture_oai_trace_signals(&sink, &mut attempt, &mut trace_usage);
            handle_events(
                &sink,
                tx,
                &mut accumulated_text,
                &mut tool_use_blocks,
                &name_map,
                suppress_stream_deltas,
            );
        }
    }

    // Flush any remaining buffered line + final Done
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("");
        sink.clear();
        decoder.push_line(line, &mut sink);
        capture_oai_trace_signals(&sink, &mut attempt, &mut trace_usage);
        handle_events(
            &sink,
            tx,
            &mut accumulated_text,
            &mut tool_use_blocks,
            &name_map,
            suppress_stream_deltas,
        );
    }
    sink.clear();
    decoder.finish(&mut sink);
    capture_oai_trace_signals(&sink, &mut attempt, &mut trace_usage);
    handle_events(
        &sink,
        tx,
        &mut accumulated_text,
        &mut tool_use_blocks,
        &name_map,
        suppress_stream_deltas,
    );

    // Normalized stop reason: only when a finish_reason was actually
    // observed on the wire — never inferred.
    let stop_reason = decoder
        .finish_reason
        .as_deref()
        .map(tr::stop_reason_from_finish_reason);
    // Broker paths never observe the upstream HTTP status: honest `None`.
    attempt.finish_success(None, None, stop_reason, trace_usage);

    // Build Anthropic-shaped final response
    let mut content: Vec<Value> = Vec::new();
    if !accumulated_text.is_empty() {
        content.push(json!({"type": "text", "text": accumulated_text}));
    }
    content.extend(tool_use_blocks);

    Ok(json!({
        "role": "assistant",
        "content": content,
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_codex_stream_inner(
    cfg: &ProviderConfig,
    client: &reqwest::Client,
    broker: &std::sync::Arc<dyn crate::auth::CredentialBroker>,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    codex_request_role: crate::runtime::openai::catalog::CodexRequestRole,
    cancel: &tokio_util::sync::CancellationToken,
    max_retries: u32,
    trace: &crate::runtime::trace::TraceContext,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    // Build the exact provider-qualified plan before any credential or network
    // access. Logical Ultra is lowered here, never in the generic level enum.
    use crate::runtime::openai::catalog::plan_codex_execution;
    let qualified_model = format!("{}/{}", cfg.provider, cfg.model);
    let plan =
        match plan_codex_execution(&qualified_model, reasoning_level, codex_request_role, None) {
            Ok(plan) => plan,
            Err(error) => {
                tracing::debug!(
                    event = "codex_mode_plan",
                    qualified_model = %qualified_model,
                    requested_level = %reasoning_level,
                    runtime_role = codex_request_role.as_str(),
                    decision = "deny",
                    deny_code = error.code().as_str(),
                    network_attempted = false,
                    "Codex execution plan denied"
                );
                return Err(Box::new(error));
            }
        };
    if plan.automatic_delegation() && !codex_has_required_delegation_tools(tools_schema) {
        tracing::debug!(
            event = "codex_mode_plan",
            qualified_model = %qualified_model,
            requested_level = %reasoning_level,
            runtime_role = codex_request_role.as_str(),
            decision = "deny",
            deny_code = "ultra_requires_subagent_tools",
            network_attempted = false,
            "Codex execution plan denied"
        );
        return Err(
            "Ultra requires subagent_start, subagent_status, and subagent_collect tools".into(),
        );
    }
    tracing::debug!(
        event = "codex_mode_plan",
        qualified_model = %plan.qualified_model,
        requested_level = %plan.selected_level,
        execution_mode = plan.mode.as_str(),
        wire_effort = plan.wire_effort_label(),
        capability_source = plan.capability_source.map_or("none", |source| source.as_str()),
        multi_agent_version = plan.multi_agent_version_label(),
        runtime_role = plan.request_role.as_str(),
        multi_agent_mode = plan.multi_agent_mode_label(),
        automatic_delegation = plan.automatic_delegation(),
        decision = "allow",
        network_attempted = false,
        "Codex execution plan allowed"
    );
    // Every Codex credential, local or remote, crosses the broker boundary:
    // the broker vends an access token + expiry only (refresh tokens are
    // broker-owned), and this path never opens auth.json.
    let access = broker
        .access_token(crate::auth::OAuthProviderId::OpenAiCodex)
        .await
        .map_err(|e| e.to_string())?
        .token;
    // Account id is provider-owned metadata carried inside the Codex JWT.
    let account_id = crate::auth::extract_codex_account_id(&access)
        .ok_or("Failed to extract ChatGPT account id from Codex token — run `synaps login --provider openai-codex`")?;

    let (oai_tools, name_map) = translate::tools_to_oai(tools_schema);
    let oai_messages = translate::messages_to_oai(messages, system_prompt, &name_map);
    let tools: Vec<Value> = oai_tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description.unwrap_or_default(),
                "parameters": tool.function.parameters,
            })
        })
        .collect();

    // Use the shared pure helper so production and unit tests exercise identical
    // body construction — no duplication between call_codex_stream_inner and tests.
    let instructions = codex_instructions(system_prompt);
    let mut input_items = codex_input_messages(oai_messages);
    // Key on the pre-insertion head: the mode item varies with the execution
    // plan (level/role switches) and must not churn the routing key.
    let prompt_cache_key = codex_prompt_cache_key(&instructions, input_items.first());
    insert_codex_multi_agent_mode(&mut input_items, &plan);
    let input = serde_json::Value::Array(input_items);
    let body = build_codex_body(
        &cfg.model,
        &plan,
        input,
        instructions,
        tools,
        temperature,
        max_tokens,
        &prompt_cache_key,
    );

    let url = format!(
        "{}/codex/responses",
        cfg.base_url
            .trim_end_matches('/')
            .trim_end_matches("/codex")
    );
    tracing::debug!(url=%url, model=%cfg.model, "codex stream request");

    // Serialize ONCE. These exact bytes are digested for the trace AND are
    // the request body of every attempt — retries resend the identical
    // buffer, so one wire digest is honest for all attempt records.
    let body_bytes = bytes::Bytes::from(serde_json::to_vec(&body)?);
    // ═══ TRACE (Task 10A): one record per actual attempt ═══════════════════
    let tracer = tr::begin_openai_tracer(
        trace,
        &cfg.provider,
        &cfg.model,
        crate::runtime::trace::TransportKind::OpenAiResponses,
        &url,
        Some(body_bytes.as_ref()),
        messages,
        system_prompt.as_deref(),
        tools_schema,
        tr::renamed_tool_losses(&name_map),
    )
    .await;
    // One-shot explicit content capture (`/trace next content`): a no-op in
    // every context without the arm. `body_bytes` is the serialized request
    // body this process built pre-send (exact wire bytes on the local
    // broker; the same body a remote broker re-serializes) — body only,
    // headers and credentials structurally never reach this seam.
    if let Some(tracer) = &tracer {
        trace.capture_request_content(tracer.request_id(), body_bytes.as_ref());
    }
    let mut attempt = tr::StreamAttempt::new(tracer);

    let resp = send_with_retries(
        "codex",
        &url,
        || {
            client
                .post(&url)
                .bearer_auth(&access)
                .header("chatgpt-account-id", account_id.as_str())
                .header("originator", "synaps")
                .header("OpenAI-Beta", "responses=experimental")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(body_bytes.clone())
        },
        cancel,
        max_retries,
        &mut attempt,
    )
    .await?;
    // Direct HTTP: upstream status and provider request id are observed.
    let http_status = Some(resp.status().as_u16());
    let trace_rid = tr::provider_request_id_from_headers(resp.headers());

    let mut accumulated_text = String::new();
    let mut parser = CodexSseDecoder::default();
    let mut buf = bytes::BytesMut::with_capacity(8 * 1024);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = tokio::select! {
        chunk = stream.next() => chunk,
        _ = cancel.cancelled() => {
            attempt.finish_canceled(http_status, parser.trace_usage());
            return Err("request canceled".into());
        }
    } {
        let chunk = match chunk {
            Ok(chunk) => {
                attempt.mark_first_byte();
                chunk
            }
            Err(e) => {
                attempt.finish_failed("stream_error", http_status, trace_rid.clone());
                return Err(e.into());
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = memchr::memchr(b'\n', &buf) {
            let line_bytes = buf.split_to(nl + 1);
            let line = std::str::from_utf8(&line_bytes[..nl]).unwrap_or("");
            parser.push_line(line, tx, &mut accumulated_text);
        }
        if parser.saw_model_event {
            attempt.mark_first_model_event();
        }
    }
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("");
        parser.push_line(line, tx, &mut accumulated_text);
    }
    parser.finish();
    // Trailing-buffer flush and finish() can surface the first (or only)
    // model event — e.g. a payload dispatched by the tail line. Mark it so
    // first_model_event_ms is honest even for tail-only streams.
    if parser.saw_model_event {
        attempt.mark_first_model_event();
    }
    if let Err(failure) = parser.terminal_result() {
        attempt.finish_failed(failure.code, http_status, trace_rid);
        return Err(failure.message.into());
    }
    attempt.finish_success(http_status, trace_rid, None, parser.trace_usage());

    let mut content: Vec<Value> = Vec::new();
    if !accumulated_text.is_empty() {
        content.push(json!({"type": "text", "text": accumulated_text}));
    }
    content.extend(translate::tool_calls_to_content_blocks(
        &parser.completed_tools,
        &name_map,
    ));

    Ok(json!({
        "role": "assistant",
        "content": content,
    }))
}

/// Stable prompt-cache routing key for this conversation.
///
/// The Codex backend routes prefix-cache lookups by `prompt_cache_key`
/// (upstream codex-rs sends its conversation UUID on every request). Synaps
/// doesn't thread a session id down to the transport, so derive a
/// deterministic key from the stable head of the prompt: the instructions
/// plus the first input item. Identical heads hash to the same key — which
/// is exactly right, since those requests share the very prefix the cache
/// stores — and a /compact rewrite changes the key together with the prefix
/// it invalidates. Without this key, cache lookups are at the mercy of
/// load-balancer routing and long conversations re-pay their full prefix.
pub(crate) fn codex_prompt_cache_key(instructions: &str, first_item: Option<&Value>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(instructions.as_bytes());
    if let Some(item) = first_item {
        hasher.update(item.to_string().as_bytes());
    }
    format!("synaps-{:x}", hasher.finalize())
}

/// Pure body construction for Codex Responses-API requests.
///
/// Separated from the async function so it can be unit-tested without
/// any credential access or network I/O.
///
/// # Arguments
/// - `model`: the Codex model id (already validated by the caller)
/// - `plan`: reasoning plan (must already be validated against model capability)
/// - `input`: pre-built input items (from `codex_input_messages`)
/// - `instructions`: pre-built instructions string (from `codex_instructions`)
/// - `tools`: pre-built tool array
/// - `temperature`: optional temperature override
/// - `max_tokens`: optional max_output_tokens override
/// - `prompt_cache_key`: pre-computed routing key; computed on the
///   pre-insertion input head by the caller, so it cannot be derived here
#[allow(clippy::too_many_arguments)] // pure builder mirroring the wire body; grouping would obscure the 1:1 field mapping
pub(crate) fn build_codex_body(
    model: &str,
    plan: &crate::runtime::openai::catalog::CodexExecutionPlan,
    input: serde_json::Value,
    instructions: String,
    tools: Vec<serde_json::Value>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    prompt_cache_key: &str,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "medium" },
        "prompt_cache_key": prompt_cache_key,
    });
    if let Some(effort) = plan.wire_effort {
        body["reasoning"] = json!({ "effort": effort.as_str() });
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    if let Some(temp) = temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = max_tokens {
        body["max_output_tokens"] = json!(max);
    }
    body
}

const CODEX_MULTI_AGENT_MODE_OPEN: &str = "<multi_agent_mode>";
const CODEX_MULTI_AGENT_MODE_CLOSE: &str = "</multi_agent_mode>";
const CODEX_EXPLICIT_REQUEST_ONLY_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const CODEX_PROACTIVE_MULTI_AGENT_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";

fn codex_multi_agent_mode_item(
    plan: &crate::runtime::openai::catalog::CodexExecutionPlan,
) -> Option<Value> {
    use crate::runtime::openai::catalog::CodexMultiAgentMode;
    let body = match plan.multi_agent_mode? {
        CodexMultiAgentMode::ExplicitRequestOnly => CODEX_EXPLICIT_REQUEST_ONLY_TEXT,
        CodexMultiAgentMode::Proactive => CODEX_PROACTIVE_MULTI_AGENT_TEXT,
    };
    Some(json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": format!("{CODEX_MULTI_AGENT_MODE_OPEN}{body}{CODEX_MULTI_AGENT_MODE_CLOSE}"),
        }],
    }))
}

fn is_codex_multi_agent_mode_item(item: &Value) -> bool {
    item.pointer("/content/0/text")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            text.trim_start().starts_with(CODEX_MULTI_AGENT_MODE_OPEN)
                && text.trim_end().ends_with(CODEX_MULTI_AGENT_MODE_CLOSE)
        })
}

fn insert_codex_multi_agent_mode(
    input: &mut Vec<Value>,
    plan: &crate::runtime::openai::catalog::CodexExecutionPlan,
) {
    input.retain(|item| !is_codex_multi_agent_mode_item(item));
    let Some(item) = codex_multi_agent_mode_item(plan) else {
        return;
    };
    // Prefix-cache stability: the mode item goes at a FIXED position (head of
    // input), never relative to the latest user message. The previous
    // placement (before the last user item) moved every user turn, so the
    // prompt prefix diverged at the prior turn's user message and the entire
    // preceding agentic turn — often the bulk of the context — was re-billed
    // uncached (incident: 2026-07-16 codex cache investigation). At index 0
    // the item is byte-stable across turns; it only changes when the
    // execution plan's mode changes, which legitimately invalidates the
    // prefix once.
    input.insert(0, item);
}

fn codex_has_required_delegation_tools(tools_schema: &[Value]) -> bool {
    ["subagent_start", "subagent_status", "subagent_collect"]
        .into_iter()
        .all(|required| {
            tools_schema.iter().any(|tool| {
                tool.get("name").and_then(Value::as_str) == Some(required)
                    || tool.pointer("/function/name").and_then(Value::as_str) == Some(required)
            })
        })
}

const CODEX_AUTONOMOUS_LOOP_POLICY: &str = "\n\n[Synaps autonomous harness policy]\nThis harness is non-interactive after the user has provided the task/spec. Do not stop at phase boundaries, milestones, checkpoints, or after presenting a plan unless the full requested job is complete. Do not ask the user whether to continue. When a phase/checkpoint is reached, run any relevant verification and continue autonomously until the full requested job is complete, blocked by an unrecoverable error, or explicit user instructions require stopping.\n[End Synaps autonomous harness policy]";

fn codex_instructions(system_prompt: &Option<String>) -> String {
    let mut instructions = system_prompt.clone().unwrap_or_default();
    if instructions.contains("[Synaps autonomous harness policy]") {
        return instructions;
    }
    instructions.push_str(CODEX_AUTONOMOUS_LOOP_POLICY);
    instructions
}

fn codex_input_messages(messages: Vec<ChatMessage>) -> Vec<Value> {
    let mut out = Vec::new();
    for msg in messages {
        if let Some(tool_calls) = msg.tool_calls {
            for call in tool_calls {
                // The Responses API rejects `id` values that are not the
                // original `fc_…` output-item id. We only carry the
                // `call_…` correlation id today (see types::ToolCall),
                // so emit `id` *only* when the value actually starts
                // with `fc`. `call_id` is sufficient on its own to
                // correlate the eventual `function_call_output`.
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                });
                if call.id.starts_with("fc") {
                    item["id"] = Value::from(call.id.clone());
                }
                out.push(item);
            }
            continue;
        }
        if msg.role == "tool" {
            // Skip tool results with no call_id — sending an empty call_id
            // to the Codex API would cause a 400 with a confusing error.
            if let Some(call_id) = msg.tool_call_id {
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": msg.content.unwrap_or_default(),
                }));
            }
            continue;
        }
        out.push(json!({
            "role": msg.role,
            "content": msg.content.unwrap_or_default(),
        }));
    }
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResponsesStreamFailure {
    code: &'static str,
    message: &'static str,
}

#[derive(Default)]
struct CodexSseDecoder {
    buffer: String,
    active_tools: Vec<CodexToolAccumulator>,
    completed_tools: Vec<ToolCall>,
    /// Trace (Task 10A): set once the decoder parses any model event —
    /// feeds the first-model-event timing bucket.
    saw_model_event: bool,
    /// A terminal Responses event was observed. Failure/incomplete events
    /// take precedence over completion and are surfaced after the stream is
    /// drained; provider-controlled text is never retained.
    terminal_success: bool,
    terminal_failure: Option<ResponsesStreamFailure>,
    emitted_output: bool,
    /// Trace (Task 10A): provider-reported usage from `response.completed`
    /// (input including cached, output, cached slice). `None` until observed.
    observed_usage: Option<(u64, u64, u64)>,
}

#[derive(Default)]
struct CodexToolAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// Parse a function-call arguments string into a JSON `Value`, mirroring
/// `runtime::api::parse_tool_input` so the chat UI's `LlmEvent::ToolUse`
/// handling sees the same shape regardless of provider.
///
/// Empty / whitespace input becomes `{}`. Invalid JSON becomes
/// `{"__parse_error": "..."}` — the agent loop already understands that
/// shape and converts it into an `is_error: true` tool_result.
fn parse_tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => json!({ "__parse_error": format!("invalid tool input JSON: {}", e) }),
    }
}

impl CodexSseDecoder {
    fn push_line(
        &mut self,
        line: &str,
        tx: &mpsc::UnboundedSender<StreamEvent>,
        text_acc: &mut String,
    ) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if !self.buffer.is_empty() {
                let payload = std::mem::take(&mut self.buffer);
                self.push_payload(&payload, tx, text_acc);
            }
            return;
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            return;
        };
        if data == "[DONE]" {
            self.terminal_success = true;
            self.finish();
            return;
        }
        self.buffer.push_str(data);
    }

    fn push_payload(
        &mut self,
        payload: &str,
        tx: &mpsc::UnboundedSender<StreamEvent>,
        text_acc: &mut String,
    ) {
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            tracing::debug!(
                payload_bytes = payload.len(),
                "discarding malformed Responses SSE payload"
            );
            return;
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            event_type,
            "response.output_text.delta"
                | "response.output_item.added"
                | "response.function_call_arguments.delta"
                | "response.output_item.done"
                | "response.completed"
                | "response.done"
                | "response.failed"
                | "response.incomplete"
                | "error"
        ) {
            self.saw_model_event = true;
        }
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.emitted_output = true;
                    }
                    text_acc.push_str(delta);
                    let _ = tx.send(StreamEvent::Llm(crate::runtime::types::LlmEvent::Text(
                        delta.to_string(),
                    )));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    let idx = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    self.add_tool_from_item(idx, item, tx);
                }
            }
            "response.function_call_arguments.delta" => {
                let idx = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let tool = self.ensure_tool(idx);
                    tool.arguments.push_str(delta);
                    let tool_id = tool.id.clone();
                    let _ = tx.send(StreamEvent::Llm(
                        crate::runtime::types::LlmEvent::ToolUseDelta {
                            tool_id,
                            delta: delta.to_string(),
                        },
                    ));
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    let idx = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    self.complete_tool_from_item(idx, item, tx);
                }
            }
            "response.completed" | "response.done" => {
                self.push_usage(&event, tx);
                self.terminal_success = true;
                self.finish();
            }
            "response.failed" | "error" => {
                self.push_usage(&event, tx);
                self.terminal_failure = Some(ResponsesStreamFailure {
                    code: "responses_failed",
                    message: "Codex response failed in stream. Provider error details withheld because they can echo request content.",
                });
                self.finish();
            }
            "response.incomplete" => {
                self.push_usage(&event, tx);
                self.terminal_failure = Some(ResponsesStreamFailure {
                    code: "responses_incomplete",
                    message: "Codex response was incomplete. Retry the request or reduce the requested output/context size.",
                });
                self.finish();
            }
            _ => {
                if !event_type.is_empty() {
                    tracing::debug!(event_type, "ignoring unknown Responses SSE event");
                }
            }
        }
    }

    fn terminal_result(&self) -> Result<(), ResponsesStreamFailure> {
        if let Some(failure) = self.terminal_failure {
            return Err(failure);
        }
        if self.terminal_success {
            if self.emitted_output || !self.completed_tools.is_empty() {
                Ok(())
            } else {
                Err(ResponsesStreamFailure {
                    code: "responses_empty",
                    message: "Codex completed without text or tool output. Retry the request.",
                })
            }
        } else {
            Err(ResponsesStreamFailure {
                code: "responses_missing_terminal",
                message: "Codex response stream ended without a terminal event. Retry the request.",
            })
        }
    }

    fn ensure_tool(&mut self, idx: usize) -> &mut CodexToolAccumulator {
        while self.active_tools.len() <= idx {
            self.active_tools.push(CodexToolAccumulator::default());
        }
        &mut self.active_tools[idx]
    }

    fn add_tool_from_item(
        &mut self,
        idx: usize,
        item: &Value,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let tool = self.ensure_tool(idx);
        if let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            tool.id = id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.to_string();
        }
        if !tool.started && !tool.name.is_empty() {
            tool.started = true;
            let _ = tx.send(StreamEvent::Llm(
                crate::runtime::types::LlmEvent::ToolUseStart {
                    tool_name: tool.name.clone(),
                    tool_id: tool.id.clone(),
                },
            ));
        }
    }

    fn complete_tool_from_item(
        &mut self,
        idx: usize,
        item: &Value,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let tool = self.ensure_tool(idx);
        if let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            tool.id = id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.to_string();
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            tool.arguments = arguments.to_string();
        }
        if !tool.started && !tool.name.is_empty() {
            tool.started = true;
            let _ = tx.send(StreamEvent::Llm(
                crate::runtime::types::LlmEvent::ToolUseStart {
                    tool_name: tool.name.clone(),
                    tool_id: tool.id.clone(),
                },
            ));
        }
        let completed = if !tool.id.is_empty() && !tool.name.is_empty() {
            Some(ToolCall {
                id: tool.id.clone(),
                kind: "function".to_string(),
                function: super::types::FunctionCall {
                    name: tool.name.clone(),
                    arguments: tool.arguments.clone(),
                },
            })
        } else {
            None
        };
        if let Some(call) = completed {
            if self.completed_tools.iter().any(|done| done.id == call.id) {
                return;
            }
            self.emitted_output = true;
            // Emit the finalized `ToolUse` event so the chat UI can collapse
            // the streaming `ToolUseStart` (animated) into a stable
            // `ToolUse` block. Without this the bash-trace animation
            // persists forever and parallel tool blocks render as "still
            // running" even after they've completed. Mirrors the
            // Anthropic path in `runtime/api.rs` which emits the same
            // event on tool-use content_block_stop.
            let input = parse_tool_arguments(&call.function.arguments);
            let _ = tx.send(StreamEvent::Llm(crate::runtime::types::LlmEvent::ToolUse {
                tool_name: call.function.name.clone(),
                tool_id: call.id.clone(),
                input,
            }));
            self.completed_tools.push(ToolCall {
                id: call.id,
                kind: call.kind,
                function: call.function,
            });
        }
    }

    fn push_usage(&mut self, event: &Value, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let usage = event
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| event.get("usage"));
        let input = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // Responses-API usage nests cache hits under input_tokens_details;
        // input_tokens INCLUDES them. Downstream accounting uses Anthropic
        // semantics (context = input + cache_read + cache_creation, hit% =
        // cache_read/total), so report the cached slice separately and
        // subtract it from input — otherwise cache hits are invisible
        // (reported 0) and the context total double-counts.
        let cached = usage
            .and_then(|u| u.pointer("/input_tokens_details/cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input);
        if input > 0 || output > 0 {
            // Trace capture (metadata only): the provider-reported totals as
            // sent — input including the cached slice.
            if self.observed_usage.is_none() {
                self.observed_usage = Some((input, output, cached));
            }
            let _ = tx.send(StreamEvent::Session(
                crate::runtime::types::SessionEvent::Usage {
                    input_tokens: input - cached,
                    output_tokens: output,
                    cache_read_input_tokens: cached,
                    cache_creation_input_tokens: 0,
                    cache_creation_5m: None,
                    cache_creation_1h: None,
                    model: None,
                },
            ));
        }
    }

    /// Provider-reported usage for the trace record; `None` until observed.
    fn trace_usage(&self) -> Option<crate::runtime::trace::UsageMeta> {
        self.observed_usage
            .map(|(input, output, cached)| tr::provider_usage(input, output, cached))
    }

    fn finish(&mut self) {
        for tool in self.active_tools.drain(..) {
            if !tool.id.is_empty()
                && !tool.name.is_empty()
                && !self.completed_tools.iter().any(|done| done.id == tool.id)
            {
                self.emitted_output = true;
                self.completed_tools.push(ToolCall {
                    id: tool.id,
                    kind: "function".to_string(),
                    function: super::types::FunctionCall {
                        name: tool.name,
                        arguments: tool.arguments,
                    },
                });
            }
        }
    }
}

fn handle_events(
    events: &[OaiEvent],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    text_acc: &mut String,
    tool_blocks: &mut Vec<Value>,
    name_map: &translate::ToolNameMap,
    suppress_stream_deltas: bool,
) {
    for ev in events {
        if let OaiEvent::TextDelta(t) = ev {
            text_acc.push_str(t);
        }
        if let OaiEvent::ToolCallsComplete { calls, .. } = ev {
            tool_blocks.extend(translate::tool_calls_to_content_blocks(calls, name_map));
        }
        if !suppress_stream_deltas {
            if let Some(se) = translate::oai_event_to_llm(ev) {
                let _ = tx.send(se);
            }
        }
    }
}

#[cfg(test)]
mod codex_input_messages_tests {
    //! Regression tests for the Codex Responses-API `input` shape.
    //!
    //! Background: the Responses API distinguishes two ids per tool
    //! invocation — `id` (the *output item id*, prefix `fc_…`) and
    //! `call_id` (the *function call id*, prefix `call_…`). When echoing
    //! a previous `function_call` back as an input item, supplying an
    //! `id` whose value is *not* a `fc_…` triggers
    //!
    //!   400 Bad Request: Invalid 'input[N].id': 'call_…'.
    //!   Expected an ID that begins with 'fc'.
    //!
    //! `id` is *optional* on input items — only `call_id` is required to
    //! correlate the eventual `function_call_output`. We elect not to
    //! emit `id` unless we actually have a real `fc_…` value to send.

    use super::super::types::{ChatMessage, FunctionCall, ToolCall};
    use super::*;

    fn sample_tool_call() -> ToolCall {
        ToolCall {
            id: "call_nZYquCuGUh8Qs9H51dwHMDgs".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "bash".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
        }
    }

    #[test]
    fn codex_instructions_appends_autonomous_loop_policy() {
        let instructions = codex_instructions(&Some("Project-specific rules.".to_string()));
        assert!(instructions.contains("Project-specific rules."));
        assert!(instructions.contains("Do not stop at phase boundaries"));
        assert!(instructions.contains("Do not ask the user whether to continue"));
        assert!(
            instructions.contains("continue autonomously until the full requested job is complete")
        );
    }

    #[test]
    fn function_call_input_omits_non_fc_id() {
        let messages = vec![ChatMessage::assistant_tool_calls(vec![sample_tool_call()])];
        let out = codex_input_messages(messages);
        assert_eq!(out.len(), 1, "one tool_call → one input item");
        let item = &out[0];
        assert_eq!(
            item.get("type").and_then(Value::as_str),
            Some("function_call")
        );
        assert!(
            item.get("id").is_none(),
            "must not echo a non-`fc_` id back; got {:?}",
            item.get("id"),
        );
        assert_eq!(
            item.get("call_id").and_then(Value::as_str),
            Some("call_nZYquCuGUh8Qs9H51dwHMDgs"),
        );
        assert_eq!(item.get("name").and_then(Value::as_str), Some("bash"));
    }

    #[test]
    fn function_call_input_keeps_real_fc_id() {
        // If we ever do have a genuine `fc_…` id (round-tripped from the
        // Responses API), we *should* echo it.
        let mut call = sample_tool_call();
        call.id = "fc_abc123".to_string();
        let messages = vec![ChatMessage::assistant_tool_calls(vec![call])];
        let out = codex_input_messages(messages);
        let item = &out[0];
        assert_eq!(item.get("id").and_then(Value::as_str), Some("fc_abc123"));
        assert_eq!(
            item.get("call_id").and_then(Value::as_str),
            Some("fc_abc123")
        );
    }

    #[test]
    fn function_call_output_round_trips_call_id() {
        // The follow-up tool message must reference the original call_id.
        let messages = vec![ChatMessage::tool_result(
            "call_nZYquCuGUh8Qs9H51dwHMDgs",
            "bash",
            "total 0",
        )];
        let out = codex_input_messages(messages);
        let item = &out[0];
        assert_eq!(
            item.get("type").and_then(Value::as_str),
            Some("function_call_output"),
        );
        assert_eq!(
            item.get("call_id").and_then(Value::as_str),
            Some("call_nZYquCuGUh8Qs9H51dwHMDgs"),
        );
        assert_eq!(item.get("output").and_then(Value::as_str), Some("total 0"));
    }
}

#[cfg(test)]
mod codex_decoder_tests {
    //! Regression tests for `CodexSseDecoder`.
    //!
    //! The decoder is sync — we drive it via `push_line` and capture
    //! emitted `StreamEvent`s from an `unbounded_channel` using
    //! `try_recv`, no async runtime needed.

    use super::*;
    use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};

    fn collect_events(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn drive(lines: &[&str]) -> (CodexSseDecoder, String, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut decoder = CodexSseDecoder::default();
        let mut text_acc = String::new();
        for line in lines {
            decoder.push_line(line, &tx, &mut text_acc);
        }
        let events = collect_events(&mut rx);
        (decoder, text_acc, events)
    }

    #[test]
    fn text_delta_aggregates_into_text_acc_and_emits_text_events() {
        let lines = [
            r#"data: {"type":"response.output_text.delta","delta":"Hello, "}"#,
            "",
            r#"data: {"type":"response.output_text.delta","delta":"world!"}"#,
            "",
        ];
        let (_decoder, text_acc, events) = drive(&lines);
        assert_eq!(text_acc, "Hello, world!");
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello, ", "world!"]);
    }

    #[test]
    fn single_function_call_completes_via_output_item_done() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\""}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":":\"ls\"}"}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            "",
        ];
        let (decoder, _text, events) = drive(&lines);

        assert_eq!(decoder.completed_tools.len(), 1);
        let tool = &decoder.completed_tools[0];
        assert_eq!(tool.id, "call_abc");
        assert_eq!(tool.function.name, "bash");
        assert_eq!(tool.function.arguments, r#"{"cmd":"ls"}"#);

        // Exactly one ToolUseStart for the tool.
        let starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
                    Some((tool_name.as_str(), tool_id.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![("bash", "call_abc")],
            "exactly one ToolUseStart with correct tool_id"
        );

        // Two argument deltas streamed (each carrying the tool_id so
        // parallel calls can be routed correctly by the chat UI).
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
                    Some((tool_id.as_str(), delta.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec![("call_abc", r#"{"cmd""#), ("call_abc", r#":"ls"}"#)]
        );
    }

    #[test]
    fn parallel_tool_calls_indexed_separately() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"a\"}"}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\":\"ls\"}"}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read","arguments":"{\"path\":\"a\"}"}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);

        assert_eq!(decoder.completed_tools.len(), 2);
        let mut by_id: std::collections::BTreeMap<&str, &ToolCall> =
            std::collections::BTreeMap::new();
        for tool in &decoder.completed_tools {
            by_id.insert(tool.id.as_str(), tool);
        }
        assert_eq!(by_id["call_1"].function.name, "bash");
        assert_eq!(by_id["call_1"].function.arguments, r#"{"cmd":"ls"}"#);
        assert_eq!(by_id["call_2"].function.name, "read");
        assert_eq!(by_id["call_2"].function.arguments, r#"{"path":"a"}"#);
    }

    #[test]
    fn output_item_done_emits_tool_use_event() {
        // Regression: the codex decoder must emit `LlmEvent::ToolUse` once a
        // function_call's `output_item.done` arrives so the chat UI can
        // collapse `ChatMessage::ToolUseStart` (animated) into the finalized
        // `ChatMessage::ToolUse`. Without this the bash-trace animation
        // persists forever and parallel tool blocks render as "still
        // running" even after they've completed.
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"command\":\"ls\"}"}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);

        let tool_uses: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name,
                    tool_id,
                    input,
                }) => Some((tool_name.as_str(), tool_id.as_str(), input.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_uses.len(),
            1,
            "expected exactly one ToolUse finalize event"
        );
        assert_eq!(tool_uses[0].0, "bash");
        assert_eq!(tool_uses[0].1, "call_abc");
        assert_eq!(
            tool_uses[0].2,
            serde_json::json!({"command": "ls"}),
            "input must be parsed as a JSON Value, not a string"
        );
    }

    #[test]
    fn parallel_tool_calls_emit_tool_use_per_index() {
        // Regression: parallel tool calls must each get their own ToolUse
        // finalize event with the correct tool_id, so the chat UI can route
        // their results back to the right block by id.
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read"}}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read","arguments":"{\"path\":\"a\"}"}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);

        let tool_uses: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name,
                    tool_id,
                    input,
                }) => Some((tool_name.clone(), tool_id.clone(), input.clone())),
                _ => None,
            })
            .collect();

        assert_eq!(tool_uses.len(), 2, "one ToolUse finalize per parallel call");
        let by_id: std::collections::BTreeMap<&str, &(String, String, serde_json::Value)> =
            tool_uses.iter().map(|t| (t.1.as_str(), t)).collect();
        assert_eq!(by_id["call_1"].0, "bash");
        assert_eq!(by_id["call_1"].2, serde_json::json!({"command": "ls"}));
        assert_eq!(by_id["call_2"].0, "read");
        assert_eq!(by_id["call_2"].2, serde_json::json!({"path": "a"}));
    }

    #[test]
    fn malformed_arguments_emit_tool_use_with_parse_error() {
        // If the model produces invalid JSON arguments, surface a structured
        // parse error in the `input` (matching how the Anthropic path
        // handles it via parse_tool_input) so the agent loop can return an
        // error tool_result instead of silently dropping the tool.
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_bad","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_bad","name":"bash","arguments":"{not json"}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);

        let tool_use = events.iter().find_map(|e| match e {
            StreamEvent::Llm(LlmEvent::ToolUse { input, .. }) => Some(input.clone()),
            _ => None,
        });
        let input = tool_use.expect("ToolUse event missing");
        assert!(
            input.get("__parse_error").and_then(Value::as_str).is_some(),
            "malformed arguments must surface __parse_error, got {input}"
        );
    }

    #[test]
    fn response_completed_emits_usage_event() {
        let lines = [
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":42,"output_tokens":17}}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            }) => Some((*input_tokens, *output_tokens)),
            _ => None,
        });
        assert_eq!(usage, Some((42, 17)));
    }

    /// Responses-API usage nests cache hits under input_tokens_details, and
    /// input_tokens INCLUDES them. The decoder must surface the cached slice
    /// as cache_read (Anthropic semantics) and subtract it from input —
    /// dropping it (the old hardcoded 0) made every codex turn look 100%
    /// uncached (incident: 2026-07-16 codex cache investigation).
    #[test]
    fn response_completed_surfaces_cached_tokens_without_double_count() {
        let lines = [
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":900},"output_tokens":17}}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                ..
            }) => Some((*input_tokens, *output_tokens, *cache_read_input_tokens)),
            _ => None,
        });
        assert_eq!(
            usage,
            Some((100, 17, 900)),
            "input must exclude the cached slice; cache_read must carry it"
        );
    }

    #[test]
    fn response_completed_with_zero_usage_emits_nothing() {
        let lines = [
            r#"data: {"type":"response.output_text.delta","delta":"ok"}"#,
            "",
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "",
        ];
        let (decoder, _text, events) = drive(&lines);
        let any_usage = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })));
        assert!(!any_usage, "zero-token usage should be suppressed");
        assert_eq!(decoder.terminal_result(), Ok(()));
    }

    #[test]
    fn response_failed_is_a_terminal_failure_without_retaining_provider_text() {
        let lines = [
            r#"data: {"type":"response.failed","response":{"error":{"type":"server_error","message":"ECHOED:secret prompt"}}}"#,
            "",
        ];
        let (decoder, text, events) = drive(&lines);
        let failure = decoder.terminal_result().expect_err("must fail");
        assert_eq!(failure.code, "responses_failed");
        assert_eq!(
            failure.message,
            "Codex response failed in stream. Provider error details withheld because they can echo request content."
        );
        assert!(!failure.message.contains("ECHOED"));
        assert!(text.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn top_level_error_event_is_a_terminal_failure() {
        let lines = [
            r#"data: {"type":"error","error":{"type":"server_error","message":"private"}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        assert_eq!(
            decoder.terminal_result().expect_err("must fail").code,
            "responses_failed"
        );
    }

    #[test]
    fn response_incomplete_is_not_misreported_as_empty_success() {
        let lines = [
            r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        let failure = decoder.terminal_result().expect_err("must fail");
        assert_eq!(failure.code, "responses_incomplete");
        assert!(failure.message.contains("incomplete"));
        assert!(!failure.message.contains("max_output_tokens"));
    }

    #[test]
    fn completed_without_text_or_tools_fails_at_transport_boundary() {
        let lines = [
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":0}}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        assert_eq!(
            decoder.terminal_result().expect_err("must fail").code,
            "responses_empty"
        );
    }

    #[test]
    fn eof_without_terminal_event_fails_closed() {
        let lines = [
            r#"data: {"type":"response.created","response":{"id":"resp_123"}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        assert_eq!(
            decoder.terminal_result().expect_err("must fail").code,
            "responses_missing_terminal"
        );
    }

    #[test]
    fn done_marker_finishes_decoder() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_x","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}"#,
            "",
            "data: [DONE]",
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        // active_tools promoted to completed_tools by finish().
        assert_eq!(decoder.completed_tools.len(), 1);
        assert_eq!(decoder.completed_tools[0].id, "call_x");
        assert_eq!(decoder.completed_tools[0].function.arguments, "{}");
    }

    #[test]
    fn finish_is_idempotent_no_double_emit() {
        let lines = [
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_y","name":"bash","arguments":"{}"}}"#,
            "",
        ];
        let (mut decoder, _text, _events) = drive(&lines);
        assert_eq!(decoder.completed_tools.len(), 1);

        // Calling finish() again must not duplicate the tool.
        decoder.finish();
        assert_eq!(
            decoder.completed_tools.len(),
            1,
            "finish() called twice must not double-emit"
        );
    }

    #[test]
    fn finish_drains_active_tools_for_state_hygiene() {
        // After [DONE], any leftover active tool entries should have been
        // promoted *and* drained from `active_tools`. This guards against
        // future code paths that re-call finish() (or new event types
        // that would otherwise re-iterate the old buffer).
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_z","name":"bash"}}"#,
            "",
            "data: [DONE]",
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        assert_eq!(decoder.completed_tools.len(), 1);
        assert!(
            decoder.active_tools.is_empty(),
            "active_tools must be drained after finish()"
        );
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let lines = [
            r#"data: {"type":"response.future_unknown_event","payload":{"x":1}}"#,
            "",
            r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
            "",
        ];
        let (_decoder, text_acc, _events) = drive(&lines);
        assert_eq!(text_acc, "hi");
    }
}

#[cfg(test)]
mod broker_stream_tests {
    //! Broker-boundary streaming tests: the OpenAI-compatible stream path is
    //! driven end-to-end through an in-process `LocalBroker` against a fake
    //! upstream. The runtime side supplies NO credential — the broker applies
    //! it — and SSE deltas are forwarded to the event channel in real time.

    use super::*;
    use crate::auth::{CredentialBroker, LocalBroker};
    use std::sync::Arc;

    async fn spawn_fake_openai_sse() -> (String, Arc<std::sync::Mutex<String>>) {
        use axum::routing::post;
        let seen_auth = Arc::new(std::sync::Mutex::new(String::new()));
        let seen = seen_auth.clone();
        let app = axum::Router::new().route(
            "/chat/completions",
            post(move |headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let body = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
                        "data: [DONE]\n\n",
                    );
                    ([("content-type", "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen_auth)
    }

    #[tokio::test]
    async fn oai_stream_flows_through_broker_and_forwards_deltas() {
        let (upstream, seen_auth) = spawn_fake_openai_sse().await;
        let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
            reqwest::Client::new(),
            upstream,
        ));
        let cfg = ProviderConfig {
            base_url: "unused-broker-derives-the-url".to_string(),
            model: "test-model".to_string(),
            provider: "local".to_string(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let cancel = tokio_util::sync::CancellationToken::new();

        let result = call_oai_stream_inner(
            &cfg,
            &broker,
            &[],
            &None,
            &[],
            &tx,
            None,
            None,
            0,
            &cancel,
            &crate::runtime::trace::TraceContext::disabled(),
            true,
            false,
        )
        .await
        .expect("stream must complete");

        // Final Anthropic-shaped value carries the accumulated text.
        assert_eq!(result["content"][0]["text"], "Hello");

        // Deltas were forwarded in real time on the event channel.
        drop(tx);
        let mut streamed = String::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Llm(crate::runtime::types::LlmEvent::Text(t)) = ev {
                streamed.push_str(&t);
            }
        }
        assert_eq!(streamed, "Hello");

        // The credential was applied by the broker, not by this call site.
        assert_eq!(&*seen_auth.lock().unwrap(), "Bearer local");
    }

    /// Broker errors surface as typed failures without opening a stream and
    /// without any credential material in the message.
    #[tokio::test]
    async fn suppressed_sync_route_does_not_enqueue_display_deltas() {
        let (upstream, _seen_auth) = spawn_fake_openai_sse().await;
        let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
            reqwest::Client::new(),
            upstream,
        ));
        let cfg = ProviderConfig {
            base_url: "unused-broker-derives-the-url".to_string(),
            model: "test-model".to_string(),
            provider: "local".to_string(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let result = call_oai_stream_inner(
            &cfg,
            &broker,
            &[],
            &None,
            &[],
            &tx,
            None,
            None,
            0,
            &tokio_util::sync::CancellationToken::new(),
            &crate::runtime::trace::TraceContext::disabled(),
            true,
            true,
        )
        .await
        .expect("stream must complete");
        assert_eq!(result["content"][0]["text"], "Hello");
        assert!(
            rx.try_recv().is_err(),
            "sync route must suppress display deltas at production"
        );
    }

    #[tokio::test]
    async fn oai_stream_broker_error_fails_closed() {
        // Unconfigured static provider → NotConfigured, no upstream contact.
        let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::new(reqwest::Client::new()));
        let cfg = ProviderConfig {
            base_url: String::new(),
            model: "m".to_string(),
            provider: "definitely-not-a-provider".to_string(),
        };
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = call_oai_stream_inner(
            &cfg,
            &broker,
            &[],
            &None,
            &[],
            &tx,
            None,
            None,
            0,
            &cancel,
            &crate::runtime::trace::TraceContext::disabled(),
            true,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown provider"), "got: {err}");
        assert!(!err.to_lowercase().contains("bearer"));
    }

    /// Phase 1 privacy (spec §5.1): a hostile upstream answers the streaming
    /// open with HTTP 500 whose body echoes the full request. The broker's
    /// transport error carries a body snippet; the runtime must not surface
    /// or log it — status + provider label only.
    #[tokio::test]
    async fn oai_stream_upstream_error_body_never_surfaces() {
        use axum::response::IntoResponse;
        const SENTINEL: &str = "PH1-OAI-BROKER-SENTINEL-91d2-RAW";
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move |body: String| async move {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-type", "application/json")],
                    format!("{{\"error\":{{\"message\":\"ECHOED:{body}\"}}}}"),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
            reqwest::Client::new(),
            format!("http://{addr}"),
        ));
        let cfg = ProviderConfig {
            base_url: "unused-broker-derives-the-url".to_string(),
            model: "test-model".to_string(),
            provider: "local".to_string(),
        };
        let tools = vec![serde_json::json!({
            "name": "ph1_secret_tool_zz",
            "description": "internal tool schema",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(
            serde_json::json!({"role": "user", "content": SENTINEL}),
        )];
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        let cancel = tokio_util::sync::CancellationToken::new();

        let err = call_oai_stream_inner(
            &cfg,
            &broker,
            &tools,
            &Some(format!("system secret {SENTINEL}")),
            &msgs,
            &tx,
            None,
            None,
            0,
            &cancel,
            &crate::runtime::trace::TraceContext::disabled(),
            true,
            false,
        )
        .await
        .expect_err("500 must fail")
        .to_string();

        assert!(err.contains("500"), "status must survive: {err}");
        for banned in ["ECHOED", SENTINEL, "ph1_secret_tool_zz", "system secret"] {
            assert!(
                !err.contains(banned),
                "provider body content `{banned}` leaked into the surfaced error: {err}"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_xai_responses_stream_inner(
    cfg: &ProviderConfig,
    broker: &std::sync::Arc<dyn crate::auth::CredentialBroker>,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    max_tokens: Option<u32>,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    cancel: &tokio_util::sync::CancellationToken,
    trace: &crate::runtime::trace::TraceContext,
    exact_wire_bytes: bool,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (oai_tools, names) = translate::tools_to_oai(tools_schema);
    let input = codex_input_messages(translate::messages_to_oai(messages, system_prompt, &names));
    let tools: Vec<Value> = oai_tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description.unwrap_or_default(),
                "parameters": tool.function.parameters,
            })
        })
        .collect();
    // Pure, validated body construction — rejects unsupported reasoning
    // combinations BEFORE any broker credential access or network I/O.
    let body = build_xai_body(
        &cfg.model,
        reasoning_level,
        serde_json::to_value(input)?,
        tools,
        max_tokens,
    )
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    // Serialize ONCE via the sanctioned constructor: the digested bytes are
    // the very bytes the broker sends upstream (`body_bytes` handoff,
    // LocalBroker verbatim) — `body`/`body_bytes` cannot diverge. On a
    // remote broker the daemon re-serializes out of process → honest
    // `CloudProxy` kind and no wire-byte claim.
    let (proxy_request, body_bytes) =
        crate::auth::ProxyRequest::post_json_exact("xai-auth", "/responses", body, true)?;
    let tracer = tr::begin_openai_tracer(
        trace,
        &cfg.provider,
        &cfg.model,
        if exact_wire_bytes {
            crate::runtime::trace::TransportKind::OpenAiResponses
        } else {
            crate::runtime::trace::TransportKind::CloudProxy
        },
        &format!("{}/responses", cfg.base_url.trim_end_matches('/')),
        exact_wire_bytes.then_some(body_bytes.as_ref()),
        messages,
        system_prompt.as_deref(),
        tools_schema,
        tr::renamed_tool_losses(&names),
    )
    .await;
    // One-shot explicit content capture (`/trace next content`): a no-op in
    // every context without the arm. `body_bytes` is the serialized request
    // body this process built pre-send (exact wire bytes on the local
    // broker; the same body a remote broker re-serializes) — body only,
    // headers and credentials structurally never reach this seam.
    if let Some(tracer) = &tracer {
        trace.capture_request_content(tracer.request_id(), body_bytes.as_ref());
    }
    let mut attempt = tr::StreamAttempt::new(tracer);
    let stream = broker
        .proxy_stream(proxy_request)
        .await
        // Privacy (spec §5.1): a broker proxy error may carry an upstream
        // response-body snippet; redact it to status-only before surfacing.
        .map_err(|e| super::net::redact_provider_proxy_error(&e.to_string()));
    let mut stream = match stream {
        Ok(stream) => {
            attempt.mark_headers();
            stream
        }
        Err(msg) => {
            let status = tr::broker_error_status(&msg);
            let code = status.map_or_else(|| "broker_error".to_string(), |s| format!("http_{s}"));
            attempt.finish_failed(&code, status, None);
            return Err(msg.into());
        }
    };
    let mut text = String::new();
    let mut parser = CodexSseDecoder::default();
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = tokio::select! {
        c = stream.next() => c,
        _ = cancel.cancelled() => {
            attempt.finish_canceled(None, parser.trace_usage());
            return Err("request canceled".into());
        }
    } {
        let chunk = match chunk {
            Ok(chunk) => {
                attempt.mark_first_byte();
                chunk
            }
            Err(e) => {
                attempt.finish_failed("stream_error", None, None);
                return Err(e.into());
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(n) = memchr::memchr(b'\n', &buf) {
            let line = buf.split_to(n + 1);
            parser.push_line(std::str::from_utf8(&line[..n]).unwrap_or(""), tx, &mut text);
        }
        if parser.saw_model_event {
            attempt.mark_first_model_event();
        }
    }
    if !buf.is_empty() {
        parser.push_line(std::str::from_utf8(&buf).unwrap_or(""), tx, &mut text);
    }
    parser.finish();
    // Trailing-buffer flush and finish() can surface the first (or only)
    // model event; mark it so first_model_event_ms is honest for tail-only
    // streams (mirrors the Codex direct path above).
    if parser.saw_model_event {
        attempt.mark_first_model_event();
    }
    // Broker paths never observe the upstream HTTP status; the Responses
    // stream does not yet expose a normalized stop reason — both stay
    // honest `None` rather than guessed values.
    if let Err(failure) = parser.terminal_result() {
        attempt.finish_failed(failure.code, None, None);
        return Err(failure.message.into());
    }
    attempt.finish_success(None, None, None, parser.trace_usage());
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type":"text","text":text}));
    }
    content.extend(translate::tool_calls_to_content_blocks(
        &parser.completed_tools,
        &names,
    ));
    Ok(json!({"role":"assistant","content":content}))
}

/// Pure, validated body construction for the xAI Responses API.
///
/// Enforces the exact-model reasoning matrix from
/// `docs/anthropic-xai-reasoning-modes-spec.md` BEFORE any credential or
/// network access, via the shared per-provider validator:
/// - `Off` is rejected (never silently omitted) on models whose reasoning
///   cannot be disabled; on the non-reasoning model it is trivially omission.
/// - `Adaptive` omits the `reasoning` field → documented provider default.
/// - Named levels are emitted as exact `reasoning:{effort:"..."}` only when
///   the exact model id documents them; otherwise `Err` (fail closed).
pub(crate) fn build_xai_body(
    model: &str,
    level: agent_core::reasoning::ReasoningLevel,
    input: Value,
    tools: Vec<Value>,
    max_tokens: Option<u32>,
) -> Result<Value, String> {
    use agent_core::reasoning::ReasoningLevel;
    crate::runtime::openai::catalog::validation::validate_reasoning_mutation(
        &format!("xai-auth/{model}"),
        level,
    )?;
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(model));
    body.insert("input".into(), input);
    body.insert("stream".into(), json!(true));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max) = max_tokens {
        body.insert("max_output_tokens".into(), json!(max));
    }
    match level {
        // Off (validated: only reachable where reasoning is absent/disableable)
        // and Adaptive: omit `reasoning` — provider default applies.
        ReasoningLevel::Off | ReasoningLevel::Adaptive => {}
        l => {
            body.insert("reasoning".into(), json!({ "effort": l.as_str() }));
        }
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod xai_tests {
    use super::*;
    use agent_core::reasoning::ReasoningLevel;

    #[test]
    fn xai_fixture_is_public_responses_shape() {
        let fixture = serde_json::json!({"type":"response.output_text.delta","delta":"hello"});
        assert_eq!(fixture["type"], "response.output_text.delta");
        assert_eq!(fixture["delta"], "hello");
    }

    fn body_for(model: &str, level: ReasoningLevel) -> Result<Value, String> {
        build_xai_body(model, level, json!([]), Vec::new(), Some(1024))
    }

    // ── Exact Responses wire (spec: anthropic-xai-reasoning-modes) ───────────

    #[test]
    fn grok45_emits_exact_documented_efforts() {
        for (level, effort) in [
            (ReasoningLevel::Low, "low"),
            (ReasoningLevel::Medium, "medium"),
            (ReasoningLevel::High, "high"),
        ] {
            let body = body_for("grok-4.5", level).expect("supported effort");
            assert_eq!(body["reasoning"], json!({"effort": effort}), "{level}");
            assert_eq!(body["model"], "grok-4.5");
            assert_eq!(body["stream"], json!(true));
            assert_eq!(body["max_output_tokens"], json!(1024));
        }
    }

    #[test]
    fn adaptive_omits_reasoning_field_provider_default() {
        for model in [
            "grok-4.5",
            "grok-4.5-latest",
            "grok-4.3",
            "grok-4.20-0309-non-reasoning",
        ] {
            let body = body_for(model, ReasoningLevel::Adaptive).expect("adaptive is omission");
            assert!(body.get("reasoning").is_none(), "{model}");
        }
    }

    #[test]
    fn off_is_rejected_pre_network_when_reasoning_cannot_be_disabled() {
        for model in [
            "grok-4.5",
            "grok-4.5-latest",
            "grok-4.20-multi-agent-0309",
            "grok-4.3",
        ] {
            let err = body_for(model, ReasoningLevel::Off).unwrap_err();
            assert!(err.contains(model), "{model}: {err}");
        }
        // Non-reasoning model: Off is trivially satisfied by omission.
        let body = body_for("grok-4.20-0309-non-reasoning", ReasoningLevel::Off).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn unsupported_named_efforts_are_rejected_pre_network() {
        // 4.5 has no documented xhigh; max/ultra never exist on xAI.
        for level in [
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
        ] {
            assert!(body_for("grok-4.5", level).is_err(), "{level}");
        }
        // Intrinsic-reasoning models have no documented effort control.
        assert!(body_for("grok-4.3", ReasoningLevel::Medium).is_err());
        // Non-reasoning models reject named reasoning outright.
        assert!(body_for("grok-4.20-0309-non-reasoning", ReasoningLevel::Medium).is_err());
        // Unknown exact ids fail closed.
        assert!(body_for("grok-9000", ReasoningLevel::Medium).is_err());
    }

    #[test]
    fn multi_agent_xhigh_is_exact_agent_count_control() {
        let body = body_for("grok-4.20-multi-agent-0309", ReasoningLevel::XHigh).unwrap();
        assert_eq!(body["reasoning"], json!({"effort": "xhigh"}));
    }

    #[test]
    fn responses_wire_shape_tools_flat_and_no_chat_completions_fields() {
        let tools = vec![json!({
            "type": "function",
            "name": "get_weather",
            "description": "d",
            "parameters": {"type": "object"}
        })];
        let body =
            build_xai_body("grok-4.5", ReasoningLevel::High, json!([]), tools, None).unwrap();
        // Responses API: flat tool objects + `input`, never chat-completions
        // `messages`/nested function wrappers.
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert!(body["tools"][0].get("function").is_none());
        assert!(body.get("input").is_some());
        assert!(body.get("messages").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }
}

// ─── Codex wire body tests ────────────────────────────────────────────────────

#[cfg(test)]
mod codex_wire_tests {
    use crate::runtime::openai::catalog::{
        plan_codex_execution, CodexMultiAgentMode, CodexRequestRole,
    };
    use agent_core::reasoning::ReasoningLevel;

    fn plan(
        level: ReasoningLevel,
        role: CodexRequestRole,
    ) -> crate::runtime::openai::catalog::CodexExecutionPlan {
        plan_codex_execution("openai-codex/gpt-5.6-sol", level, role, None)
            .expect("Sol request plan")
    }

    #[allow(clippy::too_many_arguments)]
    fn body_for_level(
        model: &str,
        level: ReasoningLevel,
        input: serde_json::Value,
        instructions: String,
        tools: Vec<serde_json::Value>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> serde_json::Value {
        let qualified = format!("openai-codex/{model}");
        let plan = plan_codex_execution(&qualified, level, CodexRequestRole::Foreground, None)
            .expect("request plan");
        super::build_codex_body(
            model,
            &plan,
            input,
            instructions,
            tools,
            temperature,
            max_tokens,
            "synaps-test-cache-key",
        )
    }

    /// Validate → emit exact reasoning.effort shape for known levels.
    fn codex_effort_for(model_id: &str, level: ReasoningLevel) -> Result<String, String> {
        let qualified = format!("openai-codex/{model_id}");
        let plan = plan_codex_execution(&qualified, level, CodexRequestRole::Foreground, None)
            .map_err(|error| error.to_string())?;
        plan.wire_effort
            .map(|effort| effort.as_str().to_string())
            .ok_or_else(|| "reasoning effort omitted".to_string())
    }

    #[test]
    fn sol_lowers_ultra_to_max_for_requests() {
        let effort = codex_effort_for("gpt-5.6-sol", ReasoningLevel::Ultra).unwrap();
        assert_eq!(effort, "max");
    }

    #[test]
    fn sol_emits_max_exact() {
        let effort = codex_effort_for("gpt-5.6-sol", ReasoningLevel::Max).unwrap();
        assert_eq!(effort, "max");
    }

    #[test]
    fn sol_emits_xhigh_exact() {
        let effort = codex_effort_for("gpt-5.6-sol", ReasoningLevel::XHigh).unwrap();
        assert_eq!(effort, "xhigh");
    }

    #[test]
    fn luna_emits_max_exact() {
        let effort = codex_effort_for("gpt-5.6-luna", ReasoningLevel::Max).unwrap();
        assert_eq!(effort, "max");
    }

    #[test]
    fn luna_rejects_ultra_before_network() {
        // Validation must fail before any credential/network access.
        let err = codex_effort_for("gpt-5.6-luna", ReasoningLevel::Ultra).unwrap_err();
        assert!(err.contains("ultra"), "{err}");
        assert!(err.contains("gpt-5.6-luna"), "{err}");
    }

    #[test]
    fn gpt55_emits_xhigh_exact() {
        let effort = codex_effort_for("gpt-5.5", ReasoningLevel::XHigh).unwrap();
        assert_eq!(effort, "xhigh");
    }

    #[test]
    fn gpt55_rejects_max_before_network() {
        assert!(codex_effort_for("gpt-5.5", ReasoningLevel::Max).is_err());
        assert!(codex_effort_for("gpt-5.5", ReasoningLevel::Ultra).is_err());
    }

    #[test]
    fn off_and_adaptive_omit_reasoning_field() {
        // Off/Adaptive: build_codex_body must NOT emit a "reasoning" key.
        for level in [ReasoningLevel::Off, ReasoningLevel::Adaptive] {
            let body = body_for_level(
                "gpt-5.6-sol",
                level,
                serde_json::json!([]),
                "sys".to_string(),
                vec![],
                None,
                None,
            );
            assert!(
                body.get("reasoning").is_none(),
                "{level:?} must not emit reasoning field, body={body}"
            );
        }
    }

    #[test]
    fn max_as_str_is_not_xhigh() {
        assert_ne!(ReasoningLevel::Max.as_str(), ReasoningLevel::XHigh.as_str());
        assert_eq!(ReasoningLevel::Max.as_str(), "max");
    }

    #[test]
    fn ultra_mode_context_is_exact_proactive_developer_item() {
        let plan = plan(ReasoningLevel::Ultra, CodexRequestRole::Foreground);
        assert_eq!(plan.multi_agent_mode, Some(CodexMultiAgentMode::Proactive));
        let item = super::codex_multi_agent_mode_item(&plan).expect("Ultra context item");
        assert_eq!(
            item.get("type").and_then(serde_json::Value::as_str),
            Some("message")
        );
        assert_eq!(
            item.get("role").and_then(serde_json::Value::as_str),
            Some("developer")
        );
        assert_eq!(
            item.pointer("/content/0/text")
                .and_then(serde_json::Value::as_str),
            Some(concat!(
                "<multi_agent_mode>",
                "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.",
                "</multi_agent_mode>"
            ))
        );
    }

    #[test]
    fn max_and_xhigh_mode_context_is_exact_explicit_only_item() {
        for level in [ReasoningLevel::Max, ReasoningLevel::XHigh] {
            let plan = plan(level, CodexRequestRole::Foreground);
            assert_eq!(
                plan.multi_agent_mode,
                Some(CodexMultiAgentMode::ExplicitRequestOnly)
            );
            let item = super::codex_multi_agent_mode_item(&plan).expect("v2 context item");
            assert_eq!(
                item.pointer("/content/0/text")
                    .and_then(serde_json::Value::as_str),
                Some(concat!(
                    "<multi_agent_mode>",
                    "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.",
                    "</multi_agent_mode>"
                )),
                "{level:?}"
            );
        }
    }

    #[test]
    fn mode_context_is_inserted_once_at_stable_head_position() {
        let plan = plan(ReasoningLevel::Ultra, CodexRequestRole::Foreground);
        let mut input = vec![
            serde_json::json!({"role":"user","content":"first"}),
            serde_json::json!({"role":"assistant","content":"answer"}),
            serde_json::json!({"role":"user","content":"current"}),
        ];
        super::insert_codex_multi_agent_mode(&mut input, &plan);
        let mode_positions = input
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.pointer("/content/0/text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.starts_with("<multi_agent_mode>"))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(mode_positions, vec![0]);
        assert_eq!(
            input[3].get("content").and_then(serde_json::Value::as_str),
            Some("current")
        );
    }

    /// Prefix-cache stability: as the conversation grows across user turns,
    /// the already-sent items must remain a byte-identical prefix of the next
    /// request's input. The old before-last-user placement moved the mode
    /// item every turn, invalidating the cached prefix at the prior user
    /// message and re-billing the entire previous agentic turn uncached.
    #[test]
    fn mode_context_placement_preserves_prompt_prefix_across_turns() {
        let plan = plan(ReasoningLevel::Ultra, CodexRequestRole::Foreground);
        let turn1 = vec![
            serde_json::json!({"role":"user","content":"first"}),
            serde_json::json!({"role":"assistant","content":"answer"}),
        ];
        let mut turn2 = turn1.clone();
        turn2.push(serde_json::json!({"role":"user","content":"follow-up"}));
        let mut input1 = turn1;
        let mut input2 = turn2;
        super::insert_codex_multi_agent_mode(&mut input1, &plan);
        super::insert_codex_multi_agent_mode(&mut input2, &plan);
        assert_eq!(
            &input2[..input1.len()],
            &input1[..],
            "turn N's input must be a strict prefix of turn N+1's input"
        );
    }

    #[test]
    fn prompt_cache_key_is_stable_for_the_same_conversation_head() {
        let first = serde_json::json!({"role":"user","content":"hello"});
        let a = super::codex_prompt_cache_key("sys", Some(&first));
        let b = super::codex_prompt_cache_key("sys", Some(&first));
        assert_eq!(a, b);
        assert!(a.starts_with("synaps-"), "{a}");
    }

    #[test]
    fn prompt_cache_key_differs_across_conversations() {
        let first = serde_json::json!({"role":"user","content":"hello"});
        let other = serde_json::json!({"role":"user","content":"different task"});
        assert_ne!(
            super::codex_prompt_cache_key("sys", Some(&first)),
            super::codex_prompt_cache_key("sys", Some(&other)),
        );
        assert_ne!(
            super::codex_prompt_cache_key("sys", Some(&first)),
            super::codex_prompt_cache_key("other-sys", Some(&first)),
        );
    }

    #[test]
    fn body_carries_prompt_cache_key() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::Medium,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            None,
            None,
        );
        assert_eq!(
            body.get("prompt_cache_key")
                .and_then(serde_json::Value::as_str),
            Some("synaps-test-cache-key")
        );
    }

    #[test]
    fn worker_ultra_has_no_mode_context_item() {
        let plan = plan(ReasoningLevel::Ultra, CodexRequestRole::Worker);
        assert!(super::codex_multi_agent_mode_item(&plan).is_none());
    }

    // ── Pure body construction tests (zero network) ──────────────────────────

    #[test]
    fn body_lowers_ultra_reasoning_effort_to_max() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::Ultra,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            None,
            None,
        );
        let effort = body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(serde_json::Value::as_str)
            .expect("reasoning.effort must be present for Ultra");
        assert_eq!(effort, "max");
    }

    #[test]
    fn body_contains_reasoning_effort_for_max() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::Max,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            None,
            None,
        );
        let effort = body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(serde_json::Value::as_str)
            .expect("reasoning.effort must be present for Max");
        assert_eq!(effort, "max");
        // max must NOT be xhigh (critical invariant)
        assert_ne!(effort, "xhigh");
    }

    #[test]
    fn body_contains_reasoning_effort_for_xhigh() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::XHigh,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            None,
            None,
        );
        let effort = body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(serde_json::Value::as_str)
            .expect("reasoning.effort must be present for XHigh");
        assert_eq!(effort, "xhigh");
    }

    #[test]
    fn body_contains_reasoning_effort_for_low_medium_high() {
        for (level, expected) in [
            (ReasoningLevel::Low, "low"),
            (ReasoningLevel::Medium, "medium"),
            (ReasoningLevel::High, "high"),
        ] {
            let body = body_for_level(
                "gpt-5.6-sol",
                level,
                serde_json::json!([]),
                "sys".to_string(),
                vec![],
                None,
                None,
            );
            let effort = body
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{level:?} must emit reasoning.effort"));
            assert_eq!(effort, expected, "{level:?}");
        }
    }

    #[test]
    fn body_always_sets_model_and_stream_and_include() {
        let body = body_for_level(
            "gpt-5.5",
            ReasoningLevel::XHigh,
            serde_json::json!([]),
            "instructions".to_string(),
            vec![],
            None,
            None,
        );
        assert_eq!(
            body.get("model").and_then(serde_json::Value::as_str),
            Some("gpt-5.5")
        );
        assert_eq!(
            body.get("stream").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let includes = body.get("include").expect("include must be present");
        assert!(
            includes.as_array().map_or(false, |a| {
                a.iter()
                    .any(|v| v.as_str() == Some("reasoning.encrypted_content"))
            }),
            "include must contain reasoning.encrypted_content"
        );
        assert_eq!(
            body.get("store").and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn body_omits_temperature_and_max_tokens_when_none() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::Medium,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            None,
            None,
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn body_includes_temperature_and_max_tokens_when_some() {
        let body = body_for_level(
            "gpt-5.6-sol",
            ReasoningLevel::High,
            serde_json::json!([]),
            "sys".to_string(),
            vec![],
            Some(0.7),
            Some(2048),
        );
        let temp = body
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .unwrap();
        assert!((temp - 0.7f64).abs() < 0.01, "temperature={temp}");
        assert_eq!(
            body.get("max_output_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(2048)
        );
    }
}

#[cfg(test)]
mod send_retry_tests {
    //! Regression tests for transient-failure retry on the codex send path
    //! (incident: session 20260714-025948-3dab — a single transport failure
    //! against chatgpt.com aborted a whole autonomous turn because the codex
    //! route was single-shot while the Anthropic route retried).
    //!
    //! Mirrors the `on401_tests` mock-server pattern in `runtime/api.rs`.
    //! Credentials cross the broker boundary: a stub broker vends a
    //! JWT-shaped token carrying the ChatGPT account-id claim.

    use super::*;
    use agent_core::auth::{
        AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse,
    };
    use agent_core::reasoning::ReasoningLevel;
    use async_trait::async_trait;
    use axum::{http::StatusCode, response::IntoResponse, routing::post as axum_post, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::mpsc;

    /// Minimal Responses-API SSE success body: one text delta + completed.
    const CODEX_SSE_SUCCESS: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: [DONE]\n\n",
    );

    /// JWT-shaped token whose payload carries the ChatGPT account-id claim
    /// `extract_codex_account_id` looks for. Signature is irrelevant — the
    /// mock server never validates it.
    fn fake_codex_token() -> String {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_test" }
        });
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload.to_string()))
    }

    /// Stub broker that vends the fake codex token. Everything else is
    /// unreachable in these tests and fails closed.
    struct TokenOnlyBroker;

    #[async_trait]
    impl crate::auth::CredentialBroker for TokenOnlyBroker {
        async fn access_token(
            &self,
            _p: agent_core::auth::OAuthProviderId,
        ) -> Result<AccessToken, BrokerError> {
            Ok(AccessToken {
                token: fake_codex_token(),
                expires: u64::MAX,
            })
        }
        async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn proxy_stream(
            &self,
            _request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    struct DenyingCountingBroker {
        access_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::auth::CredentialBroker for DenyingCountingBroker {
        async fn access_token(
            &self,
            _provider: agent_core::auth::OAuthProviderId,
        ) -> Result<AccessToken, BrokerError> {
            self.access_calls.fetch_add(1, Ordering::SeqCst);
            Err(BrokerError::Denied(
                "credential access must not occur".into(),
            ))
        }

        async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }

        async fn proxy_stream(
            &self,
            _request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }

        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }

        async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    /// Spawn a mock Codex endpoint. First `fail_count` POSTs → `fail_status`;
    /// subsequent → SSE success. Returns (base_url, call_counter).
    async fn spawn_mock_codex(
        fail_count: usize,
        fail_status: StatusCode,
    ) -> (String, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let app = Router::new().route(
            "/codex/responses",
            axum_post(move || {
                let counter = Arc::clone(&counter_clone);
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n < fail_count {
                        (
                            fail_status,
                            [("content-type", "application/json")],
                            "{\"error\":{\"message\":\"transient upstream sadness\"}}".to_string(),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            CODEX_SSE_SUCCESS.to_string(),
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

    async fn spawn_codex_sse(body: &'static str) -> String {
        let app = Router::new().route(
            "/codex/responses",
            axum_post(move || async move {
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn run_codex(
        base_url: &str,
        max_retries: u32,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let cfg = ProviderConfig {
            base_url: base_url.to_string(),
            // The incident model — validated against the static catalog ladder.
            model: "gpt-5.6-sol".to_string(),
            provider: "openai-codex".to_string(),
        };
        let client = reqwest::Client::new();
        let broker: std::sync::Arc<dyn crate::auth::CredentialBroker> =
            std::sync::Arc::new(TokenOnlyBroker);
        let (tx, _rx) = mpsc::unbounded_channel();
        call_codex_stream_inner(
            &cfg,
            &client,
            &broker,
            &[],
            &Some("test".to_string()),
            &[],
            &tx,
            None,
            None,
            agent_core::reasoning::ReasoningLevel::Medium,
            crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            &tokio_util::sync::CancellationToken::new(),
            max_retries,
            &crate::runtime::trace::TraceContext::disabled(),
        )
        .await
    }

    #[tokio::test]
    async fn direct_ultra_tool_guard_precedes_broker_access() {
        let access_calls = Arc::new(AtomicUsize::new(0));
        let broker: Arc<dyn crate::auth::CredentialBroker> = Arc::new(DenyingCountingBroker {
            access_calls: Arc::clone(&access_calls),
        });
        let cfg = ProviderConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            model: "gpt-5.6-sol".to_string(),
            provider: "openai-codex".to_string(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();

        let error = call_codex_stream_inner(
            &cfg,
            &reqwest::Client::new(),
            &broker,
            &[],
            &Some("test".to_string()),
            &[],
            &tx,
            None,
            None,
            ReasoningLevel::Ultra,
            crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            &tokio_util::sync::CancellationToken::new(),
            0,
            &crate::runtime::trace::TraceContext::disabled(),
        )
        .await
        .expect_err("foreground Ultra without delegation tools must fail closed");

        assert!(error.to_string().contains("subagent_start"), "{error}");
        assert_eq!(
            access_calls.load(Ordering::SeqCst),
            0,
            "preflight denial must happen before broker credential access"
        );
    }

    #[tokio::test]
    async fn direct_codex_path_rejects_provider_identity_before_broker_access() {
        let access_calls = Arc::new(AtomicUsize::new(0));
        let broker: Arc<dyn crate::auth::CredentialBroker> = Arc::new(DenyingCountingBroker {
            access_calls: Arc::clone(&access_calls),
        });
        let cfg = ProviderConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            model: "gpt-5.6-sol".to_string(),
            provider: "openrouter".to_string(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();

        let error = call_codex_stream_inner(
            &cfg,
            &reqwest::Client::new(),
            &broker,
            &[],
            &Some("test".to_string()),
            &[],
            &tx,
            None,
            None,
            ReasoningLevel::Medium,
            crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            &tokio_util::sync::CancellationToken::new(),
            0,
            &crate::runtime::trace::TraceContext::disabled(),
        )
        .await
        .expect_err("the Codex seam must validate provider-qualified identity");

        assert!(error.to_string().contains("openrouter"), "{error}");
        assert_eq!(
            access_calls.load(Ordering::SeqCst),
            0,
            "provider identity denial must precede broker credential access"
        );
    }

    #[tokio::test]
    async fn codex_retries_transient_500_then_succeeds() {
        let (base_url, counter) = spawn_mock_codex(1, StatusCode::INTERNAL_SERVER_ERROR).await;
        let result = run_codex(&base_url, 2)
            .await
            .expect("one transient 500 with retries available must not abort the turn");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "expected exactly one retry"
        );
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert_eq!(text, "hello");
    }

    #[tokio::test]
    async fn codex_response_failed_surfaces_typed_provider_error_not_empty_response() {
        const BODY: &str = concat!(
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"message\":\"ECHOED:private prompt\"}}}\n\n",
            "data: [DONE]\n\n",
        );
        let base_url = spawn_codex_sse(BODY).await;
        let err = run_codex(&base_url, 0).await.expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.starts_with("Codex response failed in stream."), "{msg}");
        assert!(!msg.contains("empty response"), "{msg}");
        assert!(!msg.contains("ECHOED"), "provider text leaked: {msg}");
        assert!(
            !msg.contains("private prompt"),
            "provider text leaked: {msg}"
        );
    }

    #[tokio::test]
    async fn codex_response_incomplete_surfaces_typed_provider_error_not_empty_response() {
        const BODY: &str = concat!(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
            "data: [DONE]\n\n",
        );
        let base_url = spawn_codex_sse(BODY).await;
        let err = run_codex(&base_url, 0).await.expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.starts_with("Codex response was incomplete."), "{msg}");
        assert!(!msg.contains("empty response"), "{msg}");
        assert!(
            !msg.contains("max_output_tokens"),
            "provider text leaked: {msg}"
        );
    }

    #[tokio::test]
    async fn codex_clean_eof_without_terminal_fails_closed() {
        let base_url = spawn_codex_sse(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_123\"}}\n\n",
        )
        .await;
        let err = run_codex(&base_url, 0).await.expect_err("must fail");
        assert!(
            err.to_string()
                .starts_with("Codex response stream ended without a terminal event."),
            "{err}"
        );
    }

    #[tokio::test]
    async fn codex_retries_429_then_succeeds() {
        let (base_url, counter) = spawn_mock_codex(1, StatusCode::TOO_MANY_REQUESTS).await;
        run_codex(&base_url, 2)
            .await
            .expect("429 is transient — must retry");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codex_zero_retries_fails_fast_with_status_error() {
        let (base_url, counter) =
            spawn_mock_codex(usize::MAX, StatusCode::INTERNAL_SERVER_ERROR).await;
        let err = run_codex(&base_url, 0).await.expect_err("must fail");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "no retries budgeted");
        let msg = err.to_string();
        assert!(
            msg.starts_with("codex request failed: 500"),
            "status must survive for classification: {msg}"
        );
    }

    #[tokio::test]
    async fn codex_does_not_retry_client_errors() {
        let (base_url, counter) = spawn_mock_codex(usize::MAX, StatusCode::BAD_REQUEST).await;
        let err = run_codex(&base_url, 3).await.expect_err("must fail");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "400 is deterministic — retrying it never helps"
        );
        assert!(err.to_string().starts_with("codex request failed: 400"));
    }

    #[tokio::test]
    async fn codex_exhausted_retries_reports_last_status() {
        let (base_url, counter) =
            spawn_mock_codex(usize::MAX, StatusCode::SERVICE_UNAVAILABLE).await;
        let err = run_codex(&base_url, 1).await.expect_err("must fail");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "initial attempt + 1 retry"
        );
        assert!(err.to_string().starts_with("codex request failed: 503"));
    }

    /// The dispatch seam lifts the generic three-attempt budget to the
    /// persistent posture adopted for Anthropic OAuth overloads (10 retries)
    /// — incident: 2026-07-16 chatgpt.com 503/520/timeout bursts.
    #[test]
    fn codex_retry_budget_lifts_configured_default_to_persistent_floor() {
        assert_eq!(codex_retry_budget(3), CODEX_PERSISTENT_RETRIES);
        assert_eq!(codex_retry_budget(0), CODEX_PERSISTENT_RETRIES);
        assert_eq!(codex_retry_budget(10), 10);
    }

    /// A user who explicitly configured an even larger budget keeps it —
    /// the floor only ever raises, never lowers.
    #[test]
    fn codex_retry_budget_honors_larger_user_configuration() {
        assert_eq!(codex_retry_budget(15), 15);
    }

    /// Backoff doubles per attempt but caps at 2^6 — a 10-deep budget must
    /// never sleep 512s on its final attempt.
    #[test]
    fn retry_delay_is_exponential_with_a_capped_tail() {
        assert_eq!(retry_delay(1).as_secs(), 1);
        assert_eq!(retry_delay(2).as_secs(), 2);
        assert_eq!(retry_delay(4).as_secs(), 8);
        assert_eq!(retry_delay(7).as_secs(), 64);
        assert_eq!(retry_delay(10).as_secs(), 64);
    }

    // ─── Phase 1 privacy: hostile provider echoes the request ───────────────
    // spec §5.1: provider error bodies are untrusted and may echo the full
    // request (prompts, system text, tool schemas, credentials). Neither the
    // surfaced error nor ANY log line may contain response-body content —
    // status + provider label only.

    /// Unique raw-content sentinel placed in the outgoing request body.
    const ECHO_SENTINEL: &str = "PH1-OAI-SENTINEL-2b8c4d-RAW-CONTENT";

    /// `std::io::Write` sink capturing formatted tracing output in-process.
    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_subscriber(
        buf: &Arc<std::sync::Mutex<Vec<u8>>>,
    ) -> impl tracing::Subscriber + Send + Sync {
        let sink = buf.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || CaptureWriter(sink.clone()))
            .with_ansi(false)
            .finish()
    }

    /// Hostile loopback provider: every request is answered with `status` and
    /// a JSON error envelope whose `message` is `"ECHOED:" + the full request
    /// body` — the preserved holdout probe shape, aimed at the OpenAI routes.
    async fn spawn_hostile_echo_provider(status: StatusCode) -> String {
        let app = Router::new().fallback(move |body: String| async move {
            let envelope = serde_json::json!({
                "error": { "message": format!("ECHOED:{body}") }
            });
            (
                status,
                [("content-type", "application/json")],
                envelope.to_string(),
            )
                .into_response()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Request body shaped like a real provider request: prompt sentinel plus
    /// a distinctive tool schema. If the provider echo survives anywhere,
    /// these markers betray it.
    fn sentinel_request_body() -> Value {
        json!({
            "model": "gpt-test",
            "messages": [
                {"role": "system", "content": format!("system secret {ECHO_SENTINEL}")},
                {"role": "user", "content": ECHO_SENTINEL},
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "ph1_secret_tool_zz",
                    "description": "internal tool schema",
                    "parameters": {"type": "object", "properties": {}}
                }
            }],
        })
    }

    /// Markers that must never surface in errors or logs.
    const BANNED_MARKERS: &[&str] = &[
        "ECHOED",
        ECHO_SENTINEL,
        "ph1_secret_tool_zz",
        "\"messages\"",
        "system secret",
    ];

    fn assert_no_banned(haystack: &str, context: &str) {
        for banned in BANNED_MARKERS {
            assert!(
                !haystack.contains(banned),
                "provider body content `{banned}` leaked into {context}: {haystack}"
            );
        }
    }

    #[tokio::test]
    async fn send_with_retries_non_retryable_error_never_surfaces_provider_body() {
        let url = spawn_hostile_echo_provider(StatusCode::BAD_REQUEST).await;
        let client = reqwest::Client::new();
        let body = sentinel_request_body();

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _guard = tracing::subscriber::set_default(capture_subscriber(&buf));

        let err = send_with_retries(
            "codex",
            &url,
            || client.post(&url).json(&body),
            &tokio_util::sync::CancellationToken::new(),
            3,
            &mut tr::StreamAttempt::new(None),
        )
        .await
        .expect_err("400 must fail fast");

        let msg = err.to_string();
        assert!(
            msg.starts_with("codex request failed: 400"),
            "status + label must survive for classification/usability: {msg}"
        );
        assert_no_banned(&msg, "the surfaced error");
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_no_banned(&logs, "tracing output");
    }

    #[tokio::test(start_paused = true)]
    async fn send_with_retries_retry_logs_never_contain_provider_body() {
        let url = spawn_hostile_echo_provider(StatusCode::SERVICE_UNAVAILABLE).await;
        let client = reqwest::Client::new();
        let body = sentinel_request_body();

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _guard = tracing::subscriber::set_default(capture_subscriber(&buf));

        let err = send_with_retries(
            "codex",
            &url,
            || client.post(&url).json(&body),
            &tokio_util::sync::CancellationToken::new(),
            1,
            &mut tr::StreamAttempt::new(None),
        )
        .await
        .expect_err("persistent 503 must exhaust the budget");

        let msg = err.to_string();
        assert!(
            msg.starts_with("codex request failed: 503"),
            "status + label must survive after exhaustion: {msg}"
        );
        assert_no_banned(&msg, "the surfaced error");

        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("retry 1/1"),
            "retry accounting must stay observable: {logs}"
        );
        assert!(
            logs.contains("503"),
            "retry log must keep the status for diagnosis: {logs}"
        );
        assert_no_banned(&logs, "retry tracing output");
    }
}
