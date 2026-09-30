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

/// A client attaching mid-turn gets the latest history in its snapshot and a
/// replay of ONLY the round in flight: no per-round `MessageHistory` or
/// `Conversation` (which would re-ship the whole history once per round and
/// roll its mirror back), and none of the completed rounds' display events
/// (already in the snapshot's history: they rendered twice).
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
        !snap
            .replay
            .iter()
            .any(|e| matches!(e.event, SessionEventWire::Conversation(_))),
        "no stale Conversation in the turn replay"
    );
    // The completed rounds are in the snapshot's history: replaying their
    // tool calls too showed every finished round twice on attach.
    let replayed_calls: Vec<&str> = snap
        .replay
        .iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Stream(StreamEvent::Llm(
                LlmEvent::ToolUse { tool_id, .. } | LlmEvent::ToolUseStart { tool_id, .. },
            )) => Some(tool_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(replayed_calls.is_empty(), "completed rounds replayed: {replayed_calls:?}");
    assert!(
        snap.replay.iter().any(|e| is_text(&e.event)),
        "the round in flight is still replayed"
    );
    assert!(
        snap.replay
            .iter()
            .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })),
        "the turn start is kept"
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

// ── crash recovery: the in-flight turn draft ───────────────────────────────

use agent_engine::core::session_draft::{read_turn_draft, TurnDraft};
use agent_engine::core::session_lock::sessions_dir;
use agent_engine::engine::interrupt::InterruptReason;
use agent_engine::session::SessionCommand;

/// Poll the draft until `pred` holds (flushed on the 1 Hz turn tick).
async fn draft_until(id: &str, pred: impl Fn(Option<&TurnDraft>) -> bool) -> Option<TurnDraft> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let d = read_turn_draft(&sessions_dir(), id).expect("readable draft");
        if pred(d.as_ref()) {
            return d;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "draft never reached the expected state: {d:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The session's files exactly as they were at one instant.
struct DiskState(Vec<(std::path::PathBuf, Vec<u8>)>);

impl DiskState {
    fn capture(id: &str) -> Self {
        let dir = sessions_dir();
        DiskState(
            ["json", "journal", "turn"]
                .iter()
                .map(|ext| dir.join(format!("{id}.{ext}")))
                .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
                .collect(),
        )
    }
    /// Put the files back as captured: the disk state at the "crash".
    fn restore(&self, id: &str) {
        let dir = sessions_dir();
        for ext in ["json", "journal", "turn"] {
            let _ = std::fs::remove_file(dir.join(format!("{id}.{ext}")));
        }
        for (path, bytes) in &self.0 {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

/// Simulate `kill -9` mid-turn: capture the disk while the turn runs, let the
/// session end (its graceful cleanup is what a crash never gets to do), wait
/// until the actor is really gone (`Ended` precedes the task's exit, and a
/// `--continue` in that window attaches to the dying actor), then put the
/// captured files back.
async fn crash_mid_turn(
    a: &mut LocalTransport,
    handle: &agent_engine::session::SessionHandle,
    id: &str,
) {
    let at_crash = DiskState::capture(id);
    assert!(
        at_crash
            .0
            .iter()
            .any(|(p, _)| p.extension().is_some_and(|e| e == "turn")),
        "a running turn has a draft"
    );
    end(a).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.is_alive() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    at_crash.restore(id);
}

async fn continue_session(
    host: &Arc<agent_engine::EngineHost>,
    id: &str,
) -> (LocalTransport, agent_engine::session::AttachSnapshot) {
    let handle = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.to_string())),
            ..persist_cfg()
        })
        .await
        .unwrap();
    LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
        .await
        .unwrap()
}

fn text(m: &SharedMessage) -> String {
    match &m["content"] {
        serde_json::Value::String(s) => s.clone(),
        other => other[0]["text"].as_str().unwrap_or("").to_string(),
    }
}

