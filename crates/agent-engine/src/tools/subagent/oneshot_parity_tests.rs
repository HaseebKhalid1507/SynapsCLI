// Oneshot parity (slice 8c): included at the end of oneshot.rs as
// `mod oneshot_parity`. A oneshot whose run has a plan goes through the
// shared launch, records and drive of a reactive worker, on one clock (its
// timeout is the run deadline), and answers in the oneshot's words. Each
// case runs the real `subagent` tool against a loopback Anthropic
// stand-in. Worker rollover is off, so no run wants a plan: the cases give
// one through `dispatch`, with the run's records in a temporary root. The
// run's worker is on the real worker registry; a v2 run gets its own
// owned-history tools there (8g: `o9`, `g1`, `g4`, `g6`).
//
// A rollover is made the way the worker-clock tests make one: low
// thresholds in the parent's settings and long replies, so the window
// passes them after a few rounds. The stand-in tells a rollover's summary
// request (not streamed, or carrying the host's rollover request) from the
// conversation's.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::runtime::subagent::{SubagentState, SubagentStatus, WindowCounters, WorkerVisibility};
use crate::tools::subagent::lineage::{worker_logical_id, Refusal};
use crate::runtime::child_spend::{SinkMessage, SpendParent, SpendReceiver, SpendSink};
use crate::tools::subagent::record::{
    Amount, Candidate, FinalRecord, RecordKind, RequestPurpose, RunId, WorkerRecord,
};
use crate::tools::subagent::record_file;
use crate::tools::subagent::test_fixtures::{self as fx, write_root, WriteRoot, PROCESS_DIR};
use crate::{AgentEvent, SessionEvent, StreamEvent};

// ── The stand-in ────────────────────────────────────────────────────────

/// A conversation reply.
#[derive(Clone)]
enum Reply {
    /// The text, then a `read` call on a path that does not exist.
    ToolUse(String),
    /// The text, ending the turn.
    Text(String),
    /// The text, then nothing more, ever.
    Held(String),
    /// The provider refuses the request (HTTP 400).
    Invalid,
    /// A call of the tool `name` with `input` (its ID names the tool:
    /// `toolu_<name>_<n>`).
    Call { name: &'static str, input: Value },
    /// A `memory_fetch` call of the `i`-th ctx- ID the `memory_search`
    /// results in the request hold.
    FetchFound(usize),
}

/// How the stand-in answers a rollover's summary request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Summary {
    /// At once.
    Answer,
    /// Once released ([`StandIn::release`]); never if it isn't.
    Held,
}

type Script = Arc<dyn Fn(usize, usize) -> Reply + Send + Sync>;

#[derive(Clone)]
struct StandIn {
    /// Conversation requests so far.
    turns: Arc<AtomicUsize>,
    /// Summary requests so far.
    summaries: Arc<AtomicUsize>,
    /// Signalled at each summary request.
    summary_seen: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::watch::Sender<bool>>,
    summary: Summary,
    /// The reply to conversation request `n` after `s` summary requests.
    script: Script,
    /// The records directory to look at when the first conversation
    /// request arrives, and whether a spawn head was in it.
    watch: Arc<Mutex<(Option<PathBuf>, Option<bool>)>>,
    /// Every conversation request's body, in order.
    bodies: Arc<Mutex<Vec<String>>>,
}

impl StandIn {
    fn turns(&self) -> usize {
        self.turns.load(Ordering::SeqCst)
    }

    fn summaries(&self) -> usize {
        self.summaries.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.turns.store(0, Ordering::SeqCst);
        self.summaries.store(0, Ordering::SeqCst);
        self.watch.lock().unwrap().1 = None;
        self.bodies.lock().unwrap().clear();
    }

    /// The conversation requests' bodies so far.
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    fn release(&self) {
        let _ = self.released.send(true);
    }

    /// Look for a spawn head in `dir` when the first conversation request
    /// arrives.
    fn watch(&self, dir: PathBuf) {
        *self.watch.lock().unwrap() = (Some(dir), None);
    }

    fn head_at_first_request(&self) -> Option<bool> {
        self.watch.lock().unwrap().1
    }
}

fn is_summary(body: &str) -> bool {
    !body.contains("\"stream\":true") || body.contains("[Host request at a context rollover")
}

fn usage() -> Value {
    json!({"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":0}})
}

fn sse_events(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn message_start(n: usize) -> Value {
    json!({"type":"message_start","message":{"id":format!("msg_parity_{n}"),"type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage()}})
}

fn text_block(index: usize, text: &str) -> [Value; 3] {
    [
        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":index}),
    ]
}

fn finish(stop: &str) -> [Value; 2] {
    [
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":usage()}),
        json!({"type":"message_stop"}),
    ]
}

fn event_stream(body: String) -> Response {
    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
}

fn reply(n: usize, reply: Reply) -> Response {
    match reply {
        Reply::ToolUse(text) => {
            let mut events = vec![message_start(n)];
            events.extend(text_block(0, &text));
            events.extend([
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":format!("toolu_parity_{n}"),"name":"read","input":{}}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/nonexistent/oneshot-parity.txt\"}"}}),
                json!({"type":"content_block_stop","index":1}),
            ]);
            events.extend(finish("tool_use"));
            event_stream(sse_events(&events))
        }
        Reply::Text(text) => {
            let mut events = vec![message_start(n)];
            events.extend(text_block(0, &text));
            events.extend(finish("end_turn"));
            event_stream(sse_events(&events))
        }
        Reply::Held(text) => {
            let [start, delta, _] = text_block(0, &text);
            let head = sse_events(&[message_start(n), start, delta]);
            let stream = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(head))])
                .chain(futures::stream::pending());
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Reply::Invalid => (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            json!({"type":"error","error":{"type":"invalid_request_error","message":"parity: the request is not valid"}})
                .to_string(),
        )
            .into_response(),
        Reply::Call { name, input } => {
            let mut events = vec![message_start(n)];
            events.extend(text_block(0, &format!("calling {name}")));
            events.extend([
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":format!("toolu_{name}_{n}"),"name":name,"input":{}}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
                json!({"type":"content_block_stop","index":1}),
            ]);
            events.extend(finish("tool_use"));
            event_stream(sse_events(&events))
        }
        Reply::FetchFound(_) => unreachable!("resolved against the request first"),
    }
}

/// The results of the calls whose tool-use ID starts `toolu_<name>` in a
/// request `body`, in order: each one's text and whether it is an error.
fn tool_results(body: &str, name: &str) -> Vec<(String, bool)> {
    let body: Value = serde_json::from_str(body).expect("a JSON request");
    let prefix = format!("toolu_{name}_");
    let mut out = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        for block in message["content"].as_array().into_iter().flatten() {
            let id = block["tool_use_id"].as_str().unwrap_or_default();
            if block["type"] != "tool_result" || !id.starts_with(&prefix) {
                continue;
            }
            let text = match &block["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => other.to_string(),
            };
            out.push((text, block["is_error"] == true));
        }
    }
    out
}

/// The ctx- IDs a text holds, in order.
fn ctx_ids(text: &str) -> Vec<String> {
    regex::Regex::new(r"ctx-[0-9a-f]{32}")
        .unwrap()
        .find_iter(text)
        .map(|m| m.as_str().to_owned())
        .collect()
}

/// The names of the tools a request `body` offers, sorted.
fn offered(body: &str) -> std::collections::BTreeSet<String> {
    let body: Value = serde_json::from_str(body).expect("a JSON request");
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

const SUMMARY_TEXT: &str = "## Window 1\nThe worker read the round replies; nothing is left open.";

async fn answer(State(s): State<StandIn>, body: String) -> Response {
    if is_summary(&body) {
        s.summaries.fetch_add(1, Ordering::SeqCst);
        s.summary_seen.notify_one();
        if s.summary == Summary::Held {
            let mut released = s.released.subscribe();
            let _ = released.wait_for(|r| *r).await;
        }
        if body.contains("\"stream\":true") {
            return reply(usize::MAX, Reply::Text(SUMMARY_TEXT.into()));
        }
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            json!({"id":"msg_parity_summary","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":SUMMARY_TEXT}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}})
                .to_string(),
        )
            .into_response();
    }
    let n = s.turns.fetch_add(1, Ordering::SeqCst);
    s.bodies.lock().unwrap().push(body.clone());
    if n == 0 {
        let mut watch = s.watch.lock().unwrap();
        if let Some(dir) = watch.0.clone() {
            let head = std::fs::read_dir(&dir).is_ok_and(|entries| {
                entries.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".head.json"))
            });
            watch.1 = Some(head);
        }
    }
    let r = match (s.script)(n, s.summaries.load(Ordering::SeqCst)) {
        Reply::FetchFound(i) => {
            let found = tool_results(&body, "memory_search")
                .into_iter()
                .flat_map(|(text, _)| ctx_ids(&text))
                .nth(i)
                .unwrap_or_else(|| "ctx-none-found".into());
            Reply::Call {
                name: "memory_fetch",
                input: json!({"source": "history", "ids": [found]}),
            }
        }
        r => r,
    };
    reply(n, r)
}

async fn stand_in(summary: Summary, script: impl Fn(usize, usize) -> Reply + Send + Sync + 'static) -> (String, StandIn) {
    let (released, _) = tokio::sync::watch::channel(false);
    let s = StandIn {
        turns: Arc::default(),
        summaries: Arc::default(),
        summary_seen: Arc::default(),
        released: Arc::new(released),
        summary,
        script: Arc::new(script),
        watch: Arc::default(),
        bodies: Arc::default(),
    };
    let app = Router::new().fallback(answer).with_state(s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), s)
}

/// A long reply for conversation round `n`: a few rounds of these pass the
/// low thresholds of [`rollover_policy`].
fn long_round(n: usize) -> String {
    format!("round {n}: {} ", "x".repeat(40_000))
}

/// Long rounds until the first summary request, then `after`; a run that
/// never rolls over ends after 30 rounds (and fails its case).
fn rolling(after: Reply) -> impl Fn(usize, usize) -> Reply + Send + Sync + 'static {
    move |n, summaries| {
        if summaries > 0 {
            after.clone()
        } else if n >= 30 {
            Reply::Text("no rollover".into())
        } else {
            Reply::ToolUse(long_round(n))
        }
    }
}

// ── Running the oneshot ─────────────────────────────────────────────────

