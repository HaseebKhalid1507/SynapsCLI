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
    /// Session id the open draft is persisted under; `None` = not persisted
    /// (no turn open, or this actor doesn't write drafts). `base_len` and
    /// `text` are tracked for every turn: a cancel whose drain times out
    /// keeps the streamed text (`SessionActor::keep_streamed_partial`).
    open: Option<String>,
    /// History length the in-flight response continues from.
    base_len: usize,
    /// Text streamed in the in-flight response so far.
    text: String,
    /// Tools the in-flight response has called (complete `ToolUse`s), which
    /// may be running. Not persisted.
    tools: Vec<String>,
    /// Changed since the last flush.
    dirty: bool,
}

/// What a started turn needs to open its stream (`start_turn`), held while
/// its request waits for `on_session_start`.
pub(crate) struct TurnStart {
    cancel: CancellationToken,
    steer_rx: mpsc::UnboundedReceiver<String>,
    auto_approve: bool,
    completion: crate::runtime::TurnCompletion,
    /// Events started this turn (nobody typed it): its stream runs under the
    /// event turn budget (#251). Decided from the turn's trigger at
    /// `start_turn`, applied when the stream opens (`open_turn_stream`),
    /// which may be after `on_session_start` has run.
    event_turn: bool,
}

/// Why autonomous work may not start now
/// ([`SessionActor::autonomous_work_blocked`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkBlock {
    /// A reload checkpointed this session: nothing may start work the reload
    /// record does not carry until the exec, or until the daemon reports
    /// that it failed (#255's reload fence).
    Fenced,
    /// This session's `on_session_start` has not run yet: a request sent
    /// now would lack its injection.
    SessionStartPending,
}

/// A source of autonomous work in the actor's loop: work nobody asked for
/// with a command ([`SessionActor::holds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkSource {
    /// Queued events (inbox, `synaps send`, worker completions).
    QueueWake,
    /// The park grace or a worker re-check.
    ParkTimer,
    /// A due prompt-cache keep-warm ping.
    KeepWarmTimer,
    /// The armed session driver's 200 ms tick.
    DriverTick,
    /// A due capacity resume (`session::actor_capacity`).
    CapacityResume,
    /// (S1) The hold lease ran out (`session::actor_held`). A lease that
    /// runs out during a reload fence is released by the next image.
    HeldLease,
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

/// How often a detached idle session re-checks a park its workers hold
/// (`can_park_ignoring_keepwarm`).
pub const WORKER_PARK_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(any(test, feature = "testing"))]
static WORKER_PARK_RECHECK_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// [`WORKER_PARK_RECHECK`], or the test override. Code-only, like
/// `cache_keepwarm::interval_override`: no binary input can reach it.
fn worker_park_recheck() -> std::time::Duration {
    #[cfg(any(test, feature = "testing"))]
    {
        let ms = WORKER_PARK_RECHECK_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return std::time::Duration::from_millis(ms);
        }
    }
    WORKER_PARK_RECHECK
}

/// Test-only controls (`cfg(test)` / `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Replaces the worker park re-check interval for every session in
    /// this process while alive (tests that use it must be `#[serial]`).
    #[must_use = "the override is cleared when the guard drops"]
    pub struct WorkerParkRecheck(());

    impl WorkerParkRecheck {
        pub fn set(interval: Duration) -> Self {
            let ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX).max(1);
            super::WORKER_PARK_RECHECK_OVERRIDE_MS.store(ms, Ordering::Relaxed);
            Self(())
        }
    }

    impl Drop for WorkerParkRecheck {
        fn drop(&mut self) {
            super::WORKER_PARK_RECHECK_OVERRIDE_MS.store(0, Ordering::Relaxed);
        }
    }
}

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

