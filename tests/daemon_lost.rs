//! F9 — daemon SIGKILL while a client is attached: the client must print a
//! visible "daemon connection lost" line and exit non-zero within the
//! reconnect budget.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::process::{Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::{registry, EXIT_DAEMON_LOST};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use phase2::*;
use serial_test::serial;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// Spawn `synaps daemon --foreground` and wait for the socket to answer Ping.
async fn spawn_daemon(guard: &HomeGuard, url: &str) -> (std::process::Child, registry::DaemonPaths) {
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
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&paths.sock).await.is_ok() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "daemon never answered"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (child, paths)
}

/// F9: SIGKILL the daemon while a line client (`synaps attach`) is attached.
/// The client must print the lost-daemon message and exit with EXIT_DAEMON_LOST.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn daemon_sigkill_line_client_exits_nonzero() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let (mut daemon, paths) = spawn_daemon(&guard, &url).await;

    // Create a session so `synaps attach` has something to attach to.
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();
    let session_id = t.session_id().clone();
    t.detach().await;

    // Launch the line client `synaps attach <session_id>` with a short reconnect
    // budget so the test completes quickly.
    let mut client = Command::new(bin())
        .args(["attach", session_id.as_str()])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", guard.base_dir().join("run"))
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", &url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env("SYNAPS_TUI_ATTACH_RECONNECT_SECS", "3")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::piped())  // keep stdin open so the client blocks on read_line
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attach client");

    // Wait for the client to attach (the "[attached …] ○ ready" line).
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Take the stdin handle — we keep it alive so the client doesn't exit
    // on stdin EOF. We'll drop it after killing the daemon.
    let client_stdin = client.stdin.take();

    // SIGKILL the daemon.
    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();

    // Give the client a moment to detect the dead socket.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Now drop stdin — the client should already be in the reconnect loop.
    drop(client_stdin);

    // Wait for the client to exit (budget = 3s, plus margin).
    let output = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::task::spawn_blocking(move || client.wait_with_output().expect("client output"))
            .await
            .unwrap()
    })
    .await
    .expect("client should exit within budget");

    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The client must exit with EXIT_DAEMON_LOST (4).
    assert_eq!(
        code, EXIT_DAEMON_LOST,
        "expected exit code {EXIT_DAEMON_LOST}, got {code}\nstdout: {stdout}\nstderr: {stderr}"
    );

    // stderr must mention the daemon loss.
    assert!(
        stderr.contains("lost the daemon"),
        "stderr should say 'lost the daemon': {stderr}"
    );
    assert!(
        stderr.contains(session_id.as_str()),
        "stderr should mention session id: {stderr}"
    );

    // stdout should contain the reconnection attempt messages.
    assert!(
        stdout.contains("daemon connection lost"),
        "stdout should show 'daemon connection lost': {stdout}"
    );
}

/// F9: SocketTransport-level test: SIGKILL daemon, reconnect_once fails,
/// transport reports is_reload_pending() == false (crash, not reload).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn daemon_sigkill_socket_transport_reports_crash() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let (mut daemon, paths) = spawn_daemon(&guard, &url).await;

    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (mut t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                env: None,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();

    // Consume the ClientJoined event.
    let _ = tokio::time::timeout(Duration::from_secs(5), t.next_event()).await;

    // SIGKILL.
    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();

    // next_event should return None (EOF).
    let ev = tokio::time::timeout(Duration::from_secs(5), t.next_event())
        .await
        .expect("should return quickly");
    assert!(ev.is_none(), "expected None after daemon death, got {ev:?}");

    // Crash, not reload.
    assert!(
        !t.is_reload_pending(),
        "should NOT be a reload — it was a crash"
    );

    // reconnect_once should fail (nobody is listening).
    let r = t.reconnect_once(AttachMode::Mirror).await;
    assert!(r.is_err(), "reconnect_once should fail: {r:?}");
}

/// Unit test: EXIT_DAEMON_LOST is 4 (not colliding with existing codes).
#[test]
fn exit_daemon_lost_is_distinct() {
    assert_eq!(EXIT_DAEMON_LOST, 4);
    assert_ne!(EXIT_DAEMON_LOST, agent_engine::daemon::EXIT_REFUSED);
    assert_ne!(EXIT_DAEMON_LOST, agent_engine::daemon::EXIT_VERSION);
}

/// Unit test: daemon_lost stderr message format.
#[test]
fn daemon_lost_message_format() {
    let msg = format!(
        "synaps: lost the daemon (pid {}) and could not reconnect within {} s \
         — session {}; resume with synaps --attach {} / --continue {}",
        42, 60, "20260919-123456-abcd", "20260919-123456-abcd", "20260919-123456-abcd",
    );
    assert!(msg.contains("lost the daemon (pid 42)"));
    assert!(msg.contains("60 s"));
    assert!(msg.contains("20260919-123456-abcd"));
    assert!(msg.contains("--attach"));
    assert!(msg.contains("--continue"));
}
