// Warm rolling-summary tests, included into
// `rolling_summary::transport_tests::warm_tests` (its helpers and mocks):
// the summary request reuses the closing window's recorded conversation
// request. Every endpoint is a loopback mock.
use crate::runtime::warm_summary as ws;
use std::collections::BTreeMap;

/// Marks the warm summary request (the appended host instruction).
const WARM_MARK: &str = "[Host request at a context rollover";
/// The instruction's clause that sets earlier host advisories aside.
const SUPERSEDES: &str = "This host-requested summary is the exception";

/// One request the mock received: its body and its header names and values.
#[derive(Clone)]
struct Seen {
    body: String,
    headers: BTreeMap<String, String>,
}

/// How the mock answers the warm summary request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WarmKind {
    /// A written summary.
    #[default]
    Text,
    /// Text and a client tool call.
    ToolCall,
    /// A normal end with no text.
    Empty,
    /// Cut at the output limit.
    Length,
    /// A refusal.
    Refusal,
    /// A written summary whose usage lacks the cache-read count.
    PartialUsage,
    /// A written summary with no usage on its final `message_delta`.
    NoFinalUsage,
    /// A written summary, sent after five seconds.
    Stall,
    /// A summary whose only section is for a later window.
    FutureOnly,
    /// Partial text, then a refusal (a classifier stop mid-reply).
    RefusalAfterText,
    /// A refusal, sent after three seconds.
    SlowRefusal,
    /// Partial text, then an in-stream `error` frame of this type.
    SseError(&'static str),
    /// Partial text, a `refusal` stop reason, then an in-stream `error`
    /// frame of this type (the error takes precedence).
    RefusalThenError(&'static str),
    /// Partial text, then the connection breaks before any stop reason.
    BrokenBody,
    /// Partial text, then five seconds with no body bytes.
    IdleStall,
    /// A stream with no content and no stop reason.
    NoStop,
}

#[derive(Clone, Default)]
struct WarmMock {
    requests: Arc<Mutex<Vec<Seen>>>,
    kind: Arc<Mutex<WarmKind>>,
    /// The `message_start` usage of conversation turns (default: a 40k
    /// cache write).
    turn_usage: Arc<Mutex<Option<Value>>>,
    /// When set, a summary request (warm or cold) is held once its whole
    /// body has arrived, until released.
    hold: Arc<Mutex<Option<Hold>>>,
    /// When set, a warm summary is answered with this redirect status and
    /// `Location`.
    redirect: Arc<Mutex<Option<(u16, String)>>>,
    /// When set, every summary request (warm or cold) is answered with this
    /// status and JSON body.
    fail: Arc<Mutex<Option<(u16, String)>>>,
    /// The cold summary's text (default: a window-1 section).
    cold_text: Arc<Mutex<Option<String>>>,
    /// The cold summary's stop reason (default: `end_turn`).
    cold_stop: Arc<Mutex<Option<&'static str>>>,
}

/// A summary reply held at the mock: `received` is notified once the whole
/// request body has arrived, and the reply waits for `release`.
#[derive(Clone, Default)]
struct Hold {
    received: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl WarmMock {
    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|s| serde_json::from_str(&s.body).unwrap())
            .collect()
    }

    fn warm_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.body.contains(WARM_MARK))
            .count()
    }

    /// Non-streaming requests: the screened (cold) summary path.
    fn cold_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|s| !s.body.contains("\"stream\":true"))
            .count()
    }
}

fn sse_of(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

/// A streamed reply with `text`, the start usage and the final output count.
fn streamed(text: &str, start_usage: Value, output: u64) -> String {
    streamed_as(text, false, "end_turn", start_usage, output)
}

/// A streamed reply with `text` (no text block when empty), optionally a
/// client tool call after it, and the given stop reason.
fn streamed_as(text: &str, tool: bool, stop: &str, start_usage: Value, output: u64) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":start_usage}}),
    ];
    let mut index = 0;
    if !text.is_empty() {
        events.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
        index += 1;
    }
    if tool {
        events.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":"toolu_warm_call","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
    }
    events.extend([
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":output}}),
        json!({"type":"message_stop"}),
    ]);
    sse_of(&events)
}

/// The mock's warm reply for `kind`: 45k cache reads, 60 uncached, 250 out.
fn warm_reply(kind: WarmKind) -> String {
    let usage = json!({"input_tokens":60,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":45_000});
    match kind {
        WarmKind::Text => streamed("## Window 1\nWARM-SUMMARY", usage, 250),
        WarmKind::ToolCall => {
            streamed_as("## Window 1\nWARM-WITH-TOOL", true, "tool_use", usage, 250)
        }
        WarmKind::Empty => streamed_as("", false, "end_turn", usage, 250),
        WarmKind::Length => streamed_as("## Window 1\nWARM-CUT", false, "max_tokens", usage, 250),
        WarmKind::Refusal | WarmKind::SlowRefusal => {
            streamed_as("", false, "refusal", usage, 250)
        }
        WarmKind::RefusalAfterText => {
            streamed_as("## Window 1\nWARM-PARTIAL-REFUSED", false, "refusal", usage, 250)
        }
        WarmKind::SseError(kind) => {
            let mut events = partial_reply();
            events.push(json!({"type":"error","error":{"type":kind,"message":"ECHOED-PROVIDER-TEXT"}}));
            sse_of(&events)
        }
        WarmKind::RefusalThenError(kind) => {
            let mut events = partial_reply();
            events.push(json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null},"usage":{"output_tokens":250}}));
            events.push(json!({"type":"error","error":{"type":kind,"message":"ECHOED-PROVIDER-TEXT"}}));
            sse_of(&events)
        }
        WarmKind::BrokenBody | WarmKind::IdleStall => sse_of(&partial_reply()),
        WarmKind::NoStop => sse_of(&[
            json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
            json!({"type":"message_stop"}),
        ]),
        WarmKind::PartialUsage => streamed(
            "## Window 1\nWARM-PARTIAL",
            json!({"input_tokens":60,"output_tokens":0,"cache_creation_input_tokens":0}),
            250,
        ),
        WarmKind::Stall => streamed("## Window 1\nWARM-LATE", usage, 250),
        WarmKind::FutureOnly => streamed("## Window 2\nWARM-FUTURE-ONLY", usage, 250),
        WarmKind::NoFinalUsage => sse_of(&[
            json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"## Window 1\nWARM-NO-FINAL"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
            json!({"type":"message_stop"}),
        ]),
    }
}

/// The start of a warm reply: `message_start` with its usage and the
/// first text, no stop reason yet.
fn partial_reply() -> Vec<Value> {
    let usage = json!({"input_tokens":60,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":45_000});
    vec![
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":usage}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"## Window 1\nWARM-BROKEN"}}),
    ]
}

/// A streamed body that sends `head`, then either breaks the connection
/// (`BrokenBody`) or goes quiet for five seconds and ends (`IdleStall`).
fn interrupted_body(kind: WarmKind, head: String) -> axum::body::Body {
    let first = futures::stream::once(async move {
        Ok::<_, std::io::Error>(bytes::Bytes::from(head))
    });
    let rest = futures::stream::once(async move {
        if kind == WarmKind::IdleStall {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(bytes::Bytes::new())
        } else {
            // After the head and the first chunk are on the wire (a 200),
            // not before.
            tokio::time::sleep(Duration::from_millis(300)).await;
            Err(std::io::Error::other("connection broken"))
        }
    });
    axum::body::Body::from_stream(first.chain(rest))
}

/// Conversation turns write the cache; the warm summary reads it; a
/// non-streaming (cold) summary gets a plain message.
async fn spawn_warm() -> (WarmMock, crate::test_env::EnvVarGuard) {
    let mock = WarmMock::default();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(m): State<WarmMock>, headers: axum::http::HeaderMap, body: String| async move {
                    let headers = headers
                        .iter()
                        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                        .collect();
                    let warm = body.contains(WARM_MARK);
                    let stream = body.contains("\"stream\":true");
                    m.requests.lock().unwrap().push(Seen { body, headers });
                    let hold = m.hold.lock().unwrap().clone();
                    if let Some(hold) = hold.filter(|_| warm || !stream) {
                        hold.received.notify_one();
                        hold.release.notified().await;
                    }
                    let fail = m.fail.lock().unwrap().clone();
                    if let Some((status, body)) = fail.filter(|_| warm || !stream) {
                        return (
                            StatusCode::from_u16(status).unwrap(),
                            [("content-type", "application/json")],
                            body,
                        )
                            .into_response();
                    }
                    if !stream {
                        let text = m
                            .cold_text
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| "## Window 1\nCOLD-SUMMARY".into());
                        let stop = m.cold_stop.lock().unwrap().unwrap_or("end_turn");
                        let reply = json!({"id":"s","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":text}],"stop_reason":stop,"usage":{"input_tokens":30_000,"output_tokens":300}});
                        return (StatusCode::OK, [("content-type", "application/json")], reply.to_string())
                            .into_response();
                    }
                    let redirect = m.redirect.lock().unwrap().clone();
                    if let Some((status, location)) = redirect.filter(|_| warm) {
                        return (
                            StatusCode::from_u16(status).unwrap(),
                            [("location", location)],
                            String::new(),
                        )
                            .into_response();
                    }
                    let kind = *m.kind.lock().unwrap();
                    if warm && kind == WarmKind::Stall {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    if warm && kind == WarmKind::SlowRefusal {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    if warm && matches!(kind, WarmKind::BrokenBody | WarmKind::IdleStall) {
                        return (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            interrupted_body(kind, warm_reply(kind)),
                        )
                            .into_response();
                    }
                    let reply = if warm {
                        warm_reply(kind)
                    } else {
                        let usage = m.turn_usage.lock().unwrap().clone().unwrap_or_else(|| {
                            json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":40_000,"cache_read_input_tokens":0})
                        });
                        streamed("ok", usage, 5)
                    };
                    (StatusCode::OK, [("content-type", "text/event-stream")], reply).into_response()
                },
            ),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let guard =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    (mock, guard)
}

fn set_thresholds(rt: &crate::Runtime, pressure: u64, rollover: u64) {
    let mut s = rt.continuation.lock().unwrap();
    s.config.pressure_tokens = Some(pressure);
    s.config.rollover_tokens = Some(rollover);
}

/// Turn 1 stays below the thresholds (its request is recorded); turn 2
/// re-sends that history plus one message with the thresholds lowered, so
/// the rollover drops only messages the recorded request carried.
async fn two_turns(rt: &crate::Runtime) -> (Turn, Turn) {
    two_turns_from(rt, None).await
}

/// [`two_turns`] from a history that may carry a previous summary.
async fn two_turns_from(rt: &crate::Runtime, previous: Option<(&str, u64)>) -> (Turn, Turn) {
    two_turns_over(rt, history(previous)).await
}

