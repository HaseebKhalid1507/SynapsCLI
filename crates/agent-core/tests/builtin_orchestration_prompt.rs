//! Builtin orchestration prompt adapters — provider-selected doctrine that
//! Synaps composes into the default system prompt in source, without any
//! external manifest. Selection is typed (exact provider atom equality via
//! `PromptSelectors`); substring or family inference must never match.

use agent_core::prompt::{
    builtin_orchestration_adapters, compose_orchestration_prompt, AdapterRegistry,
    ModuleMutability, PromptModuleSource, QualifiedModelId, SelectionContext, WorkflowMode,
};

fn ctx(model: &str) -> SelectionContext {
    SelectionContext::new(QualifiedModelId::parse(model).unwrap(), None).unwrap()
}

#[test]
fn codex_models_compose_base_plus_supervision_doctrine() {
    for model in [
        "openai-codex/gpt-5.6-sol",
        "openai-codex/gpt-5.5",
        "openai-codex/gpt-5.4-mini",
    ] {
        let composed = compose_orchestration_prompt(Some("BASE."), &ctx(model))
            .unwrap_or_else(|| panic!("{model}: doctrine must compose"));
        assert!(composed.starts_with("BASE."), "{model}: base must lead");
        assert!(
            composed.contains("## Subagent supervision"),
            "{model}: doctrine heading missing"
        );
        assert!(composed.contains("subagent_status"), "{model}");
        assert!(composed.contains("subagent_collect"), "{model}");
        assert!(composed.contains("subagent_steer"), "{model}");
        assert!(
            composed.contains("every started handle reports a terminal status"),
            "{model}: status loop termination rule missing"
        );
        assert!(
            composed.contains("exactly once with reconciled=true"),
            "{model}: single-call terminal collection/reconciliation rule missing"
        );
        assert!(
            composed.contains("Never call subagent_collect without reconciled=true"),
            "{model}: unreconciled-collect prohibition missing"
        );
        assert!(
            composed.contains("NEVER end your turn"),
            "{model}: turn discipline missing"
        );
        assert!(
            composed.contains("sleep 240"),
            "{model}: 4-minute cadence missing"
        );
    }
}

#[test]
fn non_codex_models_pass_base_through_byte_identical() {
    for model in [
        "anthropic/claude-fable-5",
        "xai-auth/grok-4.5-latest",
        "google-gemini/gemini-2.5-pro",
        "openrouter/z-ai/glm-5.1",
    ] {
        assert_eq!(
            compose_orchestration_prompt(Some("BASE."), &ctx(model)).as_deref(),
            Some("BASE."),
            "{model}: base must pass through unchanged"
        );
    }
}

#[test]
fn provider_matching_is_typed_not_substring() {
    // `openai-codex` embedded as a *path segment* under another provider
    // must not match — the provider atom is `openrouter`.
    assert_eq!(
        compose_orchestration_prompt(Some("B"), &ctx("openrouter/openai-codex/gpt-5.5")).as_deref(),
        Some("B")
    );
    // Near-miss provider atoms must not match.
    assert_eq!(
        compose_orchestration_prompt(Some("B"), &ctx("openai-codexish/gpt-5.5")).as_deref(),
        Some("B")
    );
    assert_eq!(
        compose_orchestration_prompt(Some("B"), &ctx("openai/gpt-5.5")).as_deref(),
        Some("B")
    );
}

#[test]
fn no_base_composes_doctrine_alone_for_codex_and_none_otherwise() {
    let codex = compose_orchestration_prompt(None, &ctx("openai-codex/gpt-5.5"))
        .expect("codex doctrine must compose without a base");
    assert!(codex.starts_with("## Subagent supervision"));
    assert_eq!(
        compose_orchestration_prompt(None, &ctx("xai-auth/grok-4.5-latest")),
        None
    );
}

#[test]
fn builtin_adapters_are_builtin_provider_selected_guidance() {
    let adapters = builtin_orchestration_adapters();
    assert_eq!(adapters.len(), 2, "exactly the two known builtin adapters");

    for module in &adapters {
        assert!(matches!(module.source, PromptModuleSource::Builtin));
        assert!(matches!(
            module.mutability,
            ModuleMutability::MutableGuidance
        ));
        assert!(!module.content().is_empty());
    }

    let codex = adapters
        .iter()
        .find(|module| module.id.as_str() == "builtin.codex.subagent-supervision")
        .expect("Codex builtin must exist");
    assert!(
        codex.content().len() <= 2048,
        "doctrine must stay bounded; every codex request carries it"
    );

    let anthropic = adapters
        .iter()
        .find(|module| module.id.as_str() == "builtin.anthropic.ultracode-workflow")
        .expect("Anthropic UltraCode builtin must exist");

    let registry =
        AdapterRegistry::new(adapters.clone()).expect("known builtins must form a registry");
    let codex_context = ctx("openai-codex/gpt-5.5");
    let selected = registry.select(&codex_context).unwrap();
    assert_eq!(selected.len(), 1, "Codex selector must not overlap");
    assert_eq!(selected[0].id, codex.id);

    let anthropic_context =
        ctx("anthropic/claude-fable-5").with_workflow_mode(Some(WorkflowMode::UltraCode));
    let selected = registry.select(&anthropic_context).unwrap();
    assert_eq!(selected.len(), 1, "Anthropic selector must not overlap");
    assert_eq!(selected[0].id, anthropic.id);

    for context in [
        ctx("anthropic/claude-fable-5"),
        ctx("anthropic/claude-fable-5").with_workflow_mode(Some(WorkflowMode::Max)),
        ctx("anthropic/claude-sonnet-4-5").with_workflow_mode(Some(WorkflowMode::UltraCode)),
        ctx("openrouter/anthropic/claude-fable-5")
            .with_workflow_mode(Some(WorkflowMode::UltraCode)),
    ] {
        assert!(
            registry.select(&context).unwrap().is_empty(),
            "Anthropic builtin requires exact provider, model, and UltraCode mode"
        );
    }
}
