//! `SessionQuery::CacheExpiry` through the real session actor
//! (arch-review-435-436-437.md Contract v3 §3, v3.1): what the session's
//! cache anchor cell (its last accepted request, the one keep-warm and
//! compaction read) shows for the next request, the states decided at ask,
//! the bounded job (supersession, routing, the checks at `finish`), and the
//! races of §3.10.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::cache_expiry_testing::Hold;
use agent_engine::session::{
    decode_query_reply, CacheExpiry, CacheReason, CacheState, ClientKind, ClientMeta,
    ClientTransport, Envelope, LocalTransport, QueryError, QueryErrorCode, SessionCommand,
    SessionEventWire, SessionHandle, SessionQuery, SessionSetting,
};
use axum::response::IntoResponse;
use serial_test::serial;

const PATIENCE: Duration = Duration::from_secs(30);
const MODEL_B: &str = "claude-opus-4-6";
const PING_MARK: &str = "prompt-cache keep-warm ping";
const NO_KEEPWARM_1H: &str = "cache_keepwarm = off\ncache_ttl = 1h\nthinking = off\n";
const KEEPWARM_1H: &str = "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n";

/// A streamed answer: `usage` is the message_start/message_delta usage
/// JSON (or `None`: no usage at all), then `content` blocks' events.
macro_rules! sse {
    ($usage:expr, $stop:expr, $blocks:expr) => {
        concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ce\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null", $usage, "}}\n\n",
            $blocks,
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"", $stop, "\",",
            "\"stop_sequence\":null}", $usage, "}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
    };
}

macro_rules! text_block {
    () => {
        concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        )
    };
}

/// Read 18k, wrote 2k with the 1h TTL: 20k tokens of cache activity.
const SSE_1H: &str = sse!(
    concat!(
        ",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_creation_input_tokens\":2000,",
        "\"cache_read_input_tokens\":18000,\"cache_creation\":{\"ephemeral_5m_input_tokens\":0,",
        "\"ephemeral_1h_input_tokens\":2000}}"
    ),
    "end_turn",
    text_block!()
);
/// A 1h request whose write came back 5m only (a downgrade).
const SSE_5M: &str = sse!(
    concat!(
        ",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_creation_input_tokens\":2000,",
        "\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":2000,",
        "\"ephemeral_1h_input_tokens\":0}}"
    ),
    "end_turn",
    text_block!()
);
/// Read 20k, wrote nothing.
const SSE_READ: &str = sse!(
    ",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":20000}",
    "end_turn",
    text_block!()
);
/// Read and wrote nothing.
const SSE_UNCACHED: &str = sse!(
    ",\"usage\":{\"input_tokens\":20010,\"output_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}",
    "end_turn",
    text_block!()
);
/// A 200 with no usage at all.
const SSE_NO_USAGE: &str = sse!("", "end_turn", text_block!());
/// A 1h write, and a call of [`GateTool`].
const SSE_TOOL: &str = sse!(
    concat!(
        ",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"cache_creation_input_tokens\":2000,",
        "\"cache_read_input_tokens\":18000,\"cache_creation\":{\"ephemeral_5m_input_tokens\":0,",
        "\"ephemeral_1h_input_tokens\":2000}}"
    ),
    "tool_use",
    concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_gate\",\"name\":\"cache_gate\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    )
);

/// How the stub answers a kind of request.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reply {
    OneHour,
    FiveMin,
    Read,
    Uncached,
    NoUsage,
    /// HTTP 400 (not retried): nothing accepted.
    Refused,
    /// `OneHour`, its answer held until [`Script::release`].
    Held,
    /// `SSE_TOOL`.
    Tool,
}

/// What the stub answers turns and pings with, settable mid-test.
#[derive(Clone)]
struct Script(Arc<Mutex<[Reply; 2]>>, Arc<tokio::sync::Notify>);