/// The draft exists while a turn runs, carries the streamed reply, and is
/// gone once the turn ends — by completion or by cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn turn_draft_follows_the_reply_and_is_removed_at_turn_end() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let h1 = host().await;
    let handle = h1.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("stream something")).await.unwrap();
    until(&mut a, is_text).await;
    let d = draft_until(&id, |d| d.is_some_and(|d| d.partial_text == "hi"))
        .await
        .unwrap();
    assert_eq!(d.base_len, 1, "continues from the saved prompt");

    a.send(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    draft_until(&id, |d| d.is_none()).await;
    end(&mut a).await;

    // Normal completion removes it too.
    let _h2 = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host2 = host().await;
    let handle = host2.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("quick one")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    draft_until(&id, |d| d.is_none()).await;
    end(&mut a).await;
}

/// A long reply with no tool calls, cut by a crash: on `--continue` the
/// streamed text comes back as a real assistant message, then the crash
/// marker; the draft is gone and the recovery is on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn crash_mid_reply_is_recovered_on_continue() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("write me an essay")).await.unwrap();
    until(&mut a, is_text).await;
    draft_until(&id, |d| d.is_some_and(|d| d.partial_text == "hi")).await;
    crash_mid_turn(&mut a, &handle, &id).await;

    let (mut b, snap) = continue_session(&host, &id).await;
    let msgs = &snap.conversation.api_messages;
    let texts: Vec<String> = msgs.iter().map(text).collect();
    assert_eq!(
        texts,
        ["write me an essay", "hi", InterruptReason::Crash.marker()],
        "{msgs:#?}"
    );
    assert_eq!(msgs[1]["role"], "assistant");
    draft_until(&id, |d| d.is_none()).await;
    let saved = Session::load(&id).unwrap().api_messages;
    assert_eq!(saved.len(), 3, "the recovery was persisted");
    end(&mut b).await;
}

/// A crash between rounds: the completed round is kept (round checkpoint),
/// the in-flight reply's text comes back after it, then the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn crash_after_a_tool_round_keeps_the_round_and_the_partial_reply() {
    let _h = Home::new();
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([sse_read_round("toolu_c1"), SSE_PREFIX]));
    let (url, _) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("read then explain")).await.unwrap();
    until(&mut a, is_text).await;
    draft_until(&id, |d| {
        d.is_some_and(|d| d.base_len == 3 && d.partial_text == "hi")
    })
    .await;
    on_disk_until(&id, |m| m.len() == 3).await;
    crash_mid_turn(&mut a, &handle, &id).await;

    let (mut b, snap) = continue_session(&host, &id).await;
    let msgs = &snap.conversation.api_messages;
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "assistant", "user"],
        "{msgs:#?}"
    );
    assert_eq!(tool_use_ids(msgs), ["toolu_c1"]);
    assert!(has_result_for(msgs, "toolu_c1"));
    assert_eq!(text(&msgs[3]), "hi");
    assert_eq!(text(&msgs[4]), InterruptReason::Crash.marker());
    end(&mut b).await;
}

/// `/resume` into a session that crashed mid-turn recovers it the same way
/// (it now takes the session lock first, as NewSession does).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn resume_recovers_a_session_that_crashed_mid_turn() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let crashed = host.create_session(persist_cfg()).await.unwrap();
    let crashed_id = crashed.journal_id();
    let (mut a, _) = LocalTransport::attach(crashed.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("interrupted work")).await.unwrap();
    until(&mut a, is_text).await;
    draft_until(&crashed_id, |d| d.is_some_and(|d| d.partial_text == "hi")).await;
    crash_mid_turn(&mut a, &crashed, &crashed_id).await;

    let other = host.create_session(persist_cfg()).await.unwrap();
    let (mut b, _) = LocalTransport::attach(other.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    b.send(SessionCommand::Resume {
        id: 7,
        query: crashed_id.clone(),
    })
    .await
    .unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    let msgs = last_conversation(&seen).api_messages;
    let texts: Vec<String> = msgs.iter().map(text).collect();
    assert_eq!(
        texts,
        ["interrupted work", "hi", InterruptReason::Crash.marker()],
        "{msgs:#?}"
    );
    draft_until(&crashed_id, |d| d.is_none()).await;
    end(&mut b).await;
}

