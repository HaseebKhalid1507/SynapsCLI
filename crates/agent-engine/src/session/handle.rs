//! `SessionHandle` — the in-process address of a session (PLAN-phase2 §2.6).
//!
//! `EngineHost::create_session` (A1) hands one out per session; every
//! `ClientTransport` is built on it. `echo_for_test` ships a stand-in actor
//! so package B can build the wire before the real `SessionActor` exists.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use super::transport::TransportError;
use super::types::{Addressed, ClientId, Envelope, SessionCommand, SessionConfig, SessionId, SessionLifecycle, SessionMeta};

/// Live client accounting published by the actor (B4 `sessions()` listing).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Presence {
    pub clients: usize,
    pub input_owner: Option<ClientId>,
    pub awaiting_input: usize,
    /// Every attached client, named (`synaps daemon sessions`).
    pub roster: Vec<super::types::ClientInfo>,
    /// Who may drive (`None` until the actor publishes).
    pub client_input: Option<super::types::ClientInput>,
    /// A turn or a compaction is running.
    pub busy: bool,
    /// What `state_since` dates (idle / busy / parked): a publish that
    /// changes it restamps `state_since`.
    pub state: PresenceState,
    pub state_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock idle park (Live, zero clients, grace armed).
    pub park_due: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock eviction of a Parked session.
    pub evict_due: Option<chrono::DateTime<chrono::Utc>>,
    /// Conversation title / cost / api message count. Kept as last seen
    /// while Parked (no conversation in memory).
    pub title: Option<String>,
    pub cost_usd: f64,
    pub messages: usize,
    /// Owned sessions: the client bound as the owner (`None` while the
    /// session waits for its owner to bind or re-bind). Always `None` for a
    /// session that is not owned (`SessionHandle::is_owned`).
    pub owning_client: Option<ClientId>,
    // PR 3: `saved: daemon::saved::SavedFacts` (the actor driver), with
    // `chain_facts` walked at create from the `parent_session` headers, so
    // the hub can retire a session as the row `CatalogPage` will list.
}

/// The coarse state `Presence::state_since` dates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PresenceState {
    #[default]
    Idle,
    Busy,
    Parked,
}
use super::view::RuntimeView;

/// How many superseded journal ids a handle remembers (oldest dropped).
pub const SUPERSEDES_CAP: usize = 32;

/// The conversation an actor holds. One cell, so a reader sees the journal
/// id, what it superseded and when it started together.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalCell {
    /// `conv.session.id`.
    pub journal_id: String,
    /// The journal ids this actor compacted away from since it last began a
    /// conversation (create, resume or `/clear`), oldest first. A successor
    /// may not be on disk yet, so the disk chain can still end at one of
    /// these: the actor holds them all ([`SessionHandle::holds`]).
    pub supersedes: Vec<String>,
    /// When the conversation began: kept across compactions (chain start),
    /// replaced when the actor begins another conversation.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Why an actor's journal id changes ([`JournalCell::moved`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalMove {
    /// A linked-successor compaction: the same conversation, a new journal.
    Compaction,
    /// `/clear`: a new conversation; the old one is no longer held here.
    Clear,
    /// In-session `/resume`: another conversation; the old one is let go.
    Resume,
}

impl JournalCell {
    pub fn new(journal_id: String, created_at: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            journal_id,
            supersedes: Vec::new(),
            created_at,
        }
    }

    /// The cell after a move to `new_id`. A compaction keeps `created_at`
    /// and appends the old id (unless it is the same journal: an in-place
    /// compaction); a clear or resume starts over at `created_at`, unless
    /// it lands on the journal already held (`/resume` of the current
    /// session reloads it from disk: the same conversation, cell kept).
    pub fn moved(
        &self,
        new_id: String,
        why: JournalMove,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        match why {
            JournalMove::Compaction => {
                let mut supersedes = self.supersedes.clone();
                if self.journal_id != new_id {
                    supersedes.retain(|j| *j != self.journal_id && *j != new_id);
                    supersedes.push(self.journal_id.clone());
                    let over = supersedes.len().saturating_sub(SUPERSEDES_CAP);
                    supersedes.drain(..over);
                }
                Self {
                    journal_id: new_id,
                    supersedes,
                    created_at: self.created_at,
                }
            }
            JournalMove::Clear | JournalMove::Resume if new_id == self.journal_id => self.clone(),
            JournalMove::Clear | JournalMove::Resume => Self::new(new_id, created_at),
        }
    }

    /// This conversation: the current journal or one it superseded.
    pub fn holds(&self, conversation: &str) -> bool {
        self.journal_id == conversation || self.supersedes.iter().any(|j| j == conversation)
    }
}

/// The actor's end of [`JournalCell`].
pub type JournalSlot = Arc<arc_swap::ArcSwap<JournalCell>>;

