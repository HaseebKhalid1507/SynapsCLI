//! Spawned-child events on a session: `tool_child` after the call's
//! `tool_use` and before its output, `tool_child_ended` with the exit code or
//! the kill signal before its result, the same records in the ledger, and a
//! worker's children on the parent session with the worker's handle id.
//!
//! Loopback Anthropic SSE stub; production runtime / actor / bash tool.
#![cfg(target_os = "linux")]

mod session_actor_common;
use session_actor_common::*;

use agent_engine::session::child_events::ChildProcessEvent;
use agent_engine::session::children::{self, ChildState};
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, SessionCommand, SessionConfig, SessionEventWire,
    SessionLifecycle,
};
use agent_engine::{LlmEvent, StreamEvent};
use serde_json::{json, Value};
use serial_test::serial;

fn sse(events: Vec<Value>) -> &'static str {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(&event.to_string());
        out.push_str("\n\n");
    }
    Box::leak(out.into_boxed_str())
}

/// One assistant response calling `calls` (id, tool, input) in order.
fn tool_round(msg_id: &str, calls: &[(&str, &str, Value)]) -> &'static str {
    let usage = json!({"input_tokens": 10, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": msg_id, "type": "message", "role": "assistant", "content": [], "model": MODEL,
        "stop_reason": null, "stop_sequence": null, "usage": usage}})];
    for (index, (id, name, input)) in calls.iter().enumerate() {
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name}}));
        events.push(json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
        "usage": usage}));
    events.push(json!({"type": "message_stop"}));
    sse(events)
}

fn position(seen: &[Envelope], pred: impl Fn(&SessionEventWire) -> bool) -> Vec<usize> {
    seen.iter().enumerate().filter(|(_, env)| pred(&env.event)).map(|(i, _)| i).collect()
}

fn is_tool_use(event: &SessionEventWire, id: &str) -> bool {
    matches!(event, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolUse { tool_id, .. })) if tool_id == id)
}

fn is_output(event: &SessionEventWire, id: &str) -> bool {
    matches!(event, SessionEventWire::Stream(StreamEvent::Llm(
        LlmEvent::ToolResultDelta { tool_id, .. } | LlmEvent::ToolResult { tool_id, .. })) if tool_id == id)
}

fn is_result(event: &SessionEventWire, id: &str) -> bool {
    matches!(event, SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult { tool_id, .. })) if tool_id == id)
}

fn child_event(event: &SessionEventWire) -> Option<&ChildProcessEvent> {
    match event {
        SessionEventWire::ChildProcess(child) => Some(child),
        _ => None,
    }
}

