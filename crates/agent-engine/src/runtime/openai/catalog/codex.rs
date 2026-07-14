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
    let (supported, default_level): (&[ReasoningLevel], ReasoningLevel) = match model_id {
        "gpt-5.6-sol" => (&[Low, Medium, High, XHigh, Max, Ultra], Low),
        "gpt-5.6-terra" => (&[Low, Medium, High, XHigh, Max, Ultra], Medium),
        "gpt-5.6-luna" => (&[Low, Medium, High, XHigh, Max], Medium),
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" => (&[Low, Medium, High, XHigh], Medium),
        "gpt-5.3-codex-spark" => (&[Low, Medium, High, XHigh], High),
        _ => return None,
    };
    Some(ReasoningSupport::CodexNamed {
        supported: supported.to_vec(),
        default_level: Some(default_level),
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
    supported_reasoning_levels: Vec<CodexReasoningLevelItem>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
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

/// Build a `ReasoningSupport` for a parsed Codex model item.
/// Falls back to static capability data when the live response omits the fields.
fn codex_reasoning_support(item: &CodexModelItem) -> ReasoningSupport {
    let supported = parse_reasoning_levels(&item.supported_reasoning_levels);
    let default_level = parse_default_level(item.default_reasoning_level.as_deref());

    if !supported.is_empty() {
        return ReasoningSupport::CodexNamed {
            supported,
            default_level,
        };
    }
    // Live response didn't include reasoning levels — fall back to static.
    codex_static_capability(&item.slug).unwrap_or(ReasoningSupport::Unknown)
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
    // Populate the process-local capability cache so validation paths
    // (commands, settings) see live data without re-parsing.
    super::capability_cache::populate(&models);
    Ok(models)
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
    // Off/Adaptive omit the provider field; catalogs list concrete efforts only.
    if matches!(level, ReasoningLevel::Off | ReasoningLevel::Adaptive) {
        return Ok(());
    }
    // 1. Explicit CatalogModel argument takes top priority.
    if let Some(model) = catalog_model {
        if let Some(levels) = model.codex_supported_levels() {
            if levels.contains(&level) {
                return Ok(());
            }
            return Err(format!(
                "reasoning level '{level}' is not supported by openai-codex/{model_id}; \
                 supported: [{}]",
                levels
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    // 2. Process-local capability cache (populated from live Codex catalog parse).
    let qualified = format!("openai-codex/{model_id}");
    if let Some(cached) = super::capability_cache::get(&qualified) {
        if let Some(levels) = cached.codex_supported_levels() {
            if levels.contains(&level) {
                return Ok(());
            }
            return Err(format!(
                "reasoning level '{level}' is not supported by openai-codex/{model_id}; \
                 supported: [{}]",
                levels
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    // 3. Static fallback table.
    match codex_static_capability(model_id) {
        Some(ReasoningSupport::CodexNamed { supported, .. }) => {
            if supported.contains(&level) {
                Ok(())
            } else {
                Err(format!(
                    "reasoning level '{level}' is not supported by openai-codex/{model_id}; \
                     supported: [{}]",
                    supported
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
        None => Err(format!(
            "no capability metadata for openai-codex/{model_id}; \
             cannot authorize level '{level}'"
        )),
        _ => unreachable!(),
    }
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

    // ── Static capability table ───────────────────────────────────────────────

    #[test]
    fn static_sol_has_ultra_and_default_low() {
        let cap = codex_static_capability("gpt-5.6-sol").expect("sol");
        match cap {
            ReasoningSupport::CodexNamed {
                supported,
                default_level,
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
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
        };
        // Ultra is in the static table but the live model says otherwise.
        let err =
            validate_codex_level("gpt-5.6-sol", ReasoningLevel::Ultra, Some(&live)).unwrap_err();
        assert!(err.contains("ultra"));
        // Low is accepted.
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Low, Some(&live)).is_ok());
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
        live.source = CatalogSource::Live;
        live.reasoning = ReasoningSupport::CodexNamed {
            supported: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_level: Some(ReasoningLevel::Low),
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
