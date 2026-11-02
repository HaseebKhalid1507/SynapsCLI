//! Child spend in the session actor (P1g): what the session's subagents
//! book reaches the session through its spend sink, with or without a turn.
//! It is booked into the inclusive totals and the child share (never as a
//! Usage event or a request of the session's own), shown in status and the
//! Conversation event, and a session does not park or end idle while one of
//! its workers still reports spend (rule A).

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::runtime::child_spend::{
    BookOutcome, SpendKind, SpendReport, SpendSink, SpendTokens,
};
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire, SessionHandle, SessionLifecycle, SessionQuery,
};
use agent_engine::{SessionEvent, StreamEvent};
use serial_test::serial;

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

/// Sets an env var for the test and removes it after.
struct Env(&'static str);
impl Env {
    fn set(key: &'static str, v: &str) -> Self {
        std::env::set_var(key, v);
        Self(key)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

fn report(input: u64, output: u64, cost_nanousd: u64) -> SpendReport {
    SpendReport {
        kind: SpendKind::Turn,
        model: "anthropic/claude-sonnet-4-5".to_string(),
        tokens: SpendTokens {
            input,
            output,
            ..SpendTokens::default()
        },
        cost_nanousd,
    }
}

fn is_usage(e: &SessionEventWire) -> bool {
    matches!(
        e,
        SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Usage { .. }))
    )
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

async fn detach(t: &mut LocalTransport) {
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
}

async fn status(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    t.send(SessionCommand::Query {
        id,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

/// Poll `handle.lifecycle()` until `want` (≤ `for_`), returning whether seen.
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

/// Status, polled until `pred` holds (≤ 5 s); the last status either way.
async fn status_until(
    t: &mut LocalTransport,
    first_id: u64,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut id = first_id;
    loop {
        let st = status(t, id).await;
        if pred(&st) || tokio::time::Instant::now() >= deadline {
            return st;
        }
        id += 1;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A session idle after one turn; the spend sink its runtimes launch under.
async fn idle_session(cfg: SessionConfig) -> (SessionHandle, SpendSink, LocalTransport) {
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg).await.unwrap();
    let sink = task.spend_sink();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    (handle, sink, a)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c1_a_report_while_idle_is_booked_shown_and_is_not_a_usage_event() {
    let _h = Home::new();
    let (_handle, sink, mut a) = idle_session(cfg()).await;
    let before = status(&mut a, 1).await;
    assert_eq!(before["tokens"]["input"], 10);
    assert_eq!(before["tokens"]["output"], 1);
    assert_eq!(before["child_cost"], 0.0);
    assert!(
        before.get("spend_reporting_workers").is_none(),
        "absent with no worker: {before}"
    );
    let cost_before = before["cost"].as_f64().unwrap();

    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    assert!(
        !seen.iter().any(|e| is_usage(&e.event)),
        "child spend is never a Usage event: {:?}",
        seen.iter().map(|e| format!("{:?}", e.event)).collect::<Vec<_>>()
    );
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })),
        "booking starts no turn"
    );
    let conv = last_conversation(&seen);
    assert_eq!(conv.tokens.input, 110);
    assert_eq!(conv.tokens.output, 21);
    assert!((conv.cost - (cost_before + 0.005)).abs() < 1e-9, "{}", conv.cost);
    assert_eq!(conv.api_messages.len(), 2, "no message of its own");

    let st = status(&mut a, 2).await;
    assert_eq!(st["tokens"]["input"], 110);
    assert_eq!(st["tokens"]["output"], 21);
    assert!((st["cost"].as_f64().unwrap() - (cost_before + 0.005)).abs() < 1e-9);
    assert!((st["child_cost"].as_f64().unwrap() - 0.005).abs() < 1e-12, "{st}");
    assert_eq!(
        st["child_tokens"],
        serde_json::json!({"input": 100, "output": 20, "cache_read": 0, "cache_creation": 0})
    );
    assert_eq!(st["messages"], 2);
    assert_eq!(st["spend_reporting_workers"], 1, "{st}");

    drop(lease);
    let st = status_until(&mut a, 3, |st| st.get("spend_reporting_workers").is_none()).await;
    assert!(st.get("spend_reporting_workers").is_none(), "gone at 0: {st}");
    // The run's end books nothing.
    assert_eq!(st["tokens"]["input"], 110);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c2_two_runs_of_one_handle_each_book_all_their_reports() {
    // A resume is a new run of the same handle (a new nonce): the session's
    // dedupe keys on both, so neither run's reports read as duplicates of
    // the other's. (Duplicates and gaps themselves cannot be produced
    // through a lease; the dedupe's verdicts are unit-tested, q1-q5.)
    let _h = Home::new();
    let (_handle, sink, mut a) = idle_session(cfg()).await;
    let first = sink.lease("sa_1", "scout").unwrap();
    let resumed = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(first.book(report(1, 0, 1_000)), BookOutcome::Booked { seq: 0 });
    assert_eq!(resumed.book(report(2, 0, 2_000)), BookOutcome::Booked { seq: 0 });
    assert_eq!(first.book(report(4, 0, 4_000)), BookOutcome::Booked { seq: 1 });
    let st = status_until(&mut a, 1, |st| st["child_tokens"]["input"] == 7).await;
    assert_eq!(st["child_tokens"]["input"], 7, "every report of both runs: {st}");
    assert_eq!(st["spend_reporting_workers"], 2);
    drop(first);
    drop(resumed);
    let st = status_until(&mut a, 20, |st| st.get("spend_reporting_workers").is_none()).await;
    assert!(st.get("spend_reporting_workers").is_none(), "{st}");
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c3_no_idle_park_while_a_worker_reports_and_the_cost_survives_the_park() {
    let _h = Home::new();
    let _g = Env::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let (handle, sink, mut a) = idle_session(pcfg()).await;
    let cost_before = status(&mut a, 1).await["cost"].as_f64().unwrap();

    let live = sink.lease("sa_1", "scout").unwrap();
    let sealed = sink.lease("sa_2", "scout").unwrap();
    assert_eq!(live.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    assert_eq!(sealed.book(report(50, 10, 2_000_000)), BookOutcome::Booked { seq: 0 });
    let st = status_until(&mut a, 2, |st| st["child_tokens"]["input"] == 150).await;
    assert_eq!(st["child_tokens"]["input"], 150, "{st}");
    assert_eq!(st["spend_reporting_workers"], 2);

    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "two workers still report: no park"
    );
    assert!(sealed.seal());
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "one worker still reports: no park"
    );
    drop(live);
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "the last run ended: parks at the grace"
    );
    drop(sealed);

    // Unpark: the inclusive cost (journaled) kept both bookings.
    let mut b = attach(&handle).await;
    let st = status(&mut b, 10).await;
    assert!(
        (st["cost"].as_f64().unwrap() - (cost_before + 0.007)).abs() < 1e-9,
        "{st}"
    );
    assert_eq!(st["tokens"]["input"], 160);
    assert_eq!(st["tokens"]["output"], 31);
    assert!(st.get("spend_reporting_workers").is_none(), "{st}");
    end(&mut b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c4_a_report_that_arrives_while_a_turn_runs_is_booked_once() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let sink = task.spend_sink();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    let lease = sink.lease("sa_1", "scout").unwrap();
    a.send(submit("hello")).await.unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let st = status_until(&mut a, 1, |st| st["child_tokens"]["input"] == 100).await;
    assert_eq!(st["tokens"]["input"], 110, "own 10 + child 100, once: {st}");
    assert_eq!(st["tokens"]["output"], 21);
    assert_eq!(st["child_tokens"]["input"], 100);
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c5_an_empty_session_ends_idle_only_after_its_workers_end() {
    let _h = Home::new();
    let _g = Env::set("SYNAPS_DAEMON_IDLE_END_GRACE_SECS", "0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let sink = task.spend_sink();
    let lease = sink.lease("sa_1", "scout").unwrap();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    detach(&mut a).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), handle.closed())
            .await
            .is_err(),
        "a worker still reports: the empty session stays"
    );
    drop(lease);
    tokio::time::timeout(Duration::from_secs(5), handle.closed())
        .await
        .expect("the last run ended: the empty session ends at the grace");
}

/// A turn that calls the `subagent` tool (a oneshot with an inline worker).
const SSE_SUBAGENT_TOOL_USE: &str = concat!(
    r#"data: {"type":"message_start","message":{"id":"msg_s1","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    "\n\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_sub","name":"subagent"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"system_prompt\":\"You are a worker.\",\"task\":\"Say hi.\",\"timeout\":30}"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    "\n\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c6_a_oneshot_books_its_worker_once_into_the_totals_and_the_child_share() {
    let _h = Home::new();
    // Parent round 1 (tool use), the worker's one request, parent round 2.
    static BODIES: &[&str] = &[SSE_SUBAGENT_TOOL_USE, SSE_HI, SSE_HI];
    let (url, hits) = stub_seq(BODIES).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    a.send(submit("delegate")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    let results = tool_results(&last_conversation(&seen));
    assert!(
        results.iter().any(|r| r.contains("hi")),
        "the worker answered: {results:?}"
    );
    // The aggregate Usage of the worker is gone: only the parent's own two
    // rounds emit Usage.
    let usage: Vec<_> = seen.iter().filter(|e| is_usage(&e.event)).collect();
    assert_eq!(usage.len(), 2, "{usage:?}");

    let st = status_until(&mut a, 1, |st| {
        st["child_tokens"]["input"] == 10 && st.get("spend_reporting_workers").is_none()
    })
    .await;
    // Own 10/5 + 10/1, child 10/1: 30/7 (40/8 would be the worker twice).
    assert_eq!(st["tokens"]["input"], 30, "{st}");
    assert_eq!(st["tokens"]["output"], 7, "{st}");
    assert_eq!(
        st["child_tokens"],
        serde_json::json!({"input": 10, "output": 1, "cache_read": 0, "cache_creation": 0})
    );
    let child = st["child_cost"].as_f64().unwrap();
    assert!(child > 0.0, "{st}");
    assert!(st["cost"].as_f64().unwrap() > child, "{st}");
    assert!(st.get("spend_reporting_workers").is_none(), "{st}");
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c7_after_an_unpark_a_oneshot_still_books_under_the_session() {
    let _h = Home::new();
    let _g = Env::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    // Turn 1; after the unpark: round 1 (tool use), the worker, round 2.
    static BODIES: &[&str] = &[SSE_HI, SSE_SUBAGENT_TOOL_USE, SSE_HI, SSE_HI];
    let (url, hits) = stub_seq(BODIES).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);

    let mut b = attach(&handle).await;
    b.send(submit("delegate")).await.unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);
    let st = status_until(&mut b, 1, |st| {
        st["child_tokens"]["input"] == 10 && st.get("spend_reporting_workers").is_none()
    })
    .await;
    // The rebuilt runtime launched the worker under the session's sink.
    assert_eq!(
        st["child_tokens"],
        serde_json::json!({"input": 10, "output": 1, "cache_read": 0, "cache_creation": 0}),
        "{st}"
    );
    // Turn 1 10/1, round 1 10/5, the worker 10/1, round 2 10/1.
    assert_eq!(st["tokens"]["input"], 40, "{st}");
    assert_eq!(st["tokens"]["output"], 8, "{st}");
    end(&mut b).await;
}

// P1g2: an end and a reload checkpoint stop the session's workers and book
// what they report inside one deadline; late spend after an end is not
// booked; a reload checkpoint reopens admissions (the exec may fail); a host
// checkpoint leaves idle workers running, as before.

use std::sync::{Arc, RwLock};

use agent_engine::runtime::child_spend::{NotBooked, RunLease};
use agent_engine::runtime::subagent::SubagentRegistry;
use agent_engine::session::CheckpointReason;
use agent_engine::tools::{SubagentHandle, SubagentState};

type Registry = Arc<std::sync::Mutex<SubagentRegistry>>;

/// A persisted session idle after one turn (own 10/1), its spend sink and
/// its worker registry.
async fn idle_persisted() -> (SessionHandle, SpendSink, Registry, LocalTransport) {
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let sink = task.spend_sink();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    (handle, sink, registry, a)
}

/// A running worker `id` in the session's registry: its state and the
/// receiver its stop arrives on.
fn worker(
    registry: &Registry,
    id: &str,
) -> (Arc<RwLock<SubagentState>>, tokio::sync::oneshot::Receiver<()>) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let state = Arc::new(RwLock::new(SubagentState::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        id.into(),
        1,
        "scout".into(),
        "look around".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(&state),
        None,
        Some(tx),
        None,
    ));
    (state, rx)
}

/// The worker stops when told, as a real one does: its last attempt books
/// `last`, then its run ends (the lease drops with its runtime), then its
/// finalizer returns and marks the completion settled, which a reload's
/// stop wait (`await_stopped_workers`) waits for.
fn stops_when_told(
    state: &Arc<RwLock<SubagentState>>,
    stop: tokio::sync::oneshot::Receiver<()>,
    lease: RunLease,
    last: SpendReport,
) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let _ = stop.await;
        let _ = lease.book(last);
        drop(lease);
        state.write().unwrap().completion_settled = true;
    });
}

