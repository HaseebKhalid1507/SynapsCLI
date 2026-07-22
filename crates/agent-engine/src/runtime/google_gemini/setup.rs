//! Google Gemini Code Assist project setup: bounded `loadCodeAssist` and
//! `onboardUser`/operations polling via the broker proxy.
//!
//! Experimental. `cloudcode-pa.googleapis.com/v1internal` is a
//! product-client-observed integration surface and is not documented as a
//! stable public third-party API; see docs/google-gemini-oauth-spec.md.
//!
//! This module never touches secrets or the network directly — it composes
//! typed `ProxyRequest` values through a `CredentialBroker`, so all upstream
//! egress goes through the broker's pinned host + path allowlist + timeouts +
//! body caps. Validation links returned by `loadCodeAssist` are surfaced to
//! the caller but never automatically followed.

use std::time::Duration;

use agent_core::auth::{BrokerError, CredentialBroker, ProxyMethod, ProxyRequest, ProxyResponse};
use serde::{Deserialize, Serialize};

// ── Wire types (subset needed for setup) ─────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ClientMetadata {
    #[serde(rename = "ideType")]
    pub ide_type: &'static str,
    pub platform: &'static str,
    #[serde(rename = "pluginType")]
    pub plugin_type: &'static str,
    #[serde(rename = "duetProject", skip_serializing_if = "Option::is_none")]
    pub duet_project: Option<String>,
}

