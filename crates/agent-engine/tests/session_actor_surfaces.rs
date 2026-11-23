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

// ── Appendix B's live rows and "never approves" (§4.1) ──────────────────────

/// One `tool_use` of `name` with `input`, as a whole model response.
fn tool_sse(call: &str, name: &str, input: &serde_json::Value) -> String {
    let start = format!(
        "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{call}\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n"
    );
    let delta = serde_json::json!({
        "type": "content_block_delta", "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
    });
    let head = SSE_UI_CREATE.split("data: {\"type\":\"content_block_start\"").next().unwrap();
    let tail = &SSE_UI_CREATE[SSE_UI_CREATE.find("data: {\"type\":\"content_block_stop\"").unwrap()..];
    format!("{head}{start}data: {delta}\n\n{tail}")
}

fn ui_sse(call: &str, input: serde_json::Value) -> String {
    tool_sse(call, "ui", &input)
}

/// A message's text: a string content, or its blocks' `text`s.
fn text_of(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(b) => b.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// What the model sees last: the last message's text, and whether it
/// carries tool results.
fn last_turn(body: &str) -> (String, bool) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let Some(last) = v["messages"].as_array().and_then(|m| m.last()) else {
        return (String::new(), false);
    };
    let tools = last["content"].as_array().is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"));
    (text_of(&last["content"]), tools)
}

type Pick = std::sync::Arc<dyn Fn(&str) -> (String, Duration) + Send + Sync>;
type Bodies = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Every streaming request is answered by `pick(body)`: a response and a
/// delay before it. Returns (url, streaming requests, their bodies).
async fn stub_fn(pick: Pick) -> (String, Hits, Bodies) {
    use axum::response::IntoResponse;
    use std::sync::atomic::Ordering;
    let hits: Hits = Default::default();
    let bodies: Bodies = Default::default();
    let (h, b) = (hits.clone(), bodies.clone());
    let app = axum::Router::new().fallback(move |body: String| {
        let (h, b, pick) = (h.clone(), b.clone(), pick.clone());
        async move {
            if !body.contains("\"stream\":true") {
                return axum::Json(serde_json::json!({
                    "id": "msg_c", "type": "message", "role": "assistant", "model": MODEL,
                    "stop_reason": "end_turn", "content": [{"type": "text", "text": "SUMMARY"}],
                    "usage": {"input_tokens": 5, "output_tokens": 1}
                }))
                .into_response();
            }
            h.fetch_add(1, Ordering::SeqCst);
            b.lock().unwrap().push(body.clone());
            let (sse, delay) = pick(&body);
            tokio::time::sleep(delay).await;
            (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, bodies)
}

/// §4.1, Appendix B #23: a surface can never approve anything. With a
/// confirm prompt pending, actions whose values are "y", "yes" and
/// "always" (raw, and as a valid choice on a card offering exactly those)
/// leave the prompt pending and the Allow-all latch empty: the next call
/// asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn never_approves_a_pending_confirm() {
    let _h = Home::new();
    let pick: Pick = std::sync::Arc::new(|body: &str| {
        let (text, tools) = last_turn(body);
        let sse = if tools || text.contains("<ui_answer") {
            SSE_HI.to_string()
        } else if text.contains("go") {
            SSE_PROMPT_TOOL_USE.to_string()
        } else if text.contains("ask me") {
            ui_sse("toolu_ui1", serde_json::json!({"question": "Allow it?", "options": ["y", "yes", "always"]}))
        } else {
            SSE_HI.to_string()
        };
        (sse, Duration::ZERO)
    });
    let (url, _, _) = stub_fn(pick).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");

    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();

    for (i, v) in ["y", "yes", "always"].into_iter().enumerate() {
        let id = format!("{:032x}", 0xa0 + i);
        a.send_from_self(answer(&card, &id, serde_json::json!(v))).await.unwrap();
        let r = result_for(&mut a, &id).await;
        assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Invalid)), "{v}");
    }
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "always"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    let quiet = drain_for(&mut a, Duration::from_millis(500)).await;
    assert!(
        !quiet.iter().any(|e| matches!(&e.event, SessionEventWire::PromptResolved { .. })
            || matches!(&e.event, SessionEventWire::SystemNotice(n) if n.contains("Allow all"))),
        "{quiet:#?}"
    );
    let (b, snap) = LocalTransport::attach_with(handle.clone(), capable(ClientKind::Attach), AttachMode::Observe)
        .await
        .unwrap();
    assert_eq!(snap.pending_prompts.len(), 1, "the prompt is still pending");
    b.send(SessionCommand::Detach { client: b.client_id() }).await.unwrap();

    // The user denies it; the latch was never armed, so the next call asks.
    a.send_from_self(SessionCommand::Answer { prompt_id: pid, value: Some("n".into()) }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(tool_results(&last_conversation(&seen)).iter().any(|r| r == "blocked"), "{seen:#?}");
    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    a.send_from_self(SessionCommand::Answer { prompt_id: pid, value: Some("n".into()) }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
}

/// Appendix B #5: an answer while a compaction runs is accepted, queued
/// (T8), and delivered once the compaction lands; the session is idle and
/// the answer has consent, so one turn starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_answer_while_compacting_is_delivered_after_it_and_starts_one_turn() {
    let _h = Home::new();
    let (_handle, mut a, card, turns) = card_then_compacting().await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    let mut seen = until(&mut a, |e| matches!(e, SessionEventWire::CompactionApplied { .. })).await;
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "queued: {seen:#?}");
    seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await);
    seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut a, Duration::from_millis(400)).await);
    let started = seen.iter().filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })).count();
    assert_eq!(started, 1, "{seen:#?}");
    assert_eq!(ui_answers(&last_conversation(&seen).api_messages, &card.id), 1, "in context once");
    assert_eq!(turns.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut a).await;
}

