use super::*;
use agent_core::reasoning::ReasoningLevel;
use serde::Deserialize;

/// Canonical provider key (matches OAuth storage / broker id).
pub const PROVIDER_KEY: &str = "openai-codex";
/// User-facing provider name.
pub const PROVIDER_NAME: &str = "OpenAI Codex";
/// Pinned ChatGPT backend host for Codex model catalog discovery.
pub const MODELS_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// Relative path allowlisted on the broker for catalog discovery.
pub const MODELS_PATH: &str = "/models";

// ─── Static capability table ─────────────────────────────────────────────────
//
// Per spec §Assumptions #1: the live catalog publishes exact
// `supported_reasoning_levels` per model. These static values are the
// safe fallback for offline / not-yet-configured sessions, and the
// authoritative capability table for the worktree implementation.
//
// Levels are ordered from least to most intensive (matching the spec table).

/// Static per-model capability for the known Codex catalog, keyed by model id.
/// Used when live catalog data is unavailable.
pub fn codex_static_capability(model_id: &str) -> Option<ReasoningSupport> {
    use ReasoningLevel::*;
    let (supported, default_level, multi_agent_version): (
        &[ReasoningLevel],
        ReasoningLevel,
        Option<CodexMultiAgentVersion>,
    ) = match model_id {
        "gpt-5.6-sol" => (
            &[Low, Medium, High, XHigh, Max, Ultra],
            Low,
            Some(CodexMultiAgentVersion::V2),
        ),
        "gpt-5.6-terra" => (
            &[Low, Medium, High, XHigh, Max, Ultra],
            Medium,
            Some(CodexMultiAgentVersion::V2),
        ),
        "gpt-5.6-luna" => (
            &[Low, Medium, High, XHigh, Max],
            Medium,
            Some(CodexMultiAgentVersion::V1),
        ),
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" => (&[Low, Medium, High, XHigh], Medium, None),
        "gpt-5.3-codex-spark" => (&[Low, Medium, High, XHigh], High, None),
        _ => return None,
    };
    Some(ReasoningSupport::CodexNamed {
        supported: supported.to_vec(),
        default_level: Some(default_level),
        multi_agent_version,
    })
}

/// Build the static catalog models with static capability data attached.
pub fn codex_static_catalog_models() -> Vec<CatalogModel> {
    [
        ("gpt-5.6-sol", "GPT-5.6-Sol"),
        ("gpt-5.6-terra", "GPT-5.6-Terra"),
        ("gpt-5.6-luna", "GPT-5.6-Luna"),
        ("gpt-5.5", "GPT-5.5"),
        ("gpt-5.4", "GPT-5.4"),
        ("gpt-5.4-mini", "GPT-5.4 Mini"),
        ("gpt-5.3-codex-spark", "GPT-5.3-Codex-Spark"),
    ]
    .into_iter()
    .filter_map(|(id, label)| {
        let mut m = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, id)?;
        m.provider_kind = CatalogProviderKind::OpenAiCodex;
        m.label = Some(label.to_string());
        m.reasoning = codex_static_capability(id).unwrap_or(ReasoningSupport::Unknown);
        m.source = CatalogSource::StaticFallback;
        Some(m)
    })
    .collect()
}

/// Build the broker-relative path for the ChatGPT backend models endpoint,
/// including the required `client_version` query (matches official Codex).
pub fn codex_models_path(client_version: &str) -> String {
    let version = if client_version.trim().is_empty() {
        env!("CARGO_PKG_VERSION")
    } else {
        client_version.trim()
    };
    format!("{MODELS_PATH}?client_version={version}")
}

/// Full pinned models URL (host + path + client_version query).
pub fn codex_models_url(client_version: &str) -> String {
    format!("{MODELS_BASE_URL}{}", codex_models_path(client_version))
}

// ─── Live catalog wire types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexModelItem>,
}