/// Bounded command queue depth (rpc `WRITER_CHAN_CAP` precedent).
pub const CMD_CHAN_CAP: usize = 256;
/// Default broadcast capacity; `SYNAPS_SESSION_EVENTS_CAP` overrides.
pub const DEFAULT_EVENTS_CAP: usize = 1024;

/// Broadcast capacity from `SYNAPS_SESSION_EVENTS_CAP` (min 16) or the default.
pub fn events_cap() -> usize {
    std::env::var("SYNAPS_SESSION_EVENTS_CAP")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.max(16))
        .unwrap_or(DEFAULT_EVENTS_CAP)
}

#[derive(Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    cmd_tx: mpsc::Sender<Addressed>,
    events: broadcast::Sender<Envelope>,
    meta: Arc<SessionMeta>,
    /// Sync, lock-free read of the last published RuntimeView (§2.7).
    view: Arc<arc_swap::ArcSwap<RuntimeView>>,
    /// Written by the actor (Live/Parking/Parked/Ending).
    lifecycle: Arc<AtomicU8>,
    /// `conv.session.id` and what it superseded — changes on a
    /// LinkedSuccessor compaction, `/clear` and an in-session `/resume`.
    journal: JournalSlot,
    /// Clients / input owner / pending prompts, written by the actor.
    presence: Arc<arc_swap::ArcSwap<Presence>>,
    /// The workers that hold the park (`SessionMeta::workers`), written by
    /// the actor's `publish_workers` only when the block changes. Its own
    /// cell: a worker change does not rebuild the presence.
    workers: Arc<arc_swap::ArcSwap<super::types::SessionWorkers>>,
    /// Session name — `--name` at create / `/cmd saveas` later. Written by
    /// the actor so listings and `--continue <name>` see renames live.
    name: Arc<arc_swap::ArcSwap<Option<String>>>,
    /// The pin (`KeepWarm{on}`), written by the actor. Read without a probe
    /// by listings and by the daemon's idle exit, which a pin holds.
    keep_warm: Arc<AtomicBool>,
    /// Per-session `/keepwarm` override (task #444), written by the actor.
    /// Lives here so a daemon reload whose checkpoint did not confirm still
    /// carries the user's explicit choice.
    cache_keepwarm: super::cache_keepwarm::OverrideSlot,
    /// F23: only on a lock-held placeholder — the config to retry the real
    /// `create` with on the next attach. `None` for real actors.
    placeholder_config: Option<Arc<SessionConfig>>,
    /// F23: how many real `create`s of this placeholder are running
    /// ([`RetryGuard`]). While any is, it stays listed (no flicker) but is
    /// not a resume target nor an id holder until the real session
    /// replaces it. A count, not a flag: one retry ending never clears
    /// another's.
    retrying: Arc<AtomicUsize>,
    /// Owned sessions only: the owner token, fixed at create. Here so a
    /// reload whose checkpoint did not confirm still records the session
    /// as owned (it would otherwise come back unowned and never end).
    owner_token: Option<super::types::OwnerToken>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("model", &self.meta.model)
            .finish()
    }
}

/// One running retry of a placeholder ([`SessionHandle::retry_guard`]).
#[must_use = "the retry ends when the guard drops"]
pub(crate) struct RetryGuard(Arc<AtomicUsize>);

impl Drop for RetryGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A session's identity without its command sender
/// ([`SessionHandle::session_ref`]): holding it never keeps the actor alive.
#[derive(Clone)]
pub struct SessionRef {
    cmd_tx: mpsc::WeakSender<Addressed>,
}

impl std::fmt::Debug for SessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionRef")
    }
}

/// Actor-side ends of a session's channels, minted with the handle.
pub struct SessionEndpoints {
    pub cmd_rx: mpsc::Receiver<Addressed>,
    pub events: broadcast::Sender<Envelope>,
    pub view: Arc<arc_swap::ArcSwap<RuntimeView>>,
    pub lifecycle: Arc<AtomicU8>,
    pub journal: JournalSlot,
    pub presence: Arc<arc_swap::ArcSwap<Presence>>,
    pub workers: Arc<arc_swap::ArcSwap<super::types::SessionWorkers>>,
    pub name: Arc<arc_swap::ArcSwap<Option<String>>>,
    pub keep_warm: Arc<AtomicBool>,
    pub cache_keepwarm: super::cache_keepwarm::OverrideSlot,
}

