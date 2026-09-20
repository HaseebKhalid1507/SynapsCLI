//! Account references for multi-account OAuth credentials.
//!
//! An account is a `(provider, label)` pair. The bare provider storage key
//! (`"openai-codex"`) is the `default` account; every other account is stored
//! under `"<provider>@<label>"`. Nothing that already exists moves or needs
//! migration.
//!
//! Labels are validated at every boundary (CLI, env, config, HTTP query, proxy
//! body) *before* they are used as a JSON key, a file-name fragment, a URL
//! parameter, or a log field. A label that fails the grammar never reaches
//! the filesystem.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::broker::BrokerError;
use super::provider::OAuthProviderId;

/// Reserved word for the bare provider slot. Never a label.
pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Reserved selector word for opt-in automatic selection (G5).
pub const AUTO_ACCOUNT_NAME: &str = "auto";

const MAX_LABEL_LEN: usize = 32;

// ── Label ────────────────────────────────────────────────────────────────────

/// Validated account label. Grammar: `^[a-z0-9][a-z0-9._-]{0,31}$`; the words
/// `default` and `auto` are reserved and rejected.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountLabel(String);

impl AccountLabel {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim();
        if value.is_empty() {
            return Err("account label is empty".into());
        }
        if value == DEFAULT_ACCOUNT_NAME || value == AUTO_ACCOUNT_NAME {
            return Err(format!("'{value}' is reserved and cannot be used as an account label"));
        }
        if value.len() > MAX_LABEL_LEN {
            return Err(format!(
                "account label is too long ({} chars; max {MAX_LABEL_LEN})",
                value.len()
            ));
        }
        let mut chars = value.chars();
        let first = chars.next().expect("non-empty");
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(
                "account label must start with a lowercase letter or digit (a-z, 0-9)".into(),
            );
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(
                "account label may only contain a-z, 0-9, '.', '_' and '-' (max 32 chars)".into(),
            );
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccountLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountLabel({})", self.0)
    }
}

impl fmt::Display for AccountLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Account ──────────────────────────────────────────────────────────────────

/// A concrete account slot for a provider.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Account {
    /// The bare provider key — every pre-existing installation.
    #[default]
    Default,
    Named(AccountLabel),
}

impl Account {
    /// `"default"` → [`Account::Default`]; anything else must be a valid
    /// label. Empty or malformed input is an error — an explicit account
    /// request is never coerced to the default slot.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim();
        if value == DEFAULT_ACCOUNT_NAME {
            return Ok(Self::Default);
        }
        AccountLabel::parse(value).map(Self::Named)
    }

    pub fn named(label: &str) -> Result<Self, String> {
        AccountLabel::parse(label).map(Self::Named)
    }

    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    /// `"default"` or the label text. Safe for display, JSON and URLs.
    pub fn label_str(&self) -> &str {
        match self {
            Self::Default => DEFAULT_ACCOUNT_NAME,
            Self::Named(label) => label.as_str(),
        }
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Account({})", self.label_str())
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label_str())
    }
}

impl Serialize for Account {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label_str())
    }
}

impl<'de> Deserialize<'de> for Account {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Account::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ── CredentialRef ────────────────────────────────────────────────────────────

/// A fully addressed OAuth credential: provider + account slot.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialRef {
    pub provider: OAuthProviderId,
    pub account: Account,
}

impl CredentialRef {
    pub fn new(provider: OAuthProviderId, account: Account) -> Self {
        Self { provider, account }
    }

    pub fn default_for(provider: OAuthProviderId) -> Self {
        Self {
            provider,
            account: Account::Default,
        }
    }

    /// `"openai-codex"` for the default slot, `"openai-codex@astra2"` otherwise.
    pub fn storage_key(&self) -> String {
        match &self.account {
            Account::Default => self.provider.as_str().to_string(),
            Account::Named(label) => format!("{}@{}", self.provider.as_str(), label.as_str()),
        }
    }

