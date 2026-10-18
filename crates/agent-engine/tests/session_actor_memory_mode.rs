//! A session's explicit `/memory` choice outlives a rebuilt runtime. The
//! operator's confirmed `memory.default_mode` (#225) starts every new
//! session in that mode; `/memory off` revokes it for the session — and a
//! park→unpark or a daemon reload (both build a fresh runtime) must not
//! quietly turn it back on.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME whose
//! config confirms `capture_and_recall`.

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle,
    SessionReloadRecord,
};
use serial_test::serial;

struct Grace;
impl Grace {
    fn zero() -> Self {
        std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
        Self
    }
}
impl Drop for Grace {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    }
}

/// `Home` whose config confirms `memory.default_mode = capture_and_recall`.
fn home_with_memory_default() -> Home {
    let h = Home::new();
    std::fs::write(
        h.dir.path().join(".synaps-cli").join("config"),
        "memory.default_mode = capture_and_recall\nmemory.default_mode_confirmed = true\n",
    )
    .unwrap();
    h
}

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

/// `/memory <arg>` → the rendered reply.
async fn memory(t: &mut LocalTransport, id: u64, arg: &str) -> String {
    t.send(SessionCommand::EngineCommand {
        id,
        name: "memory".into(),
        arg: arg.into(),
    })
    .await
    .unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value["text"].as_str().unwrap_or("").to_string(),
        _ => unreachable!(),
    }
}

async fn parked(handle: &SessionHandle) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if handle.lifecycle() == SessionLifecycle::Parked {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Detach, wait for the park, attach again (which unparks).
async fn park_and_unpark(handle: &SessionHandle, t: LocalTransport) -> LocalTransport {
    t.send(SessionCommand::Detach { client: t.client_id() }).await.unwrap();
    drop(t);
    assert!(parked(handle).await, "grace 0 → parked");
    attach(handle).await
}

/// A session with one turn (so it can park) in the default mode.
async fn session() -> (SessionHandle, LocalTransport) {
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let mut t = attach(&handle).await;
    assert!(
        memory(&mut t, 1, "status").await.contains("mode capture_and_recall"),
        "the confirmed default applies to a new session"
    );
    t.send(submit("hello")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    (handle, t)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn memory_off_survives_park_and_unpark() {
    let _h = home_with_memory_default();
    let _g = Grace::zero();
    let (handle, mut t) = session().await;

    // No explicit choice yet: an unparked runtime starts in the default.
    t = park_and_unpark(&handle, t).await;
    assert!(memory(&mut t, 2, "status").await.contains("mode capture_and_recall"));

    assert!(memory(&mut t, 3, "off").await.contains("mode off"));
    t = park_and_unpark(&handle, t).await;
    let status = memory(&mut t, 4, "status").await;
    assert!(status.contains("mode off"), "/memory off was undone by the unpark: {status}");

    // An explicit mode other than the default is the session's too.
    assert!(memory(&mut t, 5, "recall").await.contains("mode recall_each_prompt"));
    t = park_and_unpark(&handle, t).await;
    let status = memory(&mut t, 6, "status").await;
    assert!(status.contains("mode recall_each_prompt"), "{status}");
    end(&mut t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn memory_off_survives_a_daemon_reload() {
    let _h = home_with_memory_default();
    let (handle, mut t) = session().await;
    assert!(memory(&mut t, 2, "off").await.contains("mode off"));

    // What `daemon reload` records for the session…
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload })
        .await
        .unwrap();
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    let record: SessionReloadRecord = serde_json::from_value(value["record"].clone()).unwrap();
    let journal = handle.journal_id();
    t.send(SessionCommand::End { reason: EndReason::ClientQuit }).await.unwrap();
    while handle.is_alive() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // …and how the next image rebuilds it (`reload::rehydrate`).
    let host = host().await;
    let reborn = host
        .create_session(SessionConfig {
            continue_session: Some(Some(journal)),
            ..record.config
        })
        .await
        .unwrap();
    let mut t = attach(&reborn).await;
    let status = memory(&mut t, 3, "status").await;
    assert!(status.contains("mode off"), "/memory off was undone by the reload: {status}");
    end(&mut t).await;
}
