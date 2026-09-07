//! Local interactive session-driver adapter. Policy belongs to the owning plugin.
//! Nothing here is restored from disk or reachable from tools/notifications.

mod feedback;
#[cfg(test)]
mod steering_tests;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::TurnOutcome;
use synaps_cli::engine::reactor::EventDisposition;
use synaps_cli::extensions::invoke_output::{
    invoke_event_channel, InvokeOutputBudget, InvokeOutputReport,
};
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::runtime::{ExtensionHandler, ExtensionHealth};
use synaps_cli::extensions::session_driver::{
    self as protocol, Grant, Outcome, PollRequest, PrepareError, Proposal, Reply, Selection,
};
use synaps_cli::{CancellationToken, Runtime, SessionEvent, StreamEvent};

type Workers = Arc<std::sync::Mutex<synaps_cli::runtime::subagent::SubagentRegistry>>;

fn cancel_workers(workers: &Workers, epoch: u64) {
    workers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .cancel_spawn_epoch(epoch);
}

use super::app::{App, ChatMessage, THINKING_PLACEHOLDER};
use super::stream_handler::ActiveStream;

type Manager = Arc<tokio::sync::RwLock<ExtensionManager>>;

/// Dropping a JoinHandle alone detaches it. Every driver-owned task must abort.
struct Task<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Scheduled {
    proposal: Proposal,
    due: Instant,
}

struct Active {
    grant: Grant,
    handler: Arc<dyn ExtensionHandler>,
    handler_generation: u64,
    cancel: CancellationToken,
    workers: Workers,
    worker_epoch: u64,
    _deadline_task: Option<Task<()>>,
    proposal: Option<Scheduled>,
    selection: Selection,
    awaiting_terminal: bool,
    outcome: Option<(Outcome, String)>,
    feedback: feedback::Tracker,
    completed_feedback: &'static str,
    // Only explicit human submissions, never draft text. Entries remain until
    // delivery acknowledgement or an atomic next-turn commit.
    steering: VecDeque<String>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.cancel();
        cancel_workers(&self.workers, self.worker_epoch);
    }
}

struct Pending {
    generation: u64,
    session_id: String,
    task: Task<TaskResult>,
}

enum TaskResult {
    Command {
        owner: String,
        command: String,
        handler: Arc<dyn ExtensionHandler>,
        handler_generation: Option<u64>,
        result: Result<serde_json::Value, String>,
        report: InvokeOutputReport,
    },
    Poll(Result<Reply, String>),
    Prepared {
        proposal: Proposal,
        result: Box<Result<Runtime, PrepareError>>,
    },
    Started(Result<ActiveStream, String>),
}

#[derive(Default)]
pub(crate) struct SessionDriver {
    generation: u64,
    active: Option<Active>,
    pending: Option<Pending>,
    // Remember the command owner across revocation,
    // solely to let their generic interactive stop command cancel a live turn.
    interrupted_owner: Option<String>,
    // Revoked runs must not restart through generic event-bus wakes. Do not
    // trust raw event labels to identify workers; inhibit all automatic wakes
    // until the user actually submits new work or explicitly arms a new run.
    auto_wakes_blocked: bool,
}

impl SessionDriver {
    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some() || self.pending.is_some()
    }

    pub(crate) fn owner(&self) -> Option<&str> {
        self.active
            .as_ref()
            .map(|a| a.grant.plugin_id.as_str())
            .or(self.interrupted_owner.as_deref())
    }

    fn invalidate(&mut self) -> bool {
        let was_active = self.is_active();
        if let Some(active) = &self.active {
            self.auto_wakes_blocked = true;
            self.interrupted_owner = Some(active.grant.plugin_id.clone());
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.active = None;
        was_active
    }

    fn spawn(
        &mut self,
        session_id: String,
        future: impl std::future::Future<Output = TaskResult> + Send + 'static,
    ) {
        debug_assert!(self.pending.is_none());
        self.pending = Some(Pending {
            generation: self.generation,
            session_id,
            task: Task(tokio::spawn(future)),
        });
    }
}

