//! Central event-drain and wake-action policy — mode-agnostic engine layer.
//!
//! `drain_event_queue` pops every pending event from the queue in priority
//! order, formats it, and classifies it as Steered / Buffered / Injected.
//! `wake_action` decides what the caller should do next.

use tokio::sync::mpsc::UnboundedSender;

use crate::events::{EventQueue, format_event_for_agent};
use crate::SharedMessage;

/// Hard cap: how many consecutive auto-triggered model turns can fire before
/// the engine parks and waits for real user input.
pub const AUTO_TURN_CAP: u32 = 5;

// ── Types ────────────────────────────────────────────────────────────────────

/// What happened to a single event that was drained from the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventDisposition {
    /// Sent via the steering channel into an active stream.
    Steered,
    /// Buffered in `pending_events` (busy but no live steer channel).
    Buffered,
    /// Injected as a `role=user` message into `messages` (idle turn).
    Injected,
}

/// A single event that was drained, together with its formatted payload and
/// how it was handled.
#[derive(Debug, Clone)]
pub struct DrainedEvent {
    pub event: crate::events::types::Event,
    pub formatted: String,
    pub disposition: EventDisposition,
}

/// What the caller's event-loop arm should do after `drain_event_queue` + `wake_action`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeAction {
    /// Spawn a new model turn (all preconditions satisfied).
    RunTurn,
    /// Forward / present the drained events but do NOT spawn a turn
    /// (already streaming, no events, cap reached, etc.).
    Forward,
    /// Nothing was drained — no work to do.
    Nothing,
}

// ── Core functions ────────────────────────────────────────────────────────────

/// Drain every event currently in `queue`.
///
/// For each event:
/// * **busy** (streaming or compact running):
///   - try to steer via `steer_tx`; on success → `Steered`
///   - if no live channel → `Buffered` (pushed to `pending_events`)
/// * **idle**: inject as a `role=user` `SharedMessage` → `Injected`
///
/// Formatting is always done via the existing `format_event_for_agent` so
/// canonical output is produced in exactly one place.
pub fn drain_event_queue(
    queue: &EventQueue,
    messages: &mut Vec<SharedMessage>,
    pending_events: &mut Vec<String>,
    busy: bool,
    steer_tx: Option<&UnboundedSender<String>>,
) -> Vec<DrainedEvent> {
    let mut drained = Vec::new();

    while let Some(event) = queue.pop() {
        let formatted = format_event_for_agent(&event);

        let disposition = if busy {
            // Steer into active stream if the channel is live.
            let steered = steer_tx
                .map(|tx| tx.send(formatted.clone()).is_ok())
                .unwrap_or(false);
            if steered {
                EventDisposition::Steered
            } else {
                pending_events.push(formatted.clone());
                EventDisposition::Buffered
            }
        } else {
            // Idle: inject directly into message history.
            messages.push(std::sync::Arc::new(serde_json::json!({
                "role": "user",
                "content": formatted
            })));
            EventDisposition::Injected
        };

        drained.push(DrainedEvent { event, formatted, disposition });
    }

    drained
}