/// [`two_turns`] with turn 1 sending `start`.
async fn two_turns_over(rt: &crate::Runtime, start: Vec<SharedMessage>) -> (Turn, Turn) {
    set_thresholds(rt, 300_000, 400_000);
    let first = turn(rt, start, Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(first.heads.is_empty(), "no rollover in turn 1");
    set_thresholds(rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(
        json!({"role":"user","content":"carry on in the next window"}),
    ));
    let second = turn(rt, next, Default::default()).await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(!second.heads.is_empty(), "turn 2 rolls over");
    (first, second)
}

/// The cache-input projection of an Anthropic request: every field except
/// the output limit.
fn cache_input(body: &Value) -> Value {
    let mut body = body.clone();
    body.as_object_mut().unwrap().remove("max_tokens");
    body
}

/// The word count the summary instruction asks for ("under about N words").
fn instruction_words(instruction: &str) -> u64 {
    let (_, rest) = instruction.split_once("under about ").expect("a word count");
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

/// On an adaptive model the warm limit leaves room for thinking by the
/// effort the recorded request carries: twice the cap below high effort,
/// the cap plus 16,000 at high (and with no effort on the wire, the model's
/// default), the cap plus 32,000 at xhigh; and the instruction asks for
/// brief reasoning. (A live xhigh run spent twice the cap on thinking alone
/// and wrote no text.)
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_warm_limit_leaves_room_for_thinking_at_the_recorded_effort() {
    use agent_core::reasoning::ReasoningLevel;
    // Low first: its limit is twice the cap, which fixes the cap.
    let mut cap = None;
    for (level, effort) in [
        (ReasoningLevel::Low, Some("low")),
        (ReasoningLevel::Medium, Some("medium")),
        (ReasoningLevel::High, Some("high")),
        (ReasoningLevel::XHigh, Some("xhigh")),
        (ReasoningLevel::Adaptive, None),
    ] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let mut rt = runtime(Some(SUMMARY));
        rt.set_model("claude-opus-5-5".to_string());
        rt.set_reasoning_level_explicit(level);
        let (_first, second) = two_turns(&rt).await;
        assert!(
            summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")),
            "{level:?}"
        );
        let bodies = mock.bodies();
        let recorded = &bodies[0];
        let warm = bodies
            .iter()
            .find(|b| b.to_string().contains(WARM_MARK))
            .expect("a warm request");
        assert_eq!(recorded["thinking"]["type"], "adaptive", "{level:?}");
        assert_eq!(warm["thinking"], recorded["thinking"], "{level:?}");
        assert_eq!(
            warm["output_config"]["effort"].as_str(),
            effort,
            "{level:?}: the recorded effort, unchanged"
        );
        assert_eq!(warm["output_config"], recorded["output_config"], "{level:?}");
        let limit = warm["max_tokens"].as_u64().unwrap();
        let real = recorded["max_tokens"].as_u64().unwrap();
        let cap = *cap.get_or_insert(limit / 2);
        assert!(cap > 0 && cap <= SUMMARY, "cap {cap}");
        let wanted = match effort {
            Some("low" | "medium") => 2 * cap,
            Some("xhigh") => cap + 32_000,
            _ => cap + 16_000,
        };
        assert_eq!(limit, wanted.min(real), "{level:?}");
        let instruction = warm["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(instruction_words(&instruction), cap * 3 / 5, "{level:?}");
        assert!(
            instruction.contains("Keep your reasoning brief"),
            "{level:?}: {instruction}"
        );
    }
}

/// T1: the warm summary request is the closing window's last conversation
/// request (system, tools, thinking, effort, cache markers and messages,
/// plus the beta and version headers and the same credential), with only the
/// output limit lowered and one unmarked user instruction appended. It is
/// the only summary request, and its usage is booked at cache-read rates.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_warm_summary_reuses_the_recorded_request() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, second) = two_turns(&rt).await;

    assert!(
        summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")),
        "{:?}",
        summary_of(&second.heads[0])
    );
    assert_eq!(mock.warm_requests(), 1, "one warm request");
    assert_eq!(mock.cold_requests(), 0, "no cold request");

    let seen = mock.requests.lock().unwrap().clone();
    let bodies = mock.bodies();
    let recorded = &bodies[0];
    let warm_at = seen
        .iter()
        .position(|s| s.body.contains(WARM_MARK))
        .unwrap();
    assert_eq!(warm_at, 1, "sent before the successor's first request");
    let warm = &bodies[warm_at];

    // The recorded request ends with a user message, so the wire carries
    // the instruction as one trailing text block of that message, after the
    // marked block; everything else is unchanged.
    let mut warm_input = cache_input(warm);
    let last = warm_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    assert_eq!(last["role"], "user");
    let appended = last["content"].as_array_mut().unwrap().pop().unwrap();
    assert_eq!(appended["type"], "text");
    let instruction = appended["text"].as_str().unwrap().to_owned();
    assert!(instruction.starts_with(WARM_MARK));
    assert!(
        instruction.contains(
            "Keep your reasoning brief and write the whole summary as plain text in your reply"
        ),
        "{instruction}"
    );
    assert!(
        instruction.contains(SUPERSEDES),
        "every warm instruction carries the clause: {instruction}"
    );
    assert!(
        appended.get("cache_control").is_none(),
        "the instruction carries no cache marker"
    );
    assert!(
        last["content"].as_array().unwrap().last().unwrap()["cache_control"].is_object(),
        "the recorded marker stays on its block"
    );
    assert_eq!(warm_input, cache_input(recorded), "identical cache input");
    let (limit, real) = (
        warm["max_tokens"].as_u64().unwrap(),
        recorded["max_tokens"].as_u64().unwrap(),
    );
    // Budgeted thinking: L = budget + cap, the cap the instruction states
    // (about 3/5 of a word per token).
    let budget = recorded["thinking"]["budget_tokens"].as_u64().unwrap();
    assert_eq!(warm["thinking"], recorded["thinking"]);
    let cap = limit - budget;
    assert!(cap > 0 && cap <= SUMMARY, "cap {cap}");
    assert_eq!(instruction_words(&instruction), cap * 3 / 5);
    assert!(limit <= real, "limit {limit} within {real}");
    // Every header but the body length is the same (version, beta,
    // content type and the credential).
    let headers = |s: &Seen| {
        let mut h = s.headers.clone();
        h.remove("content-length");
        h
    };
    assert_eq!(
        headers(&seen[warm_at]),
        headers(&seen[0]),
        "headers unchanged"
    );
    assert!(seen[0].headers.contains_key("anthropic-version"));

    // Booked once, as reported: 45k cache reads, 60 uncached, 250 out.
    let booked: Vec<_> = second
        .usages
        .iter()
        .filter(|(i, o, _)| *i == 60 && *o == 250)
        .collect();
    assert_eq!(booked.len(), 1, "{:?}", second.usages);
    let read_priced = agent_core::pricing::calculate_cost_optional_split(
        "claude-sonnet-4-6",
        60,
        250,
        45_000,
        0,
        Some(0),
        Some(0),
    );
    let full_priced = agent_core::pricing::calculate_cost_optional_split(
        "claude-sonnet-4-6",
        45_060,
        250,
        0,
        0,
        Some(0),
        Some(0),
    );
    assert!(
        read_priced < full_priced / 3.0,
        "{read_priced} vs {full_priced}"
    );
}

/// The tool names a request body offers, in order.
fn offered(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A session that withholds tools the host registry holds (context
/// management on, a legacy memory backend: no forum) records the projected
/// schema it sent, and its warm summary replays exactly those bytes: the
/// recorded and warm requests both leave out forum_post/read/forget and
/// offer context_checkpoint, with identical tools and cache input, one warm
/// request and no cold one. A sibling session of the same host (context
/// management off) taking a turn before the rollover changes neither the
/// shared registry's catalog generation nor the warm outcome.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_withholding_session_replays_its_projected_schema_warm() {
    const FORUM: [&str; 3] = ["forum_post", "forum_read", "forum_forget"];
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    assert!(
        !rt.memory_backend_for_test().is_axel(),
        "fixture: a legacy memory backend"
    );
    assert_eq!(
        rt.continuation.lock().unwrap().config.mode,
        agent_core::config::ContextManagementMode::Auto,
        "fixture: context management on"
    );
    let mut sibling = runtime(Some(SUMMARY));
    sibling.tools = Arc::clone(&rt.tools);
    sibling.hook_bus = Arc::clone(&rt.hook_bus);
    sibling.continuation.lock().unwrap().config.mode =
        agent_core::config::ContextManagementMode::Off;

    // Turn 1 is recorded.
    set_thresholds(&rt, 300_000, 400_000);
    let first = turn(&rt, history(None), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(first.heads.is_empty(), "no rollover in turn 1");
    let (record_tools, registry_names) = {
        let record = rt
            .continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded");
        let names = |schema: &[Value]| -> Vec<String> {
            schema
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        };
        (
            names(&record.inputs.tools_schema),
            names(&record.inputs.registry_schema),
        )
    };
    {
        let registry = rt.tools.read().await;
        for name in FORUM {
            assert!(registry.get(name).is_some(), "the host registry holds {name}");
        }
    }
    for name in FORUM {
        assert!(registry_names.iter().any(|n| n == name), "registry schema has {name}");
        assert!(!record_tools.iter().any(|n| n == name), "record withholds {name}");
    }
    let projected: Vec<String> = registry_names
        .iter()
        .filter(|n| !FORUM.contains(&n.as_str()))
        .cloned()
        .collect();
    assert_eq!(record_tools, projected, "exactly the forum entries are left out");

    // The sibling's turn: its own withheld set, the shared registry untouched.
    let generation = rt.tools.read().await.catalog().generation();
    let other = turn(&sibling, history(None), Default::default()).await;
    assert!(other.errors.is_empty(), "{:?}", other.errors);
    assert_eq!(
        rt.tools.read().await.catalog().generation(),
        generation,
        "the sibling's turn changed the shared catalog generation"
    );

    // Turn 2 rolls over; the summary goes warm.
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on in the next window"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(!second.heads.is_empty(), "turn 2 rolls over");
    assert!(
        summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")),
        "{:?}",
        summary_of(&second.heads[0])
    );
    assert_eq!(mock.warm_requests(), 1, "one warm request");
    assert_eq!(mock.cold_requests(), 0, "no cold request");

    let bodies = mock.bodies();
    let recorded = &bodies[0];
    let sibling_body = &bodies[1];
    let warm = bodies
        .iter()
        .find(|b| b.to_string().contains(WARM_MARK))
        .expect("a warm request");
    assert_eq!(offered(recorded), record_tools, "the record is what was sent");
    assert!(offered(recorded).iter().any(|n| n == "context_checkpoint"));
    assert!(
        !offered(sibling_body).iter().any(|n| n == "context_checkpoint"),
        "the sibling withholds context_checkpoint"
    );
    assert_eq!(warm["tools"], recorded["tools"], "identical tool bytes");
    let mut warm_input = cache_input(warm);
    let last = warm_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    last["content"].as_array_mut().unwrap().pop();
    assert_eq!(warm_input, cache_input(recorded), "identical cache input");
}

/// A record never holds credential bytes: the authorization is kept only as
/// an opaque scope, so a dump of every field of a record built from an
/// authenticated request does not contain the key.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_record_holds_no_credential_bytes() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let secret = "sk-ant-api03-SYNTHETIC-RECORD-SECRET-0001";
    let rt = runtime(Some(SUMMARY));
    *rt.auth.write().await = super::super::super::types::AuthState {
        auth_token: secret.to_owned(),
        auth_type: "api_key".to_owned(),
        refresh_token: None,
        token_expires: None,
        auth_file_mtime: None,
        bound_credential: None,
    };
    set_thresholds(&rt, 300_000, 400_000);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let sent_with_key = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.headers.values().any(|v| v.contains(secret)));
    assert!(sent_with_key, "the request was authenticated with the key");
    let record = rt
        .continuation
        .lock()
        .unwrap()
        .warm_record()
        .expect("recorded");
    assert!(record.tap.evidence().credential.is_some());
    let dump = record.dump();
    assert!(
        !dump.contains("SYNTHETIC-RECORD-SECRET"),
        "no credential bytes"
    );
    assert!(!format!("{record:?}").contains("SYNTHETIC-RECORD-SECRET"));
}

/// A model switch drops the record and the history it holds.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_model_switch_drops_the_record() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    set_thresholds(&rt, 300_000, 400_000);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let weak = Arc::downgrade(
        &rt.continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded"),
    );
    rt.set_model("claude-sonnet-4-6".to_string());
    assert!(weak.upgrade().is_some(), "the same model keeps it");
    rt.set_model("claude-opus-4-7".to_string());
    assert!(rt.continuation.lock().unwrap().warm_record().is_none());
    assert!(
        weak.upgrade().is_none(),
        "the record and its history are gone"
    );
}

/// A reset (`/compact`, restore) and a rollover each drop the record and
/// the history it holds.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_reset_or_a_rollover_drops_the_record() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let recorded = |rt: &crate::Runtime| {
        Arc::downgrade(
            &rt.continuation
                .lock()
                .unwrap()
                .warm_record()
                .expect("recorded"),
        )
    };

    set_thresholds(&rt, 300_000, 400_000);
    let first = turn(&rt, history(None), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let weak = recorded(&rt);
    let session = rt.continuation.lock().unwrap().logical_id.clone();
    rt.reset_context_continuation(&session, &first.history);
    assert!(rt.continuation.lock().unwrap().warm_record().is_none());
    assert!(weak.upgrade().is_none(), "a reset drops the record");

    let again = turn(&rt, first.history.clone(), Default::default()).await;
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    let weak = recorded(&rt);
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = again.history.clone();
    next.push(msg(
        json!({"role":"user","content":"carry on in the next window"}),
    ));
    let rolled = turn(&rt, next, Default::default()).await;
    assert!(!rolled.heads.is_empty(), "this turn rolls over");
    assert!(
        weak.upgrade().is_none(),
        "the closed window's record is gone"
    );
}

/// The transport refuses a warm request whose rebuilt prefix differs from
/// the recorded one: nothing is sent, and the rollover then uses the cold
/// (screened) path once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_changed_prefix_is_not_sent() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    set_thresholds(&rt, 300_000, 400_000);
    let out = turn(&rt, history(None), Default::default()).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let record = rt
        .continuation
        .lock()
        .unwrap()
        .warm_record()
        .expect("recorded");
    let evidence = record.tap.evidence();
    let before = mock.requests.lock().unwrap().len();
    let reply = rt
        .warm_summary_call(
            Arc::clone(&record),
            super::super::super::warm_summary::WarmSend {
                instruction: format!("{WARM_MARK}] test"),
                limit: 8_000,
                fingerprint: {
                    let mut changed = evidence.fingerprint.unwrap();
                    changed[31] ^= 1;
                    changed
                },
                credential: evidence.credential.unwrap(),
                endpoint: evidence.endpoint.unwrap(),
                correlation: 1,
                published: Default::default(),
                run_deadline: None,
            },
            Default::default(),
        )
        .await;
    assert_eq!(
        reply.terminal,
        super::super::super::warm_summary::WarmTerminal::NotSent(
            super::super::super::warm_summary::WarmMiss::PrefixChanged
        )
    );
    assert!(!reply.dispatched && reply.usage.is_none());
    assert_eq!(mock.requests.lock().unwrap().len(), before, "nothing sent");

    // A changed credential or endpoint is refused the same way.
    for (credential, endpoint) in [
        (
            evidence.credential.unwrap().wrapping_add(1),
            evidence.endpoint.unwrap(),
        ),
        (
            evidence.credential.unwrap(),
            evidence.endpoint.unwrap().wrapping_add(1),
        ),
    ] {
        let reply = rt
            .warm_summary_call(
                Arc::clone(&record),
                super::super::super::warm_summary::WarmSend {
                    instruction: format!("{WARM_MARK}] test"),
                    limit: 8_000,
                    fingerprint: evidence.fingerprint.unwrap(),
                    credential,
                    endpoint,
                    correlation: 1,
                    published: Default::default(),
                    run_deadline: None,
                },
                Default::default(),
            )
            .await;
        assert!(
            matches!(
                reply.terminal,
                super::super::super::warm_summary::WarmTerminal::NotSent(_)
            ),
            "{:?}",
            reply.terminal
        );
    }
    assert_eq!(mock.requests.lock().unwrap().len(), before, "nothing sent");
}

/// Without cache evidence (the recorded reply reported no cache read or
/// write) the summary goes cold: one screened request, no warm request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn without_cache_evidence_the_summary_goes_cold() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    set_thresholds(&rt, 300_000, 400_000);
    let first = turn(&rt, history(None), Default::default()).await;
    // Overwrite the evidence as if the reply had reported no cache usage.
    rt.continuation
        .lock()
        .unwrap()
        .warm_record()
        .expect("recorded")
        .tap
        .completed(Some(ws::WarmUsage {
            input: 40_100,
            output: 5,
            ..ws::WarmUsage::default()
        }));
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")));
    assert_eq!(mock.warm_requests(), 0);
    assert_eq!(mock.cold_requests(), 1);
}

