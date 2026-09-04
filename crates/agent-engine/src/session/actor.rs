//! `SessionActor` — owns THE `Runtime` + `ConversationState` for one
//! conversation and runs its turn machine (PLAN-phase2 §2.5).
//!
//! Every method body is moved from an existing reactor site (TUI
//! `dispatch.rs` Abort/Submit/StreamingInput, `stream_handler.rs`
//! event-queue + stream arms, `tui/mod.rs` teardown, `cmd/chat.rs`
//! post-turn); the presentation halves stay client-side and are fed by
//! the envelopes this actor emits. Each `StreamEvent` is forwarded to
//! clients BEFORE the actor acts on it, so a client sees the same order it
//! sees today.
//!
//! Invariants:
//! - `Runtime::clone()` resets TTL latches — the actor never clones it.
//! - `emit` is the ONLY `seq` increment site.
//! - `Detach` never touches `stream`/`cancel`; only `End` (and `Cancel`)
//!   stop a turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::engine::reactor::{
    claim_auto_turn, drain_event_queue, wake_action, EventDisposition, WakeAction, AUTO_TURN_CAP,
};
use crate::engine::session::ConversationState;
use crate::engine::setup::BackgroundTasks;
use crate::runtime::compaction::{
    apply_compaction, compact_conversation, preview_compaction_disclosure, CompactionPolicy,
    CompactionTransition,
};
use crate::tools::{SecretPromptHandle, SecretPromptRequest};
use crate::{
    AgentEvent, CancellationToken, EngineHost, LlmEvent, Result, Runtime, SessionEvent,
    StreamEvent,
};

use super::budgets;
use super::handle::{SessionEndpoints, SessionHandle};
use super::types::*;
use super::view::RuntimeView;

/// The in-flight response stream (tui/stream_handler.rs `ActiveStream`).
pub type ActiveStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// `turn_replay` cap (envelopes). §6 #9: the 2 MiB text bound is day 3.
const TURN_REPLAY_CAP: usize = 4096;

/// Chronological record of the current turn's assistant output, mirroring
/// what `App::capture_abort_context` walks in the TUI transcript
/// (`ChatMessage::{Thinking,Text,ToolUse,ToolResult}` since the last user
/// message). Consecutive text/thinking deltas coalesce like
/// `append_or_update_*` does; tool-result deltas accumulate per `tool_id`
/// at the position of the first delta and the final `ToolResult` replaces
/// them in place (`Transcript::on_tool_result_delta`/`on_tool_result`), so
/// an abort mid-tool captures the partial output exactly as the TUI does.
///
/// Known divergence from the TUI (documented, not closed): the TUI walks
/// its transcript back to the last `ChatMessage::User` card. During an
/// event-triggered auto-turn there is no User card, so the TUI's context
/// also includes the PREVIOUS turn's output; `TurnLog` is cleared at every
/// `start_turn` and holds the current turn only. The actor's content is
/// the narrower, arguably correct one.
#[derive(Default)]
pub(crate) struct TurnLog {
    parts: Vec<TurnPart>,
}

pub(crate) enum TurnPart {
    Thinking(String),
    Text(String),
    ToolUse { name: String, input: String },
    ToolResult { tool_id: String, content: String },
}

impl TurnLog {
    fn clear(&mut self) {
        self.parts.clear();
    }

    fn tool_result_mut(&mut self, tool_id: &str) -> Option<&mut String> {
        self.parts.iter_mut().rev().find_map(|p| match p {
            TurnPart::ToolResult { tool_id: id, content } if id == tool_id => Some(content),
            _ => None,
        })
    }

    /// `Transcript::on_tool_result_delta`: append to the in-flight result
    /// for this tool, or open one.
    fn tool_result_delta(&mut self, tool_id: String, delta: String) {
        match self.tool_result_mut(&tool_id) {
            Some(c) => c.push_str(&delta),
            None => self.parts.push(TurnPart::ToolResult {
                tool_id,
                content: delta,
            }),
        }
    }

    /// `Transcript::on_tool_result`: the final result replaces any
    /// delta-buffered content in place.
    fn tool_result(&mut self, tool_id: String, result: String) {
        match self.tool_result_mut(&tool_id) {
            Some(c) => *c = result,
            None => self.parts.push(TurnPart::ToolResult {
                tool_id,
                content: result,
            }),
        }
    }

    fn text(&mut self, t: &str) {
        if let Some(TurnPart::Text(s)) = self.parts.last_mut() {
            s.push_str(t);
        } else {
            self.parts.push(TurnPart::Text(t.to_string()));
        }
    }

    fn thinking(&mut self, t: &str) {
        if let Some(TurnPart::Thinking(s)) = self.parts.last_mut() {
            s.push_str(t);
        } else {
            self.parts.push(TurnPart::Thinking(t.to_string()));
        }
    }

    /// `App::capture_abort_context` verbatim (app.rs:644-683).
    fn abort_context(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for p in &self.parts {
            match p {
                TurnPart::Thinking(t) if !t.is_empty() => {
                    let preview: String = t.chars().take(500).collect();
                    parts.push(format!("[thinking]: {}", preview));
                }
                TurnPart::Text(t) if !t.is_empty() => {
                    parts.push(format!("[response]: {}", t));
                }
                TurnPart::ToolUse { name, input } => {
                    let input_preview: String = input.chars().take(200).collect();
                    parts.push(format!("[tool_use]: {} — {}", name, input_preview));
                }
                TurnPart::ToolResult { content, .. } if !content.is_empty() => {
                    let preview: String = content.chars().take(300).collect();
                    parts.push(format!("[tool_result]: {}", preview));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]",
            parts.join("\n")
        ))
    }
}

/// A field that is `None` exactly while the session is `Parked` (B3).
/// Derefs to the value so the turn machine reads `self.runtime` / `self.conv`
/// unchanged; every command that needs them goes through `ensure_live()`
/// first, so the panic path is a programming error, not a runtime state.
pub(crate) struct Live<T>(Option<T>);

impl<T> Live<T> {
    pub(crate) fn new(v: T) -> Self {
        Self(Some(v))
    }
    pub(crate) fn is_live(&self) -> bool {
        self.0.is_some()
    }
    pub(crate) fn park_take(&mut self) -> Option<T> {
        self.0.take()
    }
    pub(crate) fn unpark_set(&mut self, v: T) {
        self.0 = Some(v);
    }
}

impl<T> std::ops::Deref for Live<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.as_ref().expect("session is parked: runtime/conv unavailable")
    }
}

impl<T> std::ops::DerefMut for Live<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("session is parked: runtime/conv unavailable")
    }
}

/// `SYNAPS_DAEMON_PARK_GRACE_SECS`: `never` → `None` (Parked disabled);
/// `n` → n seconds after the last detach once idle; default 60.
pub fn park_grace() -> Option<std::time::Duration> {
    match std::env::var("SYNAPS_DAEMON_PARK_GRACE_SECS") {
        Ok(v) if v.trim().eq_ignore_ascii_case("never") => None,
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_secs)
            .or(Some(DEFAULT_PARK_GRACE)),
        Err(_) => Some(DEFAULT_PARK_GRACE),
    }
}

