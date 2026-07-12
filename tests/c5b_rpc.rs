//! C5b RED → GREEN: RPC mode forwards runtime events without auto-turn.
//!
//! Tests:
//!  1. drain_and_build_rpc_events — verifies that event_payload_from_drained
//!     produces correct RpcEvent::Event values from a queue drain.
//!  2. rpc_event_buffered_at_idle — verify event queue items produce Event frames.
//!  3. rpc_event_frame_never_auto_turns — forwarding only; no prompt logic.
//!
//! No live API calls. All tests exercise the drain+payload+frame pipeline only.

use agent_engine::engine::reactor::{drain_event_queue, event_payload_from_drained, EventDisposition};
use agent_engine::events::{types::{Event, Severity}, EventQueue};
use synaps_cli::core::rpc_protocol::RpcEvent;

/// Build a queue with a set of events.
fn make_queue(items: &[(&str, &str, Option<Severity>)]) -> EventQueue {
    let q = EventQueue::new(64);
    for (source, text, sev) in items {
        let mut ev = Event::simple(source, text, sev.clone());
        ev.content.content_type = "message".into();
        q.push(ev).unwrap();
    }
    q
}

// ─── T1: drain + build RpcEvent::Event from idle queue ───────────────────────

#[test]
fn drain_idle_queue_produces_rpc_event_frames() {
    let q = make_queue(&[
        ("discord", "hello from discord", Some(Severity::High)),
        ("uptime-kuma", "service down", Some(Severity::Critical)),
    ]);

    let mut messages = Vec::new();
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    assert_eq!(drained.len(), 2);

    // Build RpcEvent::Event frames from the drained batch.
    let frames: Vec<RpcEvent> = drained
        .iter()
        .map(|d| RpcEvent::Event { payload: event_payload_from_drained(d) })
        .collect();

    assert_eq!(frames.len(), 2);

    // Critical was inserted second but should drain first (priority queue).
    match &frames[0] {
        RpcEvent::Event { payload } => {
            assert_eq!(payload.source, "uptime-kuma");
            assert_eq!(payload.severity, "critical");
            assert_eq!(payload.text, "service down");
        }
        _ => panic!("expected Event frame"),
    }

    match &frames[1] {
        RpcEvent::Event { payload } => {
            assert_eq!(payload.source, "discord");
            assert_eq!(payload.severity, "high");
        }
        _ => panic!("expected Event frame"),
    }
}

// ─── T2: frames serialise to valid wire JSON ──────────────────────────────────

#[test]
fn rpc_event_event_frames_serialise_to_wire_json() {
    let q = make_queue(&[("test-src", "ping", Some(Severity::Medium))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    let frame = RpcEvent::Event { payload: event_payload_from_drained(&drained[0]) };

    let json = serde_json::to_string(&frame).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(val["type"], "event");
    assert_eq!(val["payload"]["source"], "test-src");
    assert_eq!(val["payload"]["severity"], "medium");
    assert_eq!(val["payload"]["text"], "ping");
    assert!(val["payload"]["timestamp"].as_str().unwrap().len() > 0);
    assert!(val["payload"]["formatted"].as_str().unwrap().starts_with("<event "));
}

// ─── T3: disposition is Injected when idle, Buffered when busy ────────────────

#[test]
fn idle_events_injected_busy_events_buffered() {
    // Idle drain
    let q = make_queue(&[("src", "idle-ev", Some(Severity::Low))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();
    let d = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    assert_eq!(d[0].disposition, EventDisposition::Injected);

    // Busy drain (no steer tx)
    let q = make_queue(&[("src", "busy-ev", Some(Severity::Low))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();
    let d = drain_event_queue(&q, &mut messages, &mut pending, true, None);
    assert_eq!(d[0].disposition, EventDisposition::Buffered);
}

// ─── T4: no auto-turn spawned (WakeAction != RunTurn) for RPC policy ─────────

#[test]
fn rpc_wake_action_never_run_turn_because_auto_turn_disabled() {
    use agent_engine::engine::reactor::wake_action;

    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // RPC mode: auto_turn_enabled = false
    let action = wake_action(&drained, &messages, false, false, 0);
    // Must NOT be RunTurn — RPC never auto-turns.
    assert_ne!(action, agent_engine::engine::reactor::WakeAction::RunTurn,
        "RPC mode must not auto-turn on event drain");
}
