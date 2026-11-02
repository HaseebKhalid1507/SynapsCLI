//! The child-spend off gate (P1b): a runtime without a spend tap (every
//! session's own requests, every host without a sink, and every worker
//! before P1 installs one) sends the same requests and emits the same
//! events as before child spend existed. Each case runs one real streamed
//! request against a loopback stand-in (or an in-process broker or
//! extension) and compares what the provider received (method, path,
//! headers, the exact body bytes), the request's event sequence and those
//! events' client-wire lines, plus the result where the route returns one,
//! with fixtures recorded on the pre-P1 code.
//!
//! Provenance: the fixtures were written at 5f49b0a7 (before child spend),
//! twice, with identical results, by
//! `SPEND_GATE_WRITE=1 cargo test -p synaps-engine --lib spend_gate`. On the
//! dev line, the six retry cases' `_events` and `_wire` lines were written
//! again for dev's typed retry event (f556b322: `Retry` in process, a notice
//! with `"retry":true` on the wire), one line each and nothing else. Write
//! them again only when a request or event change is intended and reviewed.
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Router;
use futures::StreamExt;
use serde_json::json;

use crate::SharedMessage;
use agent_core::core::config::ContextManagementMode;

/// One request as the stand-in received it.
struct Received {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

/// Headers that differ between runs for reasons unrelated to the request:
/// the stand-in's ephemeral port.
const VOLATILE_HEADERS: &[&str] = &["host"];

fn render_requests(requests: &[Received]) -> String {
    let mut out = String::new();
    for (i, r) in requests.iter().enumerate() {
        out.push_str(&format!("=== request {i}\n{} {}\n", r.method, r.uri));
        let mut headers: Vec<(String, String)> = r
            .headers
            .iter()
            .filter(|(name, _)| !VOLATILE_HEADERS.contains(&name.as_str()))
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        headers.sort();
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\n"));
        }
        out.push_str(&format!("--- body ({} bytes)\n", r.body.len()));
        out.push_str(&String::from_utf8_lossy(&r.body));
        out.push('\n');
    }
    out
}

/// The stand-in: records every request and answers the `n`th with
/// `reply(n)`.
async fn stand_in(
    reply: impl Fn(usize) -> axum::response::Response + Clone + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Received>>>) {
    let seen: Arc<Mutex<Vec<Received>>> = Arc::default();
    let record = Arc::clone(&seen);
    let app = Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let record = Arc::clone(&record);
            let reply = reply.clone();
            async move {
                let n = {
                    let mut seen = record.lock().unwrap();
                    seen.push(Received {
                        method,
                        uri,
                        headers,
                        body,
                    });
                    seen.len() - 1
                };
                reply(n)
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn stream_ok(body: String) -> axum::response::Response {
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        body,
    )
        .into_response()
}

/// A rate-limit answer that asks for an immediate retry.
fn rate_limited(body: serde_json::Value) -> axum::response::Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("content-type", "application/json"), ("retry-after", "0")],
        body.to_string(),
    )
        .into_response()
}

