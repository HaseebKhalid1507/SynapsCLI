//! `agent-core` — foundational types, config, session, auth, logging,
//! protocol, memory, and pricing. Leaf crate: no in-repo upward deps.

pub mod core;
pub mod memory;
pub mod pricing;

// Re-export stream event types at crate root so engine/tui can import them
// cleanly as `agent_core::{StreamEvent, LlmEvent, ...}` without digging
// into the internal module path.
pub use core::stream_types::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

// Replicate the root lib.rs aliases so internal `crate::config`,
// `crate::models`, `crate::session`, etc. resolve INSIDE agent-core.
pub use core::config;
pub use core::session;
pub use core::auth;
pub use core::logging;
pub use core::protocol;
pub use core::error;
pub use core::watcher_types;
pub use core::models;
pub use core::chain;

/// Current time as Unix epoch milliseconds. Panics only if system clock is before 1970.
#[inline]
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as u64
}

// Expose Result + RuntimeError at crate root so `crate::Result<T>` resolves
// inside agent-core modules (mirrors root lib.rs lines 28-29).
pub use error::{Result, RuntimeError};
