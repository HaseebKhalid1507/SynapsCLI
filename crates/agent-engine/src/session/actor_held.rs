//! Focus hold mode (S1) in the session actor: the delivery mode and its
//! lease, the gates that hold events while the mode is `hold`, the
//! `held_*` commands and the held queries. The wire types are `held.rs`, the
//! store file is `held_store.rs`; the contract is
//! `pria-apps/.local/plans/2026-10-21-focus-side-chats-synaps-wire.md`.
//!
//! The gates (each calls into this file while the mode is `hold`):
//! - `on_queue_wake`: before the drain and before any unpark, every queued
//!   event goes to the store ([`SessionActor::hold_queue_wake`]); a parked
//!   session stays parked. Never-persist events keep their display-only
//!   path, and a completion of this session's own subagent that arrives
//!   while a turn runs still steers into that turn (or is buffered, as a
//!   busy session buffers it today).
//! - the `Done` arm: events buffered during the turn go into history, but
//!   the post-turn event trigger starts no turn, unless a released digest
//!   rode the turn's end ([`Hold::digest_buffered`]).
//! - `send_work_queued_behind_stop`: the same for a capacity stop's resume.
//! - `note_in_main_conversation`: the note is held.
//!
//! The lease: a timer in the actor's select loop. When it runs out the mode
//! becomes `auto` and every held/join_next item is released as ONE digest
//! (at most one turn, by the idle-wake rule). At actor start an unexpired
//! hold comes back (`restored`); an expired one is released at once.
//!
//! join_next: the items (state `join_next`) and the app's texts go in front
//! of the next user message that starts a turn (a `Submit` from any client,
//! or a message queued behind a turn or a stop), as its FIRST text block:
//! ONE `held_join` envelope whose body is each item's own envelope as today,
//! then the texts ([`SessionActor::held_join`]). The texts live in memory
//! only: a reload loses a text not yet joined (the items stay in the store).

use std::time::Duration;

use super::actor::SessionActor;
use super::held::*;
use super::held_store::{self, Entry, HeldStore};
use super::types::*;
use crate::engine::reactor::{
    auto_turn_cap_reached, wake_action_with_cap, DrainedEvent, EventDisposition, WakeAction,
};
use crate::events::types::{Event, EventContent, EventSource, Severity};

/// The actor's hold state: the store (mode, lease, items), and what lives
/// in memory only.
pub(crate) struct Hold {
    pub(crate) store: HeldStore,
    /// When the lease runs out, on the tokio clock (while `hold`; also
    /// `now` at start for a lease that ran out while no actor ran).
    pub(crate) lease_deadline: Option<tokio::time::Instant>,
    /// A released digest was steered or buffered into the running turn:
    /// that turn's end may start its turn although the mode is `hold`.
    pub(crate) digest_buffered: bool,
    /// `held_release` join_next texts not joined yet, in order (memory
    /// only).
    pub(crate) join_texts: Vec<String>,
}

/// A join block ready to go in front of a user message
/// ([`SessionActor::held_join`]); [`SessionActor::held_join_committed`]
/// once it is in.
pub(crate) struct Join {
    /// The block's text: the `held_join` envelope.
    pub(crate) block: String,
    /// The `held_join` mark for the stream (§3.1).
    event: Event,
    /// The joined items, arrival order.
    ids: Vec<String>,
}

/// A user message's content with the join block in front: the block, then
/// the user's text (its own block, when not empty), then `rest`.
pub(crate) fn content_with_join(
    block: &str,
    text: &str,
    rest: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut blocks = Vec::with_capacity(rest.len() + 2);
    blocks.push(serde_json::json!({ "type": "text", "text": block }));
    if !text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
    }
    blocks.extend(rest);
    serde_json::Value::Array(blocks)
}

impl Hold {
    /// The store of session `session_id` under the base dir (no profile),
    /// read when the actor starts ([`SessionActor::held_restore`]).
    pub(crate) fn new(session_id: &str) -> Self {
        let dir = HeldStore::dir_under(&crate::config::base_dir());
        Self {
            store: HeldStore::unopened(HeldStore::path_in(&dir, session_id)),
            lease_deadline: None,
            digest_buffered: false,
            join_texts: Vec::new(),
        }
    }

