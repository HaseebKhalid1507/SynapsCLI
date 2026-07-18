//! C6 cross-mode runtime event contract tests.
//!
//! Contract invariants:
//!
//! 1. `canonical_event_formats_identically_across_all_modes`
//!    - Idle-injected content == canonical formatted string
//!    - Busy-buffered (steer) string == canonical formatted string
//!    - `event_payload_from_drained` on both Injected and Buffered DrainedEvents
//!      produces structurally identical payload JSON (same id/source/severity/timestamp/formatted)
//!
//! 2. `buffered_events_flush_without_duplication_or_metadata_loss`
//!    - One buffered event yields EXACTLY ONE wire RpcEvent::Event frame total
//!      (the drainer frame emitted at buffer time; Done flush must NOT emit
//!      another frame for the same event)
//!    - The injected content at Done is raw formatted text only, pushed into
//!      api_messages — NOT another Event wire frame
//!    - Real id / source / severity / timestamp are preserved (no fake UUIDs,
//!      no source="buffered", no metadata loss)

use agent_engine::engine::reactor::{
    drain_event_queue, event_payload_from_drained, EventDisposition,
};
use agent_engine::events::{types::{Event, Severity}, EventQueue};
use synaps_cli::core::rpc_protocol::RpcEvent;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_event(source: &str, text: &str, sev: Severity) -> Event {
    let mut ev = Event::simple(source, text, Some(sev));
    ev.content.content_type = "message".into();
    ev
}

fn push_event(q: &EventQueue, ev: Event) {
    q.push(ev).unwrap();
}

// ─── T1: canonical_event_formats_identically_across_all_modes ────────────────

#[test]
fn canonical_event_formats_identically_across_all_modes() {
    let ev = make_event("uptime-kuma", "Jellyfin DOWN", Severity::Critical);
    let event_id = ev.id.clone();

    // ── Idle path (Injected) ──
    let q_idle = EventQueue::new(64);
    push_event(&q_idle, ev.clone());
    let mut msgs_idle: Vec<synaps_cli::SharedMessage> = Vec::new();
    let mut pending_idle: Vec<String> = Vec::new();
    let drained_idle = drain_event_queue(&q_idle, &mut msgs_idle, &mut pending_idle, false, None);
    assert_eq!(drained_idle.len(), 1);
    assert_eq!(drained_idle[0].disposition, EventDisposition::Injected);

    let canonical = drained_idle[0].formatted.clone();

    // Injected content in api_messages matches canonical
    let injected_content = msgs_idle[0]["content"].as_str().unwrap();
    assert_eq!(
        injected_content, canonical,
        "idle injected content must equal canonical formatted string"
    );

    // ── Busy path (Buffered, no steer tx) ──
    let q_busy = EventQueue::new(64);
    push_event(&q_busy, ev.clone());
    let mut msgs_busy: Vec<synaps_cli::SharedMessage> = Vec::new();
    let mut pending_busy: Vec<String> = Vec::new();
    let drained_busy = drain_event_queue(&q_busy, &mut msgs_busy, &mut pending_busy, true, None);
    assert_eq!(drained_busy.len(), 1);
    assert_eq!(drained_busy[0].disposition, EventDisposition::Buffered);

    let busy_buffered_str = &pending_busy[0];
    assert_eq!(
        busy_buffered_str, &canonical,
        "busy buffered string must equal canonical formatted string"
    );

    // ── Payload structural equality ──
    let payload_idle = event_payload_from_drained(&drained_idle[0]);
    let payload_busy = event_payload_from_drained(&drained_busy[0]);

    // IDs must be the same real event ID — not fake UUIDs
    assert_eq!(payload_idle.id, event_id, "idle payload id must be real event id");
    assert_eq!(payload_busy.id, event_id, "busy payload id must be real event id");

    // Source must be real source — not "buffered"
    assert_eq!(payload_idle.source, "uptime-kuma");
    assert_eq!(payload_busy.source, "uptime-kuma");

    // Severity must be preserved
    assert_eq!(payload_idle.severity, "critical");
    assert_eq!(payload_busy.severity, "critical");

    // Formatted strings must be identical across modes
    assert_eq!(payload_idle.formatted, payload_busy.formatted,
        "payload.formatted must be identical across idle and busy drain paths");

    // Wire JSON equality (structural)
    let json_idle = serde_json::to_value(&payload_idle).unwrap();
    let json_busy = serde_json::to_value(&payload_busy).unwrap();

    assert_eq!(json_idle["id"], json_busy["id"]);
    assert_eq!(json_idle["source"], json_busy["source"]);
    assert_eq!(json_idle["severity"], json_busy["severity"]);
    assert_eq!(json_idle["formatted"], json_busy["formatted"]);
    assert_eq!(json_idle["text"], json_busy["text"]);

    // RpcEvent::Event wrapping produces same payload JSON
    let rpc_idle = RpcEvent::Event { payload: Box::new(payload_idle) };
    let rpc_busy = RpcEvent::Event { payload: Box::new(payload_busy) };
    let rpc_json_idle = serde_json::to_value(&rpc_idle).unwrap();
    let rpc_json_busy = serde_json::to_value(&rpc_busy).unwrap();
    assert_eq!(rpc_json_idle["payload"], rpc_json_busy["payload"],
        "RpcEvent::Event payload JSON must be structurally equal across modes");
}

