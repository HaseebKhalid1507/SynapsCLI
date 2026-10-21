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

/// A thread-less worker handle over `state` (a cancel only flags it).
fn fake_worker(f: &Fixture, id: &str, agent: &str, state: &Arc<RwLock<SubagentState>>) {
    f.registry.lock().unwrap().register(SubagentHandle::new(
        id.into(),
        id.trim_start_matches("sa_").parse().unwrap(),
        agent.into(),
        "work".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(state),
        None,
        None,
        None,
    ));
}

/// Settle a fake worker's finalizer the way `finalize_subagent` does.
fn settle(state: &Arc<RwLock<SubagentState>>) {
    let mut s = state.write().unwrap();
    if s.cancel_requested {
        s.status = SubagentStatus::Cancelled;
        s.archive_path = Some("/archives/fake.json".into());
    }
    s.finished_at.get_or_insert_with(std::time::Instant::now);
    s.finalization_started = true;
    s.finalization_ready = true;
    s.completion_settled = true;
}

/// `sa_7` finished on its own, but its finalizer has not settled: the
/// checkpoint waits for it, and it pushes its completion wake into the
/// queue while the checkpoint runs. Deterministic: a running sentinel
/// worker `sa_6` is stopped by the checkpoint's `stop_for_reload`, and only
/// then does `sa_7`'s finalizer push (no sleep racing the dequeue).
fn worker_finishing_during_the_checkpoint(f: &Fixture) {
    let sentinel = Arc::new(RwLock::new(SubagentState::new()));
    fake_worker(f, "sa_6", "sentinel", &sentinel);
    let state = Arc::new(RwLock::new(SubagentState::new()));
    {
        let mut s = state.write().unwrap();
        s.status = SubagentStatus::Completed;
        s.finished_at = Some(std::time::Instant::now());
    }
    fake_worker(f, "sa_7", "scout", &state);
    let queue = Arc::clone(&f.queue);
    std::thread::spawn(move || {
        while !sentinel.read().unwrap().cancel_requested {
            std::thread::sleep(Duration::from_millis(5));
        }
        settle(&sentinel);
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
        settle(&state);
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
    fake_worker(f, "sa_8", "digger", &state);
    std::thread::spawn(move || {
        while !state.read().unwrap().cancel_requested {
            std::thread::sleep(Duration::from_millis(5));
        }
        settle(&state);
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
    let mut seen = until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await);
    // The user is told the reload failed — never that it happened.
    let notices: Vec<&String> = seen
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::SystemNotice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(notices.iter().any(|n| n.starts_with("daemon reload failed")), "{notices:?}");
    assert!(notices.iter().any(|n| n.contains("sa_8")), "the stopped worker is named: {notices:?}");
    assert!(!notices.iter().any(|n| n.contains("daemon reloaded")), "{notices:?}");
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

fn refusals(seen: &[Envelope]) -> Vec<(String, String)> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Refused { command, reason, .. } => Some((command.clone(), reason.clone())),
            _ => None,
        })
        .collect()
}

/// The fence covers commands too. A client that missed the checkpoint's
/// cancel sends `Steer`, which with no turn running starts one: under the
/// fence that turn would run after the record, and any worker it launched
/// would be stopped at birth and never announced. Every command that starts
/// a turn or changes what the record captured is refused, and the user is
/// told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn commands_that_start_work_are_refused_while_fenced() {
    let _h = Home::new();
    let mut f = fixture().await;
    checkpoint_for_reload(&mut f).await;

    for cmd in [
        SessionCommand::Steer { text: "and also".into() },
        submit("hello again"),
        SessionCommand::NewSession,
        SessionCommand::Resume { id: 3, query: "other".into() },
        SessionCommand::Compact { instructions: None },
        SessionCommand::EngineCommand { id: 4, name: "memory".into(), arg: "off".into() },
    ] {
        f.client.send_from_self(cmd).await.unwrap();
    }
    let window = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&window), 0, "a turn started under the fence: {window:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    let refused = refusals(&window);
    let names: Vec<&str> = refused.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        names,
        ["steer", "submit", "new_session", "resume", "compact", "engine_command"],
        "{window:#?}"
    );
    assert!(refused.iter().all(|(_, r)| r == "daemon reloading; retry in a moment"), "{refused:?}");
    assert!(
        !window.iter().any(|e| matches!(e.event, SessionEventWire::Cleared { .. })),
        "the conversation was replaced under the fence"
    );

    // Lifted: the same Steer starts its turn.
    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    f.client.send_from_self(SessionCommand::Steer { text: "and also".into() }).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut f.client).await;
}

