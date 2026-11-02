//! Child spend, route by route (P1d): a worker runtime with a spend tap
//! books each paid attempt's reported usage once, at the attempt's end,
//! before anything dispatches again; with no report it books nothing.
//! Each case runs real requests against a loopback stand-in and reads what
//! the session's spend receiver got.
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::StreamExt;
use serde_json::json;

use super::child_spend::{
    SinkMessage, SpendKind, SpendReceiver, SpendReport, SpendSink, SpendTap, SpendTokens,
};
use crate::SharedMessage;
use agent_core::core::config::ContextManagementMode;

/// The stand-in: answers the `n`th request with `reply(n, booked)`, where
/// `booked` is the number of reports the spend receiver held when request
/// `n` arrived (drained into the shared log first).
type Log = Arc<Mutex<(SpendReceiver, Vec<SinkMessage>)>>;

async fn stand_in(
    log: Log,
    reply: impl Fn(usize) -> Response + Clone + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<usize>>>) {
    let booked_at: Arc<Mutex<Vec<usize>>> = Arc::default();
    let seen = Arc::clone(&booked_at);
    let app = Router::new().fallback(
        move |_method: Method, _uri: Uri, _headers: HeaderMap, _body: Bytes| {
            let seen = Arc::clone(&seen);
            let log = Arc::clone(&log);
            let reply = reply.clone();
            async move {
                let n = {
                    let mut seen = seen.lock().unwrap();
                    let booked = drain_into(&log);
                    seen.push(booked);
                    seen.len() - 1
                };
                reply(n)
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), booked_at)
}

/// Move what the receiver holds into the log; the number of spend reports
/// logged so far.
fn drain_into(log: &Log) -> usize {
    let mut guard = log.lock().unwrap();
    let (rx, messages) = &mut *guard;
    while let Ok(message) = rx.try_recv() {
        messages.push(message);
    }
    messages
        .iter()
        .filter(|m| matches!(m, SinkMessage::Spend { .. }))
        .count()
}

fn tap() -> (SpendTap, Log) {
    let (sink, rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "worker").unwrap();
    (SpendTap::new(lease), Arc::new(Mutex::new((rx, Vec::new()))))
}

/// The booked reports, in order.
fn booked(log: &Log) -> Vec<(u64, SpendReport)> {
    drain_into(log);
    log.lock()
        .unwrap()
        .1
        .iter()
        .filter_map(|m| match m {
            SinkMessage::Spend { seq, report, .. } => Some((*seq, report.clone())),
            _ => None,
        })
        .collect()
}

fn tokens(input: u64, output: u64, read: u64, write: u64) -> SpendTokens {
    SpendTokens {
        input,
        output,
        cache_read: read,
        cache_creation: write,
        cache_creation_5m: Some(write),
        cache_creation_1h: Some(0),
    }
}

fn stream_ok(body: String) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        body,
    )
        .into_response()
}

/// A stream that sends `head` and then stays open without another byte.
fn stream_stalled(head: String) -> Response {
    let chunks = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(head))])
        .chain(futures::stream::pending());
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(chunks))
        .unwrap()
}

fn rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("content-type", "application/json"), ("retry-after", "0")],
        json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}})
            .to_string(),
    )
        .into_response()
}

fn sse(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn usage(input: u64, output: u64, read: u64, write: u64) -> serde_json::Value {
    json!({"input_tokens":input,"output_tokens":output,"cache_read_input_tokens":read,
        "cache_creation_input_tokens":write,
        "cache_creation":{"ephemeral_5m_input_tokens":write,"ephemeral_1h_input_tokens":0}})
}

fn start(u: serde_json::Value) -> serde_json::Value {
    json!({"type":"message_start","message":{"id":"msg_spend","type":"message","role":"assistant",
        "content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":u}})
}

fn text(t: &str) -> Vec<serde_json::Value> {
    vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":t}}),
    ]
}

/// A whole Anthropic answer: start (input side), text, delta (final counts).
fn anthropic_ok(stop: &str) -> String {
    let mut events = vec![start(usage(12, 1, 4, 5))];
    events.extend(text("spend ok"));
    events.push(json!({"type":"content_block_stop","index":0}));
    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},
        "usage":usage(12, 3, 4, 5)}));
    events.push(json!({"type":"message_stop"}));
    sse(&events)
}

