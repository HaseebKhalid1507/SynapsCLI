//! Live elements in the transcript keep animating wherever they are.
//!
//! A running tool card's clock and sweep, and the thinking placeholder's
//! spinner, are redrawn by the animation tick. They must keep moving when
//! something lands below them — a steer, a system notice, an event or
//! subagent completion, or a second tool card running in parallel — in the
//! main thread and in a worker view alike. Driven through the production
//! session arm and the run loop's real tick (`tick_animations`).

use agent_engine::events::types::Event;
use agent_engine::runtime::subagent_watch::{WatchEvent, WatchPage};
use agent_engine::session::{SessionEventWire, TurnTrigger};
use agent_engine::{AgentEvent, StreamEvent};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers};

fn feed(h: &mut TestHarness, events: Vec<WatchEvent>) {
    for ev in events {
        h.feed_event(SessionEventWire::Stream(ev.to_stream().unwrap()));
    }
}

fn bash_call(id: &str, command: &str) -> Vec<WatchEvent> {
    vec![
        WatchEvent::ToolUseStart {
            tool_id: id.into(),
            tool_name: "bash".into(),
        },
        WatchEvent::ToolUse {
            tool_id: id.into(),
            tool_name: "bash".into(),
            input: serde_json::json!({ "command": command }),
        },
    ]
}

/// A main-thread turn that is running `sleep 25` with bash.
fn running_sleep(h: &mut TestHarness) {
    h.set_streaming(true);
    feed(h, vec![WatchEvent::ResponseStart]);
    feed(h, bash_call("m1", "sleep 25"));
}

/// The `❯ bash` header row of the card that runs `command`.
fn card_header(frame: &str, command: &str) -> String {
    let lines: Vec<&str> = frame.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(&format!("command: {command}")))
        .unwrap_or_else(|| panic!("no card for {command:?}:\n{frame}"));
    lines[..at]
        .iter()
        .rev()
        .find(|l| l.contains("\u{276f} bash"))
        .unwrap_or_else(|| panic!("no header above {command:?}:\n{frame}"))
        .to_string()
}

/// Let `ms` pass and run a few frames of the run loop's tick.
fn wait(h: &mut TestHarness, ms: u64) {
    h.advance_clock_ms(ms);
    h.tick_animations(3);
}

fn steered(text: &str) -> SessionEventWire {
    SessionEventWire::Steered {
        text: text.into(),
        delivered: true,
    }
}

/// The issue's repro: a running card followed by a `→ steering:` line. Its
/// clock stayed at the moment the steer landed.
#[test]
fn a_running_card_keeps_counting_after_a_steer_lands_below_it() {
    let mut h = TestHarness::boot_with_size(120, 30);
    running_sleep(&mut h);
    wait(&mut h, 5_400);
    let header = card_header(&h.snapshot(), "sleep 25");
    assert!(header.contains("5.4s"), "{header:?}");

    h.feed_event(steered("also check the logs"));
    let frame = h.snapshot();
    assert!(
        frame.contains("\u{2192} steering: also check the logs"),
        "{frame}"
    );

    wait(&mut h, 3_000);
    let header = card_header(&h.snapshot(), "sleep 25");
    assert!(header.contains("8.4s"), "the clock moved on: {header:?}");
    wait(&mut h, 3_000);
    let header = card_header(&h.snapshot(), "sleep 25");
    assert!(header.contains("11.4s"), "and keeps moving: {header:?}");
}

/// The sweep animates too, not just the clock: two frames apart in time
/// and in spinner phase render a different header.
#[test]
fn a_running_cards_sweep_keeps_moving_after_a_steer_lands_below_it() {
    let mut h = TestHarness::boot_with_size(120, 30);
    running_sleep(&mut h);
    h.feed_event(steered("hurry"));
    let before = card_header(&h.snapshot(), "sleep 25");
    // Same clock reading, later spinner phase: only the sweep can differ.
    let mut changed = false;
    for _ in 0..8 {
        h.tick_animations(3);
        if card_header(&h.snapshot(), "sleep 25") != before {
            changed = true;
            break;
        }
    }
    assert!(changed, "the sweep never moved: {before:?}");
}

