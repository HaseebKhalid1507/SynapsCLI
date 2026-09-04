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
    // t sees t2 join
    let j = next(&mut t).await;
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
    assert!(matches!(err, TransportError::Protocol(ref m) if m.contains("unknown session")), "{err}");

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
