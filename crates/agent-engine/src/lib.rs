//! `agent-engine` — streaming runtime, tools, MCP, skills, extensions, sidecar, events.
//! Depends on `agent-core` + external crates only. Never depends on tui or bin.

pub mod engine;
pub mod events;
pub mod extensions;
pub mod help;
pub mod mcp;
pub mod runtime;
pub mod sidecar;
pub mod skills;
#[cfg(test)]
pub(crate) mod test_env;
pub mod tools;

// ── agent-core facade ──────────────────────────────────────────────────────────
// Re-export core so that internal `crate::core::X`, `crate::config`, etc.
// resolve inside agent-engine (mirrors root lib.rs approach).
pub use agent_core::{
    auth, chain, config, error, logging, models, protocol, session, watcher_types,
};
pub use agent_core::{core, memory, pricing};
pub use agent_core::{epoch_millis, truncate_str, BoundedText};

// ── engine-internal top-level re-exports ──────────────────────────────────────
// These mirror what root lib.rs was exporting; they let intra-engine code use
// `crate::Runtime`, `crate::StreamEvent`, etc. (45 uses of StreamEvent alone).
pub use agent_core::SharedMessage;
pub use agent_core::{next_turn_correlation_id, BudgetDimension, TurnError, TurnOutcome};
pub use config::{load_config, resolve_system_prompt, SynapsConfig};
pub use error::{Result, RuntimeError};
pub use runtime::{AgentEvent, LlmEvent, Runtime, SessionEvent, StreamEvent};
pub use serde_json::Value;
pub use session::{
    find_session, find_session_by_name, latest_session, list_recent_sessions, list_sessions,
    resolve_session, validate_name, Session, SessionInfo,
};
pub use tokio_util::sync::CancellationToken;
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use watcher_types::{
    AgentConfig, AgentStatusInfo, ExitReason, HandoffState, SessionLimits, SessionStats,
    WatcherCommand, WatcherResponse,
};

pub mod orchestration;
