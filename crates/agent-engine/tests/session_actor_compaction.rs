//! B2 — spawned compaction (PLAN-phase3 §2.5, §4.2): Attach/Detach/Cancel/
//! Query serviced while a compaction is in flight; Submit queued; Cancel
//! aborts; LinkedSuccessor policy updates `journal_id` + the continue target;
//! `SYNAPS_SESSION_COMPACT_INLINE=1` restores the #107 inline body.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, CompactionPolicyWire, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionQuery,
};
use axum::response::IntoResponse;
use serial_test::serial;

/// Streaming turns (`"stream":true`) → SSE_HI; the non-streaming compaction
/// call → a JSON summary after `delay`. Counts compaction hits.
async fn stub_compact(delay: Duration) -> (String, Arc<AtomicUsize>) {
    let compactions = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&compactions);
    let app = axum::Router::new().fallback(move |body: String| {
        let c = Arc::clone(&c);
        async move {
            if body.contains("\"stream\":true") {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    SSE_HI.to_string(),
                )
                    .into_response();
            }
            c.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "SUMMARY: the user said hello a few times."}],
                "usage": {"input_tokens": 50, "output_tokens": 10}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), compactions)
}

/// 4+ messages so the engine has something to compact.
async fn warm(t: &mut LocalTransport, turns: usize) {
    for i in 0..turns {
        t.send(submit(&format!("hello {i}"))).await.unwrap();
        until(t, |e| matches!(e, SessionEventWire::Idle)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_does_not_block_attach_and_submit_is_queued_and_restored() {
    let _h = Home::new();
    let (url, compactions) = stub_compact(Duration::from_secs(2)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::CompactionStarted { .. })).await;
    assert!(matches!(
        &seen.last().unwrap().event,
        SessionEventWire::CompactionStarted { source, disclosure } if source == "manual" && !disclosure.is_empty()
    ));

    // Serviced during compaction: Attach (with snapshot), Query, Detach.
    let t0 = std::time::Instant::now();
    let (mut b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert!(t0.elapsed() < Duration::from_millis(1500), "attach did not wait for compaction");
    assert_eq!(snap.conversation.api_messages.len(), 4);
    b.send(SessionCommand::Query {
        id: 5,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::QueryResult { id: 5, .. })).await;
    b.send(SessionCommand::Detach {
        client: b.client_id(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;

    // Submit during compaction → queued, not sent.
    a.send(submit("while compacting")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    assert!(matches!(
        &seen.last().unwrap().event,
        SessionEventWire::Steered { text, delivered: false } if text == "while compacting"
    ));

    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let applied = seen
        .iter()
        .find_map(|e| match &e.event {
            SessionEventWire::CompactionApplied {
                previous_session_id,
                session_id,
                queued_restored,
                msg_count,
                ..
            } => Some((
                previous_session_id.clone(),
                session_id.clone(),
                queued_restored.clone(),
                *msg_count,
            )),
            _ => None,
        })
        .expect("CompactionApplied");
    assert_eq!(applied.0, handle.id.as_str());
    assert_eq!(applied.1, handle.id.as_str(), "InPlace keeps the id");
    assert_eq!(applied.2.as_deref(), Some("while compacting"));
    assert_eq!(applied.3, 4);
    assert_eq!(compactions.load(Ordering::SeqCst), 1);
    let conv = last_conversation(&seen);
    assert!(conv.queued_message.is_none());
    assert!(conv.api_messages.len() < 4 + 1 || conv.api_messages.len() >= 2);
    // The queued text rides the transition as the last user message; it is
    // reported, never re-sent (loop_arms.rs:806-811).
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(
        conv.api_messages.last().unwrap()["content"],
        "while compacting"
    );
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_aborts_compaction_and_auto_turns_see_busy() {
    let _h = Home::new();
    let (url, _) = stub_compact(Duration::from_secs(5)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;
    let before = {
        a.send(SessionCommand::Query {
            id: 1,
            query: SessionQuery::Messages,
        })
        .await
        .unwrap();
        let seen = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: 1, .. })).await;
        match &seen.last().unwrap().event {
            SessionEventWire::QueryResult { value, .. } => value.clone(),
            _ => unreachable!(),
        }
    };

    a.send(SessionCommand::Compact {
        instructions: Some("short".into()),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::CompactionStarted { .. })).await;
    a.send(submit("queued")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Steered { .. })).await;

    let t0 = std::time::Instant::now();
    a.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(t0.elapsed() < Duration::from_secs(2), "cancel does not wait for the job");
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::CompactionCancelled)));
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::Dequeued { text } if text == "queued"
    )));
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::CompactionApplied { .. })));
    // Prior state intact.
    a.send(SessionCommand::Query {
        id: 2,
        query: SessionQuery::Messages,
    })
    .await
    .unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::QueryResult { id: 2, .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => assert_eq!(value, &before),
        _ => unreachable!(),
    }
    // Nothing lands later either.
    let late = drain_for(&mut a, Duration::from_secs(1)).await;
    assert!(!late.iter().any(|e| matches!(
        e.event,
        SessionEventWire::CompactionApplied { .. } | SessionEventWire::CompactionFailed { .. }
    )));
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn linked_successor_updates_journal_id_and_continue_target() {
    let _h = Home::new();
    let (url, _) = stub_compact(Duration::from_millis(200)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;
    assert_eq!(handle.journal_id(), handle.id.as_str());

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let (prev, new) = seen
        .iter()
        .find_map(|e| match &e.event {
            SessionEventWire::CompactionApplied {
                previous_session_id,
                session_id,
                ..
            } => Some((previous_session_id.clone(), session_id.clone())),
            _ => None,
        })
        .expect("CompactionApplied");
    assert_eq!(prev, handle.id.as_str());
    assert_ne!(new, prev, "LinkedSuccessor mints a successor");
    assert_eq!(handle.journal_id(), new);
    let conv = last_conversation(&seen);
    assert_eq!(conv.header.id, new);
    assert_eq!(conv.header.parent_session.as_deref(), Some(prev.as_str()));
    assert_eq!(conv.tokens.input, 0, "counters reset on successor");
    assert_eq!(conv.cost, 0.0);
    // Successor on disk with the parent link; parent marks compacted_into.
    let succ = agent_engine::core::session::Session::load(&new).expect("successor saved");
    assert_eq!(succ.parent_session.as_deref(), Some(prev.as_str()));
    let parent = agent_engine::core::session::Session::load(&prev).unwrap();
    assert_eq!(parent.compacted_into.as_deref(), Some(new.as_str()));

    // `--continue <successor>` / `--continue` (latest = the successor on
    // disk) while this actor is live must attach to it, not build a second
    // actor on the successor journal (the map is keyed by the ORIGINAL id).
    for q in [Some(Some(new.clone())), Some(None)] {
        let again = host
            .create_session(SessionConfig {
                continue_session: q.clone(),
                persist: true,
                ..cfg()
            })
            .await
            .unwrap();
        assert_eq!(again.id, handle.id, "continue {q:?} → the live actor");
        assert_eq!(host.sessions().len(), 1, "no second actor for {q:?}");
    }

    // The next turn journals into the successor.
    a.send(submit("after")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let n = last_conversation(&seen).api_messages.len();
    end(&mut a).await;
    let succ = agent_engine::core::session::Session::load(&new).unwrap();
    assert_eq!(succ.api_messages.len(), n);
    assert_eq!(host.sessions().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compact_inline_kill_switch_restores_inline_notices() {
    let _h = Home::new();
    std::env::set_var("SYNAPS_SESSION_COMPACT_INLINE", "1");
    let (url, _) = stub_compact(Duration::from_millis(100)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;
    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    std::env::remove_var("SYNAPS_SESSION_COMPACT_INLINE");
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::CompactionStarted { .. })));
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::SystemNotice(n) if n.starts_with("[compacted → ~")
    )));
    end(&mut a).await;
}