impl Script {
    fn new() -> Self {
        Self(
            Arc::new(Mutex::new([Reply::OneHour, Reply::Read])),
            Arc::new(tokio::sync::Notify::new()),
        )
    }
    fn release(&self) {
        self.1.notify_one();
    }
    fn turns(&self, r: Reply) {
        self.0.lock().unwrap()[0] = r;
    }
    fn pings(&self, r: Reply) {
        self.0.lock().unwrap()[1] = r;
    }
}

async fn scripted_stub(script: Script) -> (String, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: String| {
        let (b, script) = (Arc::clone(&b), script.clone());
        async move {
            let ping = body.contains(PING_MARK);
            b.lock().unwrap().push(body);
            let [turns, pings] = *script.0.lock().unwrap();
            let reply = if ping { pings } else { turns };
            let sse = match reply {
                Reply::Refused => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"type": "error", "error": {
                            "type": "invalid_request_error", "message": "synthetic refusal"}})),
                    )
                        .into_response()
                }
                Reply::Held => {
                    script.1.notified().await;
                    SSE_1H
                }
                Reply::OneHour => SSE_1H,
                Reply::FiveMin => SSE_5M,
                Reply::Read => SSE_READ,
                Reply::Uncached => SSE_UNCACHED,
                Reply::NoUsage => SSE_NO_USAGE,
                Reply::Tool => SSE_TOOL,
            };
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                sse.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bodies)
}

/// Holds its turn at the tool: says it started, then waits to be let go.
struct GateTool {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl agent_engine::Tool for GateTool {
    fn name(&self) -> &str {
        "cache_gate"
    }
    fn description(&self) -> &str {
        "waits to be let go"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        self.started.notify_one();
        self.release.notified().await;
        Ok("let go".into())
    }
}

struct Fixture {
    _h: Home,
    script: Script,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: SessionHandle,
    t: LocalTransport,
    host: Arc<agent_engine::EngineHost>,
}

async fn stub_host(
    config: &str,
) -> (
    Home,
    Script,
    Arc<Mutex<Vec<String>>>,
    Arc<agent_engine::EngineHost>,
) {
    let h = Home::new();
    std::fs::write(h.dir.path().join(".synaps-cli").join("config"), config).unwrap();
    let script = Script::new();
    let (url, bodies) = scripted_stub(script.clone()).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    (h, script, bodies, host().await)
}

async fn scripted(config: &str) -> Fixture {
    let (_h, script, bodies, host) = stub_host(config).await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    Fixture {
        _h,
        script,
        bodies,
        handle,
        t,
        host,
    }
}

/// [`scripted`] with [`GateTool`] registered: its "started" and "let go".
async fn scripted_gate(
    config: &str,
) -> (Fixture, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let (_h, script, bodies, host) = stub_host(config).await;
    let (started, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(GateTool {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }));
    let handle = host.create_session(cfg()).await.unwrap();
    let (t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    (
        Fixture {
            _h,
            script,
            bodies,
            handle,
            t,
            host,
        },
        started,
        release,
    )
}

async fn turn(t: &mut LocalTransport, text: &str) {
    t.send(submit(text)).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

async fn ask(t: &mut LocalTransport, id: u64) {
    t.send_from_self(SessionCommand::Query {
        id,
        query: SessionQuery::CacheExpiry,
    })
    .await
    .unwrap();
}

/// Every reply `t` gets under `id`, up to the first.
async fn reply(t: &mut LocalTransport, id: u64) -> Result<CacheExpiry, QueryError> {
    let seen = until(
        t,
        |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id),
    )
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => decode_query_reply::<CacheExpiry>(value),
        _ => unreachable!(),
    }
}

/// The answer, asking again on `stale`.
async fn expiry(t: &mut LocalTransport, id: u64) -> CacheExpiry {
    for attempt in 0..3 {
        ask(t, id + attempt * 1000).await;
        match reply(t, id + attempt * 1000).await {
            Ok(c) => return c,
            Err(e) if e.code == QueryErrorCode::Stale => continue,
            Err(e) => panic!("{e}"),
        }
    }
    panic!("stale three times")
}

fn says(c: &CacheExpiry) -> (CacheState, Option<CacheReason>) {
    (c.state, c.reason)
}

const WARM: (CacheState, Option<CacheReason>) = (CacheState::Warm, None);

fn unknown(r: CacheReason) -> (CacheState, Option<CacheReason>) {
    (CacheState::Unknown, Some(r))
}

async fn set(t: &mut LocalTransport, id: u64, setting: SessionSetting) {
    t.send_from_self(SessionCommand::Set { id, setting })
        .await
        .unwrap();
    let seen = until(
        t,
        |e| matches!(e, SessionEventWire::SettingChanged(a) if a.id == id),
    )
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::SettingChanged(a) => assert!(a.ok, "{:?}", a.message),
        _ => unreachable!(),
    }
}

