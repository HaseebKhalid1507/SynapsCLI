//! Watching a subagent in its own thread view, end to end through the
//! production key handling, session arm and render path: Ctrl-↑/↓ switch
//! views, the focused worker's journal is paged in by the poll arm, Enter
//! steers the worker, Esc returns to main without cancelling its turn, and
//! the main thread's transcript and scroll position never move.

use agent_engine::runtime::subagent_watch::{WatchEvent, WatchPage};
use agent_engine::session::SessionEventWire;
use agent_engine::{AgentEvent, StreamEvent};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

fn start(h: &mut TestHarness, id: u64, name: &str) {
    h.feed_event(SessionEventWire::Stream(StreamEvent::Agent(
        AgentEvent::SubagentStart {
            subagent_id: id,
            agent_name: name.into(),
            task_preview: "dig through the repo".into(),
        },
    )));
}

fn ctrl(h: &mut TestHarness, code: KeyCode) {
    h.key(code, KeyModifiers::CONTROL);
}

fn page(
    handle: &str,
    name: &str,
    status: &str,
    events: Vec<WatchEvent>,
    next_seq: u64,
) -> WatchPage {
    WatchPage {
        handle_id: handle.into(),
        subagent_id: handle[3..].parse().unwrap(),
        agent_name: name.into(),
        model: String::new(),
        status: status.into(),
        steerable: status == "running",
        next_seq,
        truncated: false,
        more: false,
        events,
    }
}

/// Fire the poll arm and answer it with `p` through the session arm.
fn answer_poll(h: &mut TestHarness, p: WatchPage) {
    assert!(h.watch_poll_due(), "a focused worker view polls");
    let id = h.drive_watch_poll().expect("the poll went out");
    h.feed_event(SessionEventWire::QueryResult {
        id,
        value: serde_json::to_value(p).unwrap(),
    });
}

fn worker_page(n: u64) -> Vec<WatchEvent> {
    vec![
        WatchEvent::ResponseStart,
        WatchEvent::Text {
            text: format!("Reading the files, pass {n}."),
        },
        WatchEvent::ToolUseStart {
            tool_id: format!("t{n}"),
            tool_name: "bash".into(),
        },
        WatchEvent::ToolUse {
            tool_id: format!("t{n}"),
            tool_name: "bash".into(),
            input: serde_json::json!({ "command": "ls crates" }),
        },
        WatchEvent::ToolResult {
            tool_id: format!("t{n}"),
            result: "agent-engine\nagent-tui".into(),
        },
    ]
}

