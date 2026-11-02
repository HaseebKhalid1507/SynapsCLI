//! A session that quiesces (a host checkpoint, a reload's
//! drain) ends a running `wait` as `interrupted`, and its turn stops at the
//! next round boundary on its own: no new provider round, no cancel.
//!
//! Production actor → runtime → Anthropic transport against a loopback SSE
//! stub; the stub's hit count is the number of provider rounds.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_engine::events::queue::Quiesce;
use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport,
    SessionCommand, SessionEventWire,
};
use agent_engine::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};
use axum::response::IntoResponse;
use serial_test::serial;

// ── fixtures ─────────────────────────────────────────────────────────────────

/// One complete assistant round that calls `name` with `input` (JSON text).
fn sse_tool(id: &str, name: &str, input: &str) -> &'static str {
    let escaped = serde_json::to_string(input).unwrap();
    Box::leak(
        format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_{id}\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
                "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,",
                "\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{escaped}}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
                "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
                "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            id = id,
            name = name,
            escaped = escaped,
        )
        .into_boxed_str(),
    )
}

/// Round 1 calls `wait` on a file that never appears (60 s timeout); every
/// later round answers "hi".
async fn wait_stub(home: &Home) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let never = home.dir.path().join("never-appears");
    let input = serde_json::json!({
        "any": [{"file": never.to_str().unwrap()}],
        "timeout_secs": 60,
    })
    .to_string();
    let call = sse_tool("toolu_wait", "wait", &input);
    let bodies: &'static [&'static str] = Box::leak(Box::new([call, SSE_HI]));
    stub_seq(bodies).await
}

async fn attach(handle: &agent_engine::session::SessionHandle) -> LocalTransport {
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientJoined { .. })).await;
    t
}

/// Submit, then return once the model's `wait` call is running.
async fn start_the_wait(a: &mut LocalTransport) {
    a.send(submit("wait for it")).await.unwrap();
    until(a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolUse { tool_name, .. }))
                if tool_name == "wait"
        )
    })
    .await;
    // Past the call's first condition pass, so the quiesce lands mid-wait.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn aborted(seen: &[Envelope]) -> bool {
    seen.iter()
        .any(|e| matches!(e.event, SessionEventWire::Aborted { .. }))
}

fn notice(seen: &[Envelope], text: &str) -> bool {
    seen.iter().any(|e| {
        matches!(
            &e.event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(t))) if t == text
        )
    })
}

fn the_wait_result(seen: &[Envelope]) -> String {
    let conv = last_conversation(seen);
    let results = tool_results(&conv);
    assert_eq!(results.len(), 1, "{:#?}", conv.api_messages);
    results[0].clone()
}

fn history_text(seen: &[Envelope]) -> String {
    serde_json::to_string(&last_conversation(seen).api_messages).unwrap()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// A host checkpoint during a wait: the wait returns `interrupted`
/// (checkpoint), the turn ends at the round boundary with no second provider
/// round and no cancel, the checkpoint completes inside its grace, the flag
/// is cleared, and the session takes the next prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_host_checkpoint_ends_a_wait_and_its_turn_without_a_cancel() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    let started = Instant::now();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let took = started.elapsed();

    let r = the_wait_result(&seen);
    assert!(r.contains("interrupted"), "{r}");
    assert!(r.contains("the host is checkpointing this session"), "{r}");
    assert!(
        r.contains("Nothing will wake this session for this wait"),
        "{r}"
    );
    assert!(!r.contains("Canceled"), "the wait ended, not its cancel: {r}");
    assert!(!aborted(&seen), "the turn ended on its own, no cancel");
    assert!(notice(
        &seen,
        "Turn stopped at a round boundary: the session is checkpointing."
    ));
    assert!(
        seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)),
        "the turn's end announced Idle"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no round after the wait");
    assert!(
        took < Duration::from_secs(2),
        "inside the quiesce grace, not at its end: {took:?}"
    );
    let last = last_conversation(&seen).api_messages.last().cloned().unwrap();
    assert_eq!(last["role"], "user", "history ends on the paired tool_result");
    assert_eq!(last["content"][0]["type"], "tool_result", "{last}");
    assert_eq!(handle.quiesced(), None, "a checkpoint's flag ends with it");

    a.send(submit("again")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the next prompt runs");
    assert_eq!(
        last_conversation(&seen).api_messages.last().unwrap()["role"],
        "assistant"
    );
    end(&mut a).await;
}

