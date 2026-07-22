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

/// Bounded observability flush for TUI teardown (Task 11): stop intake on
/// the session's telemetry/trace writer and drain it under the default
/// shutdown budget.
///
/// Semantics: telemetry `off` → no writer → `None`, a true no-op.
/// "Flushed" means every queued record was appended into OS file buffers
/// (no fsync — best-effort diagnostic logs). A timeout logs a
/// metadata-only warning (counter stats, never record content) and
/// teardown continues — trace loss must never abort or fail an exit.
pub(super) async fn flush_observability(runtime: &synaps_cli::Runtime) {
    flush_observability_within(
        runtime,
        synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
    )
    .await;
}

/// Emergency-exit epilogue (session save timed out): short (1 s) bounded
/// observability flush, then terminal restore and `process::exit(1)`. The
/// exit reason is the save timeout — never trace loss; the flush is
/// best-effort and cannot extend the exit beyond its own bound.
pub(super) async fn emergency_flush_and_exit(runtime: &synaps_cli::Runtime) -> ! {
    flush_observability_within(runtime, std::time::Duration::from_secs(1)).await;
    emergency_teardown_terminal();
    std::process::exit(1);
}

async fn flush_observability_within(runtime: &synaps_cli::Runtime, budget: std::time::Duration) {
    match runtime.shutdown_observability_async(budget).await {
        None => {} // telemetry off — nothing to flush
        Some(outcome) if outcome.is_flushed() => {
            tracing::debug!("observability flush completed");
        }
        Some(outcome) => {
            tracing::warn!(
                stats = ?outcome.stats(),
                "observability flush timed out — detached worker keeps draining"
            );
        }
    }
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
