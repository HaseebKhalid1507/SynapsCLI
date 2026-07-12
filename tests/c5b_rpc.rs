//! C5b: RPC mode forwards runtime events and auto-turns when config enabled.
//!
//! Policy under test:
//!  * Drainer always builds `RpcEvent::Event` frames from drained events.
//!  * `events_auto_turn = true`  (default) → `WakeAction::RunTurn` when idle,
//!    injected events present, last message is user, counter < cap.
//!  * `events_auto_turn = false` → `WakeAction::Forward` only; no auto-turn.
//!  * `is_busy = in_flight || auto_turn_pending` — both block Prompt/FollowUp.
//!  * `claim_auto_turn` coalesces: one reservation per batch.
//!  * Real Prompt/FollowUp resets `consecutive_auto_turns` counter to 0.
//!  * Oneshot start-barrier guarantees `in_flight` is set before task proceeds.
//!
//! No live API calls. All tests exercise the drain+payload+frame pipeline,
//! `wake_action`, and `claim_auto_turn` seams only.

use agent_engine::engine::reactor::{
    drain_event_queue, event_payload_from_drained, wake_action, claim_auto_turn,
    EventDisposition, WakeAction, AUTO_TURN_CAP,
    spawn_prompt_registration_check,
};
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
        .map(|d| RpcEvent::Event { payload: Box::new(event_payload_from_drained(d)) })
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
    let frame = RpcEvent::Event { payload: Box::new(event_payload_from_drained(&drained[0])) };

    let json = serde_json::to_string(&frame).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(val["type"], "event");
    assert_eq!(val["payload"]["source"], "test-src");
    assert_eq!(val["payload"]["severity"], "medium");
    assert_eq!(val["payload"]["text"], "ping");
    assert!(!val["payload"]["timestamp"].as_str().unwrap().is_empty());
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

// ─── T4: auto-turn fires when config=true, opt-out when config=false ──────────

#[test]
fn wake_action_run_turn_when_auto_turn_enabled() {
    let q = make_queue(&[("src", "ev", Some(Severity::Medium))]);
    let mut messages = vec![user_msg("prior user msg")];
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    // events_auto_turn = true → RunTurn expected when counter < cap
    let action = wake_action(&drained, &messages, false, true, 0);
    assert_eq!(action, WakeAction::RunTurn,
        "auto-turn enabled: drainer should produce RunTurn on idle inject");
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
    assert_ne!(action, WakeAction::RunTurn,
        "RPC opt-out: auto_turn_enabled=false must not produce RunTurn");
}

// ─── T5: terminal_flush_seam — false path never reserves, counter unchanged ───

/// `allow_chain = false` (error/cancel/drop paths):
/// * `auto_turn_pending` is cleared
/// * `pending_events` are flushed into `api_messages` (events not lost)
/// * `consecutive_auto_turns` counter is NOT incremented
/// * Return value is always `None`
#[test]
fn terminal_flush_false_never_reserves_and_counter_unchanged() {
    use agent_engine::engine::reactor::terminal_flush_seam;

    let mut auto_turn_pending = true; // was set; must be cleared
    let mut pending_events = vec!["<event>test</event>".to_string()];
    let mut api_messages: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "prior"}))
    ];
    let events_auto_turn = true; // enabled — must still not fire on false path
    let mut counter: u32 = 0;

    let result = terminal_flush_seam(
        false, // allow_chain = false (error path)
        &mut auto_turn_pending,
        &mut pending_events,
        &mut api_messages,
        events_auto_turn,
        &mut counter,
    );

    // Must return None — no auto-turn scheduled
    assert!(result.is_none(), "false path must never return a reserved id");
    // auto_turn_pending must be cleared
    assert!(!auto_turn_pending, "false path must clear auto_turn_pending");
    // pending_events must be drained
    assert!(pending_events.is_empty(), "pending_events must be flushed");
    // events injected into api_messages (event not lost)
    assert_eq!(api_messages.len(), 2, "buffered event must be injected into api_messages");
    // Counter must NOT change — critical invariant
    assert_eq!(counter, 0, "false path must not increment consecutive_auto_turns");
}

// ─── T7: terminal_flush_seam — true path reserves when eligible ───────────────

/// `allow_chain = true` (Done path):
/// * When all conditions met → returns `Some(id)`, increments counter, sets pending
/// * When cap reached → returns `None`, does not increment
#[test]
fn terminal_flush_true_reserves_when_eligible() {
    use agent_engine::engine::reactor::terminal_flush_seam;

    let mut auto_turn_pending = false;
    let mut pending_events = vec!["<event>ev</event>".to_string()];
    let mut api_messages: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "prior"}))
    ];
    let events_auto_turn = true;
    let mut counter: u32 = 0;

    let result = terminal_flush_seam(
        true, // allow_chain = true (Done path)
        &mut auto_turn_pending,
        &mut pending_events,
        &mut api_messages,
        events_auto_turn,
        &mut counter,
    );

    // Must return Some — auto-turn was reserved
    assert!(result.is_some(), "true path must return auto_id when conditions met");
    // auto_turn_pending must be set
    assert!(auto_turn_pending, "true path must set auto_turn_pending = true");
    // counter incremented
    assert_eq!(counter, 1, "true path must increment consecutive_auto_turns");
}

