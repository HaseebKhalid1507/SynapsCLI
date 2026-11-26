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
    ClientKind, ClientMeta, ClientTransport, CompactionPolicyWire, Envelope, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionQuery,
};
use axum::response::IntoResponse;
use futures::StreamExt;
use serial_test::serial;

/// What tells a cache-preserving compaction request (the chat's own next
/// request plus the summary instructions) from a turn.
const CHAT_SUMMARY_REQUEST: &str = "Automated context-compaction request";

/// Summary requests the stub served, by kind.
#[derive(Clone, Default)]
struct Summaries {
    /// The chat's own next request plus the summary instructions.
    chat: Arc<AtomicUsize>,
    /// The flattened transcript (non-streaming).
    transcript: Arc<AtomicUsize>,
}

impl Summaries {
    fn total(&self) -> usize {
        self.chat.load(Ordering::SeqCst) + self.transcript.load(Ordering::SeqCst)
    }
}

/// Streaming turns (`"stream":true`) → SSE_HI. A summary request → a summary
/// after `delay`: as SSE for the chat's own request (streaming, carrying the
/// summary instructions), as JSON for the non-streaming transcript request.
async fn stub_compact(delay: Duration) -> (String, Summaries) {
    let summaries = Summaries::default();
    let s = summaries.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let s = s.clone();
        async move {
            if body.contains("\"stream\":true") {
                let sse = if body.contains(CHAT_SUMMARY_REQUEST) {
                    s.chat.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    SSE_HI.replace("\"text\":\"hi\"", "\"text\":\"SUMMARY: hello, a few times.\"")
                } else {
                    SSE_HI.to_string()
                };
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
                    .into_response();
            }
            s.transcript.fetch_add(1, Ordering::SeqCst);
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
    (format!("http://{addr}"), summaries)
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
    // One summary request, and it is the chat's own next request: the turns
    // just before left the prompt cache warm, and the job's runtime (a clone)
    // knows it. No transcript request.
    assert_eq!(compactions.total(), 1);
    assert_eq!(
        compactions.chat.load(Ordering::SeqCst),
        1,
        "a warm chat is summarized by its own next request"
    );
    let conv = last_conversation(&seen);
    assert!(
        conv.api_messages[0]["content"]
            .as_str()
            .is_some_and(|c| c.contains("SUMMARY: hello, a few times.")),
        "the reply to the chat's own request is the summary"
    );
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
    let before = warm_conv(&mut a, 2).await;
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
    // The summary request was paid for by the session it summarized: the
    // predecessor's saved totals hold it; the successor restarts at zero.
    assert!(before.cost > 0.0);
    assert_close(parent.session_cost, before.cost + cost_of(10, 1, 0, 0));
    assert_eq!(parent.total_input_tokens, before.tokens.input + 10);

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
    // `Ended` precedes the actor task's exit (and its removal from the host
    // map); wait for the exit instead of racing it.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !host.sessions().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
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

// ── compaction spend in the session's totals ────────────────────────────

/// What the stub's chat-request reply reports as usage, and whether the
/// reply is a tool call (which the engine refuses: no summary).
#[derive(Clone, Copy)]
struct ChatReply {
    cache_read: u64,
    cache_write: u64,
    tool_call: bool,
}

const CHAT_INPUT: u64 = 7;
const CHAT_OUTPUT: u64 = 3;
const TRANSCRIPT_INPUT: u64 = 50;
const TRANSCRIPT_OUTPUT: u64 = 10;

fn chat_sse(reply: ChatReply) -> String {
    let usage = |output: u64| {
        serde_json::json!({
            "input_tokens": CHAT_INPUT, "output_tokens": output,
            "cache_creation_input_tokens": reply.cache_write,
            "cache_read_input_tokens": reply.cache_read,
        })
    };
    let mut events = vec![serde_json::json!({"type": "message_start", "message": {
        "id": "msg_sum", "type": "message", "role": "assistant", "content": [],
        "model": MODEL, "stop_reason": null, "stop_sequence": null, "usage": usage(0)}})];
    let stop = if reply.tool_call {
        events.push(serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash"}}));
        events.push(serde_json::json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"true\"}"}}));
        "tool_use"
    } else {
        events.push(serde_json::json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}));
        events.push(serde_json::json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "SUMMARY: from the chat request."}}));
        "end_turn"
    };
    events.push(serde_json::json!({"type": "content_block_stop", "index": 0}));
    events.push(serde_json::json!({"type": "message_delta",
        "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": usage(CHAT_OUTPUT)}));
    events.push(serde_json::json!({"type": "message_stop"}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

/// [`stub_compact`] with a chat-request reply of the test's choosing and
/// known usage on both summary requests.
async fn stub_billed(reply: ChatReply) -> (String, Summaries) {
    let summaries = Summaries::default();
    let s = summaries.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let s = s.clone();
        async move {
            if body.contains("\"stream\":true") {
                let sse = if body.contains(CHAT_SUMMARY_REQUEST) {
                    s.chat.fetch_add(1, Ordering::SeqCst);
                    chat_sse(reply)
                } else {
                    SSE_HI.to_string()
                };
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
                    .into_response();
            }
            s.transcript.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "SUMMARY: from the transcript."}],
                "usage": {"input_tokens": TRANSCRIPT_INPUT, "output_tokens": TRANSCRIPT_OUTPUT}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), summaries)
}

