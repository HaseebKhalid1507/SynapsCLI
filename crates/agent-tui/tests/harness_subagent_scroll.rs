//! Subagent activity must never move a scrolled-up main thread.
//!
//! Drives real subagent envelopes (`SubagentStart`/`Update`/`Done`, a
//! completion wake delivered as steering) through the PRODUCTION session
//! arm while the user is scrolled up, rendering a full frame after each one.
//! The tray appearing/growing/shrinking resizes the transcript body and the
//! events used to invalidate the whole transcript — both moved the view.

use agent_engine::session::SessionEventWire;
use agent_engine::{AgentEvent, StreamEvent};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

fn wheel_up() -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 10,
        modifiers: KeyModifiers::empty(),
    }
}

fn agent(ev: AgentEvent) -> SessionEventWire {
    SessionEventWire::Stream(StreamEvent::Agent(ev))
}

/// Transcript rows of a frame: every row carrying one of the seeded
/// markers, in screen order (header/tray/input chrome never contains them).
fn transcript_rows(frame: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = ["msg-", "detail for", "wrapping", "\u{25C8} agent"];
    frame
        .lines()
        .filter(|l| MARKERS.iter().any(|m| l.contains(m)))
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn scrolled_harness() -> (TestHarness, Vec<String>) {
    let mut h = TestHarness::boot_with_size(100, 40);
    for i in 0..80 {
        h.push_text_message(&format!(
            "msg-{i:03} — the main thread\n\n- detail for {i}\n- {}",
            "wrapping words ".repeat(i % 5 * 4)
        ));
    }
    h.render();
    for _ in 0..6 {
        h.mouse(wheel_up());
    }
    let frame = h.snapshot();
    assert!(!h.scroll_pinned(), "precondition: user scrolled up");
    let rows = transcript_rows(&frame);
    assert!(rows.len() > 8, "precondition: transcript visible:\n{frame}");
    (h, rows)
}

/// The view may lose rows at the bottom (the tray covers them) but the rows
/// it shows must be the same rows, starting from the same top row.
fn assert_same_view(before: &[String], h: &mut TestHarness, what: &str) {
    let frame = h.snapshot();
    let now = transcript_rows(&frame);
    assert!(!now.is_empty(), "{what}: transcript vanished:\n{frame}");
    let n = now.len().min(before.len());
    assert_eq!(
        now[..n],
        before[..n],
        "{what}: subagent activity moved the scrolled-up main view:\n{frame}"
    );
    assert!(
        !h.scroll_pinned(),
        "{what}: view was re-pinned to the bottom"
    );
}

#[test]
fn subagent_lifecycle_never_moves_a_scrolled_up_main_thread() {
    let (mut h, before) = scrolled_harness();

    for id in 1..=3u64 {
        h.feed_event(agent(AgentEvent::SubagentStart {
            subagent_id: id,
            agent_name: format!("worker-{id}"),
            task_preview: "dig through the repo".into(),
        }));
        assert_same_view(&before, &mut h, "SubagentStart");
    }
    for n in 1..=25u64 {
        h.feed_event(agent(AgentEvent::SubagentUpdate {
            subagent_id: 1 + n % 3,
            agent_name: "worker".into(),
            status: format!("\u{2699} bash (tool #{n})"),
        }));
        assert_same_view(&before, &mut h, "SubagentUpdate");
    }
    h.feed_event(agent(AgentEvent::SubagentDone {
        subagent_id: 2,
        agent_name: "worker-2".into(),
        result_preview: "found it".into(),
        duration_secs: 3.5,
    }));
    assert_same_view(&before, &mut h, "SubagentDone");

    // The completion wake rides the steering channel as an <event> payload.
    h.feed_event(agent(AgentEvent::SteeringDelivered {
        message: "<event id=\"x\" type=\"subagent_completion\">worker-2 done</event>".into(),
    }));
    assert_same_view(&before, &mut h, "completion wake (SteeringDelivered)");
}

#[test]
fn user_steering_still_jumps_to_the_bottom() {
    let (mut h, _) = scrolled_harness();
    h.feed_event(agent(AgentEvent::SteeringDelivered {
        message: "actually, use the other approach".into(),
    }));
    h.render();
    assert!(
        h.scroll_pinned(),
        "the user's own steering re-pins the view"
    );
}
