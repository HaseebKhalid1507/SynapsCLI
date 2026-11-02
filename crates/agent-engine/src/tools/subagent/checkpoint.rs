//! A worker run's checkpoint owner (`docs/specs/subagent-context-rollover.md`
//! §3.2–3.3 and §5, slice 4 durability): publication states, the owned
//! persistence task, and finalization that never outruns an earlier write.
//!
//! One [`WorkerCheckpoint`] per run owns its [`RunOwnership`] and every
//! publication. Each runs as an owned `spawn_blocking` task. The task is
//! installed in the checkpoint *before* any await, so the checkpoint, not
//! the caller's future, owns it: a waiter that times out or is dropped
//! (cancelled) leaves it in place as a barrier ([`PublicationState::Pending`]),
//! and every other operation is refused until [`WorkerCheckpoint::quiesce`]
//! joins it and applies its outcome. Joining is the only in-process proof
//! that the task can no longer publish. States:
//!
//! - `Committed(g)`: head `g` is durable; the worker may run its next request.
//! - `PrePublicationFailure`: head `g + 1` provably never reached its name; the
//!   final carries the latest in-memory history at `g`.
//! - `Uncertain(g + 1)`: the rename or the directory sync failed, or the
//!   receipt was lost. Blocks inference and finalization;
//!   [`WorkerCheckpoint::reconcile`] re-reads the disk head: the `g + 1`
//!   candidate (run ID, generation, digest) is adopted once republished
//!   durably; `g` is kept as is and the unpublished candidate goes to the
//!   non-authoritative `<handle>.report.json` (best effort); anything else
//!   stays unresolved ("publication uncertain").
//!
//! A final is acknowledged only once durable; an uncertain final keeps the
//! head and is republished ([`WorkerCheckpoint::retry_final`]) before the head
//! is removed. Removing the head may fail harmlessly: a final supersedes heads
//! of its generation or older. Only a v2 run creates a checkpoint (worker
//! rollover, enabled since slice 9).

// Off Unix the tests are compiled out too (`mod tests` below is Unix only).
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::sync::Arc;

use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

use super::record::{
    record_file_name, Candidate, HistoryDigest, RecordError, RecordKind, WorkerRecord,
};
use super::record_store::{publish, report_file_name, Publication, RecordFs, RunOwnership};

/// A started task the checkpoint owns and has not joined yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingOp {
    /// Head `g`.
    Head(u64),
    /// Republishing the uncertain head `g` found on disk.
    Republish(u64),
    /// The non-authoritative report.
    Report,
    Final,
    /// Removing the head after a durable final.
    RemoveHead,
}

/// Where a run's publications stand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicationState {
    /// No head yet: the spawn head (generation 0) comes first.
    NoHead,
    Committed(u64),
    /// Publishing `attempted` failed before its rename; `committed` stays
    /// authoritative.
    PrePublicationFailure {
        committed: Option<u64>,
        attempted: u64,
    },
    /// A started task has not been joined: nothing else may happen until
    /// [`WorkerCheckpoint::quiesce`] joins it.
    Pending(PendingOp),
    /// Publishing `attempted` may or may not have happened.
    Uncertain {
        attempted: u64,
    },
    /// After an uncertain publication the disk still had `committed`.
    KeptAfterUncertain {
        committed: Option<u64>,
    },
    /// The disk head matches neither candidate: "publication uncertain".
    Unresolved,
    /// The final failed before its rename; finalizing may be retried.
    FinalNotPublished,
    /// The final may or may not be durable; the head is kept.
    FinalUncertain,
    /// The final is durable.
    Final,
}