#[test]
fn ctrl_arrows_walk_the_workers_and_back_to_main() {
    let mut h = TestHarness::boot_with_size(100, 30);
    ctrl(&mut h, KeyCode::Up);
    assert_eq!(h.watched(), None, "no workers: stays on main");

    start(&mut h, 1, "scout");
    start(&mut h, 2, "fixer");
    ctrl(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_2"), "newest first");
    ctrl(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_1"));
    ctrl(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_1"), "stops at the oldest");
    ctrl(&mut h, KeyCode::Down);
    assert_eq!(h.watched().as_deref(), Some("sa_2"));
    ctrl(&mut h, KeyCode::Down);
    assert_eq!(h.watched(), None, "past the newest: main");
    assert!(h.take_actions().is_empty(), "switching views sends nothing");
}

#[test]
fn a_worker_view_shows_its_own_stream_and_main_never_moves() {
    let mut h = TestHarness::boot_with_size(100, 30);
    for i in 0..60 {
        h.push_text_message(&format!("main-{i:02} the conversation so far"));
    }
    start(&mut h, 3, "scout"); // the tray is up before the scroll position is taken
    h.render();
    for _ in 0..5 {
        h.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::empty(),
        });
    }
    let main_frame = h.snapshot();
    let main_rows: Vec<&str> = main_frame.lines().filter(|l| l.contains("main-")).collect();
    assert!(!h.scroll_pinned(), "precondition: main scrolled up");
    let main_before = h.main_transcript_text();

    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_3", "scout", "running", worker_page(1), 5));
    let frame = h.snapshot();
    assert!(frame.contains("Reading the files, pass 1."), "{frame}");
    assert!(
        !frame.contains("main-"),
        "the body is the worker's thread:\n{frame}"
    );
    let header = frame.lines().next().unwrap();
    assert!(
        header.contains("scout") && header.contains("sa_3") && header.contains("running"),
        "the header says whose thread this is: {header}"
    );

    // More arrives while the worker is watched; main gets none of it.
    answer_poll(&mut h, page("sa_3", "scout", "running", worker_page(2), 10));
    let worker = h.worker_transcript_text("sa_3");
    assert!(
        worker.contains("pass 1.") && worker.contains("pass 2."),
        "{worker}"
    );
    assert_eq!(
        h.main_transcript_text(),
        main_before,
        "main transcript untouched"
    );

    // Back on main: exactly where the user left it.
    ctrl(&mut h, KeyCode::Down);
    let frame = h.snapshot();
    let rows: Vec<&str> = frame.lines().filter(|l| l.contains("main-")).collect();
    assert_eq!(rows, main_rows, "main view unmoved:\n{frame}");
    assert!(!h.scroll_pinned());
    assert!(!h.watch_poll_due(), "no polling while main is in view");
}

#[test]
fn enter_steers_the_watched_worker_and_slash_commands_still_run() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 4, "scout");
    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_4", "scout", "running", vec![], 0));

    h.type_str("check the tests first");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        h.take_actions(),
        ["subagent-steer:sa_4:check the tests first"],
        "Enter in a worker view steers that worker"
    );
    assert_eq!(h.input_contents(), "", "the box is cleared like a submit");

    // The worker takes the steer in: it shows as the user's message there.
    answer_poll(
        &mut h,
        page(
            "sa_4",
            "scout",
            "running",
            vec![WatchEvent::Steered {
                text: "check the tests first".into(),
            }],
            1,
        ),
    );
    assert!(h
        .worker_transcript_text("sa_4")
        .contains("user: check the tests first"));
    assert!(!h.main_transcript_text().contains("check the tests first"));

    h.type_str("/watch main");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.take_actions(), ["slash:watch:main"]);
    h.drive_slash_commands();
    assert_eq!(h.watched(), None);
}

#[test]
fn a_finished_worker_view_is_read_only() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.run_slash_command("watch", "sa_5");
    answer_poll(
        &mut h,
        page(
            "sa_5",
            "scout",
            "completed",
            vec![
                WatchEvent::Text {
                    text: "All done.".into(),
                },
                WatchEvent::Done,
            ],
            2,
        ),
    );
    assert!(!h.watch_poll_due(), "drained: no more polling");
    h.type_str("one more thing");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(h.take_actions().is_empty(), "nothing is sent");
    assert_eq!(h.input_contents(), "one more thing", "the text stays");
    let worker = h.worker_transcript_text("sa_5");
    assert!(worker.contains("its view is read-only"), "{worker}");
    // 04-tui M5: the opening line no longer promises "Enter steers it".
    assert!(!worker.contains("Enter steers"), "{worker}");
    assert!(h.snapshot().lines().next().unwrap().contains("read-only"));
}

#[test]
fn esc_returns_to_main_without_cancelling_its_turn() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 6, "scout");
    h.set_streaming(true);
    ctrl(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_6"));

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(h.watched(), None, "Esc goes back to main");
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        !h.take_actions().iter().any(|a| a == "abort"),
        "neither Esc (nor a quick double tap) aborts the main turn"
    );
}

#[test]
fn a_refused_steer_stays_in_the_worker_view() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 7, "scout");
    h.set_streaming(true);
    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_7", "scout", "running", vec![], 0));
    h.type_str("hijack");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.take_actions(), ["subagent-steer:sa_7:hijack"]);
    // The engine refuses a non-owner.
    h.feed_event(SessionEventWire::Refused {
        client: agent_engine::session::ClientId(1),
        command: "subagent_steer".into(),
        reason: "input owned by client #2".into(),
    });
    let worker = h.worker_transcript_text("sa_7");
    assert!(
        worker.contains("refused: input owned by client #2"),
        "{worker}"
    );
    assert!(
        !h.main_transcript_text().contains("refused"),
        "main is not told about it"
    );
    assert_eq!(h.input_contents(), "hijack", "the text comes back");
}

