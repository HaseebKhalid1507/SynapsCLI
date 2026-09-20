use serde::Deserialize;

// Stream event types now live in agent-core so rpc_dispatch can use them
// without an upward dependency on the engine layer.
pub use agent_core::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// Shared mutable auth state. Lives behind `Arc<RwLock<_>>` so the spawned
/// streaming task and the parent Runtime always see the same (freshest) token.
#[derive(Debug, Clone)]
pub(super) struct AuthState {
    pub(super) auth_token: String,
    pub(super) auth_type: String,
    pub(super) refresh_token: Option<String>,
    pub(super) token_expires: Option<u64>,
    /// Which credential `auth_token` was vended for:
    /// `"<local | remote:<endpoint>#<principal fingerprint>>|<storage_key>"`
    /// (see `runtime/auth.rs::anthropic_binding`). The principal fingerprint
    /// is an opaque SHA-256 prefix of the machine token — never the token
    /// itself, never an access token. `None` = unknown/legacy (api_key
    /// harnesses, scrubbed). The pre-stream refresh refuses to serve a
    /// cached token whose binding no longer matches the selected account on
    /// the current source, so a config/source/principal switch can never
    /// silently keep the previous account's token.
    pub(super) bound_credential: Option<String>,
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
