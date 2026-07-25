//! Shared, credential-free cloud provider contracts.
//!
//! Provider-local modules consume these types. They intentionally contain no
//! tokens, keys, authorization headers, absolute runtime URLs, or signing inputs.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

use super::provider::OAuthProviderId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloudProviderId {
    AzureOpenAi,
    AwsBedrock,
    GoogleVertex,
}

impl CloudProviderId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AzureOpenAi => "azure-openai",
            Self::AwsBedrock => "aws-bedrock",
            Self::GoogleVertex => "google-vertex",
        }
    }
    /// Whether this cloud route supports tool use. Cloud broker routes are
    /// text-only until full tool translation exists (spec §5.5); the value is
    /// sourced from the provider descriptor so listing and enforcement agree.
    pub fn supports_tools(self) -> bool {
        CLOUD_PROVIDER_DESCRIPTORS
            .iter()
            .find(|d| d.id == self)
            .is_some_and(|d| d.supports_tools)
    }
}
impl fmt::Display for CloudProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for CloudProviderId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "azure-openai" => Ok(Self::AzureOpenAi),
            "aws-bedrock" => Ok(Self::AwsBedrock),
            "google-vertex" => Ok(Self::GoogleVertex),
            _ => Err(format!("unknown canonical cloud provider id: {s}")),
        }
    }
}

/// Persist a credential-free cloud route while retaining the broker's opaque context.
/// The context is not interpreted outside the broker and contains no account identity.
pub fn qualify_model_route(model_id: &str, context_ref: &str) -> Result<String, String> {
    if !context_ref.starts_with("ctx-")
        || context_ref.len() > 128
        || !context_ref
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || model_id.contains("#synaps-context=")
    {
        return Err("invalid opaque cloud model context".into());
    }
    Ok(format!("{model_id}#synaps-context={context_ref}"))
}

pub fn split_model_route(route: &str) -> (&str, Option<&str>) {
    route
        .rsplit_once("#synaps-context=")
        .map_or((route, None), |(model, context)| (model, Some(context)))
}

/// General auth identity. Existing OAuth identity remains strongly typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    OAuth(OAuthProviderId),
    Cloud(CloudProviderId),
}
impl TryFrom<&str> for ProviderId {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse::<OAuthProviderId>()
            .map(Self::OAuth)
            .or_else(|_| s.parse::<CloudProviderId>().map(Self::Cloud))
    }
}

/// Credential mechanism used behind the broker boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentity {
    OAuthBearer,
    AwsTemporaryCredentials,
}
impl AuthIdentity {
    pub const fn for_provider(provider: ProviderId) -> Self {
        match provider {
            ProviderId::Cloud(CloudProviderId::AwsBedrock) => Self::AwsTemporaryCredentials,
            _ => Self::OAuthBearer,
        }
    }
}