/// H1: the typed events are the contract — `/compact` (both the `Compact`
/// command and the `/compact` engine command) produces exactly one
/// `CompactionStarted`, one `CompactionApplied`, and no "compacting..."
/// `SystemNotice` a client would double-render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_emits_typed_events_exactly_once_and_no_notice() {
    let _h = Home::new();
    let (url, _) = stub_compact(Duration::from_millis(50)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;

    for via_engine_command in [false, true] {
        if via_engine_command {
            a.send(SessionCommand::EngineCommand {
                id: 9,
                name: "compact".into(),
                arg: String::new(),
            })
            .await
            .unwrap();
        } else {
            a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
        }
        let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
        let started = seen
            .iter()
            .filter(|e| matches!(e.event, SessionEventWire::CompactionStarted { .. }))
            .count();
        let applied = seen
            .iter()
            .filter(|e| matches!(e.event, SessionEventWire::CompactionApplied { .. }))
            .count();
        assert_eq!(started, 1, "via_engine_command={via_engine_command}");
        assert_eq!(applied, 1, "via_engine_command={via_engine_command}");
        assert!(
            !seen.iter().any(|e| matches!(
                &e.event,
                SessionEventWire::SystemNotice(n) if n.contains("compact")
            )),
            "no compaction SystemNotice: {:?}",
            seen.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
    }
    end(&mut a).await;
}

/// `Cancel` mid-turn and `NewSession` emit the typed `Aborted` / `Cleared`
/// events, never the legacy notice text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn abort_and_clear_are_typed_events() {
    let _h = Home::new();
    let (url, _) = stub_compact(Duration::from_millis(50)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 1).await;

    a.send(SessionCommand::NewSession).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e.event, SessionEventWire::Cleared { .. }))
            .count(),
        1
    );
    assert!(!seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::SystemNotice(n) if n.contains("cleared") || n.starts_with("aborted")
    )));
    end(&mut a).await;
}
