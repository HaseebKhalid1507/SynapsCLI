# TDD Behavioral Test Scenarios — Dynamic Model Catalog Tasks 1–7

**Spec:** `docs/specs/2026-04-27-model-routing-dynamic-catalog.md`
**Mode:** Informed (existing code structure referenced)
**Author:** Research sub-agent pass
**Date:** 2026-04-27

---

## How to read this document

Each task section has:
- **Red phase** — the failing test that should be written *before* the implementation exists.
  The test must compile (types declared, stubs return `todo!()`) but must fail at runtime.
- **Green phase** — implementation notes reminding the developer what minimal code makes the test pass.
- **Refactor guard** — tests that must stay green through subsequent tasks.

All tests use `#[cfg(test)]` modules adjacent to the code being tested.
No live network calls anywhere — parsers are pure functions fed fixture JSON.
Secrets must never appear in fixtures or assertions.

---

## Task 1 — Normalized Catalog Contract

**Target module:** `src/runtime/openai/catalog.rs` (new file)
**Cargo test target:** `cargo test runtime::openai::catalog`

### T1-01 — CatalogModel rejects an empty model id

**Intent:** Validate that the canonical `empty id → filter` rule is enforced at the type/constructor level, not left to each parser.

**Location:** `src/runtime/openai/catalog.rs` `#[cfg(test)] mod tests`

```rust
#[test]
fn catalog_model_rejects_empty_id() {
    // CatalogModel::new / CatalogModel::try_new or a catalog-level filter
    // must return Err / None for an id that is blank or whitespace-only.
    assert!(CatalogModel::try_new("", None, None, ReasoningSupport::None, CatalogSource::Fallback).is_none());
    assert!(CatalogModel::try_new("   ", None, None, ReasoningSupport::None, CatalogSource::Fallback).is_none());
}
```

**Fails until:** `CatalogModel::try_new` is defined and returns `None` for empty/whitespace ids.

---

### T1-02 — CatalogModel stores trimmed, non-empty id

**Intent:** Normal construction succeeds; id is preserved verbatim (not trimmed by default — the input already passed validation).

```rust
#[test]
fn catalog_model_stores_valid_id() {
    let m = CatalogModel::try_new(
        "meta-llama/llama-3.3-70b-instruct",
        Some("Llama 3.3 70B"),
        Some(128_000),
        ReasoningSupport::None,
        CatalogSource::Live,
    ).expect("valid id");
    assert_eq!(m.id, "meta-llama/llama-3.3-70b-instruct");
    assert_eq!(m.label.as_deref(), Some("Llama 3.3 70B"));
    assert_eq!(m.context_tokens, Some(128_000));
    assert_eq!(m.reasoning, ReasoningSupport::None);
    assert!(matches!(m.source, CatalogSource::Live));
}
```

---

### T1-03 — All ReasoningSupport variants are PartialEq + Clone

**Intent:** Downstream sort, dedup, and UI-badge code require value equality without mutation.

```rust
#[test]
fn reasoning_support_variants_are_eq_and_clone() {
    let a = ReasoningSupport::AnthropicBudget { adaptive: true };
    let b = a.clone();
    assert_eq!(a, b);

    let c = ReasoningSupport::EffortLevels {
        levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
    };
    let d = c.clone();
    assert_eq!(c, d);
    assert_ne!(a, c);

    let e = ReasoningSupport::OpenRouterReasoning { max_tokens: Some(4096), effort: true };
    assert_ne!(e, ReasoningSupport::None);
    assert_ne!(e, ReasoningSupport::Unknown);
}
```

---

### T1-04 — CatalogSource::Fallback and Live are distinguishable

**Intent:** UI fallback indicators and stale-badge logic need to tell the sources apart.

```rust
#[test]
fn catalog_source_distinguishes_live_from_fallback() {
    assert_ne!(CatalogSource::Live, CatalogSource::Fallback);
    assert_eq!(CatalogSource::Fallback, CatalogSource::Fallback);
}
```

---

### T1-05 — Static provider seeds convert to CatalogModel without information loss

**Intent:** Existing `registry::ProviderSpec.models` tuples `(id, label, tier)` must be representable as `CatalogModel` via a conversion helper so fallback seeds can be served through the new trait without re-implementing the static list.

**Location:** `src/runtime/openai/catalog.rs`

```rust
#[test]
fn provider_spec_seed_converts_to_catalog_model() {
    // registry::providers() is available; groq is entry 0 with known models.
    let spec = synaps_cli::runtime::openai::registry::providers()
        .iter()
        .find(|s| s.key == "groq")
        .expect("groq spec");

    let seeds: Vec<CatalogModel> = spec.models
        .iter()
        .filter_map(|(id, label, _tier)| {
            CatalogModel::try_new(
                id,
                Some(label),
                None,                        // no context metadata in static list
                ReasoningSupport::Unknown,   // groq reasoning TBD in Task 4
                CatalogSource::Fallback,
            )
        })
        .collect();

    assert!(!seeds.is_empty());
    assert!(seeds.iter().all(|m| !m.id.is_empty()));
    // Every seed has a label carried from the static tuple
    assert!(seeds.iter().all(|m| m.label.is_some()));
}
```

---

### T1-06 — filter_empty_ids helper drops blank but keeps valid entries

**Intent:** Parser-level guard used by all provider handlers.

```rust
#[test]
fn filter_empty_ids_keeps_valid_removes_blank() {
    let raw = vec![
        ("valid-id", Some("Label A")),
        ("", Some("No-id model")),
        ("   ", None),
        ("another-valid", None),
    ];
    let filtered: Vec<CatalogModel> = raw
        .into_iter()
        .filter_map(|(id, label)| {
            CatalogModel::try_new(id, label, None, ReasoningSupport::None, CatalogSource::Live)
        })
        .collect();
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].id, "valid-id");
    assert_eq!(filtered[1].id, "another-valid");
}
```

---

## Task 2 — OpenRouter Rich Catalog Handler

