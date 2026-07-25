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
    // Running workers pass through the gate (reactive subagent pattern).
    assert_eq!(registry.completion_gate(), CompletionGate::Allowed);
    registry.mark_starting(&worker).unwrap();
    registry.mark_running(&worker).unwrap();
    // Still running — gate allows.
    assert_eq!(registry.completion_gate(), CompletionGate::Allowed);
    registry
        .mark_terminal(&worker, WorkerTerminal::Completed)
        .unwrap();
    // Terminal: gate blocks — model must collect.
    assert!(matches!(
        registry.completion_gate(),
        CompletionGate::Blocked { .. }
    ));
    registry.collect(&worker).unwrap();
    // Collected but not reconciled: gate still blocks.
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
    registry.mark_starting(&worker).unwrap();
    registry.mark_running(&worker).unwrap();

    // The first observation establishes a baseline; only a repeated fingerprint
    // counts as unchanged progress.
    registry.poll(&worker, "initial-progress").unwrap();
    registry.poll(&worker, "initial-progress").unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());
    assert!(matches!(
        registry.check_foreground_write("src/lib.rs"),
        agent_core::orchestration::ScopeDecision::ReconciliationRequired { .. }
    ));

    // Steering is required before replacement, but subsequent progress clears
    // both the unchanged-poll streak and the previous steering attempt.
    registry.steer(&worker).unwrap();
    registry.poll(&worker, "new-progress").unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());
    registry.poll(&worker, "new-progress").unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());

    // After steering the newly stuck worker, a second unchanged poll makes it
    // eligible for replacement.
    registry.steer(&worker).unwrap();
    assert!(!registry.is_stalled(&worker).unwrap());
    registry.poll(&worker, "new-progress").unwrap();
    assert!(registry.is_stalled(&worker).unwrap());
}