/// [`warm`], returning the conversation as the last turn left it.
async fn warm_conv(
    t: &mut LocalTransport,
    turns: usize,
) -> agent_engine::session::ConversationSnapshot {
    let mut seen = Vec::new();
    for i in 0..turns {
        t.send(submit(&format!("hello {i}"))).await.unwrap();
        seen = until(t, |e| matches!(e, SessionEventWire::Idle)).await;
    }
    last_conversation(&seen)
}

fn cost_of(input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
    agent_engine::pricing::calculate_cost_optional_split(
        MODEL, input, output, cache_read, cache_write, None, None,
    )
}

#[track_caller]
fn assert_close(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-12, "cost {got} != {want}");
}

/// The lines a client was shown about a missed cache.
fn miss_lines(seen: &[Envelope]) -> Vec<String> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::SystemNotice(n) if n.contains("missed the cache") => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// A warm session on a stub answering the chat request with `reply`.
async fn billed_session(reply: ChatReply) -> (LocalTransport, Summaries, agent_engine::session::ConversationSnapshot) {
    let (url, summaries) = stub_billed(reply).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let before = warm_conv(&mut a, 2).await;
    (a, summaries, before)
}

/// `/compact`, to `Idle`: what the client saw and the conversation after.
async fn compact_once(
    a: &mut LocalTransport,
) -> (Vec<Envelope>, agent_engine::session::ConversationSnapshot) {
    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(a, |e| matches!(e, SessionEventWire::Idle)).await;
    let conv = last_conversation(&seen);
    (seen, conv)
}

/// A compaction's own usage is in the session's totals and cost, once, on
/// each path: the chat's own request (a cache read), then, compacting the
/// summary again (its history is not the cached one), the transcript
/// request. No line about the cache: it was read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_spend_is_in_the_session_cost_once_on_each_path() {
    let _h = Home::new();
    let reply = ChatReply { cache_read: 4_000, cache_write: 20, tool_call: false };
    let (mut a, summaries, before) = billed_session(reply).await;
    assert!(before.cost > 0.0, "the turns were booked");

    let (seen, chat) = compact_once(&mut a).await;
    assert_eq!((summaries.chat.load(Ordering::SeqCst), summaries.transcript.load(Ordering::SeqCst)), (1, 0));
    assert_close(chat.cost, before.cost + cost_of(CHAT_INPUT, CHAT_OUTPUT, 4_000, 20));
    assert_eq!(chat.tokens.input, before.tokens.input + CHAT_INPUT);
    assert_eq!(chat.tokens.output, before.tokens.output + CHAT_OUTPUT);
    assert_eq!(chat.tokens.cache_read, before.tokens.cache_read + 4_000);
    assert_eq!(chat.tokens.cache_creation, before.tokens.cache_creation + 20);
    assert_eq!(miss_lines(&seen), Vec::<String>::new());

    let (seen, transcript) = compact_once(&mut a).await;
    assert_eq!((summaries.chat.load(Ordering::SeqCst), summaries.transcript.load(Ordering::SeqCst)), (1, 1));
    assert_close(transcript.cost, chat.cost + cost_of(TRANSCRIPT_INPUT, TRANSCRIPT_OUTPUT, 0, 0));
    assert_eq!(transcript.tokens.input, chat.tokens.input + TRANSCRIPT_INPUT);
    assert_eq!(transcript.tokens.output, chat.tokens.output + TRANSCRIPT_OUTPUT);
    assert_eq!(miss_lines(&seen), Vec::<String>::new());
    end(&mut a).await;
}

