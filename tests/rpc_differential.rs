//! Differential test for `synaps rpc`: the stdout frames of scripted
//! scenarios against the loopback Anthropic stub must match golden
//! fixtures captured from the pre-daemon in-process loop (`src/cmd/rpc.rs`
//! before PR 4 of "daemon is the only mode"). The thin daemon-client rpc
//! must produce the same frames, in the same order.
//!
//! Fixtures: `tests/fixtures/rpc_golden/<scenario>.jsonl`, one normalised
//! frame per line, plus a final `{"_exit": <code>}` line where the scenario
//! ends the process. Regenerate with `SYNAPS_RPC_GOLDEN_UPDATE=1` (only
//! ever from a build whose behaviour is the reference).
//!
//! Normalisation (the ONLY differences tolerated), see [`Norm`]:
//! - ids minted at run time: session ids (from `ready` / `new_session`) →
//!   `<SID0>`, `<SID1>`…; external event ids (from `event` frames, also in
//!   `auto:<id>` correlation ids) → `<EVT0>`…;
//! - the temp HOME path → `<HOME>`;
//! - timestamps: values of keys `timestamp`, `ts`, `created_at`,
//!   `updated_at` → `<TS>`;
//! - the per-turn `outcome.correlation_id` (a minted id) → `<CORR>`;
//! - durations: `duration_secs` → `0`.
//!
//! Intentional differences (see [`intentional`], applied to both sides):
//! - `get_session_stats.cost`: the in-process loop never accumulated cost
//!   (it reported 0.0, or the cost a `--continue`d session was saved with);
//!   the session actor reports the real session cost from the pricing
//!   table. The fixture's value is replaced by the exact price of the
//!   stub's tokens ([`priced_cost`], computed by hand from the published
//!   per-token prices), which the frame must carry. Token counts, which
//!   billing reads, are compared exactly.
//!
//! Build note: the fixtures are from the default build. Without
//! `command-exec` the binary has no `bash`/`powershell`/`shell_*` tools, so
//! `tools_list` drops those from the fixture before comparing (and the tool
//! scenario does not run). Fixtures are only written from a default build.
//!
//! Every test spawns real processes under one temp HOME with its own
//! `SYNAPS_BASE_DIR` and `SYNAPS_RUNTIME_DIR`; an auto-spawned daemon is
//! private to that HOME and stopped on drop. Nothing here can reach the
//! user's daemon or `~/.synaps-cli`.
#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{http::StatusCode, response::IntoResponse, Router};
use phase2::{spawn_stub, Script, ANTHROPIC_MESSAGES_JSON, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

const MODEL: &str = "claude-sonnet-4-5";
const WAIT: Duration = Duration::from_secs(30);

// ── provider scripts ────────────────────────────────────────────────────────

/// Thinking + text, cache read/write tokens with the 5m/1h split.
const SSE_USAGE_CACHE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_u1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":40,",
    "\"cache_creation\":{\"ephemeral_5m_input_tokens\":20,\"ephemeral_1h_input_tokens\":10}}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"pondering\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"signature_delta\",\"signature\":\"c2ln\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"cached \"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Text, then a `bash` tool call (`echo tool-ok`), `tool_use` stop.
#[cfg(feature = "command-exec")]
const SSE_BASH_TOOL: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":20,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"running\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_diff1\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,",
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\": \\\"echo tool-ok\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The continuation after the tool result.
#[cfg(feature = "command-exec")]
const SSE_AFTER_TOOL: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":35,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":15}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"tool said ok\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A provider that refuses every request with a fixed 400 (no retry, no
/// echo of the request: the frame text must be deterministic).
async fn spawn_error_stub() -> String {
    let app = Router::new().fallback(|| async {
        (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"diff-boom\"}}",
        )
            .into_response()
    });
    phase2::serve(app).await
}

// ── harness ─────────────────────────────────────────────────────────────────

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

/// One isolated HOME: base dir with synthetic auth, its own runtime dir and
/// a work dir (the rpc process' cwd). Stops a daemon spawned under it.
struct Env {
    home: tempfile::TempDir,
    url: String,
    /// Provider requests the stub has answered (`Env::new` only).
    hits: Arc<AtomicUsize>,
}

impl Env {
    async fn new(script: Script) -> Self {
        let (url, hits, _bodies) = spawn_stub(script).await;
        let mut env = Self::with_url(url);
        env.hits = hits;
        env
    }

    fn with_url(url: String) -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        Self { home, url, hits: Arc::default() }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn run_dir(&self) -> PathBuf {
        self.home.path().join("run")
    }

    /// The private daemon run dirs `synaps rpc` processes made
    /// (`<run>/rpc/<pid>-<rand>`).
    fn private_dirs(&self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(self.run_dir().join("rpc")) else { return Vec::new() };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        dirs
    }

    /// A private daemon's files.
    fn private_paths(dir: &Path) -> agent_engine::daemon::registry::DaemonPaths {
        agent_engine::daemon::registry::daemon_paths_in(dir, None)
    }

    /// The one private dir with a live daemon.
    fn live_private_dir(&self) -> PathBuf {
        let live: Vec<PathBuf> = self
            .private_dirs()
            .into_iter()
            .filter(|d| agent_engine::daemon::registry::is_alive(&Self::private_paths(d)))
            .collect();
        assert_eq!(live.len(), 1, "one live private daemon: {live:?}");
        live[0].clone()
    }

    /// A `synaps` command aimed at the private daemon in `dir`.
    fn private_cmd(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = self.cmd(args);
        c.env("SYNAPS_DAEMON_DIR", dir);
        c
    }

