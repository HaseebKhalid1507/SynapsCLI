//! Central event-drain and wake-action policy — mode-agnostic engine layer.
//!
//! `drain_event_queue` pops every pending event from the queue in priority
//! order, formats it, and classifies it as Steered / Buffered / Injected.
//! `wake_action` decides what the caller should do next.
//!
//! `event_payload_from_drained` builds the canonical wire-format payload
//! (shared by RPC and server) from a `DrainedEvent`.

use tokio::sync::mpsc::UnboundedSender;

use crate::events::{EventQueue, format_event_for_agent};
use crate::SharedMessage;

// Re-export the shared wire payload so callers can use `engine::reactor::EventPayload`.
pub use agent_core::core::rpc_protocol::EventPayload;

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

// ── Wire-payload constructor ──────────────────────────────────────────────────

/// Build the canonical `EventPayload` from a `DrainedEvent`.
///
/// This is the **single canonical constructor** shared by RPC and server
/// modes — both produce identical structured payloads from the same source.
/// The `formatted` field carries the full XML-tagged, injection-safe string
/// produced by `format_event_for_agent`.
pub fn event_payload_from_drained(drained: &DrainedEvent) -> EventPayload {
    let ev = &drained.event;
    let severity = ev
        .content
        .severity
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or("medium")
        .to_string();
    EventPayload {
        id: ev.id.clone(),
        source: ev.source.source_type.clone(),
        severity,
        content_type: ev.content.content_type.clone(),
        text: ev.content.text.clone(),
        timestamp: ev.timestamp.to_rfc3339(),
        formatted: drained.formatted.clone(),
    }
}

// ── Auto-turn cap helper ──────────────────────────────────────────────────────

/// Centralised auto-turn claim gate.
///
/// **Semantics (single source of truth):**
/// * `counter < AUTO_TURN_CAP` → allowed: increment `counter` and return `true`.
/// * `counter >= AUTO_TURN_CAP` → denied: leave `counter` unchanged, return `false`.
/// * User input resets `counter` to 0 (caller responsibility — not this function).
///
/// All paths that may fire an auto-triggered model turn (TUI event arm, TUI
/// stream arm, chat EventWake, chat AutoTriggerEvents, server idle wake, server
/// AutoTriggerEvents) **must** call this function and bail when it returns `false`
/// instead of duplicating the `fetch_add / >= cap` pattern inline.
///
/// # Arguments
/// * `counter` — mutable reference to the caller's `u32` counter.
///
/// # Returns
/// `true` if a turn is allowed (counter was incremented), `false` if parked.
#[inline]
pub fn claim_auto_turn(counter: &mut u32) -> bool {
    if *counter < AUTO_TURN_CAP {
        *counter += 1;
        true
    } else {
        false
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns `true` when the agent idle loop should park on `queue.notified()`
/// instead of immediately nagging with a user message.
///
/// Invariant: exactly one waiter on `queue.notified()` exists in agent mode.
/// The caller **must** drop any registry lock before awaiting the future.
///
/// Conditions to wait:
/// * At least one child subagent is still running, **or**
/// * The event queue already has items (drain them first on loop-top).
#[inline]
pub fn idle_should_wait(children_running: bool, queue_len: usize) -> bool {
    children_running || queue_len > 0
}

// ─── terminal_flush seam for integration testing ─────────────────────────────

/// Pure decision logic mirroring `terminal_flush` from `cmd/rpc.rs` for use in
/// integration tests.
///
/// This seam allows tests to verify the `allow_chain` contract on the mutable
/// state fields that `terminal_flush` operates on, **without** requiring a live
/// `Runtime`, `Session`, or `Mutex<RpcState>`.
///
/// Rules (identical to `terminal_flush`):
/// * Always: clear `auto_turn_pending`; take `pending_events` → inject into `api_messages`.
/// * `allow_chain = false`: never reserve; return `None`; leave `counter` unchanged.
/// * `allow_chain = true`: if all conditions met, increment `counter`, set
///   `auto_turn_pending = true`, return `Some("auto:seam")`.
pub fn terminal_flush_seam(
    allow_chain: bool,
    auto_turn_pending: &mut bool,
    pending_events: &mut Vec<String>,
    api_messages: &mut Vec<crate::SharedMessage>,
    events_auto_turn: bool,
    consecutive_auto_turns: &mut u32,
) -> Option<String> {
    *auto_turn_pending = false;
    let to_inject = std::mem::take(pending_events);
    let had_buffered = !to_inject.is_empty();
    for formatted in to_inject {
        api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": formatted})
        ));
    }
    if allow_chain
        && had_buffered
        && events_auto_turn
        && *consecutive_auto_turns < AUTO_TURN_CAP
        && api_messages.last().map(|m| m["role"].as_str() == Some("user")).unwrap_or(false)
        && claim_auto_turn(consecutive_auto_turns)
    {
        *auto_turn_pending = true;
        return Some("auto:seam".to_string());
    }
    None
}

