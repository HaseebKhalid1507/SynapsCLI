//! Task 10B wiring tests: the explicit cloud-invoke routes (AWS Bedrock,
//! Azure OpenAI, Google Vertex) emit one schema-valid
//! `synaps-request-trace/1` record per actual broker attempt. All brokers
//! are in-process fakes — no real network. Text-only cloud pre-flight
//! failures happen before any broker work and emit no record.

use super::{CollectingTraceSink, RequestTrace, TraceContext, TransportKind, UsageProvenance};
use crate::auth::{
    AccessToken, BrokerError, CloudProviderId, CredentialBroker, OAuthProviderId, ProviderStatus,
    ProxyByteStream, ProxyRequest, ProxyResponse,
};
use crate::runtime::cloud_invoke::cloud_invoke_stream;
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
    _tmp: tempfile::TempDir,
}

fn harness() -> Harness {
    let tmp = tempfile::TempDir::new().unwrap();
    let key_path = tmp.path().join("trace").join("digest.key");
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink.clone()).with_key_path(key_path);
    Harness {
        sink,
        trace,
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

// ═══ Cloud invoke (AWS Bedrock / Azure OpenAI / Google Vertex) ══════════════

/// Fake cloud broker: scripted `cloud_invoke` outcomes; counts invocations.
type CloudScript = Result<Vec<Result<CloudEvent, BrokerError>>, BrokerError>;

struct CloudBroker {
    script: Mutex<Option<CloudScript>>,
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
