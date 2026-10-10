//! Task #444 — prompt-cache keep-warm through the real session actor:
//! the command matrix (read-only commands keep the idle period, prefix
//! changes end it through the fingerprint, a turn starts a new one), pings
//! firing on schedule with their cost surfaced, and an active period
//! deferring park until it ends.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, EndReason, HostEvent,
    LocalTransport, SessionCommand, SessionConfig, SessionHandle, SessionLifecycle, SessionQuery,
    SessionSetting,
};
use axum::response::IntoResponse;
use serial_test::serial;

/// A real turn: read 18k, wrote 2k with the 1h TTL (evidence that the
/// server honours 1h), 10 uncached input tokens, a 1-token reply.
const SSE_TURN: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kw\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":18000,",
    "\"cache_creation\":{\"ephemeral_5m_input_tokens\":0,\"ephemeral_1h_input_tokens\":2000}}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":18000,",
    "\"cache_creation\":{\"ephemeral_5m_input_tokens\":0,\"ephemeral_1h_input_tokens\":2000}}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A ping that READ the 20k-token prefix from the cache.
const SSE_CACHED: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kw\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":20000}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":20000}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A ping that was billed (usage arrived) and then failed in-stream.
const SSE_USAGE_THEN_ERROR: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kw\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":20000}}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null,",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":20000}}\n\n",
    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
    "\"message\":\"synthetic\"}}\n\n",
);

/// Marker text of the keep-warm ping's trailing message.
const PING_MARK: &str = "prompt-cache keep-warm ping";

/// How the stub answers keep-warm pings (real turns always get `SSE_TURN`).
#[derive(Clone, Copy)]
enum Pings {
    /// A clean cache read.
    Cached,
    /// Never answers (the request is in flight until aborted).
    Hang,
    /// Usage, then an in-stream error.
    UsageThenError,
}

/// Records every request body; streaming requests get an SSE body, the
/// non-streaming compaction call a JSON summary.
async fn recording_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    recording_stub_with(Pings::Cached).await
}

async fn recording_stub_with(mode: Pings) -> (String, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: String| {
        let b = Arc::clone(&b);
        async move {
            let streaming = body.contains("\"stream\":true");
            let ping = body.contains(PING_MARK);
            b.lock().unwrap().push(body);
            if streaming {
                let sse = match (ping, mode) {
                    (false, _) => SSE_TURN,
                    (true, Pings::Cached) => SSE_CACHED,
                    (true, Pings::UsageThenError) => SSE_USAGE_THEN_ERROR,
                    (true, Pings::Hang) => {
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        SSE_CACHED
                    }
                };
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse.to_string(),
                )
                    .into_response();
            }
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "summary"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bodies)
}

fn pings(bodies: &Arc<Mutex<Vec<String>>>) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.contains(PING_MARK))
        .count()
}

fn write_config(home: &Home, body: &str) {
    std::fs::write(home.dir.path().join(".synaps-cli").join("config"), body).unwrap();
}

