//! P9 C2 — `synaps send` remote-steering contract.
//!
//! `synaps send` delivers an `Event` onto the target session's per-session
//! event socket, which the runtime pushes into `event_queue`. It does NOT
//! speak `SessionCommand` and never inspects driver state — Submit-vs-Steer
//! routing is owned server-side by the actor's event-queue wake path
//! (`on_queue_wake` → `drain_event_queue` + `wake_action`).
//!
//! These tests pin the routing contract that makes a `send` into remote
//! steering of a *busy / driver-owned* session and keeps it safe against a
//! driver-armed idle session:
//!
//! 1. busy + live steer channel  → event is **Steered** into the stream
//!    (this IS "route to Steer while a driver turn is running").
//! 2. busy + no steer channel    → **Buffered** (replayed at turn end).
//! 3. idle                       → **Injected** as a `role=user` message;
//!    whether that injection wakes a turn is `wake_action`'s call, and the
//!    actor additionally inhibits it while a driver is armed (covered by
//!    `session_actor_driver::submit_while_armed_idle_queues_steering`).

use agent_engine::engine::reactor::{
    drain_event_queue, wake_action_with_cap, EventDisposition, WakeAction,
};
use agent_engine::events::{
    types::{Event, Severity},
    EventQueue,
};
use synaps_cli::SharedMessage;
use tokio::sync::mpsc::unbounded_channel;

fn make_event(text: &str) -> Event {
    let mut ev = Event::simple("synaps-send", text, Some(Severity::High));
    ev.content.content_type = "message".into();
    ev
}

/// C2: a `send` that lands while a (driver-owned) turn is streaming is
/// Steered into the live stream — the server-side equivalent of routing to
/// `Steer` when the session is driver-armed and busy.
#[test]
fn send_while_busy_steers_into_the_live_stream() {
    let q = EventQueue::new(16);
    q.push(make_event("nudge: prefer the smaller diff")).unwrap();

    let (steer_tx, mut steer_rx) = unbounded_channel::<String>();
    let mut msgs: Vec<SharedMessage> = Vec::new();
    let mut pending: Vec<String> = Vec::new();

    let drained = drain_event_queue(&q, &mut msgs, &mut pending, true, Some(&steer_tx));

    assert_eq!(drained.len(), 1);
    assert_eq!(
        drained[0].disposition,
        EventDisposition::Steered,
        "a send during a live (driver-owned) turn must be steered"
    );
    // The steer channel actually carries the formatted event.
    let steered = steer_rx.try_recv().expect("steer channel got the event");
    assert!(steered.contains("nudge: prefer the smaller diff"));
    assert!(msgs.is_empty(), "steered events do not inject a user message");
    assert!(pending.is_empty(), "steered events are not buffered");
}

/// C2: busy but no live steer channel → buffered for turn-end replay, never
/// dropped.
#[test]
fn send_while_busy_without_steer_channel_buffers() {
    let q = EventQueue::new(16);
    q.push(make_event("late note")).unwrap();

    let mut msgs: Vec<SharedMessage> = Vec::new();
    let mut pending: Vec<String> = Vec::new();

    let drained = drain_event_queue(&q, &mut msgs, &mut pending, true, None);

    assert_eq!(drained[0].disposition, EventDisposition::Buffered);
    assert_eq!(pending.len(), 1);
    assert!(pending[0].contains("late note"));
}

/// C2: idle → injected as a user message. On an *unarmed* session this then
/// wakes an auto-turn (historical `Submit` behavior); on a driver-armed
/// session the actor inhibits that turn (driver owns scheduling).
#[test]
fn send_while_idle_injects_and_would_wake_unarmed() {
    let q = EventQueue::new(16);
    q.push(make_event("kick off the task")).unwrap();

    let mut msgs: Vec<SharedMessage> = Vec::new();
    let mut pending: Vec<String> = Vec::new();

    let drained = drain_event_queue(&q, &mut msgs, &mut pending, false, None);
    assert_eq!(drained[0].disposition, EventDisposition::Injected);
    assert_eq!(msgs.len(), 1, "idle send injects a role=user message");

    // Unarmed + idle + auto-turn on + last msg is user → RunTurn (Submit-like).
    let action = wake_action_with_cap(&drained, &msgs, false, true, 0, 0);
    assert_eq!(
        action,
        WakeAction::RunTurn,
        "an idle send on an unarmed session behaves like Submit (wakes a turn)"
    );
}
