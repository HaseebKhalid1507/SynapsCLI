//! Provider-local Azure OpenAI OAuth, catalog, and broker request contracts.
//! No function in this module performs network I/O or returns credentials.
use super::AzureOpenAiConfig;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt};

pub const ARM_API_VERSION: &str = "2024-10-01";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureError {
    code: &'static str,
    message: String,
}
impl AzureError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn code(&self) -> &'static str {
        self.code
    }
}
impl fmt::Display for AzureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for AzureError {}
type Result<T> = std::result::Result<T, AzureError>;

#[derive(Clone, PartialEq, Eq)]
pub struct AzureRegistration {
    client_id: String,
}
impl fmt::Debug for AzureRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureRegistration")
            .field("client_id", &"[configured]")
            .finish()
    }
}
impl AzureRegistration {
    pub fn production(client_id: Option<String>) -> Result<Self> {
        match client_id.filter(|v| !v.trim().is_empty()) { Some(id) => Self::validated(id), None => Err(AzureError::new("registration_required", "configure a Synaps-owned Microsoft Entra public-client application ID with device-code flow enabled; no client secret is used")) }
    }
    #[doc(hidden)]
    pub fn test(client_id: &str) -> Result<Self> {
        Self::validated(client_id.to_owned())
    }
    fn validated(client_id: String) -> Result<Self> {
        if !guid(&client_id) {
            return Err(AzureError::new(
                "invalid_client_id",
                "public client ID must be a UUID",
            ));
        }
        Ok(Self { client_id })
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}
fn guid(v: &str) -> bool {
    let p: Vec<_> = v.split('-').collect();
    p.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(p)
            .all(|(n, s)| s.len() == *n && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn segment(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AzureAudience {
    Arm,
    Inference,
}
impl AzureAudience {
    pub const fn scope(self) -> &'static str {
        match self {
            Self::Arm => "https://management.azure.com/.default",
            Self::Inference => "https://cognitiveservices.azure.com/.default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormRequest {
    pub method: &'static str,
    pub url: String,
    pub form: Vec<(String, String)>,
}
pub fn device_code_request(
    config: &AzureOpenAiConfig,
    registration: &AzureRegistration,
) -> Result<FormRequest> {
    if config.tenant == "common" || !segment(&config.tenant) {
        return Err(AzureError::new(
            "invalid_tenant",
            "use a tenant UUID or organizations",
        ));
    }
    Ok(FormRequest {
        method: "POST",
        url: format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
            config.tenant
        ),
        form: vec![
            ("client_id".into(), registration.client_id.clone()),
            ("scope".into(), AzureAudience::Arm.scope().into()),
        ],
    })
}
pub fn refresh_request(
    config: &AzureOpenAiConfig,
    registration: &AzureRegistration,
    audience: AzureAudience,
    refresh_token: &str,
) -> Result<FormRequest> {
    let mut r = device_code_request(config, registration)?;
    r.url = r.url.replace("/devicecode", "/token");
    r.form = vec![
        ("client_id".into(), registration.client_id.clone()),
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.into()),
        ("scope".into(), audience.scope().into()),
    ];
    Ok(r)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollReply {
    AuthorizationPending,
    SlowDown,
    Denied,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollAction {
    Sleep(u64),
}
pub struct DevicePoll {
    interval: u64,
    expires_at: u64,
}
impl DevicePoll {
    pub fn new(interval: u64, issued_at: u64, expires_in: u64) -> Self {
        Self {
            interval: interval.max(1),
            expires_at: issued_at.saturating_add(expires_in),
        }
    }
    pub fn apply(&mut self, now: u64, cancelled: bool, reply: PollReply) -> Result<PollAction> {
        if cancelled {
            return Err(AzureError::new(
                "cancelled",
                "device authorization cancelled",
            ));
        }
        if now >= self.expires_at {
            return Err(AzureError::new(
                "device_code_expired",
                "device code expired",
            ));
        }
        match reply {
            PollReply::AuthorizationPending => Ok(PollAction::Sleep(self.interval)),
            PollReply::SlowDown => {
                self.interval = self.interval.saturating_add(5);
                Ok(PollAction::Sleep(self.interval))
            }
            PollReply::Denied => Err(AzureError::new("access_denied", "authorization denied")),
        }
    }
}

#[derive(Clone)]
pub struct TokenGrant {
    access: String,
    refresh: Option<String>,
    expires_at: u64,
}
impl TokenGrant {
    pub fn new(access: impl Into<String>, refresh: Option<String>, expires_at: u64) -> Self {
        Self {
            access: access.into(),
            refresh,
            expires_at,
        }
    }
}
#[derive(Clone, Serialize)]
pub struct AzureTokenSet {
    #[serde(skip)]
    refresh: String,
    #[serde(skip)]
    arm: Option<TokenGrant>,
    #[serde(skip)]
    inference: Option<TokenGrant>,
}
impl fmt::Debug for AzureTokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AzureTokenSet { credentials: [REDACTED] }")
    }
}
impl AzureTokenSet {
    pub fn new(refresh: impl Into<String>) -> Self {
        Self {
            refresh: refresh.into(),
            arm: None,
            inference: None,
        }
    }
    pub fn commit(&mut self, a: AzureAudience, mut g: TokenGrant) {
        if let Some(r) = g.refresh.take() {
            self.refresh = r;
        }
        match a {
            AzureAudience::Arm => self.arm = Some(g),
            AzureAudience::Inference => self.inference = Some(g),
        }
    }
    pub fn access(&self, a: AzureAudience, now: u64) -> Result<&str> {
        let g = match a {
            AzureAudience::Arm => &self.arm,
            AzureAudience::Inference => &self.inference,
        }
        .as_ref()
        .ok_or_else(|| AzureError::new("login_required", "audience token absent"))?;
        if now >= g.expires_at {
            return Err(AzureError::new("token_expired", "audience token expired"));
        }
        Ok(&g.access)
    }
    pub fn refresh_token(&self) -> &str {
        &self.refresh
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureEndpoint(String);
impl AzureEndpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let u = url::Url::parse(value)
            .map_err(|_| AzureError::new("invalid_endpoint", "invalid Azure endpoint"))?;
        let host = u.host_str().unwrap_or_default();
        if u.scheme() != "https"
            || u.port().is_some()
            || u.path() != "/"
            || u.query().is_some()
            || !(host.ends_with(".openai.azure.com")
                || host.ends_with(".cognitiveservices.azure.com"))
        {
            return Err(AzureError::new(
                "invalid_endpoint",
                "endpoint must be an exact Azure OpenAI/Cognitive Services HTTPS origin",
            ));
        }
        Ok(Self(format!("https://{host}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub display_name: String,
    pub source: &'static str,
    pub model: Option<String>,
    pub version: Option<String>,
}
#[derive(Deserialize)]
struct Page {
    value: Vec<Deployment>,
    #[serde(rename = "nextLink")]
    next_link: Option<String>,
}
#[derive(Deserialize)]
struct Deployment {
    name: String,
    properties: Properties,
}
#[derive(Deserialize)]
struct Properties {
    model: Option<Model>,
    #[serde(rename = "provisioningState")]
    state: Option<String>,
}
#[derive(Deserialize)]
struct Model {
    name: String,
    version: Option<String>,
}
pub struct DeploymentDiscovery {
    config: AzureOpenAiConfig,
    max_pages: usize,
    max_entries: usize,
    pages: usize,
    seen: HashSet<String>,
    entries: Vec<CatalogEntry>,
}
impl DeploymentDiscovery {
    pub fn new(config: AzureOpenAiConfig, max_pages: usize, max_entries: usize) -> Self {
        Self {
            config,
            max_pages,
            max_entries,
            pages: 0,
            seen: HashSet::new(),
            entries: vec![],
        }
    }
    pub fn initial_url(&self) -> String {
        format!("https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.CognitiveServices/accounts/{}/deployments?api-version={ARM_API_VERSION}",self.config.subscription_id,self.config.resource_group,self.config.resource_name)
    }
    pub fn accept_page(&mut self, body: &str) -> Result<Option<String>> {
        if body.len() > 1024 * 1024 || self.pages >= self.max_pages {
            return Err(AzureError::new(
                "catalog_limit",
                "ARM catalog limit exceeded",
            ));
        }
        self.pages += 1;
        let p: Page = serde_json::from_str(body).map_err(|_| {
            AzureError::new("malformed_catalog", "malformed ARM deployments response")
        })?;
        for d in p.value {
            if d.properties.state.as_deref() != Some("Succeeded") || !segment(&d.name) {
                continue;
            }
            if self.seen.insert(d.name.clone()) {
                if self.entries.len() >= self.max_entries {
                    return Err(AzureError::new(
                        "catalog_limit",
                        "deployment limit exceeded",
                    ));
                }
                let (m, v) = d
                    .properties
                    .model
                    .map(|m| (Some(m.name), m.version))
                    .unwrap_or_default();
                self.entries.push(CatalogEntry {
                    id: format!("azure-openai/{}", d.name),
                    display_name: d.name,
                    source: "dynamic",
                    model: m,
                    version: v,
                });
            }
        }
        match p.next_link {
            Some(n) => {
                let u = url::Url::parse(&n)
                    .map_err(|_| AzureError::new("invalid_pagination", "invalid ARM nextLink"))?;
                if u.scheme() != "https"
                    || u.host_str() != Some("management.azure.com")
                    || !u.path().starts_with("/subscriptions/")
                {
                    return Err(AzureError::new(
                        "invalid_pagination",
                        "ARM nextLink escaped fixed authority/path",
                    ));
                }
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }
    pub fn finish(mut self) -> Result<Vec<CatalogEntry>> {
        if self.entries.is_empty() {
            return Err(AzureError::new(
                "empty_catalog",
                "no callable Azure OpenAI deployments found",
            ));
        }
        self.entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(self.entries)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRequest {
    pub method: &'static str,
    pub url: String,
    pub body: Vec<u8>,
}
pub fn responses_request(
    endpoint: &AzureEndpoint,
    deployment: &str,
    body: &[u8],
) -> Result<RuntimeRequest> {
    if !segment(deployment) || body.len() > 4 * 1024 * 1024 {
        return Err(AzureError::new(
            "invalid_request",
            "invalid deployment or oversized body",
        ));
    }
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| AzureError::new("invalid_request", "Responses body must be JSON"))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| AzureError::new("invalid_request", "Responses body must be an object"))?;
    obj.insert("model".into(), serde_json::Value::String(deployment.into()));
    let body = serde_json::to_vec(&value).unwrap();
    Ok(RuntimeRequest {
        method: "POST",
        url: format!("{}/openai/v1/responses", endpoint.0),
        body,
    })
}
