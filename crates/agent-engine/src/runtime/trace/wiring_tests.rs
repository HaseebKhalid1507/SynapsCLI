//! Task 8 wiring tests: the Anthropic transport emits one schema-valid
//! `synaps-request-trace/1` record per actual attempt, with wire metadata
//! computed from the exact bytes the loopback server received.
//!
//! All fixtures are local loopback servers — no real network. Timing
//! assertions use tolerant lower bounds against independently delayed
//! stages.

use super::{CollectingTraceSink, RequestTrace, StopReason, TraceContext};
use crate::runtime::api::{ApiMethods, ApiOptions};
use crate::runtime::telemetry::TelemetryLevel;
use crate::runtime::types::AuthState;
use crate::{StreamEvent, ToolRegistry};
use agent_core::TurnOutcome;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post as axum_post;
use axum::Router;
use reqwest::Client;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "TRACE_SENTINEL_0badc0de_never_persist";

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

type CapturedBodies = Arc<Mutex<Vec<Vec<u8>>>>;

fn sse_response() -> Response {
    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream"),
            ("request-id", "req_stub_abc123"),
        ],
        SSE_SUCCESS.to_string(),
    )
        .into_response()
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Loopback Anthropic stub: the first `fail_statuses.len()` POSTs answer
/// with those statuses (`retry-after: 0` so tests stay fast), then SSE
/// success. Captures every received body byte-exactly.
async fn spawn_stub(fail_statuses: Vec<u16>) -> (String, CapturedBodies, Arc<AtomicUsize>) {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies_c = Arc::clone(&bodies);
    let hits_c = Arc::clone(&hits);
    let app = Router::new().route(
        "/v1/messages",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            let hits = Arc::clone(&hits_c);
            let fail_statuses = fail_statuses.clone();
            async move {
                bodies.lock().unwrap().push(body.to_vec());
                let n = hits.fetch_add(1, Ordering::SeqCst);
                match fail_statuses.get(n) {
                    Some(&status) => (
                        StatusCode::from_u16(status).unwrap(),
                        [("retry-after", "0")],
                        "{\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}".to_string(),
                    )
                        .into_response(),
                    None => sse_response(),
                }
            }
        }),
    );
    (serve(app).await, bodies, hits)
}

struct Harness {
    sink: Arc<CollectingTraceSink>,
    options: ApiOptions,
    key_path: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn harness(base_url: String) -> Harness {
    let tmp = tempfile::TempDir::new().unwrap();
    let key_path = tmp.path().join("trace").join("digest.key");
    let sink = CollectingTraceSink::new();
    let options = ApiOptions {
        anthropic_base_url: Some(base_url),
        trace: TraceContext::with_sink(sink.clone()).with_key_path(key_path.clone()),
        ..Default::default()
    };
    Harness {
        sink,
        options,
        key_path,
        _tmp: tmp,
    }
}

fn auth() -> Arc<RwLock<AuthState>> {
    Arc::new(RwLock::new(AuthState {
        auth_token: "sk-test".into(),
        auth_type: "api_key".into(),
        refresh_token: None,
        token_expires: Some(9_999_999_999_999),
    }))
}

async fn drive_stream(
    options: &ApiOptions,
    max_retries: u32,
    cancel: &CancellationToken,
) -> crate::error::Result<serde_json::Value> {
    let client = Client::new();
    let tools = ToolRegistry::new();
    let messages = vec![Arc::new(json!({
        "role": "user",
        "content": format!("{SENTINEL} hello"),
    }))];
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
    ApiMethods::call_api_stream_inner(
        &auth(),
        &client,
        "claude-haiku-4-5",
        &tools,
        &Some(format!("system {SENTINEL}")),
        0,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &messages,
        tx,
        cancel,
        max_retries,
        0,
        options,
        TelemetryLevel::Off,
    )
    .await
}

/// Every record must round-trip through serde (schema validity) and must
/// never contain the raw content sentinel.
fn assert_schema_valid_and_content_free(records: &[RequestTrace]) {
    for record in records {
        let json = serde_json::to_string(record).expect("record serializes");
        assert!(
            !json.contains(SENTINEL),
            "raw content leaked into serialized trace: {json}"
        );
        let back: RequestTrace = serde_json::from_str(&json).expect("record re-validates on read");
        assert_eq!(&back, record, "record must round-trip deterministically");
    }
}

fn expected_wire_digest(key_path: &std::path::Path, bytes: &[u8]) -> super::ComponentDigest {
    let key = super::load_or_create_digest_key_at(key_path).expect("test key loads");
    super::keyed_digest(&key, super::DigestDomain::Wire, bytes)
}

// ─── Success ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn success_emits_one_record_with_exact_wire_digest() {
    let (url, bodies, _hits) = spawn_stub(vec![]).await;
    let h = harness(url);

