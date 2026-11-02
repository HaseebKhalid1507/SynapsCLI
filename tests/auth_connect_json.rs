//! Subprocess contract tests for `synaps auth connect --json` (accounts
//! contract §5) on the real binary. Each process gets an empty environment, a
//! private temp SYNAPS_BASE_DIR/HOME, a fake browser opener that must never
//! run, and dead proxies. No provider is contacted: these cover the
//! pre-start refusals and the stdin-EOF / SIGTERM exits before `exchanging`
//! (the full flows run against a fake provider in agent-core's unit tests).
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const CLAUDE_PORT: u16 = 53692;
const CODEX_PORT: u16 = 1455;

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = dir.path().join("browser-opened");
        for opener in ["xdg-open", "open", "sensible-browser", "x-www-browser", "gio", "wslview"] {
            let path = bin.join(opener);
            std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            make_executable(&path);
        }
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn store(&self) -> PathBuf {
        self.path().join("auth.json")
    }

    fn seed(&self, data: Value) -> Vec<u8> {
        let bytes = serde_json::to_vec_pretty(&data).unwrap();
        std::fs::write(self.store(), &bytes).unwrap();
        bytes
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_synaps"));
        let bin = self.path().join("bin");
        let opener = bin.join("xdg-open");
        cmd.env_clear()
            .env("HOME", self.path())
            .env("SYNAPS_BASE_DIR", self.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("BROWSER", &opener)
            .env("TERM", "dumb")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .current_dir(self.path())
            .args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn assert_no_browser(&self) {
        assert!(!self.path().join("browser-opened").exists(), "a browser opener ran");
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

fn slot(seat: &str) -> Value {
    json!({"type":"oauth","access":"at-old","refresh":"rt-old",
        "expires":1_700_000_000_000_u64,"accountId":seat,"addedAt":1_700_000_000_u64})
}

fn seat_fingerprint(provider: &str, seat: &str) -> String {
    let provider: synaps_cli::auth::OAuthProviderId = provider.parse().unwrap();
    synaps_cli::auth::seat_fingerprint(provider, seat).unwrap()
}

/// A running `synaps auth connect --json` with its stdout lines on a channel.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: std::thread::JoinHandle<String>,
    seen: Vec<String>,
}

impl Running {
    fn spawn(sandbox: &Sandbox, args: &[&str]) -> Self {
        let mut child = sandbox
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
            text
        });
        Self { child, stdin, lines, stderr, seen: Vec::new() }
    }

    /// Next stdout line (10 s bound).
    fn next_event(&mut self) -> Value {
        let line = self.lines.recv_timeout(Duration::from_secs(10)).expect("no stdout line");
        self.seen.push(line.clone());
        serde_json::from_str(&line).unwrap()
    }

    fn wait_for(&mut self, event: &str) -> Value {
        loop {
            let value = self.next_event();
            if value["event"] == event {
                return value;
            }
        }
    }

    /// Wait for the exit within `bound`; returns (code, elapsed, transcript, stderr).
    fn exit_within(mut self, bound: Duration) -> (i32, Duration, String, String) {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > Duration::from_secs(10) {
                let _ = self.child.kill();
                panic!("connect did not exit");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let elapsed = start.elapsed();
        assert!(elapsed < bound, "exit took {elapsed:?} (bound {bound:?})");
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.seen.push(line);
        }
        let mut transcript = self.seen.join("\n");
        transcript.push('\n');
        let stderr = self.stderr.join().unwrap();
        (status.code().unwrap_or(-1), elapsed, transcript, stderr)
    }
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/accounts_v1").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Replace the value after every `marker` (up to the first `stop` char).
fn redact(text: &str, marker: &str, stop: &[char], with: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let (head, tail) = rest.split_at(at + marker.len());
        out.push_str(head);
        out.push_str(with);
        let end = tail.find(stop).unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn normalized(text: &str) -> String {
    let text = redact(text, "\"expires_at_ms\":", &[',', '}'], "1800000600000");
    let text = redact(&text, "code_challenge=", &['&', '"'], "fixture");
    redact(&text, "&state=", &['&', '"'], "fixture")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn assert_port_free(port: u16) {
    for _ in 0..100 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("callback port {port} still bound after exit");
}

fn param<'a>(url: &'a str, name: &str) -> &'a str {
    let query = url.split_once('?').unwrap().1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
        .unwrap()
}

// ── Pre-start refusals: only the done line, byte-exact ──────────────────────

#[test]
#[serial_test::serial]
fn pre_start_refusals_print_only_the_done_line() {
    let cases: [(&str, &[&str], Option<Value>, &str); 8] = [
        ("bad label", &["--provider", "anthropic", "--account", "Work"], None, "connect_bad_label.jsonl"),
        ("reserved label", &["--provider", "anthropic", "--account", "auto"], None, "connect_bad_label.jsonl"),
        ("padded label", &["--provider", "anthropic", "--account", " work"], None, "connect_bad_label.jsonl"),
        ("provider", &["--provider", "kimi-code", "--account", "work"], None, "connect_unsupported_provider.jsonl"),
        ("alias", &["--provider", "claude", "--account", "work"], None, "connect_unsupported_provider.jsonl"),
        (
            "taken slot",
            &["--provider", "anthropic", "--account", "work"],
            Some(json!({"anthropic@work": slot("acct-a")})),
            "connect_slot_exists.jsonl",
        ),
        ("missing slot", &["--provider", "anthropic", "--account", "work", "--expect-seat", "none"], None, "connect_not_found.jsonl"),
        (
            "stale fingerprint",
            &["--provider", "openai-codex", "--account", "default", "--expect-seat", "none"],
            Some(json!({"openai-codex": slot("acct_codex_0001")})),
            "connect_conflict.jsonl",
        ),
    ];
    for (what, args, store, name) in cases {
        let sandbox = Sandbox::new();
        let before = store.map(|data| sandbox.seed(data));
        let mut full = vec!["auth", "connect"];
        full.extend_from_slice(args);
        full.push("--json");
        let output = sandbox.run(&full);
        assert_eq!(output.status.code(), Some(1), "{what}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), fixture(name), "{what}");
        match before {
            Some(bytes) => assert_eq!(std::fs::read(sandbox.store()).unwrap(), bytes, "{what}"),
            None => assert!(!sandbox.store().exists(), "{what}"),
        }
        sandbox.assert_no_browser();
    }
}

#[test]
#[serial_test::serial]
fn an_inherited_profile_store_is_refused() {
    let sandbox = Sandbox::new();
    let before = sandbox.seed(json!({"anthropic": slot("acct-a")}));
    let output = sandbox.run(&[
        "--profile", "work", "auth", "connect", "--provider", "anthropic", "--account", "claude3", "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), fixture("connect_refused_profile.jsonl"));
    assert_eq!(std::fs::read(sandbox.store()).unwrap(), before);
    assert!(!sandbox.path().join("work/auth.json").exists());
}

#[test]
#[serial_test::serial]
fn without_json_it_refuses_with_nothing_on_stdout() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["auth", "connect", "--provider", "anthropic", "--account", "claude3"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(!sandbox.store().exists());
    sandbox.assert_no_browser();
}

#[test]
#[serial_test::serial]
fn usage_errors_are_exit_2() {
    let sandbox = Sandbox::new();
    for args in [
        &["auth", "connect", "--account", "claude3", "--json"][..],
        &["auth", "connect", "--provider", "anthropic", "--json"][..],
        &["auth", "connect", "--provider", "anthropic", "--account", "x", "--timeout", "-5", "--json"][..],
    ] {
        let output = sandbox.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

// ── Parent death (stdin EOF) and SIGTERM before `exchanging` ─────────────────

#[test]
#[serial_test::serial]
fn stdin_eof_before_exchanging_exits_promptly_cancelled_and_frees_the_port() {
    for (provider, port, allow) in [
        ("anthropic", CLAUDE_PORT, "https://claude.ai/oauth/authorize"),
        ("openai-codex", CODEX_PORT, "https://auth.openai.com/oauth/authorize"),
    ] {
        let sandbox = Sandbox::new();
        let before_ms = now_ms();
        let mut run = Running::spawn(
            &sandbox,
            &["auth", "connect", "--provider", provider, "--account", "claude3", "--json"],
        );
        let started = run.next_event();
        assert_eq!(started["event"], "started");
        assert_eq!(started["mode"], "add");
        let expires = started["expires_at_ms"].as_u64().unwrap();
        assert!(expires >= before_ms + 600_000 && expires <= now_ms() + 600_000, "{provider}");
        let open = run.next_event();
        assert_eq!(open["allow"], allow);
        let url = open["url"].as_str().unwrap().to_string();
        assert!(url.starts_with(&format!("{allow}?")));
        // The listener is up while the flow waits for the browser.
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err(), "{provider}");
        // Bytes on stdin are never a pasted code; only EOF matters.
        let mut stdin = run.stdin.take().unwrap();
        stdin.write_all(format!("c0de#{}\n", param(&url, "state")).as_bytes()).unwrap();
        stdin.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(stdin);
        let (code, _, transcript, stderr) = run.exit_within(Duration::from_secs(1));
        assert_eq!(code, 1, "{provider}");
        assert_port_free(port);
        let lines: Vec<Value> = transcript.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3, "{transcript}");
        assert_eq!(lines[2], json!({"v":1,"event":"done","ok":false,"reason":"cancelled"}));
        if provider == "anthropic" {
            assert_eq!(normalized(&transcript), fixture("connect_cancelled.jsonl"));
        }
        // No leak: stderr never carries the URL, its state or its challenge.
        assert!(!stderr.contains(&url) && !stderr.contains(param(&url, "state")), "{provider}");
        assert!(!stderr.contains(param(&url, "code_challenge")), "{provider}");
        assert!(!stderr.contains("authorize"), "{provider}");
        assert!(!sandbox.store().exists(), "{provider}");
        sandbox.assert_no_browser();
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn sigterm_before_exchanging_cancels_and_frees_the_port() {
    let sandbox = Sandbox::new();
    let mut run = Running::spawn(
        &sandbox,
        &["auth", "connect", "--provider", "anthropic", "--account", "claude3", "--json"],
    );
    let open = run.wait_for("open_url");
    let url = open["url"].as_str().unwrap().to_string();
    let pid = run.child.id().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(killed.success());
    let (code, _, transcript, stderr) = run.exit_within(Duration::from_secs(2));
    assert_eq!(code, 1);
    assert_eq!(normalized(&transcript), fixture("connect_cancelled.jsonl"));
    assert_port_free(CLAUDE_PORT);
    assert!(!stderr.contains(&url) && !stderr.contains(param(&url, "state")));
    assert!(!sandbox.store().exists());
}

#[test]
#[serial_test::serial]
fn the_timeout_is_clamped_to_at_least_30_seconds() {
    let sandbox = Sandbox::new();
    let before_ms = now_ms();
    let mut run = Running::spawn(
        &sandbox,
        &["auth", "connect", "--provider", "anthropic", "--account", "claude3", "--timeout", "1", "--json"],
    );
    let started = run.next_event();
    let expires = started["expires_at_ms"].as_u64().unwrap();
    assert!(expires >= before_ms + 30_000 && expires <= now_ms() + 30_000, "{expires}");
    run.wait_for("open_url");
    drop(run.stdin.take());
    let (code, ..) = run.exit_within(Duration::from_secs(1));
    assert_eq!(code, 1);
    assert_port_free(CLAUDE_PORT);
}

#[test]
#[serial_test::serial]
fn a_reauth_announces_its_mode_and_saves_nothing_when_cancelled() {
    let sandbox = Sandbox::new();
    let before = sandbox.seed(json!({"openai-codex": slot("acct_codex_0001")}));
    let want = seat_fingerprint("openai-codex", "acct_codex_0001");
    let mut run = Running::spawn(
        &sandbox,
        &["auth", "connect", "--provider", "openai-codex", "--account", "default", "--expect-seat", &want, "--json"],
    );
    let started = run.next_event();
    assert_eq!(started["provider"], "openai-codex");
    assert_eq!(started["account"], "default");
    assert_eq!(started["mode"], "reauth");
    assert_eq!(started["flow"], "browser");
    run.wait_for("open_url");
    drop(run.stdin.take());
    let (code, ..) = run.exit_within(Duration::from_secs(1));
    assert_eq!(code, 1);
    assert_eq!(std::fs::read(sandbox.store()).unwrap(), before);
    assert_port_free(CODEX_PORT);
}

#[test]
#[serial_test::serial]
fn a_closed_stdout_exits_1_without_a_panic() {
    let sandbox = Sandbox::new();
    let mut child = sandbox
        .command(&["auth", "connect", "--provider", "anthropic", "--account", "claude3", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    std::thread::sleep(Duration::from_millis(200));
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("authorize"), "{stderr}");
    assert!(!sandbox.store().exists());
    assert_port_free(CLAUDE_PORT);
}

/// Every `connect_*.jsonl` fixture is pinned by a test (here or in
/// agent-core's `core::auth::connect` tests); a new outcome needs both.
#[test]
fn the_connect_fixture_set_is_closed() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/accounts_v1");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("connect_"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "connect_bad_label.jsonl",
            "connect_cancelled.jsonl",
            "connect_conflict.jsonl",
            "connect_denied.jsonl",
            "connect_duplicate_seat.jsonl",
            "connect_identity_changed.jsonl",
            "connect_not_found.jsonl",
            "connect_ok_add.jsonl",
            "connect_ok_reauth_codex.jsonl",
            "connect_port_busy.jsonl",
            "connect_refused_profile.jsonl",
            "connect_slot_exists.jsonl",
            "connect_state_mismatch.jsonl",
            "connect_timeout.jsonl",
            "connect_unsupported_provider.jsonl",
        ]
    );
    for name in &names {
        let text = fixture(name);
        assert!(text.ends_with('\n') && !text.contains('\r'), "{name}");
        let lines: Vec<&str> = text.lines().collect();
        let done = lines.iter().filter(|l| l.contains("\"event\":\"done\"")).count();
        assert_eq!(done, 1, "{name}");
        assert!(lines.last().unwrap().contains("\"event\":\"done\""), "{name}");
        for line in lines {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["v"], 1, "{name}");
            assert!(line.len() <= 16 * 1024, "{name}");
        }
    }
}