impl ClientMetadata {
    pub fn baseline(duet_project: Option<String>) -> Self {
        Self {
            ide_type: "IDE_UNSPECIFIED",
            platform: "PLATFORM_UNSPECIFIED",
            plugin_type: "GEMINI",
            duet_project,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadCodeAssistRequest {
    #[serde(
        rename = "cloudaicompanionProject",
        skip_serializing_if = "Option::is_none"
    )]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct TierMetadata {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "hasOnboardedPreviously", default)]
    pub has_onboarded_previously: Option<bool>,
    #[serde(rename = "isDefault", default)]
    pub is_default: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct IneligibleTier {
    #[serde(rename = "tierId")]
    pub tier_id: Option<String>,
    #[serde(rename = "reasonCode")]
    pub reason_code: Option<String>,
    #[serde(rename = "reasonMessage")]
    pub reason_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoadCodeAssistResponse {
    #[serde(rename = "cloudaicompanionProject", default)]
    pub cloudaicompanion_project: Option<String>,
    #[serde(rename = "currentTier", default)]
    pub current_tier: Option<TierMetadata>,
    #[serde(rename = "allowedTiers", default)]
    pub allowed_tiers: Vec<TierMetadata>,
    #[serde(rename = "ineligibleTiers", default)]
    pub ineligible_tiers: Vec<IneligibleTier>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnboardUserRequest {
    #[serde(rename = "tierId")]
    pub tier_id: String,
    #[serde(
        rename = "cloudaicompanionProject",
        skip_serializing_if = "Option::is_none"
    )]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OperationResponse {
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub response: Option<OperationDone>,
    #[serde(default)]
    pub error: Option<OperationError>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OperationDone {
    #[serde(rename = "cloudaicompanionProject", default)]
    pub cloudaicompanion_project: Option<CloudCompanionProject>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudCompanionProject {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OperationError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

// ── Public: user-facing setup result ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserData {
    pub project_id: String,
    pub tier_id: String,
    pub tier_name: Option<String>,
    pub has_onboarded_previously: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// The user must provide a Google Cloud project id (non-free tier).
    ProjectIdRequired,
    /// One or more tiers are ineligible; message list is safe to display.
    IneligibleTiers(Vec<String>),
    /// Onboarding LRO returned an error status; message is upstream-owned.
    OperationFailed(String),
    /// Onboarding polling exhausted the bounded budget.
    OnboardingTimeout,
    /// Upstream returned a validation link that requires user action.
    ValidationRequired {
        link: Option<String>,
        description: Option<String>,
    },
    /// Broker/transport error surfaced verbatim (already secret-safe).
    Broker(String),
    /// Response body was not valid JSON of the expected shape.
    InvalidResponse,
    /// Numeric project id is not accepted (matches reference client).
    InvalidNumericProjectId,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProjectIdRequired => f.write_str(
                "google-gemini: no Cloud project id available. Set GOOGLE_CLOUD_PROJECT.",
            ),
            Self::IneligibleTiers(reasons) => {
                write!(
                    f,
                    "google-gemini: ineligible for available tiers: {}",
                    reasons.join("; ")
                )
            }
            Self::OperationFailed(msg) => {
                write!(f, "google-gemini: onboarding operation failed: {msg}")
            }
            Self::OnboardingTimeout => f.write_str("google-gemini: onboarding timed out"),
            Self::ValidationRequired { link, .. } => match link {
                Some(link) => write!(
                    f,
                    "google-gemini: account validation required. Visit: {link}"
                ),
                None => f.write_str("google-gemini: account validation required"),
            },
            Self::Broker(msg) => write!(f, "google-gemini: broker error: {msg}"),
            Self::InvalidResponse => f.write_str("google-gemini: invalid Code Assist response"),
            Self::InvalidNumericProjectId => {
                f.write_str("google-gemini: numeric project ids are not supported")
            }
        }
    }
}

impl From<BrokerError> for SetupError {
    fn from(err: BrokerError) -> Self {
        SetupError::Broker(err.to_string())
    }
}

// ── Bounded polling ──────────────────────────────────────────────────────────

/// Bounded onboarding polling budget. The reference client polls every 5s
/// indefinitely; we cap the total wait so tests and production never spin.
pub const ONBOARDING_MAX_ATTEMPTS: usize = 60; // ~5 minutes at 5s spacing
pub const ONBOARDING_POLL_INTERVAL: Duration = Duration::from_secs(5);

// ── High-level setup ─────────────────────────────────────────────────────────

/// Resolve the user's Code Assist project id and tier through the broker.
///
/// The `project_id_env` argument is what the caller reads from
/// `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID` — passing it in keeps
/// this module env-free and testable.
pub async fn setup_user<B: CredentialBroker + ?Sized>(
    broker: &B,
    project_id_env: Option<String>,
) -> Result<UserData, SetupError> {
    setup_user_with_sleeper(broker, project_id_env, tokio_sleeper).await
}

/// Same as `setup_user`, but the caller injects a sleeper (test seam). The
/// polling loop is bounded by ONBOARDING_MAX_ATTEMPTS regardless.
pub async fn setup_user_with_sleeper<B, F, Fut>(
    broker: &B,
    project_id_env: Option<String>,
    sleeper: F,
) -> Result<UserData, SetupError>
where
    B: CredentialBroker + ?Sized,
    F: Fn(Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if let Some(p) = &project_id_env {
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            return Err(SetupError::InvalidNumericProjectId);
        }
    }
    let load = load_code_assist(broker, project_id_env.clone()).await?;

    if let Some(current) = &load.current_tier {
        if let Some(project_id) = load
            .cloudaicompanion_project
            .clone()
            .or(project_id_env.clone())
        {
            return Ok(UserData {
                project_id,
                tier_id: current.id.clone().unwrap_or_else(|| "STANDARD".into()),
                tier_name: current.name.clone(),
                has_onboarded_previously: current.has_onboarded_previously.unwrap_or(true),
            });
        }
        // No project — surface ineligibility reasons if present.
        return Err(ineligibility_or_project_id(&load));
    }

    // Onboarding path.
    let tier = default_allowed_tier(&load).ok_or_else(|| ineligibility_or_project_id(&load))?;
    let tier_id = tier.id.clone().unwrap_or_else(|| "STANDARD".into());
    // Free tier onboarding must NOT include cloudaicompanionProject.
    let onboard_project = if tier_id == "FREE" {
        None
    } else {
        project_id_env.clone()
    };
    let onboard_req = OnboardUserRequest {
        tier_id: tier_id.clone(),
        cloudaicompanion_project: onboard_project.clone(),
        metadata: ClientMetadata::baseline(onboard_project.clone()),
    };
    let mut op = onboard_user(broker, &onboard_req).await?;
    if !op.done {
        let name = op
            .name
            .clone()
            .ok_or_else(|| SetupError::OperationFailed("missing operation name".into()))?;
        for _ in 0..ONBOARDING_MAX_ATTEMPTS {
            if op.done {
                break;
            }
            sleeper(ONBOARDING_POLL_INTERVAL).await;
            op = get_operation(broker, &name).await?;
        }
        if !op.done {
            return Err(SetupError::OnboardingTimeout);
        }
    }
    if let Some(err) = op.error {
        return Err(SetupError::OperationFailed(
            err.message.unwrap_or_else(|| "unspecified".into()),
        ));
    }
    let project_id = op
        .response
        .as_ref()
        .and_then(|r| r.cloudaicompanion_project.as_ref())
        .and_then(|p| p.id.clone())
        .or(project_id_env)
        .ok_or(SetupError::ProjectIdRequired)?;
    Ok(UserData {
        project_id,
        tier_id,
        tier_name: tier.name.clone(),
        has_onboarded_previously: tier.has_onboarded_previously.unwrap_or(false),
    })
}

async fn tokio_sleeper(d: Duration) {
    tokio::time::sleep(d).await;
}

fn default_allowed_tier(res: &LoadCodeAssistResponse) -> Option<&TierMetadata> {
    res.allowed_tiers
        .iter()
        .find(|t| t.is_default == Some(true))
}

fn ineligibility_or_project_id(res: &LoadCodeAssistResponse) -> SetupError {
    if res.ineligible_tiers.is_empty() {
        SetupError::ProjectIdRequired
    } else {
        SetupError::IneligibleTiers(
            res.ineligible_tiers
                .iter()
                .filter_map(|t| t.reason_message.clone())
                .collect(),
        )
    }
}

// ── Typed broker POSTs ───────────────────────────────────────────────────────

pub async fn load_code_assist<B: CredentialBroker + ?Sized>(
    broker: &B,
    project_id: Option<String>,
) -> Result<LoadCodeAssistResponse, SetupError> {
    let body = LoadCodeAssistRequest {
        cloudaicompanion_project: project_id.clone(),
        metadata: ClientMetadata::baseline(project_id),
    };
    let resp = broker
        .proxy(ProxyRequest {
            provider: "google-gemini".into(),
            method: ProxyMethod::Post,
            path: "/v1internal:loadCodeAssist".into(),
            body: Some(serde_json::to_value(&body).map_err(|_| SetupError::InvalidResponse)?),
            stream: false,
            body_bytes: None,
        })
        .await?;
    parse_success::<LoadCodeAssistResponse>(resp)
}

pub async fn onboard_user<B: CredentialBroker + ?Sized>(
    broker: &B,
    req: &OnboardUserRequest,
) -> Result<OperationResponse, SetupError> {
    let resp = broker
        .proxy(ProxyRequest {
            provider: "google-gemini".into(),
            method: ProxyMethod::Post,
            path: "/v1internal:onboardUser".into(),
            body: Some(serde_json::to_value(req).map_err(|_| SetupError::InvalidResponse)?),
            stream: false,
            body_bytes: None,
        })
        .await?;
    parse_success::<OperationResponse>(resp)
}

pub async fn get_operation<B: CredentialBroker + ?Sized>(
    broker: &B,
    operation_name: &str,
) -> Result<OperationResponse, SetupError> {
    // Match the reference client's `getOperationUrl(name) = ${base}/${name}`
    // and its `getOperation` = HTTP GET. `name` is the full LRO resource name
    // (e.g. `operations/op-42` or nested `operations/projects/{p}/op-xyz`)
    // and must be embedded verbatim after the API version — stripping the
    // `operations/` prefix or rewriting interior slashes yields HTTP 404
    // from cloudcode-pa.
    //
    // We keep a defensive validator so a malformed/hostile name cannot be
    // used to escape the pinned allowlist prefix `/v1internal/operations/`
    // enforced by the broker.
    if !is_safe_operation_name(operation_name) {
        return Err(SetupError::InvalidResponse);
    }
    let path = format!("/v1internal/{operation_name}");
    let resp = broker
        .proxy(ProxyRequest {
            provider: "google-gemini".into(),
            method: ProxyMethod::Get,
            path,
            body: None,
            stream: false,
            body_bytes: None,
        })
        .await?;
    parse_success::<OperationResponse>(resp)
}

/// Accept only Google LRO names of the form `operations/<segment>[/<segment>…]`
/// where each segment is non-empty, does not equal `..`, and contains no
/// URL-reserved characters that could break the pinned host + path allowlist.
fn is_safe_operation_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("operations/") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    for segment in rest.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
        if segment
            .chars()
            .any(|c| matches!(c, '?' | '#' | '\\' | ' ' | '\t' | '\n' | '\r'))
        {
            return false;
        }
    }
    true
}

