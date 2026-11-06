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
//!   `updated_at` → `<TS>`; a history message's `_synaps_at` time stamp is
//!   dropped (which messages carry one is bookkeeping, not protocol);
//! - the per-turn `outcome.correlation_id` (a minted id) → `<CORR>`;
//! - durations: `duration_secs` → `0`.
//!
//! Intentional differences (see [`intentional`], applied to both sides):
//! - `get_session_stats.cost`: the in-process loop never accumulated cost
//!   (it reported 0.0, or the cost a `--continue`d session was saved with);
//!   the session actor reports the real session cost from the pricing
//!   table. Compared as "a number ≥ 0" (`<COST>`). Token counts, which
//!   billing reads, are compared exactly.
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
}

impl Env {
    async fn new(script: Script) -> Self {
        let (url, _hits, _bodies) = spawn_stub(script).await;
        Self::with_url(url)
    }

    fn with_url(url: String) -> Self {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
        std::fs::create_dir_all(home.path().join("run")).unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        Self { home, url }
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    fn run_dir(&self) -> PathBuf {
        self.home.path().join("run")
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
        let paths = agent_engine::daemon::registry::daemon_paths_in(&self.run_dir(), None);
        if agent_engine::daemon::registry::is_alive(&paths) {
            let _ = std::process::Command::new(bin())
                .args(["daemon", "stop", "--force"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", self.home.path())
                .env("SYNAPS_BASE_DIR", self.base())
                .env("SYNAPS_RUNTIME_DIR", self.run_dir())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
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
                    // A message's time stamp (`_synaps_at`) is dropped, not
                    // replaced: which messages carry one is bookkeeping.
                    .filter(|(k, _)| k.as_str() != "_synaps_at")
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
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, got.join("\n") + "\n").unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let want: Vec<String> = want.lines().map(intentional).collect();
    let got: Vec<String> = got.iter().map(|g| intentional(g)).collect();
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

/// The documented intentional differences (module docs), applied to a
/// normalised frame line on both sides.
fn intentional(line: &str) -> String {
    let mut v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"));
    if v["type"] == "response" && v["command"] == "get_session_stats" {
        let cost = v["cost"].as_f64().unwrap_or_else(|| panic!("get_session_stats.cost is not a number: {line}"));
        assert!(cost >= 0.0, "negative cost: {line}");
        v["cost"] = json!("<COST>");
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

/// The session is OWNED by the rpc process: killing it mid-turn cancels the
/// turn and ends the session in the daemon, so it can be `--continue`d.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_mid_turn_ends_owned_session() {
    let env = Env::new(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    let mut rpc = env.rpc(&[]).await;
    let sid = rpc.ready().await;
    rpc.send(json!({"type": "prompt", "id": "p1", "message": "go"})).await;
    rpc.until("text", |v| v["event"]["type"] == "text_delta").await;
    rpc.child.start_kill().unwrap();
    rpc.child.wait().await.unwrap();

    // Gone from the daemon (or the daemon idled out with nothing left).
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let out = env.cmd(&["daemon", "sessions", "--json"]).stdin(Stdio::null()).output().await.unwrap();
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() || !listed.contains(&sid) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "session {sid} outlived its owner: {listed}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Free to continue, with the prompt that was cut kept in the history.
    let mut again = env.rpc(&["--continue", &sid]).await;
    assert_eq!(again.ready().await, sid);
    again.send(json!({"type": "get_messages", "id": "m1"})).await;
    let m = again.response("m1").await;
    assert_eq!(m["messages"][0]["content"], "go", "{m}");
    again.shutdown().await;
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
