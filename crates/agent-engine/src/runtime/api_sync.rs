//! Non-streaming Anthropic API calls (`call_api`, `call_api_simple`).
//!
//! Extracted from `api.rs`. The streaming path lives in `api.rs`; this file
//! holds the synchronous variants used by `Runtime::run_single` and
//! `Runtime::compact_call`. All methods are additional `impl ApiMethods`
//! blocks — the struct itself is defined in `api.rs`.

use super::api::{ApiMethods, ApiOptions};
use super::helpers::HelperMethods;
use super::net_retry::{warn_transport, TransportDiag};
use super::types::AuthState;
use crate::{Result, RuntimeError, ToolRegistry};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Provider-reported usage from a non-streaming response body. Only fields
/// the provider actually sent become `Some` — nothing is zero-filled.
fn sync_usage_meta(json: &Value) -> Option<crate::runtime::trace::UsageMeta> {
    let usage = json.get("usage")?.as_object()?;
    let get = |key: &str| usage.get(key).and_then(Value::as_u64);
    Some(crate::runtime::trace::UsageMeta {
        provenance: crate::runtime::trace::UsageProvenance::ProviderReported,
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_write_tokens: get("cache_creation_input_tokens"),
    })
}

/// Assemble the compaction request's `system` blocks together with the
/// [`TranslationReport`](crate::runtime::transport::report::TranslationReport)
/// that describes the assembly decision — both are produced in the same
/// branch so the report can never drift from the wire.
///
/// OAuth transport prepends the same three synthesized segments as every
/// other Anthropic OAuth request — the fixed Claude Code transport identity
/// FIRST, then the product identity and the assistant guidance — ahead of
/// the caller's compaction prompt (reported as `Synthesized`/`SystemSegment`
/// with symbolic `system.synthetic[N]` IDs). API-key transport sends the
/// prompt alone (lossless).
///
/// The transport identity is not optional here: before it was added, an
/// OAuth compaction request led with the configurable product identity and
/// Anthropic refused every one with HTTP 429 `rate_limit_error` (while the
/// session's normal requests, which lead with it, kept succeeding). Unlike
/// the streaming path, compaction sends no `cache_control` markers.
fn compaction_system_blocks(
    auth_type: &str,
    system_prompt: &str,
) -> (Value, crate::runtime::transport::report::TranslationReport) {
    let mut report = crate::runtime::transport::report::TranslationReport::lossless();
    let blocks = if auth_type == "oauth" {
        let identity = crate::core::config::get_identity();
        let synthesized: [&str; 3] = [
            super::helpers::CLAUDE_CODE_TRANSPORT_IDENTITY,
            identity.as_str(),
            super::helpers::OAUTH_ASSISTANT_GUIDANCE,
        ];
        let mut blocks = Vec::with_capacity(synthesized.len() + 1);
        for (i, text) in synthesized.into_iter().enumerate() {
            blocks.push(json!({"type": "text", "text": text}));
            report.push(
                crate::runtime::trace::TranslationAction::Synthesized,
                crate::runtime::trace::TranslationElement::SystemSegment,
                Some(crate::runtime::transport::report::synthetic_system_id(i)),
            );
        }
        blocks.push(json!({"type": "text", "text": system_prompt}));
        blocks
    } else {
        vec![json!({"type": "text", "text": system_prompt})]
    };
    (Value::Array(blocks), report)
}

