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

/// Ordered key-value snapshot of a client's environment.
pub type SessionEnv = Vec<(String, String)>;

/// Prefixes stripped from the client env before it enters the session.
/// These are client-only or daemon-only knobs — never session identity.
const ENV_STRIP_PREFIXES: &[&str] = &[
    "SYNAPS_CLIENT_",
    "SYNAPS_TUI_",
    "SYNAPS_DAEMON_",
    "SYNAPS_MEM_TRACE",
];

/// Secret patterns (case-insensitive suffix/infix) stripped at the client.
/// Secrets never reach the daemon — the broker owns credentials.
/// Build a [`SessionEnv`] from the current process environment, stripping
/// client-only prefixes and secrets. Returns `(kept_env, stripped_secret_names)`.
/// The stripped names are ONLY the secrets (not `SYNAPS_*` prefixes, those are noise).
/// Used by thin clients before `Hello`.
pub fn capture_client_env() -> (SessionEnv, Vec<String>) {
    strip_client_env(std::env::vars())
}

/// The pure part of [`capture_client_env`]: filter `vars` (unit-testable
/// without touching the process environment).
pub fn strip_client_env(
    vars: impl IntoIterator<Item = (String, String)>,
) -> (SessionEnv, Vec<String>) {
    let mut env: SessionEnv = Vec::new();
    let mut stripped: Vec<String> = Vec::new();
    for (k, v) in vars {
        // A credential VALUE is stripped whatever its key is called
        // (`DATABASE_URL=postgres://u:p@h`): the name denylist alone let it
        // through to the daemon and the session files.
        let secret_value = agent_core::core::config::is_secret_env_value(&v);
        if should_strip_env(&k) || secret_value {
            // Only record secret names, not SYNAPS_* prefix noise.
            if is_secret_key(&k) || secret_value {
                stripped.push(k);
            }
        } else {
            env.push((k, v));
        }
    }
    env.sort_by(|(a, _), (b, _)| a.cmp(b));
    stripped.sort();
    (env, stripped)
}

/// Whether a key should be stripped from the client env snapshot.
pub fn should_strip_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    for prefix in ENV_STRIP_PREFIXES {
        if upper.starts_with(prefix) {
            return true;
        }
    }
    is_secret_key(&upper)
}

/// Whether a key matches the secret denylist (case-insensitive).
/// Delegates to the canonical implementation in `agent_core::core::config`.
pub fn is_secret_key(key: &str) -> bool {
    agent_core::core::config::is_secret_key(key)
}

/// Everything `EngineHost::create_session` needs. Serializable: it is the
/// payload of the wire `Attach::Create`. Mirrors `EngineOpts` minus the
/// host-level fields (profile, no_extensions), plus the per-session facts a
/// daemon cannot inherit from its own process (cwd, env overlay).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfig {
    /// `EngineOpts.continue_session`. `Some(None)` = bare `--continue` (most
    /// recent journal). Plain serde encodes `Some(None)` as `null` and decodes
    /// that as `None`, so the daemon could never receive a bare `--continue`
    /// (soak F21) — `double_option` keeps the two levels apart on the wire.
    #[serde(default, with = "double_option", skip_serializing_if = "Option::is_none")]
    pub continue_session: Option<Option<String>>,
    /// Non-content admission policy; restored before automatic turns. No layer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_layer_wait_ms: Option<u64>,
    /// Memory-only tool globs (`*`, `?`), added to config `memory_only_tools`.
    /// Matched against an extension or MCP tool's runtime name
    /// (`plugin:tool`, `ext__server__tool`) or its wire name (`plugin_tool`).
    /// Policy only, never result text; see docs/memory-only-results.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_only_tools: Vec<String>,
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
    /// Per-session environment snapshot. `None` = inherit process env
    /// (in-process hosts). The daemon fills it from `Hello.env` on
    /// `Attach::Create` only; attaching to an existing session does not
    /// change the creator's env.
    #[serde(default)]
    pub env: Option<SessionEnv>,
    /// Names of environment variables stripped as secrets at the client
    /// (e.g. `GH_TOKEN`, `AWS_SECRET_ACCESS_KEY`). NOT the `SYNAPS_*`
    /// prefixes (those are noise). Used for loud notices when a tool fails
    /// referencing a stripped secret. Persisted in the journal (names only,
    /// never values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_stripped: Vec<String>,
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
    /// Who may drive this session ([`ClientInput`]); `None` = config
    /// `client_input`. An older daemon ignores it (and enforces the owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_input: Option<ClientInput>,
    /// `--name`: name the session at create (same rules as `/cmd saveas`;
    /// must be unique among saved sessions). Applied before the registry
    /// entry is written so `synaps send --session <name>` resolves at once.
    #[serde(default)]
    pub name: Option<String>,
    /// (E-P7, §S3) Hard per-session spend ceiling in USD. `None` = unbounded.
    /// Checked after every `Usage` stream event; a breach cancels the turn,
    /// revokes any armed driver, and emits `CostCapReached`. This is the
    /// host-owned circuit breaker for an actor-resident autonomous driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_session_cost: Option<f64>,
    /// The session's explicit `/memory` choice, recorded by the actor so a
    /// rebuilt runtime (unpark, daemon reload) restores it instead of
    /// re-applying the operator's `memory.default_mode`. `None` = no choice
    /// yet: the default applies. Host-recorded: the daemon drops a value a
    /// client sends at create.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "SessionMemoryMode::deserialize_lenient"
    )]
    pub memory_mode: Option<SessionMemoryMode>,
    /// Set by a daemon reload for a session it restores Parked (never on
    /// the wire): with `await_extensions=false`, `on_session_start` waits
    /// until the session is unparked instead of running right after create.
    #[serde(skip)]
    pub restore_parked: bool,
    /// Where this session's message times come from (never on the wire).
    /// `None` = the wall clock; tests inject a clock to advance time.
    #[serde(skip)]
    pub message_clock: Option<crate::runtime::message_time::MessageClock>,
    /// Set by a crash or stop restore for a session it restores Parked
    /// (never on the wire): when it parked, so its eviction timer continues
    /// from there instead of starting over.
    #[serde(skip)]
    pub restore_parked_since: Option<chrono::DateTime<chrono::Utc>>,
    /// How `continue_session` is looked up on disk. Set by the daemon's
    /// restore for the sessions it brings back (never on the wire, so no
    /// client create can take it). It is `pub` only because SessionConfig is
    /// built by struct literal across the workspace; nothing outside the
    /// daemon's restore (`daemon::reload::restore_config`) should set
    /// `ExactJournalId`.
    #[serde(skip)]
    pub continue_lookup: ContinueLookup,
    /// Set by `fork` on the parent (never on the wire): the new session is
    /// that parent's fork, built from this copy of its state
    /// (`session::fork`) instead of a fresh conversation.
    #[serde(skip)]
    pub fork: Option<std::sync::Arc<super::fork::ForkSeed>>,
}

/// How a create's `continue_session` query is looked up on disk
/// (`SessionConfig::continue_lookup`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContinueLookup {
    /// A user's `--continue <query>`: a chain name, then a session name,
    /// then a (partial) session id.
    #[default]
    Query,
    /// The query is the exact journal id of a session the daemon recorded
    /// itself (a crash or stop restore, a reload's rehydrate): loaded by that
    /// id when its file is on disk, with no chain or name scan; looked up as
    /// a [`Self::Query`] when it is not.
    ExactJournalId,
}

/// A durable `/memory` mode the user chose for a session
/// (`SessionConfig::memory_mode`). `/memory once` is a one-shot lease, not
/// a mode, and is not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMemoryMode {
    Off,
    RecallEachPrompt,
    CaptureOnly,
    CaptureAndRecall,
}

impl SessionMemoryMode {
    /// `SessionConfig::memory_mode` reader: a value this build does not know
    /// (a newer image's mode) reads as `Off` — the consent-safe direction —
    /// rather than an error that would fail the whole reload state and
    /// rehydrate no session at all. Null (or a missing field) is no choice.
    fn deserialize_lenient<'de, D>(d: D) -> Result<Option<Self>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(d)?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v.clone()).unwrap_or_else(|_| {
            tracing::warn!(mode = %v, "unknown session memory mode; treating as off");
            Self::Off
        })))
    }

    /// The mode `/memory <arg>` sets, if it sets one.
    pub fn from_command_arg(arg: &str) -> Option<Self> {
        match arg.trim() {
            "off" => Some(Self::Off),
            "recall" => Some(Self::RecallEachPrompt),
            "capture" => Some(Self::CaptureOnly),
            "on" => Some(Self::CaptureAndRecall),
            _ => None,
        }
    }

    /// The lease mode to grant; `None` for `Off`.
    pub fn lease_mode(self) -> Option<crate::runtime::memory_context::MemoryContextMode> {
        use crate::runtime::memory_context::MemoryContextMode as M;
        match self {
            Self::Off => None,
            Self::RecallEachPrompt => Some(M::RecallEachPrompt),
            Self::CaptureOnly => Some(M::CaptureOnly),
            Self::CaptureAndRecall => Some(M::CaptureAndRecall),
        }
    }
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
            ephemeral_layer_wait_ms: None,
            memory_only_tools: Vec::new(),
            system: None,
            prompt_manifest: None,
            cwd: None,
            env: None,
            env_stripped: Vec::new(),
            auto_approve_confirms: false,
            model_override: None,
            persist: true,
            auto_compact: false,
            compaction_policy: CompactionPolicyWire::InPlace,
            await_extensions: true,
            keep_warm: false,
            client_input: None,
            name: None,
            max_session_cost: None,
            memory_mode: None,
            restore_parked: false,
            message_clock: None,
            restore_parked_since: None,
            continue_lookup: ContinueLookup::Query,
            fork: None,
        }
    }
}