pub const DEFAULT_PARK_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Non-persisted runtime knobs replayed after unpark (last-wins per
/// variant). Model/reasoning/system prompt live in the journal already.
fn replayable(setting: &SessionSetting) -> bool {
    matches!(
        setting,
        SessionSetting::ContextWindow { .. }
            | SessionSetting::CompactionModel { .. }
            | SessionSetting::ApiRetries { .. }
            | SessionSetting::SubagentTimeout { .. }
            | SessionSetting::MaxToolOutput { .. }
            | SessionSetting::BashTimeout { .. }
            | SessionSetting::BashMaxTimeout { .. }
            | SessionSetting::GrantWorkerModel { .. }
    )
}

fn replay_setting(rt: &mut Runtime, setting: &SessionSetting) {
    match setting {
        SessionSetting::ContextWindow { tokens } => rt.set_context_window(*tokens),
        SessionSetting::CompactionModel { model } => rt.set_compaction_model(model.clone()),
        SessionSetting::ApiRetries { n } => rt.set_api_retries(*n),
        SessionSetting::SubagentTimeout { secs } => rt.set_subagent_timeout(*secs),
        SessionSetting::MaxToolOutput { bytes } => rt.set_max_tool_output(*bytes),
        SessionSetting::BashTimeout { secs } => rt.set_bash_timeout(*secs),
        SessionSetting::BashMaxTimeout { secs } => rt.set_bash_max_timeout(*secs),
        SessionSetting::GrantWorkerModel { model } => {
            let _ = rt.grant_worker_model(model);
        }
        _ => {}
    }
}

/// One attached client: its meta + the mode it attached with (B1 ownership
/// needs the mode to pick the next owner when the owner detaches).
pub(crate) struct AttachedClient {
    pub(crate) meta: ClientMeta,
    pub(crate) mode: AttachMode,
}

/// B2: a spawned `compact_conversation` (§2.5).
pub(crate) struct CompactionJob {
    pub(crate) task: tokio::task::JoinHandle<Result<crate::runtime::compaction::CompactionOutcome>>,
    pub(crate) source: String,
    #[allow(dead_code)]
    pub(crate) started: tokio::time::Instant,
}

/// Commands that only the input owner may send (B1). Everything else
/// (`Answer`, `Query`, `Save`, `Detach`, `Attach`, `Resync`, `HostEvent`)
/// is open to every attached client.
fn is_input_command(cmd: &SessionCommand) -> bool {
    matches!(
        cmd,
        SessionCommand::Submit { .. }
            | SessionCommand::SubmitPrepared { .. }
            | SessionCommand::Steer { .. }
            | SessionCommand::Cancel
            | SessionCommand::Set { .. }
            | SessionCommand::Compact { .. }
            | SessionCommand::NewSession
            | SessionCommand::EngineCommand { .. }
            | SessionCommand::PluginCommand { .. }
            | SessionCommand::Resume { .. }
            | SessionCommand::KeepWarm { .. }
            | SessionCommand::End {
                reason: EndReason::ClientQuit
            }
    )
}

fn command_name(cmd: &SessionCommand) -> &'static str {
    match cmd {
        SessionCommand::Submit { .. } => "submit",
        SessionCommand::SubmitPrepared { .. } => "submit_prepared",
        SessionCommand::Steer { .. } => "steer",
        SessionCommand::Cancel => "cancel",
        SessionCommand::Answer { .. } => "answer",
        SessionCommand::Set { .. } => "set",
        SessionCommand::Compact { .. } => "compact",
        SessionCommand::NewSession => "new_session",
        SessionCommand::Save => "save",
        SessionCommand::Query { .. } => "query",
        SessionCommand::Attach { .. } => "attach",
        SessionCommand::Detach { .. } => "detach",
        SessionCommand::End { .. } => "end",
        SessionCommand::Resync { .. } => "resync",
        SessionCommand::EngineCommand { .. } => "engine_command",
        SessionCommand::PluginCommand { .. } => "plugin_command",
        SessionCommand::Resume { .. } => "resume",
        SessionCommand::Checkpoint { .. } => "checkpoint",
        SessionCommand::KeepWarm { .. } => "keep_warm",
        SessionCommand::HostEvent(_) => "host_event",
    }
}

pub struct SessionActor {
    pub(crate) id: SessionId,
    pub(crate) meta: SessionMeta,
    pub(crate) config: SessionConfig,
    /// THE runtime; `session_id` + `cwd` set by `create`. `None` ⇔ Parked.
    pub(crate) runtime: Live<Runtime>,
    /// `None` ⇔ Parked.
    pub(crate) conv: Live<ConversationState>,
    /// B3: unpark needs a fresh `foreground_runtime()`.
    pub(crate) host: Arc<EngineHost>,
    /// Kept across Park (`finish()` needs it without a Runtime).
    pub(crate) hook_bus: Arc<crate::extensions::hooks::HookBus>,
    /// Session-lifetime queue; `background` pushes into it, every Runtime
    /// this actor builds is pointed at it (`Runtime::set_event_queue`).
    pub(crate) event_queue: Arc<crate::events::EventQueue>,
    /// Last-wins per variant; replayed after unpark (non-persisted knobs).
    pub(crate) settings_replay: Vec<SessionSetting>,
    pub(crate) keep_warm: bool,
    /// Armed on last-detach-while-idle / idle-while-detached.
    pub(crate) park_deadline: Option<tokio::time::Instant>,
    // ── turn machine: the run() loop locals + App fields ──
    pub(crate) stream: Option<ActiveStream>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) steer_tx: Option<mpsc::UnboundedSender<String>>,
    pub(crate) streaming: bool,
    pub(crate) turn_baseline: usize,
    pub(crate) consecutive_auto_turns: u32,
    pub(crate) turn_log: TurnLog,
    // ── prompts ──
    pub(crate) secret_prompt_handle: SecretPromptHandle,
    pub(crate) secret_prompt_rx: mpsc::UnboundedReceiver<SecretPromptRequest>,
    /// Held across detach; replayed in `AttachSnapshot.pending_prompts`.
    pub(crate) pending_prompts: VecDeque<(PromptRequest, oneshot::Sender<Option<String>>)>,
    pub(crate) next_prompt_id: u64,
    // ── clients ──
    pub(crate) cmd_rx: mpsc::Receiver<Addressed>,
    pub(crate) events: broadcast::Sender<Envelope>,
    pub(crate) view: Arc<arc_swap::ArcSwap<RuntimeView>>,
    pub(crate) attached: HashMap<ClientId, AttachedClient>,
    /// B1: the one client whose input commands are honoured (`None` = any
    /// host-originated sender only).
    pub(crate) input_owner: Option<ClientId>,
    pub(crate) next_client_id: u64,
    pub(crate) seq: u64,
    pub(crate) turn_replay: VecDeque<Envelope>,
    pub(crate) state: AttachState,
    pub(crate) background: BackgroundTasks,
    /// B2: spawned compaction in flight (`busy` while `Some`).
    pub(crate) compact: Option<CompactionJob>,
    /// `await_extensions=false`: fires when process-level discovery is done;
    /// the actor then emits `on_session_start` without having blocked boot.
    pub(crate) ext_ready: Option<oneshot::Receiver<()>>,
    /// 1 Hz `SubagentRows` cadence while a turn runs (`None` when idle).
    pub(crate) subagent_tick: Option<tokio::time::Interval>,
    /// Mirrors `handle.lifecycle()` (B3 writes Parking/Parked).
    pub(crate) lifecycle: Arc<std::sync::atomic::AtomicU8>,
    /// Mirrors `handle.journal_id()` (B2 stores the successor id).
    pub(crate) journal_id: Arc<arc_swap::ArcSwap<String>>,
    /// Mirrors `handle.presence()` (clients / owner / pending prompts).
    pub(crate) presence: Arc<arc_swap::ArcSwap<super::handle::Presence>>,
}

