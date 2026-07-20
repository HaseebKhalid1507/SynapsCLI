//! Shared fixtures for the Phase 2 trace-conformance harness
//! (`tests/phase2_trace_conformance.rs`). Loopback-only: every server binds
//! `127.0.0.1:0`; nothing here can reach a non-loopback address.
//!
//! Multiple integration-test binaries include this module via `#[path]`;
//! each binary only uses a subset of the helpers, so per-binary dead-code
//! lints are expected and suppressed here.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use synaps_cli::runtime::trace::{RequestTrace, TransportKind};
use tempfile::TempDir;

// ─── Sentinels (synthetic — never real prompts or credentials) ───────────────

pub const S_PROMPT: &str = "PH2-SENTINEL-PROMPT-71b4c09d";
pub const S_SYSTEM: &str = "PH2-SENTINEL-SYSTEM-2fe6a1d3";
pub const S_TOOL_ARGS: &str = "PH2-SENTINEL-TOOLARGS-9c04eb55";
pub const S_TOOL_RESULT: &str = "PH2-SENTINEL-TOOLRESULT-c33d1a76";
pub const S_TOKEN: &str = "PH2-SENTINEL-ACCESS-TOKEN-e19f2b84";
pub const S_PROVIDER_ERR: &str = "PH2-SENTINEL-PROVIDER-ERROR-5da2c4f1";
pub const S_NESTED: &str = "PH2-SENTINEL-NESTED-SECRET-0a8be327";

pub fn all_sentinels() -> [&'static str; 7] {
    [
        S_PROMPT,
        S_SYSTEM,
        S_TOOL_ARGS,
        S_TOOL_RESULT,
        S_TOKEN,
        S_PROVIDER_ERR,
        S_NESTED,
    ]
}

// ─── Provider wire fixtures ──────────────────────────────────────────────────

/// Minimal Anthropic Messages SSE success body.
pub const ANTHROPIC_SSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Anthropic SSE prefix — a started stream with one text delta and NO
/// terminal event, for cancellation fixtures (pair with `Script::Endless`).
pub const ANTHROPIC_SSE_PREFIX: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
);

/// Anthropic Messages SSE turn that requests the innocuous local `ls` tool
/// (empty input → lists the CWD) and stops with `tool_use` — the engine
/// tool loop then executes the tool and issues a continuation request.
pub const ANTHROPIC_SSE_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_03\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_ph2\",\"name\":\"ls\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Minimal OpenAI Chat Completions SSE success body.
pub const OAI_CHAT_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,",
    "\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n",
    "data: [DONE]\n\n",
);

/// Minimal OpenAI Responses SSE success body.
pub const OAI_RESPONSES_SSE: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":",
    "{\"input_tokens\":9,\"output_tokens\":2}}}\n\n",
    "data: [DONE]\n\n",
);

/// Minimal Gemini Code Assist SSE success body.
pub const GEMINI_SSE: &str = concat!(
    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}\n\n",
    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]},",
    "\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,",
    "\"candidatesTokenCount\":4}}}\n\n",
    "data: [DONE]\n\n",
);

/// Cloud broker `POST /cloud/invoke` newline-delimited `CloudEvent` body.
pub const CLOUD_EVENTS: &str = concat!(
    "{\"type\":\"text_delta\",\"delta\":\"cloud \"}\n",
    "{\"type\":\"text_delta\",\"delta\":\"hi\"}\n",
    "{\"type\":\"usage\",\"input_tokens\":7,\"output_tokens\":2}\n",
    "{\"type\":\"done\"}\n",
);

// ─── Loopback servers ────────────────────────────────────────────────────────

pub type Bodies = Arc<Mutex<Vec<Vec<u8>>>>;

pub async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn sse(body: String) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        body,
    )
        .into_response()
}

