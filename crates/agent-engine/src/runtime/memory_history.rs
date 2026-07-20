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
}
