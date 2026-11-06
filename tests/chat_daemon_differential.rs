//! `synaps chat` differential: scripted sessions against the loopback
//! Anthropic stub must print the same stdout, the same (normalised) stderr
//! and exit with the same code as the in-process chat did before chat moved
//! onto the session daemon (PR 5, "daemon is the only mode").
//!
//! The goldens under `tests/fixtures/chat_differential/` were captured from
//! the in-process chat before the switch (`SYNAPS_UPDATE_CHAT_GOLDENS=1`
//! with `SYNAPS_CHAT_DIFF_BIN` = a build of the parent commit); the same
//! harness now runs the daemon-backed chat against them.
//!
//! Every run is hermetic: `env_clear`, a temp HOME / `SYNAPS_BASE_DIR` /
//! `SYNAPS_RUNTIME_DIR`, the stub as `SYNAPS_ANTHROPIC_BASE_URL`, and a
//! `daemon stop --force` on drop, so no test ever reaches a daemon outside
//! its temp dir or leaves one behind.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phase2::Script;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STEP_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const SYSTEM_SENTINEL: &str = "CHATDIFF-SYSTEM-7c1e";
const AGENT_SENTINEL: &str = "CHATDIFF-AGENT-93ab";

/// One `read` of `alpha.txt` — a path RELATIVE to chat's cwd, so the turn
/// also proves the session runs in the client's working directory.
const SSE_READ_TOOL: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_rd\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_rd\",\"name\":\"read\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"alpha.txt\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The binary under test. `SYNAPS_CHAT_DIFF_BIN` points the harness at
/// another build (how the goldens were captured from the in-process chat).
fn bin() -> std::ffi::OsString {
    std::env::var_os("SYNAPS_CHAT_DIFF_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_synaps").into())
}

struct Env {
    home: tempfile::TempDir,
    stub: String,
}

impl Env {
    fn new(stub: String) -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(base.join("agents")).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::write(
            base.join("agents/tester.md"),
            format!("---\nname: tester\n---\nYou are {AGENT_SENTINEL}.\n"),
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        let work = home.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("alpha.txt"), "a\n").unwrap();
        std::fs::write(work.join("beta.txt"), "b\n").unwrap();
        Self { home, stub }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn run_dir(&self) -> PathBuf {
        self.home.path().join("run")
    }

    fn work(&self) -> PathBuf {
        self.home.path().join("work")
    }

    fn std_cmd(&self, args: &[&str]) -> std::process::Command {
        let mut c = std::process::Command::new(bin());
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("SYNAPS_RUNTIME_DIR", self.run_dir())
            .env("SYNAPS_ANTHROPIC_BASE_URL", &self.stub)
            .env("SYNAPS_NO_BOOT_FX", "1")
            .env("TERM", "dumb")
            .current_dir(self.work());
        #[cfg(windows)]
        {
            for k in ["SystemRoot", "SystemDrive", "WINDIR", "ComSpec", "PATHEXT", "TEMP", "TMP"] {
                if let Some(v) = std::env::var_os(k) {
                    c.env(k, v);
                }
            }
            c.env("USERPROFILE", self.home.path());
        }
        c
    }

    fn cmd(&self, args: &[&str]) -> tokio::process::Command {
        tokio::process::Command::from(self.std_cmd(args))
    }

    fn daemon_alive(&self, profile: Option<&str>) -> bool {
        agent_engine::daemon::registry::is_alive(&agent_engine::daemon::registry::daemon_paths_in(&self.run_dir(), profile))
    }

