//! PLAN-phase3 §5.1 layer 2 — the presentation half of the TUI's turn
//! machine, driven by session envelopes through the PRODUCTION arm
//! (`stream_handler::handle_session_event_arm`). The code path is one for
//! both transports (`Box<dyn ClientTransport>`), so identical envelopes →
//! identical frames under `LocalTransport` and `SocketTransport`.
//!
//! Each scenario asserts the §2.4 contract lines (transcript kinds + text
//! via the rendered frame) and pins determinism (two replays, same bytes).

use agent_engine::session::wire::{
    WireLlmEvent, WireSessionEvent as W, WireSessionStreamEvent, WireStreamEvent,
};
use agent_engine::session::{ClientId, OwnerChangeReason, TurnTrigger};
use agent_tui::tui::testing::tape::Tape;
use agent_tui::tui::testing::TestHarness;

fn text(t: &str) -> W {
    W::Stream {
        event: WireStreamEvent::Llm(WireLlmEvent::Text { text: t.into() }),
    }
}
fn done() -> W {
    W::Stream {
        event: WireStreamEvent::Session(WireSessionStreamEvent::Done),
    }
}
fn turn(trigger: TurnTrigger, user_text: Option<&str>) -> W {
    W::TurnStarted {
        turn_baseline: 1,
        trigger,
        user_text: user_text.map(str::to_string),
    }
}

/// Record a tape of `events` on a fresh harness, replay it twice, return the
/// frame (asserting determinism).
fn frame_for(events: Vec<W>) -> (String, Tape) {
    let mut h = TestHarness::boot_with_size(100, 30);
    let tape = {
        let mut rec = h.record_tape();
        for ev in events {
            rec.session_event(ev);
        }
        rec.finish()
    };
    let a = TestHarness::replay_with_size(&tape, 100, 30);
    let b = TestHarness::replay_with_size(&tape, 100, 30);
    assert_eq!(a, b, "session tape replay must be deterministic");
    (a, tape)
}

#[test]
fn session_plain_turn_renders_text_and_idles() {
    let (frame, tape) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("hello from the actor"),
        done(),
        W::Idle,
    ]);
    assert!(frame.contains("hello from the actor"), "{frame}");
    // The tape serialises (fixture-authorable) and round-trips.
    let json = tape.to_json();
    let back = Tape::from_json(&json).expect("tape parses");
    assert_eq!(TestHarness::replay_with_size(&back, 100, 30), frame);
}

#[test]
fn session_queued_autosend_pushes_user_card() {
    let (frame, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("first"),
        W::Steered {
            text: "second please".into(),
            delivered: false,
        },
        done(),
        turn(TurnTrigger::QueuedAuto, Some("second please")),
        text("second answer"),
        done(),
        W::Idle,
    ]);
    assert!(frame.contains("queued: second please"), "{frame}");
    assert!(frame.contains("second answer"), "{frame}");
}

#[test]
fn session_steer_line() {
    let (frame, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        W::Steered {
            text: "turn left".into(),
            delivered: true,
        },
    ]);
    assert!(frame.contains("→ steering: turn left"), "{frame}");
}

#[test]
fn session_abort_renders_error_line_and_stops_streaming() {
    let (frame, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::Dequeued {
            text: "later".into(),
        },
        W::Aborted {
            context_saved: false,
        },
        W::Idle,
    ]);
    assert!(frame.contains("dequeued: later"), "{frame}");
    assert!(frame.contains("aborted"), "{frame}");
}

#[test]
fn session_abort_via_actor_notice_shim_matches_typed() {
    // Until the actor emits `Aborted`, it sends the same text as a notice;
    // both must render the identical frame.
    let (typed, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::Aborted {
            context_saved: true,
        },
    ]);
    let (shim, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::SystemNotice {
            text: "aborted — context saved for next message".into(),
        },
    ]);
    assert_eq!(typed, shim);
    assert!(typed.contains("aborted — context saved for next message"));
}

#[test]
fn session_event_card_and_cap_notice() {
    let ev = agent_engine::events::types::Event::simple("watcher", "disk is full", None);
    let (frame, _) = frame_for(vec![
        W::External { event: ev },
        W::AutoTurnCapReached { cap: 5 },
    ]);
    assert!(frame.contains("disk is full"), "{frame}");
    assert!(
        frame.contains("auto-turn cap reached (5 consecutive)"),
        "{frame}"
    );
}

