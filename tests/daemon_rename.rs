//! `synaps daemon rename` (`ClientFrame::Rename`): name or unname one session
//! from a control connection, with host authority and without attaching,
//! through the actor's own `/saveas` path. A parked session is refused, never
//! woken. Real host (`host_factory`), real UDS, loopback Anthropic stub.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{Connected, RenameFailure, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn start(guard: &HomeGuard) -> Daemon {
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start")
}

/// Stub provider + daemon; `park_grace` is `SYNAPS_DAEMON_PARK_GRACE_SECS`.
async fn boot(guard: &HomeGuard, park_grace: &str) -> (Daemon, Arc<std::sync::atomic::AtomicUsize>) {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", park_grace);
    let (url, hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (start(guard).await, hits)
}

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open")
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await;
        let done = pred(&e.event);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

/// Create a persisted session over the socket; this client owns input.
async fn create(sock: &Path, cwd: &Path) -> (SocketTransport, SessionId) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                persist: true,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");
    let sid = t.session_id().clone();
    (t, sid)
}

async fn observe(sock: &Path, sid: &SessionId) -> SocketTransport {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) =
        SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Observe })
            .await
            .expect("attach observe");
    t
}

async fn run_turn(t: &mut SocketTransport) {
    t.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

async fn listed(sock: &Path, sid: &SessionId) -> Option<SessionMeta> {
    SocketTransport::sessions(sock).await.unwrap_or_default().into_iter().find(|m| &m.id == sid)
}

async fn wait_lifecycle(sock: &Path, sid: &SessionId, want: SessionLifecycle, budget: Duration) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        if listed(sock, sid).await.is_some_and(|m| m.lifecycle == want) {
            return true;
        }
        if t0.elapsed() > budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(d: Daemon, sock: &Path) {
    SocketTransport::shutdown(sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

/// The saved session file's `name` (`sessions/<journal id>.json`).
fn saved_name(guard: &HomeGuard, journal_id: &str) -> Option<String> {
    let path = guard.base_dir().join("sessions").join(format!("{journal_id}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["name"].as_str().map(str::to_string)
}

/// The `name` of every session card under `run/` (`run/<sid>.json`).
fn card_names(guard: &HomeGuard) -> Vec<Option<String>> {
    let Ok(dir) = std::fs::read_dir(guard.base_dir().join("run")) else { return Vec::new() };
    dir.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(e.path()).ok()?).ok())
        .filter(|v| v.get("session_id").is_some())
        .map(|v| v["name"].as_str().map(str::to_string))
        .collect()
}

fn failure(r: Result<Result<Option<String>, RenameFailure>, TransportError>) -> RenameFailure {
    match r {
        Ok(Err(f)) => f,
        other => panic!("expected a rename failure, got {other:?}"),
    }
}

/// The subconscious' case: an observer is attached (it does not own input
/// and must not have to), the session is renamed from a control
/// connection, and the observer sees the name at once. Presence is
/// unchanged: still one owner and one observer, no new client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_names_a_live_session_without_attaching_and_an_observer_sees_it() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut owner).await;
    let mut obs = observe(&sock, &sid).await;
    let before = d.state.attach(&sid).unwrap().presence();

    let got = SocketTransport::rename(&sock, &sid, Some("ambient-notes")).await.expect("reply");
    assert_eq!(got, Ok(Some("ambient-notes".into())));

    let seen = until(&mut obs, |e| {
        matches!(e, SessionEventWire::Conversation(c) if c.header.name.as_deref() == Some("ambient-notes"))
    })
    .await;
    assert!(!seen.is_empty());
    let m = listed(&sock, &sid).await.expect("listed");
    assert_eq!(m.name.as_deref(), Some("ambient-notes"), "the listing shows it");
    assert_eq!(saved_name(&guard, &m.journal_id).as_deref(), Some("ambient-notes"), "saved to the session file");
    let cards = card_names(&guard);
    assert!(cards.contains(&Some("ambient-notes".into())), "run/<sid>.json carries it: {cards:?}");
    let after = d.state.attach(&sid).unwrap().presence();
    assert_eq!((after.clients, after.input_owner), (before.clients, before.input_owner), "presence unchanged");

    // Renaming to the current name again is not an error.
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("ambient-notes")).await.unwrap(), Ok(Some("ambient-notes".into())));

    // Clear: `None` and blank both clear.
    assert_eq!(SocketTransport::rename(&sock, &sid, None).await.unwrap(), Ok(None));
    until(&mut obs, |e| matches!(e, SessionEventWire::Conversation(c) if c.header.name.is_none())).await;
    assert_eq!(listed(&sock, &sid).await.unwrap().name, None);
    assert_eq!(saved_name(&guard, &m.journal_id), None);
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("x")).await.unwrap(), Ok(Some("x".into())));
    assert_eq!(SocketTransport::rename(&sock, &sid, Some("   ")).await.unwrap(), Ok(None));

    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_refuses_an_invalid_a_taken_and_a_chain_name_with_the_precise_reason() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut a, sid_a) = create(&sock, guard.home.path()).await;
    run_turn(&mut a).await;
    let (mut b, sid_b) = create(&sock, guard.home.path()).await;
    run_turn(&mut b).await;
    assert_eq!(SocketTransport::rename(&sock, &sid_a, Some("axon")).await.unwrap(), Ok(Some("axon".into())));

    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("Bad Name")).await);
    assert_eq!(f.reason, RenameRefusal::Invalid);
    assert!(f.message.contains("invalid name 'Bad Name'"), "{}", f.message);

    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("axon")).await);
    let journal_a = listed(&sock, &sid_a).await.unwrap().journal_id;
    assert_eq!(f.reason, RenameRefusal::Taken);
    assert_eq!(f.taken_by.as_deref(), Some(journal_a.as_str()), "names the holder");
    assert!(f.message.contains("already used by session"), "{}", f.message);

    agent_engine::core::chain::save_chain("my-chain", &journal_a).expect("chain");
    let f = failure(SocketTransport::rename(&sock, &sid_b, Some("my-chain")).await);
    assert_eq!(f.reason, RenameRefusal::ChainConflict, "{}", f.message);

    assert_eq!(listed(&sock, &sid_b).await.unwrap().name, None, "nothing changed");
    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_of_an_unknown_session_is_refused_and_the_connection_closes() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let mut c = Connected::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let err = c.rename(&SessionId::from("no-such-session"), Some("x")).await.unwrap_err();
    assert!(matches!(err, TransportError::UnknownSession(ref m) if m.contains("no-such-session")), "{err}");
    assert!(c.ping().await.is_err(), "a refusal is terminal, as for pin and close");
    stop(d, &sock).await;
}

