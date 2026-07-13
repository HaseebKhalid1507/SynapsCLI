// agent-core is now a separate crate; re-export its modules as if they lived here
pub use agent_core::core;
pub use agent_core::memory;
pub use agent_core::pricing;

// agent-engine is now a separate crate; re-export its modules as if they lived here
pub use agent_engine::{engine, events, extensions, help, mcp, runtime, sidecar, skills, tools};

// agent-tui is now a separate crate; re-export tui + toast so bin/cmd still resolve
pub use agent_tui::{toast, tui};

// Allow intra-crate self-reference via `synaps_cli::` (used in src/tui/**).
extern crate self as synaps_cli;

// Re-export core modules at crate root for backward compatibility
pub use core::auth;
pub use core::chain;
pub use core::config;
pub use core::error;
pub use core::logging;
pub use core::models;
pub use core::protocol;
pub use core::reasoning;
pub use core::session;
pub use core::watcher_types;

pub use agent_core::SharedMessage;
pub use config::{load_config, resolve_system_prompt, SynapsConfig};
pub use error::{Result, RuntimeError};
pub use runtime::{AgentEvent, LlmEvent, Runtime, SessionEvent, StreamEvent};
pub use session::{
    find_session, find_session_by_name, latest_session, list_recent_sessions, list_sessions,
    resolve_session, validate_name, Session, SessionInfo,
};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use watcher_types::{
    AgentConfig, AgentStatusInfo, ExitReason, HandoffState, SessionLimits, SessionStats,
    WatcherCommand, WatcherResponse,
};

// Re-export for convenience
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;

/// Re-export epoch_millis from agent-core (moved there for the leaf crate split).
pub use agent_core::epoch_millis;

/// Re-export truncate_str from agent-core.
pub use agent_core::truncate_str;

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

/// Current time as Unix epoch seconds.
#[inline]
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}
