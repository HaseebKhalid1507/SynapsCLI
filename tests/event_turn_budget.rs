//! Event-triggered turns run under the event turn budget
//! (`turn_budget.event.*`, default 24 rounds / 96 tool calls / 8 MiB),
//! narrowed by the session's own budget; turns the user types stay
//! unlimited (#219). End to end through `EngineHost::create_session` and a
//! loopback Anthropic stub whose model asks for a tool every round.

#[path = "support/phase2/mod.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::{EngineHost, HostOpts};
use serial_test::serial;
use support::*;
use synaps_cli::{BudgetDimension, SessionEvent, StreamEvent, TurnOutcome};

const MODEL: &str = "claude-sonnet-4-5";

/// 30 tool rounds, then a plain answer.
static THIRTY_TOOL_ROUNDS_THEN_DONE: &[&str] = &[
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE_TOOL_USE,
    ANTHROPIC_SSE,
];

async fn session(host: &Arc<EngineHost>) -> LocalTransport {
    let handle = host
        .create_session(SessionConfig {
            model_override: Some(MODEL.into()),
            persist: false,
            ..SessionConfig::default()
        })
        .await
        .expect("create_session");
    let (t, _snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    t
}

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot")
}

/// Pump until the turn machine parks; returns the turn's terminal outcome
/// (if any) and its final history.
async fn drive_to_idle(
    t: &mut LocalTransport,
) -> (Option<TurnOutcome>, Vec<synaps_cli::SharedMessage>) {
    let mut outcome = None;
    let mut history = Vec::new();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(60), t.next_event())
            .await
            .expect("actor hung")
            .expect("actor alive");
        match env.event {
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Error(e))) => {
                outcome = Some(e.outcome)
            }
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(h))) => {
                history = h
            }
            SessionEventWire::Idle => return (outcome, history),
            SessionEventWire::Ended { .. } => panic!("ended early"),
            _ => {}
        }
    }
}

