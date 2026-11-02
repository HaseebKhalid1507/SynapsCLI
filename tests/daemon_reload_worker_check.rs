//! `synaps daemon reload` and the worker-record check (spec §5), in a real
//! scratch daemon (`synaps daemon --foreground`; one temp HOME + runtime
//! dir per test, no process-env mutation). THESE START DAEMONS: run them
//! only where daemon tests are allowed.
//!
//! The daemon holds one writer-table entry for no real run through its
//! test seam (`SYNAPS_TEST_HOLD_WORKER_WRITER`, the `testing` feature), so
//! the check runs with worker rollover off:
//!
//! - a record write still in flight refuses a normal reload with exit 3
//!   after the drain and the 5 s wait (the worst case: drain cap + 5 s);
//!   the image keeps running (same pid, same generation);
//! - an unresolved record refuses at once, before any drain;
//! - `--force` goes ahead: one stderr line names the workers, stdout is
//!   unchanged, and `--json` carries them as `waived`.

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

const HOLD_ENV: &str = "SYNAPS_TEST_HOLD_WORKER_WRITER";
const TAIL: &str = "; the daemon keeps running this image; no worker was stopped and any paused turn resumes in \
                    place; retry, or reload with --force to go ahead without them";

struct Env {
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        Self { home, daemon: None }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn paths(&self) -> registry::DaemonPaths {
        registry::daemon_paths_in(&self.home.path().join("run"), None)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
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

    /// `synaps daemon --foreground` holding a writer in `state`.
    fn start_holding(&mut self, state: &str) -> u32 {
        let mut cmd = self.cmd(&["daemon", "--foreground"]);
        cmd.env(HOLD_ENV, state).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn daemon");
        let pid = child.id();
        self.daemon = Some(child);
        let t0 = Instant::now();
        while !registry::is_alive(&self.paths()) {
            assert!(t0.elapsed() < Duration::from_secs(30), "the daemon never came up");
            std::thread::sleep(Duration::from_millis(50));
        }
        pid
    }

    /// `synaps daemon reload <args>`, timed.
    fn reload(&self, args: &[&str]) -> (Output, Duration) {
        let mut all = vec!["daemon", "reload"];
        all.extend_from_slice(args);
        let t0 = Instant::now();
        let out = self.cmd(&all).stdin(Stdio::null()).output().expect("run reload");
        (out, t0.elapsed())
    }

    fn generation(&self) -> u64 {
        registry::read_daemon_json(&self.paths()).expect("daemon.json").generation
    }

    /// Wait until daemon.json names `generation` (a forced reload's exec).
    fn wait_generation(&self, generation: u64) {
        let t0 = Instant::now();
        while registry::read_daemon_json(&self.paths()).is_none_or(|i| i.generation != generation) {
            assert!(t0.elapsed() < Duration::from_secs(60), "generation {generation} never came up");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn status_ok(&self) -> bool {
        self.cmd(&["daemon", "status"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("pid: "))
    }

    /// Every `synaps.log*` file in the base dir, concatenated.
    fn synaps_log(&self) -> String {
        let mut all = String::new();
        if let Ok(rd) = std::fs::read_dir(self.base()) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("synaps.log") {
                    all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                }
            }
        }
        all
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leak a scratch daemon past the test.
        if registry::is_alive(&self.paths()) {
            let _ = self
                .cmd(&["daemon", "stop", "--force"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if let Some(mut child) = self.daemon.take() {
            let t0 = Instant::now();
            while child.try_wait().ok().flatten().is_none() && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_record_write_in_flight_refuses_a_reload_until_it_is_forced() {
    let mut env = Env::new();
    let pid = env.start_holding("publishing");
    let refusal = format!(
        "worker records are not durable: sa_test (session test-session, a record write is still in flight){TAIL}"
    );

    // A normal reload: the drain runs to its 1 s cap (the write counts as
    // busy), the write gets its 5 s, then the reload is refused.
    let (out, took) = env.reload(&["--drain-secs", "1"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stdout {:?} stderr {stderr:?}", text(&out.stdout));
    assert_eq!(stderr.trim_end(), format!("reload refused: refused: {refusal}"), "{stderr:?}");
    assert!(took >= Duration::from_secs(6), "the drain cap + 5 s was cut short: {took:?}");
    assert!(took < Duration::from_secs(20), "the refusal took {took:?}");
    assert_eq!(env.generation(), 1, "a refused reload keeps the image");
    assert_eq!(registry::read_daemon_json(&env.paths()).unwrap().pid, pid);
    assert!(env.status_ok(), "the daemon stopped answering after a refusal");
    let log = env.synaps_log();
    let drain = log.lines().find(|l| l.contains("daemon: reload drain ended"));
    let drain = drain.unwrap_or_else(|| panic!("no drain line:\n{log}"));
    for field in ["writers=1", "cap_used=true"] {
        assert!(drain.contains(field), "{field} in {drain:?}");
    }
    assert!(log.lines().any(|l| l.contains("WARN") && l.contains(&format!("reload refused: {refusal}"))), "{log}");

    // `--json`: the same refusal, as the error field.
    let (out, _) = env.reload(&["--now", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{:?}", text(&out.stderr));
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one json line");
    assert_eq!(reply, serde_json::json!({"ok": false, "error": format!("refused: {refusal}")}));
    assert_eq!(env.generation(), 1);

    // `--force`: one stderr line names the worker; stdout as ever.
    let (out, _) = env.reload(&["--now", "--force"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr:?}");
    assert_eq!(
        stderr.trim_end(),
        "reload forced: worker records not preserved: sa_test (session test-session, a record write was still in flight)"
    );
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with(&format!("reloaded (pid {pid}, generation ")), "{stdout:?}");
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    env.wait_generation(2);
    assert_eq!(registry::read_daemon_json(&env.paths()).unwrap().pid, pid, "a reload keeps the pid");

    // The next image holds the writer again: `--force --json` names it.
    let (out, _) = env.reload(&["--now", "--force", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "", "--json prints no forced line");
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one json line");
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        reply["waived"],
        serde_json::json!([{"worker": "sa_test", "session": "test-session", "why": "publishing"}]),
        "{reply}"
    );
    env.wait_generation(3);
}

#[test]
fn an_unresolved_record_refuses_a_reload_before_any_drain() {
    let mut env = Env::new();
    env.start_holding("unresolved");
    let (out, took) = env.reload(&["--drain-secs", "30"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{stderr:?}");
    assert_eq!(
        stderr.trim_end(),
        format!(
            "reload refused: refused: worker records are not durable: sa_test (session test-session, no durable \
             record: its publication is unresolved){TAIL}"
        )
    );
    assert!(took < Duration::from_secs(5), "an unresolved record waited: {took:?}");
    let log = env.synaps_log();
    assert!(!log.contains("daemon: reload drain ended"), "the refusal came after a drain:\n{log}");
    assert!(!log.contains("daemon: reload accepted"), "{log}");
    assert_eq!(env.generation(), 1);
    assert!(env.status_ok());

    // `--force` goes ahead without it.
    let (out, _) = env.reload(&["--now", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr).trim_end(),
        "reload forced: worker records not preserved: sa_test (session test-session, no durable record: its \
         publication was unresolved)"
    );
    env.wait_generation(2);
}
