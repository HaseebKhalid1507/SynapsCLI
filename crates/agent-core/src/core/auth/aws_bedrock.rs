//! Provider-local IAM Identity Center and Bedrock broker boundary.
//! The transport trait is intentionally typed: it cannot sign arbitrary URLs or vend credentials.
use super::cloud::{AwsBedrockConfig, InvokeRequest};
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};

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
    async fn converse_stream(&self, request: SignedRequest)
        -> Result<Vec<ConverseEvent>, AwsError>;
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
    ) -> Result<Vec<ConverseEvent>, AwsError> {
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
    async fn converse_stream(&self, _: SignedRequest) -> Result<Vec<ConverseEvent>, AwsError> {
        Err(AwsError::Upstream)
    }
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
    let canonical=format!("{method}\n{path}\n\nhost:{host}\nx-amz-date:{amz}\nx-amz-security-token:{}\n\nhost;x-amz-date;x-amz-security-token\n{payload}",c.session_token);
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
    headers.insert("host".into(), host.clone());
    headers.insert("x-amz-date".into(), amz);
    headers.insert("x-amz-security-token".into(), c.session_token.clone());
    headers.insert("authorization".into(),format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature={sig}",c.access_key));
    Ok(SignedRequest {
        method: if method == "GET" { "GET" } else { "POST" },
        host,
        path: path.into(),
        body: body.into(),
        headers,
    })
}
