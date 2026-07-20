//! C5c: server internalizes runtime events and policy-gates owning-session auto-turn.
//!
//! Tests:
//!  1. events_auto_turn_default_true — EventsConfig default
//!  2. explicit false/0/no opt out
//!  3. server drain injects canonical content without a raw event frame
//!  4. wake policy respects enabled/disabled configuration

use agent_engine::engine::reactor::{drain_event_queue, wake_action, WakeAction};
use agent_engine::events::{
    types::{Event, Severity},
    EventQueue,
};
use synaps_cli::core::config::{load_config_from_str, EventsConfig};

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
fn events_auto_turn_default_true() {
    assert!(
        EventsConfig::default().auto_turn,
        "default auto_turn must be true; opt-out via events.auto_turn = false"
    );
}

// ─── 2. EventsConfig parses ───────────────────────────────────────────────────

#[test]
fn events_auto_turn_explicit_true_parses() {
    let cfg = load_config_from_str("events.auto_turn = true\n");
    assert!(cfg.events.auto_turn);
}

#[test]
fn events_auto_turn_opt_out_false_parses() {
    let cfg = load_config_from_str("events.auto_turn = false\n");
    assert!(!cfg.events.auto_turn);
}

#[test]
fn events_auto_turn_opt_out_zero_parses() {
    let cfg = load_config_from_str("events.auto_turn = 0\n");
    assert!(!cfg.events.auto_turn);
}

#[test]
fn events_auto_turn_opt_out_no_parses() {
    let cfg = load_config_from_str("events.auto_turn = no\n");
    assert!(!cfg.events.auto_turn);
}

// ─── 3. Server internalizes canonical formatted content ─────────────────────

#[test]
fn drain_for_server_injects_canonical_event_without_raw_frame() {
    let q = make_queue(&[("grafana", "CPU spike", Some(Severity::High))]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();
    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    assert_eq!(drained.len(), 1);
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]["content"].as_str(),
        Some(drained[0].formatted.as_str())
    );
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
    assert_ne!(
        action,
        WakeAction::RunTurn,
        "server must not auto-turn when events.auto_turn = false"
    );
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
    assert_eq!(
        action,
        WakeAction::RunTurn,
        "server should RunTurn when events.auto_turn = true and conditions met"
    );
}
