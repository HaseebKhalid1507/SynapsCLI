//! Agent cards in the session actor (agent-ui §3, phase C4a): the
//! `SurfaceTable`, the `ui` tool's create and close (T1, T18), the
//! committed projection clients see (upserts, `Attached.surfaces`,
//! presence), and `/ui on|off` (T23).
//!
//! The actor is the only authority over cards (§1.1). Its UI image lives in
//! the C2 store (`agent_core::core::session_surfaces`): one writer, one
//! commit in flight, three images (durable, in flight, later). Clients and
//! the model see only the committed projection (§3.4, R3-H1), so nothing
//! here emits a card before its commit is `Durable`.
//!
//! The writer's outcome is read without blocking the actor: while a commit
//! is in flight the actor polls it on a short tick ([`surface_tick`]) and
//! runs what waited on it then. A tool call that arrives while one is in
//! flight waits its turn, in order.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use agent_core::core::session_surfaces::{
    self as ss, Images, Lifecycle, Loaded, Op, Outcome, Poll, Receipt, Refusal, Sidecar, StoreConfig,
    Ticket, UiFs, UiStore,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::tools::ui::{unavailable, SurfaceToolHandle, SurfaceToolRequest};
use crate::ui::{
    self, CardDoc, Origin, Phase, SurfaceState, UiInput, Verb, WaitMode,
};

use super::actor::SessionActor;
use super::types::{
    ClientId, SessionCommand, SessionEventWire, SurfaceOutcome, SurfaceRefusal, SurfaceResult,
    SurfaceUnavailable,
};

/// The catalog this build draws and answers (`synaps:card/1`).
pub const CATALOG: u32 = 1;
/// How often an in-flight commit's outcome is looked for.
const POLL_EVERY: Duration = Duration::from_millis(10);
/// How often a history save's receipt is looked for (T22).
const RECEIPT_EVERY: Duration = Duration::from_millis(50);
/// §3.13 / §4.6: creations per turn and per hour, open live cards.
pub const CREATES_PER_TURN: u32 = 4;
pub const CREATES_PER_HOUR: usize = 20;
pub const OPEN_LIVE_MAX: usize = 3;
/// §3.3 #1 / §4.6: client actions per second (token bucket).
pub const ACTIONS_PER_SECOND: f64 = 10.0;

/// `UiStore` over any [`UiFs`]: the sessions directory, or a test's model.
pub struct AnyFs(pub Arc<dyn UiFs>);

impl UiFs for AnyFs {
    fn write_atomic(&self, name: &str, data: &[u8]) -> Result<(), ss::WriteFailure> {
        self.0.write_atomic(name, data)
    }
    fn sync_dir(&self) -> std::io::Result<()> {
        self.0.sync_dir()
    }
    fn read(&self, name: &str, max: usize) -> std::io::Result<ss::ReadBack> {
        self.0.read(name, max)
    }
    fn remove(&self, name: &str) -> std::io::Result<()> {
        self.0.remove(name)
    }
    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.0.rename(from, to)
    }
}

/// The sessions directory as a [`UiFs`]. Only where directory sync exists
/// (§3.4): elsewhere cards are unavailable.
#[cfg(unix)]
fn sessions_fs() -> std::io::Result<Arc<dyn UiFs>> {
    let dir = agent_core::core::private_fs::ConfinedDir::create_absolute_no_symlinks(
        &agent_core::session_lock::sessions_dir(),
    )?;
    Ok(Arc::new(ss::ConfinedUiFs::new(dir)))
}

#[cfg(not(unix))]
fn sessions_fs() -> std::io::Result<Arc<dyn UiFs>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "agent cards need directory sync",
    ))
}

/// The store and its three images.
pub(crate) struct Open {
    pub(crate) store: UiStore<AnyFs>,
    pub(crate) images: Images,
}

pub(crate) enum Store {
    /// No sidecar was found, and no card was made yet.
    Unopened,
    Open(Box<Open>),
    /// The store couldn't be opened or its sidecar read: no new cards, and
    /// every action is refused `storage` (fail closed, §3.4).
    Failed(String),
}

/// What waits on the in-flight commit.
enum Waiter {
    /// T1: the tool returns `shown` once the card is durable.
    Create { reply: Option<ReplyTo>, id: String },
    /// T18: `closed` once the close is durable.
    Close { reply: Option<ReplyTo>, id: String },
    /// A commit with nobody waiting (`/ui off`, a stop's transitions).
    Plain,
    /// T2/T3: a client's answer or skip. `accepted` once durable, then
    /// routed; `pending` at the budget (a final result follows).
    Action {
        client: ClientId,
        action_id: String,
        answer_id: String,
        rev: u32,
        deadline: tokio::time::Instant,
        pending_sent: bool,
    },
}

struct ReplyTo {
    tx: tokio::sync::oneshot::Sender<Value>,
    deadline: tokio::time::Instant,
}

impl ReplyTo {
    fn new(tx: tokio::sync::oneshot::Sender<Value>, budget: Duration) -> Self {
        Self { tx, deadline: tokio::time::Instant::now() + budget }
    }
}

pub(crate) struct SurfaceTable {
    /// Cards are on for this session: `ui.surfaces`, the create override,
    /// then `/ui on|off` (§1.5).
    pub(crate) enabled: bool,
    pub(crate) handle: SurfaceToolHandle,
    pub(crate) tool_rx: mpsc::UnboundedReceiver<SurfaceToolRequest>,
    pub(crate) store: Store,
    /// Tool calls that arrived while a commit was in flight, in order.
    queued: VecDeque<SurfaceToolRequest>,
    waiter: Option<Waiter>,
    /// Cards created in the current turn, and the creation times of the
    /// last hour (§3.13).
    created_this_turn: u32,
    created_at: VecDeque<tokio::time::Instant>,
    /// The filesystem new stores use; the sessions directory unless a test
    /// injected one.
    fs: Option<Arc<dyn UiFs>>,
    /// §3.3 #1: each client's token bucket (tokens, last refill).
    buckets: HashMap<ClientId, (f64, tokio::time::Instant)>,
    /// T6: answers steered into the running turn: `(answer_id, message
    /// text, in context)`. Every history the turn publishes is the engine's
    /// own copy, so the marks are stamped again on each adoption (T9) until
    /// the turn ends (T11/T12).
    handed: Vec<(String, String, bool)>,
    /// `ui` results this turn made, to mark on each adoption: `(call_id,
    /// card)` (§2.5, §3.6).
    results: Vec<(String, String)>,
    /// T22: for each record in context, the first save ticket whose
    /// history holds its mark (§3.11 "Recorded").
    pub(crate) tickets: HashMap<String, Ticket>,
    /// The ticket of the last queued history save.
    pub(crate) last_ticket: Option<Ticket>,
    /// Spawn loaded a chat: recover its sidecar before the first command.
    pub(crate) recover_at_start: bool,
    /// A compaction handoff is between Prepare and Finish or Rollback
    /// (§3.11), or a reload barrier runs (§3.10): accepted answers stay
    /// queued (T8).
    hold_routes: bool,
}

impl SurfaceTable {
    pub(crate) fn new(enabled: bool) -> Self {
        let (tx, tool_rx) = mpsc::unbounded_channel();
        Self {
            enabled,
            handle: SurfaceToolHandle::new(tx),
            tool_rx,
            store: Store::Unopened,
            queued: VecDeque::new(),
            waiter: None,
            created_this_turn: 0,
            created_at: VecDeque::new(),
            fs: None,
            buckets: HashMap::new(),
            handed: Vec::new(),
            results: Vec::new(),
            tickets: HashMap::new(),
            last_ticket: None,
            hold_routes: false,
            recover_at_start: false,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.store, Store::Open(_))
    }

