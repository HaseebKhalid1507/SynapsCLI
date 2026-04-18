pub mod core;
pub mod runtime;
pub mod tools;
pub mod mcp;
pub mod skills;

// Re-export core modules at crate root for backward compatibility
pub use core::config;
pub use core::session;
pub use core::auth;
pub use core::logging;
pub use core::protocol;
pub use core::error;
pub use core::watcher_types;
pub use core::models;

pub use runtime::{Runtime, StreamEvent};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt};
pub use watcher_types::{
    AgentConfig, SessionLimits, HandoffState, ExitReason, SessionStats,
    WatcherCommand, WatcherResponse, AgentStatusInfo
};

// Re-export for convenience
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;

/// Flush stdout, ignoring errors (pipe closed, etc.)
#[inline]
pub fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Flush stderr, ignoring errors (pipe closed, etc.)
#[inline]
pub fn flush_stderr() {
    use std::io::Write;
    let _ = std::io::stderr().flush();
}

/// Current time as Unix epoch milliseconds. Panics only if system clock is before 1970.
#[inline]
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as u64
}

/// Current time as Unix epoch seconds.
#[inline]
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

/// Truncate a string to at most `max` bytes at a valid UTF-8 boundary.
#[inline]
pub fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
