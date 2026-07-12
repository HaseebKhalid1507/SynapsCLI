//! C5c RED → GREEN: server broadcasts runtime events, policy-gated auto-turn.
//!
//! Tests:
//!  1. events_auto_turn_default_false — EventsConfig default
//!  2. events_auto_turn_parses_true — config key parsing
//!  3. server_message_event_broadcast_shape — ServerMessage::Event serialises
//!  4. drain_for_server_builds_server_message_event — integration of drain +
//!     event_payload_from_drained → ServerMessage::Event
//!  5. server_event_broadcast_always_no_auto_turn — WakeAction::Forward when
//!     auto_turn disabled (default)
//!  6. server_event_broadcast_wakeaction_run_turn_when_enabled — WakeAction::RunTurn
//!     when auto_turn = true, idle, last message is user

use agent_engine::engine::reactor::{
    drain_event_queue, event_payload_from_drained, wake_action, WakeAction,
};
use agent_engine::events::{types::{Event, Severity}, EventQueue};
use synaps_cli::core::config::{EventsConfig, load_config_from_str};
use synaps_cli::protocol::ServerMessage;

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

// ─── 1. EventsConfig default ──────────────────────────────────────────────────

#[test]
fn events_auto_turn_default_false() {
    assert!(!EventsConfig::default().auto_turn);
}

// ─── 2. EventsConfig parses ───────────────────────────────────────────────────

#[test]
fn events_auto_turn_parses_true() {
    let cfg = load_config_from_str("events.auto_turn = true\n");
    assert!(cfg.events.auto_turn);
}

// ─── 3. ServerMessage::Event serialises correctly ────────────────────────────

#[test]
fn server_message_event_broadcast_shape() {
    let msg = ServerMessage::Event {
        payload: synaps_cli::core::rpc_protocol::EventPayload {
            id: "sv-1".into(),
            source: "monitor".into(),
            severity: "high".into(),
            content_type: "alert".into(),
            text: "disk 90% full".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            formatted: "<event>disk 90% full</event>".into(),
        },
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(val["type"], "event");
    assert_eq!(val["payload"]["source"], "monitor");
    assert_eq!(val["payload"]["severity"], "high");
    assert_eq!(val["payload"]["text"], "disk 90% full");
}

// ─── 4. Drain → ServerMessage::Event ─────────────────────────────────────────

#[test]
fn drain_for_server_builds_server_message_event() {
    let q = make_queue(&[("grafana", "CPU spike", Some(Severity::High))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();
    // Server: idle, no streaming
    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    assert_eq!(drained.len(), 1);

    let server_msg = ServerMessage::Event {
        payload: event_payload_from_drained(&drained[0]),
    };

    match &server_msg {
        ServerMessage::Event { payload } => {
            assert_eq!(payload.source, "grafana");
            assert_eq!(payload.text, "CPU spike");
            assert_eq!(payload.severity, "high");
        }
        _ => panic!("expected Event variant"),
    }
}

// ─── 5. No auto-turn when events.auto_turn = false (default) ─────────────────

#[test]
fn server_event_broadcast_no_auto_turn_when_disabled() {
    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = vec![user_msg("initial")];
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // auto_turn = false → WakeAction must NOT be RunTurn
    let action = wake_action(&drained, &messages, false, false, 0);
    assert_ne!(action, WakeAction::RunTurn,
        "server must not auto-turn when events.auto_turn = false");
}

// ─── 6. Auto-turn fires when explicitly enabled ────────────────────────────────

#[test]
fn server_event_broadcast_auto_turn_when_enabled() {
    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = vec![user_msg("initial")];
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // Injected events expand messages; last element is now the injected event.
    // auto_turn = true, not busy, consecutive_auto_turns = 0 → RunTurn
    let action = wake_action(&drained, &messages, false, true, 0);
    assert_eq!(action, WakeAction::RunTurn,
        "server should RunTurn when events.auto_turn = true and conditions met");
}