/// A checkpoint that lands while a provider round is still in flight gives
/// the turn its grace: the round's response arrives later, its tool runs,
/// and the turn stops at the next round boundary on its own (no cancel, no
/// second round). A zero grace would cancel it instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_round_in_flight_at_a_checkpoint_finishes_inside_the_grace() {
    let home = Home::new();
    let present = home.dir.path().join("present");
    std::fs::write(&present, b"here\n").unwrap();
    let input = serde_json::json!({
        "any": [{"file": present.to_str().unwrap()}],
        "timeout_secs": 5,
    })
    .to_string();
    let call = sse_tool("toolu_slow", "wait", &input);
    // Every hit answers after 700 ms: the checkpoint lands mid-round.
    let (url, hits) = stub_slow(call, Duration::from_millis(700)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("go")).await.unwrap();
    let t0 = Instant::now();
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the round never reached the stub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = Instant::now();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let took = started.elapsed();
    // Measured under load in the gate (`--show-output`).
    eprintln!("quiesce-took round_in_flight: {took:?}");

    assert!(!aborted(&seen), "the turn ended on its own, no cancel");
    assert!(notice(
        &seen,
        "Turn stopped at a round boundary: the session is checkpointing."
    ));
    let r = the_wait_result(&seen);
    let json: serde_json::Value =
        serde_json::from_str(r.split_once('\n').map_or(r.as_str(), |(_, j)| j)).unwrap();
    assert_eq!(json["outcome"], "met", "the in-flight round's tool ran: {r}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no round after the tool");
    assert!(
        took >= Duration::from_millis(400),
        "the checkpoint waited for the round: {took:?}"
    );
    assert!(took < Duration::from_secs(2), "inside the grace: {took:?}");
    let last = last_conversation(&seen).api_messages.last().cloned().unwrap();
    assert_eq!(last["role"], "user", "history ends on the paired tool_result");
    assert_eq!(last["content"][0]["type"], "tool_result", "{last}");
    end(&mut a).await;
}

/// A reload's quiesce flag alone (the daemon's drain, before any
/// checkpoint) ends the wait as `interrupted` (reload) and the turn at its
/// round boundary; the flag stays until the reload lifts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_flag_ends_a_wait_and_its_turn_before_any_checkpoint() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    let started = Instant::now();
    handle.quiesce(Some(Quiesce::Reload));
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    let r = the_wait_result(&seen);
    assert!(r.contains("interrupted"), "{r}");
    assert!(r.contains("the daemon is reloading"), "{r}");
    assert!(!aborted(&seen));
    assert!(notice(
        &seen,
        "Turn stopped at a round boundary: the daemon is reloading."
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(handle.quiesced(), Some(Quiesce::Reload), "held until lifted");

    handle.quiesce(None);
    a.send(submit("again")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    end(&mut a).await;
}

/// A message the user steered in during the wait is not buried in history
/// with no round to answer it, and starts no turn inside the checkpoint's
/// grace: it comes back to its sender as `Dequeued`, as a cancel returns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_steer_during_the_wait_is_returned_by_a_checkpoint() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    a.send(SessionCommand::Steer {
        text: "follow-up-steer".into(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;

    assert!(
        seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::Dequeued { text } if text == "follow-up-steer"
        )),
        "the steer goes back to its sender"
    );
    assert!(
        !history_text(&seen).contains("follow-up-steer"),
        "never delivered into history: {}",
        history_text(&seen)
    );
    assert!(!aborted(&seen));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no turn for it in the grace");
    end(&mut a).await;
}

/// During a reload's drain (no checkpoint yet), the same steer becomes the
/// next turn's prompt, and that turn's first round runs: a turn's first
/// round is exempt from the quiesce stop, so the user's message is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_steer_during_a_reload_drain_gets_its_own_turn() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    a.send(SessionCommand::Steer {
        text: "follow-up-steer".into(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    handle.quiesce(Some(Quiesce::Reload));
    // The wait's turn ends (Idle is not announced between the two turns:
    // the queued message starts the next one at once), then the steer's
    // turn answers and ends.
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    assert_eq!(hits.load(Ordering::SeqCst), 2, "one round for the steer");
    let msgs = last_conversation(&seen).api_messages;
    let n = msgs.len();
    assert!(n >= 2, "{msgs:#?}");
    assert_eq!(msgs[n - 2]["role"], "user");
    assert_eq!(msgs[n - 2]["content"], "follow-up-steer", "{msgs:#?}");
    assert_eq!(msgs[n - 1]["role"], "assistant");
    assert!(!aborted(&seen));
    handle.quiesce(None);
    end(&mut a).await;
}

