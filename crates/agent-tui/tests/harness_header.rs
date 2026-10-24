//! The Synaps header (`src/tui/header.rs`) driven through the PRODUCTION
//! session-event arm: the wordmark and folder on the left, and a state block
//! at the right end whose fill and words name the state.

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

fn header(h: &mut TestHarness) -> String {
    h.snapshot().lines().next().unwrap_or("").to_string()
}

/// The state block's text: what follows the last run of 3+ spaces, without
/// the slab's right cap.
fn block(h: &mut TestHarness) -> String {
    let line = header(h);
    let line = line.trim_end().trim_end_matches('\u{258c}').trim_end();
    line.rsplit("   ").next().unwrap_or("").trim().to_string()
}

fn start(h: &mut TestHarness, id: u64, name: &str) {
    h.feed_event(agent(WireAgentEvent::SubagentStart {
        subagent_id: id,
        agent_name: name.into(),
        task_preview: "work".into(),
    }));
}

fn fail_401(h: &mut TestHarness) {
    h.feed_event(session(WireSessionStreamEvent::Error {
        message: "unauthorized".into(),
        outcome: TurnOutcome::ProviderFailed {
            code: "401".into(),
            correlation_id: "c1".into(),
        },
    }));
}

#[test]
fn idle_session_shows_the_wordmark_folder_label_and_build() {
    let mut h = TestHarness::boot_with_size(100, 30);
    let line = header(&mut h);
    assert!(
        line.contains("SynapsCLI \u{276f} ~/Projects/SynapsCLI \u{b7} test-session"),
        "{line:?}"
    );
    let ver = concat!("v", env!("CARGO_PKG_VERSION"));
    h.set_build_id("ae490b32");
    assert_eq!(
        block(&mut h),
        format!("{ver} \u{b7} ae490b32"),
        "dev build: the hash"
    );
    h.set_build_id("");
    assert_eq!(block(&mut h), ver, "release build: the version alone");
    // The rim under it, from the prompt's left edge to its right edge.
    let edge = h.snapshot().lines().nth(1).unwrap_or("").to_string();
    assert!(edge.starts_with(" \u{259d}\u{2580}"), "{edge:?}");
    assert!(edge.trim_end().ends_with("\u{2580}\u{2598}"), "{edge:?}");
}

#[test]
fn each_phase_of_a_turn_names_itself_with_the_turn_clock() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(turn());
    assert!(block(&mut h).ends_with("waiting 0s"), "{:?}", block(&mut h));
    h.advance_clock_ms(65_000);
    for (ev, want) in [
        (
            llm(WireLlmEvent::Thinking { text: "hmm".into() }),
            "thinking 1m05s",
        ),
        (
            llm(WireLlmEvent::Text { text: "hi".into() }),
            "writing 1m05s",
        ),
        (
            llm(WireLlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "t1".into(),
                input: serde_json::json!({"command": "ls"}),
            }),
            "tools 1m05s",
        ),
        (
            llm(WireLlmEvent::ToolResult {
                tool_id: "t1".into(),
                result: "ok".into(),
            }),
            "waiting 1m05s",
        ),
    ] {
        h.feed_event(ev);
        let b = block(&mut h);
        assert!(b.ends_with(want), "want {want:?}: {b:?}");
    }
    h.feed_event(session(WireSessionStreamEvent::Done));
    let b = block(&mut h);
    assert!(b.starts_with('v'), "back to the build after done: {b:?}");
}

/// 04-tui I4: the retry state comes from the typed retry flag the engine
/// sets where it retries, not from the notice text.
#[test]
fn retrying_follows_the_typed_retry_signal_not_the_text() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(turn());
    for text in [
        "⚠ Rate limited — resuming in 42s (1/10)",
        "⚠ Rate limited — retrying (2/10)",
        "Response incomplete — retrying response (1/3); incomplete output discarded, no tools replayed.",
        "⚠ Gemini rate limited — resuming in 7s (1/5)",
    ] {
        h.feed_event(session(WireSessionStreamEvent::Notice {
            text: text.into(),
            retry: true,
        }));
        let b = block(&mut h);
        assert!(b.contains("retrying"), "{text:?}: {b:?}");
        assert!(h.main_transcript_text().contains(text), "it still prints as a notice");
        h.feed_event(llm(WireLlmEvent::ResponseStart));
        assert!(!block(&mut h).contains("retrying"), "the retry went out");
    }
    h.feed_event(session(WireSessionStreamEvent::Notice {
        text: "⏳ looks like a retry".into(),
        retry: false,
    }));
    assert!(!block(&mut h).contains("retrying"));
}

#[test]
fn a_failed_turn_is_flagged_until_the_next_turn() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(turn());
    fail_401(&mut h);
    assert_eq!(block(&mut h), "\u{2717} 401");
    h.feed_event(turn());
    assert!(
        !block(&mut h).contains('\u{2717}'),
        "a new turn clears the failure"
    );
}

/// 04-tui M3: a failed turn's label must not survive `/new`.
#[test]
fn a_new_session_drops_the_failed_turn_label() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(turn());
    fail_401(&mut h);
    assert_eq!(block(&mut h), "\u{2717} 401");
    h.feed_event(
        W::Cleared {
            session_id: "next".into(),
        }
        .into(),
    );
    assert!(
        !header(&mut h).contains('\u{2717}'),
        "/new clears the failure"
    );
}

