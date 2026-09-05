//! Auto-spawn (jcode model): the first `synaps attach` / `synaps --attach`
//! starts the daemon itself — `setsid` + ready-fd, the daemon holds the
//! flock — and later clients find it. Real processes, one temp HOME +
//! runtime dir per test, no process-env mutation (parallel-safe).
//!
//! Covered: spawn on first attach (daemon.json, pid alive, flock held) →
//! second attach reuses (same pid) → `daemon stop` ends it; concurrent
//! first-clients → exactly ONE daemon (spawn lock); `SYNAPS_DAEMON_AUTOSPAWN=0`
//! → the no-daemon message, nothing spawned; `SYNAPS_DAEMON=0` → exit 3;
//! the legacy-MCP refusal → line client exits 3 with the reason, the TUI
//! falls back in-process (stderr notice, no thin-client ladder — the
//! on-screen SystemNotice itself needs the tmux harness and is not asserted
//! here); `daemon status` with nobody running says it auto-starts.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// One isolated HOME (+ base dir with synthetic auth, + runtime dir).
struct Env {
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        Self { home }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn run_dir(&self) -> PathBuf {
        self.home.path().join("run")
    }

    fn paths(&self) -> registry::DaemonPaths {
        registry::daemon_paths_in(&self.run_dir(), None)
    }

    /// Legacy-MCP refusal setup: `progressive_tool_disclosure=false` + one server.
    fn legacy_mcp(&self) {
        std::fs::write(self.base().join("config"), "progressive_tool_disclosure = false\n").unwrap();
        std::fs::write(
            self.base().join("mcp.json"),
            r#"{"mcpServers":{"noop":{"command":"/bin/cat","args":[]}}}"#,
        )
        .unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("SYNAPS_RUNTIME_DIR", self.run_dir())
            .env("SYNAPS_ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
            .env("SYNAPS_NO_BOOT_FX", "1")
            .env("TERM", "dumb")
            .current_dir(self.home.path());
        c
    }

    fn stop(&self) {
        let _ = self
            .cmd(&["daemon", "stop", "--force"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leak a setsid'd daemon past the test.
        if registry::is_alive(&self.paths()) {
            self.stop();
        }
    }
}

/// A line client kept attached (stdin open) with its stdout/stderr tapped.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    out_rx: mpsc::Receiver<String>,
    err_rx: mpsc::Receiver<String>,
    out: String,
    err: String,
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
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if out_tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if err_tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self { child, stdin, out_rx, err_rx, out: String::new(), err: String::new() }
    }

    fn drain(&mut self) {
        while let Ok(l) = self.out_rx.try_recv() {
            self.out.push_str(&l);
        }
        while let Ok(l) = self.err_rx.try_recv() {
            self.err.push_str(&l);
        }
    }

    /// Wait until stdout contains `needle` (≤ `secs`).
    fn wait_stdout(&mut self, needle: &str, secs: u64) {
        let t0 = Instant::now();
        loop {
            self.drain();
            if self.out.contains(needle) {
                return;
            }
            if let Some(st) = self.child.try_wait().unwrap() {
                self.drain();
                panic!("client exited ({st}) before {needle:?}\nstdout:\n{}\nstderr:\n{}", self.out, self.err);
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "timeout waiting for {needle:?}\nstdout:\n{}\nstderr:\n{}",
                self.out,
                self.err
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_exit(&mut self, secs: u64) -> std::process::ExitStatus {
        let t0 = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(50));
                self.drain();
                return st;
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                let _ = self.child.kill();
                let st = self.child.wait().unwrap();
                self.drain();
                return st;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn detach(&mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"/detach\n");
        }
        let _ = self.wait_exit(10);
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
}

#[test]
fn first_attach_spawns_second_reuses_stop_ends() {
    let env = Env::new();
    let paths = env.paths();
    assert!(!registry::is_alive(&paths));

    let mut a = Client::spawn(env.cmd(&["attach", "--create"]));
    a.wait_stdout("○ ready", 30);
    assert!(a.err.contains("starting daemon (pid "), "first client announces the spawn:\n{}", a.err);
    assert!(a.err.contains("synaps daemon stop to end it"), "{}", a.err);
    assert!(registry::is_alive(&paths), "flock held");
    let info = registry::read_daemon_json(&paths).expect("daemon.json");
    assert!(pid_alive(info.pid), "pid {} alive", info.pid);
    assert!(paths.spawn_lock().exists(), "spawn lock file next to the flock");
    assert!(paths.sock.exists());

    // Second client: same daemon, no spawn line.
    let mut b = Client::spawn(env.cmd(&["attach"]));
    b.wait_stdout("○ ready", 20);
    assert!(!b.err.contains("starting daemon"), "second client must reuse:\n{}", b.err);
    assert_eq!(registry::read_daemon_json(&paths).unwrap().pid, info.pid);

    a.detach();
    b.detach();

    let st = env.cmd(&["daemon", "stop"]).stdin(Stdio::null()).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    assert!(String::from_utf8_lossy(&st.stdout).contains("daemon stopped"));
    wait_alive(&paths, false, 10);
    let t0 = Instant::now();
    while pid_alive(info.pid) && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(info.pid), "daemon pid {} still alive after stop", info.pid);
}

#[test]
fn concurrent_first_clients_spawn_exactly_one_daemon() {
    let env = Env::new();
    let paths = env.paths();
    let mut clients: Vec<Client> = (0..4).map(|_| Client::spawn(env.cmd(&["attach", "--create"]))).collect();
    for c in &mut clients {
        c.wait_stdout("○ ready", 40);
    }
    let spawners = clients.iter().filter(|c| c.err.contains("starting daemon (pid ")).count();
    assert_eq!(spawners, 1, "exactly one client spawns; stderrs:\n{}", clients.iter().map(|c| c.err.clone()).collect::<Vec<_>>().join("---\n"));
    let info = registry::read_daemon_json(&paths).unwrap();
    let announced: Vec<u32> = clients
        .iter()
        .filter_map(|c| c.err.split("starting daemon (pid ").nth(1))
        .filter_map(|s| s.split(')').next()?.parse().ok())
        .collect();
    assert_eq!(announced, vec![info.pid], "announced pid is the registry pid");
    // Every client landed in that daemon: 4 sessions.
    let out = env.cmd(&["daemon", "sessions", "--json"]).stdin(Stdio::null()).output().unwrap();
    let list: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).expect("sessions json");
    assert_eq!(list.len(), 4, "{}", String::from_utf8_lossy(&out.stdout));
    for c in &mut clients {
        c.detach();
    }
    env.stop();
    wait_alive(&paths, false, 10);
}

#[test]
fn autospawn_disabled_prints_no_daemon_message() {
    let env = Env::new();
    let paths = env.paths();
    let out = env
        .cmd(&["attach", "--create"])
        .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no daemon running"), "{err}");
    assert!(err.contains("synaps daemon --detach"), "{err}");
    assert!(!err.contains("starting daemon"), "{err}");
    assert!(!registry::is_alive(&paths));
    assert!(!paths.json.exists() && !paths.sock.exists());
}

#[test]
fn daemon_flag_off_refuses_attach() {
    let env = Env::new();
    let out = env
        .cmd(&["attach", "--create"])
        .env("SYNAPS_DAEMON", "0")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("daemon disabled by SYNAPS_DAEMON=0"), "{err}");
    assert!(!registry::is_alive(&env.paths()));
}