pub(crate) fn revoke(app: &mut App, reason: &str) {
    // A failed/expired run must neither lose submitted guidance nor dispatch it
    // as an unowned automatic turn. Restore only unacknowledged entries as draft.
    let unsent = app
        .session_driver
        .active
        .as_mut()
        .map(|a| a.steering.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    if !unsent.is_empty() {
        let mut draft = unsent.join("\n\n");
        let existing = app.input_text();
        if !existing.is_empty() {
            draft.push_str("\n\n");
            draft.push_str(&existing);
        }
        app.set_input_text(&draft);
        app.push_msg(ChatMessage::System(
            "Undelivered steering restored to the input draft; not automatically resent.".into(),
        ));
    }
    if app.session_driver.invalidate() {
        app.push_msg(ChatMessage::System(format!(
            "Session driver stopped: {reason}"
        )));
    }
}

/// Returns true when the armed driver handled this explicit submission. No
/// control command calls this; the dispatcher resolves those first. A bounded
/// FIFO handles channel-close/terminal races without the ordinary single-slot
/// auto-send queue escaping the grant or overwriting earlier human messages.
pub(crate) fn submit_steering(
    app: &mut App,
    input: &str,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) -> bool {
    let Some(active) = app.session_driver.active.as_ref() else {
        return false;
    };
    if active.cancel.is_cancelled()
        || active.grant.expired()
        || active.grant.session_id != app.session.id
        || app.context_head.is_blocked(&app.session)
    {
        revoke(app, "steering cannot renew an expired or invalid grant");
        restore_submission(app, input);
        return true;
    }
    if !app.pending_attachments.is_empty() {
        restore_submission(app, input);
        app.push_msg(ChatMessage::Error("Autonomous steering is text-only. Pending attachments retained; press Esc before submitting attachments.".into()));
        return true;
    }
    if input.trim().is_empty() {
        return true;
    }
    const MAX_MESSAGES: usize = 16;
    const MAX_BYTES: usize = 256 * 1024;
    if active.steering.len() >= MAX_MESSAGES
        || active
            .steering
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(input.len())
            > MAX_BYTES
    {
        restore_submission(app, input);
        app.push_msg(ChatMessage::Error(
            "Steering queue is full; input retained. Wait for delivery or press Esc to stop."
                .into(),
        ));
        return true;
    }
    let active = app.session_driver.active.as_mut().expect("checked active");
    active.steering.push_back(input.to_owned());
    // Changed human guidance invalidates prior-output comparisons. An in-flight
    // mixed-purpose turn reports unknown, not an invented progress/repeat signal.
    active.feedback = feedback::Tracker::default();
    active.completed_feedback = "unknown";
    let sent = app.streaming && tx.is_some_and(|tx| tx.send(input.to_owned()).is_ok());
    app.input_before_paste = None;
    app.pasted_char_count = 0;
    app.push_msg(ChatMessage::System(if sent {
        format!("→ steering: {input}")
    } else {
        format!("→ steering queued for the next autonomous turn: {input}")
    }));
    true
}

fn restore_submission(app: &mut App, input: &str) {
    let draft = app.input_text();
    if draft.is_empty() {
        app.set_input_text(input);
    } else if draft != input {
        app.set_input_text(&format!("{input}\n\n{draft}"));
    }
}

pub(crate) fn steering_delivered(app: &mut App, message: &str) {
    if let Some(active) = app.session_driver.active.as_mut() {
        if active
            .steering
            .front()
            .is_some_and(|first| first == message)
        {
            let delivered = active.steering.pop_front().expect("checked first");
            // The engine acknowledgment precedes its next MessageHistory.
            // Retain the human instruction now so abort/error-before-history
            // cannot lose it. The later authoritative snapshot replaces this
            // provisional append (never merges it), avoiding duplicate delivery.
            app.api_messages.push(Arc::new(serde_json::json!({
                "role":"user", "content":delivered
            })));
        }
    }
}

fn history_with_steering(app: &App) -> Vec<agent_core::SharedMessage> {
    let mut history = app.api_messages.clone();
    if let Some(active) = &app.session_driver.active {
        history.extend(
            active
                .steering
                .iter()
                .map(|text| Arc::new(serde_json::json!({"role":"user", "content":text}))),
        );
    }
    history
}

/// Called only after full latest-history validation. Drafts/attachments are not
/// read or consumed by an automatic submission.
fn commit_submission(app: &mut App, prompt: &str) {
    if let Some(active) = app.session_driver.active.as_mut() {
        let steering = active.steering.drain(..).collect::<Vec<_>>();
        for text in steering {
            app.api_messages
                .push(Arc::new(serde_json::json!({"role":"user", "content":text})));
            app.push_msg(ChatMessage::User(text));
        }
    }
    let content = app.abort_context.take().map_or_else(
        || prompt.to_owned(),
        |context| format!("{context}\n\n{prompt}"),
    );
    app.api_messages.push(Arc::new(
        serde_json::json!({"role":"user", "content":content}),
    ));
}

pub(crate) fn auto_wakes_allowed(app: &App) -> bool {
    !app.session_driver.auto_wakes_blocked
        && !app
            .session_driver
            .active
            .as_ref()
            .is_some_and(|a| a.cancel.is_cancelled() || a.grant.expired())
}

/// Called only after a real user's new submission passes ordinary preflight.
pub(crate) fn user_takeover(app: &mut App, runtime: &Runtime) {
    app.session_driver.auto_wakes_blocked = false;
    app.session_driver.interrupted_owner = None;
    runtime
        .subagent_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set_spawn_cancellation(None);
}

/// Event delivery inside the already-owned foreground turn is not a new turn
/// and grants no new authority. Idle injection or buffering takes priority.
pub(crate) fn observe_events<'a>(
    app: &mut App,
    dispositions: impl IntoIterator<Item = &'a EventDisposition>,
) {
    let owned_turn = app.streaming
        && app
            .session_driver
            .active
            .as_ref()
            .is_some_and(|a| a.awaiting_terminal);
    if !owned_turn
        || dispositions
            .into_iter()
            .any(|d| !matches!(d, EventDisposition::Steered | EventDisposition::DisplayOnly))
    {
        revoke(app, "event-bus work took priority");
    }
}

/// A successfully persisted same-session pressure checkpoint continues the
/// current authorized turn. Failure or replacement never does.
pub(crate) fn observe_checkpoint(app: &mut App, session_id: &str, succeeded: bool) {
    if !succeeded
        || app
            .session_driver
            .active
            .as_ref()
            .is_some_and(|a| a.grant.session_id != session_id || session_id != app.session.id)
    {
        revoke(app, "context checkpoint failed or session replaced");
    }
}

fn notice(app: &mut App, text: &str) {
    if !text.is_empty() {
        app.push_msg(ChatMessage::System(super::stream_handler::sanitize_notice(
            text,
        )));
    }
}

/// Only the slash-command dispatch path calls this. Editors/tools keep using
/// their ordinary command helper and cannot consume a session_driver response.
pub(crate) fn start_command(
    app: &mut App,
    manager: &Manager,
    owner: &str,
    command: &str,
    arg: &str,
) {
    revoke(app, "explicit command");
    let (handler, timeout) = match manager.try_read() {
        Ok(manager) => (
            manager.user_action_handler(owner),
            if manager.session_driver_handler(owner).is_ok() {
                5
            } else {
                120
            },
        ),
        Err(_) => (
            Err("extensions are loading or busy — try again shortly".into()),
            120,
        ),
    };
    let handler = match handler {
        Ok(handler) => handler,
        Err(error) => {
            app.push_msg(ChatMessage::Error(error));
            return;
        }
    };
    let handler_generation = live_generation(&handler).ok();
    let owner = owner.to_owned();
    let command = command.to_owned();
    let args = arg.split_whitespace().map(str::to_owned).collect();
    app.session_driver
        .spawn(app.session.id.clone(), async move {
            let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
            let request_id = uuid::Uuid::new_v4().to_string();
            // No manager guard survives into this task. The collector always runs
            // concurrently, including when output floods or cancellation occurs.
            let (result, report) = tokio::time::timeout(Duration::from_secs(timeout), async {
                tokio::join!(
                    handler.invoke_command(&command, args, &request_id, sink),
                    collector.collect()
                )
            })
            .await
            .unwrap_or_else(|_| {
                (
                    Err(format!("interactive command timed out ({timeout}s)")),
                    InvokeOutputReport {
                        events: Vec::new(),
                        counters: Default::default(),
                    },
                )
            });
            TaskResult::Command {
                owner,
                command,
                handler,
                handler_generation,
                result,
                report,
            }
        });
}

/// Conditions checked both before starting async work and after its completion.
/// Active-turn secret prompts remain governed by the ordinary confirmation UI.
fn idle_conflict(app: &App, runtime: &Runtime) -> Option<&'static str> {
    if app.queued_message.is_some()
        || !app.pending_events.is_empty()
        || !runtime.event_queue().is_empty()
    {
        return Some("other queued work");
    }
    if app.compact_task.is_some() {
        return Some("compaction");
    }
    if app.modal_stack.top() != super::focus::PaneId::Chat || app.secret_prompts.is_active() {
        return Some("interactive dialog");
    }
    if app.extension_loader_running {
        return Some("extensions loading or reloading");
    }
    if app.gamba_child.is_some() {
        return Some("terminal handed off");
    }
    if app.context_head.is_blocked(&app.session) {
        return Some("unverified context head");
    }
    if completion_blocked(runtime) {
        return Some("outstanding workers require collection/reconciliation");
    }
    None
}

fn completion_blocked(runtime: &Runtime) -> bool {
    runtime.orchestration().is_some_and(|o| {
        !matches!(
            o.completion_gate(),
            agent_core::orchestration::CompletionGate::Allowed
        )
    }) || runtime
        .subagent_registry()
        .lock()
        .map(|r| {
            r.list_active().iter().any(|(_, _, status)| {
                *status == synaps_cli::runtime::subagent::SubagentStatus::Running
            })
        })
        .unwrap_or(true)
}