#[derive(Debug, Deserialize)]
struct CodexModelItem {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    context_window: Option<u64>,
    /// Live catalog reasoning metadata.
    #[serde(default)]
    supported_reasoning_levels: Option<Vec<CodexReasoningLevelItem>>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    multi_agent_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexReasoningLevelItem {
    effort: String,
}

// ─── Picker eligibility ───────────────────────────────────────────────────────

/// True when a Codex catalog row should appear as a selectable model.
///
/// Mirrors the official Codex picker rule: picker eligibility is determined by
/// list visibility. `supported_in_api` describes API-key support and therefore
/// must not filter the ChatGPT OAuth catalog.
pub fn codex_model_is_selectable(
    visibility: Option<&str>,
    _supported_in_api: Option<bool>,
) -> bool {
    match visibility.map(str::trim).filter(|v| !v.is_empty()) {
        None => true,
        Some("list") => true,
        Some(other) => {
            // Anything other than the known list-visible token is hidden.
            // Official Codex uses "list" for picker rows and "hide" for internal.
            other.eq_ignore_ascii_case("list")
        }
    }
}

// ─── Reasoning level parsing ──────────────────────────────────────────────────

/// Parse an ordered list of `CodexReasoningLevelItem` into `ReasoningLevel`
/// values, silently discarding unknown strings.
///
/// Unknown effort strings are ignored, never silently coerced to a known level.
fn parse_reasoning_levels(items: &[CodexReasoningLevelItem]) -> Vec<ReasoningLevel> {
    items
        .iter()
        .filter_map(|item| ReasoningLevel::parse(&item.effort))
        .collect()
}

/// Parse a `default_reasoning_level` string, returning `None` for unknown values.
fn parse_default_level(s: Option<&str>) -> Option<ReasoningLevel> {
    s.and_then(ReasoningLevel::parse)
}

fn parse_multi_agent_version(s: Option<&str>) -> Option<CodexMultiAgentVersion> {
    s.map(|version| match version {
        "v1" => CodexMultiAgentVersion::V1,
        "v2" => CodexMultiAgentVersion::V2,
        _ => CodexMultiAgentVersion::Unknown,
    })
}

/// Build a `ReasoningSupport` for a parsed Codex model item.
/// Falls back to static capability data when the live response omits the fields.
fn codex_reasoning_support(item: &CodexModelItem) -> ReasoningSupport {
    let default_level = parse_default_level(item.default_reasoning_level.as_deref());
    let multi_agent_version = parse_multi_agent_version(item.multi_agent_version.as_deref());

    if let Some(items) = item.supported_reasoning_levels.as_deref() {
        return ReasoningSupport::CodexNamed {
            supported: parse_reasoning_levels(items),
            default_level,
            multi_agent_version,
        };
    }
    // A live row may omit reasoning levels, so retain the existing static
    // level fallback. Never backfill the live row's collaboration version:
    // missing live V2 evidence must not authorize Ultra.
    match codex_static_capability(&item.slug) {
        Some(ReasoningSupport::CodexNamed {
            supported,
            default_level: static_default,
            ..
        }) => ReasoningSupport::CodexNamed {
            supported,
            default_level: default_level.or(static_default),
            multi_agent_version,
        },
        Some(other) => other,
        None => ReasoningSupport::Unknown,
    }
}

// ─── Parser ───────────────────────────────────────────────────────────────────

/// Parse the ChatGPT backend-api models response into normalized catalog rows.
///
/// Response shape: `{ "models": [ { "slug", "display_name", "visibility",
/// "supported_in_api", "context_window",
/// "supported_reasoning_levels": [{"effort": "..."}],
/// "default_reasoning_level": "...", ... }, ... ] }`.
pub fn parse_codex_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: CodexModelsResponse = serde_json::from_str(body)?;
    let models: Vec<CatalogModel> = resp
        .models
        .into_iter()
        .filter(|item| codex_model_is_selectable(item.visibility.as_deref(), item.supported_in_api))
        .filter_map(|item| {
            let reasoning = codex_reasoning_support(&item);
            let mut m = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, item.slug)?;
            m.provider_kind = CatalogProviderKind::OpenAiCodex;
            m.label = item.display_name.filter(|name| !name.trim().is_empty());
            m.context_tokens = item.context_window;
            m.reasoning = reasoning;
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect();
    Ok(models)
}

// ─── Typed request planning ───────────────────────────────────────────────────

/// Reasoning values accepted by the Codex Responses request wire.
/// Logical Ultra is deliberately absent and must be lowered by the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexWireEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl CodexWireEffort {
    pub fn as_str(self) -> &'static str {
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
pub enum CodexExecutionMode {
    Standard,
    Max,
    Ultra,
}

impl CodexExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexMultiAgentMode {
    ExplicitRequestOnly,
    Proactive,
}

impl CodexMultiAgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitRequestOnly => "explicit_request_only",
            Self::Proactive => "proactive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexRequestRole {
    #[default]
    Foreground,
    Worker,
    Internal,
}

impl CodexRequestRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Worker => "worker",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexCapabilitySource {
    Live,
    Static,
}

impl CodexCapabilitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Static => "static",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexPlanErrorCode {
    InvalidProviderIdentity,
    CapabilityMetadataMissing,
    UnsupportedReasoningLevel,
    UltraRequiresMultiAgentV2,
}

impl CodexPlanErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidProviderIdentity => "invalid_provider_identity",
            Self::CapabilityMetadataMissing => "capability_metadata_missing",
            Self::UnsupportedReasoningLevel => "unsupported_reasoning_level",
            Self::UltraRequiresMultiAgentV2 => "ultra_requires_multi_agent_v2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPlanError {
    code: CodexPlanErrorCode,
    message: String,
}

impl CodexPlanError {
    fn new(code: CodexPlanErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> CodexPlanErrorCode {
        self.code
    }
}

impl std::fmt::Display for CodexPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexPlanError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexExecutionPlan {
    pub qualified_model: String,
    pub selected_level: ReasoningLevel,
    pub mode: CodexExecutionMode,
    pub wire_effort: Option<CodexWireEffort>,
    pub multi_agent_version: Option<CodexMultiAgentVersion>,
    pub multi_agent_mode: Option<CodexMultiAgentMode>,
    pub request_role: CodexRequestRole,
    pub capability_source: Option<CodexCapabilitySource>,
}

impl CodexExecutionPlan {
    pub fn automatic_delegation(&self) -> bool {
        self.multi_agent_mode == Some(CodexMultiAgentMode::Proactive)
    }

