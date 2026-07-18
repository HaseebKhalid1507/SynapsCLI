//! Task 10B wiring tests: the Google Gemini Code Assist transport and the
//! explicit cloud-invoke routes (AWS Bedrock, Azure OpenAI, Google Vertex)
//! emit one schema-valid `synaps-request-trace/1` record per actual broker
//! attempt. All brokers are in-process fakes — no real network. Text-only
//! cloud pre-flight failures happen before any broker work and emit no
//! record.

use super::{
    BlockKind, CollectingTraceSink, RequestTrace, StopReason, TraceContext, TranslationAction,
    TranslationElement, TransportKind, UsageProvenance,
};
use crate::auth::{
    AccessToken, BrokerError, CloudProviderId, CredentialBroker, OAuthProviderId, ProviderStatus,
    ProxyByteStream, ProxyRequest, ProxyResponse,
};
use crate::runtime::cloud_invoke::cloud_invoke_stream;
use crate::runtime::google_gemini::runtime::call_google_gemini_stream_inner;
use crate::runtime::openai::types::ProviderConfig;
use agent_core::auth::broker::{CloudEvent, CloudEventStream};
use agent_core::auth::InvokeRequest;
use agent_core::TurnOutcome;
use async_trait::async_trait;
use futures::stream;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "GOOGLE_TRACE_SENTINEL_7c1d_never_persist";

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

// ═══ Gemini Code Assist (broker-proxied stream) ═════════════════════════════

const SETUP_BODY: &str = r#"{"cloudaicompanionProject":"test-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#;

/// Fake broker: answers `loadCodeAssist` for project setup, then serves the
/// configured chunk script per `proxy_stream` call (one script per attempt).
struct GeminiBroker {
    /// Per-attempt outcomes: `Err(text)` rejects the stream open with a
    /// broker transport error; `Ok(chunks)` serves the byte chunks.
    attempts: Mutex<Vec<Result<Vec<Result<bytes::Bytes, BrokerError>>, String>>>,
    seen: Mutex<Vec<ProxyRequest>>,
}

impl GeminiBroker {
    fn new(attempts: Vec<Result<Vec<Result<bytes::Bytes, BrokerError>>, String>>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(attempts),
            seen: Mutex::new(Vec::new()),
        })
    }
    fn stream_calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl CredentialBroker for GeminiBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::NotConfigured("stub".into()))
    }
    async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        if r.path == "/v1internal:loadCodeAssist" {
            return Ok(ProxyResponse {
                status: 200,
                body: SETUP_BODY.to_string(),
            });
        }
        Err(BrokerError::Denied("not implemented".into()))
    }
    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        self.seen.lock().unwrap().push(request);
        let mut attempts = self.attempts.lock().unwrap();
        if attempts.is_empty() {
            return Err(BrokerError::Transport("no scripted attempt".into()));
        }
        match attempts.remove(0) {
            Err(text) => Err(BrokerError::Transport(text)),
            Ok(chunks) => Ok(Box::pin(stream::iter(chunks))),
        }
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not implemented".into()))
    }
    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn chunk(s: &str) -> Result<bytes::Bytes, BrokerError> {
    Ok(bytes::Bytes::copy_from_slice(s.as_bytes()))
}

fn success_chunks() -> Vec<Result<bytes::Bytes, BrokerError>> {
    vec![
        chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}\n"),
        chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":4}}}\n"),
    ]
}