**Target module:** `src/runtime/openai/catalog.rs` (openrouter submodule / parser fn)
**Cargo test target:** `cargo test runtime::openai::catalog::openrouter`

### T2-01 — OpenRouter parser extracts id, name, and context_length

**Intent:** Baseline structural parsing.
**Fixture source:** OpenRouter `/api/v1/models` schema (Research Appendix).

```rust
#[test]
fn openrouter_parser_extracts_basic_fields() {
    let json = r#"{
        "data": [
            {
                "id": "qwen/qwen3-coder",
                "name": "Qwen: Qwen3 Coder",
                "context_length": 262144,
                "architecture": { "input_modalities": ["text"] },
                "supported_parameters": [],
                "pricing": { "prompt": "0.0000002", "completion": "0.0000008" }
            }
        ]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "qwen/qwen3-coder");
    assert_eq!(models[0].label.as_deref(), Some("Qwen: Qwen3 Coder"));
    assert_eq!(models[0].context_tokens, Some(262_144));
    assert!(matches!(models[0].source, CatalogSource::Live));
}
```

---

### T2-02 — OpenRouter parser maps `reasoning` + `include_reasoning` to OpenRouterReasoning

**Intent:** Core reasoning-support inference for models that expose the reasoning parameter.

```rust
#[test]
fn openrouter_parser_detects_reasoning_include_reasoning() {
    let json = r#"{
        "data": [{
            "id": "deepseek/deepseek-r1",
            "name": "DeepSeek: R1",
            "context_length": 65536,
            "supported_parameters": ["reasoning", "include_reasoning"],
            "pricing": { "prompt": "0", "completion": "0", "internal_reasoning": "0.0000014" }
        }]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    match &models[0].reasoning {
        ReasoningSupport::OpenRouterReasoning { effort, .. } => {
            // include_reasoning present but no reasoning_effort → effort=false
            assert!(!effort, "effort flag should be false when reasoning_effort not in supported_parameters");
        }
        other => panic!("expected OpenRouterReasoning, got {other:?}"),
    }
}
```

---

### T2-03 — OpenRouter parser maps `reasoning_effort` to effort=true

```rust
#[test]
fn openrouter_parser_detects_reasoning_effort_flag() {
    let json = r#"{
        "data": [{
            "id": "openai/gpt-oss-120b",
            "name": "OpenAI: GPT-OSS 120B",
            "context_length": 131072,
            "supported_parameters": ["reasoning", "reasoning_effort", "include_reasoning"],
            "pricing": { "prompt": "0", "completion": "0" }
        }]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    match &models[0].reasoning {
        ReasoningSupport::OpenRouterReasoning { effort, .. } => {
            assert!(effort, "effort should be true when reasoning_effort in supported_parameters");
        }
        other => panic!("expected OpenRouterReasoning, got {other:?}"),
    }
}
```

---

### T2-04 — OpenRouter parser maps `verbosity` to AnthropicBudget path

**Intent:** OpenRouter routes some Anthropic models using `verbosity`; normalize to the Anthropic variant to avoid duplicating reasoning UI logic.

```rust
#[test]
fn openrouter_parser_maps_verbosity_to_anthropic_budget() {
    let json = r#"{
        "data": [{
            "id": "anthropic/claude-opus-4-7",
            "name": "Anthropic: Claude Opus 4.7",
            "context_length": 200000,
            "supported_parameters": ["verbosity"],
            "pricing": { "prompt": "0", "completion": "0" }
        }]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    assert!(
        matches!(models[0].reasoning, ReasoningSupport::AnthropicBudget { .. }),
        "verbosity in supported_parameters should map to AnthropicBudget"
    );
}
```

---

### T2-05 — OpenRouter parser produces ReasoningSupport::None for models with no reasoning params

```rust
#[test]
fn openrouter_parser_none_reasoning_for_vanilla_model() {
    let json = r#"{
        "data": [{
            "id": "google/gemma-3-27b-it",
            "name": "Google: Gemma 3 27B",
            "context_length": 32768,
            "supported_parameters": ["temperature", "top_p"],
            "pricing": { "prompt": "0", "completion": "0" }
        }]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    assert_eq!(models[0].reasoning, ReasoningSupport::None);
}
```

---

### T2-06 — OpenRouter parser silently drops models with blank id

```rust
#[test]
fn openrouter_parser_drops_blank_id_models() {
    let json = r#"{
        "data": [
            { "id": "", "name": "Ghost", "context_length": 4096, "supported_parameters": [], "pricing": {"prompt":"0","completion":"0"} },
            { "id": "valid/model-x", "name": "Valid", "context_length": 4096, "supported_parameters": [], "pricing": {"prompt":"0","completion":"0"} }
        ]
    }"#;
    let models = parse_openrouter_models(json).expect("parse");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "valid/model-x");
}
```

---

### T2-07 — OpenRouter parser tolerates missing optional fields (pricing, context_length)

**Intent:** Schema drift resilience — provider may omit fields.

```rust
#[test]
fn openrouter_parser_tolerates_missing_optional_fields() {
    let json = r#"{
        "data": [{
            "id": "mystery/new-model",
            "supported_parameters": []
        }]
    }"#;
    let models = parse_openrouter_models(json).expect("should not error on missing optional fields");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context_tokens, None);
    assert_eq!(models[0].label, None);
}
```

---

### T2-08 — OpenRouter handler does not require auth header for fetch

**Intent:** Spec states auth is not required for the model list. The handler's `fetch_models` implementation must not panic or error when the API key map is empty for OpenRouter.

```rust
#[test]
fn openrouter_handler_omits_auth_for_model_list_endpoint() {
    // We can't make network calls, but we can verify that the *request builder*
    // produced by the handler doesn't carry an Authorization header when the
    // openrouter key is absent.
    let handler = OpenRouterCatalogHandler;
    let keys: BTreeMap<String, String> = BTreeMap::new();
    // build_request is a sync helper that returns a reqwest::RequestBuilder
    // without executing the call. Inspect its header set.
    let client = reqwest::Client::new();
    let req = handler.build_list_request(&client, &keys)
        .build()
        .expect("request builds without key");
    assert!(
        req.headers().get("Authorization").is_none(),
        "OpenRouter model list must not send Authorization header"
    );
}
```

