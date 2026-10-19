//! `synaps daemon pin` (`ClientFrame::Pin`): pin or unpin one session from a
//! control connection, with host authority, through the actor's own
//! `KeepWarm`. A pin holds parking, F18 idle-end and the daemon's idle exit.
//! Real host (`host_factory`), real UDS, loopback Anthropic stub.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{Connected, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn start(guard: &HomeGuard, idle_exit: Option<Duration>) -> Daemon {
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(
        host,
        DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), idle_exit, ..Default::default() },
    )
    .await
    .expect("daemon start")
}

/// Stub provider + daemon; `park_grace` is `SYNAPS_DAEMON_PARK_GRACE_SECS`.
async fn boot(
    guard: &HomeGuard,
    park_grace: &str,
    idle_exit: Option<Duration>,
) -> (Daemon, Arc<std::sync::atomic::AtomicUsize>) {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", park_grace);
    let (url, hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (start(guard, idle_exit).await, hits)
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), t.next_event())
            .await
            .expect("timely")
            .expect("open");
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pinned_session_is_listed_as_pinned_and_never_parks_until_unpinned() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "0", None).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    assert!(!listed(&sock, &sid).await.expect("listed").pinned);

    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"), "the reply says pinned");
    assert!(listed(&sock, &sid).await.expect("listed").pinned, "the listing shows the pin");
    // Pinning again is not an error.
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned again"));

    t.detach().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let m = listed(&sock, &sid).await.expect("still listed");
    assert_eq!(m.lifecycle, SessionLifecycle::Live, "grace 0, detached, yet pinned: stays live");

    assert!(!SocketTransport::pin(&sock, &sid, false).await.expect("unpinned"));
    assert!(wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await, "unpinned, it parks");
    assert!(!listed(&sock, &sid).await.expect("listed").pinned);

    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pinning_a_parked_session_wakes_it_and_unpinning_one_leaves_it_parked() {
    let guard = HomeGuard::new();
    let (d, hits) = boot(&guard, "0", None).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(
        wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await,
        "session must park after detach with grace=0"
    );
    let calls = hits.load(Ordering::SeqCst);
    let handle = d.state.attach(&sid).expect("parked session is listed");
    let mut rx = handle.subscribe();

    // Unpinning a parked session only clears the flag.
    assert!(!SocketTransport::pin(&sock, &sid, false).await.expect("unpinned"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut events = Vec::new();
    while let Ok(env) = rx.try_recv() {
        events.push(env.event);
    }
    assert!(
        !events.iter().any(|e| matches!(e, SessionEventWire::Lifecycle(SessionLifecycle::Live))),
        "unpinning never rebuilt the session: {events:#?}"
    );
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
    assert_eq!(hits.load(Ordering::SeqCst), calls, "no provider call");

    // Pinning one wakes it, and it stays awake past the park grace.
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"));
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live, "the pin is applied after the unpark");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let m = listed(&sock, &sid).await.expect("listed");
    assert_eq!((m.lifecycle, m.pinned), (SessionLifecycle::Live, true));
    let mut events = Vec::new();
    while let Ok(env) = rx.try_recv() {
        events.push(env.event);
    }
    assert!(events.iter().any(|e| matches!(e, SessionEventWire::Lifecycle(SessionLifecycle::Live))), "{events:#?}");

    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

/// With no connections and nothing running, the daemon idle-exits (ending
/// every session); a pin holds that, and unpinning releases it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pin_holds_the_daemons_idle_exit_and_unpinning_releases_it() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never", Some(Duration::from_secs(1))).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    // Pinned while still attached, so the daemon never sees an idle moment
    // without the pin.
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"));
    t.detach().await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!d.state.shutdown.is_cancelled(), "a pinned session holds the idle exit");
    assert!(d.state.attach(&sid).is_some(), "and the session is still there");

    assert!(!SocketTransport::pin(&sock, &sid, false).await.expect("unpinned"));
    tokio::time::timeout(Duration::from_secs(10), d.wait())
        .await
        .expect("unpinned, the daemon idle-exits");
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pin_of_an_unknown_session_is_refused_and_the_connection_closes() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never", None).await;
    let sock = d.paths.sock.clone();

    let mut c = Connected::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let err = c.pin(&SessionId::from("no-such-session"), true).await.unwrap_err();
    assert!(matches!(err, TransportError::UnknownSession(ref m) if m.contains("no-such-session")), "{err}");
    assert!(c.ping().await.is_err(), "a refusal is terminal: the daemon closed the connection");

    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

// ── raw-socket client for the pump-loop refusal ──────────────────────────────

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

/// An attached client has `/pin`; the control frame is refused on its
/// connection and the pin is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pin_on_an_attached_connection_is_refused_as_a_control_frame() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, "never", None).await;
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

    write_frame(&mut w, &ClientFrame::Pin { session_id: sid.clone(), on: true }).await;
    let message = read_until(&mut lines, |f| match f {
        DaemonFrame::Error { message, .. } => Some(message),
        _ => None,
    })
    .await;
    assert!(message.contains("control frames are not accepted"), "{message}");
    assert!(!listed(&sock, &sid).await.expect("the session stays").pinned, "and is not pinned");

    write_frame(&mut w, &ClientFrame::Bye).await;
    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

/// `synaps daemon <args>` as a separate process against the daemon under
/// `run`: (exit code, stdout).
async fn cli(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.arg("daemon")
        .args(args)
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(std::process::Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run synaps daemon")).await.unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `synaps daemon pin <args> --json`: (exit code, parsed stdout).
async fn cli_pin(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let mut all = vec!["pin"];
    all.extend_from_slice(args);
    all.push("--json");
    let (code, stdout) = cli(guard, run, &all).await;
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    (code, json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_cli_pins_and_unpins_a_prefix_and_reports_each_outcome_with_its_exit_code() {
    let guard = HomeGuard::new();
    let run: PathBuf = guard.base_dir().join("run");

    let (code, json) = cli_pin(&guard, &run, &["anything"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("no_daemon")), "{json}");

    let (d, _hits) = boot(&guard, "never", None).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;

    let (code, json) = cli_pin(&guard, &run, &["no-such-session"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("not_found")), "{json}");
    assert_eq!(json["ok"], false);

    let prefix = &sid.as_str()[..sid.as_str().len() - 1];
    let (code, json) = cli_pin(&guard, &run, &[prefix]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["ok"], true);
    assert_eq!(json["outcome"], "pinned");
    assert_eq!(json["pinned"], true);
    assert_eq!(json["session_id"], sid.as_str());
    assert!(listed(&sock, &sid).await.expect("listed").pinned);

    // The text listing marks it after the lifecycle.
    let (code, text) = cli(&guard, &run, &["sessions"]).await;
    assert_eq!(code, 0, "{text}");
    let line = text.lines().find(|l| l.starts_with(sid.as_str())).unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("Live  pinned  clients="), "{line}");

    let (code, json) = cli_pin(&guard, &run, &[prefix, "--off"]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["outcome"], "unpinned");
    assert_eq!(json["pinned"], false);
    assert!(!listed(&sock, &sid).await.expect("listed").pinned);
    let (_, text) = cli(&guard, &run, &["sessions"]).await;
    assert!(!text.contains("  pinned  "), "{text}");

    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}