/// The reply contract: a warm reply that calls a tool, has no text, is cut
/// at the limit or folds to nothing is a failed summary. The previous
/// summary is carried forward, the reported usage is booked once, no cold
/// request follows a dispatched warm request (only a refusal gets one
/// screened attempt; see `a_warm_refusal_gets_one_screened_attempt`), and
/// the tool call never reaches the conversation (history or any later
/// request).
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_rejected_warm_reply_carries_the_previous_summary_forward() {
    // (kind, its terminal, the wire stop reason on the rejected line)
    for (kind, terminal, stop) in [
        (WarmKind::ToolCall, "tool_call", Some("tool_use")),
        (WarmKind::Empty, "empty", None),
        (WarmKind::Length, "length", Some("max_tokens")),
        (WarmKind::FutureOnly, "empty_after_fold", None),
    ] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = kind;
        let rt = runtime(Some(SUMMARY));
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
        let event = events.rollover();
        assert_eq!(
            event.get("summary_terminal").map(String::as_str),
            Some(terminal),
            "{kind:?}: {event:?}"
        );
        assert_eq!(event.get("summary_attempted_path").map(String::as_str), Some("warm"));
        assert!(!event.contains_key("summary_cold_reason"), "{kind:?}: {event:?}");
        if let Some(stop) = stop {
            let rejected = events.logged("warm rollover summary rejected");
            assert_eq!(rejected.len(), 1, "{kind:?}");
            assert_eq!(rejected[0].get("stop_reason").map(String::as_str), Some(stop));
        }
        assert_eq!(
            summary_of(&second.heads[0]).as_deref(),
            Some(PREVIOUS),
            "{kind:?}: the previous summary is carried forward"
        );
        assert_eq!(mock.warm_requests(), 1, "{kind:?}: one warm request");
        assert_eq!(mock.cold_requests(), 0, "{kind:?}: no cold retry");
        let booked = second
            .usages
            .iter()
            .filter(|(i, o, _)| *i == 60 && *o == 250)
            .count();
        assert_eq!(booked, 1, "{kind:?}: {:?}", second.usages);
        let seen = mock.requests.lock().unwrap().clone();
        let warm_at = seen
            .iter()
            .position(|s| s.body.contains(WARM_MARK))
            .unwrap();
        assert!(
            seen[warm_at + 1..]
                .iter()
                .all(|s| !s.body.contains("toolu_warm_call") && !s.body.contains("WARM-")),
            "{kind:?}: nothing of the warm reply reaches a later request"
        );
        let history = serde_json::to_string(&second.history).unwrap();
        assert!(!history.contains("toolu_warm_call") && !history.contains("WARM-"));
    }
}

/// A summary whose only section is for a later window folds to nothing.
/// Warm or cold, that is a failed summary (`empty_after_fold`): the
/// previous summary is carried forward and nothing is retried.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_summary_that_folds_to_nothing_is_not_written() {
    // Warm.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = WarmKind::FutureOnly;
        let rt = runtime(Some(SUMMARY));
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
        assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
        assert_eq!(mock.warm_requests(), 1);
        assert_eq!(mock.cold_requests(), 0, "no cold retry");
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        assert_eq!(field("summary"), Some("carried_forward"));
        assert_eq!(field("summary_reason"), Some("failed"));
        assert_eq!(field("summary_attempted_path"), Some("warm"));
        assert_eq!(field("summary_terminal"), Some("empty_after_fold"));
    }
    // Cold: the record reported no cache read or write.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.cold_text.lock().unwrap() = Some("## Window 2\nCOLD-FUTURE-ONLY".into());
        let rt = runtime(Some(SUMMARY));
        set_thresholds(&rt, 300_000, 400_000);
        let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
        rt.continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded")
            .tap
            .completed(Some(ws::WarmUsage {
                input: 40_100,
                output: 5,
                ..ws::WarmUsage::default()
            }));
        let (events, _guard) = ContextEvents::install();
        set_thresholds(&rt, 60_000, 80_000);
        let mut next = first.history.clone();
        next.push(msg(json!({"role":"user","content":"carry on"})));
        let second = turn(&rt, next, Default::default()).await;
        assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
        assert_eq!(mock.cold_requests(), 1, "one cold attempt");
        assert_eq!(mock.warm_requests(), 0);
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        assert_eq!(field("summary"), Some("carried_forward"));
        assert_eq!(field("summary_reason"), Some("failed"));
        assert_eq!(field("summary_attempted_path"), Some("cold"));
        assert_eq!(field("summary_terminal"), Some("empty_after_fold"));
    }
}

/// Turn 1 below the thresholds: its request becomes the record.
async fn warmed(rt: &crate::Runtime) -> (Turn, Arc<ws::RequestRecord>) {
    set_thresholds(rt, 300_000, 400_000);
    let first = turn(rt, history(None), Default::default()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let record = rt
        .continuation
        .lock()
        .unwrap()
        .warm_record()
        .expect("recorded");
    (first, record)
}

/// What to send for `record`, from its own evidence.
fn send_for(record: &ws::RequestRecord) -> ws::WarmSend {
    let evidence = record.tap.evidence();
    ws::WarmSend {
        instruction: format!("{WARM_MARK}] test"),
        limit: 8_000,
        fingerprint: evidence.fingerprint.expect("fingerprint"),
        credential: evidence.credential.expect("credential"),
        endpoint: evidence.endpoint.expect("endpoint"),
        correlation: 1,
        published: Default::default(),
        run_deadline: None,
    }
}

/// Install `messages` as the chain's current record (same scope and other
/// inputs as `like`) and return it.
fn record_with(
    rt: &crate::Runtime,
    like: &ws::RequestRecord,
    messages: Vec<SharedMessage>,
) -> Arc<ws::RequestRecord> {
    let mut inputs = like.inputs.clone();
    inputs.durable = messages.clone();
    inputs.messages = messages;
    let mut state = rt.continuation.lock().unwrap();
    state.record_request(like.scope.clone(), inputs);
    state.warm_record().unwrap()
}

/// B1: a disclosure or compaction-mode change voids the record and starts a
/// new policy generation; the next rollover sends no warm request (the
/// screened path runs instead). Setting the same values changes nothing.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_disclosure_change_voids_the_record() {
    use agent_core::compaction::{CompactionMode, ContentClass};
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;
    let policy = rt.continuation.lock().unwrap().warm_policy;
    rt.set_compaction_exclusions(Vec::new());
    rt.set_compaction_mode(rt.compaction_mode);
    assert!(
        rt.continuation.lock().unwrap().warm_record().is_some(),
        "unchanged settings keep the record"
    );
    rt.set_compaction_exclusions(vec![ContentClass::Thinking]);
    assert!(rt.continuation.lock().unwrap().warm_record().is_none());
    assert_eq!(rt.continuation.lock().unwrap().warm_policy, policy + 1);
    drop(record);

    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(!second.heads.is_empty(), "the rollover still happens");
    assert_eq!(mock.warm_requests(), 0, "no warm replay");
    assert_eq!(mock.cold_requests(), 1, "the screened path runs once");

    // A compaction-mode change voids a newer record the same way.
    let _ = warmed(&rt).await;
    rt.set_compaction_mode(CompactionMode::LocalOnly);
    assert!(rt.continuation.lock().unwrap().warm_record().is_none());
}

/// B1: the record is bound to the credential source's identity. Any change
/// (local to remote, one broker to another, another machine identity)
/// voids it; a record from another source never passes W1.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_credential_source_change_voids_the_record() {
    use crate::auth::CredentialSource;
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;

    // W1 rejects a record from another source even if it were not cleared.
    rt.credential_source = CredentialSource::Remote {
        endpoint: "https://broker-a.invalid".into(),
        machine_token: "synthetic-machine-a".into(),
    };
    let summarizer = rt.warm_summarizer();
    let dropped = [1usize];
    let ctx = ws::WarmContext {
        warm: &summarizer,
        state: &rt.continuation,
        messages: &first.history,
        dropped: &dropped,
        newest_run_start: None,
    };
    assert_eq!(ws::check(&ctx, SUMMARY).unwrap_err(), ws::WarmMiss::Scope);
    rt.credential_source = CredentialSource::Local;

    let sources = [
        CredentialSource::Remote {
            endpoint: "https://broker-a.invalid".into(),
            machine_token: "synthetic-machine-a".into(),
        },
        CredentialSource::Remote {
            endpoint: "https://broker-a.invalid".into(),
            machine_token: "synthetic-machine-b".into(),
        },
        CredentialSource::Remote {
            endpoint: "https://broker-b.invalid".into(),
            machine_token: "synthetic-machine-b".into(),
        },
        CredentialSource::Local,
    ];
    for source in sources {
        let current = record_with(&rt, &record, record.inputs.messages.clone());
        let weak = Arc::downgrade(&current);
        drop(current);
        rt.set_credential_source(source.clone());
        assert!(weak.upgrade().is_none(), "{source:?} voids the record");
        // The same source again keeps a new record.
        let current = record_with(&rt, &record, record.inputs.messages.clone());
        rt.set_credential_source(source);
        assert!(rt
            .continuation
            .lock()
            .unwrap()
            .warm_record()
            .is_some_and(|r| Arc::ptr_eq(&r, &current)));
    }
    let scopes: std::collections::BTreeSet<u64> = [
        CredentialSource::Local,
        CredentialSource::Remote {
            endpoint: "https://broker-a.invalid".into(),
            machine_token: "synthetic-machine-a".into(),
        },
        CredentialSource::Remote {
            endpoint: "https://broker-a.invalid".into(),
            machine_token: "synthetic-machine-b".into(),
        },
        CredentialSource::Remote {
            endpoint: "https://broker-b.invalid".into(),
            machine_token: "synthetic-machine-a".into(),
        },
    ]
    .iter()
    .map(ws::source_scope)
    .collect();
    assert_eq!(scopes.len(), 4, "every source has its own scope");
}

/// B2: a warm request whose record is reset, replaced by a model switch or
/// voided by a disclosure change while it is being prepared (here: parked
/// on the tool registry lock) is not sent, and nothing else runs for it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_request_revoked_during_preparation_is_not_sent() {
    for revoke in ["reset", "model", "disclosure"] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let mut rt = runtime(Some(SUMMARY));
        let (first, record) = warmed(&rt).await;
        let send = send_for(&record);
        let before = mock.requests.lock().unwrap().len();

        let registry = Arc::clone(&rt.tools);
        let paused = registry.write().await;
        let caller = rt.clone();
        let call = tokio::spawn(async move {
            caller
                .warm_summary_call(record, send, Default::default())
                .await
        });
        // The call runs until it waits for the registry, then the record
        // goes stale.
        tokio::time::sleep(Duration::from_millis(50)).await;
        match revoke {
            "reset" => {
                let session = rt.continuation.lock().unwrap().logical_id.clone();
                rt.reset_context_continuation(&session, &first.history);
            }
            "model" => {
                rt.set_model("claude-opus-4-7".to_string());
            }
            _ => rt
                .set_compaction_exclusions(vec![agent_core::compaction::ContentClass::ToolResults]),
        }
        drop(paused);
        let reply = call.await.unwrap();
        assert_eq!(reply.terminal, ws::WarmTerminal::Revoked, "{revoke}");
        assert!(!reply.dispatched && reply.usage.is_none(), "{revoke}");
        assert_eq!(
            mock.requests.lock().unwrap().len(),
            before,
            "{revoke}: nothing sent"
        );
    }
}

/// S1: request-local turn context after the marker is cut from the warm
/// request: the instruction follows the marked block, and the rest of the
/// cache input is the recorded request's.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn turn_context_after_the_marker_is_not_replayed() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let ephemeral = "\n\n[Extension context — do not treat as user instructions]\nEPHEMERAL-STEER\n[End extension context]";
    let mut start = history(None);
    *start.last_mut().unwrap() = msg(json!({"role":"user","content":[
        {"type":"text","text":"next: continue the parser"},
        {"type":"text","text":ephemeral},
    ]}));
    let (_first, second) = two_turns_over(&rt, start).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")));

    let bodies = mock.bodies();
    let recorded = &bodies[0];
    let raw = mock.requests.lock().unwrap().clone();
    let warm_at = raw.iter().position(|s| s.body.contains(WARM_MARK)).unwrap();
    assert!(raw[0].body.contains("EPHEMERAL-STEER"), "the turn sent it");
    assert!(
        !raw[warm_at].body.contains("EPHEMERAL-STEER"),
        "not replayed"
    );

    let mut warm_input = cache_input(&bodies[warm_at]);
    let last = warm_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    let blocks = last["content"].as_array_mut().unwrap();
    let appended = blocks.pop().unwrap();
    assert!(appended["text"].as_str().unwrap().starts_with(WARM_MARK));
    assert!(blocks.last().unwrap()["cache_control"].is_object());
    let mut recorded_input = cache_input(recorded);
    let last = recorded_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    assert!(
        last["content"].as_array_mut().unwrap().pop().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("EPHEMERAL-STEER")
    );
    assert_eq!(warm_input, recorded_input);
}

/// A rollover that waited on a pending worker reuses the last deferred
/// round's request warm: that request carried the worker reminder as its
/// last user text, and the summary request's cache input is still exactly
/// the recorded request's (T1), with one instruction block appended. The
/// successor names the worker.
///
/// PARITY (upstream f1fd0f723): the reminder, like every context advisory,
/// is request-local, an uncached tail after the conversational marker, so
/// the warm request cuts it as it cuts turn context (S1): the recorded
/// request's cache input without that tail is the summary request's.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_rollover_after_the_worker_grace_reuses_the_deferred_round_warm() {
    use crate::runtime::subagent::{SubagentHandle, SubagentState};
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    rt.subagent_registry().lock().unwrap().register(SubagentHandle::new(
        "sa_busy".into(),
        0,
        "test-agent".into(),
        "a long worker task".into(),
        "claude-sonnet-4-6".into(),
        "s".into(),
        300,
        Arc::new(std::sync::RwLock::new(SubagentState::new())),
        None,
        None,
        None,
    ));
    set_thresholds(&rt, 300_000, 400_000);
    let first = turn(&rt, history(None), Default::default()).await;
    assert!(first.errors.is_empty() && first.heads.is_empty());
    set_thresholds(&rt, 60_000, 80_000);
    let reminder = "waiting briefly for pending workers";
    let mut previous = first.history;
    let mut rolled = None;
    for round in 0..=crate::runtime::continuation::WORKER_ROLLOVER_GRACE_ROUNDS {
        let mut next = previous.clone();
        next.push(msg(json!({"role":"user","content":format!("round {round}")})));
        let out = turn(&rt, next, Default::default()).await;
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        if !out.heads.is_empty() {
            rolled = Some((round, out));
            break;
        }
        previous = out.history;
    }
    let (round, out) = rolled.expect("rolls over once the grace is spent");
    assert_eq!(round, crate::runtime::continuation::WORKER_ROLLOVER_GRACE_ROUNDS);
    let head = &out.heads[0];
    assert!(summary_of(head).is_some_and(|s| s.contains("WARM-SUMMARY")));
    assert!(head[0]["content"].as_str().unwrap().contains("sa_busy"));
    assert!(!head.iter().any(|m| m.to_string().contains(reminder)));
    assert_eq!(mock.warm_requests(), 1, "one warm request");
    assert_eq!(mock.cold_requests(), 0, "no cold request");

    let raw = mock.requests.lock().unwrap().clone();
    let bodies = mock.bodies();
    let warm_at = raw.iter().position(|s| s.body.contains(WARM_MARK)).unwrap();
    let recorded = &bodies[warm_at - 1];
    assert!(raw[warm_at - 1].body.contains(reminder), "a deferred round");
    let mut recorded_input = cache_input(recorded);
    let last = recorded_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    let blocks = last["content"].as_array_mut().unwrap();
    let tail = blocks.pop().unwrap();
    assert!(tail["text"].as_str().unwrap().contains(reminder), "{tail}");
    assert!(tail.get("cache_control").is_none(), "after the marker");
    assert!(blocks.last().unwrap()["cache_control"].is_object());
    let mut warm_input = cache_input(&bodies[warm_at]);
    let last = warm_input["messages"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    let appended = last["content"].as_array_mut().unwrap().pop().unwrap();
    let text = appended["text"].as_str().unwrap();
    assert!(text.starts_with(WARM_MARK));
    assert!(text.contains(SUPERSEDES), "{text}");
    assert!(appended.get("cache_control").is_none());
    assert_eq!(warm_input, recorded_input, "identical cache input");
    // The request-local reminder is not replayed; the instruction follows
    // the marked block.
    let body = &raw[warm_at].body;
    assert!(!body.contains(reminder), "not replayed");
    assert!(body.contains(SUPERSEDES));
}

