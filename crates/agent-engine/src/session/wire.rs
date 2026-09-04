//! Wire frames for the daemon UDS (PLAN-phase2 §2.9, S289 D-2 amended).
//!
//! Framing is `synaps rpc`'s: one JSON object per `\n`, at most
//! [`MAX_FRAME_BYTES`]. `WireSessionEvent` is a *lossless* serde mirror of
//! `SessionEventWire` — `StreamEvent`/`TurnError`/`ExtensionLoaderEvent` are
//! not `Serialize`, so they are mirrored variant-for-variant here and
//! converted only at the socket boundary. In-process transports never touch
//! this module.
//!
//! Security: nothing in here carries a credential. `Welcome`/`Attached` are
//! summaries; `ClientFrame`'s `Debug` redacts `Answer.value` and
//! `Submit.text` so a frame can be traced by *type* without leaking bodies.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::*;
use super::view::RuntimeView;
use crate::{AgentEvent, LlmEvent, SessionEvent, StreamEvent, TurnError, TurnOutcome};

/// Separate from `RPC_PROTOCOL_VERSION`; bump on any non-additive change.
pub const PROTOCOL_VERSION: u32 = 1;
/// Exact-match policy today (`min == max == PROTOCOL_VERSION`).
pub const PROTOCOL_MIN: u32 = PROTOCOL_VERSION;
pub const PROTOCOL_MAX: u32 = PROTOCOL_VERSION;
pub const MAX_FRAME_BYTES: usize = agent_core::core::rpc_dispatch::MAX_FRAME_BYTES;

/// The binary version the daemon/client report in the handshake.
pub fn binary_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ── client → daemon ───────────────────────────────────────────────────────────

/// Client → daemon. First frame on a connection MUST be `Hello`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    // ── lightweight control fast path: no session allocated, no Attach ──
    Ping,
    Sessions,
    Shutdown {
        #[serde(default)]
        force: bool,
    },
    // ── session-scoped ──
    Attach(Attach),
    Cmd {
        session_id: SessionId,
        cmd: SessionCommand,
    },
    Bye,
}

impl std::fmt::Debug for ClientFrame {
    /// Frame *types* only for anything that can carry user text or a secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hello(h) => f.debug_tuple("Hello").field(h).finish(),
            Self::Ping => f.write_str("Ping"),
            Self::Sessions => f.write_str("Sessions"),
            Self::Shutdown { force } => f.debug_struct("Shutdown").field("force", force).finish(),
            Self::Attach(a) => f.debug_tuple("Attach").field(a).finish(),
            Self::Cmd { session_id, cmd } => {
                let cmd: &dyn std::fmt::Debug = match cmd {
                    SessionCommand::Answer { prompt_id, value } => {
                        return f
                            .debug_struct("Cmd")
                            .field("session_id", session_id)
                            .field("cmd", &format_args!("Answer {{ prompt_id: {prompt_id}, value: <redacted {} chars> }}", value.as_ref().map_or(0, |v| v.chars().count())))
                            .finish();
                    }
                    SessionCommand::Submit { text, attachments } => {
                        return f
                            .debug_struct("Cmd")
                            .field("session_id", session_id)
                            .field("cmd", &format_args!("Submit {{ text: <{} chars>, attachments: {} }}", text.chars().count(), attachments.len()))
                            .finish();
                    }
                    SessionCommand::Steer { text } => {
                        return f
                            .debug_struct("Cmd")
                            .field("session_id", session_id)
                            .field("cmd", &format_args!("Steer {{ text: <{} chars> }}", text.chars().count()))
                            .finish();
                    }
                    other => other,
                };
                f.debug_struct("Cmd").field("session_id", session_id).field("cmd", cmd).finish()
            }
            Self::Bye => f.write_str("Bye"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub client: ClientMeta,
    pub cwd: PathBuf,
    pub client_version: String,
}

impl Hello {
    pub fn new(kind: ClientKind) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client: ClientMeta {
                terminal: std::env::var("TERM").ok(),
                ..ClientMeta::new(kind)
            },
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            client_version: binary_version(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "attach", rename_all = "snake_case")]
pub enum Attach {
    Existing {
        session_id: SessionId,
        #[serde(default = "mirror")]
        mode: AttachMode,
    },
    Create {
        #[serde(default)]
        config: SessionConfig,
        #[serde(default = "mirror")]
        mode: AttachMode,
    },
}

fn mirror() -> AttachMode {
    AttachMode::Mirror
}

// ── daemon → client ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum DaemonFrame {
    Welcome(Welcome),
    /// Then the daemon closes.
    Refused {
        reason: RefuseReason,
        message: String,
    },
    Pong {
        pid: u32,
        uptime_s: u64,
        sessions: usize,
    },
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    Attached(AttachedWire),
    Event(WireEnvelope),
    Error {
        session_id: Option<SessionId>,
        message: String,
    },
    Bye,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RefuseReason {
    Version {
        daemon_version: u32,
        min: u32,
        max: u32,
    },
    Protocol,
    Busy,
    UnknownSession,
    Config,
}

/// Auth-status *summary* only — never key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub daemon_version: String,
    pub pid: u32,
    pub profile: Option<String>,
    pub sessions: Vec<SessionMeta>,
    pub progressive_tool_disclosure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub session_id: SessionId,
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub event: WireSessionEvent,
}

