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

/// `SessionActor::create` bound on `EngineHost::extensions_ready()`: the
/// loader guard should make this unreachable; it exists so a session can
/// never hang on a loader that never reports (warns, then proceeds).
pub const EXTENSIONS_READY_TIMEOUT_SECS: u64 = 30;
pub const EXTENSIONS_READY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(EXTENSIONS_READY_TIMEOUT_SECS);

/// `SessionActor::unpark` bound (B3): journal load + runtime rebuild.
/// Transports wait `ATTACH_TIMEOUT_PARKED` (25 s) for a parked attach.
pub const UNPARK_TIMEOUT_SECS: u64 = 20;
pub const UNPARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(UNPARK_TIMEOUT_SECS);