#[test]
fn session_prompt_activates_pane_and_resolution_by_other_dismisses() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_events(&[agent_engine::session::SessionEventWire::Prompt(
        agent_engine::session::PromptRequest {
            id: 7,
            kind: agent_engine::session::PromptKind::Secret,
            title: "API key".into(),
            prompt: "paste it".into(),
            raised_at: chrono::Utc::now(),
        },
    )]);
    assert!(h.secret_prompt_active(), "Prompt must activate the pane");
    h.feed_events(&[agent_engine::session::SessionEventWire::PromptResolved { prompt_id: 7 }]);
    assert!(!h.secret_prompt_active(), "foreign resolution dismisses");
}

#[test]
fn session_refused_is_rendered_only_for_me() {
    let (mine, _) = frame_for(vec![W::Refused {
        client: ClientId(1),
        command: "submit".into(),
        reason: "input owned by client #2".into(),
    }]);
    assert!(mine.contains("submit refused: input owned by client #2"), "{mine}");
    let (theirs, _) = frame_for(vec![W::Refused {
        client: ClientId(9),
        command: "submit".into(),
        reason: "input owned by client #2".into(),
    }]);
    assert!(!theirs.contains("refused"), "{theirs}");
}

#[test]
fn session_owner_change_toasts_previous_owner() {
    let (frame, _) = frame_for(vec![W::InputOwnerChanged {
        from: Some(ClientId(1)),
        to: Some(ClientId(3)),
        reason: OwnerChangeReason::Takeover,
    }]);
    assert!(frame.contains("input taken over by client #3"), "{frame}");
}

#[test]
fn session_abort_notice_plus_typed_renders_once() {
    // Shim idempotency: the actor may send BOTH the notice text and the
    // typed `Aborted` (engine emitting typed events while the notice is
    // still there) — exactly one "aborted" line, same frame as typed-only.
    let (typed, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::Aborted {
            context_saved: true,
        },
        W::Idle,
    ]);
    let (both, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::SystemNotice {
            text: "aborted — context saved for next message".into(),
        },
        W::Aborted {
            context_saved: true,
        },
        W::Idle,
    ]);
    assert_eq!(typed, both);
    assert_eq!(both.matches("aborted — context saved").count(), 1, "{both}");
    // A later turn's abort renders again (latch resets on TurnStarted).
    let (two_turns, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::Aborted {
            context_saved: false,
        },
        W::Idle,
        turn(TurnTrigger::User, None),
        text("partial 2"),
        W::SystemNotice {
            text: "aborted".into(),
        },
        W::Aborted {
            context_saved: false,
        },
        W::Idle,
    ]);
    assert_eq!(two_turns.matches("aborted").count(), 2, "{two_turns}");
}

#[test]
fn session_cleared_notice_plus_typed_renders_once() {
    let (both, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("old stuff"),
        done(),
        W::SystemNotice {
            text: "session cleared → new-1".into(),
        },
        W::Cleared {
            session_id: "new-1".into(),
        },
        // A second /clear (different id) must still render.
        W::Cleared {
            session_id: "new-2".into(),
        },
    ]);
    assert_eq!(both.matches("new session started").count(), 2, "{both}");
}

#[test]
fn session_cleared_resets_transcript() {
    let (frame, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("old stuff"),
        done(),
        W::Cleared {
            session_id: "new".into(),
        },
    ]);
    assert!(!frame.contains("old stuff"), "{frame}");
    assert!(frame.contains("new session started"), "{frame}");
}

#[test]
fn session_compaction_lines() {
    let (frame, _) = frame_for(vec![
        W::CompactionStarted {
            source: "manual".into(),
            disclosure: "[compaction: model x, ~10 tokens]".into(),
        },
        W::CompactionFailed {
            message: "boom".into(),
            panicked: false,
        },
    ]);
    assert!(frame.contains("compacting conversation..."), "{frame}");
    assert!(frame.contains("compaction failed: boom"), "{frame}");
}