impl SessionActor {
    /// = today's `setup::boot()` per session (foreground_runtime →
    /// resolve_session_and_prompt → set_cwd → model override →
    /// spawn_session_background → finish_session_setup) then
    /// `on_session_start` (keyed injection).
    pub(crate) async fn create(
        host: &Arc<EngineHost>,
        cfg: SessionConfig,
    ) -> Result<(SessionHandle, SessionTask)> {
        let mut runtime = host.foreground_runtime().await?;
        let config: crate::SynapsConfig = (**host.config()).clone();

        let sb = crate::engine::setup::resolve_session_and_prompt(
            &mut runtime,
            &cfg.continue_session,
            cfg.system.as_deref(),
            cfg.prompt_manifest.as_deref(),
        )?;
        runtime.set_cwd(cfg.cwd.clone());
        // CLI `--model` overrides whatever was persisted (rpc.rs precedent).
        if let Some(ref m) = cfg.model_override {
            runtime.set_model(m.clone());
        }

        let background =
            crate::engine::setup::spawn_session_background(&runtime, &sb.session)?;
        crate::engine::setup::finish_session_setup(
            &mut runtime,
            &config,
            &sb.session,
            cfg.cwd.clone(),
            crate::engine::setup::IndexRecord::Start,
        );

        // C2: per-session on_session_start (keyed injection) once the
        // process-level discovery is known-finished. Never re-runs discovery.
        // `await_extensions=false` (TUI): a spawned waiter fires the
        // `ext_ready` arm instead, so boot never blocks on discovery.
        let mut ext_ready = None;
        if cfg.await_extensions {
            if tokio::time::timeout(budgets::EXTENSIONS_READY_TIMEOUT, host.extensions_ready())
                .await
                .is_err()
            {
                tracing::warn!(
                    budget_secs = budgets::EXTENSIONS_READY_TIMEOUT_SECS,
                    "extensions_ready timed out — on_session_start may miss late extensions"
                );
            }
            crate::extensions::loader::emit_session_start(runtime.hook_bus(), &sb.session.id)
                .await;
        } else {
            let (tx, rx) = oneshot::channel();
            let waiter_host = Arc::clone(host);
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    budgets::EXTENSIONS_READY_TIMEOUT,
                    waiter_host.extensions_ready(),
                )
                .await;
                let _ = tx.send(());
            });
            ext_ready = Some(rx);
        }

        let id = SessionId::from(sb.session.id.clone());
        let meta = SessionMeta {
            id: id.clone(),
            name: sb.session.name.clone(),
            model: runtime.model().to_string(),
            cwd: cfg.cwd.clone(),
            created_at: sb.session.created_at,
            continued: sb.continued,
            continue_info: sb.continue_info.as_ref().map(ContinueInfoWire::from),
            host_pid: std::process::id(),
            lifecycle: SessionLifecycle::Live,
            clients: 0,
            input_owner: None,
            awaiting_input: 0,
            journal_id: sb.session.id.clone(),
        };
        let mut conv = if sb.continued {
            ConversationState::from_resumed(sb.session)
        } else {
            ConversationState::new(sb.session)
        };
        conv.api_messages = sb.api_messages;
        conv.total_input_tokens = sb.total_input_tokens;
        conv.total_output_tokens = sb.total_output_tokens;
        conv.session_cost = sb.session_cost;
        conv.abort_context = sb.abort_context;

        let view = RuntimeView::from_runtime(&runtime).await;
        let hook_bus = Arc::clone(runtime.hook_bus());
        let event_queue = Arc::clone(runtime.event_queue());
        let keep_warm = cfg.keep_warm;
        let (
            handle,
            SessionEndpoints {
                cmd_rx,
                events,
                view,
                lifecycle,
                journal_id,
                presence,
            },
        ) = SessionHandle::new(meta.clone(), view);
        let (sp_tx, secret_prompt_rx) = mpsc::unbounded_channel();

        let actor = SessionActor {
            id,
            meta,
            config: cfg,
            runtime: Live::new(runtime),
            conv: Live::new(conv),
            host: Arc::clone(host),
            hook_bus,
            event_queue,
            settings_replay: Vec::new(),
            keep_warm,
            park_deadline: None,
            stream: None,
            cancel: None,
            steer_tx: None,
            streaming: false,
            turn_baseline: 0,
            consecutive_auto_turns: 0,
            turn_log: TurnLog::default(),
            secret_prompt_handle: SecretPromptHandle::new(sp_tx),
            secret_prompt_rx,
            pending_prompts: VecDeque::new(),
            next_prompt_id: 1,
            cmd_rx,
            events,
            view,
            attached: HashMap::new(),
            input_owner: None,
            next_client_id: 1,
            seq: 0,
            turn_replay: VecDeque::new(),
            state: AttachState::Detached { running: false },
            background,
            compact: None,
            ext_ready,
            subagent_tick: None,
            lifecycle,
            journal_id,
            presence,
        };
        Ok((handle, SessionTask(actor)))
    }

    // ── emit ─────────────────────────────────────────────────────────────

    /// The ONLY seq++ site. Pushes to `turn_replay` while streaming, except
    /// prompt traffic (never replayed) and per-client replies.
    pub(crate) fn emit(&mut self, event: SessionEventWire) {
        let replay = self.streaming
            && !matches!(
                event,
                SessionEventWire::Prompt(_)
                    | SessionEventWire::PromptResolved { .. }
                    | SessionEventWire::Attached { .. }
                    | SessionEventWire::QueryResult { .. }
            );
        let env = Envelope {
            session_id: self.id.clone(),
            seq: self.seq,
            ts: chrono::Utc::now(),
            event,
        };
        self.seq += 1;
        if replay {
            if self.turn_replay.len() >= TURN_REPLAY_CAP {
                self.turn_replay.pop_front();
            }
            self.turn_replay.push_back(env.clone());
        }
        // No receivers is not an error: streams are not tied to clients.
        let _ = self.events.send(env);
    }

    pub(crate) fn emit_conversation(&mut self) {
        let snap = self.conv.snapshot(self.consecutive_auto_turns);
        self.emit(SessionEventWire::Conversation(snap));
    }

    pub(crate) async fn publish_view(&mut self) {
        let v = RuntimeView::from_runtime(&self.runtime).await;
        self.view.store(Arc::new(v));
    }

    pub(crate) async fn save(&mut self) {
        if self.config.persist && self.conv.is_live() {
            self.conv.save().await;
        }
    }

    pub(crate) fn publish_presence(&self) {
        self.presence.store(Arc::new(super::handle::Presence {
            clients: self.attached.len(),
            input_owner: self.input_owner,
            awaiting_input: self.pending_prompts.len(),
        }));
    }

    /// Recomputes `state` from clients/streaming and (re)arms or disarms the
    /// park grace timer. A no-op while Parking/Parked.
    pub(crate) fn update_attach_state(&mut self) {
        self.publish_presence();
        if matches!(self.state, AttachState::Parking | AttachState::Parked) {
            return;
        }
        self.state = if self.attached.is_empty() {
            AttachState::Detached {
                running: self.streaming,
            }
        } else {
            AttachState::Attached(self.attached.len())
        };
        self.rearm_park();
    }

    // ── Parked (B3) ──────────────────────────────────────────────────────

    pub(crate) fn is_parked(&self) -> bool {
        matches!(self.state, AttachState::Parked)
    }

    /// A session with nothing to save (`ConversationState::save` skips an
    /// empty `api_messages`, so no journal ever exists) never parks: it
    /// costs nothing warm and could not be unparked from disk (H2).
    pub(crate) fn can_park(&self) -> bool {
        self.attached.is_empty()
            && !self.streaming
            && self.compact.is_none()
            && self.pending_prompts.is_empty()
            && self.conv.is_live()
            && !self.conv.api_messages.is_empty()
            && self.conv.queued_message.is_none()
            && !self.keep_warm
            && self.config.persist
            && !self.is_parked()
    }

    /// `<sessions>/<id>.json` — written by both persistence modes.
    fn journal_exists(&self) -> bool {
        let id = (**self.journal_id.load()).clone();
        crate::config::resolve_write_path("sessions")
            .join(format!("{id}.json"))
            .is_file()
    }

    fn rearm_park(&mut self) {
        if !self.can_park() {
            self.park_deadline = None;
            return;
        }
        match park_grace() {
            Some(grace) => {
                if self.park_deadline.is_none() {
                    self.park_deadline = Some(tokio::time::Instant::now() + grace);
                }
            }
            None => self.park_deadline = None,
        }
    }

    fn set_lifecycle(&mut self, l: SessionLifecycle) {
        self.lifecycle
            .store(l as u8, std::sync::atomic::Ordering::Release);
        self.emit(SessionEventWire::Lifecycle(l));
    }

    /// Save, close PTYs, drop `conv` THEN `runtime`. `background` (inbox
    /// watcher + per-session UDS + registry) stays: `synaps send` keeps
    /// resolving and its push into `event_queue` is the wake-up.
    pub(crate) async fn park(&mut self) {
        self.park_deadline = None;
        if !self.can_park() {
            return;
        }
        self.state = AttachState::Parking;
        self.set_lifecycle(SessionLifecycle::Parking);
        if tokio::time::timeout(budgets::SAVE_TIMEOUT, self.save())
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "park: save timed out — staying live");
            self.state = AttachState::Detached { running: false };
            self.set_lifecycle(SessionLifecycle::Live);
            return;
        }
        if !self.journal_exists() {
            // Never park what cannot be restored (H2).
            tracing::warn!(session = %self.id, "park: no journal on disk — staying live");
            self.state = AttachState::Detached { running: false };
            self.set_lifecycle(SessionLifecycle::Live);
            return;
        }
        if self.runtime.session_manager().active_count() > 0 {
            self.emit(SessionEventWire::SystemNotice(
                "parked: background shells closed".into(),
            ));
        }
        self.runtime.session_manager().shutdown_all();
        self.turn_replay.clear();
        // Drop order: conv first, then runtime — a panic between them can
        // never leave a Runtime without state to save.
        let conv = self.conv.park_take();
        drop(conv);
        let runtime = self.runtime.park_take();
        drop(runtime);
        // Hand the freed pages back: jemalloc otherwise keeps them as
        // dirty/muzzy for its decay window, and a parked session that
        // still shows in RssAnon buys nothing.
        crate::core::memstat::purge_arenas();
        self.state = AttachState::Parked;
        self.set_lifecycle(SessionLifecycle::Parked);
        tracing::info!(session = %self.id, "session parked");
    }

    /// Rebuild `runtime` + `conv` from the journal (`load_session_in_dir`
    /// via `resolve_session_and_prompt`), replay non-persisted settings,
    /// `finish_session_setup(IndexRecord::Skip)`. On error the session
    /// stays Parked (nothing is half-built).
    pub(crate) async fn unpark(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        let journal_id = (**self.journal_id.load()).clone();
        let host = Arc::clone(&self.host);
        let cfg = self.config.clone();
        let queue = Arc::clone(&self.event_queue);
        let replay = self.settings_replay.clone();
        let journal_present = self.journal_exists();
        let view = self.view.load_full();
        let build = async move {
            let config: crate::SynapsConfig = (**host.config()).clone();
            let mut runtime = host.foreground_runtime().await?;
            runtime.set_event_queue(queue);
            let mut sb = if journal_present {
                crate::engine::setup::resolve_session_and_prompt(
                    &mut runtime,
                    &Some(Some(journal_id)),
                    cfg.system.as_deref(),
                    cfg.prompt_manifest.as_deref(),
                )?
            } else {
                // The journal vanished under us (H2): rebuild an empty
                // conversation under the SAME id with the last-published
                // model/thinking rather than leaving a zombie Parked session.
                tracing::warn!(
                    session = %journal_id,
                    "unpark: journal missing — restoring as a fresh conversation"
                );
                let mut sb = crate::engine::setup::resolve_session_and_prompt(
                    &mut runtime,
                    &None,
                    cfg.system.as_deref(),
                    cfg.prompt_manifest.as_deref(),
                )?;
                sb.session.id = journal_id.clone();
                runtime.set_session_id(Some(journal_id));
                sb
            };
            runtime.set_cwd(cfg.cwd.clone());
            // The CURRENT model/thinking (the last published view), not
            // `cfg.model_override` frozen at create: `/model` survives park.
            runtime.set_model(view.model.clone());
            let _ = runtime.restore_session_reasoning(&view.thinking_level);
            sb.session.model = view.model.clone();
            sb.session.thinking_level = runtime.thinking_level().to_string();
            crate::engine::setup::finish_session_setup(
                &mut runtime,
                &config,
                &sb.session,
                cfg.cwd.clone(),
                crate::engine::setup::IndexRecord::Skip,
            );
            for s in &replay {
                replay_setting(&mut runtime, s);
            }
            Ok::<_, crate::RuntimeError>((runtime, sb))
        };
        let (runtime, sb) = match tokio::time::timeout(budgets::UNPARK_TIMEOUT, build).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(crate::RuntimeError::Session(format!(
                    "unpark timed out after {}s",
                    budgets::UNPARK_TIMEOUT_SECS
                )))
            }
        };
        let mut conv = ConversationState::from_resumed(sb.session);
        conv.api_messages = sb.api_messages;
        conv.total_input_tokens = sb.total_input_tokens;
        conv.total_output_tokens = sb.total_output_tokens;
        conv.session_cost = sb.session_cost;
        conv.abort_context = sb.abort_context;
        self.runtime.unpark_set(runtime);
        self.conv.unpark_set(conv);
        self.publish_view().await;
        self.state = AttachState::Detached { running: false };
        self.set_lifecycle(SessionLifecycle::Live);
        self.update_attach_state();
        tracing::info!(session = %self.id, ms = started.elapsed().as_millis() as u64, "session unparked");
        Ok(())
    }

    /// Unpark if Parked. `Err` = still Parked; the caller reports it
    /// (`Refused` to a client, `AttachRefused` to an attacher, a warning
    /// for host wakes).
    pub(crate) async fn ensure_live(&mut self) -> std::result::Result<(), String> {
        if !self.is_parked() {
            return Ok(());
        }
        self.unpark()
            .await
            .map_err(|e| format!("session parked and could not be restored: {e}"))
    }

    /// `stream_handler.rs:264`: a turn OR a compaction blocks auto-turns.
    pub(crate) fn busy(&self) -> bool {
        self.streaming || self.compact.is_some()
    }

    /// `runtime.subagent_registry().display_rows()` → `SubagentRows`, only
    /// when there is something to show (the TUI's reconcile is a no-op on
    /// an empty registry).
    pub(crate) fn publish_subagent_rows(&mut self) {
        let rows = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows();
        if !rows.is_empty() {
            self.emit(SessionEventWire::SubagentRows(rows));
        }
    }

    // ── turn start (dispatch.rs Submit tail / stream_handler.rs RunTurn) ──

    pub(crate) async fn start_turn(&mut self, trigger: TurnTrigger, user_text: Option<String>) {
        let ct = CancellationToken::new();
        let (s_tx, s_rx) = mpsc::unbounded_channel::<String>();
        self.streaming = true;
        self.turn_baseline = self.conv.api_messages.len();
        self.turn_log.clear();
        self.turn_replay.clear();
        self.update_attach_state();
        self.emit(SessionEventWire::TurnStarted {
            turn_baseline: self.turn_baseline,
            trigger,
            user_text,
        });
        // Blocks command processing during setup exactly like the TUI loop.
        let stream = self
            .runtime
            .run_stream_with_messages(
                self.conv.api_messages.clone(),
                ct.clone(),
                Some(s_rx),
                Some(self.secret_prompt_handle.clone()),
                self.config.auto_approve_confirms,
            )
            .await;
        self.stream = Some(stream);
        self.cancel = Some(ct);
        self.steer_tx = Some(s_tx);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.reset();
        self.subagent_tick = Some(tick);
    }

    /// Every turn-end path (Done/Error/Cancel/stream EOF). Clears `turn_log`
    /// too: a `Cancel` racing a `Done` must not scrape the finished turn into
    /// `abort_context`.
    pub(crate) fn clear_stream(&mut self) {
        self.stream = None;
        self.cancel = None;
        self.steer_tx = None;
        self.streaming = false;
        self.subagent_tick = None;
        self.turn_log.clear();
        self.update_attach_state();
    }

    /// dispatch.rs Submit (:1231-1288) minus presentation.
    pub(crate) async fn submit(&mut self, text: String) {
        if self.streaming {
            // A Submit while streaming is what the TUI calls StreamingInput.
            self.steer(text);
            return;
        }
        if self.compact.is_some() {
            // dispatch.rs:1233-1237: queued until the compaction lands.
            self.emit(SessionEventWire::Steered {
                text: text.clone(),
                delivered: false,
            });
            self.conv.queued_message = Some(text);
            return;
        }
        // Real user send — reset auto-turn counter.
        self.consecutive_auto_turns = 0;
        // Inject abort context if previous response was interrupted
        let api_content = if let Some(ref ctx) = self.conv.abort_context {
            let combined = format!("{}\n\n{}", ctx, text);
            self.conv.abort_context = None;
            combined
        } else {
            text
        };
        self.conv.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": api_content}),
        ));
        self.start_turn(TurnTrigger::User, None).await;
    }

    /// dispatch.rs StreamingInput plain-text branch (:1369-1378).
    pub(crate) fn steer(&mut self, text: String) {
        let delivered = self
            .steer_tx
            .as_ref()
            .map(|tx| tx.send(text.clone()).is_ok())
            .unwrap_or(false);
        self.emit(SessionEventWire::Steered {
            text: text.clone(),
            delivered,
        });
        self.conv.queued_message = Some(text);
    }

    /// dispatch.rs Abort (:134-192) verbatim minus presentation, behind the
    /// TUI's `if streaming` guard (input.rs:350): a `Cancel` while idle is a
    /// no-op that only re-announces `Idle` — it must never touch
    /// `abort_context`, save, or emit `Aborted`.
    pub(crate) async fn cancel_turn(&mut self) {
        if !self.streaming {
            if self.compact.is_some() {
                self.abort_compaction();
                if let Some(q) = self.conv.queued_message.take() {
                    self.emit(SessionEventWire::Dequeued { text: q });
                }
                self.update_attach_state();
            }
            self.emit(SessionEventWire::Idle);
            return;
        }
        if let Some(ref ct) = self.cancel {
            ct.cancel();
        }
        self.conv.abort_context = self.turn_log.abort_context();
        if let Some(q) = self.conv.queued_message.take() {
            self.emit(SessionEventWire::Dequeued { text: q });
        }
        // Flush any events that arrived during streaming
        {
            let conv: &mut ConversationState = &mut self.conv;
            for formatted in conv.pending_events.drain(..) {
                conv.api_messages
                    .push(std::sync::Arc::new(serde_json::json!({
                        "role": "user",
                        "content": formatted
                    })));
            }
        }
        self.clear_stream();
        // Cancel all running reactive subagents; recover a poisoned guard
        // rather than skip cancellation.
        {
            let mut registry = match self.runtime.subagent_registry().lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    tracing::warn!(
                        "subagent registry mutex poisoned during abort; recovering to cancel running handles"
                    );
                    poisoned.into_inner()
                }
            };
            for handle in registry.iter_mut_handles() {
                if handle.status() == crate::runtime::subagent::SubagentStatus::Running {
                    handle.cancel();
                }
            }
        }
        // Typed event; clients render "aborted[ — context saved …]".
        self.emit(SessionEventWire::Aborted {
            context_saved: self.conv.abort_context.is_some(),
        });
        self.save().await;
        self.emit_conversation();
        self.emit(SessionEventWire::Idle);
    }

    // ── event-queue wake (stream_handler.rs handle_event_queue_arm) ──────

    async fn on_queue_wake(&mut self) {
        if self.is_parked() {
            if self.event_queue.is_empty() {
                return;
            }
            if let Err(e) = self.ensure_live().await {
                tracing::warn!(session = %self.id, error = %e, "event wake could not unpark; events stay queued");
                return;
            }
        }
        let busy = self.busy();
        let conv: &mut ConversationState = &mut self.conv;
        let drained = drain_event_queue(
            &self.event_queue,
            &mut conv.api_messages,
            &mut conv.pending_events,
            busy,
            self.steer_tx.as_ref(),
        );
        if drained.is_empty() {
            return;
        }
        for de in &drained {
            self.emit(SessionEventWire::External(de.event.clone()));
        }
        let injected = drained
            .iter()
            .any(|d| d.disposition == EventDisposition::Injected);
        if injected || busy {
            self.emit_conversation();
        }

        let auto_turn_enabled = true;
        let action = wake_action(
            &drained,
            &self.conv.api_messages,
            busy,
            auto_turn_enabled,
            self.consecutive_auto_turns,
        );
        match action {
            WakeAction::RunTurn => {
                if self.stream.is_some() {
                    tracing::warn!("handle_event_arm: RunTurn with active stream — skipping");
                } else {
                    self.consecutive_auto_turns += 1;
                    self.start_turn(TurnTrigger::EventAuto, None).await;
                }
            }
            WakeAction::Forward => {
                let hit_cap = injected
                    && !busy
                    && auto_turn_enabled
                    && self.consecutive_auto_turns >= AUTO_TURN_CAP;
                if hit_cap {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: AUTO_TURN_CAP });
                }
            }
            WakeAction::Nothing => {}
        }
    }

    // ── stream events (stream_handler.rs handle_stream_event + arm tail) ──

    async fn on_stream_event(&mut self, event: StreamEvent) {
        // Forward first: clients see the same order they see today.
        self.emit(SessionEventWire::Stream(event.clone()));

        enum After {
            Continue,
            AutoSendQueued(String),
            AutoTriggerEvents,
            Failed,
        }
        let mut after = After::Continue;

        match event {
            StreamEvent::Llm(LlmEvent::Thinking(text)) => self.turn_log.thinking(&text),
            StreamEvent::Llm(LlmEvent::Text(text)) => self.turn_log.text(&text),
            StreamEvent::Llm(LlmEvent::ToolUse {
                tool_name, input, ..
            }) => {
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                self.turn_log.parts.push(TurnPart::ToolUse {
                    name: tool_name,
                    input: input_str,
                });
            }
            StreamEvent::Llm(LlmEvent::ToolResultDelta { tool_id, delta }) => {
                self.turn_log.tool_result_delta(tool_id, delta);
            }
            StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => {
                self.turn_log.tool_result(tool_id, result);
            }
            StreamEvent::Llm(_) => {}
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                self.conv.api_messages = history;
                self.save().await;
                self.emit_conversation();
            }
            StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                if self.conv.queued_message.as_ref() == Some(&message) {
                    self.conv.queued_message = None;
                    self.emit_conversation();
                }
            }
            StreamEvent::Agent(_) => {}
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model: usage_model,
            }) => {
                let model_for_pricing = usage_model
                    .as_deref()
                    .unwrap_or(self.runtime.model())
                    .to_string();
                self.conv.add_usage(
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    &model_for_pricing,
                );
            }
            StreamEvent::Session(SessionEvent::Notice(_)) => {}
            StreamEvent::Session(SessionEvent::Done) => {
                self.clear_stream();
                self.publish_subagent_rows();
                // Flush events that arrived during streaming into api_messages
                let had_pending = !self.conv.pending_events.is_empty();
                {
                    let conv: &mut ConversationState = &mut self.conv;
                    for formatted in conv.pending_events.drain(..) {
                        conv.api_messages
                            .push(std::sync::Arc::new(serde_json::json!({
                                "role": "user",
                                "content": formatted
                            })));
                    }
                }
                if let Some(queued) = self.conv.queued_message.take() {
                    after = After::AutoSendQueued(queued);
                } else if had_pending {
                    self.save().await;
                    after = After::AutoTriggerEvents;
                }
                self.emit_conversation();
                if matches!(after, After::Continue) {
                    if self.config.auto_compact {
                        self.post_turn_chat().await;
                    }
                    // A spawned compaction emits Idle when it lands.
                    if self.compact.is_none() {
                        self.emit(SessionEventWire::Idle);
                    }
                }
            }
            StreamEvent::Session(SessionEvent::Error(_)) => {
                self.clear_stream();
                self.publish_subagent_rows();
                // Remove only invalid messages appended by the ACTIVE turn.
                crate::engine::stream::repair_history_after_failure(
                    &mut self.conv.api_messages,
                    self.turn_baseline,
                );
                self.emit_conversation();
                after = After::Failed;
            }
        }

        match after {
            After::Continue => {}
            After::Failed => {
                if self.config.auto_compact {
                    self.post_turn_chat().await;
                }
                if self.compact.is_none() {
                    self.emit(SessionEventWire::Idle);
                }
            }
            After::AutoSendQueued(queued) => {
                if self.config.auto_compact {
                    self.post_turn_chat().await;
                }
                if self.compact.is_some() {
                    // Rides the compaction transition (queued_message
                    // restored last); the TUI reports it, never re-sends.
                    self.conv.queued_message = Some(queued);
                    return;
                }
                // Auto-send the queued message (user-authored — reset counter)
                self.consecutive_auto_turns = 0;
                let user_text = queued.clone();
                let api_content = if let Some(ref ctx) = self.conv.abort_context {
                    let combined = format!("{}\n\n{}", ctx, queued);
                    self.conv.abort_context = None;
                    combined
                } else {
                    queued
                };
                self.conv.api_messages.push(std::sync::Arc::new(
                    serde_json::json!({"role": "user", "content": api_content}),
                ));
                self.start_turn(TurnTrigger::QueuedAuto, Some(user_text)).await;
            }
            After::AutoTriggerEvents => {
                if self.config.auto_compact {
                    self.post_turn_chat().await;
                }
                if self.compact.is_some() {
                    return;
                }
                // Central claim gate: allows turns 1-5, denies the 6th.
                if claim_auto_turn(&mut self.consecutive_auto_turns) {
                    self.start_turn(TurnTrigger::EventAuto, None).await;
                } else {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: AUTO_TURN_CAP });
                    self.emit(SessionEventWire::Idle);
                }
            }
        }
    }

    /// chat.rs post-turn block: save + engine-budget auto-compaction.
    async fn post_turn_chat(&mut self) {
        self.save().await;
        let assessment = self.runtime.assess_context(&self.conv.api_messages).await;
        if assessment.should_compact() {
            self.emit(SessionEventWire::SystemNotice(format!(
                "[auto-compacting ~{} tokens...]",
                assessment.used_tokens()
            )));
            self.compact(None, "auto").await;
        }
    }

    /// `SYNAPS_SESSION_COMPACT_INLINE=1`: one-release kill-switch back to the
    /// #107 inline body (deleted in phase 4).
    fn compact_inline_env() -> bool {
        matches!(
            std::env::var("SYNAPS_SESSION_COMPACT_INLINE").as_deref(),
            Ok("1") | Ok("true")
        )
    }

    /// `/compact` + auto-compaction entry (B2): spawns `compact_conversation`
    /// on a `Runtime::clone()` (the same clone the TUI makes at
    /// dispatch.rs:450 — the clone never runs turns) and returns; the
    /// `select!` arm `on_compaction_done` applies the outcome. While a job
    /// is in flight Attach/Detach/Cancel/Query are serviced, `Submit` is
    /// queued (`Steered{delivered:false}`), auto-turns see `busy`.
    pub(crate) async fn compact(&mut self, instructions: Option<String>, source: &str) {
        if Self::compact_inline_env() {
            return self.compact_inline(instructions, source).await;
        }
        if self.compact.is_some() {
            self.emit(SessionEventWire::SystemNotice(
                "compaction already in progress".into(),
            ));
            return;
        }
        if self.streaming {
            self.emit(SessionEventWire::SystemNotice(
                "cannot compact while a turn is running".into(),
            ));
            return;
        }
        let disclosure =
            preview_compaction_disclosure(&self.runtime, &self.conv.api_messages).render_line();
        self.emit(SessionEventWire::CompactionStarted {
            source: source.to_string(),
            disclosure,
        });
        let msgs = self.conv.api_messages.clone();
        let rt: Runtime = (*self.runtime).clone();
        let task = tokio::spawn(async move {
            compact_conversation(&msgs, &rt, instructions.as_deref()).await
        });
        self.compact = Some(CompactionJob {
            task,
            source: source.to_string(),
            started: tokio::time::Instant::now(),
        });
        self.update_attach_state();
    }

    /// `select!` arm: the spawned job finished (or was aborted). Applies via
    /// the ONE engine transition with the configured policy; pending events
    /// and the queued message ride the transition (loop_arms.rs:761-836).
    async fn on_compaction_done(
        &mut self,
        res: std::result::Result<
            Result<crate::runtime::compaction::CompactionOutcome>,
            tokio::task::JoinError,
        >,
    ) {
        let Some(job) = self.compact.take() else {
            return;
        };
        let msg_count = self.conv.api_messages.len();
        match res {
            Err(join) => {
                if !join.is_cancelled() {
                    self.emit(SessionEventWire::CompactionFailed {
                        message: join.to_string(),
                        panicked: true,
                    });
                }
            }
            Ok(Err(e)) => self.emit(SessionEventWire::CompactionFailed {
                message: e.to_string(),
                panicked: false,
            }),
            Ok(Ok(outcome)) => {
                let policy: CompactionPolicy = self.config.compaction_policy.into();
                let queued = self.conv.queued_message.clone();
                let applied = apply_compaction(
                    &self.runtime,
                    &self.conv.session,
                    &self.conv.api_messages,
                    &outcome,
                    CompactionTransition {
                        policy,
                        pending_events: self.conv.pending_events.clone(),
                        queued_message: queued.clone(),
                        hook_source: job.source.clone(),
                    },
                )
                .await;
                match applied {
                    Ok(applied) => {
                        let previous = applied.previous_session_id.clone();
                        self.conv.session = applied.session;
                        self.conv.api_messages = applied.api_messages;
                        self.conv.pending_events.clear();
                        self.conv.queued_message = None;
                        if policy == CompactionPolicy::LinkedSuccessor {
                            self.conv.total_input_tokens = 0;
                            self.conv.total_output_tokens = 0;
                            self.conv.session_cost = 0.0;
                        }
                        let new_id = self.conv.session.id.clone();
                        self.runtime.set_session_id(Some(new_id.clone()));
                        self.journal_id.store(Arc::new(new_id.clone()));
                        self.emit(SessionEventWire::CompactionApplied {
                            previous_session_id: previous,
                            session_id: new_id,
                            chains_advanced: applied.chains_advanced,
                            queued_restored: queued,
                            msg_count,
                        });
                    }
                    Err(e) => self.emit(SessionEventWire::CompactionFailed {
                        message: e.to_string(),
                        panicked: false,
                    }),
                }
            }
        }
        self.emit_conversation();
        self.update_attach_state();
        self.emit(SessionEventWire::Idle);
    }

    /// #107 inline body (kill-switch only).
    async fn compact_inline(&mut self, instructions: Option<String>, source: &str) {
        self.emit(SessionEventWire::SystemNotice(format!(
            "[{}]",
            preview_compaction_disclosure(&self.runtime, &self.conv.api_messages).render_line()
        )));
        let outcome =
            compact_conversation(&self.conv.api_messages, &self.runtime, instructions.as_deref())
                .await;
        let applied = match outcome {
            Ok(outcome) => {
                apply_compaction(
                    &self.runtime,
                    &self.conv.session,
                    &self.conv.api_messages,
                    &outcome,
                    CompactionTransition {
                        policy: CompactionPolicy::InPlace,
                        pending_events: Vec::new(),
                        queued_message: None,
                        hook_source: source.to_string(),
                    },
                )
                .await
            }
            Err(e) => Err(e),
        };
        match applied {
            Ok(applied) => {
                self.conv.session = applied.session;
                self.conv.api_messages = applied.api_messages;
                let after = self.runtime.assess_context(&self.conv.api_messages).await;
                self.emit(SessionEventWire::SystemNotice(format!(
                    "[compacted → ~{} tokens]",
                    after.used_tokens()
                )));
            }
            Err(e) => self.emit(SessionEventWire::SystemNotice(format!(
                "[compaction failed: {}]",
                e
            ))),
        }
        self.emit_conversation();
    }

    // ── prompts ──────────────────────────────────────────────────────────

    fn on_prompt_request(&mut self, req: SecretPromptRequest) {
        let id = self.next_prompt_id;
        self.next_prompt_id += 1;
        let pr = PromptRequest {
            id,
            kind: PromptKind::from_title(&req.title),
            title: req.title,
            prompt: req.prompt,
            raised_at: chrono::Utc::now(),
        };
        self.pending_prompts.push_back((pr.clone(), req.response_tx));
        self.publish_presence();
        self.emit(SessionEventWire::Prompt(pr));
    }

    fn answer(&mut self, prompt_id: u64, value: Option<String>) {
        let Some(pos) = self.pending_prompts.iter().position(|(p, _)| p.id == prompt_id) else {
            return; // unknown or already answered — dedup on id
        };
        let (_, tx) = self.pending_prompts.remove(pos).expect("position");
        let _ = tx.send(value);
        self.publish_presence();
        self.emit(SessionEventWire::PromptResolved { prompt_id });
    }

    // ── settings / queries / engine commands ─────────────────────────────

    pub(crate) async fn apply_setting(&mut self, id: u64, setting: SessionSetting) {
        if replayable(&setting) {
            let disc = std::mem::discriminant(&setting);
            self.settings_replay
                .retain(|s| std::mem::discriminant(s) != disc);
            self.settings_replay.push(setting.clone());
        }
        let name = match &setting {
            SessionSetting::Model { .. } => "model",
            SessionSetting::ReasoningLevel { .. } => "reasoning_level",
            SessionSetting::ContextWindow { .. } => "context_window",
            SessionSetting::CompactionModel { .. } => "compaction_model",
            SessionSetting::ApiRetries { .. } => "api_retries",
            SessionSetting::SubagentTimeout { .. } => "subagent_timeout",
            SessionSetting::MaxToolOutput { .. } => "max_tool_output",
            SessionSetting::BashTimeout { .. } => "bash_timeout",
            SessionSetting::BashMaxTimeout { .. } => "bash_max_timeout",
            SessionSetting::SystemPrompt { .. } => "system_prompt",
            SessionSetting::ReloadPrompt => "reload_prompt",
            SessionSetting::GrantWorkerModel { .. } => "grant_worker_model",
        };
        let rt = &mut self.runtime;
        let mut clamp_wire = None;
        let result: std::result::Result<Option<String>, String> = match setting {
            SessionSetting::Model { model } => rt.try_set_model(model).map(|clamp| {
                self.conv.session.model = rt.model().to_string();
                clamp.map(|c| {
                    self.conv.session.thinking_level = rt.thinking_level().to_string();
                    clamp_wire = Some(ReasoningClampWire {
                        from: c.from.as_str().to_string(),
                        to: c.to.as_str().to_string(),
                    });
                    format!(
                        "thinking → {} (clamped from {}: not supported by {})",
                        c.to.as_str(),
                        c.from.as_str(),
                        rt.model()
                    )
                })
            }),
            SessionSetting::ReasoningLevel { level } => {
                rt.set_reasoning_level_checked(level).map(|_| {
                    self.conv.session.thinking_level = rt.thinking_level().to_string();
                    None
                })
            }
            SessionSetting::ContextWindow { tokens } => {
                rt.set_context_window(tokens);
                Ok(None)
            }
            SessionSetting::CompactionModel { model } => {
                rt.set_compaction_model(model);
                Ok(None)
            }
            SessionSetting::ApiRetries { n } => {
                rt.set_api_retries(n);
                Ok(None)
            }
            SessionSetting::SubagentTimeout { secs } => {
                rt.set_subagent_timeout(secs);
                Ok(None)
            }
            SessionSetting::MaxToolOutput { bytes } => {
                rt.set_max_tool_output(bytes);
                Ok(None)
            }
            SessionSetting::BashTimeout { secs } => {
                rt.set_bash_timeout(secs);
                Ok(None)
            }
            SessionSetting::BashMaxTimeout { secs } => {
                rt.set_bash_max_timeout(secs);
                Ok(None)
            }
            SessionSetting::SystemPrompt { text } => {
                rt.set_system_prompt(text);
                Ok(None)
            }
            SessionSetting::ReloadPrompt => rt
                .reload_prompt()
                .map(|generation| Some(format!("prompt reloaded (generation {generation})")))
                .map_err(|e| e.to_string()),
            SessionSetting::GrantWorkerModel { model } => {
                rt.grant_worker_model(&model).map(|_| None)
            }
        };
        self.publish_view().await;
        let view = (**self.view.load()).clone();
        let (ok, message) = match result {
            Ok(m) => (true, m),
            Err(e) => (false, Some(e)),
        };
        self.emit(SessionEventWire::SettingChanged(SettingApplied {
            id,
            setting: name.to_string(),
            ok,
            message,
            view,
            clamp: clamp_wire,
        }));
    }

    pub(crate) async fn query(&mut self, id: u64, query: SessionQuery) {
        // Status/View never wake a parked session (the idle probe must not).
        if self.is_parked() {
            let value = match query {
                SessionQuery::Status => serde_json::json!({
                    "session": (**self.journal_id.load()).clone(),
                    "model": self.view.load().model,
                    "lifecycle": SessionLifecycle::Parked,
                    "streaming": false,
                    "auto_turns": self.consecutive_auto_turns,
                    "attached": 0,
                    "pending_prompts": 0,
                }),
                SessionQuery::View => serde_json::to_value(&**self.view.load()).unwrap_or_default(),
                _ => match self.ensure_live().await {
                    Ok(()) => return Box::pin(self.query(id, query)).await,
                    Err(e) => serde_json::json!({ "error": e }),
                },
            };
            self.emit(SessionEventWire::QueryResult { id, value });
            return;
        }
        let value = match query {
            SessionQuery::Status => serde_json::json!({
                "session": self.conv.session.id,
                "model": self.runtime.model(),
                "lifecycle": SessionLifecycle::from_u8(self.lifecycle.load(std::sync::atomic::Ordering::Acquire)),
                "tokens": { "input": self.conv.total_input_tokens, "output": self.conv.total_output_tokens },
                "cost": self.conv.session_cost,
                "messages": self.conv.api_messages.len(),
                "streaming": self.streaming,
                "auto_turns": self.consecutive_auto_turns,
                "attached": self.attached.len(),
                "pending_prompts": self.pending_prompts.len(),
            }),
            SessionQuery::Messages => {
                serde_json::to_value(&self.conv.api_messages).unwrap_or_default()
            }
            SessionQuery::SubagentRows => {
                let rows = self
                    .runtime
                    .subagent_registry()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .display_rows();
                serde_json::Value::Array(
                    rows.iter()
                        .map(|r| {
                            serde_json::json!({
                                "subagent_id": r.subagent_id,
                                "agent_name": r.agent_name,
                                "status": format!("{:?}", r.status),
                                "cancel_requested": r.cancel_requested,
                                "elapsed_secs": r.elapsed_secs,
                            })
                        })
                        .collect(),
                )
            }
            SessionQuery::PromptInspection => {
                serde_json::to_value(self.runtime.prompt_inspection_json()).unwrap_or_default()
            }
            SessionQuery::ToolsSchema => serde_json::json!({ "unsupported": "tools_schema" }),
            SessionQuery::View => serde_json::to_value(&**self.view.load()).unwrap_or_default(),
            SessionQuery::ContextAssessment => {
                let a = self.runtime.assess_context(&self.conv.api_messages).await;
                serde_json::json!({
                    "used_tokens": a.used_tokens(),
                    "budget_tokens": a.budget_tokens(),
                    "provider_window": a.provider_window,
                    "should_compact": a.should_compact(),
                })
            }
            SessionQuery::ContextReport => {
                use crate::engine::commands::{context_command, CommandResult};
                match context_command(&self.runtime, Some(&self.conv.api_messages)) {
                    CommandResult::Output(text) => serde_json::json!({ "text": text }),
                    other => serde_json::json!({ "unsupported": format!("{other:?}") }),
                }
            }
        };
        self.emit(SessionEventWire::QueryResult { id, value });
    }

    // ── attach / detach ──────────────────────────────────────────────────

    pub(crate) fn snapshot(&self) -> AttachSnapshot {
        AttachSnapshot {
            meta: self.meta.clone(),
            view: (**self.view.load()).clone(),
            conversation: self.conv.snapshot(self.consecutive_auto_turns),
            streaming: self.streaming,
            replay: self.turn_replay.iter().cloned().collect(),
            pending_prompts: self.pending_prompts.iter().map(|(p, _)| p.clone()).collect(),
            clients: self.attached.iter().map(|(c, a)| (*c, a.meta.kind)).collect(),
            input_owner: self.input_owner,
        }
    }

    /// B1 ownership: `Observe` never owns; `Mirror` owns iff nobody does
    /// (else read-only + notice); `Takeover` steals with
    /// `InputOwnerChanged{reason: Takeover}` to everyone (the old owner's
    /// client renders the toast).
    async fn attach(&mut self, client: ClientMeta, mode: AttachMode) {
        if let Err(e) = self.ensure_live().await {
            self.emit(SessionEventWire::AttachRefused { message: e });
            return;
        }
        let cid = ClientId(self.next_client_id);
        self.next_client_id += 1;
        let kind = client.kind;
        self.attached.insert(cid, AttachedClient { meta: client, mode });
        self.update_attach_state();
        let mut owner_change: Option<(Option<ClientId>, OwnerChangeReason)> = None;
        let mut notice: Option<String> = None;
        match mode {
            AttachMode::Observe => {}
            AttachMode::Mirror => match self.input_owner {
                None => owner_change = Some((None, OwnerChangeReason::Attach)),
                Some(owner) => {
                    let owner_kind = self
                        .attached
                        .get(&owner)
                        .map(|a| format!("{:?}", a.meta.kind).to_lowercase())
                        .unwrap_or_else(|| "?".into());
                    notice = Some(format!(
                        "input is owned by client #{} ({}); attach with --takeover to steal it",
                        owner.0, owner_kind
                    ));
                }
            },
            AttachMode::Takeover => {
                let reason = if self.input_owner.is_some() {
                    OwnerChangeReason::Takeover
                } else {
                    OwnerChangeReason::Attach
                };
                owner_change = Some((self.input_owner, reason));
            }
        }
        if let Some((from, reason)) = owner_change {
            self.input_owner = Some(cid);
            self.emit(SessionEventWire::InputOwnerChanged {
                from,
                to: Some(cid),
                reason,
            });
        }
        self.publish_presence();
        let snapshot = self.snapshot();
        self.emit(SessionEventWire::Attached {
            client: cid,
            snapshot,
        });
        self.emit(SessionEventWire::ClientJoined { client: cid, kind });
        if let Some(n) = notice {
            self.emit(SessionEventWire::SystemNotice(n));
        }
    }

    /// Never touches `stream`/`cancel`: the turn keeps running and its
    /// events buffer in the broadcast (§8 detach-without-abort). The owner
    /// leaving passes input to the oldest attached non-`Observe` client.
    fn detach(&mut self, client: ClientId) {
        if self.attached.remove(&client).is_none() {
            return;
        }
        self.update_attach_state();
        self.emit(SessionEventWire::ClientLeft { client });
        if self.input_owner == Some(client) {
            let next = self
                .attached
                .iter()
                .filter(|(_, a)| a.mode != AttachMode::Observe)
                .map(|(c, _)| *c)
                .min_by_key(|c| c.0);
            self.input_owner = next;
            self.emit(SessionEventWire::InputOwnerChanged {
                from: Some(client),
                to: next,
                reason: OwnerChangeReason::OwnerDetached,
            });
            self.publish_presence();
        }
    }

    /// B1 (used by C3 reload): checkpoint the session without ending it —
    /// cancel any turn (abort_context captured), abort compaction, answer
    /// pending prompts `None`, save, close PTYs. Replies on
    /// `CHECKPOINT_QUERY_ID` so `reload.rs` can await it per session.
    pub(crate) async fn checkpoint(&mut self, reason: CheckpointReason) {
        if self.streaming {
            self.cancel_turn().await;
        }
        self.abort_compaction();
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }
        if tokio::time::timeout(budgets::SAVE_TIMEOUT, self.save())
            .await
            .is_err()
        {
            tracing::warn!("checkpoint: save timed out");
        }
        let notice = match reason {
            CheckpointReason::Reload => {
                "daemon reloading — background shells/PTYs will be closed; the turn was checkpointed"
            }
            CheckpointReason::HostRequest => {
                "checkpoint — background shells/PTYs closed; the turn was checkpointed"
            }
        };
        self.emit(SessionEventWire::SystemNotice(notice.to_string()));
        self.runtime.session_manager().shutdown_all();
        self.emit(SessionEventWire::QueryResult {
            id: super::wire::CHECKPOINT_QUERY_ID,
            value: serde_json::json!({ "ok": true }),
        });
    }

    /// Abort the spawned job (`CompactionCancelled`); prior state intact.
    pub(crate) fn abort_compaction(&mut self) {
        if let Some(job) = self.compact.take() {
            job.task.abort();
            self.emit(SessionEventWire::CompactionCancelled);
        }
    }

    // ── command dispatch ─────────────────────────────────────────────────

    /// B1: a client-stamped input command from anyone but the owner is
    /// `Refused` with no side effect; `from: None` (host) bypasses.
    async fn handle(&mut self, addressed: Addressed) -> std::ops::ControlFlow<EndReason> {
        use std::ops::ControlFlow;
        let Addressed { from, cmd } = addressed;
        if let Some(c) = from {
            if is_input_command(&cmd) && self.input_owner != Some(c) {
                let reason = match self.input_owner {
                    Some(o) => format!("input owned by client #{}", o.0),
                    None => "input has no owner; attach with --takeover".to_string(),
                };
                self.emit(SessionEventWire::Refused {
                    client: c,
                    command: command_name(&cmd).to_string(),
                    reason,
                });
                return ControlFlow::Continue(());
            }
        }
        // B3: anything that needs the runtime wakes a parked session first.
        if self.is_parked() {
            let needs_runtime = !matches!(
                cmd,
                SessionCommand::Attach { .. }
                    | SessionCommand::Detach { .. }
                    | SessionCommand::End { .. }
                    | SessionCommand::Query { .. }
                    | SessionCommand::Answer { .. }
                    | SessionCommand::Save
                    | SessionCommand::Cancel
                    | SessionCommand::Checkpoint { .. }
                    | SessionCommand::Resync { .. }
                    | SessionCommand::HostEvent(_)
            );
            if matches!(cmd, SessionCommand::HostEvent(_)) {
                return ControlFlow::Continue(()); // nobody attached, nothing to render
            }
            if matches!(cmd, SessionCommand::Save | SessionCommand::Cancel | SessionCommand::Checkpoint { .. }) {
                if let SessionCommand::Checkpoint { .. } = cmd {
                    self.emit(SessionEventWire::QueryResult {
                        id: super::wire::CHECKPOINT_QUERY_ID,
                        value: serde_json::json!({ "ok": true, "parked": true }),
                    });
                }
                return ControlFlow::Continue(()); // already on disk / idle
            }
            if needs_runtime {
                if let Err(e) = self.ensure_live().await {
                    match from {
                        Some(c) => self.emit(SessionEventWire::Refused {
                            client: c,
                            command: command_name(&cmd).to_string(),
                            reason: e,
                        }),
                        None => self.emit(SessionEventWire::SystemNotice(e)),
                    }
                    return ControlFlow::Continue(());
                }
            }
        }
        match cmd {
            SessionCommand::Submit { text, .. } => self.submit(text).await,
            SessionCommand::Steer { text } => {
                if self.streaming {
                    self.steer(text)
                } else {
                    self.submit(text).await
                }
            }
            SessionCommand::Cancel => self.cancel_turn().await,
            SessionCommand::Answer { prompt_id, value } => self.answer(prompt_id, value),
            SessionCommand::Set { id, setting } => self.apply_setting(id, setting).await,
            // `CompactionStarted` is the contract; no notice before it.
            SessionCommand::Compact { instructions } => {
                self.compact(instructions, "manual").await;
            }
            SessionCommand::NewSession => {
                self.conv.clear(&self.runtime).await;
                self.runtime
                    .set_session_id(Some(self.conv.session.id.clone()));
                self.journal_id.store(Arc::new(self.conv.session.id.clone()));
                self.emit(SessionEventWire::Cleared {
                    session_id: self.conv.session.id.clone(),
                });
                self.emit_conversation();
            }
            SessionCommand::Save => self.save().await,
            SessionCommand::Query { id, query } => self.query(id, query).await,
            SessionCommand::EngineCommand { id, name, arg } => {
                self.engine_command(id, name, arg).await
            }
            SessionCommand::Attach { client, mode } => self.attach(client, mode).await,
            SessionCommand::Detach { client } => self.detach(client),
            SessionCommand::End { reason } => return ControlFlow::Break(reason),
            SessionCommand::Resync { .. } => self.emit(SessionEventWire::SystemNotice(
                "resync not supported yet".into(),
            )),
            // Phase-3 stubs: A3 (SubmitPrepared/PluginCommand/Resume), B1
            // (Checkpoint), B3 (KeepWarm) fill these in.
            SessionCommand::SubmitPrepared { .. } => self.emit(SessionEventWire::SystemNotice(
                "submit_prepared: not implemented in this build".into(),
            )),
            SessionCommand::PluginCommand { id, .. } => self.emit(SessionEventWire::QueryResult {
                id,
                value: serde_json::json!({ "kind": "error", "text": "plugin_command: not implemented in this build" }),
            }),
            SessionCommand::Resume { .. } => self.emit(SessionEventWire::SystemNotice(
                "resume: not implemented in this build".into(),
            )),
            SessionCommand::Checkpoint { reason } => self.checkpoint(reason).await,
            SessionCommand::KeepWarm { on } => {
                self.keep_warm = on;
                self.rearm_park();
                self.emit(SessionEventWire::SystemNotice(format!(
                    "keep-warm {}",
                    if on { "on: this session will not be parked" } else { "off" }
                )));
            }
            SessionCommand::HostEvent(ev) => match ev {
                HostEvent::ExtensionNotification {
                    extension_id,
                    method,
                    params,
                } => self.emit(SessionEventWire::ExtensionNotification {
                    extension_id,
                    method,
                    params,
                }),
                HostEvent::LoaderProgress(ev) => self.emit(SessionEventWire::LoaderProgress(ev)),
            },
        }
        ControlFlow::Continue(())
    }

    // ── teardown (tui/mod.rs:352-432 + chat.rs shutdown) ─────────────────

    async fn finish(&mut self, reason: EndReason) {
        self.lifecycle.store(
            SessionLifecycle::Ending as u8,
            std::sync::atomic::Ordering::Release,
        );
        if self.streaming {
            self.cancel_turn().await;
        }
        self.abort_compaction();
        // Outstanding prompts are cancelled (tool sees `None`).
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }

        let parked = !self.conv.is_live();
        let session_id = (**self.journal_id.load()).clone();
        let api_messages = if parked {
            None
        } else {
            Some(self.conv.api_messages.clone())
        };

        // STEP 1: save — own bounded budget, highest priority. A parked
        // session is already on disk: end record only.
        let persist = self.config.persist;
        let save_fut = async {
            if persist {
                if !parked {
                    self.conv.save().await;
                }
                let mut index_record =
                    crate::core::session_index::SessionIndexRecord::end(&session_id);
                index_record.turns = api_messages.as_ref().map(|m| m.len());
                if let Err(err) = crate::core::session_index::append_record(&index_record) {
                    tracing::warn!("failed to append session end index record: {}", err);
                }
            }
        };
        if tokio::time::timeout(budgets::SAVE_TIMEOUT, save_fut)
            .await
            .is_err()
        {
            tracing::warn!(
                budget_secs = budgets::SAVE_TIMEOUT_SECS,
                "session save timed out — data may be incomplete"
            );
        }

        // STEP 2 (C2): on_session_end — per session, concurrent, fail-open,
        // own budget; clears this session's keyed injection.
        let hook_bus = Arc::clone(&self.hook_bus);
        crate::extensions::loader::emit_session_end(
            &hook_bus,
            &session_id,
            api_messages,
            budgets::HOOKS_TIMEOUT,
        )
        .await;

        // STEP 3: bounded observability flush (live only).
        if self.runtime.is_live() {
            if let Some(outcome) = self
                .runtime
                .shutdown_observability_async(
                    crate::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
                )
                .await
            {
                if !outcome.is_flushed() {
                    tracing::warn!(
                        stats = ?outcome.stats(),
                        "observability flush timed out — detached worker keeps draining"
                    );
                }
            }
        }

        // Inbox watcher, per-session UDS, registry entry, keyed injection.
        self.background.shutdown();
        self.emit(SessionEventWire::Ended { reason });
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }
}