/// A start report, then a retryable overload error before any content.
fn anthropic_overloaded() -> String {
    sse(&[
        start(usage(20, 0, 6, 7)),
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
    ])
}

fn history() -> Vec<SharedMessage> {
    vec![Arc::new(json!({"role":"user","content":"Say spend ok."}))]
}

fn runtime(model: &str, spend: Option<SpendTap>) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(model.to_owned());
    rt.set_api_retries(1);
    // PARITY (upstream 3bc5a7bcf message times): each new message is stamped
    // with the runtime's clock, so two turns compared event for event take
    // one fixed clock, or a second boundary between them tells them apart.
    rt.set_message_clock(crate::runtime::message_time::MessageClock::new(|| {
        chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap()
    }));
    rt.set_tools(crate::tools::ToolRegistry::from_tools_for_tests(vec![
        Arc::new(crate::tools::ReadTool),
    ]));
    rt.continuation.lock().unwrap().config.mode = ContextManagementMode::Off;
    rt.set_spend_tap(spend);
    rt
}

/// One turn; every event as a debug line (correlation ids made stable).
async fn turn(rt: &crate::Runtime, cancel: tokio_util::sync::CancellationToken) -> Vec<String> {
    let stream = rt
        .run_stream_with_messages(history(), cancel, None, None, false)
        .await;
    let events: Vec<crate::StreamEvent> = stream.collect().await;
    events
        .iter()
        .map(|e| {
            regex::Regex::new(r"turn-\d+-\d+")
                .unwrap()
                .replace_all(&format!("{e:?}"), "turn-<n>")
                .into_owned()
        })
        .collect()
}

fn usage_lines(events: &[String]) -> Vec<&String> {
    events.iter().filter(|e| e.contains("Usage {")).collect()
}

fn priced(model: &str, t: SpendTokens) -> SpendReport {
    SpendReport::priced(SpendKind::Turn, model, t)
}

const SONNET: &str = "claude-sonnet-4-6";

/// One turn, cancelled as soon as an event carries the text "partial"; every
/// event as a debug line.
async fn cancel_at_partial(rt: &crate::Runtime) -> Vec<String> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stream = rt
        .run_stream_with_messages(history(), cancel.clone(), None, None, false)
        .await;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let line = format!("{event:?}");
        let partial = line.contains("partial");
        events.push(line);
        if partial {
            cancel.cancel();
        }
    }
    events
}

/// One streamed request on `model`, its future dropped once an event
/// carries the text "partial" (nothing is booked before the drop).
async fn drop_at_partial(model: &str, options: &super::api::ApiOptions, log: &Log) {
    let auth = Arc::new(tokio::sync::RwLock::new(super::types::AuthState {
        auth_token: "sk-test".to_string(),
        auth_type: "api_key".to_string(),
        refresh_token: None,
        token_expires: Some(9_999_999_999_999),
        bound_credential: None,
        auth_file_mtime: None,
    }));
    let client = reqwest::Client::new();
    let tools = crate::tools::ToolRegistry::new();
    let messages = history();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let call = super::api::ApiMethods::call_api_stream_inner(
        &auth,
        &client,
        model,
        &tools,
        &None,
        0,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &messages,
        tx,
        &cancel,
        0,
        0,
        options,
        super::telemetry::TelemetryLevel::Off,
    );
    let mut call = std::pin::pin!(call);
    let partial = async {
        while let Some(event) = events.recv().await {
            if format!("{event:?}").contains("partial") {
                return;
            }
        }
    };
    tokio::select! {
        result = &mut call => panic!("the stalled call ended: {result:?}"),
        () = partial => {}
    }
    assert!(booked(log).is_empty(), "nothing is booked while the attempt runs");
}

// ─── Native Anthropic ────────────────────────────────────────────────────

/// An ordinary answer books one report: the counts the turn's Usage event
/// carries, priced at the worker's model. The turn's events are the same as
/// without a tap.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_an_answer_books_its_usage_once_and_changes_no_event() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |_| stream_ok(anthropic_ok("end_turn"))).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let with_tap = turn(&runtime(SONNET, Some(spend)), Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(SONNET, tokens(12, 3, 4, 5)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0]);
    let without = turn(&runtime(SONNET, None), Default::default()).await;
    assert_eq!(with_tap, without, "a tap changes no event");
    assert_eq!(usage_lines(&with_tap).len(), 1);
    assert_eq!(booked(&log).len(), 1, "a runtime without a tap books nothing");
}

