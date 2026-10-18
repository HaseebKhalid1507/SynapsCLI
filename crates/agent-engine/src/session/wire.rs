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
/// v3 (E-P0): driver events (`DriverArmed`, `DriverRevoked`,
/// `DriverTurnOutcome`); old clients skip unknown via `#[serde(other)]`.
/// Protocol stays exact-match; only *binary* versions compare directionally
/// (reload, §2.8).
/// `Close`/`Closed` were added without a bump: an older daemon answers the
/// unknown `ClientFrame` with `Error{"malformed frame: unknown variant
/// `close`…"}` (the client maps that to "daemon too old"), whereas a bump
/// would refuse every older TUI that reconnects after a `daemon reload`.
/// Additive `ClientFrame` variants that only a new client sends follow this
/// rule; `Closed` only ever answers `Close`, so old clients never see it.
/// `Pin`/`Pinned` (`synaps daemon pin`) follow the same rule, and so does
/// `SessionMeta.pinned` (`#[serde(default)]`: an older daemon omits it).
pub const PROTOCOL_VERSION: u32 = 3;
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
/// `synaps daemon close` sends the engine command `close` under a fresh id
/// at or above this one per request (`daemon/close.rs`) and awaits that id's
/// `QueryResult`, so concurrent closes never take each other's reply.
/// Attached clients never see these.
pub const CLOSE_QUERY_ID_BASE: u64 = RESERVED_QUERY_ID_BASE + (1 << 32);

/// The binary version the daemon/client report in the handshake.
pub fn binary_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

static BUILD_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record this process's build id. The `synaps` binary calls it first thing
/// in `main` with the id its build script baked in (the short commit,
/// `-dirty` with uncommitted changes, or `SYNAPS_BUILD_ID` from the build
/// environment). It lives in the root crate, not here, so a new commit does
/// not recompile the engine. First call wins; later calls are ignored.
pub fn set_build_id(id: &str) {
    let _ = BUILD_ID.set(id.trim().to_string());
}

/// Which build of [`binary_version`] this is, as set by [`set_build_id`];
/// empty when unknown (never set: library embedders, tests, packaged builds
/// without git). Two builds of one release share the version, not this.
pub fn build_id() -> &'static str {
    BUILD_ID.get().map(String::as_str).unwrap_or("")
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
    /// Control fast path: end one session as `/close [force]` would, with
    /// host authority (no attach, never unparks). A session with clients
    /// attached is refused unless `attached` (`--attached`); `force` only
    /// overrides work in flight. Reply: `Closed`, or
    /// `Refused{Busy|UnknownSession}`. Not accepted on an attached
    /// connection (that client has `/close`).
    Close {
        session_id: SessionId,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        attached: bool,
    },
    /// Control fast path: pin (`on`) or unpin one session, as `/pin on|off`
    /// would, with host authority. `on` is required so a frame always says
    /// which way it goes. Pinning a parked session unparks it; unpinning one
    /// leaves it parked. Reply: `Pinned`, or `Refused{Busy|UnknownSession}`.
    /// Not accepted on an attached connection (that client has `/pin`).
    Pin {
        session_id: SessionId,
        on: bool,
    },
    /// Control fast path (no session): the in-memory account state of the
    /// broker every session of this daemon shares (`synaps auth state`),
    /// cleared first when `clear` is set. Reply: `AuthState`. A daemon that
    /// predates this frame answers `Error` "malformed frame: unknown variant
    /// `auth_state`, ..." and keeps the connection usable.
    AuthState(agent_core::auth::AuthStateQuery),
    /// Control query (no session, also on an attached connection): the
    /// daemon's child-process ledger. Reply: `ChildList`. Feature "children".
    Children {
        /// Echoed in the reply.
        #[serde(default)]
        id: Option<u64>,
        /// One session's records; `None` = every session plus the host list.
        #[serde(default)]
        session_id: Option<SessionId>,
        /// Only records whose `child.sid` equals this (filters, attributes nothing).
        #[serde(default)]
        sid: Option<u32>,
        /// Also scan `/proc` for the live processes in each record's sid.
        #[serde(default)]
        members: bool,
    },
    /// Control query (no session, also on an attached connection): the
    /// thinking levels this daemon would offer for `model`. Reply:
    /// `ThinkingOptionsResult`. Feature "thinking_options".
    ThinkingOptions {
        /// Echoed in the reply.
        #[serde(default)]
        id: Option<u64>,
        /// `None` = the model a new session on this daemon gets (the config
        /// as of the last boot or reload; the reply echoes the resolved id).
        #[serde(default)]
        model: Option<String>,
        /// Opt-in: first run the bounded Muse catalog warm-up for a `muse/`
        /// id with no live row (the reply then comes from a task).
        #[serde(default)]
        warm: bool,
    },
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
            Self::Close { session_id, force, attached } => f
                .debug_struct("Close")
                .field("session_id", session_id)
                .field("force", force)
                .field("attached", attached)
                .finish(),
            Self::Pin { session_id, on } => f
                .debug_struct("Pin")
                .field("session_id", session_id)
                .field("on", on)
                .finish(),
            Self::AuthState(query) => f.debug_tuple("AuthState").field(query).finish(),
            Self::Children { id, session_id, sid, members } => f
                .debug_struct("Children")
                .field("id", id)
                .field("session_id", session_id)
                .field("sid", sid)
                .field("members", members)
                .finish(),
            Self::ThinkingOptions { id, model, warm } => f
                .debug_struct("ThinkingOptions")
                .field("id", id)
                .field("model", model)
                .field("warm", warm)
                .finish(),
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
    /// Client environment snapshot (secrets and `SYNAPS_CLIENT_*`/`SYNAPS_TUI_*`/
    /// `SYNAPS_DAEMON_*`/`SYNAPS_MEM_TRACE*` stripped). `None` = in-process
    /// (inherit process env). Legacy frames without this field decode as `None`.
    #[serde(default)]
    pub env: Option<super::types::SessionEnv>,
    /// Names of env vars stripped as secrets at the client (T5). Names only,
    /// never values. `serde(default)` → legacy frames decode as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_stripped: Vec<String>,
    /// Set when re-connecting after `Reloading`/EOF (C3/A4).
    #[serde(default)]
    pub reconnect_of: Option<ClientReconnect>,
    /// Optional event kinds this connection wants, one name per kind
    /// ([`OPTIONAL_EVENTS`]). Unknown names are ignored; an optional event
    /// not listed here is never sent on this connection. Legacy frames
    /// decode as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_events: Vec<String>,
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
        let (env, env_stripped) = super::types::capture_client_env();
        Self {
            protocol_version: PROTOCOL_VERSION,
            client: ClientMeta {
                terminal: std::env::var("TERM").ok(),
                ..ClientMeta::new(kind)
            },
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            client_version: binary_version(),
            env: Some(env),
            env_stripped,
            reconnect_of: None,
            accepted_events: Vec::new(),
        }
    }

    /// Opt in to optional event kinds by name (see [`OPTIONAL_EVENTS`]).
    pub fn with_accepted_events<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.accepted_events = names.into_iter().map(Into::into).collect();
        self
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
    /// Reply to `Close`: the session ended; `text` is the actor's `/close`
    /// reply (how to continue it).
    Closed {
        session_id: SessionId,
        text: String,
    },
    /// Reply to `Pin`: the session's pin is now `on`.
    Pinned {
        session_id: SessionId,
        on: bool,
    },
    /// `Some(Reloading{..})` before exec; `None` otherwise (today).
    Bye {
        #[serde(default)]
        reason: Option<ByeReason>,
    },
    /// Reply to `ClientFrame::AuthState`. Labels, kinds and times only:
    /// never token material, emails, account ids or identity hashes.
    AuthState {
        report: agent_core::auth::AuthStateReport,
    },
    /// Answer to `Children`. Only sent on request, so an old client (whose
    /// `DaemonFrame` has no catch-all) never receives one.
    ChildList(ChildListReply),
    /// Answer to `ThinkingOptions`. Only sent on request.
    ThinkingOptionsResult {
        #[serde(default)]
        id: Option<u64>,
        /// The id asked about (the resolved one when the query had none).
        model: String,
        /// `None` for a malformed id.
        options: Option<ThinkingOptionsWire>,
    },
}

