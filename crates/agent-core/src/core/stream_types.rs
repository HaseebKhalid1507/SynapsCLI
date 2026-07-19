//! Core streaming event types shared across agent-core (rpc_dispatch) and
//! the engine layer (runtime, engine). Defined here so `agent-core` is a
//! clean leaf with no upward dependencies into the engine layer.

use serde_json::Value;
use std::sync::Arc;

/// Reference-counted shared message payload. Introduced at the runtime's
/// inner boundary (T128 Slice 2) to enable clone-free fan-out of message
/// history through the stream loop and `SessionEvent::MessageHistory`.
/// Outer state (App/Session/ConversationState) remains `Vec<Value>` and
/// shims at the boundary — see Slice 5 for full conversion.
pub type SharedMessage = Arc<Value>;

/// Top-level stream event — grouped into concern-specific sub-enums.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Llm(LlmEvent),
    Session(SessionEvent),
    Agent(AgentEvent),
}

#[derive(Debug, Clone)]
pub enum LlmEvent {
    Thinking(String),
    Text(String),
    /// A tool-use block has begun streaming. `tool_id` is the call id the
    /// model will reference in its eventual `tool_use` content block.
    ToolUseStart {
        tool_name: String,
        tool_id: String,
    },
    /// Streaming chunk of a tool's input JSON.
    ToolUseDelta {
        tool_id: String,
        delta: String,
    },
    ToolUse {
        tool_name: String,
        tool_id: String,
        input: Value,
    },
    ToolResult {
        tool_id: String,
        result: String,
    },
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    MessageHistory(Vec<SharedMessage>),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        /// Cache-write TTL split. `None` when the API omits the breakdown.
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: Option<String>,
    },
    Done,
    Error(TurnError),
    /// Transient status line — display-only, never persisted.
    Notice(String),
}

/// Budget dimension placeholder (spec §5.2). The full turn-budget system is
/// later work (spec §8.1); only the typed surface lands here so
/// `TurnOutcome::BudgetExceeded` is representable without re-plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    InputTokens,
    OutputTokens,
    ToolCalls,
    WallClock,
    /// Provider rounds within one stream turn (spec §8.1).
    ProviderRounds,
    /// Accumulated tool-result bytes within one stream turn (spec §8.1).
    ToolResultBytes,
    /// Accumulated estimated USD cost within one stream turn (spec §8.1).
    CostUsd,
}

impl BudgetDimension {
    /// Stable wire/diagnostic label (matches the serde snake_case form).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::ToolCalls => "tool_calls",
            Self::WallClock => "wall_clock",
            Self::ProviderRounds => "provider_rounds",
            Self::ToolResultBytes => "tool_result_bytes",
            Self::CostUsd => "cost_usd",
        }
    }
}

/// Typed terminal outcome of a model turn (spec §5.2). Produced ONCE by the
/// engine; every frontend (chat, TUI, RPC, server, watcher, subagents)
/// receives the same value and must never re-derive it from message text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Canceled,
    ProviderFailed {
        code: String,
        correlation_id: String,
    },
    ToolFailed {
        tool_id: String,
        correlation_id: String,
    },
    BudgetExceeded {
        dimension: BudgetDimension,
    },
    InterruptedAfterSideEffect {
        call_id: String,
    },
}

impl TurnOutcome {
    /// The correlation ID tying this outcome to engine trace/log lines,
    /// when the variant carries one.
    pub fn correlation_id(&self) -> Option<&str> {
        match self {
            TurnOutcome::ProviderFailed { correlation_id, .. }
            | TurnOutcome::ToolFailed { correlation_id, .. } => Some(correlation_id),
            _ => None,
        }
    }

    /// True for outcomes that must surface as a non-success terminal state.
    pub fn is_failure(&self) -> bool {
        !matches!(self, TurnOutcome::Completed | TurnOutcome::Canceled)
    }
}

/// Typed terminal failure carried by [`SessionEvent::Error`]: the
/// human-readable message plus the spec §5.2 [`TurnOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnError {
    pub message: String,
    pub outcome: TurnOutcome,
}

impl TurnError {
    /// Typed budget exhaustion (spec §8.1): static message naming the
    /// dimension only — no counts, no content.
    pub fn budget(dimension: BudgetDimension) -> Self {
        Self {
            message: format!("turn budget exhausted ({})", dimension.as_str()),
            outcome: TurnOutcome::BudgetExceeded { dimension },
        }
    }

    /// Provider-category failure with an explicit correlation ID.
    pub fn provider(
        message: impl Into<String>,
        code: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            outcome: TurnOutcome::ProviderFailed {
                code: code.into(),
                correlation_id: correlation_id.into(),
            },
        }
    }

    /// Uniform terminal-category suffix shared by every frontend, e.g.
    /// `provider_failed code=auth_error correlation=turn-123-0`. Metadata
    /// only — never includes message content.
    pub fn category_label(&self) -> String {
        match &self.outcome {
            TurnOutcome::Completed => "completed".to_string(),
            TurnOutcome::Canceled => "canceled".to_string(),
            TurnOutcome::ProviderFailed {
                code,
                correlation_id,
            } => format!("provider_failed code={code} correlation={correlation_id}"),
            TurnOutcome::ToolFailed {
                tool_id,
                correlation_id,
            } => format!("tool_failed tool={tool_id} correlation={correlation_id}"),
            TurnOutcome::BudgetExceeded { dimension } => {
                format!("budget_exceeded dimension={dimension:?}")
            }
            TurnOutcome::InterruptedAfterSideEffect { call_id } => {
                format!("interrupted_after_side_effect call={call_id}")
            }
        }
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Process-unique correlation ID for one engine turn. Ties the typed
/// terminal outcome to metadata-only trace/log lines for the same turn.
pub fn next_turn_correlation_id() -> String {
    static TURN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TURN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("turn-{}-{}", std::process::id(), seq)
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    SubagentStart {
        subagent_id: u64,
        agent_name: String,
        task_preview: String,
    },
    SubagentUpdate {
        subagent_id: u64,
        agent_name: String,
        status: String,
    },
    SubagentDone {
        subagent_id: u64,
        agent_name: String,
        result_preview: String,
        duration_secs: f64,
    },
    SteeringDelivered {
        message: String,
    },
}
