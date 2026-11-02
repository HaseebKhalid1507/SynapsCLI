//! Focus hold mode (S1), in the production actor against a loopback SSE
//! stub: while the delivery mode is `hold`, events go to the held store
//! instead of the context, and the lease releases them as ONE digest.
//! Contract: `pria-apps/.local/plans/2026-10-21-focus-side-chats-synaps-wire.md`.
//!
//! Guard rows here (each goes red with its gate undone): 1 a held event
//! starts no turn and stays out of the context; 2 the busy path holds; 3 the
//! post-turn trigger holds; 4 lease expiry gives ONE digest and at most one
//! turn; 5 hold survives a reload; 6 join_next goes in front of the next
//! Submit as one block; 7 individual is refused while hold; 8 dismiss keeps
//! the text; 9 an own-subagent completion during a turn still steers.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use agent_engine::events::{Event, Severity};
use agent_engine::session::display::is_event_payload;
use agent_engine::session::held::{DeliveryMode, DeliveryReason, HeldState, ReleaseAs};
use agent_engine::session::held_store::{item_for_event, Entry, HeldStore};
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, SessionCommand, SessionEventWire,
    SessionHandle, SessionQuery, SessionTask,
};
use agent_engine::tools::{build_completion_event, SubagentHandle, SubagentState, SubagentStatus};
use agent_engine::EngineHost;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use serial_test::serial;

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Hit `i` is answered `seq[min(i, len-1)]`; request bodies recorded.
async fn stub_capture(seq: &'static [&'static str]) -> (Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Bodies = Arc::default();
    let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (hits, seen) = (Arc::clone(&hits_c), Arc::clone(&seen_c));
        async move {
            let i = hits.fetch_add(1, Ordering::SeqCst);
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                seen.lock().unwrap().push(v);
            }
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                seq[i.min(seq.len() - 1)].to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    (hits, seen)
}

/// One complete assistant round that calls `name` with `{}`.
fn sse_tool(id: &str, name: &str) -> &'static str {
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_{id}\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,",
                "\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{}}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
            name = name,
        )
        .into_boxed_str(),
    )
}

/// Pushes its events into the session's queue mid-turn, then waits long
/// enough for the actor to take them before the turn's next round.
struct PushThenWait {
    events: Vec<Event>,
}

#[async_trait::async_trait]
impl agent_engine::Tool for PushThenWait {
    fn name(&self) -> &str {
        "push_then_wait"
    }
    fn description(&self) -> &str {
        "pushes events, then waits"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(&self, _params: Value, ctx: agent_engine::ToolContext) -> agent_engine::Result<String> {
        let queue = ctx.capabilities.event_queue.expect("runtime event queue");
        for e in &self.events {
            queue.push(e.clone()).expect("push");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok("done".into())
    }
}

async fn host_with(tools: Vec<Arc<dyn agent_engine::Tool>>) -> Arc<EngineHost> {
    let host = host().await;
    for t in tools {
        host.parts().tools.write().await.register(t);
    }
    host
}

async fn session(host: &Arc<EngineHost>) -> (SessionHandle, SessionTask) {
    host.create_session_unspawned(cfg()).await.unwrap()
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientJoined { .. })).await;
    t
}

fn held_path(task: &SessionTask) -> std::path::PathBuf {
    HeldStore::path_in(&HeldStore::dir_under(&agent_engine::config::base_dir()), task.id().as_str())
}

async fn set_hold(c: &mut LocalTransport, secs: u64) {
    c.send(SessionCommand::Delivery {
        mode: DeliveryMode::Hold,
        lease_secs: Some(secs),
    })
    .await
    .unwrap();
    until(c, |e| matches!(e, SessionEventWire::DeliveryChanged { mode: DeliveryMode::Hold, .. })).await;
}