fn assert_history_pairing(history: &[synaps_cli::SharedMessage]) {
    let mut uses = 0usize;
    let mut results = 0usize;
    for msg in history {
        for b in msg["content"].as_array().into_iter().flatten() {
            match b["type"].as_str() {
                Some("tool_use") => uses += 1,
                Some("tool_result") => results += 1,
                _ => {}
            }
        }
    }
    assert!(uses > 0);
    assert_eq!(uses, results, "every tool_use keeps a tool_result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn event_triggered_turn_stops_at_the_event_round_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[ANTHROPIC_SSE_TOOL_USE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let mut t = session(&host).await;

    let mut ev = synaps_cli::events::types::Event::simple("budget-test", "wake up", None);
    ev.id = "evt-budget".into();
    let path = synaps_cli::events::registry::find_session_registration(t.session_id().as_str())
        .expect("published session endpoint")
        .socket_path;
    {
        use tokio::io::AsyncWriteExt;
        let mut s = tokio::net::UnixStream::connect(&path)
            .await
            .expect("session socket");
        s.write_all(serde_json::to_string(&ev).unwrap().as_bytes())
            .await
            .unwrap();
        s.shutdown().await.unwrap();
    }
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("actor hung")
            .expect("alive");
        if matches!(env.event, SessionEventWire::TurnStarted { .. }) {
            break;
        }
    }
    let (outcome, history) = drive_to_idle(&mut t).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        24,
        "event turns stop at 24 rounds"
    );
    assert_eq!(
        outcome,
        Some(TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ProviderRounds
        })
    );
    assert_history_pairing(&history);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_typed_turn_is_not_bound_by_the_event_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(THIRTY_TOOL_ROUNDS_THEN_DONE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let mut t = session(&host).await;

    t.send(SessionCommand::Submit {
        text: "do thirty things".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    let (outcome, history) = drive_to_idle(&mut t).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        31,
        "30 tool rounds and the answer"
    );
    assert_eq!(outcome, None, "no budget stop for a typed turn");
    assert_history_pairing(&history);
}

// ── #251 × the session-start gate ───────────────────────────────────────────
//
// A turn's stream can wait for `on_session_start` (`await_extensions =
// false`; the host's gate held by a test seam). The event budget belongs to
// the turn, not to the moment `start_turn` ran: the deferred stream of an
// event turn still runs under it, and a turn the user types runs without it,
// whether it waited or not.

use agent_engine::session::session_start_testing as start_seam;

/// A session whose `on_session_start` runs in the background, through the
/// host's gate (so a held gate keeps its first turn's request waiting).
async fn deferred_start_session(
    host: &Arc<EngineHost>,
) -> (LocalTransport, String) {
    let handle = host
        .create_session(SessionConfig {
            model_override: Some(MODEL.into()),
            persist: false,
            await_extensions: false,
            ..SessionConfig::default()
        })
        .await
        .expect("create_session");
    let journal = handle.journal_id();
    let (t, _snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    (t, journal)
}

/// Deliver one event to the session's socket, as `synaps send` does.
async fn send_event(t: &LocalTransport, id: &str) {
    let mut ev = synaps_cli::events::types::Event::simple("budget-test", "wake up", None);
    ev.id = id.into();
    let path = synaps_cli::events::registry::find_session_registration(t.session_id().as_str())
        .expect("published session endpoint")
        .socket_path;
    use tokio::io::AsyncWriteExt;
    let mut s = tokio::net::UnixStream::connect(&path)
        .await
        .expect("session socket");
    s.write_all(serde_json::to_string(&ev).unwrap().as_bytes())
        .await
        .unwrap();
    s.shutdown().await.unwrap();
}

async fn until_turn_started(t: &mut LocalTransport) {
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("actor hung")
            .expect("alive");
        if matches!(env.event, SessionEventWire::TurnStarted { .. }) {
            return;
        }
    }
}

/// `n` tool rounds, then a plain answer.
fn tool_rounds_then_done(n: usize) -> &'static [&'static str] {
    let mut bodies = vec![ANTHROPIC_SSE_TOOL_USE; n];
    bodies.push(ANTHROPIC_SSE);
    Box::leak(bodies.into_boxed_slice())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_event_turn_waiting_for_on_session_start_runs_under_the_event_budget_and_a_typed_turn_after_it_does_not() {
    let _guard = HomeGuard::new();
    // 24 rounds for the event turn (its budget), then 30 tool rounds and
    // the answer for the typed turn. An event turn without its budget would
    // run through all of them.
    let (url, hits, _) = spawn_stub(Script::SeqSse(tool_rounds_then_done(24 + 30))).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let hold = start_seam::hold_gate(&host).await;
    let (mut t, journal) = deferred_start_session(&host).await;

    send_event(&t, "evt-budget-gated").await;
    until_turn_started(&mut t).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the event turn's request waits for on_session_start");
    assert_eq!(start_seam::calls(&journal), 0, "the hook waits behind the held gate");

    drop(hold);
    let (outcome, history) = drive_to_idle(&mut t).await;
    assert_eq!(start_seam::calls(&journal), 1, "the hook ran once, before the request");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        24,
        "the event turn stops at 24 rounds after waiting for the hook"
    );
    assert_eq!(
        outcome,
        Some(TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ProviderRounds
        })
    );
    assert_history_pairing(&history);

    t.send(SessionCommand::Submit {
        text: "do thirty things".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    let (outcome, history) = drive_to_idle(&mut t).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        24 + 31,
        "the typed turn after it: 30 tool rounds and the answer"
    );
    assert_eq!(outcome, None, "no budget stop for the typed turn");
    assert_history_pairing(&history);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_typed_turn_waiting_for_on_session_start_is_not_bound_by_the_event_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(THIRTY_TOOL_ROUNDS_THEN_DONE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let hold = start_seam::hold_gate(&host).await;
    let (mut t, journal) = deferred_start_session(&host).await;

    t.send(SessionCommand::Submit {
        text: "do thirty things".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    until_turn_started(&mut t).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the typed turn's request waits for on_session_start");

    drop(hold);
    let (outcome, history) = drive_to_idle(&mut t).await;
    assert_eq!(start_seam::calls(&journal), 1, "the hook ran once, before the request");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        31,
        "30 tool rounds and the answer"
    );
    assert_eq!(outcome, None, "no budget stop for a typed turn");
    assert_history_pairing(&history);
}

/// `/budget` reply text (`EngineCommand`, served mid-turn too).
async fn budget(t: &mut LocalTransport, id: u64, arg: &str) -> String {
    t.send(SessionCommand::EngineCommand {
        id,
        name: "budget".into(),
        arg: arg.into(),
    })
    .await
    .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("actor hung")
            .expect("alive");
        if let SessionEventWire::QueryResult { id: i, value } = env.event {
            if i == id {
                return value.to_string();
            }
        }
    }
}

/// The event budget lives only in the stream's own copy. While an event turn
/// runs (here one whose request waited for the hook), the session's budget
/// stays the session's: `/budget status` shows it, and a `/budget time` set
/// mid-turn is still there once the event turn has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn during_an_event_turn_the_budget_command_sees_the_session_budget() {
    let _guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let hold = start_seam::hold_gate(&host).await;
    let (mut t, _journal) = deferred_start_session(&host).await;

    send_event(&t, "evt-budget-status").await;
    until_turn_started(&mut t).await;
    drop(hold);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while hits.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the event turn's request is in flight");

    let status = budget(&mut t, 41, "status").await;
    assert!(
        status.contains("provider rounds: off"),
        "the session's budget, not the event turn's: {status}"
    );
    let set = budget(&mut t, 42, "time 4h").await;
    assert!(set.contains("14400s"), "{set}");

    t.send(SessionCommand::Cancel).await.unwrap();
    drive_to_idle(&mut t).await;
    let status = budget(&mut t, 43, "status").await;
    assert!(
        status.contains("wall-clock: 14400s") && status.contains("provider rounds: off"),
        "the mid-turn change outlives the event turn: {status}"
    );
}