/// Decide what the caller should do after draining the queue.
///
/// Rules (in order):
/// 1. Nothing drained → `Nothing`.
/// 2. Any `Injected` event + idle + auto-turn enabled + last message is
///    `role=user` + consecutive cap not reached → `RunTurn`.
/// 3. Otherwise → `Forward` (show events; caller may still steer/display).
pub fn wake_action(
    drained: &[DrainedEvent],
    messages: &[SharedMessage],
    busy: bool,
    auto_turn_enabled: bool,
    consecutive_auto_turns: u32,
) -> WakeAction {
    if drained.is_empty() {
        return WakeAction::Nothing;
    }

    let has_injected = drained.iter().any(|d| d.disposition == EventDisposition::Injected);

    if has_injected
        && !busy
        && auto_turn_enabled
        && consecutive_auto_turns < AUTO_TURN_CAP
        && messages.last().map(|m| m["role"].as_str() == Some("user")).unwrap_or(false)
    {
        return WakeAction::RunTurn;
    }

    WakeAction::Forward
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{types::Severity, EventQueue};

    fn make_queue_with(texts: &[(&str, Option<Severity>)]) -> EventQueue {
        let q = EventQueue::new(64);
        for (text, sev) in texts {
            q.push(crate::events::types::Event::simple("test", text, sev.clone())).unwrap();
        }
        q
    }

    fn user_msg(text: &str) -> SharedMessage {
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": text}))
    }

    fn assistant_msg() -> SharedMessage {
        std::sync::Arc::new(serde_json::json!({"role": "assistant", "content": "ok"}))
    }

    // ── drain_event_queue ────────────────────────────────────────────────

    #[test]
    fn idle_single_event_injected() {
        let q = make_queue_with(&[("hello", Some(Severity::Medium))]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].disposition, EventDisposition::Injected);
        assert!(drained[0].formatted.contains("hello"));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert!(messages[0]["content"].as_str().unwrap().contains("hello"));
        assert!(pending.is_empty());
        assert!(q.is_empty());
    }

    #[test]
    fn idle_multiple_events_injected_in_order() {
        let q = make_queue_with(&[
            ("first", Some(Severity::Medium)),
            ("second", Some(Severity::Medium)),
            ("third", Some(Severity::Low)),
        ]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);

        assert_eq!(drained.len(), 3);
        assert!(drained.iter().all(|d| d.disposition == EventDisposition::Injected));
        // Canonical order preserved
        assert!(drained[0].formatted.contains("first"));
        assert!(drained[1].formatted.contains("second"));
        assert!(drained[2].formatted.contains("third"));
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn busy_live_steer_tx_steers() {
        let q = make_queue_with(&[("ev", Some(Severity::Medium))]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, true, Some(&tx));

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].disposition, EventDisposition::Steered);
        assert!(messages.is_empty());
        assert!(pending.is_empty());
        // The formatted payload was actually sent
        let sent = rx.try_recv().expect("message should have been steered");
        assert!(sent.contains("ev"));
    }

    #[test]
    fn busy_dead_steer_tx_buffers() {
        let q = make_queue_with(&[("ev", Some(Severity::Medium))]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        // Create a tx whose rx is immediately dropped → send will fail
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        drop(rx);

        let drained = drain_event_queue(&q, &mut messages, &mut pending, true, Some(&tx));

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].disposition, EventDisposition::Buffered);
        assert!(messages.is_empty());
        assert_eq!(pending.len(), 1);
        assert!(pending[0].contains("ev"));
    }

    #[test]
    fn busy_no_steer_tx_buffers() {
        let q = make_queue_with(&[("ev", Some(Severity::Medium))]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, true, None);

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].disposition, EventDisposition::Buffered);
        assert!(pending.len() == 1);
    }

    #[test]
    fn empty_queue_returns_nothing() {
        let q = EventQueue::new(10);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);

        assert!(drained.is_empty());
        assert!(messages.is_empty());
        assert!(pending.is_empty());
    }

    #[test]
    fn canonical_format_contains_xml_event_tags() {
        let q = make_queue_with(&[("ping", Some(Severity::High))]);
        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);

        assert!(drained[0].formatted.starts_with("<event "));
        assert!(drained[0].formatted.ends_with("</event>"));
        assert!(drained[0].formatted.contains("severity=\"high\""));
    }

    #[test]
    fn priority_ordering_preserved_critical_first() {
        // Push medium first, then critical — critical should be drained first
        let q = EventQueue::new(10);
        q.push(crate::events::types::Event::simple("test", "medium", Some(Severity::Medium))).unwrap();
        q.push(crate::events::types::Event::simple("test", "critical", Some(Severity::Critical))).unwrap();

        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);

        assert_eq!(drained.len(), 2);
        assert!(drained[0].formatted.contains("critical"));
        assert!(drained[1].formatted.contains("medium"));
    }

    // ── wake_action truth table ──────────────────────────────────────────

    fn injected_event() -> DrainedEvent {
        let event = crate::events::types::Event::simple("test", "x", None);
        DrainedEvent {
            formatted: format_event_for_agent(&event),
            event,
            disposition: EventDisposition::Injected,
        }
    }

    fn steered_event() -> DrainedEvent {
        let event = crate::events::types::Event::simple("test", "x", None);
        DrainedEvent {
            formatted: format_event_for_agent(&event),
            event,
            disposition: EventDisposition::Steered,
        }
    }

    fn buffered_event() -> DrainedEvent {
        let event = crate::events::types::Event::simple("test", "x", None);
        DrainedEvent {
            formatted: format_event_for_agent(&event),
            event,
            disposition: EventDisposition::Buffered,
        }
    }

    #[test]
    fn wake_nothing_when_drained_empty() {
        let result = wake_action(&[], &[], false, true, 0);
        assert_eq!(result, WakeAction::Nothing);
    }

    #[test]
    fn wake_run_turn_when_injected_idle_enabled_under_cap() {
        let drained = vec![injected_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(result, WakeAction::RunTurn);
    }

    #[test]
    fn wake_forward_when_busy() {
        let drained = vec![injected_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, true, true, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_forward_when_auto_turn_disabled() {
        let drained = vec![injected_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, false, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_forward_when_last_message_not_user() {
        let drained = vec![injected_event()];
        let messages = vec![assistant_msg()];
        let result = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_forward_when_messages_empty() {
        let drained = vec![injected_event()];
        let result = wake_action(&drained, &[], false, true, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_forward_when_cap_reached() {
        let drained = vec![injected_event()];
        let messages = vec![user_msg("hello")];
        // Exactly at cap — should NOT run
        let result = wake_action(&drained, &messages, false, true, AUTO_TURN_CAP);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_run_turn_when_one_below_cap() {
        let drained = vec![injected_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, true, AUTO_TURN_CAP - 1);
        assert_eq!(result, WakeAction::RunTurn);
    }

    #[test]
    fn wake_forward_when_only_steered_events() {
        // Steered events don't qualify for RunTurn
        let drained = vec![steered_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_forward_when_only_buffered_events() {
        let drained = vec![buffered_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(result, WakeAction::Forward);
    }

    #[test]
    fn wake_run_turn_with_mixed_injected_and_steered() {
        // At least one Injected is enough
        let drained = vec![steered_event(), injected_event()];
        let messages = vec![user_msg("hello")];
        let result = wake_action(&drained, &messages, false, true, 0);
        assert_eq!(result, WakeAction::RunTurn);
    }
}