/// A chat request whose reply is refused (it called a tool) was paid for:
/// it is booked, and so is the transcript request sent after it. Each once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refused_chat_request_is_booked_with_its_fallback() {
    let _h = Home::new();
    let reply = ChatReply { cache_read: 4_000, cache_write: 20, tool_call: true };
    let (mut a, summaries, before) = billed_session(reply).await;

    let (seen, after) = compact_once(&mut a).await;
    assert_eq!((summaries.chat.load(Ordering::SeqCst), summaries.transcript.load(Ordering::SeqCst)), (1, 1));
    assert!(after.api_messages[0]["content"]
        .as_str()
        .is_some_and(|c| c.contains("SUMMARY: from the transcript.")));
    assert_close(
        after.cost,
        before.cost
            + cost_of(CHAT_INPUT, CHAT_OUTPUT, 4_000, 20)
            + cost_of(TRANSCRIPT_INPUT, TRANSCRIPT_OUTPUT, 0, 0),
    );
    assert_eq!(after.tokens.input, before.tokens.input + CHAT_INPUT + TRANSCRIPT_INPUT);
    assert_eq!(after.tokens.output, before.tokens.output + CHAT_OUTPUT + TRANSCRIPT_OUTPUT);
    assert_eq!(miss_lines(&seen), Vec::<String>::new());
    end(&mut a).await;
}

/// A chat request that wrote more cache than it read is said to the user,
/// once, after the disclosure line, with both numbers; and booked at what
/// it cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_missed_cache_is_said_to_the_user_once() {
    let _h = Home::new();
    let reply = ChatReply { cache_read: 100, cache_write: 4_000, tool_call: false };
    let (mut a, _summaries, before) = billed_session(reply).await;

    let (seen, after) = compact_once(&mut a).await;
    assert_eq!(
        miss_lines(&seen),
        ["compaction: the summary request missed the cache (wrote 4000, read 100 tokens)"]
    );
    let position = |pred: &dyn Fn(&SessionEventWire) -> bool| {
        seen.iter().position(|e| pred(&e.event)).expect("event")
    };
    let disclosed = position(&|e| matches!(e, SessionEventWire::CompactionStarted { .. }));
    let missed = position(&|e| matches!(e, SessionEventWire::SystemNotice(n) if n.contains("missed the cache")));
    let applied = position(&|e| matches!(e, SessionEventWire::CompactionApplied { .. }));
    assert!(disclosed < missed && missed < applied);
    assert_close(after.cost, before.cost + cost_of(CHAT_INPUT, CHAT_OUTPUT, 100, 4_000));
    end(&mut a).await;
}

/// The chat request's first event (with its usage), then the stream stays
/// open: the provider has billed the request, and nothing ends it.
fn chat_sse_open() -> String {
    let start = serde_json::json!({"type": "message_start", "message": {
        "id": "msg_sum", "type": "message", "role": "assistant", "content": [],
        "model": MODEL, "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": CHAT_INPUT, "output_tokens": 0,
            "cache_creation_input_tokens": 20, "cache_read_input_tokens": 4_000}}});
    format!("data: {start}\n\n")
}

