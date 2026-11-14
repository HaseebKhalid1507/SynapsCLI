//! `SessionMeta` state for the `/sessions` modal: busy / state_since /
//! park_due / evict_due / title / cost / messages, as the actor publishes
//! them through the handle's presence (`meta_live`).

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle, SessionMeta,
};
use chrono::Utc;
use serial_test::serial;

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

/// Sets an env var for the test, removes it on drop.
struct Env(&'static str);
impl Env {
    fn set(k: &'static str, v: &str) -> Self {
        std::env::set_var(k, v);
        Self(k)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

fn grace(v: &str) -> Env {
    Env::set("SYNAPS_DAEMON_PARK_GRACE_SECS", v)
}

async fn detach(t: &mut LocalTransport) {
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
}

/// Poll `meta_live()` until `pred` holds (≤ `for_`); the last meta either way.
async fn wait_meta(
    handle: &SessionHandle,
    for_: Duration,
    pred: impl Fn(&SessionMeta) -> bool,
) -> (bool, SessionMeta) {
    let deadline = tokio::time::Instant::now() + for_;
    loop {
        let m = handle.meta_live();
        if pred(&m) {
            return (true, m);
        }
        if tokio::time::Instant::now() >= deadline {
            return (false, m);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn shutdown(handle: &SessionHandle) {
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
}

fn secs_until(t: Option<chrono::DateTime<Utc>>) -> i64 {
    (t.expect("deadline set") - Utc::now()).num_seconds()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn busy_while_a_turn_runs_and_not_after() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let (ok, idle) = wait_meta(&handle, Duration::from_secs(2), |m| m.state_since.is_some()).await;
    assert!(ok && !idle.busy, "attached, idle: {idle:?}");

    // The turn holds on a prompt: busy, dated from the turn start.
    a.send(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    let busy = handle.meta_live();
    assert!(busy.busy, "a running turn is busy");
    assert!(busy.state_since > idle.state_since, "state_since moved to the turn start");
    assert!(busy.park_due.is_none(), "attached: no park due");

    a.send(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("abc".into()),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let (ok, after) = wait_meta(&handle, Duration::from_secs(2), |m| !m.busy).await;
    assert!(ok, "the turn ended: not busy");
    assert!(after.state_since > busy.state_since, "state_since moved to the turn end");
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn title_cost_and_messages_after_a_turn() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let m = handle.meta_live();
    assert_eq!((m.title.as_deref(), m.messages), (None, 0), "fresh session");

    a.send(submit("any ai events in nyc today?")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(2), |m| m.messages == 2).await;
    assert!(ok, "two api messages after one turn: {m:?}");
    assert_eq!(m.title.as_deref(), Some("any ai events in nyc today?"));
    assert!(m.cost_usd > 0.0, "the turn's usage is costed: {}", m.cost_usd);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn park_due_set_after_the_last_detach_and_cleared_on_reattach() {
    let _h = Home::new();
    let _g = grace("60");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(handle.meta_live().park_due.is_none(), "a client is attached");

    detach(&mut a).await;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(2), |m| m.park_due.is_some()).await;
    assert!(ok, "the grace is armed: {m:?}");
    let left = secs_until(m.park_due);
    assert!((55..=60).contains(&left), "parks in ~60s, got {left}s");
    assert!(m.evict_due.is_none());
    assert!(!m.busy);

    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    let (ok, m) = wait_meta(&handle, Duration::from_secs(2), |m| m.park_due.is_none()).await;
    assert!(ok, "re-attach disarms the park: {m:?}");
    assert_eq!(m.clients, 1);
    end(&mut b).await;
}

/// While only workers hold the park the deadline is a re-check, not a park:
/// no `park_due`. Once the last worker is collected the grace arms and
/// `park_due` shows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn park_due_none_while_only_workers_hold_the_park() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = grace("60");
    let _r = WorkerParkRecheck::set(Duration::from_secs(600));
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    registry.lock().unwrap().register(agent_engine::tools::SubagentHandle::new(
        "sa_1".into(),
        1,
        "reviewer".into(),
        "review".into(),
        MODEL.into(),
        String::new(),
        0,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    let (armed, m) = wait_meta(&handle, Duration::from_millis(1500), |m| m.park_due.is_some()).await;
    assert!(!armed, "a worker re-check is not a park due: {m:?}");
    assert_eq!(m.lifecycle, SessionLifecycle::Live);
    assert_eq!(m.clients, 0);

    registry.lock().unwrap().get_mut("sa_1").unwrap().mark_collected();
    {
        let mut s = state.write().unwrap();
        s.status = agent_engine::tools::SubagentStatus::Completed;
        s.finished_at = Some(std::time::Instant::now());
        s.finalization_started = true;
        s.finalization_ready = true;
        s.completion_settled = true;
    }
    let (ok, m) = wait_meta(&handle, Duration::from_secs(4), |m| m.park_due.is_some()).await;
    assert!(ok, "the workers let go: the grace arms: {m:?}");
    let left = secs_until(m.park_due);
    assert!((50..=60).contains(&left), "the grace, not the 600s re-check: {left}s");
    shutdown(&handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn evict_due_set_when_parked_and_cleared_on_unpark() {
    let _h = Home::new();
    let _g = grace("0");
    let _e = Env::set("SYNAPS_DAEMON_PARKED_EVICT_SECS", "3000");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let live = handle.meta_live();
    detach(&mut a).await;

    let (ok, m) = wait_meta(&handle, Duration::from_secs(5), |m| {
        m.lifecycle == SessionLifecycle::Parked && m.evict_due.is_some()
    })
    .await;
    assert!(ok, "parked with an eviction deadline: {m:?}");
    let left = secs_until(m.evict_due);
    assert!((2990..=3000).contains(&left), "evicts in ~50m, got {left}s");
    assert!(m.park_due.is_none(), "an eviction is not a park due");
    assert!(m.state_since > live.state_since, "dated from the park");
    assert_eq!(m.title.as_deref(), Some("hello"), "title kept while parked");
    assert_eq!(m.messages, 2, "messages kept while parked");
    assert_eq!(m.cost_usd, live.cost_usd, "cost kept while parked");
    assert_eq!(m.subagents_running, 0, "a parked session runs no workers");

    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    let (ok, m) = wait_meta(&handle, Duration::from_secs(2), |m| {
        m.lifecycle == SessionLifecycle::Live && m.evict_due.is_none()
    })
    .await;
    assert!(ok, "unparked: no eviction: {m:?}");
    end(&mut b).await;
}

/// An older daemon's listing (no state fields) still decodes, as defaults.
#[test]
fn session_meta_without_the_state_fields_deserializes() {
    let meta = agent_engine::session::handle::echo::meta_for(&"s-1".into());
    let mut v = serde_json::to_value(&meta).unwrap();
    let obj = v.as_object_mut().unwrap();
    for k in [
        "busy",
        "state_since",
        "park_due",
        "evict_due",
        "title",
        "cost_usd",
        "messages",
        "subagents_running",
    ] {
        assert!(obj.remove(k).is_some(), "{k} is serialized");
    }
    let old: SessionMeta = serde_json::from_value(v).unwrap();
    assert!(!old.busy);
    assert_eq!((old.state_since, old.park_due, old.evict_due), (None, None, None));
    assert_eq!((old.title, old.cost_usd, old.messages), (None, 0.0, 0));
    assert_eq!(old.subagents_running, 0);
}

type Registry = Arc<std::sync::Mutex<agent_engine::runtime::subagent::SubagentRegistry>>;
type WorkerState = Arc<std::sync::RwLock<agent_engine::tools::SubagentState>>;

/// A fake running worker `id` in `registry` (no thread: it reads as running
/// until the test settles it, and a cancel only flags it).
fn register_worker(registry: &Registry, id: &str) -> WorkerState {
    let state = Arc::new(std::sync::RwLock::new(
        agent_engine::tools::SubagentState::new(),
    ));
    registry
        .lock()
        .unwrap()
        .register(agent_engine::tools::SubagentHandle::new(
            id.into(),
            id.trim_start_matches("sa_").parse().unwrap(),
            "reviewer".into(),
            "review".into(),
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

/// A turn that starts `workers` mid-turn (held on a prompt, like
/// `subagent_start` tool calls) and then ends: the listing counts them
/// during the turn and after it, with `busy` only during it.
async fn turn_that_leaves_workers_running(
    workers: &[&str],
) -> (SessionHandle, LocalTransport, Vec<WorkerState>, SessionMeta) {
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert_eq!(handle.meta_live().subagents_running, 0, "fresh session");

    a.send(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    let states: Vec<WorkerState> = workers
        .iter()
        .map(|id| register_worker(&registry, id))
        .collect();
    let n = workers.len();
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| {
        m.subagents_running == n
    })
    .await;
    assert!(ok && m.busy, "the workers show up during the turn: {m:?}");

    a.send(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("abc".into()),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let (ok, after) = wait_meta(&handle, Duration::from_secs(2), |m| !m.busy).await;
    assert!(ok, "the turn ended: not busy: {after:?}");
    assert_eq!(
        after.subagents_running, n,
        "the workers outlive the turn: {after:?}"
    );
    // Still counted across further worker ticks.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let m = handle.meta_live();
    assert_eq!((m.busy, m.subagents_running), (false, n), "{m:?}");
    (handle, a, states, after)
}

fn finish(state: &WorkerState) {
    let mut s = state.write().unwrap();
    s.status = agent_engine::tools::SubagentStatus::Completed;
    s.finished_at = Some(std::time::Instant::now());
    s.finalization_started = true;
    s.finalization_ready = true;
    s.completion_settled = true;
}

/// Each worker that finishes after the turn leaves the count on the next
/// listing: the first while another still runs (the worker tick
/// republishes), the last as the tick stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn subagents_running_outlives_the_turn_and_drops_as_workers_finish() {
    let _h = Home::new();
    let (handle, mut a, states, after) = turn_that_leaves_workers_running(&["sa_1", "sa_2"]).await;

    finish(&states[0]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| {
        m.subagents_running == 1
    })
    .await;
    assert!(ok, "one finished, one still running: {m:?}");
    finish(&states[1]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| {
        m.subagents_running == 0
    })
    .await;
    assert!(
        ok,
        "the last finished worker leaves the count, with no turn running: {m:?}"
    );
    assert!(!m.busy, "busy is the turn only");
    assert_eq!(
        m.state_since, after.state_since,
        "a worker ending is not a state change"
    );
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn subagents_running_drops_when_the_worker_is_cancelled() {
    let _h = Home::new();
    let (handle, mut a, states, _) = turn_that_leaves_workers_running(&["sa_1"]).await;

    a.send(SessionCommand::EngineCommand {
        id: 9,
        name: "workers".into(),
        arg: "cancel sa_1".into(),
    })
    .await
    .unwrap();
    until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 9, .. })
    })
    .await;
    assert!(states[0].read().unwrap().cancel_requested);
    // The fake worker's thread never winds down: it still reads as running
    // in the registry, but it is stopping, so the listing drops it. Read
    // straight after the reply (published before it), not polled: the
    // worker tick would hide a missing publish a second later.
    let m = handle.meta_live();
    assert_eq!(
        m.subagents_running, 0,
        "dropped at the cancel, not a tick later: {m:?}"
    );
    assert!(!m.busy);
    end(&mut a).await;
}

/// Esc stops the turn and the workers it launched: the listing drops them
/// with the turn's end, not a worker tick later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn subagents_running_drops_at_an_esc_cancel() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("go")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let state = register_worker(&registry, "sa_1");
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| {
        m.subagents_running == 1
    })
    .await;
    assert!(ok && m.busy, "the turn's worker shows up: {m:?}");

    a.send(SessionCommand::Cancel).await.unwrap();
    // `Aborted` follows the cancel's publish; read at once, not polled.
    until(&mut a, |e| matches!(e, SessionEventWire::Aborted { .. })).await;
    assert!(
        state.read().unwrap().cancel_requested,
        "Esc stops the turn's worker"
    );
    let m = handle.meta_live();
    assert_eq!(
        (m.busy, m.subagents_running),
        (false, 0),
        "dropped with the turn's end, not a tick later: {m:?}"
    );
    end(&mut a).await;
}

/// A worker tick still due when the session parks never reads the parked
/// runtime. A cancelled worker that just wound down holds no park, so with
/// a zero grace the park can beat the tick that would notice it is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_worker_tick_due_at_the_park_never_reads_the_parked_runtime() {
    let _h = Home::new();
    let _g = grace("0");
    let (handle, mut a, states, _) = turn_that_leaves_workers_running(&["sa_1"]).await;
    {
        let mut s = states[0].write().unwrap();
        s.cancel_requested = true;
        s.status = agent_engine::tools::SubagentStatus::Cancelled;
        s.finished_at = Some(std::time::Instant::now());
    }
    detach(&mut a).await;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(5), |m| {
        m.lifecycle == SessionLifecycle::Parked
    })
    .await;
    assert!(ok, "parked: {m:?}");
    assert_eq!(m.subagents_running, 0);
    // Past the tick that was due: the actor is still there to unpark.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .expect("the actor survived its worker tick");
    end(&mut b).await;
}