/// Serializable copy of `engine::setup::ContinueInfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinueInfoWire {
    pub session_id: String,
    /// "chain", "name", "compacted", or None.
    pub resolved_via: Option<String>,
    pub query: String,
    /// F24: notice when the session was followed through a compaction chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_notice: Option<String>,
}

impl From<&crate::engine::setup::ContinueInfo> for ContinueInfoWire {
    fn from(c: &crate::engine::setup::ContinueInfo) -> Self {
        Self {
            session_id: c.session_id.clone(),
            resolved_via: c.resolved_via.clone(),
            query: c.query.clone(),
            compaction_notice: c.compaction_notice.clone(),
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
    /// Its meaning depends on `client_input`: with `Exclusive`, or with
    /// `client_input` absent, the only client that may type; with `Shared`,
    /// the client that typed last. Never infer exclusivity from this field
    /// alone (790d R6).
    #[serde(default)]
    pub input_owner: Option<ClientId>,
    /// The kind the input owner declared in its `Hello` (`chat` = the app's
    /// drive, `tui` = a terminal). Set at list time with `input_owner`, and
    /// absent when nobody owns input. An `input_owner` with no kind beside
    /// it comes from a daemon that does not report one: it says nothing about
    /// the owner. Declared, not proven: the same trust as the socket itself.
    /// Absent from a frozen meta (an `Attached` snapshot's), so that frame's
    /// bytes are unchanged. Follows `input_owner`'s rule: with `Exclusive`
    /// (or `client_input` absent) the kind of the only client that may
    /// type, with `Shared` the kind of the client that typed last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_owner_kind: Option<ClientKind>,
    #[serde(default)]
    pub awaiting_input: usize,
    /// Every attached client, named. Empty from an older daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roster: Vec<ClientInfo>,
    /// Who may drive ([`ClientInput`]); `None` from an older daemon (which
    /// enforces the owner: `Exclusive`). With `Shared`, `input_owner` is the
    /// active client, not the only one that may type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_input: Option<ClientInput>,
    /// `conv.session.id` — differs from `id` after a LinkedSuccessor compaction.
    #[serde(default)]
    pub journal_id: String,
    /// Set when the session's journal is locked by another process (F23):
    /// `"pid N (kind)"`. Shown in `daemon sessions`; cleared on unpark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_by: Option<String>,
    /// Pinned (`KeepWarm{on}`, `/pin`): never parked, never idle-ended, and
    /// the daemon does not idle-exit while it is set. An older daemon omits
    /// it (→ false); an older client ignores it.
    #[serde(default)]
    pub pinned: bool,
    // ── state for the `/sessions` modal, published by the actor. An older
    //    daemon omits them (→ defaults); an older client ignores them. ──
    /// A turn or a compaction is running.
    #[serde(default)]
    pub busy: bool,
    /// When the session entered its current state (turn/compaction start,
    /// idle start, park time).
    #[serde(default)]
    pub state_since: Option<chrono::DateTime<chrono::Utc>>,
    /// When the idle park fires (Live, zero clients, grace armed). `None`
    /// when not armed — a worker re-check is not a park due.
    #[serde(default)]
    pub park_due: Option<chrono::DateTime<chrono::Utc>>,
    /// When a Parked session is evicted; `None` if not parked or never.
    #[serde(default)]
    pub evict_due: Option<chrono::DateTime<chrono::Utc>>,
    /// The live conversation's title, as of the last publish.
    #[serde(default)]
    pub title: Option<String>,
    /// Session cost in USD, as of the last publish.
    #[serde(default)]
    pub cost_usd: f64,
    /// API message count, as of the last publish.
    #[serde(default)]
    pub messages: usize,
    /// Listed by a daemon whose start is still restoring this session from
    /// its crash record (not attachable yet; an attach waits for it). The
    /// other fields come from the record, `lifecycle` as recorded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restoring: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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

pub use crate::core::config::ClientInput;

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
    /// Additive stream events this client can decode. Absent from older
    /// clients (all `false`), so a newer daemon never sends them one; an
    /// older daemon ignores the unknown field.
    #[serde(default, skip_serializing_if = "ClientCapabilities::is_none")]
    pub capabilities: ClientCapabilities,
    /// What this client asks to be called (`--as`, `SYNAPS_CLIENT_NAME`):
    /// a label, not a login — every client is the daemon's own user. The
    /// session resolves it ([`resolve_client_name`]) and reports the result
    /// in `ClientJoined`/`roster`. Absent = the kind's name. An older
    /// daemon ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Longest client name ([`validate_client_name`]).
pub const CLIENT_NAME_MAX: usize = 32;

/// A client name is 1–32 ASCII letters, digits, `-` or `_`, starting with
/// a letter or digit. The client checks before connecting; the daemon
/// checks again and refuses the handshake.
pub fn validate_client_name(name: &str) -> Result<(), String> {
    let ok_first = name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    let ok_rest = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if name.is_empty() || name.len() > CLIENT_NAME_MAX || !ok_first || !ok_rest {
        return Err(format!(
            "invalid client name {name:?}: use 1-{CLIENT_NAME_MAX} letters, digits, '-' or '_', starting with a letter or digit"
        ));
    }
    Ok(())
}

/// `SYNAPS_CLIENT_NAME`, trimmed; `None` when unset or empty. Not
/// validated here: the caller reports a bad one.
pub fn client_name_from_env() -> Option<String> {
    std::env::var("SYNAPS_CLIENT_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The name a joining client gets in a session: its requested name, or its
/// kind's name if it asked for none (or an invalid one), with the lowest
/// free `-N` suffix (N ≥ 2) if another client already has it. Names
/// compare case-insensitively.
pub fn resolve_client_name<'a>(
    requested: Option<&str>,
    kind: ClientKind,
    taken: impl IntoIterator<Item = &'a str>,
) -> String {
    let base = requested
        .filter(|n| validate_client_name(n).is_ok())
        .unwrap_or(kind.default_name());
    let taken: std::collections::HashSet<String> = taken.into_iter().map(str::to_ascii_lowercase).collect();
    if !taken.contains(&base.to_ascii_lowercase()) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(&c.to_ascii_lowercase()))
        .expect("an unbounded range always has a free suffix")
}

/// One attached client as every client sees it (`AttachSnapshot.roster`,
/// `ClientJoined`, `/clients`, `synaps daemon sessions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: ClientId,
    /// Resolved by the session ([`resolve_client_name`]); unique in it.
    pub name: String,
    pub kind: ClientKind,
    pub mode: AttachMode,
    /// Unix ms.
    pub attached_at_ms: u64,
}

impl ClientInfo {
    /// `discord (#3)`: the name for people, the id for scripts that grep.
    pub fn label(&self) -> String {
        format!("{} (#{})", self.name, self.id.0)
    }
}

/// A client-side copy of the session's roster, kept current from the
/// attach snapshot, `ClientJoined` and `ClientLeft` (each transport owns
/// one).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roster(Vec<ClientInfo>);

impl Roster {
    pub fn from_snapshot(snapshot: &AttachSnapshot) -> Self {
        let mut r = Self(snapshot.roster.clone());
        // An older daemon sends no roster: fall back to ids and kinds.
        if r.0.is_empty() {
            r.0 = snapshot
                .clients
                .iter()
                .map(|(id, kind)| ClientInfo {
                    id: *id,
                    name: kind.default_name().to_string(),
                    kind: *kind,
                    mode: AttachMode::Mirror,
                    attached_at_ms: 0,
                })
                .collect();
        }
        r
    }

    pub fn apply(&mut self, event: &SessionEventWire) {
        match event {
            SessionEventWire::ClientJoined { client, kind, name, mode } => {
                self.0.retain(|c| c.id != *client);
                self.0.push(ClientInfo {
                    id: *client,
                    name: if name.is_empty() { kind.default_name().to_string() } else { name.clone() },
                    kind: *kind,
                    mode: *mode,
                    // The event carries no time: it just happened.
                    attached_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
                });
                self.0.sort_by_key(|c| c.id.0);
            }
            SessionEventWire::ClientLeft { client } => self.0.retain(|c| c.id != *client),
            _ => {}
        }
    }

    pub fn clients(&self) -> &[ClientInfo] {
        &self.0
    }

    pub fn get(&self, id: ClientId) -> Option<&ClientInfo> {
        self.0.iter().find(|c| c.id == id)
    }

    /// `discord (#3)`, or `client #3` for an id not in the roster.
    pub fn label(&self, id: ClientId) -> String {
        self.get(id).map(ClientInfo::label).unwrap_or_else(|| format!("client #{}", id.0))
    }
}

/// Opt-in stream signals (see [`ClientMeta::capabilities`]). The daemon's
/// event enums nested under `Stream` (`WireLlmEvent`, ...) have no
/// `#[serde(other)]`: an older client fails the whole frame on an unknown
/// variant — and a failed frame reads as a dead socket (the TUI drops into
/// reconnect, whose `Attached` replay would carry the same variant). So
/// every new nested variant is sent only to a client that declares it here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// `LlmEvent::ToolQueued` / `ToolStarted` (scheduler lane waits).
    #[serde(default)]
    pub tool_queue: bool,
    /// Knows `client_input = shared`: `OwnerChangeReason::Input` and
    /// `Attached.client_input`. Without it, a shared session is presented
    /// in owner terms (`wire::legacy_shared`).
    #[serde(default)]
    pub owner_reason_input: bool,
}

impl ClientCapabilities {
    /// What this build decodes.
    pub fn current() -> Self {
        Self { tool_queue: true, owner_reason_input: true }
    }

