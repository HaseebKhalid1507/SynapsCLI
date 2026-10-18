//! What the user does to a session's workers, and what each side is told:
//! `/workers cancel` tells the MAIN agent (it may be waiting for a wake that
//! will never come), and the Esc notice counts only the background workers
//! that really keep running.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, RwLock};

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionEventWire,
};
use agent_engine::tools::{SubagentHandle, SubagentState};
use agent_engine::{SessionEvent, StreamEvent};
use serial_test::serial;

type Registry = Arc<std::sync::Mutex<agent_engine::runtime::subagent::SubagentRegistry>>;

/// A fake running worker `id` (no thread: a cancel only flags it, and it
/// keeps reading as running, like a real worker between the cancel and
/// its next step).
fn register(registry: &Registry, id: &str, agent: &str) -> Arc<RwLock<SubagentState>> {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        id.into(),
        id.trim_start_matches("sa_").parse().unwrap(),
        agent.into(),
        "work".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    state
}

async fn session(body: &'static str, endless: bool) -> (LocalTransport, Registry) {
    let (url, _) = stub(body, endless).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (t, _) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    (t, registry)
}

/// `/workers cancel sa_N` withholds the worker's completion wake. The main
/// agent that dispatched it in an earlier turn must hear it was cancelled,
/// or it waits for a wake that never comes (or re-dispatches the work).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn workers_cancel_tells_the_main_agent() {
    let _h = Home::new();
    let (mut t, registry) = session(SSE_HI, false).await;
    t.send(submit("start the reviewer")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let state = register(&registry, "sa_4", "reviewer");

    t.send(SessionCommand::EngineCommand { id: 9, name: "workers".into(), arg: "cancel sa_4".into() })
        .await
        .unwrap();
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::External(ev) if ev.content.content_type == "subagent_cancelled")
    })
    .await;
    assert!(state.read().unwrap().cancel_requested);
    let SessionEventWire::External(ev) = &seen.last().unwrap().event else { unreachable!() };
    assert!(ev.content.text.contains("'reviewer' (sa_4)"), "{}", ev.content.text);
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));

    // It is in the main conversation (for the next turn), not a turn of its own.
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    let conv = last_conversation(&seen);
    let last = conv.api_messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let text = last["content"].to_string();
    assert!(text.contains("sa_4") && text.contains("cancel"), "{text}");
    end(&mut t).await;
}

/// Esc cancels the interrupted turn's own workers; background workers from
/// earlier turns keep running and the notice counts those — not the ones
/// this Esc just cancelled, which read as running until their next step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_esc_notice_counts_only_the_workers_left_running() {
    let _h = Home::new();
    let (mut t, registry) = session(SSE_PREFIX, true).await;
    let background = register(&registry, "sa_1", "watcher");
    t.send(submit("go")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Stream(StreamEvent::Llm(_)))).await;
    // Launched by the turn about to be interrupted.
    let this_turn = register(&registry, "sa_2", "helper");

    t.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(this_turn.read().unwrap().cancel_requested, "the turn's own worker stops");
    assert!(!background.read().unwrap().cancel_requested, "the background one keeps running");
    let notices: Vec<&String> = seen
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(n))) => Some(n),
            _ => None,
        })
        .filter(|n| n.contains("background worker"))
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("1 background worker from earlier turns still running"),
        "{}",
        notices[0]
    );
    end(&mut t).await;
}