/// What a stub endpoint does with each request, in arrival order.
#[derive(Clone)]
pub enum Script {
    /// Immediate SSE success with this body.
    Sse(&'static str),
    /// `fails` requests answered `status` (with an echo/hostile body), then
    /// SSE success with `then`.
    FailThen {
        fails: usize,
        status: u16,
        then: &'static str,
    },
    /// Every request answered `status`, body echoes the request (hostile
    /// provider — provider-controlled error text).
    AlwaysFailEcho(u16),
    /// One data frame, then an endless slow keep-alive stream (for cancel).
    Endless(&'static str),
    /// `preamble` once, then `frame` repeated FOREVER at full speed — a
    /// hostile unbounded-volume delta flood (CP-11 fix-2 A/B fixtures).
    FloodSse {
        preamble: &'static str,
        frame: &'static str,
    },
    /// SSE bodies answered per arrival order; the last body repeats for any
    /// further hits (tool-loop fixtures: tool-use turn, then continuation).
    SeqSse(&'static [&'static str]),
    /// Delay headers, then a comment first byte, then the model events, with
    /// each SSE frame fragmented into small chunks.
    Timed {
        header_delay: Duration,
        first_byte_delay: Duration,
        event_delay: Duration,
        body: &'static str,
    },
}

fn scripted_response(script: &Script, hit: usize, req_body: &[u8]) -> Response {
    match script {
        Script::Sse(body) => sse((*body).to_string()),
        Script::SeqSse(bodies) => {
            let body = bodies
                .get(hit)
                .or_else(|| bodies.last())
                .expect("SeqSse requires at least one body");
            sse((*body).to_string())
        }
        Script::FailThen { fails, status, then } => {
            if hit < *fails {
                (
                    StatusCode::from_u16(*status).unwrap(),
                    // retry-after: 0 keeps header-aware backoff at zero so
                    // retry fixtures stay fast and deterministic.
                    [("content-type", "application/json"), ("retry-after", "0")],
                    format!(
                        "{{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"ECHOED:{}\"}}}}",
                        String::from_utf8_lossy(req_body).escape_default()
                    ),
                )
                    .into_response()
            } else {
                sse((*then).to_string())
            }
        }
        Script::AlwaysFailEcho(status) => (
            StatusCode::from_u16(*status).unwrap(),
            [("content-type", "application/json"), ("retry-after", "0")],
            format!(
                "{{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"ECHOED:{} {}\"}}}}",
                S_PROVIDER_ERR,
                String::from_utf8_lossy(req_body).escape_default()
            ),
        )
            .into_response(),
        Script::Endless(first) => {
            let first = (*first).to_string();
            let stream = futures::stream::unfold(0u64, move |i| {
                let first = first.clone();
                async move {
                    if i == 0 {
                        return Some((
                            Ok::<_, std::convert::Infallible>(Bytes::from(first)),
                            1,
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((Ok(Bytes::from(": keep-alive\n\n")), i + 1))
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Script::FloodSse { preamble, frame } => {
            let preamble = (*preamble).to_string();
            let frame = Bytes::from((*frame).to_string());
            let stream = futures::stream::unfold(0u64, move |i| {
                let preamble = preamble.clone();
                let frame = frame.clone();
                async move {
                    if i == 0 && !preamble.is_empty() {
                        return Some((
                            Ok::<_, std::convert::Infallible>(Bytes::from(preamble)),
                            1,
                        ));
                    }
                    // Yield each frame so the stub stays cancellable; TCP
                    // backpressure paces production to the reader.
                    tokio::task::yield_now().await;
                    Some((Ok(frame), i + 1))
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Script::Timed {
            header_delay,
            first_byte_delay,
            event_delay,
            body,
        } => {
            // Fragment every SSE frame into ≤7-byte chunks: the decoder must
            // reassemble frames split at arbitrary byte boundaries.
            let frags: Vec<Bytes> = body
                .as_bytes()
                .chunks(7)
                .map(|c| Bytes::copy_from_slice(c))
                .collect();
            let first_byte_delay = *first_byte_delay;
            let event_delay = *event_delay;
            let header_delay = *header_delay;
            let stream = futures::stream::unfold(
                (0usize, frags),
                move |(i, frags)| async move {
                    if i == 0 {
                        // First byte on the wire: an SSE comment — not a
                        // model event, so first_byte and first_model_event
                        // land in different buckets.
                        tokio::time::sleep(first_byte_delay).await;
                        return Some((
                            Ok::<_, std::convert::Infallible>(Bytes::from(": preamble\n\n")),
                            (1, frags),
                        ));
                    }
                    if i == 1 {
                        tokio::time::sleep(event_delay).await;
                    }
                    frags
                        .get(i - 1)
                        .cloned()
                        .map(|b| (Ok(b), (i + 1, frags)))
                },
            );
            let header_sleep = header_delay;
            // Header delay was already applied by `spawn_stub` before this
            // response was built; nothing further to do with it here.
            let _ = header_sleep;
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

/// Spawn a loopback stub answering EVERY path with `script`. Returns
/// `(base_url, hit_counter, captured_request_bodies)`.
pub async fn spawn_stub(script: Script) -> (String, Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let hits_c = Arc::clone(&hits);
    let bodies_c = Arc::clone(&bodies);
    let app = Router::new().fallback(move |body: Bytes| {
        let hits = Arc::clone(&hits_c);
        let bodies = Arc::clone(&bodies_c);
        let script = script.clone();
        async move {
            let hit = hits.fetch_add(1, Ordering::SeqCst);
            bodies.lock().unwrap().push(body.to_vec());
            if let Script::Timed { header_delay, .. } = &script {
                tokio::time::sleep(*header_delay).await;
            }
            scripted_response(&script, hit, &body)
        }
    });
    (serve(app).await, hits, bodies)
}

/// What the fake remote credential broker returns for `POST /proxy` and
/// `POST /cloud/invoke`. `GET /token` always vends a synthetic token.
#[derive(Clone)]
pub enum BrokerScript {
    /// `/proxy` streams this SSE body.
    ProxySse(&'static str),
    /// `/proxy` answers `status` `fails` times (RemoteBroker surfaces a
    /// typed transport error; the body is dropped unread), then streams
    /// `then`.
    ProxyFailThen {
        fails: usize,
        status: u16,
        then: &'static str,
    },
    /// `/proxy` streams one frame then stalls forever (for cancel tests).
    ProxyEndless(&'static str),
    /// `/proxy` streams `preamble` once then `frame` FOREVER (hostile
    /// unbounded-volume delta flood through the broker wire).
    ProxyFlood {
        preamble: &'static str,
        frame: &'static str,
    },
    /// `/cloud/invoke` streams these newline-delimited CloudEvent lines.
    CloudLines(&'static str),
    /// `/cloud/invoke` answers HTTP 500.
    CloudFail,
    /// `/cloud/invoke` streams one text delta then stalls (for cancel).
    CloudEndless,
}

/// Spawn a fake remote `synaps auth-broker`. Returns
/// `(endpoint, hit_counter, captured /proxy + /cloud/invoke bodies)`.
pub async fn spawn_broker(script: BrokerScript) -> (String, Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let hits_c = Arc::clone(&hits);
    let bodies_c = Arc::clone(&bodies);
    let script_c = script.clone();
    let proxy = post(move |body: Bytes| {
        let hits = Arc::clone(&hits_c);
        let bodies = Arc::clone(&bodies_c);
        let script = script_c.clone();
        async move {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            // Non-streaming broker RPC (e.g. Gemini `loadCodeAssist` setup):
            // answer a benign buffered `ProxyResponse` envelope.
            if !parsed["stream"].as_bool().unwrap_or(false) {
                return (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    serde_json::json!({
                        "status": 200,
                        "body": "{\"cloudaicompanionProject\":\"fixture-project\",\
                                 \"currentTier\":{\"id\":\"free-tier\"}}"
                    })
                    .to_string(),
                )
                    .into_response();
            }
            let hit = hits.fetch_add(1, Ordering::SeqCst);
            bodies.lock().unwrap().push(body.to_vec());
            match script {
                BrokerScript::ProxySse(b) => sse(b.to_string()),
                BrokerScript::ProxyFailThen {
                    fails,
                    status,
                    then,
                } => {
                    if hit < fails {
                        (
                            StatusCode::from_u16(status).unwrap(),
                            [("retry-after", "0")],
                            "broker upstream failure",
                        )
                            .into_response()
                    } else {
                        sse(then.to_string())
                    }
                }
                BrokerScript::ProxyEndless(first) => {
                    scripted_response(&Script::Endless(first), hit, &[])
                }
                BrokerScript::ProxyFlood { preamble, frame } => {
                    scripted_response(&Script::FloodSse { preamble, frame }, hit, &[])
                }
                _ => (StatusCode::NOT_FOUND, "no proxy scripted").into_response(),
            }
        }
    });
    let hits_c2 = Arc::clone(&hits);
    let bodies_c2 = Arc::clone(&bodies);
    let cloud = post(move |body: Bytes| {
        let hits = Arc::clone(&hits_c2);
        let bodies = Arc::clone(&bodies_c2);
        let script = script.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            bodies.lock().unwrap().push(body.to_vec());
            match script {
                BrokerScript::CloudLines(lines) => (
                    StatusCode::OK,
                    [("content-type", "application/x-ndjson")],
                    lines.to_string(),
                )
                    .into_response(),
                BrokerScript::CloudFail => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "cloud upstream 500").into_response()
                }
                BrokerScript::CloudEndless => {
                    let stream = futures::stream::unfold(0u64, |i| async move {
                        if i == 0 {
                            return Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(
                                    "{\"type\":\"text_delta\",\"delta\":\"x\"}\n",
                                )),
                                1,
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        Some((Ok(Bytes::from("")), i + 1))
                    });
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
                _ => (StatusCode::NOT_FOUND, "no cloud scripted").into_response(),
            }
        }
    });
    let token = get(|| async {
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            format!(
                "{{\"access_token\":\"{S_TOKEN}\",\"expires\":9999999999999,\"ttl_ms\":3600000}}"
            ),
        )
    });
    let app = Router::new()
        .route("/proxy", proxy)
        .route("/cloud/invoke", cloud)
        .route("/token", token);
    (serve(app).await, hits, bodies)
}

// ─── Environment isolation (serial tests only) ───────────────────────────────

/// Synthetic Anthropic OAuth credential (non-expired) so the Local broker
/// resolves without network. The access token doubles as a header-secret
/// sentinel for the exfiltration scenario.
pub fn synthetic_auth_json() -> String {
    format!(
        "{{\"anthropic\": {{\"type\": \"oauth\", \"refresh\": \"synthetic-refresh\", \"access\": \"{S_TOKEN}\", \"expires\": 9999999999999}}}}"
    )
}

/// Process-global env guard: temp HOME + SYNAPS_BASE_DIR, all provider keys
/// removed. Tests using it MUST be `#[serial]`. Restores prior env on drop.
pub struct HomeGuard {
    pub home: TempDir,
    saved: Vec<(&'static str, Option<String>)>,
}

const GUARDED_VARS: &[&str] = &[
    "HOME",
    "SYNAPS_BASE_DIR",
    "SYNAPS_ANTHROPIC_BASE_URL",
    "SYNAPS_AUTH_ENDPOINT",
    "SYNAPS_MACHINE_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "RUST_LOG",
];

impl HomeGuard {
    pub fn new() -> Self {
        let home = TempDir::new().expect("temp home");
        let saved = GUARDED_VARS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for k in GUARDED_VARS {
            std::env::remove_var(k);
        }
        let base = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(base.join("auth.json"), synthetic_auth_json()).unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("SYNAPS_BASE_DIR", &base);
        Self { home, saved }
    }

    pub fn base_dir(&self) -> PathBuf {
        self.home.path().join(".synaps-cli")
    }

    /// Default persisted trace log path under this HOME.
    pub fn trace_log(&self) -> PathBuf {
        self.home.path().join(".cache/synaps/request-trace.jsonl")
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

// ─── Trace-record validation ─────────────────────────────────────────────────

/// Strict-parse every line of a persisted trace log through the production
/// `RequestTrace` reader (schema tag, bounded IDs, enums all validated).
pub fn read_traces(path: &Path) -> Vec<RequestTrace> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read trace log {}: {e}", path.display()));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<RequestTrace>(line).unwrap_or_else(|e| {
                panic!("trace line failed strict schema validation: {e}\n{line}")
            })
        })
        .collect()
}

/// Envelope invariants every record must satisfy, plus a deterministic
/// serialize→strict-reparse round-trip and a sentinel scan.
pub fn assert_record_conformant(r: &RequestTrace) {
    assert_eq!(
        r.attempt as usize,
        r.outcome.retries.len() + 1,
        "attempt ordinal must equal prior retries + 1"
    );
    let json = serde_json::to_string(r).expect("record serializes");
    assert!(
        json.contains("synaps-request-trace/1"),
        "schema tag missing from serialized record"
    );
    for s in all_sentinels() {
        assert!(
            !json.contains(s),
            "sentinel {s} leaked into trace record: {json}"
        );
    }
    let back: RequestTrace =
        serde_json::from_str(&json).expect("serialized record must re-validate strictly");
    assert_eq!(&back, r, "record must round-trip deterministically");
}

/// Scan a whole directory tree for sentinel leaks, skipping paths for which
/// `skip` returns true (e.g. session transcripts, which legitimately contain
/// the user's own prompt text).
pub fn scan_tree_for_sentinels(root: &Path, skip: &dyn Fn(&Path) -> bool) -> Vec<String> {
    let mut leaks = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if skip(&path) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for s in all_sentinels() {
                if text.contains(s) {
                    leaks.push(format!("{} contains {s}", path.display()));
                }
            }
        }
    }
    leaks
}

// ─── Turn drivers (shared by the conformance tests) ──────────────────────────

use synaps_cli::auth::{CredentialSource, TokenCache};
use synaps_cli::runtime::trace::{CollectingTraceSink, TraceContext};
use synaps_cli::runtime::{LlmEvent, Runtime, SessionEvent, StreamEvent};
use synaps_cli::{SharedMessage, TurnOutcome};
use tokio_util::sync::CancellationToken;

/// Drain a full `Runtime::run_stream` turn; optionally cancel after the
/// first streamed text delta. Returns every event observed.
pub async fn drive_runtime_turn(
    rt: &Runtime,
    prompt: &str,
    cancel_after_first_text: bool,
) -> Vec<StreamEvent> {
    use futures::StreamExt;
    let cancel = CancellationToken::new();
    let mut stream = rt.run_stream(prompt.to_string(), cancel.clone()).await;
    let mut events = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("turn hung beyond 30 s")
    {
        if cancel_after_first_text {
            if let StreamEvent::Llm(LlmEvent::Text(_)) = &ev {
                cancel.cancel();
            }
        }
        let done = matches!(ev, StreamEvent::Session(SessionEvent::Done));
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

/// Drain a prebuilt multi-message turn (`run_stream_with_messages`),
/// returning every event observed so callers can scan the user-surfaced
/// error/notice strings.
pub async fn drive_runtime_history_turn(
    rt: &Runtime,
    history: Vec<SharedMessage>,
) -> Vec<StreamEvent> {
    use futures::StreamExt;
    let mut s = rt
        .run_stream_with_messages(history, CancellationToken::new(), None, None, false)
        .await;
    let mut events = Vec::new();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), s.next())
        .await
        .expect("history turn hung beyond 30 s")
    {
        let done = matches!(ev, StreamEvent::Session(SessionEvent::Done));
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

/// Every user-surfaced terminal-error / notice string from a turn's events
/// — exactly what a headless frontend would print.
pub fn surfaced_error_strings(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(SessionEvent::Error(err)) => Some(err.message.clone()),
            StreamEvent::Session(SessionEvent::Notice(n)) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// Hostile-echo honesty: the surfaced error/notice strings must exist (the
/// failure is reported) yet contain NO sentinel and none of the
/// provider-controlled `ECHOED:` body the hostile stub reflects back.
pub fn assert_surfaced_errors_sentinel_free(events: &[StreamEvent]) {
    let surfaced = surfaced_error_strings(events);
    assert!(
        !surfaced.is_empty(),
        "failure fixture must surface at least one error/notice string"
    );
    for s in &surfaced {
        for sentinel in all_sentinels() {
            assert!(
                !s.contains(sentinel),
                "sentinel {sentinel} leaked into a surfaced error string: {s}"
            );
        }
        assert!(
            !s.contains("ECHOED:"),
            "provider-controlled echoed body leaked into a surfaced error string: {s}"
        );
    }
}

pub fn turn_completed(events: &[StreamEvent]) -> bool {
    !events
        .iter()
        .any(|e| matches!(e, StreamEvent::Session(SessionEvent::Error(_))))
}

/// One `try_route` request through the REAL routing entry point (the same
/// call `runtime/api.rs` makes), with an independent collecting sink.
pub struct RouteRun {
    pub result: Result<serde_json::Value, String>,
    pub records: Vec<RequestTrace>,
}

#[allow(clippy::too_many_arguments)]
pub async fn drive_try_route(
    model: &str,
    source: &CredentialSource,
    trace: &TraceContext,
    sink: &Arc<CollectingTraceSink>,
    messages: Vec<SharedMessage>,
    tools: Vec<serde_json::Value>,
    system: Option<String>,
    cancel_after_first_text: bool,
) -> RouteRun {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
    let cancel = CancellationToken::new();
    let cancel_c = cancel.clone();
    let watcher = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if cancel_after_first_text {
                if let StreamEvent::Llm(LlmEvent::Text(_)) = ev {
                    cancel_c.cancel();
                }
            }
        }
    });
    let tools = Arc::new(tools);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        synaps_cli::runtime::openai::try_route(
            model,
            &reqwest::Client::new(),
            &tools,
            &system,
            &messages,
            &tx,
            None,
            None,
            0,
            synaps_cli::reasoning::ReasoningLevel::Adaptive,
            &cancel,
            source,
            &TokenCache::new(),
            1,
            synaps_cli::runtime::openai::catalog::ExecutionRole::Foreground,
            None,
            None,
            trace,
            false,
        ),
    )
    .await
    .expect("try_route hung beyond 30 s")
    .expect("model must resolve to a routed provider")
    .map_err(|e| e.to_string());
    drop(tx);
    let _ = watcher.await;
    RouteRun {
        result,
        records: sink.records(),
    }
}

pub fn collecting_ctx(tmp: &TempDir) -> (TraceContext, Arc<CollectingTraceSink>) {
    let sink = CollectingTraceSink::new();
    let ctx =
        TraceContext::with_sink(sink.clone()).with_key_path(tmp.path().join("trace/digest.key"));
    (ctx, sink)
}

pub fn remote(endpoint: &str) -> CredentialSource {
    CredentialSource::Remote {
        endpoint: endpoint.to_string(),
        machine_token: "machine-token-fixture".to_string(),
    }
}

pub fn user_msg(text: &str) -> SharedMessage {
    Arc::new(serde_json::json!({"role": "user", "content": text}))
}

pub fn is_completed(r: &RequestTrace) -> bool {
    r.outcome.terminal == TurnOutcome::Completed
}

pub fn is_canceled(r: &RequestTrace) -> bool {
    r.outcome.terminal == TurnOutcome::Canceled
}

pub fn is_provider_failed(r: &RequestTrace) -> bool {
    matches!(r.outcome.terminal, TurnOutcome::ProviderFailed { .. })
}

/// Spawn a fresh loopback broker with `script`, then drive one `try_route`
/// request for `model` against it (remote credential source, fresh
/// collecting sink + key). The workhorse behind the per-family S1/S4 cases.
pub async fn broker_route_run(
    model: &str,
    script: BrokerScript,
    messages: Vec<SharedMessage>,
    tools: Vec<serde_json::Value>,
    system: Option<String>,
    cancel_after_first_text: bool,
) -> RouteRun {
    let (endpoint, _, _) = spawn_broker(script).await;
    let tmp = TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    drive_try_route(
        model,
        &remote(&endpoint),
        &ctx,
        &sink,
        messages,
        tools,
        system,
        cancel_after_first_text,
    )
    .await
}

/// Common failure-fixture asserts: request errored, ≥1 conformant record,
/// at least one `ProviderFailed` terminal.
pub fn assert_failure_run(run: &RouteRun) {
    assert!(run.result.is_err(), "failure fixture must surface an error");
    assert!(!run.records.is_empty(), "failure must emit a record");
    for r in &run.records {
        assert_record_conformant(r);
    }
    assert!(
        run.records.iter().any(is_provider_failed),
        "{:#?}",
        run.records
    );
}

/// Common cancel-fixture asserts: request errored, ≥1 conformant record,
/// at least one `Canceled` terminal.
pub fn assert_cancel_run(run: &RouteRun) {
    assert!(run.result.is_err(), "canceled turn must not report success");
    assert!(!run.records.is_empty(), "cancel must emit a record");
    for r in &run.records {
        assert_record_conformant(r);
    }
    assert!(run.records.iter().any(is_canceled), "{:#?}", run.records);
}

/// Common success-fixture asserts for remote-broker runs: exactly one
/// conformant `Completed` record, honest `wire: None`, and the documented
/// remote-broker transport label — `CloudProxy` (the exact provider bytes
/// are serialized behind the broker, so the transport must not claim the
/// provider-direct kind). The wire family is pinned via `expected_path`.
pub fn assert_remote_success<'a>(run: &'a RouteRun, expected_path: &str) -> &'a RequestTrace {
    run.result
        .as_ref()
        .unwrap_or_else(|e| panic!("{expected_path} success fixture failed: {e}"));
    assert_eq!(
        run.records.len(),
        1,
        "{expected_path}: one record per attempt"
    );
    let r = &run.records[0];
    assert_record_conformant(r);
    assert_eq!(
        r.transport,
        TransportKind::CloudProxy,
        "remote-broker sends are honestly labeled CloudProxy"
    );
    assert!(
        r.endpoint.path().ends_with(expected_path),
        "wire family via endpoint path: {} !~ {expected_path}",
        r.endpoint.path()
    );
    assert!(is_completed(r), "{expected_path}: terminal Completed");
    assert!(
        r.wire.is_none(),
        "{expected_path}: remote broker must not claim wire bytes"
    );
    r
}

/// Run the real `synaps trace export` binary against `guard`'s HOME.
pub fn run_export_cli(guard: &HomeGuard, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_synaps"))
        .arg("trace")
        .arg("export")
        .args(args)
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .output()
        .expect("spawn synaps trace export")
}

#[cfg(unix)]
pub fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).expect("stat").mode() & 0o7777
}

/// Load the real python streaming-provider extension fixture into a fresh
/// `ExtensionManager` and install it as the global routing manager. The
/// returned guard shuts the sidecar down and clears the global on drop-call.
pub async fn load_extension_fixture(
    ext_id: &str,
) -> (
    Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    TempDir,
) {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/streaming_provider_extension.py");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");
    load_extension_from_script(ext_id, fixture).await
}

/// A scriptable python provider sidecar (same JSON-RPC framing as the repo
/// fixture) whose `provider.stream` behavior is keyed off the last user
/// message: `PH2-FAIL` → JSON-RPC error (provider failure), `PH2-STALL` →
/// one text delta then a long stall (cancellation fixtures; the sleep is a
/// controlled stub delay, the sidecar is killed at shutdown), anything else
/// → the normal streamed success.
pub const SCRIPTED_EXTENSION_PY: &str = r#"#!/usr/bin/env python3
import json
import sys
import time


def read_frame():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def write_frame(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()


def last_user_text(params):
    for msg in reversed(params.get("messages", [])):
        if msg.get("role") == "user":
            content = msg.get("content")
            if isinstance(content, str):
                return content
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "text":
                        return block.get("text", "")
            return ""
    return ""


while True:
    req = read_frame()
    if req is None:
        break
    method = req.get("method")
    req_id = req.get("id")
    if method == "initialize":
        write_frame({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocol_version": 1,
                "capabilities": {
                    "providers": [{
                        "id": "scripted",
                        "display_name": "Scripted Test Provider",
                        "description": "Failure/cancel fixture provider",
                        "models": [{
                            "id": "scripted-mini",
                            "display_name": "Scripted Mini",
                            "capabilities": {"streaming": True, "tool_use": False},
                            "context_window": 4096
                        }]
                    }]
                }
            }
        })
    elif method == "provider.stream":
        text = last_user_text(req.get("params", {}))
        if "PH2-FAIL" in text:
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32000, "message": "scripted synthetic provider failure"}
            })
        elif "PH2-STALL" in text:
            write_frame({
                "jsonrpc": "2.0",
                "method": "provider.stream.event",
                "params": {"type": "text", "delta": "stall "}
            })
            time.sleep(30)
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": "stall"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            })
        else:
            write_frame({
                "jsonrpc": "2.0",
                "method": "provider.stream.event",
                "params": {"type": "text", "delta": "ok"}
            })
            write_frame({
                "jsonrpc": "2.0",
                "method": "provider.stream.event",
                "params": {"type": "done"}
            })
            write_frame({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            })
    elif method == "shutdown":
        write_frame({"jsonrpc": "2.0", "id": req_id, "result": None})
        break
    else:
        write_frame({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "unknown method"}})
"#;

/// Load [`SCRIPTED_EXTENSION_PY`] (written into the plugin temp dir) as an
/// extension provider through the SAME real routing manager path as the
/// repo fixture.
pub async fn load_scripted_extension_fixture(
    ext_id: &str,
) -> (
    Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    TempDir,
) {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("scripted_provider_extension.py");
    std::fs::write(&script, SCRIPTED_EXTENSION_PY).unwrap();
    load_extension_from_script(ext_id, script).await
}

/// Shared loader: spawn `python3 <script>` as a process extension with the
/// `providers.register` permission and install it as the global routing
/// manager.
pub async fn load_extension_from_script(
    ext_id: &str,
    script: PathBuf,
) -> (
    Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    TempDir,
) {
    let plugin_dir = TempDir::new().unwrap();
    let hook_bus = Arc::new(synaps_cli::extensions::hooks::HookBus::new());
    let manager = Arc::new(tokio::sync::RwLock::new(
        synaps_cli::extensions::manager::ExtensionManager::new(hook_bus),
    ));
    synaps_cli::runtime::openai::set_extension_manager_for_routing(manager.clone());
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        theme_tokens: Default::default(),
        deferred: None,
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: std::collections::HashMap::new(),
        args: vec![script.to_string_lossy().to_string()],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![],
    };
    manager
        .write()
        .await
        .load_with_cwd(ext_id, &manifest, Some(plugin_dir.path().to_path_buf()))
        .await
        .expect("load extension fixture");
    (manager, plugin_dir)
}

/// A minimal handcrafted-but-schema-valid `RequestTrace` for writer-focused
/// tests (bounded shutdown), strict-parsed through the production reader so
/// it can never drift from the real schema.
pub fn handcrafted_trace_record(n: usize) -> RequestTrace {
    serde_json::from_value(serde_json::json!({
        "schema": "synaps-request-trace/1",
        "session_id": "session-shutdown-fixture",
        "turn_id": "turn-shutdown-fixture",
        "request_id": format!("req-shutdown-{n}"),
        "attempt": 1,
        "model": "provider/fixture-model",
        "transport": serde_json::to_value(TransportKind::AnthropicMessages).unwrap(),
        "endpoint": {"host": "127.0.0.1", "path": "/fixture"},
        "anatomy": {
            "system_segment_count": 0, "message_count": 1,
            "block_count": 1, "tool_count": 0
        },
        "system_segments": [],
        "messages": [],
        "tools": [],
        "cache": {"boundaries": []},
        "translation_losses": [],
        "outcome": {"timings": {}, "retries": [], "terminal": {"kind": "completed"}}
    }))
    .expect("handcrafted record must strict-parse")
}
