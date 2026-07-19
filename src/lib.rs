// agent-core is now a separate crate; re-export its modules as if they lived here
pub use agent_core::core;
pub use agent_core::memory;
pub use agent_core::pricing;

// agent-engine is now a separate crate; re-export its modules as if they lived here
pub use agent_engine::{runtime, tools, mcp, skills, events, extensions, sidecar, engine, help, orchestration};

// agent-tui is now a separate crate; re-export tui + toast so bin/cmd still resolve
pub use agent_tui::{tui, toast};

// Allow intra-crate self-reference via `synaps_cli::` (used in src/tui/**).
extern crate self as synaps_cli;

// Re-export core modules at crate root for backward compatibility
pub use core::reasoning;
pub use core::config;
pub use core::session;
pub use core::auth;
pub use core::logging;
pub use core::protocol;
pub use core::error;
pub use core::watcher_types;
pub use core::models;
pub use core::chain;

pub use runtime::{Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent};
pub use agent_core::SharedMessage;
pub use agent_core::{next_turn_correlation_id, BudgetDimension, TurnError, TurnOutcome};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions, list_recent_sessions, resolve_session, find_session_by_name, validate_name};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt};
pub use watcher_types::{
    AgentConfig, SessionLimits, HandoffState, ExitReason, SessionStats,
    WatcherCommand, WatcherResponse, AgentStatusInfo
};

// Re-export for convenience
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;

/// Re-export epoch_millis from agent-core (moved there for the leaf crate split).
pub use agent_core::epoch_millis;

/// Re-export truncate_str + BoundedText from agent-core.
pub use agent_core::{truncate_str, BoundedText};

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
