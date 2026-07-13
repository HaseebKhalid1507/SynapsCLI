use agent_core::orchestration::{
    CompletionGate, DelegationPolicy, WorkerRegistry, WorkerRole, WorkerTerminal, WorkerWritePolicy,
};
use agent_core::prompt::QualifiedModelId;

fn model(value: &str) -> QualifiedModelId {
    QualifiedModelId::parse(value).unwrap()
}

#[test]
fn sonnet_foreman_cannot_finish_after_delegating_without_reconciliation() {
    let mut registry = WorkerRegistry::new(DelegationPolicy::enforced(
        model("anthropic/claude-sonnet"),
        [model("anthropic/claude-haiku")],
        1,
        1,
    ));
    let worker = registry
        .authorize_dispatch(
            &model("anthropic/claude-haiku"),
            WorkerRole::Implementer,
            WorkerWritePolicy::NonOverlappingPaths(vec!["src/**".into()]),
        )
        .unwrap();
    assert!(matches!(
        registry.completion_gate(),
        CompletionGate::Blocked { .. }
    ));
    registry.mark_running(&worker).unwrap();
    registry
        .mark_terminal(&worker, WorkerTerminal::Completed)
        .unwrap();
    registry.collect(&worker).unwrap();
    assert!(matches!(
        registry.completion_gate(),
        CompletionGate::Blocked { .. }
    ));
    registry.reconcile(&worker).unwrap();
    assert_eq!(registry.completion_gate(), CompletionGate::Allowed);
}

#[test]
fn kimi_foreman_does_not_take_over_after_one_unchanged_poll() {
    let mut registry = WorkerRegistry::new(DelegationPolicy::enforced(
        model("openrouter/moonshotai/kimi"),
        [model("openrouter/z-ai/glm")],
        1,
        1,
    ));
    let worker = registry
        .authorize_dispatch(
            &model("openrouter/z-ai/glm"),
            WorkerRole::Implementer,
            WorkerWritePolicy::NonOverlappingPaths(vec!["src/**".into()]),
        )
        .unwrap();
    registry.mark_running(&worker).unwrap();
    registry.poll(&worker, "unchanged").unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());
    assert!(matches!(
        registry.check_foreground_write("src/lib.rs"),
        agent_core::orchestration::ScopeDecision::ReconciliationRequired { .. }
    ));
    registry.steer(&worker).unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());
    registry.poll(&worker, "unchanged").unwrap();
    assert!(registry.is_stalled(&worker).unwrap());
}
