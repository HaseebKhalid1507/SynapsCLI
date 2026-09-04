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
    /// Compaction transition policy: `InPlace` (default; chat, today's
    /// actor) or `LinkedSuccessor` (the TUI's successor-session chain).
    #[serde(default)]
    pub compaction_policy: CompactionPolicyWire,
    /// Block `create` on process-level extension discovery before emitting
    /// `on_session_start` (chat/daemon). The TUI passes `false`: the actor
    /// emits it from a spawned waiter so boot never blocks on discovery.
    #[serde(default = "default_true")]
    pub await_extensions: bool,
    /// Never `Park` this session (`--keep-warm`). Default `false`.
    #[serde(default)]
    pub keep_warm: bool,
}

fn default_true() -> bool {
    true
}

/// Serializable mirror of `runtime::compaction::CompactionPolicy`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPolicyWire {
    #[default]
    InPlace,
    LinkedSuccessor,
}

impl From<CompactionPolicyWire> for crate::runtime::compaction::CompactionPolicy {
    fn from(p: CompactionPolicyWire) -> Self {
        match p {
            CompactionPolicyWire::InPlace => Self::InPlace,
            CompactionPolicyWire::LinkedSuccessor => Self::LinkedSuccessor,
        }
    }
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
            compaction_policy: CompactionPolicyWire::InPlace,
            await_extensions: true,
            keep_warm: false,
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
    // ── filled at list time from the handle's cells (meta itself is frozen) ──
    #[serde(default)]
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
    pub clients: usize,
    #[serde(default)]
    pub input_owner: Option<ClientId>,
    #[serde(default)]
    pub awaiting_input: usize,
    /// `conv.session.id` — differs from `id` after a LinkedSuccessor compaction.
    #[serde(default)]
    pub journal_id: String,
}

/// Where a session is in its park/unpark life (`SessionHandle::lifecycle`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    #[default]
    Live = 0,
    Parking = 1,
    Parked = 2,
    Ending = 3,
}

impl SessionLifecycle {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Parking,
            2 => Self::Parked,
            3 => Self::Ending,
            _ => Self::Live,
        }
    }
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
    /// How much history this client wants mirrored (phase 4).
    #[serde(default)]
    pub history: HistoryMode,
    /// Display items in `Attached.display_tail` / `DisplayTail` queries (phase 4).
    #[serde(default = "default_tail_items")]
    pub tail_items: usize,
}

/// Default for `ClientMeta.tail_items`: matches the TUI's resumed-display cap.
pub const DEFAULT_TAIL_ITEMS: usize = 120;

fn default_tail_items() -> usize {
    DEFAULT_TAIL_ITEMS
}

/// `SYNAPS_ATTACH_TAIL_ITEMS` (client side): display items requested in
/// `Attached.display_tail`; `DEFAULT_TAIL_ITEMS` when unset/invalid.
pub fn tail_items_from_env() -> usize {
    std::env::var("SYNAPS_ATTACH_TAIL_ITEMS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TAIL_ITEMS)
}

impl ClientMeta {
    /// Fresh meta with a random instance id and no terminal.
    pub fn new(kind: ClientKind) -> Self {
        Self {
            kind,
            terminal: None,
            instance: uuid::Uuid::new_v4().to_string(),
            history: HistoryMode::default(),
            tail_items: tail_items_from_env(),
        }
    }
}

/// What a client wants in `Attached` / `Conversation`: the full
/// `api_messages` mirror (`Full`, the 741b6b60 behaviour) or only the
/// digest + a daemon-side display tail (`Digest`, phase 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    #[default]
    Full,
    Digest,
}

impl HistoryMode {
    /// `SYNAPS_CLIENT_HISTORY=full|digest`; `default` wins when unset or
    /// unrecognised.
    pub fn from_env_or(default: Self) -> Self {
        match std::env::var("SYNAPS_CLIENT_HISTORY").ok().as_deref().map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("full") => Self::Full,
            Some(v) if v.eq_ignore_ascii_case("digest") => Self::Digest,
            _ => default,
        }
    }

    /// What `synaps --attach` asks for (phase 4 B7: `Digest`).
    /// `SYNAPS_CLIENT_HISTORY=full` restores the 741b6b60 mirror wholesale.
    pub fn attach_client_default() -> Self {
        Self::Digest
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

/// Attach mode. Ownership (B1): `Mirror` owns input iff nobody does,
/// `Takeover` steals it, `Observe` never owns it.
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
    /// Save + PTY teardown in progress (B3).
    Parking,
    /// `runtime`/`conv` dropped; `background` alive; unpark on Attach/wake (B3).
    Parked,
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

/// A command with its origin. `from: None` = host-originated (router,
/// lifecycle, idle probe) and bypasses input ownership (B1).
#[derive(Debug)]
pub struct Addressed {
    pub from: Option<ClientId>,
    pub cmd: SessionCommand,
}

/// Why a `Checkpoint` was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    Reload,
    HostRequest,
}