struct EnvGuard(&'static str);
impl EnvGuard {
    fn set(k: &'static str, v: &str) -> Self {
        std::env::set_var(k, v);
        Self(k)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

async fn turn(t: &mut LocalTransport, text: &str) {
    t.send(submit(text)).await.unwrap();
    until(t, |e| {
        matches!(e, agent_engine::session::SessionEventWire::Idle)
    })
    .await;
}

async fn query_result(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    let seen = until(t, |e| {
        matches!(e, agent_engine::session::SessionEventWire::QueryResult { id: i, .. } if *i == id)
    })
    .await;
    match &seen.last().unwrap().event {
        agent_engine::session::SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

/// `/keepwarm <arg>` through the real dispatch path; the structured state.
async fn keepwarm(t: &mut LocalTransport, id: u64, arg: &str) -> serde_json::Value {
    t.send(SessionCommand::EngineCommand {
        id,
        name: "keepwarm".into(),
        arg: arg.into(),
    })
    .await
    .unwrap();
    let v = query_result(t, id).await;
    assert_eq!(v["kind"], "output", "{v}");
    v["keepwarm"].clone()
}

/// How a command can affect an idle keep-warm period. Exhaustive on
/// purpose: a new `SessionCommand` variant does not compile here until it is
/// classified. Production code has NO such table — the fingerprint decides.
#[derive(Debug, PartialEq)]
enum Effect {
    /// Never changes the next request: the period continues.
    Untouched,
    /// May change the next request's prefix (model, thinking, tools, system,
    /// history): the period ends iff the fingerprint changed.
    Fingerprint,
    /// Starts a real turn: the period ends, the turn's Idle starts a new one.
    TurnStart,
    /// Compaction replaces the history: the period ends at its start.
    CompactionStart,
    /// An armed driver owns the session: the blocker ends the period.
    DriverOwned,
    /// The session ends.
    SessionEnd,
}

fn classify(cmd: &SessionCommand) -> Effect {
    match cmd {
        SessionCommand::Submit { .. }
        | SessionCommand::Steer { .. }
        | SessionCommand::SubmitPrepared { .. } => Effect::TurnStart,
        SessionCommand::Compact { .. } => Effect::CompactionStart,
        SessionCommand::NewSession
        | SessionCommand::Resume { .. }
        | SessionCommand::EngineCommand { .. }
        | SessionCommand::PluginCommand { .. } => Effect::Fingerprint,
        SessionCommand::Set { setting, .. } => match setting {
            SessionSetting::Model { .. }
            | SessionSetting::ReasoningLevel { .. }
            | SessionSetting::SystemPrompt { .. }
            | SessionSetting::ReloadPrompt
            | SessionSetting::ContextWindow { .. }
            | SessionSetting::GrantWorkerModel { .. } => Effect::Fingerprint,
            SessionSetting::CompactionModel { .. }
            | SessionSetting::ApiRetries { .. }
            | SessionSetting::SubagentTimeout { .. }
            | SessionSetting::MaxToolOutput { .. }
            | SessionSetting::BashTimeout { .. }
            | SessionSetting::BashMaxTimeout { .. } => Effect::Untouched,
        },
        SessionCommand::Cancel
        | SessionCommand::Answer { .. }
        | SessionCommand::Save
        | SessionCommand::Query { .. }
        | SessionCommand::Attach { .. }
        | SessionCommand::Detach { .. }
        | SessionCommand::Resync { .. }
        | SessionCommand::Checkpoint { .. }
        | SessionCommand::KeepWarm { .. }
        | SessionCommand::Park
        | SessionCommand::HostEvent(_) => Effect::Untouched,
        SessionCommand::DriverStart { .. } => Effect::DriverOwned,
        SessionCommand::End { .. } => Effect::SessionEnd,
    }
}

/// The keep-warm test seam: 300 ms between pings for this test (code-only,
/// never an environment variable — see `cache_keepwarm::interval_override`).
fn fast_pings() -> agent_engine::session::cache_keepwarm::testing::IntervalOverride {
    agent_engine::session::cache_keepwarm::testing::IntervalOverride::set(Duration::from_millis(
        300,
    ))
}

fn period_id(st: &serde_json::Value) -> u64 {
    st["period"]["id"].as_u64().expect("an active period")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn command_matrix_read_only_keeps_period_prefix_changes_end_it() {
    let h = Home::new();
    // Thinking off: the ping's max_tokens is 1 (budgeted thinking would put
    // `budget + 1` output tokens into the worst case and no ping would fit).
    write_config(&h, "cache_keepwarm = auto\nthinking = off\n");
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let mut id = 1_000u64;
    let mut next_id = || {
        id += 1;
        id
    };

    // No period before a real Anthropic request ran.
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert_eq!(st["mode"], "auto");
    assert_eq!(st["active"], false, "{st}");

    turn(&mut t, "hello").await;
    let st = keepwarm(&mut t, next_id(), "").await;
    assert_eq!(st["active"], true, "a finished turn starts a period: {st}");
    assert_eq!(st["prefix_matches"], true, "{st}");
    let first_period = period_id(&st);

    // ── read-only: every one of these leaves the period running ──
    let me = t.client_id();
    let read_only: Vec<(&str, SessionCommand)> = vec![
        (
            "query status",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::Status,
            },
        ),
        (
            "query view",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::View,
            },
        ),
        (
            "query messages",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::Messages,
            },
        ),
        (
            "query context report",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::ContextReport,
            },
        ),
        (
            "query context assessment",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::ContextAssessment,
            },
        ),
        (
            "query prompt inspection",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::PromptInspection,
            },
        ),
        (
            "query tools schema",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::ToolsSchema,
            },
        ),
        (
            "query subagent rows",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::SubagentRows,
            },
        ),
        (
            "query display tail",
            SessionCommand::Query {
                id: next_id(),
                query: SessionQuery::DisplayTail { items: 5 },
            },
        ),
        (
            "/context",
            SessionCommand::EngineCommand {
                id: next_id(),
                name: "context".into(),
                arg: String::new(),
            },
        ),
        (
            "/budget status",
            SessionCommand::EngineCommand {
                id: next_id(),
                name: "budget".into(),
                arg: "status".into(),
            },
        ),
        (
            "/keepwarm status",
            SessionCommand::EngineCommand {
                id: next_id(),
                name: "keepwarm".into(),
                arg: "status".into(),
            },
        ),
        (
            "unknown plugin command",
            SessionCommand::PluginCommand {
                id: next_id(),
                plugin: "no-such-plugin".into(),
                name: "x".into(),
                arg: String::new(),
            },
        ),
        ("save", SessionCommand::Save),
        (
            "resync",
            SessionCommand::Resync {
                client: me,
                since_seq: 0,
            },
        ),
        ("pin", SessionCommand::KeepWarm { on: true }),
        ("unpin", SessionCommand::KeepWarm { on: false }),
        ("cancel while idle", SessionCommand::Cancel),
        (
            "answer unknown prompt",
            SessionCommand::Answer {
                prompt_id: 9_999,
                value: None,
            },
        ),
        (
            "checkpoint",
            SessionCommand::Checkpoint {
                reason: CheckpointReason::HostRequest,
            },
        ),
        (
            "set api retries",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::ApiRetries { n: 3 },
            },
        ),
        (
            "set max tool output",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::MaxToolOutput { bytes: 50_000 },
            },
        ),
        (
            "set bash timeout",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::BashTimeout { secs: 60 },
            },
        ),
        (
            "set subagent timeout",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::SubagentTimeout { secs: 120 },
            },
        ),
    ];
    for (name, cmd) in read_only {
        let effect = classify(&cmd);
        assert!(
            matches!(effect, Effect::Untouched | Effect::Fingerprint),
            "{name}: {effect:?}"
        );
        t.send(cmd).await.unwrap();
        let st = keepwarm(&mut t, next_id(), "status").await;
        assert_eq!(st["active"], true, "{name} stopped keep-warm: {st}");
        assert_eq!(
            st["prefix_matches"], true,
            "{name} changed the prefix: {st}"
        );
        assert_eq!(period_id(&st), first_period, "{name}");
    }
    // Host-side, never from a client: an extension notification and a park
    // request. persist = false here, so the session cannot park and an
    // explicit Park leaves the period alone (with persist it ends it first,
    // see `explicit_park_ends_the_period_then_parks`).
    for cmd in [
        SessionCommand::HostEvent(HostEvent::ExtensionNotification {
            extension_id: "ext".into(),
            method: "note".into(),
            params: serde_json::json!({}),
        }),
        SessionCommand::Park,
    ] {
        assert_eq!(classify(&cmd), Effect::Untouched);
        handle.send(cmd).await.unwrap();
    }
    // A second client attaching and leaving.
    let (other, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    other
        .send(SessionCommand::Detach {
            client: other.client_id(),
        })
        .await
        .unwrap();
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert_eq!(st["active"], true, "host events / attach / detach: {st}");
    assert_eq!(period_id(&st), first_period);

    // ── a real turn ends the period; its Idle starts a fresh one ──
    let cmd = submit("again");
    assert_eq!(classify(&cmd), Effect::TurnStart);
    t.send(cmd).await.unwrap();
    until(&mut t, |e| {
        matches!(e, agent_engine::session::SessionEventWire::Idle)
    })
    .await;
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert!(period_id(&st) > first_period, "{st}");
    assert_eq!(st["last_end"], "turn started", "{st}");

    // ── prefix changes: each ends the period through the fingerprint ──
    let thinking = if t.view().thinking_level == "off" {
        "high"
    } else {
        "off"
    };
    let changers: Vec<(&str, SessionCommand)> = vec![
        (
            "set system prompt",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::SystemPrompt {
                    text: "A different system prompt.".into(),
                },
            },
        ),
        (
            "/thinking",
            SessionCommand::EngineCommand {
                id: next_id(),
                name: "thinking".into(),
                arg: thinking.into(),
            },
        ),
        (
            "set model",
            SessionCommand::Set {
                id: next_id(),
                setting: SessionSetting::Model {
                    model: "claude-opus-4-6".into(),
                },
            },
        ),
        ("/clear", SessionCommand::NewSession),
    ];
    for (name, cmd) in changers {
        assert_eq!(classify(&cmd), Effect::Fingerprint, "{name}");
        turn(&mut t, "baseline").await;
        let st = keepwarm(&mut t, next_id(), "status").await;
        assert_eq!(st["active"], true, "{name}: baseline period: {st}");
        t.send(cmd).await.unwrap();
        let st = keepwarm(&mut t, next_id(), "status").await;
        assert_eq!(st["active"], false, "{name} must end the period: {st}");
        assert_eq!(st["last_end"], "prefix changed", "{name}: {st}");
        if name == "/thinking" && thinking != "off" {
            // Budgeted thinking on this small mock prefix is (correctly) refused by
            // the worst-case bound, which would mask the next changer's baseline.
            // Restore the cheap thinking-off baseline before the next case.
            t.send(SessionCommand::EngineCommand {
                id: next_id(),
                name: "thinking".into(),
                arg: "off".into(),
            })
            .await
            .unwrap();
        }
    }

    // ── /keepwarm off is honoured per session; auto turns it back on ──
    turn(&mut t, "baseline").await;
    let st = keepwarm(&mut t, next_id(), "off").await;
    assert_eq!(
        (st["mode"].as_str(), st["active"].as_bool()),
        (Some("off"), Some(false))
    );
    assert_eq!(st["last_end"], "disabled", "{st}");
    turn(&mut t, "while off").await;
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert_eq!(st["active"], false, "off: no period after a turn: {st}");
    let st = keepwarm(&mut t, next_id(), "6h").await;
    assert_eq!(st["mode"], "6h");
    turn(&mut t, "back on").await;
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert_eq!(st["active"], true, "{st}");

    // ── compaction ends the period at its start ──
    let cmd = SessionCommand::Compact { instructions: None };
    assert_eq!(classify(&cmd), Effect::CompactionStart);
    t.send(cmd).await.unwrap();
    let st = keepwarm(&mut t, next_id(), "status").await;
    assert_eq!(st["active"], false, "{st}");
    assert_eq!(st["last_end"], "compaction", "{st}");

    // The interval is the real one (minutes): not a single ping was sent.
    assert_eq!(pings(&bodies), 0);
    let end_cmd = SessionCommand::End {
        reason: EndReason::ClientQuit,
    };
    assert_eq!(classify(&end_cmd), Effect::SessionEnd);
    end(&mut t).await;
}