fn cfg() -> ProviderConfig {
    ProviderConfig {
        base_url: "https://cloudcode-pa.googleapis.com".into(),
        model: "gemini-2.5-pro".into(),
        provider: "google-gemini".into(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_gemini(
    h: &Harness,
    broker: &Arc<dyn CredentialBroker>,
    tools: &[Value],
    system: &Option<String>,
    msgs: &[crate::SharedMessage],
    cancel: &CancellationToken,
    exact: bool,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (tx, _rx) = mpsc::unbounded_channel();
    call_google_gemini_stream_inner(
        &cfg(),
        broker,
        tools,
        system,
        msgs,
        &tx,
        cancel,
        &h.trace,
        exact,
    )
    .await
}

#[tokio::test]
async fn gemini_success_emits_one_exact_wire_record() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Ok(success_chunks())]);
    let broker: Arc<dyn CredentialBroker> = stub.clone();
    let system = Some(format!("{SENTINEL} system"));
    let tools = vec![json!({
        "name": "search",
        "description": format!("{SENTINEL} desc"),
        "input_schema": {"type": "object"}
    })];

    run_gemini(
        &h,
        &broker,
        &tools,
        &system,
        &messages(),
        &CancellationToken::new(),
        true,
    )
    .await
    .expect("stream succeeds");

    let records = h.sink.records();
    assert_eq!(records.len(), 1, "one record per actual broker attempt");
    let r = &records[0];
    assert_eq!(r.attempt, 1);
    assert!(r.outcome.retries.is_empty());
    assert_eq!(r.transport, TransportKind::GeminiGenerateContent);
    assert_eq!(r.model.as_str(), "google-gemini/gemini-2.5-pro");
    assert_eq!(r.endpoint.host(), "cloudcode-pa.googleapis.com");
    assert_eq!(r.endpoint.path(), "/v1internal:streamGenerateContent");
    assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(r.outcome.stop_reason, Some(StopReason::EndTurn));

    // Usage exactly as the provider reported it — no zero-filling.
    let usage = r.outcome.usage.expect("provider reported usage");
    assert_eq!(usage.provenance, UsageProvenance::ProviderReported);
    assert_eq!(usage.input_tokens, Some(9));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cache_read_tokens, None);
    assert_eq!(usage.cache_write_tokens, None);

    // Timings observed on this attempt's own clock.
    assert!(r.outcome.timings.send_start_unix_ms.is_some());
    assert!(r.outcome.timings.first_byte_ms.is_some());
    assert!(r.outcome.timings.first_model_event_ms.is_some());
    assert!(r.outcome.timings.stream_end_ms.is_some());

    // Exact wire: the digest describes the very body_bytes handed to the
    // broker (which LocalBroker sends verbatim).
    let seen = stub.seen.lock().unwrap();
    let sent = seen[0]
        .body_bytes
        .as_ref()
        .expect("exact-byte handoff populated");
    let wire = r.wire.as_ref().expect("wire metadata populated");
    assert_eq!(wire.byte_len, sent.len() as u64);
    let key = super::load_or_create_digest_key_at(&h.key_path).unwrap();
    assert_eq!(
        wire.digest,
        super::keyed_digest(&key, super::DigestDomain::Wire, sent)
    );

    // Structural anatomy from the normalized request.
    assert_eq!(r.anatomy.message_count, 1);
    assert_eq!(r.anatomy.tool_count, 1);
    assert_eq!(r.anatomy.system_segment_count, 1);
    assert_eq!(r.messages[0].blocks[0].kind, BlockKind::Text);

    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_remote_broker_record_claims_no_wire_bytes() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Ok(success_chunks())]);
    let broker: Arc<dyn CredentialBroker> = stub;

    run_gemini(
        &h,
        &broker,
        &[],
        &None,
        &messages(),
        &CancellationToken::new(),
        false, // remote broker: upstream bytes serialized out of process
    )
    .await
    .expect("stream succeeds");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].transport, TransportKind::CloudProxy);
    assert!(
        records[0].wire.is_none(),
        "no exact bytes → no wire claim, ever"
    );
    assert_schema_valid_and_content_free(&records);
}

/// Live-shaped Code Assist 429 transport error with a 1s reset hint.
const RATE_LIMIT_429: &str = "provider request failed: 429 Too Many Requests: \
    {\"error\":{\"code\":429,\"message\":\"You have exhausted your capacity on \
    this model. Your quota will reset after 1s.\",\"status\":\"RESOURCE_EXHAUSTED\"}}";