    pub fn wire_effort_label(&self) -> &'static str {
        self.wire_effort.map_or("omitted", CodexWireEffort::as_str)
    }

    pub fn multi_agent_version_label(&self) -> &'static str {
        self.multi_agent_version
            .map_or("missing", CodexMultiAgentVersion::as_str)
    }

    pub fn multi_agent_mode_label(&self) -> &'static str {
        self.multi_agent_mode
            .map_or("disabled", CodexMultiAgentMode::as_str)
    }
}

#[derive(Debug, Clone)]
struct CodexCapabilitySnapshot {
    supported: Vec<ReasoningLevel>,
    multi_agent_version: Option<CodexMultiAgentVersion>,
    source: CodexCapabilitySource,
}

fn snapshot_from_model(
    model: &CatalogModel,
    source: CodexCapabilitySource,
) -> Option<CodexCapabilitySnapshot> {
    match &model.reasoning {
        ReasoningSupport::CodexNamed {
            supported,
            multi_agent_version,
            ..
        } => Some(CodexCapabilitySnapshot {
            supported: supported.clone(),
            multi_agent_version: *multi_agent_version,
            source,
        }),
        _ => None,
    }
}

fn authoritative_source(
    model: &CatalogModel,
    qualified_model: &str,
) -> Result<CodexCapabilitySource, CodexPlanError> {
    match model.source {
        CatalogSource::Live => Ok(CodexCapabilitySource::Live),
        CatalogSource::StaticFallback => Ok(CodexCapabilitySource::Static),
        CatalogSource::StaticWithLive | CatalogSource::Inferred => Err(CodexPlanError::new(
            CodexPlanErrorCode::CapabilityMetadataMissing,
            format!(
                "authoritative exact-model capability metadata is unavailable for {qualified_model}"
            ),
        )),
    }
}

fn authoritative_capability(
    qualified_model: &str,
    model_id: &str,
    catalog_model: Option<&CatalogModel>,
) -> Result<Option<CodexCapabilitySnapshot>, CodexPlanError> {
    if let Some(model) = catalog_model {
        if model.runtime_id() != qualified_model
            || model.provider_kind != CatalogProviderKind::OpenAiCodex
        {
            return Err(CodexPlanError::new(
                CodexPlanErrorCode::InvalidProviderIdentity,
                format!("catalog identity does not match exact Codex model {qualified_model}"),
            ));
        }
        let source = authoritative_source(model, qualified_model)?;
        return Ok(snapshot_from_model(model, source));
    }

    // A present live row is authoritative even when it lacks usable Codex
    // metadata. Never mix that absence with a static V2 fallback.
    if let Some(model) = super::capability_cache::get(qualified_model) {
        if model.runtime_id() != qualified_model
            || model.provider_kind != CatalogProviderKind::OpenAiCodex
        {
            return Err(CodexPlanError::new(
                CodexPlanErrorCode::InvalidProviderIdentity,
                format!(
                    "cached catalog identity does not match exact Codex model {qualified_model}"
                ),
            ));
        }
        let source = authoritative_source(&model, qualified_model)?;
        return Ok(snapshot_from_model(&model, source));
    }

    Ok(match codex_static_capability(model_id) {
        Some(ReasoningSupport::CodexNamed {
            supported,
            multi_agent_version,
            ..
        }) => Some(CodexCapabilitySnapshot {
            supported,
            multi_agent_version,
            source: CodexCapabilitySource::Static,
        }),
        _ => None,
    })
}

