//! `synaps daemon close` (`ClientFrame::Close`): end one session from a
//! control connection, with host authority, through the actor's own
//! `/close`. Real host (`host_factory`), real UDS, loopback Anthropic stub.

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
use agent_engine::{EngineHost, HostOpts, StreamEvent};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX};
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
async fn boot(guard: &HomeGuard, script: Script, park_grace: &str) -> (Daemon, Arc<std::sync::atomic::AtomicUsize>) {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", park_grace);
    let (url, hits, _bodies) = spawn_stub(script).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (start(guard).await, hits)
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

async fn listed(sock: &Path) -> Vec<SessionId> {
    SocketTransport::sessions(sock).await.unwrap().into_iter().map(|m| m.id).collect()
}

async fn wait_lifecycle(sock: &Path, sid: &SessionId, want: SessionLifecycle, budget: Duration) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        let metas = SocketTransport::sessions(sock).await.unwrap_or_default();
        if metas.iter().any(|m| &m.id == sid && m.lifecycle == want) {
            return true;
        }
        if t0.elapsed() > budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// After `detach()`, wait until the daemon lists no client on `sid` (the
/// detach reaches the actor asynchronously).
async fn wait_detached(sock: &Path, sid: &SessionId) {
    let t0 = std::time::Instant::now();
    while SocketTransport::sessions(sock).await.unwrap_or_default().iter().any(|m| &m.id == sid && m.clients > 0) {
        assert!(t0.elapsed() < Duration::from_secs(10), "{sid} still lists a client after detach");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(d: Daemon, sock: &Path) {
    SocketTransport::shutdown(sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_ends_an_idle_session_and_leaves_the_others() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
    let sock = d.paths.sock.clone();
    let cwd = guard.home.path().to_path_buf();

    let (mut ta, a) = create(&sock, &cwd).await;
    run_turn(&mut ta).await;
    let (mut tb, b) = create(&sock, &cwd).await;
    run_turn(&mut tb).await;
    ta.detach().await;
    tb.detach().await;
    wait_detached(&sock, &a).await;

    let text = SocketTransport::close(&sock, &a, false, false).await.expect("closed");
    assert!(text.contains(&format!("--continue {a}")), "{text}");
    assert_eq!(listed(&sock).await, vec![b.clone()], "only the closed session left");

    // The other session is untouched and still runs turns.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut tb, _) = SocketTransport::attach(conn, Attach::Existing { session_id: b.clone(), mode: AttachMode::Takeover })
        .await
        .expect("re-attach the other session");
    run_turn(&mut tb).await;
    tb.detach().await;

    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_refuses_mid_turn_without_force_and_force_ends_it() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Endless(ANTHROPIC_SSE_PREFIX), "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    t.send_from_self(SessionCommand::Submit { text: "go".into(), attachments: vec![] }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Stream(StreamEvent::Llm(_)))).await;

    match SocketTransport::close(&sock, &sid, false, true).await {
        Err(TransportError::Refused(m)) => {
            assert!(m.contains("a turn is running"), "{m}");
            assert!(m.contains("/close force"), "the daemon passes the actor's text through: {m}");
        }
        other => panic!("expected a busy refusal, got {other:?}"),
    }
    assert!(listed(&sock).await.contains(&sid), "a refused close leaves the session");

    let text = SocketTransport::close(&sock, &sid, true, true).await.expect("force closes");
    assert!(text.contains("closing session"), "{text}");
    assert!(!listed(&sock).await.contains(&sid));
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(matches!(seen.last().unwrap().event, SessionEventWire::Ended { reason: EndReason::ClientQuit }));

    stop(d, &sock).await;
}

/// A session with a client attached is refused without `attached`, and
/// `force` alone does not override that (it only overrides work in flight);
/// the refusal says how many clients and how to close it anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_refuses_a_session_with_clients_attached_unless_asked() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut owner).await;

    for force in [false, true] {
        match SocketTransport::close(&sock, &sid, force, false).await {
            Err(TransportError::Refused(m)) => {
                assert!(m.contains("1 client attached"), "force={force}: {m}");
                assert!(m.contains("--attached"), "force={force}: {m}");
            }
            other => panic!("force={force}: expected an attached refusal, got {other:?}"),
        }
        assert!(listed(&sock).await.contains(&sid), "a refused close leaves the session");
    }
    // The owner was not disturbed: it still runs turns.
    run_turn(&mut owner).await;

    let text = SocketTransport::close(&sock, &sid, false, true).await.expect("closed with attached");
    assert!(text.contains("--continue"), "{text}");
    assert!(!listed(&sock).await.contains(&sid));

    stop(d, &sock).await;
}

/// The attached owner's pump sees the actor go away at the same moment its
/// forwarder holds the `Ended`; the client must get `Ended` before EOF
/// (that is what makes its TUI print how to continue the session).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_reaches_an_attached_owners_tui_with_ended_before_eof() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
    let sock = d.paths.sock.clone();
    let (mut owner, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut owner).await;

    let text = SocketTransport::close(&sock, &sid, false, true).await.expect("closed");
    assert!(text.contains("--continue"), "{text}");

    let mut seen = Vec::new();
    while let Some(env) = tokio::time::timeout(Duration::from_secs(10), owner.next_event()).await.expect("timely") {
        seen.push(env.event);
    }
    assert!(
        matches!(seen.last(), Some(SessionEventWire::Ended { reason: EndReason::ClientQuit })),
        "the owner must receive Ended{{ClientQuit}} before the connection closes: {seen:#?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id >= RESERVED_QUERY_ID_BASE)),
        "the daemon's close reply is never surfaced to clients"
    );

    stop(d, &sock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_does_not_unpark_a_parked_session() {
    let guard = HomeGuard::new();
    let (d, hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "0").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    t.detach().await;
    assert!(
        wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await,
        "session must park after detach with grace=0"
    );
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "never");
    let calls = hits.load(Ordering::SeqCst);
    let mut rx = d.state.attach(&sid).expect("parked session is listed").subscribe();

    let text = SocketTransport::close(&sock, &sid, false, false).await.expect("closed");
    assert!(text.contains(&format!("--continue {sid}")), "{text}");
    assert!(!listed(&sock).await.contains(&sid));

    let mut events = Vec::new();
    while let Ok(env) = rx.try_recv() {
        events.push(env.event);
    }
    assert!(events.iter().any(|e| matches!(e, SessionEventWire::Ended { .. })), "{events:#?}");
    assert!(
        !events.iter().any(|e| matches!(e, SessionEventWire::Lifecycle(SessionLifecycle::Live))),
        "closing never rebuilt the session: {events:#?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), calls, "no provider call");

    stop(d, &sock).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_unknown_session_is_refused_and_the_connection_closes() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
    let sock = d.paths.sock.clone();

    let mut c = Connected::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let err = c.close(&SessionId::from("no-such-session"), false, false).await.unwrap_err();
    assert!(matches!(err, TransportError::UnknownSession(ref m) if m.contains("no-such-session")), "{err}");
    assert!(c.ping().await.is_err(), "a refusal is terminal: the daemon closed the connection");

    stop(d, &sock).await;
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

/// An attached client has `/close`; the control frame is refused on its
/// connection and the session stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_on_an_attached_connection_is_refused_as_a_control_frame() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
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

    write_frame(&mut w, &ClientFrame::Close { session_id: sid.clone(), force: true, attached: true }).await;
    let message = read_until(&mut lines, |f| match f {
        DaemonFrame::Error { message, .. } => Some(message),
        _ => None,
    })
    .await;
    assert!(message.contains("control frames are not accepted"), "{message}");
    assert!(listed(&sock).await.contains(&sid), "the session stays");

    write_frame(&mut w, &ClientFrame::Bye).await;
    stop(d, &sock).await;
}

/// A turn whose reply is `n` text deltas: enough frames to fill an unread
/// socket and the connection's bounded writer queue behind it.
fn many_deltas_sse(n: usize) -> &'static str {
    let mut s = String::from(concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    ));
    let chunk = "x".repeat(200);
    for _ in 0..n {
        s.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{chunk}\"}}}}\n\n"
        ));
    }
    s.push_str(concat!(
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));
    Box::leak(s.into_boxed_str())
}

