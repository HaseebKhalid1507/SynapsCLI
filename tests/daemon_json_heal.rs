//! `daemon.json` self-heal against a REAL daemon process. The file can be
//! lost while the daemon runs (an older `synaps` binary that lists sessions
//! unlinks every run-dir `*.json` it cannot parse as a session card). The
//! daemon that holds the flock rewrites it from the record it holds in
//! memory when a Ping finds it missing or naming another pid, so
//! `daemon status` and `daemon reload` keep reporting the daemon's version.
//!
//! Unix only (flock + execv).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::binary_version;
use phase2::*;
use serial_test::serial;

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
    run: PathBuf,
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

/// The environment every `synaps` process of one test shares.
fn synaps_env(cmd: &mut Command, guard: &HomeGuard, run: &Path) {
    cmd.env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD");
}

/// A private copy of the daemon binary, mode 0755. `reload` refuses a
/// group- or world-writable exe, and cargo links `bin()` under the runner's
/// umask (0775 under 0002), so a daemon that reloads into its own binary
/// starts from this copy: no test depends on the runner's umask.
fn private_exe(guard: &HomeGuard) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = guard.home.path().join("synaps-under-test");
    std::fs::copy(bin(), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    spawn_daemon_from(guard, url, Path::new(bin())).await
}

async fn spawn_daemon_from(guard: &HomeGuard, url: &str, exe: &Path) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "--foreground"]);
    synaps_env(&mut cmd, guard, &run);
    let child = cmd
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths, run };
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

/// `synaps daemon <args> --json` as a separate process (a client, never the
/// lock holder); its stdout parsed as JSON.
fn cli_json(guard: &HomeGuard, d: &DaemonProc, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(bin());
    cmd.arg("daemon").args(args).arg("--json");
    synaps_env(&mut cmd, guard, &d.run);
    let out = cmd.stdin(Stdio::null()).output().expect("run synaps daemon");
    assert!(out.status.success(), "synaps daemon {args:?}: {:?} {}", out.status, String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

/// The daemon log's heal lines.
fn heal_log_lines(guard: &HomeGuard) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(guard.base_dir()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("synaps.log") {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            lines.extend(text.lines().filter(|l| l.contains("daemon.json rewritten")).map(str::to_string));
        }
    }
    lines
}

async fn shutdown(d: &DaemonProc) {
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A deleted `daemon.json` comes back on the next Ping with exactly the
/// record the daemon wrote at start (not re-derived), so `daemon status`
/// reports the version. One INFO line, rate-limited: a second loss right
/// after is healed too but not logged again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_deleted_daemon_json_is_rewritten_when_status_pings() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let recorded = registry::read_daemon_json(&d.paths).expect("daemon.json at start");
    assert_eq!(recorded.pid, d.child.id());

    std::fs::remove_file(&d.paths.json).unwrap();
    let status = cli_json(&guard, &d, &["status"]);
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["daemon_version"], binary_version(), "{status}");
    assert_eq!(status["protocol_version"], recorded.protocol_version, "{status}");
    assert_eq!(registry::read_daemon_json(&d.paths), Some(recorded.clone()), "the record from memory, not re-derived");

    std::fs::remove_file(&d.paths.json).unwrap();
    SocketTransport::ping(&d.paths.sock).await.unwrap();
    assert_eq!(registry::read_daemon_json(&d.paths), Some(recorded.clone()), "healed again");
    // A Ping that finds the file in place leaves it alone.
    let before = std::fs::metadata(&d.paths.json).unwrap();
    SocketTransport::ping(&d.paths.sock).await.unwrap();
    let after = std::fs::metadata(&d.paths.json).unwrap();
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!((before.ino(), before.mtime_nsec()), (after.ino(), after.mtime_nsec()), "no rewrite");
        assert_eq!(after.mode() & 0o777, 0o600);
    }

    shutdown(&d).await;
    let lines = heal_log_lines(&guard);
    assert_eq!(lines.len(), 1, "one INFO line within the rate limit: {lines:#?}");
    assert!(lines[0].contains("INFO"), "{}", lines[0]);
}

