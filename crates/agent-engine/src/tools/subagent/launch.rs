//! Launching a reactive worker's run (`docs/specs/subagent-context-rollover.md`
//! §3.2–3.3 and §5; slice 4 drive wiring, dark).
//!
//! `subagent_start` and `subagent_resume` hand their configured runtime's
//! stream start to [`run_worker`]. Without a [`LaunchPlan`] (a worker that
//! doesn't want rollover: [`wants_rollover`]; before slice 9, every worker,
//! with [`WORKER_ROLLOVER`] built off) it is the drive as before: nothing is claimed, created or published, a
//! context-head checkpoint is refused, and the finalizer writes the v1
//! archive. With a plan:
//!
//! 1. [`launch`] decides the run's records before its first inference. A
//!    binding that cannot confine retrieval to a lineage, a run that cannot
//!    be claimed (a busy lifecycle lock included), a spawn head that cannot
//!    be built or provably was not published: the worker runs with rollover
//!    off, as a v1 worker, never as a failed start. A spawn head that may be
//!    on disk and cannot be decided refuses the run before any inference.
//! 2. Once that is decided the run is a v2 run
//!    (`SubagentState::v2_records`): the finalizer and recovery never write
//!    its v1 archive, which would take the final's name.
//! 3. Cancellation and the run deadline (one monotonic run start, §5) are
//!    checked again after the launch, however it ended; either ends the run
//!    before its first inference. A run that fell back to rollover off keeps
//!    that deadline (no fresh start) and is archived by the finalizer as v1.
//! 4. The drive persists each rollover head
//!    ([`WorkerPersistence::on_head`]). After it, the final is published
//!    from the status the finalizer will report, the run's output and its
//!    latest history ([`WorkerPersistence::finish`]), and the handle points
//!    at it and records the run that published it
//!    (`SubagentState::v2_final_run`; collect reads a v2 final only for that
//!    run); a final that could not be published leaves the history in
//!    memory and the head as the crash checkpoint.
// Off Unix the tests are compiled out too (`mod tests` below is Unix only).
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::drive::{WorkerDrive, WorkerStream};
use super::ledger::LedgerSlot;
use super::lineage::{worker_rollover_support, OwnedHistoryGrant, SuccessorLineage};
use super::persist::{
    claim, resume_template, start_template, Begin, Terminal, WorkerPersistence, BARRIER_WAIT,
};
use super::record::{Binding, RunId, WorkerMeta};
use super::record_store::{RecordDir, RecordFs, STARTUP_WAIT};
use crate::tools::{WorkerContext, WorkerContextPolicy};
use agent_core::config::ContextRollover;
use crate::memory_backend::{MemoryBinding, OwnedHistorySupport};
use crate::runtime::budget::RunClock;
use crate::runtime::subagent::{SubagentResult, SubagentState, SubagentStatus, WorkerVisibility};
use crate::{CancellationToken, SharedMessage};

/// Worker rollover for reactive workers (`subagent_start`,
/// `subagent_resume`) and oneshot workers, built on together (slice 9): a
/// worker whose parent's live context management is auto gets a
/// [`LaunchPlan`] unless the operator or its caller opts out
/// ([`wants_rollover`]). Built off, no plan is built at all.
pub(super) const WORKER_ROLLOVER: bool = true;