#[test]
fn clicking_a_tray_row_opens_that_workers_view() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 1, "scout");
    start(&mut h, 2, "fixer");
    let frame = h.snapshot();
    let row = frame
        .lines()
        .position(|l| l.contains("scout") && l.contains("starting"))
        .expect("scout's tray row") as u16;
    h.mouse(MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 8,
        row,
        modifiers: KeyModifiers::empty(),
    });
    assert_eq!(h.watched().as_deref(), Some("sa_1"));
    let header = h.snapshot().lines().next().unwrap().to_string();
    assert!(header.contains("scout"), "{header}");
}

/// 04-tui I1: with the main turn streaming, Shift/Alt+Enter in a worker view
/// used to fall through to the main thread's streaming submit and steer the
/// MAIN agent. In a worker view they only ever add a line; nothing goes to
/// main.
#[test]
fn shift_or_alt_enter_in_a_worker_view_never_steers_main() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 8, "scout");
    h.set_streaming(true);
    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_8", "scout", "running", vec![], 0));

    h.type_str("check the tests first");
    h.key(KeyCode::Enter, KeyModifiers::ALT);
    h.type_str("then the docs");
    h.key(KeyCode::Enter, KeyModifiers::SHIFT);
    assert!(h.take_actions().is_empty(), "nothing is sent anywhere");
    assert_eq!(
        h.input_contents(),
        "check the tests first\nthen the docs\n",
        "both add a line"
    );

    h.key(KeyCode::Enter, KeyModifiers::NONE);
    let actions = h.take_actions();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(
        actions[0].starts_with("subagent-steer:sa_8:check the tests first"),
        "Enter steers the worker: {actions:?}"
    );
}

/// 04-tui I1: an unknown `/word` typed in a worker view while main streams
/// used to reach the main thread's streaming path, which steers main with
/// it. It is refused in the worker view instead and the text stays.
#[test]
fn an_unknown_slash_in_a_worker_view_is_refused_there_not_sent_to_main() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 9, "scout");
    h.set_streaming(true);
    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_9", "scout", "running", vec![], 0));

    h.type_str("/zzz-not-a-command please");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(h.take_actions().is_empty(), "nothing is sent");
    assert_eq!(h.input_contents(), "/zzz-not-a-command please");
    let worker = h.worker_transcript_text("sa_9");
    assert!(worker.contains("unknown command: /zzz-not-a-command"), "{worker}");
    assert!(!h.main_transcript_text().contains("zzz"));
}

/// 04-tui I2: a slash command typed in a worker view printed its output into
/// the hidden main transcript and snapped main to the bottom. Its output now
/// shows in the worker view, and main is exactly where it was left.
#[test]
fn a_slash_command_in_a_worker_view_answers_there_and_main_never_moves() {
    let mut h = TestHarness::boot_with_size(100, 30);
    for i in 0..60 {
        h.push_text_message(&format!("main-{i:02} the conversation so far"));
    }
    start(&mut h, 10, "scout");
    h.render();
    for _ in 0..5 {
        h.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::empty(),
        });
    }
    let main_frame = h.snapshot();
    let main_rows: Vec<String> = main_frame
        .lines()
        .filter(|l| l.contains("main-"))
        .map(str::to_string)
        .collect();
    assert!(!h.scroll_pinned(), "precondition: main scrolled up");
    let main_before = h.main_transcript_text();

    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_10", "scout", "running", vec![], 0));
    h.type_str("/zzz-no-such-command");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    h.drive_slash_commands();
    let worker = h.worker_transcript_text("sa_10");
    assert!(
        worker.contains("unknown command: /zzz-no-such-command"),
        "the answer is in the view it was typed in: {worker}"
    );
    assert_eq!(h.main_transcript_text(), main_before, "main gets nothing");

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    let frame = h.snapshot();
    let rows: Vec<String> = frame
        .lines()
        .filter(|l| l.contains("main-"))
        .map(str::to_string)
        .collect();
    assert_eq!(rows, main_rows, "main view unmoved:\n{frame}");
    assert!(!h.scroll_pinned(), "main was not snapped to the bottom");
}

