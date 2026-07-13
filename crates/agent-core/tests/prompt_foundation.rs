use agent_core::prompt::*;

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
    .unwrap()
}
fn context(model: &str, family: &str) -> SelectionContext {
    SelectionContext::new(
        QualifiedModelId::parse(model).unwrap(),
        Some(ModelFamilyId::parse(family).unwrap()),
    )
    .unwrap()
}

#[test]
fn parses_real_yaml_and_rejects_unknown_policy_and_fields() {
    let yaml = r#"
schema: synaps-prompt/1
kernel: kernel.foreman
adapters:
  - adapter.openrouter
modules:
  - id: user.extra
    version: 1.2.0
    source: user
    priority: 9
    selectors: { provider: openrouter }
    mutability: mutable_guidance
    content: "exact bytes\n"
"#;
    let m = PromptManifest::parse(yaml).unwrap();
    assert_eq!(m.schema(), PROMPT_SCHEMA);
    assert!(m
        .validate_references(["kernel.foreman", "adapter.openrouter"])
        .is_ok());
    assert!(PromptManifest::parse(&(yaml.to_owned() + "surprise: true\n")).is_err());
    let policy = PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies:\n  delegation:\n    mode: enforced\n    allowed_models: [anthropic/claude-haiku]\n    max_concurrent_workers: 2\n    max_total_workers: 4\n"
    )
    .unwrap()
    .delegation_policy(QualifiedModelId::parse("anthropic/claude-sonnet").unwrap())
    .unwrap()
    .unwrap();
    assert_eq!(policy.max_concurrent_workers, 2);
    assert_eq!(policy.max_total_workers, 4);
    assert!(PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies: { delegation: { mode: enforced, allowed_models: [], max_concurrent_workers: 0, max_total_workers: 1 } }\n"
    ).is_err());
    assert!(PromptManifest::parse("schema: synaps-prompt/1\nkernel: k\nmodules: [{id: x, version: v, source: user, priority: 1, selectors: {}, mutability: mutable_guidance, content: x, bogus: y}]\n").is_err());
}

#[test]
fn manifest_compiles_exact_cross_provider_grants_and_rejects_ambiguous_shapes() {
    let manifest = PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies:\n  delegation:\n    enforcement: enforced\n    same_provider_models: [openai-codex/gpt-5.6-sol]\n    cross_provider_grants:\n      - id: review-01\n        from_provider: openai-codex\n        to_provider: anthropic\n        allowed_models: [anthropic/claude-opus-4-7]\n    max_concurrent_workers: 2\n    max_total_workers: 4\n",
    )
    .unwrap();
    let policy = manifest
        .delegation_policy(QualifiedModelId::parse("openai-codex/gpt-5.6-sol").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(policy.effective_choices().len(), 2);
    assert!(policy
        .authorize(&QualifiedModelId::parse("anthropic/claude-opus-4-7").unwrap())
        .is_ok());

    assert!(PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies: {delegation: {enforcement: enforced, same_provider_models: [openai-codex/gpt], cross_provider_grants: [{id: bad, from_provider: openai-codex, to_provider: anthropic, allowed_models: [openai/gpt]}], max_concurrent_workers: 1, max_total_workers: 1}}\n"
    )
    .unwrap()
    .delegation_policy(QualifiedModelId::parse("openai-codex/gpt").unwrap())
    .is_err());
}

#[test]
fn malformed_oversize_and_duplicate_manifest_modules_fail() {
    assert!(PromptManifest::parse("not: [yaml").is_err());
    let huge = format!(
        "schema: synaps-prompt/1\nkernel: k\n#{}",
        "x".repeat(MAX_MANIFEST_BYTES)
    );
    assert!(PromptManifest::parse(&huge).is_err());
    let duplicate = "schema: synaps-prompt/1\nkernel: k\nmodules:\n- {id: x, version: v, source: user, priority: 1, selectors: {}, mutability: mutable_guidance, content: a}\n- {id: x, version: v, source: user, priority: 2, selectors: {}, mutability: mutable_guidance, content: b}\n";
    assert!(PromptManifest::parse(duplicate).is_err());
    assert!(PromptModule::new(
        PromptModuleId::parse("x").unwrap(),
        "v",
        PromptModuleSource::User,
        0,
        PromptSelectors::default(),
        ModuleMutability::MutableGuidance,
        "x".repeat(MAX_MODULE_BYTES + 1)
    )
    .is_err());
}

#[test]
fn qualified_ids_are_strict_and_openrouter_nesting_is_valid() {
    for bad in [
        "x",
        "/x",
        "x/",
        "provider//model",
        " provider/model",
        "provider/model\n",
        "provider/mo del",
    ] {
        assert!(QualifiedModelId::parse(bad).is_err(), "{bad:?}");
    }
    let id = QualifiedModelId::parse("openrouter/moonshotai/kimi-k2.7-code").unwrap();
    assert_eq!(
        (id.provider(), id.model()),
        ("openrouter", "moonshotai/kimi-k2.7-code")
    );
}

#[test]
fn explicit_family_exact_matching_and_consistency() {
    let ctx = context("openrouter/moonshotai/kimi", "kimi");
    let modules = vec![
        module(
            "provider",
            0,
            PromptSelectors::provider("openrouter").unwrap(),
            "p",
        ),
        module(
            "family",
            0,
            PromptSelectors::family(ModelFamilyId::parse("kimi").unwrap()),
            "f",
        ),
        module(
            "substring",
            1,
            PromptSelectors::provider("open").unwrap(),
            "no",
        ),
        module(
            "wrong-family",
            1,
            PromptSelectors::family(ModelFamilyId::parse("moonshotai").unwrap()),
            "no",
        ),
    ];
    assert_eq!(
        AdapterRegistry::new(modules)
            .unwrap()
            .select(&ctx)
            .unwrap()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["provider", "family"]
    );
    assert!(PromptSelectors::provider_and_exact(
        "anthropic",
        QualifiedModelId::parse("openrouter/x/y").unwrap()
    )
    .is_err());
}