/// A parked session holds no conversation and has released its journal
/// lock: it is refused with `parked` and never woken (no rebuild, no
/// provider call, the name on disk unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_of_a_parked_session_is_refused_and_does_not_wake_it() {
    let guard = HomeGuard::new();
    let (d, hits) = boot(&guard, "0").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await, "parks");
    let calls = hits.load(Ordering::SeqCst);
    let handle = d.state.attach(&sid).expect("listed");
    let mut rx = handle.subscribe();

    let f = failure(SocketTransport::rename(&sock, &sid, Some("later")).await);
    assert_eq!(f.reason, RenameRefusal::Parked);
    assert!(f.message.contains("parked") && f.message.contains("not woken"), "{}", f.message);

    // The actor's own guard (a park that lands after the daemon's check)
    // answers the same way, still without waking.
    handle
        .send(SessionCommand::EngineCommand { id: 7, name: "rename".into(), arg: "later".into() })
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let SessionEventWire::QueryResult { id: 7, value } = rx.recv().await.unwrap().event {
                return value;
            }
        }
    })
    .await
    .expect("the actor answers");
    assert_eq!(reply["code"], "parked", "{reply}");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut events = Vec::new();
    while let Ok(env) = rx.try_recv() {
        events.push(env.event);
    }
    assert!(
        !events.iter().any(|e| matches!(e, SessionEventWire::Lifecycle(SessionLifecycle::Live))),
        "never rebuilt: {events:#?}"
    );
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
    assert_eq!(hits.load(Ordering::SeqCst), calls, "no provider call");
    let m = listed(&sock, &sid).await.unwrap();
    assert_eq!(m.name, None);
    assert_eq!(saved_name(&guard, &m.journal_id), None);
    stop(d, &sock).await;
}

