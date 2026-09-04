//! Identity, configuration, commands, events and the envelope — the entire
//! client↔session contract (PLAN-phase2 §2.1–§2.4). Everything a client can
//! send is serde-able from day one; `SessionEventWire::Stream` deliberately
//! is not (it carries the un-serialised `StreamEvent` in-process).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::view::RuntimeView;

// ── identity + configuration ──────────────────────────────────────────────────

/// Conversation session id — the `Session.id` string (`{name}-{ts}-{pid}`; see
/// events/registry.rs sanitize rules). NOT the tools::activation::SessionId
/// (that is the per-runtime gate identity minted at Runtime::from_parts).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Everything `EngineHost::create_session` needs. Serializable: it is the
/// payload of the wire `Attach::Create`. Mirrors `EngineOpts` minus the
/// host-level fields (profile, no_extensions), plus the per-session facts a
/// daemon cannot inherit from its own process (cwd, env overlay).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfig {
    /// `EngineOpts.continue_session`.
    #[serde(default)]
    pub continue_session: Option<Option<String>>,
    /// `EngineOpts.system`.
    #[serde(default)]
    pub system: Option<String>,
    /// `EngineOpts.prompt_manifest`.
    #[serde(default)]
    pub prompt_manifest: Option<PathBuf>,
    /// Per-session working directory. `None` = process cwd (in-process hosts
    /// pass `None` today → byte-identical). The daemon ALWAYS fills it from
    /// `Hello.cwd`.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// `Confirm` hook results auto-approved (server `--auto-approve-confirms`).
    /// Interactive hosts: false.
    #[serde(default)]
    pub auto_approve_confirms: bool,
    /// CLI `--model` override applied after session resolution.
    #[serde(default)]
    pub model_override: Option<String>,
    /// Save policy: only "save at all" — the TUI/chat/rpc always save; tests
    /// may disable. Default `true`.
    #[serde(default = "default_true")]
    pub persist: bool,
    /// Headless-chat policy: after every turn, save + `assess_context` and
    /// auto-compact when the engine budget says so (chat.rs post-turn block).
    /// The TUI does its own compaction; default `false`.
    #[serde(default)]
    pub auto_compact: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            continue_session: None,
            system: None,
            prompt_manifest: None,
            cwd: None,
            auto_approve_confirms: false,
            model_override: None,
            persist: true,
            auto_compact: false,
        }
    }
}

/// Serializable copy of `engine::setup::ContinueInfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinueInfoWire {
    pub session_id: String,
    /// "chain", "name", or None.
    pub resolved_via: Option<String>,
    pub query: String,
}

impl From<&crate::engine::setup::ContinueInfo> for ContinueInfoWire {
    fn from(c: &crate::engine::setup::ContinueInfo) -> Self {
        Self {
            session_id: c.session_id.clone(),
            resolved_via: c.resolved_via.clone(),
            query: c.query.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub name: Option<String>,
    pub model: String,
    pub cwd: Option<PathBuf>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub continued: bool,
    pub continue_info: Option<ContinueInfoWire>,
    /// Daemon pid (in-process: this process).
    pub host_pid: u32,
}

/// Minted by the actor per Attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub u64);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientMeta {
    pub kind: ClientKind,
    /// `$TERM`, informational.
    pub terminal: Option<String>,
    /// Client-generated uuid; reattach dedup (day 3).
    pub instance: String,
}

impl ClientMeta {
    /// Fresh meta with a random instance id and no terminal.
    pub fn new(kind: ClientKind) -> Self {
        Self {
            kind,
            terminal: None,
            instance: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Tui,
    Chat,
    Attach,
    Rpc,
    Server,
    Test,
}

/// Today: Mirror only is honoured; Takeover/Observe parse and map to Mirror
/// with a Notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    Mirror,
    Takeover,
    Observe,
}

/// Actor-side client accounting (§8): `Detached` never cancels a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachState {
    Attached(usize),
    Detached { running: bool },
}

// ── prompts ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Secret,
    Confirm,
}

/// Fixed title `resolve_before_tool_call_result` uses for Confirm prompts
/// (runtime/mod.rs). Everything else is `Secret`.
pub const CONFIRM_PROMPT_TITLE: &str = "Confirm tool call";