/// `/resume` of a session that is live in another actor (it holds the
/// session lock, its turn is running and its draft is open) is REFUSED, and
/// nothing changes on either side. It used to warn, drop its own lock, and
/// fold the live turn's draft in as a crash that never happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn resume_refuses_a_session_live_elsewhere() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let live = host.create_session(persist_cfg()).await.unwrap();
    let live_id = live.journal_id();
    let (mut a, _) = LocalTransport::attach(live.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("busy here")).await.unwrap();
    until(&mut a, is_text).await;
    let draft = draft_until(&live_id, |d| d.is_some_and(|d| d.partial_text == "hi")).await;

    let other = host.create_session(persist_cfg()).await.unwrap();
    let other_id = other.journal_id();
    let (mut b, _) = LocalTransport::attach(other.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    b.send(SessionCommand::Resume {
        id: 7,
        query: live_id.clone(),
    })
    .await
    .unwrap();
    let seen = until(&mut b, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 7, .. } | SessionEventWire::Resumed { .. })
    })
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => {
            assert_eq!(value["kind"], "error", "{value}");
            assert!(
                value["text"].as_str().unwrap().contains("cannot resume"),
                "{value}"
            );
        }
        other => panic!("resumed a session live elsewhere: {other:?}"),
    }
    assert_eq!(other.journal_id(), other_id, "B stayed on its own session");
    assert_eq!(
        read_turn_draft(&sessions_dir(), &live_id).unwrap(),
        draft,
        "the live turn's draft is untouched"
    );
    let on_disk = Session::load(&live_id).unwrap().api_messages;
    assert!(
        !on_disk.iter().map(text).any(|t| t == InterruptReason::Crash.marker()),
        "no crash recorded for a live turn: {on_disk:#?}"
    );

    // The live session is unaffected: its own cancel still works normally.
    a.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let msgs = last_conversation(&seen).api_messages;
    assert_eq!(text(msgs.last().unwrap()), InterruptReason::User.marker());
    end(&mut b).await;
    end(&mut a).await;
}

/// A turn's draft is removed only AFTER the history that ends the turn is
/// on disk: at no instant is the draft gone while the saved history still
/// lacks the turn's end (the marker, here). `cancel_turn` used to remove the
/// draft first, then save — and a save that timed out was lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_draft_outlives_the_save_that_ends_its_turn() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("stream something")).await.unwrap();
    until(&mut a, is_text).await;
    draft_until(&id, |d| d.is_some()).await;

    // Watch the disk from before the cancel until the draft is gone.
    let dir = sessions_dir();
    let watch_id = id.clone();
    let watcher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let draft = dir.join(format!("{watch_id}.turn")).exists();
            if !draft {
                // Gone: the concluding history must already be saved.
                let saved = Session::load(&watch_id).unwrap().api_messages;
                return saved.last().map(text);
            }
            assert!(std::time::Instant::now() < deadline, "draft never removed");
            std::thread::yield_now();
        }
    });
    a.send(SessionCommand::Cancel).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let last_saved = tokio::task::spawn_blocking(move || watcher.join().unwrap())
        .await
        .unwrap();
    assert_eq!(last_saved.as_deref(), Some(InterruptReason::User.marker()));
    end(&mut a).await;
}

/// A leftover draft from a turn that actually completed (only its removal
/// was lost) never alters the history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn stale_draft_of_a_completed_turn_is_just_removed() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("done quickly")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
    let before = Session::load(&id).unwrap().api_messages;
    assert_eq!(before.last().unwrap()["role"], "assistant");
    agent_engine::core::session_draft::write_turn_draft(
        &sessions_dir(),
        &id,
        &TurnDraft {
            base_len: 1,
            partial_text: "hi".into(),
        },
    )
    .unwrap();

    let (mut b, snap) = continue_session(&host, &id).await;
    assert_eq!(snap.conversation.api_messages, before, "history untouched");
    draft_until(&id, |d| d.is_none()).await;
    end(&mut b).await;
}