impl ApiMethods {
    /// Concatenate the `text` fields of every block in an Anthropic-shaped
    /// response `content` array. Returns the empty string if the value is
    /// not an array or contains no text blocks.
    pub(super) fn concat_response_text(response: &Value) -> String {
        response["content"]
            .as_array()
            .map(|content| {
                content
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_api(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        max_retries: u32,
        options: &ApiOptions,
    ) -> Result<Value> {
        super::attachments::validate_history_for_request(model, messages).map_err(RuntimeError::Config)?;
        // Host `_synaps_*` keys: routed transports strip at their entry; the
        // Anthropic body below drops them while serializing (`WireMessages`).
        // Route through OpenAI-compat provider if model resolves to one.
        // This synchronous facade consumes only the final response value, so
        // high-volume display deltas are suppressed at the provider boundary.
        let tools_schema = tools.tools_schema();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let diagnostic_context = crate::runtime::openai::terminal_diagnostics::Context {
            session: options.diagnostic_session_id.clone(),
            window: options.diagnostic_context_window,
        };
        if let Some(result) = crate::runtime::openai::terminal_diagnostics::CONTEXT
            .scope(
                diagnostic_context,
                crate::runtime::openai::try_route_with_memory_backend(
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
                    &tokio_util::sync::CancellationToken::new(),
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
                    // Only the legacy `run_single` loop sends this; it books no
                    // child spend (workers drive the streaming path).
                    None,
                ),
            )
            .await
        {
            drop(tx);
            let _ = drain.await;
            return result
                .map_err(|e| crate::runtime::openai::net::provider_error_to_runtime_for(model, e));
        }
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

        // Read auth state
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No Anthropic credentials. Run `synaps login`, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`.".to_string()
            ));
        }
        // Resolve `cache_ttl = auto` against this request's credential kind
        // (1h OAuth, 5m API key); explicit values pass through unchanged.
        let cache_ttl = options.cache_ttl.resolve_for_auth_type(&auth_type);

        let auth_header = if auth_type == "oauth" {
            (
                "authorization".to_string(),
                format!("Bearer {}", auth_token),
            )
        } else {
            ("x-api-key".to_string(), auth_token.clone())
        };

        // Avoid modifying past messages to maintain a 100% stable prefix for Anthropic caching.
        // Arc-shared (#128): to_vec() is N refcount bumps, not a deep copy;
        // sanitize/annotate clone-on-write only what they edit.
        let mut cleaned_messages = messages.to_vec();
        let retention =
            super::image_retention::plan_request(messages, super::image_retention::Budget::Native)
                .map_err(RuntimeError::Config)?;
        let retired_count = retention.retired;
        // Stable per-image projection precedes cache markers, only on Anthropic.
        // Original preflight above remains authoritative; recheck output budgets.
        super::image_limits::enforce_image_limits(&mut cleaned_messages, retired_count)
            .await
            .map_err(RuntimeError::Config)?;
        super::attachments::validate_messages(model, &cleaned_messages)
            .map_err(RuntimeError::Config)?;
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
        // The sync transport sends the full registry and never defers, so any
        // `tool_reference` in the history is stripped (it would be a 400).
        HelperMethods::wire_tool_references(&mut cleaned_messages, &|_| None);
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, cache_ttl);

        // Body assembly (#128 Slice 4): borrow the history instead of the old
        // `json!` deep rebuild. Sync transport → no "stream" key. Byte-identity
        // enforced by `runtime::body_golden` (sync_no_stream_1h fixture).
        let tools_schema = tools.tools_schema();
        let parts = crate::runtime::transport::anthropic::build_anthropic_request(
            model,
            &cleaned_messages,
            &tools_schema,
            &[],
            system_prompt,
            &auth_type,
            thinking_budget,
            reasoning_level,
            Some(&execution_plan),
            cache_ttl,
            false,
            options.effective_context_window(model),
        );
        let mut body = parts.body;
        body.set_thinking_display(options.thinking_display);
        // A model that rejects input + `max_tokens` over the window gets the
        // room the input leaves (no-op on Claude 4.5 and later).
        body.fit_max_tokens_to_window(Self::provider_window(options, model));
        let translation_report = parts.report;

        // Serialize once up-front; each retry attempt reuses the same bytes
        // via a cheap `Bytes::clone()` (refcount bump, no copy).
        let body_bytes: bytes::Bytes = serde_json::to_vec(&body)
            .map_err(|e| {
                RuntimeError::ApiStatus(format!("failed to serialize request body: {}", e))
            })?
            .into();

        // Same base-URL resolution as the streaming path (test seam + env
        // override); production passes `None` with the env unset -> the
        // canonical endpoint, unchanged.
        let anthropic_base = options
            .anthropic_base_url
            .clone()
            .or_else(|| std::env::var("SYNAPS_ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        let anthropic_url = format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));

        // ═══ TRACE (Task 8): shared construction with the streaming path ═══
        let mut tracer = super::api::begin_anthropic_tracer(
            options,
            cache_ttl,
            &anthropic_url,
            model,
            &body_bytes,
            &cleaned_messages,
            system_prompt.as_deref(),
            &tools_schema,
            body.has_tool_marker(),
            body.has_system_marker(),
            &translation_report,
        )
        .await;
        #[allow(unused_assignments)]
        let mut clock = crate::runtime::trace::AttemptClock::start();
        // `body_bytes` stays the JSON the trace digests; `wire` is what
        // reqwest uploads (see `request_encoding`).
        let mut wire = options
            .request_compression
            .encode(&anthropic_url, &body_bytes)
            .await;

        // Retry loop for transient API errors (429, 529, 500, 502, 503).
        // 429 gets a higher budget (MAX_429_RETRIES) and honours rate-limit
        // headers for delay; other errors use the user-configured max_retries.
        const MAX_429_RETRIES: u32 = 8;
        let json: Value = {
            let mut last_err = String::new();
            let mut last_reset_hint: Option<String> = None;
            #[allow(unused_assignments)]
            let mut result_json = None;
            let mut non_429_attempts: u32 = 0;
            let mut attempt: u32 = 0;

            loop {
                clock = crate::runtime::trace::AttemptClock::start();
                let mut req = client
                    .post(&anthropic_url)
                    .header(auth_header.0.clone(), auth_header.1.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                // One builder, two transports — the auth-aware beta gating
                // (incl. extended-cache-ttl) must not diverge between the
                // streaming and sync paths (spec §3.4 required refactor).
                if let Some(beta) = Self::build_beta_header(&auth_type, options, model) {
                    req = req.header("anthropic-beta", beta);
                }

                match wire.encoding_header(req).body(wire.bytes.clone()).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        options
                            .request_compression
                            .settle(&mut wire, status.is_success());
                        if status.is_success() {
                            clock.mark_headers();
                            let trace_rid =
                                super::api::provider_request_id_from_headers(resp.headers());
                            match resp.json::<Value>().await {
                                Ok(j) => {
                                    if j["error"].is_object() {
                                        // SECURITY (spec §5.1): the error envelope is
                                        // untrusted and can echo the request — never
                                        // print/log it. Surface only a vetted static
                                        // class label.
                                        if let Some(error_type) = j["error"]["type"].as_str() {
                                            let vetted =
                                                crate::core::error::sanitize_error_type(error_type)
                                                    .unwrap_or("unrecognized_error");
                                            tracing::warn!(
                                                error_type = vetted,
                                                "API returned an error envelope in a 200 response \
                                                 (details withheld — untrusted)"
                                            );
                                            if let Some(t) = tracer.take() {
                                                clock.mark_stream_end();
                                                let terminal = t.failed_terminal(vetted);
                                                t.finish(
                                                    clock,
                                                    Some(200),
                                                    trace_rid,
                                                    None,
                                                    None,
                                                    terminal,
                                                );
                                            }
                                            return Err(RuntimeError::Tool(format!(
                                                "API Error: {}",
                                                vetted
                                            )));
                                        }
                                    }
                                    if let Some(t) = tracer.take() {
                                        clock.mark_stream_end();
                                        let stop = j["stop_reason"].as_str().map(
                                            crate::runtime::trace::anthropic::stop_reason_from_wire,
                                        );
                                        t.finish(
                                            clock,
                                            Some(200),
                                            trace_rid,
                                            stop,
                                            sync_usage_meta(&j),
                                            agent_core::TurnOutcome::Completed,
                                        );
                                    }
                                    result_json = Some(j);
                                    break;
                                }
                                Err(e) => {
                                    non_429_attempts += 1;
                                    if non_429_attempts > max_retries {
                                        if let Some(t) = tracer.take() {
                                            let terminal = t.failed_terminal("response_decode");
                                            t.finish(
                                                clock,
                                                Some(200),
                                                trace_rid,
                                                None,
                                                None,
                                                terminal,
                                            );
                                        }
                                        return Err(RuntimeError::Api(e));
                                    }
                                    if let Some(t) = tracer.as_mut() {
                                        let delay =
                                            super::telemetry::network_retry_delay(non_429_attempts);
                                        t.attempt_failed(
                                            clock,
                                            crate::runtime::trace::RetryClass::Other,
                                            delay,
                                            Some(200),
                                            trace_rid,
                                            "response_decode",
                                        );
                                    }
                                    last_err = e.to_string();
                                    let delay =
                                        super::telemetry::network_retry_delay(non_429_attempts);
                                    warn_transport!(
                                        TransportDiag::from_reqwest(&e, "response_decode"),
                                        non_429_attempts,
                                        max_retries,
                                        "API retry {}/{} after {:?}: {}",
                                        non_429_attempts,
                                        max_retries,
                                        delay,
                                        last_err
                                    );
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        } else {
                            let is_429 = status.as_u16() == 429;
                            let is_retryable =
                                matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                            let (delay, from_hdr) = super::telemetry::retry_delay_from_headers(
                                resp.headers(),
                                attempt + 1,
                            );
                            let reset_hint = if from_hdr {
                                Some(format!("{}s", delay.as_secs()))
                            } else {
                                None
                            };
                            clock.mark_headers();
                            let trace_rid =
                                super::api::provider_request_id_from_headers(resp.headers());
                            let error_text = resp.text().await.unwrap_or_default();

                            // gzip refusal fail-soft: resend once uncompressed,
                            // costing no retry budget (see `request_encoding`).
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
                                        std::time::Duration::ZERO,
                                        Some(status.as_u16()),
                                        trace_rid,
                                        "gzip_refused",
                                    );
                                }
                                continue;
                            }

                            let retry_exhausted = if is_429 {
                                attempt >= MAX_429_RETRIES
                            } else {
                                non_429_attempts >= max_retries
                            };

                            if !is_retryable || retry_exhausted {
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
                                let hint = reset_hint.as_deref().or(last_reset_hint.as_deref());
                                return Err(RuntimeError::Tool(super::api::terminal_http_error(
                                    status.as_u16(),
                                    &error_text,
                                    hint,
                                    log_rid.as_ref(),
                                )));
                            }

                            last_reset_hint = reset_hint.clone();
                            // SECURITY (spec §5.1): `error_text` is untrusted and can
                            // echo the request — retain only the status.
                            last_err = format!("HTTP {}", status.as_u16());
                            if !is_429 {
                                non_429_attempts += 1;
                            }

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
                            tracing::warn!("API retry after {:?}: {}", delay, notice);
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
                            tokio::time::sleep(delay).await;
                        }
                    }
                    Err(e) => {
                        options.request_compression.settle(&mut wire, false);
                        non_429_attempts += 1;
                        let diag = TransportDiag::from_reqwest(&e, "transport");
                        if non_429_attempts > max_retries {
                            if let Some(t) = tracer.take() {
                                let terminal = t.failed_terminal("network");
                                t.finish(clock, None, None, None, None, terminal);
                            }
                            warn_transport!(
                                diag,
                                non_429_attempts,
                                max_retries,
                                error = %crate::core::error::humanize_network_error(&e),
                                "API network retries exhausted"
                            );
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                        let delay = super::telemetry::network_retry_delay(non_429_attempts);
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
                        // `last_err` carries reqwest's ` for url (…)`; the
                        // line logs the URL-free humanized text instead.
                        warn_transport!(
                            diag,
                            non_429_attempts,
                            max_retries,
                            "API retry {}/{} after {:?}: {}",
                            non_429_attempts,
                            max_retries,
                            delay,
                            crate::core::error::humanize_network_error(&e)
                        );
                        tokio::time::sleep(delay).await;
                    }
                }

                attempt += 1;
            }

            result_json.ok_or_else(|| {
                RuntimeError::Tool(format!("API failed after retries: {}", last_err))
            })?
        };

        // Log usage for cache analysis.
        // Deliberately aggregates-only: the 5m/1h split parsing and the
        // silent-downgrade detector live in the streaming path (api.rs) ONLY.
        // This sync path is near-dead (compaction/title calls); duplicating
        // the detector here would double the maintenance surface for traffic
        // that barely exists. Fail-cheap by design.
        if let Some(usage) = json.get("usage") {
            let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
            let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
        }

        Ok(json)
    }

    /// Simple non-streaming API call without tools (compaction and the
    /// rollover summary). Uses a caller-supplied system prompt (replaces the
    /// runtime's). Without an output ceiling it forces "low" effort on
    /// adaptive models — summarization doesn't benefit from heavy reasoning
    /// budgets; with one, thinking is off so the ceiling bounds all output.
    /// Retry bounds come from `policy`; [`SimpleCallPolicy::single`] makes
    /// exactly one upstream attempt on every route. The reply's usage is
    /// reported separately from its text, on every route that reports it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_api_simple(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        system_prompt: &str,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        policy: SimpleCallPolicy,
        options: &ApiOptions,
    ) -> SimpleReply {
        // Host-only message metadata never reaches any provider, whatever
        // history the caller passes in; strip before routing/serializing.
        let wire_messages = super::continuation::wire_messages(messages);
        let messages = wire_messages.as_deref().unwrap_or(messages);
        let tools_schema = Arc::new(Vec::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Display events are not needed here; routed transports return the
        // provider-reported usage, as reported, in the reply's `usage`.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let routed_system_prompt = Some(system_prompt.to_string());
        let diagnostic_context = crate::runtime::openai::terminal_diagnostics::Context {
            session: options.diagnostic_session_id.clone(),
            window: options.diagnostic_context_window,
        };
        if let Some(result) = crate::runtime::openai::terminal_diagnostics::CONTEXT
            .scope(
                diagnostic_context,
                crate::runtime::openai::try_route_with_memory_backend(
                    model,
                    client,
                    &tools_schema,
                    &routed_system_prompt,
                    messages,
                    &tx,
                    None,
                    policy.max_output_tokens,
                    thinking_budget,
                    reasoning_level,
                    &tokio_util::sync::CancellationToken::new(),
                    // The runtime's credential source and token cache, as for any
                    // conversation request: a remote broker owns these credentials.
                    &options.credential_source,
                    &options.token_cache,
                    policy.max_retries,
                    crate::runtime::openai::catalog::CodexRequestRole::Internal,
                    // Internal text-only helper: no tool session identity here, so
                    // an extension route (which cannot execute tools from this
                    // empty-schema path anyway) fails closed to a local
                    // default-core gate identity — never to ungated execution.
                    None,
                    None,
                    options.memory_backend.as_ref(),
                    options.sandbox.as_ref(),
                    &options.trace,
                    options.suppress_stream_deltas,
                    policy.attempts,
                    // A summary's spend is booked at its own settlement, never here.
                    None,
                ),
            )
            .await
        {
            drop(tx);
            let _ = drain.await;
            // Usage is settled independently of the text: a failed reply
            // that reported its usage is charged that usage.
            let usage = match &result {
                Ok(reply) => reported_usage(reply.get("usage")),
                Err(error) => failure_reported_usage(error.as_ref()),
            };
            let spend = match (usage, &result) {
                (Some(usage), _) => Spend::Reported(usage),
                (None, Ok(_)) => Spend::Unknown,
                (None, Err(error)) => routed_failure_spend(error.as_ref()),
            };
            return SimpleReply {
                text: result
                    .map(|response| Self::concat_response_text(&response))
                    .map_err(|e| {
                        crate::runtime::openai::net::provider_error_to_runtime_for(model, e)
                    }),
                spend,
                failure: None,
                stop: None,
            };
        }
        drop(tx);
        let _ = drain.await;
        let mut spend = Spend::NotDispatched;
        let mut failure = None;
        match Self::anthropic_simple(
            auth,
            client,
            model,
            system_prompt,
            thinking_budget,
            messages,
            policy,
            options,
            &mut spend,
            &mut failure,
        )
        .await
        {
            Ok((text, usage, stop)) => SimpleReply {
                text: Ok(text),
                spend: reported_usage(usage.as_ref()).map_or(Spend::Unknown, Spend::Reported),
                failure: None,
                stop,
            },
            Err(error) => SimpleReply {
                text: Err(error),
                spend,
                failure,
                stop: None,
            },
        }
    }

    /// The Anthropic Messages leg of [`Self::call_api_simple`]. An empty
    /// reply is `Ok("")`, so its usage is still returned, and so is its
    /// stop reason. On failure `spend`
    /// says what the failure is known to have cost; it starts
    /// [`Spend::NotDispatched`]. `failure` says how the last attempt failed
    /// once it had a response or lost its connection.
    #[allow(clippy::too_many_arguments)]
    async fn anthropic_simple(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        system_prompt: &str,
        thinking_budget: u32,
        messages: &[crate::SharedMessage],
        policy: SimpleCallPolicy,
        options: &ApiOptions,
        spend: &mut Spend,
        failure: &mut Option<SentFailure>,
    ) -> Result<(
        String,
        Option<Value>,
        Option<crate::runtime::trace::StopReason>,
    )> {
        let (max_retries, max_429_retries) = (policy.max_retries, policy.max_429_retries);
        let model = model.strip_prefix("anthropic/").unwrap_or(model);

        let (auth_header_name, auth_header_value, auth_type) = Self::build_auth_header(auth).await;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No API key or OAuth token found. Run `synaps login` to authenticate.".to_string(),
            ));
        }

        let Some((mut body, compaction_report)) = simple_body(
            model,
            messages,
            thinking_budget,
            policy.max_output_tokens,
            &auth_type,
            system_prompt,
            options.effective_context_window(model),
        ) else {
            // Not sent: `spend` stays NotDispatched.
            return Err(RuntimeError::Config(
                "bounded request not sent: no verified output ceiling for this model".into(),
            ));
        };
        // PARITY (upstream fit): a model that rejects input + `max_tokens`
        // over the window gets the room the input leaves (no-op on Claude
        // 4.5 and later), never raised. Measured against the window this
        // path's beta set (default options) opens; the floor keeps
        // `max_tokens > budget_tokens` where the body budgets thinking.
        let fitted = HelperMethods::fit_max_tokens_to_window(
            model,
            body["max_tokens"].as_u64().unwrap_or_default(),
            Self::provider_window(&ApiOptions::default(), model),
            body["thinking"]["budget_tokens"].as_u64(),
            || {
                super::context::conservative_token_estimate(&body["system"].to_string())
                    .saturating_add(super::context::estimate_history(messages))
            },
        );
        body["max_tokens"] = json!(fitted);

        // Serialize once up-front (exact-bytes contract, spec 6.2): the
        // same buffer is sent on every retry and is the sole source for the
        // trace wire digest. `.json(&body)` would re-serialize per attempt.
        let body_bytes: bytes::Bytes = serde_json::to_vec(&body)
            .map_err(|e| {
                RuntimeError::ApiStatus(format!("failed to serialize request body: {}", e))
            })?
            .into();

        // Base-URL resolution matches the other Anthropic transports.
        // NOTE: `options` is used for the endpoint + trace seam ONLY -- beta
        // gating below intentionally keeps the historical default-options
        // behaviour of this compaction path.
        let anthropic_base = options
            .anthropic_base_url
            .clone()
            .or_else(|| std::env::var("SYNAPS_ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        let anthropic_url = format!("{}/v1/messages", anthropic_base.trim_end_matches('/'));
        // A bounded single attempt goes out only on a client that never
        // follows redirects; only there does a refused connection prove
        // not-sent. Without one it is not sent at all (`spend` stays
        // NotDispatched), never on the caller's client.
        let bounded = policy.attempts == crate::runtime::openai::AttemptPolicy::Single;
        let bounded_client = if bounded {
            match super::bounded_http_client() {
                Some(client) => Some(client),
                None => {
                    return Err(RuntimeError::Config(
                        "bounded request not sent: no HTTP client that refuses redirects".into(),
                    ))
                }
            }
        } else {
            None
        };
        let redirect_free = bounded_client.is_some();
        let client = bounded_client.as_ref().unwrap_or(client);
        // First-party rejection provenance only holds when no redirect can
        // have taken the request to another host.
        let settlement = if redirect_free && anthropic_base == "https://api.anthropic.com" {
            SettlementRoute::AnthropicFirstParty
        } else {
            SettlementRoute::Unverified
        };

        // ═══ TRACE (Task 8): compaction requests are traced too ═══
        // Report built above alongside the system blocks (Task 9 fix): OAuth
        // carries the three synthesized identity/guidance segment entries.
        let mut tracer = super::api::begin_anthropic_tracer(
            options,
            options.cache_ttl.resolve_for_auth_type(&auth_type),
            &anthropic_url,
            model,
            &body_bytes,
            messages,
            Some(system_prompt),
            &[],
            false,
            false,
            &compaction_report,
        )
        .await;
        #[allow(unused_assignments)]
        let mut clock = crate::runtime::trace::AttemptClock::start();
        // `body_bytes` stays the JSON the trace digests; `wire` is what
        // reqwest uploads (see `request_encoding`).
        let mut wire = options
            .request_compression
            .encode(&anthropic_url, &body_bytes)
            .await;
        // A gated (rollover summary) body goes out as one chunk, published
        // only if the snapshot is still valid when the client pulls it: a
        // session that changed while the request was connecting gets no
        // body. Published once any attempt's body was published or answered.
        let published_or_answered =
            std::sync::Arc::new(super::keepwarm::Publication::default());
        // A gated body is withdrawn when this call ends or is dropped
        // unfinished (its caller timed out or gave up): the client's
        // connection task, which can still hold it, then never produces it.
        let _withdraw_on_drop = options.snapshot_gate.as_ref().map(|_| {
            super::keepwarm::WithdrawOnDrop(std::sync::Arc::clone(&published_or_answered))
        });
        #[cfg(test)]
        if let Some(seam) = options.snapshot_gate.as_ref().and_then(|gate| gate.seam()) {
            published_or_answered.install_seam(seam);
        }

        // Retry loop for transient errors (compaction path).
        // Same header-aware 429 handling as the main call_api loop.
        let json: Value = {
            let mut last_err = String::new();
            let mut last_reset_hint: Option<String> = None;
            #[allow(unused_assignments)]
            let mut result_json = None;
            let mut non_429_attempts: u32 = 0;
            let mut attempt: u32 = 0;

            loop {
                // A rollover summary whose session changed while it was
                // prepared is not sent (spend stays NotDispatched).
                if options
                    .snapshot_gate
                    .as_ref()
                    .is_some_and(|gate| !gate.valid())
                {
                    return Err(RuntimeError::Canceled);
                }
                clock = crate::runtime::trace::AttemptClock::start();
                // From here on a failure may have cost something.
                *spend = Spend::Unknown;
                let mut req = client
                    .post(&anthropic_url)
                    .header(auth_header_name.clone(), auth_header_value.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if let Some(beta) =
                    Self::build_beta_header(&auth_type, &ApiOptions::default(), model)
                {
                    req = req.header("anthropic-beta", beta);
                }

                // PARITY: ours' gated body carries upstream's encoded `wire`
                // bytes (#321), like the streaming path (api.rs).
                let req = wire.encoding_header(req);
                let req = match &options.snapshot_gate {
                    Some(gate) => super::api::gated_body(
                        req,
                        wire.bytes.clone(),
                        &published_or_answered,
                        Some(super::warm_summary::BodyGate::Snapshot(gate.clone())),
                    ),
                    None => req.body(wire.bytes.clone()),
                };
                match req.send().await {
                    Ok(resp) => {
                        published_or_answered.answered();
                        let status = resp.status();
                        options
                            .request_compression
                            .settle(&mut wire, status.is_success());
                        if status.is_success() {
                            clock.mark_headers();
                            let trace_rid =
                                super::api::provider_request_id_from_headers(resp.headers());
                            match resp.json::<Value>().await {
                                Ok(j) => {
                                    if j["error"].is_object() {
                                        // SECURITY (spec §5.1): untrusted envelope —
                                        // surface only a vetted static class label.
                                        if let Some(error_type) = j["error"]["type"].as_str() {
                                            let known =
                                                crate::core::error::sanitize_error_type(error_type);
                                            *failure = Some(SentFailure {
                                                class: FailureClass::Envelope,
                                                error_type: known,
                                            });
                                            #[cfg(test)]
                                            set_error_body_for_test(&j.to_string());
                                            let vetted = known.unwrap_or("unrecognized_error");
                                            if let Some(t) = tracer.take() {
                                                clock.mark_stream_end();
                                                let terminal = t.failed_terminal(vetted);
                                                t.finish(
                                                    clock,
                                                    Some(200),
                                                    trace_rid,
                                                    None,
                                                    None,
                                                    terminal,
                                                );
                                            }
                                            return Err(RuntimeError::Tool(format!(
                                                "API Error: {}",
                                                vetted
                                            )));
                                        }
                                    }
                                    if let Some(t) = tracer.take() {
                                        clock.mark_stream_end();
                                        let stop = j["stop_reason"].as_str().map(
                                            crate::runtime::trace::anthropic::stop_reason_from_wire,
                                        );
                                        t.finish(
                                            clock,
                                            Some(200),
                                            trace_rid,
                                            stop,
                                            sync_usage_meta(&j),
                                            agent_core::TurnOutcome::Completed,
                                        );
                                    }
                                    result_json = Some(j);
                                    break;
                                }
                                Err(e) => {
                                    non_429_attempts += 1;
                                    if non_429_attempts > max_retries {
                                        *failure = Some(SentFailure {
                                            class: FailureClass::Decode,
                                            error_type: None,
                                        });
                                        if let Some(t) = tracer.take() {
                                            let terminal = t.failed_terminal("response_decode");
                                            t.finish(
                                                clock,
                                                Some(200),
                                                trace_rid,
                                                None,
                                                None,
                                                terminal,
                                            );
                                        }
                                        return Err(RuntimeError::Api(e));
                                    }
                                    last_err = e.to_string();
                                    let delay =
                                        super::telemetry::network_retry_delay(non_429_attempts);
                                    if let Some(t) = tracer.as_mut() {
                                        t.attempt_failed(
                                            clock,
                                            crate::runtime::trace::RetryClass::Other,
                                            delay,
                                            Some(200),
                                            trace_rid,
                                            "response_decode",
                                        );
                                    }
                                    warn_transport!(
                                        TransportDiag::from_reqwest(&e, "response_decode"),
                                        non_429_attempts,
                                        max_retries,
                                        "Compaction retry {}/{} after {:?}: {}",
                                        non_429_attempts,
                                        max_retries,
                                        delay,
                                        last_err
                                    );
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        } else {
                            clock.mark_headers();
                            let trace_rid =
                                super::api::provider_request_id_from_headers(resp.headers());
                            let is_429 = status.as_u16() == 429;
                            let is_retryable =
                                matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                            let (delay, from_hdr) = super::telemetry::retry_delay_from_headers(
                                resp.headers(),
                                attempt + 1,
                            );
                            let reset_hint = if from_hdr {
                                Some(format!("{}s", delay.as_secs()))
                            } else {
                                None
                            };
                            let error_text = resp.text().await.unwrap_or_default();

                            // gzip refusal fail-soft: resend once uncompressed,
                            // costing no retry budget (see `request_encoding`).
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
                                        std::time::Duration::ZERO,
                                        Some(status.as_u16()),
                                        trace_rid,
                                        "gzip_refused",
                                    );
                                }
                                continue;
                            }

                            let retry_exhausted = if is_429 {
                                attempt >= max_429_retries
                            } else {
                                non_429_attempts >= max_retries
                            };

                            if !is_retryable || retry_exhausted {
                                *failure = Some(SentFailure {
                                    class: FailureClass::Status(status.as_u16()),
                                    error_type: vetted_error_type(&error_text),
                                });
                                #[cfg(test)]
                                set_error_body_for_test(&error_text);
                                // Only the first attempt: an earlier retried
                                // failure may have generated something.
                                *spend = failure_spend(
                                    settlement,
                                    if attempt == 0 {
                                        FailureStage::HttpError {
                                            status: status.as_u16(),
                                            error_type: anthropic_error_type(&error_text),
                                        }
                                    } else {
                                        FailureStage::AfterSend
                                    },
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
                                let hint = reset_hint.as_deref().or(last_reset_hint.as_deref());
                                return Err(RuntimeError::Tool(super::api::terminal_http_error(
                                    status.as_u16(),
                                    &error_text,
                                    hint,
                                    log_rid.as_ref(),
                                )));
                            }

                            last_reset_hint = reset_hint;
                            // SECURITY (spec §5.1): `error_text` is untrusted and can
                            // echo the request — retain only the status.
                            last_err = format!("HTTP {}", status.as_u16());
                            if !is_429 {
                                non_429_attempts += 1;
                            }

                            tracing::warn!(
                                "Compaction API retry after {:?}: {}: {}",
                                delay,
                                status,
                                last_err
                            );
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
                            tokio::time::sleep(delay).await;
                        }
                    }
                    Err(e) => {
                        options.request_compression.settle(&mut wire, false);
                        // A gated body no attempt published, withdrawn
                        // so none can be published later by the client's
                        // connection task: the provider never had a
                        // complete request. Withdrawn only where no retry
                        // follows.
                        let gated = options.snapshot_gate.is_some();
                        let withdrawn = || gated && published_or_answered.withdraw();
                        // Withheld because the session changed: not sent,
                        // and not retried.
                        if options
                            .snapshot_gate
                            .as_ref()
                            .is_some_and(|gate| !gate.valid())
                            && withdrawn()
                        {
                            *spend = failure_spend(settlement, FailureStage::NotSent);
                            if let Some(t) = tracer.take() {
                                let terminal = t.failed_terminal("withdrawn");
                                t.finish(clock, None, None, None, None, terminal);
                            }
                            return Err(RuntimeError::Canceled);
                        }
                        non_429_attempts += 1;
                        if non_429_attempts > max_retries {
                            // A refused connection on the only attempt, with
                            // no redirect followed, sent nothing, and so did
                            // a gated body never published; any other
                            // transport failure may have.
                            if (e.is_connect() && attempt == 0 && redirect_free) || withdrawn() {
                                *spend = failure_spend(settlement, FailureStage::NotSent);
                            }
                            *failure = Some(SentFailure {
                                class: FailureClass::Network,
                                error_type: None,
                            });
                            if let Some(t) = tracer.take() {
                                let terminal = t.failed_terminal("network");
                                t.finish(clock, None, None, None, None, terminal);
                            }
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                        let delay = super::telemetry::network_retry_delay(non_429_attempts);
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
                        // `last_err` carries reqwest's ` for url (…)`; the
                        // line logs the URL-free humanized text instead.
                        warn_transport!(
                            TransportDiag::from_reqwest(&e, "transport"),
                            non_429_attempts,
                            max_retries,
                            "Compaction API retry {}/{} after {:?}: {}",
                            non_429_attempts,
                            max_retries,
                            delay,
                            crate::core::error::humanize_network_error(&e)
                        );
                        tokio::time::sleep(delay).await;
                    }
                }

                attempt += 1;
            }

            result_json.ok_or_else(|| {
                RuntimeError::Tool(format!("API failed after retries: {}", last_err))
            })?
        };

        // Log usage
        if let Some(usage) = json.get("usage") {
            let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
            let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
        }

        // Extract text content from the response (skip thinking blocks)
        let mut out = String::new();
        if let Some(content) = json["content"].as_array() {
            for block in content {
                if block["type"].as_str() == Some("text") {
                    if let Some(t) = block["text"].as_str() {
                        out.push_str(t);
                    }
                }
            }
        }

        let stop = json["stop_reason"]
            .as_str()
            .map(crate::runtime::trace::anthropic::stop_reason_from_wire);
        Ok((out, json.get("usage").cloned(), stop))
    }
}

/// Retry and output limits for one [`ApiMethods::call_api_simple`] call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SimpleCallPolicy {
    pub max_retries: u32,
    pub max_429_retries: u32,
    pub attempts: crate::runtime::openai::AttemptPolicy,
    /// Provider output ceiling (`max_tokens`), including any reasoning.
    pub max_output_tokens: Option<u32>,
}

impl SimpleCallPolicy {
    /// `/compact`: the configured retries and the model's full output.
    pub(crate) fn compaction(max_retries: u32) -> Self {
        Self {
            max_retries,
            max_429_retries: MAX_429_RETRIES_COMPACT,
            attempts: crate::runtime::openai::AttemptPolicy::Configured,
            max_output_tokens: None,
        }
    }

    /// Exactly one upstream attempt on every route, with an output ceiling.
    pub(crate) fn single(max_output_tokens: u32) -> Self {
        Self {
            max_retries: 0,
            max_429_retries: 0,
            attempts: crate::runtime::openai::AttemptPolicy::Single,
            max_output_tokens: Some(max_output_tokens),
        }
    }
}

/// Outcome of one simple call: the reply text (an empty reply is `Ok("")`)
/// and, separately, what is known about its spend.
#[derive(Debug)]
pub(crate) struct SimpleReply {
    pub text: Result<String>,
    pub spend: Spend,
    /// How a request that was sent failed, when the transport knows
    /// (native Anthropic); `None` otherwise.
    pub failure: Option<SentFailure>,
    /// The stop reason the reply reported (native Anthropic); `None`
    /// otherwise. A reply that stopped with a refusal may still carry the
    /// text written before it.
    pub stop: Option<crate::runtime::trace::StopReason>,
}

/// How a sent request failed (telemetry `summary_error_status`): the HTTP
/// status, or what went wrong after a 200. Never provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// A non-success HTTP status.
    Status(u16),
    /// A 200 whose body is an error envelope.
    Envelope,
    /// A 200 whose body did not decode.
    Decode,
    /// A streamed 200 that ended in an error or broke off.
    Stream,
    /// No response: the connection failed after the request may have gone
    /// out.
    Network,
}

impl FailureClass {
    /// The telemetry value: the status number, or the class name.
    pub(crate) fn label(self) -> String {
        match self {
            Self::Status(status) => status.to_string(),
            Self::Envelope => "envelope".into(),
            Self::Decode => "decode".into(),
            Self::Stream => "stream".into(),
            Self::Network => "network".into(),
        }
    }
}

/// [`FailureClass`] plus the provider's error type, when it is one of our
/// vetted static labels (never the provider's bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentFailure {
    pub class: FailureClass,
    pub error_type: Option<&'static str>,
}

/// How a native Anthropic request with an output ceiling keeps thinking
/// inside it (thinking tokens count toward `max_tokens`). Vetted per model
/// from the provider's thinking table ("Configuring thinking" and the
/// per-model notes, platform.claude.com/docs/en/build-with-claude/thinking);
/// a model not listed has no verified ceiling and is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CeilingThinking {
    /// Thinking is off unless requested (models before 4.5): no thinking
    /// field.
    OffWhenOmitted,
    /// The documented disable: `thinking: {"type": "disabled"}` (at the
    /// default effort, which is high or below on every listed model).
    Disabled,
    /// Claude Sonnet 5.5 rejects `disabled`; `between_tools` turns
    /// up-front thinking off, and without tools the reply is text only.
    BetweenTools,
    /// Thinking can't be turned off: explicit adaptive at effort low, and
    /// the limit leaves the cap again for thinking.
    AdaptiveLow,
}

impl CeilingThinking {
    /// The vetted class of `model` (an `anthropic/` prefix, a `-YYYYMMDD`
    /// date or `-latest` suffix ignored); `None` for any model not listed.
    pub(crate) fn of(model: &str) -> Option<Self> {
        let id = model.to_ascii_lowercase();
        let id = id.strip_prefix("anthropic/").unwrap_or(&id);
        let id = id.strip_suffix("-latest").unwrap_or(id);
        let id = match id.rsplit_once('-') {
            Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
                base
            }
            _ => id,
        };
        // Thinking can't be turned off (the table's "disabled" column is a
        // 400 error, and so is every other off switch).
        const ADAPTIVE_ONLY: &[&str] = &[
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
        ];
        // `disabled` turns thinking off (Opus 5 and Haiku 5.5 at effort
        // high or below; their defaults are high and medium).
        const DISABLE: &[&str] = &[
            "claude-haiku-5-5",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ];
        // Before 4.5: extended thinking is opt-in (`enabled` with a
        // budget) or absent.
        const OFF_WHEN_OMITTED: &[&str] = &[
            "claude-opus-4-1",
            "claude-opus-4-0",
            "claude-opus-4",
            "claude-sonnet-4-0",
            "claude-sonnet-4",
            "claude-3-7-sonnet",
            "claude-3-5-sonnet",
            "claude-3-5-haiku",
            "claude-3-opus",
            "claude-3-haiku",
        ];
        if ADAPTIVE_ONLY.contains(&id) {
            Some(Self::AdaptiveLow)
        } else if id == "claude-sonnet-5-5" {
            Some(Self::BetweenTools)
        } else if DISABLE.contains(&id) {
            Some(Self::Disabled)
        } else if OFF_WHEN_OMITTED.contains(&id) {
            Some(Self::OffWhenOmitted)
        } else {
            None
        }
    }

    /// The `max_tokens` for a reply of `cap` tokens on a model whose largest
    /// output is `model_max`: the cap, or the cap plus as much again for
    /// thinking where it can't be off; never above `model_max`.
    pub(crate) fn limit(self, cap: u64, model_max: u64) -> u64 {
        let limit = match self {
            Self::AdaptiveLow => cap.saturating_add(cap),
            Self::OffWhenOmitted | Self::Disabled | Self::BetweenTools => cap,
        };
        limit.min(model_max)
    }

    /// Sets the body's `thinking` and `output_config` for this class.
    fn apply(self, body: &mut Value) {
        let Some(fields) = body.as_object_mut() else {
            return;
        };
        fields.remove("thinking");
        fields.remove("output_config");
        let thinking = match self {
            Self::OffWhenOmitted => return,
            Self::Disabled => json!({"type": "disabled"}),
            Self::BetweenTools => json!({"type": "between_tools"}),
            Self::AdaptiveLow => {
                fields.insert("output_config".into(), json!({"effort": "low"}));
                json!({"type": "adaptive"})
            }
        };
        fields.insert("thinking".into(), thinking);
    }
}


/// The Anthropic Messages body of a simple request (`/compact`, the
/// screened rollover summary) and its system translation report, without
/// the transport. `model` has no `anthropic/` prefix; `context_window` is
/// the runtime's effective window, which sizes an unbounded request's
/// `max_tokens` ([`HelperMethods::max_tokens_for_request`]). `None`: not
/// sent.
fn simple_body(
    model: &str,
    messages: &[crate::SharedMessage],
    thinking_budget: u32,
    max_output_tokens: Option<u32>,
    auth_type: &str,
    system_prompt: &str,
    context_window: u64,
) -> Option<(Value, crate::runtime::transport::report::TranslationReport)> {
    let request_max = HelperMethods::max_tokens_for_request(model, context_window);
    let mut body = json!({
        "model": model,
        "max_tokens": max_output_tokens
            .map_or(request_max, u64::from),
        "messages": messages,
        "thinking": if crate::core::models::model_supports_adaptive_thinking(model) {
            json!({ "type": "adaptive", "display": "summarized" })
        } else {
            // An explicit ceiling replaces this field below.
            let budget = HelperMethods::anthropic_thinking_budget(thinking_budget, request_max);
            json!({
                "type": "enabled",
                "budget_tokens": budget,
                "display": "summarized"
            })
        }
    });

    // Force low effort on adaptive models — compaction is a structured
    // summarization task; heavy reasoning wastes tokens.
    if crate::core::models::model_supports_adaptive_thinking(model) {
        body["output_config"] = json!({"effort": "low"});
    }
    // An explicit output ceiling bounds every output token, thinking
    // included: the model's vetted way to keep thinking inside it, or no
    // request.
    if max_output_tokens.is_some() {
        CeilingThinking::of(model)?.apply(&mut body);
    }

    // System blocks + translation report come from one helper, in one
    // branch: the report is derived from the actual assembly decision
    // (OAuth synthesizes transport identity + product identity +
    // guidance; api_key is lossless) and can never drift from the wire.
    let (system_blocks, compaction_report) =
        compaction_system_blocks(auth_type, system_prompt);
    body["system"] = system_blocks;
    Some((body, compaction_report))
}

/// Test seam (the live probe only, never built outside tests): the last
/// error body a native Anthropic simple request received, so a live probe
/// can read why a request was rejected. The product keeps only the status
/// and a vetted label.
#[cfg(test)]
pub(crate) static ERROR_BODY_FOR_TEST: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn set_error_body_for_test(body: &str) {
    *ERROR_BODY_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body.to_owned());
}

/// The vetted static label for an Anthropic error body's `error.type`.
pub(super) fn vetted_error_type(body: &str) -> Option<&'static str> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    crate::core::error::sanitize_error_type(parsed["error"]["type"].as_str()?)
}

/// What a simple call is known to have cost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Spend {
    /// Nothing reached the provider (preflight, routing or local refusal,
    /// connection refused): no cost.
    NotDispatched,
    /// A first-party endpoint positively identified the request as rejected
    /// before generation (see [`failure_spend`]): no cost.
    RejectedBeforeGeneration,
    /// Provider-reported usage, in Anthropic fields.
    Reported(Value),
    /// Dispatched, but the cost is not known (timeout, disconnect, server
    /// error, a reply without usage).
    Unknown,
}

/// Provider-reported usage (Anthropic field names) normalized for
/// settlement, or `None` when the report does not show what the call cost:
/// an absent, `null` or empty report, or one without a numeric input or
/// output counter (a missing output count is not free output). Explicit
/// zeros are a report. Missing cache counters are zero.
pub(crate) fn reported_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?.as_object()?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    Some(json!({
        "input_tokens": count("input_tokens")?,
        "output_tokens": count("output_tokens")?,
        "cache_read_input_tokens": count("cache_read_input_tokens").unwrap_or(0),
        "cache_creation_input_tokens": count("cache_creation_input_tokens").unwrap_or(0),
    }))
}

/// The endpoint a simple call reached, as far as evidence about its spend
/// goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettlementRoute {
    /// Anthropic Messages on the official `https://api.anthropic.com` host,
    /// not overridden: its documented error types say what was rejected.
    AnthropicFirstParty,
    /// Anything else: an overridden or proxied endpoint, a router, a
    /// compatible server, or a broker-routed provider. Its error statuses
    /// are not evidence that nothing was generated.
    Unverified,
}

/// How far one failed request got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureStage {
    /// Refused locally or never connected: nothing was sent.
    NotSent,
    /// The first and only attempt was sent and answered with an HTTP error
    /// status; `error_type` is the provider's error type when it parsed to
    /// one this host recognizes.
    HttpError {
        status: u16,
        error_type: Option<&'static str>,
    },
    /// Sent, then failed some other way (timeout, disconnect, decode, an
    /// earlier retried attempt): what it cost is unknown.
    AfterSend,
}

/// Anthropic error types (Messages API, non-streaming) returned while
/// validating or authorizing a request, before any generation. Rate limits
/// (`rate_limit_error`), size limits (`request_too_large`), overload and
/// server errors are excluded: they are not shown to be free.
const ANTHROPIC_PRE_GENERATION_REJECTIONS: &[(u16, &str)] = &[
    (400, "invalid_request_error"),
    (401, "authentication_error"),
    (403, "permission_error"),
];

/// The recognized Anthropic error type of an error body, if any. Only a
/// static label leaves this function; the body is untrusted.
pub(crate) fn anthropic_error_type(body: &str) -> Option<&'static str> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let error_type = parsed["error"]["type"].as_str()?;
    ANTHROPIC_PRE_GENERATION_REJECTIONS
        .iter()
        .map(|(_, t)| *t)
        .find(|t| *t == error_type)
}

/// What a failed request is known to have cost. Only a positively
/// identified pre-generation rejection from a first-party endpoint, or a
/// request that was never sent, is free; every other failure after sending
/// (any status on an unverified endpoint, 409/413/429/5xx everywhere) keeps
/// its spend unknown.
pub(crate) fn failure_spend(route: SettlementRoute, stage: FailureStage) -> Spend {
    match (route, stage) {
        (_, FailureStage::NotSent) => Spend::NotDispatched,
        (
            SettlementRoute::AnthropicFirstParty,
            FailureStage::HttpError {
                status,
                error_type: Some(error_type),
            },
        ) if ANTHROPIC_PRE_GENERATION_REJECTIONS.contains(&(status, error_type)) => {
            Spend::RejectedBeforeGeneration
        }
        _ => Spend::Unknown,
    }
}

/// Spend of a routed request that failed without reporting usage. Routed
/// transports mark a request [`NotDispatched`](crate::runtime::openai::NotDispatched)
/// when they know nothing was sent; any other routed failure (including
/// every HTTP error status through the broker) is unverified.
pub(crate) fn routed_failure_spend(error: &(dyn std::error::Error + 'static)) -> Spend {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if e.is::<crate::runtime::openai::NotDispatched>() {
            return failure_spend(SettlementRoute::Unverified, FailureStage::NotSent);
        }
        current = e.source();
    }
    failure_spend(SettlementRoute::Unverified, FailureStage::AfterSend)
}

/// Usage a failed routed request reported for its only attempt (see
/// [`FailedWithUsage`](crate::runtime::openai::FailedWithUsage)), normalized
/// like [`reported_usage`]: `None` when absent or partial.
pub(crate) fn failure_reported_usage(error: &(dyn std::error::Error + 'static)) -> Option<Value> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if let Some(failed) = e.downcast_ref::<crate::runtime::openai::FailedWithUsage>() {
            return reported_usage(Some(&failed.usage));
        }
        current = e.source();
    }
    None
}

