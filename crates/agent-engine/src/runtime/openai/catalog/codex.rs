use super::*;
use serde::Deserialize;

/// Canonical provider key (matches OAuth storage / broker id).
pub const PROVIDER_KEY: &str = "openai-codex";
/// User-facing provider name.
pub const PROVIDER_NAME: &str = "OpenAI Codex";
/// Pinned ChatGPT backend host for Codex model catalog discovery.
pub const MODELS_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// Relative path allowlisted on the broker for catalog discovery.
pub const MODELS_PATH: &str = "/models";

/// Offline seed models used only when the account is not configured / offline.
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
        m.reasoning = ReasoningSupport::Unknown;
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
}

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

/// Parse the ChatGPT backend-api models response into normalized catalog rows.
///
/// Response shape: `{ "models": [ { "slug", "display_name", "visibility",
/// "supported_in_api", "context_window", ... }, ... ] }`.
pub fn parse_codex_catalog_models(body: &str) -> Result<Vec<CatalogModel>, serde_json::Error> {
    let resp: CodexModelsResponse = serde_json::from_str(body)?;
    let models = resp
        .models
        .into_iter()
        .filter(|item| codex_model_is_selectable(item.visibility.as_deref(), item.supported_in_api))
        .filter_map(|item| {
            let mut m = CatalogModel::new(PROVIDER_KEY, PROVIDER_NAME, item.slug)?;
            m.provider_kind = CatalogProviderKind::OpenAiCodex;
            m.label = item.display_name.filter(|name| !name.trim().is_empty());
            m.context_tokens = item.context_window;
            m.reasoning = ReasoningSupport::Unknown;
            m.source = CatalogSource::Live;
            Some(m)
        })
        .collect();
    Ok(models)
}

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
    fn invalid_json_returns_error() {
        assert!(parse_codex_catalog_models("{not json}").is_err());
    }

    #[test]
    fn missing_models_key_returns_error() {
        assert!(parse_codex_catalog_models(r#"{"data":[]}"#).is_err());
    }
}
