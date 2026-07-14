use super::*;
use agent_core::reasoning::ReasoningLevel;
use serde::Deserialize;

pub const ANTHROPIC_ULTRACODE_WORKFLOW: &str = r#"<anthropic-ultracode-workflow>
Use subagents as a bounded, model-directed workflow when independent work will help. Do not create an eager fixed pool. Start only justified work with subagent_start; monitor with subagent_status; redirect with subagent_steer; gather completed results with subagent_collect; and use subagent_resume only when further work is necessary. Keep delegation finite, preserve the foreground cancellation boundary, collect all required results, and finish only after no required work remains.
</anthropic-ultracode-workflow>"#;

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
    pub builtin_lifecycle_tools: bool,
}

impl AnthropicPlanPrerequisites {
    pub const fn installed() -> Self {
        Self {
            orchestration_policy: true,
            builtin_lifecycle_tools: true,
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

/// Look up exact qualified identities only. No family or substring inference.
pub fn anthropic_mode_capabilities(qualified_model: &str) -> Option<AnthropicModeCapabilities> {
    match qualified_model {
        // Evidence: Claude Code 2.1.207 binary SHA-256
        // 85e7e988a392d859f90802ca21fb26e89d3c9ab527f5ed0b08df3955e34d5c83
        // and its matching settings schema advertise Fable 5 max_effort and
        // xhigh_effort; the live picker displays Max and UltraCode.
        "anthropic/claude-fable-5" => Some(AnthropicModeCapabilities {
            max_supported: true,
            xhigh_supported: true,
            workflow_supported: true,
        }),
        _ => None,
    }
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
        if !prerequisites.builtin_lifecycle_tools {
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
                    builtin_lifecycle_tools: true,
                },
                AnthropicPlanErrorCode::UltraCodeRequiresOrchestration,
            ),
            (
                "anthropic/claude-fable-5",
                ReasoningLevel::UltraCode,
                ExecutionRole::Foreground,
                AnthropicPlanPrerequisites {
                    orchestration_policy: true,
                    builtin_lifecycle_tools: false,
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