#[tokio::test(start_paused = true)]
async fn gemini_429_retry_emits_one_record_per_attempt() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Err(RATE_LIMIT_429.to_string()), Ok(success_chunks())]);
    let broker: Arc<dyn CredentialBroker> = stub.clone();

    run_gemini(
        &h,
        &broker,
        &[],
        &None,
        &messages(),
        &CancellationToken::new(),
        true,
    )
    .await
    .expect("429 then success must recover");

    assert_eq!(stub.stream_calls(), 2);
    let records = h.sink.records();
    assert_eq!(records.len(), 2, "one record per actual attempt");
    assert_eq!(records[0].request_id, records[1].request_id);
    assert_eq!(records[0].attempt, 1);
    assert_eq!(records[1].attempt, 2);

    // Attempt 1: its own typed failure, no prior retries.
    assert!(records[0].outcome.retries.is_empty());
    assert_eq!(records[0].outcome.http_status, Some(429));
    match &records[0].outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_429"),
        other => panic!("attempt 1 must be ProviderFailed, got {other:?}"),
    }

    // Attempt 2: carries the prior retry and completes.
    assert_eq!(records[1].outcome.retries.len(), 1);
    assert_eq!(
        records[1].outcome.retries[0].class,
        super::RetryClass::RateLimited
    );
    assert_eq!(records[1].outcome.terminal, TurnOutcome::Completed);
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_terminal_failure_records_status_without_provider_text() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Err(format!(
        "provider request failed: 404 Not Found: ECHOED:{SENTINEL}"
    ))]);
    let broker: Arc<dyn CredentialBroker> = stub.clone();

    let err = run_gemini(
        &h,
        &broker,
        &[],
        &None,
        &messages(),
        &CancellationToken::new(),
        true,
    )
    .await
    .expect_err("404 must fail immediately");
    assert!(!err.to_string().contains(SENTINEL), "user error redacted");

    assert_eq!(stub.stream_calls(), 1, "non-429 errors are not retried");
    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome.http_status, Some(404));
    match &records[0].outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "http_404"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_cancellation_mid_stream_records_canceled_terminal() {
    let h = harness();
    // One delivered chunk, then the stream stays open: the cancel token is
    // the only path to termination.
    let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<Result<bytes::Bytes, BrokerError>>();
    chunk_tx
        .send(chunk(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"one\"}]}}]}}\n",
        ))
        .unwrap();

    struct BlockingBroker(
        Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, BrokerError>>>>,
    );
    #[async_trait]
    impl CredentialBroker for BlockingBroker {
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            if r.path == "/v1internal:loadCodeAssist" {
                return Ok(ProxyResponse {
                    status: 200,
                    body: SETUP_BODY.to_string(),
                });
            }
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
            let rx = self.0.lock().unwrap().take().unwrap();
            Ok(Box::pin(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
        }
        async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    let broker: Arc<dyn CredentialBroker> = Arc::new(BlockingBroker(Mutex::new(Some(chunk_rx))));
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        canceller.cancel();
    });

    let err = run_gemini(&h, &broker, &[], &None, &messages(), &cancel, true)
        .await
        .expect_err("cancel must surface");
    assert!(err.to_string().contains("canceled"), "{err}");
    drop(chunk_tx);

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert!(records[0].outcome.timings.first_byte_ms.is_some());
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_mid_stream_error_records_failed_terminal() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Ok(vec![
        chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n"),
        Err(BrokerError::Transport(format!("upstream reset {SENTINEL}"))),
    ])]);
    let broker: Arc<dyn CredentialBroker> = stub;

    run_gemini(
        &h,
        &broker,
        &[],
        &None,
        &messages(),
        &CancellationToken::new(),
        true,
    )
    .await
    .expect_err("mid-stream error must fail");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    match &records[0].outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "stream_error"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_records_report_known_translation_losses() {
    let h = harness();
    let stub = GeminiBroker::new(vec![Ok(success_chunks())]);
    let broker: Arc<dyn CredentialBroker> = stub;
    let msgs: Vec<crate::SharedMessage> = vec![
        Arc::new(json!({"role":"user","content":format!("{SENTINEL} go")})),
        Arc::new(json!({"role":"assistant","content":[
            {"type":"thinking","thinking":format!("{SENTINEL} private")},
            {"type":"text","text":"a"},
            {"type":"text","text":"b"},
            {"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}}
        ]})),
        Arc::new(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"ok"}
        ]})),
    ];
    let tools = vec![
        json!({"name":"respond","input_schema":{"type":"object"}}),
        json!({"name":"bash","input_schema":{"type":"object"}}),
    ];

    run_gemini(
        &h,
        &broker,
        &tools,
        &None,
        &msgs,
        &CancellationToken::new(),
        true,
    )
    .await
    .expect("stream succeeds");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let losses = &records[0].translation_losses;
    let has = |action: TranslationAction, element: TranslationElement, id: &str| {
        losses.iter().any(|l| {
            l.action == action
                && l.element == element
                && l.element_id.as_ref().map(|i| i.as_str()) == Some(id)
        })
    };
    // Internal-only tool dropped from the wire tool list.
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Tool,
        "respond"
    ));
    // Thinking block dropped (not representable on the Gemini wire).
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::MessageBlock,
        "messages[1].blocks[0]"
    ));
    // Adjacent text segments merged into one turn: the run starts at
    // blocks[1] (the leading thinking block does not break the run because
    // the translator's text buffer survives dropped blocks).
    assert!(has(
        TranslationAction::Merged,
        TranslationElement::MessageBlock,
        "messages[1].blocks[1]"
    ));
    // functionResponse name synthesized from the tool_use id→name map.
    assert!(has(
        TranslationAction::Synthesized,
        TranslationElement::MessageBlock,
        "messages[2].blocks[0]"
    ));
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn gemini_disabled_context_emits_nothing_and_still_streams() {
    let stub = GeminiBroker::new(vec![Ok(success_chunks())]);
    let broker: Arc<dyn CredentialBroker> = stub;
    let (tx, _rx) = mpsc::unbounded_channel();
    let out = call_google_gemini_stream_inner(
        &cfg(),
        &broker,
        &[],
        &None,
        &messages(),
        &tx,
        &CancellationToken::new(),
        &TraceContext::disabled(),
        true,
    )
    .await
    .expect("stream succeeds without tracing");
    assert_eq!(out["content"][0]["text"], "hi there");
}