/// The parent's live settings: `auto`, with thresholds low enough that a
/// few long rounds pass them.
fn rollover_policy() -> crate::tools::WorkerContextPolicy {
    crate::tools::WorkerContextPolicy {
        context_management: agent_core::config::ContextManagementConfig {
            mode: agent_core::config::ContextManagementMode::Auto,
            pressure_tokens: Some(60_000),
            rollover_tokens: Some(80_000),
            target_tokens: Some(25_000),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// One oneshot's environment: its base (a stored Anthropic login the
/// worker's broker reads), its memory store, the stand-in and the records
/// root a run with a plan uses.
struct Env {
    base: crate::test_env::BaseDirGuard,
    store: fx::Store,
    root: WriteRoot,
    stand: StandIn,
    _url: crate::test_env::EnvVarGuard,
}

async fn env(summary: Summary, script: impl Fn(usize, usize) -> Reply + Send + Sync + 'static) -> Env {
    let base = crate::test_env::BaseDirGuard::new();
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"parity-refresh","access":"parity-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let (url, stand) = stand_in(summary, script).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    Env {
        base,
        store: fx::legacy(),
        root: write_root(),
        stand,
        _url,
    }
}

/// What one oneshot returned and left behind.
struct Run {
    result: String,
    events: Vec<StreamEvent>,
    /// Its handle (`sa_<id>`), its records' name.
    handle: String,
    /// The log file(s) it wrote.
    log: String,
}

impl Run {
    fn events_text(&self) -> String {
        self.events.iter().map(|e| format!("{e:?}\n")).collect()
    }

    fn usage_events(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })))
            .count()
    }

    fn done_events(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Agent(AgentEvent::SubagentDone { result_preview, .. }) => {
                    Some(result_preview.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn updates(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Agent(AgentEvent::SubagentUpdate { status, .. }) => Some(status.clone()),
                _ => None,
            })
            .collect()
    }
}

fn params(timeout: u64) -> Value {
    json!({
        "name": "gate",
        "system_prompt": "You are the oneshot parity worker.",
        "task": "Read /nonexistent/oneshot-parity.txt and report what it says.",
        "timeout": timeout,
    })
}

/// The tool context a parent gives the oneshot: its live settings
/// `policy` and the memory binding the worker runs on.
fn context(
    env: &Env,
    policy: Option<crate::tools::WorkerContextPolicy>,
) -> (ToolContext, tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.channels.tx_events = Some(tx);
    ctx.capabilities.memory_backend = Some(env.store.binding.clone());
    ctx.capabilities.worker_context = policy;
    (ctx, rx)
}

/// Where a run with a plan keeps its records. Its worker runs on the real
/// worker registry.
fn site(env: &Env) -> RecordSite {
    RecordSite::at(env.root.root.clone(), PROCESS_DIR.into())
}

/// The run's record directory as its writes see it, with faults a test
/// injects once the run has one.
type Records = Arc<Mutex<Option<Arc<fx::FaultFs>>>>;

/// [`site`] whose record writes go through [`Records`].
fn faulty_site(env: &Env) -> (RecordSite, Records) {
    let records: Records = Arc::default();
    let wrap: RecordWrap = {
        let records = Arc::clone(&records);
        Arc::new(move |dir| {
            let fs = fx::FaultFs::new(dir);
            *records.lock().unwrap() = Some(Arc::clone(&fs));
            fs as Arc<dyn crate::tools::subagent::record_store::RecordFs>
        })
    };
    let site = RecordSite {
        test_wrap: Some(wrap),
        ..site(env)
    };
    (site, records)
}

fn handle_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Agent(AgentEvent::SubagentStart { subagent_id, .. }) => {
                Some(format!("sa_{subagent_id}"))
            }
            _ => None,
        })
        .expect("a SubagentStart event")
}

fn logs(env: &Env) -> String {
    let dir = env.base.path().join("logs").join("subagents");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    entries.sort();
    let mut log = String::new();
    for path in entries {
        log.push_str(&std::fs::read_to_string(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
    }
    log
}

/// Run the oneshot to its end, with a plan (`planned`) or without.
async fn run(env: &Env, policy: Option<crate::tools::WorkerContextPolicy>, timeout: u64, planned: bool) -> Run {
    let site = match planned {
        true => Some(site(env)),
        false => None,
    };
    run_at(env, policy, timeout, site).await
}

/// Run the oneshot to its end, with a plan at `site` or without one.
async fn run_at(
    env: &Env,
    policy: Option<crate::tools::WorkerContextPolicy>,
    timeout: u64,
    site: Option<RecordSite>,
) -> Run {
    run_in(env, policy, timeout, site, None).await
}

/// [`run_at`] in a host whose child spend is `spend` (a sink: the daemon).
async fn run_in(
    env: &Env,
    policy: Option<crate::tools::WorkerContextPolicy>,
    timeout: u64,
    site: Option<RecordSite>,
    spend: Option<SpendParent>,
) -> Run {
    env.stand.reset();
    let (mut ctx, mut rx) = context(env, policy);
    ctx.capabilities.spend = spend;
    let result = SubagentTool
        .dispatch(params(timeout), ctx, move |_| site)
        .await
        .expect("a oneshot returns its outcome as text");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    Run {
        result,
        events,
        handle,
        log: logs(env),
    }
}

/// The run-specific parts of a run's text, as the gate replaces them.
fn stable(text: &str) -> String {
    let id = regex::Regex::new(r"subagent_id: \d+").unwrap();
    let secs = regex::Regex::new(r"duration_secs: [0-9.]+(e-?\d+)?").unwrap();
    let log_secs = regex::Regex::new(r"Duration: [0-9.]+s").unwrap();
    let stamp = regex::Regex::new(r"\d{8}-\d{6}").unwrap();
    let turn = regex::Regex::new(r"turn-\d+-\d+").unwrap();
    let t = id.replace_all(text, "subagent_id: <id>");
    let t = secs.replace_all(&t, "duration_secs: <secs>");
    let t = log_secs.replace_all(&t, "Duration: <secs>s");
    let t = stamp.replace_all(&t, "<timestamp>");
    turn.replace_all(&t, "turn-<pid>-<n>").into_owned()
}

/// A run's v2 final record, read the way a resume reads it.
fn final_of(env: &Env, handle: &str) -> WorkerRecord {
    final_at(&env.root.root, PROCESS_DIR, handle)
}

/// The v2 final record of `handle` in `process_dir` below `root`.
fn final_at(root: &std::path::Path, process_dir: &str, handle: &str) -> WorkerRecord {
    let location = record_file::location_of(root, process_dir, handle, RecordKind::Final).unwrap();
    match record_file::load(&location).unwrap().final_record {
        Candidate::Present(FinalRecord::V2(record)) => *record,
        Candidate::Present(FinalRecord::V1(_)) => panic!("a v1 archive took the final's name"),
        Candidate::Absent => panic!("no final record"),
        Candidate::Unreadable(e) => panic!("unreadable final: {e}"),
    }
}

fn head_left(env: &Env, handle: &str) -> bool {
    env.root.dir.join(format!("{handle}.head.json")).exists()
}

/// Nothing of a run is on disk.
fn no_records(env: &Env) -> bool {
    std::fs::read_dir(&env.root.dir).map_or(true, |mut entries| entries.next().is_none())
}

/// The planned and the unplanned run of one script agree on everything
/// their caller sees: the result, the events, the log.
fn assert_same(planned: &Run, unplanned: &Run) {
    assert_eq!(stable(&planned.result), stable(&unplanned.result), "result");
    assert_eq!(stable(&planned.events_text()), stable(&unplanned.events_text()), "events");
    assert_eq!(stable(&planned.log), stable(&unplanned.log), "log");
}

/// [`assert_same`] but for the numbers of the one usage event (its four
/// counts and its cache-creation TTL split) and of the log's tokens line,
/// which [`usage_of`] and [`split_of`] read. Everything else, the usage
/// event's model and which TTL fields it states included, must agree.
fn assert_same_but_usage(planned: &Run, unplanned: &Run) {
    let counts = regex::Regex::new(
        r"\b(input_tokens|output_tokens|cache_read_input_tokens|cache_creation_input_tokens): \d+",
    )
    .unwrap();
    let split = regex::Regex::new(r"\b(cache_creation_5m|cache_creation_1h): Some\(\d+\)").unwrap();
    let tokens = regex::Regex::new(r"Tokens: \d+in/\d+out \(\d+cr/\d+cw\)").unwrap();
    let blank = |text: &str| {
        let text = counts.replace_all(text, "$1: <n>");
        let text = split.replace_all(&text, "$1: Some(<n>)");
        tokens
            .replace_all(&text, "Tokens: <n>in/<n>out (<n>cr/<n>cw)")
            .into_owned()
    };
    assert_eq!(stable(&planned.result), stable(&unplanned.result), "result");
    assert_eq!(blank(&stable(&planned.events_text())), blank(&stable(&unplanned.events_text())), "events");
    assert_eq!(blank(&stable(&planned.log)), blank(&stable(&unplanned.log)), "log");
}

/// The one usage event's counts (input, output, cache read, cache
/// creation), and that the log's tokens line says the same.
fn usage_of(run: &Run) -> (u64, u64, u64, u64) {
    let counts: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            }) => Some((
                *input_tokens,
                *output_tokens,
                *cache_read_input_tokens,
                *cache_creation_input_tokens,
            )),
            _ => None,
        })
        .collect();
    let [(i, o, r, w)] = counts.as_slice() else {
        panic!("one usage event: {counts:?}");
    };
    let line = format!("Tokens: {i}in/{o}out ({r}cr/{w}cw)");
    assert!(run.log.contains(&line), "the log says {line}: {}", run.log);
    (*i, *o, *r, *w)
}

/// The measured conversation of `fin`'s ledger on its worker's model,
/// summed here by hand (input, output, cache read, cache creation): what a
/// v2 run's usage event must say (8f).
fn ledger_aggregate(fin: &WorkerRecord) -> (u64, u64, u64, u64) {
    fin.ledger
        .entries()
        .filter(|e| e.purpose == RequestPurpose::Conversation && e.model == fin.meta.model)
        .filter_map(|e| match &e.amount {
            Amount::Measured { usage } => Some(usage),
            _ => None,
        })
        .fold((0, 0, 0, 0), |(i, o, r, w), u| {
            (
                i + u.input_tokens,
                o + u.output_tokens,
                r + u.cache_read_tokens,
                w + u.cache_write_tokens,
            )
        })
}

/// The one usage event's cache-creation TTL split (5 minutes, 1 hour) and
/// its model.
fn split_of(run: &Run) -> (Option<u64>, Option<u64>, Option<String>) {
    let splits: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                cache_creation_5m,
                cache_creation_1h,
                model,
                ..
            }) => Some((*cache_creation_5m, *cache_creation_1h, model.clone())),
            _ => None,
        })
        .collect();
    let [split] = splits.as_slice() else {
        panic!("one usage event: {splits:?}");
    };
    split.clone()
}

