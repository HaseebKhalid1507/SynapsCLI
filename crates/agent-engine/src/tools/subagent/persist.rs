//! A worker run's record persistence at the drive
//! (`docs/specs/subagent-context-rollover.md` §3.2–3.3 and §5; slice 4 drive
//! wiring, dark).
//!
//! [`WorkerPersistence`] owns the run's [`WorkerCheckpoint`] for the reactive
//! drive:
//!
//! - **Claim** ([`claim`]): the run's ownership is taken on a blocking worker,
//!   never on the executor, waiting at most a short startup deadline for the
//!   root's lifecycle lock. Any failure means "this worker runs with rollover
//!   off", never a failed start.
//! - **Spawn head** ([`WorkerPersistence::begin`]): generation 0 is published
//!   before the first inference. Not published, or an uncertain write that the
//!   disk shows never landed, runs the worker with rollover off (nothing
//!   authoritative of it is on disk). An uncertain write that cannot be
//!   decided refuses the run before any inference.
//! - **Rollover heads** ([`WorkerPersistence::on_head`]): the runtime's
//!   context-head checkpoint is checked against this run (logical ID, exactly
//!   one marker naming the next window) and published as the next head; the
//!   receipt is acknowledged only once that head is durable; the window it
//!   adds then joins the run's owned-history grant, before the receipt is
//!   acknowledged (8g). Anything else
//!   fails the receipt, which stops the runtime before further inference. A
//!   deadline that passes while the write is in flight takes the barrier:
//!   cancel the worker, fail the receipt, wait for the write, then time out.
//!   A cancel that lands while the write is in flight fails the receipt at
//!   once and waits for the write the same way (it is never raced); a
//!   checkpoint that arrives after a cancel is refused before anything is
//!   written.
//! - **Final** ([`WorkerPersistence::finish`]): after the drive ends, started
//!   writes are joined, an uncertain head is reconciled from disk, and the
//!   final is published (an uncertain final is retried once). The final's
//!   history is the run's latest in-memory history when it continues the
//!   committed head's lineage, otherwise the committed head's own.
//!
//! Only a v2 run's launch constructs it (worker rollover, enabled since
//! slice 9).
// Off Unix the tests are compiled out too (`mod tests` below is Unix only).
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use super::checkpoint::{FinalOutcome, PublicationState, WorkerCheckpoint};
use super::ledger::RequestLedgerHandle;
use super::lineage::{
    history_matches_lineage, worker_logical_id, OwnedHistoryGrant, SuccessorLineage,
};
use super::record::{
    ArchiveId, Binding, HistoryDigest, Ledger, RecordKind, RunId, WindowRef, WorkerMeta,
    WorkerRecord, RECORD_VERSION,
};
use super::record_store::{OwnershipError, RecordDir, RecordFs, RunOwnership};
use crate::runtime::subagent::WindowCounters;
use crate::{CancellationToken, SharedMessage};
use agent_core::core::context_head::ContextHeadReceipt;

/// How long a started write is waited for at a barrier (deadline during a
/// publication, or the end of the run) before the run gives up on it. The
/// write keeps the run's ownership until it ends.
pub(super) const BARRIER_WAIT: Duration = Duration::from_secs(30);

/// Stands in for "no deadline" (`timeout_secs == 0`).
const NO_DEADLINE: Duration = Duration::from_secs(365 * 24 * 3600);

/// The receipt error of a checkpoint that reaches a worker without
/// persistence.
pub(super) const ROLLOVER_OFF: &str = "worker rollover is off";

/// Why a run could not be claimed; nothing was claimed in any case.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaimError {
    /// The process directory could not be opened or created.
    Dir(String),
    Owner(OwnershipError),
    /// The blocking task did not report back.
    Task(String),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir(e) => write!(f, "record directory: {e}"),
            Self::Owner(e) => e.fmt(f),
            Self::Task(e) => write!(f, "ownership task: {e}"),
        }
    }
}