/// A `daemon.json` naming another pid (here: a stale file put back by hand)
/// is replaced by the daemon that holds the flock, and only by it: a client
/// process reads, never writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_daemon_json_naming_another_pid_is_replaced_by_the_lock_holder() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let recorded = registry::read_daemon_json(&d.paths).expect("daemon.json at start");

    let mut foreign = recorded.clone();
    foreign.pid = 1;
    foreign.daemon_version = "0.0.1".into();
    foreign.generation = 9;
    std::fs::write(&d.paths.json, serde_json::to_vec_pretty(&foreign).unwrap()).unwrap();
    // `daemon sessions` is a client that never pings: the file stays foreign.
    let _ = cli_json(&guard, &d, &["sessions"]);
    assert_eq!(registry::read_daemon_json(&d.paths), Some(foreign), "a client never rewrites");

    SocketTransport::ping(&d.paths.sock).await.unwrap();
    assert_eq!(registry::read_daemon_json(&d.paths), Some(recorded), "the lock holder put its record back");

    shutdown(&d).await;
}

/// `daemon reload` with `daemon.json` deleted beforehand still reports the
/// version it reloaded from and the one it reloaded to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_reports_the_previous_version_with_daemon_json_deleted() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon_from(&guard, &url, &private_exe(&guard)).await;
    let recorded = registry::read_daemon_json(&d.paths).expect("daemon.json at start");

    std::fs::remove_file(&d.paths.json).unwrap();
    let r = cli_json(&guard, &d, &["reload", "--now"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["generation"], 2, "{r}");
    assert_eq!(r["previous_version"], binary_version(), "{r}");
    assert_eq!(r["version"], binary_version(), "{r}");
    let after = registry::read_daemon_json(&d.paths).expect("daemon.json after the reload");
    assert_eq!((after.pid, after.generation), (recorded.pid, 2));
    assert_eq!(
        after.exe.as_deref().map(|p| p.canonicalize().unwrap()),
        recorded.exe.as_deref().map(|p| p.canonicalize().unwrap()),
        "the recorded exe, from memory"
    );

    shutdown(&d).await;
}

/// `daemon start --detach` against a running daemon whose `daemon.json` was
/// deleted names the running daemon's pid (the CLI pings first, so the
/// daemon has put the file back), not "pid ?". `daemon stop` reads the pid
/// for its kill fallback the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn start_detach_names_the_running_daemon_with_daemon_json_deleted() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;

    std::fs::remove_file(&d.paths.json).unwrap();
    let mut cmd = Command::new(bin());
    cmd.args(["daemon", "start", "--detach"]);
    synaps_env(&mut cmd, &guard, &d.run);
    let out = cmd.stdin(Stdio::null()).output().expect("run synaps daemon start");
    assert!(out.status.success(), "{:?} {}", out.status, String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("daemon already running (pid {})", d.child.id())),
        "{stdout}"
    );

    shutdown(&d).await;
}

/// A script that answers `--print-version` with our version (it passes the
/// gate) and then strips its own exec bit, so the `execv` that follows fails
/// with EACCES: a deterministic exec failure after the gate.
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

/// A reload whose exec fails leaves the old image's record exactly as it
/// was, in memory and on disk, exe and started_at included (so the next
/// default reload uses the old exe, not the failed one). Deleting the file
/// afterwards heals it back to the same record, which shows memory too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_reload_exec_leaves_the_previous_record_in_memory_and_on_disk() {
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let d = spawn_daemon(&guard, &url).await;
    let recorded = registry::read_daemon_json(&d.paths).expect("daemon.json at start");

    let disarm = self_disarming_binary(guard.home.path());
    let r = SocketTransport::reload(&d.paths.sock, true, None, Some(disarm)).await;
    assert!(matches!(r, Ok(2)), "{r:?}");
    let rs_path = agent_engine::daemon::reload::reload_state_path(&d.paths);
    let t0 = std::time::Instant::now();
    while rs_path.exists() || SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the old image did not recover from the failed exec");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(registry::read_daemon_json(&d.paths), Some(recorded.clone()), "on disk: the previous record");
    std::fs::remove_file(&d.paths.json).unwrap();
    SocketTransport::ping(&d.paths.sock).await.unwrap();
    assert_eq!(registry::read_daemon_json(&d.paths), Some(recorded), "in memory: the healed record is the previous one");

    shutdown(&d).await;
}