/// What a run with worker rollover on is launched with.
pub(super) struct LaunchPlan {
    /// The trusted archive root (`<base>/subagent-history`).
    pub root: PathBuf,
    /// This process's directory name below the root.
    pub process_dir: String,
    pub handle_id: String,
    /// The conversation launching the run (`ToolCapabilities::session_id`;
    /// `None` for a worker's own worker): its writer-table entry names it,
    /// and so does a reload refusal (spec §5).
    pub session: Option<String>,
    /// The memory binding the worker runs on.
    pub binding: MemoryBinding,
    pub meta: WorkerMeta,
    /// A validated v2 predecessor's lineage (resume); `None` starts a fresh
    /// one (start, or a resume of a v1 archive, an in-memory history or the
    /// prior run's text).
    pub lineage: Option<SuccessorLineage>,
    /// The runtime's request-ledger slot ([`arm_request_ledger`]): filled
    /// with the run's ledger only once the run is a v2 run.
    pub ledger: LedgerSlot,
    /// The parent's context settings resolved for this worker
    /// ([`worker_context`]), applied to its runtime only once the run is a
    /// v2 run; why it runs with rollover off when they can't be.
    pub context: Result<WorkerContext, String>,
    /// The owned-history grant a resume was admitted with (§4): its
    /// worker's registry already holds that grant's tools, and a v2 run's
    /// windows grow it. `None`: a fresh lineage (start, a resume without a
    /// grant, the oneshot), whose v2 run gets a grant of its own (8g).
    pub grant: Option<Arc<OwnedHistoryGrant>>,
    /// The worker's registry as configured, for a fresh lineage's v2 run
    /// to add its own grant's tools to, and the tools the operator disabled
    /// ([`arm_owned_history`]); `None` until armed.
    pub tools: Option<WorkerTools>,
    /// A resume's listed windows the store evicted (0 = none; start and
    /// oneshot always 0): its caller sees the count whether the run goes on
    /// with rollover on or falls back to off (`SubagentState::evicted_windows`).
    pub evicted_windows: u64,
}

/// A worker's registry as configured, before its launch, and the tools the
/// operator disabled ([`arm_owned_history`]).
pub(super) struct WorkerTools {
    /// `None` for a resume admitted with a grant: its worker already runs
    /// on its owned registry.
    pub registry: Option<crate::ToolRegistry>,
    pub disabled: Vec<String>,
}

/// A v2 run's registry with its own owned-history tools (8g): the worker's
/// registry without other memory capabilities, plus `memory_search` and
/// `memory_fetch` over the run's grant
/// ([`super::retrieval::owned_worker_registry`]). Its runtime runs on it
/// from its first inference. Compared and shown by its tool names.
#[derive(Clone)]
pub(super) struct OwnedRegistry(pub(super) crate::ToolRegistry);

impl OwnedRegistry {
    /// Its tools' names, sorted.
    pub(super) fn names(&self) -> Vec<String> {
        self.0
            .iter_tools_sorted()
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }
}

impl PartialEq for OwnedRegistry {
    fn eq(&self, other: &Self) -> bool {
        self.names() == other.names()
    }
}

impl std::fmt::Debug for OwnedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OwnedRegistry").field(&self.names()).finish()
    }
}

/// What a v2 run's runtime is started with (`Launch::On` only), through
/// `Runtime::start_worker_run`: the parent's settings resolved for the
/// worker, and the run's continuation identity.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct WorkerStart {
    pub context: WorkerContext,
    /// [`worker_logical_id`](super::lineage::worker_logical_id) of the run
    /// its spawn head names: the session ID its heads must carry
    /// (`WorkerPersistence::successor`) and its windows are sealed under.
    pub logical_id: String,
    /// The run's own owned-history registry (a fresh lineage, 8g); `None`:
    /// a resume admitted with a grant, already on its owned registry.
    pub tools: Option<OwnedRegistry>,
}

/// Start a v2 run's runtime before its first inference (`Launch::On`
/// only; the start closures of start, resume and the oneshot): on its own
/// owned-history registry when it has one (8g), then bound to its run with
/// the parent's settings resolved for it
/// (`Runtime::start_worker_run`). A run without rollover gets nothing.
pub(super) fn start_v2_runtime(
    runtime: &mut crate::Runtime,
    start: WorkerStart,
    opening: &[SharedMessage],
) {
    if let Some(tools) = start.tools {
        runtime.set_tools(tools.0);
    }
    runtime.start_worker_run(&start.logical_id, &start.context, opening);
}