    drive_stream(&h.options, 0, &CancellationToken::new())
        .await
        .expect("success fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "success must emit exactly one record");
    let r = &records[0];
    assert_schema_valid_and_content_free(&records);

    assert_eq!(r.attempt, 1);
    assert!(r.outcome.retries.is_empty());
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(r.outcome.http_status, Some(200));
    assert_eq!(r.outcome.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(
        r.outcome.provider_request_id.as_ref().map(|id| id.as_str()),
        Some("req_stub_abc123"),
        "provider request id from response headers"
    );
    let usage = r.outcome.usage.expect("usage observed from stream");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(1));
    assert_eq!(r.model.provider(), "anthropic");
    assert_eq!(r.endpoint.path(), "/v1/messages");
    assert_eq!(r.anatomy.message_count, 1);

    // The exact-bytes contract (spec §6.2): digest + length of the bytes
    // the SERVER received, not any re-serialization.
    let received = bodies.lock().unwrap();
    assert_eq!(received.len(), 1);
    let wire = r.wire.as_ref().expect("wire metadata populated");
    assert_eq!(wire.byte_len, received[0].len() as u64);
    assert_eq!(wire.digest, expected_wire_digest(&h.key_path, &received[0]));
}

// ─── Terminal failure ───────────────────────────────────────────────────────

#[tokio::test]
async fn non_retryable_failure_emits_one_terminal_record() {
    // 400 is non-retryable: exactly one attempt, one record.
    let (url, _bodies, hits) = spawn_stub(vec![400, 400, 400]).await;
    let h = harness(url);

    let err = drive_stream(&h.options, 3, &CancellationToken::new()).await;
    assert!(err.is_err(), "400 must surface as a typed failure");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "failure must emit exactly one record");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.attempt, 1);
    assert_eq!(r.outcome.http_status, Some(400));
    match &r.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_400"),
        other => panic!("expected ProviderFailed terminal, got {other:?}"),
    }
    assert!(r.outcome.timings.headers_ms.is_some());
    assert!(r.outcome.usage.is_none(), "no fabricated usage on failure");
}

// ─── Retry: one record per actual attempt ───────────────────────────────────

#[tokio::test]
async fn retry_emits_one_record_per_attempt_with_shared_request_id() {
    let (url, bodies, hits) = spawn_stub(vec![500]).await;
    let h = harness(url);

    drive_stream(&h.options, 2, &CancellationToken::new())
        .await
        .expect("retry fixture must eventually succeed");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let records = h.sink.records();
    assert_eq!(records.len(), 2, "one record per actual attempt");
    assert_schema_valid_and_content_free(&records);

    let (first, second) = (&records[0], &records[1]);
    assert_eq!(
        first.request_id, second.request_id,
        "attempts of one request share its request id"
    );
    assert_eq!(first.attempt, 1);
    assert_eq!(second.attempt, 2);

    // First attempt: failed, no retries listed yet (attempt == retries+1).
    assert!(first.outcome.retries.is_empty());
    match &first.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_500"),
        other => panic!("expected ProviderFailed on attempt 1, got {other:?}"),
    }
    assert_eq!(first.outcome.http_status, Some(500));

    // Second attempt: carries the prior failed try, terminal success.
    assert_eq!(second.outcome.retries.len(), 1);
    assert_eq!(second.outcome.retries[0].attempt, 1);
    assert_eq!(
        second.outcome.retries[0].class,
        super::RetryClass::ServerError
    );
    assert_eq!(second.outcome.terminal, TurnOutcome::Completed);

    // Both attempts sent identical bytes; both records carry their digest.
    let received = bodies.lock().unwrap();
    assert_eq!(received[0], received[1], "retries resend identical bytes");
    for r in &records {
        let wire = r.wire.as_ref().expect("wire populated on every attempt");
        assert_eq!(wire.byte_len, received[0].len() as u64);
        assert_eq!(wire.digest, expected_wire_digest(&h.key_path, &received[0]));
    }
}

