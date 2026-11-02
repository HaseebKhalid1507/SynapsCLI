//! Child spend on the Responses wire (P1d): the Codex transport (direct
//! HTTP to a loopback stand-in) and the broker Responses transport
//! (GitHub Copilot, xAI and Muse share it; a scripted broker here). Each
//! answered send or opened stream is one paid attempt: its last usage
//! report with counts is booked once, when the attempt ends, before
//! anything dispatches again.
use super::*;

use agent_core::auth::{AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use crate::runtime::child_spend::RouteSpend;
use crate::runtime::openai::types::ProviderConfig;

const CODEX: &str = "openai-codex/gpt-5.6-sol";
const COPILOT: &str = "github-copilot/gpt-5-mini";

type CallResult = Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

fn responses_sse(events: &[serde_json::Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn usage(input: u64, cached: u64, output: u64) -> serde_json::Value {
    json!({"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},"output_tokens":output})
}

fn delta(t: &str) -> serde_json::Value {
    json!({"type":"response.output_text.delta","delta":t})
}

fn completed(u: serde_json::Value) -> serde_json::Value {
    json!({"type":"response.completed","response":{"usage":u}})
}

/// Text, then one report: 10 input tokens (4 of them cached), 3 output.
fn answer() -> String {
    responses_sse(&[delta("spend "), delta("ok"), completed(usage(10, 4, 3))])
}

/// A stream cut before its terminal event: no report.
fn cut() -> String {
    responses_sse(&[delta("cut ")])
}

/// Booked Responses counts: input less its cached slice, the slice a cache
/// read; no cache writes, no TTL split.
fn r_tokens(input: u64, output: u64, read: u64) -> SpendTokens {
    chat_tokens(input, output, read)
}

/// Read events until one carries a usage report, then cancel the call (and
/// let it end) or drop it; nothing is booked before.
async fn stop_after_usage(
    call: impl std::future::Future<Output = CallResult>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::StreamEvent>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    log: &Log,
) {
    let mut call = std::pin::pin!(call);
    let usage = async {
        while let Some(event) = rx.recv().await {
            if format!("{event:?}").contains("Usage {") {
                return;
            }
        }
    };
    tokio::select! {
        result = &mut call => panic!("the stalled call ended: {result:?}"),
        () = usage => {}
    }
    assert!(booked(log).is_empty(), "nothing is booked while the attempt runs");
    if let Some(cancel) = cancel {
        cancel.cancel();
        assert!(call.await.is_err(), "a cancelled call ends with an error");
    }
}

// ─── Codex (direct HTTP) ─────────────────────────────────────────────────

/// A JWT-shaped token carrying the ChatGPT account-id claim (not a
/// credential: the stand-in accepts anything).
fn token() -> String {
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct_spend" } });
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

/// One Codex request to `url` with `tap`; its events (debug lines) and
/// result.
async fn codex(
    url: &str,
    max_retries: u32,
    tap: Option<&SpendTap>,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>,
) -> CallResult {
    let broker: Arc<dyn crate::auth::CredentialBroker> = Arc::new(TokenBroker);
    crate::runtime::openai::stream::call_codex_stream_inner(
        &ProviderConfig {
            base_url: url.to_owned(),
            model: "gpt-5.6-sol".to_owned(),
            provider: "openai-codex".to_owned(),
        },
        &reqwest::Client::new(),
        &broker,
        &[],
        &Some("You are a worker.".to_owned()),
        &history(),
        tx,
        None,
        None,
        agent_core::reasoning::ReasoningLevel::Medium,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        cancel,
        max_retries,
        &crate::runtime::trace::TraceContext::disabled(),
        RouteSpend { tap, model: CODEX },
        None,
    )
    .await
}

/// One whole Codex request; its events (debug lines) and result.
async fn codex_run(url: &str, max_retries: u32, tap: Option<&SpendTap>) -> (Vec<String>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = codex(url, max_retries, tap, &Default::default(), &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(format!("{event:?}"));
    }
    (events, format!("{:?}", result.map_err(|e| e.to_string())))
}

/// An answer books one report, priced at the worker's qualified model; the
/// events and the result are the same as without a tap.
#[tokio::test]
async fn codex_an_answer_books_its_usage_once_and_changes_nothing_else() {
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |_| stream_ok(answer())).await;
    let with_tap = codex_run(&url, 0, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(6, 3, 4)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0]);
    let without = codex_run(&url, 0, None).await;
    assert_eq!(with_tap, without, "a tap changes no event and no result");
    assert_eq!(booked(&log).len(), 1, "a call without a tap books nothing");
}

/// A rate-limited send carries no body usage: only the answered resend
/// books.
#[tokio::test]
async fn codex_a_resent_request_books_only_the_answered_attempt() {
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => rate_limited(),
        _ => stream_ok(answer()),
    })
    .await;
    codex_run(&url, 1, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(6, 3, 4)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 0]);
}

/// A cut stream reported nothing, so it books nothing; its retry books.
#[tokio::test]
async fn codex_a_cut_stream_without_a_report_books_nothing() {
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => stream_ok(cut()),
        _ => stream_ok(answer()),
    })
    .await;
    codex_run(&url, 1, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(6, 3, 4)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 0]);
}

