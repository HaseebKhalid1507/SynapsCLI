//! Pinned sessions across daemon restarts (`daemon/pins.rs`): the keeper
//! persists the pin set to `<config dir>/daemon-pins.json`, a fresh start
//! restores it, ends the user chose (unpin, `/close`, `daemon close`) drop
//! it, and a restore that keeps failing is dropped after three attempts.
//! Real host, real UDS, loopback Anthropic stub; the crash and reload cases
//! run the daemon as a subprocess.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::pins::{self, PinEntry, PinFile, RestoreState};
use agent_engine::daemon::{registry, Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

const MODEL: &str = "claude-sonnet-4-5";

// ── in-process daemon ────────────────────────────────────────────────────────

async fn start(guard: &HomeGuard, idle_exit: Option<Duration>) -> Daemon {
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(
        host,
        DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), idle_exit, ..Default::default() },
    )
    .await
    .expect("daemon start")
}

async fn stub() {
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "never");
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
}

async fn stop(d: Daemon) {
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), d.wait()).await.expect("daemon wait");
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), t.next_event())
            .await
            .expect("timely")
            .expect("open");
        if pred(&e.event) {
            return;
        }
    }
}

/// A persisted session over the socket; this client owns input.
async fn create(sock: &Path, cwd: &Path) -> (SocketTransport, SessionId) {
    let conn = SocketTransport::connect(sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.to_path_buf()),
                env: None,
                model_override: Some(MODEL.into()),
                persist: true,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");
    let sid = t.session_id().clone();
    (t, sid)
}

