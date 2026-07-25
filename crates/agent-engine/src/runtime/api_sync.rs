//! Non-streaming Anthropic API calls (`call_api`, `call_api_simple`).
//!
//! Extracted from `api.rs`. The streaming path lives in `api.rs`; this file
//! holds the synchronous variants used by `Runtime::run_single` and
//! `Runtime::compact_call`. All methods are additional `impl ApiMethods`
//! blocks — the struct itself is defined in `api.rs`.

use super::api::{ApiMethods, ApiOptions};
use super::helpers::HelperMethods;
use super::types::AuthState;
use crate::{Result, RuntimeError, ToolRegistry};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
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
/// OAuth transport prepends exactly two synthesized identity/guidance
/// segments ahead of the caller's compaction prompt (reported as
/// `Synthesized`/`SystemSegment` with symbolic `system.synthetic[N]` IDs);
/// API-key transport sends the prompt alone (lossless).
fn compaction_system_blocks(
    auth_type: &str,
    system_prompt: &str,
) -> (Value, crate::runtime::transport::report::TranslationReport) {
    let mut report = crate::runtime::transport::report::TranslationReport::lossless();
    let blocks = if auth_type == "oauth" {
        let identity = crate::core::config::get_identity();
        let synthesized: [&str; 2] = [
            identity.as_str(),
            "You are a helpful AI assistant with access to tools. Use them when needed.",
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
        // Route through OpenAI-compat provider if model resolves to one.
        // This synchronous facade consumes only the final response value, so
        // high-volume display deltas are suppressed at the provider boundary.
        let tools_schema = tools.tools_schema();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
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
            &tokio_util::sync::CancellationToken::new(),
            &options.credential_source,
            &options.token_cache,
            max_retries,
            options.codex_request_role,
            options.tool_session_id.as_ref(),
            options.session_tool_set.as_ref(),
            &options.trace,
            options.suppress_stream_deltas,
        )
        .await
        {
            drop(tx);
            let _ = drain.await;
            return result.map_err(crate::runtime::openai::net::provider_error_to_runtime);
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
        // Strip empty/invalid thinking blocks before they hit the API. See
        // `sanitize_thinking_blocks` for the failure mode this guards against.
        HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, options.cache_ttl);

        // Body assembly (#128 Slice 4): borrow the history instead of the old
        // `json!` deep rebuild. Sync transport → no "stream" key. Byte-identity
        // enforced by `runtime::body_golden` (sync_no_stream_1h fixture).
        let tools_schema = tools.tools_schema();
        let parts = crate::runtime::transport::anthropic::build_anthropic_request(
            model,
            &cleaned_messages,
            &tools_schema,
            system_prompt,
            &auth_type,
            thinking_budget,
            reasoning_level,
            Some(&execution_plan),
            options.cache_ttl,
            false,
        );
        let body = parts.body;
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

                match req.body(body_bytes.clone()).send().await {
                    Ok(resp) => {
                        let status = resp.status();
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
                                        let delay = Duration::from_millis(
                                            1000 * 2u64.pow(non_429_attempts.saturating_sub(1)),
                                        );
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

                            let retry_exhausted = if is_429 {
                                attempt >= MAX_429_RETRIES
                            } else {
                                non_429_attempts >= max_retries
                            };

                            if !is_retryable || retry_exhausted {
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
                                return Err(RuntimeError::Tool(
                                    crate::core::error::humanize_api_error_with_reset(
                                        status.as_u16(),
                                        &error_text,
                                        hint,
                                    ),
                                ));
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
                        non_429_attempts += 1;
                        if non_429_attempts > max_retries {
                            if let Some(t) = tracer.take() {
                                let terminal = t.failed_terminal("network");
                                t.finish(clock, None, None, None, None, terminal);
                            }
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                        let delay = Duration::from_millis(
                            1000 * 2u64.pow(non_429_attempts.saturating_sub(1)),
                        );
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
                            "API retry {}/{} after {:?}: {}",
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
    ) -> Result<String> {
        let tools_schema = Arc::new(Vec::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let routed_system_prompt = Some(system_prompt.to_string());
        if let Some(result) = crate::runtime::openai::try_route(
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
            &options.trace,
            options.suppress_stream_deltas,
        )
        .await
        {
            drop(tx);
            let _ = drain.await;
            let response =
                result.map_err(crate::runtime::openai::net::provider_error_to_runtime)?;
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

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
            "messages": messages,
            "thinking": if crate::core::models::model_supports_adaptive_thinking(model) {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                let budget = if thinking_budget == 0 {
                    crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK
                } else {
                    thinking_budget
                };
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

        // System blocks + translation report come from one helper, in one
        // branch: the report is derived from the actual assembly decision
        // (OAuth synthesizes two identity/guidance segments; api_key is
        // lossless) and can never drift from the wire bytes.
        let (system_blocks, compaction_report) =
            compaction_system_blocks(&auth_type, system_prompt);
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
        // carries the two synthesized identity/guidance segment entries.
        let mut tracer = super::api::begin_anthropic_tracer(
            options,
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

        // Retry loop for transient errors (compaction path).
        // Same header-aware 429 handling as the main call_api loop.
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

                match req.body(body_bytes.clone()).send().await {
                    Ok(resp) => {
                        let status = resp.status();
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
                                    last_err = e.to_string();
                                    let delay = Duration::from_millis(
                                        1000 * 2u64.pow(non_429_attempts.saturating_sub(1)),
                                    );
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

                            let retry_exhausted = if is_429 {
                                attempt >= MAX_429_RETRIES_COMPACT
                            } else {
                                non_429_attempts >= max_retries
                            };

                            if !is_retryable || retry_exhausted {
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
                                return Err(RuntimeError::Tool(
                                    crate::core::error::humanize_api_error_with_reset(
                                        status.as_u16(),
                                        &error_text,
                                        hint,
                                    ),
                                ));
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
                        non_429_attempts += 1;
                        if non_429_attempts > max_retries {
                            if let Some(t) = tracer.take() {
                                let terminal = t.failed_terminal("network");
                                t.finish(clock, None, None, None, None, terminal);
                            }
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                        let delay = Duration::from_millis(
                            1000 * 2u64.pow(non_429_attempts.saturating_sub(1)),
                        );
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

    /// OAuth compaction synthesizes exactly two identity/guidance system
    /// segments; the report records exactly those two — action, element,
    /// and symbolic structural ID all exact.
    #[test]
    fn oauth_report_has_exactly_two_synthesized_system_entries() {
        let (_, report) = compaction_system_blocks("oauth", PROMPT_SENTINEL);
        assert_eq!(
            report.entries.len(),
            2,
            "oauth synthesizes exactly two system segments, got {:?}",
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

    /// Wire bytes are unchanged from the legacy inline assembly: OAuth is
    /// [identity, guidance, prompt]; api_key is [prompt] — byte-identical.
    #[test]
    fn system_blocks_match_legacy_wire_bytes() {
        let (oauth_blocks, _) = compaction_system_blocks("oauth", PROMPT_SENTINEL);
        let legacy_oauth = json!([
            {"type": "text", "text": crate::core::config::get_identity()},
            {"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."},
            {"type": "text", "text": PROMPT_SENTINEL},
        ]);
        assert_eq!(
            serde_json::to_vec(&oauth_blocks).unwrap(),
            serde_json::to_vec(&legacy_oauth).unwrap(),
            "oauth system blocks must be byte-identical to the legacy assembly"
        );

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
