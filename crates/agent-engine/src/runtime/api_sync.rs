//! The non-streaming Anthropic API call (`call_api_simple`).
//!
//! Extracted from `api.rs`. The streaming path lives in `api.rs`; this file
//! holds the synchronous call `Runtime::compact_call` makes. Methods are
//! additional `impl ApiMethods` blocks; the struct is defined in `api.rs`.

use super::api::{ApiMethods, ApiOptions};
use super::helpers::HelperMethods;
use super::types::AuthState;
use crate::{Result, RuntimeError};
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

/// The usage a non-streaming response body reports, cache-write TTL split
/// included, for the api-log record and for booking. `None` without a
/// `usage` object; a count the provider did not send is zero, as on the
/// streaming path.
fn sync_usage_record(json: &Value) -> Option<super::telemetry::UsageRecord> {
    let usage = json.get("usage")?.as_object()?;
    let get = |key: &str| usage.get(key).and_then(Value::as_u64);
    let split = |key: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(key))
            .and_then(Value::as_u64)
    };
    let mut record = super::telemetry::UsageRecord {
        input: get("input_tokens").unwrap_or(0),
        output: get("output_tokens").unwrap_or(0),
        cache_read: get("cache_read_input_tokens").unwrap_or(0),
        cache_write: get("cache_creation_input_tokens").unwrap_or(0),
        cache_write_5m: split("ephemeral_5m_input_tokens"),
        cache_write_1h: split("ephemeral_1h_input_tokens"),
        hit_pct: 0.0,
    };
    record.compute_hit_pct();
    Some(record)
}

/// What [`ApiMethods::call_api_simple`] reports about the request it sent,
/// besides the reply text. The default reports nothing.
#[derive(Default, Clone, Copy)]
pub(super) struct SimpleCallReport<'a> {
    /// Level of the request's api-log record, as for a streamed request:
    /// `Off` writes none, `Full` adds the rate-limit headers.
    pub telemetry_level: super::telemetry::TelemetryLevel,
    /// Where the usage the provider billed is booked (the session that
    /// asked for the compaction books it from there).
    pub spend: Option<&'a super::compaction::CompactionSpend>,
}

/// What the response headers of an accepted request say, read before the
/// body consumes the response. Each only at the level that records it.
struct AcceptedHeaders {
    request_id: Option<String>,
    ratelimit: Option<super::telemetry::RateLimitRecord>,
}

impl AcceptedHeaders {
    fn read(headers: &reqwest::header::HeaderMap, level: super::telemetry::TelemetryLevel) -> Self {
        use super::telemetry::{self, TelemetryLevel};
        Self {
            request_id: level
                .enabled()
                .then(|| telemetry::request_id_from_headers(headers))
                .flatten(),
            ratelimit: (level == TelemetryLevel::Full)
                .then(|| telemetry::ratelimit_from_headers(headers))
                .filter(|rl| !rl.is_empty()),
        }
    }
}

/// Bytes of JSON request body that are certainly more than one input token.
/// English prose runs at 3.5 to 4 bytes a token and code or JSON at fewer;
/// twice that leaves room for escaping and long whitespace runs, so
/// `body bytes / 8` is a floor under the request's token count, not an
/// estimate of it.
const BODY_BYTES_PER_TOKEN_CEILING: u64 = 8;