/// Rate-limit retries on the compaction path (`/compact`).
pub(super) const MAX_429_RETRIES_COMPACT: u32 = 8;

#[cfg(test)]
mod compaction_system_blocks_tests {
    use super::*;
    use crate::runtime::trace::{TranslationAction, TranslationElement};
    use serde_json::json;

    const PROMPT_SENTINEL: &str = "COMPACTION-PROMPT-SENTINEL-4c1d";

    /// OAuth compaction synthesizes exactly three identity/guidance system
    /// segments; the report records exactly those three — action, element,
    /// and symbolic structural ID all exact.
    #[test]
    fn oauth_report_has_exactly_three_synthesized_system_entries() {
        let (_, report) = compaction_system_blocks("oauth", PROMPT_SENTINEL);
        assert_eq!(
            report.entries.len(),
            3,
            "oauth synthesizes exactly three system segments, got {:?}",
            report
        );
        for (i, entry) in report.entries.iter().enumerate() {
            assert_eq!(entry.action, TranslationAction::Synthesized);
            assert_eq!(entry.element, TranslationElement::SystemSegment);
            assert_eq!(
                entry.element_id,
                Some(crate::runtime::transport::report::synthetic_system_id(i)),
                "entry {i} must carry structural ID system.synthetic[{i}]"
            );
        }
    }