fn saved(id: &str) -> agent_engine::core::session::Session {
    agent_engine::core::session::Session::load(id).expect("the saved session")
}

async fn checkpoint(
    t: &mut LocalTransport,
    reason: CheckpointReason,
    deadline: Option<tokio::time::Instant>,
) {
    t.send(SessionCommand::Checkpoint { reason, deadline, skip_quiesce_grace: false })
        .await
        .unwrap();
    until(t, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. }
            if *id == agent_engine::session::wire::CHECKPOINT_QUERY_ID)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e1_an_end_stops_an_idle_worker_and_saves_what_it_reports() {
    let _h = Home::new();
    let (handle, sink, registry, mut a) = idle_persisted().await;
    let st = status(&mut a, 1).await;
    let id = st["session"].as_str().unwrap().to_string();
    let cost_before = st["cost"].as_f64().unwrap();
    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    let (state, stop) = worker(&registry, "sa_1");
    stops_when_told(&state, stop, lease, report(7, 3, 1_000_000));
    status_until(&mut a, 2, |st| st["child_tokens"]["input"] == 100).await;

    let started = std::time::Instant::now();
    end(&mut a).await;
    tokio::time::timeout(Duration::from_secs(10), handle.closed())
        .await
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "the worker stopped at once: {took:?}");
    assert!(
        state.read().unwrap().cancel_requested,
        "the end stopped the idle worker"
    );
    let s = saved(&id);
    assert_eq!(s.total_input_tokens, 117, "own 10 + 100 + the stop's 7");
    assert_eq!(s.total_output_tokens, 24);
    assert!(
        (s.session_cost - (cost_before + 0.006)).abs() < 1e-9,
        "{}",
        s.session_cost
    );
    assert!(sink.lease("sa_2", "scout").is_err(), "an ended session admits no run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2_an_end_waits_for_a_stuck_worker_only_until_its_deadline() {
    let _h = Home::new();
    let (handle, sink, registry, mut a) = idle_persisted().await;
    let id = status(&mut a, 1).await["session"].as_str().unwrap().to_string();
    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    // It ignores the stop and keeps its run.
    let (_state, _stop) = worker(&registry, "sa_1");
    status_until(&mut a, 2, |st| st["child_tokens"]["input"] == 100).await;

    let started = std::time::Instant::now();
    end(&mut a).await;
    tokio::time::timeout(Duration::from_secs(10), handle.closed())
        .await
        .unwrap();
    let took = started.elapsed();
    // The wait shares the end's cancel budget (3 s): no fresh bound.
    assert!(
        took >= Duration::from_millis(2500),
        "waited for the worker until the deadline: {took:?}"
    );
    assert!(took < Duration::from_secs(6), "within the end's budget: {took:?}");
    assert_eq!(
        lease.book(report(7, 3, 1_000_000)),
        BookOutcome::NotBooked(NotBooked::SessionClosed),
        "spend after the end is not booked"
    );
    assert_eq!(saved(&id).total_input_tokens, 110);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e3_a_reload_checkpoint_books_its_stopped_workers_before_its_save_and_reopens() {
    let _h = Home::new();
    let (_handle, sink, registry, mut a) = idle_persisted().await;
    let id = status(&mut a, 1).await["session"].as_str().unwrap().to_string();
    let lease = sink.lease("sa_1", "scout").unwrap();
    let (state, stop) = worker(&registry, "sa_1");
    stops_when_told(&state, stop, lease, report(7, 3, 1_000_000));

    let started = std::time::Instant::now();
    checkpoint(
        &mut a,
        CheckpointReason::Reload,
        Some(tokio::time::Instant::now() + Duration::from_secs(6)),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(state.read().unwrap().cancel_requested, "a reload stops idle workers");
    assert_eq!(
        saved(&id).total_input_tokens,
        17,
        "booked before the checkpoint's save"
    );
    // The session continues here (a failed exec): runs are admitted again.
    let again = sink.lease("sa_2", "scout").expect("reopened");
    assert_eq!(again.book(report(1, 0, 1_000)), BookOutcome::Booked { seq: 0 });
    let st = status_until(&mut a, 5, |st| st["child_tokens"]["input"] == 8).await;
    assert_eq!(st["child_tokens"]["input"], 8, "{st}");
    drop(again);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e4_a_reload_checkpoint_keeps_to_the_coordinators_deadline() {
    let _h = Home::new();
    let (_handle, sink, _, mut a) = idle_persisted().await;
    // A one-shot run's lease: one-shot runs are threads of their own, not
    // registry workers, so the reload's stop wait (`await_stopped_workers`,
    // its own bound on stopped workers' finalizers) does not cover it, and
    // the run keeps its lease past the spend wait. Only the spend wait,
    // bounded by the caller's deadline, is measured here.
    let lease = sink.lease("sa_1", "scout").unwrap();

    // 4 s, of which 3 s are kept for the save: the wait for spend ends
    // after about 1 s.
    let started = std::time::Instant::now();
    checkpoint(
        &mut a,
        CheckpointReason::Reload,
        Some(tokio::time::Instant::now() + Duration::from_secs(4)),
    )
    .await;
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(800), "waited for the run: {took:?}");
    assert!(
        took < Duration::from_millis(2500),
        "the caller's deadline, not a fresh budget: {took:?}"
    );
    // Reopened: the still-running run's later report is booked.
    assert_eq!(lease.book(report(5, 0, 1_000)), BookOutcome::Booked { seq: 0 });
    let st = status_until(&mut a, 3, |st| st["child_tokens"]["input"] == 5).await;
    assert_eq!(st["child_tokens"]["input"], 5, "{st}");
    assert_eq!(st["spend_reporting_workers"], 1, "{st}");
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e5_a_host_checkpoint_leaves_idle_workers_running_and_admits_runs() {
    let _h = Home::new();
    let (_handle, sink, registry, mut a) = idle_persisted().await;
    let lease = sink.lease("sa_1", "scout").unwrap();
    let (state, stop) = worker(&registry, "sa_1");
    stops_when_told(&state, stop, lease, report(7, 3, 1_000_000));

    checkpoint(&mut a, CheckpointReason::HostRequest, None).await;
    assert!(
        !state.read().unwrap().cancel_requested,
        "as before: a host checkpoint stops no idle worker"
    );
    let other = sink.lease("sa_2", "scout").expect("still admitting");
    let st = status(&mut a, 3).await;
    assert_eq!(st["spend_reporting_workers"], 2, "{st}");
    drop(other);
    end(&mut a).await;
}

// ── The child share in status, events, attach, saves and restores ───────

use agent_engine::session::wire::{encode_line, WireSessionEvent};
use agent_engine::session::{AttachMode, HistoryMode};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn h1_a_created_session_shows_an_exact_child_share_in_status_events_and_attach() {
    let _h = Home::new();
    let (handle, sink, mut a) = idle_session(cfg()).await;
    let st = status(&mut a, 1).await;
    assert_eq!(st["child_share_exact"], true, "{st}");
    assert_eq!(st["child_cost"], 0.0, "{st}");

    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::Conversation(c) if c.child_tokens.input == 100)
    })
    .await;
    let event = seen.into_iter().last().unwrap().event;
    let SessionEventWire::Conversation(c) = &event else {
        unreachable!()
    };
    assert!((c.child_cost - 0.005).abs() < 1e-12, "{}", c.child_cost);
    assert_eq!(c.child_tokens.output, 20);
    assert!(c.child_share_exact);
    // On the socket the event is the digest: it carries the share too.
    let line = encode_line(&WireSessionEvent::for_client(event).unwrap()).unwrap();
    assert!(line.contains(r#""child_share_exact":true"#), "{line}");
    assert!(line.contains(r#""child_tokens":{"input":100,"output":20"#), "{line}");

    let st = status(&mut a, 2).await;
    assert_eq!(st["child_share_exact"], true, "{st}");
    for history in [HistoryMode::Full, HistoryMode::Digest] {
        let mut m = ClientMeta::new(ClientKind::Tui);
        m.history = history;
        let (other, snap) = LocalTransport::attach_with(handle.clone(), m, AttachMode::Observe)
            .await
            .unwrap();
        let conv = &snap.conversation;
        assert!((conv.child_cost - 0.005).abs() < 1e-12, "{history:?}");
        assert_eq!(conv.child_tokens.input, 100, "{history:?}");
        assert!(conv.child_share_exact, "{history:?}");
        drop(other);
    }
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn h2_a_restore_shows_the_saved_share_not_exact_and_never_adds_it_again() {
    let _h = Home::new();
    let _g = Env::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let (handle, sink, mut a) = idle_session(pcfg()).await;
    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    let st = status_until(&mut a, 1, |st| st["child_tokens"]["input"] == 100).await;
    assert_eq!(st["child_share_exact"], true, "{st}");
    let cost = st["cost"].as_f64().unwrap();
    drop(lease);
    detach(&mut a).await;
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "parks once its worker's run ended"
    );

    // Unpark (a restore): the share as of the park's save, not exact.
    let mut b = attach(&handle).await;
    let st = status(&mut b, 10).await;
    assert_eq!(st["child_share_exact"], false, "{st}");
    assert!((st["cost"].as_f64().unwrap() - cost).abs() < 1e-12, "never added again: {st}");
    assert!((st["child_cost"].as_f64().unwrap() - 0.005).abs() < 1e-12, "{st}");
    assert_eq!(st["child_tokens"]["input"], 100, "{st}");
    assert_eq!(st["child_tokens"]["output"], 20, "{st}");

    // A booking after the restore adds to both once; still not exact.
    let lease = sink.lease("sa_2", "scout").unwrap();
    assert_eq!(lease.book(report(10, 2, 1_000_000)), BookOutcome::Booked { seq: 0 });
    let st = status_until(&mut b, 11, |st| st["child_tokens"]["input"] == 110).await;
    assert!((st["cost"].as_f64().unwrap() - (cost + 0.001)).abs() < 1e-9, "{st}");
    assert!((st["child_cost"].as_f64().unwrap() - 0.006).abs() < 1e-9, "{st}");
    assert_eq!(st["child_share_exact"], false, "{st}");
    drop(lease);
    end(&mut b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn h3_the_saved_session_carries_the_child_share() {
    let _h = Home::new();
    let (_handle, sink, mut a) = idle_session(pcfg()).await;
    let id = status(&mut a, 1).await["session"]
        .as_str()
        .unwrap()
        .to_string();
    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(30, 6, 2_000_000)), BookOutcome::Booked { seq: 0 });
    status_until(&mut a, 2, |st| st["child_tokens"]["input"] == 30).await;
    drop(lease);
    end(&mut a).await;
    let s = saved(&id);
    assert!((s.child_cost - 0.002).abs() < 1e-12, "{}", s.child_cost);
    assert_eq!(s.child_tokens.input, 30);
    assert_eq!(s.child_tokens.output, 6);
    assert!(s.session_cost >= s.child_cost);
    assert!(s.total_input_tokens >= 30);
}

// P1 review r1, F3: reports queued while the actor is busy are booked in
// one go (one Conversation event, one queued save), and a reload checkpoint
// with 4096 queued reports (the warning threshold), a long history and a real save
// finishes well inside its budget. The deadline bounds the wait for
// producers; booking what was already accepted may run past it.

use agent_engine::session::{budgets, persister_testing};

/// A saved session with a realistic history of `n` messages (`n` even):
/// prompts and answers of about 800 bytes, and every third exchange a tool
/// call with its result. Its id.
async fn saved_history(n: usize) -> String {
    let filler = "lorem ipsum dolor sit amet, consectetur adipiscing ".repeat(16);
    let mut messages: Vec<agent_engine::SharedMessage> = Vec::with_capacity(n);
    let exchanges = n / 2;
    let mut after_tool = false;
    for i in 0..exchanges {
        messages.push(Arc::new(if after_tool {
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("toolu_{}", i - 1), "content": filler}
            ]})
        } else {
            serde_json::json!({"role": "user", "content": format!("question {i}: {filler}")})
        }));
        // A tool call only where the next exchange can carry its result.
        let tool_call = i % 3 == 1 && i + 1 < exchanges;
        after_tool = tool_call;
        messages.push(Arc::new(if tool_call {
            serde_json::json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("checking {i}")},
                {"type": "tool_use", "id": format!("toolu_{i}"), "name": "read", "input": {"path": format!("/src/f{i}.rs")}}
            ]})
        } else {
            serde_json::json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("answer {i}: {filler}")}
            ]})
        }));
    }
    let mut session = agent_engine::core::session::Session::new(MODEL, "low", None);
    session.api_messages = messages;
    session.save().await.unwrap();
    session.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c8_a_queued_backlog_is_booked_in_one_go_with_one_event_and_one_save() {
    let _h = Home::new();
    let id = saved_history(4).await;
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig {
            continue_session: Some(Some(id.clone())),
            ..pcfg()
        })
        .await
        .unwrap();
    let sink = task.spend_sink();
    let lease = sink.lease("sa_1", "scout").unwrap();
    for seq in 0..100 {
        assert_eq!(lease.book(report(1, 0, 1_000)), BookOutcome::Booked { seq });
    }
    let mut rx = handle.subscribe();
    let saves_before = persister_testing::saves_requested(&id);
    tokio::spawn(task.run());
    // The first Conversation event that carries any child share.
    let first = loop {
        match rx.recv().await {
            Ok(e) => {
                if let SessionEventWire::Conversation(c) = e.event {
                    if c.child_tokens.input > 0 {
                        break c.child_tokens.input;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (),
            Err(e) => panic!("{e}"),
        }
    };
    let mut a = attach(&handle).await;
    let st = status_until(&mut a, 1, |st| st["child_tokens"]["input"] == 100).await;
    assert_eq!(st["child_tokens"]["input"], 100, "{st}");
    let saves = persister_testing::saves_requested(&id) - saves_before;
    assert!(
        first == 100 && saves < 10,
        "one event and one save for the backlog: the first child Conversation \
         carried {first} of 100 reports; {saves} saves were queued"
    );
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e6_a_reload_checkpoint_books_a_full_backlog_well_inside_its_budget() {
    // The warning and load-test threshold (also the arm's per-pass cap).
    const BOUND: u64 = agent_engine::session::child_spend_testing::PASS_CAP as u64;
    const MESSAGES: usize = 2000;
    let _h = Home::new();
    let id = saved_history(MESSAGES).await;
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig {
            continue_session: Some(Some(id.clone())),
            ..pcfg()
        })
        .await
        .unwrap();
    let sink = task.spend_sink();
    let lease = sink.lease("sa_1", "scout").unwrap();
    for seq in 0..BOUND {
        assert_eq!(lease.book(report(1, 0, 1_000)), BookOutcome::Booked { seq });
    }
    drop(lease);
    let mut rx = handle.subscribe();
    let started = tokio::time::Instant::now();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: Some(started + budgets::CHECKPOINT_BUDGET),
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    tokio::spawn(task.run());
    loop {
        match rx.recv().await {
            Ok(e) if matches!(e.event, SessionEventWire::QueryResult { id, .. }
                if id == agent_engine::session::wire::CHECKPOINT_QUERY_ID) => break,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (),
            Err(e) => panic!("{e}"),
        }
    }
    let took = started.elapsed();
    eprintln!(
        "e6: a reload checkpoint with {BOUND} queued reports, {MESSAGES} saved messages \
         and a real save took {took:?} (budget {:?})",
        budgets::CHECKPOINT_BUDGET
    );
    assert!(
        took < budgets::CHECKPOINT_BUDGET / 2,
        "well inside the checkpoint budget: {took:?}"
    );
    // Everything was booked before the checkpoint's save.
    let s = saved(&id);
    assert_eq!(s.child_tokens.input, BOUND);
    assert_eq!(s.api_messages.len(), MESSAGES);
    let mut a = attach(&handle).await;
    let st = status(&mut a, 1).await;
    assert_eq!(st["child_tokens"]["input"], BOUND, "{st}");
    end(&mut a).await;
}

// A backlog over the arm's per-pass cap is booked in passes of at most the
// cap, each with one Conversation event and one queued save; a command that
// arrives during the backlog is answered between passes; and every report
// is booked exactly once.

use agent_engine::session::child_spend_testing;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn c9_a_backlog_over_the_pass_cap_is_booked_in_passes_and_a_command_is_answered_between() {
    const PASSES: u64 = 12;
    const QUERY: u64 = 77;
    let cap = child_spend_testing::PASS_CAP as u64;
    // The last pass is short.
    let total = PASSES * cap - 5;
    let _h = Home::new();
    let id = saved_history(4).await;
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig {
            continue_session: Some(Some(id.clone())),
            ..pcfg()
        })
        .await
        .unwrap();
    let sink = task.spend_sink();
    // Two runs, interleaved. Report `g` carries input 1, output g and cache
    // read g squared, so the totals tell that each was booked exactly once.
    let leases = [sink.lease("sa_1", "scout").unwrap(), sink.lease("sa_2", "scout").unwrap()];
    for g in 0..total {
        let mut r = report(1, g, 1_000);
        r.tokens.cache_read = g * g;
        assert_eq!(leases[(g % 2) as usize].book(r), BookOutcome::Booked { seq: g / 2 });
    }
    // After the first full pass, before the loop turns again, a command
    // arrives: a status query.
    let asker = handle.clone();
    child_spend_testing::after_a_full_pass(&id, move || {
        futures::executor::block_on(asker.send(SessionCommand::Query {
            id: QUERY,
            query: SessionQuery::Status,
        }))
        .unwrap();
    });
    let mut rx = handle.subscribe();
    let saves_before = persister_testing::saves_requested(&id);
    tokio::spawn(task.run());
    // The child share each Conversation event carried, and where the
    // query's answer came among them.
    let mut steps: Vec<u64> = Vec::new();
    let mut answer: Option<(usize, u64)> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while steps.last() != Some(&total) || answer.is_none() {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(e)) => match e.event {
                SessionEventWire::Conversation(c) if c.child_tokens.input > 0 => {
                    steps.push(c.child_tokens.input)
                }
                SessionEventWire::QueryResult { id: QUERY, value, .. } => {
                    answer = Some((steps.len(), value["child_tokens"]["input"].as_u64().unwrap()))
                }
                _ => (),
            },
            Ok(Err(e)) => panic!("{e}: {steps:?}"),
            Err(_) => panic!("timed out: steps {steps:?}, answer {answer:?}"),
        }
    }
    let saves = persister_testing::saves_requested(&id) - saves_before;
    let (after_steps, answered) = answer.unwrap();
    let passes: Vec<u64> = (1..=PASSES).map(|k| (k * cap).min(total)).collect();
    assert!(
        steps == passes && answered % cap == 0 && answered >= cap && answered < total
            && after_steps < steps.len(),
        "passes of at most {cap} with one Conversation each, and the query answered \
         between passes: the Conversations carried {steps:?} of {total}; the query was \
         answered with {answered} booked, after {after_steps} of them"
    );
    assert!(saves <= PASSES + 9, "one save per pass: {saves} queued for {PASSES} passes");
    // Every report exactly once: the count, the sum and the sum of squares
    // of their indexes.
    let mut a = attach(&handle).await;
    let st = status(&mut a, 1).await;
    let n = total as u128;
    let sum = n * (n - 1) / 2;
    let squares = (n - 1) * n * (2 * n - 1) / 6;
    assert_eq!(st["child_tokens"]["input"], total, "{st}");
    assert_eq!(st["child_tokens"]["output"].as_u64().unwrap() as u128, sum, "{st}");
    assert_eq!(st["child_tokens"]["cache_read"].as_u64().unwrap() as u128, squares, "{st}");
    assert!(
        (st["child_cost"].as_f64().unwrap() - total as f64 * 1e-6).abs() < 1e-9,
        "{st}"
    );
    // Each run's sequence continues where the actor's count of it stands.
    let next = [total.div_ceil(2), total / 2];
    for (lease, seq) in leases.iter().zip(next) {
        assert_eq!(lease.book(report(1, 0, 1_000)), BookOutcome::Booked { seq });
    }
    let st = status_until(&mut a, 2, |st| st["child_tokens"]["input"] == total + 2).await;
    assert_eq!(st["child_tokens"]["input"], total + 2, "{st}");
    drop(leases);
    end(&mut a).await;
}

