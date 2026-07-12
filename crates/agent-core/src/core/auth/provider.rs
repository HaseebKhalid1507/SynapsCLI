//! Typed OAuth provider identity, descriptors, registry validation, and dispatch.
//!
//! This registry is deliberately about authentication policy, not runtime wire
//! routing. CLI aliases are normalized before a value enters this module.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
};

use reqwest::Client;

use super::OAuthCredentials;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OAuthProviderId {
    Anthropic,
    OpenAiCodex,
    Xai,
    GitHubCopilot,
    GoogleGemini,
}

impl OAuthProviderId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCodex => "openai-codex",
            Self::Xai => "xai-auth",
            Self::GitHubCopilot => "github-copilot",
            Self::GoogleGemini => "google-gemini",
        }
    }
}

impl fmt::Display for OAuthProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OAuthProviderId {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "openai-codex" => Ok(Self::OpenAiCodex),
            "xai-auth" => Ok(Self::Xai),
            "github-copilot" => Ok(Self::GitHubCopilot),
            "google-gemini" => Ok(Self::GoogleGemini),
            _ => Err(format!("unknown canonical OAuth provider id: {value}")),
        }
    }
}

impl TryFrom<&str> for OAuthProviderId {
    type Error = String;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<String> for OAuthProviderId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<&String> for OAuthProviderId {
    type Error = String;
    fn try_from(value: &String) -> Result<Self, Self::Error> {
        value.as_str().parse()
    }
}

/// How a runtime may obtain a usable credential from the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerCredentialStrategy {
    /// Broker may vend only an access token and its expiry. Refresh credentials
    /// remain broker-owned.
    OAuthAccessToken,
    /// Requests are executed/signed at the broker; no raw static key is vended.
    ProxyOrSign,
    /// Temporary local-only compatibility. Implementations MUST reject remote
    /// callers and audit use. No provider currently opts into this mode.
    SameHostStaticKeyCompatibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderBehavior {
    Anthropic,
    OpenAiCodex,
    Xai,
    GitHubCopilot,
    GoogleGemini,
}

#[derive(Debug, Clone, Copy)]
pub struct OAuthProviderDescriptor {
    pub id: OAuthProviderId,
    pub display_name: &'static str,
    pub description: &'static str,
    pub recommended: bool,
    pub broker_strategy: BrokerCredentialStrategy,
    pub behavior: ProviderBehavior,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderReference {
    pub name: &'static str,
    pub oauth_provider: OAuthProviderId,
}

#[derive(Debug)]
pub struct OAuthProviderRegistry {
    descriptors: HashMap<OAuthProviderId, OAuthProviderDescriptor>,
}

impl OAuthProviderRegistry {
    pub fn validate(
        descriptors: impl IntoIterator<Item = OAuthProviderDescriptor>,
        references: impl IntoIterator<Item = ProviderReference>,
    ) -> Result<Self, String> {
        let mut map = HashMap::new();
        let mut behavior = HashSet::new();
        for descriptor in descriptors {
            if map.insert(descriptor.id, descriptor).is_some() {
                return Err(format!("duplicate OAuth provider id: {}", descriptor.id));
            }
            if !behavior.insert(descriptor.behavior) {
                return Err(format!(
                    "duplicate OAuth provider behavior: {:?}",
                    descriptor.behavior
                ));
            }
        }
        for reference in references {
            if !map.contains_key(&reference.oauth_provider) {
                return Err(format!(
                    "{} references missing OAuth provider {}",
                    reference.name, reference.oauth_provider
                ));
            }
        }
        Ok(Self { descriptors: map })
    }