    pub(crate) fn is_hold(&self) -> bool {
        self.store.mode() == DeliveryMode::Hold
    }

    /// Ids whose state passes `keep`, oldest first (arrival, then store
    /// order).
    fn ids_where(&self, keep: impl Fn(HeldState) -> bool) -> Vec<String> {
        let mut picked: Vec<(u64, usize, String)> = self
            .store
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, e)| keep(e.item.state))
            .map(|(i, e)| (e.item.ts_ms, i, e.item.id.clone()))
            .collect();
        picked.sort();
        picked.into_iter().map(|(_, _, id)| id).collect()
    }

    /// `ids` in arrival order, each once.
    fn in_order(&self, ids: &[String]) -> Vec<String> {
        self.ids_where(|_| true).into_iter().filter(|id| ids.contains(id)).collect()
    }
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

fn deadline_for(lease_until_ms: u64, now_ms: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_millis(lease_until_ms.saturating_sub(now_ms))
}

/// The lease timer: pends forever without a lease.
pub(crate) async fn lease_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn severity_of(s: HeldSeverity) -> Severity {
    match s {
        HeldSeverity::Low => Severity::Low,
        HeldSeverity::Medium => Severity::Medium,
        HeldSeverity::High => Severity::High,
        HeldSeverity::Critical => Severity::Critical,
    }
}

/// The event `entry` came from, as received; a fold item (no event of its
/// own) gets one from its own fields, source `synaps`/`held`.
fn item_event(entry: &Entry) -> Event {
    match &entry.event {
        Some(event) => event.clone(),
        None => held_event(&entry.item.kind, entry.text.clone(), entry.item.severity, None),
    }
}