// ── The child_spend frame (P1j) ──────────────────────────────────────────
//
// Frames go out on the session's optional channel (P1j r1 F1), never the
// main one: these tests read them from `subscribe_optional()` and order them
// against the main channel by envelope seq (one counter for both).

fn is_child_spend(e: &SessionEventWire) -> bool {
    matches!(e, SessionEventWire::ChildSpend(_))
}

/// The next envelope on the optional channel (≤ 10 s): a child spend frame
/// for `session`, with its envelope seq.
async fn next_frame(
    rx: &mut tokio::sync::broadcast::Receiver<agent_engine::session::Envelope>,
    session: &agent_engine::session::SessionId,
) -> (u64, agent_engine::session::ChildSpendEvent) {
    let env = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("a child spend frame within 10 s")
        .expect("the optional channel is open and did not lag");
    assert_eq!(&env.session_id, session, "the frame's envelope names its session");
    match env.event {
        SessionEventWire::ChildSpend(f) => (env.seq, f),
        other => panic!("the optional channel carries only child spend frames: {other:?}"),
    }
}

/// The next envelope on a raw main-channel receiver (≤ 10 s, no lag).
async fn next_main(
    rx: &mut tokio::sync::broadcast::Receiver<agent_engine::session::Envelope>,
) -> agent_engine::session::Envelope {
    match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
        Ok(Ok(env)) => env,
        Ok(Err(e)) => panic!("the main channel: {e}"),
        Err(_) => panic!("nothing on the main channel within 10 s"),
    }
}