/// The bound a oneshot returns its timed-out result within: its timeout
/// and a second, with or without a plan.
fn on_time(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs + 1)
}

/// The window a run sealed at its rollover carries the run's logical ID:
/// the store says so, a resume's lineage check of the run's final admits
/// it, and the same final claimed by another run is refused.
async fn assert_sealed_under_the_run(env: &Env, fin: &WorkerRecord) {
    let [window] = fin.windows.as_slice() else {
        panic!("one sealed window: {:?}", fin.windows);
    };
    let binding = &env.store.binding;
    let support = binding.owned_history_support().await.unwrap().expect("owned history");
    let entries = binding
        .history_inspect(&support, &[window.archive_id.as_str().to_owned()])
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        entries[0].logical_id,
        agent_core::context_archive::logical_digest(&worker_logical_id(&fin.run_id)).unwrap(),
        "sealed under the run's logical ID"
    );
    let grant = fx::grant(fin, binding).await;
    assert!(grant.allows(window.archive_id.as_str()), "a resume may retrieve it");
    let mut claimed = fin.clone();
    claimed.run_id = RunId::random();
    match fx::refusal(&claimed, binding).await {
        Refusal::Foreign(id) => assert_eq!(id, window.archive_id),
        other => panic!("another run's claim is refused as foreign: {other:?}"),
    }
}

fn short(text: &str) -> String {
    let chars = text.chars().count();
    if chars <= 600 {
        return text.to_owned();
    }
    let head: String = text.chars().take(300).collect();
    let tail: String = text.chars().skip(chars - 300).collect();
    format!("{head} … {tail}")
}

/// The lines after a result's output: what [`rollover_trailer`] added.
fn trailer_lines(result: &str) -> Vec<String> {
    let at = result.find("\n\n[rollover").unwrap_or_else(|| panic!("no trailer: {}", short(result)));
    result[at + 2..].lines().map(str::to_owned).collect()
}

// ── Parity without a rollover ───────────────────────────────────────────

/// o1: a run with a plan that uses a tool and answers returns what it
/// returns without one (result, parent events including the one usage
/// event, log), with its spawn head on disk before its first request and
/// its final after.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o1_a_finished_run_with_a_plan_answers_as_one_without() {
    let env = env(Summary::Answer, |n, _| {
        if n == 0 {
            Reply::ToolUse("looking. ".into())
        } else {
            Reply::Text("gate ok".into())
        }
    })
    .await;
    let unplanned = run(&env, Some(rollover_policy()), 30, false).await;
    assert!(no_records(&env));
    env.stand.watch(env.root.dir.clone());
    let planned = run(&env, Some(rollover_policy()), 30, true).await;
    assert_same(&planned, &unplanned);
    // PARITY (upstream #364): the oneshot's loop sets each response off
    // with a blank line (`append_response_text`), and so does the drive.
    assert_eq!(planned.result, "[subagent:gate] looking. \n\ngate ok");
    assert_eq!(planned.usage_events(), 1);
    assert_eq!(env.stand.head_at_first_request(), Some(true), "spawn head before inference");

    let fin = final_of(&env, &planned.handle);
    assert_eq!(
        (fin.status.as_str(), fin.output.as_str()),
        ("completed", "looking. \n\ngate ok")
    );
    assert_eq!(fin.handle_id, planned.handle);
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (1, 0));
    assert_eq!(fin.meta.agent_name, "gate");
    assert_eq!(fin.meta.system_prompt, "You are the oneshot parity worker.");
    assert_eq!(fin.meta.timeout_secs, 30);
    assert!(!head_left(&env, &planned.handle));
}

/// o2: a run with a plan that runs out of time returns the oneshot's
/// timed-out partial result, as without one; its timeout is the run's
/// deadline, and its final says it timed out.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o2_a_timed_out_run_with_a_plan_answers_as_one_without() {
    let env = env(Summary::Answer, |n, _| {
        if n == 0 {
            Reply::ToolUse("looking. ".into())
        } else {
            Reply::Held("partial answer".into())
        }
    })
    .await;
    let started = std::time::Instant::now();
    let unplanned = run(&env, Some(rollover_policy()), 3, false).await;
    let unplanned_took = started.elapsed();
    let started = std::time::Instant::now();
    let planned = run(&env, Some(rollover_policy()), 3, true).await;
    let took = started.elapsed();
    assert!(unplanned_took < on_time(3), "{unplanned_took:?}");
    assert!(took < on_time(3), "returns on time: {took:?}");
    assert_same_but_usage(&planned, &unplanned);
    // f2 (8f): the planned run's usage is its ledger's measured
    // conversation. Its second attempt, cut at the deadline, reported
    // usage as it started but never ended measured: an open liability
    // there, so it adds nothing, as without a plan (before 8f the planned
    // run sent its drive's totals, which count it: (24, 6, 8, 10)).
    assert_eq!(usage_of(&unplanned), (12, 3, 4, 5), "the first attempt");
    assert_eq!(usage_of(&planned), (12, 3, 4, 5), "the measured attempt only");
    // The same model, and the attempt's cache creation in its 5-minute
    // share (the worker's cache TTL), in both.
    let model = Some("anthropic/claude-sonnet-4-6".to_owned());
    assert_eq!(split_of(&unplanned), (Some(5), Some(0), model.clone()), "the first attempt");
    assert_eq!(split_of(&planned), (Some(5), Some(0), model), "the measured attempt only");
    assert!(
        planned.result.starts_with("[subagent:gate] [TIMED OUT after 3s — partial results below]"),
        "{}",
        planned.result
    );
    let fin = final_of(&env, &planned.handle);
    assert_eq!(usage_of(&planned), ledger_aggregate(&fin), "the final's ledger");
    assert!(
        fin.ledger.entries().any(|e| matches!(e.amount, Amount::Liability { .. })),
        "the cut attempt is open there: {:?}",
        fin.ledger
    );
    assert_eq!(fin.status, "timed_out");
    assert_eq!(
        Some(fin.output.as_str()),
        planned.result.strip_prefix("[subagent:gate] "),
        "the final's output is the result"
    );
}

/// o3: a run with a plan that the provider refuses reports the oneshot's
/// error (message and category), as without one, and publishes no usage.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o3_a_failed_run_with_a_plan_answers_as_one_without() {
    let env = env(Summary::Answer, |_, _| Reply::Invalid).await;
    let unplanned = run(&env, Some(rollover_policy()), 30, false).await;
    let planned = run(&env, Some(rollover_policy()), 30, true).await;
    assert_same(&planned, &unplanned);
    assert!(
        planned.result.starts_with("[subagent:gate ERROR] Bad request (HTTP 400 [invalid_request_error])."),
        "{}",
        planned.result
    );
    assert_eq!(planned.usage_events(), 0);
    let fin = final_of(&env, &planned.handle);
    assert!(fin.status.starts_with("failed"), "{}", fin.status);
}

/// o4: a run with a plan that falls back to rollover off (here: the
/// parent's settings are not available) runs as without one, writes no
/// records, and says why after its output.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o4_a_fallback_says_why_after_the_output() {
    let env = env(Summary::Answer, |n, _| {
        if n == 0 {
            Reply::ToolUse("looking. ".into())
        } else {
            Reply::Text("gate ok".into())
        }
    })
    .await;
    let unplanned = run(&env, None, 30, false).await;
    let planned = run(&env, None, 30, true).await;
    assert_eq!(
        planned.result,
        format!(
            "{}\n\n[rollover]: rollover disabled for this worker: the parent's context settings are not available",
            unplanned.result
        )
    );
    assert!(no_records(&env), "nothing of a fallback is on disk");
    assert_eq!(planned.usage_events(), 1);
    assert_eq!(planned.done_events().len(), 1);
}

/// o9 (flipped by 8g): a fresh planned run on the real worker registry,
/// which holds no memory tools, runs with owned-history tools of its own
/// (before 8g it failed closed at stream start: "automatic context
/// management requires builtin memory_search and memory_fetch"). It
/// answers in window 1, and a search there finds nothing: its lineage has
/// sealed no window yet.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o9_a_fresh_planned_run_on_the_worker_registry_has_owned_history() {
    let env = env(Summary::Answer, |n, _| match n {
        0 => search("round"),
        _ => Reply::Text("gate ok".into()),
    })
    .await;
    let planned = run(&env, Some(rollover_policy()), 30, true).await;
    assert!(planned.result.starts_with("[subagent:gate] "), "{}", short(&planned.result));
    assert!(planned.result.contains("gate ok"), "{}", short(&planned.result));
    assert!(!planned.result.contains("ERROR"), "{}", short(&planned.result));
    assert_eq!((env.stand.turns(), env.stand.summaries()), (2, 0));
    let bodies = env.stand.bodies();
    let tools = offered(&bodies[0]);
    assert!(tools.contains("memory_search") && tools.contains("memory_fetch"), "{tools:?}");
    let searched = tool_results(&bodies[1], "memory_search");
    assert_eq!(searched.len(), 1, "{searched:?}");
    assert!(!searched[0].1, "a search, not an error: {}", short(&searched[0].0));
    assert_eq!(ctx_ids(&searched[0].0), Vec::<String>::new(), "nothing sealed yet");
    assert_eq!(planned.usage_events(), 1);
    assert_eq!(planned.done_events().len(), 1);
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (1, 0));
}

// ── With a rollover ─────────────────────────────────────────────────────

/// o5: a run that rolls over keeps the text of its earlier window in its
/// result, then shows its windows and the runtime's notices after the
/// output; it publishes one usage event and one completion, and its final
/// is in its second window.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o5_a_rollover_shows_its_windows_and_notices_after_the_output() {
    let env = env(Summary::Answer, rolling(Reply::Text("gate ok".into()))).await;
    let planned = run(&env, Some(rollover_policy()), 120, true).await;
    assert_eq!(env.stand.summaries(), 1, "one rollover: {}", short(&planned.result));
    assert!(planned.result.starts_with("[subagent:gate] round 0: "), "{}", short(&planned.result));
    assert!(planned.result.contains("gate ok"), "{}", short(&planned.result));
    let lines = trailer_lines(&planned.result);
    assert_eq!(lines[0], "[rollover windows]: window 2; rollovers this run: 1", "{lines:?}");
    assert!(lines.len() > 1, "the rollover's notice: {lines:?}");
    for line in &lines[1..] {
        assert!(line.starts_with("[rollover notice]: "), "{lines:?}");
        assert!(line.len() <= "[rollover notice]: ".len() + crate::runtime::subagent::NOTICE_BYTES);
    }
    let updates = planned.updates();
    // Live at most one a second ((b)): the first notice at once, the
    // latest by the drive's end; any between may be coalesced.
    let notices: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line.strip_prefix("[rollover notice]: ").unwrap())
        .collect();
    for notice in [notices[0], notices[notices.len() - 1]] {
        assert!(updates.iter().any(|u| u == notice), "sent live: {notice}");
    }
    assert_eq!(planned.usage_events(), 1, "published once");
    assert_eq!(planned.done_events().len(), 1);
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (2, 1));
    assert!(!head_left(&env, &planned.handle));
    assert_sealed_under_the_run(&env, &fin).await;
}