/// The other things that land below a running card: a system notice, an
/// event card, and a subagent completion (an event too).
#[test]
fn a_running_card_keeps_counting_under_notices_events_and_completions() {
    let mut completion = Event::simple("subagent", "worker finished", None);
    completion.content.content_type = "subagent_completion".into();
    let below: Vec<(&str, SessionEventWire)> = vec![
        (
            "notice",
            SessionEventWire::SystemNotice("model switched".into()),
        ),
        (
            "event",
            SessionEventWire::External(Event::simple("inbox", "a message arrived", None)),
        ),
        ("completion", SessionEventWire::External(completion)),
    ];
    for (what, ev) in below {
        let mut h = TestHarness::boot_with_size(120, 30);
        running_sleep(&mut h);
        wait(&mut h, 2_000);
        h.feed_event(ev);
        let _ = h.snapshot();
        wait(&mut h, 4_000);
        let header = card_header(&h.snapshot(), "sleep 25");
        assert!(header.contains("6.0s"), "{what}: {header:?}");
    }
}

/// Parallel tool calls: the first card froze as soon as the second was
/// added below it. Each card counts from its own start.
#[test]
fn two_parallel_running_cards_both_keep_counting() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_streaming(true);
    feed(&mut h, vec![WatchEvent::ResponseStart]);
    feed(&mut h, bash_call("p1", "sleep 25"));
    wait(&mut h, 2_000);
    feed(&mut h, bash_call("p2", "sleep 30"));
    let _ = h.snapshot();

    wait(&mut h, 3_000);
    let frame = h.snapshot();
    let first = card_header(&frame, "sleep 25");
    let second = card_header(&frame, "sleep 30");
    assert!(first.contains("5.0s"), "first card: {first:?}");
    assert!(second.contains("3.0s"), "second card: {second:?}");

    wait(&mut h, 4_000);
    let frame = h.snapshot();
    let first = card_header(&frame, "sleep 25");
    let second = card_header(&frame, "sleep 30");
    assert!(first.contains("9.0s"), "first card: {first:?}");
    assert!(second.contains("7.0s"), "second card: {second:?}");
}

/// A card whose output is streaming carries its clock on the result row;
/// that row keeps counting under a steer too.
#[test]
fn a_running_card_with_output_keeps_counting_after_a_steer() {
    let mut h = TestHarness::boot_with_size(120, 30);
    running_sleep(&mut h);
    feed(
        &mut h,
        vec![WatchEvent::ToolResultDelta {
            tool_id: "m1".into(),
            delta: "compiling...\n".into(),
        }],
    );
    wait(&mut h, 1_000);
    h.feed_event(steered("keep going"));
    let _ = h.snapshot();
    wait(&mut h, 4_000);
    let frame = h.snapshot();
    let lines: Vec<&str> = frame.lines().collect();
    let out = lines
        .iter()
        .position(|l| l.contains("compiling..."))
        .unwrap_or_else(|| panic!("no output row:\n{frame}"));
    let clock = lines[out..].iter().find(|l| l.contains("5.0s")).copied();
    assert!(clock.is_some(), "the result row's clock moved on:\n{frame}");
}

/// The `◇ thinking` row as each cell's symbol and colour: the
/// placeholder's live state is a colour sweep over the label, not a glyph
/// spinner.
fn thinking_row(h: &mut TestHarness) -> String {
    let buf = h.render();
    let area = *buf.area();
    let y = (area.top()..area.bottom())
        .find(|&y| (area.left()..area.right()).any(|x| buf[(x, y)].symbol() == "\u{25c7}"))
        .unwrap_or_else(|| panic!("no thinking row"));
    (area.left()..area.right())
        .map(|x| format!("{}{:?}", buf[(x, y)].symbol(), buf[(x, y)].fg))
        .collect()
}

/// The thinking placeholder's shimmer keeps moving when a steer lands
/// under it before the model says anything.
#[test]
fn the_thinking_shimmer_keeps_moving_with_a_steer_below_it() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.feed_event(SessionEventWire::TurnStarted {
        turn_baseline: 1,
        trigger: TurnTrigger::User,
        user_text: None,
    });
    h.feed_event(steered("think harder"));
    assert!(h.snapshot().contains("\u{25c7} thinking"), "{}", h.snapshot());
    let before = thinking_row(&mut h);
    h.tick_animations(12);
    let after = thinking_row(&mut h);
    assert_ne!(before, after, "the shimmer stood still");
}

