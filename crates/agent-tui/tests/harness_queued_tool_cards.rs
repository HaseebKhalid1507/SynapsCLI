//! Tool calls that wait behind another call in their scheduler lane.
//!
//! The engine runs side-effecting calls of one response in a serial lane,
//! one after another. A call waiting there used to look like it was already
//! running (sweep + ticking clock from its `tool_use`), and its footer
//! counted the wait: a `sleep 24` that waited 15s finished as `ok 39.0s`.
//! Now the engine sends `ToolQueued` for a call that waits and `ToolStarted`
//! when it begins; the card shows a calm `queued` marker until then and is
//! timed from its start. Driven through the production session arm, the
//! run loop's real tick, and (for worker views) the real watch poll.

use agent_engine::runtime::subagent_watch::{WatchEvent, WatchPage};
use agent_engine::session::SessionEventWire;
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

fn queued(id: &str) -> WatchEvent {
    WatchEvent::ToolQueued { tool_id: id.into() }
}

fn started(id: &str) -> WatchEvent {
    WatchEvent::ToolStarted { tool_id: id.into() }
}

fn result(id: &str) -> WatchEvent {
    WatchEvent::ToolResult {
        tool_id: id.into(),
        result: String::new(),
    }
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

/// The `└─ …` footer row of the card that runs `command`.
fn card_footer(frame: &str, command: &str) -> String {
    let lines: Vec<&str> = frame.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(&format!("command: {command}")))
        .unwrap_or_else(|| panic!("no card for {command:?}:\n{frame}"));
    lines[at..]
        .iter()
        .find(|l| l.contains("\u{2514}\u{2500}"))
        .unwrap_or_else(|| panic!("no footer below {command:?}:\n{frame}"))
        .to_string()
}

/// Let `ms` pass and run a few frames of the run loop's tick.
fn wait(h: &mut TestHarness, ms: u64) {
    h.advance_clock_ms(ms);
    h.tick_animations(3);
}

/// The user test: two bash calls in one response; the second waits in the
/// serial lane while the first runs.
fn two_serial_calls(h: &mut TestHarness) {
    h.set_streaming(true);
    feed(h, vec![WatchEvent::ResponseStart]);
    feed(h, bash_call("s1", "sleep 15"));
    feed(h, bash_call("s2", "sleep 24"));
    feed(h, vec![queued("s2")]);
}

/// A waiting card says `queued` — no sweep, no clock — while the call ahead
/// of it runs with its clock as before.
#[test]
fn a_queued_card_shows_queued_without_a_sweep_or_clock() {
    let mut h = TestHarness::boot_with_size(120, 40);
    two_serial_calls(&mut h);
    wait(&mut h, 5_000);
    let frame = h.snapshot();
    let running = card_header(&frame, "sleep 15");
    let waiting = card_header(&frame, "sleep 24");
    assert!(running.contains("5.0s"), "the first call runs: {running:?}");
    assert!(waiting.contains("queued"), "the second waits: {waiting:?}");
    assert!(
        !waiting.contains("5.0s") && !waiting.contains("ms"),
        "no clock on a queued card: {waiting:?}"
    );

    // Calm: time passing and the sweep's phase moving change nothing on it.
    for _ in 0..6 {
        wait(&mut h, 700);
        let again = card_header(&h.snapshot(), "sleep 24");
        assert_eq!(again, waiting, "a queued card does not animate");
    }
}

/// Once it starts it looks like any running card, and both its clock and
/// its footer count from the start, not from its `tool_use`.
#[test]
fn a_queued_card_is_timed_from_its_execution_start() {
    let mut h = TestHarness::boot_with_size(120, 40);
    two_serial_calls(&mut h);
    wait(&mut h, 15_000);
    feed(&mut h, vec![result("s1"), started("s2")]);
    let frame = h.snapshot();
    let first = card_footer(&frame, "sleep 15");
    assert!(first.contains("ok (no output) 15.0s"), "{first:?}");
    let header = card_header(&frame, "sleep 24");
    assert!(!header.contains("queued"), "it runs now: {header:?}");
    assert!(header.contains("0ms"), "its clock starts at zero: {header:?}");

    wait(&mut h, 2_000);
    let header = card_header(&h.snapshot(), "sleep 24");
    assert!(header.contains("2.0s"), "counts from its start: {header:?}");

    wait(&mut h, 22_000);
    feed(&mut h, vec![result("s2")]);
    let footer = card_footer(&h.snapshot(), "sleep 24");
    assert!(
        footer.contains("ok (no output) 24.0s"),
        "the wait is not billed to it (was 39.0s): {footer:?}"
    );
}

/// The running sweep comes back once the call starts.
#[test]
fn a_started_card_animates_again() {
    let mut h = TestHarness::boot_with_size(120, 40);
    two_serial_calls(&mut h);
    wait(&mut h, 1_000);
    feed(&mut h, vec![result("s1"), started("s2")]);
    let before = card_header(&h.snapshot(), "sleep 24");
    let mut changed = false;
    for _ in 0..8 {
        h.tick_animations(3);
        if card_header(&h.snapshot(), "sleep 24") != before {
            changed = true;
            break;
        }
    }
    assert!(changed, "the sweep never moved after start: {before:?}");
}