/// Why a checkpoint operation was refused (nothing was started).
#[derive(Debug, PartialEq, Eq)]
pub enum CheckpointError {
    /// The record is not this run's next head (or not this run's).
    WrongRecord(&'static str),
    Invalid(RecordError),
    /// The deadline passed before the publication could start.
    DeadlinePassed,
    /// A started task has not been joined yet ([`WorkerCheckpoint::quiesce`]).
    Pending,
    /// A publication is uncertain or unresolved.
    PublicationUncertain,
    /// Not possible in the current state.
    State(PublicationState),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongRecord(why) => write!(f, "not this run's next record: {why}"),
            Self::Invalid(e) => write!(f, "invalid record: {e}"),
            Self::DeadlinePassed => f.write_str("deadline passed before the checkpoint started"),
            Self::Pending => f.write_str("a started publication has not finished"),
            Self::PublicationUncertain => f.write_str("publication uncertain"),
            Self::State(s) => write!(f, "not possible while {s:?}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

/// Outcome of publishing the final.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalOutcome {
    /// Durable; `head_removed` is false when removing the head failed
    /// (harmless).
    Durable { head_removed: bool },
    /// Failed before its rename; the head stays authoritative. May be retried.
    NotPublished(String),
    /// May or may not be durable; the head is kept until a retry succeeds.
    Uncertain(String),
}

/// What a started task does, and what its outcome decides.
enum Op {
    Head(WorkerRecord),
    Republish(WorkerRecord),
    Report,
    Final {
        record: WorkerRecord,
        /// Built from the committed head (after `KeptAfterUncertain`).
        from_committed: bool,
        /// A republication of an uncertain final.
        retry: bool,
    },
    RemoveHead,
}

struct InFlight {
    op: Op,
    task: JoinHandle<Publication>,
}

enum State {
    NoHead,
    Committed,
    PrePublicationFailure { attempted: u64 },
    Uncertain { candidate: WorkerRecord },
    KeptAfterUncertain,
    Unresolved,
    FinalNotPublished { from_committed: bool, error: String },
    FinalUncertain { record: WorkerRecord, error: String },
    Final { head_removed: bool },
}

/// The checkpoint owner of one run.
pub struct WorkerCheckpoint {
    fs: Arc<dyn RecordFs>,
    ownership: Arc<RunOwnership>,
    /// The last durably committed head.
    committed: Option<WorkerRecord>,
    state: State,
    /// The started task not joined yet; set before any await.
    in_flight: Option<InFlight>,
}

impl std::fmt::Debug for WorkerCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerCheckpoint")
            .field("ownership", &self.ownership)
            .field("state", &self.state())
            .finish()
    }
}

fn bytes_of(record: &WorkerRecord) -> Vec<u8> {
    serde_json::to_vec(record).expect("a worker record serializes")
}

fn same_head(a: &WorkerRecord, b: &WorkerRecord) -> bool {
    a.run_id == b.run_id && a.generation == b.generation && a.history_digest == b.history_digest
}

impl WorkerCheckpoint {
    pub fn new(fs: Arc<dyn RecordFs>, ownership: RunOwnership) -> Self {
        Self {
            fs,
            ownership: Arc::new(ownership),
            committed: None,
            state: State::NoHead,
            in_flight: None,
        }
    }

    pub fn state(&self) -> PublicationState {
        if let Some(flight) = &self.in_flight {
            return PublicationState::Pending(match &flight.op {
                Op::Head(record) => PendingOp::Head(record.generation),
                Op::Republish(record) => PendingOp::Republish(record.generation),
                Op::Report => PendingOp::Report,
                Op::Final { .. } => PendingOp::Final,
                Op::RemoveHead => PendingOp::RemoveHead,
            });
        }
        let committed = self.committed.as_ref().map(|r| r.generation);
        match &self.state {
            State::NoHead => PublicationState::NoHead,
            State::Committed => PublicationState::Committed(committed.unwrap_or_default()),
            State::PrePublicationFailure { attempted } => PublicationState::PrePublicationFailure {
                committed,
                attempted: *attempted,
            },
            State::Uncertain { candidate } => PublicationState::Uncertain {
                attempted: candidate.generation,
            },
            State::KeptAfterUncertain => PublicationState::KeptAfterUncertain { committed },
            State::Unresolved => PublicationState::Unresolved,
            State::FinalNotPublished { .. } => PublicationState::FinalNotPublished,
            State::FinalUncertain { .. } => PublicationState::FinalUncertain,
            State::Final { .. } => PublicationState::Final,
        }
    }

