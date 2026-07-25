use agent_core::prompt::{
    builtin_orchestration_adapters, compose_orchestration_prompt, PromptSelectors,
    QualifiedModelId, SelectionContext, WorkflowMode,
};

fn context(model: &str, mode: Option<WorkflowMode>) -> SelectionContext {
    SelectionContext::new(QualifiedModelId::parse(model).unwrap(), None)
        .unwrap()
        .with_workflow_mode(mode)
}

#[test]
fn exact_anthropic_ultracode_selects_once_and_only_for_typed_mode() {
    let adapters = builtin_orchestration_adapters();
    let selector = PromptSelectors::provider_exact_workflow(
        "anthropic",
        QualifiedModelId::parse("anthropic/claude-fable-5").unwrap(),
        WorkflowMode::UltraCode,
    )
    .unwrap();
    assert_eq!(selector.workflow_mode(), Some(WorkflowMode::UltraCode));
    assert_eq!(
        adapters
            .iter()
            .filter(|m| m.id.as_str() == "builtin.anthropic.ultracode-workflow")
            .count(),
        1
    );

    let positive = compose_orchestration_prompt(
        Some("BASE"),
        &context("anthropic/claude-fable-5", Some(WorkflowMode::UltraCode)),
    )
    .unwrap();
    assert_eq!(
        (
            positive.matches("<anthropic-ultracode-workflow>").count(),
            positive.matches("</anthropic-ultracode-workflow>").count(),
        ),
        (1, 1)
    );

    for (model, mode) in [
        ("anthropic/claude-fable-5", None),
        ("anthropic/claude-fable-5", Some(WorkflowMode::Max)),
        ("anthropic/claude-fable-5", Some(WorkflowMode::XHigh)),
        ("openai-codex/claude-fable-5", Some(WorkflowMode::UltraCode)),
        ("openai-codex/gpt-5.6-sol", Some(WorkflowMode::UltraCode)),
    ] {
        let prompt = compose_orchestration_prompt(Some("BASE"), &context(model, mode)).unwrap();
        assert!(
            !prompt.contains("<anthropic-ultracode-workflow>"),
            "{model:?} {mode:?}"
        );
    }
}
