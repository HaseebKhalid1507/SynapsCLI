// `real()`/`now()` are live in production (App, TranscriptStore, ToastProvider);
// `test()`/`advance()` are exercised only by the harness + unit tests, so they
// read as dead in a plain non-test build — hence the module-level allow.
#![allow(dead_code)]
//! Injectable clock abstraction for the TUI.
//!
//! Production code uses [`TuiClock::real()`], which delegates to
//! [`Instant::now()`].  Test code uses [`TuiClock::test()`], which starts
//! frozen and only advances when [`TuiClock::advance`] is called explicitly.
//! Cloned handles share the same underlying time source, so advancing through
//! one handle is immediately visible through all others.
//!
//! # Out of scope (P6.2 and later)
//! - `mod.rs` throttle / `render_thread.rs` — those timers drive the render
//!   cadence and are intentionally excluded from this abstraction.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Internal state for the test variant
// ---------------------------------------------------------------------------

/// Shared mutable state behind a test clock.
#[derive(Debug)]
pub(crate) struct TestState {
    /// A real `Instant` captured at construction time.  We add `offset` to it
    /// when answering `now()` so the returned value is always a genuine
    /// `Instant` (no unsafe transmutes needed).
    epoch: Instant,
    offset: Duration,
}

impl TestState {
    fn now(&self) -> Instant {
        self.epoch + self.offset
    }
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Lightweight, cheaply-cloneable clock handle.
///
/// ```text
/// let clock = TuiClock::real();          // production
/// let clock = TuiClock::test();          // deterministic tests
/// let t = clock.now();
/// clock.advance(Duration::from_secs(1)); // test only (no-op on Real)
/// ```
#[derive(Debug, Clone)]
pub(crate) enum TuiClock {
    /// Delegates directly to [`Instant::now()`].
    Real,
    /// Frozen clock; only moves when [`TuiClock::advance`] is called.
    /// The `Arc<Mutex<…>>` lets clones share the same time.
    Test(Arc<Mutex<TestState>>),
}

impl TuiClock {
    /// A clock backed by the real system monotonic clock.
    pub(crate) fn real() -> Self {
        Self::Real
    }

    /// A frozen clock for tests.  Time starts at "now" (as measured once at
    /// construction) and only moves when [`advance`](Self::advance) is called.
    pub(crate) fn test() -> Self {
        Self::Test(Arc::new(Mutex::new(TestState {
            epoch: Instant::now(),
            offset: Duration::ZERO,
        })))
    }

    /// Return the current time according to this clock.
    pub(crate) fn now(&self) -> Instant {
        match self {
            Self::Real => Instant::now(),
            Self::Test(state) => state.lock().expect("TuiClock poisoned").now(),
        }
    }

    /// Advance a test clock by `duration`.  On a real clock this is a no-op
    /// (there is no meaningful way to "advance" wall time, and callers should
    /// not need to in production).
    pub(crate) fn advance(&self, duration: Duration) {
        if let Self::Test(state) = self {
            state.lock().expect("TuiClock poisoned").offset += duration;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clock_is_monotonically_non_decreasing() {
        let clock = TuiClock::real();
        let t0 = clock.now();
        let t1 = clock.now();
        assert!(t1 >= t0, "real clock went backwards: {t0:?} > {t1:?}");
    }

    #[test]
    fn test_clock_is_frozen_until_advance() {
        let clock = TuiClock::test();
        let t0 = clock.now();
        let t1 = clock.now();
        assert_eq!(t0, t1, "test clock should not tick on its own");

        clock.advance(Duration::from_millis(500));
        let t2 = clock.now();
        assert_eq!(t2, t0 + Duration::from_millis(500));
    }

    #[test]
    fn cloned_test_handle_observes_advances_from_original() {
        let original = TuiClock::test();
        let clone = original.clone();

        let before = clone.now();
        original.advance(Duration::from_secs(3));
        let after = clone.now();

        assert_eq!(after, before + Duration::from_secs(3));
    }

    #[test]
    fn advance_from_clone_is_visible_through_original() {
        let a = TuiClock::test();
        let b = a.clone();

        let t0 = a.now();
        b.advance(Duration::from_millis(100));
        assert_eq!(a.now(), t0 + Duration::from_millis(100));
    }

    #[test]
    fn real_clock_advance_is_silent_noop() {
        // Just ensuring it doesn't panic.
        let clock = TuiClock::real();
        clock.advance(Duration::from_secs(9999));
        let _ = clock.now();
    }
}
