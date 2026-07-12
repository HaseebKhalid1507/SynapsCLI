use agent_core::auth::azure_openai::*;
use agent_core::auth::AzureOpenAiConfig;

fn config() -> AzureOpenAiConfig {
    AzureOpenAiConfig::new(
        "organizations",
        "00000000-0000-0000-0000-000000000001",
        "rg-one",
        "aoai-one",
        "chat-prod",
    )
    .unwrap()
}

#[test]
fn registration_and_official_audiences_are_enforced() {
    let err = AzureRegistration::production(None).unwrap_err();
    assert_eq!(err.code(), "registration_required");
    assert!(err.to_string().contains("Microsoft Entra public-client"));
    let registration = AzureRegistration::test("00000000-0000-0000-0000-000000000099").unwrap();
    assert_eq!(
        registration.client_id(),
        "00000000-0000-0000-0000-000000000099"
    );
    assert_eq!(
        AzureAudience::Arm.scope(),
        "https://management.azure.com/.default"
    );
    assert_eq!(
        AzureAudience::Inference.scope(),
        "https://cognitiveservices.azure.com/.default"
    );
}

#[test]
fn tenant_device_code_requests_use_v2_public_client_contract() {
    let registration = AzureRegistration::test("00000000-0000-0000-0000-000000000099").unwrap();
    let request = device_code_request(&config(), &registration).unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.url,
        "https://login.microsoftonline.com/organizations/oauth2/v2.0/devicecode"
    );
    assert!(request
        .form
        .contains(&("scope".into(), AzureAudience::Arm.scope().into())));
    assert!(!format!("{request:?}").contains("secret"));
}

#[test]
fn polling_honors_pending_slowdown_denial_expiry_and_cancel() {
    let mut poll = DevicePoll::new(5, 100, 900);
    assert_eq!(
        poll.apply(100, false, PollReply::AuthorizationPending)
            .unwrap(),
        PollAction::Sleep(5)
    );
    assert_eq!(
        poll.apply(105, false, PollReply::SlowDown).unwrap(),
        PollAction::Sleep(10)
    );
    assert_eq!(
        poll.apply(115, true, PollReply::AuthorizationPending)
            .unwrap_err()
            .code(),
        "cancelled"
    );
    assert_eq!(
        DevicePoll::new(5, 0, 1)
            .apply(2, false, PollReply::AuthorizationPending)
            .unwrap_err()
            .code(),
        "device_code_expired"
    );
    assert_eq!(
        DevicePoll::new(5, 0, 99)
            .apply(1, false, PollReply::Denied)
            .unwrap_err()
            .code(),
        "access_denied"
    );
}

#[test]
fn refresh_is_audience_isolated_and_rotation_preserves_refresh_material() {
    let mut tokens = AzureTokenSet::new("refresh-one");
    tokens.commit(
        AzureAudience::Arm,
        TokenGrant::new("arm-access", None, 1000),
    );
    tokens.commit(
        AzureAudience::Inference,
        TokenGrant::new("infer-access", Some("refresh-two".into()), 1100),
    );
    assert_eq!(
        tokens.access(AzureAudience::Arm, 900).unwrap(),
        "arm-access"
    );
    assert_eq!(
        tokens.access(AzureAudience::Inference, 900).unwrap(),
        "infer-access"
    );
    assert_eq!(tokens.refresh_token(), "refresh-two");
    let json = serde_json::to_string(&tokens).unwrap();
    assert!(!json.contains("arm-access"));
    assert!(!json.contains("refresh-two"));
}

#[test]
fn arm_discovery_paginates_deployments_and_rejects_host_substitution() {
    AzureEndpoint::parse("https://aoai-one.openai.azure.com").unwrap();
    assert!(AzureEndpoint::parse("https://aoai-one.openai.azure.com.evil.test").is_err());
    assert!(AzureEndpoint::parse("http://aoai-one.openai.azure.com").is_err());
    let page1 = r#"{"value":[{"name":"chat-prod","properties":{"model":{"name":"gpt-4o","version":"2024-11-20"},"provisioningState":"Succeeded"}}],"nextLink":"https://management.azure.com/subscriptions/s/providers/Microsoft.CognitiveServices/accounts/a/deployments?api-version=2024-10-01&$skiptoken=two"}"#;
    let page2 = r#"{"value":[{"name":"embed-prod","properties":{"model":{"name":"text-embedding-3-large"},"provisioningState":"Succeeded"}}]}"#;
    let mut discovery = DeploymentDiscovery::new(config(), 4, 20);
    let next = discovery.accept_page(page1).unwrap().unwrap();
    assert!(next.starts_with("https://management.azure.com/"));
    assert!(discovery.accept_page(page2).unwrap().is_none());
    let entries = discovery.finish().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "azure-openai/chat-prod");
    assert_eq!(entries[0].source, "dynamic");
}

#[test]
fn responses_request_has_exact_broker_owned_host_and_path() {
    let endpoint = AzureEndpoint::parse("https://aoai-one.openai.azure.com").unwrap();
    let request = responses_request(
        &endpoint,
        "chat-prod",
        br#"{"model":"ignored","input":"hello"}"#,
    )
    .unwrap();
    assert_eq!(
        request.url,
        "https://aoai-one.openai.azure.com/openai/v1/responses"
    );
    assert_eq!(request.method, "POST");
    assert!(!request.body.windows(7).any(|w| w == b"ignored"));
    assert!(responses_request(&endpoint, "../escape", b"{}").is_err());
}
