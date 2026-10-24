//! Task 10A wiring tests: the OpenAI-compatible transports (Chat
//! Completions via broker, xAI Responses via broker, Codex Responses direct)
//! emit one schema-valid `synaps-request-trace/1` record per actual attempt,
//! with wire metadata computed from the exact bytes the loopback server
//! received. All fixtures are local loopback servers — no real network.

use super::openai::StreamAttempt;
use super::{CollectingTraceSink, RequestTrace, StopReason, TraceContext, TransportKind};
use crate::runtime::openai::stream::{
    call_codex_stream_inner, call_oai_stream_inner, call_xai_responses_stream_inner,
};
use crate::runtime::openai::types::ProviderConfig;
use crate::{LlmEvent, StreamEvent};
use agent_core::auth::{
    AccessToken, BrokerError, CredentialBroker, LocalBroker, ProxyByteStream, ProxyRequest,
    ProxyResponse, RemoteBroker, TokenCache,
};
use agent_core::TurnOutcome;
use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post as axum_post;
use axum::Router;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "OAI_TRACE_SENTINEL_5eed_never_persist";

type CapturedBodies = Arc<Mutex<Vec<Vec<u8>>>>;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Harness {
    sink: Arc<CollectingTraceSink>,
    trace: TraceContext,
    key_path: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn harness() -> Harness {
    let tmp = tempfile::TempDir::new().unwrap();
    let key_path = tmp.path().join("trace").join("digest.key");
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink.clone()).with_key_path(key_path.clone());
    Harness {
        sink,
        trace,
        key_path,
        _tmp: tmp,
    }
}

fn messages() -> Vec<crate::SharedMessage> {
    vec![Arc::new(json!({
        "role": "user",
        "content": format!("{SENTINEL} hello"),
    }))]
}

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

fn assert_exact_wire(r: &RequestTrace, key_path: &std::path::Path, received: &[u8]) {
    let wire = r.wire.as_ref().expect("wire metadata populated");
    assert_eq!(wire.byte_len, received.len() as u64, "exact wire length");
    assert_eq!(
        wire.digest,
        expected_wire_digest(key_path, received),
        "digest of the exact bytes the server received"
    );
}

/// Event channel whose receiver cancels `cancel` once the client has
/// forwarded its first model text delta. Cancel tests key off this observed
/// mid-stream point, never a wall-clock timer: a timer can fire before or
/// during the send on a loaded runner, where no record is (correctly)
/// emitted, so "mid-stream" would not be what the test exercised.
fn cancel_on_first_text(cancel: &CancellationToken) -> mpsc::UnboundedSender<StreamEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    let canceler = cancel.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, StreamEvent::Llm(LlmEvent::Text(_))) {
                canceler.cancel();
                break;
            }
        }
    });
    tx
}

/// Hang guard for the endless-stream cancel fixtures: if the trigger event
/// never arrives the test fails instead of blocking forever.
async fn within_deadline<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("trigger event never observed: stream did not reach mid-stream")
}

/// Loopback upstream that records the full request body arrived (the send
/// happened), then never answers: the client is left awaiting headers.
async fn spawn_headerless_stub(path: &str) -> (String, Arc<tokio::sync::Notify>) {
    let received = Arc::new(tokio::sync::Notify::new());
    let received_c = Arc::clone(&received);
    let app = Router::new().route(
        path,
        axum_post(move |_body: Bytes| {
            let received = Arc::clone(&received_c);
            async move {
                received.notify_one();
                std::future::pending::<()>().await;
                StatusCode::OK
            }
        }),
    );
    (serve(app).await, received)
}

fn cancel_on(received: Arc<tokio::sync::Notify>, cancel: &CancellationToken) {
    let canceler = cancel.clone();
    tokio::spawn(async move {
        received.notified().await;
        canceler.cancel();
    });
}

// ═══ Chat Completions (broker-proxied) ══════════════════════════════════════

const CHAT_SSE_SUCCESS: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,",
    "\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n",
    "data: [DONE]\n\n",
);

async fn spawn_chat_stub(fail_status: Option<u16>) -> (String, CapturedBodies) {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().route(
        "/chat/completions",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            async move {
                bodies.lock().unwrap().push(body.to_vec());
                match fail_status {
                    Some(status) => (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        format!("{{\"error\":{{\"message\":\"ECHO {SENTINEL}\"}}}}"),
                    )
                        .into_response(),
                    None => (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        CHAT_SSE_SUCCESS.to_string(),
                    )
                        .into_response(),
                }
            }
        }),
    );
    (serve(app).await, bodies)
}

#[allow(clippy::too_many_arguments)]
async fn drive_chat(
    upstream: &str,
    broker: Arc<dyn CredentialBroker>,
    trace: &TraceContext,
    exact: bool,
    cancel: &CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
    drive_chat_with_events(upstream, broker, trace, exact, cancel, tx).await
}

/// `drive_chat` with a caller-owned event channel.
async fn drive_chat_with_events(
    upstream: &str,
    broker: Arc<dyn CredentialBroker>,
    trace: &TraceContext,
    exact: bool,
    cancel: &CancellationToken,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = ProviderConfig {
        base_url: upstream.to_string(),
        model: "test-model".to_string(),
        provider: "local".to_string(),
    };
    call_oai_stream_inner(
        &cfg,
        &broker,
        &[],
        &Some(format!("system {SENTINEL}")),
        &messages(),
        &tx,
        None,
        None,
        0,
        agent_core::reasoning::ReasoningLevel::Medium,
        cancel,
        trace,
        exact,
        false,
        0,
    )
    .await
}

