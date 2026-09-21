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

/// `SYNAPS_DAEMON_PARKED_EVICT_SECS`: how long a PARKED session stays in the
/// daemon's map before it is evicted (`EndReason::Evicted`). Its state is on
/// disk; `--continue <id>` rebuilds it exactly as after a daemon restart. The
/// row only exists so `--attach <id>` / the adopt banner can find it quickly.
/// Default 1 h; `never` → keep forever.
pub fn parked_evict_after() -> Option<std::time::Duration> {
    parked_evict_after_from(std::env::var("SYNAPS_DAEMON_PARKED_EVICT_SECS").ok().as_deref())
}

fn parked_evict_after_from(v: Option<&str>) -> Option<std::time::Duration> {
    const DEFAULT: std::time::Duration = std::time::Duration::from_secs(3600);
    match v.map(str::trim) {
        Some("never" | "0" | "off") => None,
        Some(n) => n.parse::<u64>().ok().map(std::time::Duration::from_secs).or(Some(DEFAULT)),
        None => Some(DEFAULT),
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
    prompt_abandon_timeout_from(std::env::var("SYNAPS_DAEMON_PROMPT_ABANDON_SECS").ok().as_deref())
}

fn prompt_abandon_timeout_from(v: Option<&str>) -> Option<std::time::Duration> {
    const DEFAULT: std::time::Duration = std::time::Duration::from_secs(3600);
    match v.map(str::trim) {
        Some("never" | "0" | "off") => None,
        Some(n) => n.parse::<u64>().ok().map(std::time::Duration::from_secs).or(Some(DEFAULT)),
        None => Some(DEFAULT),
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
        if self.stream.is_none()
            && self
                .cancel
                .as_ref()
                .is_some_and(|ct| ct.is_cancelled())
        {
            self.clear_stream();
            self.emit(SessionEventWire::Idle);
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
                            let mut validated = proposal.clone();
                            if let Some(context) = &self.conv.abort_context {
                                validated.prompt =
                                    format!("{context}\n\n{}", proposal.prompt);
                            }
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
                                    &mut conv.abort_context,
                                );
                            }
                            // Apply the prepared runtime.
                            *self.runtime = candidate;
                            self.conv.session.model = self.runtime.model().to_owned();
                            self.conv.session.thinking_level =
                                self.runtime.thinking_level().to_owned();
                            self.consecutive_auto_turns = 0;
                            self.turn_baseline = self.conv.api_messages.len();
                            self.turn_log.clear();
                            self.turn_replay.clear();
                            self.streaming = true;
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
                                        started = runtime.run_stream_with_messages(history, ct.clone(), Some(rx), Some(secret), false) => super::driver::TaskResult::Started(Ok(started)),
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
                            self.save().await;
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
            let mut proposal =
                driver.proposal.take().expect("checked proposal").proposal;
            let original_prompt = proposal.prompt.clone();
            if let Some(context) = &self.conv.abort_context {
                proposal.prompt = format!("{context}\n\n{}", proposal.prompt);
            }
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
                    proposal.prompt = original_prompt;
                    super::driver::TaskResult::Prepared {
                        proposal,
                        result: Box::new(result),
                    }
                })),
            });
        }
    }

    /// F10: release the old lock, acquire on `new_id`. Best-effort (log on failure).
    fn reacquire_session_lock(&mut self, new_id: &str) {
        // Drop old lock first — release the flock.
        self.session_lock = None;
        let dir = agent_core::session_lock::sessions_dir();
        let holder = agent_core::session_lock::LockHolder {
            pid: std::process::id(),
            kind: "daemon".to_string(),
        };
        match agent_core::session_lock::SessionLock::try_acquire(&dir, new_id, holder) {
            Ok(lock) => self.session_lock = Some(lock),
            Err(e) => {
                tracing::warn!(session = %new_id, "reacquire session lock: {e}");
            }
        }
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
            return std::ops::ControlFlow::Break(EndReason::Idle);
        }
        if !self.can_park() {
            return std::ops::ControlFlow::Continue(());
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
        // (E-P7, §S9) An armed driver MUST NOT auto-approve tool activation —
        // the session-driver contract keeps ordinary tool-approval gates in
        // force. Override the session config to false whenever a grant is
        // armed, regardless of what the client requested. (Driver-initiated
        // turns already pass `false` from the tick's Prepared arm; this guards
        // any foreground turn that starts while armed.)
        let auto_approve = self.config.auto_approve_confirms && self.driver.is_none();
        let stream = self
            .runtime
            .run_stream_with_messages(
                self.conv.api_messages.clone(),
                ct.clone(),
                Some(s_rx),
                Some(self.secret_prompt_handle.clone()),
                auto_approve,
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
        // Wall 1 defense-in-depth: a latched (unverified) context head must
        // not accept new inference. The stream would refuse via
        // `durability_blocked` anyway, but that leaves the user message
        // orphaned in history; refuse here, before it is pushed.
        if self.conv.context_head.is_blocked(&self.conv.session) {
            self.emit(SessionEventWire::SystemNotice(
                "context head is unverified after a failed checkpoint; reload the session                  (`--continue`) or start a new one before continuing"
                    .into(),
            ));
            return;
        }
        if self.streaming {
            // A Submit while streaming is what the TUI calls StreamingInput.
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
        // Forward first: clients see the same order they see today.
        self.emit(SessionEventWire::Stream(event.clone()));

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
                // P5/P6: pop the driver's steering FIFO on delivery ack.
                if let Some(driver) = self.driver.as_mut() {
                    if driver.steering.front() == Some(&message) {
                        driver.steering.pop_front();
                    }
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
                    if self.driver.is_some() || self.driver_pending.is_some() {
                        self.driver_revoke(&format!("{scope} cost cap reached (${cost:.4} ≥ ${cap:.4})"));
                    }
                    self.cancel_turn().await;
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
                // Central claim gate: allows turns while under the configured
                // cap (events.auto_turn_cap; 0 = unlimited), denies once reached.
                let auto_turn_cap = self.host.config().events.auto_turn_cap;
                if claim_auto_turn_with_cap(&mut self.consecutive_auto_turns, auto_turn_cap) {
                    self.start_turn(TurnTrigger::EventAuto, None).await;
                } else {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: auto_turn_cap });
                    self.emit(SessionEventWire::Idle);
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
        let snapshot = self.snapshot_for(&snapshot_meta);
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
    /// cancel any turn (abort_context captured), abort compaction, answer
    /// pending prompts `None`, save, close PTYs. Replies on
    /// `CHECKPOINT_QUERY_ID` so `reload.rs` can await it per session.
    pub(crate) async fn checkpoint(&mut self, reason: CheckpointReason) {
        // E-P3: driver does NOT survive reload (§3 S2/S5).
        if self.driver.is_some() {
            self.driver_revoke("daemon reloaded");
        }
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
            SessionCommand::Submit { text, .. } => self.submit(text).await,
            SessionCommand::Steer { text } => {
                if self.streaming {
                    self.steer(text)
                } else {
                    self.submit(text).await
                }
            }
            SessionCommand::Cancel => {
                if self.driver.is_some() {
                    self.driver_revoke("canceled");
                }
                self.cancel_turn().await;
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
            SessionCommand::Save => self.save().await,
            SessionCommand::Query { id, query } => self.query(id, query).await,
            SessionCommand::EngineCommand { id, name, arg } => {
                self.engine_command(id, name, arg).await
            }
            SessionCommand::Attach { client, mode } => self.attach(client, mode).await,
            SessionCommand::Detach { client } => self.detach(client),
            SessionCommand::End { reason } => {
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
        // E-P3: clean up driver before teardown.
        if self.driver.is_some() {
            self.driver_revoke("session ending");
        }
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
                _ = next_tick(&mut actor.subagent_tick) => actor.publish_subagent_rows(),
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
                ev = next_stream_event(&mut actor.stream) => match ev {
                    Some(ev) => actor.on_stream_event(ev).await,
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
                        actor.emit(SessionEventWire::Idle);
                    }
                },
            }
        };
        self.0.finish(reason).await;
    }
}

#[cfg(test)]
mod parked_evict_tests {
    use super::parked_evict_after_from;
    use super::prompt_abandon_timeout_from;
    use std::time::Duration;

    #[test]
    fn parked_evict_defaults_to_one_hour_and_honours_never() {
        assert_eq!(parked_evict_after_from(None), Some(Duration::from_secs(3600)));
        assert_eq!(parked_evict_after_from(Some("120")), Some(Duration::from_secs(120)));
        for never in ["never", "0", "off"] {
            assert_eq!(parked_evict_after_from(Some(never)), None, "{never:?}");
        }
        assert_eq!(parked_evict_after_from(Some("junk")), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn prompt_abandon_defaults_to_one_hour_and_honours_never() {
        // Default (unset) is a generous 1 h.
        assert_eq!(prompt_abandon_timeout_from(None), Some(Duration::from_secs(3600)));
        assert_eq!(prompt_abandon_timeout_from(Some("30")), Some(Duration::from_secs(30)));
        // Disabled sentinels ⇒ None ⇒ pure pre-#112 (prompt survives forever).
        for never in ["never", "0", "off"] {
            assert_eq!(prompt_abandon_timeout_from(Some(never)), None, "{never:?}");
        }
        // Garbage falls back to the safe default rather than disabling the guard.
        assert_eq!(prompt_abandon_timeout_from(Some("junk")), Some(Duration::from_secs(3600)));
    }
}
