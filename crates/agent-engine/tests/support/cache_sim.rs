//! A loopback Anthropic provider that simulates prompt caching, for the
//! tool-round keep-warm tests (`runtime::tool_keepwarm`). Compiled into the
//! engine's unit tests (`#[path]` from `runtime/mod.rs`, used by
//! `runtime::stream` and `tools::subagent`) and into
//! `tests/worker_tool_keepwarm.rs`.
//!
//! The cache model (simplified from the API's, conservative where it
//! matters): a request is the sequence of blocks `header` (model, thinking,
//! tools, system), then every message content block in order; a block's key
//! is the serialized prefix through it with every `cache_control` removed,
//! its size in "tokens" the key's byte length. Every marked block (a
//! `cache_control` on it; the header counts as marked when tools or system
//! carry one) up to the last one is written if not live. The read is the
//! longest live entry ending at or before the last marker; reading it
//! refreshes its TTL. Unmarked bytes after the last marker are uncached
//! input. Entries expire `ttl` after their last write or read.
//!
//! Replies, by the last message: the keep-warm ping text gets `ok`; a
//! `tool_result` gets `done`; anything else gets the configured `tool_use`.
//!
//! Determinism: the cache runs on a VIRTUAL clock that only the [`Script`]
//! moves, and the tool of a round runs until the sim releases it (creates
//! the round's `go` file, `{go}` in the tool input) — at once, or after the
//! round's Nth ping. So whether the prefix expires, and how many pings a
//! round sees before its tool returns, never depends on how fast the test
//! machine is; real time only paces the pings (the interval override).
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use serde_json::{json, Value};

/// The ping text (`runtime::keepwarm::KEEPWARM_PING_TEXT`) starts with this.
pub const PING_MARK: &str = "[automated prompt-cache keep-warm ping";

/// What the simulated provider does with time and the round's tool.
#[derive(Debug, Clone)]
pub struct Script {
    /// The cache TTL, on the virtual clock.
    pub ttl: Duration,
    /// Virtual time added after a request that asks for a tool: how long
    /// the tool runs when no ping drives the clock.
    pub per_round: Duration,
    /// Virtual time added after each ping.
    pub per_ping: Duration,
    /// Pings a round sees before its tool is released (`0` = at once).
    pub release_after: usize,
    /// The Nth ping of the sim's life (1-based) is held — recorded and
    /// billed, its response withheld — until [`release_held`].
    pub hold_ping: Option<usize>,
}

/// The virtual TTL of the scripts (the real 5m one; nothing waits for it).
pub const TTL: Duration = Duration::from_secs(300);

impl Script {
    /// The tool runs for `pings` pings, each moving the clock by 0.6 TTL:
    /// by the time it returns the round's prefix is older than the TTL, so
    /// only the pings' refreshes keep it live.
    pub fn kept_warm(pings: usize) -> Self {
        Self {
            ttl: TTL,
            per_round: Duration::ZERO,
            per_ping: TTL * 3 / 5,
            release_after: pings,
            hold_ping: None,
        }
    }

    /// The tool runs for twice the TTL, with nothing to wait for (keep-warm
    /// off: the prefix expires).
    pub fn outlives_ttl() -> Self {
        Self {
            ttl: TTL,
            per_round: TTL * 2,
            per_ping: Duration::ZERO,
            release_after: 0,
            hold_ping: None,
        }
    }

    /// The tool returns at once, the clock stands still.
    pub fn short() -> Self {
        Self {
            ttl: TTL,
            per_round: Duration::ZERO,
            per_ping: Duration::ZERO,
            release_after: 0,
            hold_ping: None,
        }
    }
}

/// One request as the provider saw it, and what it billed.
#[derive(Debug, Clone)]
pub struct Req {
    /// Real arrival time.
    pub at: Instant,
    /// Virtual arrival time.
    pub vt: Duration,
    pub body: Value,
    pub read: u64,
    pub write: u64,
    pub input: u64,
}

impl Req {
    fn last_message(&self) -> String {
        self.body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .map(Value::to_string)
            .unwrap_or_default()
    }

    /// The last message carries the keep-warm ping text.
    pub fn is_ping(&self) -> bool {
        self.last_message().contains(PING_MARK)
    }

    /// The last message carries a tool result (and is no ping).
    pub fn has_tool_result(&self) -> bool {
        !self.is_ping() && self.last_message().contains("tool_result")
    }

