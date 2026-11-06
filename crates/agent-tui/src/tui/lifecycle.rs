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

use super::termcaps::TermCaps;

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Enable raw mode, switch to the alternate screen, enable mouse capture and
/// bracketed paste, then build a ratatui `Terminal`.
///
/// ## P16.3 kitty-keyboard push gate
///
/// `caps` is the negotiated [`TermCaps`] as `Option<&_>`.
/// [`kitty_push_enabled`](super::termcaps::kitty_push_enabled) decides whether
/// to emit `PushKeyboardEnhancementFlags`:
/// * `None` (unknown / not threaded) ⇒ today's blind best-effort push.
/// * `Some` with no DA1 fence (`da1_answered == false`) ⇒ blind push (= today).
/// * `Some`, DA1-fenced ⇒ push only if `kitty_keyboard` was negotiated true.
///
/// NOTE: in production `setup_terminal` runs BEFORE the DA1 burst
/// (`run_setup` enables raw mode here, THEN negotiates on fd 0), so caps are
/// not yet fenced at this call site and it correctly degrades to the blind
/// push — byte-identical with today. The gate is fact-based + testable for the
/// day the push moves after negotiation or a re-push is added.
///
/// [`TermCaps`]: super::termcaps::TermCaps
pub(super) fn setup_terminal(caps: Option<&TermCaps>) -> synaps_cli::Result<Term> {
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
    .map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    // Best-effort: enable the kitty keyboard protocol so modifier-heavy
    // chords (Ctrl+Alt+V, Ctrl+Shift+letter, etc.) report correctly on
    // terminals that support it (kitty, wezterm, foot, iterm2, alacritty).
    // Terminals that don't support it ignore the escape sequence, so we
    // swallow any error.
    if super::termcaps::kitty_push_enabled(caps) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
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

#[cfg(test)]
mod kitty_gate_tests {
    //! P16.3 gate-3 (kitty-keyboard push) decision tests.
    //!
    //! `setup_terminal` itself drives a real terminal (raw mode, alt screen)
    //! and cannot run headless, so the gate is proven at the decision-function
    //! level — the exact predicate `setup_terminal` branches on.
    use super::TermCaps;
    use crate::tui::termcaps::kitty_push_enabled;

    #[test]
    fn unknown_caps_default_to_blind_push() {
        // No caps threaded (the production call site passes `None`) ⇒ push,
        // byte-identical with today's unconditional best-effort push.
        assert!(kitty_push_enabled(None), "unknown caps must push (= today)");
    }

    #[test]
    fn da1_timeout_defaults_to_blind_push() {
        // Default caps carry no DA1 fence ⇒ still push (= today).
        let caps = TermCaps::default();
        assert!(!caps.da1_answered);
        assert!(
            kitty_push_enabled(Some(&caps)),
            "no fence ⇒ blind push (= today)"
        );
    }

    #[test]
    fn negotiated_kitty_support_pushes() {
        let caps = TermCaps {
            da1_answered: true,
            kitty_keyboard: true,
            ..TermCaps::default()
        };
        assert!(kitty_push_enabled(Some(&caps)));
    }

    #[test]
    fn negotiated_no_kitty_skips_push() {
        let caps = TermCaps {
            da1_answered: true,
            kitty_keyboard: false,
            ..TermCaps::default()
        };
        assert!(
            !kitty_push_enabled(Some(&caps)),
            "kitty negotiated off ⇒ no push"
        );
    }
}
