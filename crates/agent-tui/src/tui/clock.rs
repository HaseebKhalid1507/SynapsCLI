// `real()`/`now()` are live in production (App, TranscriptStore, ToastProvider);
// `test()`/`advance()` are exercised only by the harness + unit tests, so they
// read as dead in a plain non-test build — hence the module-level allow.
#![allow(dead_code)]
//! Injectable clock abstraction for the TUI.
//!
//! Production code uses [`TuiClock::real()`], which delegates to
//! [`Instant::now()`] and [`chrono::Local::now()`].  Test code uses
//! [`TuiClock::test()`], which starts frozen and only advances when
//! [`TuiClock::advance`] is called explicitly — both the monotonic time and
//! the wall-clock time ([`TuiClock::wall_now`], what message `HH:MM` stamps
//! and day lines read). Cloned handles share the same underlying time source,
//! so advancing through one handle is immediately visible through all others.
//!
//! The test wall clock starts at a fixed instant ([`TEST_WALL_EPOCH_SECS`]),
//! not at boot: two harnesses booted a minute apart (a tape replayed twice)
//! must stamp the same `HH:MM`, and none may cross local midnight mid-replay.
//!
//! # Out of scope (P6.2 and later)
//! - `mod.rs` throttle / `render_thread.rs` — those timers drive the render
//!   cadence and are intentionally excluded from this abstraction.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a test clock's wall time starts: 2026-01-15 12:00:00 UTC. Mid-day
/// and mid-month, so local time in any zone is far from a midnight or DST
/// edge for any test's advances.
pub(crate) const TEST_WALL_EPOCH_SECS: i64 = 1_768_478_400;

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

    /// The local wall-clock time according to this clock: `Local::now()`
    /// live; under test, the fixed [`TEST_WALL_EPOCH_SECS`] plus every
    /// [`advance`](Self::advance) so far.
    pub(crate) fn wall_now(&self) -> chrono::DateTime<chrono::Local> {
        match self {
            Self::Real => chrono::Local::now(),
            Self::Test(state) => {
                let offset = state.lock().expect("TuiClock poisoned").offset;
                let epoch = chrono::DateTime::from_timestamp(TEST_WALL_EPOCH_SECS, 0)
                    .expect("valid test wall epoch");
                (epoch + chrono::Duration::from_std(offset).unwrap_or(chrono::Duration::zero()))
                    .with_timezone(&chrono::Local)
            }
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
    fn test_wall_clock_is_fixed_and_advances_with_the_clock() {
        // Two test clocks made at different real times read the same wall
        // time: a tape replayed twice stamps the same `HH:MM`.
        let a = TuiClock::test();
        std::thread::sleep(Duration::from_millis(5));
        let b = TuiClock::test();
        assert_eq!(a.wall_now(), b.wall_now());
        assert_eq!(a.wall_now().timestamp(), TEST_WALL_EPOCH_SECS);
        a.advance(Duration::from_secs(90));
        assert_eq!(a.wall_now().timestamp(), TEST_WALL_EPOCH_SECS + 90);
        assert_eq!(b.wall_now().timestamp(), TEST_WALL_EPOCH_SECS);
    }

    #[test]
    fn real_clock_advance_is_silent_noop() {
        // Just ensuring it doesn't panic.
        let clock = TuiClock::real();
        clock.advance(Duration::from_secs(9999));
        let _ = clock.now();
    }
}
