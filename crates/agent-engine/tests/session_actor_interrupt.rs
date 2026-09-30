//! Interrupted turns are recorded as REAL history plus one interruption
//! marker — never as a recap folded into the next user message (the old
//! "ABORT CONTEXT", which current models refuse as a prompt injection).
//!
//! Each test drives the production actor → runtime → Anthropic transport
//! against a loopback SSE stub and asserts the adopted history, the marker,
//! and — for caching — the exact request bytes the provider sees next.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::engine::interrupt::{is_interruption_marker, InterruptReason};
use agent_engine::session::display::is_event_payload;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::{LlmEvent, SessionEvent, StreamEvent};
use axum::response::IntoResponse;
use futures::StreamExt;
use serde_json::{json, Value};
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

fn sse_read_missing() -> &'static str {
    sse_tool(
        "toolu_read",
        "read",
        r#"{"path":"/nonexistent/synaps-interrupt-fixture"}"#,
    )
}

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Sequenced stub (hit `i` serves `bodies[min(i, len-1)]`) that records
/// every request body. Only hit `endless` (if any) keeps the connection open
/// with keep-alives after its body — the cancel fixture; every other body
/// closes (the SSE parser reads until the connection ends).
async fn capture_stub(
    bodies: &'static [&'static str],
    endless: Option<usize>,
) -> (String, Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (hits, seen) = (Arc::clone(&hits_c), Arc::clone(&seen_c));
        async move {
            let i = hits.fetch_add(1, Ordering::SeqCst);
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                seen.lock().unwrap().push(v);
            }
            let idx = i.min(bodies.len() - 1);
            let body = bodies[idx];
            if endless == Some(i) {
                let stream = futures::stream::once(async move {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(body))
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
    (format!("http://{addr}"), hits, seen)
}

async fn attach(handle: &agent_engine::session::SessionHandle) -> LocalTransport {
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    until(&mut t, |e| {
        matches!(e, SessionEventWire::ClientJoined { .. })
    })
    .await;
    t
}

fn is_text(e: &SessionEventWire) -> bool {
    matches!(
        e,
        SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(_)))
    )
}

/// Cancel, then collect through the cancel's `Idle`.
async fn cancel(t: &mut LocalTransport) -> Vec<agent_engine::session::Envelope> {
    t.send(SessionCommand::Cancel).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await
}

fn aborted(seen: &[agent_engine::session::Envelope]) -> Vec<bool> {
    seen.iter()
        .filter_map(|e| match e.event {
            SessionEventWire::Aborted { context_saved } => Some(context_saved),
            _ => None,
        })
        .collect()
}

fn text_of(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every `tool_use` id has exactly one `tool_result` later in history.
fn assert_tools_paired(msgs: &[Arc<Value>]) {
    let blocks = |ty: &'static str, key: &'static str| -> Vec<String> {
        msgs.iter()
            .filter_map(|m| m["content"].as_array())
            .flat_map(|b| b.iter())
            .filter(|b| b["type"] == ty)
            .map(|b| b[key].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut uses = blocks("tool_use", "id");
    let mut results = blocks("tool_result", "tool_use_id");
    uses.sort();
    results.sort();
    assert_eq!(uses, results, "tool_use/tool_result pairing");
}

fn assert_no_recap(msgs: &[Arc<Value>]) {
    let all = serde_json::to_string(msgs).unwrap();
    assert!(!all.contains("ABORT CONTEXT"), "no recap in history: {all}");
}

fn strip_cache_control(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip_cache_control(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_cache_control).collect()),
        other => other.clone(),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Esc mid-reply: the partial reply stays as a REAL assistant message, the
/// marker follows, usage for the interrupted round is billed, nothing is
/// folded into the next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_mid_reply_keeps_the_partial_reply_and_appends_the_marker() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("hello")).await.unwrap();
    until(&mut a, is_text).await;
    let seen = cancel(&mut a).await;

    assert_eq!(aborted(&seen), vec![true], "one Aborted, partial work kept");
    let conv = last_conversation(&seen);
    let msgs = &conv.api_messages;
    assert_eq!(msgs.len(), 3, "{msgs:#?}");
    assert_eq!(msgs[0]["content"], "hello");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], json!([{"type": "text", "text": "hi"}]));
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"], InterruptReason::User.marker());
    assert!(conv.abort_context.is_none());
    assert!(
        conv.tokens.input > 0,
        "the interrupted round's usage is billed"
    );
    assert_no_recap(msgs);
    end(&mut a).await;
}

