//! Last words, in real scratch daemons (`synaps daemon --foreground` and
//! auto-spawn; one temp HOME + runtime dir per test, no process-env
//! mutation). THESE START DAEMONS: run them only where daemon tests are
//! allowed.
//!
//! - A daemon's stderr no longer kills it once whoever started it has gone
//!   (the six silent deaths: `eprintln!` → EPIPE → panic → abort): a pipe
//!   stderr moves onto `logs/daemon-stderr.log` at ready, a reload's next
//!   image keeps serving after its boot-time config warnings, and a family
//!   diagnostic on a later `load_config()` (session create) is written to
//!   the file and the log instead of panicking.
//! - Auto-spawn hands the daemon the append-only file, never a pipe, and
//!   `daemon.idle_exit_secs = 0` spawns it with no `--idle-exit`.
//! - A panic leaves ONE `PANIC` ERROR line in synaps.log; under unwind (the
//!   test profile) a task's or a session actor's panic ends only that task
//!   or session: the daemon answers and other sessions carry on.

#![cfg(target_os = "linux")]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_engine::daemon::{registry, stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// A config warning at every boot (`config.warnings`) …
const BOOT_WARNING: &str = "daemon.idle_exit_secs = soon\n";
/// … and a family diagnostic, reported at a process's first
/// `load_config()` (each distinct one once per process).
const FAMILY_DIAGNOSTIC: &str = "turn_budget.bogus.max_tool_calls = 1\n";
const DIAGNOSTIC_TEXT: &str = "unknown turn_budget role 'bogus'";

struct Env {
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Env {
    fn new(config: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), config).unwrap();
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

    fn stderr_log(&self) -> PathBuf {
        let path = self.base().join("logs").join(stdio::STDERR_LOG);
        // /proc/<pid>/fd/2 shows the resolved path (TMPDIR may be a symlink).
        match (path.parent().and_then(|d| std::fs::canonicalize(d).ok()), path.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => path,
        }
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

    /// `synaps daemon --foreground` the way an older client spawned it:
    /// stderr a pipe. Once it listens, the read end is closed — the
    /// starter has gone.
    fn start_with_orphaned_stderr_pipe(&mut self, extra_env: &[(&str, &str)]) -> u32 {
        let mut cmd = self.cmd(&["daemon", "--foreground"]);
        cmd.envs(extra_env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn daemon");
        let err = child.stderr.take().unwrap();
        let pid = child.id();
        self.daemon = Some(child);
        wait_alive(&self.paths(), true, 30);
        drop(err);
        pid
    }

    /// `synaps daemon --foreground` with stderr on /dev/null.
    fn start_plain(&mut self, extra_env: &[(&str, &str)]) -> u32 {
        let mut cmd = self.cmd(&["daemon", "--foreground"]);
        cmd.envs(extra_env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn daemon");
        let pid = child.id();
        self.daemon = Some(child);
        wait_alive(&self.paths(), true, 30);
        pid
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

    fn wait_log(&self, needles: &[&str], secs: u64) -> String {
        let t0 = Instant::now();
        loop {
            let log = self.synaps_log();
            if log.lines().any(|l| needles.iter().all(|n| l.contains(n))) {
                return log;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "no synaps.log line with all of {needles:?} after {secs}s:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
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

/// A line client kept attached (stdin open), stdout/stderr tapped.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    out_rx: mpsc::Receiver<String>,
    out: String,
}

impl Client {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn client");
        let stdin = child.stdin.take();
        let (out_tx, out_rx) = mpsc::channel();
        for stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let tx = out_tx.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if tx.send(std::mem::take(&mut line)).is_err() {
                        break;
                    }
                }
            });
        }
        Self { child, stdin, out_rx, out: String::new() }
    }

    fn drain(&mut self) {
        while let Ok(l) = self.out_rx.try_recv() {
            self.out.push_str(&l);
        }
    }

    fn wait_output(&mut self, needle: &str, secs: u64) {
        let t0 = Instant::now();
        loop {
            self.drain();
            if self.out.contains(needle) {
                return;
            }
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(100));
                self.drain();
                panic!("client exited ({st}) before {needle:?}:\n{}", self.out);
            }
            assert!(t0.elapsed() < Duration::from_secs(secs), "timeout waiting for {needle:?}:\n{}", self.out);
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send_line(&mut self, text: &str) {
        let s = self.stdin.as_mut().expect("stdin open");
        s.write_all(text.as_bytes()).unwrap();
        s.write_all(b"\n").unwrap();
        s.flush().unwrap();
    }

    /// `Some(status)` if it exits within `secs`.
    fn exits_within(&mut self, secs: u64) -> Option<std::process::ExitStatus> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(100));
                self.drain();
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.drain();
        None
    }

