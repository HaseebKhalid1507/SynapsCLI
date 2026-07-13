use agent_core::prompt::{
    resolve_system_prompt_module, AdapterRegistry, ModuleMutability, PromptManifest, PromptModule,
    PromptModuleId, PromptModuleSource, PromptSelectors, PromptStack, QualifiedModelId,
    PROMPT_SCHEMA,
};

fn module(id: &str, priority: u16, selectors: PromptSelectors, content: &str) -> PromptModule {
    PromptModule::new(
        PromptModuleId::parse(id).unwrap(),
        "1.0.0",
        PromptModuleSource::Builtin,
        priority,
        selectors,
        ModuleMutability::MutableGuidance,
        content,
    )
}

#[test]
fn parses_v1_manifest_and_validates_references() {
    let manifest = PromptManifest::parse(r#"{"schema":"synaps-prompt/1","kernel":"kernel.foreman","adapters":["adapter.openrouter"]}"#).unwrap();
    assert_eq!(manifest.schema(), PROMPT_SCHEMA);
    assert!(manifest
        .validate_references(["kernel.foreman", "adapter.openrouter"])
        .is_ok());
    assert!(manifest.validate_references(["kernel.foreman"]).is_err());
    assert!(
        PromptManifest::parse(r#"{"schema":"synaps-prompt/2","kernel":"kernel.foreman"}"#).is_err()
    );
}

#[test]
fn qualified_model_splits_only_first_slash() {
    let id = QualifiedModelId::parse("openrouter/moonshotai/kimi-k2.7-code").unwrap();
    assert_eq!(
        (id.provider(), id.model()),
        ("openrouter", "moonshotai/kimi-k2.7-code")
    );
    assert!(QualifiedModelId::parse("unqualified").is_err());
}

#[test]
fn exact_selection_is_deterministic_and_ambiguity_fails_closed() {
    let target = QualifiedModelId::parse("openrouter/moonshotai/kimi-k2.7-code").unwrap();
    let provider = module(
        "adapter.provider",
        1,
        PromptSelectors::provider("openrouter"),
        "provider",
    );
    let family = module(
        "adapter.family",
        2,
        PromptSelectors::family("moonshotai"),
        "family",
    );
    let exact = module(
        "adapter.exact",
        3,
        PromptSelectors::exact(target.clone()),
        "exact",
    );
    let registry = AdapterRegistry::new(vec![exact.clone(), family, provider]);
    let selected = registry.select(&target).unwrap();
    assert_eq!(
        selected.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["adapter.provider", "adapter.family", "adapter.exact"]
    );
    let duplicate = module(
        "adapter.other",
        3,
        PromptSelectors::exact(target.clone()),
        "other",
    );
    assert!(AdapterRegistry::new(vec![exact, duplicate])
        .select(&target)
        .is_err());
}

#[test]
fn exact_byte_digest_and_secret_safe_inspection() {
    let a = module(
        "kernel.foreman",
        0,
        PromptSelectors::default(),
        "secret prompt\n",
    );
    let b = module(
        "kernel.foreman",
        0,
        PromptSelectors::default(),
        "secret prompt",
    );
    assert_eq!(
        a.sha256,
        "a2243551849ee2a446ccf4bee9848a846d35b741d27fb28c6b9c31c736fdb26c"
    );
    assert_ne!(a.sha256, b.sha256);
    let json = serde_json::to_string(&PromptStack::new(vec![a]).inspect()).unwrap();
    assert!(!json.contains("secret prompt"));
    assert!(json.contains("kernel.foreman"));
}

#[test]
fn legacy_prompt_is_unchanged_final_user_module() {
    let raw = "keep these exact bytes\n";
    let user = resolve_system_prompt_module(raw);
    assert_eq!(user.content, raw);
    let stack = PromptStack::new(vec![
        module("kernel.foreman", 0, PromptSelectors::default(), "kernel"),
        user,
    ]);
    assert_eq!(stack.modules().last().unwrap().id.as_str(), "user.system");
}
