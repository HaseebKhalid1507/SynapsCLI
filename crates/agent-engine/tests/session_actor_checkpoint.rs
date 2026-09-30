//! Mid-turn durability: the engine publishes the conversation at every round
//! boundary (the prompt before the first request, each completed tool round,
//! rollover heads) and the actor persists each one — the session on disk
//! follows a running turn instead of catching up only when it ends.
//!
//! Loopback Anthropic SSE stub; production runtime / actor / journal.

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::core::session::Session;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionConfig, SessionEventWire,
};
use agent_engine::{LlmEvent, SessionEvent, SharedMessage, StreamEvent};
use futures::StreamExt;
use serde_json::json;
use serial_test::serial;
use tokio_util::sync::CancellationToken;

/// One complete assistant round calling `read` on a path that does not exist
/// (fast, deterministic, no side effects).
fn sse_read_round(id: &str) -> &'static str {
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_{id}\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"read\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,",
                "\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"path\\\":\\\"/nonexistent/synaps-checkpoint-fixture\\\"}}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
        )
        .into_boxed_str(),
    )
}

fn is_text(e: &SessionEventWire) -> bool {
    matches!(
        e,
        SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(_)))
    )
}

fn tool_use_ids(msgs: &[SharedMessage]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| m["content"].as_array())
        .flat_map(|b| b.iter())
        .filter(|b| b["type"] == "tool_use")
        .map(|b| b["id"].as_str().unwrap_or("").to_string())
        .collect()
}

fn has_result_for(msgs: &[SharedMessage], id: &str) -> bool {
    msgs.iter()
        .filter_map(|m| m["content"].as_array())
        .flat_map(|b| b.iter())
        .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
}

/// Poll the session file until `pred` holds (saves run on a blocking pool).
async fn on_disk_until(id: &str, pred: impl Fn(&[SharedMessage]) -> bool) -> Vec<SharedMessage> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = Session::load(id) {
            if pred(&s.api_messages) {
                return s.api_messages;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session file never reached the expected state: {:#?}",
            Session::load(id).map(|s| s.api_messages)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn persist_cfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

// ── engine ───────────────────────────────────────────────────────────────────

/// The engine publishes the history before the first request (the prompt),
/// after each completed tool round, and at the end — each an append-only
/// extension of the previous, never the same history twice in a row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn engine_publishes_append_only_history_at_every_round_boundary() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        sse_read_round("toolu_r1"),
        sse_read_round("toolu_r2"),
        SSE_HI,
    ]));
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut rt = agent_engine::Runtime::new().await.unwrap();
    rt.set_model(MODEL.into());

    let prompt: SharedMessage = Arc::new(json!({"role": "user", "content": "do the thing"}));
    let mut stream = rt
        .run_stream_with_messages(
            vec![prompt.clone()],
            CancellationToken::new(),
            None,
            None,
            false,
        )
        .await;
    // (history, provider requests made when it was published, first LLM event seen yet?)
    let mut published: Vec<(Vec<SharedMessage>, usize, bool)> = Vec::new();
    let mut saw_llm = false;
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("turn hung")
    {
        match ev {
            StreamEvent::Session(SessionEvent::MessageHistory(h)) => {
                published.push((h, hits.load(std::sync::atomic::Ordering::SeqCst), saw_llm))
            }
            StreamEvent::Llm(_) => saw_llm = true,
            StreamEvent::Session(SessionEvent::Done) => break,
            _ => {}
        }
    }

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    let lens: Vec<usize> = published.iter().map(|(h, ..)| h.len()).collect();
    // prompt | + round 1 (tool_use, tool_result) | + round 2 | + final reply
    assert_eq!(lens, [1, 3, 5, 6], "one publish per round boundary");
    // The prompt was published before any model output.
    assert!(
        !published[0].2,
        "prompt checkpoint precedes the first response"
    );
    assert!(Arc::ptr_eq(&published[0].0[0], &prompt));
    // Each checkpoint extends the previous one — same Arcs, append-only.
    for w in published.windows(2) {
        let (prev, next) = (&w[0].0, &w[1].0);
        assert!(next.len() > prev.len());
        for (a, b) in prev.iter().zip(next.iter()) {
            assert!(Arc::ptr_eq(a, b), "a published message was rebuilt");
        }
    }
    let round1 = &published[1].0;
    assert_eq!(tool_use_ids(round1), ["toolu_r1"]);
    assert!(has_result_for(round1, "toolu_r1"));
}