/// A reload that never execs: the quiesce flag the daemon set for its drain
/// is lifted with the fence (`ReloadAborted`), so the session works
/// normally again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_exec_lifts_the_reload_quiesce() {
    let home = Home::new();
    let (url, _hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    // As the daemon does before its drain.
    handle.quiesce(Some(Quiesce::Reload));
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert_eq!(
        handle.quiesced(),
        Some(Quiesce::Reload),
        "a reload's flag stays until the exec"
    );
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SystemNotice(_))).await;
    assert_eq!(handle.quiesced(), None, "lifted with the fence");
    end(&mut a).await;
}

/// The turns that started with `text` as their user message.
fn turns_started_with(seen: &[Envelope], text: &str) -> usize {
    seen.iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEventWire::TurnStarted {
                    trigger: agent_engine::session::TurnTrigger::QueuedAuto,
                    user_text: Some(t),
                    ..
                } if t == text
            )
        })
        .count()
}

/// The reload's interruption markers in `messages`.
fn restart_markers(messages: &[agent_engine::SharedMessage]) -> usize {
    let marker = agent_engine::engine::interrupt::InterruptReason::Restart.marker();
    messages.iter().filter(|m| m["content"] == marker).count()
}

/// Spec §5: a reload refused after its drain (before anything was stopped)
/// lifts the quiesce, and a turn the drain paused at a round boundary
/// resumes in place: ONE resume message, however often the refusal
/// arrives, and no second marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refused_reload_resumes_the_turn_its_drain_paused_in_place() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    // As the daemon does before its drain: the turn stops on its own.
    handle.quiesce(Some(Quiesce::Reload));
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let msgs = last_conversation(&seen).api_messages;
    assert_eq!(restart_markers(&msgs), 1, "{msgs:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // As the daemon's refusal does, in its order (twice here: one resume all
    // the same). The daemon lifts no quiesce; the session lifts its own when
    // it handles the command.
    let at = chrono::Utc::now();
    for _ in 0..2 {
        agent_engine::daemon::reload::refuse_sessions(std::slice::from_ref(&handle), at).await;
    }
    let text = agent_engine::daemon::reload::refused_resume_text(at);
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(turns_started_with(&seen, &text), 1, "{seen:#?}");
    let msgs = last_conversation(&seen).api_messages;
    assert_eq!(restart_markers(&msgs), 1, "no second marker: {msgs:#?}");
    assert_eq!(msgs.last().unwrap()["role"], "assistant", "{msgs:#?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one round for the resume, and no second resume");
    assert_eq!(handle.quiesced(), None);
    end(&mut a).await;
}

/// Unsets the `testing` seam's variable however the test ends.
struct HoldDoneAfterQuiesceStop;

impl HoldDoneAfterQuiesceStop {
    fn set() -> Self {
        std::env::set_var("SYNAPS_TEST_HOLD_DONE_AFTER_QUIESCE_STOP", "1");
        Self
    }
}

impl Drop for HoldDoneAfterQuiesceStop {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_TEST_HOLD_DONE_AFTER_QUIESCE_STOP");
    }
}

/// Spec §5, the race at the lift: the refusal reaches the session after its
/// turn's stream stopped for the reload but before the turn's end (the
/// `testing` seam holds the turn's `Done` until the quiesce lifts, which in
/// the daemon's order only the session's own `ReloadRefused` does). The turn
/// still gets its one restart marker and its one resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refusal_between_the_quiesce_stop_and_the_turns_end_still_resumes_it() {
    let _seam = HoldDoneAfterQuiesceStop::set();
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    handle.quiesce(Some(Quiesce::Reload));
    // The stream stopped for the reload; its end is held.
    let seen = until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(t)))
                if t == "Turn stopped at a round boundary: the daemon is reloading."
        )
    })
    .await;
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)));
    // The daemon's refusal, with the quiesce still up: the session lifts it.
    let at = chrono::Utc::now();
    agent_engine::daemon::reload::refuse_sessions(std::slice::from_ref(&handle), at).await;
    let text = agent_engine::daemon::reload::refused_resume_text(at);
    let mut seen = until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::TurnStarted { user_text: Some(t), .. } if *t == text
        )
    })
    .await;
    seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await);
    assert_eq!(turns_started_with(&seen, &text), 1, "{seen:#?}");
    let msgs = last_conversation(&seen).api_messages;
    assert_eq!(restart_markers(&msgs), 1, "one marker: {msgs:#?}");
    assert_eq!(msgs.last().unwrap()["role"], "assistant", "{msgs:#?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one round for the resume, and no second resume");
    assert_eq!(handle.quiesced(), None);
    end(&mut a).await;
}