fn simple(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        Err(format!("invalid {name}"))
    } else {
        Ok(())
    }
}
fn region(value: &str) -> Result<(), String> {
    if value.len() >= 3
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value.contains('-')
    {
        Ok(())
    } else {
        Err("invalid region/location".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureOpenAiConfig {
    pub tenant: String,
    pub subscription_id: String,
    pub resource_group: String,
    pub resource_name: String,
    pub deployment: String,
}
impl AzureOpenAiConfig {
    pub fn new(
        tenant: impl Into<String>,
        subscription_id: impl Into<String>,
        resource_group: impl Into<String>,
        resource_name: impl Into<String>,
        deployment: impl Into<String>,
    ) -> Result<Self, String> {
        let v = Self {
            tenant: tenant.into(),
            subscription_id: subscription_id.into(),
            resource_group: resource_group.into(),
            resource_name: resource_name.into(),
            deployment: deployment.into(),
        };
        if v.tenant == "common" {
            return Err("tenant 'common' is not allowed".into());
        }
        for (x, n) in [
            (&v.tenant, "tenant"),
            (&v.subscription_id, "subscription id"),
            (&v.resource_group, "resource group"),
            (&v.resource_name, "resource name"),
            (&v.deployment, "deployment"),
        ] {
            simple(x, n)?;
        }
        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsBedrockConfig {
    pub sso_start_url: String,
    pub sso_region: String,
    pub account_id: String,
    pub role_name: String,
    pub bedrock_region: String,
}
impl AwsBedrockConfig {
    pub fn new(
        sso_start_url: impl Into<String>,
        sso_region: impl Into<String>,
        account_id: impl Into<String>,
        role_name: impl Into<String>,
        bedrock_region: impl Into<String>,
    ) -> Result<Self, String> {
        let v = Self {
            sso_start_url: sso_start_url.into(),
            sso_region: sso_region.into(),
            account_id: account_id.into(),
            role_name: role_name.into(),
            bedrock_region: bedrock_region.into(),
        };
        let url = url::Url::parse(&v.sso_start_url).map_err(|_| "invalid SSO start URL")?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err("SSO start URL must be HTTPS".into());
        }
        region(&v.sso_region)?;
        region(&v.bedrock_region)?;
        if v.account_id.len() != 12 || !v.account_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid AWS account id".into());
        }
        simple(&v.role_name, "role name")?;
        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleVertexConfig {
    pub project_id: String,
    pub location: String,
}
impl GoogleVertexConfig {
    pub fn new(project_id: impl Into<String>, location: impl Into<String>) -> Result<Self, String> {
        let v = Self {
            project_id: project_id.into(),
            location: location.into(),
        };
        region(&v.location)?;
        if v.project_id.len() < 6
            || v.project_id.len() > 30
            || !v
                .project_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("invalid Google project id".into());
        }
        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BrokerOperation {
    Catalog {
        provider: CloudProviderId,
        context_ref: String,
        #[serde(default)]
        allow_stale: bool,
    },
    Invoke {
        provider: CloudProviderId,
        context_ref: String,
        model_id: String,
        request: InvokeRequest,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub messages: Vec<BrokerMessage>,
    #[serde(default)]
    pub tools: Vec<BrokerTool>,
    pub stream: bool,
    #[serde(default)]
    pub options: InvokeOptions,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerMessage {
    pub role: MessageRole,
    pub content: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeOptions {
    pub max_output_tokens: Option<u32>,
    pub temperature_milli: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudProviderDescriptor {
    pub id: CloudProviderId,
    pub display_name: &'static str,
    /// Cloud operations always execute behind the typed broker boundary.
    pub typed_broker_only: bool,
    /// Production login needs a Synaps-owned/configured public OAuth app.
    pub registration_required: bool,
    /// Tool-use capability. `false` = text-only route: tool-requiring modes
    /// fail pre-flight, before any credential lookup or network access.
    pub supports_tools: bool,
}

pub const CLOUD_PROVIDER_DESCRIPTORS: [CloudProviderDescriptor; 3] = [
    CloudProviderDescriptor {
        id: CloudProviderId::AzureOpenAi,
        display_name: "Azure OpenAI",
        typed_broker_only: true,
        registration_required: true,
        supports_tools: false,
    },
    CloudProviderDescriptor {
        id: CloudProviderId::AwsBedrock,
        display_name: "AWS Bedrock",
        typed_broker_only: true,
        registration_required: false,
        supports_tools: false,
    },
    CloudProviderDescriptor {
        id: CloudProviderId::GoogleVertex,
        display_name: "Google Vertex",
        typed_broker_only: true,
        registration_required: true,
        supports_tools: false,
    },
];

pub fn cloud_provider_descriptors() -> &'static [CloudProviderDescriptor] {
    &CLOUD_PROVIDER_DESCRIPTORS
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    #[test]
    fn cloud_descriptors_are_complete_and_never_vend_credentials() {
        let descriptors = cloud_provider_descriptors();
        assert_eq!(descriptors.len(), 3);
        assert_eq!(descriptors[0].id.as_str(), "azure-openai");
        assert_eq!(descriptors[1].id.as_str(), "aws-bedrock");
        assert_eq!(descriptors[2].id.as_str(), "google-vertex");
        assert!(descriptors.iter().all(|d| d.typed_broker_only));
        assert_eq!(
            descriptors
                .iter()
                .filter(|d| d.registration_required)
                .map(|d| d.id)
                .collect::<Vec<_>>(),
            vec![CloudProviderId::AzureOpenAi, CloudProviderId::GoogleVertex]
        );
    }

    /// Spec §5.5: until full tool translation exists, every cloud route must
    /// advertise itself as text-only (no tool support).
    #[test]
    fn cloud_routes_advertise_text_only() {
        for descriptor in cloud_provider_descriptors() {
            assert!(
                !descriptor.supports_tools,
                "{} must advertise text-only until tool translation lands",
                descriptor.id
            );
            assert!(!descriptor.id.supports_tools());
        }
    }
}
