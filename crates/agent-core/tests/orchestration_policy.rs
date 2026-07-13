use agent_core::orchestration::*;
use agent_core::prompt::QualifiedModelId;

fn model(s: &str) -> QualifiedModelId {
    QualifiedModelId::parse(s).unwrap()
}

#[test]
fn dispatch_allowlists_and_foreground_are_enforced_before_network() {
    let policy =
        DelegationPolicy::enforced(model("anthropic/sonnet"), [model("anthropic/haiku")], 2, 3);
    let mut workers = WorkerRegistry::new(policy);
    let denied = workers
        .authorize_dispatch(
            &model("openrouter/kimi"),
            WorkerRole::Implementer,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap_err();
    assert_eq!(denied.code(), "provider_not_allowed");
    assert_eq!(workers.foreground_model().as_str(), "anthropic/sonnet");
    assert_eq!(workers.total_dispatched(), 0);
    workers
        .authorize_dispatch(
            &model("anthropic/haiku"),
            WorkerRole::Tester,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap();
    assert_eq!(workers.foreground_model().as_str(), "anthropic/sonnet");
}

#[test]
fn lifecycle_completion_and_overlap_are_enforced() {
    let policy =
        DelegationPolicy::enforced(model("anthropic/sonnet"), [model("anthropic/haiku")], 2, 3);
    let mut r = WorkerRegistry::new(policy);
    let h = r
        .authorize_dispatch(
            &model("anthropic/haiku"),
            WorkerRole::Implementer,
            WorkerWritePolicy::NonOverlappingPaths(vec!["src/**".into()]),
        )
        .unwrap();
    r.mark_running(&h).unwrap();
    assert!(matches!(
        r.completion_gate(),
        CompletionGate::Blocked { .. }
    ));
    assert!(matches!(
        r.check_foreground_write("src/lib.rs"),
        ScopeDecision::ReconciliationRequired { .. }
    ));
    r.poll(&h, "same").unwrap();
    r.poll(&h, "same").unwrap();
    assert!(!r.is_stalled(&h).unwrap());
    r.steer(&h).unwrap();
    r.mark_terminal(&h, WorkerTerminal::Completed).unwrap();
    assert!(matches!(
        r.completion_gate(),
        CompletionGate::Blocked { .. }
    ));
    r.collect(&h).unwrap();
    r.reconcile(&h).unwrap();
    assert_eq!(r.completion_gate(), CompletionGate::Allowed);
}

#[test]
fn advisory_completion_warns_and_telemetry_is_structured_and_safe() {
    let mut p =
        DelegationPolicy::enforced(model("openrouter/kimi"), [model("openrouter/glm")], 1, 2);
    p.mode = EnforcementMode::Advisory;
    let mut r = WorkerRegistry::new(p);
    let _ = r
        .authorize_dispatch(
            &model("openrouter/glm"),
            WorkerRole::Reviewer,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap();
    assert!(matches!(
        r.completion_gate(),
        CompletionGate::Warning { .. }
    ));
    let json = serde_json::to_string(r.telemetry()).unwrap();
    assert!(json.contains("worker.dispatch_requested"));
    assert!(!json.contains("prompt") && !json.contains("secret"));
}