/// Finished cards stop being live: once every tool is done, ticking
/// re-renders nothing at all.
#[test]
fn a_finished_card_is_not_ticked() {
    let mut h = TestHarness::boot_with_size(120, 30);
    running_sleep(&mut h);
    h.feed_event(steered("noted"));
    wait(&mut h, 25_900);
    feed(
        &mut h,
        vec![WatchEvent::ToolResult {
            tool_id: "m1".into(),
            result: String::new(),
        }],
    );
    let frame = h.snapshot();
    assert!(
        frame.contains("\u{2514}\u{2500} ok (no output) 25.9s"),
        "{frame}"
    );
    let header = card_header(&frame, "sleep 25");
    assert!(
        !header.contains("25.9s"),
        "done: no clock on the header: {header:?}"
    );

    h.reset_perf_probe();
    for _ in 0..4 {
        wait(&mut h, 1_000);
        let _ = h.snapshot();
    }
    assert_eq!(h.render_count(), 0, "nothing is live, nothing re-renders");
}

// ── Worker views ────────────────────────────────────────────────────────────

fn start(h: &mut TestHarness, id: u64, name: &str) {
    h.feed_event(SessionEventWire::Stream(StreamEvent::Agent(
        AgentEvent::SubagentStart {
            subagent_id: id,
            agent_name: name.into(),
            task_preview: "dig through the repo".into(),
        },
    )));
}

fn page(handle: &str, events: Vec<WatchEvent>, next_seq: u64) -> WatchPage {
    WatchPage {
        handle_id: handle.into(),
        subagent_id: handle[3..].parse().unwrap(),
        agent_name: "sleeper".into(),
        model: String::new(),
        status: "running".into(),
        steerable: true,
        next_seq,
        truncated: false,
        more: false,
        events,
        ..WatchPage::default()
    }
}

fn answer_poll(h: &mut TestHarness, p: WatchPage) {
    assert!(h.watch_poll_due(), "a focused worker view polls");
    let id = h.drive_watch_poll().expect("the poll went out");
    h.feed_event(SessionEventWire::QueryResult {
        id,
        value: serde_json::to_value(p).unwrap(),
    });
}

/// The same freeze in a worker view: a steer the worker took in lands
/// under its running card.
#[test]
fn a_running_card_in_a_worker_view_keeps_counting_after_a_message_below_it() {
    let mut h = TestHarness::boot_with_size(120, 30);
    start(&mut h, 4, "sleeper");
    h.key(KeyCode::Up, KeyModifiers::CONTROL);
    let mut events = vec![WatchEvent::ResponseStart];
    events.extend(bash_call("t1", "sleep 25"));
    answer_poll(&mut h, page("sa_4", events, 3));
    wait(&mut h, 2_000);
    let header = card_header(&h.snapshot(), "sleep 25");
    assert!(header.contains("2.0s"), "{header:?}");

    let steer = vec![WatchEvent::Steered {
        text: "look in src too".into(),
    }];
    answer_poll(&mut h, page("sa_4", steer, 4));
    let frame = h.snapshot();
    assert!(frame.contains("look in src too"), "{frame}");

    wait(&mut h, 3_000);
    let header = card_header(&h.snapshot(), "sleep 25");
    assert!(header.contains("5.0s"), "the clock moved on: {header:?}");
}

/// Parallel cards in a worker view both keep counting.
#[test]
fn two_parallel_running_cards_in_a_worker_view_both_keep_counting() {
    let mut h = TestHarness::boot_with_size(120, 40);
    start(&mut h, 5, "sleeper");
    h.key(KeyCode::Up, KeyModifiers::CONTROL);
    let mut events = vec![WatchEvent::ResponseStart];
    events.extend(bash_call("t1", "sleep 25"));
    events.extend(bash_call("t2", "sleep 30"));
    answer_poll(&mut h, page("sa_5", events, 5));
    let _ = h.snapshot();
    wait(&mut h, 4_000);
    let frame = h.snapshot();
    let first = card_header(&frame, "sleep 25");
    let second = card_header(&frame, "sleep 30");
    assert!(first.contains("4.0s"), "first card: {first:?}");
    assert!(second.contains("4.0s"), "second card: {second:?}");
}