/// A 429 that no retry can cure: the request sends more input tokens than
/// the account's per-minute input limit holds. `Some(message)` only when the
/// response states that limit and even the floor under this request's size
/// ([`BODY_BYTES_PER_TOKEN_CEILING`]) is above it. The limit is a token
/// bucket that refills up to the limit and no further
/// (<https://platform.claude.com/docs/en/api/rate-limits#about-rate-limits>),
/// so waiting never makes room for such a request.
///
/// Only for a request that carries no cache marker, like this file's: every
/// input token of it counts against the limit (cache reads would not).
fn exceeds_input_rate_limit(
    headers: &reqwest::header::HeaderMap,
    body_len: usize,
) -> Option<String> {
    let limit = super::telemetry::input_tokens_limit_from_headers(headers)?;
    let at_least = body_len as u64 / BODY_BYTES_PER_TOKEN_CEILING;
    (at_least > limit).then(|| {
        format!(
            "Compaction request refused (HTTP 429) and not retried: it sends at least \
             {at_least} input tokens, none of them cached, and this account's limit is \
             {limit} input tokens per minute, so no retry can fit it. Set \
             `compaction_mode = local`, or set `compaction_model` to a model with a higher limit."
        )
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

    /// Simple non-streaming API call without tools (used for compaction).
    /// Uses a caller-supplied system prompt (replaces the runtime's) and forces
    /// "low" effort on adaptive models — summarization doesn't benefit from
    /// heavy reasoning budgets.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_api_simple(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        system_prompt: &str,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        messages: &[crate::SharedMessage],
        max_retries: u32,
        options: &ApiOptions,
        report: SimpleCallReport<'_>,
    ) -> Result<String> {
        // Host-only message metadata never reaches any provider, whatever
        // history the caller passes in; strip before routing/serializing.
        let wire_messages = super::continuation::wire_messages(messages);
        let messages = wire_messages.as_deref().unwrap_or(messages);
        let tools_schema = Arc::new(Vec::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Nothing of the routed reply is shown; its usage is kept, to book.
        let drain = tokio::spawn(async move {
            let mut usage = super::keepwarm::KeepWarmUsage::default();
            while let Some(event) = rx.recv().await {
                usage.add(&event);
            }
            usage
        });
        let routed_system_prompt = Some(system_prompt.to_string());
        if let Some(result) = crate::runtime::openai::try_route_with_memory_backend(
            model,
            client,
            &tools_schema,
            &routed_system_prompt,
            messages,
            &tx,
            None,
            None,
            thinking_budget,
            reasoning_level,
            &tokio_util::sync::CancellationToken::new(),
            // call_api_simple is an internal helper without ApiOptions; codex here
            // uses the Local credential. (Broker-routed codex goes via the stream path.)
            &crate::auth::CredentialSource::Local,
            &crate::auth::TokenCache::new(),
            max_retries,
            crate::runtime::openai::catalog::CodexRequestRole::Internal,
            // Internal text-only helper: no tool session identity here, so
            // an extension route (which cannot execute tools from this
            // empty-schema path anyway) fails closed to a local
            // default-core gate identity — never to ungated execution.
            None,
            None,
            options.tool_context.as_ref(),
            &options.trace,
            options.suppress_stream_deltas,
        )
        .await
        {
            drop(tx);
            // Booked whatever became of the reply: a usage was billed.
            if let (Ok(usage), Some(spend)) = (drain.await, report.spend) {
                spend.record(
                    agent_core::compaction::SummaryRequest::Transcript,
                    model,
                    usage,
                );
            }
            let response = result.map_err(|e| {
                crate::runtime::openai::net::provider_error_to_runtime_for(model, e)
            })?;
            return Ok(Self::concat_response_text(&response));
        }
        let model = model.strip_prefix("anthropic/").unwrap_or(model);

        let (auth_header_name, auth_header_value, auth_type) = Self::build_auth_header(auth).await;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No API key or OAuth token found. Run `synaps login` to authenticate.".to_string(),
            ));
        }

        // System blocks + translation report come from one helper, in one
        // branch: the report is derived from the actual assembly decision
        // (OAuth synthesizes transport identity + product identity +
        // guidance; api_key is lossless) and can never drift from the wire.
        let (system_blocks, compaction_report) =
            compaction_system_blocks(&auth_type, system_prompt);
        let adaptive = crate::core::models::model_supports_adaptive_thinking(model);
        let budget = if thinking_budget == 0 {
            crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK
        } else {
            thinking_budget
        };
        // A model that rejects input + `max_tokens` over the window gets the
        // room the input leaves (no-op on Claude 4.5 and later). Measured
        // against the window this path's beta set (default options) opens.
        let max_tokens = HelperMethods::fit_max_tokens_to_window(
            model,
            HelperMethods::max_tokens_for_model(model),
            Self::provider_window(&ApiOptions::default(), model),
            (!adaptive).then_some(u64::from(budget)),
            || {
                super::context::conservative_token_estimate(&system_blocks.to_string())
                    .saturating_add(super::context::estimate_history(messages))
            },
        );
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": messages,
            "thinking": if adaptive {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
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
        // api-log: the thinking this body carries, by its level name. Not
        // the session's level: an adaptive model is sent `adaptive` whatever
        // that is. A budget model is sent the session's budget, which is
        // its level's; the fallback budget (a session with none) has no
        // level name, so the record then says nothing.
        let sent_thinking = if adaptive {
            Some(agent_core::reasoning::ReasoningLevel::Adaptive.as_str().to_string())
        } else {
            (thinking_budget != 0).then(|| reasoning_level.as_str().to_string())
        };

        body["system"] = system_blocks;

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

        // Retry loop for transient errors (compaction path).
        // Header-aware 429 handling, as on the streaming path.
        const MAX_429_RETRIES_COMPACT: u32 = 8;
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
                    .header(auth_header_name.clone(), auth_header_value.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if let Some(beta) =
                    Self::build_beta_header(&auth_type, &ApiOptions::default(), model)
                {
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
                            let accepted =
                                AcceptedHeaders::read(resp.headers(), report.telemetry_level);
                            match resp.json::<Value>().await {
                                Ok(j) => {
                                    if j["error"].is_object() {
                                        // SECURITY (spec §5.1): untrusted envelope —
                                        // surface only a vetted static class label.
                                        if let Some(error_type) = j["error"]["type"].as_str() {
                                            let vetted =
                                                crate::core::error::sanitize_error_type(error_type)
                                                    .unwrap_or("unrecognized_error");
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
                                    clock.mark_stream_end();
                                    // ═══ TELEMETRY: the api-log record ═══
                                    // One per accepted request, as on the
                                    // streaming path. Only what this request
                                    // knows: it has no first-event time
                                    // (`ttft_ms`), no refusal retries, no
                                    // cache diagnosis and no tools.
                                    if report.telemetry_level.enabled() {
                                        let usage = sync_usage_record(&j).unwrap_or_default();
                                        let mut record = super::telemetry::TelemetryRecord {
                                            ts: super::telemetry::TelemetryRecord::now_ms(),
                                            request_id: accepted.request_id,
                                            msg_id: j["id"].as_str().map(str::to_owned),
                                            model: model.to_string(),
                                            thinking: sent_thinking.clone(),
                                            effort: body["output_config"]["effort"]
                                                .as_str()
                                                .map(str::to_owned),
                                            attempt: attempt + 1,
                                            total_ms: clock.stream_end_or_now_ms(),
                                            stop_reason: j["stop_reason"]
                                                .as_str()
                                                .map(str::to_owned),
                                            cost_usd: usage.cost_usd(model),
                                            usage,
                                            ratelimit: accepted.ratelimit,
                                            context: super::telemetry::ContextRecord {
                                                messages: messages.len(),
                                                tools: 0,
                                                system_bytes: system_prompt.len(),
                                                breakpoints: Vec::new(),
                                            },
                                            compaction: true,
                                            summary_request: Some(
                                                agent_core::compaction::SummaryRequest::Transcript,
                                            ),
                                            ..Default::default()
                                        };
                                        record.set_correlation(
                                            options.trace.session_id(),
                                            tracer.as_ref().map(|t| t.correlation_ids()),
                                        );
                                        if let Some(writer) = options.telemetry.as_ref() {
                                            writer.enqueue_telemetry(record);
                                        }
                                    }
                                    if let Some(t) = tracer.take() {
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
                                    tracing::warn!(
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
                            let is_retryable = super::api::is_retryable_status(status.as_u16());
                            let (delay, from_hdr) = super::telemetry::retry_delay_from_headers(
                                resp.headers(),
                                attempt + 1,
                            );
                            let reset_hint = if from_hdr {
                                Some(format!("{}s", delay.as_secs()))
                            } else {
                                None
                            };
                            // A 429 this request can never get past: say so
                            // now instead of spending the whole retry budget.
                            let never_fits = is_429
                                .then(|| exceeds_input_rate_limit(resp.headers(), body_bytes.len()))
                                .flatten();
                            let error_text = resp.text().await.unwrap_or_default();
                            if let Some(message) = never_fits {
                                let log_rid = trace_rid.clone();
                                if let Some(t) = tracer.take() {
                                    let terminal = t.failed_terminal("http_429_input_limit");
                                    t.finish(clock, Some(429), trace_rid, None, None, terminal);
                                }
                                tracing::warn!(
                                    request_id = log_rid.as_ref().map_or("-", |id| id.as_str()),
                                    body_bytes = body_bytes.len(),
                                    "compaction request exceeds the per-minute input limit (not retried)"
                                );
                                return Err(RuntimeError::ApiStatus(message));
                            }

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
                                attempt >= MAX_429_RETRIES_COMPACT
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
                        non_429_attempts += 1;
                        if non_429_attempts > max_retries {
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
                        tracing::warn!(
                            "Compaction API retry {}/{} after {:?}: {}",
                            non_429_attempts,
                            max_retries,
                            delay,
                            last_err
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

        // Log usage, and book it: before the reply is looked at, since an
        // empty one was billed too.
        if let Some(usage) = sync_usage_record(&json) {
            HelperMethods::log_usage(usage.input, usage.cache_read, usage.cache_write, usage.output);
            if let Some(spend) = report.spend {
                spend.record(
                    agent_core::compaction::SummaryRequest::Transcript,
                    model,
                    super::keepwarm::KeepWarmUsage {
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        cache_read_tokens: usage.cache_read,
                        cache_write_tokens: usage.cache_write,
                        cache_write_5m: usage.cache_write_5m,
                        cache_write_1h: usage.cache_write_1h,
                        model: json["model"].as_str().map(str::to_owned),
                    },
                );
            }
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

        if out.is_empty() {
            return Err(RuntimeError::Tool(
                "Compaction returned empty response".to_string(),
            ));
        }

        Ok(out)
    }
}

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
mod input_rate_limit_tests {
    use super::*;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn headers(limit: Option<&str>) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        if let Some(limit) = limit {
            h.insert(
                "anthropic-ratelimit-input-tokens-limit",
                limit.parse().unwrap(),
            );
        }
        h
    }

    /// Only a stated limit that even the floor under the request's size
    /// exceeds counts; anything else keeps the ordinary retry.
    #[test]
    fn only_a_request_certainly_over_the_stated_limit_never_fits() {
        // 1.6 MB is at least 200,000 tokens.
        let incident = 1_600_000;
        let message = exceeds_input_rate_limit(&headers(Some("50000")), incident).unwrap();
        assert!(message.contains("at least 200000 input tokens"), "{message}");
        assert!(message.contains("50000 input tokens per minute"), "{message}");
        assert!(message.contains("no retry can fit it"), "{message}");
        // At the limit, or possibly under it: the provider decides.
        assert_eq!(exceeds_input_rate_limit(&headers(Some("200000")), incident), None);
        assert_eq!(exceeds_input_rate_limit(&headers(Some("2000000")), incident), None);
        // No limit stated (a subscription login), or not a number.
        assert_eq!(exceeds_input_rate_limit(&headers(None), incident), None);
        assert_eq!(exceeds_input_rate_limit(&headers(Some("lots")), incident), None);
    }

    /// A provider that answers every request with 429 and `limit`.
    async fn always_429(limit: Option<&'static str>) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1/messages",
                post(move |State(hits): State<Arc<AtomicUsize>>| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let mut response = (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        r#"{"type":"error","error":{"type":"rate_limit_error","message":"synthetic"}}"#,
                    )
                        .into_response();
                    if let Some(limit) = limit {
                        response.headers_mut().insert(
                            "anthropic-ratelimit-input-tokens-limit",
                            limit.parse().unwrap(),
                        );
                    }
                    response
                }),
            )
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    async fn compact_against(limit: Option<&'static str>) -> (String, usize) {
        let (url, hits) = always_429(limit).await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = crate::Runtime::new_headless();
        // About 40 KB: at least 5,000 tokens.
        let transcript = "a line of the transcript\n".repeat(1_600);
        let error = rt
            .compact_call(
                "claude-sonnet-4-6",
                vec![Arc::new(json!({"role": "user", "content": transcript}))],
            )
            .await
            .expect_err("every attempt is refused");
        (error.to_string(), hits.load(Ordering::SeqCst))
    }

    /// The request is larger than the whole per-minute allowance: one
    /// attempt, and an error that says why.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_429_the_request_can_never_fit_is_not_retried() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (error, attempts) = compact_against(Some("1000")).await;
        assert_eq!(attempts, 1, "retried a request that can never fit: {error}");
        assert!(error.contains("1000 input tokens per minute"), "{error}");
        assert!(error.contains("no retry can fit it"), "{error}");
    }

    /// Control: a 429 the request could get past keeps its retry budget,
    /// with a limit above the request and with no limit stated at all.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_429_the_request_could_fit_keeps_its_retries() {
        let _base = crate::test_env::BaseDirGuard::new();
        for limit in [Some("2000000"), None] {
            let (error, attempts) = compact_against(limit).await;
            assert_eq!(attempts, 9, "the first attempt and 8 retries: {error}");
            assert!(!error.contains("no retry can fit it"), "{error}");
        }
    }
}