#[tokio::test]
async fn chat_success_emits_one_record_with_exact_wire_digest() {
    let (upstream, bodies) = spawn_chat_stub(None).await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));

    drive_chat(&upstream, broker, &h.trace, true, &CancellationToken::new())
        .await
        .expect("chat success fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "success must emit exactly one record");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.attempt, 1);
    assert!(r.outcome.retries.is_empty());
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(r.transport, TransportKind::OpenAiChatCompletions);
    assert_eq!(r.model.as_str(), "local/test-model");
    assert_eq!(r.endpoint.path(), "/chat/completions");
    // Broker path: upstream HTTP status is not observed by this process.
    assert_eq!(r.outcome.http_status, None);
    assert_eq!(r.outcome.stop_reason, Some(StopReason::EndTurn));
    let usage = r.outcome.usage.expect("usage observed from stream");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(usage.cache_read_tokens, Some(2));
    assert_eq!(r.anatomy.message_count, 1);
    assert_eq!(r.anatomy.system_segment_count, 1);
    // Cache boundaries are not representable on the OpenAI wire.
    assert!(r.cache.boundaries.is_empty());

    let t = &r.outcome.timings;
    assert!(t.send_start_unix_ms.is_some());
    let headers = t.headers_ms.expect("headers observed");
    let first_byte = t.first_byte_ms.expect("first byte observed");
    let first_event = t.first_model_event_ms.expect("first model event observed");
    let end = t.stream_end_ms.expect("stream end observed");
    assert!(headers <= first_byte && first_byte <= first_event && first_event <= end);

    let received = bodies.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_exact_wire(r, &h.key_path, &received[0]);
}

#[tokio::test]
async fn chat_upstream_500_emits_one_failure_record_with_status() {
    let (upstream, _bodies) = spawn_chat_stub(Some(500)).await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));

    let err = drive_chat(&upstream, broker, &h.trace, true, &CancellationToken::new())
        .await
        .expect_err("500 must surface as a failure");
    assert!(!err.to_string().contains(SENTINEL), "body leak: {err}");

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "failure must emit exactly one record");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.attempt, 1);
    assert_eq!(r.outcome.http_status, Some(500));
    match &r.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_500"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert!(r.outcome.usage.is_none(), "no fabricated usage on failure");
}

#[tokio::test]
async fn chat_cancel_mid_stream_emits_one_canceled_record() {
    // Endless SSE ping stream.
    let app = Router::new().route(
        "/chat/completions",
        axum_post(|| async {
            let stream = futures::stream::unfold(0u64, |i| async move {
                if i == 0 {
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
                        )),
                        1,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                Some((Ok(Bytes::from(": ping\n\n")), i + 1))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let upstream = serve(app).await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));
    let cancel = CancellationToken::new();
    let tx = cancel_on_first_text(&cancel);

    let err = within_deadline(drive_chat_with_events(
        &upstream, broker, &h.trace, true, &cancel, tx,
    ))
    .await
    .expect_err("cancellation surfaces as request canceled");
    assert!(err.to_string().contains("canceled"));

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "cancel must emit exactly one record");
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.terminal, TurnOutcome::Canceled);
    assert_eq!(r.attempt, 1);
    assert!(r.outcome.timings.stream_end_ms.is_some());
    assert!(r.outcome.stop_reason.is_none(), "unobserved must stay None");
}

/// Follow-up evidence, not fixed here: the broker chat path awaits
/// `proxy_stream` under `await_or_cancel` and maps a cancel to an error
/// without finishing the attempt, so a send canceled before headers leaves
/// no record despite `trace::emit`'s one-record-per-send rule.
#[tokio::test]
#[ignore = "known gap: cancel while awaiting headers emits no record (follow-up)"]
async fn chat_cancel_while_awaiting_headers_emits_one_canceled_record() {
    let (upstream, received) = spawn_headerless_stub("/chat/completions").await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));
    let cancel = CancellationToken::new();
    cancel_on(received, &cancel);

    let err = within_deadline(drive_chat(&upstream, broker, &h.trace, true, &cancel))
        .await
        .expect_err("cancellation surfaces as request canceled");
    assert!(err.to_string().contains("canceled"), "{err}");
    let records = h.sink.records();
    assert_eq!(records.len(), 1, "a sent attempt must still be recorded");
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
}

#[tokio::test]
async fn chat_mid_stream_transport_error_emits_one_failure_record() {
    // Stream that yields one chunk then a transport error.
    let app = Router::new().route(
        "/chat/completions",
        axum_post(|| async {
            let stream = futures::stream::unfold(0u8, |i| async move {
                match i {
                    0 => Some((
                        Ok::<_, std::io::Error>(Bytes::from(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                        )),
                        1,
                    )),
                    1 => {
                        // Let the first chunk flush before killing the body.
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Some((Err(std::io::Error::other("connection reset by fixture")), 2))
                    }
                    _ => None,
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let upstream = serve(app).await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));

    drive_chat(&upstream, broker, &h.trace, true, &CancellationToken::new())
        .await
        .expect_err("mid-stream transport death must fail");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    match &records[0].outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "stream_error"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert!(records[0].outcome.timings.first_byte_ms.is_some());
}

#[tokio::test]
async fn chat_via_remote_broker_is_cloud_proxy_without_wire_claim() {
    // Fake remote broker daemon: answers /proxy with SSE success.
    let app = Router::new().route(
        "/proxy",
        axum_post(|| async {
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                CHAT_SSE_SUCCESS.to_string(),
            )
        }),
    );
    let endpoint = serve(app).await;
    let h = harness();
    let broker: Arc<dyn CredentialBroker> = Arc::new(RemoteBroker::new(
        endpoint.clone(),
        "machine-only",
        reqwest::Client::new(),
        TokenCache::new(),
    ));

    drive_chat(
        &endpoint,
        broker,
        &h.trace,
        false,
        &CancellationToken::new(),
    )
    .await
    .expect("remote-broker chat fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(
        r.transport,
        TransportKind::CloudProxy,
        "remote broker send is honestly a proxy hop"
    );
    assert!(
        r.wire.is_none(),
        "upstream bytes are serialized out of process — no wire claim"
    );
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
}

#[tokio::test]
async fn disabled_context_emits_nothing_and_still_succeeds() {
    let (upstream, _bodies) = spawn_chat_stub(None).await;
    let sink = CollectingTraceSink::new();
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::with_local_base_url(
        reqwest::Client::new(),
        upstream.clone(),
    ));
    drive_chat(
        &upstream,
        broker,
        &TraceContext::disabled(),
        true,
        &CancellationToken::new(),
    )
    .await
    .expect("disabled tracing never affects the request");
    assert!(sink.records().is_empty());
}

// ═══ xAI Responses (broker-proxied) ═════════════════════════════════════════

/// Test broker mirroring the `LocalBroker` exact-byte contract for the
/// `xai-auth` provider (whose production base URL is pinned to api.x.ai):
/// forwards `body_bytes` verbatim to the loopback upstream and flattens
/// upstream HTTP failures into the redacted status-only transport error.
struct LoopbackXaiBroker {
    upstream: String,
}

#[async_trait]
impl CredentialBroker for LoopbackXaiBroker {
    async fn access_token(
        &self,
        _p: agent_core::auth::OAuthProviderId,
    ) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("not used".into()))
    }
    async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not used".into()))
    }
    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        let bytes = request
            .body_bytes
            .expect("transport must hand the broker exact serialized bytes");
        let resp = reqwest::Client::new()
            .post(format!("{}{}", self.upstream, request.path))
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(format!("request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            drop(resp);
            return Err(BrokerError::Transport(format!(
                "provider request failed: {status}"
            )));
        }
        use futures::StreamExt;
        Ok(Box::pin(resp.bytes_stream().map(|chunk| {
            chunk.map_err(|e| BrokerError::Transport(format!("stream error: {e}")))
        })))
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not used".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

const RESPONSES_SSE_SUCCESS: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":",
    "{\"input_tokens\":7,\"output_tokens\":2,",
    "\"input_tokens_details\":{\"cached_tokens\":1}}}}\n\n",
    "data: [DONE]\n\n",
);