/// Claim `run_id` (handle `handle`) in `process_dir` below `root`, on a
/// blocking worker: create the root if absent (private, as the v1 archive
/// writer does), open or create the directory, then take the run's
/// ownership, waiting at most `wait` for the root's lifecycle lock and
/// stopping early once `cancel` fires. A caller that stops waiting leaves
/// nothing claimed: the claim is released when the task's result drops.
pub(super) async fn claim(
    root: PathBuf,
    process_dir: String,
    handle: String,
    run_id: RunId,
    wait: Duration,
    cancel: CancellationToken,
) -> Result<(RecordDir, RunOwnership), ClaimError> {
    tokio::task::spawn_blocking(move || {
        super::archive::create_private_dir(&root).map_err(|e| ClaimError::Dir(e.to_string()))?;
        let dir = RecordDir::open_or_create(&root, &process_dir)
            .map_err(|e| ClaimError::Dir(e.to_string()))?;
        let owner = RunOwnership::acquire_within(&dir, &handle, &run_id, wait, &|| {
            cancel.is_cancelled()
        })
        .map_err(ClaimError::Owner)?;
        Ok((dir, owner))
    })
    .await
    .unwrap_or_else(|e| Err(ClaimError::Task(e.to_string())))
}

fn values(messages: &[SharedMessage]) -> Vec<Value> {
    messages.iter().map(|m| m.as_ref().clone()).collect()
}

/// A head of `history` with `record`'s identity and lineage, checked.
fn checked_head(mut record: WorkerRecord, history: Vec<Value>) -> Result<WorkerRecord, String> {
    record.history_digest = HistoryDigest::of(&history).map_err(|e| e.to_string())?;
    record.history = history;
    record
        .validate(RecordKind::Head)
        .map_err(|e| e.to_string())?;
    if !history_matches_lineage(&record) {
        return Err("the history's context marker does not match its lineage".into());
    }
    Ok(record)
}

fn spawn_head(
    run_id: RunId,
    handle_id: &str,
    binding: Binding,
    meta: WorkerMeta,
    window_ordinal: u64,
) -> WorkerRecord {
    WorkerRecord {
        version: RECORD_VERSION,
        run_id,
        handle_id: handle_id.to_owned(),
        kind: RecordKind::Head,
        generation: 0,
        history_digest: HistoryDigest::of(&Vec::new()).expect("an empty history digests"),
        binding,
        predecessor: None,
        windows: Vec::new(),
        inherited_windows: Vec::new(),
        window_ordinal,
        rollovers_this_run: 0,
        ledger: Ledger::default(),
        meta,
        status: "running".into(),
        output: String::new(),
        history: Vec::new(),
    }
}

/// The spawn head of a fresh run: no lineage, its opening `history`.
pub(super) fn start_template(
    run_id: RunId,
    handle_id: &str,
    binding: Binding,
    meta: WorkerMeta,
    history: &[SharedMessage],
) -> Result<WorkerRecord, String> {
    checked_head(
        spawn_head(run_id, handle_id, binding, meta, 1),
        values(history),
    )
}

/// The spawn head of a resumed run: the validated predecessor's lineage,
/// continuing its current window with the resumed `history`.
pub(super) fn resume_template(
    run_id: RunId,
    handle_id: &str,
    binding: Binding,
    meta: WorkerMeta,
    lineage: SuccessorLineage,
    history: &[SharedMessage],
) -> Result<WorkerRecord, String> {
    let mut record = spawn_head(run_id, handle_id, binding, meta, lineage.window_ordinal);
    record.predecessor = Some(lineage.predecessor);
    record.inherited_windows = lineage.inherited_windows;
    checked_head(record, values(history))
}

/// How [`WorkerPersistence::begin`] ended.
pub(super) enum Begin {
    /// The spawn head is durable: the run may start with rollover on.
    On(Box<WorkerPersistence>),
    /// Nothing authoritative of the run is on disk; it runs with rollover
    /// off (ownership released).
    Off(String),
    /// The spawn head may be on disk and could not be decided: the run must
    /// not start (no inference).
    Refused(String),
}

/// The window counters a record states (spec §8).
pub(super) fn window_counters(record: &WorkerRecord) -> WindowCounters {
    WindowCounters {
        window_ordinal: record.window_ordinal,
        rollovers_this_run: record.rollovers_this_run,
        inherited_windows: record.inherited_windows.len() as u64,
    }
}

/// How a context-head checkpoint ended. The receipt is answered in every
/// case before this returns.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HeadStep {
    /// Durable and acknowledged: the worker continues in the new window.
    Committed,
    /// Failed or refused: the runtime stops before further inference.
    Refused(String),
    /// The run deadline passed before or while the head was written: the
    /// worker was cancelled and any started write waited for. Time out now.
    DeadlineBarrier,
}

