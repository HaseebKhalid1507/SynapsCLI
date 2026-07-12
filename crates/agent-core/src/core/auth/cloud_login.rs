//! Provider-specific interactive cloud login protocols.
//! Credentials remain persisted only through the broker-owned auth store.

use super::CloudProviderId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudLoginChallenge {
    DeviceCode {
        verification_uri: String,
        user_code: String,
    },
    AuthorizationUrl {
        url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudSelectionKind {
    Account,
    Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudLoginChoice {
    pub id: String,
    pub label: String,
}
impl CloudLoginChoice {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Presentation boundary for interactive cloud login. Core never accesses a terminal.
pub trait CloudLoginUi {
    fn present_challenge(&mut self, challenge: CloudLoginChallenge);
    fn select(
        &mut self,
        kind: CloudSelectionKind,
        choices: &[CloudLoginChoice],
    ) -> Result<String, String>;
}

/// Whether this build has an explicit login implementation for a cloud provider.
pub const fn supports_login(provider: CloudProviderId) -> bool {
    match provider {
        CloudProviderId::AwsBedrock
        | CloudProviderId::AzureOpenAi
        | CloudProviderId::GoogleVertex => true,
    }
}

/// Execute the provider-specific protocol selected by typed identity.
pub async fn login(provider: CloudProviderId, ui: &mut dyn CloudLoginUi) -> Result<(), String> {
    match provider {
        CloudProviderId::AwsBedrock => login_aws_bedrock(ui).await,
        CloudProviderId::AzureOpenAi => login_azure_openai(ui).await,
        CloudProviderId::GoogleVertex => login_google_vertex(ui).await,
    }
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

async fn login_azure_openai(ui: &mut dyn CloudLoginUi) -> Result<(), String> {
    use super::azure_openai::{
        device_code_request, refresh_request, AzureAudience, AzureRegistration,
    };
    let client_id = std::env::var("SYNAPS_AZURE_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or("registration_required: configure SYNAPS_AZURE_CLIENT_ID")?;
    let config = super::AzureOpenAiConfig::new(
        required_env("SYNAPS_AZURE_TENANT")?,
        required_env("SYNAPS_AZURE_SUBSCRIPTION_ID")?,
        required_env("SYNAPS_AZURE_RESOURCE_GROUP")?,
        required_env("SYNAPS_AZURE_RESOURCE_NAME")?,
        std::env::var("SYNAPS_AZURE_DEPLOYMENT").unwrap_or_else(|_| "default".into()),
    )?;
    let reg = AzureRegistration::production(Some(client_id.clone())).map_err(|e| e.to_string())?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let req = device_code_request(&config, &reg).map_err(|e| e.to_string())?;
    let d: serde_json::Value = http
        .post(req.url)
        .form(&req.form)
        .send()
        .await
        .map_err(|_| "Azure device authorization failed")?
        .json()
        .await
        .map_err(|_| "invalid Azure device response")?;
    ui.present_challenge(CloudLoginChallenge::DeviceCode {
        verification_uri: d["verification_uri"]
            .as_str()
            .unwrap_or("Microsoft sign-in")
            .into(),
        user_code: d["user_code"]
            .as_str()
            .unwrap_or("(provided by Microsoft)")
            .into(),
    });
    let device = d["device_code"]
        .as_str()
        .ok_or("invalid Azure device response")?;
    let token_url = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        config.tenant
    );
    let mut interval = d["interval"].as_u64().unwrap_or(5).max(1);
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(d["expires_in"].as_u64().unwrap_or(900).min(3600));
    let arm = loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("Azure device code expired".into());
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Err("Azure device authorization cancelled".into()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
        }
        let r = http
            .post(&token_url)
            .form(&[
                ("client_id", client_id.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device),
            ])
            .send()
            .await
            .map_err(|_| "Azure token polling failed")?;
        let v: serde_json::Value = r.json().await.map_err(|_| "invalid Azure token response")?;
        if v["access_token"].as_str().is_some() {
            break v;
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(e) => return Err(format!("Azure authorization failed: {e}")),
            None => return Err("invalid Azure token response".into()),
        }
    };
    let refresh = arm["refresh_token"]
        .as_str()
        .ok_or("Azure did not issue a refresh token")?
        .to_owned();
    let rr = refresh_request(&config, &reg, AzureAudience::Inference, &refresh)
        .map_err(|e| e.to_string())?;
    let infer: serde_json::Value = http
        .post(rr.url)
        .form(&rr.form)
        .send()
        .await
        .map_err(|_| "Azure inference token failed")?
        .json()
        .await
        .map_err(|_| "invalid Azure inference token")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // Resolve the data-plane endpoint from ARM metadata before committing any
    // state; never derive an authority from the user-supplied resource name.
    let account_url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.CognitiveServices/accounts/{}?api-version=2023-05-01",
        config.subscription_id, config.resource_group, config.resource_name
    );
    let account: serde_json::Value = http
        .get(account_url)
        .bearer_auth(
            arm["access_token"]
                .as_str()
                .ok_or("invalid Azure ARM token")?,
        )
        .send()
        .await
        .map_err(|_| "Azure ARM endpoint resolution failed")?
        .error_for_status()
        .map_err(|_| "Azure ARM resource validation failed")?
        .json()
        .await
        .map_err(|_| "invalid Azure ARM resource metadata")?;
    let endpoint = account
        .pointer("/properties/endpoint")
        .and_then(|v| v.as_str())
        .ok_or("Azure ARM metadata omitted the resource endpoint")?;
    super::azure_openai::AzureEndpoint::parse(endpoint)
        .map_err(|_| "Azure ARM returned an untrusted resource endpoint")?;
    // Validate the selected resource/deployment against ARM before replacing a
    // prior login. This is deliberately before the sole persistence point.
    let deployment_url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.CognitiveServices/accounts/{}/deployments?api-version=2023-05-01",
        config.subscription_id, config.resource_group, config.resource_name
    );
    let deployments: serde_json::Value = http
        .get(deployment_url)
        .bearer_auth(
            arm["access_token"]
                .as_str()
                .ok_or("invalid Azure ARM token")?,
        )
        .send()
        .await
        .map_err(|_| "Azure catalog validation failed; prior login preserved")?
        .error_for_status()
        .map_err(|_| "Azure catalog validation failed; prior login preserved")?
        .json()
        .await
        .map_err(|_| "invalid Azure deployment catalog")?;
    let deployment_exists = deployments["value"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["name"].as_str() == Some(config.deployment.as_str()));
    if !deployment_exists {
        return Err("Azure deployment is absent from catalog; prior login preserved".into());
    }
    super::save_cloud_state(
        "azure-openai",
        &serde_json::json!({"config":config,"client_id":client_id,"endpoint":endpoint,"refresh_token":infer["refresh_token"].as_str().unwrap_or(&refresh),"arm":{"access_token":arm["access_token"],"expires_at":now+arm["expires_in"].as_u64().unwrap_or(3600)*1000},"inference":{"access_token":infer["access_token"],"expires_at":now+infer["expires_in"].as_u64().unwrap_or(3600)*1000}}),
    )
}

async fn login_google_vertex(ui: &mut dyn CloudLoginUi) -> Result<(), String> {
    use base64::Engine;
    use sha2::Digest;
    let client_id = std::env::var("SYNAPS_VERTEX_CLIENT_ID")
        .or_else(|_| std::env::var("SYNAPS_GOOGLE_VERTEX_CLIENT_ID"))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or("registration_required: configure SYNAPS_VERTEX_CLIENT_ID")?;
    let reg = super::google_vertex::VertexRegistration::new(Some(&client_id))
        .map_err(|e| e.to_string())?;
    let config = super::GoogleVertexConfig::new(
        required_env("SYNAPS_VERTEX_PROJECT")?,
        required_env("SYNAPS_VERTEX_LOCATION")?,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}/oauth2callback");
    let verifier =
        uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = super::google_vertex::build_authorize_url(&reg, &challenge, &state, &redirect)
        .map_err(|e| e.to_string())?;
    ui.present_challenge(CloudLoginChallenge::AuthorizationUrl {
        url: url.to_string(),
    });
    let (mut socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut b = vec![0; 8192];
    let n = socket.read(&mut b).await.map_err(|e| e.to_string())?;
    let first = std::str::from_utf8(&b[..n])
        .map_err(|_| "invalid OAuth callback")?
        .lines()
        .next()
        .ok_or("invalid OAuth callback")?;
    let target = first
        .split_whitespace()
        .nth(1)
        .ok_or("invalid OAuth callback")?;
    let u = url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "invalid OAuth callback")?;
    let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
    if q.get("state") != Some(&state) {
        return Err("OAuth state mismatch".into());
    }
    let code = q.get("code").ok_or("Google authorization denied")?;
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\n\r\nAuthorization complete.")
        .await
        .ok();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = http
        .post(super::google_vertex::TOKEN_URL)
        .form(&[
            ("client_id", client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
        ])
        .send()
        .await
        .map_err(|_| "Vertex token exchange failed")?
        .json()
        .await
        .map_err(|_| "invalid Vertex token response")?;
    let refresh = v["refresh_token"]
        .as_str()
        .ok_or("Google did not issue offline refresh access")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // Validate access and context before the only commit, preserving old state
    // on OAuth success followed by an unusable Vertex catalog.
    let catalog_url = format!(
        "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models?pageSize=1",
        config.location, config.project_id, config.location
    );
    let catalog: serde_json::Value = http
        .get(catalog_url)
        .bearer_auth(
            v["access_token"]
                .as_str()
                .ok_or("invalid Vertex token response")?,
        )
        .send()
        .await
        .map_err(|_| "Vertex catalog validation failed; prior login preserved")?
        .error_for_status()
        .map_err(|_| "Vertex catalog validation failed; prior login preserved")?
        .json()
        .await
        .map_err(|_| "invalid Vertex model catalog")?;
    if catalog["publisherModels"]
        .as_array()
        .map_or(true, |models| models.is_empty())
    {
        return Err("Vertex model catalog is empty; prior login preserved".into());
    }
    super::save_cloud_state(
        "google-vertex",
        &serde_json::json!({"config":config,"client_id":client_id,"access_token":v["access_token"],"refresh_token":refresh,"expires_at":now+v["expires_in"].as_u64().unwrap_or(3600)*1000}),
    )
}

async fn login_aws_bedrock(ui: &mut dyn CloudLoginUi) -> Result<(), String> {
    use super::aws_bedrock::{AwsApi, AwsHttpApi, TokenGrant};
    let start = required_env("SYNAPS_AWS_SSO_START_URL")?;
    let sso_region = required_env("SYNAPS_AWS_SSO_REGION")?;
    let bedrock_region = required_env("SYNAPS_AWS_BEDROCK_REGION")?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "cannot initialize HTTPS client")?;
    let api = AwsHttpApi::new(http.clone(), &sso_region);
    let client = api
        .register_client(&sso_region)
        .await
        .map_err(|e| e.to_string())?;
    let device = api
        .start_device_authorization(&client, &start)
        .await
        .map_err(|e| e.to_string())?;
    ui.present_challenge(CloudLoginChallenge::DeviceCode {
        verification_uri: device.verification_uri.clone(),
        user_code: device.user_code.clone(),
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(1);
    let token = loop {
        if std::time::Instant::now() >= deadline {
            return Err("device authorization expired".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        match api
            .create_token(
                &client,
                &sso_region,
                TokenGrant::DeviceCode(device.device_code()),
            )
            .await
        {
            Ok(token) => break token,
            Err(super::aws_bedrock::AwsError::AuthorizationPending) => {}
            Err(super::aws_bedrock::AwsError::SlowDown) => interval = interval.saturating_add(5),
            Err(e) => return Err(e.to_string()),
        }
    };
    let accounts = api
        .list_accounts(&sso_region, token.access())
        .await
        .map_err(|e| e.to_string())?;
    let account = choose(
        CloudSelectionKind::Account,
        accounts
            .iter()
            .map(|a| CloudLoginChoice::new(&a.id, &a.name))
            .collect(),
        std::env::var("SYNAPS_AWS_ACCOUNT_ID").ok().as_deref(),
        ui,
    )?;
    let roles = api
        .list_account_roles(&sso_region, token.access(), &account)
        .await
        .map_err(|e| e.to_string())?;
    let role = choose(
        CloudSelectionKind::Role,
        roles
            .iter()
            .map(|r| CloudLoginChoice::new(&r.name, ""))
            .collect(),
        std::env::var("SYNAPS_AWS_ROLE_NAME").ok().as_deref(),
        ui,
    )?;
    let credentials = api
        .get_role_credentials(&sso_region, token.access(), &account, &role)
        .await
        .map_err(|e| e.to_string())?;
    let config = super::AwsBedrockConfig::new(start, sso_region, account, role, bedrock_region)?;
    // The login transaction is not committed until the selected Bedrock
    // context can return a non-empty catalog. A failed relogin must leave the
    // previously working credential untouched.
    let validation = super::aws_bedrock::AwsBedrockBroker::from_credentials(
        super::aws_bedrock::AwsHttpApi::new(http.clone(), &config.sso_region),
        config.clone(),
        super::aws_bedrock::RoleCredentials::new(
            credentials.access_key(),
            credentials.secret_key(),
            credentials.session_token(),
            credentials.expires_at,
        ),
    );
    validation
        .catalog()
        .await
        .map_err(|_| "AWS Bedrock catalog validation failed; prior login preserved".to_string())?;
    let state = serde_json::json!({
        "config": config, "access_key": credentials.access_key(), "secret_key": credentials.secret_key(),
        "session_token": credentials.session_token(), "expires_at": credentials.expires_at,
        "registered_client": {"id": client.id(), "secret": client.secret(), "expires_at": client.expires_at},
        "sso_access_token": token.access(), "sso_refresh_token": token.refresh(),
        "sso_expires_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 + token.expires_in * 1000
    });
    super::save_cloud_state("aws-bedrock", &state)
}

fn choose(
    kind: CloudSelectionKind,
    values: Vec<CloudLoginChoice>,
    configured: Option<&str>,
    ui: &mut dyn CloudLoginUi,
) -> Result<String, String> {
    if let Some(want) = configured {
        return values
            .iter()
            .find(|v| v.id == want)
            .map(|v| v.id.clone())
            .ok_or_else(|| format!("configured {} is not assigned", kind.name()));
    }
    if values.len() == 1 {
        return Ok(values[0].id.clone());
    }
    ui.select(kind, &values)
}

impl CloudSelectionKind {
    fn name(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Role => "role",
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[derive(Default)]
    struct RecordingUi {
        challenges: Vec<CloudLoginChallenge>,
        selections: Vec<(CloudSelectionKind, Vec<CloudLoginChoice>)>,
    }

    impl CloudLoginUi for RecordingUi {
        fn present_challenge(&mut self, challenge: CloudLoginChallenge) {
            self.challenges.push(challenge);
        }

        fn select(
            &mut self,
            kind: CloudSelectionKind,
            choices: &[CloudLoginChoice],
        ) -> Result<String, String> {
            self.selections.push((kind, choices.to_vec()));
            Ok(choices[1].id.clone())
        }
    }

    #[test]
    fn injected_ui_receives_typed_challenges_and_selections() {
        let mut ui = RecordingUi::default();
        ui.present_challenge(CloudLoginChallenge::DeviceCode {
            verification_uri: "https://example.test".into(),
            user_code: "ABCD".into(),
        });
        let selected = choose(
            CloudSelectionKind::Account,
            vec![
                CloudLoginChoice::new("1", "first"),
                CloudLoginChoice::new("2", "second"),
            ],
            None,
            &mut ui,
        )
        .unwrap();
        assert_eq!(selected, "2");
        assert_eq!(ui.challenges.len(), 1);
        assert_eq!(ui.selections[0].0, CloudSelectionKind::Account);
    }
}