---

### T2-09 — OpenRouter parser returns CatalogError on malformed JSON

```rust
#[test]
fn openrouter_parser_errors_on_malformed_json() {
    let result = parse_openrouter_models("{ not json }");
    assert!(result.is_err());
}
```

---

## Task 3 — Generic OpenAI-Compatible Handler Refactor

**Target modules:** `src/runtime/openai/registry.rs`, `src/runtime/openai/catalog.rs`
**Cargo test targets:** `cargo test runtime::openai::registry`, `cargo test runtime::openai::catalog`

### T3-01 — Existing registry parse test still passes (refactor guard)

**Intent:** The green test at line 434 of `registry.rs` (`parses_openrouter_models_response`) must continue to pass unchanged. If the generic handler replaces `parse_provider_models_response`, the old function must either remain as a compatibility shim or the test must be updated to call the equivalent new function *and produce identical results*.

```rust
// ── This test already exists in registry.rs; assert it still passes ──
// No new code required here. If it fails after Task 3 changes, that is a
// regression. Document its location: registry::model_list_tests
```

---

### T3-02 — Generic handler parses flat `{id}` list into CatalogModel with Unknown reasoning

**Intent:** When a provider's `/models` response has only `id` and no metadata, the generic handler must produce `CatalogModel` entries with `reasoning = ReasoningSupport::Unknown` and `context_tokens = None`.

```rust
#[test]
fn generic_handler_parses_minimal_models_list_to_unknown_reasoning() {
    let json = r#"{ "data": [{"id": "my-custom-model"}, {"id": "another-model"}] }"#;
    let models = parse_generic_provider_models(json).expect("parse");
    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|m| m.reasoning == ReasoningSupport::Unknown));
    assert!(models.iter().all(|m| m.context_tokens.is_none()));
    assert!(models.iter().all(|m| matches!(m.source, CatalogSource::Live)));
}
```

---

### T3-03 — Generic handler strips blank-id entries (mirrors existing filter)

**Intent:** The existing `!item.id.trim().is_empty()` filter in `parse_provider_models_response` must be replicated in the generic catalog handler path.

```rust
#[test]
fn generic_handler_filters_blank_ids() {
    let json = r#"{ "data": [{"id": ""}, {"id": "valid-model"}, {"id": "  "}] }"#;
    let models = parse_generic_provider_models(json).expect("parse");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "valid-model");
}
```

---

### T3-04 — resolve_shorthand routing remains unchanged after refactor

**Intent:** `registry::resolve_shorthand` is consumed by `openai::resolve_route`; both must continue to produce identical outputs before and after Task 3.

```rust
#[test]
fn resolve_shorthand_groq_model_produces_correct_base_url() {
    use std::collections::BTreeMap;
    let keys = BTreeMap::from([("groq".to_string(), "sk-test".to_string())]);
    let cfg = synaps_cli::runtime::openai::registry::resolve_shorthand(
        "groq/llama-3.3-70b-versatile",
        &keys,
    ).expect("should resolve");
    assert!(cfg.base_url.contains("api.groq.com"));
    assert_eq!(cfg.model, "llama-3.3-70b-versatile");
    assert_eq!(cfg.provider, "groq");
}
```

---

### T3-05 — Generic handler exposes API key for providers that require it

**Intent:** Auth-bearing providers (Groq, NVIDIA, etc.) must still pass bearer tokens on the `/models` request. The handler must resolve key from config override → env var.

```rust
#[test]
fn generic_handler_attaches_bearer_token_when_key_present() {
    // We verify via the request builder (no network).
    let handler = GenericOpenAiCatalogHandler { provider_key: "groq" };
    let keys = BTreeMap::from([("groq".to_string(), "gsk_test_token".to_string())]);
    let client = reqwest::Client::new();
    let req = handler.build_list_request(&client, &keys)
        .build()
        .expect("request builds");
    let auth = req.headers()
        .get("Authorization")
        .expect("Authorization header present")
        .to_str()
        .unwrap();
    assert!(auth.starts_with("Bearer "), "must be Bearer scheme");
    // Token value must not be the literal string in the assertion —
    // just confirm the header exists and has the Bearer prefix.
    assert!(auth.len() > "Bearer ".len(), "token must not be empty");
}
```

---

### T3-06 — fetch_provider_models compatibility shim returns CatalogModel slice

**Intent:** `chatui/models` calls `fetch_provider_models` today. After Task 3 it may remain as a shim wrapping the generic handler. The return type must be source-compatible — if signature changes, all callers compile. This test verifies the shim output type, not the network call.

```rust
// If `fetch_provider_models` return type is changed, this function-signature
// test must be written to confirm it still returns something convertible to
// `Vec<ExpandedModelEntry>` via the new converter.
#[test]
fn catalog_model_converts_to_expanded_model_entry() {
    let m = CatalogModel::try_new(
        "groq/llama-3.3-70b-versatile",
        Some("Llama 3.3 70B Versatile"),
        Some(131_072),
        ReasoningSupport::None,
        CatalogSource::Live,
    ).unwrap();
    let entry: ExpandedModelEntry = m.into_expanded_entry(/* is_favorite= */ false);
    assert_eq!(entry.id, "groq/llama-3.3-70b-versatile");
    assert_eq!(entry.label, "Llama 3.3 70B Versatile");
    assert!(!entry.is_favorite);
}
```

---

## Task 4 — Groq and NVIDIA Capability Enrichment

**Target module:** `src/runtime/openai/catalog.rs` (groq / nvidia submodules)
**Cargo test targets:** `cargo test runtime::openai::catalog::groq`, `cargo test runtime::openai::catalog::nvidia`

### T4-01 — Groq parser reads context_window field