    /// The profile's shared daemon: `synaps rpc` must never create it.
    fn assert_no_profile_daemon(&self) {
        let paths = agent_engine::daemon::registry::daemon_paths_in(&self.run_dir(), None);
        for f in [&paths.sock, &paths.lock, &paths.json] {
            assert!(!f.exists(), "the profile daemon was touched: {}", f.display());
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("SYNAPS_RUNTIME_DIR", self.run_dir())
            .env("SYNAPS_ANTHROPIC_BASE_URL", &self.url)
            .env("SYNAPS_NO_BOOT_FX", "1")
            .env("SYNAPS_DAEMON_IDLE_EXIT_SECS", "2")
            .env("TERM", "dumb")
            .current_dir(self.home.path().join("work"));
        c
    }

    async fn rpc(&self, extra: &[&str]) -> Rpc {
        let mut args = vec!["rpc", "-m", MODEL];
        args.extend_from_slice(extra);
        let mut child = self
            .cmd(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn synaps rpc");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Rpc { child, stdin, rx, frames: Vec::new() }
    }

    /// `synaps send <text> --session <sid>` — an external event.
    async fn send_event(&self, sid: &str, text: &str) {
        let out = self
            .cmd(&["send", text, "--session", sid, "--source", "diff"])
            .stdin(Stdio::null())
            .output()
            .await
            .expect("synaps send");
        assert!(out.status.success(), "synaps send failed: {}", String::from_utf8_lossy(&out.stderr));
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let mut dirs = self.private_dirs();
        dirs.push(self.run_dir());
        for dir in dirs {
            let paths = agent_engine::daemon::registry::daemon_paths_in(&dir, None);
            if agent_engine::daemon::registry::is_alive(&paths) {
                let _ = std::process::Command::new(bin())
                    .args(["daemon", "stop", "--force"])
                    .env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                    .env("HOME", self.home.path())
                    .env("SYNAPS_BASE_DIR", self.base())
                    .env("SYNAPS_RUNTIME_DIR", self.run_dir())
                    .env("SYNAPS_DAEMON_DIR", &dir)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

struct Rpc {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::UnboundedReceiver<String>,
    /// Every frame seen, in order (the transcript).
    frames: Vec<Value>,
}

impl Rpc {
    async fn send(&mut self, v: Value) {
        let mut line = serde_json::to_string(&v).unwrap();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// Read frames into the transcript until one matches `pred`.
    async fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let line = tokio::time::timeout(WAIT, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; saw {:#?}", self.frames))
                .unwrap_or_else(|| panic!("stdout closed waiting for {what}; saw {:#?}", self.frames));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carried a non-JSON line {line:?}: {e}"));
            self.frames.push(v.clone());
            if pred(&v) {
                return v;
            }
        }
    }

    async fn ready(&mut self) -> String {
        let r = self.until("ready", |v| v["type"] == "ready").await;
        assert_eq!(self.frames.len(), 1, "ready is the first frame: {:?}", self.frames);
        r["session_id"].as_str().unwrap().to_string()
    }

    async fn response(&mut self, id: &str) -> Value {
        self.until(id, |v| v["type"] == "response" && v["id"] == id && !is_disclosure(v)).await
    }

    async fn error(&mut self, id: &str) -> Value {
        self.until(id, |v| v["type"] == "error" && v["id"] == id).await
    }

    /// `prompt` then its terminal `response`.
    async fn turn(&mut self, id: &str, text: &str) -> Value {
        self.send(json!({"type": "prompt", "id": id, "message": text})).await;
        self.response(id).await
    }

    /// Send `shutdown`, drain stdout to EOF, record the exit code.
    async fn shutdown(mut self) -> Vec<Value> {
        self.send(json!({"type": "shutdown"})).await;
        self.finish().await
    }

    /// Close stdin (EOF), drain stdout, record the exit code.
    async fn eof(mut self) -> Vec<Value> {
        self.stdin = None;
        self.finish().await
    }

    async fn finish(mut self) -> Vec<Value> {
        while let Ok(Some(line)) = tokio::time::timeout(WAIT, self.rx.recv()).await {
            self.frames.push(serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON {line:?}: {e}")));
        }
        let status = tokio::time::timeout(WAIT, self.child.wait()).await.expect("rpc exits").unwrap();
        self.frames.push(json!({"_exit": status.code()}));
        self.frames
    }
}

fn is_disclosure(v: &Value) -> bool {
    v["command"].as_str().is_some_and(|c| c.ends_with(".disclosure"))
}

// ── normalisation + golden compare ──────────────────────────────────────────

/// Run-time ids and paths → placeholders, in first-seen order.
#[derive(Default)]
struct Norm {
    subs: Vec<(String, String)>,
    sids: usize,
    evts: usize,
}

impl Norm {
    fn new(home: &Path) -> Self {
        let mut n = Self::default();
        let canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        n.subs.push((canon.display().to_string(), "<HOME>".into()));
        n.subs.push((home.display().to_string(), "<HOME>".into()));
        n
    }

    fn learn(&mut self, frames: &[Value]) {
        for f in frames {
            let sid = match (f["type"].as_str(), f["command"].as_str()) {
                (Some("ready"), _) => f["session_id"].as_str(),
                // Response bodies are flattened into the frame.
                (Some("response"), Some("new_session")) => f["session_id"].as_str(),
                _ => None,
            };
            if let Some(s) = sid {
                if !self.subs.iter().any(|(k, _)| k == s) {
                    self.subs.push((s.to_string(), format!("<SID{}>", self.sids)));
                    self.sids += 1;
                }
            }
            if f["type"] == "event" {
                if let Some(e) = f["payload"]["id"].as_str() {
                    if !self.subs.iter().any(|(k, _)| k == e) {
                        self.subs.push((e.to_string(), format!("<EVT{}>", self.evts)));
                        self.evts += 1;
                    }
                }
            }
        }
    }

    fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => {
                let mut s = s.clone();
                for (from, to) in &self.subs {
                    if !from.is_empty() {
                        s = s.replace(from, to);
                    }
                }
                Value::String(s)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, x)| {
                        let x = match k.as_str() {
                            "timestamp" | "ts" | "created_at" | "updated_at" if !x.is_null() => json!("<TS>"),
                            "correlation_id" if !x.is_null() => json!("<CORR>"),
                            "duration_secs" => json!(0),
                            _ => self.value(x),
                        };
                        (k.clone(), x)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rpc_golden")
}

/// Normalise `frames` and compare to (or, with `SYNAPS_RPC_GOLDEN_UPDATE=1`,
/// write) `tests/fixtures/rpc_golden/<name>.jsonl`.
fn check(name: &str, env: &Env, frames: &[Value]) {
    let mut norm = Norm::new(env.home.path());
    norm.learn(frames);
    let got: Vec<String> = frames.iter().map(|f| serde_json::to_string(&norm.value(f)).unwrap()).collect();
    let path = golden_dir().join(format!("{name}.jsonl"));
    if std::env::var("SYNAPS_RPC_GOLDEN_UPDATE").as_deref() == Ok("1") {
        if !cfg!(feature = "command-exec") {
            panic!("write fixtures from a default (command-exec) build");
        }
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, got.join("\n") + "\n").unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let want: Vec<String> = want.lines().map(|w| intentional(name, w, false)).collect();
    let got: Vec<String> = got.iter().map(|g| intentional(name, g, true)).collect();
    if want != got {
        let mut report = String::new();
        for i in 0..want.len().max(got.len()) {
            let w = want.get(i).map(String::as_str).unwrap_or("<none>");
            let g = got.get(i).map(String::as_str).unwrap_or("<none>");
            let mark = if w == g { "  " } else { "!!" };
            report.push_str(&format!("{mark} {i:>3} want {w}\n{mark} {i:>3} got  {g}\n"));
        }
        panic!("{name}: frames differ from the golden fixture\n{report}");
    }
}

/// The session cost a scenario's `get_session_stats` (by id) must report:
/// the stub's tokens at the published prices ($/MTok — Sonnet 4.5: 3 in,
/// 15 out; Opus 4.5: 5 in, 25 out; cache read 0.1×, cache write 1.25× (5m)
/// / 2× (1h) the input price).
fn priced_cost(scenario: &str, id: &str) -> f64 {
    const M: f64 = 1e-6;
    match (scenario, id) {
        // SSE_USAGE_CACHE: 12 in, 9 out, 40 cache read, 20 (5m) + 10 (1h) written.
        ("usage_cache_and_stats", "s1") => 12.0 * 3.0 * M + 9.0 * 15.0 * M + 40.0 * 0.3 * M + 20.0 * 3.75 * M + 10.0 * 6.0 * M,
        // ANTHROPIC_SSE: 10 in, 1 out, on Sonnet 4.5 …
        ("continue_resume", "s1") => 10.0 * 3.0 * M + 15.0 * M,
        // … and after `set_model claude-opus-4-5`, priced as Opus 4.5.
        ("set_model", "st") => 10.0 * 5.0 * M + 25.0 * M,
        // Nothing ran in that session.
        ("queries_fresh", "s1") | ("new_session", "s1") => 0.0,
        other => panic!("no priced cost for {other:?}"),
    }
}

/// The documented intentional differences (module docs), applied to a
/// normalised frame line on both sides (`got`: this build's frame).
fn intentional(scenario: &str, line: &str, got: bool) -> String {
    let mut v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"));
    if v["type"] == "response" && v["command"] == "get_session_stats" {
        let cost = v["cost"].as_f64().unwrap_or_else(|| panic!("get_session_stats.cost is not a number: {line}"));
        if got {
            let want = priced_cost(scenario, v["id"].as_str().unwrap_or_default());
            assert!((cost - want).abs() < 1e-12, "{scenario}: cost {cost} != priced {want}: {line}");
        }
        v["cost"] = json!("<COST>");
    }
    // Build note (module docs): no command tools without `command-exec`.
    if !cfg!(feature = "command-exec") && v["type"] == "response" && v["command"] == "tools_list" {
        if let Some(tools) = v["tools"].as_array_mut() {
            tools.retain(|t| {
                let name = t["name"].as_str().unwrap_or_default();
                !(name == "bash" || name == "powershell" || name.starts_with("shell_"))
            });
        }
    }
    serde_json::to_string(&v).unwrap()
}

// ── scenarios ───────────────────────────────────────────────────────────────

/// One prompt, one text reply: ready, response_start, text, agent_end with
/// usage, the prompt's response; shutdown → exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_turn() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    let frames = rpc.shutdown().await;
    check("plain_turn", &env, &frames);
}

/// Thinking + text; cache read/write tokens (with the 5m/1h split) in
/// `agent_end.usage`; then the stats/state queries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_cache_and_stats() {
    let env = Env::new(Script::Sse(SSE_USAGE_CACHE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "get_session_stats", "id": "s1"})).await;
    rpc.response("s1").await;
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    rpc.response("g1").await;
    let frames = rpc.shutdown().await;
    check("usage_cache_and_stats", &env, &frames);
}

/// A tool round: tool call start/input/result, then the continuation; the
/// history (`get_messages`) carries the tool_use/tool_result pair.
#[cfg(feature = "command-exec")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop() {
    static BODIES: [&str; 2] = [SSE_BASH_TOOL, SSE_AFTER_TOOL];
    let env = Env::new(Script::SeqSse(&BODIES)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "run it").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    let frames = rpc.shutdown().await;
    check("tool_loop", &env, &frames);
}

/// `follow_up` (and a second `prompt`) while a turn streams: refused with an
/// error frame carrying the command id; the turn is untouched. Then abort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_up_mid_turn() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    rpc.send(json!({"type": "follow_up", "id": "f1", "message": "more"})).await;
    rpc.error("f1").await;
    rpc.send(json!({"type": "prompt", "id": "p2", "message": "again"})).await;
    rpc.error("p2").await;
    rpc.send(json!({"type": "abort", "id": "a1"})).await;
    rpc.response("a1").await;
    let frames = rpc.shutdown().await;
    check("follow_up_mid_turn", &env, &frames);
}

/// `abort` mid-stream: agent_end, the prompt's `cancelled` response, the
/// abort's response; the history keeps the partial reply and the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_mid_turn() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    rpc.send(json!({"type": "abort", "id": "a1"})).await;
    rpc.response("a1").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    rpc.send(json!({"type": "abort", "id": "a2"})).await;
    rpc.response("a2").await;
    let frames = rpc.shutdown().await;
    check("abort_mid_turn", &env, &frames);
}

/// `compact` after a turn: the disclosure frame, then the summary response;
/// the history is replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact() {
    let env = Env::new(Script::SseOrJson {
        sse: ANTHROPIC_SSE,
        json: ANTHROPIC_MESSAGES_JSON,
        json_delay: Duration::from_millis(0),
    })
    .await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "compact", "id": "c1"})).await;
    rpc.until("c1", |v| v["id"] == "c1" && !is_disclosure(v)).await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    let frames = rpc.shutdown().await;
    check("compact", &env, &frames);
}

