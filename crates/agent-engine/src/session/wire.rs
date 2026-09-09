//! Wire frames for the daemon UDS (PLAN-phase2 §2.9, S289 D-2 amended).
//!
//! Framing is `synaps rpc`'s: one JSON object per `\n`, at most
//! [`DAEMON_MAX_FRAME_BYTES`] (64 MiB — distinct from rpc's 1 MiB because
//! `Attached` carries the whole `api_messages`). Enforced symmetrically:
//! `encode_line` refuses to build an oversize frame, `decode_line` and both
//! readers refuse to accept one. `WireSessionEvent` is a serde mirror of
//! `SessionEventWire` — `StreamEvent`/`TurnError`/`ExtensionLoaderEvent` are
//! not `Serialize`, so they are mirrored variant-for-variant here and
//! converted only at the socket boundary. In-process transports never touch
//! this module.
//!
//! The one *lossy* mirror is `Conversation`: on the wire it is a
//! [`ConversationDigest`] (len + hash + tokens/cost), never the messages.
//! Full history travels only in `Attached` and `QueryResult{Messages}`;
//! `SocketTransport` rebuilds the snapshot from its local mirror
//! (`Attached` + `Stream(MessageHistory)`) and re-queries on a hash miss.
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
/// v2 (phase 3): `Set` became a struct variant, `Bye` gained a reason.
/// Protocol stays exact-match; only *binary* versions compare directionally
/// (reload, §2.8).
pub const PROTOCOL_VERSION: u32 = 2;
/// Exact-match policy today (`min == max == PROTOCOL_VERSION`).
pub const PROTOCOL_MIN: u32 = PROTOCOL_VERSION;
pub const PROTOCOL_MAX: u32 = PROTOCOL_VERSION;
/// rpc's cap, kept for reference; daemon frames use [`DAEMON_MAX_FRAME_BYTES`].
pub const RPC_MAX_FRAME_BYTES: usize = agent_core::core::rpc_dispatch::MAX_FRAME_BYTES;
/// Hard cap on one daemon frame, both directions (`Attached` ships the
/// whole conversation; 64 MiB is well past any context window's JSON).
pub const DAEMON_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Alias used by the readers/writers of this protocol.
pub const MAX_FRAME_BYTES: usize = DAEMON_MAX_FRAME_BYTES;
/// Query ids at or above this are reserved for the transport/daemon and
/// never surfaced to a client as `QueryResult`.
pub const RESERVED_QUERY_ID_BASE: u64 = 1 << 63;
/// `SocketTransport` re-fetches `api_messages` under this id on a digest miss.
pub const DIGEST_RESYNC_QUERY_ID: u64 = RESERVED_QUERY_ID_BASE;
/// The daemon's idle monitor probes `Status` under this id.
pub const IDLE_PROBE_QUERY_ID: u64 = RESERVED_QUERY_ID_BASE + 1;
/// `Checkpoint` replies `QueryResult{id: CHECKPOINT_QUERY_ID, {ok:true}}`
/// so `reload.rs` can await it per session.
pub const CHECKPOINT_QUERY_ID: u64 = RESERVED_QUERY_ID_BASE + 2;

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
    /// Control fast path (no session): checkpoint every session, write
    /// reload-state, execv self. Reply: `Reloading` to every attached
    /// client, `Bye` to this one. (C3)
    Reload {
        #[serde(default)]
        now: bool,
        #[serde(default)]
        drain_secs: Option<u64>,
        #[serde(default)]
        exe: Option<PathBuf>,
    },
    /// Control fast path: `memstat::purge_arenas()` in the daemon (bench
    /// hygiene). Reply: `Pong`. (C2)
    Purge,
    Bye,
}