    fn is_none(&self) -> bool {
        *self == Self::default()
    }
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
            capabilities: ClientCapabilities::current(),
            name: None,
        }
    }

    /// With a requested name (see [`ClientMeta::name`]).
    pub fn named(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
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

impl ClientKind {
    /// The name a client of this kind gets when it asks for none.
    pub fn default_name(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Chat => "chat",
            Self::Attach => "attach",
            Self::Rpc => "rpc",
            Self::Server => "server",
            Self::Test => "test",
        }
    }
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
/// (runtime/mod.rs).
pub const CONFIRM_PROMPT_TITLE: &str = "Confirm tool call";
/// Fixed title `confirm_activation_with_host` uses for the `activate_tools`
/// y/yes gate (tools/discovery.rs). Everything not in the confirm set is
/// `Secret` (masked input).
pub const CONFIRM_ACTIVATION_PROMPT_TITLE: &str = "Confirm tool activation";

impl PromptKind {
    /// Derive the kind from the prompt title (the actor classifies at receipt;
    /// `SecretPromptHandle::prompt` classifies for the in-process TUI).
    pub fn from_title(title: &str) -> Self {
        if title == CONFIRM_PROMPT_TITLE || title == CONFIRM_ACTIVATION_PROMPT_TITLE {
            Self::Confirm
        } else {
            Self::Secret
        }
    }
}

/// Who raised an extension tool-call Confirm and what "Allow all" would
/// cover. Built host-side from the hook chain (`HookBus::emit` stamps the
/// extension ids; an extension cannot name itself or another extension), so
/// the dialog can show the exact scope BEFORE the user answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmScope {
    /// Extension ids whose `Confirm` this prompt resolves; "Allow all"
    /// latches exactly these.
    pub extensions: Vec<String>,
    /// Tool the call targets (display only; the latch is per extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Extensions whose `Modify` rewrote the input this prompt approves
    /// (empty = the model's own input). Lets the session record, for every
    /// client's transcript, that an approved call ran rewritten input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewritten_by: Vec<String>,
    /// The provider's id for the call this prompt confirms (`tool_use.id`),
    /// the same id that call's `before_tool_call` event carries, so a client
    /// pairs the prompt with its call by identity. Absent when the dispatch
    /// site has none; omitted from the JSON then, so old clients see the
    /// shape they knew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ConfirmScope {
    /// `a` or `a, b`.
    pub fn extensions_label(&self) -> String {
        self.extensions.join(", ")
    }

    /// Text of the "Allow all" button (without the key hint).
    pub fn allow_all_label(&self) -> String {
        format!("Allow all from {} this session", self.extensions_label())
    }

    /// The exact reach of "Allow all", shown before the user answers.
    pub fn allow_all_scope(&self) -> String {
        format!(
            "Allow all: later confirms from {} (any tool) are approved without asking until \
             /new or /confirms reset. Other extensions still ask.",
            self.extensions_label()
        )
    }
}