async fn query(c: &mut LocalTransport, query: SessionQuery) -> Value {
    static NEXT: AtomicU64 = AtomicU64::new(9100);
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    c.send(SessionCommand::Query { id, query }).await.unwrap();
    let seen = until(c, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

fn started(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .count()
}

fn held_items(seen: &[Envelope]) -> Vec<agent_engine::session::held::HeldItem> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::EventHeld { item } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

/// The messages request `n` sent, as JSON text.
fn sent(bodies: &Bodies, n: usize) -> String {
    serde_json::to_string(&bodies.lock().unwrap()[n]["messages"]).unwrap()
}

/// The user messages of request `n` whose content holds `needle`.
fn user_messages_with(bodies: &Bodies, n: usize, needle: &str) -> Vec<String> {
    let b = bodies.lock().unwrap();
    b[n]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| serde_json::to_string(&m["content"]).unwrap())
        .filter(|c| c.contains(needle))
        .collect()
}

/// GUARD ROW 1: while hold, an idle session's event starts no turn and
/// doesn't enter the context; it is in the store (held_list / held_show).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_held_event_starts_no_turn_and_stays_out_of_the_context() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;

    queue
        .push(Event::simple("watcher", "deploy finished: v2 live\nsecond line", Some(Severity::High)))
        .unwrap();
    let seen = until(&mut c, |e| {
        matches!(e, SessionEventWire::EventHeld { .. } | SessionEventWire::TurnStarted { .. })
    })
    .await;
    let SessionEventWire::EventHeld { item } = &seen.last().unwrap().event else {
        panic!("the event started a turn: {seen:#?}");
    };
    assert_eq!(item.headline, "deploy finished: v2 live");
    assert_eq!(item.state, HeldState::Held);
    assert_eq!(item.source, "watcher");
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::External(_))), "not delivered");
    let later = drain_for(&mut c, Duration::from_millis(800)).await;
    assert_eq!(started(&later), 0, "{later:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["mode"], "hold");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], item.id.as_str());
    let show = query(&mut c, SessionQuery::HeldShow { held_id: item.id.clone() }).await;
    assert_eq!(show["text"], "deploy finished: v2 live\nsecond line");

    // The next user turn's request does not carry it.
    c.send(submit("next")).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let messages = sent(&bodies, 0);
    assert!(messages.contains("next"), "{messages}");
    assert!(!messages.contains("deploy finished"), "a held event reached the context: {messages}");
    end(&mut c).await;
}

/// GUARD ROW 2: while hold, an event arriving during a turn is not steered
/// into it (nor buffered for its end): the turn's next round doesn't carry
/// it and no turn follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_busy_path_holds() {
    let _h = Home::new();
    let seq: &'static [&'static str] = Box::leak(Box::new([sse_tool("toolu_1", "push_then_wait"), SSE_HI]));
    let (hits, bodies) = stub_capture(seq).await;
    let host = host_with(vec![Arc::new(PushThenWait {
        events: vec![Event::simple("watcher", "build finished: 3 warnings", None)],
    })])
    .await;
    let (handle, task) = session(&host).await;
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;

    c.send(submit("run the build")).await.unwrap();
    let mut seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut c, Duration::from_millis(800)).await);
    let held = held_items(&seen);
    assert_eq!(held.len(), 1, "{seen:#?}");
    assert_eq!(held[0].headline, "build finished: 3 warnings");
    assert_eq!(started(&seen), 1, "no turn after it: {seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the tool round and the reply");
    let round2 = sent(&bodies, 1);
    assert!(round2.contains("done"), "the tool result: {round2}");
    assert!(!round2.contains("build finished"), "steered into the turn: {round2}");
    end(&mut c).await;
}

/// GUARD ROW 3: while hold, events buffered during a turn go into history
/// at its end, but the post-turn event trigger starts no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_post_turn_trigger_holds() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, mut task) = session(&host).await;
    task.seed_pending_event(&Event::simple("watcher", "nightly report ready", None));
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;

    c.send(submit("hello")).await.unwrap();
    let mut seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut c, Duration::from_millis(1000)).await);
    assert_eq!(started(&seen), 1, "the buffered event started a turn: {seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // It went into history: the next turn carries it.
    c.send(submit("again")).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(sent(&bodies, 1).contains("nightly report ready"));
    end(&mut c).await;
}