/// Whether a worker started or resumed under the parent settings `policy`,
/// whose caller asked for `choice`, gets a [`LaunchPlan`]: worker rollover
/// is on ([`WORKER_ROLLOVER`]) and [`rollover_wanted`]. Otherwise the
/// worker runs as before, with nothing inherited.
pub(super) fn wants_rollover(policy: Option<&WorkerContextPolicy>, choice: ContextRollover) -> bool {
    WORKER_ROLLOVER && rollover_wanted(policy, choice)
}

/// [`wants_rollover`] without the build switch: the parent's live context
/// management is `auto` and neither the operator
/// (`subagent.context_rollover`) nor the caller (`context_rollover`) opted
/// out (§2). Off from either wins over inherit.
pub(super) fn rollover_wanted(policy: Option<&WorkerContextPolicy>, choice: ContextRollover) -> bool {
    choice == ContextRollover::Inherit
        && policy.is_some_and(|p| {
            p.operator_rollover == ContextRollover::Inherit
                && p.context_management.mode == agent_core::config::ContextManagementMode::Auto
        })
}

/// What a `context_rollover` value other than `inherit` or `off` is told.
pub(super) const CONTEXT_ROLLOVER_VALUES: &str = "context_rollover must be \"inherit\" or \"off\"";

/// The caller's `context_rollover` (§2), read only when `enabled` (the
/// tools pass [`WORKER_ROLLOVER`]): absent or null is `inherit`; any value
/// but the strings "inherit" and "off" is refused, before anything starts.
/// Built off, the parameter is not read at all, so a call that passes it
/// is treated like a call with any other unknown key.
pub(super) fn context_rollover_param(
    params: &serde_json::Value,
    enabled: bool,
) -> Result<ContextRollover, String> {
    if !enabled {
        return Ok(ContextRollover::Inherit);
    }
    match params.get("context_rollover") {
        None | Some(serde_json::Value::Null) => Ok(ContextRollover::Inherit),
        Some(serde_json::Value::String(value)) => {
            ContextRollover::parse(value).ok_or_else(|| CONTEXT_ROLLOVER_VALUES.to_owned())
        }
        Some(_) => Err(CONTEXT_ROLLOVER_VALUES.to_owned()),
    }
}

/// A worker tool's parameter `schema`, with the optional
/// `context_rollover` parameter when `enabled` (the tools pass
/// [`WORKER_ROLLOVER`]): built off, no model is offered a
/// parameter that would do nothing, and the schemas stay as they were.
pub(super) fn with_rollover_param(mut schema: serde_json::Value, enabled: bool) -> serde_json::Value {
    if enabled {
        if let Some(properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            properties.insert(
                "context_rollover".into(),
                serde_json::json!({
                    "type": "string",
                    "enum": ["inherit", "off"],
                    "description": "Optional. \"off\" runs this worker without context \
                                    rollover; \"inherit\" (the default) follows this session's \
                                    context management."
                }),
            );
        }
    }
    schema
}

/// The parent settings `policy` resolved for a worker on `model` (§2,
/// §7.2), or why the worker runs with rollover off.
pub(super) fn worker_context(
    policy: Option<&WorkerContextPolicy>,
    model: &str,
) -> Result<WorkerContext, String> {
    match policy {
        Some(policy) => policy.resolve(model),
        None => Err("the parent's context settings are not available".into()),
    }
}

/// How [`launch`] decided the run's records.
pub(super) enum Launch {
    /// The spawn head is durable: the run is a v2 run, with its own
    /// owned-history registry when its lineage is fresh (8g).
    On(Box<WorkerPersistence>, Option<OwnedRegistry>),
    /// Nothing authoritative of the run is on disk: it runs as a v1 worker.
    Off(String),
    /// The spawn head may be on disk and could not be decided: no inference.
    Refused(String),
}

pub(super) fn real_fs(dir: RecordDir) -> Arc<dyn RecordFs> {
    Arc::new(dir)
}