    /// API-key compaction performs no synthesis: lossless report.
    #[test]
    fn api_key_report_is_lossless() {
        let (_, report) = compaction_system_blocks("api_key", PROMPT_SENTINEL);
        assert!(
            report.is_lossless(),
            "api_key compaction must be lossless, got {:?}",
            report
        );
    }

    /// The report never carries prompt content — only positional/symbolic
    /// IDs (spec §6.3: entries never carry content or previews).
    #[test]
    fn report_serialization_contains_no_prompt_content() {
        for auth in ["oauth", "api_key"] {
            let (_, report) = compaction_system_blocks(auth, PROMPT_SENTINEL);
            let serialized = serde_json::to_string(&report).unwrap();
            assert!(
                !serialized.contains(PROMPT_SENTINEL),
                "{auth}: report leaked prompt content: {serialized}"
            );
        }
    }

    /// OAuth compaction leads with the fixed Claude Code transport identity,
    /// exactly like the streaming path. Regression: compaction led with the
    /// configurable product identity, and Anthropic refused every OAuth
    /// compaction with HTTP 429 while ordinary turns kept working.
    #[test]
    fn oauth_compaction_leads_with_transport_identity() {
        let (oauth_blocks, _) = compaction_system_blocks("oauth", PROMPT_SENTINEL);
        assert_eq!(
            oauth_blocks[0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.",
            "OAuth compaction must lead with the transport identity"
        );
        let expected = json!([
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": crate::core::config::get_identity()},
            {"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."},
            {"type": "text", "text": PROMPT_SENTINEL},
        ]);
        assert_eq!(
            serde_json::to_vec(&oauth_blocks).unwrap(),
            serde_json::to_vec(&expected).unwrap(),
            "oauth compaction system blocks: transport identity, product identity, guidance, prompt"
        );
    }

    /// Compaction and the streaming path share one OAuth identity layout: the
    /// synthesized prefix is text-identical (compaction omits cache markers).
    #[test]
    fn oauth_compaction_prefix_matches_streaming_system_blocks() {
        let (compaction, _) = compaction_system_blocks("oauth", PROMPT_SENTINEL);
        let streaming = HelperMethods::build_system_blocks(
            "oauth",
            &Some(PROMPT_SENTINEL.to_string()),
            crate::core::config::CacheTtl::FiveMinutes,
        )
        .unwrap();
        let texts = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|b| b["text"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(texts(&compaction), texts(&streaming));
        assert!(
            compaction
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b.get("cache_control").is_none()),
            "compaction sends no cache markers"
        );
    }

    /// API-key compaction is unchanged: the prompt alone, byte-identical.
    #[test]
    fn api_key_system_blocks_match_legacy_wire_bytes() {
        let (key_blocks, _) = compaction_system_blocks("api_key", PROMPT_SENTINEL);
        let legacy_key = json!([{"type": "text", "text": PROMPT_SENTINEL}]);
        assert_eq!(
            serde_json::to_vec(&key_blocks).unwrap(),
            serde_json::to_vec(&legacy_key).unwrap(),
            "api_key system blocks must be byte-identical to the legacy assembly"
        );
    }
}

#[cfg(test)]
mod concat_response_text_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_text_from_single_block() {
        let v = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(ApiMethods::concat_response_text(&v), "hello");
    }