fn live_generation(handler: &Arc<dyn ExtensionHandler>) -> Result<u64, String> {
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

fn same_lifecycle(handler: &Arc<dyn ExtensionHandler>, generation: u64) -> Result<(), String> {
    if live_generation(handler)? == generation {
        Ok(())
    } else {
        Err("owning extension process restarted or transport was lost".into())
    }
}

fn same_handler(
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

fn schedule(active: &mut Active, proposal: Proposal) -> Result<(), String> {
    // Defense in depth even for directly constructed Reply values in tests.
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

fn arm(
    app: &mut App,
    manager: &Manager,
    owner: String,
    handler: Arc<dyn ExtensionHandler>,
    handler_generation: u64,
    reply: Reply,
    runtime: &Runtime,
) -> Result<(), String> {
    same_handler(manager, &owner, &handler, handler_generation)?;
    if app.session_driver.active.is_some() {
        return Err("cannot replace an armed grant".into());
    }
    let (grant, proposal) = Grant::from_start(&owner, &app.session.id, reply)?;
    let cancel = CancellationToken::new();
    let workers = runtime.subagent_registry().clone();
    let worker_epoch = workers
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set_spawn_cancellation(Some(cancel.clone()));
    let deadline_task = grant.deadline().map(|deadline| {
        let cancel = cancel.clone();
        let workers = workers.clone();
        // Independent of the UI loop: even a different UI arm doing slow IO
        // cannot let an active model call outlive the monotonic grant deadline.
        Task(tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            cancel.cancel();
            cancel_workers(&workers, worker_epoch);
        }))
    });
    let mut active = Active {
        selection: proposal.selection.clone(),
        grant,
        handler,
        handler_generation,
        cancel,
        workers,
        worker_epoch,
        _deadline_task: deadline_task,
        proposal: None,
        awaiting_terminal: false,
        outcome: None,
        feedback: feedback::Tracker::default(),
        completed_feedback: "unknown",
        steering: VecDeque::new(),
    };
    let proposal_notice = proposal.notice.clone();
    schedule(&mut active, proposal)?;
    // Last fallible start step: failed/invalid replies never change the mode.
    // This is session-only, like /context auto/off, and never grants memory
    // capture/recall/import consent or clears an unresolved durability barrier.
    let context_notice = active.grant.apply_context_mode(runtime)?;
    if let Some(text) = context_notice {
        notice(app, &text);
    }
    app.push_msg(ChatMessage::System(format!(
        "Session driver authorized for {owner} in this local TUI session. It may spend money and send the proposed prompt AND retained conversation history (including submitted attachments) across the allowed providers. Model/effort changes are session-only. Esc/Ctrl-C stops. Typing keeps the loop running; submitted text steers it. Control commands (including status) revoke the grant."
    )));
    notice(app, &proposal_notice);
    app.session_driver.interrupted_owner = None;
    app.session_driver.auto_wakes_blocked = false;
    app.session_driver.active = Some(active);
    Ok(())
}

/// Observe only an owned, opted-in turn, before the UI consumes events. No
/// content or hashes leave the local tracker; policy stays in the plugin.
pub(crate) fn observe_feedback(app: &mut App, event: Option<&StreamEvent>) {
    if let (Some(active), Some(event)) = (app.session_driver.active.as_mut(), event) {
        if active.awaiting_terminal && active.grant.feedback_enabled() {
            active.feedback.observe(event);
        }
    }
}

/// Capture BEFORE the stream handler consumes/drops Error, Done, or EOF. The
/// resulting metadata is observed AFTER normal history/accounting repair.
pub(crate) enum Terminal {
    Success,
    Failure(Outcome, String),
    Canceled,
}

pub(crate) fn capture_terminal(event: Option<&StreamEvent>, canceled: bool) -> Option<Terminal> {
    if canceled {
        return Some(Terminal::Canceled);
    }
    match event {
        Some(StreamEvent::Session(SessionEvent::Done)) => Some(Terminal::Success),
        Some(StreamEvent::Session(SessionEvent::Error(error))) => {
            if matches!(error.outcome, TurnOutcome::Canceled) {
                return Some(Terminal::Canceled);
            }
            let (outcome, kind) = protocol::classify_turn_error(error);
            Some(Terminal::Failure(outcome, kind))
        }
        None => Some(Terminal::Failure(Outcome::Blocked, "unknown".into())),
        _ => None,
    }
}

pub(crate) fn observe_terminal(app: &mut App, runtime: &Runtime, terminal: Option<Terminal>) {
    let Some(terminal) = terminal else {
        return;
    };
    let Some(active) = app.session_driver.active.as_mut() else {
        return;
    };
    // Exactly once, and only for a turn this grant actually submitted.
    if !active.awaiting_terminal {
        return;
    }
    active.awaiting_terminal = false;
    let outcome = match terminal {
        Terminal::Canceled => {
            revoke(app, "canceled");
            return;
        }
        Terminal::Success => {
            active.completed_feedback = if active.grant.feedback_enabled() {
                active.feedback.finish()
            } else {
                "unknown"
            };
            (Outcome::Success, "none".into())
        }
        Terminal::Failure(Outcome::TimeCheckpoint, kind)
            if active.grant.time_checkpoints_enabled()
                && !runtime.turn_budget().max_elapsed.is_zero() =>
        {
            // Not success, provider failure, or repeated/empty feedback. The
            // opted-in plugin decides whether the SAME run may take a new turn.
            active.completed_feedback = "unknown";
            active.feedback = feedback::Tracker::default();
            (Outcome::TimeCheckpoint, kind)
        }
        Terminal::Failure(Outcome::TimeCheckpoint, _) => (Outcome::Blocked, "unknown".into()),
        Terminal::Failure(outcome, kind) => (outcome, kind),
    };
    if matches!(outcome.0, Outcome::Blocked) {
        revoke(
            app,
            "blocked or unexpected end of stream; explicit user action required",
        );
    } else if let Some(reason) =
        idle_conflict(app, runtime).filter(|_| runtime.event_queue().is_empty())
    {
        revoke(app, reason);
    } else if let Some(active) = app.session_driver.active.as_mut() {
        active.outcome = Some(outcome);
    }
}

/// Dedicated persistent timer, NOT an animation sleep reset by stream deltas.
/// Each invocation performs only bounded state transitions; all plugin and
/// provider/preparation IO is in abort-on-drop tasks.
pub(crate) async fn tick(
    app: &mut App,
    runtime: &mut Runtime,
    manager: &Manager,
    secret_prompt: &synaps_cli::tools::SecretPromptHandle,
    stream: &mut Option<ActiveStream>,
    cancel_token: &mut Option<CancellationToken>,
    steer_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    // Cancellation during async stream setup has no stream event to clean
    // up the UI. Never leave the frontend falsely busy after aborting setup.
    if stream.is_none() && cancel_token.as_ref().is_some_and(|ct| ct.is_cancelled()) {
        app.streaming = false;
        app.status_text = None;
        app.drop_empty_thinking();
        *cancel_token = None;
        *steer_tx = None;
    }
    if let Some(active) = &app.session_driver.active {
        let invalid = if active.grant.session_id != app.session.id {
            Some("session replaced".to_string())
        } else if active.grant.expired() {
            Some("grant deadline reached".into())
        } else if active.cancel.is_cancelled() {
            Some("canceled".into())
        } else {
            same_handler(
                manager,
                &active.grant.plugin_id,
                &active.handler,
                active.handler_generation,
            )
            .err()
        };
        if let Some(reason) = invalid {
            revoke(app, &reason);
            return;
        }
    }
    // Queued external work and safety/lifecycle changes outrank pending results.
    // Draft edits and the driver-owned human steering queue do not. Modal gates are checked at
    // idle boundaries, not while tools are using the normal confirmation UI.
    if app.compact_task.is_some()
        || app.queued_message.is_some()
        || !app.pending_events.is_empty()
        || app.extension_loader_running
        || app.context_head.is_blocked(&app.session)
    {
        revoke(app, "queued work or session/lifecycle change");
        return;
    }
    // Let the reactor classify queued events before deciding whether they are
    // competing work or steering within this owned turn. Never dispatch past it.
    if !runtime.event_queue().is_empty() {
        return;
    }
    // Never send staged attachment bytes without explicit human submission.
    // Keep the grant (and its deadline), but defer idle work until detached.
    if !app.streaming && !app.pending_attachments.is_empty() {
        return;
    }
    if app
        .session_driver
        .pending
        .as_ref()
        .is_some_and(|p| p.task.0.is_finished())
    {
        let mut pending = app.session_driver.pending.take().expect("checked pending");
        if pending.generation != app.session_driver.generation
            || pending.session_id != app.session.id
        {
            return;
        }
        let result = match (&mut pending.task.0).await {
            Ok(result) => result,
            Err(error) => {
                revoke(app, "driver task failed");
                app.push_msg(ChatMessage::Error(format!(
                    "Session driver task failed: {error}"
                )));
                return;
            }
        };
        match result {
            TaskResult::Command {
                owner,
                command,
                handler,
                handler_generation,
                result,
                report,
            } => {
                let current = manager
                    .try_read()
                    .ok()
                    .and_then(|m| m.user_action_handler(&owner).ok());
                if !current
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &handler))
                {
                    revoke(app, "command owner unloaded, replaced, or unavailable");
                    app.push_msg(ChatMessage::Error(
                        "Interactive command result discarded: owner lifecycle changed".into(),
                    ));
                    return;
                }
                let output_failed = report.is_limited()
                    || report.events.iter().any(|event| {
                        matches!(
                            event,
                            synaps_cli::extensions::runtime::InvokeCommandEvent::Output(
                                synaps_cli::extensions::commands::CommandOutputEvent::Error { .. }
                            )
                        )
                    });
                if let Some(value) = super::commands::apply_interactive_command_result(
                    &owner, &command, result, report, app,
                ) {
                    let result = match protocol::parse_reply(&value) {
                        Ok(Some(reply @ Reply::Start { .. })) => {
                            if output_failed {
                                Err("command reported an error or exceeded its output budget"
                                    .into())
                            } else if app.streaming || stream.is_some() {
                                Err("foreground turn still active".into())
                            } else if let Some(reason) = idle_conflict(app, runtime) {
                                Err(reason.into())
                            } else {
                                match handler_generation {
                                    Some(generation) => arm(
                                        app, manager, owner, handler, generation, reply, runtime,
                                    ),
                                    None => Err(
                                        "command owner did not expose a live lifecycle at dispatch"
                                            .into(),
                                    ),
                                }
                            }
                        }
                        Ok(Some(Reply::Stop { notice: text } | Reply::Status { notice: text })) => {
                            notice(app, &text);
                            Ok(())
                        }
                        Ok(Some(Reply::Next { .. })) => {
                            Err("next requires an armed poll, not an interactive command".into())
                        }
                        Ok(None) => Ok(()),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        app.push_msg(ChatMessage::Error(format!(
                            "Session driver rejected: {error}"
                        )));
                    }
                }
            }
            TaskResult::Poll(result) => {
                let Some(active) = app.session_driver.active.as_mut() else {
                    return;
                };
                let stop_notice = match &result {
                    Ok(Reply::Stop { notice }) => Some(notice.clone()),
                    _ => None,
                };
                match result.and_then(|reply| active.grant.accept(reply)) {
                    Ok(Some(proposal)) => {
                        let text = proposal.notice.clone();
                        if let Err(error) = schedule(active, proposal) {
                            revoke(app, &error);
                        } else {
                            notice(app, &text);
                        }
                    }
                    Ok(None) => {
                        revoke(app, "owner requested stop");
                        if let Some(text) = stop_notice {
                            notice(app, &text);
                        }
                    }
                    Err(error) => revoke(app, &format!("invalid poll response: {error}")),
                }
            }
            TaskResult::Prepared { proposal, result } => {
                if app.streaming || stream.is_some() {
                    revoke(app, "foreground work took priority");
                    return;
                }
                if let Some(reason) = idle_conflict(app, runtime) {
                    revoke(app, reason);
                    return;
                }
                match *result {
                    Err(PrepareError::Selection(error)) => {
                        app.push_msg(ChatMessage::Error(format!(
                            "Session driver selection rejected (nothing sent): {error}"
                        )));
                        if let Some(active) = app.session_driver.active.as_mut() {
                            active.outcome = Some((Outcome::SelectionRejected, "unknown".into()));
                        }
                    }
                    Err(PrepareError::Blocked(error)) => {
                        revoke(app, &format!("preflight blocked: {error}"))
                    }
                    Ok(candidate) => {
                        let mut validated = proposal.clone();
                        if let Some(context) = &app.abort_context {
                            validated.prompt = format!("{context}\n\n{}", proposal.prompt);
                        }
                        if let Err(error) = protocol::validate_prepared(
                            &candidate,
                            &validated,
                            &history_with_steering(app),
                        ) {
                            revoke(
                                app,
                                &format!("prepared selection changed before commit: {error}"),
                            );
                            return;
                        }
                        // Use the latest acknowledged history plus queued human
                        // steering, not a stale pre-prepare snapshot. Never drain
                        // the user's draft or staged attachment buffer here.
                        commit_submission(app, &proposal.prompt);
                        *runtime = candidate;
                        app.session.model = runtime.model().to_owned();
                        app.session.thinking_level = runtime.thinking_level().to_owned();
                        app.push_msg(ChatMessage::User(proposal.prompt));
                        app.consecutive_auto_turns = 0;
                        app.turn_baseline = app.api_messages.len();
                        app.spinner_frame = 0;
                        app.streaming = true;
                        app.status_text = Some("connecting…".into());
                        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
                        let Some(active) = app.session_driver.active.as_mut() else {
                            return;
                        };
                        active.awaiting_terminal = true;
                        active.completed_feedback = "unknown";
                        if active.grant.feedback_enabled() {
                            active
                                .feedback
                                .begin_turn(&active.selection.model, &active.selection.effort);
                        }
                        let handler = active.handler.clone();
                        let handler_generation = active.handler_generation;
                        let ct = active.cancel.child_token();
                        *cancel_token = Some(ct.clone());
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        *steer_tx = Some(tx);
                        let runtime = runtime.clone();
                        let history = app.api_messages.clone();
                        let secret = secret_prompt.clone();
                        app.session_driver
                            .spawn(app.session.id.clone(), async move {
                                if ct.is_cancelled() {
                                    return TaskResult::Started(Err(
                                        "canceled before stream setup".into(),
                                    ));
                                }
                                if let Err(error) = same_lifecycle(&handler, handler_generation) {
                                    return TaskResult::Started(Err(error));
                                }
                                tokio::select! {
                                    biased;
                                    _ = ct.cancelled() => TaskResult::Started(Err("canceled during stream setup".into())),
                                    started = runtime.run_stream_with_messages(history, ct.clone(), Some(rx), Some(secret), false) => TaskResult::Started(Ok(started)),
                                }
                            });
                        // Session-only mirrors follow ordinary session persistence,
                        // never global model/thinking config persistence.
                        app.save_session().await;
                    }
                }
            }
            TaskResult::Started(started) => match started {
                Ok(started) if app.session_driver.active.is_some() => {
                    *stream = Some(started);
                    app.status_text = None;
                }
                Err(error) => revoke(app, &error),
                _ => {}
            },
        }
    }
    if app.streaming || stream.is_some() || app.session_driver.pending.is_some() {
        return;
    }
    if let Some(reason) = idle_conflict(app, runtime) {
        revoke(app, reason);
        return;
    }
    let Some(active) = app.session_driver.active.as_mut() else {
        return;
    };
    if let Some((outcome, error_kind)) = active.outcome.take() {
        let request = poll_request(active, outcome, error_kind);
        let handler = active.handler.clone();
        app.session_driver
            .spawn(app.session.id.clone(), async move {
                TaskResult::Poll(protocol::poll(handler, request).await)
            });
    } else if active
        .proposal
        .as_ref()
        .is_some_and(|p| Instant::now() >= p.due)
    {
        let mut proposal = active.proposal.take().expect("checked proposal").proposal;
        // Match the ordinary append path's retained abort context in preflight,
        // but keep the displayed/submitted prompt separate so it isn't doubled.
        let original_prompt = proposal.prompt.clone();
        if let Some(context) = &app.abort_context {
            proposal.prompt = format!("{context}\n\n{}", proposal.prompt);
        }
        let runtime = runtime.clone();
        let history = history_with_steering(app);
        app.session_driver
            .spawn(app.session.id.clone(), async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    protocol::prepare(&runtime, &proposal, &history),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(PrepareError::Blocked("preparation timed out (5s)".into()))
                });
                proposal.prompt = original_prompt;
                TaskResult::Prepared {
                    proposal,
                    result: Box::new(result),
                }
            });
    }
}