impl std::fmt::Debug for ClientFrame {
    /// Frame *types* only for anything that can carry user text or a secret
    /// (`SessionCommand`'s own `Debug` redacts bodies without lengths).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hello(h) => f.debug_tuple("Hello").field(h).finish(),
            Self::Ping => f.write_str("Ping"),
            Self::Sessions => f.write_str("Sessions"),
            Self::Shutdown { force } => f.debug_struct("Shutdown").field("force", force).finish(),
            Self::Attach(a) => f.debug_tuple("Attach").field(a).finish(),
            Self::Cmd { session_id, cmd } => {
                f.debug_struct("Cmd").field("session_id", session_id).field("cmd", cmd).finish()
            }
            Self::Reload { now, drain_secs, exe } => f
                .debug_struct("Reload")
                .field("now", now)
                .field("drain_secs", drain_secs)
                .field("exe", exe)
                .finish(),
            Self::Purge => f.write_str("Purge"),
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
    /// Set when re-connecting after `Reloading`/EOF (C3/A4).
    #[serde(default)]
    pub reconnect_of: Option<ClientReconnect>,
}

/// Who this connection used to be, so the daemon can restore ownership by
/// reconnect order + `was_owner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientReconnect {
    pub previous_client: ClientId,
    pub session_id: SessionId,
    pub was_owner: bool,
    pub generation: u64,
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
            reconnect_of: None,
        }
    }

    /// Ask for `Full` (mirror) or `Digest` (digest + display tail) history.
    pub fn with_history(mut self, history: HistoryMode) -> Self {
        self.client.history = history;
        self
    }

    /// Display items wanted in `Attached.display_tail` / `DisplayTail` queries.
    pub fn with_tail_items(mut self, tail_items: usize) -> Self {
        self.client.tail_items = tail_items;
        self
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
    /// `Some(Reloading{..})` before exec; `None` otherwise (today).
    Bye {
        #[serde(default)]
        reason: Option<ByeReason>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "bye", rename_all = "snake_case")]
pub enum ByeReason {
    Reloading { generation: u64, retry_after_ms: u64 },
    Shutdown,
    Detached,
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
    /// `synaps daemon reload` version gate failed (C3).
    ReloadRefused { why: String },
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
    /// Reload counter (starts at 1).
    #[serde(default = "one")]
    pub generation: u64,
}

fn one() -> u64 {
    1
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
    #[serde(default)]
    pub input_owner: Option<ClientId>,
    #[serde(default)]
    pub display_tail: Option<crate::session::display::DisplayTail>,
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
            input_owner: s.input_owner,
            display_tail: s.display_tail,
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
                input_owner: self.input_owner,
                display_tail: self.display_tail,
            },
        )
    }
}

// ── conversation digest ───────────────────────────────────────────────────────

/// `ConversationSnapshot` minus `api_messages`: what `Conversation` carries
/// on the wire. `messages_hash` is FNV-1a over each message's JSON so a
/// client can tell whether its local mirror is current.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationDigest {
    #[serde(default)]
    pub header: SessionHeader,
    pub messages_len: usize,
    pub messages_hash: u64,
    pub tokens: ConversationTokens,
    pub cost: f64,
    pub abort_context: Option<String>,
    pub queued_message: Option<String>,
    pub pending_events_len: usize,
    pub consecutive_auto_turns: u32,
}

/// FNV-1a 64 over the canonical JSON of every message (stable across
/// binaries, unlike `DefaultHasher`).
pub fn messages_hash(messages: &[crate::SharedMessage]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for m in messages {
        if let Ok(bytes) = serde_json::to_vec(&**m) {
            for b in bytes {
                feed(b);
            }
        }
        feed(0x1f);
    }
    h
}

impl ConversationDigest {
    pub fn of(s: &ConversationSnapshot) -> Self {
        Self {
            header: s.header.clone(),
            messages_len: s.api_messages.len(),
            messages_hash: messages_hash(&s.api_messages),
            tokens: s.tokens.clone(),
            cost: s.cost,
            abort_context: s.abort_context.clone(),
            queued_message: s.queued_message.clone(),
            pending_events_len: s.pending_events_len,
            consecutive_auto_turns: s.consecutive_auto_turns,
        }
    }