    /// Inverse of [`storage_key`](Self::storage_key). Returns `None` for keys
    /// that are not OAuth provider keys (static keys, cloud state, unknown
    /// providers) and for malformed labels (`"x@default"`, `"x@Bad Label"`).
    pub fn parse_storage_key(key: &str) -> Option<Self> {
        match key.split_once('@') {
            None => {
                let provider: OAuthProviderId = key.parse().ok()?;
                Some(Self::default_for(provider))
            }
            Some((provider, label)) => {
                let provider: OAuthProviderId = provider.parse().ok()?;
                let label = AccountLabel::parse(label).ok()?;
                Some(Self::new(provider, Account::Named(label)))
            }
        }
    }

    /// True if `key` names *some* slot of `provider` (valid or not). Used to
    /// surface malformed sibling keys in listings without ever loading them.
    pub(crate) fn key_belongs_to(key: &str, provider: OAuthProviderId) -> bool {
        key == provider.as_str()
            || key
                .strip_prefix(provider.as_str())
                .is_some_and(|rest| rest.starts_with('@'))
    }
}

#[derive(Serialize, Deserialize)]
struct CredentialRefWire {
    provider: String,
    account: String,
}

impl Serialize for CredentialRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CredentialRefWire {
            provider: self.provider.as_str().to_string(),
            account: self.account.label_str().to_string(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CredentialRefWire::deserialize(deserializer)?;
        let provider: OAuthProviderId = wire.provider.parse().map_err(serde::de::Error::custom)?;
        let account = Account::parse(&wire.account).map_err(serde::de::Error::custom)?;
        Ok(Self { provider, account })
    }
}

impl fmt::Debug for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CredentialRef({})", self.storage_key())
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.storage_key())
    }
}

// ── Selection policy ─────────────────────────────────────────────────────────

/// What a caller asks for when no explicit account is given.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AccountSelector {
    Account(Account),
    /// Opt-in automatic selection (G5). Resolved through the broker's
    /// installed capacity selector; fails closed when none is installed.
    Auto,
    /// A configured selector that failed validation. Kept in the policy so
    /// resolution FAILS CLOSED instead of silently using another account.
    /// `reason` is secret-free (grammar message + config/env source).
    Invalid { source: String, reason: String },
}

impl AccountSelector {
    /// `"auto"` → `Auto`; otherwise an [`Account`].
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim();
        if value == AUTO_ACCOUNT_NAME {
            return Ok(Self::Auto);
        }
        Account::parse(value).map(Self::Account)
    }

    /// Parse and, on failure, produce the fail-closed `Invalid` selector.
    pub fn parse_or_invalid(raw: &str, source: &str) -> Self {
        match Self::parse(raw) {
            Ok(selector) => selector,
            Err(reason) => Self::Invalid {
                source: source.to_string(),
                reason,
            },
        }
    }

    pub fn as_config_value(&self) -> String {
        match self {
            Self::Account(account) => account.label_str().to_string(),
            Self::Auto => AUTO_ACCOUNT_NAME.to_string(),
            Self::Invalid { .. } => String::new(),
        }
    }
}

/// Environment variable that selects the account for a provider:
/// `SYNAPS_ACCOUNT_<PROVIDER>` with `-` → `_`, upper-cased.
pub fn account_env_var(provider: OAuthProviderId) -> String {
    format!(
        "SYNAPS_ACCOUNT_{}",
        provider.as_str().to_ascii_uppercase().replace('-', "_")
    )
}

/// Config key carrying the selected account for a provider.
pub fn account_config_key(provider: OAuthProviderId) -> String {
    format!("auth.account.{}", provider.as_str())
}

/// Per-provider account selection. Built once from config (+ env overlay);
/// never mutates the process environment or the active profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountPolicy {
    per_provider: BTreeMap<OAuthProviderId, AccountSelector>,
}