    /// The last durably committed head.
    pub fn committed(&self) -> Option<&WorkerRecord> {
        self.committed.as_ref()
    }

    /// Whether the worker may send its next request: only once its latest
    /// head is durable and no started task is unjoined (§3.3: no successor
    /// request before a durable ACK).
    pub fn may_infer(&self) -> bool {
        self.in_flight.is_none() && matches!(self.state, State::Committed)
    }

    fn idle(&self) -> Result<(), CheckpointError> {
        match self.in_flight {
            Some(_) => Err(CheckpointError::Pending),
            None => Ok(()),
        }
    }

    fn next_generation(&self) -> Result<u64, CheckpointError> {
        match &self.state {
            State::NoHead => Ok(0),
            State::Committed => Ok(self.committed.as_ref().map_or(0, |r| r.generation + 1)),
            State::Uncertain { .. } | State::Unresolved => {
                Err(CheckpointError::PublicationUncertain)
            }
            _ => Err(CheckpointError::State(self.state())),
        }
    }

    fn check_own(&self, record: &WorkerRecord) -> Result<(), CheckpointError> {
        if &record.run_id != self.ownership.run_id() {
            return Err(CheckpointError::WrongRecord("another run"));
        }
        if record.handle_id != self.ownership.handle() {
            return Err(CheckpointError::WrongRecord("another handle"));
        }
        Ok(())
    }

    /// Start `op` (`bytes` → `name`) on an owned blocking task holding the
    /// run's ownership, and install it before anything awaits.
    fn launch(&mut self, op: Op, name: String, bytes: Vec<u8>) {
        debug_assert!(self.in_flight.is_none());
        let fs = Arc::clone(&self.fs);
        let ownership = Arc::clone(&self.ownership);
        let task = tokio::task::spawn_blocking(move || {
            let _owner = ownership;
            publish(fs.as_ref(), &name, &bytes)
        });
        self.in_flight = Some(InFlight { op, task });
        self.note();
    }

    /// Publish [`Self::state`] in the run's writer-table entry, which a
    /// daemon reload reads (`record_store::writers_snapshot`). Called on
    /// every change, before anything awaits.
    fn note(&self) {
        self.ownership
            .note_state(self.committed.as_ref().map(|r| r.generation), self.state());
    }

    fn launch_head_removal(&mut self, head: String) {
        debug_assert!(self.in_flight.is_none());
        let fs = Arc::clone(&self.fs);
        let ownership = Arc::clone(&self.ownership);
        let task = tokio::task::spawn_blocking(move || {
            let _owner = ownership;
            match fs.remove(&head).and_then(|()| fs.sync_dir()) {
                Ok(()) => Publication::Durable,
                Err(e) => Publication::Uncertain(e.to_string()),
            }
        });
        self.in_flight = Some(InFlight {
            op: Op::RemoveHead,
            task,
        });
        self.note();
    }

