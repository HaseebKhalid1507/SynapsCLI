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
    assert_eq!(
        r.header("authorization").unwrap(),
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-west-2/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=067f29cba0db54a62f64051ba66eb4e321a6ac6a347753a2cc5041dca178cdc7"
    );
    // Independently computed fixture following AWS IAM's documented SigV4
    // derivation: https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
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

fn test_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn event_frame(headers: &[(&str, u8, &str)], payload: &str) -> Vec<u8> {
    let mut hs = Vec::new();
    for (name, ty, value) in headers {
        hs.push(name.len() as u8);
        hs.extend_from_slice(name.as_bytes());
        hs.push(*ty);
        hs.extend_from_slice(&(value.len() as u16).to_be_bytes());
        hs.extend_from_slice(value.as_bytes());
    }
    let total = 16 + hs.len() + payload.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(hs.len() as u32).to_be_bytes());
    let prelude_crc = test_crc32(&out);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&hs);
    out.extend_from_slice(payload.as_bytes());
    let message_crc = test_crc32(&out);
    out.extend_from_slice(&message_crc.to_be_bytes());
    out
}

#[test]
fn eventstream_headers_exceptions_and_malformed_frames_fail_closed() {
    let base = [
        (":message-type", 7, "event"),
        (":event-type", 7, "messageStop"),
        (":content-type", 7, "application/json"),
    ];
    assert!(decode_converse_stream(&event_frame(&base, r#"{"messageStop":{}}"#)).is_ok());

    let missing = &base[..2];
    assert!(decode_converse_stream(&event_frame(missing, r#"{"messageStop":{}}"#)).is_err());
    let duplicate = [base[0], base[0], base[1], base[2]];
    assert!(decode_converse_stream(&event_frame(&duplicate, r#"{"messageStop":{}}"#)).is_err());
    let wrong_type = [(":message-type", 6, "event"), base[1], base[2]];
    assert!(decode_converse_stream(&event_frame(&wrong_type, r#"{"messageStop":{}}"#)).is_err());
    let mismatch = [base[0], (":event-type", 7, "metadata"), base[2]];
    assert!(decode_converse_stream(&event_frame(&mismatch, r#"{"messageStop":{}}"#)).is_err());
    let exception = [
        (":message-type", 7, "exception"),
        (":exception-type", 7, "internalServerException"),
        base[2],
    ];
    assert!(decode_converse_stream(&event_frame(
        &exception,
        r#"{"internalServerException":{"message":"no"}}"#,
    ))
    .is_err());
    let mut malformed = event_frame(&base, r#"{"messageStop":{}}"#);
    malformed[0] ^= 1;
    assert!(decode_converse_stream(&malformed).is_err());
}