/// `new_session` after a turn: a new id, empty history, zeroed stats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_session() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "new_session", "id": "n1"})).await;
    rpc.response("n1").await;
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    rpc.response("g1").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    rpc.send(json!({"type": "get_session_stats", "id": "s1"})).await;
    rpc.response("s1").await;
    let frames = rpc.shutdown().await;
    check("new_session", &env, &frames);
}

/// `get_messages` / `get_state` / `get_session_stats` on a fresh session,
/// plus a malformed line (error with no id).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queries_fresh() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    rpc.response("g1").await;
    rpc.send(json!({"type": "get_session_stats", "id": "s1"})).await;
    rpc.response("s1").await;
    rpc.send(json!({"type": "no_such_command", "id": "x1"})).await;
    rpc.until("parse error", |v| v["type"] == "error").await;
    let frames = rpc.eof().await;
    check("queries_fresh", &env, &frames);
}

/// `tools_list` — the session's tool schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "tools_list", "id": "t1"})).await;
    rpc.response("t1").await;
    let frames = rpc.shutdown().await;
    check("tools_list", &env, &frames);
}

/// `set_model`, then the state/stats reflect it, and the next turn's usage
/// is attributed by the provider's model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_model() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "set_model", "id": "s1", "model": "claude-opus-4-5"})).await;
    rpc.response("s1").await;
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    rpc.response("g1").await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "get_session_stats", "id": "st"})).await;
    rpc.response("st").await;
    let frames = rpc.shutdown().await;
    check("set_model", &env, &frames);
}