// ── raw-socket client ─────────────────────────────────────────────────────────

async fn write_frame(w: &mut tokio::net::unix::OwnedWriteHalf, f: &ClientFrame) {
    w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
}

async fn read_until<T>(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut pick: impl FnMut(DaemonFrame) -> Option<T>,
) -> T {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("timely")
            .unwrap()
            .expect("open");
        if let Some(t) = pick(decode_line::<DaemonFrame>(&line).unwrap()) {
            return t;
        }
    }
}

/// An attached client has `/saveas`; neither the control frame on its
/// connection nor the host-only `rename` engine command from it renames.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn rename_from_an_attached_connection_is_refused() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();

    let (r, mut w) = tokio::net::UnixStream::connect(&sock).await.unwrap().into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    write_frame(&mut w, &ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    read_until(&mut lines, |f| matches!(f, DaemonFrame::Welcome(_)).then_some(())).await;
    let config = SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() };
    write_frame(&mut w, &ClientFrame::Attach(Attach::Create { config, mode: AttachMode::Mirror })).await;
    let sid = read_until(&mut lines, |f| match f {
        DaemonFrame::Attached(a) => Some(a.meta.id),
        _ => None,
    })
    .await;

    write_frame(&mut w, &ClientFrame::Rename { session_id: sid.clone(), name: Some("nope".into()) }).await;
    let message = read_until(&mut lines, |f| match f {
        DaemonFrame::Error { message, .. } => Some(message),
        _ => None,
    })
    .await;
    assert!(message.contains("control frames are not accepted"), "{message}");

    // The owner, stamping the host-only command itself, is refused too.
    let cmd = SessionCommand::EngineCommand { id: 9, name: "rename".into(), arg: "nope".into() };
    write_frame(&mut w, &ClientFrame::Cmd { session_id: sid.clone(), cmd }).await;
    let reason = read_until(&mut lines, |f| match f {
        DaemonFrame::Event(env) => match SessionEventWire::from(env.event) {
            SessionEventWire::Refused { reason, .. } => Some(reason),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(reason.contains("host-only"), "{reason}");
    assert_eq!(listed(&sock, &sid).await.expect("listed").name, None);

    write_frame(&mut w, &ClientFrame::Bye).await;
    stop(d, &sock).await;
}

/// `synaps daemon rename <args> --json` as a separate process: (exit code, stdout JSON).
async fn cli_rename(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.arg("daemon")
        .arg("rename")
        .args(args)
        .arg("--json")
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(std::process::Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run synaps daemon rename")).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    (out.status.code().unwrap_or(-1), json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_cli_renames_and_clears_by_prefix_and_reports_each_outcome_with_its_exit_code() {
    let guard = HomeGuard::new();
    let run: PathBuf = guard.base_dir().join("run");

    let (code, json) = cli_rename(&guard, &run, &["anything", "x"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("no_daemon")), "{json}");

    let (d, _hits) = boot(&guard, "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;

    let (code, json) = cli_rename(&guard, &run, &["no-such-session", "x"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("not_found")), "{json}");

    let prefix = &sid.as_str()[..sid.as_str().len() - 1];
    let (code, json) = cli_rename(&guard, &run, &[prefix, "daily-notes"]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!((json["ok"].as_bool(), json["outcome"].as_str()), (Some(true), Some("renamed")));
    assert_eq!(json["name"], "daily-notes");
    assert_eq!(json["session_id"], sid.as_str());
    assert_eq!(listed(&sock, &sid).await.unwrap().name.as_deref(), Some("daily-notes"));

    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "UPPER"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("invalid")), "the target can be the name: {json}");

    let (code, json) = cli_rename(&guard, &run, &["daily-notes", "--clear"]).await;
    assert_eq!((code, json["outcome"].as_str()), (0, Some("cleared")), "{json}");
    assert!(json["name"].is_null());
    assert_eq!(listed(&sock, &sid).await.unwrap().name, None);

    stop(d, &sock).await;
}