async fn wait_lifecycle(handle: &SessionHandle, want: SessionLifecycle, for_: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + for_;
    while tokio::time::Instant::now() < deadline {
        if handle.lifecycle() == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handle.lifecycle() == want
}

async fn wait_pings(bodies: &Arc<Mutex<Vec<String>>>, n: usize, for_: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + for_;
    while tokio::time::Instant::now() < deadline {
        if pings(bodies) >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    pings(bodies) >= n
}

/// `Status` query through the real dispatch path.
async fn status(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    t.send(SessionCommand::Query {
        id,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    query_result(t, id).await
}

/// Poll `Status` until `pred` holds (or `for_` elapses); the last status.
async fn status_until(
    t: &mut LocalTransport,
    first_id: u64,
    for_: Duration,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + for_;
    let mut id = first_id;
    loop {
        let st = status(t, id).await;
        if pred(&st) || tokio::time::Instant::now() >= deadline {
            return st;
        }
        id += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Generous waits. Nothing here asserts that something happens WITHIN a
/// window: the schedule is driven by a 300 ms test interval against a 1h
/// TTL, so a ping that runs late under load is still a cache read (never
/// expired), the number of pings is fixed by the ledger arithmetic, not by
/// time, and every wait below only bounds how long a hung test takes to fail.
const PATIENCE: Duration = Duration::from_secs(60);

/// Pings fire on schedule (interval shortened by the test seam), carry the
/// keep-warm text after the history, are costed into the session; detaching
/// does NOT stop them and an active period defers park; once the ski-rental
/// bound ends the period the session parks normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pings_are_costed_and_an_active_period_defers_park_until_it_ends() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _grace = EnvGuard::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let _interval = fast_pings();
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    assert!(
        wait_pings(&bodies, 1, PATIENCE).await,
        "the first ping fires one interval after the turn's request"
    );
    let ping: serde_json::Value = {
        let all = bodies.lock().unwrap();
        serde_json::from_str(all.iter().find(|b| b.contains(PING_MARK)).unwrap()).unwrap()
    };
    let msgs = ping["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        3,
        "history (user, assistant) + the trailing ping"
    );
    assert!(ping.get("thinking").is_none(), "{}", ping["thinking"]);
    assert_eq!(ping["max_tokens"], 1);
    assert!(msgs[2]["content"].as_str().unwrap().contains(PING_MARK));

    // Cost is surfaced: labelled, inside the session total, and the ping
    // tokens are inside the token totals (shown separately).
    let st = status_until(&mut t, 77, PATIENCE, |st| {
        st["keepwarm_cost"].as_f64().unwrap_or(0.0) > 0.0
    })
    .await;
    let kw = st["keepwarm_cost"].as_f64().unwrap();
    assert!(kw > 0.0, "{st}");
    assert!(
        st["keepwarm"]["session_pings"].as_u64().unwrap() >= 1,
        "{st}"
    );
    assert!(
        st["cost"].as_f64().unwrap() > kw,
        "the total includes the turn: {st}"
    );
    // The turn's 1h write is the evidence: governed by the 1h TTL.
    assert_eq!(st["keepwarm"]["period"]["ttl_secs"], 3600, "{st}");

    // Detach: keep-warm continues, and while a period is active the session
    // never parks (grace is 0, so it would park at once otherwise).
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    let after_detach = pings(&bodies);
    assert!(
        wait_pings(&bodies, after_detach + 1, PATIENCE).await || pings(&bodies) >= 19,
        "detach does not stop keep-warm"
    );

    // 20k-token prefix under the honoured 1h TTL: the bound is the $0.12
    // rewrite, one ping costs $0.006045 (read + 10 input + 1 output), so the
    // ledger admits exactly 19 pings; then the period ends and the session
    // parks normally.
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, PATIENCE).await,
        "parks once the ski-rental bound ends the period"
    );
    let total = pings(&bodies);
    assert_eq!(total, 19, "{total} pings");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(pings(&bodies), total, "no ping after the period ended");
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
}

/// An explicit `Park` while a period is active ends it ("parked by
/// request") and parks; only the idle auto-park waits for the period.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn explicit_park_ends_the_period_then_parks() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    // Grace 0: without the keep-warm deferral the session would park the
    // moment its last client detaches.
    let _grace = EnvGuard::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    let st = keepwarm(&mut t, 500, "status").await;
    assert_eq!(st["active"], true, "{st}");
    // Detach: with grace 0 the idle auto-park is due at once, but the
    // active period defers it — still Live well after the detach.
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    assert!(
        !wait_lifecycle(
            &handle,
            SessionLifecycle::Parked,
            Duration::from_millis(750)
        )
        .await,
        "an active period defers the zero-grace auto-park"
    );
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    // An explicit park is not deferred: it ends the period and parks.
    handle.send(SessionCommand::Park).await.unwrap();
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, PATIENCE).await,
        "an explicit park is not deferred"
    );
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let st = keepwarm(&mut t, 501, "status").await;
    assert_eq!(st["active"], false, "{st}");
    assert_eq!(st["last_end"], "parked by request", "{st}");
    assert_eq!(pings(&bodies), 0, "the real interval: no ping was due");
    end(&mut t).await;
}