/// Make prompt text safe to print in any client (prA I-R4-3, I-R5): drop
/// C0/C1 control characters except `\n` (a `\t` becomes a space); the
/// zero-width space and the LRM/RLM marks (U+200B, U+200E, U+200F); bidi
/// embeddings/overrides/isolates (U+202A–U+202E, U+2066–U+2069) and the
/// Arabic letter mark (U+061C); and the line/paragraph separators (U+2028,
/// U+2029), which some terminals treat as line breaks. ZWNJ / ZWJ
/// (U+200C, U+200D) are KEPT: emoji sequences and several scripts need
/// them. Prevents an extension's text from moving the cursor (`\x1b[3A`,
/// `\r`) over the host-built header, or disguising text with direction
/// overrides.
pub fn sanitize_prompt_text(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{2028}' | '\u{2029}' => None,
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => None,
            c => Some(c),
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    /// Per-session monotonic; actor dedups answers on it.
    pub id: u64,
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    pub raised_at: chrono::DateTime<chrono::Utc>,
    /// Extension tool-call confirms only: the source extension(s) and tool.
    /// Absent → "Allow all" is not offered (it could not be scoped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_scope: Option<ConfirmScope>,
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

/// Whether a session's actor resumes the workers a daemon reload stopped
/// (`SessionCommand::AdoptWorkers`), instead of telling the model to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerResume {
    /// Resume each one the daemon may (see
    /// [`crate::runtime::subagent::plan_daemon_resumes`]).
    Auto,
    /// Leave them to the model, saying why (e.g. the session was parked).
    Off(&'static str),
    /// Leave them to the model as before, saying nothing more (a crash or
    /// stop restore: not built for these yet).
    Never,
}

/// The entire client→session control surface (SEAMS §5.2/§5.3).
///
/// `Debug` is manual: `Submit`/`Steer`/`SubmitPrepared`/`Answer`/`Resume`
/// print `<redacted>` (no lengths) so a command can be traced by *type*.
#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SessionCommand {
    /// User-authored prompt. Actor: reset auto-turn counter, push user msg
    /// verbatim, start turn.
    Submit {
        text: String,
        /// Pre-built canonical user content blocks (images, documents)
        /// produced by `PendingAttachments::build_content` on the client.
        /// Empty ⇔ text-only (backward compatible).
        #[serde(default)]
        attachments: Vec<serde_json::Value>,
    },
    /// Text typed while streaming. Actor: steer if a steer_tx is live else
    /// queue; ALWAYS also sets queued_message.
    Steer { text: String },
    /// Steer one of this session's running subagents (`sa_N`) from its watch
    /// view. Owner-only like `Steer`. The note also enters the MAIN
    /// conversation as an event (no auto-turn), so the main agent knows the
    /// user redirected its worker. Failure → `Refused`.
    SubagentSteer { handle_id: String, text: String },
    /// Esc. Cancel, adopt the turn's partial history + append the
    /// interruption marker (`engine::interrupt`), dequeue, flush pending
    /// events, cancel subagents, save.
    Cancel,
    /// Answer to a PromptRequest. `None` = cancelled.
    /// NEVER journaled, NEVER replayed, NEVER traced.
    Answer { prompt_id: u64, value: Option<String> },
    /// `id` is client-chosen and echoed in `SettingChanged.id`.
    Set { id: u64, setting: SessionSetting },
    /// Request-only material, never added to settings_replay.
    EphemeralLayer {
        id: u64,
        epoch: String,
        generation: u64,
        #[serde(flatten)]
        operation: crate::runtime::ephemeral::Operation,
    },
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
    /// (+ optional user text) then a turn. Does NOT reset
    /// consecutive_auto_turns.
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
    /// (B1, used by C3 reload) cancel any turn (partial history + marker),
    /// abort compaction, save, close PTYs, emit Notice. Never ends the
    /// session. Reply = `QueryResult{id: CHECKPOINT_QUERY_ID, {ok:true}}`.
    Checkpoint {
        reason: CheckpointReason,
        /// Host-only (never wire): the absolute deadline the caller waits
        /// for this checkpoint until (the reload coordinator's). `None`:
        /// the actor's own bound from receipt (`budgets::CHECKPOINT_BUDGET`).
        #[serde(skip)]
        deadline: Option<tokio::time::Instant>,
        /// Host-only (never wire): the reload's drain ran to its cap, so a
        /// quiesced turn has had its chance to end at a round boundary and
        /// gets no [`crate::session::budgets::QUIESCE_GRACE`] on top. `false`
        /// (and every checkpoint read from the wire): the grace as before.
        #[serde(skip)]
        skip_quiesce_grace: bool,
    },
    /// (B3) pin/unpin (`/pin on|off`, `--keep-warm`, `/keep-warm on|off`,
    /// `synaps daemon pin`). Published on `SessionHandle::pinned()`.
    KeepWarm { on: bool },
    /// (B1) `/takeover`: the sending client takes input ownership in place,
    /// like an `--takeover` attach without re-attaching. Not an input
    /// command (a read-only client sends it); refused from an `Observe`
    /// client, a no-op from the owner. Emits `InputOwnerChanged{Takeover}`
    /// (`Attach` when nobody owned input).
    TakeInput,
    /// (S1, Focus) Delivery mode: `hold` sends every event that would reach
    /// the context to the held store instead, for `lease_secs` (30..=3600,
    /// required with hold; ignored with auto); hold again renews. `auto`
    /// releases nothing by itself. Not a `SessionSetting`: the mode lives in
    /// the held store, never in a reload record. Reply = `DeliveryChanged`
    /// to every client. Open to every attached client.
    Delivery {
        mode: super::held::DeliveryMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease_secs: Option<u64>,
    },
    /// (S1) Release held items: `ids` null = every held, join_next or
    /// handled_on_side item. `as`: digest (ONE user message, at most one
    /// turn), join_next (in front of the next user message, no turn),
    /// individual (back into the queue; auto mode only). `text` (at most
    /// 16384 bytes): the app's own digest/join text, refused with
    /// individual. Reply = `HeldUpdated`; refusal = `Refused`.
    HeldRelease {
        #[serde(default)]
        ids: Option<Vec<String>>,
        #[serde(rename = "as")]
        release_as: super::held::ReleaseAs,
        #[serde(default)]
        text: Option<String>,
    },
    /// (S1) Mark items dismissed; nothing is deleted (the store keeps the
    /// text for 7 days). Reply = `HeldUpdated`.
    HeldDismiss { ids: Vec<String> },
    /// (S1) Mark one item handled in the side session `side_session`.
    /// Reply = `HeldUpdated`.
    HeldMark { id: String, side_session: String },
    /// Host-only (never wire): park now if `can_park()` (reload rehydrates
    /// a Parked session Parked). A no-op otherwise.
    #[serde(skip)]
    Park,
    /// Host-only (never wire): reload's rehydrate hands the new image the
    /// workers the old image stopped or still owed a wake
    /// (`SessionReloadRecord::workers`). The actor re-registers their ids
    /// against their archives and wakes the parent once per worker.
    /// `resume`: whether the actor resumes the workers the reload stopped
    /// itself, or leaves them to the model.
    #[serde(skip)]
    AdoptWorkers {
        workers: Vec<crate::runtime::subagent::CarriedWorker>,
        resume: WorkerResume,
    },
    /// Host-only (never wire): reload's rehydrate hands the new image the
    /// exact tool activations the session held in the previous image
    /// (`SessionReloadRecord::tool_activations`). The runtime re-validates
    /// and restores them at its first turn start; a parked session keeps
    /// them for its unpark. Sent before anything can start a turn.
    #[serde(skip)]
    RestoreToolActivations {
        activations: Vec<crate::tools::activation::CarriedActivation>,
    },
    /// Host-only (never wire): reload's rehydrate, last, for a session that
    /// comes back Live. The old image's park timer did not survive the exec
    /// and only an attach or detach arms a new one, so a session no client
    /// reconnects to stayed Live with zero clients forever. Arms the park
    /// grace as a detach does; a client reconnecting within it clears it.
    /// A no-op on a parked session.
    #[serde(skip)]
    ArmPark,
    /// Host-only (never wire): a reload's or a crash restore's rehydrate
    /// hands the new actor the capacity resume its record carried
    /// (`ReloadSession::capacity_stop`), after the resume decision and the
    /// carried workers, before any Park. The actor arms it only while its
    /// restore window is open (nothing started a turn, the owner did not
    /// cancel or replace the session, no checkpoint, no arm before), and
    /// only a stop that validates; never when a client loaded the session
    /// first (the daemon does not send it then).
    #[serde(skip)]
    ArmCapacityResume { stop: CapacityStop },
    /// Host-only (never wire): the reload this session checkpointed for
    /// did not exec (the exec failed, or the reload stopped after the
    /// checkpoints). The checkpoint's reload fence lifts, and the workers
    /// the reload stopped are announced to this same image.
    #[serde(skip)]
    ReloadAborted,
    /// Host-only (never wire): a reload refused after its drain, before
    /// anything was stopped (spec §5; nothing checkpointed). The session's
    /// quiesce lifts, and a turn the drain paused at a round boundary
    /// resumes in place: ONE message ("resume: a daemon reload was
    /// refused ...") starts a new turn, unless something re-prompted the
    /// turn since. `refused_at`: when the reload was refused.
    #[serde(skip)]
    ReloadRefused { refused_at: chrono::DateTime<chrono::Utc> },
    /// Host-only (never wire): reload's rehydrate, in the NEW image, for a
    /// session that came back Live. When its history shows the reload cut
    /// its turn (the latest interruption marker is the reload's
    /// `Restart` marker and nothing answered it), the actor queues ONE
    /// message that starts a new turn ("resume: ..."), at most once per
    /// `generation`. `turn_cut`: what the old image's checkpoint recorded
    /// (`Some(false)`: it cut nothing); `None` (an older image that does not
    /// record it, or no checkpoint reply) resumes nothing: the history
    /// can't tell this reload's marker from an earlier one's.
    /// `stopped`: not a reload but a clean stop whose record a later start
    /// restored (`daemon::crash_record`); the stop checkpointed the turns
    /// the same way, only the message says so. `cut_tail`: how many
    /// messages followed the cut turn's marker at the checkpoint (that
    /// turn's own tail, `SessionReloadRecord::cut_tail`); a user message
    /// past them re-prompted the turn, and nothing is queued. `None` (an
    /// older image's record): no re-prompt check. `reply`: the actor's
    /// decision ("resumed", "skip", "none"), for a restore's summary line.
    #[serde(skip)]
    ResumeAfterReload {
        generation: u64,
        reloaded_at: chrono::DateTime<chrono::Utc>,
        turn_cut: Option<bool>,
        cut_tail: Option<u32>,
        stopped: bool,
        reply: Option<tokio::sync::oneshot::Sender<&'static str>>,
    },
    /// Host-only (never wire): a fresh daemon's crash restore, for a
    /// session it brought back Live from the dead daemon's crash record
    /// (`daemon::crash_record`). When the record says a turn was in flight,
    /// THIS load folded that turn's draft in (the `Crash` marker it wrote),
    /// nothing answered it and nothing re-prompted it since, the actor
    /// queues ONE message that starts a new turn ("resume: the daemon
    /// restarted after a crash ..."), at most once per `crash` (the dead
    /// writer's pid and start time). `lost_workers`: subagents the record
    /// saw running, named in the message (they died with the daemon); a
    /// session that was idle while they ran gets one note naming them in
    /// place of the completion wakes the crash ate.
    #[serde(skip)]
    ResumeAfterCrash {
        crash: String,
        crashed_at: chrono::DateTime<chrono::Utc>,
        turn_in_flight: bool,
        lost_workers: Vec<String>,
        /// The actor's decision ("resumed", "skip", "none"), for the
        /// restore's summary line. Dropped unanswered by a parked or
        /// ending actor.
        reply: Option<tokio::sync::oneshot::Sender<&'static str>>,
    },
    /// Host→session (never wire): the actor re-emits as `SessionEventWire`.
    #[serde(skip)]
    HostEvent(HostEvent),
    /// (E-P0) Start or re-arm the session driver for `plugin`.
    /// `command` is the interactive command name (e.g. "auto");
    /// `arg` is everything after the command ("start -- do X").
    /// No `DriverStop` — `Cancel` handles that (§2).
    DriverStart {
        plugin: String,
        command: String,
        arg: String,
    },
    /// Fork this session (side chats): a new live session starts from its
    /// state as sent (`session::fork`). Reply = `Forked` to THIS client
    /// only, or `Refused{command: "fork"}`.
    Fork { label: String },
    /// Client tools v1 (docs/client-tools.md): the input owner registers
    /// tools it answers itself. The fields are checked by the actor
    /// (`session::client_tools`), so a bad shape is an `invalid` reply.
    /// Reply = `ClientTools` to THIS client only.
    RegisterClientTools {
        id: u64,
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
    /// Client tools v1: the installer answers one `tool_invoke`. Reply =
    /// `ToolReturnAck` to THIS client only. Content is never traced.
    ToolReturn {
        id: u64,
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
    /// Host-only (never wire): the fork's create finished (`session::fork`);
    /// the parent answers the client that asked. `result`: the fork's
    /// session id, or why the create failed.
    #[serde(skip)]
    ForkDone {
        client: Option<ClientId>,
        label: String,
        message_count: usize,
        result: Result<String, String>,
    },
}

/// What `Checkpoint{Reload}` reports so `daemon reload` can rebuild the
/// session as it IS (not as it was created): the actor's config, keep-warm
/// pin, lifecycle, the non-persisted knobs (`settings_replay`, incl.
/// `/system`), the CURRENT model/thinking, and its tool activations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionReloadRecord {
    pub config: SessionConfig,
    pub keep_warm: bool,
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
    pub settings_replay: Vec<SessionSetting>,
    /// Per-session `/keepwarm` override (`off`/`auto`/`<N>h`), if any.
    #[serde(default)]
    pub cache_keepwarm: Option<String>,
    pub model: String,
    pub thinking_level: String,
    /// Workers a reload stopped (or that still owed a completion wake), so
    /// the next image can re-register their ids and wake the parent.
    #[serde(default)]
    pub workers: Vec<crate::runtime::subagent::CarriedWorker>,
    /// The session's exact tool activations (`activate_tools` grants), so
    /// the next image's runtime keeps the tools array the cached prompt
    /// prefix starts with. Pins only, re-validated on restore.
    #[serde(default)]
    pub tool_activations: Vec<crate::tools::activation::CarriedActivation>,
    /// A reload checkpoint cut this session's running turn (it was
    /// streaming, and the cancel left the `Restart` marker unanswered). The
    /// next image resumes such a session (`ResumeAfterReload`).
    #[serde(default)]
    pub turn_cut: bool,
    /// With `turn_cut`: how many messages followed the `Restart` marker
    /// when the checkpoint saved the session (the cut turn's own tail:
    /// events that arrived during the turn, a host note). The next image
    /// counts only user messages past them as a re-prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_tail: Option<u32>,
    /// The session's last turn stopped on a reset-based capacity limit and
    /// is owed one automatic resume (`session::capacity_resume`). Older
    /// images ignore the field; a rollback loses a pending resume. Read
    /// leniently: one that does not parse drops only itself
    /// (`capacity_resume::lenient_stop`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::session::capacity_resume::lenient_stop"
    )]
    pub capacity_stop: Option<CapacityStop>,
    /// When the session parked (`None`: Live, or an older image's record),
    /// so the next image continues its eviction clock instead of starting
    /// it over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_since: Option<chrono::DateTime<chrono::Utc>>,
}