/// Where the run's terminal record ended up, for the handle's state.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Terminal {
    pub archive_path: Option<PathBuf>,
    /// The run whose final `archive_path` names.
    pub run_id: Option<RunId>,
    pub archive_error: Option<String>,
}

impl Terminal {
    fn error(reason: impl Into<String>) -> Self {
        Self {
            archive_path: None,
            run_id: None,
            archive_error: Some(reason.into()),
        }
    }
}

/// A run's checkpoint owner at the drive.
pub(super) struct WorkerPersistence {
    checkpoint: WorkerCheckpoint,
    fs: Arc<dyn RecordFs>,
    /// The run deadline on the monotonic clock; `None` = no limit.
    deadline: Option<Instant>,
    /// The run's request ledger (§6), fed by its transport; every rollover
    /// head and the final snapshot it.
    ledger: RequestLedgerHandle,
    /// A watch worker's lease used through this run (the prior runs'
    /// usage included), charged by the drive when the run ends; the final
    /// record carries it, so a resume from that record gets only what is
    /// left. `None`: no lease, or not charged.
    watch_used: Option<super::watch_admission::WatchUsage>,
    /// The owned-history grant the run's worker retrieves through (8g):
    /// each window a durable head adds joins it. `None`: none given.
    grant: Option<Arc<OwnedHistoryGrant>>,
}