impl SessionHandle {
    /// Mint a handle + the actor's ends. Used by `SessionActor` (A1) and the
    /// test `EchoActor`.
    pub fn new(meta: SessionMeta, view: RuntimeView) -> (Self, SessionEndpoints) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHAN_CAP);
        let (events, _) = broadcast::channel(events_cap());
        let view = Arc::new(arc_swap::ArcSwap::from_pointee(view));
        let lifecycle = Arc::new(AtomicU8::new(SessionLifecycle::Live as u8));
        let journal = Arc::new(arc_swap::ArcSwap::from_pointee(JournalCell::new(
            meta.journal_id.clone(),
            meta.created_at,
        )));
        let presence = Arc::new(arc_swap::ArcSwap::from_pointee(Presence::default()));
        let workers = Arc::new(arc_swap::ArcSwap::from_pointee(
            super::types::SessionWorkers::default(),
        ));
        let name = Arc::new(arc_swap::ArcSwap::from_pointee(meta.name.clone()));
        let keep_warm = Arc::new(AtomicBool::new(meta.pinned));
        let cache_keepwarm = super::cache_keepwarm::OverrideSlot::default();
        let handle = Self {
            id: meta.id.clone(),
            cmd_tx,
            events: events.clone(),
            meta: Arc::new(meta),
            view: Arc::clone(&view),
            lifecycle: Arc::clone(&lifecycle),
            journal: Arc::clone(&journal),
            presence: Arc::clone(&presence),
            workers: Arc::clone(&workers),
            name: Arc::clone(&name),
            keep_warm: Arc::clone(&keep_warm),
            cache_keepwarm: Arc::clone(&cache_keepwarm),
            placeholder_config: None,
            retrying: Arc::new(AtomicUsize::new(0)),
            owner_token: None,
        };
        (
            handle,
            SessionEndpoints {
                cmd_rx,
                events,
                view,
                lifecycle,
                journal,
                presence,
                workers,
                name,
                keep_warm,
                cache_keepwarm,
            },
        )
    }

    /// Send a host-originated command (`from: None`). `Backpressure` when
    /// the queue is full (never drop silently); `Closed` once the actor is
    /// gone. A trusted in-process API (the daemon's idle probe, reload,
    /// close, rename, pins): it bypasses input ownership, and its replies
    /// are the host's (`Audience::Host`), so the caller reads them on its
    /// own `subscribe()` receiver, never through an attachment. A client
    /// sends with [`Self::send_from`] (the daemon stamps every frame,
    /// `LocalTransport` its attachment).
    pub async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.send_addressed(Addressed { from: None, cmd })
    }

    /// Send on behalf of an attached client (ownership is enforced by the
    /// actor, B1).
    pub async fn send_from(&self, from: ClientId, cmd: SessionCommand) -> Result<(), TransportError> {
        self.send_addressed(Addressed { from: Some(from), cmd })
    }

    pub(crate) fn send_addressed(&self, cmd: Addressed) -> Result<(), TransportError> {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
        }
    }

    /// Park/unpark state as last written by the actor.
    pub fn lifecycle(&self) -> SessionLifecycle {
        SessionLifecycle::from_u8(self.lifecycle.load(Ordering::Acquire))
    }

    /// Current `conv.session.id` (= `id` until a LinkedSuccessor compaction,
    /// a `/clear` or a resume, unless the actor took a minted id).
    pub fn journal_id(&self) -> String {
        self.journal.load().journal_id.clone()
    }

    /// The conversation cell as last written by the actor.
    pub fn journal(&self) -> Arc<JournalCell> {
        self.journal.load_full()
    }

    /// Whether this actor holds `conversation` (a journal id): its current
    /// journal or one it compacted away from. The one test for "is this
    /// conversation running here"; never the actor id, which a `/clear`
    /// leaves on a different conversation.
    pub fn holds(&self, conversation: &str) -> bool {
        self.journal.load().holds(conversation)
    }

    /// This session's identity, which (unlike a handle) does not keep its
    /// command queue open.
    pub fn session_ref(&self) -> SessionRef {
        SessionRef {
            cmd_tx: self.cmd_tx.downgrade(),
        }
    }

    /// Whether `which` is this very session (same command channel), not
    /// merely one registered under the same id.
    pub fn is_session(&self, which: &SessionRef) -> bool {
        // No strong sender left: no handle of that session exists, so this
        // one is not it.
        which
            .cmd_tx
            .upgrade()
            .is_some_and(|tx| tx.same_channel(&self.cmd_tx))
    }

    /// F23: a placeholder whose real `create` is running.
    pub fn retrying(&self) -> bool {
        self.retrying.load(Ordering::Acquire) > 0
    }

    /// F23: mark one real `create` of this placeholder as running until the
    /// guard drops (also when the retrying future is dropped mid-await).
    pub(crate) fn retry_guard(&self) -> RetryGuard {
        self.retrying.fetch_add(1, Ordering::AcqRel);
        RetryGuard(Arc::clone(&self.retrying))
    }

    /// Test seam: write the conversation cell as an actor would.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_journal_for_test(&self, cell: JournalCell) {
        self.journal.store(Arc::new(cell));
    }

    /// Clients / input owner / pending prompts as last published.
    pub fn presence(&self) -> Presence {
        (**self.presence.load()).clone()
    }

    /// Current session name (create `--name` or a later `saveas`).
    pub fn name(&self) -> Option<String> {
        (**self.name.load()).clone()
    }

    /// Test seam: the workers cell as stored (its `Arc` changes only when
    /// the actor publishes a different block).
    #[cfg(any(test, feature = "testing"))]
    pub fn workers_cell_for_test(&self) -> Arc<super::types::SessionWorkers> {
        self.workers.load_full()
    }

    /// Whether the session is pinned (`KeepWarm{on}`), as last written by
    /// the actor.
    pub fn pinned(&self) -> bool {
        self.keep_warm.load(Ordering::Acquire)
    }

    /// The session's `/keepwarm` override (`off`/`auto`/`<N>h`), if any.
    pub fn cache_keepwarm_override(&self) -> Option<String> {
        self.cache_keepwarm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|m| m.to_string())
    }

    /// `meta()` with the live cells filled in (lifecycle, clients, owner,
    /// awaiting_input, client_input, model, journal_id, name, pinned, the
    /// presence's busy/state/deadlines/title/cost/messages, and the
    /// workers) — what listings show.
    pub fn meta_live(&self) -> SessionMeta {
        let mut m = (*self.meta).clone();
        let p = self.presence();
        m.lifecycle = self.lifecycle();
        m.clients = p.clients;
        m.input_owner = p.input_owner;
        m.awaiting_input = p.awaiting_input;
        m.roster = p.roster;
        m.client_input = p.client_input;
        // `/model` from any client changes it; the create-time meta doesn't.
        m.model = self.view.load().model.clone();
        let journal = self.journal();
        m.journal_id = journal.journal_id.clone();
        m.created_at = journal.created_at;
        m.name = self.name();
        m.pinned = self.pinned();
        m.busy = p.busy;
        m.state_since = p.state_since;
        m.park_due = p.park_due;
        m.evict_due = p.evict_due;
        m.title = p.title;
        m.cost_usd = p.cost_usd;
        m.messages = p.messages;
        m.workers = (**self.workers.load()).clone();
        m
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.events.subscribe()
    }

    /// F23: the retry config carried by a lock-held placeholder.
    pub fn placeholder_config(&self) -> Option<&SessionConfig> {
        self.placeholder_config.as_deref()
    }

    pub(crate) fn with_placeholder_config(mut self, cfg: SessionConfig) -> Self {
        self.placeholder_config = Some(Arc::new(cfg));
        self
    }

    pub(crate) fn with_owner_token(mut self, token: Option<super::types::OwnerToken>) -> Self {
        self.owner_token = token;
        self
    }

    /// Whether this is an owned session (bound to its creating connection).
    pub fn is_owned(&self) -> bool {
        self.owner_token.is_some()
    }

    /// The owner token of an owned session (daemon-internal: the conn hands
    /// it to the owner only, the reload record carries it).
    pub fn owner_token(&self) -> Option<&super::types::OwnerToken> {
        self.owner_token.as_ref()
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn view(&self) -> arc_swap::Guard<Arc<RuntimeView>> {
        self.view.load()
    }

    /// Whether the actor is still alive (its command receiver exists).
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// Resolves once the actor is gone (command receiver dropped).
    pub async fn closed(&self) {
        self.cmd_tx.closed().await
    }

    /// EchoActor: `Submit{text}` → `TurnStarted`, `Stream(Text(text))`,
    /// `Stream(Done)`, `Conversation` — for B's socket tests before A1 lands.
    #[cfg(any(test, feature = "testing"))]
    pub fn echo_for_test(id: SessionId) -> (Self, tokio::task::JoinHandle<()>) {
        echo::spawn(id)
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod echo {
    //! Minimal stand-in actor. Honours the envelope/seq contract, `Attach`,
    //! `Detach`, `Submit`, `Steer`, `Cancel`, `Query{View}`, `End`;
    //! everything else is acknowledged with a `SystemNotice`.

    use std::collections::HashMap;

    use agent_core::reasoning::ReasoningLevel;

    use super::*;
    use crate::session::types::*;
    use crate::{LlmEvent, SessionEvent, StreamEvent};

    pub fn meta_for(id: &SessionId) -> SessionMeta {
        SessionMeta {
            id: id.clone(),
            name: None,
            model: "echo".into(),
            cwd: None,
            created_at: chrono::Utc::now(),
            continued: false,
            continue_info: None,
            host_pid: std::process::id(),
            lifecycle: SessionLifecycle::Live,
            clients: 0,
            input_owner: None,
            awaiting_input: 0,
            roster: Vec::new(),
            client_input: None,
            journal_id: id.0.clone(),
            locked_by: None,
            pinned: false,
            busy: false,
            state_since: None,
            park_due: None,
            evict_due: None,
            title: None,
            cost_usd: 0.0,
            messages: 0,
            workers: Default::default(),
            updated_at: None,
        }
    }

    pub fn view_for() -> RuntimeView {
        RuntimeView {
            model: "echo".into(),
            thinking_level: "off".into(),
            reasoning_level: ReasoningLevel::Off,
            is_reasoning_explicit: false,
            thinking_budget: 0,
            context_window: 200_000,
            system_prompt: None,
            compaction_model: "echo".into(),
            api_retries: 0,
            subagent_timeout: 0,
            max_tool_output: 4096,
            bash_timeout: 30,
            bash_max_timeout: 300,
            prompt_generation: 0,
            hook_handler_count: 0,
            prompt_inspection: None,
        }
    }

    pub fn spawn(id: SessionId) -> (SessionHandle, tokio::task::JoinHandle<()>) {
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        let task = tokio::spawn(run(id, ep));
        (handle, task)
    }

    struct Echo {
        id: SessionId,
        events: broadcast::Sender<Envelope>,
        view: Arc<arc_swap::ArcSwap<RuntimeView>>,
        seq: u64,
        next_client: u64,
        clients: HashMap<ClientId, ClientMeta>,
        conv: ConversationSnapshot,
    }

    impl Echo {
        fn emit(&mut self, event: SessionEventWire) {
            self.emit_to(Audience::All, event);
        }

        /// Addressed like the real actor's replies (`SessionActor::reply`,
        /// `emit_shared`).
        fn emit_to(&mut self, audience: Audience, event: SessionEventWire) {
            let env = Envelope::new(self.id.clone(), self.seq, chrono::Utc::now(), event).to(audience);
            self.seq += 1;
            // No receivers is not an error: streams are not tied to clients.
            let _ = self.events.send(env);
        }

        fn conv_snapshot(&self) -> ConversationSnapshot {
            let mut c = self.conv.clone();
            c.messages_len = c.api_messages.len();
            c
        }

        /// Mirrors `SessionActor::snapshot_for`: Digest clients get the
        /// tail, not the history.
        fn snapshot_for(&self, client: &ClientMeta) -> AttachSnapshot {
            let mut conversation = self.conv_snapshot();
            let display_tail = (client.history == HistoryMode::Digest).then(|| {
                let t = crate::session::display::display_tail(&conversation.api_messages, client.tail_items);
                conversation.api_messages = Vec::new();
                t
            });
            AttachSnapshot {
                meta: meta_for(&self.id),
                view: (**self.view.load()).clone(),
                conversation,
                streaming: false,
                replay: Vec::new(),
                pending_prompts: Vec::new(),
                clients: self.clients.iter().map(|(c, m)| (*c, m.kind)).collect(),
                roster: Vec::new(),
                client_input: ClientInput::wire_default(),
                input_owner: None,
                display_tail,
            }
        }

        fn handle(&mut self, from: Option<ClientId>, cmd: SessionCommand) -> bool {
            match cmd {
                SessionCommand::Attach { client, mode, ticket, .. } => {
                    let cid = ClientId(self.next_client);
                    let Some(to) = attach_audience(ticket.as_ref(), Some(cid)) else { return true };
                    self.next_client += 1;
                    let kind = client.kind;
                    let snapshot_meta = client.clone();
                    self.clients.insert(cid, client);
                    if mode != AttachMode::Mirror {
                        self.emit(SessionEventWire::SystemNotice(format!(
                            "attach mode {mode:?} not supported yet; using mirror"
                        )));
                    }
                    let snapshot = self.snapshot_for(&snapshot_meta);
                    self.emit_to(to.unwrap_or(Audience::Client(cid)), SessionEventWire::Attached {
                        client: cid,
                        snapshot,
                    });
                    if let Some(t) = &ticket {
                        t.sent();
                    }
                    let name = kind.default_name().to_string();
                    self.emit(SessionEventWire::ClientJoined { client: cid, kind, name, mode: AttachMode::Mirror });
                }
                SessionCommand::Detach { client } => {
                    if self.clients.remove(&client).is_some() {
                        self.emit(SessionEventWire::ClientLeft { client });
                    }
                }
                SessionCommand::Submit { text, .. } => {
                    let baseline = self.conv.api_messages.len();
                    self.conv.api_messages.push(Arc::new(serde_json::json!({
                        "role": "user", "content": text
                    })));
                    self.emit(SessionEventWire::TurnStarted {
                        turn_baseline: baseline,
                        trigger: TurnTrigger::User,
                        user_text: None,
                    });
                    self.emit(SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(
                        text.clone(),
                    ))));
                    self.conv.api_messages.push(Arc::new(serde_json::json!({
                        "role": "assistant", "content": text
                    })));
                    self.emit(SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::MessageHistory(self.conv.api_messages.clone()),
                    )));
                    self.emit(SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::Done,
                    )));
                    let snap = self.conv_snapshot();
                    self.emit(SessionEventWire::Conversation(snap));
                }
                SessionCommand::Steer { text } => {
                    self.conv.queued_message = Some(text.clone());
                    self.emit(SessionEventWire::Steered {
                        text,
                        delivered: false,
                    });
                }
                SessionCommand::Cancel => {
                    let snap = self.conv_snapshot();
                    self.emit(SessionEventWire::Conversation(snap));
                }
                SessionCommand::Query { id, query } => {
                    let value = match query {
                        SessionQuery::View => {
                            serde_json::to_value(&**self.view.load()).unwrap_or_default()
                        }
                        SessionQuery::Messages => {
                            serde_json::to_value(&self.conv.api_messages).unwrap_or_default()
                        }
                        SessionQuery::DisplayTail { items } => serde_json::to_value(
                            super::super::display::display_tail(&self.conv.api_messages, items),
                        )
                        .unwrap_or_default(),
                        other => serde_json::json!({ "unsupported": format!("{other:?}") }),
                    };
                    self.emit_to(from.map_or(Audience::Host, Audience::Client), SessionEventWire::QueryResult { id, value });
                }
                SessionCommand::Set { id, setting } => {
                    let mut view = (**self.view.load()).clone();
                    if let SessionSetting::Model { model } = &setting {
                        view.model = model.clone();
                    }
                    self.view.store(Arc::new(view.clone()));
                    let mut ev = SessionEventWire::SettingChanged(SettingApplied {
                        id,
                        setting: "echo".into(),
                        ok: true,
                        message: None,
                        view,
                        clamp: None,
                    });
                    let audience = match from {
                        Some(c) => Audience::Shared { reply_to: c },
                        None => {
                            crate::session::types::view_only(&mut ev);
                            Audience::All
                        }
                    };
                    self.emit_to(audience, ev);
                }
                SessionCommand::End { reason } => {
                    self.emit(SessionEventWire::Ended { reason });
                    return false;
                }
                other => {
                    self.emit(SessionEventWire::SystemNotice(format!("echo: {other:?}")));
                }
            }
            true
        }
    }

    async fn run(id: SessionId, mut ep: SessionEndpoints) {
        let mut echo = Echo {
            id,
            events: ep.events,
            view: ep.view,
            seq: 0,
            next_client: 1,
            clients: HashMap::new(),
            conv: ConversationSnapshot::default(),
        };
        while let Some(Addressed { from, cmd }) = ep.cmd_rx.recv().await {
            if !echo.handle(from, cmd) {
                break;
            }
        }
    }
}

