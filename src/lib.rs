pub mod runtime;
pub mod tools;
pub mod session;
pub mod error;
pub mod protocol;
pub mod auth;
pub mod logging;
pub mod config;

pub use runtime::{Runtime, StreamEvent};
pub use tools::{ToolType, ToolRegistry};
pub use session::{Session, SessionInfo, find_session, latest_session, list_sessions};
pub use error::{RuntimeError, Result};
pub use config::{SynapsConfig, load_config, resolve_system_prompt, apply_config};

// Re-export for convenience
pub use serde_json::Value;
pub use tokio_util::sync::CancellationToken;