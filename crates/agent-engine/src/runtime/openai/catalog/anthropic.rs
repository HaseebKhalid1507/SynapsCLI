use super::*;
use agent_core::reasoning::ReasoningLevel;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicExecutionMode {
    Standard,
    Max,
    UltraCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicWireEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl AnthropicWireEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicWorkflowPlan {
    None,
    Standing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicPlanPrerequisites {
    pub orchestration_policy: bool,
    pub foreground_worker_authorized: bool,
    pub concurrent_limit: usize,
    pub total_limit: usize,
    pub lifecycle_start: bool,
    pub lifecycle_status: bool,
    pub lifecycle_steer: bool,
    pub lifecycle_collect: bool,
    pub lifecycle_resume: bool,
}

impl AnthropicPlanPrerequisites {
    pub const fn installed() -> Self {
        Self {
            orchestration_policy: true,
            foreground_worker_authorized: true,
            concurrent_limit: 1,
            total_limit: 1,
            lifecycle_start: true,
            lifecycle_status: true,
            lifecycle_steer: true,
            lifecycle_collect: true,
            lifecycle_resume: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicPlanErrorCode {
    InvalidProviderIdentity,
    CapabilityMetadataMissing,
    UnsupportedReasoningLevel,
    UltraCodeRequiresForeground,
    UltraCodeRequiresOrchestration,
    UltraCodeRequiresLifecycleTools,
    UltraCodeRequiresWorkerAuthorization,
    UltraCodeRequiresLimits,
}

impl AnthropicPlanErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProviderIdentity => "invalid_provider_identity",
            Self::CapabilityMetadataMissing => "capability_metadata_missing",
            Self::UnsupportedReasoningLevel => "unsupported_reasoning_level",
            Self::UltraCodeRequiresForeground => "ultracode_requires_foreground",
            Self::UltraCodeRequiresOrchestration => "ultracode_requires_orchestration",
            Self::UltraCodeRequiresLifecycleTools => "ultracode_requires_lifecycle_tools",
            Self::UltraCodeRequiresWorkerAuthorization => "ultracode_requires_worker_authorization",
            Self::UltraCodeRequiresLimits => "ultracode_requires_limits",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicPlanError(AnthropicPlanErrorCode);
impl AnthropicPlanError {
    pub const fn code(&self) -> AnthropicPlanErrorCode {
        self.0
    }
}
impl std::fmt::Display for AnthropicPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}
impl std::error::Error for AnthropicPlanError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicExecutionPlan {
    pub qualified_model: String,
    pub requested_level: ReasoningLevel,
    pub role: ExecutionRole,
    pub mode: AnthropicExecutionMode,
    pub wire_effort: Option<AnthropicWireEffort>,
    pub workflow: AnthropicWorkflowPlan,
}

pub(super) const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
pub(super) const ANTHROPIC_MODELS_PAGE_LIMIT: usize = 100;

/// Exact, evidence-backed Anthropic logical-mode capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicModeCapabilities {
    pub max_supported: bool,
    pub xhigh_supported: bool,
    pub workflow_supported: bool,
}

impl AnthropicModeCapabilities {
    pub const fn none() -> Self {
        Self {
            max_supported: false,
            xhigh_supported: false,
            workflow_supported: false,
        }
    }

    pub const fn ultracode_supported(self) -> bool {
        self.xhigh_supported && self.workflow_supported
    }

    /// Generic live effort metadata can only preserve or revoke special modes.
    pub const fn narrow_with_live_effort(mut self, supported: Option<bool>) -> Self {
        if matches!(supported, Some(false)) {
            self.max_supported = false;
            self.xhigh_supported = false;
            self.workflow_supported = false;
        }
        self
    }
}

/// Source-controlled schema for exact Anthropic logical-mode authority.
const ANTHROPIC_MODE_MANIFEST_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicManifestRow {
    pub qualified_id: &'static str,
    pub max_supported: bool,
    pub xhigh_supported: bool,
    pub workflow_supported: bool,
    pub evidence: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicCapabilityManifest {
    pub schema_version: u16,
    pub rows: &'static [AnthropicManifestRow],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicManifestErrorCode {
    UnsupportedVersion,
    MalformedQualifiedId,
    DuplicateId,
    ContradictoryCapabilities,
    EvidenceMissing,
}

#[cfg(test)]
impl AnthropicManifestErrorCode {
    /// Stable diagnostic identifier. Typed const manifests cannot contain
    /// unknown enum values, so diagnostics intentionally never echo raw input.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedVersion => "unsupported_version",
            Self::MalformedQualifiedId => "malformed_qualified_id",
            Self::DuplicateId => "duplicate_id",
            Self::ContradictoryCapabilities => "contradictory_capabilities",
            Self::EvidenceMissing => "evidence_missing",
        }
    }
}

impl AnthropicCapabilityManifest {
    pub fn validate(self) -> Result<(), AnthropicManifestErrorCode> {
        if self.schema_version != ANTHROPIC_MODE_MANIFEST_VERSION {
            return Err(AnthropicManifestErrorCode::UnsupportedVersion);
        }
        let mut ids = std::collections::BTreeSet::new();
        for row in self.rows {
            let Some(id) = row.qualified_id.strip_prefix("anthropic/") else {
                return Err(AnthropicManifestErrorCode::MalformedQualifiedId);
            };
            if id.is_empty() || id.contains('/') {
                return Err(AnthropicManifestErrorCode::MalformedQualifiedId);
            }
            if !ids.insert(row.qualified_id) {
                return Err(AnthropicManifestErrorCode::DuplicateId);
            }
            if row.workflow_supported && !row.xhigh_supported {
                return Err(AnthropicManifestErrorCode::ContradictoryCapabilities);
            }
            if (row.max_supported || row.workflow_supported) && row.evidence.trim().is_empty() {
                return Err(AnthropicManifestErrorCode::EvidenceMissing);
            }
        }
        Ok(())
    }
}

const ANTHROPIC_MODE_ROWS: &[AnthropicManifestRow] = &[AnthropicManifestRow {
    qualified_id: "anthropic/claude-fable-5",
    max_supported: true,
    xhigh_supported: true,
    workflow_supported: true,
    evidence: "Claude Code 2.1.207; sha256:85e7e988a392d859f90802ca21fb26e89d3c9ab527f5ed0b08df3955e34d5c83",
}];
const ANTHROPIC_MODE_MANIFEST: AnthropicCapabilityManifest = AnthropicCapabilityManifest {
    schema_version: ANTHROPIC_MODE_MANIFEST_VERSION,
    rows: ANTHROPIC_MODE_ROWS,
};

/// Look up exact qualified identities only. The complete manifest is validated
/// before each decision; malformed source authority therefore fails closed.
pub fn anthropic_mode_capabilities(qualified_model: &str) -> Option<AnthropicModeCapabilities> {
    ANTHROPIC_MODE_MANIFEST.validate().ok()?;
    let row = ANTHROPIC_MODE_MANIFEST
        .rows
        .iter()
        .find(|row| row.qualified_id == qualified_model)?;
    Some(AnthropicModeCapabilities {
        max_supported: row.max_supported,
        xhigh_supported: row.xhigh_supported,
        workflow_supported: row.workflow_supported,
    })
}

pub fn plan_standard_anthropic_transport(
    model: &str,
    requested_level: ReasoningLevel,
    role: ExecutionRole,
) -> Option<AnthropicExecutionPlan> {
    if matches!(
        requested_level,
        ReasoningLevel::Max | ReasoningLevel::UltraCode
    ) {
        return None;
    }
    let wire_effort = match requested_level {
        ReasoningLevel::Low => Some(AnthropicWireEffort::Low),
        ReasoningLevel::Medium => Some(AnthropicWireEffort::Medium),
        ReasoningLevel::High => Some(AnthropicWireEffort::High),
        ReasoningLevel::XHigh => Some(AnthropicWireEffort::XHigh),
        _ => None,
    };
    Some(AnthropicExecutionPlan {
        qualified_model: model.to_owned(),
        requested_level,
        role,
        mode: AnthropicExecutionMode::Standard,
        wire_effort,
        workflow: AnthropicWorkflowPlan::None,
    })
}

pub fn plan_anthropic_execution(
    qualified_model: &str,
    requested_level: ReasoningLevel,
    role: ExecutionRole,
    prerequisites: AnthropicPlanPrerequisites,
    live_effort_supported: Option<bool>,
) -> Result<AnthropicExecutionPlan, AnthropicPlanError> {
    let Some(id) = qualified_model.strip_prefix("anthropic/") else {
        return Err(AnthropicPlanError(
            AnthropicPlanErrorCode::InvalidProviderIdentity,
        ));
    };
    if id.is_empty() || id.contains('/') {
        return Err(AnthropicPlanError(
            AnthropicPlanErrorCode::InvalidProviderIdentity,
        ));
    }
    if requested_level == ReasoningLevel::Ultra {
        return Err(AnthropicPlanError(
            AnthropicPlanErrorCode::UnsupportedReasoningLevel,
        ));
    }
    let capabilities = anthropic_mode_capabilities(qualified_model)
        .map(|caps| caps.narrow_with_live_effort(live_effort_supported));
    if matches!(
        requested_level,
        ReasoningLevel::Max | ReasoningLevel::UltraCode
    ) && capabilities.is_none()
    {
        return Err(AnthropicPlanError(
            AnthropicPlanErrorCode::CapabilityMetadataMissing,
        ));
    }
    if match requested_level {
        ReasoningLevel::Max => !capabilities.is_some_and(|caps| caps.max_supported),
        ReasoningLevel::UltraCode => !capabilities.is_some_and(|caps| caps.ultracode_supported()),
        _ => false,
    } {
        return Err(AnthropicPlanError(
            AnthropicPlanErrorCode::UnsupportedReasoningLevel,
        ));
    }
    if requested_level == ReasoningLevel::UltraCode {
        if role != ExecutionRole::Foreground {
            return Err(AnthropicPlanError(
                AnthropicPlanErrorCode::UltraCodeRequiresForeground,
            ));
        }
        if !prerequisites.orchestration_policy {
            return Err(AnthropicPlanError(
                AnthropicPlanErrorCode::UltraCodeRequiresOrchestration,
            ));
        }
        if !prerequisites.foreground_worker_authorized {
            return Err(AnthropicPlanError(
                AnthropicPlanErrorCode::UltraCodeRequiresWorkerAuthorization,
            ));
        }
        if prerequisites.concurrent_limit == 0 || prerequisites.total_limit == 0 {
            return Err(AnthropicPlanError(
                AnthropicPlanErrorCode::UltraCodeRequiresLimits,
            ));
        }
        if ![
            prerequisites.lifecycle_start,
            prerequisites.lifecycle_status,
            prerequisites.lifecycle_steer,
            prerequisites.lifecycle_collect,
            prerequisites.lifecycle_resume,
        ]
        .into_iter()
        .all(|present| present)
        {
            return Err(AnthropicPlanError(
                AnthropicPlanErrorCode::UltraCodeRequiresLifecycleTools,
            ));
        }
    }
    let wire_effort = match requested_level {
        ReasoningLevel::Low => Some(AnthropicWireEffort::Low),
        ReasoningLevel::Medium => Some(AnthropicWireEffort::Medium),
        ReasoningLevel::High => Some(AnthropicWireEffort::High),
        ReasoningLevel::XHigh | ReasoningLevel::UltraCode => Some(AnthropicWireEffort::XHigh),
        ReasoningLevel::Max => Some(AnthropicWireEffort::Max),
        _ => None,
    };
    Ok(AnthropicExecutionPlan {
        qualified_model: qualified_model.to_owned(),
        requested_level,
        role,
        mode: match requested_level {
            ReasoningLevel::Max => AnthropicExecutionMode::Max,
            ReasoningLevel::UltraCode => AnthropicExecutionMode::UltraCode,
            _ => AnthropicExecutionMode::Standard,
        },
        wire_effort,
        workflow: if requested_level == ReasoningLevel::UltraCode {
            AnthropicWorkflowPlan::Standing
        } else {
            AnthropicWorkflowPlan::None
        },
    })
}

#[cfg(test)]
mod manifest_validation_tests {
    use super::*;
    const GOOD: AnthropicManifestRow = AnthropicManifestRow {
        qualified_id: "anthropic/claude-fable-5",
        max_supported: true,
        xhigh_supported: true,
        workflow_supported: true,
        evidence: "exact evidence",
    };
    fn manifest(
        version: u16,
        rows: &'static [AnthropicManifestRow],
    ) -> AnthropicCapabilityManifest {
        AnthropicCapabilityManifest {
            schema_version: version,
            rows,
        }
    }
    #[test]
    fn dedicated_manifest_failures_and_exact_fable_success() {
        assert_eq!(
            manifest(2, &[GOOD]).validate(),
            Err(AnthropicManifestErrorCode::UnsupportedVersion)
        );
        assert_eq!(
            manifest(
                1,
                &[AnthropicManifestRow {
                    qualified_id: "anthropic/fable/5",
                    ..GOOD
                }]
            )
            .validate(),
            Err(AnthropicManifestErrorCode::MalformedQualifiedId)
        );
        assert_eq!(
            manifest(1, &[GOOD, GOOD]).validate(),
            Err(AnthropicManifestErrorCode::DuplicateId)
        );
        assert_eq!(
            manifest(
                1,
                &[AnthropicManifestRow {
                    xhigh_supported: false,
                    ..GOOD
                }]
            )
            .validate(),
            Err(AnthropicManifestErrorCode::ContradictoryCapabilities)
        );
        assert_eq!(
            manifest(
                1,
                &[AnthropicManifestRow {
                    evidence: " ",
                    ..GOOD
                }]
            )
            .validate(),
            Err(AnthropicManifestErrorCode::EvidenceMissing)
        );
        assert_eq!(manifest(1, &[GOOD]).validate(), Ok(()));
        assert!(anthropic_mode_capabilities("anthropic/claude-fable-5").is_some());
        assert!(anthropic_mode_capabilities("anthropic/claude-fable-5-near").is_none());
    }
    #[test]
    fn stable_errors_are_input_free() {
        assert_eq!(
            [
                AnthropicManifestErrorCode::UnsupportedVersion.as_str(),
                AnthropicManifestErrorCode::MalformedQualifiedId.as_str(),
                AnthropicManifestErrorCode::DuplicateId.as_str(),
                AnthropicManifestErrorCode::ContradictoryCapabilities.as_str(),
                AnthropicManifestErrorCode::EvidenceMissing.as_str()
            ],
            [
                "unsupported_version",
                "malformed_qualified_id",
                "duplicate_id",
                "contradictory_capabilities",
                "evidence_missing"
            ]
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicCatalogPage {
    pub models: Vec<CatalogModel>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsPage {
    data: Vec<AnthropicModelItem>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelItem {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<AnthropicCapabilities>,
}

#[derive(Debug, Deserialize)]
struct AnthropicCapabilities {
    #[serde(default)]
    thinking: Option<CapabilitySupported>,
    #[serde(default)]
    effort: Option<AnthropicEffortCapability>,
}

#[derive(Debug, Deserialize)]
struct CapabilitySupported {
    #[serde(default)]
    supported: bool,
}

#[derive(Debug, Deserialize)]
struct AnthropicEffortCapability {
    #[serde(default)]
    supported: bool,
}

pub fn parse_anthropic_catalog_page(body: &str) -> Result<AnthropicCatalogPage, serde_json::Error> {
    let page: AnthropicModelsPage = serde_json::from_str(body)?;
    let models: Vec<CatalogModel> = page
        .data
        .into_iter()
        .filter_map(|item| {
            let mut m = CatalogModel::new("anthropic", "Anthropic", item.id)?;
            m.provider_kind = CatalogProviderKind::Anthropic;
            m.label = item.display_name.filter(|name| !name.trim().is_empty());
            m.context_tokens = item.max_input_tokens;
            m.max_output_tokens = item.max_tokens;
            m.reasoning = match item.capabilities {
                Some(caps) => match caps.thinking.as_ref() {
                    Some(thinking) if thinking.supported => ReasoningSupport::AnthropicAdaptive {
                        adaptive: caps.effort.as_ref().is_some_and(|c| c.supported),
                    },
                    // Explicit evidence that thinking is unsupported → None
                    // (named reasoning fails closed for this model).
                    Some(_) => ReasoningSupport::None,
                    // Capabilities present but thinking omitted → Unknown.
                    None => ReasoningSupport::Unknown,
                },
                _ => ReasoningSupport::Unknown,
            };
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect();

    // Populate the process-local capability cache (keyed by "anthropic/<id>")
    // so validation and dynamic option derivation see live data.
    super::capability_cache::populate(&models);

    Ok(AnthropicCatalogPage {
        models,
        has_more: page.has_more,
        last_id: page.last_id.filter(|id| !id.trim().is_empty()),
    })
}

pub fn parse_anthropic_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    parse_anthropic_catalog_page(body).map(|page| page.models)
}

/// Conservative exact static fallback descriptors for the source-controlled
/// known-model list (`agent_core::models::KNOWN_MODELS`). Exact ids only —
/// never substring-based. Evidence: adaptive-thinking notes in
/// `crates/agent-core/src/core/models.rs` (Opus 4.7+ / Fable 5 adaptive+effort;
/// Sonnet 4.6, Opus 4.6, Haiku 4.5 fixed-budget thinking).
pub fn anthropic_static_capability(model_id: &str) -> Option<ReasoningSupport> {
    match model_id {
        "claude-opus-4-7" | "claude-fable-5" => {
            Some(ReasoningSupport::AnthropicAdaptive { adaptive: true })
        }
        "claude-sonnet-4-6" | "claude-opus-4-6" | "claude-haiku-4-5-20251001" => {
            Some(ReasoningSupport::AnthropicAdaptive { adaptive: false })
        }
        _ => None,
    }
}

pub fn anthropic_models_url(after_id: Option<&str>) -> String {
    let mut url = format!("{ANTHROPIC_MODELS_URL}?limit={ANTHROPIC_MODELS_PAGE_LIMIT}");
    if let Some(after_id) = after_id.filter(|id| !id.trim().is_empty()) {
        url.push_str("&after_id=");
        url.push_str(after_id);
    }
    url
}

pub fn merge_catalog_pages(pages: Vec<Vec<CatalogModel>>) -> Vec<CatalogModel> {
    let mut seen = std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for page in pages {
        for model in page {
            if seen.insert(model.id.clone()) {
                merged.push(model);
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_transport_accepts_bare_or_unknown_models_but_special_modes_require_authority() {
        for model in ["claude-haiku-4-5", "anthropic/unknown-model"] {
            for level in [
                ReasoningLevel::Off,
                ReasoningLevel::Adaptive,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
            ] {
                let plan =
                    plan_standard_anthropic_transport(model, level, ExecutionRole::Foreground)
                        .expect("ordinary transport plan");
                assert_eq!(plan.mode, AnthropicExecutionMode::Standard);
                assert_eq!(plan.qualified_model, model);
            }
            assert!(plan_standard_anthropic_transport(
                model,
                ReasoningLevel::Max,
                ExecutionRole::Foreground
            )
            .is_none());
            assert!(plan_standard_anthropic_transport(
                model,
                ReasoningLevel::UltraCode,
                ExecutionRole::Foreground
            )
            .is_none());
        }
    }

    #[test]
    fn exact_fable_mode_capabilities_are_evidence_locked() {
        let caps = anthropic_mode_capabilities("anthropic/claude-fable-5")
            .expect("exact qualified Fable 5 row");
        assert!(caps.max_supported);
        assert!(caps.xhigh_supported);
        assert!(caps.workflow_supported);
        assert!(caps.ultracode_supported());

        for denied in [
            "anthropic/claude-fable-5-preview",
            "anthropic/claude-opus-4-7",
            "openai/claude-fable-5",
            "claude-fable-5",
            "anthropic/fable-5",
        ] {
            assert_eq!(anthropic_mode_capabilities(denied), None, "{denied}");
        }
    }

    #[test]
    fn generic_live_effort_cannot_invent_special_modes_and_can_revoke() {
        let static_caps = anthropic_mode_capabilities("anthropic/claude-fable-5").unwrap();
        assert_eq!(static_caps.narrow_with_live_effort(Some(true)), static_caps);
        let revoked = static_caps.narrow_with_live_effort(Some(false));
        assert!(!revoked.max_supported);
        assert!(!revoked.ultracode_supported());

        assert_eq!(
            AnthropicModeCapabilities::none().narrow_with_live_effort(Some(true)),
            AnthropicModeCapabilities::none()
        );
    }

    #[test]
    fn typed_planner_maps_exact_modes_and_fails_closed() {
        let prerequisites = AnthropicPlanPrerequisites::installed();
        let plan = plan_anthropic_execution(
            "anthropic/claude-fable-5",
            ReasoningLevel::UltraCode,
            ExecutionRole::Foreground,
            prerequisites,
            None,
        )
        .unwrap();
        assert_eq!(plan.mode, AnthropicExecutionMode::UltraCode);
        assert_eq!(plan.wire_effort, Some(AnthropicWireEffort::XHigh));
        assert_eq!(plan.workflow, AnthropicWorkflowPlan::Standing);

        for (level, wire, mode) in [
            (
                ReasoningLevel::Max,
                AnthropicWireEffort::Max,
                AnthropicExecutionMode::Max,
            ),
            (
                ReasoningLevel::XHigh,
                AnthropicWireEffort::XHigh,
                AnthropicExecutionMode::Standard,
            ),
        ] {
            let plan = plan_anthropic_execution(
                "anthropic/claude-fable-5",
                level,
                ExecutionRole::Foreground,
                prerequisites,
                None,
            )
            .unwrap();
            assert_eq!(plan.mode, mode);
            assert_eq!(plan.wire_effort, Some(wire));
            assert_eq!(plan.workflow, AnthropicWorkflowPlan::None);
        }

        for (model, level, role, prerequisites, code) in [
            (
                "openai-codex/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Foreground,
                prerequisites,
                AnthropicPlanErrorCode::InvalidProviderIdentity,
            ),
            (
                "anthropic/claude-fable-5-preview",
                ReasoningLevel::Max,
                ExecutionRole::Foreground,
                prerequisites,
                AnthropicPlanErrorCode::CapabilityMetadataMissing,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::Ultra,
                ExecutionRole::Foreground,
                prerequisites,
                AnthropicPlanErrorCode::UnsupportedReasoningLevel,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Worker,
                prerequisites,
                AnthropicPlanErrorCode::UltraCodeRequiresForeground,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Internal,
                prerequisites,
                AnthropicPlanErrorCode::UltraCodeRequiresForeground,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Foreground,
                AnthropicPlanPrerequisites {
                    orchestration_policy: false,
                    ..AnthropicPlanPrerequisites::installed()
                },
                AnthropicPlanErrorCode::UltraCodeRequiresOrchestration,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Foreground,
                AnthropicPlanPrerequisites {
                    lifecycle_start: false,
                    ..AnthropicPlanPrerequisites::installed()
                },
                AnthropicPlanErrorCode::UltraCodeRequiresLifecycleTools,
            ),
        ] {
            assert_eq!(
                plan_anthropic_execution(model, level, role, prerequisites, None)
                    .unwrap_err()
                    .code(),
                code
            );
        }
    }

    #[test]
    fn live_false_revokes_but_live_true_does_not_invent() {
        let installed = AnthropicPlanPrerequisites::installed();
        assert_eq!(
            plan_anthropic_execution(
                "anthropic/unknown",
                ReasoningLevel::Max,
                ExecutionRole::Foreground,
                installed,
                Some(true)
            )
            .unwrap_err()
            .code(),
            AnthropicPlanErrorCode::CapabilityMetadataMissing
        );
        assert_eq!(
            plan_anthropic_execution(
                "anthropic/claude-fable-5",
                ReasoningLevel::Max,
                ExecutionRole::Foreground,
                installed,
                Some(false)
            )
            .unwrap_err()
            .code(),
            AnthropicPlanErrorCode::UnsupportedReasoningLevel
        );
    }

    // ── Exact static fallback descriptors (spec: anthropic-xai-reasoning-modes) ──

    #[test]
    fn static_capability_covers_exact_known_models_only() {
        for id in ["claude-opus-4-7", "claude-fable-5"] {
            assert_eq!(
                anthropic_static_capability(id),
                Some(ReasoningSupport::AnthropicAdaptive { adaptive: true }),
                "{id}"
            );
        }
        for id in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(
                anthropic_static_capability(id),
                Some(ReasoningSupport::AnthropicAdaptive { adaptive: false }),
                "{id}"
            );
        }
        // No substring inference: near-miss ids fail closed.
        for id in [
            "claude-opus-4-7-preview",
            "claude-haiku-4-5",
            "opus-4-7",
            "",
        ] {
            assert_eq!(anthropic_static_capability(id), None, "{id}");
        }
    }

    /// Drift guard: the exact static capability table and the legacy
    /// substring wire-shape classifier (`model_supports_adaptive_thinking`,
    /// explicitly deferred — see spec) must agree on the adaptive/fixed wire
    /// shape for EVERY source-controlled descriptor. If either side changes,
    /// this test forces reconciling the other.
    #[test]
    fn static_table_and_wire_shape_classifier_agree_for_known_models() {
        use agent_core::models::{model_supports_adaptive_thinking, KNOWN_MODELS};
        for (id, _label) in KNOWN_MODELS {
            let expected = ReasoningSupport::AnthropicAdaptive {
                adaptive: model_supports_adaptive_thinking(id),
            };
            assert_eq!(
                anthropic_static_capability(id),
                Some(expected),
                "static table and wire-shape classifier disagree for {id}"
            );
        }
        // Near-miss ids: the substring classifier may infer a wire shape, but
        // the exact static table must stay fail-closed (None) — do NOT broaden
        // it to substring inference.
        for (id, classifier_infers_adaptive) in [
            ("claude-opus-4-7-preview", true),
            ("claude-fable-5-latest", true),
            ("claude-sonnet-4-6-legacy", false),
        ] {
            assert_eq!(
                anthropic_static_capability(id),
                None,
                "near-miss {id} must fail closed in the static table"
            );
            // Documented divergence: classifier substring-infers on near-miss.
            assert_eq!(
                agent_core::models::model_supports_adaptive_thinking(id),
                classifier_infers_adaptive,
                "classifier expectation drifted for {id}"
            );
        }
    }

    #[test]
    fn live_parse_maps_explicit_unsupported_thinking_to_none() {
        let body = r#"{
            "data": [
                {"id": "claude-test-thinker",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": true}}},
                {"id": "claude-test-fixed",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": false}}},
                {"id": "claude-test-nothink",
                 "capabilities": {"thinking": {"supported": false}}},
                {"id": "claude-test-nocaps"}
            ],
            "has_more": false
        }"#;
        let models = parse_anthropic_catalog_models(body).expect("parse");
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(
            by_id("claude-test-thinker").reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: true }
        );
        assert_eq!(
            by_id("claude-test-fixed").reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: false }
        );
        // Explicit evidence of no thinking support fails closed as None.
        assert_eq!(
            by_id("claude-test-nothink").reasoning,
            ReasoningSupport::None
        );
        // Absent capabilities stay Unknown (conservative, backward compatible).
        assert_eq!(
            by_id("claude-test-nocaps").reasoning,
            ReasoningSupport::Unknown
        );
    }

    #[test]
    fn live_parse_populates_capability_cache_with_qualified_ids() {
        let body = r#"{
            "data": [
                {"id": "claude-test-cache-entry",
                 "capabilities": {"thinking": {"supported": true},
                                   "effort": {"supported": true}}}
            ],
            "has_more": false
        }"#;
        parse_anthropic_catalog_models(body).expect("parse");
        let cached = super::super::capability_cache::get("anthropic/claude-test-cache-entry")
            .expect("live anthropic parse must populate the capability cache");
        assert_eq!(
            cached.reasoning,
            ReasoningSupport::AnthropicAdaptive { adaptive: true }
        );
    }
}