/// The levels a model offers, with the evidence behind them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingOptionsWire {
    /// Canonical level names, in the order the settings UI shows them.
    pub levels: Vec<String>,
    /// True when an exact row decided (`basis` live or static).
    pub exact: bool,
    pub basis: crate::runtime::openai::catalog::validation::OptionsBasis,
    /// The level a model switch applies when none was chosen, if any.
    #[serde(default)]
    pub default_level: Option<String>,
}

/// The `child_list` reply: the ledger view plus what a client needs to
/// compare identities (`boot_id`) and display start times (`clk_tck`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildListReply {
    #[serde(default)]
    pub id: Option<u64>,
    /// False off Linux (or without a boot id): no records are kept.
    pub supported: bool,
    #[serde(default)]
    pub boot_id: Option<String>,
    pub clk_tck: u64,
    #[serde(default)]
    pub sessions: Vec<super::children::SessionChildren>,
    #[serde(default)]
    pub host: Vec<super::children::HostChild>,
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
    /// The daemon's [`build_id`]: tells two builds of one version apart.
    /// Empty from older daemons (unknown). `daemon_version` stays the plain
    /// semver the reload gate parses.
    #[serde(default)]
    pub daemon_build: String,
    /// Optional control frames this daemon answers ("children", ...), on
    /// every Welcome including the ones after a reload. Old daemons omit it.
    #[serde(default)]
    pub features: Vec<String>,
    /// The optional events this daemon will send on this connection: the
    /// known subset of `Hello.accepted_events`, in [`OPTIONAL_EVENTS`] order.
    /// Omitted when empty (and by old daemons).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_events: Vec<String>,
}

/// Event kinds sent only to connections that name them in
/// `Hello.accepted_events`: one name per kind, no groups. Every other event
/// goes to every attached client as before, so an old client (which shows
/// an unknown event as "unknown event from a newer daemon") never sees one.
pub const OPTIONAL_EVENTS: &[&str] = &[EVENT_TOOL_CHILD, EVENT_TOOL_CHILD_ENDED];

/// A tool spawned a child process.
pub const EVENT_TOOL_CHILD: &str = "tool_child";
/// A tool's spawned child was reaped.
pub const EVENT_TOOL_CHILD_ENDED: &str = "tool_child_ended";

/// The opt-in name of an optional event kind; `None` = always sent.
pub fn optional_event_name(event: &SessionEventWire) -> Option<&'static str> {
    use super::child_events::ChildProcessEvent;
    match event {
        SessionEventWire::ChildProcess(ChildProcessEvent::Spawned(_)) => Some(EVENT_TOOL_CHILD),
        SessionEventWire::ChildProcess(ChildProcessEvent::Ended { .. }) => Some(EVENT_TOOL_CHILD_ENDED),
        _ => None,
    }
}

/// The optional events one connection gets, negotiated from its Hello.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedEvents(Vec<&'static str>);

impl AcceptedEvents {
    pub fn negotiate(requested: &[String]) -> Self {
        Self::from_known(requested, OPTIONAL_EVENTS)
    }

