use serde::Deserialize;

// Stream event types now live in agent-core so rpc_dispatch can use them
// without an upward dependency on the engine layer.
pub use agent_core::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// Per-turn signals shared by the host and the engine.
///
/// Engine → host: whether the turn reached its NORMAL end — the final
/// response came back un-cancelled and asked for no tools (nothing left to
/// do). Set before the engine publishes that turn's final `MessageHistory`.
/// `Done` cannot say this: several cancelled paths also end `Ok` + `Done`.
/// A host that cancels a turn reads it once the stream is drained: a turn
/// that finished just before the cancel reached it (Esc a moment too late)
/// is complete, not interrupted.
///
/// Host → engine: WHY the host cancelled the turn, noted before it cancels
/// the token, so a canceled `tool_result` says so ("Canceled by user" only
/// when it was the user). Nothing noted = the turn's default cause: the
/// user (the historical wording), or what the host chose at turn start
/// (`with_default_cause`: a driver turn whose token something else cancels,
/// e.g. the grant's deadline, was cut by the driver ending).
#[derive(Debug, Clone, Default)]
pub struct TurnCompletion {
    completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// `0` = unset, else `CancelCause as u8`.
    cancel_cause: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Used when nothing was noted; `None` = the user.
    default_cause: Option<CancelCause>,
}

/// Why a host cancelled a turn, as far as a canceled tool result says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelCause {
    User = 1,
    CostCap = 2,
    Restart = 3,
    Host = 4,
    Driver = 5,
}

impl CancelCause {
    /// Leading words of a canceled `tool_result`. Every variant starts with
    /// "Canceled" (the phase-4 bound tests key on it); the user's is the
    /// historical, byte-identical "Canceled by user".
    pub fn phrase(self) -> &'static str {
        match self {
            CancelCause::User => "Canceled by user",
            CancelCause::CostCap => "Canceled (session cost cap reached)",
            CancelCause::Restart => "Canceled (Synaps restarted)",
            CancelCause::Host => "Canceled (session stopped by the host)",
            CancelCause::Driver => "Canceled (session driver revoked)",
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => CancelCause::User,
            2 => CancelCause::CostCap,
            3 => CancelCause::Restart,
            4 => CancelCause::Host,
            5 => CancelCause::Driver,
            _ => return None,
        })
    }
}

impl TurnCompletion {
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn whose un-noted cancels are `cause`'s (see the type docs).
    pub fn with_default_cause(cause: CancelCause) -> Self {
        Self {
            default_cause: Some(cause),
            ..Self::default()
        }
    }

    pub fn completed(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn mark_completed(&self) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Note why the host is cancelling this turn. First cause wins: a
    /// teardown that revokes a driver and then cancels keeps its own cause.
    pub fn note_cancel_cause(&self, cause: CancelCause) {
        let _ = self.cancel_cause.compare_exchange(
            0,
            cause as u8,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }

    /// Leading words of this turn's canceled tool results.
    pub(crate) fn cancel_phrase(&self) -> &'static str {
        CancelCause::from_u8(self.cancel_cause.load(std::sync::atomic::Ordering::Acquire))
            .or(self.default_cause)
            .unwrap_or(CancelCause::User)
            .phrase()
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
