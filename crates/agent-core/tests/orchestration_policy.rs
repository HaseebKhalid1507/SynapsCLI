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
    assert_eq!(denied.code(), "catalog_model_unknown");
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
    r.mark_starting(&h).unwrap();
    r.mark_running(&h).unwrap();
    // Running workers pass through the gate (reactive subagent pattern).
    assert_eq!(r.completion_gate(), CompletionGate::Allowed);
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
    let h = r
        .authorize_dispatch(
            &model("openrouter/glm"),
            WorkerRole::Reviewer,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap();
    // Running workers pass through even in advisory mode.
    assert_eq!(r.completion_gate(), CompletionGate::Allowed);
    // Transition to terminal — advisory mode warns (not blocks).
    r.mark_starting(&h).unwrap();
    r.mark_running(&h).unwrap();
    r.mark_terminal(&h, WorkerTerminal::Completed).unwrap();
    assert!(matches!(
        r.completion_gate(),
        CompletionGate::Warning { .. }
    ));
    let json = serde_json::to_string(r.telemetry()).unwrap();
    assert!(json.contains("worker.dispatch_requested"));
    assert!(!json.contains("prompt") && !json.contains("secret"));
}

#[test]
fn telemetry_is_bounded_and_fingerprint_progress_resets_stall_state() {
    let policy =
        DelegationPolicy::enforced(model("anthropic/sonnet"), [model("anthropic/haiku")], 1, 1);
    let mut r = WorkerRegistry::new(policy);
    let h = r
        .authorize_dispatch(
            &model("anthropic/haiku"),
            WorkerRole::Tester,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap();
    r.mark_starting(&h).unwrap();
    r.mark_running(&h).unwrap();
    for _ in 0..300 {
        r.poll(&h, "unchanged").unwrap();
    }
    r.steer(&h).unwrap();
    assert!(r.is_stalled(&h).unwrap());
    r.poll(&h, "progress").unwrap();
    assert!(!r.is_stalled(&h).unwrap());
    assert_eq!(r.telemetry().len(), 256);
    assert!(r.dropped_telemetry() > 0);
}

#[test]
fn rollback_restores_dispatch_budget() {
    let policy =
        DelegationPolicy::enforced(model("anthropic/sonnet"), [model("anthropic/haiku")], 1, 1);
    let mut r = WorkerRegistry::new(policy);
    let h = r
        .authorize_dispatch(
            &model("anthropic/haiku"),
            WorkerRole::Tester,
            WorkerWritePolicy::ReadOnly,
        )
        .unwrap();
    r.rollback_dispatch(&h).unwrap();
    assert_eq!(r.total_dispatched(), 0);
    assert!(r
        .authorize_dispatch(
            &model("anthropic/haiku"),
            WorkerRole::Tester,
            WorkerWritePolicy::ReadOnly,
        )
        .is_ok());
}

/// Regression: trusted models were never meant to be pinned at session start.
/// An explicit mid-session user grant for a cross-provider model must flip the
/// next dispatch from `provider_not_allowed` to authorized.
#[test]
fn mid_session_user_grant_admits_cross_provider_worker() {
    let sol = model("openai-codex/gpt-5.6-sol");
    let policy = DelegationPolicy::enforced(
        model("anthropic/claude-fable-5"),
        [model("anthropic/claude-fable-5")],
        2,
        4,
    );
    let mut r = WorkerRegistry::new(policy);
    let denied = r.validate_dispatch(&sol).unwrap_err();
    assert!(matches!(
        denied.typed_code(),
        DispatchFailureCode::ProviderNotAllowed | DispatchFailureCode::CatalogModelUnknown
    ));

    r.grant_worker_model(sol.clone()).unwrap();

    r.validate_dispatch(&sol).unwrap();
    let grant_id = r.policy().authorize(&sol).unwrap();
    assert_eq!(
        grant_id,
        Some("session-user-grant-openai-codex/gpt-5.6-sol")
    );
    assert!(r.policy().effective_choices().contains(&sol));
    let granted_events = r
        .telemetry()
        .iter()
        .filter(|event| event.name == "worker.model_granted")
        .count();
    assert_eq!(granted_events, 1);
}

/// A mid-session grant for a same-provider model joins the allowlist directly
/// (no cross-provider grant id) and is honored by the next dispatch.
#[test]
fn mid_session_user_grant_admits_same_provider_worker() {
    let sonnet = model("anthropic/claude-sonnet-5");
    let policy = DelegationPolicy::enforced(
        model("anthropic/claude-fable-5"),
        [model("anthropic/claude-fable-5")],
        2,
        4,
    );
    let mut r = WorkerRegistry::new(policy);
    assert!(r.validate_dispatch(&sonnet).is_err());

    r.grant_worker_model(sonnet.clone()).unwrap();

    r.validate_dispatch(&sonnet).unwrap();
    assert_eq!(r.policy().authorize(&sonnet).unwrap(), None);
    assert!(r.policy().effective_choices().contains(&sonnet));
}

/// A fresh user grant must win over a stale expiring grant that names the
/// same identity: the session grant is evaluated first and never expires.
#[test]
fn mid_session_user_grant_wins_over_expired_pinned_grant() {
    let sol = model("openai-codex/gpt-5.6-sol");
    let foreground = model("anthropic/claude-fable-5");
    let catalog = CatalogSnapshot::new([foreground.clone(), sol.clone()]);
    let expired =
        CrossProviderGrant::new("pinned-grant", "anthropic", "openai-codex", [sol.clone()])
            .unwrap()
            .expiring_at(1)
            .unwrap();
    let mut policy =
        DelegationPolicy::with_grants(foreground.clone(), catalog, [foreground], [expired], 2, 4)
            .unwrap();
    assert_eq!(
        policy.authorize_at(&sol, 2).unwrap_err().typed_code(),
        DispatchFailureCode::CrossProviderGrantExpired
    );

    policy.grant_worker_model(sol.clone()).unwrap();

    assert_eq!(
        policy.authorize_at(&sol, 2).unwrap(),
        Some("session-user-grant-openai-codex/gpt-5.6-sol")
    );
}