async fn run_turn(t: &mut SocketTransport) {
    t.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

async fn listed(sock: &Path, sid: &SessionId) -> Option<SessionMeta> {
    SocketTransport::sessions(sock).await.unwrap_or_default().into_iter().find(|m| &m.id == sid)
}

async fn wait_listed(sock: &Path, sid: &SessionId, budget: Duration) -> Option<SessionMeta> {
    let t0 = std::time::Instant::now();
    loop {
        if let Some(m) = listed(sock, sid).await {
            return Some(m);
        }
        if t0.elapsed() > budget {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn pins_path(guard: &HomeGuard) -> PathBuf {
    guard.base_dir().join(pins::PINS_FILE_NAME)
}

fn read_pins(guard: &HomeGuard) -> PinFile {
    pins::load(&pins_path(guard)).unwrap_or_default()
}

fn pin_ids(f: &PinFile) -> Vec<String> {
    let mut ids: Vec<String> = f.pins.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    ids
}

/// Poll the pins file until `pred` holds (the keeper writes on change and
/// every 2 s).
async fn wait_pins(guard: &HomeGuard, pred: impl Fn(&PinFile) -> bool) -> PinFile {
    let t0 = std::time::Instant::now();
    loop {
        let f = read_pins(guard);
        if pred(&f) {
            return f;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "pins file never matched: {f:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_restored(d: &Daemon) {
    let t0 = std::time::Instant::now();
    while d.state.restoring.load(Ordering::SeqCst) {
        assert!(t0.elapsed() < Duration::from_secs(30), "restore never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn seed_pins(guard: &HomeGuard, pins: Vec<PinEntry>) {
    let file = PinFile { version: pins::PINS_FILE_VERSION, pins, ..PinFile::default() };
    std::fs::write(pins_path(guard), serde_json::to_vec_pretty(&file).unwrap()).unwrap();
}

fn seeded(id: &str, cwd: &Path, has_journal: bool, attempts: u32) -> PinEntry {
    PinEntry {
        id: id.into(),
        journal_id: id.into(),
        has_journal,
        config: SessionConfig {
            continue_session: Some(Some(id.into())),
            cwd: Some(cwd.to_path_buf()),
            model_override: Some(MODEL.into()),
            keep_warm: true,
            ..Default::default()
        },
        settings_replay: Vec::new(),
        cache_keepwarm: None,
        workers_running: Vec::new(),
        restore: RestoreState { attempts, ..Default::default() },
    }
}

/// `synaps daemon <args>` against the daemon under `guard`: (exit, stdout).
async fn cli(guard: &HomeGuard, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.arg("daemon")
        .args(args)
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", guard.base_dir().join("run"))
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null());
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run synaps daemon")).await.unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn stop_then_start_restores_the_pinned_session_with_its_history_model_and_settings() {
    let guard = HomeGuard::new();
    stub().await;
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();

    let (mut a, a_id) = create(&sock, guard.home.path()).await;
    run_turn(&mut a).await;
    a.send_from_self(SessionCommand::Set { id: 1, setting: SessionSetting::ApiRetries { n: 7 } }).await.unwrap();
    a.send_from_self(SessionCommand::Set { id: 2, setting: SessionSetting::SystemPrompt { text: "restore-me".into() } })
        .await
        .unwrap();
    let mut seen = 0;
    while seen < 2 {
        let env = tokio::time::timeout(Duration::from_secs(10), a.next_event()).await.unwrap().unwrap();
        if let SessionEventWire::SettingChanged(_) = env.event {
            seen += 1;
        }
    }
    assert!(SocketTransport::pin(&sock, &a_id, true).await.expect("pinned"));
    a.detach().await;
    let (mut b, b_id) = create(&sock, guard.home.path()).await;
    run_turn(&mut b).await;
    b.detach().await;

    let f = wait_pins(&guard, |f| pin_ids(f) == [a_id.as_str()]).await;
    assert!(!f.clean_shutdown, "written while serving");
    stop(d).await;
    let f = read_pins(&guard);
    assert_eq!(pin_ids(&f), [a_id.as_str()], "stop keeps the pin");
    assert!(f.clean_shutdown);
    assert!(f.pins[0].config.env.is_none(), "env is never persisted");
    assert!(
        f.pins[0].settings_replay.iter().any(|s| matches!(s, SessionSetting::ApiRetries { n: 7 })),
        "{:?}",
        f.pins[0].settings_replay
    );

    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();
    let m = wait_listed(&sock, &a_id, Duration::from_secs(20)).await.expect("A restored under its id");
    assert!(m.pinned, "and pinned: {m:#?}");
    assert_eq!(m.lifecycle, SessionLifecycle::Live);
    wait_restored(&d).await;
    assert!(listed(&sock, &b_id).await.is_none(), "B was not pinned");

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (a2, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: a_id.clone(), mode: AttachMode::Mirror })
        .await
        .unwrap();
    assert_eq!(snap.conversation.api_messages.len(), 2, "history intact");
    assert_eq!(snap.view.model, MODEL);
    assert_eq!(snap.view.api_retries, 7, "settings_replay re-applied");
    assert_eq!(snap.view.system_prompt.as_deref(), Some("restore-me"));
    a2.detach().await;

    stop(d).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn unpin_close_and_daemon_close_drop_the_pin() {
    let guard = HomeGuard::new();
    stub().await;
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();

    let (mut t1, s1) = create(&sock, guard.home.path()).await;
    let (mut t2, s2) = create(&sock, guard.home.path()).await;
    let (mut t3, s3) = create(&sock, guard.home.path()).await;
    for (t, sid) in [(&mut t1, &s1), (&mut t2, &s2), (&mut t3, &s3)] {
        run_turn(t).await;
        assert!(SocketTransport::pin(&sock, sid, true).await.expect("pinned"));
    }
    let mut all = vec![s1.to_string(), s2.to_string(), s3.to_string()];
    all.sort();
    wait_pins(&guard, |f| pin_ids(f) == all).await;

    // `/pin off`, `/close`, `daemon close`.
    assert!(!SocketTransport::pin(&sock, &s1, false).await.expect("unpinned"));
    t2.send_from_self(SessionCommand::EngineCommand { id: 9, name: "close".into(), arg: String::new() })
        .await
        .unwrap();
    until(&mut t2, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    t3.detach().await;
    SocketTransport::close(&sock, &s3, false).await.expect("daemon close");
    wait_pins(&guard, |f| f.pins.is_empty()).await;
    t1.detach().await;

    stop(d).await;
    assert!(read_pins(&guard).pins.is_empty(), "nothing to restore");
    let d = start(&guard, None).await;
    wait_restored(&d).await;
    let listing = SocketTransport::sessions(&d.paths.sock).await.unwrap();
    assert!(listing.is_empty(), "nothing restored: {listing:#?}");
    stop(d).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failing_restore_is_dropped_after_three_attempts_and_listed_by_daemon_sessions() {
    let guard = HomeGuard::new();
    stub().await;
    let home = guard.home.path();
    let sessions = guard.base_dir().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for id in ["corrupt", "crashed"] {
        std::fs::write(sessions.join(format!("{id}.json")), b"{ not a session").unwrap();
    }
    seed_pins(
        &guard,
        vec![
            seeded("gone-cwd", &home.join("no-such-dir"), false, 0),
            seeded("corrupt", home, true, 0),
            // The previous daemon died in its second attempt (abort): this
            // start's failure is the third.
            seeded("crashed", home, true, 2),
            // It died in its third: no attempt is made.
            seeded("crashed-out", home, true, 3),
        ],
    );

    let attempts = |f: &PinFile, id: &str| f.pins.iter().find(|p| p.id == id).map(|p| p.restore.attempts);
    let dropped = |f: &PinFile| {
        let mut ids: Vec<String> = f.dropped.iter().map(|d| d.id.clone()).collect();
        ids.sort();
        ids
    };

    // Start 1: the cwd entry and both crashed ones are dropped; `corrupt`
    // failed once. The daemon is healthy.
    let d = start(&guard, None).await;
    wait_restored(&d).await;
    SocketTransport::ping(&d.paths.sock).await.expect("daemon healthy");
    let f = wait_pins(&guard, |f| f.dropped.len() == 3).await;
    assert_eq!(pin_ids(&f), ["corrupt"]);
    assert_eq!(attempts(&f, "corrupt"), Some(1));
    assert!(f.pins[0].restore.last_error.is_some());
    assert_eq!(dropped(&f), ["crashed", "crashed-out", "gone-cwd"]);
    let reason = |id: &str| f.dropped.iter().find(|d| d.id == id).map(|d| d.reason.clone()).unwrap_or_default();
    assert!(reason("gone-cwd").contains("no longer exists"), "{}", reason("gone-cwd"));
    assert!(reason("crashed").contains("restore failed 3 times"), "{}", reason("crashed"));
    assert!(reason("crashed-out").contains("died while restoring"), "{}", reason("crashed-out"));
    let (code, text) = cli(&guard, &["sessions"]).await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("pin dropped: gone-cwd (cwd "), "{text}");
    stop(d).await;

    // Starts 2 and 3: the third failure drops it.
    let d = start(&guard, None).await;
    wait_restored(&d).await;
    stop(d).await;
    assert_eq!(attempts(&read_pins(&guard), "corrupt"), Some(2));
    let d = start(&guard, None).await;
    wait_restored(&d).await;
    stop(d).await;
    let f = read_pins(&guard);
    assert!(f.pins.is_empty(), "{f:#?}");
    assert!(dropped(&f).contains(&"corrupt".to_string()));

    // Start 4: nothing left to attempt.
    let d = start(&guard, None).await;
    assert!(!d.state.restoring.load(Ordering::SeqCst), "no restore task at all");
    stop(d).await;
    assert!(read_pins(&guard).pins.is_empty());
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_journal_held_elsewhere_restores_as_a_placeholder_and_comes_back_pinned_once_released() {
    let guard = HomeGuard::new();
    stub().await;
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"));
    t.detach().await;
    stop(d).await;
    assert_eq!(pin_ids(&read_pins(&guard)), [sid.as_str()]);

    let lock = hold_journal(&sid).await;

    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();
    wait_restored(&d).await;
    let m = listed(&sock, &sid).await.expect("a placeholder stands in");
    assert!(m.locked_by.is_some(), "{m:?}");
    assert!(m.pinned, "the placeholder of a pinned session is listed as pinned: {m:?}");
    let f = read_pins(&guard);
    assert_eq!(pin_ids(&f), [sid.as_str()], "the entry is kept");
    assert_eq!(f.pins[0].restore.attempts, 0, "a held journal is not an attempt");

    drop(lock);
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .expect("attach after the holder let go");
    assert_eq!(snap.conversation.api_messages.len(), 2);
    t.detach().await;
    let m = listed(&sock, &sid).await.expect("listed");
    assert!(m.locked_by.is_none() && m.pinned, "{m:?}");

    stop(d).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

/// Another process holds the journal (flock conflicts across open files,
/// even within one process). Retried briefly: the stopped daemon's actor
/// may still be letting go of it.
async fn hold_journal(sid: &SessionId) -> agent_core::session_lock::SessionLock {
    let t0 = std::time::Instant::now();
    loop {
        let holder = agent_core::session_lock::LockHolder { pid: std::process::id(), kind: "tui".into() };
        match agent_core::session_lock::SessionLock::try_acquire(
            &agent_core::session_lock::sessions_dir(),
            sid.as_str(),
            holder,
        ) {
            Ok(lock) => return lock,
            Err(e) => assert!(t0.elapsed() < Duration::from_secs(5), "test could not take the journal lock: {e:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A pinned session usually has nobody to attach and bring it back: once
/// the other process lets go of its journal, the daemon restores it by
/// itself (the keeper retries the lock every `PLACEHOLDER_RETRY`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_held_pin_comes_back_by_itself_once_the_holder_lets_go() {
    let guard = HomeGuard::new();
    stub().await;
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();
    let (mut t, sid) = create(&sock, guard.home.path()).await;
    run_turn(&mut t).await;
    assert!(SocketTransport::pin(&sock, &sid, true).await.expect("pinned"));
    t.detach().await;
    stop(d).await;

    let lock = hold_journal(&sid).await;
    let d = start(&guard, None).await;
    let sock = d.paths.sock.clone();
    wait_restored(&d).await;
    assert!(listed(&sock, &sid).await.is_some_and(|m| m.locked_by.is_some()), "a placeholder stands in");
    drop(lock);

    let t0 = std::time::Instant::now();
    let m = loop {
        if let Some(m) = listed(&sock, &sid).await.filter(|m| m.locked_by.is_none()) {
            break m;
        }
        assert!(t0.elapsed() < pins::PLACEHOLDER_RETRY * 2, "never restored without an attach");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(m.pinned && m.lifecycle == SessionLifecycle::Live, "{m:?}");
    assert_eq!(m.clients, 0, "nobody attached");
    let f = wait_pins(&guard, |f| pin_ids(f) == [sid.as_str()]).await;
    assert_eq!(f.pins[0].restore.attempts, 0, "a held journal never counted as an attempt");

    stop(d).await;
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_pin_holds_the_idle_exit() {
    let guard = HomeGuard::new();
    stub().await;
    // Never journaled (no turn): restored as a fresh pinned session.
    seed_pins(&guard, vec![seeded("never-journaled", guard.home.path(), false, 0)]);
    let d = start(&guard, Some(Duration::from_secs(1))).await;
    let sock = d.paths.sock.clone();
    wait_restored(&d).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!d.state.shutdown.is_cancelled(), "the restored pin holds the idle exit");
    let listing = SocketTransport::sessions(&sock).await.unwrap();
    assert_eq!(listing.len(), 1, "{listing:#?}");
    assert!(listing[0].pinned);

    assert!(!SocketTransport::pin(&sock, &listing[0].id, false).await.expect("unpinned"));
    tokio::time::timeout(Duration::from_secs(15), d.wait()).await.expect("unpinned, the daemon idle-exits");
    // Idle exit keeps pins; this one was unpinned first.
    assert!(read_pins(&guard).pins.is_empty());
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
}

// ── subprocess daemon ────────────────────────────────────────────────────────

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

async fn spawn_daemon(guard: &HomeGuard, url: &str, extra: &[(&str, &str)]) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env("SYNAPS_DAEMON_PARK_GRACE_SECS", "never")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .env_remove(pins::RESTORE_PINNED_ENV)
        // After the removals, so a test's own setting wins.
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

/// A graceful stop of a subprocess daemon, bounded.
async fn stop_proc(mut d: DaemonProc) {
    SocketTransport::shutdown(&d.paths.sock, false).await.expect("shutdown");
    let t0 = std::time::Instant::now();
    while d.child.try_wait().ok().flatten().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(30), "daemon did not stop");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn create_pinned_with_a_turn(paths: &registry::DaemonPaths, cwd: &Path) -> SessionId {
    let (mut t, sid) = create(&paths.sock, cwd).await;
    run_turn(&mut t).await;
    assert!(SocketTransport::pin(&paths.sock, &sid, true).await.expect("pinned"));
    t.detach().await;
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_sigkilled_daemon_restores_its_pinned_session_on_the_next_start() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let mut d = spawn_daemon(&guard, &url, &[]).await;
    let sid = create_pinned_with_a_turn(&d.paths, guard.home.path()).await;
    let f = wait_pins(&guard, |f| pin_ids(f) == [sid.as_str()]).await;
    assert!(!f.clean_shutdown);

    d.child.kill().expect("SIGKILL");
    let _ = d.child.wait();
    drop(d);

    let d = spawn_daemon(&guard, &url, &[]).await;
    let m = wait_listed(&d.paths.sock, &sid, Duration::from_secs(20)).await.expect("restored after the crash");
    assert!(m.pinned);
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (t, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })
        .await
        .unwrap();
    assert_eq!(snap.conversation.api_messages.len(), 2, "history intact");
    t.detach().await;
    stop_proc(d).await;
    assert_eq!(pin_ids(&read_pins(&guard)), [sid.as_str()], "and still pinned for the next start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_does_not_restore_twice_and_the_opt_out_keeps_the_pins() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url, &[]).await;
    let sid = create_pinned_with_a_turn(&d.paths, guard.home.path()).await;
    wait_pins(&guard, |f| pin_ids(f) == [sid.as_str()]).await;

    SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
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
    let m = wait_listed(&d.paths.sock, &sid, Duration::from_secs(10)).await.expect("rehydrated");
    assert!(m.pinned);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let listing = SocketTransport::sessions(&d.paths.sock).await.unwrap();
    assert_eq!(listing.len(), 1, "exactly one session after the reload: {listing:#?}");
    stop_proc(d).await;

    // Opted out: nothing restored, and the file still lists it.
    let d = spawn_daemon(&guard, &url, &[(pins::RESTORE_PINNED_ENV, "0")]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let listing = SocketTransport::sessions(&d.paths.sock).await.unwrap();
    assert!(listing.is_empty(), "{listing:#?}");
    stop_proc(d).await;
    assert_eq!(pin_ids(&read_pins(&guard)), [sid.as_str()], "the opt-out carried it");

    let d = spawn_daemon(&guard, &url, &[]).await;
    let m = wait_listed(&d.paths.sock, &sid, Duration::from_secs(20)).await.expect("restored");
    assert!(m.pinned);
    stop_proc(d).await;
}
