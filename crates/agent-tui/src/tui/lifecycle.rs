//! Terminal lifecycle: enter/leave alternate screen, raw mode, mouse, paste.
//!
//! Extracted from `mod.rs` so `run()` doesn't have to spell out the dance.

use std::io;

use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    // Best-effort: enable the kitty keyboard protocol so modifier-heavy
    // chords (Ctrl+Alt+V, Ctrl+Shift+letter, etc.) report correctly on
    // terminals that support it (kitty, wezterm, foot, iterm2, alacritty).
    // Terminals that don't support it ignore the escape sequence, so we
    // swallow any error.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| {
        synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e))
    })?;
    // Synaps renders its own input cursor in the ratatui buffer. Keeping the
    // hardware cursor hidden for the whole TUI frame lifecycle prevents it from
    // becoming visible at transient backend draw/scrub positions during
    // high-frequency streaming redraws.
    terminal.hide_cursor().ok();
    Ok(terminal)
}

pub(super) fn emergency_teardown_terminal() {
    disable_raw_mode().ok();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    execute!(
        stdout,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    // Restore cursor visibility. setup_terminal() hides the cursor for the
    // entire TUI lifecycle; if a main-thread panic triggers this path the
    // cursor must be shown again so the user's shell is usable afterward.
    // do_teardown() (render_thread.rs) covers the normal exit path; this
    // covers every other path (panic hook, signal, render-thread catch_unwind).
    execute!(stdout, cursor::Show).ok();
}

// `teardown_terminal` was removed in the #116 render-thread work — the render
// thread now owns the Terminal and performs its own teardown (see render_thread.rs).
