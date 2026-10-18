//! A daemon reload's checkpoint fences the session until the `execv`:
//! nothing may start a turn (and so launch an unrecorded worker) between
//! the reload record and the exec. A wake that lands during the checkpoint
//! stays queued — the record already carries it to the next image. When
//! the exec fails, the fence lifts and the same image delivers it.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use agent_engine::runtime::subagent::CarriedWorker;
use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, SessionCommand,
    SessionEventWire,
};
use agent_engine::tools::{build_completion_event, SubagentHandle, SubagentState, SubagentStatus};
use axum::response::IntoResponse;
use serde_json::Value;
use serial_test::serial;

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Every request answered `SSE_HI`; bodies recorded.
async fn capture_stub() -> (String, Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Bodies = Arc::default();
    let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (hits, seen) = (Arc::clone(&hits_c), Arc::clone(&seen_c));
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                seen.lock().unwrap().push(v);
            }
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                SSE_HI.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

fn started(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .count()
}

struct Fixture {
    handle: agent_engine::session::SessionHandle,
    client: LocalTransport,
    registry: Arc<std::sync::Mutex<agent_engine::runtime::subagent::SubagentRegistry>>,
    queue: Arc<agent_engine::events::EventQueue>,
    hits: Arc<AtomicUsize>,
    bodies: Bodies,
}

/// A session with one finished turn in its history.
async fn fixture() -> Fixture {
    let (url, hits, bodies) = capture_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let registry = task.subagent_registry();
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    client.send(submit("hello")).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    Fixture { handle, client, registry, queue, hits, bodies }
}

/// `sa_7` finished on its own, but its finalizer has not settled: the
/// checkpoint waits for it, and it pushes its completion wake into the
/// queue while the checkpoint runs.
fn worker_finishing_during_the_checkpoint(f: &Fixture) {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.status = SubagentStatus::Completed;
        s.finished_at = Some(std::time::Instant::now());
    }
    f.registry.lock().unwrap().register(SubagentHandle::new(
        "sa_7".into(),
        7,
        "scout".into(),
        "look around".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    let queue = Arc::clone(&f.queue);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let ev = build_completion_event(
            "sa_7",
            7,
            "scout",
            &SubagentStatus::Completed,
            "SA7-RESULT",
            1.0,
            None,
        );
        queue.push(ev).unwrap();
        let mut s = state.write().unwrap();
        s.finalization_started = true;
        s.finalization_ready = true;
        s.completion_settled = true;
    });
}

async fn checkpoint_for_reload(f: &mut Fixture) -> Vec<CarriedWorker> {
    f.handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload })
        .await
        .unwrap();
    let seen = until(&mut f.client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert_eq!(started(&seen), 0, "no turn during the checkpoint: {seen:#?}");
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    serde_json::from_value(value["record"]["workers"].clone()).unwrap()
}

/// The wake that lands during the checkpoint starts no turn before the
/// exec: the record carries it (`wake_lost`), and the exec would discard
/// a turn started now. A failed exec lifts the fence: the same image then
/// runs the turn the wake asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_wake_queued_by_the_reload_checkpoint_starts_no_turn_until_the_exec_fails() {
    let _h = Home::new();
    let mut f = fixture().await;
    worker_finishing_during_the_checkpoint(&f);

    let workers = checkpoint_for_reload(&mut f).await;
    let w = workers.iter().find(|w| w.id == "sa_7").expect("sa_7 carried");
    assert!(w.wake_lost && w.needs_wake(), "the next image owes its wake: {w:?}");

    // Between the record and the exec: fenced.
    let window = drain_for(&mut f.client, Duration::from_millis(1500)).await;
    assert_eq!(started(&window), 0, "a turn started before the exec: {window:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "no provider round before the exec");
    assert!(!f.queue.is_empty(), "the wake stays queued for the record");

    // The exec failed: the old image keeps serving, the wake is delivered.
    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let seen = until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    assert_eq!(started(&seen), 1);
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
    end(&mut f.client).await;
}

/// `sa_8` is running: the reload stops it. Its "finalizer" settles as soon
/// as the stop reaches it, withholding its completion wake as a cancel does.
fn worker_running_into_the_reload(f: &Fixture) {
    let state = Arc::new(RwLock::new(SubagentState::new()));
    f.registry.lock().unwrap().register(SubagentHandle::new(
        "sa_8".into(),
        8,
        "digger".into(),
        "dig".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(20));
        let mut s = state.write().unwrap();
        if s.cancel_requested {
            s.status = SubagentStatus::Cancelled;
            s.finished_at = Some(std::time::Instant::now());
            s.archive_path = Some("/archives/sa_8.json".into());
            s.finalization_started = true;
            s.finalization_ready = true;
            s.completion_settled = true;
            return;
        }
    });
}

/// Every wake in the last provider request's user turns that names
/// worker `id` (adjacent user messages are merged on the wire, so count
/// the wakes, not the messages).
fn mentions(bodies: &Bodies, id: &str) -> Vec<String> {
    let bodies = bodies.lock().unwrap();
    let last = bodies.last().expect("a request");
    let text: String = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect();
    text.split("Subagent '")
        .skip(1)
        .filter(|wake| wake.contains(&format!("({id})")))
        .map(str::to_string)
        .collect()
}

/// A failed exec delivers each worker's news to its parent exactly once.
/// The completion `sa_7` queued during the checkpoint is still queued in
/// this image (only the exec would have discarded it): it is not re-posted
/// from the record. `sa_8`, which the reload itself stopped (its finalizer
/// withheld the wake), is announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_exec_wakes_the_parent_once_per_worker() {
    let _h = Home::new();
    let mut f = fixture().await;
    worker_finishing_during_the_checkpoint(&f);
    worker_running_into_the_reload(&f);

    let workers = checkpoint_for_reload(&mut f).await;
    let by = |id: &str| workers.iter().find(|w| w.id == id).unwrap().clone();
    assert!(by("sa_7").wake_lost && by("sa_7").needs_wake());
    assert!(by("sa_8").stopped_by_reload && by("sa_8").needs_wake());

    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    // No second wake turn for a re-posted completion either.
    let later = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&later), 0, "{later:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);

    let sa7 = mentions(&f.bodies, "sa_7");
    assert_eq!(sa7.len(), 1, "sa_7's completion, once: {sa7:#?}");
    assert!(sa7[0].contains("SA7-RESULT"), "the original wake: {sa7:#?}");
    let sa8 = mentions(&f.bodies, "sa_8");
    assert_eq!(sa8.len(), 1, "sa_8's stop, once: {sa8:#?}");
    assert!(sa8[0].contains("by a daemon reload, not by the user"), "{sa8:#?}");
    end(&mut f.client).await;
}
