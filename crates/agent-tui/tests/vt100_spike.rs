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
