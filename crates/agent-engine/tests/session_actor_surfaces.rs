//! Agent cards in the session actor (agent-ui C4a): the `ui` tool, the
//! card table, upserts, `Attached.surfaces`, presence and `/ui on|off`.
//! Over `LocalTransport` with the loopback SSE stub.

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, SessionConfig,
    SessionEventWire,
};
use agent_engine::ui::{Phase, SurfaceState, WaitMode};
use serial_test::serial;

/// A turn whose model calls `ui` once (`toolu_ui1`); the next round serves
/// `SSE_HI`.
const SSE_UI_CREATE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_u1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_ui1\",\"name\":\"ui\",\"input\":{}}}\n\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"question\":\"Which approach?\",\"options\":[\"Refactor\",\"Leave it\"],\"default\":\"Refactor\"}"}}"#,
    "\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn cards_cfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        surfaces: Some(true),
        ..cfg()
    }
}

/// A client that renders and answers catalog 1 (a D3 desk).
fn capable(kind: ClientKind) -> ClientMeta {
    let mut m = ClientMeta::new(kind);
    m.capabilities.surfaces = 1;
    m
}

fn cards(seen: &[Envelope]) -> Vec<SurfaceState> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Surface(s) => Some((**s).clone()),
            _ => None,
        })
        .collect()
}

/// T1: the model asks; the card is durable before anyone sees it, the tool
/// returns `shown` with its id, a capable client gets one upsert, a later
/// attach finds it in `Attached.surfaces`, and presence counts it open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_ui_call_raises_a_durable_live_card() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_UI_CREATE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    let upserts = cards(&seen);
    assert_eq!(upserts.len(), 1, "one upsert: {upserts:?}");
    let card = &upserts[0];
    assert_eq!((card.phase, card.mode, card.rev, card.catalog), (Phase::Open, WaitMode::Live, 1, 1));
    assert_eq!(card.call_id, "toolu_ui1");
    assert_eq!(card.doc.question, "Which approach?");
    assert!(agent_engine::ui::is_host_id(&card.id, "s_"), "{}", card.id);
    assert!(card.contract.starts_with("c1:"));
    assert_eq!(card.origin.task, "ask me");
    assert!(card.answer.is_none() && card.actions.is_empty());

    let results = tool_results(&last_conversation(&seen));
    let shown: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!(shown["status"], "shown", "{results:?}");
    assert_eq!(shown["id"], card.id.as_str());

    // Durable: the sidecar holds it.
    let journal = handle.meta_live().journal_id;
    let path = agent_engine::core::session_lock::sessions_dir().join(format!("{journal}.ui"));
    let bytes = std::fs::read(&path).expect("the sidecar was written");
    let sc = agent_engine::core::session_surfaces::decode_sidecar(&bytes).unwrap();
    assert_eq!(sc.card(&card.id), Some(card));

    // A later attach finds it; presence counts it.
    let (mut b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Attach)).await.unwrap();
    assert_eq!(snap.surfaces, vec![card.clone()]);
    let meta = handle.meta_live();
    assert_eq!((meta.surfaces_open, meta.surfaces_waiting), (1, 0));
    end(&mut b).await;
}

/// §1.5: `ui.surfaces` is off by default. The chat is not offered `ui`,
/// and `/ui on` offers it on the same session, `/ui off` withholds it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ui_is_withheld_until_cards_are_on() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let runtime = task.runtime_for_harness();
    tokio::spawn(task.run());
    let offered = |schema: std::sync::Arc<Vec<serde_json::Value>>| {
        schema.iter().any(|t| t["name"] == "ui")
    };
    assert!(!runtime.cards_enabled());
    assert!(!offered(runtime.offered_tools_schema().await), "off by default");

    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    for (arg, on) in [("on", true), ("off", false)] {
        a.send(agent_engine::session::SessionCommand::EngineCommand {
            id: 7,
            name: "ui".into(),
            arg: arg.into(),
        })
        .await
        .unwrap();
        let seen = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: 7, .. })).await;
        let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
        assert_eq!(value["on"], on, "{value}");
        assert_eq!(runtime.cards_enabled(), on);
        assert_eq!(offered(runtime.offered_tools_schema().await), on, "/ui {arg}");
    }
    end(&mut a).await;
}