// ── actor: what is on disk while the turn is still running ───────────────────

/// The prompt reaches disk before the model has finished its first response
/// (previously nothing reached disk until the turn ended).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn prompt_is_on_disk_while_the_first_response_streams() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await; // never finishes
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("remember this prompt")).await.unwrap();
    until(&mut a, is_text).await; // mid-response, turn still running

    let saved = on_disk_until(&handle.journal_id(), |m| !m.is_empty()).await;
    assert_eq!(saved.len(), 1, "{saved:#?}");
    assert_eq!(saved[0]["content"], "remember this prompt");
    end(&mut a).await;
}

/// Each completed tool round is on disk while the next round is still
/// streaming — a crash mid-turn loses at most the round in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn completed_tool_rounds_are_on_disk_while_the_turn_runs() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        sse_read_round("toolu_d1"),
        sse_read_round("toolu_d2"),
        SSE_PREFIX,
    ]));
    let (url, hits) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("two rounds then stream")).await.unwrap();
    until(&mut a, is_text).await; // third response is streaming
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);

    let saved = on_disk_until(&handle.journal_id(), |m| m.len() >= 5).await;
    assert_eq!(saved[0]["content"], "two rounds then stream");
    assert_eq!(tool_use_ids(&saved), ["toolu_d1", "toolu_d2"]);
    assert!(has_result_for(&saved, "toolu_d1") && has_result_for(&saved, "toolu_d2"));
    let roles: Vec<&str> = saved.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "assistant", "user"],
        "valid history"
    );
    end(&mut a).await;
}

/// A client attaching mid-turn gets the latest history in its snapshot and
/// NO per-round `MessageHistory` in the replay (which would re-ship the whole
/// history once per round and roll its mirror back).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mid_turn_attach_replays_no_stale_histories() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        sse_read_round("toolu_a1"),
        sse_read_round("toolu_a2"),
        SSE_PREFIX,
    ]));
    let (url, _) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("attach during this")).await.unwrap();
    until(&mut a, is_text).await;

    let (b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    assert!(snap.streaming);
    let msgs = &snap.conversation.api_messages;
    assert_eq!(
        tool_use_ids(msgs),
        ["toolu_a1", "toolu_a2"],
        "snapshot has the latest round"
    );
    let stale = snap
        .replay
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
            )
        })
        .count();
    assert_eq!(stale, 0, "no MessageHistory in the turn replay");
    assert!(
        snap.replay.iter().any(|e| is_text(&e.event)),
        "the turn's display events are still replayed"
    );
    drop(b);
    end(&mut a).await;
}

/// What a crash mid-turn leaves on disk is a VALID history to resume from:
/// every tool_use paired with its result, ending on a user turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn saved_mid_turn_history_is_valid_to_resume() {
    let _h = Home::new();
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([sse_read_round("toolu_v1"), SSE_PREFIX]));
    let (url, _) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("resume me")).await.unwrap();
    until(&mut a, is_text).await;

    let saved = on_disk_until(&handle.journal_id(), |m| m.len() >= 3).await;
    // Every tool_use has its result; the history ends on a user turn, so a
    // resumed session can send the next message straight after it.
    for id in tool_use_ids(&saved) {
        assert!(has_result_for(&saved, &id), "unpaired {id}");
    }
    assert_eq!(saved.last().unwrap()["role"], "user");
    end(&mut a).await;
}
