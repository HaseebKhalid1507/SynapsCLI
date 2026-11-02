//! The Codex transport's spend gate: a Codex request with no tap sends the
//! same requests and emits the same events as before child spend. The
//! Codex route has no base-URL override, so each case drives the transport
//! directly against a loopback stand-in and compares what it received
//! (method, path, headers, the exact body bytes), the request's event
//! sequence and those events' client-wire lines, plus its result.
use super::*;

use agent_core::auth::{AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

/// A JWT-shaped token carrying the ChatGPT account-id claim (not a
/// credential: the stand-in accepts anything).
fn token() -> String {
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct_gate" } });
    format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

/// A broker that vends the token and nothing else.
struct TokenBroker;

#[async_trait]
impl crate::auth::CredentialBroker for TokenBroker {
    async fn access_token(
        &self,
        _p: agent_core::auth::OAuthProviderId,
    ) -> Result<AccessToken, BrokerError> {
        Ok(AccessToken {
            token: token(),
            expires: u64::MAX,
        })
    }
    async fn proxy(&self, _request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn proxy_stream(&self, _request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn codex_sse(events: &[serde_json::Value], done: bool) -> String {
    events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .chain(done.then(|| "data: [DONE]\n\n".to_owned()))
        .collect()
}

/// A complete response: text, then its one usage report.
fn codex_ok() -> String {
    codex_sse(
        &[
            json!({"type":"response.output_text.delta","delta":"gate "}),
            json!({"type":"response.output_text.delta","delta":"ok"}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":3}}}),
        ],
        true,
    )
}

/// A stream cut off before its terminal event.
fn codex_cut() -> String {
    codex_sse(
        &[json!({"type":"response.output_text.delta","delta":"gate "})],
        false,
    )
}

/// Two terminal reports with counts in one stream (today each emits a
/// Usage event).
fn codex_two_terminals() -> String {
    codex_sse(
        &[
            json!({"type":"response.output_text.delta","delta":"gate ok"}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":3}}}),
            json!({"type":"response.done","response":{"usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":4},"output_tokens":5}}}),
        ],
        true,
    )
}

fn codex_429() -> serde_json::Value {
    json!({"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}})
}

/// One Codex request against `reply`; compare the requests, events and
/// result with `name`'s fixtures.
async fn codex_gate(
    name: &str,
    max_retries: u32,
    reply: impl Fn(usize) -> axum::response::Response + Clone + Send + Sync + 'static,
) {
    let (url, seen) = stand_in(reply).await;
    let broker: Arc<dyn crate::auth::CredentialBroker> = Arc::new(TokenBroker);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = crate::runtime::openai::stream::call_codex_stream_inner(
        &crate::runtime::openai::types::ProviderConfig {
            base_url: url,
            model: "gpt-5.6-sol".to_owned(),
            provider: "openai-codex".to_owned(),
        },
        &reqwest::Client::new(),
        &broker,
        &[],
        &Some("You are a gate.".to_owned()),
        &history(),
        &tx,
        None,
        None,
        agent_core::reasoning::ReasoningLevel::Medium,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        &cancel,
        max_retries,
        &crate::runtime::trace::TraceContext::disabled(),
        crate::runtime::child_spend::RouteSpend::NONE,
        None,
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
    let requests = render_requests(&seen.lock().unwrap());
    check(&format!("{name}_requests.txt"), &stable(&requests));
    check(&format!("{name}_events.txt"), &stable(&events));
    check(&format!("{name}_wire.txt"), &stable(&wire));
}

/// A Codex request with no tap: the same request (headers and body
/// bytes), the same events, plain `Usage` included, and the same result.
#[tokio::test]
async fn spend_gate_codex_is_unchanged_without_a_tap() {
    codex_gate("codex", 0, |_| stream_ok(codex_ok())).await;
}

/// The same with a rate-limited first send that the transport sends again
/// (`send_with_retries`), and a stream cut off before its terminal event
/// that the stream loop retries.
#[tokio::test]
async fn spend_gate_codex_resend_and_stream_retry_are_unchanged_without_a_tap() {
    codex_gate("codex_retry", 2, |n| match n {
        0 => rate_limited(codex_429()),
        1 => stream_ok(codex_cut()),
        _ => stream_ok(codex_ok()),
    })
    .await;
}

/// The same with two counted terminal reports in one stream: today's two
/// Usage events stay as they are (the spend guard counts the last once;
/// the events don't change).
#[tokio::test]
async fn spend_gate_codex_two_terminal_reports_are_unchanged_without_a_tap() {
    codex_gate("codex_two_terminals", 0, |_| {
        stream_ok(codex_two_terminals())
    })
    .await;
}