/// The control for row 3 (the fixture reaches the trigger): in auto mode
/// the same buffered event starts its turn after the user's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn without_hold_the_buffered_event_starts_its_turn() {
    let _h = Home::new();
    let (hits, _) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, mut task) = session(&host).await;
    task.seed_pending_event(&Event::simple("watcher", "nightly report ready", None));
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;

    c.send(submit("hello")).await.unwrap();
    let seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(started(&seen), 2, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    end(&mut c).await;
}

/// A store file as the daemon writes it: hold until `lease_until_ms`, three
/// held items (ts order a, b, c). Returns their ids.
fn write_store(path: std::path::PathBuf, lease_until_ms: u64) -> Vec<String> {
    let now = agent_engine::epoch_millis();
    let mut store = HeldStore::open(path, now);
    store.set_mode(DeliveryMode::Hold, Some(lease_until_ms), now);
    let mut ids = Vec::new();
    for (i, (text, sev)) in [
        ("alpha arrived", Severity::Low),
        ("bravo arrived", Severity::High),
        ("charlie arrived", Severity::Medium),
    ]
    .into_iter()
    .enumerate()
    {
        let ev = Event::simple("watcher", text, Some(sev));
        let ts = now - 1000 + i as u64;
        let (item, text) = item_for_event(&ev, false, ts);
        let added = store.add(Entry { item, text, event: Some(ev), changed_ms: ts }, ts);
        ids.push(added.id);
    }
    ids
}

/// GUARD ROW 4: the lease runs out: auto, and every held item is released
/// as ONE digest (one user message, one `held_digest` mark) with at most one
/// turn. The hold came from the store at the actor's start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn lease_expiry_gives_one_digest_and_at_most_one_turn() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let lease_until = agent_engine::epoch_millis() + 3000;
    let ids = write_store(held_path(&task), lease_until);
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;

    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["mode"], "hold", "restored from the store: {list}");
    assert_eq!(list["lease_until_ms"], lease_until);
    assert_eq!(list["items"].as_array().unwrap().len(), 3);

    let mut seen = until(&mut c, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut c, Duration::from_millis(1000)).await);
    let expired: Vec<_> = seen
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                SessionEventWire::DeliveryChanged {
                    mode: DeliveryMode::Auto,
                    lease_until_ms: None,
                    reason: DeliveryReason::LeaseExpired
                }
            )
        })
        .collect();
    assert_eq!(expired.len(), 1, "{seen:#?}");
    let released: Vec<_> = seen
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::HeldUpdated { ids, state: HeldState::Released } => Some(ids.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(released, vec![ids.clone()], "all three at once, arrival order");
    let digests: Vec<&Event> = seen
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::External(ev) if ev.content.content_type == "held_digest" => Some(ev),
            _ => None,
        })
        .collect();
    assert_eq!(digests.len(), 1, "ONE digest: {seen:#?}");
    let d = digests[0];
    assert_eq!(d.source.source_type, "synaps");
    assert_eq!(d.source.name, "held");
    assert_eq!(d.content.data, Some(json!({ "held_ids": ids, "reason": "lease_expired" })));
    assert_eq!(started(&seen), 1, "at most one turn: {seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let in_context = user_messages_with(&bodies, 0, "held_digest");
    assert_eq!(in_context.len(), 1, "one user message: {in_context:?}");
    let digest = &in_context[0];
    assert!(digest.contains(&format!("- [low] watcher: alpha arrived ({})", ids[0])), "{digest}");
    assert!(digest.contains(&format!("- [high] watcher: bravo arrived ({})", ids[1])), "{digest}");
    assert!(digest.contains(&format!("- [medium] watcher: charlie arrived ({})", ids[2])), "{digest}");

    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list, json!({ "mode": "auto", "lease_until_ms": null, "items": [] }));
    end(&mut c).await;
}

