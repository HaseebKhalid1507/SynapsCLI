//! `/close` (engine command `close`): the owner ends the session in the
//! daemon instead of only detaching from it. It refuses while closing would
//! stop work in flight unless `force`; `force` cancels the running workers
//! and tells the conversation.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, EndReason, Envelope, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle,
};
use agent_engine::tools::{SubagentHandle, SubagentState};
use agent_engine::StreamEvent;
use serial_test::serial;

type Registry = Arc<std::sync::Mutex<agent_engine::runtime::subagent::SubagentRegistry>>;

/// A fake running worker `id` (no thread: a cancel only flags it).
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

async fn session(body: &'static str, endless: bool) -> (LocalTransport, SessionHandle, Registry) {
    let (url, _) = stub(body, endless).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(exclusive_cfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    (t, handle, registry)
}

/// `/close [arg]` as the TUI sends it: stamped with the client's id.
async fn close(t: &mut LocalTransport, id: u64, arg: &str) {
    t.send_from_self(SessionCommand::EngineCommand {
        id,
        name: "close".into(),
        arg: arg.into(),
    })
    .await
    .unwrap();
}

fn reply(seen: &[Envelope], want: u64) -> serde_json::Value {
    seen.iter()
        .find_map(|e| match &e.event {
            SessionEventWire::QueryResult { id, value } if *id == want => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no QueryResult {want}: {seen:#?}"))
}

fn ended(seen: &[Envelope]) -> Option<EndReason> {
    seen.iter().find_map(|e| match e.event {
        SessionEventWire::Ended { reason } => Some(reason),
        _ => None,
    })
}

/// An idle session closes: the reply says how to continue it, then the
/// session ends as a client quit, and every attached client hears `Ended`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_ends_an_idle_session_for_every_client() {
    let _h = Home::new();
    let (mut t, handle, _) = session(SSE_HI, false).await;
    t.send_from_self(submit("hi")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let (mut other, _) = LocalTransport::attach_with(
        handle.clone(),
        ClientMeta::new(ClientKind::Attach),
        AttachMode::Observe,
    )
    .await
    .unwrap();

    close(&mut t, 7, "").await;
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    let value = reply(&seen, 7);
    assert_eq!(value["kind"], "output", "{value}");
    let text = value["text"].as_str().unwrap();
    assert!(text.starts_with("closing session "), "{text}");
    assert!(text.contains("synaps --attach --continue "), "{text}");
    assert_eq!(ended(&seen), Some(EndReason::ClientQuit));
    let theirs = until(&mut other, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert_eq!(ended(&theirs), Some(EndReason::ClientQuit));
    assert!(tokio::time::timeout(Duration::from_secs(5), t.next_event())
        .await
        .unwrap()
        .is_none());
}

/// A running worker would die with the session: `/close` refuses, names
/// it, and leaves both the worker and the session alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_refuses_while_a_worker_runs() {
    let _h = Home::new();
    let (mut t, _handle, registry) = session(SSE_HI, false).await;
    t.send_from_self(submit("start the reviewer"))
        .await
        .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let state = register(&registry, "sa_4", "reviewer");
    // A worker that already finished holds nothing up.
    let done = register(&registry, "sa_2", "scout");
    done.write().unwrap().status = agent_engine::runtime::subagent::SubagentStatus::Completed;

    close(&mut t, 8, "").await;
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 8, .. })
    })
    .await;
    let value = reply(&seen, 8);
    assert_eq!(value["kind"], "error", "{value}");
    let text = value["text"].as_str().unwrap();
    assert!(
        text.contains("1 worker is running (sa_4 reviewer)"),
        "{text}"
    );
    assert!(
        !text.contains("sa_2"),
        "a finished worker is not a blocker: {text}"
    );
    assert!(text.contains("/close force"), "{text}");
    assert!(
        !state.read().unwrap().cancel_requested,
        "the worker keeps running"
    );
    let after = drain_for(&mut t, Duration::from_millis(300)).await;
    assert_eq!(ended(&after), None, "the session stays: {after:#?}");

    // Bad arguments are refused the same way.
    close(&mut t, 9, "now").await;
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 9, .. })
    })
    .await;
    assert_eq!(reply(&seen, 9)["text"], "usage: /close [force]");
    end(&mut t).await;
}

