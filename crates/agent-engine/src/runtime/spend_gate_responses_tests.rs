//! The broker Responses transport's spend gate (GitHub Copilot, xAI and
//! Muse share its stream loop): a request with no tap hands the broker the
//! same requests and emits the same events as before child spend. The
//! route sends through the credential broker, so each case drives the
//! transport directly with a broker that records each request and answers
//! from a script, and compares what the broker received (provider, method,
//! path, stream flag, the exact body bytes), the request's event sequence
//! and those events' client-wire lines, plus its result.
use super::*;

use agent_core::auth::{AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse};
use async_trait::async_trait;

/// A broker that records each streamed request and answers the `n`th with
/// `script(n)`.
struct ScriptBroker {
    seen: Mutex<Vec<ProxyRequest>>,
    script: fn(usize) -> String,
}

#[async_trait]
impl crate::auth::CredentialBroker for ScriptBroker {
    async fn access_token(
        &self,
        _p: agent_core::auth::OAuthProviderId,
    ) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("proxy only".into()))
    }
    async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("stream only".into()))
    }
    async fn proxy_stream(&self, request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        request.validate()?;
        let n = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(request);
            seen.len() - 1
        };
        let sse = (self.script)(n);
        Ok(Box::pin(futures::stream::iter(vec![Ok(Bytes::from(sse))])))
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn render_proxy_requests(requests: &[ProxyRequest]) -> String {
    let mut out = String::new();
    for (i, r) in requests.iter().enumerate() {
        out.push_str(&format!(
            "=== request {i}\n{} {:?} {} stream={}\n",
            r.provider, r.method, r.path, r.stream
        ));
        let bytes = r
            .body_bytes
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        out.push_str(&format!("--- body bytes ({} bytes)\n{bytes}\n", bytes.len()));
        let same = r.body.as_ref().map(serde_json::Value::to_string)
            == serde_json::from_str::<serde_json::Value>(&bytes)
                .ok()
                .map(|v| v.to_string());
        out.push_str(&format!("--- body matches its bytes: {same}\n"));
    }
    out
}

fn responses_sse(events: &[serde_json::Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn usage(input: u64, cached: u64, output: u64) -> serde_json::Value {
    json!({"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},"output_tokens":output})
}

/// A complete response: text, then its one usage report.
fn responses_ok() -> String {
    responses_sse(&[
        json!({"type":"response.output_text.delta","delta":"gate "}),
        json!({"type":"response.output_text.delta","delta":"ok"}),
        json!({"type":"response.completed","response":{"usage":usage(10, 4, 3)}}),
    ])
}

/// A stream cut off before its terminal event.
fn responses_cut() -> String {
    responses_sse(&[json!({"type":"response.output_text.delta","delta":"gate "})])
}

/// A lone in-stream capacity error before any output (retried).
fn responses_capacity() -> String {
    responses_sse(&[
        json!({"type":"error","code":"server_overloaded","message":"at capacity"}),
    ])
}

/// A terminal failure that carries a usage report.
fn responses_failed() -> String {
    responses_sse(&[json!({"type":"response.failed","response":{
        "error":{"type":"server_error","code":"server_error","message":"failed"},
        "usage":usage(9, 0, 2)}})])
}

/// Two terminal reports with counts in one stream (today each emits a
/// Usage event).
fn responses_two_terminals() -> String {
    responses_sse(&[
        json!({"type":"response.output_text.delta","delta":"gate ok"}),
        json!({"type":"response.completed","response":{"usage":usage(10, 4, 3)}}),
        json!({"type":"response.done","response":{"usage":usage(11, 4, 5)}}),
    ])
}

/// One GitHub Copilot Responses request against `script`; compare the
/// requests, events and result with `name`'s fixtures.
async fn responses_gate(name: &str, max_retries: u32, script: fn(usize) -> String) {
    let broker = Arc::new(ScriptBroker {
        seen: Mutex::new(Vec::new()),
        script,
    });
    let dyn_broker: Arc<dyn crate::auth::CredentialBroker> = broker.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = crate::runtime::openai::stream::call_responses_route_inner(
        "github-copilot",
        &crate::runtime::openai::types::ProviderConfig {
            base_url: "https://api.githubcopilot.com".to_owned(),
            model: "gpt-5-mini".to_owned(),
            provider: "github-copilot".to_owned(),
        },
        &dyn_broker,
        &[],
        &Some("You are a gate.".to_owned()),
        &history(),
        &tx,
        None,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &cancel,
        max_retries,
        &crate::runtime::trace::TraceContext::disabled(),
        true,
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
        "=> {:?}\n",
        result.as_ref().map_err(ToString::to_string)
    ));
    let requests = render_proxy_requests(&broker.seen.lock().unwrap());
    check(&format!("{name}_requests.txt"), &stable(&requests));
    check(&format!("{name}_events.txt"), &stable(&events));
    check(&format!("{name}_wire.txt"), &stable(&wire));
}

/// A broker Responses request with no tap: the same request, the same
/// events, plain `Usage` included, and the same result.
#[tokio::test]
async fn spend_gate_responses_is_unchanged_without_a_tap() {
    responses_gate("responses", 0, |_| responses_ok()).await;
}

/// The same with a stream cut off before its terminal event and its retry.
#[tokio::test]
async fn spend_gate_responses_stream_retry_is_unchanged_without_a_tap() {
    responses_gate("responses_stream_retry", 1, |n| match n {
        0 => responses_cut(),
        _ => responses_ok(),
    })
    .await;
}

/// The same with a lone capacity error the stream loop sends again.
#[tokio::test]
async fn spend_gate_responses_capacity_retry_is_unchanged_without_a_tap() {
    responses_gate("responses_capacity_retry", 1, |n| match n {
        0 => responses_capacity(),
        _ => responses_ok(),
    })
    .await;
}

/// The same with a terminal failure that reports usage.
#[tokio::test]
async fn spend_gate_responses_failure_is_unchanged_without_a_tap() {
    responses_gate("responses_failure", 0, |_| responses_failed()).await;
}

/// The same with two counted terminal reports in one stream: today's two
/// Usage events stay as they are.
#[tokio::test]
async fn spend_gate_responses_two_terminal_reports_are_unchanged_without_a_tap() {
    responses_gate("responses_two_terminals", 0, |_| responses_two_terminals()).await;
}