```rust
#[test]
fn groq_parser_extracts_context_window() {
    let json = r#"{
        "object": "list",
        "data": [{
            "id": "llama-3.3-70b-versatile",
            "object": "model",
            "created": 1234567890,
            "owned_by": "Meta",
            "active": true,
            "context_window": 131072,
            "public_apps": null
        }]
    }"#;
    let models = parse_groq_models(json).expect("parse");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context_tokens, Some(131_072));
}
```

---

### T4-02 — Groq parser filters inactive models

**Intent:** Groq API marks decommissioned models `active: false`; they must be excluded.

```rust
#[test]
fn groq_parser_excludes_inactive_models() {
    let json = r#"{
        "data": [
            { "id": "llama-3.3-70b-versatile", "active": true,  "context_window": 131072, "owned_by": "Meta" },
            { "id": "old-model-v1",             "active": false, "context_window": 8192,   "owned_by": "Meta" }
        ]
    }"#;
    let models = parse_groq_models(json).expect("parse");
    assert_eq!(models.len(), 1, "inactive model must be excluded");
    assert_eq!(models[0].id, "llama-3.3-70b-versatile");
}
```

---

### T4-03 — Groq capability rules map known reasoning families

**Intent:** Groq reasoning metadata is NOT in the API response; it must be inferred from model-id families. Test the static inference function directly.

```rust
#[test]
fn groq_capability_inference_maps_qwen3_family_to_effort_levels() {
    // qwen/qwen3-* family supports reasoning_effort per Groq docs
    let support = infer_groq_reasoning("qwen/qwen3-32b");
    match support {
        ReasoningSupport::EffortLevels { ref levels } => {
            assert!(levels.contains(&ReasoningEffort::Low));
            assert!(levels.contains(&ReasoningEffort::High));
        }
        other => panic!("expected EffortLevels for qwen3 family, got {other:?}"),
    }
}

#[test]
fn groq_capability_inference_maps_gpt_oss_family_to_effort_levels() {
    let support = infer_groq_reasoning("openai/gpt-oss-120b");
    assert!(matches!(support, ReasoningSupport::EffortLevels { .. }));
}

#[test]
fn groq_capability_inference_returns_none_for_vanilla_llama() {
    // Standard llama models on Groq do not support reasoning params
    let support = infer_groq_reasoning("meta-llama/llama-4-maverick-17b-128e-instruct");
    assert_eq!(support, ReasoningSupport::None);
}
```

---

### T4-04 — Groq `reasoning_format` and `include_reasoning` are mutually exclusive guard

**Intent:** The spec states these are mutually exclusive. The request-guard helper should enforce this before a request is built.

```rust
#[test]
fn groq_request_guard_rejects_both_reasoning_format_and_include_reasoning() {
    let result = validate_groq_reasoning_params(
        Some(GroqReasoningFormat::Hidden),
        /* include_reasoning= */ true,
    );
    assert!(result.is_err(), "mutually exclusive params must be rejected");
}

#[test]
fn groq_request_guard_accepts_reasoning_format_alone() {
    let result = validate_groq_reasoning_params(
        Some(GroqReasoningFormat::Parsed),
        /* include_reasoning= */ false,
    );
    assert!(result.is_ok());
}
```

---

### T4-05 — NVIDIA parser dedupes duplicate ids

**Intent:** The spec and live API testing show NIM sometimes returns duplicate model entries.

```rust
#[test]
fn nvidia_parser_dedupes_duplicate_ids() {
    let json = r#"{
        "data": [
            { "id": "meta/llama-3.3-70b-instruct", "object": "model", "owned_by": "Meta" },
            { "id": "meta/llama-3.3-70b-instruct", "object": "model", "owned_by": "Meta" },
            { "id": "nvidia/llama-3.1-nemotron-ultra-253b-v1", "object": "model", "owned_by": "NVIDIA" }
        ]
    }"#;
    let models = parse_nvidia_models(json).expect("parse");
    assert_eq!(models.len(), 2, "duplicate id must be deduplicated");
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"meta/llama-3.3-70b-instruct"));
    assert!(ids.contains(&"nvidia/llama-3.1-nemotron-ultra-253b-v1"));
}
```

---

### T4-06 — NVIDIA heuristic capability table marks Nemotron Ultra as system-prompt thinking

**Intent:** Nemotron Ultra/Super expose thinking via system prompt injection, not API params. The static heuristic must annotate these.

```rust
#[test]
fn nvidia_heuristic_marks_nemotron_ultra_as_system_prompt_thinking() {
    let support = infer_nvidia_reasoning("nvidia/llama-3.1-nemotron-ultra-253b-v1");
    match support {
        ReasoningSupport::SystemPromptControlled { ref hint } => {
            // hint should reference `detailed thinking on` or similar
            assert!(!hint.is_empty());
        }
        other => panic!("expected SystemPromptControlled for Nemotron Ultra, got {other:?}"),
    }
}
```

> **Note:** This requires a `SystemPromptControlled { hint: String }` variant to be added to `ReasoningSupport` if it isn't in the initial design. Add it in Task 4 if needed; Task 1's `Unknown` variant is the fallback until then.

---

### T4-07 — NVIDIA parser ignores non-chat models

**Intent:** NIM exposes embedding and other non-chat models. Only chat-capable models should appear in the catalog.

```rust
#[test]
fn nvidia_parser_excludes_embedding_models() {
    let json = r#"{
        "data": [
            { "id": "nvidia/nv-embed-v1", "object": "model", "owned_by": "NVIDIA" },
            { "id": "meta/llama-3.3-70b-instruct", "object": "model", "owned_by": "Meta" }
        ]
    }"#;
    // parse_nvidia_models should apply the heuristic static exclusion list
    let models = parse_nvidia_models(json).expect("parse");
    assert!(
        !models.iter().any(|m| m.id.contains("embed")),
        "embedding models must be excluded"
    );
}
```

---

## Task 5 — Anthropic Live Catalog and Codex Static Handler