/// Nothing more on the optional channel.
fn no_more_frames(rx: &mut tokio::sync::broadcast::Receiver<agent_engine::session::Envelope>) -> bool {
    matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn j1_each_booking_sends_one_child_spend_with_the_totals_after_it() {
    let _h = Home::new();
    let (handle, sink, mut a) = idle_session(cfg()).await;
    let mut optional = handle.subscribe_optional();
    let cost_before = status(&mut a, 1).await["cost"].as_f64().unwrap();
    let lease = sink.lease("sa_1", "scout").unwrap();
    let grand = lease.child("sa_2", "grand").unwrap();
    let run = lease.identity().run_hex();
    let grand_run = grand.identity().run_hex();
    assert_ne!(run, grand_run);

    // A worker's own request, with a cache split and its TTL split.
    let mut first = report(100, 20, 5_000_000);
    first.tokens.cache_read = 7;
    first.tokens.cache_creation = 3;
    first.tokens.cache_creation_5m = Some(2);
    first.tokens.cache_creation_1h = Some(1);
    assert_eq!(lease.book(first), BookOutcome::Booked { seq: 0 });
    let (frame_seq, f) = next_frame(&mut optional, &handle.id).await;
    assert_eq!(f.handle, "sa_1");
    assert_eq!(f.run, run);
    assert!(f.run.len() == 16 && f.run.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(f.spend_seq, 0);
    assert_eq!(f.agent, "scout");
    assert_eq!(f.parent_handle, None, "the session's own turn delegated it");
    assert_eq!(f.kind, "turn");
    assert_eq!(f.model, "anthropic/claude-sonnet-4-5");
    assert_eq!(
        (f.input_tokens, f.output_tokens, f.cache_read_input_tokens, f.cache_creation_input_tokens),
        (100, 20, 7, 3)
    );
    assert_eq!((f.cache_creation_5m, f.cache_creation_1h), (Some(2), Some(1)));
    assert_eq!(f.cost_nanousd, 5_000_000);
    assert!((f.cost_usd - 0.005).abs() < 1e-15, "{}", f.cost_usd);
    assert!((f.session_cost_usd - (cost_before + 0.005)).abs() < 1e-9, "{}", f.session_cost_usd);
    assert!((f.child_cost_usd - 0.005).abs() < 1e-12, "{}", f.child_cost_usd);
    // The pass's one snapshot follows the frame, on the main channel, later
    // in seq, with the same totals; the main channel carries no frame.
    let after = until(&mut a, |e| matches!(e, SessionEventWire::Conversation(c) if c.child_cost > 0.0)).await;
    let snapshot_seq = after.last().unwrap().seq;
    assert!(frame_seq < snapshot_seq, "the frame comes before the pass's snapshot: {frame_seq} vs {snapshot_seq}");
    let conv = last_conversation(&after);
    assert_eq!(f.session_cost_usd, conv.cost);
    assert_eq!(f.child_cost_usd, conv.child_cost);
    assert!(!after.iter().any(|e| is_usage(&e.event)), "never a Usage event");
    assert!(!after.iter().any(|e| is_child_spend(&e.event)), "no frame on the main channel");
    assert!(no_more_frames(&mut optional), "one frame per booking");

    // A grandchild's summary: its own run, its parent's handle, its kind and
    // model, no TTL split.
    let summary = SpendReport {
        kind: SpendKind::Summary,
        model: "anthropic/claude-haiku-4-5".to_string(),
        ..report(30, 6, 1_000_000)
    };
    assert_eq!(grand.book(summary), BookOutcome::Booked { seq: 0 });
    let (_, f) = next_frame(&mut optional, &handle.id).await;
    assert_eq!((f.handle.as_str(), f.run.as_str()), ("sa_2", grand_run.as_str()));
    assert_eq!(f.parent_handle.as_deref(), Some("sa_1"));
    assert_eq!(f.agent, "grand");
    assert_eq!(f.kind, "summary");
    assert_eq!(f.model, "anthropic/claude-haiku-4-5");
    assert_eq!((f.cache_creation_5m, f.cache_creation_1h), (None, None));
    assert_eq!(f.cost_nanousd, 1_000_000);
    assert!((f.session_cost_usd - (cost_before + 0.006)).abs() < 1e-9);
    assert!((f.child_cost_usd - 0.006).abs() < 1e-12);

    // The run's next report steps its own sequence.
    assert_eq!(lease.book(report(1, 1, 1_000)), BookOutcome::Booked { seq: 1 });
    let (_, f) = next_frame(&mut optional, &handle.id).await;
    assert_eq!((f.handle.as_str(), f.spend_seq), ("sa_1", 1));
    assert_eq!(f.run, run);

    // What is not booked sends nothing.
    assert!(grand.seal());
    assert!(matches!(grand.book(report(9, 9, 9_000)), BookOutcome::NotBooked(_)));
    let quiet = drain_for(&mut a, Duration::from_millis(400)).await;
    assert!(!quiet.iter().any(|e| is_child_spend(&e.event)));
    assert!(no_more_frames(&mut optional), "a refused report sends no frame");
    let st = status(&mut a, 2).await;
    assert!((st["child_cost"].as_f64().unwrap() - 0.006001).abs() < 1e-12, "{st}");
    drop(grand);
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn j2_a_child_spend_during_a_turn_is_sent_but_never_replayed_on_attach() {
    let _h = Home::new();
    let (url, _) = stub_slow(SSE_HI, Duration::from_secs(3)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let sink = task.spend_sink();
    let mut optional = handle.subscribe_optional();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;

    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(100, 20, 5_000_000)), BookOutcome::Booked { seq: 0 });
    let (frame_seq, _) = next_frame(&mut optional, &handle.id).await;

    let (b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    assert!(snap.streaming, "attached mid-turn, after the frame");
    assert!(
        snap.replay
            .iter()
            .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })),
        "the turn's replay"
    );
    assert!(
        !snap.replay.iter().any(|e| is_child_spend(&e.event)),
        "never replayed on attach"
    );
    assert!((snap.conversation.child_cost - 0.005).abs() < 1e-12, "the share is in the snapshot");
    drop(b);
    let rest = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let idle_seq = rest.last().unwrap().seq;
    assert!(frame_seq < idle_seq, "sent while the turn runs: {frame_seq} vs {idle_seq}");
    assert!(!rest.iter().any(|e| is_child_spend(&e.event)), "no frame on the main channel");
    drop(lease);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn j3_a_pass_sends_a_frame_per_report_then_one_snapshot_with_the_last_totals() {
    const QUERY: u64 = 7;
    let _h = Home::new();
    let id = saved_history(4).await;
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig {
            continue_session: Some(Some(id.clone())),
            ..pcfg()
        })
        .await
        .unwrap();
    let sink = task.spend_sink();
    let lease = sink.lease("sa_1", "scout").unwrap();
    for (seq, cost) in [(0, 1_000_000), (1, 2_000_000), (2, 3_000_000)] {
        assert_eq!(lease.book(report(10, 1, cost)), BookOutcome::Booked { seq });
    }
    let mut rx = handle.subscribe();
    let mut optional = handle.subscribe_optional();
    tokio::spawn(task.run());
    // The main channel, up to the first snapshot that carries a child share.
    let (snapshot_seq, conv) = loop {
        let env = next_main(&mut rx).await;
        match env.event {
            SessionEventWire::Conversation(c) if c.child_cost > 0.0 => break (env.seq, c),
            SessionEventWire::ChildSpend(_) => panic!("a frame on the main channel"),
            _ => (),
        }
    };
    let mut frames = Vec::new();
    for _ in 0..3 {
        frames.push(next_frame(&mut optional, &handle.id).await);
    }
    assert!(no_more_frames(&mut optional), "one frame per report");
    assert!(
        frames.iter().all(|(seq, _)| *seq < snapshot_seq),
        "every frame before the pass's snapshot: {:?} vs {snapshot_seq}",
        frames.iter().map(|(seq, _)| *seq).collect::<Vec<_>>()
    );
    let frames: Vec<_> = frames.into_iter().map(|(_, f)| f).collect();
    let seqs: Vec<u64> = frames.iter().map(|f| f.spend_seq).collect();
    assert_eq!(seqs, [0, 1, 2], "one frame per report, in order");
    let shares: Vec<f64> = frames.iter().map(|f| f.child_cost_usd).collect();
    for (got, want) in shares.iter().zip([0.001, 0.003, 0.006]) {
        assert!((got - want).abs() < 1e-12, "each frame's share right after its booking: {shares:?}");
    }
    let base = frames[0].session_cost_usd - 0.001;
    for (f, share) in frames.iter().zip([0.001, 0.003, 0.006]) {
        assert!((f.session_cost_usd - (base + share)).abs() < 1e-9, "{}", f.session_cost_usd);
    }
    let last = frames.last().unwrap();
    assert_eq!((conv.cost, conv.child_cost), (last.session_cost_usd, last.child_cost_usd));
    assert_eq!(conv.child_tokens.input, 30);

    // Exactly one snapshot for the pass: a host query answered after it,
    // and no other snapshot with a child share before the answer.
    handle
        .send(SessionCommand::Query { id: QUERY, query: SessionQuery::Status })
        .await
        .unwrap();
    loop {
        let env = next_main(&mut rx).await;
        match env.event {
            SessionEventWire::QueryResult { id: QUERY, .. } => break,
            SessionEventWire::Conversation(c) if c.child_cost > 0.0 => {
                panic!("a second snapshot for one pass (seq {})", env.seq)
            }
            SessionEventWire::ChildSpend(_) => panic!("a frame on the main channel"),
            _ => (),
        }
    }
    assert!(no_more_frames(&mut optional));
    let mut a = attach(&handle).await;
    drop(lease);
    end(&mut a).await;
}