/// S1: when the recorded request ends with tool results, the instruction is
/// appended after them in the same user message; the pairs stay intact.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_tool_result_tail_keeps_its_pairs() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let mut start = history(None);
    start.pop(); // ends with the t_new tool result
    let (_first, second) = two_turns_over(&rt, start).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")));
    let bodies = mock.bodies();
    let warm = bodies
        .iter()
        .find(|b| b.to_string().contains(WARM_MARK))
        .unwrap();
    let messages: Vec<SharedMessage> = warm["messages"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(Arc::new)
        .collect();
    let last = &messages.last().unwrap()["content"];
    assert_eq!(last[0]["type"], "tool_result");
    assert_eq!(last[0]["tool_use_id"], "t_new");
    assert!(last[0]["cache_control"].is_object());
    assert!(last[1]["text"].as_str().unwrap().starts_with(WARM_MARK));
    assert!(ws::protocol_complete(&messages));
    assert_pairs_intact(&messages);
}

/// S1: a record ending with an assistant message (a prefill) or with an
/// unanswered tool call is refused before sending, as a protocol miss (the
/// cold path may run).
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_assistant_tail_or_open_tool_call_is_not_sent() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    let send = send_for(&record);
    let before = mock.requests.lock().unwrap().len();
    let mut prefill = record.inputs.messages.clone();
    prefill.push(msg(json!({"role":"assistant","content":"Sure, here"})));
    let mut open = record.inputs.messages.clone();
    open.push(msg(json!({"role":"assistant","content":[{"type":"tool_use","id":"t_open","name":"bash","input":{}}]})));
    open.push(msg(json!({"role":"user","content":"no result for it"})));
    for messages in [prefill, open] {
        let current = record_with(&rt, &record, messages);
        let reply = rt
            .warm_summary_call(current, send.clone(), Default::default())
            .await;
        assert_eq!(
            reply.terminal,
            ws::WarmTerminal::NotSent(ws::WarmMiss::Protocol)
        );
        assert!(!reply.dispatched);
    }
    assert_eq!(mock.requests.lock().unwrap().len(), before, "nothing sent");
}

/// S2: a local failure while the request is prepared (here: history the
/// request validator rejects) is a typed pre-send miss, not an error: the
/// cold path may run. Provenance comes from the transport's sent flag.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_failure_before_sending_is_a_pre_send_miss() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    let send = send_for(&record);
    let before = mock.requests.lock().unwrap().len();
    let mut invalid = record.inputs.messages.clone();
    invalid.insert(
        0,
        msg(json!({"role":"user","content":{"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}})),
    );
    let current = record_with(&rt, &record, invalid);
    let reply = rt
        .warm_summary_call(current, send, Default::default())
        .await;
    assert_eq!(
        reply.terminal,
        ws::WarmTerminal::NotSent(ws::WarmMiss::PreSend)
    );
    assert!(!reply.dispatched && reply.usage.is_none());
    assert_eq!(mock.requests.lock().unwrap().len(), before, "nothing sent");
}

/// A transport-level miss after the checks passed (the rebuilt prefix
/// differs from the recorded digest) sends nothing warm and runs the
/// screened path exactly once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_transport_miss_goes_cold_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;
    record.tap.built([0x5a; 32]);
    drop(record);
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")));
    assert_eq!(mock.warm_requests(), 0);
    assert_eq!(mock.cold_requests(), 1);
}

/// The recorded request's credential is the one it was authorized with; an
/// Anthropic failover hop rebinds the session's seat without re-authorizing
/// the record. A summary on a seat other than the recorded one (that
/// account holds no cache for the prefix) is never sent warm: it goes cold
/// once, on the seat the session is bound to now.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_summary_on_another_seat_than_the_record_goes_cold() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;
    drop(record);
    let seat_b = "SYNTHETIC-SEAT-B-TOKEN";
    rt.auth.write().await.auth_token = seat_b.to_owned();
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")));
    assert_eq!(mock.warm_requests(), 0);
    assert_eq!(mock.cold_requests(), 1);
    let cold_on_b = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.headers.values().any(|v| v.contains(seat_b)))
        .count();
    assert!(cold_on_b >= 1, "the cold summary used the bound seat");
}

/// A warm summary is sent once and never follows a redirect, whatever the
/// status (a POST 301/302/303 would become a body-less GET, a 307/308 a
/// replay) or the target (another listener, or a refused port). The redirect
/// is the request's answer: it was sent, nothing is measured, so the
/// reservation is kept as its charge.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_summary_never_follows_a_redirect() {
    for refused in [false, true] {
        for status in [301u16, 302, 303, 307, 308] {
            let _base = crate::test_env::BaseDirGuard::new();
            let (mock, _g) = spawn_warm().await;
            let rt = runtime(Some(SUMMARY));
            let (_first, record) = warmed(&rt).await;
            let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = other.local_addr().unwrap();
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counting = if refused {
                drop(other);
                None
            } else {
                let connections = Arc::clone(&connections);
                Some(tokio::spawn(async move {
                    while let Ok((_stream, _)) = other.accept().await {
                        connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }))
            };
            *mock.redirect.lock().unwrap() =
                Some((status, format!("http://{target}/v1/messages")));
            let reply = rt
                .warm_summary_call(Arc::clone(&record), send_for(&record), Default::default())
                .await;
            let case = format!("{status} refused={refused}");
            assert!(
                matches!(reply.terminal, ws::WarmTerminal::Error),
                "{case}: {:?}",
                reply.terminal
            );
            assert!(reply.dispatched, "{case}: the first request was sent");
            assert_eq!(reply.usage, None, "{case}: nothing measured");
            assert_eq!(mock.warm_requests(), 1, "{case}: one request");
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                connections.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{case}: no second connection"
            );
            if let Some(counting) = counting {
                counting.abort();
            }
        }
    }
}

/// Through the accounting entry point (`summarize()`, as a rollover runs
/// it): a redirected warm summary was sent and nothing was measured, so its
/// reservation stays held as the charge. The redirect is not followed (no
/// connection to its target) and the cold path does not run.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_redirected_warm_rollover_keeps_its_reservation() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = other.local_addr().unwrap();
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting = {
        let connections = Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((_stream, _)) = other.accept().await {
                connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    *mock.redirect.lock().unwrap() = Some((307, format!("http://{target}/v1/messages")));
    let before = mock.requests.lock().unwrap().len();

    let out = summarize_held(&mut rt, &mock, &first, true, |_| {}).await;

    assert!(matches!(out.text, Err(None)), "{:?}", out.text);
    assert!(
        out.reserved > 0.0 && out.cost == 0.0,
        "spend unknown: the reservation is kept ({} reserved, {} measured)",
        out.reserved,
        out.cost
    );
    assert_eq!(out.usage_events, 0, "nothing booked");
    assert_eq!(mock.warm_requests(), 1, "one warm request");
    assert_eq!(
        mock.requests.lock().unwrap().len() - before,
        1,
        "no cold request after it"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no second connection"
    );
    counting.abort();
}

/// L1: warm usage is booked only when the provider reported every counter
/// the price needs (output on the final `message_delta`; input, cache reads
/// and writes there or on `message_start`). A partial report is Unknown:
/// nothing is booked from zero-filled counters and the reservation stays
/// held as the charge.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn warm_usage_is_booked_only_when_reported_in_full() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    let full = ws::WarmUsage {
        input: 60,
        output: 250,
        cache_read: 45_000,
        cache_write: 0,
        cache_write_5m: None,
        cache_write_1h: None,
    };
    for (kind, expected) in [
        (WarmKind::Text, Some(full)),
        (WarmKind::PartialUsage, None),
        (WarmKind::NoFinalUsage, None),
    ] {
        *mock.kind.lock().unwrap() = kind;
        let reply = rt
            .warm_summary_call(Arc::clone(&record), send_for(&record), Default::default())
            .await;
        assert!(
            matches!(reply.terminal, ws::WarmTerminal::Written(_)),
            "{kind:?}: {:?}",
            reply.terminal
        );
        assert!(reply.dispatched, "{kind:?}");
        assert_eq!(reply.usage, expected, "{kind:?}");
    }

    // Through a rollover: the summary is written, nothing is booked for it.
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = WarmKind::PartialUsage;
    let rt = runtime(Some(SUMMARY));
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-PARTIAL")));
    assert!(
        second.usages.iter().all(|(i, o, _)| (*i, *o) != (60, 250)),
        "{:?}",
        second.usages
    );
}

/// L1: the recorded request's usage must be reported in full too; without
/// it there is no cache or size evidence, so the summary goes cold.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_record_without_full_usage_goes_cold() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.turn_usage.lock().unwrap() =
        Some(json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":40_000}));
    let rt = runtime(Some(SUMMARY));
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")));
    assert_eq!(mock.warm_requests(), 0);
    assert_eq!(mock.cold_requests(), 1);
}

/// L1: an armed `/trace next content` belongs to the next conversation
/// request. The warm summary request neither consumes the arm nor writes a
/// content capture.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_warm_request_leaves_an_armed_content_capture_alone() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    rt.trace_arm_next(true);
    let armed = rt.trace_controls.peek();
    let reply = rt
        .warm_summary_call(Arc::clone(&record), send_for(&record), Default::default())
        .await;
    assert!(matches!(reply.terminal, ws::WarmTerminal::Written(_)));
    assert_eq!(rt.trace_controls.peek(), armed, "the arm is still pending");
    let captured = std::fs::read_dir(&rt.capture_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(captured, 0, "no content capture");
}

/// What one parked `summarize()` run ended with.
struct Parked {
    text: std::result::Result<String, Option<crate::runtime::rolling_summary::SummarySkip>>,
    miss: Option<ws::WarmMiss>,
    /// The allowance still held, and what was booked.
    reserved: f64,
    cost: f64,
    /// Usage events the summary sent.
    usage_events: usize,
}

/// One rollover summary through `summarize()` over turn 1's record, its
/// warm request parked at its start until `during` has run on the runtime.
/// With `tamper`, the released request then fails locally on its way out:
/// its digest no longer matches the rebuilt prefix.
async fn summarize_parked(
    rt: &mut crate::Runtime,
    first: &Turn,
    tamper: bool,
    during: impl FnOnce(&mut crate::Runtime),
) -> Parked {
    let mut warm = rt.warm_summarizer();
    let parked = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let inner = rt.clone();
    let (p, r) = (parked.clone(), release.clone());
    warm.call = Arc::new(move |record, mut send, cancel| {
        let (runtime, parked, release) = (inner.clone(), p.clone(), r.clone());
        Box::pin(async move {
            parked.notify_one();
            release.notified().await;
            if tamper {
                send.fingerprint[0] ^= 1;
            }
            runtime.warm_summary_call(record, send, cancel).await
        })
    });
    summarize_during(rt, first, Some(warm), &parked, &release, during).await
}

/// One rollover summary through `summarize()` while the mock holds its
/// reply: `during` runs once the mock has the whole request body, then the
/// reply is released. `warm`: with the warm context, else the screened
/// path alone.
async fn summarize_held(
    rt: &mut crate::Runtime,
    mock: &WarmMock,
    first: &Turn,
    warm: bool,
    during: impl FnOnce(&mut crate::Runtime),
) -> Parked {
    let hold = Hold::default();
    *mock.hold.lock().unwrap() = Some(hold.clone());
    let warm = warm.then(|| rt.warm_summarizer());
    let out = summarize_during(rt, first, warm, &hold.received, &hold.release, during).await;
    *mock.hold.lock().unwrap() = None;
    out
}

/// Runs one summary of turn 1's second message through `summarize()`
/// (warm through `warm` when given), and `during` on the runtime once
/// `reached` is notified, then notifies `release`. Bounded.
async fn summarize_during(
    rt: &mut crate::Runtime,
    first: &Turn,
    warm: Option<ws::WarmSummarizer>,
    reached: &tokio::sync::Notify,
    release: &tokio::sync::Notify,
    during: impl FnOnce(&mut crate::Runtime),
) -> Parked {
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    assert!(summarizer.warm().is_some(), "the warm path is configured");
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let archive = "a".repeat(32);
    let dropped = vec![first.history[1].clone()];
    let indices = [1usize];
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = rt.continuation.clone();
    let logical_id = state.lock().unwrap().logical_id.clone();
    let messages = first.history.clone();
    let summary = crate::runtime::rolling_summary::summarize(
        &summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &logical_id,
            previous: None,
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout: Duration::from_secs(30),
            cancel: &cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(ws::SnapshotGate::new(&state)),
            run_deadline: None,
        },
        warm.as_ref().map(|warm| ws::WarmContext {
            warm,
            state: &state,
            messages: &messages,
            dropped: &indices,
            newest_run_start: None,
        }),
    );
    let invalidate = async {
        let reached = tokio::time::timeout(Duration::from_secs(10), reached.notified())
            .await
            .is_ok();
        during(rt);
        release.notify_one();
        reached
    };
    let (out, reached) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(summary, invalidate)
    })
    .await
    .expect("the summary finishes");
    assert!(reached, "the summary request was prepared");
    let mut usage_events = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(
            event,
            crate::StreamEvent::Session(crate::SessionEvent::Usage { .. })
        ) {
            usage_events += 1;
        }
    }
    Parked {
        text: out.text,
        miss: out.report.warm_miss,
        reserved: usage.reserved_usd(),
        cost: usage.cost_usd(),
        usage_events,
    }
}