    /// Join started tasks (and any they lead to), at most until `deadline`.
    /// True when none is left. Cancel-safe: the task stays installed until
    /// its outcome is applied, with no await in between.
    async fn join(&mut self, deadline: Option<Instant>) -> bool {
        while let Some(flight) = self.in_flight.as_mut() {
            let joined = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, &mut flight.task).await {
                    Ok(joined) => joined,
                    Err(_elapsed) => return false,
                },
                None => (&mut flight.task).await,
            };
            let flight = self.in_flight.take().expect("joined task is installed");
            self.apply(flight.op, joined);
        }
        true
    }

    fn apply(&mut self, op: Op, joined: Result<Publication, JoinError>) {
        // A lost receipt (task panic) is uncertain.
        let outcome =
            joined.unwrap_or_else(|e| Publication::Uncertain(format!("receipt lost: {e}")));
        match op {
            Op::Head(record) => {
                self.state = match outcome {
                    Publication::Durable => {
                        self.committed = Some(record);
                        State::Committed
                    }
                    Publication::NotPublished(_) => State::PrePublicationFailure {
                        attempted: record.generation,
                    },
                    Publication::Uncertain(_) => State::Uncertain { candidate: record },
                }
            }
            Op::Republish(record) => {
                self.state = match outcome {
                    Publication::Durable => {
                        self.committed = Some(record);
                        State::Committed
                    }
                    _ => State::Uncertain { candidate: record },
                }
            }
            // Best effort: the run finalizes from the kept head either way.
            Op::Report => self.state = State::KeptAfterUncertain,
            Op::Final {
                record,
                from_committed,
                retry,
            } => match outcome {
                Publication::Durable => {
                    self.state = State::Final {
                        head_removed: false,
                    };
                    let head = record_file_name(&record.handle_id, RecordKind::Head)
                        .expect("a validated handle names its head");
                    self.launch_head_removal(head);
                }
                // A failed republication leaves the earlier attempt uncertain.
                Publication::NotPublished(error) if !retry => {
                    self.state = State::FinalNotPublished {
                        from_committed,
                        error,
                    }
                }
                Publication::NotPublished(error) | Publication::Uncertain(error) => {
                    self.state = State::FinalUncertain { record, error }
                }
            },
            Op::RemoveHead => {
                self.state = State::Final {
                    head_removed: outcome == Publication::Durable,
                }
            }
        }
        self.note();
    }

    /// Publish `record` as the run's next head (generation 0 first, then
    /// the committed one plus 1), waiting at most until `deadline`. A
    /// deadline that has passed refuses before anything starts; one that
    /// passes while waiting leaves the task pending.
    pub async fn publish_head(
        &mut self,
        mut record: WorkerRecord,
        deadline: Instant,
    ) -> Result<PublicationState, CheckpointError> {
        self.idle()?;
        self.check_own(&record)?;
        let generation = self.next_generation()?;
        if record.generation != generation {
            return Err(CheckpointError::WrongRecord("not the next generation"));
        }
        record.kind = RecordKind::Head;
        record
            .validate(RecordKind::Head)
            .map_err(CheckpointError::Invalid)?;
        if Instant::now() >= deadline {
            return Err(CheckpointError::DeadlinePassed);
        }
        let name = record_file_name(&record.handle_id, RecordKind::Head)
            .map_err(CheckpointError::Invalid)?;
        let bytes = bytes_of(&record);
        self.launch(Op::Head(record), name, bytes);
        self.join(Some(deadline)).await;
        Ok(self.state())
    }

    /// Wait at most until `deadline` for started tasks to finish and apply
    /// their outcomes. True when none is left.
    pub async fn quiesce(&mut self, deadline: Instant) -> bool {
        self.join(Some(deadline)).await
    }

    /// Decide an uncertain publication from the disk head.
    pub async fn reconcile(&mut self) -> Result<PublicationState, CheckpointError> {
        self.idle()?;
        let State::Uncertain { candidate } = &self.state else {
            return Err(CheckpointError::State(self.state()));
        };
        let candidate = candidate.clone();
        let location = self
            .fs
            .location(&candidate.handle_id, RecordKind::Head)
            .map_err(|_| CheckpointError::PublicationUncertain)?;
        // Read-only: dropping this await starts nothing.
        let disk = tokio::task::spawn_blocking(move || super::record_file::load(&location))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|loaded| loaded.head);
        match disk {
            Some(Candidate::Present(head)) if same_head(&head, &candidate) => {
                let name = record_file_name(&head.handle_id, RecordKind::Head)
                    .map_err(CheckpointError::Invalid)?;
                let bytes = bytes_of(&head);
                // Adopt only once it is durable under its name.
                self.launch(Op::Republish(head), name, bytes);
            }
            Some(Candidate::Present(head))
                if self.committed.as_ref().is_some_and(|c| same_head(&head, c)) =>
            {
                self.launch_report(&candidate);
            }
            Some(Candidate::Absent) if self.committed.is_none() => self.launch_report(&candidate),
            _ => {
                self.state = State::Unresolved;
                self.note();
            }
        }
        self.join(None).await;
        Ok(self.state())
    }

    /// The evidence that could not be committed, never read as a record.
    fn launch_report(&mut self, candidate: &WorkerRecord) {
        let name = report_file_name(&candidate.handle_id);
        let body = serde_json::to_vec(&serde_json::json!({
            "report": "non-authoritative: a head whose publication was uncertain and not found on disk",
            "candidate": candidate,
        }))
        .expect("a report serializes");
        self.launch(Op::Report, name, body);
    }

    /// Publish the final. `latest` is the run's in-memory state; after an
    /// uncertain publication that kept the committed head, the final is
    /// built from that head instead (its status, output and metadata, which
    /// carries a watch lease's charge, from `latest`).
    /// Refused while a task is pending or a publication is uncertain.
    pub async fn finalize(
        &mut self,
        latest: WorkerRecord,
    ) -> Result<FinalOutcome, CheckpointError> {
        self.idle()?;
        self.check_own(&latest)?;
        let from_committed = match &self.state {
            State::Uncertain { .. } | State::Unresolved => {
                return Err(CheckpointError::PublicationUncertain)
            }
            State::FinalUncertain { .. } | State::Final { .. } => {
                return Err(CheckpointError::State(self.state()))
            }
            State::KeptAfterUncertain => true,
            State::FinalNotPublished { from_committed, .. } => *from_committed,
            State::NoHead | State::Committed | State::PrePublicationFailure { .. } => false,
        };
        let mut record = match (&self.committed, from_committed) {
            (Some(head), true) => WorkerRecord {
                status: latest.status,
                output: latest.output,
                meta: latest.meta,
                ..head.clone()
            },
            _ => latest,
        };
        record.kind = RecordKind::Final;
        record.generation = self.committed.as_ref().map_or(0, |r| r.generation);
        record.history_digest =
            HistoryDigest::of(&record.history).map_err(CheckpointError::Invalid)?;
        record
            .validate(RecordKind::Final)
            .map_err(CheckpointError::Invalid)?;
        let name = record_file_name(&record.handle_id, RecordKind::Final)
            .map_err(CheckpointError::Invalid)?;
        let bytes = bytes_of(&record);
        self.launch(
            Op::Final {
                record,
                from_committed,
                retry: false,
            },
            name,
            bytes,
        );
        self.join(None).await;
        Ok(self.final_outcome())
    }

    /// Republish an uncertain final; the head is removed only once it is
    /// durable.
    pub async fn retry_final(&mut self) -> Result<FinalOutcome, CheckpointError> {
        self.idle()?;
        let State::FinalUncertain { record, .. } = &self.state else {
            return Err(CheckpointError::State(self.state()));
        };
        let record = record.clone();
        let name = record_file_name(&record.handle_id, RecordKind::Final)
            .map_err(CheckpointError::Invalid)?;
        let bytes = bytes_of(&record);
        self.launch(
            Op::Final {
                record,
                from_committed: false,
                retry: true,
            },
            name,
            bytes,
        );
        self.join(None).await;
        Ok(self.final_outcome())
    }

    fn final_outcome(&self) -> FinalOutcome {
        match &self.state {
            State::Final { head_removed } => FinalOutcome::Durable {
                head_removed: *head_removed,
            },
            State::FinalNotPublished { error, .. } => FinalOutcome::NotPublished(error.clone()),
            State::FinalUncertain { error, .. } => FinalOutcome::Uncertain(error.clone()),
            _ => FinalOutcome::Uncertain("final not settled".into()),
        }
    }
}

#[cfg(all(test, unix))]
#[path = "checkpoint_tests.rs"]
mod tests;
