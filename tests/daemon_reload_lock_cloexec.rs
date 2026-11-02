//! A reload hands the flock to the next image by clearing close-on-exec on
//! the lock fd for the exec. The next image must set it back BEFORE it
//! starts any process. Otherwise every child it starts (an extension
//! sidecar first) inherits an fd on `daemon.lock`, and one that outlives
//! the daemon keeps the flock, so every later fresh start refuses ("another
//! daemon holds ..."). This runs against a REAL daemon with one extension
//! sidecar (`tests/fixtures/slow_session_start_extension.py`).
//!
//! - `no_child_of_a_reloaded_daemon_holds_daemon_lock_and_a_stop_frees_it`:
//!   - reload twice; after each reload no descendant process of the daemon
//!     has an fd on `daemon.lock` (by device and inode), and the daemon
//!     still holds the flock at generation 2, then 3;
//!   - then a graceful stop, after which no process has an fd on it and the
//!     flock is free, and a fresh start takes the flock.
//!
//! Linux only (`/proc`).

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use axum::response::IntoResponse;
use phase2::*;
use serial_test::serial;

/// The fixture sidecar's script name, as it shows in its command line.
const SIDECAR: &str = "slow_session_start_extension.py";

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

/// `synaps daemon --foreground` under the guard's HOME; waits for a ping.
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
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .env_remove("SYNAPS_DAEMON_SUBAGENT_ID_FLOOR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let mut d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    loop {
        if SocketTransport::ping(&d.paths.sock).await.is_ok() {
            return d;
        }
        if let Some(status) = d.child.try_wait().expect("try_wait") {
            panic!("the daemon exited ({status}) before it listened");
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "the daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Install the slow-start fixture extension as a plugin (one sidecar).
fn install_sidecar_plugin(guard: &HomeGuard) -> PathBuf {
    let dir = guard.base_dir().join("plugins/slow-start");
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(dir.join("extensions")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(SIDECAR),
        dir.join("extensions").join(SIDECAR),
    )
    .unwrap();
    std::fs::write(
        dir.join(".synaps-plugin/plugin.json"),
        format!(
            r#"{{
  "name": "slow-start",
  "version": "0.1.0",
  "extension": {{
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/{SIDECAR}"],
    "permissions": ["session.lifecycle"],
    "hooks": [{{"hook": "on_session_start"}}]
  }}
}}
"#
        ),
    )
    .unwrap();
    dir
}

fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

fn ppid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...": comm may hold spaces and parens.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Every descendant process of `root` (not `root` itself).
fn descendants(root: u32) -> Vec<u32> {
    let parents: Vec<(u32, u32)> = all_pids().into_iter().filter_map(|p| Some((p, ppid_of(p)?))).collect();
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(child, parent) in &parents {
            if parent == p && !found.contains(&child) {
                found.push(child);
                frontier.push(child);
            }
        }
    }
    found
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
        .unwrap_or_default()
}

/// The device and inode of the daemon's lock file.
fn lock_id(paths: &registry::DaemonPaths) -> (u64, u64) {
    let m = std::fs::metadata(&paths.lock).expect("daemon.lock");
    (m.dev(), m.ino())
}

/// The fds among `pids` open on the file `id`, as "pid N (cmdline) fd M".
fn holders(pids: &[u32], id: (u64, u64)) -> Vec<String> {
    let mut found = Vec::new();
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::metadata(fd.path()).is_ok_and(|m| (m.dev(), m.ino()) == id) {
                found.push(format!("pid {pid} ({}) fd {}", cmdline(pid), fd.file_name().to_string_lossy()));
            }
        }
    }
    found
}

/// The fixture sidecars among `root`'s descendants.
fn sidecars(root: u32) -> Vec<u32> {
    descendants(root).into_iter().filter(|&p| cmdline(p).contains(SIDECAR)).collect()
}

/// Wait until a sidecar that is not in `seen` runs under `root`.
async fn wait_new_sidecar(root: u32, seen: &[u32]) -> u32 {
    let t0 = std::time::Instant::now();
    loop {
        if let Some(p) = sidecars(root).into_iter().find(|p| !seen.contains(p)) {
            return p;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "no new {SIDECAR} sidecar under pid {root}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the daemon answers as `generation`.
async fn wait_generation(paths: &registry::DaemonPaths, generation: u64) {
    let t0 = std::time::Instant::now();
    loop {
        if registry::read_daemon_json(paths).is_some_and(|i| i.generation == generation)
            && tokio::time::timeout(Duration::from_millis(500), SocketTransport::ping(&paths.sock))
                .await
                .is_ok_and(|r| r.is_ok())
        {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "generation {generation} never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn no_child_of_a_reloaded_daemon_holds_daemon_lock_and_a_stop_frees_it() {
    let guard = HomeGuard::new();
    install_sidecar_plugin(&guard);
    let url = spawn_provider().await;
    let mut d = spawn_daemon(&guard, &url).await;
    let pid = d.child.id();
    let id = lock_id(&d.paths);
    let mut seen = vec![wait_new_sidecar(pid, &[]).await];
    let h = holders(&descendants(pid), id);
    assert!(h.is_empty(), "at generation 1, a child holds daemon.lock: {h:#?}");

    for generation in [2u64, 3] {
        let got = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
        assert_eq!(got, generation);
        wait_generation(&d.paths, generation).await;
        assert_eq!(registry::read_daemon_json(&d.paths).map(|i| i.pid), Some(pid), "an exec keeps the pid");
        // The new image's own sidecar, started after the exec.
        seen.push(wait_new_sidecar(pid, &seen).await);
        let kids = descendants(pid);
        let h = holders(&kids, id);
        assert!(
            h.is_empty(),
            "after reload to generation {generation}, a child holds daemon.lock: {h:#?} (descendants {kids:?})"
        );
        assert!(registry::is_alive(&d.paths), "generation {generation} holds the flock");
        assert_eq!(lock_id(&d.paths), id, "the same lock file");
    }

    // A graceful stop: afterwards no process has an fd on daemon.lock and
    // the flock is free.
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while d.child.try_wait().expect("try_wait").is_none() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the daemon never exited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let t0 = std::time::Instant::now();
    loop {
        let h = holders(&all_pids(), id);
        if h.is_empty() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "after the stop, a process still holds daemon.lock: {h:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!registry::is_alive(&d.paths), "nothing holds the flock after the stop");

    // A fresh start takes the flock.
    let e = spawn_daemon(&guard, &url).await;
    assert!(registry::is_alive(&e.paths), "the fresh start holds the flock");
    assert_eq!(registry::read_daemon_json(&e.paths).map(|i| i.generation), Some(1));
    SocketTransport::shutdown(&e.paths.sock, true).await.unwrap();
}