    /// Session journals under `profile` (`<base>/<profile>/sessions`).
    fn journals(&self, profile: Option<&str>) -> usize {
        let dir = match profile {
            Some(p) => self.base().join(p).join("sessions"),
            None => self.base().join("sessions"),
        };
        std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).count())
            .unwrap_or(0)
    }

    /// The chat that just ran was served by `profile`'s daemon: it is up (it
    /// idle-exits seconds after its last client) and the profile journaled
    /// the session; the other profile has neither.
    fn assert_served_by(&self, profile: Option<&str>) {
        let other = if profile.is_some() { None } else { Some("tprof") };
        assert!(self.daemon_alive(profile), "{profile:?}'s daemon served the chat");
        assert!(!self.daemon_alive(other), "{other:?}'s daemon was not started");
        assert!(self.journals(profile) >= 1, "journaled under {profile:?}");
        assert_eq!(self.journals(other), 0, "nothing journaled under {other:?}");
    }

    fn sessions_text(&self) -> String {
        let mut all = String::new();
        if let Ok(rd) = std::fs::read_dir(self.base().join("sessions")) {
            for e in rd.flatten() {
                all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
        all
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leak a setsid'd daemon past the test (default + test profile).
        for args in [
            &["daemon", "stop", "--force"][..],
            &["--profile", "tprof", "daemon", "stop", "--force"][..],
            &["daemon", "--no-extensions", "stop", "--force"][..],
        ] {
            let _ = self.std_cmd(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

#[derive(Clone, Copy)]
enum Step {
    /// Write to chat's stdin.
    Write(&'static str),
    /// Wait until stdout holds `n` occurrences of the needle.
    Out(&'static str, usize),
    /// Wait until stderr holds `n` occurrences of the needle.
    Err(&'static str, usize),
    /// `synaps send <text> --session <this chat's session>`.
    SendEvent(&'static str),
    /// Close stdin (EOF).
    Close,
}

#[derive(Default)]
struct Bufs {
    out: Vec<u8>,
    err: Vec<u8>,
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn count(hay: &[u8], needle: &str) -> usize {
    let hay = String::from_utf8_lossy(hay);
    hay.matches(needle).count()
}

async fn wait_for(bufs: &Arc<Mutex<Bufs>>, stdout: bool, needle: &str, n: usize) {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    loop {
        {
            let b = bufs.lock().unwrap();
            let hay = if stdout { &b.out } else { &b.err };
            if count(hay, needle) >= n {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for {n}× {needle:?} on {}\n--- stdout\n{}\n--- stderr\n{}",
                    if stdout { "stdout" } else { "stderr" },
                    String::from_utf8_lossy(&b.out),
                    String::from_utf8_lossy(&b.err)
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn session_prefix(stderr: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(stderr);
    let line = s.lines().find(|l| l.starts_with("synaps ") && l.contains("| session "))?;
    Some(line.rsplit("| session ").next()?.trim().to_string())
}

async fn run_chat(env: &Env, args: &[&str], steps: &[Step]) -> Run {
    run_chat_with(env, args, &[], steps).await
}

async fn run_chat_with(env: &Env, args: &[&str], vars: &[(&str, &str)], steps: &[Step]) -> Run {
    let mut full = vec!["chat"];
    full.extend_from_slice(args);
    let mut child = env
        .cmd(&full)
        .envs(vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn synaps chat");
    let mut stdin = child.stdin.take();
    let bufs = Arc::new(Mutex::new(Bufs::default()));
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let b1 = Arc::clone(&bufs);
    let t_out = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf).await {
            if n == 0 {
                break;
            }
            b1.lock().unwrap().out.extend_from_slice(&buf[..n]);
        }
    });
    let b2 = Arc::clone(&bufs);
    let t_err = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = err.read(&mut buf).await {
            if n == 0 {
                break;
            }
            b2.lock().unwrap().err.extend_from_slice(&buf[..n]);
        }
    });
    for step in steps {
        match *step {
            Step::Write(s) => {
                let w = stdin.as_mut().expect("stdin open");
                w.write_all(s.as_bytes()).await.unwrap();
                w.flush().await.unwrap();
            }
            Step::Out(n, k) => wait_for(&bufs, true, n, k).await,
            Step::Err(n, k) => wait_for(&bufs, false, n, k).await,
            Step::SendEvent(text) => {
                wait_for(&bufs, false, "| session ", 1).await;
                let sid = session_prefix(&bufs.lock().unwrap().err).expect("session id in banner");
                let st = env
                    .cmd(&["send", text, "--session", &sid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .output()
                    .await
                    .unwrap();
                assert!(st.status.success(), "synaps send failed: {}", String::from_utf8_lossy(&st.stderr));
            }
            Step::Close => drop(stdin.take()),
        }
    }
    drop(stdin.take());
    let status = tokio::time::timeout(EXIT_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| {
            let b = bufs.lock().unwrap();
            panic!(
                "synaps chat did not exit\n--- stdout\n{}\n--- stderr\n{}",
                String::from_utf8_lossy(&b.out),
                String::from_utf8_lossy(&b.err)
            )
        })
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), t_out).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), t_err).await;
    let b = bufs.lock().unwrap();
    Run {
        code: status.code(),
        stdout: String::from_utf8_lossy(&b.out).into_owned(),
        stderr: String::from_utf8_lossy(&b.err).into_owned(),
    }
}

/// Strip ANSI escapes; replace ids, uuids, the version and timestamps.
fn normalise(s: &str) -> String {
    let mut no_ansi = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        no_ansi.push(c);
    }
    let re_uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let re_sid = regex::Regex::new(r"(session |cleared → |session saved: |→ )[0-9a-f]{8}\b").unwrap();
    let re_ts = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap();
    let re_ver = regex::Regex::new(r"^synaps \S+ \|").unwrap();
    let re_hhmm = regex::Regex::new(r"\b\d{2}:\d{2}(:\d{2})?\b").unwrap();
    let re_corr = regex::Regex::new(r"correlation=turn-\d+-(\d+)").unwrap();
    let re_status_sid = regex::Regex::new(r"^session: \S+$").unwrap();
    let mut lines = Vec::new();
    for line in no_ansi.lines() {
        let l = re_uuid.replace_all(line, "<UUID>");
        let l = re_sid.replace_all(&l, "${1}<SID>");
        let l = re_ts.replace_all(&l, "<TS>");
        let l = re_ver.replace_all(&l, "synaps <VER> |");
        let l = re_hhmm.replace_all(&l, "<HH:MM>");
        let l = re_corr.replace_all(&l, "correlation=turn-<PID>-${1}");
        let l = re_status_sid.replace_all(&l, "session: <SID>");
        lines.push(l.trim_end().to_string());
    }
    lines.join("\n")
}

fn render(case: &str, runs: &[Run]) -> String {
    let mut s = format!("# chat differential: {case}\n");
    for (i, r) in runs.iter().enumerate() {
        s.push_str(&format!(
            "## run {i}\nexit: {:?}\n--- stdout\n{}\n--- stderr\n{}\n",
            r.code,
            r.stdout,
            normalise(&r.stderr)
        ));
    }
    s
}

/// The goldens are default-feature Linux runs. Without `command-exec` the
/// `bash`/`shell_*` tool schemas are not sent, and on Windows the tool set
/// and schemas differ, so the context estimates (`[compacted → ~N tokens]`,
/// `/status`'s `context: ~N of …`) differ for in-process and daemon chat
/// alike: mask just those numbers there.
fn feature_mask(s: String) -> String {
    if cfg!(feature = "command-exec") && !cfg!(windows) {
        return s;
    }
    let re = regex::Regex::new(r"~\d+( tokens| of )").unwrap();
    re.replace_all(&s, "~<TOK>${1}").into_owned()
}

fn golden_path(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/chat_differential")
        .join(format!("{case}.golden"))
}

fn check_golden(case: &str, runs: &[Run]) {
    let got = render(case, runs);
    let path = golden_path(case);
    if std::env::var("SYNAPS_UPDATE_CHAT_GOLDENS").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("golden {} missing ({e}); capture with SYNAPS_UPDATE_CHAT_GOLDENS=1", path.display()));
    let (got, want) = (feature_mask(got), feature_mask(want));
    if got != want {
        panic!(
            "chat differential `{case}` diverged from the in-process golden {}\n=== want\n{want}\n=== got\n{got}",
            path.display()
        );
    }
}

async fn one(case: &str, script: Script, args: &[&str], steps: &[Step]) -> (Env, phase2::Bodies) {
    let (url, _hits, bodies) = phase2::spawn_stub(script).await;
    let env = Env::new(url);
    let run = run_chat(&env, args, steps).await;
    check_golden(case, &[run]);
    (env, bodies)
}

fn bodies_text(b: &phase2::Bodies) -> String {
    b.lock().unwrap().iter().map(|x| String::from_utf8_lossy(x).into_owned()).collect::<Vec<_>>().join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_turn() {
    one("plain_turn", Script::Sse(phase2::ANTHROPIC_SSE), &[], &[Step::Write("hello\n"), Step::Close]).await;
}

/// Harbor's exact shape: `printf %s "$task" | synaps chat` — no trailing
/// newline, then EOF. The task must run to completion and chat exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harbor_pipe_without_newline() {
    one(
        "harbor_pipe_without_newline",
        Script::Sse(phase2::ANTHROPIC_SSE),
        &[],
        &[Step::Write("solve the task in /app"), Step::Close],
    )
    .await;
}

/// Several piped lines then EOF: each line is one turn, in order (stdin is
/// read only while the session is idle), then a command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_lines_then_eof() {
    one(
        "piped_lines_then_eof",
        Script::Sse(phase2::ANTHROPIC_SSE),
        &[],
        &[Step::Write("first\nsecond\n/status\n"), Step::Close],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn() {
    one(
        "tool_turn",
        Script::SeqSse(&[SSE_READ_TOOL, phase2::ANTHROPIC_SSE]),
        &[],
        &[Step::Write("read alpha\n"), Step::Close],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact() {
    one(
        "compact",
        Script::SseOrJson {
            sse: phase2::ANTHROPIC_SSE,
            json: phase2::ANTHROPIC_MESSAGES_JSON,
            json_delay: Duration::from_millis(0),
        },
        &[],
        &[
            Step::Write("hello\n"),
            Step::Out("hi", 1),
            Step::Write("/compact\n"),
            Step::Err("[compact", 1),
            Step::Close,
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_help_unknown_quit() {
    one(
        "clear_help_unknown_quit",
        Script::Sse(phase2::ANTHROPIC_SSE),
        &[],
        &[
            Step::Write("hello\n"),
            Step::Out("hi", 1),
            Step::Write("/clear\n"),
            Step::Err("session cleared", 1),
            Step::Write("/help\n/nonsense\n/quit\n"),
        ],
    )
    .await;
}

/// C4a/C4b: an event delivered to the idle session wakes chat and runs an
/// auto-turn without any stdin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_auto_turn() {
    one(
        "event_auto_turn",
        Script::Sse(phase2::ANTHROPIC_SSE),
        &[],
        &[
            Step::Write("hello\n"),
            Step::Out("hi", 1),
            Step::SendEvent("ping from the differential"),
            Step::Out("hi", 2),
            Step::Close,
        ],
    )
    .await;
}

/// An unrecovered provider failure: exit non-zero, the user message kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_error() {
    let (env, _) = one(
        "provider_error",
        Script::AlwaysFailEcho(400),
        &[],
        &[Step::Write("CHATDIFF-PROVIDER-ERR probe\n"), Step::Close],
    )
    .await;
    assert!(env.sessions_text().contains("CHATDIFF-PROVIDER-ERR probe"), "history lost");
}

/// `-c <id>` (`--continue-session`) resumes that session; `--system` and `--agent` reach
/// the provider request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_system_agent() {
    let (url, _hits, bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let first = run_chat(&env, &["--system", SYSTEM_SENTINEL], &[Step::Write("hello\n"), Step::Close]).await;
    let sid = session_prefix(first.stderr.as_bytes()).expect("session id in banner");
    let second = run_chat(&env, &["-c", &sid], &[Step::Write("again\n"), Step::Close]).await;
    let third = run_chat(&env, &["--agent", "tester"], &[Step::Write("as agent\n"), Step::Close]).await;
    check_golden("continue_system_agent", &[first, second, third]);
    let all = bodies_text(&bodies);
    assert!(all.contains(SYSTEM_SENTINEL), "--system never reached the provider");
    assert!(all.contains(AGENT_SENTINEL), "--agent prompt never reached the provider");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_and_no_extensions() {
    let (url, _hits, _bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let p = run_chat(&env, &["--profile", "tprof"], &[Step::Write("hello\n"), Step::Close]).await;
    env.assert_served_by(Some("tprof"));
    let n = run_chat(&env, &["--no-extensions"], &[Step::Write("hello\n"), Step::Close]).await;
    check_golden("profile_and_no_extensions", &[p, n]);
}

/// `SYNAPS_PROFILE=tprof` with no flag is the tprof profile, as for every
/// other client: its daemon serves the session (and journals it under the
/// profile), also when the default profile's daemon is already up. Same
/// output as the in-process `plain_turn` golden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_from_env_uses_its_daemon() {
    let (url, _hits, _bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let steps = [Step::Write("hello\n"), Step::Close];
    // No daemon at all: the tprof one is started and dialled.
    let run = run_chat_with(&env, &[], &[("SYNAPS_PROFILE", "tprof")], &steps).await;
    env.assert_served_by(Some("tprof"));
    check_golden("plain_turn", &[run]);
    // The default daemon up (a plain chat): SYNAPS_PROFILE still wins.
    let _ = run_chat(&env, &[], &steps).await;
    assert!(env.daemon_alive(None), "the default chat started the default daemon");
    let run = run_chat_with(&env, &[], &[("SYNAPS_PROFILE", "tprof")], &steps).await;
    check_golden("plain_turn", &[run]);
    assert_eq!(env.journals(Some("tprof")), 2, "both tprof chats journaled under tprof");
    assert_eq!(env.journals(None), 1, "only the plain chat journaled under the default profile");
}

// ── daemon-only behaviour (no in-process golden: these did not exist) ──────

/// A daemon that is exiting (idle exit, `daemon stop`) has stopped
/// accepting but still holds its lock for a few seconds, so the first
/// ensure says it is running and the dial fails. chat waits for the lock
/// to go, starts a fresh daemon and runs the turn as usual (chats run
/// 10–15 s apart hit an idle exit). Here the test holds the profile's
/// daemon lock with no socket behind it and lets go after 2 s; the output
/// is the plain turn's, exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_exiting_under_the_dial_is_waited_out() {
    use agent_engine::daemon::registry;
    let (url, _hits, _bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let paths = registry::daemon_paths_in(&env.run_dir(), None);
    std::fs::create_dir_all(&paths.dir).unwrap();
    let dying = registry::DaemonLock::try_acquire(&paths).unwrap().expect("the lock is free");
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        drop(dying);
    });
    let run = run_chat(&env, &[], &[Step::Write("hello\n"), Step::Close]).await;
    release.join().unwrap();
    assert!(env.daemon_alive(None), "a fresh daemon served the chat");
    check_golden("plain_turn", &[run]);
}

/// Killing `synaps chat` mid-turn (SIGKILL: no End, no Bye) cancels the
/// turn and ends the owned session in the daemon; the journal keeps the
/// partial turn and the interruption marker.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_mid_turn_ends_the_owned_session() {
    use agent_engine::session::socket_transport::SocketTransport;
    let (url, hits, _b) = phase2::spawn_stub(Script::Endless(phase2::ANTHROPIC_SSE_PREFIX)).await;
    let env = Env::new(url);
    let mut child = env
        .cmd(&["chat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"CHATDIFF-KILL never finishes\n").await.unwrap();
    stdin.flush().await.unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut first = [0u8; 1];
    tokio::time::timeout(STEP_TIMEOUT, out.read_exact(&mut first)).await.expect("partial reply").unwrap();
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) >= 1);

    let sock = agent_engine::daemon::registry::daemon_paths_in(&env.run_dir(), None).sock;
    let live = SocketTransport::sessions(&sock).await.expect("daemon lists sessions");
    assert_eq!(live.len(), 1, "chat's session is live in the daemon");
    child.start_kill().unwrap();
    let _ = child.wait().await;

    let t0 = std::time::Instant::now();
    while !SocketTransport::sessions(&sock).await.unwrap_or_default().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(10), "owned session outlived its killed owner");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let journal = env.sessions_text();
    assert!(journal.contains("CHATDIFF-KILL never finishes"), "user turn kept: {journal}");
    assert!(
        journal.contains(agent_engine::engine::interrupt::InterruptReason::User.marker()),
        "the turn was cancelled (interruption marker): {journal}"
    );
}

/// No daemon and autospawn off: a clear error naming the fix, non-zero
/// exit, and no in-process session (nothing journaled, no provider call).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_unavailable_is_a_clear_error() {
    let (url, hits, _b) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let mut c = env.cmd(&["chat"]);
    c.env("SYNAPS_DAEMON_AUTOSPAWN", "0").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello\n").await.unwrap();
    drop(stdin);
    let out = tokio::time::timeout(EXIT_TIMEOUT, child.wait_with_output()).await.unwrap().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(agent_engine::daemon::EXIT_REFUSED), "stderr: {err}");
    assert!(err.contains("no daemon running") && err.contains("SYNAPS_DAEMON_AUTOSPAWN"), "stderr: {err}");
    assert!(err.contains("no in-process mode"), "stderr: {err}");
    assert!(err.contains("daemon --detach"), "names the fix: {err}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "no in-process turn ran");
    assert!(env.sessions_text().is_empty(), "no in-process session was journaled");
}

/// Install `tests/fixtures/slow_session_start_extension.py` (logs every
/// `on_session_start` to `session-start.jsonl` in its plugin dir and injects
/// a sentinel into the session's context).
#[cfg(unix)]
fn install_session_start_plugin(env: &Env) -> PathBuf {
    let dir = env.base().join("plugins/start-probe");
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(dir.join("extensions")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slow_session_start_extension.py"),
        dir.join("extensions/slow_session_start_extension.py"),
    )
    .unwrap();
    let manifest = serde_json::json!({
        "name": "start-probe",
        "version": "0.1.0",
        "extension": {
            "protocol_version": 1,
            "runtime": "process",
            "command": "python3",
            "args": ["extensions/slow_session_start_extension.py"],
            "permissions": ["session.lifecycle"],
            "hooks": [{"hook": "on_session_start"}]
        }
    });
    std::fs::write(dir.join(".synaps-plugin/plugin.json"), serde_json::to_string_pretty(&manifest).unwrap()).unwrap();
    dir
}

#[cfg(unix)]
async fn one_piped_turn(env: &Env, args: &[&str], text: &str) -> std::process::Output {
    let mut child = env
        .cmd(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(text.as_bytes()).await.unwrap();
    drop(stdin);
    tokio::time::timeout(EXIT_TIMEOUT, child.wait_with_output()).await.expect("chat exits").unwrap()
}

/// `--no-extensions` runs over the daemon: chat uses the profile's private
/// no-extensions daemon, where no extension is loaded, so no hook runs for
/// that session. Control: the same plugin's hook runs (and injects) for a
/// plain `synaps chat` on the profile daemon.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_extensions_runs_no_extension_hooks() {
    use agent_engine::session::socket_transport::SocketTransport;
    let (url, _hits, bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let plugin = install_session_start_plugin(&env);
    let log = plugin.join("session-start.jsonl");
    let sentinel = "SLOW-START-SENTINEL";

    let out = one_piped_turn(&env, &["chat", "--no-extensions"], "CHATDIFF-NOEXT one\n").await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    let noext = agent_engine::daemon::registry::daemon_paths_in_for(&env.run_dir(), None, true);
    assert!(agent_engine::daemon::registry::is_alive(&noext), "the private no-extensions daemon serves it");
    let probe = SocketTransport::ping(&noext.sock).await;
    assert!(probe.is_ok(), "{:?}", probe.err());
    let profile_daemon = agent_engine::daemon::registry::daemon_paths_in(&env.run_dir(), None);
    assert!(!agent_engine::daemon::registry::is_alive(&profile_daemon), "the profile daemon was not started");
    assert!(!log.exists(), "on_session_start ran for a --no-extensions session: {:?}", std::fs::read_to_string(&log));
    {
        let b = bodies.lock().unwrap();
        let body = String::from_utf8_lossy(b.last().expect("one provider request"));
        assert!(body.contains("CHATDIFF-NOEXT one"), "{body}");
        assert!(!body.contains(sentinel), "a hook injected into a --no-extensions session: {body}");
    }

    // Control: without the flag the same plugin's hook runs and injects.
    let out = one_piped_turn(&env, &["chat"], "CHATDIFF-NOEXT two\n").await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {err}");
    assert!(agent_engine::daemon::registry::is_alive(&profile_daemon));
    let t0 = std::time::Instant::now();
    while !log.exists() {
        assert!(t0.elapsed() < Duration::from_secs(10), "control: the hook never ran (fixture broken?)");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let b = bodies.lock().unwrap();
    let body = String::from_utf8_lossy(b.last().unwrap());
    assert!(body.contains("CHATDIFF-NOEXT two") && body.contains(sentinel), "control injected: {body}");
}

/// `phase2::ANTHROPIC_SSE` with its one text delta replaced by `deltas`
/// deltas of `text` each.
fn flood_sse(deltas: usize, text: &str) -> String {
    let frames: Vec<&str> = phase2::ANTHROPIC_SSE.split_inclusive("\n\n").collect();
    assert!(frames[2].contains("text_delta"), "fixture shape changed");
    let delta = format!(
        "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    );
    let mut body = frames[..2].concat();
    body.push_str(&delta.repeat(deltas));
    body.push_str(&frames[3..].concat());
    body
}

/// A blocked stdout must not cost the session. The daemon ends an owned
/// session whose owner stops reading (PR 3: lag → `Bye{owner_lagged}`), so
/// chat drains the socket into its own output queue and never makes the
/// daemon wait on the terminal (a paused terminal, `| less`). Here nobody
/// reads chat's stdout while a reply far larger than the pipe and socket
/// buffers (3 MB in 1500 deltas) streams at a provider-like pace through a
/// 256-event session buffer; once read, all of it is there and chat exits
/// 0. (A chat that writes on its reading path blocks on the full pipe and
/// is disconnected: `lost the session`, exit 4. The cap is cut from 1024 to
/// keep the run short; it is not tighter, so a busy test host cannot lag a
/// chat that keeps reading.)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_stdout_does_not_lag_the_owner_off() {
    const DELTAS: usize = 1500;
    const WIDTH: usize = 2000;
    let body: &'static str = Box::leak(flood_sse(DELTAS, &"x".repeat(WIDTH)).into_boxed_str());
    let (url, _hits, _b) =
        phase2::spawn_stub(Script::Paced { body, frame_delay: Duration::from_millis(1) }).await;
    let env = Env::new(url);
    let mut child = env
        .cmd(&["chat"])
        .env("SYNAPS_SESSION_EVENTS_CAP", "256")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"flood\n").await.unwrap();
    drop(stdin);
    let mut err = child.stderr.take().unwrap();
    let err_task = tokio::spawn(async move {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s).await;
        s
    });
    // Nobody reads stdout while the reply streams (~1.5-3 s at 1 ms/frame).
    tokio::time::sleep(Duration::from_secs(8)).await;
    let mut out = child.stdout.take().unwrap();
    let mut stdout = Vec::new();
    tokio::time::timeout(EXIT_TIMEOUT, out.read_to_end(&mut stdout)).await.expect("stdout closes").unwrap();
    let status = tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await.expect("chat exits").unwrap();
    let stderr = String::from_utf8_lossy(&err_task.await.unwrap()).into_owned();
    assert!(status.success(), "exit {status:?}\nstderr: {stderr}");
    assert!(!stderr.contains("lost the session") && !stderr.contains("dropped"), "{stderr}");
    let text = String::from_utf8_lossy(&stdout);
    assert_eq!(text.matches('x').count(), DELTAS * WIDTH, "every delta reached stdout");
}

/// stdin EOF right behind a line whose reply streams slowly: chat finishes
/// the turn, prints all of it, ends the session cleanly (`End` → `Ended`)
/// and exits 0. EOF is read only while the session is idle, so it never
/// cancels a turn in flight, and nothing the session sends is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_behind_a_slow_turn_prints_all_of_it() {
    const DELTAS: usize = 30;
    let body: &'static str = Box::leak(flood_sse(DELTAS, "y").into_boxed_str());
    let (url, _hits, _b) =
        phase2::spawn_stub(Script::Paced { body, frame_delay: Duration::from_millis(50) }).await;
    let env = Env::new(url);
    let run = run_chat(&env, &[], &[Step::Write("go\n"), Step::Close]).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.matches('y').count(), DELTAS, "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    assert!(run.stderr.contains("session saved"), "{}", run.stderr);
    let journal = env.sessions_text();
    assert!(journal.contains(&"y".repeat(DELTAS)), "the whole reply was journaled: {journal}");
}

/// A chat whose session the daemon ends while it sits idle at the prompt
/// (`daemon stop` → `Ended{HostShutdown}`), or whose daemon dies outright,
/// says so and exits 4; it never hangs and never reports a clean exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_gone_under_an_idle_chat_exits_4() {
    let (url, _hits, _b) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    for kill in [false, true] {
        if kill && !cfg!(unix) {
            continue;
        }
        let mut child = env
            .cmd(&["chat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"hello\n").await.unwrap();
        stdin.flush().await.unwrap();
        let mut err = child.stderr.take().unwrap();
        let err_task = tokio::spawn(async move {
            let mut s = Vec::new();
            let _ = err.read_to_end(&mut s).await;
            s
        });
        let mut out = child.stdout.take().unwrap();
        let mut got = Vec::new();
        tokio::time::timeout(STEP_TIMEOUT, async {
            let mut buf = [0u8; 256];
            while !String::from_utf8_lossy(&got).contains("hi") {
                let n = out.read(&mut buf).await.unwrap();
                assert!(n > 0, "stdout closed before the reply");
                got.extend_from_slice(&buf[..n]);
            }
        })
        .await
        .expect("the reply");
        tokio::time::sleep(Duration::from_millis(300)).await; // idle at the prompt
        let paths = agent_engine::daemon::registry::daemon_paths_in(&env.run_dir(), None);
        if kill {
            let pid = agent_engine::daemon::registry::read_daemon_json(&paths).expect("daemon.json").pid;
            let st = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
            assert!(st.success());
        } else {
            let st = env.cmd(&["daemon", "stop"]).stdout(Stdio::null()).stderr(Stdio::null()).status().await.unwrap();
            assert!(st.success(), "daemon stop");
        }
        let status = tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await.expect("chat exits, never hangs").unwrap();
        let stderr = String::from_utf8_lossy(&err_task.await.unwrap()).into_owned();
        assert_eq!(status.code(), Some(4), "kill={kill}: {stderr}");
        if kill {
            assert!(stderr.contains("synaps chat: lost the session"), "{stderr}");
        } else {
            assert!(stderr.contains("synaps chat: the session ended under chat: the daemon is shutting down"), "{stderr}");
        }
        assert!(stderr.contains("synaps chat --continue"), "{stderr}");
        assert!(!stderr.contains("session saved"), "not a clean exit: {stderr}");
        drop(stdin);
        // The next chat starts a fresh daemon.
        let t0 = std::time::Instant::now();
        while agent_engine::daemon::registry::is_alive(&paths) {
            assert!(t0.elapsed() < Duration::from_secs(10), "daemon still alive");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// `synaps daemon reload` under an idle chat: chat re-binds its owned
/// session with its token (PR 3), the next line is answered in the same
/// conversation, and EOF still ends it cleanly (exit 0). Unix: the
/// Windows reload hands off to a new pid (daemon_owned is Unix-only too).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_reload_under_an_idle_chat_rebinds() {
    let (url, _hits, bodies) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let mut child = env
        .cmd(&["chat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let err_task = tokio::spawn(async move {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s).await;
        s
    });
    let mut got = Vec::new();
    async fn replies(out: &mut tokio::process::ChildStdout, got: &mut Vec<u8>, n: usize) {
        let mut buf = [0u8; 256];
        while String::from_utf8_lossy(got).matches("hi").count() < n {
            let k = out.read(&mut buf).await.unwrap();
            assert!(k > 0, "stdout closed before reply {n}");
            got.extend_from_slice(&buf[..k]);
        }
    }
    stdin.write_all(b"CHATDIFF-RELOAD-1\n").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::timeout(STEP_TIMEOUT, replies(&mut out, &mut got, 1)).await.expect("first reply");
    tokio::time::sleep(Duration::from_millis(300)).await; // idle at the prompt

    let st = env.cmd(&["daemon", "reload", "--now"]).stdout(Stdio::null()).stderr(Stdio::null()).status().await.unwrap();
    assert!(st.success(), "daemon reload");
    stdin.write_all(b"CHATDIFF-RELOAD-2\n").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::timeout(STEP_TIMEOUT, replies(&mut out, &mut got, 2)).await.expect("reply after the reload");
    drop(stdin);
    let status = tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await.expect("chat exits").unwrap();
    let stderr = String::from_utf8_lossy(&err_task.await.unwrap()).into_owned();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("session saved"), "{stderr}");
    assert!(!stderr.contains("lost the session"), "{stderr}");
    let second: Vec<String> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|x| String::from_utf8_lossy(x).into_owned())
        .filter(|b| b.contains("CHATDIFF-RELOAD-2"))
        .collect();
    assert!(
        !second.is_empty() && second.iter().all(|b| b.contains("CHATDIFF-RELOAD-1")),
        "the turn after the reload carries the conversation: {}",
        bodies_text(&bodies)
    );
}

/// chat's session is owned, so PR 3 refuses `/keepwarm <mode>`: the
/// refusal prints as an error line and chat carries on (exit 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keepwarm_is_refused_for_the_owned_session() {
    let (url, _hits, _b) = phase2::spawn_stub(Script::Sse(phase2::ANTHROPIC_SSE)).await;
    let env = Env::new(url);
    let run = run_chat(
        &env,
        &[],
        &[Step::Write("/keepwarm auto\n"), Step::Err("never keeps", 1), Step::Write("hello\n"), Step::Out("hi", 1), Step::Close],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("error: an owned session (bound to the client that created it) never keeps"), "{}", run.stderr);
    assert!(run.stderr.contains("session saved"), "{}", run.stderr);
}