**Target module:** `src/runtime/openai/catalog.rs` (anthropic / codex submodules)
**Cargo test targets:** `cargo test runtime::openai::catalog::anthropic`, `cargo test core::auth` (if auth paths change)

### T5-01 — Anthropic parser extracts paginated first-page models

**Intent:** Anthropic list endpoint is paginated. The parser should handle one page; the fetch orchestrator handles pagination.

```rust
#[test]
fn anthropic_parser_extracts_models_from_paginated_page() {
    let json = r#"{
        "data": [
            {
                "id": "claude-opus-4-7",
                "display_name": "Claude Opus 4.7",
                "created_at": "2025-01-01T00:00:00Z",
                "type": "model"
            },
            {
                "id": "claude-sonnet-4-6",
                "display_name": "Claude Sonnet 4.6",
                "created_at": "2024-11-01T00:00:00Z",
                "type": "model"
            }
        ],
        "has_more": true,
        "first_id": "claude-opus-4-7",
        "last_id": "claude-sonnet-4-6"
    }"#;
    let (models, has_more) = parse_anthropic_models_page(json).expect("parse");
    assert_eq!(models.len(), 2);
    assert!(has_more);
    assert_eq!(models[0].id, "claude-opus-4-7");
    assert_eq!(models[0].label.as_deref(), Some("Claude Opus 4.7"));
}
```

---

### T5-02 — Anthropic parser reads optional capabilities.thinking field

**Intent:** Newer Anthropic model objects may expose `capabilities.thinking = true`; this maps to `ReasoningSupport::AnthropicBudget`.

```rust
#[test]
fn anthropic_parser_maps_capabilities_thinking_to_anthropic_budget() {
    let json = r#"{
        "data": [{
            "id": "claude-opus-4-7",
            "display_name": "Claude Opus 4.7",
            "type": "model",
            "capabilities": {
                "thinking": true,
                "effort": true,
                "context_management": true
            }
        }],
        "has_more": false
    }"#;
    let (models, _) = parse_anthropic_models_page(json).expect("parse");
    match &models[0].reasoning {
        ReasoningSupport::AnthropicBudget { adaptive } => {
            // effort: true in capabilities means adaptive is supported
            assert!(adaptive);
        }
        other => panic!("expected AnthropicBudget, got {other:?}"),
    }
}
```

---

### T5-03 — Anthropic parser falls back to heuristic when capabilities field is absent

**Intent:** Most models in the live API response don't have `capabilities` yet. The existing `model_supports_adaptive_thinking` heuristic must be used as fallback.

```rust
#[test]
fn anthropic_parser_falls_back_to_heuristic_when_no_capabilities() {
    let json = r#"{
        "data": [{
            "id": "claude-opus-4-7",
            "display_name": "Claude Opus 4.7",
            "type": "model"
        }],
        "has_more": false
    }"#;
    let (models, _) = parse_anthropic_models_page(json).expect("parse");
    // claude-opus-4-7 triggers adaptive thinking heuristic → AnthropicBudget { adaptive: true }
    assert!(
        matches!(models[0].reasoning, ReasoningSupport::AnthropicBudget { adaptive: true }),
        "heuristic fallback must produce adaptive=true for opus-4-7"
    );
}

#[test]
fn anthropic_parser_falls_back_to_non_adaptive_heuristic_for_opus_4_6() {
    let json = r#"{
        "data": [{"id": "claude-opus-4-6", "display_name": "Claude Opus 4.6", "type": "model"}],
        "has_more": false
    }"#;
    let (models, _) = parse_anthropic_models_page(json).expect("parse");
    assert!(
        matches!(models[0].reasoning, ReasoningSupport::AnthropicBudget { adaptive: false }),
        "opus-4-6 must map to non-adaptive budget path"
    );
}
```

---

### T5-04 — Anthropic handler does not log or expose API key value

**Intent:** Security contract — keys must never appear in log strings, error messages, or test output.

```rust
#[test]
fn anthropic_handler_auth_header_debug_does_not_expose_key() {
    // ProviderConfig already redacts api_key in Debug; the anthropic handler
    // must use that config's pattern and never format the key into a String.
    let cfg = synaps_cli::runtime::openai::types::ProviderConfig {
        base_url: "https://api.anthropic.com".into(),
        api_key: "sk-ant-secret-key".into(),
        model: "claude-opus-4-7".into(),
        provider: "claude".into(),
    };
    let debug_str = format!("{cfg:?}");
    assert!(
        !debug_str.contains("sk-ant-secret-key"),
        "ProviderConfig Debug must redact api_key; got: {debug_str}"
    );
}
```

---

### T5-05 — Anthropic handler includes required anthropic-version header

**Intent:** All Anthropic API calls require `anthropic-version: 2023-06-01`.

```rust
#[test]
fn anthropic_handler_adds_version_header_to_list_request() {
    let handler = AnthropicCatalogHandler;
    let keys = BTreeMap::from([("claude".to_string(), "sk-test".to_string())]);
    let client = reqwest::Client::new();
    let req = handler.build_list_request(&client, &keys)
        .build()
        .expect("builds");
    let version_header = req.headers()
        .get("anthropic-version")
        .expect("anthropic-version header")
        .to_str()
        .unwrap();
    assert_eq!(version_header, "2023-06-01");
}
```

---

### T5-06 — Codex static handler marks source as Fallback and has no live endpoint

**Intent:** Codex ChatGPT backend has no documented model-list endpoint. The handler must signal `CatalogSource::Fallback` and `fetch_models` must return early with the static list rather than attempt a network call.

