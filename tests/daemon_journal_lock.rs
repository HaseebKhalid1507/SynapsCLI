//! F10: in-process `--continue` of a daemon-owned session must be refused.
//!
//! Verifies:
//! 1. A daemon session acquires the journal lock.
//! 2. An in-process `create_session` with `continue_session` pointing at the
//!    same session fails with an actionable error (pid, kind, --attach hint).
//! 3. After the lock is released, the same continue works.

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn in_process_continue_of_live_daemon_session_refused() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    // ── 1. Start daemon with one session ──
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true })
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

    // Create a session via the daemon (persist so --continue can find it).
    let cwd = guard.home.path().to_path_buf();
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (mut t, snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.clone()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: true,
                await_extensions: true,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("daemon attach");
    let session_id = snap.meta.journal_id.clone();

    // Run one turn so the journal exists on disk.
    t.send(SessionCommand::Submit {
        text: "hello".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    loop {
        let e = next(&mut t).await;
        if matches!(e.event, SessionEventWire::Idle) {
            break;
        }
    }

    // ── 2. Simulate in-process --continue by attempting to acquire the lock ──
    // The daemon actor already holds the journal lock. A second acquire in
    // the same process (different fd) must fail — flock is per-fd on Linux.
    let sessions_dir = agent_core::session_lock::sessions_dir();
    let holder = agent_core::session_lock::LockHolder {
        pid: std::process::id(),
        kind: "tui".to_string(),
    };
    let err =
        agent_core::session_lock::SessionLock::try_acquire(&sessions_dir, &session_id, holder);
    let err = err.expect_err("lock must be held by the daemon actor");
    let msg = err.to_string();
    assert!(
        msg.contains("is live in another process"),
        "error must say 'is live in another process': {msg}"
    );
    assert!(
        msg.contains("--attach"),
        "error must suggest --attach: {msg}"
    );

    // ── 3. Stop daemon → lock released → same acquire must succeed ──
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait())
        .await
        .expect("daemon wait");

    // Small grace for file handles to close.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let holder2 = agent_core::session_lock::LockHolder {
        pid: std::process::id(),
        kind: "tui".to_string(),
    };
    let lock =
        agent_core::session_lock::SessionLock::try_acquire(&sessions_dir, &session_id, holder2);
    lock.expect("after daemon stop, lock must be acquirable");
}