/// An external event (`synaps send --session`) on an idle session: the
/// `event` frame, then an auto-turn answered as `auto_turn`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_auto_turn() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    let sid = rpc.ready().await;
    env.send_event(&sid, "ping from outside").await;
    rpc.until("auto_turn", |v| v["type"] == "response" && v["command"] == "auto_turn").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    let frames = rpc.shutdown().await;
    check("event_auto_turn", &env, &frames);
}

/// `--continue <id>` resumes the history of a previous run (same id), and
/// `--continue <prefix>` resolves it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_resume() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut a = env.rpc(&[]).await;
    let sid = a.ready().await;
    a.turn("p1", "remember me").await;
    let mut frames = a.shutdown().await;

    let mut b = env.rpc(&["--continue", &sid]).await;
    assert_eq!(b.ready().await, sid, "same session id");
    b.send(json!({"type": "get_messages", "id": "m1"})).await;
    b.response("m1").await;
    b.send(json!({"type": "get_session_stats", "id": "s1"})).await;
    b.response("s1").await;
    frames.extend(b.shutdown().await);

    let prefix = &sid[..sid.len() - 2];
    let mut c = env.rpc(&["--continue", prefix]).await;
    assert_eq!(c.ready().await, sid, "prefix resolves");
    frames.extend(c.shutdown().await);
    check("continue_resume", &env, &frames);
}

/// A provider error: the `error` frame with the prompt's id, agent_end, the
/// failed response with its typed outcome; the session stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_error() {
    let env = Env::with_url(spawn_error_stub().await);
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    rpc.response("g1").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.response("m1").await;
    let frames = rpc.shutdown().await;
    check("provider_error", &env, &frames);
}

