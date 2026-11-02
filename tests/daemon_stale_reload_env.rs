//! A FRESH `synaps daemon --foreground` whose environment still holds a
//! reload hand-off (`SYNAPS_DAEMON_RELOAD_STATE`, `SYNAPS_DAEMON_LOCK_FD`,
//! `SYNAPS_DAEMON_SUBAGENT_ID_FLOOR`) must start fresh, against REAL
//! daemon processes. The variables are a real reload only when the state
//! file parses AND the fd they name is open on this daemon's `daemon.lock`.
//! An environment copied from a reloaded daemon (`/proc/<pid>/environ`)
//! into a stop + start holds them with neither true. The new daemon then
//! logs one WARN, takes the flock itself, and restores what the stopped
//! daemon's record names. It never adopts, closes or changes the fd.
//!
//! - `a_fresh_start_whose_env_names_a_missing_reload_state_starts_fresh_and_restores`:
//!   the state file is gone and fd 29 is not the lock.
//! - `a_stale_reload_state_with_a_lock_fd_that_is_not_the_lock_starts_fresh_and_leaves_the_fd_alone`:
//!   a stale state file that parses, and fd 29 open on another file. The fd
//!   stays open on that file, still inheritable, and the state file is left
//!   on disk as it was.
//!
//! Linux only (`/proc/<pid>/fd`, flock).

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::{crash_record, registry, reload};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use axum::response::IntoResponse;
use phase2::*;
use serial_test::serial;

/// The fd number the stale environment names (the one the 17:02 start got).
const STALE_FD: i32 = 29;
/// What the new daemon logs (and writes to stderr) for a stale hand-off.
const STALE_LINE: &str = "stale reload hand-off in the environment";
/// The restore summary of a start that restored the stopped daemon's session.
const RESTORED_ONE: &str = "crash restore: restored 1 sessions, resume queued 0, given up 0, skipped 0";

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

fn run_dir(guard: &HomeGuard) -> PathBuf {
    guard.base_dir().join("run")
}

/// Every reply is a whole answer.
async fn spawn_provider() -> String {
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::OK,
            [("content-type", "text/event-stream")],
            ANTHROPIC_SSE.to_string(),
        )
            .into_response()
    });
    serve(app).await
}

/// `synaps daemon --foreground` under the guard's home. With `stale`, the
/// hand-off variables are set as given; without, they are removed. `wrap`
/// runs the daemon through `bash -c` (to open an inherited fd first).
fn daemon_command(guard: &HomeGuard, url: &str, stale: &[(&str, &str)], wrap: Option<&str>) -> Command {
    let run = run_dir(guard);
    std::fs::create_dir_all(&run).unwrap();
    let mut cmd = match wrap {
        Some(script) => {
            let mut c = Command::new("bash");
            c.args(["-c", script]).env("SYNAPS_TEST_BIN", bin());
            c
        }
        None => {
            let mut c = Command::new(bin());
            c.args(["daemon", "--foreground"]);
            c
        }
    };
    cmd.env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove(reload::RELOAD_STATE_ENV)
        .env_remove(reload::LOCK_FD_ENV)
        .env_remove(reload::SUBAGENT_ID_FLOOR_ENV)
        .envs(stale.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    cmd
}

/// Spawn and wait until the daemon answers a ping. A daemon that exits
/// first fails the test at once, with its stderr.
async fn start(mut cmd: Command, guard: &HomeGuard, stderr: Option<&Path>) -> DaemonProc {
    match stderr {
        Some(path) => cmd.stderr(Stdio::from(std::fs::File::create(path).unwrap())),
        None => cmd.stderr(Stdio::inherit()),
    };
    let child = cmd.spawn().expect("spawn daemon");
    let mut d = DaemonProc { child, paths: registry::daemon_paths_in(&run_dir(guard), None) };
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            return d;
        }
        if let Some(status) = d.child.try_wait().expect("try_wait") {
            let err = stderr.map(|p| std::fs::read_to_string(p).unwrap_or_default()).unwrap_or_default();
            panic!("the daemon exited ({status}) before it listened; its stderr:\n{err}");
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "the daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn session_config(cwd: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_path_buf()),
        env: None,
        model_override: Some("claude-sonnet-4-5".into()),
        ..Default::default()
    }
}