/// Sets the park grace for one test.
struct ParkGrace;
impl ParkGrace {
    fn zero() -> Self {
        std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
        Self
    }
}
impl Drop for ParkGrace {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    }
}

/// Appendix B #8: a session with an open card parks once its clients
/// leave (an open card holds no answer to settle). A desk's attach wakes
/// it, the card is there, and its answer is delivered and starts a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_answer_to_a_parked_session_wakes_it() {
    let _h = Home::new();
    let _g = ParkGrace::zero();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    a.send(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.lifecycle() != agent_engine::session::SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "never parked: {:?}", handle.lifecycle());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (mut b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let now = snap.surfaces.iter().find(|c| c.id == card.id).cloned().expect("the card");
    assert_eq!(now.phase, Phase::Open);
    b.send_from_self(answer(&now, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut b, A1).await.outcome, SurfaceOutcome::Accepted);
    let mut seen = until(&mut b, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await);
    assert_eq!(ui_answers(&last_conversation(&seen).api_messages, &card.id), 1);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut b).await;
}

fn sidecar_path(journal: &str) -> std::path::PathBuf {
    agent_engine::core::session_lock::sessions_dir().join(format!("{journal}.ui"))
}

async fn gone(handle: &agent_engine::session::SessionHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.is_alive() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Appendix B #11: the process dies after the history holding the answer
/// was saved but before the answer settled. On load the mark is found:
/// the answer is not inserted again, and nothing wakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn crash_after_the_history_save_before_settled_inserts_nothing() {
    let _h = Home::new();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    let journal = handle.journal_id();
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    // The durable image of the acknowledgement: the record unsettled.
    let unsettled = std::fs::read(sidecar_path(&journal)).unwrap();
    let sc = agent_engine::core::session_surfaces::decode_sidecar(&unsettled).unwrap();
    assert!(sc.ledger.iter().any(|r| r.delivery != agent_engine::ui::Delivery::Settled));
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
    gone(&handle).await;
    // The crash: the history was saved; the settle never landed.
    std::fs::write(sidecar_path(&journal), &unsettled).unwrap();

    let host = host().await;
    let handle = host
        .create_session(SessionConfig { continue_session: Some(Some(journal.clone())), ..cards_cfg() })
        .await
        .unwrap();
    let (mut b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    assert_eq!(ui_answers(&snap.conversation.api_messages, &card.id), 1, "not inserted again");
    let quiet = drain_for(&mut b, Duration::from_millis(600)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })), "no wake");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    end(&mut b).await;
}