/// Spec §5, the control: a refusal that reaches a streaming turn which never
/// stopped for the reload (its round answers with text) adds no marker and
/// no resume when the turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refusal_during_a_turn_that_never_stopped_for_the_reload_adds_nothing() {
    let _home = Home::new();
    let (url, hits, gate) = stub_gated(SSE_HI).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_a_held_round(&mut a, &hits).await;
    handle.quiesce(Some(Quiesce::Reload));
    let at = chrono::Utc::now();
    handle
        .send(SessionCommand::ReloadRefused { refused_at: at })
        .await
        .unwrap();
    let t0 = Instant::now();
    while handle.quiesced().is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the refusal never lifted the quiesce");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    gate.add_permits(1);
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let text = agent_engine::daemon::reload::refused_resume_text(at);
    assert_eq!(turns_started_with(&seen, &text), 0, "{seen:#?}");
    let msgs = last_conversation(&seen).api_messages;
    assert_eq!(restart_markers(&msgs), 0, "{msgs:#?}");
    assert!(history_text(&seen).contains("hi"));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no resume round");
    end(&mut a).await;
}

/// A stub whose every response waits for one permit of the returned gate
/// (the test releases it), then serves `body`. `hits` counts the requests
/// that reached it, released or not.
async fn stub_gated(body: &'static str) -> (String, Arc<AtomicUsize>, Arc<tokio::sync::Semaphore>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (hits_c, gate_c) = (Arc::clone(&hits), Arc::clone(&gate));
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        let gate = Arc::clone(&gate_c);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.expect("gate open").forget();
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                body.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, gate)
}

/// Every steering drain honours the quiesce flag, the no-tool one included.
/// A provider round in flight at a checkpoint answers with text only, inside
/// the grace; a message steered in while it ran is not delivered into that
/// round's history (no round would answer it: the loop top stops the turn),
/// it goes back to its sender as `Dequeued`. Barrier-driven: the round's
/// response is released only after the steer is queued and the checkpoint's
/// flag is up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_text_only_round_during_checkpoint_returns_queued_steering() {
    let _home = Home::new();
    let (url, hits, gate) = stub_gated(SSE_HI).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("go")).await.unwrap();
    let t0 = Instant::now();
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the round never reached the stub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.send(SessionCommand::Steer {
        text: "steer-b".into(),
    })
    .await
    .unwrap();
    until(
        &mut a,
        |e| matches!(e, SessionEventWire::Steered { text, delivered: true } if text == "steer-b"),
    )
    .await;
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    let t0 = Instant::now();
    while handle.quiesced() != Some(Quiesce::Checkpoint) {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the checkpoint never raised its flag"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Now the in-flight round answers, with text and no tool call.
    gate.add_permits(1);
    let seen = until(
        &mut a,
        |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID),
    )
    .await;

    assert_eq!(hits.load(Ordering::SeqCst), 1, "one provider request");
    assert!(
        seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::Dequeued { text } if text == "steer-b"
        )),
        "the steer goes back to its sender"
    );
    assert!(
        !seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::Stream(StreamEvent::Agent(AgentEvent::SteeringDelivered { message }))
                if message == "steer-b"
        )),
        "never delivered into the ended round"
    );
    let history = history_text(&seen);
    assert!(
        history.contains("hi"),
        "the round's answer is kept: {history}"
    );
    assert!(
        !history.contains("steer-b"),
        "never in the answered history: {history}"
    );
    assert!(!aborted(&seen), "the round ended inside the grace");
    end(&mut a).await;
}

