//! GitHub Copilot model catalog (C2 catalog + descriptor-driven routing).
//!
//! Live discovery hits the community-observed authenticated endpoint
//! `GET https://api.githubcopilot.com/models` (experimental — not a documented
//! stable third-party public product API). Prefer broker-proxied, account-
//! specific discovery. Curated static fallback IDs are restricted to wire IDs
//! established by fixtures/live discovery — never guessed display names.
//!
//! Runtime endpoint selection is *evidence-driven*: each fixture row exposes
//! `supported_endpoints`, and the picker/router consult that list rather than
//! guessing by model family. Currently reviewed broker paths are `/responses`
//! and `/chat/completions`; `/v1/messages` is advertised by Anthropic-vendor
//! rows but is not yet routed by this broker, so we fall back to
//! `/chat/completions` where it is also advertised.
//!
//! See `docs/github-copilot-model-catalog-spec.md`.

use super::{
    CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary, ReasoningSupport,
};
use serde::Deserialize;

/// Canonical provider key (matches OAuth storage / broker id).
pub const PROVIDER_KEY: &str = "github-copilot";
/// User-facing provider name.
pub const PROVIDER_NAME: &str = "GitHub Copilot";

/// Pinned experimental models host for personal Copilot catalog discovery.
/// Community-observed; not a GitHub-documented stable third-party API.
pub const MODELS_BASE_URL: &str = "https://api.githubcopilot.com";
/// Relative path allowlisted on the broker for this slice (catalog only).
pub const MODELS_PATH: &str = "/models";
/// Full pinned models URL (host + path, no query).
pub const MODELS_URL: &str = "https://api.githubcopilot.com/models";

/// Live-verified API version header for `api.githubcopilot.com` catalog GETs.
pub const COPILOT_API_VERSION: &str = "2025-10-01";

/// Maximum response body accepted from the models endpoint (256 KiB).
pub const MAX_MODELS_BODY_BYTES: usize = 256 * 1024;

// ── Endpoint & policy shape ──────────────────────────────────────────────────

/// Wire endpoint literals advertised on Copilot catalog rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotEndpoint {
    /// OpenAI Responses (`/responses`).
    Responses,
    /// OpenAI Chat Completions (`/chat/completions`).
    ChatCompletions,
    /// Anthropic Messages (`/v1/messages`) — not currently broker-reviewed.
    V1Messages,
    /// WebSocket variant of `/responses` — out of scope for this broker.
    WsResponses,
    /// Anything else the upstream may add in the future.
    Other(String),
}

impl CopilotEndpoint {
    pub fn parse(s: &str) -> Self {
        match s {
            "/responses" => Self::Responses,
            "/chat/completions" => Self::ChatCompletions,
            "/v1/messages" => Self::V1Messages,
            "ws:/responses" => Self::WsResponses,
            other => Self::Other(other.to_string()),
        }
    }

    /// Reviewed & broker-supported for outbound inference.
    pub fn is_reviewed(&self) -> bool {
        matches!(self, Self::Responses | Self::ChatCompletions)
    }
}

/// Copilot per-model policy `state` — reflects account entitlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotPolicyState {
    /// Row is opted-in for this account.
    Enabled,
    /// Row is admin-blocked / opt-in required — never selectable.
    Disabled,
    /// No `policy` object on the row (e.g. defaults). Not treated as disabled.
    Unspecified,
    /// Forward-compat: unknown state literal.
    Other(String),
}

impl CopilotPolicyState {
    fn from_opt(s: Option<&str>) -> Self {
        match s {
            None => Self::Unspecified,
            Some("enabled") => Self::Enabled,
            Some("disabled") => Self::Disabled,
            Some(other) => Self::Other(other.to_string()),
        }
    }
}

// ── Typed descriptor for curated fallback ────────────────────────────────────

/// Statically-selected wire endpoint for a curated fallback descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotWire {
    Responses,
    ChatCompletions,
}

impl CopilotWire {
    pub fn to_wire_protocol(self) -> crate::runtime::openai::WireProtocol {
        match self {
            Self::Responses => crate::runtime::openai::WireProtocol::OpenAiResponses,
            Self::ChatCompletions => crate::runtime::openai::WireProtocol::OpenAiChatCompletions,
        }
    }
}

