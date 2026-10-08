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

/// A turn (or ping) that READ a 20k-token prefix from the cache.
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

/// Marker text of the keep-warm ping's trailing message.
const PING_MARK: &str = "prompt-cache keep-warm ping";

/// Records every request body; streaming requests get `SSE_CACHED`, the
/// non-streaming compaction call a JSON summary.
async fn recording_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: String| {
        let b = Arc::clone(&b);
        async move {
            let streaming = body.contains("\"stream\":true");
            b.lock().unwrap().push(body);
            if streaming {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    SSE_CACHED.to_string(),
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

fn period_id(st: &serde_json::Value) -> u64 {
    st["period"]["id"].as_u64().expect("an active period")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn command_matrix_read_only_keeps_period_prefix_changes_end_it() {
    let h = Home::new();
    write_config(&h, "cache_keepwarm = auto\n");
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
    // request (persist = false here, so it cannot park anyway).
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
    let (mut other, _) =
        LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
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

/// Pings fire on schedule (interval shortened by the test seam), carry the
/// keep-warm text after the history, are costed into the session; detaching
/// does NOT stop them and an active period defers park; once the ski-rental
/// bound ends the period the session parks normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pings_are_costed_and_an_active_period_defers_park_until_it_ends() {
    let h = Home::new();
    write_config(&h, "cache_keepwarm = auto\ncache_ttl = 1h\n");
    let _grace = EnvGuard::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let _interval = EnvGuard::set("SYNAPS_CACHE_KEEPWARM_INTERVAL_MS", "300");
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
        wait_pings(&bodies, 1, Duration::from_secs(10)).await,
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
    // Output cut to the thinking mode's minimum: one token, or budget + 1
    // when the model runs budgeted thinking (the budget is in the cache key).
    let want = match ping["thinking"]["type"].as_str() {
        Some("enabled") => ping["thinking"]["budget_tokens"].as_u64().unwrap() + 1,
        _ => 1,
    };
    assert_eq!(ping["max_tokens"], want, "{}", ping["thinking"]);
    assert!(msgs[2]["content"].as_str().unwrap().contains(PING_MARK));

    // Cost is surfaced: labelled, and inside the session total.
    let mut st = serde_json::Value::Null;
    for id in 77..177 {
        t.send(SessionCommand::Query {
            id,
            query: SessionQuery::Status,
        })
        .await
        .unwrap();
        st = query_result(&mut t, id).await;
        if st["keepwarm_cost"].as_f64().unwrap_or(0.0) > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

    // Detach: keep-warm continues, the session does not park meanwhile.
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    let after_detach = pings(&bodies);
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        handle.lifecycle(),
        SessionLifecycle::Live,
        "an active period defers park (grace is 0)"
    );
    assert!(
        pings(&bodies) > after_detach,
        "detach does not stop keep-warm"
    );

    // 20k-token prefix under 1h: bound = 2.0×, a ping ≈ 0.1× + overhead →
    // ~19 pings, then the period ends and the session parks normally.
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(30)).await,
        "parks once the ski-rental bound ends the period"
    );
    let total = pings(&bodies);
    assert!((15..=20).contains(&total), "{total} pings");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(pings(&bodies), total, "no ping after the period ended");
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
}