    /// §3.3 #1: 10 actions a second per client, refilled continuously.
    fn take_token(&mut self, c: ClientId) -> bool {
        let now = tokio::time::Instant::now();
        let (tokens, at) = self.buckets.entry(c).or_insert((ACTIONS_PER_SECOND, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * ACTIONS_PER_SECOND)
            .min(ACTIONS_PER_SECOND);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// A client left: its bucket goes.
    pub(crate) fn client_gone(&mut self, c: ClientId) {
        self.buckets.remove(&c);
    }

    /// Test seam: stores open on `fs` (the C2 fault model).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn use_fs(&mut self, fs: Arc<dyn UiFs>) {
        self.fs = Some(fs);
    }

    /// A commit is in flight: [`surface_tick`] polls it.
    pub(crate) fn committing(&self) -> bool {
        self.waiter.is_some()
    }

    /// What the actor's tick waits for: a commit's outcome, or a save
    /// receipt for an answer in context.
    pub(crate) fn tick(&self) -> Tick {
        if self.committing() {
            Tick::Commit
        } else if !self.tickets.is_empty() {
            Tick::Receipt
        } else {
            Tick::Never
        }
    }

    fn open(&self) -> Option<&Open> {
        match &self.store {
            Store::Open(o) => Some(o),
            _ => None,
        }
    }

    /// What clients and the model see: the durable image plus the
    /// recovery-equivalent transitions (§3.4). Empty before any card.
    pub(crate) fn projection(&self) -> Option<Sidecar> {
        self.open().map(|o| o.images.projection())
    }

    /// Every retained card, as `Attached.surfaces` carries them (§2.5).
    pub(crate) fn snapshot_cards(&self) -> Vec<SurfaceState> {
        self.projection()
            .map(|p| p.cards.into_iter().map(|c| c.state).collect())
            .unwrap_or_default()
    }

    /// Presence's host-only `ui_barrier` (§3.10, decision 12): open cards,
    /// answers not yet settled, a commit in flight, or a transition not
    /// yet committed.
    pub(crate) fn barrier_needed(&self) -> bool {
        let Some(o) = self.open() else {
            return false;
        };
        if self.committing() || o.images.has_uncommitted() {
            return true;
        }
        let live = o.images.live();
        live.cards.iter().any(|c| c.state.phase == Phase::Open)
            || live.ledger.iter().any(|r| r.delivery != ui::Delivery::Settled)
    }

    /// `(surfaces_waiting, surfaces_open)` for presence (§2.7).
    pub(crate) fn counts(&self) -> (u32, u32) {
        let Some(p) = self.projection() else {
            return (0, 0);
        };
        let open = p.cards.iter().filter(|c| c.state.phase == Phase::Open);
        let (waiting, live): (Vec<_>, Vec<_>) =
            open.partition(|c| c.state.mode == WaitMode::Waiting);
        (waiting.len() as u32, live.len() as u32)
    }

    /// A turn started: the per-turn creation count restarts, and nothing
    /// is left to stamp from the last one.
    pub(crate) fn turn_started(&mut self) {
        self.created_this_turn = 0;
        self.handed.clear();
        self.results.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tick {
    Commit,
    Receipt,
    Never,
}

/// Wakes the actor every [`POLL_EVERY`] while a commit is in flight, every
/// [`RECEIPT_EVERY`] while an answer waits for its save receipt; otherwise
/// never.
pub(crate) async fn surface_tick(tick: Tick) {
    match tick {
        Tick::Commit => tokio::time::sleep(POLL_EVERY).await,
        Tick::Receipt => tokio::time::sleep(RECEIPT_EVERY).await,
        Tick::Never => std::future::pending::<()>().await,
    }
}

/// `"s_"` + 16 hex of `sha256(journal id ‖ 0x00 ‖ call_id)` (§2.2):
/// deterministic, so history, sidecar and table agree.
pub fn surface_id(journal_id: &str, call_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(journal_id.as_bytes());
    h.update([0u8]);
    h.update(call_id.as_bytes());
    let d = h.finalize();
    let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("s_{hex}")
}

/// Step 0's read of a prepared sidecar's parent (§3.11): its record's
/// `compacted_into` and its sidecar. Unreadable files are `Unreadable`.
fn parent_facts(store: &UiStore<AnyFs>, parent: &str) -> ss::ParentFacts {
    if !ui::is_session_id(parent) {
        return ss::ParentFacts::Unreadable;
    }
    let Ok(record) = agent_core::session::Session::load(parent) else {
        return ss::ParentFacts::Unreadable;
    };
    let sidecar = match store.read(parent) {
        Ok(Loaded::Image(sc)) => Some(sc),
        Ok(_) => None,
        Err(_) => return ss::ParentFacts::Unreadable,
    };
    ss::ParentFacts::Read { compacted_into: record.compacted_into, sidecar }
}

fn tool_error(line: impl Into<String>) -> Value {
    json!({"status": "error", "error": line.into()})
}

fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The text of a user message: its string content, or its text blocks.
fn user_text(msg: &Value) -> Option<String> {
    if msg.get("role")?.as_str()? != "user" {
        return None;
    }
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}

impl SessionActor {
    /// Cards on or off for this session (`/ui on|off`, the create override).
    /// Off withdraws every open card and makes receipts final, durable
    /// before the reply (T23): a withdrawal that isn't published refuses
    /// `/ui off`; one still unknown at the budget says "turning off…".
    /// Returns the reply line.
    pub(crate) async fn set_cards(&mut self, on: bool) -> Result<String, String> {
        let was = self.surfaces.enabled;
        if on {
            self.cards_enabled_now(true);
            return Ok(if was { "cards are already on".into() } else { "cards are on".into() });
        }
        if !was {
            return Ok("cards are already off".into());
        }
        let open: Vec<String> = self
            .surfaces
            .open()
            .map(|o| {
                o.images
                    .live()
                    .cards
                    .iter()
                    .filter(|c| c.state.phase == Phase::Open || !c.state.offers.is_empty())
                    .map(|c| c.state.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if open.is_empty() {
            self.cards_enabled_now(false);
            return Ok("cards are off".into());
        }
        let budget = self.surfaces_budget().unwrap_or(ss::COMMIT_BUDGET);
        if !self.join_surfaces(budget).await {
            return Err("cards: the card store didn't answer; try /ui off again".into());
        }
        if let Store::Open(o) = &mut self.surfaces.store {
            for card in open {
                o.images.note(Op::Close { card });
            }
        }
        match self.commit_durable(None, budget) {
            Ok(()) => {
                self.cards_enabled_now(false);
                self.publish_presence();
                Ok("cards are off: open cards were withdrawn".into())
            }
            // Unknown: off now; the final upsert follows the outcome.
            Err(_) if self.surfaces.committing() => {
                self.cards_enabled_now(false);
                Ok("cards: turning off…".into())
            }
            Err(e) => Err(format!("cards: couldn't withdraw the open cards ({e}); they stay on")),
        }
    }

    fn cards_enabled_now(&mut self, on: bool) {
        self.config.surfaces = Some(on);
        self.surfaces.enabled = on;
        if self.runtime.is_live() {
            self.runtime.set_cards_enabled(on);
        }
    }

    /// Open the store on first use. A chat with a sidecar opened it at load
    /// ([`Self::recover_surfaces_at_load`]); here it starts empty. An error
    /// fails the store closed.
    fn ensure_surface_store(&mut self) -> Result<(), String> {
        match &self.surfaces.store {
            Store::Open(_) => return Ok(()),
            Store::Failed(e) => return Err(e.clone()),
            Store::Unopened => {}
        }
        self.open_and_recover(false).map(|_| ())
    }

    /// T21 at the three load sites (spawn, unpark, resume): forget the
    /// previous conversation's store, then, if this chat has a sidecar,
    /// recover it (§3.11) before the first attach snapshot. Never starts
    /// a turn (decision 11).
    pub(crate) fn recover_surfaces_at_load(&mut self) {
        self.surfaces_reset();
        if !self.config.persist || !self.conv.is_live() {
            return;
        }
        if let Err(e) = self.open_and_recover(true) {
            tracing::warn!(session = %self.id, "agent cards: recovery failed: {e}");
        }
    }

    /// Open the store and run recovery. `only_if_present`: a chat with no
    /// sidecar stays unopened. `Ok(true)`: the store is open.
    fn open_and_recover(&mut self, only_if_present: bool) -> Result<bool, String> {
        let session_id = self.journal_id_now();
        match self.open_surface_store(&session_id, only_if_present) {
            Ok(None) => Ok(false),
            Ok(Some((open, plan))) => {
                self.surfaces.store = Store::Open(Box::new(open));
                self.apply_recovery(plan);
                self.publish_presence();
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(session = %self.id, "agent cards: store failed: {e}");
                self.surfaces.store = Store::Failed(e.clone());
                Err(e)
            }
        }
    }

    /// The store, and recovery's plan already committed (§3.11 steps 0–6).
    fn open_surface_store(&self, session_id: &str, only_if_present: bool) -> Result<Option<(Open, ss::Plan)>, String> {
        if !ui::is_session_id(session_id) {
            return Err("session id outside its bound".into());
        }
        let fs = match &self.surfaces.fs {
            Some(fs) => Arc::clone(fs),
            None => sessions_fs().map_err(|e| e.to_string())?,
        };
        let mut store = UiStore::open(Arc::new(AnyFs(Arc::clone(&fs))), session_id, StoreConfig::default())
            .map_err(|e| e.to_string())?;
        let loaded = ui::load::revalidated(store.read(session_id).map_err(|e| e.to_string())?);
        if only_if_present && matches!(loaded, Loaded::Missing) {
            return Ok(None);
        }
        let facts = self.history_facts(session_id);
        // Step 0: a prepared successor reads its parent's files, never
        // writing them.
        let parent = match &loaded {
            Loaded::Image(sc) => sc.prepared.as_ref().map(|p| parent_facts(&store, &p.parent)),
            _ => None,
        };
        let plan = ss::recover(&loaded, &facts, parent.as_ref(), chrono::Utc::now());
        if !plan.files.is_empty() {
            ss::apply_files(&*fs, &plan.files).map_err(|e| e.to_string())?;
        }
        let durable = match plan.commit.clone() {
            Some(mut image) => {
                store.commit(&mut image).map_err(|r| format!("{r:?}"))?;
                match store.join(ss::COMMIT_BUDGET) {
                    Poll::Done(Outcome::Durable) => image,
                    other => return Err(format!("recovery commit: {other:?}")),
                }
            }
            None => Sidecar::new(session_id, &ss::mint_hex128(), &facts.base),
        };
        Ok(Some((Open { store, images: Images::new(durable) }, plan)))
    }

    /// Recovery's last step: records the history lacks go back into
    /// context once, after any interruption marker, with no wake; records
    /// it holds settle with the next commit.
    fn apply_recovery(&mut self, plan: ss::Plan) {
        let mut pushed = false;
        for answer_id in &plan.deliver {
            pushed |= self.push_answer(answer_id);
        }
        if let Store::Open(o) = &mut self.surfaces.store {
            for answer_id in plan.recorded {
                o.images.note(Op::Settle { answer_id });
            }
        }
        if pushed {
            self.request_save();
        }
        self.surfaces_commit_noted();
    }

    /// Forget this conversation's store (the conversation changed): its
    /// in-flight commit is joined first, so nothing of it lands later
    /// unseen.
    pub(crate) fn surfaces_reset(&mut self) {
        if let Store::Open(o) = &mut self.surfaces.store {
            let budget = o.store.config().budget;
            let _ = o.images.poll(&mut o.store, budget);
        }
        self.surfaces.store = Store::Unopened;
        self.surfaces.waiter = None;
        self.surfaces.tickets.clear();
        self.surfaces.handed.clear();
        self.surfaces.results.clear();
        self.surfaces.last_ticket = None;
        self.surfaces.hold_routes = false;
    }

    /// T19 (`/new`), before the conversation is replaced: every card
    /// closed, the table tombstones, durable. A clear that isn't published
    /// unlinks the sidecar instead; if that fails too, `/new` is refused
    /// with the storage error.
    pub(crate) async fn surfaces_clear_for_new(&mut self) -> Result<(), String> {
        let Some(budget) = self.surfaces_budget() else {
            return Ok(());
        };
        if !self.join_surfaces(budget).await {
            return Err("cards: the card store didn't answer; try /new again".into());
        }
        let session_id = self.journal_id_now();
        if let Err(e) = self.commit_durable(Some(Op::Clear { at: rfc3339_now() }), budget) {
            if self.surfaces.committing() {
                // Unknown: it may still land; a retry clears again.
                return Err("cards: still clearing; try /new again".into());
            }
            tracing::warn!(session = %self.id, "agent cards: clear not published ({e}); removing the sidecar");
            self.store_durable(None, Some(&session_id), budget)
                .map_err(|e| format!("cards: couldn't clear the card store: {e}"))?;
        }
        Ok(())
    }

    /// What the loaded history says, for recovery (§3.11).
    pub(crate) fn history_facts(&self, session_id: &str) -> ss::HistoryFacts {
        if !self.conv.is_live() {
            return ss::HistoryFacts {
                session_id: session_id.to_string(),
                base: ss::ROOT_BASE.to_string(),
                ..Default::default()
            };
        }
        let s = &self.conv.session;
        let base = s.compaction.as_ref().map(|c| c.source_range_digest.as_str());
        ss::HistoryFacts::from_messages(
            session_id,
            base,
            s.compacted_into.as_deref(),
            self.conv.api_messages.iter().map(|m| &**m),
        )
    }

    /// The turn a card raised now belongs to, and the task it serves: the
    /// last user message with text and a turn id (§2.2 `origin`). Outside a
    /// turn there is none, and the card's answers count as late.
    fn card_origin(&self) -> Origin {
        let mut origin = Origin { turn: None, task: String::new(), stopped: None };
        if !self.conv.is_live() {
            return origin;
        }
        let last = self.conv.api_messages.iter().rev().find_map(|m| {
            let text = user_text(m)?;
            Some((m.get(crate::runtime::continuation::TURN_KEY).and_then(|t| t.get("id")).and_then(Value::as_str).map(str::to_string), text))
        });
        if let Some((turn, text)) = last {
            origin.task = ui::answer::task_excerpt(&text);
            if self.streaming {
                origin.turn = turn.filter(|t| ui::is_turn_id(t));
            }
        }
        origin
    }

    /// §1.5's capable answerer: with clients attached, at least one renders
    /// `catalog` and may give input. With none attached the card waits.
    fn capable_answerer(&self, catalog: u32) -> bool {
        if self.attached.is_empty() {
            return true;
        }
        self.attached.iter().any(|(c, a)| {
            a.meta.capabilities.surfaces >= catalog
                && self.input_refusal(*c).is_none()
                && !self.watches_owned(*c)
        })
    }

    /// A `ui` call (§1.4). Every request gets exactly one reply.
    pub(crate) fn on_ui_tool(&mut self, req: SurfaceToolRequest) {
        if self.surfaces.committing() {
            self.surfaces.queued.push_back(req);
            return;
        }
        let SurfaceToolRequest { call_id, input, reply } = req;
        let _ = match self.ui_tool_now(call_id, input, reply) {
            Some((reply, v)) => reply.send(v),
            None => Ok(()),
        };
    }

    /// The call, now that no commit is in flight. `None`: a commit was
    /// issued, and its outcome replies.
    fn ui_tool_now(
        &mut self,
        call_id: Option<String>,
        input: Value,
        reply: tokio::sync::oneshot::Sender<Value>,
    ) -> Option<(tokio::sync::oneshot::Sender<Value>, Value)> {
        if !self.surfaces.enabled {
            return Some((reply, unavailable(Some("cards_off"))));
        }
        let parsed = match ui::parse_input(&input) {
            Ok(p) => p,
            Err(e) => return Some((reply, tool_error(e.to_string()))),
        };
        match parsed {
            UiInput::Create { doc, wait } => self.ui_create(call_id, doc, wait, reply),
            UiInput::Close(id) => {
                if let Some(c) = call_id.filter(|c| ui::is_call_id(c)) {
                    self.surfaces.results.push((c, id.clone()));
                }
                self.ui_close(id, reply)
            }
        }
    }

    /// T1.
    fn ui_create(
        &mut self,
        call_id: Option<String>,
        doc: CardDoc,
        wait: bool,
        reply: tokio::sync::oneshot::Sender<Value>,
    ) -> Option<(tokio::sync::oneshot::Sender<Value>, Value)> {
        if wait {
            // v3.2 #13: parsed, then refused until C4b.
            return Some((reply, tool_error("wait: not available yet; omit it")));
        }
        let Some(call_id) = call_id.filter(|c| ui::is_call_id(c)) else {
            return Some((reply, unavailable(Some("call_id"))));
        };
        // An answer settles only through a history save (T22); a chat that
        // is never saved can't hold one (an ambiguity: the conservative
        // reading).
        if !self.config.persist {
            return Some((reply, unavailable(Some("not_saved"))));
        }
        if !self.capable_answerer(CATALOG) {
            return Some((reply, unavailable(Some("no_capable_client"))));
        }
        if self.ensure_surface_store().is_err() {
            return Some((reply, unavailable(Some("storage"))));
        }
        // §3.13: per turn, per hour, open live cards.
        let now = tokio::time::Instant::now();
        while self
            .surfaces
            .created_at
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600))
        {
            self.surfaces.created_at.pop_front();
        }
        if self.surfaces.created_this_turn >= CREATES_PER_TURN {
            return Some((reply, tool_error(format!("ui: at most {CREATES_PER_TURN} cards per turn; ask in plain text"))));
        }
        if self.surfaces.created_at.len() >= CREATES_PER_HOUR {
            return Some((reply, tool_error(format!("ui: at most {CREATES_PER_HOUR} cards an hour; ask in plain text"))));
        }
        let id = surface_id(&self.journal_id_now(), &call_id);
        let origin = self.card_origin();
        let Store::Open(o) = &mut self.surfaces.store else {
            return Some((reply, unavailable(Some("storage"))));
        };
        let live = o.images.live();
        let open_live = live
            .cards
            .iter()
            .filter(|c| c.state.phase == Phase::Open && c.state.mode == WaitMode::Live)
            .count();
        if open_live >= OPEN_LIVE_MAX {
            return Some((reply, tool_error(format!("ui: at most {OPEN_LIVE_MAX} open cards; close one or ask in plain text"))));
        }
        if live.card(&id).is_some() || live.tombstones.iter().any(|t| t.id == id) {
            // L3: fail closed; the model's retry has a new call id.
            return Some((reply, tool_error("ui: card id already in use; try again")));
        }
        let call_id_for_mark = call_id.clone();
        let state = SurfaceState {
            id: id.clone(),
            call_id,
            rev: 1,
            contract: ui::contract(&doc),
            catalog: CATALOG,
            mode: WaitMode::Live,
            phase: Phase::Open,
            offers: vec![Verb::Answer, Verb::Skip],
            doc,
            origin,
            epoch: live.epoch,
            resume: false,
            actions: Vec::new(),
            raised_at: rfc3339_now(),
            downgraded: None,
            answer: None,
        };
        // v3.3 #2 (b): due settlements, the change, then the limits, on a
        // copy: a create that would leave more than 12 cards is refused
        // before anything is committed.
        let mut trial = live.clone();
        ss::apply(&mut trial, &Op::Create(Box::new(state.clone())));
        ss::enforce_limits(&mut trial, chrono::Utc::now());
        if trial.cards.len() > ui::ATTACHED_MAX_STATES || trial.card(&id).is_none() {
            self.request_history_save();
            return Some((reply, unavailable(Some("cards_full"))));
        }
        match o.images.commit(&mut o.store, Some(Op::Create(Box::new(state)))) {
            Ok(_) => {}
            Err(Refusal::Busy) => return Some((reply, unavailable(Some("busy")))),
            Err(Refusal::Storage(_)) => return Some((reply, unavailable(Some("storage")))),
            Err(Refusal::Bounds(e)) => {
                tracing::warn!(session = %self.id, "agent cards: create over bounds: {e}");
                return Some((reply, unavailable(Some("cards_full"))));
            }
        }
        self.surfaces.created_this_turn += 1;
        self.surfaces.created_at.push_back(now);
        self.surfaces.results.push((call_id_for_mark, id.clone()));
        let budget = o.store.config().budget;
        self.surfaces.waiter = Some(Waiter::Create { reply: Some(ReplyTo::new(reply, budget)), id });
        self.publish_presence();
        None
    }

    /// T18: the agent withdraws its own card.
    fn ui_close(
        &mut self,
        id: String,
        reply: tokio::sync::oneshot::Sender<Value>,
    ) -> Option<(tokio::sync::oneshot::Sender<Value>, Value)> {
        if self.ensure_surface_store().is_err() {
            return Some((reply, unavailable(Some("storage"))));
        }
        let Store::Open(o) = &mut self.surfaces.store else {
            return Some((reply, unavailable(Some("storage"))));
        };
        let live = o.images.live();
        let Some(card) = live.card(&id) else {
            return Some((reply, tool_error(format!("close: no card {id} in this chat"))));
        };
        if !(card.phase == Phase::Open || card.phase == Phase::Answered || card.phase == Phase::Skipped) {
            return Some((reply, json!({"status": "closed", "id": id})));
        }
        match o.images.commit(&mut o.store, Some(Op::Close { card: id.clone() })) {
            Ok(_) => {}
            Err(_) => return Some((reply, tool_error("close: couldn't save; the card may still be open"))),
        }
        let budget = o.store.config().budget;
        self.surfaces.waiter = Some(Waiter::Close { reply: Some(ReplyTo::new(reply, budget)), id });
        self.publish_presence();
        None
    }

    /// Commit what was noted (a stop, `/ui off`) when nothing is in flight;
    /// otherwise it rides the next commit.
    pub(crate) fn surfaces_commit_noted(&mut self) {
        if self.surfaces.committing() {
            return;
        }
        let Store::Open(o) = &mut self.surfaces.store else {
            return;
        };
        if !o.images.has_uncommitted() {
            return;
        }
        if o.images.commit(&mut o.store, None).is_ok() {
            self.surfaces.waiter = Some(Waiter::Plain);
            self.publish_presence();
        }
    }

    /// The tick: read the in-flight outcome, run what waited on it, then
    /// the next queued call.
    pub(crate) async fn poll_surfaces(&mut self) {
        if !self.surfaces.committing() {
            self.settle_recorded();
            return;
        }
        self.poll_surfaces_with(Duration::ZERO).await;
    }

    /// One look at the in-flight commit, waiting up to `timeout` (a
    /// Prepare's join).
    async fn poll_surfaces_with(&mut self, timeout: Duration) {
        let Store::Open(o) = &mut self.surfaces.store else {
            self.surfaces.waiter = None;
            return;
        };
        let before = o.images.projection();
        let poll = o.images.poll(&mut o.store, timeout);
        let lifecycle = Lifecycle::of(&poll);
        let done = matches!(poll, Poll::Done(_) | Poll::Idle);
        let after = o.images.projection();
        if done {
            self.emit_card_changes(&before, &after);
        }
        let now = tokio::time::Instant::now();
        let unconfirmed = |id: &str, note: &str| json!({"status": "unconfirmed", "id": id, "note": note});
        let mut route = None;
        match self.surfaces.waiter.take() {
            None => {}
            Some(Waiter::Plain) => {
                if !done {
                    self.surfaces.waiter = Some(Waiter::Plain);
                }
            }
            Some(Waiter::Create { mut reply, id }) => {
                let answer = match (&lifecycle, done) {
                    (Lifecycle::Done, true) => Some(json!({"status": "shown", "id": id})),
                    (Lifecycle::Failed(_), true) => Some(tool_error("ui: couldn't save the card; ask in plain text")),
                    _ if reply.as_ref().is_some_and(|r| now >= r.deadline) || done => Some(unconfirmed(
                        &id,
                        "Not confirmed yet: the card may still appear, and its answer would arrive as a message. Close it if you ask in plain text instead.",
                    )),
                    _ => None,
                };
                if let Some(v) = answer {
                    if let Some(r) = reply.take() {
                        let _ = r.tx.send(v);
                    }
                }
                if !done {
                    self.surfaces.waiter = Some(Waiter::Create { reply, id });
                }
            }
            Some(Waiter::Close { mut reply, id }) => {
                let answer = match (&lifecycle, done) {
                    (Lifecycle::Done, true) => Some(json!({"status": "closed", "id": id})),
                    (Lifecycle::Failed(_), true) => Some(tool_error("close: couldn't save; the card is still open")),
                    _ if reply.as_ref().is_some_and(|r| now >= r.deadline) || done => {
                        Some(unconfirmed(&id, "Not confirmed yet: the card may still be open."))
                    }
                    _ => None,
                };
                if let Some(v) = answer {
                    if let Some(r) = reply.take() {
                        let _ = r.tx.send(v);
                    }
                }
                if !done {
                    self.surfaces.waiter = Some(Waiter::Close { reply, id });
                }
            }
            Some(Waiter::Action { client, action_id, answer_id, rev, deadline, pending_sent }) => match &poll {
                Poll::Idle | Poll::Done(Outcome::Durable) => {
                    // Durable: the ack (the upsert went above), then the route.
                    self.surface_result(client, &action_id, SurfaceOutcome::Accepted, None, None, Some(rev), None);
                    route = Some(answer_id);
                }
                Poll::Done(Outcome::NotPublished(_)) => {
                    // The only outcome that may say "Not sent" (§3.4).
                    self.surface_refused(client, &action_id, SurfaceRefusal::Storage, true, None);
                }
                Poll::Done(Outcome::Unsynced(_)) => {
                    // Published, never durable: the store failed. It stays
                    // pending and nothing is delivered from it (§3.4).
                    if !pending_sent {
                        self.surface_result(client, &action_id, SurfaceOutcome::Pending, None, None, None, None);
                    }
                }
                Poll::Pending => {
                    let mut pending_sent = pending_sent;
                    if !pending_sent && now >= deadline {
                        self.surface_result(client, &action_id, SurfaceOutcome::Pending, None, None, None, None);
                        pending_sent = true;
                    }
                    self.surfaces.waiter =
                        Some(Waiter::Action { client, action_id, answer_id, rev, deadline, pending_sent });
                }
            },
        }
        if done {
            self.publish_presence();
            if let Some(answer_id) = route {
                self.route_answer(&answer_id).await;
            }
            // T22: settled records leave the ledger once that is durable,
            // and a park that waited on them is armed again.
            self.release_settled();
            if self.attached.is_empty() && !self.cards_unsettled() {
                self.rearm_park();
            }
            // What a stop or a delivery noted meanwhile commits next.
            self.surfaces_commit_noted();
            while !self.surfaces.committing() {
                let Some(req) = self.surfaces.queued.pop_front() else {
                    break;
                };
                self.on_ui_tool(req);
            }
        }
    }

    /// One `surface` upsert per card whose committed state changed (§2.3):
    /// the pump sends each only to clients whose catalog covers it.
    pub(crate) fn emit_card_changes(&mut self, before: &Sidecar, after: &Sidecar) {
        for c in &after.cards {
            let prev = before.card(&c.state.id);
            if prev != Some(&c.state) {
                self.emit(SessionEventWire::Surface(Box::new(c.state.clone())));
            }
        }
    }

    /// Ask the persister for one history save (a create refused
    /// `cards_full` waits on settlements, v3.3 #2).
    fn request_history_save(&mut self) {
        self.request_save();
    }

}

/// `hex128(sha256(canonical_json({verb, expect_rev, epoch, values})))`
/// over the values **as received** (v3.4 #5, §3.7): admission #4 runs
/// before #11 parses them, and a retry re-sends the stored payload, so its
/// hash matches. A skip's absent values hash as `null`.
pub fn action_hash(verb: Verb, expect_rev: u32, epoch: u32, values: Option<&Value>) -> String {
    use sha2::{Digest, Sha256};
    let payload = json!({
        "verb": verb,
        "expect_rev": expect_rev,
        "epoch": epoch,
        "values": values.cloned().unwrap_or(Value::Null),
    });
    let d = Sha256::digest(ui::contract::canonical_json(&payload).as_bytes());
    d[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn is_action_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `"a_"` + 16 hex, host-random (§3.6).
fn mint_answer_id() -> String {
    format!("a_{}", &ss::mint_hex128()[..16])
}

impl SessionActor {
    pub(crate) fn surface_result(
        &mut self,
        client: ClientId,
        action_id: &str,
        outcome: SurfaceOutcome,
        reason: Option<SurfaceRefusal>,
        retry: Option<bool>,
        answer_rev: Option<u32>,
        detail: Option<SurfaceUnavailable>,
    ) {
        self.emit(SessionEventWire::SurfaceResult(SurfaceResult {
            client,
            action_id: action_id.to_string(),
            outcome,
            reason,
            retry,
            answer_rev,
            detail,
        }));
    }

    fn surface_refused(
        &mut self,
        client: ClientId,
        action_id: &str,
        reason: SurfaceRefusal,
        retry: bool,
        detail: Option<SurfaceUnavailable>,
    ) {
        self.surface_result(client, action_id, SurfaceOutcome::Refused, Some(reason), Some(retry), None, detail);
    }

    /// A client's `surface_action` (§2.4, §3.3). All of admission runs
    /// here, in order, before anything else of `handle`: before
    /// `note_input` and before the B3 wake, so refused traffic never wakes
    /// a parked session (v3.4 #6). Every refusal has no side effect.
    pub(crate) async fn on_surface_action(&mut self, from: Option<ClientId>, cmd: SessionCommand) {
        // The host never answers a card (§4.2).
        let Some(c) = from else {
            return;
        };
        let caps = self.attached.get(&c).map_or(0, |a| a.meta.capabilities.surfaces);
        // v3.4 #3 (b), #6: a client that declared 0 keeps C3's `Refused`
        // (owned, input and fence reasons first), never a `surface_result`.
        if caps == 0 {
            let reason = self
                .owned_refusal(c, &cmd)
                .or_else(|| self.input_refusal(c))
                .or_else(|| self.reload_fence.is_some().then(|| super::RELOADING_REFUSAL.to_string()))
                .unwrap_or_else(|| super::SURFACES_UNAVAILABLE.to_string());
            self.emit(SessionEventWire::Refused { client: c, command: "surface_action".into(), reason });
            return;
        }
        // #1: the client's bucket, first and cheap.
        let rate_ok = self.surfaces.take_token(c);
        let forbidden = self.owned_refusal(c, &cmd).is_some() || self.input_refusal(c).is_some();
        let SessionCommand::SurfaceAction { surface_id, contract, action_id, expect_rev, epoch, verb, values } = cmd
        else {
            return;
        };
        if !rate_ok {
            return self.surface_refused(c, &action_id, SurfaceRefusal::RateLimited, true, None);
        }
        // #2: owned session, input authority.
        if forbidden {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Forbidden, false, None);
        }
        // #3: the reload fence.
        if self.reload_fence.is_some() {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Reloading, true, None);
        }
        if !is_action_id(&action_id) {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Invalid, false, None);
        }
        if self.ensure_surface_store().is_err() {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Storage, false, None);
        }
        let hash = action_hash(verb, expect_rev, epoch, values.as_ref());
        let req = ss::ActionRequest {
            card: surface_id.clone(),
            action_id: action_id.clone(),
            hash: hash.clone(),
            verb,
            contract,
            epoch,
            expect_rev,
        };
        let Store::Open(o) = &self.surfaces.store else {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Storage, false, None);
        };
        // #4–#10 on the live state (C2's `Images::admit`).
        if let Err(a) = o.images.admit(&o.store, &req) {
            use ss::Admission as A;
            return match a {
                A::Duplicate { rev } => {
                    self.surface_result(c, &action_id, SurfaceOutcome::Duplicate, None, None, Some(rev), None)
                }
                A::Pending => self.surface_result(c, &action_id, SurfaceOutcome::Pending, None, None, None, None),
                A::Reused => self.surface_refused(c, &action_id, SurfaceRefusal::Invalid, false, None),
                A::Busy => self.surface_refused(c, &action_id, SurfaceRefusal::Busy, true, None),
                A::Storage(_) => self.surface_refused(c, &action_id, SurfaceRefusal::Storage, false, None),
                A::Resolved => self.surface_refused(c, &action_id, SurfaceRefusal::Resolved, false, None),
                A::Gone => self.surface_refused(c, &action_id, SurfaceRefusal::Gone, false, None),
                A::Stale => self.surface_refused(c, &action_id, SurfaceRefusal::Stale, false, None),
                A::Stopped => self.surface_refused(c, &action_id, SurfaceRefusal::Stopped, false, None),
                A::Conflict => self.surface_refused(c, &action_id, SurfaceRefusal::Conflict, false, None),
            };
        }
        let live = o.images.live();
        let Some(card) = live.card(&surface_id).cloned() else {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Gone, false, None);
        };
        // v3.4 #3 (c): a catalog this client doesn't draw was never sent to
        // it, so for it the card doesn't exist.
        if card.catalog > caps {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Gone, false, None);
        }
        // #11: the values, against the doc. Verb and values pair strictly
        // (v3.4 #5): an answer has values, a skip none.
        let parsed = match (verb, &values) {
            (Verb::Answer, Some(v)) if ui::contract::canonical_json(v).len() <= ui::ANSWER_MAX_BYTES => {
                ui::answer::parse_answer(&card.doc, v).ok()
            }
            (Verb::Skip, None) => Some(ui::Answer::default()),
            _ => None,
        };
        let Some(answer) = parsed else {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Invalid, false, None);
        };
        // #12: routing is admissible now.
        let blocked = if self.conv.is_live() && self.conv.context_head.is_blocked(&self.conv.session) {
            Some(SurfaceUnavailable::ContextBlocked)
        } else if self.driver.as_ref().is_some_and(|d| d.steering.len() >= super::driver::STEERING_MAX_MESSAGES) {
            Some(SurfaceUnavailable::QueueFull)
        } else if live.ledger.len() >= ui::LEDGER_MAX_RECORDS
            || live.ledger.iter().map(|r| r.values.to_json().len()).sum::<usize>() + answer.to_json().len()
                > ui::LEDGER_MAX_VALUE_BYTES
        {
            Some(SurfaceUnavailable::LedgerFull)
        } else if self.owned.as_ref().is_some_and(|o| o.awaiting_owner) {
            Some(SurfaceUnavailable::AwaitingOwner)
        } else {
            None
        };
        if let Some(detail) = blocked {
            return self.surface_refused(c, &action_id, SurfaceRefusal::Unavailable, true, Some(detail));
        }
        // Admitted: with shared input the answerer becomes the active client.
        self.note_input(c);
        let status = match verb {
            Verb::Skip => ui::AnswerStatus::Skipped,
            _ => ui::AnswerStatus::Answered,
        };
        let q = card.doc.question.clone();
        let asked = ui::answer::build_capsule(&card.doc, &card.origin.task, &answer);
        let answer_id = mint_answer_id();
        let by = self.attached.get(&c).map(|a| a.name.clone()).unwrap_or_default();
        let accepted = ss::Accepted {
            record: ui::LedgerRecord {
                answer_id: answer_id.clone(),
                card: surface_id,
                rev: 1,
                status,
                values: answer,
                // §3.9: consent for this epoch; a skip never starts a turn.
                wake: verb == Verb::Answer,
                epoch: live.epoch,
                delivery: ui::Delivery::Queued,
                q,
                asked: Some(asked.as_str().to_string()),
            },
            action: ui::LogEntry { action_id: action_id.clone(), hash, rev: 1 },
            by: ui::roster_label(&by),
            at: rfc3339_now(),
            // C4c adds `revise`.
            offers: Vec::new(),
        };
        let Store::Open(o) = &mut self.surfaces.store else {
            return;
        };
        match o.images.commit(&mut o.store, Some(Op::Accept(Box::new(accepted)))) {
            Ok(_) => {}
            Err(Refusal::Busy) => return self.surface_refused(c, &action_id, SurfaceRefusal::Busy, true, None),
            Err(Refusal::Storage(_) | Refusal::Bounds(_)) => {
                return self.surface_refused(c, &action_id, SurfaceRefusal::Storage, false, None)
            }
        }
        let deadline = tokio::time::Instant::now() + o.store.config().budget;
        self.surfaces.waiter =
            Some(Waiter::Action { client: c, action_id, answer_id, rev: 1, deadline, pending_sent: false });
        self.publish_presence();
    }

    /// `may_wake(r)` (§3.9, R3-H2), evaluated at the start itself: the
    /// record has consent, for the current epoch, and policy allows a start
    /// (cards on, no fence, no owner wait).
    pub(crate) fn may_wake(&self, answer_id: &str) -> bool {
        let Some(o) = self.surfaces.open() else {
            return false;
        };
        let live = o.images.live();
        let Some(r) = live.record(answer_id) else {
            return false;
        };
        r.wake && r.epoch == live.epoch && self.surfaces.enabled && self.autonomous_work_blocked().is_none()
    }

    /// The message an answer reaches the model as (§3.6), with its mark.
    /// `late`: delivered outside the turn that raised its card; it carries
    /// the capsule built at acceptance.
    fn answer_message(&self, answer_id: &str, late: bool) -> Option<Value> {
        let o = self.surfaces.open()?;
        let live = o.images.live();
        let r = live.record(answer_id)?;
        let m = ui::AnswerMessage {
            card: r.card.clone(),
            answer_id: r.answer_id.clone(),
            q: r.q.clone(),
            status: r.status,
            rev: r.rev,
            values: r.values.clone(),
            asked: if late { r.asked.as_deref().and_then(ui::Capsule::parse) } else { None },
        };
        let mark = ui::UiMark::Answer(ui::AnswerMark {
            l: live.lineage.clone(),
            card: r.card.clone(),
            answer: r.answer_id.clone(),
            rev: r.rev,
            status: r.status,
        });
        let mut msg = json!({"role": "user", "content": ui::answer::format_answer_message(&m)});
        msg[ui::MARK_KEY] = serde_json::to_value(mark).ok()?;
        Some(msg)
    }

    /// Whether this answer would reach the turn that asked (no capsule).
    fn on_time(&self, answer_id: &str) -> bool {
        let Some(o) = self.surfaces.open() else {
            return false;
        };
        let live = o.images.live();
        let card = live.record(answer_id).and_then(|r| live.card(&r.card));
        let turn = self.card_origin().turn;
        self.streaming
            && card.is_some_and(|c| c.origin.stopped.is_none() && c.origin.turn.is_some() && c.origin.turn == turn)
    }

    fn note_delivery(&mut self, answer_id: &str, to: ui::Delivery) {
        if let Store::Open(o) = &mut self.surfaces.store {
            o.images.note(Op::Deliver { answer_id: answer_id.to_string(), to });
        }
    }

    /// Push a record's marked message into history (T7, T8's drain, T11,
    /// T12): in context. The caller decides about a turn.
    fn push_answer(&mut self, answer_id: &str) -> bool {
        if !self.conv.is_live() {
            return false;
        }
        let Some(msg) = self.answer_message(answer_id, true) else {
            return false;
        };
        self.conv.api_messages.push(Arc::new(msg));
        self.note_delivery(answer_id, ui::Delivery::InContext);
        true
    }

    /// T5–T8: route an answer that just became durable (§3.5).
    pub(crate) async fn route_answer(&mut self, answer_id: &str) {
        // T8: compaction running: stays queued, drained when it lands.
        if self.compact.is_some() || self.compacting_inline || self.surfaces.hold_routes {
            return;
        }
        if self.is_parked() {
            let wakes = self.may_wake(answer_id);
            // Only an answer whose route starts a turn wakes a parked
            // session (v3.4 #6); a failed wake leaves it queued (T8).
            if !wakes || self.ensure_live().await.is_err() {
                return;
            }
        }
        if self.streaming {
            // T6: into the running turn, tracked until adopted.
            let on_time = self.on_time(answer_id);
            let Some(msg) = self.answer_message(answer_id, !on_time) else {
                return;
            };
            let text = msg["content"].as_str().unwrap_or_default().to_string();
            if let Some(tx) = &self.steer_tx {
                let _ = tx.send(text.clone());
            }
            self.surfaces.handed.push((answer_id.to_string(), text, false));
            self.note_delivery(answer_id, ui::Delivery::Handed);
            self.emit_conversation();
            return;
        }
        // T7: idle: into history, then a turn if consent holds now.
        if !self.push_answer(answer_id) {
            return;
        }
        self.request_save();
        self.emit_conversation();
        if self.driver.is_some() {
            // An armed driver runs the session's turns: the answer waits in
            // history for its next one (the conservative reading).
            return;
        }
        if self.may_wake(answer_id) {
            // Human input: the auto-turn count starts over, as a submit's.
            self.consecutive_auto_turns = 0;
            self.start_turn(super::types::TurnTrigger::User, None).await;
        }
    }

    /// T9: stamp the marks an adopted history now holds: an answer steered
    /// into the turn, and the user message carrying `ui` results.
    /// Idempotent: each adoption is the engine's unstamped copy.
    pub(crate) fn stamp_adopted(&mut self, history: &mut [crate::SharedMessage]) {
        let Some(lineage) = self.surfaces.open().map(|o| o.images.live().lineage.clone()) else {
            return;
        };
        let mut handed = std::mem::take(&mut self.surfaces.handed);
        let mut noted = false;
        for (answer_id, text, in_context) in &mut handed {
            let hit = history.iter_mut().rev().find(|m| {
                m.get("role").and_then(Value::as_str) == Some("user")
                    && m.get("content").and_then(Value::as_str) == Some(text.as_str())
            });
            let (Some(m), Some(marked)) = (hit, self.answer_message(answer_id, false)) else {
                continue;
            };
            if m.get(ui::MARK_KEY) != Some(&marked[ui::MARK_KEY]) {
                Arc::make_mut(m)[ui::MARK_KEY] = marked[ui::MARK_KEY].clone();
            }
            if !*in_context {
                *in_context = true;
                noted = true;
                self.note_delivery(answer_id, ui::Delivery::InContext);
            }
        }
        self.surfaces.handed = handed;
        if noted {
            self.surfaces_commit_noted();
        }
        // The `ui` results: one mark per user message, listing its cards.
        for (call_id, card) in self.surfaces.results.clone() {
            let hit = history.iter_mut().rev().find(|m| {
                m.get("content").and_then(Value::as_array).is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        b.get("type").and_then(Value::as_str) == Some("tool_result")
                            && b.get("tool_use_id").and_then(Value::as_str) == Some(call_id.as_str())
                    })
                })
            });
            let Some(m) = hit else {
                continue;
            };
            let mut results = match m.get(ui::MARK_KEY).cloned().and_then(|v| serde_json::from_value::<ui::UiMark>(v).ok()) {
                Some(ui::UiMark::Results(r)) => r.results,
                _ => Vec::new(),
            };
            if results.iter().any(|r| r.card == card) {
                continue;
            }
            results.push(ui::ResultMark { card, answer: None });
            let mark = ui::UiMark::Results(ui::ResultsMark { l: lineage.clone(), results });
            if let Ok(v) = serde_json::to_value(mark) {
                Arc::make_mut(m)[ui::MARK_KEY] = v;
            }
        }
    }

    /// A `SteeringDelivered` for an answer this turn was handed (T6).
    pub(crate) fn is_handed_answer(&self, event: &crate::StreamEvent) -> bool {
        match event {
            crate::StreamEvent::Agent(crate::AgentEvent::SteeringDelivered { message }) => {
                self.surfaces.handed.iter().any(|(_, text, _)| text == message)
            }
            _ => false,
        }
    }

    /// T11: the turn ended with answers handed but never adopted: into
    /// history, in ledger order. Returns those that may start a turn.
    pub(crate) fn flush_handed_answers(&mut self) -> Vec<String> {
        let handed = std::mem::take(&mut self.surfaces.handed);
        self.surfaces.results.clear();
        let mut wake = Vec::new();
        for (answer_id, _, in_context) in handed {
            if !in_context && self.push_answer(&answer_id) && self.may_wake(&answer_id) {
                wake.push(answer_id);
            }
        }
        if !wake.is_empty() || self.surfaces.open().is_some_and(|o| o.images.has_uncommitted()) {
            self.surfaces_commit_noted();
        }
        wake
    }

    /// The tail of an interrupted turn (T12, T15–T17): the stop raises the
    /// epoch, then answers handed but never adopted go in after the marker,
    /// and no turn starts. A turn the engine had already finished
    /// (`_completed`) still raises it: a cancel withdraws consent either way
    /// (the conservative reading).
    pub(crate) fn surfaces_interrupted(&mut self, reason: crate::engine::interrupt::InterruptReason, _completed: bool) {
        use crate::engine::interrupt::InterruptReason as R;
        let kind = match reason {
            R::User => ui::StopKind::User,
            R::CostCap | R::Host | R::Driver => ui::StopKind::System,
            R::Restart => ui::StopKind::Reload,
            R::Crash | R::Unknown => ui::StopKind::Crash,
        };
        self.surfaces_stop(kind);
        let handed = std::mem::take(&mut self.surfaces.handed);
        self.surfaces.results.clear();
        for (answer_id, _, in_context) in handed {
            if !in_context {
                self.push_answer(&answer_id);
            }
        }
    }

    /// A stop (T15/T16): the epoch rises in memory now and rides the next
    /// commit; the cancel never waits for it.
    pub(crate) fn surfaces_stop(&mut self, kind: ui::StopKind) {
        let Store::Open(o) = &mut self.surfaces.store else {
            return;
        };
        o.images.note(Op::Stop(kind));
        self.surfaces_commit_noted();
    }

    /// A user Cancel with no turn running revokes a pending wake (T15): a
    /// queued record with consent.
    pub(crate) fn surfaces_cancel_idle(&mut self) {
        let wake_pending = self.surfaces.open().is_some_and(|o| {
            let live = o.images.live();
            live.ledger
                .iter()
                .any(|r| r.wake && r.epoch == live.epoch && r.delivery == ui::Delivery::Queued)
        });
        if wake_pending {
            self.surfaces_stop(ui::StopKind::User);
        }
    }

    /// A history save was queued (`request_save`): every record in context
    /// whose mark this history holds and that has no ticket yet gets this
    /// one (§3.11: "the first ticket whose history holds its mark").
    pub(crate) fn note_save_ticket(&mut self, ticket: Ticket) {
        let Some(o) = self.surfaces.open() else {
            return;
        };
        let live = o.images.live();
        let waiting: Vec<String> = live
            .ledger
            .iter()
            .filter(|r| r.delivery == ui::Delivery::InContext && !self.surfaces.tickets.contains_key(&r.answer_id))
            .map(|r| r.answer_id.clone())
            .collect();
        if waiting.is_empty() || !self.conv.is_live() {
            return;
        }
        let facts = ss::HistoryFacts::from_messages(&ticket.session_id, None, None, self.conv.api_messages.iter().map(|m| &**m));
        for answer_id in waiting {
            if facts.has_answer(&answer_id) {
                self.surfaces.tickets.insert(answer_id, ticket.clone());
            }
        }
    }

    /// T22: a record whose ticket is `Saved` is `Recorded`, then settled by
    /// the next commit.
    fn settle_recorded(&mut self) {
        let saved: Vec<String> = self
            .surfaces
            .tickets
            .iter()
            .filter(|(_, t)| self.persister.receipt(t) == Receipt::Saved)
            .map(|(id, _)| id.clone())
            .collect();
        if saved.is_empty() {
            return;
        }
        let Store::Open(o) = &mut self.surfaces.store else {
            self.surfaces.tickets.clear();
            return;
        };
        for answer_id in saved {
            self.surfaces.tickets.remove(&answer_id);
            o.images.note(Op::Deliver { answer_id: answer_id.clone(), to: ui::Delivery::Recorded });
            o.images.note(Op::Settle { answer_id });
        }
        self.surfaces_commit_noted();
    }

    /// Settled records leave the ledger, riding the next commit, once
    /// their settlement is durable.
    fn release_settled(&mut self) {
        let Store::Open(o) = &mut self.surfaces.store else {
            return;
        };
        let settled: Vec<String> = o
            .images
            .durable()
            .ledger
            .iter()
            .filter(|r| r.delivery == ui::Delivery::Settled)
            .map(|r| r.answer_id.clone())
            .collect();
        let live = o.images.live();
        for answer_id in settled {
            if live.record(&answer_id).is_some_and(|r| r.delivery == ui::Delivery::Settled) {
                o.images.note(Op::Release { answer_id });
            }
        }
    }

    /// An answer is in history but not yet settled (T22): parking,
    /// compaction and eviction wait for it.
    pub(crate) fn cards_unsettled(&self) -> bool {
        self.surfaces.open().is_some_and(|o| {
            o.images.live().ledger.iter().any(|r| {
                matches!(r.delivery, ui::Delivery::Handed | ui::Delivery::InContext | ui::Delivery::Recorded)
            })
        })
    }

    /// T8's drain: after a compaction lands (or a wake), queued answers go
    /// into history. `true`: one may start a turn now.
    pub(crate) fn drain_queued_answers(&mut self) -> bool {
        if self.streaming {
            return false;
        }
        let queued: Vec<String> = self
            .surfaces
            .open()
            .map(|o| {
                o.images
                    .live()
                    .ledger
                    .iter()
                    .filter(|r| r.delivery == ui::Delivery::Queued)
                    .map(|r| r.answer_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let (mut wake, mut pushed) = (false, false);
        for answer_id in queued {
            if self.push_answer(&answer_id) {
                pushed = true;
                wake |= self.may_wake(&answer_id);
            }
        }
        if pushed {
            self.request_save();
            self.emit_conversation();
        }
        self.surfaces_commit_noted();
        wake
    }
}

impl SessionActor {
    /// The reload barrier (§3.10), beside the checkpoint's save: join the
    /// in-flight commit, every answer in context must have a `Saved`
    /// receipt (they become `Recorded`), then one `checkpointed` commit
    /// must be durable. `Err` names the reason the checkpoint reports:
    /// `ui_store` or `history`. A chat without cards passes.
    pub(crate) async fn reload_barrier(&mut self) -> Result<(), &'static str> {
        let Some(budget) = self.surfaces_budget() else {
            return Ok(());
        };
        self.surfaces.hold_routes = true;
        let res = self.reload_barrier_inner(budget).await;
        self.surfaces.hold_routes = false;
        self.publish_presence();
        res
    }

    async fn reload_barrier_inner(&mut self, budget: Duration) -> Result<(), &'static str> {
        if !self.join_surfaces(budget).await {
            return Err("ui_store");
        }
        let receipts = self.persister.receipts();
        let Store::Open(o) = &mut self.surfaces.store else {
            return Ok(());
        };
        let before = o.images.projection();
        let res = ss::checkpoint_barrier(&mut o.images, &mut o.store, &self.surfaces.tickets, &receipts, budget);
        let after = o.images.projection();
        self.emit_card_changes(&before, &after);
        match res {
            Ok(()) => Ok(()),
            Err(ss::BarrierFailure::History) => Err("history"),
            Err(ss::BarrierFailure::UiStore(e)) => {
                tracing::warn!(session = %self.id, "agent cards: reload barrier: {e}");
                // A commit still unknown stays with the tick.
                if self.surfaces.open().is_some_and(|o| o.images.in_flight()) {
                    self.surfaces.waiter = Some(Waiter::Plain);
                }
                Err("ui_store")
            }
        }
    }
}