impl From<Envelope> for WireEnvelope {
    fn from(e: Envelope) -> Self {
        Self {
            session_id: e.session_id,
            seq: e.seq,
            ts: e.ts,
            event: e.event.into(),
        }
    }
}

impl From<WireEnvelope> for Envelope {
    fn from(e: WireEnvelope) -> Self {
        Self {
            session_id: e.session_id,
            seq: e.seq,
            ts: e.ts,
            event: e.event.into(),
        }
    }
}

/// `AttachSnapshot` with a serialisable replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachedWire {
    pub client: ClientId,
    pub meta: SessionMeta,
    pub view: RuntimeView,
    pub conversation: ConversationSnapshot,
    pub streaming: bool,
    pub replay: Vec<WireEnvelope>,
    pub pending_prompts: Vec<PromptRequest>,
    pub clients: Vec<(ClientId, ClientKind)>,
}

impl AttachedWire {
    pub fn new(client: ClientId, s: AttachSnapshot) -> Self {
        Self {
            client,
            meta: s.meta,
            view: s.view,
            conversation: s.conversation,
            streaming: s.streaming,
            replay: s.replay.into_iter().map(Into::into).collect(),
            pending_prompts: s.pending_prompts,
            clients: s.clients,
        }
    }

    pub fn into_snapshot(self) -> (ClientId, AttachSnapshot) {
        (
            self.client,
            AttachSnapshot {
                meta: self.meta,
                view: self.view,
                conversation: self.conversation,
                streaming: self.streaming,
                replay: self.replay.into_iter().map(Into::into).collect(),
                pending_prompts: self.pending_prompts,
                clients: self.clients,
            },
        )
    }
}

// ── lossless mirror of SessionEventWire ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum WireSessionEvent {
    Stream { event: WireStreamEvent },
    TurnStarted { turn_baseline: usize, trigger: TurnTrigger },
    Conversation { snapshot: ConversationSnapshot },
    Prompt { request: PromptRequest },
    PromptResolved { prompt_id: u64 },
    External { event: crate::events::types::Event },
    AutoTurnCapReached { cap: u32 },
    Steered { text: String, delivered: bool },
    Dequeued { text: String },
    SystemNotice { text: String },
    LoaderProgress { event: WireLoaderEvent },
    ExtensionNotification { extension_id: String, method: String, params: serde_json::Value },
    SettingChanged { applied: SettingApplied },
    QueryResult { id: u64, value: serde_json::Value },
    Attached { client: ClientId, snapshot: AttachedWire },
    ClientJoined { client: ClientId, kind: ClientKind },
    ClientLeft { client: ClientId },
    Ended { reason: EndReason },
    /// Forward-compat: an additive variant from a newer daemon.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireStreamEvent {
    Llm(WireLlmEvent),
    Session(WireSessionStreamEvent),
    Agent(WireAgentEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "llm", rename_all = "snake_case")]
pub enum WireLlmEvent {
    Thinking { text: String },
    Text { text: String },
    ToolUseStart { tool_name: String, tool_id: String },
    ToolUseDelta { tool_id: String, delta: String },
    ToolUse { tool_name: String, tool_id: String, input: serde_json::Value },
    ToolResult { tool_id: String, result: String },
    ToolResultDelta { tool_id: String, delta: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "session", rename_all = "snake_case")]
