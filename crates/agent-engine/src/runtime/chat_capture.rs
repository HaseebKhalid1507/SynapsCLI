//! Typed, terminal-only continuous-memory chat capture (task C1).
//!
//! The builder accepts [`TerminalTurnHistory`], not streaming state. It rejects
//! foreign-project provenance before inspecting content, filters forbidden
//! classes, bounds every retained body, and derives a deterministic source
//! digest / capture id from canonical source data.

use super::memory_context::{ProjectId, RetentionClass, SessionId, TurnId};
use agent_core::core::disclosure::DisclosureClass;
use agent_core::{BoundedText, BudgetDimension, TurnOutcome};
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CAPTURE_SEGMENT_MAX_BYTES: usize = 4096;
pub const SUMMARY_CAPTURE_MAX_BYTES: usize = 12_000;
pub const TOOL_SUMMARY_MAX_BYTES: usize = 1024;
pub const TOOL_NAME_MAX_BYTES: usize = 128;
pub const MAX_TOOL_SUMMARIES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSchemaVersion {
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sensitivity {
    Normal,
    Sensitive,
    Secret,
}

/// Host classification of one canonical history item. Only the first three
/// variants can produce captured content; all other variants are forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureContentClass {
    UserMessage,
    AssistantFinal,
    ToolSummary,
    SystemPrompt,
    DeveloperPrompt,
    PrivateReasoning,
    Secret,
    NeverPersist,
    RawBinary,
    RawToolOutput,
}

impl CaptureContentClass {
    fn digest_tag(self) -> u8 {
        match self {
            Self::UserMessage => 0,
            Self::AssistantFinal => 1,
            Self::ToolSummary => 2,
            Self::SystemPrompt => 3,
            Self::DeveloperPrompt => 4,
            Self::PrivateReasoning => 5,
            Self::Secret => 6,
            Self::NeverPersist => 7,
            Self::RawBinary => 8,
            Self::RawToolOutput => 9,
        }
    }

    fn is_capture_content(self) -> bool {
        matches!(
            self,
            Self::UserMessage | Self::AssistantFinal | Self::ToolSummary
        )
    }
}

/// One item from canonical completed history, already classified by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalCaptureItem {
    pub project_id: ProjectId,
    pub class: CaptureContentClass,
    pub disclosure: DisclosureClass,
    pub sensitivity: Sensitivity,
    pub text: String,
    /// Required for [`CaptureContentClass::ToolSummary`], ignored otherwise.
    pub tool_name: Option<String>,
}