/// A turn that ended on a reset-based capacity stop (no Anthropic account
/// had capacity, and a limited one comes back at `resume_at`): what the
/// session resumes from, once, at that time. It carries no identity, token,
/// account label or text: the failure kind and a digest of the history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityStop {
    /// The reset the turn stopped on (no jitter).
    pub resume_at: chrono::DateTime<chrono::Utc>,
    pub stopped_at: chrono::DateTime<chrono::Utc>,
    /// The selection-failure kind (a closed set; validated on load).
    pub kind: String,
    /// The history's length when the turn stopped.
    pub history_len: u32,
    /// sha256 of the history's last message, 64 lowercase hex.
    pub last_digest: String,
    /// Capacity resumes already dispatched in this chain (0..=2).
    pub attempt: u8,
}

/// What a session tells a client whose command a daemon reload refused.
pub const RELOADING_REFUSAL: &str = "daemon reloading; retry in a moment";

impl SessionCommand {
    /// Refused while a daemon reload has checkpointed the session (its
    /// reload fence is up) until the exec, or until the daemon reports the
    /// exec failed: anything that starts a turn (a `Steer` with no turn
    /// running is a `Submit`), changes the conversation or journal the
    /// record points at, launches or steers work, or changes a setting the
    /// record already carries. Cancel, answers, queries, saves and
    /// attach/detach stay open.
    pub fn refused_while_reloading(&self) -> bool {
        matches!(
            self,
            Self::Submit { .. }
                | Self::SubmitPrepared { .. }
                | Self::Steer { .. }
                | Self::SubagentSteer { .. }
                | Self::Compact { .. }
                | Self::NewSession
                | Self::Resume { .. }
                | Self::EngineCommand { .. }
                | Self::PluginCommand { .. }
                | Self::DriverStart { .. }
                | Self::Set { .. }
                | Self::EphemeralLayer { .. }
                | Self::KeepWarm { .. }
                | Self::HeldRelease { .. }
                | Self::Fork { .. }
        )
    }
}

impl std::fmt::Debug for SessionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submit { attachments, .. } => {
                let kinds: Vec<&str> = attachments
                    .iter()
                    .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
                    .collect();
                f.debug_struct("Submit")
                    .field("text", &format_args!("<redacted>"))
                    .field("attachments", &format_args!("{}:{:?}", attachments.len(), kinds))
                    .finish()
            }
            Self::Steer { .. } => f
                .debug_struct("Steer")
                .field("text", &format_args!("<redacted>"))
                .finish(),
            Self::SubagentSteer { handle_id, .. } => f
                .debug_struct("SubagentSteer")
                .field("handle_id", handle_id)
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
            Self::EphemeralLayer { id, .. } => f.debug_struct("EphemeralLayer").field("id", id).finish_non_exhaustive(),
            Self::RegisterClientTools { id, .. } => {
                f.debug_struct("RegisterClientTools").field("id", id).finish_non_exhaustive()
            }
            Self::ToolReturn { id, .. } => f.debug_struct("ToolReturn").field("id", id).finish_non_exhaustive(),
            Self::Set { id, setting } => {
                f.debug_struct("Set").field("id", id).field("setting", setting).finish()
            }
            Self::Compact { instructions } => {
                f.debug_struct("Compact").field("instructions", instructions).finish()
            }
            Self::NewSession => f.write_str("NewSession"),
            Self::Save => f.write_str("Save"),
            Self::TakeInput => f.write_str("TakeInput"),
            Self::Park => f.write_str("Park"),
            Self::AdoptWorkers { workers, resume } => f
                .debug_struct("AdoptWorkers")
                .field("workers", &workers.iter().map(|w| w.id.as_str()).collect::<Vec<_>>())
                .field("resume", resume)
                .finish(),
            Self::ArmCapacityResume { stop } => f
                .debug_struct("ArmCapacityResume")
                .field("resume_at", &stop.resume_at)
                .field("attempt", &stop.attempt)
                .finish(),
            Self::RestoreToolActivations { activations } => f
                .debug_struct("RestoreToolActivations")
                .field(
                    "tools",
                    &activations.iter().map(|a| a.tool_id.as_str()).collect::<Vec<_>>(),
                )
                .finish(),
            Self::ReloadAborted => f.write_str("ReloadAborted"),
            Self::ArmPark => f.write_str("ArmPark"),
            Self::ReloadRefused { refused_at } => {
                f.debug_struct("ReloadRefused").field("refused_at", refused_at).finish()
            }
            Self::ResumeAfterReload { generation, reloaded_at, turn_cut, cut_tail, stopped, .. } => f
                .debug_struct("ResumeAfterReload")
                .field("generation", generation)
                .field("reloaded_at", reloaded_at)
                .field("turn_cut", turn_cut)
                .field("cut_tail", cut_tail)
                .field("stopped", stopped)
                .finish(),
            Self::ResumeAfterCrash { crash, crashed_at, turn_in_flight, lost_workers, .. } => f
                .debug_struct("ResumeAfterCrash")
                .field("crash", crash)
                .field("crashed_at", crashed_at)
                .field("turn_in_flight", turn_in_flight)
                .field("lost_workers", lost_workers)
                .finish(),
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
            Self::Checkpoint { reason, deadline, skip_quiesce_grace } => f
                .debug_struct("Checkpoint")
                .field("reason", reason)
                .field("deadline", deadline)
                .field("skip_quiesce_grace", skip_quiesce_grace)
                .finish(),
            Self::KeepWarm { on } => f.debug_struct("KeepWarm").field("on", on).finish(),
            Self::Delivery { mode, lease_secs } => f
                .debug_struct("Delivery")
                .field("mode", mode)
                .field("lease_secs", lease_secs)
                .finish(),
            Self::HeldRelease { ids, release_as, text } => f
                .debug_struct("HeldRelease")
                .field("ids", ids)
                .field("as", release_as)
                .field("text", &format_args!("{}", if text.is_some() { "<redacted>" } else { "None" }))
                .finish(),
            Self::HeldDismiss { ids } => f.debug_struct("HeldDismiss").field("ids", ids).finish(),
            Self::HeldMark { id, side_session } => f
                .debug_struct("HeldMark")
                .field("id", id)
                .field("side_session", side_session)
                .finish(),
            Self::HostEvent(ev) => f.debug_tuple("HostEvent").field(ev).finish(),
            Self::DriverStart { plugin, command, .. } => f
                .debug_struct("DriverStart")
                .field("plugin", plugin)
                .field("command", command)
                .field("arg", &format_args!("<redacted>"))
                .finish(),
            Self::Fork { .. } => f
                .debug_struct("Fork")
                .field("label", &format_args!("<redacted>"))
                .finish(),
            Self::ForkDone { client, message_count, result, .. } => f
                .debug_struct("ForkDone")
                .field("client", client)
                .field("message_count", message_count)
                .field("ok", &result.is_ok())
                .finish(),
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
    /// Add to this session's memory-only tool globs
    /// (`SessionConfig.memory_only_tools`, docs/memory-only-results.md).
    /// Add-only: `globs` must hold every glob the session already has (a
    /// superset adds, an identical resend is ok); a list that drops one is
    /// refused (`ok:false`, the message names the dropped globs). Marks every
    /// later check, in the turn in flight and in running workers too; a
    /// result already held stays held. The reply echoes the effective list.
    MemoryOnlyTools { globs: Vec<String> },
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
    /// `memory_only_tools` replies only (ok or refused): every glob in force
    /// on the session after the Set, config globs first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_only_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningClampWire {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum SessionQuery {
    EphemeralLayer,
    /// `{kind:"memory_only", live, live_bytes, per_result_cap, session_cap,
    /// evicted, refused, set_refused}` — counts only, never result text
    /// (docs/memory-only-results.md). `set_refused` counts live
    /// `memory_only_tools` Sets refused for dropping a glob.
    MemoryOnly,
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
    /// `runtime::subagent_watch::WatchPage`: one of this session's workers,
    /// its journal from `since` (the previous page's `next_seq`; 0 = all).
    /// Unknown handle → `{"error": ..}`. `tool_queue`: include
    /// `WatchEvent::ToolQueued`/`ToolStarted` entries (an older client
    /// omits it and would fail to decode a page that had them; an older
    /// daemon ignores it and never records them).
    SubagentWatch {
        handle_id: String,
        since: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tool_queue: bool,
    },
    /// (S1) `held::HeldList`: the mode, the lease and the items not released
    /// or dismissed (no text).
    HeldList,
    /// (S1) `held::HeldItemWithText` for one id, or
    /// `{"error": "unknown held id"}`.
    HeldShow { held_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ClientQuit,
    HostShutdown,
    Evicted,
    Error,
    /// F18: zero turns, zero clients, park grace elapsed. There is nothing
    /// to journal, so the session cannot park; it ends instead of holding a
    /// Runtime resident forever.
    Idle,
}

// ── events + envelope ─────────────────────────────────────────────────────────

