//! Crash restore, the given-up and stop-during-restore paths end to end: a
//! restore whose creates go through a BARRIER factory in an in-process
//! daemon, between two real daemon processes and a loopback provider.
//!
//! A (a real daemon) has four sessions, each in its own cwd: three cut
//! mid-turn (`g`, `l`, `s`) and one idle (`h`); `kill -9`. B (in this
//! process; the restore's first create per cwd goes through the barrier)
//! restores A's record:
//! - `g`: its create fails: given up. An attach by id is refused with "could
//!   not be restored"; a client's `--continue <id>` then loads it, and it
//!   gets its one resume.
//! - `l`: its create finishes, then never returns (only the cap can end its
//!   lane): given up while it is live with no client (its WARN line says
//!   so), it gets its one resume at once, with no client asking.
//! - `h`: its create waits longer than the cap BEFORE it takes the create
//!   lock: still restored (the cap counts from the lock).
//! - `s`: its create never starts (held at the barrier): still restoring
//!   when B stops gracefully.
//!
//! B's stop record carries `s` unchanged (A's entry, A's crash its origin) and
//! records `g`, `l` and `h` from their actors, nothing cut. C (a real daemon)
//! restores all four from it: `s` gets its one resume and nothing else is
//! resumed again, so each cut turn is resumed exactly once across B and C.
//!
//! In-process because only `DaemonOpts::factory` can hold a create; one test
//! in this binary because the engine host is one per process. Unix only
//! (`kill -9`, flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::daemon::{crash_record, registry, Daemon, DaemonOpts, SessionFactory};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use phase2::*;
use serde_json::Value;
use serial_test::serial;

/// The task of the session whose restore create fails once.
const G_TASK: &str = "given up at its create";
/// The task of the session whose restore create stalls after it finished.
const L_TASK: &str = "capped after its create";
/// The task of the session whose restore create is held until the stop.
const S_TASK: &str = "held at the barrier";
/// The task of the idle session whose restore create waits before the lock.
const H_TASK: &str = "slow before the lock";
const CRASH_PREFIX: &str = "resume: the daemon restarted after a crash mid-turn (";

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

fn run_dir(guard: &HomeGuard) -> PathBuf {
    guard.base_dir().join("run")
}

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = run_dir(guard);
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
        // No profile: the socket is `daemon_paths_in(&run, None)`'s, the
        // sessions `<base>/sessions`.
        .env_remove("SYNAPS_PROFILE")
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
/// message's whole text (the wire merges adjacent user messages).
fn first_and_last(body: &Value) -> Option<(String, String)> {
    let messages = body["messages"].as_array()?;
    Some((first_text_of(messages.first()?), text_of(messages.last()?)))
}

