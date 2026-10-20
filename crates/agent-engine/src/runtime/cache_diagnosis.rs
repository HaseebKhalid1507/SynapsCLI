//! Anthropic cache diagnostics (`cache_diagnostics = true`).
//!
//! Wire shape (docs `build-with-claude/cache-diagnostics`, confirmed by a
//! live OAuth probe against claude-haiku-4-5):
//! - request: top-level `"diagnostics": {"previous_message_id": <msg_… | null>}`.
//!   The object IS the opt-in; `null` on the first request of a chain.
//! - response: `message_start.message.diagnostics` is `null` (no
//!   divergence / nothing to compare), `{"cache_miss_reason": null}`
//!   (comparison still pending) or
//!   `{"cache_miss_reason": {"type": "system_changed", "cache_missed_input_tokens": 2314}}`.
//! - header: `cache-diagnosis-2026-04-07`. The feature is GA and works
//!   without it (probed on OAuth); like `extended-cache-ttl` it is sent on
//!   API-key auth only, so the OAuth beta set stays untouched.
//!
//! Only the foreground turn chain carries the chain state: keep-warm pings
//! send the header (cache-key parity with the turns they keep warm) but
//! never the `diagnostics` object and never move the chain, so the next
//! turn is compared against the previous TURN, not a ping whose trailing
//! message is not part of the durable history.
//!
//! Fail soft: a 4xx that names the beta or the `diagnostics` field latches
//! the feature off for the session (logged once) and the same request is
//! re-sent without it. A turn never fails because of diagnostics.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::runtime::trace::{CacheMissReason, ServerCacheDiagnosis};

/// The cache-diagnosis beta token.
pub(crate) const CACHE_DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";

/// Session-scoped cache-diagnosis chain: the last foreground Anthropic
/// message id plus the "beta rejected" latch. Cheap to clone (two Arcs);
/// every clone observes the same chain.
#[derive(Debug, Clone, Default)]
pub struct CacheDiagnosis {
    last_msg_id: Arc<Mutex<Option<String>>>,
    rejected: Arc<AtomicBool>,
}

impl CacheDiagnosis {
    pub(crate) fn new(last_msg_id: Arc<Mutex<Option<String>>>, rejected: Arc<AtomicBool>) -> Self {
        Self {
            last_msg_id,
            rejected,
        }
    }

    /// False once the API rejected the beta or the field this session.
    pub(crate) fn active(&self) -> bool {
        !self.rejected.load(Ordering::Relaxed)
    }

    pub(crate) fn previous_message_id(&self) -> Option<String> {
        self.last_msg_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Advance the chain. Only `msg_…` ids are kept: the API 400s on any
    /// other `previous_message_id`, so a malformed id must never be sent.
    pub(crate) fn record(&self, id: &str) {
        if !id.starts_with("msg_") {
            return;
        }
        *self
            .last_msg_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.to_string());
    }

    /// Latch diagnostics off for the session. Logs once.
    pub(crate) fn reject(&self, status: u16) {
        if !self.rejected.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                status,
                "cache diagnostics rejected by the API; disabled for this session"
            );
        }
    }
}

/// The request-side `diagnostics` object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct DiagnosticsParam {
    pub(super) previous_message_id: Option<String>,
}

/// `diagnostics` for a request with these options: `None` when the setting
/// is off, latched off, or the request is a keep-warm ping.
pub(super) fn diagnostics_for(options: &super::api::ApiOptions) -> Option<DiagnosticsParam> {
    if options.keepwarm.is_some() {
        return None;
    }
    let chain = options.cache_diagnosis.as_ref().filter(|c| c.active())?;
    Some(DiagnosticsParam {
        previous_message_id: chain.previous_message_id(),
    })
}

/// Whether the beta header goes on this request (pings included — the
/// header set must match the turns they keep warm).
pub(super) fn wants_beta_header(auth_type: &str, options: &super::api::ApiOptions) -> bool {
    auth_type != "oauth"
        && options
            .cache_diagnosis
            .as_ref()
            .is_some_and(CacheDiagnosis::active)
}