    /// The tokens through the last marker (what the request caches).
    pub fn cached_prefix(&self) -> u64 {
        self.read + self.write
    }
}

pub struct Sim {
    pub requests: Vec<Req>,
    pub script: Script,
    /// The virtual clock.
    now: Duration,
    entries: HashMap<String, Duration>,
    /// Requests so far (never reset).
    seq: usize,
    /// Pings so far, and since the last tool-asking request.
    pings: usize,
    round_pings: usize,
    /// The current round's `go` file, until released.
    pending_go: Option<PathBuf>,
    go_dir: tempfile::TempDir,
    held: Arc<tokio::sync::Notify>,
    /// `requests.len()` when the held ping was released.
    pub held_released_at: Option<usize>,
}

impl Sim {
    fn new(script: Script) -> Self {
        Self {
            requests: Vec::new(),
            script,
            now: Duration::ZERO,
            entries: HashMap::new(),
            seq: 0,
            pings: 0,
            round_pings: 0,
            pending_go: None,
            go_dir: tempfile::tempdir().unwrap(),
            held: Arc::default(),
            held_released_at: None,
        }
    }

    fn release_tool(&mut self) {
        if let Some(go) = self.pending_go.take() {
            std::fs::write(go, "").unwrap();
        }
    }

    /// The pings so far.
    pub fn pings(&self) -> usize {
        self.pings
    }
}

/// Let the held ping ([`Script::hold_ping`]) answer.
pub fn release_held(sim: &Arc<Mutex<Sim>>) {
    let mut sim = sim.lock().unwrap();
    sim.held_released_at = Some(sim.requests.len());
    sim.held.notify_one();
}

fn strip(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip).collect()),
        other => other.clone(),
    }
}

fn marked(v: &Value) -> bool {
    v.to_string().contains("\"cache_control\"")
}

/// `(key, marked)` per block, in order.
fn blocks(body: &Value) -> Vec<(String, bool)> {
    let header = json!({
        "model": body["model"],
        "thinking": body["thinking"],
        "tools": strip(&body["tools"]),
        "system": strip(&body["system"]),
    });
    let mut key = header.to_string();
    let mut out = vec![(
        key.clone(),
        marked(&body["tools"]) || marked(&body["system"]),
    )];
    for message in body["messages"].as_array().into_iter().flatten() {
        key.push_str(&format!("|{}:", message["role"]));
        let content = match &message["content"] {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            Value::Array(items) => items.clone(),
            _ => Vec::new(),
        };
        for block in content {
            key.push_str(&strip(&block).to_string());
            out.push((key.clone(), marked(&block)));
        }
    }
    out
}

impl Sim {
    /// Bill one request at the virtual now (see the module docs).
    fn bill(&mut self, body: &Value) -> (u64, u64, u64) {
        let (now, ttl) = (self.now, self.script.ttl);
        self.entries.retain(|_, at| now.saturating_sub(*at) < ttl);
        let blocks = blocks(body);
        let total = blocks.last().map_or(0, |(k, _)| k.len() as u64);
        let Some(last_mark) = blocks.iter().rposition(|(_, m)| *m) else {
            return (0, 0, total);
        };
        let hit = blocks[..=last_mark]
            .iter()
            .rposition(|(k, _)| self.entries.contains_key(k));
        let read = hit.map_or(0, |i| blocks[i].0.len() as u64);
        if let Some(i) = hit {
            self.entries.insert(blocks[i].0.clone(), now);
        }
        let through = blocks[last_mark].0.len() as u64;
        let mut write = 0;
        for (key, mark) in &blocks[..=last_mark] {
            if *mark && !self.entries.contains_key(key) {
                self.entries.insert(key.clone(), now);
                write = through - read;
            }
        }
        (read, write, total - through)
    }
}

fn sse(id: &str, usage: (u64, u64, u64), block: Value, delta: Value, stop: &str) -> String {
    let (read, write, input) = usage;
    let usage = json!({
        "input_tokens": input,
        "output_tokens": 1,
        "cache_creation_input_tokens": write,
        "cache_read_input_tokens": read,
    });
    let mut out = format!(
        "data: {}\n\n",
        json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant",
            "content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,
            "usage":usage}})
    );
    out += &format!(
        "data: {}\n\n",
        json!({"type":"content_block_start","index":0,"content_block":block})
    );
    out += &format!(
        "data: {}\n\n",
        json!({"type":"content_block_delta","index":0,"delta":delta})
    );
    out += "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    out += &format!(
        "data: {}\n\n",
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":usage})
    );
    out + "data: {\"type\":\"message_stop\"}\n\n"
}

