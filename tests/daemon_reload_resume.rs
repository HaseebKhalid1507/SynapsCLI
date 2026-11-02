//! A daemon reload must not end work: a turn the reload cut is resumed in
//! the new image, against a REAL daemon process and a loopback provider.
//!
//! Session A has a turn in flight when the reload checkpoints it (its
//! journal ends with the partial reply and the `Restart` marker). Session B
//! finished its turn before the reload. After the `execv`:
//! - A gets ONE queued message ("resume: the daemon reloaded mid-turn ...")
//!   and a new turn starts: the provider sees exactly one request whose
//!   newest message is that text, with nobody attached;
//! - B gets nothing: no request after the reload;
//! - A's journal reads: the cut turn, the marker, the resume message, the
//!   reply.
//!
//! Unix only (execv + flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use phase2::*;
use serde_json::Value;
use serial_test::serial;

/// The task of the session whose turn the reload cuts.
const CUT_TASK: &str = "never finishes";
/// The task of the session that is idle at the reload.
const IDLE_TASK: &str = "hello";
const RESUME_PREFIX: &str = "resume: the daemon reloaded mid-turn (";

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

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

async fn attach_create(paths: &registry::DaemonPaths, cwd: &Path) -> SocketTransport {
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    SocketTransport::attach(
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
    .unwrap()
    .0
}

/// A message's text: a string content, or its text blocks joined.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// The session a request belongs to (its first message) and its newest
/// message's text.
fn first_and_last(body: &Value) -> Option<(String, String)> {
    let messages = body["messages"].as_array()?;
    Some((text_of(messages.first()?), text_of(messages.last()?)))
}

/// `CUT_TASK`'s first request streams one text delta and never ends (the
/// reload cuts it); every other request is a whole reply. Bodies recorded.
async fn spawn_provider() -> (String, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = Arc::clone(&seen);
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let endless = first_and_last(&v).is_some_and(|(_, last)| last == CUT_TASK);
            seen.lock().unwrap().push(v);
            if !endless {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    ANTHROPIC_SSE.to_string(),
                )
                    .into_response();
            }
            let stream = futures::stream::unfold(0u64, |i| async move {
                if i == 0 {
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(ANTHROPIC_SSE_PREFIX)),
                        1,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some((Ok(Bytes::from(": keep-alive\n\n")), i + 1))
            });
            Response::builder()
                .status(axum::http::StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    });
    (serve(app).await, bodies)
}

/// Requests of the session whose first message is `task`, as (newest text).
fn requests_of(bodies: &Mutex<Vec<Value>>, task: &str) -> Vec<String> {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter_map(first_and_last)
        .filter(|(first, _)| first == task)
        .map(|(_, last)| last)
        .collect()
}

async fn wait_for(what: &str, budget: Duration, mut f: impl FnMut() -> bool, why: impl Fn() -> String) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < budget, "timed out waiting for {what}: {}", why());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Until the event stream ends (the daemon's `Bye{Reloading}` + EOF).
async fn until_eof(t: &mut SocketTransport) {
    while tokio::time::timeout(Duration::from_secs(10), t.next_event())
        .await
        .expect("the reload ends this connection")
        .is_some()
    {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_the_reload_cut_is_resumed_once_and_an_idle_session_is_left_alone() {
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();

    // B: one finished turn, idle at the reload.
    let mut b = attach_create(&d.paths, guard.home.path()).await;
    b.send_from_self(SessionCommand::Submit { text: IDLE_TASK.into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), b.next_event())
            .await
            .expect("B's turn hung")
            .expect("alive");
        match env.event {
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }

    // A: a turn in flight at the reload, past its first text.
    let mut a = attach_create(&d.paths, guard.home.path()).await;
    let a_sid = a.session_id().clone();
    a.send_from_self(SessionCommand::Submit { text: CUT_TASK.into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), a.next_event())
            .await
            .expect("A's turn never streamed")
            .expect("alive");
        if matches!(
            env.event,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        ) {
            break;
        }
    }
    assert_eq!(requests_of(&bodies, IDLE_TASK).len(), 1);
    assert_eq!(requests_of(&bodies, CUT_TASK), vec![CUT_TASK.to_string()]);

    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    until_eof(&mut a).await;
    until_eof(&mut b).await;

    // The new image resumes A with nobody attached: a new turn whose newest
    // message is the resume text. The request merges the cut turn's marker
    // and the resume (two user messages in history) into its newest user
    // message, so the text is looked for inside it.
    let resumed = |bodies: &Mutex<Vec<Value>>| {
        requests_of(bodies, CUT_TASK)
            .into_iter()
            .filter(|last| last.contains(RESUME_PREFIX))
            .count()
    };
    let every_request = || {
        let all: Vec<(String, String)> = bodies.lock().unwrap().iter().filter_map(first_and_last).collect();
        format!("every request (first, newest): {all:#?}")
    };
    wait_for(
        "the resumed turn's request",
        Duration::from_secs(20),
        || resumed(&bodies) >= 1,
        every_request,
    )
    .await;
    // Its reply ends the turn; nothing else follows. Long enough for a
    // second resume or a turn of B's to have reached the provider.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let a_requests = requests_of(&bodies, CUT_TASK);
    assert_eq!(resumed(&bodies), 1, "exactly one resume: {a_requests:#?}");
    assert_eq!(a_requests.len(), 2, "the cut request, then the resumed one: {a_requests:#?}");
    let resume = &a_requests[1];
    assert!(
        resume.starts_with(agent_engine::engine::interrupt::InterruptReason::Restart.marker())
            && resume.ends_with(
                "UTC); your last turn was cut, so check your last tool call before trusting it, then continue."
            ),
        "{resume}"
    );
    assert_eq!(
        requests_of(&bodies, IDLE_TASK).len(),
        1,
        "the idle session started nothing after the reload"
    );

    // A's journal: the cut turn, the marker, the resume, the reply.
    let snap = tokio::time::timeout(Duration::from_secs(15), a.reconnect(AttachMode::Mirror))
        .await
        .unwrap()
        .expect("reconnected");
    assert_eq!(a.welcome.pid, pid_before);
    assert_eq!(a.session_id(), &a_sid);
    let msgs = &snap.conversation.api_messages;
    assert_eq!(msgs.len(), 5, "{msgs:#?}");
    assert_eq!(text_of(&msgs[0]), CUT_TASK);
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(
        text_of(&msgs[2]),
        agent_engine::engine::interrupt::InterruptReason::Restart.marker()
    );
    assert_eq!(msgs[3]["role"], "user");
    assert!(text_of(&msgs[3]).starts_with(RESUME_PREFIX), "{msgs:#?}");
    assert_eq!(msgs[4]["role"], "assistant");
    assert_eq!(text_of(&msgs[4]), "hi");

    a.detach().await;
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}
