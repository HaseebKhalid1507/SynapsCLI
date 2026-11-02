//! `synaps daemon reload` while a session's turn is blocked in `wait`,
//! against a REAL daemon process and a scripted loopback provider.
//!
//! The reload quiesces every live session before its drain, so:
//!
//! 1. the drain ends early: the reload answers well inside its drain
//!    budget, instead of waiting out the blocked tool;
//! 2. the wait returns `interrupted` (the daemon is reloading) at once,
//!    not at its timeout, and says nothing will wake the session for it;
//! 3. the turn starts no new provider round after that result in the old
//!    image; the new image resumes it (reload-resume): one request whose
//!    newest message is the resume text, with the result in its history.
//!
//! Unix only (execv + flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{SessionEvent, StreamEvent};
use axum::body::Bytes;
use axum::response::IntoResponse;
use phase2::*;
use serde_json::{json, Value};
use serial_test::serial;

/// The reload's drain budget: a reload that waited out the blocked wait
/// would take about this long.
const DRAIN_SECS: u64 = 30;

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

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(["daemon", "--foreground"])
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
    let t0 = Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

fn sse_frames(blocks: &str, stop: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_rw\",\"type\":\"message\",",
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

fn last_message(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m.to_string())
        .unwrap_or_default()
}

/// The text of the tool result answering `tool_use_id`, from any message of
/// a request's history.
fn tool_result_text(body: &Value, tool_use_id: &str) -> Option<String> {
    body["messages"].as_array()?.iter().find_map(|m| {
        m["content"].as_array()?.iter().find_map(|b| {
            (b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id).then(|| match &b["content"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect(),
                other => other.to_string(),
            })
        })
    })
}

/// "wait for the file": one `wait` on a file that never appears (60 s).
/// Any later request after a tool result: "after the wait" (the test
/// asserts none is made). Anything else: "ack".
fn respond(body: &Value, never: &str) -> String {
    let last = last_message(body);
    if last.contains("tool_result") {
        return sse_text("after the wait");
    }
    if last.contains("wait for the file") {
        return sse_tool("toolu_wait", "wait", json!({"any": [{"file": never}], "timeout_secs": 60}));
    }
    sse_text("ack")
}

async fn spawn_scripted_provider(never: String) -> (String, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = Arc::clone(&seen);
        let never = never.clone();
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = respond(&v, &never);
            seen.lock().unwrap().push(v);
            (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], reply).into_response()
        }
    });
    (serve(app).await, bodies)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_ends_a_blocked_wait_and_its_turn_without_waiting_out_the_drain() {
    let guard = HomeGuard::new();
    let never = guard.home.path().join("never-appears");
    let (url, bodies) = spawn_scripted_provider(never.display().to_string()).await;
    let d = spawn_daemon(&guard, &url).await;

    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();

    t.send_from_self(SessionCommand::Submit { text: "wait for the file".into(), attachments: vec![] })
        .await
        .unwrap();
    // The provider answered round 1 with the wait call; give the tool a
    // moment to start blocking.
    let t0 = Instant::now();
    while bodies.lock().unwrap().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the turn never reached the provider");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 1. The reload answers well inside its drain budget.
    let started = Instant::now();
    let gen = SocketTransport::reload(&d.paths.sock, false, Some(DRAIN_SECS), None)
        .await
        .expect("reload accepted");
    let took = started.elapsed();
    assert_eq!(gen, 2);
    assert!(
        took < Duration::from_secs(DRAIN_SECS / 3),
        "the drain waited out the blocked wait: {took:?} of a {DRAIN_SECS} s drain"
    );
    let mut notices = Vec::new();
    while let Some(env) = tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.expect("announce") {
        if let SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(text))) = &env.event {
            notices.push(text.clone());
        }
    }
    // The new image may already have sent the resumed turn's request (step
    // 3); apart from it, nothing followed the wait's result.
    let before_resume = bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|b| !last_message(b).contains("resume: the daemon reloaded mid-turn ("))
        .count();
    assert_eq!(before_resume, 1, "no provider round after the wait's result");
    tokio::time::timeout(Duration::from_secs(20), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");

    // 3. The new image resumes the turn the reload stopped (reload-resume):
    //    ONE request on its own, whose newest message carries the resume
    //    text, with the wait's result in its history; nothing after it.
    let t0 = Instant::now();
    while bodies.lock().unwrap().len() < 2 {
        assert!(t0.elapsed() < Duration::from_secs(30), "the resumed turn never reached the provider");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(bodies.lock().unwrap().len(), 2, "one resumed turn, nothing else");
    let second = bodies.lock().unwrap()[1].clone();
    assert!(
        last_message(&second).contains("resume: the daemon reloaded mid-turn ("),
        "{second}"
    );

    // 2. The wait ended as interrupted, at once, and said so.
    let result = tool_result_text(&second, "toolu_wait").expect("the wait's result is in the history");
    assert!(result.contains("interrupted"), "{result}");
    assert!(result.contains("the daemon is reloading"), "{result}");
    assert!(result.contains("Nothing will wake this session"), "{result}");
    let secs: f64 = result
        .split("interrupted after ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no elapsed time in {result}"));
    assert!(secs < 5.0, "interrupted after {secs} s, not at once: {result}");
    eprintln!("reload answered in {took:?}; notices before the exec: {notices:?}");

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