/// Deterministic form of the delivery race: the owner has not read its
/// socket, so its forwarder is parked behind the full writer queue when the
/// session ends. Aborting the forwarder there (the old `ended` path) dropped
/// `Ended`; the client then saw `Bye`/EOF and never learned the session was
/// closed. The pump now lets the forwarder finish once the client reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_owner_whose_socket_is_backed_up_still_gets_ended_before_eof() {
    let guard = HomeGuard::new();
    let (d, _hits) = boot(&guard, Script::Sse(many_deltas_sse(3000)), "never").await;
    let sock = d.paths.sock.clone();

    let (r, mut w) = tokio::net::UnixStream::connect(&sock).await.unwrap().into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    write_frame(&mut w, &ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    read_until(&mut lines, |f| matches!(f, DaemonFrame::Welcome(_)).then_some(())).await;
    let config = SessionConfig {
        cwd: Some(guard.home.path().to_path_buf()),
        model_override: Some("claude-sonnet-4-5".into()),
        ..Default::default()
    };
    write_frame(&mut w, &ClientFrame::Attach(Attach::Create { config, mode: AttachMode::Mirror })).await;
    let sid = read_until(&mut lines, |f| match f {
        DaemonFrame::Attached(a) => Some(a.meta.id),
        _ => None,
    })
    .await;

    // Watch the turn in-process; the owner reads nothing from here on.
    let mut rx = d.state.attach(&sid).expect("listed").subscribe();
    let submit = SessionCommand::Submit { text: "go".into(), attachments: vec![] };
    write_frame(&mut w, &ClientFrame::Cmd { session_id: sid.clone(), cmd: submit }).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Ok(env) if matches!(env.event, SessionEventWire::Idle) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("session stream closed: {e}"),
            }
        }
    })
    .await
    .expect("the turn finishes");

    let text = SocketTransport::close(&sock, &sid, false, true).await.expect("closed");
    assert!(text.contains("--continue"), "{text}");

    // Only now does the owner drain its socket, to EOF.
    let mut ended = false;
    let mut frames = 0usize;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await.expect("timely");
        let Ok(Some(line)) = line else { break };
        frames += 1;
        match decode_line::<DaemonFrame>(&line).unwrap() {
            DaemonFrame::Event(e) if matches!(e.event, WireSessionEvent::Ended { .. }) => ended = true,
            DaemonFrame::Bye { .. } => {
                assert!(ended, "Bye after {frames} frames without Ended: the client never learns its session closed");
            }
            _ => {}
        }
    }
    assert!(frames > 256, "the writer queue was never full ({frames} frames), so this proves nothing");
    assert!(ended, "the owner must receive Ended before EOF");

    stop(d, &sock).await;
}

