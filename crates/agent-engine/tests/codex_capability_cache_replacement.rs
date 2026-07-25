use agent_core::reasoning::ReasoningLevel;
use agent_engine::runtime::openai::catalog::{
    capability_cache, parse_codex_catalog_models, plan_codex_execution, CatalogModel,
    CodexPlanErrorCode, CodexRequestRole, CODEX_PROVIDER_KEY,
};

#[test]
fn successful_codex_catalog_replacement_revokes_removed_ultra_row() {
    let other_provider =
        CatalogModel::new("anthropic", "Anthropic", "claude-cache-sentinel").unwrap();
    capability_cache::insert(other_provider.clone());

    let first = parse_codex_catalog_models(
        r#"{"models":[{
            "slug":"gpt-cache-ultra-v2",
            "visibility":"list",
            "supported_reasoning_levels":[{"effort":"max"},{"effort":"ultra"}],
            "multi_agent_version":"v2"
        }]}"#,
    )
    .expect("first successful Codex catalog");
    capability_cache::replace_provider(CODEX_PROVIDER_KEY, &first);

    plan_codex_execution(
        "openai-codex/gpt-cache-ultra-v2",
        ReasoningLevel::Ultra,
        CodexRequestRole::Foreground,
        None,
    )
    .expect("the first live snapshot authorizes Ultra");

    let replacement = parse_codex_catalog_models(
        r#"{"models":[{
            "slug":"gpt-cache-replacement",
            "visibility":"list",
            "supported_reasoning_levels":[{"effort":"low"}],
            "multi_agent_version":"v1"
        }]}"#,
    )
    .expect("later successful Codex catalog");
    capability_cache::replace_provider(CODEX_PROVIDER_KEY, &replacement);

    let error = plan_codex_execution(
        "openai-codex/gpt-cache-ultra-v2",
        ReasoningLevel::Ultra,
        CodexRequestRole::Foreground,
        None,
    )
    .expect_err("a removed live row must not retain stale Ultra authorization");
    assert_eq!(error.code(), CodexPlanErrorCode::CapabilityMetadataMissing);
    assert!(
        capability_cache::get("openai-codex/gpt-cache-ultra-v2").is_none(),
        "the removed Codex row must leave the cache"
    );
    assert!(
        capability_cache::get("openai-codex/gpt-cache-replacement").is_some(),
        "the replacement Codex row must be installed"
    );
    assert_eq!(
        capability_cache::get(&other_provider.runtime_id()),
        Some(other_provider),
        "provider-scoped replacement must preserve other providers"
    );
}
