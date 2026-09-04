//! Teardown budgets shared by every session host (values copied from
//! `agent-tui/src/tui/signals.rs`; `signals.rs` re-exports these on day 2).
//!
//! - `SAVE_TIMEOUT_SECS`  — session save + index end record (data safety first)
//! - `HOOKS_TIMEOUT_SECS` — `on_session_end` hook emit (concurrent, fail-open)
//! - `TEARDOWN_TIMEOUT_SECS` = their sum.

pub const SAVE_TIMEOUT_SECS: u64 = 2;
pub const HOOKS_TIMEOUT_SECS: u64 = 5;
pub const TEARDOWN_TIMEOUT_SECS: u64 = SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS;

pub const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(SAVE_TIMEOUT_SECS);
pub const HOOKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(HOOKS_TIMEOUT_SECS);
