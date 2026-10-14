//! Ghost header (`src/tui/header.rs`) driven through the PRODUCTION
//! session-event arm: the right side names the turn's state with a label and
//! an indicator whose shape is the state; an idle session shows nothing there.

use agent_engine::session::wire::{
    WireAgentEvent, WireLlmEvent, WireSessionEvent as W, WireSessionStreamEvent, WireStreamEvent,
};
use agent_engine::session::{SessionEventWire, TurnTrigger};
use agent_engine::TurnOutcome;
use agent_tui::tui::testing::TestHarness;

fn llm(e: WireLlmEvent) -> SessionEventWire {
    W::Stream {
        event: WireStreamEvent::Llm(e),
    }
    .into()
}
fn session(e: WireSessionStreamEvent) -> SessionEventWire {
    W::Stream {
        event: WireStreamEvent::Session(e),
    }
    .into()
}
fn agent(e: WireAgentEvent) -> SessionEventWire {
    W::Stream {
        event: WireStreamEvent::Agent(e),
    }
    .into()
}
fn turn() -> SessionEventWire {
    W::TurnStarted {
        turn_baseline: 1,
        trigger: TurnTrigger::EventAuto,
        user_text: None,
    }
    .into()
}

/// Every glyph any indicator can draw.
const INDICATOR_GLYPHS: &[char] = &[
    '●', '·', '∘', '○', '◯', '•', '━', '─', '⎽', '⎼', '⎻', '⎺', '▁', '▂', '▃', '▄', '▅', '▆', '▇',
];

fn header(h: &mut TestHarness) -> String {
    h.snapshot().lines().next().unwrap_or("").to_string()
}

/// The header's right-hand cluster: the text after the last run of 3+
/// spaces (the gap between the centred session label and the state).
fn right(h: &mut TestHarness) -> String {
    let line = header(h);
    let line = line.trim_end();
    line.rsplit("   ").next().unwrap_or("").trim().to_string()
}

fn has_any(s: &str, glyphs: &[char]) -> bool {
    s.chars().any(|c| glyphs.contains(&c))
}

#[test]
fn idle_session_shows_only_the_wordmark_and_label() {
    let mut h = TestHarness::boot_with_size(100, 30);
    let line = header(&mut h);
    assert!(line.contains("synaps"), "wordmark missing: {line:?}");
    assert!(!line.contains("0:0"), "idle header shows a clock: {line:?}");
    assert!(
        !line.contains("ready"),
        "Ghost drops the 'ready' text: {line:?}"
    );
    let after_wordmark = &line[line.find("synaps").unwrap_or(0) + "synaps".len()..];
    assert!(
        !has_any(after_wordmark, INDICATOR_GLYPHS),
        "idle header shows an indicator: {line:?}"
    );
}

#[test]
fn each_phase_of_a_turn_has_its_own_indicator() {
    let mut h = TestHarness::boot_with_size(100, 30);

    // Turn started, no first token: clock + dots.
    h.feed_event(turn());
    let r = right(&mut h);
    assert!(r.starts_with("0:00"), "turn clock missing: {r:?}");
    assert!(r.contains("•••"), "connecting should show dots: {r:?}");

    // The clock follows the (test) wall clock.
    h.advance_clock_ms(65_000);
    assert!(
        right(&mut h).starts_with("1:05"),
        "clock: {:?}",
        right(&mut h)
    );

    // Thinking: one breathing dot.
    h.feed_event(llm(WireLlmEvent::Thinking { text: "hmm".into() }));
    let r = right(&mut h);
    assert!(r.ends_with('●'), "thinking should breathe: {r:?}");

    // Writing: the scope.
    h.feed_event(llm(WireLlmEvent::Text {
        text: "hello".into(),
    }));
    let r = right(&mut h);
    assert!(
        has_any(&r, &['⎽', '⎼', '⎻', '⎺']),
        "writing should show the scope: {r:?}"
    );

    // A tool call is dispatched: the scanner.
    h.feed_event(llm(WireLlmEvent::ToolUse {
        tool_name: "bash".into(),
        tool_id: "t1".into(),
        input: serde_json::json!({"command": "ls"}),
    }));
    let r = right(&mut h);
    assert!(r.contains('━'), "tool should show the scanner: {r:?}");

    // Its result is back: waiting on the next request again.
    h.feed_event(llm(WireLlmEvent::ToolResult {
        tool_id: "t1".into(),
        result: "ok".into(),
    }));
    assert!(
        right(&mut h).contains("•••"),
        "after results: {:?}",
        right(&mut h)
    );

    // The provider pushed back: retrying + ripple.
    h.feed_event(session(WireSessionStreamEvent::Notice {
        text: "⏳ API error, retrying (1/3)…".into(),
    }));
    let r = right(&mut h);
    assert!(r.starts_with("retrying"), "retry label missing: {r:?}");

    // Done: back to an empty right side.
    h.feed_event(session(WireSessionStreamEvent::Done));
    let line = header(&mut h);
    assert!(
        !line.contains("0:0") && !line.contains("1:0"),
        "clock after done: {line:?}"
    );
    assert!(
        !line.contains("retrying"),
        "retry label after done: {line:?}"
    );
}

#[test]
fn a_failed_turn_is_flagged_until_the_next_turn() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(turn());
    h.feed_event(session(WireSessionStreamEvent::Error {
        message: "unauthorized".into(),
        outcome: TurnOutcome::ProviderFailed {
            code: "401".into(),
            correlation_id: "c1".into(),
        },
    }));
    let r = right(&mut h);
    assert_eq!(r, "✗ 401", "failure label: {r:?}");

    h.feed_event(turn());
    let r = right(&mut h);
    assert!(!r.contains('✗'), "a new turn clears the failure: {r:?}");
}

#[test]
fn running_workers_show_done_over_total_and_one_bar_each() {
    let mut h = TestHarness::boot_with_size(100, 30);
    for id in 1..=3 {
        h.feed_event(agent(WireAgentEvent::SubagentStart {
            subagent_id: id,
            agent_name: format!("w{id}"),
            task_preview: "work".into(),
        }));
    }
    h.feed_event(agent(WireAgentEvent::SubagentDone {
        subagent_id: 1,
        agent_name: "w1".into(),
        result_preview: "done".into(),
        duration_secs: 1.0,
    }));
    let r = right(&mut h);
    assert!(r.starts_with("1/3"), "worker count: {r:?}");
    let bars: Vec<char> = r
        .chars()
        .filter(|c| ['▁', '▂', '▃', '▄', '▅', '▆', '▇'].contains(c))
        .collect();
    assert_eq!(bars.len(), 3, "one bar per worker: {r:?}");
    assert_eq!(bars[0], '▁', "the finished worker's bar lies flat: {r:?}");
}