/// Responses stub that always succeeds with a caller-chosen SSE byte body —
/// used to exercise decoder edge shapes (e.g. trailing-buffer-only events).
async fn spawn_responses_stub_sse(sse: &'static str) -> String {
    let app = Router::new().route(
        "/responses",
        axum_post(move |_body: Bytes| async move {
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                sse.to_string(),
            )
                .into_response()
        }),
    );
    serve(app).await
}

async fn spawn_responses_stub(fail_status: Option<u16>) -> (String, CapturedBodies) {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().route(
        "/responses",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            async move {
                bodies.lock().unwrap().push(body.to_vec());
                match fail_status {
                    Some(status) => (
                        StatusCode::from_u16(status).unwrap(),
                        format!("{{\"error\":\"{SENTINEL}\"}}"),
                    )
                        .into_response(),
                    None => (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        RESPONSES_SSE_SUCCESS.to_string(),
                    )
                        .into_response(),
                }
            }
        }),
    );
    (serve(app).await, bodies)
}

async fn drive_responses(
    upstream: &str,
    trace: &TraceContext,
    cancel: &CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = ProviderConfig {
        base_url: upstream.to_string(),
        model: "grok-4.5".to_string(),
        provider: "xai-auth".to_string(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(LoopbackXaiBroker {
        upstream: upstream.to_string(),
    });
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
    call_xai_responses_stream_inner(
        &cfg,
        &broker,
        &[],
        &Some(format!("system {SENTINEL}")),
        &messages(),
        &tx,
        None,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        cancel,
        0,
        trace,
        true,
    )
    .await
}

#[tokio::test]
async fn responses_success_emits_one_record_with_exact_wire_digest() {
    let (upstream, bodies) = spawn_responses_stub(None).await;
    let h = harness();

    drive_responses(&upstream, &h.trace, &CancellationToken::new())
        .await
        .expect("responses success fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.transport, TransportKind::OpenAiResponses);
    assert_eq!(r.model.as_str(), "xai-auth/grok-4.5");
    assert_eq!(r.endpoint.path(), "/responses");
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    let usage = r.outcome.usage.expect("usage observed from stream");
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(2));
    assert_eq!(usage.cache_read_tokens, Some(1));
    assert!(
        r.outcome.stop_reason.is_none(),
        "responses stop reason is not yet observed — must stay None, never guessed"
    );
    let received = bodies.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_exact_wire(r, &h.key_path, &received[0]);
}

#[tokio::test]
async fn responses_tail_only_model_event_marks_first_model_event() {
    // The only model event dispatches in the TRAILING buffer: the stream
    // ends `…}\n\r` with no final newline, so the blank line that flushes
    // the buffered payload is only ever seen by the tail `push_line` (after
    // the chunk loop) — and `first_model_event_ms` must still be marked.
    const TAIL_ONLY_SSE: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: [DONE]\n\r",
    );
    let upstream = spawn_responses_stub_sse(TAIL_ONLY_SSE).await;
    let h = harness();

    let out = drive_responses(&upstream, &h.trace, &CancellationToken::new())
        .await
        .expect("tail-only stream must succeed");
    assert_eq!(
        out.pointer("/content/0/text").and_then(Value::as_str),
        Some("hi"),
        "the tail-flushed delta must reach the response"
    );

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let t = &records[0].outcome.timings;
    assert!(
        t.first_model_event_ms.is_some(),
        "trailing-buffer model event must mark first_model_event_ms"
    );
    assert!(t.stream_end_ms.is_some());
}

#[tokio::test]
async fn responses_failure_emits_one_record_with_status() {
    let (upstream, _bodies) = spawn_responses_stub(Some(429)).await;
    let h = harness();

    let err = drive_responses(&upstream, &h.trace, &CancellationToken::new())
        .await
        .expect_err("429 must fail");
    assert!(!err.to_string().contains(SENTINEL), "body leak: {err}");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.http_status, Some(429));
    match &r.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_429"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
}

// ═══ Codex Responses (direct HTTP, persistent retries) ══════════════════════

const CODEX_SSE_SUCCESS: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":",
    "{\"input_tokens\":5,\"output_tokens\":4}}}\n\n",
    "data: [DONE]\n\n",
);

fn fake_codex_token() -> String {
    let payload = json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct_test" }
    });
    format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

struct TokenOnlyBroker;