/// A client that never sees `queued` keeps today's behaviour: a stray
/// `started` (or none at all) leaves the card timed from its `tool_use`.
#[test]
fn without_queued_a_card_behaves_as_before() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_streaming(true);
    feed(&mut h, vec![WatchEvent::ResponseStart]);
    feed(&mut h, bash_call("n1", "sleep 9"));
    wait(&mut h, 3_000);
    feed(&mut h, vec![started("n1")]);
    wait(&mut h, 1_000);
    let header = card_header(&h.snapshot(), "sleep 9");
    assert!(header.contains("4.0s"), "unchanged clock: {header:?}");
    wait(&mut h, 5_000);
    feed(&mut h, vec![result("n1")]);
    let footer = card_footer(&h.snapshot(), "sleep 9");
    assert!(footer.contains("ok (no output) 9.0s"), "{footer:?}");
}

/// A turn whose queued call never ran (cancelled behind the running one)
/// gets that call's result from the engine; the card settles, not stuck.
#[test]
fn a_queued_card_that_gets_a_result_without_starting_settles() {
    let mut h = TestHarness::boot_with_size(120, 40);
    two_serial_calls(&mut h);
    wait(&mut h, 2_000);
    feed(
        &mut h,
        vec![
            WatchEvent::ToolResult {
                tool_id: "s1".into(),
                result: "Interrupted".into(),
            },
            WatchEvent::ToolResult {
                tool_id: "s2".into(),
                result: "Interrupted".into(),
            },
        ],
    );
    let frame = h.snapshot();
    let header = card_header(&frame, "sleep 24");
    assert!(!header.contains("queued"), "settled: {header:?}");
    h.reset_perf_probe();
    for _ in 0..3 {
        wait(&mut h, 1_000);
        let _ = h.snapshot();
    }
    assert_eq!(h.render_count(), 0, "nothing is live after both results");
}

// ── Worker views (Ctrl+↑) ───────────────────────────────────────────────────

fn start_worker(h: &mut TestHarness, id: u64) {
    h.feed_event(SessionEventWire::Stream(StreamEvent::Agent(
        AgentEvent::SubagentStart {
            subagent_id: id,
            agent_name: "sleeper".into(),
            task_preview: "run two sleeps".into(),
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

/// Answer the focused worker's poll with `p`, stamped like a current
/// daemon's journal (`at_ms` per event, `now_ms` at read).
fn answer_poll_timed(h: &mut TestHarness, p: WatchPage, at_ms: &[u64], now_ms: u64) {
    assert!(h.watch_poll_due(), "a focused worker view polls");
    let id = h.drive_watch_poll().expect("the poll went out");
    let mut value = serde_json::to_value(p).unwrap();
    value["at_ms"] = serde_json::json!(at_ms);
    value["now_ms"] = serde_json::json!(now_ms);
    h.feed_event(SessionEventWire::QueryResult { id, value });
}

/// The whole story in a worker view, applied in one late page: the second
/// card is queued for 15s, then runs 24s; its footer says 24.0s.
#[test]
fn a_worker_views_queued_card_is_timed_from_its_start() {
    let mut h = TestHarness::boot_with_size(120, 40);
    start_worker(&mut h, 6);
    h.key(KeyCode::Up, KeyModifiers::CONTROL);

    // First page, read 5s in: s1 runs, s2 waits.
    let mut events = vec![WatchEvent::ResponseStart];
    events.extend(bash_call("s1", "sleep 15"));
    events.extend(bash_call("s2", "sleep 24"));
    events.push(queued("s2"));
    let at = vec![0, 0, 0, 0, 0, 0];
    answer_poll_timed(&mut h, page("sa_6", events, 6), &at, 5_000);
    let frame = h.snapshot();
    let running = card_header(&frame, "sleep 15");
    let waiting = card_header(&frame, "sleep 24");
    assert!(running.contains("5.0s"), "{running:?}");
    assert!(waiting.contains("queued"), "{waiting:?}");
    assert!(!waiting.contains("5.0s"), "{waiting:?}");

    // Second page, read at 41s: s1 done at 15s, s2 started at 15s and done at 39s.
    h.advance_clock_ms(36_000);
    let events = vec![result("s1"), started("s2"), result("s2")];
    answer_poll_timed(&mut h, page("sa_6", events, 9), &[15_000, 15_000, 39_000], 41_000);
    let frame = h.snapshot();
    assert!(
        card_footer(&frame, "sleep 15").contains("ok (no output) 15.0s"),
        "{frame}"
    );
    let footer = card_footer(&frame, "sleep 24");
    assert!(
        footer.contains("ok (no output) 24.0s"),
        "timed from its start, not 39.0s: {footer:?}"
    );
}