impl AccountPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, provider: OAuthProviderId, selector: AccountSelector) {
        self.per_provider.insert(provider, selector);
    }

    pub fn with(mut self, provider: OAuthProviderId, selector: AccountSelector) -> Self {
        self.set(provider, selector);
        self
    }

    pub fn selector(&self, provider: OAuthProviderId) -> AccountSelector {
        self.per_provider
            .get(&provider)
            .cloned()
            .unwrap_or(AccountSelector::Account(Account::Default))
    }

    pub fn is_empty(&self) -> bool {
        self.per_provider.is_empty()
    }

    /// Build from the `auth.account.<provider> = <label|auto>` map.
    ///
    /// Fail closed: a malformed label for a known provider becomes
    /// [`AccountSelector::Invalid`], so every later resolution for that
    /// provider errors instead of quietly using the default (or any other)
    /// account. Unknown providers cannot affect resolution and are reported
    /// as warnings only. Empty values mean "unset".
    pub fn from_config_map(map: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut policy = Self::new();
        let mut warnings = Vec::new();
        for (provider, value) in map {
            if value.trim().is_empty() {
                continue;
            }
            let source = format!("auth.account.{provider}");
            let Ok(provider_id) = provider.parse::<OAuthProviderId>() else {
                warnings.push(format!("{source}: unknown OAuth provider (ignored)"));
                continue;
            };
            let selector = AccountSelector::parse_or_invalid(value, &source);
            if let AccountSelector::Invalid { reason, .. } = &selector {
                warnings.push(format!(
                    "{source}: {reason} — account selection for {provider} is disabled until fixed"
                ));
            }
            policy.set(provider_id, selector);
        }
        (policy, warnings)
    }

    /// Apply `SYNAPS_ACCOUNT_<PROVIDER>` overrides on top of `self`. A
    /// malformed value fails closed for that provider (a typo must never
    /// redirect traffic to a different account). Empty values are ignored.
    pub fn with_env_overlay(mut self) -> Self {
        for descriptor in super::provider::DESCRIPTORS.iter() {
            let var = account_env_var(descriptor.id);
            let Ok(raw) = std::env::var(&var) else {
                continue;
            };
            if raw.trim().is_empty() {
                continue;
            }
            let selector = AccountSelector::parse_or_invalid(&raw, &var);
            if let AccountSelector::Invalid { reason, .. } = &selector {
                tracing::warn!(var = %var, "invalid account selector; failing closed: {reason}");
            }
            self.set(descriptor.id, selector);
        }
        self
    }

    /// Precedence: env `SYNAPS_ACCOUNT_<PROVIDER>` > config
    /// `auth.account.<provider>` > default. Reads the active profile's config
    /// file; never writes anything.
    pub fn from_environment() -> Self {
        let (policy, _warnings) = Self::from_config_map(&crate::config::load_config().auth.accounts);
        policy.with_env_overlay()
    }

    /// Resolve the credential to use for `provider` when the caller did not
    /// pin one. `Auto` is not resolvable here (the broker consults its
    /// capacity selector); `Invalid` always errors.
    pub fn resolve(&self, provider: OAuthProviderId) -> Result<CredentialRef, BrokerError> {
        match self.selector(provider) {
            AccountSelector::Account(account) => Ok(CredentialRef::new(provider, account)),
            AccountSelector::Auto => Err(BrokerError::NoAccountAvailable {
                provider: provider.as_str().to_string(),
                reason: "automatic account selection requires a capacity selector".into(),
            }),
            AccountSelector::Invalid { source, reason } => Err(BrokerError::InvalidAccount(
                format!("{source}: {reason}"),
            )),
        }
    }
}

// ── Listing row ──────────────────────────────────────────────────────────────

/// Non-secret listing row for one stored account. Never carries token
/// material; `account_id_prefix` is at most 8 characters.
///
/// Construct with [`AccountSummary::new`] and set the optional fields you
/// know; new optional fields are added with serde defaults so older brokers
/// and clients keep interoperating.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSummary {
    pub provider: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_prefix: Option<String>,
    /// Stable, opaque fingerprint of the provider seat this slot holds
    /// ([`seat_fingerprint`] over the FULL provider account id). Two aliases
    /// of one seat share it; a re-login with another seat changes it. `None`
    /// when the provider exposes no account id — consumers that must act on
    /// exactly one seat (activation, spending) fail closed on `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_fingerprint: Option<String>,
    /// Access-token expiry (epoch ms); `0` when unknown.
    #[serde(default)]
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<u64>,
    /// True when the active policy would select this account.
    #[serde(default)]
    pub selected: bool,
    /// Broker-side cooldown (epoch ms) after a reported provider limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<u64>,
}