/// Canonical history after the engine has produced exactly one terminal
/// [`TurnOutcome`]. Partial stream accumulators are a different type and cannot
/// be passed to [`build_chat_turn_capture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTurnHistory {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub turn_ordinal: u64,
    pub started_at: SystemTime,
    pub completed_at: SystemTime,
    pub outcome: TurnOutcome,
    pub items: Vec<CanonicalCaptureItem>,
    pub compaction: Option<CompactionSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSegment {
    pub content: BoundedText,
    pub disclosure: DisclosureClass,
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCaptureSummary {
    pub tool_name: BoundedText,
    pub summary: BoundedText,
    pub disclosure: DisclosureClass,
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageRangeDigest([u8; 32]);

impl MessageRangeDigest {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        decode_hex_32(hex).map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex_bytes(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptStackDigest([u8; 32]);

impl PromptStackDigest {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        decode_hex_32(hex).map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex_bytes(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureId([u8; 32]);

impl CaptureId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex_bytes(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionSchemaVersion {
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionSummaryOrigin {
    LocalOnly,
    Provider {
        provider_id: String,
        model_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionPolicy {
    None,
    HostRedacted,
}

/// First-class provenance for a compaction summary linked to this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSource {
    pub schema: CompactionSchemaVersion,
    pub project_id: ProjectId,
    pub source_session_id: SessionId,
    /// Inclusive source range. Compaction of legacy histories uses canonical
    /// message positions because those messages have no durable turn ordinal;
    /// the accompanying digest binds the exact ordered content.
    pub first_turn_ordinal: u64,
    pub last_turn_ordinal: u64,
    pub source_digest: MessageRangeDigest,
    pub summary_origin: CompactionSummaryOrigin,
    pub prompt_stack_digest: PromptStackDigest,
    pub redaction: RedactionPolicy,
    pub content_classes: Vec<agent_core::compaction::ContentClass>,
    pub summarized_at: SystemTime,
}

/// First-class compaction memory. It links to (and never substitutes for) the
/// source range whose full provenance remains on the compacted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummaryCapture {
    pub schema: CompactionSchemaVersion,
    pub capture_id: CaptureId,
    pub project_id: ProjectId,
    pub source_session_id: SessionId,
    pub source_message_count: usize,
    pub first_turn_ordinal: u64,
    pub last_turn_ordinal: u64,
    pub source_turn_range_digest: MessageRangeDigest,
    pub summary: BoundedText,
    pub summary_origin: CompactionSummaryOrigin,
    pub prompt_stack_digest: PromptStackDigest,
    pub redaction_policy: agent_core::compaction::RedactionPolicy,
    pub content_classes: Vec<agent_core::compaction::ContentClass>,
    pub summarized_at: SystemTime,
    pub retention: RetentionClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryCaptureBuildError {
    ForeignProject,
    EmptySourceRange,
    InvalidTurnRange,
}

/// Build the bounded first-class memory emitted after a persisted compaction
/// transition. The source digest and session are copied from the transition's
/// typed provenance rather than recomputed from the post-compaction history.
pub fn build_conversation_summary_capture(
    expected_project: &ProjectId,
    source: CompactionSource,
    source_message_count: usize,
    summary_text: &str,
    redaction_policy: agent_core::compaction::RedactionPolicy,
    retention: RetentionClass,
) -> Result<ConversationSummaryCapture, SummaryCaptureBuildError> {
    if &source.project_id != expected_project {
        return Err(SummaryCaptureBuildError::ForeignProject);
    }
    if source_message_count == 0 {
        return Err(SummaryCaptureBuildError::EmptySourceRange);
    }
    if source.first_turn_ordinal > source.last_turn_ordinal {
        return Err(SummaryCaptureBuildError::InvalidTurnRange);
    }

    let mut digest = Sha256::new();
    digest.update(b"synaps.conversation-summary-capture-id.v1\0");
    digest_text(&mut digest, expected_project.as_str());
    digest_text(&mut digest, source.source_session_id.as_str());
    digest_u64(&mut digest, source.first_turn_ordinal);
    digest_u64(&mut digest, source.last_turn_ordinal);
    digest.update(source.source_digest.as_bytes());
    let capture_id = CaptureId(digest.finalize().into());

    Ok(ConversationSummaryCapture {
        schema: source.schema,
        capture_id,
        project_id: source.project_id,
        source_session_id: source.source_session_id,
        source_message_count,
        first_turn_ordinal: source.first_turn_ordinal,
        last_turn_ordinal: source.last_turn_ordinal,
        source_turn_range_digest: source.source_digest,
        summary: BoundedText::new(summary_text, SUMMARY_CAPTURE_MAX_BYTES),
        summary_origin: source.summary_origin,
        prompt_stack_digest: source.prompt_stack_digest,
        redaction_policy,
        content_classes: source.content_classes,
        summarized_at: source.summarized_at,
        retention,
    })
}

/// Exact disclosure classes present after filtering, in deterministic order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDisclosure {
    pub classes: Vec<DisclosureClass>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurnCapture {
    pub schema: CaptureSchemaVersion,
    pub capture_id: CaptureId,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub turn_ordinal: u64,
    pub started_at: SystemTime,
    pub completed_at: SystemTime,
    pub user: CaptureSegment,
    pub assistant: CaptureSegment,
    pub tools: Vec<ToolCaptureSummary>,
    pub outcome: TurnOutcome,
    pub compaction: Option<CompactionSource>,
    pub source_digest: MessageRangeDigest,
    pub disclosure: CaptureDisclosure,
    pub sensitivity: Sensitivity,
    pub retention: RetentionClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureBuildError {
    ForeignProject,
    InvalidTimes,
    InvalidCompactionRange,
    MissingUser,
    MissingFinalAssistant,
    MissingToolName,
}

impl fmt::Display for CaptureBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ForeignProject => "capture contains foreign-project provenance",
            Self::InvalidTimes => "turn completion precedes turn start",
            Self::InvalidCompactionRange => "compaction source turn range is invalid",
            Self::MissingUser => "capture has no persistable canonical user message",
            Self::MissingFinalAssistant => "capture has no persistable final assistant message",
            Self::MissingToolName => "tool capture summary has no tool name",
        };
        f.write_str(message)
    }
}

impl std::error::Error for CaptureBuildError {}

/// Build one bounded capture from a typed terminal history snapshot.
///
/// Foreign provenance rejects the entire capture, even when attached to an
/// otherwise forbidden item. Forbidden content is never copied into the
/// capture. Secret sensitivity and `never_persist` disclosure fail closed.
pub fn build_chat_turn_capture(
    expected_project: &ProjectId,
    history: TerminalTurnHistory,
    retention: RetentionClass,
) -> Result<ChatTurnCapture, CaptureBuildError> {
    validate_provenance(expected_project, &history)?;

    let mut user = None;
    let mut assistant = None;
    let mut tools = Vec::new();
    let mut disclosure_classes = Vec::new();
    let mut sensitivity = Sensitivity::Normal;

    for item in &history.items {
        if !is_persistable_capture_item(item) {
            continue;
        }

        match item.class {
            CaptureContentClass::UserMessage => {
                sensitivity = sensitivity.max(item.sensitivity);
                insert_disclosure(&mut disclosure_classes, item.disclosure);
                user = Some(CaptureSegment {
                    content: BoundedText::new(&item.text, CAPTURE_SEGMENT_MAX_BYTES),
                    disclosure: item.disclosure,
                    sensitivity: item.sensitivity,
                });
            }
            CaptureContentClass::AssistantFinal => {
                sensitivity = sensitivity.max(item.sensitivity);
                insert_disclosure(&mut disclosure_classes, item.disclosure);
                assistant = Some(CaptureSegment {
                    content: BoundedText::new(&item.text, CAPTURE_SEGMENT_MAX_BYTES),
                    disclosure: item.disclosure,
                    sensitivity: item.sensitivity,
                });
            }
            CaptureContentClass::ToolSummary => {
                let name = item
                    .tool_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or(CaptureBuildError::MissingToolName)?;
                if tools.len() < MAX_TOOL_SUMMARIES {
                    sensitivity = sensitivity.max(item.sensitivity);
                    insert_disclosure(&mut disclosure_classes, item.disclosure);
                    tools.push(ToolCaptureSummary {
                        tool_name: BoundedText::new(name, TOOL_NAME_MAX_BYTES),
                        summary: BoundedText::new(&item.text, TOOL_SUMMARY_MAX_BYTES),
                        disclosure: item.disclosure,
                        sensitivity: item.sensitivity,
                    });
                }
            }
            CaptureContentClass::SystemPrompt
            | CaptureContentClass::DeveloperPrompt
            | CaptureContentClass::PrivateReasoning
            | CaptureContentClass::Secret
            | CaptureContentClass::NeverPersist
            | CaptureContentClass::RawBinary
            | CaptureContentClass::RawToolOutput => unreachable!("forbidden item passed filter"),
        }
    }

    let user = user.ok_or(CaptureBuildError::MissingUser)?;
    let assistant = assistant.ok_or(CaptureBuildError::MissingFinalAssistant)?;
    let source_digest = source_digest(&history);
    let capture_id = capture_id(&history, source_digest);

    Ok(ChatTurnCapture {
        schema: CaptureSchemaVersion::V1,
        capture_id,
        project_id: history.project_id,
        session_id: history.session_id,
        turn_id: history.turn_id,
        turn_ordinal: history.turn_ordinal,
        started_at: history.started_at,
        completed_at: history.completed_at,
        user,
        assistant,
        tools,
        outcome: history.outcome,
        compaction: history.compaction,
        source_digest,
        disclosure: CaptureDisclosure {
            classes: disclosure_classes,
        },
        sensitivity,
        retention,
    })
}

fn validate_provenance(
    expected_project: &ProjectId,
    history: &TerminalTurnHistory,
) -> Result<(), CaptureBuildError> {
    if &history.project_id != expected_project
        || history
            .items
            .iter()
            .any(|item| &item.project_id != expected_project)
        || history
            .compaction
            .as_ref()
            .is_some_and(|source| &source.project_id != expected_project)
    {
        return Err(CaptureBuildError::ForeignProject);
    }
    if history.completed_at < history.started_at {
        return Err(CaptureBuildError::InvalidTimes);
    }
    if history
        .compaction
        .as_ref()
        .is_some_and(|source| source.first_turn_ordinal > source.last_turn_ordinal)
    {
        return Err(CaptureBuildError::InvalidCompactionRange);
    }
    Ok(())
}

fn is_persistable_capture_item(item: &CanonicalCaptureItem) -> bool {
    item.class.is_capture_content()
        && item.class != CaptureContentClass::NeverPersist
        && item.sensitivity != Sensitivity::Secret
        && agent_core::core::disclosure::may_persist(item.disclosure)
}

fn insert_disclosure(classes: &mut Vec<DisclosureClass>, class: DisclosureClass) {
    if !classes.contains(&class) {
        classes.push(class);
        classes.sort_by_key(|class| disclosure_tag(*class));
    }
}

fn source_digest(history: &TerminalTurnHistory) -> MessageRangeDigest {
    let mut digest = Sha256::new();
    digest.update(b"synaps.chat-turn-source.v1\0");
    digest_text(&mut digest, history.project_id.as_str());
    digest_text(&mut digest, history.session_id.as_str());
    digest_text(&mut digest, history.turn_id.as_str());
    digest_u64(&mut digest, history.turn_ordinal);
    digest_time(&mut digest, history.started_at);
    digest_time(&mut digest, history.completed_at);
    digest_outcome(&mut digest, &history.outcome);

    for item in &history.items {
        if !is_persistable_capture_item(item) {
            continue;
        }
        digest.update([item.class.digest_tag()]);
        digest.update([disclosure_tag(item.disclosure)]);
        digest.update([sensitivity_tag(item.sensitivity)]);
        digest_text(&mut digest, item.tool_name.as_deref().unwrap_or(""));
        digest_text(&mut digest, &item.text);
    }

    match &history.compaction {
        None => digest.update([0]),
        Some(source) => {
            digest.update([1]);
            digest_compaction(&mut digest, source);
        }
    }

    MessageRangeDigest(digest.finalize().into())
}

fn capture_id(history: &TerminalTurnHistory, source: MessageRangeDigest) -> CaptureId {
    let mut digest = Sha256::new();
    digest.update(b"synaps.chat-turn-capture-id.v1\0");
    digest_text(&mut digest, history.project_id.as_str());
    digest_text(&mut digest, history.session_id.as_str());
    digest_text(&mut digest, history.turn_id.as_str());
    digest_u64(&mut digest, history.turn_ordinal);
    digest.update(source.as_bytes());
    CaptureId(digest.finalize().into())
}

fn digest_compaction(digest: &mut Sha256, source: &CompactionSource) {
    digest.update([match source.schema {
        CompactionSchemaVersion::V1 => 1,
    }]);
    digest_text(digest, source.project_id.as_str());
    digest_text(digest, source.source_session_id.as_str());
    digest_u64(digest, source.first_turn_ordinal);
    digest_u64(digest, source.last_turn_ordinal);
    digest.update(source.source_digest.as_bytes());
    match &source.summary_origin {
        CompactionSummaryOrigin::LocalOnly => digest.update([0]),
        CompactionSummaryOrigin::Provider {
            provider_id,
            model_id,
        } => {
            digest.update([1]);
            digest_text(digest, provider_id);
            digest_text(digest, model_id);
        }
    }
    digest.update(source.prompt_stack_digest.as_bytes());
    digest.update([match source.redaction {
        RedactionPolicy::None => 0,
        RedactionPolicy::HostRedacted => 1,
    }]);
    digest_u64(digest, source.content_classes.len() as u64);
    for class in &source.content_classes {
        digest_text(digest, class.as_str());
    }
    digest_time(digest, source.summarized_at);
}

fn digest_outcome(digest: &mut Sha256, outcome: &TurnOutcome) {
    match outcome {
        TurnOutcome::Completed => digest.update([0]),
        TurnOutcome::Canceled => digest.update([1]),
        TurnOutcome::ProviderFailed {
            code,
            correlation_id,
        } => {
            digest.update([2]);
            digest_text(digest, code);
            digest_text(digest, correlation_id);
        }
        TurnOutcome::ToolFailed {
            tool_id,
            correlation_id,
        } => {
            digest.update([3]);
            digest_text(digest, tool_id);
            digest_text(digest, correlation_id);
        }
        TurnOutcome::BudgetExceeded { dimension } => {
            digest.update([4]);
            digest.update([budget_dimension_tag(*dimension)]);
        }
        TurnOutcome::InterruptedAfterSideEffect { call_id } => {
            digest.update([5]);
            digest_text(digest, call_id);
        }
    }
}

fn budget_dimension_tag(dimension: BudgetDimension) -> u8 {
    match dimension {
        BudgetDimension::InputTokens => 0,
        BudgetDimension::OutputTokens => 1,
        BudgetDimension::ToolCalls => 2,
        BudgetDimension::WallClock => 3,
        BudgetDimension::ProviderRounds => 4,
        BudgetDimension::ToolResultBytes => 5,
        BudgetDimension::CostUsd => 6,
    }
}

fn disclosure_tag(class: DisclosureClass) -> u8 {
    match class {
        DisclosureClass::ModelVisible => 0,
        DisclosureClass::LocalOnly => 1,
        DisclosureClass::ModelVisibleAfterRedaction => 2,
        DisclosureClass::ModelVisibleAfterConsent => 3,
        DisclosureClass::PersistNeverTransmit => 4,
        DisclosureClass::NeverPersist => 5,
    }
}

fn sensitivity_tag(sensitivity: Sensitivity) -> u8 {
    match sensitivity {
        Sensitivity::Normal => 0,
        Sensitivity::Sensitive => 1,
        Sensitivity::Secret => 2,
    }
}

fn digest_text(digest: &mut Sha256, text: &str) {
    digest_u64(digest, text.len() as u64);
    digest.update(text.as_bytes());
}

fn digest_u64(digest: &mut Sha256, value: u64) {
    digest.update(value.to_be_bytes());
}

fn digest_time(digest: &mut Sha256, time: SystemTime) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            digest.update([0]);
            digest_u64(digest, duration.as_secs());
            digest.update(duration.subsec_nanos().to_be_bytes());
        }
        Err(error) => {
            digest.update([1]);
            digest_u64(digest, error.duration().as_secs());
            digest.update(error.duration().subsec_nanos().to_be_bytes());
        }
    }
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16)? as u8;
        let low = (pair[1] as char).to_digit(16)? as u8;
        bytes[index] = (high << 4) | low;
    }
    Some(bytes)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const FORBIDDEN: &str = "FORBIDDEN-CONTENT-SENTINEL";

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).unwrap()
    }

    fn item(class: CaptureContentClass, text: &str) -> CanonicalCaptureItem {
        CanonicalCaptureItem {
            project_id: project("project-a"),
            class,
            disclosure: DisclosureClass::ModelVisible,
            sensitivity: Sensitivity::Normal,
            text: text.to_string(),
            tool_name: None,
        }
    }

    fn tool(text: &str) -> CanonicalCaptureItem {
        CanonicalCaptureItem {
            tool_name: Some("bash".to_string()),
            ..item(CaptureContentClass::ToolSummary, text)
        }
    }

    fn terminal(items: Vec<CanonicalCaptureItem>) -> TerminalTurnHistory {
        TerminalTurnHistory {
            project_id: project("project-a"),
            session_id: SessionId::parse("session-1").unwrap(),
            turn_id: TurnId::parse("turn-7").unwrap(),
            turn_ordinal: 7,
            started_at: UNIX_EPOCH + Duration::from_secs(10),
            completed_at: UNIX_EPOCH + Duration::from_secs(20),
            outcome: TurnOutcome::Completed,
            items,
            compaction: None,
        }
    }

    fn required_items() -> Vec<CanonicalCaptureItem> {
        vec![
            item(CaptureContentClass::UserMessage, "user"),
            item(CaptureContentClass::AssistantFinal, "assistant"),
        ]
    }

    fn build(history: TerminalTurnHistory) -> Result<ChatTurnCapture, CaptureBuildError> {
        build_chat_turn_capture(&project("project-a"), history, RetentionClass::Standard)
    }

    #[test]
    fn bounds_user_assistant_and_tool_summaries() {
        let mut items = vec![
            item(CaptureContentClass::UserMessage, &"u".repeat(5000)),
            item(CaptureContentClass::AssistantFinal, &"a".repeat(5000)),
        ];
        items.extend((0..MAX_TOOL_SUMMARIES + 3).map(|_| tool(&"t".repeat(2000))));

        let capture = build(terminal(items)).unwrap();

        assert_eq!(
            capture.user.content.retained_bytes,
            CAPTURE_SEGMENT_MAX_BYTES
        );
        assert!(capture.user.content.truncated);
        assert_eq!(
            capture.assistant.content.retained_bytes,
            CAPTURE_SEGMENT_MAX_BYTES
        );
        assert!(capture.assistant.content.truncated);
        assert_eq!(capture.tools.len(), MAX_TOOL_SUMMARIES);
        assert!(capture
            .tools
            .iter()
            .all(|tool| tool.summary.retained_bytes == TOOL_SUMMARY_MAX_BYTES
                && tool.summary.truncated));
    }

    #[test]
    fn filters_every_forbidden_class_and_never_persist_or_secret_content() {
        let mut items = required_items();
        for class in [
            CaptureContentClass::SystemPrompt,
            CaptureContentClass::DeveloperPrompt,
            CaptureContentClass::PrivateReasoning,
            CaptureContentClass::Secret,
            CaptureContentClass::NeverPersist,
            CaptureContentClass::RawBinary,
            CaptureContentClass::RawToolOutput,
        ] {
            items.push(item(class, FORBIDDEN));
        }
        let mut never_persist = tool(FORBIDDEN);
        never_persist.disclosure = DisclosureClass::NeverPersist;
        items.push(never_persist);
        let mut secret = tool(FORBIDDEN);
        secret.sensitivity = Sensitivity::Secret;
        items.push(secret);

        let capture = build(terminal(items)).unwrap();
        let rendered = format!("{capture:?}");

        assert!(!rendered.contains(FORBIDDEN));
        assert!(capture.tools.is_empty());
        assert!(!capture
            .disclosure
            .classes
            .contains(&DisclosureClass::NeverPersist));
        assert_ne!(capture.sensitivity, Sensitivity::Secret);
    }

    #[test]
    fn rejects_foreign_project_even_when_item_content_would_be_filtered() {
        let mut items = required_items();
        let mut foreign = item(CaptureContentClass::SystemPrompt, FORBIDDEN);
        foreign.project_id = project("project-b");
        items.push(foreign);

        assert_eq!(
            build(terminal(items)),
            Err(CaptureBuildError::ForeignProject)
        );
    }

    #[test]
    fn source_digest_and_capture_id_are_deterministic_and_content_sensitive() {
        let history = terminal(required_items());
        let first = build(history.clone()).unwrap();
        let second = build(history.clone()).unwrap();
        let mut changed = history;
        changed.items[1].text.push('!');
        let changed = build(changed).unwrap();

        assert_eq!(first.source_digest, second.source_digest);
        assert_eq!(first.capture_id, second.capture_id);
        assert_eq!(first.capture_id.to_hex().len(), 64);
        assert_ne!(first.source_digest, changed.source_digest);
        assert_ne!(first.capture_id, changed.capture_id);
    }

    #[test]
    fn interrupted_non_idempotent_outcome_is_never_clean_success() {
        let mut history = terminal(required_items());
        history.outcome = TurnOutcome::InterruptedAfterSideEffect {
            call_id: "call-9".to_string(),
        };

        let capture = build(history).unwrap();

        assert_eq!(
            capture.outcome,
            TurnOutcome::InterruptedAfterSideEffect {
                call_id: "call-9".to_string()
            }
        );
        assert_ne!(capture.outcome, TurnOutcome::Completed);
    }

    #[test]
    fn conversation_summary_builder_is_bounded_and_digest_linked() {
        let source_digest = MessageRangeDigest::from_bytes([7; 32]);
        let source = CompactionSource {
            schema: CompactionSchemaVersion::V1,
            project_id: project("project-a"),
            source_session_id: SessionId::parse("session-previous").unwrap(),
            first_turn_ordinal: 2,
            last_turn_ordinal: 5,
            source_digest,
            summary_origin: CompactionSummaryOrigin::Provider {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet-4-6".into(),
            },
            prompt_stack_digest: PromptStackDigest::from_bytes([8; 32]),
            redaction: RedactionPolicy::HostRedacted,
            content_classes: vec![agent_core::compaction::ContentClass::UserText],
            summarized_at: UNIX_EPOCH + Duration::from_secs(9),
        };

        let summary = build_conversation_summary_capture(
            &project("project-a"),
            source,
            4,
            &"s".repeat(SUMMARY_CAPTURE_MAX_BYTES + 1),
            agent_core::compaction::RedactionPolicy::PolicyExclusions,
            RetentionClass::Standard,
        )
        .unwrap();

        assert_eq!(summary.source_session_id.as_str(), "session-previous");
        assert_eq!(summary.source_turn_range_digest, source_digest);
        assert_eq!(summary.first_turn_ordinal, 2);
        assert_eq!(summary.last_turn_ordinal, 5);
        assert_eq!(
            summary.redaction_policy,
            agent_core::compaction::RedactionPolicy::PolicyExclusions
        );
        assert_eq!(summary.summary.retained_bytes, SUMMARY_CAPTURE_MAX_BYTES);
        assert!(summary.summary.truncated);
    }

    #[test]
    fn carries_times_ordinal_disclosure_retention_sensitivity_and_compaction() {
        let mut items = required_items();
        items[0].disclosure = DisclosureClass::LocalOnly;
        items[1].sensitivity = Sensitivity::Sensitive;
        let mut history = terminal(items);
        let compaction_digest = MessageRangeDigest([7; 32]);
        history.compaction = Some(CompactionSource {
            schema: CompactionSchemaVersion::V1,
            project_id: project("project-a"),
            source_session_id: SessionId::parse("session-previous").unwrap(),
            first_turn_ordinal: 1,
            last_turn_ordinal: 6,
            source_digest: compaction_digest,
            summary_origin: CompactionSummaryOrigin::LocalOnly,
            prompt_stack_digest: PromptStackDigest::from_bytes([8; 32]),
            redaction: RedactionPolicy::HostRedacted,
            content_classes: vec![agent_core::compaction::ContentClass::UserText],
            summarized_at: UNIX_EPOCH + Duration::from_secs(9),
        });

        let capture = build(history).unwrap();

        assert_eq!(capture.turn_ordinal, 7);
        assert_eq!(capture.started_at, UNIX_EPOCH + Duration::from_secs(10));
        assert_eq!(capture.completed_at, UNIX_EPOCH + Duration::from_secs(20));
        assert_eq!(capture.sensitivity, Sensitivity::Sensitive);
        assert_eq!(capture.retention, RetentionClass::Standard);
        assert_eq!(
            capture.disclosure.classes,
            vec![DisclosureClass::ModelVisible, DisclosureClass::LocalOnly]
        );
        assert_eq!(capture.compaction.unwrap().source_digest, compaction_digest);
    }
}