// ─── Cancellation ───────────────────────────────────────────────────────────

#[tokio::test]
async fn cancellation_mid_stream_emits_one_canceled_record() {
    // Stream that never finishes: message_start, then endless SSE pings.
    let app = Router::new().route(
        "/v1/messages",
        axum_post(|| async {
            let first = Bytes::from(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\
                 \"usage\":{\"input_tokens\":5,\"output_tokens\":0,\
                 \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
            );
            let stream = futures::stream::unfold(0u64, move |i| {
                let first = first.clone();
                async move {
                    if i == 0 {
                        return Some((Ok::<_, std::convert::Infallible>(first), 1));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Some((Ok(Bytes::from(": ping\n\n")), i + 1))
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }),
    );
    let url = serve(app).await;
    let h = harness(url);

    let cancel = CancellationToken::new();
    let canceler = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        canceler.cancel();
    });

    let result = drive_stream(&h.options, 0, &cancel).await;
    assert!(result.is_ok(), "cancellation is not an error: {result:?}");

    let records = h.sink.records();
    assert_eq!(
        records.len(),
        1,
        "cancellation must emit exactly one record"
    );
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.terminal, TurnOutcome::Canceled);
    assert_eq!(r.attempt, 1);
    assert!(r.outcome.timings.stream_end_ms.is_some());
    assert!(
        r.outcome.stop_reason.is_none(),
        "no stop reason was observed — must stay absent"
    );
}

// ─── Timing buckets ─────────────────────────────────────────────────────────

#[tokio::test]
async fn timing_buckets_from_independently_delayed_stages() {
    const STAGE_MS: u64 = 80;
    const TOL_MS: u64 = 60; // tolerant lower bound per stage
    let app = Router::new().route(
        "/v1/messages",
        axum_post(|| async {
            // Stage 1: delay headers.
            tokio::time::sleep(Duration::from_millis(STAGE_MS)).await;
            let stream = futures::stream::unfold(0u8, |i| async move {
                match i {
                    // Stage 2: delay first body byte (a non-event comment).
                    0 => {
                        tokio::time::sleep(Duration::from_millis(STAGE_MS)).await;
                        Some((
                            Ok::<_, std::convert::Infallible>(Bytes::from(": warmup\n\n")),
                            1,
                        ))
                    }
                    // Stage 3: delay first model event.
                    1 => {
                        tokio::time::sleep(Duration::from_millis(STAGE_MS)).await;
                        Some((Ok(Bytes::from(SSE_SUCCESS)), 2))
                    }
                    _ => None,
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }),
    );
    let url = serve(app).await;
    let h = harness(url);

    drive_stream(&h.options, 0, &CancellationToken::new())
        .await
        .expect("delayed fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let t = records[0].outcome.timings;
    let headers = t.headers_ms.expect("headers stage observed");
    let first_byte = t.first_byte_ms.expect("first byte stage observed");
    let first_event = t.first_model_event_ms.expect("first model event observed");
    let end = t.stream_end_ms.expect("stream end observed");

    assert!(headers >= TOL_MS, "headers delayed: {headers}ms");
    assert!(
        first_byte >= headers + TOL_MS,
        "first byte distinctly after headers: {headers} → {first_byte}"
    );
    assert!(
        first_event >= first_byte + TOL_MS,
        "first model event distinctly after first byte: {first_byte} → {first_event}"
    );
    assert!(end >= first_event, "stream end ordered last");
    assert!(t.send_start_unix_ms.is_some());
}

// ─── Key I/O failure never affects the request ──────────────────────────────

#[tokio::test]
async fn key_failure_degrades_trace_but_never_the_request() {
    let (url, _bodies, _hits) = spawn_stub(vec![]).await;
    let tmp = tempfile::TempDir::new().unwrap();
    // Plant a regular FILE where the key's parent directory must go — key
    // creation cannot succeed.
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"not a dir").unwrap();
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink.clone()).with_key_path(blocker.join("digest.key"));
    let options = ApiOptions {
        anthropic_base_url: Some(url),
        trace: trace.clone(),
        ..Default::default()
    };

    drive_stream(&options, 0, &CancellationToken::new())
        .await
        .expect("key I/O failure must never fail the request");

    let records = sink.records();
    assert_eq!(records.len(), 1, "record still emitted, degraded");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert!(
        r.wire.is_none(),
        "no digest key → no wire digest, never fake"
    );
    assert!(r.system_segments.is_empty());
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    assert!(trace.degraded_records() >= 1, "degradation is counted");
}

// ─── Sync (non-streaming) path: call_api ────────────────────────────────────

#[tokio::test]
async fn sync_call_api_emits_one_record_from_exact_bytes() {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/messages",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            async move {
                bodies.lock().unwrap().push(body.to_vec());
                (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("request-id", "req_sync_1"),
                    ],
                    json!({
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 7, "output_tokens": 2}
                    })
                    .to_string(),
                )
            }
        }),
    );
    let url = serve(app).await;
    let h = harness(url);

    let client = Client::new();
    let tools = ToolRegistry::new();
    let messages = vec![Arc::new(json!({
        "role": "user",
        "content": format!("{SENTINEL} sync"),
    }))];
    ApiMethods::call_api(
        &auth(),
        &client,
        "claude-haiku-4-5",
        &tools,
        &Some(format!("system {SENTINEL}")),
        0,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &messages,
        0,
        &h.options,
    )
    .await
    .expect("sync call must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(r.outcome.http_status, Some(200));
    assert_eq!(r.outcome.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(
        r.outcome.usage.and_then(|u| u.input_tokens),
        Some(7),
        "usage from the provider JSON body"
    );
    assert_eq!(
        r.outcome.provider_request_id.as_ref().map(|id| id.as_str()),
        Some("req_sync_1")
    );
    let received = bodies.lock().unwrap();
    let wire = r.wire.as_ref().expect("wire populated");
    assert_eq!(wire.byte_len, received[0].len() as u64);
    assert_eq!(wire.digest, expected_wire_digest(&h.key_path, &received[0]));
}

// ─── Compaction path: call_api_simple ───────────────────────────────────────

#[tokio::test]
async fn compaction_call_api_simple_emits_one_record_from_exact_bytes() {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/messages",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            async move {
                bodies.lock().unwrap().push(body.to_vec());
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    json!({
                        "content": [{"type": "text", "text": "summary"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 3, "output_tokens": 1}
                    })
                    .to_string(),
                )
            }
        }),
    );
    let url = serve(app).await;
    let h = harness(url);

    let client = Client::new();
    let messages = vec![Arc::new(json!({
        "role": "user",
        "content": format!("{SENTINEL} compact this"),
    }))];
    let text = ApiMethods::call_api_simple(
        &auth(),
        &client,
        "claude-haiku-4-5",
        &format!("summarize {SENTINEL}"),
        0,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &messages,
        0,
        &h.options,
    )
    .await
    .expect("compaction call must succeed");
    assert_eq!(text, "summary");

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "compaction is traced too");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    let received = bodies.lock().unwrap();
    let wire = r.wire.as_ref().expect("wire populated");
    assert_eq!(wire.byte_len, received[0].len() as u64);
    assert_eq!(wire.digest, expected_wire_digest(&h.key_path, &received[0]));
}
