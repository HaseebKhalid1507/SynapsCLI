//! C4 — attach modes over the wire against the REAL actor: a second
//! `Mirror` is read-only (its `Submit` is `Refused`), `Takeover` steals
//! ownership and the previous owner sees `InputOwnerChanged`; an owner may
//! `End{ClientQuit}` its session; `Observe` never owns.

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::*;
use serial_test::serial;

async fn start(runtime_dir: &std::path::Path) -> Daemon {
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true }).await.expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(runtime_dir.to_path_buf()), ..Default::default() })
        .await
        .expect("daemon start")
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> SessionEventWire {
    loop {
        let env = tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely").expect("open");
        if pred(&env.event) {
            return env.event;
        }
    }
}

async fn attach(paths: &agent_engine::daemon::registry::DaemonPaths, kind: ClientKind, a: Attach) -> (SocketTransport, AttachSnapshot) {
    let conn = SocketTransport::connect(&paths.sock, Hello::new(kind)).await.unwrap();
    SocketTransport::attach(conn, a).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mirror_is_read_only_takeover_steals_owner_may_end() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let paths = d.paths.clone();

    let (mut a, snap) = attach(
        &paths,
        ClientKind::Tui,
        Attach::Create {
            config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), model_override: Some("claude-sonnet-4-5".into()), persist: false, ..Default::default() },
            mode: AttachMode::Mirror,
        },
    )
    .await;
    assert_eq!(snap.input_owner, Some(a.client_id()), "first mirror owns");
    let sid = a.session_id().clone();

    // Second mirror: read-only.
    let (mut b, snap_b) = attach(&paths, ClientKind::Attach, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror }).await;
    assert_eq!(snap_b.input_owner, Some(a.client_id()));
    b.send_from_self(SessionCommand::Submit { text: "from b".into(), attachments: vec![] }).await.unwrap();
    let r = until(&mut b, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    let SessionEventWire::Refused { client, command, reason } = r else { unreachable!() };
    assert_eq!(client, b.client_id());
    assert_eq!(command, "submit");
    assert!(reason.contains("owned by"), "{reason}");

    // Observer: never owns, refused too.
    let (mut o, snap_o) = attach(&paths, ClientKind::Attach, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Observe }).await;
    assert_eq!(snap_o.input_owner, Some(a.client_id()));
    o.send_from_self(SessionCommand::Steer { text: "x".into() }).await.unwrap();
    until(&mut o, |e| matches!(e, SessionEventWire::Refused { .. })).await;

    // Takeover: c steals; a is told; a's next Submit is refused; c's works.
    let (mut c, snap_c) = attach(&paths, ClientKind::Attach, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Takeover }).await;
    assert_eq!(snap_c.input_owner, Some(c.client_id()));
    let ev = until(&mut a, |e| matches!(e, SessionEventWire::InputOwnerChanged { .. })).await;
    let SessionEventWire::InputOwnerChanged { from, to, reason } = ev else { unreachable!() };
    assert_eq!(from, Some(a.client_id()));
    assert_eq!(to, Some(c.client_id()));
    assert_eq!(reason, OwnerChangeReason::Takeover);
    assert_eq!(a.input_owner(), Some(c.client_id()));
    a.send_from_self(SessionCommand::Submit { text: "from a".into(), attachments: vec![] }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    c.send_from_self(SessionCommand::Submit { text: "from c".into(), attachments: vec![] }).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;

    // Listing carries lifecycle + journal id (owner/clients come with B4's cells).
    let list = SocketTransport::sessions(&paths.sock).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].lifecycle, SessionLifecycle::Live);
    assert_eq!(list[0].journal_id, sid.as_str());

    // Non-owner End is refused by the actor; owner End ends for everyone.
    b.send_from_self(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::Refused { command, .. } if command == "end")).await;
    assert_eq!(d.state.live_sessions().len(), 1, "session survives a non-owner End");
    c.send_from_self(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
    // `Ended` then `Bye`/EOF; the conn may close before the forwarder got
    // `Ended` out (pre-existing race, daemon_socket.rs:148 tolerates it too).
    for t in [&mut a, &mut b, &mut c, &mut o] {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("timely") {
                Some(env) if matches!(env.event, SessionEventWire::Ended { .. }) => break,
                Some(_) => continue,
                None => break,
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(d.state.live_sessions().len(), 0);

    d.shutdown_token().cancel();
    d.wait().await;
}