/// `/keepwarm auto` on a session that is already idle starts a period at
/// once (no new turn needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn keepwarm_auto_on_an_idle_session_starts_a_period_now() {
    let h = Home::new();
    write_config(&h, "cache_keepwarm = off\ncache_ttl = 1h\nthinking = off\n");
    let (url, _bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    let st = keepwarm(&mut t, 600, "status").await;
    assert_eq!(
        (st["mode"].as_str(), st["active"].as_bool()),
        (Some("off"), Some(false))
    );
    let st = keepwarm(&mut t, 601, "auto").await;
    assert_eq!(st["active"], true, "started immediately: {st}");
    let st = keepwarm(&mut t, 602, "off").await;
    assert_eq!(st["active"], false, "{st}");
    let st = keepwarm(&mut t, 603, "6h").await;
    assert_eq!(st["active"], true, "<N>h starts it too: {st}");
    end(&mut t).await;
}

/// Spend is booked on every path: a ping that was billed and then failed
/// (usage arrived, then an in-stream error) lands in the keep-warm cost, the
/// session cost and the token totals, though it counts as no successful ping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_billed_ping_that_failed_is_still_booked() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _interval = fast_pings();
    let (url, bodies) = recording_stub_with(Pings::UsageThenError).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    assert!(wait_pings(&bodies, 1, PATIENCE).await);
    let st = status_until(&mut t, 701, PATIENCE, |st| {
        st["keepwarm_cost"].as_f64().unwrap_or(0.0) > 0.0
    })
    .await;
    let kw = st["keepwarm_cost"].as_f64().unwrap();
    // 20k read + 10 input + 1 output on Sonnet 4.5.
    assert!((kw - 0.006045).abs() < 1e-6, "{st}");
    assert_eq!(st["keepwarm"]["session_pings"], 0, "a failed ping: {st}");
    // The turn: 10 input + 1 output + 18k read + 2k written at 1h.
    let turn_cost = (10.0 * 3.0 + 15.0 + 18_000.0 * 0.3 + 2_000.0 * 6.0) / 1e6;
    let cost = st["cost"].as_f64().unwrap();
    assert!((cost - turn_cost - kw).abs() < 1e-9, "{st}");
    assert!(
        st["tokens"]["input"].as_u64().unwrap() > 10,
        "ping tokens are in the totals: {st}"
    );
    end(&mut t).await;
}