/// §1.5's capable answerer: with only a client that can't answer cards
/// attached (catalog 0, this build's TUI), the tool says `unavailable` and
/// nothing is stored or sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn no_capable_client_attached_is_unavailable() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_UI_CREATE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(cards(&seen).is_empty());
    let results = tool_results(&last_conversation(&seen));
    let v: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!((v["status"].as_str(), v["note"].as_str()), (Some("unavailable"), Some("ask in plain text")));
    let journal = handle.meta_live().journal_id;
    let path = agent_engine::core::session_lock::sessions_dir().join(format!("{journal}.ui"));
    assert!(!path.exists(), "nothing stored");
    drain_for(&mut a, Duration::from_millis(50)).await;
    end(&mut a).await;
}

// ── answers (T2/T3, §3.3, §3.5) ─────────────────────────────────────────────

use agent_engine::session::{AttachMode, SessionCommand, SurfaceOutcome, SurfaceRefusal, SurfaceResult};

fn answer(card: &SurfaceState, action_id: &str, values: serde_json::Value) -> SessionCommand {
    SessionCommand::SurfaceAction {
        surface_id: card.id.clone(),
        contract: card.contract.clone(),
        action_id: action_id.into(),
        expect_rev: 0,
        epoch: card.epoch,
        verb: agent_engine::ui::Verb::Answer,
        values: Some(values),
    }
}

/// `t`'s next `surface_result` for `action_id`.
/// The result for `action_id`, and everything that came with it (the ack's
/// upsert arrives just before it).
async fn result_seen(t: &mut LocalTransport, action_id: &str) -> (SurfaceResult, Vec<Envelope>) {
    let seen = until(t, |e| matches!(e, SessionEventWire::SurfaceResult(r) if r.action_id == action_id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::SurfaceResult(r) => (r.clone(), seen),
        _ => unreachable!(),
    }
}

async fn result_for(t: &mut LocalTransport, action_id: &str) -> SurfaceResult {
    let seen = until(t, |e| matches!(e, SessionEventWire::SurfaceResult(r) if r.action_id == action_id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::SurfaceResult(r) => r.clone(),
        _ => unreachable!(),
    }
}

/// A session with one open card, raised by a turn that has ended.
async fn with_card(bodies: &'static [&'static str]) -> CardSession {
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");
    (handle, a, card, hits)
}

/// A request counter.
type Hits = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// The model has had `n` requests: a turn's request lands just after its
/// `TurnStarted`, so a test that cancels it waits for this first.
async fn reached(hits: &Hits, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while hits.load(std::sync::atomic::Ordering::SeqCst) < n {
        assert!(tokio::time::Instant::now() < deadline, "the model never got request {n}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
/// A session with one open card: its handle, a capable client, the card,
/// the model's request count.
type CardSession = (agent_engine::session::SessionHandle, LocalTransport, SurfaceState, Hits);

const A1: &str = "0123456789abcdef0123456789abcdef";
const A2: &str = "fedcba9876543210fedcba9876543210";

/// T2 + T7 (decision 1): an answer while idle is acknowledged once durable
/// (`accepted`, `answer_rev` 1), the card becomes a receipt, the marked
/// `<ui_answer>` enters history, and one turn starts. A retry of the same
/// action is a `duplicate` with the logged rev and no effect (Appendix B
/// #3, v3.4 #1); the same id with other values is `invalid`; a new action
/// on the answered card is `resolved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_idle_answer_is_acked_delivered_once_and_starts_one_turn() {
    let _h = Home::new();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let (r, mut seen) = result_seen(&mut a, A1).await;
    assert_eq!((r.outcome, r.answer_rev, r.client), (SurfaceOutcome::Accepted, Some(1), a.client_id()));
    seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await);
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "one turn: {seen:#?}");
    let receipt = cards(&seen).into_iter().find(|c| c.phase == Phase::Answered).expect("an upsert");
    assert!(receipt.rev > card.rev && receipt.offers.is_empty());
    assert_eq!(receipt.actions, vec![A1.to_string()]);
    let ans = receipt.answer.as_ref().unwrap();
    assert_eq!((ans.rev, ans.wake), (1, true));
    let history = last_conversation(&seen).api_messages;
    let delivered: Vec<_> = history
        .iter()
        .filter(|m| m["content"].as_str().is_some_and(|t| t.starts_with(&format!("<ui_answer card=\"{}\"", card.id))))
        .collect();
    assert_eq!(delivered.len(), 1, "in context once");
    assert_eq!(delivered[0][agent_engine::ui::MARK_KEY]["answer"], ans.answer_id.as_str(), "marked");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);

    // The same action again: its original outcome, no effect.
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.answer_rev), (SurfaceOutcome::Duplicate, Some(1)));
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Leave it"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason, r.retry), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Invalid), Some(false)));
    // A settle commit may still be in flight: `busy` is retried, as the
    // outbox does (§3.8).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let r = loop {
        a.send_from_self(answer(&card, A2, serde_json::json!({"choice": "Leave it"}))).await.unwrap();
        let r = result_for(&mut a, A2).await;
        if r.reason != Some(SurfaceRefusal::Busy) || tokio::time::Instant::now() > deadline {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Resolved)));
    let late = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(!late.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3, "no second turn");
    let _ = handle;
    end(&mut a).await;
}