/// An event the daemon writes for a digest, a join or a fold (§3.1):
/// source `synaps`/`held`, never sent in from outside (the reserved types).
pub(crate) fn held_event(
    content_type: &str,
    text: String,
    severity: HeldSeverity,
    data: Option<serde_json::Value>,
) -> Event {
    Event {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now(),
        source: EventSource {
            source_type: "synaps".to_string(),
            name: "held".to_string(),
            callback: None,
        },
        channel: None,
        sender: None,
        content: EventContent {
            text,
            content_type: content_type.to_string(),
            severity: Some(severity_of(severity)),
            data,
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
        peer: None,
    }
}

impl SessionActor {
    fn held_refuse(&mut self, from: Option<ClientId>, command: &str, reason: String) {
        tracing::info!(session = %self.id, command, reason = %reason, "hold: refused");
        match from {
            Some(client) => self.emit(SessionEventWire::Refused {
                client,
                command: command.to_string(),
                reason,
            }),
            None => self.emit(SessionEventWire::SystemNotice(reason)),
        }
    }

    /// Actor start: an unexpired hold comes back (`restored`); a lease that
    /// ran out while no actor ran is released by the lease timer at once.
    pub(crate) fn held_restore(&mut self) {
        let now = now_ms();
        let path = self.hold.store.path().to_path_buf();
        self.hold.store = HeldStore::open(path, now);
        if !self.hold.is_hold() {
            return;
        }
        match self.hold.store.lease_until_ms() {
            Some(until) if until > now => {
                tracing::info!(session = %self.id, lease_until_ms = until, "hold: restored");
                self.hold.lease_deadline = Some(deadline_for(until, now));
                self.emit(SessionEventWire::DeliveryChanged {
                    mode: DeliveryMode::Hold,
                    lease_until_ms: Some(until),
                    reason: DeliveryReason::Restored,
                });
            }
            _ => {
                tracing::info!(session = %self.id, "hold: the lease ran out while down; releasing");
                self.hold.lease_deadline = Some(tokio::time::Instant::now());
            }
        }
    }

    /// GATE (`on_queue_wake` while `hold`): every queued event goes to the
    /// store, without unparking. See the module doc for the two exceptions.
    pub(crate) fn hold_queue_wake(&mut self) {
        let busy = self.busy();
        let mut into_turn = false;
        while let Some(event) = self.event_queue.pop() {
            let class = event.content.disclosure.unwrap_or_default();
            if !agent_core::disclosure::may_persist(class) {
                // Display-only, as `drain_event_queue` treats it.
                self.emit(SessionEventWire::External(event));
                continue;
            }
            let own = self.own_subagent_completion(&event);
            if own && busy {
                let formatted = crate::events::format_event_for_agent(&event);
                let steered = self
                    .steer_tx
                    .as_ref()
                    .is_some_and(|tx| tx.send(formatted.clone()).is_ok());
                if steered {
                    self.turn_steered_events.push(formatted);
                } else {
                    self.conv.pending_events.push(formatted);
                }
                self.emit(SessionEventWire::External(event));
                into_turn = true;
                continue;
            }
            self.hold_event(event, own);
        }
        if into_turn {
            self.emit_conversation();
        }
    }

    /// A completion wake of a worker in this session's registry.
    fn own_subagent_completion(&self, event: &Event) -> bool {
        if event.content.content_type != "subagent_completion" || !self.runtime.is_live() {
            return false;
        }
        let Some(id) = event
            .content
            .data
            .as_ref()
            .and_then(|d| d.get("handle_id"))
            .and_then(|v| v.as_str())
        else {
            return false;
        };
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .is_some()
    }

    /// Put `event` in the store and tell every client.
    pub(crate) fn hold_event(&mut self, event: Event, own_subagent: bool) {
        let now = now_ms();
        let (item, text) = held_store::item_for_event(&event, own_subagent, now);
        let added = self.hold.store.add(
            Entry {
                item,
                text,
                event: Some(event),
                changed_ms: now,
            },
            now,
        );
        tracing::info!(session = %self.id, held = %added.id, "hold: event held");
        if let Some(e) = self.hold.store.get(&added.id) {
            let item = e.item.clone();
            self.emit(SessionEventWire::EventHeld { item });
        }
        if let Some(fold) = added.fold {
            tracing::info!(session = %self.id, folded = fold.folded.len(), "hold: past the bound, folded");
            self.emit(SessionEventWire::HeldUpdated {
                ids: fold.folded,
                state: HeldState::Released,
            });
            self.emit(SessionEventWire::EventHeld { item: fold.item });
        }
    }

    /// The lease timer fired.
    pub(crate) async fn on_held_lease_due(&mut self) {
        self.hold.lease_deadline = None;
        if !self.hold.is_hold() {
            return;
        }
        let now = now_ms();
        if let Some(until) = self.hold.store.lease_until_ms() {
            if until > now {
                // The wall clock says not yet (the tokio clock ran ahead).
                self.hold.lease_deadline = Some(deadline_for(until, now));
                return;
            }
        }
        self.hold.store.set_mode(DeliveryMode::Auto, None, now);
        tracing::info!(session = %self.id, "hold: the lease ran out; auto");
        self.emit(SessionEventWire::DeliveryChanged {
            mode: DeliveryMode::Auto,
            lease_until_ms: None,
            reason: DeliveryReason::LeaseExpired,
        });
        let ids = self.hold.ids_where(HeldState::is_pending);
        if ids.is_empty() {
            return;
        }
        if let Err(e) = self.ensure_live().await {
            tracing::warn!(session = %self.id, error = %e, "hold: the lease ran out but the session could not unpark; items stay held");
            return;
        }
        self.deliver_digest(ids, None, "lease_expired").await;
    }

    /// ONE digest of `ids` (arrival order; they become released): one user
    /// message in the event envelope (`held_digest`), going in like one
    /// drained event: steered or buffered while busy, injected while idle,
    /// then at most one turn by the idle-wake rule.
    async fn deliver_digest(&mut self, ids: Vec<String>, text: Option<String>, reason: &str) {
        let mut severity = HeldSeverity::Medium;
        let mut lines = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(e) = self.hold.store.get(id) {
                let i = &e.item;
                severity = severity.max(i.severity);
                let mut line = format!("- [{}] {}: {} ({})", i.severity.as_str(), i.source, i.headline, i.id);
                if let (HeldState::HandledOnSide, Some(side)) = (i.state, i.side_session.as_deref()) {
                    line.push_str(&format!(" (handled on the side: {side})"));
                }
                lines.push(line);
            }
        }
        let body = text.unwrap_or_else(|| lines.join("\n"));
        let event = held_event(
            HELD_DIGEST_TYPE,
            body,
            severity,
            Some(serde_json::json!({ "held_ids": ids, "reason": reason })),
        );
        let formatted = crate::events::format_event_for_agent(&event);
        let released = self.hold.store.set_state(&ids, HeldState::Released, None, now_ms());
        tracing::info!(session = %self.id, items = released.len(), reason, "hold: digest released");
        self.emit(SessionEventWire::HeldUpdated {
            ids: released,
            state: HeldState::Released,
        });
        if self.busy() {
            let steered = self
                .steer_tx
                .as_ref()
                .is_some_and(|tx| tx.send(formatted.clone()).is_ok());
            if steered {
                self.turn_steered_events.push(formatted);
            } else {
                self.conv.pending_events.push(formatted);
            }
            self.hold.digest_buffered = true;
            self.emit(SessionEventWire::External(event));
            self.emit_conversation();
            return;
        }
        self.conv
            .api_messages
            .push(std::sync::Arc::new(serde_json::json!({ "role": "user", "content": formatted.clone() })));
        self.request_save();
        self.emit(SessionEventWire::External(event.clone()));
        self.emit_conversation();
        let drained = [DrainedEvent {
            event,
            formatted,
            disposition: EventDisposition::Injected,
        }];
        let enabled = self.host.config().events.auto_turn;
        let cap = self.host.config().events.auto_turn_cap;
        match wake_action_with_cap(&drained, &self.conv.api_messages, false, enabled, self.consecutive_auto_turns, cap) {
            WakeAction::RunTurn => {
                if self.driver.is_some() {
                    tracing::debug!("hold digest: RunTurn inhibited — driver armed");
                } else if self.streaming || self.stream.is_some() {
                    tracing::warn!("hold digest: RunTurn with active turn — skipping");
                } else {
                    self.consecutive_auto_turns += 1;
                    self.start_turn(TurnTrigger::EventAuto, None).await;
                }
            }
            WakeAction::Forward => {
                if enabled && auto_turn_cap_reached(self.consecutive_auto_turns, cap) {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap });
                }
            }
            WakeAction::Nothing => {}
        }
    }

    /// `Delivery`, `HeldRelease`, `HeldDismiss`, `HeldMark`.
    pub(crate) async fn held_command(&mut self, from: Option<ClientId>, cmd: SessionCommand) {
        match cmd {
            SessionCommand::Delivery { mode, lease_secs } => self.set_delivery(from, mode, lease_secs),
            SessionCommand::HeldRelease { ids, release_as, text } => {
                self.held_release(from, ids, release_as, text).await
            }
            SessionCommand::HeldDismiss { ids } => self.held_dismiss(from, ids),
            SessionCommand::HeldMark { id, side_session } => self.held_mark(from, id, side_session),
            _ => {}
        }
    }

    /// The first of `ids` that is unknown or already released (delivered
    /// once), for the refusal.
    fn not_open<'a>(&self, ids: &'a [String]) -> Option<&'a String> {
        ids.iter()
            .find(|id| !matches!(self.hold.store.get(id), Some(e) if e.item.state != HeldState::Released))
    }

    /// `held_dismiss`: dismissed; nothing is deleted (the store keeps the
    /// text for 7 days).
    fn held_dismiss(&mut self, from: Option<ClientId>, ids: Vec<String>) {
        if let Some(bad) = self.not_open(&ids) {
            return self.held_refuse(from, "held_dismiss", format!("{UNKNOWN_HELD_ID}: {bad}"));
        }
        let ids = self
            .hold
            .store
            .set_state(&ids, HeldState::Dismissed, None, now_ms());
        tracing::info!(session = %self.id, items = ids.len(), "hold: dismissed");
        self.emit(SessionEventWire::HeldUpdated {
            ids,
            state: HeldState::Dismissed,
        });
    }

    /// `held_mark`: handled on the side, in session `side_session`.
    fn held_mark(&mut self, from: Option<ClientId>, id: String, side_session: String) {
        let ids = vec![id];
        if let Some(bad) = self.not_open(&ids) {
            return self.held_refuse(from, "held_mark", format!("{UNKNOWN_HELD_ID}: {bad}"));
        }
        let ids = self.hold.store.set_state(
            &ids,
            HeldState::HandledOnSide,
            Some(&side_session),
            now_ms(),
        );
        tracing::info!(session = %self.id, "hold: handled on the side");
        self.emit(SessionEventWire::HeldUpdated {
            ids,
            state: HeldState::HandledOnSide,
        });
    }

    fn set_delivery(&mut self, from: Option<ClientId>, mode: DeliveryMode, lease_secs: Option<u64>) {
        let now = now_ms();
        match mode {
            DeliveryMode::Hold => {
                let Some(secs) = lease_secs.filter(|s| (LEASE_SECS_MIN..=LEASE_SECS_MAX).contains(s)) else {
                    return self.held_refuse(
                        from,
                        "delivery",
                        format!("hold needs lease_secs {LEASE_SECS_MIN}..={LEASE_SECS_MAX}"),
                    );
                };
                let reason = if self.hold.is_hold() {
                    DeliveryReason::Renewed
                } else {
                    DeliveryReason::Set
                };
                let until = now + secs * 1000;
                self.hold.store.set_mode(DeliveryMode::Hold, Some(until), now);
                self.hold.lease_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                tracing::info!(session = %self.id, lease_secs = secs, ?reason, "hold: on");
                self.emit(SessionEventWire::DeliveryChanged {
                    mode: DeliveryMode::Hold,
                    lease_until_ms: Some(until),
                    reason,
                });
            }
            DeliveryMode::Auto => {
                // Releases nothing by itself.
                self.hold.store.set_mode(DeliveryMode::Auto, None, now);
                self.hold.lease_deadline = None;
                tracing::info!(session = %self.id, "hold: auto");
                self.emit(SessionEventWire::DeliveryChanged {
                    mode: DeliveryMode::Auto,
                    lease_until_ms: None,
                    reason: DeliveryReason::Set,
                });
            }
        }
    }

    async fn held_release(
        &mut self,
        from: Option<ClientId>,
        ids: Option<Vec<String>>,
        release_as: ReleaseAs,
        text: Option<String>,
    ) {
        const CMD: &str = "held_release";
        if text.as_ref().is_some_and(|t| t.len() > RELEASE_TEXT_MAX_BYTES) {
            return self.held_refuse(from, CMD, format!("text is longer than {RELEASE_TEXT_MAX_BYTES} bytes"));
        }
        if release_as == ReleaseAs::Individual && text.is_some() {
            return self.held_refuse(from, CMD, "text is not allowed with individual".to_string());
        }
        if release_as == ReleaseAs::Individual && self.hold.is_hold() {
            return self.held_refuse(from, CMD, "individual is refused while the mode is hold".to_string());
        }
        let ids = match ids {
            None => self.hold.ids_where(HeldState::is_releasable_by_all),
            Some(ids) => {
                if let Some(bad) = self.not_open(&ids) {
                    return self.held_refuse(from, CMD, format!("{UNKNOWN_HELD_ID}: {bad}"));
                }
                self.hold.in_order(&ids)
            }
        };
        match release_as {
            ReleaseAs::Digest => {
                if ids.is_empty() && text.is_none() {
                    self.emit(SessionEventWire::HeldUpdated {
                        ids: Vec::new(),
                        state: HeldState::Released,
                    });
                    return;
                }
                self.deliver_digest(ids, text, "release").await;
            }
            ReleaseAs::JoinNext => {
                let ids = self
                    .hold
                    .store
                    .set_state(&ids, HeldState::JoinNext, None, now_ms());
                if let Some(text) = text {
                    self.hold.join_texts.push(text);
                }
                tracing::info!(session = %self.id, items = ids.len(), texts = self.hold.join_texts.len(), "hold: join next");
                self.emit(SessionEventWire::HeldUpdated {
                    ids,
                    state: HeldState::JoinNext,
                });
            }
            ReleaseAs::Individual => self.release_individual(ids),
        }
    }

    /// `individual` (auto only): each item's event goes back into the
    /// event queue in arrival order and is delivered as today. An item the
    /// full queue refuses stays as it is.
    fn release_individual(&mut self, ids: Vec<String>) {
        let mut pushed = Vec::with_capacity(ids.len());
        for id in &ids {
            let Some(event) = self.hold.store.get(id).map(item_event) else {
                continue;
            };
            match self.event_queue.push(event) {
                Ok(()) => pushed.push(id.clone()),
                Err(e) => {
                    tracing::warn!(session = %self.id, held = %id, error = %e, "hold: individual release refused by the event queue; the item stays");
                    break;
                }
            }
        }
        let released = self
            .hold
            .store
            .set_state(&pushed, HeldState::Released, None, now_ms());
        tracing::info!(session = %self.id, items = released.len(), "hold: released individually");
        self.emit(SessionEventWire::HeldUpdated {
            ids: released,
            state: HeldState::Released,
        });
    }

    /// The join block for the next user message, when join_next items or
    /// texts wait: ONE `held_join` envelope (its data `{held_ids,
    /// has_text}`) whose body is each item's own envelope exactly as today,
    /// in arrival order, then each text, sanitized like an envelope's text.
    /// Nothing changes until [`Self::held_join_committed`].
    pub(crate) fn held_join(&self) -> Option<Join> {
        let ids = self.hold.ids_where(|s| s == HeldState::JoinNext);
        if ids.is_empty() && self.hold.join_texts.is_empty() {
            return None;
        }
        let mut severity = HeldSeverity::Medium;
        let mut parts = Vec::with_capacity(ids.len() + self.hold.join_texts.len());
        for id in &ids {
            if let Some(e) = self.hold.store.get(id) {
                severity = severity.max(e.item.severity);
                parts.push(crate::events::format_event_for_agent(&item_event(e)));
            }
        }
        for text in &self.hold.join_texts {
            parts.push(crate::events::format::sanitize_text(text));
        }
        let body = parts.join("\n");
        let has_text = !self.hold.join_texts.is_empty();
        let mut event = held_event(
            HELD_JOIN_TYPE,
            String::new(),
            severity,
            Some(serde_json::json!({ "held_ids": ids, "has_text": has_text })),
        );
        // The envelope around an empty text; the body goes in after its
        // header, which ends at its first `>` (attribute values never hold
        // one).
        let shell = crate::events::format_event_for_agent(&event);
        let at = shell.find('>').map_or(shell.len(), |i| i + 1);
        let block = format!("{}{}{}", &shell[..at], body, &shell[at..]);
        event.content.text = body;
        Some(Join { block, event, ids })
    }

    /// `join` went in front of a user message: its items are released, its
    /// texts are spent, and the `held_join` mark goes out (the caller starts
    /// the turn right after, so it comes just before `TurnStarted`).
    pub(crate) fn held_join_committed(&mut self, join: Join) {
        self.hold.join_texts.clear();
        let released = self
            .hold
            .store
            .set_state(&join.ids, HeldState::Released, None, now_ms());
        tracing::info!(session = %self.id, items = released.len(), "hold: joined the next user message");
        if !released.is_empty() {
            self.emit(SessionEventWire::HeldUpdated {
                ids: released,
                state: HeldState::Released,
            });
        }
        self.emit(SessionEventWire::External(join.event));
    }

    /// A queued user text's message content, with the join block in front
    /// when one waits (then committed).
    pub(crate) fn queued_user_content(&mut self, text: String) -> serde_json::Value {
        match self.held_join() {
            None => serde_json::Value::String(text),
            Some(join) => {
                let content = content_with_join(&join.block, &text, Vec::new());
                self.held_join_committed(join);
                content
            }
        }
    }

    /// `HeldList`, `HeldShow`.
    pub(crate) fn held_query(&mut self, query: SessionQuery) -> serde_json::Value {
        match query {
            SessionQuery::HeldList => serde_json::to_value(self.hold.store.list()).unwrap_or_default(),
            SessionQuery::HeldShow { held_id } => match self.hold.store.get(&held_id) {
                Some(e) => serde_json::to_value(HeldItemWithText {
                    item: e.item.clone(),
                    text: e.text.clone(),
                })
                .unwrap_or_default(),
                None => serde_json::json!({ "error": UNKNOWN_HELD_ID }),
            },
            _ => serde_json::Value::Null,
        }
    }
}