/// F1: an invalidation while the warm request is prepared outranks a local
/// failure on its way out (here: the rebuilt prefix no longer matches its
/// digest). Through `summarize()`: no warm and no cold request is sent, the
/// reservation is released, and no summary is written (the previous one
/// is carried).
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_invalidation_during_preparation_outranks_a_local_failure() {
    for revoke in ["reset", "credential source", "disclosure"] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let mut rt = runtime(Some(SUMMARY));
        let (first, _record) = warmed(&rt).await;
        let before = mock.requests.lock().unwrap().len();
        let history = first.history.clone();
        let out = summarize_parked(&mut rt, &first, true, |rt| match revoke {
            "reset" => {
                let session = rt.continuation.lock().unwrap().logical_id.clone();
                rt.reset_context_continuation(&session, &history);
            }
            "credential source" => rt.set_credential_source(crate::auth::CredentialSource::Remote {
                endpoint: "https://broker-a.invalid".into(),
                machine_token: "synthetic-machine-a".into(),
            }),
            _ => rt.set_compaction_exclusions(vec![
                agent_core::compaction::ContentClass::ToolResults,
            ]),
        })
        .await;
        assert!(matches!(out.text, Err(None)), "{revoke}: {:?}", out.text);
        assert_eq!(out.miss, Some(ws::WarmMiss::Superseded), "{revoke}");
        assert_eq!(
            mock.requests.lock().unwrap().len(),
            before,
            "{revoke}: no warm and no cold request"
        );
        assert_eq!((out.reserved, out.cost), (0.0, 0.0), "{revoke}: released");
    }
}

/// A tool for registering into a live registry.
struct LateTool;
#[async_trait::async_trait]
impl crate::Tool for LateTool {
    fn name(&self) -> &str {
        "late_tool"
    }
    fn description(&self) -> &str {
        "registered after the record"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, _ctx: crate::ToolContext) -> crate::Result<String> {
        Ok(String::new())
    }
}

/// One change to what conversation requests' cached prefix is built from.
fn change_prefix(rt: &mut crate::Runtime, change: &str) {
    match change {
        "tool disabled" => rt
            .tools
            .try_write()
            .expect("the registry is free")
            .disable(&["bash".to_owned()]),
        "tool registered" => rt
            .tools
            .try_write()
            .expect("the registry is free")
            .register(Arc::new(LateTool)),
        "prompt reloaded" => {
            let module = agent_core::prompt::PromptModule::new(
                agent_core::prompt::PromptModuleId::parse("kernel.test").unwrap(),
                "1.0.0",
                agent_core::prompt::PromptModuleSource::User,
                0,
                agent_core::prompt::PromptSelectors::default(),
                agent_core::prompt::ModuleMutability::MutableGuidance,
                "RELOADED PROMPT.",
            )
            .unwrap();
            let context = agent_core::prompt::SelectionContext::new(
                agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap(),
                None,
            )
            .unwrap();
            rt.apply_prompt_stack(
                agent_core::prompt::PromptStack::new(vec![module], context).unwrap(),
            )
            .unwrap();
        }
        _ => rt.set_cache_ttl(crate::core::config::CacheTtl::OneHour),
    }
}

/// F2: a change to what the cached prefix is built from (a tool disabled or
/// registered, the prompt stack reloaded, the cache TTL) sends that
/// rollover's summary screened cold, whether it happens before the
/// rollover plans or while the warm request is prepared: no warm request,
/// one screened request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_live_prefix_change_sends_the_summary_cold() {
    for change in ["tool disabled", "tool registered", "prompt reloaded", "ttl"] {
        // Before the rollover plans.
        {
            let _base = crate::test_env::BaseDirGuard::new();
            let (mock, _g) = spawn_warm().await;
            let mut rt = runtime(Some(SUMMARY));
            let (first, record) = warmed(&rt).await;
            drop(record);
            change_prefix(&mut rt, change);
            set_thresholds(&rt, 60_000, 80_000);
            let mut next = first.history.clone();
            next.push(msg(json!({"role":"user","content":"carry on"})));
            let second = turn(&rt, next, Default::default()).await;
            assert!(
                summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")),
                "{change}"
            );
            assert_eq!(mock.warm_requests(), 0, "{change}: before planning");
            assert_eq!(mock.cold_requests(), 1, "{change}: before planning");
        }
        // While the warm request is prepared.
        {
            let _base = crate::test_env::BaseDirGuard::new();
            let (mock, _g) = spawn_warm().await;
            let mut rt = runtime(Some(SUMMARY));
            let (first, _record) = warmed(&rt).await;
            let out = summarize_parked(&mut rt, &first, false, |rt| change_prefix(rt, change)).await;
            assert!(
                out.text.as_ref().is_ok_and(|t| t.contains("COLD-SUMMARY")),
                "{change}: {:?}",
                out.text
            );
            assert_eq!(out.miss, Some(ws::WarmMiss::PrefixPolicy), "{change}");
            assert_eq!(mock.warm_requests(), 0, "{change}: while parked");
            assert_eq!(mock.cold_requests(), 1, "{change}: while parked");
        }
    }
}

/// True while a connection to 127.0.0.1:`port` is in SYN-SENT: a client
/// reached its connect, and the listener's full accept queue holds it there.
/// IPv4 only (`/proc/net/tcp`): the fixtures bind 127.0.0.1.
#[cfg(target_os = "linux")]
fn connect_stalled_at(port: u16) -> bool {
    let remote = format!("0100007F:{port:04X}");
    std::fs::read_to_string("/proc/net/tcp").is_ok_and(|table| {
        table.lines().skip(1).any(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.get(2) == Some(&remote.as_str()) && fields.get(3) == Some(&"02")
        })
    })
}

/// Transport barrier: waits (at most 10 s) until a connect to `port` is
/// stalled in SYN-SENT.
#[cfg(target_os = "linux")]
async fn connect_reached(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if connect_stalled_at(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// A loopback listener with room for exactly one unaccepted connection.
#[cfg(target_os = "linux")]
fn one_slot_listener() -> tokio::net::TcpListener {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    socket.listen(0).unwrap()
}

/// Fills `addr`'s one-slot accept queue (bounded): the next connect's SYN
/// is dropped until the queue drains.
#[cfg(target_os = "linux")]
async fn fill_queue(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
        .await
        .expect("the queue-filling connect completes")
        .unwrap()
}

/// Drains the queue (the filler first), then returns what the stalled
/// connection sends until it closes. Every step is bounded.
#[cfg(target_os = "linux")]
async fn drain_stalled(
    listener: &tokio::net::TcpListener,
    filler: tokio::net::TcpStream,
) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let (accepted, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("the filler is accepted")
        .unwrap();
    drop((accepted, filler));
    let (mut stalled, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
        .await
        .expect("the stalled connection arrives")
        .unwrap();
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stalled.read_to_end(&mut raw)).await;
    raw
}

/// Body bytes after the request head in `raw` (0 when no head arrived).
fn body_len(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(0, |end| raw.len() - end - 4)
}

/// F3: the last lease check, when the client pulls the body's only chunk,
/// authorizes publishing it. A warm request whose connection stalls (the
/// listener's accept queue is full, so the connect waits in SYN-SENT for a
/// retransmission) and that is revoked meanwhile is withheld there: the
/// listener receives no body byte, the outcome is Revoked and nothing is
/// booked.
#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_request_revoked_on_a_stalled_connection_sends_no_body() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let upstream = std::env::var("SYNAPS_ANTHROPIC_BASE_URL")
        .unwrap()
        .trim_start_matches("http://")
        .to_owned();
    // A forwarding listener in front of the mock.
    let listener = Arc::new(one_slot_listener());
    let addr = listener.local_addr().unwrap();
    let forwarding = {
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut inbound, _)) = listener.accept().await {
                let upstream = upstream.clone();
                connections.spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        })
    };
    let _proxy =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    let rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;
    let send = send_for(&record);
    // Stop forwarding (closing every forwarded connection), then fill the
    // accept queue.
    forwarding.abort();
    let _ = forwarding.await;
    let filler = fill_queue(addr).await;
    let before = mock.requests.lock().unwrap().len();

    let caller = rt.clone();
    let call = tokio::spawn(async move {
        caller
            .warm_summary_call(record, send, Default::default())
            .await
    });
    assert!(
        connect_reached(addr.port()).await,
        "the request reached its connect and stalled there"
    );
    assert!(!call.is_finished(), "the connection is stalled");
    let session = rt.continuation.lock().unwrap().logical_id.clone();
    rt.reset_context_continuation(&session, &first.history);

    // Drain the queue; record what the stalled connection then sends.
    let raw = drain_stalled(&listener, filler).await;
    let reply = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("the warm call ends")
        .unwrap();
    assert_eq!(reply.terminal, ws::WarmTerminal::Revoked);
    assert!(!reply.dispatched && reply.usage.is_none());
    assert_eq!(body_len(&raw), 0, "no body byte reached the listener");
    assert_eq!(mock.requests.lock().unwrap().len(), before);
}

/// A screened (cold) summary on a model whose thinking cannot be turned off
/// (Opus 5.5) asks for adaptive thinking at effort low and leaves the cap
/// again for it: the request's `max_tokens`, the reported limit and the
/// reservation all use twice the cap. Through `update()`.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cold_summary_where_thinking_cannot_be_off_leaves_room_for_it() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let mut rt = runtime(Some(SUMMARY));
    rt.set_model("claude-opus-5-5".to_string());
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    assert_eq!(summarizer.model(), "claude-opus-5-5");
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped = history(None)[..4].to_vec();
    let archive = "a".repeat(32);
    let cancel = tokio_util::sync::CancellationToken::new();
    let session = rt.continuation.lock().unwrap().logical_id.clone();
    let (out, report) = crate::runtime::rolling_summary::update_with_report(
        &summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &session,
            previous: Some("## Window 0\nPREVIOUS"),
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout: Duration::from_secs(30),
            cancel: &cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(ws::SnapshotGate::new(&rt.continuation)),
            run_deadline: None,
        },
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    let cold: Vec<Value> = mock
        .bodies()
        .into_iter()
        .filter(|b| b.get("stream").is_none())
        .collect();
    assert_eq!(cold.len(), 1);
    let limit = 2 * SUMMARY;
    assert_eq!(cold[0]["max_tokens"], limit);
    assert_eq!(cold[0]["thinking"], json!({"type": "adaptive"}));
    assert_eq!(cold[0]["output_config"], json!({"effort": "low"}));
    assert_eq!(report.limit_tokens, Some(limit));
    // The reservation priced every token the limit allows.
    let output_only = agent_core::pricing::calculate_cost_optional_split(
        "claude-opus-5-5",
        0,
        limit,
        0,
        0,
        None,
        None,
    );
    let reserved = report.reserved_usd.expect("reserved");
    assert!(reserved > output_only, "{reserved} <= {output_only}");
}

/// N1: a screened (cold) summary on the native Anthropic transport has the
/// same publication point. Its connection stalls after the last check
/// before the send, and the session is reset meanwhile: the body is
/// withheld, the request counts as not sent (the allowance is released,
/// nothing is booked) and no summary is used. Through `update()`.
#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cold_summary_revoked_on_a_stalled_connection_sends_no_body() {
    let _base = crate::test_env::BaseDirGuard::new();
    let listener = one_slot_listener();
    let addr = listener.local_addr().unwrap();
    let filler = fill_queue(addr).await;
    let _g =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    let rt = runtime(Some(SUMMARY));
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped = history(None)[..4].to_vec();
    let archive = "a".repeat(32);
    let cancel = tokio_util::sync::CancellationToken::new();
    let session = rt.continuation.lock().unwrap().logical_id.clone();
    let summary = crate::runtime::rolling_summary::update(
        &summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &session,
            previous: Some("## Window 0\nPREVIOUS"),
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout: Duration::from_secs(30),
            cancel: &cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(ws::SnapshotGate::new(&rt.continuation)),
            run_deadline: None,
        },
    );
    let revoke = async {
        let reached = connect_reached(addr.port()).await;
        let held = usage.reserved_usd();
        rt.reset_context_continuation(&session, &[]);
        (reached, held, drain_stalled(&listener, filler).await)
    };
    let (out, (reached, held, raw)) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(summary, revoke)
    })
    .await
    .expect("the cold summary ends");
    assert!(reached, "the request reached its connect and stalled there");
    assert!(held > 0.0, "the allowance was held while it stalled");
    assert!(matches!(out, Err(None)), "{out:?}");
    assert_eq!(body_len(&raw), 0, "no body byte reached the listener");
    assert_eq!(
        (usage.reserved_usd(), usage.cost_usd()),
        (0.0, 0.0),
        "not sent: released, nothing booked"
    );
    assert!(rx.try_recv().is_err(), "no usage event");
}

/// The session changes after the provider has the whole summary request,
/// before its reply completes. Warm and native cold alike: the usage is
/// booked once, the text is not used (the previous summary is carried),
/// and nothing stays reserved.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_reply_to_a_changed_session_is_booked_once_and_not_used() {
    for warm in [true, false] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let mut rt = runtime(Some(SUMMARY));
        let (first, _record) = warmed(&rt).await;
        let before = mock.requests.lock().unwrap().len();
        let history = first.history.clone();
        let out = summarize_held(&mut rt, &mock, &first, warm, |rt| {
            let session = rt.continuation.lock().unwrap().logical_id.clone();
            rt.reset_context_continuation(&session, &history);
        })
        .await;
        assert!(matches!(out.text, Err(None)), "warm={warm}: {:?}", out.text);
        assert_eq!(out.usage_events, 1, "warm={warm}: booked once");
        assert!(out.cost > 0.0, "warm={warm}: the measured usage is booked");
        assert_eq!(out.reserved, 0.0, "warm={warm}: nothing stays held");
        assert_eq!(
            mock.requests.lock().unwrap().len() - before,
            1,
            "warm={warm}: one summary request"
        );
        assert_eq!(mock.warm_requests(), usize::from(warm), "warm={warm}");
        assert_eq!(mock.cold_requests(), usize::from(!warm), "warm={warm}");
    }
}