async fn set_model(t: &mut LocalTransport, id: u64, model: &str) {
    set(
        t,
        id,
        SessionSetting::Model {
            model: model.into(),
        },
    )
    .await;
}

fn pings(bodies: &Arc<Mutex<Vec<String>>>) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.contains(PING_MARK))
        .count()
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One permit through a held job's `selected` gate.
fn let_one_through(hold: &Hold) {
    hold.selected.add_permits(1);
}

/// Idle pings a second apart: a job reads the cell well before the next.
fn fast_pings() -> agent_engine::session::cache_keepwarm::testing::IntervalOverride {
    agent_engine::session::cache_keepwarm::testing::IntervalOverride::set(Duration::from_millis(
        1000,
    ))
}

async fn reached(hold: &Hold, n: usize) {
    wait_for("the job's read", || hold.reached() >= n).await;
}

// ── what a session's own requests show ───────────────────────────────────

/// A turn's measured 1h request (its 1h write is the evidence for the
/// hour): warm for about the hour, the TTL and the request's cache activity
/// named; no keep-warm period, no ping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_measured_turn_is_warm_with_its_proof() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    let c = expiry(&mut f.t, 10).await;
    assert_eq!(says(&c), WARM, "{c:?}");
    assert!((3590..=3600).contains(&c.warm_for_secs.unwrap()), "{c:?}");
    assert!((3590..=3601).contains(&c.lapse_by_secs.unwrap()), "{c:?}");
    assert_eq!(
        (c.proof_ttl_secs, c.requested_ttl_secs, c.proof_cache_tokens),
        (Some(3600), Some(3600), Some(20000))
    );
    assert_eq!(c.next_ping_in_secs, None);
    end(&mut f.t).await;
}

/// A 1h request answered with a 5m write latches the downgrade: warm for
/// the 5m the server honours; the hour asked for bounds the band.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_downgraded_request_proves_five_minutes() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    f.script.turns(Reply::FiveMin);
    turn(&mut f.t, "hello").await;
    let c = expiry(&mut f.t, 20).await;
    assert_eq!(says(&c), WARM, "{c:?}");
    assert!(c.warm_for_secs.unwrap() <= 300, "{c:?}");
    assert_eq!(
        (c.proof_ttl_secs, c.requested_ttl_secs),
        (Some(300), Some(3600))
    );
    assert!(
        c.lapse_by_secs.unwrap() > 3500,
        "the hour it asked for: {c:?}"
    );
    end(&mut f.t).await;
}

/// Zero read and write: the last request is cold. No usage at all:
/// unmeasured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn uncached_and_unmeasured_requests() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    f.script.turns(Reply::Uncached);
    turn(&mut f.t, "hello").await;
    let c = expiry(&mut f.t, 30).await;
    assert_eq!(
        says(&c),
        (CacheState::Cold, Some(CacheReason::Uncached)),
        "{c:?}"
    );
    assert_eq!(c.lapse_by_secs, None);
    f.script.turns(Reply::NoUsage);
    turn(&mut f.t, "again").await;
    let c = expiry(&mut f.t, 31).await;
    assert_eq!(says(&c), unknown(CacheReason::Unmeasured), "{c:?}");
    end(&mut f.t).await;
}