/// `--system <file>`: the system prompt reaches the provider (checked via
/// the stub's request body) and the frames are unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_file() {
    let (url, _hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    let env = Env::with_url(url);
    let sys = env.home.path().join("work").join("system.md");
    std::fs::write(&sys, "DIFF-SYSTEM-SENTINEL-7f3a").unwrap();
    let mut rpc = env.rpc(&["--system", sys.to_str().unwrap()]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    let frames = rpc.shutdown().await;
    let seen = bodies.lock().unwrap().iter().any(|b| String::from_utf8_lossy(b).contains("DIFF-SYSTEM-SENTINEL-7f3a"));
    assert!(seen, "system prompt from --system file reached the provider");
    check("system_file", &env, &frames);
}

// ── daemon client behaviour (new in PR 4; not differential) ────────────────

/// An idle rpc session never keeps the prompt cache warm (PR 3: owned
/// sessions never do), even with `cache_keepwarm = auto` and headless
/// clients opted in: a ping's spend would never reach an `agent_end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_rpc_session_never_keeps_warm() {
    use agent_engine::session::socket_transport::SocketTransport;
    use agent_engine::session::wire::{Attach, Hello};
    use agent_engine::session::{
        AttachMode, ClientKind, ClientTransport, SessionCommand, SessionEventWire, SessionId, SessionQuery,
    };

    let env = Env::new(Script::Sse(SSE_USAGE_CACHE)).await;
    std::fs::write(env.base().join("config"), "cache_keepwarm = auto\ncache_keepwarm_headless = true\n").unwrap();
    let mut rpc = env.rpc(&[]).await;
    let sid = rpc.ready().await;
    rpc.turn("p1", "hello").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let paths = Env::private_paths(&env.live_private_dir());
    let conn = SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _) = SocketTransport::attach(
        conn,
        Attach::Existing { session_id: SessionId::from(sid.clone()), mode: AttachMode::Observe },
    )
    .await
    .expect("observe the rpc session");
    t.send_from_self(SessionCommand::Query { id: 77, query: SessionQuery::Status }).await.unwrap();
    let status = loop {
        let e = tokio::time::timeout(Duration::from_secs(10), t.next_event()).await.unwrap().expect("event");
        if let SessionEventWire::QueryResult { id: 77, value } = e.event {
            break value;
        }
    };
    let kw = &status["keepwarm"];
    assert_eq!(kw["active"], false, "{kw}");
    assert!(kw["period"].is_null(), "no keep-warm period: {kw}");
    assert_eq!(kw["session_pings"], 0, "{kw}");
    assert_eq!(status["keepwarm_cost"], 0.0, "{status}");
    assert_eq!(env.hits.load(Ordering::SeqCst), 1, "only the turn reached the provider");
    drop(t);
    rpc.shutdown().await;
}

/// The session is OWNED by the rpc process: killing it mid-turn cancels the
/// turn and ends the session in the daemon, so it can be `--continue`d.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_mid_turn_ends_owned_session() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    let sid = rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    let dir = env.live_private_dir();
    rpc.child.start_kill().unwrap();
    rpc.child.wait().await.unwrap();

    // Gone from its private daemon, which then idles out (≤ 2 s here).
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let out = env.private_cmd(&dir, &["daemon", "sessions", "--json"]).stdin(Stdio::null()).output().await.unwrap();
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() || !listed.contains(&sid) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "session {sid} outlived its owner: {listed}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    while agent_engine::daemon::registry::is_alive(&Env::private_paths(&dir)) {
        assert!(tokio::time::Instant::now() < deadline, "the private daemon outlived its idle exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Free to continue, with the prompt that was cut kept in the history;
    // the dead process' run dir is swept on the way (once older than the
    // sweep's 60 s grace: age it).
    let old = std::time::SystemTime::now() - Duration::from_secs(120);
    std::fs::File::open(&dir).unwrap().set_modified(old).unwrap();
    let mut again = env.rpc(&["--continue", &sid]).await;
    assert_eq!(again.ready().await, sid);
    assert!(!dir.exists(), "stale private run dir not swept: {}", dir.display());
    again.send(json!({"type": "get_messages", "id": "m1"})).await;
    let m = again.response("m1").await;
    assert_eq!(m["messages"][0]["content"], "go", "{m}");
    again.shutdown().await;
    env.assert_no_profile_daemon();
}

/// No daemon, no rpc: `SYNAPS_DAEMON=0`, or autospawn off with nothing
/// running → exit 3, one error frame on stdout, the reason on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_unavailable_exits_3() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    for (var, val) in [("SYNAPS_DAEMON", "0"), ("SYNAPS_DAEMON_AUTOSPAWN", "0")] {
        let out = env
            .cmd(&["rpc", "-m", MODEL])
            .env(var, val)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{var}={val}: stdout {stdout} stderr {stderr}");
        let frame: Value = serde_json::from_str(stdout.lines().next().unwrap_or("")).expect("an error frame");
        assert_eq!(frame["type"], "error", "{frame}");
        assert!(frame["message"].as_str().unwrap().contains("daemon"), "{frame}");
        assert!(stderr.contains("daemon"), "{stderr}");
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
    }
}

/// `--continue` of a session another rpc process has live is refused (exit
/// 1, no `ready`), and the live one is not disturbed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_continue_refused() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut a = env.rpc(&[]).await;
    let sid = a.ready().await;

    let out = env
        .cmd(&["rpc", "-m", MODEL, "--continue", &sid])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout {stdout} stderr {}", String::from_utf8_lossy(&out.stderr));
    assert!(!stdout.contains("\"ready\""), "{stdout}");

    let r = a.turn("p1", "still mine").await;
    assert_eq!(r["ok"], true, "{r}");
    a.shutdown().await;
}

