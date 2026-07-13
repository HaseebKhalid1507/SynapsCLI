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
    let runtime = OrchestrationRuntime::baseline(foreground, 1, 2);
    runtime.resolve_and_authorize("sa_1", None).unwrap();
    let denied = runtime.resolve_and_authorize("sa_2", None).unwrap_err();
    assert_eq!(denied.code, "concurrency_limit");
    assert!(!denied.network_attempted);
}