/// Derive the exact Codex request contract from provider-qualified identity,
/// logical selection, authoritative capability metadata, and runtime role.
pub fn plan_codex_execution(
    qualified_model: &str,
    selected_level: ReasoningLevel,
    request_role: CodexRequestRole,
    catalog_model: Option<&CatalogModel>,
) -> Result<CodexExecutionPlan, CodexPlanError> {
    let Some(model_id) = qualified_model.strip_prefix("openai-codex/") else {
        return Err(CodexPlanError::new(
            CodexPlanErrorCode::InvalidProviderIdentity,
            format!("{qualified_model} is not an exact openai-codex identity"),
        ));
    };
    if model_id.is_empty() || model_id.contains('/') {
        return Err(CodexPlanError::new(
            CodexPlanErrorCode::InvalidProviderIdentity,
            format!("{qualified_model} is not an exact openai-codex identity"),
        ));
    }

    let capability = authoritative_capability(qualified_model, model_id, catalog_model)?;
    if !matches!(
        selected_level,
        ReasoningLevel::Off | ReasoningLevel::Adaptive
    ) {
        let Some(capability) = capability.as_ref() else {
            return Err(CodexPlanError::new(
                CodexPlanErrorCode::CapabilityMetadataMissing,
                format!(
                    "no capability metadata for {qualified_model}; cannot authorize level '{selected_level}'"
                ),
            ));
        };
        if !capability.supported.contains(&selected_level) {
            return Err(CodexPlanError::new(
                CodexPlanErrorCode::UnsupportedReasoningLevel,
                format!(
                    "reasoning level '{selected_level}' is not supported by {qualified_model}; supported: [{}]",
                    capability
                        .supported
                        .iter()
                        .map(|level| level.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    }

    let multi_agent_version = capability
        .as_ref()
        .and_then(|capability| capability.multi_agent_version);
    if selected_level == ReasoningLevel::Ultra
        && multi_agent_version != Some(CodexMultiAgentVersion::V2)
    {
        return Err(CodexPlanError::new(
            CodexPlanErrorCode::UltraRequiresMultiAgentV2,
            format!(
                "reasoning level 'ultra' requires multi-agent v2 capability for {qualified_model}"
            ),
        ));
    }

    let mode = match selected_level {
        ReasoningLevel::Max => CodexExecutionMode::Max,
        ReasoningLevel::Ultra => CodexExecutionMode::Ultra,
        _ => CodexExecutionMode::Standard,
    };
    let wire_effort = match selected_level {
        ReasoningLevel::Off | ReasoningLevel::Adaptive => None,
        ReasoningLevel::Low => Some(CodexWireEffort::Low),
        ReasoningLevel::Medium => Some(CodexWireEffort::Medium),
        ReasoningLevel::High => Some(CodexWireEffort::High),
        ReasoningLevel::XHigh => Some(CodexWireEffort::XHigh),
        ReasoningLevel::Max | ReasoningLevel::Ultra => Some(CodexWireEffort::Max),
        // Anthropic-only logical mode; Codex validation rejects it before lowering.
        ReasoningLevel::UltraCode => None,
    };
    let multi_agent_mode = if request_role == CodexRequestRole::Foreground
        && multi_agent_version == Some(CodexMultiAgentVersion::V2)
    {
        Some(if selected_level == ReasoningLevel::Ultra {
            CodexMultiAgentMode::Proactive
        } else {
            CodexMultiAgentMode::ExplicitRequestOnly
        })
    } else {
        None
    };

    Ok(CodexExecutionPlan {
        qualified_model: qualified_model.to_string(),
        selected_level,
        mode,
        wire_effort,
        multi_agent_version,
        multi_agent_mode,
        request_role,
        capability_source: capability.map(|capability| capability.source),
    })
}

// ─── Capability validation helper ─────────────────────────────────────────────

/// Validate that the given `level` is supported by the Codex model identified
/// by `model_id`. Uses the provided catalog when available, else falls back to
/// the static table.
///
/// Returns `Ok(())` if supported, `Err(message)` if not.
/// Always fails for providers without authoritative Codex metadata.
pub fn validate_codex_level(
    model_id: &str,
    level: ReasoningLevel,
    catalog_model: Option<&CatalogModel>,
) -> Result<(), String> {
    let qualified = format!("openai-codex/{model_id}");
    plan_codex_execution(
        &qualified,
        level,
        CodexRequestRole::Foreground,
        catalog_model,
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/openai_codex_models.json");

    #[test]
    fn parse_fixture_matches_list_visible_chatgpt_picker_models() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex-spark",
            ]
        );
        // `supported_in_api` describes API-key availability, not ChatGPT OAuth
        // picker eligibility. Spark is list-visible despite this being false.
        assert!(ids.contains(&"gpt-5.3-codex-spark"));
        // Non-list-visible backend rows are not user-selectable.
        assert!(!ids.contains(&"codex-auto-review"));
        assert!(!ids.contains(&"codex-internal-eval"));
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("openai-codex/")));
        assert!(models.iter().all(|m| m.source == CatalogSource::Live));
        assert!(models
            .iter()
            .all(|m| m.provider_kind == CatalogProviderKind::OpenAiCodex));
    }

    #[test]
    fn parse_fixture_reads_display_name_and_context_window() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let gpt55 = models.iter().find(|m| m.id == "gpt-5.5").unwrap();
        assert_eq!(gpt55.label.as_deref(), Some("GPT-5.5"));
        assert_eq!(gpt55.context_tokens, Some(272_000));
    }

    #[test]
    fn execution_plan_fixture_preserves_multi_agent_versions() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let version = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .and_then(CatalogModel::codex_multi_agent_version)
        };
        assert_eq!(version("gpt-5.6-sol"), Some(CodexMultiAgentVersion::V2));
        assert_eq!(version("gpt-5.6-terra"), Some(CodexMultiAgentVersion::V2));
        assert_eq!(version("gpt-5.6-luna"), Some(CodexMultiAgentVersion::V1));
        assert_eq!(version("gpt-5.5"), None);
    }

    #[test]
    fn execution_plan_maps_max_ultra_and_xhigh_exactly() {
        let max = plan_codex_execution(
            "openai-codex/gpt-5.6-sol",
            ReasoningLevel::Max,
            CodexRequestRole::Foreground,
            None,
        )
        .expect("Sol Max plan");
        assert_eq!(max.mode, CodexExecutionMode::Max);
        assert_eq!(max.wire_effort, Some(CodexWireEffort::Max));
        assert_eq!(
            max.multi_agent_mode,
            Some(CodexMultiAgentMode::ExplicitRequestOnly)
        );

        let ultra = plan_codex_execution(
            "openai-codex/gpt-5.6-sol",
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            None,
        )
        .expect("Sol Ultra plan");
        assert_eq!(ultra.mode, CodexExecutionMode::Ultra);
        assert_eq!(ultra.wire_effort, Some(CodexWireEffort::Max));
        assert_eq!(ultra.multi_agent_mode, Some(CodexMultiAgentMode::Proactive));

        let xhigh = plan_codex_execution(
            "openai-codex/gpt-5.6-sol",
            ReasoningLevel::XHigh,
            CodexRequestRole::Foreground,
            None,
        )
        .expect("Sol XHigh plan");
        assert_eq!(xhigh.mode, CodexExecutionMode::Standard);
        assert_eq!(xhigh.wire_effort, Some(CodexWireEffort::XHigh));
        assert_eq!(
            xhigh.multi_agent_mode,
            Some(CodexMultiAgentMode::ExplicitRequestOnly)
        );
    }

    #[test]
    fn execution_plan_luna_max_uses_max_without_v2_context() {
        let plan = plan_codex_execution(
            "openai-codex/gpt-5.6-luna",
            ReasoningLevel::Max,
            CodexRequestRole::Foreground,
            None,
        )
        .expect("Luna Max plan");
        assert_eq!(plan.wire_effort, Some(CodexWireEffort::Max));
        assert_eq!(plan.multi_agent_version, Some(CodexMultiAgentVersion::V1));
        assert_eq!(plan.multi_agent_mode, None);
    }

    #[test]
    fn execution_plan_worker_ultra_never_enables_proactive_mode() {
        let plan = plan_codex_execution(
            "openai-codex/gpt-5.6-sol",
            ReasoningLevel::Ultra,
            CodexRequestRole::Worker,
            None,
        )
        .expect("worker Ultra plan");
        assert_eq!(plan.wire_effort, Some(CodexWireEffort::Max));
        assert_eq!(plan.multi_agent_mode, None);
        assert!(!plan.automatic_delegation());
    }

    #[test]
    fn execution_plan_ultra_requires_live_v2_without_static_backfill() {
        for (slug, version_field) in [
            ("gpt-ultra-live-missing", ""),
            ("gpt-ultra-live-v1", r#", "multi_agent_version": "v1""#),
            ("gpt-ultra-live-unknown", r#", "multi_agent_version": "v3""#),
        ] {
            let body = format!(
                r#"{{"models":[{{
                    "slug":"{slug}",
                    "visibility":"list",
                    "supported_reasoning_levels":[{{"effort":"max"}},{{"effort":"ultra"}}]
                    {version_field}
                }}]}}"#
            );
            let models = parse_codex_catalog_models(&body).expect("parse live row");
            let error = plan_codex_execution(
                &format!("openai-codex/{slug}"),
                ReasoningLevel::Ultra,
                CodexRequestRole::Foreground,
                Some(&models[0]),
            )
            .expect_err("live missing/v1/unknown version must deny Ultra");
            assert_eq!(error.code(), CodexPlanErrorCode::UltraRequiresMultiAgentV2);
        }
    }

    #[test]
    fn execution_plan_rejects_wrong_provider_identity() {
        let error = plan_codex_execution(
            "openrouter/gpt-5.6-sol",
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            None,
        )
        .expect_err("wrong provider must fail closed");
        assert_eq!(error.code(), CodexPlanErrorCode::InvalidProviderIdentity);
    }

    #[test]
    fn execution_plan_rejects_wrong_provider_kind_in_cache() {
        let id = "gpt-cached-provider-kind-spoof";
        let qualified = format!("openai-codex/{id}");
        let mut spoofed = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, id).unwrap();
        spoofed.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Max, ReasoningLevel::Ultra],
            default_level: Some(ReasoningLevel::Max),
            multi_agent_version: Some(CodexMultiAgentVersion::V2),
        };
        super::super::capability_cache::insert(spoofed);

        let error = plan_codex_execution(
            &qualified,
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            None,
        )
        .expect_err("cache rows must preserve the exact Codex provider kind");
        assert_eq!(error.code(), CodexPlanErrorCode::InvalidProviderIdentity);
    }

    #[test]
    fn execution_plan_rejects_inferred_capability_provenance() {
        fn inferred_model(id: &str) -> CatalogModel {
            let mut model = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, id).unwrap();
            model.provider_kind = CatalogProviderKind::OpenAiCodex;
            model.source = CatalogSource::Inferred;
            model.reasoning = ReasoningSupport::CodexNamed {
                supported: vec![ReasoningLevel::Max, ReasoningLevel::Ultra],
                default_level: Some(ReasoningLevel::Max),
                multi_agent_version: Some(CodexMultiAgentVersion::V2),
            };
            model
        }

        let explicit_id = "gpt-inferred-explicit-provenance";
        let explicit = inferred_model(explicit_id);
        let explicit_error = plan_codex_execution(
            &format!("openai-codex/{explicit_id}"),
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            Some(&explicit),
        )
        .expect_err("inferred explicit metadata must never authorize Ultra");
        assert_eq!(
            explicit_error.code(),
            CodexPlanErrorCode::CapabilityMetadataMissing
        );

        let cached_id = "gpt-inferred-cached-provenance";
        super::super::capability_cache::insert(inferred_model(cached_id));
        let cached_error = plan_codex_execution(
            &format!("openai-codex/{cached_id}"),
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            None,
        )
        .expect_err("inferred cached metadata must never authorize Ultra");
        assert_eq!(
            cached_error.code(),
            CodexPlanErrorCode::CapabilityMetadataMissing
        );
    }

    // ── Reasoning level parsing from fixture ──────────────────────────────────

    #[test]
    fn parse_fixture_sol_has_ultra() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let sol = models.iter().find(|m| m.id == "gpt-5.6-sol").unwrap();
        assert!(
            sol.codex_supports_level(ReasoningLevel::Ultra),
            "sol must support ultra"
        );
        assert!(sol.codex_supports_level(ReasoningLevel::Max));
        assert!(sol.codex_supports_level(ReasoningLevel::XHigh));
    }

    #[test]
    fn parse_fixture_terra_has_ultra() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let terra = models.iter().find(|m| m.id == "gpt-5.6-terra").unwrap();
        assert!(terra.codex_supports_level(ReasoningLevel::Ultra));
        assert!(terra.codex_supports_level(ReasoningLevel::Max));
    }

    #[test]
    fn parse_fixture_luna_has_max_not_ultra() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let luna = models.iter().find(|m| m.id == "gpt-5.6-luna").unwrap();
        assert!(luna.codex_supports_level(ReasoningLevel::Max));
        assert!(
            !luna.codex_supports_level(ReasoningLevel::Ultra),
            "luna must NOT support ultra"
        );
    }

    #[test]
    fn parse_fixture_gpt55_has_xhigh_not_max_or_ultra() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let m = models.iter().find(|m| m.id == "gpt-5.5").unwrap();
        assert!(m.codex_supports_level(ReasoningLevel::XHigh));
        assert!(
            !m.codex_supports_level(ReasoningLevel::Max),
            "gpt-5.5 must NOT support max"
        );
        assert!(
            !m.codex_supports_level(ReasoningLevel::Ultra),
            "gpt-5.5 must NOT support ultra"
        );
    }

    #[test]
    fn parse_fixture_spark_has_xhigh_not_max_or_ultra() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let spark = models
            .iter()
            .find(|m| m.id == "gpt-5.3-codex-spark")
            .unwrap();
        assert!(spark.codex_supports_level(ReasoningLevel::XHigh));
        assert!(!spark.codex_supports_level(ReasoningLevel::Max));
        assert!(!spark.codex_supports_level(ReasoningLevel::Ultra));
    }

    #[test]
    fn parse_fixture_default_levels_match_observed_cache() {
        let models = parse_codex_catalog_models(FIXTURE).expect("parse fixture");
        let get = |id: &str| {
            models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("missing {id}"))
        };
        let default_of = |id: &str| match &get(id).reasoning {
            ReasoningSupport::CodexNamed { default_level, .. } => *default_level,
            other => panic!("{id}: expected CodexNamed, got {other:?}"),
        };
        assert_eq!(default_of("gpt-5.6-sol"), Some(ReasoningLevel::Low), "sol");
        assert_eq!(
            default_of("gpt-5.6-terra"),
            Some(ReasoningLevel::Medium),
            "terra"
        );
        assert_eq!(
            default_of("gpt-5.6-luna"),
            Some(ReasoningLevel::Medium),
            "luna"
        );
        assert_eq!(default_of("gpt-5.5"), Some(ReasoningLevel::Medium), "5.5");
        assert_eq!(default_of("gpt-5.4"), Some(ReasoningLevel::Medium), "5.4");
        assert_eq!(
            default_of("gpt-5.4-mini"),
            Some(ReasoningLevel::Medium),
            "5.4-mini"
        );
        assert_eq!(
            default_of("gpt-5.3-codex-spark"),
            Some(ReasoningLevel::High),
            "spark"
        );
    }

    #[test]
    fn unknown_effort_strings_are_ignored_not_coerced() {
        let body = r#"{"models":[{
            "slug": "gpt-test",
            "visibility": "list",
            "supported_reasoning_levels": [
                {"effort": "low"},
                {"effort": "hyper-ultra-experimental"},
                {"effort": "high"}
            ]
        }]}"#;
        let models = parse_codex_catalog_models(body).unwrap();
        let m = &models[0];
        // "hyper-ultra-experimental" is silently dropped; only low and high survive.
        assert!(m.codex_supports_level(ReasoningLevel::Low));
        assert!(m.codex_supports_level(ReasoningLevel::High));
        assert!(!m.codex_supports_level(ReasoningLevel::Ultra));
        assert!(!m.codex_supports_level(ReasoningLevel::Max));
    }

    #[test]
    fn live_known_model_with_only_unknown_efforts_does_not_borrow_static_ultra() {
        let item: CodexModelItem = serde_json::from_str(
            r#"{
            "slug": "gpt-5.6-sol",
            "visibility": "list",
            "supported_reasoning_levels": [{"effort": "future-effort"}],
            "multi_agent_version": "v2"
        }"#,
        )
        .expect("parse live Sol row");
        let mut model = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, &item.slug).unwrap();
        model.provider_kind = CatalogProviderKind::OpenAiCodex;
        model.source = CatalogSource::Live;
        model.reasoning = codex_reasoning_support(&item);

        let error = plan_codex_execution(
            "openai-codex/gpt-5.6-sol",
            ReasoningLevel::Ultra,
            CodexRequestRole::Foreground,
            Some(&model),
        )
        .expect_err("an explicit unknown live effort list must fail closed");
        assert_eq!(error.code(), CodexPlanErrorCode::UnsupportedReasoningLevel);
    }

    // ── Static capability table ───────────────────────────────────────────────

    #[test]
    fn static_sol_has_ultra_and_default_low() {
        let cap = codex_static_capability("gpt-5.6-sol").expect("sol");
        match cap {
            ReasoningSupport::CodexNamed {
                supported,
                default_level,
                ..
            } => {
                assert!(
                    supported.contains(&ReasoningLevel::Ultra),
                    "sol needs ultra"
                );
                assert!(supported.contains(&ReasoningLevel::Max), "sol needs max");
                assert_eq!(
                    default_level,
                    Some(ReasoningLevel::Low),
                    "sol default is Low"
                );
            }
            _ => panic!("expected CodexNamed"),
        }
    }

    #[test]
    fn static_terra_has_ultra_and_default_medium() {
        let cap = codex_static_capability("gpt-5.6-terra").expect("terra");
        match cap {
            ReasoningSupport::CodexNamed {
                supported,
                default_level,
                ..
            } => {
                assert!(
                    supported.contains(&ReasoningLevel::Ultra),
                    "terra needs ultra"
                );
                assert!(supported.contains(&ReasoningLevel::Max), "terra needs max");
                assert_eq!(
                    default_level,
                    Some(ReasoningLevel::Medium),
                    "terra default is Medium"
                );
            }
            _ => panic!("expected CodexNamed"),
        }
    }

    #[test]
    fn static_luna_has_max_not_ultra_and_default_medium() {
        let cap = codex_static_capability("gpt-5.6-luna").unwrap();
        match cap {
            ReasoningSupport::CodexNamed {
                supported,
                default_level,
                ..
            } => {
                assert!(supported.contains(&ReasoningLevel::Max));
                assert!(!supported.contains(&ReasoningLevel::Ultra));
                assert_eq!(
                    default_level,
                    Some(ReasoningLevel::Medium),
                    "luna default is Medium"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn static_gpt55_family_has_xhigh_not_max_ultra_default_medium() {
        for id in ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"] {
            let cap = codex_static_capability(id).expect(id);
            match cap {
                ReasoningSupport::CodexNamed {
                    supported,
                    default_level,
                    ..
                } => {
                    assert!(
                        supported.contains(&ReasoningLevel::XHigh),
                        "{id} needs xhigh"
                    );
                    assert!(
                        !supported.contains(&ReasoningLevel::Max),
                        "{id} must NOT have max"
                    );
                    assert!(
                        !supported.contains(&ReasoningLevel::Ultra),
                        "{id} must NOT have ultra"
                    );
                    assert_eq!(
                        default_level,
                        Some(ReasoningLevel::Medium),
                        "{id} default is Medium"
                    );
                }
                _ => panic!(),
            }
        }
    }

    #[test]
    fn static_spark_default_high() {
        let cap = codex_static_capability("gpt-5.3-codex-spark").unwrap();
        match cap {
            ReasoningSupport::CodexNamed {
                supported,
                default_level,
                ..
            } => {
                assert!(supported.contains(&ReasoningLevel::XHigh));
                assert!(!supported.contains(&ReasoningLevel::Max));
                assert!(!supported.contains(&ReasoningLevel::Ultra));
                assert_eq!(
                    default_level,
                    Some(ReasoningLevel::High),
                    "spark default is High"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn static_unknown_model_returns_none() {
        assert!(codex_static_capability("gpt-future-unknown").is_none());
        // Internal/hidden models are also not in the table.
        assert!(codex_static_capability("codex-auto-review").is_none());
    }

    // ── validate_codex_level ──────────────────────────────────────────────────

    #[test]
    fn validate_sol_ultra_ok() {
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Ultra, None).is_ok());
    }

    #[test]
    fn validate_sol_client_omission_modes_ok() {
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Off, None).is_ok());
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Adaptive, None).is_ok());
    }

    #[test]
    fn validate_luna_ultra_rejected() {
        let err = validate_codex_level("gpt-5.6-luna", ReasoningLevel::Ultra, None).unwrap_err();
        assert!(err.contains("ultra"), "error must name the rejected level");
        assert!(err.contains("gpt-5.6-luna"));
    }

    #[test]
    fn validate_gpt55_max_rejected() {
        assert!(validate_codex_level("gpt-5.5", ReasoningLevel::Max, None).is_err());
        assert!(validate_codex_level("gpt-5.5", ReasoningLevel::Ultra, None).is_err());
    }

    #[test]
    fn validate_unknown_model_rejected() {
        let err = validate_codex_level("gpt-future-x", ReasoningLevel::Max, None).unwrap_err();
        assert!(err.contains("no capability metadata"));
    }

    #[test]
    fn validate_live_catalog_overrides_static() {
        // Build a live catalog model that supports ONLY low+medium (fewer than static).
        let mut live = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, "gpt-5.6-sol").unwrap();
        live.provider_kind = CatalogProviderKind::OpenAiCodex;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
            multi_agent_version: None,
        };
        // Ultra is in the static table but the live model says otherwise.
        let err =
            validate_codex_level("gpt-5.6-sol", ReasoningLevel::Ultra, Some(&live)).unwrap_err();
        assert!(err.contains("ultra"));
        // Low is accepted.
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Low, Some(&live)).is_ok());
    }

    #[test]
    fn validate_live_ultra_requires_v2_even_when_listed() {
        let mut live = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, "gpt-ultra-no-v2").unwrap();
        live.provider_kind = CatalogProviderKind::OpenAiCodex;
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Max, ReasoningLevel::Ultra],
            default_level: Some(ReasoningLevel::Max),
            multi_agent_version: None,
        };

        assert!(validate_codex_level("gpt-ultra-no-v2", ReasoningLevel::Max, Some(&live)).is_ok());
        let err = validate_codex_level("gpt-ultra-no-v2", ReasoningLevel::Ultra, Some(&live))
            .expect_err("Ultra must require exact v2 capability");
        assert!(err.contains("multi-agent v2"), "{err}");
    }

    /// Verify that `validate_codex_level(..., None)` consults the process-local
    /// capability cache (gap 1): a narrower live entry must override the static
    /// table even when no explicit catalog_model argument is supplied.
    #[test]
    fn validate_cache_narrows_sol_rejects_ultra_without_catalog_arg() {
        // Use a unique model slug to avoid cross-test pollution from the shared cache.
        let unique_id = "gpt-5.6-sol-cache-test-ultra";
        let qualified = format!("openai-codex/{unique_id}");

        // Insert a live cache entry that supports only Low+Medium (no Ultra).
        let mut live = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, unique_id).unwrap();
        live.provider_kind = CatalogProviderKind::OpenAiCodex;
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
            multi_agent_version: None,
        };
        super::super::capability_cache::insert(live);

        // Confirm it's in the cache.
        let cached = super::super::capability_cache::get(&qualified)
            .expect("model must be in cache after insert");
        assert!(matches!(
            cached.reasoning,
            ReasoningSupport::CodexNamed { .. }
        ));

        // validate_codex_level with catalog_model=None must use the cache and
        // reject Ultra (which the static sol table would have allowed).
        let err = validate_codex_level(unique_id, ReasoningLevel::Ultra, None)
            .expect_err("cache should narrow Ultra rejection");
        assert!(
            err.contains("ultra"),
            "error must name the rejected level; got: {err}"
        );

        // Low is still accepted via the cache.
        assert!(
            validate_codex_level(unique_id, ReasoningLevel::Low, None).is_ok(),
            "Low must still pass via cache"
        );
    }

    // ── Existing catalog tests ────────────────────────────────────────────────

    #[test]
    fn models_path_includes_client_version_query() {
        assert_eq!(codex_models_path("0.6.0"), "/models?client_version=0.6.0");
        assert_eq!(
            codex_models_url("0.6.0"),
            "https://chatgpt.com/backend-api/models?client_version=0.6.0"
        );
    }

    #[test]
    fn selectable_rules_match_codex_picker() {
        assert!(codex_model_is_selectable(Some("list"), Some(true)));
        assert!(codex_model_is_selectable(Some("list"), Some(false)));
        assert!(codex_model_is_selectable(None, Some(true)));
        assert!(codex_model_is_selectable(Some("list"), None));
        assert!(!codex_model_is_selectable(Some("hide"), Some(true)));
        assert!(!codex_model_is_selectable(Some("internal"), Some(false)));
        assert!(!codex_model_is_selectable(Some("hidden"), Some(true)));
    }

    #[test]
    fn static_catalog_is_current_safe_chatgpt_oauth_set() {
        let models = codex_static_catalog_models();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex-spark",
            ]
        );
        for api_only in ["gpt-5.6-sol-wm", "gpt-5.6-pro", "gpt-5.5-pro"] {
            assert!(!ids.contains(&api_only));
        }
        assert!(models
            .iter()
            .all(|m| m.source == CatalogSource::StaticFallback));
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("openai-codex/")));
    }

    #[test]
    fn static_catalog_has_codex_named_reasoning() {
        let models = codex_static_catalog_models();
        for m in &models {
            assert!(
                matches!(m.reasoning, ReasoningSupport::CodexNamed { .. }),
                "static model {} must have CodexNamed reasoning",
                m.id
            );
        }
    }

    #[test]
    fn invalid_json_returns_error() {
        assert!(parse_codex_catalog_models("{not json}").is_err());
    }

    #[test]
    fn missing_models_key_returns_error() {
        assert!(parse_codex_catalog_models(r#"{"data":[]}"#).is_err());
    }
}
