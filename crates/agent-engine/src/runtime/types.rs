use serde::Deserialize;

// Stream event types now live in agent-core so rpc_dispatch can use them
// without an upward dependency on the engine layer.
pub use agent_core::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// Whether a turn reached its NORMAL end: the final response came back
/// un-cancelled and asked for no tools (nothing left to do). Set by the
/// engine before it publishes that turn's final `MessageHistory`.
///
/// `Done` cannot say this: several cancelled paths also end `Ok` + `Done`.
/// A host that cancels a turn reads it once the stream is drained: a turn
/// that finished just before the cancel reached it (Esc a moment too late)
/// is complete, not interrupted.
#[derive(Debug, Clone, Default)]
pub struct TurnCompletion(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl TurnCompletion {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn completed(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn mark_completed(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Shared mutable auth state. Lives behind `Arc<RwLock<_>>` so the spawned
/// streaming task and the parent Runtime always see the same (freshest) token.
#[derive(Debug, Clone)]
pub(super) struct AuthState {
    pub(super) auth_token: String,
    pub(super) auth_type: String,
    pub(super) refresh_token: Option<String>,
    pub(super) token_expires: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub(super) struct AnthropicAuth {
    #[serde(rename = "type")]
    pub(super) auth_type: String,
    pub(super) refresh: Option<String>,
    pub(super) access: Option<String>,
    pub(super) expires: Option<u64>,
    pub(super) key: Option<String>,
}