/// A ping aborted in flight (a turn starts while it is on the wire) is
/// booked at the period's worst-case estimate: it may well be billed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_aborted_in_flight_ping_is_booked() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _interval = fast_pings();
    let (url, bodies) = recording_stub_with(Pings::Hang).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    // The stub has the ping's body: it left this process and is in flight.
    assert!(wait_pings(&bodies, 1, PATIENCE).await);
    let before = status(&mut t, 800).await;
    assert_eq!(before["keepwarm"]["period"]["in_flight"], true, "{before}");
    assert_eq!(before["keepwarm_cost"].as_f64().unwrap(), 0.0);
    turn(&mut t, "again").await;
    let st = status(&mut t, 801).await;
    let kw = st["keepwarm_cost"].as_f64().unwrap();
    // Worst case of ping 1: 20k read + the 11-token tail written at 1h +
    // 64 estimated input + 1 output, on Sonnet 4.5.
    let worst = (20_000.0 * 0.3 + 11.0 * 6.0 + 64.0 * 3.0 + 15.0) / 1e6;
    assert!((kw - worst).abs() < 1e-9, "{kw} vs {worst}: {st}");
    assert_eq!(st["keepwarm"]["session_pings"], 0, "{st}");
    end(&mut t).await;
}

/// Worst case of ping 1 in these tests: 20k read + the 11-token tail
/// written at 1h + 64 estimated input + 1 output, on Sonnet 4.5.
const WORST_PING_1: f64 = (20_000.0 * 0.3 + 11.0 * 6.0 + 64.0 * 3.0 + 15.0) / 1e6;

