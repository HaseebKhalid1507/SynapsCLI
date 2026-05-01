//! Terminal lifecycle: enter/leave alternate screen, raw mode, mouse, paste.
//!
//! Extracted from `mod.rs` so `run()` doesn't have to spell out the dance.

use std::io;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Enable raw mode, switch to the alternate screen, enable mouse capture and
/// bracketed paste, then build a ratatui `Terminal`.
pub(super) fn setup_terminal() -> synaps_cli::Result<Term> {
    enable_raw_mode().map_err(|e| {
        synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e))
    })?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(|e| {
        synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e))
    })?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).map_err(|e| {
        synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e))
    })?;
    Ok(terminal)
}

/// Reverse of `setup_terminal`: drop raw mode, leave alt screen, restore cursor.
/// Best-effort — errors are swallowed (we are usually exiting anyway).
pub(super) fn teardown_terminal(terminal: &mut Term) {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
}