#[async_trait]
impl CredentialBroker for TokenOnlyBroker {
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
        Err(BrokerError::Denied("not used".into()))
    }
    async fn proxy_stream(&self, _request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not used".into()))
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not used".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

/// First `fail_statuses.len()` POSTs answer those statuses, then SSE
/// success. Captures every received body byte-exactly.
async fn spawn_codex_stub(fail_statuses: Vec<u16>) -> (String, CapturedBodies, Arc<AtomicUsize>) {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies_c = Arc::clone(&bodies);
    let hits_c = Arc::clone(&hits);
    let app = Router::new().route(
        "/codex/responses",
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
                        [("content-type", "application/json")],
                        format!("{{\"error\":\"{SENTINEL}\"}}"),
                    )
                        .into_response(),
                    None => (
                        StatusCode::OK,
                        [
                            ("content-type", "text/event-stream"),
                            ("request-id", "req_codex_42"),
                        ],
                        CODEX_SSE_SUCCESS.to_string(),
                    )
                        .into_response(),
                }
            }
        }),
    );
    (serve(app).await, bodies, hits)
}

async fn drive_codex(
    base_url: &str,
    trace: &TraceContext,
    max_retries: u32,
    cancel: &CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
    drive_codex_with_events(base_url, trace, max_retries, cancel, tx).await
}