/// F23: a Parked placeholder for sessions whose journal lock is held by
/// another process at reload time. Refuses Attach while held; retries
/// on every Attach so that once the holder exits, the next attach works.
pub mod locked_placeholder {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::session::types::*;
    

    /// Spawn a placeholder actor for session `id` whose journal is locked
    /// by `holder_desc` (e.g. `"pid 1234 (tui)"`). The handle starts
    /// Parked; Attach emits `AttachRefused` naming the holder. The caller
    /// registers it under the id a real session on its journal would take
    /// (`EngineHost::adopt_for_journal`), aliasing the old id to it.
    ///
    /// It holds the conversation `config` continues, which after a `/clear`
    /// is not the old actor id, with what the reload record knew of it
    /// (`restore_conversation`: what it superseded, its start).
    pub fn spawn(
        id: SessionId,
        config: SessionConfig,
        holder_desc: String,
    ) -> (SessionHandle, tokio::task::JoinHandle<()>) {
        let journal_id = match &config.continue_session {
            Some(Some(j)) => j.clone(),
            _ => id.0.clone(),
        };
        let cell = match &config.restore_conversation {
            Some(r) if r.journal_id == journal_id => JournalCell {
                journal_id: journal_id.clone(),
                supersedes: r.supersedes.clone(),
                created_at: r.created_at,
            },
            _ => JournalCell::new(journal_id.clone(), chrono::Utc::now()),
        };
        let meta = SessionMeta {
            id: id.clone(),
            name: config.name.clone(),
            model: config.model_override.clone().unwrap_or_else(|| "unknown".into()),
            cwd: config.cwd.clone(),
            created_at: cell.created_at,
            continued: true,
            continue_info: None,
            host_pid: std::process::id(),
            lifecycle: SessionLifecycle::Parked,
            clients: 0,
            input_owner: None,
            awaiting_input: 0,
            roster: Vec::new(),
            client_input: None,
            journal_id,
            locked_by: Some(holder_desc.clone()),
            pinned: false,
            busy: false,
            state_since: None,
            park_due: None,
            evict_due: None,
            title: None,
            cost_usd: 0.0,
            messages: 0,
            workers: Default::default(),
            updated_at: None,
        };
        let view = crate::session::view::RuntimeView {
            model: config.model_override.clone().unwrap_or_else(|| "unknown".into()),
            thinking_level: "off".into(),
            reasoning_level: agent_core::reasoning::ReasoningLevel::Off,
            is_reasoning_explicit: false,
            thinking_budget: 0,
            context_window: 200_000,
            system_prompt: None,
            compaction_model: "unknown".into(),
            api_retries: 0,
            subagent_timeout: 0,
            max_tool_output: 4096,
            bash_timeout: 30,
            bash_max_timeout: 300,
            prompt_generation: 0,
            hook_handler_count: 0,
            prompt_inspection: None,
        };
        let (handle, ep) = SessionHandle::new(meta, view);
        let handle = handle
            .with_placeholder_config(config.clone())
            .with_owner_token(config.owned.as_ref().map(|o| o.token.clone()));
        ep.journal.store(Arc::new(cell));
        // Start as Parked.
        ep.lifecycle.store(SessionLifecycle::Parked as u8, Ordering::Release);
        let task = tokio::spawn(run(id, ep, config, holder_desc));
        (handle, task)
    }

