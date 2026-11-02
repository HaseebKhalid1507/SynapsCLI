//! Child spend on the Gemini (Code Assist) route (P1d): each opened stream
//! is one paid attempt (a 429 opens none). The last usage report with
//! counts is booked once when the attempt ends, at every exit; a running
//! count left by a cut or a cancel is booked as the lower bound it is.
//! The route is broker-proxied only: a scripted broker answers here.
use super::*;

use agent_core::auth::{AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse};
use async_trait::async_trait;

use crate::runtime::child_spend::RouteSpend;
use crate::runtime::google_gemini::runtime::{call_google_gemini_stream_limited, GeminiLimits};
use crate::runtime::openai::types::ProviderConfig;

const GEMINI: &str = "google-gemini/gemini-2.5-pro";

type CallResult = Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

/// One scripted stream call.
enum Reply {
    /// The Code Assist 429 transport error (reset after 1 s).
    RateLimited,
    /// These chunks, then the end of the stream.
    Chunks(Vec<Result<Bytes, BrokerError>>),
    /// These chunks, then the stream stays open.
    Stall(Vec<Result<Bytes, BrokerError>>),
}

/// Serves `loadCodeAssist` and answers the `n`th stream call with
/// `script(n)`, recording how many reports were booked when it arrived.
struct GeminiBroker {
    log: Log,
    booked_at: Mutex<Vec<usize>>,
    script: Box<dyn Fn(usize) -> Reply + Send + Sync>,
}

#[async_trait]
impl crate::auth::CredentialBroker for GeminiBroker {
    async fn access_token(
        &self,
        _p: agent_core::auth::OAuthProviderId,
    ) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::NotConfigured("proxy only".into()))
    }
    async fn proxy(&self, request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        if request.path == "/v1internal:loadCodeAssist" {
            return Ok(ProxyResponse {
                status: 200,
                body: r#"{"cloudaicompanionProject":"spend-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
            });
        }
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn proxy_stream(&self, _request: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        let n = {
            let mut seen = self.booked_at.lock().unwrap();
            seen.push(drain_into(&self.log));
            seen.len() - 1
        };
        match (self.script)(n) {
            Reply::RateLimited => Err(BrokerError::Transport(
                "provider request failed: 429 Too Many Requests: {\"error\":{\"code\":429,\
                 \"message\":\"You have exhausted your capacity on this model. Your quota \
                 will reset after 1s.\",\"status\":\"RESOURCE_EXHAUSTED\"}}"
                    .to_string(),
            )),
            Reply::Chunks(chunks) => Ok(Box::pin(futures::stream::iter(chunks))),
            Reply::Stall(chunks) => Ok(Box::pin(
                futures::stream::iter(chunks).chain(futures::stream::pending()),
            )),
        }
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn gemini_broker(
    log: &Log,
    script: impl Fn(usize) -> Reply + Send + Sync + 'static,
) -> Arc<GeminiBroker> {
    Arc::new(GeminiBroker {
        log: Arc::clone(log),
        booked_at: Mutex::new(Vec::new()),
        script: Box::new(script),
    })
}

fn chunk(value: serde_json::Value) -> Result<Bytes, BrokerError> {
    Ok(Bytes::from(format!("data: {value}\n")))
}

fn says(t: &str) -> Result<Bytes, BrokerError> {
    chunk(json!({"response":{"candidates":[{"content":{"parts":[{"text":t}]}}]}}))
}

/// Text with a running count: 50 prompt tokens (10 of them cached), 1
/// output.
fn running(t: &str) -> Result<Bytes, BrokerError> {
    chunk(json!({"response":{"candidates":[{"content":{"parts":[{"text":t}]}}],
        "usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":1,"cachedContentTokenCount":10}}}))
}

/// The finish with the final count: 50 prompt tokens (10 cached), 3
/// output and 7 thinking (billed as output).
fn stop() -> Result<Bytes, BrokerError> {
    chunk(json!({"response":{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":3,"thoughtsTokenCount":7,"cachedContentTokenCount":10}}}))
}

/// A mid-stream transport error.
fn reset() -> Result<Bytes, BrokerError> {
    Err(BrokerError::Transport("connection reset".into()))
}

/// The final count as booked: the prompt less its cached slice is input,
/// the slice a cache read, thinking added to output; no cache writes, no
/// TTL split.
fn final_tokens() -> SpendTokens {
    chat_tokens(40, 10, 10)
}

/// The running count as booked.
fn running_tokens() -> SpendTokens {
    chat_tokens(40, 1, 10)
}

fn limits(max_429_retries: u32) -> GeminiLimits {
    GeminiLimits {
        max_output_tokens: None,
        max_429_retries,
    }
}

/// One Gemini request through `broker` with `tap`.
async fn gemini(
    broker: &Arc<GeminiBroker>,
    limits: GeminiLimits,
    tap: Option<&SpendTap>,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>,
) -> CallResult {
    let broker: Arc<dyn crate::auth::CredentialBroker> = broker.clone();
    call_google_gemini_stream_limited(
        &ProviderConfig {
            base_url: "https://cloudcode-pa.googleapis.com".to_owned(),
            model: "gemini-2.5-pro".to_owned(),
            provider: "google-gemini".to_owned(),
        },
        &broker,
        &[],
        &Some("You are a worker.".to_owned()),
        &history(),
        tx,
        cancel,
        &crate::runtime::trace::TraceContext::disabled(),
        false,
        limits,
        RouteSpend { tap, model: GEMINI },
    )
    .await
}

/// One whole Gemini request; its events (debug lines) and result.
async fn gemini_run(
    broker: &Arc<GeminiBroker>,
    limits: GeminiLimits,
    tap: Option<&SpendTap>,
) -> (Vec<String>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = gemini(broker, limits, tap, &Default::default(), &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(format!("{event:?}"));
    }
    (events, format!("{:?}", result.map_err(|e| e.to_string())))
}

/// Read events until the text `after` arrives (the route sends no Usage
/// event, so a later text proves the report before it was seen), then
/// cancel the call (and let it end) or drop it; nothing is booked before.
async fn stop_after_text(
    call: impl std::future::Future<Output = CallResult>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::StreamEvent>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    log: &Log,
) {
    let mut call = std::pin::pin!(call);
    let seen = async {
        while let Some(event) = rx.recv().await {
            if format!("{event:?}").contains("\"after\"") {
                return;
            }
        }
    };
    tokio::select! {
        result = &mut call => panic!("the stalled call ended: {result:?}"),
        () = seen => {}
    }
    assert!(booked(log).is_empty(), "nothing is booked while the attempt runs");
    if let Some(cancel) = cancel {
        cancel.cancel();
        assert!(call.await.is_err(), "a cancelled call ends with an error");
    }
}

/// An answer books its final report once, priced at the worker's qualified
/// model; the events and the result are the same as without a tap.
#[tokio::test]
async fn gemini_an_answer_books_its_usage_once_and_changes_nothing_else() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Chunks(vec![says("spend "), stop()]));
    let with_tap = gemini_run(&broker, limits(0), Some(&spend)).await;
    assert!(with_tap.1.starts_with("Ok("), "{}", with_tap.1);
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
    let without = gemini_run(&broker, limits(0), None).await;
    assert_eq!(with_tap, without, "a tap changes no event and no result");
    assert_eq!(booked(&log).len(), 1, "a call without a tap books nothing");
}

/// Running counts then the final one: the last is booked, once.
#[tokio::test]
async fn gemini_the_last_counted_report_is_booked_once() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| {
        Reply::Chunks(vec![running("a "), running("b "), stop()])
    });
    gemini_run(&broker, limits(0), Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
}

/// A later report without counts (thinking alone) leaves the earlier one.
#[tokio::test]
async fn gemini_a_report_without_counts_leaves_the_earlier_one() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| {
        Reply::Chunks(vec![
            stop(),
            chunk(json!({"response":{"usageMetadata":{"thoughtsTokenCount":2}}})),
        ])
    });
    gemini_run(&broker, limits(0), Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
}

/// A stream that ends without a finish marker books its running count.
#[tokio::test]
async fn gemini_a_stream_without_a_finish_books_its_running_count() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Chunks(vec![running("a ")]));
    let (_, result) = gemini_run(&broker, limits(0), Some(&spend)).await;
    assert!(result.starts_with("Ok("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, running_tokens()))]);
}

