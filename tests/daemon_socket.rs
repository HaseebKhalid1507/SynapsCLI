//! Real-UDS round trip against `agent_engine::daemon` (PLAN-phase2 §5.3).
//! Sessions are EchoActor-backed until A1 (`EngineHost::create_session`)
//! merges; everything else — flock, socket perms, daemon.json, handshake,
//! control fast path, pump, detach-without-abort — is the real code.

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{self, registry, Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts, LlmEvent, SessionEvent, StreamEvent};
use phase2::HomeGuard;
use serial_test::serial;

async fn host() -> Arc<EngineHost> {
    EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true })
        .await
        .expect("host boot")
}

async fn start(runtime_dir: &std::path::Path) -> Daemon {
    Daemon::start(
        host().await,
        DaemonOpts {
            runtime_dir: Some(runtime_dir.to_path_buf()),
            factory: Some(daemon::echo_factory()),
            ..Default::default()
        },
    )
    .await
    .expect("daemon start")
}

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(3), t.next_event())
        .await
        .expect("timely")
        .expect("open")
}

fn mode_of(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
#[serial]
async fn socket_roundtrip_real_uds() {
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let paths = d.paths.clone();

    // perms + registry
    assert_eq!(mode_of(&paths.dir), 0o700);
    assert_eq!(mode_of(&paths.sock), 0o600);
    assert_eq!(mode_of(&paths.json), 0o600);
    let info = registry::read_daemon_json(&paths).expect("daemon.json");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert!(registry::is_alive(&paths), "flock held");
    assert!(!registry::reap_stale(&paths), "live daemon never reaped");

    // control fast path allocates no session
    let pong = SocketTransport::ping(&paths.sock).await.unwrap();
    assert_eq!(pong.pid, std::process::id());
    assert_eq!(pong.sessions, 0);
    assert!(SocketTransport::sessions(&paths.sock).await.unwrap().is_empty());
    assert!(d.state.live_sessions().is_empty());

    // attach + create
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    assert_eq!(conn.welcome.pid, std::process::id());
    let (mut t, snap) = SocketTransport::attach(
        conn,
        Attach::Create { config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() }, mode: AttachMode::Mirror },
    )
    .await
    .unwrap();
    assert_eq!(snap.meta.id, *t.session_id());
    assert_eq!(d.state.live_sessions().len(), 1);
    let joined = next(&mut t).await;
    assert!(matches!(joined.event, SessionEventWire::ClientJoined { .. }));

    t.send(SessionCommand::Submit { text: "hi".into(), attachments: vec![] }).await.unwrap();
    let mut seen = Vec::new();
    loop {
        let e = next(&mut t).await;
        let done = matches!(e.event, SessionEventWire::Conversation(_));
        seen.push(e);
        if done {
            break;
        }
    }
    assert!(matches!(seen[0].event, SessionEventWire::TurnStarted { .. }));
    match &seen[1].event {
        SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(s))) => assert_eq!(s, "hi"),
        o => panic!("{o:?}"),
    }
    assert!(matches!(seen[3].event, SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))));
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "gapless {seqs:?}");

    // a second client sees the same session in the list and can attach to it
    let list = SocketTransport::sessions(&paths.sock).await.unwrap();
    assert_eq!(list.len(), 1);
    let conn2 = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let (mut t2, snap2) =
        SocketTransport::attach(conn2, Attach::Existing { session_id: list[0].id.clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap2.conversation.api_messages.len(), 2);
    assert_ne!(t2.client_id(), t.client_id());
    // both see t2 join
    let j = next(&mut t).await;
    assert!(matches!(j.event, SessionEventWire::ClientJoined { .. }));
    let j = next(&mut t2).await;
    assert!(matches!(j.event, SessionEventWire::ClientJoined { .. }));

    // detach t without ending the session
    t.detach().await;
    assert!(t.next_event().await.is_none());
    let left = next(&mut t2).await;
    assert!(matches!(left.event, SessionEventWire::ClientLeft { .. }));
    assert_eq!(d.state.live_sessions().len(), 1, "detach never ends a session");

    // unknown session refused with Error
    let conn3 = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let err = SocketTransport::attach(conn3, Attach::Existing { session_id: "nope".into(), mode: AttachMode::Mirror })
        .await
        .err()
        .unwrap();
    assert!(matches!(err, TransportError::Refused(ref m) if m.contains("unknown session")), "{err}");

    // shutdown over the socket: sessions ended, files unlinked, lock released
    SocketTransport::shutdown(&paths.sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), d.wait()).await.expect("daemon wait");
    assert!(!paths.sock.exists());
    assert!(!paths.json.exists());
    assert!(!registry::is_alive(&paths));
    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match t2.next_event().await {
                Some(e) if matches!(e.event, SessionEventWire::Ended { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap();
    assert!(ended, "attached client sees Ended on daemon stop");
}

/// §4 HIGH: a session whose history is past rpc's 1 MiB cap must still
/// attach (`Attached` carries the whole history) and stream (`Conversation`
/// is a digest, so the per-event cost is O(1) regardless of history size).
#[tokio::test]
#[serial]
async fn attach_to_session_with_history_over_1mib() {
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let paths = d.paths.clone();

    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _) = SocketTransport::attach(
        conn,
        Attach::Create { config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() }, mode: AttachMode::Mirror },
    )
    .await
    .unwrap();
    assert!(matches!(next(&mut t).await.event, SessionEventWire::ClientJoined { .. }));

    // echo: user + assistant both carry the text → history ≈ 2 × 700 KiB > 1 MiB
    let big = "z".repeat(700 * 1024);
    t.send(SessionCommand::Submit { text: big.clone(), attachments: vec![] }).await.unwrap();
    let mut conv = None;
    let mut saw_history = false;
    loop {
        let e = next(&mut t).await;
        match e.event {
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(m))) => saw_history = m.len() == 2,
            SessionEventWire::Conversation(c) => {
                conv = Some(c);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_history, "MessageHistory (> 1 MiB) delivered over the daemon wire");
    let conv = conv.unwrap();
    assert_eq!(conv.api_messages.len(), 2, "digest matched the MessageHistory mirror");
    assert_eq!(conv.api_messages[1]["content"], big);
    let bytes: usize = conv.api_messages.iter().map(|m| serde_json::to_vec(&**m).unwrap().len()).sum();
    assert!(bytes > RPC_MAX_FRAME_BYTES, "history is {bytes} B, must exceed rpc's cap for this test to mean anything");

    // a fresh client attaches to that session: Attached > 1 MiB
    let conn2 = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let (mut t2, snap2) = SocketTransport::attach(conn2, Attach::Existing { session_id: t.session_id().clone(), mode: AttachMode::Mirror })
        .await
        .expect("attach to a > 1 MiB session");
    assert_eq!(snap2.conversation.api_messages.len(), 2);
    assert_eq!(snap2.conversation.api_messages[1]["content"], big);
    assert_eq!(t2.messages().len(), 2);
    assert!(matches!(next(&mut t2).await.event, SessionEventWire::ClientJoined { .. }));

    // the second client's mirror also survives a Cancel (echo re-emits Conversation, digest-only)
    t2.send(SessionCommand::Cancel).await.unwrap();
    let e = next(&mut t2).await;
    match e.event {
        SessionEventWire::Conversation(c) => assert_eq!(c.api_messages.len(), 2),
        other => panic!("{other:?}"),
    }

    d.shutdown_token().cancel();
    d.wait().await;
}