/// A compaction handoff between Prepare and Finish (§3.11).
pub(crate) struct Handoff {
    /// The successor's prepared image; `None` in place.
    successor: Option<Sidecar>,
}

impl SessionActor {
    fn surfaces_budget(&self) -> Option<Duration> {
        self.surfaces.open().map(|o| o.store.config().budget)
    }

    /// Join the in-flight commit, and whatever it set off, within `budget`.
    async fn join_surfaces(&mut self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        while self.surfaces.committing() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            self.poll_surfaces_with(left).await;
        }
        true
    }

    /// Commit what is noted plus `op`, and wait for it to be durable. A
    /// commit still unknown at `budget` stays with the tick.
    fn commit_durable(&mut self, op: Option<Op>, budget: Duration) -> Result<(), String> {
        let Store::Open(o) = &mut self.surfaces.store else {
            return Err("no card store".into());
        };
        let before = o.images.projection();
        o.images.commit(&mut o.store, op).map_err(|r| format!("{r:?}"))?;
        let poll = o.images.poll(&mut o.store, budget);
        let after = o.images.projection();
        let res = match poll {
            Poll::Done(Outcome::Durable) | Poll::Idle => Ok(()),
            Poll::Done(Outcome::NotPublished(e) | Outcome::Unsynced(e)) => Err(e),
            Poll::Pending => {
                self.surfaces.waiter = Some(Waiter::Plain);
                Err("the card store didn't answer".into())
            }
        };
        self.emit_card_changes(&before, &after);
        res
    }

    /// A write outside the images (a successor's sidecar, or its removal),
    /// joined within `budget`.
    fn store_durable(&mut self, image: Option<&mut Sidecar>, remove: Option<&str>, budget: Duration) -> Result<(), String> {
        let Store::Open(o) = &mut self.surfaces.store else {
            return Err("no card store".into());
        };
        let issued = match (image, remove) {
            (Some(img), _) => o.store.commit(img),
            (None, Some(id)) => o.store.remove(id),
            (None, None) => return Ok(()),
        };
        issued.map_err(|r| format!("{r:?}"))?;
        match o.store.join(budget) {
            Poll::Done(Outcome::Durable) | Poll::Idle => Ok(()),
            Poll::Done(Outcome::NotPublished(e) | Outcome::Unsynced(e)) => Err(e),
            Poll::Pending => Err("the card store didn't answer".into()),
        }
    }

    /// Prepare (§3.11 step 1), when the summary lands: join the writer,
    /// settle every answer in context, commit `compacting` on the parent
    /// (the ledger cut), and for a successor write its prepared sidecar.
    /// `Ok(None)`: this chat has no card store, nothing to hand off. `Err`:
    /// the compaction fails, and is retried at the next boundary.
    pub(crate) async fn handoff_prepare(&mut self, successor_id: Option<&str>) -> Result<Option<Handoff>, String> {
        let Some(budget) = self.surfaces_budget() else {
            return Ok(None);
        };
        self.surfaces.hold_routes = true;
        let res = self.handoff_prepare_inner(successor_id, budget).await;
        if res.is_err() {
            self.surfaces.hold_routes = false;
        }
        res.map(Some)
    }

    async fn handoff_prepare_inner(&mut self, successor_id: Option<&str>, budget: Duration) -> Result<Handoff, String> {
        // The fence: the in-flight commit's outcome, at the budget.
        if !self.join_surfaces(budget).await {
            return Err("card answers not yet saved: the card store didn't answer".into());
        }
        // Both receipts for every answer in context (T22).
        let receipts = self.persister.receipts();
        let Some(o) = self.surfaces.open() else {
            return Err("no card store".into());
        };
        let live = o.images.live();
        let mut settle = Vec::new();
        for r in &live.ledger {
            match r.delivery {
                ui::Delivery::Handed => return Err(ss::HandoffError::Unsettled.to_string()),
                ui::Delivery::InContext | ui::Delivery::Recorded => {
                    let saved = r.delivery == ui::Delivery::Recorded
                        || self.surfaces.tickets.get(&r.answer_id).is_some_and(|t| receipts.receipt(t) == Receipt::Saved);
                    if !saved {
                        return Err(ss::HandoffError::Unsettled.to_string());
                    }
                    settle.push(r.answer_id.clone());
                }
                _ => {}
            }
        }
        if let Store::Open(o) = &mut self.surfaces.store {
            for answer_id in settle {
                self.surfaces.tickets.remove(&answer_id);
                o.images.note(Op::Deliver { answer_id: answer_id.clone(), to: ui::Delivery::Recorded });
                o.images.note(Op::Settle { answer_id });
            }
        }
        if self.surfaces.open().is_some_and(|o| o.images.has_uncommitted()) {
            self.commit_durable(None, budget)?;
        }
        // The cut.
        let Some(o) = self.surfaces.open() else {
            return Err("no card store".into());
        };
        let live = o.images.live();
        let to = ss::HandoffTo {
            session_id: successor_id.map_or_else(|| live.session_id.clone(), str::to_string),
            base: agent_core::compaction::message_range_digest(&self.conv.api_messages),
        };
        let hx = ss::mint_hex128();
        let (op, successor) = ss::prepare(&live, to, &hx).map_err(|e| e.to_string())?;
        self.commit_durable(Some(op), budget)?;
        let Some(mut successor) = successor else {
            return Ok(Handoff { successor: None });
        };
        if let Err(e) = self.store_durable(Some(&mut successor), None, budget) {
            self.handoff_rollback(Some(&successor.session_id), budget);
            return Err(e);
        }
        Ok(Handoff { successor: Some(successor) })
    }

    /// Rollback (§3.11): the parent's writer removes `<successor>.ui`
    /// first, then clears `compacting`.
    fn handoff_rollback(&mut self, successor: Option<&str>, budget: Duration) {
        if let Some(id) = successor {
            if let Err(e) = self.store_durable(None, Some(id), budget) {
                tracing::warn!(session = %self.id, "agent cards: rollback couldn't remove the successor's sidecar: {e}");
            }
        }
        let Some(o) = self.surfaces.open() else {
            return;
        };
        let op = ss::rollback(&o.images.live());
        if let Err(e) = self.commit_durable(Some(op), budget) {
            tracing::warn!(session = %self.id, "agent cards: handoff rollback not durable: {e}");
        }
    }

    /// Finish (§3.11 step 3), or Rollback when the transition failed.
    pub(crate) fn handoff_finish(&mut self, h: Option<Handoff>, applied: bool) {
        let Some(h) = h else {
            return;
        };
        self.surfaces.hold_routes = false;
        let Some(budget) = self.surfaces_budget() else {
            return;
        };
        if !applied {
            let id = h.successor.as_ref().map(|s| s.session_id.clone());
            self.handoff_rollback(id.as_deref(), budget);
            return;
        }
        let Some(successor) = h.successor else {
            let Some(o) = self.surfaces.open() else {
                return;
            };
            let op = ss::finish_in_place(&o.images.live());
            if let Err(e) = self.commit_durable(Some(op), budget) {
                // Recovery finishes it: the history's base is `to.base`.
                tracing::warn!(session = %self.id, "agent cards: in-place finish not durable: {e}");
            }
            return;
        };
        // The successor's sidecar is now the authority: bind the store to
        // it and clear `prepared`; then the parent's last write, `moved_to`.
        let Store::Open(o) = &mut self.surfaces.store else {
            return;
        };
        let parent = o.images.live();
        o.store.rebind(&successor.session_id);
        o.images = Images::new(successor.clone());
        let finish = ss::finish_successor(&successor);
        if let Err(e) = self.commit_durable(Some(finish), budget) {
            // Recovery step 0 proves it active from the parent's link.
            tracing::warn!(session = %self.id, "agent cards: successor finish not durable: {e}");
        }
        let mut moved = parent.clone();
        ss::apply(&mut moved, &ss::moved(&parent));
        if let Err(e) = self.store_durable(Some(&mut moved), None, budget) {
            // The parent has `compacted_into`: its sidecar is ignored.
            tracing::warn!(session = %self.id, "agent cards: parent moved_to not durable: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_id_is_deterministic_and_well_formed() {
        let a = surface_id("20261123-0542-abcd", "toolu_01");
        assert_eq!(a, surface_id("20261123-0542-abcd", "toolu_01"));
        assert!(ui::is_host_id(&a, "s_"), "{a}");
        assert_ne!(a, surface_id("20261123-0542-abcd", "toolu_02"));
        // The separator keeps the two parts apart.
        assert_ne!(surface_id("ab", "c"), surface_id("a", "bc"));
    }

    #[test]
    fn user_text_reads_strings_and_text_blocks_only() {
        assert_eq!(user_text(&json!({"role":"user","content":"hi"})).as_deref(), Some("hi"));
        assert_eq!(
            user_text(&json!({"role":"user","content":[{"type":"text","text":"a"},{"type":"tool_result","content":"x"}]})).as_deref(),
            Some("a")
        );
        assert_eq!(user_text(&json!({"role":"user","content":[{"type":"tool_result","content":"x"}]})), None);
        assert_eq!(user_text(&json!({"role":"assistant","content":"hi"})), None);
    }
}