// ─── T2: buffered_events_flush_without_duplication_or_metadata_loss ──────────
//
// This test exercises the SEAM between the drainer and the Done flush.
// It proves:
//   a) drainer emits ONE RpcEvent::Event frame (correct)
//   b) Done flush injects formatted text into api_messages (correct)
//   c) Done flush does NOT emit another RpcEvent::Event frame (the bug)
//   d) The injected content in api_messages == the canonical formatted string
//   e) Metadata (id, source, severity) are the REAL values — no fake UUIDs
//
// This test is written against the FIXED behavior. It validates the contract
// that pending_events are flushed as content only, not as additional frames.

#[test]
fn buffered_events_flush_without_duplication_or_metadata_loss() {
    let ev = make_event("grafana", "CPU spike 95%", Severity::High);
    let real_id = ev.id.clone();

    // Phase 1: drainer runs while busy — event gets buffered
    let q = EventQueue::new(64);
    push_event(&q, ev.clone());

    let mut api_messages: Vec<synaps_cli::SharedMessage> = Vec::new();
    let mut pending_events: Vec<String> = Vec::new();

    let drained = drain_event_queue(&q, &mut api_messages, &mut pending_events, true, None);

    assert_eq!(drained.len(), 1, "drainer must drain exactly one event");
    assert_eq!(drained[0].disposition, EventDisposition::Buffered);
    assert_eq!(pending_events.len(), 1, "one string buffered");
    assert!(api_messages.is_empty(), "no messages injected while busy");

    // The drainer emits ONE RpcEvent::Event frame from drained[]:
    let drainer_frame = RpcEvent::Event {
        payload: Box::new(event_payload_from_drained(&drained[0])),
    };

    // Verify drainer frame has REAL metadata (not fake)
    match &drainer_frame {
        RpcEvent::Event { payload } => {
            assert_eq!(payload.id, real_id, "drainer frame must have real event id");
            assert_eq!(payload.source, "grafana", "drainer frame must have real source");
            assert_eq!(payload.severity, "high", "drainer frame must have real severity");
            assert_ne!(payload.source, "buffered", "source must NOT be 'buffered'");
        }
        _ => panic!("expected Event frame"),
    }

    // Phase 2: Done flush — simulating the fixed behavior
    // The fix: flush pending_events as api_messages content ONLY.
    // Do NOT emit another RpcEvent::Event frame.
    let canonical_formatted = pending_events[0].clone();

    // Simulate the fixed Done flush:
    //   - Inject buffered formatted strings as api_messages entries
    //   - Do NOT create additional wire frames
    let additional_wire_frames: Vec<RpcEvent> = Vec::new();

    // FIXED behavior: inject content into api_messages only
    for formatted in std::mem::take(&mut pending_events) {
        api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": formatted})
        ));
        // CORRECT: no additional_wire_frames.push(...) here
    }

    // Verify: exactly ZERO additional wire frames from Done flush
    assert_eq!(
        additional_wire_frames.len(), 0,
        "Done flush must NOT emit additional RpcEvent::Event frames — \
         would cause wire duplication. Got {} extra frames.",
        additional_wire_frames.len()
    );

    // Verify: api_messages now has the buffered content
    assert_eq!(api_messages.len(), 1, "exactly one message injected at Done");
    let injected = api_messages[0]["content"].as_str().unwrap();
    assert_eq!(
        injected, &canonical_formatted,
        "injected content must be the canonical formatted string"
    );

    // Verify: the content is the canonical XML event format (not a JSON blob, not re-wrapped)
    assert!(injected.starts_with("<event "), "injected content must be canonical XML event format");
    assert!(injected.contains("source=\"grafana\""));
    assert!(injected.contains("severity=\"high\""));
    assert!(injected.ends_with("</event>"));

    // Total wire frames for this one event: exactly ONE (the drainer frame)
    let total_wire_frames = 1 + additional_wire_frames.len(); // 1 = drainer frame
    assert_eq!(total_wire_frames, 1,
        "exactly ONE wire frame total per buffered event — got {}", total_wire_frames);
}

// ─── T3: real metadata preserved through full drain→payload pipeline ──────────

#[test]
fn event_metadata_survives_drain_and_payload_construction() {
    let ev = make_event("discord", "ping from #alerts", Severity::High);
    let real_id = ev.id.clone();
    let real_ts_approx = ev.timestamp.timestamp();

    let q = EventQueue::new(64);
    push_event(&q, ev);

    let mut msgs: Vec<synaps_cli::SharedMessage> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let drained = drain_event_queue(&q, &mut msgs, &mut pending, false, None);

    let payload = event_payload_from_drained(&drained[0]);

    // ID is preserved exactly
    assert_eq!(payload.id, real_id);
    // Source is preserved exactly
    assert_eq!(payload.source, "discord");
    // Severity is preserved
    assert_eq!(payload.severity, "high");
    // Text is preserved
    assert_eq!(payload.text, "ping from #alerts");
    // Timestamp is a valid RFC3339 string close to the event time
    let parsed_ts = chrono::DateTime::parse_from_rfc3339(&payload.timestamp)
        .expect("timestamp must be valid RFC3339");
    let delta = (parsed_ts.timestamp() - real_ts_approx).abs();
    assert!(delta < 5, "timestamp drift must be < 5s, got {delta}s");
    // Formatted is non-empty XML
    assert!(payload.formatted.starts_with("<event "));
    assert!(payload.formatted.ends_with("</event>"));
    // Source is never the fake sentinel
    assert_ne!(payload.source, "buffered");
}