/// `synaps daemon reload --now` mid-turn: the checkpoint cuts the turn, which
/// ends with its terminal frames (error, agent_end, response ok:false); the
/// rpc re-binds to its session and keeps serving; shutdown still exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_mid_turn_rebinds() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    let sid = rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;

    let dir = env.live_private_dir();
    let out = env.private_cmd(&dir, &["daemon", "reload", "--now"]).stdin(Stdio::null()).output().await.unwrap();
    assert!(out.status.success(), "reload: {}", String::from_utf8_lossy(&out.stderr));

    let r = rpc.response("p1").await;
    assert_eq!(r["ok"], false, "{r}");
    let n = rpc.frames.len();
    assert_eq!(rpc.frames[n - 2]["type"], "agent_end", "{:#?}", rpc.frames);
    assert!(rpc.frames.iter().any(|f| f["type"] == "error" && f["id"] == "p1"), "{:#?}", rpc.frames);

    // Re-bound: the same session answers.
    rpc.send(json!({"type": "get_state", "id": "g1"})).await;
    let g = rpc.response("g1").await;
    assert_eq!(g["session_id"], sid.as_str(), "{g}");
    let frames = rpc.shutdown().await;
    assert_eq!(frames.last().unwrap()["_exit"], 0, "{frames:#?}");
    // Its run dir goes with it.
    assert!(!dir.exists(), "private run dir left behind: {}", dir.display());
}

/// Each `synaps rpc` gets its own private daemon (the old per-process
/// isolation): two at once → two daemons in two run dirs, and neither
/// creates or connects to the profile's shared daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_daemon_per_process() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut a = env.rpc(&[]).await;
    let mut b = env.rpc(&[]).await;
    let (sa, sb) = (a.ready().await, b.ready().await);
    assert_ne!(sa, sb);
    let dirs = env.private_dirs();
    assert_eq!(dirs.len(), 2, "{dirs:?}");
    let pids: Vec<u32> = dirs
        .iter()
        .map(|d| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", d.display());
            }
            agent_engine::daemon::registry::read_daemon_json(&Env::private_paths(d)).expect("daemon.json").pid
        })
        .collect();
    assert_ne!(pids[0], pids[1]);
    for (rpc, sid) in [(&mut a, &sa), (&mut b, &sb)] {
        let r = rpc.turn("p1", "hi").await;
        assert_eq!(r["ok"], true, "{r}");
        rpc.send(json!({"type": "get_state", "id": "g"})).await;
        assert_eq!(rpc.response("g").await["session_id"], sid.as_str());
    }
    env.assert_no_profile_daemon();
    a.shutdown().await;
    b.shutdown().await;
    assert!(env.private_dirs().is_empty(), "{:?}", env.private_dirs());
    env.assert_no_profile_daemon();
}

/// A parent that stops reading stdout for a while (Pria POSTs every frame
/// in turn) loses nothing: the daemon's events are read eagerly and queue
/// for stdout, the owner never lags, every frame arrives in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_consumer_loses_nothing() {
    const N: usize = 3000;
    let env = Env::new(Script::Sse(flood_body(N))).await;

    let errlog = env.home.path().join("rpc.stderr");
    let mut child = env
        .cmd(&["rpc", "-m", MODEL])
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&errlog).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = || std::fs::read_to_string(&errlog).unwrap_or_default();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ready["type"], "ready");
    stdin.write_all(b"{\"type\":\"prompt\",\"id\":\"p1\",\"message\":\"flood\"}\n").await.unwrap();
    stdin.flush().await.unwrap();
    // Not reading: the pipe (64 KiB) fills within the first few hundred
    // deltas; the whole stream (~250 KiB of frames) lands meanwhile.
    tokio::time::sleep(Duration::from_secs(6)).await;
    if let Some(st) = child.try_wait().unwrap() {
        panic!("rpc exited ({st}) while its stdout was not read; stderr:\n{}", stderr());
    }

    let mut frames = Vec::new();
    loop {
        let line = tokio::time::timeout(WAIT, lines.next_line()).await.expect("frames").unwrap().expect("stdout open");
        let v: Value = serde_json::from_str(&line).unwrap();
        let done = v["type"] == "response" && v["id"] == "p1";
        frames.push(v);
        if done {
            break;
        }
    }
    // The engine may coalesce adjacent deltas that arrive together (as it
    // always did); the text itself must be complete and in order.
    let text: String = frames
        .iter()
        .filter(|f| f["type"] == "message_update" && f["event"]["type"] == "text_delta")
        .map(|f| f["event"]["delta"].as_str().unwrap())
        .collect();
    let want: String = (0..N).map(|i| format!("<{i:04}> padding padding padding padding padding")).collect();
    assert!(text == want, "text lost or reordered: {} of {} bytes", text.len(), want.len());
    assert!(!frames.iter().any(|f| f["type"] == "error"), "{:?}", frames.iter().find(|f| f["type"] == "error"));
    let end = frames.iter().rev().find(|f| f["type"] == "agent_end").expect("agent_end");
    assert_eq!(end["usage"]["output_tokens"], 3000, "{end}");
    assert_eq!(frames.last().unwrap()["ok"], true, "{:?}", frames.last());
    stdin.write_all(b"{\"type\":\"shutdown\"}\n").await.unwrap();
    drop(stdin);
    while let Ok(Some(_)) = lines.next_line().await {}
    assert_eq!(tokio::time::timeout(WAIT, child.wait()).await.unwrap().unwrap().code(), Some(0));
}

/// The flood turn of [`slow_consumer_loses_nothing`]: `n` deltas, ~80 B each.
fn flood_body(n: usize) -> &'static str {
    let mut body = String::from(concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_flood\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    ));
    for i in 0..n {
        body.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"<{i:04}> padding padding padding padding padding\"}}}}\n\n"
        ));
    }
    body.push_str(concat!(
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},",
        "\"usage\":{\"output_tokens\":3000}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));
    Box::leak(body.into_boxed_str())
}