/// §4 MED: a client may only send the user-facing subset; `Detach` only for
/// its own id; `End{host reason}`/`Attach`/`Resync` are refused with an
/// `Error` frame and the session stays alive for everyone else
/// (`End{ClientQuit}` passes the conn since C4 — ownership is the actor's).
#[tokio::test]
#[serial]
async fn client_commands_are_whitelisted() {
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let paths = d.paths.clone();

    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut a, _) = SocketTransport::attach(
        conn,
        Attach::Create { config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() }, mode: AttachMode::Mirror },
    )
    .await
    .unwrap();
    assert!(matches!(next(&mut a).await.event, SessionEventWire::ClientJoined { .. }));
    let conn2 = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let (mut b, _) = SocketTransport::attach(conn2, Attach::Existing { session_id: a.session_id().clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert!(matches!(next(&mut a).await.event, SessionEventWire::ClientJoined { .. }));
    assert!(matches!(next(&mut b).await.event, SessionEventWire::ClientJoined { .. }));
    let other = a.client_id();
    assert_ne!(other, b.client_id());

    // Errors surface as SystemNotice("refused: …") on the sender only.
    async fn refused(t: &mut SocketTransport) -> String {
        match next(t).await.event {
            SessionEventWire::SystemNotice(m) => {
                assert!(m.starts_with("refused:"), "{m}");
                m
            }
            o => panic!("{o:?}"),
        }
    }
    b.send(SessionCommand::Detach { client: other }).await.unwrap();
    assert!(refused(&mut b).await.contains("not your client id"));
    b.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
    assert!(refused(&mut b).await.contains("end:"));
    b.send(SessionCommand::Attach { client: ClientMeta::new(ClientKind::Test), mode: AttachMode::Mirror }).await.unwrap();
    assert!(refused(&mut b).await.contains("attach:"));
    b.send(SessionCommand::Resync { client: other, since_seq: 0 }).await.unwrap();
    assert!(refused(&mut b).await.contains("resync:"));
    assert_eq!(d.state.live_sessions().len(), 1, "session survives refused End");

    // a is untouched: nothing was forwarded to the actor (Save round-trips as an echo notice)
    a.send(SessionCommand::Save).await.unwrap();
    for t in [&mut a, &mut b] {
        match next(t).await.event {
            SessionEventWire::SystemNotice(m) => assert!(m.starts_with("echo:"), "{m}"),
            o => panic!("{o:?}"),
        }
    }
    // whitelisted command from b reaches the actor and fans out to both
    b.send(SessionCommand::Steer { text: "ok".into() }).await.unwrap();
    assert!(matches!(next(&mut a).await.event, SessionEventWire::Steered { .. }));
    assert!(matches!(next(&mut b).await.event, SessionEventWire::Steered { .. }));
    // Detach{own} is allowed and only b leaves
    b.send(SessionCommand::Detach { client: b.client_id() }).await.unwrap();
    assert!(matches!(next(&mut a).await.event, SessionEventWire::ClientLeft { client } if client == b.client_id()));

    d.shutdown_token().cancel();
    d.wait().await;
}

#[tokio::test]
#[serial]
async fn stale_socket_reaped_and_second_daemon_refused() {
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    // stale leftovers: socket + json with a dead pid, nobody holding the lock
    std::os::unix::net::UnixListener::bind(&paths.sock).unwrap();
    registry::write_daemon_json(
        &paths,
        &registry::DaemonInfo {
            pid: 4_000_000,
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "old".into(),
            profile: None,
            started_at: chrono::Utc::now(),
            socket: paths.sock.to_string_lossy().into_owned(),
            exe: None,
            generation: 1,
        },
    )
    .unwrap();
    let d = start(&run).await;
    assert_eq!(registry::read_daemon_json(&paths).unwrap().pid, std::process::id());
    assert!(SocketTransport::ping(&paths.sock).await.is_ok());

    // a second daemon on the same paths is refused by the flock
    let second = Daemon::start(
        host().await,
        DaemonOpts { runtime_dir: Some(run.clone()), factory: Some(daemon::echo_factory()), ..Default::default() },
    )
    .await;
    assert!(second.is_err());
    assert!(second.err().unwrap().to_string().contains("another daemon"));
    assert!(SocketTransport::ping(&paths.sock).await.is_ok(), "first daemon untouched");

    d.shutdown_token().cancel();
    d.wait().await;
}

/// §5 / §11.8c: `--idle-exit` is no longer inert once a session exists —
/// a client-less session between turns is idle-eligible; a session that
/// reports `streaming` (or does not answer the probe) pins the daemon up.
#[tokio::test]
#[serial]
async fn idle_exit_counts_clientless_idle_sessions_and_never_a_running_turn() {
    use agent_engine::session::wire::IDLE_PROBE_QUERY_ID;

    // 1. probe semantics against a hand-rolled actor
    let meta = agent_engine::session::handle::echo::meta_for(&SessionId::from("probe"));
    let view = agent_engine::session::handle::echo::view_for();
    let (h, mut ep) = SessionHandle::new(meta, view);
    let events = ep.events.clone();
    let sid = h.id.clone();
    let streaming = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let s2 = Arc::clone(&streaming);
    let actor = tokio::spawn(async move {
        while let Some(cmd) = ep.cmd_rx.recv().await {
            if let SessionCommand::Query { id, query: SessionQuery::Status } = cmd.cmd {
                assert_eq!(id, IDLE_PROBE_QUERY_ID);
                assert!(cmd.from.is_none(), "idle probe is host-originated");
                let value = serde_json::json!({ "streaming": s2.load(std::sync::atomic::Ordering::SeqCst), "pending_prompts": 0 });
                let _ = events.send(Envelope { session_id: sid.clone(), seq: 0, ts: chrono::Utc::now(), event: SessionEventWire::QueryResult { id, value } });
            }
        }
    });
    assert!(!daemon::session_is_idle(&h).await, "streaming → busy");
    streaming.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(daemon::session_is_idle(&h).await, "between turns → idle");
    actor.abort();
    let _ = actor.await;

    // 2. end to end: a created-then-detached echo session lets the daemon exit
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    let d = Daemon::start(
        host().await,
        DaemonOpts {
            runtime_dir: Some(run.clone()),
            factory: Some(daemon::echo_factory()),
            idle_exit: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let paths = d.paths.clone();
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _) = SocketTransport::attach(
        conn,
        Attach::Create { config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), ..Default::default() }, mode: AttachMode::Mirror },
    )
    .await
    .unwrap();
    assert!(matches!(next(&mut t).await.event, SessionEventWire::ClientJoined { .. }));
    // attached client pins the daemon regardless of session state
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!d.state.shutdown.is_cancelled(), "a connected client blocks idle-exit");
    assert!(!daemon::daemon_is_idle(&d.state).await);
    t.detach().await;
    assert!(t.next_event().await.is_none());
    assert_eq!(d.state.live_sessions().len(), 1, "session outlives the client");
    // now: zero connections + one client-less idle session → exits
    tokio::time::timeout(Duration::from_secs(5), d.wait()).await.expect("idle-exit fired with a live idle session");
    assert!(!paths.sock.exists());
}

/// C2: `Purge` is a control fast path — answered with `Pong`, no session
/// allocated.
#[tokio::test]
#[serial]
async fn purge_frame_answers_pong() {
    let guard = HomeGuard::new();
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let paths = d.paths.clone();
    let pong = SocketTransport::purge(&paths.sock).await.unwrap();
    assert_eq!(pong.pid, std::process::id());
    assert_eq!(pong.sessions, 0);
    assert!(d.state.live_sessions().is_empty());
    d.state.request_shutdown(false);
}
