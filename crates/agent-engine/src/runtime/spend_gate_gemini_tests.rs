//! The Gemini (Code Assist) transport's spend gate: a Gemini request with
//! no tap sends the same broker requests and emits the same events, and
//! returns the same result (its usage travels in the result, not as a
//! Usage event), as before child spend. The route is broker-proxied only,
//! so each case drives the transport against an in-process broker that
//! records every request it is given.
use super::*;

use crate::auth::{
    AccessToken, BrokerError, OAuthProviderId, ProviderStatus, ProxyByteStream, ProxyRequest,
    ProxyResponse,
};
use async_trait::async_trait;

/// One scripted `proxy_stream` outcome.
enum Reply {
    /// The Code Assist 429 transport error (reset after 1 s).
    RateLimited,
    /// These stream chunks, in order.
    Chunks(Vec<Result<bytes::Bytes, BrokerError>>),
}

/// Serves `loadCodeAssist`, answers each stream call with the next scripted
/// reply, and records every request it receives.
struct ScriptedBroker {
    replies: Mutex<Vec<Reply>>,
    seen: Mutex<Vec<String>>,
}

impl ScriptedBroker {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies),
            seen: Mutex::default(),
        }
    }

    fn record(&self, kind: &str, request: &ProxyRequest) {
        self.seen.lock().unwrap().push(format!(
            "=== {kind}\n{}\n",
            serde_json::to_string_pretty(request).unwrap()
        ));
    }
}

#[async_trait]
impl crate::auth::CredentialBroker for ScriptedBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::NotConfigured("gate".into()))
    }
    async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        self.record("proxy", &r);
        if r.path == "/v1internal:loadCodeAssist" {
            return Ok(ProxyResponse {
                status: 200,
                body: r#"{"cloudaicompanionProject":"gate-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
            });
        }
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        self.record("proxy_stream", &request);
        let reply = {
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err(BrokerError::Denied("unscripted".into()));
            }
            replies.remove(0)
        };
        match reply {
            Reply::RateLimited => Err(BrokerError::Transport(
                "provider request failed: 429 Too Many Requests: {\"error\":{\"code\":429,\
                 \"message\":\"You have exhausted your capacity on this model. Your quota \
                 will reset after 1s.\",\"status\":\"RESOURCE_EXHAUSTED\"}}"
                    .to_string(),
            )),
            Reply::Chunks(chunks) => Ok(Box::pin(futures::stream::iter(chunks))),
        }
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn line(value: serde_json::Value) -> Result<bytes::Bytes, BrokerError> {
    Ok(bytes::Bytes::from(format!("data: {value}\n")))
}

fn text(t: &str) -> serde_json::Value {
    json!({"response":{"candidates":[{"content":{"parts":[{"text":t}]}}]}})
}

fn stop_with_usage() -> serde_json::Value {
    json!({"response":{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":3,"thoughtsTokenCount":7,"cachedContentTokenCount":10}}})
}

fn running_usage() -> serde_json::Value {
    json!({"response":{"candidates":[{"content":{"parts":[{"text":"gate "}]}}],
        "usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":1,"cachedContentTokenCount":10}}})
}

/// One Gemini request against `replies`; compare the broker requests,
/// events and result with `name`'s fixtures.
async fn gemini_gate(name: &str, replies: Vec<Reply>) {
    let broker = Arc::new(ScriptedBroker::new(replies));
    let dyn_broker: Arc<dyn crate::auth::CredentialBroker> = broker.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = crate::runtime::google_gemini::runtime::call_google_gemini_stream_limited(
        &crate::runtime::openai::types::ProviderConfig {
            base_url: "https://cloudcode-pa.googleapis.com".into(),
            model: "gemini-2.5-pro".into(),
            provider: "google-gemini".into(),
        },
        &dyn_broker,
        &[],
        &Some("You are a gate.".to_owned()),
        &history(),
        &tx,
        &cancel,
        &crate::runtime::trace::TraceContext::disabled(),
        false,
        crate::runtime::google_gemini::runtime::GeminiLimits::default(),
        crate::runtime::child_spend::RouteSpend::NONE,
    )
    .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let (mut events, wire) = render_events(events);
    events.push_str(&format!(
        "=> {}\n",
        match &result {
            Ok(value) => serde_json::to_string(value).unwrap(),
            Err(e) => format!("Err({e})"),
        }
    ));
    let requests = broker.seen.lock().unwrap().concat();
    check(&format!("{name}_requests.txt"), &stable(&requests));
    check(&format!("{name}_events.txt"), &stable(&events));
    check(&format!("{name}_wire.txt"), &stable(&wire));
}

/// A Gemini request with no tap: the same broker requests, events and
/// result (usage in the result) as before.
#[tokio::test]
async fn spend_gate_gemini_is_unchanged_without_a_tap() {
    gemini_gate(
        "gemini",
        vec![Reply::Chunks(vec![
            line(text("gate ")),
            line(stop_with_usage()),
        ])],
    )
    .await;
}

/// The same with a 429 first (no usage) and the retry after the reported
/// reset.
#[tokio::test(start_paused = true)]
async fn spend_gate_gemini_429_retry_is_unchanged_without_a_tap() {
    gemini_gate(
        "gemini_429_retry",
        vec![
            Reply::RateLimited,
            Reply::Chunks(vec![line(text("gate ")), line(stop_with_usage())]),
        ],
    )
    .await;
}

/// The same with a stream error after a running usage report (a late
/// error).
#[tokio::test]
async fn spend_gate_gemini_late_error_is_unchanged_without_a_tap() {
    gemini_gate(
        "gemini_late_error",
        vec![Reply::Chunks(vec![
            line(running_usage()),
            Err(BrokerError::Transport("connection reset".into())),
        ])],
    )
    .await;
}