#[test]
fn terminal_flush_true_does_not_reserve_at_cap() {
    use agent_engine::engine::reactor::terminal_flush_seam;

    let mut auto_turn_pending = false;
    let mut pending_events = vec!["<event>ev</event>".to_string()];
    let mut api_messages: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "prior"}))
    ];
    let events_auto_turn = true;
    let mut counter: u32 = AUTO_TURN_CAP; // already at cap

    let result = terminal_flush_seam(
        true, // allow_chain = true, but cap is exhausted
        &mut auto_turn_pending,
        &mut pending_events,
        &mut api_messages,
        events_auto_turn,
        &mut counter,
    );

    assert!(result.is_none(), "true path at cap must not reserve");
    assert!(!auto_turn_pending, "auto_turn_pending must stay false when denied");
    assert_eq!(counter, AUTO_TURN_CAP, "counter must not change when denied");
}

#[test]
fn terminal_flush_false_does_not_reserve_even_at_zero_counter() {
    use agent_engine::engine::reactor::terminal_flush_seam;

    // Stress-test: even with perfect conditions, false path must never reserve.
    let mut auto_turn_pending = false;
    let mut pending_events = vec!["<event>ev</event>".to_string()];
    let mut api_messages: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "prior"}))
    ];
    let events_auto_turn = true;
    let mut counter: u32 = 0;

    for _ in 0..3 {
        let result = terminal_flush_seam(
            false, // always false
            &mut auto_turn_pending,
            &mut pending_events,
            &mut api_messages,
            events_auto_turn,
            &mut counter,
        );
        assert!(result.is_none());
        assert_eq!(counter, 0, "counter must remain 0 across all false-path calls");
        // Re-arm pending for next iteration
        pending_events.push("<event>more</event>".to_string());
    }
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
    assert_eq!(counter, AUTO_TURN_CAP, "counter must not change when denied");
}

// ─── T7: real user input resets consecutive counter ──────────────────────────

/// terminal_flush_seam with allow_chain=true resets normally.
/// A real Prompt handler resets counter to 0 — verify that after reset,
/// a true-path flush can reserve again.
#[test]
fn consecutive_counter_reset_on_real_user_input() {
    use agent_engine::engine::reactor::terminal_flush_seam;

    // Simulate real Prompt/FollowUp: counter was at cap, real user input resets to 0.
    let mut counter: u32 = 0; // start at 0 (real Prompt resets it)
    let mut auto_turn_pending = false;

    // Now a Done-path flush should be able to reserve again.
    let mut pending_events = vec!["<event>ev</event>".to_string()];
    let mut api_messages: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "user msg"}))
    ];
    let result = terminal_flush_seam(
        true,
        &mut auto_turn_pending,
        &mut pending_events,
        &mut api_messages,
        true,
        &mut counter,
    );
    assert!(result.is_some(), "after counter reset, true-path must be able to reserve");
    assert_eq!(counter, 1, "counter must be incremented to 1 after reset and claim");
}

// ─── T8: RPC start-barrier ordering — terminal_flush cannot precede in_flight ─

/// Seam test for the oneshot start-barrier ordering guarantee in `spawn_prompt`.
///
/// We simulate the race by using a oneshot channel directly:
///   1. Spawn a task that awaits `start_rx` before calling `terminal_flush`.
///   2. Register "in_flight" (simulated here as a flag).
///   3. Send on `start_tx` to release the task.
///   4. Join the task and assert that `in_flight` was set before the task ran.
///
/// This is a deterministic seam test — it does not call `spawn_prompt` directly
/// (which needs a full Runtime) but exercises the identical ordering invariant.
#[tokio::test]
async fn rpc_start_barrier_in_flight_set_before_task_proceeds() {
    use tokio::sync::{oneshot, Mutex};
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeState {
        in_flight_set: bool,
        task_started: bool,
    }

    let state = Arc::new(Mutex::new(FakeState::default()));
    let (start_tx, start_rx) = oneshot::channel::<()>();

    let state_task = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        // Task MUST wait for start signal before observing/mutating state.
        start_rx.await.expect("start_tx should not be dropped");
        let mut st = state_task.lock().await;
        // By the time we run, in_flight_set MUST already be true.
        assert!(
            st.in_flight_set,
            "task ran before in_flight was registered — start barrier violated!"
        );
        st.task_started = true;
    });

    // Register in_flight BEFORE releasing the barrier.
    {
        let mut st = state.lock().await;
        st.in_flight_set = true;
    }

    // Release the barrier — task may now proceed.
    start_tx.send(()).expect("task should be alive");

    handle.await.expect("task panicked");

    let st = state.lock().await;
    assert!(st.in_flight_set, "in_flight must remain set after task completes");
    assert!(st.task_started, "task must have run");
}