// ═══ Cloud invoke (AWS Bedrock / Azure OpenAI / Google Vertex) ══════════════

/// Fake cloud broker: scripted `cloud_invoke` outcomes; counts invocations.
struct CloudBroker {
    script: Mutex<Option<Result<Vec<Result<CloudEvent, BrokerError>>, BrokerError>>>,
    invokes: AtomicUsize,
}

impl CloudBroker {
    fn new(script: Result<Vec<Result<CloudEvent, BrokerError>>, BrokerError>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(Some(script)),
            invokes: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl CredentialBroker for CloudBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::NotConfigured("stub".into()))
    }
    async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not implemented".into()))
    }
    async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not implemented".into()))
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not implemented".into()))
    }
    async fn cloud_invoke(
        &self,
        _provider: CloudProviderId,
        _context_ref: &str,
        _model_id: &str,
        _request: InvokeRequest,
    ) -> Result<CloudEventStream, BrokerError> {
        self.invokes.fetch_add(1, Ordering::SeqCst);
        match self.script.lock().unwrap().take() {
            Some(Ok(events)) => Ok(Box::pin(stream::iter(events))),
            Some(Err(e)) => Err(e),
            None => Err(BrokerError::Denied("no scripted invoke".into())),
        }
    }
    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

const CLOUD_PROVIDERS: [CloudProviderId; 3] = [
    CloudProviderId::AwsBedrock,
    CloudProviderId::AzureOpenAi,
    CloudProviderId::GoogleVertex,
];

#[tokio::test]
async fn cloud_invoke_success_emits_one_cloudproxy_record_per_provider() {
    for provider in CLOUD_PROVIDERS {
        let h = harness();
        let stub = CloudBroker::new(Ok(vec![
            Ok(CloudEvent::TextDelta {
                delta: "hello".into(),
            }),
            Ok(CloudEvent::Usage {
                input_tokens: 7,
                output_tokens: 2,
            }),
            Ok(CloudEvent::Done),
        ]));
        let broker: Arc<dyn CredentialBroker> = stub.clone();
        let cloud_model = format!("{}/test-model-v1", provider.as_str());
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = cloud_invoke_stream(
            provider,
            &cloud_model,
            &cloud_model,
            None,
            false,
            move || broker,
            &Some(format!("{SENTINEL} system")),
            &messages(),
            &tx,
            &CancellationToken::new(),
            &h.trace,
        )
        .await
        .expect("cloud invoke succeeds");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(stub.invokes.load(Ordering::SeqCst), 1);

        let records = h.sink.records();
        assert_eq!(records.len(), 1, "{provider}: one record per invocation");
        let r = &records[0];
        assert_eq!(r.attempt, 1);
        assert_eq!(r.transport, TransportKind::CloudProxy);
        assert!(
            r.wire.is_none(),
            "{provider}: provider bytes are broker-owned — never a wire claim"
        );
        assert_eq!(r.model.as_str(), cloud_model);
        assert_eq!(
            r.endpoint.host(),
            format!("{}.cloud-broker.invalid", provider.as_str()),
            "static reserved-name endpoint, never a fabricated real host"
        );
        assert_eq!(r.outcome.terminal, TurnOutcome::Completed);
        let usage = r.outcome.usage.expect("broker-reported usage");
        assert_eq!(usage.provenance, UsageProvenance::ProviderReported);
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(2));
        assert_eq!(usage.cache_read_tokens, None);
        assert!(r.outcome.timings.send_start_unix_ms.is_some());
        assert_schema_valid_and_content_free(&records);
    }
}

