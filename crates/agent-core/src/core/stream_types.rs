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

impl StreamEvent {
    /// How a client sees this event: [`SessionEvent::for_clients`] for a
    /// session event; every other event unchanged.
    pub fn for_clients(self) -> Option<Self> {
        match self {
            Self::Session(event) => event.for_clients().map(Self::Session),
            other => Some(other),
        }
    }
}

#[derive(Debug, Clone)]
pub enum LlmEvent {
    /// Marks a new uncommitted provider response (not a new turn).
    ResponseStart,
    /// Discard previews since ResponseStart. History/tools/usage are unchanged.
    ResponseReset,
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
        /// The result is memory-only (the typed take result): `result` is
        /// the engine's fixed placeholder or notice, and the raw text went
        /// to the live provider request only.
        memory_only: bool,
        /// Set only on a client tool's result (contract §8): who answered
        /// and how. `None` for every other tool.
        client_tool: Option<Box<ClientToolStamp>>,
    },
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },
    /// The call `tool_id` is waiting behind another call in its scheduler
    /// lane (serial / same-key lane) and has NOT started executing.
    /// Opt-in: only calls that actually wait get it, always before their
    /// [`LlmEvent::ToolStarted`]. A consumer that ignores both keeps the
    /// old behaviour (every call "runs" from its `ToolUse`).
    ToolQueued {
        tool_id: String,
    },
    /// A call previously announced by [`LlmEvent::ToolQueued`] has begun
    /// executing. Never sent for a call that was not queued.
    ToolStarted {
        tool_id: String,
    },
}

/// A client tool result's stamp (client tools contract §8). Built by the
/// daemon from the pending invocation's registration and the validated
/// return code, never from result text or any other reply field. It holds
/// no arguments and no content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientToolStamp {
    pub registration_id: String,
    /// `None` when no invoke was sent (busy, unregistered, not_root,
    /// args_too_large).
    pub invocation_id: Option<String>,
    pub extension_id: String,
    pub tool: String,
    pub family: String,
    /// A declared outcome code, or a `synaps_*` code.
    pub outcome: String,
    pub memory_only: bool,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    MessageHistory(Vec<SharedMessage>),
    /// A sealed archive exists; persist this replacement active head and then
    /// acknowledge. The runtime must not dispatch another provider round until
    /// the frontend reports a durable save. Unsupported consumers fail closed.
    ContextHeadCheckpoint {
        session_id: String,
        messages: Vec<SharedMessage>,
        receipt: crate::core::context_head::ContextHeadReceipt,
    },
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
    /// One billable upstream attempt's usage, emitted by a runtime that
    /// keeps a run's request ledger (a worker run with rollover on) in
    /// place of the `Usage` it would otherwise send for that attempt, never
    /// both. Host-internal: clients see the plain `Usage`
    /// ([`SessionEvent::for_clients`]); it is never serialized.
    UsageAttempt {
        request: UsageRequest,
        usage: UsageCounts,
    },
    /// A ledgered attempt whose spend is not measured: an open liability
    /// (unknown spend) or its release (it provably cost nothing).
    /// Host-internal: clients never see it ([`SessionEvent::for_clients`]).
    UsageLiability {
        request: UsageRequest,
        estimate_micro_usd: u64,
        state: LiabilityState,
    },
    Done,
    Error(TurnError),
    /// Transient status line — display-only, never persisted.
    Notice(String),
    /// The provider pushed back (429, 5xx, a stream error, a refusal, an
    /// incomplete response) and a retry is pending. Shown like
    /// [`Notice`](Self::Notice); clients also show a retrying state, which
    /// they must not infer from notice text. On the session wire it is a
    /// `notice` with `retry: true`, so older peers read it as a notice.
    Retry(String),
}

impl SessionEvent {
    /// How a client (a session, the wire, a frontend) sees this event. A
    /// run's request-ledger events are host-internal: a ledgered attempt's
    /// usage is the plain `Usage` it stands in for, attributed to the
    /// attempt's model; a liability is not shown (`None`). Every other
    /// event is returned unchanged.
    pub fn for_clients(self) -> Option<Self> {
        match self {
            Self::UsageAttempt { request, usage } => Some(Self::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                cache_creation_5m: usage.cache_creation_5m,
                cache_creation_1h: usage.cache_creation_1h,
                model: Some(request.model),
            }),
            Self::UsageLiability { .. } => None,
            other => Some(other),
        }
    }
}

