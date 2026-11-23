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
