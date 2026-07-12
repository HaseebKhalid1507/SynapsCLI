//! Opt-in, executable AWS Bedrock live gate. No response or credential is printed.
use agent_core::core::auth::{
    self,
    cloud::{AwsBedrockConfig, BrokerMessage, CloudProviderId, InvokeRequest, MessageRole},
    CredentialBroker,
};
use futures::StreamExt;

#[tokio::test]
#[ignore = "requires SYNAPS_AWS_LIVE_TEST=1 and pre-populated broker state"]
async fn aws_bedrock_live_catalog_and_stream() {
    assert_eq!(std::env::var("SYNAPS_AWS_LIVE_TEST").as_deref(), Ok("1"));
    let model = std::env::var("SYNAPS_AWS_MODEL_ID").expect("model id");
    // Validate all operator context even though persisted broker context remains authoritative.
    AwsBedrockConfig::new(
        std::env::var("SYNAPS_AWS_SSO_START_URL").unwrap(),
        std::env::var("SYNAPS_AWS_SSO_REGION").unwrap(),
        std::env::var("SYNAPS_AWS_ACCOUNT_ID").unwrap(),
        std::env::var("SYNAPS_AWS_ROLE_NAME").unwrap(),
        std::env::var("SYNAPS_AWS_BEDROCK_REGION").unwrap(),
    )
    .expect("valid context");
    let broker = auth::LocalBroker::new(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    );
    let catalog = broker
        .cloud_catalog(CloudProviderId::AwsBedrock, "aws-bedrock", false)
        .await
        .expect("live catalog");
    assert!(
        catalog.iter().any(|entry| entry.id == model),
        "configured model absent from live catalog"
    );
    let request = InvokeRequest {
        messages: vec![BrokerMessage {
            role: MessageRole::User,
            content: "Reply with one short greeting.".into(),
        }],
        tools: vec![],
        stream: true,
        options: Default::default(),
    };
    let mut stream = broker
        .cloud_invoke(CloudProviderId::AwsBedrock, "aws-bedrock", &model, request)
        .await
        .expect("live stream");
    let mut done = false;
    while let Some(event) = stream.next().await {
        if matches!(event.expect("event"), auth::broker::CloudEvent::Done) {
            done = true;
            break;
        }
    }
    assert!(done);
    eprintln!("AWS Bedrock live gate passed (secret-safe)");
}