/// Upstream's worker-rollover transcripts (`<handle>.window-<N>.md`) under
/// `dir`, at any depth.
fn window_transcripts(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
            if path.is_dir() {
                pending.push(path);
            } else if name.contains(".window-") && name.ends_with(".md") {
                found.push(path);
            }
        }
    }
    found
}

/// PARITY E9 pin (a): a worker under the parent's live context management
/// rolls over once, through ours (the warm-cache summary its own model
/// writes; window 2 of its records), with upstream's worker rollover on at
/// its lowest line (`subagent_context_management = auto`,
/// `subagent_rollover_tokens = 20000`, far below ours' 80k) and rounds
/// short enough for upstream's to cut (~1.3k tokens each; ours needs about
/// 55 of them, so the worker's round limit is raised above its 64).
/// Upstream's `worker_context` never fires for it (stream.rs:
/// `worker_rollover` is `None` while context management is on): no
/// "↻ context window" update, no "Worker context rolled over" notice, no
/// `.window-<N>.md` transcript. Were it to fire, the window would never
/// reach ours' line and the run would end with no summary.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o5b_e9_a_worker_under_ours_rolls_over_once_and_upstreams_rollover_never_fires() {
    let script = |n: usize, summaries: usize| {
        if summaries > 0 {
            Reply::Text("gate ok".into())
        } else if n >= 150 {
            Reply::Text("no rollover".into())
        } else {
            Reply::ToolUse(format!("round {n}: {} ", "x".repeat(3_000)))
        }
    };
    let env = env(Summary::Answer, script).await;
    std::fs::write(
        env.base.path().join("config"),
        "subagent_context_management = auto\nsubagent_rollover_tokens = 20000\n\
         turn_budget.worker.max_provider_rounds = 200\n",
    )
    .unwrap();
    let planned = run(&env, Some(rollover_policy()), 120, true).await;
    assert_eq!(env.stand.summaries(), 1, "one rollover, ours: {}", short(&planned.result));
    assert!(planned.result.contains("gate ok"), "{}", short(&planned.result));
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (2, 1));
    let updates = planned.updates();
    assert!(!updates.iter().any(|u| u.starts_with("↻ context window")), "{updates:?}");
    assert!(!planned.events_text().contains("Worker context rolled over"));
    assert!(!planned.result.contains("Worker context rolled over"), "{}", short(&planned.result));
    let transcripts = window_transcripts(env.base.path());
    assert!(transcripts.is_empty(), "upstream's transcripts: {transcripts:?}");
}

/// Worker successor trial (790d iii, pin d end to end): a worker target
/// that does not fit (60,000 is not below the parent's 60k pressure
/// threshold) is said once per worker, not once per rollover. The worker
/// is resolved once, at its launch: it rolls over three times on the
/// inherited 25k target and its result carries the one `[rollover]` line
/// saying so, never a `[rollover notice]` per window; nothing of it is
/// sent live, and its log has it once (the result's).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o5c_a_worker_target_that_does_not_fit_is_said_once_per_worker_not_per_rollover() {
    let script = |n: usize, summaries: usize| {
        if summaries >= 3 {
            Reply::Text("gate ok".into())
        } else if n >= 60 {
            Reply::Text("no third rollover".into())
        } else {
            Reply::ToolUse(long_round(n))
        }
    };
    let env = env(Summary::Answer, script).await;
    let policy = crate::tools::WorkerContextPolicy {
        worker_target_tokens: Some(60_000),
        ..rollover_policy()
    };
    let planned = run(&env, Some(policy), 240, true).await;
    assert_eq!(
        env.stand.summaries(),
        3,
        "three rollovers: {}",
        short(&planned.result)
    );
    assert!(
        planned.result.contains("gate ok"),
        "{}",
        short(&planned.result)
    );
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (4, 3));
    const KEY: &str = "subagent.context_target_tokens";
    assert_eq!(
        planned.result.matches(KEY).count(),
        1,
        "{}",
        short(&planned.result)
    );
    let lines = trailer_lines(&planned.result);
    let said = lines[0]
        .strip_prefix("[rollover]: ")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(
        said.starts_with(
            "subagent.context_target_tokens (60000) does not fit this worker: target_tokens \
             must be below pressure_tokens (window "
        ) && said.ends_with(" tokens); using the inherited target (25000 tokens)"),
        "{said}"
    );
    assert_eq!(
        lines[1], "[rollover windows]: window 4; rollovers this run: 3",
        "{lines:?}"
    );
    assert!(
        lines[2..].iter().all(|line| !line.contains(KEY)),
        "{lines:?}"
    );
    let updates = planned.updates();
    assert!(updates.iter().all(|u| !u.contains(KEY)), "{updates:?}");
    let events = planned.events_text();
    assert!(
        events.matches(KEY).count() <= 1,
        "at most the completion's preview: {events}"
    );
    assert_eq!(
        planned.log.matches(KEY).count(),
        1,
        "{}",
        short(&planned.log)
    );
}

/// o6: a run whose deadline falls inside its rollover's summary times out
/// at the deadline, well inside the summary's own timeout: the summary
/// waits at most until the run deadline (§5) and is never adopted. Its
/// result is the oneshot's timed-out partial result with the earlier
/// window's text; nothing is sent after the summary; its final says it
/// timed out in its first window.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o6_a_deadline_inside_the_summary_times_out_in_the_oneshot_words() {
    let env = env(Summary::Held, rolling(Reply::Text("too late".into()))).await;
    let started = std::time::Instant::now();
    let planned = run(&env, Some(rollover_policy()), 8, true).await;
    let took = started.elapsed();
    assert_eq!(env.stand.summaries(), 1, "the summary was asked for: {}", short(&planned.result));
    assert!(took < on_time(8), "on time, inside the summary: {took:?}");
    assert!(
        planned.result.starts_with("[subagent:gate] [TIMED OUT after 8s — partial results below]"),
        "{}",
        short(&planned.result)
    );
    assert!(planned.result.contains("[partial response]:\nround 0: "), "{}", short(&planned.result));
    assert!(!planned.result.contains("too late"));
    assert!(!planned.result.contains("[rollover windows]"), "{}", short(&planned.result));
    assert_eq!(planned.usage_events(), 1, "a timed-out oneshot publishes its usage once");
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "timed_out");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (1, 0), "no summary adopted");
    let turns = env.stand.turns();
    env.stand.release();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.stand.turns(), turns, "no request after the deadline");
}

/// o7: a provider error after a rollover reports the oneshot's error, then
/// the windows and notices; no usage is published, one completion is.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o7_a_provider_error_after_a_rollover_answers_in_the_oneshot_words() {
    let env = env(Summary::Answer, rolling(Reply::Invalid)).await;
    let planned = run(&env, Some(rollover_policy()), 120, true).await;
    assert_eq!(env.stand.summaries(), 1, "{}", short(&planned.result));
    assert!(
        planned.result.starts_with(
            "[subagent:gate ERROR] Bad request (HTTP 400 [invalid_request_error]). Provider error details withheld — they can echo request content. [provider_failed code=api_status correlation=turn-"
        ),
        "{}",
        short(&planned.result)
    );
    let lines = trailer_lines(&planned.result);
    assert_eq!(lines[0], "[rollover windows]: window 2; rollovers this run: 1", "{lines:?}");
    assert_eq!(planned.usage_events(), 0);
    let done = planned.done_events();
    assert_eq!(done.len(), 1);
    assert!(done[0].starts_with("ERROR: Bad request"), "{}", done[0]);
    let fin = final_of(&env, &planned.handle);
    assert!(fin.status.starts_with("failed"), "{}", fin.status);
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (2, 1));
    assert_sealed_under_the_run(&env, &fin).await;
}