/// A worker registered after the checkpoint took its snapshot (whatever
/// launched it) is stopped at birth by the registry's reload fence. A failed
/// exec announces it from the live registry, exactly once — not only the
/// workers the snapshot knew.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_exec_announces_a_worker_registered_after_the_snapshot() {
    let _h = Home::new();
    let mut f = fixture().await;
    let workers = checkpoint_for_reload(&mut f).await;
    assert!(workers.iter().all(|w| w.id != "sa_9"));

    let late = Arc::new(RwLock::new(SubagentState::new()));
    fake_worker(&f, "sa_9", "latecomer", &late);
    {
        let s = late.read().unwrap();
        assert!(s.cancel_requested && s.stopped_by_reload, "stopped at birth by the reload");
    }
    settle(&late);

    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    let later = drain_for(&mut f.client, Duration::from_millis(500)).await;
    assert_eq!(started(&later), 0);
    let sa9 = mentions(&f.bodies, "sa_9");
    assert_eq!(sa9.len(), 1, "{sa9:#?}");
    assert!(sa9[0].contains("by a daemon reload, not by the user"), "{sa9:#?}");
    end(&mut f.client).await;
}

/// A reload checkpoint is the daemon's: from a client it would stop every
/// worker and fence the session for an exec that never comes. Refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_client_cannot_send_a_reload_checkpoint() {
    let _h = Home::new();
    let mut f = fixture().await;
    let worker = Arc::new(RwLock::new(SubagentState::new()));
    fake_worker(&f, "sa_3", "busy", &worker);

    f.client
        .send_from_self(SessionCommand::Checkpoint { reason: CheckpointReason::Reload })
        .await
        .unwrap();
    let seen = until(&mut f.client, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    let refused = refusals(&seen);
    assert_eq!(refused[0].0, "checkpoint", "{refused:?}");
    assert!(!worker.read().unwrap().cancel_requested, "its workers keep running");
    assert!(!seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID
    )));
    // Not fenced: the next turn runs.
    f.client.send_from_self(submit("still here")).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut f.client).await;
}

// ── The reload fence × a pending `on_session_start` ─────────────────────────
//
// Both hold back autonomous work: the fence until the exec (or the daemon's
// report that it failed), the hook until it has run. A session that is
// fenced AND waits for its hook sends no request for an event until both
// have cleared, in either order. Once the fence lifts, the event's turn may
// start while its request waits for the hook: a pending hook does not hold
// the queue wake, and the turn's stream opens only after the hook. The hook
// itself may run under the fence. Two test seams make the hook pending
// after the fixture's turn: the host's session-start gate is held, and that
// turn puts a fresh hook back in line at its end.
//
// What the timed drains show: "nothing started within 800 ms" (and the
// 500 ms drain after the work) are negative observations over that window,
// not barriers. The counts read after `Idle` check that the work ran, once
// within what the test saw; they do not prove a second turn can never come.

use agent_engine::session::session_start_testing as start_seam;

/// `fixture()`, with this session's `on_session_start` back in line behind
/// the held gate once its turn has ended. Returns the hold, the journal id
/// and the hook-call count after the turn.
async fn fixture_with_a_pending_hook() -> (Fixture, start_seam::GateHold, String, u64) {
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
    let hold = start_seam::hold_gate(&host).await;
    let sid = handle.journal_id();
    let calls = start_seam::calls(&sid);
    start_seam::requeue_after_next_turn(&sid);
    client.send(submit("hello")).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(start_seam::calls(&sid), calls, "the hook waits behind the held gate");
    (Fixture { handle, client, registry, queue, hits, bodies }, hold, sid, calls)
}

/// An event for the session, as its inbox watcher queues one.
fn queue_event(f: &Fixture) {
    f.queue
        .push(agent_engine::events::types::Event::simple("test", "wake up", None))
        .unwrap();
}

/// Wait (bounded) until `n` hooks of `sid` have run since `base`.
async fn hooks_run(sid: &str, base: u64, n: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if start_seam::calls(sid) >= base + n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// The fence lifts first: the event's turn may start, but its request waits
/// for the hook. Once the hook has run, the event gets one turn and one
/// request, and the hook ran once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_event_for_a_fenced_session_waiting_for_its_hook_runs_once_both_clear_fence_first() {
    let _h = Home::new();
    let (mut f, hold, sid, calls) = fixture_with_a_pending_hook().await;
    checkpoint_for_reload(&mut f).await;
    queue_event(&f);
    let fenced = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&fenced), 0, "a turn started under the fence: {fenced:#?}");

    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let lifted = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "no request while the hook is pending");
    assert_eq!(start_seam::calls(&sid), calls, "the hook still waits");

    drop(hold);
    let seen = until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(started(&lifted) + started(&seen), 1, "one turn for the event");
    assert_eq!(f.hits.load(Ordering::SeqCst), 2, "one request for the event");
    assert_eq!(start_seam::calls(&sid), calls + 1, "the hook ran once");
    let after = drain_for(&mut f.client, Duration::from_millis(500)).await;
    assert_eq!(started(&after), 0, "{after:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
    end(&mut f.client).await;
}