    async fn run(
        id: SessionId,
        mut ep: SessionEndpoints,
        config: SessionConfig,
        holder_desc: String,
    ) {
        // An owned session's placeholder ends like the session would: its
        // owner gets the re-bind grace (armed once the daemon accepts
        // connections; the fallback deadline if that never arrives), then it
        // is gone. Only a re-bind that finds the journal released replaces
        // it with the real session.
        let mut rebind_deadline: Option<tokio::time::Instant> =
            config.owned.as_ref().map(|o| crate::session::owned::initial_rebind_deadline(o.restored));
        let mut seq: u64 = 0;
        let emit_to = |seq: &mut u64, events: &broadcast::Sender<Envelope>, audience: Audience, event: SessionEventWire| {
            let env = Envelope::new(id.clone(), *seq, chrono::Utc::now(), event).to(audience);
            *seq += 1;
            let _ = events.send(env);
        };
        let emit = |seq: &mut u64, events: &broadcast::Sender<Envelope>, event: SessionEventWire| {
            emit_to(seq, events, Audience::All, event)
        };

        loop {
            let next = tokio::select! {
                c = ep.cmd_rx.recv() => c,
                _ = crate::session::owned::rebind_timer(rebind_deadline) => {
                    tracing::info!(session = %id, "owned session placeholder: the owner never re-bound; ending it");
                    emit(&mut seq, &ep.events, SessionEventWire::Ended { reason: EndReason::OwnerGone });
                    break;
                }
            };
            let Some(Addressed { from, cmd }) = next else { break };
            match cmd {
                SessionCommand::ArmOwnerRebind if config.owned.is_some() => {
                    rebind_deadline = Some(crate::session::owned::armed_rebind_deadline(rebind_deadline));
                }
                SessionCommand::Attach { ticket, .. } => {
                    let Some(to) = attach_audience(ticket.as_ref(), None) else { continue };
                    let msg = format!(
                        "session {}: journal locked by {} \
                         — close it or use synaps --attach from that process",
                        id.as_str(),
                        holder_desc
                    );
                    emit_to(&mut seq, &ep.events, to.unwrap_or(Audience::Host), SessionEventWire::AttachRefused { message: msg });
                    if let Some(t) = &ticket {
                        t.sent();
                    }
                }
                SessionCommand::Park => {
                    // Already parked, no-op.
                }
                SessionCommand::End { reason } => {
                    emit(&mut seq, &ep.events, SessionEventWire::Ended { reason });
                    break;
                }
                SessionCommand::Detach { client } => {
                    emit(&mut seq, &ep.events, SessionEventWire::ClientLeft { client });
                }
                SessionCommand::Query { id: qid, .. } => {
                    emit_to(
                        &mut seq,
                        &ep.events,
                        from.map_or(Audience::Host, Audience::Client),
                        SessionEventWire::QueryResult {
                            id: qid,
                            value: serde_json::json!({
                                "locked_by": holder_desc,
                                "streaming": false,
                                "pending_prompts": 0,
                            }),
                        },
                    );
                }
                _ => {
                    emit(
                        &mut seq,
                        &ep.events,
                        SessionEventWire::SystemNotice(format!(
                            "session {}: journal locked by {} — command ignored",
                            id.as_str(),
                            holder_desc
                        )),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::*;
    use crate::{LlmEvent, SessionEvent, StreamEvent};

    async fn next(rx: &mut broadcast::Receiver<Envelope>) -> Envelope {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timely")
            .expect("open")
    }

    /// Overlapping retries of one placeholder: the first to end does not
    /// end the other's.
    #[tokio::test]
    async fn a_retry_ends_only_itself() {
        let (handle, _task) = SessionHandle::echo_for_test(SessionId::from("p"));
        assert!(!handle.retrying());
        let first = handle.retry_guard();
        let second = handle.clone().retry_guard();
        drop(first);
        assert!(handle.retrying(), "the second retry still runs");
        drop(second);
        assert!(!handle.retrying());
    }

    #[tokio::test]
    async fn echo_actor_submit_emits_turn_in_order() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("echo-1"));
        let mut rx = handle.subscribe();
        handle
            .send(SessionCommand::Submit {
                text: "hello".into(),
                attachments: vec![],
            })
            .await
            .unwrap();

        let e0 = next(&mut rx).await;
        assert!(matches!(
            e0.event,
            SessionEventWire::TurnStarted {
                turn_baseline: 0,
                trigger: TurnTrigger::User,
                user_text: None,
            }
        ));
        let e1 = next(&mut rx).await;
        match e1.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) => assert_eq!(t, "hello"),
            other => panic!("unexpected {other:?}"),
        }
        let e2 = next(&mut rx).await;
        assert!(matches!(
            e2.event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
        ));
        let e3 = next(&mut rx).await;
        assert!(matches!(
            e3.event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
        ));
        let e4 = next(&mut rx).await;
        match e4.event {
            SessionEventWire::Conversation(c) => assert_eq!(c.api_messages.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        // gapless seq, same session id on every envelope
        assert_eq!(
            [e0.seq, e1.seq, e2.seq, e3.seq, e4.seq],
            [0, 1, 2, 3, 4]
        );
        assert!(e4.session_id == SessionId::from("echo-1"));

        handle
            .send(SessionCommand::End {
                reason: EndReason::ClientQuit,
            })
            .await
            .unwrap();
        let e5 = next(&mut rx).await;
        assert!(matches!(
            e5.event,
            SessionEventWire::Ended {
                reason: EndReason::ClientQuit
            }
        ));
        task.await.unwrap();
        assert!(!handle.is_alive());
        assert!(matches!(
            handle.send(SessionCommand::Save).await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn handle_view_swaps_on_set() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("echo-2"));
        let mut rx = handle.subscribe();
        assert_eq!(handle.view().model, "echo");
        handle
            .send(SessionCommand::Set {
                id: 0,
                setting: SessionSetting::Model { model: "other".into() },
            })
            .await
            .unwrap();
        let e = next(&mut rx).await;
        assert!(matches!(e.event, SessionEventWire::SettingChanged(_)));
        assert_eq!(handle.view().model, "other");
        drop(handle);
        task.await.unwrap();
    }
}

#[cfg(test)]
mod journal_cell_tests {
    use super::{JournalCell, JournalMove, SUPERSEDES_CAP};

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn a_compaction_keeps_the_old_id_and_the_start() {
        let a = JournalCell::new("A".into(), at("2026-11-01T10:00:00Z"));
        let b = a.moved(
            "B".into(),
            JournalMove::Compaction,
            at("2026-11-02T10:00:00Z"),
        );
        assert_eq!(b.journal_id, "B");
        assert_eq!(b.supersedes, ["A"]);
        assert_eq!(b.created_at, a.created_at, "the chain start");
        assert!(b.holds("A") && b.holds("B") && !b.holds("C"));
        let c = b.moved(
            "C".into(),
            JournalMove::Compaction,
            at("2026-11-03T10:00:00Z"),
        );
        assert_eq!(c.supersedes, ["A", "B"]);
        // In place: the journal id does not change, nothing is superseded.
        assert_eq!(
            c.moved(
                "C".into(),
                JournalMove::Compaction,
                at("2026-11-04T10:00:00Z")
            ),
            c
        );
    }

    #[test]
    fn a_clear_or_a_resume_lets_the_old_conversation_go() {
        let b = JournalCell::new("A".into(), at("2026-11-01T10:00:00Z")).moved(
            "B".into(),
            JournalMove::Compaction,
            at("2026-11-02T10:00:00Z"),
        );
        for why in [JournalMove::Clear, JournalMove::Resume] {
            let n = b.moved("N".into(), why, at("2026-11-05T10:00:00Z"));
            assert_eq!(
                n,
                JournalCell::new("N".into(), at("2026-11-05T10:00:00Z")),
                "{why:?}"
            );
            assert!(!n.holds("A") && !n.holds("B"));
        }
    }

    /// `/resume` of the journal already held reloads it: the same
    /// conversation, so the cell (start, supersedes) is kept.
    #[test]
    fn a_resume_of_the_held_journal_keeps_the_cell() {
        let b = JournalCell::new("A".into(), at("2026-11-01T10:00:00Z")).moved(
            "B".into(),
            JournalMove::Compaction,
            at("2026-11-02T10:00:00Z"),
        );
        for why in [JournalMove::Clear, JournalMove::Resume] {
            assert_eq!(
                b.moved("B".into(), why, at("2026-11-02T10:00:00Z")),
                b,
                "{why:?}"
            );
        }
    }

    #[test]
    fn supersedes_is_capped_oldest_first() {
        let t = at("2026-11-01T10:00:00Z");
        let mut c = JournalCell::new("j0".into(), t);
        for i in 1..=SUPERSEDES_CAP + 5 {
            c = c.moved(format!("j{i}"), JournalMove::Compaction, t);
        }
        assert_eq!(c.supersedes.len(), SUPERSEDES_CAP);
        assert_eq!(c.supersedes.first().map(String::as_str), Some("j5"));
        assert!(c.holds(&format!("j{}", SUPERSEDES_CAP + 4)) && !c.holds("j4"));
    }
}