#[tokio::test]
async fn cloud_invoke_failure_emits_failed_record_without_provider_text() {
    for provider in CLOUD_PROVIDERS {
        let h = harness();
        let stub = CloudBroker::new(Err(BrokerError::Transport(format!(
            "provider exploded: {SENTINEL}"
        ))));
        let broker: Arc<dyn CredentialBroker> = stub;
        let cloud_model = format!("{}/test-model-v1", provider.as_str());
        let (tx, _rx) = mpsc::unbounded_channel();

        cloud_invoke_stream(
            provider,
            &cloud_model,
            &cloud_model,
            None,
            false,
            move || broker,
            &None,
            &messages(),
            &tx,
            &CancellationToken::new(),
            &h.trace,
        )
        .await
        .expect_err("broker failure must surface");

        let records = h.sink.records();
        assert_eq!(records.len(), 1, "{provider}: failed call still one record");
        match &records[0].outcome.terminal {
            TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "cloud_invoke_error"),
            other => panic!("{provider}: expected ProviderFailed, got {other:?}"),
        }
        assert_schema_valid_and_content_free(&records);
    }
}

#[tokio::test]
async fn cloud_invoke_mid_stream_error_emits_failed_record() {
    let h = harness();
    let stub = CloudBroker::new(Ok(vec![
        Ok(CloudEvent::TextDelta {
            delta: "part".into(),
        }),
        Err(BrokerError::Transport(format!("boom {SENTINEL}"))),
    ]));
    let broker: Arc<dyn CredentialBroker> = stub;
    let (tx, _rx) = mpsc::unbounded_channel();

    cloud_invoke_stream(
        CloudProviderId::AwsBedrock,
        "aws-bedrock/test-model-v1",
        "aws-bedrock/test-model-v1",
        None,
        false,
        move || broker,
        &None,
        &messages(),
        &tx,
        &CancellationToken::new(),
        &h.trace,
    )
    .await
    .expect_err("stream error must surface");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    match &records[0].outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => assert_eq!(code, "cloud_stream_error"),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn cloud_invoke_cancellation_emits_canceled_record() {
    let h = harness();
    // A stream that yields nothing and never terminates.
    struct PendingCloudBroker;
    #[async_trait]
    impl CredentialBroker for PendingCloudBroker {
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn cloud_invoke(
            &self,
            _provider: CloudProviderId,
            _context_ref: &str,
            _model_id: &str,
            _request: InvokeRequest,
        ) -> Result<CloudEventStream, BrokerError> {
            Ok(Box::pin(stream::pending()))
        }
        async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }
    let broker: Arc<dyn CredentialBroker> = Arc::new(PendingCloudBroker);
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        canceller.cancel();
    });

    cloud_invoke_stream(
        CloudProviderId::GoogleVertex,
        "google-vertex/test-model-v1",
        "google-vertex/test-model-v1",
        None,
        false,
        move || broker,
        &None,
        &messages(),
        &tx,
        &cancel,
        &h.trace,
    )
    .await
    .expect_err("cancellation must surface");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn cloud_invoke_tool_bearing_preflight_makes_no_broker_and_no_record() {
    for provider in CLOUD_PROVIDERS {
        let h = harness();
        let constructed = Arc::new(AtomicUsize::new(0));
        let constructed_c = constructed.clone();
        let cloud_model = format!("{}/test-model-v1", provider.as_str());
        let (tx, _rx) = mpsc::unbounded_channel();

        let err = cloud_invoke_stream(
            provider,
            &cloud_model,
            &cloud_model,
            None,
            true, // tool-bearing: must fail §5.5 pre-flight
            move || {
                constructed_c.fetch_add(1, Ordering::SeqCst);
                let broker: Arc<dyn CredentialBroker> =
                    CloudBroker::new(Err(BrokerError::Denied("unreachable".into())));
                broker
            },
            &None,
            &messages(),
            &tx,
            &CancellationToken::new(),
            &h.trace,
        )
        .await
        .expect_err("tool-bearing cloud request must fail pre-flight");
        assert!(
            err.to_string().contains("tools"),
            "{provider}: typed capability error expected, got {err}"
        );
        assert_eq!(
            constructed.load(Ordering::SeqCst),
            0,
            "{provider}: pre-flight failure must construct no broker"
        );
        assert!(
            h.sink.records().is_empty(),
            "{provider}: a request that never became an attempt emits no record"
        );
    }
}