/// Slice 7 (design §1 ordering): the summary attempt waits for no successor
/// acknowledgement, but the successor head is published only after it, and
/// carries the written summary (or the carried one when the warm reply is
/// rejected); exactly one head is published per rollover, never a
/// summary-less one first. Successor inference waits for the head's
/// durable acknowledgement; a failed acknowledgement stops the turn before
/// any further request.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_successor_head_carries_the_summary_and_inference_waits_for_its_ack() {
    for (kind, acknowledged, expected) in [
        (WarmKind::Text, true, "WARM-SUMMARY"),
        (WarmKind::ToolCall, true, PREVIOUS),
        (WarmKind::Text, false, "WARM-SUMMARY"),
    ] {
        let case = format!("{kind:?} ack={acknowledged}");
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = kind;
        let rt = runtime(Some(SUMMARY));
        set_thresholds(&rt, 300_000, 400_000);
        let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
        assert!(first.errors.is_empty(), "{case}: {:?}", first.errors);
        assert!(first.heads.is_empty(), "{case}");
        set_thresholds(&rt, 60_000, 80_000);
        let mut next = first.history.clone();
        next.push(msg(json!({"role":"user","content":"carry on"})));

        let mut stream = rt
            .run_stream_with_messages(next, Default::default(), None, None, false)
            .await;
        let (mut heads, mut errors) = (0, Vec::new());
        while let Some(event) = stream.next().await {
            match event {
                agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(e)) => {
                    errors.push(e)
                }
                agent_core::StreamEvent::Session(
                    agent_core::SessionEvent::ContextHeadCheckpoint {
                        messages, receipt, ..
                    },
                ) => {
                    heads += 1;
                    // The summary attempt is over before the head exists.
                    assert_eq!(mock.warm_requests(), 1, "{case}: summary before the head");
                    assert_eq!(
                        summary_of(&messages).as_deref().map(|s| s.contains(expected)),
                        Some(true),
                        "{case}: {:?}",
                        summary_of(&messages)
                    );
                    // No successor request while the head is not durable.
                    let sent = mock.requests.lock().unwrap().len();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    assert_eq!(
                        mock.requests.lock().unwrap().len(),
                        sent,
                        "{case}: inference waits for the acknowledgement"
                    );
                    receipt.complete(if acknowledged {
                        Ok(())
                    } else {
                        Err(std::io::Error::other("synthetic save failure"))
                    });
                    if acknowledged {
                        assert_eq!(sent, 2, "{case}: turn 1 and the warm summary only");
                    }
                }
                _ => {}
            }
        }
        assert_eq!(heads, 1, "{case}: one head per rollover");
        let after = mock.requests.lock().unwrap().len();
        if acknowledged {
            assert!(errors.is_empty(), "{case}: {errors:?}");
            assert_eq!(after, 3, "{case}: the successor's request follows the ack");
        } else {
            assert!(!errors.is_empty(), "{case}: the failed save is reported");
            assert_eq!(after, 2, "{case}: no request after a failed save");
        }
    }
}

/// Slice 7: the summary attempt on the rollover's critical path is bounded.
/// A warm reply slower than `summary_timeout_secs` carries the previous
/// summary into the one published head, and the successor proceeds after
/// its acknowledgement; a user cancel during the attempt stops the rollover
/// before any head is published or any further request is sent.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_slow_or_canceled_warm_summary_stays_bounded() {
    for cancel_it in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = WarmKind::Stall;
        let rt = runtime(Some(SUMMARY));
        rt.continuation.lock().unwrap().config.summary_timeout_secs = 1;
        set_thresholds(&rt, 300_000, 400_000);
        let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        set_thresholds(&rt, 60_000, 80_000);
        let mut next = first.history.clone();
        next.push(msg(json!({"role":"user","content":"carry on"})));
        let cancel = tokio_util::sync::CancellationToken::new();
        if cancel_it {
            let cancel = cancel.clone();
            let mock = mock.clone();
            tokio::spawn(async move {
                while mock.warm_requests() == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                cancel.cancel();
            });
        }
        let (events, _guard) = ContextEvents::install();
        let started = Instant::now();
        let second = turn(&rt, next, cancel).await;
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "cancel={cancel_it}: bounded, not the reply's five seconds"
        );
        if !cancel_it {
            // The request reached the provider before the timeout: it is
            // reported as sent, with its id.
            let event = events.rollover();
            let field = |key: &str| event.get(key).map(String::as_str);
            assert_eq!(field("summary_attempted_path"), Some("warm"));
            assert_eq!(field("summary_terminal"), Some("timeout"));
            assert!(event.contains_key("summary_id"), "{event:?}");
        }
        assert_eq!(mock.warm_requests(), 1, "cancel={cancel_it}");
        assert_eq!(mock.cold_requests(), 0, "cancel={cancel_it}: no cold retry");
        if cancel_it {
            assert!(second.heads.is_empty(), "no head after a cancel");
            assert_eq!(mock.requests.lock().unwrap().len(), 2, "no further request");
        } else {
            assert_eq!(second.heads.len(), 1);
            assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
            assert!(second.errors.is_empty(), "{:?}", second.errors);
            assert_eq!(mock.requests.lock().unwrap().len(), 3, "the successor proceeds");
        }
    }
}

/// Runs one summary of turn 1's second message through `summarize()` on
/// `warm`, bounded by `timeout` and `cancel`.
async fn summarize_bounded(
    rt: &crate::Runtime,
    first: &Turn,
    warm: ws::WarmSummarizer,
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> crate::runtime::rolling_summary::Summarized {
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let archive = "a".repeat(32);
    let dropped = vec![first.history[1].clone()];
    let indices = [1usize];
    let state = rt.continuation.clone();
    let logical_id = state.lock().unwrap().logical_id.clone();
    let messages = first.history.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        crate::runtime::rolling_summary::summarize(
            &summarizer,
            crate::runtime::rolling_summary::SummaryRequest {
                logical_id: &logical_id,
                previous: None,
                dropped: &dropped,
                window: 1,
                archive_id: &archive,
                cap_tokens: SUMMARY,
                timeout,
                cancel,
                accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                    usage: &usage,
                    max_cost_usd: None,
                    events: &events,
                    tap: None,
                    ledger: None,
                }),
                snapshot: Some(ws::SnapshotGate::new(&state)),
                run_deadline: None,
            },
            Some(ws::WarmContext {
                warm: &warm,
                state: &state,
                messages: &messages,
                dropped: &indices,
                newest_run_start: None,
            }),
        ),
    )
    .await
    .expect("the summary is bounded")
}

/// A warm attempt that times out or is canceled before its body is
/// published sent nothing: it is reported as no attempt (`none`), with the
/// terminal saying why and no correlation id. Here the call never gets
/// past its preparation.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn an_unsent_warm_attempt_that_times_out_or_is_canceled_is_not_reported_as_sent() {
    use crate::runtime::rolling_summary::SummaryPath;
    for cancel_it in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let rt = runtime(Some(SUMMARY));
        let (first, _record) = warmed(&rt).await;
        let before = mock.requests.lock().unwrap().len();
        let mut warm = rt.warm_summarizer();
        warm.call = Arc::new(|_, _, _| Box::pin(std::future::pending::<ws::WarmReply>()));
        let cancel = tokio_util::sync::CancellationToken::new();
        if cancel_it {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                cancel.cancel();
            });
        }
        let timeout = Duration::from_secs(if cancel_it { 20 } else { 1 });
        let out = summarize_bounded(&rt, &first, warm, timeout, &cancel).await;
        assert!(matches!(out.text, Err(None)), "cancel={cancel_it}: {:?}", out.text);
        assert_eq!(out.report.path, SummaryPath::None, "cancel={cancel_it}");
        assert_eq!(out.report.id, None, "cancel={cancel_it}: no id");
        let expected = if cancel_it { "canceled" } else { "timeout" };
        assert_eq!(out.report.terminal, Some(expected), "cancel={cancel_it}");
        assert_eq!(mock.requests.lock().unwrap().len(), before, "cancel={cancel_it}");
    }
}

/// The same through the real transport: a warm request whose connection
/// stalls (the listener's accept queue is full) and that times out there
/// published no body, so it is reported as no attempt and has no id.
#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_request_timing_out_on_a_stalled_connect_is_not_reported_as_sent() {
    use crate::runtime::rolling_summary::SummaryPath;
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let upstream = std::env::var("SYNAPS_ANTHROPIC_BASE_URL")
        .unwrap()
        .trim_start_matches("http://")
        .to_owned();
    let listener = Arc::new(one_slot_listener());
    let addr = listener.local_addr().unwrap();
    let forwarding = {
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut inbound, _)) = listener.accept().await {
                let upstream = upstream.clone();
                connections.spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        })
    };
    let _proxy =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    forwarding.abort();
    let _ = forwarding.await;
    let _filler = fill_queue(addr).await;
    let before = mock.requests.lock().unwrap().len();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (out, reached) = tokio::join!(
        summarize_bounded(
            &rt,
            &first,
            rt.warm_summarizer(),
            Duration::from_secs(2),
            &cancel
        ),
        connect_reached(addr.port())
    );
    assert!(reached, "the request reached its connect and stalled there");
    assert!(matches!(out.text, Err(None)), "{:?}", out.text);
    assert_eq!(out.report.path, SummaryPath::None);
    assert_eq!(out.report.id, None, "no id for a request that was not sent");
    assert_eq!(out.report.terminal, Some("timeout"));
    assert_eq!(mock.requests.lock().unwrap().len(), before);
}

/// The bytes after the request head (`\r\n\r\n`) on one connection.
fn body_bytes(received: &[u8]) -> usize {
    received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(0, |head| received.len() - head - 4)
}

/// The timeout reporter and the body stream decide once whether the warm
/// body went out. The HTTP client polls the body on its own connection
/// task, which can still hold it after the timed-out call was dropped; the
/// seam blocks that task inside the body poll (on another worker thread)
/// while the summary's timeout fires and the reporter decides.
///
/// (a) Held immediately before the publication compare-exchange: the
///     reporter withdraws the body, so once released no body byte reaches
///     the listener; reported as no attempt (`none`), without an id.
/// (b) Held after a won publication, immediately before the body is
///     yielded: the reporter loses, so the attempt is reported as sent
///     (`warm`) with its id, and the body does go out.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_timeout_reporter_and_the_body_decide_publication_once() {
    use crate::runtime::keepwarm::{PublicationSeam, SeamPoint};
    use crate::runtime::rolling_summary::SummaryPath;
    use tokio::io::AsyncReadExt;
    for at in [SeamPoint::BeforePublish, SeamPoint::BeforeYield] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let upstream = std::env::var("SYNAPS_ANTHROPIC_BASE_URL")
            .unwrap()
            .trim_start_matches("http://")
            .to_owned();
        let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
        let addr = listener.local_addr().unwrap();
        // Turn 1 is forwarded to the mock, which records the warm request.
        let forwarding = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                while let Ok((mut inbound, _)) = listener.accept().await {
                    let upstream = upstream.clone();
                    connections.spawn(async move {
                        if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                    });
                }
            })
        };
        let _proxy = crate::test_env::EnvVarGuard::set(
            "SYNAPS_ANTHROPIC_BASE_URL",
            &format!("http://{addr}"),
        );
        let rt = runtime(Some(SUMMARY));
        let (first, _record) = warmed(&rt).await;
        forwarding.abort();
        let _ = forwarding.await;
        // The summary's connection: recorded, never answered.
        let recorder = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move {
                let (mut inbound, _) = listener.accept().await.expect("the summary connects");
                let mut received = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match tokio::time::timeout(Duration::from_secs(10), inbound.read(&mut buf)).await
                    {
                        Ok(Ok(n)) if n > 0 => received.extend_from_slice(&buf[..n]),
                        _ => break received,
                    }
                }
            })
        };
        let (seam, control) = PublicationSeam::new(at);
        let mut warm = rt.warm_summarizer();
        let inner = Arc::clone(&warm.call);
        warm.call = Arc::new(move |record, send: ws::WarmSend, cancel| {
            send.published.install_seam(Arc::clone(&seam));
            inner(record, send, cancel)
        });
        let reached = control.reached;
        let cancel = tokio_util::sync::CancellationToken::new();
        let ((out, done_at), reached_at) = tokio::join!(
            async {
                let out =
                    summarize_bounded(&rt, &first, warm, Duration::from_secs(2), &cancel).await;
                (out, Instant::now())
            },
            async {
                tokio::task::spawn_blocking(move || {
                    reached.recv_timeout(Duration::from_secs(10)).ok().map(|()| Instant::now())
                })
                .await
                .unwrap()
            },
        );
        let reached_at = reached_at.unwrap_or_else(|| panic!("{at:?}: the body poll was held"));
        assert!(reached_at < done_at, "{at:?}: held before the timeout fired");
        // Let the held body stream go, then see what reached the listener.
        drop(control.release);
        let received = tokio::time::timeout(Duration::from_secs(20), recorder)
            .await
            .expect("the summary connection ends")
            .unwrap();
        assert!(matches!(out.text, Err(None)), "{at:?}: {:?}", out.text);
        assert_eq!(out.report.terminal, Some("timeout"), "{at:?}");
        match at {
            SeamPoint::BeforePublish => {
                assert_eq!(out.report.path, SummaryPath::None, "withdrawn: not sent");
                assert_eq!(out.report.id, None, "no id for a request that was not sent");
                assert_eq!(body_bytes(&received), 0, "no body byte after the withdrawal");
            }
            SeamPoint::BeforeYield => {
                assert_eq!(out.report.path, SummaryPath::Warm, "published first: sent");
                assert!(out.report.id.is_some(), "a sent request keeps its id");
                assert!(body_bytes(&received) > 0, "the published body went out");
            }
        }
        assert_eq!(mock.warm_requests(), 0, "{at:?}: the summary never reached the mock");
    }
}

/// One captured event: its message and fields.
type ContextEvent = (String, BTreeMap<String, String>);

/// `synaps::context` events captured on this thread: (message, fields).
#[derive(Clone, Default)]
struct ContextEvents(Arc<Mutex<Vec<ContextEvent>>>);

struct EventFields<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for EventFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ContextEvents {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // `synaps::context` events, and the summary's own log lines.
        let target = event.metadata().target();
        if target != "synaps::context"
            && !(target.ends_with("rolling_summary") && *event.metadata().level() <= tracing::Level::INFO)
        {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut EventFields(&mut fields));
        let message = fields.remove("message").unwrap_or_default();
        self.0.lock().unwrap().push((message, fields));
    }
}

impl ContextEvents {
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt;
        let events = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));
        (events, guard)
    }

    /// The fields of the one `context rollover` event.
    fn rollover(&self) -> BTreeMap<String, String> {
        let all: Vec<_> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(message, _)| message == "context rollover")
            .map(|(_, fields)| fields.clone())
            .collect();
        assert_eq!(all.len(), 1, "{all:?}");
        all.into_iter().next().unwrap()
    }

    /// The fields of every captured event with this message.
    fn logged(&self, message: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == message)
            .map(|(_, fields)| fields.clone())
            .collect()
    }
}

