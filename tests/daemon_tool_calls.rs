//! `ClientFrame::ToolCalls`: a daemon-socket client subscribes to the
//! tool-call records of every session of a REAL daemon process (scripted
//! loopback provider, temp HOME), as the docs page `docs/tool-call-events.md`
//! describes.
//!
//! 1. With `daemon.tool_call_events` off (the default) a subscribe is
//!    refused with an `Error` that names the key, and the connection stays
//!    usable; so does one that sent a frame type this daemon does not know.
//! 2. With it on, a subscriber gets `ToolCallsSubscribed`, then one
//!    `ToolCall` per phase (called → decided → finished) for a session's
//!    call, under the provider's call id and the session's id, and a
//!    worker's call under its PARENT's session id plus its `handle_id`. No
//!    frame carries the call's input or output. A connection that did not
//!    subscribe never gets a `ToolCall`.
//! 3. A reload ends the subscription with `Bye{Reloading}`; subscribing
//!    again on the new image works.
//!
//! Unix only (UDS + execv). Uses the `read` tool, which both feature sets
//! build.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use axum::body::Bytes;
use axum::response::IntoResponse;
use phase2::*;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const WORKER_MARK: &str = "TOOL-CALLS-E2E-WORKER";
/// In the file the tools read: no frame may carry it.
const SECRET: &str = "sk-ant-api03-TOOLCALLSE2ESECRET0123456789abcdef";
/// The file's name: input, so no frame may carry it either.
const FILE_NAME: &str = "tce-e2e-input.txt";

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// A daemon child whose own cwd is `cwd` (a worker's base directory).
async fn spawn_daemon(guard: &HomeGuard, url: &str, cwd: &Path) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .current_dir(cwd)
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .env_remove("SYNAPS_DAEMON_SUBAGENT_ID_FLOOR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    wait_answering(&d.paths.sock).await;
    d
}

async fn wait_answering(sock: &Path) {
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn stop(d: &DaemonProc) {
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `daemon.tool_call_events = on` in the daemon's config.
fn turn_events_on(guard: &HomeGuard) {
    std::fs::write(guard.base_dir().join("config"), "daemon.tool_call_events = on\n").unwrap();
}

// ── scripted provider ───────────────────────────────────────────────────────

fn sse_frames(blocks: &str, stop: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_tc\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "{blocks}",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        blocks = blocks,
        stop = stop,
    )
}

fn sse_text(text: &str) -> String {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}});
    let stop = json!({"type":"content_block_stop","index":0});
    sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "end_turn")
}

fn sse_tool(id: &str, name: &str, input: Value) -> String {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}});
    let stop = json!({"type":"content_block_stop","index":0});
    sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "tool_use")
}

fn last_tool_result_id(body: &Value) -> Option<String> {
    let last = body["messages"].as_array()?.last()?;
    last["content"].as_array()?.iter().find_map(|b| {
        (b["type"] == "tool_result").then(|| b["tool_use_id"].as_str().unwrap_or("").to_string())
    })
}

/// Parent: "read the file" → one `read` (`toolu_read`); "start the worker"
/// → `subagent_start`. The worker reads the same file (`toolu_wread`).
fn respond(body: &Value, file: &Path) -> String {
    let system = body["system"].to_string();
    let last = body["messages"].as_array().and_then(|m| m.last()).map(|m| m.to_string()).unwrap_or_default();
    let read = |id: &str| sse_tool(id, "read", json!({"path": file.to_string_lossy()}));
    if system.contains(WORKER_MARK) {
        return match last_tool_result_id(body) {
            Some(_) => sse_text("worker done"),
            None => read("toolu_wread"),
        };
    }
    match last_tool_result_id(body) {
        Some(_) => sse_text("done"),
        None if last.contains("read the file") => read("toolu_read"),
        None if last.contains("start the worker") => sse_tool(
            "toolu_start",
            "subagent_start",
            json!({"system_prompt": WORKER_MARK, "task": "read the file", "name": "reader"}),
        ),
        None => sse_text("ack"),
    }
}

async fn spawn_scripted_provider(file: PathBuf) -> String {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = Arc::clone(&bodies);
        let file = file.clone();
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = respond(&v, &file);
            seen.lock().unwrap().push(v);
            (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], reply).into_response()
        }
    });
    serve(app).await
}

// ── a raw control connection ────────────────────────────────────────────────

/// One daemon-socket connection past `Hello`/`Welcome`, read and written
/// frame by frame. Keeps every raw line it read.
struct Raw {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
    lines: Vec<String>,
}

impl Raw {
    async fn connect(sock: &Path) -> Raw {
        let (r, w) = UnixStream::connect(sock).await.expect("connect").into_split();
        let mut raw = Raw { r: BufReader::new(r), w, lines: Vec::new() };
        raw.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
        let welcome = raw.next(Duration::from_secs(10)).await.expect("welcome");
        assert!(matches!(welcome, DaemonFrame::Welcome(_)), "{welcome:?}");
        raw
    }