/// Arm the run clock (§5) of a worker whose run has a `plan`, on its
/// `runtime`, before the launch: the run starts now, and the drive and the
/// runtime share its deadline, the earlier of the start plus `timeout_secs`
/// (0: none) and the start plus the worker's configured `max_elapsed`.
/// Without a plan (worker rollover off) nothing changes: a v1 run keeps the
/// drive's own timeout and the runtime's per-turn clock.
pub(super) fn arm_run_clock(
    plan: Option<&LaunchPlan>,
    runtime: &mut crate::Runtime,
    timeout_secs: u64,
) -> Option<RunClock> {
    plan?;
    Some(runtime.fix_run_clock((timeout_secs > 0).then(|| Duration::from_secs(timeout_secs))))
}

/// Arm the request-ledger slot (§6) of a worker whose run has a `plan`, on
/// its `runtime`, before the launch. The slot gets the run's ledger only
/// if the launch makes the run a v2 run; until then, and for good on a
/// fallback to rollover off, the runtime has none and its requests
/// dispatch exactly as before. Without a plan nothing changes. The only
/// place a runtime gets a request ledger.
pub(super) fn arm_request_ledger(plan: Option<&LaunchPlan>, runtime: &mut crate::Runtime) {
    if let Some(plan) = plan {
        runtime.arm_request_ledger(plan.ledger.clone());
    }
}

/// Arm the owned history (8g) of a worker whose run has a `plan`, from its
/// configured `runtime` and the operator's `config`, before the launch:
/// the tools the operator disabled, and for a fresh lineage its registry.
/// If the launch makes the run a v2 run, a fresh lineage's runtime runs on
/// a copy of this registry with its own grant's tools added ([`launch`]).
/// A fallback to rollover off and a run without a plan keep the registry
/// as it is; a resume admitted with a grant already runs on its owned
/// registry, so no registry is armed for it.
pub(super) async fn arm_owned_history(
    plan: Option<&mut LaunchPlan>,
    runtime: &crate::Runtime,
    config: &crate::config::SynapsConfig,
) {
    if let Some(plan) = plan {
        let registry = match plan.grant {
            None => Some(runtime.tools_shared().read().await.clone()),
            Some(_) => None,
        };
        plan.tools = Some(WorkerTools {
            registry,
            disabled: config.disabled_tools.clone(),
        });
    }
}

/// The owned-history tools' names.
const OWNED_TOOLS: [&str; 2] = ["memory_search", "memory_fetch"];

/// The grant a v2 run `run_id` on `binding` retrieves through, and the
/// registry its runtime runs on (8g): a resume admitted with a grant
/// (`admitted`) keeps it and its registry; a fresh lineage gets a grant of
/// its own, empty, with `support`, and a copy of its armed `tools` with
/// that grant's tools added. Why not, if they can't be installed (for
/// either: an owned-history tool the operator disabled, which a v2 run's
/// automatic context management can't run without): the run then runs
/// with rollover off, before anything is written.
fn owned_history(
    admitted: Option<Arc<OwnedHistoryGrant>>,
    tools: Option<WorkerTools>,
    binding: &MemoryBinding,
    support: OwnedHistorySupport,
    run_id: &RunId,
) -> Result<(Arc<OwnedHistoryGrant>, Option<OwnedRegistry>), String> {
    const NOT_ARMED: &str = "owned history tools could not be installed: the worker's registry was not armed";
    let tools = tools.ok_or(NOT_ARMED)?;
    if let Some(name) = OWNED_TOOLS
        .iter()
        .find(|name| tools.disabled.iter().any(|d| d == *name))
    {
        return Err(format!(
            "owned history tools could not be installed: the operator disabled {name}"
        ));
    }
    if let Some(grant) = admitted {
        return Ok((grant, None));
    }
    let registry = tools.registry.ok_or(NOT_ARMED)?;
    let grant = OwnedHistoryGrant::fresh(binding.clone(), support, run_id.clone());
    let registry = super::retrieval::owned_worker_registry(&registry, Arc::clone(&grant))?;
    Ok((grant, Some(OwnedRegistry(registry))))
}

/// Longest reason a rollover notice repeats, in characters.
const NOTICE_REASON_CHARS: usize = 200;

