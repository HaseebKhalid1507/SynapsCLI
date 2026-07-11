use std::io::{self, Write};

use crossterm::{cursor::MoveTo, style::Print, QueueableCommand};
#[cfg(test)]
use ratatui::backend::Backend;
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    Terminal,
};
#[cfg(test)]
use ratatui::{text::Line, widgets::{Paragraph, Widget}};

/// Terminal cells that should be physically blanked before each diff draw.
///
/// Some terminals/tmux combinations can leave stale glyphs in the first or last
/// column when the pane scrolls by one row outside ratatui's buffered model. The
/// diff renderer may believe those edge cells are already blank and skip writing
/// them, so we proactively scrub the physical edge columns.
pub(crate) fn edge_scrub_positions(area: Rect) -> Vec<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut positions = Vec::with_capacity(area.height as usize * if area.width > 1 { 2 } else { 1 });
    for y in area.y..area.y.saturating_add(area.height) {
        positions.push((area.x, y));
        if area.width > 1 {
            positions.push((area.x + area.width - 1, y));
        }
    }
    positions
}

/// Clear edge columns in ratatui's inactive back buffer before rendering.
///
/// This makes the next diff pass emit blanks for edge cells even when ratatui's
/// previous-frame model already thinks those cells are blank. That is the stale
/// state seen after external terminal/pane scrolling: the physical terminal has
/// residue, but the diff buffer does not.
pub(crate) fn scrub_edge_columns_in_buffer(buf: &mut Buffer, area: Rect, style: Style) {
    for (x, y) in edge_scrub_positions(area) {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.reset();
            cell.set_style(style);
        }
    }
}

/// Compute the row range that is safe to physically scrub without touching the
/// input/footer area. The input height is dynamic, so this must be derived per
/// frame rather than using a fixed bottom margin.
pub(crate) fn edge_scrub_area(size: Rect, protected_bottom_rows: u16) -> Option<Rect> {
    // Skip the header plus the message pane's top border/padding area. This
    // preserves top-level UI chrome while still covering the transcript rows
    // that can accumulate edge residue during streaming redraws.
    let skip_top = 2u16;
    let safe_height = size
        .height
        .saturating_sub(skip_top.saturating_add(protected_bottom_rows));
    (safe_height > 0).then(|| Rect::new(0, skip_top, size.width, safe_height))
}

/// Physically blank the terminal edge columns and reset ratatui's back buffer so
/// the following draw does not optimize those blanks away.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn scrub_terminal_edges<B>(terminal: &mut Terminal<B>, style: Style) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
{
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    scrub_edge_columns_in_buffer(terminal.current_buffer_mut(), area, style);
    terminal.backend_mut().flush()
}

/// Queue the physical edge-scrub escape sequences (`MoveTo` + `Print(" ")`) for
/// every edge cell in `area` through the crossterm backend, then flush.
///
/// Split out from [`scrub_crossterm_terminal_edges`] so the exact bytes can be
/// captured in a vt100/byte-level test with an explicit `area` — the parent
/// derives `area` from `terminal.size()`, which opens `/dev/tty` and is
/// therefore non-deterministic headless.
fn queue_edge_scrub<W: Write>(
    backend: &mut CrosstermBackend<W>,
    area: Rect,
) -> io::Result<()> {
    for (x, y) in edge_scrub_positions(area) {
        backend.queue(MoveTo(x, y))?;
        backend.queue(Print(" "))?;
    }
    std::io::Write::flush(backend)
}

/// Crossterm-specific physical edge scrub used by the real chat UI terminal.
///
/// Only scrubs edge columns for the message content area (which has no side
/// borders). Skips the header, input box, status bar, and subagent panel rows
/// which use Borders::ALL and would lose their side border characters.
///
/// ## P16.3 capability gate
///
/// `caps` is the negotiated [`TermCaps`](super::termcaps::TermCaps) (as
/// `Option<&_>`). The gate — [`edge_scrub_enabled`](super::termcaps::edge_scrub_enabled)
/// — runs **before** `terminal.size()`:
/// * `None` (unknown / not threaded) ⇒ today's UNCONDITIONAL scrub.
/// * `Some` under tmux provenance ⇒ scrub (edge residue is a tmux artifact).
/// * `Some` affirmatively NOT under tmux ⇒ short-circuit with **zero bytes**.
///
/// Short-circuiting before `terminal.size()` means the no-tmux case emits no
/// escapes *and* performs no `/dev/tty` query — cleanly testable headless.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn scrub_crossterm_terminal_edges<W>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    caps: Option<&super::termcaps::TermCaps>,
    protected_bottom_rows: u16,
    style: Style,
) -> io::Result<()>
where
    W: Write,
{
    // Gate FIRST — before any `/dev/tty`-touching size query. Affirmatively
    // no-tmux ⇒ no scrub bytes at all (the one behavior change vs today).
    if !super::termcaps::edge_scrub_enabled(caps) {
        return Ok(());
    }

    let size = terminal.size()?;
    let Some(area) = edge_scrub_area(Rect::new(0, 0, size.width, size.height), protected_bottom_rows) else {
        return Ok(());
    };

    // This scrub writes directly to the terminal before ratatui's diff flush.
    // The TUI keeps the hardware cursor hidden for its whole lifecycle and
    // draws the input cursor into the ratatui buffer, so these transient backend
    // cursor moves can never become visible.

    queue_edge_scrub(terminal.backend_mut(), area)?;

    scrub_edge_columns_in_buffer(terminal.current_buffer_mut(), area, style);
    Ok(())
}