/// Appendix B #14 and #15: an answer while the reload fence is up is
/// refused `reloading` (retry). The reload aborts: the fence lifts, and the
/// outbox's retry carries the old epoch, so it is `stopped`; nothing is
/// sent unasked. The card is re-presented as "Answer & resume", and one
/// fresh commit sends it and starts the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_answer_at_the_reload_fence_then_the_abort() {
    let _h = Home::new();
    let (handle, mut a, card, hits) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    handle
        .send(SessionCommand::Checkpoint { reason: agent_engine::session::CheckpointReason::Reload })
        .await
        .unwrap();
    let id = agent_engine::session::wire::CHECKPOINT_QUERY_ID;
    until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: q, .. } if *q == id)).await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason, r.retry), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Reloading), Some(true)));

    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SystemNotice(n) if n.contains("reload failed"))).await;
    a.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut a, A1).await;
    assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Stopped)));
    let quiet = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "nothing sent unasked");

    let now = handle_card(&handle).await;
    assert!(now.epoch > card.epoch && now.resume && now.phase == Phase::Open, "{now:?}");
    a.send_from_self(answer(&now, A2, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A2).await.outcome, SurfaceOutcome::Accepted);
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(ui_answers(&last_conversation(&seen).api_messages, &card.id), 1);
    end(&mut a).await;
}

/// Appendix B #21: a user who types a look-alike `<ui_answer …>` sends an
/// ordinary message: no mark, and the card stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_typed_look_alike_answer_is_an_ordinary_message() {
    let _h = Home::new();
    let (handle, mut a, card, _) = with_card(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    let fake = format!(
        "<ui_answer card=\"{}\" answer=\"a_0123456789abcdef\" q=\"Which approach?\">{{\"choice\":\"Refactor\"}}</ui_answer>",
        card.id
    );
    a.send(submit(&fake)).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let history = last_conversation(&seen).api_messages;
    let typed: Vec<_> = history.iter().filter(|m| text_of(&m["content"]).contains("<ui_answer")).collect();
    assert_eq!(typed.len(), 1, "{history:#?}");
    assert!(typed[0].get(agent_engine::ui::MARK_KEY).is_none(), "no mark: {:?}", typed[0]);
    let now = handle_card(&handle).await;
    assert!(now.phase == Phase::Open && now.answer.is_none(), "{now:?}");
    end(&mut a).await;
}