/// Serve the simulated provider. A request that is neither a ping nor
/// carries a tool result is answered with a `tool_use` of `tool` with
/// `input`, every `{go}` in it replaced by the round's `go` file.
pub async fn spawn(script: Script, tool: &str, input: Value) -> (String, Arc<Mutex<Sim>>) {
    let sim = Arc::new(Mutex::new(Sim::new(script)));
    let state = sim.clone();
    let (tool, input) = (tool.to_string(), input.to_string());
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, raw: axum::body::Bytes| {
                    let state = state.clone();
                    let (tool, input) = (tool.clone(), input.clone());
                    async move {
                        let bytes = if headers.get("content-encoding").is_some_and(|v| v == "gzip")
                        {
                            let mut out = Vec::new();
                            flate2::read::GzDecoder::new(&raw[..])
                                .read_to_end(&mut out)
                                .unwrap();
                            out
                        } else {
                            raw.to_vec()
                        };
                        let body: Value = serde_json::from_slice(&bytes).unwrap();
                        let (reply, hold) = {
                            let mut sim = state.lock().unwrap();
                            let (read, write, uncached) = sim.bill(&body);
                            let req = Req {
                                at: Instant::now(),
                                vt: sim.now,
                                body,
                                read,
                                write,
                                input: uncached,
                            };
                            let usage = (read, write, uncached);
                            // Unique over the sim's life (`take` clears the
                            // log): a resumed worker's history holds the earlier
                            // round's call too, and each round has its own `go`.
                            sim.seq += 1;
                            let call_id = format!("toolu_{}", sim.seq);
                            let mut hold = None;
                            let reply = if req.is_ping() {
                                sim.pings += 1;
                                sim.round_pings += 1;
                                let step = sim.script.per_ping;
                                sim.now += step;
                                if sim.round_pings >= sim.script.release_after {
                                    sim.release_tool();
                                }
                                if sim.script.hold_ping == Some(sim.pings) {
                                    hold = Some(Arc::clone(&sim.held));
                                }
                                sse(
                                    "msg_ping",
                                    usage,
                                    json!({"type":"text","text":""}),
                                    json!({"type":"text_delta","text":"ok"}),
                                    "end_turn",
                                )
                            } else if req.has_tool_result() {
                                sse(
                                    "msg_done",
                                    usage,
                                    json!({"type":"text","text":""}),
                                    json!({"type":"text_delta","text":"done"}),
                                    "end_turn",
                                )
                            } else {
                                let go = sim.go_dir.path().join(&call_id);
                                let tool_input = input.replace("{go}", go.to_str().unwrap());
                                sim.release_tool();
                                sim.pending_go = Some(go);
                                sim.round_pings = 0;
                                let step = sim.script.per_round;
                                sim.now += step;
                                if sim.script.release_after == 0 {
                                    sim.release_tool();
                                }
                                sse(
                                    "msg_tool",
                                    usage,
                                    json!({"type":"tool_use","id":call_id,"name":tool,"input":{}}),
                                    json!({"type":"input_json_delta","partial_json":tool_input}),
                                    "tool_use",
                                )
                            };
                            sim.requests.push(req);
                            (reply, hold)
                        };
                        if let Some(held) = hold {
                            let _ = tokio::time::timeout(Duration::from_secs(600), held.notified())
                                .await;
                        }
                        ([("content-type", "text/event-stream")], reply).into_response()
                    }
                },
            ),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), sim)
}

/// `body` with every `cache_control` removed.
pub fn without_markers(body: &Value) -> Value {
    strip(body)
}

/// Where the markers sit: `(message, block)` per marked message block.
pub fn marker_layout(body: &Value) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, message) in body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (j, block) in message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if block.get("cache_control").is_some() {
                out.push((i, j));
            }
        }
    }
    out
}