/// Render a scrolled transcript viewport without relying on terminal scroll-region
/// optimizations. Clearing the viewport cells before drawing prevents edge-column
/// residue when content moves upward by one row and a previously occupied first or
/// last cell is blank in the new frame.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn render_scrolled_lines(
    buf: &mut Buffer,
    area: Rect,
    lines: &[Line<'static>],
    style: Style,
) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
                cell.set_style(style);
            }
        }
    }

    Paragraph::new(lines.to_vec()).style(style).render(area, buf);
}

#[cfg(test)]
mod scrub_gate_tests {
    //! P16.3 gate-1 (edge-scrub) byte-level tests.
    //!
    //! `crossterm::terminal::size()` opens `/dev/tty` and is non-deterministic
    //! headless, so we split the proof:
    //!   * **absent when no-tmux** — driven through the real gated
    //!     `scrub_crossterm_terminal_edges`, which short-circuits BEFORE any
    //!     size query, so zero bytes / no `/dev/tty` touch (fully deterministic).
    //!   * **present when tmux/unknown** — the emission core `queue_edge_scrub`
    //!     with an explicit `area` emits the CUP + space bytes; combined with
    //!     `edge_scrub_enabled(Some(tmux)) == true` / `edge_scrub_enabled(None)
    //!     == true` (see `termcaps::gate_tests`) this proves the gate lets those
    //!     cases reach emission.
    use super::*;
    use crate::tui::termcaps::{edge_scrub_enabled, TermCaps};
    use ratatui::{TerminalOptions, Viewport};
    use std::sync::{Arc, Mutex};

    /// A cloneable in-memory `Write` sink so we can read bytes back after the
    /// backend has consumed our clone (ratatui 0.30 `writer_mut` is unstable).
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }
    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn caps_tmux() -> TermCaps {
        TermCaps { tmux: Some("3.4".to_string()), ..TermCaps::default() }
    }
    fn caps_no_tmux() -> TermCaps {
        TermCaps { tmux: None, ..TermCaps::default() }
    }

    fn fixed_terminal(sink: SharedSink) -> Terminal<CrosstermBackend<SharedSink>> {
        // Fixed viewport ⇒ construction never calls backend.size()/`/dev/tty`.
        Terminal::with_options(
            CrosstermBackend::new(sink),
            TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)) },
        )
        .expect("fixed-viewport in-memory terminal construction is infallible")
    }

    // ── absent when affirmatively no-tmux (deterministic full-path) ─────────

    #[test]
    fn scrub_emits_zero_bytes_when_affirmatively_no_tmux() {
        let sink = SharedSink::default();
        let mut term = fixed_terminal(sink.clone());
        // Gate short-circuits before terminal.size(): no bytes, no /dev/tty.
        scrub_crossterm_terminal_edges(&mut term, Some(&caps_no_tmux()), 6, Style::default())
            .expect("gated-off scrub returns Ok without touching the terminal");
        assert!(
            sink.bytes().is_empty(),
            "affirmatively no-tmux caps must emit ZERO scrub bytes"
        );
    }

    // ── present when tmux / unknown (emission core, explicit area) ──────────

    #[test]
    fn emission_core_writes_cup_and_space_bytes() {
        let sink = SharedSink::default();
        let mut backend = CrosstermBackend::new(sink.clone());
        // Message-pane area (below the 2-row header): edges get scrubbed.
        queue_edge_scrub(&mut backend, Rect::new(0, 2, 80, 16)).expect("queue emits");
        let bytes = sink.bytes();
        assert!(!bytes.is_empty(), "edge scrub must emit bytes");
        // Contains at least one CSI cursor-position (`ESC [`) escape …
        assert!(
            bytes.windows(2).any(|w| w == [0x1b, b'[']),
            "scrub must emit CUP (ESC [) escapes"
        );
        // … and the blanking spaces.
        assert!(bytes.contains(&b' '), "scrub must emit blanking spaces");
    }

    #[test]
    fn gate_lets_tmux_and_unknown_reach_emission() {
        // The tmux + unknown cases pass the gate → reach queue_edge_scrub above.
        assert!(edge_scrub_enabled(Some(&caps_tmux())), "tmux ⇒ scrub");
        assert!(edge_scrub_enabled(None), "unknown ⇒ scrub (= today)");
        assert!(!edge_scrub_enabled(Some(&caps_no_tmux())), "no-tmux ⇒ skip");
    }
}
