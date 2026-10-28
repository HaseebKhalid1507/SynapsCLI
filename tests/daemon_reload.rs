//! C3 — `synaps daemon reload` against a REAL daemon process (PLAN-phase3
//! §5.4): the `synaps` binary is spawned as `daemon --foreground`, a client
//! attaches over the socket and runs one scripted turn, `Reload{now}` is
//! sent on a control connection, the client sees `Reloading` + EOF and
//! `reconnect`s; the pid is unchanged, `daemon.json.generation == 2`, the
//! flock is still held, the conversation survived, a second turn works.
//! Plus: an OLDER binary (a shell script faking `--print-version`) is
//! refused up front and the daemon keeps serving.
//!
//! Unix: execv + flock — the pid is unchanged. Windows: the hand-off (the
//! next image queues the `daemon.lock` request behind the old one, which
//! then exits) — the pid changes, everything else holds.

#[path = "support/phase2/mod.rs"]
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
        // Windows: after a reload the daemon is the successor image, not
        // our child — end it too, or a failed test leaks it.
        #[cfg(windows)]
        if let Some(info) = registry::read_daemon_json(&self.paths) {
            if info.pid != self.child.id() && registry::is_alive(&self.paths) {
                let _ = Command::new("taskkill").args(["/PID", &info.pid.to_string(), "/T", "/F"]).output();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The pid a client sees after a reload: the same process on Unix
/// (`execv`), the successor image on Windows (the lock hand-off).
fn assert_reload_pid(after: u32, before: u32) {
    #[cfg(unix)]
    assert_eq!(after, before, "same pid after execv");
    #[cfg(windows)]
    assert_ne!(after, before, "the successor image took over");
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
                env: None,
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

#[cfg(unix)]
fn fake_older_binary(dir: &Path) -> PathBuf {
    let p = dir.join("synaps-old");
    std::fs::write(
        &p,
        "#!/bin/sh\nif [ \"$2\" = \"--print-version\" ]; then echo '{\"binary_version\":\"0.0.1\",\"protocol_version\":3}'; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Windows: the same fake as a `.cmd` (`%2` is the second argument).
#[cfg(windows)]
fn fake_older_binary(dir: &Path) -> PathBuf {
    let p = dir.join("synaps-old.cmd");
    std::fs::write(
        &p,
        "@echo off\r\nif \"%2\"==\"--print-version\" (echo {\"binary_version\":\"0.0.1\",\"protocol_version\":3}& exit /b 0)\r\nexit /b 1\r\n",
    )
    .unwrap();
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
    // The real daemon reports the build id the root build script baked in
    // (`main` hands it to the engine), and a client of the same build sees
    // no version-skew notice.
    assert_eq!(t.welcome.daemon_build, env!("SYNAPS_BUILD_ID"), "daemon reports its build id");
    let same = agent_engine::session::transport::BinaryId { version: binary_version(), build: env!("SYNAPS_BUILD_ID").into() };
    let daemon_id = agent_engine::session::transport::BinaryId::of(&t.welcome);
    assert_eq!(agent_engine::session::transport::version_skew_notice(&same, &daemon_id, "s"), None);
    if !daemon_id.build.is_empty() {
        let other = agent_engine::session::transport::BinaryId { build: "0000000".into(), ..same };
        assert!(agent_engine::session::transport::version_skew_notice(&other, &daemon_id, "s").is_some(), "another build of the same version is flagged");
    }
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
    while let Some(env) =
        tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.expect("announce")
    {
        if let SessionEventWire::Reloading { generation, retry_after_ms } = env.event {
            assert_eq!(generation, 2);
            assert!(retry_after_ms > 0);
            saw_reloading = true;
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
    assert_reload_pid(t.welcome.pid, pid_before);
    assert_eq!(t.session_id(), &sid, "session id survives (same journal)");
    assert_eq!(
        serde_json::to_string(&snap.conversation.api_messages).unwrap(),
        serde_json::to_string(&before).unwrap(),
        "conversation survived the reload"
    );
    assert_eq!(snap.input_owner, Some(t.client_id()), "was_owner → Takeover → owner again");

    let info = registry::read_daemon_json(&d.paths).unwrap();
    assert_eq!(info.pid, t.welcome.pid);
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
async fn reload_with_turn_in_flight_checkpoints_and_keeps_the_partial_turn() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();

    let (mut t, _snap) = attach_create(&d.paths, guard.home.path()).await;
    let sid = t.session_id().clone();
    t.send_from_self(SessionCommand::Submit { text: "never finishes".into(), attachments: vec![] }).await.unwrap();
    // Wait for the partial text so the checkpoint has partial work to keep.
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
    // re-query, and the connection is gone before the answer — so the kept
    // history itself is asserted from the reconnect snapshot, i.e. from the
    // journal), then Reloading, then EOF.
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
    assert_reload_pid(t.welcome.pid, pid_before);
    assert_eq!(t.session_id(), &sid);
    assert!(!snap.streaming, "the turn was checkpointed, not resumed");
    // The interrupted turn came back from the journal as REAL history: the
    // partial assistant reply, then the restart marker — no recap.
    assert!(snap.conversation.abort_context.is_none());
    let msgs = &snap.conversation.api_messages;
    assert_eq!(msgs.len(), 3, "{msgs:#?}");
    assert_eq!(msgs[0]["content"], "never finishes");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][0]["text"], "partial");
    assert_eq!(
        msgs[2]["content"],
        agent_engine::engine::interrupt::InterruptReason::Restart.marker()
    );

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

/// `(a.pinned, b.pinned)` as `daemon sessions` lists them.
async fn listed_pins(paths: &registry::DaemonPaths, a: &SessionId, b: &SessionId) -> (bool, bool) {
    let metas = SocketTransport::sessions(&paths.sock).await.expect("sessions");
    let pinned = |id: &SessionId| metas.iter().find(|m| &m.id == id).map(|m| m.pinned).expect("listed");
    (pinned(a), pinned(b))
}

/// H3 (§5.4 `reload_preserves_parked_and_keep_warm`): reload-state carries
/// the session as it IS — `/model` mid-session, a keep-warm pin, a
/// non-persisted knob (`/context`), and a Parked session comes back Parked.
/// A session restored Parked runs `on_session_start` only once it is
/// unparked, and once; a Live one runs it once after the restore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_preserves_model_keep_warm_settings_and_parked() {
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard);
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
    assert_eq!(listed_pins(&d.paths, &a_id, &b_id).await, (true, false), "the listing shows A's pin");

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
    assert_reload_pid(registry::read_daemon_json(&d.paths).unwrap().pid, pid_before);

    // B came back Parked; A came back Live with the pin.
    assert!(wait_listed_lifecycle(&d.paths, &b_id, SessionLifecycle::Parked, Duration::from_secs(10)).await, "B rehydrated Parked");
    assert!(wait_listed_lifecycle(&d.paths, &a_id, SessionLifecycle::Live, Duration::from_secs(1)).await, "A rehydrated Live");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(wait_listed_lifecycle(&d.paths, &a_id, SessionLifecycle::Live, Duration::from_secs(1)).await, "A keep-warm survived (not parked after grace)");
    assert_eq!(listed_pins(&d.paths, &a_id, &b_id).await, (true, false), "the listing shows the pin after reload");

    // Before the reload each session ran its hook once. After it: A (Live)
    // once more; B (Parked) not until it is unparked.
    assert!(wait_hook_count(&plugin, &a_id, 2, Duration::from_secs(10)).await, "A's hook after the restore");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(hook_count(&plugin, &b_id), 1, "B is parked: no hook since the reload");

    // A: model/context/system survived.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (a2, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: a_id.clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap.view.model, "claude-opus-4-6", "/model survived reload");
    assert_eq!(snap.view.context_window, 1_000_000, "settings_replay survived reload");
    assert_eq!(snap.view.system_prompt.as_deref(), Some("reload-me"), "/system survived reload");
    assert_eq!(snap.conversation.api_messages.len(), 2);
    a2.detach().await;

    // B: attach unparks with its history.
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (b2, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: b_id.clone(), mode: AttachMode::Mirror }).await.unwrap();
    assert_eq!(snap.conversation.api_messages.len(), 2, "parked session's history survived reload");
    // Unparked: its hook runs now, once.
    assert!(wait_hook_count(&plugin, &b_id, 2, Duration::from_secs(10)).await, "B's hook after the unpark");
    b2.detach().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(hook_count(&plugin, &b_id), 2, "B: one hook after the unpark, not more");
    assert_eq!(hook_count(&plugin, &a_id), 2, "A: one hook after the restore, not more");

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// A script that answers `--print-version` with OUR version (passes the
/// gate) and then strips its own exec bit, so the `execv` that follows
/// fails with EACCES: the one way past `validate_exe` + the probe.
#[cfg(unix)]
fn self_disarming_binary(dir: &Path) -> PathBuf {
    let p = dir.join("synaps-disarm");
    let pv = serde_json::to_string(&agent_engine::daemon::reload::PrintVersion::current()).unwrap();
    std::fs::write(
        &p,
        format!("#!/bin/sh\nif [ \"$2\" = \"--print-version\" ]; then echo '{pv}'; chmod 644 \"$0\"; exit 0; fi\nexit 1\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// M4 + M8: `--exe` is validated up front (non-executable / world-writable
/// / missing → refused, nothing disturbed); an `execv` that fails AFTER the
/// gate leaves the old image serving — generation unchanged, reload-state
/// gone, `reloading` cleared (a second reload is accepted), a turn works.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn exe_validation_refuses_and_exec_failure_keeps_serving() {
    use std::os::unix::fs::PermissionsExt;
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid = d.child.id();
    let (mut t, _) = attach_create(&d.paths, guard.home.path()).await;
    one_turn(&mut t, "before").await;

    // Refused up front: missing, non-executable, world-writable.
    let missing = guard.home.path().join("does-not-exist");
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(missing)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("does-not-exist")), "{r:?}");
    let noexec = fake_older_binary(guard.home.path());
    std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(noexec.clone())).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("not executable")), "{r:?}");
    std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o777)).unwrap();
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(noexec)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("writable")), "{r:?}");
    assert_eq!(registry::read_daemon_json(&d.paths).unwrap().generation, 1);

    // Past the gate: the requester is told Bye{Reloading} BEFORE the exec
    // (the reply is Ok(2)); the exec then fails and the old image keeps
    // serving — generation restored, reload-state removed, socket alive.
    let disarm = self_disarming_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(disarm)).await;
    assert!(matches!(r, Ok(2)), "{r:?}");
    let t0 = std::time::Instant::now();
    loop {
        let info = registry::read_daemon_json(&d.paths).unwrap();
        if info.generation == 1
            && !agent_engine::daemon::reload::reload_state_path(&d.paths).exists()
            && SocketTransport::ping(&d.paths.sock).await.is_ok()
        {
            assert_eq!(info.pid, pid);
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "old image did not recover: {info:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(registry::is_alive(&d.paths));

    // The session was checkpointed (Aborted-free: it was idle) but is alive;
    // the mirror reconnects and a turn works.
    let snap = tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror)).await.unwrap().expect("reconnected");
    assert_eq!(snap.conversation.api_messages.len(), 2);
    let after = one_turn(&mut t, "after").await;
    assert_eq!(after.len(), 4);
    // `reloading` cleared: a second (refused) reload is judged on its merits.
    let old = fake_older_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(old)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("older")), "{r:?}");

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