// ═══ Unit coverage for the google helpers ═══════════════════════════════════

#[test]
fn gemini_stop_reasons_normalize_via_closed_mapping() {
    use super::google::stop_reason_from_gemini;
    assert_eq!(stop_reason_from_gemini("STOP"), StopReason::EndTurn);
    assert_eq!(stop_reason_from_gemini("MAX_TOKENS"), StopReason::MaxTokens);
    assert_eq!(stop_reason_from_gemini("TOOL_CALL"), StopReason::ToolUse);
    assert_eq!(
        stop_reason_from_gemini("FUNCTION_CALL"),
        StopReason::ToolUse
    );
    assert_eq!(stop_reason_from_gemini("SAFETY"), StopReason::ContentFilter);
    assert_eq!(
        stop_reason_from_gemini("SOMETHING_NEW"),
        StopReason::Other,
        "unknown raw values collapse — the raw string is never stored"
    );
}

#[test]
fn gemini_usage_preserves_absent_counts() {
    use crate::runtime::google_gemini::translate::GeminiUsage;
    let usage = super::google::usage_from_gemini(&GeminiUsage {
        prompt_tokens: Some(5),
        candidates_tokens: None,
        cached_tokens: None,
    });
    assert_eq!(usage.input_tokens, Some(5));
    assert_eq!(usage.output_tokens, None, "no fabricated zeros");
    assert_eq!(usage.cache_read_tokens, None);
}

// ═══ Report honesty: merges and edge drops mirror the actual translator ═════

/// Direct fixtures for `gemini_translation_losses` — no sentinels needed:
/// every assertion below is on structural IDs and enum variants only, and a
/// blanket check proves no element id ever carries message text.
fn losses_for(messages: &[crate::SharedMessage], tools: &[Value]) -> Vec<super::TranslationLoss> {
    let losses = super::google::gemini_translation_losses(messages, tools);
    for loss in &losses {
        if let Some(id) = &loss.element_id {
            let structural = id.as_str().starts_with("messages[")
                || id.as_str().starts_with("tools[")
                || super::google::GEMINI_INTERNAL_TOOLS.contains(&id.as_str());
            assert!(
                structural,
                "element ids must be structural paths or shared internal tool names, got {id}"
            );
        }
    }
    losses
}

fn merged_ids(losses: &[super::TranslationLoss]) -> Vec<String> {
    losses
        .iter()
        .filter(|l| l.action == TranslationAction::Merged)
        .map(|l| l.element_id.as_ref().expect("merged id").to_string())
        .collect()
}

#[test]
fn gemini_report_no_merge_when_tool_use_breaks_the_text_run() {
    // Regression: the translator flushes the text buffer at tool_use, so
    // [text, tool_use, text] becomes three turns — nothing is merged.
    let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "before"},
            {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
            {"type": "text", "text": "after"},
        ],
    }))];
    let losses = losses_for(&msgs, &[]);
    assert!(
        merged_ids(&losses).is_empty(),
        "tool_use breaks the run — no merge may be reported: {losses:?}"
    );
}