/// Turns as usual; the chat request answers [`chat_sse_open`] and keeps
/// the connection open (keep-alive comments). `opened` counts the chat
/// requests whose usage has been sent.
async fn stub_open_chat() -> (String, Summaries, Arc<AtomicUsize>) {
    let summaries = Summaries::default();
    let opened = Arc::new(AtomicUsize::new(0));
    let (s, o) = (summaries.clone(), Arc::clone(&opened));
    let app = axum::Router::new().fallback(move |body: String| {
        let (s, o) = (s.clone(), Arc::clone(&o));
        async move {
            if body.contains("\"stream\":true") && body.contains(CHAT_SUMMARY_REQUEST) {
                s.chat.fetch_add(1, Ordering::SeqCst);
                let stream = futures::stream::once(async move {
                    o.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(chat_sse_open()))
                })
                .chain(futures::stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), ()))
                }));
                return axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap();
            }
            if body.contains("\"stream\":true") {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    SSE_HI.to_string(),
                )
                    .into_response();
            }
            s.transcript.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), summaries, opened)
}

/// `Status`: the session's cost and token totals.
async fn status(a: &mut LocalTransport, id: u64) -> (f64, u64, u64) {
    a.send(SessionCommand::Query { id, query: SessionQuery::Status }).await.unwrap();
    let seen = until(a, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => (
            value["cost"].as_f64().expect("cost"),
            value["tokens"]["input"].as_u64().expect("input"),
            value["tokens"]["output"].as_u64().expect("output"),
        ),
        _ => unreachable!(),
    }
}

/// Cancel while the chat request's stream is still open, after the
/// provider reported its usage: the request was billed, so it is booked,
/// once, and nothing is added later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_after_the_usage_arrived_books_it_once() {
    let _h = Home::new();
    let (url, summaries, opened) = stub_open_chat().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;
    let (cost, input, output) = status(&mut a, 1).await;

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::CompactionStarted { .. })).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while opened.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "the chat request was sent");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The usage is on the wire; give the client a moment to read it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::CompactionCancelled)));
    assert_eq!((summaries.chat.load(Ordering::SeqCst), summaries.transcript.load(Ordering::SeqCst)), (1, 0));

    let booked = status(&mut a, 2).await;
    assert_close(booked.0, cost + cost_of(CHAT_INPUT, 0, 4_000, 20));
    assert_eq!((booked.1, booked.2), (input + CHAT_INPUT, output));
    // Nothing lands later.
    drain_for(&mut a, Duration::from_secs(1)).await;
    let later = status(&mut a, 3).await;
    assert_close(later.0, booked.0);
    assert_eq!((later.1, later.2), (booked.1, booked.2));
    end(&mut a).await;
}

/// The chat request answers `reply`; the transcript request after it is
/// billed and, after `delay`, answers with no text, so it fails.
async fn stub_failing_fallback(reply: ChatReply, delay: Duration) -> (String, Summaries) {
    let summaries = Summaries::default();
    let s = summaries.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let s = s.clone();
        async move {
            if body.contains("\"stream\":true") {
                let sse = if body.contains(CHAT_SUMMARY_REQUEST) {
                    s.chat.fetch_add(1, Ordering::SeqCst);
                    chat_sse(reply)
                } else {
                    SSE_HI.to_string()
                };
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
                    .into_response();
            }
            s.transcript.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn", "content": [],
                "usage": {"input_tokens": TRANSCRIPT_INPUT, "output_tokens": TRANSCRIPT_OUTPUT}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), summaries)
}

/// A warm, saved session on [`stub_failing_fallback`]; what is on disk
/// before the compaction.
async fn saved_session_failing_fallback(
    delay: Duration,
) -> (LocalTransport, Summaries, String, agent_engine::core::session::Session) {
    let reply = ChatReply { cache_read: 4_000, cache_write: 20, tool_call: true };
    let (url, summaries) = stub_failing_fallback(reply, delay).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig { persist: true, ..cfg() })
        .await
        .unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    warm(&mut a, 2).await;
    let id = handle.journal_id();
    let saved = agent_engine::core::session::Session::load(&id).expect("saved after the turns");
    assert!(saved.session_cost > 0.0);
    (a, summaries, id, saved)
}