pub enum WireSessionStreamEvent {
    MessageHistory { messages: Vec<crate::SharedMessage> },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: Option<String>,
    },
    Done,
    Error { message: String, outcome: TurnOutcome },
    Notice { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "agent", rename_all = "snake_case")]
pub enum WireAgentEvent {
    SubagentStart { subagent_id: u64, agent_name: String, task_preview: String },
    SubagentUpdate { subagent_id: u64, agent_name: String, status: String },
    SubagentDone { subagent_id: u64, agent_name: String, result_preview: String, duration_secs: f64 },
    SteeringDelivered { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireLoadFailure {
    pub plugin: String,
    pub manifest_path: Option<PathBuf>,
    pub reason: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "loader", rename_all = "snake_case")]
pub enum WireLoaderEvent {
    Started,
    Loaded { plugin: String, loaded: usize, failed: usize },
    Failed { failure: WireLoadFailure, loaded: usize, failed: usize },
    Finished { loaded: Vec<String>, failed: Vec<WireLoadFailure> },
}

type LoadFailure = crate::extensions::manager::ExtensionLoadFailure;
type LoaderEvent = crate::extensions::loader::ExtensionLoaderEvent;

impl From<LoadFailure> for WireLoadFailure {
    fn from(f: LoadFailure) -> Self {
        Self { plugin: f.plugin, manifest_path: f.manifest_path, reason: f.reason, hint: f.hint }
    }
}
impl From<WireLoadFailure> for LoadFailure {
    fn from(f: WireLoadFailure) -> Self {
        Self { plugin: f.plugin, manifest_path: f.manifest_path, reason: f.reason, hint: f.hint }
    }
}
impl From<LoaderEvent> for WireLoaderEvent {
    fn from(e: LoaderEvent) -> Self {
        match e {
            LoaderEvent::Started => Self::Started,
            LoaderEvent::Loaded { plugin, loaded, failed } => Self::Loaded { plugin, loaded, failed },
            LoaderEvent::Failed { failure, loaded, failed } => {
                Self::Failed { failure: failure.into(), loaded, failed }
            }
            LoaderEvent::Finished { loaded, failed } => Self::Finished {
                loaded,
                failed: failed.into_iter().map(Into::into).collect(),
            },
        }
    }
}
impl From<WireLoaderEvent> for LoaderEvent {
    fn from(e: WireLoaderEvent) -> Self {
        match e {
            WireLoaderEvent::Started => Self::Started,
            WireLoaderEvent::Loaded { plugin, loaded, failed } => Self::Loaded { plugin, loaded, failed },
            WireLoaderEvent::Failed { failure, loaded, failed } => {
                Self::Failed { failure: failure.into(), loaded, failed }
            }
            WireLoaderEvent::Finished { loaded, failed } => Self::Finished {
                loaded,
                failed: failed.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<StreamEvent> for WireStreamEvent {
    fn from(e: StreamEvent) -> Self {
        match e {
            StreamEvent::Llm(l) => Self::Llm(match l {
                LlmEvent::Thinking(text) => WireLlmEvent::Thinking { text },
                LlmEvent::Text(text) => WireLlmEvent::Text { text },
                LlmEvent::ToolUseStart { tool_name, tool_id } => WireLlmEvent::ToolUseStart { tool_name, tool_id },
                LlmEvent::ToolUseDelta { tool_id, delta } => WireLlmEvent::ToolUseDelta { tool_id, delta },
                LlmEvent::ToolUse { tool_name, tool_id, input } => WireLlmEvent::ToolUse { tool_name, tool_id, input },
                LlmEvent::ToolResult { tool_id, result } => WireLlmEvent::ToolResult { tool_id, result },
                LlmEvent::ToolResultDelta { tool_id, delta } => WireLlmEvent::ToolResultDelta { tool_id, delta },
            }),
            StreamEvent::Session(s) => Self::Session(match s {
                SessionEvent::MessageHistory(messages) => WireSessionStreamEvent::MessageHistory { messages },
                SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    model,
                } => WireSessionStreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    model,
                },
                SessionEvent::Done => WireSessionStreamEvent::Done,
                SessionEvent::Error(TurnError { message, outcome }) => WireSessionStreamEvent::Error { message, outcome },
                SessionEvent::Notice(text) => WireSessionStreamEvent::Notice { text },
            }),
            StreamEvent::Agent(a) => Self::Agent(match a {
                AgentEvent::SubagentStart { subagent_id, agent_name, task_preview } => {
                    WireAgentEvent::SubagentStart { subagent_id, agent_name, task_preview }
                }
                AgentEvent::SubagentUpdate { subagent_id, agent_name, status } => {
                    WireAgentEvent::SubagentUpdate { subagent_id, agent_name, status }
                }
                AgentEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs } => {
                    WireAgentEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs }
                }
                AgentEvent::SteeringDelivered { message } => WireAgentEvent::SteeringDelivered { message },
            }),
        }
    }
}

impl From<WireStreamEvent> for StreamEvent {
    fn from(e: WireStreamEvent) -> Self {
        match e {
            WireStreamEvent::Llm(l) => Self::Llm(match l {
                WireLlmEvent::Thinking { text } => LlmEvent::Thinking(text),
                WireLlmEvent::Text { text } => LlmEvent::Text(text),
                WireLlmEvent::ToolUseStart { tool_name, tool_id } => LlmEvent::ToolUseStart { tool_name, tool_id },
                WireLlmEvent::ToolUseDelta { tool_id, delta } => LlmEvent::ToolUseDelta { tool_id, delta },
                WireLlmEvent::ToolUse { tool_name, tool_id, input } => LlmEvent::ToolUse { tool_name, tool_id, input },
                WireLlmEvent::ToolResult { tool_id, result } => LlmEvent::ToolResult { tool_id, result },
                WireLlmEvent::ToolResultDelta { tool_id, delta } => LlmEvent::ToolResultDelta { tool_id, delta },
            }),
            WireStreamEvent::Session(s) => Self::Session(match s {
                WireSessionStreamEvent::MessageHistory { messages } => SessionEvent::MessageHistory(messages),
                WireSessionStreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    model,
                } => SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    model,
                },
                WireSessionStreamEvent::Done => SessionEvent::Done,
                WireSessionStreamEvent::Error { message, outcome } => SessionEvent::Error(TurnError { message, outcome }),
                WireSessionStreamEvent::Notice { text } => SessionEvent::Notice(text),
            }),
            WireStreamEvent::Agent(a) => Self::Agent(match a {
                WireAgentEvent::SubagentStart { subagent_id, agent_name, task_preview } => {
                    AgentEvent::SubagentStart { subagent_id, agent_name, task_preview }
                }
                WireAgentEvent::SubagentUpdate { subagent_id, agent_name, status } => {
                    AgentEvent::SubagentUpdate { subagent_id, agent_name, status }
                }
                WireAgentEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs } => {
                    AgentEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs }
                }
                WireAgentEvent::SteeringDelivered { message } => AgentEvent::SteeringDelivered { message },
            }),
        }
    }
}

