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
pub mod sentinel_types;

pub use runtime::{Runtime, StreamEvent};
pub use tools::{Tool, ToolContext, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt, apply_config};
pub use sentinel_types::{
    AgentConfig, SessionLimits, HandoffState, ExitReason, SessionStats,
    SentinelCommand, SentinelResponse, AgentStatusInfo
};

// Re-export for convenience
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;