/// The tool-round keep-warm contract for one worker or turn: `requests` are
/// the round's request (`[0]`), its pings, then the request carrying the
/// tool result. Every ping is the round's request byte for byte (tools,
/// system, thinking, markers, messages) plus one trailing ping text block
/// in the last user message, reads the whole cached prefix and writes
/// nothing; the request after the round reads the round's whole prefix.
/// Returns the number of pings.
pub fn assert_round_kept_warm(label: &str, requests: &[Req]) -> usize {
    assert!(
        requests.len() >= 3,
        "{label}: round, ping(s), result: {}",
        requests.len()
    );
    let round = &requests[0];
    assert!(
        !round.is_ping() && !round.has_tool_result(),
        "{label}: the round first"
    );
    assert!(
        round.cached_prefix() > 0,
        "{label}: the round caches its prefix"
    );
    let result = requests.last().unwrap();
    assert!(result.has_tool_result(), "{label}: the tool result last");
    let pings = &requests[1..requests.len() - 1];
    for (i, ping) in pings.iter().enumerate() {
        let label = format!("{label}: ping {i}");
        assert!(ping.is_ping(), "{label}: only pings inside the round");
        for key in ["model", "tools", "system", "thinking", "stream"] {
            assert_eq!(ping.body[key], round.body[key], "{label}: {key}");
        }
        // Minimal output: one token past a budgeted thinking block (the
        // thinking config must match the round's for the cache to hold).
        let floor = match &round.body["thinking"] {
            t if t["type"] == "enabled" => t["budget_tokens"].as_u64().unwrap(),
            _ => 0,
        };
        assert_eq!(
            ping.body["max_tokens"],
            json!(floor + 1),
            "{label}: minimal output"
        );
        assert_eq!(
            marker_layout(&ping.body),
            marker_layout(&round.body),
            "{label}: the breakpoint layout"
        );
        let (pm, rm) = (
            ping.body["messages"].as_array().unwrap(),
            round.body["messages"].as_array().unwrap(),
        );
        assert_eq!(
            pm.len(),
            rm.len(),
            "{label}: the ping folds into the last message"
        );
        assert_eq!(pm[..pm.len() - 1], rm[..rm.len() - 1], "{label}: history");
        let (pl, rl) = (
            pm.last().unwrap()["content"].as_array().unwrap(),
            rm.last().unwrap()["content"].as_array().unwrap(),
        );
        assert_eq!(
            pl[..pl.len() - 1],
            rl[..],
            "{label}: the round's last message"
        );
        let tail = pl.last().unwrap();
        assert!(
            tail["text"].as_str().unwrap().starts_with(PING_MARK),
            "{label}: {tail}"
        );
        assert!(
            tail.get("cache_control").is_none(),
            "{label}: an uncached tail"
        );
        assert_eq!(
            ping.read,
            round.cached_prefix(),
            "{label}: reads the whole prefix"
        );
        assert_eq!(ping.write, 0, "{label}: writes nothing");
    }
    assert!(
        result.read >= round.cached_prefix(),
        "{label}: the request after the round reads the round's prefix \
         (read {}, prefix {})",
        result.read,
        round.cached_prefix()
    );
    pings.len()
}

/// Everything the provider saw so far, clearing the log.
pub fn take(sim: &Arc<Mutex<Sim>>) -> Vec<Req> {
    std::mem::take(&mut sim.lock().unwrap().requests)
}

/// The pings' interval in the worker tests (`tool_keepwarm::testing`):
/// real time, it only paces them.
pub const WORKER_INTERVAL: Duration = Duration::from_millis(40);

/// What a worker's `bash` call runs: wait for the round's `go` file.
pub const WORKER_COMMAND: &str = "while [ ! -e '{go}' ]; do sleep 0.02; done";

/// One worker's requests with tool-round keep-warm on: kept warm (see
/// [`assert_round_kept_warm`]), with the pings repeating.
pub fn assert_worker_kept_warm(label: &str, requests: &[Req]) {
    let pings = assert_round_kept_warm(label, requests);
    assert!(pings >= 2, "{label}: repeats while the tool runs: {pings}");
}

/// One worker's requests with tool-round keep-warm off: no ping, and the
/// request after the round finds the prefix expired and rewrites it.
pub fn assert_worker_rewrites(label: &str, requests: &[Req]) {
    assert_eq!(requests.len(), 2, "{label}: round and result only");
    assert!(!requests.iter().any(Req::is_ping), "{label}: no ping");
    assert_eq!(requests[1].read, 0, "{label}: expired");
    assert!(
        requests[1].write >= requests[0].cached_prefix(),
        "{label}: rewritten"
    );
}
