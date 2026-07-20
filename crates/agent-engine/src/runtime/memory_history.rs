//! Existing-history disclosure preview and consent gate (task D1).
//!
//! The host owns metadata discovery, disclosure policy, and authority. Both
//! `/memory index-history` and the `memory_context(action=index_history)` tool
//! enter through this engine; D2 will consume the typed [`ImportPlan`].
//!
//! This module deliberately contains no importer. A successful confirmation
//! returns a plan and performs no session-content reads or Axel writes.

use crate::runtime::memory_context::UserIntentProof;
use std::path::PathBuf;

/// Metadata that may be inspected before the user decides whether to import.
/// It must not contain message, prompt, or tool-result bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySessionMetadata {
    pub id: String,
    pub approx_bytes: u64,
    /// RFC3339 timestamp supplied by the canonical session metadata API.
    pub started_at: String,
}

/// Narrow I/O boundary for history import. D1 calls only `scan_metadata`;
/// content reads and provider writes are reserved for D2 consuming a plan.
pub trait HistoryImportIo {
    fn scan_metadata(&mut self) -> Result<Vec<HistorySessionMetadata>, HistoryImportError>;
    fn read_session_content(&mut self, id: &str) -> Result<(), HistoryImportError>;
    fn write_axel(&mut self) -> Result<(), HistoryImportError>;
}

/// Trusted, host-computed scope and destination. No field comes from model
/// parameters or a provider response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryImportHostState {
    pub project_id: String,
    pub project_root: PathBuf,
    pub destination_r8_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedDateRange {
    pub earliest: String,
    pub latest: String,
}

/// Complete disclosure presented before consent is requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryImportPreview {
    pub project_id: String,
    pub project_root: PathBuf,
    pub session_count: usize,
    pub approx_bytes: u64,
    pub included_date_range: Option<IncludedDateRange>,
    pub included_content_classes: Vec<&'static str>,
    pub excluded_content_classes: Vec<&'static str>,
    pub retention_policy: String,
    pub redaction_policy: String,
    pub destination_r8_path: PathBuf,
    pub explicit_confirmation_required: bool,
}

impl HistoryImportPreview {
    /// Stable frontend-neutral rendering of every mandatory disclosure field.
    pub fn render(&self) -> String {
        let date_range = self
            .included_date_range
            .as_ref()
            .map(|range| format!("{} to {}", range.earliest, range.latest))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "History import preview\n\
             project: {}\n\
             root: {}\n\
             sessions: {}\n\
             approximate bytes: {}\n\
             included date range: {}\n\
             included classes: {}\n\
             excluded classes: {}\n\
             retention policy: {}\n\
             redaction policy: {}\n\
             destination .r8: {}\n\
             explicit confirmation required: {}\n\
             Run `/memory index-history confirm` to create an import plan; no import has started.",
            self.project_id,
            self.project_root.display(),
            self.session_count,
            self.approx_bytes,
            date_range,
            self.included_content_classes.join(", "),
            self.excluded_content_classes.join(", "),
            self.retention_policy,
            self.redaction_policy,
            self.destination_r8_path.display(),
            self.explicit_confirmation_required,
        )
    }
}

/// Confirmation authority is represented by the same unforgeable A4 proof
/// type used by memory leases. There is intentionally no string constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedUserIntent {
    proof: UserIntentProof,
}

impl ConfirmedUserIntent {
    /// Host-only constructor for a slash-command confirmation. Frontends call
    /// this only after presenting the preview; model JSON cannot construct a
    /// `RequestId` or reach this crate-private gate.
    pub(crate) fn from_explicit_command(
        command_id: crate::runtime::memory_context::RequestId,
    ) -> Self {
        Self {
            proof: UserIntentProof::ExplicitCommand { command_id },
        }
    }

    /// Host-only constructor for an affirmative confirmation prompt.
    #[cfg(test)]
    pub(crate) fn from_confirmed_prompt(
        confirmation_id: crate::runtime::memory_context::ConfirmationId,
    ) -> Self {
        Self {
            proof: UserIntentProof::ConfirmedPrompt { confirmation_id },
        }
    }

    fn confirmation_id(&self) -> &str {
        match &self.proof {
            UserIntentProof::ExplicitCommand { command_id } => command_id.as_str(),
            UserIntentProof::ConfirmedPrompt { confirmation_id } => confirmation_id.as_str(),
            UserIntentProof::ExactCurrentRequest { .. } => "exact-current-request",
        }
    }

