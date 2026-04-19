//! Session manager — owns and manages active shell sessions.

use std::sync::Arc;

/// Manages active shell sessions. Shared across tools via Arc.
pub struct SessionManager;

impl SessionManager {
    /// Create a new session manager (placeholder).
    pub fn new(_config: super::ShellConfig) -> Arc<Self> {
        Arc::new(Self)
    }
}
