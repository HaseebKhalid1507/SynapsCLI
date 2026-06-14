//! `agent-engine` — streaming runtime, tools, MCP, skills, extensions, sidecar, events.
//! Depends on `agent-core` + external crates only. Never depends on tui or bin.

pub mod runtime;
pub mod tools;
pub mod mcp;
pub mod skills;
pub mod events;
pub mod extensions;
pub mod sidecar;
pub mod engine;
pub mod help;

// ── agent-core facade ──────────────────────────────────────────────────────────
// Re-export core so that internal `crate::core::X`, `crate::config`, etc.
// resolve inside agent-engine (mirrors root lib.rs approach).
pub use agent_core::{core, memory, pricing};
pub use agent_core::{config, session, auth, logging, protocol, error, watcher_types, models, chain};
pub use agent_core::{epoch_millis, truncate_str};

// ── engine-internal top-level re-exports ──────────────────────────────────────
// These mirror what root lib.rs was exporting; they let intra-engine code use
// `crate::Runtime`, `crate::StreamEvent`, etc. (45 uses of StreamEvent alone).
pub use runtime::{Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions, list_recent_sessions,
                  resolve_session, find_session_by_name, validate_name};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt};
pub use watcher_types::{
    AgentConfig, SessionLimits, HandoffState, ExitReason, SessionStats,
    WatcherCommand, WatcherResponse, AgentStatusInfo,
};
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;