/// The chat request is billed (its reply calls a tool) and the transcript
/// request after it is billed and fails: at `Idle` the journal holds both
/// charges, with the history it had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_compaction_saves_what_it_was_billed() {
    let _h = Home::new();
    let (mut a, summaries, id, before) = saved_session_failing_fallback(Duration::ZERO).await;

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::CompactionFailed { .. })));
    assert_eq!((summaries.chat.load(Ordering::SeqCst), summaries.transcript.load(Ordering::SeqCst)), (1, 1));

    let after = agent_engine::core::session::Session::load(&id).unwrap();
    assert_close(
        after.session_cost,
        before.session_cost
            + cost_of(CHAT_INPUT, CHAT_OUTPUT, 4_000, 20)
            + cost_of(TRANSCRIPT_INPUT, TRANSCRIPT_OUTPUT, 0, 0),
    );
    assert_eq!(after.total_input_tokens, before.total_input_tokens + CHAT_INPUT + TRANSCRIPT_INPUT);
    assert_eq!(after.api_messages, before.api_messages, "the history is unchanged");
    assert_eq!(after.compacted_into, None);
    end(&mut a).await;
}

/// Cancel after the chat request was answered (and refused) while the
/// transcript request is still pending: at `Idle` the journal holds the
/// chat request's charge, with the history it had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cancelled_compaction_saves_what_it_was_billed() {
    let _h = Home::new();
    let (mut a, summaries, id, before) =
        saved_session_failing_fallback(Duration::from_secs(30)).await;

    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::CompactionStarted { .. })).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while summaries.transcript.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "the fallback was sent");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::CompactionCancelled)));

    let after = agent_engine::core::session::Session::load(&id).unwrap();
    assert_close(after.session_cost, before.session_cost + cost_of(CHAT_INPUT, CHAT_OUTPUT, 4_000, 20));
    assert_eq!(after.total_input_tokens, before.total_input_tokens + CHAT_INPUT);
    assert_eq!(after.api_messages, before.api_messages, "the history is unchanged");
    end(&mut a).await;
}

/// Linked successor: the predecessor's `compacted_into` link is written,
/// then advancing a named chain fails. The transition rolls back and the
/// failure path saves the session as it was: on disk it is unlinked, with
/// its history and the compaction's charge, its chain still points at it,
/// and no successor is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_transition_failing_after_the_link_saves_the_session_unlinked() {
    use agent_engine::runtime::compaction::{set_transition_failpoint, TransitionFailpoint};
    let _h = Home::new();
    let reply = ChatReply { cache_read: 4_000, cache_write: 20, tool_call: false };
    let (url, summaries) = stub_billed(reply).await;
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
    let id = handle.journal_id();
    let before = agent_engine::core::session::Session::load(&id).unwrap();
    agent_engine::chain::save_chain("work", &id).unwrap();

    set_transition_failpoint(Some((TransitionFailpoint::AtChainAdvance, 0)));
    a.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    set_transition_failpoint(None);
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::CompactionFailed { .. })));
    assert_eq!(summaries.chat.load(Ordering::SeqCst), 1);
    assert_eq!(handle.journal_id(), id, "the session stays on its own journal");

    let after = agent_engine::core::session::Session::load(&id).unwrap();
    assert_eq!(after.compacted_into, None, "the link is gone");
    assert_eq!(after.api_messages, before.api_messages, "the history is unchanged");
    assert_close(after.session_cost, before.session_cost + cost_of(CHAT_INPUT, CHAT_OUTPUT, 4_000, 20));
    assert_eq!(after.total_input_tokens, before.total_input_tokens + CHAT_INPUT);
    assert_eq!(agent_engine::chain::load_chain("work").unwrap().head, id);
    let ids: Vec<String> = agent_engine::core::session::list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![id.clone()], "no successor is left");
    end(&mut a).await;
}