/// Wait (at most `bound`) until `policy` holds no unreconciled worker.
async fn all_ended_within(policy: &crate::orchestration::OrchestrationRuntime, bound: Duration) -> bool {
    let waited = std::time::Instant::now();
    while !policy.unreconciled_runtime_handles().is_empty() {
        if waited.elapsed() > bound {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// o8: a parent that drops the oneshot while its rollover's summary is
/// out cancels the run: its final says cancelled, in its first window (the
/// summary, answered later, is never adopted), nothing is sent after, the
/// dropped oneshot publishes neither usage nor a completion, and its
/// worker's handle ends once the run has.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o8_a_parent_drop_during_the_summary_cancels_the_run() {
    let env = env(Summary::Held, rolling(Reply::Text("too late".into()))).await;
    let (ctx, mut rx) = context(&env, Some(rollover_policy()));
    let policy = ctx.capabilities.orchestration.clone().expect("the parent's policy");
    let site = site(&env);
    let oneshot = tokio::spawn(async move {
        let tool = SubagentTool;
        tool.dispatch(params(120), ctx, move |_| Some(site)).await
    });
    tokio::time::timeout(Duration::from_secs(60), env.stand.summary_seen.notified())
        .await
        .expect("the run asks for its summary");
    oneshot.abort();
    let _ = oneshot.await;
    let turns = env.stand.turns();

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    let location =
        record_file::location_of(&env.root.root, PROCESS_DIR, &handle, RecordKind::Final).unwrap();
    let waited = std::time::Instant::now();
    while !matches!(record_file::load(&location).unwrap().final_record, Candidate::Present(_)) {
        assert!(waited.elapsed() < Duration::from_secs(40), "the cancelled run publishes its final");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let fin = final_of(&env, &handle);
    assert_eq!(fin.status, "cancelled");
    assert_eq!((fin.window_ordinal, fin.rollovers_this_run), (1, 0));

    env.stand.release();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.stand.turns(), turns, "nothing sent after the drop");
    assert_eq!(final_of(&env, &handle), fin, "no summary adopted late");
    assert!(!head_left(&env, &handle));
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let run = Run {
        result: String::new(),
        events,
        handle,
        log: String::new(),
    };
    assert_eq!(run.usage_events(), 0);
    assert!(run.done_events().is_empty());
    assert!(
        all_ended_within(&policy, Duration::from_secs(15)).await,
        "the dropped oneshot's handle has ended: {:?}",
        policy.unreconciled_runtime_handles()
    );
}

/// o10: once a dropped oneshot's run with a plan has ended, its parent
/// admits new work under a concurrency limit of one: the dropped call kept
/// no slot.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o10_a_parent_admits_new_work_after_a_dropped_run_with_a_plan() {
    let env = env(Summary::Answer, |n, _| match n {
        0 => Reply::Held("waiting".into()),
        _ => Reply::Text("next answer".into()),
    })
    .await;
    let foreground =
        agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
    let policy = Arc::new(crate::orchestration::OrchestrationRuntime::baseline(foreground, 1, 64).unwrap());
    let (mut ctx, mut rx) = context(&env, Some(rollover_policy()));
    ctx.capabilities.orchestration = Some(Arc::clone(&policy));
    let site = site(&env);
    let oneshot = tokio::spawn(async move {
        let tool = SubagentTool;
        tool.dispatch(params(120), ctx, move |_| Some(site)).await
    });
    let waited = std::time::Instant::now();
    while env.stand.turns() == 0 {
        assert!(waited.elapsed() < Duration::from_secs(60), "the run sends its first request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    assert!(policy.is_unreconciled(&handle), "the run holds the one slot");
    oneshot.abort();
    assert!(oneshot.await.unwrap_err().is_cancelled(), "the call was dropped, not finished");
    assert!(
        all_ended_within(&policy, Duration::from_secs(15)).await,
        "the dropped oneshot's handle {handle} has ended: {:?}",
        policy.unreconciled_runtime_handles()
    );

    let (mut ctx, _rx) = context(&env, Some(rollover_policy()));
    ctx.capabilities.orchestration = Some(Arc::clone(&policy));
    let next = SubagentTool
        .dispatch(params(30), ctx, |_| None)
        .await
        .expect("the parent admits the next oneshot under a limit of one");
    assert_eq!(next, "[subagent:gate] next answer");
    assert!(policy.unreconciled_runtime_handles().is_empty());
}

/// o11: a run with a plan whose final record's write is held past its
/// deadline (no head write was in flight at the deadline) still returns its
/// timed-out result on time, with its usage and then its completion, once
/// each. The write goes on: the handle stays pending (the head is the crash
/// checkpoint meanwhile) until the record lands, then ends once, and
/// nothing is published again.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o11_a_held_final_write_does_not_hold_the_timed_out_result() {
    let env = env(Summary::Answer, |n, _| {
        if n == 0 {
            Reply::ToolUse("looking. ".into())
        } else {
            Reply::Held("partial answer".into())
        }
    })
    .await;
    let (site, records) = faulty_site(&env);
    let (ctx, mut rx) = context(&env, Some(rollover_policy()));
    let policy = ctx.capabilities.orchestration.clone().expect("the parent's policy");
    env.stand.reset();
    let (gate, fault) = fx::gate();
    let started = std::time::Instant::now();
    let call = tokio::spawn(async move {
        let tool = SubagentTool;
        tool.dispatch(params(3), ctx, move |_| Some(site)).await
    });
    // The spawn head is durable before the first request: the next rename
    // is the final's.
    while env.stand.turns() == 0 {
        assert!(started.elapsed() < Duration::from_secs(30), "the run sends its first request");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    records
        .lock()
        .unwrap()
        .as_ref()
        .expect("the run's record directory")
        .inject(fx::Op::Rename, "", fault);
    let result = tokio::time::timeout(on_time(3) + Duration::from_secs(2), call)
        .await
        .expect("the oneshot waits for its held final write")
        .unwrap()
        .expect("a oneshot returns its outcome as text");
    let took = started.elapsed();
    assert!(took < on_time(3), "returns on time: {took:?}");
    assert!(
        result.starts_with("[subagent:gate] [TIMED OUT after 3s — partial results below]"),
        "{}",
        short(&result)
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    let published: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage { .. }) => Some("usage"),
            StreamEvent::Agent(AgentEvent::SubagentDone { .. }) => Some("done"),
            _ => None,
        })
        .collect();
    assert_eq!(published, ["usage", "done"], "with the result, in order, once each");

    tokio::time::timeout(Duration::from_secs(10), gate.reached)
        .await
        .expect("the final's rename is the one held")
        .unwrap();
    assert!(policy.is_unreconciled(&handle), "pending while its final may still land");
    assert!(head_left(&env, &handle), "the head is the crash checkpoint meanwhile");
    gate.open.send(()).unwrap();
    assert!(
        all_ended_within(&policy, Duration::from_secs(15)).await,
        "the handle ends once the final has landed: {:?}",
        policy.unreconciled_runtime_handles()
    );
    assert_eq!(super::reap::finished::count(&handle), 1, "ended once, by the worker's lifecycle");
    let fin = final_of(&env, &handle);
    assert_eq!(fin.status, "timed_out");
    assert!(!head_left(&env, &handle));
    let log = records.lock().unwrap().as_ref().unwrap().log();
    let final_renames = log
        .iter()
        .filter(|line| line.starts_with(&format!("rename {handle}.json")))
        .count();
    assert_eq!(final_renames, 1, "the final is published once: {log:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let late: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| {
            matches!(
                e,
                StreamEvent::Session(SessionEvent::Usage { .. })
                    | StreamEvent::Agent(AgentEvent::SubagentDone { .. })
            )
        })
        .map(|e| format!("{e:?}"))
        .collect();
    assert_eq!(late, Vec::<String>::new(), "nothing is published again");
}

/// o12: a parent that drops the oneshot while its run's final record is
/// being written, when no reaper thread can be started then (as under
/// thread exhaustion), leaves the handle pending: the worker's own
/// lifecycle ends it once the write has ended, exactly once, with nothing
/// published for the dropped call, and the parent then admits new work
/// under a concurrency limit of one.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn o12_a_dropped_oneshot_whose_reaper_cannot_start_stays_pending_while_its_final_is_written() {
    let env = env(Summary::Answer, |n, _| match n {
        0 => Reply::ToolUse("looking. ".into()),
        1 => Reply::Held("partial answer".into()),
        _ => Reply::Text("next answer".into()),
    })
    .await;
    let (site, records) = faulty_site(&env);
    let foreground =
        agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
    let policy = Arc::new(crate::orchestration::OrchestrationRuntime::baseline(foreground, 1, 64).unwrap());
    let (mut ctx, mut rx) = context(&env, Some(rollover_policy()));
    ctx.capabilities.orchestration = Some(Arc::clone(&policy));
    env.stand.reset();
    let (gate, fault) = fx::gate();
    let started = std::time::Instant::now();
    let call = tokio::spawn(async move {
        let tool = SubagentTool;
        tool.dispatch(params(3), ctx, move |_| Some(site)).await
    });
    // The spawn head is durable before the first request: the next rename
    // is the final's.
    while env.stand.turns() == 0 {
        assert!(started.elapsed() < Duration::from_secs(30), "the run sends its first request");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    records
        .lock()
        .unwrap()
        .as_ref()
        .expect("the run's record directory")
        .inject(fx::Op::Rename, "", fault);
    tokio::time::timeout(Duration::from_secs(15), gate.reached)
        .await
        .expect("the run's final rename is held")
        .unwrap();
    // The call is dropped while its run writes its final; no reaper thread
    // can be started on this thread now.
    super::reap::spawn_fault::set(true);
    call.abort();
    let dropped = call.await;
    super::reap::spawn_fault::set(false);
    assert!(dropped.unwrap_err().is_cancelled(), "the call was dropped, not finished");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    assert!(policy.is_unreconciled(&handle), "pending at the drop: its final may still land");
    // Past the bound the run waits for its final: still pending.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(policy.is_unreconciled(&handle), "pending while its final is held");
    assert_eq!(super::reap::finished::count(&handle), 0);
    assert!(head_left(&env, &handle), "the head is the crash checkpoint meanwhile");

    gate.open.send(()).unwrap();
    assert!(
        all_ended_within(&policy, Duration::from_secs(15)).await,
        "the handle ends once the final has landed: {:?}",
        policy.unreconciled_runtime_handles()
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(super::reap::finished::count(&handle), 1, "ended once, by the worker's lifecycle");
    assert!(!head_left(&env, &handle));
    let log = records.lock().unwrap().as_ref().unwrap().log();
    let final_renames = log
        .iter()
        .filter(|line| line.starts_with(&format!("rename {handle}.json")))
        .count();
    assert_eq!(final_renames, 1, "the final is published once: {log:?}");
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let run = Run {
        result: String::new(),
        events,
        handle,
        log: String::new(),
    };
    assert_eq!(run.usage_events(), 0, "the dropped call publishes no usage");
    assert!(run.done_events().is_empty(), "nor a completion");

    let (mut ctx, _rx) = context(&env, Some(rollover_policy()));
    ctx.capabilities.orchestration = Some(Arc::clone(&policy));
    let next = SubagentTool
        .dispatch(params(30), ctx, |_| None)
        .await
        .expect("the parent admits the next oneshot under a limit of one");
    assert_eq!(next, "[subagent:gate] next answer");
    assert!(policy.unreconciled_runtime_handles().is_empty());
}

// ── Owned history (8g) ──────────────────────────────────────────────────
//
// A v2 run's worker retrieves its own lineage's windows and nothing else,
// through the real tools on the real worker registry: a start and a resume
// (through S0's sites) as well as the oneshot.

/// A `memory_search` of `query` in the worker's history.
fn search(query: &str) -> Reply {
    Reply::Call {
        name: "memory_search",
        input: json!({"source": "history", "query": query}),
    }
}

/// A `memory_fetch` of `id`.
fn fetch(id: &str) -> Reply {
    Reply::Call {
        name: "memory_fetch",
        input: json!({"source": "history", "ids": [id]}),
    }
}

/// Long rounds until each run's first summary request, then `after(k)` at
/// its `k`-th conversation request since; a run that never rolls over ends
/// after 30 rounds (and fails its case).
fn rolling_with(
    after: impl Fn(usize) -> Reply + Send + Sync + 'static,
) -> impl Fn(usize, usize) -> Reply + Send + Sync + 'static {
    let first = Mutex::new(None::<usize>);
    move |n, summaries| {
        let mut first = first.lock().unwrap();
        if summaries == 0 {
            // A run before its rollover (the stand-in is reset per run).
            *first = None;
            return if n >= 30 {
                Reply::Text("no rollover".into())
            } else {
                Reply::ToolUse(long_round(n))
            };
        }
        let at = *first.get_or_insert(n);
        after(n - at)
    }
}

/// [`rolling_with`] that answers `steps` in order after the rollover, the
/// last one again once they are used up.
fn rolling_then(steps: Vec<Reply>) -> impl Fn(usize, usize) -> Reply + Send + Sync + 'static {
    rolling_with(move |k| steps.get(k).or(steps.last()).cloned().unwrap())
}