/// The caller-visible notice of a run that falls back to rollover off:
/// one plain line, its reason with control characters blanked and cut to
/// [`NOTICE_REASON_CHARS`].
pub(super) fn rollover_off_notice(why: &str) -> String {
    let plain: String = why
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut reason: String = plain.chars().take(NOTICE_REASON_CHARS).collect();
    if plain.chars().count() > NOTICE_REASON_CHARS {
        reason.push('…');
    }
    format!("rollover disabled for this worker: {reason}")
}

/// Decide the records of the run `plan` describes, opening with `opening`,
/// before its first inference. `deadline` is the run deadline; `cancel` the
/// worker's cancellation (it ends the wait for ownership).
pub(super) async fn launch(
    plan: LaunchPlan,
    opening: &[SharedMessage],
    deadline: Option<Instant>,
    cancel: &CancellationToken,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
) -> Launch {
    // §6: a v2 run books every request it sends in its ledger; a worker
    // whose route can't (the cloud routes, OpenAI Responses, an extension
    // provider) runs with rollover off from the start, before anything is
    // written.
    if !crate::runtime::openai::books_requests(&plan.meta.model) {
        return Launch::Off(format!(
            "'{}' can't book its requests in a run's request ledger",
            plan.meta.model
        ));
    }
    // §2: the parent's settings must resolve for the worker's window (with
    // its defaults, at worst); when they can't, the worker runs with
    // rollover off, before anything is written.
    if let Err(why) = &plan.context {
        return Launch::Off(why.clone());
    }
    let Some(support) = worker_rollover_support(&plan.binding).await else {
        return Launch::Off("the memory binding cannot confine retrieval to a lineage".into());
    };
    let binding = match Binding::of_memory(&plan.binding) {
        Ok(binding) => binding,
        Err(e) => return Launch::Off(e.to_string()),
    };
    let run_id = RunId::random();
    // 8g: the history the run's worker retrieves, its own lineage only;
    // decided before anything is written, so tools that can't be
    // installed run the worker with rollover off.
    let (grant, tools) = match owned_history(plan.grant, plan.tools, &plan.binding, support, &run_id) {
        Ok(owned) => owned,
        Err(why) => return Launch::Off(why),
    };
    let template = match plan.lineage {
        Some(lineage) => resume_template(
            run_id.clone(),
            &plan.handle_id,
            binding,
            plan.meta,
            lineage,
            opening,
        ),
        None => start_template(run_id.clone(), &plan.handle_id, binding, plan.meta, opening),
    };
    let template = match template {
        Ok(template) => template,
        Err(e) => return Launch::Off(format!("spawn head: {e}")),
    };
    let session = plan.session;
    let (dir, ownership) = match claim(
        plan.root,
        plan.process_dir,
        plan.handle_id,
        run_id,
        STARTUP_WAIT,
        cancel.clone(),
    )
    .await
    {
        Ok(claimed) => claimed,
        Err(e) => return Launch::Off(format!("ownership: {e}")),
    };
    ownership.label_session(session.as_deref());
    match WorkerPersistence::begin(wrap(dir), ownership, template, deadline).await {
        Begin::On(mut p) => {
            p.grow_into(grant);
            Launch::On(p, tools)
        }
        Begin::Off(why) => Launch::Off(why),
        Begin::Refused(why) => Launch::Refused(why),
    }
}

/// The status the finalizer will report for `outcome`: the caller and
/// `finalize_subagent` apply these rules to the state after the run (a
/// cancellation requested later is not in the record).
fn final_status(s: &SubagentState, outcome: &Result<SubagentResult, String>) -> SubagentStatus {
    if s.cancel_requested {
        return SubagentStatus::Cancelled;
    }
    match outcome {
        Err(e) => SubagentStatus::Failed(e.clone()),
        Ok(_) if s.status == SubagentStatus::Running => SubagentStatus::Completed,
        Ok(_) => s.status.clone(),
    }
}

