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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use agent_core::core::session_surfaces::{
    self as ss, Images, Lifecycle, Loaded, Op, Outcome, Poll, Refusal, Sidecar, StoreConfig, UiFs,
    UiStore,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::tools::ui::{unavailable, SurfaceToolHandle, SurfaceToolRequest};
use crate::ui::{
    self, CardDoc, Origin, Phase, SurfaceState, UiInput, Verb, WaitMode,
};

use super::actor::SessionActor;
use super::types::SessionEventWire;

/// The catalog this build draws and answers (`synaps:card/1`).
pub const CATALOG: u32 = 1;
/// How often an in-flight commit's outcome is looked for.
const POLL_EVERY: Duration = Duration::from_millis(10);
/// §3.13 / §4.6: creations per turn and per hour, open live cards.
pub const CREATES_PER_TURN: u32 = 4;
pub const CREATES_PER_HOUR: usize = 20;
pub const OPEN_LIVE_MAX: usize = 3;

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
        }
    }

    /// Test seam: stores open on `fs` (the C2 fault model).
    pub(crate) fn use_fs(&mut self, fs: Arc<dyn UiFs>) {
        self.fs = Some(fs);
    }

    /// A commit is in flight: [`surface_tick`] polls it.
    pub(crate) fn committing(&self) -> bool {
        self.waiter.is_some()
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

    /// A turn started: the per-turn creation count restarts.
    pub(crate) fn turn_started(&mut self) {
        self.created_this_turn = 0;
    }
}

/// Wakes the actor every [`POLL_EVERY`] while a commit is in flight;
/// otherwise never.
pub(crate) async fn surface_tick(committing: bool) {
    if committing {
        tokio::time::sleep(POLL_EVERY).await;
    } else {
        std::future::pending::<()>().await;
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
    /// before the reply (T23). Returns the reply line.
    pub(crate) fn set_cards(&mut self, on: bool) -> String {
        self.config.surfaces = Some(on);
        let was = self.surfaces.enabled;
        self.surfaces.enabled = on;
        if self.runtime.is_live() {
            self.runtime.set_cards_enabled(on);
        }
        if on {
            return if was { "cards are already on".into() } else { "cards are on".into() };
        }
        if !was {
            return "cards are already off".into();
        }
        let open: Vec<String> = self
            .surfaces
            .open()
            .map(|o| o.images.live())
            .map(|live| {
                live.cards
                    .iter()
                    .filter(|c| c.state.phase == Phase::Open || !c.state.offers.is_empty())
                    .map(|c| c.state.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if open.is_empty() {
            return "cards are off".into();
        }
        if let Store::Open(o) = &mut self.surfaces.store {
            for card in open {
                o.images.note(Op::Close { card });
            }
        }
        self.surfaces_commit_noted();
        "cards are off: open cards were withdrawn".into()
    }

    /// Open the store, reading this session's sidecar (recovery, T21, runs
    /// at load; this is its first-use half). An error fails the store
    /// closed.
    fn ensure_surface_store(&mut self) -> Result<(), String> {
        match &self.surfaces.store {
            Store::Open(_) => return Ok(()),
            Store::Failed(e) => return Err(e.clone()),
            Store::Unopened => {}
        }
        let session_id = self.journal_id_now();
        let res = self.open_surface_store(&session_id);
        match res {
            Ok(open) => {
                self.surfaces.store = Store::Open(Box::new(open));
                Ok(())
            }
            Err(e) => {
                tracing::warn!(session = %self.id, "agent cards: store failed: {e}");
                self.surfaces.store = Store::Failed(e.clone());
                Err(e)
            }
        }
    }

    fn open_surface_store(&self, session_id: &str) -> Result<Open, String> {
        if !ui::is_session_id(session_id) {
            return Err("session id outside its bound".into());
        }
        let fs = match &self.surfaces.fs {
            Some(fs) => Arc::clone(fs),
            None => sessions_fs().map_err(|e| e.to_string())?,
        };
        let mut store = UiStore::open(Arc::new(AnyFs(fs)), session_id, StoreConfig::default())
            .map_err(|e| e.to_string())?;
        let loaded = ui::load::revalidated(store.read(session_id).map_err(|e| e.to_string())?);
        let facts = self.history_facts(session_id);
        let plan = ss::recover(&loaded, &facts, None, chrono::Utc::now());
        if !plan.files.is_empty() {
            // Only the store's own files: the plan names this session's.
            tracing::info!(session = %self.id, files = plan.files.len(), "agent cards: recovery file actions skipped at first use");
        }
        let durable = match plan.commit {
            Some(mut image) => {
                store.commit(&mut image).map_err(|r| format!("{r:?}"))?;
                match store.join(ss::COMMIT_BUDGET) {
                    Poll::Done(Outcome::Durable) => image,
                    other => return Err(format!("recovery commit: {other:?}")),
                }
            }
            None => match loaded {
                Loaded::Image(sc) if plan.class == ss::Class::Related => *sc,
                _ => Sidecar::new(session_id, &ss::mint_hex128(), &facts.base),
            },
        };
        Ok(Open { store, images: Images::new(durable) })
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
            UiInput::Close(id) => self.ui_close(id, reply),
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
        let budget = o.store.config().budget;
        self.surfaces.waiter = Some(Waiter::Create { reply: Some(ReplyTo::new(reply, budget)), id });
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
        }
    }

    /// The tick: read the in-flight outcome, run what waited on it, then
    /// the next queued call.
    pub(crate) fn poll_surfaces(&mut self) {
        let Store::Open(o) = &mut self.surfaces.store else {
            self.surfaces.waiter = None;
            return;
        };
        let before = o.images.projection();
        let poll = o.images.poll(&mut o.store, Duration::ZERO);
        let lifecycle = Lifecycle::of(&poll);
        let done = matches!(poll, Poll::Done(_) | Poll::Idle);
        let after = o.images.projection();
        if done {
            self.emit_card_changes(&before, &after);
        }
        let now = tokio::time::Instant::now();
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
                    _ if reply.as_ref().is_some_and(|r| now >= r.deadline) || done => Some(json!({
                        "status": "unconfirmed", "id": id,
                        "note": "Not confirmed yet: the card may still appear, and its answer would arrive as a message. Close it if you ask in plain text instead.",
                    })),
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
                    _ if reply.as_ref().is_some_and(|r| now >= r.deadline) || done => Some(json!({
                        "status": "unconfirmed", "id": id,
                        "note": "Not confirmed yet: the card may still be open.",
                    })),
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
        }
        if done {
            self.publish_presence();
            // What a stop or a downgrade noted meanwhile commits next.
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
    fn emit_card_changes(&mut self, before: &Option<Sidecar>, after: &Option<Sidecar>) {
        let Some(after) = after else {
            return;
        };
        for c in &after.cards {
            let prev = before.as_ref().and_then(|b| b.card(&c.state.id));
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