    /// What was asked for and is known, in `known` order, once each.
    fn from_known(requested: &[String], known: &[&'static str]) -> Self {
        Self(known.iter().copied().filter(|k| requested.iter().any(|r| r == k)).collect())
    }

    /// The Welcome echo.
    pub fn names(&self) -> Vec<String> {
        self.0.iter().map(|s| (*s).to_string()).collect()
    }

    fn allows_name(&self, name: Option<&str>) -> bool {
        name.map_or(true, |n| self.0.contains(&n))
    }

    /// Whether `event` may be sent on this connection.
    pub fn allows(&self, event: &SessionEventWire) -> bool {
        self.allows_name(optional_event_name(event))
    }
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
    #[serde(default)]
    pub keepwarm_cost: f64,
    #[serde(default)]
    pub keepwarm_tokens: ConversationTokens,
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
            messages_hash: s.messages_hash_memo.hash(&s.api_messages),
            tokens: s.tokens.clone(),
            cost: s.cost,
            keepwarm_cost: s.keepwarm_cost,
            keepwarm_tokens: s.keepwarm_tokens.clone(),
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
            keepwarm_cost: self.keepwarm_cost,
            keepwarm_tokens: self.keepwarm_tokens,
            abort_context: self.abort_context,
            queued_message: self.queued_message,
            pending_events_len: self.pending_events_len,
            consecutive_auto_turns: self.consecutive_auto_turns,
            messages_hash_memo: Default::default(),
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
    /// (E-P0) Driver armed.
    DriverArmed {
        plugin_id: String,
        run_id: String,
        models: Vec<crate::extensions::session_driver::Selection>,
        selection: crate::extensions::session_driver::Selection,
        #[serde(default)]
        deadline_ms: Option<u64>,
        #[serde(default)]
        notice: String,
    },
    /// (E-P0) Driver revoked.
    DriverRevoked {
        reason: String,
        #[serde(default)]
        undelivered_steering: Vec<String>,
    },
    /// (E-P0) Driver turn outcome.
    DriverTurnOutcome {
        outcome: crate::extensions::session_driver::Outcome,
        selection: crate::extensions::session_driver::Selection,
        #[serde(default)]
        feedback: Option<String>,
    },
    /// (E-P7) Spend ceiling breached; turn cancelled and driver revoked.
    CostCapReached {
        scope: String,
        cost: f64,
        cap: f64,
    },
    /// A tool call spawned a child process (opt-in: `tool_child`).
    ToolChild {
        tool_id: String,
        tool_name: String,
        #[serde(default)]
        handle_id: Option<String>,
        #[serde(default)]
        shell_session_id: Option<String>,
        child: super::children::ChildIdentity,
    },
    /// A tool call's spawned child was reaped (opt-in: `tool_child_ended`).
    ToolChildEnded {
        tool_id: String,
        #[serde(default)]
        handle_id: Option<String>,
        child: super::children::ChildIdentity,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
    },
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
    ResponseStart,
    ResponseReset,
    /// Opt-in (`ClientCapabilities::tool_queue`): this enum has no
    /// `#[serde(other)]`, so an older client fails the WHOLE frame on an
    /// unknown variant — the daemon never sends these two to a client that
    /// did not declare it can decode them (`daemon/conn.rs`).
    ToolQueued { tool_id: String },
    ToolStarted { tool_id: String },
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
    /// `retry`: a `SessionEvent::Retry`. Omitted when false, so a plain
    /// notice's frame is unchanged and peers that predate the flag read a
    /// retry as a plain notice.
    Notice {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retry: bool,
    },
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
                LlmEvent::ResponseStart => WireLlmEvent::ResponseStart,
                LlmEvent::ResponseReset => WireLlmEvent::ResponseReset,
                LlmEvent::ToolQueued { tool_id } => WireLlmEvent::ToolQueued { tool_id },
                LlmEvent::ToolStarted { tool_id } => WireLlmEvent::ToolStarted { tool_id },
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
                SessionEvent::Notice(text) => WireSessionStreamEvent::Notice { text, retry: false },
                SessionEvent::Retry(text) => WireSessionStreamEvent::Notice { text, retry: true },
                // merge(112): ContextHeadCheckpoint is actor-internal (Wall 1); never crosses the wire.
                // Drop it silently — the actor handles persistence before forwarding.
                SessionEvent::ContextHeadCheckpoint { .. } => WireSessionStreamEvent::Done,
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
                WireLlmEvent::ResponseStart => LlmEvent::ResponseStart,
                WireLlmEvent::ResponseReset => LlmEvent::ResponseReset,
                WireLlmEvent::ToolQueued { tool_id } => LlmEvent::ToolQueued { tool_id },
                WireLlmEvent::ToolStarted { tool_id } => LlmEvent::ToolStarted { tool_id },
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
                WireSessionStreamEvent::Notice { text, retry: false } => SessionEvent::Notice(text),
                WireSessionStreamEvent::Notice { text, retry: true } => SessionEvent::Retry(text),
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
            S::DriverArmed { plugin_id, run_id, models, selection, deadline_ms, notice } => {
                Self::DriverArmed { plugin_id, run_id, models, selection, deadline_ms, notice }
            }
            S::DriverRevoked { reason, undelivered_steering } => {
                Self::DriverRevoked { reason, undelivered_steering }
            }
            S::DriverTurnOutcome { outcome, selection, feedback } => {
                Self::DriverTurnOutcome { outcome, selection, feedback }
            }
            S::CostCapReached { scope, cost, cap } => {
                Self::CostCapReached { scope, cost, cap }
            }
            S::ChildProcess(super::child_events::ChildProcessEvent::Spawned(spawn)) => Self::ToolChild {
                tool_id: spawn.tool_id,
                tool_name: spawn.tool_name,
                handle_id: spawn.handle_id,
                shell_session_id: spawn.shell_session_id,
                child: spawn.child,
            },
            S::ChildProcess(super::child_events::ChildProcessEvent::Ended {
                tool_id,
                handle_id,
                child,
                exit_code,
                signal,
            }) => Self::ToolChildEnded { tool_id, handle_id, child, exit_code, signal },
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
            W::DriverArmed { plugin_id, run_id, models, selection, deadline_ms, notice } => {
                Self::DriverArmed { plugin_id, run_id, models, selection, deadline_ms, notice }
            }
            W::DriverRevoked { reason, undelivered_steering } => {
                Self::DriverRevoked { reason, undelivered_steering }
            }
            W::DriverTurnOutcome { outcome, selection, feedback } => {
                Self::DriverTurnOutcome { outcome, selection, feedback }
            }
            W::CostCapReached { scope, cost, cap } => {
                Self::CostCapReached { scope, cost, cap }
            }
            W::ToolChild { tool_id, tool_name, handle_id, shell_session_id, child } => {
                Self::ChildProcess(super::child_events::ChildProcessEvent::Spawned(super::children::ChildSpawn {
                    tool_id,
                    tool_name,
                    handle_id,
                    shell_session_id,
                    child,
                }))
            }
            W::ToolChildEnded { tool_id, handle_id, child, exit_code, signal } => {
                Self::ChildProcess(super::child_events::ChildProcessEvent::Ended {
                    tool_id,
                    handle_id,
                    child,
                    exit_code,
                    signal,
                })
            }
            W::Unknown => Self::SystemNotice("unknown event from a newer daemon (ignored)".into()),
        }
    }
}

// ── per-client decode gate ────────────────────────────────────────────────────