/// A skip is terminal and never starts a turn (T3): its message enters
/// history, and the session stays idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_skip_enters_history_and_starts_nothing() {
    let _h = Home::new();
    let (_handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    a.send_from_self(SessionCommand::SurfaceAction {
        surface_id: card.id.clone(),
        contract: card.contract.clone(),
        action_id: A1.into(),
        expect_rev: 0,
        epoch: card.epoch,
        verb: agent_engine::ui::Verb::Skip,
        values: None,
    })
    .await
    .unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    let seen = drain_for(&mut a, Duration::from_millis(500)).await;
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert!(cards(&seen).iter().any(|c| c.phase == Phase::Skipped && c.answer.as_ref().is_some_and(|x| !x.wake)));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// #445: card traffic reaches an in-process client only as it would over
/// the socket: a catalog-0 local client gets no `surface` and no
/// `surface_result`, the capable one gets both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn card_traffic_reaches_only_a_local_client_with_the_catalog() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let (mut old, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    // The old client saw both turns, and none of the card traffic.
    let mut idles = 0;
    let mut saw = Vec::new();
    while idles < 2 {
        let env = next(&mut old).await;
        idles += usize::from(matches!(env.event, SessionEventWire::Idle));
        saw.push(env);
    }
    assert!(
        !saw.iter().any(|e| matches!(e.event, SessionEventWire::Surface(_) | SessionEventWire::SurfaceResult(_))),
        "{saw:#?}"
    );
    end(&mut a).await;
}

/// v3.4 #6: to a capable client every refusal is a `surface_result` with
/// its action id: an observer's is `forbidden` (no retry), and nothing
/// changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_capable_observers_answer_is_forbidden() {
    let _h = Home::new();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    let (mut o, _) = LocalTransport::attach_with(handle.clone(), capable(ClientKind::Attach), AttachMode::Observe)
        .await
        .unwrap();
    o.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut o, A1).await;
    assert_eq!((r.outcome, r.reason, r.retry), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Forbidden), Some(false)));
    let seen = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(cards(&seen).is_empty() && !seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// C4a exit gate, `retry_after_cancel_needs_new_consent` (§3.9): an answer
/// is refused `rate_limited` while the turn that asked still runs; the user
/// Cancels; the outbox's retry of the same action gets `stopped` and starts
/// nothing. A fresh commit at the new epoch ("Answer & resume") is
/// accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn retry_after_cancel_needs_new_consent() {
    let _h = Home::new();
    let (url, hits) = stub_seq_endless_last(&[SSE_UI_CREATE, SSE_PREFIX]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Surface(_))).await;
    let card = cards(&seen).pop().unwrap();
    // Spend the bucket on bogus ids, then the real action is rate limited.
    for i in 0..10 {
        a.send_from_self(answer(&card, &format!("{i:032x}"), serde_json::json!("bogus"))).await.unwrap();
    }
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.reason, r.retry), (Some(SurfaceRefusal::RateLimited), Some(true)));
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await; // the bucket refills
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Stopped)));
    let quiet = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    // The card says so: resume, at the new epoch.
    let now = handle_card(&handle).await;
    assert!(now.epoch > card.epoch && now.resume && now.phase == Phase::Open, "{now:?}");
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    let mut fresh = answer(&now, A2, serde_json::json!({"choice": "Refactor"}));
    if let SessionCommand::SurfaceAction { epoch, .. } = &mut fresh {
        *epoch = now.epoch;
    }
    a.send_from_self(fresh).await.unwrap();
    assert_eq!(result_for(&mut a, A2).await.outcome, SurfaceOutcome::Accepted);
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    // The turn's request reaches the model just after `TurnStarted`.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while hits.load(std::sync::atomic::Ordering::SeqCst) <= before {
        assert!(tokio::time::Instant::now() < deadline, "Answer & resume starts the turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
}