/// Summary-attempt keys that exist only when a request was attempted.
const ATTEMPT_KEYS: &[&str] = &[
    "summary_id",
    "summary_cold_reason",
    "summary_warm_settled_microusd",
    "summary_warm_miss",
    "summary_terminal",
    "summary_limit_tokens",
    "summary_cache_age_s",
    "summary_reserved_microusd",
    "summary_settled_microusd",
    "summary_duration_ms",
    "summary_error_status",
    "provider_summary_input_tokens",
    "provider_summary_output_tokens",
    "provider_summary_cache_read_tokens",
    "provider_summary_cache_write_tokens",
    "provider_summary_cache_write_5m_tokens",
    "provider_summary_cache_write_1h_tokens",
    "summary_cache_read_pct",
];

fn number(event: &BTreeMap<String, String>, key: &str) -> u64 {
    event
        .get(key)
        .unwrap_or_else(|| panic!("{key}: {event:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key}: {event:?}"))
}

/// Slice 8: a warm rollover's `context rollover` event says how its summary
/// was attempted and what the provider measured (no content); the warm
/// request's own telemetry record carries the same summary id, so it is
/// told apart from turns and keep-warm pings.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_rollover_logs_its_summary_attempt() {
    use crate::runtime::telemetry::{TelemetryLevel, TelemetryWriter, WriterOptions};
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let logs = tempfile::TempDir::new().unwrap();
    let log = logs.path().join("api-log.jsonl");
    let mut rt = runtime(Some(SUMMARY));
    rt.set_telemetry_level(TelemetryLevel::Basic);
    let writer = TelemetryWriter::new(WriterOptions {
        telemetry_path: Some(log.clone()),
        ..WriterOptions::default()
    });
    rt.telemetry_writer = Some(writer.clone());
    let (events, _guard) = ContextEvents::install();
    let (_first, second) = two_turns(&rt).await;
    assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-SUMMARY")));

    let event = events.rollover();
    let field = |key: &str| event.get(key).map(String::as_str);
    assert_eq!(field("summary"), Some("written"));
    assert_eq!(field("summary_reason"), Some("ok"));
    assert_eq!(field("summary_attempted_path"), Some("warm"));
    assert_eq!(field("summary_terminal"), Some("written"));
    assert_eq!(field("summary_warm_miss"), None);
    assert_eq!(number(&event, "provider_summary_input_tokens"), 60);
    assert_eq!(number(&event, "provider_summary_cache_read_tokens"), 45_000);
    assert_eq!(number(&event, "provider_summary_cache_write_tokens"), 0);
    assert_eq!(number(&event, "provider_summary_output_tokens"), 250);
    assert_eq!(field("provider_summary_cache_write_5m_tokens"), None);
    assert_eq!(field("provider_summary_cache_write_1h_tokens"), None);
    // floor(100 * 45000 / 45060)
    assert_eq!(number(&event, "summary_cache_read_pct"), 99);
    assert!(number(&event, "summary_limit_tokens") > SUMMARY);
    assert!(number(&event, "summary_cache_age_s") <= 3_600);
    let (reserved, settled) = (
        number(&event, "summary_reserved_microusd"),
        number(&event, "summary_settled_microusd"),
    );
    assert!(0 < settled && settled <= reserved, "{settled} {reserved}");
    let _ = number(&event, "summary_duration_ms");
    let id = field("summary_id").expect("summary id").to_owned();
    assert!(
        id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );

    // The warm request's own record carries the id; turn records do not.
    assert!(writer.shutdown(Duration::from_secs(5)).is_flushed());
    let records: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let tagged: Vec<&Value> = records
        .iter()
        .filter(|r| r.get("rollover_summary").is_some())
        .collect();
    assert_eq!(tagged.len(), 1, "{records:?}");
    assert_eq!(tagged[0]["rollover_summary"], id.as_str());
    assert!(tagged[0].get("keepwarm").is_none(), "not a keep-warm ping");
    assert!(records.len() > tagged.len(), "turn records are written untagged");
}

/// A summary request that was sent and failed says how in
/// `summary_error_status`: the HTTP status on the warm and the cold path,
/// `envelope` for a 200 whose body is an error. Never provider text; a
/// written summary has no such key.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_failed_summary_request_logs_its_error_status() {
    let rejection = json!({"type":"error","error":{"type":"invalid_request_error","message":"ECHOED-PROVIDER-TEXT"}}).to_string();
    // Warm: HTTP 400.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.fail.lock().unwrap() = Some((400, rejection.clone()));
        let rt = runtime(Some(SUMMARY));
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns(&rt).await;
        assert!(summary_of(&second.heads[0]).is_none());
        assert_eq!(mock.warm_requests(), 1);
        assert_eq!(mock.cold_requests(), 0, "no fallback after a sent request");
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        assert_eq!(field("summary_attempted_path"), Some("warm"));
        assert_eq!(field("summary_terminal"), Some("error"));
        assert_eq!(field("summary_error_status"), Some("400"));
        assert!(!format!("{event:?}").contains("ECHOED"), "{event:?}");
    }
    // Cold: HTTP 400, and a 200 whose body is an error envelope.
    for (status, expected) in [(400, "400"), (200, "envelope")] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let rt = runtime(Some(SUMMARY));
        set_thresholds(&rt, 300_000, 400_000);
        let first = turn(&rt, history(None), Default::default()).await;
        // The record's reply reported no cache read or write: cold.
        rt.continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded")
            .tap
            .completed(Some(ws::WarmUsage {
                input: 40_100,
                output: 5,
                ..ws::WarmUsage::default()
            }));
        *mock.fail.lock().unwrap() = Some((status, rejection.clone()));
        let (events, _guard) = ContextEvents::install();
        set_thresholds(&rt, 60_000, 80_000);
        let mut next = first.history.clone();
        next.push(msg(json!({"role":"user","content":"carry on"})));
        let second = turn(&rt, next, Default::default()).await;
        assert!(summary_of(&second.heads[0]).is_none());
        assert_eq!(mock.cold_requests(), 1);
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        assert_eq!(field("summary_attempted_path"), Some("cold"), "{status}");
        assert_eq!(field("summary_terminal"), Some("error"), "{status}");
        assert_eq!(field("summary_error_status"), Some(expected), "{status}");
        assert!(!format!("{event:?}").contains("ECHOED"), "{event:?}");
    }
    // Written: no error status.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (_mock, _g) = spawn_warm().await;
        let rt = runtime(Some(SUMMARY));
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns(&rt).await;
        assert!(summary_of(&second.heads[0]).is_some());
        let event = events.rollover();
        assert_eq!(event.get("summary_terminal").map(String::as_str), Some("written"));
        assert!(!event.contains_key("summary_error_status"), "{event:?}");
    }
}

/// Slice 8, missing data: the screened request reports measured input and
/// output only (its normalized usage zero-fills cache counts, so they are
/// left out, and with them the read share); why warm was not used is said;
/// a warm reply whose usage is partial reports no counters and no settled
/// cost; with summaries off no attempt key appears at all.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_rollover_logs_only_the_summary_data_it_has() {
    // Cold: the record's reply reported no cache read or write.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let rt = runtime(Some(SUMMARY));
        set_thresholds(&rt, 300_000, 400_000);
        let first = turn(&rt, history(None), Default::default()).await;
        rt.continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded")
            .tap
            .completed(Some(ws::WarmUsage {
                input: 40_100,
                output: 5,
                ..ws::WarmUsage::default()
            }));
        let (events, _guard) = ContextEvents::install();
        set_thresholds(&rt, 60_000, 80_000);
        let mut next = first.history.clone();
        next.push(msg(json!({"role":"user","content":"carry on"})));
        let second = turn(&rt, next, Default::default()).await;
        assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("COLD-SUMMARY")));
        assert_eq!(mock.cold_requests(), 1);
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        assert_eq!(field("summary"), Some("written"));
        assert_eq!(field("summary_attempted_path"), Some("cold"));
        assert_eq!(field("summary_warm_miss"), Some("no_cache_evidence"));
        assert_eq!(field("summary_terminal"), Some("written"));
        assert_eq!(number(&event, "provider_summary_input_tokens"), 30_000);
        assert_eq!(number(&event, "provider_summary_output_tokens"), 300);
        for key in [
            "provider_summary_cache_read_tokens",
            "provider_summary_cache_write_tokens",
            "provider_summary_cache_write_5m_tokens",
            "provider_summary_cache_write_1h_tokens",
            "summary_cache_read_pct",
            "summary_id",
            "summary_cache_age_s",
        ] {
            assert!(!event.contains_key(key), "cold: {key}: {event:?}");
        }
        let limit = number(&event, "summary_limit_tokens");
        assert!(0 < limit && limit <= SUMMARY, "the cold ceiling is the cap: {limit}");
        let settled = number(&event, "summary_settled_microusd");
        assert!(0 < settled && settled <= number(&event, "summary_reserved_microusd"));
        let _ = number(&event, "summary_duration_ms");
    }
    // Warm with partial usage: no counters, nothing settled, still held.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = WarmKind::PartialUsage;
        let rt = runtime(Some(SUMMARY));
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns(&rt).await;
        assert!(summary_of(&second.heads[0]).is_some_and(|s| s.contains("WARM-PARTIAL")));
        let event = events.rollover();
        assert_eq!(event.get("summary_attempted_path").map(String::as_str), Some("warm"));
        assert_eq!(event.get("summary_terminal").map(String::as_str), Some("written"));
        for key in [
            "provider_summary_input_tokens",
            "provider_summary_output_tokens",
            "provider_summary_cache_read_tokens",
            "provider_summary_cache_write_tokens",
            "summary_cache_read_pct",
            "summary_settled_microusd",
        ] {
            assert!(!event.contains_key(key), "partial: {key}: {event:?}");
        }
        for key in ["summary_id", "summary_reserved_microusd", "summary_duration_ms"] {
            assert!(event.contains_key(key), "partial: {key}: {event:?}");
        }
    }
    // Summaries off: no attempt, no attempt keys.
    {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        let rt = runtime(Some(0));
        let (events, _guard) = ContextEvents::install();
        let (_first, _second) = two_turns(&rt).await;
        assert_eq!(mock.warm_requests() + mock.cold_requests(), 0);
        let event = events.rollover();
        assert_eq!(event.get("summary_attempted_path").map(String::as_str), Some("none"));
        assert_eq!(event.get("summary_reason").map(String::as_str), Some("disabled"));
        for key in ATTEMPT_KEYS {
            assert!(!event.contains_key(*key), "off: {key}: {event:?}");
        }
    }
}

// ── Refused and failed warm replies ─────────────────────────────────────

/// A warm request that went out and failed: its reply.
async fn warm_reply_for(kind: WarmKind, idle: Option<Duration>) -> ws::WarmReply {
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    *mock.kind.lock().unwrap() = kind;
    let send = send_for(&record);
    let call = rt.warm_summary_call(record, send, Default::default());
    match idle {
        Some(idle) => {
            crate::runtime::http_phases::STREAM_IDLE_FOR_TEST
                .scope(idle, call)
                .await
        }
        None => call.await,
    }
}

/// A reply that ends with stop reason `refusal` is a refusal (the transport
/// turns it into an error once its refusal budget, zero here, is spent),
/// with or without partial text before it; its usage is still reported.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_refused_warm_reply_is_a_refusal_not_an_error() {
    for kind in [WarmKind::Refusal, WarmKind::RefusalAfterText] {
        let _base = crate::test_env::BaseDirGuard::new();
        let reply = warm_reply_for(kind, None).await;
        assert_eq!(reply.terminal, ws::WarmTerminal::Refusal, "{kind:?}: {reply:?}");
        assert!(reply.dispatched, "{kind:?}");
        assert_eq!(reply.failure, None, "{kind:?}");
        let usage = reply.usage.expect("usage reported in full");
        assert_eq!((usage.input, usage.output, usage.cache_read), (60, 250, 45_000));
    }
}

/// Every way a sent warm request fails after a 200 names its kind with a
/// static label (never provider text): an in-stream error frame (its vetted
/// type, else `unrecognized_error`), a broken body, a body that goes
/// quiet, and a stream with no content and no stop reason. An HTTP error
/// status names the vetted type of its body.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_failed_warm_request_names_how_it_failed() {
    use crate::runtime::api_sync::FailureClass;
    for (kind, idle, label) in [
        (WarmKind::SseError("overloaded_error"), None, "overloaded_error"),
        (WarmKind::SseError("mystery_error"), None, "unrecognized_error"),
        // An error frame after a refusal stop is the stream error, not a
        // refusal.
        (WarmKind::RefusalThenError("overloaded_error"), None, "overloaded_error"),
        (WarmKind::BrokenBody, None, "transport"),
        (
            WarmKind::IdleStall,
            Some(Duration::from_millis(300)),
            "idle_timeout",
        ),
        (WarmKind::NoStop, None, "empty_response"),
    ] {
        let _base = crate::test_env::BaseDirGuard::new();
        let reply = warm_reply_for(kind, idle).await;
        assert_eq!(reply.terminal, ws::WarmTerminal::Error, "{kind:?}: {reply:?}");
        assert!(reply.dispatched, "{kind:?}");
        let failure = reply.failure.expect("a sent failure");
        assert_eq!(failure.class, FailureClass::Stream, "{kind:?}");
        assert_eq!(failure.error_type, Some(label), "{kind:?}");
    }
    // An HTTP error status.
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (_first, record) = warmed(&rt).await;
    let overloaded =
        json!({"type":"error","error":{"type":"overloaded_error","message":"ECHOED-PROVIDER-TEXT"}});
    *mock.fail.lock().unwrap() = Some((529, overloaded.to_string()));
    let send = send_for(&record);
    let reply = rt.warm_summary_call(record, send, Default::default()).await;
    assert_eq!(reply.terminal, ws::WarmTerminal::Error);
    let failure = reply.failure.expect("a sent failure");
    assert_eq!(failure.class, FailureClass::Status(529));
    assert_eq!(failure.error_type, Some("overloaded_error"));
}

/// A rollover whose warm request failed after a 200 logs the failure's
/// kind; no screened attempt follows (only a refusal gets one), also when
/// the error frame came after a refusal stop.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_failed_warm_rollover_logs_the_failure_kind() {
    for kind in [
        WarmKind::SseError("overloaded_error"),
        WarmKind::RefusalThenError("overloaded_error"),
    ] {
        a_failed_warm_rollover_logs(kind).await;
    }
}