/// An HTTP error answer carries no usage: only the answered retry books.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_an_http_retry_books_only_the_answered_attempt() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => rate_limited(),
        _ => stream_ok(anthropic_ok("end_turn")),
    })
    .await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    turn(&runtime(SONNET, Some(spend)), Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(SONNET, tokens(12, 3, 4, 5)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 0]);
}

/// A stream the provider cuts with an overload after its start report: that
/// attempt books the start's counts before the retry is sent; the retry
/// books its own.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_stream_retry_books_the_cut_attempt_before_the_retry_is_sent() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => stream_ok(anthropic_overloaded()),
        _ => stream_ok(anthropic_ok("end_turn")),
    })
    .await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let events = turn(&runtime(SONNET, Some(spend)), Default::default()).await;
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(SONNET, tokens(20, 0, 6, 7))),
            (1, priced(SONNET, tokens(12, 3, 4, 5))),
        ]
    );
    assert_eq!(
        booked_at.lock().unwrap().clone(),
        vec![0, 1],
        "the cut attempt was booked before the retry reached the provider"
    );
    assert_eq!(usage_lines(&events).len(), 2, "one Usage event per attempt, as before");
}

/// A refused answer (stop reason `refusal`) is paid: it books before the
/// refusal retry is sent.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_refusal_retry_books_the_refused_attempt_first() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (url, booked_at) = stand_in(Arc::clone(&log), |n| match n {
        0 => stream_ok(anthropic_ok("refusal")),
        _ => stream_ok(anthropic_ok("end_turn")),
    })
    .await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    turn(&runtime(SONNET, Some(spend)), Default::default()).await;
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(SONNET, tokens(12, 3, 4, 5))),
            (1, priced(SONNET, tokens(12, 3, 4, 5))),
        ]
    );
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 1]);
}

/// A cancel after the start report: the attempt books the start's counts
/// once, the same counts as the turn's residual Usage event.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_cancel_after_the_start_report_books_the_start_counts() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let mut head = vec![start(usage(30, 1, 2, 3))];
    head.extend(text("partial"));
    let head = sse(&head);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_stalled(head.clone())).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let events = cancel_at_partial(&runtime(SONNET, Some(spend))).await;
    assert_eq!(booked(&log), vec![(0, priced(SONNET, tokens(30, 1, 2, 3)))]);
    let lines = usage_lines(&events);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("input_tokens: 30"), "{lines:?}");
}

/// A dropped attempt (its future dropped mid-stream, e.g. a parent's drop)
/// books what its start reported, synchronously, once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_dropped_attempt_books_what_its_start_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let mut head = vec![start(usage(40, 1, 0, 9))];
    head.extend(text("partial"));
    let head = sse(&head);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_stalled(head.clone())).await;
    let options = super::api::ApiOptions {
        anthropic_base_url: Some(url),
        spend_tap: Some(spend),
        ..Default::default()
    };
    drop_at_partial(SONNET, &options, &log).await;
    assert_eq!(booked(&log), vec![(0, priced(SONNET, tokens(40, 1, 0, 9)))]);
}

// ─── OpenAI-compatible Chat Completions ──────────────────────────────────

/// A Chat Completions model served by the stand-in (`LOCAL_ENDPOINT`).
const LOCAL: &str = "local/test-model";

fn chat_text(t: &str) -> serde_json::Value {
    json!({"id":"c","object":"chat.completion.chunk",
        "choices":[{"index":0,"delta":{"role":"assistant","content":t},"finish_reason":null}]})
}

