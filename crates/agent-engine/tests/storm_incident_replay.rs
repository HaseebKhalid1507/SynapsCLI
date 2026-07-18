//! Network-free replay of the Storm provider-confusion sequence through the
//! same central decision point used by start, one-shot, and resume.
use agent_core::orchestration::{CatalogSnapshot, DelegationPolicy};
use agent_core::prompt::QualifiedModelId;
use agent_engine::orchestration::OrchestrationRuntime;

fn model(value: &str) -> QualifiedModelId {
    QualifiedModelId::parse(value).unwrap()
}

#[test]
fn storm_incident_replay() {
    let foreground = model("openai-codex/gpt-5.6-sol");
    let catalog = CatalogSnapshot::new([
        foreground.clone(),
        model("openai/gpt-5.4"),
        model("openai/gpt-5.2"),
        model("openai/gpt-5.5"),
        model("anthropic/claude-opus-4-7"),
    ]);
    let runtime = OrchestrationRuntime::new(
        DelegationPolicy::baseline(foreground.clone(), catalog, 2, 8).unwrap(),
    );
    let digest_before = runtime.telemetry_json();
    let tempting_log = "historical: anthropic/claude-opus-4-7 and openai-codex/gpt-9";
    assert!(!tempting_log.is_empty());

    for (index, guess) in [
        "openai/gpt-5.4",
        "openai/gpt-5.2",
        "openai/gpt-5.5",
        "anthropic/claude-opus-4-7",
    ]
    .iter()
    .enumerate()
    {
        let denial = runtime
            .resolve_and_authorize(&format!("storm-denied-{index}"), Some(guess))
            .unwrap_err();
        assert!(!denial.network_attempted);
        assert_eq!(denial.code, "provider_not_allowed");
        let rendered = denial.to_string();
        assert!(!rendered.contains("token") && !rendered.contains("historical:"));
    }
    // Reading arbitrary context cannot mutate catalog/policy or reserve limits.
    assert_eq!(
        runtime.telemetry_json().matches("dispatch_allowed").count(),
        0
    );
    assert_ne!(runtime.telemetry_json(), digest_before); // bounded denial audit was emitted

    let inherited = runtime.resolve_and_authorize("storm-final", None).unwrap();
    assert_eq!(inherited.model, foreground);
    assert!(!inherited.network_attempted); // authorization itself performs no I/O
}