/// Why a ledgered request was made: a same-model summary's usage stays
/// attributable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePurpose {
    Conversation,
    RolloverSummary,
}

/// The host identity of one billable upstream attempt in a run's request
/// ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRequest {
    /// Host-assigned, unique within the run.
    pub request_id: String,
    pub purpose: UsagePurpose,
    /// The model the attempt is priced against.
    pub model: String,
}

/// Provider-reported token counts of one attempt (the counters of
/// [`SessionEvent::Usage`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// Cache-write TTL split. `None` when the API omits the breakdown.
    pub cache_creation_5m: Option<u64>,
    pub cache_creation_1h: Option<u64>,
}

/// The state a [`SessionEvent::UsageLiability`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiabilityState {
    /// Unknown spend: the estimate is held.
    Open,
    /// The attempt provably cost nothing: the liability ended.
    Released,
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
    /// Uncached input + output tokens summed over one stream turn.
    TotalTokens,
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
            Self::TotalTokens => "total_tokens",
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

    /// Interrupted after a possible-but-unconfirmed side effect (spec §8.3):
    /// cancellation or transport failure landed with a call that had started
    /// (side effect possible) but not recorded a result. Metadata only — the
    /// static message names the interrupted call, never its content. The
    /// operation is never automatically rerun.
    pub fn interrupted_after_side_effect(call_id: impl Into<String>) -> Self {
        let call_id = call_id.into();
        Self {
            message: format!(
                "tool call interrupted after a possible side effect (call {call_id}); not rerun"
            ),
            outcome: TurnOutcome::InterruptedAfterSideEffect { call_id },
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

#[cfg(test)]
mod ledger_event_tests {
    use super::*;

    fn request(purpose: UsagePurpose) -> UsageRequest {
        UsageRequest {
            request_id: "req-1".into(),
            purpose,
            model: "provider/model-b".into(),
        }
    }

    /// A ledgered attempt's usage reaches clients as the plain `Usage` it
    /// stands in for, attributed to its model.
    #[test]
    fn a_usage_attempt_reaches_clients_as_the_plain_usage() {
        let usage = UsageCounts {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
            cache_creation_5m: Some(1),
            cache_creation_1h: None,
        };
        for purpose in [UsagePurpose::Conversation, UsagePurpose::RolloverSummary] {
            let event = SessionEvent::UsageAttempt {
                request: request(purpose),
                usage: usage.clone(),
            };
            match event.for_clients() {
                Some(SessionEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cache_read_input_tokens: 3,
                    cache_creation_input_tokens: 4,
                    cache_creation_5m: Some(1),
                    cache_creation_1h: None,
                    model: Some(model),
                }) => assert_eq!(model, "provider/model-b"),
                other => panic!("expected the plain usage, got {other:?}"),
            }
        }
    }

    /// A liability is host-internal: no client event, open or released.
    #[test]
    fn a_liability_never_reaches_clients() {
        for state in [LiabilityState::Open, LiabilityState::Released] {
            let event = SessionEvent::UsageLiability {
                request: request(UsagePurpose::Conversation),
                estimate_micro_usd: 7,
                state,
            };
            assert!(event.for_clients().is_none());
        }
    }

    /// Every other event passes through unchanged.
    #[test]
    fn other_events_pass_through() {
        assert!(matches!(
            SessionEvent::Done.for_clients(),
            Some(SessionEvent::Done)
        ));
        assert!(matches!(
            SessionEvent::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_creation_5m: None,
                cache_creation_1h: None,
                model: None,
            }
            .for_clients(),
            Some(SessionEvent::Usage { model: None, .. })
        ));
        assert!(matches!(
            SessionEvent::Notice("n".into()).for_clients(),
            Some(SessionEvent::Notice(t)) if t == "n"
        ));
    }
}