fn chat_stop() -> serde_json::Value {
    json!({"id":"c","object":"chat.completion.chunk",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
}

fn chat_usage(prompt: u64, completion: u64, cached: u64) -> serde_json::Value {
    json!({"id":"c","object":"chat.completion.chunk","choices":[],
        "usage":{"prompt_tokens":prompt,"completion_tokens":completion,
            "total_tokens":prompt + completion,"prompt_tokens_details":{"cached_tokens":cached}}})
}

fn chat_sse(chunks: &[serde_json::Value], done: bool) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

/// A whole answer whose usage report states 12 prompt tokens (4 of them
/// cached) and 3 completion tokens.
fn chat_ok() -> String {
    chat_sse(&[chat_text("spend ok"), chat_stop(), chat_usage(12, 3, 4)], true)
}

/// A stream cut after a usage report: no finish reason, no `[DONE]`.
fn chat_cut(prompt: u64, completion: u64, cached: u64) -> String {
    chat_sse(&[chat_text("cut "), chat_usage(prompt, completion, cached)], false)
}

fn chat_rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("content-type", "application/json"), ("retry-after", "0")],
        json!({"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}})
            .to_string(),
    )
        .into_response()
}

/// Chat Completions counts as booked: the prompt less its cached slice is
/// input, the cached slice a cache read; the route reports no cache writes
/// and no TTL split.
fn chat_tokens(input: u64, output: u64, read: u64) -> SpendTokens {
    SpendTokens {
        input,
        output,
        cache_read: read,
        cache_creation: 0,
        cache_creation_5m: None,
        cache_creation_1h: None,
    }
}

async fn chat_stand_in(
    log: &Log,
    reply: impl Fn(usize) -> Response + Clone + Send + Sync + 'static,
) -> (Arc<Mutex<Vec<usize>>>, crate::test_env::EnvVarGuard) {
    let (url, booked_at) = stand_in(Arc::clone(log), reply).await;
    let env = crate::test_env::EnvVarGuard::set("LOCAL_ENDPOINT", &format!("{url}/v1"));
    (booked_at, env)
}

/// An ordinary answer books one report, the counts of the turn's Usage
/// event, priced at the worker's qualified model; the turn's events are the
/// same as without a tap.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_an_answer_books_its_usage_once_and_changes_no_event() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (booked_at, _env) = chat_stand_in(&log, |_| stream_ok(chat_ok())).await;
    let with_tap = turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(LOCAL, chat_tokens(8, 3, 4)))]);
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0]);
    let without = turn(&runtime(LOCAL, None), Default::default()).await;
    assert_eq!(with_tap, without, "a tap changes no event");
    let lines = usage_lines(&with_tap);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("input_tokens: 8"), "{lines:?}");
    assert_eq!(booked(&log).len(), 1, "a runtime without a tap books nothing");
}

/// A stream cut after its usage report: that attempt books before the retry
/// is sent; the retry books its own.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_stream_retry_books_the_cut_attempt_before_the_retry_is_sent() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (booked_at, _env) = chat_stand_in(&log, |n| match n {
        0 => stream_ok(chat_cut(20, 1, 0)),
        _ => stream_ok(chat_ok()),
    })
    .await;
    turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(LOCAL, chat_tokens(20, 1, 0))),
            (1, priced(LOCAL, chat_tokens(8, 3, 4))),
        ]
    );
    assert_eq!(
        booked_at.lock().unwrap().clone(),
        vec![0, 1],
        "the cut attempt was booked before the retry reached the provider"
    );
}

/// A turn that fails once its stream retries are spent books every attempt
/// it paid for.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_failed_turn_books_each_attempt_that_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (booked_at, _env) = chat_stand_in(&log, |n| match n {
        0 => stream_ok(chat_cut(20, 1, 0)),
        _ => stream_ok(chat_cut(21, 2, 5)),
    })
    .await;
    let events = turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    assert!(
        events.iter().any(|e| e.contains("connection interrupted")),
        "the turn failed: {events:?}"
    );
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(LOCAL, chat_tokens(20, 1, 0))),
            (1, priced(LOCAL, chat_tokens(16, 2, 5))),
        ]
    );
    assert_eq!(booked_at.lock().unwrap().clone(), vec![0, 1]);
}

/// A rate-limited request opens no stream and reports no usage: nothing is
/// booked.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_rate_limited_request_books_nothing() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (booked_at, _env) = chat_stand_in(&log, |_| chat_rate_limited()).await;
    turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    assert_eq!(booked_at.lock().unwrap().len(), 1, "one request, no retry");
    assert!(booked(&log).is_empty());
}