/// The states decided at ask: an empty history has no matching request;
/// a running turn is busy; a turn that failed leaves the history unsettled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn states_decided_at_ask() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    let c = expiry(&mut f.t, 40).await;
    assert_eq!(says(&c), unknown(CacheReason::NoMatchingRequest), "{c:?}");
    f.script.turns(Reply::Held);
    f.t.send(submit("hello")).await.unwrap();
    until(&mut f.t, |e| {
        matches!(e, SessionEventWire::TurnStarted { .. })
    })
    .await;
    let c = expiry(&mut f.t, 41).await;
    assert_eq!(says(&c), unknown(CacheReason::Busy), "{c:?}");
    f.script.release();
    until(&mut f.t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(says(&expiry(&mut f.t, 42).await), WARM);
    f.script.turns(Reply::Refused);
    turn(&mut f.t, "refused").await;
    let c = expiry(&mut f.t, 43).await;
    assert_eq!(says(&c), unknown(CacheReason::HistoryUnsettled), "{c:?}");
    end(&mut f.t).await;
}

/// Parked: answered at ask without waking the session. Woken, the session
/// runs a new runtime, whose cell knows no request until its next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_session_answers_parked_and_knows_nothing_once_woken() {
    use agent_engine::session::SessionLifecycle;
    let (_h, _script, _bodies, host) = stub_host(NO_KEEPWARM_1H).await;
    let handle = host
        .create_session(agent_engine::session::SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    let before = expiry(&mut t, 49).await;
    assert_eq!(
        (says(&before), before.proof_ttl_secs),
        (WARM, Some(3600)),
        "{before:?}"
    );
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    handle.send(SessionCommand::Park).await.unwrap();
    wait_for("parked", || handle.lifecycle() == SessionLifecycle::Parked).await;
    // Asked as the host: an attach would wake it.
    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::Query {
            id: 50,
            query: SessionQuery::CacheExpiry,
        })
        .await
        .unwrap();
    let c = match watch(&mut rx, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 50, .. })
    })
    .await
    {
        SessionEventWire::QueryResult { value, .. } => {
            decode_query_reply::<CacheExpiry>(&value).unwrap()
        }
        _ => unreachable!(),
    };
    assert_eq!(says(&c), unknown(CacheReason::Parked), "{c:?}");
    assert_eq!(
        handle.lifecycle(),
        SessionLifecycle::Parked,
        "the ask did not wake it"
    );
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    wait_for("woken", || handle.lifecycle() == SessionLifecycle::Live).await;
    let c = expiry(&mut t, 51).await;
    assert_eq!(
        says(&c),
        unknown(CacheReason::NoMatchingRequest),
        "no request since the wake: {c:?}"
    );
    turn(&mut t, "again").await;
    assert_eq!(says(&expiry(&mut t, 52).await), WARM);
    end(&mut t).await;
}

// ── identity: a key fixed at send (§3.8, §3.10) ──────────────────────────

/// A→B: B has no matching request until its own, which is then the one
/// the cell knows (its 5m write latches the downgrade: 5m). B→A: only the
/// latest request is known, and it is B's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_switch_has_no_match_until_its_own_request() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    assert_eq!(says(&expiry(&mut f.t, 60).await), WARM);
    set_model(&mut f.t, 61, MODEL_B).await;
    let c = expiry(&mut f.t, 62).await;
    assert_eq!(says(&c), unknown(CacheReason::NoMatchingRequest), "{c:?}");
    f.script.turns(Reply::FiveMin);
    turn(&mut f.t, "on b").await;
    let b = expiry(&mut f.t, 63).await;
    assert_eq!(
        (says(&b), b.proof_ttl_secs),
        (WARM, Some(300)),
        "B's own: {b:?}"
    );
    set_model(&mut f.t, 64, MODEL).await;
    let back = expiry(&mut f.t, 65).await;
    assert_eq!(
        says(&back),
        unknown(CacheReason::NoMatchingRequest),
        "the latest request is B's: {back:?}"
    );
    end(&mut f.t).await;
}