    pub fn get(&self, id: OAuthProviderId) -> Option<&OAuthProviderDescriptor> {
        self.descriptors.get(&id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &OAuthProviderDescriptor> {
        self.descriptors.values()
    }
}

pub fn registry() -> OAuthProviderRegistry {
    OAuthProviderRegistry::validate(DESCRIPTORS, []).expect("built-in OAuth registry must be valid")
}

pub const DESCRIPTORS: [OAuthProviderDescriptor; 5] = [
    OAuthProviderDescriptor {
        id: OAuthProviderId::Anthropic,
        display_name: "Claude",
        description: "Claude account OAuth",
        recommended: true,
        broker_strategy: BrokerCredentialStrategy::OAuthAccessToken,
        behavior: ProviderBehavior::Anthropic,
    },
    OAuthProviderDescriptor {
        id: OAuthProviderId::OpenAiCodex,
        display_name: "OpenAI Codex",
        description: "ChatGPT Plus/Pro OAuth",
        recommended: false,
        broker_strategy: BrokerCredentialStrategy::OAuthAccessToken,
        behavior: ProviderBehavior::OpenAiCodex,
    },
    OAuthProviderDescriptor {
        id: OAuthProviderId::Xai,
        display_name: "xAI (Grok)",
        description: "xAI account OAuth",
        recommended: false,
        broker_strategy: BrokerCredentialStrategy::OAuthAccessToken,
        behavior: ProviderBehavior::Xai,
    },
    OAuthProviderDescriptor {
        id: OAuthProviderId::GitHubCopilot,
        display_name: "GitHub Copilot",
        description: "GitHub Copilot device OAuth (experimental)",
        recommended: false,
        // Vends only the short-lived Copilot session token via AccessToken.
        // Long-lived GitHub user token remains in refresh, broker-owned.
        broker_strategy: BrokerCredentialStrategy::OAuthAccessToken,
        behavior: ProviderBehavior::GitHubCopilot,
    },
    OAuthProviderDescriptor {
        id: OAuthProviderId::GoogleGemini,
        display_name: "Google Gemini (Code Assist)",
        description: "Google account OAuth for Gemini CLI / Code Assist (experimental)",
        recommended: false,
        // Broker keeps the Google refresh token; runtime only ever sees the
        // short-lived access token via AccessToken.
        broker_strategy: BrokerCredentialStrategy::OAuthAccessToken,
        behavior: ProviderBehavior::GoogleGemini,
    },
];

/// CLI-only normalization. Internal callers must carry the canonical typed ID.
pub fn parse_cli_provider(value: &str) -> Result<OAuthProviderId, String> {
    match value.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Ok(OAuthProviderId::Anthropic),
        "openai-codex" => Ok(OAuthProviderId::OpenAiCodex),
        "xai-auth" => Ok(OAuthProviderId::Xai),
        // Aliases normalize only at CLI parsing; storage key remains github-copilot.
        "github-copilot" | "copilot" | "gh-copilot" => Ok(OAuthProviderId::GitHubCopilot),
        // CLI aliases only; canonical storage key is "google-gemini".
        // `google` is NOT accepted here: the static provider key "google"
        // (Google AI Studio, API-key) already claims that CLI token and we
        // must not silently redirect it to the Code Assist OAuth flow.
        "google-gemini" | "gemini" | "gemini-cli" | "gemini-code-assist" => {
            Ok(OAuthProviderId::GoogleGemini)
        }
        _ => Err(format!("unknown OAuth provider: {value}")),
    }
}

pub async fn login(id: OAuthProviderId) -> Result<OAuthCredentials, String> {
    match registry()
        .get(id)
        .expect("typed built-in provider")
        .behavior
    {
        ProviderBehavior::Anthropic => super::providers::anthropic::login().await,
        ProviderBehavior::OpenAiCodex => super::providers::openai_codex::login().await,
        ProviderBehavior::Xai => super::providers::xai::login().await,
        ProviderBehavior::GitHubCopilot => super::providers::github_copilot::login().await,
        ProviderBehavior::GoogleGemini => super::providers::google_gemini::login().await,
    }
}

pub async fn refresh(
    client: &Client,
    id: OAuthProviderId,
    refresh: &str,
) -> Result<OAuthCredentials, String> {
    match registry()
        .get(id)
        .expect("typed built-in provider")
        .behavior
    {
        ProviderBehavior::Anthropic => super::providers::anthropic::refresh(client, refresh).await,
        ProviderBehavior::OpenAiCodex => {
            super::providers::openai_codex::refresh(client, refresh).await
        }
        ProviderBehavior::Xai => super::providers::xai::refresh(client, refresh).await,
        ProviderBehavior::GitHubCopilot => {
            super::providers::github_copilot::refresh(client, refresh).await
        }
        ProviderBehavior::GoogleGemini => {
            super::providers::google_gemini::refresh(client, refresh).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicates_and_missing_references() {
        assert!(
            OAuthProviderRegistry::validate([DESCRIPTORS[0], DESCRIPTORS[0]], [])
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(OAuthProviderRegistry::validate(
            [DESCRIPTORS[0]],
            [ProviderReference {
                name: "codex-model",
                oauth_provider: OAuthProviderId::OpenAiCodex
            }]
        )
        .unwrap_err()
        .contains("missing"));
    }

    #[test]
    fn every_descriptor_has_explicit_safe_strategy() {
        for provider in registry().iter() {
            assert_ne!(
                provider.broker_strategy,
                BrokerCredentialStrategy::SameHostStaticKeyCompatibility
            );
        }
    }

    #[test]
    fn claude_is_only_a_cli_alias() {
        assert_eq!(
            parse_cli_provider("ClAuDe").unwrap(),
            OAuthProviderId::Anthropic
        );
        assert!(OAuthProviderId::from_str("claude").is_err());
    }

    #[test]
    fn google_gemini_canonical_id_and_cli_aliases() {
        // Canonical wire/storage id is exactly "google-gemini".
        assert_eq!(OAuthProviderId::GoogleGemini.as_str(), "google-gemini");
        assert_eq!(
            OAuthProviderId::from_str("google-gemini").unwrap(),
            OAuthProviderId::GoogleGemini
        );
        // CLI aliases are CLI-only; canonical FromStr must reject them.
        assert!(OAuthProviderId::from_str("gemini").is_err());
        assert!(OAuthProviderId::from_str("gemini-cli").is_err());
        // `google` MUST remain reserved for the static AI-Studio API-key
        // provider; treating it as a Gemini OAuth alias would silently
        // redirect existing users' `synaps login --provider google` flow.
        assert!(
            parse_cli_provider("google").is_err(),
            "'google' must not resolve to google-gemini OAuth"
        );
        assert_eq!(
            parse_cli_provider("Gemini").unwrap(),
            OAuthProviderId::GoogleGemini
        );
        assert_eq!(
            parse_cli_provider("GEMINI-CLI").unwrap(),
            OAuthProviderId::GoogleGemini
        );
        assert_eq!(
            parse_cli_provider("gemini-code-assist").unwrap(),
            OAuthProviderId::GoogleGemini
        );
        assert_eq!(
            parse_cli_provider("google-gemini").unwrap(),
            OAuthProviderId::GoogleGemini
        );

        let registry = registry();
        let desc = registry
            .get(OAuthProviderId::GoogleGemini)
            .expect("google-gemini descriptor must be registered");
        assert_eq!(desc.id, OAuthProviderId::GoogleGemini);
        // Broker keeps refresh; only vends access token + expiry.
        assert_eq!(
            desc.broker_strategy,
            BrokerCredentialStrategy::OAuthAccessToken
        );
        assert_eq!(desc.behavior, ProviderBehavior::GoogleGemini);
        assert!(!desc.recommended, "gemini is experimental — not recommended");
    }

    #[test]
    fn github_copilot_canonical_id_and_cli_aliases() {
        assert_eq!(OAuthProviderId::GitHubCopilot.as_str(), "github-copilot");
        assert_eq!(
            OAuthProviderId::from_str("github-copilot").unwrap(),
            OAuthProviderId::GitHubCopilot
        );
        // Aliases are CLI-only — canonical FromStr rejects them.
        assert!(OAuthProviderId::from_str("copilot").is_err());
        assert!(OAuthProviderId::from_str("gh-copilot").is_err());
        assert_eq!(
            parse_cli_provider("copilot").unwrap(),
            OAuthProviderId::GitHubCopilot
        );
        assert_eq!(
            parse_cli_provider("GH-Copilot").unwrap(),
            OAuthProviderId::GitHubCopilot
        );
        assert_eq!(
            parse_cli_provider("github-copilot").unwrap(),
            OAuthProviderId::GitHubCopilot
        );
        let registry = registry();
        let desc = registry
            .get(OAuthProviderId::GitHubCopilot)
            .expect("descriptor");
        assert_eq!(desc.id, OAuthProviderId::GitHubCopilot);
        assert_eq!(
            desc.broker_strategy,
            BrokerCredentialStrategy::OAuthAccessToken
        );
        assert_eq!(desc.behavior, ProviderBehavior::GitHubCopilot);
        assert!(!desc.recommended);
    }
}