impl AccountSummary {
    /// Row for one slot with every optional field unset.
    pub fn new(provider: OAuthProviderId, account: &Account) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            label: account.label_str().to_string(),
            ..Self::default()
        }
    }

    /// Set both identity views derived from the provider's account id: the
    /// short display prefix and the full-id fingerprint.
    pub fn with_account_id(mut self, account_id: Option<&str>) -> Self {
        self.account_id_prefix = account_id.and_then(account_id_prefix);
        self.seat_fingerprint = self
            .provider
            .parse::<OAuthProviderId>()
            .ok()
            .zip(account_id)
            .and_then(|(provider, id)| seat_fingerprint(provider, id));
        self
    }

    pub fn credential_ref(&self) -> Option<CredentialRef> {
        let provider: OAuthProviderId = self.provider.parse().ok()?;
        let account = Account::parse(&self.label).ok()?;
        Some(CredentialRef::new(provider, account))
    }
}

/// Stable, opaque seat fingerprint: hex SHA-256 over a domain tag, the
/// provider id and the provider's FULL account id (e.g. the Codex
/// `chatgpt_account_id` claim). Deterministic across processes and hosts, so
/// a consumer holding a vended token can derive the same value from that
/// token's claim and compare it with [`AccountSummary::seat_fingerprint`]
/// before spending on the listed seat. Never derived from token material.
pub fn seat_fingerprint(provider: OAuthProviderId, account_id: &str) -> Option<String> {
    let id = account_id.trim();
    if id.is_empty() {
        return None;
    }
    Some(hex_digest(&[b"synaps-seat-v1", provider.as_str().as_bytes(), id.as_bytes()]))
}

fn hex_digest(parts: &[&[u8]]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

/// Identity of the credential material currently stored in one slot. Scopes
/// per-slot caches and pairs a proven-capacity reading with the token vended
/// for it: the provider seat (via [`seat_fingerprint`]) when the credential
/// carries an account id — stable across token rotation — otherwise a digest
/// of the refresh material, which changes on re-login and on rotation. An
/// opaque digest: not reversible, never logged with token material.
#[derive(Clone, PartialEq, Eq)]
pub struct SeatIdentity(String);

impl SeatIdentity {
    pub fn of(provider: OAuthProviderId, creds: &super::OAuthCredentials) -> Self {
        if let Some(fp) = creds
            .account_id
            .as_deref()
            .and_then(|id| seat_fingerprint(provider, id))
        {
            return Self(format!("id:{fp}"));
        }
        let material = if creds.refresh.is_empty() {
            creds.access.as_bytes()
        } else {
            creds.refresh.as_bytes()
        };
        Self(format!(
            "cred:{}",
            hex_digest(&[b"synaps-cred-v1", provider.as_str().as_bytes(), material])
        ))
    }

    /// Seat fingerprint when this identity is a provider account id.
    pub fn seat_fingerprint(&self) -> Option<&str> {
        self.0.strip_prefix("id:")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SeatIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short preview only: enough to tell two identities apart in a trace.
        let (kind, digest) = self.0.split_once(':').unwrap_or(("?", &self.0));
        write!(f, "SeatIdentity({kind}:{}…)", &digest[..digest.len().min(8)])
    }
}

/// Short, non-reversible identity preview (`account_id_prefix`).
pub(crate) fn account_id_prefix(account_id: &str) -> Option<String> {
    let trimmed = account_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(8).collect())
}