// ─── spawn_prompt registration seam ─────────────────────────────────────────

/// Pure decision logic for the `spawn_prompt` registration re-check (Bug 1 fix).
///
/// Models the guard that runs **inside** the registration lock just before
/// `in_flight` is written. Returns `true` if it is safe to proceed with
/// registration (caller should write `in_flight` and clear `auto_turn_pending`);
/// returns `false` if the reservation was revoked by an intervening Abort and
/// the caller must abort the spawned task (drop `start_tx`).
///
/// Rules (identical to the re-check in `spawn_prompt`):
/// * Non-auto prompts (`!is_auto`) always return `true` — they are validated
///   by `handle_prompt`'s `is_busy()` check before reaching here.
/// * Auto prompts: `true` iff `auto_turn_pending && !in_flight_live`.
/// * If returning `false`, the helper also defensively clears `auto_turn_pending`.
///
/// Used by `spawn_prompt` and by integration tests (not the replica scheduler).
pub fn spawn_prompt_registration_check(
    is_auto: bool,
    auto_turn_pending: &mut bool,
    in_flight_live: bool,
) -> bool {
    if !is_auto {
        // A client prompt was checked before spawning, but an auto-turn or
        // another prompt may have reserved/registered in the meantime.
        return !*auto_turn_pending && !in_flight_live;
    }
    if *auto_turn_pending && !in_flight_live {
        return true;
    }
    // Reservation revoked — clear pending defensively before returning false.
    *auto_turn_pending = false;
    false
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

    // ── idle_should_wait truth table ─────────────────────────────────────────

    #[test]
    fn idle_should_wait_false_when_no_children_no_queue() {
        assert!(!idle_should_wait(false, 0));
    }

    #[test]
    fn idle_should_wait_true_when_children_running() {
        assert!(idle_should_wait(true, 0));
    }

    #[test]
    fn idle_should_wait_true_when_queue_nonempty() {
        assert!(idle_should_wait(false, 1));
    }

    #[test]
    fn idle_should_wait_true_when_both_children_and_queue() {
        assert!(idle_should_wait(true, 3));
    }

    #[test]
    fn idle_should_wait_true_when_large_queue() {
        assert!(idle_should_wait(false, 999));
    }

    // ── claim_auto_turn boundary tests ─────────────────────────────────────

    #[test]
    fn claim_auto_turn_first_five_allowed() {
        let mut counter: u32 = 0;
        for _ in 0..AUTO_TURN_CAP {
            assert!(claim_auto_turn(&mut counter), "turn within cap must be allowed");
        }
        assert_eq!(counter, AUTO_TURN_CAP, "counter must equal cap after 5 claims");
    }

    #[test]
    fn claim_auto_turn_sixth_denied() {
        let mut counter: u32 = AUTO_TURN_CAP;
        assert!(!claim_auto_turn(&mut counter), "turn at cap must be denied");
        assert_eq!(counter, AUTO_TURN_CAP, "counter must remain unchanged when denied");
    }

    #[test]
    fn claim_auto_turn_remains_denied_until_reset() {
        let mut counter: u32 = AUTO_TURN_CAP;
        assert!(!claim_auto_turn(&mut counter));
        assert!(!claim_auto_turn(&mut counter));
        assert!(!claim_auto_turn(&mut counter));
        assert_eq!(counter, AUTO_TURN_CAP, "counter must not change while denied");
    }

    #[test]
    fn claim_auto_turn_reset_re_enables() {
        let mut counter: u32 = AUTO_TURN_CAP;
        assert!(!claim_auto_turn(&mut counter));
        // Simulate user input reset
        counter = 0;
        assert!(claim_auto_turn(&mut counter), "after reset first turn must be allowed");
        assert_eq!(counter, 1);
    }

    #[test]
    fn claim_auto_turn_exact_boundary_sequence() {
        // Exactly AUTO_TURN_CAP (5) allowed, 6th denied
        let mut c: u32 = 0;
        for i in 1..=AUTO_TURN_CAP {
            assert!(claim_auto_turn(&mut c), "turn {i} must be allowed");
            assert_eq!(c, i);
        }
        // 6th
        assert!(!claim_auto_turn(&mut c));
        assert_eq!(c, AUTO_TURN_CAP);
    }
}