/// Typed static descriptor for curated fallback models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopilotModelDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    /// Upstream `vendor` string on the fixture row (informational).
    pub vendor: &'static str,
    /// Broker wire selected from the row's `supported_endpoints`.
    pub selected_wire: CopilotWire,
}

/// Curated fallback — every entry is proven by [`selectable_copilot_entries`]
/// on the reviewed fixture: `type == chat`, `model_picker_enabled == true`,
/// and at least one reviewed endpoint. Policy/picker flags are opt-in metadata. Ordered
/// for UI presentation.
pub const COPILOT_FALLBACK_MODELS: &[CopilotModelDescriptor] = &[
    CopilotModelDescriptor {
        id: "gpt-5.3-codex",
        label: "GPT-5.3-Codex",
        vendor: "OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor {
        id: "gpt-5.4",
        label: "GPT-5.4",
        vendor: "OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor {
        id: "gpt-5.4-mini",
        label: "GPT-5.4 mini",
        vendor: "OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor {
        id: "gpt-5.6-luna",
        label: "GPT-5.6 Luna",
        vendor: "OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor {
        id: "gpt-5.6-terra",
        label: "GPT-5.6 Terra",
        vendor: "OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor {
        id: "gpt-5-mini",
        label: "GPT-5 mini",
        vendor: "Azure OpenAI",
        selected_wire: CopilotWire::Responses,
    },
    CopilotModelDescriptor { id: "gpt-5.5", label: "GPT-5.5", vendor: "OpenAI", selected_wire: CopilotWire::Responses },
    CopilotModelDescriptor { id: "claude-fable-5", label: "Claude Fable 5", vendor: "Anthropic", selected_wire: CopilotWire::ChatCompletions },
    CopilotModelDescriptor { id: "claude-opus-4.7", label: "Claude Opus 4.7", vendor: "Anthropic", selected_wire: CopilotWire::ChatCompletions },
    CopilotModelDescriptor { id: "claude-opus-4.8", label: "Claude Opus 4.8", vendor: "Anthropic", selected_wire: CopilotWire::ChatCompletions },
    CopilotModelDescriptor { id: "claude-opus-4.8-fast", label: "Claude Opus 4.8 (fast mode)", vendor: "Anthropic", selected_wire: CopilotWire::ChatCompletions },
    CopilotModelDescriptor {
        id: "claude-sonnet-4.6",
        label: "Claude Sonnet 4.6",
        vendor: "Anthropic",
        selected_wire: CopilotWire::ChatCompletions,
    },
    CopilotModelDescriptor {
        id: "claude-sonnet-5",
        label: "Claude Sonnet 5",
        vendor: "Anthropic",
        selected_wire: CopilotWire::ChatCompletions,
    },
    CopilotModelDescriptor {
        id: "claude-haiku-4.5",
        label: "Claude Haiku 4.5",
        vendor: "Anthropic",
        selected_wire: CopilotWire::ChatCompletions,
    },
    CopilotModelDescriptor {
        id: "gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro",
        vendor: "Google",
        selected_wire: CopilotWire::ChatCompletions,
    },
    CopilotModelDescriptor {
        id: "gemini-3.5-flash",
        label: "Gemini 3.5 Flash",
        vendor: "Google",
        selected_wire: CopilotWire::ChatCompletions,
    },
];

/// Headers attached to the experimental Copilot models GET.
/// Session bearer is applied by the broker — never pass the GitHub user token.
pub fn models_request_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("User-Agent", "SynapsCLI/0.6.0"),
        ("Accept", "application/json"),
        ("Editor-Version", "vscode/1.107.0"),
        ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
        ("Copilot-Integration-Id", "vscode-chat"),
        ("X-Github-Api-Version", COPILOT_API_VERSION),
    ]
}

/// Validate the pinned models URL (fail closed on host/path drift).
pub fn validate_models_endpoint(url: &str) -> Result<(), String> {
    if url != MODELS_URL {
        return Err("github-copilot models endpoint is not the pinned URL".into());
    }
    Ok(())
}

