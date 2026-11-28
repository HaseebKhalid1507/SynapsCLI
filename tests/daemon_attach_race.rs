//! #457: two connections attach to the same session at the same moment.
//! Each is told its own client id and gets the snapshot built for its own
//! `Hello`; a later detach removes the client that detached, not the other.
//! Real daemon, real actor, real socket.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event())
        .await
        .expect("timely")
        .expect("open")
}

async fn until(t: &mut SocketTransport, mut done: impl FnMut(&SessionEventWire) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await;
        let stop = done(&e.event);
        seen.push(e);
        if stop {
            return seen;
        }
    }
}

async fn wait_parked(handle: &SessionHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "the session parks");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn attach(sock: &std::path::Path, hello: Hello, sid: &SessionId) -> (SocketTransport, AttachSnapshot) {
    let conn = SocketTransport::connect(sock, hello).await.unwrap();
    SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .expect("attach")
}

/// Rounds of: park the session, then a full-history TUI and a digest client
/// attach to it at once (both Attaches queue behind the load).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_socket_attaches_each_get_their_own_answer() {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    let d = Daemon::start(host.clone(), DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start");
    let sock = d.paths.sock.clone();

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _) = SocketTransport::attach(
        conn,
        Attach::create(
            SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: true,
                client_input: Some(ClientInput::Exclusive),
                ..Default::default()
            },
            AttachMode::Mirror,
        ),
    )
    .await
    .expect("create");
    let sid = t.session_id().clone();
    t.send(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    t.detach().await;
    let handle = d.state.live_sessions().pop().expect("the session");
    assert_eq!(handle.id, sid);

    for round in 0..4 {
        wait_parked(&handle).await;
        let full = Hello::new(ClientKind::Tui);
        let digest = Hello::new(ClientKind::Attach).with_history(HistoryMode::Digest).with_tail_items(50);
        let ((a, snap_a), (b, snap_b)) = tokio::join!(attach(&sock, full, &sid), attach(&sock, digest, &sid));

        assert_ne!(a.client_id(), b.client_id(), "round {round}: each connection got its own client id");
        assert!(!snap_a.conversation.api_messages.is_empty(), "round {round}: the full client got the history");
        assert!(snap_a.display_tail.is_none(), "round {round}: the full client got no digest");
        assert!(snap_b.conversation.api_messages.is_empty(), "round {round}: the digest client got no history");
        assert!(snap_b.display_tail.is_some(), "round {round}: the digest client got its tail");

        // The session agrees with what each connection was told.
        let p = handle.presence();
        let kind_of = |id: ClientId| p.roster.iter().find(|c| c.id == id).map(|c| c.kind);
        assert_eq!(p.clients, 2, "round {round}");
        assert_eq!(kind_of(a.client_id()), Some(ClientKind::Tui), "round {round}");
        assert_eq!(kind_of(b.client_id()), Some(ClientKind::Attach), "round {round}");
        let owner = p.input_owner.expect("one of them owns input");
        assert_eq!(snap_a.input_owner, Some(owner), "round {round}");
        assert_eq!(snap_b.input_owner, Some(owner), "round {round}");

        // The one that does not own input leaves: the owner stays attached,
        // keeps input, and drives a turn.
        let (mut stay, leave) = if owner == a.client_id() { (a, b) } else { (b, a) };
        let left = leave.client_id();
        leave.detach().await;
        until(&mut stay, |e| matches!(e, SessionEventWire::ClientLeft { client } if *client == left)).await;
        let p = handle.presence();
        assert_eq!(p.roster.iter().map(|c| c.id).collect::<Vec<_>>(), [stay.client_id()], "round {round}");
        assert_eq!(p.input_owner, Some(stay.client_id()), "round {round}");
        stay.send(SessionCommand::Submit { text: "again".into(), attachments: vec![] }).await.unwrap();
        let seen = until(&mut stay, |e| matches!(e, SessionEventWire::Idle | SessionEventWire::Refused { .. })).await;
        assert!(matches!(seen.last().unwrap().event, SessionEventWire::Idle), "round {round}: the owner's turn ran");
        stay.detach().await;
    }
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait()).await.expect("daemon wait");
}