/// The entire client→session control surface (SEAMS §5.2/§5.3).
///
/// `Debug` is manual: `Submit`/`Steer`/`SubmitPrepared`/`Answer`/`Resume`
/// print `<redacted>` (no lengths) so a command can be traced by *type*.
#[derive(Serialize, Deserialize)]
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
    /// `id` is client-chosen and echoed in `SettingChanged.id`.
    Set { id: u64, setting: SessionSetting },
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
    /// (A3) dispatch.rs LoadSkill — pre-built tool_use/tool_result pair
    /// (+ optional user text) then a turn. Does NOT fold abort_context and
    /// does NOT reset consecutive_auto_turns.
    SubmitPrepared {
        messages: Vec<crate::SharedMessage>,
        #[serde(default)]
        user_text: Option<String>,
    },
    /// (A3) tools-backed (non-interactive) plugin command on THE runtime's
    /// tool set. Reply = `QueryResult{id, {kind:"plugin_output", ..}}`.
    PluginCommand { id: u64, plugin: String, name: String, arg: String },
    /// (A3) `/resume`: save current, load `query`, restore model/reasoning/
    /// system prompt, swap conversation. Reply = `Resumed{id, ..}`.
    Resume { id: u64, query: String },
    /// (B1, used by C3 reload) cancel any turn (abort_context captured),
    /// abort compaction, save, close PTYs, emit Notice. Never ends the
    /// session. Reply = `QueryResult{id: CHECKPOINT_QUERY_ID, {ok:true}}`.
    Checkpoint { reason: CheckpointReason },
    /// (B3) pin/unpin (`--keep-warm`, `/keep-warm on|off`).
    KeepWarm { on: bool },
    /// Host-only (never wire): park now if `can_park()` (reload rehydrates
    /// a Parked session Parked). A no-op otherwise.
    #[serde(skip)]
    Park,
    /// Host→session (never wire): the actor re-emits as `SessionEventWire`.
    #[serde(skip)]
    HostEvent(HostEvent),
}

/// What `Checkpoint{Reload}` reports so `daemon reload` can rebuild the
/// session as it IS (not as it was created): the actor's config, keep-warm
/// pin, lifecycle, the non-persisted knobs (`settings_replay`, incl.
/// `/system`), and the CURRENT model/thinking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionReloadRecord {
    pub config: SessionConfig,
    pub keep_warm: bool,
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
    pub settings_replay: Vec<SessionSetting>,
    pub model: String,
    pub thinking_level: String,
}