/// A steer typed mid-reply and not yet delivered when Esc lands is DEQUEUED
/// (the UI shows "dequeued: …") — it never enters history after the cancel,
/// whichever point of the turn the cancel interrupts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn undelivered_steer_is_dequeued_not_slipped_into_history() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("hello")).await.unwrap();
    until(&mut a, is_text).await;
    a.send(SessionCommand::Steer {
        text: "actually do Y".into(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    let seen = cancel(&mut a).await;

    assert!(
        seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::Dequeued { text } if text == "actually do Y"
        )),
        "the undelivered steer is dequeued"
    );
    let conv = last_conversation(&seen);
    assert!(conv.queued_message.is_none());
    let texts: Vec<String> = conv.api_messages.iter().map(|m| text_of(m)).collect();
    assert!(
        !texts.iter().any(|t| t.contains("actually do Y")),
        "a dequeued steer never enters history: {texts:?}"
    );
    assert_eq!(texts.last().unwrap(), InterruptReason::User.marker());
    end(&mut a).await;
}

/// The old recap dropped every completed tool round of the aborted turn.
/// Now the round is kept verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_after_a_tool_round_keeps_the_round() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([sse_read_missing(), SSE_PREFIX]));
    let (url, hits) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("look at the file")).await.unwrap();
    until(&mut a, is_text).await; // second round is streaming
    let seen = cancel(&mut a).await;

    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let msgs = last_conversation(&seen).api_messages;
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "assistant", "user"],
        "{msgs:#?}"
    );
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[3]["content"], json!([{"type": "text", "text": "hi"}]));
    assert_eq!(msgs[4]["content"], InterruptReason::User.marker());
    assert_tools_paired(&msgs);
    assert_no_recap(&msgs);
    end(&mut a).await;
}

/// Prompt caching: the first request after an abort re-sends, byte for byte
/// (modulo the single moving cache marker), everything the cancelled
/// request sent — tools, system, and its whole message list — and only
/// appends after it. (The old path restarted from the turn's first user
/// message, so the tool round fell out of the shared prefix.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn next_request_after_cancel_extends_the_cancelled_requests_prefix() {
    let _h = Home::new();
    let bodies: &'static [&'static str] =
        Box::leak(Box::new([sse_read_missing(), SSE_PREFIX, SSE_HI]));
    // Hits: 0 = tool round, 1 = cancelled round (held open), 2 = next turn.
    let (url, hits, seen_bodies) = capture_stub(bodies, Some(1)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("look at the file")).await.unwrap();
    until(&mut a, is_text).await;
    cancel(&mut a).await;
    a.send(submit("actually, do Y")).await.unwrap();
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
        )
    })
    .await;
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let reqs = seen_bodies.lock().unwrap().clone();
    let (cancelled, next) = (&reqs[1], &reqs[2]);
    assert_eq!(
        next["system"], cancelled["system"],
        "system prefix identical"
    );
    assert_eq!(next["tools"], cancelled["tools"], "tools prefix identical");

    let before = cancelled["messages"].as_array().unwrap();
    let after = next["messages"].as_array().unwrap();
    assert!(after.len() > before.len());
    for (i, (b, n)) in before.iter().zip(after).enumerate() {
        assert_eq!(
            strip_cache_control(b),
            strip_cache_control(n),
            "message {i} changed between the cancelled request and the next one"
        );
    }
    // Appended: the partial reply, then marker + new prompt (the Anthropic
    // path merges adjacent user turns into one message).
    let tail = &after[before.len()..];
    assert_eq!(tail[0]["role"], "assistant");
    assert_eq!(
        strip_cache_control(&tail[0]["content"]),
        json!([{"type": "text", "text": "hi"}])
    );
    let last = strip_cache_control(tail.last().unwrap());
    assert_eq!(last["role"], "user");
    assert_eq!(
        last["content"],
        json!([
            {"type": "text", "text": InterruptReason::User.marker()},
            {"type": "text", "text": "actually, do Y"}
        ])
    );
    assert!(!serde_json::to_string(next)
        .unwrap()
        .contains("ABORT CONTEXT"));
    end(&mut a).await;
}

