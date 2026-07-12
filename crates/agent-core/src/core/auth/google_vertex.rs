//! Provider-local Google Vertex AI installed-app OAuth primitives.
use serde::Deserialize;
use std::fmt;
use url::Url;

pub const PROVIDER: &str = "google-vertex";
pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[derive(Clone, PartialEq, Eq)]
pub struct VertexRegistration {
    client_id: String,
}
impl VertexRegistration {
    pub fn new(client_id: Option<&str>) -> Result<Self, VertexError> {
        let client_id = client_id
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(VertexError::RegistrationRequired)?;
        if !client_id.ends_with(".apps.googleusercontent.com") || client_id.contains('/') {
            return Err(VertexError::InvalidRegistration);
        }
        Ok(Self {
            client_id: client_id.into(),
        })
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}
impl fmt::Debug for VertexRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexRegistration")
            .field("client_id", &"[configured]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexContext {
    project: String,
    location: String,
}
impl VertexContext {
    pub fn new(project: &str, location: &str) -> Result<Self, VertexError> {
        if !valid_project(project) {
            return Err(VertexError::InvalidProject);
        }
        if !valid_location(location) {
            return Err(VertexError::InvalidLocation);
        }
        Ok(Self {
            project: project.into(),
            location: location.into(),
        })
    }
    pub fn project(&self) -> &str {
        &self.project
    }
    pub fn location(&self) -> &str {
        &self.location
    }
    pub fn host(&self) -> String {
        format!("{}-aiplatform.googleapis.com", self.location)
    }
}
fn valid_project(v: &str) -> bool {
    (6..=63).contains(&v.len())
        && v.starts_with(|c: char| c.is_ascii_lowercase())
        && v.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && v.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
fn valid_location(v: &str) -> bool {
    (2..=32).contains(&v.len())
        && v.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !v.starts_with('-')
        && !v.ends_with('-')
}

pub struct VertexCredentials {
    access: String,
    refresh: String,
    expires: u64,
}
impl VertexCredentials {
    pub fn new(access: impl Into<String>, refresh: impl Into<String>, expires: u64) -> Self {
        Self {
            access: access.into(),
            refresh: refresh.into(),
            expires,
        }
    }
    pub fn access_token(&self) -> &str {
        &self.access
    }
    pub fn refresh_token(&self) -> &str {
        &self.refresh
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires
    }
}
impl fmt::Debug for VertexCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexCredentials")
            .field("access", &"[REDACTED]")
            .field("refresh", &"[REDACTED]")
            .field("expires", &self.expires)
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VertexError {
    #[error("google-vertex: Synaps Google Desktop OAuth client registration required; configure the broker-owned client ID")]
    RegistrationRequired,
    #[error("google-vertex: invalid Desktop OAuth client registration")]
    InvalidRegistration,
    #[error("google-vertex: invalid project")]
    InvalidProject,
    #[error("google-vertex: invalid location")]
    InvalidLocation,
    #[error("google-vertex: untrusted OAuth endpoint")]
    UntrustedEndpoint,
    #[error("google-vertex: invalid token response")]
    InvalidTokenResponse,
}
impl VertexError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RegistrationRequired => "registration_required",
            Self::InvalidRegistration => "invalid_registration",
            Self::InvalidProject => "invalid_project",
            Self::InvalidLocation => "invalid_location",
            Self::UntrustedEndpoint => "untrusted_endpoint",
            Self::InvalidTokenResponse => "invalid_token_response",
        }
    }
}