/// The results of the run's calls of `name`, read from its last request
/// (the window it ended in).
fn results_of(env: &Env, name: &str) -> Vec<(String, bool)> {
    let bodies = env.stand.bodies();
    tool_results(bodies.last().expect("a request"), name)
}

/// What a fetch outside the worker's own lineage answers.
const NOT_OWN: &str = "that ID is not one of your own earlier context windows";

/// The ctx- IDs of `record`'s own sealed windows.
fn own_windows(record: &WorkerRecord) -> Vec<String> {
    record.windows.iter().map(|w| fx::ctx(&w.archive_id)).collect()
}

/// A reactive worker's thread, to its end.
async fn join(registry: &Mutex<crate::runtime::subagent::SubagentRegistry>, handle: &str) {
    let thread = registry
        .lock()
        .unwrap()
        .get_mut(handle)
        .expect("registered")
        .thread_handle
        .take()
        .expect("the thread handle is attached before dispatch returns");
    let joined = tokio::time::timeout(
        Duration::from_secs(150),
        tokio::task::spawn_blocking(move || thread.join().is_ok()),
    )
    .await
    .expect("the worker ends within its timeout")
    .unwrap();
    assert!(joined, "the worker ends cleanly");
}

type Registry = Arc<Mutex<crate::runtime::subagent::SubagentRegistry>>;

/// A parent's registry of reactive workers.
fn registry() -> Registry {
    Arc::new(Mutex::new(crate::runtime::subagent::SubagentRegistry::new()))
}

/// Where a reactive worker's records go here: the live site below the
/// test's base directory, as in production, where the finalizer also puts
/// a v1 run's archive, so a resume finds either under one root.
fn live_site() -> RecordSite {
    fx::private_dir(&crate::tools::subagent::archive::archive_root());
    RecordSite::live()
}

/// The v2 final record of the reactive run `handle` (at [`live_site`]).
fn live_final_of(handle: &str) -> WorkerRecord {
    final_at(
        &crate::tools::subagent::archive::archive_root(),
        crate::tools::subagent::archive::process_dir_name(),
        handle,
    )
}

/// A `subagent_start` to its end, with a plan at [`live_site`]
/// (`planned`) or without one: its handle.
async fn start_to_end(env: &Env, registry: &Registry, planned: bool) -> String {
    env.stand.reset();
    let (mut ctx, _rx) = context(env, Some(rollover_policy()));
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    let site = planned.then(live_site);
    let reply = crate::tools::subagent::start::SubagentStartTool
        .dispatch(params(120), ctx, move |_| site)
        .await
        .expect("started");
    let body: Value = serde_json::from_str(&reply).unwrap();
    let handle = body["handle_id"].as_str().expect("a handle").to_owned();
    join(registry, &handle).await;
    say_status(registry, &handle);
    handle
}

/// A `subagent_resume` of `prior` to its end, with a plan at
/// [`live_site`], below the archive root as in production: its handle.
async fn resume_to_end(env: &Env, registry: &Registry, prior: &str) -> String {
    env.stand.reset();
    let (mut ctx, _rx) = context(env, Some(rollover_policy()));
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    let binding = env.store.binding.clone();
    let bind = move |_: &crate::config::SynapsConfig, _: Option<&std::path::Path>| binding.clone();
    let site = live_site();
    let root = crate::tools::subagent::archive::archive_root();
    let resume_env = crate::tools::subagent::resume::ResumeEnv {
        root: &root,
        own_process_dir: crate::tools::subagent::archive::process_dir_name(),
        binding: &bind,
        worker_tools: None,
        planned: Some(site),
    };
    let reply = crate::tools::subagent::resume::SubagentResumeTool
        .execute_in(
            json!({"handle_id": prior, "instructions": "Go on reading.", "timeout": 120}),
            ctx,
            &resume_env,
        )
        .await
        .expect("resumed");
    let body: Value = serde_json::from_str(&reply).unwrap();
    let handle = body["handle_id"].as_str().expect("a handle").to_owned();
    join(registry, &handle).await;
    say_status(registry, &handle);
    handle
}

/// The ended run `handle`'s status, with a failure's reason, in the
/// test's captured output (shown when the test fails).
fn say_status(registry: &Registry, handle: &str) {
    let mut registry = registry.lock().unwrap();
    let status = registry
        .get_mut(handle)
        .map(|h| format!("{:?}", h.state.read().unwrap().status));
    eprintln!("{handle}: {status:?}");
}

/// The run whose final is `fin` rolled over once, completed, and in its
/// last window found its own sealed window by search (that ID only) and
/// fetched it.
fn assert_retrieved_its_own_window(env: &Env, fin: &WorkerRecord) {
    assert_eq!(env.stand.summaries(), 1, "one rollover");
    assert_eq!(fin.status, "completed", "{}", short(&fin.output));
    let own = own_windows(fin);
    assert_eq!(own.len(), 1, "{:?}", fin.windows);
    let searched = results_of(env, "memory_search");
    assert_eq!(searched.len(), 1, "{searched:?}");
    assert!(!searched[0].1, "{}", short(&searched[0].0));
    assert_eq!(ctx_ids(&searched[0].0), own, "{}", short(&searched[0].0));
    let fetched = results_of(env, "memory_fetch");
    assert_eq!(fetched.len(), 1, "{fetched:?}");
    assert!(!fetched[0].1, "{}", short(&fetched[0].0));
    assert!(fetched[0].0.contains("oneshot-parity.txt"), "its window: {}", short(&fetched[0].0));
}

/// g1 (8g): a fresh v2 run on the real worker registry rolls over, then
/// retrieves its own sealed window: a search finds it and only it, and a
/// fetch of that ID returns its messages. For a start and a oneshot.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g1_a_fresh_start_retrieves_its_own_sealed_window_and_nothing_else() {
    let env = env(
        Summary::Answer,
        rolling_then(vec![search("round 0"), Reply::FetchFound(0), Reply::Text("gate ok".into())]),
    )
    .await;
    let registry = registry();
    let started = start_to_end(&env, &registry, true).await;
    assert_retrieved_its_own_window(&env, &live_final_of(&started));
    let oneshot = run(&env, Some(rollover_policy()), 120, true).await;
    assert!(oneshot.result.contains("gate ok"), "{}", short(&oneshot.result));
    assert_retrieved_its_own_window(&env, &final_of(&env, &oneshot.handle));
}

/// g2 (8g): a resume that was admitted without a grant (here: a v1 run's
/// handle, its history in memory) gets a fresh owned lineage of its own:
/// once it rolls over it retrieves its new window, and only that.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g2_a_resume_without_a_grant_gets_a_fresh_owned_lineage() {
    let short_prior = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let prior_mode = Arc::clone(&short_prior);
    let rolling = rolling_then(vec![search("round 0"), Reply::FetchFound(0), Reply::Text("gate ok".into())]);
    let env = env(Summary::Answer, move |n, s| {
        if prior_mode.load(Ordering::SeqCst) {
            Reply::Text("prior done".into())
        } else {
            rolling(n, s)
        }
    })
    .await;
    let registry = registry();
    let prior = start_to_end(&env, &registry, false).await;
    assert_eq!(env.stand.turns(), 1, "the v1 prior answered at once");
    short_prior.store(false, Ordering::SeqCst);
    let resumed = resume_to_end(&env, &registry, &prior).await;
    let fin = live_final_of(&resumed);
    assert_retrieved_its_own_window(&env, &fin);
    assert_eq!(fin.inherited_windows, Vec::new(), "a fresh lineage");
}

/// g4 (8g): a worker reaches its own lineage only. The parent's history
/// and an earlier sibling's sealed window are in the same store, and both
/// match its search: its search finds neither, and a fetch of either is
/// refused. For a oneshot and a start.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g4_a_worker_cannot_reach_the_parents_or_a_siblings_history() {
    let foreign: Arc<Mutex<Vec<String>>> = Arc::default();
    let ids = Arc::clone(&foreign);
    let env = env(
        Summary::Answer,
        rolling_with(move |k| {
            let ids = ids.lock().unwrap();
            match k {
                0 => search("round 0"),
                k if k <= ids.len() => fetch(&ids[k - 1]),
                _ => Reply::Text("gate ok".into()),
            }
        }),
    )
    .await;
    // A sibling: an earlier oneshot of the same parent that rolled over.
    let sibling = run(&env, Some(rollover_policy()), 120, true).await;
    let sibling = own_windows(&final_of(&env, &sibling.handle));
    assert_eq!(sibling.len(), 1, "the sibling sealed a window");
    // The parent's own history, matching the same search.
    let parent = fx::seal(&env.store.binding, "parent-session-g4", "round 0: the parent's window").await;
    let parent = fx::ctx(&parent);
    *foreign.lock().unwrap() = vec![sibling[0].clone(), parent.clone()];
    let oneshot = run(&env, Some(rollover_policy()), 120, true).await;
    let assert_confined = |fin: WorkerRecord| {
        assert_eq!(env.stand.summaries(), 1, "one rollover");
        let own = own_windows(&fin);
        assert_eq!(own.len(), 1);
        let searched = results_of(&env, "memory_search");
        assert_eq!(searched.len(), 1, "{searched:?}");
        assert_eq!(ctx_ids(&searched[0].0), own, "its own window only: {}", short(&searched[0].0));
        assert!(!searched[0].0.contains("the parent's window"));
        let fetched = results_of(&env, "memory_fetch");
        assert_eq!(fetched.len(), 2, "{fetched:?}");
        for (text, _) in &fetched {
            assert!(text.contains(NOT_OWN), "refused: {}", short(text));
        }
    };
    assert_confined(final_of(&env, &oneshot.handle));
    let registry = registry();
    let started = start_to_end(&env, &registry, true).await;
    assert_confined(live_final_of(&started));
}

/// g6 (8g): a run without a plan and a fallback to rollover off keep the
/// worker registry as it is: the same tools, no memory tool. Only a v2 run
/// adds its own owned-history tools (and the context checkpoint its
/// automatic context management registers).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g6_a_v1_or_fallback_run_keeps_the_worker_registry() {
    let env = env(Summary::Answer, |_, _| Reply::Text("gate ok".into())).await;
    let first_offer = |env: &Env| offered(&env.stand.bodies()[0]);
    run(&env, Some(rollover_policy()), 30, false).await;
    let v1 = first_offer(&env);
    let fallback = run(&env, None, 30, true).await;
    assert!(fallback.result.contains("[rollover]: rollover disabled"), "{}", short(&fallback.result));
    let off = first_offer(&env);
    run(&env, Some(rollover_policy()), 30, true).await;
    let v2 = first_offer(&env);
    let registry = registry();
    start_to_end(&env, &registry, false).await;
    let start_v1 = first_offer(&env);
    assert_eq!(off, v1, "a fallback keeps the worker registry");
    assert_eq!(start_v1, v1, "a v1 start, too");
    for name in ["memory_search", "memory_fetch"] {
        assert!(!v1.contains(name), "{v1:?}");
        assert!(v2.contains(name), "{v2:?}");
    }
    assert!(v1.is_subset(&v2), "{v1:?} / {v2:?}");
    let added: Vec<&String> = v2.difference(&v1).collect();
    for name in &added {
        assert!(
            ["context_checkpoint", "memory_fetch", "memory_search"].contains(&name.as_str()),
            "only owned history is added: {added:?}"
        );
    }
}

