//! Lineage validation and owned-history admission for resumed workers
//! (`docs/specs/subagent-context-rollover.md` §4, slice 3).
//!
//! A worker record is trusted host storage, but it never grants access by
//! itself. Before a resumed worker's first inference the host checks the
//! record against the *current* memory binding and the backend's own store:
//! every listed window the store still has must be live there and must have
//! been sealed under the logical ID of the run the record names as its
//! owner. A listed window the store no longer has (evicted for capacity,
//! which leaves no tombstone) is left out, and the worker is told once
//! ([`evicted_notice`]). Only then does the worker get
//! [`OwnedHistoryGrant`], the allowlist its history tools are confined to.
//! Model text, marker IDs, ID prefixes and caller-supplied files create no
//! grants.
//!
//! The live resume path admits through [`admit_record`], [`admit_legacy`]
//! and [`admit_unlineaged`]; a fresh v2 run's grant is
//! [`OwnedHistoryGrant::fresh`] (launch).
// Off Unix the tests are compiled out too (see `mod tests` below), so the
// test-only users of these items are gone there as well.
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::sync::Arc;

use agent_core::context_archive::logical_digest;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::archive::SubagentArchive;
use super::record::{
    ArchiveId, BackendKind, Binding, InheritedRef, Predecessor, RecordError, RecordKind, RunId,
    WorkerRecord,
};
use crate::memory_backend::{MemoryBinding, OwnedHistorySupport};
use crate::runtime::continuation::MARKER;

/// Prefix of a worker run's continuation logical ID.
pub const WORKER_LOGICAL_PREFIX: &str = "worker-run:";
/// Domain prefix of [`scope_digest`].
pub const SCOPE_DIGEST_DOMAIN: &[u8] = b"synaps.worker-binding.scope.v1\0";

/// The continuation logical ID a worker run seals its windows under. The
/// store keeps only its hash ([`agent_core::context_archive::logical_digest`]).
pub fn worker_logical_id(run: &RunId) -> String {
    format!("{WORKER_LOGICAL_PREFIX}{}", run.as_str())
}