pub(super) fn replay_setting(rt: &mut Runtime, setting: &SessionSetting) {
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
    /// Resolved at attach ([`resolve_client_name`]); unique in the session.
    pub(crate) name: String,
    /// Unix ms.
    pub(crate) attached_at_ms: u64,
    /// `SessionActor::input_seq` at this client's last input; 0 = none yet.
    pub(crate) last_input: u64,
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
/// is open to every attached client — except that `answer` refuses an
/// observer's answer to a Confirm prompt and accepts "Allow all" only from
/// the owner (#148 C2). `Checkpoint` cancels the owner's turn
/// and kills its PTYs, so it is owner-only from a client (M1); the daemon
/// sends it `from: None`.
fn is_input_command(cmd: &SessionCommand) -> bool {
    matches!(
        cmd,
        SessionCommand::Checkpoint { .. }
            | SessionCommand::Submit { .. }
            | SessionCommand::SubmitPrepared { .. }
            | SessionCommand::Steer { .. }
            | SessionCommand::SubagentSteer { .. }
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

/// The MAIN-conversation note for a user steer of a worker (`SubagentSteer`):
/// a low-severity event, formatted for the model like any drained event.
fn subagent_steer_event(handle_id: &str, agent_name: &str, text: &str) -> crate::events::types::Event {
    use crate::events::types::{Event, EventContent, EventSource, Severity};
    Event {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now(),
        source: EventSource {
            source_type: "user".to_string(),
            name: "user".to_string(),
            callback: None,
        },
        channel: None,
        sender: None,
        content: EventContent {
            text: format!("User steered subagent '{agent_name}' ({handle_id}) directly: {text}"),
            content_type: "subagent_steer".to_string(),
            severity: Some(Severity::Low),
            data: Some(serde_json::json!({ "handle_id": handle_id, "agent_name": agent_name })),
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
        peer: None,
    }
}

/// The MAIN-conversation note for a worker the user cancelled with
/// `/workers cancel`: its completion wake is withheld, so without this the
/// main agent would wait for it (or dispatch the work again).
pub(crate) fn subagent_user_cancel_event(
    handle_id: &str,
    agent_name: &str,
) -> crate::events::types::Event {
    use crate::events::types::{Event, EventContent, EventSource, Severity};
    Event {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now(),
        source: EventSource {
            source_type: "user".to_string(),
            name: "user".to_string(),
            callback: None,
        },
        channel: None,
        sender: None,
        content: EventContent {
            text: format!(
                "User cancelled subagent '{agent_name}' ({handle_id}) with /workers cancel. It stops at \
                 its next step and sends no completion wake; do not wait for it. Its partial output is \
                 readable with subagent_collect (handle_id \"{handle_id}\") only while this session \
                 stays loaded: the handle does not survive the session being parked."
            ),
            content_type: "subagent_cancelled".to_string(),
            severity: Some(Severity::Low),
            data: Some(serde_json::json!({ "handle_id": handle_id, "agent_name": agent_name })),
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
        peer: None,
    }
}

/// The MAIN-conversation note for a worker `/close force` cancelled. The
/// session ends right after, so this is for whoever continues it later
/// (`--continue`): the worker never finished and no wake is coming.
pub(crate) fn subagent_close_cancel_event(
    handle_id: &str,
    agent_name: &str,
) -> crate::events::types::Event {
    let mut event = subagent_user_cancel_event(handle_id, agent_name);
    event.content.text = format!(
        "User closed this session with /close force while subagent '{agent_name}' ({handle_id}) was \
         running, so it was cancelled before it finished. It sends no completion wake; do not wait for \
         it. If its work is still needed, check what it left behind and dispatch it again."
    );
    event
}

/// `KeepWarm{on: true}`'s notice (`/pin on`, `daemon pin`, `--keep-warm`).
pub const PIN_ON_NOTICE: &str = "pinned: this session stays loaded (never parked or idle-ended, \
     and the daemon will not idle-exit while it is pinned) and comes back after daemon restarts. \
     /pin off to undo.";
/// `KeepWarm{on: false}`'s notice.
pub const PIN_OFF_NOTICE: &str = "unpinned: this session parks again when idle and won't come back after a restart";

/// The engine command behind `/close` (`SessionActor::close_command`).
pub(crate) const CLOSE_COMMAND: &str = "close";

fn is_close_command(cmd: &SessionCommand) -> bool {
    matches!(cmd, SessionCommand::EngineCommand { name, .. } if name == CLOSE_COMMAND)
}

/// The host-only engine command behind `synaps daemon rename`
/// (`SessionActor::host_rename`; JSON args, `RenameArgs`). Clients rename
/// with `saveas`.
pub(crate) const RENAME_COMMAND: &str = "rename";

fn is_rename_command(cmd: &SessionCommand) -> bool {
    matches!(cmd, SessionCommand::EngineCommand { name, .. } if name == RENAME_COMMAND)
}

fn command_name(cmd: &SessionCommand) -> &'static str {
    match cmd {
        SessionCommand::Submit { .. } => "submit",
        SessionCommand::SubmitPrepared { .. } => "submit_prepared",
        SessionCommand::Steer { .. } => "steer",
        SessionCommand::SubagentSteer { .. } => "subagent_steer",
        SessionCommand::Cancel => "cancel",
        SessionCommand::Answer { .. } => "answer",
        SessionCommand::Set { .. } => "set",
        SessionCommand::EphemeralLayer { .. } => "ephemeral_layer",
        SessionCommand::Compact { .. } => "compact",
        SessionCommand::NewSession => "new_session",
        SessionCommand::Save => "save",
        SessionCommand::Query { .. } => "query",
        SessionCommand::Attach { .. } => "attach",
        SessionCommand::Detach { .. } => "detach",
        SessionCommand::End { .. } => "end",
        SessionCommand::Resync { .. } => "resync",
        SessionCommand::EngineCommand { name, .. } if name == CLOSE_COMMAND => "close",
        SessionCommand::EngineCommand { name, .. } if name == RENAME_COMMAND => "rename",
        SessionCommand::EngineCommand { .. } => "engine_command",
        SessionCommand::PluginCommand { .. } => "plugin_command",
        SessionCommand::Resume { .. } => "resume",
        SessionCommand::Checkpoint { .. } => "checkpoint",
        SessionCommand::KeepWarm { .. } => "keep_warm",
        SessionCommand::TakeInput => "take_input",
        SessionCommand::Park => "park",
        SessionCommand::AdoptWorkers { .. } => "adopt_workers",
        SessionCommand::RestoreToolActivations { .. } => "restore_tool_activations",
        SessionCommand::ReloadAborted => "reload_aborted",
        SessionCommand::ArmPark => "arm_park",
        SessionCommand::ArmCapacityResume { .. } => "arm_capacity_resume",
        SessionCommand::ReloadRefused { .. } => "reload_refused",
        SessionCommand::ResumeAfterReload { .. } => "resume_after_reload",
        SessionCommand::ResumeAfterCrash { .. } => "resume_after_crash",
        SessionCommand::HostEvent(_) => "host_event",
        SessionCommand::DriverStart { .. } => "driver_start",
        SessionCommand::Delivery { .. } => "delivery",
        SessionCommand::HeldRelease { .. } => "held_release",
        SessionCommand::HeldDismiss { .. } => "held_dismiss",
        SessionCommand::HeldMark { .. } => "held_mark",
        SessionCommand::Fork { .. } => "fork",
        SessionCommand::ForkDone { .. } => "fork_done",
        SessionCommand::RegisterClientTools { .. } => "register_client_tools",
        SessionCommand::ToolReturn { .. } => "tool_return",
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
    pub(crate) ephemeral_layer: crate::runtime::ephemeral::Layer,
    /// Memory-only tool results (docs/memory-only-results.md): RAM only,
    /// handed explicitly to each runtime this actor runs turns on, cleared
    /// at park, rollover, compaction and a new or resumed conversation.
    pub(crate) memory_only: crate::runtime::memory_only::Store,
    /// The session's own memory-only globs: ONE add-only list, shared with
    /// every runtime, stream and worker of this session and read at each
    /// mark check, so a live `Set` reaches a turn in flight.
    memory_only_globs: crate::runtime::memory_only::SessionGlobs,
    /// Client tools v1 (docs/client-tools.md): RAM only, handed explicitly
    /// to each runtime this actor runs root turns on, never to a clone,
    /// worker, fork, reload or crash record.
    pub(crate) client_tools: crate::runtime::client_tools::Overlay,
    /// The frames the overlay's calls send (`tool_invoke`, `tool_cancel`),
    /// drained by the actor loop and addressed to the installer only.
    pub(crate) client_tool_frames: mpsc::UnboundedReceiver<crate::runtime::client_tools::Outbound>,
    /// The client that registered the client tools in force (the input
    /// owner then); `None` without a registration.
    pub(crate) client_tool_installer: Option<ClientId>,
    ephemeral_changes: tokio::sync::watch::Receiver<crate::runtime::ephemeral::Status>,
    ephemeral_installer: Option<ClientId>,
    pub(crate) keep_warm: bool,
    /// Opt-in prompt-cache keep-warm pings while idle (task #444). Unrelated
    /// to `keep_warm` above, which only stops the session from parking.
    pub(crate) cache_keepwarm: super::cache_keepwarm::KeepWarmState,
    /// The session's spend sink (child spend). Every runtime this actor
    /// builds launches its workers under it; `spend_rx` receives what they
    /// book, with or without a turn (`session::child_spend`). Kept across
    /// Park and runtime rebuilds.
    pub(crate) spend_sink: crate::runtime::child_spend::SpendSink,
    pub(crate) spend_rx: crate::runtime::child_spend::SpendReceiver,
    pub(crate) spend_seqs: crate::runtime::child_spend::SpendSeqs,
    /// Armed on last-detach-while-idle / idle-while-detached.
    pub(crate) park_deadline: Option<tokio::time::Instant>,
    /// `park_deadline` when it was armed as a worker re-check (workers held
    /// the park), not as the reconnect grace: a re-check that finds the
    /// session parkable arms the grace instead of parking.
    pub(crate) park_recheck_at: Option<tokio::time::Instant>,
    /// (P11) Armed when the last client detaches while a pending host prompt is
    /// held on a NON-driver session; a re-attach cancels it, expiry denies the
    /// prompt(s) so `can_park()` can proceed. `None` = no abandoned prompt (or
    /// the deadline is disabled by config).
    pub(crate) prompt_abandon_deadline: Option<tokio::time::Instant>,
    // ── turn machine: the run() loop locals + App fields ──
    pub(crate) stream: Option<ActiveStream>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) steer_tx: Option<mpsc::UnboundedSender<String>>,
    /// An event-started turn's way onto the session budget: `steer` sends
    /// the owner's message through it, and the turn's stream switches to the
    /// session budget when it delivers that message. `None` for every other
    /// turn (they already run on the session budget).
    pub(crate) owner_steer_budget: Option<Arc<crate::runtime::budget::OwnerSteerBudget>>,
    pub(crate) streaming: bool,
    pub(crate) turn_baseline: usize,
    /// Stamps each message appended to `conv.api_messages` with its time
    /// (`_synaps_at`); what was loaded is settled and never stamped.
    pub(crate) message_stamper: crate::runtime::message_time::Stamper,
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
    /// Optional events (child spend frames), on their own channel: a reader
    /// behind on one channel never loses the other's envelopes.
    pub(crate) optional_events: broadcast::Sender<Envelope>,
    pub(crate) view: Arc<arc_swap::ArcSwap<RuntimeView>>,
    pub(crate) attached: HashMap<ClientId, AttachedClient>,
    /// B1: the one client whose input commands are honoured (`None` = any
    /// host-originated sender only). With `client_input = shared` it is the
    /// *active* client: the drive client that sent input last.
    pub(crate) input_owner: Option<ClientId>,
    /// Who may drive (config `client_input`, or the create override).
    pub(crate) client_input: ClientInput,
    /// Bumped on each input from a client ([`AttachedClient::last_input`]).
    pub(crate) input_seq: u64,
    pub(crate) next_client_id: u64,
    pub(crate) seq: u64,
    pub(crate) turn_replay: VecDeque<Envelope>,
    /// Last Codex hosted-web_search state line shown (once per session per
    /// model/state; see `codex_wire::session_web_search_notice`).
    pub(crate) web_search_notice: Option<String>,
    pub(crate) state: AttachState,
    pub(crate) background: BackgroundTasks,
    /// B2: spawned compaction in flight (`busy` while `Some`).
    pub(crate) compact: Option<CompactionJob>,
    /// `compact_inline` is running (it blocks the actor; listings still
    /// see the session busy).
    pub(crate) compacting_inline: bool,
    /// `on_session_start` for this session. `await_extensions=false`: a
    /// task waits for process-level discovery and its turn at the host's
    /// gate (one hook at a time), then calls it, so boot never blocks on
    /// it. A turn that comes first moves it ahead of background hooks and
    /// waits for it without blocking the loop (`turn_awaiting_start`); a
    /// keep-warm ping waits with its timer off (`holds`).
    pub(crate) session_start: super::session_start::SessionStart,
    /// A started turn whose request waits for `on_session_start`: its
    /// stream opens when the hook has run. The turn is live meanwhile
    /// (`streaming`), so Cancel, End and a driver revocation end it as any
    /// turn, with nothing sent.
    pub(crate) turn_awaiting_start: Option<TurnStart>,
    /// 1 Hz `SubagentRows` cadence while a turn runs (`None` when idle).
    pub(crate) subagent_tick: Option<tokio::time::Interval>,
    /// Worker-registry generation of the current/last turn: a user
    /// interrupt cancels only the workers this turn launched.
    pub(crate) turn_worker_generation: u64,
    /// Mirrors `handle.lifecycle()` (B3 writes Parking/Parked).
    pub(crate) lifecycle: Arc<std::sync::atomic::AtomicU8>,
    /// Mirrors `handle.journal_id()` (B2 stores the successor id).
    pub(crate) journal_id: Arc<arc_swap::ArcSwap<String>>,
    /// Mirrors `handle.presence()` (clients / owner / pending prompts).
    pub(crate) presence: Arc<arc_swap::ArcSwap<super::handle::Presence>>,
    /// Mirrors `handle.name()`; `sync_name` after any rename.
    pub(crate) name: Arc<arc_swap::ArcSwap<Option<String>>>,
    /// Mirrors `handle.pinned()`; written with `keep_warm` (create seeds it
    /// from `meta.pinned`, `KeepWarm{on}` stores it).
    pub(crate) keep_warm_cell: Arc<std::sync::atomic::AtomicBool>,
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
    /// Set by a reload checkpoint until the `execv` (or until the daemon
    /// reports it failed): see [`ReloadFence`].
    pub(crate) reload_fence: Option<ReloadFence>,
    /// Set while a checkpoint lets its quiesced turn end on its own, to the
    /// interruption that checkpoint records: a turn ending then starts no
    /// turn of its own, and a round that fails keeps the reply text clients
    /// saw, as the checkpoint's cancel would have.
    pub(crate) checkpoint_grace: Option<crate::engine::interrupt::InterruptReason>,
    /// The reload generation this session already resumed a cut turn for
    /// (`ResumeAfterReload`): a repeat for the same generation queues
    /// nothing.
    pub(crate) resumed_after_reload: Option<u64>,
    /// The crash (the dead daemon's `pid:start_ticks:boot_id`) this session
    /// already resumed a cut turn for (`ResumeAfterCrash`): a repeat queues
    /// nothing.
    pub(crate) resumed_after_crash: Option<String>,
    /// This actor's load folded a crashed turn's draft into the history
    /// (`recover_turn_draft`): `Some(writer)`, the process that ran that
    /// turn. `None`: no draft was folded. Only a draft the crashed daemon
    /// wrote makes a crash restore resume the turn.
    pub(crate) crash_draft: Option<Option<String>>,
    /// Shared with the handle: the crash-record entry (`session::live`).
    pub(crate) live: super::live::LiveSlot,
    /// When a crash or stop restore resumed this session's cut turn (the
    /// crash record carries it: a crash soon after it suspects a loop).
    pub(crate) restore_resumed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock time this session parked (`None` while Live); the crash
    /// record carries it so a restore keeps the eviction timer.
    pub(crate) parked_since: Option<chrono::DateTime<chrono::Utc>>,
    /// This actor's publish counter (`LiveEntry::gen`).
    pub(crate) live_gen: u64,
    /// A reload's drain stopped this session's last turn at a round
    /// boundary (the quiesce stop): the reload cut it as surely as a
    /// cancel, so its checkpoint records `turn_cut`. Cleared when a turn
    /// starts.
    pub(crate) reload_quiesced_turn: bool,
    /// How many messages followed that paused turn's marker once its own
    /// tail (the events it flushed) was in: a user message past them
    /// re-prompted the turn, so a refused reload does not resume it
    /// (`SessionCommand::ReloadRefused`).
    pub(crate) reload_quiesced_tail: Option<usize>,
    /// A reload refused (`SessionCommand::ReloadRefused`) while this turn
    /// was streaming, and when: if the turn's stream had already stopped
    /// for the reload (`EventQueue::take_quiesce_stop`), its end resumes it
    /// in place. Cleared at the turn's end and when a turn starts.
    pub(crate) reload_refused_pending: Option<chrono::DateTime<chrono::Utc>>,
    /// Shared with the handle: `config.memory_mode`, readable by a reload
    /// that gets no checkpoint reply.
    pub(crate) memory_mode_slot: super::handle::MemoryModeSlot,
    /// This runtime's memory has been seen active (a grant took effect).
    /// Only then does "memory is off" mean consent was revoked; see
    /// `effective_memory_mode`. Reset whenever the runtime is rebuilt.
    pub(crate) memory_seen_active: bool,
    /// While parked: the exact tool activations the dropped runtime held,
    /// handed to the runtime the unpark builds (and to a reload record
    /// written while parked). Empty while live.
    pub(crate) parked_tool_activations: Vec<crate::tools::activation::CarriedActivation>,
    // ── spawned-child events (`session::child_events`) ──
    /// This session's route; set on every runtime the actor builds.
    pub(crate) child_route: super::child_events::ChildRoute,
    pub(crate) child_rx: mpsc::UnboundedReceiver<super::child_events::ChildProcessEvent>,
    /// Holds a foreground event until its call's `tool_use` went out.
    pub(crate) child_order: super::child_events::EventOrder,
    /// The armed capacity resume and its chain (`session::actor_capacity`).
    pub(crate) capacity: super::actor_capacity::CapacityState,
    /// (S1) Focus hold mode: the held store, the lease (`session::actor_held`).
    pub(crate) hold: super::actor_held::Hold,
    /// `fork` bookkeeping (`session::fork`).
    pub(crate) fork: super::fork::ForkState,
}

/// A reload checkpoint wrote this session's reload record; until the
/// `execv` replaces the process nothing may start new work the record does
/// not know about. Queue wakes stay queued (the record carries the wakes it
/// owes), and keep-warm pings, driver ticks and parking wait. Lifted by
/// `ReloadAborted` (the exec failed) or, as a backstop, at `deadline`.
pub(crate) struct ReloadFence {
    pub(crate) deadline: tokio::time::Instant,
    /// What the checkpoint recorded: re-announced to this image when the
    /// exec does not happen.
    pub(crate) workers: Vec<crate::runtime::subagent::CarriedWorker>,
}

/// A create that fails after taking the journal lock: a FRESH session never
/// journaled, so its `.lock` would be litter next to no `.json` (F18). A
/// continued session's lock file sits next to its journal and stays.
fn release_fresh_lock(lock: Option<agent_core::session_lock::SessionLock>, continued: bool) {
    if let (Some(lock), false) = (lock, continued) {
        lock.release_and_remove();
    }
}

impl SessionActor {
    /// A create with `await_extensions` (the daemon's client creates) first
    /// waits for process-level extension discovery, bounded by
    /// `startup.extensions_ready_timeout_secs` (config, default 30; the
    /// const is the fallback). The host runs this BEFORE the create reserves
    /// its session, so no other create of that session (a crash restore's)
    /// queues behind a discovery still running.
    pub(crate) async fn wait_extensions_for_create(host: &Arc<EngineHost>, cfg: &SessionConfig) {
        if !cfg.await_extensions {
            return;
        }
        let budget = extensions_ready_timeout(&host.config());
        if tokio::time::timeout(budget, host.extensions_ready()).await.is_err() {
            tracing::warn!(
                budget_secs = budget.as_secs(),
                "extensions_ready timed out — on_session_start may miss late extensions"
            );
        }
    }

    /// = today's `setup::boot()` per session (foreground_runtime →
    /// resolve_session_and_prompt → journal lock → set_cwd → model override →
    /// spawn_session_background → finish_session_setup) then
    /// `on_session_start` (keyed injection). `confirm` gets the session's id
    /// (and whether it is a new session) once it is resolved, before the
    /// build writes anything: the host's create reservation
    /// ([`crate::host::CreateReservation::confirm`]); `Err` stops the build.
    pub(crate) async fn create(
        host: &Arc<EngineHost>,
        mut cfg: SessionConfig,
        confirm: crate::host::ConfirmId<'_>,
    ) -> Result<(SessionHandle, SessionTask)> {
        if cfg.ephemeral_layer_wait_ms.is_some_and(|ms| ms > crate::runtime::ephemeral::MAX_WAIT_MS) {
            return Err(crate::RuntimeError::Config("ephemeral_layer_wait_ms exceeds 15000".into()));
        }
        // cwd goes in BEFORE apply_config (inside foreground_runtime_for) so the
        // one-shot memory binding scopes to the session's project (§4).
        // A session that made its own `/memory` choice (carried by a reload)
        // gets that, not the operator default.
        let mut runtime = host
            .foreground_runtime_with(cfg.cwd.clone(), cfg.memory_mode.is_none())
            .await?;
        if let Some(mode) = cfg.memory_mode {
            runtime.restore_session_memory_mode(mode);
        }
        let config: crate::SynapsConfig = (**host.config()).clone();
        let client_input = cfg.client_input.unwrap_or(config.client_input);

        let mut sb = crate::engine::setup::resolve_session_and_prompt(
            &mut runtime,
            &cfg.continue_session,
            cfg.continue_lookup,
            cfg.system.as_deref(),
            cfg.prompt_manifest.as_deref(),
        )?;
        // Nothing is written yet (session file, per-session socket, registry
        // entry, index record, journal lock): the id this build resolved
        // must be free.
        confirm(&sb.session.id, !sb.continued)?;
        // F10: acquire the per-session journal ownership lock BEFORE anything
        // publishes or writes for this id: the `--name` save, the registry
        // card + per-session socket (`spawn_session_background`), the START
        // index record and `on_session_start`. A refused `--continue` of a
        // session live in another process used to publish its own card over
        // the owner's first, then delete it on drop: the owner stayed live
        // (socket up, journal locked) but `synaps send` and peers lost it.
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

        // `--name` at create: apply BEFORE the registry entry is written so
        // `synaps send --session <name>` and `--continue <name>` resolve
        // from the first second — not only after a later `--continue`.
        if let Some(name) = cfg.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            if let Err(e) = sb.session.set_name(name) {
                release_fresh_lock(session_lock, sb.continued);
                return Err(crate::RuntimeError::Session(format!("--name {name:?}: {e}")));
            }
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
        if let Some(clock) = &cfg.message_clock {
            runtime.set_message_clock(clock.clone());
        }
        // A fork (`session::fork`): the parent's settings, request state and
        // history, and its sandbox policy file, before anything can run.
        // Every failure below drops `fork_policy`, which removes the policy
        // file this create carried.
        let fork_seed = cfg.fork.take();
        let fork_policy = match fork_seed.as_deref() {
            Some(seed) => match super::fork::seed_child(&mut runtime, &mut sb, seed, cfg.persist).await {
                Ok(policy) => Some(policy),
                Err(e) => {
                    release_fresh_lock(session_lock, sb.continued);
                    return Err(e);
                }
            },
            None => None,
        };

        // Shared-inbox files are for whoever is at a keyboard: the gate stays
        // shut until a client owns input (`publish_presence`), so a detached,
        // parked or freshly rehydrated session is never woken by them.
        // The endpoint start (registry scan + socket bind under the
        // registry's file lock) takes this host's turn, so parallel builds
        // queue for it instead of failing "registry lock busy". The guard
        // covers spawn_session_background_gated, whose only other work is
        // the inbox watcher's tokio::spawn.
        let background = {
            let _turn = host.endpoint_start_turn().await;
            crate::engine::setup::spawn_session_background_gated(
                &runtime,
                &sb.session,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
        };
        let background = match background {
            Ok(background) => background,
            Err(e) => {
                release_fresh_lock(session_lock, sb.continued);
                return Err(e);
            }
        };
        crate::engine::setup::finish_session_setup(
            &mut runtime,
            &config,
            &sb.session,
            cfg.cwd.clone(),
            crate::engine::setup::IndexRecord::Start,
        );

        // C2: per-session on_session_start (keyed injection) once the
        // process-level discovery is known-finished. Never re-runs discovery.
        // Every call goes through the host's session-start gate.
        // `await_extensions=false` (TUI, a reload's restore): a spawned task
        // runs it (`session_start` below), so boot never blocks on it.
        // The wait on extension discovery ran before the host's create
        // reservation (`wait_extensions_for_create`, bounded by
        // `startup.extensions_ready_timeout_secs`). `run_at_create` then
        // waits its turn at the gate, behind a running hook and older
        // foreground ones (see `session_start`).
        // A fork whose parent's hook has run gets the parent's injection as
        // the parent sent it (never computed again) and runs no hook.
        let fork_started = match fork_seed.as_deref().and_then(|seed| seed.injection.clone()) {
            Some(injection) => {
                if let Some(content) = injection {
                    runtime
                        .hook_bus()
                        .set_session_injection_for(&sb.session.id, content)
                        .await;
                }
                true
            }
            None => false,
        };
        if cfg.await_extensions && !fork_started {
            super::session_start::run_at_create(host, runtime.hook_bus(), &sb.session.id).await;
        }

        let id = SessionId::from(sb.session.id.clone());
        let (child_route, child_rx) = super::child_events::ChildRoute::channel(id.as_str());
        runtime.set_child_route(Some(child_route.clone()));

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
            input_owner_kind: None,
            awaiting_input: 0,
            roster: Vec::new(),
            client_input: Some(client_input),
            journal_id: sb.session.id.clone(),
            locked_by: None,
            pinned: cfg.keep_warm,
            busy: false,
            state_since: None,
            park_due: None,
            evict_due: None,
            title: None,
            cost_usd: 0.0,
            messages: 0,
            restoring: false,
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
        let crash_draft = if sb.continued && cfg.persist && session_lock.is_some() {
            crate::engine::setup::recover_turn_draft(&mut conv).await
        } else {
            None
        };
        // Loaded (and recovered) history keeps whatever times it carried.
        let message_stamper = crate::runtime::message_time::Stamper::new(conv.api_messages.len());

        // Child spend: the session's sink, for the subagents its turns
        // launch.
        let (spend_sink, spend_rx) = crate::runtime::child_spend::SpendSink::new();
        runtime.set_spend_sink(Some(spend_sink.clone()));
        let ephemeral_layer = crate::runtime::ephemeral::Layer::configured(cfg.ephemeral_layer_wait_ms);
        let ephemeral_changes = ephemeral_layer.subscribe();
        runtime.ephemeral_layer = ephemeral_layer.clone();
        let memory_only = crate::runtime::memory_only::Store::default();
        runtime.memory_only = memory_only.clone();
        let memory_only_globs = crate::runtime::memory_only::SessionGlobs::new(cfg.memory_only_tools.clone());
        runtime.share_memory_only_session(&memory_only_globs);
        let (client_tools, client_tool_frames) = crate::runtime::client_tools::Overlay::new();
        runtime.share_client_tools(&client_tools);
        let view = RuntimeView::from_runtime(&runtime).await;
        let hook_bus = Arc::clone(runtime.hook_bus());
        let event_queue = Arc::clone(runtime.event_queue());
        let keep_warm = cfg.keep_warm;
        let (
            handle,
            SessionEndpoints {
                cmd_rx,
                events,
                optional_events,
                view,
                lifecycle,
                journal_id,
                presence,
                name,
                keep_warm: keep_warm_cell,
                cache_keepwarm,
                memory_mode,
                live,
            },
        ) = SessionHandle::with_quiesce_slot(meta.clone(), view, event_queue.quiesce_slot());
        // The mode is fixed for the session: seed the listing with it now.
        // Otherwise only a presence publish (attach, detach, input) carries
        // it, so a session restored at reload, with no client back yet,
        // listed as `owner=-`. Not `publish_presence()`: that also sets the
        // inbox gate, which stays as `SessionHandle::new` left it.
        presence.store(Arc::new(super::handle::Presence {
            client_input: Some(client_input),
            ..Default::default()
        }));
        *memory_mode.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = cfg.memory_mode;
        let (sp_tx, secret_prompt_rx) = mpsc::unbounded_channel();
        let session_start = if cfg.await_extensions || fork_started {
            super::session_start::SessionStart::Done
        } else if cfg.restore_parked {
            // Parked right after create: the hook waits for the unpark.
            super::session_start::SessionStart::Pending
        } else {
            super::session_start::SessionStart::queue(
                host,
                Arc::clone(&hook_bus),
                Arc::clone(&journal_id),
                extensions_ready_timeout(&config),
            )
        };

        let hold = super::actor_held::Hold::new(id.as_str());
        let mut actor = SessionActor {
            ephemeral_layer,
            memory_only,
            memory_only_globs,
            client_tools,
            client_tool_frames,
            client_tool_installer: None,
            ephemeral_changes,
            ephemeral_installer: None,
            id,
            meta,
            config: cfg,
            runtime: Live::new(runtime),
            conv: Live::new(conv),
            host: Arc::clone(host),
            hook_bus,
            event_queue,
            // A fork replays its parent's settings after an unpark too.
            settings_replay: fork_seed
                .as_deref()
                .map(|seed| seed.settings_replay.clone())
                .unwrap_or_default(),
            keep_warm,
            cache_keepwarm: super::cache_keepwarm::KeepWarmState::new(
                config.cache_keepwarm,
                config.cache_keepwarm_auto_max_hours,
                config.cache_keepwarm_headless,
                cache_keepwarm,
            ),
            spend_sink,
            spend_rx,
            spend_seqs: Default::default(),
            park_deadline: None,
            park_recheck_at: None,
            prompt_abandon_deadline: None,
            stream: None,
            cancel: None,
            steer_tx: None,
            owner_steer_budget: None,
            streaming: false,
            turn_baseline: 0,
            message_stamper,
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
            optional_events,
            view,
            attached: HashMap::new(),
            input_owner: None,
            client_input,
            input_seq: 0,
            next_client_id: 1,
            seq: 0,
            turn_replay: VecDeque::new(),
            web_search_notice: None,
            state: AttachState::Detached { running: false },
            background,
            compact: None,
            compacting_inline: false,
            session_start,
            turn_awaiting_start: None,
            subagent_tick: None,
            turn_worker_generation: 0,
            lifecycle,
            journal_id,
            presence,
            name,
            keep_warm_cell,
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
            reload_fence: None,
            checkpoint_grace: None,
            resumed_after_reload: None,
            resumed_after_crash: None,
            crash_draft,
            live,
            restore_resumed_at: None,
            parked_since: None,
            live_gen: 0,
            reload_quiesced_turn: false,
            reload_quiesced_tail: None,
            reload_refused_pending: None,
            memory_mode_slot: memory_mode,
            memory_seen_active: false,
            parked_tool_activations: Vec::new(),
            child_route,
            child_rx,
            child_order: Default::default(),
            capacity: Default::default(),
            hold,
            fork: Default::default(),
        };
        // Whether the build's grant (default or restored choice) took.
        actor.observe_memory();
        if actor.keep_warm {
            actor.note_pin();
        }
        actor.fork_note_idle();
        actor.publish_live(false);
        // The fork exists: its carried policy file stays.
        if let Some(policy) = fork_policy {
            policy.keep();
        }
        Ok((handle, SessionTask(actor)))
    }

    /// Publish `conv.session.name` to the handle (listings, `--continue
    /// <name>`) and rewrite this session's registry entry so `synaps send
    /// --session <name>` resolves the new name. Call after any rename.
    /// Write the pin as it stands now to the daemon's pin book (no-op
    /// in-process): the entry is added while pinned, dropped otherwise.
    pub(crate) fn note_pin(&self) {
        let Some(sink) = self.host.pin_sink() else { return };
        let journal = (**self.journal_id.load()).clone();
        if self.keep_warm {
            let kw = self.cache_keepwarm.override_mode.map(|m| m.to_string());
            sink.pinned(self.id.as_str(), &journal, self.conv.session.name.as_deref(), self.config.cwd.as_deref(), kw.as_deref());
        } else {
            sink.unpinned(self.id.as_str(), &journal);
        }
    }

    /// Publish a new journal id; a pinned session's book entry follows it.
    pub(crate) fn set_journal_id(&mut self, new_id: String) {
        let old = self.journal_id.swap(Arc::new(new_id.clone()));
        if self.keep_warm && *old != new_id {
            if let Some(sink) = self.host.pin_sink() {
                sink.moved(&old, &new_id);
            }
        }
    }

    pub(crate) fn sync_name(&mut self) {
        let name = self.conv.session.name.clone();
        self.mirror_name(name);
    }

    /// Publish a name the session now has on disk (or in memory): handle
    /// and listing, the `run/<sid>.json` card, and a pinned session's book
    /// entry (verbatim, so a cleared name stops resolving).
    pub(crate) fn mirror_name(&mut self, name: Option<String>) {
        self.meta.name = name.clone();
        self.name.store(Arc::new(name.clone()));
        if self.keep_warm {
            if let Some(sink) = self.host.pin_sink() {
                let journal = (**self.journal_id.load()).clone();
                sink.renamed(&journal, name.as_deref());
            }
        }
        if let Err(e) = self.background.update_session_name(name.as_deref()) {
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
        // Host-internal ledger events never reach clients; the stream
        // intake already normalizes them.
        let Some(event) = client_form(event) else {
            return;
        };
        let replay = self.streaming
            && !matches!(
                event,
                SessionEventWire::Prompt(_)
                    | SessionEventWire::PromptResolved { .. }
                    | SessionEventWire::Attached { .. }
                    | SessionEventWire::QueryResult { .. }
                    | SessionEventWire::Forked { .. }
                    // Client tools v1: addressed, and an old invoke must
                    // never reach a late attach or a Resync.
                    | SessionEventWire::ClientTools { .. }
                    | SessionEventWire::ToolInvoke { .. }
                    | SessionEventWire::ToolCancel { .. }
                    | SessionEventWire::ToolReturnAck { .. }
                    | SessionEventWire::Conversation(_)
                    | SessionEventWire::ChildProcess(_)
                    | SessionEventWire::ChildSpend(_)
                    | SessionEventWire::ChildSpendLagged { .. }
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
        // One seq counter, two channels: optional events (child spend
        // frames) go out apart from the main stream, so their volume never
        // evicts an ordinary event.
        super::handle::publish(&self.events, &self.optional_events, env);
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
        self.stamp_new_messages();
        let snap = self.conv.snapshot(self.consecutive_auto_turns);
        self.emit(SessionEventWire::Conversation(snap));
    }

    /// Give every message appended since the last pass its time. Called
    /// wherever history leaves the actor (turn start, save, clients), so a
    /// message added outside a turn (events flushed at Done, notes,
    /// interruption markers) is stamped too. O(new messages).
    pub(crate) fn stamp_new_messages(&mut self) {
        if !self.conv.is_live() || !self.runtime.is_live() {
            return;
        }
        let clock = self.runtime.message_clock().clone();
        self.message_stamper.stamp(&mut self.conv.api_messages, &clock);
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
        self.stamp_new_messages();
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
        // A turn may have revoked memory consent (the `memory_context`
        // tool): keep the handle's slot current for a reload that gets no
        // checkpoint reply.
        self.sync_memory_choice();
        self.flush_saves().await;
        // Before `Idle`, so a test that saw `Idle` knows the hook is in line.
        #[cfg(any(test, feature = "testing"))]
        if super::session_start::testing::take_requeue(&self.journal_id.load()) {
            self.queue_session_start();
        }
        self.emit(SessionEventWire::Idle);
        self.cache_keepwarm_on_idle().await;
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
        self.turn_draft_rolled_over();
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
            // A consent revocation since the last record counts.
            config: SessionConfig {
                memory_mode: self.effective_memory_mode(),
                ..self.config.clone()
            },
            keep_warm: self.keep_warm,
            lifecycle: SessionLifecycle::from_u8(
                self.lifecycle.load(std::sync::atomic::Ordering::Acquire),
            ),
            settings_replay: self.settings_replay.clone(),
            cache_keepwarm: self.cache_keepwarm.override_mode.map(|m| m.to_string()),
            model: view.model.clone(),
            thinking_level: view.thinking_level.clone(),
            workers: Vec::new(),
            tool_activations: if self.runtime.is_live() {
                self.runtime.carried_tool_activations()
            } else {
                self.parked_tool_activations.clone()
            },
            turn_cut: false,
            cut_tail: None,
            capacity_stop: self.capacity.stop.clone(),
            parked_since: self.parked_since,
        }
    }

    /// Handle ids of the workers running right now (none while parked).
    fn running_worker_ids(&self) -> Vec<String> {
        if !self.runtime.is_live() {
            return Vec::new();
        }
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows()
            .iter()
            .filter(|r| matches!(r.status, crate::runtime::subagent::SubagentStatus::Running))
            .map(|r| r.handle_id.clone())
            .collect()
    }

    /// Publish this session's crash-record entry (`session::live`) and mark
    /// the record dirty; `urgent` at a turn start, so the daemon writes the
    /// record at once rather than after its debounce.
    pub(crate) fn publish_live(&mut self, urgent: bool) {
        let entry = super::live::LiveEntry {
            record: self.reload_record(),
            turn_in_flight: self.streaming,
            running_workers: self.running_worker_ids(),
            resumed_at: self.restore_resumed_at,
            parked_since: self.parked_since,
            gen: {
                self.live_gen += 1;
                self.live_gen
            },
        };
        self.live.store(Some(Arc::new(entry)));
        super::live::mark_dirty(urgent);
    }

    /// Publish again if the set of running workers changed since the last
    /// entry (a worker started or finished between turn boundaries).
    fn republish_live_if_workers_changed(&mut self) {
        let running = self.running_worker_ids();
        let changed = self
            .live
            .load()
            .as_ref()
            .is_none_or(|e| e.running_workers != running);
        if changed {
            self.publish_live(false);
        }
    }

    /// Also the one place the inbox gate follows `input_owner`: every owner
    /// change (attach, detach, takeover) publishes presence. `Observe`
    /// clients never own input, so a keep-warm observer keeps it shut.
    ///
    /// Also the `/sessions` state: busy, `state_since` (restamped whenever
    /// idle/busy/parked changes), the park / eviction deadline as wall
    /// clock, and the conversation's title / cost / message count (kept as
    /// last published while Parked). Every transition that moves them —
    /// turn and compaction start/end, attach/detach, `rearm_park`,
    /// park/unpark — ends in a publish; stream deltas do not.
    pub(crate) fn publish_presence(&self) {
        use super::handle::{Presence, PresenceState};
        self.background
            .set_inbox_accepting(self.input_owner.is_some());
        let prev = self.presence.load();
        let busy = self.busy() || self.compacting_inline;
        let parked = self.is_parked();
        let state = if parked {
            PresenceState::Parked
        } else if busy {
            PresenceState::Busy
        } else {
            PresenceState::Idle
        };
        let now = chrono::Utc::now();
        let state_since = match prev.state_since {
            Some(t) if prev.state == state => Some(t),
            _ => Some(now),
        };
        let wall = |at: tokio::time::Instant| {
            let left = at.saturating_duration_since(tokio::time::Instant::now());
            chrono::Duration::from_std(left)
                .ok()
                .and_then(|d| now.checked_add_signed(d))
        };
        // A worker re-check is not a park (it only looks again), nor is a
        // grace that workers now hold: its firing would only re-check. The
        // F18 idle end of a zero-turn session counts — it leaves Live too.
        let park_due = self
            .park_deadline
            .filter(|d| !parked && Some(*d) != self.park_recheck_at)
            .filter(|_| self.can_end_idle() || self.can_park())
            .and_then(wall);
        let evict_due = self.park_deadline.filter(|_| parked).and_then(wall);
        let (title, cost_usd, messages) = if self.conv.is_live() {
            (
                conversation_title(&self.conv),
                self.conv.session_cost,
                self.conv.api_messages.len(),
            )
        } else {
            (prev.title.clone(), prev.cost_usd, prev.messages)
        };
        self.presence.store(Arc::new(Presence {
            clients: self.attached.len(),
            input_owner: self.input_owner,
            // The kind the owner declared at attach; `None` with no owner
            // (and, defensively, for an owner no longer attached).
            input_owner_kind: self
                .input_owner
                .and_then(|o| self.attached.get(&o))
                .map(|a| a.meta.kind),
            awaiting_input: self.pending_prompts.len(),
            roster: self.roster(),
            client_input: Some(self.client_input),
            busy,
            state,
            state_since,
            park_due,
            evict_due,
            title,
            cost_usd,
            messages,
        }));
    }

    /// Every attached client, by id.
    pub(crate) fn roster(&self) -> Vec<ClientInfo> {
        let mut roster: Vec<ClientInfo> = self
            .attached
            .iter()
            .map(|(id, a)| ClientInfo {
                id: *id,
                name: a.name.clone(),
                kind: a.meta.kind,
                mode: a.mode,
                attached_at_ms: a.attached_at_ms,
            })
            .collect();
        roster.sort_by_key(|c| c.id.0);
        roster
    }

    /// `tui (#1)`, or `client #1` for an id that isn't attached.
    fn client_label(&self, id: ClientId) -> String {
        match self.attached.get(&id) {
            Some(a) => format!("{} (#{})", a.name, id.0),
            None => format!("client #{}", id.0),
        }
    }

    /// Why client `c` may not send an input command, if it may not.
    /// `exclusive`: anyone but the owner. `shared`: observers (and clients
    /// that aren't attached); every drive client may.
    fn input_refusal(&self, c: ClientId) -> Option<String> {
        match self.client_input {
            ClientInput::Exclusive => (self.input_owner != Some(c)).then(|| match self.input_owner {
                // PARITY: 790d R6: the app maps the "input owned by client
                // #<id>" prefix to not_input_owner (upstream: "input owned
                // by tui (#1)"); the client's name follows the id.
                Some(o) => match self.attached.get(&o) {
                    Some(a) => format!("input owned by client #{} ({})", o.0, a.name),
                    None => format!("input owned by client #{}", o.0),
                },
                None => "input has no owner; attach with --takeover".to_string(),
            }),
            ClientInput::Shared => match self.attached.get(&c).map(|a| a.mode) {
                Some(AttachMode::Mirror | AttachMode::Takeover) => None,
                Some(AttachMode::Observe) => {
                    Some("this client is read-only (attached with --observe)".to_string())
                }
                None => Some("this client is not attached".to_string()),
            },
        }
    }

    /// Client `c` sent input (or answered a prompt): remember when, and with
    /// `client_input = shared` make it the active client. Observers never
    /// become active.
    fn note_input(&mut self, c: ClientId) {
        let Some(a) = self.attached.get_mut(&c) else { return };
        if a.mode == AttachMode::Observe {
            return;
        }
        self.input_seq += 1;
        a.last_input = self.input_seq;
        if self.client_input == ClientInput::Shared && self.input_owner != Some(c) {
            let from = self.input_owner;
            // PARITY: client tools belong to the input owner (ours' rule):
            // a new active client drops another installer's registration.
            self.client_tools_owner_changed(Some(c));
            self.input_owner = Some(c);
            self.emit(SessionEventWire::InputOwnerChanged { from, to: Some(c), reason: OwnerChangeReason::Input });
            self.publish_presence();
        }
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
        // An active cache keep-warm period defers the idle auto-park (its
        // cost is bounded by the ski-rental ledger); when it ends,
        // `rearm_park`. An explicit `Park` ends the period first.
        self.can_park_ignoring_keepwarm() && !self.cache_keepwarm.active()
    }

    /// `can_park` minus the keep-warm deferral.
    pub(crate) fn can_park_ignoring_keepwarm(&self) -> bool {
        self.can_park_but_for_workers() && !self.workers_hold_park()
    }

    /// Whether parking would cost a worker (`SubagentRegistry::holds_park`):
    /// the park waits and re-checks every [`WORKER_PARK_RECHECK`].
    fn workers_hold_park(&self) -> bool {
        self.runtime.is_live()
            && self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .holds_park()
    }

    /// Every park condition except the session's workers.
    fn can_park_but_for_workers(&self) -> bool {
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
            // Rule A (child spend): a worker still reporting spend is real
            // work; so are reports not yet booked.
            && self.spend_quiet()
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

    // ── autonomous work: the reload fence and on_session_start ───────────

    /// Whether autonomous work may start now, and if not, why. Two states
    /// hold it back: a reload's fence (#255) until the exec or its abort, and
    /// this session's `on_session_start` until it has run. `Fenced` wins when
    /// both hold. Each source of autonomous work asks [`Self::holds`]; none
    /// reads the fence or the hook's state itself.
    ///
    /// | source          | `Fenced`  | `SessionStartPending`                                         |
    /// |-----------------|-----------|---------------------------------------------------------------|
    /// | queue wake      | waits     | runs: the turn starts, and its request waits for the hook      |
    /// | park timer      | waits     | runs: a park that happens takes a hook not called out of line  |
    /// | keep-warm timer | waits     | waits (timer off): a due ping goes out once the hook has run   |
    /// | driver tick     | waits     | runs: an armed driver is revoked (`driver_idle_conflict`)      |
    /// | driver start    | refused * | rejected in the tick (`driver_idle_conflict`)                  |
    /// | capacity resume | waits     | runs: consumed, and its turn's request waits for the hook      |
    ///
    /// \* `DriverStart` is a command: under the fence the command fence
    /// refuses it first ("daemon reloading"); the tick never sees it.
    ///
    /// The queue wake must not wait for the hook: a parked session's hook is
    /// out of line (`SessionStart::Pending`) and joins it only at the unpark,
    /// and the unpark is what the queue wake does. Waiting there would leave
    /// an event for a parked session queued for good. The turn's own wait
    /// (`open_turn_stream` after `session_start_ready`) holds its request
    /// instead, without blocking the loop. A capacity resume runs for the
    /// same reason: a parked session's resume is what unparks it.
    pub(crate) fn autonomous_work_blocked(&self) -> Option<WorkBlock> {
        if self.reload_fence.is_some() {
            Some(WorkBlock::Fenced)
        } else if !self.session_start.is_done() {
            Some(WorkBlock::SessionStartPending)
        } else {
            None
        }
    }

    /// Whether `source` waits now (the table on
    /// [`Self::autonomous_work_blocked`]).
    pub(crate) fn holds(&self, source: WorkSource) -> bool {
        match self.autonomous_work_blocked() {
            None => false,
            Some(WorkBlock::Fenced) => true,
            Some(WorkBlock::SessionStartPending) => matches!(source, WorkSource::KeepWarmTimer),
        }
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
        match self.autonomous_work_blocked() {
            Some(WorkBlock::Fenced) => return Some("daemon reloading"),
            Some(WorkBlock::SessionStartPending) => return Some("extensions loading or reloading"),
            None => {}
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
        if self.holds(WorkSource::DriverTick) {
            return;
        }

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
                    // This includes a pending `on_session_start`: a driver
                    // turn opens its stream below, not through `start_turn`,
                    // so it never waits for the hook and must not run before it.
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
                        Ok(mut candidate) => {
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
                            candidate.ephemeral_layer = self.ephemeral_layer.clone();
                            candidate.memory_only = self.memory_only.clone();
                            // The session's one shared glob list: a live
                            // `Set{MemoryOnlyTools}` since then still holds.
                            candidate.share_memory_only_session(&self.memory_only_globs);
                            candidate.share_client_tools(&self.client_tools);
                            *self.runtime = candidate;
                            self.conv.session.model = self.runtime.model().to_owned();
                            self.conv.session.thinking_level =
                                self.runtime.thinking_level().to_owned();
                            self.consecutive_auto_turns = 0;
                            // Admission: the request gets its time now.
                            self.stamp_new_messages();
                            self.turn_baseline = self.conv.api_messages.len();
                            self.turn_steered_events.clear();
                            self.turn_replay.clear();
                            self.streaming = true;
                            self.fork_turn_started();
                            self.capacity_turn_started();
                            self.open_turn_draft();
                            self.update_attach_state();
                            self.publish_live(true);
                            self.emit(SessionEventWire::TurnStarted {
                                turn_baseline: self.turn_baseline,
                                trigger: TurnTrigger::DriverAuto,
                                user_text: Some(proposal.prompt.clone()),
                            });
                            self.announce_web_search_state();

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
                            self.owner_steer_budget = None;
                            let mut runtime = self.runtime.clone();
                            // Explicit same-session stream handoff, not generic clone inheritance.
                            runtime.ephemeral_layer = self.ephemeral_layer.clone();
                            runtime.memory_only = self.memory_only.clone();
                            runtime.share_memory_only_session(&self.memory_only_globs);
                            runtime.share_client_tools(&self.client_tools);
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
            && self.spend_quiet()
    }

    pub(crate) fn rearm_park(&mut self) {
        self.rearm_park_deadline();
        self.publish_presence();
    }

    fn rearm_park_deadline(&mut self) {
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
            // Only workers in the way: look again later (a worker finishing
            // or being collected does not always pass through here).
            let workers_only = self.can_park_but_for_workers()
                && !self.cache_keepwarm.active()
                && park_grace().is_some();
            self.park_deadline = if workers_only {
                self.park_deadline.or_else(|| {
                    let at = tokio::time::Instant::now() + worker_park_recheck();
                    self.park_recheck_at = Some(at);
                    Some(at)
                })
            } else {
                None
            };
            return;
        }
        match park_grace() {
            Some(grace) => {
                // The earlier of a pending deadline and a fresh grace: a
                // grace armed at detach keeps counting, while a far-off
                // worker re-check gives way once the workers let go.
                let due = tokio::time::Instant::now() + grace;
                // A re-check due before the grace stays a re-check: when it
                // fires it arms the grace rather than parking early.
                self.park_deadline = Some(self.park_deadline.map_or(due, |d| d.min(due)));
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
        self.publish_live(false);
    }

    /// Save, close PTYs, drop `conv` THEN `runtime`. `background` (inbox
    /// watcher + per-session UDS + registry) stays: `synaps send` keeps
    /// resolving and its push into `event_queue` is the wake-up.
    pub(crate) async fn park(&mut self) -> std::ops::ControlFlow<EndReason> {
        let flow = self.park_now().await;
        self.publish_presence();
        // Restored to be parked but still live (it could not park): its
        // `on_session_start` runs now, as for any live session.
        if flow.is_continue()
            && !self.is_parked()
            && matches!(self.session_start, super::session_start::SessionStart::Pending)
        {
            self.queue_session_start();
        }
        flow
    }

    async fn park_now(&mut self) -> std::ops::ControlFlow<EndReason> {
        // Fired by a worker re-check (not the grace, not an explicit Park)?
        let recheck = self.park_deadline.is_some() && self.park_deadline == self.park_recheck_at;
        self.park_recheck_at = None;
        self.park_deadline = None;
        if self.is_parked() {
            if self.attached.is_empty() {
                // An armed capacity resume holds the eviction: it unparks
                // the session at its time.
                if self.capacity_holds_eviction() {
                    return std::ops::ControlFlow::Continue(());
                }
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
            // Re-arms a re-check while only workers hold the park.
            self.rearm_park();
            return std::ops::ControlFlow::Continue(());
        }
        if recheck {
            // The workers let go: now the reconnect grace runs, from here.
            // A re-check is not the user's grace having elapsed.
            if let Some(grace) = park_grace() {
                self.park_deadline = Some(tokio::time::Instant::now() + grace);
                return std::ops::ControlFlow::Continue(());
            }
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
        // The unparked runtime is built from `config.memory_mode`: a
        // revocation made without `/memory` must be recorded first.
        self.sync_memory_choice();
        // The runtime's exact tool activations outlive it: the unparked
        // runtime restores them, so the tools array (the start of the cached
        // prompt prefix) and the model's tools are what they were.
        self.parked_tool_activations = self.runtime.carried_tool_activations();
        // The Status query names a parked session's parent too.
        self.fork.parked_from = self.conv.session.forked_from.clone();
        // Last look at the conversation for listings (title/cost/messages).
        self.publish_presence();
        // Drop order: conv first, then runtime — a panic between them can
        // never leave a Runtime without state to save.
        // Memory-only results die with the runtime: an unparked session
        // gets them back only by calling the tool again.
        self.memory_only.clear();
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
        // A hook not called yet waits for the unpark, out of line.
        self.session_start.withdraw_for_park();
        self.state = AttachState::Parked;
        // A session a crash or stop restore put back parked keeps the time
        // it first parked (its eviction timer continues); any other park
        // starts now.
        let now = chrono::Utc::now();
        let since = self.config.restore_parked_since.take().filter(|s| *s <= now).unwrap_or(now);
        self.parked_since = Some(since);
        self.set_lifecycle(SessionLifecycle::Parked);
        tracing::info!(session = %self.id, "session parked");
        // The park timer is reused as the eviction timer: a session nobody
        // re-attaches to within `parked_evict_after` leaves the map (its
        // journal stays; `--continue` brings it back).
        let parked_for = (now - since).to_std().unwrap_or_default();
        self.park_deadline =
            parked_evict_after().map(|d| tokio::time::Instant::now() + d.saturating_sub(parked_for));
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
        let spend_sink = self.spend_sink.clone();
        let replay = self.settings_replay.clone();
        let journal_present = self.journal_exists();
        let view = self.view.load_full();
        let build = async move {
            let config: crate::SynapsConfig = (**host.config()).clone();
            // §4: cwd before apply_config — see `create`. The session's own
            // `/memory` choice, if it made one, instead of the default.
            let mut runtime = host
                .foreground_runtime_with(cfg.cwd.clone(), cfg.memory_mode.is_none())
                .await?;
            if let Some(mode) = cfg.memory_mode {
                runtime.restore_session_memory_mode(mode);
            }
            runtime.set_event_queue(queue);
            runtime.set_spend_sink(Some(spend_sink));
            let mut sb = if journal_present {
                crate::engine::setup::resolve_session_and_prompt(
                    &mut runtime,
                    &Some(Some(journal_id)),
                    crate::session::ContinueLookup::Query,
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
                    crate::session::ContinueLookup::Query,
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
            if let Some(clock) = &cfg.message_clock {
                runtime.set_message_clock(clock.clone());
            }
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
        let (mut runtime, sb) = match tokio::time::timeout(budgets::UNPARK_TIMEOUT, build).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(crate::RuntimeError::Session(format!(
                    "unpark timed out after {}s",
                    budgets::UNPARK_TIMEOUT_SECS
                )))
            }
        };
        runtime.ephemeral_layer = self.ephemeral_layer.clone();
        runtime.memory_only = self.memory_only.clone();
        runtime.share_memory_only_session(&self.memory_only_globs);
        runtime.share_client_tools(&self.client_tools);
        let mut conv = ConversationState::from_resumed(sb.session);
        conv.api_messages = sb.api_messages;
        conv.total_input_tokens = sb.total_input_tokens;
        conv.total_output_tokens = sb.total_output_tokens;
        conv.session_cost = sb.session_cost;
        if journal_present && self.config.persist && self.session_lock.is_some() {
            crate::engine::setup::recover_turn_draft(&mut conv).await;
        }
        self.message_stamper.settle(conv.api_messages.len());
        runtime.restore_tool_activations(std::mem::take(&mut self.parked_tool_activations));
        runtime.set_child_route(Some(self.child_route.clone()));
        self.runtime.unpark_set(runtime);
        self.conv.unpark_set(conv);
        // The name comes from disk: a rename while parked wrote it there.
        if self.meta.name != self.conv.session.name {
            let name = self.conv.session.name.clone();
            self.mirror_name(name);
        }
        self.fork_note_idle();
        // A fresh runtime: whether its grant took effect is its own.
        self.memory_seen_active = false;
        self.observe_memory();
        self.publish_view().await;
        self.state = AttachState::Detached { running: false };
        self.parked_since = None;
        self.set_lifecycle(SessionLifecycle::Live);
        self.update_attach_state();
        // A hook that waited for the unpark runs now (once per session).
        if matches!(self.session_start, super::session_start::SessionStart::Pending) {
            self.queue_session_start();
        }
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
        let res = self
            .unpark()
            .await
            .map_err(|e| format!("session parked and could not be restored: {e}"));
        self.publish_presence();
        res
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
        self.republish_live_if_workers_changed();
        if !keep_subagent_tick(self.streaming, running) {
            self.subagent_tick = None;
            // The last worker finished: a park it held may be due now.
            self.rearm_park();
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

    /// Codex hosted web_search is read from this (engine/daemon) process's
    /// env, so say so in the session — once per session per model/state,
    /// inside the turn so its replay ring carries it to clients attaching
    /// mid-turn.
    fn announce_web_search_state(&mut self) {
        let notice = crate::runtime::openai::codex_wire::session_web_search_notice(
            self.runtime.model(),
            crate::runtime::openai::codex_wire::engine_env,
        );
        if notice.is_some() && notice != self.web_search_notice {
            self.web_search_notice = notice.clone();
            if let Some(text) = notice {
                self.emit(SessionEventWire::Stream(StreamEvent::Session(
                    SessionEvent::Notice(text),
                )));
            }
        }
    }

    // ── turn start (dispatch.rs Submit tail / stream_handler.rs RunTurn) ──

    /// This session's `on_session_start` task ended: the hook ran (or the
    /// task failed, which counts as done: fail-open like a hook timeout).
    /// Refresh the view, which shows the injection, then send what waited
    /// for it: a started turn's request. A due keep-warm ping waited with its
    /// timer off (`holds`); the loop's next pass arms it again.
    async fn session_start_finished(
        &mut self,
        ran: std::result::Result<bool, tokio::task::JoinError>,
    ) {
        use super::session_start::SessionStart;
        match ran {
            Ok(true) => {}
            // Withdrawn before it was called (a park): not run yet. Anything
            // waiting for it puts it back in line.
            Ok(false) => {
                self.session_start = SessionStart::Pending;
                if self.turn_awaiting_start.is_some() || self.cache_keepwarm.deadline().is_some() {
                    self.session_start_ready();
                }
                return;
            }
            Err(e) => tracing::warn!(session = %self.id, "on_session_start task failed: {e}"),
        }
        self.session_start = SessionStart::Done;
        if self.runtime.is_live() {
            self.publish_view().await;
        }
        if let Some(start) = self.turn_awaiting_start.take() {
            // A turn canceled from outside (a driver revocation) ends on its
            // own path (`finish_revoked_turn`) with nothing sent.
            if !start.cancel.is_cancelled() {
                self.open_turn_stream(start).await;
            }
        }
    }

    /// No turn and no keep-warm ping goes out before `on_session_start` has
    /// run. Its injection is part of their system prompt, so a request sent
    /// before it would lack the extension context and the next request would
    /// change the cached system prompt. (A manual `/compact` does not wait:
    /// it builds its own prompt; a rolling summary reuses the request of the
    /// turn before it or builds its own prompt.) A session created with
    /// `await_extensions=false` (quick start, every session a reload
    /// restores) has a task in line at the host's gate for it, or, while
    /// parked, nothing in line until the unpark.
    ///
    /// `true` once the hook has run. Otherwise it puts the hook in line if
    /// it is not (after a park) and moves it ahead of background hooks; the
    /// caller waits for `session_start_finished` without blocking the loop.
    /// The hook itself always runs in that task, through the gate, so it is
    /// never called twice or beside another session's.
    pub(crate) fn session_start_ready(&mut self) -> bool {
        use super::session_start::SessionStart;
        if matches!(self.session_start, SessionStart::Pending) {
            self.queue_session_start();
        }
        match &self.session_start {
            SessionStart::Done => true,
            SessionStart::Queued { ticket, .. } => {
                ticket.prefer();
                false
            }
            SessionStart::Pending => false,
        }
    }

    /// Put this session's `on_session_start` in line (in the background).
    fn queue_session_start(&mut self) {
        self.session_start = super::session_start::SessionStart::queue(
            &self.host,
            Arc::clone(&self.hook_bus),
            Arc::clone(&self.journal_id),
            extensions_ready_timeout(&self.host.config()),
        );
    }

    /// Open the turn's stream: the request goes out.
    async fn open_turn_stream(&mut self, start: TurnStart) {
        // A turn started by events (nobody typed it) runs under the event
        // turn budget for this turn only; the stream copies the budget when
        // it starts, so the session budget is restored right after. Here and
        // not in `start_turn`: a turn whose request waited for
        // `on_session_start` opens its stream later, under its own budget.
        let session_budget = start.event_turn.then(|| {
            let event_budget = self
                .runtime
                .turn_budget()
                .for_event_turn(&self.host.config().turn_budgets);
            let session_budget = self.runtime.turn_budget().clone();
            self.runtime.set_turn_budget(event_budget);
            self.runtime
                .set_owner_steer_budget(self.owner_steer_budget.clone());
            session_budget
        });
        let stream = self
            .runtime
            .run_stream_tracked_for_trigger(
                self.conv.api_messages.clone(),
                start.cancel,
                Some(start.steer_rx),
                Some(self.secret_prompt_handle.clone()),
                start.auto_approve,
                start.completion,
                start.event_turn,
            )
            .await;
        if let Some(session_budget) = session_budget {
            self.runtime.set_turn_budget(session_budget);
            self.runtime.set_owner_steer_budget(None);
        }
        self.stream = Some(stream);
    }

    /// Start a turn. Its request goes out at once, or, while this session's
    /// `on_session_start` has not run, once it has (`session_start_ready`);
    /// the turn is live from here either way.
    pub(crate) async fn start_turn(&mut self, trigger: TurnTrigger, user_text: Option<String>) {
        self.cache_keepwarm_turn_started();
        self.reload_quiesced_turn = false;
        self.reload_refused_pending = None;
        self.event_queue.take_quiesce_stop();
        // Before the publish below, which carries the clear.
        self.capacity_turn_started();
        let ct = CancellationToken::new();
        let (s_tx, s_rx) = mpsc::unbounded_channel::<String>();
        self.streaming = true;
        // Admission: the request (and anything queued with it) gets its
        // time now, before the stream copies the history.
        self.stamp_new_messages();
        self.turn_baseline = self.conv.api_messages.len();
        self.fork_turn_started();
        self.open_turn_draft();
        self.turn_steered_events.clear();
        self.turn_replay.clear();
        self.update_attach_state();
        // The crash record says a turn runs here before the turn can do
        // anything a crash would cut.
        self.publish_live(true);
        self.emit(SessionEventWire::TurnStarted {
            turn_baseline: self.turn_baseline,
            trigger,
            user_text,
        });
        self.announce_web_search_state();
        // Blocks command processing during setup exactly like the TUI loop.
        // (E-P7, §S9) An armed driver MUST NOT auto-approve tool activation —
        // the session-driver contract keeps ordinary tool-approval gates in
        // force. Override the session config to false whenever a grant is
        // armed, regardless of what the client requested. (Driver-initiated
        // turns already pass `false` from the tick's Prepared arm; this guards
        // any foreground turn that starts while armed.)
        let auto_approve = self.config.auto_approve_confirms && self.driver.is_none();
        // Stamp this turn's worker launches before the stream can start any.
        // A grant made since the last turn (e.g. while the session was
        // being rebuilt) is seen before this turn can revoke it.
        self.observe_memory();
        self.turn_worker_generation = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin_turn();
        let completion = crate::runtime::TurnCompletion::new();
        self.turn_completion = Some(completion.clone());
        self.cancel = Some(ct.clone());
        self.steer_tx = Some(s_tx);
        let event_turn = trigger == TurnTrigger::EventAuto;
        self.owner_steer_budget = event_turn.then(Default::default);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.reset();
        self.subagent_tick = Some(tick);
        let start = TurnStart {
            cancel: ct,
            steer_rx: s_rx,
            auto_approve,
            completion,
            event_turn,
        };
        if self.session_start_ready() {
            self.open_turn_stream(start).await;
        } else {
            tracing::debug!(session = %self.id, "turn waits for on_session_start");
            self.turn_awaiting_start = Some(start);
            // Quick Start's first turn can wait here for extension discovery
            // (`startup.extensions_ready_timeout_secs`) and then the hook:
            // say so, or the silence looks like a hang.
            self.emit(SessionEventWire::SystemNotice(
                "waiting for extensions to start (on_session_start) before sending...".into(),
            ));
        }
    }

    /// Every turn-end path (Done/Error/Cancel/stream EOF). `streaming=false`
    /// makes a `Cancel` racing a `Done` the idle no-op. Events steered into
    /// the stream that the engine never took (`turn_steered_events`) go back
    /// to `pending_events`, first: they are flushed into history like any
    /// event that arrived during the turn (the interrupted path takes them
    /// earlier, to place them after the marker). Queues the save of the
    /// turn's final history, then the draft removal (`close_turn_draft`).
    pub(crate) fn clear_stream(&mut self) {
        // Child events still held for a `tool_use` go out with the turn.
        self.drain_child_events();
        for event in self.child_order.turn_ended() {
            self.emit(SessionEventWire::ChildProcess(event));
        }
        self.stream = None;
        self.turn_awaiting_start = None;
        self.cancel = None;
        self.steer_tx = None;
        self.owner_steer_budget = None;
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
        self.fork_turn_ended();
        // A client tool stamp whose result never reached the history that
        // ends the turn (a cancel, a failed round) goes with it.
        let conv = &mut *self.conv;
        conv.session.retain_client_tool_stamps(&conv.api_messages);
        // The history that ends the turn — whether or not a draft is open
        // (no draft without the session lock) — then the draft removal.
        self.request_save();
        self.close_turn_draft();
        self.update_attach_state();
        self.publish_live(false);
    }

    // ── in-flight turn draft (crash recovery) ──────────────────────────

    /// Turn start: the draft's existence is the "turn open" signal a
    /// loader uses to detect a process that died mid-turn. Only the session
    /// lock holder writes one: without the lock another process may own
    /// this session, and a draft is a claim that a turn is running HERE.
    fn open_turn_draft(&mut self) {
        let persist =
            self.config.persist && self.conv.is_live() && self.session_lock.is_some();
        self.turn_draft = TurnDraftState {
            open: persist.then(|| self.conv.session.id.clone()),
            base_len: self.conv.api_messages.len(),
            text: String::new(),
            tools: Vec::new(),
            dirty: true,
        };
        self.flush_turn_draft();
    }

    fn turn_draft_text(&mut self, delta: &str) {
        let d = &mut self.turn_draft;
        if d.text.len() < agent_core::core::session_draft::TURN_DRAFT_MAX_TEXT_BYTES {
            d.text.push_str(delta);
            d.dirty = true;
        }
    }

    fn turn_draft_reset_text(&mut self) {
        let d = &mut self.turn_draft;
        d.tools.clear();
        if !d.text.is_empty() {
            d.text.clear();
            d.dirty = true;
        }
    }

    /// A round checkpoint was adopted and saved: later text continues from it.
    fn turn_draft_committed(&mut self) {
        self.turn_draft.base_len = self.conv.api_messages.len();
        self.turn_draft.text.clear();
        self.turn_draft.tools.clear();
        self.turn_draft.dirty = true;
        self.flush_turn_draft();
    }

    /// A context rollover replaced the history head, and `base_len` no
    /// longer refers to it. Until the engine's next round checkpoint
    /// re-bases the draft, nothing streamed can be placed: an impossible
    /// length makes the cancel fallback (`keep_streamed_partial`) and crash
    /// recovery (`recover_crashed_turn`) both skip the text.
    fn turn_draft_rolled_over(&mut self) {
        self.turn_draft.base_len = usize::MAX;
        self.turn_draft.text.clear();
        self.turn_draft.tools.clear();
        self.turn_draft.dirty = true;
        self.flush_turn_draft();
    }

    /// Follow the in-flight response for the draft: its text, a new
    /// response or retry starting over, the tools it calls. Shared by the
    /// stream handler and the cancel drain, so text the actor only receives
    /// while draining (already shown to clients) is tracked too.
    fn observe_turn_draft(&mut self, event: &crate::LlmEvent) {
        match event {
            crate::LlmEvent::Text(text) => self.turn_draft_text(text),
            crate::LlmEvent::ResponseStart | crate::LlmEvent::ResponseReset => {
                self.turn_draft_reset_text()
            }
            crate::LlmEvent::ToolUse { tool_name, .. } => {
                self.turn_draft.tools.push(tool_name.clone())
            }
            // A client tool's stamp joins the conversation's side table (the
            // history it belongs to arrives with the round; reads filter by
            // it, so a stamp whose result never lands is never shown).
            crate::LlmEvent::ToolResult { tool_id, client_tool: Some(stamp), .. } => {
                self.conv.session.client_tool_stamps.insert(tool_id.clone(), (**stamp).clone());
            }
            _ => {}
        }
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
                writer: super::live::process_key().map(str::to_string),
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

        // (S1) join_next items and texts go in front, as the first block.
        let join = self.held_join();
        if attachments.is_empty() && join.is_none() {
            // Text-only: existing path.
            self.conv.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": api_content}),
            ));
        } else if let Some(join) = join.as_ref().filter(|_| attachments.is_empty()) {
            let content = super::actor_held::content_with_join(&join.block, &api_content, Vec::new());
            self.conv.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": content}),
            ));
        } else {
            // Build multipart content: text block (if non-empty) + attachment blocks.
            let blocks = match join.as_ref() {
                Some(join) => super::actor_held::content_with_join(&join.block, &api_content, attachments),
                None => {
                    let mut blocks = Vec::with_capacity(attachments.len() + 1);
                    if !api_content.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": api_content}));
                    }
                    blocks.extend(attachments);
                    serde_json::Value::Array(blocks)
                }
            };
            let candidate = std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": blocks}),
            );
            // Validate the complete history (including this message) for the
            // session's current model before accepting.
            let mut proposed = self.conv.api_messages.clone();
            proposed.push(candidate.clone());
            let model = self.runtime.model().to_string();
            if let Err(e) = crate::runtime::attachments::validate_history_for_request(&model, &proposed) {
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
        if let Some(join) = join {
            self.held_join_committed(join);
        }
        self.start_turn(TurnTrigger::User, Some(user_text)).await;
    }

    /// dispatch.rs StreamingInput plain-text branch (:1369-1378).
    ///
    /// Only the input owner steers (`is_input_command`). In an event-started
    /// turn, the owner's message also moves the turn onto the session budget
    /// (`self.runtime.turn_budget()`, which a `/budget` changes) once the
    /// stream delivers it. A steer that misses the turn (its stream ended)
    /// stays queued and starts a typed turn, on that budget anyway.
    pub(crate) fn steer(&mut self, text: String) {
        let send = |tx: &mpsc::UnboundedSender<String>| tx.send(text.clone()).is_ok();
        let delivered = match (self.steer_tx.as_ref(), self.owner_steer_budget.as_deref()) {
            (Some(tx), Some(lift)) => {
                lift.send_owner_steer(self.runtime.turn_budget().clone(), || send(tx))
            }
            (Some(tx), None) => send(tx),
            (None, _) => false,
        };
        self.emit(SessionEventWire::Steered {
            text: text.clone(),
            delivered,
        });
        self.conv.queued_message = Some(text);
    }

    /// `SubagentSteer`: hand the user's text to one of this session's running
    /// workers, then note it in the MAIN conversation so the main agent knows
    /// the user redirected its worker. The note travels like a drained event
    /// (steered into a live turn, buffered while busy, injected while idle)
    /// but never starts a turn on its own; clients render it from `External`.
    pub(crate) fn steer_subagent(&mut self, from: Option<ClientId>, handle_id: &str, text: String) {
        let steered = {
            let registry = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match registry.get(handle_id) {
                None => Err(format!("no subagent {handle_id} in this session")),
                Some(h) => {
                    let running = h
                        .state
                        .read()
                        .unwrap_or_else(|p| p.into_inner())
                        .public_status()
                        == crate::runtime::subagent::SubagentStatus::Running;
                    if running {
                        h.steer(&text).map(|()| h.agent_name.clone())
                    } else {
                        Err(format!("{} ({handle_id}) is not running", h.agent_name))
                    }
                }
            }
        };
        let agent_name = match steered {
            Ok(name) => name,
            Err(reason) => {
                match from {
                    Some(c) => self.emit(SessionEventWire::Refused {
                        client: c,
                        command: "subagent_steer".to_string(),
                        reason,
                    }),
                    None => self.emit(SessionEventWire::SystemNotice(reason)),
                }
                return;
            }
        };
        self.note_in_main_conversation(subagent_steer_event(handle_id, &agent_name, &text));
    }

    /// Tell the MAIN agent something the user did to its workers: the note
    /// travels like a drained event (steered into a live turn, buffered while
    /// busy, injected while idle) but never starts a turn on its own; clients
    /// render it from `External`.
    pub(crate) fn note_in_main_conversation(&mut self, event: crate::events::types::Event) {
        // GATE (S1): while hold the note is held like any event.
        if self.hold.is_hold() {
            self.hold_event(event, false);
            return;
        }
        let formatted = crate::events::format_event_for_agent(&event);
        if self.busy() {
            let into_turn = self
                .steer_tx
                .as_ref()
                .is_some_and(|tx| tx.send(formatted.clone()).is_ok());
            if into_turn {
                self.turn_steered_events.push(formatted);
            } else {
                self.conv.pending_events.push(formatted);
            }
        } else {
            self.conv
                .api_messages
                .push(std::sync::Arc::new(serde_json::json!({
                    "role": "user",
                    "content": formatted
                })));
            self.request_save();
        }
        self.emit(SessionEventWire::External(event));
        self.emit_conversation();
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
        // The dropped client tool call's `tool_cancel` before the `Idle`.
        self.flush_client_tool_frames();
        match drain.history {
            Some(history) => self.conv.api_messages = history,
            None => {
                if !drain.closed {
                    tracing::warn!(
                        session = %self.id,
                        budget_ms = budgets::CANCEL_DRAIN_TIMEOUT_MS,
                        "cancelled turn did not publish its history within the drain budget; \
                         keeping the last adopted history plus the streamed partial reply"
                    );
                }
                self.keep_streamed_partial(reason);
            }
        }
        // Complete only if the drain consumed the whole stream: the final
        // history is published right after the completion is recorded.
        let completed = drain.closed && self.turn_completed();
        self.finish_interrupted_turn(reason, completed).await;
    }

    /// An interrupted turn whose engine never published its post-cancel
    /// history (the drain budget ran out, or the stream ended without it):
    /// keep the reply text clients were already shown, as a real assistant
    /// message, so history matches the screen and the marker that follows
    /// doesn't merge into the next prompt. Same rule as crash recovery
    /// (`engine::interrupt::recover_crashed_turn`): only when the history is
    /// still exactly where that text began (`base_len`), never a stale one,
    /// and right after a user message. Text only, so it can't leave a
    /// `tool_use` without its result: an in-flight tool call and unsigned
    /// thinking are dropped. (Rebuilding the call isn't safe: with extended
    /// thinking a `tool_use` turn needs its signed thinking block.) A tool
    /// the response called instead gets a host-authored note after it
    /// (`engine::interrupt::tool_in_flight_note_message`), worded like the
    /// engine's canceled `tool_result`, so the next turn doesn't blindly
    /// repeat a non-idempotent action; clients get the same note. The
    /// engine's late publication is not applied: its stream is gone with the
    /// drain.
    ///
    /// Also a round that fails inside a checkpoint's quiesce grace (the
    /// `SessionEvent::Error` arm of `on_stream_event`): the checkpoint would
    /// have cancelled it, so it keeps the same text.
    fn keep_streamed_partial(&mut self, reason: crate::engine::interrupt::InterruptReason) {
        let d = &self.turn_draft;
        let follows_user = self.conv.api_messages.last().is_some_and(|m| m["role"] == "user");
        if d.base_len != self.conv.api_messages.len() || !follows_user {
            return;
        }
        let (text, tools) = (d.text.clone(), d.tools.clone());
        if !text.trim().is_empty() {
            self.conv.api_messages.push(std::sync::Arc::new(serde_json::json!({
                "role": "assistant",
                "content": [{"type": "text", "text": text}]
            })));
        }
        if !tools.is_empty() {
            let note = crate::engine::interrupt::tool_in_flight_note_message(reason, &tools);
            let text = note["content"].as_str().unwrap_or_default().to_owned();
            self.conv.api_messages.push(note);
            self.emit(SessionEventWire::SystemNotice(text));
        }
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
        // A released digest among them is in history; no turn follows a cancel.
        self.hold.digest_buffered = false;
        // Queues the save of this history, then the draft removal.
        self.clear_stream();
        // Cancel running reactive subagents; recover a poisoned guard rather
        // than skip cancellation. A user interrupt (Esc) stops the turn it
        // interrupted and the workers THAT turn launched; reactive workers
        // started by earlier turns are background work and keep running
        // (`/workers cancel` stops them). Every other reason — session
        // teardown, cost cap, host checkpoint, driver revocation — stops all.
        {
            use crate::runtime::subagent::WorkerCancelScope;
            let ending = self.lifecycle.load(std::sync::atomic::Ordering::Acquire)
                == SessionLifecycle::Ending as u8;
            let scope = worker_cancel_scope(reason, ending, self.turn_worker_generation);
            let mut registry = match self.runtime.subagent_registry().lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    tracing::warn!(
                        "subagent registry mutex poisoned during abort; recovering to cancel running handles"
                    );
                    poisoned.into_inner()
                }
            };
            let cancelled = registry.cancel_running(scope);
            if !cancelled.is_empty() {
                tracing::info!(
                    ?scope,
                    workers = ?cancelled,
                    "turn interrupted: cancelled running workers"
                );
            }
            if let WorkerCancelScope::Turn(_) = scope {
                // Still running and staying so: not the ones just cancelled
                // (they read as running until their next step).
                let spared = registry
                    .display_rows()
                    .iter()
                    .filter(|r| {
                        r.status == crate::runtime::subagent::SubagentStatus::Running
                            && !r.cancel_requested
                    })
                    .count();
                drop(registry);
                if spared > 0 {
                    self.emit(SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::Notice(format!(
                            "{spared} background worker{} from earlier turns still running — /workers to list, /workers cancel all to stop",
                            if spared == 1 { "" } else { "s" }
                        )),
                    )));
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
        if !completed {
            self.keep_streamed_partial(crate::engine::interrupt::InterruptReason::Driver);
        }
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
            // Host-internal ledger events, as in `on_stream_event`.
            let Some(event) = event.for_clients() else {
                continue;
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
                StreamEvent::Llm(ref llm) => {
                    self.observe_turn_draft(llm);
                    self.emit(SessionEventWire::Stream(event));
                }
                other => self.emit(SessionEventWire::Stream(other)),
            }
        }
        drop(stream);
        out
    }

    /// A child report from this session's route (see `EventOrder`).
    fn on_child_event(&mut self, event: super::child_events::ChildProcessEvent) {
        if let Some(event) = self.child_order.offer(event, self.streaming) {
            self.emit(SessionEventWire::ChildProcess(event));
        }
    }

    fn drain_child_events(&mut self) {
        while let Ok(event) = self.child_rx.try_recv() {
            self.on_child_event(event);
        }
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
            self.cache_keepwarm_note_usage(crate::runtime::keepwarm::KeepWarmUsage {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cache_read_input_tokens,
                cache_write_tokens: *cache_creation_input_tokens,
                cache_write_5m: *cache_creation_5m,
                cache_write_1h: *cache_creation_1h,
                model: usage_model.clone(),
            });
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
        if self.holds(WorkSource::QueueWake) {
            // Fenced for a reload: the record carries what is queued.
            return;
        }
        // GATE (S1): while hold, the events go to the held store, before
        // the drain and before any unpark (a parked session stays parked).
        if self.hold.is_hold() {
            self.hold_queue_wake();
            return;
        }
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
                } else if self.streaming || self.stream.is_some() {
                    tracing::warn!("handle_event_arm: RunTurn with active turn — skipping");
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
        // Child reports sent before this event (a call's spawn precedes its
        // output) go out first.
        self.drain_child_events();
        // A run's request-ledger events are host-internal: a ledgered
        // attempt's usage is handled (booked, cost cap, forwarded) as the
        // plain `Usage` it stands in for; a liability is dropped. A session
        // runtime keeps no request ledger, so neither is expected here.
        let Some(event) = event.for_clients() else {
            return;
        };
        // A turn cancelled from outside `cancel_turn` (a driver revocation):
        // its terminal events are not a normal end — the turn is finished as
        // interrupted, never through the post-turn machinery below.
        if self.turn_cancelled() {
            match event {
                StreamEvent::Session(SessionEvent::Done) => {
                    on_heap(|| self.finish_revoked_turn(true)).await;
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
        if let StreamEvent::Llm(
            crate::LlmEvent::ToolUseStart { tool_id, .. } | crate::LlmEvent::ToolUse { tool_id, .. },
        ) = &event
        {
            for child in self.child_order.announced(tool_id) {
                self.emit(SessionEventWire::ChildProcess(child));
            }
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
            /// Ended inside a checkpoint's quiesce grace: idle, no new turn.
            Held,
        }
        let mut after = After::Continue;
        // (S1) A released digest rode this turn's end.
        let mut digest_rides = false;
        // A reload refused while this turn streamed, once the turn stopped
        // for it (`ReloadRefused`): resumed at the end of this function.
        let mut resume_refused = None;

        match event {
            // Text, a new provider response (or a retry's reset: the
            // in-flight text starts over), tool calls.
            StreamEvent::Llm(ref llm) => self.observe_turn_draft(llm),
            StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                // Published at every round boundary as well as at the end of
                // the turn (`runtime/stream.rs` ROUND CHECKPOINT): the session
                // on disk follows the turn as it progresses. Queued, never
                // awaited — the persister writes it (latest wins), so a slow
                // disk can never stall the turn machine.
                //
                // Inside a checkpoint's grace, a history no longer than the
                // draft's base predates the in-flight response (the engine's
                // provider-error arm publishes the round's input): the draft
                // keeps the text clients saw for the failure that follows.
                let predates_response =
                    self.checkpoint_grace.is_some() && history.len() == self.turn_draft.base_len;
                self.conv.api_messages = history;
                self.request_save();
                // The round is committed: the draft now continues from here.
                if !predates_response {
                    self.turn_draft_committed();
                }
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
                    on_heap(|| {
                        self.cancel_turn(crate::engine::interrupt::InterruptReason::CostCap)
                    })
                    .await;
                    return;
                }
            }
            StreamEvent::Session(SessionEvent::Notice(_) | SessionEvent::Retry(_)) => {}
            // Normalized at the top of this function.
            StreamEvent::Session(
                SessionEvent::UsageAttempt { .. } | SessionEvent::UsageLiability { .. },
            ) => {}
            StreamEvent::Session(SessionEvent::Done) => {
                // A reload's drain stopped this turn at a round boundary
                // (the quiesce stop in `runtime/stream.rs`): no reply ends
                // it. Mark it the way the checkpoint's cancel marks a cut
                // turn (before the buffered events, as `cancel_turn` puts
                // them), so the next image resumes it (`ResumeAfterReload`).
                // A turn whose last round was a reply is complete. A reload
                // refused after the stream stopped for it, but before this
                // end, lifted the quiesce already: the stream's own stop
                // says so, and the turn resumes below (`ReloadRefused`).
                let stopped_for_reload = self.event_queue.take_quiesce_stop()
                    == Some(crate::events::queue::Quiesce::Reload);
                resume_refused = self
                    .reload_refused_pending
                    .take()
                    .filter(|_| stopped_for_reload);
                if (self.event_queue.quiesced() == Some(crate::events::queue::Quiesce::Reload)
                    || resume_refused.is_some())
                    && self
                        .conv
                        .api_messages
                        .last()
                        .is_some_and(|m| m["role"] != "assistant")
                {
                    crate::engine::interrupt::append_marker(
                        &mut self.conv.api_messages,
                        crate::engine::interrupt::InterruptReason::Restart,
                    );
                    self.reload_quiesced_turn = true;
                    self.reload_quiesced_tail = None;
                } else if self
                    .conv
                    .api_messages
                    .last()
                    .is_some_and(|m| m["role"] == "assistant")
                {
                    // The turn ended with its reply: a crash or stop
                    // restore's resume (if one ran) is answered, so a later
                    // death that cuts an unrelated turn is not taken for a
                    // crash loop (`crash_record::crash_loop_suspected`).
                    // `clear_stream` below publishes the entry.
                    self.restore_resumed_at = None;
                }
                self.clear_stream();
                self.publish_subagent_rows();
                // Flush events that arrived during streaming into api_messages
                let had_pending = !self.conv.pending_events.is_empty();
                digest_rides = std::mem::take(&mut self.hold.digest_buffered);
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
                // The paused turn's own tail is in: only what comes after it
                // re-prompts the turn (`ReloadRefused`).
                if self.reload_quiesced_turn {
                    self.reload_quiesced_tail =
                        crate::engine::interrupt::after_latest_marker(&self.conv.api_messages);
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
                        on_heap(|| self.post_turn_chat()).await;
                    }
                    // A spawned compaction emits Idle when it lands.
                    if self.compact.is_none() {
                        on_heap(|| self.announce_idle()).await;
                    }
                }
            }
            StreamEvent::Session(SessionEvent::Error(_)) => {
                // Before `clear_stream` drops the completion it was noted on.
                let capacity_stop = self.take_capacity_stop();
                // Remove only invalid messages appended by the ACTIVE turn —
                // before `clear_stream` queues the turn's final save.
                crate::engine::stream::repair_history_after_failure(
                    &mut self.conv.api_messages,
                    self.turn_baseline,
                );
                let mut cut = false;
                // Inside a checkpoint's quiesce grace the checkpoint would
                // have cancelled this turn and kept what clients were shown:
                // keep it here too, as the cancel path does
                // (`keep_streamed_partial`, then the checkpoint's marker).
                if let Some(reason) = self.checkpoint_grace {
                    let before = self.conv.api_messages.len();
                    self.keep_streamed_partial(reason);
                    if self.conv.api_messages.len() > before {
                        crate::engine::interrupt::append_marker(
                            &mut self.conv.api_messages,
                            reason,
                        );
                        cut = true;
                    }
                }
                // A reset-based capacity stop arms its resume, which the
                // entry `clear_stream` publishes carries.
                let armed = capacity_stop.and_then(|noted| self.capacity_arm(noted, cut));
                self.clear_stream();
                self.publish_subagent_rows();
                self.emit_conversation();
                if let Some(notice) = armed {
                    self.emit(SessionEventWire::SystemNotice(notice));
                }
                after = After::Failed;
            }
            StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                session_id,
                messages,
                receipt,
            }) => {
                on_heap(|| self.handle_context_head_checkpoint(session_id, messages, receipt))
                    .await;
            }
        }

        // A turn that ends inside a checkpoint's quiesce grace
        // starts no turn of its own, as if the checkpoint had cancelled it:
        // a queued message goes back to its sender (as `cancel_turn` does),
        // and buffered events stay in history.
        if self.checkpoint_grace.is_some() {
            after = match after {
                After::AutoSendQueued(text) => {
                    self.emit(SessionEventWire::Dequeued { text });
                    After::Held
                }
                After::AutoTriggerEvents => After::Held,
                other => other,
            };
        }
        // GATE (S1): while hold, events buffered during the turn stay in
        // history but start no turn, unless a released digest rode them.
        if self.hold.is_hold() && !digest_rides && matches!(after, After::AutoTriggerEvents) {
            after = After::Held;
        }

        // Only a turn that ends with nothing to send next resumes; anything
        // that starts a turn here answers the paused one itself.
        let resume_refused = resume_refused.filter(|_| matches!(after, After::Continue));
        match after {
            After::Continue => {}
            After::Held => on_heap(|| self.announce_idle()).await,
            After::Failed => {
                if self.config.auto_compact {
                    on_heap(|| self.post_turn_chat()).await;
                }
                if self.compact.is_none() {
                    on_heap(|| self.announce_idle()).await;
                }
            }
            After::AutoSendQueued(queued) => {
                if self.config.auto_compact {
                    on_heap(|| self.post_turn_chat()).await;
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
                let content = self.queued_user_content(queued);
                self.conv.api_messages.push(std::sync::Arc::new(
                    serde_json::json!({"role": "user", "content": content}),
                ));
                on_heap(|| self.start_turn(TurnTrigger::QueuedAuto, Some(user_text))).await;
            }
            After::AutoTriggerEvents => {
                if self.config.auto_compact {
                    on_heap(|| self.post_turn_chat()).await;
                }
                if self.compact.is_some() {
                    return;
                }
                // Central claim gate: allows turns while under the configured
                // cap (events.auto_turn_cap; 0 = unlimited), denies once reached.
                let auto_turn_cap = self.host.config().events.auto_turn_cap;
                if claim_auto_turn_with_cap(&mut self.consecutive_auto_turns, auto_turn_cap) {
                    on_heap(|| self.start_turn(TurnTrigger::EventAuto, None)).await;
                } else {
                    self.emit(SessionEventWire::AutoTurnCapReached { cap: auto_turn_cap });
                    on_heap(|| self.announce_idle()).await;
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

        // (spec §5) The reload was refused after this turn's stream stopped
        // for it: the same resume in place as `reload_refused`'s.
        if let Some(refused_at) = resume_refused {
            if self.refused_turn_paused() {
                on_heap(|| self.resume_refused_turn(refused_at)).await;
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
        // Compaction replaces the history: the old cached prefix is dead.
        self.cache_keepwarm_compaction();
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
        // Memory-only results never reach the summary request (the clone
        // below has a fresh store) and do not survive compaction.
        self.memory_only.clear();
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
                            // The keep-warm subtotal follows the totals it
                            // is part of: input/output/cost restart, the
                            // cache totals (not reset here) keep their
                            // keep-warm share.
                            self.conv.keepwarm_cost = 0.0;
                            self.conv.keepwarm_tokens.input = 0;
                            self.conv.keepwarm_tokens.output = 0;
                            // The child share likewise.
                            self.conv.child_cost = 0.0;
                            self.conv.child_tokens.input = 0;
                            self.conv.child_tokens.output = 0;
                        }
                        let new_id = self.conv.session.id.clone();
                        self.runtime.set_session_id(Some(new_id.clone()));
                        self.set_journal_id(new_id.clone());
                        // F10: lock follows the new journal id.
                        self.reacquire_session_lock(&new_id);
                        // Wall 1: reset continuation state for the new session id
                        // so stale epoch checks in persist_head() don't reject
                        // future checkpoints.
                        self.conv.context_head = crate::engine::session::ContextHeadPersistence::default();
                        self.runtime
                            .reset_context_continuation(&new_id, &self.conv.api_messages);
                        self.capacity_refingerprint();
                        self.ephemeral_layer.invalidate(crate::runtime::ephemeral::Reason::Rollover);
                        self.memory_only.clear();
                        let rolled_over = previous != new_id;
                        self.emit(SessionEventWire::CompactionApplied {
                            previous_session_id: previous,
                            session_id: new_id,
                            chains_advanced: applied.chains_advanced,
                            queued_restored: queued,
                            msg_count,
                        });
                        // Client tools: the registration survives a
                        // compaction; its installer hears the new id.
                        if rolled_over {
                            self.client_tools_rolled_over();
                        }
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
        self.compacting_inline = true;
        self.publish_presence();
        self.compact_inline_inner(instructions, source).await;
        self.compacting_inline = false;
        self.publish_presence();
    }

    async fn compact_inline_inner(&mut self, instructions: Option<String>, source: &str) {
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
                self.capacity_refingerprint();
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
            confirm_scope: req.confirm_scope,
        };
        self.pending_prompts.push_back((pr.clone(), req.response_tx));
        self.publish_presence();
        self.emit(SessionEventWire::Prompt(pr));
        // A prompt can be raised while already detached (a turn running with no
        // client). Arm the abandonment deadline if this left us pinned.
        self.rearm_prompt_abandon();
    }

    /// Why `from` may not give this answer to `prompt`, if it may not.
    /// Confirm prompts gate tool execution, so an `Observe` client cannot
    /// answer them at all, and only the input owner (or the host, `from:
    /// None`) can choose "Allow all", which arms a session-wide latch.
    fn confirm_answer_refusal(
        &self,
        from: Option<ClientId>,
        prompt: &PromptRequest,
        allow_all: bool,
    ) -> Option<&'static str> {
        let c = from?;
        if prompt.kind != PromptKind::Confirm {
            return None;
        }
        if self.attached.get(&c).map(|a| a.mode) == Some(AttachMode::Observe) {
            return Some("observe-mode clients cannot answer confirm prompts");
        }
        if allow_all && self.input_owner != Some(c) {
            return Some("only the input owner can choose Allow all");
        }
        None
    }

    fn answer(&mut self, from: Option<ClientId>, prompt_id: u64, value: Option<String>) {
        let Some(pos) = self.pending_prompts.iter().position(|(p, _)| p.id == prompt_id) else {
            return; // unknown or already answered — dedup on id
        };
        let allow_all = value.as_deref().is_some_and(|v| {
            v.trim()
                .eq_ignore_ascii_case(crate::tools::CONFIRM_ANSWER_ALLOW_ALL)
        });
        // An answer is input too: with `client_input = shared` the answering
        // drive client becomes active first, so "Allow all" (owner only)
        // is its to choose.
        if let Some(c) = from {
            self.note_input(c);
        }
        if let Some(reason) = self.confirm_answer_refusal(from, &self.pending_prompts[pos].0, allow_all) {
            if let Some(c) = from {
                self.emit(SessionEventWire::Refused {
                    client: c,
                    command: "answer".to_string(),
                    reason: reason.to_string(),
                });
            }
            return; // the prompt stays pending for an eligible client
        }
        let approved = allow_all
            || value
                .as_deref()
                .map(str::trim)
                .is_some_and(|v| v.eq_ignore_ascii_case("y") || v.eq_ignore_ascii_case("yes"));
        let (prompt, tx) = self.pending_prompts.remove(pos).expect("position");
        let _ = tx.send(value);
        self.publish_presence();
        self.emit(SessionEventWire::PromptResolved { prompt_id });
        let by = match from {
            Some(c) => self.client_label(c),
            None => "the host".to_string(),
        };
        // prA N3: the tool card shows the model's original input; when the
        // approved call runs input rewritten by an extension, every client's
        // transcript records that (the prompt showed the final input).
        if approved && prompt.title == CONFIRM_PROMPT_TITLE {
            if let Some(scope) = prompt.confirm_scope.as_ref().filter(|s| !s.rewritten_by.is_empty()) {
                self.emit(SessionEventWire::SystemNotice(format!(
                    "Approved by {by}: {} runs with input rewritten by {} (not the input shown \
                     on its tool card; the approval prompt showed what runs).",
                    scope.tool.as_deref().unwrap_or("the tool call"),
                    scope.rewritten_by.join(", ")
                )));
            }
        }
        // Arming the latch is announced to every attached client, not just
        // the one that pressed the key.
        if allow_all && prompt.title == CONFIRM_PROMPT_TITLE {
            if let Some(scope) = prompt.confirm_scope.as_ref().filter(|s| !s.extensions.is_empty()) {
                self.emit(SessionEventWire::SystemNotice(format!(
                    "Allow all armed by {by}: tool-call confirms from {} are auto-approved \
                     until /new. Other extensions still ask. /confirms reset revokes.",
                    scope.extensions_label()
                )));
            }
        }
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
            SessionSetting::MemoryOnlyTools { .. } => "memory_only_tools",
        };
        // Capability evidence must exist in THIS (possibly daemon) process
        // before the mutation is validated and a reasoning clamp decided.
        match &setting {
            SessionSetting::Model { model } => self.runtime.prepare_model_capabilities(model).await,
            SessionSetting::ReasoningLevel { .. } => {
                let model = self.runtime.model().to_string();
                self.runtime.prepare_model_capabilities(&model).await;
            }
            _ => {}
        }
        let rt = &mut self.runtime;
        let mut clamp_wire = None;
        let mut memory_only_echo = None;
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
            // Add-only: the list must keep every glob the session already
            // has (a superset adds, an identical resend is a no-op); one that
            // drops any is refused, counted, and changes nothing. In the
            // config, so the reload/crash record, a fork and an unpark carry
            // it. The shared list is read at each mark check, so the turn in
            // flight and running workers see an addition. The store is
            // untouched: a result already held stays memory-only.
            SessionSetting::MemoryOnlyTools { globs } => {
                let outcome = match self.memory_only_globs.add(&globs) {
                    Ok(_) => {
                        self.config.memory_only_tools = self.memory_only_globs.globs();
                        Ok(None)
                    }
                    Err(dropped) => Err(format!(
                        "memory-only tools are add-only: this list drops {}; nothing changed. Removing a glob takes a new session.",
                        dropped.iter().map(|g| format!("`{g}`")).collect::<Vec<_>>().join(", ")
                    )),
                };
                memory_only_echo = Some(rt.memory_only_globs());
                outcome
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
            memory_only_tools: memory_only_echo,
        }));
    }

    fn ephemeral_status(&self, outcome: Option<crate::runtime::ephemeral::Outcome>) -> serde_json::Value {
        let mut value = serde_json::to_value(self.ephemeral_layer.status()).unwrap_or_default();
        value["kind"] = serde_json::json!("ephemeral_layer");
        value["session_id"] = serde_json::json!(self.id.as_str());
        if let Some(outcome) = outcome { value["outcome"] = serde_json::json!(outcome); }
        value
    }

    /// `query: memory_only` — counts only, never result text.
    fn memory_only_status(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self.memory_only.status()).unwrap_or_default();
        value["kind"] = serde_json::json!("memory_only");
        value["session_id"] = serde_json::json!(self.id.as_str());
        // Live Sets refused for dropping a glob (add-only).
        value["set_refused"] = serde_json::json!(self.memory_only_globs.refused());
        value
    }

    pub(crate) async fn query(&mut self, id: u64, query: SessionQuery) {
        if matches!(query, SessionQuery::EphemeralLayer) {
            self.emit(SessionEventWire::QueryResult {
                id,
                value: self.ephemeral_status(None),
                client_tools: None,
            });
            return;
        }
        // Answered parked or live: the store is the actor's.
        if matches!(query, SessionQuery::MemoryOnly) {
            self.emit(SessionEventWire::QueryResult {
                id,
                value: self.memory_only_status(),
                client_tools: None,
            });
            return;
        }
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
                    "pinned": self.keep_warm,
                    "forked_from": self.fork.parked_from,
                }),
                SessionQuery::View => serde_json::to_value(&**self.view.load()).unwrap_or_default(),
                // A parked session has no live workers; a watch poll must not wake it.
                SessionQuery::SubagentWatch { handle_id, .. } => {
                    serde_json::json!({ "error": format!("no subagent {handle_id} in this session") })
                }
                // The held store is the actor's, not the runtime's.
                SessionQuery::HeldList | SessionQuery::HeldShow { .. } => self.held_query(query),
                _ => match self.ensure_live().await {
                    Ok(()) => return Box::pin(self.query(id, query)).await,
                    Err(e) => serde_json::json!({ "error": e }),
                },
            };
            self.emit(SessionEventWire::QueryResult { id, value, client_tools: None });
            return;
        }
        // The `Messages` answer carries the client tool stamps of that
        // history (absent when it holds none).
        let client_tools = matches!(query, SessionQuery::Messages)
            .then(|| self.conv.session.client_tool_stamps_of(&self.conv.api_messages))
            .filter(|stamps| !stamps.is_empty());
        let value = match query {
            SessionQuery::Status => {
                // Configured + resolved prompt-cache TTL (`auto` → 1h on OAuth).
                let cache_ttl = self.cache_ttl_status_json().await;
                self.with_spend_status(serde_json::json!({
                    "session": self.conv.session.id,
                    "cache_ttl": cache_ttl,
                    "model": self.runtime.model(),
                    "lifecycle": SessionLifecycle::from_u8(self.lifecycle.load(std::sync::atomic::Ordering::Acquire)),
                    "tokens": { "input": self.conv.total_input_tokens, "output": self.conv.total_output_tokens },
                    "cost": self.conv.session_cost,
                    // Included in `cost`; labelled separately (task #444).
                    "keepwarm_cost": self.conv.keepwarm_cost,
                    "keepwarm": self.cache_keepwarm_status_json(None),
                    // Included in `cost` and `tokens`; labelled separately
                    // (child spend).
                    "child_cost": self.conv.child_cost,
                    "child_tokens": {
                        "input": self.conv.child_tokens.input,
                        "output": self.conv.child_tokens.output,
                        "cache_read": self.conv.child_tokens.cache_read,
                        "cache_creation": self.conv.child_tokens.cache_creation,
                    },
                    "child_share_exact": self.conv.child_share_exact,
                    "messages": self.conv.api_messages.len(),
                    "streaming": self.streaming,
                    "auto_turns": self.consecutive_auto_turns,
                    "attached": self.attached.len(),
                    "pending_prompts": self.pending_prompts.len(),
                    // `/pin` status in the TUI.
                    "pinned": self.keep_warm,
                    "forked_from": self.conv.session.forked_from,
                }))
            }
            SessionQuery::EphemeralLayer => self.ephemeral_status(None),
            SessionQuery::MemoryOnly => self.memory_only_status(),
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
            SessionQuery::SubagentWatch { handle_id, since, tool_queue } => {
                let mut registry = self
                    .runtime
                    .subagent_registry()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                match registry.watch_page(&handle_id, since) {
                    // An older client did not ask for queue entries and
                    // could not decode a page that had them.
                    Some(page) if !tool_queue => {
                        serde_json::to_value(page.without_tool_queue()).unwrap_or_default()
                    }
                    Some(page) => serde_json::to_value(page).unwrap_or_default(),
                    None => serde_json::json!({
                        "error": format!("no subagent {handle_id} in this session")
                    }),
                }
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
            SessionQuery::HeldList | SessionQuery::HeldShow { .. } => self.held_query(query),
        };
        self.emit(SessionEventWire::QueryResult { id, value, client_tools });
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
            roster: self.roster(),
            client_input: self.client_input,
            input_owner: self.input_owner,
            display_tail: None,
            client_tools: self.conv.session.client_tool_stamps_of(&self.conv.api_messages),
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
        self.cache_keepwarm_note_attach(kind, mode);
        let snapshot_meta = client.clone();
        let name = resolve_client_name(
            client.name.as_deref(),
            kind,
            self.attached.values().map(|a| a.name.as_str()),
        );
        let attached_at_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        self.attached.insert(
            cid,
            AttachedClient { meta: client, mode, name: name.clone(), attached_at_ms, last_input: 0 },
        );
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
            // Before the new owner holds input: a registration another
            // client installed ends (cancels + `dropped{owner_change}`).
            self.client_tools_owner_changed(Some(cid));
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
        self.emit(SessionEventWire::ClientJoined { client: cid, kind, name, mode });
    }

    /// B1 `/takeover`: the sending client takes input in place, as an
    /// `AttachMode::Takeover` attach would, without re-attaching. An
    /// `Observe` client is refused (it attached read-only on purpose); the
    /// owner, the host and unknown clients change nothing. Any turn keeps
    /// running: only who may drive the session changes.
    fn take_input(&mut self, from: Option<ClientId>) {
        let Some(c) = from else { return };
        let Some(mode) = self.attached.get(&c).map(|a| a.mode) else { return };
        if mode == AttachMode::Observe {
            self.emit(SessionEventWire::Refused {
                client: c,
                command: command_name(&SessionCommand::TakeInput).to_string(),
                reason: "this client attached with --observe and never owns input; \
                         attach without --observe to take it"
                    .to_string(),
            });
            return;
        }
        if self.input_owner == Some(c) {
            return;
        }
        let from = self.input_owner;
        let reason = if from.is_some() {
            OwnerChangeReason::Takeover
        } else {
            OwnerChangeReason::Attach
        };
        self.client_tools_owner_changed(Some(c));
        self.input_owner = Some(c);
        self.emit(SessionEventWire::InputOwnerChanged { from, to: Some(c), reason });
        self.publish_presence();
        self.note_input(c); // most recent, for promotion on detach
    }

    /// Never touches `stream`/`cancel`: the turn keeps running and its
    /// events buffer in the broadcast (§8 detach-without-abort). The owner
    /// leaving passes input to the oldest attached non-`Observe` client.
    fn detach(&mut self, client: ClientId) {
        // The installer leaving ends its calls at once (synaps_detached),
        // with no frame: there is nobody to send one to.
        if self.client_tool_installer == Some(client) {
            self.drop_client_tools(crate::runtime::client_tools::DropReason::Detached);
        }
        if self.ephemeral_installer == Some(client) {
            self.ephemeral_layer.invalidate(crate::runtime::ephemeral::Reason::Disconnect);
            self.ephemeral_installer = None;
        }
        if self.attached.remove(&client).is_none() {
            return;
        }
        self.update_attach_state();
        self.emit(SessionEventWire::ClientLeft { client });
        if self.input_owner == Some(client) {
            // `shared`: back to whoever typed last (a script that sent one
            // prompt and left hands input back to the tab in use);
            // `exclusive`: the lowest id, as before. Ties: the lowest id.
            let shared = self.client_input == ClientInput::Shared;
            let next = self
                .attached
                .iter()
                .filter(|(_, a)| a.mode != AttachMode::Observe)
                .max_by_key(|(c, a)| (if shared { a.last_input } else { 0 }, std::cmp::Reverse(c.0)))
                .map(|(c, _)| *c);
            self.client_tools_owner_changed(next);
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
    pub(crate) async fn checkpoint(
        &mut self,
        reason: CheckpointReason,
        deadline: Option<tokio::time::Instant>,
        skip_quiesce_grace: bool,
    ) {
        // Child spend: a reload stops the session's workers and books what
        // they report before its save, inside the caller's one deadline
        // (the coordinator's; else ours from receipt). No run is admitted
        // meanwhile; admissions reopen after the save, since the session
        // continues here if the exec fails. A host checkpoint leaves the
        // workers running, as before.
        let settle = (reason == CheckpointReason::Reload).then(|| {
            self.spend_sink.begin_closing();
            super::child_spend::spend_deadline(deadline.unwrap_or_else(|| {
                tokio::time::Instant::now() + budgets::CHECKPOINT_BUDGET
            }))
        });
        // The record this checkpoint captures is what the next image's
        // rehydrate hands over; a late restore arm here is not taken.
        self.capacity_close_restore();
        let interrupt = match reason {
            CheckpointReason::Reload => crate::engine::interrupt::InterruptReason::Restart,
            CheckpointReason::HostRequest => crate::engine::interrupt::InterruptReason::Host,
        };
        // Before the revoke below cancels a driver turn.
        self.note_cancel_cause(interrupt);
        // Client tools do not survive the exec: every live call ends with
        // `tool_cancel{reload}` now, before the daemon's `Reloading`, and
        // the record never holds the registration (it lives in RAM only).
        if reason == CheckpointReason::Reload {
            self.drop_client_tools(crate::runtime::client_tools::DropReason::Reload);
        }
        // E-P3: driver does NOT survive reload (§3 S2/S5).
        if self.driver.is_some() {
            self.driver_revoke("daemon reloaded");
        }
        // Workers cannot survive the exec either (they are threads of this
        // process). Flag + cancel them BEFORE `cancel_turn` (whose own
        // cancel would otherwise label them user-cancelled). This also
        // fences new launches: a worker the turn registers before its
        // cancel lands is stopped at birth as the reload's.
        let reload = matches!(reason, CheckpointReason::Reload);
        if reload {
            self.stop_workers_for_reload();
        }
        let was_streaming = self.streaming;
        if self.streaming {
            // Quiesce first, so a running `wait` returns
            // `interrupted` and the turn stops at its next round boundary
            // on its own (no new provider round); the cancel below is the
            // backstop for a turn still running after the grace. A reload's
            // flag is usually set already, by the daemon before its drain.
            self.event_queue.set_quiesce(Some(match reason {
                CheckpointReason::Reload => crate::events::queue::Quiesce::Reload,
                CheckpointReason::HostRequest => crate::events::queue::Quiesce::Checkpoint,
            }));
            // A reload whose drain ran to its cap gave the turn that
            // chance already: no grace on top (events already in hand are
            // still handled).
            let grace = if skip_quiesce_grace { std::time::Duration::ZERO } else { budgets::QUIESCE_GRACE };
            self.let_quiesced_turn_end(interrupt, grace).await;
        }
        if self.streaming {
            self.cancel_turn(interrupt).await;
        }
        // The reload cut a running turn: the next image resumes it
        // (`ResumeAfterReload`). A turn that completed during the cancel
        // left no marker, so there is nothing to resume. A turn the drain
        // already stopped at a round boundary is cut too
        // (`reload_quiesced_turn`).
        let turn_cut = reload
            && (was_streaming || self.reload_quiesced_turn)
            && crate::engine::interrupt::ends_unanswered_after(
                &self.conv.api_messages,
                crate::engine::interrupt::InterruptReason::Restart,
            );
        // Again after the turn cancel: every worker to await, including
        // any the cancelled turn registered meanwhile.
        let stopping = if reload { self.stop_workers_for_reload() } else { Vec::new() };
        let stopped_workers = await_stopped_workers(self.id.as_str(), stopping).await;
        self.abort_compaction();
        while let Some((pr, tx)) = self.pending_prompts.pop_front() {
            let _ = tx.send(None);
            self.emit(SessionEventWire::PromptResolved { prompt_id: pr.id });
        }
        if let Some(spend_deadline) = settle {
            self.settle_child_spend(spend_deadline).await;
        }
        // One bounded wait covers the cancelled turn's save too (queued, not
        // awaited, by `cancel_turn`).
        // The cut turn's own tail is in history now (events flushed after
        // its marker), exactly as this save writes it: the next image counts
        // only what comes after it as a re-prompt
        // (`SessionCommand::ResumeAfterReload::cut_tail`).
        let cut_tail = if turn_cut {
            crate::engine::interrupt::after_latest_marker(&self.conv.api_messages)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        } else {
            None
        };
        if !self.save().await {
            tracing::warn!(session = %self.id, "checkpoint: save failed or timed out");
        }
        if settle.is_some() {
            self.spend_sink.reopen();
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
        if !stopped_workers.is_empty() {
            self.emit(SessionEventWire::SystemNotice(format!(
                "daemon reloading — stopped worker{} {} (archived; resumable after the reload)",
                if stopped_workers.len() == 1 { "" } else { "s" },
                stopped_workers.join(", ")
            )));
        }
        self.runtime.session_manager().shutdown_all();
        let mut record = self.reload_record();
        if reload {
            record.workers = self.reload_carried_workers();
            record.turn_cut = turn_cut;
            record.cut_tail = cut_tail;
            // Recorded: from here to the exec, nothing may start work the
            // record does not carry (a queued wake must stay queued).
            self.reload_fence = Some(ReloadFence {
                deadline: tokio::time::Instant::now() + budgets::RELOAD_FENCE_TIMEOUT,
                workers: record.workers.clone(),
            });
        }
        let record = serde_json::to_value(record).unwrap_or_default();
        // A host checkpoint's quiesce ends with it; a reload's stays until
        // the exec (or its abort lifts the fence).
        if self.event_queue.quiesced() == Some(crate::events::queue::Quiesce::Checkpoint) {
            self.event_queue.set_quiesce(None);
        }
        self.emit(SessionEventWire::QueryResult {
            id: super::wire::CHECKPOINT_QUERY_ID,
            value: serde_json::json!({ "ok": true, "record": record }),
            client_tools: None,
        });
    }

    /// Give a quiesced turn up to `grace` ([`budgets::QUIESCE_GRACE`], or
    /// zero after a full-cap reload drain) to end on its own, pumping its
    /// stream through the normal handler (a running `wait` returns
    /// `interrupted`; the round top then ends the turn). Secret prompts stay pending for the cancel path, which answers
    /// them after the cancel. A round that fails meanwhile keeps the reply
    /// text clients saw, with `interrupt`'s marker, as the cancel would
    /// (the `SessionEvent::Error` arm of `on_stream_event`).
    async fn let_quiesced_turn_end(
        &mut self,
        interrupt: crate::engine::interrupt::InterruptReason,
        grace: std::time::Duration,
    ) {
        let deadline = tokio::time::Instant::now() + grace;
        self.checkpoint_grace = Some(interrupt);
        while self.streaming {
            match tokio::time::timeout_at(deadline, next_stream_event(&mut self.stream)).await {
                Ok(Some(event)) => self.on_stream_event(event).await,
                // The stream ended without its terminal, or the grace ran
                // out: the cancel path takes it from here.
                Ok(None) | Err(_) => break,
            }
        }
        self.checkpoint_grace = None;
        tracing::info!(
            session = %self.id,
            ended = !self.streaming,
            "checkpoint: quiesce grace over"
        );
    }

    /// The workers a reload carries for this session: every registry entry,
    /// plus whether a finished worker's completion wake is still queued
    /// (the exec discards the queue, so the next image re-posts it).
    pub(crate) fn reload_carried_workers(&self) -> Vec<crate::runtime::subagent::CarriedWorker> {
        let undelivered: std::collections::HashSet<String> = self
            .event_queue
            .snapshot()
            .iter()
            .filter(|e| e.content.content_type == "subagent_completion")
            .filter_map(|e| {
                e.content
                    .data
                    .as_ref()?
                    .get("handle_id")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reload_carry(&undelivered)
    }

    /// Host-only (`SessionCommand::RestoreToolActivations`, sent by
    /// reload's rehydrate): the activations this conversation held in the
    /// previous image. A live runtime takes them (restored at its first turn
    /// start); a parked session keeps them for the runtime its unpark
    /// builds, without unparking.
    pub(crate) fn restore_tool_activations(
        &mut self,
        activations: Vec<crate::tools::activation::CarriedActivation>,
    ) {
        if activations.is_empty() {
            return;
        }
        tracing::info!(
            session = %self.id,
            count = activations.len(),
            "reload: tool activations handed over from the previous image"
        );
        if self.runtime.is_live() {
            self.runtime.restore_tool_activations(activations);
        } else {
            for a in activations {
                if !self.parked_tool_activations.iter().any(|p| p.tool_id == a.tool_id) {
                    self.parked_tool_activations.push(a);
                }
            }
        }
    }

    /// Host-only (`SessionCommand::AdoptWorkers`, sent by reload's
    /// rehydrate): the workers the previous daemon image stopped or still
    /// owed a wake. Their ids resolve to their archives again, and the
    /// parent is woken once per worker — it was waiting for completions the
    /// exec destroyed.
    ///
    /// `resume`: after a reload ([`super::WorkerResume::Auto`]) the actor
    /// resumes the workers the reload stopped itself, from their archives,
    /// before it takes another command (the launches are local: archive
    /// read, size estimate, admission; no provider round). The parent then
    /// hears once per worker that the daemon resumed it as a new handle.
    /// A worker it may not resume keeps today's wake, with the reason.
    pub(crate) async fn adopt_workers(
        &mut self,
        workers: Vec<crate::runtime::subagent::CarriedWorker>,
        resume: super::WorkerResume,
    ) {
        let resume = match resume {
            super::WorkerResume::Auto if self.reload_fence.is_some() => {
                super::WorkerResume::Off("the session is checkpointing for another reload")
            }
            other => other,
        };
        self.adopt_carried_workers(workers, false, resume).await;
    }

    /// Host-only (`SessionCommand::ResumeAfterReload`, sent by reload's
    /// rehydrate in the NEW image). A daemon reload must not end work: when
    /// the reload cut this session's turn, queue ONE message that starts a
    /// new turn, so the agent picks up where it was cut instead of sitting
    /// idle. At most once per reload `generation`. Nothing for a session
    /// that was idle (`turn_cut == Some(false)`), whose cut turn something
    /// already answered (the history decides), or whose record does not say
    /// (`None`: an older image did the reload, or no checkpoint reply).
    /// `stopped`: a clean stop's record, restored by a later start
    /// (`daemon::crash_record`): the same rules, the stop's message.
    /// `cut_tail`: the messages that followed the cut turn's marker at the
    /// checkpoint (that turn's own tail); a user message past them, an
    /// event-shaped one included, re-prompted the turn. `None` (an older
    /// image's record): no such check, as before.
    /// Returns the action ("resumed", "skip", "none"), for the restore's
    /// summary line.
    pub(crate) async fn resume_after_reload(
        &mut self,
        generation: u64,
        reloaded_at: chrono::DateTime<chrono::Utc>,
        turn_cut: Option<bool>,
        cut_tail: Option<u32>,
        stopped: bool,
    ) -> &'static str {
        use crate::daemon::reload::LOG_TARGET;
        if self.resumed_after_reload == Some(generation) {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                generation,
                stopped,
                action = "skip",
                why = "already_resumed",
                "reload resume: this reload's resume was already queued"
            );
            return "skip";
        }
        // Only this reload's own record counts. `None` (an older image's
        // record has no `turn_cut`, or no checkpoint reply came) is NOT a
        // cut: the marker carries no time, and in json persistence every
        // save stamps `updated_at`, so a marker an earlier reload left
        // unanswered can't be told from one this reload wrote.
        if turn_cut.is_none() {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                generation,
                stopped,
                action = "skip",
                why = "no_turn_cut_record",
                "reload resume: the reload's record does not say it cut a turn here; not resumed"
            );
            return "skip";
        }
        let cut = turn_cut == Some(true)
            && crate::engine::interrupt::ends_unanswered_after(
                &self.conv.api_messages,
                crate::engine::interrupt::InterruptReason::Restart,
            );
        if !cut {
            tracing::debug!(
                target: LOG_TARGET,
                session = %self.id,
                generation,
                stopped,
                ?turn_cut,
                action = "none",
                "reload resume: the reload cut no turn here"
            );
            return "none";
        }
        // Another sender (a sweep's nudge, an earlier start's resume that
        // reached the journal before that start died) already re-prompted
        // the cut turn: one resume is enough. The cut turn's own tail (the
        // events it flushed after its marker at the checkpoint) is not a
        // re-prompt; anything after that tail is.
        let reprompted = cut_tail.is_some_and(|tail| {
            crate::engine::interrupt::prompted_after_cut_tail(&self.conv.api_messages, tail as usize)
        });
        if reprompted {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                generation,
                stopped,
                ?cut_tail,
                action = "skip",
                why = "reprompted",
                "reload resume: the cut turn was already re-prompted; not resumed"
            );
            return "skip";
        }
        if let Some(why) = self.resume_blocked() {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                generation,
                stopped,
                action = "skip",
                why,
                "reload resume: the cut turn cannot be resumed now"
            );
            return "skip";
        }
        self.resumed_after_reload = Some(generation);
        if stopped {
            self.restore_resumed_at = Some(chrono::Utc::now());
        }
        let text = if stopped {
            crate::daemon::reload::stop_resume_text(reloaded_at)
        } else {
            crate::daemon::reload::resume_text(reloaded_at)
        };
        let action = self.queue_resume(text).await;
        if stopped {
            // The record carries `resumed_at` at once (the crash-loop cap).
            self.publish_live(true);
        }
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            generation,
            stopped,
            ?turn_cut,
            action,
            "reload resume: queued the resume message for the turn the reload cut"
        );
        if action == "skip_queued_message_continues" {
            "skip"
        } else {
            "resumed"
        }
    }

    /// Why a cut turn can't be resumed right now, if it can't.
    fn resume_blocked(&self) -> Option<&'static str> {
        if self.reload_fence.is_some() {
            Some("reload_fence")
        } else if self.conv.context_head.is_blocked(&self.conv.session) {
            Some("context_head_blocked")
        } else {
            None
        }
    }

    /// Start a new turn with `text` as its user message, or, when something
    /// already runs (a turn a carried worker's wake started, a compaction),
    /// let the message ride its end as a queued message does. Returns the
    /// action for the log line.
    pub(crate) async fn queue_resume(&mut self, text: String) -> &'static str {
        if self.streaming || self.compact.is_some() {
            if self.conv.queued_message.is_some() {
                "skip_queued_message_continues"
            } else {
                self.emit(SessionEventWire::Steered {
                    text: text.clone(),
                    delivered: false,
                });
                self.conv.queued_message = Some(text);
                "queued_behind_turn"
            }
        } else {
            self.conv.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": text.clone()}),
            ));
            self.start_turn(TurnTrigger::QueuedAuto, Some(text)).await;
            "turn_started"
        }
    }

    /// The work a capacity-stopped turn left queued, sent when that stop's
    /// resume is consumed and in place of its text, the way the turn's end
    /// would have sent it had the turn finished (the `Done` arm): events
    /// buffered during the turn go into history first; then a queued user
    /// message is the next user turn, or else the events start an event
    /// turn through the usual claim (the cap notice and idle when it is
    /// spent). Returns what was sent (`None`: nothing was queued). The turn
    /// this starts is the owner's or the events', so it carries no resume
    /// chain.
    pub(crate) async fn send_work_queued_behind_stop(&mut self) -> Option<&'static str> {
        let had_pending = !self.conv.pending_events.is_empty();
        let digest_rides = std::mem::take(&mut self.hold.digest_buffered);
        if !had_pending && self.conv.queued_message.is_none() {
            return None;
        }
        {
            let conv: &mut ConversationState = &mut self.conv;
            for formatted in conv.pending_events.drain(..) {
                conv.api_messages.push(std::sync::Arc::new(serde_json::json!({
                    "role": "user",
                    "content": formatted
                })));
            }
        }
        if let Some(queued) = self.conv.queued_message.take() {
            self.emit_conversation();
            // User-authored: the auto-turn counter resets.
            self.consecutive_auto_turns = 0;
            let content = self.queued_user_content(queued.clone());
            self.conv.api_messages.push(std::sync::Arc::new(
                serde_json::json!({"role": "user", "content": content}),
            ));
            self.start_turn(TurnTrigger::QueuedAuto, Some(queued)).await;
            return Some("queued_message");
        }
        self.request_save();
        self.emit_conversation();
        // GATE (S1): while hold the events stay in history and start no
        // turn (unless a released digest is among them); the resume's own
        // text goes next.
        if self.hold.is_hold() && !digest_rides {
            return None;
        }
        let auto_turn_cap = self.host.config().events.auto_turn_cap;
        if claim_auto_turn_with_cap(&mut self.consecutive_auto_turns, auto_turn_cap) {
            self.start_turn(TurnTrigger::EventAuto, None).await;
            Some("events")
        } else {
            self.emit(SessionEventWire::AutoTurnCapReached { cap: auto_turn_cap });
            self.announce_idle().await;
            Some("events_capped")
        }
    }

    /// Host-only (`SessionCommand::ResumeAfterCrash`, sent by a fresh
    /// daemon's crash restore). A daemon crash must not end work: when the
    /// crash cut this session's turn, queue ONE message that starts a new
    /// turn, at most once per `crash` (the dead daemon's process key).
    ///
    /// The turn counts as cut by THIS crash only when all of these hold:
    /// the dead daemon's record says a turn was in flight; this actor's load
    /// folded a turn draft in, and that draft was written by the dead daemon
    /// (`TurnDraft::writer` = `crash`), so a draft some later process left
    /// (an older binary, a daemon with `daemon.crash_restore = off` that
    /// served the session after the crash, a TUI) never counts; the
    /// history's latest interruption marker is the `Crash` marker that load
    /// wrote, with nothing answering it; and no user message follows that
    /// marker (a resume or nudge that already reached the session after the
    /// crash). Parked and excluded sessions are never sent the command.
    ///
    /// A session that was idle while workers it started were running (the
    /// record's `running_workers`) waits for completion wakes the crash ate:
    /// it gets one note naming them instead (not sent again when the history
    /// already holds that exact note).
    /// Returns the action, for the restore's summary.
    pub(crate) async fn resume_after_crash(
        &mut self,
        crash: String,
        crashed_at: chrono::DateTime<chrono::Utc>,
        turn_in_flight: bool,
        lost_workers: Vec<String>,
    ) -> &'static str {
        use crate::daemon::reload::LOG_TARGET;
        use crate::engine::interrupt::InterruptReason;
        let skip = |why: &'static str| {
            tracing::info!(
                target: LOG_TARGET,
                session = %self.id,
                crash = %crash,
                action = "skip",
                why,
                "crash resume: not resumed"
            );
            "skip"
        };
        if self.resumed_after_crash.as_deref() == Some(crash.as_str()) {
            return skip("already_resumed");
        }
        if !turn_in_flight && lost_workers.is_empty() {
            tracing::debug!(
                target: LOG_TARGET,
                session = %self.id,
                crash = %crash,
                action = "none",
                "crash resume: no turn was in flight and no worker ran here"
            );
            return "none";
        }
        let text =
            crate::daemon::crash_record::resume_text(crashed_at, turn_in_flight, &lost_workers);
        if turn_in_flight {
            match &self.crash_draft {
                None => return skip("no_crash_draft"),
                Some(writer) if writer.as_deref() != Some(crash.as_str()) => {
                    return skip("draft_not_from_this_crash")
                }
                Some(_) => {}
            }
            if !crate::engine::interrupt::ends_unanswered_after(
                &self.conv.api_messages,
                InterruptReason::Crash,
            ) {
                return skip("answered");
            }
            if crate::engine::interrupt::prompted_after_latest_marker(&self.conv.api_messages) {
                return skip("reprompted");
            }
        } else if crate::engine::interrupt::has_user_text(&self.conv.api_messages, &text) {
            // Idle while its workers ran: the stand-in for their completion
            // wakes, sent once (a restore from the same record finds it).
            return skip("already_noted");
        }
        if let Some(why) = self.resume_blocked() {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                crash = %crash,
                action = "skip",
                why,
                "crash resume: the cut turn cannot be resumed now"
            );
            return "skip";
        }
        self.resumed_after_crash = Some(crash.clone());
        if turn_in_flight {
            self.restore_resumed_at = Some(chrono::Utc::now());
        }
        let action = self.queue_resume(text).await;
        if turn_in_flight {
            // The record carries `resumed_at` at once (the crash-loop cap).
            self.publish_live(true);
        }
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            crash = %crash,
            turn_cut = turn_in_flight,
            lost_workers = lost_workers.len(),
            action,
            "crash resume: queued the resume message for the work the crash cut"
        );
        if action == "skip_queued_message_continues" {
            "skip"
        } else {
            "resumed"
        }
    }

    /// [`Self::adopt_workers`]; `failed_reload`: this image's own exec
    /// failed (`ReloadAborted`), so the notice must not say it reloaded.
    async fn adopt_carried_workers(
        &mut self,
        workers: Vec<crate::runtime::subagent::CarriedWorker>,
        failed_reload: bool,
        resume: super::WorkerResume,
    ) {
        use crate::runtime::subagent::DaemonResume;
        if workers.is_empty() {
            return;
        }
        // A repeated `AdoptWorkers` (a record restored twice into this
        // session) must not wake the parent twice for the same worker.
        let workers: Vec<crate::runtime::subagent::CarriedWorker> = if failed_reload {
            workers
        } else {
            let registry = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let mut kept = workers;
            kept.retain(|w| !registry.adopted_already(&w.id));
            kept
        };
        if workers.is_empty() {
            return;
        }
        let woken: Vec<&crate::runtime::subagent::CarriedWorker> =
            workers.iter().filter(|w| w.needs_wake()).collect();
        let mut plan: Vec<Option<DaemonResume>> = match resume {
            super::WorkerResume::Auto => crate::runtime::subagent::plan_daemon_resumes(&woken),
            super::WorkerResume::Off(why) => woken
                .iter()
                .map(|w| w.stopped_by_reload.then(|| DaemonResume::Leave(why.to_string())))
                .collect(),
            super::WorkerResume::Never => vec![None; woken.len()],
        };
        {
            let mut registry = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for w in &workers {
                registry.adopt_carried(w);
            }
            for (w, plan) in woken.iter().zip(&plan) {
                if plan == &Some(DaemonResume::Resume) {
                    // `wait`/`collect`/`subagent_resume` on its id say it is
                    // being resumed until the launch below ends.
                    registry.begin_daemon_resume(&w.id);
                } else {
                    // The next reload repeats this wake only while it is
                    // still queued.
                    registry.note_reload_wake(w);
                }
            }
        }
        // The daemon's resumes, one at a time, oldest handle first. Only a
        // worker actually resumed takes one of the session's slots: one the
        // launch refused or failed leaves its slot to the next in line, and
        // once every slot is taken the rest are left to the model.
        let mut launched: Vec<Option<std::result::Result<(String, u64), String>>> =
            vec![None; woken.len()];
        let mut slots = crate::runtime::subagent::DaemonResumeSlots::default();
        for i in crate::runtime::subagent::daemon_resume_order(&woken, &plan) {
            let w = woken[i];
            if let Some(why) = slots.full() {
                let mut registry = self
                    .runtime
                    .subagent_registry()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                registry.finish_daemon_resume(&w.id, None);
                registry.note_reload_wake(w);
                plan[i] = Some(DaemonResume::Leave(why));
                continue;
            }
            let result = self.resume_for_daemon(w).await;
            slots.launched(result.is_ok());
            launched[i] = Some(result);
        }
        // Then every wake at once: the parent gets one turn for the batch
        // (or none, when the turn the reload cut is already running again:
        // they ride into it).
        let mut resumed: Vec<String> = Vec::new();
        let mut left: Vec<&str> = Vec::new();
        for (i, w) in woken.iter().enumerate() {
            let ev = match (&plan[i], &launched[i]) {
                (Some(DaemonResume::Resume), Some(Ok((new, evicted)))) => {
                    resumed.push(format!("{} as {new}", w.id));
                    crate::tools::build_reload_resumed_event(w, new, *evicted)
                }
                (Some(DaemonResume::Resume), Some(Err(why))) | (Some(DaemonResume::Leave(why)), _) => {
                    left.push(w.id.as_str());
                    crate::tools::build_reload_left_event(w, why)
                }
                _ => {
                    if w.stopped_by_reload {
                        left.push(w.id.as_str());
                    }
                    crate::tools::build_reload_carry_event(w)
                }
            };
            if self.event_queue.push(ev.clone()).is_err() {
                self.event_queue.push_priority(ev);
            }
        }
        // A worker left with its final record lost still has its head on
        // disk reading "running", which no registry owns: mark it terminal
        // (status "cancelled"; it still resumes as a crash checkpoint by the
        // archive_path its wake gives).
        let lost: Vec<crate::runtime::subagent::CarriedWorker> = woken
            .iter()
            .filter(|w| w.crash_head.is_some() && left.contains(&w.id.as_str()))
            .map(|w| (*w).clone())
            .collect();
        if !lost.is_empty() {
            let marked = tokio::task::spawn_blocking(move || {
                let lost: Vec<&crate::runtime::subagent::CarriedWorker> = lost.iter().collect();
                crate::tools::mark_left_crash_heads(&crate::tools::archive_root(), &lost)
            })
            .await;
            match marked {
                Ok(marked) => {
                    for (handle, outcome) in marked {
                        match outcome {
                            Ok(crate::tools::CrashHeadMarked::Rewritten) => tracing::info!(
                                session = %self.id,
                                handle = %handle,
                                "reload: marked the head of a worker whose final record was lost cancelled"
                            ),
                            Ok(crate::tools::CrashHeadMarked::Unchanged(why)) => tracing::info!(
                                session = %self.id,
                                handle = %handle,
                                "reload: left the head of a worker whose final record was lost as it is: {why}"
                            ),
                            Err(e) => tracing::warn!(
                                session = %self.id,
                                handle = %handle,
                                "reload: could not mark the head of a worker whose final record was lost cancelled: {e}"
                            ),
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    session = %self.id,
                    "reload: marking the heads of workers whose final record was lost failed: {e}"
                ),
            }
        }
        tracing::info!(
            session = %self.id,
            carried = workers.len(),
            resumed = ?resumed,
            stopped = ?left,
            woken = woken.len(),
            "reload: adopted workers from the previous image"
        );
        let reloaded = if failed_reload { "daemon reload failed" } else { "daemon reloaded" };
        if !resumed.is_empty() {
            self.emit(SessionEventWire::SystemNotice(format!(
                "{reloaded} — the daemon resumed worker{} {} stopped by the reload",
                if resumed.len() == 1 { "" } else { "s" },
                resumed.join(", "),
            )));
        }
        if !left.is_empty() {
            self.emit(SessionEventWire::SystemNotice(format!(
                "{reloaded} — worker{} {} stopped by the reload; subagent_resume continues {} from {} archive",
                if left.len() == 1 { "" } else { "s" },
                left.join(", "),
                if left.len() == 1 { "it" } else { "them" },
                if left.len() == 1 { "its" } else { "their" },
            )));
        }
    }

    /// The daemon's resume of reload-stopped worker `w` (see
    /// [`Self::adopt_workers`]): `Ok((new handle, evicted windows))`, or why
    /// nothing started. Either way the registry's alias for `w.id` is
    /// settled; a failure leaves its wake to the model.
    async fn resume_for_daemon(
        &mut self,
        w: &crate::runtime::subagent::CarriedWorker,
    ) -> std::result::Result<(String, u64), String> {
        use crate::daemon::reload::LOG_TARGET;
        let ctx = self.runtime.daemon_tool_context(Some(self.secret_prompt_handle.clone()));
        // A worker's `before_tool_call` hooks route to its caller: here the
        // session itself, which never auto-approves a Confirm.
        let hooks = crate::tools::CallerHooks {
            bus: std::sync::Arc::clone(self.runtime.hook_bus()),
            auto_approve: false,
            memory_only: self.runtime.memory_only_policy.clone(),
        };
        let route = self.runtime.child_route().cloned();
        // Sandbox `tools = confined`: a confined session resumes no worker,
        // the daemon's resume included (the gate logs its own line; the
        // reason here never quotes the policy's contact).
        let gated = crate::sandbox::gate_tool(ctx.capabilities.sandbox.as_deref(), "subagent_resume");
        let result = match gated {
            Err(_) => Err("refused by this session's sandbox (tools = confined)".to_string()),
            Ok(()) => {
                crate::tools::CALLER_HOOKS
                    .scope(hooks, crate::tools::resume_for_daemon(ctx, &w.id, route.as_ref()))
                    .await
            }
        };
        let mut registry = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match result {
            Ok((new, context_tokens, evicted)) => {
                registry.finish_daemon_resume(&w.id, Some(&new));
                drop(registry);
                tracing::info!(
                    target: LOG_TARGET,
                    session = %self.id,
                    old = %w.id,
                    new = %new,
                    archive_tokens = context_tokens,
                    evicted_windows = evicted,
                    "reload resume: the daemon resumed a worker the reload stopped"
                );
                Ok((new, evicted))
            }
            Err(why) => {
                registry.finish_daemon_resume(&w.id, None);
                registry.note_reload_wake(w);
                drop(registry);
                tracing::warn!(
                    target: LOG_TARGET,
                    session = %self.id,
                    old = %w.id,
                    why = %why,
                    "reload resume: the daemon could not resume a worker the reload stopped"
                );
                Err(why)
            }
        }
    }

    /// `stop_for_reload` on this session's registry (fences new launches).
    fn stop_workers_for_reload(
        &self,
    ) -> Vec<(String, Arc<std::sync::RwLock<crate::runtime::subagent::SubagentState>>)> {
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop_for_reload()
    }

    /// Host-only (`SessionCommand::ReloadAborted`): the reload this session
    /// checkpointed for did not exec, so the old image keeps serving it.
    /// Lift the reload fence, hand the workers the checkpoint stopped back
    /// to the parent, and deliver what queued up meanwhile.
    pub(crate) async fn reload_aborted(&mut self) {
        self.lift_reload_fence(
            "daemon reload failed — this session continues on the running daemon",
        )
        .await;
    }

    /// Host-only (`SessionCommand::ReloadRefused`, spec §5): the reload was
    /// refused after its drain, before anything was stopped. The quiesce
    /// lifts; a turn the drain paused at a round boundary
    /// ([`Self::refused_turn_paused`]) resumes in place with one message. A
    /// turn still streaming resumes at its end if its stream had already
    /// stopped for the reload (`reload_refused_pending`; the `Done` arm). A
    /// checkpointed session (never one here: the refusal comes before the
    /// checkpoints) is handled as an aborted reload.
    pub(crate) async fn reload_refused(&mut self, refused_at: chrono::DateTime<chrono::Utc>) {
        use crate::daemon::reload::LOG_TARGET;
        if self.reload_fence.is_some() {
            self.reload_aborted().await;
            return;
        }
        self.event_queue.set_quiesce(None);
        if self.streaming {
            self.reload_refused_pending = Some(refused_at);
            tracing::debug!(
                target: LOG_TARGET,
                session = %self.id,
                "reload refused: quiesce lifted; the running turn resumes at its end only if it stopped for the reload"
            );
            return;
        }
        if !self.refused_turn_paused() {
            tracing::debug!(
                target: LOG_TARGET,
                session = %self.id,
                "reload refused: no turn paused here; quiesce lifted"
            );
            return;
        }
        self.resume_refused_turn(refused_at).await;
    }

    /// Whether the last turn is one a reload's drain paused at a round
    /// boundary and nothing has taken up since: its `Restart` marker is
    /// unanswered, no user message came after the turn's own tail, nothing
    /// runs and nothing is queued.
    fn refused_turn_paused(&self) -> bool {
        use crate::engine::interrupt::{ends_unanswered_after, prompted_after_cut_tail, InterruptReason};
        self.reload_quiesced_turn
            && !self.is_parked()
            && !self.streaming
            && self.conv.queued_message.is_none()
            && ends_unanswered_after(&self.conv.api_messages, InterruptReason::Restart)
            && !prompted_after_cut_tail(
                &self.conv.api_messages,
                self.reload_quiesced_tail.unwrap_or(0),
            )
    }

    /// Resume the turn a refused reload's drain paused: ONE message
    /// ([`crate::daemon::reload::refused_resume_text`]), no second marker.
    async fn resume_refused_turn(&mut self, refused_at: chrono::DateTime<chrono::Utc>) {
        use crate::daemon::reload::LOG_TARGET;
        if let Some(why) = self.resume_blocked() {
            tracing::warn!(
                target: LOG_TARGET,
                session = %self.id,
                why,
                "reload refused: the paused turn cannot be resumed now"
            );
            return;
        }
        let action = self
            .queue_resume(crate::daemon::reload::refused_resume_text(refused_at))
            .await;
        tracing::info!(
            target: LOG_TARGET,
            session = %self.id,
            action,
            "reload refused: resumed the turn the drain paused"
        );
    }

    /// Lift the reload fence (see [`Self::reload_aborted`]), telling the
    /// user `notice` (the reload did not happen).
    async fn lift_reload_fence(&mut self, notice: &str) {
        // No exec: the session works normally again.
        self.event_queue.set_quiesce(None);
        let Some(fence) = self.reload_fence.take() else {
            return;
        };
        self.emit(SessionEventWire::SystemNotice(notice.to_string()));
        let workers = if self.runtime.is_live() {
            // From the LIVE registry, not only the checkpoint's snapshot: a
            // worker registered after it was stopped at birth by the
            // registry's reload fence and is owed its wake too. Snapshot
            // entries the registry no longer holds ride along.
            let mut carried = self.reload_carried_workers();
            carried.extend(
                fence
                    .workers
                    .into_iter()
                    .filter(|w| !carried.iter().any(|c| c.id == w.id))
                    .collect::<Vec<_>>(),
            );
            let mut registry = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            registry.lift_reload_fence();
            // The queue the record assumed lost is still here: only the
            // reload's own stops owe a wake.
            registry.rewake_after_failed_exec(carried)
        } else {
            Vec::new()
        };
        tracing::info!(session = %self.id, "reload did not exec: fence lifted");
        self.adopt_carried_workers(workers, true, super::WorkerResume::Never).await;
        self.rearm_park();
        if !self.event_queue.is_empty() {
            self.on_queue_wake().await;
        }
    }

    /// Backstop: no exec and no `ReloadAborted` within
    /// [`budgets::RELOAD_FENCE_TIMEOUT`] of the checkpoint.
    async fn reload_fence_expired(&mut self) {
        tracing::warn!(
            session = %self.id,
            "reload fence expired with no exec and no abort from the daemon; lifting it"
        );
        self.lift_reload_fence(
            "daemon reload never completed or reported back — this session continues on the running daemon",
        )
        .await;
    }

    /// Abort the spawned job (`CompactionCancelled`); prior state intact.
    pub(crate) fn abort_compaction(&mut self) {
        if let Some(job) = self.compact.take() {
            job.task.abort();
            self.emit(SessionEventWire::CompactionCancelled);
            self.publish_presence();
        }
    }

    // ── command dispatch ─────────────────────────────────────────────────

    /// B1: a client-stamped input command from anyone but the owner is
    /// `Refused` with no side effect; `from: None` (host) bypasses.
    async fn handle(&mut self, addressed: Addressed) -> std::ops::ControlFlow<EndReason> {
        use std::ops::ControlFlow;
        let Addressed { from, cmd } = addressed;
        if let SessionCommand::EphemeralLayer { id, epoch, generation, operation } = cmd {
            use crate::runtime::ephemeral::Outcome;
            let outcome = if from.is_none() || from != self.input_owner {
                Outcome::NotOwner
            } else if self.reload_fence.is_some() {
                Outcome::Busy
            } else { self.ephemeral_layer.update(&epoch, generation, operation) };
            if outcome == Outcome::Applied {
                self.ephemeral_installer = from;
                self.config.ephemeral_layer_wait_ms = self.ephemeral_layer.wait_ms();
                self.publish_live(false);
            }
            self.emit(SessionEventWire::QueryResult {
                id,
                value: self.ephemeral_status(Some(outcome)),
                client_tools: None,
            });
            return ControlFlow::Continue(());
        }
        if let Some(c) = from {
            // `rename` is the daemon's control-connection rename; an
            // attached client has `/saveas` (owner only).
            if is_rename_command(&cmd) {
                self.emit(SessionEventWire::Refused {
                    client: c,
                    command: command_name(&cmd).to_string(),
                    reason: "rename is host-only; use /saveas".into(),
                });
                return ControlFlow::Continue(());
            }
            // Only the daemon's reload sends a reload checkpoint (it stops
            // every worker and fences the session for the exec after it).
            if matches!(cmd, SessionCommand::Checkpoint { reason: CheckpointReason::Reload, .. }) {
                self.emit(SessionEventWire::Refused {
                    client: c,
                    command: command_name(&cmd).to_string(),
                    reason: "checkpoint(reload) is host-only; use `synaps daemon reload`".into(),
                });
                return ControlFlow::Continue(());
            }
            if let Some(reason) = is_input_command(&cmd).then(|| self.input_refusal(c)).flatten() {
                self.emit(SessionEventWire::Refused {
                    client: c,
                    command: command_name(&cmd).to_string(),
                    reason,
                });
                return ControlFlow::Continue(());
            }
        }
        // A reload checkpointed this session: nothing may start work or
        // change what the reload record captured until the exec (or until
        // the daemon reports it failed). The wake sources are fenced in the
        // select loop; commands are fenced here.
        if self.reload_fence.is_some() && cmd.refused_while_reloading() {
            // A host rename answers its caller only, never the whole room.
            if let SessionCommand::EngineCommand { id, name, .. } = &cmd {
                if name == RENAME_COMMAND {
                    self.emit(SessionEventWire::QueryResult {
                        id: *id,
                        value: serde_json::json!({
                            "kind": "error", "code": "busy",
                            "text": "daemon reloading; retry in a moment",
                        }),
                        client_tools: None,
                    });
                    return ControlFlow::Continue(());
                }
            }
            let reason = super::RELOADING_REFUSAL.to_string();
            match from {
                Some(c) => self.emit(SessionEventWire::Refused {
                    client: c,
                    command: command_name(&cmd).to_string(),
                    reason,
                }),
                None => self.emit(SessionEventWire::SystemNotice(reason)),
            }
            return ControlFlow::Continue(());
        }
        // Accepted input: with `client_input = shared` its sender becomes the
        // active client. `KeepWarm` doesn't count: `--keep-warm` sends it
        // on attach, not a person typing.
        if let Some(c) = from {
            if is_input_command(&cmd) && !matches!(cmd, SessionCommand::KeepWarm { .. }) {
                self.note_input(c);
            }
        }
        // A host rename (`synaps daemon rename`): handled here for live and
        // parked sessions alike. A parked one is renamed on disk and NOT
        // woken (`host_rename`), so this runs before the B3 wake below.
        if is_rename_command(&cmd) {
            if let SessionCommand::EngineCommand { id, arg, .. } = cmd {
                let value = on_heap(|| self.host_rename(&arg)).await;
                self.emit(SessionEventWire::QueryResult { id, value, client_tools: None });
            }
            return ControlFlow::Continue(());
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
                    | SessionCommand::ReloadAborted
                    | SessionCommand::ReloadRefused { .. }
                    | SessionCommand::ArmPark
                    | SessionCommand::ArmCapacityResume { .. }
                    | SessionCommand::RestoreToolActivations { .. }
                    | SessionCommand::Resync { .. }
                    | SessionCommand::HostEvent(_)
                    | SessionCommand::TakeInput
                    // The held store and the mode are the actor's: a parked
                    // session stays parked for them.
                    | SessionCommand::Delivery { .. }
                    | SessionCommand::HeldDismiss { .. }
                    | SessionCommand::HeldMark { .. }
                    | SessionCommand::ForkDone { .. }
                    // A parked session has no installer: refused at once.
                    | SessionCommand::ToolReturn { .. }
            ) && !is_close_command(&cmd) // closing a parked session must not rebuild it
                // Unpinning a parked session only clears the flag; pinning
                // one wakes it (a pinned session is live by definition).
                && !matches!(cmd, SessionCommand::KeepWarm { on: false });
            if matches!(cmd, SessionCommand::HostEvent(_)) {
                return ControlFlow::Continue(()); // nobody attached, nothing to render
            }
            if matches!(
                cmd,
                SessionCommand::Save
                    | SessionCommand::Cancel
                    | SessionCommand::Park
                    | SessionCommand::ArmPark
                    | SessionCommand::Checkpoint { .. }
            ) {
                if let SessionCommand::Checkpoint { .. } = cmd {
                    self.capacity_close_restore();
                    let record = serde_json::to_value(self.reload_record()).unwrap_or_default();
                    self.emit(SessionEventWire::QueryResult {
                        id: super::wire::CHECKPOINT_QUERY_ID,
                        value: serde_json::json!({ "ok": true, "parked": true, "record": record }),
                        client_tools: None,
                    });
                }
                // An Esc cancels an armed capacity resume without waking
                // the session.
                if matches!(cmd, SessionCommand::Cancel) {
                    on_heap(|| self.capacity_owner_cancel()).await;
                }
                return ControlFlow::Continue(()); // already on disk / idle
            }
            if needs_runtime {
                if let Err(e) = on_heap(|| self.ensure_live()).await {
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
        // Debug builds only, opt-in: a test makes THIS session's actor
        // panic on one exact prompt (panic-containment tests).
        #[cfg(debug_assertions)]
        if let SessionCommand::Submit { text, .. } = &cmd {
            if std::env::var("SYNAPS_TEST_PANIC_PROMPT").is_ok_and(|p| !p.is_empty() && *text == p) {
                panic!("synaps test: forced panic in a session actor");
            }
        }
        match cmd {
            SessionCommand::Submit { text, attachments } => {
                on_heap(|| self.submit(text, attachments, from)).await
            }
            SessionCommand::Steer { text } => {
                if self.streaming {
                    self.steer(text)
                } else {
                    on_heap(|| self.submit(text, vec![], from)).await
                }
            }
            SessionCommand::SubagentSteer { handle_id, text } => {
                self.steer_subagent(from, &handle_id, text)
            }
            SessionCommand::Cancel => {
                // The user's cancel, even of a driver's turn: noted before
                // the revoke below cancels that turn.
                self.note_cancel_cause(crate::engine::interrupt::InterruptReason::User);
                if self.driver.is_some() {
                    self.driver_revoke("canceled");
                }
                on_heap(|| self.cancel_turn(crate::engine::interrupt::InterruptReason::User)).await;
                // An idle Esc also cancels an armed capacity resume.
                on_heap(|| self.capacity_owner_cancel()).await;
            }
            SessionCommand::Answer { prompt_id, value } => self.answer(from, prompt_id, value),
            SessionCommand::EphemeralLayer { .. } => unreachable!("handled before dispatch"),
            // Client tools (actor_client_tools.rs): the reply goes to the
            // sender alone; a return never touches the runtime.
            SessionCommand::RegisterClientTools { id, fields } => {
                on_heap(|| self.register_client_tools(from, id, fields)).await;
            }
            SessionCommand::ToolReturn { id, fields } => self.client_tool_return(from, id, fields),
            SessionCommand::Set { id, setting } => {
                on_heap(|| self.apply_setting(id, setting)).await;
                // A crash restore re-applies what the record carries.
                self.publish_live(false);
            }
            // `CompactionStarted` is the contract; no notice before it.
            SessionCommand::Compact { instructions } => {
                on_heap(|| self.compact(instructions, "manual")).await;
            }
            SessionCommand::NewSession => {
                // The registration belongs to the old conversation: its
                // calls end (cancels + `dropped{new_conversation}`). A turn
                // still waiting on one is cancelled, as Esc would.
                let ended = self.drop_client_tools(crate::runtime::client_tools::DropReason::NewConversation);
                if ended > 0 && self.streaming {
                    self.note_cancel_cause(crate::engine::interrupt::InterruptReason::User);
                    on_heap(|| self.cancel_turn(crate::engine::interrupt::InterruptReason::User))
                        .await;
                }
                self.ephemeral_layer.invalidate(crate::runtime::ephemeral::Reason::NewSession);
                self.memory_only.clear();
                // The owner moved on: the old session's armed capacity
                // resume is cleared, acknowledged, before the new one.
                on_heap(|| self.capacity_owner_clear("new_session")).await;
                if self.driver.is_some() {
                    self.driver_revoke("session replaced");
                }
                // A turn still waiting for `on_session_start` would open its
                // stream later on the cleared history, under the new journal
                // id: cancel it first, so it ends (and is saved) in the old
                // session with nothing sent.
                if self.turn_awaiting_start.is_some() {
                    on_heap(|| self.cancel_turn(crate::engine::interrupt::InterruptReason::User))
                        .await;
                }
                // Before `clear` saves the old session: a ping in flight is
                // aborted and booked to it, not to the new one.
                self.cache_keepwarm_conversation_replaced();
                // `clear` saves the old session directly: queued saves of it
                // must land first, never after.
                on_heap(|| self.persister.flush()).await;
                on_heap(|| self.conv.clear(&self.runtime)).await;
                self.runtime
                    .begin_conversation(Some(self.conv.session.id.clone()));
                self.set_journal_id(self.conv.session.id.clone());
                // F10: lock follows the new session id.
                self.reacquire_session_lock(&self.conv.session.id.clone());
                self.emit(SessionEventWire::Cleared {
                    session_id: self.conv.session.id.clone(),
                });
                self.emit_conversation();
            }
            SessionCommand::Save => {
                on_heap(|| self.save()).await;
            }
            SessionCommand::Query { id, query } => on_heap(|| self.query(id, query)).await,
            // `/close` ends the session, which no other engine command can.
            SessionCommand::EngineCommand { id, name, arg } if name == CLOSE_COMMAND => {
                if let Some(reason) = self.close_command(id, &arg) {
                    return ControlFlow::Break(reason);
                }
            }
            SessionCommand::EngineCommand { id, name, arg } => {
                on_heap(|| self.engine_command(id, name, arg)).await;
                // `/model`, `/keepwarm`, `saveas` …: what the record carries.
                self.publish_live(false);
            }
            SessionCommand::Attach { client, mode } => on_heap(|| self.attach(client, mode)).await,
            SessionCommand::Detach { client } => {
                self.detach(client);
                // Nobody left to notice a stale period: confirm the prefix
                // now rather than at the next ping, so a session whose next
                // request already changed is free to park.
                if self.attached.is_empty() && self.cache_keepwarm.active() {
                    on_heap(|| self.cache_keepwarm_recheck()).await;
                }
            }
            SessionCommand::End { reason } => {
                // The owner closed the session: an armed capacity resume is
                // cleared, acknowledged, before it ends. A host's end (a
                // shutdown) keeps it in the record.
                if matches!(reason, EndReason::ClientQuit) {
                    on_heap(|| self.capacity_owner_clear("closed")).await;
                }
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
            } => on_heap(|| self.submit_prepared(messages, user_text)).await,
            SessionCommand::PluginCommand {
                id,
                plugin,
                name,
                arg,
            } => on_heap(|| self.plugin_command(id, plugin, name, arg)).await,
            SessionCommand::Resume { id, query } => on_heap(|| self.resume(id, query)).await,
            SessionCommand::RestoreToolActivations { activations } => {
                self.restore_tool_activations(activations)
            }
            // Parked sessions returned above (their evict deadline stands).
            SessionCommand::ArmPark => self.rearm_park(),
            SessionCommand::Checkpoint {
                reason,
                deadline,
                skip_quiesce_grace,
            } => on_heap(|| self.checkpoint(reason, deadline, skip_quiesce_grace)).await,
            SessionCommand::AdoptWorkers { workers, resume } => {
                on_heap(|| self.adopt_workers(workers, resume)).await
            }
            SessionCommand::ArmCapacityResume { stop } => self.capacity_arm_restored(stop),
            SessionCommand::ReloadAborted => on_heap(|| self.reload_aborted()).await,
            SessionCommand::ReloadRefused { refused_at } => {
                on_heap(|| self.reload_refused(refused_at)).await
            }
            SessionCommand::ResumeAfterReload {
                generation,
                reloaded_at,
                turn_cut,
                cut_tail,
                stopped,
                reply,
            } => {
                let action = on_heap(|| {
                    self.resume_after_reload(generation, reloaded_at, turn_cut, cut_tail, stopped)
                })
                .await;
                if let Some(reply) = reply {
                    let _ = reply.send(action);
                }
            }
            SessionCommand::ResumeAfterCrash {
                crash,
                crashed_at,
                turn_in_flight,
                lost_workers,
                reply,
            } => {
                let action = on_heap(|| {
                    self.resume_after_crash(crash, crashed_at, turn_in_flight, lost_workers)
                })
                .await;
                if let Some(reply) = reply {
                    let _ = reply.send(action);
                }
            }
            SessionCommand::Park => {
                // Explicit request: end an active keep-warm period, then park.
                self.cache_keepwarm_park_requested();
                if let std::ops::ControlFlow::Break(reason) = on_heap(|| self.park()).await {
                    return ControlFlow::Break(reason);
                }
            }
            SessionCommand::TakeInput => self.take_input(from),
            SessionCommand::KeepWarm { on } => {
                self.keep_warm = on;
                self.meta.pinned = on; // the attach snapshot's copy
                self.keep_warm_cell.store(on, std::sync::atomic::Ordering::Release);
                self.note_pin();
                // Unpinned with nobody who ever typed: back to the old rule.
                if !on && !self.cache_keepwarm.interactive_seen && self.cache_keepwarm.active() {
                    self.cache_keepwarm_end(super::cache_keepwarm::PeriodEnd::Disabled);
                }
                self.rearm_park();
                self.publish_live(false);
                self.emit(SessionEventWire::SystemNotice(if on {
                    PIN_ON_NOTICE.to_string()
                } else {
                    PIN_OFF_NOTICE.to_string()
                }));
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
            // S1 Focus hold mode (`actor_held.rs`).
            cmd @ (SessionCommand::Delivery { .. }
            | SessionCommand::HeldRelease { .. }
            | SessionCommand::HeldDismiss { .. }
            | SessionCommand::HeldMark { .. }) => on_heap(|| self.held_command(from, cmd)).await,
            SessionCommand::Fork { label } => on_heap(|| self.fork(label, from)).await,
            SessionCommand::ForkDone {
                client,
                label,
                message_count,
                result,
            } => self.fork_done(client, label, message_count, result),
        }
        ControlFlow::Continue(())
    }

    // ── teardown (tui/mod.rs:352-432 + chat.rs shutdown) ─────────────────

    async fn finish(&mut self, reason: EndReason) {
        // Ended on purpose (`/close`, `daemon close`, a client's quit): the
        // pin goes with it. A daemon stop, reload or crash keeps it.
        if self.keep_warm && reason == EndReason::ClientQuit {
            self.keep_warm = false;
            self.note_pin();
        }
        // Child spend: no new run from here. The wait for the workers'
        // spend shares the cancel step's budget (one deadline), so an end
        // takes no longer than before.
        let spend_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(budgets::CANCEL_TURN_TIMEOUT_SECS);
        self.spend_sink.begin_closing();
        self.cache_keepwarm_end(super::cache_keepwarm::PeriodEnd::SessionEnding);
        // Before on_session_end clears the keyed injection (STEP 2): a hook
        // not yet called leaves the line; one in flight finishes holding the
        // gate and drops what it injects (`SessionStart::end`).
        self.session_start.end();
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
        // Child spend: stop the remaining workers and book what they report
        // until none still reports or the deadline; then publish nothing
        // more and book what was already queued, before the save.
        self.settle_child_spend(spend_deadline).await;
        self.spend_sink.close();
        self.book_queued_spend();

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

/// The conversation's title for listings: the saved title, else what
/// `Session::auto_title` will make of the first user message.
fn conversation_title(conv: &ConversationState) -> Option<String> {
    if !conv.session.title.is_empty() {
        return Some(conv.session.title.clone());
    }
    conv.api_messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("user"))
        .find_map(|m| agent_core::core::session::user_content_for_display(&m["content"]))
        .map(|t| t.chars().take(80).collect())
}

/// Whether the 1 Hz `SubagentRows` tick keeps running: always during a turn,
/// and after it while any worker is still running (a background worker's
/// progress events rode the ended turn's stream; the rows carry it now).
fn keep_subagent_tick(streaming: bool, any_running: bool) -> bool {
    streaming || any_running
}

/// Which running workers an interrupted turn cancels. Only the user's own
/// interrupt of a live session is turn-scoped: Esc stops what that turn
/// started, not reactive workers inherited from earlier turns (which the
/// user may be waiting on). Quitting/teardown (`ending`), the cost cap, host
/// checkpoints/restarts and driver revocation stop every running worker.
fn worker_cancel_scope(
    reason: crate::engine::interrupt::InterruptReason,
    ending: bool,
    turn_generation: u64,
) -> crate::runtime::subagent::WorkerCancelScope {
    use crate::engine::interrupt::InterruptReason;
    use crate::runtime::subagent::WorkerCancelScope;
    match reason {
        InterruptReason::User if !ending => WorkerCancelScope::Turn(turn_generation),
        _ => WorkerCancelScope::All,
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

/// The reload fence's backstop deadline; pends forever when unfenced.
async fn reload_fence_timer(deadline: Option<tokio::time::Instant>) {
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

/// How long a session waits for process-level extension discovery before
/// `on_session_start`: `startup.extensions_ready_timeout_secs`, or the
/// budget constant when that is 0.
fn extensions_ready_timeout(config: &crate::SynapsConfig) -> Duration {
    match config.startup.extensions_ready_timeout_secs {
        0 => budgets::EXTENSIONS_READY_TIMEOUT,
        secs => Duration::from_secs(secs),
    }
}

/// This session's `on_session_start` task, while it is in line or running.
async fn session_start_task(
    start: &mut super::session_start::SessionStart,
) -> std::result::Result<bool, tokio::task::JoinError> {
    match start {
        super::session_start::SessionStart::Queued { task, .. } => task.await,
        _ => std::future::pending().await,
    }
}

impl SessionTask {
    pub fn id(&self) -> &SessionId {
        self.0.id()
    }

    /// Test seam: this session's spend sink, to publish child spend as its
    /// workers do (child spend).
    #[cfg(any(test, feature = "testing"))]
    pub fn spend_sink(&self) -> crate::runtime::child_spend::SpendSink {
        self.0.spend_sink.clone()
    }

    /// Test seam: this session's subagent registry, to seed fake workers
    /// before `run()` (see `EngineHost::create_session_unspawned`).
    #[cfg(any(test, feature = "testing"))]
    pub fn subagent_registry(
        &self,
    ) -> Arc<std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>> {
        Arc::clone(self.0.runtime.subagent_registry())
    }

    /// Test seam: a clone of this session's runtime. Clones share the
    /// memory-consent state, so a harness can revoke consent the way the
    /// `memory_context` tool's capability does (`memory_context_disable`).
    #[cfg(any(test, feature = "testing"))]
    pub fn runtime_for_harness(&self) -> crate::Runtime {
        (*self.0.runtime).clone()
    }

    /// Test seam: this session's event queue (what a worker's finalizer
    /// pushes its completion wake into).
    #[cfg(any(test, feature = "testing"))]
    pub fn event_queue(&self) -> Arc<crate::events::EventQueue> {
        Arc::clone(&self.0.event_queue)
    }

    /// Test seam: an event buffered during a turn (`pending_events`), as a
    /// busy session without a live steering channel buffers it.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_pending_event(&mut self, event: &crate::events::Event) {
        let formatted = crate::events::format_event_for_agent(event);
        self.0.conv.pending_events.push(formatted);
    }

    /// Unbiased select (the TUI loop is unbiased too).
    pub async fn run(mut self) {
        // (S1) An unexpired hold comes back; an expired one is released.
        self.0.held_restore();
        let queue = Arc::clone(&self.0.event_queue);
        let reason = loop {
            let actor = &mut self.0;
            tokio::select! {
                cmd = actor.cmd_rx.recv() => match cmd {
                    Some(cmd) => {
                        if let std::ops::ControlFlow::Break(reason) = on_heap(move || actor.handle(cmd)).await {
                            break reason;
                        }
                    }
                    // Every handle dropped: nobody can ever reach us again.
                    None => break EndReason::HostShutdown,
                },
                Ok(()) = actor.ephemeral_changes.changed() => {
                    let status = actor.ephemeral_changes.borrow_and_update().clone();
                    if actor.ephemeral_layer.opted_in() {
                        actor.emit(SessionEventWire::EphemeralLayerChanged { status });
                    }
                },
                Some(req) = actor.secret_prompt_rx.recv() => actor.on_prompt_request(req),
                Some(child) = actor.child_rx.recv() => actor.on_child_event(child),
                // A client tool call's invoke/cancel, for its installer.
                Some(frame) = actor.client_tool_frames.recv() => actor.on_client_tool_frame(frame),
                // A held wake (and its permit) stays queued.
                _ = queue.notified(), if !actor.holds(WorkSource::QueueWake) => on_heap(|| actor.on_queue_wake()).await,
                // Child spend, with or without a turn. While a cancelled
                // stream drains, reports queue here and are booked after.
                Some(message) = actor.spend_rx.recv() => actor.on_child_spend(message),
                _ = next_tick(&mut actor.subagent_tick) => {
                    actor.publish_subagent_rows();
                    actor.flush_turn_draft();
                }
                _ = park_timer(actor.park_deadline), if !actor.holds(WorkSource::ParkTimer) => {
                    if let std::ops::ControlFlow::Break(reason) = on_heap(|| actor.park()).await {
                        break reason;
                    }
                }
                _ = super::actor_capacity::capacity_timer(actor.capacity.timer), if !actor.holds(WorkSource::CapacityResume) => {
                    on_heap(|| actor.on_capacity_timer()).await;
                }
                _ = prompt_abandon_timer(actor.prompt_abandon_deadline) => {
                    actor.on_prompt_abandon_deadline();
                }
                _ = super::cache_keepwarm::keepwarm_timer(actor.cache_keepwarm.deadline()), if !actor.holds(WorkSource::KeepWarmTimer) => {
                    on_heap(|| actor.on_cache_keepwarm_due()).await;
                }
                res = super::cache_keepwarm::poll_keepwarm_ping(&mut actor.cache_keepwarm.task) => {
                    actor.on_cache_keepwarm_done(res);
                }
                res = poll_compaction(&mut actor.compact) => on_heap(|| actor.on_compaction_done(res)).await,
                ran = session_start_task(&mut actor.session_start) => {
                    on_heap(|| actor.session_start_finished(ran)).await;
                },
                _ = driver_tick_timer(actor.driver.is_some(), actor.driver_pending.is_some(), &mut actor.driver_tick_interval), if !actor.holds(WorkSource::DriverTick) => {
                    on_heap(|| actor.driver_tick()).await;
                }
                _ = super::actor_held::lease_timer(actor.hold.lease_deadline), if !actor.holds(WorkSource::HeldLease) => {
                    on_heap(|| actor.on_held_lease_due()).await;
                }
                _ = reload_fence_timer(actor.reload_fence.as_ref().map(|f| f.deadline)) => {
                    on_heap(|| actor.reload_fence_expired()).await;
                }
                _ = revoked_turn_timer(actor.revoked_turn_deadline) => {
                    on_heap(|| actor.finish_revoked_turn(false)).await;
                }
                ev = next_stream_event(&mut actor.stream) => match ev {
                    Some(ev) => on_heap(move || actor.on_stream_event(ev)).await,
                    // A revoked turn's stream ending is its end.
                    None if actor.turn_cancelled() => on_heap(|| actor.finish_revoked_turn(true)).await,
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
                        on_heap(|| actor.announce_idle()).await;
                    }
                },
            }
            // What a fork copies while no turn runs (`session::fork`).
            self.0.fork_note_idle();
        };
        on_heap(|| self.0.finish(reason)).await;
    }
}

/// The future `make` builds, on the heap.
///
/// An unoptimized build gives every temporary of an `async fn` body its own
/// slot in that body's poll frame, so awaiting a large future inline puts
/// copies of it on the stack. The actor loop awaited `handle` and
/// `on_stream_event` inline, and they awaited every command's and every
/// turn end's future inline, down to the spawn of a turn; with the merged
/// turn path that chain outgrew a 2 MiB tokio worker stack in debug builds
/// (the session-actor tests, `chat` on a provider failure). The loop, the
/// dispatch in `handle` and the turn-end steps in `on_stream_event` build
/// each future in this non-inlined frame instead, which keeps it out of
/// theirs; one allocation per awaited step is the cost.
#[inline(never)]
fn on_heap<'a, T, F>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>
where
    F: std::future::Future<Output = T> + Send + 'a,
{
    Box::pin(make())
}

/// Wait (bounded by [`budgets::WORKER_RELOAD_STOP_TIMEOUT`]) until every worker
/// `stop_for_reload` returned has settled its finalizer: a stopped worker's
/// resume archive is on disk and its in-flight tool was dropped, which kills
/// that tool's process group (after the exec nothing could); a worker that
/// finished on its own has pushed its completion wake into the queue the
/// reload carries. Returns the ids the reload stopped.
async fn await_stopped_workers(
    session: &str,
    stopping: Vec<(String, Arc<std::sync::RwLock<crate::runtime::subagent::SubagentState>>)>,
) -> Vec<String> {
    if stopping.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = stopping.iter().map(|(id, _)| id.clone()).collect();
    tracing::info!(session, workers = ?ids, "reload: stopping running workers");
    let deadline = tokio::time::Instant::now() + budgets::WORKER_RELOAD_STOP_TIMEOUT;
    loop {
        let pending: Vec<&str> = stopping
            .iter()
            .filter(|(_, s)| !s.read().unwrap_or_else(|p| p.into_inner()).completion_settled)
            .map(|(id, _)| id.as_str())
            .collect();
        if pending.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                session,
                workers = ?pending,
                "reload: workers did not stop in time; only their last archive survives the exec"
            );
            // A v2 run (spec §5): the reload's check found its record
            // durable, so its last committed head is what the exec leaves.
            for entry in crate::tools::worker_records::writers_snapshot() {
                if let (true, Some(generation)) =
                    (pending.contains(&entry.handle.as_str()), entry.committed)
                {
                    tracing::warn!(
                        session,
                        worker = %entry.handle,
                        generation,
                        "reload: worker {}'s final missed the stop bound; its head (generation {generation}) is the recoverable record",
                        entry.handle
                    );
                }
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    stopping
        .iter()
        .filter(|(_, s)| s.read().unwrap_or_else(|p| p.into_inner()).stopped_by_reload)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod worker_cancel_scope_tests {
    use super::worker_cancel_scope;
    use crate::engine::interrupt::InterruptReason;
    use crate::runtime::subagent::WorkerCancelScope;

    #[test]
    fn only_a_live_user_interrupt_is_turn_scoped() {
        assert_eq!(
            worker_cancel_scope(InterruptReason::User, false, 7),
            WorkerCancelScope::Turn(7),
            "Esc stops the interrupted turn's workers only"
        );
        assert_eq!(
            worker_cancel_scope(InterruptReason::User, true, 7),
            WorkerCancelScope::All,
            "quitting the client mid-turn tears every worker down"
        );
        for reason in [
            InterruptReason::CostCap,
            InterruptReason::Restart,
            InterruptReason::Host,
            InterruptReason::Driver,
        ] {
            assert_eq!(
                worker_cancel_scope(reason, false, 7),
                WorkerCancelScope::All,
                "{reason:?} stops every running worker"
            );
        }
    }
}

/// What `emit` sends for `event`: a run's request-ledger events are
/// host-internal (`SessionEvent::for_clients`): a ledgered attempt's usage
/// goes out as the plain `Usage` it stands in for, a liability not at all.
/// Every other envelope is unchanged.
fn client_form(event: SessionEventWire) -> Option<SessionEventWire> {
    match event {
        SessionEventWire::Stream(stream) => stream.for_clients().map(SessionEventWire::Stream),
        other => Some(other),
    }
}

#[cfg(test)]
mod client_form_tests {
    use super::{client_form, SessionEventWire};
    use agent_core::{LiabilityState, UsageCounts, UsagePurpose, UsageRequest};
    use crate::{SessionEvent, StreamEvent};

    fn request() -> UsageRequest {
        UsageRequest {
            request_id: "req-1".into(),
            purpose: UsagePurpose::RolloverSummary,
            model: "provider/model-b".into(),
        }
    }

    /// `emit` never sends a liability, open or released.
    #[test]
    fn a_liability_is_never_emitted() {
        for state in [LiabilityState::Open, LiabilityState::Released] {
            let event = SessionEventWire::Stream(StreamEvent::Session(
                SessionEvent::UsageLiability {
                    request: request(),
                    estimate_micro_usd: 9,
                    state,
                },
            ));
            assert!(client_form(event).is_none());
        }
    }

    /// `emit` sends a ledgered attempt as its plain usage, attributed to the
    /// attempt's model.
    #[test]
    fn a_ledgered_attempt_is_emitted_as_its_plain_usage() {
        let event = SessionEventWire::Stream(StreamEvent::Session(SessionEvent::UsageAttempt {
            request: request(),
            usage: UsageCounts {
                input_tokens: 5,
                output_tokens: 6,
                ..Default::default()
            },
        }));
        match client_form(event) {
            Some(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Usage {
                input_tokens: 5,
                output_tokens: 6,
                model: Some(model),
                ..
            }))) => assert_eq!(model, "provider/model-b"),
            other => panic!("expected the plain usage, got {other:?}"),
        }
    }

    /// Everything else is emitted unchanged.
    #[test]
    fn other_envelopes_pass_through() {
        assert!(matches!(
            client_form(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))),
            Some(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done)))
        ));
        assert!(matches!(
            client_form(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(
                "n".into()
            )))),
            Some(SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(t)))) if t == "n"
        ));
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
