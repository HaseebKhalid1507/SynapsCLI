use agent_core::auth::{
    AuthIdentity, AwsBedrockConfig, AzureOpenAiConfig, BrokerOperation, CloudProviderId,
    GoogleVertexConfig, ProviderId,
};

#[test]
fn canonical_cloud_identities_are_not_oauth_aliases() {
    assert_eq!(
        ProviderId::try_from("azure-openai").unwrap(),
        ProviderId::Cloud(CloudProviderId::AzureOpenAi)
    );
    assert_eq!(
        ProviderId::try_from("aws-bedrock").unwrap(),
        ProviderId::Cloud(CloudProviderId::AwsBedrock)
    );
    assert_eq!(
        ProviderId::try_from("google-vertex").unwrap(),
        ProviderId::Cloud(CloudProviderId::GoogleVertex)
    );
    assert_eq!(
        AuthIdentity::for_provider(ProviderId::Cloud(CloudProviderId::AwsBedrock)),
        AuthIdentity::AwsTemporaryCredentials
    );
}

#[test]
fn non_secret_cloud_context_validates_before_network() {
    assert!(AzureOpenAiConfig::new(
        "organizations",
        "sub-1",
        "group_1",
        "account-1",
        "deployment-1"
    )
    .is_ok());
    assert!(AzureOpenAiConfig::new("common", "sub-1", "group", "account", "deployment").is_err());
    assert!(AwsBedrockConfig::new(
        "https://example.awsapps.com/start",
        "us-east-1",
        "123456789012",
        "BedrockRole",
        "us-west-2"
    )
    .is_ok());
    assert!(AwsBedrockConfig::new("http://bad", "us_east_1", "123", "", "everywhere").is_err());
    assert!(GoogleVertexConfig::new("my-project-1", "us-central1").is_ok());
    assert!(GoogleVertexConfig::new("UPPER", "https://evil.example").is_err());
}

#[test]
fn broker_operations_have_no_credential_or_url_fields() {
    let op = BrokerOperation::Catalog {
        provider: CloudProviderId::AwsBedrock,
        context_ref: "ctx-1".into(),
        allow_stale: true,
    };
    let json = serde_json::to_value(op).unwrap();
    let text = json.to_string();
    assert!(!text.contains("token"));
    assert!(!text.contains("secret"));
    assert!(!text.contains("url"));
}
