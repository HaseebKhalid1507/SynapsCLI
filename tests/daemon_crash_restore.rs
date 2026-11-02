//! A daemon that dies without a clean stop must not end work: the next
//! fresh start restores every session its crash record names once it
//! listens (listed `restoring` until each is back), and resumes once each
//! turn the death cut, against REAL daemon processes (`kill -9`) and a
//! loopback provider.
//!
//! - `a_killed_daemon_comes_back_with_its_sessions_and_the_cut_turn_resumed_once`:
//!   a cut session (mid-turn), an idle one and a parked one; `kill -9`; the
//!   next start lists all three from its first answer (the cut and the idle
//!   one Live, the parked one Parked), restoring or back;
//!   only the cut one gets ONE "resume: the daemon restarted after a crash
//!   mid-turn ..." message and a new turn; an attach by id finds it. Then a
//!   third start from the FIRST daemon's record (copied back) restores the
//!   sessions again and resumes nothing new.
//! - `a_clean_stop_keeps_the_record_and_the_next_start_resumes_the_turn_it_cut`:
//!   a graceful stop writes the record from its checkpoints (marked as a
//!   stop, `turn_cut` per session); the next start restores all three and
//!   resumes the cut turn once with the stop's message; a stop that cut
//!   nothing resumes nothing.
//! - `a_record_whose_writer_still_runs_restores_nothing`.
//! - `a_record_two_starts_died_restoring_is_moved_aside_and_not_restored`.
//! - `fifty_sessions_are_restored_in_parallel_within_a_generous_bound`.
//! - `crash_restore_off_restores_nothing_and_keeps_no_record`.
//! - `a_turn_another_sender_already_reprompted_is_not_resumed_again`: the
//!   session was re-attached and re-prompted (the operator's sweep line) by a
//!   daemon with the restore off; a later start from the dead daemon's
//!   record must not resume it.
//! - `a_crash_restore_keeps_each_sessions_resolved_client_input`: a session
//!   created with no `client_input` under `exclusive` comes back Exclusive
//!   after the config turned `shared`, an explicitly Shared one Shared
//!   (790d SHOULD 1).
//!
//! Unix only (`kill -9`, flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::daemon::{crash_record, registry};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use phase2::*;
use serde_json::Value;
use serial_test::serial;

/// The task of the session whose turn the crash cuts.
const CUT_TASK: &str = "never finishes";
/// The task of the session that is idle at the crash.
const IDLE_TASK: &str = "hello";
/// The task of the session that is parked at the crash.
const PARKED_TASK: &str = "park me";
const CRASH_PREFIX: &str = "resume: the daemon restarted after a crash mid-turn (";
const STOP_PREFIX: &str = "resume: the daemon was stopped and started again mid-turn (";

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

impl DaemonProc {
    /// SIGKILL: no shutdown path runs.
    fn kill_9(&mut self) {
        self.child.kill().expect("kill -9 the daemon");
        self.child.wait().expect("reap the daemon");
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

async fn spawn_daemon(guard: &HomeGuard, url: &str, extra: &[(&str, &str)]) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .envs(extra.iter().copied())
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
        assert!(t0.elapsed() < Duration::from_secs(60), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

fn session_config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_path_buf()),
        env: None,
        model_override: Some("claude-sonnet-4-5".into()),
        ..Default::default()
    }
}

async fn attach(paths: &registry::DaemonPaths, attach: Attach) -> (SocketTransport, AttachSnapshot) {
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    SocketTransport::attach(conn, attach).await.expect("attached")
}

async fn create(paths: &registry::DaemonPaths, cwd: &Path) -> SocketTransport {
    attach(paths, Attach::Create { config: session_config(cwd), mode: AttachMode::Mirror }).await.0
}

/// Submit `text` and wait for the turn's end.
async fn one_turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("the turn hung")
            .expect("alive");
        match env.event {
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
}

/// Submit `text` and wait for its first streamed text (the turn is then in
/// flight and stays so: the provider never ends it).
async fn start_endless_turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("the turn never streamed")
            .expect("alive");
        if matches!(
            env.event,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        ) {
            break;
        }
    }
}

/// A message's text: a string content, or its text blocks joined.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// A message's first text: a string content, or its first text block.
fn first_text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().find_map(|b| b["text"].as_str()).unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