/// The card as a fresh capable attach sees it.
async fn handle_card(handle: &agent_engine::session::SessionHandle) -> SurfaceState {
    let (b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Attach)).await.unwrap();
    b.send(SessionCommand::Detach { client: b.client_id() }).await.unwrap();
    snap.surfaces.into_iter().next().expect("the card")
}

// ── consent and lifecycle (the C4a exit gate, §3.9–§3.11) ───────────────────

use agent_engine::core::session_surfaces::ModelFs;

/// A session whose card store is `fs`, with one open card from a turn that
/// has ended.
async fn with_card_on(
    fs: std::sync::Arc<ModelFs>,
    bodies: &'static [&'static str],
) -> CardSession {
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, mut task) = host.create_session_unspawned(cards_cfg()).await.unwrap();
    task.ui_fs_for_harness(fs);
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");
    (handle, a, card, hits)
}

fn ui_answers(history: &[agent_engine::SharedMessage], card: &str) -> usize {
    history
        .iter()
        .filter(|m| m["content"].as_str().is_some_and(|t| t.starts_with(&format!("<ui_answer card=\"{card}\""))))
        .count()
}

/// Exit gate, consent at dispatch (R3-H2): the answer's commit is still
/// unknown when the user Cancels (`pending`); the Cancel finds its wake and
/// raises the epoch; the commit then lands. The answer enters context
/// once, with no wake: zero new turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pending_answer_commit_then_cancel_then_durable() {
    let _h = Home::new();
    let fs = ModelFs::new();
    let (_handle, mut a, card, hits) = with_card_on(fs.clone(), &[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    fs.stall();
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Pending, "unknown at the budget");
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    fs.release();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.answer_rev), (SurfaceOutcome::Accepted, Some(1)));
    let after = drain_for(&mut a, Duration::from_millis(600)).await;
    assert!(!after.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "zero new turns: {after:#?}");
    assert_eq!(ui_answers(&last_conversation(&after).api_messages, &card.id), 1, "in context once");
    let shown = cards(&after);
    assert!(shown.iter().any(|c| c.epoch > card.epoch), "the stop's upsert: {shown:?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// Exit gate, consent (§3.9), C4a's half (`revise` is C4c): a system stop
/// (a host stop here; the cost cap takes the same path) while a card is
/// open expires it and leaves it nothing to offer; an answer sent anyway is
/// `resolved`; no message, no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn system_stop_freezes_receipts() {
    let _h = Home::new();
    let (url, hits) = stub_seq_endless_last(&[SSE_UI_CREATE, SSE_HI, SSE_PREFIX]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().unwrap();
    a.send(submit("meanwhile")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    reached(&hits, 3).await;
    handle
        .send(SessionCommand::Checkpoint { reason: agent_engine::session::CheckpointReason::HostRequest })
        .await
        .unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Surface(s) if s.phase == Phase::Expired)).await;
    let expired = cards(&seen).pop().unwrap();
    assert!(expired.offers.is_empty() && expired.epoch > card.epoch, "{expired:?}");
    // `stopped` marks a card a stop left open (it resumes); an expired one
    // can't (the C2 model, `worst_sidecar_of_real_types_fits`).
    assert_eq!(expired.origin.stopped, None);
    let mut late = answer(&card, A1, serde_json::json!({"choice": "Refactor"}));
    if let SessionCommand::SurfaceAction { epoch, .. } = &mut late {
        *epoch = expired.epoch;
    }
    a.send_from_self(late).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Resolved)));
    let quiet = drain_for(&mut a, Duration::from_millis(400)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut a).await;
}