/// The tokio task. `SessionTask::run` is the reactor loop.
pub struct SessionTask(SessionActor);

async fn next_stream_event(stream: &mut Option<ActiveStream>) -> Option<StreamEvent> {
    match stream {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

async fn next_tick(tick: &mut Option<tokio::time::Interval>) {
    match tick {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn poll_compaction(
    job: &mut Option<CompactionJob>,
) -> std::result::Result<Result<crate::runtime::compaction::CompactionOutcome>, tokio::task::JoinError> {
    match job {
        Some(j) => (&mut j.task).await,
        None => std::future::pending().await,
    }
}

async fn park_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

async fn ext_ready(rx: &mut Option<oneshot::Receiver<()>>) {
    match rx {
        Some(r) => {
            let _ = r.await;
        }
        None => std::future::pending().await,
    }
}

impl SessionTask {
    pub fn id(&self) -> &SessionId {
        self.0.id()
    }

    /// Unbiased select (the TUI loop is unbiased too).
    pub async fn run(mut self) {
        let queue = Arc::clone(&self.0.event_queue);
        let reason = loop {
            let actor = &mut self.0;
            tokio::select! {
                cmd = actor.cmd_rx.recv() => match cmd {
                    Some(cmd) => {
                        if let std::ops::ControlFlow::Break(reason) = actor.handle(cmd).await {
                            break reason;
                        }
                    }
                    // Every handle dropped: nobody can ever reach us again.
                    None => break EndReason::HostShutdown,
                },
                Some(req) = actor.secret_prompt_rx.recv() => actor.on_prompt_request(req),
                _ = queue.notified() => actor.on_queue_wake().await,
                _ = next_tick(&mut actor.subagent_tick) => actor.publish_subagent_rows(),
                _ = park_timer(actor.park_deadline) => actor.park().await,
                res = poll_compaction(&mut actor.compact) => actor.on_compaction_done(res).await,
                _ = ext_ready(&mut actor.ext_ready) => {
                    actor.ext_ready = None;
                    let id = (**actor.journal_id.load()).clone();
                    let bus = Arc::clone(&actor.hook_bus);
                    crate::extensions::loader::emit_session_start(&bus, &id).await;
                    if actor.runtime.is_live() {
                        actor.publish_view().await;
                    }
                },
                ev = next_stream_event(&mut actor.stream) => match ev {
                    Some(ev) => actor.on_stream_event(ev).await,
                    None => {
                        // Stream ended without a terminal event: defensive reset.
                        actor.clear_stream();
                        actor.emit_conversation();
                        actor.emit(SessionEventWire::Idle);
                    }
                },
            }
        };
        self.0.finish(reason).await;
    }
}
