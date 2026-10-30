//! The thinking block, end to end: streamed through the production session
//! arm (live header → "thought for 12s"), its spacing against the answer,
//! Ctrl+O expansion, and copy (a selection over the collapsed preview
//! copies the whole thought, markdown markers and hidden rows included;
//! a selection inside a prose row is char-precise).

use agent_engine::runtime::subagent_watch::WatchEvent;
use agent_engine::session::{SessionEventWire, TurnTrigger};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn feed(h: &mut TestHarness, events: Vec<WatchEvent>) {
    for ev in events {
        h.feed_event(SessionEventWire::Stream(ev.to_stream().unwrap()));
    }
}

fn think(h: &mut TestHarness, text: &str) {
    feed(h, vec![WatchEvent::Thinking { text: text.into() }]);
}

/// Steps of a long thought, one per source line.
fn steps(n: usize) -> String {
    (0..n)
        .map(|i| format!("Step {i}: weigh the option, then note **why** it matters."))
        .collect::<Vec<_>>()
        .join("\n")
}

fn drag(h: &mut TestHarness, from: (u16, u16), to: (u16, u16)) {
    let ev = |kind, (column, row): (u16, u16)| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    };
    h.mouse(ev(MouseEventKind::Down(MouseButton::Left), from));
    h.mouse(ev(MouseEventKind::Drag(MouseButton::Left), to));
    h.mouse(ev(MouseEventKind::Up(MouseButton::Left), to));
}

/// A turn that thinks for 12s and then answers: live while it thinks, then
/// "thought for 12s" with a collapsed preview, the answer one row of air
/// below; Ctrl+O shows the thought whole.
#[test]
fn a_streamed_thought_goes_live_then_reports_how_long_it_took() {
    let mut h = TestHarness::boot_with_size(100, 40);
    h.push_user_message("why does the last row vanish?");
    h.feed_event(SessionEventWire::TurnStarted {
        turn_baseline: 1,
        trigger: TurnTrigger::User,
        user_text: None,
    });
    h.set_streaming(true);
    assert!(h.snapshot().contains("\u{2576}\u{2500} \u{25c7} thinking"), "{}", h.snapshot());
    feed(&mut h, vec![WatchEvent::ResponseStart]);
    let text = steps(20);
    for line in text.split_inclusive('\n') {
        think(&mut h, line);
        h.advance_clock_ms(600);
        h.tick_animations(3);
    }
    let frame = h.snapshot();
    assert!(frame.contains("\u{256d}\u{2500} \u{25c7} thinking 12s"), "{frame}");
    assert!(frame.contains("Step 19"), "follows the newest rows:\n{frame}");
    assert!(!frame.contains("**"), "markdown renders:\n{frame}");

    feed(&mut h, vec![WatchEvent::Text { text: "It is the live flag.".into() }]);
    let frame = h.snapshot();
    let lines: Vec<&str> = frame.lines().collect();
    let head = lines
        .iter()
        .position(|l| l.contains("\u{25c7} thought for 12s"))
        .unwrap_or_else(|| panic!("no done header:\n{frame}"));
    let footer = lines
        .iter()
        .position(|l| l.contains("\u{2570}\u{2500} +14 lines \u{00b7} ctrl+o to expand"))
        .unwrap_or_else(|| panic!("no footer:\n{frame}"));
    assert!(lines[head + 1].contains("Step 0:"), "head preview:\n{frame}");
    assert_eq!(lines[footer + 1].trim(), "", "one row of air:\n{frame}");
    assert!(lines[footer + 2].contains("\u{25c8} agent"), "then the answer:\n{frame}");
    assert!(lines[head - 1].trim().is_empty(), "air above:\n{frame}");

    h.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    let frame = h.snapshot();
    assert!(frame.contains("Step 19"), "{frame}");
    assert!(!frame.contains("ctrl+o to expand"), "{frame}");
    assert!(
        frame.lines().any(|l| l.starts_with("    \u{2570}  Step 19")),
        "expanded, closed on its last row:\n{frame}"
    );
}

/// D4 on the rail: selecting the collapsed preview through its footer
/// copies the thought's whole source — the hidden rows and the markdown
/// markers the render consumed — and none of the rail or header.
#[test]
fn copying_a_collapsed_thought_copies_all_of_it() {
    let mut h = TestHarness::boot_with_size(100, 30);
    let text = format!("## Plan\n{}", steps(20));
    h.push_thinking_message(&text);
    let frame = h.snapshot();
    assert!(frame.contains("+15 lines"), "collapsed:\n{frame}");
    drag(&mut h, (1, 2), (98, 27));
    let copied = h.selected_text().expect("a selection over the thought copies");
    assert_eq!(copied, text, "the whole source, verbatim");
}

/// Prose rows are char-precise: the rail swapped two spaces for a glyph of
/// the same width, so a drag that starts mid-row copies from that word.
#[test]
fn a_selection_inside_a_prose_row_is_char_precise() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.push_thinking_message("plain words here\nsecond plain line with more words");
    let frame = h.snapshot();
    let (y, row) = frame
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("second plain"))
        .unwrap_or_else(|| panic!("no prose row:\n{frame}"));
    // Terminal columns: the rail glyph is one cell but three bytes.
    let col = row[..row.find("plain").unwrap()].chars().count() as u16;
    let end = row.chars().count() as u16;
    drag(&mut h, (col, y as u16), (end, y as u16));
    let copied = h.selected_text().expect("prose copies");
    assert_eq!(copied, "plain line with more words");
}