impl WorkerPersistence {
    /// Publish `template` (generation 0) as the run's spawn head, waiting at
    /// most until `deadline` and then at most [`BARRIER_WAIT`] for a write
    /// still in flight.
    pub(super) async fn begin(
        fs: Arc<dyn RecordFs>,
        ownership: RunOwnership,
        template: WorkerRecord,
        deadline: Option<Instant>,
    ) -> Begin {
        let mut p = Self {
            checkpoint: WorkerCheckpoint::new(Arc::clone(&fs), ownership),
            fs,
            deadline,
            ledger: RequestLedgerHandle::new(template.meta.model.clone()),
            watch_used: None,
            grant: None,
        };
        let until = p.until();
        let state = match p.checkpoint.publish_head(template, until).await {
            Ok(state) => state,
            // Nothing was started.
            Err(e) => return Begin::Off(format!("spawn head: {e}")),
        };
        let state = match state {
            PublicationState::Pending(_) => {
                if !p.checkpoint.quiesce(Instant::now() + BARRIER_WAIT).await {
                    return Begin::Refused("spawn head: the write has not finished".into());
                }
                p.checkpoint.state()
            }
            state => state,
        };
        let state = match state {
            PublicationState::Uncertain { .. } => match p.checkpoint.reconcile().await {
                Ok(state) => state,
                Err(e) => return Begin::Refused(format!("spawn head: {e}")),
            },
            state => state,
        };
        match state {
            PublicationState::Committed(0) => Begin::On(Box::new(p)),
            PublicationState::PrePublicationFailure { .. } => {
                Begin::Off("spawn head not published".into())
            }
            // Re-read: no head of this run reached its name.
            PublicationState::KeptAfterUncertain { committed: None } => {
                Begin::Off("spawn head not published".into())
            }
            other => Begin::Refused(format!("spawn head: publication uncertain ({other:?})")),
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The owned-history grant the run's worker retrieves through: each
    /// window a durable head of the run adds joins it ([`Self::on_head`]).
    pub(super) fn grow_into(&mut self, grant: Arc<OwnedHistoryGrant>) {
        self.grant = Some(grant);
    }

    pub(super) fn state(&self) -> PublicationState {
        self.checkpoint.state()
    }

    /// The run's request ledger: the worker's model, conversation. Shared
    /// with the run's runtime (its summaries attribute their requests with
    /// [`RequestLedgerHandle::for_requests`]).
    pub(super) fn ledger(&self) -> &RequestLedgerHandle {
        &self.ledger
    }

    /// The continuation identity of the run's runtime: the logical ID of
    /// the run its durable head names, which every head it checkpoints
    /// must carry ([`Self::on_head`]).
    pub(super) fn logical_id(&self) -> Option<String> {
        self.checkpoint
            .committed()
            .map(|head| worker_logical_id(&head.run_id))
    }

    /// The run its durable head names (once its spawn head is published).
    pub(super) fn run_id(&self) -> Option<String> {
        self.checkpoint
            .committed()
            .map(|head| head.run_id.as_str().to_owned())
    }

    /// The path of the run's durable head record (once its spawn head is
    /// published): its crash checkpoint should its final never be.
    pub(super) fn head_path(&self) -> Option<PathBuf> {
        let head = self.checkpoint.committed()?;
        self.fs
            .location(&head.handle_id, RecordKind::Head)
            .ok()
            .map(|location| location.path())
    }

    /// The run's window counters from its last durable head (spec §8).
    pub(super) fn windows(&self) -> Option<WindowCounters> {
        self.checkpoint.committed().map(window_counters)
    }

    fn until(&self) -> Instant {
        self.deadline
            .unwrap_or_else(|| Instant::now() + NO_DEADLINE)
    }

    /// The successor head a context-head checkpoint describes, or why it is
    /// not this run's next head.
    fn successor(&self, session_id: &str, messages: &[SharedMessage]) -> Result<WorkerRecord, String> {
        let head = self
            .checkpoint
            .committed()
            .ok_or("no committed head")?;
        if session_id != worker_logical_id(&head.run_id) {
            return Err("not this run's logical ID".into());
        }
        let history = values(messages);
        let markers: Vec<&Value> = history
            .iter()
            .filter_map(|m| m.get("_synaps_context"))
            .collect();
        let [marker] = markers.as_slice() else {
            return Err("expected exactly one context marker".into());
        };
        let archive = marker["archive"]
            .as_str()
            .ok_or("marker without an archive")
            .and_then(|raw| ArchiveId::parse(raw).map_err(|_| "malformed marker archive"))?;
        let mut next = head.clone();
        next.windows.push(WindowRef {
            window: head.window_ordinal,
            archive_id: archive,
        });
        next.window_ordinal = head.window_ordinal + 1;
        next.rollovers_this_run = head.rollovers_this_run + 1;
        next.generation = head.generation + 1;
        next.ledger = self.ledger.snapshot();
        // Validates the lineage and that the marker names the new window.
        checked_head(next, history)
    }

    /// Persist the runtime's context-head checkpoint as the run's next head
    /// and answer `receipt`: acknowledged only once durable.
    pub(super) async fn on_head(
        &mut self,
        session_id: &str,
        messages: &[SharedMessage],
        receipt: &ContextHeadReceipt,
        cancel: &CancellationToken,
    ) -> HeadStep {
        let fail = |reason: &str| receipt.complete(Err(std::io::Error::other(reason.to_owned())));
        if cancel.is_cancelled() {
            fail("cancelled before the checkpoint started");
            return HeadStep::Refused("cancelled".into());
        }
        let record = match self.successor(session_id, messages) {
            Ok(record) => record,
            Err(why) => {
                fail(&format!("not this run's next head: {why}"));
                return HeadStep::Refused(why);
            }
        };
        // The window this head adds (`successor` appends it).
        let sealed = record.windows.last().map(|w| w.archive_id.clone());
        let until = self.until();
        // A cancel that lands while the write is started never races it
        // (§5): the publication future is dropped, its task stays a
        // barrier, the receipt is refused at once, and the run waits for
        // the write and records its outcome before it goes on stopping.
        let published = tokio::select! {
            biased;
            published = self.checkpoint.publish_head(record, until) => Some(published),
            () = cancel.cancelled() => None,
        };
        let Some(published) = published else {
            fail("cancelled during the checkpoint");
            self.checkpoint
                .quiesce(Instant::now() + BARRIER_WAIT)
                .await;
            return HeadStep::Refused("cancelled during the checkpoint".into());
        };
        let state = match published {
            Ok(state) => state,
            Err(super::checkpoint::CheckpointError::DeadlinePassed) => {
                fail("deadline passed before the checkpoint started");
                return HeadStep::DeadlineBarrier;
            }
            Err(e) => {
                let why = e.to_string();
                fail(&why);
                return HeadStep::Refused(why);
            }
        };
        match state {
            PublicationState::Committed(_) => {
                // Durable: the window joins the run's grant before the
                // receipt lets the runtime send its next request (8g).
                if let (Some(grant), Some(window)) = (&self.grant, &sealed) {
                    grant.grow(window);
                }
                acknowledge(receipt);
                HeadStep::Committed
            }
            PublicationState::Pending(_) => {
                // Barrier: stop the worker, never acknowledge, then wait for
                // the write so nothing else runs while it may still land.
                cancel.cancel();
                fail("deadline passed during the checkpoint");
                self.checkpoint
                    .quiesce(Instant::now() + BARRIER_WAIT)
                    .await;
                HeadStep::DeadlineBarrier
            }
            other => {
                let why = format!("head not durable: {other:?}");
                fail(&why);
                HeadStep::Refused(why)
            }
        }
    }

    /// Charge the watch lease this run used (the drive's charge when the
    /// run ended) to the run's final record.
    pub(super) fn charge_watch(&mut self, used: super::watch_admission::WatchUsage) {
        self.watch_used = Some(used);
    }

    /// Publish the run's final from its terminal `status`, `output` and
    /// latest in-memory `history`, after joining started writes and
    /// reconciling an uncertain head.
    pub(super) async fn finish(
        mut self,
        status: &str,
        output: &str,
        history: Option<&[SharedMessage]>,
    ) -> Terminal {
        if matches!(self.checkpoint.state(), PublicationState::Pending(_))
            && !self
                .checkpoint
                .quiesce(Instant::now() + BARRIER_WAIT)
                .await
        {
            return Terminal::error(
                "publication uncertain: a record write has not finished; history retained in memory",
            );
        }
        if matches!(self.checkpoint.state(), PublicationState::Uncertain { .. }) {
            let _ = self.checkpoint.reconcile().await;
        }
        let Some(head) = self.checkpoint.committed().cloned() else {
            return Terminal::error("no committed head; history retained in memory");
        };
        // Evidence that does not continue the committed lineage (an
        // uncertain successor the runtime never took) is not the final's.
        let latest_history = history
            .map(values)
            .filter(|h| {
                let mut candidate = head.clone();
                candidate.history = h.clone();
                history_matches_lineage(&candidate)
            })
            .unwrap_or_else(|| head.history.clone());
        // The ledger as settled when the run ended (after its drive's
        // terminal drain), open liabilities included.
        let mut latest = WorkerRecord {
            status: status.to_owned(),
            output: output.to_owned(),
            history: latest_history,
            ledger: self.ledger.snapshot(),
            ..head
        };
        // A watch worker's final carries the lease used through this run,
        // not its spawn head's (the usage when the run started).
        if let Some(used) = self.watch_used {
            latest.meta.watch_usage = Some(used.into());
        }
        let handle = latest.handle_id.clone();
        let run_id = latest.run_id.clone();
        let outcome = match self.checkpoint.finalize(latest).await {
            Ok(FinalOutcome::Uncertain(_)) => self.checkpoint.retry_final().await,
            other => other,
        };
        match outcome {
            Ok(FinalOutcome::Durable { .. }) => match self.fs.location(&handle, RecordKind::Final) {
                Ok(location) => Terminal {
                    archive_path: Some(location.path()),
                    run_id: Some(run_id),
                    archive_error: None,
                },
                Err(e) => Terminal::error(format!("final record location: {e}")),
            },
            Ok(FinalOutcome::NotPublished(e)) => Terminal::error(format!(
                "final record not published: {e}; history retained in memory"
            )),
            Ok(FinalOutcome::Uncertain(e)) => Terminal::error(format!(
                "publication uncertain: final record: {e}; history retained in memory"
            )),
            Err(e) => Terminal::error(format!("{e}; history retained in memory")),
        }
    }
}

/// Acknowledge a durable head's receipt: from here the runtime may send its
/// next request.
fn acknowledge(receipt: &ContextHeadReceipt) {
    #[cfg(test)]
    ack_probe::fire();
    receipt.complete(Ok(()));
}

/// A test's look at the run at the moment a durable head is acknowledged
/// (the receipt's completion is what wakes the runtime).
#[cfg(test)]
pub(super) mod ack_probe {
    use std::cell::RefCell;

    thread_local! {
        static PROBE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    /// Run `probe` on this thread each time a durable head is acknowledged,
    /// just before its receipt completes, until the returned guard drops.
    #[must_use]
    pub(in crate::tools::subagent) fn set(probe: impl FnMut() + 'static) -> Armed {
        PROBE.with(|p| *p.borrow_mut() = Some(Box::new(probe)));
        Armed
    }

    pub(in crate::tools::subagent) struct Armed;

    impl Drop for Armed {
        fn drop(&mut self) {
            PROBE.with(|p| *p.borrow_mut() = None);
        }
    }

    pub(super) fn fire() {
        PROBE.with(|p| {
            if let Some(probe) = p.borrow_mut().as_mut() {
                probe();
            }
        });
    }
}

#[cfg(all(test, unix))]
#[path = "persist_tests.rs"]
mod tests;