/// A system prompt change, and `/clear`: no request of this session
/// matches the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_prompt_change_or_a_clear_has_no_matching_request() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    set(
        &mut f.t,
        70,
        SessionSetting::SystemPrompt {
            text: "be brief".into(),
        },
    )
    .await;
    let c = expiry(&mut f.t, 71).await;
    assert_eq!(says(&c), unknown(CacheReason::NoMatchingRequest), "{c:?}");
    turn(&mut f.t, "again").await;
    assert_eq!(says(&expiry(&mut f.t, 72).await), WARM);
    f.t.send(SessionCommand::NewSession).await.unwrap();
    until(&mut f.t, |e| matches!(e, SessionEventWire::Cleared { .. })).await;
    let c = expiry(&mut f.t, 73).await;
    assert_eq!(says(&c), unknown(CacheReason::NoMatchingRequest), "{c:?}");
    end(&mut f.t).await;
}

/// R3-1: A's answer lands after a switch to B (its identity was fixed when
/// it was built): B still has no matching request, and B's own request is
/// then the one known.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_answer_that_lands_after_a_switch_is_not_the_new_model_s() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    f.script.turns(Reply::Held);
    let sent = f.bodies.lock().unwrap().len();
    f.t.send(submit("hello")).await.unwrap();
    wait_for("A's request", || f.bodies.lock().unwrap().len() > sent).await;
    set_model(&mut f.t, 80, MODEL_B).await;
    f.script.release();
    until(&mut f.t, |e| matches!(e, SessionEventWire::Idle)).await;
    let c = expiry(&mut f.t, 81).await;
    assert_eq!(
        says(&c),
        unknown(CacheReason::NoMatchingRequest),
        "A's answer is not B's: {c:?}"
    );
    f.script.turns(Reply::FiveMin);
    turn(&mut f.t, "on b").await;
    let c = expiry(&mut f.t, 82).await;
    assert_eq!(
        (says(&c), c.proof_ttl_secs),
        (WARM, Some(300)),
        "B's own write: {c:?}"
    );
    end(&mut f.t).await;
}

/// R3-2: a turn on A paused at its tool across a switch to B. Its next
/// round still goes to A after the switch: never B's; switched back, it is
/// A's latest request (it really touched A's entry, §3.8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_running_across_a_switch_is_its_own_model_s_evidence() {
    let (mut f, started, release) = scripted_gate(NO_KEEPWARM_1H).await;
    f.script.turns(Reply::Tool);
    f.t.send(submit("hello")).await.unwrap();
    tokio::time::timeout(PATIENCE, started.notified())
        .await
        .expect("the tool runs");
    f.script.turns(Reply::OneHour);
    set_model(&mut f.t, 90, MODEL_B).await;
    let rounds = f.bodies.lock().unwrap().len();
    release.notify_one();
    until(&mut f.t, |e| matches!(e, SessionEventWire::Idle)).await;
    let after: serde_json::Value = serde_json::from_str(&f.bodies.lock().unwrap()[rounds]).unwrap();
    assert_eq!(after["model"], MODEL, "the running turn kept A");
    let c = expiry(&mut f.t, 91).await;
    assert_eq!(
        says(&c),
        unknown(CacheReason::NoMatchingRequest),
        "A's round is not B's: {c:?}"
    );
    set_model(&mut f.t, 92, MODEL).await;
    let c = expiry(&mut f.t, 93).await;
    assert_eq!(
        says(&c),
        WARM,
        "A's round, sent after the switch, is A's: {c:?}"
    );
    end(&mut f.t).await;
}