impl PromptKind {
    /// Derive the kind from the prompt title (the actor classifies at receipt).
    pub fn from_title(title: &str) -> Self {
        if title == CONFIRM_PROMPT_TITLE {
            Self::Confirm
        } else {
            Self::Secret
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    /// Per-session monotonic; actor dedups answers on it.
    pub id: u64,
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    pub raised_at: chrono::DateTime<chrono::Utc>,
}

// ── commands, settings, queries ───────────────────────────────────────────────

/// The entire client→session control surface (SEAMS §5.2/§5.3).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SessionCommand {
    /// User-authored prompt. Actor: reset auto-turn counter, fold
    /// abort_context, push user msg, start turn.
    Submit {
        text: String,
        #[serde(default)]
        attachments: Vec<agent_core::core::rpc_protocol::RpcAttachment>,
    },
    /// Text typed while streaming. Actor: steer if a steer_tx is live else
    /// queue; ALWAYS also sets queued_message.
    Steer { text: String },
    /// Esc. Cancel, capture abort context, dequeue, flush pending events,
    /// cancel subagents, save.
    Cancel,
    /// Answer to a PromptRequest. `None` = cancelled.
    /// NEVER journaled, NEVER replayed, NEVER traced.
    Answer { prompt_id: u64, value: Option<String> },
    Set(SessionSetting),
    Compact { instructions: Option<String> },
    /// `/new`: ConversationState::clear.
    NewSession,
    Save,
    /// Reply = `SessionEventWire::QueryResult { id, value }`.
    Query { id: u64, query: SessionQuery },
    /// Reply = `Attached { client_id, … }` on THIS client's channel only.
    Attach { client: ClientMeta, mode: AttachMode },
    Detach { client: ClientId },
    /// Host shutdown of this session: save → on_session_end → leases → Ended.
    End { reason: EndReason },
    /// Resync after Lagged (B, day 3): actor re-sends history + turn_replay.
    Resync { client: ClientId, since_seq: u64 },
    /// Runtime-backed slash command (`engine::commands::handle_engine_command`:
    /// /model /thinking /context /trace /memory /compact …). Reply =
    /// `QueryResult { id, value: {"kind": .., "text": ..} }`.
    EngineCommand { id: u64, name: String, arg: String },
    /// Host→session (never wire): the actor re-emits as `SessionEventWire`.
    #[serde(skip)]
    HostEvent(HostEvent),
}

/// Host-originated events pushed into a session (C3 router, loader).
#[derive(Debug, Clone)]
pub enum HostEvent {
    ExtensionNotification {
        extension_id: String,
        method: String,
        params: serde_json::Value,
    },
    LoaderProgress(crate::extensions::loader::ExtensionLoaderEvent),
}

/// The `&mut Runtime` setters the TUI/rpc/server call (SEAMS §5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "setting", rename_all = "snake_case")]
pub enum SessionSetting {
    /// try_set_model → SettingApplied::Model{applied, thinking_level}
    Model { model: String },
    /// set_reasoning_level_checked
    ReasoningLevel {
        #[serde(with = "reasoning_level_serde")]
        level: agent_core::reasoning::ReasoningLevel,
    },
    /// set_context_window
    ContextWindow { tokens: Option<u64> },
    CompactionModel { model: Option<String> },
    ApiRetries { n: u32 },
    SubagentTimeout { secs: u64 },
    MaxToolOutput { bytes: usize },
    BashTimeout { secs: u64 },
    BashMaxTimeout { secs: u64 },
    /// set_system_prompt
    SystemPrompt { text: String },
    /// reload_prompt() → SettingApplied::PromptReloaded{generation, source}
    ReloadPrompt,
    /// grant_worker_model
    GrantWorkerModel { model: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingApplied {
    pub setting: String,
    pub ok: bool,
    pub message: Option<String>,
    pub view: RuntimeView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum SessionQuery {
    /// {session, model, tokens, cost, streaming, auto_turns, attached, pending_prompts}
    Status,
    /// Vec<SharedMessage> (api_messages)
    Messages,
    /// Vec<SubagentDisplayRow>
    SubagentRows,
    /// runtime.prompt_inspection_json()
    PromptInspection,
    /// rpc_dispatch::build_tools_list_body input
    ToolsSchema,
    /// RuntimeView (cheap; LocalTransport answers without a hop)
    View,
    /// assess_context
    ContextAssessment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ClientQuit,
    HostShutdown,
    Evicted,
    Error,
}

// ── events + envelope ─────────────────────────────────────────────────────────

/// What a session tells its clients. `Stream` carries the un-serialised
/// StreamEvent in-process (LocalTransport) — that IS the byte-identical
/// contract. The socket boundary converts to `wire::WireSessionEvent`.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Attached carries the full snapshot by design (§2.4)
pub enum SessionEventWire {
    Stream(crate::StreamEvent),
    /// Actor bookkeeping the client needs to mirror `App` fields exactly.
    TurnStarted {
        turn_baseline: usize,
        trigger: TurnTrigger,
    },
    /// Emitted after every MessageHistory / Done / Error / Cancel / Compact so
    /// mirrors never diverge: the actor's api_messages IS the truth.
    Conversation(ConversationSnapshot),
    Prompt(PromptRequest),
    /// So mirrors dismiss the modal.
    PromptResolved { prompt_id: u64 },
    /// Drained event card — presentation only.
    External(crate::events::types::Event),
    AutoTurnCapReached { cap: u32 },
    /// The turn machine is idle: a stream ended (Done/Error/Cancel) and NO
    /// auto-turn followed. Headless hosts gate stdin on this so a queued
    /// auto-turn is never raced by the next line.
    Idle,
    /// "→ steering:" vs "queued:".
    Steered { text: String, delivered: bool },
    Dequeued { text: String },
    /// Any ChatMessage::System the actor would have pushed.
    SystemNotice(String),
    LoaderProgress(crate::extensions::loader::ExtensionLoaderEvent),
    ExtensionNotification {
        extension_id: String,
        method: String,
        params: serde_json::Value,
    },
    SettingChanged(SettingApplied),
    QueryResult { id: u64, value: serde_json::Value },
    /// Only to the attaching client.
    Attached {
        client: ClientId,
        snapshot: AttachSnapshot,
    },
    ClientJoined { client: ClientId, kind: ClientKind },
    ClientLeft { client: ClientId },
    Ended { reason: EndReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTrigger {
    User,
    QueuedAuto,
    EventAuto,
    PluginCommand,
    Compaction,
}

/// Per-session, gapless `seq` assigned at the single emit site
/// (`SessionActor::emit`); `ts` from the host clock.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub session_id: SessionId,
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub event: SessionEventWire,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

/// Serializable mirror of `ConversationState` (+ the actor's auto-turn
/// counter). Clients MUST replace, never merge, on `Conversation(_)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub api_messages: Vec<crate::SharedMessage>,
    pub tokens: ConversationTokens,
    pub cost: f64,
    pub abort_context: Option<String>,
    pub queued_message: Option<String>,
    pub pending_events_len: usize,
    pub consecutive_auto_turns: u32,
}

#[derive(Debug, Clone)]
pub struct AttachSnapshot {
    pub meta: SessionMeta,
    pub view: RuntimeView,
    pub conversation: ConversationSnapshot,
    pub streaming: bool,
    /// Current-turn events since TurnStarted (bounded ring) so a mid-turn
    /// attach can rebuild partial text/tool cards.
    pub replay: Vec<Envelope>,
    pub pending_prompts: Vec<PromptRequest>,
    pub clients: Vec<(ClientId, ClientKind)>,
}

/// `ReasoningLevel` has no serde impls in agent-core; go through its
/// canonical string form.
pub mod reasoning_level_serde {
    use agent_core::reasoning::ReasoningLevel;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(level: &ReasoningLevel, s: S) -> Result<S::Ok, S::Error> {
        level.as_str().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ReasoningLevel, D::Error> {
        let s = String::deserialize(d)?;
        ReasoningLevel::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown reasoning level `{s}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_kind_from_title() {
        assert_eq!(PromptKind::from_title("Confirm tool call"), PromptKind::Confirm);
        assert_eq!(PromptKind::from_title("API key"), PromptKind::Secret);
    }

    #[test]
    fn session_config_default_persists() {
        let cfg: SessionConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.persist);
        assert!(cfg.cwd.is_none());
        assert_eq!(
            serde_json::to_value(SessionConfig::default()).unwrap()["persist"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn command_json_round_trip() {
        let cmd = SessionCommand::Set(SessionSetting::ReasoningLevel {
            level: agent_core::reasoning::ReasoningLevel::Ultra,
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            json,
            r#"{"cmd":"set","setting":"reasoning_level","level":"ultra"}"#
        );
        let back: SessionCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            SessionCommand::Set(SessionSetting::ReasoningLevel {
                level: agent_core::reasoning::ReasoningLevel::Ultra
            })
        ));

        let submit: SessionCommand = serde_json::from_str(r#"{"cmd":"submit","text":"hi"}"#).unwrap();
        match submit {
            SessionCommand::Submit { text, attachments } => {
                assert_eq!(text, "hi");
                assert!(attachments.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn host_event_is_never_serialised() {
        let cmd = SessionCommand::HostEvent(HostEvent::LoaderProgress(
            crate::extensions::loader::ExtensionLoaderEvent::Started,
        ));
        assert!(serde_json::to_string(&cmd).is_err());
    }
}