    pub fn matches(&self, messages: &[crate::SharedMessage]) -> bool {
        self.messages_len == messages.len() && self.messages_hash == messages_hash(messages)
    }

    /// Rebuild a snapshot around `messages` (the caller's mirror).
    pub fn into_snapshot(self, api_messages: Vec<crate::SharedMessage>) -> ConversationSnapshot {
        ConversationSnapshot {
            header: self.header,
            messages_len: self.messages_len,
            api_messages,
            tokens: self.tokens,
            cost: self.cost,
            abort_context: self.abort_context,
            queued_message: self.queued_message,
            pending_events_len: self.pending_events_len,
            consecutive_auto_turns: self.consecutive_auto_turns,
        }
    }
}

// ── mirror of SessionEventWire (lossless except Conversation → digest) ────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // mirrors SessionEventWire by design
pub enum WireSessionEvent {
    Stream { event: WireStreamEvent },
    TurnStarted {
        turn_baseline: usize,
        trigger: TurnTrigger,
        #[serde(default)]
        user_text: Option<String>,
    },
    /// Digest only — see module docs.
    Conversation { digest: ConversationDigest },
    Prompt { request: PromptRequest },
    PromptResolved { prompt_id: u64 },
    External { event: crate::events::types::Event },
    AutoTurnCapReached { cap: u32 },
    Idle,
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
    Aborted { context_saved: bool },
    Cleared { session_id: String },
    CompactionStarted { source: String, disclosure: String },
    CompactionApplied {
        previous_session_id: String,
        session_id: String,
        chains_advanced: Vec<String>,
        queued_restored: Option<String>,
        msg_count: usize,
    },
    CompactionFailed { message: String, panicked: bool },
    CompactionCancelled,
    SubagentRows { rows: Vec<crate::runtime::subagent::SubagentDisplayRow> },
    Resumed {
        id: u64,
        old_id: String,
        new_id: String,
        via: Option<String>,
        clamp_notice: Option<String>,
    },
    InputOwnerChanged { from: Option<ClientId>, to: Option<ClientId>, reason: OwnerChangeReason },
    Refused { client: ClientId, command: String, reason: String },
    AttachRefused { message: String },
    Lifecycle { lifecycle: SessionLifecycle },
    Reloading { generation: u64, retry_after_ms: u64 },
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
            S::TurnStarted { turn_baseline, trigger, user_text } => {
                Self::TurnStarted { turn_baseline, trigger, user_text }
            }
            S::Conversation(snapshot) => Self::Conversation { digest: ConversationDigest::of(&snapshot) },
            S::Prompt(request) => Self::Prompt { request },
            S::PromptResolved { prompt_id } => Self::PromptResolved { prompt_id },
            S::External(event) => Self::External { event },
            S::AutoTurnCapReached { cap } => Self::AutoTurnCapReached { cap },
            S::Idle => Self::Idle,
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
            S::Aborted { context_saved } => Self::Aborted { context_saved },
            S::Cleared { session_id } => Self::Cleared { session_id },
            S::CompactionStarted { source, disclosure } => Self::CompactionStarted { source, disclosure },
            S::CompactionApplied { previous_session_id, session_id, chains_advanced, queued_restored, msg_count } => {
                Self::CompactionApplied { previous_session_id, session_id, chains_advanced, queued_restored, msg_count }
            }
            S::CompactionFailed { message, panicked } => Self::CompactionFailed { message, panicked },
            S::CompactionCancelled => Self::CompactionCancelled,
            S::SubagentRows(rows) => Self::SubagentRows { rows },
            S::Resumed { id, old_id, new_id, via, clamp_notice } => {
                Self::Resumed { id, old_id, new_id, via, clamp_notice }
            }
            S::InputOwnerChanged { from, to, reason } => Self::InputOwnerChanged { from, to, reason },
            S::Refused { client, command, reason } => Self::Refused { client, command, reason },
            S::AttachRefused { message } => Self::AttachRefused { message },
            S::Lifecycle(lifecycle) => Self::Lifecycle { lifecycle },
            S::Reloading { generation, retry_after_ms } => Self::Reloading { generation, retry_after_ms },
        }
    }
}