    #[cfg(test)]
    fn for_test(confirmation_id: &str) -> Self {
        use crate::runtime::memory_context::ConfirmationId;
        Self::from_confirmed_prompt(
            ConfirmationId::parse(confirmation_id).expect("test confirmation id is valid"),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryImportConsent {
    Declined,
    Confirmed(ConfirmedUserIntent),
}

/// Identifies the caller, independently of any claimed consent payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAuthority {
    UserFrontend,
    ModelTool,
}

/// D2 input: an already disclosed preview bound to explicit user intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    pub preview: HistoryImportPreview,
    pub confirmation_id: String,
    pub user_intent: UserIntentProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // public API kept stable; plans are short-lived consent tokens
pub enum HistoryImportOutcome {
    Declined,
    Ready(ImportPlan),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryImportError {
    #[error("a model tool call may propose history import but cannot confirm it")]
    ModelCannotConfirm,
    #[error("history import requires explicit user confirmation after preview")]
    ConsentRequired,
    #[error("history metadata scan failed")]
    MetadataScanFailed,
    #[error("history import host scope is unavailable")]
    HostStateUnavailable,
    #[error("history import batch size is outside the host bound")]
    InvalidBatchSize,
    #[error("history import session index could not be read")]
    SessionIndexReadFailed,
    #[error("history import session could not be loaded")]
    SessionLoadFailed,
    #[error("history import identity is invalid")]
    InvalidIdentity,
    #[error("history import capture could not be built")]
    CaptureBuildFailed,
    #[error("history import requires a live capture lease")]
    CaptureLeaseUnavailable,
    #[error("history import requires the leased local provider")]
    CaptureProviderUnavailable,
    #[error("history import capture queue is full")]
    CaptureQueueFull,
    #[error("history import was cancelled")]
    Cancelled,
    #[error("history import checkpoint could not be read")]
    CheckpointReadFailed,
    #[error("history import checkpoint could not be committed")]
    CheckpointWriteFailed,
    #[error("history import provider capture failed")]
    ProviderCaptureFailed,
}

/// Hard ceiling on records accumulated before dispatch. The C3 worker queue is
/// independently fixed-capacity and nonblocking; imports can therefore bound
/// both their producer batch and consumer queue.
pub const IMPORT_BATCH_MAX_RECORDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryImportReport {
    pub sessions_loaded: usize,
    pub captures_built: usize,
    pub batches_submitted: usize,
    pub ranges_skipped: usize,
}

/// Maximum durable host state for one project import. One digest is 64 bytes;
/// this supports thousands of committed source ranges without unbounded growth.
pub const CHECKPOINT_FILE_MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct HistoryImportCancellation(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl HistoryImportCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Content-free event envelope for frontends and diagnostics. The payload type
/// intentionally has no content, session id, project id, or filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HistoryImportEvent {
    pub name: &'static str,
    pub status: &'static str,
    pub ranges_committed: usize,
    pub ranges_total: usize,
    pub bytes_processed: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
}

impl HistoryImportEvent {
    pub fn progress(
        ranges_committed: usize,
        ranges_total: usize,
        bytes_processed: u64,
        duration: std::time::Duration,
    ) -> Self {
        Self {
            name: "memory_import.progress",
            status: "running",
            ranges_committed,
            ranges_total,
            bytes_processed,
            duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
            error_code: None,
        }
    }

    pub fn error(
        ranges_committed: usize,
        bytes_processed: u64,
        duration: std::time::Duration,
        error: HistoryImportError,
    ) -> Self {
        Self {
            name: "memory_import.progress",
            status: "error",
            ranges_committed,
            ranges_total: 0,
            bytes_processed,
            duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
            error_code: Some(error.code()),
        }
    }
}

impl HistoryImportError {
    fn code(&self) -> &'static str {
        match self {
            Self::ModelCannotConfirm => "model_cannot_confirm",
            Self::ConsentRequired => "consent_required",
            Self::MetadataScanFailed => "metadata_scan_failed",
            Self::HostStateUnavailable => "host_state_unavailable",
            Self::InvalidBatchSize => "invalid_batch_size",
            Self::SessionIndexReadFailed => "session_index_read_failed",
            Self::SessionLoadFailed => "session_load_failed",
            Self::InvalidIdentity => "invalid_identity",
            Self::CaptureBuildFailed => "capture_build_failed",
            Self::CaptureLeaseUnavailable => "capture_lease_unavailable",
            Self::CaptureProviderUnavailable => "capture_provider_unavailable",
            Self::CaptureQueueFull => "capture_queue_full",
            Self::Cancelled => "cancelled",
            Self::CheckpointReadFailed => "checkpoint_read_failed",
            Self::CheckpointWriteFailed => "checkpoint_write_failed",
            Self::ProviderCaptureFailed => "provider_capture_failed",
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ImportCheckpoint {
    version: u8,
    key: String,
    committed_source_digests: Vec<String>,
}

#[cfg(any(test, feature = "testing"))]
pub fn authorize_import_for_tests(
    preview: HistoryImportPreview,
) -> (
    ImportPlan,
    crate::runtime::memory_context::MemoryContextLease,
) {
    use crate::runtime::memory_context::{
        mint_explicit_command_proof, CapturePolicy, ContextProviderId, MemoryContextLease,
        MemoryContextMode, MemoryLeaseId, ProjectId, RecallPolicy, SessionId,
    };

    let intent = mint_explicit_command_proof();
    let plan = ImportPlan {
        preview: preview.clone(),
        confirmation_id: "history-import-test-confirmation".into(),
        user_intent: intent.clone(),
    };
    let lease = MemoryContextLease::grant(
        MemoryLeaseId::parse("history-import-test-lease").expect("static lease id"),
        SessionId::parse("history-import-test-session").expect("static session id"),
        ProjectId::parse(&preview.project_id).expect("test project id must be valid"),
        ContextProviderId::parse("extension:test:memory").expect("static provider id"),
        MemoryContextMode::CaptureOnly,
        CapturePolicy::default(),
        RecallPolicy::default(),
        intent,
        std::time::SystemTime::now(),
        None,
    )
    .expect("test lease must be valid");
    (plan, lease)
}

/// D2 host-only import path. Session scope comes from the host session index;
/// a foreign ID is rejected before its session artifact is opened. Session
/// bodies then load exclusively through [`agent_core::session::Session`]'s
/// canonical legacy+journal API and become C1-shaped, bounded captures sent
/// through the existing C3 worker/provider seam.
pub(crate) fn import_history_from_dir(
    plan: &ImportPlan,
    lease: &crate::runtime::memory_context::MemoryContextLease,
    sessions_dir: &std::path::Path,
    batch_size: usize,
    worker: &crate::runtime::capture_worker::CaptureWorker,
    provider: std::sync::Arc<dyn crate::runtime::capture_worker::CaptureProvider>,
) -> Result<HistoryImportReport, HistoryImportError> {
    use crate::runtime::chat_capture::{
        build_chat_turn_capture, CanonicalCaptureItem, CaptureContentClass, Sensitivity,
        TerminalTurnHistory,
    };
    use crate::runtime::memory_context::{ProjectId, RetentionClass, SessionId, TurnId};
    use agent_core::core::disclosure::{
        gate_for_model, may_persist, DisclosureClass, ModelVisibility,
    };
    use agent_core::core::session_index::{SessionIndexEventKind, SessionIndexRecord};
    use std::collections::HashSet;
    use std::io::BufRead;
    use std::time::{Duration, SystemTime};

    if batch_size == 0 || batch_size > IMPORT_BATCH_MAX_RECORDS {
        return Err(HistoryImportError::InvalidBatchSize);
    }
    if lease.project_id.as_str() != plan.preview.project_id {
        return Err(HistoryImportError::InvalidIdentity);
    }

    // Scope first, content second: the index contains metadata only and lets
    // us exclude foreign project sessions without probing their body files.
    let index = match std::fs::File::open(sessions_dir.join("index.jsonl")) {
        Ok(index) => index,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HistoryImportReport::default())
        }
        Err(_) => return Err(HistoryImportError::SessionIndexReadFailed),
    };
    let mut scoped = HashSet::new();
    for line in std::io::BufReader::new(index).lines() {
        let line = line.map_err(|_| HistoryImportError::SessionIndexReadFailed)?;
        let Ok(record) = serde_json::from_str::<SessionIndexRecord>(&line) else {
            continue;
        };
        if record.event == SessionIndexEventKind::Start
            && record.cwd.as_deref() == Some(plan.preview.project_root.as_path())
        {
            scoped.insert(record.session_id);
        }
    }

    let project_id = ProjectId::parse(&plan.preview.project_id)
        .map_err(|_| HistoryImportError::InvalidIdentity)?;
    let redactor = |text: &str| redact_import_text(text);
    let mut captures = Vec::with_capacity(batch_size);
    let mut report = HistoryImportReport::default();

    // Only enumerate names; full bodies are opened after the scope check and
    // only by Session::load_from_dir (the canonical compatibility boundary).
    let mut ids: Vec<String> = agent_core::session_journal::session_dir_entries(sessions_dir)
        .map_err(|_| HistoryImportError::SessionIndexReadFailed)?
        .into_iter()
        .filter_map(|entry| entry.name.strip_suffix(".json").map(str::to_owned))
        .filter(|id| scoped.contains(id))
        .collect();
    ids.sort();

    for id in ids {
        let session = agent_core::session::Session::load_from_dir(sessions_dir, &id)
            .map_err(|_| HistoryImportError::SessionLoadFailed)?;
        report.sessions_loaded += 1;
        let session_id =
            SessionId::parse(&session.id).map_err(|_| HistoryImportError::InvalidIdentity)?;
        let started_at: SystemTime = session.created_at.into();
        let completed_at: SystemTime = session.updated_at.into();

        let mut pending_user: Option<String> = None;
        let mut ordinal = 0_u64;
        for message in &session.api_messages {
            let role = message.get("role").and_then(serde_json::Value::as_str);
            let Some(text) = import_message_text(message) else {
                continue;
            };
            match role {
                Some("user") => pending_user = Some(text),
                Some("assistant") => {
                    let Some(user) = pending_user.take() else {
                        continue;
                    };
                    ordinal = ordinal.saturating_add(1);
                    let disclosure = DisclosureClass::ModelVisibleAfterRedaction;
                    if !may_persist(disclosure) {
                        continue;
                    }
                    let user = match gate_for_model(disclosure, &user, false, Some(&redactor)) {
                        ModelVisibility::Visible(text) => text,
                        ModelVisibility::Withheld(_) => continue,
                    };
                    let assistant = match gate_for_model(disclosure, &text, false, Some(&redactor))
                    {
                        ModelVisibility::Visible(text) => text,
                        ModelVisibility::Withheld(_) => continue,
                    };
                    let history = TerminalTurnHistory {
                        project_id: project_id.clone(),
                        session_id: session_id.clone(),
                        turn_id: TurnId::parse(&format!("import-{ordinal}"))
                            .map_err(|_| HistoryImportError::InvalidIdentity)?,
                        turn_ordinal: ordinal,
                        started_at,
                        completed_at: completed_at.max(started_at + Duration::from_nanos(1)),
                        outcome: agent_core::TurnOutcome::Completed,
                        items: vec![
                            CanonicalCaptureItem {
                                project_id: project_id.clone(),
                                class: CaptureContentClass::UserMessage,
                                disclosure,
                                sensitivity: Sensitivity::Normal,
                                text: user,
                                tool_name: None,
                            },
                            CanonicalCaptureItem {
                                project_id: project_id.clone(),
                                class: CaptureContentClass::AssistantFinal,
                                disclosure,
                                sensitivity: Sensitivity::Normal,
                                text: assistant,
                                tool_name: None,
                            },
                        ],
                        compaction: None,
                    };
                    captures.push(
                        build_chat_turn_capture(&project_id, history, RetentionClass::Standard)
                            .map_err(|_| HistoryImportError::CaptureBuildFailed)?,
                    );
                    report.captures_built += 1;
                    if captures.len() == batch_size {
                        submit_import_batch(worker, lease, provider.clone(), &mut captures)?;
                        report.batches_submitted += 1;
                    }
                }
                _ => {} // system/developer/tool/raw content is excluded.
            }
        }
    }
    if !captures.is_empty() {
        submit_import_batch(worker, lease, provider, &mut captures)?;
        report.batches_submitted += 1;
    }
    Ok(report)
}

/// Incremental D3 import. A source range is checkpointed only after the C3
/// worker receives a provider acknowledgement (or idempotency reconciliation).
/// Therefore a process killed between provider commit and checkpoint rename may
/// replay at most one stable capture id, which the provider store deduplicates.
#[allow(clippy::too_many_arguments)] // resumable import transaction dependencies remain explicit
pub fn import_history_resumable_from_dir(
    plan: &ImportPlan,
    lease: &crate::runtime::memory_context::MemoryContextLease,
    sessions_dir: &std::path::Path,
    checkpoint_path: &std::path::Path,
    batch_size: usize,
    worker: &crate::runtime::capture_worker::CaptureWorker,
    provider: std::sync::Arc<dyn crate::runtime::capture_worker::CaptureProvider>,
    cancellation: &HistoryImportCancellation,
    emit: &mut impl FnMut(HistoryImportEvent),
) -> Result<HistoryImportReport, HistoryImportError> {
    use crate::runtime::chat_capture::{
        build_chat_turn_capture, CanonicalCaptureItem, CaptureContentClass, Sensitivity,
        TerminalTurnHistory,
    };
    use crate::runtime::memory_context::{ProjectId, RetentionClass, SessionId, TurnId};
    use agent_core::core::disclosure::{
        gate_for_model, may_persist, DisclosureClass, ModelVisibility,
    };
    use agent_core::core::session_index::{SessionIndexEventKind, SessionIndexRecord};
    use std::collections::HashSet;
    use std::io::BufRead;
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime};

    let started = Instant::now();
    let mut report = HistoryImportReport::default();
    let mut bytes_processed = 0_u64;
    let key = import_checkpoint_key(&plan.preview.project_id);
    let mut committed = load_import_checkpoint(checkpoint_path, &key)?;

    let run = (|| {
        if batch_size == 0 || batch_size > IMPORT_BATCH_MAX_RECORDS {
            return Err(HistoryImportError::InvalidBatchSize);
        }
        if lease.project_id.as_str() != plan.preview.project_id {
            return Err(HistoryImportError::InvalidIdentity);
        }
        if cancellation.is_cancelled() {
            return Err(HistoryImportError::Cancelled);
        }

        let index = match std::fs::File::open(sessions_dir.join("index.jsonl")) {
            Ok(index) => index,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(_) => return Err(HistoryImportError::SessionIndexReadFailed),
        };
        let mut scoped = HashSet::new();
        for line in std::io::BufReader::new(index).lines() {
            let line = line.map_err(|_| HistoryImportError::SessionIndexReadFailed)?;
            let Ok(record) = serde_json::from_str::<SessionIndexRecord>(&line) else {
                continue;
            };
            if record.event == SessionIndexEventKind::Start
                && record.cwd.as_deref() == Some(plan.preview.project_root.as_path())
            {
                scoped.insert(record.session_id);
            }
        }

        let project_id = ProjectId::parse(&plan.preview.project_id)
            .map_err(|_| HistoryImportError::InvalidIdentity)?;
        let redactor = |text: &str| redact_import_text(text);
        let mut ids: Vec<String> = agent_core::session_journal::session_dir_entries(sessions_dir)
            .map_err(|_| HistoryImportError::SessionIndexReadFailed)?
            .into_iter()
            .filter_map(|entry| entry.name.strip_suffix(".json").map(str::to_owned))
            .filter(|id| scoped.contains(id))
            .collect();
        ids.sort();

        let mut submitted_in_batch = 0_usize;
        for id in ids {
            if cancellation.is_cancelled() {
                return Err(HistoryImportError::Cancelled);
            }
            let session = agent_core::session::Session::load_from_dir(sessions_dir, &id)
                .map_err(|_| HistoryImportError::SessionLoadFailed)?;
            report.sessions_loaded += 1;
            let session_id =
                SessionId::parse(&session.id).map_err(|_| HistoryImportError::InvalidIdentity)?;
            let started_at: SystemTime = session.created_at.into();
            let completed_at: SystemTime = session.updated_at.into();
            let mut pending_user: Option<String> = None;
            let mut ordinal = 0_u64;

            for message in &session.api_messages {
                if cancellation.is_cancelled() {
                    return Err(HistoryImportError::Cancelled);
                }
                let role = message.get("role").and_then(serde_json::Value::as_str);
                let Some(text) = import_message_text(message) else {
                    continue;
                };
                match role {
                    Some("user") => pending_user = Some(text),
                    Some("assistant") => {
                        let Some(user) = pending_user.take() else {
                            continue;
                        };
                        ordinal = ordinal.saturating_add(1);
                        let disclosure = DisclosureClass::ModelVisibleAfterRedaction;
                        if !may_persist(disclosure) {
                            continue;
                        }
                        let user = match gate_for_model(disclosure, &user, false, Some(&redactor)) {
                            ModelVisibility::Visible(text) => text,
                            ModelVisibility::Withheld(_) => continue,
                        };
                        let assistant =
                            match gate_for_model(disclosure, &text, false, Some(&redactor)) {
                                ModelVisibility::Visible(text) => text,
                                ModelVisibility::Withheld(_) => continue,
                            };
                        let history = TerminalTurnHistory {
                            project_id: project_id.clone(),
                            session_id: session_id.clone(),
                            turn_id: TurnId::parse(&format!("import-{ordinal}"))
                                .map_err(|_| HistoryImportError::InvalidIdentity)?,
                            turn_ordinal: ordinal,
                            started_at,
                            completed_at: completed_at.max(started_at + Duration::from_nanos(1)),
                            outcome: agent_core::TurnOutcome::Completed,
                            items: vec![
                                CanonicalCaptureItem {
                                    project_id: project_id.clone(),
                                    class: CaptureContentClass::UserMessage,
                                    disclosure,
                                    sensitivity: Sensitivity::Normal,
                                    text: user,
                                    tool_name: None,
                                },
                                CanonicalCaptureItem {
                                    project_id: project_id.clone(),
                                    class: CaptureContentClass::AssistantFinal,
                                    disclosure,
                                    sensitivity: Sensitivity::Normal,
                                    text: assistant,
                                    tool_name: None,
                                },
                            ],
                            compaction: None,
                        };
                        let capture =
                            build_chat_turn_capture(&project_id, history, RetentionClass::Standard)
                                .map_err(|_| HistoryImportError::CaptureBuildFailed)?;
                        report.captures_built += 1;
                        let digest = capture.source_digest.to_hex();
                        if committed.contains(&digest) {
                            report.ranges_skipped += 1;
                            continue;
                        }
                        let capture_bytes = capture
                            .user
                            .content
                            .text
                            .len()
                            .saturating_add(capture.assistant.content.text.len())
                            as u64;
                        let (receipt_tx, receipt_rx) = mpsc::channel();
                        if !worker.submit_built_with_receipt(
                            lease,
                            capture,
                            provider.clone(),
                            receipt_tx,
                        ) {
                            return Err(HistoryImportError::CaptureQueueFull);
                        }
                        loop {
                            match receipt_rx.try_recv() {
                                Ok(Ok(())) => break,
                                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                                    return Err(HistoryImportError::ProviderCaptureFailed)
                                }
                                Err(mpsc::TryRecvError::Empty) if cancellation.is_cancelled() => {
                                    return Err(HistoryImportError::Cancelled)
                                }
                                Err(mpsc::TryRecvError::Empty) => {
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                            }
                        }
                        // Cancellation can race with a provider acknowledgement.
                        // Do not advance the host checkpoint after cancellation;
                        // the stable C1 idempotency key makes this ambiguous range
                        // safe for the existing C3/plugin store to see on resume.
                        if cancellation.is_cancelled() {
                            return Err(HistoryImportError::Cancelled);
                        }

                        committed.insert(digest);
                        persist_import_checkpoint(checkpoint_path, &key, &committed)?;
                        bytes_processed = bytes_processed.saturating_add(capture_bytes);
                        submitted_in_batch += 1;
                        if submitted_in_batch == batch_size {
                            report.batches_submitted += 1;
                            submitted_in_batch = 0;
                        }
                        emit(HistoryImportEvent::progress(
                            committed.len(),
                            plan.preview.session_count,
                            bytes_processed,
                            started.elapsed(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        if submitted_in_batch != 0 {
            report.batches_submitted += 1;
        }
        Ok(report)
    })();

    if let Err(error) = &run {
        emit(HistoryImportEvent::error(
            committed.len(),
            bytes_processed,
            started.elapsed(),
            error.clone(),
        ));
    }
    run
}

fn import_checkpoint_key(project_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"synaps-memory-history-checkpoint-v1\0");
    digest.update(project_id.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_import_checkpoint(
    path: &std::path::Path,
    key: &str,
) -> Result<std::collections::HashSet<String>, HistoryImportError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(_) => return Err(HistoryImportError::CheckpointReadFailed),
    };
    if bytes.len() as u64 > CHECKPOINT_FILE_MAX_BYTES {
        return Err(HistoryImportError::CheckpointReadFailed);
    }
    let checkpoint: ImportCheckpoint =
        serde_json::from_slice(&bytes).map_err(|_| HistoryImportError::CheckpointReadFailed)?;
    if checkpoint.version != 1 {
        return Err(HistoryImportError::CheckpointReadFailed);
    }
    if checkpoint.key != key {
        return Ok(Default::default());
    }
    if checkpoint
        .committed_source_digests
        .iter()
        .any(|digest| digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(HistoryImportError::CheckpointReadFailed);
    }
    Ok(checkpoint.committed_source_digests.into_iter().collect())
}

fn persist_import_checkpoint(
    path: &std::path::Path,
    key: &str,
    committed: &std::collections::HashSet<String>,
) -> Result<(), HistoryImportError> {
    use std::io::Write;
    let mut digests: Vec<_> = committed.iter().cloned().collect();
    digests.sort();
    let bytes = serde_json::to_vec(&ImportCheckpoint {
        version: 1,
        key: key.to_owned(),
        committed_source_digests: digests,
    })
    .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    if bytes.len() as u64 > CHECKPOINT_FILE_MAX_BYTES {
        return Err(HistoryImportError::CheckpointWriteFailed);
    }
    let parent = path
        .parent()
        .ok_or(HistoryImportError::CheckpointWriteFailed)?;
    std::fs::create_dir_all(parent).map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file_mut().sync_all())
        .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    temporary
        .persist(path)
        .map_err(|_| HistoryImportError::CheckpointWriteFailed)?;
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
fn load_import_checkpoint_for_test(
    path: &std::path::Path,
) -> Result<std::collections::HashSet<String>, HistoryImportError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(_) => return Err(HistoryImportError::CheckpointReadFailed),
    };
    let checkpoint: ImportCheckpoint =
        serde_json::from_slice(&bytes).map_err(|_| HistoryImportError::CheckpointReadFailed)?;
    Ok(checkpoint.committed_source_digests.into_iter().collect())
}

fn submit_import_batch(
    worker: &crate::runtime::capture_worker::CaptureWorker,
    lease: &crate::runtime::memory_context::MemoryContextLease,
    provider: std::sync::Arc<dyn crate::runtime::capture_worker::CaptureProvider>,
    captures: &mut Vec<crate::runtime::chat_capture::ChatTurnCapture>,
) -> Result<(), HistoryImportError> {
    for capture in captures.drain(..) {
        if !worker.submit_built(lease, capture, provider.clone()) {
            return Err(HistoryImportError::CaptureQueueFull);
        }
    }
    Ok(())
}

fn import_message_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(text) => (!text.trim().is_empty()).then(|| text.clone()),
        serde_json::Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                })
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn redact_import_text(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if ["password=", "token=", "secret=", "api_key="]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                "[REDACTED]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl HistoryImportHostState {
    /// Resolve the same stable host-owned project scope used by memory tools.
    pub fn from_current_host() -> Result<Self, HistoryImportError> {
        let cwd = std::env::current_dir().map_err(|_| HistoryImportError::HostStateUnavailable)?;
        let scope = agent_core::memory::store::ProjectScope::discover(&cwd)
            .map_err(|_| HistoryImportError::HostStateUnavailable)?;
        Ok(Self {
            project_id: scope.key().to_owned(),
            project_root: scope.root().to_path_buf(),
            destination_r8_path: std::env::var_os("AXEL_BRAIN")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("SYNAPS_DATA_DIR")
                        .map(PathBuf::from)
                        .map(|dir| dir.join("axel.r8"))
                })
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|home| home.join(".config/axel/axel.r8"))
                })
                .unwrap_or_else(|| scope.root().join("axel.r8")),
        })
    }
}