    #[test]
    fn concatenates_multiple_text_blocks() {
        let v = json!({"content": [
            {"type": "text", "text": "alpha "},
            {"type": "text", "text": "beta"},
        ]});
        assert_eq!(ApiMethods::concat_response_text(&v), "alpha beta");
    }

    #[test]
    fn skips_non_text_blocks() {
        let v = json!({"content": [
            {"type": "tool_use", "name": "bash"},
            {"type": "text", "text": "result"},
        ]});
        assert_eq!(ApiMethods::concat_response_text(&v), "result");
    }

    #[test]
    fn returns_empty_for_missing_content() {
        let v = json!({"role": "assistant"});
        assert_eq!(ApiMethods::concat_response_text(&v), "");
    }

    #[test]
    fn returns_empty_for_non_array_content() {
        let v = json!({"content": "stringified"});
        assert_eq!(ApiMethods::concat_response_text(&v), "");
    }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;

    fn http(status: u16, body: &str) -> FailureStage {
        FailureStage::HttpError {
            status,
            error_type: anthropic_error_type(body),
        }
    }

    #[test]
    fn only_a_first_party_pre_generation_rejection_is_free() {
        use SettlementRoute::{AnthropicFirstParty as First, Unverified};
        let typed =
            |t: &str| format!(r#"{{"type":"error","error":{{"type":"{t}","message":"m"}}}}"#);
        for (status, error_type) in [
            (400, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
        ] {
            assert_eq!(
                failure_spend(First, http(status, &typed(error_type))),
                Spend::RejectedBeforeGeneration,
                "{status} {error_type}"
            );
            assert_eq!(
                failure_spend(Unverified, http(status, &typed(error_type))),
                Spend::Unknown,
                "unverified endpoint, {status}"
            );
        }
        for (status, body) in [
            (409, typed("invalid_request_error")),
            (413, typed("request_too_large")),
            (429, typed("rate_limit_error")),
            (400, "{}".to_string()),
            (400, typed("overloaded_error")),
            (401, typed("invalid_request_error")),
            (500, typed("api_error")),
            (529, typed("overloaded_error")),
        ] {
            assert_eq!(
                failure_spend(First, http(status, &body)),
                Spend::Unknown,
                "{status} {body}"
            );
        }
        for route in [First, Unverified] {
            assert_eq!(
                failure_spend(route, FailureStage::NotSent),
                Spend::NotDispatched
            );
            assert_eq!(
                failure_spend(route, FailureStage::AfterSend),
                Spend::Unknown
            );
        }
    }

    #[test]
    fn routed_failures_are_free_only_when_marked_not_dispatched() {
        let marked = crate::runtime::openai::NotDispatched::boxed("refused");
        assert_eq!(routed_failure_spend(marked.as_ref()), Spend::NotDispatched);
        for text in [
            "openai request failed: broker transport error: provider request failed: 400 Bad Request",
            "openai request failed: broker transport error: provider request failed: 429 Too Many Requests",
            "codex request failed: 413",
        ] {
            let error: Box<dyn std::error::Error + Send + Sync> = text.into();
            assert_eq!(routed_failure_spend(error.as_ref()), Spend::Unknown, "{text}");
        }
    }
}

#[cfg(test)]
mod simple_body_tests {
    use super::*;
    use serde_json::json;

    fn body(model: &str, ceiling: Option<u32>, auth: &str) -> Option<String> {
        let messages = vec![Arc::new(json!({"role":"user","content":"TRANSCRIPT"}))];
        simple_body(model, &messages, 0, ceiling, auth, "PROMPT", 200_000)
            .map(|(b, _)| b.to_string())
    }

    /// An unbounded (`/compact`) body for `thinking_budget` at `window`.
    fn unbounded(model: &str, thinking_budget: u32, window: u64) -> Value {
        let messages = vec![Arc::new(json!({"role":"user","content":"TRANSCRIPT"}))];
        simple_body(model, &messages, thinking_budget, None, "api_key", "PROMPT", window)
            .unwrap()
            .0
    }

    /// Goldens of the bounded (rollover summary) body per thinking class:
    /// the ceiling is `max_tokens` as given; thinking is the model's
    /// documented disable, absent where it is off when omitted, or explicit
    /// adaptive at effort low where it cannot be turned off.
    #[test]
    fn bounded_bodies_per_thinking_class() {
        let prefix = r#"{"max_tokens":"#;
        let msgs = r#""messages":[{"content":"TRANSCRIPT","role":"user"}]"#;
        let system = r#""system":[{"text":"PROMPT","type":"text"}]"#;
        let golden = |max: u32, model: &str, middle: &str, thinking: &str| {
            format!("{prefix}{max},{msgs},\"model\":\"{model}\",{middle}{system}{thinking}}}")
        };
        // Thinking cannot be off: adaptive, effort low.
        assert_eq!(
            body("claude-opus-5-5", Some(5038), "api_key").as_deref(),
            Some(
                golden(
                    5038,
                    "claude-opus-5-5",
                    r#""output_config":{"effort":"low"},"#,
                    r#","thinking":{"type":"adaptive"}"#
                )
                .as_str()
            )
        );
        // Sonnet 5.5 rejects `disabled`; `between_tools` is its off switch.
        assert_eq!(
            body("claude-sonnet-5-5", Some(2519), "api_key").as_deref(),
            Some(
                golden(
                    2519,
                    "claude-sonnet-5-5",
                    "",
                    r#","thinking":{"type":"between_tools"}"#
                )
                .as_str()
            )
        );
        // The documented disable.
        for model in ["claude-sonnet-4-6", "claude-haiku-4-5-20251001", "claude-opus-5"] {
            assert_eq!(
                body(model, Some(2519), "api_key").as_deref(),
                Some(golden(2519, model, "", r#","thinking":{"type":"disabled"}"#).as_str()),
                "{model}"
            );
        }
        // Off when omitted (before the 4.5 models): no thinking field.
        assert_eq!(
            body("claude-opus-4-1-20250805", Some(2519), "api_key").as_deref(),
            Some(golden(2519, "claude-opus-4-1-20250805", "", "").as_str())
        );
        // No verified ceiling: not built, so not sent.
        for model in ["claude-opus-6", "claude-opus-5-5-preview", "claude-next", ""] {
            assert_eq!(body(model, Some(2519), "api_key"), None, "{model:?}");
        }
    }

    /// The OAuth bounded body: the turn's transport identity first, then the
    /// thinking class (golden over the whole body).
    #[test]
    fn the_oauth_bounded_body() {
        let messages = vec![Arc::new(json!({"role":"user","content":"TRANSCRIPT"}))];
        let (body, _) =
            simple_body("claude-opus-5-5", &messages, 0, Some(5038), "oauth", "PROMPT", 200_000)
                .unwrap();
        let expected = json!({
            "max_tokens": 5038,
            "messages": [{"role":"user","content":"TRANSCRIPT"}],
            "model": "claude-opus-5-5",
            "output_config": {"effort": "low"},
            "system": [
                {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                {"type":"text","text": crate::core::config::get_identity()},
                {"type":"text","text":"You are a helpful AI assistant with access to tools. Use them when needed."},
                {"type":"text","text":"PROMPT"},
            ],
            "thinking": {"type": "adaptive"},
        });
        assert_eq!(body.to_string(), expected.to_string());
    }

    /// `/compact` (no ceiling) is unchanged: adaptive at effort low with the
    /// request's output cap at the window (B32: 64K at 200K), or the legacy
    /// budget.
    #[test]
    fn the_unbounded_compaction_body_is_unchanged() {
        let opus = body("claude-opus-5-5", None, "api_key").unwrap();
        let max = HelperMethods::max_tokens_for_request("claude-opus-5-5", 200_000);
        assert_eq!(max, 64_000);
        assert!(opus.starts_with(&format!(r#"{{"max_tokens":{max},"#)), "{opus}");
        assert!(
            opus.ends_with(r#""output_config":{"effort":"low"},"system":[{"text":"PROMPT","type":"text"}],"thinking":{"display":"summarized","type":"adaptive"}}"#),
            "{opus}"
        );
        let legacy = body("claude-sonnet-4-5", None, "api_key").unwrap();
        assert!(
            legacy.contains(r#""thinking":{"budget_tokens":"#) && !legacy.contains("output_config"),
            "{legacy}"
        );
    }

    /// B32: the unbounded body's `max_tokens` is the window-aware request
    /// cap (Opus: 64K at 200K, 128K above it), and a budgeted body's
    /// thinking stays strictly below it at every named level, with a custom
    /// budget at or over the cap clamped to `max_tokens - 1`.
    #[test]
    fn the_unbounded_body_sizes_output_by_the_window_and_keeps_thinking_below_it() {
        for (model, window, want) in [
            ("claude-opus-5-5", 200_000, 64_000),
            ("claude-opus-5-5", 1_000_000, 128_000),
            ("claude-opus-4-6", 200_000, 64_000),
            ("claude-opus-4-6", 1_000_000, 128_000),
            ("claude-sonnet-4-5", 200_000, 64_000),
            ("claude-sonnet-4-5", 1_000_000, 64_000),
        ] {
            let wire = unbounded(model, 0, window);
            assert_eq!(wire["max_tokens"], json!(want), "{model}@{window}");
        }
        for budget in [0, 2_048, 4_096, 16_384, 32_768] {
            let wire = unbounded("claude-opus-4-6", budget, 200_000);
            let sent = wire["thinking"]["budget_tokens"].as_u64().unwrap();
            assert!(sent < 64_000, "{budget}: {wire}");
        }
        let wire = unbounded("claude-opus-4-6", 100_000, 200_000);
        assert_eq!(wire["thinking"]["budget_tokens"], json!(63_999), "{wire}");
        let wire = unbounded("claude-opus-4-6", 100_000, 1_000_000);
        assert_eq!(wire["thinking"]["budget_tokens"], json!(100_000), "{wire}");
    }
}

#[cfg(test)]
mod ceiling_thinking_tests {
    use super::CeilingThinking::{self, *};

    /// Every listed id, with the prefix and suffixes it may carry; anything
    /// else (future, malformed, near misses) has no class.
    #[test]
    fn the_vetted_classes() {
        let cases = [
            ("claude-opus-5-5", Some(AdaptiveLow)),
            ("anthropic/claude-opus-5-5", Some(AdaptiveLow)),
            ("Claude-Opus-5-5", Some(AdaptiveLow)),
            ("claude-opus-5-5-20260301", Some(AdaptiveLow)),
            ("claude-fable-5-1", Some(AdaptiveLow)),
            ("claude-mythos-5-1", Some(AdaptiveLow)),
            ("claude-fable-5", Some(AdaptiveLow)),
            ("claude-mythos-5", Some(AdaptiveLow)),
            ("claude-mythos-preview", Some(AdaptiveLow)),
            ("claude-sonnet-5-5", Some(BetweenTools)),
            ("claude-haiku-5-5", Some(Disabled)),
            ("claude-opus-5", Some(Disabled)),
            ("claude-sonnet-5", Some(Disabled)),
            ("claude-opus-4-8", Some(Disabled)),
            ("claude-opus-4-7", Some(Disabled)),
            ("claude-opus-4-6", Some(Disabled)),
            ("claude-sonnet-4-6", Some(Disabled)),
            ("claude-opus-4-5-20251101", Some(Disabled)),
            ("claude-sonnet-4-5-20250929", Some(Disabled)),
            ("claude-haiku-4-5-20251001", Some(Disabled)),
            ("claude-opus-4-1-20250805", Some(OffWhenOmitted)),
            ("claude-opus-4-20250514", Some(OffWhenOmitted)),
            ("claude-sonnet-4-20250514", Some(OffWhenOmitted)),
            ("claude-3-7-sonnet-latest", Some(OffWhenOmitted)),
            ("claude-3-5-haiku-20241022", Some(OffWhenOmitted)),
            ("claude-opus-6", None),
            ("claude-opus-5-6", None),
            ("claude-opus-5-5-preview", None),
            ("claude-opus-5-5-2026030", None),
            ("claude-opus-999-0", None),
            ("claude-sonnet-5-5-x", None),
            ("openai/gpt-5", None),
            ("anthropic/", None),
            ("", None),
        ];
        for (model, class) in cases {
            assert_eq!(CeilingThinking::of(model), class, "{model:?}");
        }
    }

    #[test]
    fn the_limit_is_twice_the_cap_only_where_thinking_cannot_be_off() {
        assert_eq!(AdaptiveLow.limit(2_519, 128_000), 5_038);
        for class in [Disabled, BetweenTools, OffWhenOmitted] {
            assert_eq!(class.limit(2_519, 128_000), 2_519, "{class:?}");
        }
        // Clamped to the model's maximum, without overflow.
        assert_eq!(AdaptiveLow.limit(70_000, 128_000), 128_000);
        assert_eq!(AdaptiveLow.limit(u64::MAX, 64_000), 64_000);
        assert_eq!(Disabled.limit(70_000, 64_000), 64_000);
    }
}
