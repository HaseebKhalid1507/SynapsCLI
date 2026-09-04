//! C3 — `synaps daemon reload` against a REAL daemon process (PLAN-phase3
//! §5.4): the `synaps` binary is spawned as `daemon --foreground`, a client
//! attaches over the socket and runs one scripted turn, `Reload{now}` is
//! sent on a control connection, the client sees `Reloading` + EOF and
//! `reconnect`s; the pid is unchanged, `daemon.json.generation == 2`, the
//! flock is still held, the conversation survived, a second turn works.
//! Plus: an OLDER binary (a shell script faking `--print-version`) is
//! refused up front and the daemon keeps serving.
//!
//! Unix only (execv + flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::path::{Path, PathBuf};
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

/// Spawn `synaps daemon --foreground` under the guard's HOME with the stub
/// provider; wait for the socket to answer Ping (≤ 20 s).
async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    spawn_daemon_env(guard, url, &[]).await
}

async fn spawn_daemon_env(guard: &HomeGuard, url: &str, extra: &[(&str, &str)]) -> DaemonProc {
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

fn fake_older_binary(dir: &Path) -> PathBuf {
    let p = dir.join("synaps-old");
    std::fs::write(
        &p,
        "#!/bin/sh\nif [ \"$2\" = \"--print-version\" ]; then echo '{\"binary_version\":\"0.0.1\",\"protocol_version\":2}'; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_keeps_pid_sessions_and_reconnects_clients() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();
    let info = registry::read_daemon_json(&d.paths).expect("daemon.json");
    assert_eq!(info.pid, pid_before);
    assert_eq!(info.generation, 1);
    assert_eq!(info.exe.as_deref().map(|p| p.canonicalize().unwrap()), Some(Path::new(bin()).canonicalize().unwrap()));

    let (mut t, snap) = attach_create(&d.paths, guard.home.path()).await;
    assert_eq!(t.welcome.generation, 1);
    assert_eq!(snap.input_owner, Some(t.client_id()), "first mirror owns");
    let sid = t.session_id().clone();
    let before = one_turn(&mut t, "hello before reload").await;
    assert_eq!(before.len(), 2, "user + assistant");

    // Older binary → refused up front; the daemon keeps serving.
    let old = fake_older_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(old)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("older")), "{r:?}");
    assert!(registry::is_alive(&d.paths));
    assert_eq!(registry::read_daemon_json(&d.paths).unwrap().generation, 1);
    assert!(SocketTransport::ping(&d.paths.sock).await.is_ok());

    // Real reload to the same binary (equal version = allowed).
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);

    // The attached client sees Reloading, then EOF flagged as a reload.
    let mut saw_reloading = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("announce") {
            Some(env) => {
                if let SessionEventWire::Reloading { generation, retry_after_ms } = env.event {
                    assert_eq!(generation, 2);
                    assert!(retry_after_ms > 0);
                    saw_reloading = true;
                }
            }
            None => break,
        }
    }
    assert!(saw_reloading);
    assert!(matches!(t.last_error(), Some(TransportError::Reloading { generation: 2 })));

    // Reconnect (backoff) → same session, same history, owner again.
    let snap = tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");
    assert_eq!(t.welcome.generation, 2);
    assert_eq!(t.welcome.pid, pid_before, "same pid after execv");
    assert_eq!(t.session_id(), &sid, "session id survives (same journal)");
    assert_eq!(
        serde_json::to_string(&snap.conversation.api_messages).unwrap(),
        serde_json::to_string(&before).unwrap(),
        "conversation survived the reload"
    );
    assert_eq!(snap.input_owner, Some(t.client_id()), "was_owner → Takeover → owner again");

    let info = registry::read_daemon_json(&d.paths).unwrap();
    assert_eq!(info.pid, pid_before);
    assert_eq!(info.generation, 2);
    assert!(registry::is_alive(&d.paths), "flock adopted, never released");
    assert!(!agent_engine::daemon::reload::reload_state_path(&d.paths).exists(), "reload-state consumed");

    let after = one_turn(&mut t, "hello after reload").await;
    assert_eq!(after.len(), 4);

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!registry::is_alive(&d.paths));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_with_turn_in_flight_checkpoints_and_saves_abort_context() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();

    let (mut t, _snap) = attach_create(&d.paths, guard.home.path()).await;
    let sid = t.session_id().clone();
    t.send_from_self(SessionCommand::Submit { text: "never finishes".into(), attachments: vec![] }).await.unwrap();
    // Wait for the partial text so the checkpoint has something to fold.
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.unwrap().unwrap();
        if matches!(env.event, SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))) {
            break;
        }
    }

    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    // Checkpoint before the announce: the client sees the abort notice
    // (the `Conversation` that follows arrives as a digest the mirror must
    // re-query, and the connection is gone before the answer — so the
    // abort context itself is asserted from the reconnect snapshot, i.e.
    // from the journal), then Reloading, then EOF.
    let mut notices = Vec::new();
    let mut aborted = false;
    while let Some(env) = tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("announce") {
        match env.event {
            SessionEventWire::SystemNotice(n) => notices.push(n),
            SessionEventWire::Aborted { .. } => aborted = true,
            _ => {}
        }
    }
    assert!(aborted, "typed Aborted expected; notices: {notices:?}");
    assert!(notices.iter().any(|n| n.contains("daemon reloading")), "{notices:?}");

    let snap = tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror)).await.unwrap().expect("reconnected");
    assert_eq!(t.welcome.pid, pid_before);
    assert_eq!(t.session_id(), &sid);
    assert!(!snap.streaming, "the turn was checkpointed, not resumed");
    let ctx = snap.conversation.abort_context.clone().expect("abort context came back from the journal");
    assert!(ctx.contains("[response]: partial"), "{ctx}");
    assert_eq!(snap.conversation.api_messages.len(), 1, "the interrupted assistant turn is not in the history");

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// Wait until `sessions` lists `sid` with `want` (≤ `for_`).
async fn wait_listed_lifecycle(paths: &registry::DaemonPaths, sid: &SessionId, want: SessionLifecycle, for_: Duration) -> bool {
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

/// H3 (§5.4 `reload_preserves_parked_and_keep_warm`): reload-state carries
/// the session as it IS — `/model` mid-session, a keep-warm pin, a
/// non-persisted knob (`/context`), and a Parked session comes back Parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_preserves_model_keep_warm_settings_and_parked() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon_env(&guard, &url, &[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0")]).await;
    let pid_before = d.child.id();

    // Session A: turn, /model, /context, keep-warm → stays Live (pinned).
    let (mut a, snap) = attach_create(&d.paths, guard.home.path()).await;
    assert_eq!(snap.view.model, "claude-sonnet-4-5");
    let a_id = a.session_id().clone();
    one_turn(&mut a, "hello").await;
    a.send_from_self(SessionCommand::Set { id: 1, setting: SessionSetting::Model { model: "claude-opus-4-6".into() } }).await.unwrap();
    a.send_from_self(SessionCommand::Set { id: 2, setting: SessionSetting::ContextWindow { tokens: Some(1_000_000) } }).await.unwrap();
    a.send_from_self(SessionCommand::Set { id: 3, setting: SessionSetting::SystemPrompt { text: "reload-me".into() } }).await.unwrap();
    a.send_from_self(SessionCommand::KeepWarm { on: true }).await.unwrap();
    let mut seen = 0;
    while seen < 3 {
        let env = tokio::time::timeout(Duration::from_secs(10), a.next_event()).await.unwrap().unwrap();
        if let SessionEventWire::SettingChanged(_) = env.event {
            seen += 1;
        }
    }
    a.detach().await;

    // Session B: one turn, detach → parks (grace 0).
    let (mut b, _) = attach_create(&d.paths, guard.home.path()).await;
    let b_id = b.session_id().clone();
    one_turn(&mut b, "park me").await;
    b.detach().await;
    assert!(wait_listed_lifecycle(&d.paths, &b_id, SessionLifecycle::Parked, Duration::from_secs(10)).await, "B parks");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(wait_listed_lifecycle(&d.paths, &a_id, SessionLifecycle::Live, Duration::from_secs(1)).await, "A pinned stays Live");

    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
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
    assert_eq!(registry::read_daemon_json(&d.paths).unwrap().pid, pid_before);

    // B came back Parked; A came back Live with the pin.
    assert!(wait_listed_lifecycle(&d.paths, &b_id, SessionLifecycle::Parked, Duration::from_secs(10)).await, "B rehydrated Parked");
    assert!(wait_listed_lifecycle(&d.paths, &a_id, SessionLifecycle::Live, Duration::from_secs(1)).await, "A rehydrated Live");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(wait_listed_lifecycle(&d.paths, &a_id, SessionLifecycle::Live, Duration::from_secs(1)).await, "A keep-warm survived (not parked after grace)");

    // A: model/context/system survived.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut a2, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: a_id.clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap.view.model, "claude-opus-4-6", "/model survived reload");
    assert_eq!(snap.view.context_window, 1_000_000, "settings_replay survived reload");
    assert_eq!(snap.view.system_prompt.as_deref(), Some("reload-me"), "/system survived reload");
    assert_eq!(snap.conversation.api_messages.len(), 2);
    a2.detach().await;

    // B: attach unparks with its history.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut b2, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: b_id.clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap.conversation.api_messages.len(), 2, "parked session's history survived reload");
    b2.detach().await;

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}
