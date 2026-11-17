//! `SessionMeta` state for the `/sessions` modal: busy / state_since /
//! park_due / evict_due / title / cost / messages, as the actor publishes
//! them through the handle's presence (`meta_live`), and the workers that
//! hold the park (`SessionMeta::workers`).

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, EndReason, Envelope, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle, SessionMeta,
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
    assert!(m.workers.is_empty(), "a parked session lists no workers");

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
    for k in ["busy", "state_since", "park_due", "evict_due", "title", "cost_usd", "messages"] {
        assert!(obj.remove(k).is_some(), "{k} is serialized");
    }
    let old: SessionMeta = serde_json::from_value(v).unwrap();
    assert!(!old.busy);
    assert_eq!((old.state_since, old.park_due, old.evict_due), (None, None, None));
    assert_eq!((old.title, old.cost_usd, old.messages), (None, 0.0, 0));
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

/// `(handle_id, status, cancel_requested)` of every listed worker.
fn rows(m: &SessionMeta) -> Vec<(String, String, bool)> {
    m.workers
        .rows
        .iter()
        .map(|r| (r.handle_id.clone(), r.status.clone(), r.cancel_requested))
        .collect()
}

fn row(id: &str, status: &str, cancel: bool) -> (String, String, bool) {
    (id.into(), status.into(), cancel)
}

/// A turn that starts `workers` mid-turn (held on a prompt, like
/// `subagent_start` tool calls) and then ends: the listing shows them
/// during the turn and after it, with `busy` only during it.
async fn turn_that_leaves_workers_running(
    cfg: SessionConfig,
    workers: &[&str],
) -> (
    SessionHandle,
    LocalTransport,
    Registry,
    Vec<WorkerState>,
    SessionMeta,
) {
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let (handle, task) = host.create_session_unspawned(cfg).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert!(handle.meta_live().workers.is_empty(), "fresh session");

    a.send(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    let states: Vec<WorkerState> = workers
        .iter()
        .map(|id| register_worker(&registry, id))
        .collect();
    let n = workers.len() as u32;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == n).await;
    assert!(ok && m.busy, "the workers show up during the turn: {m:?}");
    assert!(m
        .workers
        .rows
        .iter()
        .all(|r| r.status == "running" && r.finished_at.is_none()));

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
        after.workers.running, n,
        "the workers outlive the turn: {after:?}"
    );
    assert_eq!(
        after.workers_tag(),
        if n == 1 {
            " · 1 worker".to_string()
        } else {
            format!(" · {n} workers")
        }
    );
    // Still listed across further worker ticks.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let m = handle.meta_live();
    assert_eq!((m.busy, m.workers.running), (false, n), "{m:?}");
    (handle, a, registry, states, after)
}

fn finish(state: &WorkerState) {
    let mut s = state.write().unwrap();
    s.status = agent_engine::tools::SubagentStatus::Completed;
    s.stamp_finished();
    s.finalization_started = true;
    s.finalization_ready = true;
    s.completion_settled = true;
}

/// A worker that finishes after the turn stays listed as `completed` (its
/// result is uncollected: it holds the park) and leaves `running`: the
/// first while another still runs (the worker tick republishes), the last
/// as the tick stops. Collecting it drops the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn workers_outlive_the_turn_and_finish_into_completed_rows() {
    let _h = Home::new();
    let (handle, mut a, registry, states, after) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1", "sa_2"]).await;

    finish(&states[0]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 1).await;
    assert!(ok, "one finished, one still running: {m:?}");
    assert_eq!(
        rows(&m),
        [
            row("sa_2", "running", false),
            row("sa_1", "completed", false)
        ]
    );
    let done = &m.workers.rows[1];
    assert!(done.finished_at.is_some_and(|t| t >= done.started_at));
    finish(&states[1]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 0).await;
    assert!(ok, "the last one finished, with no turn running: {m:?}");
    assert_eq!(m.workers.rows.len(), 2, "both results uncollected: listed");
    assert!(m.workers.rows.iter().all(|r| r.status == "completed"));
    assert!(!m.busy, "busy is the turn only");
    assert_eq!(
        m.state_since, after.state_since,
        "a worker ending is not a state change"
    );

    // Collected (a later turn's `subagent_collect`): the row goes with the
    // publish that turn ends with.
    for id in ["sa_1", "sa_2"] {
        registry
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .mark_collected();
    }
    a.send(submit("next")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.is_empty()).await;
    assert!(ok, "collected: no longer listed: {m:?}");
    end(&mut a).await;
}