/// Production metadata provider over the canonical session storage API. It
/// intentionally does not implement content reads or Axel writes in D1.
pub struct CanonicalHistoryMetadataIo {
    sessions_dir: PathBuf,
}

impl CanonicalHistoryMetadataIo {
    pub fn new() -> Self {
        Self {
            sessions_dir: agent_core::config::get_active_config_dir().join("sessions"),
        }
    }
}

impl Default for CanonicalHistoryMetadataIo {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryImportIo for CanonicalHistoryMetadataIo {
    fn scan_metadata(&mut self) -> Result<Vec<HistorySessionMetadata>, HistoryImportError> {
        let entries = agent_core::session_journal::session_dir_entries(&self.sessions_dir)
            .map_err(|_| HistoryImportError::MetadataScanFailed)?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.name.ends_with(".json"))
            .map(|entry| {
                let id = entry.name.trim_end_matches(".json").to_owned();
                let started_at = entry
                    .mtime
                    .map(chrono::DateTime::<chrono::Utc>::from)
                    .map(|time| time.to_rfc3339())
                    .unwrap_or_else(|| "unknown".to_string());
                HistorySessionMetadata {
                    id,
                    approx_bytes: entry.byte_len,
                    started_at,
                }
            })
            .collect())
    }

    fn read_session_content(&mut self, _id: &str) -> Result<(), HistoryImportError> {
        Err(HistoryImportError::ConsentRequired)
    }

    fn write_axel(&mut self) -> Result<(), HistoryImportError> {
        Err(HistoryImportError::ConsentRequired)
    }
}