/// A stream error after the final report books it once.
#[tokio::test]
async fn gemini_a_late_error_books_the_final_report() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Chunks(vec![stop(), reset()]));
    let (_, result) = gemini_run(&broker, limits(0), Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
}

/// A stream error after a running count books that count, as the lower
/// bound of what the attempt cost.
#[tokio::test]
async fn gemini_a_cut_after_a_running_count_books_it() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Chunks(vec![running("a "), reset()]));
    let (_, result) = gemini_run(&broker, limits(0), Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, running_tokens()))]);
}

/// A 429 opens no stream and books nothing; the answer after it books
/// once.
#[tokio::test]
async fn gemini_a_rate_limited_request_then_an_answer_books_once() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |n| match n {
        0 => Reply::RateLimited,
        _ => Reply::Chunks(vec![stop()]),
    });
    let (_, result) = gemini_run(&broker, limits(1), Some(&spend)).await;
    assert!(result.starts_with("Ok("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
    assert_eq!(broker.booked_at.lock().unwrap().clone(), vec![0, 0]);
}

/// After a 429, a late error's final report is still booked (the error's
/// own usage keeps today's narrower rule; the guard books the attempt).
#[tokio::test]
async fn gemini_a_rate_limited_request_then_a_late_error_books_the_final_report() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |n| match n {
        0 => Reply::RateLimited,
        _ => Reply::Chunks(vec![stop(), reset()]),
    });
    let (_, result) = gemini_run(&broker, limits(1), Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
}

/// A request that stays rate limited opens no stream: nothing is booked.
#[tokio::test]
async fn gemini_a_request_that_stays_rate_limited_books_nothing() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::RateLimited);
    let (_, result) = gemini_run(&broker, limits(0), Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert!(booked(&log).is_empty());
}

/// A cancel after a report books it once.
#[tokio::test]
async fn gemini_a_cancel_after_a_report_books_it() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Stall(vec![running("a "), says("after")]));
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_text(
        gemini(&broker, limits(0), Some(&spend), &cancel, &tx),
        rx,
        Some(&cancel),
        &log,
    )
    .await;
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, running_tokens()))]);
}

/// A dropped attempt books what was reported, synchronously, once.
#[tokio::test]
async fn gemini_a_dropped_attempt_books_what_was_reported() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Stall(vec![running("a "), says("after")]));
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_text(
        gemini(&broker, limits(0), Some(&spend), &cancel, &tx),
        rx,
        None,
        &log,
    )
    .await;
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, running_tokens()))]);
}

/// The route dispatch hands the Gemini arm the worker's guard.
#[tokio::test]
async fn gemini_the_route_dispatch_hands_the_arm_the_guard() {
    let (spend, log) = tap();
    let broker = gemini_broker(&log, |_| Reply::Chunks(vec![stop()]));
    let route = crate::runtime::openai::resolve_route(GEMINI).expect("a resolved route");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = dispatched(&route, broker, &tx, Some(&spend), GEMINI).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(booked(&log), vec![(0, priced(GEMINI, final_tokens()))]);
}