impl From<SessionEventWire> for WireSessionEvent {
    fn from(e: SessionEventWire) -> Self {
        use SessionEventWire as S;
        match e {
            S::Stream(ev) => Self::Stream { event: ev.into() },
            S::TurnStarted { turn_baseline, trigger } => Self::TurnStarted { turn_baseline, trigger },
            S::Conversation(snapshot) => Self::Conversation { snapshot },
            S::Prompt(request) => Self::Prompt { request },
            S::PromptResolved { prompt_id } => Self::PromptResolved { prompt_id },
            S::External(event) => Self::External { event },
            S::AutoTurnCapReached { cap } => Self::AutoTurnCapReached { cap },
            S::Steered { text, delivered } => Self::Steered { text, delivered },
            S::Dequeued { text } => Self::Dequeued { text },
            S::SystemNotice(text) => Self::SystemNotice { text },
            S::LoaderProgress(ev) => Self::LoaderProgress { event: ev.into() },
            S::ExtensionNotification { extension_id, method, params } => {
                Self::ExtensionNotification { extension_id, method, params }
            }
            S::SettingChanged(applied) => Self::SettingChanged { applied },
            S::QueryResult { id, value } => Self::QueryResult { id, value },
            S::Attached { client, snapshot } => Self::Attached { client, snapshot: AttachedWire::new(client, snapshot) },
            S::ClientJoined { client, kind } => Self::ClientJoined { client, kind },
            S::ClientLeft { client } => Self::ClientLeft { client },
            S::Ended { reason } => Self::Ended { reason },
        }
    }
}

