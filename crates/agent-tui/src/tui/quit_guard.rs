//! F27: quit-while-streaming guard for socket (daemon) sessions.
//!
//! When a user presses Ctrl+C (or `/quit`) while streaming over a socket
//! transport, the turn keeps running headlessly in the daemon. This module
//! provides a two-press state machine: the first press shows a notice; the
//! second press within [`WINDOW`] detaches as today.
//!
//! In-process (`TransportMode::Local`) and idle (not streaming) quit paths
//! are unchanged — they bypass this guard entirely.

use std::time::{Duration, Instant};

/// Window within which a second Ctrl+C detaches.
pub const WINDOW: Duration = Duration::from_secs(3);

/// Notice shown on the first Ctrl+C while streaming over a socket transport.
pub const NOTICE: &str =
    "turn still running in the daemon \u{2014} press Esc to abort it, or Ctrl+C again within 3 s to detach (session keeps running)";

/// Same notice for the line client (`synaps attach`), written to stderr.
pub const NOTICE_LINE: &str =
    "turn still running in the daemon \u{2014} Ctrl+C again within 3 s to detach (session keeps running)";

/// Two-press quit guard. Timestamps are injectable for testing.
#[derive(Debug, Clone)]
pub struct QuitGuard {
    /// Timestamp of the first (warning) press, if any.
    first_press: Option<Instant>,
}

impl QuitGuard {
    pub fn new() -> Self {
        Self { first_press: None }
    }

    /// Called on a quit intent while streaming over a socket transport.
    /// Returns `true` if the caller should proceed with detach (second press
    /// within the window). Returns `false` if this was the first press (the
    /// caller should show the notice instead).
    pub fn press(&mut self, now: Instant) -> bool {
        if let Some(first) = self.first_press {
            if now.duration_since(first) <= WINDOW {
                self.first_press = None;
                return true;
            }
        }
        // First press (or expired window) — record and warn.
        self.first_press = Some(now);
        false
    }

    /// Reset when streaming ends (the guard is only relevant while streaming).
    pub fn reset(&mut self) {
        self.first_press = None;
    }
}

impl Default for QuitGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a fake "now" at `offset` from a shared anchor.
    fn fake_now(offset: Duration) -> Instant {
        use std::sync::OnceLock;
        static ANCHOR: OnceLock<Instant> = OnceLock::new();
        *ANCHOR.get_or_init(Instant::now) + offset
    }

    #[test]
    fn first_press_returns_false() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
    }

    #[test]
    fn second_press_within_window_returns_true() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        assert!(g.press(fake_now(Duration::from_secs(2))));
    }

    #[test]
    fn second_press_after_window_returns_false() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        assert!(!g.press(fake_now(Duration::from_secs(4))));
    }

    #[test]
    fn third_press_within_new_window_returns_true() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        // Expired.
        assert!(!g.press(fake_now(Duration::from_secs(4))));
        // Now within the new window.
        assert!(g.press(fake_now(Duration::from_secs(5))));
    }

    #[test]
    fn reset_clears_state() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        g.reset();
        assert!(!g.press(fake_now(Duration::from_secs(1))));
    }

    #[test]
    fn exact_boundary_is_within_window() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        assert!(g.press(fake_now(Duration::from_secs(3))));
    }

    #[test]
    fn just_past_boundary_is_outside_window() {
        let mut g = QuitGuard::new();
        assert!(!g.press(fake_now(Duration::from_secs(0))));
        assert!(!g.press(fake_now(Duration::from_millis(3001))));
    }
}