/// P1j r1: a duplicate (a seq the session already booked for that run)
/// goes through the session's dedupe: no frame, nothing booked, and the
/// run's next report still books and sends its frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn j4_a_duplicate_report_sends_no_frame_and_the_next_still_books() {
    let _h = Home::new();
    let (handle, sink, mut a) = idle_session(cfg()).await;
    let mut optional = handle.subscribe_optional();
    let lease = sink.lease("sa_1", "scout").unwrap();
    assert_eq!(lease.book(report(10, 1, 1_000_000)), BookOutcome::Booked { seq: 0 });
    let (_, f) = next_frame(&mut optional, &handle.id).await;
    assert_eq!(f.spend_seq, 0);
    let st = status_until(&mut a, 1, |st| st["child_tokens"]["input"] == 10).await;
    assert_eq!(st["child_tokens"]["input"], 10, "{st}");
    let share = st["child_cost"].as_f64().unwrap();

    // The run's seq 0 again, then its next report.
    assert!(agent_engine::runtime::child_spend::testing::publish_at(
        &lease,
        0,
        report(500, 50, 9_000_000)
    ));
    assert_eq!(lease.book(report(1, 1, 1_000)), BookOutcome::Booked { seq: 1 });
    let (_, f) = next_frame(&mut optional, &handle.id).await;
    assert_eq!(f.spend_seq, 1, "the duplicate sent no frame");
    assert_eq!((f.input_tokens, f.cost_nanousd), (1, 1_000));
    assert!(no_more_frames(&mut optional));
    let st = status_until(&mut a, 100, |st| st["child_tokens"]["input"] == 11).await;
    assert_eq!(st["child_tokens"]["input"], 11, "the duplicate booked nothing: {st}");
    assert!((st["child_cost"].as_f64().unwrap() - (share + 0.000001)).abs() < 1e-12, "{st}");
    drop(lease);
    end(&mut a).await;
}