/// A completed answer without usable output is paid: it books before its
/// retry is sent.
#[tokio::test]
async fn codex_an_empty_answer_books_before_its_retry_is_sent() {
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => stream_ok(responses_sse(&[completed(usage(20, 0, 1))])),
        _ => stream_ok(answer()),
    })
    .await;
    codex_run(&url, 1, Some(&spend)).await;
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(CODEX, r_tokens(20, 1, 0))),
            (1, priced(CODEX, r_tokens(6, 3, 4))),
        ]
    );
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 1]);
}

/// Two counted terminal reports: the last is booked, once (today's two
/// Usage events stay).
#[tokio::test]
async fn codex_two_counted_terminals_book_the_last_once() {
    let (spend, log) = tap();
    let body = responses_sse(&[
        delta("spend ok"),
        completed(usage(10, 4, 3)),
        json!({"type":"response.done","response":{"usage":usage(11, 4, 5)}}),
    ]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_ok(body.clone())).await;
    let (events, _) = codex_run(&url, 0, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(7, 5, 4)))]);
    assert_eq!(events.iter().filter(|e| e.contains("Usage {")).count(), 2);
}

/// A later report without counts leaves the earlier one standing.
#[tokio::test]
async fn codex_a_count_less_report_leaves_the_earlier_one() {
    let (spend, log) = tap();
    let body = responses_sse(&[
        delta("spend ok"),
        completed(usage(10, 4, 3)),
        json!({"type":"response.done","response":{"usage":{}}}),
    ]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_ok(body.clone())).await;
    codex_run(&url, 0, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(6, 3, 4)))]);
}

/// An explicit zero is a report: it books a zero-cost report.
#[tokio::test]
async fn codex_an_explicit_zero_report_books_a_zero_cost_report() {
    let (spend, log) = tap();
    let body = responses_sse(&[delta("spend ok"), completed(usage(0, 0, 0))]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_ok(body.clone())).await;
    codex_run(&url, 0, Some(&spend)).await;
    let reports = booked(&log);
    assert_eq!(reports, vec![(0, priced(CODEX, r_tokens(0, 0, 0)))]);
    assert_eq!(reports[0].1.cost_nanousd, 0);
}

/// A terminal failure that reports usage books it once.
#[tokio::test]
async fn codex_a_terminal_failure_books_what_it_reported() {
    let (spend, log) = tap();
    let body = responses_sse(&[json!({"type":"response.failed","response":{
        "error":{"type":"server_error","code":"server_error","message":"failed"},
        "usage":usage(9, 0, 2)}})]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_ok(body.clone())).await;
    let (_, result) = codex_run(&url, 0, Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(9, 2, 0)))]);
}

/// A cancel after a report books it once.
#[tokio::test]
async fn codex_a_cancel_after_a_report_books_it() {
    let (spend, log) = tap();
    let head = responses_sse(&[delta("partial"), completed(usage(30, 6, 2))]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_stalled(head.clone())).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_usage(codex(&url, 0, Some(&spend), &cancel, &tx), rx, Some(&cancel), &log).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(24, 2, 6)))]);
}

/// A dropped attempt books what was reported, synchronously, once.
#[tokio::test]
async fn codex_a_dropped_attempt_books_what_was_reported() {
    let (spend, log) = tap();
    let head = responses_sse(&[delta("partial"), completed(usage(40, 0, 1))]);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_stalled(head.clone())).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_usage(codex(&url, 0, Some(&spend), &cancel, &tx), rx, None, &log).await;
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(40, 1, 0)))]);
}

