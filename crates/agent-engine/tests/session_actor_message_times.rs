//! Each message keeps its own time (`_synaps_at`) across re-attach, a new
//! turn and a restart (`--continue`): a rebuilt transcript shows when a
//! message happened, not when it was rebuilt.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::runtime::message_time::{message_time, MessageClock};
use agent_engine::session::display::{DisplayItem, DisplayTail};
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, HistoryMode, LocalTransport, SessionConfig,
};
use serial_test::serial;

const T0: i64 = 1_792_907_220; // 2026-10-25T05:47:00Z
const T0_STR: &str = "2026-10-25T05:47:00Z";
const T1_STR: &str = "2026-10-25T10:58:00Z";
const T2_STR: &str = "2026-10-26T09:00:00Z";

fn clock(secs: &Arc<AtomicI64>) -> MessageClock {
    let secs = Arc::clone(secs);
    MessageClock::new(move || {
        chrono::DateTime::from_timestamp(secs.load(Ordering::SeqCst), 0).unwrap()
    })
}

fn set(secs: &AtomicI64, ts: &str) {
    secs.store(
        chrono::DateTime::parse_from_rfc3339(ts).unwrap().timestamp(),
        Ordering::SeqCst,
    );
}

async fn digest_tail(handle: agent_engine::session::SessionHandle) -> DisplayTail {
    let mut meta = ClientMeta::new(ClientKind::Tui);
    meta.history = HistoryMode::Digest;
    meta.tail_items = 120;
    let (_t, snap) = LocalTransport::attach_with(handle, meta, AttachMode::Observe)
        .await
        .unwrap();
    snap.display_tail.expect("Digest attach carries a tail")
}

/// `(user text or "reply", time)` per display item.
fn rows(tail: &DisplayTail) -> Vec<(String, Option<String>)> {
    tail.items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let label = match item {
                DisplayItem::User { text } => text.clone(),
                _ => "reply".into(),
            };
            (label, tail.time(i).map(str::to_owned))
        })
        .collect()
}

async fn turn(t: &mut LocalTransport, text: &str) {
    t.send_from_self(submit(text)).await.unwrap();
    until(t, |e| matches!(e, agent_engine::session::SessionEventWire::Idle)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_message_keeps_its_own_time_across_reattach_new_turns_and_restart() {
    let h = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let secs = Arc::new(AtomicI64::new(T0));
    assert_eq!(chrono::DateTime::from_timestamp(T0, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true), T0_STR);
    let config = SessionConfig {
        persist: true,
        message_clock: Some(clock(&secs)),
        ..cfg()
    };
    let host = host().await;
    let handle = host.create_session(config.clone()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) =
        LocalTransport::attach_with(handle.clone(), ClientMeta::new(ClientKind::Tui), AttachMode::Mirror)
            .await
            .unwrap();
    turn(&mut a, "first").await;

    // Five hours later: a re-attach shows 05:47, not now.
    set(&secs, T1_STR);
    let t0 = Some(T0_STR.to_string());
    assert_eq!(
        rows(&digest_tail(handle.clone()).await),
        vec![("first".into(), t0.clone()), ("reply".into(), t0.clone())]
    );

    // A new turn stamps only its own messages.
    turn(&mut a, "second").await;
    let t1 = Some(T1_STR.to_string());
    let after = vec![
        ("first".into(), t0.clone()),
        ("reply".into(), t0.clone()),
        ("second".into(), t1.clone()),
        ("reply".into(), t1.clone()),
    ];
    assert_eq!(rows(&digest_tail(handle.clone()).await), after);

    // Full-history clients project locally: the key survives the wire.
    let (_full, snap) =
        LocalTransport::attach_with(handle.clone(), ClientMeta::new(ClientKind::Tui), AttachMode::Observe)
            .await
            .unwrap();
    let times: Vec<_> = snap.conversation.api_messages.iter().map(|m| message_time(m)).collect();
    assert_eq!(times, vec![Some(T0_STR), Some(T0_STR), Some(T1_STR), Some(T1_STR)]);
    assert_eq!(
        rows(&agent_engine::session::display::display_tail(&snap.conversation.api_messages, 120)),
        after
    );
    end(&mut a).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.is_alive() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // On disk: times beside the messages, never inside them.
    let snapshot = std::fs::read_to_string(
        h.dir.path().join(".synaps-cli/sessions").join(format!("{id}.json")),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    for m in parsed["api_messages"].as_array().unwrap() {
        assert!(m.get("_synaps_at").is_none(), "inline time persisted: {m}");
    }

    // Next day, after a restart: `--continue` still shows the old times.
    set(&secs, T2_STR);
    let resumed = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.to_string())),
            ..config
        })
        .await
        .unwrap();
    assert_eq!(rows(&digest_tail(resumed.clone()).await), after);
}

/// A session saved before message times existed: its messages show no
/// time after `--continue` and a new turn, never the time of that turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn legacy_messages_stay_untimed_after_continue_and_a_new_turn() {
    let _h = Home::new();
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut legacy = agent_engine::core::session::Session::new(MODEL, "low", None);
    legacy.api_messages = vec![
        Arc::new(serde_json::json!({"role": "user", "content": "old question"})),
        Arc::new(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "old answer"}]})),
    ];
    legacy.save().await.unwrap();

    let secs = Arc::new(AtomicI64::new(0));
    set(&secs, T1_STR);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            continue_session: Some(Some(legacy.id.clone())),
            persist: true,
            message_clock: Some(clock(&secs)),
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, _) =
        LocalTransport::attach_with(handle.clone(), ClientMeta::new(ClientKind::Tui), AttachMode::Mirror)
            .await
            .unwrap();
    assert_eq!(
        rows(&digest_tail(handle.clone()).await),
        vec![("old question".into(), None), ("reply".into(), None)]
    );
    turn(&mut a, "new question").await;
    let t1 = Some(T1_STR.to_string());
    assert_eq!(
        rows(&digest_tail(handle.clone()).await),
        vec![
            ("old question".into(), None),
            ("reply".into(), None),
            ("new question".into(), t1.clone()),
            ("reply".into(), t1),
        ]
    );
    end(&mut a).await;
}