/// On exit, frames still queued wait for a parent that reads stdout late
/// (past the old 2 s give-up): the turn's `agent_end` and response arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_reader_after_eof_gets_every_frame() {
    let env = Env::new(Script::Sse(flood_body(3000))).await;
    let mut child = env
        .cmd(&["rpc", "-m", MODEL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ready["type"], "ready");
    stdin.write_all(b"{\"type\":\"prompt\",\"id\":\"p1\",\"message\":\"flood\"}\n").await.unwrap();
    stdin.flush().await.unwrap();
    // The pipe fills; then EOF, and nothing is read for 5 s.
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(stdin);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut frames = Vec::new();
    while let Ok(Ok(Some(line))) = tokio::time::timeout(WAIT, lines.next_line()).await {
        frames.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    assert!(frames.iter().any(|f| f["type"] == "agent_end"), "no agent_end in {} frames", frames.len());
    assert!(frames.iter().any(|f| f["type"] == "response" && f["id"] == "p1"), "no response in {} frames", frames.len());
    assert_eq!(tokio::time::timeout(WAIT, child.wait()).await.unwrap().unwrap().code(), Some(0));
}

/// The private daemon's stderr is copied to rpc's (diagnostics such as
/// config warnings, which the in-process engine printed itself); stdout
/// stays protocol frames only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_stderr_reaches_rpc_stderr() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    std::fs::write(env.base().join("config"), "cache_ttl = bogus\n").unwrap();
    let errlog = env.home.path().join("rpc.stderr");
    let out = env
        .cmd(&["rpc", "-m", MODEL])
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&errlog).unwrap())
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("{\"type\":\"ready\""), "{stdout}");
    assert!(stdout.lines().all(|l| serde_json::from_str::<Value>(l).is_ok()), "{stdout}");
    let stderr = std::fs::read_to_string(&errlog).unwrap();
    assert!(stderr.contains("config warning: cache_ttl = bogus"), "{stderr}");
}

/// A parent that closes stdout is gone: rpc does not wait out the drain cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_stdout_exits_promptly() {
    let env = Env::new(Script::Sse(flood_body(3000))).await;
    let mut child = env
        .cmd(&["rpc", "-m", MODEL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ready["type"], "ready");
    stdin.write_all(b"{\"type\":\"prompt\",\"id\":\"p1\",\"message\":\"flood\"}\n").await.unwrap();
    stdin.flush().await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(lines);
    drop(stdin);
    let t0 = std::time::Instant::now();
    let st = tokio::time::timeout(Duration::from_secs(20), child.wait()).await.expect("rpc exits").unwrap();
    assert!(t0.elapsed() < Duration::from_secs(15), "took {:?} ({st})", t0.elapsed());
}

/// stdin EOF mid-turn: the turn is cancelled and still closed out — its
/// `agent_end` (with the usage seen so far) and `response` — then exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_mid_turn_closes_the_turn() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    let frames = rpc.eof().await;
    let n = frames.len();
    assert_eq!(frames[n - 1]["_exit"], 0, "{frames:#?}");
    assert_eq!(frames[n - 2]["type"], "response", "{frames:#?}");
    assert_eq!(frames[n - 2]["id"], "p1", "{frames:#?}");
    assert_eq!(frames[n - 2]["cancelled"], true, "{frames:#?}");
    assert_eq!(frames[n - 3]["type"], "agent_end", "{frames:#?}");
    assert_eq!(frames[n - 3]["usage"]["input_tokens"], 10, "{frames:#?}");
}

/// stdin EOF right after a query: the query is still answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_answers_a_pending_query() {
    let env = Env::new(Script::Sse(ANTHROPIC_SSE)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "get_messages", "id": "m1"})).await;
    rpc.send(json!({"type": "get_session_stats", "id": "s1"})).await;
    let frames = rpc.eof().await;
    let m = frames.iter().find(|f| f["id"] == "m1").expect("get_messages answered");
    assert_eq!(m["type"], "response", "{m}");
    assert_eq!(m["messages"].as_array().map(Vec::len), Some(2), "{m}");
    let st = frames.iter().find(|f| f["id"] == "s1").expect("get_session_stats answered");
    assert_eq!(st["input_tokens"], 10, "{st}");
    assert_eq!(frames.last().unwrap()["_exit"], 0, "{frames:#?}");
}

/// SIGTERM mid-turn (Pria's `close()`): the turn is cancelled, its
/// `agent_end` (usage so far) and `response` written, then exit 143.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_mid_turn_writes_agent_end() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    let dir = env.live_private_dir();
    let t0 = std::time::Instant::now();
    let pid = rpc.child.id().unwrap() as i32;
    assert!(std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap().success());
    let frames = rpc.finish().await;
    assert!(t0.elapsed() < Duration::from_secs(5), "SIGTERM took {:?} (Pria's grace is 5 s)", t0.elapsed());
    let n = frames.len();
    assert_eq!(frames[n - 1]["_exit"], 143, "{frames:#?}");
    assert_eq!(frames[n - 2]["id"], "p1", "{frames:#?}");
    assert_eq!(frames[n - 2]["cancelled"], true, "{frames:#?}");
    assert_eq!(frames[n - 3]["type"], "agent_end", "{frames:#?}");
    assert_eq!(frames[n - 3]["usage"]["input_tokens"], 10, "{frames:#?}");
    assert!(!dir.exists(), "private run dir left behind: {}", dir.display());
}

