//! P5 de-risking spike — escape-level render verification via vt100.
//!
//! Proves ONE capture path end-to-end: frame content rendered through
//! `CrosstermBackend<Vec<u8>>` (the production backend, in-memory `Write`
//! sink), parsed by `vt100::Parser`, asserted on the *parsed* screen grid.
//! This is the textual-rs lesson applied: `TestBackend` buffer tests can
//! pass while the live ANSI stream is broken; this path exercises the real
//! escape emission.
//!
//! # Scoping note for the full P5 rig
//!
//! ## What the easy path (this file) CAN verify
//!
//! - Everything crossterm emits while flushing a `terminal.draw()` diff:
//!   cursor positioning (CUP), SGR styling, content bytes, cursor
//!   hide/show. Any bug where the backend emits wrong/mis-ordered
//!   positioning or styling escapes for frame content is catchable here.
//! - Diff behavior across successive draws (render twice into the same
//!   backend, assert the second flush only repaints changed regions).
//! - **Edge scrub is capturable with modest effort**: contrary to the
//!   working assumption, `viewport::scrub_crossterm_terminal_edges` is
//!   generic over `W: Write` and queues `MoveTo`/`Print` *through the
//!   backend*, not raw stdout. A test that builds
//!   `Terminal<CrosstermBackend<Vec<u8>>>` can call it directly and assert
//!   the scrub sequences at the vt100 level. Only `render_frame` (the
//!   composed scrub+draw) is hardcoded to `Stdout`.
//!
//! ## What it CANNOT verify
//!
//! - **Lifecycle sequences**: `lifecycle.rs` writes alt-screen
//!   enter/leave, kitty keyboard-enhancement push/pop, and the emergency
//!   teardown sequence via `execute!(io::stdout(), …)` — bypassing any
//!   injectable backend. Same for the render thread's teardown path
//!   (`do_teardown` → `lifecycle::emergency_teardown_terminal`).
//! - **P1 sync brackets (2026)**: as of this branch no
//!   `BeginSynchronizedUpdate`/`EndSynchronizedUpdate` exists in the crate
//!   yet. When P1 lands, whether it's capturable depends on where it's
//!   written: through the backend (capturable) or raw stdout (not).
//! - **Real-TTY behaviors**: anything conditional on `is_tty`, terminal
//!   size queries, or the actual fd (nothing observed in the render path,
//!   but lifecycle probes keyboard-enhancement support).
//!
//! ## Recommendation for capturing the rest
//!
//! Prefer **(a) an injectable `Write` sink** over (b) fd-level stdout
//! capture. Concretely: make `lifecycle.rs` functions and
//! `render_frame`/`do_teardown` generic over `W: Write + ?Sized` (or take
//! `&mut dyn Write`), with production passing `io::stdout()`. The
//! type-plumbing is mechanical — `render_thread.rs` becomes
//! `Terminal<CrosstermBackend<W>>` end-to-end, which it already almost is
//! (`spawn_render_thread` is the only place `Stdout` is named as a
//! concrete type besides `render_frame`/`do_teardown` signatures).
//! fd-level capture (dup2 tricks) is process-global, races with test
//! parallelism, breaks under `cargo test`'s output capture, and can't
//! isolate which subsystem emitted what. Reject it.
//!
//! ## Full-rig estimate (5 sequence categories from the plan)
//!
//! 1. **Edge scrub** — small: capturable today via the generic
//!    `scrub_crossterm_terminal_edges` (see above). ~½ day incl. asserts.
//! 2. **2026 sync brackets** — blocked on P1 landing; if P1 writes through
//!    the backend, ~½ day. If through raw stdout, needs the W-injection
//!    refactor first.
//! 3. **Alt-screen enter/leave** + 4. **kitty push/pop** + 5. **teardown
//!    completeness** — all live in `lifecycle.rs` behind hardcoded
//!    `io::stdout()`; all three unlock together with one `W: Write`
//!    injection refactor (~1 day incl. not regressing the panic-hook /
//!    emergency-teardown paths, which must keep a real stdout fallback),
//!    then ~½–1 day of vt100 assertions across the three categories.
//!
//! Total: **~2–3 days**, matching the plan's estimate — provided P1 has
//! landed. The refactor is the bulk; the assertions are cheap once bytes
//! are capturable. The `render_ansi()` seam added to `TestHarness` in this
//! spike is the pattern the rig should extend, not replace.

use agent_tui::tui::testing::TestHarness;