/// One `SSE_TURN` turn on Sonnet 4.5: 10 input + 1 output + 18k read + 2k
/// written at 1h.
const TURN_COST: f64 = (10.0 * 3.0 + 15.0 + 18_000.0 * 0.3 + 2_000.0 * 6.0) / 1e6;

/// I-1: once the ski-rental bound ended the period, `/keepwarm off` then
/// `auto` in the same cache epoch continues the exhausted ledger: nothing
/// more is pinged until a real turn starts a new epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn re_enabling_after_the_bound_pings_nothing_until_a_real_turn() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _interval = fast_pings();
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    let st = status_until(&mut t, 900, PATIENCE, |st| {
        st["keepwarm"]["last_end"]
            .as_str()
            .is_some_and(|e| e.contains("ski-rental bound"))
    })
    .await;
    assert_eq!(st["keepwarm"]["active"], false, "{st}");
    let total = pings(&bodies);
    assert_eq!(total, 19, "{st}");

    let st = keepwarm(&mut t, 1_000, "off").await;
    assert_eq!(st["active"], false, "{st}");
    let st = keepwarm(&mut t, 1_001, "auto").await;
    assert_eq!(st["active"], false, "the epoch's ledger is spent: {st}");
    assert!(
        st["last_end"]
            .as_str()
            .is_some_and(|e| e.contains("not started: ski-rental bound")),
        "{st}"
    );
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(pings(&bodies), total, "re-enabling pinged nothing");

    // A real turn is a new epoch: a fresh ledger.
    turn(&mut t, "again").await;
    let st = keepwarm(&mut t, 1_002, "status").await;
    assert_eq!(st["active"], true, "{st}");
    end(&mut t).await;
}