impl From<WireSessionEvent> for SessionEventWire {
    fn from(e: WireSessionEvent) -> Self {
        use WireSessionEvent as W;
        match e {
            W::Stream { event } => Self::Stream(event.into()),
            W::TurnStarted { turn_baseline, trigger } => Self::TurnStarted { turn_baseline, trigger },
            W::Conversation { snapshot } => Self::Conversation(snapshot),
            W::Prompt { request } => Self::Prompt(request),
            W::PromptResolved { prompt_id } => Self::PromptResolved { prompt_id },
            W::External { event } => Self::External(event),
            W::AutoTurnCapReached { cap } => Self::AutoTurnCapReached { cap },
            W::Steered { text, delivered } => Self::Steered { text, delivered },
            W::Dequeued { text } => Self::Dequeued { text },
            W::SystemNotice { text } => Self::SystemNotice(text),
            W::LoaderProgress { event } => Self::LoaderProgress(event.into()),
            W::ExtensionNotification { extension_id, method, params } => {
                Self::ExtensionNotification { extension_id, method, params }
            }
            W::SettingChanged { applied } => Self::SettingChanged(applied),
            W::QueryResult { id, value } => Self::QueryResult { id, value },
            W::Attached { snapshot, .. } => {
                let (client, snapshot) = snapshot.into_snapshot();
                Self::Attached { client, snapshot }
            }
            W::ClientJoined { client, kind } => Self::ClientJoined { client, kind },
            W::ClientLeft { client } => Self::ClientLeft { client },
            W::Ended { reason } => Self::Ended { reason },
            W::Unknown => Self::SystemNotice("unknown event from a newer daemon (ignored)".into()),
        }
    }
}

// ── framing helpers ───────────────────────────────────────────────────────────

/// Encode one frame as a single line (no embedded newline: serde_json escapes them).
pub fn encode_line<T: Serialize>(frame: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    Ok(s)
}

