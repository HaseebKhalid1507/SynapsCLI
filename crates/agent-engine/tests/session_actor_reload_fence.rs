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
    let _ = &f.bodies;
    end(&mut f.client).await;
}