fn parse_success<T: for<'de> Deserialize<'de>>(resp: ProxyResponse) -> Result<T, SetupError> {
    if !(200..300).contains(&resp.status) {
        return Err(SetupError::OperationFailed(format!(
            "HTTP {} from cloudcode-pa",
            resp.status
        )));
    }
    serde_json::from_str::<T>(&resp.body).map_err(|_| SetupError::InvalidResponse)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::auth::{AccessToken, ProxyByteStream};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct FakeBroker {
        seen: Arc<Mutex<Vec<ProxyRequest>>>,
        responses: Mutex<Vec<Result<ProxyResponse, BrokerError>>>,
    }

    impl FakeBroker {
        fn new(responses: Vec<Result<ProxyResponse, BrokerError>>) -> Self {
            Self {
                seen: Arc::default(),
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl CredentialBroker for FakeBroker {
        async fn access_token(
            &self,
            _p: agent_core::auth::OAuthProviderId,
        ) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("test".into()))
        }
        async fn proxy(&self, request: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            self.seen.lock().unwrap().push(request);
            self.responses.lock().unwrap().remove(0)
        }
        async fn proxy_stream(
            &self,
            _request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in fake".into()))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn ok(body: &str) -> Result<ProxyResponse, BrokerError> {
        Ok(ProxyResponse {
            status: 200,
            body: body.to_string(),
        })
    }

    async fn no_sleep(_: Duration) {}

    #[tokio::test]
    async fn setup_returns_existing_project_when_current_tier_present() {
        let broker = FakeBroker::new(vec![ok(r#"{"cloudaicompanionProject":"my-proj",
                "currentTier":{"id":"STANDARD","name":"Standard","hasOnboardedPreviously":true}}"#)]);
        let data = setup_user_with_sleeper(&broker, None, no_sleep)
            .await
            .unwrap();
        assert_eq!(data.project_id, "my-proj");
        assert_eq!(data.tier_id, "STANDARD");
        assert!(data.has_onboarded_previously);

        // Wire request went to the pinned path with metadata baseline.
        let seen = broker.seen.lock().unwrap();
        assert_eq!(seen[0].provider, "google-gemini");
        assert_eq!(seen[0].path, "/v1internal:loadCodeAssist");
        assert_eq!(seen[0].method, ProxyMethod::Post);
        let body = seen[0].body.as_ref().unwrap();
        assert_eq!(body["metadata"]["ideType"], "IDE_UNSPECIFIED");
        assert_eq!(body["metadata"]["pluginType"], "GEMINI");
    }

    #[tokio::test]
    async fn setup_uses_env_project_when_response_omits_it() {
        let broker = FakeBroker::new(vec![ok(
            r#"{"currentTier":{"id":"STANDARD","name":"Standard"}}"#,
        )]);
        let data = setup_user_with_sleeper(&broker, Some("env-proj".into()), no_sleep)
            .await
            .unwrap();
        assert_eq!(data.project_id, "env-proj");
        assert_eq!(data.tier_id, "STANDARD");
    }

    #[tokio::test]
    async fn setup_reports_ineligibility_when_no_project_and_no_current_tier() {
        let broker = FakeBroker::new(vec![ok(
            r#"{"ineligibleTiers":[{"tierId":"FREE","reasonMessage":"not available in your region"}]}"#,
        )]);
        match setup_user_with_sleeper(&broker, None, no_sleep).await {
            Err(SetupError::IneligibleTiers(msgs)) => {
                assert_eq!(msgs, vec!["not available in your region".to_string()]);
            }
            other => panic!("expected IneligibleTiers, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn setup_onboards_free_tier_with_no_project_and_polls_operation() {
        let broker = FakeBroker::new(vec![
            // loadCodeAssist: no currentTier, one allowedTier default FREE.
            ok(r#"{"allowedTiers":[{"id":"FREE","name":"Free","isDefault":true}]}"#),
            // onboardUser: LRO pending, name returned.
            ok(r#"{"done":false,"name":"operations/op-42"}"#),
            // getOperation poll #1: still pending.
            ok(r#"{"done":false,"name":"operations/op-42"}"#),
            // getOperation poll #2: done, returns project id.
            ok(r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"managed-proj"}}}"#),
        ]);
        let data = setup_user_with_sleeper(&broker, None, no_sleep)
            .await
            .unwrap();
        assert_eq!(data.project_id, "managed-proj");
        assert_eq!(data.tier_id, "FREE");

        let seen = broker.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].path, "/v1internal:loadCodeAssist");
        assert_eq!(seen[1].path, "/v1internal:onboardUser");
        // FREE tier must NOT carry cloudaicompanionProject in onboardUser.
        let onboard_body = seen[1].body.as_ref().unwrap();
        assert!(
            onboard_body.get("cloudaicompanionProject").is_none()
                || onboard_body["cloudaicompanionProject"].is_null()
        );
        assert_eq!(seen[2].path, "/v1internal/operations/op-42");
        assert_eq!(seen[3].path, "/v1internal/operations/op-42");
    }

    #[tokio::test]
    async fn setup_rejects_numeric_project_id() {
        let broker = FakeBroker::new(vec![]);
        assert_eq!(
            setup_user_with_sleeper(&broker, Some("12345".into()), no_sleep).await,
            Err(SetupError::InvalidNumericProjectId)
        );
        // No wire call must have happened.
        assert!(broker.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn setup_surfaces_operation_error_secret_safely() {
        let broker = FakeBroker::new(vec![
            ok(r#"{"allowedTiers":[{"id":"STANDARD","name":"Std","isDefault":true}]}"#),
            ok(r#"{"done":true,"error":{"code":13,"message":"internal"}}"#),
        ]);
        match setup_user_with_sleeper(&broker, Some("proj-x".into()), no_sleep).await {
            Err(SetupError::OperationFailed(msg)) => assert!(msg.contains("internal")),
            other => panic!("expected OperationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn setup_bounded_polling_times_out_instead_of_spinning() {
        // Enough poll responses to trigger the bound; supply pending forever.
        let mut responses: Vec<Result<ProxyResponse, BrokerError>> = vec![
            ok(r#"{"allowedTiers":[{"id":"STANDARD","name":"Std","isDefault":true}]}"#),
            ok(r#"{"done":false,"name":"operations/op-slow"}"#),
        ];
        for _ in 0..ONBOARDING_MAX_ATTEMPTS + 5 {
            responses.push(ok(r#"{"done":false,"name":"operations/op-slow"}"#));
        }
        let broker = FakeBroker::new(responses);
        let err = setup_user_with_sleeper(&broker, Some("proj".into()), no_sleep)
            .await
            .unwrap_err();
        assert_eq!(err, SetupError::OnboardingTimeout);
    }

    #[test]
    fn get_operation_rejects_traversal_and_absolute_paths() {
        // The path builder must reject traversal/absolute-path tokens, but
        // MUST accept valid Google LRO names which begin with `operations/`
        // and may contain additional slash-separated segments (e.g.
        // `projects/{p}/operations/{o}`), matching the reference client's
        // `getOperationUrl(name)` = `${base}/${name}` construction.
        let broker = FakeBroker::new(vec![]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for bad in [
            "",                  // empty
            "../secret",         // does not start with operations/
            "op-42",             // missing operations/ prefix
            "operations/",       // trailing empty segment
            "operations/../x",   // traversal
            "operations/x/../y", // traversal mid-name
            "operations//x",     // empty segment
            "/operations/x",     // absolute path
            "operations/x?y=1",  // query character
            "operations/x#frag", // fragment character
        ] {
            let r = rt.block_on(get_operation(&broker, bad));
            assert!(
                matches!(r, Err(SetupError::InvalidResponse)),
                "must reject {bad:?}"
            );
        }
        // No wire calls emitted for any bad name.
        assert!(broker.seen.lock().unwrap().is_empty());
    }

    /// Regression: matches the reference client's `getOperation` contract.
    /// The wire request MUST be an HTTP GET (not POST), MUST carry no body,
    /// and MUST embed the full LRO `name` verbatim after the API version —
    /// including any interior slashes for nested resource names. Failing any
    /// of these produces `HTTP 404 from cloudcode-pa` in production.
    #[tokio::test]
    async fn get_operation_matches_reference_wire_contract() {
        // Simple LRO name.
        let broker = FakeBroker::new(vec![ok(r#"{"done":true}"#)]);
        let _ = get_operation(&broker, "operations/op-42").await.unwrap();
        let seen = broker.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].provider, "google-gemini");
        assert_eq!(
            seen[0].method,
            ProxyMethod::Get,
            "reference uses GET for getOperation; POST returns 404 from cloudcode-pa"
        );
        assert_eq!(seen[0].path, "/v1internal/operations/op-42");
        assert!(
            seen[0].body.is_none(),
            "GET must not carry a body; reference sends none"
        );
        drop(seen);

        // Nested LRO name — must be preserved verbatim, not stripped.
        let broker = FakeBroker::new(vec![ok(r#"{"done":true}"#)]);
        let _ = get_operation(&broker, "operations/projects/my-proj/op-xyz")
            .await
            .unwrap();
        let seen = broker.seen.lock().unwrap();
        assert_eq!(
            seen[0].path,
            "/v1internal/operations/projects/my-proj/op-xyz"
        );
        assert_eq!(seen[0].method, ProxyMethod::Get);
    }
}
