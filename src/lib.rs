pub mod runtime;
pub mod tools;
pub mod session;
pub mod error;
pub mod protocol;
pub mod auth;
pub mod logging;
pub mod config;
pub mod mcp;
pub mod skills;
pub mod watcher_types;

pub use runtime::{Runtime, StreamEvent};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt, apply_config};
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