/// M-2: `/clear` while a ping is in flight ends the period at once, aborts
/// the ping, and books it to the OLD conversation (saved by the clear).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn clear_mid_ping_books_the_ping_to_the_old_conversation() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _interval = fast_pings();
    let (url, bodies) = recording_stub_with(Pings::Hang).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    assert!(wait_pings(&bodies, 1, PATIENCE).await);
    let before = status(&mut t, 1_100).await;
    assert_eq!(before["keepwarm"]["period"]["in_flight"], true, "{before}");
    let old_id = before["session"].as_str().unwrap().to_string();

    t.send(SessionCommand::NewSession).await.unwrap();
    until(&mut t, |e| {
        matches!(e, agent_engine::session::SessionEventWire::Cleared { .. })
    })
    .await;
    let st = status(&mut t, 1_101).await;
    assert_ne!(st["session"], old_id.as_str(), "{st}");
    assert_eq!(st["keepwarm"]["active"], false, "{st}");
    assert_eq!(st["keepwarm"]["last_end"], "prefix changed", "{st}");
    assert_eq!(st["keepwarm_cost"].as_f64().unwrap(), 0.0, "{st}");
    assert_eq!(
        st["cost"].as_f64().unwrap(),
        0.0,
        "not on the new one: {st}"
    );
    let old = agent_engine::resolve_session(&old_id).unwrap();
    assert!(
        (old.session_cost - TURN_COST - WORST_PING_1).abs() < 1e-9,
        "booked to the old conversation: {} vs {}",
        old.session_cost,
        TURN_COST + WORST_PING_1
    );
    end(&mut t).await;
}