/// The cost cap cancels from INSIDE the stream handler. The drain must not
/// re-check the cap (no recursive cancel) and records its own marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cost_cap_interrupts_once_with_its_own_marker() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([sse_read_missing(), SSE_PREFIX]));
    let (url, _) = stub_seq_endless_last(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            max_session_cost: Some(1e-9),
            ..cfg()
        })
        .await
        .unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("spend")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    // Anything still in flight after the first Idle.
    let late = drain_for(&mut a, Duration::from_millis(1500)).await;
    let all: Vec<_> = seen.iter().chain(late.iter()).cloned().collect();

    let caps = all
        .iter()
        .filter(|e| matches!(e.event, SessionEventWire::CostCapReached { .. }))
        .count();
    assert_eq!(caps, 1, "exactly one cost-cap breach");
    assert_eq!(
        aborted(&all).len(),
        1,
        "exactly one Aborted (no recursive cancel)"
    );
    let msgs = last_conversation(&all).api_messages;
    assert_eq!(
        msgs.last().unwrap()["content"],
        InterruptReason::CostCap.marker(),
        "{msgs:#?}"
    );
    assert_eq!(
        msgs[1]["content"][0]["type"], "tool_use",
        "the round survives"
    );
    assert_tools_paired(&msgs);
    assert_no_recap(&msgs);
    end(&mut a).await;
}

/// A tool blocked on a host prompt: cancel answers it `None`, the turn
/// unwinds, the prompt is resolved for clients, history stays paired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pending_prompt_is_resolved_and_the_turn_unwinds() {
    let _h = Home::new();
    let bodies: &'static [&'static str] = Box::leak(Box::new([SSE_PROMPT_TOOL_USE, SSE_HI]));
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("needs a secret")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let id = prompt_id(seen.last().unwrap()).unwrap();
    let started = std::time::Instant::now();
    let seen = cancel(&mut a).await;

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel must not hang on the prompt ({:?})",
        started.elapsed()
    );
    assert!(seen.iter().any(|e| matches!(
        e.event,
        SessionEventWire::PromptResolved { prompt_id } if prompt_id == id
    )));
    assert_eq!(aborted(&seen).len(), 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "no follow-up request after cancel"
    );
    let conv = last_conversation(&seen);
    assert_tools_paired(&conv.api_messages);
    assert_eq!(tool_results(&conv).len(), 1);
    assert_eq!(
        conv.api_messages.last().unwrap()["content"],
        InterruptReason::User.marker()
    );
    end(&mut a).await;
}

/// A tool canceled mid-execution after streaming output: the partial output
/// is kept but labelled — it must never read as a completed result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn tool_canceled_mid_output_is_labelled_partial() {
    let _h = Home::new();
    let bash = sse_tool(
        "toolu_bash",
        "bash",
        r#"{"command":"printf 'partial-out\\n'; sleep 30"}"#,
    );
    let bodies: &'static [&'static str] = Box::leak(Box::new([bash, SSE_HI]));
    let (url, _) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("run it")).await.unwrap();
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, .. }))
                if delta.contains("partial-out")
        )
    })
    .await;
    let seen = cancel(&mut a).await;

    let conv = last_conversation(&seen);
    let results = tool_results(&conv);
    assert_eq!(results.len(), 1, "{:#?}", conv.api_messages);
    let r = &results[0];
    assert!(r.starts_with("partial-out"), "partial output kept: {r}");
    assert!(
        r.contains("Canceled by user before the tool finished; the output above is partial."),
        "labelled partial: {r}"
    );
    assert_eq!(
        conv.api_messages.last().unwrap()["content"],
        InterruptReason::User.marker()
    );
    end(&mut a).await;
}

