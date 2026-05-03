use std::io::{self, Write};

use crossterm::{cursor::MoveTo, style::Print, QueueableCommand};
#[cfg(test)]
use ratatui::backend::Backend;
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Widget},
    Terminal,
};

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

/// Physically blank the terminal edge columns and reset ratatui's back buffer so
/// the following draw does not optimize those blanks away.
#[cfg(test)]
pub(crate) fn scrub_terminal_edges<B>(terminal: &mut Terminal<B>, style: Style) -> io::Result<()>
where
    B: Backend,
{
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    scrub_edge_columns_in_buffer(terminal.current_buffer_mut(), area, style);
    terminal.backend_mut().flush()
}

/// Crossterm-specific physical edge scrub used by the real chat UI terminal.
///
/// Only scrubs edge columns for the message content area (which has no side
/// borders). Skips the header, input box, status bar, and subagent panel rows
/// which use Borders::ALL and would lose their side border characters.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn scrub_crossterm_terminal_edges<W>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    style: Style,
) -> io::Result<()>
where
    W: Write,
{
    let size = terminal.size()?;
    // Only scrub the interior rows, skip first 2 rows (header + top border)
    // and last 4 rows (input box borders + status bar). This preserves
    // border characters on widgets that use Borders::ALL.
    let skip_top = 2u16;
    let skip_bottom = 4u16;
    let safe_height = size.height.saturating_sub(skip_top + skip_bottom);
    if safe_height == 0 {
        return Ok(());
    }
    let area = Rect::new(0, skip_top, size.width, safe_height);
    let backend = terminal.backend_mut();
    for (x, y) in edge_scrub_positions(area) {
        backend.queue(MoveTo(x, y))?;
        backend.queue(Print(" "))?;
    }
    scrub_edge_columns_in_buffer(terminal.current_buffer_mut(), area, style);
    std::io::Write::flush(terminal.backend_mut())
}

/// Render a scrolled transcript viewport without relying on terminal scroll-region
/// optimizations. Clearing the viewport cells before drawing prevents edge-column
/// residue when content moves upward by one row and a previously occupied first or
/// last cell is blank in the new frame.
#[cfg_attr(not(test), allow(dead_code))]
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