// ── restore with a slow `on_session_start` hook ─────────────────────────────

/// Install `tests/fixtures/slow_session_start_extension.py` as a plugin under
/// the guard's base dir. Its knobs are files in the returned plugin dir
/// (`slow-ms`); the daemon clears an extension's environment.
fn install_slow_start_plugin(guard: &HomeGuard) -> PathBuf {
    let dir = guard.base_dir().join("plugins/slow-start");
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(dir.join("extensions")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slow_session_start_extension.py"),
        dir.join("extensions/slow_session_start_extension.py"),
    )
    .unwrap();
    std::fs::write(
        dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "slow-start",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/slow_session_start_extension.py"],
    "permissions": ["session.lifecycle"],
    "hooks": [{"hook": "on_session_start"}]
  }
}
"#,
    )
    .unwrap();
    dir
}

/// Create `n` sessions, each with one finished turn, and detach.
async fn sessions_with_one_turn(paths: &registry::DaemonPaths, cwd: &Path, n: usize) -> Vec<SessionId> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let (mut t, _) = attach_create(paths, cwd).await;
        one_turn(&mut t, &format!("before reload {i}")).await;
        ids.push(t.session_id().clone());
        t.detach().await;
    }
    ids
}

/// Wait for the new image (`generation`) to answer Ping; returns how long
/// that took after `t0`, or `None` past `budget`, including an answer that
/// arrived only after it. A bound but not yet accepting socket holds the
/// handshake, so every attempt is time-boxed.
async fn answered_within(paths: &registry::DaemonPaths, generation: u64, t0: std::time::Instant, budget: Duration) -> Option<Duration> {
    loop {
        if t0.elapsed() > budget {
            return None;
        }
        if registry::read_daemon_json(paths).is_some_and(|i| i.generation == generation)
            && tokio::time::timeout(Duration::from_millis(500), SocketTransport::ping(&paths.sock))
                .await
                .is_ok_and(|r| r.is_ok())
        {
            let took = t0.elapsed();
            return (took <= budget).then_some(took);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A reload with 30 sessions and an extension whose `on_session_start` takes
/// 3 s: the restore no longer waits on the hook, so the new image answers
/// within seconds (it took about 90 s, one hook after another) and every
/// session reattaches under its id. The bound includes the process restart
/// and the restore, so it is generous (20 s) yet far below 90 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_with_a_slow_session_start_hook_answers_within_seconds() {
    const SESSIONS: usize = 30;
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard);
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid_before = d.child.id();
    let ids = sessions_with_one_turn(&d.paths, guard.home.path(), SESSIONS).await;

    std::fs::write(plugin.join("slow-ms"), "3000").unwrap();
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    let t0 = std::time::Instant::now();
    let took = answered_within(&d.paths, 2, t0, Duration::from_secs(20)).await;
    assert!(took.is_some(), "the new image did not answer within 20 s of the reload");
    eprintln!("reload with {SESSIONS} slow hooks: answered after {took:?}");
    assert_eq!(registry::read_daemon_json(&d.paths).unwrap().pid, pid_before);

    let listed = SocketTransport::sessions(&d.paths.sock).await.expect("sessions");
    for id in &ids {
        assert!(listed.iter().any(|m| &m.id == id), "{id} listed after the reload");
    }
    for id in &ids {
        let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (t, snap) = tokio::time::timeout(
            Duration::from_secs(15),
            SocketTransport::attach(conn, Attach::Existing { session_id: id.clone(), mode: AttachMode::Mirror }),
        )
        .await
        .expect("attach budget")
        .expect("reattached");
        assert_eq!(t.session_id(), id, "same id, not a fresh session");
        assert_eq!(snap.conversation.api_messages.len(), 2, "{id}: history survived");
        t.detach().await;
    }

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// What the slow session-start fixture injects (tests/fixtures).
const SLOW_START_SENTINEL: &str = "SLOW-START-SENTINEL";

/// The request body whose messages carry `marker`, if the stub saw one.
fn body_with(bodies: &Bodies, marker: &str) -> Option<serde_json::Value> {
    let bodies = bodies.lock().unwrap_or_else(|p| p.into_inner());
    bodies
        .iter()
        .filter_map(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
        .find(|b| b["messages"].to_string().contains(marker))
}

/// How many `on_session_start` calls the slow-start fixture logged for `id`.
fn hook_count(plugin: &Path, id: &SessionId) -> usize {
    std::fs::read_to_string(plugin.join("session-start.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["session_id"].as_str() == Some(id.as_str()))
        .count()
}

async fn wait_hook_count(plugin: &Path, id: &SessionId, want: usize, for_: Duration) -> bool {
    let t0 = std::time::Instant::now();
    while hook_count(plugin, id) < want {
        if t0.elapsed() > for_ {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

/// The restored sessions' hooks run after the restore, but still one at a
/// time: 30 hooks at once would queue in the single extension process and
/// the tail would pass the 5 s handler timeout and lose its injection for
/// the whole session. Every restored session's first turn carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn every_restored_session_gets_its_session_start_injection() {
    const SESSIONS: usize = 30;
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard);
    let (url, _hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let ids = sessions_with_one_turn(&d.paths, guard.home.path(), SESSIONS).await;
    for i in 0..SESSIONS {
        let body = body_with(&bodies, &format!("before reload {i}")).expect("pre-reload request");
        assert!(body["system"].to_string().contains(SLOW_START_SENTINEL), "fixture wired: session {i}");
    }

    // 30 x 300 ms one after another fits every hook in the handler timeout;
    // 30 at once does not.
    std::fs::write(plugin.join("slow-ms"), "300").unwrap();
    let log = plugin.join("session-start.jsonl");
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    let t0 = std::time::Instant::now();
    assert!(answered_within(&d.paths, 2, t0, Duration::from_secs(10)).await.is_some(), "new image answers");
    // Let the background hooks finish before any turn (a turn would run its
    // own session's hook at once; this test is about the background wave).
    let lines = || std::fs::read_to_string(&log).map(|s| s.lines().count()).unwrap_or(0);
    while lines() < 2 * SESSIONS {
        assert!(t0.elapsed() < Duration::from_secs(60), "hooks after the reload: {}", lines() - SESSIONS);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for (i, id) in ids.iter().enumerate() {
        let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (mut t, _) = SocketTransport::attach(conn, Attach::Existing { session_id: id.clone(), mode: AttachMode::Mirror })
            .await
            .expect("reattached");
        one_turn(&mut t, &format!("after reload {i}")).await;
        t.detach().await;
        let body = body_with(&bodies, &format!("after reload {i}")).expect("post-reload request");
        assert!(
            body["system"].to_string().contains(SLOW_START_SENTINEL),
            "session {i} lost its session-start injection in the restore"
        );
    }

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// Every restored session's first turn starts right after the reload, all at
/// once, before the background hooks are through. Each turn waits for its
/// own session's hook, and those calls go through the same one-at-a-time
/// gate: 30 calls at once would queue in the single extension process and
/// the tail would pass the handler timeout. Every first request carries the
/// injection, and no session's hook runs twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn concurrent_first_turns_after_a_reload_keep_the_injection() {
    const SESSIONS: usize = 30;
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard);
    let (url, _hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let ids = sessions_with_one_turn(&d.paths, guard.home.path(), SESSIONS).await;

    std::fs::write(plugin.join("slow-ms"), "300").unwrap();
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    let t0 = std::time::Instant::now();
    assert!(answered_within(&d.paths, 2, t0, Duration::from_secs(20)).await.is_some(), "new image answers");

    let paths = &d.paths;
    let turns = ids.iter().enumerate().map(|(i, id)| async move {
        let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
        let (mut t, _) = SocketTransport::attach(conn, Attach::Existing { session_id: id.clone(), mode: AttachMode::Mirror })
            .await
            .expect("reattached");
        one_turn(&mut t, &format!("after reload {i}")).await;
        t.detach().await;
    });
    futures::future::join_all(turns).await;
    let missing: Vec<usize> = (0..SESSIONS)
        .filter(|i| {
            let body = body_with(&bodies, &format!("after reload {i}")).expect("post-reload request");
            !body["system"].to_string().contains(SLOW_START_SENTINEL)
        })
        .collect();
    assert!(missing.is_empty(), "first turns without the session-start injection: {missing:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let counts: Vec<usize> = ids.iter().map(|id| hook_count(&plugin, id)).collect();
    assert!(counts.iter().all(|&n| n == 2), "one hook before and one after the reload per session: {counts:?}");

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// The next event within `budget`, or `None`.
async fn event_within(t: &mut SocketTransport, budget: Duration) -> Option<SessionEventWire> {
    match tokio::time::timeout(budget, t.next_event()).await {
        Ok(Some(env)) => Some(env.event),
        _ => None,
    }
}

/// A first turn that waits for its session's hook does not make the session
/// unresponsive: Cancel ends that turn at once (nothing is sent) and the hook
/// still runs, so the next turn carries it; End ends the session at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_session_start_wait_yields_to_cancel_and_end() {
    const SESSIONS: usize = 3;
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard);
    let (url, _hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let ids = sessions_with_one_turn(&d.paths, guard.home.path(), SESSIONS).await;

    // Restored hooks take 3 s each, one at a time: a first turn now waits
    // seconds for its own session's hook.
    std::fs::write(plugin.join("slow-ms"), "3000").unwrap();
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    let t0 = std::time::Instant::now();
    assert!(answered_within(&d.paths, 2, t0, Duration::from_secs(20)).await.is_some(), "new image answers");
    let attach = |id: SessionId| {
        let sock = d.paths.sock.clone();
        async move {
            let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
            SocketTransport::attach(conn, Attach::Existing { session_id: id, mode: AttachMode::Mirror })
                .await
                .expect("reattached")
                .0
        }
    };

    // End while the first turn waits: the session ends at once.
    let (end_id, cancel_id) = (ids[1].clone(), ids[2].clone());
    let mut y = attach(end_id.clone()).await;
    y.send_from_self(SessionCommand::Submit { text: "end me".into(), attachments: vec![] }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let asked = std::time::Instant::now();
    y.send_from_self(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
    loop {
        let left = Duration::from_secs(10).saturating_sub(asked.elapsed());
        match event_within(&mut y, left).await {
            Some(SessionEventWire::Ended { .. }) => break,
            Some(SessionEventWire::Refused { reason, .. }) => panic!("End refused: {reason}"),
            Some(_) => {}
            None => panic!("no Ended within 10 s of the End"),
        }
    }
    let took = asked.elapsed();
    assert!(took < Duration::from_millis(1500), "End during the session-start wait took {took:?}");
    let listed = SocketTransport::sessions(&d.paths.sock).await.expect("sessions");
    assert!(!listed.iter().any(|m| m.id == end_id), "the ended session left the list");
    drop(y);

    // Cancel while the first turn waits: that turn ends at once, aborted.
    let mut x = attach(cancel_id.clone()).await;
    x.send_from_self(SessionCommand::Submit { text: "cancel me".into(), attachments: vec![] }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let asked = std::time::Instant::now();
    x.send_from_self(SessionCommand::Cancel).await.unwrap();
    let mut aborted = false;
    loop {
        let left = Duration::from_secs(10).saturating_sub(asked.elapsed());
        match event_within(&mut x, left).await {
            Some(SessionEventWire::Aborted { .. }) => aborted = true,
            Some(SessionEventWire::Idle) if aborted => break,
            Some(_) => {}
            None => panic!("no Aborted + Idle within 10 s of the Cancel"),
        }
    }
    let took = asked.elapsed();
    assert!(took < Duration::from_millis(1500), "Cancel during the session-start wait took {took:?}");
    assert!(body_with(&bodies, "cancel me").is_none(), "a canceled turn sent nothing");
    assert!(body_with(&bodies, "end me").is_none(), "an ended session's turn sent nothing");

    // The canceled turn's hook still runs; the next turn carries it.
    assert!(wait_hook_count(&plugin, &cancel_id, 2, Duration::from_secs(15)).await, "the hook ran after the cancel");
    one_turn(&mut x, "after the cancel").await;
    let body = body_with(&bodies, "after the cancel").expect("request after the cancel");
    assert!(body["system"].to_string().contains(SLOW_START_SENTINEL), "the next turn carries the injection");
    assert_eq!(hook_count(&plugin, &cancel_id), 2, "one hook after the reload, not more");
    x.detach().await;

    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}

/// `daemon rename` survives `daemon reload`: the name was saved durably, so
/// the rehydrated session lists it and the file still carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rename_survives_a_reload() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let (mut t, snap) = attach_create(&d.paths, guard.home.path()).await;
    let sid = t.session_id().clone();
    let journal = snap.meta.journal_id.clone();
    one_turn(&mut t, "hello").await;
    let r = SocketTransport::rename(&d.paths.sock, &sid, Some("reload-proof"), Some(None)).await.unwrap();
    assert!(r.is_ok(), "{r:?}");

    assert_eq!(SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload"), 2);
    tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");
    let m = SocketTransport::sessions(&d.paths.sock)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == sid)
        .expect("rehydrated");
    assert_eq!(m.name.as_deref(), Some("reload-proof"));
    let file = guard.base_dir().join("sessions").join(format!("{journal}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(v["name"], "reload-proof");
}

/// Windows: a `.cmd` that answers `--print-version` with OUR version (it
/// passes the gate), then — started as the next image — dies before it
/// takes `daemon.lock` over: the hand-off's failure path.
#[cfg(windows)]
fn dud_binary(dir: &Path) -> PathBuf {
    let p = dir.join("synaps-dud.cmd");
    let pv = serde_json::to_string(&agent_engine::daemon::reload::PrintVersion::current()).unwrap();
    std::fs::write(
        &p,
        format!(
            "@echo off\r\nif \"%2\"==\"--print-version\" (echo {pv}& exit /b 0)\r\necho synaps daemon: dud image 1>&2\r\nexit /b 1\r\n"
        ),
    )
    .unwrap();
    p
}

/// Windows twin of the exec-failure half of the test above: a next image
/// that dies before the hand-off leaves the old image serving — same pid,
/// generation restored, reload-state removed, `reloading` cleared (a second
/// reload is judged on its merits), the mirror reconnects and a turn works.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn successor_dying_before_the_handoff_keeps_serving() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let pid = d.child.id();
    let (mut t, _) = attach_create(&d.paths, guard.home.path()).await;
    one_turn(&mut t, "before").await;

    let missing = guard.home.path().join("does-not-exist");
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(missing)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("does-not-exist")), "{r:?}");

    let dud = dud_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(dud)).await;
    assert!(matches!(r, Ok(2)), "{r:?}");
    let t0 = std::time::Instant::now();
    loop {
        let info = registry::read_daemon_json(&d.paths).unwrap();
        if info.generation == 1
            && !agent_engine::daemon::reload::reload_state_path(&d.paths).exists()
            && SocketTransport::ping(&d.paths.sock).await.is_ok()
        {
            assert_eq!(info.pid, pid);
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "old image did not recover: {info:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(registry::is_alive(&d.paths));

    let snap = tokio::time::timeout(Duration::from_secs(15), t.reconnect(AttachMode::Mirror)).await.unwrap().expect("reconnected");
    assert_eq!(t.welcome.pid, pid, "still the old image");
    assert_eq!(snap.conversation.api_messages.len(), 2);
    let after = one_turn(&mut t, "after").await;
    assert_eq!(after.len(), 4);
    let old = fake_older_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(old)).await;
    assert!(matches!(r, Err(TransportError::Refused(ref m)) if m.contains("older")), "{r:?}");

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, true).await.unwrap();
}