    async fn send(&mut self, frame: &ClientFrame) {
        self.send_line(&encode_line(frame).unwrap()).await;
    }

    async fn send_line(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.flush().await.unwrap();
    }

    /// The next frame within `budget`; `None` on timeout or EOF.
    async fn next(&mut self, budget: Duration) -> Option<DaemonFrame> {
        let mut line = String::new();
        match tokio::time::timeout(budget, self.r.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => {
                let frame = decode_line::<DaemonFrame>(line.trim_end()).expect("a daemon frame");
                self.lines.push(line);
                Some(frame)
            }
            _ => None,
        }
    }

    async fn ping(&mut self) {
        self.send(&ClientFrame::Ping).await;
        let pong = self.next(Duration::from_secs(10)).await;
        assert!(matches!(pong, Some(DaemonFrame::Pong { .. })), "the connection stays usable: {pong:?}");
    }

    async fn subscribe(&mut self) {
        self.send(&ClientFrame::ToolCalls {}).await;
        let reply = self.next(Duration::from_secs(10)).await;
        assert!(matches!(reply, Some(DaemonFrame::ToolCallsSubscribed {})), "{reply:?}");
    }
}

/// `(seq, dropped, record)` of the next `ToolCall`s on `raw` until `done`
/// holds for the records so far.
async fn records_until(
    raw: &mut Raw,
    budget: Duration,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<(u64, u64, Value)> {
    let t0 = std::time::Instant::now();
    let mut got = Vec::new();
    while !done(&got.iter().map(|(_, _, r): &(u64, u64, Value)| r.clone()).collect::<Vec<_>>()) {
        let left = budget.checked_sub(t0.elapsed()).unwrap_or_else(|| panic!("records timed out: {got:?}"));
        match raw.next(left).await {
            Some(DaemonFrame::ToolCall { seq, dropped, record }) => {
                got.push((seq, dropped, serde_json::to_value(&record).unwrap()));
            }
            Some(other) => panic!("a subscriber gets only ToolCall frames: {other:?}"),
            None => panic!("records timed out or the connection ended: {got:?}"),
        }
    }
    got
}

fn phases_of(records: &[Value], tool_id: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["tool_id"] == tool_id)
        .map(|r| r["phase"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn finished(records: &[Value], tool_id: &str) -> bool {
    phases_of(records, tool_id).iter().any(|p| p == "finished")
}

async fn create(sock: &Path, cwd: &Path) -> (SocketTransport, SessionId) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();
    (t, snap.meta.id)
}

async fn turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] }).await.unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event()).await.expect("turn hung").expect("alive");
        if matches!(env.event, SessionEventWire::Idle) {
            return;
        }
    }
}

/// The input file, with a secret the tools read.
fn input_file(guard: &HomeGuard) -> PathBuf {
    let file = guard.home.path().join(FILE_NAME);
    std::fs::write(&file, format!("token={SECRET}\n")).unwrap();
    file
}

