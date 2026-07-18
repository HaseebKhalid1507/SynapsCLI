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
use crate::StreamEvent;
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
    let cfg = ProviderConfig {
        base_url: upstream.to_string(),
        model: "test-model".to_string(),
        provider: "local".to_string(),
    };
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
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
        cancel,
        trace,
        exact,
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
    let canceler = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        canceler.cancel();
    });

    let err = drive_chat(&upstream, broker, &h.trace, true, &cancel)
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
    const TAIL_ONLY_SSE: &str =
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\r";
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
    let cfg = ProviderConfig {
        base_url: base_url.to_string(),
        model: "gpt-5.6-sol".to_string(),
        provider: "openai-codex".to_string(),
    };
    let broker: Arc<dyn CredentialBroker> = Arc::new(TokenOnlyBroker);
    let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
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
    let canceler = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        canceler.cancel();
    });

    drive_codex(&upstream, &h.trace, 0, &cancel)
        .await
        .expect_err("cancellation surfaces as request canceled");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_schema_valid_and_content_free(&records);
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert_eq!(records[0].outcome.http_status, Some(200));
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
