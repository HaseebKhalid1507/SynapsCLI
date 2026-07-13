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
        ModelFamilyId::parse(family).unwrap(),
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
    path: prompts/extra.md
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
    assert!(PromptManifest::parse(
        "schema: synaps-prompt/1\nkernel: k\npolicies: { delegation: { mode: enforced } }\n"
    )
    .is_err());
    assert!(PromptManifest::parse("schema: synaps-prompt/1\nkernel: k\nmodules: [{id: x, version: v, source: user, priority: 1, selectors: {}, mutability: mutable_guidance, content: x, bogus: y}]\n").is_err());
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
            PromptSelectors::family(ctx.family().clone()),
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
    let json = serde_json::to_string(&stack.inspect()).unwrap();
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