/// GUARD ROW 5: hold survives a reload. The reload keeps the session id, so
/// the next image's actor reads the same store file: the mode, the lease and
/// the items come back, and a new event is held, not delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn hold_survives_a_reload() {
    let _h = Home::new();
    let (hits, _) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;

    let (handle_a, task_a) = session(&host).await;
    let path_a = held_path(&task_a);
    let queue_a = task_a.event_queue();
    tokio::spawn(task_a.run());
    let mut a = attach(&handle_a).await;
    set_hold(&mut a, 600).await;
    queue_a.push(Event::simple("watcher", "held before the reload", None)).unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::EventHeld { .. })).await;
    let before = query(&mut a, SessionQuery::HeldList).await;

    // The next image: the same session's file, read at its actor's start.
    let (handle_b, task_b) = session(&host).await;
    std::fs::copy(&path_a, held_path(&task_b)).unwrap();
    let queue_b = task_b.event_queue();
    tokio::spawn(task_b.run());
    let mut b = attach(&handle_b).await;
    let after = query(&mut b, SessionQuery::HeldList).await;
    assert_eq!(after, before, "mode, lease and items come back");
    assert_eq!(after["mode"], "hold");

    queue_b.push(Event::simple("watcher", "after the reload", None)).unwrap();
    let seen = until(&mut b, |e| {
        matches!(e, SessionEventWire::EventHeld { .. } | SessionEventWire::TurnStarted { .. })
    })
    .await;
    assert!(
        matches!(&seen.last().unwrap().event, SessionEventWire::EventHeld { item } if item.headline == "after the reload"),
        "{seen:#?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    end(&mut a).await;
    end(&mut b).await;
}