// ─── Broker Responses ────────────────────────────────────────────────────

enum Reply {
    /// A whole stream.
    Sse(String),
    /// A stream that sends this and then stays open.
    Stall(String),
    /// The broker refuses the request.
    Refuse,
}

/// A broker that answers the `n`th streamed request with `script(n)`,
/// recording how many reports were booked when it arrived.
struct ScriptBroker {
    log: Log,
    booked_at: Mutex<Vec<usize>>,
    script: Box<dyn Fn(usize) -> Reply + Send + Sync>,
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
            let mut seen = self.booked_at.lock().unwrap();
            seen.push(drain_into(&self.log));
            seen.len() - 1
        };
        let once = |s: String| futures::stream::iter(vec![Ok(Bytes::from(s))]);
        match (self.script)(n) {
            Reply::Sse(s) => Ok(Box::pin(once(s))),
            Reply::Stall(s) => Ok(Box::pin(once(s).chain(futures::stream::pending()))),
            Reply::Refuse => Err(BrokerError::Denied("refused".into())),
        }
    }
    async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
        Err(BrokerError::Denied("not in this test".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn script_broker(
    log: &Log,
    script: impl Fn(usize) -> Reply + Send + Sync + 'static,
) -> Arc<ScriptBroker> {
    Arc::new(ScriptBroker {
        log: Arc::clone(log),
        booked_at: Mutex::new(Vec::new()),
        script: Box::new(script),
    })
}

/// One GitHub Copilot Responses request through `broker` with `tap`.
async fn responses(
    broker: &Arc<ScriptBroker>,
    max_retries: u32,
    tap: Option<&SpendTap>,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>,
) -> CallResult {
    let broker: Arc<dyn crate::auth::CredentialBroker> = broker.clone();
    crate::runtime::openai::stream::call_responses_route_inner(
        "github-copilot",
        &ProviderConfig {
            base_url: "https://api.githubcopilot.com".to_owned(),
            model: "gpt-5-mini".to_owned(),
            provider: "github-copilot".to_owned(),
        },
        &broker,
        &[],
        &Some("You are a worker.".to_owned()),
        &history(),
        tx,
        None,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        cancel,
        max_retries,
        &crate::runtime::trace::TraceContext::disabled(),
        true,
        RouteSpend {
            tap,
            model: COPILOT,
        },
    )
    .await
}

async fn responses_run(
    broker: &Arc<ScriptBroker>,
    max_retries: u32,
    tap: Option<&SpendTap>,
) -> (Vec<String>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = responses(broker, max_retries, tap, &Default::default(), &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(format!("{event:?}"));
    }
    (events, format!("{:?}", result.map_err(|e| e.to_string())))
}

/// An answer books one report; the events and the result are the same as
/// without a tap.
#[tokio::test]
async fn responses_an_answer_books_its_usage_once_and_changes_nothing_else() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| Reply::Sse(answer()));
    let with_tap = responses_run(&broker, 0, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(6, 3, 4)))]);
    let without = responses_run(&broker, 0, None).await;
    assert_eq!(with_tap, without, "a tap changes no event and no result");
    assert_eq!(booked(&log).len(), 1, "a call without a tap books nothing");
}

/// A capacity error that reports usage is paid: it books before the resend.
#[tokio::test]
async fn responses_a_capacity_retry_books_the_failed_attempt_first() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |n| match n {
        0 => Reply::Sse(responses_sse(&[json!({"type":"error","code":"server_overloaded",
            "message":"at capacity","usage":usage(15, 0, 0)})])),
        _ => Reply::Sse(answer()),
    });
    let (_, result) = responses_run(&broker, 1, Some(&spend)).await;
    assert!(result.starts_with("Ok("), "{result}");
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(COPILOT, r_tokens(15, 0, 0))),
            (1, priced(COPILOT, r_tokens(6, 3, 4))),
        ]
    );
    assert_eq!(broker.booked_at.lock().unwrap().clone(), vec![0, 1]);
}