impl From<WireSessionEvent> for SessionEventWire {
    fn from(e: WireSessionEvent) -> Self {
        use WireSessionEvent as W;
        match e {
            W::Stream { event } => Self::Stream(event.into()),
            W::TurnStarted { turn_baseline, trigger, user_text } => {
                Self::TurnStarted { turn_baseline, trigger, user_text }
            }
            // Messages are not on the wire; `SocketTransport` fills them from
            // its mirror before handing the envelope up.
            W::Conversation { digest } => Self::Conversation(digest.into_snapshot(Vec::new())),
            W::Prompt { request } => Self::Prompt(request),
            W::PromptResolved { prompt_id } => Self::PromptResolved { prompt_id },
            W::External { event } => Self::External(event),
            W::AutoTurnCapReached { cap } => Self::AutoTurnCapReached { cap },
            W::Idle => Self::Idle,
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
            W::Aborted { context_saved } => Self::Aborted { context_saved },
            W::Cleared { session_id } => Self::Cleared { session_id },
            W::CompactionStarted { source, disclosure } => Self::CompactionStarted { source, disclosure },
            W::CompactionApplied { previous_session_id, session_id, chains_advanced, queued_restored, msg_count } => {
                Self::CompactionApplied { previous_session_id, session_id, chains_advanced, queued_restored, msg_count }
            }
            W::CompactionFailed { message, panicked } => Self::CompactionFailed { message, panicked },
            W::CompactionCancelled => Self::CompactionCancelled,
            W::SubagentRows { rows } => Self::SubagentRows(rows),
            W::Resumed { id, old_id, new_id, via, clamp_notice } => {
                Self::Resumed { id, old_id, new_id, via, clamp_notice }
            }
            W::InputOwnerChanged { from, to, reason } => Self::InputOwnerChanged { from, to, reason },
            W::Refused { client, command, reason } => Self::Refused { client, command, reason },
            W::AttachRefused { message } => Self::AttachRefused { message },
            W::Lifecycle { lifecycle } => Self::Lifecycle(lifecycle),
            W::Reloading { generation, retry_after_ms } => Self::Reloading { generation, retry_after_ms },
            W::Unknown => Self::SystemNotice("unknown event from a newer daemon (ignored)".into()),
        }
    }
}

// ── framing helpers ───────────────────────────────────────────────────────────

/// Human-readable form of the cap for error frames.
pub fn frame_limit_msg() -> String {
    format!("frame exceeds {} MiB limit", DAEMON_MAX_FRAME_BYTES / (1024 * 1024))
}

/// Encode one frame as a single line (no embedded newline: serde_json
/// escapes them). Refuses to produce a line over [`DAEMON_MAX_FRAME_BYTES`].
pub fn encode_line<T: Serialize>(frame: &T) -> Result<String, String> {
    let mut s = serde_json::to_string(frame).map_err(|e| e.to_string())?;
    if s.len() + 1 > DAEMON_MAX_FRAME_BYTES {
        return Err(frame_limit_msg());
    }
    s.push('\n');
    Ok(s)
}