/// A request's task (its first message's first text) and its newest
/// message's whole text. The wire merges adjacent user messages: after a
/// cut with no reply, the task, its crash marker and the resume (or the
/// next prompt) arrive as ONE user message, so the task is its first block,
/// not the whole message.
fn first_and_last(body: &Value) -> Option<(String, String)> {
    let messages = body["messages"].as_array()?;
    Some((first_text_of(messages.first()?), text_of(messages.last()?)))
}

/// `CUT_TASK`'s first request streams one text delta and never ends (the
/// kill cuts it); every other request is a whole reply. Bodies recorded.
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

/// Requests of the session whose first message is `task`, as their newest
/// message's text.
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

fn crash_resumes(bodies: &Mutex<Vec<Value>>, task: &str) -> usize {
    requests_of(bodies, task).iter().filter(|last| last.contains(CRASH_PREFIX)).count()
}

async fn wait_for(what: &str, budget: Duration, mut f: impl FnMut() -> bool, why: impl Fn() -> String) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < budget, "timed out waiting for {what}: {}", why());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn record_of(paths: &registry::DaemonPaths) -> Option<crash_record::CrashRecord> {
    crash_record::read(&crash_record::path(paths)).ok().flatten()
}

/// Wait until the crash record satisfies `f`.
async fn wait_record(paths: &registry::DaemonPaths, what: &str, f: impl Fn(&crash_record::CrashRecord) -> bool) {
    let t0 = std::time::Instant::now();
    loop {
        if record_of(paths).is_some_and(|r| f(&r)) {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "the crash record never showed {what}: {:#?}",
            record_of(paths)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every daemon log line (`synaps.log*` under the base dir) containing `needle`.
fn log_lines(guard: &HomeGuard, needle: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(guard.base_dir()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("synaps.log") {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            lines.extend(text.lines().filter(|l| l.contains(needle)).map(str::to_string));
        }
    }
    lines
}

/// Wait until the log holds `n` lines with `needle`; returns them.
async fn wait_log(guard: &HomeGuard, needle: &str, n: usize) -> Vec<String> {
    let t0 = std::time::Instant::now();
    loop {
        let lines = log_lines(guard, needle);
        if lines.len() >= n {
            return lines;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the daemon log never had {n} lines with {needle:?}: {lines:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The `took N ms` of a summary line.
fn took_ms(line: &str) -> u64 {
    let rest = &line[line.find("took ").expect("took") + 5..];
    rest.split_whitespace().next().unwrap().parse().expect("a number")
}

fn listed(metas: &[SessionMeta], id: &SessionId) -> Option<SessionLifecycle> {
    metas.iter().find(|m| &m.id == id).map(|m| m.lifecycle)
}

/// Wait until the daemon lists `id` with `want`.
async fn wait_listed(paths: &registry::DaemonPaths, id: &SessionId, want: SessionLifecycle) {
    let t0 = std::time::Instant::now();
    loop {
        let metas = SocketTransport::sessions(&paths.sock).await.unwrap_or_default();
        if listed(&metas, id) == Some(want) {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "{id} never listed {want:?}: {metas:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_killed_daemon_comes_back_with_its_sessions_and_the_cut_turn_resumed_once() {
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    // Park grace 0: a session with no client parks at once (the cut and the
    // idle session keep theirs attached).
    let mut a = spawn_daemon(&guard, &url, &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")]).await;
    let cwd = guard.home.path().to_path_buf();

    let mut idle = create(&a.paths, &cwd).await;
    let idle_id = idle.session_id().clone();
    one_turn(&mut idle, IDLE_TASK).await;

    let mut parked = create(&a.paths, &cwd).await;
    let parked_id = parked.session_id().clone();
    one_turn(&mut parked, PARKED_TASK).await;
    parked.detach().await;

    let mut cut = create(&a.paths, &cwd).await;
    let cut_id = cut.session_id().clone();
    start_endless_turn(&mut cut, CUT_TASK).await;

    let (cut_s, idle_s, parked_s) = (cut_id.to_string(), idle_id.to_string(), parked_id.to_string());
    wait_record(&a.paths, "the cut turn in flight and the parked session parked", |r| {
        let by = |id: &str| r.sessions.iter().find(|s| s.session.id == id);
        r.sessions.len() == 3
            && by(&cut_s).is_some_and(|s| s.turn_in_flight)
            && by(&idle_s).is_some_and(|s| !s.turn_in_flight && s.session.lifecycle == SessionLifecycle::Live)
            && by(&parked_s).is_some_and(|s| s.session.lifecycle == SessionLifecycle::Parked)
    })
    .await;
    // The turn draft (written at the turn's start and on its text) is on disk.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let a_pid = a.child.id();
    a.kill_9();
    let a_record = std::fs::read(crash_record::path(&a.paths)).expect("a kill leaves the record");
    assert_eq!(record_of(&a.paths).unwrap().writer.pid, a_pid);
    drop(idle);
    drop(cut);
    drop(parked);
    assert_eq!(requests_of(&bodies, CUT_TASK), vec![CUT_TASK.to_string()]);

    // B: listed before any client attached, the cut turn resumed once.
    let mut b = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &cut_id), Some(SessionLifecycle::Live), "{metas:#?}");
    assert_eq!(listed(&metas, &idle_id), Some(SessionLifecycle::Live), "{metas:#?}");
    // Listed at once; it parks again right after its restore.
    assert!(listed(&metas, &parked_id).is_some(), "{metas:#?}");
    wait_listed(&b.paths, &parked_id, SessionLifecycle::Parked).await;
    // The dead daemon's cards are swept at the start (their owner pid is
    // gone); each session registers again as it comes back.
    let every_request = || {
        let all: Vec<(String, String)> = bodies.lock().unwrap().iter().filter_map(first_and_last).collect();
        format!("every request (first, newest): {all:#?}")
    };
    wait_for("the resumed turn's request", Duration::from_secs(30), || crash_resumes(&bodies, CUT_TASK) >= 1, every_request)
        .await;
    // Its reply ends the turn; nothing else follows.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let cut_requests = requests_of(&bodies, CUT_TASK);
    assert_eq!(crash_resumes(&bodies, CUT_TASK), 1, "exactly one resume: {cut_requests:#?}");
    assert_eq!(cut_requests.len(), 2, "the cut request, then the resumed one: {cut_requests:#?}");
    assert!(
        cut_requests[1].ends_with(
            "UTC); your last turn was cut, so check your last tool call before trusting it, then continue."
        ),
        "{cut_requests:#?}"
    );
    assert_eq!(requests_of(&bodies, IDLE_TASK).len(), 1, "the idle session started nothing");
    assert_eq!(requests_of(&bodies, PARKED_TASK).len(), 1, "the parked session started nothing");
    let summary = wait_log(&guard, "crash restore: restored", 1).await;
    assert!(
        summary[0].contains("crash restore: restored 3 sessions, resume queued 1, given up 0, skipped 0"),
        "{summary:#?}"
    );
    let cut_line = wait_log(&guard, &format!("crash restore: session={cut_id} restored, resumed="), 1).await;
    assert!(cut_line[0].contains("resumed=yes"), "{cut_line:#?}");
    let idle_line = wait_log(&guard, &format!("crash restore: session={idle_id} restored, resumed="), 1).await;
    assert!(idle_line[0].contains("resumed=no"), "{idle_line:#?}");

    // An attach by id finds the restored session (no create): the cut turn,
    // its crash marker, the resume, the reply.
    let (again, snap) =
        attach(&b.paths, Attach::Existing { session_id: cut_id.clone(), mode: AttachMode::Mirror }).await;
    assert_eq!(again.session_id(), &cut_id);
    let msgs = &snap.conversation.api_messages;
    assert_eq!(text_of(&msgs[0]), CUT_TASK, "{msgs:#?}");
    assert!(msgs.iter().any(|m| text_of(m) == agent_engine::engine::interrupt::InterruptReason::Crash.marker()), "{msgs:#?}");
    assert_eq!(msgs.iter().filter(|m| text_of(m).starts_with(CRASH_PREFIX)).count(), 1, "{msgs:#?}");
    assert_eq!(msgs.last().unwrap()["role"], "assistant", "{msgs:#?}");
    again.detach().await;
    wait_record(&b.paths, "B's own record", |r| r.writer.pid == b.child.id() && r.sessions.len() == 3).await;

    // C, from A's record put back: the sessions come back again, the cut
    // turn's draft is gone (B consumed it), so nothing is resumed again.
    b.kill_9();
    std::fs::write(crash_record::path(&b.paths), &a_record).unwrap();
    let c = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&c.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &cut_id), Some(SessionLifecycle::Live), "{metas:#?}");
    let summary = wait_log(&guard, "crash restore: restored", 2).await;
    assert!(
        summary[1].contains("crash restore: restored 3 sessions, resume queued 1, given up 0, skipped 0"),
        "the cut turn is asked again (A's record says it was in flight): {summary:#?}"
    );
    let cut_lines = wait_log(&guard, &format!("crash restore: session={cut_id} restored, resumed="), 2).await;
    assert!(cut_lines[1].contains("resumed=no"), "the actor skips it: {cut_lines:#?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(crash_resumes(&bodies, CUT_TASK), 1, "no second resume: {:#?}", requests_of(&bodies, CUT_TASK));
    assert_eq!(requests_of(&bodies, CUT_TASK).len(), 2);
    assert_eq!(requests_of(&bodies, IDLE_TASK).len(), 1);
    assert!(
        log_lines(&guard, "no_crash_draft").iter().any(|l| l.contains(cut_id.as_str())),
        "the skip names why: {:#?}",
        log_lines(&guard, "crash resume")
    );
    SocketTransport::shutdown(&c.paths.sock, true).await.unwrap();
}

/// Wait (bounded) until the daemon process has exited.
fn wait_exit(d: &mut DaemonProc) {
    let t0 = std::time::Instant::now();
    while d.child.try_wait().expect("try_wait").is_none() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the daemon never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A clean stop keeps the record, written from the stop's own checkpoints:
/// the next start restores every session (Live or Parked as recorded) and
/// resumes once the turn the stop cut, with the stop's message; a stop that
/// cut nothing leaves a record whose start resumes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_clean_stop_keeps_the_record_and_the_next_start_resumes_the_turn_it_cut() {
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")]).await;
    let cwd = guard.home.path().to_path_buf();

    let mut idle = create(&a.paths, &cwd).await;
    let idle_id = idle.session_id().clone();
    one_turn(&mut idle, IDLE_TASK).await;
    let mut parked = create(&a.paths, &cwd).await;
    let parked_id = parked.session_id().clone();
    one_turn(&mut parked, PARKED_TASK).await;
    parked.detach().await;
    wait_listed(&a.paths, &parked_id, SessionLifecycle::Parked).await;
    let mut cut = create(&a.paths, &cwd).await;
    let cut_id = cut.session_id().clone();
    start_endless_turn(&mut cut, CUT_TASK).await;

    // A graceful stop (not forced): the record stays, marked as a stop.
    let a_pid = a.child.id();
    SocketTransport::shutdown(&a.paths.sock, false).await.unwrap();
    wait_exit(&mut a);
    drop(idle);
    drop(cut);
    drop(parked);
    let rec = record_of(&a.paths).expect("a clean stop keeps the record");
    assert_eq!(rec.writer.pid, a_pid);
    assert!(rec.reload.as_ref().is_some_and(|h| h.stop), "{rec:#?}");
    let by = |id: &SessionId| rec.sessions.iter().find(|s| s.session.id == id.as_str()).cloned();
    assert_eq!(rec.sessions.len(), 3, "{rec:#?}");
    assert_eq!(by(&cut_id).unwrap().session.turn_cut, Some(true), "{rec:#?}");
    assert_eq!(by(&idle_id).unwrap().session.turn_cut, Some(false), "{rec:#?}");
    assert_eq!(by(&parked_id).unwrap().session.lifecycle, SessionLifecycle::Parked, "{rec:#?}");
    let stop_line = wait_log(&guard, "crash record: written for the stop", 1).await;
    assert!(stop_line[0].contains("turns_cut=1"), "{stop_line:#?}");
    assert_eq!(requests_of(&bodies, CUT_TASK).len(), 1);

    // B: every session back before any client attaches, the cut turn
    // resumed once with the stop's message.
    let mut b = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &cut_id), Some(SessionLifecycle::Live), "{metas:#?}");
    assert_eq!(listed(&metas, &idle_id), Some(SessionLifecycle::Live), "{metas:#?}");
    assert!(listed(&metas, &parked_id).is_some(), "{metas:#?}");
    wait_listed(&b.paths, &parked_id, SessionLifecycle::Parked).await;
    let stop_resumes = || {
        requests_of(&bodies, CUT_TASK).iter().filter(|last| last.contains(STOP_PREFIX)).count()
    };
    wait_for("the resumed turn's request", Duration::from_secs(30), || stop_resumes() >= 1, || {
        format!("{:#?}", requests_of(&bodies, CUT_TASK))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let cut_requests = requests_of(&bodies, CUT_TASK);
    assert_eq!(stop_resumes(), 1, "exactly one resume: {cut_requests:#?}");
    assert_eq!(cut_requests.len(), 2, "{cut_requests:#?}");
    assert_eq!(crash_resumes(&bodies, CUT_TASK), 0, "not the crash message: {cut_requests:#?}");
    assert!(
        cut_requests[1].ends_with(
            "UTC); your last turn was cut, so check your last tool call before trusting it, then continue."
        ),
        "{cut_requests:#?}"
    );
    assert_eq!(requests_of(&bodies, IDLE_TASK).len(), 1, "the idle session started nothing");
    assert_eq!(requests_of(&bodies, PARKED_TASK).len(), 1, "the parked session started nothing");
    let summary = wait_log(&guard, "crash restore: restored", 1).await;
    assert!(
        summary[0].contains("crash restore: restored 3 sessions, resume queued 1, given up 0, skipped 0"),
        "{summary:#?}"
    );
    assert!(summary[0].contains("how=\"stop\"") || summary[0].contains("how=stop"), "{summary:#?}");

    // B stops with nothing in flight: C restores the sessions, resumes none.
    SocketTransport::shutdown(&b.paths.sock, false).await.unwrap();
    wait_exit(&mut b);
    let rec = record_of(&b.paths).expect("the second stop keeps a record too");
    assert!(rec.sessions.iter().all(|s| s.session.turn_cut == Some(false)), "{rec:#?}");
    let c = spawn_daemon(&guard, &url, &[]).await;
    let summary = wait_log(&guard, "crash restore: restored", 2).await;
    assert!(
        summary[1].contains("crash restore: restored 3 sessions, resume queued 0, given up 0, skipped 0"),
        "{summary:#?}"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(requests_of(&bodies, CUT_TASK).len(), 2, "nothing resumed again");
    SocketTransport::shutdown(&c.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_record_whose_writer_still_runs_restores_nothing() {
    let guard = HomeGuard::new();
    let (url, _bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let mut s = create(&a.paths, guard.home.path()).await;
    let id = s.session_id().clone();
    one_turn(&mut s, IDLE_TASK).await;
    wait_record(&a.paths, "the session", |r| r.sessions.len() == 1).await;
    a.kill_9();
    drop(s);

    // The record now names THIS (running) process as its writer.
    let path = crash_record::path(&a.paths);
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let me = crash_record::Writer::current().expect("this process can be named");
    v["writer"] = serde_json::to_value(&me).unwrap();
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();

    let b = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &id), None, "nothing restored: {metas:#?}");
    let lines = wait_log(&guard, "the record's writer still runs", 1).await;
    assert!(lines[0].contains(&me.key()), "{lines:#?}");
    assert!(log_lines(&guard, "crash restore: restored").is_empty());
    let skipped = std::fs::read_dir(&b.paths.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.live.json.skipped-"))
        .count();
    assert_eq!(skipped, 1, "moved aside, not overwritten");
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_record_two_starts_died_restoring_is_moved_aside_and_not_restored() {
    let guard = HomeGuard::new();
    let (url, _bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let mut s = create(&a.paths, guard.home.path()).await;
    let id = s.session_id().clone();
    one_turn(&mut s, IDLE_TASK).await;
    wait_record(&a.paths, "the session", |r| r.sessions.len() == 1).await;
    a.kill_9();
    drop(s);

    // Two earlier starts began restoring it and died.
    let path = crash_record::path(&a.paths);
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["restore_attempts"] = Value::from(2);
    let body = serde_json::to_vec(&v).unwrap();
    std::fs::write(&path, &body).unwrap();

    let b = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &id), None, "nothing restored: {metas:#?}");
    let lines = wait_log(&guard, "earlier starts died restoring this record", 1).await;
    assert!(lines[0].contains("WARN"), "{lines:#?}");
    assert!(log_lines(&guard, "crash restore: restored").is_empty());
    let moved: Vec<_> = std::fs::read_dir(&b.paths.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.live.json.quarantined-"))
        .collect();
    assert_eq!(moved.len(), 1, "moved aside once");
    assert_eq!(std::fs::read(moved[0].path()).unwrap(), body, "kept as it was");
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn fifty_sessions_are_restored_in_parallel_within_a_generous_bound() {
    const N: usize = 50;
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let cwd = guard.home.path().to_path_buf();
    let mut ids = Vec::new();
    for batch in 0..N / 10 {
        let turns = (0..10).map(|i| {
            let paths = a.paths.clone();
            let cwd = cwd.clone();
            async move {
                let mut t = create(&paths, &cwd).await;
                one_turn(&mut t, &format!("session {}", batch * 10 + i)).await;
                let id = t.session_id().clone();
                t.detach().await;
                id
            }
        });
        ids.extend(futures::future::join_all(turns).await);
    }
    // A turn's start reaches the record at once, its end on the debounce:
    // wait until no session is still "in flight" there, or the restore
    // rightly asks those few to resume (the actor then skips them).
    wait_record(&a.paths, "every session, none in flight", |r| {
        r.sessions.len() == N && r.sessions.iter().all(|s| !s.turn_in_flight)
    })
    .await;
    a.kill_9();

    // The shape a kill inside the debounce leaves: three ended turns still
    // marked in flight. The restore asks them to resume; each actor finds
    // no turn draft and skips it, so nothing is re-prompted.
    const STALE: usize = 3;
    let path = crash_record::path(&a.paths);
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for s in v["sessions"].as_array_mut().unwrap().iter_mut().take(STALE) {
        s["turn_in_flight"] = Value::Bool(true);
    }
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();

    // Ready and answering first (an autospawn kills a child that is not
    // ready within 15 s), the restore after: every session is listed from
    // the first answer on, restoring or back.
    let t0 = std::time::Instant::now();
    let b = spawn_daemon(&guard, &url, &[]).await;
    let answered = t0.elapsed();
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    for id in &ids {
        assert_eq!(listed(&metas, id), Some(SessionLifecycle::Live), "{id}: {metas:#?}");
    }
    eprintln!("the daemon answered {} ms after its spawn", answered.as_millis());
    assert!(answered < Duration::from_secs(15), "answered after {answered:?}");
    let summary = wait_log(&guard, "crash restore: restored", 1).await;
    eprintln!("summary: {}", summary[0]);
    assert!(
        summary[0].contains(&format!(
            "crash restore: restored {N} sessions, resume queued {STALE}, given up 0, skipped 0"
        )),
        "{summary:#?}"
    );
    let took = took_ms(&summary[0]);
    eprintln!("crash restore of {N} sessions took {took} ms");
    assert!(took < 20_000, "restoring {N} sessions took {took} ms: {summary:#?}");
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert!(metas.iter().all(|m| !m.restoring), "all back after the summary: {metas:#?}");
    // Every turn had ended: each session's line says resumed=no, the three
    // marked in flight included, and no resume reached the provider.
    let lines = wait_log(&guard, "restored, resumed=", N).await;
    assert_eq!(lines.len(), N, "one line per session: {lines:#?}");
    for id in &ids {
        let line = format!("crash restore: session={id} restored, resumed=no");
        assert_eq!(lines.iter().filter(|l| l.contains(&line)).count(), 1, "{id}: {lines:#?}");
    }
    eprintln!("per-session lines: {N} of {N} resumed=no ({STALE} marked in flight)");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let resumes = bodies
        .lock()
        .unwrap()
        .iter()
        .filter_map(first_and_last)
        .filter(|(_, last)| last.contains(CRASH_PREFIX))
        .count();
    assert_eq!(resumes, 0, "no crash resume was sent");
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn crash_restore_off_restores_nothing_and_keeps_no_record() {
    let guard = HomeGuard::new();
    let (url, _bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let mut s = create(&a.paths, guard.home.path()).await;
    let id = s.session_id().clone();
    one_turn(&mut s, IDLE_TASK).await;
    wait_record(&a.paths, "the session", |r| r.sessions.len() == 1).await;
    a.kill_9();
    drop(s);
    let path = crash_record::path(&a.paths);
    assert!(path.exists());

    std::fs::write(guard.base_dir().join("config"), "daemon.crash_restore = off\n").unwrap();
    let b = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &id), None, "today's behaviour: nothing restored: {metas:#?}");
    assert!(!path.exists(), "the leftover record is removed");
    wait_log(&guard, "removed stale session registrations", 1).await;
    assert!(log_lines(&guard, "crash restore: restored").is_empty());

    // Off: a session and its turn leave no record.
    let mut s = create(&b.paths, guard.home.path()).await;
    one_turn(&mut s, IDLE_TASK).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!path.exists(), "off keeps no record");
    s.detach().await;
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_another_sender_already_reprompted_is_not_resumed_again() {
    const SWEEP_LINE: &str = "resume: the operator's sweep after the crash; continue.";
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let mut cut = create(&a.paths, guard.home.path()).await;
    let cut_id = cut.session_id().clone();
    start_endless_turn(&mut cut, CUT_TASK).await;
    let cut_s = cut_id.to_string();
    wait_record(&a.paths, "the cut turn in flight", |r| {
        r.sessions.iter().any(|s| s.session.id == cut_s && s.turn_in_flight)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.kill_9();
    drop(cut);
    let a_record = std::fs::read(crash_record::path(&a.paths)).unwrap();

    // B, restore off: the app brings the session back from its journal and
    // the sweep re-prompts it.
    std::fs::write(guard.base_dir().join("config"), "daemon.crash_restore = off\n").unwrap();
    let mut b = spawn_daemon(&guard, &url, &[]).await;
    let config = SessionConfig {
        continue_session: Some(Some(cut_id.to_string())),
        ..session_config(guard.home.path())
    };
    let (mut back, _) = attach(&b.paths, Attach::Create { config, mode: AttachMode::Mirror }).await;
    assert_eq!(back.session_id(), &cut_id);
    one_turn(&mut back, SWEEP_LINE).await;
    back.detach().await;
    assert_eq!(requests_of(&bodies, CUT_TASK).len(), 2, "the cut request and the sweep's");
    b.kill_9();

    // C, restore on, from A's record: the sweep already answered the cut.
    std::fs::write(guard.base_dir().join("config"), "").unwrap();
    std::fs::write(crash_record::path(&b.paths), &a_record).unwrap();
    let c = spawn_daemon(&guard, &url, &[]).await;
    let metas = SocketTransport::sessions(&c.paths.sock).await.unwrap();
    assert_eq!(listed(&metas, &cut_id), Some(SessionLifecycle::Live), "restored: {metas:#?}");
    let summary = wait_log(&guard, "crash restore: restored", 1).await;
    assert!(summary[0].contains("resume queued 1"), "asked (A's record says in flight): {summary:#?}");
    let cut_line = wait_log(&guard, &format!("crash restore: session={cut_id} restored, resumed="), 1).await;
    assert!(cut_line[0].contains("resumed=no"), "the actor skips it: {cut_line:#?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(crash_resumes(&bodies, CUT_TASK), 0, "{:#?}", requests_of(&bodies, CUT_TASK));
    assert_eq!(requests_of(&bodies, CUT_TASK).len(), 2, "nothing after the sweep's turn");
    SocketTransport::shutdown(&c.paths.sock, true).await.unwrap();
}

/// Submit `text`: `Err(reason)` when refused, `Ok` once the turn it started
/// is idle.
async fn try_turn(t: &mut SocketTransport, text: &str) -> Result<(), String> {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] }).await.unwrap();
    let mut started = false;
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("no answer to the submit")
            .expect("alive");
        match env.event {
            SessionEventWire::TurnStarted { .. } => started = true,
            SessionEventWire::Idle if started => return Ok(()),
            SessionEventWire::Refused { reason, .. } => return Err(reason),
            _ => {}
        }
    }
}

/// `id`'s `client_input` as the listing shows it.
fn listed_input(metas: &[SessionMeta], id: &SessionId) -> Option<ClientInput> {
    metas.iter().find(|m| &m.id == id).and_then(|m| m.client_input)
}

/// 790d SHOULD 1 (R6) through the crash record: a session created with no
/// `client_input` (as the Pria app creates them) while the default is
/// `exclusive` comes back Exclusive after a `kill -9`, though the config
/// the next start reads says `client_input = shared`; one created
/// explicitly Shared comes back Shared. The listing and the Attached
/// snapshot say so, a second client's input on the Exclusive one is
/// refused in the form the app reads as `not_input_owner`, and the dead
/// daemon's record carried each resolved mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_crash_restore_keeps_each_sessions_resolved_client_input() {
    let guard = HomeGuard::new();
    let config = guard.base_dir().join("config");
    std::fs::write(&config, "client_input = exclusive\n").unwrap();
    let (url, _bodies) = spawn_provider().await;
    let mut a = spawn_daemon(&guard, &url, &[]).await;
    let cwd = guard.home.path().to_path_buf();

    let (mut implicit, snap) =
        attach(&a.paths, Attach::Create { config: session_config(&cwd), mode: AttachMode::Mirror }).await;
    assert_eq!(snap.client_input, ClientInput::Exclusive, "resolved at create");
    let implicit_id = implicit.session_id().clone();
    one_turn(&mut implicit, IDLE_TASK).await;
    let shared_config = SessionConfig { client_input: Some(ClientInput::Shared), ..session_config(&cwd) };
    let (mut shared, snap) = attach(&a.paths, Attach::Create { config: shared_config, mode: AttachMode::Mirror }).await;
    assert_eq!(snap.client_input, ClientInput::Shared);
    let shared_id = shared.session_id().clone();
    one_turn(&mut shared, PARKED_TASK).await;
    let (implicit_s, shared_s) = (implicit_id.to_string(), shared_id.to_string());
    wait_record(&a.paths, "both sessions, no turn in flight", |r| {
        r.sessions.len() == 2
            && r.sessions.iter().any(|s| s.session.id == implicit_s)
            && r.sessions.iter().any(|s| s.session.id == shared_s)
            && r.sessions.iter().all(|s| !s.turn_in_flight)
    })
    .await;
    std::fs::write(&config, "client_input = shared\n").unwrap();
    a.kill_9();
    let a_record = record_of(&a.paths).expect("a kill leaves the record");
    drop(implicit);
    drop(shared);

    let b = spawn_daemon(&guard, &url, &[]).await;
    let summary = wait_log(&guard, "crash restore: restored", 1).await;
    assert!(
        summary[0].contains("crash restore: restored 2 sessions, resume queued 0, given up 0, skipped 0"),
        "{summary:#?}"
    );
    // The next start does read the new default: a new session is Shared.
    let (fresh, snap) =
        attach(&b.paths, Attach::Create { config: session_config(&cwd), mode: AttachMode::Mirror }).await;
    assert_eq!(snap.client_input, ClientInput::Shared, "the next start's default");
    fresh.detach().await;

    let metas = SocketTransport::sessions(&b.paths.sock).await.unwrap();
    assert_eq!(listed_input(&metas, &implicit_id), Some(ClientInput::Exclusive), "the listing: {metas:#?}");
    assert_eq!(listed_input(&metas, &shared_id), Some(ClientInput::Shared), "the listing: {metas:#?}");
    let existing = |sid: &SessionId| Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror };
    let (first, snap) = attach(&b.paths, existing(&implicit_id)).await;
    assert_eq!(snap.client_input, ClientInput::Exclusive, "the Attached snapshot after the restore");
    assert_eq!(snap.input_owner, Some(first.client_id()), "the first drive client owns input");
    let (mut second, _) = attach(&b.paths, existing(&implicit_id)).await;
    let reason = try_turn(&mut second, "from the second client").await.expect_err("a non-owner's input is refused");
    assert!(reason.starts_with(&format!("input owned by client #{}", first.client_id().0)), "{reason}");
    second.detach().await;
    first.detach().await;
    let (other, snap) = attach(&b.paths, existing(&shared_id)).await;
    assert_eq!(snap.client_input, ClientInput::Shared, "the explicitly Shared one after the restore");
    other.detach().await;

    let recorded =
        |id: &str| a_record.sessions.iter().find(|s| s.session.id == id).and_then(|s| s.session.config.client_input);
    assert_eq!(recorded(&implicit_s), Some(ClientInput::Exclusive), "the record carries the resolved mode");
    assert_eq!(recorded(&shared_s), Some(ClientInput::Shared));
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}
