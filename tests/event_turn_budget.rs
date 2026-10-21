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
    synaps_cli::events::socket::send_to_session_socket(&path, serde_json::to_string(&ev).unwrap().as_bytes())
        .await
        .expect("session socket");
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