/// `drive_codex` with a caller-owned event channel.
async fn drive_codex_with_events(
    base_url: &str,
    trace: &TraceContext,
    max_retries: u32,
    cancel: &CancellationToken,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let cfg = ProviderConfig {
        base_url: base_url.to_string(),
        model: "gpt-5.6-sol".to_string(),
        provider: "openai-codex".to_string(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(TokenOnlyBroker);
    call_codex_stream_inner(
        &cfg,
        &reqwest::Client::new(),
        &broker,
        &[],
        &Some(format!("system {SENTINEL}")),
        &messages(),
        &tx,
        None,
        None,
        agent_core::reasoning::ReasoningLevel::Medium,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        cancel,
        max_retries,
        trace,
    )
    .await
}

#[tokio::test]
async fn codex_success_emits_one_record_with_status_and_request_id() {
    let (upstream, bodies, _hits) = spawn_codex_stub(vec![]).await;
    let h = harness();

    drive_codex(&upstream, &h.trace, 0, &CancellationToken::new())
        .await
        .expect("codex success fixture must succeed");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.transport, TransportKind::OpenAiResponses);
    assert_eq!(r.model.as_str(), "openai-codex/gpt-5.6-sol");
    assert_eq!(r.endpoint.path(), "/codex/responses");
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    // Direct HTTP: status and provider request ID are observed.
    assert_eq!(r.outcome.http_status, Some(200));
    assert_eq!(
        r.outcome.provider_request_id.as_ref().map(|id| id.as_str()),
        Some("req_codex_42")
    );
    let usage = r.outcome.usage.expect("usage observed from stream");
    assert_eq!(usage.input_tokens, Some(5));
    assert_eq!(usage.output_tokens, Some(4));
    let t = &r.outcome.timings;
    assert!(t.headers_ms.is_some());
    assert!(t.first_byte_ms.is_some());
    assert!(t.first_model_event_ms.is_some());
    assert!(t.stream_end_ms.is_some());

    let received = bodies.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_exact_wire(r, &h.key_path, &received[0]);
}

#[tokio::test]
async fn codex_system_guidance_is_sent_once_and_preserves_history_prefix() {
    let (upstream, bodies, _hits) = spawn_codex_stub(vec![]).await;
    let h = harness();
    let cfg = ProviderConfig {
        base_url: upstream,
        model: "gpt-5.6-sol".into(),
        provider: "openai-codex".into(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(TokenOnlyBroker);
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
    let system = Some(format!(
        "EFFECTIVE_SYSTEM_ONLY {}",
        crate::runtime::continuation::GUIDANCE
    ));
    let mut history: Vec<crate::SharedMessage> = vec![
        Arc::new(json!({"role": "user", "content": "Perform the authorized task. Do not deploy."})),
        Arc::new(json!({"role": "assistant", "content": "Retained result."})),
        Arc::new(json!({"role": "user", "content": "Finish the implementation."})),
    ];
    for turn in 0..2 {
        if turn == 1 {
            history.push(Arc::new(
                json!({"role": "assistant", "content": "First step done."}),
            ));
            history.push(Arc::new(
                json!({"role": "user", "content": "Continue remaining work."}),
            ));
        }
        call_codex_stream_inner(
            &cfg,
            &reqwest::Client::new(),
            &broker,
            &[],
            &system,
            &history,
            &tx,
            None,
            None,
            agent_core::reasoning::ReasoningLevel::Medium,
            crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            &CancellationToken::new(),
            0,
            &h.trace,
        )
        .await
        .expect("loopback Codex request");
    }
    let requests = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    for body in &requests {
        let instructions = body["instructions"].as_str().unwrap();
        assert_eq!(
            instructions
                .matches(crate::runtime::continuation::GUIDANCE)
                .count(),
            1
        );
        assert_eq!(body.to_string().matches("EFFECTIVE_SYSTEM_ONLY").count(), 1);
        assert_eq!(
            instructions
                .matches("[Synaps autonomous harness policy]")
                .count(),
            1
        );
        let input = body["input"].as_array().unwrap();
        assert!(
            !input.iter().any(|item| item["role"] == "system"),
            "no duplicate synthetic system message"
        );
        assert!(!body["input"]
            .to_string()
            .contains(crate::runtime::continuation::GUIDANCE));
        for (role, text) in [
            ("user", "Perform the authorized task. Do not deploy."),
            ("assistant", "Retained result."),
            ("user", "Finish the implementation."),
        ] {
            assert_eq!(
                input
                    .iter()
                    .filter(|item| item["role"] == role && item["content"] == text)
                    .count(),
                1
            );
        }
    }
    let prefix = requests[0]["input"].as_array().unwrap();
    assert_eq!(
        &requests[1]["input"].as_array().unwrap()[..prefix.len()],
        prefix.as_slice()
    );
    assert_eq!(
        requests[0]["prompt_cache_key"],
        requests[1]["prompt_cache_key"]
    );
}

#[tokio::test]
async fn codex_retry_emits_one_record_per_attempt_with_shared_request_id() {
    let (upstream, bodies, hits) = spawn_codex_stub(vec![500]).await;
    let h = harness();

    drive_codex(&upstream, &h.trace, 2, &CancellationToken::new())
        .await
        .expect("retry fixture must eventually succeed");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let records = h.sink.records();
    assert_eq!(records.len(), 2, "one record per actual attempt");
    assert_schema_valid_and_content_free(&records);
    let (first, second) = (&records[0], &records[1]);
    assert_eq!(first.request_id, second.request_id);
    assert_eq!(first.attempt, 1);
    assert_eq!(second.attempt, 2);
    assert!(first.outcome.retries.is_empty());
    match &first.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_500"),
        other => panic!("expected ProviderFailed on attempt 1, got {other:?}"),
    }
    assert_eq!(first.outcome.http_status, Some(500));
    assert!(
        first.outcome.timings.headers_ms.is_some(),
        "a received HTTP 500 has observed headers timing"
    );
    assert_eq!(second.outcome.retries.len(), 1);
    assert_eq!(second.outcome.retries[0].attempt, 1);
    assert_eq!(
        second.outcome.retries[0].class,
        super::RetryClass::ServerError
    );
    assert_eq!(second.outcome.terminal, TurnOutcome::Completed);

    let received = bodies.lock().unwrap();
    assert_eq!(received[0], received[1], "retries resend identical bytes");
    for r in &records {
        assert_exact_wire(r, &h.key_path, &received[0]);
    }
}

#[tokio::test]
async fn codex_terminal_400_emits_one_failure_record() {
    let (upstream, _bodies, hits) = spawn_codex_stub(vec![400, 400]).await;
    let h = harness();

    let err = drive_codex(&upstream, &h.trace, 3, &CancellationToken::new())
        .await
        .expect_err("400 must fail without retry");
    assert!(!err.to_string().contains(SENTINEL), "body leak: {err}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "400 is not retried");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    let r = &records[0];
    assert_eq!(r.outcome.http_status, Some(400));
    assert!(
        r.outcome.timings.headers_ms.is_some(),
        "a received HTTP 400 has observed headers timing"
    );
    match &r.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_400"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn codex_cancel_mid_stream_emits_one_canceled_record() {
    let app = Router::new().route(
        "/codex/responses",
        axum_post(|| async {
            let stream = futures::stream::unfold(0u64, |i| async move {
                if i == 0 {
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(
                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
                        )),
                        1,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                Some((Ok(Bytes::from(": ping\n\n")), i + 1))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let upstream = serve(app).await;
    let h = harness();
    let cancel = CancellationToken::new();
    let tx = cancel_on_first_text(&cancel);

    within_deadline(drive_codex_with_events(&upstream, &h.trace, 0, &cancel, tx))
        .await
        .expect_err("cancellation surfaces as request canceled");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert_eq!(records[0].outcome.http_status, Some(200));
}

/// Contrast with the Anthropic and broker-chat gaps: Codex's
/// `send_with_retries` finishes the attempt as canceled when the cancel
/// lands during the send, so the one-record-per-send rule holds here.
#[tokio::test]
async fn codex_cancel_while_awaiting_headers_emits_one_canceled_record() {
    let (upstream, received) = spawn_headerless_stub("/codex/responses").await;
    let h = harness();
    let cancel = CancellationToken::new();
    cancel_on(received, &cancel);

    within_deadline(drive_codex(&upstream, &h.trace, 0, &cancel))
        .await
        .expect_err("cancellation surfaces as request canceled");
    let records = h.sink.records();
    assert_eq!(records.len(), 1, "a sent attempt must still be recorded");
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert_eq!(records[0].outcome.http_status, None, "no headers observed");
}

// ═══ Unit coverage for the new helpers ══════════════════════════════════════

#[test]
fn broker_error_status_parses_only_safe_numeric_statuses() {
    use super::openai::broker_error_status;
    assert_eq!(
        broker_error_status("provider request failed: 503 Service Unavailable"),
        Some(503)
    );
    assert_eq!(
        broker_error_status(
            "openai request failed: provider request failed: 429 Too Many Requests"
        ),
        Some(429)
    );
    assert_eq!(
        broker_error_status("provider request failed: garbage"),
        None
    );
    assert_eq!(broker_error_status("unrelated error"), None);
    assert_eq!(broker_error_status("provider request failed: 99"), None);
}

#[test]
fn stream_attempt_without_tracer_is_inert() {
    // No panic, no record: the disabled path is a pile of no-ops.
    let mut attempt = StreamAttempt::new(None);
    attempt.mark_headers();
    attempt.mark_first_byte();
    attempt.mark_first_model_event();
    attempt.attempt_failed(
        super::RetryClass::ServerError,
        Duration::ZERO,
        Some(500),
        None,
        "http_500",
    );
    attempt.finish_success(None, None, None, None);
}

/// Exercises actual serialized POST bodies, including identical retry bytes.
#[tokio::test]
async fn astra_ultra_posts_xhigh_for_foreground_and_worker_without_recursive_mode() {
    use crate::runtime::openai::catalog::CodexRequestRole;
    use agent_core::reasoning::ReasoningLevel;
    for role in [CodexRequestRole::Foreground, CodexRequestRole::Worker] {
        let (base_url, bodies, _) = spawn_codex_stub(vec![503]).await;
        let cfg = ProviderConfig {
            base_url,
            model: "gpt-6-astra".into(),
            provider: "openai-codex".into(),
        };
        let broker: Arc<dyn CredentialBroker> = Arc::new(TokenOnlyBroker);
        let tools = if role == CodexRequestRole::Foreground {
            ["subagent_start", "subagent_status", "subagent_collect"].iter().map(|name| json!({"name":name, "description":"test", "input_schema":{"type":"object","properties":{}}})).collect::<Vec<_>>()
        } else {
            vec![]
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let h = harness();
        call_codex_stream_inner(
            &cfg,
            &reqwest::Client::new(),
            &broker,
            &tools,
            &Some("system".into()),
            &messages(),
            &tx,
            None,
            None,
            ReasoningLevel::Ultra,
            role,
            &CancellationToken::new(),
            1,
            &h.trace,
        )
        .await
        .unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1], "retries must preserve bytes");
        let body: Value = serde_json::from_slice(&bodies[0]).unwrap();
        assert_eq!(body["model"], "gpt-6-astra");
        assert_eq!(body["reasoning"], json!({"effort":"xhigh"}));
        assert_eq!(body["store"], false);
        assert_eq!(body["parallel_tool_calls"], true);
        assert!(body["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("synaps-"));
        assert_eq!(
            body["input"].to_string().contains("Proactive multi-agent"),
            role == CodexRequestRole::Foreground
        );
        assert_eq!(
            body["input"].to_string().contains("<multi_agent_mode>"),
            role == CodexRequestRole::Foreground
        );
        assert_eq!(h.sink.records().len(), 2);
    }
}

/// Opt-in: uses the operator's broker-owned Codex OAuth session and makes two
/// small real inference requests. Never prints credentials or response bodies.
#[tokio::test]
#[ignore = "live Codex OAuth smoke; consumes account usage"]
async fn astra_ultra_live_smoke() {
    use crate::runtime::openai::catalog::{
        fetch_catalog_models, plan_codex_execution, CatalogSource, CodexRequestRole,
        CodexWireEffort,
    };
    use agent_core::reasoning::ReasoningLevel;
    assert_eq!(
        std::env::var("SYNAPS_ASTRA_LIVE_SMOKE").as_deref(),
        Ok("1"),
        "explicit live-smoke opt-in required"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let models = fetch_catalog_models(&client, "openai-codex")
        .await
        .expect("live Codex catalog");
    let astra = models
        .iter()
        .find(|row| row.id == "gpt-6-astra")
        .expect("account must expose Astra");
    assert_eq!(astra.source, CatalogSource::Live);
    let cfg = ProviderConfig {
        base_url: "https://chatgpt.com/backend-api".into(),
        model: "gpt-6-astra".into(),
        provider: "openai-codex".into(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(LocalBroker::new(client.clone()));
    let messages = vec![Arc::new(
        json!({"role":"user", "content":"Reply with exactly ASTRA_ULTRA_OK. Do not use tools or delegate."}),
    )];
    for role in [CodexRequestRole::Foreground, CodexRequestRole::Worker] {
        // Uses the cache populated by the real catalog fetch, as production does.
        let plan =
            plan_codex_execution(&astra.runtime_id(), ReasoningLevel::Ultra, role, None).unwrap();
        assert_eq!(plan.wire_effort, Some(CodexWireEffort::XHigh));
        let tools = if role == CodexRequestRole::Foreground {
            ["subagent_start", "subagent_status", "subagent_collect"].iter().map(|name| json!({"name":name, "description":"Unavailable in smoke test; do not call", "input_schema":{"type":"object","properties":{}}})).collect::<Vec<_>>()
        } else {
            vec![]
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(90),
            call_codex_stream_inner(
                &cfg,
                &client,
                &broker,
                &tools,
                &Some("You are a terse assistant. No tools for this smoke test.".into()),
                &messages,
                &tx,
                None,
                None,
                ReasoningLevel::Ultra,
                role,
                &CancellationToken::new(),
                0,
                &TraceContext::disabled(),
            ),
        )
        .await
        .expect("live request deadline")
        .expect("live request succeeds");
        let blocks = result["content"].as_array().expect("assistant content");
        assert!(blocks.iter().all(|block| block["type"] != "tool_use"));
        let text: String = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        assert_eq!(text.trim(), "ASTRA_ULTRA_OK");
        println!(
            "Astra live smoke: role={} logical=ultra wire=xhigh result=OK",
            role.as_str()
        );
    }
}

// ═══ Codex 4xx diagnosis + hosted web_search visibility (07 I2 / M2, 01 I4) ═══

/// Every POST answers `status` with `body`. Loopback only.
async fn spawn_codex_body_stub(
    status: u16,
    content_type: &'static str,
    body: String,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = Router::new().route(
        "/codex/responses",
        axum_post(move || {
            let hits = Arc::clone(&hits_c);
            let body = body.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("content-type", content_type)],
                    body,
                )
                    .into_response()
            }
        }),
    );
    (serve(app).await, hits)
}

async fn drive_codex_collect(
    base_url: &str,
    trace: &TraceContext,
) -> (
    Result<Value, Box<dyn std::error::Error + Send + Sync>>,
    Vec<StreamEvent>,
) {
    drive_codex_model(
        base_url,
        trace,
        "gpt-5.6-sol",
        agent_core::reasoning::ReasoningLevel::Medium,
    )
    .await
}

async fn drive_codex_model(
    base_url: &str,
    trace: &TraceContext,
    model: &str,
    level: agent_core::reasoning::ReasoningLevel,
) -> (
    Result<Value, Box<dyn std::error::Error + Send + Sync>>,
    Vec<StreamEvent>,
) {
    drive_codex_model_tools(base_url, trace, model, level, &[]).await
}

async fn drive_codex_model_tools(
    base_url: &str,
    trace: &TraceContext,
    model: &str,
    level: agent_core::reasoning::ReasoningLevel,
    tools: &[Value],
) -> (
    Result<Value, Box<dyn std::error::Error + Send + Sync>>,
    Vec<StreamEvent>,
) {
    let cfg = ProviderConfig {
        base_url: base_url.to_string(),
        model: model.to_string(),
        provider: "openai-codex".to_string(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(TokenOnlyBroker);
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    let result = call_codex_stream_inner(
        &cfg,
        &reqwest::Client::new(),
        &broker,
        tools,
        &Some(format!("system {SENTINEL}")),
        &messages(),
        &tx,
        None,
        None,
        level,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        &CancellationToken::new(),
        3,
        trace,
    )
    .await;
    drop(tx);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (result, events)
}

#[tokio::test]
async fn codex_400_surfaces_sanitized_provider_error_in_message_and_trace() {
    let cases = [
        (
            // Unsupported reasoning effort.
            r#"{"error":{"message":"Unsupported value: 'reasoning.effort' does not support 'max' with this model.","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_value"}}"#.to_string(),
            "[invalid_request_error/unsupported_value, param reasoning.effort] — this model rejected the requested reasoning setting",
            "http_400:invalid_request_error:unsupported_value",
        ),
        (
            // Model not entitled on this ChatGPT seat (FastAPI `detail`).
            r#"{"detail":"The 'gpt-5.6-sol' model is not supported when using Codex with a ChatGPT account."}"#.to_string(),
            "400 Bad Request — this model isn't available on the selected ChatGPT account",
            "http_400",
        ),
        (
            // Hosted tool not allowed for the model.
            r#"{"error":{"message":"Tool 'web_search' is not supported with gpt-5.6-sol.","type":"invalid_request_error","param":"tools","code":null}}"#.to_string(),
            "[invalid_request_error, param tools] — the backend rejected a request tool definition",
            "http_400:invalid_request_error",
        ),
        (
            // A body that echoes request content: the echo never surfaces.
            format!(r#"{{"error":{{"message":"Invalid 'input[0].content': '{SENTINEL} hello and a lot more prompt text here'","type":"invalid_request_error","code":"invalid_value"}}}}"#),
            "400 Bad Request [invalid_request_error/invalid_value]",
            "http_400:invalid_request_error:invalid_value",
        ),
    ];
    for (body, expected_detail, expected_code) in cases {
        let (upstream, hits) = spawn_codex_body_stub(400, "application/json", body).await;
        let h = harness();
        let (result, _events) = drive_codex_collect(&upstream, &h.trace).await;
        let err = result.expect_err("400 must fail").to_string();
        assert!(
            err.starts_with("codex request failed: 400 Bad Request"),
            "{err}"
        );
        assert!(
            err.contains(expected_detail),
            "missing {expected_detail:?} in {err}"
        );
        assert!(!err.contains(SENTINEL), "body leak: {err}");
        assert!(!err.contains("acct_test"), "account id leak: {err}");
        // Identifiers and Synaps text only: no provider prose at all.
        for prose in [
            "Unsupported value",
            "is not supported",
            "Invalid '",
            "hello",
        ] {
            assert!(
                !err.contains(prose),
                "provider prose {prose:?} leaked: {err}"
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "400 is not retried");
        let records = h.sink.records();
        assert_eq!(records.len(), 1);
        assert_schema_valid_and_content_free(&records);
        match &records[0].outcome.terminal {
            TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, expected_code),
            other => panic!("expected ProviderFailed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn codex_hosted_web_search_call_is_announced_with_query() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"in_progress\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"rust reqwest read_timeout\"}}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"done\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    );
    let (upstream, _hits) = spawn_codex_body_stub(200, "text/event-stream", sse.to_string()).await;
    let h = harness();
    let (result, events) = drive_codex_collect(&upstream, &h.trace).await;
    result.expect("stream completes");
    let notices: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(crate::SessionEvent::Notice(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        vec!["🔎 Codex web_search (search, completed): rust reqwest read_timeout"],
        "exactly one notice, on the completed item"
    );
}

// ═══ Hosted web_search: learned rejection (prE round 3, I1 option b) ═══

const TOOLS_400: &str = r#"{"error":{"message":"Tool 'web_search' is not supported with this model.","type":"invalid_request_error","param":"tools","code":null}}"#;

/// `/codex/responses` stub: answers a tools-400 to any body carrying the
/// hosted web_search tool (or to every body when `always_reject`), SSE
/// success otherwise. Records every body.
async fn spawn_web_search_rejecting_stub(always_reject: bool) -> (String, CapturedBodies) {
    spawn_rejecting_stub_with(always_reject, TOOLS_400).await
}

async fn spawn_rejecting_stub_with(
    always_reject: bool,
    reject_body: &'static str,
) -> (String, CapturedBodies) {
    let bodies: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().route(
        "/codex/responses",
        axum_post(move |body: Bytes| {
            let bodies = Arc::clone(&bodies_c);
            async move {
                let has_search = body_has_web_search(&body);
                bodies.lock().unwrap().push(body.to_vec());
                if always_reject || has_search {
                    (
                        StatusCode::BAD_REQUEST,
                        [("content-type", "application/json")],
                        reject_body.to_string(),
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
    (serve(app).await, bodies)
}

fn body_has_web_search(body: &[u8]) -> bool {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    v["tools"]
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t["type"] == "web_search"))
}

fn notice_texts(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(crate::SessionEvent::Notice(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn web_search_on() -> crate::runtime::openai::codex_wire::EnvOverrideGuard {
    crate::runtime::openai::codex_wire::override_env_for_test(&[(
        "SYNAPS_CODEX_WEB_SEARCH",
        "live",
    )])
}

#[tokio::test]
async fn codex_tools_400_is_remembered_per_model_and_retried_once_without_search() {
    let _env = web_search_on();
    let (upstream, bodies) = spawn_web_search_rejecting_stub(false).await;
    let h = harness();
    let model = "gpt-zz-ws-retry";
    let level = agent_core::reasoning::ReasoningLevel::Adaptive;

    let (result, events) = drive_codex_model(&upstream, &h.trace, model, level).await;
    result.expect("the retry without web_search succeeds");
    {
        let sent = bodies.lock().unwrap();
        assert_eq!(sent.len(), 2, "one rejected send, one retry");
        assert!(body_has_web_search(&sent[0]));
        assert!(!body_has_web_search(&sent[1]), "retry drops web_search");
    }
    let notices = notice_texts(&events);
    assert_eq!(
        notices
            .iter()
            .filter(|n| n.contains("web_search (rejected by gpt-zz-ws-retry)"))
            .count(),
        1,
        "{notices:?}"
    );
    // Trace: the rejected attempt and the retry are both recorded.
    let records = h.sink.records();
    assert_schema_valid_and_content_free(&records);
    assert_eq!(records.len(), 2);

    // Remembered for this process: the next request never attaches it.
    let (result, events) = drive_codex_model(&upstream, &h.trace, model, level).await;
    result.expect("second request succeeds");
    let sent = bodies.lock().unwrap();
    assert_eq!(sent.len(), 3, "no rejected send the second time");
    assert!(!body_has_web_search(&sent[2]));
    assert!(
        notice_texts(&events)
            .iter()
            .all(|n| !n.contains("rejected")),
        "the rejection notice is one-time"
    );
}

#[tokio::test]
async fn codex_tools_400_is_never_retried_twice() {
    let _env = web_search_on();
    let (upstream, bodies) = spawn_web_search_rejecting_stub(true).await;
    let h = harness();
    let (result, _events) = drive_codex_model(
        &upstream,
        &h.trace,
        "gpt-zz-ws-loop",
        agent_core::reasoning::ReasoningLevel::Adaptive,
    )
    .await;
    let err = result
        .expect_err("a tools-400 without web_search is terminal")
        .to_string();
    assert_eq!(bodies.lock().unwrap().len(), 2, "exactly one retry");
    assert!(
        err.starts_with("codex request failed: 400 Bad Request"),
        "{err}"
    );
    // web_search was not attached on the failing send, so it isn't blamed.
    assert!(!err.contains("web_search"), "{err}");
    // The resend failed the same way: web_search wasn't the cause, so
    // nothing is learned and nothing is announced.
    assert_eq!(
        crate::runtime::openai::codex_wire::codex_web_search_unsupported_reason("gpt-zz-ws-loop"),
        None
    );
    assert!(
        notice_texts(&_events)
            .iter()
            .all(|n| !n.contains("rejected")),
        "{:?}",
        notice_texts(&_events)
    );
}

#[tokio::test]
async fn codex_bad_user_tool_schema_400_is_not_blamed_on_web_search() {
    let _env = web_search_on();
    const BAD_SCHEMA_400: &str = r#"{"error":{"message":"Invalid schema for function 'mcp_tool'.","type":"invalid_request_error","param":"tools[0].parameters","code":"invalid_function_parameters"}}"#;
    let (upstream, bodies) = spawn_rejecting_stub_with(true, BAD_SCHEMA_400).await;
    let h = harness();
    let user_tool = json!({
        "name": "mcp_tool",
        "description": "an MCP tool",
        "input_schema": {"type": "object", "properties": {}}
    });
    let (result, events) = drive_codex_model_tools(
        &upstream,
        &h.trace,
        "gpt-zz-ws-badschema",
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &[user_tool],
    )
    .await;
    let err = result.expect_err("the schema 400 surfaces").to_string();
    {
        let sent = bodies.lock().unwrap();
        assert_eq!(sent.len(), 1, "no resend for a user-tool rejection");
        assert!(
            body_has_web_search(&sent[0]),
            "web_search was attached at tools[1]"
        );
    }
    assert!(err.contains("param tools[0].parameters"), "{err}");
    assert_eq!(
        crate::runtime::openai::codex_wire::codex_web_search_unsupported_reason(
            "gpt-zz-ws-badschema"
        ),
        None
    );
    assert!(notice_texts(&events)
        .iter()
        .all(|n| !n.contains("rejected")));
}

#[tokio::test]
async fn codex_web_search_resend_is_traced_under_an_armed_one_shot_gate() {
    let _env = web_search_on();
    let (upstream, _bodies) = spawn_web_search_rejecting_stub(false).await;
    let h = harness();
    let armed = h.trace.clone().with_one_shot_request_gate();
    let (result, _) = drive_codex_model(
        &upstream,
        &armed,
        "gpt-zz-ws-oneshot",
        agent_core::reasoning::ReasoningLevel::Adaptive,
    )
    .await;
    result.expect("resend succeeds");
    let records = h.sink.records();
    assert_schema_valid_and_content_free(&records);
    assert_eq!(records.len(), 2, "rejected send and resend are both traced");
}

#[tokio::test]
async fn codex_spark_never_gets_web_search_and_lite_rows_still_do() {
    let _env = web_search_on();
    let (upstream, bodies) = spawn_web_search_rejecting_stub(false).await;
    let h = harness();
    let (result, _) = drive_codex_model(
        &upstream,
        &h.trace,
        "gpt-5.3-codex-spark",
        agent_core::reasoning::ReasoningLevel::Medium,
    )
    .await;
    result.expect("spark request succeeds");
    assert_eq!(bodies.lock().unwrap().len(), 1);
    assert!(!body_has_web_search(&bodies.lock().unwrap()[0]));

    // A Responses-Lite flagship (upstream use_responses_lite: true) still
    // gets the hosted tool: Synaps sends full Responses requests.
    let (upstream, bodies, _hits) = spawn_codex_stub(vec![]).await;
    let _ = drive_codex_model(
        &upstream,
        &h.trace,
        "gpt-5.6-terra",
        agent_core::reasoning::ReasoningLevel::Medium,
    )
    .await;
    assert!(body_has_web_search(&bodies.lock().unwrap()[0]));
}

#[tokio::test]
async fn codex_untraced_continuation_resend_stays_untraced_under_one_shot_gate() {
    let _env = web_search_on();
    let (upstream, bodies) = spawn_web_search_rejecting_stub(false).await;
    let h = harness();
    let armed = h.trace.clone().with_one_shot_request_gate();
    // Request 1 (spark: no web_search) holds the one-shot gate.
    let (result, _) = drive_codex_model(
        &upstream,
        &armed,
        "gpt-5.3-codex-spark",
        agent_core::reasoning::ReasoningLevel::Medium,
    )
    .await;
    result.expect("first request succeeds");
    // Request 2 is an untraced continuation that hits a web_search 400 and
    // resends; the resend must not borrow the spent gate.
    let (result, _) = drive_codex_model(
        &upstream,
        &armed,
        "gpt-zz-ws-untraced",
        agent_core::reasoning::ReasoningLevel::Adaptive,
    )
    .await;
    result.expect("resend succeeds");
    assert_eq!(
        bodies.lock().unwrap().len(),
        3,
        "1 + rejected send + resend"
    );
    let records = h.sink.records();
    assert_schema_valid_and_content_free(&records);
    assert_eq!(records.len(), 1, "only the gate-holding request is traced");
}
