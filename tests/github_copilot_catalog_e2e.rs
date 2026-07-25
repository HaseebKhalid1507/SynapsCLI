//! Zero-network GitHub Copilot model-catalog e2e (C2 catalog only).
//!
//! Proves:
//! - curated fallback wire IDs are fixture-established (not guessed)
//! - experimental `/models` parser keeps chat IDs and drops embeddings/completion
//! - broker allowlists `github-copilot` for `/models` only
//! - malformed / oversized bodies fail closed
//!
//! No network. No inference routing.

use synaps_cli::auth::broker::{ProxyMethod, ProxyRequest};
use synaps_cli::runtime::openai::catalog::{
    copilot_model, copilot_static_catalog_models, parse_copilot_catalog_models,
    validate_models_endpoint, COPILOT_FALLBACK_MODELS, MAX_MODELS_BODY_BYTES, MODELS_URL,
};

const FIXTURE: &str = include_str!(
    "../crates/agent-engine/src/runtime/openai/catalog/fixtures/github_copilot_models.json"
);

#[test]
fn curated_fallback_ids_are_established_by_fixture() {
    let models = parse_copilot_catalog_models(FIXTURE).expect("fixture");
    let live: std::collections::HashSet<_> = models.iter().map(|m| m.id.as_str()).collect();
    for d in COPILOT_FALLBACK_MODELS {
        assert!(
            live.contains(d.id),
            "fallback id {} must appear in live fixture",
            d.id
        );
        assert!(copilot_model(d.id).is_some());
    }
    // Unobserved / retired must not be seeded.
    assert!(copilot_model("gpt-5.6-sol").is_none());
    assert!(copilot_model("auto").is_none());
    assert!(copilot_model("gpt-4.1").is_none());
}

#[test]
fn parser_filters_non_chat_and_prefixes_runtime_ids() {
    let models = parse_copilot_catalog_models(FIXTURE).expect("fixture");
    assert!(models
        .iter()
        .all(|m| m.runtime_id().starts_with("github-copilot/")));
    assert!(!models.iter().any(|m| m.id == "text-embedding-3-small"));
    assert!(!models.iter().any(|m| m.id == "gpt-41-copilot"));
    assert!(models.iter().any(|m| m.id == "gpt-5.3-codex"));
    assert!(models.iter().any(|m| m.id == "claude-sonnet-4.6"));
    assert!(models.iter().any(|m| m.id == "gemini-3.1-pro-preview"));
}

#[test]
fn parser_fails_closed_on_malformed_and_oversized() {
    assert!(parse_copilot_catalog_models("{nope").is_err());
    assert!(parse_copilot_catalog_models(r#"{"models":[]}"#).is_err());
    let huge = format!("{{\"data\":[]}}{}", "x".repeat(MAX_MODELS_BODY_BYTES));
    assert!(parse_copilot_catalog_models(&huge).is_err());
}

#[test]
fn models_endpoint_pin_and_static_catalog_shape() {
    validate_models_endpoint(MODELS_URL).unwrap();
    assert!(validate_models_endpoint("https://api.github.com/models").is_err());
    let static_models = copilot_static_catalog_models();
    assert_eq!(static_models.len(), COPILOT_FALLBACK_MODELS.len());
    assert!(static_models
        .iter()
        .all(|m| m.runtime_id().starts_with("github-copilot/")));
}

#[test]
fn broker_allowlists_only_reviewed_github_copilot_runtime_paths() {
    let ok = ProxyRequest {
        provider: "github-copilot".into(),
        method: ProxyMethod::Get,
        path: "/models".into(),
        body: None,
        stream: false,
        body_bytes: None,
    };
    assert!(ok.validate().is_ok());

    for path in ["/chat/completions", "/responses"] {
        let allowed = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: path.into(),
            body: Some(serde_json::json!({"model":"fixture","stream":true})),
            stream: true,
            body_bytes: None,
        };
        assert!(
            allowed.validate().is_ok(),
            "{path} is a reviewed inference path"
        );
    }
    for path in ["/v1/messages", "/models/gpt-5.4/policy", "/models?all=true"] {
        let denied = ProxyRequest {
            provider: "github-copilot".into(),
            method: ProxyMethod::Post,
            path: path.into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(denied.validate().is_err(), "{path} must remain deny-listed");
    }

    // Other OAuth providers remain non-proxyable.
    for provider in ["anthropic", "openai-codex", "claude"] {
        let req = ProxyRequest {
            provider: provider.into(),
            method: ProxyMethod::Get,
            path: "/models".into(),
            body: None,
            stream: false,
            body_bytes: None,
        };
        assert!(req.validate().is_err(), "{provider} must not be proxyable");
    }
}