/// Submit `text` and wait for the turn's end.
async fn one_turn(t: &mut SocketTransport, text: &str) {
    t.send_from_self(SessionCommand::Submit { text: text.into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("the turn hung")
            .expect("alive");
        match env.event {
            SessionEventWire::Idle => break,
            SessionEventWire::Refused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
}

/// Every daemon log line (`synaps.log*` under the base dir) containing `needle`.
fn log_lines(guard: &HomeGuard, needle: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(guard.base_dir()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("synaps.log") {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            lines.extend(text.lines().filter(|l| l.contains(needle)).map(str::to_string));
        }
    }
    lines
}

/// Wait until the log holds a line with `needle`; returns every such line.
async fn wait_log(guard: &HomeGuard, needle: &str) -> Vec<String> {
    let t0 = std::time::Instant::now();
    loop {
        let lines = log_lines(guard, needle);
        if !lines.is_empty() {
            return lines;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the daemon log never had a line with {needle:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the daemon lists `id` with `want`.
async fn wait_listed(paths: &registry::DaemonPaths, id: &SessionId, want: SessionLifecycle) {
    let t0 = std::time::Instant::now();
    loop {
        let metas = SocketTransport::sessions(&paths.sock).await.unwrap_or_default();
        if metas.iter().any(|m| &m.id == id && m.lifecycle == want) {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "{id} never listed {want:?}: {metas:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Daemon A: one session with one turn, then a graceful stop, which keeps
/// a crash record marked as a stop. Returns the session's id.
async fn a_stopped_daemon_with_one_session(guard: &HomeGuard, url: &str) -> SessionId {
    let mut a = start(daemon_command(guard, url, &[], None), guard, None).await;
    let cwd = guard.home.path().to_path_buf();
    let conn = SocketTransport::connect(&a.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _) = SocketTransport::attach(
        conn,
        Attach::Create { config: session_config(&cwd), mode: AttachMode::Mirror },
    )
    .await
    .expect("attached");
    let id = t.session_id().clone();
    one_turn(&mut t, "hello").await;
    SocketTransport::shutdown(&a.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while a.child.try_wait().expect("try_wait").is_none() {
        assert!(t0.elapsed() < Duration::from_secs(30), "daemon A never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(t);
    let rec = crash_record::read(&crash_record::path(&a.paths)).ok().flatten().expect("a clean stop keeps the record");
    assert!(rec.reload.as_ref().is_some_and(|h| h.stop), "{rec:#?}");
    assert_eq!(rec.sessions.len(), 1, "{rec:#?}");
    id
}

/// What both cases want of daemon B: a fresh start that holds the flock,
/// one WARN in the log and one line in its stderr file, and the stopped
/// daemon's session restored.
async fn assert_started_fresh_and_restored(guard: &HomeGuard, b: &DaemonProc, stderr: &Path, id: &SessionId) {
    let info = registry::read_daemon_json(&b.paths).expect("daemon.json");
    assert_eq!(info.generation, 1, "a fresh start is generation 1, not the stale state's");
    assert!(SocketTransport::ping(&b.paths.sock).await.is_ok(), "status answers");
    assert!(registry::is_alive(&b.paths), "the fresh start holds daemon.lock's flock itself");
    let warn = wait_log(guard, STALE_LINE).await;
    assert!(warn[0].contains("WARN") && warn[0].contains("starting fresh"), "{warn:#?}");
    let summary = wait_log(guard, "crash restore: restored").await;
    assert!(summary[0].contains(RESTORED_ONE), "the stop record is restored: {summary:#?}");
    assert!(summary[0].contains("how=\"stop\"") || summary[0].contains("how=stop"), "{summary:#?}");
    wait_listed(&b.paths, id, SessionLifecycle::Live).await;
    assert_eq!(log_lines(guard, STALE_LINE).len(), 1, "the WARN is logged once");
    let err = std::fs::read_to_string(stderr).unwrap();
    assert_eq!(err.matches(STALE_LINE).count(), 1, "one stderr line:\n{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fresh_start_whose_env_names_a_missing_reload_state_starts_fresh_and_restores() {
    let guard = HomeGuard::new();
    let url = spawn_provider().await;
    let id = a_stopped_daemon_with_one_session(&guard, &url).await;

    // The environment a reloaded daemon had, copied into a fresh start: its
    // state file is gone (the reload removed it) and fd 29 is not the lock.
    let paths = registry::daemon_paths_in(&run_dir(&guard), None);
    let state = reload::reload_state_path(&paths);
    assert!(!state.exists());
    let state_s = state.to_string_lossy().into_owned();
    let fd = STALE_FD.to_string();
    let stale = [
        (reload::RELOAD_STATE_ENV, state_s.as_str()),
        (reload::LOCK_FD_ENV, fd.as_str()),
        (reload::SUBAGENT_ID_FLOOR_ENV, "20"),
    ];
    let stderr = guard.home.path().join("b.stderr");
    let b = start(daemon_command(&guard, &url, &stale, None), &guard, Some(&stderr)).await;
    assert_started_fresh_and_restored(&guard, &b, &stderr, &id).await;
    assert!(!state.exists(), "nothing wrote a state file");
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_stale_reload_state_with_a_lock_fd_that_is_not_the_lock_starts_fresh_and_leaves_the_fd_alone() {
    let guard = HomeGuard::new();
    let url = spawn_provider().await;
    let id = a_stopped_daemon_with_one_session(&guard, &url).await;

    // A stale state file that parses (an earlier reload's, generation 7),
    // and fd 29 open on another file.
    let paths = registry::daemon_paths_in(&run_dir(&guard), None);
    let state = reload::reload_state_path(&paths);
    let stale_state = reload::ReloadState {
        generation: 7,
        written_at: chrono::Utc::now(),
        sessions: vec![],
        expected_clients: 0,
        next_subagent_id: 20,
        base_umask: None,
        children: Vec::new(),
    };
    let body = serde_json::to_vec(&stale_state).unwrap();
    std::fs::write(&state, &body).unwrap();
    let decoy = guard.home.path().join("decoy");
    std::fs::write(&decoy, "not the lock\n").unwrap();
    let state_s = state.to_string_lossy().into_owned();
    let fd = STALE_FD.to_string();
    let stale = [
        (reload::RELOAD_STATE_ENV, state_s.as_str()),
        (reload::LOCK_FD_ENV, fd.as_str()),
        (reload::SUBAGENT_ID_FLOOR_ENV, "20"),
    ];
    // bash opens the decoy on fd 29 without close-on-exec, then execs the
    // daemon (same pid), which inherits it.
    let script = format!("exec {STALE_FD}<\"$SYNAPS_TEST_DECOY\" && exec \"$SYNAPS_TEST_BIN\" daemon --foreground");
    let mut cmd = daemon_command(&guard, &url, &stale, Some(&script));
    cmd.env("SYNAPS_TEST_DECOY", &decoy);
    let stderr = guard.home.path().join("b.stderr");
    let b = start(cmd, &guard, Some(&stderr)).await;
    assert_started_fresh_and_restored(&guard, &b, &stderr, &id).await;

    // The fd is left alone: still open on the decoy, still inheritable.
    let pid = b.child.id();
    let target = std::fs::read_link(format!("/proc/{pid}/fd/{STALE_FD}")).expect("fd 29 is still open");
    assert_eq!(target, decoy, "fd 29 still names the decoy");
    let fdinfo = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{STALE_FD}")).unwrap();
    let flags = fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .map(|v| u32::from_str_radix(v.trim(), 8).unwrap())
        .expect("flags line");
    assert_eq!(flags & 0o2_000_000, 0, "fd 29 is not made close-on-exec: {fdinfo}");
    assert_eq!(std::fs::read(&state).unwrap(), body, "the stale state file is left on disk as it was");
    assert_eq!(std::fs::read_to_string(&decoy).unwrap(), "not the lock\n");
    SocketTransport::shutdown(&b.paths.sock, true).await.unwrap();
}