/// `sha256:<64 lowercase hex>` of [`SCOPE_DIGEST_DOMAIN`] followed by the
/// host-resolved repository scope key's UTF-8 bytes. Never the raw path.
pub fn scope_digest(scope_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCOPE_DIGEST_DOMAIN);
    hasher.update(scope_key.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

impl Binding {
    /// The binding a record must carry to be resumed under `binding`: its
    /// backend kind and the digest of its host-resolved scope key.
    pub fn of_memory(binding: &MemoryBinding) -> Result<Self, Refusal> {
        let backend = if !binding.exclusive() {
            BackendKind::Legacy
        } else if binding.is_axel() {
            BackendKind::Axel
        } else {
            return Err(Refusal::BackendUnavailable);
        };
        let scope = binding.scope().map_err(|_| Refusal::BackendUnavailable)?;
        Ok(Self {
            backend,
            scope_digest: scope_digest(scope.key()),
        })
    }
}

/// Why a resume was refused before inference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The current memory backend or its scope is unavailable.
    BackendUnavailable,
    /// The record was sealed under another backend or repository scope.
    BindingMismatch,
    /// The record itself is invalid.
    Invalid(RecordError),
    /// The history is shortened but the backend cannot confine retrieval to
    /// the lineage (for example an Axel service without the capability).
    RetrievalUnavailable,
    /// The backend could not be asked; nothing is assumed.
    Lookup(String),
    /// A listed window was forgotten; it is never re-sealed.
    Tombstoned(ArchiveId),
    /// A listed window was sealed by a run other than its listed owner.
    Foreign(ArchiveId),
    /// The context marker in the history does not match the lineage.
    Marker(&'static str),
    /// A legacy (v1) archive whose history is shortened has no lineage to
    /// validate.
    LegacyShortened,
    /// `absent` of the `listed` windows are missing from a store that has
    /// never evicted history (its eviction count is 0, or it is the legacy
    /// store, which never evicts): they were never here, so this is not the
    /// store the worker ran against. Counts only; never IDs.
    NotHeld { absent: usize, listed: usize },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BackendUnavailable => f.write_str("memory backend unavailable"),
            Self::BindingMismatch => {
                f.write_str("worker record belongs to another memory backend or repository")
            }
            Self::Invalid(e) => write!(f, "invalid worker record: {e}"),
            Self::RetrievalUnavailable => f.write_str(
                "memory backend cannot confine history retrieval to this worker's lineage",
            ),
            Self::Lookup(e) => write!(f, "could not verify worker windows: {e}"),
            Self::Tombstoned(id) => write!(f, "worker window ctx-{} was forgotten", id.as_str()),
            Self::Foreign(id) => {
                write!(
                    f,
                    "worker window ctx-{} belongs to another run",
                    id.as_str()
                )
            }
            Self::Marker(why) => write!(f, "context marker does not match the lineage: {why}"),
            Self::LegacyShortened => {
                f.write_str("legacy worker archive holds a shortened history without lineage")
            }
            Self::NotHeld { absent, listed } => write!(
                f,
                "this memory store holds {absent} of this worker's {listed} windows nowhere \
                 and has never evicted history: it is not the store the worker ran against \
                 (brain wiped, replaced or misconfigured)"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// What a resumed run inherits from a validated predecessor (§4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuccessorLineage {
    pub predecessor: Predecessor,
    /// The predecessor's inherited windows, then its own windows assigned to
    /// its run ID, ordinals unchanged.
    pub inherited_windows: Vec<InheritedRef>,
    /// The successor continues the predecessor's current window.
    pub window_ordinal: u64,
}

/// Build a successor's lineage from a predecessor record of `kind` (a head
/// is a crash checkpoint). The predecessor is re-validated first. Windows the
/// store has evicted stay listed, ordinals unchanged, so the lineage and its
/// marker still validate; the next [`admit_record`] finds them absent again
/// and leaves them out again.
pub fn successor_lineage(
    predecessor: &WorkerRecord,
    kind: RecordKind,
) -> Result<SuccessorLineage, RecordError> {
    predecessor.validate(kind)?;
    let mut inherited_windows = predecessor.inherited_windows.clone();
    inherited_windows.extend(predecessor.windows.iter().map(|w| InheritedRef {
        run_id: predecessor.run_id.clone(),
        window: w.window,
        archive_id: w.archive_id.clone(),
    }));
    Ok(SuccessorLineage {
        predecessor: Predecessor {
            run_id: predecessor.run_id.clone(),
            kind,
            generation: predecessor.generation,
            digest: predecessor.history_digest.clone(),
        },
        inherited_windows,
        window_ordinal: predecessor.window_ordinal,
    })
}

/// The windows a worker may retrieve, bound to the backend and negotiated
/// support it was validated against: a resumed worker's admitted lineage
/// ([`admit_record`]), or a fresh v2 run's own lineage, empty at its start
/// ([`Self::fresh`], 8g). A v2 run's grant grows by each window the run
/// seals, once the head that lists it is durable ([`Self::grow`]); nothing
/// else adds to it.
#[derive(Debug)]
pub struct OwnedHistoryGrant {
    binding: MemoryBinding,
    support: OwnedHistorySupport,
    /// The run the grant was made for: the admitted record's, or the fresh
    /// run's.
    run_id: RunId,
    /// Sorted, unique 32-hex archive IDs.
    allow: std::sync::RwLock<Vec<String>>,
}

impl OwnedHistoryGrant {
    /// A fresh v2 run's grant (8g): its own lineage on `binding` with the
    /// `support` negotiated for it, before its first seal. It allows no
    /// window, so a search asks the backend nothing and no fetch passes.
    pub(super) fn fresh(binding: MemoryBinding, support: OwnedHistorySupport, run_id: RunId) -> Arc<Self> {
        Arc::new(Self {
            binding,
            support,
            run_id,
            allow: Default::default(),
        })
    }

    /// Add a window the grant's run has sealed (8g): called only once the
    /// head that lists it is durable, before that head's receipt is
    /// acknowledged, so the run's next request can retrieve it. The ID is
    /// host-sealed under the run's logical ID; adding it twice changes
    /// nothing.
    pub(super) fn grow(&self, archive: &ArchiveId) {
        let mut allow = self.allow.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(at) = allow.binary_search_by(|a| a.as_str().cmp(archive.as_str())) {
            allow.insert(at, archive.as_str().to_owned());
        }
    }

    pub fn allows(&self, archive_id: &str) -> bool {
        self.allow
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binary_search_by(|a| a.as_str().cmp(archive_id))
            .is_ok()
    }
    /// The windows allowed now (sorted).
    pub fn allow(&self) -> Vec<String> {
        self.allow
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn binding(&self) -> &MemoryBinding {
        &self.binding
    }
    pub fn support(&self) -> &OwnedHistorySupport {
        &self.support
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
}

/// How a resumed worker may see its earlier context.
#[derive(Debug)]
pub enum ResumeRetrieval {
    /// The history is not shortened: nothing to retrieve.
    FullHistory,
    /// Shortened history: register the owned-history tools with `grant`,
    /// whether or not new rollovers are enabled for the successor.
    Owned {
        grant: Arc<OwnedHistoryGrant>,
        /// Listed windows the store no longer has (evicted for capacity), in
        /// lineage order: left out of `grant`; the worker is told once
        /// ([`evicted_notice`]).
        evicted: Vec<ArchiveId>,
    },
}

/// The one notice a resumed worker gets when `evicted` of its listed
/// windows are gone from the store: the worker's form of the main session's
/// [`crate::runtime::continuation::ARCHIVE_UNAVAILABLE_NOTICE`]. `None` when
/// nothing was evicted.
pub fn evicted_notice(evicted: usize) -> Option<String> {
    let (windows, verb, whose) = match evicted {
        0 => return None,
        1 => ("window", "is", "that window's"),
        _ => ("windows", "are", "those windows'"),
    };
    Some(format!(
        "[{evicted} earlier context {windows} of this worker {verb} no longer in memory \
         (removed to make room, or outside this project's memory scope). Continue from the \
         summary and retained messages you have; {whose} history can't be fetched.]"
    ))
}

/// How many listed windows an admission left out because the store evicted
/// them (0 for a full history).
pub fn evicted_count(retrieval: &ResumeRetrieval) -> usize {
    match retrieval {
        ResumeRetrieval::Owned { evicted, .. } => evicted.len(),
        ResumeRetrieval::FullHistory => 0,
    }
}

/// The parent's form of [`evicted_notice`]: a count-only warning for the
/// resume reply (never IDs, paths or content). `None` when nothing was
/// evicted.
pub fn evicted_parent_note(evicted: usize) -> Option<String> {
    let (windows, verb) = match evicted {
        0 => return None,
        1 => ("window", "is"),
        _ => ("windows", "are"),
    };
    Some(format!(
        "{evicted} earlier context {windows} of the resumed worker {verb} no longer in \
         memory; it continues from its summary"
    ))
}

/// Admit a legacy (v1) archive: full history, no lineage. A context marker
/// would mean a shortened history nobody can validate, so it is refused.
pub fn admit_legacy(archive: &SubagentArchive) -> Result<ResumeRetrieval, Refusal> {
    admit_unlineaged(&archive.history)
}

/// Admit a history that has no record or lineage (a v1 archive, or a
/// finished worker's history kept in memory because its archive could not
/// be written): only when it is not shortened.
pub fn admit_unlineaged(history: &[Value]) -> Result<ResumeRetrieval, Refusal> {
    if markers(history).is_empty() {
        Ok(ResumeRetrieval::FullHistory)
    } else {
        Err(Refusal::LegacyShortened)
    }
}

/// Validate a resolved v2 record against the current binding and its store
/// before any inference (§4 "Validation before inference"); also when new
/// rollovers are off. Returns the retrieval the successor gets. A listed
/// window absent from a successful inspection is evicted: left out of the
/// grant and returned in `evicted` (even the latest, even all of them, which
/// leaves an empty owned grant, never [`ResumeRetrieval::FullHistory`]),
/// unless the store reports it has never evicted history (legacy, or an Axel
/// eviction count of 0): then absent windows were never here and the resume
/// is refused as [`Refusal::NotHeld`]. A failed inspection is
/// [`Refusal::Lookup`], never eviction.
pub async fn admit_record(
    record: &WorkerRecord,
    binding: &MemoryBinding,
) -> Result<ResumeRetrieval, Refusal> {
    record.validate(record.kind).map_err(Refusal::Invalid)?;
    if Binding::of_memory(binding)? != record.binding {
        return Err(Refusal::BindingMismatch);
    }
    // Every sealed window of the lineage with the run that sealed it:
    // inherited (oldest run first), then this run's own.
    let refs: Vec<(&ArchiveId, &RunId)> = record
        .inherited_windows
        .iter()
        .map(|w| (&w.archive_id, &w.run_id))
        .chain(
            record
                .windows
                .iter()
                .map(|w| (&w.archive_id, &record.run_id)),
        )
        .collect();
    check_marker(record, refs.last().map(|(id, _)| *id))?;
    if refs.is_empty() {
        return Ok(ResumeRetrieval::FullHistory);
    }
    let support = binding
        .owned_history_support()
        .await
        .map_err(|e| Refusal::Lookup(e.to_string()))?
        .ok_or(Refusal::RetrievalUnavailable)?;
    let ids: Vec<String> = refs.iter().map(|(id, _)| id.as_str().to_owned()).collect();
    let inspection = binding
        .history_inspect(&support, &ids)
        .await
        .map_err(|e| Refusal::Lookup(e.to_string()))?;
    let entries = &inspection.entries;
    // A window absent from a successful inspection was evicted for capacity
    // (the store deletes the row, leaving no tombstone): it is left out of
    // the grant, never allowed. Every window still present must pass the
    // owner and liveness checks; one that fails refuses the whole resume.
    let mut allow = Vec::with_capacity(refs.len());
    let mut evicted = Vec::new();
    for (id, owner) in &refs {
        let Some(entry) = entries.iter().find(|e| e.id == id.as_str()) else {
            evicted.push((*id).clone());
            continue;
        };
        let expected = logical_digest(&worker_logical_id(owner))
            .map_err(|e| Refusal::Lookup(e.to_string()))?;
        if entry.logical_id != expected {
            return Err(Refusal::Foreign((*id).clone()));
        }
        if !entry.live {
            return Err(Refusal::Tombstoned((*id).clone()));
        }
        allow.push(id.as_str().to_owned());
    }
    // Absent windows in a store that has never evicted (count 0; the legacy
    // store never evicts) were never stored here: not eviction but the wrong
    // store. An unknown count (`None`) keeps the eviction reading.
    if !evicted.is_empty() && inspection.evictions == Some(0) {
        tracing::info!(
            handle = record.handle_id.as_str(),
            run = record.run_id.as_str(),
            listed = refs.len(),
            absent = evicted.len(),
            store_evictions = 0u64,
            "worker resume: lineage refused, windows not held (absent={} listed={})",
            evicted.len(),
            refs.len()
        );
        return Err(Refusal::NotHeld {
            absent: evicted.len(),
            listed: refs.len(),
        });
    }
    allow.sort_unstable();
    let evicted_ids: Vec<&str> = evicted.iter().map(ArchiveId::as_str).collect();
    tracing::info!(
        handle = record.handle_id.as_str(),
        run = record.run_id.as_str(),
        listed = refs.len(),
        evicted = evicted.len(),
        evicted_ids = ?evicted_ids,
        store_evictions = ?inspection.evictions,
        "worker resume: lineage admitted (evicted={} listed={})",
        evicted.len(),
        refs.len()
    );
    Ok(ResumeRetrieval::Owned {
        grant: Arc::new(OwnedHistoryGrant {
            binding: binding.clone(),
            support,
            run_id: record.run_id.clone(),
            allow: std::sync::RwLock::new(allow),
        }),
        evicted,
    })
}

/// §4: the latest marker archive equals the record's latest window. A record
/// with sealed windows holds exactly one marker of the current schema whose
/// archive is the last listed window and whose window is the current
/// ordinal; a pinned first-request stub must name a listed window. A record
/// without sealed windows holds no marker.
fn check_marker(record: &WorkerRecord, latest: Option<&ArchiveId>) -> Result<(), Refusal> {
    let found = markers(&record.history);
    let Some(latest) = latest else {
        return if found.is_empty() {
            Ok(())
        } else {
            Err(Refusal::Marker("a marker without any sealed window"))
        };
    };
    let [marker] = found.as_slice() else {
        return Err(Refusal::Marker("expected exactly one marker"));
    };
    if marker["schema"] != MARKER {
        return Err(Refusal::Marker("unknown marker schema"));
    }
    if marker["archive"].as_str() != Some(latest.as_str()) {
        return Err(Refusal::Marker("marker archive is not the latest window"));
    }
    if marker["window"].as_u64() != Some(record.window_ordinal) {
        return Err(Refusal::Marker("marker window is not the current ordinal"));
    }
    if let Some(stub) = marker.get("pinned_stub") {
        let listed = |id: &str| {
            record
                .inherited_windows
                .iter()
                .map(|w| &w.archive_id)
                .chain(record.windows.iter().map(|w| &w.archive_id))
                .any(|a| a.as_str() == id)
        };
        if !stub["archive"].as_str().is_some_and(listed) {
            return Err(Refusal::Marker("pinned stub names an unlisted window"));
        }
    }
    Ok(())
}

/// Whether `record`'s history carries exactly the context marker its lineage
/// requires (§4; the same check as [`admit_record`]): none without sealed
/// windows, otherwise one naming the latest window and the current ordinal.
pub fn history_matches_lineage(record: &WorkerRecord) -> bool {
    let latest = record
        .windows
        .last()
        .map(|w| &w.archive_id)
        .or_else(|| record.inherited_windows.last().map(|w| &w.archive_id));
    check_marker(record, latest).is_ok()
}

/// Whether new worker rollovers may run on this binding: only when its
/// backend can confine retrieval to a lineage (older Axel cannot).
pub async fn worker_rollover_supported(binding: &MemoryBinding) -> bool {
    worker_rollover_support(binding).await.is_some()
}

/// [`worker_rollover_supported`], with the support it negotiated: what a
/// fresh v2 run's own grant is made with (8g).
pub async fn worker_rollover_support(binding: &MemoryBinding) -> Option<OwnedHistorySupport> {
    if Binding::of_memory(binding).is_err() {
        return None;
    }
    binding.owned_history_support().await.ok().flatten()
}

fn markers(history: &[Value]) -> Vec<&Value> {
    history
        .iter()
        .filter_map(|m| m.get("_synaps_context"))
        .collect()
}

// Unix only, like the other record tests: the fake Axel is a script run by
// path, and the record-file tests use modes, symlinks, flock and a FIFO.
#[cfg(all(test, unix))]
#[path = "lineage_tests.rs"]
mod tests;