fn daemon_cwd(guard: &HomeGuard) -> PathBuf {
    let dir = guard.home.path().join("daemon-cwd");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── the tests ───────────────────────────────────────────────────────────────

/// Group 1 + group 9: the key is off by default; a subscribe is refused by
/// name and the connection stays usable, as it does after a frame type this
/// daemon cannot decode (what an older daemon does with `tool_calls`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn with_the_key_off_a_subscribe_is_refused_and_the_connection_stays_usable() {
    let guard = HomeGuard::new();
    let file = input_file(&guard);
    let url = spawn_scripted_provider(file).await;
    let d = spawn_daemon(&guard, &url, &daemon_cwd(&guard)).await;

    let mut raw = Raw::connect(&d.paths.sock).await;
    raw.send(&ClientFrame::ToolCalls {}).await;
    match raw.next(Duration::from_secs(10)).await {
        Some(DaemonFrame::Error { session_id: None, message }) => {
            assert!(message.contains("daemon.tool_call_events"), "the refusal names the key: {message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    raw.ping().await;

    raw.send_line("{\"type\":\"tool_calls_from_the_future\"}\n").await;
    match raw.next(Duration::from_secs(10)).await {
        Some(DaemonFrame::Error { message, .. }) => {
            assert!(message.contains("unknown variant `tool_calls_from_the_future`"), "{message}");
        }
        other => panic!("expected a decode error, got {other:?}"),
    }
    raw.ping().await;
    stop(&d).await;
}

/// Groups 2, 5, 7 and 9 end to end: a session's call and a worker's call
/// reach a subscriber; a connection that did not subscribe gets none; no
/// frame carries the input or the output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_subscriber_gets_every_sessions_calls_and_a_workers_carry_its_parent_and_handle() {
    let guard = HomeGuard::new();
    turn_events_on(&guard);
    let file = input_file(&guard);
    let url = spawn_scripted_provider(file).await;
    let process_cwd = daemon_cwd(&guard);
    let d = spawn_daemon(&guard, &url, &process_cwd).await;

    let mut sub = Raw::connect(&d.paths.sock).await;
    sub.subscribe().await;
    sub.ping().await;
    let mut bystander = Raw::connect(&d.paths.sock).await;
    bystander.ping().await;

    let session_cwd = guard.home.path().to_path_buf();
    let (mut t, sid) = create(&d.paths.sock, &session_cwd).await;
    turn(&mut t, "read the file").await;
    let own = records_until(&mut sub, Duration::from_secs(30), |r| finished(r, "toolu_read")).await;
    let records: Vec<Value> = own.iter().map(|(_, _, r)| r.clone()).collect();
    assert_eq!(phases_of(&records, "toolu_read"), ["called", "decided", "finished"], "{records:?}");
    assert!(own.windows(2).all(|w| w[1].0 > w[0].0), "seq increases: {own:?}");
    assert!(own.iter().all(|(_, dropped, _)| *dropped == 0));
    for r in records.iter().filter(|r| r["tool_id"] == "toolu_read") {
        assert_eq!(r["v"], 1);
        assert_eq!(r["session_id"], json!(sid.as_str()), "{r}");
        assert!(r.get("handle_id").is_none(), "a session's own call has no handle: {r}");
        assert_eq!((r["tool"].as_str(), r["tool_runtime"].as_str()), (Some("read"), Some("read")));
        match r["phase"].as_str() {
            Some("called") => {
                let cwd = r["cwd"].as_str().expect("called carries the base directory");
                let canonical = std::fs::canonicalize(&session_cwd).unwrap();
                assert!(Path::new(cwd) == session_cwd || Path::new(cwd) == canonical, "{r}");
            }
            Some("decided") => {
                assert_eq!((r["decision"].as_str(), r["by"].clone()), (Some("allow"), json!([])), "{r}");
                assert!(r.get("cwd").is_none());
            }
            Some("finished") => assert_eq!(r["status"], "ok", "{r}"),
            other => panic!("phase {other:?}"),
        }
    }

    turn(&mut t, "start the worker").await;
    let worker = records_until(&mut sub, Duration::from_secs(60), |r| finished(r, "toolu_wread")).await;
    let records: Vec<Value> = worker.iter().map(|(_, _, r)| r.clone()).collect();
    assert_eq!(phases_of(&records, "toolu_wread"), ["called", "decided", "finished"], "{records:?}");
    for r in records.iter().filter(|r| r["tool_id"] == "toolu_wread") {
        assert_eq!(r["session_id"], json!(sid.as_str()), "a worker's call carries its parent: {r}");
        assert_eq!(r["handle_id"], "sa_1", "{r}");
        if r["phase"] == "called" {
            // A worker runs in its parent session's cwd (dev 69fa04547), not the process cwd.
            let canonical = std::fs::canonicalize(&session_cwd).unwrap();
            let cwd = r["cwd"].as_str().expect("called carries the base directory");
            assert!(Path::new(cwd) == session_cwd || Path::new(cwd) == canonical, "{r}");
        }
    }
    // The parent's own subagent_start is a call of the session too.
    let start = phases_of(&records, "toolu_start");
    assert!(start.is_empty() || start.first().map(String::as_str) == Some("called"), "{start:?}");

    for line in &sub.lines {
        for needle in [SECRET, FILE_NAME, "token="] {
            assert!(!line.contains(needle), "{needle} on the wire: {line}");
        }
    }
    bystander.ping().await;
    assert!(
        bystander.lines.iter().all(|l| !l.contains("\"tool_call\"")),
        "a connection that did not subscribe got a ToolCall: {:?}",
        bystander.lines
    );

    t.detach().await;
    stop(&d).await;
}

/// Group 10: a reload ends a subscription with `Bye{Reloading}`; the
/// subscriber subscribes again on the new image and gets its records.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_ends_the_subscription_and_a_new_one_works_on_the_new_image() {
    let guard = HomeGuard::new();
    turn_events_on(&guard);
    let file = input_file(&guard);
    let url = spawn_scripted_provider(file).await;
    let d = spawn_daemon(&guard, &url, &daemon_cwd(&guard)).await;

    let mut sub = Raw::connect(&d.paths.sock).await;
    sub.subscribe().await;
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    match sub.next(Duration::from_secs(20)).await {
        Some(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation, .. }) }) => assert_eq!(generation, 2),
        other => panic!("expected Bye{{Reloading}}, got {other:?}"),
    }

    wait_answering(&d.paths.sock).await;
    let mut again = Raw::connect(&d.paths.sock).await;
    again.subscribe().await;
    let (mut t, sid) = create(&d.paths.sock, guard.home.path()).await;
    turn(&mut t, "read the file").await;
    let got = records_until(&mut again, Duration::from_secs(30), |r| finished(r, "toolu_read")).await;
    let records: Vec<Value> = got.iter().map(|(_, _, r)| r.clone()).collect();
    assert_eq!(phases_of(&records, "toolu_read"), ["called", "decided", "finished"], "{records:?}");
    assert!(records.iter().all(|r| r["session_id"] == json!(sid.as_str())));

    t.detach().await;
    stop(&d).await;
}
