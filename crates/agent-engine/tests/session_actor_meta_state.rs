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