/// R6-1: an event turn on A before any command, then the first command
/// switches to B: B has no matching request; the same-model `Set` keeps
/// A's request the match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_first_command_after_an_event_turn() {
    for (model, want) in [
        (MODEL_B, unknown(CacheReason::NoMatchingRequest)),
        (MODEL, WARM),
    ] {
        let (_h, _script, _bodies, host) = stub_host(NO_KEEPWARM_1H).await;
        let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
        let queue = task.event_queue();
        let mut rx = handle.subscribe();
        tokio::spawn(task.run());
        queue
            .push(agent_engine::events::Event::simple(
                "watcher",
                "build finished",
                None,
            ))
            .unwrap();
        watch(&mut rx, |e| {
            matches!(e, SessionEventWire::TurnStarted { .. })
        })
        .await;
        watch(&mut rx, |e| matches!(e, SessionEventWire::Idle)).await;
        handle
            .send(SessionCommand::Set {
                id: 100,
                setting: SessionSetting::Model {
                    model: model.into(),
                },
            })
            .await
            .unwrap();
        // A host's `Set` is view-only for everyone (its id is not echoed).
        watch(&mut rx, |e| {
            matches!(e, SessionEventWire::SettingChanged(_))
        })
        .await;
        handle
            .send(SessionCommand::Query {
                id: 101,
                query: SessionQuery::CacheExpiry,
            })
            .await
            .unwrap();
        let c = match watch(&mut rx, |e| {
            matches!(e, SessionEventWire::QueryResult { id: 101, .. })
        })
        .await
        {
            SessionEventWire::QueryResult { value, .. } => {
                decode_query_reply::<CacheExpiry>(&value).unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(says(&c), want, "{model}: {c:?}");
        handle
            .send(SessionCommand::End {
                reason: agent_engine::session::EndReason::ClientQuit,
            })
            .await
            .unwrap();
        watch(&mut rx, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    }
}

async fn watch(
    rx: &mut tokio::sync::broadcast::Receiver<Envelope>,
    pred: impl Fn(&SessionEventWire) -> bool,
) -> SessionEventWire {
    loop {
        let env = tokio::time::timeout(PATIENCE, rx.recv())
            .await
            .expect("timely")
            .expect("alive");
        if pred(&env.event) {
            return env.event;
        }
    }
}

// ── the job: one reply, to its requester, checked at finish ──────────────

/// Two desks ask under the same request id while their jobs are held: each
/// gets exactly one reply, its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn colliding_ids_from_two_desks_each_get_their_own_answer() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    let (mut b, _) = LocalTransport::attach(f.handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let hold = Hold::new().open();
    ask(&mut f.t, 7).await;
    ask(&mut b, 7).await;
    let_one_through(&hold);
    let_one_through(&hold);
    for t in [&mut f.t, &mut b] {
        assert_eq!(says(&reply(t, 7).await.unwrap()), WARM);
        let more = drain_for(t, Duration::from_millis(300)).await;
        assert!(
            !more
                .iter()
                .any(|e| matches!(e.event, SessionEventWire::QueryResult { id: 7, .. })),
            "one reply each: {more:?}"
        );
    }
    drop(hold);
    end(&mut f.t).await;
}

/// A desk's newer ask supersedes its older one: the older id is answered
/// `superseded` at once, the newer one with the evidence; another desk's
/// job is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_newer_ask_supersedes_the_older_one() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    let (mut b, _) = LocalTransport::attach(f.handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let hold = Hold::new();
    ask(&mut b, 1).await;
    ask(&mut f.t, 1).await;
    ask(&mut f.t, 2).await;
    assert_eq!(
        reply(&mut f.t, 1).await.unwrap_err().code,
        QueryErrorCode::Superseded
    );
    hold.start.add_permits(3);
    hold.selected.add_permits(3);
    assert_eq!(says(&reply(&mut f.t, 2).await.unwrap()), WARM);
    assert_eq!(
        says(&reply(&mut b, 1).await.unwrap()),
        WARM,
        "the other desk's job ran on"
    );
    drop(hold);
    end(&mut f.t).await;
}