/// Selection is pinned when scheduling, not read from the Runtime: a zero-send
/// selection rejection still reports the rejected proposal, not the old model.
fn poll_request(active: &Active, outcome: Outcome, error_kind: String) -> PollRequest {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};

    struct Handler;
    #[async_trait::async_trait]
    impl ExtensionHandler for Handler {
        fn id(&self) -> &str {
            "fixture"
        }
        fn lifecycle_snapshot(
            &self,
        ) -> Option<synaps_cli::extensions::runtime::ExtensionLifecycle> {
            Some(synaps_cli::extensions::runtime::ExtensionLifecycle {
                generation: 1,
                health: ExtensionHealth::Running,
            })
        }
        async fn handle(&self, _: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }

    fn proposal() -> Proposal {
        Proposal {
            selection: Selection {
                model: "example/exact-model".into(),
                effort: "high".into(),
            },
            prompt: "Inspect retained work; continue without replay".into(),
            delay: Duration::from_secs(1),
            notice: String::new(),
        }
    }

    fn active(session: &str, max_duration_ms: Option<u64>) -> Active {
        let p = proposal();
        let (grant, _) = Grant::from_start(
            "fixture",
            session,
            Reply::Start {
                run_id: "run-test".into(),
                models: vec![p.selection.clone()],
                prompt: p.prompt.clone(),
                delay_ms: 1000,
                max_duration_ms,
                feedback_version: None,
                time_checkpoint_version: None,
                context_mode: None,
                notice: String::new(),
            },
        )
        .unwrap();
        Active {
            grant,
            handler: Arc::new(Handler),
            handler_generation: 1,
            cancel: CancellationToken::new(),
            workers: Runtime::new_headless().subagent_registry().clone(),
            worker_epoch: 0,
            _deadline_task: None,
            proposal: None,
            selection: p.selection,
            awaiting_terminal: true,
            outcome: None,
            feedback: feedback::Tracker::default(),
            completed_feedback: "unknown",
            steering: VecDeque::new(),
        }
    }

    fn app() -> App {
        let mut app = App::new(synaps_cli::Session::new(
            "example/exact-model",
            "high",
            None,
        ));
        app.session_driver.active = Some(active(&app.session.id, None));
        app
    }

    struct ChangingHandler(std::sync::atomic::AtomicU64);
    #[async_trait::async_trait]
    impl ExtensionHandler for ChangingHandler {
        fn id(&self) -> &str {
            "changing"
        }
        fn lifecycle_snapshot(
            &self,
        ) -> Option<synaps_cli::extensions::runtime::ExtensionLifecycle> {
            let generation = self.0.load(std::sync::atomic::Ordering::SeqCst);
            (generation != 0).then_some(synaps_cli::extensions::runtime::ExtensionLifecycle {
                generation,
                health: ExtensionHealth::Running,
            })
        }
        async fn handle(&self, _: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }

    #[test]
    fn same_handler_pointer_does_not_preserve_authority_after_restart_or_death() {
        let handler = Arc::new(ChangingHandler(std::sync::atomic::AtomicU64::new(1)));
        let erased: Arc<dyn ExtensionHandler> = handler.clone();
        let generation = live_generation(&erased).unwrap();
        assert!(same_lifecycle(&erased, generation).is_ok());
        handler.0.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(same_lifecycle(&erased, generation).is_err());
        handler.0.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(same_lifecycle(&erased, generation).is_err());
    }

    #[test]
    fn owned_turn_steering_is_not_competing_work_but_buffered_or_idle_is() {
        let mut app = app();
        app.streaming = true;
        observe_events(
            &mut app,
            &[EventDisposition::Steered, EventDisposition::DisplayOnly],
        );
        assert!(app.session_driver.is_active());
        observe_events(&mut app, &[EventDisposition::Buffered]);
        assert!(!app.session_driver.is_active());
        assert!(!auto_wakes_allowed(&app));
        let mut app = self::app();
        app.streaming = false;
        observe_events(&mut app, &[EventDisposition::Injected]);
        assert!(!app.session_driver.is_active());
    }

    #[test]
    fn terminal_waits_for_reactor_classification_and_never_polls_queued_work() {
        let mut app = app();
        let runtime = Runtime::new_headless();
        runtime
            .event_queue()
            .push(synaps_cli::events::types::Event::simple(
                "test", "event", None,
            ))
            .unwrap();
        observe_terminal(&mut app, &runtime, Some(Terminal::Success));
        assert!(app
            .session_driver
            .active
            .as_ref()
            .unwrap()
            .outcome
            .is_some());
        assert!(idle_conflict(&app, &runtime).is_some());
        observe_events(&mut app, &[EventDisposition::Injected]);
        assert!(!app.session_driver.is_active());
    }

    #[tokio::test]
    async fn same_session_checkpoint_receipt_retains_grant_failure_revokes() {
        let mut app = app();
        let id = app.session.id.clone();
        let (receipt, ack) = agent_core::core::context_head::ContextHeadReceipt::channel();
        // Use the post-persistence observer, not user filesystem IO.
        observe_checkpoint(&mut app, &id, true);
        receipt.complete(Ok(()));
        assert!(ack.await.unwrap().is_ok());
        assert!(app.session_driver.is_active());
        observe_checkpoint(&mut app, &id, false);
        assert!(!app.session_driver.is_active());
        let mut app = self::app();
        observe_checkpoint(&mut app, "foreign", true);
        assert!(!app.session_driver.is_active());
    }

    #[test]
    fn revocation_cancels_workers_and_event_wakes_until_explicit_takeover() {
        use synaps_cli::runtime::subagent::{SubagentHandle, SubagentState};
        let runtime = Runtime::new_headless();
        let mut app = app();
        app.session_driver.active.as_mut().unwrap().workers = runtime.subagent_registry().clone();
        let state = Arc::new(std::sync::RwLock::new(SubagentState::new()));
        let (shutdown, mut rx) = tokio::sync::oneshot::channel();
        let handle = SubagentHandle::new(
            "sa-test".into(),
            1,
            "test".into(),
            "task".into(),
            "model".into(),
            "system".into(),
            300,
            state.clone(),
            None,
            Some(shutdown),
            None,
        );
        runtime.subagent_registry().lock().unwrap().register(handle);
        revoke(&mut app, "deadline");
        assert!(state.read().unwrap().cancel_requested);
        assert!(rx.try_recv().is_ok());
        assert!(!auto_wakes_allowed(&app));
        user_takeover(&mut app, &runtime);
        assert!(auto_wakes_allowed(&app));
    }

    #[test]
    fn only_final_done_is_success_not_tools_not_notices_not_eof() {
        assert!(capture_terminal(
            Some(&StreamEvent::Session(SessionEvent::Notice("Done".into()))),
            false
        )
        .is_none());
        assert!(capture_terminal(
            Some(&StreamEvent::Llm(synaps_cli::LlmEvent::Text("Done".into()))),
            false
        )
        .is_none());
        assert!(matches!(
            capture_terminal(Some(&StreamEvent::Session(SessionEvent::Done)), false),
            Some(Terminal::Success)
        ));
        assert!(matches!(
            capture_terminal(None, false),
            Some(Terminal::Failure(Outcome::Blocked, _))
        ));
        assert!(matches!(
            capture_terminal(Some(&StreamEvent::Session(SessionEvent::Done)), true),
            Some(Terminal::Canceled)
        ));
    }

    #[test]
    fn time_checkpoint_needs_opt_in_and_is_observed_once_not_success() {
        let runtime = Runtime::new_headless();
        let mut legacy = app();
        observe_terminal(
            &mut legacy,
            &runtime,
            Some(Terminal::Failure(
                Outcome::TimeCheckpoint,
                "wall_clock".into(),
            )),
        );
        assert!(!legacy.session_driver.is_active());
        let mut app = app();
        let mut start = serde_json::json!({"session_driver": {
            "action": "start", "run_id": "checkpoint-run",
            "models": [{"model":"example/exact-model", "effort":"high"}],
            "prompt":"continue", "delay_ms":1000, "time_checkpoint_version":1,
            "feedback_version":1
        }});
        let grant = protocol::Grant::from_start(
            "fixture",
            &app.session.id,
            protocol::parse_reply(&start).unwrap().unwrap(),
        )
        .unwrap()
        .0;
        app.session_driver.active.as_mut().unwrap().grant = grant;
        observe_terminal(
            &mut app,
            &runtime,
            Some(Terminal::Failure(
                Outcome::TimeCheckpoint,
                "wall_clock".into(),
            )),
        );
        observe_terminal(&mut app, &runtime, Some(Terminal::Success));
        let active = app.session_driver.active.as_mut().unwrap();
        assert_eq!(
            active.outcome,
            Some((Outcome::TimeCheckpoint, "wall_clock".into()))
        );
        let request = poll_request(active, Outcome::TimeCheckpoint, "wall_clock".into());
        assert_eq!(request.feedback.as_deref(), Some("unknown"));
        assert!(!active.awaiting_terminal);
        // A deliberate zero budget is never an automatic busy loop.
        start["session_driver"]["run_id"] = serde_json::json!("zero-budget");
        active.grant = protocol::Grant::from_start(
            "fixture",
            &app.session.id,
            protocol::parse_reply(&start).unwrap().unwrap(),
        )
        .unwrap()
        .0;
        active.awaiting_terminal = true;
        let mut zero = runtime.clone();
        let mut budget = zero.turn_budget().clone();
        budget.max_elapsed = Duration::ZERO;
        zero.set_turn_budget(budget);
        observe_terminal(
            &mut app,
            &zero,
            Some(Terminal::Failure(
                Outcome::TimeCheckpoint,
                "wall_clock".into(),
            )),
        );
        assert!(!app.session_driver.is_active());
    }

    #[test]
    fn typed_local_and_side_effect_errors_never_fail_over() {
        let errors = [
            agent_core::TurnError::provider("HTTP 429", "config_error", "t"),
            agent_core::TurnError::provider("HTTP 401", "session_error", "t"),
            agent_core::TurnError::provider("HTTP 503", "tool_error", "t"),
            agent_core::TurnError::interrupted_after_side_effect("call"),
            agent_core::TurnError::budget(agent_core::BudgetDimension::CostUsd),
            agent_core::TurnError {
                message: "HTTP 429".into(),
                outcome: TurnOutcome::ToolFailed {
                    tool_id: "tool".into(),
                    correlation_id: "t".into(),
                },
            },
        ];
        for error in errors {
            assert!(matches!(
                capture_terminal(
                    Some(&StreamEvent::Session(SessionEvent::Error(error))),
                    false
                ),
                Some(Terminal::Failure(Outcome::Blocked, _))
            ));
        }
    }

    #[test]
    fn error_then_done_is_observed_once_and_coarse_only() {
        let mut app = app();
        let runtime = Runtime::new_headless();
        let error = StreamEvent::Session(SessionEvent::Error(agent_core::TurnError::provider(
            "API stream error (authentication_error). Provider error details withheld — they can echo request content.",
            "api_status",
            "private-correlation",
        )));
        observe_terminal(&mut app, &runtime, capture_terminal(Some(&error), false));
        observe_terminal(
            &mut app,
            &runtime,
            capture_terminal(Some(&StreamEvent::Session(SessionEvent::Done)), false),
        );
        let active = app.session_driver.active.as_ref().unwrap();
        assert_eq!(
            active.outcome,
            Some((Outcome::ProviderError, "auth".into()))
        );
        let value =
            serde_json::to_string(&poll_request(active, Outcome::ProviderError, "auth".into()))
                .unwrap();
        assert!(!value.contains("private"));
    }

    #[test]
    fn success_has_valid_kind_and_reports_submitted_qualified_selection() {
        let mut app = app();
        observe_terminal(&mut app, &Runtime::new_headless(), Some(Terminal::Success));
        let active = app.session_driver.active.as_ref().unwrap();
        assert_eq!(active.outcome, Some((Outcome::Success, "none".into())));
        let request = poll_request(active, Outcome::Success, "none".into());
        assert_eq!(request.model, "example/exact-model");
        assert_eq!(request.effort, "high");
        assert!(uuid::Uuid::parse_str(&request.decision_id).is_ok());
    }

    #[test]
    fn opted_in_feedback_observes_only_owned_successful_turns_and_no_content_leaves() {
        let mut app = app();
        let runtime = Runtime::new_headless();
        let active = app.session_driver.active.as_mut().unwrap();
        let start = serde_json::json!({"session_driver": {
            "action":"start", "run_id":"feedback-run", "models":[active.selection.clone()],
            "prompt":"goal", "delay_ms":1000, "feedback_version":1
        }});
        active.grant = Grant::from_start(
            "fixture",
            &app.session.id,
            protocol::parse_reply(&start).unwrap().unwrap(),
        )
        .unwrap()
        .0;
        let text = StreamEvent::Llm(synaps_cli::LlmEvent::Text("private repeating text".into()));
        for expected in ["changed", "repeated"] {
            let active = app.session_driver.active.as_mut().unwrap();
            active.awaiting_terminal = true;
            active
                .feedback
                .begin_turn(&active.selection.model, &active.selection.effort);
            observe_feedback(&mut app, Some(&text));
            observe_terminal(&mut app, &runtime, Some(Terminal::Success));
            // Late events and repeated Done must not mutate a settled fingerprint.
            observe_feedback(
                &mut app,
                Some(&StreamEvent::Llm(synaps_cli::LlmEvent::Text(
                    "late private".into(),
                ))),
            );
            observe_terminal(&mut app, &runtime, Some(Terminal::Success));
            let active = app.session_driver.active.as_ref().unwrap();
            let request = poll_request(active, Outcome::Success, "none".into());
            assert_eq!(request.feedback.as_deref(), Some(expected));
            assert!(!serde_json::to_string(&request).unwrap().contains("private"));
            assert_eq!(
                poll_request(active, Outcome::ProviderError, "auth".into())
                    .feedback
                    .as_deref(),
                Some("unknown")
            );
        }
        let legacy = self::active("legacy", None);
        assert!(poll_request(&legacy, Outcome::Success, "none".into())
            .feedback
            .is_none());
    }

    #[test]
    fn rejected_selection_reports_proposal_not_unchanged_runtime() {
        let mut active = active("s", None);
        let mut p = proposal();
        p.selection = Selection {
            model: "another/rejected".into(),
            effort: "ultra".into(),
        };
        schedule(&mut active, p).unwrap();
        let one = poll_request(&active, Outcome::SelectionRejected, "unknown".into());
        let two = poll_request(&active, Outcome::SelectionRejected, "unknown".into());
        assert_eq!(one.model, "another/rejected");
        assert_eq!(one.effort, "ultra");
        assert_eq!(one.error_kind, "unknown");
        assert_ne!(one.decision_id, two.decision_id);
    }

    #[test]
    fn delays_are_bounded_and_cannot_cross_deadline() {
        let mut active = active("s", Some(2000));
        let mut crossing = proposal();
        crossing.delay = Duration::from_secs(2);
        assert!(schedule(&mut active, crossing).is_err());
        let mut p = proposal();
        p.delay = Duration::from_millis(999);
        assert!(schedule(&mut active, p).is_err());
        let mut p = proposal();
        p.delay = Duration::from_secs(301);
        assert!(schedule(&mut active, p).is_err());
    }

    #[test]
    fn cancel_and_unexpected_eof_revoke_without_outcome_or_restart() {
        for terminal in [
            Terminal::Canceled,
            Terminal::Failure(Outcome::Blocked, "unknown".into()),
        ] {
            let mut app = app();
            let ct = app
                .session_driver
                .active
                .as_ref()
                .unwrap()
                .cancel
                .child_token();
            observe_terminal(&mut app, &Runtime::new_headless(), Some(terminal));
            assert!(!app.session_driver.is_active());
            assert!(ct.is_cancelled());
            observe_terminal(&mut app, &Runtime::new_headless(), Some(Terminal::Success));
            assert!(!app.session_driver.is_active());
        }
    }

    #[test]
    fn ordinary_gates_prevent_idle_poll_but_drafts_do_not_revoke() {
        let runtime = Runtime::new_headless();
        let mut app = app();
        app.set_input_text("new user request");
        observe_terminal(&mut app, &runtime, Some(Terminal::Success));
        assert!(app.session_driver.is_active());
        assert!(idle_conflict(&app, &runtime).is_none());
        assert_eq!(app.input_text(), "new user request");
        app.clear_input();
        app.queued_message = Some("queued".into());
        assert!(idle_conflict(&app, &runtime).is_some());
        app.queued_message = None;
        app.modal_stack.push(super::super::focus::PaneId::Settings);
        assert!(idle_conflict(&app, &runtime).is_some());
    }

    #[tokio::test]
    async fn dropping_pending_task_aborts_and_invalidates_generation() {
        let mut app = app();
        let generation = app.session_driver.generation;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        app.session_driver
            .spawn(app.session.id.clone(), async move {
                let _sender = tx;
                std::future::pending::<TaskResult>().await
            });
        revoke(&mut app, "test cancellation");
        assert!(tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .is_err());
        assert_ne!(app.session_driver.generation, generation);
        assert!(!app.session_driver.is_active());
    }

    #[test]
    fn idle_escape_and_ctrl_c_outrank_registered_bindings() {
        let runtime = Runtime::new_headless();
        let registry = Arc::new(synaps_cli::skills::registry::CommandRegistry::new(
            &[],
            vec![],
        ));
        let keys = synaps_cli::skills::keybinds::KeybindRegistry::new();
        for (code, modifiers) in [
            (
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            (
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ] {
            let mut app = app();
            let action = super::super::input::handle_event(
                crossterm::event::Event::Key(crossterm::event::KeyEvent::new(code, modifiers)),
                &mut app,
                &runtime,
                false,
                &registry,
                &keys,
                3,
            );
            assert!(matches!(action, super::super::input::InputAction::Abort));
        }
    }

    // Real extension lifecycle, synthetic host results, no provider setup/inference.
    #[tokio::test]
    async fn timer_keeps_drafts_and_steering_across_pending_selection_and_poll() {
        use synaps_cli::extensions::{hooks::HookBus, manifest::ExtensionManifest};
        let dir = tempfile::tempdir().unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/extensions/autonomous");
        std::fs::copy(source.join("main.py"), dir.path().join("main.py")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(source.join(".synaps-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        let manifest: ExtensionManifest =
            serde_json::from_value(value["extension"].clone()).unwrap();
        let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
        manager
            .load_with_cwd("autonomous", &manifest, Some(dir.path().to_owned()))
            .await
            .unwrap();
        let handler = manager.session_driver_handler("autonomous").unwrap();
        let generation = live_generation(&handler).unwrap();
        let manager = Arc::new(tokio::sync::RwLock::new(manager));
        let mut runtime = Runtime::new_headless();
        let mut app = App::new(synaps_cli::Session::new(
            "example/exact-model",
            "high",
            None,
        ));
        let (secret, _rx) = tokio::sync::mpsc::unbounded_channel();
        let secret = synaps_cli::tools::SecretPromptHandle::new(secret);
        let (mut stream, mut cancel, mut steer) = (None, None, None);
        arm(
            &mut app,
            &manager,
            "autonomous".into(),
            handler,
            generation,
            Reply::Start {
                run_id: "timer-run".into(),
                models: vec![proposal().selection],
                prompt: "continue".into(),
                delay_ms: 300000,
                max_duration_ms: None,
                feedback_version: None,
                time_checkpoint_version: None,
                context_mode: Some(protocol::ContextMode::Auto),
                notice: String::new(),
            },
            &runtime,
        )
        .unwrap();
        assert!(runtime.context_management_enabled());
        app.set_input_text("draft must remain private");
        assert!(submit_steering(&mut app, "first", None));
        tick(
            &mut app,
            &mut runtime,
            &manager,
            &secret,
            &mut stream,
            &mut cancel,
            &mut steer,
        )
        .await;
        assert!(app.session_driver.is_active());
        assert_eq!(app.input_text(), "draft must remain private");
        assert!(stream.is_none());
        assert!(app.api_messages.is_empty());

        // Completion in the same interval as another human submission cannot
        // discard it or dispatch an unowned turn. A zero-send selection failure
        // still goes through exactly one plugin decision, retaining the queue.
        let p = app
            .session_driver
            .active
            .as_mut()
            .unwrap()
            .proposal
            .take()
            .unwrap()
            .proposal;
        app.session_driver
            .spawn(app.session.id.clone(), async move {
                TaskResult::Prepared {
                    proposal: p,
                    result: Box::new(Err(PrepareError::Selection("fixture".into()))),
                }
            });
        while !app
            .session_driver
            .pending
            .as_ref()
            .unwrap()
            .task
            .0
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        assert!(submit_steering(&mut app, "second", None));
        tick(
            &mut app,
            &mut runtime,
            &manager,
            &secret,
            &mut stream,
            &mut cancel,
            &mut steer,
        )
        .await;
        assert!(app.session_driver.is_active());
        assert!(app.session_driver.pending.is_some()); // policy decision, not a stream
        assert!(stream.is_none());
        assert_eq!(history_with_steering(&app).len(), 2);
        assert!(app.api_messages.is_empty());

        // Replace that synthetic poll with a deterministic proposal response.
        app.session_driver.pending = None;
        app.session_driver.spawn(app.session.id.clone(), async {
            TaskResult::Poll(Ok(Reply::Next {
                run_id: "timer-run".into(),
                selection: proposal().selection,
                prompt: "next".into(),
                delay_ms: 300000,
                notice: String::new(),
            }))
        });
        while !app
            .session_driver
            .pending
            .as_ref()
            .unwrap()
            .task
            .0
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        assert!(submit_steering(&mut app, "third", None));
        tick(
            &mut app,
            &mut runtime,
            &manager,
            &secret,
            &mut stream,
            &mut cancel,
            &mut steer,
        )
        .await;
        assert!(app
            .session_driver
            .active
            .as_ref()
            .unwrap()
            .proposal
            .is_some());
        assert_eq!(history_with_steering(&app).len(), 3);
        assert_eq!(app.input_text(), "draft must remain private");
        assert!(app.queued_message.is_none());
        assert!(stream.is_none());
        revoke(&mut app, "Esc");
        assert!(!auto_wakes_allowed(&app));
        assert!(app.input_text().contains("first\n\nsecond\n\nthird"));
        assert!(
            runtime.context_management_enabled(),
            "stop keeps the session preference"
        );
        let off = Reply::Start {
            run_id: "off-run".into(),
            models: vec![proposal().selection],
            prompt: "continue".into(),
            delay_ms: 300000,
            max_duration_ms: None,
            feedback_version: None,
            time_checkpoint_version: None,
            context_mode: Some(protocol::ContextMode::Off),
            notice: String::new(),
        };
        let handler = manager
            .read()
            .await
            .session_driver_handler("autonomous")
            .unwrap();
        let generation = live_generation(&handler).unwrap();
        // Incorrect owner/generation may not mutate even an explicit off offer.
        assert!(arm(
            &mut app,
            &manager,
            "autonomous".into(),
            handler.clone(),
            generation.wrapping_add(1),
            off.clone(),
            &runtime
        )
        .is_err());
        assert!(runtime.context_management_enabled());
        arm(
            &mut app,
            &manager,
            "autonomous".into(),
            handler,
            generation,
            off,
            &runtime,
        )
        .unwrap();
        assert!(!runtime.context_management_enabled());
        revoke(&mut app, "test done");
        manager.write().await.shutdown_all().await;
    }

    #[test]
    fn slash_prefixed_driver_prompt_is_ordinary_user_content() {
        let mut app = app();
        app.abort_context = Some("retained work".into());
        app.append_user_submission("example/exact-model", "/clear is goal text")
            .unwrap();
        assert_eq!(app.api_messages.len(), 1);
        assert_eq!(app.api_messages[0]["role"], "user");
        // Text-only normal submission may use either text or text blocks.
        let content = app.api_messages[0]["content"].to_string();
        assert!(content.contains("retained work"));
        assert!(content.contains("/clear is goal text"));
        assert!(app.session_driver.is_active());
    }
}