/// What a session tells its clients. `Stream` carries the un-serialised
/// StreamEvent in-process (LocalTransport) — that IS the byte-identical
/// contract. The socket boundary converts to `wire::WireSessionEvent`.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Attached carries the full snapshot by design (§2.4)
pub enum SessionEventWire {
    EphemeralLayerChanged { status: crate::runtime::ephemeral::Status },
    Stream(crate::StreamEvent),
    /// Actor bookkeeping the client needs to mirror `App` fields exactly.
    /// `user_text` = the prompt that started the turn: the queued text on
    /// `QueuedAuto` (every client pushes the User card + scroll), the
    /// submitted text on `User` (clients OTHER than the submitter push the
    /// card; the submitter drew it at submit); `None` otherwise. The attach
    /// replay ring stores it as `None` (the snapshot's history has it).
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
    /// `client_tools`: only on the `Messages` answer, and only when a tool
    /// result in that history came from a client tool (its stamp, by
    /// `tool_use_id`); `None` otherwise, so the frame is unchanged.
    QueryResult {
        id: u64,
        value: serde_json::Value,
        client_tools: Option<ClientToolStamps>,
    },
    /// Only to the attaching client.
    Attached {
        client: ClientId,
        snapshot: AttachSnapshot,
    },
    /// `name`: resolved by the session (unique in it).
    ClientJoined { client: ClientId, kind: ClientKind, name: String, mode: AttachMode },
    ClientLeft { client: ClientId },
    Ended { reason: EndReason },
    /// Cancel landed. TUI: drop_empty_thinking, push Error(abort_msg),
    /// subagents.clear(), streaming=false. `context_saved` (name kept for
    /// protocol v3): the interrupted turn's partial work is kept in history.
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
    /// (E-P0) Driver armed on this session.
    DriverArmed {
        plugin_id: String,
        run_id: String,
        models: Vec<crate::extensions::session_driver::Selection>,
        selection: crate::extensions::session_driver::Selection,
        /// Remaining ms, not absolute Instant.
        deadline_ms: Option<u64>,
        notice: String,
    },
    /// (E-P0) Driver revoked.
    DriverRevoked {
        reason: String,
        undelivered_steering: Vec<String>,
    },
    /// (E-P0) One driver turn completed.
    DriverTurnOutcome {
        outcome: crate::extensions::session_driver::Outcome,
        selection: crate::extensions::session_driver::Selection,
        feedback: Option<String>,
    },
    /// (E-P7, §S3) A spend ceiling was breached: the turn was cancelled and any
    /// armed driver revoked. `scope` is "session" (config cap) or "run" (the
    /// driver grant's per-run cap). Additive v3 driver event — old clients skip
    /// it via `#[serde(other)]`.
    CostCapReached {
        scope: String,
        cost: f64,
        cap: f64,
    },
    /// A tool call spawned or reaped a child process (`tool_child` /
    /// `tool_child_ended`). Optional: sent only to connections that list it
    /// in `Hello.accepted_events`; never replayed on Attach.
    ChildProcess(super::child_events::ChildProcessEvent),
    /// A subagent's paid request was booked into the session's totals
    /// (`child_spend`, child spend). Optional: it goes out on the session's
    /// optional channel ([`SessionHandle::subscribe_optional`]), never the
    /// main one, and the daemon forwards it only to connections whose Hello
    /// accepted "child_spend"; never replayed on attach.
    ///
    /// [`SessionHandle::subscribe_optional`]: super::SessionHandle::subscribe_optional
    ChildSpend(ChildSpendEvent),
    /// A connection's optional receiver fell behind and `dropped`
    /// child_spend frames were overwritten before it read them. Made by the
    /// daemon's forwarder for its own connection (envelope seq `u64::MAX`),
    /// never by the actor, and sent only on a connection that accepted
    /// "child_spend", right before the first frame that survived the gap
    /// (child spend §7.4). `dropped` counts child_spend frames only.
    ChildSpendLagged { dropped: u64 },
    /// (S1) An event went to the held store instead of the context.
    EventHeld { item: super::held::HeldItem },
    /// (S1) The delivery mode or its lease changed.
    DeliveryChanged {
        mode: super::held::DeliveryMode,
        lease_until_ms: Option<u64>,
        reason: super::held::DeliveryReason,
    },
    /// (S1) Held items changed state.
    HeldUpdated {
        ids: Vec<String>,
        state: super::held::HeldState,
    },
    /// A `Fork` made session `session_id` with `message_count`
    /// messages. Addressed to the client that asked (`None`: a host
    /// sender, every in-process client): the daemon sends it on that
    /// client's connection only, without the client field.
    Forked {
        client: Option<ClientId>,
        session_id: String,
        label: String,
        message_count: usize,
    },
    /// Client tools v1: a `register_client_tools` reply or notice, for
    /// `client` only (sent without the field, like `Forked`). Never
    /// replayed.
    ClientTools {
        client: Option<ClientId>,
        ack: Box<super::client_tools::ClientToolsAck>,
    },
    /// Client tools v1: one call for the installer only. Never replayed.
    ToolInvoke {
        client: Option<ClientId>,
        invoke: Box<super::client_tools::ToolInvokeFrame>,
    },
    /// Client tools v1: a pending call ended, for the installer only.
    ToolCancel {
        client: Option<ClientId>,
        cancel: super::client_tools::ToolCancelFrame,
    },
    /// Client tools v1: a `tool_return` reply, for its sender only.
    ToolReturnAck {
        client: Option<ClientId>,
        ack: super::client_tools::ToolReturnAckFrame,
    },
}

impl SessionEventWire {
    /// Whether the event goes out on the session's optional channel: events
    /// a client must opt in to, published apart so that their volume never
    /// evicts an ordinary event from the main channel's ring.
    pub fn is_optional(&self) -> bool {
        matches!(self, Self::ChildSpend(_))
    }
}