/// A legacy store where the runtime restores a resumed window from on a
/// non-exclusive binding (`continuation::archive_store`: the base
/// directory and the scope of the process's working directory), as for a
/// CLI session. [`fx::legacy`]'s store is elsewhere, so a resumed run on it
/// restores no window: its first rollover names the wrong window and its
/// head is refused.
fn cwd_store(env: &Env) -> fx::Store {
    let base = env.base.path().to_path_buf();
    let cwd = std::env::current_dir().unwrap();
    let scope = agent_core::memory::store::ProjectScope::discover(&cwd).unwrap();
    fx::Store {
        _tmp: tempfile::tempdir().unwrap(),
        binding: crate::memory_backend::MemoryBinding::for_test(
            base.clone(),
            scope,
            &Default::default(),
        ),
        base,
    }
}

/// g8 (8g, Q1): a resumed v2 run keeps the grant it was admitted with and
/// grows it with its own new windows: after it rolls over it finds both
/// its predecessor's window and its own new one, and fetches each. (On
/// [`cwd_store`], where its window is restored from.)
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn g8_a_resumed_v2_run_retrieves_its_new_windows() {
    let mut env = env(
        Summary::Answer,
        rolling_then(vec![
            search("round 0"),
            Reply::FetchFound(0),
            Reply::FetchFound(1),
            Reply::Text("gate ok".into()),
        ]),
    )
    .await;
    env.store = cwd_store(&env);
    let registry = registry();
    let prior = start_to_end(&env, &registry, true).await;
    let inherited = own_windows(&live_final_of(&prior));
    assert_eq!(inherited.len(), 1, "the predecessor sealed a window");
    let resumed = resume_to_end(&env, &registry, &prior).await;
    assert_retrieved_its_lineage(&env, &live_final_of(&resumed), &inherited);
}

/// The resumed run whose final is `fin` rolled over once, completed,
/// continued the lineage whose windows were `inherited`, and in its last
/// window found both windows of its lineage by search and fetched each.
fn assert_retrieved_its_lineage(env: &Env, fin: &WorkerRecord, inherited: &[String]) {
    assert_eq!(env.stand.summaries(), 1, "the resumed run rolled over");
    assert_eq!(fin.status, "completed", "{}", short(&fin.output));
    assert_eq!(
        fin.inherited_windows.iter().map(|w| fx::ctx(&w.archive_id)).collect::<Vec<_>>(),
        inherited,
        "it continues its predecessor's lineage"
    );
    let own = own_windows(fin);
    assert_eq!(own.len(), 1, "{:?}", fin.windows);
    let searched = results_of(env, "memory_search");
    assert_eq!(searched.len(), 1, "{searched:?}");
    let mut found = ctx_ids(&searched[0].0);
    found.sort();
    let mut lineage = vec![inherited[0].clone(), own[0].clone()];
    lineage.sort();
    assert_eq!(found, lineage, "both windows of its lineage: {}", short(&searched[0].0));
    let fetched = results_of(env, "memory_fetch");
    assert_eq!(fetched.len(), 2, "{fetched:?}");
    for (text, error) in &fetched {
        assert!(!error && !text.contains(NOT_OWN), "fetched: {}", short(text));
    }
}

// ── The aggregate (8f) ──────────────────────────────────────────────────

/// A returned result with these counts (a v2 run's run id travels next
/// to it: `aggregate_usage`'s `v2_run`).
fn counted() -> SubagentResult {
    SubagentResult {
        text: "gate ok".into(),
        model: "anthropic/claude-sonnet-4-6".into(),
        input_tokens: 12,
        output_tokens: 3,
        cache_read: 4,
        cache_creation: 5,
        cache_creation_5m: Some(5),
        cache_creation_1h: Some(0),
        tool_count: 1,
        timed_out: false,
    }
}

/// f4 (8f, D2 (i)): the publisher sends a v2 run's aggregate once, however
/// often it is asked; a run without worker rollover sends its own each
/// time, as always; in a host with a sink it sends none (the tap is the
/// publisher).
#[test]
fn f4_the_publisher_sends_a_v2_runs_aggregate_once() {
    let id = RunId::random();
    let run = Some(id.as_str());
    match aggregate_usage(&counted(), run, false) {
        Some(SessionEvent::Usage {
            input_tokens: 12,
            output_tokens: 3,
            cache_read_input_tokens: 4,
            cache_creation_input_tokens: 5,
            cache_creation_5m: Some(5),
            cache_creation_1h: Some(0),
            model: Some(model),
        }) => assert_eq!(model, "anthropic/claude-sonnet-4-6"),
        other => panic!("the run's aggregate: {other:?}"),
    }
    assert!(aggregate_usage(&counted(), run, false).is_none(), "never a second one");
    assert!(aggregate_usage(&counted(), run, false).is_none());
    for _ in 0..2 {
        assert!(aggregate_usage(&counted(), None, false).is_some(), "a v1 run's, each time");
    }
    let sunk = RunId::random();
    assert!(aggregate_usage(&counted(), Some(sunk.as_str()), true).is_none(), "the tap's");
    assert!(aggregate_usage(&counted(), None, true).is_none(), "the tap's");
}

/// What the session's spend receiver holds now.
fn spent(rx: &mut SpendReceiver) -> Vec<SinkMessage> {
    let mut out = Vec::new();
    while let Ok(message) = rx.try_recv() {
        out.push(message);
    }
    out
}

/// f5 (8f, Q4 A): a v2 oneshot in a host with a sink (the daemon) books
/// each request through its tap as it is made, and the tap is its one
/// publisher: its parent gets no aggregate usage event.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn f5_a_v2_oneshot_in_a_host_with_a_sink_sends_no_aggregate() {
    let env = env(Summary::Answer, |n, _| {
        if n == 0 {
            Reply::ToolUse("looking. ".into())
        } else {
            Reply::Text("gate ok".into())
        }
    })
    .await;
    let (sink, mut rx) = SpendSink::new();
    let planned = run_in(
        &env,
        Some(rollover_policy()),
        30,
        Some(site(&env)),
        Some(SpendParent::Session(sink)),
    )
    .await;
    // PARITY (upstream #364): responses set off by a blank line, as in o1.
    assert_eq!(planned.result, "[subagent:gate] looking. \n\ngate ok");
    assert_eq!(planned.usage_events(), 0, "{}", planned.events_text());
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed");
    assert_eq!(ledger_aggregate(&fin), (24, 6, 8, 10), "a v2 run: both requests are in its ledger");
    let messages = spent(&mut rx);
    let spends = messages
        .iter()
        .filter(|m| matches!(m, SinkMessage::Spend { .. }))
        .count();
    assert_eq!(spends, 2, "each request booked once, by the tap: {messages:?}");
}

/// f6 (8f, Q5): a oneshot that rolls over sends its ledger's measured
/// conversation on its worker's model as its usage; its rollover's
/// summary, measured in the same ledger, is not in it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn f6_a_rolled_over_oneshots_usage_is_its_ledgers_conversation() {
    let env = env(Summary::Answer, rolling(Reply::Text("gate ok".into()))).await;
    let planned = run(&env, Some(rollover_policy()), 120, true).await;
    assert_eq!(env.stand.summaries(), 1, "one rollover: {}", short(&planned.result));
    let fin = final_of(&env, &planned.handle);
    assert_eq!(fin.status, "completed", "{}", short(&fin.output));
    let of = |purpose: RequestPurpose| {
        fin.ledger
            .entries()
            .filter(|e| e.purpose == purpose && matches!(e.amount, Amount::Measured { .. }))
            .count()
    };
    assert_eq!(of(RequestPurpose::RolloverSummary), 1, "the summary is measured: {:?}", fin.ledger);
    assert_eq!(of(RequestPurpose::Conversation), env.stand.turns(), "{:?}", fin.ledger);
    assert_eq!(usage_of(&planned), ledger_aggregate(&fin), "the conversation only");
    assert_eq!(planned.usage_events(), 1, "published once");
    // The run's id reached the publisher (it travels next to the result):
    // its aggregate is claimed, so no second one can go out.
    assert!(
        !crate::tools::subagent::ledger::claim_aggregate(fin.run_id.as_str()),
        "the publisher claimed the run's aggregate"
    );
}

// ── Enablement (slice 9) ────────────────────────────────────────────────
//
// The tools' production entries (`execute`), with worker rollover on: each
// tool decides its run's site itself (start and the oneshot: the live one;
// resume: the archive root it resolves its predecessor in), on the
// parent's memory binding, and nothing here gives a run a plan.

/// `params` with `extra`'s keys added.
fn with(mut params: Value, extra: Value) -> Value {
    let map = params.as_object_mut().expect("an object");
    for (key, value) in extra.as_object().expect("an object") {
        map.insert(key.clone(), value.clone());
    }
    params
}

/// The parent's settings with the operator's `subagent.context_rollover =
/// off`.
fn operator_off() -> crate::tools::WorkerContextPolicy {
    crate::tools::WorkerContextPolicy {
        operator_rollover: agent_core::config::ContextRollover::Off,
        ..rollover_policy()
    }
}

/// A `subagent_start` through its production entry, its parent's live
/// settings `policy`, with `extra` parameters: its handle, once its run has
/// ended.
async fn start_live(
    env: &Env,
    registry: &Registry,
    policy: crate::tools::WorkerContextPolicy,
    extra: Value,
) -> String {
    env.stand.reset();
    let (mut ctx, _rx) = context(env, Some(policy));
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    let reply = crate::tools::subagent::start::SubagentStartTool
        .execute(with(params(120), extra), ctx)
        .await
        .expect("started");
    let body: Value = serde_json::from_str(&reply).unwrap();
    let handle = body["handle_id"].as_str().expect("a handle").to_owned();
    join(registry, &handle).await;
    say_status(registry, &handle);
    handle
}