impl std::fmt::Debug for SessionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submit { attachments, .. } => f
                .debug_struct("Submit")
                .field("text", &format_args!("<redacted>"))
                .field("attachments", &attachments.len())
                .finish(),
            Self::Steer { .. } => f
                .debug_struct("Steer")
                .field("text", &format_args!("<redacted>"))
                .finish(),
            Self::SubmitPrepared { .. } => f
                .debug_struct("SubmitPrepared")
                .field("messages", &format_args!("<redacted>"))
                .field("user_text", &format_args!("<redacted>"))
                .finish(),
            Self::Answer { prompt_id, .. } => f
                .debug_struct("Answer")
                .field("prompt_id", prompt_id)
                .field("value", &format_args!("<redacted>"))
                .finish(),
            Self::Resume { id, .. } => f
                .debug_struct("Resume")
                .field("id", id)
                .field("query", &format_args!("<redacted>"))
                .finish(),
            Self::Cancel => f.write_str("Cancel"),
            Self::Set { id, setting } => {
                f.debug_struct("Set").field("id", id).field("setting", setting).finish()
            }
            Self::Compact { instructions } => {
                f.debug_struct("Compact").field("instructions", instructions).finish()
            }
            Self::NewSession => f.write_str("NewSession"),
            Self::Save => f.write_str("Save"),
            Self::Park => f.write_str("Park"),
            Self::Query { id, query } => {
                f.debug_struct("Query").field("id", id).field("query", query).finish()
            }
            Self::Attach { client, mode } => {
                f.debug_struct("Attach").field("client", client).field("mode", mode).finish()
            }
            Self::Detach { client } => f.debug_struct("Detach").field("client", client).finish(),
            Self::End { reason } => f.debug_struct("End").field("reason", reason).finish(),
            Self::Resync { client, since_seq } => f
                .debug_struct("Resync")
                .field("client", client)
                .field("since_seq", since_seq)
                .finish(),
            Self::EngineCommand { id, name, arg } => f
                .debug_struct("EngineCommand")
                .field("id", id)
                .field("name", name)
                .field("arg", arg)
                .finish(),
            Self::PluginCommand { id, plugin, name, arg } => f
                .debug_struct("PluginCommand")
                .field("id", id)
                .field("plugin", plugin)
                .field("name", name)
                .field("arg", arg)
                .finish(),
            Self::Checkpoint { reason } => {
                f.debug_struct("Checkpoint").field("reason", reason).finish()
            }
            Self::KeepWarm { on } => f.debug_struct("KeepWarm").field("on", on).finish(),
            Self::HostEvent(ev) => f.debug_tuple("HostEvent").field(ev).finish(),
        }
    }
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
    /// Correlates with `Set{id}`.
    #[serde(default)]
    pub id: u64,
    pub setting: String,
    pub ok: bool,
    pub message: Option<String>,
    pub view: RuntimeView,
    /// `{from, to}` when a model change clamped the reasoning level, so the
    /// TUI renders `reasoning_clamp_notice` byte-for-byte.
    #[serde(default)]
    pub clamp: Option<ReasoningClampWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningClampWire {
    pub from: String,
    pub to: String,
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
    /// `engine::commands::context_command(runtime, Some(api_messages))` text.
    ContextReport,
    /// `display::DisplayTail` of the last `items` display items (phase 4).
    DisplayTail { items: usize },
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
    /// `user_text` = the queued text on `QueuedAuto` (TUI pushes the User
    /// card + scroll); `None` otherwise.
    TurnStarted {
        turn_baseline: usize,
        trigger: TurnTrigger,
        user_text: Option<String>,
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
    /// Cancel landed. TUI: drop_empty_thinking, push Error(abort_msg),
    /// subagents.clear(), streaming=false.
    Aborted { context_saved: bool },
    /// `/clear`. TUI: transcript.clear, counters=0, "new session started".
    Cleared { session_id: String },
    /// (B2) `disclosure` = preview_compaction_disclosure().render_line().
    CompactionStarted { source: String, disclosure: String },
    CompactionApplied {
        previous_session_id: String,
        session_id: String,
        chains_advanced: Vec<String>,
        queued_restored: Option<String>,
        msg_count: usize,
    },
    /// "compaction failed: {e}" vs "compaction task panicked: {e}".
    CompactionFailed { message: String, panicked: bool },
    CompactionCancelled,
    /// `runtime.subagent_registry().display_rows()` — at Done/Error and at
    /// 1 Hz while any row is non-terminal.
    SubagentRows(Vec<crate::runtime::subagent::SubagentDisplayRow>),
    /// `/resume` reply (`Resume{id}`).
    Resumed {
        id: u64,
        old_id: String,
        new_id: String,
        via: Option<String>,
        clamp_notice: Option<String>,
    },
    /// (B1) sent to the previous owner on takeover, and to everyone on any
    /// owner change.
    InputOwnerChanged {
        from: Option<ClientId>,
        to: Option<ClientId>,
        reason: OwnerChangeReason,
    },
    /// (B1) a non-owner sent an input command. Only the sender renders it.
    Refused { client: ClientId, command: String, reason: String },
    /// (B3) targeted like `Attached`: the attach could not be honoured.
    AttachRefused { message: String },
    /// (B3) lifecycle transitions for mirrors and `sessions` listings.
    Lifecycle(SessionLifecycle),
    /// (C3) daemon is about to exec itself; clients reconnect.
    Reloading { generation: u64, retry_after_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerChangeReason {
    Attach,
    Takeover,
    OwnerDetached,
    Reload,
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
    /// The TUI's `App.session` mirror (never `api_messages`).
    #[serde(default)]
    pub header: SessionHeader,
    pub api_messages: Vec<crate::SharedMessage>,
    /// `api_messages.len()` on the daemon — always filled by the actor;
    /// `api_messages` itself may be empty for `HistoryMode::Digest` clients.
    #[serde(default)]
    pub messages_len: usize,
    pub tokens: ConversationTokens,
    pub cost: f64,
    pub abort_context: Option<String>,
    pub queued_message: Option<String>,
    pub pending_events_len: usize,
    pub consecutive_auto_turns: u32,
}

/// `Session` minus `api_messages` and accounting: what a client mirrors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub id: String,
    pub title: String,
    pub name: Option<String>,
    pub model: String,
    pub thinking_level: String,
    pub system_prompt: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub parent_session: Option<String>,
}

impl From<&crate::Session> for SessionHeader {
    fn from(s: &crate::Session) -> Self {
        Self {
            id: s.id.clone(),
            title: s.title.clone(),
            name: s.name.clone(),
            model: s.model.clone(),
            thinking_level: s.thinking_level.clone(),
            system_prompt: s.system_prompt.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            parent_session: s.parent_session.clone(),
        }
    }
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
    /// Who owns input right now (B1) — so a joiner knows immediately
    /// whether its keystrokes will be honoured.
    pub input_owner: Option<ClientId>,    /// Daemon-projected display tail — `Some` iff the client attached with
    /// `HistoryMode::Digest` (`conversation.api_messages` is then empty).
    pub display_tail: Option<crate::session::display::DisplayTail>,
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
        let cmd = SessionCommand::Set {
            id: 7,
            setting: SessionSetting::ReasoningLevel {
                level: agent_core::reasoning::ReasoningLevel::Ultra,
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            json,
            r#"{"cmd":"set","id":7,"setting":{"setting":"reasoning_level","level":"ultra"}}"#
        );
        let back: SessionCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            SessionCommand::Set {
                id: 7,
                setting: SessionSetting::ReasoningLevel {
                    level: agent_core::reasoning::ReasoningLevel::Ultra
                }
            }
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
    fn new_commands_round_trip() {
        let cmds = vec![
            SessionCommand::SubmitPrepared {
                messages: vec![std::sync::Arc::new(serde_json::json!({"role":"user","content":"x"}))],
                user_text: Some("t".into()),
            },
            SessionCommand::PluginCommand { id: 1, plugin: "p".into(), name: "n".into(), arg: "a".into() },
            SessionCommand::Resume { id: 2, query: "q".into() },
            SessionCommand::Checkpoint { reason: CheckpointReason::Reload },
            SessionCommand::KeepWarm { on: true },
            SessionCommand::Query { id: 3, query: SessionQuery::ContextReport },
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: SessionCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
        let cp: SessionCommand = serde_json::from_str(r#"{"cmd":"checkpoint","reason":"host_request"}"#).unwrap();
        assert!(matches!(cp, SessionCommand::Checkpoint { reason: CheckpointReason::HostRequest }));
    }

    #[test]
    fn command_debug_redacts_bodies_without_lengths() {
        let secret = "hunter2-very-secret";
        let cmds = vec![
            SessionCommand::Submit { text: secret.into(), attachments: vec![] },
            SessionCommand::Steer { text: secret.into() },
            SessionCommand::SubmitPrepared {
                messages: vec![std::sync::Arc::new(serde_json::json!({"content": secret}))],
                user_text: Some(secret.into()),
            },
            SessionCommand::Answer { prompt_id: 1, value: Some(secret.into()) },
            SessionCommand::Resume { id: 1, query: secret.into() },
        ];
        for cmd in cmds {
            let d = format!("{cmd:?}");
            assert!(!d.contains(secret), "{d}");
            assert!(d.contains("<redacted>"), "{d}");
            assert!(!d.contains("chars"), "{d}");
            assert!(!d.contains(&secret.len().to_string()), "{d}");
        }
        let d = format!("{:?}", SessionCommand::Set { id: 4, setting: SessionSetting::ReloadPrompt });
        assert_eq!(d, "Set { id: 4, setting: ReloadPrompt }");
    }

    #[test]
    fn session_config_new_fields_default() {
        let cfg: SessionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.compaction_policy, CompactionPolicyWire::InPlace);
        assert!(cfg.await_extensions);
        assert!(!cfg.keep_warm);
        let cfg: SessionConfig =
            serde_json::from_str(r#"{"compaction_policy":"linked_successor","await_extensions":false}"#).unwrap();
        assert_eq!(cfg.compaction_policy, CompactionPolicyWire::LinkedSuccessor);
        assert!(!cfg.await_extensions);
    }

    #[test]
    fn session_lifecycle_u8_round_trip() {
        for l in [SessionLifecycle::Live, SessionLifecycle::Parking, SessionLifecycle::Parked, SessionLifecycle::Ending] {
            assert_eq!(SessionLifecycle::from_u8(l as u8), l);
        }
        assert_eq!(serde_json::to_string(&SessionLifecycle::Parked).unwrap(), r#""parked""#);
    }

    #[test]
    fn host_event_is_never_serialised() {
        let cmd = SessionCommand::HostEvent(HostEvent::LoaderProgress(
            crate::extensions::loader::ExtensionLoaderEvent::Started,
        ));
        assert!(serde_json::to_string(&cmd).is_err());
    }
}