/// GUARD ROW 9: while hold, a completion of this session's own subagent
/// that arrives during a turn still steers into it; another event that
/// arrives with it is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_own_subagent_completion_during_a_turn_still_steers() {
    let _h = Home::new();
    let seq: &'static [&'static str] = Box::leak(Box::new([sse_tool("toolu_1", "push_then_wait"), SSE_HI]));
    let (hits, bodies) = stub_capture(seq).await;
    let completion =
        build_completion_event("sa_1", 1, "scout", &SubagentStatus::Completed, "SCOUT-RESULT", 1.0, None, None, 0);
    let host = host_with(vec![Arc::new(PushThenWait {
        events: vec![completion, Event::simple("watcher", "unrelated news", None)],
    })])
    .await;
    let (handle, task) = session(&host).await;
    // A fake worker of this session (no thread): the completion's handle
    // is in the registry, which is what makes it the session's own.
    let state = Arc::new(RwLock::new(SubagentState::new()));
    task.subagent_registry().lock().unwrap().register(SubagentHandle::new(
        "sa_1".into(),
        1,
        "scout".into(),
        "work".into(),
        MODEL.into(),
        String::new(),
        300,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;

    c.send(submit("scout it")).await.unwrap();
    let seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let round2 = sent(&bodies, 1);
    assert!(round2.contains("SCOUT-RESULT"), "the own completion steered in: {round2}");
    assert!(!round2.contains("unrelated news"), "{round2}");
    let held = held_items(&seen);
    assert_eq!(held.len(), 1, "{seen:#?}");
    assert_eq!(held[0].headline, "unrelated news");
    assert!(!held[0].own_subagent);
    end(&mut c).await;
}

/// Push `event` and wait for it to be held; its held id.
async fn hold_one(c: &mut LocalTransport, queue: &agent_engine::events::EventQueue, event: Event) -> String {
    queue.push(event).unwrap();
    let seen = until(c, |e| matches!(e, SessionEventWire::EventHeld { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::EventHeld { item } => item.id.clone(),
        _ => unreachable!(),
    }
}

async fn updated(c: &mut LocalTransport) -> (Vec<String>, HeldState) {
    let seen = until(c, |e| matches!(e, SessionEventWire::HeldUpdated { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::HeldUpdated { ids, state } => (ids.clone(), *state),
        _ => unreachable!(),
    }
}

fn release(ids: Option<Vec<String>>, release_as: ReleaseAs, text: Option<&str>) -> SessionCommand {
    SessionCommand::HeldRelease {
        ids,
        release_as,
        text: text.map(str::to_string),
    }
}

/// The content of the last user message request `n` sent.
fn last_user_content(bodies: &Bodies, n: usize) -> Value {
    let b = bodies.lock().unwrap();
    b[n]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"]
        .clone()
}

/// GUARD ROW 6: join_next goes in front of the next Submit as ONE block:
/// the user message's first text block is one `held_join` envelope holding
/// each item's own envelope (arrival order), then the app's text; the
/// user's text is the next block. No turn of its own; the mark comes just
/// before TurnStarted; the Submit after it carries no block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn join_next_goes_in_front_of_the_next_submit_as_one_block() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;
    let a = hold_one(&mut c, &queue, Event::simple("watcher", "first news", None)).await;
    let b = hold_one(&mut c, &queue, Event::simple("master", "second news", Some(Severity::High))).await;

    c.send(release(None, ReleaseAs::JoinNext, Some("[side Fix tray] Summary: done")))
        .await
        .unwrap();
    assert_eq!(updated(&mut c).await, (vec![a.clone(), b.clone()], HeldState::JoinNext));
    let quiet = drain_for(&mut c, Duration::from_millis(800)).await;
    assert_eq!(started(&quiet), 0, "no turn of its own: {quiet:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    c.send(submit("my question")).await.unwrap();
    let seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(started(&seen), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let at = seen
        .iter()
        .position(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .unwrap();
    let SessionEventWire::External(mark) = &seen[at - 1].event else {
        panic!("the held_join mark is not just before TurnStarted: {seen:#?}");
    };
    assert_eq!(mark.content.content_type, "held_join");
    assert_eq!((mark.source.source_type.as_str(), mark.source.name.as_str()), ("synaps", "held"));
    assert_eq!(mark.content.data, Some(json!({ "held_ids": [a, b], "has_text": true })));
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::HeldUpdated { ids, state: HeldState::Released } if ids.len() == 2
    )));

    let content = last_user_content(&bodies, 0);
    let blocks = content.as_array().expect("array content");
    assert_eq!(blocks.len(), 2, "{content:#}");
    let block = blocks[0]["text"].as_str().unwrap();
    // The request may add a cache breakpoint to the last block.
    assert_eq!((&blocks[1]["type"], &blocks[1]["text"]), (&json!("text"), &json!("my question")), "{content:#}");
    assert!(is_event_payload(block), "{block}");
    assert_eq!(block.matches("type=\"held_join\"").count(), 1, "{block}");
    assert_eq!(block.matches("<event ").count(), 3, "the outer and the two items: {block}");
    assert_eq!(block.matches("</event>").count(), 3, "{block}");
    let (first, second, text) = (
        block.find("first news").unwrap(),
        block.find("second news").unwrap(),
        block.find("[side Fix tray] Summary: done").unwrap(),
    );
    assert!(first < second && second < text, "arrival order, then the text: {block}");
    assert!(block.contains("type=\"message\" severity=\"high\" source=\"master\""), "its own envelope: {block}");
    assert_eq!(user_messages_with(&bodies, 0, "first news").len(), 1, "in one block only");
    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["items"], json!([]), "{list}");

    c.send(submit("again")).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    // No join block: the user's text alone (the request may turn it into
    // one block with a cache breakpoint).
    let again = last_user_content(&bodies, 1);
    let texts: Vec<&str> = match &again {
        Value::String(s) => vec![s.as_str()],
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => Vec::new(),
    };
    assert_eq!(texts, vec!["again"], "{again:#}");
    end(&mut c).await;
}

/// GUARD ROW 7: individual is refused while hold, and the item stays held;
/// in auto it goes back into the queue and is delivered as today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn individual_is_refused_while_hold() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;
    let id = hold_one(&mut c, &queue, Event::simple("watcher", "lonely news", None)).await;

    // From this client, so the refusal comes back as `Refused`.
    c.send_from_self(release(Some(vec![id.clone()]), ReleaseAs::Individual, None)).await.unwrap();
    let seen = until(&mut c, |e| {
        matches!(e, SessionEventWire::Refused { .. } | SessionEventWire::HeldUpdated { .. })
    })
    .await;
    let SessionEventWire::Refused { command, reason, .. } = &seen.last().unwrap().event else {
        panic!("individual was not refused while hold: {seen:#?}");
    };
    assert_eq!(command, "held_release");
    assert!(reason.contains("individual"), "{reason}");
    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["items"][0]["id"], id.as_str());
    assert_eq!(list["items"][0]["state"], "held");
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    c.send(SessionCommand::Delivery {
        mode: DeliveryMode::Auto,
        lease_secs: None,
    })
    .await
    .unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::DeliveryChanged { mode: DeliveryMode::Auto, .. })).await;
    c.send(release(Some(vec![id.clone()]), ReleaseAs::Individual, None)).await.unwrap();
    let seen = until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::HeldUpdated { ids, state: HeldState::Released } if *ids == vec![id.clone()]
    )));
    assert_eq!(started(&seen), 1, "delivered as today: an idle wake");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(sent(&bodies, 0).contains("lonely news"));
    end(&mut c).await;
}