/// A `subagent_resume` of `prior` through its production entry: its
/// handle, once its run has ended.
async fn resume_live(
    env: &Env,
    registry: &Registry,
    prior: &str,
    policy: crate::tools::WorkerContextPolicy,
    extra: Value,
) -> String {
    env.stand.reset();
    let (mut ctx, _rx) = context(env, Some(policy));
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    let call = json!({"handle_id": prior, "instructions": "Go on reading.", "timeout": 120});
    let reply = crate::tools::subagent::resume::SubagentResumeTool
        .execute(with(call, extra), ctx)
        .await
        .expect("resumed");
    let body: Value = serde_json::from_str(&reply).unwrap();
    let handle = body["handle_id"].as_str().expect("a handle").to_owned();
    join(registry, &handle).await;
    say_status(registry, &handle);
    handle
}

/// A oneshot through its production entry, in a host whose child spend is
/// `spend`.
async fn oneshot_live(
    env: &Env,
    policy: crate::tools::WorkerContextPolicy,
    extra: Value,
    spend: Option<SpendParent>,
) -> Run {
    env.stand.reset();
    let (mut ctx, mut rx) = context(env, Some(policy));
    ctx.capabilities.spend = spend;
    let result = SubagentTool
        .execute(with(params(120), extra), ctx)
        .await
        .expect("a oneshot returns its outcome as text");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let handle = handle_of(&events);
    Run {
        result,
        events,
        handle,
        log: logs(env),
    }
}

/// The names of `handle`'s records at the live site.
fn live_records(handle: &str) -> Vec<String> {
    let dir = crate::tools::subagent::archive::process_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|d| d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.retain(|name| name.starts_with(&format!("{handle}.")));
    names.sort();
    names
}

/// The ended reactive run `handle`: its status, its notice and whether it
/// was a v2 run.
fn ended(registry: &Registry, handle: &str) -> (SubagentStatus, Option<String>, bool) {
    let registry = registry.lock().unwrap();
    let state = registry.get(handle).expect("registered").state.read().unwrap();
    (state.status.clone(), state.rollover_notice.clone(), state.v2_records)
}

/// The reactive run `handle` ran as a v1 worker and completed: its record
/// is its v1 archive alone (no head, no v2 final), with `notice` or none.
fn assert_ran_v1(registry: &Registry, handle: &str, notice: Option<&str>) {
    let (status, said, v2) = ended(registry, handle);
    assert_eq!(status, SubagentStatus::Completed, "{handle}");
    assert_eq!(said.as_deref(), notice, "{handle}");
    assert!(!v2, "{handle}: a v1 run");
    assert_eq!(live_records(handle), [format!("{handle}.json")], "{handle}");
    let location = record_file::location_of(
        &crate::tools::subagent::archive::archive_root(),
        crate::tools::subagent::archive::process_dir_name(),
        handle,
        RecordKind::Final,
    )
    .unwrap();
    assert!(
        matches!(record_file::load(&location).unwrap().final_record, Candidate::Present(FinalRecord::V1(_))),
        "{handle}: its record is a v1 archive"
    );
}

/// e1 (slice 9): with worker rollover on, a `subagent_start` through its
/// production entry, under a parent whose context management is auto, is a
/// v2 run at the live site: it rolls over and retrieves its own sealed
/// window, and only that.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e1_a_start_rolls_over_and_retrieves_its_own_window() {
    let env = env(
        Summary::Answer,
        rolling_then(vec![search("round 0"), Reply::FetchFound(0), Reply::Text("gate ok".into())]),
    )
    .await;
    let registry = registry();
    let started = start_live(&env, &registry, rollover_policy(), json!({})).await;
    assert_retrieved_its_own_window(&env, &live_final_of(&started));
}

/// e2 (slice 9): a `subagent_resume` through its production entry (the
/// archive root, the parent's binding: the default legacy store) continues
/// its v2 predecessor's lineage: once it rolls over it finds both windows
/// of its lineage and fetches each. Its window is restored from the store
/// its own binding seals into (slice 9 (e)), not the working directory's.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e2_a_resume_continues_its_lineage_and_retrieves_both_windows() {
    let env = env(
        Summary::Answer,
        rolling_then(vec![
            search("round 0"),
            Reply::FetchFound(0),
            Reply::FetchFound(1),
            Reply::Text("gate ok".into()),
        ]),
    )
    .await;
    let registry = registry();
    let prior = start_live(&env, &registry, rollover_policy(), json!({})).await;
    let inherited = own_windows(&live_final_of(&prior));
    assert_eq!(inherited.len(), 1, "the predecessor sealed a window");
    let resumed = resume_live(&env, &registry, &prior, rollover_policy(), json!({})).await;
    assert_retrieved_its_lineage(&env, &live_final_of(&resumed), &inherited);
}

/// e3 (slice 9, Q4 A): a oneshot through its production entry is a v2 run
/// at the live site that rolls over. Without a sink its parent gets its
/// ledger's aggregate as its one usage event; in a host with a sink (the
/// daemon) the tap books its requests and no usage event is sent.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e3_a_oneshot_rolls_over_and_publishes_its_usage_once_in_either_host() {
    let env = env(Summary::Answer, rolling(Reply::Text("gate ok".into()))).await;
    let plain = oneshot_live(&env, rollover_policy(), json!({}), None).await;
    assert_eq!(env.stand.summaries(), 1, "one rollover: {}", short(&plain.result));
    assert!(plain.result.contains("gate ok"), "{}", short(&plain.result));
    let fin = live_final_of(&plain.handle);
    assert_eq!(fin.status, "completed", "{}", short(&fin.output));
    assert_eq!(plain.usage_events(), 1, "published once");
    assert_eq!(usage_of(&plain), ledger_aggregate(&fin), "its ledger's conversation");

    let (sink, mut rx) = SpendSink::new();
    let sunk = oneshot_live(&env, rollover_policy(), json!({}), Some(SpendParent::Session(sink))).await;
    assert_eq!(env.stand.summaries(), 1, "one rollover: {}", short(&sunk.result));
    let fin = live_final_of(&sunk.handle);
    assert_eq!(fin.status, "completed", "{}", short(&fin.output));
    assert_eq!(sunk.usage_events(), 0, "the tap is the one publisher: {}", sunk.events_text());
    let spends = spent(&mut rx)
        .iter()
        .filter(|m| matches!(m, SinkMessage::Spend { .. }))
        .count();
    assert!(spends >= env.stand.turns(), "each conversation request booked by the tap: {spends}");
}

/// e5 (slice 9): an opt-out runs the worker as before, without a word. The
/// operator's `subagent.context_rollover = off` and the caller's
/// `context_rollover: "off"`, each on a start, a resume and a oneshot, give
/// a v1 run (a reactive run's v1 archive and nothing else; nothing of a
/// oneshot) and no notice.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e5_an_opt_out_runs_the_worker_as_before_without_a_notice() {
    let env = env(Summary::Answer, |_, _| Reply::Text("gate ok".into())).await;
    let registry = registry();
    for (policy, extra) in [
        (operator_off(), json!({})),
        (rollover_policy(), json!({"context_rollover": "off"})),
    ] {
        let started = start_live(&env, &registry, policy.clone(), extra.clone()).await;
        assert_ran_v1(&registry, &started, None);
        let resumed = resume_live(&env, &registry, &started, policy.clone(), extra.clone()).await;
        assert_ran_v1(&registry, &resumed, None);
        let oneshot = oneshot_live(&env, policy, extra, None).await;
        assert_eq!(oneshot.result, "[subagent:gate] gate ok", "no notice");
        assert_eq!(live_records(&oneshot.handle), Vec::<String>::new(), "nothing of a v1 oneshot");
    }
}

/// e5 (slice 9, SC's 8g review): an owned-history tool the operator
/// disabled gives a resume admitted with a grant the fallback a fresh
/// lineage gets: rollover off before anything is written, with the notice;
/// the worker runs as a v1 worker and completes. (Not a v2 run whose stream
/// fails closed at its start for want of `memory_fetch`.)
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e5_a_disabled_history_tool_gives_a_granted_resume_the_fresh_lineages_fallback() {
    let rolling_prior = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mode = Arc::clone(&rolling_prior);
    let rolling = rolling(Reply::Text("prior done".into()));
    let env = env(Summary::Answer, move |n, s| {
        if mode.load(Ordering::SeqCst) {
            rolling(n, s)
        } else {
            Reply::Text("gate ok".into())
        }
    })
    .await;
    let registry = registry();
    let prior = start_live(&env, &registry, rollover_policy(), json!({})).await;
    assert_eq!(
        own_windows(&live_final_of(&prior)).len(),
        1,
        "the predecessor sealed a window, so a resume of it is admitted with a grant"
    );
    rolling_prior.store(false, Ordering::SeqCst);
    std::fs::write(env.base.path().join("config"), "disabled_tools = memory_fetch\n").unwrap();
    let notice = "rollover disabled for this worker: owned history tools could not be installed: \
                  the operator disabled memory_fetch";
    let fresh = start_live(&env, &registry, rollover_policy(), json!({})).await;
    assert_ran_v1(&registry, &fresh, Some(notice));
    let resumed = resume_live(&env, &registry, &prior, rollover_policy(), json!({})).await;
    assert_ran_v1(&registry, &resumed, Some(notice));
    assert_eq!(env.stand.turns(), 1, "the resumed worker answered");
}

// ── The trailer ─────────────────────────────────────────────────────────

/// What a oneshot shows after its output: nothing unless something
/// happened; then the notice, the windows (only after a rollover), the
/// kept notices oldest first, and how many were dropped.
#[test]
fn the_trailer_says_nothing_unless_something_happened() {
    let mut s = SubagentState::new();
    assert_eq!(rollover_trailer(&s), "");
    s.visibility = Some(WorkerVisibility::new(WindowCounters {
        window_ordinal: 1,
        rollovers_this_run: 0,
        inherited_windows: 0,
    }));
    assert_eq!(rollover_trailer(&s), "", "a v2 run that made no rollover");

    s.rollover_notice = Some("rollover thresholds reset to this worker's defaults: why".into());
    let visibility = s.visibility.as_mut().unwrap();
    visibility.windows = WindowCounters {
        window_ordinal: 3,
        rollovers_this_run: 2,
        inherited_windows: 0,
    };
    for n in 0..crate::runtime::subagent::NOTICE_RING + 2 {
        visibility.push_notice(&format!("notice {n}"));
    }
    let mut expected = String::from(
        "\n\n[rollover]: rollover thresholds reset to this worker's defaults: why\n\
         [rollover windows]: window 3; rollovers this run: 2",
    );
    for n in 2..crate::runtime::subagent::NOTICE_RING + 2 {
        expected.push_str(&format!("\n[rollover notice]: notice {n}"));
    }
    expected.push_str("\n[rollover notices dropped]: 2");
    assert_eq!(rollover_trailer(&s), expected);
}