/// Steers an event into the running turn, then stalls until cancelled.
struct EventThenStallTool;

#[async_trait::async_trait]
impl agent_engine::Tool for EventThenStallTool {
    fn name(&self) -> &str {
        "event_then_stall"
    }
    fn description(&self) -> &str {
        "pushes an event then waits"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        let queue = ctx.capabilities.event_queue.expect("runtime event queue");
        queue
            .push(agent_engine::events::Event::simple(
                "watcher",
                "build finished: 3 warnings",
                None,
            ))
            .expect("push");
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok("unreachable".into())
    }
}

/// An event steered into the stream but never drained by the engine (it
/// only drains at round boundaries) used to die with the steering channel on
/// cancel. It now lands in history right after the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn steered_event_the_engine_never_drained_is_kept_after_the_marker() {
    let _h = Home::new();
    let tool = sse_tool("toolu_evt", "event_then_stall", "{}");
    let bodies: &'static [&'static str] = Box::leak(Box::new([tool, SSE_HI]));
    let (url, _) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(EventThenStallTool));
    let handle = host
        .create_session(SessionConfig {
            // No auto-turn after the cancel: we only inspect history.
            ..cfg()
        })
        .await
        .unwrap();
    let mut a = attach(&handle).await;

    a.send(submit("wait for the build")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::External(_))).await;
    let seen = cancel(&mut a).await;

    let msgs = last_conversation(&seen).api_messages;
    let n = msgs.len();
    assert!(n >= 5, "{msgs:#?}");
    assert!(is_interruption_marker(&text_of(&msgs[n - 2])), "{msgs:#?}");
    let event = text_of(&msgs[n - 1]);
    assert!(is_event_payload(&event), "{event}");
    assert!(event.contains("build finished: 3 warnings"), "{event}");
    assert_tools_paired(&msgs);
    end(&mut a).await;
}

/// A session saved by an older Synaps with an `abort_context` recap is
/// migrated on `--continue`: recap dropped, marker appended, and the next
/// request carries no recap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn legacy_abort_context_session_is_migrated_on_continue() {
    let _h = Home::new();
    let (url, _, seen_bodies) = capture_stub(Box::leak(Box::new([SSE_HI])), None).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut legacy = agent_engine::core::session::Session::new(MODEL, "low", None);
    legacy.api_messages = vec![Arc::new(json!({"role": "user", "content": "do X"}))];
    legacy.abort_context = Some(
        "(System note — ABORT CONTEXT: your previous response was interrupted …)\n\
         - you had started writing: Let me"
            .into(),
    );
    legacy.save().await.unwrap();

    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            continue_session: Some(Some(legacy.id.clone())),
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let msgs = &snap.conversation.api_messages;
    assert_eq!(msgs.len(), 2, "{msgs:#?}");
    assert_eq!(msgs[1]["content"], InterruptReason::Unknown.marker());
    assert!(snap.conversation.abort_context.is_none());

    a.send(submit("carry on")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let body = seen_bodies.lock().unwrap().last().cloned().unwrap();
    let wire = serde_json::to_string(&body).unwrap();
    assert!(!wire.contains("ABORT CONTEXT"), "{wire}");
    assert!(wire.contains(InterruptReason::Unknown.marker()), "{wire}");

    // Saved back without the legacy field.
    let saved = agent_engine::core::session::Session::load(&legacy.id).unwrap();
    assert!(saved.abort_context.is_none());
    end(&mut a).await;
}
