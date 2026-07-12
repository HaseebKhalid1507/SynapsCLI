//! GitHub Copilot model catalog (C2 catalog-only).
//!
//! Live discovery hits the community-observed authenticated endpoint
//! `GET https://api.githubcopilot.com/models` (experimental — not a documented
//! stable third-party public product API). Prefer broker-proxied, account-
//! specific discovery. Curated static fallback IDs are restricted to wire IDs
//! established by fixtures/live discovery — never guessed display names.
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

/// Typed static descriptor for curated fallback models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopilotModelDescriptor {
    pub id: &'static str,
    pub label: &'static str,
}

/// Curated fallback — wire IDs established by live discovery/fixtures only.
/// Ordered for UI presentation. Excludes retired models and IDs not observed
/// on the authenticated personal catalog (e.g. `gpt-5.6-sol`, bare `auto`).
pub const COPILOT_FALLBACK_MODELS: &[CopilotModelDescriptor] = &[
    CopilotModelDescriptor {
        id: "gpt-5.3-codex",
        label: "GPT-5.3-Codex",
    },
    CopilotModelDescriptor {
        id: "gpt-5.4",
        label: "GPT-5.4",
    },
    CopilotModelDescriptor {
        id: "gpt-5.5",
        label: "GPT-5.5",
    },
    CopilotModelDescriptor {
        id: "gpt-5.6-luna",
        label: "GPT-5.6 Luna",
    },
    CopilotModelDescriptor {
        id: "gpt-5.6-terra",
        label: "GPT-5.6 Terra",
    },
    CopilotModelDescriptor {
        id: "claude-sonnet-4.6",
        label: "Claude Sonnet 4.6",
    },
    CopilotModelDescriptor {
        id: "claude-sonnet-5",
        label: "Claude Sonnet 5",
    },
    CopilotModelDescriptor {
        id: "claude-opus-4.7",
        label: "Claude Opus 4.7",
    },
    CopilotModelDescriptor {
        id: "claude-opus-4.8",
        label: "Claude Opus 4.8",
    },
    CopilotModelDescriptor {
        id: "claude-fable-5",
        label: "Claude Fable 5",
    },
    CopilotModelDescriptor {
        id: "gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro",
    },
    CopilotModelDescriptor {
        id: "gemini-3.5-flash",
        label: "Gemini 3.5 Flash",
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

/// Static fallback catalog for offline / seed UI paths.
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
    capabilities: Option<CopilotCapabilities>,
    #[serde(default)]
    preview: Option<bool>,
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

/// Parse an experimental Copilot `/models` body into normalized catalog rows.
///
/// Fail closed on malformed JSON or a missing `data` array. Non-chat capability
/// types (embeddings, completion, utility) are skipped. Empty ids are skipped.
pub fn parse_copilot_catalog_models(body: &str) -> Result<Vec<CatalogModel>, String> {
    if body.len() > MAX_MODELS_BODY_BYTES {
        return Err(format!(
            "github-copilot models body exceeded the {MAX_MODELS_BODY_BYTES}-byte cap"
        ));
    }
    let resp: CopilotModelsResponse = serde_json::from_str(body)
        .map_err(|e| format!("github-copilot models parse failed: {e}"))?;
    Ok(resp
        .data
        .into_iter()
        .filter_map(|item| {
            let id = item.id.trim();
            if id.is_empty() {
                return None;
            }
            let kind = item
                .capabilities
                .as_ref()
                .and_then(|c| c.kind.as_deref())
                .unwrap_or("chat");
            // Fail closed for non-chat catalog entries (embeddings/completion/utility).
            if !kind.eq_ignore_ascii_case("chat") {
                return None;
            }
            let mut m = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, id)?;
            m.provider_kind = CatalogProviderKind::Generic {
                key: PROVIDER_KEY.into(),
            };
            m.label = item
                .name
                .filter(|n| !n.trim().is_empty())
                .or_else(|| Some(id.to_string()));
            if let Some(caps) = item.capabilities.as_ref() {
                if let Some(limits) = caps.limits.as_ref() {
                    m.context_tokens = limits
                        .max_context_window_tokens
                        .or(limits.max_prompt_tokens);
                    m.max_output_tokens = limits.max_output_tokens;
                }
                let mut modalities = vec![Modality::Text];
                if caps.supports.as_ref().and_then(|s| s.vision) == Some(true) {
                    modalities.push(Modality::Image);
                }
                m.input_modalities = modalities;
                let thinking = caps.supports.as_ref().is_some_and(|s| {
                    s.adaptive_thinking == Some(true)
                        || s.reasoning_effort
                            .as_ref()
                            .is_some_and(|v| !v.is_null())
                });
                m.reasoning = if thinking {
                    ReasoningSupport::GenericOpenAi
                } else {
                    ReasoningSupport::Unknown
                };
            }
            // Preview flag is retained only via label when name missing; source is live.
            let _ = item.preview;
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_FIXTURE: &str = include_str!("fixtures/github_copilot_models.json");

    #[test]
    fn fallback_wire_ids_match_live_established_set() {
        let ids: Vec<&str> = COPILOT_FALLBACK_MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-5.3-codex",
                "gpt-5.4",
                "gpt-5.5",
                "gpt-5.6-luna",
                "gpt-5.6-terra",
                "claude-sonnet-4.6",
                "claude-sonnet-5",
                "claude-opus-4.7",
                "claude-opus-4.8",
                "claude-fable-5",
                "gemini-3.1-pro-preview",
                "gemini-3.5-flash",
            ]
        );
        // Never seed retired / unobserved ids.
        assert!(copilot_model("gpt-5.6-sol").is_none());
        assert!(copilot_model("auto").is_none());
        assert!(copilot_model("gpt-4.1").is_none());
        assert!(copilot_model("claude-sonnet-4").is_none());
        assert!(copilot_model("gemini-3-pro").is_none());
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
        assert!(validate_models_endpoint("https://api.individual.githubcopilot.com/models").is_err());
        assert!(validate_models_endpoint("https://api.githubcopilot.com/models/").is_err());
        assert!(validate_models_endpoint("https://evil.example/models").is_err());
        assert!(validate_models_endpoint("http://api.githubcopilot.com/models").is_err());
    }

    #[test]
    fn models_headers_include_integration_and_api_version() {
        let map: std::collections::HashMap<_, _> = models_request_headers().iter().copied().collect();
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
    fn parse_skips_non_chat_and_empty_ids() {
        let body = r#"{
          "object":"list",
          "data":[
            {"id":"","capabilities":{"type":"chat"}},
            {"id":"text-embedding-3-small","name":"Embedding","capabilities":{"type":"embeddings"}},
            {"id":"gpt-41-copilot","name":"Completion","capabilities":{"type":"completion"}},
            {"id":"trajectory-compaction","name":"Utility","capabilities":{"type":"chat"}},
            {"id":"gpt-5.4","name":"GPT-5.4","capabilities":{"type":"chat","limits":{"max_context_window_tokens":128000,"max_output_tokens":16384},"supports":{"vision":true,"tool_calls":true,"reasoning_effort":["low","high"]}}}
          ]
        }"#;
        // Note: trajectory-compaction is type=chat in live data — still a utility model.
        // Filtering is by capabilities.type only for this slice (embeddings/completion out).
        let models = parse_copilot_catalog_models(body).expect("parse");
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-5.4"));
        assert!(ids.contains(&"trajectory-compaction")); // type chat in fixture semantics
        assert!(!ids.contains(&"text-embedding-3-small"));
        assert!(!ids.contains(&"gpt-41-copilot"));
        assert!(!ids.iter().any(|id| id.is_empty()));
        let gpt = models.iter().find(|m| m.id == "gpt-5.4").unwrap();
        assert_eq!(gpt.context_tokens, Some(128000));
        assert_eq!(gpt.max_output_tokens, Some(16384));
        assert!(gpt.input_modalities.contains(&Modality::Image));
        assert_eq!(gpt.reasoning, ReasoningSupport::GenericOpenAi);
        assert_eq!(gpt.source, CatalogSource::Live);
        assert_eq!(gpt.runtime_id(), "github-copilot/gpt-5.4");
    }

    #[test]
    fn parse_live_fixture_keeps_high_value_chat_ids() {
        let models = parse_copilot_catalog_models(LIVE_FIXTURE).expect("fixture parse");
        let ids: std::collections::HashSet<_> =
            models.iter().map(|m| m.id.as_str()).collect();
        for expected in [
            "gpt-5.3-codex",
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
            "claude-sonnet-4.6",
            "claude-sonnet-5",
            "claude-opus-4.7",
            "claude-opus-4.8",
            "claude-fable-5",
            "gemini-3.1-pro-preview",
            "gemini-3.5-flash",
        ] {
            assert!(ids.contains(expected), "missing {expected}");
        }
        assert!(!ids.contains("text-embedding-3-small"));
        assert!(!ids.contains("gpt-41-copilot"));
        assert!(models.iter().all(|m| m.provider_key == PROVIDER_KEY));
        assert!(models
            .iter()
            .all(|m| m.runtime_id().starts_with("github-copilot/")));
    }

    #[test]
    fn every_fallback_id_appears_in_live_fixture() {
        let models = parse_copilot_catalog_models(LIVE_FIXTURE).expect("fixture parse");
        let ids: std::collections::HashSet<_> =
            models.iter().map(|m| m.id.as_str()).collect();
        for d in COPILOT_FALLBACK_MODELS {
            assert!(
                ids.contains(d.id),
                "fallback id {} missing from live fixture — do not guess wire ids",
                d.id
            );
        }
    }
}