/// Two tool calls in one assistant message (read-only: they run in
/// parallel): both start/result pairs, then the continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tool_calls() {
    const SSE_TWO_READS: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_p1\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":20,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \\\"a.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \\\"b.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
        "\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    static BODIES: [&str; 2] = [SSE_TWO_READS, ANTHROPIC_SSE];
    let env = Env::new(Script::SeqSse(&BODIES)).await;
    std::fs::write(env.home.path().join("work/a.txt"), "alpha-content\n").unwrap();
    std::fs::write(env.home.path().join("work/b.txt"), "bravo-content\n").unwrap();
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    let r = rpc.turn("p1", "read both").await;
    assert_eq!(r["ok"], true, "{r}");
    let ev = |t: &str| -> Vec<Value> {
        rpc.frames.iter().filter(|f| f["event"]["type"] == t).map(|f| f["event"].clone()).collect()
    };
    let starts = ev("toolcall_start");
    assert_eq!(starts.len(), 2, "{:#?}", rpc.frames);
    let results = ev("toolcall_result");
    assert_eq!(results.len(), 2, "{:#?}", rpc.frames);
    for (id, want) in [("toolu_a", "alpha-content"), ("toolu_b", "bravo-content")] {
        let res = results.iter().find(|r| r["tool_id"] == id).unwrap_or_else(|| panic!("{id}: {results:#?}"));
        assert!(res.to_string().contains(want), "{res}");
    }
    let end = rpc.frames.iter().find(|f| f["type"] == "agent_end").expect("agent_end");
    assert_eq!(end["usage"]["input_tokens"], 30, "{end}");
    assert_eq!(env.hits.load(Ordering::SeqCst), 2);
    rpc.shutdown().await;
}

/// The `subagent` tool end to end (the Slack bridge renders these): a
/// `subagent_start`, the child's run, a `subagent_done`, then the parent's
/// continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_frames() {
    const SSE_SUBAGENT: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s1\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":20,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_sub\",\"name\":\"subagent\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"task\\\": \\\"say hi\\\", \\\"name\\\": \\\"helper\\\", \\\"system_prompt\\\": \\\"You help.\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
        "\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    // Parent tool call, the child's answer, the parent's continuation.
    static BODIES: [&str; 3] = [SSE_SUBAGENT, ANTHROPIC_SSE, ANTHROPIC_SSE];
    let env = Env::new(Script::SeqSse(&BODIES)).await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    let r = rpc.turn("p1", "delegate").await;
    assert_eq!(r["ok"], true, "{r} {:#?}", rpc.frames);
    let pos = |t: &str| rpc.frames.iter().position(|f| f["type"] == t).unwrap_or_else(|| panic!("no {t}: {:#?}", rpc.frames));
    let (start, done, end) = (pos("subagent_start"), pos("subagent_done"), pos("agent_end"));
    assert!(start < done && done < end, "{:#?}", rpc.frames);
    let s = &rpc.frames[start];
    assert!(s["subagent_id"].is_u64(), "{s}");
    assert!(s["task_preview"].as_str().unwrap().contains("say hi"), "{s}");
    let d = &rpc.frames[done];
    assert_eq!(d["subagent_id"], s["subagent_id"], "{d}");
    assert_eq!(env.hits.load(Ordering::SeqCst), 3);
    rpc.shutdown().await;
}

/// A large tool result arrives whole in one `toolcall_result` (result
/// deltas are not protocol frames and never appear).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_tool_result() {
    const SSE_READ_BIG: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b1\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":20,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_big\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \\\"big.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
        "\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    static BODIES: [&str; 2] = [SSE_READ_BIG, ANTHROPIC_SSE];
    let env = Env::new(Script::SeqSse(&BODIES)).await;
    let big: String = (0..1500).map(|i| format!("line {i:05} {}\n", "x".repeat(60))).collect();
    std::fs::write(env.home.path().join("work/big.txt"), &big).unwrap();
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    let r = rpc.turn("p1", "read it").await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(!rpc.frames.iter().any(|f| f["event"]["type"] == "toolcall_result_delta"), "{:#?}", rpc.frames);
    let res: Vec<&Value> = rpc.frames.iter().filter(|f| f["event"]["type"] == "toolcall_result").collect();
    assert_eq!(res.len(), 1);
    let text = res[0]["event"].to_string();
    // The read tool caps its output (~30 KB); still far beyond one stream chunk.
    assert!(text.len() > 25_000, "a large result ({} bytes)", text.len());
    assert!(text.contains("line 00000") && text.contains("line 00300"), "head and middle present");
    rpc.shutdown().await;
}

/// The private daemon dies while idle with a `compact` in flight: the
/// compact gets its error frame, then exit 4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_lost_with_compact_pending() {
    let env = Env::new(Script::SseOrJson {
        sse: ANTHROPIC_SSE,
        json: ANTHROPIC_MESSAGES_JSON,
        json_delay: Duration::from_secs(120),
    })
    .await;
    let mut rpc = env.rpc(&[]).await;
    rpc.ready().await;
    rpc.turn("p1", "hello").await;
    rpc.send(json!({"type": "compact", "id": "c1"})).await;
    rpc.until("disclosure", is_disclosure).await;
    let dir = env.live_private_dir();
    let pid = agent_engine::daemon::registry::read_daemon_json(&Env::private_paths(&dir)).unwrap().pid;
    assert!(std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status().unwrap().success());
    let frames = rpc.finish().await;
    let n = frames.len();
    assert_eq!(frames[n - 1]["_exit"], 4, "{frames:#?}");
    let err = frames.iter().find(|f| f["type"] == "error" && f["id"] == "c1").expect("compact answered");
    assert!(err["message"].as_str().unwrap().contains("daemon"), "{err}");
    assert!(!frames.iter().any(|f| f["type"] == "error" && f["id"].is_null()), "{frames:#?}");
}