/// Reload-resume with the drain's quiesce: a turn the reload's drain stops
/// at a round boundary (no cancel) is cut as surely as a cancelled one. Its
/// history ends with the restart marker, the reload's checkpoint (the
/// session idle by then) records `turn_cut`, and the next image's
/// `ResumeAfterReload` starts ONE turn for it, however often it arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_the_drain_stopped_at_a_round_boundary_is_resumed_after_the_reload() {
    let home = Home::new();
    let (url, hits) = wait_stub(&home).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_the_wait(&mut a).await;
    // As the daemon does before its drain: the turn ends on its own.
    handle.quiesce(Some(Quiesce::Reload));
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!aborted(&seen));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let msgs = last_conversation(&seen).api_messages;
    let marker = agent_engine::engine::interrupt::InterruptReason::Restart.marker();
    assert_eq!(msgs.last().unwrap()["content"], marker, "{msgs:#?}");
    assert_eq!(msgs[msgs.len() - 2]["content"][0]["type"], "tool_result", "{msgs:#?}");

    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(value["record"]["turn_cut"], serde_json::Value::Bool(true), "{value}");

    // No exec here: lift the fence and the quiesce as a failed exec does,
    // then the next image's command, twice: ONE resume turn, one new round.
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SystemNotice(_))).await;
    let at = chrono::Utc::now();
    for _ in 0..2 {
        handle
            .send(SessionCommand::ResumeAfterReload {
                generation: 4,
                reloaded_at: at,
                turn_cut: Some(true),
                cut_tail: None,
                stopped: false,
                reply: None,
            })
            .await
            .unwrap();
    }
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let text = agent_engine::daemon::reload::resume_text(at);
    let resumed = seen
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEventWire::TurnStarted {
                    trigger: agent_engine::session::TurnTrigger::QueuedAuto,
                    user_text: Some(t),
                    ..
                } if *t == text
            )
        })
        .count();
    assert_eq!(resumed, 1, "{seen:#?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one round for the resume, and no second resume");
    end(&mut a).await;
}

/// Submit, then return once the turn's provider round is held at the stub.
async fn start_a_held_round(a: &mut LocalTransport, hits: &AtomicUsize) {
    a.send(submit("go")).await.unwrap();
    let t0 = Instant::now();
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the round never reached the stub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A reload whose drain ran to its cap (`skip_quiesce_grace`): a turn still
/// in a provider round at its checkpoint is cut at once by the cancel
/// backstop, with no quiesce grace on top of the drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_checkpoint_after_a_full_cap_drain_skips_the_quiesce_grace() {
    let _home = Home::new();
    let (url, hits, gate) = stub_gated(SSE_HI).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_a_held_round(&mut a, &hits).await;
    // As the daemon does before its drain (which then ran to its cap).
    handle.quiesce(Some(Quiesce::Reload));
    let started = Instant::now();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: true,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let took = started.elapsed();
    eprintln!("checkpoint-took full_cap_drain: {took:?}");

    assert!(
        took < Duration::from_secs(1),
        "no quiesce grace after a full-cap drain: {took:?}"
    );
    assert!(aborted(&seen), "the round in flight was cancelled, not waited for");
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(value["record"]["turn_cut"], serde_json::Value::Bool(true), "{value}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one provider request");
    gate.add_permits(1);
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SystemNotice(_))).await;
    end(&mut a).await;
}

/// A reload whose drain ended early (`skip_quiesce_grace: false`) keeps the
/// quiesce grace: a round in flight at its checkpoint answers inside it and
/// the turn ends on its own, with no cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_checkpoint_after_an_early_drain_keeps_the_quiesce_grace() {
    let _home = Home::new();
    let (url, hits, gate) = stub_gated(SSE_HI).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    start_a_held_round(&mut a, &hits).await;
    handle.quiesce(Some(Quiesce::Reload));
    let started = Instant::now();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    // The held round answers 500 ms into the grace.
    let release = Arc::clone(&gate);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        release.add_permits(1);
    });
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let took = started.elapsed();
    eprintln!("checkpoint-took early_drain: {took:?}");

    assert!(!aborted(&seen), "the round ended inside the grace, no cancel");
    assert!(
        took >= Duration::from_millis(400),
        "the checkpoint waited for the round: {took:?}"
    );
    let history = history_text(&seen);
    assert!(history.contains("hi"), "the round's answer is kept: {history}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one provider request");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SystemNotice(_))).await;
    end(&mut a).await;
}
