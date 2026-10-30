//! Watching and steering a session's subagent from its own thread view:
//! `SessionQuery::SubagentWatch` pages a worker's journal, and
//! `SessionCommand::SubagentSteer` steers it (owner-only) while noting the
//! steer in the MAIN conversation without starting a turn.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_engine::runtime::subagent_watch::{WatchEvent, WatchPage};
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionEventWire, SessionQuery,
};
use agent_engine::tools::{SubagentHandle, SubagentState, SubagentStatus};
use serial_test::serial;
use tokio::sync::mpsc;

struct FakeWorker {
    state: Arc<RwLock<SubagentState>>,
    steered: mpsc::UnboundedReceiver<String>,
}

/// A session whose registry holds one fake running worker `sa_1` ("scout").
async fn session_with_worker() -> (agent_engine::session::SessionHandle, FakeWorker) {
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(exclusive_cfg()).await.unwrap();
    let state = Arc::new(RwLock::new(SubagentState::new()));
    let (tx, rx) = mpsc::unbounded_channel();
    task.subagent_registry()
        .lock()
        .unwrap()
        .register(SubagentHandle::new(
            "sa_1".into(),
            1,
            "scout".into(),
            "look around".into(),
            MODEL.into(),
            String::new(),
            300,
            Arc::clone(&state),
            Some(tx),
            None,
            None,
        ));
    tokio::spawn(task.run());
    (handle, FakeWorker { state, steered: rx })
}

async fn attach(handle: &agent_engine::session::SessionHandle, mode: AttachMode) -> LocalTransport {
    LocalTransport::attach_with(handle.clone(), ClientMeta::new(ClientKind::Tui), mode)
        .await
        .unwrap()
        .0
}

async fn watch(t: &mut LocalTransport, id: u64, handle_id: &str, since: u64) -> serde_json::Value {
    t.send(SessionCommand::Query {
        id,
        query: SessionQuery::SubagentWatch {
            handle_id: handle_id.into(),
            since,
            tool_queue: true,
        },
    })
    .await
    .unwrap();
    let seen = until(
        t,
        |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id),
    )
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn watch_pages_a_workers_journal_incrementally() {
    let _h = Home::new();
    let (handle, worker) = session_with_worker().await;
    let mut t = attach(&handle, AttachMode::Mirror).await;

    {
        let mut s = worker.state.write().unwrap();
        s.watch.push(WatchEvent::ResponseStart);
        s.watch.push(WatchEvent::Text {
            text: "Reading the files.".into(),
        });
    }
    let page: WatchPage = serde_json::from_value(watch(&mut t, 1, "sa_1", 0).await).unwrap();
    assert_eq!(page.handle_id, "sa_1");
    assert_eq!((page.subagent_id, page.agent_name.as_str()), (1, "scout"));
    assert_eq!(page.model, MODEL, "the page names the worker's model");
    assert_eq!(page.status, "running");
    assert!(page.steerable);
    assert_eq!(page.events.len(), 2, "{page:?}");

    // Only what is new comes back on the next page.
    worker.state.write().unwrap().watch.push(WatchEvent::Text {
        text: " Done.".into(),
    });
    let next: WatchPage =
        serde_json::from_value(watch(&mut t, 2, "sa_1", page.next_seq).await).unwrap();
    assert_eq!(
        next.events,
        vec![WatchEvent::Text {
            text: " Done.".into()
        }]
    );

    // A finished worker is still watchable but no longer steerable.
    worker.state.write().unwrap().status = SubagentStatus::Completed;
    let done: WatchPage =
        serde_json::from_value(watch(&mut t, 3, "sa_1", next.next_seq).await).unwrap();
    assert_eq!(done.status, "completed");
    assert!(!done.steerable);

    let missing = watch(&mut t, 4, "sa_404", 0).await;
    assert!(
        missing["error"].as_str().unwrap().contains("sa_404"),
        "{missing}"
    );
    end(&mut t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn owner_steer_reaches_the_worker_and_the_main_conversation_without_a_turn() {
    let _h = Home::new();
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (handle, mut worker) = session_with_worker().await;
    let mut owner = attach(&handle, AttachMode::Mirror).await;

    owner
        .send_from_self(SessionCommand::SubagentSteer {
            handle_id: "sa_1".into(),
            text: "check the tests first".into(),
        })
        .await
        .unwrap();
    let seen = until(&mut owner, |e| {
        matches!(e, SessionEventWire::Conversation(_))
    })
    .await;

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), worker.steered.recv())
            .await
            .unwrap(),
        Some("check the tests first".to_string()),
        "the worker got the user's text verbatim"
    );
    let ev = seen
        .iter()
        .find_map(|e| match &e.event {
            SessionEventWire::External(ev) => Some(ev.clone()),
            _ => None,
        })
        .expect("an External event card for every client");
    assert_eq!(ev.content.content_type, "subagent_steer");
    assert!(
        ev.content.text.contains("'scout' (sa_1)"),
        "{}",
        ev.content.text
    );
    assert!(ev.content.text.contains("check the tests first"));

    // The main agent will see it: a user-role <event> in the history.
    let snap = last_conversation(&seen);
    let last = snap.api_messages.last().expect("history");
    assert_eq!(last["role"], "user");
    let content = last["content"].as_str().unwrap();
    assert!(
        content.starts_with("<event") && content.contains("subagent_steer"),
        "{content}"
    );

    // ...but it does not start a turn on its own.
    let after = drain_for(&mut owner, Duration::from_millis(500)).await;
    assert!(
        !seen
            .iter()
            .chain(&after)
            .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })),
        "no auto-turn"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no model call"
    );
    end(&mut owner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn non_owners_and_finished_workers_are_refused() {
    let _h = Home::new();
    let (handle, mut worker) = session_with_worker().await;
    let mut owner = attach(&handle, AttachMode::Mirror).await;
    let mut viewer = attach(&handle, AttachMode::Mirror).await;

    // A second (non-owner) client can watch, but not steer.
    viewer
        .send_from_self(SessionCommand::SubagentSteer {
            handle_id: "sa_1".into(),
            text: "hijack".into(),
        })
        .await
        .unwrap();
    let seen = until(&mut viewer, |e| {
        matches!(e, SessionEventWire::Refused { .. })
    })
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { command, .. } => assert_eq!(command, "subagent_steer"),
        _ => unreachable!(),
    }
    let page = watch(&mut viewer, 1, "sa_1", 0).await;
    assert_eq!(page["status"], "running", "viewers may watch: {page}");

    // The owner can't steer a worker that already finished.
    worker.state.write().unwrap().status = SubagentStatus::Completed;
    owner
        .send_from_self(SessionCommand::SubagentSteer {
            handle_id: "sa_1".into(),
            text: "too late".into(),
        })
        .await
        .unwrap();
    // Refusals are broadcast; wait for the one addressed to the owner.
    let me = owner.client_id();
    let seen = until(
        &mut owner,
        |e| matches!(e, SessionEventWire::Refused { client, .. } if *client == me),
    )
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { reason, .. } => {
            assert!(reason.contains("not running"), "{reason}")
        }
        _ => unreachable!(),
    }
    assert!(
        worker.steered.try_recv().is_err(),
        "nothing reached the worker"
    );
    end(&mut owner).await;
}
