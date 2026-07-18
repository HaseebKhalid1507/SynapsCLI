//! `agent-tui` — ratatui terminal UI layer.
//! Depends on `agent-core` + `agent-engine` + render crates only. Never depends on bin.

// Allow intra-crate self-reference via `synaps_cli::` (mirrors root lib.rs trick).
// tui/ has ~300 synaps_cli:: references — this resolves them all to agent_tui without
// touching a single call site.
extern crate self as synaps_cli;

pub mod toast;
pub mod tui;

// ── agent-core re-exports ──────────────────────────────────────────────────────
pub use agent_core::{
    auth, chain, config, error, logging, models, protocol, session, watcher_types,
};
pub use agent_core::{core, memory, pricing};
pub use agent_core::{epoch_millis, truncate_str, BoundedText};

// ── agent-engine re-exports ────────────────────────────────────────────────────
pub use agent_engine::{engine, events, extensions, help, mcp, runtime, sidecar, skills, tools};

// ── item re-exports (tui uses these at crate root) ────────────────────────────
pub use agent_engine::{
    find_session, find_session_by_name, latest_session, list_recent_sessions, list_sessions,
    resolve_session, validate_name, Session, SessionInfo,
};
pub use agent_engine::{load_config, resolve_system_prompt, SynapsConfig};
pub use agent_engine::{AgentEvent, LlmEvent, Runtime, SessionEvent, SharedMessage, StreamEvent};
pub use agent_engine::{Result, RuntimeError};
pub use agent_engine::{Tool, ToolContext, ToolRegistry};
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;