#[test]
fn running_workers_show_their_count_in_the_block_or_the_chip() {
    let mut h = TestHarness::boot_with_size(100, 30);
    for id in 1..=3 {
        start(&mut h, id, &format!("w{id}"));
    }
    h.feed_event(agent(WireAgentEvent::SubagentDone {
        subagent_id: 1,
        agent_name: "w1".into(),
        result_preview: "done".into(),
        duration_secs: 1.0,
    }));
    // Main idle: the block itself is the agents' (count of the running).
    let b = block(&mut h);
    assert!(b.contains("2 agents 0s"), "{b:?}");
    // A turn starts: the block is the turn's, the chip carries the agents.
    h.feed_event(turn());
    let line = header(&mut h);
    assert!(line.contains("2 agents 0s"), "chip: {line:?}");
    assert!(block(&mut h).ends_with("waiting 0s"), "{line:?}");
}

#[test]
fn the_reconnect_countdown_shows_in_the_block() {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.set_status_text(Some("reconnecting… (4s left, attempt 2)"));
    assert!(
        block(&mut h).ends_with("reconnecting 4s"),
        "{:?}",
        block(&mut h)
    );
    h.resize(80, 20);
    let line = header(&mut h);
    assert!(line.contains("reconnecting 4s"), "{line:?}");
    assert!(line.contains("SynapsCLI"), "the wordmark stays: {line:?}");
}

#[test]
fn an_observer_sees_observing() {
    let mut h = TestHarness::boot_with_size(100, 30);
    assert!(!header(&mut h).contains("observing"));
    h.set_observing(true);
    assert!(header(&mut h).contains("observing"));
}

/// 04-tui M1: at 80 columns a long worker name ran under the clock. The
/// worker view shows `❯ name` after the session; the block never gives way.
#[test]
fn a_long_worker_name_never_runs_under_the_block_at_80_columns() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut h = TestHarness::boot_with_size(80, 24);
    h.feed_event(turn());
    start(&mut h, 1, "code-reviewer-specialist");
    h.key(KeyCode::Up, KeyModifiers::ALT);
    let line = header(&mut h);
    assert!(line.contains("\u{276f} code-rev"), "{line:?}");
    assert!(
        line.contains("1 agent 0s") || line.contains(" 1 0s "),
        "the chip (count and clock) is intact: {line:?}"
    );
    assert!(
        block(&mut h).ends_with("waiting 0s"),
        "the block is intact: {line:?}"
    );
}

#[test]
fn a_long_session_name_is_cut_at_every_width() {
    for w in [80u16, 100, 120, 160] {
        let mut h = TestHarness::boot_with_size(w, 24);
        h.set_session_name("axon-overnight-cache-fix-followups-round-two-and-three");
        h.feed_event(turn());
        let line = header(&mut h);
        assert!(line.contains("SynapsCLI"), "w={w}: {line:?}");
        assert!(block(&mut h).ends_with("waiting 0s"), "w={w}: {line:?}");
        assert!(!line.contains("and-three"), "w={w}: {line:?}");
    }
}

/// Dumps the real header rows (ANSI, truecolor) in a few states:
/// `cargo test -p synaps-tui --test harness_header dump -- --nocapture`.
#[test]
fn dump_header_rows() {
    let mut grids = Vec::new();
    let mut ansi = |h: &mut TestHarness| -> String {
        let buf = h.render().clone();
        let cells: Vec<Vec<serde_json::Value>> = (0..4)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        let c = &buf[(x, y)];
                        let rgb = |col: ratatui::style::Color| match col {
                            ratatui::style::Color::Rgb(r, g, b) => serde_json::json!([r, g, b]),
                            _ => serde_json::Value::Null,
                        };
                        serde_json::json!([
                            c.symbol(),
                            rgb(c.fg),
                            rgb(c.bg),
                            c.modifier.contains(ratatui::style::Modifier::BOLD)
                        ])
                    })
                    .collect()
            })
            .collect();
        grids.push(cells);
        let mut out = String::new();
        for y in 0..2 {
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                let rgb = |col: ratatui::style::Color| match col {
                    ratatui::style::Color::Rgb(r, g, b) => Some((r, g, b)),
                    _ => None,
                };
                out.push_str("\x1b[0m");
                if c.modifier.contains(ratatui::style::Modifier::BOLD) {
                    out.push_str("\x1b[1m");
                }
                if let Some((r, g, b)) = rgb(c.fg) {
                    out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                }
                if let Some((r, g, b)) = rgb(c.bg) {
                    out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                }
                out.push_str(c.symbol());
            }
            out.push_str("\x1b[0m\n");
        }
        out
    };
    let mut dump = String::new();
    let mut h = TestHarness::boot_with_size(100, 24);
    h.set_build_id("71502686");
    dump += &format!("rest · dev build\n{}", ansi(&mut h));
    start(&mut h, 1, "spike");
    start(&mut h, 2, "shady");
    dump += &format!("main idle · 2 workers\n{}", ansi(&mut h));
    h.feed_event(turn());
    h.advance_clock_ms(12_000);
    h.feed_event(llm(WireLlmEvent::Thinking { text: "hmm".into() }));
    h.set_observing(true);
    dump += &format!("thinking · 2 workers · observing\n{}", ansi(&mut h));
    let mut h = TestHarness::boot_with_size(80, 24);
    h.set_build_id("");
    h.feed_event(turn());
    fail_401(&mut h);
    dump += &format!("failed · release · 80 cols\n{}", ansi(&mut h));
    let mut h = TestHarness::boot_with_size(100, 24);
    h.set_session_name("axon-overnight-cache-fix-followups-round-two");
    h.set_status_text(Some("reconnecting… (4s left, attempt 2)"));
    dump += &format!("reconnecting · long session\n{}", ansi(&mut h));
    println!("{dump}");
    if let Some(path) = std::env::var_os("HEADER_DUMP_JSON") {
        std::fs::write(path, serde_json::to_string(&grids).unwrap()).unwrap();
    }
    assert!(dump.contains("\u{2717}"));
}
