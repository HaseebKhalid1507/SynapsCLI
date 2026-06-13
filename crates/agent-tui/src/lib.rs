//! `agent-tui` — ratatui terminal UI layer.
//! Depends on `agent-core` + `agent-engine` + render crates only. Never depends on bin.

// Allow intra-crate self-reference via `synaps_cli::` (mirrors root lib.rs trick).
// tui/ has ~300 synaps_cli:: references — this resolves them all to agent_tui without
// touching a single call site.
extern crate self as synaps_cli;

pub mod toast;
pub mod tui;

// ── agent-core re-exports ──────────────────────────────────────────────────────
pub use agent_core::{core, memory, pricing};
pub use agent_core::{config, session, auth, logging, protocol, error, watcher_types, models, chain};
pub use agent_core::{epoch_millis, truncate_str};

// ── agent-engine re-exports ────────────────────────────────────────────────────
pub use agent_engine::{runtime, tools, mcp, skills, events, extensions, sidecar, engine, help};

// ── item re-exports (tui uses these at crate root) ────────────────────────────
pub use agent_engine::{Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent};
pub use agent_engine::{Tool, ToolContext, ToolRegistry};
pub use agent_engine::{Session, SessionInfo, find_session, latest_session, list_sessions, list_recent_sessions, resolve_session, find_session_by_name, validate_name};
pub use agent_engine::{RuntimeError, Result};
pub use agent_engine::{SynapsConfig, load_config, resolve_system_prompt};
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;
