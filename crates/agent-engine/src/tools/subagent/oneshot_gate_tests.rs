// The oneshot gate: included at the end of oneshot.rs as `mod oneshot_gate`.
// A oneshot whose parent keeps waiting sends the same provider requests,
// gives its parent the same events, returns the same result text and writes
// the same log as before the drop guard. Each case runs the real `subagent`
// tool, its worker runtime built as in production (no host: the fresh
// runtime), against a loopback provider stand-in.
//
// Fixtures in tests/fixtures/oneshot_gate/ were written with
// ONESHOT_GATE_WRITE=1 at the base of this fix, twice, with no difference
// between the two writes. Normalization: the stand-in's `host` header; the
// subagent id (a process-wide counter); elapsed seconds; the log file's
// timestamp; a turn error's correlation ID. Nothing else.
//
// Re-pinned for subagent thinking (fdb275f4, train 26.3: a worker's effort
// defaults to subagent_thinking, xhigh) by an edit of each *_requests.txt,
// not by a write: each request's `thinking.budget_tokens` is 32768 (xhigh;
// was 4096, the medium default), so its content-length and body size are
// one byte more. Nothing else changed: the events, result and log fixtures
// are as written. This gate passing at the re-pin's tree (3434f892 plus the
// edited fixtures, no production change) shows each fixture is byte for
// byte what ONESHOT_GATE_WRITE=1 would write.

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::tools::Tool;

const ONESHOT_GATE_DIR: &str = "tests/fixtures/oneshot_gate";

struct Received {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

type Seen = Arc<Mutex<Vec<Received>>>;

/// The stand-in: records every request and answers the `n`th with `reply(n)`.
async fn stand_in(reply: impl Fn(usize) -> Response + Clone + Send + Sync + 'static) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let record = Arc::clone(&record);
        let reply = reply.clone();
        async move {
            let n = {
                let mut seen = record.lock().unwrap();
                seen.push(Received { method, uri, headers, body });
                seen.len() - 1
            };
            reply(n)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), seen)
}

fn sse_events(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn usage() -> Value {
    json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":0}})
}

fn message_start(n: usize) -> Value {
    json!({"type":"message_start","message":{"id":format!("msg_gate_{n}"),"type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage()}})
}

/// A reply that asks for the `read` tool on a path that does not exist.
fn tool_use_reply(n: usize) -> Response {
    let body = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"looking. "}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":format!("toolu_gate_{n}"),"name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/nonexistent/oneshot-gate.txt\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// A reply that answers in text and ends the turn.
fn text_reply(n: usize) -> Response {
    let body = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"gate "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

/// A reply that starts its text and then never finishes.
fn held_reply(n: usize) -> Response {
    let head = sse_events(&[
        message_start(n),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial answer"}}),
    ]);
    let stream = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(head))])
        .chain(futures::stream::pending());
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// A provider refusal of the request itself.
fn invalid_reply() -> Response {
    (
        StatusCode::BAD_REQUEST,
        [("content-type", "application/json")],
        json!({"type":"error","error":{"type":"invalid_request_error","message":"gate: the request is not valid"}})
            .to_string(),
    )
        .into_response()
}

fn render_requests(requests: &[Received]) -> String {
    let mut out = String::new();
    for (i, r) in requests.iter().enumerate() {
        out.push_str(&format!("=== request {i}\n{} {}\n", r.method, r.uri));
        let mut headers: Vec<(String, String)> = r
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != "host")
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

fn stable(text: &str) -> String {
    let id = regex::Regex::new(r"subagent_id: \d+").unwrap();
    let secs = regex::Regex::new(r"duration_secs: [0-9.]+(e-?\d+)?").unwrap();
    let log_secs = regex::Regex::new(r"Duration: [0-9.]+s").unwrap();
    let stamp = regex::Regex::new(r"\d{8}-\d{6}").unwrap();
    let turn = regex::Regex::new(r"turn-\d+-\d+").unwrap();
    let t = id.replace_all(text, "subagent_id: <id>");
    let t = secs.replace_all(&t, "duration_secs: <secs>");
    let t = log_secs.replace_all(&t, "Duration: <secs>s");
    let t = stamp.replace_all(&t, "<timestamp>");
    turn.replace_all(&t, "turn-<pid>-<n>").into_owned()
}

fn gate_check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(ONESHOT_GATE_DIR)
        .join(name);
    if std::env::var_os("ONESHOT_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        let cut = |s: &str| -> String { s.lines().nth(line).unwrap_or("<end>").chars().take(400).collect() };
        panic!(
            "{name} differs from the fixture at line {}:\n  fixture: {}\n  now:     {}",
            line + 1,
            cut(&expected),
            cut(actual)
        );
    }
}

/// Run the `subagent` tool once with `timeout` seconds against `reply`,
/// and pin what the provider received, what the parent was sent, the
/// result and the log the run wrote, as `name`'s fixtures.
async fn oneshot_gate(
    name: &str,
    timeout: u64,
    reply: impl Fn(usize) -> Response + Clone + Send + Sync + 'static,
) {
    let base = crate::test_env::BaseDirGuard::new();
    // A stored Anthropic login the worker's broker reads: a stand-in
    // credential, never refreshed.
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"gate-refresh","access":"gate-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let (url, seen) = stand_in(reply).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut ctx = crate::tools::test_helpers::create_tool_context();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.channels.tx_events = Some(tx);
    let result = SubagentTool
        .execute(
            json!({
                "name": "gate",
                "system_prompt": "You are the oneshot gate's worker.",
                "task": "Read /nonexistent/oneshot-gate.txt and report what it says.",
                "timeout": timeout,
            }),
            ctx,
        )
        .await
        .expect("a oneshot returns its outcome as text");

    gate_check(&format!("{name}_requests.txt"), &stable(&render_requests(&seen.lock().unwrap())));
    let mut events = String::new();
    while let Ok(event) = rx.try_recv() {
        events.push_str(&format!("{event:?}\n"));
    }
    gate_check(&format!("{name}_events.txt"), &stable(&events));
    gate_check(&format!("{name}_result.txt"), &stable(&result));
    let logs = base.path().join("logs").join("subagents");
    let mut entries: Vec<_> = std::fs::read_dir(&logs)
        .map(|dir| dir.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    entries.sort();
    let mut log = String::new();
    for path in entries {
        log.push_str(&format!("=== {}\n", path.file_name().unwrap().to_string_lossy()));
        log.push_str(&std::fs::read_to_string(&path).unwrap());
    }
    gate_check(&format!("{name}_log.txt"), &stable(&log));
}

/// A run that uses a tool and then answers.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn oneshot_gate_a_finished_run_is_unchanged() {
    oneshot_gate("finished", 30, |n| if n == 0 { tool_use_reply(n) } else { text_reply(n) }).await;
}

/// A run that uses a tool, starts its answer and runs out of time: the
/// timed-out partial result. Three seconds leave room for both requests
/// and the tool call on a loaded machine; the held reply never ends.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn oneshot_gate_a_timed_out_run_is_unchanged() {
    oneshot_gate("timed_out", 3, |n| if n == 0 { tool_use_reply(n) } else { held_reply(n) }).await;
}

/// A run whose first request the provider refuses: the error result.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn oneshot_gate_a_failed_run_is_unchanged() {
    oneshot_gate("failed", 30, |_| invalid_reply()).await;
}
