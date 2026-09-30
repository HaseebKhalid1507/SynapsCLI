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
//! - `Runtime::clone()` resets TTL latches. The actor clones it only for
//!   driver stream-start (Prepared→Started) and compaction. The stream-start
//!   clone shares the original's TTL atomics via `share_ttl_latches` so a
//!   redundant 1h→5m downgrade notice per driver turn is avoided. The
//!   compaction clone is read-only+discarded and needs no sharing.
//! - `emit` is the ONLY `seq` increment site.
//! - `Detach` never touches `stream`/`cancel`; only `End` (and `Cancel`)
//!   stop a turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::engine::reactor::{
    auto_turn_cap_reached, claim_auto_turn_with_cap, drain_event_queue, wake_action_with_cap,
    EventDisposition, WakeAction,
};
use crate::engine::session::ConversationState;
use crate::engine::setup::BackgroundTasks;
use crate::extensions::invoke_output::{invoke_event_channel, InvokeOutputBudget};
use crate::extensions::session_driver::{self as protocol, Grant, Reply};
use crate::runtime::compaction::{
    apply_compaction, compact_conversation, preview_compaction_disclosure, CompactionPolicy,
    CompactionTransition,
};
use crate::tools::{SecretPromptHandle, SecretPromptRequest};
use crate::{
    AgentEvent, CancellationToken, EngineHost, Result, Runtime, SessionEvent,
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

/// The in-flight turn draft (`sessions/<id>.turn`, see
/// `agent_core::core::session_draft`): what the actor last knew about the
/// response being streamed, flushed at most once per 1 Hz turn tick.
#[derive(Default)]
pub(crate) struct TurnDraftState {
    /// Session id the open draft lives under; `None` = no turn open.
    open: Option<String>,
    /// History length the in-flight response continues from.
    base_len: usize,
    /// Text streamed in the in-flight response so far.
    text: String,
    /// Changed since the last flush.
    dirty: bool,
}

/// What `SessionActor::drain_cancelled_stream` recovered from a cancelled
/// turn's stream.
#[derive(Default)]
pub(crate) struct CancelDrain {
    /// The engine's final history for the turn (last `MessageHistory` seen).
    pub(crate) history: Option<Vec<crate::SharedMessage>>,
    /// The stream ended (`Done` / EOF) inside the drain budget.
    pub(crate) closed: bool,
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

/// `SYNAPS_DAEMON_PARKED_EVICT_SECS`: how long a PARKED session stays in the
/// daemon's map before it is evicted (`EndReason::Evicted`). Its state is on
/// disk; `--continue <id>` rebuilds it exactly as after a daemon restart. The
/// row only exists so `--attach <id>` / the adopt banner can find it quickly.
/// Default 1 h; `never` → keep forever.
pub fn parked_evict_after() -> Option<std::time::Duration> {
    // Precedence: env > config (`daemon.parked_evict_secs`) > builtin default.
    let cfg_secs = crate::config::load_config().daemon.parked_evict_secs;
    parked_evict_after_from(
        std::env::var("SYNAPS_DAEMON_PARKED_EVICT_SECS").ok().as_deref(),
        cfg_secs,
    )
}

fn parked_evict_after_from(v: Option<&str>, cfg_secs: u64) -> Option<std::time::Duration> {
    let from_cfg = || (cfg_secs != 0).then(|| std::time::Duration::from_secs(cfg_secs));
    match v.map(str::trim) {
        Some("never" | "0" | "off") => None,
        Some(n) => n
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_secs)
            .or_else(from_cfg),
        None => from_cfg(),
    }
}

/// `SYNAPS_DAEMON_IDLE_END_GRACE_SECS`: how long a ZERO-turn session with no
/// clients lingers before it ends (F18). Default 5 s — long enough for a
/// client that disconnected mid-reconnect, short enough that open-and-close
/// does not pin a Runtime for a minute.
pub fn idle_end_grace() -> std::time::Duration {
    std::env::var("SYNAPS_DAEMON_IDLE_END_GRACE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(5))
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

/// `SYNAPS_DAEMON_PROMPT_ABANDON_SECS`: how long a pending host confirmation on
/// a NON-driver session survives after the LAST client detaches before it is
/// fail-closed answered `None` (deny) so `can_park()` can proceed. This bounds
/// the resource pin from an ABANDONED prompt WITHOUT punishing a transient
/// disconnect (SSH drop, sleep, wifi): a re-attach before expiry cancels it and
/// the user answers the prompt normally. Default 1 h — generous enough that a
/// human coming back from lunch still answers; `never`/`0`/`off` → `None` =
/// disabled = pure pre-#112 behaviour (a prompt survives detach forever).
///
/// (The driver-armed case fail-closes immediately on last detach; this deadline
/// is only for a plain interactive session — see `detach` / `rearm_prompt_abandon`.)
pub fn prompt_abandon_timeout() -> Option<std::time::Duration> {
    // Precedence: env > config (`daemon.prompt_abandon_secs`) > builtin default.
    let cfg_secs = crate::config::load_config().daemon.prompt_abandon_secs;
    prompt_abandon_timeout_from(
        std::env::var("SYNAPS_DAEMON_PROMPT_ABANDON_SECS").ok().as_deref(),
        cfg_secs,
    )
}

fn prompt_abandon_timeout_from(v: Option<&str>, cfg_secs: u64) -> Option<std::time::Duration> {
    let from_cfg = || (cfg_secs != 0).then(|| std::time::Duration::from_secs(cfg_secs));
    match v.map(str::trim) {
        Some("never" | "0" | "off") => None,
        Some(n) => n
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_secs)
            .or_else(from_cfg),
        None => from_cfg(),
    }
}

/// Non-persisted runtime knobs replayed after unpark (last-wins per
/// variant). Model/reasoning live in the journal already; `/system` is
/// runtime-only (never journaled) so it replays too (M7).
fn replayable(setting: &SessionSetting) -> bool {
    matches!(
        setting,
        SessionSetting::SystemPrompt { .. }
            | SessionSetting::ContextWindow { .. }
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
        SessionSetting::SystemPrompt { text } => rt.set_system_prompt(text.clone()),
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
/// is open to every attached client. `Checkpoint` cancels the owner's turn
/// and kills its PTYs, so it is owner-only from a client (M1); the daemon
/// sends it `from: None`.
fn is_input_command(cmd: &SessionCommand) -> bool {
    matches!(
        cmd,
        SessionCommand::Checkpoint { .. }
            | SessionCommand::Submit { .. }
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
            | SessionCommand::DriverStart { .. }
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
        SessionCommand::Park => "park",
        SessionCommand::HostEvent(_) => "host_event",
        SessionCommand::DriverStart { .. } => "driver_start",
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
    /// (P11) Armed when the last client detaches while a pending host prompt is
    /// held on a NON-driver session; a re-attach cancels it, expiry denies the
    /// prompt(s) so `can_park()` can proceed. `None` = no abandoned prompt (or
    /// the deadline is disabled by config).
    pub(crate) prompt_abandon_deadline: Option<tokio::time::Instant>,
    // ── turn machine: the run() loop locals + App fields ──
    pub(crate) stream: Option<ActiveStream>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) steer_tx: Option<mpsc::UnboundedSender<String>>,
    pub(crate) streaming: bool,
    pub(crate) turn_baseline: usize,
    pub(crate) consecutive_auto_turns: u32,
    /// Formatted events `on_queue_wake` steered into this turn's stream
    /// that the engine has not acknowledged (`SteeringDelivered`) yet. On
    /// cancel they would die with the steering channel; `cancel_turn` moves
    /// them into history after the interruption marker instead.
    pub(crate) turn_steered_events: Vec<String>,
    /// Set by the engine when the running turn reaches its normal end
    /// (`TurnCompletion`); read when a cancel races that end.
    pub(crate) turn_completion: Option<crate::runtime::TurnCompletion>,
    /// A turn cut from outside `cancel_turn` (driver revocation cancels the
    /// driver turn's token): the stream is given until this instant to
    /// deliver its terminal event, then dropped and the turn finished as
    /// interrupted (`finish_revoked_turn`).
    pub(crate) revoked_turn_deadline: Option<tokio::time::Instant>,
    /// In-flight turn draft for crash recovery (persisting sessions only).
    pub(crate) turn_draft: TurnDraftState,
    /// Session saves and turn-draft writes, off the turn machine
    /// (`session::persister`).
    pub(crate) persister: super::persister::Persister,
    /// The snapshot last handed to the persister: an identical one is not
    /// queued again (a turn's end asks several times for the same state).
    pub(crate) last_queued_save: Option<super::persister::SnapshotKey>,
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
    /// Mirrors `handle.name()`; `sync_name` after any rename.
    pub(crate) name: Arc<arc_swap::ArcSwap<Option<String>>>,
    /// F10: per-session journal ownership lock. Held while Live, released on Park.
    pub(crate) session_lock: Option<agent_core::session_lock::SessionLock>,
    // ── E: session driver (actor-side, behind `session.drive` permission) ──
    pub(crate) driver: Option<super::driver::DriverState>,
    pub(crate) driver_pending: Option<super::driver::DriverPending>,
    /// Generation counter for invalidating stale pending results.
    pub(crate) driver_generation: u64,
    /// Interrupted owner (for generic stop commands across revocation).
    pub(crate) driver_interrupted_owner: Option<String>,
    /// 200 ms cadence for `driver_tick()`, always present.
    pub(crate) driver_tick_interval: tokio::time::Interval,
}

impl SessionActor {
    /// = today's `setup::boot()` per session (foreground_runtime →
    /// resolve_session_and_prompt → set_cwd → model override →
    /// spawn_session_background → finish_session_setup) then
    /// `on_session_start` (keyed injection).
    pub(crate) async fn create(
        host: &Arc<EngineHost>,
        mut cfg: SessionConfig,
    ) -> Result<(SessionHandle, SessionTask)> {
        // cwd goes in BEFORE apply_config (inside foreground_runtime_for) so the
        // one-shot memory binding scopes to the session's project (§4).
        let mut runtime = host.foreground_runtime_for(cfg.cwd.clone()).await?;
        let config: crate::SynapsConfig = (**host.config()).clone();

        let mut sb = crate::engine::setup::resolve_session_and_prompt(
            &mut runtime,
            &cfg.continue_session,
            cfg.system.as_deref(),
            cfg.prompt_manifest.as_deref(),
        )?;
        // `--name` at create: apply BEFORE the registry entry is written so
        // `synaps send --session <name>` and `--continue <name>` resolve
        // from the first second — not only after a later `--continue`.
        if let Some(name) = cfg.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            sb.session
                .set_name(name)
                .map_err(|e| crate::RuntimeError::Session(format!("--name {name:?}: {e}")))?;
            if cfg.persist {
                // Persist the name now so disk resolution (chain/name → id)
                // agrees with the live map even before the first turn.
                if let Err(e) = sb.session.save().await {
                    tracing::warn!(session = %sb.session.id, "failed to save named session at create: {e}");
                }
            }
        }
        runtime.set_cwd(cfg.cwd.clone());
        // T5: rehydrate env from journal when the client sent none.
        // Precedence: explicit Hello.env from the continuing client > journal > None.
        // Never fall back to the daemon's own process env.
        if cfg.env.is_none() {
            if let Some(ref journal_env) = sb.session.env {
                cfg.env = Some(journal_env.clone());
            }
            if cfg.env_stripped.is_empty() && !sb.session.env_stripped.is_empty() {
                cfg.env_stripped = sb.session.env_stripped.clone();
            }
        }
        // Always persist the latest env on the session so the next
        // restart/continue sees it — a fresh client env supersedes
        // whatever was journaled previously.
        if cfg.env.is_some() {
            sb.session.env = cfg.env.clone();
            sb.session.env_stripped = cfg.env_stripped.clone();
        }
        runtime.set_env(cfg.env.clone());
        runtime.set_env_stripped(cfg.env_stripped.clone());
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
        // `startup.extensions_ready_timeout_secs` (config, default 30) bounds
        // the wait on extension discovery; the const remains the fallback.
        let ext_ready_timeout = {
            let secs = config.startup.extensions_ready_timeout_secs;
            if secs == 0 {
                budgets::EXTENSIONS_READY_TIMEOUT
            } else {
                std::time::Duration::from_secs(secs)
            }
        };
        let mut ext_ready = None;
        if cfg.await_extensions {
            if tokio::time::timeout(ext_ready_timeout, host.extensions_ready())
                .await
                .is_err()
            {
                tracing::warn!(
                    budget_secs = ext_ready_timeout.as_secs(),
                    "extensions_ready timed out — on_session_start may miss late extensions"
                );
            }
            crate::extensions::loader::emit_session_start(runtime.hook_bus(), &sb.session.id)
                .await;
        } else {
            let (tx, rx) = oneshot::channel();
            let waiter_host = Arc::clone(host);
            tokio::spawn(async move {
                let _ = tokio::time::timeout(ext_ready_timeout, waiter_host.extensions_ready())
                    .await;
                let _ = tx.send(());
            });
            ext_ready = Some(rx);
        }

        let id = SessionId::from(sb.session.id.clone());

        // F10: acquire the per-session journal ownership lock.
        let lock_kind = if cfg.await_extensions { "daemon" } else { "tui" };
        let session_lock = if sb.continued {
            // Continuing an existing session — the lock MUST succeed.
            // If another process holds it, refuse with an actionable error.
            let dir = agent_core::session_lock::sessions_dir();
            let holder = agent_core::session_lock::LockHolder {
                pid: std::process::id(),
                kind: lock_kind.to_string(),
            };
            match agent_core::session_lock::SessionLock::try_acquire(&dir, &sb.session.id, holder) {
                Ok(lock) => Some(lock),
                Err(agent_core::session_lock::SessionLockError::Held { session_id, holder }) => {
                    let msg = agent_core::session_lock::SessionLockError::Held { session_id, holder };
                    return Err(crate::RuntimeError::Session(msg.to_string()));
                }
                Err(e) => {
                    tracing::warn!(session = %sb.session.id, "session lock: {e}");
                    None
                }
            }
        } else {
            // Fresh session — best-effort lock.
            let dir = agent_core::session_lock::sessions_dir();
            let holder = agent_core::session_lock::LockHolder {
                pid: std::process::id(),
                kind: lock_kind.to_string(),
            };
            match agent_core::session_lock::SessionLock::try_acquire(&dir, &sb.session.id, holder) {
                Ok(lock) => Some(lock),
                Err(e) => {
                    tracing::warn!(session = %sb.session.id, "session lock: {e}");
                    None
                }
            }
        };

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
            locked_by: None,
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
        // Crash recovery (the last process died mid-turn): only the lock
        // holder may fold the turn draft in, persist it and remove it.
        if sb.continued && cfg.persist && session_lock.is_some() {
            crate::engine::setup::recover_turn_draft(&mut conv).await;
        }

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
                name,
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
            prompt_abandon_deadline: None,
            stream: None,
            cancel: None,
            steer_tx: None,
            streaming: false,
            turn_baseline: 0,
            consecutive_auto_turns: 0,
            turn_steered_events: Vec::new(),
            turn_completion: None,
            revoked_turn_deadline: None,
            turn_draft: TurnDraftState::default(),
            persister: super::persister::Persister::new(agent_core::session_lock::sessions_dir()),
            last_queued_save: None,
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
            name,
            session_lock,
            driver: None,
            driver_pending: None,
            driver_generation: 0,
            driver_interrupted_owner: None,
            driver_tick_interval: {
                let mut interval = tokio::time::interval(Duration::from_millis(200));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval
            },
        };
        Ok((handle, SessionTask(actor)))
    }

    /// Publish `conv.session.name` to the handle (listings, `--continue
    /// <name>`) and rewrite this session's registry entry so `synaps send
    /// --session <name>` resolves the new name. Call after any rename.
    pub(crate) fn sync_name(&mut self) {
        let name = self.conv.session.name.clone();
        self.meta.name = name.clone();
        self.name.store(Arc::new(name.clone()));
        if let Err(e) = crate::events::registry::update_session_name(self.id.as_str(), name.as_deref()) {
            tracing::warn!(session = %self.id, "registry name update failed: {e}");
        }
    }

    // ── emit ─────────────────────────────────────────────────────────────

    /// The ONLY seq++ site. Pushes to `turn_replay` while streaming, except
    /// prompt traffic (never replayed), per-client replies, and full-history
    /// envelopes (`MessageHistory`, `Conversation`): the engine publishes one
    /// per round, and an attaching client already gets the LATEST history in
    /// its snapshot's `conversation` — replaying older ones would ship the
    /// whole history once per round and roll its mirror back to a stale
    /// state. The ring's `TurnStarted` drops `user_text`: that prompt is
    /// already in the snapshot's history (a replayed copy would show it
    /// twice). Completed rounds leave the ring when their checkpoint is
    /// adopted (`trim_replay_to_checkpoint`).
    pub(crate) fn emit(&mut self, event: SessionEventWire) {
        let replay = self.streaming
            && !matches!(
                event,
                SessionEventWire::Prompt(_)
                    | SessionEventWire::PromptResolved { .. }
                    | SessionEventWire::Attached { .. }
                    | SessionEventWire::QueryResult { .. }
                    | SessionEventWire::Conversation(_)
                    | SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::MessageHistory(_)
                    ))
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
            let mut ring = env.clone();
            if let SessionEventWire::TurnStarted { user_text, .. } = &mut ring.event {
                *user_text = None;
            }
            self.turn_replay.push_back(ring);
        }
        // No receivers is not an error: streams are not tied to clients.
        let _ = self.events.send(env);
    }

    /// A round checkpoint was adopted: every round in it is now in the
    /// attach snapshot's `conversation`, so the replay ring drops those
    /// rounds' display events — a mid-turn attach would otherwise render
    /// them twice (once from history, once from the ring). The engine
    /// publishes the checkpoint at a round boundary, so a tool call and its
    /// result are always on the same side of the cut. Kept: the turn's
    /// `TurnStarted`, notices, subagent progress, `Usage`.
    fn trim_replay_to_checkpoint(&mut self) {
        self.turn_replay.retain(|env| {
            !matches!(
                env.event,
                SessionEventWire::Stream(StreamEvent::Llm(_))
                    | SessionEventWire::Stream(StreamEvent::Agent(
                        AgentEvent::SteeringDelivered { .. }
                    ))
            )
        });
    }

    pub(crate) fn emit_conversation(&mut self) {
        let snap = self.conv.snapshot(self.consecutive_auto_turns);
        self.emit(SessionEventWire::Conversation(snap));
    }

    pub(crate) async fn publish_view(&mut self) {
        let v = RuntimeView::from_runtime(&self.runtime).await;
        self.view.store(Arc::new(v));
    }

    /// Queue a save of the current conversation. Never waits: the write
    /// runs on the actor's persister (latest wins), so a slow disk cannot
    /// stall the turn machine.
    pub(crate) fn request_save(&mut self) {
        if !self.config.persist || !self.conv.is_live() {
            return;
        }
        let Some(session) = self.conv.prepare_save() else {
            return;
        };
        let key = super::persister::SnapshotKey::of(&session);
        // This exact state is already queued or on disk — unless the last
        // write failed, in which case it is queued again.
        if self.last_queued_save.as_ref() == Some(&key) && self.persister.saves_ok() {
            return;
        }
        self.last_queued_save = Some(key);
        self.persister.save(session);
    }

    /// Wait, at most `SAVE_TIMEOUT`, until every queued save and draft
    /// operation has been applied. `false` on a failed save or the timeout
    /// (the queued writes still land, in order, when the disk catches up).
    pub(crate) async fn flush_saves(&mut self) -> bool {
        let ok = match tokio::time::timeout(budgets::SAVE_TIMEOUT, self.persister.flush()).await {
            Ok(ok) => ok,
            Err(_) => {
                tracing::warn!(session = %self.id, "session save still pending after the save budget");
                false
            }
        };
        if !ok {
            // Whatever failed is not known to be on disk: queue it again next
            // time instead of skipping it as a duplicate.
            self.last_queued_save = None;
        }
        ok
    }

    /// Save now: queue the current conversation and wait for it (bounded,
    /// see `flush_saves`).
    pub(crate) async fn save(&mut self) -> bool {
        self.request_save();
        self.flush_saves().await
    }

    /// A turn is over: announce `Idle` only once everything it queued is on
    /// disk (bounded, `flush_saves`). `Idle` has always meant "the session
    /// is saved": a client or script that reads the session, quits or
    /// hands off on `Idle` must never see the previous state. Per-round
    /// saves stay in the background; this waits once per turn.
    pub(crate) async fn announce_idle(&mut self) {
        self.flush_saves().await;
        self.emit(SessionEventWire::Idle);
    }

    /// Wall 1 — actor-owned context-head checkpoint persistence.
    ///
    /// The receipt stays in-process (actor task → runtime stream task).
    /// It never crosses the wire — `SessionEventWire` maps this to `Done`.
    ///
    /// Park ordering: `can_park()` requires `!self.streaming`, and streaming
    /// is only cleared on stream completion/error. The checkpoint event
    /// arrives mid-stream, so park cannot race with a pending checkpoint.
    /// No explicit drain is needed.
    async fn handle_context_head_checkpoint(
        &mut self,
        session_id: String,
        messages: Vec<crate::SharedMessage>,
        receipt: agent_core::core::context_head::ContextHeadReceipt,
    ) {
        // Stale session id guard (compaction may have changed it).
        if session_id != self.conv.session.id {
            tracing::warn!(
                event_id = %session_id,
                actor_id = %self.conv.session.id,
                "context head checkpoint: stale session id — ignoring"
            );
            receipt.complete(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "stale session id after compaction",
            )));
            return;
        }

        // Non-persistent sessions (tests / --no-persist): complete without I/O.
        if !self.config.persist {
            receipt.complete(Ok(()));
            return;
        }

        // A queued ordinary save must not land after (and over) the durable
        // head: drain the persister first.
        if !self.persister.flush().await {
            tracing::warn!(session = %self.id, "context head checkpoint: an earlier save failed");
        }
        let result = self.conv.persist_context_head(&session_id, messages).await;
        if let Err(ref e) = result {
            tracing::error!(session = %session_id, "context head checkpoint save failed: {e}");
        }
        let succeeded = result.is_ok();
        receipt.complete(result);

        // P5: observe_checkpoint — revoke driver on failure or mismatch.
        if let Some(driver) = &self.driver {
            if !succeeded
                || driver.grant.session_id != session_id
                || session_id != self.conv.session.id
            {
                self.driver_revoke("context checkpoint failed or session replaced");
            }
        }
    }


    /// C3: the checkpoint reply payload (`SessionReloadRecord`).
    pub(crate) fn reload_record(&self) -> SessionReloadRecord {
        let view = self.view.load();
        SessionReloadRecord {
            config: self.config.clone(),
            keep_warm: self.keep_warm,
            lifecycle: SessionLifecycle::from_u8(
                self.lifecycle.load(std::sync::atomic::Ordering::Acquire),
            ),
            settings_replay: self.settings_replay.clone(),
            model: view.model.clone(),
            thinking_level: view.thinking_level.clone(),
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
        self.rearm_prompt_abandon();
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
            && self.driver.is_none()
    }

    /// (E-P7, §S3) Return the first breached spend ceiling, if any, as
    /// `(scope, cost_observed, cap)`. Checks the host-owned per-session cap
    /// (`config.max_session_cost`) and, when a driver is armed, the effective
    /// per-run cap (`min(plugin grant, host)`) measured from `cost_at_arm`.
    /// `None` means every configured ceiling still has headroom.
    fn cost_cap_breach(&self) -> Option<(&'static str, f64, f64)> {
        let total = self.conv.session_cost;
        if let Some(cap) = self.config.max_session_cost {
            if cap.is_finite() && total >= cap {
                return Some(("session", total, cap));
            }
        }
        if let Some(driver) = &self.driver {
            // Host cap for the run is not yet a config field (S3 should-have
            // #4, daemon-level); pass `None` so the plugin's proposal stands
            // alone until the daemon cap lands.
            if let Some(cap) = driver.grant.effective_cost_cap(None) {
                let run_cost = total - driver.cost_at_arm;
                if cap.is_finite() && run_cost >= cap {
                    return Some(("run", run_cost, cap));
                }
            }
        }
        None
    }

    /// `<sessions>/<id>.json` — written by both persistence modes.
    fn journal_exists(&self) -> bool {        let id = (**self.journal_id.load()).clone();
        crate::config::resolve_write_path("sessions")
            .join(format!("{id}.json"))
            .is_file()
    }

    // ── E: driver arm/revoke ─────────────────────────────────────────────

    /// Invalidate the driver: drop `DriverState`, cancel pending work, emit
    /// `DriverRevoked` with any undelivered steering, and release the host grant.
    pub(crate) fn driver_revoke(&mut self, reason: &str) {
        let undelivered = self
            .driver
            .as_mut()
            .map(|d| d.steering.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        let was_active = self.driver.is_some() || self.driver_pending.is_some();
        // Dropping the driver below cancels the driver turn's token.
        if self.driver.as_ref().is_some_and(|d| d.awaiting_terminal) {
            self.note_cancel_cause(crate::engine::interrupt::InterruptReason::Driver);
        }
        if let Some(driver) = &self.driver {
            self.driver_interrupted_owner = Some(driver.grant.plugin_id.clone());
            self.host
                .release_driver(&driver.grant.plugin_id, &self.id);
        }
        self.driver_generation = self.driver_generation.wrapping_add(1);
        self.driver_pending = None;
        self.driver = None;
        if was_active {
            self.emit(SessionEventWire::DriverRevoked {
                reason: reason.to_string(),
                undelivered_steering: undelivered,
            });
        }
        // Dropping `DriverState` cancelled the driver turn's token (a child
        // of the driver's): that turn is over, but not through `cancel_turn`.
        // Unwind prompts now (as `cancel_turn` does), then give its stream
        // the drain budget to deliver its cancel-path history and terminal
        // event (`finish_revoked_turn`); a stream that never started (its
        // start task was just aborted) ends at the next loop turn. A caller
        // that cancels the turn itself right after (`finish`, `checkpoint`,
        // the cost cap) supersedes this: `cancel_turn` clears the deadline.
        if self.turn_cancelled() && self.revoked_turn_deadline.is_none() {
            self.resolve_pending_prompts();
            let wait = if self.stream.is_some() {
                budgets::CANCEL_DRAIN_TIMEOUT
            } else {
                std::time::Duration::ZERO
            };
            self.revoked_turn_deadline = Some(tokio::time::Instant::now() + wait);
        }
        self.rearm_park();
    }

    /// Spawn the plugin's start command as an async task; the result is
    /// processed in `driver_tick()` (P4). Invokes the command via the
    /// extension manager, just like the TUI's `start_command`.
    pub(crate) fn driver_start(&mut self, plugin: String, command: String, arg: String) {
        // Revoke any existing driver first (TUI :369).
        if self.driver.is_some() || self.driver_pending.is_some() {
            self.driver_revoke("explicit command");
        }

        let manager = self.host.ext_manager().clone();
        let session_id = self.id.0.clone();

        // Resolve handler + check permissions synchronously.
        let (handler, timeout) = match manager.try_read() {
            Ok(mgr) => {
                let handler = match mgr.user_action_handler(&plugin) {
                    Ok(h) => h,
                    Err(error) => {
                        self.emit(SessionEventWire::SystemNotice(error));
                        return;
                    }
                };
                let timeout = if mgr.session_driver_handler(&plugin).is_ok() {
                    5
                } else {
                    self.emit(SessionEventWire::SystemNotice(
                        "session driver extension lacks validated session.drive permission".into(),
                    ));
                    return;
                };
                (handler, timeout)
            }
            Err(_) => {
                self.emit(SessionEventWire::SystemNotice(
                    "extensions are loading or busy — try again shortly".into(),
                ));
                return;
            }
        };

        let handler_generation = super::driver::live_generation(&handler).ok();

        let owner = plugin.clone();
        let cmd = command.clone();
        let args: Vec<String> = arg.split_whitespace().map(str::to_owned).collect();

        let generation = self.driver_generation;
        let task_handler = handler.clone();
        self.driver_pending = Some(super::driver::DriverPending {
            generation,
            session_id: session_id.clone(),
            task: super::driver::Task(tokio::spawn(async move {
                let (sink, collector) =
                    invoke_event_channel(InvokeOutputBudget::default());
                let request_id = uuid::Uuid::new_v4().to_string();
                let (result, report) =
                    tokio::time::timeout(Duration::from_secs(timeout), async {
                        tokio::join!(
                            task_handler.invoke_command(&cmd, args, &request_id, sink),
                            collector.collect()
                        )
                    })
                    .await
                    .unwrap_or_else(|_| {
                        (
                            Err(format!("interactive command timed out ({timeout}s)")),
                            crate::extensions::invoke_output::InvokeOutputReport {
                                events: Vec::new(),
                                counters: Default::default(),
                            },
                        )
                    });
                super::driver::TaskResult::Command {
                    owner,
                    command,
                    handler: task_handler,
                    handler_generation,
                    result,
                    report,
                }
            })),
        });
    }

    /// Arm the driver after a successful start command result. Mirrors
    /// the TUI's `arm()` (:538-602). Called from P4's `driver_tick()`.
    pub(crate) fn driver_arm(
        &mut self,
        owner: String,
        handler: Arc<dyn crate::extensions::runtime::ExtensionHandler>,
        handler_generation: u64,
        reply: Reply,
    ) -> std::result::Result<(), String> {
        let manager = self.host.ext_manager().clone();
        super::driver::same_handler(&manager, &owner, &handler, handler_generation)?;

        if self.driver.is_some() {
            return Err("cannot replace an armed grant".into());
        }

        let session_id = self.id.0.clone();
        let (grant, proposal) = Grant::from_start(&owner, &session_id, reply)?;

        // Claim the host-level single-tenancy grant.
        self.host
            .claim_driver(&owner, &self.id)
            .map_err(|e| e.to_string())?;

        let cancel = CancellationToken::new();
        let workers = self.runtime.subagent_registry().clone();
        let worker_epoch = workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set_spawn_cancellation(Some(cancel.clone()));
        let deadline_task = grant.deadline().map(|deadline| {
            let c = cancel.clone();
            let w = workers.clone();
            super::driver::Task(tokio::spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                c.cancel();
                super::driver::cancel_workers(&w, worker_epoch);
            }))
        });

        let models = grant.models().to_vec();
        let proposal_notice = proposal.notice.clone();
        let selection = proposal.selection.clone();
        let deadline_ms = super::driver::deadline_ms(&grant);
        let run_id = grant.run_id.clone();
        let cost_at_arm = self.conv.session_cost;

        let mut state = super::driver::DriverState {
            selection: proposal.selection.clone(),
            grant,
            handler,
            handler_generation,
            cancel,
            workers,
            worker_epoch,
            deadline_task,
            proposal: None,
            awaiting_terminal: false,
            outcome: None,
            feedback: crate::extensions::feedback::Tracker::default(),
            completed_feedback: "unknown",
            steering: VecDeque::new(),
            auto_wakes_blocked: false,
            cost_at_arm,
        };

        super::driver::schedule(&mut state, proposal)?;

        // Apply context mode (session-only, like /context auto/off).
        let context_notice = state.grant.apply_context_mode(&self.runtime)?;
        if let Some(text) = context_notice {
            self.emit(SessionEventWire::SystemNotice(text));
        }

        self.driver_interrupted_owner = None;
        self.driver = Some(state);

        self.emit(SessionEventWire::DriverArmed {
            plugin_id: owner,
            run_id,
            models,
            selection,
            deadline_ms,
            notice: proposal_notice,
        });

        Ok(())
    }

    // ── E-P4: driver_tick ────────────────────────────────────────────────

    /// Actor-equivalent of the TUI's `idle_conflict()` (TUI :437-463).
    /// Returns `Some(reason)` when the driver must defer or revoke.
    fn driver_idle_conflict(&self) -> Option<&'static str> {
        if self.conv.queued_message.is_some() || !self.conv.pending_events.is_empty() {
            return Some("other queued work");
        }
        if self.compact.is_some() {
            return Some("compaction");
        }
        if self.ext_ready.is_some() {
            return Some("extensions loading or reloading");
        }
        if self.conv.context_head.is_blocked(&self.conv.session) {
            return Some("unverified context head");
        }
        if self.driver_completion_blocked() {
            return Some("outstanding workers require collection/reconciliation");
        }
        None
    }

    /// Actor-equivalent of the TUI's `completion_blocked()` (TUI :465-480).
    fn driver_completion_blocked(&self) -> bool {
        use agent_core::orchestration::CompletionGate;
        self.runtime.orchestration().is_some_and(|o| {
            !matches!(o.completion_gate(), CompletionGate::Allowed)
        }) || self
            .runtime
            .subagent_registry()
            .lock()
            .map(|r| {
                r.list_active().iter().any(|(_, _, status)| {
                    *status == crate::runtime::subagent::SubagentStatus::Running
                })
            })
            .unwrap_or(true)
    }

    /// 200 ms timer arm — the core driver loop. Every invocation performs only
    /// bounded state transitions; all plugin and provider IO is in abort-on-drop
    /// spawned tasks. Mirrors the TUI's `tick()` (TUI :695-1027).
    pub(crate) async fn driver_tick(&mut self) {
        use crate::extensions::session_driver::Outcome;

        // ── 1. Cleanup cancelled stream setup ────────────────────────────
        // (e.g. the grant's deadline cancelled the driver token before the
        // turn's stream started): an interrupted turn, ended like any other.
        if self.stream.is_none() && self.turn_cancelled() {
            self.finish_revoked_turn(false).await;
        }

        // ── 2. Validate lifecycle ────────────────────────────────────────
        if let Some(driver) = &self.driver {
            let invalid = if driver.grant.session_id != self.id.0 {
                Some("session replaced".to_string())
            } else if driver.grant.expired() {
                Some("grant deadline reached".into())
            } else if driver.cancel.is_cancelled() {
                Some("canceled".into())
            } else {
                let manager = self.host.ext_manager().clone();
                super::driver::same_handler(
                    &manager,
                    &driver.grant.plugin_id,
                    &driver.handler,
                    driver.handler_generation,
                )
                .err()
            };
            if let Some(reason) = invalid {
                self.driver_revoke(&reason);
                return;
            }
        }

        // ── 3. Idle conflict (only when driver is armed) ─────────────────
        if self.driver.is_some() {
            // (E-P7, §S1) No clients → no headless spend. `detach()` revokes on
            // the last-client transition; this is the belt-and-suspenders check
            // for any path that leaves a grant armed with zero clients.
            if self.attached.is_empty() {
                self.driver_revoke("no clients attached");
                return;
            }
            // F10/F14: use the full driver_idle_conflict() instead of an
            // inline subset that was missing `completion_blocked`.
            if let Some(reason) = self.driver_idle_conflict() {
                self.driver_revoke(reason);
                return;
            }

            // Let the reactor classify queued events before deciding whether they
            // are competing work or steering within this owned turn.
            if !self.runtime.event_queue().is_empty() {
                return;
            }
        }

        // ── 4. Process finished pending task ─────────────────────────────
        if self
            .driver_pending
            .as_ref()
            .is_some_and(|p| p.task.0.is_finished())
        {
            let mut pending = self.driver_pending.take().expect("checked pending");
            if pending.generation != self.driver_generation
                || pending.session_id != self.id.0
            {
                return;
            }
            let result = match (&mut pending.task.0).await {
                Ok(result) => result,
                Err(error) => {
                    self.driver_revoke("driver task failed");
                    self.emit(SessionEventWire::SystemNotice(format!(
                        "Session driver task failed: {error}"
                    )));
                    return;
                }
            };
            match result {
                super::driver::TaskResult::Command {
                    owner,
                    command: _,
                    handler,
                    handler_generation,
                    result,
                    report,
                } => {
                    let current = self
                        .host
                        .ext_manager()
                        .try_read()
                        .ok()
                        .and_then(|m| m.user_action_handler(&owner).ok());
                    if !current
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &handler))
                    {
                        self.driver_revoke(
                            "command owner unloaded, replaced, or unavailable",
                        );
                        return;
                    }
                    let output_failed = report.is_limited()
                        || report.events.iter().any(|event| {
                            matches!(
                                event,
                                crate::extensions::runtime::InvokeCommandEvent::Output(
                                    crate::extensions::commands::CommandOutputEvent::Error { .. }
                                )
                            )
                        });
                    // F7 (shady should-fix): plugin command output is visible
                    // to the user, but BATCHED into one notice (cap 20 lines) —
                    // a chatty `/auto start` must not flood the client with one
                    // wire event per line.
                    let mut lines: Vec<String> = Vec::new();
                    for event in &report.events {
                        if let crate::extensions::runtime::InvokeCommandEvent::Output(out) = event {
                            let text = match out {
                                crate::extensions::commands::CommandOutputEvent::Text { content }
                                | crate::extensions::commands::CommandOutputEvent::System { content } => {
                                    Some(content.clone())
                                }
                                crate::extensions::commands::CommandOutputEvent::Error { content } => {
                                    Some(format!("Error: {content}"))
                                }
                                _ => None,
                            };
                            if let Some(text) = text {
                                lines.push(text);
                            }
                        }
                    }
                    if !lines.is_empty() {
                        const MAX_LINES: usize = 20;
                        let truncated = lines.len() > MAX_LINES;
                        if truncated {
                            lines.truncate(MAX_LINES);
                            lines.push("…output truncated".into());
                        }
                        self.emit(SessionEventWire::SystemNotice(lines.join("\n")));
                    }
                    if let Some(notice) = report.limit_notice() {
                        self.emit(SessionEventWire::SystemNotice(format!(
                            "command output truncated: {}",
                            notice.message,
                        )));
                    }
                    if let Ok(value) = result {
                        let result = match protocol::parse_reply(&value) {
                            Ok(Some(reply @ Reply::Start { .. })) => {
                                if output_failed {
                                    Err("command reported an error or exceeded its output budget"
                                        .into())
                                } else if self.streaming || self.stream.is_some() {
                                    Err("foreground turn still active".into())
                                } else if let Some(reason) = self.driver_idle_conflict() {
                                    Err(reason.into())
                                } else {
                                    match handler_generation {
                                        Some(generation) => {
                                            self.driver_arm(
                                                owner, handler, generation, reply,
                                            )
                                        }
                                        None => Err(
                                            "command owner did not expose a live lifecycle at dispatch"
                                                .into(),
                                        ),
                                    }
                                }
                            }
                            Ok(Some(
                                Reply::Stop { notice: text }
                                | Reply::Status { notice: text },
                            )) => {
                                self.emit(SessionEventWire::SystemNotice(text));
                                Ok(())
                            }
                            Ok(Some(Reply::Next { .. })) => Err(
                                "next requires an armed poll, not an interactive command"
                                    .into(),
                            ),
                            Ok(None) => Ok(()),
                            Err(error) => Err(error),
                        };
                        if let Err(error) = result {
                            self.emit(SessionEventWire::SystemNotice(format!(
                                "Session driver rejected: {error}"
                            )));
                        }
                    }
                }
                super::driver::TaskResult::Poll(result) => {
                    let Some(driver) = self.driver.as_mut() else {
                        return;
                    };
                    let stop_notice = match &result {
                        Ok(Reply::Stop { notice }) => Some(notice.clone()),
                        _ => None,
                    };
                    match result.and_then(|reply| driver.grant.accept(reply)) {
                        Ok(Some(proposal)) => {
                            let text = proposal.notice.clone();
                            if let Err(error) =
                                super::driver::schedule(driver, proposal)
                            {
                                self.driver_revoke(&error);
                            } else {
                                self.emit(SessionEventWire::SystemNotice(text));
                            }
                        }
                        Ok(None) => {
                            self.driver_revoke("owner requested stop");
                            if let Some(text) = stop_notice {
                                self.emit(SessionEventWire::SystemNotice(text));
                            }
                        }
                        Err(error) => {
                            self.driver_revoke(&format!("invalid poll response: {error}"))
                        }
                    }
                }
                super::driver::TaskResult::Prepared { proposal, result } => {
                    if self.streaming || self.stream.is_some() {
                        self.driver_revoke("foreground work took priority");
                        return;
                    }
                    if let Some(reason) = self.driver_idle_conflict() {
                        self.driver_revoke(reason);
                        return;
                    }
                    match *result {
                        Err(protocol::PrepareError::Selection(error)) => {
                            self.emit(SessionEventWire::SystemNotice(format!(
                                "Session driver selection rejected (nothing sent): {error}"
                            )));
                            if let Some(driver) = self.driver.as_mut() {
                                driver.outcome = Some((
                                    Outcome::SelectionRejected,
                                    "unknown".into(),
                                ));
                            }
                        }
                        Err(protocol::PrepareError::Blocked(error)) => {
                            self.driver_revoke(&format!("preflight blocked: {error}"));
                        }
                        Ok(candidate) => {
                            let validated = proposal.clone();
                            let history = super::driver::history_with_steering(
                                &self.conv.api_messages,
                                &self
                                    .driver
                                    .as_ref()
                                    .map(|d| &d.steering)
                                    .cloned()
                                    .unwrap_or_default(),
                            );
                            if let Err(error) = protocol::validate_prepared(
                                &candidate, &validated, &history,
                            ) {
                                self.driver_revoke(&format!(
                                    "prepared selection changed before commit: {error}"
                                ));
                                return;
                            }
                            // F3: commit steering + prompt via the
                            // driver.rs helper (single call site).
                            // Destructure to split borrows across conv fields.
                            {
                                let driver = self.driver.as_mut().expect("driver present");
                                let conv = &mut *self.conv;
                                super::driver::commit_submission(
                                    &mut conv.api_messages,
                                    &mut driver.steering,
                                    &proposal.prompt,
                                );
                            }
                            // Apply the prepared runtime.
                            *self.runtime = candidate;
                            self.conv.session.model = self.runtime.model().to_owned();
                            self.conv.session.thinking_level =
                                self.runtime.thinking_level().to_owned();
                            self.consecutive_auto_turns = 0;
                            self.turn_baseline = self.conv.api_messages.len();
                            self.turn_steered_events.clear();
                            self.turn_replay.clear();
                            self.streaming = true;
                            self.open_turn_draft();
                            self.update_attach_state();
                            self.emit(SessionEventWire::TurnStarted {
                                turn_baseline: self.turn_baseline,
                                trigger: TurnTrigger::DriverAuto,
                                user_text: Some(proposal.prompt.clone()),
                            });

                            // Set up the driver's awaiting_terminal.
                            if let Some(driver) = self.driver.as_mut() {
                                driver.awaiting_terminal = true;
                                driver.completed_feedback = "unknown";
                                if driver.grant.feedback_enabled() {
                                    driver.feedback.begin_turn(
                                        &driver.selection.model,
                                        &driver.selection.effort,
                                    );
                                }
                            }

                            // Spawn stream start as a task (never block tick).
                            let handler = self
                                .driver
                                .as_ref()
                                .expect("driver")
                                .handler
                                .clone();
                            let handler_generation = self
                                .driver
                                .as_ref()
                                .expect("driver")
                                .handler_generation;
                            let ct = self
                                .driver
                                .as_ref()
                                .expect("driver")
                                .cancel
                                .child_token();
                            self.cancel = Some(ct.clone());
                            // Cancelled by anything that notes no cause (the
                            // grant's deadline task): the driver ending.
                            let completion = crate::runtime::TurnCompletion::with_default_cause(
                                crate::runtime::CancelCause::Driver,
                            );
                            self.turn_completion = Some(completion.clone());
                            let (tx, rx) = mpsc::unbounded_channel();
                            self.steer_tx = Some(tx);
                            let mut runtime = self.runtime.clone();
                            // F2: share the parent's TTL latches so the
                            // driver turn doesn't fire a redundant 1h→5m
                            // downgrade notice.
                            runtime.share_ttl_latches(&self.runtime);
                            let history = self.conv.api_messages.clone();
                            let secret = self.secret_prompt_handle.clone();
                            let session_id = self.id.0.clone();
                            let generation = self.driver_generation;
                            self.driver_pending = Some(super::driver::DriverPending {
                                generation,
                                session_id,
                                task: super::driver::Task(tokio::spawn(async move {
                                    if ct.is_cancelled() {
                                        return super::driver::TaskResult::Started(
                                            Err("canceled before stream setup".into()),
                                        );
                                    }
                                    if let Err(error) =
                                        super::driver::same_lifecycle(
                                            &handler,
                                            handler_generation,
                                        )
                                    {
                                        return super::driver::TaskResult::Started(
                                            Err(error),
                                        );
                                    }
                                    tokio::select! {
                                        biased;
                                        _ = ct.cancelled() => super::driver::TaskResult::Started(Err("canceled during stream setup".into())),
                                        started = runtime.run_stream_tracked(history, ct.clone(), Some(rx), Some(secret), false, completion) => super::driver::TaskResult::Started(Ok(started)),
                                    }
                                })),
                            });

                            // Start subagent tick (same as normal start_turn).
                            let mut tick = tokio::time::interval(
                                std::time::Duration::from_secs(1),
                            );
                            tick.set_missed_tick_behavior(
                                tokio::time::MissedTickBehavior::Delay,
                            );
                            tick.reset();
                            self.subagent_tick = Some(tick);
                            self.request_save();
                        }
                    }
                }
                super::driver::TaskResult::Started(started) => match started {
                    Ok(started) if self.driver.is_some() => {
                        self.stream = Some(started);
                    }
                    Err(error) => {
                        self.driver_revoke(&error);
                    }
                    _ => {}
                },
            }
        }

        // ── 5. Idle scheduling ───────────────────────────────────────────
        if self.streaming || self.stream.is_some() || self.driver_pending.is_some() {
            return;
        }
        if let Some(reason) = self.driver_idle_conflict() {
            self.driver_revoke(reason);
            return;
        }
        let session_cost = self.conv.session_cost;
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        if let Some((outcome, error_kind)) = driver.outcome.take() {
            let request = super::driver::poll_request(driver, outcome, error_kind, session_cost);
            let handler = driver.handler.clone();
            let session_id = self.id.0.clone();
            let generation = self.driver_generation;
            self.driver_pending = Some(super::driver::DriverPending {
                generation,
                session_id,
                task: super::driver::Task(tokio::spawn(async move {
                    // F4: poll must not hang forever — 30 s timeout like
                    // the prepare path's 5 s.
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        protocol::poll(handler, request),
                    )
                    .await
                    .unwrap_or_else(|_| Err("poll timed out (30s)".into()));
                    super::driver::TaskResult::Poll(result)
                })),
            });
        } else if driver
            .proposal
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.due)
        {
            let proposal =
                driver.proposal.take().expect("checked proposal").proposal;
            // Clone is read-only (prepare never sends a request) and
            // discarded — no TTL latch sharing needed.
            let runtime = self.runtime.clone();
            let history = super::driver::history_with_steering(
                &self.conv.api_messages,
                &driver.steering,
            );
            let session_id = self.id.0.clone();
            let generation = self.driver_generation;
            self.driver_pending = Some(super::driver::DriverPending {
                generation,
                session_id,
                task: super::driver::Task(tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        Duration::from_secs(5),
                        protocol::prepare(&runtime, &proposal, &history),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(protocol::PrepareError::Blocked(
                            "preparation timed out (5s)".into(),
                        ))
                    });
                    super::driver::TaskResult::Prepared {
                        proposal,
                        result: Box::new(result),
                    }
                })),
            });
        }
    }

    /// F10: the session lock follows the conversation to `new_id` (a fresh
    /// id from NewSession or compaction). Best-effort: on failure the old
    /// lock is still released (the conversation left it) and the failure
    /// logged. The new lock is taken BEFORE the old one is dropped.
    pub(crate) fn reacquire_session_lock(&mut self, new_id: &str) {
        match Self::try_lock_session(new_id) {
            Ok(lock) => self.session_lock = Some(lock),
            Err(e) => {
                tracing::warn!(session = %new_id, "reacquire session lock: {e}");
                self.session_lock = None;
            }
        }
    }

    /// Try to take the journal lock on `id` for this daemon process.
    pub(crate) fn try_lock_session(
        id: &str,
    ) -> std::result::Result<agent_core::session_lock::SessionLock, agent_core::session_lock::SessionLockError>
    {
        let dir = agent_core::session_lock::sessions_dir();
        let holder = agent_core::session_lock::LockHolder {
            pid: std::process::id(),
            kind: "daemon".to_string(),
        };
        agent_core::session_lock::SessionLock::try_acquire(&dir, id, holder)
    }

    /// F18: a session with NO history can never park (nothing to journal),
    /// so with no clients it would stay Live — Runtime resident — forever.
    /// Same gates as `can_park` minus the history requirement: at the park
    /// deadline such a session ends instead.
    pub(crate) fn can_end_idle(&self) -> bool {
        self.attached.is_empty()
            && !self.streaming
            && self.compact.is_none()
            && self.pending_prompts.is_empty()
            && self.conv.is_live()
            && self.conv.api_messages.is_empty()
            && self.conv.queued_message.is_none()
            && !self.keep_warm
            && !self.is_parked()
            // An armed driver keeps the session alive between turns exactly
            // like an attached client would (shady P3/P4 F6: without this, a
            // zero-turn armed session is idle-ended 5 s after the last client
            // detaches, killing the run mid-arm). `can_park` already guards.
            && self.driver.is_none()
            && self.driver_pending.is_none()
    }

    fn rearm_park(&mut self) {
        if self.is_parked() {
            // Eviction deadline is owned by `park()`; the attach path unparks
            // (which rebuilds the runtime and clears the deadline).
            return;
        }
        if self.can_end_idle() {
            // F18: nothing to keep warm — a zero-turn session with no
            // clients ends after a short grace, not the full park grace
            // (which exists to keep a runtime WITH history warm for a
            // quick reconnect).
            if self.park_deadline.is_none() {
                self.park_deadline =
                    Some(tokio::time::Instant::now() + idle_end_grace());
            }
            return;
        }
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

    /// (P11) Bound the resource pin from an ABANDONED prompt. A pending host
    /// confirmation on a plain interactive session survives detach so the user
    /// reattaches and answers it — but if NObody ever comes back it pins the
    /// session forever (`can_park()` requires `pending_prompts` empty). Arm a
    /// deadline only in that abandoned state (zero clients, prompt pending, no
    /// driver); a re-attach clears it (prompt survives, user answers); expiry
    /// denies the prompt(s). The driver-armed case fail-closes immediately in
    /// `detach` and never reaches here. Idempotent: safe to call on every
    /// attach/detach/prompt transition.
    fn rearm_prompt_abandon(&mut self) {
        let armed_driver = self.driver.is_some() || self.driver_pending.is_some();
        let abandoned = self.attached.is_empty()
            && !self.pending_prompts.is_empty()
            && !armed_driver
            && !self.is_parked();
        if !abandoned {
            // Someone is attached, the prompt was answered, or a driver took
            // over the fail-closed path — never auto-deny while any of these.
            self.prompt_abandon_deadline = None;
            return;
        }
        if self.prompt_abandon_deadline.is_none() {
            // `None` config ⇒ deadline stays `None` ⇒ disabled (pre-#112).
            self.prompt_abandon_deadline =
                prompt_abandon_timeout().map(|d| tokio::time::Instant::now() + d);
        }
    }

    /// (P11) The prompt-abandonment deadline expired with still zero clients:
    /// deny every pending prompt (send `None` + emit `PromptResolved`) so the
    /// session stops zombie-blocking and `can_park()` becomes true. A re-attach
    /// would have cleared the deadline first; re-check defensively.
    fn on_prompt_abandon_deadline(&mut self) {
        self.prompt_abandon_deadline = None;
        if !self.attached.is_empty() {
            return;
        }
        let had_prompts = !self.pending_prompts.is_empty();
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }
        if had_prompts {
            tracing::info!(session = %self.id, "pending prompt abandoned (no clients before deadline) — denied");
            self.publish_presence();
            // `can_park()` may now be true — re-evaluate the park deadline.
            self.rearm_park();
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
    pub(crate) async fn park(&mut self) -> std::ops::ControlFlow<EndReason> {
        self.park_deadline = None;
        if self.is_parked() {
            if self.attached.is_empty() {
                tracing::info!(session = %self.id, "parked past the eviction age — leaving the map (journal kept)");
                return std::ops::ControlFlow::Break(EndReason::Evicted);
            }
            return std::ops::ControlFlow::Continue(());
        }
        if self.can_end_idle() {
            tracing::info!(session = %self.id, "idle with no history — ending instead of parking (F18)");
            // No journal will ever exist for this id: take the lock file
            // with us instead of leaving an orphan `.lock` (RC soak F-NEW-1).
            if let Some(lock) = self.session_lock.take() {
                lock.release_and_remove();
            }
            return std::ops::ControlFlow::Break(EndReason::Idle);
        }
        if !self.can_park() {
            return std::ops::ControlFlow::Continue(());
        }
        self.state = AttachState::Parking;
        self.set_lifecycle(SessionLifecycle::Parking);
        // Parking drops the conversation from memory: everything queued must
        // be on disk first, and a failed save keeps the session live.
        if !self.save().await {
            tracing::warn!(session = %self.id, "park: save failed or timed out — staying live");
            self.state = AttachState::Detached { running: false };
            self.set_lifecycle(SessionLifecycle::Live);
            return std::ops::ControlFlow::Continue(());
        }
        if !self.journal_exists() {
            // Never park what cannot be restored (H2).
            tracing::warn!(session = %self.id, "park: no journal on disk — staying live");
            self.state = AttachState::Detached { running: false };
            self.set_lifecycle(SessionLifecycle::Live);
            return std::ops::ControlFlow::Continue(());
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
        // F10: release the journal lock so an in-process --continue can
        // pick up the parked session (the daemon will fail to re-acquire
        // on unpark and surface a clear error instead of a zombie).
        self.session_lock = None;
        self.state = AttachState::Parked;
        self.set_lifecycle(SessionLifecycle::Parked);
        tracing::info!(session = %self.id, "session parked");
        // The park timer is reused as the eviction timer: a session nobody
        // re-attaches to within `parked_evict_after` leaves the map (its
        // journal stays; `--continue` brings it back).
        self.park_deadline = parked_evict_after().map(|d| tokio::time::Instant::now() + d);
        std::ops::ControlFlow::Continue(())
    }

    /// Rebuild `runtime` + `conv` from the journal (`load_session_in_dir`
    /// via `resolve_session_and_prompt`), replay non-persisted settings,
    /// `finish_session_setup(IndexRecord::Skip)`. On error the session
    /// stays Parked (nothing is half-built).
    pub(crate) async fn unpark(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        // Somebody came back: the eviction deadline armed by `park()` is void.
        self.park_deadline = None;
        let journal_id = (**self.journal_id.load()).clone();

        // F10: re-acquire the journal lock before rebuilding the runtime.
        // If an in-process --continue took over while we were parked, fail
        // loudly so the attach gets a clear error instead of a zombie.
        {
            let dir = agent_core::session_lock::sessions_dir();
            let holder = agent_core::session_lock::LockHolder {
                pid: std::process::id(),
                kind: "daemon".to_string(),
            };
            match agent_core::session_lock::SessionLock::try_acquire(&dir, &journal_id, holder) {
                Ok(lock) => self.session_lock = Some(lock),
                Err(agent_core::session_lock::SessionLockError::Held { .. }) => {
                    return Err(crate::RuntimeError::Session(format!(
                        "cannot unpark session {}: journal locked by another process",
                        journal_id
                    )));
                }
                Err(e) => {
                    tracing::warn!(session = %journal_id, "unpark: session lock: {e}");
                    // Non-fatal: proceed without the lock (e.g. read-only fs).
                }
            }
        }

        let host = Arc::clone(&self.host);
        let cfg = self.config.clone();
        let queue = Arc::clone(&self.event_queue);
        let replay = self.settings_replay.clone();
        let journal_present = self.journal_exists();
        let view = self.view.load_full();
        let build = async move {
            let config: crate::SynapsConfig = (**host.config()).clone();
            // §4: cwd before apply_config — see `create`.
            let mut runtime = host.foreground_runtime_for(cfg.cwd.clone()).await?;
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
            runtime.set_env(cfg.env.clone());
            runtime.set_env_stripped(cfg.env_stripped.clone());
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
        if journal_present && self.config.persist && self.session_lock.is_some() {
            crate::engine::setup::recover_turn_draft(&mut conv).await;
        }
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
    ///
    /// The 1 Hz tick that drives this starts with a turn and outlives it
    /// while a background worker (`subagent_start`) is still running: those
    /// workers' progress events rode the ended turn's stream, so these rows
    /// (which carry each worker's step and tool count) are how clients keep
    /// up. The tick stops once no worker is running after the turn, having
    /// published the terminal rows.
    pub(crate) fn publish_subagent_rows(&mut self) {
        let rows = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows();
        let running = rows
            .iter()
            .any(|r| matches!(r.status, crate::runtime::subagent::SubagentStatus::Running));
        if !rows.is_empty() {
            self.emit(SessionEventWire::SubagentRows(rows));
        }
        if !keep_subagent_tick(self.streaming, running) {
            self.subagent_tick = None;
        }
    }

    /// Whether any worker in the registry is still running.
    fn subagents_running(&self) -> bool {
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows()
            .iter()
            .any(|r| matches!(r.status, crate::runtime::subagent::SubagentStatus::Running))
    }

    // ── turn start (dispatch.rs Submit tail / stream_handler.rs RunTurn) ──

    pub(crate) async fn start_turn(&mut self, trigger: TurnTrigger, user_text: Option<String>) {
        let ct = CancellationToken::new();
        let (s_tx, s_rx) = mpsc::unbounded_channel::<String>();
        self.streaming = true;
        self.turn_baseline = self.conv.api_messages.len();
        self.open_turn_draft();
        self.turn_steered_events.clear();
        self.turn_replay.clear();
        self.update_attach_state();
        self.emit(SessionEventWire::TurnStarted {
            turn_baseline: self.turn_baseline,
            trigger,
            user_text,
        });
        // Blocks command processing during setup exactly like the TUI loop.
        // (E-P7, §S9) An armed driver MUST NOT auto-approve tool activation —
        // the session-driver contract keeps ordinary tool-approval gates in
        // force. Override the session config to false whenever a grant is
        // armed, regardless of what the client requested. (Driver-initiated
        // turns already pass `false` from the tick's Prepared arm; this guards
        // any foreground turn that starts while armed.)
        let auto_approve = self.config.auto_approve_confirms && self.driver.is_none();
        let completion = crate::runtime::TurnCompletion::new();
        self.turn_completion = Some(completion.clone());
        let stream = self
            .runtime
            .run_stream_tracked(
                self.conv.api_messages.clone(),
                ct.clone(),
                Some(s_rx),
                Some(self.secret_prompt_handle.clone()),
                auto_approve,
                completion,
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

    /// Every turn-end path (Done/Error/Cancel/stream EOF). `streaming=false`
    /// makes a `Cancel` racing a `Done` the idle no-op. Events steered into
    /// the stream that the engine never took (`turn_steered_events`) go back
    /// to `pending_events`, first: they are flushed into history like any
    /// event that arrived during the turn (the interrupted path takes them
    /// earlier, to place them after the marker). Queues the save of the
    /// turn's final history, then the draft removal (`close_turn_draft`).
    pub(crate) fn clear_stream(&mut self) {
        self.stream = None;
        self.cancel = None;
        self.steer_tx = None;
        self.streaming = false;
        self.turn_completion = None;
        self.revoked_turn_deadline = None;
        // Background workers outlive the turn: keep publishing their rows
        // until the last one finishes (see `publish_subagent_rows`).
        if !keep_subagent_tick(false, self.subagents_running()) {
            self.subagent_tick = None;
        }
        if !self.turn_steered_events.is_empty() && self.conv.is_live() {
            let undelivered = std::mem::take(&mut self.turn_steered_events);
            self.conv.pending_events.splice(0..0, undelivered);
        }
        // The history that ends the turn — whether or not a draft is open
        // (no draft without the session lock) — then the draft removal.
        self.request_save();
        self.close_turn_draft();
        self.update_attach_state();
    }

    // ── in-flight turn draft (crash recovery) ──────────────────────────

    /// Turn start: the draft's existence is the "turn open" signal a
    /// loader uses to detect a process that died mid-turn. Only the session
    /// lock holder writes one: without the lock another process may own
    /// this session, and a draft is a claim that a turn is running HERE.
    fn open_turn_draft(&mut self) {
        if !self.config.persist || !self.conv.is_live() || self.session_lock.is_none() {
            return;
        }
        self.turn_draft = TurnDraftState {
            open: Some(self.conv.session.id.clone()),
            base_len: self.conv.api_messages.len(),
            text: String::new(),
            dirty: true,
        };
        self.flush_turn_draft();
    }

    fn turn_draft_text(&mut self, delta: &str) {
        let d = &mut self.turn_draft;
        if d.open.is_some()
            && d.text.len() < agent_core::core::session_draft::TURN_DRAFT_MAX_TEXT_BYTES
        {
            d.text.push_str(delta);
            d.dirty = true;
        }
    }

    fn turn_draft_reset_text(&mut self) {
        let d = &mut self.turn_draft;
        if d.open.is_some() && !d.text.is_empty() {
            d.text.clear();
            d.dirty = true;
        }
    }

    /// A round checkpoint was adopted and saved: later text continues from it.
    fn turn_draft_committed(&mut self) {
        if self.turn_draft.open.is_none() {
            return;
        }
        self.turn_draft.base_len = self.conv.api_messages.len();
        self.turn_draft.text.clear();
        self.turn_draft.dirty = true;
        self.flush_turn_draft();
    }

    /// Write the draft if it changed (1 Hz turn tick; round checkpoints).
    /// Non-blocking and ordered (`persister`).
    pub(crate) fn flush_turn_draft(&mut self) {
        let Some(open) = self.turn_draft.open.clone() else {
            return;
        };
        if !self.turn_draft.dirty {
            return;
        }
        // Defensive: follow the conversation if its id changed mid-turn.
        let id = self.conv.session.id.clone();
        if open != id {
            self.persister.remove_draft(&open);
            self.turn_draft.open = Some(id.clone());
        }
        self.turn_draft.dirty = false;
        self.persister.write_draft(
            &id,
            agent_core::core::session_draft::TurnDraft {
                base_len: self.turn_draft.base_len,
                partial_text: self.turn_draft.text.clone(),
            },
        );
    }

    /// Turn end (every path goes through `clear_stream`, which has just
    /// queued the save of the history that ends the turn): remove the draft.
    /// Queued after that save, and the persister never removes a draft while
    /// its session's latest save has failed, so the draft cannot disappear
    /// before the history that concludes its turn is on disk.
    fn close_turn_draft(&mut self) {
        if let Some(id) = self.turn_draft.open.take() {
            self.persister.remove_draft(&id);
        }
        self.turn_draft = TurnDraftState::default();
    }

    /// dispatch.rs Submit (:1231-1288) minus presentation.
    pub(crate) async fn submit(
        &mut self,
        text: String,
        attachments: Vec<serde_json::Value>,
        from: Option<ClientId>,
    ) {
        // Wall 1 defense-in-depth: a latched (unverified) context head must
        // not accept new inference. The stream would refuse via
        // `durability_blocked` anyway, but that leaves the user message
        // orphaned in history; refuse here, before it is pushed.
        if self.conv.context_head.is_blocked(&self.conv.session) {
            let reason = "context head is unverified after a failed checkpoint; reload the \
                          session (`--continue`) or start a new one before continuing"
                .to_string();
            // Refused to the submitter (its editor text comes back and it
            // stops expecting this turn), a notice for everyone else.
            match from {
                Some(client) => self.emit(SessionEventWire::Refused {
                    client,
                    command: "submit".into(),
                    reason,
                }),
                None => self.emit(SessionEventWire::SystemNotice(reason)),
            }
            return;
        }
        if self.streaming {
            // A Submit while streaming is what the TUI calls StreamingInput.
            // Attachments during streaming are rejected by the client; if they
            // arrive anyway, ignore them (text-only steer).
            // P6: if the driver is armed and streaming, steer AND update
            // the driver's steering FIFO.
            if let Some(driver) = self.driver.as_mut() {
                let byte_total: usize = driver.steering.iter().map(|s| s.len()).sum();
                if driver.steering.len() >= super::driver::STEERING_MAX_MESSAGES {
                    self.emit(SessionEventWire::SystemNotice(
                        "steering queue full (16 messages) — message dropped".into(),
                    ));
                    return;
                }
                if byte_total + text.len() > super::driver::STEERING_MAX_BYTES {
                    self.emit(SessionEventWire::SystemNotice(
                        "steering queue full (256 KiB) — message dropped".into(),
                    ));
                    return;
                }
                driver.steering.push_back(text.clone());
            }
            self.steer(text);
            return;
        }
        // P6: while the driver is armed and idle, route to steering FIFO
        // instead of starting a normal turn.
        if self.driver.is_some() {
            let driver = self.driver.as_mut().unwrap();
            let byte_total: usize = driver.steering.iter().map(|s| s.len()).sum();
            if driver.steering.len() >= super::driver::STEERING_MAX_MESSAGES {
                self.emit(SessionEventWire::SystemNotice(
                    "steering queue full (16 messages) — message dropped".into(),
                ));
                return;
            }
            if byte_total + text.len() > super::driver::STEERING_MAX_BYTES {
                self.emit(SessionEventWire::SystemNotice(
                    "steering queue full (256 KiB) — message dropped".into(),
                ));
                return;
            }
            driver.steering.push_back(text.clone());
            // Clear auto_wakes_blocked on explicit user submit (user takeover).
            driver.auto_wakes_blocked = false;
            self.emit(SessionEventWire::Steered {
                text,
                delivered: false,
            });
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
        // For every OTHER attached client: the submitter drew its own card.
        let user_text = text.clone();
        let api_content = text;

        if attachments.is_empty() {
            // Text-only: existing path.
            self.conv.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": api_content}),
            ));
        } else {
            // Build multipart content: text block (if non-empty) + attachment blocks.
            let mut blocks = Vec::with_capacity(attachments.len() + 1);
            if !api_content.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": api_content}));
            }
            blocks.extend(attachments);
            let candidate = std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": blocks}),
            );
            // Validate the complete history (including this message) for the
            // session's current model before accepting.
            let mut proposed = self.conv.api_messages.clone();
            proposed.push(candidate.clone());
            let model = self.runtime.model().to_string();
            if let Err(e) = crate::runtime::attachments::validate_messages(&model, &proposed) {
                // Typed refusal: the submitting client gets its editor text
                // back (stream_handler restores `last_submitted` on `Refused`)
                // and keeps its attachment drafts; mirrors see a notice.
                match from {
                    Some(client) => self.emit(SessionEventWire::Refused {
                        client,
                        command: "submit".into(),
                        reason: format!("attachments rejected: {e}"),
                    }),
                    None => self.emit(SessionEventWire::SystemNotice(format!(
                        "attachments rejected: {e}"
                    ))),
                }
                return;
            }
            self.conv.api_messages.push(candidate);
        }
        self.start_turn(TurnTrigger::User, Some(user_text)).await;
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

    /// Every cancel path — `Cancel` (Esc), quit mid-turn (`finish`), the cost
    /// cap, a reload/host checkpoint — behind the TUI's `if streaming` guard
    /// (input.rs:350): a `Cancel` while idle is a no-op that only re-announces
    /// `Idle` — it must never touch history, save, or emit `Aborted`.
    ///
    /// The interrupted turn is recorded as REAL history, never as a recap:
    /// 1. the token is cancelled, then pending host prompts are answered
    ///    `None` (a tool or `before_tool_call` hook blocked on one must unwind);
    /// 2. the still-live stream is drained for a
    ///    bounded time (`drain_cancelled_stream`) so the engine's cancel-path
    ///    history — partial assistant message, completed tool rounds,
    ///    delivered steering, canceled `tool_result`s — plus its final Usage
    ///    and any in-flight context-head checkpoint reach the actor;
    /// 3. that history is adopted verbatim (else the last adopted history is
    ///    kept — every history the engine publishes is valid), then
    ///    `finish_interrupted_turn` APPENDS one interruption marker
    ///    (`engine::interrupt`). Nothing already sent is edited, so the
    ///    provider's cached prefix survives.
    ///
    /// A turn the engine had already finished when the cancel reached it
    /// (Esc a moment after the answer ended) is not interrupted: no marker.
    pub(crate) async fn cancel_turn(&mut self, reason: crate::engine::interrupt::InterruptReason) {
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
        self.note_cancel_cause(reason);
        if let Some(ref ct) = self.cancel {
            ct.cancel();
        }
        // After the cancel, so a tool awaiting a prompt observes the cancel
        // (not a `None` it could read as "declined"); a `before_tool_call`
        // hook awaiting a confirm prompt is not cancel-aware and needs this
        // answer to unwind at all.
        self.resolve_pending_prompts();
        let drain = self.drain_cancelled_stream().await;
        match drain.history {
            Some(history) => self.conv.api_messages = history,
            None if !drain.closed => tracing::warn!(
                session = %self.id,
                budget_ms = budgets::CANCEL_DRAIN_TIMEOUT_MS,
                "cancelled turn did not publish its history within the drain budget; \
                 keeping the last adopted history"
            ),
            None => {}
        }
        // Complete only if the drain consumed the whole stream: the final
        // history is published right after the completion is recorded.
        let completed = drain.closed && self.turn_completed();
        self.finish_interrupted_turn(reason, completed).await;
    }

    /// The engine recorded the running turn's normal end (`TurnCompletion`).
    fn turn_completed(&self) -> bool {
        self.turn_completion.as_ref().is_some_and(|c| c.completed())
    }

    /// Tell the engine why the running turn is being cancelled, BEFORE its
    /// token is: its canceled tool results say so ("Canceled by user" only
    /// for the user). First cause wins, so a teardown that revokes a driver
    /// and then cancels notes its own cause first.
    fn note_cancel_cause(&self, reason: crate::engine::interrupt::InterruptReason) {
        if !self.streaming {
            return;
        }
        if let (Some(turn), Some(cause)) = (&self.turn_completion, reason.cancel_cause()) {
            turn.note_cancel_cause(cause);
        }
    }

    /// The ONE tail of every interrupted turn, once its stream has been
    /// drained or dropped (`cancel_turn`, a revoked driver turn): record the
    /// interruption in history, end the turn, tell clients. Never runs the
    /// post-turn machinery of a normal end (queued auto-send, event
    /// auto-turns, auto-compaction).
    ///
    /// `completed`: the engine had finished the turn normally before the
    /// cancel reached it — the history is a complete answer, so no marker is
    /// appended and clients get the `Done` they would have had.
    async fn finish_interrupted_turn(
        &mut self,
        reason: crate::engine::interrupt::InterruptReason,
        completed: bool,
    ) {
        // Defensive: only trailing invalid messages this turn appended.
        crate::engine::stream::repair_history_after_failure(
            &mut self.conv.api_messages,
            self.turn_baseline,
        );
        let kept_partial = self.conv.api_messages.len() > self.turn_baseline;
        if !completed {
            crate::engine::interrupt::append_marker(&mut self.conv.api_messages, reason);
        }
        // A user steer the engine never picked up was not delivered.
        if let Some(q) = self.conv.queued_message.take() {
            self.emit(SessionEventWire::Dequeued { text: q });
        }
        // Events that arrived during the turn go in after the marker: first
        // those steered into the stream but never drained by the engine
        // (otherwise lost with the channel), then those buffered.
        {
            let undelivered = std::mem::take(&mut self.turn_steered_events);
            let conv: &mut ConversationState = &mut self.conv;
            for formatted in undelivered.into_iter().chain(conv.pending_events.drain(..)) {
                conv.api_messages.push(std::sync::Arc::new(serde_json::json!({
                    "role": "user",
                    "content": formatted
                })));
            }
        }
        // Queues the save of this history, then the draft removal.
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
        if completed {
            self.emit(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done)));
        } else {
            // Typed event. `context_saved` (wire name kept for compatibility)
            // now means "the turn's partial work is kept in history".
            self.emit(SessionEventWire::Aborted {
                context_saved: kept_partial,
            });
        }
        self.emit_conversation();
        self.announce_idle().await;
    }

    /// A turn whose token was cancelled from OUTSIDE `cancel_turn` — a
    /// driver revocation drops `DriverState`, cancelling the driver turn's
    /// child token (the grant's deadline task cancels that token too; the
    /// next `driver_tick` then revokes and arms the deadline) — ends here: on its `Done`, its stream's end, or when
    /// `revoked_turn_deadline` passes (a tool that ignores the cancel must
    /// not keep the session busy forever; the same budget as
    /// `drain_cancelled_stream`). What is left of the stream is dropped.
    async fn finish_revoked_turn(&mut self, stream_closed: bool) {
        if !self.streaming {
            return;
        }
        let completed = stream_closed && self.turn_completed();
        self.stream = None;
        self.finish_interrupted_turn(crate::engine::interrupt::InterruptReason::Driver, completed)
            .await;
    }

    /// Whether the running turn's token has been cancelled (by `cancel_turn`
    /// or a driver revocation).
    fn turn_cancelled(&self) -> bool {
        self.streaming && self.cancel.as_ref().is_some_and(|ct| ct.is_cancelled())
    }

    /// Answer every pending host prompt `None` (same as `checkpoint`/`finish`).
    fn resolve_pending_prompts(&mut self) {
        if self.pending_prompts.is_empty() {
            return;
        }
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }
        self.publish_presence();
        self.rearm_prompt_abandon();
    }

    /// Consume what a CANCELLED turn's stream still carries, for at most
    /// `budgets::CANCEL_DRAIN_TIMEOUT`, then drop it.
    ///
    /// Deliberately NOT `on_stream_event`: that handler would re-check the
    /// cost cap on the final `Usage` (recursive `cancel_turn`), run the
    /// failure path on the engine's typed `Canceled` error, and treat `Done`
    /// as a normal completion (queued auto-send, auto-compaction).
    ///
    /// Forwarded to clients: display events (partial text, tool results —
    /// the history being adopted contains them), `Usage`, agent events.
    /// Handled: `MessageHistory` (kept, last wins), `ContextHeadCheckpoint`
    /// (serviced — dropping its receipt would latch `durability_blocked`;
    /// the head it installs replaces any history kept before it),
    /// `SteeringDelivered`. Swallowed: `Done`, the `Canceled` error. A typed
    /// `InterruptedAfterSideEffect` becomes a notice (it is not a failure of
    /// the turn; the canceled `tool_result` already tells the model). A tool
    /// raising a host prompt before it observed the cancel is answered
    /// `None` at once, not at the deadline.
    async fn drain_cancelled_stream(&mut self) -> CancelDrain {
        let mut out = CancelDrain::default();
        let Some(mut stream) = self.stream.take() else {
            out.closed = true;
            return out;
        };
        let deadline = tokio::time::Instant::now() + budgets::CANCEL_DRAIN_TIMEOUT;
        loop {
            let event = tokio::select! {
                biased;
                Some(req) = self.secret_prompt_rx.recv() => {
                    let _ = req.response_tx.send(None);
                    continue;
                }
                next = tokio::time::timeout_at(deadline, stream.next()) => match next {
                    Err(_) => break,
                    Ok(None) => {
                        out.closed = true;
                        break;
                    }
                    Ok(Some(event)) => event,
                },
            };
            match event {
                StreamEvent::Session(SessionEvent::Done) => {
                    out.closed = true;
                    break;
                }
                StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                    out.history = Some(history);
                }
                StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                    session_id,
                    messages,
                    receipt,
                }) => {
                    self.handle_context_head_checkpoint(session_id, messages, receipt)
                        .await;
                    // The adopted head is the actor's history now: a history
                    // kept from BEFORE it is stale and must not override it
                    // (a later `MessageHistory`, if any, still wins).
                    out.history = None;
                }
                StreamEvent::Session(SessionEvent::Error(err)) => match err.outcome {
                    crate::TurnOutcome::Canceled => {}
                    crate::TurnOutcome::InterruptedAfterSideEffect { .. } => {
                        self.emit(SessionEventWire::SystemNotice(err.message));
                    }
                    _ => {
                        tracing::warn!(
                            session = %self.id,
                            category = err.category_label(),
                            "cancelled turn reported an error while draining"
                        );
                    }
                },
                StreamEvent::Agent(AgentEvent::SteeringDelivered { ref message }) => {
                    self.note_steering_delivered(message);
                    self.emit(SessionEventWire::Stream(event));
                }
                StreamEvent::Session(SessionEvent::Usage { .. }) => {
                    self.record_usage(&event);
                    self.emit(SessionEventWire::Stream(event));
                }
                other => self.emit(SessionEventWire::Stream(other)),
            }
        }
        drop(stream);
        out
    }

    /// `SteeringDelivered`: the engine injected `message` into history.
    fn note_steering_delivered(&mut self, message: &str) {
        if self.conv.queued_message.as_deref() == Some(message) {
            self.conv.queued_message = None;
            self.emit_conversation();
        }
        // P5/P6: pop the driver's steering FIFO on delivery ack.
        if let Some(driver) = self.driver.as_mut() {
            if driver.steering.front().map(String::as_str) == Some(message) {
                driver.steering.pop_front();
            }
        }
        if let Some(pos) = self.turn_steered_events.iter().position(|e| e == message) {
            self.turn_steered_events.remove(pos);
        }
    }

    /// Accumulate one `SessionEvent::Usage` into the conversation totals.
    fn record_usage(&mut self, event: &StreamEvent) {
        if let StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model: usage_model,
        }) = event
        {
            let model_for_pricing = usage_model
                .as_deref()
                .unwrap_or(self.runtime.model())
                .to_string();
            self.conv.add_usage(
                *input_tokens,
                *output_tokens,
                *cache_read_input_tokens,
                *cache_creation_input_tokens,
                *cache_creation_5m,
                *cache_creation_1h,
                &model_for_pricing,
            );
        }
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
            if de.disposition == EventDisposition::Steered {
                self.turn_steered_events.push(de.formatted.clone());
            }
            self.emit(SessionEventWire::External(de.event.clone()));
        }
        let injected = drained
            .iter()
            .any(|d| d.disposition == EventDisposition::Injected);
        if injected || busy {
            self.emit_conversation();
        }

        // P5: observe_events — if non-steering events arrive during a
        // driver-OWNED turn, revoke (competing work). Coordinates with F8.
        if self.streaming
            && self
                .driver
                .as_ref()
                .is_some_and(|d| d.awaiting_terminal)
        {
            let competing = drained
                .iter()
                .any(|d| !matches!(d.disposition, EventDisposition::Steered | EventDisposition::DisplayOnly));
            if competing {
                self.driver_revoke("event-bus work took priority");
            }
        }

        // `events.auto_turn = false` opts the session out of event-driven
        // turns (events are still injected/forwarded; the spend governor
        // for an ambient session). Read live from host config, like the
        // RPC/server hosts do — it used to be hardcoded `true` here.
        let auto_turn_enabled = self.host.config().events.auto_turn;
        let auto_turn_cap = self.host.config().events.auto_turn_cap;
        let action = wake_action_with_cap(
            &drained,
            &self.conv.api_messages,
            busy,
            auto_turn_enabled,
            self.consecutive_auto_turns,
            auto_turn_cap,
        );
        match action {
            WakeAction::RunTurn => {
                // F8: while a driver is armed, its tick owns turn scheduling.
                // Starting a competing turn here would race the driver.
                if self.driver.is_some() {
                    tracing::debug!("on_queue_wake: RunTurn inhibited — driver armed");
                } else if self.stream.is_some() {
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
                    && auto_turn_cap_reached(self.consecutive_auto_turns, auto_turn_cap);
                if hit_cap {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: auto_turn_cap });
                }
            }
            WakeAction::Nothing => {}
        }
    }

    // ── stream events (stream_handler.rs handle_stream_event + arm tail) ──

    async fn on_stream_event(&mut self, event: StreamEvent) {
        // A turn cancelled from outside `cancel_turn` (a driver revocation):
        // its terminal events are not a normal end — the turn is finished as
        // interrupted, never through the post-turn machinery below.
        if self.turn_cancelled() {
            match event {
                StreamEvent::Session(SessionEvent::Done) => {
                    self.finish_revoked_turn(true).await;
                    return;
                }
                // Swallowed as in `drain_cancelled_stream`; `Done` follows.
                StreamEvent::Session(SessionEvent::Error(err)) => {
                    match err.outcome {
                        crate::TurnOutcome::Canceled => {}
                        crate::TurnOutcome::InterruptedAfterSideEffect { .. } => {
                            self.emit(SessionEventWire::SystemNotice(err.message));
                        }
                        _ => tracing::warn!(
                            session = %self.id,
                            category = err.category_label(),
                            "revoked turn reported an error"
                        ),
                    }
                    return;
                }
                _ => {}
            }
        }
        // Forward first: clients see the same order they see today. A
        // context-head checkpoint is actor-internal (its receipt is ours to
        // complete) and must not reach clients: the wire has no such event
        // and maps it to `Done`, which ended the turn on socket clients.
        if !matches!(
            event,
            StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { .. })
        ) {
            self.emit(SessionEventWire::Stream(event.clone()));
        }

        // P5: observe_feedback — feed opted-in driver turns.
        if let Some(driver) = self.driver.as_mut() {
            if driver.awaiting_terminal && driver.grant.feedback_enabled() {
                driver.feedback.observe(&event);
            }
        }

        // P5: capture_terminal BEFORE Done/Error handlers modify state.
        let is_canceled = self
            .cancel
            .as_ref()
            .is_some_and(|ct| ct.is_cancelled());
        let terminal = if self
            .driver
            .as_ref()
            .is_some_and(|d| d.awaiting_terminal)
        {
            super::driver::capture_terminal(Some(&event), is_canceled)
        } else {
            None
        };

        enum After {
            Continue,
            AutoSendQueued(String),
            AutoTriggerEvents,
            Failed,
        }
        let mut after = After::Continue;

        match event {
            StreamEvent::Llm(crate::LlmEvent::Text(text)) => self.turn_draft_text(&text),
            // A new provider response (or a retry's reset): the in-flight
            // text starts over.
            StreamEvent::Llm(crate::LlmEvent::ResponseStart | crate::LlmEvent::ResponseReset) => {
                self.turn_draft_reset_text()
            }
            StreamEvent::Llm(_) => {}
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                // Published at every round boundary as well as at the end of
                // the turn (`runtime/stream.rs` ROUND CHECKPOINT): the session
                // on disk follows the turn as it progresses. Queued, never
                // awaited — the persister writes it (latest wins), so a slow
                // disk can never stall the turn machine.
                self.conv.api_messages = history;
                self.request_save();
                // The round is committed: the draft now continues from here.
                self.turn_draft_committed();
                self.trim_replay_to_checkpoint();
                self.emit_conversation();
            }
            StreamEvent::Agent(AgentEvent::SteeringDelivered { ref message }) => {
                self.note_steering_delivered(message);
            }
            StreamEvent::Agent(_) => {}
            StreamEvent::Session(SessionEvent::Usage { .. }) => {
                self.record_usage(&event);
                // (E-P7, §S3) Host-owned spend circuit breaker. Checked after
                // every Usage event — a breach cancels the in-flight turn,
                // revokes any armed driver (so it cannot re-poll and keep
                // spending), and tells clients why.
                if let Some((scope, cost, cap)) = self.cost_cap_breach() {
                    self.emit(SessionEventWire::CostCapReached {
                        scope: scope.to_string(),
                        cost,
                        cap,
                    });
                    self.note_cancel_cause(crate::engine::interrupt::InterruptReason::CostCap);
                    if self.driver.is_some() || self.driver_pending.is_some() {
                        self.driver_revoke(&format!("{scope} cost cap reached (${cost:.4} ≥ ${cap:.4})"));
                    }
                    self.cancel_turn(crate::engine::interrupt::InterruptReason::CostCap)
                        .await;
                    return;
                }
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
                    self.request_save();
                    after = After::AutoTriggerEvents;
                }
                self.emit_conversation();
                if matches!(after, After::Continue) {
                    if self.config.auto_compact {
                        self.post_turn_chat().await;
                    }
                    // A spawned compaction emits Idle when it lands.
                    if self.compact.is_none() {
                        self.announce_idle().await;
                    }
                }
            }
            StreamEvent::Session(SessionEvent::Error(_)) => {
                // Remove only invalid messages appended by the ACTIVE turn —
                // before `clear_stream` queues the turn's final save.
                crate::engine::stream::repair_history_after_failure(
                    &mut self.conv.api_messages,
                    self.turn_baseline,
                );
                self.clear_stream();
                self.publish_subagent_rows();
                self.emit_conversation();
                after = After::Failed;
            }
            StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                session_id,
                messages,
                receipt,
            }) => {
                self.handle_context_head_checkpoint(session_id, messages, receipt)
                    .await;
            }
        }

        match after {
            After::Continue => {}
            After::Failed => {
                if self.config.auto_compact {
                    self.post_turn_chat().await;
                }
                if self.compact.is_none() {
                    self.announce_idle().await;
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
                self.conv.api_messages.push(std::sync::Arc::new(
                    serde_json::json!({"role": "user", "content": queued}),
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
                // Central claim gate: allows turns while under the configured
                // cap (events.auto_turn_cap; 0 = unlimited), denies once reached.
                let auto_turn_cap = self.host.config().events.auto_turn_cap;
                if claim_auto_turn_with_cap(&mut self.consecutive_auto_turns, auto_turn_cap) {
                    self.start_turn(TurnTrigger::EventAuto, None).await;
                } else {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: auto_turn_cap });
                    self.announce_idle().await;
                }
            }
        }

        // P5: observe_terminal — set driver outcome or revoke.
        if let Some(terminal) = terminal {
            let revoke_reason = if let Some(driver) = self.driver.as_mut() {
                super::driver::observe_terminal(driver, &self.runtime, terminal)
            } else {
                None
            };
            // Emit DriverTurnOutcome if an outcome was just set.
            if let Some(driver) = self.driver.as_ref() {
                if let Some((outcome, _)) = &driver.outcome {
                    self.emit(SessionEventWire::DriverTurnOutcome {
                        outcome: *outcome,
                        selection: driver.selection.clone(),
                        feedback: driver.grant.feedback_enabled().then(|| {
                            driver.completed_feedback.to_string()
                        }),
                    });
                }
            }
            if let Some(reason) = revoke_reason {
                self.driver_revoke(&reason);
            }
        }
    }

    /// chat.rs post-turn block: save + engine-budget auto-compaction.
    async fn post_turn_chat(&mut self) {
        self.request_save();
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
                // The transition rewrites the predecessor on disk (its
                // `compacted_into` link): a queued save of it must land
                // first, never after.
                if !self.persister.flush().await {
                    tracing::warn!(session = %self.id, "compaction: an earlier save failed");
                }
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
                        // F10: lock follows the new journal id.
                        self.reacquire_session_lock(&new_id);
                        // Wall 1: reset continuation state for the new session id
                        // so stale epoch checks in persist_head() don't reject
                        // future checkpoints.
                        self.conv.context_head = crate::engine::session::ContextHeadPersistence::default();
                        self.runtime
                            .reset_context_continuation(&new_id, &self.conv.api_messages);
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
        self.announce_idle().await;
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
                // Queued saves land before the transition rewrites the file.
                if !self.persister.flush().await {
                    tracing::warn!(session = %self.id, "compaction: an earlier save failed");
                }
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
        // A prompt can be raised while already detached (a turn running with no
        // client). Arm the abandonment deadline if this left us pinned.
        self.rearm_prompt_abandon();
    }

    fn answer(&mut self, prompt_id: u64, value: Option<String>) {
        let Some(pos) = self.pending_prompts.iter().position(|(p, _)| p.id == prompt_id) else {
            return; // unknown or already answered — dedup on id
        };
        let (_, tx) = self.pending_prompts.remove(pos).expect("position");
        let _ = tx.send(value);
        self.publish_presence();
        self.emit(SessionEventWire::PromptResolved { prompt_id });
        // The last pending prompt may have just cleared — drop the deadline.
        self.rearm_prompt_abandon();
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
            SessionQuery::DisplayTail { items } => {
                serde_json::to_value(super::display::display_tail(&self.conv.api_messages, items))
                    .unwrap_or_default()
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
            display_tail: None,
        }
    }

    /// `snapshot()` shaped for one client (phase 4 §2.3): `Digest` clients
    /// get an empty `api_messages` (`messages_len` kept), a daemon-projected
    /// `display_tail`, and a replay without the per-round `MessageHistory`
    /// envelopes (the trailing `Conversation` digest carries len/hash).
    pub(crate) fn snapshot_for(&self, client: &ClientMeta) -> AttachSnapshot {
        let mut snap = self.snapshot();
        if client.history == HistoryMode::Digest {
            snap.display_tail = Some(super::display::display_tail(
                &snap.conversation.api_messages,
                client.tail_items,
            ));
            snap.conversation.api_messages = Vec::new();
            snap.replay.retain(|e| {
                !matches!(
                    e.event,
                    SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
                )
            });
        }
        snap
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
        let snapshot_meta = client.clone();
        self.attached.insert(cid, AttachedClient { meta: client, mode });
        self.update_attach_state();
        let mut owner_change: Option<(Option<ClientId>, OwnerChangeReason)> = None;
        match mode {
            AttachMode::Observe => {}
            // Input owned elsewhere: the joiner learns it from its snapshot
            // (`AttachSnapshot::input_owned_elsewhere`), never from a notice
            // broadcast to every client.
            AttachMode::Mirror => {
                if self.input_owner.is_none() {
                    owner_change = Some((None, OwnerChangeReason::Attach));
                }
            }
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
        let snapshot = self.snapshot_for(&snapshot_meta);
        self.emit(SessionEventWire::Attached {
            client: cid,
            snapshot,
        });
        self.emit(SessionEventWire::ClientJoined { client: cid, kind });
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
        // (E-P7 §S1 / P11) The last client just left. Fail-closed gates apply
        // ONLY when a driver is (or was about to be) armed:
        //
        //  1. An armed driver is revoked: it must never run turns headless with
        //     nobody watching and no way to answer a confirmation.
        //  2. Its pending host confirmations are answered `None` (deny) — a
        //     headless autonomous run must not sit on an unanswerable prompt.
        //     `tools/discovery.rs` treats `None` as Unauthorized (deny).
        //
        // For a PLAIN interactive session (no driver), a pending prompt SURVIVES
        // detach: the user reattaches and answers it — the daemon's core
        // detach/reattach contract, and detach is often involuntary (SSH drop,
        // sleep, wifi). Denying on detach would let a transient disconnect
        // silently reject the user's action. The resource pin from an
        // *abandoned* prompt is bounded by the pending-prompt deadline instead
        // (see the actor select loop), not by punishing every detach.
        if self.attached.is_empty() && (self.driver.is_some() || self.driver_pending.is_some()) {
            self.driver_revoke("no clients attached");
            let had_prompts = !self.pending_prompts.is_empty();
            while let Some((pr, tx)) = self.pending_prompts.pop_front() {
                let _ = tx.send(None);
                self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
            }
            if had_prompts {
                self.publish_presence();
            }
        }
    }

    /// B1 (used by C3 reload): checkpoint the session without ending it —
    /// cancel any turn (partial history kept + interruption marker), abort compaction, answer
    /// pending prompts `None`, save, close PTYs. Replies on
    /// `CHECKPOINT_QUERY_ID` so `reload.rs` can await it per session.
    pub(crate) async fn checkpoint(&mut self, reason: CheckpointReason) {
        let interrupt = match reason {
            CheckpointReason::Reload => crate::engine::interrupt::InterruptReason::Restart,
            CheckpointReason::HostRequest => crate::engine::interrupt::InterruptReason::Host,
        };
        // Before the revoke below cancels a driver turn.
        self.note_cancel_cause(interrupt);
        // E-P3: driver does NOT survive reload (§3 S2/S5).
        if self.driver.is_some() {
            self.driver_revoke("daemon reloaded");
        }
        if self.streaming {
            self.cancel_turn(interrupt).await;
        }
        self.abort_compaction();
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }
        // One bounded wait covers the cancelled turn's save too (queued, not
        // awaited, by `cancel_turn`).
        if !self.save().await {
            tracing::warn!(session = %self.id, "checkpoint: save failed or timed out");
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
        let record = serde_json::to_value(self.reload_record()).unwrap_or_default();
        self.emit(SessionEventWire::QueryResult {
            id: super::wire::CHECKPOINT_QUERY_ID,
            value: serde_json::json!({ "ok": true, "record": record }),
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
                    | SessionCommand::Park
                    | SessionCommand::Resync { .. }
                    | SessionCommand::HostEvent(_)
            );
            if matches!(cmd, SessionCommand::HostEvent(_)) {
                return ControlFlow::Continue(()); // nobody attached, nothing to render
            }
            if matches!(
                cmd,
                SessionCommand::Save
                    | SessionCommand::Cancel
                    | SessionCommand::Park
                    | SessionCommand::Checkpoint { .. }
            ) {
                if let SessionCommand::Checkpoint { .. } = cmd {
                    let record = serde_json::to_value(self.reload_record()).unwrap_or_default();
                    self.emit(SessionEventWire::QueryResult {
                        id: super::wire::CHECKPOINT_QUERY_ID,
                        value: serde_json::json!({ "ok": true, "parked": true, "record": record }),
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
            SessionCommand::Submit { text, attachments } => self.submit(text, attachments, from).await,
            SessionCommand::Steer { text } => {
                if self.streaming {
                    self.steer(text)
                } else {
                    self.submit(text, vec![], from).await
                }
            }
            SessionCommand::Cancel => {
                // The user's cancel, even of a driver's turn: noted before
                // the revoke below cancels that turn.
                self.note_cancel_cause(crate::engine::interrupt::InterruptReason::User);
                if self.driver.is_some() {
                    self.driver_revoke("canceled");
                }
                self.cancel_turn(crate::engine::interrupt::InterruptReason::User)
                    .await;
            }
            SessionCommand::Answer { prompt_id, value } => self.answer(prompt_id, value),
            SessionCommand::Set { id, setting } => self.apply_setting(id, setting).await,
            // `CompactionStarted` is the contract; no notice before it.
            SessionCommand::Compact { instructions } => {
                self.compact(instructions, "manual").await;
            }
            SessionCommand::NewSession => {
                if self.driver.is_some() {
                    self.driver_revoke("session replaced");
                }
                // `clear` saves the old session directly: queued saves of it
                // must land first, never after.
                self.persister.flush().await;
                self.conv.clear(&self.runtime).await;
                self.runtime
                    .set_session_id(Some(self.conv.session.id.clone()));
                self.journal_id.store(Arc::new(self.conv.session.id.clone()));
                // F10: lock follows the new session id.
                self.reacquire_session_lock(&self.conv.session.id.clone());
                self.emit(SessionEventWire::Cleared {
                    session_id: self.conv.session.id.clone(),
                });
                self.emit_conversation();
            }
            SessionCommand::Save => {
                self.save().await;
            }
            SessionCommand::Query { id, query } => self.query(id, query).await,
            SessionCommand::EngineCommand { id, name, arg } => {
                self.engine_command(id, name, arg).await
            }
            SessionCommand::Attach { client, mode } => self.attach(client, mode).await,
            SessionCommand::Detach { client } => self.detach(client),
            SessionCommand::End { reason } => {
                // `finish` cancels a running turn for this reason; note it
                // before the revoke below cancels a driver turn first.
                self.note_cancel_cause(match reason {
                    EndReason::ClientQuit => crate::engine::interrupt::InterruptReason::User,
                    _ => crate::engine::interrupt::InterruptReason::Host,
                });
                if self.driver.is_some() {
                    self.driver_revoke("session ending");
                }
                return ControlFlow::Break(reason);
            }
            SessionCommand::Resync { .. } => self.emit(SessionEventWire::SystemNotice(
                "resync not supported yet".into(),
            )),
            // A3 bodies live in actor_cmds.rs; B1 (Checkpoint), B3 (KeepWarm)
            // fill in the rest.
            SessionCommand::SubmitPrepared {
                messages,
                user_text,
            } => self.submit_prepared(messages, user_text).await,
            SessionCommand::PluginCommand {
                id,
                plugin,
                name,
                arg,
            } => self.plugin_command(id, plugin, name, arg).await,
            SessionCommand::Resume { id, query } => self.resume(id, query).await,
            SessionCommand::Checkpoint { reason } => self.checkpoint(reason).await,
            SessionCommand::Park => {
                if let std::ops::ControlFlow::Break(reason) = self.park().await {
                    return ControlFlow::Break(reason);
                }
            }
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
            // E-P3: driver start handler.
            SessionCommand::DriverStart { plugin, command, arg } => {
                self.driver_start(plugin, command, arg);
            }
        }
        ControlFlow::Continue(())
    }

    // ── teardown (tui/mod.rs:352-432 + chat.rs shutdown) ─────────────────

    async fn finish(&mut self, reason: EndReason) {
        self.lifecycle.store(
            SessionLifecycle::Ending as u8,
            std::sync::atomic::Ordering::Release,
        );
        // Quitting the client mid-turn is the user's interruption; every
        // other end reason is the host's.
        let interrupt = match reason {
            EndReason::ClientQuit => crate::engine::interrupt::InterruptReason::User,
            _ => crate::engine::interrupt::InterruptReason::Host,
        };
        // Before the revoke below cancels a driver turn.
        self.note_cancel_cause(interrupt);
        // E-P3: clean up driver before teardown.
        if self.driver.is_some() {
            self.driver_revoke("session ending");
        }
        if self.streaming {
            self.cancel_turn(interrupt).await;
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
        // session is already on disk: end record only. The wait covers
        // everything queued, including the cancelled turn's save.
        let persist = self.config.persist;
        if !parked {
            self.request_save();
        }
        let persister = &self.persister;
        let save_fut = async {
            if persist {
                if !persister.flush().await {
                    tracing::warn!(session = %session_id, "session end: save failed");
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

/// Whether the 1 Hz `SubagentRows` tick keeps running: always during a turn,
/// and after it while any worker is still running (a background worker's
/// progress events rode the ended turn's stream; the rows carry it now).
fn keep_subagent_tick(streaming: bool, any_running: bool) -> bool {
    streaming || any_running
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

/// `revoked_turn_deadline`: same shape as `park_timer`.
async fn revoked_turn_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// (P11) Prompt-abandonment deadline: same shape as `park_timer`. `None` =
/// disabled (no abandoned prompt, or the feature is off) → pends forever.
async fn prompt_abandon_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// 200 ms cadence, gated on driver state — `None` driver+pending → pending forever.
async fn driver_tick_timer(
    has_driver: bool,
    has_pending: bool,
    interval: &mut tokio::time::Interval,
) {
    if has_driver || has_pending {
        interval.tick().await;
    } else {
        std::future::pending().await
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
                _ = next_tick(&mut actor.subagent_tick) => {
                    actor.publish_subagent_rows();
                    actor.flush_turn_draft();
                }
                _ = park_timer(actor.park_deadline) => {
                    if let std::ops::ControlFlow::Break(reason) = actor.park().await {
                        break reason;
                    }
                }
                _ = prompt_abandon_timer(actor.prompt_abandon_deadline) => {
                    actor.on_prompt_abandon_deadline();
                }
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
                _ = driver_tick_timer(actor.driver.is_some(), actor.driver_pending.is_some(), &mut actor.driver_tick_interval) => {
                    actor.driver_tick().await;
                }
                _ = revoked_turn_timer(actor.revoked_turn_deadline) => {
                    actor.finish_revoked_turn(false).await;
                }
                ev = next_stream_event(&mut actor.stream) => match ev {
                    Some(ev) => actor.on_stream_event(ev).await,
                    // A revoked turn's stream ending is its end.
                    None if actor.turn_cancelled() => actor.finish_revoked_turn(true).await,
                    None => {
                        // Stream ended without a terminal event: defensive reset.
                        // P5: capture_terminal(None) = EOF → revoke driver.
                        if actor.driver.as_ref().is_some_and(|d| d.awaiting_terminal) {
                            let terminal = super::driver::capture_terminal(None, false);
                            if let Some(terminal) = terminal {
                                if let Some(driver) = actor.driver.as_mut() {
                                    let reason = super::driver::observe_terminal(driver, &actor.runtime, terminal);
                                    // F-NEW-1 (shady): EOF is a Blocked terminal.
                                    // Emit a DriverTurnOutcome before revoking so a
                                    // client tracking outcomes sees the same
                                    // outcome+revoke pair the Error path produces —
                                    // not a turn that silently vanishes.
                                    let sel = driver.selection.clone();
                                    let fb = driver.grant.feedback_enabled()
                                        .then(|| driver.completed_feedback.to_string());
                                    actor.emit(SessionEventWire::DriverTurnOutcome {
                                        outcome: crate::extensions::session_driver::Outcome::Blocked,
                                        selection: sel,
                                        feedback: fb,
                                    });
                                    if let Some(reason) = reason {
                                        actor.driver_revoke(&reason);
                                    }
                                }
                            }
                        }
                        actor.clear_stream();
                        actor.emit_conversation();
                        actor.announce_idle().await;
                    }
                },
            }
        };
        self.0.finish(reason).await;
    }
}

#[cfg(test)]
mod subagent_tick_tests {
    use super::keep_subagent_tick;

    #[test]
    fn rows_keep_flowing_after_the_turn_while_a_worker_runs() {
        assert!(keep_subagent_tick(true, false), "during a turn, always");
        assert!(keep_subagent_tick(true, true));
        assert!(
            keep_subagent_tick(false, true),
            "after the turn, while a background worker runs"
        );
        assert!(
            !keep_subagent_tick(false, false),
            "stops once the turn is over and no worker runs"
        );
    }
}

#[cfg(test)]
mod parked_evict_tests {
    use super::parked_evict_after_from;
    use super::prompt_abandon_timeout_from;
    use std::time::Duration;

    #[test]
    fn parked_evict_defaults_to_one_hour_and_honours_never() {
        // cfg_secs = 3600 mirrors the DaemonConfig default.
        let d = 3600u64;
        assert_eq!(parked_evict_after_from(None, d), Some(Duration::from_secs(3600)));
        assert_eq!(parked_evict_after_from(Some("120"), d), Some(Duration::from_secs(120)));
        for never in ["never", "0", "off"] {
            assert_eq!(parked_evict_after_from(Some(never), d), None, "{never:?}");
        }
        assert_eq!(parked_evict_after_from(Some("junk"), d), Some(Duration::from_secs(3600)));
        // env absent → config wins; config 0 disables; env still beats config.
        assert_eq!(parked_evict_after_from(None, 42), Some(Duration::from_secs(42)));
        assert_eq!(parked_evict_after_from(None, 0), None);
        assert_eq!(parked_evict_after_from(Some("7"), 42), Some(Duration::from_secs(7)));
    }

    #[test]
    fn prompt_abandon_defaults_to_one_hour_and_honours_never() {
        // cfg_secs = 3600 mirrors the DaemonConfig default.
        let d = 3600u64;
        assert_eq!(prompt_abandon_timeout_from(None, d), Some(Duration::from_secs(3600)));
        assert_eq!(prompt_abandon_timeout_from(Some("30"), d), Some(Duration::from_secs(30)));
        // Disabled sentinels ⇒ None ⇒ pure pre-#112 (prompt survives forever).
        for never in ["never", "0", "off"] {
            assert_eq!(prompt_abandon_timeout_from(Some(never), d), None, "{never:?}");
        }
        // Garbage falls back to the safe default rather than disabling the guard.
        assert_eq!(prompt_abandon_timeout_from(Some("junk"), d), Some(Duration::from_secs(3600)));
        // env absent → config wins; config 0 disables; env still beats config.
        assert_eq!(prompt_abandon_timeout_from(None, 42), Some(Duration::from_secs(42)));
        assert_eq!(prompt_abandon_timeout_from(None, 0), None);
        assert_eq!(prompt_abandon_timeout_from(Some("7"), 42), Some(Duration::from_secs(7)));
    }
}