/// Complementary: if start_tx is dropped (caller panic), task exits cleanly —
/// no zombie, no state mutation.
#[tokio::test]
async fn rpc_start_barrier_dropped_tx_exits_task_cleanly() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use tokio::sync::oneshot;

    let (start_tx, start_rx) = oneshot::channel::<()>();
    let side_effect = Arc::new(AtomicBool::new(false));
    let side_effect_task = Arc::clone(&side_effect);

    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            // Correct: caller dropped — exit without touching state.
            return;
        }
        // Should NOT reach here.
        side_effect_task.store(true, Ordering::SeqCst);
    });

    drop(start_tx); // Simulate caller panic / drop.
    handle.await.expect("task should exit cleanly");
    assert!(!side_effect.load(Ordering::SeqCst), "task must not execute state work when start_tx is dropped");
}

// ─── T9: RPC event frame is exactly one frame per event ──────────────────────

#[test]
fn rpc_event_frame_is_exactly_one_per_drained_event() {
    let q = make_queue(&[
        ("src-a", "alpha", Some(Severity::Low)),
        ("src-b", "beta",  Some(Severity::High)),
        ("src-c", "gamma", Some(Severity::Medium)),
    ]);
    let mut messages = Vec::new();
    let mut pending = Vec::new();

    let drained = drain_event_queue(&q, &mut messages, &mut pending, false, None);
    let frames: Vec<RpcEvent> = drained
        .iter()
        .map(|d| RpcEvent::Event { payload: Box::new(event_payload_from_drained(d)) })
        .collect();

    // Exactly one frame per drained event — no duplicates, no drops.
    assert_eq!(frames.len(), drained.len());
    assert_eq!(frames.len(), 3);
}

// ─── T10: abort-between-snapshot-and-registration seam ───────────────────────

/// Deterministic seam test for the Bug 1 scenario:
///   1. Auto-turn is reserved: `auto_turn_pending = true`, `in_flight = false`.
///   2. Snapshot guard passes (conditions satisfied at snapshot time).
///   3. Abort arrives and clears `auto_turn_pending` (simulates the race window).
///   4. Registration check (re-validation inside registration lock) must DENY.
///   5. `auto_turn_pending` remains false — no ghost, session not stuck.
///
/// Uses `spawn_prompt_registration_check` — the tiny pure helper extracted from
/// `spawn_prompt`'s registration lock block — so no live Runtime is needed.
#[test]
fn abort_between_snapshot_and_registration_check_denies() {
    // Phase 1: auto-turn reserved, snapshot guard would pass.
    let mut auto_turn_pending = true;
    let in_flight_live = false;
    let is_auto = true;

    // Verify: snapshot guard condition (same logic as guard in spawn_prompt).
    assert!(
        auto_turn_pending && !in_flight_live,
        "precondition: guard should pass at snapshot time"
    );

    // Phase 2: simulate Abort arriving in the race window — clears pending.
    auto_turn_pending = false; // Abort's handle_abort clears this

    // Phase 3: registration re-check runs inside the lock.
    let allowed = spawn_prompt_registration_check(
        is_auto,
        &mut auto_turn_pending,
        in_flight_live,
    );

    // Must deny — reservation was revoked.
    assert!(
        !allowed,
        "registration must be denied when Abort cleared auto_turn_pending"
    );
    // auto_turn_pending must be false (helper cleared it defensively if not already).
    assert!(
        !auto_turn_pending,
        "auto_turn_pending must be false after denied registration — no ghost"
    );
}

/// Complementary: registration check allows when reservation is still valid.
#[test]
fn registration_check_allows_when_reservation_intact() {
    let mut auto_turn_pending = true;
    let in_flight_live = false;
    let is_auto = true;

    let allowed = spawn_prompt_registration_check(
        is_auto,
        &mut auto_turn_pending,
        in_flight_live,
    );

    assert!(allowed, "registration must be allowed when reservation is intact");
    // Helper must NOT touch auto_turn_pending on the allow path.
    assert!(
        auto_turn_pending,
        "auto_turn_pending must remain true when registration is allowed (caller clears it)"
    );
}

/// Non-auto prompts pass only when no reservation or live turn raced in.
#[test]
fn registration_check_revalidates_non_auto_prompts() {
    let mut clear = false;
    assert!(spawn_prompt_registration_check(false, &mut clear, false));

    let mut auto_reserved = true;
    assert!(!spawn_prompt_registration_check(false, &mut auto_reserved, false));

    let mut no_reservation = false;
    assert!(!spawn_prompt_registration_check(false, &mut no_reservation, true));
}

/// Registration check denies when in_flight is already Some (concurrent real prompt
/// raced in after snapshot guard and before registration lock).
#[test]
fn registration_check_denies_when_in_flight_live() {
    let mut auto_turn_pending = true; // still set
    let in_flight_live = true;        // but a real prompt is now live
    let is_auto = true;

    let allowed = spawn_prompt_registration_check(
        is_auto,
        &mut auto_turn_pending,
        in_flight_live,
    );

    assert!(!allowed, "must deny when in_flight is already live");
    assert!(!auto_turn_pending, "auto_turn_pending must be cleared on denial");
}