/// A finished worker whose result is uncollected holds the park: it stays
/// listed and the detached session does not park.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_finished_uncollected_worker_stays_listed_and_holds_the_park() {
    let _h = Home::new();
    let _g = grace("0");
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    finish(&states[0]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 0).await;
    assert!(ok, "{m:?}");
    detach(&mut a).await;
    let (parked, m) = wait_meta(&handle, Duration::from_millis(2000), |m| {
        m.lifecycle != SessionLifecycle::Live
    })
    .await;
    assert!(!parked, "an uncollected result holds the park: {m:?}");
    assert_eq!(rows(&m), [row("sa_1", "completed", false)]);
    shutdown(&handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cancelled_worker_leaves_running_at_once() {
    let _h = Home::new();
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;

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
    // The fake worker never winds down: it still runs (and holds the park),
    // but it is stopping. Read straight after the reply (published before
    // it), not polled: the worker tick would hide a missing publish.
    let m = handle.meta_live();
    assert_eq!(
        m.workers.running, 0,
        "stopping at the cancel, not a tick later: {m:?}"
    );
    assert_eq!(rows(&m), [row("sa_1", "running", true)]);
    assert!(!m.busy);

    // Once it has wound down it holds nothing: the tick's last pass drops it.
    {
        let mut s = states[0].write().unwrap();
        s.status = agent_engine::tools::SubagentStatus::Cancelled;
        s.stamp_finished();
    }
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.is_empty()).await;
    assert!(ok, "a cancelled worker owes no result: {m:?}");
    end(&mut a).await;
}

/// Esc stops the turn and the workers it launched: the listing shows them
/// stopping with the turn's end, not a worker tick later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_esc_cancel_shows_at_once() {
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
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 1).await;
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
        (m.busy, m.workers.running),
        (false, 0),
        "with the turn's end: {m:?}"
    );
    assert_eq!(rows(&m), [row("sa_1", "running", true)]);
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
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    {
        let mut s = states[0].write().unwrap();
        s.cancel_requested = true;
        s.status = agent_engine::tools::SubagentStatus::Cancelled;
        s.stamp_finished();
    }
    detach(&mut a).await;
    let (ok, m) = wait_meta(&handle, Duration::from_secs(5), |m| {
        m.lifecycle == SessionLifecycle::Parked
    })
    .await;
    assert!(ok, "parked: {m:?}");
    assert!(m.workers.is_empty(), "parked: no workers");
    // Past the tick that was due: the actor is still there to unpark.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .expect("the actor survived its worker tick");
    end(&mut b).await;
}

/// Restores the finished-result TTL on drop.
struct Ttl;
impl Ttl {
    fn set(d: Duration) -> Self {
        agent_engine::runtime::subagent::set_finished_handle_ttl_for_test(Some(d));
        Self
    }
}
impl Drop for Ttl {
    fn drop(&mut self) {
        agent_engine::runtime::subagent::set_finished_handle_ttl_for_test(None);
    }
}

/// On a pinned, attached session (no park re-check, no detach to re-arm
/// anything) a finished worker's row leaves the listing when its result
/// expires, with no other event: the actor's expiry one-shot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_finished_row_expires_on_a_pinned_attached_session() {
    let _h = Home::new();
    let _t = Ttl::set(Duration::from_secs(3));
    let (handle, mut a, _registry, states, _) = turn_that_leaves_workers_running(
        SessionConfig {
            keep_warm: true,
            ..pcfg()
        },
        &["sa_1"],
    )
    .await;
    assert!(handle.pinned());
    finish(&states[0]);
    let t0 = std::time::Instant::now();
    let (ok, m) = wait_meta(&handle, Duration::from_secs(2), |m| m.workers.running == 0).await;
    assert!(ok, "{m:?}");
    assert_eq!(rows(&m), [row("sa_1", "completed", false)]);
    // The tick has stopped (nothing runs); nothing else happens until the
    // result expires, 3 s after it finished.
    let (gone, m) = wait_meta(&handle, Duration::from_secs(6), |m| m.workers.is_empty()).await;
    assert!(gone, "expired: no longer listed: {m:?}");
    assert!(
        t0.elapsed() >= Duration::from_millis(2500),
        "not before the TTL: {:?}",
        t0.elapsed()
    );
    assert_eq!(m.lifecycle, SessionLifecycle::Live);
    end(&mut a).await;
}

/// A streaming worker whose only change is its progress line causes no
/// publish: rows carry no step text (not sampled in this PR), so the block
/// stays the same `Arc` across worker ticks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_worker_step_alone_never_republishes() {
    let _h = Home::new();
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    let before = handle.workers_cell_for_test();
    for i in 0..5u32 {
        states[0]
            .write()
            .unwrap()
            .note_progress(&format!("⚙ bash (tool #{i})"), i);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let after = handle.workers_cell_for_test();
    assert!(
        Arc::ptr_eq(&before, &after),
        "no publish across the worker ticks"
    );
    assert!(after.rows.iter().all(|r| r.step.is_empty() && r.tools == 0));
    end(&mut a).await;
}