#[test]
fn legacy_mcp_refusal_line_client_exits_3() {
    let env = Env::new();
    env.legacy_mcp();
    let paths = env.paths();
    let out = env.cmd(&["attach", "--create"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("daemon unavailable:"), "{err}");
    assert!(err.contains("progressive_tool_disclosure=false"), "reason is the daemon's one-liner:\n{err}");
    assert!(!err.contains("starting daemon"), "{err}");
    wait_alive(&paths, false, 5);
}

/// `synaps --attach` when the daemon refuses: in-process TUI with the
/// notice, exit 0, and none of the thin-client ladder stages (no re-exec,
/// no allocator diet — review H2).
#[test]
fn legacy_mcp_refusal_tui_falls_back_in_process() {
    let env = Env::new();
    env.legacy_mcp();
    let ladder = env.home.path().join("ladder.log");
    let mut child = env
        .cmd(&["--attach"])
        .env("SYNAPS_MEM_TRACE", "1")
        .env("SYNAPS_MEM_TRACE_FILE", &ladder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let err = reader.join().unwrap();
    assert!(err.contains("daemon unavailable:") && err.contains("running in-process"), "fallback notice:\n{err}");
    assert!(err.contains("progressive_tool_disclosure=false"), "{err}");
    // Exit status is the TUI's own (stdin is /dev/null here, so it may
    // bail on the terminal); the fallback itself is proven by the notice.
    let _ = status;
    let trace = std::fs::read_to_string(&ladder).unwrap_or_default();
    for stage in ["stage=reexec", "stage=main", "stage=alloc"] {
        assert!(!trace.contains(stage), "thin-client ladder stage {stage} on the fallback:\n{trace}");
    }
    assert!(!registry::is_alive(&env.paths()));
}

#[test]
fn status_without_daemon_says_auto_start() {
    let env = Env::new();
    let out = env.cmd(&["daemon", "status"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(so.contains("not running (auto-starts on first --attach)"), "{so}");
    let out = env.cmd(&["daemon", "sessions"]).stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("daemon not running"));
}