/// Exit gate, consent (§3.9 case 2), the daemon's half: desk A's answer
/// starts the turn and A loses its connection; desk B Cancels. The epoch
/// rose. A reconnects and its outbox re-sends the same action: a
/// `duplicate`, nothing sent, no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn remote_cancel_while_disconnected() {
    let _h = Home::new();
    let (url, hits) = stub_seq_endless_last(&[SSE_UI_CREATE, SSE_HI, SSE_PREFIX]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(shared_cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let (mut b, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Attach)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().unwrap();
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    until(&mut b, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    reached(&hits, 3).await;
    a.send(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    b.send_from_self(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    // The stop is durable before its upsert shows it (the write rule); an
    // attach before that would still see the old epoch.
    if !cards(&seen).iter().any(|c| c.epoch > card.epoch) {
        until(&mut b, |e| matches!(e, SessionEventWire::Surface(s) if s.epoch > card.epoch)).await;
    }
    let (mut a2, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let now = snap.surfaces.iter().find(|c| c.id == card.id).cloned().expect("retained");
    assert!(now.epoch > card.epoch, "{now:?}");
    a2.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a2, A1).await;
    assert_eq!((r.outcome, r.answer_rev), (SurfaceOutcome::Duplicate, Some(1)));
    let quiet = drain_for(&mut a2, Duration::from_millis(400)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut a2).await;
}

fn shared_cards_cfg() -> SessionConfig {
    SessionConfig { client_input: Some(agent_engine::session::ClientInput::Shared), ..cards_cfg() }
}

/// The session's history files at one instant (not its `.ui`).
struct History(Vec<(std::path::PathBuf, Vec<u8>)>);

impl History {
    fn capture(id: &str) -> Self {
        let dir = agent_engine::core::session_lock::sessions_dir();
        History(
            ["json", "journal", "turn"]
                .iter()
                .map(|ext| dir.join(format!("{id}.{ext}")))
                .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
                .collect(),
        )
    }
    fn restore(&self, id: &str) {
        let dir = agent_engine::core::session_lock::sessions_dir();
        for ext in ["json", "journal", "turn"] {
            let _ = std::fs::remove_file(dir.join(format!("{id}.{ext}")));
        }
        for (path, bytes) in &self.0 {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

/// Exit gate, consent (§3.9 case 3): an answer is accepted while a turn
/// runs, the turn is interrupted (T12 puts it after the marker), and the
/// process dies before that history is saved. Recovery puts the answer into
/// context once and starts no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn interrupt_then_crash_keeps_no_wake() {
    let _h = Home::new();
    let (url, hits) = stub_seq_endless_last(&[SSE_UI_CREATE, SSE_HI, SSE_PREFIX]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().unwrap();
    let id = handle.journal_id();
    let before = History::capture(&id);
    a.send(submit("meanwhile")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    // The crash: the session goes, and its history is as it was before the
    // answer (the save never happened); the sidecar keeps what it wrote.
    end(&mut a).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.is_alive() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    before.restore(&id);

    let handle = host
        .create_session(SessionConfig { continue_session: Some(Some(id.clone())), ..cards_cfg() })
        .await
        .unwrap();
    let (mut b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    assert_eq!(ui_answers(&snap.conversation.api_messages, &card.id), 1, "in context once");
    let quiet = drain_for(&mut b, Duration::from_millis(600)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "no wake");
    let restored = snap.surfaces.iter().find(|c| c.id == card.id).expect("retained");
    assert!(restored.answer.as_ref().is_some_and(|x| !x.wake), "{restored:?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut b).await;
}

/// The reload barrier (§3.10): with the card store failing, the reload
/// checkpoint replies `{ok:false, reason:"ui_store"}`; `ReloadAborted` then
/// lifts the fence and the session goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_reload_barrier_is_the_checkpoints_reply() {
    let _h = Home::new();
    let fs = ModelFs::new();
    let (handle, mut a, _card, _) = with_card_on(fs.clone(), &[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    // Every directory sync fails: the store fails closed (§3.4).
    fs.fail_sync(1000);
    handle
        .send(SessionCommand::Checkpoint { reason: agent_engine::session::CheckpointReason::Reload })
        .await
        .unwrap();
    let id = agent_engine::session::wire::CHECKPOINT_QUERY_ID;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: q, .. } if *q == id)).await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
    assert_eq!((value["ok"].as_bool(), value["reason"].as_str()), (Some(false), Some("ui_store")), "{value}");
    assert!(handle.ui_barrier(), "the session holds card state");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    a.send(submit("still here")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
}

// ── availability (R2-M5, §1.5, Appendix B #31) ──────────────────────────────

async fn ui_command(t: &mut LocalTransport, id: u64, arg: &str) -> serde_json::Value {
    t.send(SessionCommand::EngineCommand { id, name: "ui".into(), arg: arg.into() }).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: q, .. } if *q == id)).await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
    value.clone()
}

/// Global off (the default), the create override on: a capable desk's
/// answer is accepted and starts the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn global_off_create_override_on_a_capable_desk_answers() {
    let _h = Home::new();
    let (_handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Leave it"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut a).await;
}

/// `/ui on` on an existing connection: the next turn's model can raise a
/// card, and the same client answers it, with no reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ui_on_works_on_an_existing_connection() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(SessionConfig { persist: true, ..cfg() }).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    assert_eq!(ui_command(&mut a, 7, "on").await["on"], true);
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card after /ui on");
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
}

/// `/ui off` with an open live card (T23): withdrawn and durable before the
/// reply; an answer sent anyway is `resolved`; the tool is withheld.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ui_off_withdraws_open_live_cards() {
    let _h = Home::new();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    let v = ui_command(&mut a, 8, "off").await;
    assert_eq!(v["on"], false, "{v}");
    assert_eq!(v["text"], "cards are off: open cards were withdrawn");
    let journal = handle.meta_live().journal_id;
    let path = agent_engine::core::session_lock::sessions_dir().join(format!("{journal}.ui"));
    let sc = agent_engine::core::session_surfaces::decode_sidecar(&std::fs::read(&path).unwrap()).unwrap();
    let on_disk = sc.card(&card.id).expect("retained");
    assert_eq!(on_disk.phase, Phase::Withdrawn, "durable before the reply");
    assert!(on_disk.offers.is_empty());
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Resolved)));
    assert_eq!(handle.meta_live().surfaces_open, 0);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

// ── answers and compaction (T8, §3.6 capsule, R3-H2) ────────────────────────

/// Streaming turns are served `bodies` in order (the last repeats); the
/// non-streaming compaction call gets a JSON summary after `delay`.
/// Returns (url, turn requests, compaction requests).
async fn stub_cards(
    bodies: &'static [&'static str],
    delay: Duration,
) -> (String, Hits, Hits) {
    use axum::response::IntoResponse;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let turns = Arc::new(AtomicUsize::new(0));
    let compactions = Arc::new(AtomicUsize::new(0));
    let (t, c) = (Arc::clone(&turns), Arc::clone(&compactions));
    let app = axum::Router::new().fallback(move |body: String| {
        let (t, c) = (Arc::clone(&t), Arc::clone(&c));
        async move {
            if body.contains("\"stream\":true") {
                let i = t.fetch_add(1, Ordering::SeqCst).min(bodies.len() - 1);
                return (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], bodies[i].to_string())
                    .into_response();
            }
            c.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "SUMMARY: the user was asked a question."}],
                "usage": {"input_tokens": 50, "output_tokens": 10}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), turns, compactions)
}