/// Run a reactive worker: `start` starts its runtime with the
/// [`WorkerStart`] when it gets one (only a v2 run does) and starts its
/// stream with the opening messages; `plan` is `Some` only with worker
/// rollover on.
pub(super) async fn run_worker<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: Option<LaunchPlan>,
    start: S,
) -> Result<SubagentResult, String>
where
    S: FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> F,
    F: Future<Output = WorkerStream>,
{
    run_worker_configured_with(drive, opening, plan, start, &real_fs).await
}

/// [`run_worker`] writing through `wrap` (tests inject faults), for a
/// `start` that ignores the worker context.
#[cfg(test)]
pub(super) async fn run_worker_with<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: Option<LaunchPlan>,
    start: S,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
) -> Result<SubagentResult, String>
where
    S: FnOnce(Vec<SharedMessage>) -> F,
    F: Future<Output = WorkerStream>,
{
    run_worker_configured_with(drive, opening, plan, move |o, _| start(o), wrap).await
}

/// [`run_worker`] writing through `wrap`.
pub(super) async fn run_worker_configured_with<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: Option<LaunchPlan>,
    start: S,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
) -> Result<SubagentResult, String>
where
    S: FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> F,
    F: Future<Output = WorkerStream>,
{
    let Some(plan) = plan else {
        // Worker rollover off: the drive as before, nothing inherited.
        return drive.run(start(opening, None).await).await;
    };
    // One monotonic run start for the run deadline (§5): the run clock's,
    // armed on the runtime before the launch, when the run has one.
    let deadline = match drive.run_clock {
        Some(clock) => clock.deadline(),
        None => (drive.timeout_secs > 0)
            .then(|| Instant::now() + Duration::from_secs(drive.timeout_secs)),
    };
    run_worker_until(drive, opening, plan, start, wrap, deadline).await
}

/// [`run_worker_configured_with`] of a run with a plan and its run
/// `deadline`. Its caller waits for the run's terminal publication, however
/// long it takes (a reactive worker's caller is not waiting for its result;
/// see [`run_oneshot_worker`] for one that is).
pub(super) async fn run_worker_until<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: LaunchPlan,
    start: S,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
    deadline: Option<Instant>,
) -> Result<SubagentResult, String>
where
    S: FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> F,
    F: Future<Output = WorkerStream>,
{
    let (outcome, settling) =
        run_planned_worker(drive, opening, plan, start, wrap, deadline, TerminalWait::Joined).await;
    debug_assert!(settling.is_none(), "a joined terminal publication never settles later");
    outcome
}

/// Where a oneshot's run with a plan is with its terminal publication
/// (§5, slice 8c): set by the run, read by the oneshot's caller and by the
/// reaper of a dropped call. It only moves forward.
#[derive(Debug, Default)]
pub(super) struct TerminalPhase(std::sync::atomic::AtomicU8);

impl TerminalPhase {
    const FINALIZING: u8 = 1;
    const DETACHED: u8 = 2;

    fn enter(&self, phase: u8) {
        self.0.fetch_max(phase, std::sync::atomic::Ordering::AcqRel);
    }

    /// The run's terminal publication starts.
    pub(super) fn begin_finalizing(&self) {
        self.enter(Self::FINALIZING);
    }

    /// Its caller stops waiting for it; it goes on.
    pub(super) fn detach(&self) {
        self.enter(Self::DETACHED);
    }

    /// The run's terminal publication has started: its records may be
    /// written until it ends.
    pub(super) fn finalizing(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire) >= Self::FINALIZING
    }

    /// The publication outlasted the bound its caller waits
    /// ([`terminal_bound`]): the run returned its result, and the
    /// publication goes on in the task the run handed back.
    pub(super) fn detached(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire) >= Self::DETACHED
    }
}

/// How a run's caller waits for its terminal publication.
pub(super) enum TerminalWait {
    /// To its end (reactive workers).
    Joined,
    /// At most until [`terminal_bound`] (a oneshot, whose caller waits for
    /// its result), reporting how it stands in the phase.
    Bounded(Arc<TerminalPhase>),
}