pub fn build_authorize_url(
    reg: &VertexRegistration,
    challenge: &str,
    state: &str,
    redirect: &str,
) -> Result<Url, VertexError> {
    let redirect = Url::parse(redirect).map_err(|_| VertexError::UntrustedEndpoint)?;
    if redirect.scheme() != "http"
        || redirect.host_str() != Some("127.0.0.1")
        || redirect.port().is_none()
        || redirect.path() != "/oauth2callback"
    {
        return Err(VertexError::UntrustedEndpoint);
    }
    let mut url = Url::parse(AUTHORIZE_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("client_id", reg.client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect.as_str())
        .append_pair("scope", CLOUD_PLATFORM_SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

pub fn validate_oauth_endpoint(value: &str, test_loopback: bool) -> Result<Url, VertexError> {
    let url = Url::parse(value).map_err(|_| VertexError::UntrustedEndpoint)?;
    let production =
        (url.as_str() == TOKEN_URL || url.as_str() == AUTHORIZE_URL) && url.scheme() == "https";
    let seam = test_loopback && url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
    if production || seam {
        Ok(url)
    } else {
        Err(VertexError::UntrustedEndpoint)
    }
}

#[derive(Deserialize)]
struct TokenWire {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}
pub fn parse_token_response(
    body: &[u8],
    previous: Option<&VertexCredentials>,
    now_ms: u64,
) -> Result<VertexCredentials, VertexError> {
    if body.len() > 64 * 1024 {
        return Err(VertexError::InvalidTokenResponse);
    }
    let wire: TokenWire =
        serde_json::from_slice(body).map_err(|_| VertexError::InvalidTokenResponse)?;
    if wire.access_token.is_empty() {
        return Err(VertexError::InvalidTokenResponse);
    }
    let refresh = wire
        .refresh_token
        .filter(|v| !v.is_empty())
        .or_else(|| previous.map(|v| v.refresh.clone()))
        .ok_or(VertexError::InvalidTokenResponse)?;
    Ok(VertexCredentials::new(
        wire.access_token,
        refresh,
        now_ms.saturating_add(wire.expires_in.saturating_mul(1000)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_and_registration_are_distinct_from_gemini_cli() {
        assert_eq!(PROVIDER, "google-vertex");
        let error = VertexRegistration::new(None).unwrap_err();
        assert_eq!(error.code(), "registration_required");
        assert!(!error.to_string().contains("681255809395"));
    }
    #[test]
    fn validates_context_before_network() {
        assert!(VertexContext::new("my-project-123", "us-central1").is_ok());
        for project in ["", "UPPER", "-bad", "bad_", "a"] {
            assert!(
                VertexContext::new(project, "us-central1").is_err(),
                "{project}"
            );
        }
        for location in ["", "US-CENTRAL1", "../evil", "us_central1"] {
            assert!(
                VertexContext::new("my-project-123", location).is_err(),
                "{location}"
            );
        }
    }
    #[test]
    fn authorization_is_pkce_offline_and_exact_scope() {
        let registration =
            VertexRegistration::new(Some("synaps.apps.googleusercontent.com")).unwrap();
        let url = build_authorize_url(
            &registration,
            "challenge",
            "state",
            "http://127.0.0.1:3210/oauth2callback",
        )
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["access_type"], "offline");
        assert_eq!(query["state"], "state");
        assert!(!query.contains_key("client_secret"));
    }
    #[test]
    fn refresh_rotation_preserves_omitted_refresh_token() {
        let old = VertexCredentials::new("access-old", "refresh-old", 1);
        let next = parse_token_response(
            br#"{"access_token":"access-new","expires_in":3600}"#,
            Some(&old),
            10,
        )
        .unwrap();
        assert_eq!(next.refresh_token(), "refresh-old");
        assert_eq!(next.access_token(), "access-new");
        let rotated = parse_token_response(
            br#"{"access_token":"a","refresh_token":"refresh-new","expires_in":2}"#,
            Some(&old),
            10,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token(), "refresh-new");
    }
    #[test]
    fn only_exact_oauth_endpoints_and_loopback_test_seam_pass() {
        assert!(validate_oauth_endpoint(TOKEN_URL, false).is_ok());
        assert!(
            validate_oauth_endpoint("https://oauth2.googleapis.com.evil.test/token", false)
                .is_err()
        );
        assert!(validate_oauth_endpoint("http://127.0.0.1:9000/token", true).is_ok());
        assert!(validate_oauth_endpoint("http://10.0.0.1/token", true).is_err());
    }
}