/// A `Set` and a tool registry change after the job read its inputs: the
/// answer is `stale`, and asking again answers for the new request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_change_after_the_read_is_stale() {
    let mut f = scripted(NO_KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    let hold = Hold::new().open();
    ask(&mut f.t, 110).await;
    reached(&hold, 1).await;
    set(&mut f.t, 111, SessionSetting::ApiRetries { n: 2 }).await;
    let_one_through(&hold);
    assert_eq!(
        reply(&mut f.t, 110).await.unwrap_err().code,
        QueryErrorCode::Stale,
        "a view publish"
    );
    ask(&mut f.t, 112).await;
    reached(&hold, 2).await;
    f.host
        .parts()
        .tools
        .write()
        .await
        .register(Arc::new(GateTool {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }));
    let_one_through(&hold);
    assert_eq!(
        reply(&mut f.t, 112).await.unwrap_err().code,
        QueryErrorCode::Stale,
        "the registry"
    );
    drop(hold);
    let c = expiry(&mut f.t, 113).await;
    assert_eq!(
        says(&c),
        unknown(CacheReason::NoMatchingRequest),
        "the tools are part of the key: {c:?}"
    );
    end(&mut f.t).await;
}

/// R4-M2 at the actor: a job read the cell, then an idle keep-warm ping is
/// dispatched, and separately it is accepted and measured, with no prefix,
/// view, registry or injection change. Each answer is `stale`, never the
/// old reading aged; a fresh ask answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_producer_after_the_read_is_stale() {
    let _fast = fast_pings();
    let mut f = scripted(KEEPWARM_1H).await;
    f.script.pings(Reply::Held);
    let hold = Hold::new().open();
    turn(&mut f.t, "hello").await;
    // Read, then a dispatch.
    let before = pings(&f.bodies);
    ask(&mut f.t, 120).await;
    reached(&hold, 1).await;
    assert_eq!(pings(&f.bodies), before, "the job read before the ping");
    wait_for("a ping dispatched", || pings(&f.bodies) > before).await;
    let_one_through(&hold);
    assert_eq!(
        reply(&mut f.t, 120).await.unwrap_err().code,
        QueryErrorCode::Stale,
        "a dispatch"
    );
    // The ping is in flight (dispatched): read, then it is accepted.
    ask(&mut f.t, 121).await;
    reached(&hold, 2).await;
    let (cost_before, _) = host_reply(
        &mut f.t,
        SessionCommand::Query {
            id: 122,
            query: SessionQuery::Status,
        },
        122,
    )
    .await;
    f.script.pings(Reply::Read);
    f.script.release();
    loop {
        let (st, _) = host_reply(
            &mut f.t,
            SessionCommand::Query {
                id: 123,
                query: SessionQuery::Status,
            },
            123,
        )
        .await;
        if st["keepwarm_cost"] != cost_before["keepwarm_cost"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let_one_through(&hold);
    assert_eq!(
        reply(&mut f.t, 121).await.unwrap_err().code,
        QueryErrorCode::Stale,
        "an acceptance"
    );
    hold.selected.add_permits(100);
    let c = expiry(&mut f.t, 124).await;
    assert_eq!(says(&c), WARM, "{c:?}");
    drop(hold);
    end(&mut f.t).await;
}

/// A keep-warm period: the answer names the next scheduled attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_active_period_names_the_next_attempt() {
    let mut f = scripted(KEEPWARM_1H).await;
    turn(&mut f.t, "hello").await;
    let c = expiry(&mut f.t, 130).await;
    assert_eq!(says(&c), WARM, "{c:?}");
    assert!(c.next_ping_in_secs.is_some(), "{c:?}");
    end(&mut f.t).await;
}