#[test]
fn compilation_is_permutation_stable_layered_and_ambiguity_fails() {
    let ctx = context("openrouter/moonshotai/kimi", "kimi");
    let base = vec![
        module("exact", 7, PromptSelectors::exact(ctx.model().clone()), "e"),
        module("kernel", 50, PromptSelectors::default(), "k"),
        module(
            "provider",
            2,
            PromptSelectors::provider("openrouter").unwrap(),
            "p",
        ),
        module(
            "family",
            9,
            PromptSelectors::family(ctx.family().unwrap().clone()),
            "f",
        ),
    ];
    let manifest = PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: kernel\nadapters: [provider, family, exact]\n",
    )
    .unwrap();
    let mut reversed = base.clone();
    reversed.reverse();
    let ids = |r: AdapterRegistry| {
        compile_prompt_stack(&manifest, &r, &ctx, None)
            .unwrap()
            .modules()
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(AdapterRegistry::new(base).unwrap()),
        ["kernel", "provider", "family", "exact"]
    );
    assert_eq!(
        ids(AdapterRegistry::new(reversed).unwrap()),
        ["kernel", "provider", "family", "exact"]
    );
    let ambiguous = AdapterRegistry::new(vec![
        module(
            "a",
            1,
            PromptSelectors::provider("openrouter").unwrap(),
            "a",
        ),
        module(
            "b",
            1,
            PromptSelectors::provider("openrouter").unwrap(),
            "b",
        ),
    ])
    .unwrap();
    assert!(ambiguous.select(&ctx).is_err());
}

#[test]
fn inspection_and_debug_never_expose_content() {
    let canary = "CANARY-super-secret";
    let m = module("kernel", 0, PromptSelectors::default(), canary);
    assert_eq!(
        m.sha256,
        "05a741260e581671b8e7a035e194c14f6f44e3568043376f19b7a3314d710400"
    );
    let stack = PromptStack::new(vec![m], context("openrouter/x/y", "x")).unwrap();
    let json =
        serde_json::to_string(&stack.inspect(agent_core::orchestration::EnforcementMode::Advisory))
            .unwrap();
    let debug = format!("{stack:?} {:?}", stack.modules());
    assert!(!json.contains(canary));
    assert!(!debug.contains(canary));
    assert!(
        json.contains("synaps-prompt/1")
            && json.contains("openrouter/x/y")
            && json.contains("byte_count")
    );
}

#[test]
fn resolved_legacy_output_is_exact_and_user_last() {
    let raw = "keep exact bytes\n";
    let user = resolved_system_prompt_as_user_module(raw).unwrap();
    assert_eq!(user.content(), raw);
    let manifest = PromptManifest::parse("schema: synaps-prompt/1\nkernel: kernel\n").unwrap();
    let registry =
        AdapterRegistry::new(vec![module("kernel", 0, PromptSelectors::default(), "k")]).unwrap();
    let stack = compile_prompt_stack(
        &manifest,
        &registry,
        &context("openrouter/x/y", "x"),
        Some(user),
    )
    .unwrap();
    assert_eq!(
        stack.modules().last().unwrap().content().as_bytes(),
        raw.as_bytes()
    );
    assert_eq!(stack.composed().as_bytes(), b"k\nkeep exact bytes\n");
}

#[test]
fn declared_modules_are_materialized_and_requested_mismatch_fails() {
    let manifest = PromptManifest::parse("schema: synaps-prompt/1\nkernel: kernel\nadapters: [wrong]\nmodules:\n- {id: kernel, version: v, source: user, priority: 0, selectors: {}, mutability: immutable_policy, content: exact-kernel}\n- {id: wrong, version: v, source: user, priority: 1, selectors: {provider: anthropic}, mutability: mutable_guidance, content: wrong}\n").unwrap();
    let registry = manifest.registry(None).unwrap();
    let ctx = SelectionContext::new(QualifiedModelId::parse("openai/gpt").unwrap(), None).unwrap();
    let error = compile_prompt_stack(&manifest, &registry, &ctx, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("wrong") && error.contains("does not match"),
        "{error}"
    );
}

#[test]
fn family_selector_does_not_match_when_context_has_no_family() {
    let ctx = SelectionContext::new(
        QualifiedModelId::parse("openai/gpt-family-name").unwrap(),
        None,
    )
    .unwrap();
    let registry = AdapterRegistry::new(vec![module(
        "family",
        1,
        PromptSelectors::family(ModelFamilyId::parse("gpt-family-name").unwrap()),
        "no",
    )])
    .unwrap();
    assert!(registry.select(&ctx).unwrap().is_empty());
}

#[test]
fn inspection_reports_every_effective_enforcement_mode() {
    let stack = PromptStack::new(
        vec![module("kernel", 0, PromptSelectors::default(), "k")],
        context("openrouter/x/y", "x"),
    )
    .unwrap();
    for (mode, expected) in [
        (agent_core::orchestration::EnforcementMode::Off, "off"),
        (
            agent_core::orchestration::EnforcementMode::Advisory,
            "advisory",
        ),
        (
            agent_core::orchestration::EnforcementMode::Enforced,
            "enforced",
        ),
    ] {
        let value = serde_json::to_value(stack.inspect(mode)).unwrap();
        assert_eq!(value["enforcement_state"], expected);
    }
}