/// Compute the disclosure using only trusted host state and a metadata scan.
pub fn preview_history_import(
    host: &HistoryImportHostState,
    io: &mut impl HistoryImportIo,
) -> Result<HistoryImportPreview, HistoryImportError> {
    let sessions = io.scan_metadata()?;
    let approx_bytes = sessions.iter().fold(0_u64, |total, session| {
        total.saturating_add(session.approx_bytes)
    });
    let included_date_range = sessions
        .iter()
        .map(|session| session.started_at.as_str())
        .min()
        .zip(
            sessions
                .iter()
                .map(|session| session.started_at.as_str())
                .max(),
        )
        .map(|(earliest, latest)| IncludedDateRange {
            earliest: earliest.to_owned(),
            latest: latest.to_owned(),
        });

    Ok(HistoryImportPreview {
        project_id: host.project_id.clone(),
        project_root: host.project_root.clone(),
        session_count: sessions.len(),
        approx_bytes,
        included_date_range,
        included_content_classes: vec![
            "user_messages",
            "assistant_final_messages",
            "eligible_tool_outcome_summaries",
            "typed_terminal_outcomes",
            "session_turn_time_provenance",
            "compaction_linkage",
        ],
        excluded_content_classes: vec![
            "system_and_developer_prompts",
            "private_reasoning",
            "credentials_and_secret_prompt_responses",
            "raw_binary_attachments",
            "unbounded_tool_output",
            "never_persist_content",
            "foreign_project_content",
        ],
        retention_policy: "project-scoped local retention; never_persist content is excluded"
            .to_string(),
        redaction_policy:
            "host disclosure filtering, secret exclusion, and bounded tool-result summaries"
                .to_string(),
        destination_r8_path: host.destination_r8_path.clone(),
        explicit_confirmation_required: true,
    })
}