/// End-to-end proof: harness frame → real CrosstermBackend ANSI bytes →
/// vt100 parse → assertions on the parsed screen grid.
#[test]
fn vt100_parses_captured_frame_content() {
    let mut h = TestHarness::boot(); // 80x24
    h.type_str("hello vt100");

    let bytes = h.render_ansi();
    assert!(
        !bytes.is_empty(),
        "CrosstermBackend<Vec<u8>> captured no bytes"
    );
    // Sanity: the stream contains real escape sequences, not just text.
    assert!(
        bytes.contains(&0x1b),
        "captured stream contains no ESC bytes — not an ANSI stream"
    );

    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(&bytes);
    let screen = parser.screen();

    // Header chrome lands on row 0 of the *parsed* terminal.
    let row0: String = (0..80)
        .filter_map(|col| screen.cell(0, col))
        .map(|c| c.contents())
        .collect();
    assert!(
        row0.contains("Synaps"),
        "header not on parsed row 0: {row0:?}"
    );

    // Typed input appears in the lower quarter of the parsed screen
    // (input box sits above the status bar; exact row depends on layout).
    let lower: String = (18..24)
        .map(|row| {
            (0..80)
                .filter_map(|col| screen.cell(row, col))
                .map(|c| c.contents())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lower.contains("hello vt100"),
        "typed input not in parsed bottom rows:\n{lower}"
    );

    // Whole-screen check mirrors the P4 smoke test at the escape level.
    let contents = screen.contents();
    assert!(
        contents.contains("ready"),
        "ready status missing from parsed screen:\n{contents}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// P16.2 — DA1-fenced boot query burst, at the byte/escape level.
//
// Same lesson as the render spike: assert on the REAL bytes the production
// writer emits (write_query_burst is exactly what run_setup flushes to the
// tty), and on the parsed result of a realistic synthetic reply stream —
// not on an abstraction of either.
// ─────────────────────────────────────────────────────────────────────────────

use agent_tui::tui::testing::termcaps::{
    parse_burst_replies, write_query_burst, TermCaps, QUERY_BURST,
};

/// The burst as emitted: all five capability queries present, DA1 strictly
/// LAST (the fence — in-band ordering is what makes "no reply by fence time"
/// mean "unsupported"), one flushed write.
#[test]
fn termcaps_burst_emits_da1_fenced_query_bytes() {
    let mut sink: Vec<u8> = Vec::new();
    write_query_burst(&mut sink).expect("write into Vec cannot fail");
    assert_eq!(
        sink.as_slice(),
        QUERY_BURST,
        "production writer must emit the canonical burst verbatim"
    );

    let s = String::from_utf8(sink).expect("burst is pure ASCII");
    // Every query present…
    assert!(s.contains("\x1b[>0q"), "XTVERSION query missing: {s:?}");
    assert!(s.contains("\x1b[?2026$p"), "DECRQM 2026 (sync output) query missing: {s:?}");
    assert!(s.contains("\x1b[?2027$p"), "DECRQM 2027 (unicode width) query missing: {s:?}");
    assert!(s.contains("\x1b[?u"), "kitty keyboard query missing: {s:?}");
    assert!(s.contains("\x1b[>c"), "DA2 query missing: {s:?}");
    // …and the fence is last, exactly once.
    assert!(s.ends_with("\x1b[c"), "DA1 must be the FINAL query (the fence): {s:?}");
    assert_eq!(s.matches("\x1b[c").count(), 1, "exactly one DA1 in the burst");
}

/// The burst must be invisible: feeding the query bytes through a real vt100
/// terminal leaves the screen blank — queries render nothing and move nothing.
#[test]
fn termcaps_burst_is_invisible_on_a_vt100_screen() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(QUERY_BURST);
    let contents = parser.screen().contents();
    assert!(
        contents.trim().is_empty(),
        "query burst painted visible cells: {contents:?}"
    );
    assert_eq!(parser.screen().cursor_position(), (0, 0), "burst moved the cursor");
}

/// A realistic kitty-style answer stream (XTVERSION DCS, kitty flags,
/// DECRPM 2026/2027, DA2, DA1 fence) parses into fact-based TermCaps.
#[test]
fn termcaps_synthetic_da1_reply_parses_into_caps() {
    let replies: &[u8] =
        b"\x1bP>|kitty(0.32.2)\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?2027;1$y\x1b[>1;4000;13c\x1b[?62;22c";

    let parsed = parse_burst_replies(replies);
    assert!(parsed.da1, "DA1 fence not detected");
    assert!(parsed.kitty, "kitty flags reply not detected");
    assert_eq!(parsed.mode_2026, Some(2));
    assert_eq!(parsed.mode_2027, Some(1));

    let mut caps = TermCaps::default();
    caps.merge_burst(&parsed);
    assert!(caps.da1_answered);
    assert!(caps.kitty_keyboard, "kitty reply must flip kitty_keyboard to fact-true");
    assert!(caps.sync_output, "DECRPM 2 (reset, settable) counts as supported");
    assert!(caps.mode_2027, "DECRPM 1 (set) counts as supported");
}

/// The safety property behind the boot-hang gate: WITHOUT the DA1 fence
/// (timeout / dumb terminal / partial replies) the merge is a strict no-op —
/// caps stay at the env-detected defaults and boot behaves exactly as today.
#[test]
fn termcaps_unfenced_partial_replies_leave_defaults_untouched() {
    let parsed = parse_burst_replies(b"\x1b[?1u\x1b[?2026;1$y"); // no DA1 reply
    assert!(!parsed.da1);

    let mut caps = TermCaps::default();
    let before = caps.clone();
    caps.merge_burst(&parsed);
    assert_eq!(caps, before, "unfenced replies must be discarded wholesale");
    assert!(!caps.da1_answered);
}