/// Whether a client that declared `caps` can decode `event` once it is a
/// wire frame. The enums nested under `Stream` have no `#[serde(other)]`,
/// so an older client does not skip an unknown nested variant: the whole
/// frame fails to decode, `SocketTransport::next_event` treats that as a
/// dead socket and the TUI drops into reconnect — whose `Attached` replay
/// may carry the very same event and fail again. The daemon therefore
/// drops, per connection, every event the client did not opt into
/// (`daemon/conn.rs`, live pump and attach replay alike).
pub fn client_decodes(caps: ClientCapabilities, event: &SessionEventWire) -> bool {
    match event {
        SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { .. } | LlmEvent::ToolStarted { .. })) => {
            caps.tool_queue
        }
        _ => true,
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

    /// An `AuthState` reply with every nested type present.
    fn auth_state_report() -> agent_core::auth::AuthStateReport {
        use agent_core::auth::auth_state::{AccountState, CooldownReason, CooldownState, IdentityKind, ObservedOutcome, SelectionState};
        use agent_core::auth::quota_policy::{
            AccountFailure, AccountFailureKind, ObservationFailure, SelectionFailure, SelectionFailureKind,
        };
        let usage_429 = ObservationFailure { kind: AccountFailureKind::UsageRateLimited, http_status: Some(429), retry_after_s: Some(120) };
        agent_core::auth::AuthStateReport {
            schema_version: agent_core::auth::AUTH_STATE_SCHEMA_VERSION,
            live: true,
            broker_active: true,
            generated_at_ms: 1_800_000_000_000,
            store: "/scratch/auth.json".into(),
            accounts: vec![AccountState {
                provider: "anthropic".into(),
                label: "a".into(),
                identity: IdentityKind::Stable,
                access_expires_ms: Some(1_800_000_600_000),
                cooldown: Some(CooldownState { until_ms: 1_800_003_600_000, reason: CooldownReason::AnthropicQuotaExhausted }),
                last_usage: Some(ObservedOutcome { at_ms: 1_799_999_990_000, failure: Some(usage_429.clone()) }),
                last_usage_ok_ms: Some(1_799_999_000_000),
                last_refresh: Some(ObservedOutcome { at_ms: 1_799_999_500_000, failure: None }),
            }],
            selections: vec![SelectionState {
                provider: "anthropic".into(),
                model: Some("claude-x".into()),
                at_ms: 1_799_999_990_000,
                chosen: None,
                failure: Some(SelectionFailure {
                    kind: SelectionFailureKind::UsageReadFailed,
                    accounts: vec![AccountFailure {
                        account: "a".into(),
                        kind: AccountFailureKind::UsageRateLimited,
                        http_status: Some(429),
                        retry_after_s: Some(120),
                        until_ms: None,
                        cooling_down: false,
                    }],
                    earliest_reset_ms: None,
                    usage_read_retry_ms: None,
                }),
                vend_failure: None,
            }],
            cleared: vec!["anthropic:b".into()],
        }
    }

    fn meta() -> SessionMeta {
        crate::session::handle::echo::meta_for(&SessionId::from("wire-1"))
    }

    fn thinking_options_sample() -> DaemonFrame {
        DaemonFrame::ThinkingOptionsResult {
            id: Some(8),
            model: "muse/muse-wonder-1.0".into(),
            options: Some(ThinkingOptionsWire {
                levels: ["adaptive", "low", "medium", "high", "xhigh"].map(String::from).to_vec(),
                exact: true,
                basis: crate::runtime::openai::catalog::validation::OptionsBasis::Live,
                default_level: Some("high".into()),
            }),
        }
    }

    fn child_list_sample() -> ChildListReply {
        use crate::session::children::*;
        let child = ChildIdentity { pid: 3646188, start_ticks: 26981650, boot_id: "b".into(), sid: 3646188, pgid: 3646188 };
        ChildListReply {
            id: Some(7),
            supported: true,
            boot_id: Some("b".into()),
            clk_tck: 100,
            sessions: vec![SessionChildren {
                session_id: "s".into(),
                truncated: false,
                children: vec![ChildEntry {
                    tool_id: "toolu_1".into(),
                    tool_name: "bash".into(),
                    handle_id: Some("sa_12".into()),
                    shell_session_id: None,
                    child: child.clone(),
                    state: ChildState::Running,
                    exit_code: None,
                    signal: None,
                    members: Some(vec![ChildMember { pid: 3646201, start_ticks: 26981702 }]),
                }],
            }],
            host: vec![HostChild { owner_kind: HostOwnerKind::McpServer, owner_id: "figma".into(), child }],
        }
    }

    fn prompt() -> PromptRequest {
        PromptRequest {
            id: 3,
            kind: PromptKind::Secret,
            title: "sudo".into(),
            prompt: "password".into(),
            raised_at: chrono::Utc::now(),
            confirm_scope: None,
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
            keepwarm_cost: 0.125,
            keepwarm_tokens: ConversationTokens { input: 0, output: 0, cache_read: 9, cache_creation: 0 },
            abort_context: Some("ctx".into()),
            queued_message: None,
            pending_events_len: 2,
            consecutive_auto_turns: 1,
            messages_hash_memo: Default::default(),
        }
    }

    /// One snapshot fanned out to N socket clients is hashed ONCE (the memo
    /// is shared by its clones), and a snapshot whose messages were replaced
    /// afterwards is re-hashed, never served a stale hash.
    #[test]
    fn conversation_digest_hash_is_computed_once_and_never_stale() {
        let a = conv();
        let b = a.clone();
        assert_eq!(ConversationDigest::of(&a), ConversationDigest::of(&b));
        assert!(a.messages_hash_memo.0.lock().unwrap().is_some(), "memo shared");
        assert_eq!(
            ConversationDigest::of(&b).messages_hash,
            messages_hash(&a.api_messages)
        );
        let mut c = a.clone();
        c.api_messages = Vec::new();
        assert_eq!(ConversationDigest::of(&c).messages_hash, messages_hash(&[]));
        let mut d = a.clone();
        d.api_messages.push(Arc::new(serde_json::json!({"role":"assistant","content":"yo"})));
        assert_eq!(
            ConversationDigest::of(&d).messages_hash,
            messages_hash(&d.api_messages)
        );
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
            StreamEvent::Llm(LlmEvent::ToolQueued { tool_id: "id2".into() }),
            StreamEvent::Llm(LlmEvent::ToolStarted { tool_id: "id2".into() }),
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
            StreamEvent::Session(SessionEvent::Retry("r".into())),
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

    /// 04-tui I4: a retry rides on the existing `notice` frame with a flag,
    /// so a peer that predates it decodes it as a plain notice (it never
    /// becomes a malformed frame), and a plain notice's bytes are unchanged.
    #[test]
    fn a_retry_is_a_flagged_notice_on_the_wire() {
        let retry = StreamEvent::Session(SessionEvent::Retry("⚠ Rate limited — retrying (1/5)".into()));
        let json = serde_json::to_value(WireStreamEvent::from(retry)).unwrap();
        assert_eq!(json["session"], "notice", "{json}");
        assert_eq!(json["retry"], true, "{json}");
        let back: WireStreamEvent = serde_json::from_value(json).unwrap();
        assert!(
            matches!(StreamEvent::from(back), StreamEvent::Session(SessionEvent::Retry(t)) if t.starts_with("⚠ Rate limited")),
            "a retry stays a retry"
        );
        let plain = StreamEvent::Session(SessionEvent::Notice("n".into()));
        let json = serde_json::to_value(WireStreamEvent::from(plain)).unwrap();
        assert!(json.get("retry").is_none(), "a plain notice is unchanged: {json}");
        // An older daemon's notice has no flag: a plain notice.
        let old: WireStreamEvent =
            serde_json::from_value(serde_json::json!({"kind": "session", "session": "notice", "text": "x"})).unwrap();
        assert!(matches!(StreamEvent::from(old), StreamEvent::Session(SessionEvent::Notice(t)) if t == "x"));
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
            S::TurnStarted { turn_baseline: 6, trigger: TurnTrigger::DriverAuto, user_text: None },
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
                step: "$ cargo test".into(),
                tools: 3,
                handle_id: "sa_1".into(),
                model: "provider/model-a".into(),
            }]),
            S::Resumed { id: 2, old_id: "o".into(), new_id: "n".into(), via: Some("name".into()), clamp_notice: None },
            S::InputOwnerChanged { from: Some(ClientId(1)), to: Some(ClientId(2)), reason: OwnerChangeReason::Takeover },
            S::Refused { client: ClientId(2), command: "submit".into(), reason: "input owned by client #1".into() },
            S::AttachRefused { message: "parked".into() },
            S::Lifecycle(SessionLifecycle::Parked),
            S::Reloading { generation: 2, retry_after_ms: 500 },
            S::DriverArmed {
                plugin_id: "autonomous".into(),
                run_id: "run-1".into(),
                models: vec![
                    crate::extensions::session_driver::Selection { model: "m1".into(), effort: "high".into() },
                    crate::extensions::session_driver::Selection { model: "m2".into(), effort: "low".into() },
                ],
                selection: crate::extensions::session_driver::Selection { model: "m1".into(), effort: "high".into() },
                deadline_ms: Some(60_000),
                notice: "armed".into(),
            },
            S::DriverRevoked { reason: "canceled".into(), undelivered_steering: vec!["steer1".into()] },
            S::DriverTurnOutcome {
                outcome: crate::extensions::session_driver::Outcome::Success,
                selection: crate::extensions::session_driver::Selection { model: "m1".into(), effort: "high".into() },
                feedback: Some("changed".into()),
            },
        ]);
        v
    }

    fn queued_frame() -> String {
        let ev = env(SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { tool_id: "toolu_2".into() })));
        encode_line(&DaemonFrame::Event(ev.into())).unwrap()
    }

    /// What an OLDER client does with a new nested `llm` variant: its
    /// `WireLlmEvent` lacks the tag, so serde fails the WHOLE frame (no
    /// `#[serde(other)]` down there) — `SocketTransport::next_event` then
    /// ends the stream as if the socket closed. Simulated the way the
    /// driver-event test does: rename the tag to one this build lacks.
    /// Only the top level (`ev`) has an `Unknown` fallback, and that one
    /// shows the user a notice.
    #[test]
    fn an_unknown_nested_llm_variant_fails_the_whole_frame() {
        let line = queued_frame();
        assert!(line.contains(r#""llm":"tool_queued""#), "{line}");
        let as_old = line.replace(r#""llm":"tool_queued""#, r#""llm":"tool_from_the_future""#);
        let err = decode_line::<DaemonFrame>(as_old.trim_end()).unwrap_err();
        assert!(err.contains("unknown variant `tool_from_the_future`"), "{err}");
        // The same inside an attach replay sinks the whole `Attached`.
        let attached = AttachedWire::new(
            ClientId(1),
            AttachSnapshot {
                meta: meta(),
                view: view(),
                conversation: conv(),
                streaming: true,
                replay: vec![env(SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { tool_id: "toolu_2".into() })))],
                pending_prompts: vec![],
                clients: vec![],
                input_owner: None,
                display_tail: None,
            },
        );
        let line = encode_line(&DaemonFrame::Attached(attached)).unwrap();
        let as_old = line.replace(r#""llm":"tool_queued""#, r#""llm":"tool_from_the_future""#);
        assert!(decode_line::<DaemonFrame>(as_old.trim_end()).is_err());
        // Whereas an unknown TOP-level event decodes, as a user-visible notice.
        let top_frame = queued_frame().replace(r#""ev":"stream""#, r#""ev":"zz_new""#);
        match decode_line::<DaemonFrame>(top_frame.trim_end()).unwrap() {
            DaemonFrame::Event(w) => match Envelope::from(w).event {
                SessionEventWire::SystemNotice(t) => assert!(t.contains("newer daemon"), "{t}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// The gate: a client that did not declare `tool_queue` (every client
    /// that predates it) is never sent the queue signals; everything else
    /// it decodes exactly as before. A current client gets them.
    #[test]
    fn queue_signals_reach_only_clients_that_opted_in() {
        let old = ClientCapabilities::default();
        let new = ClientCapabilities::current();
        for ev in fixtures() {
            let queue = matches!(
                ev,
                SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { .. } | LlmEvent::ToolStarted { .. }))
            );
            assert_eq!(client_decodes(old, &ev), !queue, "old client: {ev:?}");
            assert!(client_decodes(new, &ev), "new client: {ev:?}");
        }
    }

    /// Hello compat both ways: an older client's meta has no capabilities
    /// (decodes as none), a current one declares `tool_queue`, and an older
    /// daemon's `ClientMeta` (no such field) still decodes the new frame.
    #[test]
    fn client_capabilities_default_off_and_old_daemons_ignore_them() {
        let legacy = r#"{"kind":"tui","terminal":null,"instance":"i"}"#;
        let m: ClientMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.capabilities, ClientCapabilities::default());
        assert!(!m.capabilities.tool_queue);

        let h = Hello::new(ClientKind::Tui);
        assert!(h.client.capabilities.tool_queue);
        let line = encode_line(&ClientFrame::Hello(h)).unwrap();
        assert!(line.contains(r#""capabilities":{"tool_queue":true}"#), "{line}");
        match decode_line::<ClientFrame>(line.trim_end()).unwrap() {
            ClientFrame::Hello(h) => assert!(h.client.capabilities.tool_queue),
            other => panic!("{other:?}"),
        }

        // The ClientMeta an older daemon decodes (as of da1eb372).
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldClientMeta {
            kind: ClientKind,
            terminal: Option<String>,
            instance: String,
            #[serde(default)]
            history: HistoryMode,
            #[serde(default)]
            tail_items: usize,
        }
        let json = serde_json::to_string(&ClientMeta::new(ClientKind::Tui)).unwrap();
        let old: OldClientMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(old.kind, ClientKind::Tui);
    }

    /// The watch query's opt-in: an older client's query omits it (off), an
    /// off flag is not serialized (an older daemon sees the old bytes).
    #[test]
    fn the_watch_queries_tool_queue_flag_defaults_off() {
        let old: SessionQuery =
            serde_json::from_str(r#"{"query":"subagent_watch","handle_id":"sa_1","since":4}"#).unwrap();
        assert!(matches!(old, SessionQuery::SubagentWatch { tool_queue: false, since: 4, .. }));
        let off = SessionQuery::SubagentWatch { handle_id: "sa_1".into(), since: 4, tool_queue: false };
        assert!(!serde_json::to_string(&off).unwrap().contains("tool_queue"));
        let on = SessionQuery::SubagentWatch { handle_id: "sa_1".into(), since: 4, tool_queue: true };
        let back: SessionQuery = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
        assert!(matches!(back, SessionQuery::SubagentWatch { tool_queue: true, .. }));
    }

    #[test]
    fn wire_roundtrip_every_variant() {
        let all = fixtures();
        // 36 non-Stream variants (TurnStarted thrice = 3; +3 driver events) + 21 StreamEvent leaves.
        assert_eq!(all.len(), 36 + 21);
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

    /// An older daemon cannot decode a newer client's command; serde names
    /// the unknown tag in backticks. The TUI keys on exactly that
    /// (`tui::watch::absorb_notice`, "malformed frame: unknown variant
    /// `subagent_watch`") to tell the user to reload the daemon.
    #[test]
    fn an_unknown_command_or_query_names_its_tag_in_the_decode_error() {
        let frames = [
            (
                ClientFrame::Cmd {
                    session_id: "s".into(),
                    cmd: SessionCommand::Query {
                        id: 1,
                        query: SessionQuery::SubagentWatch { handle_id: "sa_1".into(), since: 0, tool_queue: false },
                    },
                },
                "subagent_watch",
            ),
            (
                ClientFrame::Cmd {
                    session_id: "s".into(),
                    cmd: SessionCommand::SubagentSteer { handle_id: "sa_1".into(), text: "x".into() },
                },
                "subagent_steer",
            ),
        ];
        for (frame, tag) in frames {
            let json = serde_json::to_string(&frame).unwrap();
            assert!(decode_line::<ClientFrame>(&json).is_ok());
            // What an older daemon sees: a tag it does not know.
            let old = json.replace(&format!("\"{tag}\""), "\"zz_from_the_future\"");
            assert_ne!(old, json, "{tag} is serialized as a quoted tag");
            let err = decode_line::<ClientFrame>(&old).unwrap_err();
            assert!(err.contains("unknown variant `zz_from_the_future`"), "{err}");
        }
    }

    #[test]
    fn driver_armed_round_trips_and_old_client_skips() {
        // A v3 daemon emits DriverArmed; a v2 client decodes it as Unknown.
        let armed = WireSessionEvent::DriverArmed {
            plugin_id: "autonomous".into(),
            run_id: "run-42".into(),
            models: vec![
                crate::extensions::session_driver::Selection { model: "m1".into(), effort: "high".into() },
            ],
            selection: crate::extensions::session_driver::Selection { model: "m1".into(), effort: "high".into() },
            deadline_ms: Some(120_000),
            notice: "armed and ready".into(),
        };
        let json = serde_json::to_string(&armed).unwrap();
        // v3 client: exact round-trip.
        let back: WireSessionEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(&back, WireSessionEvent::DriverArmed { plugin_id, .. } if plugin_id == "autonomous"));
        // Simulated v2 client: rename tag so it hits Unknown.
        let old_json = json.replace(r#""ev":"driver_armed""#, r#""ev":"v2_unknown_event""#);
        let old: WireSessionEvent = serde_json::from_str(&old_json).unwrap();
        assert!(matches!(old, WireSessionEvent::Unknown));
    }

    #[test]
    fn driver_revoked_and_outcome_round_trip() {
        let revoked = WireSessionEvent::DriverRevoked {
            reason: "canceled".into(),
            undelivered_steering: vec!["steer1".into(), "steer2".into()],
        };
        let json = serde_json::to_string(&revoked).unwrap();
        let back: WireSessionEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(&back, WireSessionEvent::DriverRevoked { reason, undelivered_steering }
            if reason == "canceled" && undelivered_steering.len() == 2));

        let outcome = WireSessionEvent::DriverTurnOutcome {
            outcome: crate::extensions::session_driver::Outcome::Success,
            selection: crate::extensions::session_driver::Selection { model: "m".into(), effort: "high".into() },
            feedback: Some("changed".into()),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: WireSessionEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(&back, WireSessionEvent::DriverTurnOutcome { feedback: Some(f), .. } if f == "changed"));
    }

    #[test]
    fn driver_revoked_defaults_tolerate_missing_fields() {
        // Old daemon or minimal frame: undelivered_steering missing → empty vec.
        let minimal = r#"{"ev":"driver_revoked","reason":"timeout"}"#;
        let v: WireSessionEvent = serde_json::from_str(minimal).unwrap();
        assert!(matches!(&v, WireSessionEvent::DriverRevoked { reason, undelivered_steering }
            if reason == "timeout" && undelivered_steering.is_empty()));
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
            ClientFrame::Cmd { session_id: "s".into(), cmd: SessionCommand::DriverStart {
                plugin: "autonomous".into(), command: "auto".into(), arg: "start -- do stuff".into(),
            }},
            ClientFrame::Reload { now: true, drain_secs: Some(3), exe: Some(PathBuf::from("/bin/synaps")) },
            ClientFrame::Purge,
            ClientFrame::Close { session_id: "s".into(), force: true, attached: true },
            ClientFrame::Close { session_id: "s".into(), force: false, attached: false },
            ClientFrame::Pin { session_id: "s".into(), on: true },
            ClientFrame::Pin { session_id: "s".into(), on: false },
            ClientFrame::AuthState(agent_core::auth::AuthStateQuery::default()),
            ClientFrame::AuthState(agent_core::auth::AuthStateQuery {
                provider: Some("anthropic".into()),
                account: Some("b".into()),
                clear: true,
            }),
            ClientFrame::Children { id: Some(7), session_id: Some("s".into()), sid: Some(42), members: true },
            ClientFrame::Children { id: None, session_id: None, sid: None, members: false },
            ClientFrame::ThinkingOptions { id: Some(8), model: Some("muse/muse-wonder-1.0".into()), warm: true },
            ClientFrame::ThinkingOptions { id: None, model: None, warm: false },
            ClientFrame::Bye,
        ];
        for f in frames {
            let line = encode_line(&f).unwrap();
            let back: ClientFrame = decode_line(line.trim_end()).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line.trim_end());
        }
        let frames = vec![
            DaemonFrame::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.9.0".into(),
                pid: 1,
                profile: None,
                sessions: vec![meta()],
                progressive_tool_disclosure: true,
                generation: 1,
                daemon_build: "abc1234".into(),
                features: vec!["children".into()],
                accepted_events: vec![],
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
            DaemonFrame::Closed { session_id: "s".into(), text: "closed".into() },
            DaemonFrame::Pinned { session_id: "s".into(), on: true },
            DaemonFrame::Pinned { session_id: "s".into(), on: false },
            DaemonFrame::Bye { reason: None },
            DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation: 2, retry_after_ms: 500 }) },
            DaemonFrame::Bye { reason: Some(ByeReason::Shutdown) },
            DaemonFrame::AuthState { report: auth_state_report() },
            DaemonFrame::ChildList(child_list_sample()),
            thinking_options_sample(),
            DaemonFrame::ThinkingOptionsResult { id: None, model: "a b".into(), options: None },
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
        assert_eq!(PROTOCOL_VERSION, 3);
        // v1 frames (no `reconnect_of`, bare `bye`) still parse — the refusal is by version, not by shape.
        let h: Hello = serde_json::from_str(
            r#"{"protocol_version":1,"client":{"kind":"tui","terminal":null,"instance":"i"},"cwd":"/","client_version":"0"}"#,
        )
        .unwrap();
        assert!(h.reconnect_of.is_none());
        // Legacy Hello without env field → None (backwards compatible).
        assert!(h.env.is_none());
        // Hello::new captures env.
        let h2 = Hello::new(ClientKind::Test);
        assert!(h2.env.is_some(), "Hello::new must capture client env");
        let b: DaemonFrame = serde_json::from_str(r#"{"type":"bye"}"#).unwrap();
        assert!(matches!(b, DaemonFrame::Bye { reason: None }));
    }

    #[test]
    fn close_defaults_force_off_and_older_daemons_reject_it_as_an_unknown_variant() {
        let f: ClientFrame = decode_line(r#"{"type":"close","session_id":"s"}"#).unwrap();
        assert!(matches!(f, ClientFrame::Close { ref session_id, force: false, attached: false } if session_id.as_str() == "s"));
        // A daemon built before `Close` decodes into an enum without it; the
        // client keys "daemon too old" on this exact serde wording.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum OldClientFrame {
            Ping,
            Purge,
        }
        let err = decode_line::<OldClientFrame>(r#"{"type":"close","session_id":"s","force":true}"#).unwrap_err();
        assert!(err.contains("unknown variant `close`"), "{err}");
    }

    #[test]
    fn pin_requires_on_and_older_daemons_reject_it_as_an_unknown_variant() {
        let f: ClientFrame = decode_line(r#"{"type":"pin","session_id":"s","on":false}"#).unwrap();
        assert!(matches!(f, ClientFrame::Pin { ref session_id, on: false } if session_id.as_str() == "s"));
        assert!(decode_line::<ClientFrame>(r#"{"type":"pin","session_id":"s"}"#).is_err());
        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum OldClientFrame {
            Ping,
            Purge,
        }
        let err = decode_line::<OldClientFrame>(r#"{"type":"pin","session_id":"s","on":true}"#).unwrap_err();
        assert!(err.contains("unknown variant `pin`"), "{err}");
    }

    #[test]
    fn a_session_meta_without_pinned_decodes_as_not_pinned() {
        let mut v = serde_json::to_value(meta()).unwrap();
        v.as_object_mut().unwrap().remove("pinned");
        let m: SessionMeta = serde_json::from_value(v).unwrap();
        assert!(!m.pinned);
        let mut pinned = meta();
        pinned.pinned = true;
        let back: SessionMeta = serde_json::from_value(serde_json::to_value(&pinned).unwrap()).unwrap();
        assert!(back.pinned);
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
        const { assert!(DAEMON_MAX_FRAME_BYTES > RPC_MAX_FRAME_BYTES) };
        let big = "x".repeat(DAEMON_MAX_FRAME_BYTES + 1);
        assert!(decode_line::<ClientFrame>(&big)
            .unwrap_err()
            .contains("64 MiB"));
        // encode side: a frame that would serialise past the cap is refused
        let f = DaemonFrame::Error { session_id: None, message: big };
        assert!(encode_line(&f).unwrap_err().contains("64 MiB"));
        // a > 1 MiB (rpc cap) frame is fine on the daemon wire
        let f = DaemonFrame::Error { session_id: None, message: "y".repeat(RPC_MAX_FRAME_BYTES * 2) };
        let line = encode_line(&f).unwrap();
        assert!(decode_line::<DaemonFrame>(line.trim_end()).is_ok());
    }

    #[test]
    fn build_id_is_set_once_by_the_binary() {
        // The only caller in this test binary; `main` is the only one in `synaps`.
        set_build_id(" abc1234-dirty \n");
        assert_eq!(build_id(), "abc1234-dirty");
        set_build_id("later");
        assert_eq!(build_id(), "abc1234-dirty", "first call wins");
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
            daemon_build: String::new(),
            features: vec![],
            accepted_events: vec![],
        })
        .unwrap()
        .to_lowercase();
        for bad in ["token", "secret", "api_key", "apikey", "password", "credential"] {
            assert!(!w.contains(bad), "{bad} in Welcome");
        }
    }

    /// The names PD vendors (4/4b v1.1). Changing one breaks the app.
    #[test]
    fn children_frames_keep_the_agreed_names() {
        let q: serde_json::Value = serde_json::to_value(&ClientFrame::Children {
            id: Some(7),
            session_id: Some("s".into()),
            sid: Some(42),
            members: true,
        })
        .unwrap();
        assert_eq!(q, serde_json::json!({"type":"children","id":7,"session_id":"s","sid":42,"members":true}));
        // Every field is optional on the way in.
        let bare: ClientFrame = decode_line(r#"{"type":"children"}"#).unwrap();
        assert!(matches!(bare, ClientFrame::Children { id: None, session_id: None, sid: None, members: false }));

        let r = serde_json::to_value(DaemonFrame::ChildList(child_list_sample())).unwrap();
        let child = serde_json::json!({"pid":3646188,"start_ticks":26981650,"boot_id":"b","sid":3646188,"pgid":3646188});
        assert_eq!(
            r,
            serde_json::json!({
                "type":"child_list","id":7,"supported":true,"boot_id":"b","clk_tck":100,
                "sessions":[{"session_id":"s","truncated":false,"children":[{
                    "tool_id":"toolu_1","tool_name":"bash","handle_id":"sa_12","shell_session_id":null,
                    "child":child,"state":"running","exit_code":null,"signal":null,
                    "members":[{"pid":3646201,"start_ticks":26981702}]}]}],
                "host":[{"owner_kind":"mcp_server","owner_id":"figma","child":child}]
            })
        );
        let w = serde_json::to_value(&Welcome {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "v".into(),
            pid: 1,
            profile: None,
            sessions: vec![],
            progressive_tool_disclosure: false,
            generation: 2,
            daemon_build: String::new(),
            features: vec![crate::daemon::control_queries::FEATURE_CHILDREN.into()],
            accepted_events: vec![],
        })
        .unwrap();
        assert_eq!(w["features"], serde_json::json!(["children"]));
    }

    /// The `thinking_options` names PD vendors (4/4b v1.1 §3.1).
    #[test]
    fn thinking_options_frames_keep_the_agreed_names() {
        let q = serde_json::to_value(&ClientFrame::ThinkingOptions {
            id: Some(8),
            model: Some("muse/muse-wonder-1.0".into()),
            warm: false,
        })
        .unwrap();
        assert_eq!(q, serde_json::json!({"type":"thinking_options","id":8,"model":"muse/muse-wonder-1.0","warm":false}));
        let bare: ClientFrame = decode_line(r#"{"type":"thinking_options","model":null}"#).unwrap();
        assert!(matches!(bare, ClientFrame::ThinkingOptions { id: None, model: None, warm: false }));
        assert_eq!(
            serde_json::to_value(thinking_options_sample()).unwrap(),
            serde_json::json!({"type":"thinking_options_result","id":8,"model":"muse/muse-wonder-1.0",
                "options":{"levels":["adaptive","low","medium","high","xhigh"],"exact":true,"basis":"live","default_level":"high"}})
        );
        for (basis, name) in [
            (crate::runtime::openai::catalog::validation::OptionsBasis::Live, "live"),
            (crate::runtime::openai::catalog::validation::OptionsBasis::Static, "static"),
            (crate::runtime::openai::catalog::validation::OptionsBasis::Fallback, "fallback"),
        ] {
            assert_eq!(serde_json::to_value(basis).unwrap(), serde_json::json!(name));
        }
        let none = serde_json::to_value(&DaemonFrame::ThinkingOptionsResult { id: Some(9), model: "a b".into(), options: None }).unwrap();
        assert_eq!(none, serde_json::json!({"type":"thinking_options_result","id":9,"model":"a b","options":null}));
        assert_eq!(crate::daemon::control_queries::FEATURE_THINKING_OPTIONS, "thinking_options");
    }

    /// Old peers: a pre-4/4b client decodes the new Welcome (it ignores
    /// `features`), a new client reads an old Welcome as no features, and a
    /// pre-4/4b daemon rejects `children` as an unknown variant (conn.rs
    /// answers that with `Error{"malformed frame: ..."}` and keeps the
    /// connection).
    #[test]
    fn children_and_features_are_compatible_with_old_decoders() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldWelcome {
            protocol_version: u32,
            daemon_version: String,
            pid: u32,
            profile: Option<String>,
            sessions: Vec<SessionMeta>,
            progressive_tool_disclosure: bool,
            #[serde(default = "one")]
            generation: u64,
        }
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldDaemonFrame {
            Welcome(OldWelcome),
            Pong { pid: u32, uptime_s: u64, sessions: usize },
        }
        #[derive(Deserialize, Debug)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldClientFrame {
            Ping,
            Sessions,
            Purge,
            Bye,
        }
        let new = encode_line(&DaemonFrame::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "v".into(),
            pid: 1,
            profile: None,
            sessions: vec![meta()],
            progressive_tool_disclosure: true,
            generation: 3,
            daemon_build: String::new(),
            features: vec!["children".into()],
            accepted_events: vec![],
        }))
        .unwrap();
        match decode_line::<OldDaemonFrame>(new.trim_end()).unwrap() {
            OldDaemonFrame::Welcome(w) => assert_eq!((w.generation, w.sessions.len()), (3, 1)),
            OldDaemonFrame::Pong { .. } => panic!("welcome expected"),
        }
        let old = r#"{"type":"welcome","protocol_version":3,"daemon_version":"v","pid":1,"profile":null,"sessions":[],"progressive_tool_disclosure":true,"generation":1}"#;
        match decode_line::<DaemonFrame>(old).unwrap() {
            DaemonFrame::Welcome(w) => assert!(w.features.is_empty()),
            other => panic!("{other:?}"),
        }
        for query in [
            ClientFrame::Children { id: Some(1), session_id: None, sid: None, members: false },
            ClientFrame::ThinkingOptions { id: Some(2), model: None, warm: false },
        ] {
            let line = encode_line(&query).unwrap();
            let err = decode_line::<OldClientFrame>(line.trim_end()).unwrap_err();
            assert!(err.contains("unknown variant"), "{err}");
        }
    }

    /// C4a: `accepted_events` (the names PD vendors), the negotiation rule
    /// and old peers.
    /// The frozen `tool_child` / `tool_child_ended` shapes, their opt-in
    /// names, and an old client's decode (`Unknown`, then the usual notice).
    #[test]
    fn tool_child_events_keep_the_agreed_names_and_old_clients_skip_them() {
        use super::super::child_events::ChildProcessEvent;
        use super::super::children::{ChildIdentity, ChildSpawn};
        let child = ChildIdentity { pid: 3646158, start_ticks: 26981644, boot_id: "680bd989".into(), sid: 3646158, pgid: 3646158 };
        let spawned = SessionEventWire::ChildProcess(ChildProcessEvent::Spawned(ChildSpawn {
            tool_id: "toolu_01ABC".into(),
            tool_name: "bash".into(),
            handle_id: None,
            shell_session_id: None,
            child: child.clone(),
        }));
        let ended = SessionEventWire::ChildProcess(ChildProcessEvent::Ended {
            tool_id: "toolu_01ABC".into(),
            handle_id: Some("sa_3".into()),
            child: child.clone(),
            exit_code: None,
            signal: Some(9),
        });
        assert_eq!(OPTIONAL_EVENTS, &["tool_child", "tool_child_ended"]);
        assert_eq!(optional_event_name(&spawned), Some("tool_child"));
        assert_eq!(optional_event_name(&ended), Some("tool_child_ended"));
        assert_eq!(optional_event_name(&SessionEventWire::Idle), None);

        let inner = |e: SessionEventWire| match e {
            SessionEventWire::ChildProcess(event) => event,
            other => panic!("not a child event: {other:?}"),
        };
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let v = serde_json::to_value(WireSessionEvent::from(spawned.clone())).unwrap();
        assert_eq!(v["ev"], "tool_child");
        assert_eq!(keys(&v), ["child", "ev", "handle_id", "shell_session_id", "tool_id", "tool_name"]);
        assert!(v["handle_id"].is_null() && v["shell_session_id"].is_null(), "nulls are sent: {v}");
        assert_eq!(keys(&v["child"]), ["boot_id", "pgid", "pid", "sid", "start_ticks"]);
        assert_eq!(v["child"]["start_ticks"], 26981644);
        let back: WireSessionEvent = serde_json::from_value(v).unwrap();
        assert_eq!(inner(SessionEventWire::from(back)), inner(spawned.clone()), "round trip");

        let v = serde_json::to_value(WireSessionEvent::from(ended.clone())).unwrap();
        assert_eq!(v["ev"], "tool_child_ended");
        assert_eq!(keys(&v), ["child", "ev", "exit_code", "handle_id", "signal", "tool_id"]);
        assert_eq!((v["handle_id"].as_str(), v["signal"].as_i64(), v["exit_code"].is_null()), (Some("sa_3"), Some(9), true));
        let back: WireSessionEvent = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(inner(SessionEventWire::from(back)), inner(ended), "round trip");

        // An old client: a v0.10.2-shaped event enum with `#[serde(other)]`.
        #[derive(Deserialize)]
        #[serde(tag = "ev", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldEvent {
            Idle,
            SystemNotice { text: String },
            CostCapReached { scope: String, cost: f64, cap: f64 },
            #[serde(other)]
            Unknown,
        }
        for json in [serde_json::to_value(WireSessionEvent::from(spawned)).unwrap(), v] {
            assert!(matches!(serde_json::from_value::<OldEvent>(json).unwrap(), OldEvent::Unknown));
        }
    }

    #[test]
    fn accepted_events_are_exact_names_and_compatible_with_old_peers() {
        let known: &[&'static str] = &["tool_child", "tool_child_ended"];
        let ask = |names: &[&str]| {
            AcceptedEvents::from_known(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>(), known).names()
        };
        assert_eq!(ask(&["tool_child_ended", "bogus", "tool_child_ended"]), vec!["tool_child_ended"]);
        assert_eq!(ask(&["tool_child_ended", "tool_child"]), vec!["tool_child", "tool_child_ended"], "daemon order");
        assert!(ask(&["bogus", ""]).is_empty());
        let only_ended = AcceptedEvents::from_known(&["tool_child_ended".to_string()], known);
        assert!(only_ended.allows_name(None), "ordinary events always pass");
        assert!(only_ended.allows_name(Some("tool_child_ended")));
        assert!(!only_ended.allows_name(Some("tool_child")), "no implied groups");
        assert!(!AcceptedEvents::default().allows_name(Some("tool_child_ended")));
        assert!(AcceptedEvents::default().allows(&SessionEventWire::SystemNotice("x".into())));
        // Every optional event this daemon knows has a distinct name.
        let mut names = OPTIONAL_EVENTS.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OPTIONAL_EVENTS.len());

        // Hello: the field name; empty is omitted; a legacy Hello is empty;
        // Hello stays lenient (no deny_unknown_fields), so a released daemon,
        // whose Hello is this one without the field, accepts the new frame.
        let mut v = serde_json::to_value(Hello::new(ClientKind::Test).with_accepted_events(["tool_child"])).unwrap();
        assert_eq!(v["accepted_events"], serde_json::json!(["tool_child"]));
        let plain = serde_json::to_value(Hello::new(ClientKind::Test)).unwrap();
        assert!(plain.get("accepted_events").is_none(), "{plain}");
        let legacy: Hello = serde_json::from_value(plain).unwrap();
        assert!(legacy.accepted_events.is_empty());
        v["zz_from_a_newer_client"] = serde_json::json!(1);
        let lenient: Hello = serde_json::from_value(v).unwrap();
        assert_eq!(lenient.accepted_events, vec!["tool_child".to_string()]);

        // Welcome: the echo is present when non-empty and omitted when empty;
        // an old Welcome decodes as no echo.
        let welcome = |accepted_events: Vec<String>| {
            serde_json::to_value(&Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "v".into(),
                pid: 1,
                profile: None,
                sessions: vec![],
                progressive_tool_disclosure: false,
                generation: 1,
                daemon_build: String::new(),
                features: vec![],
                accepted_events,
            })
            .unwrap()
        };
        assert_eq!(welcome(vec!["tool_child_ended".into()])["accepted_events"], serde_json::json!(["tool_child_ended"]));
        assert!(welcome(vec![]).get("accepted_events").is_none());
        let old = r#"{"type":"welcome","protocol_version":3,"daemon_version":"v","pid":1,"profile":null,"sessions":[],"progressive_tool_disclosure":true,"generation":1}"#;
        match decode_line::<DaemonFrame>(old).unwrap() {
            DaemonFrame::Welcome(w) => assert!(w.accepted_events.is_empty()),
            other => panic!("{other:?}"),
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
