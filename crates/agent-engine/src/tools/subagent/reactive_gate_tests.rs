// The reactive-worker gate (slice 8g): included at the end of start.rs as
// `mod reactive_gate`. A worker started by `subagent_start`, and a worker
// resumed by `subagent_resume` from it, both without a launch plan (every
// worker while worker rollover is off; the resumed one has no owned
// history grant), send the same provider requests (the worker registry's
// tools included), give their parent the same events and answer the same
// start, resume and collect bodies as before slice 8g. Each case runs the
// real tools, their worker runtimes built as in production (no host: the
// fresh runtime), against a loopback provider stand-in.
//
// Fixtures in tests/fixtures/reactive_gate/ were written with
// REACTIVE_GATE_WRITE=1 at the slice-8g base, twice, with no difference
// between the two writes. Normalization: the stand-in's `host` header; the
// handle and subagent IDs (process-wide counters); elapsed and duration
// seconds; uuids; RFC 3339 timestamps; the test's base directory; the
// process ID in an archive directory's name; a turn error's correlation ID.
// Nothing else.
//
// Provenance on the dev port: the fixtures are re-pinned (e68127e6) to pin
// A, 4dbbdcb2, written with REACTIVE_GATE_WRITE=1 at a7e1f463 (the dev base
// 7fdd5a4d plus these gate files, no production change), twice, the second
// write identical to the first. The slice-8g-base recording is history.
//
// Re-pinned for subagent thinking (fdb275f4, train 26.3: a worker's effort
// defaults to subagent_thinking, xhigh, and a resume keeps the effort its
// worker recorded) by an edit of start_requests.txt and
// resume_requests.txt, not by a write: each request's
// `thinking.budget_tokens` is 32768 (xhigh; was 4096, the medium default),
// so its content-length and body size are one byte more. Nothing else
// changed: the result, events and collect fixtures are as written. This
// gate passing at the re-pin's tree (3434f892 plus the edited fixtures, no
// production change) shows each fixture is byte for byte what
// REACTIVE_GATE_WRITE=1 would write.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use crate::tools::{SubagentRegistry, Tool};

const REACTIVE_GATE_DIR: &str = "tests/fixtures/reactive_gate";

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
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/nonexistent/reactive-gate.txt\"}"}}),
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
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!("answer {n}")}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]);
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
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