/// `/close force` stops the running workers and ends the session. From the
/// moment it starts to end it lists no workers: no `running` rows for
/// workers that are being torn down with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_closing_session_lists_no_workers() {
    let _h = Home::new();
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    a.send_from_self(SessionCommand::EngineCommand {
        id: 9,
        name: "close".into(),
        arg: "force".into(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(
        states[0].read().unwrap().cancel_requested,
        "force stops the worker"
    );
    let m = handle.meta_live();
    assert!(m.workers.is_empty(), "an ending session lists none: {m:?}");
}

/// The 1 Hz worker tick stops once no worker runs and no turn does:
/// nothing more arrives after the last worker finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_worker_tick_stops_once_nothing_runs() {
    let _h = Home::new();
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    let is_rows = |e: &Envelope| matches!(e.event, SessionEventWire::SubagentRows(_));
    let ticking = drain_for(&mut a, Duration::from_millis(1500)).await;
    assert!(
        ticking.iter().any(is_rows),
        "the tick runs while the worker does"
    );
    finish(&states[0]);
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 0).await;
    assert!(ok, "{m:?}");
    // The stopping tick's last rows were sent before that publish.
    drain_for(&mut a, Duration::from_millis(100)).await;
    let after = drain_for(&mut a, Duration::from_millis(1500)).await;
    assert!(!after.iter().any(is_rows), "the tick stopped: {after:?}");
    end(&mut a).await;
}

/// A worker that finishes right after the tick read its rows (the test seam
/// runs there) keeps the tick one more beat, which sends it finished.
/// Deciding from a second registry walk would stop the tick on rows that
/// show it running, and clients would keep that ghost row (after a
/// `/workers cancel`, whose wake is withheld, until the next turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_tick_never_stops_on_rows_that_show_a_worker_running() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _h = Home::new();
    let (handle, mut a, _registry, states, _) =
        turn_that_leaves_workers_running(pcfg(), &["sa_1"]).await;
    drain_for(&mut a, Duration::from_millis(100)).await;
    let armed = Arc::new(AtomicBool::new(true));
    {
        let (state, armed) = (Arc::clone(&states[0]), Arc::clone(&armed));
        agent_engine::runtime::subagent::set_after_tick_rows_for_test(Some(Box::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                finish(&state);
            }
        })));
    }
    let events = drain_for(&mut a, Duration::from_millis(3500)).await;
    agent_engine::runtime::subagent::set_after_tick_rows_for_test(None);
    assert!(!armed.load(Ordering::SeqCst), "a tick ran the seam");
    assert_eq!(handle.meta_live().workers.running, 0);
    let last = events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEventWire::SubagentRows(rows) => Some(rows.clone()),
            _ => None,
        })
        .expect("the tick sent rows");
    assert!(
        last.iter()
            .all(|r| r.status != agent_engine::runtime::subagent::SubagentStatus::Running),
        "the last rows clients get show no worker running: {last:?}"
    );
    let after = drain_for(&mut a, Duration::from_millis(1500)).await;
    assert!(
        !after
            .iter()
            .any(|e| matches!(e.event, SessionEventWire::SubagentRows(_))),
        "and the tick stopped: {after:?}"
    );
    end(&mut a).await;
}

/// Over the socket: `SessionList` and `Welcome.sessions` carry the worker
/// block (keys at their default left out), and `Attached.meta` never does
/// (design Q14: workers come from the listing, not the attach).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn workers_cross_the_socket_in_listings_never_in_attached() {
    use agent_engine::session::socket_transport::SocketTransport;
    use agent_engine::session::wire::{Attach, Hello};
    let home = Home::new();
    let host = host().await;
    let d = agent_engine::daemon::Daemon::start(
        Arc::clone(&host),
        agent_engine::daemon::DaemonOpts {
            runtime_dir: Some(home.dir.path().join("run")),
            ..Default::default()
        },
    )
    .await
    .expect("daemon start");
    let sock = d.paths.sock.clone();
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let _worker = register_worker(&registry, "sa_1");
    // An attach is a transition: its publish carries the block.
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let (ok, m) = wait_meta(&handle, Duration::from_secs(3), |m| m.workers.running == 1).await;
    assert!(ok, "{m:?}");

    let listed = SocketTransport::sessions(&sock).await.unwrap();
    let m = listed.iter().find(|m| m.id == handle.id).expect("listed");
    assert_eq!(m.workers.running, 1);
    assert_eq!(rows(m), [row("sa_1", "running", false)]);
    let raw = SocketTransport::sessions_raw(&sock).await.unwrap();
    let w = &raw
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == handle.id.as_str())
        .expect("listed")["workers"];
    assert_eq!(w["running"], 1, "{w}");
    assert_eq!(
        (&w["rows"][0]["handle_id"], &w["rows"][0]["status"]),
        (&serde_json::json!("sa_1"), &serde_json::json!("running"))
    );
    assert!(
        w.get("more").is_none() && w["rows"][0].get("cancel_requested").is_none(),
        "defaults are left out: {w}"
    );

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let welcomed = conn
        .welcome
        .sessions
        .iter()
        .find(|m| m.id == handle.id)
        .expect("welcomed");
    assert_eq!(welcomed.workers.running, 1);
    let (_t, snap) = SocketTransport::attach(
        conn,
        Attach::Existing {
            session_id: handle.id.clone(),
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();
    assert!(
        snap.meta.workers.is_empty(),
        "Attached.meta is the create-time record: {:?}",
        snap.meta.workers
    );

    end(&mut a).await;
    SocketTransport::shutdown(&sock, true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait())
        .await
        .expect("daemon wait");
}
