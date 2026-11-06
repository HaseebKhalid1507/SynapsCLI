//! C5b: RPC mode forwards runtime events and auto-turns when config enabled.
//!
//! Policy under test:
//!  * Drainer always builds `RpcEvent::Event` frames from drained events.
//!  * `events_auto_turn = true`  (default) → `WakeAction::RunTurn` when idle,
//!    injected events present, last message is user, counter < cap.
//!  * `events_auto_turn = false` → `WakeAction::Forward` only; no auto-turn.
//!  * `claim_auto_turn` coalesces: one reservation per batch.
//!  * Real Prompt/FollowUp resets `consecutive_auto_turns` counter to 0.
//!
//! No live API calls. All tests exercise the drain+payload+frame pipeline,
//! `wake_action`, and `claim_auto_turn` seams only. (`synaps rpc` is a daemon
//! client now: the session actor runs these seams; the in-process loop's
//! own seams — `terminal_flush_seam`, `spawn_prompt_registration_check`, the
//! start barrier — went with it.)

use agent_engine::engine::reactor::{
    claim_auto_turn, drain_event_queue, event_payload_from_drained,
    wake_action, EventDisposition, WakeAction, AUTO_TURN_CAP,
};
use agent_engine::events::{
    types::{Event, Severity},
    EventQueue,
};
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

fn user_msg(text: &str) -> synaps_cli::SharedMessage {
    std::sync::Arc::new(serde_json::json!({"role": "user", "content": text}))
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
        .map(|d| RpcEvent::Event {
            payload: Box::new(event_payload_from_drained(d)),
        })
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
    let frame = RpcEvent::Event {
        payload: Box::new(event_payload_from_drained(&drained[0])),
    };

    let json = serde_json::to_string(&frame).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(val["type"], "event");
    assert_eq!(val["payload"]["source"], "test-src");
    assert_eq!(val["payload"]["severity"], "medium");
    assert_eq!(val["payload"]["text"], "ping");
    assert!(!val["payload"]["timestamp"].as_str().unwrap().is_empty());
    assert!(val["payload"]["formatted"]
        .as_str()
        .unwrap()
        .starts_with("<event "));
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

// ─── T4: auto-turn fires when config=true, opt-out when config=false ──────────

#[test]
fn wake_action_run_turn_when_auto_turn_enabled() {
    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = vec![user_msg("prior user msg")];
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // events_auto_turn = true → RunTurn expected when counter < cap
    let action = wake_action(&drained, &messages, false, true, 0);
    assert_eq!(
        action,
        WakeAction::RunTurn,
        "auto-turn enabled: drainer should produce RunTurn on idle inject"
    );
}

#[test]
fn wake_action_forward_when_auto_turn_disabled() {
    use agent_engine::engine::reactor::wake_action;

    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = vec![user_msg("prior")];
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // events_auto_turn = false → must NOT auto-turn
    let action = wake_action(&drained, &messages, false, false, 0);
    assert_ne!(
        action,
        WakeAction::RunTurn,
        "RPC opt-out: auto_turn_enabled=false must not produce RunTurn"
    );
}

// ─── T6: claim_auto_turn coalescing — one reservation per batch ───────────────

#[test]
fn claim_auto_turn_coalesces_one_per_batch() {
    let mut counter: u32 = 0;
    // First claim in a batch → allowed
    assert!(claim_auto_turn(&mut counter));
    // Counter incremented
    assert_eq!(counter, 1);
    // A second claim in the "same batch" would also be allowed while counter < cap,
    // but the drainer only calls it once per drained batch (coalescing contract).
    // Verify the cap boundary is still respected:
    let mut counter: u32 = AUTO_TURN_CAP;
    assert!(!claim_auto_turn(&mut counter), "at cap — must deny");
    assert_eq!(
        counter, AUTO_TURN_CAP,
        "counter must not change when denied"
    );
}

// ─── T9: RPC event frame is exactly one frame per event ──────────────────────

#[test]
fn rpc_event_frame_is_exactly_one_per_drained_event() {
    let q = make_queue(&[
        ("src-a", "alpha", Some(Severity::Low)),
        ("src-b", "beta", Some(Severity::High)),
        ("src-c", "gamma", Some(Severity::Medium)),
    ]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    let frames: Vec<RpcEvent> = drained
        .iter()
        .map(|d| RpcEvent::Event {
            payload: Box::new(event_payload_from_drained(d)),
        })
        .collect();

    // Exactly one frame per drained event — no duplicates, no drops.
    assert_eq!(frames.len(), drained.len());
    assert_eq!(frames.len(), 3);
}
