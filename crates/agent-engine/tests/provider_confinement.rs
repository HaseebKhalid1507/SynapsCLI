use agent_core::orchestration::{CatalogSnapshot, DelegationPolicy};
use agent_core::prompt::QualifiedModelId;
use agent_engine::orchestration::OrchestrationRuntime;

fn model(value: &str) -> QualifiedModelId {
    QualifiedModelId::parse(value).unwrap()
}

#[test]
fn omitted_worker_model_inherits_foreground_and_all_explicit_requests_are_authorized() {
    let foreground = model("openai-codex/gpt-5.6-sol");
    let catalog = CatalogSnapshot::new([foreground.clone()]);
    let runtime = OrchestrationRuntime::new(
        DelegationPolicy::baseline(foreground.clone(), catalog, 1, 2).unwrap(),
    );

    let inherited = runtime.resolve_and_authorize("sa_1", None).unwrap();
    assert_eq!(inherited.model.as_str(), foreground.as_str());
    assert_eq!(
        inherited.selection_source.as_str(),
        "foreground_inheritance"
    );
    assert!(!inherited.network_attempted);

    let denied = runtime
        .resolve_and_authorize("sa_2", Some("anthropic/claude-opus-4-7"))
        .unwrap_err();
    assert_eq!(denied.code, "catalog_model_unknown");
    assert_eq!(denied.foreground_model, foreground.as_str());
    assert!(!denied.network_attempted);
}

#[test]
fn limits_are_reserved_atomically_by_the_central_decision_point() {
    let foreground = model("openai-codex/gpt-5.6-sol");
    let runtime = OrchestrationRuntime::baseline(foreground, 1, 2).unwrap();
    runtime.resolve_and_authorize("sa_1", None).unwrap();
    let denied = runtime.resolve_and_authorize("sa_2", None).unwrap_err();
    assert_eq!(denied.code, "concurrency_limit");
    assert!(!denied.network_attempted);
}

// ── Task 4: model-aware reasoning level validation ────────────────────────────

#[cfg(test)]
mod reasoning_validation {
    use agent_core::reasoning::ReasoningLevel;
    use agent_engine::runtime::openai::catalog::{
        codex_static_capability, validate_codex_level, ReasoningSupport,
    };

    #[test]
    fn sol_accepts_ultra_and_max() {
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Ultra, None).is_ok());
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::Max, None).is_ok());
        assert!(validate_codex_level("gpt-5.6-sol", ReasoningLevel::XHigh, None).is_ok());
    }

    #[test]
    fn luna_rejects_ultra() {
        let err = validate_codex_level("gpt-5.6-luna", ReasoningLevel::Ultra, None).unwrap_err();
        assert!(err.contains("ultra"), "{err}");
        assert!(err.contains("gpt-5.6-luna"), "{err}");
    }

    #[test]
    fn luna_accepts_max() {
        assert!(validate_codex_level("gpt-5.6-luna", ReasoningLevel::Max, None).is_ok());
    }

    #[test]
    fn gpt55_rejects_max_and_ultra() {
        assert!(validate_codex_level("gpt-5.5", ReasoningLevel::Max, None).is_err());
        assert!(validate_codex_level("gpt-5.5", ReasoningLevel::Ultra, None).is_err());
        assert!(validate_codex_level("gpt-5.4", ReasoningLevel::Max, None).is_err());
        assert!(validate_codex_level("gpt-5.4-mini", ReasoningLevel::Max, None).is_err());
        assert!(validate_codex_level("gpt-5.3-codex-spark", ReasoningLevel::Max, None).is_err());
    }

    #[test]
    fn gpt55_accepts_xhigh() {
        assert!(validate_codex_level("gpt-5.5", ReasoningLevel::XHigh, None).is_ok());
        assert!(validate_codex_level("gpt-5.4", ReasoningLevel::XHigh, None).is_ok());
    }

    #[test]
    fn non_codex_model_has_no_capability_table() {
        // Providers without authoritative metadata must not gain max/ultra
        assert!(codex_static_capability("anthropic/claude-opus-4-7").is_none());
        assert!(codex_static_capability("groq/llama-3.3-70b").is_none());
        assert!(codex_static_capability("gpt-4o").is_none());
    }
}
