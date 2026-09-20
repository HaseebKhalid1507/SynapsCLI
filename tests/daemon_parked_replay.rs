//! F2: attaching to a Parked session must replay the conversation (tail and
//! messages_len) exactly as a Live attach does — never an empty transcript.

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

/// Drain events until Idle.
async fn turn(t: &mut SocketTransport) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let e = next(t).await;
        let done = matches!(e.event, SessionEventWire::Idle);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

/// Poll `sessions` until the target session reaches the wanted lifecycle.
async fn wait_lifecycle(
    sock: &std::path::Path,
    sid: &SessionId,
    want: SessionLifecycle,
    budget: Duration,
) -> bool {
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

/// F2 main: create → turn → detach → park → re-attach → snapshot must
/// carry the conversation history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn attach_to_parked_session_replays_conversation() {
    // Park immediately after detach.
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");

    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .expect("host boot");

    let d = Daemon::start(
        host.clone(),
        DaemonOpts {
            runtime_dir: Some(guard.base_dir().join("run")),
            ..Default::default()
        },
    )
    .await
    .expect("daemon start");
    let sock = d.paths.sock.clone();
    let cwd = guard.home.path().to_path_buf();

    // ── create + one turn ────────────────────────────────────────────────
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, snap_create) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.clone()),
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
    // Fresh session: no history yet.
    assert_eq!(snap_create.conversation.messages_len, 0);

    // Drain ClientJoined.
    assert!(matches!(next(&mut t).await.event, SessionEventWire::ClientJoined { .. }));

    // Run a turn: stub replies "hi".
    t.send_from_self(SessionCommand::Submit {
        text: "hello".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    let seen = turn(&mut t).await;
    // The turn produced a Conversation with messages.
    let conv_after_turn = seen
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEventWire::Conversation(c) => Some(c.clone()),
            _ => None,
        })
        .expect("Conversation after turn");
    assert!(
        conv_after_turn.messages_len >= 2,
        "should have at least user + assistant, got {}",
        conv_after_turn.messages_len
    );

    // ── detach → park ────────────────────────────────────────────────────
    t.detach().await;
    assert!(
        wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await,
        "session must park after detach with grace=0"
    );

    // ── re-attach (Full client) ──────────────────────────────────────────
    let conn2 = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (_t2, snap_reattach) = SocketTransport::attach(
        conn2,
        Attach::Existing {
            session_id: sid.clone(),
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("re-attach to parked session");

    // The key assertion: messages_len must reflect the conversation.
    assert!(
        snap_reattach.conversation.messages_len >= 2,
        "F2: re-attach to parked session must have messages_len >= 2, got {}",
        snap_reattach.conversation.messages_len
    );
    assert!(
        !snap_reattach.conversation.api_messages.is_empty(),
        "F2: Full client re-attach must carry api_messages"
    );

    // ── re-attach (Digest client) ────────────────────────────────────────
    // Detach the Full client first so it parks again.
    _t2.detach().await;
    assert!(
        wait_lifecycle(&sock, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await,
        "session must re-park"
    );

    let conn3 = SocketTransport::connect(
        &sock,
        Hello::new(ClientKind::Tui)
            .with_history(HistoryMode::Digest)
            .with_tail_items(50),
    )
    .await
    .unwrap();
    let (_t3, snap_digest) = SocketTransport::attach(
        conn3,
        Attach::Existing {
            session_id: sid.clone(),
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("re-attach digest to parked session");

    assert!(
        snap_digest.conversation.messages_len >= 2,
        "F2: Digest re-attach must have messages_len >= 2, got {}",
        snap_digest.conversation.messages_len
    );
    let tail = snap_digest
        .display_tail
        .as_ref()
        .expect("Digest client must get display_tail");
    assert!(
        !tail.items.is_empty(),
        "F2: Digest re-attach display_tail must not be empty"
    );

    // ── cleanup ──────────────────────────────────────────────────────────
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait())
        .await
        .expect("daemon wait");
}
