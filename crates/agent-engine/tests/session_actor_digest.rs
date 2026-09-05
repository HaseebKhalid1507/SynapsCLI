//! Phase 4 §2.3 — `snapshot_for(ClientMeta)`: a `HistoryMode::Digest`
//! attach carries `display_tail` and an EMPTY `api_messages` (with
//! `messages_len` kept), a Full attach is unchanged, and
//! `SessionQuery::DisplayTail` answers the projection on demand.

mod session_actor_common;
use session_actor_common::*;

use agent_engine::session::display::{display_tail, DisplayItem, DisplayTail};
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, HistoryMode, LocalTransport,
    SessionCommand, SessionEventWire, SessionQuery,
};
use serial_test::serial;

fn meta(history: HistoryMode, tail_items: usize) -> ClientMeta {
    let mut m = ClientMeta::new(ClientKind::Tui);
    m.history = history;
    m.tail_items = tail_items;
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn digest_attach_has_tail_not_messages() {
    let _h = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    // Build some history with a Full client.
    let (mut full, snap0) =
        LocalTransport::attach_with(handle.clone(), meta(HistoryMode::Full, 120), AttachMode::Mirror)
            .await
            .unwrap();
    assert!(snap0.display_tail.is_none(), "Full attach: no tail");
    for i in 0..3 {
        full.send_from_self(submit(&format!("hello {i}"))).await.unwrap();
        until(&mut full, |e| matches!(e, SessionEventWire::Idle)).await;
    }
    let history = handle.query_now(SessionQuery::Messages).await;
    let msgs: Vec<agent_engine::SharedMessage> = serde_json::from_value::<Vec<serde_json::Value>>(history)
        .unwrap()
        .into_iter()
        .map(std::sync::Arc::new)
        .collect();
    assert_eq!(msgs.len(), 6);

    // Full re-attach: unchanged shape.
    let (mut full2, snap_full) =
        LocalTransport::attach_with(handle.clone(), meta(HistoryMode::Full, 120), AttachMode::Observe)
            .await
            .unwrap();
    assert_eq!(snap_full.conversation.api_messages.len(), 6);
    assert_eq!(snap_full.conversation.messages_len, 6);
    assert!(snap_full.display_tail.is_none());

    // Digest attach: empty messages, len kept, tail = projection of the same history.
    let (mut digest, snap_digest) =
        LocalTransport::attach_with(handle.clone(), meta(HistoryMode::Digest, 4), AttachMode::Observe)
            .await
            .unwrap();
    assert!(snap_digest.conversation.api_messages.is_empty(), "Digest attach ships no history");
    assert_eq!(snap_digest.conversation.messages_len, 6);
    let tail = snap_digest.display_tail.clone().expect("Digest attach carries display_tail");
    assert_eq!(tail, display_tail(&msgs, 4));
    assert_eq!(tail.items.len(), 4);
    assert_eq!(tail.omitted, 2);
    assert!(matches!(&tail.items[0], DisplayItem::User { text } if text == "hello 1"), "{:?}", tail.items[0]);

    // DisplayTail query answers the same projection at any cap.
    digest
        .send_from_self(SessionCommand::Query { id: 77, query: SessionQuery::DisplayTail { items: 2 } })
        .await
        .unwrap();
    let seen = until(&mut digest, |e| matches!(e, SessionEventWire::QueryResult { id: 77, .. })).await;
    let value = match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    };
    let queried: DisplayTail = serde_json::from_value(value).unwrap();
    assert_eq!(queried, display_tail(&msgs, 2));
    assert_eq!(queried.omitted, 4);

    end(&mut full).await;
    let _ = (&mut full2, &mut digest);
}

trait QueryNow {
    async fn query_now(&self, q: SessionQuery) -> serde_json::Value;
}

impl QueryNow for agent_engine::session::SessionHandle {
    async fn query_now(&self, q: SessionQuery) -> serde_json::Value {
        let (mut t, _) = LocalTransport::attach_with(self.clone(), meta(HistoryMode::Full, 120), AttachMode::Observe)
            .await
            .unwrap();
        t.send_from_self(SessionCommand::Query { id: 99, query: q }).await.unwrap();
        let seen = until(&mut t, |e| matches!(e, SessionEventWire::QueryResult { id: 99, .. })).await;
        match &seen.last().unwrap().event {
            SessionEventWire::QueryResult { value, .. } => value.clone(),
            _ => unreachable!(),
        }
    }
}