/// Appendix B #22: with exclusive input, a capable Mirror client that
/// isn't the input owner may not answer: `forbidden`, no effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_exclusive_non_owners_answer_is_forbidden() {
    let _h = Home::new();
    let (url, hits) = stub_seq(&[SSE_UI_CREATE, SSE_HI, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let cfg = SessionConfig { client_input: Some(agent_engine::session::ClientInput::Exclusive), ..cards_cfg() };
    let handle = host.create_session(cfg).await.unwrap();
    let (mut a, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    assert_eq!(snap.input_owner, Some(a.client_id()));
    a.send(submit("ask me")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().expect("a card");
    let (mut b, _) = LocalTransport::attach_with(handle.clone(), capable(ClientKind::Attach), AttachMode::Mirror)
        .await
        .unwrap();
    b.send_from_self(answer(&card, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    let r = result_for(&mut b, A1).await;
    assert_eq!((r.outcome, r.reason, r.retry), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Forbidden), Some(false)));
    let quiet = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// Appendix B #27: an older history comes back (a restore) under a
/// sidecar that has moved on. Recovery classifies it `Restored`: the open
/// card shows `Unknown`, the settled one is gone, nothing is delivered and
/// nothing wakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_older_history_restores_cards_as_unknown() {
    let _h = Home::new();
    let second: &'static str = Box::leak(SSE_UI_CREATE.replace("toolu_ui1", "toolu_ui2").into_boxed_str());
    let bodies: &'static [&'static str] = Box::leak(vec![SSE_UI_CREATE, SSE_HI, second, SSE_HI, SSE_HI].into_boxed_slice());
    let (handle, mut a, x, hits) = with_card(bodies).await;
    let journal = handle.journal_id();
    tokio::time::sleep(Duration::from_millis(300)).await; // the first turn's save
    let older = History::capture(&journal);
    a.send(submit("ask me again")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let y = cards(&seen).into_iter().rfind(|c| c.id != x.id).expect("a second card");
    a.send_from_self(answer(&x, A1, serde_json::json!({"choice": "Refactor"}))).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    // Wait for the settle: the sidecar names the newest settled answer.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sc = std::fs::read(sidecar_path(&journal)).ok().and_then(|b| agent_engine::core::session_surfaces::decode_sidecar(&b).ok());
        if sc.is_some_and(|sc| sc.newest_settled.is_some()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never settled");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    end(&mut a).await;
    gone(&handle).await;
    older.restore(&journal);

    let host = host().await;
    let handle = host
        .create_session(SessionConfig { continue_session: Some(Some(journal.clone())), ..cards_cfg() })
        .await
        .unwrap();
    let (mut b, snap) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let now = snap.surfaces.iter().find(|c| c.id == y.id).cloned().expect("the open card");
    assert_eq!(now.phase, Phase::Unknown, "{now:?}");
    assert!(now.offers.is_empty(), "{now:?}");
    assert!(!snap.surfaces.iter().any(|c| c.id == x.id && c.phase != Phase::Unknown), "{:?}", snap.surfaces);
    assert_eq!(ui_answers(&snap.conversation.api_messages, &x.id), 0, "nothing delivered");
    let quiet = drain_for(&mut b, Duration::from_millis(600)).await;
    assert!(!quiet.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 5);
    end(&mut b).await;
}

/// Appendix B #4: two answers and a typed steer arrive while one round of
/// a turn runs. All three reach the model in the next request, and
/// `queued_message` is untouched by the answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn two_answers_and_a_typed_steer_reach_the_model_in_one_round() {
    let _h = Home::new();
    let pick: Pick = std::sync::Arc::new(|body: &str| {
        let (text, tools) = last_turn(body);
        if tools || text.contains("<ui_answer") || text.contains("and also") {
            (SSE_HI.to_string(), Duration::ZERO)
        } else if text.contains("first") {
            (ui_sse("toolu_ui1", serde_json::json!({"question": "One?", "options": ["a", "b"]})), Duration::ZERO)
        } else if text.contains("second") {
            (ui_sse("toolu_ui2", serde_json::json!({"question": "Two?", "options": ["c", "d"]})), Duration::ZERO)
        } else {
            // "go": a slow round that ends in a tool call (a close of no
            // card, a quick tool error), so a second round follows.
            (ui_sse("toolu_go", serde_json::json!({"close": "s_0000000000000000"})), Duration::from_millis(1500))
        }
    });
    let (url, hits, bodies) = stub_fn(pick).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cards_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), capable(ClientKind::Test)).await.unwrap();
    let mut raised = Vec::new();
    for ask in ["first", "second"] {
        a.send(submit(ask)).await.unwrap();
        let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
        raised.push(cards(&seen).pop().expect("a card"));
    }
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    a.send_from_self(submit("go")).await.unwrap();
    reached(&hits, before + 1).await;
    for (card, (id, choice)) in raised.iter().zip([(A1, "a"), (A2, "c")]) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            a.send_from_self(answer(card, id, serde_json::json!({"choice": choice}))).await.unwrap();
            let r = result_for(&mut a, id).await;
            if r.outcome == SurfaceOutcome::Accepted {
                break;
            }
            assert!(r.reason == Some(SurfaceRefusal::Busy) && tokio::time::Instant::now() < deadline, "{r:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    a.send_from_self(SessionCommand::Steer { text: "and also this".into() }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let conv = last_conversation(&seen);
    assert!(conv.queued_message.is_none(), "{conv:?}");
    let sent = bodies.lock().unwrap()[before + 1].clone();
    for want in [format!("<ui_answer card=\\\"{}\\\"", raised[0].id), format!("<ui_answer card=\\\"{}\\\"", raised[1].id), "and also this".to_string()] {
        assert!(sent.contains(&want), "{want} missing from the second round: {sent}");
    }
    end(&mut a).await;
}