fn stable(text: &str, base: &std::path::Path) -> String {
    let base = base.display().to_string();
    let text = text.replace(&base, "<base>");
    let handle = regex::Regex::new(r"sa_\d+").unwrap();
    let id = regex::Regex::new(r"subagent_id: \d+").unwrap();
    let secs = regex::Regex::new(r#"("elapsed_secs"|"duration_secs"):\s*[0-9.]+(e-?\d+)?"#).unwrap();
    let debug_secs = regex::Regex::new(r"duration_secs: [0-9.]+(e-?\d+)?").unwrap();
    let after = regex::Regex::new(r"after [0-9]+(\.[0-9]+)?s").unwrap();
    let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let ts = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap();
    let stamp = regex::Regex::new(r"\d{8}-\d{6}").unwrap();
    let turn = regex::Regex::new(r"turn-\d+-\d+").unwrap();
    let history = regex::Regex::new(r"subagent-history/\d+-").unwrap();
    let t = handle.replace_all(&text, "sa_<n>");
    let t = id.replace_all(&t, "subagent_id: <id>");
    let t = secs.replace_all(&t, "$1: <secs>");
    let t = debug_secs.replace_all(&t, "duration_secs: <secs>");
    let t = after.replace_all(&t, "after <secs>");
    let t = history.replace_all(&t, "subagent-history/<pid>-");
    let t = uuid.replace_all(&t, "<id>");
    let t = ts.replace_all(&t, "<ts>");
    let t = stamp.replace_all(&t, "<timestamp>");
    turn.replace_all(&t, "turn-<pid>-<n>").into_owned()
}

fn gate_check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(REACTIVE_GATE_DIR)
        .join(name);
    if std::env::var_os("REACTIVE_GATE_WRITE").is_some() {
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

/// One parent: its worker registry and its orchestration policy, shared by
/// every tool call it makes (as one session's are).
struct Parent {
    registry: Arc<Mutex<SubagentRegistry>>,
    policy: Arc<crate::orchestration::OrchestrationRuntime>,
}

impl Parent {
    fn new() -> Self {
        let policy = crate::tools::test_helpers::create_tool_context()
            .capabilities
            .orchestration
            .expect("the test context has an orchestration policy");
        Self { registry: Arc::new(Mutex::new(SubagentRegistry::new())), policy }
    }

    fn bare_context(&self) -> crate::tools::ToolContext {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.subagent_registry = Some(Arc::clone(&self.registry));
        ctx.capabilities.orchestration = Some(Arc::clone(&self.policy));
        ctx
    }
}

/// A tool context of `parent` whose parent events go to the returned
/// receiver.
fn context(
    parent: &Parent,
) -> (
    crate::tools::ToolContext,
    tokio::sync::mpsc::UnboundedReceiver<crate::StreamEvent>,
) {
    let mut ctx = parent.bare_context();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.channels.tx_events = Some(tx);
    (ctx, rx)
}

/// Wait until `handle_id`'s worker thread has ended.
async fn finished(registry: &Mutex<SubagentRegistry>, handle_id: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let done = registry
            .lock()
            .unwrap()
            .get(handle_id)
            .and_then(|h| h.thread_handle.as_ref().map(|t| t.is_finished()))
            .expect("the thread handle is attached before the tool returns");
        if done {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{handle_id} ends");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::StreamEvent>) -> String {
    let mut events = String::new();
    while let Ok(event) = rx.try_recv() {
        events.push_str(&format!("{event:?}\n"));
    }
    events
}

/// The collect body for `handle_id`, reconciled (the parent has integrated
/// the result), pretty-printed.
async fn collect(parent: &Parent, handle_id: &str) -> String {
    let ctx = parent.bare_context();
    let body = crate::tools::subagent::SubagentCollectTool
        .execute(json!({ "handle_id": handle_id, "reconciled": true }), ctx)
        .await
        .unwrap_or_else(|e| panic!("collect {handle_id}: {e}"));
    match serde_json::from_str::<Value>(&body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap(),
        Err(_) => body,
    }
}

fn handle_of(result: &str) -> String {
    let value: Value = serde_json::from_str(result).unwrap_or_else(|e| panic!("{result}: {e}"));
    value["handle_id"].as_str().expect("a handle").to_owned()
}

/// The gate: a started worker that uses a tool and answers, its reconciled
/// collect, then a resume of it that answers. Pins each one's provider
/// requests, the parent's events, the tool's own answer and the collect
/// body.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn reactive_gate_a_v1_start_and_resume_are_unchanged() {
    let base = crate::test_env::BaseDirGuard::new();
    // A stored Anthropic login the worker's broker reads: a stand-in
    // credential, never refreshed.
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"gate-refresh","access":"gate-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let (url, seen) = stand_in(|n| if n == 0 { tool_use_reply(n) } else { text_reply(n) }).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let at = |text: &str| stable(text, base.path());
    let parent = Parent::new();

    // subagent_start
    let (ctx, mut rx) = context(&parent);
    let started = SubagentStartTool
        .execute(
            json!({
                "name": "gate",
                "system_prompt": "You are the reactive gate's worker.",
                "task": "Read /nonexistent/reactive-gate.txt and report what it says.",
                "timeout": 30,
            }),
            ctx,
        )
        .await
        .expect("the worker starts");
    let first = handle_of(&started);
    finished(&parent.registry, &first).await;
    let start_requests = seen.lock().unwrap().len();
    gate_check("start_requests.txt", &at(&render_requests(&seen.lock().unwrap())));
    gate_check("start_result.txt", &at(&started));
    gate_check("start_events.txt", &at(&drain(&mut rx)));
    gate_check("start_collect.json", &at(&collect(&parent, &first).await));

    // subagent_resume of the finished worker: a v1 run, no grant.
    let (ctx, mut rx) = context(&parent);
    let resumed = crate::tools::subagent::SubagentResumeTool
        .execute(
            json!({
                "handle_id": first,
                "instructions": "Now say what you would read next.",
                "timeout": 30,
            }),
            ctx,
        )
        .await
        .expect("the worker resumes");
    let second = handle_of(&resumed);
    finished(&parent.registry, &second).await;
    gate_check(
        "resume_requests.txt",
        &at(&render_requests(&seen.lock().unwrap()[start_requests..])),
    );
    gate_check("resume_result.txt", &at(&resumed));
    gate_check("resume_events.txt", &at(&drain(&mut rx)));
    gate_check("resume_collect.json", &at(&collect(&parent, &second).await));
}