/// The hook runs first, under the fence: still nothing starts. Once the
/// fence lifts, the event gets one turn and one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_event_for_a_fenced_session_waiting_for_its_hook_runs_once_both_clear_hook_first() {
    let _h = Home::new();
    let (mut f, hold, sid, calls) = fixture_with_a_pending_hook().await;
    checkpoint_for_reload(&mut f).await;
    queue_event(&f);

    drop(hold);
    assert!(hooks_run(&sid, calls, 1).await, "the hook runs under the fence");
    let fenced = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&fenced), 0, "a turn started under the fence: {fenced:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "no request under the fence");

    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let seen = until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(started(&seen), 1, "one turn for the event");
    assert_eq!(f.hits.load(Ordering::SeqCst), 2, "one request for the event");
    assert_eq!(start_seam::calls(&sid), calls + 1, "the hook ran once");
    let after = drain_for(&mut f.client, Duration::from_millis(500)).await;
    assert_eq!(started(&after), 0, "{after:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
    end(&mut f.client).await;
}

/// A Submit while fenced and waiting for the hook: the fence's refusal comes
/// first (the reload record must not change). Once the fence has lifted, a
/// Submit starts its turn and its request waits for the hook; then one
/// request goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_submit_while_fenced_and_waiting_for_the_hook_is_refused_and_then_waits_for_the_hook() {
    let _h = Home::new();
    let (mut f, hold, sid, calls) = fixture_with_a_pending_hook().await;
    checkpoint_for_reload(&mut f).await;

    f.client.send_from_self(submit("hello again")).await.unwrap();
    let fenced = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&fenced), 0, "a turn started under the fence: {fenced:#?}");
    assert_eq!(
        refusals(&fenced),
        [("submit".to_string(), "daemon reloading; retry in a moment".to_string())],
        "{fenced:#?}"
    );

    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    f.client.send_from_self(submit("hello again")).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "the request waits for the hook");
    assert_eq!(start_seam::calls(&sid), calls, "the hook still waits");

    drop(hold);
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 2, "one request once the hook has run");
    assert_eq!(start_seam::calls(&sid), calls + 1, "the hook ran once");
    end(&mut f.client).await;
}

/// While a turn waits for the hook, the client is told why nothing happens
/// yet: a Quick Start first turn can wait for extension discovery and the
/// hook for many seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_waiting_for_the_hook_says_so() {
    let _h = Home::new();
    let (mut f, hold, _sid, _calls) = fixture_with_a_pending_hook().await;
    f.client.send(submit("hello again")).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    let waiting = drain_for(&mut f.client, Duration::from_millis(500)).await;
    assert!(
        waiting.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::SystemNotice(n) if n.contains("waiting for extensions")
        )),
        "no notice while the turn waits for the hook: {waiting:#?}"
    );
    drop(hold);
    until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut f.client).await;
}

/// `/clear` while a turn waits for the hook cancels that turn first. Its
/// request never goes out, neither on the cleared history nor under the new
/// journal id, and nothing starts once the hook has run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_clear_while_the_turn_waits_for_the_hook_cancels_the_turn() {
    let _h = Home::new();
    let (mut f, hold, sid, calls) = fixture_with_a_pending_hook().await;
    f.client.send(submit("hello again")).await.unwrap();
    until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;

    f.client.send(SessionCommand::NewSession).await.unwrap();
    let seen = until(&mut f.client, |e| matches!(e, SessionEventWire::Cleared { .. })).await;
    assert!(
        seen.iter().any(|e| matches!(e.event, SessionEventWire::Aborted { .. })),
        "the waiting turn ends before the clear: {seen:#?}"
    );

    // The hook is keyed on the session's current journal id: it runs for
    // the new conversation.
    let new_id = seen
        .iter()
        .find_map(|e| match &e.event {
            SessionEventWire::Cleared { session_id } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap();
    assert_ne!(new_id, sid);
    drop(hold);
    assert!(hooks_run(&new_id, 0, 1).await, "the hook still runs, for the new id");
    assert_eq!(start_seam::calls(&sid), calls, "not under the old id");
    let after = drain_for(&mut f.client, Duration::from_millis(800)).await;
    assert_eq!(started(&after), 0, "{after:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "the canceled turn sent no request");
    end(&mut f.client).await;
}