/// `force` cancels the running workers first and notes each one in the
/// conversation (whoever continues the session must not wait for them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_force_cancels_the_workers_and_tells_the_conversation() {
    let _h = Home::new();
    let (mut t, _handle, registry) = session(SSE_HI, false).await;
    t.send_from_self(submit("start the reviewer"))
        .await
        .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let state = register(&registry, "sa_4", "reviewer");

    close(&mut t, 10, "force").await;
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(
        state.read().unwrap().cancel_requested,
        "the worker was cancelled"
    );
    let note = seen
        .iter()
        .find_map(|e| match &e.event {
            SessionEventWire::External(ev) if ev.content.content_type == "subagent_cancelled" => {
                Some(ev.content.text.clone())
            }
            _ => None,
        })
        .expect("the conversation is told");
    assert!(
        note.contains("/close force") && note.contains("'reviewer' (sa_4)"),
        "{note}"
    );
    let conv = last_conversation(&seen);
    let last = conv.api_messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].to_string().contains("sa_4"), "{last}");
    assert_eq!(reply(&seen, 10)["kind"], "output");
    assert_eq!(ended(&seen), Some(EndReason::ClientQuit));
}

/// Mid-turn, `/close` refuses (Esc first, or force); `force` ends the
/// session and the interrupted turn is kept like any other quit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_refuses_mid_turn_and_force_ends_it() {
    let _h = Home::new();
    let (mut t, _handle, _) = session(SSE_PREFIX, true).await;
    t.send_from_self(submit("go")).await.unwrap();
    until(&mut t, |e| {
        matches!(e, SessionEventWire::Stream(StreamEvent::Llm(_)))
    })
    .await;

    close(&mut t, 11, "").await;
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 11, .. })
    })
    .await;
    let value = reply(&seen, 11);
    assert_eq!(value["kind"], "error", "{value}");
    assert!(
        value["text"]
            .as_str()
            .unwrap()
            .contains("a turn is running"),
        "{value}"
    );

    close(&mut t, 12, "force").await;
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert_eq!(reply(&seen, 12)["kind"], "output");
    assert_eq!(ended(&seen), Some(EndReason::ClientQuit));
}

/// Only the input owner may close; anyone else is `Refused` as "close".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn only_the_input_owner_can_close() {
    let _h = Home::new();
    let (mut owner, handle, _) = session(SSE_HI, false).await;
    let (mut other, _) = LocalTransport::attach_with(
        handle.clone(),
        ClientMeta::new(ClientKind::Attach),
        AttachMode::Mirror,
    )
    .await
    .unwrap();
    assert_eq!(other.input_owner(), Some(owner.client_id()));

    close(&mut other, 13, "").await;
    let seen = until(&mut other, |e| {
        matches!(e, SessionEventWire::Refused { .. })
    })
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused {
            client,
            command,
            reason,
        } => {
            assert_eq!(*client, other.client_id());
            assert_eq!(command, "close");
            assert!(reason.contains("input owned by"), "{reason}");
        }
        _ => unreachable!(),
    }
    let after = drain_for(&mut owner, Duration::from_millis(300)).await;
    assert_eq!(ended(&after), None, "the session stays: {after:#?}");
    // The owner can, and is told a session that never ran a turn has
    // nothing to resume.
    close(&mut owner, 15, "").await;
    let seen = until(&mut owner, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    let text = reply(&seen, 15)["text"].as_str().unwrap().to_string();
    assert!(text.ends_with("(it never ran a turn, so there is nothing to resume)"), "{text}");
    assert!(!text.contains("--continue"), "{text}");
}

/// A parked session (no runtime, nothing running) closes without being
/// rebuilt: the host's close never unparks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_session_closes_without_unparking() {
    let _h = Home::new();
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let (url, _) = stub(SSE_HI, false).await;
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
    t.send(submit("hi")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handle.lifecycle() != SessionLifecycle::Parked && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);

    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::EngineCommand {
            id: 14,
            name: "close".into(),
            arg: String::new(),
        })
        .await
        .unwrap();
    let mut lifecycles = Vec::new();
    let mut value = None;
    let reason = loop {
        let env = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match env.event {
            SessionEventWire::Lifecycle(l) => lifecycles.push(l),
            SessionEventWire::QueryResult { id: 14, value: v } => value = Some(v),
            SessionEventWire::Ended { reason } => break reason,
            _ => {}
        }
    };
    assert_eq!(value.expect("a reply")["kind"], "output");
    assert_eq!(reason, EndReason::ClientQuit);
    assert!(
        !lifecycles.contains(&SessionLifecycle::Live),
        "closing never rebuilt the session: {lifecycles:?}"
    );
}
