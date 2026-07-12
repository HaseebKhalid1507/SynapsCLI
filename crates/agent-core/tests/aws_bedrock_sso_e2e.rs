use agent_core::core::auth::aws_bedrock::*;
use agent_core::core::auth::{
    AwsBedrockConfig, BrokerMessage, InvokeOptions, InvokeRequest, MessageRole,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Fake {
    calls: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl AwsApi for Fake {
    async fn register_client(&self, _: &str) -> Result<RegisteredClient, AwsError> {
        self.calls.lock().unwrap().push("register");
        Ok(RegisteredClient::new("client", "secret", 9_999_999))
    }
    async fn start_device_authorization(
        &self,
        _: &RegisteredClient,
        _: &str,
    ) -> Result<DeviceAuthorization, AwsError> {
        self.calls.lock().unwrap().push("start");
        Ok(DeviceAuthorization::new(
            "device",
            "ABCD-EFGH",
            "https://device.sso.us-east-1.amazonaws.com/",
            1,
            600,
        ))
    }
    async fn create_token(
        &self,
        _: &RegisteredClient,
        _: &str,
        _: TokenGrant<'_>,
    ) -> Result<SsoToken, AwsError> {
        self.calls.lock().unwrap().push("token");
        Ok(SsoToken::new("sso-token", Some("refresh"), 3600))
    }
    async fn list_accounts(&self, _: &str, _: &str) -> Result<Vec<Account>, AwsError> {
        Ok(vec![Account {
            id: "123456789012".into(),
            name: "sandbox".into(),
        }])
    }
    async fn list_account_roles(&self, _: &str, _: &str, _: &str) -> Result<Vec<Role>, AwsError> {
        Ok(vec![Role {
            name: "BedrockRole".into(),
        }])
    }
    async fn get_role_credentials(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<RoleCredentials, AwsError> {
        Ok(RoleCredentials::new("AKID", "SECRET", "SESSION", 9_999_999))
    }
    async fn list_foundation_models(
        &self,
        _: SignedRequest,
    ) -> Result<Vec<FoundationModel>, AwsError> {
        Ok(vec![
            FoundationModel::new("anthropic.claude", "Claude", true, true),
            FoundationModel::new("embed.only", "Embed", false, false),
        ])
    }
    async fn converse(&self, r: SignedRequest) -> Result<ConverseOutput, AwsError> {
        assert!(r.has_header("x-amz-security-token"));
        Ok(ConverseOutput {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
            },
        })
    }
    async fn converse_stream(&self, _: SignedRequest) -> Result<ConverseEventStream, AwsError> {
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ConverseEvent::TextDelta("he".into())),
            Ok(ConverseEvent::ToolArguments {
                id: "1".into(),
                delta: "{}".into(),
            }),
            Ok(ConverseEvent::Usage(Usage {
                input_tokens: 2,
                output_tokens: 1,
            })),
            Ok(ConverseEvent::Done),
        ])))
    }
}
fn config() -> AwsBedrockConfig {
    AwsBedrockConfig::new(
        "https://example.awsapps.com/start",
        "us-east-1",
        "123456789012",
        "BedrockRole",
        "us-west-2",
    )
    .unwrap()
}

#[tokio::test]
async fn complete_zero_network_slice_keeps_credentials_inside_broker() {
    let fake = Fake {
        calls: Default::default(),
    };
    let broker = AwsBedrockBroker::login(fake.clone(), config(), Selection::Explicit)
        .await
        .unwrap();
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        &["register", "start", "token"]
    );
    let catalog = broker.catalog().await.unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "aws-bedrock/anthropic.claude");
    let req = InvokeRequest {
        messages: vec![BrokerMessage {
            role: MessageRole::User,
            content: "hi".into(),
        }],
        tools: vec![],
        stream: false,
        options: InvokeOptions::default(),
    };
    assert_eq!(
        broker
            .converse("aws-bedrock/anthropic.claude", req)
            .await
            .unwrap()
            .text,
        "hello"
    );
    let json = serde_json::to_string(&broker.public_context()).unwrap();
    assert!(
        !json.contains("AKID")
            && !json.contains("SECRET")
            && !json.contains("SESSION")
            && !json.contains("sso-token")
    );
}

#[tokio::test]
async fn multiple_accounts_require_explicit_selection() {
    #[derive(Clone)]
    struct Multiple(Fake);
    #[async_trait]
    impl AwsApi for Multiple {
        async fn register_client(&self, r: &str) -> Result<RegisteredClient, AwsError> {
            self.0.register_client(r).await
        }
        async fn start_device_authorization(
            &self,
            c: &RegisteredClient,
            u: &str,
        ) -> Result<DeviceAuthorization, AwsError> {
            self.0.start_device_authorization(c, u).await
        }
        async fn create_token(
            &self,
            c: &RegisteredClient,
            r: &str,
            g: TokenGrant<'_>,
        ) -> Result<SsoToken, AwsError> {
            self.0.create_token(c, r, g).await
        }
        async fn list_accounts(&self, _: &str, _: &str) -> Result<Vec<Account>, AwsError> {
            Ok(vec![
                Account {
                    id: "1".into(),
                    name: "a".into(),
                },
                Account {
                    id: "2".into(),
                    name: "b".into(),
                },
            ])
        }
        async fn list_account_roles(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Vec<Role>, AwsError> {
            unreachable!()
        }
        async fn get_role_credentials(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<RoleCredentials, AwsError> {
            unreachable!()
        }
        async fn list_foundation_models(
            &self,
            _: SignedRequest,
        ) -> Result<Vec<FoundationModel>, AwsError> {
            unreachable!()
        }
        async fn converse(&self, _: SignedRequest) -> Result<ConverseOutput, AwsError> {
            unreachable!()
        }
        async fn converse_stream(&self, _: SignedRequest) -> Result<ConverseEventStream, AwsError> {
            unreachable!()
        }
    }
    let f = Fake {
        calls: Default::default(),
    };
    assert!(matches!(
        AwsBedrockBroker::login(Multiple(f), config(), Selection::Explicit).await,
        Err(AwsError::SelectionRequired(_))
    ));
}

#[test]
fn sigv4_is_deterministic_and_secret_safe() {
    let creds = RoleCredentials::new("AKIDEXAMPLE", "secret", "token", 2_000_000_000_000);
    let r = sign_bedrock_request(
        "us-west-2",
        "POST",
        "/model/x/converse",
        b"{}",
        &creds,
        1_700_000_000,
    )
    .unwrap();
    assert!(r.header("authorization").unwrap().starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-west-2/bedrock/aws4_request"
    ));
    assert_eq!(r.header("x-amz-security-token"), Some("token"));
    assert_eq!(r.host, "bedrock-runtime.us-west-2.amazonaws.com");
    let control = sign_bedrock_request(
        "us-west-2",
        "GET",
        "/foundation-models",
        b"",
        &creds,
        1_700_000_000,
    )
    .unwrap();
    assert_eq!(control.host, "bedrock.us-west-2.amazonaws.com");
    let debug = format!("{creds:?}");
    assert!(!debug.contains("secret") && !debug.contains("token"));
}