/// Decode one line, enforcing [`DAEMON_MAX_FRAME_BYTES`].
pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, String> {
    if line.len() > DAEMON_MAX_FRAME_BYTES {
        return Err(frame_limit_msg());
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
            header: SessionHeader {
                id: "wire-1".into(),
                title: "t".into(),
                name: None,
                model: "m".into(),
                thinking_level: "off".into(),
                system_prompt: Some("sys".into()),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                parent_session: Some("p".into()),
            },
            api_messages: vec![Arc::new(serde_json::json!({"role":"user","content":"hi"}))],
            messages_len: 1,
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
            S::TurnStarted { turn_baseline: 4, trigger: TurnTrigger::EventAuto, user_text: None },
            S::TurnStarted { turn_baseline: 5, trigger: TurnTrigger::QueuedAuto, user_text: Some("q".into()) },
            S::Conversation(conv()),
            S::Prompt(prompt()),
            S::PromptResolved { prompt_id: 3 },
            S::External(crate::events::types::Event::simple("cli", "hello", None)),
            S::AutoTurnCapReached { cap: 5 },
            S::Idle,
            S::Steered { text: "s".into(), delivered: true },
            S::Dequeued { text: "d".into() },
            S::SystemNotice("sys".into()),
            S::LoaderProgress(loader),
            S::ExtensionNotification { extension_id: "e".into(), method: "widget.upsert".into(), params: serde_json::json!({"a":1}) },
            S::SettingChanged(SettingApplied {
                id: 3,
                setting: "model".into(),
                ok: true,
                message: None,
                view: view(),
                clamp: Some(ReasoningClampWire { from: "ultra".into(), to: "high".into() }),
            }),
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
                    input_owner: Some(ClientId(1)),
                    display_tail: None,
                },
            },
            S::ClientJoined { client: ClientId(1), kind: ClientKind::Attach },
            S::ClientLeft { client: ClientId(1) },
            S::Ended { reason: EndReason::HostShutdown },
            S::Aborted { context_saved: true },
            S::Cleared { session_id: "new-1".into() },
            S::CompactionStarted { source: "manual".into(), disclosure: "disc".into() },
            S::CompactionApplied {
                previous_session_id: "old".into(),
                session_id: "new".into(),
                chains_advanced: vec!["c1".into()],
                queued_restored: Some("q".into()),
                msg_count: 12,
            },
            S::CompactionFailed { message: "boom".into(), panicked: false },
            S::CompactionCancelled,
            S::SubagentRows(vec![crate::runtime::subagent::SubagentDisplayRow {
                subagent_id: 1,
                agent_name: "a".into(),
                status: crate::runtime::subagent::SubagentStatus::Failed("x".into()),
                cancel_requested: false,
                elapsed_secs: 1.25,
                finished_elapsed: Some(std::time::Duration::from_millis(1250)),
            }]),
            S::Resumed { id: 2, old_id: "o".into(), new_id: "n".into(), via: Some("name".into()), clamp_notice: None },
            S::InputOwnerChanged { from: Some(ClientId(1)), to: Some(ClientId(2)), reason: OwnerChangeReason::Takeover },
            S::Refused { client: ClientId(2), command: "submit".into(), reason: "input owned by client #1".into() },
            S::AttachRefused { message: "parked".into() },
            S::Lifecycle(SessionLifecycle::Parked),
            S::Reloading { generation: 2, retry_after_ms: 500 },
        ]);
        v
    }

    #[test]
    fn wire_roundtrip_every_variant() {
        let all = fixtures();
        // Every non-Stream SessionEventWire variant (31; TurnStarted twice = 32) + every StreamEvent leaf (18).
        assert_eq!(all.len(), 32 + 18);
        for ev in all {
            let is_conv = matches!(ev, SessionEventWire::Conversation(_));
            let e = env(ev);
            let before = format!("{e:?}");
            let wire: WireEnvelope = e.into();
            let line = encode_line(&wire).unwrap();
            assert_eq!(line.matches('\n').count(), 1, "one line per frame");
            let back: WireEnvelope = decode_line(line.trim_end()).unwrap();
            let after = format!("{:?}", Envelope::from(back));
            if is_conv {
                // Digest on the wire: everything but the messages survives.
                assert!(!line.contains("api_messages"), "{line}");
                assert!(after.contains("api_messages: []"), "{after}");
                assert_eq!(before.replace("api_messages: [Object {\"content\": String(\"hi\"), \"role\": String(\"user\")}]", "api_messages: []"), after);
            } else {
                assert_eq!(before, after);
            }
        }
    }

    #[test]
    fn hello_history_fields_default_and_roundtrip() {
        // Old peers omit the fields: Full / 120.
        let legacy = r#"{"kind":"tui","terminal":null,"instance":"i"}"#;
        let m: ClientMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.history, HistoryMode::Full);
        assert_eq!(m.tail_items, DEFAULT_TAIL_ITEMS);
        let h = Hello::new(ClientKind::Tui).with_history(HistoryMode::Digest).with_tail_items(9);
        let line = encode_line(&ClientFrame::Hello(h)).unwrap();
        assert!(line.contains("\"history\":\"digest\""), "{line}");
        assert!(line.contains("\"tail_items\":9"), "{line}");
        match decode_line::<ClientFrame>(line.trim_end()).unwrap() {
            ClientFrame::Hello(h) => {
                assert_eq!(h.client.history, HistoryMode::Digest);
                assert_eq!(h.client.tail_items, 9);
            }
            other => panic!("{other:?}"),
        }
        // Digest snapshots keep messages_len even with api_messages empty.
        let d = ConversationDigest::of(&conv());
        let snap = d.into_snapshot(Vec::new());
        assert_eq!(snap.messages_len, 1);
        assert!(snap.api_messages.is_empty());
        let legacy: ConversationSnapshot = serde_json::from_str(r#"{"api_messages":[],"tokens":{"input":0,"output":0,"cache_read":0,"cache_creation":0},"cost":0.0,"abort_context":null,"queued_message":null,"pending_events_len":0,"consecutive_auto_turns":0}"#).unwrap();
        assert_eq!(legacy.messages_len, 0);
    }

    #[test]
    fn conversation_digest_roundtrips_and_detects_drift() {
        let c = conv();
        let d = ConversationDigest::of(&c);
        assert_eq!(d.messages_len, 1);
        assert!(d.matches(&c.api_messages));
        assert!(!d.matches(&[]));
        let mut other = c.api_messages.clone();
        other.push(Arc::new(serde_json::json!({"role":"assistant","content":"x"})));
        assert!(!d.matches(&other));
        let back = d.clone().into_snapshot(c.api_messages.clone());
        assert_eq!(ConversationDigest::of(&back), d);
        // stable across runs/binaries: FNV-1a offset basis for the empty list
        assert_eq!(messages_hash(&[]), 0xcbf2_9ce4_8422_2325);
        assert_ne!(messages_hash(&c.api_messages), messages_hash(&[]));
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
            ClientFrame::Hello(Hello::new(ClientKind::Tui).with_history(HistoryMode::Digest).with_tail_items(7)),
            ClientFrame::Ping,
            ClientFrame::Sessions,
            ClientFrame::Shutdown { force: true },
            ClientFrame::Attach(Attach::Existing { session_id: "s".into(), mode: AttachMode::Mirror }),
            ClientFrame::Attach(Attach::Create { config: SessionConfig::default(), mode: AttachMode::Takeover }),
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Submit { text: "hi".into(), attachments: vec![] } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Answer { prompt_id: 1, value: Some("pw".into()) } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::EngineCommand { id: 4, name: "model".into(), arg: "x".into() } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Query { id: 5, query: SessionQuery::Status } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Set { id: 6, setting: SessionSetting::ReloadPrompt } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::Checkpoint { reason: CheckpointReason::Reload } },
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::KeepWarm { on: true } },
            ClientFrame::Reload { now: true, drain_secs: Some(3), exe: Some(PathBuf::from("/bin/synaps")) },
            ClientFrame::Purge,
            ClientFrame::Bye,
        ];
        for f in frames {
            let line = encode_line(&f).unwrap();
            let back: ClientFrame = decode_line(line.trim_end()).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line.trim_end());
        }
        let frames = vec![
            DaemonFrame::Welcome(Welcome {
                protocol_version: 2,
                daemon_version: "0.9.0".into(),
                pid: 1,
                profile: None,
                sessions: vec![meta()],
                progressive_tool_disclosure: true,
                generation: 1,
            }),
            DaemonFrame::Refused { reason: RefuseReason::Version { daemon_version: 2, min: 2, max: 2 }, message: "no".into() },
            DaemonFrame::Refused { reason: RefuseReason::ReloadRefused { why: "older".into() }, message: "no".into() },
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
                    input_owner: None,
                    display_tail: None,
                },
            )),
            DaemonFrame::Event(env(SessionEventWire::SystemNotice("x".into())).into()),
            DaemonFrame::Error { session_id: None, message: "e".into() },
            DaemonFrame::Bye { reason: None },
            DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation: 2, retry_after_ms: 500 }) },
            DaemonFrame::Bye { reason: Some(ByeReason::Shutdown) },
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
        assert_eq!(PROTOCOL_VERSION, 2);
        // v1 frames (no `reconnect_of`, bare `bye`) still parse — the refusal is by version, not by shape.
        let h: Hello = serde_json::from_str(
            r#"{"protocol_version":1,"client":{"kind":"tui","terminal":null,"instance":"i"},"cwd":"/","client_version":"0"}"#,
        )
        .unwrap();
        assert!(h.reconnect_of.is_none());
        let b: DaemonFrame = serde_json::from_str(r#"{"type":"bye"}"#).unwrap();
        assert!(matches!(b, DaemonFrame::Bye { reason: None }));
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
        let d = format!("{f:?}");
        assert!(!d.contains("steer me"));
        // No lengths either: a char count is a side channel.
        assert!(!d.contains("chars"), "{d}");
        assert!(!d.contains('8'), "{d}");
    }

    #[test]
    fn oversize_frame_rejected_both_directions() {
        assert!(DAEMON_MAX_FRAME_BYTES > RPC_MAX_FRAME_BYTES);
        let big = "x".repeat(DAEMON_MAX_FRAME_BYTES + 1);
        assert!(decode_line::<ClientFrame>(&big).unwrap_err().contains("64 MiB"));
        // encode side: a frame that would serialise past the cap is refused
        let f = DaemonFrame::Error { session_id: None, message: big };
        assert!(encode_line(&f).unwrap_err().contains("64 MiB"));
        // a > 1 MiB (rpc cap) frame is fine on the daemon wire
        let f = DaemonFrame::Error { session_id: None, message: "y".repeat(RPC_MAX_FRAME_BYTES * 2) };
        let line = encode_line(&f).unwrap();
        assert!(decode_line::<DaemonFrame>(line.trim_end()).is_ok());
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
            generation: 1,
        })
        .unwrap()
        .to_lowercase();
        for bad in ["token", "secret", "api_key", "apikey", "password", "credential"] {
            assert!(!w.contains(bad), "{bad} in Welcome");
        }
    }
}

#[cfg(test)]
mod float_roundtrip_tests {
    use super::*;

    /// L≡S (phase 4 G9): the session cost crosses the wire as an f64. Without
    /// serde_json's `float_roundtrip` the parse is one ulp short for values
    /// like `0.000105 + 0.000045` and the footer rounds `$0.0002` → `$0.0001`.
    #[test]
    fn conversation_digest_cost_survives_the_wire_exactly() {
        let cost = (10f64 / 1_000_000.0) * 3.0 + (5f64 / 1_000_000.0) * 15.0
            + ((10f64 / 1_000_000.0) * 3.0 + (1f64 / 1_000_000.0) * 15.0);
        let snap = ConversationSnapshot { cost, ..Default::default() };
        let d = ConversationDigest::of(&snap);
        let line = encode_line(&d).unwrap();
        let back: ConversationDigest = decode_line(line.trim_end()).unwrap();
        assert_eq!(back.cost.to_bits(), cost.to_bits(), "{line}");
        assert_eq!(format!("{:.4}", back.cost), format!("{:.4}", cost));
    }
}
