//! Resume admission (`docs/specs/subagent-context-rollover.md` §4, slice 4):
//! what a resumed worker continues from, decided before a handle, a tree
//! reservation or a runtime exists, and so before its first inference.
//!
//! A record file is located by spelling under the trusted root
//! ([`record_file::locate`]), both siblings are read without following links
//! and resolved (§3.4), and the selected record is admitted: a v1 archive only
//! when its history is not shortened ([`admit_legacy`]); a v2 record only
//! after its lineage is checked against the store of the binding the worker
//! will run on ([`admit_record`]). A shortened v2 history comes with an
//! [`OwnedHistoryGrant`] for the owned-history tools, whether or not new
//! rollovers are on. Anything else is refused with its reason. A refusal
//! takes no new ID, tree reservation, model grant or handle; admission only
//! reads files and asks the backend (bounded). (Before admission, finding a
//! handle of this process may retry that worker's own failed archive write.)
//!
//! Writer quiescence (§3.3): a run's owner ([`super::record_store::RunOwnership`],
//! taken by its checkpoint owner) holds the run's lock file until the owner
//! and every persistence task it started are gone, and the kernel drops it
//! with the process; [`writer_state`] reports a held (or uncheckable) lock as
//! may-publish, for runs of any process. A v1 worker takes no lock: its
//! final is published once by an atomic rename, and the one v1 writer that
//! may still publish is a worker of this registry that has neither finished
//! nor published its final record, which [`writer_state`] also reports.
//! A v2 run takes ownership at its launch (worker rollover, on since slice
//! 9).

use serde_json::Value;

use super::lineage::{
    admit_legacy, admit_record, successor_lineage, Refusal, ResumeRetrieval, SuccessorLineage,
};
use super::record::{Resolution, Unresolved, WriterState};
use super::record_file::{self, RecordFileError, RecordLocation};
use crate::memory_backend::MemoryBinding;
use crate::runtime::subagent::{SubagentArchiveMeta, SubagentRegistry, SubagentStatus};

/// Shown with a resume from a head record.
pub const CRASH_CHECKPOINT_WARNING: &str = "resumed from a crash checkpoint (the worker's head \
     record): work after its last saved checkpoint may be missing";

/// A record a resume may continue from.
#[derive(Debug)]
pub struct Admitted {
    pub meta: SubagentArchiveMeta,
    pub history: Vec<Value>,
    pub retrieval: ResumeRetrieval,
    /// For the caller (a crash checkpoint may predate later work).
    pub warning: Option<&'static str>,
    /// What a rollover-enabled successor inherits: the admitted v2
    /// record's lineage; `None` for a v1 archive (a fresh lineage).
    pub lineage: Option<SuccessorLineage>,
    /// A v1 archive's recorded cache anchor (Unix ms), for a resume's cold
    /// check (#315); `None` for a v2 record (dated by its file).
    pub cache_anchor_unix_ms: Option<u64>,
}