/// Past the run deadline, how long a oneshot waits for its run's terminal
/// publication: its result returns within its timeout and a second (§5),
/// its stopped worker already drained at most `ONESHOT_DRAIN` (500 ms).
pub(super) const TERMINAL_GRACE: Duration = Duration::from_millis(800);

/// The least a oneshot waits for its run's terminal publication, also when
/// the deadline's grace has passed (a head write in flight at the deadline
/// was waited for first, at most [`BARRIER_WAIT`]).
pub(super) const TERMINAL_MIN: Duration = Duration::from_millis(100);

/// Until when a oneshot that ended its drive `now` waits for its run's
/// terminal publication: [`TERMINAL_GRACE`] past the run `deadline`, at
/// least [`TERMINAL_MIN`] from now, never longer than [`BARRIER_WAIT`]
/// (also without a deadline).
pub(super) fn terminal_bound(deadline: Option<Instant>, now: Instant) -> Instant {
    let patient = now + BARRIER_WAIT;
    match deadline {
        Some(at) => (at + TERMINAL_GRACE).max(now + TERMINAL_MIN).min(patient),
        None => patient,
    }
}

/// The handle's state after its run's terminal publication: where the
/// final is, or why there is none (the history stays in memory).
fn apply_terminal(state: &std::sync::RwLock<SubagentState>, terminal: Terminal) {
    let mut s = state.write().unwrap_or_else(|p| p.into_inner());
    match terminal.archive_path {
        Some(path) => {
            s.archive_path = Some(path);
            s.v2_final_run = terminal.run_id.map(String::from);
            s.archive_error = None;
            s.history = None;
        }
        None => s.archive_error = terminal.archive_error,
    }
}

/// Run a oneshot's worker with a plan (slice 8c): [`run_worker`], but its
/// caller waits for the run's terminal publication only until
/// [`terminal_bound`]. A publication that has not ended by then goes on in
/// the returned task, which applies its outcome to the run's state when it
/// ends; `phase` says where it stands. The task runs on the caller's
/// runtime, which must drive it to its end (never drop it: a record write
/// may still land).
pub(super) async fn run_oneshot_worker<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: LaunchPlan,
    start: S,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
    phase: Arc<TerminalPhase>,
) -> (Result<SubagentResult, String>, Option<tokio::task::JoinHandle<()>>)
where
    S: FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> F,
    F: Future<Output = WorkerStream>,
{
    let deadline = match drive.run_clock {
        Some(clock) => clock.deadline(),
        None => (drive.timeout_secs > 0)
            .then(|| Instant::now() + Duration::from_secs(drive.timeout_secs)),
    };
    run_planned_worker(drive, opening, plan, start, wrap, deadline, TerminalWait::Bounded(phase)).await
}