#[test]
fn gemini_report_no_merge_when_tool_result_breaks_the_text_run() {
    let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "before"},
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            {"type": "text", "text": "after"},
        ],
    }))];
    let losses = losses_for(&msgs, &[]);
    assert!(
        merged_ids(&losses).is_empty(),
        "tool_result breaks the run — no merge may be reported: {losses:?}"
    );
}

#[test]
fn gemini_report_exactly_one_merge_for_adjacent_text_pair() {
    let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"},
        ],
    }))];
    let losses = losses_for(&msgs, &[]);
    assert_eq!(
        merged_ids(&losses),
        vec!["messages[0].blocks[0]".to_string()],
        "exactly one merge, identified by the run's first block"
    );
}

#[test]
fn gemini_report_one_merge_per_actually_concatenated_run() {
    // Two runs around a flushing tool_use: [a b | tool_use | c d] — the
    // wire concatenates a+b and c+d, so exactly two Merged entries.
    let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"},
            {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
            {"type": "text", "text": "c"},
            {"type": "text", "text": "d"},
        ],
    }))];
    let losses = losses_for(&msgs, &[]);
    assert_eq!(
        merged_ids(&losses),
        vec![
            "messages[0].blocks[0]".to_string(),
            "messages[0].blocks[3]".to_string(),
        ]
    );
}

#[test]
fn gemini_report_merge_survives_dropped_blocks_inside_the_run() {
    // The translator's text buffer is not flushed by dropped (thinking)
    // blocks, so [text, thinking, text] really is concatenated on the wire.
    let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": "a"},
            {"type": "thinking", "thinking": "private"},
            {"type": "text", "text": "b"},
        ],
    }))];
    let losses = losses_for(&msgs, &[]);
    assert_eq!(
        merged_ids(&losses),
        vec!["messages[0].blocks[0]".to_string()]
    );
}

#[test]
fn gemini_report_edge_drops_match_translator() {
    let msgs: Vec<crate::SharedMessage> = vec![
        // Empty string content: translator emits no turn.
        Arc::new(json!({"role": "user", "content": ""})),
        // Non-string/non-array content: translator emits no turn.
        Arc::new(json!({"role": "assistant", "content": 42})),
        // Missing content: translator emits no turn.
        Arc::new(json!({"role": "user"})),
        // Kept for contrast: a normal message that loses nothing.
        Arc::new(json!({"role": "user", "content": "hello"})),
    ];
    let tools = vec![
        json!({"description": "no name at all"}),
        json!({"name": "", "input_schema": {"type": "object"}}),
        json!({"name": "bash", "input_schema": {"type": "object"}}),
    ];
    let losses = losses_for(&msgs, &tools);
    let has = |action: TranslationAction, element: TranslationElement, id: &str| {
        losses.iter().any(|l| {
            l.action == action
                && l.element == element
                && l.element_id.as_ref().map(|i| i.as_str()) == Some(id)
        })
    };
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Other,
        "messages[0]"
    ));
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Other,
        "messages[1]"
    ));
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Other,
        "messages[2]"
    ));
    // Tools the wire filter rejects (missing or empty name) are reported by
    // structural position only — never by (absent or empty) content.
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Tool,
        "tools[0]"
    ));
    assert!(has(
        TranslationAction::Dropped,
        TranslationElement::Tool,
        "tools[1]"
    ));
    // The healthy message and tool contribute nothing.
    assert!(!losses
        .iter()
        .any(|l| l.element_id.as_ref().map(|i| i.as_str()) == Some("messages[3]")));
    assert!(!losses
        .iter()
        .any(|l| l.element_id.as_ref().map(|i| i.as_str()) == Some("tools[2]")));
    assert_eq!(losses.len(), 5, "no spurious extra entries: {losses:?}");
}

#[test]
fn gemini_report_keeps_shared_internal_tool_rule() {
    let tools = vec![
        json!({"name": "respond", "input_schema": {"type": "object"}}),
        json!({"name": "send_channel", "input_schema": {"type": "object"}}),
        json!({"name": "watcher_exit", "input_schema": {"type": "object"}}),
    ];
    let losses = losses_for(&[], &tools);
    let ids: Vec<_> = losses
        .iter()
        .filter(|l| l.action == TranslationAction::Dropped && l.element == TranslationElement::Tool)
        .map(|l| l.element_id.as_ref().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["respond", "send_channel", "watcher_exit"]);
}