/// A cut stream without a report books nothing; its retry books.
#[tokio::test]
async fn responses_a_cut_stream_without_a_report_books_nothing() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |n| match n {
        0 => Reply::Sse(cut()),
        _ => Reply::Sse(answer()),
    });
    responses_run(&broker, 1, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(6, 3, 4)))]);
    assert_eq!(broker.booked_at.lock().unwrap().clone(), vec![0, 0]);
}

/// A request the broker refuses opens no stream: nothing is booked.
#[tokio::test]
async fn responses_a_refused_request_books_nothing() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| Reply::Refuse);
    let (_, result) = responses_run(&broker, 0, Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert!(booked(&log).is_empty());
}

/// A terminal failure that reports usage books it once.
#[tokio::test]
async fn responses_a_terminal_failure_books_what_it_reported() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| {
        Reply::Sse(responses_sse(&[json!({"type":"response.failed","response":{
            "error":{"type":"server_error","code":"server_error","message":"failed"},
            "usage":usage(9, 0, 2)}})]))
    });
    let (_, result) = responses_run(&broker, 0, Some(&spend)).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(9, 2, 0)))]);
}

/// Two counted terminal reports: the last is booked, once.
#[tokio::test]
async fn responses_two_counted_terminals_book_the_last_once() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| {
        Reply::Sse(responses_sse(&[
            delta("spend ok"),
            completed(usage(10, 4, 3)),
            json!({"type":"response.done","response":{"usage":usage(11, 4, 5)}}),
        ]))
    });
    responses_run(&broker, 0, Some(&spend)).await;
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(7, 5, 4)))]);
}

/// A cancel after a report books it once.
#[tokio::test]
async fn responses_a_cancel_after_a_report_books_it() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| {
        Reply::Stall(responses_sse(&[delta("partial"), completed(usage(30, 6, 2))]))
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_usage(
        responses(&broker, 0, Some(&spend), &cancel, &tx),
        rx,
        Some(&cancel),
        &log,
    )
    .await;
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(24, 2, 6)))]);
}

/// A dropped attempt books what was reported, synchronously, once.
#[tokio::test]
async fn responses_a_dropped_attempt_books_what_was_reported() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| {
        Reply::Stall(responses_sse(&[delta("partial"), completed(usage(40, 0, 1))]))
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_usage(responses(&broker, 0, Some(&spend), &cancel, &tx), rx, None, &log).await;
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(40, 1, 0)))]);
}

/// The empty attempt is booked before the retry's wait starts, so a check
/// during the backoff sees it.
#[tokio::test]
async fn codex_an_empty_attempt_is_booked_before_the_retry_waits() {
    let (spend, log) = tap();
    let (url, _) = stand_in(Arc::clone(&log), |n| match n {
        0 => stream_ok(responses_sse(&[completed(usage(20, 0, 1))])),
        _ => stream_ok(answer()),
    })
    .await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = async {
        while let Some(event) = rx.recv().await {
            if format!("{event:?}").contains("retrying") {
                return Some(booked(&log).len());
            }
        }
        None
    };
    let (result, at_wait) = tokio::join!(codex(&url, 1, Some(&spend), &cancel, &tx), watch);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(at_wait, Some(1), "booked before the retry waited");
}

// ─── The route dispatch ──────────────────────────────────────────────────

/// The route dispatch hands the Codex arm the worker's guard.
#[tokio::test]
async fn codex_the_route_dispatch_hands_the_arm_the_guard() {
    let (spend, log) = tap();
    let (url, _) = stand_in(Arc::clone(&log), |_| stream_ok(answer())).await;
    let mut route = crate::runtime::openai::resolve_route(CODEX).expect("a resolved route");
    route.endpoint = url;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = dispatched(&route, Arc::new(TokenBroker), &tx, Some(&spend), CODEX).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(booked(&log), vec![(0, priced(CODEX, r_tokens(6, 3, 4)))]);
}

/// The route dispatch hands the broker Responses arm the worker's guard.
#[tokio::test]
async fn responses_the_route_dispatch_hands_the_arm_the_guard() {
    let (spend, log) = tap();
    let broker = script_broker(&log, |_| Reply::Sse(answer()));
    let route = crate::runtime::openai::resolve_route(COPILOT).expect("a resolved route");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = dispatched(&route, broker, &tx, Some(&spend), COPILOT).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(booked(&log), vec![(0, priced(COPILOT, r_tokens(6, 3, 4)))]);
}