/// M-2: `/resume` while a ping is in flight ends the period at once, aborts
/// the ping, and books it to the conversation being left — never to the
/// resumed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn resume_mid_ping_books_the_ping_to_the_old_conversation() {
    let h = Home::new();
    write_config(
        &h,
        "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n",
    );
    let _interval = fast_pings();
    let (url, bodies) = recording_stub_with(Pings::Hang).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    // A saved session to resume (on disk only: no actor holds its lock).
    let mut saved = agent_engine::Session::new(MODEL, "off", None);
    saved.api_messages = vec![
        Arc::new(serde_json::json!({"role": "user", "content": "other"})),
        Arc::new(
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]}),
        ),
    ];
    saved.session_cost = 0.125;
    saved.save().await.unwrap();
    let other_id = saved.id.clone();

    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    assert!(wait_pings(&bodies, 1, PATIENCE).await);
    let before = status(&mut t, 1_200).await;
    assert_eq!(before["keepwarm"]["period"]["in_flight"], true, "{before}");
    let old_id = before["session"].as_str().unwrap().to_string();

    t.send(SessionCommand::Resume {
        id: 1_202,
        query: other_id.clone(),
    })
    .await
    .unwrap();
    until(&mut t, |e| {
        matches!(e, agent_engine::session::SessionEventWire::Resumed { .. })
    })
    .await;
    let st = status(&mut t, 1_203).await;
    assert_eq!(st["session"], other_id.as_str(), "{st}");
    assert_eq!(st["keepwarm"]["active"], false, "{st}");
    assert_eq!(st["keepwarm"]["last_end"], "prefix changed", "{st}");
    assert_eq!(st["keepwarm_cost"].as_f64().unwrap(), 0.0, "{st}");
    assert_eq!(
        st["cost"].as_f64().unwrap(),
        0.125,
        "the resumed conversation is not charged: {st}"
    );
    let old = agent_engine::resolve_session(&old_id).unwrap();
    assert!(
        (old.session_cost - TURN_COST - WORST_PING_1).abs() < 1e-9,
        "booked to the old conversation: {} vs {}",
        old.session_cost,
        TURN_COST + WORST_PING_1
    );
    end(&mut t).await;
}

/// Review #205: a settled ping's charge reaches disk on its own. Without it a
/// crash while idle loses the spend and restores `max_session_cost`
/// headroom.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn review_205_completed_ping_accounting_is_persisted_without_manual_save() {
    let h = Home::new();
    write_config(&h, "cache_keepwarm = auto\ncache_ttl = 1h\nthinking = off\n");
    let _interval = fast_pings();
    let (url, _bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    turn(&mut t, "hello").await;
    let st = status_until(&mut t, 6000, PATIENCE, |s| {
        s["keepwarm_cost"].as_f64().unwrap_or(0.0) > 0.0
    })
    .await;
    assert!(st["keepwarm_cost"].as_f64().unwrap() > 0.0);
    // No manual Save/Park/End or further real turn before reading durable state.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let disk = agent_core::session::Session::load(st["session"].as_str().unwrap()).unwrap();
    let disk_kw = disk.keepwarm_cost;
    let disk_cost = disk.session_cost;
    end(&mut t).await;
    assert!(
        disk_kw > 0.0,
        "completed ping missing on disk: kw={disk_kw}, total={disk_cost}; in-memory={st}"
    );
}
