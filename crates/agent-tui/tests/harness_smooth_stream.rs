//! The response start, end to end through the production session arm: the
//! live `◇ thinking` header holds from send until the first content (no
//! blank gap at `ResponseStart`), an omitted-display thought seals to a bare
//! timed header, and smooth streaming reveals a burst of text over several
//! frames (time driven through the injected `TuiClock`), flushing at once
//! on turn end, abort, a tool call, scroll input and the setting going off.

use agent_engine::runtime::subagent_watch::WatchEvent;
use agent_engine::session::{SessionEventWire, TurnTrigger};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers};

const LIVE: &str = "\u{25c7} thinking";
/// One frame at 60 fps.
const FRAME_MS: u64 = 16;

fn stream(h: &mut TestHarness, ev: WatchEvent) {
    h.feed_event(SessionEventWire::Stream(ev.to_stream().unwrap()));
}

fn text(h: &mut TestHarness, t: &str) {
    stream(h, WatchEvent::Text { text: t.into() });
}

fn think(h: &mut TestHarness, t: &str) {
    stream(h, WatchEvent::Thinking { text: t.into() });
}

fn done(h: &mut TestHarness) {
    h.feed_event(SessionEventWire::Stream(agent_engine::StreamEvent::Session(
        agent_engine::SessionEvent::Done,
    )));
}

/// A user turn: prompt, `TurnStarted` (the placeholder), streaming on.
fn send(h: &mut TestHarness) {
    h.push_user_message("explain hash map collisions");
    h.feed_event(SessionEventWire::TurnStarted {
        turn_baseline: 1,
        trigger: TurnTrigger::User,
        user_text: None,
    });
    h.set_streaming(true);
}

/// ~1 KB of answer text, distinct words so a prefix is recognisable.
fn burst() -> String {
    (0..90).map(|i| format!("word{i:02} more ")).collect()
}

/// How much of `full` the main transcript shows as agent text.
fn shown_text(h: &TestHarness) -> usize {
    h.main_transcript_text()
        .lines()
        .filter_map(|l| l.strip_prefix("text: "))
        .map(str::len)
        .sum()
}

/// The placeholder stays the live header through `ResponseStart` (it used
/// to be dropped there, leaving nothing on screen until the first delta),
/// its clock counting from the send; the first thinking delta turns it into
/// the thought in place, the clock running on (no jump back to 0s).
#[test]
fn placeholder_persists_through_response_start_until_content() {
    let mut h = TestHarness::boot_with_size(100, 30);
    send(&mut h);
    assert!(h.snapshot().contains(LIVE), "{}", h.snapshot());
    h.advance_clock_ms(1_400);
    stream(&mut h, WatchEvent::ResponseStart);
    let frame = h.snapshot();
    assert!(frame.contains(LIVE), "the header survives ResponseStart:\n{frame}");
    h.advance_clock_ms(1_600);
    h.tick_animations(3);
    let frame = h.snapshot();
    assert!(frame.contains(&format!("{LIVE} 3s")), "timed from the send:\n{frame}");

    think(&mut h, "Weigh chaining against open addressing.");
    h.advance_clock_ms(1_000);
    h.tick_animations(3);
    let frame = h.snapshot();
    assert!(frame.contains(&format!("{LIVE} 4s")), "in place, clock runs on:\n{frame}");
    assert!(frame.contains("Weigh chaining"), "{frame}");
    assert_eq!(frame.matches('\u{25c7}').count(), 1, "one thinking block:\n{frame}");
}

/// No thinking at all: the header yields to the agent text in the same
/// frame — never a frame with neither.
#[test]
fn without_thinking_the_header_yields_straight_to_the_text() {
    let mut h = TestHarness::boot_with_size(100, 30);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    assert!(h.snapshot().contains(LIVE));
    text(&mut h, "Collisions chain.");
    let frame = h.snapshot();
    assert!(frame.contains("Collisions chain."), "{frame}");
    assert!(!frame.contains('\u{25c7}'), "the placeholder is gone:\n{frame}");
}

/// `thinking_display = omitted`: the block's only delta is empty. The live
/// header stays (timed), then seals to `◇ thought for 8s` — one row, the
/// `╶─` corner, no empty rail — when the text starts.
#[test]
fn an_omitted_thought_is_a_timed_header_without_a_rail() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.set_smooth_streaming(true);
    send(&mut h);
    h.advance_clock_ms(1_400);
    stream(&mut h, WatchEvent::ResponseStart);
    h.advance_clock_ms(5_000);
    think(&mut h, "");
    h.advance_clock_ms(1_600);
    h.tick_animations(3);
    assert!(h.snapshot().contains(&format!("{LIVE} 8s")), "{}", h.snapshot());
    text(&mut h, "Collisions chain.");
    done(&mut h);
    let frame = h.snapshot();
    let head = frame
        .lines()
        .find(|l| l.contains("\u{25c7} thought"))
        .unwrap_or_else(|| panic!("no sealed header:\n{frame}"));
    assert!(head.trim_end().ends_with("\u{2576}\u{2500} \u{25c7} thought for 8s"), "{head:?}");
    assert!(!frame.contains('\u{2502}') && !frame.contains('\u{2570}'), "no rail:\n{frame}");
    assert!(frame.contains("Collisions chain."), "{frame}");
}