```rust
#[test]
fn codex_handler_marks_all_models_as_fallback_source() {
    let handler = CodexStaticCatalogHandler;
    let fallback = handler.fallback_models();
    assert!(!fallback.is_empty(), "Codex must have at least one static fallback model");
    assert!(
        fallback.iter().all(|m| matches!(m.source, CatalogSource::Fallback)),
        "all Codex models must be CatalogSource::Fallback"
    );
}

#[test]
fn codex_handler_static_ids_match_existing_dev_model_selections() {
    // Must carry gpt-5.5 and gpt-5.1-codex-mini, matching existing chatui::models::DEV_MODEL_SELECTIONS
    let handler = CodexStaticCatalogHandler;
    let ids: Vec<_> = handler.fallback_models().into_iter().map(|m| m.id).collect();
    assert!(ids.iter().any(|id| id.contains("gpt-5.5")));
    assert!(ids.iter().any(|id| id.contains("codex-mini")));
}
```

---

### T5-07 — Codex models carry encrypted reasoning marker (not displayable)

**Intent:** Codex `reasoning.encrypted_content` is opaque. Mark reasoning as `ReasoningSupport::Unknown` and never try to display thinking blocks.

```rust
#[test]
fn codex_handler_marks_reasoning_unknown() {
    let handler = CodexStaticCatalogHandler;
    for model in handler.fallback_models() {
        assert_eq!(
            model.reasoning,
            ReasoningSupport::Unknown,
            "Codex model {} must have Unknown reasoning (encrypted content)", model.id
        );
    }
}
```

---

## Task 6 — Wire Expanded `/model` UI to Catalog Handlers

**Target modules:** `src/chatui/models/mod.rs`, `src/chatui/models/input.rs`
**Cargo test target:** `cargo test chatui::models`

### T6-01 — set_expanded_models accepts CatalogModel-derived entries and stores them

**Intent:** The existing `set_expanded_models` function today accepts `Vec<ExpandedModelEntry>`. After Task 6 it must accept catalog-backed entries with richer metadata. The `ExpandedModelEntry` type must gain optional metadata fields without breaking existing callers.

```rust
#[test]
fn set_expanded_models_stores_ready_state_with_context_badge() {
    let mut state = make_test_state_with_expanded("openrouter");
    let entries = vec![
        ExpandedModelEntry {
            id: "openrouter/qwen/qwen3-coder".into(),
            label: "Qwen3 Coder".into(),
            is_favorite: false,
            context_tokens: Some(262_144),           // NEW optional field
            reasoning_badge: Some("reasoning".into()), // NEW optional field
        }
    ];
    set_expanded_models(&mut state, "openrouter", Ok(entries));
    match &state.expanded.as_ref().unwrap().load_state {
        ExpandedLoadState::Ready(models) => {
            assert_eq!(models[0].context_tokens, Some(262_144));
            assert_eq!(models[0].reasoning_badge.as_deref(), Some("reasoning"));
        }
        _ => panic!("expected Ready state"),
    }
}
```

---

### T6-02 — set_expanded_models ignores result if provider key has changed

**Intent:** Race condition guard — if the user navigates away while a fetch is in-flight, the stale result must be discarded (existing behavior in `set_expanded_models`).

```rust
#[test]
fn set_expanded_models_ignores_stale_provider_key() {
    let mut state = make_test_state_with_expanded("openrouter");
    // Navigate to a different provider before result arrives
    state.expanded.as_mut().unwrap().provider_key = "groq".into();

    set_expanded_models(&mut state, "openrouter", Ok(vec![]));
    // Load state should remain Loading, not be updated to Ready
    assert!(matches!(
        state.expanded.as_ref().unwrap().load_state,
        ExpandedLoadState::Loading
    ));
}
```

---

### T6-03 — set_expanded_models transitions to Error state on fetch failure

```rust
#[test]
fn set_expanded_models_transitions_to_error_on_failure() {
    let mut state = make_test_state_with_expanded("groq");
    set_expanded_models(&mut state, "groq", Err("groq is not configured".into()));
    assert!(matches!(
        state.expanded.as_ref().unwrap().load_state,
        ExpandedLoadState::Error(_)
    ));
}
```

---

### T6-04 — expanded_visible_models respects fuzzy search over richer fields

**Intent:** After Task 6, `expanded_visible_models` must apply fuzzy scoring to both `id` and `label` (existing behavior), ensuring richer label text still searches correctly.

```rust
#[test]
fn expanded_visible_models_fuzzy_searches_rich_labels() {
    let mut state = make_test_state_with_expanded("openrouter");
    state.expanded.as_mut().unwrap().search = "qwen coder".into();
    state.expanded.as_mut().unwrap().load_state = ExpandedLoadState::Ready(vec![
        ExpandedModelEntry {
            id: "openrouter/qwen/qwen3-coder".into(),
            label: "Qwen3 Coder — 262K context · reasoning".into(),
            is_favorite: false,
            context_tokens: Some(262_144),
            reasoning_badge: None,
        },
        ExpandedModelEntry {
            id: "openrouter/google/gemma-3-27b-it".into(),
            label: "Gemma 3 27B".into(),
            is_favorite: false,
            context_tokens: Some(32_768),
            reasoning_badge: None,
        },
    ]);
    let visible = expanded_visible_models(&state);
    assert_eq!(visible.len(), 1);
    assert!(visible[0].id.contains("qwen3-coder"));
}
```

---

### T6-05 — selected_expanded_model returns correct entry after search filter

**Intent:** `cursor` is an index into the *visible* list; after filtering, cursor=0 should map to the first visible model, not the first raw model.

```rust
#[test]
fn selected_expanded_model_indexes_into_filtered_visible_list() {
    let mut state = make_test_state_with_expanded("openrouter");
    state.expanded.as_mut().unwrap().search = "gemma".into();
    state.expanded.as_mut().unwrap().cursor = 0;
    state.expanded.as_mut().unwrap().load_state = ExpandedLoadState::Ready(vec![
        ExpandedModelEntry::simple("openrouter/qwen/qwen3-coder", "Qwen3 Coder"),
        ExpandedModelEntry::simple("openrouter/google/gemma-3-27b-it", "Gemma 3 27B"),
    ]);
    let selected = selected_expanded_model(&state).expect("should have selection");
    assert_eq!(selected.id, "openrouter/google/gemma-3-27b-it");
}
```

---

### T6-06 — Unconfigured provider in expanded mode returns clear error (no panic)

