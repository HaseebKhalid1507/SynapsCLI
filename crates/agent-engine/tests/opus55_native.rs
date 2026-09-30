//! Offline regression coverage for native Anthropic Claude Opus 5.5
//! (`anthropic/claude-opus-5-5`): an exact known native model with adaptive
//! effort thinking and native image/PDF input, but no Max/UltraCode authority.
use agent_core::reasoning::ReasoningLevel::*;
use agent_engine::runtime::openai::{
    catalog::{self, validation, ReasoningSupport},
    resolve_route, AuthPolicy, WireProtocol,
};

const MODEL: &str = "anthropic/claude-opus-5-5";
const WIRE_ID: &str = "claude-opus-5-5";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII=";

fn image_block() -> serde_json::Value {
    serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}})
}

#[test]
fn opus55_is_an_exact_known_native_model_on_the_anthropic_messages_wire() {
    assert!(agent_core::models::KNOWN_MODELS
        .iter()
        .any(|(id, label)| *id == WIRE_ID && label.starts_with("Opus 5.5")));
    let route = resolve_route(MODEL).expect("native Anthropic route");
    assert_eq!(route.provider, "anthropic");
    assert_eq!(route.model, WIRE_ID);
    assert_eq!(route.endpoint, "https://api.anthropic.com");
    assert_eq!(
        route.auth,
        AuthPolicy::OAuthAccessToken(agent_core::auth::OAuthProviderId::Anthropic)
    );
    assert_eq!(route.wire, WireProtocol::AnthropicMessages);
}

#[test]
fn opus55_read_tool_images_and_pdfs_pass_the_attachment_gate() {
    use base64::Engine as _;
    let pdf = serde_json::json!({"type":"document","title":"spec.pdf","source":{
        "type":"base64","media_type":"application/pdf",
        "data":base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7\n%%EOF\n")
    }});
    for model in [MODEL, WIRE_ID] {
        agent_engine::runtime::attachments::validate_tool_blocks(
            model,
            &[image_block(), pdf.clone()],
        )
        .unwrap_or_else(|e| panic!("{model}: {e}"));
    }
}

#[test]
fn opus55_uses_adaptive_effort_without_special_modes() {
    assert_eq!(
        catalog::anthropic_static_capability(WIRE_ID),
        Some(ReasoningSupport::AnthropicAdaptive { adaptive: true })
    );
    assert!(agent_core::models::model_supports_adaptive_thinking(
        WIRE_ID
    ));
    assert_eq!(validation::reasoning_type_for_model(MODEL), "adaptive");
    assert_eq!(
        validation::thinking_options_for_model(MODEL),
        vec!["off", "adaptive", "low", "medium", "high", "xhigh"]
    );
    for level in [Off, Adaptive, Low, Medium, High, XHigh] {
        assert!(
            validation::validate_reasoning_mutation(MODEL, level).is_ok(),
            "{level}"
        );
    }
    // Max/UltraCode stay locked to the evidence-backed mode manifest.
    assert!(catalog::anthropic_mode_capabilities(MODEL).is_none());
    for level in [Max, Ultra, UltraCode] {
        assert!(
            validation::validate_reasoning_mutation(MODEL, level).is_err(),
            "{level}"
        );
    }
}

#[test]
fn opus55_is_authorizable_as_an_exact_worker_model() {
    let model = agent_engine::orchestration::validate_user_authorizable_model(MODEL)
        .expect("exact known native Anthropic model");
    assert_eq!(model.as_str(), MODEL);
}

#[test]
fn opus55_does_not_authorize_guessed_aliases() {
    for id in [
        "claude-opus-5.5",
        "claude-opus-5-5-preview",
        "claude-opus-5-5-latest",
    ] {
        let qualified = format!("anthropic/{id}");
        assert!(catalog::anthropic_static_capability(id).is_none(), "{id}");
        assert!(
            agent_engine::runtime::attachments::validate_tool_blocks(&qualified, &[image_block()])
                .is_err(),
            "{id}"
        );
        assert!(
            agent_engine::orchestration::validate_user_authorizable_model(&qualified).is_err(),
            "{id}"
        );
    }
}