/// Look up a curated fallback descriptor by wire id.
pub fn copilot_model(id: &str) -> Option<&'static CopilotModelDescriptor> {
    COPILOT_FALLBACK_MODELS.iter().find(|m| m.id == id)
}

/// Select the wire protocol from endpoints established by the captured live
/// catalog. This is descriptor-driven: only IDs whose reviewed endpoint has
/// been established by the fixture route. Unknown account models fail closed.
pub fn runtime_wire_protocol(id: &str) -> Option<crate::runtime::openai::WireProtocol> {
    copilot_model(id).map(|d| d.selected_wire.to_wire_protocol())
}

/// Choose a broker wire protocol from a fixture row's `vendor` and
/// `supported_endpoints`. Returns `None` when no reviewed endpoint is
/// advertised (fail closed).
///
/// Preference:
/// - OpenAI / Azure OpenAI: prefer `/responses`, else `/chat/completions`.
/// - Anything else (Google, Anthropic, unknown): prefer `/chat/completions`,
///   else `/responses`. Anthropic's `/v1/messages` is *not* selected because
///   the broker does not yet route it.
pub fn preferred_wire_protocol_from_endpoints(
    vendor: Option<&str>,
    endpoints: &[CopilotEndpoint],
) -> Option<crate::runtime::openai::WireProtocol> {
    use crate::runtime::openai::WireProtocol;
    let has_responses = endpoints.iter().any(|e| matches!(e, CopilotEndpoint::Responses));
    let has_chat = endpoints
        .iter()
        .any(|e| matches!(e, CopilotEndpoint::ChatCompletions));
    let prefer_responses = matches!(vendor, Some("OpenAI") | Some("Azure OpenAI"));
    if prefer_responses && has_responses {
        return Some(WireProtocol::OpenAiResponses);
    }
    if has_chat {
        return Some(WireProtocol::OpenAiChatCompletions);
    }
    if has_responses {
        return Some(WireProtocol::OpenAiResponses);
    }
    None
}

/// Static fallback catalog for offline / seed UI paths. Every entry here is
/// selectable by construction (see [`COPILOT_FALLBACK_MODELS`]).
pub fn copilot_static_catalog_models() -> Vec<CatalogModel> {
    COPILOT_FALLBACK_MODELS
        .iter()
        .map(|descriptor| CatalogModel {
            provider_key: PROVIDER_KEY.into(),
            provider_name: PROVIDER_NAME.into(),
            provider_kind: CatalogProviderKind::Generic {
                key: PROVIDER_KEY.into(),
            },
            id: descriptor.id.into(),
            label: Some(descriptor.label.into()),
            context_tokens: None,
            max_output_tokens: None,
            input_modalities: vec![Modality::Text],
            pricing: PricingSummary::default(),
            reasoning: ReasoningSupport::Unknown,
            source: CatalogSource::StaticFallback,
        })
        .collect()
}

// ── Wire parse (experimental) ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CopilotModelsResponse {
    data: Vec<CopilotModelItem>,
}