**Intent:** If a provider is selected for expansion but has no key, `set_expanded_models` must store `Error`, not panic.

```rust
#[test]
fn expanded_mode_unconfigured_provider_stores_error_not_panics() {
    let mut state = make_test_state_with_expanded("groq");
    // Simulate the async fetch returning an error
    let err_msg = "Groq is not configured — add provider.groq to ~/.synaps-cli/config";
    set_expanded_models(&mut state, "groq", Err(err_msg.into()));
    match &state.expanded.as_ref().unwrap().load_state {
        ExpandedLoadState::Error(msg) => {
            assert!(msg.contains("Groq is not configured"));
        }
        _ => panic!("expected Error state"),
    }
}
```

---

### T6-07 — Selecting expanded entry writes `provider/model` format (catalog-backed)

**Intent:** The existing `model_id_for_runtime` path must not be broken by richer entries. A catalog-backed `ExpandedModelEntry` with a pre-qualified `id` (e.g. `"openrouter/qwen/qwen3-coder"`) must be returned verbatim.

```rust
#[test]
fn catalog_backed_expanded_entry_id_is_verbatim_provider_model() {
    // The ExpandedModelEntry id produced by the catalog handler is already
    // fully qualified; model_id_for_runtime must not strip/alter it.
    let id = "openrouter/qwen/qwen3-coder";
    let result = model_id_for_runtime(id);
    assert_eq!(result, "openrouter/qwen/qwen3-coder");
}
```

---

### T6-08 — Existing chatui model section tests remain green (refactor guards)

**Intent:** The six existing tests in `chatui::models::tests` must not regress:
- `claude_favorite_ids_round_trip_to_runtime_ids`
- `favorites_view_only_keeps_favorite_entries`
- `unconfigured_openai_providers_are_hidden`
- `fuzzy_model_matches_are_case_insensitive_subsequences`
- `local_provider_is_hidden_when_no_local_models_are_configured`
- `local_provider_uses_explicit_local_models_config`
- `dev_selected_models_populate_for_logged_in_providers`

```
// No new code; these must compile and pass after Task 6 changes.
// If ExpandedModelEntry gains new fields, ensure existing construction sites
// use struct update syntax or defaults.
```

---

## Task 7 — Provider-Aware Thinking Request Adapter

**Target modules:** `src/runtime/api.rs`, `src/core/models.rs`, `src/runtime/openai/catalog.rs`
**Cargo test targets:** `cargo test runtime::api`, `cargo test runtime::openai`, `cargo test core::models`

### T7-01 — Anthropic adaptive-thinking request body shape is unchanged

**Intent:** Existing behavior for `claude-opus-4-7` must not change byte shape. This test documents the current contract as a regression anchor.

```rust
#[test]
fn anthropic_adapter_opus_4_7_produces_adaptive_thinking_body() {
    let adapter = build_thinking_adapter("claude-opus-4-7", /* budget= */ 0);
    // budget=0 is the "adaptive" sentinel
    let params = adapter.request_params();
    assert_eq!(params.thinking_type, "adaptive");
    assert!(params.budget_tokens.is_none(), "adaptive shape must not include budget_tokens");
}
```

---

### T7-02 — Anthropic legacy path produces enabled+budget_tokens for opus-4-6

```rust
#[test]
fn anthropic_adapter_opus_4_6_produces_legacy_budget_body() {
    let adapter = build_thinking_adapter("claude-opus-4-6", /* budget= */ 4096);
    let params = adapter.request_params();
    assert_eq!(params.thinking_type, "enabled");
    assert_eq!(params.budget_tokens, Some(4096));
}
```

---

### T7-03 — Anthropic adaptive-path outputs effort via output_config, not budget_tokens

```rust
#[test]
fn anthropic_adapter_adaptive_model_emits_output_config_effort() {
    let adapter = build_thinking_adapter("claude-opus-4-7", /* budget (xhigh)= */ 32768);
    let params = adapter.request_params();
    assert_eq!(params.effort, Some("xhigh"));
    assert!(params.budget_tokens.is_none());
}

#[test]
fn anthropic_adapter_adaptive_level_omits_effort() {
    // Sentinel 0 = "adaptive" = model decides = no effort field
    let adapter = build_thinking_adapter("claude-opus-4-7", /* budget= */ 0);
    let params = adapter.request_params();
    assert!(params.effort.is_none(), "adaptive level must omit effort entirely");
}
```

---

### T7-04 — OpenRouter adapter emits reasoning object only for reasoning-capable models

```rust
#[test]
fn openrouter_adapter_emits_reasoning_for_capable_model() {
    let support = ReasoningSupport::OpenRouterReasoning { max_tokens: None, effort: true };
    let adapter = build_openrouter_thinking_adapter(&support, "medium");
    let params = adapter.request_params();
    assert!(params.reasoning.is_some(), "reasoning param must be set for capable model");
}

#[test]
fn openrouter_adapter_omits_reasoning_for_non_reasoning_model() {
    let support = ReasoningSupport::None;
    let adapter = build_openrouter_thinking_adapter(&support, "medium");
    let params = adapter.request_params();
    assert!(params.reasoning.is_none(), "reasoning param must be absent for ReasoningSupport::None");
}
```

---

### T7-05 — Groq adapter emits reasoning_format or reasoning_effort but not both

```rust
#[test]
fn groq_adapter_does_not_emit_both_reasoning_format_and_include_reasoning() {
    let support = ReasoningSupport::EffortLevels {
        levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
    };
    let adapter = build_groq_thinking_adapter(&support, "high");
    let params = adapter.request_params();
    let has_format = params.reasoning_format.is_some();
    let has_include = params.include_reasoning.is_some();
    assert!(
        !(has_format && has_include),
        "reasoning_format and include_reasoning are mutually exclusive in Groq"
    );
}
```

---

### T7-06 — NVIDIA adapter uses system prompt injection, not API reasoning params