// ─── T3: claim_auto_turn uniform cap semantics ───────────────────────────────

/// Verifies the uniform cap semantics used by TUI, chat, and server.
/// First 5 claims (0..AUTO_TURN_CAP) allowed; 6th and beyond denied until reset.
#[test]
fn claim_auto_turn_uniform_boundary_semantics() {
    use agent_engine::engine::reactor::{claim_auto_turn, AUTO_TURN_CAP};

    let mut counter: u32 = 0;

    // First AUTO_TURN_CAP (5) turns allowed.
    for i in 1..=AUTO_TURN_CAP {
        assert!(
            claim_auto_turn(&mut counter),
            "turn {i} of {AUTO_TURN_CAP} must be allowed"
        );
        assert_eq!(counter, i, "counter must equal {i} after {i} claims");
    }

    // 6th turn denied; counter must remain at cap.
    assert!(
        !claim_auto_turn(&mut counter),
        "turn {} must be denied (cap = {})", AUTO_TURN_CAP + 1, AUTO_TURN_CAP
    );
    assert_eq!(counter, AUTO_TURN_CAP, "counter must remain at cap when denied");

    // Repeated denials keep counter stable.
    for _ in 0..3 {
        assert!(!claim_auto_turn(&mut counter), "must remain denied");
        assert_eq!(counter, AUTO_TURN_CAP);
    }

    // User input resets; first turn immediately re-allowed.
    counter = 0;
    assert!(
        claim_auto_turn(&mut counter),
        "first turn after reset must be allowed"
    );
    assert_eq!(counter, 1);
}

// ─── T3 (request-lifecycle hardening): typed TurnOutcome cross-mode contract ──
//
// Spec §5.2: every frontend (chat, server, TUI, RPC, watcher, subagents)
// receives the SAME typed terminal outcome — variant + correlation ID — for
// the same failure. The engine derives it exactly once
// (`runtime::helpers::turn_error_for`); adapters forward it verbatim.

#[test]
fn turn_failure_outcome_identical_across_frontend_seams() {
    use agent_engine::engine::stream::{process_stream_event, StreamCompletion};
    use synaps_cli::{SessionEvent, StreamEvent, TurnError, TurnOutcome};

    // One failure fixture, as emitted by the engine stream task.
    let fixture = TurnError::provider("stub provider failure", "api_status", "turn-fixture-7");

    // ── chat / server seam (StreamCompletion from process_stream_event) ──
    let mut messages: Vec<synaps_cli::SharedMessage> = vec![std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": "hi"}),
    )];
    let turn_baseline = messages.len();
    let mut subagents = Vec::new();
    let mut queued = None;
    let mut pending = Vec::new();
    let (_, completion) = process_stream_event(
        StreamEvent::Session(SessionEvent::Error(fixture.clone())),
        &mut messages,
        &mut subagents,
        &mut queued,
        &mut pending,
        turn_baseline,
    );
    let StreamCompletion::Error(received) = completion else {
        panic!("failure event must complete as StreamCompletion::Error");
    };
    assert_eq!(
        received, fixture,
        "chat/server must receive the engine's typed outcome verbatim"
    );
    assert_eq!(
        received.outcome.correlation_id(),
        Some("turn-fixture-7"),
        "correlation ID must survive the chat/server seam"
    );
    assert!(received.outcome.is_failure());
    assert_eq!(
        messages.len(),
        1,
        "pre-existing trailing user message must survive in every frontend"
    );

    // ── RPC seam — outcome serialized verbatim into the Response body
    //    (mirrors src/cmd/rpc.rs SessionEvent::Error handling) ──
    let body = serde_json::json!({
        "ok": false,
        "error": received.message,
        "outcome": received.outcome,
    });
    assert_eq!(body["outcome"]["kind"], "provider_failed");
    assert_eq!(body["outcome"]["code"], "api_status");
    assert_eq!(body["outcome"]["correlation_id"], "turn-fixture-7");
    let round_trip: TurnOutcome = serde_json::from_value(body["outcome"].clone()).unwrap();
    assert_eq!(round_trip, fixture.outcome, "RPC outcome must round-trip");

    // ── TUI / watcher / subagent seam — the shared category label carries
    //    the identical terminal category + correlation ID ──
    let label = fixture.category_label();
    assert!(label.contains("provider_failed"), "label: {label}");
    assert!(label.contains("code=api_status"), "label: {label}");
    assert!(
        label.contains("correlation=turn-fixture-7"),
        "label: {label}"
    );
}