#[derive(Debug, Deserialize)]
struct CopilotModelItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    vendor: Option<String>,
    #[serde(default)]
    capabilities: Option<CopilotCapabilities>,
    #[serde(default)]
    preview: Option<bool>,
    #[serde(default)]
    model_picker_enabled: Option<bool>,
    #[serde(default)]
    is_chat_default: Option<bool>,
    #[serde(default)]
    is_chat_fallback: Option<bool>,
    #[serde(default)]
    policy: Option<CopilotPolicy>,
    #[serde(default)]
    supported_endpoints: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CopilotPolicy {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopilotCapabilities {
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    limits: Option<CopilotLimits>,
    #[serde(default)]
    supports: Option<CopilotSupports>,
}

#[derive(Debug, Deserialize)]
struct CopilotLimits {
    #[serde(default)]
    max_context_window_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    max_prompt_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CopilotSupports {
    #[serde(default)]
    vision: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<serde_json::Value>,
    #[serde(default)]
    adaptive_thinking: Option<bool>,
}

/// Rich, evidence-preserving row from an experimental Copilot `/models` body.
///
/// Unlike [`CatalogModel`] this keeps every dimension the picker/router
/// consults: policy state, picker flag, vendor, advertised endpoints, and
/// capability hints. Use [`selectable_copilot_entries`] for the filtered
/// picker view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotCatalogEntry {
    pub id: String,
    pub label: String,
    pub vendor: Option<String>,
    /// Capabilities `type` (`chat`, `embeddings`, `completion`, …).
    pub kind: String,
    pub model_picker_enabled: bool,
    pub policy_state: CopilotPolicyState,
    pub preview: bool,
    pub is_chat_default: bool,
    pub is_chat_fallback: bool,
    pub supported_endpoints: Vec<CopilotEndpoint>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub vision: bool,
    pub thinking: bool,
}

impl CopilotCatalogEntry {
    /// Broker wire protocol chosen from vendor + `supported_endpoints`.
    pub fn preferred_wire_protocol(&self) -> Option<crate::runtime::openai::WireProtocol> {
        preferred_wire_protocol_from_endpoints(self.vendor.as_deref(), &self.supported_endpoints)
    }

    /// True when this row should surface in the picker AND is routable.
    pub fn is_selectable_for_picker(&self) -> bool {
        self.kind.eq_ignore_ascii_case("chat")
            && (self.model_picker_enabled
                || matches!(self.policy_state, CopilotPolicyState::Disabled))
            && self.preferred_wire_protocol().is_some()
    }
}

fn body_size_guard(body: &str) -> Result<(), String> {
    if body.len() > MAX_MODELS_BODY_BYTES {
        return Err(format!(
            "github-copilot models body exceeded the {MAX_MODELS_BODY_BYTES}-byte cap"
        ));
    }
    Ok(())
}

fn parse_response(body: &str) -> Result<CopilotModelsResponse, String> {
    body_size_guard(body)?;
    serde_json::from_str(body).map_err(|e| format!("github-copilot models parse failed: {e}"))
}

fn to_entry(item: CopilotModelItem) -> Option<CopilotCatalogEntry> {
    let id = item.id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let caps = item.capabilities.as_ref();
    let kind = caps
        .and_then(|c| c.kind.as_deref())
        .unwrap_or("chat")
        .to_string();
    let (context_tokens, max_output_tokens, vision, thinking) = caps
        .map(|c| {
            let (ctx, out) = c
                .limits
                .as_ref()
                .map(|l| {
                    (
                        l.max_context_window_tokens.or(l.max_prompt_tokens),
                        l.max_output_tokens,
                    )
                })
                .unwrap_or((None, None));
            let (vision, thinking) = c
                .supports
                .as_ref()
                .map(|s| {
                    let thinking = s.adaptive_thinking == Some(true)
                        || s.reasoning_effort.as_ref().is_some_and(|v| !v.is_null());
                    (s.vision.unwrap_or(false), thinking)
                })
                .unwrap_or((false, false));
            (ctx, out, vision, thinking)
        })
        .unwrap_or((None, None, false, false));
    let supported_endpoints = item
        .supported_endpoints
        .unwrap_or_default()
        .into_iter()
        .map(|s| CopilotEndpoint::parse(&s))
        .collect();
    let label = item
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    Some(CopilotCatalogEntry {
        id,
        label,
        vendor: item.vendor.filter(|v| !v.trim().is_empty()),
        kind,
        model_picker_enabled: item.model_picker_enabled.unwrap_or(false),
        policy_state: CopilotPolicyState::from_opt(item.policy.and_then(|p| p.state).as_deref()),
        preview: item.preview.unwrap_or(false),
        is_chat_default: item.is_chat_default.unwrap_or(false),
        is_chat_fallback: item.is_chat_fallback.unwrap_or(false),
        supported_endpoints,
        context_tokens,
        max_output_tokens,
        vision,
        thinking,
    })
}

/// Parse an experimental Copilot `/models` body into rich descriptor entries.
///
/// Fails closed on malformed JSON or a missing `data` array. All rows are
/// preserved (no capability-based filter here) so callers can distinguish
/// disabled/utility rows from selectable ones without re-parsing.
pub fn parse_copilot_catalog_entries(body: &str) -> Result<Vec<CopilotCatalogEntry>, String> {
    let resp = parse_response(body)?;
    Ok(resp.data.into_iter().filter_map(to_entry).collect())
}

/// Parse an experimental Copilot `/models` body into normalized catalog rows.
///
/// The returned view includes chat rows with at least one reviewed endpoint.
/// A disabled policy identifies an account opt-in row and does not hide it; otherwise
/// the upstream picker flag remains the utility-row visibility boundary. Callers wanting the raw evidence use
/// [`parse_copilot_catalog_entries`].
pub fn parse_copilot_catalog_models(body: &str) -> Result<Vec<CatalogModel>, String> {
    let entries = parse_copilot_catalog_entries(body)?;
    Ok(entries
        .into_iter()
        .filter(CopilotCatalogEntry::is_selectable_for_picker)
        .filter_map(|e| catalog_model_from_entry(&e))
        .collect())
}

/// Selectable-only view — helper for pickers and expanded-fallback checks.
pub fn selectable_copilot_entries(body: &str) -> Result<Vec<CopilotCatalogEntry>, String> {
    Ok(parse_copilot_catalog_entries(body)?
        .into_iter()
        .filter(CopilotCatalogEntry::is_selectable_for_picker)
        .collect())
}

fn catalog_model_from_entry(e: &CopilotCatalogEntry) -> Option<CatalogModel> {
    let mut m = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, &e.id)?;
    m.provider_kind = CatalogProviderKind::Generic {
        key: PROVIDER_KEY.into(),
    };
    m.label = Some(e.label.clone());
    m.context_tokens = e.context_tokens;
    m.max_output_tokens = e.max_output_tokens;
    let mut modalities = vec![Modality::Text];
    if e.vision {
        modalities.push(Modality::Image);
    }
    m.input_modalities = modalities;
    m.reasoning = if e.thinking {
        ReasoningSupport::GenericOpenAi
    } else {
        ReasoningSupport::Unknown
    };
    m.source = CatalogSource::Live;
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_FIXTURE: &str = include_str!("fixtures/github_copilot_models.json");

    #[test]
    fn fallback_wire_ids_are_fixture_selectable_set() {
        let entries = parse_copilot_catalog_entries(LIVE_FIXTURE).expect("fixture parse");
        let selectable: std::collections::HashSet<_> = entries
            .iter()
            .filter(|e| e.is_selectable_for_picker())
            .map(|e| e.id.as_str())
            .collect();
        let fallback: std::collections::HashSet<_> =
            COPILOT_FALLBACK_MODELS.iter().map(|d| d.id).collect();
        assert_eq!(
            fallback, selectable,
            "curated fallback must match the fixture's selectable set exactly"
        );
        // Never seed retired / unobserved ids.
        assert!(copilot_model("gpt-5.6-sol").is_none());
        assert!(copilot_model("auto").is_none());
        assert!(copilot_model("gpt-4.1").is_none());
        assert!(copilot_model("claude-sonnet-4").is_none());
        assert!(copilot_model("gemini-3-pro").is_none());
        // Endpointless rows remain excluded.
        assert!(copilot_model("gemini-3-flash-preview").is_none());
    }

    #[test]
    fn static_catalog_uses_fallback_source_and_prefixed_runtime_ids() {
        let models = copilot_static_catalog_models();
        assert_eq!(models.len(), COPILOT_FALLBACK_MODELS.len());
        assert!(models
            .iter()
            .all(|m| m.source == CatalogSource::StaticFallback));
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("github-copilot/")));
        assert_eq!(models[0].id, "gpt-5.3-codex");
        assert_eq!(models[0].label.as_deref(), Some("GPT-5.3-Codex"));
    }

    #[test]
    fn models_endpoint_is_pinned() {
        validate_models_endpoint(MODELS_URL).unwrap();
        assert!(
            validate_models_endpoint("https://api.individual.githubcopilot.com/models").is_err()
        );
        assert!(validate_models_endpoint("https://api.githubcopilot.com/models/").is_err());
        assert!(validate_models_endpoint("https://evil.example/models").is_err());
        assert!(validate_models_endpoint("http://api.githubcopilot.com/models").is_err());
    }

    #[test]
    fn models_headers_include_integration_and_api_version() {
        let map: std::collections::HashMap<_, _> =
            models_request_headers().iter().copied().collect();
        assert_eq!(map.get("User-Agent"), Some(&"SynapsCLI/0.6.0"));
        assert_eq!(map.get("Copilot-Integration-Id"), Some(&"vscode-chat"));
        assert_eq!(map.get("X-Github-Api-Version"), Some(&COPILOT_API_VERSION));
        assert_eq!(map.get("Editor-Version"), Some(&"vscode/1.107.0"));
    }

    #[test]
    fn parse_rejects_malformed_and_missing_data() {
        assert!(parse_copilot_catalog_models("{not json}").is_err());
        assert!(parse_copilot_catalog_models(r#"{"models":[]}"#).is_err());
        assert!(parse_copilot_catalog_models("[]").is_err());
    }

    #[test]
    fn parse_rejects_oversized_body() {
        let huge = format!(
            "{{\"data\":[{{\"id\":\"x\",\"capabilities\":{{\"type\":\"chat\"}}}}{}]}}",
            " ".repeat(MAX_MODELS_BODY_BYTES)
        );
        let err = parse_copilot_catalog_models(&huge).unwrap_err();
        assert!(err.contains("exceeded"), "{err}");
    }

    #[test]
    fn parse_filters_non_chat_and_endpointless_but_keeps_opt_in_rows() {
        let body = r#"{
          "object":"list",
          "data":[
            {"id":"","capabilities":{"type":"chat"}},
            {"id":"embed-1","name":"Embed","capabilities":{"type":"embeddings"},
             "model_picker_enabled":true,"policy":{"state":"enabled"},
             "supported_endpoints":["/chat/completions"]},
            {"id":"complete-1","name":"Complete","capabilities":{"type":"completion"},
             "model_picker_enabled":true,"policy":{"state":"enabled"},
             "supported_endpoints":["/chat/completions"]},
            {"id":"disabled-chat","name":"Disabled","capabilities":{"type":"chat"},
             "model_picker_enabled":true,"policy":{"state":"disabled"},
             "supported_endpoints":["/chat/completions"]},
            {"id":"unpicked-chat","name":"Hidden","capabilities":{"type":"chat"},
             "model_picker_enabled":false,"policy":{"state":"enabled"},
             "supported_endpoints":["/chat/completions"]},
            {"id":"endpointless","name":"Endpointless","capabilities":{"type":"chat"},
             "model_picker_enabled":true,"policy":{"state":"enabled"}},
            {"id":"only-v1-messages","name":"OnlyMessages","vendor":"Anthropic",
             "capabilities":{"type":"chat"},"model_picker_enabled":true,
             "policy":{"state":"enabled"},"supported_endpoints":["/v1/messages"]},
            {"id":"only-ws","name":"OnlyWs","vendor":"OpenAI",
             "capabilities":{"type":"chat"},"model_picker_enabled":true,
             "policy":{"state":"enabled"},"supported_endpoints":["ws:/responses"]},
            {"id":"gpt-x","name":"GPT-X","vendor":"OpenAI","capabilities":{
               "type":"chat",
               "limits":{"max_context_window_tokens":128000,"max_output_tokens":16384},
               "supports":{"vision":true,"reasoning_effort":["low","high"]}
             },"model_picker_enabled":true,"policy":{"state":"enabled"},
             "supported_endpoints":["/responses","/chat/completions"]}
          ]
        }"#;
        let models = parse_copilot_catalog_models(body).expect("parse");
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["disabled-chat", "gpt-x"]);
        let gpt = &models[1];
        assert_eq!(gpt.context_tokens, Some(128000));
        assert_eq!(gpt.max_output_tokens, Some(16384));
        assert!(gpt.input_modalities.contains(&Modality::Image));
        assert_eq!(gpt.reasoning, ReasoningSupport::GenericOpenAi);
        assert_eq!(gpt.source, CatalogSource::Live);
        assert_eq!(gpt.runtime_id(), "github-copilot/gpt-x");
    }

    #[test]
    fn parse_live_fixture_keeps_only_selectable_rows() {
        let models = parse_copilot_catalog_models(LIVE_FIXTURE).expect("fixture parse");
        let ids: std::collections::HashSet<_> = models.iter().map(|m| m.id.as_str()).collect();
        // All chat rows with reviewed endpoints MUST be present.
        for expected in [
            "gpt-5.3-codex",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
            "gpt-5-mini",
            "claude-sonnet-4.6",
            "claude-sonnet-5",
            "claude-haiku-4.5",
            "gemini-3.1-pro-preview",
            "gemini-3.5-flash",
            "claude-fable-5", "claude-opus-4.7", "claude-opus-4.8",
            "claude-opus-4.8-fast", "gpt-5.5",
        ] {
            assert!(ids.contains(expected), "missing {expected}");
        }
        // Endpointless and non-chat rows MUST NOT be present.
        for banned in [
            "gemini-3-flash-preview",
            "text-embedding-3-small",
            "gpt-41-copilot",
            "trajectory-compaction",
        ] {
            assert!(!ids.contains(banned), "must filter {banned}");
        }
        assert!(models.iter().all(|m| m.provider_key == PROVIDER_KEY));
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("github-copilot/")));
    }

    #[test]
    fn every_fallback_id_selectable_in_live_fixture() {
        let selectable = selectable_copilot_entries(LIVE_FIXTURE).expect("fixture");
        let ids: std::collections::HashSet<_> = selectable.iter().map(|e| e.id.as_str()).collect();
        for d in COPILOT_FALLBACK_MODELS {
            assert!(
                ids.contains(d.id),
                "fallback id {} is not selectable in live fixture",
                d.id
            );
            // And the descriptor's static endpoint pick must match the live
            // evidence-driven pick (no drift between fallback and fixture).
            let entry = selectable.iter().find(|e| e.id == d.id).unwrap();
            assert_eq!(
                Some(d.selected_wire.to_wire_protocol()),
                entry.preferred_wire_protocol(),
                "descriptor `{}` disagrees with fixture endpoint pick",
                d.id
            );
        }
    }

    #[test]
    fn endpoint_preference_openai_prefers_responses_others_prefer_chat() {
        use crate::runtime::openai::WireProtocol;
        // OpenAI + both → responses.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("OpenAI"),
                &[CopilotEndpoint::Responses, CopilotEndpoint::ChatCompletions]
            ),
            Some(WireProtocol::OpenAiResponses)
        );
        // OpenAI + only chat → chat.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("OpenAI"),
                &[CopilotEndpoint::ChatCompletions]
            ),
            Some(WireProtocol::OpenAiChatCompletions)
        );
        // Azure OpenAI treated as OpenAI.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("Azure OpenAI"),
                &[CopilotEndpoint::ChatCompletions, CopilotEndpoint::Responses]
            ),
            Some(WireProtocol::OpenAiResponses)
        );
        // Anthropic: /v1/messages advertised is IGNORED; chat wins.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("Anthropic"),
                &[CopilotEndpoint::V1Messages, CopilotEndpoint::ChatCompletions]
            ),
            Some(WireProtocol::OpenAiChatCompletions)
        );
        // Anthropic w/ only /v1/messages → fail closed.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("Anthropic"),
                &[CopilotEndpoint::V1Messages]
            ),
            None
        );
        // Google: chat only.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("Google"),
                &[CopilotEndpoint::ChatCompletions]
            ),
            Some(WireProtocol::OpenAiChatCompletions)
        );
        // ws:/responses alone → fail closed regardless of vendor.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(
                Some("OpenAI"),
                &[CopilotEndpoint::WsResponses]
            ),
            None
        );
        // Empty list → None.
        assert_eq!(
            preferred_wire_protocol_from_endpoints(Some("OpenAI"), &[]),
            None
        );
    }
}