    fn detach(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"/detach\n");
        }
        let _ = self.exits_within(10);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_alive(paths: &registry::DaemonPaths, want: bool, secs: u64) {
    let t0 = Instant::now();
    while registry::is_alive(paths) != want {
        assert!(t0.elapsed() < Duration::from_secs(secs), "daemon alive={} never became {want}", !want);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "))
}

fn fd2_target(pid: u32) -> PathBuf {
    std::fs::read_link(format!("/proc/{pid}/fd/2")).expect("read /proc/<pid>/fd/2")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn a_reload_after_the_starter_is_gone_keeps_serving() {
    let mut env = Env::new(&format!("{BOOT_WARNING}{FAMILY_DIAGNOSTIC}"));
    let paths = env.paths();
    let pid = env.start_with_orphaned_stderr_pipe(&[]);

    let out = env
        .cmd(&["daemon", "reload", "--now"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let t0 = Instant::now();
    while registry::read_daemon_json(&paths).is_none_or(|i| i.generation != 2) {
        assert!(
            t0.elapsed() < Duration::from_secs(60) && pid_alive(pid),
            "generation 2 never came up (pid {pid} alive: {}); reload said {}{}\nstderr file:\n{}\nlog:\n{}",
            pid_alive(pid),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            read(&env.stderr_log()),
            env.synaps_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Gen 2 printed its boot warnings to the inherited fd 2 and lives on.
    let t0 = Instant::now();
    while !env.status_ok() {
        assert!(t0.elapsed() < Duration::from_secs(30), "generation 2 does not answer");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert!(pid_alive(pid), "the daemon died after its reload");
    assert!(env.status_ok(), "generation 2 stopped answering");
    assert_eq!(registry::read_daemon_json(&paths).unwrap().pid, pid, "a reload keeps the pid");

    let stderr = read(&env.stderr_log());
    assert!(stderr.contains("synaps: config warning:"), "gen 2's boot warning is in the file:\n{stderr}");
    assert!(stderr.contains(DIAGNOSTIC_TEXT), "{stderr}");
    let log = env.synaps_log();
    assert!(log.contains("config diagnostic") && log.contains(DIAGNOSTIC_TEXT), "{log}");
    assert_eq!(fd2_target(pid), env.stderr_log(), "the pipe stderr moved onto the file");
}

/// Poll `path` until it holds `needle` (at most `secs`); returns its text.
fn wait_file(path: &Path, needle: &str, secs: u64, log: impl Fn() -> String) -> String {
    let t0 = Instant::now();
    loop {
        let text = read(path);
        if text.contains(needle) {
            return text;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(secs),
            "{needle:?} not in {} after {secs}s:\n{text}\nlog:\n{}",
            path.display(),
            log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The engine command `saveas` names the session and saves it even with no
/// messages, and that save runs `load_config()` in the daemon. A session
/// create alone runs none, and a session with no history ends at idle
/// instead of parking (no save). The attach client sends `/cmd NAME ARG` as
/// an engine command (a bare `/saveas` would be a prompt) and prints the
/// reply's JSON.
const SAVE_COMMAND: &str = "/cmd saveas last-words";
const SAVE_REPLY: &str = "session named 'last-words'";

#[test]
fn a_family_diagnostic_after_the_starter_is_gone_keeps_the_daemon_up() {
    let mut env = Env::new(FAMILY_DIAGNOSTIC);
    let pid = env.start_with_orphaned_stderr_pipe(&[]);

    // Each distinct diagnostic is reported once per process, so the boot's
    // own is not written again. A line ADDED while the daemon runs (what
    // happened live: `turn_budget.event.*` lines an older binary rejects)
    // is new: the next `load_config()` reports it, here a session save.
    const ADDED_TEXT: &str = "unknown turn_budget role 'added_later'";
    let config = env.base().join("config");
    let mut content = read(&config);
    content.push_str("turn_budget.added_later.max_tool_calls = 1\n");
    std::fs::write(&config, content).unwrap();
    assert!(!read(&env.stderr_log()).contains(ADDED_TEXT));

    let mut a = Client::spawn(env.cmd(&["attach", "--create"]));
    a.wait_output("○ ready", 30);
    a.send_line(SAVE_COMMAND);
    a.wait_output(SAVE_REPLY, 20);
    let stderr = wait_file(&env.stderr_log(), ADDED_TEXT, 20, || env.synaps_log());
    a.detach();

    std::thread::sleep(Duration::from_secs(1));
    assert!(pid_alive(pid), "the daemon died");
    assert!(env.status_ok(), "the daemon does not answer");
    assert_eq!(read(&env.stderr_log()).matches(ADDED_TEXT).count(), 1, "reported once:\n{stderr}");
    env.wait_log(&["config diagnostic", ADDED_TEXT], 10);
}

#[test]
fn an_autospawned_daemon_appends_stderr_to_a_file_and_outlives_its_client() {
    let env = Env::new(&format!("daemon.idle_exit_secs = 0\n{FAMILY_DIAGNOSTIC}"));
    let paths = env.paths();
    std::fs::create_dir_all(env.stderr_log().parent().unwrap()).unwrap();
    std::fs::write(env.stderr_log(), "an earlier daemon wrote this\n").unwrap();

    let mut a = Client::spawn(env.cmd(&["attach", "--create"]));
    a.wait_output("○ ready", 30);
    let pid = registry::read_daemon_json(&paths).expect("daemon.json").pid;
    assert_eq!(fd2_target(pid), env.stderr_log(), "stderr must be the file, not a pipe");
    a.detach();
    drop(a);

    // The spawner is gone. Each distinct diagnostic is reported once per
    // process (the boot's went to the file already), so add a line: the
    // next `load_config()` (a session save) reports it, to the file.
    const ADDED_TEXT: &str = "unknown turn_budget role 'added_after_spawner'";
    let config = env.base().join("config");
    let mut content = read(&config);
    content.push_str("turn_budget.added_after_spawner.max_tool_calls = 1\n");
    std::fs::write(&config, content).unwrap();
    let mut b = Client::spawn(env.cmd(&["attach", "--create"]));
    b.wait_output("○ ready", 30);
    b.send_line(SAVE_COMMAND);
    b.wait_output(SAVE_REPLY, 20);
    let stderr = wait_file(&env.stderr_log(), ADDED_TEXT, 20, || env.synaps_log());
    b.detach();
    assert!(pid_alive(pid), "the daemon died after its spawner left");
    assert!(stderr.starts_with("an earlier daemon wrote this\n"), "appended, never truncated:\n{stderr}");
    assert!(stderr.contains(DIAGNOSTIC_TEXT), "the boot's diagnostic:\n{stderr}");
}

/// Part 4: `daemon.idle_exit_secs = 0` in the config file → the auto-spawned
/// daemon gets no `--idle-exit` (control: 7 → `--idle-exit 7`).
#[test]
fn idle_exit_secs_zero_spawns_a_daemon_without_idle_exit() {
    for (config, want) in [("daemon.idle_exit_secs = 0\n", None), ("daemon.idle_exit_secs = 7\n", Some("7"))] {
        let env = Env::new(config);
        let mut a = Client::spawn(env.cmd(&["attach", "--create"]));
        a.wait_output("○ ready", 30);
        let pid = registry::read_daemon_json(&env.paths()).expect("daemon.json").pid;
        let argv: Vec<String> = std::fs::read(format!("/proc/{pid}/cmdline"))
            .unwrap()
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let idle = argv.iter().position(|a| a == "--idle-exit").map(|i| argv[i + 1].as_str());
        assert_eq!(idle, want, "{config:?}: argv {argv:?}");
        a.detach();
    }
}

#[test]
fn a_task_panic_leaves_its_last_words_and_the_daemon_lives_on() {
    let mut env = Env::new("");
    let pid = env.start_plain(&[("SYNAPS_TEST_PANIC_TASK", "1")]);
    let log = env.wait_log(&[" ERROR ", "PANIC", "forced panic in a daemon task"], 15);
    let line = log.lines().find(|l| l.contains("forced panic in a daemon task")).unwrap();
    assert!(line.contains("location=crates/agent-engine/src/daemon/mod.rs:"), "{line}");
    assert!(line.contains("backtrace=\"0: "), "{line}");
    assert_eq!(log.matches("forced panic in a daemon task").count(), 1, "ONE line:\n{log}");
    assert!(pid_alive(pid), "a task panic ended the daemon");
    assert!(env.status_ok(), "the daemon does not answer after a task panic");
}

#[test]
fn a_panicking_session_ends_only_that_session() {
    const BOOM: &str = "__last_words_panic_now__";
    let mut env = Env::new("");
    let pid = env.start_plain(&[("SYNAPS_TEST_PANIC_PROMPT", BOOM)]);

    let mut keeper = Client::spawn(env.cmd(&["attach", "--create"]));
    keeper.wait_output("○ ready", 30);
    let mut doomed = Client::spawn(env.cmd(&["attach", "--create"]));
    doomed.wait_output("○ ready", 30);
    doomed.send_line(BOOM);
    let st = doomed.exits_within(30);
    assert!(st.is_some(), "the client of a panicked session hangs:\n{}", doomed.out);

    env.wait_log(&["PANIC", "forced panic in a session actor"], 10);
    env.wait_log(&["session actor panicked"], 10);
    assert!(pid_alive(pid), "a session panic ended the daemon");
    assert!(env.status_ok(), "the daemon does not answer");
    let sessions = env.cmd(&["daemon", "sessions", "--json"]).stdin(Stdio::null()).output().unwrap();
    let list: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        list.as_array().map(Vec::len),
        Some(1),
        "only the keeper's session is left: {}",
        String::from_utf8_lossy(&sessions.stdout)
    );
    // The other session still takes a prompt (the model is unreachable;
    // any reply proves the actor runs) and detaches cleanly.
    keeper.send_line("hello after the panic");
    std::thread::sleep(Duration::from_secs(2));
    assert!(keeper.child.try_wait().unwrap().is_none(), "the keeper's client ended:\n{}", keeper.out);
    keeper.detach();
}