/// Decode one line, enforcing [`MAX_FRAME_BYTES`].
pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, String> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(format!("frame exceeds {} byte limit", MAX_FRAME_BYTES));
    }
    serde_json::from_str(line).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BudgetDimension;
    use std::sync::Arc;

    fn view() -> RuntimeView {
        crate::session::handle::echo::view_for()
    }

    fn meta() -> SessionMeta {
        crate::session::handle::echo::meta_for(&SessionId::from("wire-1"))
    }

    fn prompt() -> PromptRequest {
        PromptRequest {
            id: 3,
            kind: PromptKind::Secret,
            title: "sudo".into(),
            prompt: "password".into(),
            raised_at: chrono::Utc::now(),
        }
    }

    fn conv() -> ConversationSnapshot {
        ConversationSnapshot {
            api_messages: vec![Arc::new(serde_json::json!({"role":"user","content":"hi"}))],
            tokens: ConversationTokens { input: 1, output: 2, cache_read: 3, cache_creation: 4 },
            cost: 0.5,
            abort_context: Some("ctx".into()),
            queued_message: None,
            pending_events_len: 2,
            consecutive_auto_turns: 1,
        }
    }

    fn env(event: SessionEventWire) -> Envelope {
        Envelope { session_id: SessionId::from("wire-1"), seq: 9, ts: chrono::Utc::now(), event }
    }

    fn stream_fixtures() -> Vec<StreamEvent> {
        vec![
            StreamEvent::Llm(LlmEvent::Thinking("t".into())),
            StreamEvent::Llm(LlmEvent::Text("x\ny".into())),
            StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name: "bash".into(), tool_id: "id1".into() }),
            StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id: "id1".into(), delta: "{\"c".into() }),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name: "bash".into(),
                tool_id: "id1".into(),
                input: serde_json::json!({"command":"pwd"}),
            }),
            StreamEvent::Llm(LlmEvent::ToolResult { tool_id: "id1".into(), result: "/tmp".into() }),
            StreamEvent::Llm(LlmEvent::ToolResultDelta { tool_id: "id1".into(), delta: "/t".into() }),
            StreamEvent::Session(SessionEvent::MessageHistory(vec![Arc::new(serde_json::json!({"role":"user","content":"a"}))])),
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 4,
                cache_creation_5m: Some(5),
                cache_creation_1h: None,
                model: Some("m".into()),
            }),
            StreamEvent::Session(SessionEvent::Done),
            StreamEvent::Session(SessionEvent::Error(TurnError::provider("boom", "auth_error", "turn-1-0"))),
            StreamEvent::Session(SessionEvent::Error(TurnError::budget(BudgetDimension::ToolCalls))),
            StreamEvent::Session(SessionEvent::Error(TurnError { message: "c".into(), outcome: TurnOutcome::Canceled })),
            StreamEvent::Session(SessionEvent::Notice("n".into())),
            StreamEvent::Agent(AgentEvent::SubagentStart { subagent_id: 1, agent_name: "a".into(), task_preview: "p".into() }),
            StreamEvent::Agent(AgentEvent::SubagentUpdate { subagent_id: 1, agent_name: "a".into(), status: "s".into() }),
            StreamEvent::Agent(AgentEvent::SubagentDone {
                subagent_id: 1,
                agent_name: "a".into(),
                result_preview: "r".into(),
                duration_secs: 1.5,
            }),
            StreamEvent::Agent(AgentEvent::SteeringDelivered { message: "m".into() }),
        ]
    }

    fn fixtures() -> Vec<SessionEventWire> {
        use SessionEventWire as S;
        let loader = crate::extensions::loader::ExtensionLoaderEvent::Finished {
            loaded: vec!["p".into()],
            failed: vec![LoadFailure {
                plugin: "bad".into(),
                manifest_path: Some(PathBuf::from("/x/manifest.json")),
                reason: "oops".into(),
                hint: "fix".into(),
            }],
        };
        let mut v: Vec<S> = stream_fixtures().into_iter().map(S::Stream).collect();
        v.extend([
            S::TurnStarted { turn_baseline: 4, trigger: TurnTrigger::EventAuto },
            S::Conversation(conv()),
            S::Prompt(prompt()),
            S::PromptResolved { prompt_id: 3 },
            S::External(crate::events::types::Event::simple("cli", "hello", None)),
            S::AutoTurnCapReached { cap: 5 },
            S::Steered { text: "s".into(), delivered: true },
            S::Dequeued { text: "d".into() },
            S::SystemNotice("sys".into()),
            S::LoaderProgress(loader),
            S::ExtensionNotification { extension_id: "e".into(), method: "widget.upsert".into(), params: serde_json::json!({"a":1}) },
            S::SettingChanged(SettingApplied { setting: "model".into(), ok: true, message: None, view: view() }),
            S::QueryResult { id: 1, value: serde_json::json!([1, 2]) },
            S::Attached {
                client: ClientId(2),
                snapshot: AttachSnapshot {
                    meta: meta(),
                    view: view(),
                    conversation: conv(),
                    streaming: true,
                    replay: vec![env(S::Stream(StreamEvent::Llm(LlmEvent::Text("partial".into()))))],
                    pending_prompts: vec![prompt()],
                    clients: vec![(ClientId(1), ClientKind::Tui)],
                },
            },
            S::ClientJoined { client: ClientId(1), kind: ClientKind::Attach },
            S::ClientLeft { client: ClientId(1) },
            S::Ended { reason: EndReason::HostShutdown },
        ]);
        v
    }

    #[test]
    fn wire_roundtrip_every_variant() {
        let all = fixtures();
        // Every SessionEventWire variant (18) + every StreamEvent leaf (18; Stream counted once).
        assert_eq!(all.len(), 17 + 18);
        for ev in all {
            let before = format!("{:?}", env(ev.clone()));
            let wire: WireEnvelope = env(ev).into();
            let line = encode_line(&wire).unwrap();
            assert_eq!(line.matches('\n').count(), 1, "one line per frame");
            let back: WireEnvelope = decode_line(line.trim_end()).unwrap();
            let after = format!("{:?}", Envelope::from(back));
            assert_eq!(before, after);
        }
    }

    #[test]
    fn unknown_event_variant_tolerated() {
        let v: WireSessionEvent = serde_json::from_str(r#"{"ev":"from_the_future","x":1}"#).unwrap();
        assert!(matches!(v, WireSessionEvent::Unknown));
        assert!(matches!(SessionEventWire::from(v), SessionEventWire::SystemNotice(_)));
    }

    #[test]
    fn client_frames_roundtrip_and_daemon_frames_roundtrip() {
        let frames = vec![
            ClientFrame::Hello(Hello::new(ClientKind::Attach)),
            ClientFrame::Ping,
            ClientFrame::Sessions,
            ClientFrame::Shutdown { force: true },
            ClientFrame::Attach(Attach::Existing { session_id: "s".into(), mode: AttachMode::Mirror }),
            ClientFrame::Attach(Attach::Create { config: SessionConfig::default(), mode: AttachMode::Takeover }),
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Submit { text: "hi".into(), attachments: vec![] } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Answer { prompt_id: 1, value: Some("pw".into()) } },
            ClientFrame::Bye,
        ];
        for f in frames {
            let line = encode_line(&f).unwrap();
            let back: ClientFrame = decode_line(line.trim_end()).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line.trim_end());
        }
        let frames = vec![
            DaemonFrame::Welcome(Welcome {
                protocol_version: 1,
                daemon_version: "0.9.0".into(),
                pid: 1,
                profile: None,
                sessions: vec![meta()],
                progressive_tool_disclosure: true,
            }),
            DaemonFrame::Refused { reason: RefuseReason::Version { daemon_version: 1, min: 1, max: 1 }, message: "no".into() },
            DaemonFrame::Pong { pid: 1, uptime_s: 2, sessions: 0 },
            DaemonFrame::SessionList { sessions: vec![meta()] },
            DaemonFrame::Attached(AttachedWire::new(
                ClientId(1),
                AttachSnapshot {
                    meta: meta(),
                    view: view(),
                    conversation: conv(),
                    streaming: false,
                    replay: vec![],
                    pending_prompts: vec![],
                    clients: vec![],
                },
            )),
            DaemonFrame::Event(env(SessionEventWire::SystemNotice("x".into())).into()),
            DaemonFrame::Error { session_id: None, message: "e".into() },
            DaemonFrame::Bye,
        ];
        for f in frames {
            let line = encode_line(&f).unwrap();
            let back: DaemonFrame = decode_line(line.trim_end()).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line.trim_end());
        }
    }

    #[test]
    fn hello_must_be_first_is_a_type_level_fact_and_version_is_exact() {
        let h = Hello::new(ClientKind::Test);
        assert_eq!(h.protocol_version, PROTOCOL_VERSION);
        assert_eq!(PROTOCOL_MIN, PROTOCOL_MAX);
        let json = serde_json::to_string(&ClientFrame::Hello(h)).unwrap();
        assert!(json.starts_with(r#"{"type":"hello""#));
    }

    #[test]
    fn debug_redacts_answer_and_submit_bodies() {
        let secret = "hunter2-very-secret";
        let f = ClientFrame::Cmd {
            session_id: "s".into(),
            cmd: SessionCommand::Answer { prompt_id: 1, value: Some(secret.into()) },
        };
        let d = format!("{f:?}");
        assert!(!d.contains(secret), "{d}");
        assert!(d.contains("redacted"));
        let f = ClientFrame::Cmd {
            session_id: "s".into(),
            cmd: SessionCommand::Submit { text: "my private prompt".into(), attachments: vec![] },
        };
        let d = format!("{f:?}");
        assert!(!d.contains("private"), "{d}");
        let f = ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Steer { text: "steer me".into() } };
        assert!(!format!("{f:?}").contains("steer me"));
    }

    #[test]
    fn oversize_frame_rejected() {
        let big = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(decode_line::<ClientFrame>(&big).is_err());
    }

    #[test]
    fn no_secret_bearing_fields_on_handshake_frames() {
        // Structural guard: the JSON of Welcome/Attached must not contain key-ish names.
        let w = serde_json::to_string(&Welcome {
            protocol_version: 1,
            daemon_version: "v".into(),
            pid: 1,
            profile: Some("p".into()),
            sessions: vec![meta()],
            progressive_tool_disclosure: false,
        })
        .unwrap()
        .to_lowercase();
        for bad in ["token", "secret", "api_key", "apikey", "password", "credential"] {
            assert!(!w.contains(bad), "{bad} in Welcome");
        }
    }
}
