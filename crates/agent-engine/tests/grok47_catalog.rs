//! Offline regression coverage for the exact brokered Grok 4.7 route.
use agent_core::reasoning::ReasoningLevel::*;
use agent_engine::runtime::openai::{
    catalog::{self, validation},
    resolve_route, AuthPolicy, WireProtocol,
};

const MODEL: &str = "xai-auth/grok-4.7";

#[test]
fn grok47_is_selectable_and_routes_through_xai_oauth_responses() {
    let row = catalog::xai_static_catalog_models()
        .into_iter()
        .find(|m| m.runtime_id() == MODEL)
        .expect("selectable static catalog row");
    assert_eq!(row.label.as_deref(), Some("Grok 4.7"));
    assert_eq!(row.context_tokens, Some(500_000));
    assert_eq!(row.max_output_tokens, None, "do not invent an output cap");
    let route = resolve_route(MODEL).expect("brokered route");
    assert_eq!(route.model, "grok-4.7");
    assert_eq!(route.provider, "xai-auth");
    assert_eq!(route.endpoint, "https://api.x.ai/v1");
    assert_eq!(
        route.auth,
        AuthPolicy::OAuthAccessToken(agent_core::auth::OAuthProviderId::Xai)
    );
    assert_eq!(route.wire, WireProtocol::OpenAiResponses);
}

#[test]
fn grok47_validation_and_picker_share_exact_reasoning_capability() {
    assert_eq!(validation::default_level_for_model(MODEL), Some(High));
    assert_eq!(validation::reasoning_type_for_model(MODEL), "effort");
    assert_eq!(
        validation::thinking_options_for_model(MODEL),
        vec!["adaptive", "low", "medium", "high", "xhigh"]
    );
    for level in [Adaptive, Low, Medium, High, XHigh] {
        assert!(
            validation::validate_reasoning_mutation(MODEL, level).is_ok(),
            "{level}"
        );
    }
    for level in [Off, Max, Ultra, UltraCode] {
        assert!(
            validation::validate_reasoning_mutation(MODEL, level).is_err(),
            "{level}"
        );
    }
}

#[test]
fn grok47_does_not_authorize_guessed_aliases_or_change_existing_models() {
    for id in ["grok-4.7-latest", "grok-4.70", "grok-4.7-preview"] {
        assert!(catalog::xai_model(id).is_none());
        assert!(resolve_route(&format!("xai-auth/{id}")).is_none());
        assert!(catalog::xai_static_capability(id).is_none());
    }
    assert!(resolve_route("xai-auth/grok-4.6").is_some());
    assert!(resolve_route("xai-auth/grok-4.5").is_some());
}