/// A 4xx that blames the beta header or the `diagnostics` field. Probed
/// shapes: `Unexpected value(s) `cache-diagnosis-…` for the `anthropic-beta`
/// header` and `diagnostics.previous_message_id: must be the `id` …`.
pub(super) fn is_rejection(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422)
        && (body.contains("cache-diagnosis") || body.contains("diagnostics"))
}

/// Remove the leading `"diagnostics":{…},` member from a serialized request
/// body. `RequestBody` serializes `diagnostics` first, so the result is
/// byte-identical to the body built without it. `None` if the body does not
/// start with exactly that member (then nothing is stripped).
pub(super) fn strip_diagnostics(body: &[u8], param: &DiagnosticsParam) -> Option<Vec<u8>> {
    let mut prefix = b"{\"diagnostics\":".to_vec();
    serde_json::to_writer(&mut prefix, param).ok()?;
    prefix.push(b',');
    let rest = body.strip_prefix(prefix.as_slice())?;
    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push(b'{');
    out.extend_from_slice(rest);
    Some(out)
}

/// Interpret `message_start.message.diagnostics`. `None` for absent, `null`
/// (no divergence) or a shape we do not understand; never fails the parse.
pub(super) fn parse_server_diagnosis(diagnostics: Option<&Value>) -> Option<ServerCacheDiagnosis> {
    let obj = diagnostics?.as_object()?;
    let reason = obj.get("cache_miss_reason")?;
    if reason.is_null() {
        return Some(ServerCacheDiagnosis {
            reason: CacheMissReason::Pending,
            missed_input_tokens: None,
        });
    }
    let reason = reason.as_object()?;
    let kind = reason.get("type")?.as_str()?;
    Some(ServerCacheDiagnosis {
        reason: CacheMissReason::from_wire(kind),
        missed_input_tokens: reason
            .get("cache_missed_input_tokens")
            .and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::api::{ApiMethods, ApiOptions, KeepWarmMode, KeepWarmRequest};
    use crate::runtime::telemetry::TelemetryLevel;
    use crate::runtime::types::AuthState;
    use crate::{StreamEvent, ToolRegistry};
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post as axum_post;
    use axum::Router;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::{mpsc, RwLock};

    fn chain() -> CacheDiagnosis {
        CacheDiagnosis::default()
    }

    fn on(chain: &CacheDiagnosis) -> ApiOptions {
        ApiOptions {
            cache_diagnosis: Some(chain.clone()),
            ..Default::default()
        }
    }

    // ── response parsing ────────────────────────────────────────────────

    #[test]
    fn parses_a_changed_reason_with_missed_tokens() {
        // Exact probed shape (live OAuth, claude-haiku-4-5).
        let v = json!({"cache_miss_reason": {"type": "system_changed", "cache_missed_input_tokens": 2314}});
        assert_eq!(
            parse_server_diagnosis(Some(&v)),
            Some(ServerCacheDiagnosis {
                reason: CacheMissReason::SystemChanged,
                missed_input_tokens: Some(2314),
            })
        );
    }

    #[test]
    fn no_miss_and_absent_field_parse_to_none() {
        assert_eq!(parse_server_diagnosis(None), None, "field absent");
        assert_eq!(
            parse_server_diagnosis(Some(&Value::Null)),
            None,
            "null = no divergence"
        );
    }

    #[test]
    fn pending_and_tokenless_reasons() {
        let pending = json!({"cache_miss_reason": null});
        assert_eq!(
            parse_server_diagnosis(Some(&pending)).map(|d| d.reason),
            Some(CacheMissReason::Pending)
        );
        let not_found = json!({"cache_miss_reason": {"type": "previous_message_not_found"}});
        assert_eq!(
            parse_server_diagnosis(Some(&not_found)),
            Some(ServerCacheDiagnosis {
                reason: CacheMissReason::PreviousMessageNotFound,
                missed_input_tokens: None,
            })
        );
    }

    #[test]
    fn unknown_or_malformed_shapes_never_panic() {
        let novel = json!({"cache_miss_reason": {"type": "brand_new_reason"}});
        assert_eq!(
            parse_server_diagnosis(Some(&novel)).map(|d| d.reason),
            Some(CacheMissReason::Other)
        );
        for v in [
            json!("x"),
            json!({}),
            json!({"cache_miss_reason": 3}),
            json!({"cache_miss_reason": {}}),
        ] {
            assert_eq!(parse_server_diagnosis(Some(&v)), None, "{v}");
        }
    }

    #[test]
    fn every_wire_reason_round_trips_through_as_str() {
        for s in [
            "model_changed",
            "system_changed",
            "tools_changed",
            "messages_changed",
            "previous_message_not_found",
            "unavailable",
        ] {
            assert_eq!(CacheMissReason::from_wire(s).as_str(), s);
        }
        // Untrusted text is never copied: anything unknown is "other".
        assert_eq!(CacheMissReason::from_wire("x\ny").as_str(), "other");
    }

    // ── request gating ──────────────────────────────────────────────────

    #[test]
    fn off_path_sends_nothing() {
        let off = ApiOptions::default();
        assert_eq!(diagnostics_for(&off), None);
        assert!(!wants_beta_header("api_key", &off));
    }

    #[test]
    fn first_request_opts_in_with_null_then_threads_the_id() {
        let c = chain();
        assert_eq!(
            diagnostics_for(&on(&c)),
            Some(DiagnosticsParam {
                previous_message_id: None
            })
        );
        c.record("msg_01abc");
        assert_eq!(
            diagnostics_for(&on(&c))
                .unwrap()
                .previous_message_id
                .as_deref(),
            Some("msg_01abc")
        );
        c.record("not-a-message-id");
        assert_eq!(
            c.previous_message_id().as_deref(),
            Some("msg_01abc"),
            "a malformed id would 400 — never adopted"
        );
    }

    #[test]
    fn keepwarm_pings_never_carry_diagnostics_but_keep_the_header() {
        let c = chain();
        c.record("msg_turn");
        let ping = ApiOptions {
            keepwarm: Some(KeepWarmRequest {
                mode: KeepWarmMode::Probe,
                status: Default::default(),
                sent: Default::default(),
                connect_failed: Default::default(),
            }),
            ..on(&c)
        };
        assert_eq!(diagnostics_for(&ping), None);
        assert!(
            wants_beta_header("api_key", &ping),
            "header parity with the turn"
        );
    }

    #[test]
    fn beta_header_is_api_key_only_and_drops_once_rejected() {
        let c = chain();
        let model = "claude-sonnet-4-6";
        assert_eq!(
            ApiMethods::build_beta_header("api_key", &on(&c), model).as_deref(),
            Some(CACHE_DIAGNOSIS_BETA)
        );
        // OAuth: the beta set is part of the routing fingerprint and the
        // feature is honoured without the header (live probe) — unchanged.
        assert_eq!(
            ApiMethods::build_beta_header("oauth", &on(&c), model),
            ApiMethods::build_beta_header("oauth", &ApiOptions::default(), model)
        );
        c.reject(400);
        assert!(!c.active());
        assert_eq!(
            ApiMethods::build_beta_header("api_key", &on(&c), model),
            None
        );
        assert_eq!(diagnostics_for(&on(&c)), None);
    }

    #[test]
    fn rejection_classifier_matches_probed_errors_only() {
        let beta = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `cache-diagnosis-2026-04-07` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header."}}"#;
        let field = r#"{"type":"error","error":{"type":"invalid_request_error","message":"diagnostics.previous_message_id: must be the `id` from a prior /v1/messages response (starts with `msg_`)"}}"#;
        let extra = r#"{"type":"error","error":{"type":"invalid_request_error","message":"diagnostics: Extra inputs are not permitted"}}"#;
        assert!(is_rejection(400, beta));
        assert!(is_rejection(400, field));
        assert!(is_rejection(400, extra));
        assert!(!is_rejection(
            400,
            r#"{"error":{"message":"prompt is too long"}}"#
        ));
        assert!(
            !is_rejection(500, beta),
            "server errors keep the normal retry path"
        );
        assert!(!is_rejection(429, beta));
    }

    // ── body bytes ──────────────────────────────────────────────────────

    fn body_bytes(diag: Option<DiagnosticsParam>, model: &str) -> Vec<u8> {
        let messages = vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let tools = vec![json!({"name": "t", "input_schema": {"type": "object"}})];
        let mut body = super::super::request::RequestBody::new(
            model,
            &messages,
            &tools,
            &[],
            &Some("sys".to_string()),
            "api_key",
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            None,
            crate::core::config::CacheTtl::FiveMinutes,
            true,
        );
        if let Some(d) = diag {
            body.set_diagnostics(d);
        }
        serde_json::to_vec(&body).unwrap()
    }

    #[test]
    fn diagnostics_serializes_first_and_strips_back_to_the_off_bytes() {
        let model = "claude-sonnet-4-6";
        let off = body_bytes(None, model);
        let first = DiagnosticsParam {
            previous_message_id: None,
        };
        let next = DiagnosticsParam {
            previous_message_id: Some("msg_01abc".into()),
        };
        let on_first = body_bytes(Some(first.clone()), model);
        let on_next = body_bytes(Some(next.clone()), model);
        assert!(
            on_first.starts_with(br#"{"diagnostics":{"previous_message_id":null},"max_tokens":"#),
            "{}",
            String::from_utf8_lossy(&on_first)
        );
        assert!(on_next
            .starts_with(br#"{"diagnostics":{"previous_message_id":"msg_01abc"},"max_tokens":"#));
        // The rest of the body is the off body verbatim.
        assert_eq!(on_next[on_next.len() - (off.len() - 1)..], off[1..]);
        assert_eq!(
            strip_diagnostics(&on_first, &first).as_deref(),
            Some(off.as_slice())
        );
        assert_eq!(
            strip_diagnostics(&on_next, &next).as_deref(),
            Some(off.as_slice())
        );
        assert_eq!(strip_diagnostics(&off, &first), None, "nothing to strip");
        assert_eq!(
            strip_diagnostics(&on_next, &first),
            None,
            "wrong member never stripped"
        );
    }

    // ── transport (loopback) ────────────────────────────────────────────

    type Captured = Arc<Mutex<Vec<(Option<String>, Value)>>>;

    fn sse(id: &str, diagnostics: &str) -> String {
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{id}\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,",
                "\"usage\":{{\"input_tokens\":10,\"output_tokens\":1,\"cache_creation_input_tokens\":0,",
                "\"cache_read_input_tokens\":0}}{diag}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},",
                "\"usage\":{{\"input_tokens\":10,\"output_tokens\":2,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
            diag = diagnostics,
        )
    }

    /// Loopback stub. Answers 400 (beta-rejection text) to the first
    /// `reject` requests that carry `diagnostics`; otherwise SSE with ids
    /// `msg_1`, `msg_2`, … and a `system_changed` diagnosis from the second
    /// response on.
    async fn stub(reject: usize) -> (String, Captured) {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));
        let cap = Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move |headers: HeaderMap, body: Bytes| {
                let cap = Arc::clone(&cap);
                let hits = Arc::clone(&hits);
                let rejected = Arc::clone(&rejected);
                async move {
                    let beta = headers
                        .get("anthropic-beta")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    let has_diag = body.get("diagnostics").is_some();
                    cap.lock().unwrap().push((beta, body));
                    if has_diag && rejected.fetch_add(1, Ordering::SeqCst) < reject {
                        return (
                            StatusCode::BAD_REQUEST,
                            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `cache-diagnosis-2026-04-07` for the `anthropic-beta` header."}}"#.to_string(),
                        )
                            .into_response();
                    }
                    let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    let diag = if n == 1 {
                        r#","diagnostics":null"#
                    } else {
                        r#","diagnostics":{"cache_miss_reason":{"type":"system_changed","cache_missed_input_tokens":2314}}"#
                    };
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        sse(&format!("msg_{n}"), diag),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    fn auth() -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: "sk-test".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    async fn turn(options: &ApiOptions, level: TelemetryLevel) -> crate::error::Result<Value> {
        let messages = vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        ApiMethods::call_api_stream(
            &auth(),
            &reqwest::Client::new(),
            "claude-sonnet-4-6",
            &ToolRegistry::new(),
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages,
            tx,
            0,
            0,
            options,
            level,
        )
        .await
    }

    fn writer(dir: &std::path::Path) -> crate::runtime::telemetry::TelemetryWriter {
        crate::runtime::telemetry::TelemetryWriter::new(crate::runtime::telemetry::WriterOptions {
            telemetry_path: Some(dir.join("api-log.jsonl")),
            trace_path: Some(dir.join("request-trace.jsonl")),
            capacity: 16,
            max_file_bytes: crate::runtime::telemetry::DEFAULT_MAX_FILE_BYTES,
            write_delay: None,
            write_gate: None,
        })
    }

    #[tokio::test]
    async fn on_path_threads_the_chain_and_records_the_reason() {
        let (url, captured) = stub(0).await;
        let tmp = tempfile::TempDir::new().unwrap();
        let w = writer(tmp.path());
        let c = chain();
        let options = ApiOptions {
            anthropic_base_url: Some(url),
            telemetry: Some(w.clone()),
            ..on(&c)
        };
        turn(&options, TelemetryLevel::Basic).await.unwrap();
        turn(&options, TelemetryLevel::Basic).await.unwrap();
        assert_eq!(c.previous_message_id().as_deref(), Some("msg_2"));

        let reqs = captured.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        for (beta, _) in &reqs {
            assert!(
                beta.as_deref()
                    .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)),
                "{beta:?}"
            );
        }
        assert_eq!(
            reqs[0].1["diagnostics"],
            json!({"previous_message_id": null})
        );
        assert_eq!(
            reqs[1].1["diagnostics"],
            json!({"previous_message_id": "msg_1"})
        );

        w.shutdown(std::time::Duration::from_secs(5));
        let log = std::fs::read_to_string(tmp.path().join("api-log.jsonl")).unwrap();
        let recs: Vec<Value> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 2);
        assert!(
            recs[0].get("cache_diag").is_none(),
            "no divergence → absent"
        );
        assert_eq!(
            recs[1]["cache_diag"],
            json!({"miss_reason": "system_changed", "missed_tokens": 2314})
        );
    }

    #[tokio::test]
    async fn off_path_sends_no_header_and_no_field() {
        let (url, captured) = stub(0).await;
        let options = ApiOptions {
            anthropic_base_url: Some(url),
            ..Default::default()
        };
        turn(&options, TelemetryLevel::Off).await.unwrap();
        let reqs = captured.lock().unwrap().clone();
        assert_eq!(reqs[0].0, None);
        assert!(reqs[0].1.get("diagnostics").is_none());
    }

    #[tokio::test]
    async fn rejected_beta_latches_off_and_the_turn_still_succeeds() {
        let (url, captured) = stub(1).await;
        let c = chain();
        let options = ApiOptions {
            anthropic_base_url: Some(url),
            ..on(&c)
        };
        // max_retries = 0: the fallback must not depend on the error budget.
        turn(&options, TelemetryLevel::Off)
            .await
            .expect("a diagnostics rejection must never fail the turn");
        assert!(!c.active(), "latched off for the session");
        turn(&options, TelemetryLevel::Off).await.unwrap();

        let reqs = captured.lock().unwrap().clone();
        assert_eq!(reqs.len(), 3, "rejected + resend + next turn");
        assert!(reqs[0].1.get("diagnostics").is_some());
        for (beta, body) in &reqs[1..] {
            assert_eq!(beta.as_deref(), None, "header dropped");
            assert!(body.get("diagnostics").is_none(), "field dropped");
        }
        // The resend is the same request minus the diagnostics member.
        let mut first = reqs[0].1.clone();
        first.as_object_mut().unwrap().remove("diagnostics");
        assert_eq!(first, reqs[1].1);
    }
}
