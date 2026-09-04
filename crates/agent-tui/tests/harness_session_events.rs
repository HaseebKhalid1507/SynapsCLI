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

/// The actor's `SystemNotice` text is rendered as a plain system line —
/// there is no text→typed shim any more: `Aborted`/`Cleared` are typed
/// events on the wire (engine db603206), so a notice that happens to read
/// "aborted" is just a notice.
#[test]
fn session_notice_text_is_not_a_typed_event() {
    let (frame, _) = frame_for(vec![
        turn(TurnTrigger::User, None),
        text("partial"),
        W::SystemNotice {
            text: "session cleared → new-1".into(),
        },
    ]);
    assert!(frame.contains("partial"), "{frame}");
    assert!(!frame.contains("new session started"), "{frame}");
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

/// Phase 4 §2.3 (B5): a compaction under Digest mode — the `Conversation`
/// arrives with NO messages (as on the wire) and the arm fetches the
/// daemon's `DisplayTail` — renders the same transcript as Full mode,
/// where the snapshot carries the history and is projected locally.
#[test]
fn session_compaction_digest_matches_full() {
    use agent_engine::session::wire::ConversationDigest;
    use agent_engine::session::{ConversationSnapshot, SessionEventWire as S, SessionHeader};

    let history: Vec<synaps_cli::SharedMessage> = vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "<context-summary>old</context-summary>"})),
        std::sync::Arc::new(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "after compaction text"}]})),
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "follow-up question"})),
        std::sync::Arc::new(serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "deep"},
            {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}]})),
    ];
    let header = SessionHeader {
        id: "sess-new".into(),
        ..Default::default()
    };
    let applied = || W::CompactionApplied {
        previous_session_id: "sess-old".into(),
        session_id: "sess-new".into(),
        chains_advanced: vec!["main".into()],
        queued_restored: None,
        msg_count: 40,
    };

    // Full mode: the Conversation carries the history.
    let full_snap = ConversationSnapshot {
        header: header.clone(),
        messages_len: history.len(),
        api_messages: history.clone(),
        ..Default::default()
    };
    let mut full = TestHarness::boot_with_size(100, 30);
    full.feed_event(applied().into());
    full.feed_event(S::Conversation(full_snap.clone()));
    let full_frame = full.snapshot();

    // Digest mode: the Conversation is the wire digest (no messages); the arm
    // must query DisplayTail from the (scripted) daemon.
    let digest_snap = ConversationDigest::of(&full_snap).into_snapshot(Vec::new());
    assert!(digest_snap.api_messages.is_empty());
    assert_eq!(digest_snap.messages_len, history.len());
    let mut digest = TestHarness::boot_with_size(100, 30);
    digest.set_history(history.clone());
    digest.feed_event(applied().into());
    digest.feed_event(S::Conversation(digest_snap));
    let digest_frame = digest.snapshot();

    assert!(full_frame.contains("after compaction text"), "{full_frame}");
    assert!(full_frame.contains("follow-up question"), "{full_frame}");
    assert!(full_frame.contains("chain 'main' advanced: sess-old → sess-new"), "{full_frame}");
    assert!(full_frame.contains("✓ compacted 40 messages"), "{full_frame}");
    assert!(!full_frame.contains("<context-summary>"), "{full_frame}");
    assert_eq!(full_frame, digest_frame, "Digest rebuild must render byte-identical to Full");
    assert!(
        digest.sent_commands().iter().any(|c| c.contains("Query") && c.contains("DisplayTail")),
        "Digest mode fetched the tail: {:?}",
        digest.sent_commands()
    );
    assert!(
        !full.sent_commands().iter().any(|c| c.contains("DisplayTail")),
        "Full mode never queries: {:?}",
        full.sent_commands()
    );
}

/// `/resync` reloads the transcript from the engine's history.
#[test]
fn slash_resync_reloads_transcript_from_engine_history() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.set_history(vec![
        std::sync::Arc::new(serde_json::json!({"role": "user", "content": "from the daemon"})),
        std::sync::Arc::new(serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "reloaded reply"}]})),
    ]);
    h.feed_event(text("stale live text").into());
    let before = h.snapshot();
    assert!(before.contains("stale live text"), "{before}");
    h.run_slash_command("resync", "");
    let after = h.snapshot();
    assert!(after.contains("from the daemon"), "{after}");
    assert!(after.contains("reloaded reply"), "{after}");
    assert!(after.contains("transcript resynced"), "{after}");
    assert!(!after.contains("stale live text"), "{after}");
}