/// Apply the user's decision without beginning import.
pub fn authorize_history_import(
    preview: HistoryImportPreview,
    consent: HistoryImportConsent,
    authority: RequestAuthority,
    _io: &mut impl HistoryImportIo,
) -> Result<HistoryImportOutcome, HistoryImportError> {
    if authority == RequestAuthority::ModelTool {
        return Err(HistoryImportError::ModelCannotConfirm);
    }

    match consent {
        HistoryImportConsent::Declined => Ok(HistoryImportOutcome::Declined),
        HistoryImportConsent::Confirmed(intent) => Ok(HistoryImportOutcome::Ready(ImportPlan {
            confirmation_id: intent.confirmation_id().to_owned(),
            user_intent: intent.proof,
            preview,
        })),
    }
}

/// Request authority is fixed by the entry point, not accepted as an argument.
pub fn propose_history_import(
    host: &HistoryImportHostState,
    io: &mut impl HistoryImportIo,
) -> Result<HistoryImportPreview, HistoryImportError> {
    preview_history_import(host, io)
}

/// Single shared engine operation used by every frontend.
pub fn index_history(
    host: &HistoryImportHostState,
    consent: HistoryImportConsent,
    authority: RequestAuthority,
    io: &mut impl HistoryImportIo,
) -> Result<HistoryImportOutcome, HistoryImportError> {
    // Reject model self-confirmation before even the metadata scan.
    if authority == RequestAuthority::ModelTool {
        return Err(HistoryImportError::ModelCannotConfirm);
    }
    let preview = preview_history_import(host, io)?;
    authorize_history_import(preview, consent, authority, io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[derive(Default)]
    struct AccountingIo {
        metadata_scans: usize,
        content_reads: usize,
        axel_writes: usize,
    }

    impl HistoryImportIo for AccountingIo {
        fn scan_metadata(&mut self) -> Result<Vec<HistorySessionMetadata>, HistoryImportError> {
            self.metadata_scans += 1;
            Ok(vec![
                HistorySessionMetadata {
                    id: "session-a".into(),
                    approx_bytes: 120,
                    started_at: "2025-01-02T03:04:05Z".into(),
                },
                HistorySessionMetadata {
                    id: "session-b".into(),
                    approx_bytes: 80,
                    started_at: "2025-02-03T04:05:06Z".into(),
                },
            ])
        }

        fn read_session_content(&mut self, _id: &str) -> Result<(), HistoryImportError> {
            self.content_reads += 1;
            Ok(())
        }

        fn write_axel(&mut self) -> Result<(), HistoryImportError> {
            self.axel_writes += 1;
            Ok(())
        }
    }

    fn host() -> HistoryImportHostState {
        HistoryImportHostState {
            project_id: "project-host-owned".into(),
            project_root: PathBuf::from("/workspace/project"),
            destination_r8_path: PathBuf::from("/workspace/project/.r8/axel-memory"),
        }
    }

    #[test]
    fn preview_fields_are_populated_from_host_state() {
        let mut io = AccountingIo::default();
        let preview = preview_history_import(&host(), &mut io).expect("preview");

        assert_eq!(preview.project_id, "project-host-owned");
        assert_eq!(preview.project_root, PathBuf::from("/workspace/project"));
        assert_eq!(preview.session_count, 2);
        assert_eq!(preview.approx_bytes, 200);
        assert_eq!(
            preview.included_date_range,
            Some(IncludedDateRange {
                earliest: "2025-01-02T03:04:05Z".into(),
                latest: "2025-02-03T04:05:06Z".into(),
            })
        );
        assert!(!preview.included_content_classes.is_empty());
        assert!(!preview.excluded_content_classes.is_empty());
        assert!(!preview.retention_policy.is_empty());
        assert!(!preview.redaction_policy.is_empty());
        assert_eq!(
            preview.destination_r8_path,
            PathBuf::from("/workspace/project/.r8/axel-memory")
        );
        assert!(preview.explicit_confirmation_required);
        assert_eq!(io.metadata_scans, 1);
        assert_eq!(io.content_reads, 0);
        assert_eq!(io.axel_writes, 0);
    }

    #[test]
    fn declined_consent_does_no_content_reads_or_axel_writes() {
        let mut io = AccountingIo::default();
        let preview = preview_history_import(&host(), &mut io).expect("preview");
        let outcome = authorize_history_import(
            preview,
            HistoryImportConsent::Declined,
            RequestAuthority::UserFrontend,
            &mut io,
        )
        .expect("decline is not an error");

        assert_eq!(outcome, HistoryImportOutcome::Declined);
        assert_eq!(io.metadata_scans, 1);
        assert_eq!(io.content_reads, 0);
        assert_eq!(io.axel_writes, 0);
    }

    #[test]
    fn model_self_confirmation_is_denied_before_any_read() {
        let mut io = AccountingIo::default();
        let result = index_history(
            &host(),
            HistoryImportConsent::Confirmed(ConfirmedUserIntent::for_test("confirmation-1")),
            RequestAuthority::ModelTool,
            &mut io,
        );

        assert_eq!(result, Err(HistoryImportError::ModelCannotConfirm));
        assert_eq!(io.metadata_scans, 0);
        assert_eq!(io.content_reads, 0);
        assert_eq!(io.axel_writes, 0);
    }

    #[test]
    fn model_proposal_returns_preview_but_has_no_confirmation_path() {
        let mut io = AccountingIo::default();
        let preview = propose_history_import(&host(), &mut io).expect("preview");

        assert!(preview.explicit_confirmation_required);
        assert_eq!(io.metadata_scans, 1);
        assert_eq!(io.content_reads, 0);
        assert_eq!(io.axel_writes, 0);
    }

    #[test]
    fn confirmed_user_consent_returns_typed_plan_without_starting_import() {
        let mut io = AccountingIo::default();
        let outcome = index_history(
            &host(),
            HistoryImportConsent::Confirmed(ConfirmedUserIntent::for_test("confirmation-2")),
            RequestAuthority::UserFrontend,
            &mut io,
        )
        .expect("confirmed plan");

        let HistoryImportOutcome::Ready(plan) = outcome else {
            panic!("expected typed plan");
        };
        assert_eq!(plan.preview.session_count, 2);
        assert_eq!(plan.confirmation_id, "confirmation-2");
        assert_eq!(io.metadata_scans, 1);
        assert_eq!(io.content_reads, 0);
        assert_eq!(io.axel_writes, 0);
    }

    mod streaming_import {
        use super::*;
        use crate::runtime::capture_worker::{CaptureFailure, CaptureProvider, CaptureWorker};
        use crate::runtime::chat_capture::ChatTurnCapture;
        use crate::runtime::memory_context::{
            mint_explicit_command_proof, CapturePolicy, ContextProviderId, MemoryContextLease,
            MemoryContextMode, MemoryLeaseId, ProjectId, RecallPolicy, SessionId,
        };
        use agent_core::session::Session;
        use agent_core::session_journal::{save_session_in_dir, SessionPersistence};
        use chrono::{TimeZone, Utc};
        use serde_json::json;
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant, SystemTime};

        struct RecordingProvider {
            captures: Mutex<Vec<ChatTurnCapture>>,
            network_constructions: std::sync::atomic::AtomicUsize,
            block: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        }

        impl Default for RecordingProvider {
            fn default() -> Self {
                Self {
                    captures: Mutex::new(Vec::new()),
                    network_constructions: std::sync::atomic::AtomicUsize::new(0),
                    block: Mutex::new(None),
                }
            }
        }

        impl CaptureProvider for RecordingProvider {
            fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
                self.captures.lock().expect("captures lock").push(capture);
                if let Some(receiver) = self.block.lock().expect("block lock").take() {
                    let _ = receiver.recv();
                }
                Ok(())
            }
        }

        fn fixture_session(id: &str, user: &str, assistant: &str) -> Session {
            let timestamp = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
            Session {
                id: id.into(),
                title: id.into(),
                name: None,
                model: "fixture-model".into(),
                thinking_level: "brief".into(),
                system_prompt: Some("must never import".into()),
                created_at: timestamp,
                updated_at: timestamp,
                total_input_tokens: 0,
                total_output_tokens: 0,
                session_cost: 0.0,
                api_messages: vec![
                    Arc::new(json!({"role": "user", "content": user})),
                    Arc::new(json!({"role": "assistant", "content": assistant})),
                ],
                abort_context: None,
                parent_session: None,
                compacted_into: None,
                prompt_provenance: None,
                compaction: None,
            }
        }

        fn append_index(dir: &std::path::Path, id: &str, root: &std::path::Path) {
            use std::io::Write;
            std::fs::create_dir_all(dir).unwrap();
            let mut index = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("index.jsonl"))
                .unwrap();
            writeln!(
                index,
                "{}",
                json!({
                    "schema_version": 1,
                    "session_id": id,
                    "event": "start",
                    "timestamp": "2023-11-14T22:13:20Z",
                    "cwd": root,
                })
            )
            .unwrap();
        }

        fn plan(root: &std::path::Path, session_count: usize) -> ImportPlan {
            ImportPlan {
                preview: HistoryImportPreview {
                    project_id: "project-import".into(),
                    project_root: root.to_path_buf(),
                    session_count,
                    approx_bytes: 1,
                    included_date_range: None,
                    included_content_classes: vec!["user_messages", "assistant_final_messages"],
                    excluded_content_classes: vec!["foreign_project_content"],
                    retention_policy: "standard".into(),
                    redaction_policy: "host".into(),
                    destination_r8_path: root.join("axel.r8"),
                    explicit_confirmation_required: true,
                },
                confirmation_id: "confirmed-import".into(),
                user_intent: mint_explicit_command_proof(),
            }
        }

        fn lease() -> MemoryContextLease {
            let now = SystemTime::now();
            MemoryContextLease::grant(
                MemoryLeaseId::parse("lease-import").unwrap(),
                SessionId::parse("active-session").unwrap(),
                ProjectId::parse("project-import").unwrap(),
                ContextProviderId::parse("extension:axel:memory").unwrap(),
                MemoryContextMode::CaptureOnly,
                CapturePolicy::default(),
                RecallPolicy::default(),
                mint_explicit_command_proof(),
                now,
                Some(now + Duration::from_secs(60)),
            )
            .unwrap()
        }

        fn wait_for(provider: &RecordingProvider, count: usize) {
            for _ in 0..100 {
                if provider.captures.lock().unwrap().len() >= count {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("capture worker did not drain");
        }

        #[test]
        fn legacy_json_and_journal_sessions_stream_through_one_host_api() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project");
            let sessions = temp.path().join("sessions");
            std::fs::create_dir_all(&root).unwrap();

            let legacy = fixture_session("legacy", "old user token=secret-sentinel", "old answer");
            save_session_in_dir(&sessions, &legacy, SessionPersistence::Json).unwrap();
            append_index(&sessions, "legacy", &root);

            let mut journal = fixture_session("journal", "journal user", "journal answer");
            save_session_in_dir(&sessions, &journal, SessionPersistence::Journal).unwrap();
            journal.api_messages.push(Arc::new(json!({
                "role": "user", "content": "appended user"
            })));
            journal.api_messages.push(Arc::new(json!({
                "role": "assistant", "content": "appended answer"
            })));
            save_session_in_dir(&sessions, &journal, SessionPersistence::Journal).unwrap();
            append_index(&sessions, "journal", &root);

            let provider = Arc::new(RecordingProvider::default());
            let worker = CaptureWorker::new(4);
            let report = import_history_from_dir(
                &plan(&root, 2),
                &lease(),
                &sessions,
                2,
                &worker,
                provider.clone(),
            )
            .unwrap();

            assert_eq!(report.sessions_loaded, 2);
            assert_eq!(report.captures_built, 3);
            wait_for(&provider, 3);
            let captures = provider.captures.lock().unwrap();
            let users: Vec<&str> = captures
                .iter()
                .map(|c| c.user.content.text.as_str())
                .collect();
            assert!(users.contains(&"old user [REDACTED]"));
            assert!(users.contains(&"journal user"));
            assert!(users.contains(&"appended user"));
            assert!(captures.iter().all(|capture| !capture
                .user
                .content
                .text
                .contains("secret-sentinel")));
            assert_eq!(
                provider
                    .network_constructions
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        }

        #[test]
        fn foreign_project_session_is_not_opened_or_disclosed() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project-a");
            let foreign_root = temp.path().join("project-b");
            let sessions = temp.path().join("sessions");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&foreign_root).unwrap();

            let own = fixture_session("own", "own user", "own answer");
            save_session_in_dir(&sessions, &own, SessionPersistence::Json).unwrap();
            append_index(&sessions, "own", &root);
            append_index(&sessions, "foreign-sentinel", &foreign_root);
            std::fs::write(
                sessions.join("foreign-sentinel.json"),
                b"SENTINEL: opening this foreign session must fail parsing",
            )
            .unwrap();

            let provider = Arc::new(RecordingProvider::default());
            let worker = CaptureWorker::new(2);
            let report = import_history_from_dir(
                &plan(&root, 1),
                &lease(),
                &sessions,
                1,
                &worker,
                provider.clone(),
            )
            .unwrap();

            assert_eq!(report.sessions_loaded, 1);
            wait_for(&provider, 1);
            let captures = provider.captures.lock().unwrap();
            assert_eq!(captures.len(), 1);
            assert_eq!(captures[0].session_id.as_str(), "own");
        }

        #[test]
        fn import_batches_are_hard_bounded() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project");
            let sessions = temp.path().join("sessions");
            std::fs::create_dir_all(&root).unwrap();
            let provider = Arc::new(RecordingProvider::default());
            let worker = CaptureWorker::new(1);

            let error = import_history_from_dir(
                &plan(&root, 0),
                &lease(),
                &sessions,
                IMPORT_BATCH_MAX_RECORDS + 1,
                &worker,
                provider,
            )
            .unwrap_err();

            assert_eq!(error, HistoryImportError::InvalidBatchSize);
        }

        #[test]
        fn import_queue_overflow_is_bounded_and_reported() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project");
            let sessions = temp.path().join("sessions");
            std::fs::create_dir_all(&root).unwrap();
            let session = fixture_session("many", "u1", "a1");
            save_session_in_dir(&sessions, &session, SessionPersistence::Json).unwrap();
            append_index(&sessions, "many", &root);
            // Expand the legacy fixture to three completed turns.
            let mut session = session;
            for (user, assistant) in [("u2", "a2"), ("u3", "a3")] {
                session
                    .api_messages
                    .push(Arc::new(json!({"role": "user", "content": user})));
                session
                    .api_messages
                    .push(Arc::new(json!({"role": "assistant", "content": assistant})));
            }
            save_session_in_dir(&sessions, &session, SessionPersistence::Json).unwrap();

            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let provider = Arc::new(RecordingProvider {
                block: Mutex::new(Some(release_rx)),
                ..RecordingProvider::default()
            });
            let worker = CaptureWorker::new(1);
            let error =
                import_history_from_dir(&plan(&root, 1), &lease(), &sessions, 3, &worker, provider)
                    .unwrap_err();
            release_tx.send(()).unwrap();

            assert_eq!(error, HistoryImportError::CaptureQueueFull);
            assert!(worker.overflow_drops() >= 1);
        }

        struct DurableDigestProvider {
            digests: Mutex<HashSet<String>>,
            committed: AtomicUsize,
            cancel_after: Option<usize>,
            cancel: Option<HistoryImportCancellation>,
        }

        impl CaptureProvider for DurableDigestProvider {
            fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
                let inserted = self
                    .digests
                    .lock()
                    .unwrap()
                    .insert(capture.source_digest.to_hex());
                if inserted {
                    let committed = self.committed.fetch_add(1, Ordering::SeqCst) + 1;
                    if self.cancel_after == Some(committed) {
                        self.cancel.as_ref().unwrap().cancel();
                    }
                }
                Ok(())
            }

            fn contains_capture(
                &self,
                _capture_id: &[u8; 32],
            ) -> Result<crate::runtime::capture_worker::CaptureCommitState, CaptureFailure>
            {
                Ok(crate::runtime::capture_worker::CaptureCommitState::Absent)
            }
        }

        fn many_turn_session(id: &str, turns: usize, sentinel: &str) -> Session {
            let mut session = fixture_session(id, &format!("{sentinel}-u1"), "a1");
            for turn in 2..=turns {
                session.api_messages.push(Arc::new(json!({
                    "role": "user", "content": format!("{sentinel}-u{turn}")
                })));
                session.api_messages.push(Arc::new(json!({
                    "role": "assistant", "content": format!("a{turn}")
                })));
            }
            session
        }

        fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
            let deadline = Instant::now() + timeout;
            while !predicate() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(predicate());
        }

        #[test]
        fn forced_stop_resumes_last_checkpoint_without_duplicate_source_digests() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project");
            let sessions = temp.path().join("sessions");
            let checkpoints = temp.path().join("private/import-checkpoints.json");
            std::fs::create_dir_all(&root).unwrap();
            let session = many_turn_session("resume", 6, "CONTENT_SENTINEL");
            save_session_in_dir(&sessions, &session, SessionPersistence::Json).unwrap();
            append_index(&sessions, "resume", &root);

            let cancel = HistoryImportCancellation::new();
            let first_provider = Arc::new(DurableDigestProvider {
                digests: Mutex::new(HashSet::new()),
                committed: AtomicUsize::new(0),
                cancel_after: Some(2),
                cancel: Some(cancel.clone()),
            });
            let worker = CaptureWorker::new(8);
            let mut first_events = Vec::new();
            let first = import_history_resumable_from_dir(
                &plan(&root, 1),
                &lease(),
                &sessions,
                &checkpoints,
                1,
                &worker,
                first_provider.clone(),
                &cancel,
                &mut |event| first_events.push(event),
            );
            assert_eq!(first, Err(HistoryImportError::Cancelled));
            wait_until(Duration::from_secs(1), || {
                first_provider.committed.load(Ordering::SeqCst) == 2
            });

            let first_digests = first_provider.digests.lock().unwrap().clone();
            let resume_provider = Arc::new(DurableDigestProvider {
                digests: Mutex::new(first_digests.clone()),
                committed: AtomicUsize::new(0),
                cancel_after: None,
                cancel: None,
            });
            let resume_worker = CaptureWorker::new(8);
            let mut resume_events = Vec::new();
            let report = import_history_resumable_from_dir(
                &plan(&root, 1),
                &lease(),
                &sessions,
                &checkpoints,
                1,
                &resume_worker,
                resume_provider.clone(),
                &HistoryImportCancellation::new(),
                &mut |event| resume_events.push(event),
            )
            .unwrap();
            wait_until(Duration::from_secs(1), || {
                resume_provider.digests.lock().unwrap().len() == 6
            });

            let final_digests = resume_provider.digests.lock().unwrap();
            assert_eq!(final_digests.len(), 6, "source ranges must be unique");
            assert_eq!(report.ranges_skipped, 1, "resume starts at last checkpoint");
            assert!(std::fs::metadata(&checkpoints).unwrap().len() <= CHECKPOINT_FILE_MAX_BYTES);
            #[cfg(unix)]
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(
                    &std::fs::metadata(&checkpoints).unwrap().permissions()
                ) & 0o777,
                0o600
            );
        }

        #[test]
        fn cancellation_keeps_checkpoint_consistent_and_does_not_block_producer() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("project");
            let sessions = temp.path().join("sessions");
            let checkpoints = temp.path().join("private/import-checkpoints.json");
            std::fs::create_dir_all(&root).unwrap();
            let session = many_turn_session("cancel", 20, "CANCEL_SENTINEL");
            save_session_in_dir(&sessions, &session, SessionPersistence::Json).unwrap();
            append_index(&sessions, "cancel", &root);

            let cancel = HistoryImportCancellation::new();
            cancel.cancel();
            let provider = Arc::new(RecordingProvider::default());
            let worker = CaptureWorker::new(1);
            let started = Instant::now();
            let result = import_history_resumable_from_dir(
                &plan(&root, 1),
                &lease(),
                &sessions,
                &checkpoints,
                1,
                &worker,
                provider,
                &cancel,
                &mut |_| {},
            );

            assert_eq!(result, Err(HistoryImportError::Cancelled));
            assert!(started.elapsed() < Duration::from_millis(100));
            assert!(load_import_checkpoint_for_test(&checkpoints)
                .unwrap()
                .is_empty());
        }

        #[test]
        fn progress_and_error_events_are_metadata_only() {
            let event = HistoryImportEvent::progress(3, 7, 1024, Duration::from_millis(9));
            let error = HistoryImportEvent::error(
                3,
                1024,
                Duration::from_millis(10),
                HistoryImportError::SessionLoadFailed,
            );
            for serialized in [
                serde_json::to_string(&event).unwrap(),
                serde_json::to_string(&error).unwrap(),
            ] {
                assert!(!serialized.contains("CONTENT_SENTINEL"));
                assert!(!serialized.contains("project"));
                assert!(!serialized.contains("path"));
                assert!(!serialized.contains("content"));
            }
            assert_eq!(event.name, "memory_import.progress");
        }
    }
}