/// One paid request of the session's subagents (any depth), right after the
/// session booked it into its totals: the `child_spend` frame (draft 2,
/// frozen names). Display only: the snapshots' `cost`/`tokens` already
/// include it, and it is no request of the session's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildSpendEvent {
    /// The worker's handle ("sa_N"); a resume is a new handle.
    pub handle: String,
    /// The launch the request belongs to: 16 lowercase hex digits.
    pub run: String,
    /// 0, 1, 2… per (handle, run), one step per booked report.
    pub spend_seq: u64,
    /// The worker's agent label, bounded and flattened.
    pub agent: String,
    /// The worker that delegated it; `None` when the session's own turn did.
    #[serde(default)]
    pub parent_handle: Option<String>,
    /// What was paid for: "turn" or "summary" (an open set: a client maps
    /// an unknown kind to "other").
    pub kind: String,
    /// The qualified model id the request was priced at.
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// The TTL split of `cache_creation_input_tokens`; `None` when the
    /// route reports none.
    #[serde(default)]
    pub cache_creation_5m: Option<u64>,
    #[serde(default)]
    pub cache_creation_1h: Option<u64>,
    /// This request's price in USD, for display.
    pub cost_usd: f64,
    /// The same price in integer nanodollars, exactly as booked.
    pub cost_nanousd: u64,
    /// The session's inclusive total right after this booking.
    pub session_cost_usd: f64,
    /// The session's child share right after this booking.
    pub child_cost_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerChangeReason {
    Attach,
    Takeover,
    OwnerDetached,
    Reload,
    /// `client_input = shared`: this client sent input, so it is now the
    /// active one. Sent only to clients declaring
    /// `ClientCapabilities::owner_reason_input`: an older one never sees an
    /// owner change on a shared session (`wire::legacy_shared`).
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTrigger {
    User,
    QueuedAuto,
    EventAuto,
    PluginCommand,
    Compaction,
    /// (E-P0) Turn initiated by the session driver.
    DriverAuto,
}

/// Per-session `seq` assigned at the single emit site
/// (`SessionActor::emit`) from one counter for both of the session's
/// channels: strictly increasing on each, gapless across the two together
/// (the main channel alone has gaps where optional events went); `ts` from
/// the host clock.
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

/// Memo of `wire::messages_hash(api_messages)` shared by every clone of one
/// snapshot: the daemon converts a `Conversation` to its wire digest once
/// per socket client, and hashing serialises the whole history — now once
/// per event, not once per client. Keyed on the history's length and last
/// message identity (histories only grow), so a snapshot whose messages are
/// replaced afterwards recomputes instead of reusing a stale hash.
#[derive(Clone, Default)]
pub struct MessagesHashMemo(pub(crate) std::sync::Arc<std::sync::Mutex<Option<HashedHistory>>>);

/// `(len, last message, hash)` of the history a memo was computed for.
type HashedHistory = (usize, Option<crate::SharedMessage>, u64);

impl MessagesHashMemo {
    pub fn hash(&self, messages: &[crate::SharedMessage]) -> u64 {
        let mut memo = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((len, last, hash)) = memo.as_ref() {
            let same_last = match (last, messages.last()) {
                (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if *len == messages.len() && same_last {
                return *hash;
            }
        }
        let hash = super::wire::messages_hash(messages);
        *memo = Some((messages.len(), messages.last().cloned(), hash));
        hash
    }
}

impl std::fmt::Debug for MessagesHashMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MessagesHashMemo")
    }
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
    /// Prompt-cache keep-warm spend, already included in `cost` (task #444).
    #[serde(default)]
    pub keepwarm_cost: f64,
    /// Keep-warm ping tokens, already included in `tokens` (task #444).
    #[serde(default)]
    pub keepwarm_tokens: ConversationTokens,
    /// The session's subagents' spend (child spend, any depth), already
    /// included in `cost`; labelled separately, like the keep-warm share.
    #[serde(default)]
    pub child_cost: f64,
    /// The child share's tokens, already included in `tokens`.
    #[serde(default)]
    pub child_tokens: ConversationTokens,
    /// Whether the child share is exact: true for a session created in this
    /// process; false for the rest of a load that restored it (the share
    /// is snapshot-only: as of the last save, clamped under the totals).
    /// Absent (an older daemon) reads as false.
    #[serde(default)]
    pub child_share_exact: bool,
    /// Legacy (protocol v3 shape kept): always `None` — an interrupted turn
    /// is recorded in `api_messages` (`engine::interrupt`), never as a recap.
    pub abort_context: Option<String>,
    pub queued_message: Option<String>,
    pub pending_events_len: usize,
    pub consecutive_auto_turns: u32,
    /// Never serialised; see `MessagesHashMemo`.
    #[serde(skip)]
    pub messages_hash_memo: MessagesHashMemo,
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
    /// Every attached client with its resolved name (`clients` stays for
    /// older clients). Empty from an older daemon.
    pub roster: Vec<ClientInfo>,
    /// Who may drive ([`ClientInput`]); `Exclusive` from an older daemon.
    pub client_input: ClientInput,
    /// Who owns input right now (B1) — so a joiner knows immediately
    /// whether its keystrokes will be honoured.
    pub input_owner: Option<ClientId>,
    /// Daemon-projected display tail — `Some` iff the client attached with
    /// `HistoryMode::Digest` (`conversation.api_messages` is then empty).
    pub display_tail: Option<crate::session::display::DisplayTail>,
    /// The client tool stamps of the tool results in the conversation's
    /// history, by `tool_use_id` (empty when there are none).
    pub client_tools: ClientToolStamps,
}

/// Client tool stamps by `tool_use_id`: the side table a client is given
/// with a history (`Attached`, the `Messages` answer).
pub type ClientToolStamps = std::collections::BTreeMap<String, agent_core::ClientToolStamp>;

impl AttachSnapshot {
    /// For the client this snapshot was made for (`me`): who owns input, if
    /// someone else does — "input is owned by tui (#1); attach with
    /// --takeover to steal it". Rendered by each client from its OWN
    /// snapshot; the actor used to broadcast it as a `SystemNotice`, which
    /// every attached client (the owner included) and later attach replays
    /// showed.
    /// `None` with `client_input = shared`: every drive client may type.
    pub fn input_owned_elsewhere(&self, me: ClientId) -> Option<String> {
        if self.client_input == ClientInput::Shared {
            return None;
        }
        let owner = self.input_owner.filter(|o| *o != me)?;
        let who = Roster::from_snapshot(self).label(owner);
        Some(format!("input is owned by {who}; attach with --takeover to steal it"))
    }
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
    fn client_names_are_short_plain_labels() {
        for ok in ["tui", "discord", "Dash_2", "a", "9lives", &"x".repeat(CLIENT_NAME_MAX)] {
            assert_eq!(validate_client_name(ok), Ok(()), "{ok}");
        }
        for bad in ["", "-lead", "_lead", "has space", "dots.no", "émoji", &"x".repeat(CLIENT_NAME_MAX + 1)] {
            let err = validate_client_name(bad).unwrap_err();
            assert!(err.starts_with("invalid client name"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_session_resolves_names_to_unique_ones() {
        // Requested, else the kind's name; an invalid request falls back.
        assert_eq!(resolve_client_name(Some("discord"), ClientKind::Rpc, []), "discord");
        assert_eq!(resolve_client_name(None, ClientKind::Tui, []), "tui");
        assert_eq!(resolve_client_name(Some("no spaces"), ClientKind::Attach, []), "attach");
        // Taken (case-insensitively): lowest free suffix from 2.
        assert_eq!(resolve_client_name(None, ClientKind::Tui, ["tui"]), "tui-2");
        assert_eq!(resolve_client_name(Some("TUI"), ClientKind::Tui, ["tui", "tui-2"]), "TUI-3");
        assert_eq!(resolve_client_name(None, ClientKind::Tui, ["tui", "tui-3"]), "tui-2");
        // A freed name is reused.
        assert_eq!(resolve_client_name(None, ClientKind::Tui, ["tui-2"]), "tui");
    }

    #[test]
    fn a_roster_follows_joins_and_leaves_and_labels_by_name() {
        let joined = |id: u64, name: &str, mode: AttachMode| SessionEventWire::ClientJoined {
            client: ClientId(id),
            kind: ClientKind::Attach,
            name: name.to_string(),
            mode,
        };
        let mut r = Roster::default();
        r.apply(&joined(2, "dash", AttachMode::Observe));
        r.apply(&joined(1, "terminal", AttachMode::Mirror));
        // An older daemon's join has no name: the kind's is used.
        r.apply(&joined(3, "", AttachMode::Mirror));
        let ids: Vec<u64> = r.clients().iter().map(|c| c.id.0).collect();
        assert_eq!(ids, [1, 2, 3], "kept in id order");
        assert_eq!(r.get(ClientId(2)).unwrap().mode, AttachMode::Observe);
        assert_eq!(r.label(ClientId(1)), "terminal (#1)");
        assert_eq!(r.label(ClientId(3)), "attach (#3)");
        r.apply(&SessionEventWire::ClientLeft { client: ClientId(1) });
        assert_eq!(r.label(ClientId(1)), "client #1", "gone: id only");
        assert_eq!(r.clients().len(), 2);
    }

    /// prA I-R5 minor: line/paragraph separators and the Arabic letter mark
    /// are stripped with the other format characters; ZWNJ/ZWJ are kept so
    /// emoji sequences and scripts that need them render intact.
    #[test]
    fn sanitize_prompt_text_strips_separators_keeps_zwj_zwnj() {
        let input = "a\u{2028}b\u{2029}c\u{061C}d\u{200B}e\u{200E}f\u{200F}g\u{202E}h\u{2067}i\x1bj\tk\nl";
        assert_eq!(sanitize_prompt_text(input), "abcdefghij k\nl");
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(sanitize_prompt_text(family), family, "ZWJ emoji intact");
        let persian = "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}";
        assert_eq!(sanitize_prompt_text(persian), persian, "ZWNJ intact");
    }

    #[test]
    fn prompt_kind_from_title() {
        assert_eq!(PromptKind::from_title("Confirm tool call"), PromptKind::Confirm);
        assert_eq!(
            PromptKind::from_title("Confirm tool activation"),
            PromptKind::Confirm
        );
        assert_eq!(PromptKind::from_title("API key"), PromptKind::Secret);
    }

    /// `tool_call_id` is additive on the wire. A scope from an older daemon
    /// (no key) reads as `None`; `None` writes no key, so an older client
    /// sees the shape it knew; `Some` round-trips.
    #[test]
    fn confirm_scope_tool_call_id_is_additive_on_the_wire() {
        let old = r#"{"extensions":["jev"],"tool":"bash"}"#;
        let read: ConfirmScope = serde_json::from_str(old).unwrap();
        assert_eq!(read.tool_call_id, None);
        assert_eq!(serde_json::to_string(&read).unwrap(), old, "None writes no key");
        let with_id = ConfirmScope {
            tool_call_id: Some("toolu_01".into()),
            ..read
        };
        let wire = serde_json::to_string(&with_id).unwrap();
        assert_eq!(
            wire,
            r#"{"extensions":["jev"],"tool":"bash","tool_call_id":"toolu_01"}"#
        );
        assert_eq!(serde_json::from_str::<ConfirmScope>(&wire).unwrap(), with_id);
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
    fn session_config_env_defaults_to_none() {
        let cfg: SessionConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.env.is_none());
    }

    #[test]
    fn session_config_env_round_trips() {
        let env = vec![("FOO".into(), "bar".into()), ("PATH".into(), "/usr/bin".into())];
        let cfg = SessionConfig { env: Some(env.clone()), ..Default::default() };
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: SessionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2.env, Some(env));
    }

    #[test]
    fn should_strip_env_client_prefixes() {
        assert!(should_strip_env("SYNAPS_CLIENT_REEXEC"));
        assert!(should_strip_env("SYNAPS_TUI_SCROLLBACK"));
        assert!(should_strip_env("SYNAPS_DAEMON_ADOPT"));
        assert!(should_strip_env("SYNAPS_MEM_TRACE"));
        assert!(should_strip_env("SYNAPS_MEM_TRACE_FOO"));
        // Session env — not stripped
        assert!(!should_strip_env("SYNAPS_ANTHROPIC_BASE_URL"));
        assert!(!should_strip_env("HOME"));
        assert!(!should_strip_env("PATH"));
        assert!(!should_strip_env("VIRTUAL_ENV"));
    }

    #[test]
    fn should_strip_env_secrets() {
        assert!(should_strip_env("ANTHROPIC_API_KEY"));
        assert!(should_strip_env("OPENAI_API_KEY"));
        assert!(should_strip_env("my_secret_token"));
        assert!(should_strip_env("AWS_SECRET_ACCESS_KEY"));
        assert!(should_strip_env("DB_PASSWORD"));
        assert!(should_strip_env("GCP_CREDENTIALS"));
        // Case insensitive
        assert!(should_strip_env("My_Api_Key"));
        // Not a secret
        assert!(!should_strip_env("MY_VALUE"));
        assert!(!should_strip_env("TOKENBUCKET_SIZE"));
        // suffix-anchored: the secret name must END the key
        assert!(!should_strip_env("API_TOKEN_TTL_SECS"));
        assert!(!should_strip_env("KEYBOARD_LAYOUT"));
        assert!(!should_strip_env("SSH_KEY_PATH"));
        assert!(should_strip_env("GH_TOKEN"));
        assert!(should_strip_env("SSH_KEY"));
        assert!(should_strip_env("SECRET_KEY_BASE"));
        assert!(should_strip_env("MY_PASSWD"));
    }

    #[test]
    fn strip_client_env_drops_credential_values_whatever_the_key() {
        let vars = vec![
            ("HOME".to_string(), "/home/u".to_string()),
            (
                "DATABASE_URL".to_string(),
                "postgres://app:hunter2@db/prod".to_string(),
            ),
            (
                "HTTPS_PROXY".to_string(),
                "http://user:pass@proxy:3128".to_string(),
            ),
            (
                "SENTRY_DSN".to_string(),
                "https://abc123@o1.ingest.sentry.io/42".to_string(),
            ),
            ("GITHUB_PAT".to_string(), "ghp_x".to_string()),
            (
                "API_BASE_URL".to_string(),
                "https://api.example.com/v1".to_string(),
            ),
        ];
        let (env, stripped) = strip_client_env(vars);
        let kept: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kept,
            vec!["API_BASE_URL", "HOME"],
            "only non-secrets survive"
        );
        assert!(env.iter().all(|(_, v)| !v.contains("hunter2")
            && !v.contains("user:pass")
            && !v.contains("abc123")));
        assert_eq!(
            stripped,
            vec!["DATABASE_URL", "GITHUB_PAT", "HTTPS_PROXY", "SENTRY_DSN"],
            "stripped names are recorded"
        );
    }

    #[test]
    fn capture_client_env_is_sorted_and_stripped() {
        // We can't fully control the process env in a unit test, but we can
        // verify the output is sorted and that known strip-list vars are absent.
        let (env, stripped) = capture_client_env();
        // Every stripped name is a secret by our own rule, and none of them
        // survived into the kept env.
        for name in &stripped {
            let by_value = std::env::var(name)
                .map(|v| agent_core::core::config::is_secret_env_value(&v))
                .unwrap_or(false);
            assert!(
                is_secret_key(name) || by_value,
                "stripped non-secret: {name}"
            );
            assert!(
                env.iter().all(|(k, _)| k != name),
                "stripped name leaked: {name}"
            );
        }
        // Sorted
        for w in env.windows(2) {
            assert!(w[0].0 <= w[1].0, "not sorted: {:?} > {:?}", w[0].0, w[1].0);
        }
        // No secrets or client-only vars
        for (k, v) in &env {
            assert!(!should_strip_env(k), "should have been stripped: {k}");
            assert!(
                !agent_core::core::config::is_secret_env_value(v),
                "credential value kept: {k}"
            );
        }
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

        // Submit with content blocks round-trips.
        let blocks = vec![
            serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}}),
            serde_json::json!({"type":"text","text":"hello"}),
        ];
        let submit_with = SessionCommand::Submit {
            text: "describe".into(),
            attachments: blocks.clone(),
        };
        let json = serde_json::to_string(&submit_with).unwrap();
        let back: SessionCommand = serde_json::from_str(&json).unwrap();
        match back {
            SessionCommand::Submit { text, attachments } => {
                assert_eq!(text, "describe");
                assert_eq!(attachments.len(), 2);
                assert_eq!(attachments[0]["type"], "image");
                assert_eq!(attachments[1]["type"], "text");
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
            SessionCommand::Checkpoint {
                reason: CheckpointReason::Reload,
                deadline: None,
                skip_quiesce_grace: false,
            },
            SessionCommand::KeepWarm { on: true },
            SessionCommand::Query { id: 3, query: SessionQuery::ContextReport },
            SessionCommand::Query {
                id: 4,
                query: SessionQuery::SubagentWatch { handle_id: "sa_2".into(), since: 7, tool_queue: true },
            },
            SessionCommand::SubagentSteer { handle_id: "sa_2".into(), text: "focus on X".into() },
            SessionCommand::DriverStart { plugin: "p".into(), command: "auto".into(), arg: "start -- hi".into() },
            SessionCommand::TakeInput,
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: SessionCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
        let take: SessionCommand = serde_json::from_str(r#"{"cmd":"take_input"}"#).unwrap();
        assert!(matches!(take, SessionCommand::TakeInput));
        let cp: SessionCommand = serde_json::from_str(r#"{"cmd":"checkpoint","reason":"host_request"}"#).unwrap();
        assert!(matches!(
            cp,
            SessionCommand::Checkpoint {
                reason: CheckpointReason::HostRequest,
                deadline: None,
                skip_quiesce_grace: false,
            }
        ));
        // The deadline and the grace flag are host-only: never written,
        // never read (a wire checkpoint keeps the grace).
        let with = SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: Some(tokio::time::Instant::now()),
            skip_quiesce_grace: true,
        };
        assert_eq!(
            serde_json::to_string(&with).unwrap(),
            r#"{"cmd":"checkpoint","reason":"reload"}"#
        );
        let read: SessionCommand = serde_json::from_str(
            r#"{"cmd":"checkpoint","reason":"reload","deadline":5,"skip_quiesce_grace":true}"#,
        )
        .unwrap();
        assert!(matches!(
            read,
            SessionCommand::Checkpoint { deadline: None, skip_quiesce_grace: false, .. }
        ));
    }

    #[test]
    fn command_debug_redacts_bodies_without_lengths() {
        let secret = "hunter2-very-secret";
        let cmds = vec![
            SessionCommand::Submit { text: secret.into(), attachments: vec![] },
            SessionCommand::Steer { text: secret.into() },
            SessionCommand::SubagentSteer { handle_id: "sa_1".into(), text: secret.into() },
            SessionCommand::SubmitPrepared {
                messages: vec![std::sync::Arc::new(serde_json::json!({"content": secret}))],
                user_text: Some(secret.into()),
            },
            SessionCommand::Answer { prompt_id: 1, value: Some(secret.into()) },
            SessionCommand::Resume { id: 1, query: secret.into() },
            SessionCommand::DriverStart { plugin: "p".into(), command: "auto".into(), arg: secret.into() },
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

        // Submit debug shows count+kinds, never base64.
        let b64_secret = "aHVudGVyMi12ZXJ5LXNlY3JldA==";
        let cmd = SessionCommand::Submit {
            text: "x".into(),
            attachments: vec![
                serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":b64_secret}}),
                serde_json::json!({"type":"document","title":"x.pdf","source":{"type":"base64","media_type":"application/pdf","data":"JVBER"}}),
            ],
        };
        let d = format!("{cmd:?}");
        assert!(d.contains(r#"2:["image", "document"]"#), "{d}");
        assert!(!d.contains(b64_secret), "base64 leaked: {d}");
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

/// `Option<Option<String>>` on the wire without collapsing `Some(None)`:
/// absent → `None`; `{"latest": true}` → `Some(None)`; `"id"` → `Some(Some(id))`.
/// Older daemons/clients that sent a bare string or `null` still decode.
mod double_option {
    use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Latest { latest: bool },
        Id(String),
    }

    pub fn serialize<S: Serializer>(v: &Option<Option<String>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(None) => Wire::Latest { latest: true }.serialize(s),
            Some(Some(id)) => Wire::Id(id.clone()).serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        match Option::<Wire>::deserialize(d)? {
            None => Ok(None),
            Some(Wire::Latest { latest: true }) => Ok(Some(None)),
            Some(Wire::Latest { latest: false }) => Err(de::Error::custom("continue_session: latest must be true")),
            Some(Wire::Id(id)) => Ok(Some(Some(id))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::SessionConfig;

        fn rt(v: Option<Option<String>>) -> Option<Option<String>> {
            let cfg = SessionConfig { continue_session: v, ..Default::default() };
            let j = serde_json::to_string(&cfg).unwrap();
            serde_json::from_str::<SessionConfig>(&j).unwrap().continue_session
        }

        #[test]
        fn continue_session_round_trips_all_three_states() {
            assert_eq!(rt(None), None);
            assert_eq!(rt(Some(None)), Some(None), "bare --continue must survive the wire");
            assert_eq!(rt(Some(Some("abc".into()))), Some(Some("abc".into())));
        }

        #[test]
        fn continue_session_accepts_legacy_encodings() {
            let old_null: SessionConfig = serde_json::from_str(r#"{"continue_session":null}"#).unwrap();
            assert_eq!(old_null.continue_session, None);
            let old_id: SessionConfig = serde_json::from_str(r#"{"continue_session":"xyz"}"#).unwrap();
            assert_eq!(old_id.continue_session, Some(Some("xyz".into())));
            let absent: SessionConfig = serde_json::from_str(r#"{}"#).unwrap();
            assert_eq!(absent.continue_session, None);
        }
    }
}

/// Where a daemon session writes its pin (`daemon::pins::PinBook`). The
/// actor calls it exactly when the pin changes, when a pinned session moves
/// to a new journal or is renamed, and when a pinned session is ended on
/// purpose. Implementations do their own I/O and never fail the caller.
pub trait PinSink: Send + Sync {
    /// The session is pinned: add or refresh its entry (`journal_id` is
    /// current; `id` is the actor id, which may name an older journal).
    /// `cache_keepwarm`: the session's `/keepwarm` override (`off`/`auto`/`<N>h`).
    fn pinned(&self, id: &str, journal_id: &str, name: Option<&str>, cwd: Option<&std::path::Path>, cache_keepwarm: Option<&str>);
    /// Unpinned, or ended on purpose while pinned: drop its entry.
    fn unpinned(&self, id: &str, journal_id: &str);
    /// A pinned session's journal changed (`/clear`, a successor compaction,
    /// resume): the entry follows it.
    fn moved(&self, from: &str, to: &str);
    /// A pinned session was renamed (`/saveas`, `daemon rename`): its entry
    /// takes `name` verbatim, so a cleared name stops resolving.
    fn renamed(&self, journal_id: &str, name: Option<&str>);
}