async fn a_failed_warm_rollover_logs(kind: WarmKind) {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = kind;
    let rt = runtime(Some(SUMMARY));
    let (events, _guard) = ContextEvents::install();
    let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
    assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
    assert_eq!(mock.warm_requests(), 1);
    assert_eq!(mock.cold_requests(), 0, "{kind:?}: no screened attempt after an error");
    let event = events.rollover();
    let field = |key: &str| event.get(key).map(String::as_str);
    assert_eq!(field("summary_attempted_path"), Some("warm"));
    assert_eq!(field("summary_terminal"), Some("error"), "{kind:?}");
    assert_eq!(field("summary_error_status"), Some("stream"));
    assert_eq!(field("summary_cold_reason"), None);
    let failed = events.logged("rollover summary request failed; previous summary carried forward");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].get("status").map(String::as_str), Some("stream"));
    assert_eq!(failed[0].get("error_type").map(String::as_str), Some("overloaded_error"));
    assert!(!format!("{:?}", events.0.lock().unwrap()).contains("ECHOED"));
}

/// A warm refusal gets one screened (cold) attempt through the usual cold
/// path, with its own reservation and booking; the rollover line says
/// why the cold request ran and keeps the warm request's id and cost.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_refusal_gets_one_screened_attempt() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = WarmKind::RefusalAfterText;
    let rt = runtime(Some(SUMMARY));
    let (events, _guard) = ContextEvents::install();
    let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
    let summary = summary_of(&second.heads[0]).expect("a summary");
    assert!(summary.contains("COLD-SUMMARY"), "{summary}");
    assert_eq!(mock.warm_requests(), 1);
    assert_eq!(mock.cold_requests(), 1, "exactly one screened attempt");
    let history = serde_json::to_string(&second.history).unwrap();
    let head = serde_json::to_string(&second.heads).unwrap();
    assert!(!history.contains("WARM-PARTIAL") && !head.contains("WARM-PARTIAL"));
    // Both requests are booked once.
    let booked = |i: u64, o: u64| second.usages.iter().filter(|u| u.0 == i && u.1 == o).count();
    assert_eq!((booked(60, 250), booked(30_000, 300)), (1, 1), "{:?}", second.usages);
    let event = events.rollover();
    let field = |key: &str| event.get(key).map(String::as_str);
    assert_eq!(field("summary"), Some("written"));
    assert_eq!(field("summary_reason"), Some("ok"));
    assert_eq!(field("summary_attempted_path"), Some("cold"));
    assert_eq!(field("summary_cold_reason"), Some("warm_refusal"));
    assert_eq!(field("summary_terminal"), Some("written"));
    assert_eq!(field("summary_warm_miss"), None);
    assert!(field("summary_id").is_some(), "the warm request's id: {event:?}");
    assert!(number(&event, "summary_warm_settled_microusd") > 0);
    let settled = number(&event, "summary_settled_microusd");
    assert!(0 < settled && settled <= number(&event, "summary_reserved_microusd"));
    assert_eq!(number(&event, "provider_summary_input_tokens"), 30_000);
}

/// A screened retry that is refused too carries the previous summary
/// forward: two requests in all, and none of the refused text is used.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_refused_screened_retry_carries_the_previous_summary() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.kind.lock().unwrap() = WarmKind::Refusal;
    *mock.cold_text.lock().unwrap() = Some("## Window 1\nCOLD-PARTIAL-REFUSED".into());
    *mock.cold_stop.lock().unwrap() = Some("refusal");
    let rt = runtime(Some(SUMMARY));
    let (events, _guard) = ContextEvents::install();
    let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
    assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (1, 1));
    let head = serde_json::to_string(&second.heads).unwrap();
    assert!(!head.contains("COLD-PARTIAL"), "{head}");
    let event = events.rollover();
    let field = |key: &str| event.get(key).map(String::as_str);
    assert_eq!(field("summary"), Some("carried_forward"));
    assert_eq!(field("summary_reason"), Some("failed"));
    assert_eq!(field("summary_attempted_path"), Some("cold"));
    assert_eq!(field("summary_cold_reason"), Some("warm_refusal"));
    assert_eq!(field("summary_terminal"), Some("refusal"));
}

/// The screened attempt after a refusal runs only while enough of the
/// summary timeout is left (it gets what is left as its own limit); a
/// shorter configured timeout or a slow refusal leaves none.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_refusal_without_time_left_gets_no_screened_attempt() {
    // (configured timeout, kind, cold attempts)
    for (timeout, kind, cold) in [
        (60, WarmKind::Refusal, 0),
        (92, WarmKind::SlowRefusal, 0),
        (95, WarmKind::Refusal, 1),
    ] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (mock, _g) = spawn_warm().await;
        *mock.kind.lock().unwrap() = kind;
        let rt = runtime(Some(SUMMARY));
        rt.continuation.lock().unwrap().config.summary_timeout_secs = timeout;
        let (events, _guard) = ContextEvents::install();
        let (_first, second) = two_turns_from(&rt, Some((PREVIOUS, 3))).await;
        assert_eq!(mock.warm_requests(), 1, "{timeout} {kind:?}");
        assert_eq!(mock.cold_requests(), cold, "{timeout} {kind:?}");
        let event = events.rollover();
        let field = |key: &str| event.get(key).map(String::as_str);
        if cold == 0 {
            assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
            assert_eq!(field("summary"), Some("carried_forward"));
            assert_eq!(field("summary_attempted_path"), Some("warm"));
            assert_eq!(field("summary_terminal"), Some("refusal"));
            assert_eq!(field("summary_cold_reason"), None, "{event:?}");
            let rejected = events.logged("warm rollover summary rejected");
            assert_eq!(rejected.len(), 1);
            assert_eq!(rejected[0].get("stop_reason").map(String::as_str), Some("refusal"));
        } else {
            assert_eq!(field("summary_cold_reason"), Some("warm_refusal"));
            assert_eq!(field("summary"), Some("written"));
        }
    }
}

/// On the ordinary screened path a refused reply is not a summary, even
/// with text before the refusal: the previous one is carried forward.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_refused_screened_summary_is_not_written() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (mock, _g) = spawn_warm().await;
    *mock.cold_text.lock().unwrap() = Some("## Window 1\nCOLD-PARTIAL-REFUSED".into());
    *mock.cold_stop.lock().unwrap() = Some("refusal");
    let rt = runtime(Some(SUMMARY));
    set_thresholds(&rt, 300_000, 400_000);
    let first = turn(&rt, history(Some((PREVIOUS, 3))), Default::default()).await;
    // The record's reply reported no cache read or write: cold.
    rt.continuation
        .lock()
        .unwrap()
        .warm_record()
        .expect("recorded")
        .tap
        .completed(Some(ws::WarmUsage {
            input: 40_100,
            output: 5,
            ..ws::WarmUsage::default()
        }));
    let (events, _guard) = ContextEvents::install();
    set_thresholds(&rt, 60_000, 80_000);
    let mut next = first.history.clone();
    next.push(msg(json!({"role":"user","content":"carry on"})));
    let second = turn(&rt, next, Default::default()).await;
    assert_eq!(summary_of(&second.heads[0]).as_deref(), Some(PREVIOUS));
    assert_eq!((mock.warm_requests(), mock.cold_requests()), (0, 1));
    let event = events.rollover();
    let field = |key: &str| event.get(key).map(String::as_str);
    assert_eq!(field("summary"), Some("carried_forward"));
    assert_eq!(field("summary_attempted_path"), Some("cold"));
    assert_eq!(field("summary_terminal"), Some("refusal"));
    assert_eq!(field("summary_cold_reason"), None);
    // Its usage is still booked.
    assert_eq!(number(&event, "provider_summary_output_tokens"), 300);
}

// ── One deadline for the whole summary stage (paused time) ─────────────────

/// The session's summarizer with timed fakes and no network: the warm
/// request is refused after `warm_after`, the seat probe answers after
/// `probe`, and the screened request (counted in `cold_calls`) answers
/// after `cold_after`.
fn timed_summarizer(
    rt: &crate::Runtime,
    warm_after: Duration,
    probe: Duration,
    cold_after: Duration,
    cold_calls: &Arc<std::sync::atomic::AtomicUsize>,
) -> super::super::RolloverSummarizer {
    let mut summarizer = rt.rollover_summarizer().expect("summarizer");
    summarizer.seat_bound = Some(Arc::new(move || {
        Box::pin(async move {
            tokio::time::sleep(probe).await;
            true
        })
    }));
    let calls = Arc::clone(cold_calls);
    summarizer.call = Arc::new(move |_messages, _limit, _gate| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(cold_after).await;
            super::super::SummaryReply {
                text: Ok("## Window 1\nCOLD-LATE".into()),
                spend: super::super::Spend::Unknown,
                failure: None,
                stop: None,
            }
        })
    });
    let warm = summarizer.warm.as_mut().expect("the warm path is configured");
    warm.call = Arc::new(move |_record, _send, _cancel| {
        Box::pin(async move {
            tokio::time::sleep(warm_after).await;
            ws::WarmReply {
                terminal: ws::WarmTerminal::Refusal,
                usage: None,
                dispatched: true,
                failure: None,
                stop: Some(crate::runtime::trace::StopReason::Refusal),
            }
        })
    });
    summarizer
}

/// tokio's timers have millisecond granularity: a deadline fires at the
/// first timer tick at or after it, so under paused time a stage that ends at
/// its deadline `d` reports between `d` and `d` plus one tick.
const TIMER_TICK: Duration = Duration::from_millis(1);

/// `took` is the deadline `want`, give or take the timer tick above.
fn assert_ends_at(took: Duration, want: Duration, what: &str) {
    assert!(
        took >= want && took <= want + TIMER_TICK,
        "{what}: ended {took:?} after the start, want {want:?} (+ at most one {TIMER_TICK:?} timer tick)"
    );
}

/// One summary of turn 1's second message through `summarize()` (warm
/// first when `warm`), with `timeout`; returns it and how long it took.
async fn summarize_timed(
    rt: &crate::Runtime,
    first: &Turn,
    summarizer: &super::super::RolloverSummarizer,
    warm: bool,
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> (super::super::Summarized, Duration) {
    let usage = crate::runtime::budget::UsageCounters::default();
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let archive = "a".repeat(32);
    let dropped = vec![first.history[1].clone()];
    let indices = [1usize];
    let state = rt.continuation.clone();
    let logical_id = state.lock().unwrap().logical_id.clone();
    let messages = first.history.clone();
    let started = tokio::time::Instant::now();
    let out = crate::runtime::rolling_summary::summarize(
        summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &logical_id,
            previous: Some(PREVIOUS),
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout,
            cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage: &usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(ws::SnapshotGate::new(&state)),
            run_deadline: None,
        },
        summarizer
            .warm()
            .filter(|_| warm)
            .map(|warm| ws::WarmContext {
                warm,
                state: &state,
                messages: &messages,
                dropped: &indices,
                newest_run_start: None,
            }),
    )
    .await;
    (out, started.elapsed())
}

/// After a warm refusal with enough time left, a seat probe slow enough to
/// eat the screened attempt's floor: the floor is rechecked just before
/// sending, so no screened request is made and the warm refusal stands.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_slow_seat_probe_that_eats_the_floor_leaves_no_screened_attempt() {
    use crate::runtime::rolling_summary::SummaryPath;
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    tokio::time::pause();
    let cold = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let summarizer = timed_summarizer(
        &rt,
        Duration::ZERO,
        Duration::from_secs(20),
        Duration::from_secs(1),
        &cold,
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let (out, took) =
        summarize_timed(&rt, &first, &summarizer, true, Duration::from_secs(100), &cancel).await;
    assert_eq!(cold.load(std::sync::atomic::Ordering::SeqCst), 0, "no screened request");
    assert!(matches!(out.text, Err(None)), "{:?}", out.text);
    assert_eq!(out.report.path, SummaryPath::Warm, "{:?}", out.report);
    assert_eq!(out.report.terminal, Some("refusal"));
    assert_eq!(out.report.cold_reason, None);
    assert!(took <= Duration::from_secs(100), "{took:?}");
}

/// The two-attempt route ends by the stage's deadline (its start plus the
/// summary timeout): the warm refusal and the seat probe use part of it,
/// and the screened request gets only the rest, not a fresh allowance.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn the_two_attempt_route_never_ends_past_the_deadline() {
    use crate::runtime::rolling_summary::SummaryPath;
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    tokio::time::pause();
    let cold = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let summarizer = timed_summarizer(
        &rt,
        Duration::from_secs(5),
        Duration::from_secs(3),
        Duration::from_secs(1_000),
        &cold,
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let timeout = Duration::from_secs(100);
    let (out, took) = summarize_timed(&rt, &first, &summarizer, true, timeout, &cancel).await;
    assert_eq!(cold.load(std::sync::atomic::Ordering::SeqCst), 1, "one screened request");
    assert!(
        took <= timeout + TIMER_TICK,
        "ended {took:?} after the start, past the {timeout:?} deadline (+ one timer tick)"
    );
    assert!(matches!(out.text, Err(None)), "{:?}", out.text);
    assert_eq!(out.report.path, SummaryPath::Cold, "{:?}", out.report);
    assert_eq!(out.report.terminal, Some("timeout"));
    assert_eq!(out.report.cold_reason, Some("warm_refusal"));
}

/// On the ordinary cold path the seat probe is inside the deadline and the
/// cancel too: a probe that outlasts the timeout ends the summary at the
/// deadline, a cancel ends it at once; neither sends anything.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_slow_seat_probe_ends_at_the_deadline_or_the_cancel() {
    use crate::runtime::rolling_summary::SummaryPath;
    let _base = crate::test_env::BaseDirGuard::new();
    let (_mock, _g) = spawn_warm().await;
    let rt = runtime(Some(SUMMARY));
    let (first, _record) = warmed(&rt).await;
    tokio::time::pause();
    let timeout = Duration::from_secs(100);
    for cancel_after in [None, Some(Duration::from_secs(10))] {
        let cold = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let summarizer = timed_summarizer(
            &rt,
            Duration::ZERO,
            Duration::from_secs(500),
            Duration::from_secs(1),
            &cold,
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        if let Some(after) = cancel_after {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                cancel.cancel();
            });
        }
        let (out, took) =
            summarize_timed(&rt, &first, &summarizer, false, timeout, &cancel).await;
        assert_eq!(cold.load(std::sync::atomic::Ordering::SeqCst), 0, "{cancel_after:?}");
        assert!(matches!(out.text, Err(None)), "{cancel_after:?}: {:?}", out.text);
        assert_eq!(out.report.path, SummaryPath::None, "{cancel_after:?}");
        match cancel_after {
            None => {
                assert_ends_at(took, timeout, "at the deadline");
                assert_eq!(out.report.terminal, Some("timeout"));
            }
            Some(after) => {
                assert_ends_at(took, after, "at the cancel");
                assert_eq!(out.report.terminal, Some("canceled"));
            }
        }
    }
}
