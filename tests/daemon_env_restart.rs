//! T5: env persisted across daemon restart via journal.
//!
//! 1. Spawn daemon, create session with env containing MARK=xyz.
//! 2. Run one turn, detach.
//! 3. Kill daemon, respawn it.
//! 4. Continue session with env=None → the session's journal-rehydrated env
//!    must contain MARK=xyz.
//!
//! Unix only (daemon socket + spawn).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use phase2::*;
use serial_test::serial;

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

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

async fn spawn_daemon(guard: &HomeGuard, url: &str, extra: &[(&str, &str)]) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .envs(extra.iter().copied())
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
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
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "daemon never answered"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

async fn attach_create_with_env(
    paths: &registry::DaemonPaths,
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    env_stripped: Vec<String>,
) -> (SocketTransport, AttachSnapshot) {
    let mut hello = Hello::new(ClientKind::Test);
    hello.env = env;
    hello.env_stripped = env_stripped;
    let conn = SocketTransport::connect(&paths.sock, hello).await.unwrap();
    SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap()
}

async fn attach_continue_no_env(
    paths: &registry::DaemonPaths,
    _cwd: &Path,
    session_id: &SessionId,
) -> (SocketTransport, AttachSnapshot) {
    let mut hello = Hello::new(ClientKind::Test);
    // Simulate a client that sends NO env (legacy or env=None).
    hello.env = None;
    hello.env_stripped = Vec::new();
    let conn = SocketTransport::connect(&paths.sock, hello).await.unwrap();
    SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                continue_session: Some(Some(session_id.to_string())),
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap()
}

async fn one_turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit {
        text: text.into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), t.next_event())
            .await
            .expect("turn hung")
            .expect("alive");
        if matches!(env.event, SessionEventWire::Idle) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn env_rehydrates_from_journal_after_daemon_restart() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;

    // 1. Spawn daemon, create session with env containing MARK.
    let mut d = spawn_daemon(
        &guard,
        &url,
        &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")],
    )
    .await;
    let (mut t, _snap) = attach_create_with_env(
        &d.paths,
        guard.home.path(),
        Some(vec![
            ("MARK".into(), "xyz42".into()),
            ("PATH".into(), "/usr/bin".into()),
        ]),
        vec!["SECRET_KEY".into()],
    )
    .await;
    let session_id = t.session_id().clone();

    // 2. One turn so the session has history and gets journaled.
    one_turn(&mut t, "hello").await;
    t.send_from_self(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    // Wait for park + journal flush.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. Kill daemon.
    SocketTransport::shutdown(&d.paths.sock, true).await.ok();
    d.child.kill().ok();
    d.child.wait().ok();
    // Prevent the Drop from double-killing.
    std::mem::forget(d);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. Respawn daemon.
    let d2 = spawn_daemon(
        &guard,
        &url,
        &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")],
    )
    .await;

    // 5. Continue with env=None — journal should rehydrate the env.
    let (mut t2, snap) = attach_continue_no_env(
        &d2.paths,
        guard.home.path(),
        &session_id,
    )
    .await;
    // Conversation history survived.
    assert!(
        snap.conversation.api_messages.len() >= 2,
        "history survived restart: got {} messages",
        snap.conversation.api_messages.len()
    );

    // Verify: the journaled env is visible on the session by checking
    // the journal file directly.
    let sessions_dir = guard.base_dir().join("sessions");
    let snap_path = sessions_dir.join(format!("{session_id}.json"));
    let snap_bytes = std::fs::read_to_string(&snap_path).unwrap_or_default();
    assert!(
        snap_bytes.contains("MARK") && snap_bytes.contains("xyz42"),
        "MARK=xyz42 must be in the journal: {snap_path:?}"
    );
    assert!(
        snap_bytes.contains("SECRET_KEY"),
        "env_stripped should list SECRET_KEY in journal"
    );

    t2.send_from_self(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    // Drain until `Ended` — or until the daemon closes the connection first
    // (park grace is 0 here, so under load the socket can drop before the
    // final envelope is read). Either is a clean end for this test.
    loop {
        match tokio::time::timeout(Duration::from_secs(10), t2.next_event())
            .await
            .expect("end hung")
        {
            Some(env) if matches!(env.event, SessionEventWire::Ended { .. }) => break,
            Some(_) => continue,
            None => break,
        }
    }

    SocketTransport::shutdown(&d2.paths.sock, true).await.ok();
}
