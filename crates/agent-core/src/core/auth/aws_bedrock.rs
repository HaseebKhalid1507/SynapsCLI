//! Provider-local IAM Identity Center and Bedrock broker boundary.
//! The transport trait is intentionally typed: it cannot sign arbitrary URLs or vend credentials.
use super::cloud::{AwsBedrockConfig, InvokeRequest};
use async_trait::async_trait;
use futures::Stream;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, pin::Pin};

#[derive(Clone)]
pub struct RegisteredClient {
    id: String,
    secret: String,
    pub expires_at: u64,
}
impl RegisteredClient {
    pub fn new(id: impl Into<String>, secret: impl Into<String>, expires_at: u64) -> Self {
        Self {
            id: id.into(),
            secret: secret.into(),
            expires_at,
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn secret(&self) -> &str {
        &self.secret
    }
}
impl fmt::Debug for RegisteredClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredClient")
            .field("id", &"[REDACTED]")
            .field("secret", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
#[derive(Clone)]
pub struct DeviceAuthorization {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}
impl DeviceAuthorization {
    pub fn new(
        d: impl Into<String>,
        u: impl Into<String>,
        v: impl Into<String>,
        interval: u64,
        expires_in: u64,
    ) -> Self {
        Self {
            device_code: d.into(),
            user_code: u.into(),
            verification_uri: v.into(),
            interval,
            expires_in,
        }
    }
    pub fn device_code(&self) -> &str {
        &self.device_code
    }
}
impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceAuthorization([REDACTED])")
    }
}
#[derive(Clone)]
pub struct SsoToken {
    access: String,
    refresh: Option<String>,
    pub expires_in: u64,
}
impl SsoToken {
    pub fn new(a: impl Into<String>, r: Option<&str>, e: u64) -> Self {
        Self {
            access: a.into(),
            refresh: r.map(str::to_owned),
            expires_in: e,
        }
    }
    pub fn access(&self) -> &str {
        &self.access
    }
    pub fn refresh(&self) -> Option<&str> {
        self.refresh.as_deref()
    }
}
impl fmt::Debug for SsoToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SsoToken([REDACTED])")
    }
}
#[derive(Clone)]
pub struct RoleCredentials {
    access_key: String,
    secret_key: String,
    session_token: String,
    pub expires_at: u64,
}
impl RoleCredentials {
    pub fn new(a: impl Into<String>, s: impl Into<String>, t: impl Into<String>, e: u64) -> Self {
        Self {
            access_key: a.into(),
            secret_key: s.into(),
            session_token: t.into(),
            expires_at: e,
        }
    }
    pub fn access_key(&self) -> &str {
        &self.access_key
    }
    pub fn secret_key(&self) -> &str {
        &self.secret_key
    }
    pub fn session_token(&self) -> &str {
        &self.session_token
    }
}
impl fmt::Debug for RoleCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoleCredentials([REDACTED])")
    }
}
#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Clone)]
pub struct Role {
    pub name: String,
}
#[derive(Debug, Clone)]
pub struct FoundationModel {
    pub id: String,
    pub name: String,
    pub supports_text: bool,
    pub supports_converse: bool,
}
impl FoundationModel {
    pub fn new(i: impl Into<String>, n: impl Into<String>, t: bool, c: bool) -> Self {
        Self {
            id: i.into(),
            name: n.into(),
            supports_text: t,
            supports_converse: c,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub display_name: String,
    pub source: &'static str,
    pub region: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConverseOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConverseEvent {
    TextDelta(String),
    ToolArguments { id: String, delta: String },
    Usage(Usage),
    Done,
}
#[derive(Debug, Clone, Copy)]
pub enum TokenGrant<'a> {
    DeviceCode(&'a str),
    RefreshToken(&'a str),
}
#[derive(Debug, Clone, Copy)]
pub enum Selection {
    Explicit,
}
#[derive(Debug, thiserror::Error)]
pub enum AwsError {
    #[error("explicit selection required: {0}")]
    SelectionRequired(&'static str),
    #[error("authorization pending")]
    AuthorizationPending,
    #[error("authorization polling slowed down")]
    SlowDown,
    #[error("AWS authorization denied or expired")]
    AuthorizationFailed,
    #[error("invalid model id")]
    InvalidModel,
    #[error("AWS operation failed (details redacted)")]
    Upstream,
}

#[derive(Clone)]
pub struct SignedRequest {
    pub method: &'static str,
    pub host: String,
    pub path: String,
    pub body: Vec<u8>,
    headers: BTreeMap<String, String>,
}
impl SignedRequest {
    pub fn header(&self, n: &str) -> Option<&str> {
        self.headers.get(n).map(String::as_str)
    }
    pub fn has_header(&self, n: &str) -> bool {
        self.headers.contains_key(n)
    }
}
impl fmt::Debug for SignedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedRequest")
            .field("method", &self.method)
            .field("host", &self.host)
            .field("path", &self.path)
            .field("headers", &"[REDACTED]")
            .finish()
    }
}

pub type ConverseEventStream =
    Pin<Box<dyn Stream<Item = Result<ConverseEvent, AwsError>> + Send + 'static>>;

#[async_trait]
pub trait AwsApi: Send + Sync + Clone + 'static {
    async fn register_client(&self, region: &str) -> Result<RegisteredClient, AwsError>;
    async fn start_device_authorization(
        &self,
        client: &RegisteredClient,
        start_url: &str,
    ) -> Result<DeviceAuthorization, AwsError>;
    async fn create_token(
        &self,
        client: &RegisteredClient,
        region: &str,
        grant: TokenGrant<'_>,
    ) -> Result<SsoToken, AwsError>;
    async fn list_accounts(
        &self,
        region: &str,
        access_token: &str,
    ) -> Result<Vec<Account>, AwsError>;
    async fn list_account_roles(
        &self,
        region: &str,
        access_token: &str,
        account: &str,
    ) -> Result<Vec<Role>, AwsError>;
    async fn get_role_credentials(
        &self,
        region: &str,
        access_token: &str,
        account: &str,
        role: &str,
    ) -> Result<RoleCredentials, AwsError>;
    async fn list_foundation_models(
        &self,
        request: SignedRequest,
    ) -> Result<Vec<FoundationModel>, AwsError>;
    async fn converse(&self, request: SignedRequest) -> Result<ConverseOutput, AwsError>;
    async fn converse_stream(
        &self,
        request: SignedRequest,
    ) -> Result<ConverseEventStream, AwsError>;
}
#[derive(Debug, Clone, Serialize)]
pub struct PublicContext {
    pub sso_region: String,
    pub account_id: String,
    pub role_name: String,
    pub bedrock_region: String,
}
pub struct AwsBedrockBroker<A: AwsApi> {
    api: A,
    config: AwsBedrockConfig,
    credentials: RoleCredentials,
}
impl<A: AwsApi> AwsBedrockBroker<A> {
    /// Restore broker-owned temporary credentials. This is intentionally only a
    /// typed Bedrock adapter; credentials can never be extracted or used to sign
    /// caller-selected requests.
    pub fn from_credentials(
        api: A,
        config: AwsBedrockConfig,
        credentials: RoleCredentials,
    ) -> Self {
        Self {
            api,
            config,
            credentials,
        }
    }

    pub async fn login(api: A, config: AwsBedrockConfig, _: Selection) -> Result<Self, AwsError> {
        let c = api.register_client(&config.sso_region).await?;
        let d = api
            .start_device_authorization(&c, &config.sso_start_url)
            .await?;
        let token = api
            .create_token(
                &c,
                &config.sso_region,
                TokenGrant::DeviceCode(&d.device_code),
            )
            .await?;
        let accounts = api.list_accounts(&config.sso_region, &token.access).await?;
        if accounts.len() != 1 {
            return Err(AwsError::SelectionRequired("account"));
        }
        if accounts[0].id != config.account_id {
            return Err(AwsError::SelectionRequired("configured account"));
        }
        let roles = api
            .list_account_roles(&config.sso_region, &token.access, &config.account_id)
            .await?;
        if roles.len() != 1 || roles[0].name != config.role_name {
            return Err(AwsError::SelectionRequired("role"));
        }
        let credentials = api
            .get_role_credentials(
                &config.sso_region,
                &token.access,
                &config.account_id,
                &config.role_name,
            )
            .await?;
        Ok(Self {
            api,
            config,
            credentials,
        })
    }
    pub fn public_context(&self) -> PublicContext {
        PublicContext {
            sso_region: self.config.sso_region.clone(),
            account_id: self.config.account_id.clone(),
            role_name: self.config.role_name.clone(),
            bedrock_region: self.config.bedrock_region.clone(),
        }
    }
    pub async fn catalog(&self) -> Result<Vec<CatalogEntry>, AwsError> {
        let r = sign_bedrock_request(
            &self.config.bedrock_region,
            "GET",
            "/foundation-models",
            b"",
            &self.credentials,
            chrono::Utc::now().timestamp().max(0) as u64,
        )?;
        Ok(self
            .api
            .list_foundation_models(r)
            .await?
            .into_iter()
            .filter(|m| m.supports_text && m.supports_converse)
            .map(|m| CatalogEntry {
                id: format!("aws-bedrock/{}", m.id),
                display_name: m.name,
                source: "dynamic",
                region: self.config.bedrock_region.clone(),
            })
            .collect())
    }
    pub async fn converse(
        &self,
        model: &str,
        request: InvokeRequest,
    ) -> Result<ConverseOutput, AwsError> {
        let id = valid_model(model)?;
        let body = serde_json::to_vec(&request).map_err(|_| AwsError::Upstream)?;
        let path = format!("/model/{id}/converse");
        let r = sign_bedrock_request(
            &self.config.bedrock_region,
            "POST",
            &path,
            &body,
            &self.credentials,
            chrono::Utc::now().timestamp().max(0) as u64,
        )?;
        self.api.converse(r).await
    }
    pub async fn converse_stream(
        &self,
        model: &str,
        request: InvokeRequest,
    ) -> Result<ConverseEventStream, AwsError> {
        let id = valid_model(model)?;
        let body = serde_json::to_vec(&request).map_err(|_| AwsError::Upstream)?;
        let path = format!("/model/{id}/converse-stream");
        let r = sign_bedrock_request(
            &self.config.bedrock_region,
            "POST",
            &path,
            &body,
            &self.credentials,
            chrono::Utc::now().timestamp().max(0) as u64,
        )?;
        self.api.converse_stream(r).await
    }
}

#[derive(Clone)]
pub struct AwsHttpApi {
    http: reqwest::Client,
    sso_region: String,
}
impl AwsHttpApi {
    pub fn new(http: reqwest::Client, sso_region: impl Into<String>) -> Self {
        Self {
            http,
            sso_region: sso_region.into(),
        }
    }
}
fn aws_json_error(status: reqwest::StatusCode) -> AwsError {
    if status.is_success() {
        AwsError::Upstream
    } else {
        AwsError::Upstream
    }
}
#[async_trait]
impl AwsApi for AwsHttpApi {
    async fn register_client(&self, region: &str) -> Result<RegisteredClient, AwsError> {
        let u = format!("https://oidc.{region}.amazonaws.com/client/register");
        let v:serde_json::Value=self.http.post(u).json(&serde_json::json!({"clientName":"synaps-cli","clientType":"public","scopes":["sso:account:access"]})).send().await.map_err(|_|AwsError::Upstream)?.error_for_status().map_err(|e|aws_json_error(e.status().unwrap_or(reqwest::StatusCode::BAD_GATEWAY)))?.json().await.map_err(|_|AwsError::Upstream)?;
        Ok(RegisteredClient::new(
            v["clientId"].as_str().ok_or(AwsError::Upstream)?,
            v["clientSecret"].as_str().ok_or(AwsError::Upstream)?,
            v["clientSecretExpiresAt"]
                .as_u64()
                .ok_or(AwsError::Upstream)?,
        ))
    }
    async fn start_device_authorization(
        &self,
        c: &RegisteredClient,
        start: &str,
    ) -> Result<DeviceAuthorization, AwsError> {
        let u = format!(
            "https://oidc.{}.amazonaws.com/device_authorization",
            self.sso_region
        );
        let v: serde_json::Value = self
            .http
            .post(u)
            .json(&serde_json::json!({"clientId":c.id,"clientSecret":c.secret,"startUrl":start}))
            .send()
            .await
            .map_err(|_| AwsError::Upstream)?
            .error_for_status()
            .map_err(|_| AwsError::Upstream)?
            .json()
            .await
            .map_err(|_| AwsError::Upstream)?;
        Ok(DeviceAuthorization::new(
            v["deviceCode"].as_str().ok_or(AwsError::Upstream)?,
            v["userCode"].as_str().ok_or(AwsError::Upstream)?,
            v.get("verificationUriComplete")
                .or_else(|| v.get("verificationUri"))
                .and_then(|x| x.as_str())
                .ok_or(AwsError::Upstream)?,
            v["interval"].as_u64().unwrap_or(5),
            v["expiresIn"].as_u64().ok_or(AwsError::Upstream)?,
        ))
    }
    async fn create_token(
        &self,
        c: &RegisteredClient,
        region: &str,
        g: TokenGrant<'_>,
    ) -> Result<SsoToken, AwsError> {
        let (grant, key) = match g {
            TokenGrant::DeviceCode(x) => (
                "urn:ietf:params:oauth:grant-type:device_code",
                ("deviceCode", x),
            ),
            TokenGrant::RefreshToken(x) => ("refresh_token", ("refreshToken", x)),
        };
        let mut body =
            serde_json::json!({"clientId":c.id,"clientSecret":c.secret,"grantType":grant});
        body[key.0] = key.1.into();
        let r = self
            .http
            .post(format!("https://oidc.{region}.amazonaws.com/token"))
            .json(&body)
            .send()
            .await
            .map_err(|_| AwsError::Upstream)?;
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(if t.contains("AuthorizationPending") {
                AwsError::AuthorizationPending
            } else if t.contains("SlowDown") {
                AwsError::SlowDown
            } else {
                AwsError::AuthorizationFailed
            });
        }
        let v: serde_json::Value = r.json().await.map_err(|_| AwsError::Upstream)?;
        Ok(SsoToken::new(
            v["accessToken"].as_str().ok_or(AwsError::Upstream)?,
            v["refreshToken"].as_str(),
            v["expiresIn"].as_u64().ok_or(AwsError::Upstream)?,
        ))
    }
    async fn list_accounts(&self, r: &str, t: &str) -> Result<Vec<Account>, AwsError> {
        let v: serde_json::Value = self
            .http
            .get(format!(
                "https://portal.sso.{r}.amazonaws.com/assignment/accounts"
            ))
            .header("x-amz-sso_bearer_token", t)
            .send()
            .await
            .map_err(|_| AwsError::Upstream)?
            .error_for_status()
            .map_err(|_| AwsError::Upstream)?
            .json()
            .await
            .map_err(|_| AwsError::Upstream)?;
        Ok(v["accountList"]
            .as_array()
            .ok_or(AwsError::Upstream)?
            .iter()
            .filter_map(|x| {
                Some(Account {
                    id: x["accountId"].as_str()?.into(),
                    name: x["accountName"].as_str().unwrap_or("").into(),
                })
            })
            .collect())
    }
    async fn list_account_roles(&self, r: &str, t: &str, a: &str) -> Result<Vec<Role>, AwsError> {
        let v: serde_json::Value = self
            .http
            .get(format!(
                "https://portal.sso.{r}.amazonaws.com/assignment/roles?account_id={a}"
            ))
            .header("x-amz-sso_bearer_token", t)
            .send()
            .await
            .map_err(|_| AwsError::Upstream)?
            .error_for_status()
            .map_err(|_| AwsError::Upstream)?
            .json()
            .await
            .map_err(|_| AwsError::Upstream)?;
        Ok(v["roleList"]
            .as_array()
            .ok_or(AwsError::Upstream)?
            .iter()
            .filter_map(|x| {
                Some(Role {
                    name: x["roleName"].as_str()?.into(),
                })
            })
            .collect())
    }
    async fn get_role_credentials(
        &self,
        r: &str,
        t: &str,
        a: &str,
        role: &str,
    ) -> Result<RoleCredentials, AwsError> {
        let v:serde_json::Value=self.http.get(format!("https://portal.sso.{r}.amazonaws.com/federation/credentials?account_id={a}&role_name={role}")).header("x-amz-sso_bearer_token",t).send().await.map_err(|_|AwsError::Upstream)?.error_for_status().map_err(|_|AwsError::Upstream)?.json().await.map_err(|_|AwsError::Upstream)?;
        let c = &v["roleCredentials"];
        Ok(RoleCredentials::new(
            c["accessKeyId"].as_str().ok_or(AwsError::Upstream)?,
            c["secretAccessKey"].as_str().ok_or(AwsError::Upstream)?,
            c["sessionToken"].as_str().ok_or(AwsError::Upstream)?,
            c["expiration"].as_u64().ok_or(AwsError::Upstream)?,
        ))
    }
    async fn list_foundation_models(
        &self,
        q: SignedRequest,
    ) -> Result<Vec<FoundationModel>, AwsError> {
        let v = self.send_signed(q).await?;
        Ok(v["modelSummaries"]
            .as_array()
            .ok_or(AwsError::Upstream)?
            .iter()
            .filter_map(|x| {
                Some(FoundationModel::new(
                    x["modelId"].as_str()?,
                    x["modelName"].as_str().unwrap_or(""),
                    x["inputModalities"].as_array()?.iter().any(|v| v == "TEXT"),
                    x["inferenceTypesSupported"].as_array().is_some(),
                ))
            })
            .collect())
    }
    async fn converse(&self, q: SignedRequest) -> Result<ConverseOutput, AwsError> {
        let v = self.send_signed(q).await?;
        Ok(ConverseOutput {
            text: v
                .pointer("/output/message/content/0/text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .into(),
            tool_calls: vec![],
            usage: Usage {
                input_tokens: v["usage"]["inputTokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["outputTokens"].as_u64().unwrap_or(0),
            },
        })
    }
    async fn converse_stream(&self, q: SignedRequest) -> Result<ConverseEventStream, AwsError> {
        use futures::StreamExt;
        let url = format!("https://{}{}", q.host, q.path);
        let mut request = self.http.post(url).body(q.body);
        for (name, value) in q.headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| AwsError::Upstream)?
            .error_for_status()
            .map_err(|_| AwsError::Upstream)?;
        let chunks = response.bytes_stream();
        // `unfold` owns the response and parser buffer. Dropping the returned
        // stream drops the response immediately (cancellation), while yielding
        // one frame at a time and retaining at most one bounded frame.
        const MAX_FRAME: usize = 1024 * 1024;
        let stream = futures::stream::unfold(
            (Box::pin(chunks), Vec::<u8>::new(), false),
            |(mut chunks, mut buffer, done)| async move {
                if done {
                    return None;
                }
                loop {
                    if buffer.len() >= 4 {
                        let total = u32::from_be_bytes(buffer[..4].try_into().ok()?) as usize;
                        if !(16..=MAX_FRAME).contains(&total) {
                            return Some((Err(AwsError::Upstream), (chunks, buffer, true)));
                        }
                        if buffer.len() >= total {
                            let frame: Vec<u8> = buffer.drain(..total).collect();
                            let event = decode_event_frame(&frame);
                            let terminal = matches!(event, Ok(Some(ConverseEvent::Done)) | Err(_));
                            match event {
                                Ok(Some(event)) => {
                                    return Some((Ok(event), (chunks, buffer, terminal)))
                                }
                                Ok(None) => continue,
                                Err(error) => return Some((Err(error), (chunks, buffer, true))),
                            }
                        }
                    }
                    match chunks.next().await {
                        Some(Ok(chunk)) if buffer.len() + chunk.len() <= MAX_FRAME => {
                            buffer.extend_from_slice(&chunk)
                        }
                        Some(_) => return Some((Err(AwsError::Upstream), (chunks, buffer, true))),
                        None => return Some((Err(AwsError::Upstream), (chunks, buffer, true))),
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

fn decode_event_frame(bytes: &[u8]) -> Result<Option<ConverseEvent>, AwsError> {
    if bytes.len() < 16 {
        return Err(AwsError::Upstream);
    }
    let total = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let headers = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if total != bytes.len()
        || headers > total - 16
        || crc32(&bytes[..8]) != u32::from_be_bytes(bytes[8..12].try_into().unwrap())
        || crc32(&bytes[..total - 4]) != u32::from_be_bytes(bytes[total - 4..].try_into().unwrap())
    {
        return Err(AwsError::Upstream);
    }
    let header_bytes = &bytes[12..12 + headers];
    let payload = &bytes[12 + headers..total - 4];
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| AwsError::Upstream)?;
    validate_event_headers(header_bytes, &value)?;
    if let Some(delta) = value
        .pointer("/contentBlockDelta/delta/text")
        .and_then(|v| v.as_str())
    {
        Ok(Some(ConverseEvent::TextDelta(delta.to_owned())))
    } else if let Some(delta) = value
        .pointer("/contentBlockDelta/delta/toolUse/input")
        .and_then(|v| v.as_str())
    {
        Ok(Some(ConverseEvent::ToolArguments {
            id: value
                .pointer("/contentBlockDelta/delta/toolUse/toolUseId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            delta: delta.to_owned(),
        }))
    } else if let Some(usage) = value.pointer("/metadata/usage") {
        Ok(Some(ConverseEvent::Usage(Usage {
            input_tokens: usage["inputTokens"].as_u64().unwrap_or(0),
            output_tokens: usage["outputTokens"].as_u64().unwrap_or(0),
        })))
    } else if value.get("messageStop").is_some() {
        Ok(Some(ConverseEvent::Done))
    } else if value.get("internalServerException").is_some()
        || value.get("modelStreamErrorException").is_some()
    {
        Err(AwsError::Upstream)
    } else {
        Ok(None)
    }
}

fn validate_event_headers(bytes: &[u8], value: &serde_json::Value) -> Result<(), AwsError> {
    let mut cursor = 0;
    let mut values = std::collections::HashMap::new();
    while cursor < bytes.len() {
        let name_len = *bytes.get(cursor).ok_or(AwsError::Upstream)? as usize;
        cursor += 1;
        let name = std::str::from_utf8(
            bytes
                .get(cursor..cursor + name_len)
                .ok_or(AwsError::Upstream)?,
        )
        .map_err(|_| AwsError::Upstream)?;
        cursor += name_len;
        if bytes.get(cursor) != Some(&7) {
            return Err(AwsError::Upstream);
        }
        cursor += 1;
        let len = u16::from_be_bytes(
            bytes
                .get(cursor..cursor + 2)
                .ok_or(AwsError::Upstream)?
                .try_into()
                .unwrap(),
        ) as usize;
        cursor += 2;
        let text = std::str::from_utf8(bytes.get(cursor..cursor + len).ok_or(AwsError::Upstream)?)
            .map_err(|_| AwsError::Upstream)?;
        cursor += len;
        if values.insert(name, text).is_some() {
            return Err(AwsError::Upstream);
        }
    }
    if values.get(":content-type") != Some(&"application/json") {
        return Err(AwsError::Upstream);
    }
    let message = *values.get(":message-type").ok_or(AwsError::Upstream)?;
    let kind = match message {
        "event" => *values.get(":event-type").ok_or(AwsError::Upstream)?,
        "exception" => *values.get(":exception-type").ok_or(AwsError::Upstream)?,
        _ => return Err(AwsError::Upstream),
    };
    if values.contains_key(if message == "event" {
        ":exception-type"
    } else {
        ":event-type"
    }) || value.get(kind).is_none()
    {
        return Err(AwsError::Upstream);
    }
    Ok(())
}

/// Decode AWS EventStream frames returned by Bedrock ConverseStream. CRCs are
/// validated before JSON is inspected, preventing truncated/corrupt frames from
/// being interpreted as model output.
pub fn decode_converse_stream(mut bytes: &[u8]) -> Result<Vec<ConverseEvent>, AwsError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 16 {
            return Err(AwsError::Upstream);
        }
        let total = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let headers = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if total < 16 || total > bytes.len() || headers > total - 16 {
            return Err(AwsError::Upstream);
        }
        if crc32(&bytes[..8]) != u32::from_be_bytes(bytes[8..12].try_into().unwrap())
            || crc32(&bytes[..total - 4])
                != u32::from_be_bytes(bytes[total - 4..total].try_into().unwrap())
        {
            return Err(AwsError::Upstream);
        }
        match decode_event_frame(&bytes[..total])? {
            Some(ConverseEvent::Done) if out.iter().any(|e| matches!(e, ConverseEvent::Done)) => {
                return Err(AwsError::Upstream);
            }
            Some(event) => out.push(event),
            None => {}
        }
        bytes = &bytes[total..];
    }
    if !matches!(out.last(), Some(ConverseEvent::Done)) {
        // EOF without messageStop is truncation, never successful completion.
        return Err(AwsError::Upstream);
    }
    if out
        .iter()
        .filter(|e| matches!(e, ConverseEvent::Done))
        .count()
        != 1
    {
        return Err(AwsError::Upstream);
    }
    Ok(out)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
impl AwsHttpApi {
    async fn send_signed(&self, q: SignedRequest) -> Result<serde_json::Value, AwsError> {
        let url = format!("https://{}{}", q.host, q.path);
        let mut b = match q.method {
            "GET" => self.http.get(url),
            _ => self.http.post(url).body(q.body),
        };
        for (k, v) in q.headers {
            b = b.header(k, v)
        }
        b.send()
            .await
            .map_err(|_| AwsError::Upstream)?
            .error_for_status()
            .map_err(|_| AwsError::Upstream)?
            .json()
            .await
            .map_err(|_| AwsError::Upstream)
    }
}

fn valid_model(v: &str) -> Result<&str, AwsError> {
    let v = v
        .strip_prefix("aws-bedrock/")
        .ok_or(AwsError::InvalidModel)?;
    if v.is_empty()
        || v.contains('/')
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        Err(AwsError::InvalidModel)
    } else {
        Ok(v)
    }
}
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key))
    } else {
        k[..key.len()].copy_from_slice(key)
    }
    let mut i = [0x36; 64];
    let mut o = [0x5c; 64];
    for x in 0..64 {
        i[x] ^= k[x];
        o[x] ^= k[x]
    }
    let inner = Sha256::new().chain_update(i).chain_update(data).finalize();
    Sha256::new()
        .chain_update(o)
        .chain_update(inner)
        .finalize()
        .to_vec()
}
fn hex(v: impl AsRef<[u8]>) -> String {
    v.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
pub fn sign_bedrock_request(
    region: &str,
    method: &str,
    path: &str,
    body: &[u8],
    c: &RoleCredentials,
    epoch: u64,
) -> Result<SignedRequest, AwsError> {
    if method != "GET" && method != "POST" {
        return Err(AwsError::Upstream);
    }
    let dt = chrono::DateTime::from_timestamp(epoch as i64, 0).ok_or(AwsError::Upstream)?;
    let date = dt.format("%Y%m%d").to_string();
    let amz = dt.format("%Y%m%dT%H%M%SZ").to_string();
    let runtime = path.starts_with("/model/");
    let host = if runtime {
        format!("bedrock-runtime.{region}.amazonaws.com")
    } else {
        format!("bedrock.{region}.amazonaws.com")
    };
    let payload = hex(Sha256::digest(body));
    let content_type = "application/json";
    let accepts_eventstream = runtime && path.ends_with("converse-stream");
    let canonical=format!("{method}\n{path}\n\ncontent-type:{content_type}\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{amz}\nx-amz-security-token:{}\n\ncontent-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token\n{payload}",c.session_token);
    let scope = format!("{date}/{region}/bedrock/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
        hex(Sha256::digest(canonical))
    );
    let kd = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
    let kr = hmac(&kd, region.as_bytes());
    let ks = hmac(&kr, b"bedrock");
    let key = hmac(&ks, b"aws4_request");
    let sig = hex(hmac(&key, sts.as_bytes()));
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), content_type.into());
    if accepts_eventstream {
        headers.insert("accept".into(), "application/vnd.amazon.eventstream".into());
    }
    headers.insert("host".into(), host.clone());
    headers.insert("x-amz-content-sha256".into(), payload);
    headers.insert("x-amz-date".into(), amz);
    headers.insert("x-amz-security-token".into(), c.session_token.clone());
    headers.insert("authorization".into(),format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature={sig}",c.access_key));
    Ok(SignedRequest {
        method: if method == "GET" { "GET" } else { "POST" },
        host,
        path: path.into(),
        body: body.into(),
        headers,
    })
}