// ── the CLI ──────────────────────────────────────────────────────────────────

/// `synaps daemon close <args> --json` as a separate process against the
/// daemon under `run`: (exit code, parsed stdout).
async fn cli_close(guard: &HomeGuard, run: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.args(["daemon", "close"])
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
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run synaps daemon close")).await.unwrap();
    let json = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code().unwrap_or(-1), json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_cli_resolves_a_prefix_and_reports_each_outcome_with_its_exit_code() {
    let guard = HomeGuard::new();
    let run: PathBuf = guard.base_dir().join("run");

    let (code, json) = cli_close(&guard, &run, &["anything"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("no_daemon")), "{json}");

    let (d, _hits) = boot(&guard, Script::Sse(ANTHROPIC_SSE), "never").await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;

    let (code, json) = cli_close(&guard, &run, &[sid.as_str()]).await;
    assert_eq!((code, json["outcome"].as_str()), (3, Some("refused")), "attached: {json}");
    assert!(json["text"].as_str().unwrap().contains("--attached"), "{json}");
    t.detach().await;
    wait_detached(&sock, &sid).await;

    let (code, json) = cli_close(&guard, &run, &["no-such-session"]).await;
    assert_eq!((code, json["outcome"].as_str()), (1, Some("not_found")), "{json}");

    let prefix = &sid.as_str()[..sid.as_str().len() - 1];
    let (code, json) = cli_close(&guard, &run, &[prefix]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["ok"], true);
    assert_eq!(json["outcome"], "closed");
    assert_eq!(json["session_id"], sid.as_str());
    assert!(json["text"].as_str().unwrap().contains("--continue"), "{json}");
    assert!(!listed(&sock).await.contains(&sid));

    stop(d, &sock).await;
}