fn alt(h: &mut TestHarness, code: KeyCode) {
    h.key(code, KeyModifiers::ALT);
}

/// 04-tui I7: macOS takes Ctrl-↑/↓ for Mission Control before the terminal
/// sees them. Alt-↑/↓ switch views the same way.
#[test]
fn alt_arrows_walk_the_workers_and_back_to_main() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 1, "scout");
    start(&mut h, 2, "fixer");
    alt(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_2"), "newest first");
    alt(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_1"));
    alt(&mut h, KeyCode::Down);
    assert_eq!(h.watched().as_deref(), Some("sa_2"));
    alt(&mut h, KeyCode::Down);
    assert_eq!(h.watched(), None, "past the newest: main");
    assert!(h.take_actions().is_empty(), "switching views sends nothing");
}

/// 04-tui M10: at either end there was no feedback — Up on the oldest
/// re-focused it silently, Down on main did nothing.
#[test]
fn switching_past_either_end_says_so() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 1, "scout");
    alt(&mut h, KeyCode::Up);
    alt(&mut h, KeyCode::Up);
    assert_eq!(h.watched().as_deref(), Some("sa_1"));
    let frame = h.snapshot();
    assert!(frame.contains("oldest subagent"), "{frame}");

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    alt(&mut h, KeyCode::Down);
    assert_eq!(h.watched(), None);
    let frame = h.snapshot();
    assert!(frame.contains("already on the main thread"), "{frame}");
}

/// 04-tui I7: the footer advertises a binding macOS doesn't swallow.
#[test]
fn the_footer_advertises_alt_arrows_for_agents() {
    let mut h = TestHarness::boot_with_size(160, 30);
    start(&mut h, 1, "scout");
    let frame = h.snapshot();
    let footer = frame.lines().last().unwrap_or("");
    assert!(footer.contains("alt+\u{2191}\u{2193} agents"), "{footer:?}");
    alt(&mut h, KeyCode::Up);
    let frame = h.snapshot();
    let footer = frame.lines().last().unwrap_or("");
    assert!(footer.contains("alt+\u{2191}\u{2193} agents"), "{footer:?}");
}

/// prG R1: with main idle, a bare `/` (no command word) in a worker view
/// fell through to process_submit, which only treats `/x` (len > 1) as a
/// command, and was sent to MAIN as a prompt. It is refused in the view.
#[test]
fn a_bare_slash_in_a_worker_view_is_refused_there_not_sent_to_main() {
    let mut h = TestHarness::boot_with_size(100, 30);
    start(&mut h, 11, "scout");
    ctrl(&mut h, KeyCode::Up);
    answer_poll(&mut h, page("sa_11", "scout", "running", vec![], 0));
    let main_before = h.main_transcript_text();
    for typed in ["/", "/   "] {
        h.type_str(typed);
        h.key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(h.take_actions().is_empty(), "{typed:?}: nothing is sent");
        assert_eq!(h.input_contents(), typed, "the text stays");
        h.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
    }
    let worker = h.worker_transcript_text("sa_11");
    assert!(worker.contains("unknown command: /"), "{worker}");
    assert_eq!(h.main_transcript_text(), main_before, "main gets nothing");
}

/// prG review: the footer names `/watch` too, the keyless way in (no
/// arrow chord at all) when the terminal swallows both chords.
#[test]
fn the_footer_names_slash_watch_when_there_are_workers() {
    let mut h = TestHarness::boot_with_size(160, 30);
    let frame = h.snapshot();
    assert!(!frame.lines().last().unwrap_or("").contains("/watch"), "no workers: no hint");
    start(&mut h, 1, "scout");
    let frame = h.snapshot();
    let footer = frame.lines().last().unwrap_or("");
    assert!(footer.contains("/watch list"), "{footer:?}");
    alt(&mut h, KeyCode::Up);
    let frame = h.snapshot();
    let footer = frame.lines().last().unwrap_or("");
    assert!(footer.contains("/watch list"), "{footer:?}");
}