/// A run with a plan and its run `deadline`; its caller waits for its
/// terminal publication as `wait` says. Returns the run's outcome and, when
/// the publication outlasted a bounded wait, the task it goes on in.
async fn run_planned_worker<S, F>(
    drive: WorkerDrive,
    opening: Vec<SharedMessage>,
    plan: LaunchPlan,
    start: S,
    wrap: &dyn Fn(RecordDir) -> Arc<dyn RecordFs>,
    deadline: Option<Instant>,
    wait: TerminalWait,
) -> (Result<SubagentResult, String>, Option<tokio::task::JoinHandle<()>>)
where
    S: FnOnce(Vec<SharedMessage>, Option<WorkerStart>) -> F,
    F: Future<Output = WorkerStream>,
{
    let handle = plan.handle_id.clone();
    let ledger_slot = plan.ledger.clone();
    let context = plan.context.clone().ok();
    let evicted_windows = plan.evicted_windows;
    let state = Arc::clone(&drive.state);
    let (mut persistence, context) = match launch(plan, &opening, deadline, &drive.cancel, wrap).await {
        Launch::On(p, tools) => {
            let mut s = state.write().unwrap_or_else(|p| p.into_inner());
            s.v2_records = true;
            s.v2_run = p.run_id();
            // ...and the head that is its crash checkpoint until its final
            // is published (a reload carries it when it never is).
            s.v2_head = p.head_path();
            // Its caller sees its window counters and notices (§8).
            s.visibility = p.windows().map(WorkerVisibility::new);
            // ...and how many listed windows its resume found evicted.
            s.evicted_windows = evicted_windows;
            // Thresholds reset to the worker's defaults: its caller is
            // told why, in the same field as a fallback (one plain line).
            if let Some(notice) = context.as_ref().and_then(|c| c.notice.clone()) {
                s.rollover_notice = Some(notice);
            }
            drop(s);
            // A v2 run: from its first inference on, its runtime books
            // every attempt in the ledger its records snapshot.
            ledger_slot.install(p.ledger().clone());
            // ...and runs bound to its run, with the parent's settings
            // resolved for it, on its own owned history (8g).
            let start = context.zip(p.logical_id()).map(|(context, logical_id)| WorkerStart {
                context,
                logical_id,
                tools,
            });
            (Some(*p), start)
        }
        Launch::Off(why) => {
            // Nothing of the run is on disk: a v1 run from here on, which the
            // finalizer archives as always, with nothing inherited. Its
            // caller is told why.
            tracing::info!("subagent {handle}: worker rollover off for this run: {why}");
            let mut s = state.write().unwrap_or_else(|p| p.into_inner());
            s.rollover_notice = Some(rollover_off_notice(&why));
            s.evicted_windows = evicted_windows;
            drop(s);
            (None, None)
        }
        Launch::Refused(why) => {
            let mut s = state.write().unwrap_or_else(|p| p.into_inner());
            s.v2_records = true;
            s.archive_error = Some(format!(
                "publication uncertain: {why}; history retained in memory"
            ));
            return (
                Err(format!(
                    "the worker's record could not be decided ({why}); nothing was started"
                )),
                None,
            );
        }
    };
    // Checked again after the launch, before the first inference, however
    // it ended: a fallback to rollover off is no fresh start, and it keeps
    // the run deadline.
    let outcome = if drive.cancel.is_cancelled() {
        Err("cancelled before the first inference".to_owned())
    } else if deadline.is_some_and(|at| Instant::now() >= at) {
        Ok(drive.expire_before_start(persistence.as_mut()).await)
    } else {
        let stream = start(opening, context).await;
        drive
            .run_until(stream, persistence.as_mut(), deadline)
            .await
    };
    let Some(persistence) = persistence else {
        return (outcome, None);
    };
    let (status, output, history) = {
        let s = state.read().unwrap_or_else(|p| p.into_inner());
        (
            final_status(&s, &outcome),
            s.partial_text.clone(),
            s.history.clone(),
        )
    };
    let phase = match wait {
        TerminalWait::Joined => {
            let terminal = persistence
                .finish(status.as_str(), &output, history.as_deref())
                .await;
            apply_terminal(&state, terminal);
            return (outcome, None);
        }
        TerminalWait::Bounded(phase) => phase,
    };
    // The publication is the run's own task from here: a caller that stops
    // waiting for it (at the bound, or because it was dropped) never drops
    // a write that may still land.
    phase.begin_finalizing();
    let bound = terminal_bound(deadline, Instant::now());
    let mut publication = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let terminal = persistence
                .finish(status.as_str(), &output, history.as_deref())
                .await;
            apply_terminal(&state, terminal);
        })
    };
    match tokio::time::timeout_at(bound, &mut publication).await {
        Ok(Ok(())) => (outcome, None),
        Ok(Err(e)) => {
            state.write().unwrap_or_else(|p| p.into_inner()).archive_error = Some(format!(
                "publication uncertain: the final record's task ended ({e}); history retained in memory"
            ));
            (outcome, None)
        }
        Err(_) => {
            phase.detach();
            tracing::info!(
                "subagent {handle}: its final record is still being written; the result returns now and the write goes on"
            );
            (outcome, Some(publication))
        }
    }
}

#[cfg(all(test, unix))]
#[path = "launch_tests.rs"]
mod tests;
