//! F23 — reload with a lock-held journal never aliases to an empty
//! impostor. Instead it registers a Parked placeholder under the same id
//! whose attach is refused naming the lock holder.
//!
//! Steps:
//! 1. Daemon session with one turn; park it (PARK_GRACE_SECS=0).
//! 2. Take the SessionLock from the test process (simulates in-process
//!    `synaps --continue X` holding the lock).
//! 3. `Reload{now}` → rehydrate hits SessionLockError::Held.
//! 4. Assert: same id, lifecycle Parked, attach refused naming the pid.
//! 5. Release lock; assert the session is still listed (not lost).
//! 6. No "recreating fresh" for that id in the daemon's behaviour.
//!
//! Unix only (flock + execv).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_core::session_lock::{LockHolder, SessionLock};
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

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")
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
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

async fn attach_create(paths: &registry::DaemonPaths, cwd: &Path) -> (SocketTransport, AttachSnapshot) {
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
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

async fn one_turn(t: &mut SocketTransport, text: &str) -> Vec<agent_engine::SharedMessage> {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] }).await.unwrap();
    let mut msgs = None;
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.expect("turn hung").expect("alive");
        match env.event {
            SessionEventWire::Conversation(c) => msgs = Some(c.api_messages),
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
    msgs.expect("conversation after the turn")
}

/// Wait until `sessions` lists `sid` with `want` (≤ `for_`).
async fn wait_lifecycle(
    paths: &registry::DaemonPaths,
    sid: &SessionId,
    want: SessionLifecycle,
    for_: Duration,
) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        let metas = SocketTransport::sessions(&paths.sock).await.unwrap_or_default();
        if metas.iter().any(|m| &m.id == sid && m.lifecycle == want) {
            return true;
        }
        if t0.elapsed() > for_ {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_lock_held_keeps_same_id_parked_and_refuses_attach() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;

    // 1. Create a session, run one turn, then detach so it parks.
    let (mut t, _snap) = attach_create(&d.paths, guard.home.path()).await;
    let sid = t.session_id().clone();
    let before = one_turn(&mut t, "hello f23").await;
    assert_eq!(before.len(), 2, "user + assistant");
    t.detach().await;

    // Wait for it to park (grace = 0).
    assert!(
        wait_lifecycle(&d.paths, &sid, SessionLifecycle::Parked, Duration::from_secs(10)).await,
        "session must park"
    );

    // 2. Take the session lock from THIS process (simulates in-process runtime).
    let sessions_dir = guard.base_dir().join("sessions");
    // The journal_id is the session id itself (no compaction happened).
    let journal_id = sid.as_str();
    let _held_lock = SessionLock::try_acquire(
        &sessions_dir,
        journal_id,
        LockHolder { pid: std::process::id(), kind: "test".to_string() },
    )
    .expect("test process must acquire the session lock (session is Parked = lock released)");

    // 3. Reload (now) — rehydrate hits the held lock.
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None)
        .await
        .expect("reload accepted");
    assert_eq!(gen, 2);

    // Wait for the new image to answer.
    let t0 = std::time::Instant::now();
    loop {
        if registry::read_daemon_json(&d.paths).is_some_and(|i| i.generation == 2)
            && SocketTransport::ping(&d.paths.sock).await.is_ok()
        {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "new image never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 4. Assert: same id, lifecycle Parked, locked_by set.
    let metas = SocketTransport::sessions(&d.paths.sock).await.expect("sessions");
    let session = metas.iter().find(|m| m.id == sid).expect("session must exist under the SAME id");
    assert_eq!(session.lifecycle, SessionLifecycle::Parked, "must be Parked, not Live");
    assert!(
        session.locked_by.is_some(),
        "locked_by must be set on the placeholder"
    );
    let locked_by = session.locked_by.as_ref().unwrap();
    assert!(
        locked_by.contains(&format!("pid {}", std::process::id())),
        "locked_by must name our pid: {locked_by}"
    );

    // No new id should exist (no alias / no fresh session).
    assert_eq!(
        metas.iter().filter(|m| m.lifecycle != SessionLifecycle::Parked || m.id != sid).count(),
        0,
        "no other sessions should exist: {metas:?}"
    );

    // 5. Try to attach — must be refused naming the pid.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let result = SocketTransport::attach(
        conn,
        Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror },
    )
    .await;
    match result {
        Err(TransportError::Refused(msg)) => {
            assert!(
                msg.contains("journal locked") || msg.contains("locked by"),
                "refusal must mention the lock: {msg}"
            );
            assert!(
                msg.contains(&format!("pid {}", std::process::id())),
                "refusal must name our pid: {msg}"
            );
        }
        Ok(_) => panic!("expected attach refused, got Ok"),
        Err(other) => panic!("expected Refused, got: {other}"),
    }

    // 6. Release the lock.
    drop(_held_lock);

    // The session should still be listed (same id, Parked).
    let metas = SocketTransport::sessions(&d.paths.sock).await.expect("sessions after release");
    assert!(
        metas.iter().any(|m| m.id == sid),
        "session must still exist after lock release: {metas:?}"
    );

    // 7. RECOVERY: the next attach retries the real create under the same id —
    //    the placeholder is replaced and the journal's history comes back.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, snap) = SocketTransport::attach(
        conn,
        Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror },
    )
    .await
    .expect("attach after lock release must succeed (placeholder replaced)");
    assert_eq!(snap.meta.id, sid, "same id, no alias");
    assert!(snap.meta.locked_by.is_none(), "real session, not the placeholder: {:?}", snap.meta);
    assert!(
        snap.conversation.messages_len >= 2,
        "history restored from journal, got messages_len={}",
        snap.conversation.messages_len
    );
    let metas = SocketTransport::sessions(&d.paths.sock).await.unwrap();
    assert_eq!(metas.iter().filter(|m| m.id == sid).count(), 1, "exactly one entry: {metas:?}");
    assert!(metas.iter().all(|m| m.locked_by.is_none()), "no placeholder left: {metas:?}");
    let _ = t.detach().await;

    // Cleanup.
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