fn anthropic_sse() -> String {
    let usage = json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":0}});
    [
        json!({"type":"message_start","message":{"id":"msg_gate","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"gate "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

/// An Anthropic stream the provider ends with an overload error before any
/// content: a retryable stream error, after a usage report.
fn anthropic_overloaded_sse() -> String {
    let usage = json!({"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":4,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":0}});
    [
        json!({"type":"message_start","message":{"id":"msg_over","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

fn chat_sse() -> String {
    [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"gate "},"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_owned()))
    .collect()
}

/// A Chat Completions stream cut off after its first chunk: no finish
/// reason, no `[DONE]`.
fn chat_cut_sse() -> String {
    let chunk = json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"gate "},"finish_reason":null}]});
    format!("data: {chunk}\n\n")
}

fn history() -> Vec<SharedMessage> {
    vec![
        Arc::new(json!({"role":"user","content":"What's the capital of France?"})),
        Arc::new(json!({"role":"assistant","content":[{"type":"text","text":"Paris."}]})),
        Arc::new(json!({"role":"user","content":"And of Japan?"})),
    ]
}

/// Every event, one per line, as emitted, and in the daemon's client-wire
/// encoding (the line a TUI or desktop client would read).
fn render_events(events: Vec<crate::StreamEvent>) -> (String, String) {
    let (mut debug, mut wire) = (String::new(), String::new());
    for event in events {
        debug.push_str(&format!("{event:?}\n"));
        let line = crate::session::wire::WireStreamEvent::for_client(event)
            .expect("a spend-gate event has a client form");
        wire.push_str(&crate::session::wire::encode_line(&line).unwrap());
    }
    (debug, wire)
}

/// One streamed turn on `model`. The runtime's tools are pinned to two
/// (`read`, `ls`) so the fixtures follow the transport, not the tool
/// catalog.
async fn turn(model: &str) -> (String, String) {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model(model.to_owned());
    rt.set_api_retries(1);
    // Upstream's message times (3bc5a7bcf) stamp each new message with the
    // runtime's clock; a fixed clock keeps the pinned lines byte-stable.
    rt.set_message_clock(crate::runtime::message_time::MessageClock::new(|| {
        chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap()
    }));
    rt.set_tools(crate::tools::ToolRegistry::from_tools_for_tests(vec![
        Arc::new(crate::tools::ReadTool),
        Arc::new(crate::tools::LsTool),
    ]));
    rt.continuation.lock().unwrap().config.mode = ContextManagementMode::Off;
    let stream = rt
        .run_stream_with_messages(history(), Default::default(), None, None, false)
        .await;
    let events: Vec<crate::StreamEvent> = stream.collect().await;
    let (events, wire) = render_events(events);
    (stable(&events), stable(&wire))
}

/// `text` without what differs between runs for reasons unrelated to the
/// request: a turn error's correlation ID carries the process ID and the
/// process's turn count.
fn stable(text: &str) -> String {
    regex::Regex::new(r"turn-\d+-\d+")
        .unwrap()
        .replace_all(text, "turn-<pid>-<n>")
        .into_owned()
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/spend_gate")
        .join(name)
}

/// Compare `actual` with the fixture `name`, or write it under
/// `SPEND_GATE_WRITE=1`.
fn check(name: &str, actual: &str) {
    let path = fixture(name);
    if std::env::var_os("SPEND_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{name} differs from the pre-P1 fixture at line {}:\n  expected: {:?}\n  actual:   {:?}",
            line + 1,
            expected.lines().nth(line),
            actual.lines().nth(line),
        );
    }
}

fn anthropic_429() -> serde_json::Value {
    json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}})
}

fn chat_429() -> serde_json::Value {
    json!({"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}})
}

/// Run one turn on `model` against `reply`, with `env` pointing the route
/// at the stand-in; compare the requests and events with `name`'s fixtures.
async fn gate(
    name: &str,
    model: &str,
    env: &'static str,
    path: &str,
    reply: impl Fn(usize) -> axum::response::Response + Clone + Send + Sync + 'static,
) {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, seen) = stand_in(reply).await;
    let _url = crate::test_env::EnvVarGuard::set(env, &format!("{url}{path}"));
    let (events, wire) = turn(model).await;
    let requests = render_requests(&seen.lock().unwrap());
    check(&format!("{name}_requests.txt"), &requests);
    check(&format!("{name}_events.txt"), &events);
    check(&format!("{name}_wire.txt"), &wire);
}

/// A streamed Anthropic turn with no tap: the same requests (headers and
/// body bytes) and the same events, plain `Usage` included.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_anthropic_stream_is_unchanged_without_a_tap() {
    gate(
        "anthropic",
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        |_| stream_ok(anthropic_sse()),
    )
    .await;
}

/// The same with a rate-limited first attempt and its retry (the request
/// loop, where a guard is created per send).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_anthropic_retry_is_unchanged_without_a_tap() {
    gate(
        "anthropic_retry",
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        |n| match n {
            0 => rate_limited(anthropic_429()),
            _ => stream_ok(anthropic_sse()),
        },
    )
    .await;
}

/// The same with a stream the provider ends with an overload error after a
/// usage report, and its retry (the residual Usage, then the stream loop).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_anthropic_stream_retry_is_unchanged_without_a_tap() {
    gate(
        "anthropic_stream_retry",
        "claude-sonnet-4-6",
        "SYNAPS_ANTHROPIC_BASE_URL",
        "",
        |n| match n {
            0 => stream_ok(anthropic_overloaded_sse()),
            _ => stream_ok(anthropic_sse()),
        },
    )
    .await;
}

/// An OpenAI-compatible Chat Completions turn (a `local/` model through the
/// broker) with no tap.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_openai_chat_stream_is_unchanged_without_a_tap() {
    gate(
        "openai_chat",
        "local/test-model",
        "LOCAL_ENDPOINT",
        "/v1",
        |_| stream_ok(chat_sse()),
    )
    .await;
}

/// The same with a stream cut off before it completed and its retry.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_openai_chat_stream_retry_is_unchanged_without_a_tap() {
    gate(
        "openai_chat_stream_retry",
        "local/test-model",
        "LOCAL_ENDPOINT",
        "/v1",
        |n| match n {
            0 => stream_ok(chat_cut_sse()),
            _ => stream_ok(chat_sse()),
        },
    )
    .await;
}

/// The same with a rate-limited request, which this route ends the turn on
/// (no retry): a failed attempt.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_openai_chat_rate_limit_is_unchanged_without_a_tap() {
    gate(
        "openai_chat_rate_limit",
        "local/test-model",
        "LOCAL_ENDPOINT",
        "/v1",
        |_| rate_limited(chat_429()),
    )
    .await;
}

/// The Codex transport's gate.
#[path = "spend_gate_codex_tests.rs"]
mod codex;

/// The Gemini transport's gate.
#[path = "spend_gate_gemini_tests.rs"]
mod gemini;

/// The extension provider route's gate.
#[path = "spend_gate_extension_tests.rs"]
mod extension;

/// The broker Responses transport's gate.
#[path = "spend_gate_responses_tests.rs"]
mod responses;