/// Process-wide lock for tests that mutate account-selection environment
/// variables (global state shared by every test thread).
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_grammar() {
        for ok in ["a", "astra2", "m27", "work.eu", "x_y-z", "0", &"a".repeat(32)] {
            assert!(AccountLabel::parse(ok).is_ok(), "{ok} must be valid");
        }
        for bad in [
            "",
            " ",
            "default",
            "auto",
            "Astra",
            "-lead",
            ".dot",
            "with space",
            "a@b",
            "a/b",
            "../x",
            "émoji",
            &"a".repeat(33),
        ] {
            assert!(AccountLabel::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn account_parse_default_and_named() {
        assert!(Account::parse("").is_err(), "explicit empty account must error");
        assert!(Account::parse("   ").is_err());
        assert_eq!(Account::parse("default").unwrap(), Account::Default);
        assert_eq!(Account::parse(" default ").unwrap(), Account::Default);
        assert_eq!(Account::parse("astra2").unwrap().label_str(), "astra2");
        assert!(Account::parse("Bad").is_err());
        assert!(Account::parse("auto").is_err(), "auto is a selector, not an account");
    }

    #[test]
    fn storage_key_round_trip() {
        let default = CredentialRef::default_for(OAuthProviderId::OpenAiCodex);
        assert_eq!(default.storage_key(), "openai-codex");
        let named = CredentialRef::new(
            OAuthProviderId::OpenAiCodex,
            Account::named("astra2").unwrap(),
        );
        assert_eq!(named.storage_key(), "openai-codex@astra2");
        assert_eq!(
            CredentialRef::parse_storage_key("openai-codex").unwrap(),
            default
        );
        assert_eq!(
            CredentialRef::parse_storage_key("openai-codex@astra2").unwrap(),
            named
        );
        assert_eq!(
            CredentialRef::parse_storage_key("anthropic@m27").unwrap().provider,
            OAuthProviderId::Anthropic
        );
    }

    #[test]
    fn parse_storage_key_rejects_non_oauth_and_malformed() {
        for key in [
            "groq",
            "aws-bedrock",
            "local",
            "unknown@x",
            "openai-codex@",
            "openai-codex@default",
            "openai-codex@auto",
            "openai-codex@Bad",
            "openai-codex@a@b",
            "@astra2",
            "",
        ] {
            assert!(
                CredentialRef::parse_storage_key(key).is_none(),
                "{key:?} must not parse"
            );
        }
    }

    #[test]
    fn key_belongs_to_provider_prefix_is_exact() {
        assert!(CredentialRef::key_belongs_to("kimi-code", OAuthProviderId::KimiCode));
        assert!(CredentialRef::key_belongs_to("kimi-code@BAD", OAuthProviderId::KimiCode));
        assert!(!CredentialRef::key_belongs_to("kimi", OAuthProviderId::KimiCode));
        assert!(!CredentialRef::key_belongs_to("kimi-code2", OAuthProviderId::KimiCode));
    }

    #[test]
    fn env_and_config_key_names() {
        assert_eq!(
            account_env_var(OAuthProviderId::OpenAiCodex),
            "SYNAPS_ACCOUNT_OPENAI_CODEX"
        );
        assert_eq!(account_env_var(OAuthProviderId::Xai), "SYNAPS_ACCOUNT_XAI_AUTH");
        assert_eq!(
            account_config_key(OAuthProviderId::KimiCode),
            "auth.account.kimi-code"
        );
    }

    #[test]
    fn policy_from_config_map_fails_closed_on_invalid() {
        let mut map = BTreeMap::new();
        map.insert("openai-codex".to_string(), "astra2".to_string());
        map.insert("anthropic".to_string(), "auto".to_string());
        map.insert("kimi-code".to_string(), "Bad Label".to_string());
        map.insert("xai-auth".to_string(), "".to_string());
        map.insert("groq".to_string(), "x".to_string());
        let (policy, warnings) = AccountPolicy::from_config_map(&map);
        assert_eq!(
            policy.selector(OAuthProviderId::OpenAiCodex),
            AccountSelector::Account(Account::named("astra2").unwrap())
        );
        assert_eq!(
            policy.selector(OAuthProviderId::Anthropic),
            AccountSelector::Auto
        );
        assert!(
            matches!(
                policy.selector(OAuthProviderId::KimiCode),
                AccountSelector::Invalid { .. }
            ),
            "malformed label must fail closed, never default"
        );
        assert!(matches!(
            policy.resolve(OAuthProviderId::KimiCode),
            Err(BrokerError::InvalidAccount(_))
        ));
        assert_eq!(
            policy.selector(OAuthProviderId::Xai),
            AccountSelector::Account(Account::Default),
            "empty value means unset"
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("kimi-code")));
        assert!(warnings.iter().any(|w| w.contains("groq")));
        let err = policy.resolve(OAuthProviderId::KimiCode).unwrap_err().to_string();
        assert!(err.contains("auth.account.kimi-code"), "{err}");
    }

    #[test]
    fn env_overlay_overrides_config_and_fails_closed_on_garbage() {
        // Serialized through a process-wide lock: env vars are global state.
        let _guard = env_lock();
        let var = account_env_var(OAuthProviderId::GitHubCopilot);
        let mut map = BTreeMap::new();
        map.insert("github-copilot".to_string(), "cfg".to_string());
        std::env::set_var(&var, "envlabel");
        let (policy, _) = AccountPolicy::from_config_map(&map);
        let policy = policy.with_env_overlay();
        assert_eq!(
            policy.resolve(OAuthProviderId::GitHubCopilot).unwrap().storage_key(),
            "github-copilot@envlabel"
        );
        std::env::set_var(&var, "NOT VALID");
        let (policy, _) = AccountPolicy::from_config_map(&map);
        let policy = policy.with_env_overlay();
        assert!(matches!(
            policy.resolve(OAuthProviderId::GitHubCopilot),
            Err(BrokerError::InvalidAccount(_))
        ));
        std::env::set_var(&var, "");
        let (policy, _) = AccountPolicy::from_config_map(&map);
        let policy = policy.with_env_overlay();
        assert_eq!(
            policy.resolve(OAuthProviderId::GitHubCopilot).unwrap().storage_key(),
            "github-copilot@cfg",
            "empty env value is unset"
        );
        std::env::remove_var(&var);
    }

    #[test]
    fn policy_resolve_default_named_and_auto() {
        let policy = AccountPolicy::new()
            .with(
                OAuthProviderId::OpenAiCodex,
                AccountSelector::Account(Account::named("b").unwrap()),
            )
            .with(OAuthProviderId::Anthropic, AccountSelector::Auto);
        assert_eq!(
            policy.resolve(OAuthProviderId::OpenAiCodex).unwrap().storage_key(),
            "openai-codex@b"
        );
        assert_eq!(
            policy.resolve(OAuthProviderId::KimiCode).unwrap().storage_key(),
            "kimi-code"
        );
        assert!(matches!(
            policy.resolve(OAuthProviderId::Anthropic),
            Err(BrokerError::NoAccountAvailable { .. })
        ));
    }

    #[test]
    fn credential_ref_serde_round_trip_and_validation() {
        let cred = CredentialRef::new(OAuthProviderId::KimiCode, Account::named("m27").unwrap());
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json, serde_json::json!({"provider": "kimi-code", "account": "m27"}));
        assert_eq!(serde_json::from_value::<CredentialRef>(json).unwrap(), cred);
        assert!(serde_json::from_str::<CredentialRef>(
            r#"{"provider":"groq","account":"default"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CredentialRef>(
            r#"{"provider":"kimi-code","account":"Bad"}"#
        )
        .is_err());
    }

    #[test]
    fn account_serde_round_trip_and_validation() {
        let json = serde_json::to_string(&Account::named("astra2").unwrap()).unwrap();
        assert_eq!(json, "\"astra2\"");
        assert_eq!(
            serde_json::from_str::<Account>("\"default\"").unwrap(),
            Account::Default
        );
        assert!(serde_json::from_str::<Account>("\"Bad\"").is_err());
    }

    #[test]
    fn summary_has_no_secret_fields() {
        let summary = AccountSummary {
            identity: Some("user@example.com".into()),
            expires: 1,
            added_at: Some(2),
            selected: true,
            ..AccountSummary::new(OAuthProviderId::OpenAiCodex, &Account::named("astra2").unwrap())
                .with_account_id(Some("2b2f0000-aaaa-bbbb"))
        };
        let json = serde_json::to_value(&summary).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        for forbidden in ["access", "refresh", "token", "key"] {
            assert!(!keys.contains(&forbidden));
        }
        assert_eq!(json["account_id_prefix"], "2b2f0000");
        assert_eq!(summary.credential_ref().unwrap().storage_key(), "openai-codex@astra2");
        // Full-id fingerprint: opaque, stable, and not the id itself.
        let fp = summary.seat_fingerprint.clone().unwrap();
        assert_eq!(fp.len(), 64);
        assert!(!fp.contains("2b2f0000"));
        assert_eq!(
            Some(fp),
            seat_fingerprint(OAuthProviderId::OpenAiCodex, "2b2f0000-aaaa-bbbb")
        );
        // Older peers without the field still deserialize (serde default).
        let legacy: AccountSummary =
            serde_json::from_str(r#"{"provider":"openai-codex","label":"x"}"#).unwrap();
        assert_eq!(legacy.seat_fingerprint, None);
    }

    #[test]
    fn seat_fingerprint_is_stable_per_provider_and_full_id() {
        let a = seat_fingerprint(OAuthProviderId::OpenAiCodex, "acct_1").unwrap();
        assert_eq!(seat_fingerprint(OAuthProviderId::OpenAiCodex, " acct_1 ").unwrap(), a);
        assert_ne!(seat_fingerprint(OAuthProviderId::OpenAiCodex, "acct_10").unwrap(), a);
        assert_ne!(seat_fingerprint(OAuthProviderId::Anthropic, "acct_1").unwrap(), a);
        assert_eq!(seat_fingerprint(OAuthProviderId::OpenAiCodex, "  "), None);
        // Same seat under another alias: same fingerprint.
        let alias = AccountSummary::new(OAuthProviderId::OpenAiCodex, &Account::Default)
            .with_account_id(Some("acct_1"));
        assert_eq!(alias.seat_fingerprint.as_deref(), Some(a.as_str()));
        assert_eq!(alias.account_id_prefix.as_deref(), Some("acct_1"));
    }

    #[test]
    fn seat_identity_prefers_account_id_and_never_prints_material() {
        let creds = |refresh: &str, id: Option<&str>| super::super::OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: refresh.into(),
            access: "access-SECRET".into(),
            expires: 0,
            account_id: id.map(str::to_string),
        };
        let p = OAuthProviderId::OpenAiCodex;
        // Account id present: rotation of the refresh token keeps the seat.
        let a1 = SeatIdentity::of(p, &creds("r1-SECRET", Some("acct_1")));
        let a2 = SeatIdentity::of(p, &creds("r2-SECRET", Some("acct_1")));
        assert_eq!(a1, a2);
        assert_eq!(a1.seat_fingerprint(), seat_fingerprint(p, "acct_1").as_deref());
        // Different seat under the same slot → different identity.
        assert_ne!(a1, SeatIdentity::of(p, &creds("r1-SECRET", Some("acct_2"))));
        // No account id: the refresh material decides (re-login changes it).
        let b1 = SeatIdentity::of(p, &creds("r1-SECRET", None));
        let b2 = SeatIdentity::of(p, &creds("r2-SECRET", None));
        assert_ne!(b1, b2);
        assert_eq!(b1, SeatIdentity::of(p, &creds("r1-SECRET", None)));
        assert_eq!(b1.seat_fingerprint(), None);
        for id in [&a1, &b1] {
            let shown = format!("{id:?}{}", id.as_str());
            assert!(!shown.contains("SECRET") && !shown.contains("acct_1"), "{shown}");
        }
    }
}