/// Exactly one spawn and one end for `id`, in order around its output;
/// returns (spawned, ended) envelopes' child events.
fn one_report(seen: &[Envelope], id: &str, foreground: bool) -> (ChildProcessEvent, ChildProcessEvent) {
    let spawns = position(seen, |e| matches!(child_event(e), Some(ChildProcessEvent::Spawned(s)) if s.tool_id == id));
    let ends = position(seen, |e| matches!(child_event(e), Some(ChildProcessEvent::Ended { tool_id, .. }) if tool_id == id));
    assert_eq!((spawns.len(), ends.len()), (1, 1), "{id}: one tool_child and one tool_child_ended");
    let (spawn_at, end_at) = (spawns[0], ends[0]);
    assert!(spawn_at < end_at, "{id}: spawned before ended");
    if foreground {
        let uses = position(seen, |e| is_tool_use(e, id));
        let outputs = position(seen, |e| is_output(e, id));
        let results = position(seen, |e| is_result(e, id));
        assert_eq!(uses.len(), 1, "{id}: one tool_use");
        assert!(uses[0] < spawn_at, "{id}: tool_child after its tool_use");
        assert!(spawn_at < outputs[0], "{id}: tool_child before the call's first output");
        assert!(end_at < results[0], "{id}: tool_child_ended before its tool_result");
    }
    (
        child_event(&seen[spawn_at].event).unwrap().clone(),
        child_event(&seen[end_at].event).unwrap().clone(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn bash_children_are_reported_after_their_tool_use_and_before_their_results() {
    let _h = Home::new();
    if !children::supported() {
        return;
    }
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        tool_round("msg_c1", &[("toolu_c1", "bash", json!({"command": "echo hi"}))]),
        tool_round("msg_c2", &[("toolu_c2", "bash", json!({"command": "exit 3"}))]),
        tool_round("msg_c3", &[("toolu_c3", "bash", json!({"command": "sleep 30", "timeout": 1}))]),
        tool_round(
            "msg_c4",
            &[("toolu_c4a", "bash", json!({"command": "echo a"})), ("toolu_c4b", "bash", json!({"command": "echo b"}))],
        ),
        SSE_HI,
    ]));
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test)).await.unwrap();
    t.send(submit("run them")).await.unwrap();
    // Attach a second client while `toolu_c3` runs: its replay has the
    // call's tool_use but no child events (never replayed).
    let mut seen = until(&mut t, |e| {
        matches!(child_event(e), Some(ChildProcessEvent::Spawned(s)) if s.tool_id == "toolu_c3")
    })
    .await;
    let (_late, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach)).await.unwrap();
    assert!(snap.replay.iter().any(|e| is_tool_use(&e.event, "toolu_c3")), "mid-turn replay");
    assert!(snap.replay.iter().all(|e| child_event(&e.event).is_none()), "never replayed on attach");
    seen.extend(until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 5);

    let expected = [
        ("toolu_c1", Some(0), None),
        ("toolu_c2", Some(3), None),
        ("toolu_c3", None, Some(9)),
        ("toolu_c4a", Some(0), None),
        ("toolu_c4b", Some(0), None),
    ];
    let listing = children::list(Some(handle.id.as_str()), None);
    assert_eq!(listing.sessions.len(), 1);
    let records = &listing.sessions[0].children;
    for (id, code, sig) in expected {
        let (spawned, ended) = one_report(&seen, id, true);
        let ChildProcessEvent::Spawned(spawn) = spawned else { unreachable!() };
        assert_eq!((spawn.tool_name.as_str(), spawn.handle_id.as_deref(), spawn.shell_session_id.as_deref()), ("bash", None, None));
        let child = &spawn.child;
        assert_eq!((child.sid, child.pgid), (child.pid, child.pid), "{id}: its own session and group");
        assert_eq!(Some(child.boot_id.as_str()), children::boot_id());
        let ChildProcessEvent::Ended { child: ended_child, exit_code, signal, handle_id, .. } = ended else { unreachable!() };
        assert_eq!((&ended_child, exit_code, signal, handle_id), (child, code, sig, None), "{id}");
        let record = records.iter().find(|r| r.tool_id == id).unwrap_or_else(|| panic!("{id} in the ledger"));
        assert_eq!((&record.child, record.state, record.exit_code, record.signal), (child, ChildState::Ended, code, sig));
    }
    end(&mut t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_worker_bash_call_is_reported_on_the_parent_with_its_handle() {
    let _h = Home::new();
    if !children::supported() {
        return;
    }
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        tool_round(
            "msg_p1",
            &[("toolu_p1", "subagent", json!({"task": "run it", "system_prompt": "You run one command."}))],
        ),
        tool_round("msg_w1", &[("toolu_w1", "bash", json!({"command": "exit 4"}))]),
        SSE_HI, // the worker's answer
        SSE_HI, // the parent's answer
    ]));
    let (url, hits) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test)).await.unwrap();
    t.send(submit("delegate")).await.unwrap();
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4, "parent, worker x2, parent");

    let (spawned, ended) = one_report(&seen, "toolu_w1", false);
    let ChildProcessEvent::Spawned(spawn) = spawned else { unreachable!() };
    let worker = spawn.handle_id.clone().expect("a worker call carries its handle id");
    assert!(worker.starts_with("sa_"), "{worker}");
    let ChildProcessEvent::Ended { exit_code, handle_id, .. } = ended else { unreachable!() };
    assert_eq!((exit_code, handle_id.as_deref()), (Some(4), Some(worker.as_str())));
    // On the parent stream while the parent's call runs: before its result.
    let spawn_at = position(&seen, |e| matches!(child_event(e), Some(ChildProcessEvent::Spawned(s)) if s.tool_id == "toolu_w1"))[0];
    assert!(spawn_at < position(&seen, |e| is_result(e, "toolu_p1"))[0]);
    let listing = children::list(Some(handle.id.as_str()), None);
    let record = listing.sessions[0].children.iter().find(|r| r.tool_id == "toolu_w1").expect("under the parent session");
    assert_eq!(record.handle_id.as_deref(), Some(worker.as_str()));
    end(&mut t).await;
}

/// A parked session's rebuilt runtime reports to the same route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unparked_session_still_reports_its_children() {
    let _h = Home::new();
    if !children::supported() {
        return;
    }
    std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
    let bodies: &'static [&'static str] = Box::leak(Box::new([
        SSE_HI,
        tool_round("msg_u1", &[("toolu_u1", "bash", json!({"command": "exit 5"}))]),
        SSE_HI,
    ]));
    let (url, _) = stub_seq(bodies).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(SessionConfig { persist: true, ..cfg() }).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    a.send(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "grace 0 parks the idle, detached session");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    b.send(submit("run")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    let (_, ended) = one_report(&seen, "toolu_u1", true);
    assert!(matches!(ended, ChildProcessEvent::Ended { exit_code: Some(5), .. }), "{ended:?}");
    end(&mut b).await;
}