/// Why a record cannot be resumed.
#[derive(Debug)]
pub enum AdmissionError {
    /// The path or the files it names are not acceptable.
    Path(RecordFileError),
    /// Neither a head nor a final record exists there.
    NotFound,
    /// The records on disk do not determine one state to continue from.
    Unresolved(Unresolved),
    /// The selected record failed validation before inference.
    Refused(Refusal),
    /// A legacy archive with no conversation (its worker exited before its
    /// first history checkpoint, for the reason given): resuming it would
    /// start the worker blank under a preamble claiming earlier work.
    NoConversation(String),
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(e) => write!(f, "{e}"),
            Self::NotFound => f.write_str("no worker record exists at that location"),
            Self::Unresolved(u) => {
                f.write_str("the worker's records do not determine where to continue: ")?;
                match u {
                    Unresolved::WriterMayPublish => {
                        f.write_str("the worker may still be writing them")
                    }
                    Unresolved::HeadUnreadable(e) => {
                        write!(f, "the head record is unreadable ({e})")
                    }
                    Unresolved::FinalUnreadable(e) => {
                        write!(f, "the final record is unreadable ({e})")
                    }
                    Unresolved::MixedVersions => {
                        f.write_str("a current head record sits beside a legacy final record")
                    }
                    Unresolved::RunIdMismatch => {
                        f.write_str("the head and final records belong to different runs")
                    }
                    Unresolved::HandleMismatch => {
                        f.write_str("the record belongs to another handle")
                    }
                    Unresolved::BindingMismatch => {
                        f.write_str("the head and final records name different memory bindings")
                    }
                    Unresolved::HeadNewerThanFinal {
                        head,
                        final_generation,
                    } => write!(
                        f,
                        "the head record (generation {head}) is newer than the final record \
                         (generation {final_generation})"
                    ),
                    Unresolved::Invalid { kind, error } => {
                        write!(f, "invalid {kind:?} record: {error}")
                    }
                }
            }
            Self::Refused(r) => write!(f, "{r}"),
            Self::NoConversation(why) => write!(
                f,
                "it has no conversation to resume: it exited before its first history \
                 checkpoint ({why}); start a new subagent instead"
            ),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Load, resolve and admit the records at `location`. `binding` is the
/// memory binding the resumed worker will run on; it is asked for only when
/// a v2 record is selected (a v1 archive is admitted without one).
pub async fn admit_location(
    location: &RecordLocation,
    writer: WriterState,
    binding: &(dyn Fn() -> MemoryBinding + Send + Sync),
) -> Result<Admitted, AdmissionError> {
    // The binding is not an expectation here: a v1 archive has none, and a
    // v2 record is checked against it by `admit_record` below.
    let resolution =
        record_file::resolve_location(location, writer, None).map_err(AdmissionError::Path)?;
    match resolution {
        Resolution::NotFound => Err(AdmissionError::NotFound),
        Resolution::Unresolved(reason) => Err(AdmissionError::Unresolved(reason)),
        Resolution::Legacy(archive) => {
            // A startup-failure archive holds no conversation.
            if archive.history.is_empty() {
                return Err(AdmissionError::NoConversation(
                    archive.error.clone().unwrap_or_else(|| archive.status.clone()),
                ));
            }
            let retrieval = admit_legacy(&archive).map_err(AdmissionError::Refused)?;
            let archive = *archive;
            Ok(Admitted {
                meta: archive.meta,
                history: archive.history,
                retrieval,
                warning: None,
                lineage: None,
                cache_anchor_unix_ms: archive.cache_anchor_unix_ms,
            })
        }
        Resolution::Authoritative {
            kind,
            record,
            loss_warning,
        } => {
            let retrieval = admit_record(&record, &binding())
                .await
                .map_err(AdmissionError::Refused)?;
            let lineage = successor_lineage(&record, kind)
                .map_err(|e| AdmissionError::Refused(Refusal::Invalid(e)))?;
            let record = *record;
            Ok(Admitted {
                meta: record.meta.into(),
                history: record.history,
                retrieval,
                warning: loss_warning.then_some(CRASH_CHECKPOINT_WARNING),
                lineage: Some(lineage),
                cache_anchor_unix_ms: None,
            })
        }
    }
}

/// Whether a writer of the located run may still publish: its run lock is
/// held, or (v1) it is a worker of this registry and process directory that
/// is still running, or has neither finished finalizing nor published its
/// final record.
pub fn writer_state(
    location: &RecordLocation,
    own_process_dir: &str,
    registry: &SubagentRegistry,
) -> WriterState {
    writer_state_probed(
        super::record_store::writer_probe(location),
        location,
        own_process_dir,
        registry,
    )
}

/// [`writer_state`] with the run-lock probe ([`super::record_store::writer_probe`])
/// already taken, so a caller can probe before it takes the registry guard.
/// Either order is safe: each signal alone reports a writer that may still
/// publish, and a handle id is never reused within a process.
pub fn writer_state_probed(
    probe: WriterState,
    location: &RecordLocation,
    own_process_dir: &str,
    registry: &SubagentRegistry,
) -> WriterState {
    if probe == WriterState::MayPublish {
        return WriterState::MayPublish;
    }
    if location.process_dir() != own_process_dir {
        return WriterState::Quiescent;
    }
    let Some(handle) = registry.get(location.handle()) else {
        return WriterState::Quiescent;
    };
    let running = handle.status() == SubagentStatus::Running;
    let state = handle.state.read().unwrap_or_else(|p| p.into_inner());
    if running || (!state.finalization_ready && state.archive_path.is_none()) {
        WriterState::MayPublish
    } else {
        WriterState::Quiescent
    }
}
