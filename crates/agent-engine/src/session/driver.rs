//! Actor-side session driver state machine (E §2).
//!
//! `DriverState` lives on `SessionActor.driver`. It observes `StreamEvent`s the
//! actor already receives and issues the actor's own `start_turn`/`steer`/`cancel`.
//! Nothing here touches a client, a terminal, or `App`. Clients see only
//! `SessionEventWire::Driver*` events.
//!
//! Many items are consumed by P4 (driver_tick). Allow dead_code until then.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::extensions::feedback;
use crate::extensions::runtime::{ExtensionHandler, ExtensionHealth};
use crate::extensions::session_driver::{
    Grant, Outcome, PollRequest, PrepareError, Proposal, Reply, Selection,
};
use crate::{CancellationToken, Runtime, SharedMessage};

type Workers = Arc<std::sync::Mutex<crate::runtime::subagent::SubagentRegistry>>;
type Manager = Arc<tokio::sync::RwLock<crate::extensions::manager::ExtensionManager>>;

pub(crate) fn cancel_workers(workers: &Workers, epoch: u64) {
    workers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .cancel_spawn_epoch(epoch);
}

// ── Abort-on-drop task wrapper ───────────────────────────────────────────────

pub(crate) struct Task<T>(pub tokio::task::JoinHandle<T>);
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ── Scheduled proposal ──────────────────────────────────────────────────────

pub(crate) struct Scheduled {
    pub proposal: Proposal,
    pub due: Instant,
}

// ── Pending async work ──────────────────────────────────────────────────────

pub(crate) struct DriverPending {
    pub generation: u64,
    pub session_id: String,
    pub task: Task<TaskResult>,
}

pub(crate) enum TaskResult {
    Command {
        owner: String,
        command: String,
        handler: Arc<dyn ExtensionHandler>,
        handler_generation: Option<u64>,
        result: Result<serde_json::Value, String>,
        report: crate::extensions::invoke_output::InvokeOutputReport,
    },
    Poll(Result<Reply, String>),
    Prepared {
        proposal: Proposal,
        result: Box<Result<Runtime, PrepareError>>,
    },
    Started(Result<super::actor::ActiveStream, String>),
}

// ── Core state ──────────────────────────────────────────────────────────────

/// The driver state lives on the actor; dropping it cancels all owned work.
pub(crate) struct DriverState {
    pub grant: Grant,
    pub handler: Arc<dyn ExtensionHandler>,
    pub handler_generation: u64,
    pub cancel: CancellationToken,
    pub workers: Workers,
    pub worker_epoch: u64,
    pub deadline_task: Option<Task<()>>,
    pub proposal: Option<Scheduled>,
    pub selection: Selection,
    pub awaiting_terminal: bool,
    pub outcome: Option<(Outcome, String)>,
    pub feedback: feedback::Tracker,
    pub completed_feedback: &'static str,
    /// Only explicit human submissions, never draft text. Entries remain until
    /// delivery acknowledgement or an atomic next-turn commit. Bounded: 16 msgs
    /// / 256 KiB total UTF-8 bytes (§2).
    pub steering: VecDeque<String>,
    pub auto_wakes_blocked: bool,
    pub cost_at_arm: f64,
}

impl Drop for DriverState {
    fn drop(&mut self) {
        self.cancel.cancel();
        cancel_workers(&self.workers, self.worker_epoch);
    }
}

// ── Steering bounds ─────────────────────────────────────────────────────────

const STEERING_MAX_MESSAGES: usize = 16;
const STEERING_MAX_BYTES: usize = 256 * 1024;

// ── Lifecycle helpers ───────────────────────────────────────────────────────

pub(crate) fn live_generation(handler: &Arc<dyn ExtensionHandler>) -> Result<u64, String> {
    handler
        .lifecycle_snapshot()
        .filter(|s| {
            matches!(
                s.health,
                ExtensionHealth::Running | ExtensionHealth::Degraded
            )
        })
        .map(|s| s.generation)
        .ok_or_else(|| "owning extension is not live or its lifecycle is unavailable".into())
}

pub(crate) fn same_lifecycle(
    handler: &Arc<dyn ExtensionHandler>,
    generation: u64,
) -> Result<(), String> {
    if live_generation(handler)? == generation {
        Ok(())
    } else {
        Err("owning extension process restarted or transport was lost".into())
    }
}

pub(crate) fn same_handler(
    manager: &Manager,
    owner: &str,
    handler: &Arc<dyn ExtensionHandler>,
    generation: u64,
) -> Result<(), String> {
    let current = manager
        .try_read()
        .map_err(|_| "extension lifecycle busy".to_string())?
        .session_driver_handler(owner)?;
    if Arc::ptr_eq(&current, handler) {
        same_lifecycle(handler, generation)
    } else {
        Err("owning extension was replaced".into())
    }
}

pub(crate) fn schedule(active: &mut DriverState, proposal: Proposal) -> Result<(), String> {
    if proposal.delay < Duration::from_secs(1) || proposal.delay > Duration::from_secs(300) {
        return Err("invalid proposal delay".into());
    }
    let due = Instant::now() + proposal.delay;
    if active
        .grant
        .deadline()
        .is_some_and(|deadline| due >= deadline)
    {
        return Err("proposal would run past the grant deadline".into());
    }
    active.selection = proposal.selection.clone();
    active.proposal = Some(Scheduled { proposal, due });
    Ok(())
}

/// Build the pending-turn history by appending undelivered steering to
/// the current api_messages snapshot — read-only, never drains the queue.
pub(crate) fn history_with_steering(
    api_messages: &[SharedMessage],
    steering: &VecDeque<String>,
) -> Vec<SharedMessage> {
    let mut history = api_messages.to_vec();
    for text in steering {
        history.push(Arc::new(serde_json::json!({"role": "user", "content": text})));
    }
    history
}

/// Drain queued steering into api_messages before a driver turn starts.
pub(crate) fn commit_submission(
    api_messages: &mut Vec<SharedMessage>,
    steering: &mut VecDeque<String>,
    prompt: &str,
    abort_context: &mut Option<String>,
) {
    for text in steering.drain(..) {
        api_messages.push(Arc::new(serde_json::json!({"role": "user", "content": text})));
    }
    let text = if let Some(context) = abort_context.take() {
        format!("{context}\n\n{prompt}")
    } else {
        prompt.to_string()
    };
    api_messages.push(Arc::new(serde_json::json!({"role": "user", "content": text})));
}

/// Build the PollRequest for the next poll.
pub(crate) fn poll_request(
    active: &DriverState,
    outcome: Outcome,
    error_kind: String,
) -> PollRequest {
    PollRequest {
        run_id: active.grant.run_id.clone(),
        decision_id: uuid::Uuid::new_v4().to_string(),
        outcome,
        error_kind,
        model: active.selection.model.clone(),
        effort: active.selection.effort.clone(),
        feedback: active.grant.feedback_enabled().then(|| {
            if outcome == Outcome::Success {
                active.completed_feedback
            } else {
                "unknown"
            }
            .into()
        }),
        session_id: Some(active.grant.session_id.clone()),
    }
}

/// Deadline remaining in ms, for wire events.
pub(crate) fn deadline_ms(grant: &Grant) -> Option<u64> {
    grant.deadline().map(|d| {
        d.saturating_duration_since(Instant::now())
            .as_millis()
            .min(u64::MAX as u128) as u64
    })
}