```rust
#[test]
fn nvidia_adapter_injects_system_prompt_for_nemotron_ultra() {
    let support = ReasoningSupport::SystemPromptControlled {
        hint: "detailed thinking on".into(),
    };
    let adapter = build_nvidia_thinking_adapter(&support);
    let params = adapter.request_params();
    // Must have no reasoning API field
    assert!(params.reasoning.is_none());
    assert!(params.reasoning_format.is_none());
    // Must return a system-prompt injection string
    assert!(
        params.system_prompt_injection.as_deref().map(|s| s.contains("detailed thinking")).unwrap_or(false),
        "NVIDIA Nemotron must get system prompt injection, not API param"
    );
}
```

---

### T7-07 — Generic OpenAI-compatible adapter emits no reasoning params

**Intent:** For providers without a known reasoning type (`Unknown`), the adapter must never add reasoning fields to the request body.

```rust
#[test]
fn generic_adapter_emits_no_reasoning_params_for_unknown_support() {
    let support = ReasoningSupport::Unknown;
    let adapter = build_generic_thinking_adapter(&support);
    let params = adapter.request_params();
    assert!(params.reasoning.is_none());
    assert!(params.reasoning_format.is_none());
    assert!(params.include_reasoning.is_none());
    assert!(params.budget_tokens.is_none());
}
```

---

### T7-08 — core::models delegation: model_supports_adaptive_thinking delegates to catalog when available

**Intent:** After Task 7, `model_supports_adaptive_thinking` may be a compatibility shim. Its current behavior must be preserved for all models in the existing test suite — this test is a regression anchor for the delegation contract.

```rust
#[test]
fn core_models_adaptive_thinking_heuristic_preserves_existing_behavior_post_delegation() {
    // These assertions mirror the existing tests in core::models::tests exactly.
    // If the function becomes a thin wrapper, it must produce identical results.
    assert!(crate::core::models::model_supports_adaptive_thinking("claude-opus-4-7"));
    assert!(!crate::core::models::model_supports_adaptive_thinking("claude-opus-4-6"));
    assert!(!crate::core::models::model_supports_adaptive_thinking("claude-haiku-4-5-20251001"));
    assert!(crate::core::models::model_supports_adaptive_thinking("claude-opus-5-0"));
}
```

---

### T7-09 — effort_for_thinking_level delegation produces stable outputs

```rust
#[test]
fn core_models_effort_for_thinking_level_stable_after_delegation() {
    use crate::core::models::effort_for_thinking_level;
    assert_eq!(effort_for_thinking_level("low"), Some("low"));
    assert_eq!(effort_for_thinking_level("medium"), Some("medium"));
    assert_eq!(effort_for_thinking_level("high"), Some("high"));
    assert_eq!(effort_for_thinking_level("xhigh"), Some("xhigh"));
    assert_eq!(effort_for_thinking_level("adaptive"), None);
    // Unknown level falls back to "high"
    assert_eq!(effort_for_thinking_level("turbo-ultra"), Some("high"));
}
```

---

## Cross-Task Invariants (must pass at every checkpoint)

These tests should exist from Task 1 onwards and must never be broken:

| Test | Location | What it guards |
|------|----------|----------------|
| `parses_openrouter_models_response` | `registry::model_list_tests` | Generic parser backward-compat |
| `resolve_shorthand_groq_model_produces_correct_base_url` | Task 3 / registry | Routing contract |
| `resolves_openai_codex_without_requiring_eager_credentials` | `openai::tests` | Codex routing |
| `claude_favorite_ids_round_trip_to_runtime_ids` | `chatui::models::tests` | Favorite ID serialization |
| `adaptive_thinking_required_for_opus_4_7_plus` | `core::models::tests` | Thinking decision |
| `core_models_adaptive_thinking_heuristic_preserves_existing_behavior_post_delegation` | Task 7 | Delegation safety |

---

## Test Helpers (shared across tasks)

```rust
// Suggested location: src/runtime/openai/catalog.rs  #[cfg(test)] fn helpers

fn make_test_state_with_expanded(provider_key: &str) -> ModelsModalState {
    ModelsModalState {
        cursor: 0,
        search: String::new(),
        view: ModelsView::All,
        collapsed: HashSet::new(),
        favorites: BTreeSet::new(),
        expanded: Some(ExpandedModelsState {
            provider_key: provider_key.to_string(),
            provider_name: provider_key.to_string(),
            cursor: 0,
            search: String::new(),
            load_state: ExpandedLoadState::Loading,
        }),
    }
}

impl ExpandedModelEntry {
    fn simple(id: &str, label: &str) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            is_favorite: false,
            context_tokens: None,
            reasoning_badge: None,
        }
    }
}
```

---

## Test Count Summary

| Task | Tests | Type |
|------|-------|------|
| 1 – Catalog contract | 6 | Unit / type-level |
| 2 – OpenRouter handler | 9 | Parser unit |
| 3 – Generic handler refactor | 6 | Parser unit + routing |
| 4 – Groq / NVIDIA enrichment | 7 | Parser unit + inference |
| 5 – Anthropic / Codex | 7 | Parser unit + auth shape |
| 6 – Chat UI catalog wire | 8 | State / filter unit |
| 7 – Request adapter | 9 | Adapter shape unit |
| **Total** | **52** | All unit, zero network |

---

## Implementation Order Reminder

Write tests in this order *before* writing any implementation:

1. **T1-01 → T1-06** (fail at compile until `catalog.rs` skeleton added)
2. **T2-01 → T2-09** (fail until `parse_openrouter_models` written)
3. **T3-01 → T3-06** (T3-01 must stay green; others fail until refactor)
4. **T4-01 → T4-07** (require Groq/NVIDIA parser fns and inference helpers)
5. **T5-01 → T5-07** (require Anthropic parser + Codex handler)
6. **T6-01 → T6-08** (T6-08 must stay green; T6-01..07 fail until UI wired)
7. **T7-01 → T7-09** (T7-08/T7-09 must stay green from T1; others fail until adapters written)