/// A session with one open card and a compaction now running (5 s).
async fn card_then_compacting(
) -> CardSession {
    let (url, turns, _) = stub_cards(&[SSE_UI_CREATE, SSE_HI, SSE_HI], Duration::from_secs(5)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");
    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::CompactionStarted { .. })).await;
    (handle, a, card, turns)
}

/// Exit gate, consent at dispatch (R3-H2): an answer accepted while a
/// compaction runs is queued (T8); the user Cancels, which aborts the
/// compaction and finds the wake. The drain puts the answer into context
/// once, with no wake: zero new turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn queued_answer_then_cancel_then_drain() {
    let _h = Home::new();
    let (_handle, mut a, card, turns) = card_then_compacting().await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::CompactionCancelled)));
    let mut seen = seen;
    seen.extend(drain_for(&mut a, Duration::from_millis(600)).await);
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "zero new turns: {seen:#?}");
    assert_eq!(ui_answers(&last_conversation(&seen).api_messages, &card.id), 1, "in context once");
    assert!(cards(&seen).iter().any(|c| c.epoch > card.epoch), "the Cancel raised the epoch");
    assert_eq!(turns.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// Exit gate, consent at dispatch (R3-H2): the answer's start is deferred
/// behind a running compaction (T8), and a system stop (a host stop)
/// comes first. The answer enters context once, with no wake: zero new
/// turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn deferred_start_then_system_stop() {
    let _h = Home::new();
    let (handle, mut a, card, turns) = card_then_compacting().await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    handle
        .send(SessionCommand::Checkpoint { reason: agent_engine::session::CheckpointReason::HostRequest })
        .await
        .unwrap();
    let id = agent_engine::session::wire::CHECKPOINT_QUERY_ID;
    let mut seen = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: q, .. } if *q == id)).await;
    seen.extend(drain_for(&mut a, Duration::from_millis(600)).await);
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "zero new turns: {seen:#?}");
    assert_eq!(ui_answers(&last_conversation(&seen).api_messages, &card.id), 1, "in context once");
    let stopped = cards(&seen).into_iter().rfind(|c| c.id == card.id).expect("the stop's upsert");
    assert!(stopped.epoch > card.epoch && stopped.offers.is_empty(), "{stopped:?}");
    assert!(stopped.answer.as_ref().is_some_and(|x| x.rev == 1));
    assert_eq!(turns.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// `SSE_UI_CREATE` with another `ui` input.
fn ui_create_sse(input: serde_json::Value) -> &'static str {
    let old = r#""partial_json":"{\"question\":\"Which approach?\",\"options\":[\"Refactor\",\"Leave it\"],\"default\":\"Refactor\"}""#;
    assert!(SSE_UI_CREATE.contains(old));
    let new = format!("\"partial_json\":{}", serde_json::to_string(&input.to_string()).unwrap());
    Box::leak(SSE_UI_CREATE.replace(old, &new).into_boxed_str())
}