/// GUARD ROW 8: dismiss keeps the text: dismissed and out of held_list,
/// but held_show and the store file still have it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn dismiss_keeps_the_text() {
    let _h = Home::new();
    let (hits, _) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let path = held_path(&task);
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;
    let full = "dismiss me\nwith the whole story below";
    let id = hold_one(&mut c, &queue, Event::simple("watcher", full, None)).await;

    c.send(SessionCommand::HeldDismiss { ids: vec![id.clone()] }).await.unwrap();
    assert_eq!(updated(&mut c).await, (vec![id.clone()], HeldState::Dismissed));
    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["items"], json!([]), "{list}");
    let show = query(&mut c, SessionQuery::HeldShow { held_id: id.clone() }).await;
    assert_eq!(show["state"], "dismissed");
    assert_eq!(show["text"], full);
    let on_disk = HeldStore::open(path, agent_engine::epoch_millis());
    let entry = on_disk.get(&id).expect("still in the store");
    assert_eq!(entry.item.state, HeldState::Dismissed);
    assert_eq!(entry.text, full);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    end(&mut c).await;
}

/// held_mark records the side session, and a digest of the item names it;
/// an unknown id is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_marked_item_is_named_in_the_digest() {
    let _h = Home::new();
    let (hits, bodies) = stub_capture(&[SSE_HI]).await;
    let host = host_with(vec![]).await;
    let (handle, task) = session(&host).await;
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let mut c = attach(&handle).await;
    set_hold(&mut c, 300).await;
    let id = hold_one(&mut c, &queue, Event::simple("watcher", "tray is broken", None)).await;

    // From this client, so the refusal comes back as `Refused`.
    c.send_from_self(SessionCommand::HeldMark {
        id: "h-0000000000000000".into(),
        side_session: "x".into(),
    })
    .await
    .unwrap();
    let seen = until(&mut c, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    assert!(matches!(&seen.last().unwrap().event, SessionEventWire::Refused { reason, .. } if reason.starts_with("unknown held id")));

    let side = "20261021-013000-ab12";
    c.send(SessionCommand::HeldMark {
        id: id.clone(),
        side_session: side.into(),
    })
    .await
    .unwrap();
    assert_eq!(updated(&mut c).await, (vec![id.clone()], HeldState::HandledOnSide));
    let list = query(&mut c, SessionQuery::HeldList).await;
    assert_eq!(list["items"][0]["state"], "handled_on_side");
    assert_eq!(list["items"][0]["side_session"], side);

    c.send(release(None, ReleaseAs::Digest, None)).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let digest = user_messages_with(&bodies, 0, "held_digest");
    assert_eq!(digest.len(), 1);
    assert!(
        digest[0].contains(&format!("- [medium] watcher: tray is broken ({id}) (handled on the side: {side})")),
        "{}",
        digest[0]
    );
    end(&mut c).await;
}