/// P1j r1 F1: child spend frames have their own channel. A plain receiver
/// that nobody reads during a backlog (a socket connection whose forwarder
/// isn't polled) keeps every ordinary event: the main channel carries no
/// frame to evict one with, and the reload drain forwards all of them, with
/// no lag notice. 2 x 4096 + 7 reports are queued before the actor starts,
/// so the actor books them in three passes and nothing reads the receivers
/// meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn j5_a_paused_plain_receiver_keeps_every_ordinary_event_through_a_backlog() {
    const QUERY: u64 = 5;
    let cap = child_spend_testing::PASS_CAP as u64;
    let total = 2 * cap + 7;
    let _h = Home::new();
    let id = saved_history(4).await;
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig {
            continue_session: Some(Some(id.clone())),
            ..pcfg()
        })
        .await
        .unwrap();
    let sink = task.spend_sink();
    let lease = sink.lease("sa_1", "scout").unwrap();
    for g in 0..total {
        assert_eq!(lease.book(report(1, 0, 1_000)), BookOutcome::Booked { seq: g });
    }
    // Two plain receivers, neither read until the backlog is booked: one
    // read directly, one through the reload drain. An ordinary event waits
    // in the command queue: a status query.
    let mut paused = handle.subscribe();
    let mut drained = handle.subscribe();
    handle
        .send(SessionCommand::Query { id: QUERY, query: SessionQuery::Status })
        .await
        .unwrap();
    tokio::spawn(task.run());
    let mut a = attach(&handle).await;
    let st = status_until(&mut a, 100, |st| st["child_tokens"]["input"] == total).await;
    assert_eq!(st["child_tokens"]["input"], total, "{st}");

    // What each receiver kept: the passes' snapshots and the query's answer.
    fn ordinary(seen: &[(u64, SessionEventWire)]) -> (Vec<u64>, usize) {
        let passes = seen
            .iter()
            .filter_map(|(_, e)| match e {
                SessionEventWire::Conversation(c) if c.child_tokens.input > 0 => {
                    Some(c.child_tokens.input)
                }
                _ => None,
            })
            .collect();
        let answers = seen
            .iter()
            .filter(|(_, e)| matches!(e, SessionEventWire::QueryResult { id: QUERY, .. }))
            .count();
        (passes, answers)
    }
    let want = vec![cap, 2 * cap, total];
    let mut seen = Vec::new();
    loop {
        match paused.try_recv() {
            Ok(env) => seen.push((env.seq, env.event)),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(e) => panic!("the paused plain receiver lost events: {e}"),
        }
    }
    assert!(
        !seen.iter().any(|(_, e)| matches!(e, SessionEventWire::ChildSpend(_))),
        "the main channel carries no child spend frame"
    );
    assert_eq!(ordinary(&seen), (want.clone(), 1), "every pass's snapshot and the query's answer");
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0), "in seq order");

    // The reload drain on the other paused receiver: the same events, and
    // no lag notice.
    let (out, mut sent) = tokio::sync::mpsc::channel(16_384);
    assert!(
        agent_engine::daemon::conn::testing::drain_on_reload(
            &mut drained,
            &|_: &agent_engine::session::Envelope| false,
            &handle.id,
            &out,
        )
        .await
    );
    drop(out);
    let mut forwarded = Vec::new();
    while let Some(frame) = sent.recv().await {
        match frame {
            agent_engine::session::wire::DaemonFrame::Event(env) => {
                let env: agent_engine::session::Envelope = env.into();
                if let SessionEventWire::SystemNotice(text) = &env.event {
                    assert!(!text.contains("lagged"), "the reload drain lagged: {text}");
                }
                forwarded.push((env.seq, env.event));
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(ordinary(&forwarded), (want, 1), "the reload drain forwards them all");
    drop(lease);
    end(&mut a).await;
}