/// Exit gate, late-answer capsule (R3-M1): the card's user message and its
/// `ui` call are really gone from history (an in-place compaction). The
/// answer arrives late with the capsule built at acceptance from the
/// sidecar's record: at most 1 KiB encoded, naming what it omitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn late_answer_after_compaction_capsule() {
    let _h = Home::new();
    let long = |n: usize, w: &str| vec![w; n].join(" ");
    let fields: Vec<serde_json::Value> = (0..5)
        .map(|i| serde_json::json!({"id": format!("f{i}_{}", "a".repeat(21)), "label": long(6, "labelword"), "optional": true}))
        .collect();
    let doc = serde_json::json!({
        "question": "Which approach?",
        "detail": long(120, "detailword"),
        "options": [{"label": "Refactor", "hint": long(11, "hintwords1")}, "Leave it"],
        "fields": fields,
    });
    let bodies: &'static [&'static str] = Box::leak(vec![ui_create_sse(doc), SSE_HI, SSE_HI].into_boxed_slice());
    let (url, turns, compactions) = stub_cards(bodies, Duration::ZERO).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let task = format!("please decide this for me: {}", long(30, "taskword"));
    a.send(submit(&task)).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::CompactionApplied { .. })).await;
    assert_eq!(compactions.load(std::sync::atomic::Ordering::SeqCst), 1);
    let mut seen = seen;
    seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::Conversation(_))).await);
    let history = last_conversation(&seen).api_messages;
    let flat = serde_json::to_string(&history).unwrap();
    assert!(!flat.contains("toolu_ui1") && !flat.contains("taskword"), "really gone: {flat}");

    // A card with fields is answered with every field (§3.3 #11).
    let blank: serde_json::Map<String, serde_json::Value> =
        card.doc.fields.iter().map(|f| (f.id.clone(), serde_json::json!(""))).collect();
    let values = serde_json::json!({"choice": "Refactor", "fields": blank});
    a.send_from_self(answer(&card, A1, values)).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!(r.outcome, SurfaceOutcome::Accepted, "{r:?}");
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let history = last_conversation(&seen).api_messages;
    let msg = history
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|t| t.starts_with(&format!("<ui_answer card=\"{}\"", card.id)))
        .expect("the late answer");
    assert!(msg.contains(" late=\"true\""), "{msg}");
    let from = msg.find("<asked ").expect("a capsule");
    let to = msg.find("</asked>").expect("a capsule") + "</asked>".len();
    let capsule = &msg[from..to];
    assert!(capsule.len() <= 1024, "{} bytes", capsule.len());
    assert!(capsule.contains("during=\"please decide this for me"), "{capsule}");
    assert!(capsule.contains(" omitted=\""), "names its omissions: {capsule}");
    assert_eq!(turns.load(std::sync::atomic::Ordering::SeqCst), 3, "the answer started one turn");
    end(&mut a).await;
}
