//! `--profile` through the daemon path (0.10.1 incident: `synaps --profile
//! axon` ran the TUI on the DEFAULT profile — main config, main sessions
//! dir — and `/model` clobbered `~/.synaps-cli/config`). Root cause: the
//! thin client's `ensure_daemon` read config before `main` pinned the
//! profile, latching `get_profile()` to the default for the whole process.
//!
//! Real processes, one temp HOME + base + runtime dir per test (same
//! isolation as `daemon_autospawn.rs`), no process-env mutation.
//!
//! The line client (`synaps attach`) takes the exact same `main` path as
//! the TUI (`ThinClient::Line` vs `Tui`/`Adopt` → `ensure_daemon`), so it
//! exercises the profile pin without a PTY.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;

const MAIN_MODEL: &str = "claude-opus-4-7";
const PROFILE_MODEL: &str = "claude-fable-5";

struct Env {
    home: tempfile::TempDir,
}

impl Env {
    /// Main config: `MAIN_MODEL`, never idle-exit. Profile `work`:
    /// `PROFILE_MODEL`, idle-exit after 1 s. Which one a process read is
    /// visible from outside: the session model and whether the auto-spawned
    /// daemon exits on its own.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(base.join("work")).unwrap();
        std::fs::write(
            base.join("config"),
            format!("model = {MAIN_MODEL}\ndaemon.idle_exit_secs = 0\n"),
        )
        .unwrap();
        std::fs::write(
            base.join("work").join("config"),
            format!("model = {PROFILE_MODEL}\ndaemon.idle_exit_secs = 1\n"),
        )
        .unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        Self { home }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn paths(&self, profile: Option<&str>) -> registry::DaemonPaths {
        registry::daemon_paths_in(&self.home.path().join("run"), profile)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_synaps"));
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("SYNAPS_RUNTIME_DIR", self.home.path().join("run"))
            .env("SYNAPS_ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
            .env("SYNAPS_NO_BOOT_FX", "1")
            .env("TERM", "dumb")
            .current_dir(self.home.path());
        c
    }

    fn stop(&self, profile: Option<&str>) {
        let mut args = Vec::new();
        if let Some(p) = profile {
            args.extend(["--profile", p]);
        }
        args.extend(["daemon", "stop", "--force"]);
        let _ = self
            .cmd(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leak a setsid'd daemon past the test.
        for p in [None, Some("work"), Some("a.b")] {
            if registry::is_alive(&self.paths(p)) {
                self.stop(p);
            }
        }
    }
}

/// A line client held attached until `detach`.
struct Client {
    child: Child,
    out_rx: mpsc::Receiver<String>,
    out: String,
}

impl Client {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn client");
        let out = child.stdout.take().unwrap();
        let (tx, out_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut line = String::new();
            while matches!(r.read_line(&mut line), Ok(n) if n > 0) {
                if tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            out_rx,
            out: String::new(),
        }
    }

    fn wait_ready(&mut self, secs: u64) {
        let t0 = Instant::now();
        while !self.out.contains("○ ready") {
            while let Ok(l) = self.out_rx.try_recv() {
                self.out.push_str(&l);
            }
            if let Some(st) = self.child.try_wait().unwrap() {
                panic!("client exited ({st}) before ready:\n{}", self.out);
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "timeout waiting for ready:\n{}",
                self.out
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn detach(&mut self) {
        if let Some(mut s) = self.child.stdin.take() {
            let _ = s.write_all(b"/detach\n");
        }
        let t0 = Instant::now();
        while self.child.try_wait().unwrap().is_none() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_dead(paths: &registry::DaemonPaths, secs: u64) -> bool {
    let t0 = Instant::now();
    while registry::is_alive(paths) {
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[test]
fn profile_client_spawns_and_uses_the_profile_daemon() {
    let env = Env::new();
    let work = env.paths(Some("work"));
    let mut a = Client::spawn(env.cmd(&["--profile", "work", "attach", "--create"]));
    a.wait_ready(30);

    // The daemon is the profile's, and its session runs the profile config.
    assert!(registry::is_alive(&work), "daemon-work not running");
    assert!(
        !registry::is_alive(&env.paths(None)),
        "a default-profile daemon was started"
    );
    let info = registry::read_daemon_json(&work).expect("daemon-work.json");
    assert_eq!(info.profile.as_deref(), Some("work"));
    assert!(
        a.out.contains(PROFILE_MODEL),
        "session not on the profile model:\n{}",
        a.out
    );
    assert!(
        !a.out.contains(MAIN_MODEL),
        "session on the MAIN model:\n{}",
        a.out
    );

    // The client's own config reads were the profile's too: the idle-exit it
    // handed the daemon is work's 1 s, not main's "never" (0.10.1: main).
    a.detach();
    assert!(
        wait_dead(&work, 20),
        "daemon-work never idle-exited: the client spawned it with the MAIN config's idle_exit_secs"
    );
    // The global session index (sessions/index.jsonl) is shared across
    // profiles by design (each record carries `profile`); anything else in
    // the main sessions dir would be a profile session leaking out.
    let main_sessions = env.base().join("sessions");
    if main_sessions.exists() {
        let leaked: Vec<_> = std::fs::read_dir(&main_sessions)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "index.jsonl" && !n.starts_with("index.jsonl."))
            .collect();
        assert!(
            leaked.is_empty(),
            "a session was written to the MAIN sessions dir: {leaked:?}"
        );
    }
}

#[test]
fn synaps_profile_env_selects_the_profile_daemon() {
    let env = Env::new();
    let mut cmd = env.cmd(&["attach", "--create"]);
    cmd.env("SYNAPS_PROFILE", "work");
    let mut a = Client::spawn(cmd);
    a.wait_ready(30);
    assert!(
        registry::is_alive(&env.paths(Some("work"))),
        "env profile did not pick daemon-work"
    );
    assert!(
        !registry::is_alive(&env.paths(None)),
        "env-profiled client spawned the default daemon"
    );
    assert!(a.out.contains(PROFILE_MODEL), "{}", a.out);
    a.detach();
}

#[test]
fn client_refuses_a_daemon_serving_another_profile() {
    let env = Env::new();
    // `a.b` and `a_b` sanitise to the same socket stem (`daemon-a_b`).
    let start = env
        .cmd(&["--profile", "a.b", "daemon", "--detach"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let paths = env.paths(Some("a.b"));
    assert_eq!(
        paths.sock,
        env.paths(Some("a_b")).sock,
        "precondition: shared socket"
    );
    assert!(registry::is_alive(&paths));

    let out = env
        .cmd(&["--profile", "a_b", "attach", "--create"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(agent_engine::daemon::EXIT_REFUSED),
        "{err}"
    );
    assert!(err.contains("refusing to attach"), "{err}");
    assert!(
        err.contains("profile `a.b`") && err.contains("profile `a_b`"),
        "{err}"
    );
    env.stop(Some("a.b"));
}
