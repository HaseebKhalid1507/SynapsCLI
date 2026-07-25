use agent_core::prompt::*;
use agent_core::session::Session;

#[test]
fn session_provenance_is_deterministic_and_old_sessions_default() {
    let module = PromptModule::new(
        PromptModuleId::parse("kernel").unwrap(),
        "1.0",
        PromptModuleSource::Builtin,
        0,
        PromptSelectors::default(),
        ModuleMutability::ImmutablePolicy,
        "safe",
    )
    .unwrap();
    let stack = PromptStack::new(
        vec![module],
        SelectionContext::new(QualifiedModelId::parse("anthropic/sonnet").unwrap(), None).unwrap(),
    )
    .unwrap();
    let provenance = stack.provenance("policy-digest");
    assert_eq!(provenance.prompt_schema, PROMPT_SCHEMA);
    assert_eq!(provenance.foreground_model, "anthropic/sonnet");
    assert_eq!(provenance.prompt_stack[0].id, "kernel");
    let mut session = Session::new("anthropic/sonnet", "off", None);
    session.prompt_provenance = Some(provenance);
    let value = serde_json::to_value(&session).unwrap();
    assert_eq!(
        value["prompt_provenance"]["delegation_policy_digest"],
        "policy-digest"
    );
    let mut old = value;
    old.as_object_mut().unwrap().remove("prompt_provenance");
    assert!(serde_json::from_value::<Session>(old)
        .unwrap()
        .prompt_provenance
        .is_none());
}