/// A request whose newest message is one of the three cut tasks streams one
/// text delta and never ends (the kill cuts it); every other request is a
/// whole reply. Bodies recorded.
async fn spawn_provider() -> (String, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = Arc::clone(&seen);
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let endless = first_and_last(&v).is_some_and(|(_, last)| [G_TASK, L_TASK, S_TASK].contains(&last.as_str()));
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

/// The crash resumes the session whose first message is `task` was given:
/// the resume prompts in its newest request's whole history, each one
/// counted (two prompts the wire merged into one user message count as two).
fn crash_resumes(bodies: &Mutex<Vec<Value>>, task: &str) -> usize {
    let bodies = bodies.lock().unwrap();
    let newest = bodies
        .iter()
        .rev()
        .find(|b| b["messages"].as_array().and_then(|m| m.first()).is_some_and(|m| first_text_of(m) == task));
    newest.and_then(|b| b["messages"].as_array()).map_or(0, |messages| {
        messages.iter().map(|m| text_of(m).matches(CRASH_PREFIX).count()).sum()
    })
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

/// Wait until the log holds a line with `needle`; returns the lines.
async fn wait_log(guard: &HomeGuard, needle: &str) -> Vec<String> {
    let t0 = std::time::Instant::now();
    loop {
        let lines = log_lines(guard, needle);
        if !lines.is_empty() {
            return lines;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "the daemon log never had {needle:?}: {:#?}",
            log_lines(guard, "crash restore")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Writer that keeps the in-process daemon's formatted log lines.
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// B logs into this process, which has no log file: ONE process-global
/// subscriber (B's lanes run on the runtime's workers, which a thread-local
/// default would miss) keeps its WARN and ERROR lines.
fn capture_warnings() -> Arc<Mutex<Vec<u8>>> {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink = Arc::clone(&buf);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || CaptureWriter(Arc::clone(&sink)))
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber in this process");
    buf
}

/// The captured lines containing `needle`.
fn captured(buf: &Mutex<Vec<u8>>, needle: &str) -> Vec<String> {
    String::from_utf8_lossy(&buf.lock().unwrap())
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_string)
        .collect()
}

fn meta<'a>(metas: &'a [SessionMeta], id: &SessionId) -> Option<&'a SessionMeta> {
    metas.iter().find(|m| &m.id == id)
}

/// Which barrier lane a create belongs to: its cwd's last component.
fn lane(cfg: &SessionConfig) -> String {
    cfg.cwd
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

type Created = Pin<Box<dyn Future<Output = Result<SessionHandle, String>> + Send>>;

/// The host's own factory, except the FIRST create per lane (the restore's):
/// `g` fails, `l` never returns after its create finished, `h` waits
/// past the cap before it starts (before the create lock), `s` never starts.
/// Every later create (a client's) is the host's own.
fn barrier_factory(host: &Arc<EngineHost>) -> SessionFactory {
    let real = agent_engine::daemon::host_factory(host);
    let calls: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    Arc::new(move |cfg: SessionConfig| {
        let real = Arc::clone(&real);
        let lane = lane(&cfg);
        let first = {
            let mut calls = calls.lock().unwrap();
            let n = calls.entry(lane.clone()).or_insert(0);
            *n += 1;
            *n == 1
        };
        let created: Created = Box::pin(async move {
            match (lane.as_str(), first) {
                ("g", true) => Err("injected: the barrier failed this create".to_string()),
                // Its create finishes (and lets go of the create lock), then
                // the lane never returns: only the cap can end it, and the
                // restore's own resume (after the create returns) never runs.
                ("l", true) => {
                    let _live = real(cfg).await?;
                    std::future::pending().await
                }
                ("h", true) => {
                    tokio::time::sleep(crash_record::REHYDRATE_CAP + Duration::from_secs(1)).await;
                    real(cfg).await
                }
                ("s", true) => std::future::pending().await,
                _ => real(cfg).await,
            }
        });
        created
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn given_up_and_held_sessions_each_resume_their_cut_turn_once_across_a_stop() {
    let guard = HomeGuard::new();
    let (url, bodies) = spawn_provider().await;
    let home = guard.home.path().to_path_buf();
    let dir = |lane: &str| {
        let d = home.join(lane);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let (g_dir, l_dir, h_dir, s_dir) = (dir("g"), dir("l"), dir("h"), dir("s"));
    let every_request = || {
        let all: Vec<(String, String)> = bodies.lock().unwrap().iter().filter_map(first_and_last).collect();
        format!("every request (first, newest): {all:#?}")
    };

    // A: three turns cut by the kill, one session idle.
    let mut a = spawn_daemon(&guard, &url).await;
    let mut h = create(&a.paths, &h_dir).await;
    let h_id = h.session_id().clone();
    one_turn(&mut h, H_TASK).await;
    let mut cut = Vec::new();
    for (d, task) in [(&g_dir, G_TASK), (&l_dir, L_TASK), (&s_dir, S_TASK)] {
        let mut t = create(&a.paths, d).await;
        start_endless_turn(&mut t, task).await;
        cut.push(t);
    }
    let (g_id, l_id, s_id) = (cut[0].session_id().clone(), cut[1].session_id().clone(), cut[2].session_id().clone());
    let in_flight: Vec<String> = [&g_id, &l_id, &s_id].iter().map(|id| id.to_string()).collect();
    let h_s = h_id.to_string();
    wait_record(&a.paths, "three turns in flight and the idle session", |r| {
        let by = |id: &str| r.sessions.iter().find(|s| s.session.id == id);
        r.sessions.len() == 4
            && in_flight.iter().all(|id| by(id).is_some_and(|s| s.turn_in_flight))
            && by(&h_s).is_some_and(|s| !s.turn_in_flight)
    })
    .await;
    // The three turn drafts (written when each turn opened) are on disk.
    let sessions_dir = guard.base_dir().join("sessions");
    let journals: Vec<String> = {
        let r = record_of(&a.paths).expect("A's record");
        in_flight
            .iter()
            .map(|id| {
                let e = r.sessions.iter().find(|s| &s.session.id == id).expect("recorded");
                if e.session.journal_id.is_empty() {
                    e.session.id.clone()
                } else {
                    e.session.journal_id.clone()
                }
            })
            .collect()
    };
    let drafted = || -> Vec<bool> {
        journals
            .iter()
            .map(|j| matches!(agent_core::core::session_draft::read_turn_draft(&sessions_dir, j), Ok(Some(_))))
            .collect()
    };
    wait_for("the three turn drafts on disk", Duration::from_secs(15), || drafted().iter().all(|d| *d), || {
        format!("{journals:?}: {:?}", drafted())
    })
    .await;
    // Each session's history is saved where B looks for it (B skips a
    // session with no history on disk): it loads by its recorded journal id
    // and holds the session's own prompt. A turn's draft can be written
    // before the first save of its history, so the drafts alone are not
    // enough.
    let saved: Vec<(&str, String, &str)> = {
        let r = record_of(&a.paths).expect("A's record");
        let journal_of = |id: &str| {
            let e = r.sessions.iter().find(|s| s.session.id == id).expect("recorded");
            if e.session.journal_id.is_empty() {
                e.session.id.clone()
            } else {
                e.session.journal_id.clone()
            }
        };
        vec![
            ("g", journal_of(g_id.as_str()), G_TASK),
            ("l", journal_of(l_id.as_str()), L_TASK),
            ("s", journal_of(s_id.as_str()), S_TASK),
            ("h", journal_of(&h_s), H_TASK),
        ]
    };
    // Per session: its lane and journal id, which history files are there,
    // and whether the saved history loads and holds its prompt (the
    // fixture's own ids and prompts only).
    let readiness = || -> Vec<(String, bool)> {
        saved
            .iter()
            .map(|(lane, journal, task)| {
                let json = sessions_dir.join(format!("{journal}.json")).is_file();
                let log = agent_core::session_journal::journal_path(&sessions_dir, journal).is_file();
                let loaded = agent_core::core::session::Session::load_from_dir(&sessions_dir, journal);
                let ready = loaded
                    .as_ref()
                    .is_ok_and(|s| s.api_messages.iter().any(|m| m["role"] == "user" && text_of(m) == *task));
                let state = match &loaded {
                    Ok(s) => format!("loads, {} messages, its prompt saved: {ready}", s.api_messages.len()),
                    Err(e) => format!("does not load ({:?})", e.kind()),
                };
                (format!("{lane} {journal}: .json {json}, .journal {log}, {state}"), ready)
            })
            .collect()
    };
    wait_for(
        "each session's saved history with its prompt",
        Duration::from_secs(15),
        || readiness().iter().all(|(_, ready)| *ready),
        || format!("{:#?}", readiness().into_iter().map(|(line, _)| line).collect::<Vec<_>>()),
    )
    .await;
    a.kill_9();
    // A's record as B reads it: B's stop carries s's entry from it.
    let a_rec = record_of(&a.paths).expect("A's record");
    let a_s = a_rec.sessions.iter().find(|e| e.session.id == s_id.as_str()).expect("s in A's record").clone();
    drop(h);
    drop(cut);
    for task in [G_TASK, L_TASK, S_TASK] {
        assert_eq!(requests_of(&bodies, task), vec![task.to_string()]);
    }

    // B, in this process: the restore's creates go through the barrier.
    // Its log lines stay in this process: its WARN lines are kept here.
    let warnings = capture_warnings();
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    std::env::set_var("SYNAPS_NO_BOOT_FX", "1");
    let host = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    let b = Daemon::start(
        Arc::clone(&host),
        DaemonOpts {
            runtime_dir: Some(run_dir(&guard)),
            factory: Some(barrier_factory(&host)),
            ..Default::default()
        },
    )
    .await
    .expect("daemon B start");
    let b_paths = b.paths.clone();

    // g: its create failed, so it is given up: an attach by id is refused
    // with the reason, and the listing leaves it out.
    let conn = SocketTransport::connect(&b_paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    match SocketTransport::attach(conn, Attach::Existing { session_id: g_id.clone(), mode: AttachMode::Mirror }).await {
        Err(TransportError::Refused(m)) => assert!(
            m.contains(&format!("session {g_id} could not be restored: injected: the barrier failed this create")),
            "{m}"
        ),
        Err(e) => panic!("refused otherwise: {e:?}"),
        Ok(_) => panic!("a given-up session was attached by id"),
    }
    let metas = SocketTransport::sessions(&b_paths.sock).await.unwrap();
    assert!(meta(&metas, &g_id).is_none(), "a given-up session is not listed: {metas:#?}");
    assert!(
        meta(&metas, &s_id).is_some_and(|m| m.restoring),
        "s is held at the barrier: {metas:#?}\nB's crash restore warnings: {:#?}",
        captured(&warnings, "crash restore")
    );
    assert_eq!(crash_resumes(&bodies, G_TASK), 0);

    // A client's `--continue <id>` loads it; it gets its one resume.
    let (g, _) = attach(
        &b_paths,
        Attach::Create {
            config: SessionConfig { continue_session: Some(Some(g_id.to_string())), ..session_config(&g_dir) },
            mode: AttachMode::Mirror,
        },
    )
    .await;
    wait_for("g's resume once a client loaded it", Duration::from_secs(30), || crash_resumes(&bodies, G_TASK) >= 1, every_request)
        .await;

    // l: its create finished, then the cap ran out: given up while live with
    // no client, it gets its one resume with no client asking.
    wait_for("l's resume with no client", Duration::from_secs(60), || crash_resumes(&bodies, L_TASK) >= 1, every_request)
        .await;
    let gave_up_l = format!(
        "crash restore: session={l_id} given up: not restored within {} s",
        crash_record::REHYDRATE_CAP.as_secs()
    );
    assert_eq!(captured(&warnings, &gave_up_l).len(), 1, "the cap gave l up, once: {:#?}", captured(&warnings, "crash restore"));

    // h: its create waited past the cap before it took the create lock, and
    // is restored all the same (listed from its actor, not given up).
    let t0 = std::time::Instant::now();
    loop {
        let metas = SocketTransport::sessions(&b_paths.sock).await.unwrap_or_default();
        if meta(&metas, &h_id).is_some_and(|m| !m.restoring) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "h never came back: {metas:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The negative control for l's WARN line: h was never given up.
    let gave_up_h = format!("crash restore: session={h_id} given up");
    assert!(captured(&warnings, &gave_up_h).is_empty(), "h was given up: {:#?}", captured(&warnings, "crash restore"));
    let (again, _) = attach(&b_paths, Attach::Existing { session_id: h_id.clone(), mode: AttachMode::Mirror }).await;
    again.detach().await;

    // s: still held, so still restoring when B stops.
    let metas = SocketTransport::sessions(&b_paths.sock).await.unwrap();
    assert!(
        meta(&metas, &s_id).is_some_and(|m| m.restoring),
        "s is still held: {metas:#?}\nB's crash restore warnings: {:#?}",
        captured(&warnings, "crash restore")
    );
    // The resumed turns end with their replies; nothing else follows.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(crash_resumes(&bodies, G_TASK), 1, "{}", every_request());
    assert_eq!(crash_resumes(&bodies, L_TASK), 1, "{}", every_request());
    assert_eq!(requests_of(&bodies, S_TASK).len(), 1, "{}", every_request());
    assert_eq!(requests_of(&bodies, H_TASK).len(), 1, "{}", every_request());

    // B stops gracefully during its restore.
    g.detach().await;
    drop(g);
    SocketTransport::shutdown(&b_paths.sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), b.wait()).await.expect("B stopped within 60 s");
    let rec = record_of(&b_paths).expect("B's stop keeps a record");
    assert!(rec.reload.as_ref().is_some_and(|h| h.stop), "{rec:#?}");
    assert_eq!(rec.sessions.len(), 4, "{rec:#?}");
    let by = |id: &SessionId| {
        rec.sessions
            .iter()
            .find(|s| s.session.id == id.as_str() || s.session.journal_id == id.as_str())
            .unwrap_or_else(|| panic!("{id} is not in B's stop record: {rec:#?}"))
    };
    // s is carried unchanged: A's entry, with A's crash (its writer and its
    // last write) as the origin that still owes it its resume.
    let s = by(&s_id);
    assert!(s.turn_in_flight, "{rec:#?}");
    let a_crash = crash_record::Origin::Crash { writer: a_rec.writer.key(), crashed_at: a_rec.written_at };
    assert_eq!(s.origin, Some(a_crash), "{rec:#?}");
    let mut want = a_s.clone();
    want.origin = s.origin.clone();
    assert_eq!(
        serde_json::to_value(s).unwrap(),
        serde_json::to_value(&want).unwrap(),
        "s's entry in B's stop record is A's"
    );
    // g, l and h are recorded from their actors; the stop cut nothing.
    for id in [&g_id, &l_id, &h_id] {
        let e = by(id);
        assert!(e.origin.is_none(), "{id}: {rec:#?}");
        assert_eq!(e.session.turn_cut, Some(false), "{id}: {rec:#?}");
    }

    // C, from B's stop record: all four back; only s is resumed, once.
    let c = spawn_daemon(&guard, &url).await;
    let summary = wait_log(&guard, "crash restore: restored 4 sessions").await;
    assert!(
        summary.iter().any(|l| l.contains("crash restore: restored 4 sessions, resume queued 1, given up 0, skipped 0")),
        "{summary:#?}"
    );
    wait_for("s's resume", Duration::from_secs(30), || crash_resumes(&bodies, S_TASK) >= 1, every_request).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for task in [G_TASK, L_TASK, S_TASK] {
        assert_eq!(crash_resumes(&bodies, task), 1, "{task}: exactly one resume: {}", every_request());
        assert_eq!(requests_of(&bodies, task).len(), 2, "{task}: the cut request, then its resume: {}", every_request());
    }
    assert_eq!(requests_of(&bodies, H_TASK).len(), 1, "h started nothing: {}", every_request());
    SocketTransport::shutdown(&c.paths.sock, true).await.unwrap();
}