/// Two usage reports in one stream: the attempt books the last, once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_the_last_usage_report_of_an_attempt_is_booked_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let body = chat_sse(
        &[
            chat_text("spend ok"),
            chat_usage(10, 1, 0),
            chat_stop(),
            chat_usage(12, 3, 4),
        ],
        true,
    );
    let (_, _env) = chat_stand_in(&log, move |_| stream_ok(body.clone())).await;
    turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(LOCAL, chat_tokens(8, 3, 4)))]);
}

/// An explicit zero is a report: it books a zero-cost report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_an_explicit_zero_report_books_a_zero_cost_report() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let body = chat_sse(&[chat_text("spend ok"), chat_stop(), chat_usage(0, 0, 0)], true);
    let (_, _env) = chat_stand_in(&log, move |_| stream_ok(body.clone())).await;
    turn(&runtime(LOCAL, Some(spend)), Default::default()).await;
    let reports = booked(&log);
    assert_eq!(reports, vec![(0, priced(LOCAL, chat_tokens(0, 0, 0)))]);
    assert_eq!(reports[0].1.cost_nanousd, 0);
}

/// A cancel after a usage report books it once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_cancel_after_a_usage_report_books_it() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let head = chat_sse(&[chat_usage(30, 2, 6), chat_text("partial")], false);
    let (_, _env) = chat_stand_in(&log, move |_| stream_stalled(head.clone())).await;
    cancel_at_partial(&runtime(LOCAL, Some(spend))).await;
    assert_eq!(booked(&log), vec![(0, priced(LOCAL, chat_tokens(24, 2, 6)))]);
}

/// A dropped attempt books what was reported, synchronously, once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_dropped_attempt_books_what_was_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let head = chat_sse(&[chat_usage(40, 1, 0), chat_text("partial")], false);
    let (_, _env) = chat_stand_in(&log, move |_| stream_stalled(head.clone())).await;
    let options = super::api::ApiOptions {
        spend_tap: Some(spend),
        ..Default::default()
    };
    drop_at_partial(LOCAL, &options, &log).await;
    assert_eq!(booked(&log), vec![(0, priced(LOCAL, chat_tokens(40, 1, 0)))]);
}

/// The cut attempt is booked before the retry's wait starts, so a check
/// during the backoff sees it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn chat_a_cut_attempt_is_booked_before_the_retry_waits() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let (_, _env) = chat_stand_in(&log, |n| match n {
        0 => stream_ok(chat_cut(20, 1, 0)),
        _ => stream_ok(chat_ok()),
    })
    .await;
    let rt = runtime(LOCAL, Some(spend));
    let mut stream = rt
        .run_stream_with_messages(history(), Default::default(), None, None, false)
        .await;
    let mut at_wait = None;
    while let Some(event) = stream.next().await {
        if at_wait.is_none() && format!("{event:?}").contains("retrying") {
            at_wait = Some(booked(&log).len());
        }
    }
    assert_eq!(at_wait, Some(1), "booked before the retry waited");
}

/// The Responses wire: the Codex transport and the broker Responses
/// transport.
#[path = "spend_route_responses_tests.rs"]
mod responses;

/// One request on `route` through `broker`, as the router sends it, with
/// `tap` for `model`: the route's arm must hand each attempt the guard.
async fn dispatched(
    route: &crate::runtime::openai::ResolvedRoute,
    broker: Arc<dyn crate::auth::CredentialBroker>,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>,
    tap: Option<&SpendTap>,
    model: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    crate::runtime::openai::dispatch_route(
        route,
        &reqwest::Client::new(),
        &broker,
        true,
        &Arc::new(Vec::new()),
        &Some("You are a worker.".to_owned()),
        &history(),
        tx,
        None,
        None,
        0,
        agent_core::reasoning::ReasoningLevel::Medium,
        &tokio_util::sync::CancellationToken::new(),
        0,
        crate::runtime::openai::catalog::CodexRequestRole::Foreground,
        &crate::runtime::trace::TraceContext::disabled(),
        false,
        crate::runtime::openai::AttemptPolicy::Configured,
        crate::runtime::child_spend::RouteSpend { tap, model },
        None,
    )
    .await
    .expect("a routed wire")
}

/// The Gemini (Code Assist) route.
#[path = "spend_route_gemini_tests.rs"]
mod gemini;

/// The extension provider route.
#[path = "spend_route_extension_tests.rs"]
mod extension;

/// The native route against the root's own booking (the native exception).
#[path = "spend_route_native_tests.rs"]
mod native;