/// A 1 KB burst comes out over several frames, growing every frame and
/// starting on the first; it is all out within half a second, exactly the
/// streamed bytes.
#[test]
fn a_burst_is_revealed_over_frames_and_completes() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    let full = burst();
    text(&mut h, &full);
    assert_eq!(shown_text(&h), 0, "queued, not painted");
    h.render();
    let first = shown_text(&h);
    assert!(first > 0 && first < full.len() / 4, "the first frame shows a little: {first}");
    let mut frames = 1;
    let mut last = first;
    while h.reveal_backlog() > 0 {
        h.advance_clock_ms(FRAME_MS);
        h.render();
        frames += 1;
        let now = shown_text(&h);
        assert!(now > last, "grows every frame ({last} → {now})");
        last = now;
        assert!(frames < 64, "never stalls");
    }
    assert!(frames >= 8, "several frames, not one: {frames}");
    assert!(frames as u64 * FRAME_MS <= 450, "caught up in {} ms", frames as u64 * FRAME_MS);
    assert!(h.main_transcript_text().contains(&format!("text: {full}")));
    assert!(h.snapshot().contains("word89 more"));
}

/// Thinking that lands with the answer reads in first: the thought grows
/// while the answer waits, then the answer follows.
#[test]
fn a_thought_arriving_with_the_text_is_paced_ahead_of_it() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    let thought: String = (0..40).map(|i| format!("idea{i:02} ")).collect();
    think(&mut h, &thought);
    text(&mut h, &burst());
    h.render();
    let t = h.main_transcript_text();
    assert!(t.contains("thinking: idea00"), "{t}");
    assert!(!t.contains("text: "), "the answer waits for the thought:\n{t}");
    while h.reveal_backlog() > 0 {
        h.advance_clock_ms(FRAME_MS);
        h.render();
    }
    let t = h.main_transcript_text();
    assert!(t.contains(&format!("thinking: {thought}")), "{t}");
    assert!(t.contains(&format!("text: {}", burst())), "{t}");
}

/// Turn end shows everything queued at once — no clock advance, no render.
#[test]
fn turn_end_flushes_immediately() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    h.render();
    assert!(h.reveal_backlog() > 0);
    done(&mut h);
    assert_eq!(h.reveal_backlog(), 0);
    assert_eq!(shown_text(&h), burst().len(), "{}", h.main_transcript_text());
}

/// An abort lands after the whole text, not in the middle of it.
#[test]
fn abort_flushes_immediately() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    h.render();
    h.feed_event(SessionEventWire::Aborted { context_saved: false });
    assert_eq!(h.reveal_backlog(), 0);
    let t = h.main_transcript_text();
    let at = t.find(&format!("text: {}", burst())).unwrap_or_else(|| panic!("{t}"));
    assert!(t[at..].contains("aborted"), "the abort line follows the text:\n{t}");
}

/// A tool call flushes the text ahead of its card.
#[test]
fn a_tool_call_flushes_the_text_ahead_of_its_card() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    stream(
        &mut h,
        WatchEvent::ToolUseStart { tool_id: "toolu_1".into(), tool_name: "bash".into() },
    );
    assert_eq!(h.reveal_backlog(), 0);
    let t = h.main_transcript_text();
    let text_at = t.find("text: word00").unwrap_or_else(|| panic!("{t}"));
    let tool_at = t.find("tool-start").unwrap_or_else(|| panic!("{t}"));
    assert!(text_at < tool_at, "{t}");
    assert_eq!(shown_text(&h), burst().len());
}

/// Scrolling acts on the whole transcript (flush); typing does not.
#[test]
fn scroll_input_flushes_typing_does_not() {
    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    h.render();
    h.type_str("next");
    assert!(h.reveal_backlog() > 0, "typing leaves the reveal running");
    h.key(KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(h.reveal_backlog(), 0);
    assert_eq!(shown_text(&h), burst().len());
}

/// Setting off: every delta paints as it arrives (the old behaviour);
/// turning it off mid-burst shows what is queued.
#[test]
fn setting_off_paints_deltas_as_they_arrive() {
    let mut h = TestHarness::boot_with_size(120, 40);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    assert_eq!(h.reveal_backlog(), 0, "nothing queued with the setting off");
    assert_eq!(shown_text(&h), burst().len());

    let mut h = TestHarness::boot_with_size(120, 40);
    h.set_smooth_streaming(true);
    send(&mut h);
    stream(&mut h, WatchEvent::ResponseStart);
    text(&mut h, &burst());
    h.render();
    assert!(h.reveal_backlog() > 0);
    h.set_smooth_streaming(false);
    assert_eq!(h.reveal_backlog(), 0);
    assert_eq!(shown_text(&h), burst().len());
}
