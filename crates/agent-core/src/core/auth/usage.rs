//! Read-only, normalized quota/usage snapshots for the four OAuth providers
//! the multi-account broker manages: Claude (Anthropic), ChatGPT/Codex, Kimi
//! Code and Grok (xAI).
//!
//! Design rules (goal plan G3):
//!
//! - **Read-only.** Every adapter is a single pinned `GET`. Nothing here can
//!   redeem a banked reset, purchase credits, or generate inference.
//! - **Caller supplies the resolved token.** [`fetch_usage`] takes the provider
//!   and the already-resolved access token for exactly that `(provider,
//!   account)` so the credential broker invokes it *behind* its boundary. This
//!   module never reads `auth.json`, never refreshes, never sees a refresh token.
//! - **Pinned destinations.** URLs are constants. The only override is a
//!   loopback-only test seam ([`UsageFetchOptions::endpoint_override`]) that is
//!   rejected for any non-loopback host, so a bearer token can never be coaxed
//!   toward an attacker-chosen destination.
//! - **No redirects, bounded time and body.** [`UsageClient`] is built with a
//!   `redirect::Policy::none()`; 3xx is an error. Bodies are streamed into a
//!   capped buffer.
//! - **Sanitized errors.** [`UsageError`] never contains the token, the URL
//!   query, or a byte of an upstream body. Parse notes use fixed vocabulary —
//!   never raw upstream key names or values.
//! - **Unknown is not capacity.** `used_percent` is a tagged enum
//!   ([`UsedPercent`]); missing, null, non-numeric and out-of-range values are
//!   explicitly `Unknown { reason }`. Reset timestamps are provider-authoritative
//!   (RFC 3339 or unix seconds → epoch ms) or `None` — never inferred from
//!   `now + window`.
//! - **Dynamic windows.** Codex `primary_window` can be weekly and
//!   `secondary_window` can be `null`; durations come from
//!   `limit_window_seconds`, never from a hard-coded "primary = 5h".
//!
//! Schema provenance (no network was used):
//! - Codex field set verified against the string table of the locally installed
//!   official `codex` binary (`RateLimitWindowSnapshot`, `RateLimitStatusPayload`,
//!   `RateLimitResetCreditsSummary/Details`, `/wham/usage`,
//!   `/wham/rate-limit-reset-credits`).
//! - Kimi field set verified against the official `kimi` CLI bundle
//!   (`managed-usage.ts`: `usage`, `limits[].window/detail`, `boosterWallet`).
//! - Anthropic matches the previously shipped `synaps status` reader.
//! - Grok `/v1/billing?format=credits` follows the QuotaKit-documented shape
//!   (`config.creditUsagePercent`, `onDemandUsed.val / onDemandCap.val`,
//!   `config.currentPeriod.end` / `config.billingPeriodEnd`,
//!   `billingPeriodMinutes`). **Unverified against a live account**; anything
//!   else yields an explicitly unknown window.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::broker::BrokerError;
use super::provider::OAuthProviderId;

// ── Pinned endpoints and bounds ─────────────────────────────────────────────

/// Snapshot JSON schema version (bump on incompatible field changes).
pub const SCHEMA_VERSION: u32 = 1;

/// Anthropic OAuth usage summary (same endpoint the broker's typed op uses).
pub const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// ChatGPT/Codex rate-limit usage (`GET`, bearer + `chatgpt-account-id`).
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// ChatGPT/Codex banked-reset inventory (`GET` only — the sibling `/consume`
/// endpoint redeems a reset and is deliberately NOT reachable from here).
pub const CODEX_RESET_CREDITS_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
/// Kimi Code managed usage.
pub const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";
/// Grok Build billing (credits view).
pub const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

/// Total time budget for one usage request.
pub const USAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP/TLS connect budget.
pub const USAGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on a buffered usage body. Real payloads are a few KiB.
pub const USAGE_MAX_BODY_BYTES: usize = 256 * 1024;

const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const CODEX_ACCOUNT_HEADER: &str = "chatgpt-account-id";
const GROK_TOKEN_AUTH_HEADER: &str = "x-xai-token-auth";
const GROK_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const USER_AGENT: &str = concat!("SynapsCLI/", env!("CARGO_PKG_VERSION"));

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_WEEK: u64 = 604_800;

/// Whether a usage adapter exists for the provider. Copilot and Gemini expose
/// no quota endpoint we can read, so they are honestly unsupported.
pub fn supports_usage(provider: OAuthProviderId) -> bool {
    matches!(
        provider,
        OAuthProviderId::Anthropic
            | OAuthProviderId::OpenAiCodex
            | OAuthProviderId::KimiCode
            | OAuthProviderId::Xai
    )
}

/// Providers with a usage adapter, in display order.
pub fn supported_usage_providers() -> &'static [OAuthProviderId] {
    &[
        OAuthProviderId::Anthropic,
        OAuthProviderId::OpenAiCodex,
        OAuthProviderId::KimiCode,
        OAuthProviderId::Xai,
    ]
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Usage failures. `Display`/`Serialize` never include a token, a URL query,
/// or any upstream body bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageError {
    /// No adapter for this provider.
    UnsupportedProvider { provider: String },
    /// Codex: the access token carries no `chatgpt_account_id` claim, so the
    /// account header cannot be paired with the bearer. Fail closed.
    AccountIdUnavailable,
    /// Codex: the response names a different account than the token. The
    /// pairing invariant is violated; the snapshot is discarded.
    IdentityMismatch,
    /// A non-pinned, non-loopback endpoint override was supplied.
    InvalidEndpointOverride,
    /// Provider rejected the credential (401/403).
    Unauthorized { status: u16 },
    /// 429 on the usage endpoint itself. Not proof of quota exhaustion.
    RateLimited,
    /// Any other non-2xx status.
    UpstreamStatus { status: u16 },
    /// 3xx. Redirects are never followed.
    Redirected { status: u16 },
    Timeout,
    /// Transport failure; `detail` is a coarse class, never a URL or body.
    Transport { detail: String },
    BodyTooLarge { cap: usize },
    /// Body could not be parsed; `detail` names the shape, never values.
    Malformed { detail: String },
}

impl UsageError {
    /// Stable machine-readable kind (same as the serde tag).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnsupportedProvider { .. } => "unsupported_provider",
            Self::AccountIdUnavailable => "account_id_unavailable",
            Self::IdentityMismatch => "identity_mismatch",
            Self::InvalidEndpointOverride => "invalid_endpoint_override",
            Self::Unauthorized { .. } => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::UpstreamStatus { .. } => "upstream_status",
            Self::Redirected { .. } => "redirected",
            Self::Timeout => "timeout",
            Self::Transport { .. } => "transport",
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::Malformed { .. } => "malformed",
        }
    }

    /// True when the failure means the credential itself was rejected.
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized { .. } | Self::AccountIdUnavailable | Self::IdentityMismatch
        )
    }

    /// Map onto the broker error vocabulary (secret-free by construction).
    pub fn into_broker_error(self) -> BrokerError {
        match self {
            Self::UnsupportedProvider { provider } => BrokerError::UnsupportedCapability {
                provider,
                capability: "usage".to_string(),
            },
            Self::Unauthorized { .. } | Self::AccountIdUnavailable | Self::IdentityMismatch => {
                BrokerError::Credential(format!("usage: {self}"))
            }
            Self::InvalidEndpointOverride => BrokerError::Denied(format!("usage: {self}")),
            other => BrokerError::Transport(format!("usage: {other}")),
        }
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedProvider { provider } => {
                write!(f, "no usage adapter for provider '{provider}'")
            }
            Self::AccountIdUnavailable => {
                write!(f, "access token carries no account id; cannot pair account header")
            }
            Self::IdentityMismatch => {
                write!(f, "usage response names a different account than the token")
            }
            Self::InvalidEndpointOverride => {
                write!(f, "endpoint override must be a loopback test server")
            }
            Self::Unauthorized { status } => {
                write!(f, "provider rejected the access token (HTTP {status})")
            }
            Self::RateLimited => write!(f, "usage endpoint rate limited (HTTP 429)"),
            Self::UpstreamStatus { status } => write!(f, "usage request failed: HTTP {status}"),
            Self::Redirected { status } => {
                write!(f, "usage endpoint redirected (HTTP {status}); not followed")
            }
            Self::Timeout => write!(f, "usage request timed out"),
            Self::Transport { detail } => write!(f, "usage transport error: {detail}"),
            Self::BodyTooLarge { cap } => write!(f, "usage response exceeded {cap} bytes"),
            Self::Malformed { detail } => write!(f, "usage response malformed: {detail}"),
        }
    }
}

impl std::error::Error for UsageError {}

// ── Snapshot schema ─────────────────────────────────────────────────────────

/// Fraction of a window consumed. Never a bare number: the parser must decide
/// `Valid` (finite, `0.0..=100.0`) or `Unknown` with a fixed-vocabulary reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UsedPercent {
    Valid { percent: f64 },
    Unknown { reason: String },
}

impl UsedPercent {
    pub fn valid(&self) -> Option<f64> {
        match self {
            Self::Valid { percent } => Some(*percent),
            Self::Unknown { .. } => None,
        }
    }
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Valid { .. })
    }
    fn unknown(reason: &str) -> Self {
        Self::Unknown {
            reason: reason.to_string(),
        }
    }
    /// Validate a candidate percentage.
    pub fn from_f64(value: f64) -> Self {
        if !value.is_finite() {
            Self::unknown("not a number")
        } else if !(0.0..=100.0).contains(&value) {
            Self::unknown("out of range")
        } else {
            Self::Valid { percent: value }
        }
    }
    /// Validate a JSON value (`None` = key absent).
    fn from_json(value: Option<&Value>) -> Self {
        match value {
            None => Self::unknown("missing"),
            Some(Value::Null) => Self::unknown("null"),
            Some(v) => match number_from_value(v) {
                Some(n) => Self::from_f64(n),
                None => Self::unknown("not a number"),
            },
        }
    }
    /// `used / limit * 100` with the usual guards.
    fn from_ratio(used: Option<f64>, limit: Option<f64>) -> Self {
        match (used, limit) {
            (None, _) | (_, None) => Self::unknown("missing"),
            (_, Some(l)) if l <= 0.0 => Self::unknown("limit is zero"),
            (Some(u), Some(l)) => Self::from_f64(u / l * 100.0),
        }
    }
}

/// What a window applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowScope {
    /// Whole-account quota.
    Account,
    /// A model-specific sub-quota (Anthropic `seven_day_opus`, Codex slugs).
    Model { model: String },
    /// A feature-specific sub-quota (Codex code review, Anthropic OAuth apps).
    Feature { feature: String },
}

/// What kind of clock a `reset_at` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    /// Rolling/anchored quota window reset.
    QuotaWindow,
    /// Billing-period renewal (credits replenish); not an OAuth expiry.
    BillingRenewal,
}

/// One quota window as reported by the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Provider-native identifier (`five_hour`, `primary`, `limits[0]`, …).
    pub id: String,
    /// Short human label (`5h`, `7d`, provider name, or `unknown`).
    pub label: String,
    pub scope: WindowScope,
    /// Window length in seconds when the provider states or implies one.
    pub duration_secs: Option<u64>,
    pub used_percent: UsedPercent,
    /// Raw counters when the provider exposes them (Kimi, Grok on-demand).
    pub used: Option<f64>,
    pub limit: Option<f64>,
    /// Provider-authoritative reset instant, epoch milliseconds.
    pub reset_at: Option<u64>,
    pub reset_kind: ResetKind,
    /// Per-window exhaustion flag if the provider gives one.
    pub limit_reached: Option<bool>,
}

impl UsageWindow {
    /// Exhausted iff the provider says so or the valid percent is ≥ 100.
    pub fn is_exhausted(&self) -> bool {
        self.limit_reached == Some(true) || self.used_percent.valid().is_some_and(|p| p >= 100.0)
    }
}

/// Three-state model availability derived from model-scoped windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Exhausted,
    Unknown,
}

/// Per-model availability. Sources, in precedence order:
/// 1. an explicit provider statement (Codex top-level `model_usage`), and
/// 2. a model-scoped window (Anthropic `seven_day_<model>`, Codex slugs).
///
/// When both exist the entry is merged: exhaustion from either source wins
/// (fail closed), the explicit `available_at` beats the window reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAvailability {
    pub model: String,
    pub availability: Availability,
    pub used_percent: UsedPercent,
    /// When the model becomes usable again on the subscription (explicit
    /// `available_at`, else the model window reset), epoch ms.
    pub reset_at: Option<u64>,
    /// Provider says purchased credits would unlock the model right now.
    /// Credits are billing, not subscription capacity: this never makes the
    /// model `Available` and nothing here purchases or redeems anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_would_enable: Option<bool>,
    /// `model_usage` | `window` | `merged`.
    #[serde(default)]
    pub source: String,
}

impl ModelAvailability {
    /// Case-insensitive match of an availability entry against a requested
    /// model id. Exact match, or the entry names a family token contained in
    /// the id (`sonnet` ⊂ `claude-sonnet-4-5`, `gpt-6-astra` == `gpt-6-astra`).
    pub fn matches_model(&self, requested: &str) -> bool {
        let entry = self.model.to_ascii_lowercase();
        let req = requested.trim().to_ascii_lowercase();
        if entry.is_empty() || req.is_empty() {
            return false;
        }
        if entry == req {
            return true;
        }
        req.split(['-', '_', '/', ':', '.']).any(|tok| tok == entry)
    }
}

/// Billing/credit balance. This is money-like inventory, not quota.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditBalance {
    /// `codex_credits` | `grok_credits` | `kimi_booster`.
    pub kind: String,
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub remaining: Option<f64>,
    pub total: Option<f64>,
    pub unit: Option<String>,
    /// Billing renewal instant, epoch ms, when reported.
    pub renews_at: Option<u64>,
}

/// Codex banked weekly-reset inventory (read-only view).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BankedResets {
    /// `None` = provider did not report a count (unknown ≠ zero).
    pub available_count: Option<u64>,
    pub credits: Vec<BankedResetCredit>,
    /// The optional inventory call failed; the rest of the snapshot is valid.
    pub inventory_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BankedResetCredit {
    pub reset_type: Option<String>,
    pub granted_at: Option<u64>,
    pub expires_at: Option<u64>,
}

/// Normalized, secret-free usage observation for one `(provider, account)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub schema_version: u32,
    /// Canonical OAuth provider id (`anthropic`, `openai-codex`, `kimi-code`, `xai-auth`).
    pub provider: String,
    /// `default` or the account label.
    pub account: String,
    /// Epoch ms when the body was received; authoritative for staleness.
    pub observed_at: u64,
    pub plan: Option<String>,
    /// Provider-asserted overall exhaustion flag (Codex `rate_limit.limit_reached`).
    pub limit_reached: Option<bool>,
    /// Workspace/account spend control tripped (Codex `spend_control.reached`).
    /// `Some(true)` blocks subscribed availability exactly like `limit_reached`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control_reached: Option<bool>,
    pub windows: Vec<UsageWindow>,
    pub model_availability: Vec<ModelAvailability>,
    pub credits: Option<CreditBalance>,
    pub banked_resets: Option<BankedResets>,
    /// First 8 chars of a provider account id when the response carries one.
    pub identity_prefix: Option<String>,
    /// Opaque full seat identity paired with the bearer used for this reading.
    /// Currently supplied by the Codex adapter; absent on older brokers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_fingerprint: Option<String>,
    /// Fixed-vocabulary parse notes (never raw keys or values).
    pub notes: Vec<String>,
}

impl UsageSnapshot {
    fn empty(provider: OAuthProviderId, account: &str, observed_at: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            provider: provider.as_str().to_string(),
            account: account.to_string(),
            observed_at,
            plan: None,
            limit_reached: None,
            spend_control_reached: None,
            windows: Vec::new(),
            model_availability: Vec::new(),
            credits: None,
            banked_resets: None,
            identity_prefix: None,
            seat_fingerprint: None,
            notes: Vec::new(),
        }
    }

    /// Observation older than `max_age_ms` (or from the future) is stale.
    /// Stale readings are never capacity.
    pub fn is_stale(&self, now_ms: u64, max_age_ms: u64) -> bool {
        now_ms < self.observed_at || now_ms - self.observed_at > max_age_ms
    }

    pub fn window(&self, id: &str) -> Option<&UsageWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn account_windows(&self) -> impl Iterator<Item = &UsageWindow> {
        self.windows
            .iter()
            .filter(|w| matches!(w.scope, WindowScope::Account))
    }

    /// The account window with the longest known duration (e.g. weekly).
    /// Windows with unknown duration sort last.
    pub fn longest_account_window(&self) -> Option<&UsageWindow> {
        self.account_windows()
            .max_by_key(|w| (w.duration_secs.is_some(), w.duration_secs.unwrap_or(0)))
    }

    /// Fresh, provider did not flag exhaustion or a tripped spend control, at
    /// least one account window, and every account window has a *valid*
    /// percent below 100.
    pub fn has_proven_capacity(&self, now_ms: u64, max_age_ms: u64) -> bool {
        if self.is_stale(now_ms, max_age_ms)
            || self.limit_reached == Some(true)
            || self.spend_control_reached == Some(true)
        {
            return false;
        }
        let mut any = false;
        for w in self.account_windows() {
            any = true;
            match w.used_percent.valid() {
                Some(p) if p < 100.0 && w.limit_reached != Some(true) => {}
                _ => return false,
            }
        }
        any
    }

    /// Availability entry for a requested model id, if the provider reported one.
    pub fn model_availability_for(&self, model: &str) -> Option<&ModelAvailability> {
        self.model_availability
            .iter()
            .find(|m| m.matches_model(model))
    }

    /// Model-aware capacity: account capacity AND, when the provider reported
    /// anything about this model, that entry must be `Available`. A gated
    /// model (`model_usage.available == false`, Astra-style) or an exhausted
    /// model window rejects even when the generic quota is nearly unused.
    /// Models the provider did not mention fall back to account capacity.
    pub fn has_proven_capacity_for_model(&self, model: &str, now_ms: u64, max_age_ms: u64) -> bool {
        if !self.has_proven_capacity(now_ms, max_age_ms) {
            return false;
        }
        match self.model_availability_for(model) {
            Some(entry) => entry.availability == Availability::Available,
            None => true,
        }
    }

    /// Earliest reset across all windows and model availability entries.
    pub fn earliest_reset_at(&self) -> Option<u64> {
        self.windows
            .iter()
            .filter_map(|w| w.reset_at)
            .chain(self.model_availability.iter().filter_map(|m| m.reset_at))
            .min()
    }

    /// Fold model-scoped windows into `model_availability`, merging with any
    /// explicit provider entries already present (e.g. Codex `model_usage`).
    /// Explicit entries are never overwritten: exhaustion from either source
    /// wins, an explicit reset beats the window reset, and the window
    /// contributes its percent when the explicit entry has none.
    fn derive_model_availability(&mut self) {
        let mut derived: Vec<ModelAvailability> = Vec::new();
        for w in &self.windows {
            let WindowScope::Model { model } = &w.scope else {
                continue;
            };
            let from_window = ModelAvailability {
                model: model.clone(),
                availability: if w.is_exhausted() {
                    Availability::Exhausted
                } else if w.used_percent.is_known() {
                    Availability::Available
                } else {
                    Availability::Unknown
                },
                used_percent: w.used_percent.clone(),
                reset_at: w.reset_at,
                credits_would_enable: None,
                source: "window".into(),
            };
            match self
                .model_availability
                .iter_mut()
                .find(|m| m.model.eq_ignore_ascii_case(model))
            {
                Some(explicit) => {
                    if from_window.availability == Availability::Exhausted {
                        explicit.availability = Availability::Exhausted;
                    }
                    if !explicit.used_percent.is_known() {
                        explicit.used_percent = from_window.used_percent;
                    }
                    if explicit.reset_at.is_none() {
                        explicit.reset_at = from_window.reset_at;
                    }
                    explicit.source = "merged".into();
                }
                None => derived.push(from_window),
            }
        }
        self.model_availability.extend(derived);
    }

    fn note(&mut self, text: &str) {
        if !self.notes.iter().any(|n| n == text) {
            self.notes.push(text.to_string());
        }
    }
}

// ── Aggregate report (status command / broker listing) ──────────────────────

/// One account's outcome inside a [`UsageReport`]: partial failures never
/// discard the accounts that succeeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AccountUsageOutcome {
    Ok { snapshot: Box<UsageSnapshot> },
    Error { error: UsageErrorSummary },
}

impl AccountUsageOutcome {
    pub fn ok(snapshot: UsageSnapshot) -> Self {
        Self::Ok {
            snapshot: Box::new(snapshot),
        }
    }
    pub fn error(error: &UsageError) -> Self {
        Self::Error {
            error: error.into(),
        }
    }
    pub fn snapshot(&self) -> Option<&UsageSnapshot> {
        match self {
            Self::Ok { snapshot } => Some(snapshot),
            Self::Error { .. } => None,
        }
    }
}

/// Secret-free error row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageErrorSummary {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl From<&UsageError> for UsageErrorSummary {
    fn from(e: &UsageError) -> Self {
        let http_status = match e {
            UsageError::Unauthorized { status }
            | UsageError::UpstreamStatus { status }
            | UsageError::Redirected { status } => Some(*status),
            UsageError::RateLimited => Some(429),
            _ => None,
        };
        Self {
            kind: e.kind().to_string(),
            message: e.to_string(),
            http_status,
        }
    }
}

impl UsageErrorSummary {
    /// Generic (non-`UsageError`) failure such as token resolution.
    pub fn other(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            message: message.into(),
            http_status: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageEntry {
    pub provider: String,
    pub account: String,
    /// Display-only identity from broker metadata (usually an email); never a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(flatten)]
    pub outcome: AccountUsageOutcome,
}

/// `synaps status --json` / broker listing payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub schema_version: u32,
    pub observed_at: u64,
    /// `local` or `remote` credential source.
    pub source: String,
    pub accounts: Vec<AccountUsageEntry>,
}

impl UsageReport {
    pub fn new(source: &str, observed_at: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            observed_at,
            source: source.to_string(),
            accounts: Vec::new(),
        }
    }
    pub fn ok_count(&self) -> usize {
        self.accounts
            .iter()
            .filter(|a| matches!(a.outcome, AccountUsageOutcome::Ok { .. }))
            .count()
    }
    pub fn error_count(&self) -> usize {
        self.accounts.len() - self.ok_count()
    }
}

// ── Value helpers (pure) ────────────────────────────────────────────────────

fn number_from_value(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn u64_from_value(v: &Value) -> Option<u64> {
    number_from_value(v).filter(|f| *f >= 0.0 && *f <= u64::MAX as f64).map(|f| f as u64)
}

fn string_from_value(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn bool_from_value(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

fn epoch_ms_from_rfc3339(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.timestamp_millis())
        .filter(|ms| *ms >= 0)
        .map(|ms| ms as u64)
}

/// Threshold above which a bare number is treated as milliseconds (≈ 5138 AD
/// in seconds, ≈ 1973 in ms).
const EPOCH_MS_THRESHOLD: f64 = 1.0e11;

/// RFC 3339 string, unix seconds, or unix milliseconds → epoch ms.
fn epoch_ms_from_value(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::String(s) => epoch_ms_from_rfc3339(s).or_else(|| {
            s.trim()
                .parse::<f64>()
                .ok()
                .and_then(epoch_ms_from_number)
        }),
        Value::Number(n) => n.as_f64().and_then(epoch_ms_from_number),
        _ => None,
    }
}

fn epoch_ms_from_number(n: f64) -> Option<u64> {
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let ms = if n > EPOCH_MS_THRESHOLD { n } else { n * 1000.0 };
    (ms <= u64::MAX as f64).then_some(ms as u64)
}

/// `18000` → `5h`, `604800` → `7d`, `90` → `90s`.
pub fn duration_label(secs: u64) -> String {
    if secs == 0 {
        "0s".into()
    } else if secs % SECS_PER_DAY == 0 {
        format!("{}d", secs / SECS_PER_DAY)
    } else if secs % SECS_PER_HOUR == 0 {
        format!("{}h", secs / SECS_PER_HOUR)
    } else if secs % SECS_PER_MINUTE == 0 {
        format!("{}m", secs / SECS_PER_MINUTE)
    } else {
        format!("{secs}s")
    }
}

fn identity_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

fn parse_json_object(body: &str) -> Result<serde_json::Map<String, Value>, UsageError> {
    let root: Value = serde_json::from_str(body).map_err(|_| UsageError::Malformed {
        detail: "body is not JSON".into(),
    })?;
    match root {
        Value::Object(map) => Ok(map),
        _ => Err(UsageError::Malformed {
            detail: "top-level value is not an object".into(),
        }),
    }
}

// ── Anthropic parser ────────────────────────────────────────────────────────

/// `five_hour` → 18000, `seven_day…` → 604800, `<n>_<unit>` generally.
fn anthropic_duration_hint(key: &str) -> Option<u64> {
    let mut parts = key.splitn(3, '_');
    let count = parts.next()?;
    let unit = parts.next()?;
    let n: u64 = match count {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "twelve" => 12,
        "thirty" => 30,
        other => other.parse().ok()?,
    };
    let unit_secs = match unit {
        "minute" | "minutes" => SECS_PER_MINUTE,
        "hour" | "hours" => SECS_PER_HOUR,
        "day" | "days" => SECS_PER_DAY,
        "week" | "weeks" => SECS_PER_WEEK,
        _ => return None,
    };
    n.checked_mul(unit_secs)
}

/// The Anthropic scope for a window key: `seven_day_sonnet` → Model, `…_oauth_apps` → Feature.
fn anthropic_scope(key: &str) -> WindowScope {
    if key == "extra_usage" {
        return WindowScope::Feature {
            feature: "extra_usage".into(),
        };
    }
    let mut parts = key.splitn(3, '_');
    let (_count, _unit, rest) = (parts.next(), parts.next(), parts.next());
    match rest {
        None => WindowScope::Account,
        Some("oauth_apps") => WindowScope::Feature {
            feature: "oauth_apps".into(),
        },
        Some(model) if !model.is_empty() => WindowScope::Model {
            model: model.to_string(),
        },
        Some(_) => WindowScope::Account,
    }
}

/// Parse `GET /api/oauth/usage`. Every object value carrying `utilization`
/// or `resets_at` is a window; `null` windows are skipped with a note.
pub fn parse_anthropic_usage(
    body: &str,
    account_label: &str,
    observed_at: u64,
) -> Result<UsageSnapshot, UsageError> {
    let map = parse_json_object(body)?;
    let mut snap = UsageSnapshot::empty(OAuthProviderId::Anthropic, account_label, observed_at);
    let mut skipped_null = 0usize;
    let mut unrecognized = 0usize;
    for (key, val) in &map {
        match val {
            Value::Object(w) if w.contains_key("utilization") || w.contains_key("resets_at") => {
                let used_percent = UsedPercent::from_json(w.get("utilization"));
                let reset_at = match w.get("resets_at") {
                    None | Some(Value::Null) => None,
                    Some(v) => {
                        let parsed = epoch_ms_from_value(Some(v));
                        if parsed.is_none() {
                            snap.note("reset timestamp unparseable");
                        }
                        parsed
                    }
                };
                let duration_secs = anthropic_duration_hint(key);
                snap.windows.push(UsageWindow {
                    id: key.clone(),
                    label: duration_secs
                        .map(duration_label)
                        .unwrap_or_else(|| "unknown".into()),
                    scope: anthropic_scope(key),
                    duration_secs,
                    used_percent,
                    used: None,
                    limit: None,
                    reset_at,
                    reset_kind: ResetKind::QuotaWindow,
                    limit_reached: None,
                });
            }
            Value::Null => skipped_null += 1,
            _ => unrecognized += 1,
        }
    }
    if skipped_null > 0 {
        snap.note("null windows skipped");
    }
    if unrecognized > 0 {
        snap.note("unrecognized fields ignored");
    }
    if snap.windows.is_empty() {
        snap.note("no usage windows in response");
    }
    snap.derive_model_availability();
    Ok(snap)
}

// ── Codex parser ────────────────────────────────────────────────────────────

/// One `RateLimitWindowSnapshot`: `used_percent`, `limit_window_seconds`,
/// `reset_after_seconds`, `reset_at` (unix seconds).
fn codex_window(
    snap: &mut UsageSnapshot,
    id: &str,
    scope: WindowScope,
    raw: &Value,
    observed_at: u64,
) -> Option<UsageWindow> {
    let w = match raw {
        Value::Object(w) => w,
        Value::Null => {
            snap.note("null window skipped");
            return None;
        }
        _ => {
            snap.note("window is not an object");
            return None;
        }
    };
    let used_percent = UsedPercent::from_json(w.get("used_percent"));
    let duration_secs = w.get("limit_window_seconds").and_then(u64_from_value);
    let mut reset_at = epoch_ms_from_value(w.get("reset_at"));
    if reset_at.is_none() {
        if let Some(after) = w.get("reset_after_seconds").and_then(u64_from_value) {
            reset_at = observed_at.checked_add(after.saturating_mul(1000));
            snap.note("reset derived from reset_after_seconds");
        }
    }
    Some(UsageWindow {
        id: id.to_string(),
        label: duration_secs
            .map(duration_label)
            .unwrap_or_else(|| "unknown".into()),
        scope,
        duration_secs,
        used_percent,
        used: None,
        limit: None,
        reset_at,
        reset_kind: ResetKind::QuotaWindow,
        limit_reached: None,
    })
}

/// One `RateLimitStatusPayload` (`allowed`, `limit_reached`, `primary_window`,
/// `secondary_window`) → up to two windows tagged `<prefix>primary` / `<prefix>secondary`.
fn codex_status_windows(
    snap: &mut UsageSnapshot,
    prefix: &str,
    scope: WindowScope,
    raw: &Value,
    observed_at: u64,
) -> Option<bool> {
    let status = raw.as_object()?;
    let limit_reached = bool_from_value(status.get("limit_reached"));
    for (suffix, key) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        match status.get(key) {
            None => {}
            Some(v) => {
                if let Some(mut w) =
                    codex_window(snap, &format!("{prefix}{suffix}"), scope.clone(), v, observed_at)
                {
                    w.limit_reached = limit_reached;
                    snap.windows.push(w);
                }
            }
        }
    }
    limit_reached
}

fn codex_banked_resets(raw: &Value) -> Option<BankedResets> {
    let obj = raw.as_object()?;
    let available_count = obj.get("available_count").and_then(u64_from_value);
    let credits = obj
        .get("credits")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_object)
                .map(|c| BankedResetCredit {
                    reset_type: string_from_value(c.get("reset_type")),
                    granted_at: epoch_ms_from_value(c.get("granted_at")),
                    expires_at: epoch_ms_from_value(c.get("expires_at")),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(BankedResets {
        available_count,
        credits,
        inventory_error: None,
    })
}

/// Parse `GET /wham/usage`. `expected_account_id` is the id derived from the
/// bearer token; a differing response `account_id` fails closed.
pub fn parse_codex_usage(
    body: &str,
    account_label: &str,
    observed_at: u64,
    expected_account_id: Option<&str>,
) -> Result<UsageSnapshot, UsageError> {
    let map = parse_json_object(body)?;
    let mut snap = UsageSnapshot::empty(OAuthProviderId::OpenAiCodex, account_label, observed_at);

    if let Some(id) = string_from_value(map.get("account_id")) {
        if let Some(expected) = expected_account_id {
            if id != expected {
                return Err(UsageError::IdentityMismatch);
            }
        }
        snap.identity_prefix = Some(identity_prefix(&id));
    } else if let Some(expected) = expected_account_id {
        snap.identity_prefix = Some(identity_prefix(expected));
    }

    // Only the caller-paired credential identity authorizes spending. A body
    // claiming an id without an expected token identity is display-only.
    snap.seat_fingerprint = expected_account_id
        .and_then(|id| super::seat_fingerprint(OAuthProviderId::OpenAiCodex, id));

    snap.plan = string_from_value(map.get("plan_type"));

    match map.get("rate_limit") {
        Some(rl @ Value::Object(_)) => {
            snap.limit_reached = codex_status_windows(&mut snap, "", WindowScope::Account, rl, observed_at);
        }
        Some(Value::Null) | None => snap.note("rate_limit missing"),
        Some(_) => snap.note("rate_limit is not an object"),
    }

    if let Some(cr @ Value::Object(_)) = map.get("code_review_rate_limit") {
        codex_status_windows(
            &mut snap,
            "code_review.",
            WindowScope::Feature {
                feature: "code_review".into(),
            },
            cr,
            observed_at,
        );
    }

    if let Some(extra) = map.get("additional_rate_limits").and_then(Value::as_array) {
        for (i, item) in extra.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                snap.note("additional rate limit is not an object");
                continue;
            };
            let name = string_from_value(obj.get("limit_name"))
                .or_else(|| string_from_value(obj.get("metered_feature")))
                .unwrap_or_else(|| format!("additional[{i}]"));
            let scope = match string_from_value(obj.get("normal_model_slug")) {
                Some(model) => WindowScope::Model { model },
                None => WindowScope::Feature {
                    feature: name.clone(),
                },
            };
            if let Some(rl) = obj.get("rate_limit") {
                codex_status_windows(&mut snap, &format!("{name}."), scope, rl, observed_at);
            }
        }
    }

    // Explicit per-model gating (e.g. `{"gpt-6-astra": {"available": false,
    // "available_at": "…", "credits_would_enable": true}}`). This is the
    // authoritative statement for premium models and must be parsed before
    // windows are folded in so the merge never overwrites it.
    match map.get("model_usage") {
        Some(Value::Object(models)) => {
            for (model, raw) in models {
                let Some(entry) = raw.as_object() else {
                    snap.note("model usage entry is not an object");
                    continue;
                };
                let availability = match entry.get("available") {
                    Some(Value::Bool(true)) => Availability::Available,
                    Some(Value::Bool(false)) => Availability::Exhausted,
                    _ => Availability::Unknown,
                };
                let reset_at = match entry.get("available_at") {
                    None | Some(Value::Null) => None,
                    Some(v) => {
                        let parsed = epoch_ms_from_value(Some(v));
                        if parsed.is_none() {
                            snap.note("reset timestamp unparseable");
                        }
                        parsed
                    }
                };
                snap.model_availability.push(ModelAvailability {
                    model: model.clone(),
                    availability,
                    used_percent: UsedPercent::from_json(entry.get("used_percent")),
                    reset_at,
                    credits_would_enable: bool_from_value(entry.get("credits_would_enable")),
                    source: "model_usage".into(),
                });
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => snap.note("model usage field is not an object"),
    }

    // Spend control: `reached` at the top of `spend_control` or on any nested
    // limit object trips the flag (fail closed). A nested limit with
    // `remaining_percent`/`resets_at` becomes a feature-scoped window.
    match map.get("spend_control") {
        Some(Value::Object(sc)) => {
            let mut reached = bool_from_value(sc.get("reached"));
            for (_key, nested) in sc {
                let Some(limit) = nested.as_object() else {
                    continue;
                };
                if bool_from_value(limit.get("reached")) == Some(true) {
                    reached = Some(true);
                }
                let remaining = limit.get("remaining_percent").and_then(number_from_value);
                let resets_at = epoch_ms_from_value(limit.get("resets_at"));
                if remaining.is_some() || resets_at.is_some() {
                    snap.windows.push(UsageWindow {
                        id: "spend_control".into(),
                        label: "spend control".into(),
                        scope: WindowScope::Feature {
                            feature: "spend_control".into(),
                        },
                        duration_secs: None,
                        used_percent: match remaining {
                            Some(r) => UsedPercent::from_f64(100.0 - r),
                            None => UsedPercent::unknown("missing"),
                        },
                        used: None,
                        limit: limit.get("limit").and_then(number_from_value),
                        reset_at: resets_at,
                        reset_kind: ResetKind::BillingRenewal,
                        limit_reached: bool_from_value(limit.get("reached")),
                    });
                }
            }
            snap.spend_control_reached = reached;
        }
        Some(Value::Null) | None => {}
        Some(_) => snap.note("spend control field is not an object"),
    }

    if let Some(credits) = map.get("credits").and_then(Value::as_object) {
        snap.credits = Some(CreditBalance {
            kind: "codex_credits".into(),
            has_credits: bool_from_value(credits.get("has_credits")),
            unlimited: bool_from_value(credits.get("unlimited")),
            remaining: credits.get("balance").and_then(number_from_value),
            total: None,
            unit: Some("credits".into()),
            renews_at: None,
        });
    }

    match map.get("rate_limit_reset_credits") {
        Some(v @ Value::Object(_)) => snap.banked_resets = codex_banked_resets(v),
        Some(Value::Null) | None => {}
        Some(_) => snap.note("reset credits field is not an object"),
    }

    if snap.windows.is_empty() {
        snap.note("no usage windows in response");
    }
    snap.derive_model_availability();
    Ok(snap)
}

/// Parse `GET /wham/rate-limit-reset-credits` (inventory detail).
pub fn parse_codex_reset_credits(body: &str) -> Result<BankedResets, UsageError> {
    let map = parse_json_object(body)?;
    codex_banked_resets(&Value::Object(map)).ok_or_else(|| UsageError::Malformed {
        detail: "reset credits payload has unexpected shape".into(),
    })
}

// ── Kimi parser ─────────────────────────────────────────────────────────────

fn kimi_unit_secs(raw: Option<&Value>) -> Option<u64> {
    match raw?.as_str()? {
        "TIME_UNIT_MINUTE" | "minute" => Some(SECS_PER_MINUTE),
        "TIME_UNIT_HOUR" | "hour" => Some(SECS_PER_HOUR),
        "TIME_UNIT_DAY" | "day" => Some(SECS_PER_DAY),
        "TIME_UNIT_WEEK" | "week" => Some(SECS_PER_WEEK),
        _ => None,
    }
}

fn kimi_window_secs(raw: Option<&Value>) -> Option<u64> {
    let w = raw?.as_object()?;
    let duration = w.get("duration").and_then(u64_from_value)?;
    let unit = kimi_unit_secs(w.get("timeUnit"))?;
    duration.checked_mul(unit)
}

/// `{used, limit, resetTime}` (numbers or numeric strings) → window.
fn kimi_row(
    snap: &mut UsageSnapshot,
    id: &str,
    label_hint: Option<String>,
    duration_secs: Option<u64>,
    raw: Option<&Value>,
) -> Option<UsageWindow> {
    let row = raw?.as_object()?;
    let used = row.get("used").and_then(number_from_value);
    let limit = row.get("limit").and_then(number_from_value);
    if used.is_none() && limit.is_none() {
        return None;
    }
    let reset_at = match row.get("resetTime") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let parsed = epoch_ms_from_value(Some(v));
            if parsed.is_none() {
                snap.note("reset timestamp unparseable");
            }
            parsed
        }
    };
    let label = label_hint
        .or_else(|| duration_secs.map(duration_label))
        .unwrap_or_else(|| "unknown".into());
    Some(UsageWindow {
        id: id.to_string(),
        label,
        scope: WindowScope::Account,
        duration_secs,
        used_percent: UsedPercent::from_ratio(used, limit),
        used,
        limit,
        reset_at,
        reset_kind: ResetKind::QuotaWindow,
        limit_reached: None,
    })
}

/// Parse `GET /coding/v1/usages` (official CLI `parseManagedUsagePayload`).
pub fn parse_kimi_usage(
    body: &str,
    account_label: &str,
    observed_at: u64,
) -> Result<UsageSnapshot, UsageError> {
    let map = parse_json_object(body)?;
    let mut snap = UsageSnapshot::empty(OAuthProviderId::KimiCode, account_label, observed_at);

    // Summary row: the official CLI treats a summary without a window as 1 week.
    if let Some(w) = kimi_row(&mut snap, "usage", None, Some(SECS_PER_WEEK), map.get("usage")) {
        snap.windows.push(w);
    } else if map.contains_key("usage") {
        snap.note("summary usage row unparseable");
    }

    match map.get("limits") {
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let Some(obj) = item.as_object() else {
                    snap.note("limit row is not an object");
                    continue;
                };
                let duration = kimi_window_secs(obj.get("window"));
                let name = string_from_value(obj.get("name")).or_else(|| {
                    obj.get("detail")
                        .and_then(Value::as_object)
                        .and_then(|d| string_from_value(d.get("name")))
                });
                let id = format!("limits[{i}]");
                match kimi_row(&mut snap, &id, name, duration, obj.get("detail")) {
                    Some(w) => snap.windows.push(w),
                    None => snap.note("limit row unparseable"),
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => snap.note("limits field is not an array"),
    }

    if let Some(wallet) = map.get("boosterWallet").and_then(Value::as_object) {
        if let Some(balance) = wallet.get("balance").and_then(Value::as_object) {
            // Fixed-point amounts (1e6 = one cent in the official CLI).
            const FIXED_POINT_PER_CENT: f64 = 1.0e6;
            let total = balance
                .get("amount")
                .and_then(number_from_value)
                .map(|v| v / FIXED_POINT_PER_CENT / 100.0);
            let remaining = balance
                .get("amountLeft")
                .and_then(number_from_value)
                .map(|v| v / FIXED_POINT_PER_CENT / 100.0);
            let currency = ["monthlyChargeLimit", "monthlyUsed"]
                .iter()
                .filter_map(|k| wallet.get(*k).and_then(Value::as_object))
                .find_map(|m| string_from_value(m.get("currency")))
                .unwrap_or_else(|| "USD".into());
            if total.is_some() || remaining.is_some() {
                snap.credits = Some(CreditBalance {
                    kind: "kimi_booster".into(),
                    has_credits: remaining.map(|r| r > 0.0),
                    unlimited: None,
                    remaining,
                    total,
                    unit: Some(currency),
                    renews_at: None,
                });
            }
        }
    }

    if snap.windows.is_empty() {
        snap.note("no usage windows in response");
    }
    snap.derive_model_availability();
    Ok(snap)
}

// ── Grok parser (schema unverified live; QuotaKit-documented shape) ─────────

/// Look up `key` in `config` first, then at the root.
fn grok_lookup<'a>(
    root: &'a serde_json::Map<String, Value>,
    config: Option<&'a serde_json::Map<String, Value>>,
    key: &str,
) -> Option<&'a Value> {
    config
        .and_then(|c| c.get(key))
        .filter(|v| !v.is_null())
        .or_else(|| root.get(key).filter(|v| !v.is_null()))
}

/// `{ "val": 12 }` or a bare number/string.
fn grok_val(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Object(o) => o.get("val").and_then(number_from_value),
        other => number_from_value(other),
    }
}

/// Parse `GET /v1/billing?format=credits`.
///
/// Recognized: `config.creditUsagePercent` (preferred), else
/// `onDemandUsed.val / onDemandCap.val * 100`; reset from
/// `config.currentPeriod.end` or `config.billingPeriodEnd`; duration from
/// `billingPeriodMinutes`. Each field is also accepted at the root. Without a
/// recognizable fraction the window is explicitly `Unknown`.
pub fn parse_grok_billing(
    body: &str,
    account_label: &str,
    observed_at: u64,
) -> Result<UsageSnapshot, UsageError> {
    let map = parse_json_object(body)?;
    let mut snap = UsageSnapshot::empty(OAuthProviderId::Xai, account_label, observed_at);
    let config = map.get("config").and_then(Value::as_object);

    let used = grok_val(grok_lookup(&map, config, "onDemandUsed"));
    let cap = grok_val(grok_lookup(&map, config, "onDemandCap"));

    let percent_field = grok_lookup(&map, config, "creditUsagePercent");
    let used_percent = match percent_field {
        Some(v) => match number_from_value(v) {
            Some(n) => UsedPercent::from_f64(n),
            None => UsedPercent::unknown("not a number"),
        },
        None => match (used, cap) {
            (Some(_), Some(_)) => {
                snap.note("percent derived from on-demand counters");
                UsedPercent::from_ratio(used, cap)
            }
            _ => UsedPercent::unknown("missing"),
        },
    };

    let reset_at = grok_lookup(&map, config, "currentPeriod")
        .and_then(Value::as_object)
        .and_then(|p| epoch_ms_from_value(p.get("end")))
        .or_else(|| epoch_ms_from_value(grok_lookup(&map, config, "billingPeriodEnd")));
    let duration_secs = grok_lookup(&map, config, "billingPeriodMinutes")
        .and_then(u64_from_value)
        .and_then(|m| m.checked_mul(SECS_PER_MINUTE));

    // The schema counts as recognized when any documented field is present,
    // even if its value failed validation (the window is then Unknown).
    let recognized = percent_field.is_some()
        || reset_at.is_some()
        || duration_secs.is_some()
        || used.is_some()
        || cap.is_some();
    if recognized {
        snap.windows.push(UsageWindow {
            id: "credits".into(),
            label: duration_secs
                .map(duration_label)
                .unwrap_or_else(|| "billing period".into()),
            scope: WindowScope::Account,
            duration_secs,
            used_percent,
            used,
            limit: cap,
            reset_at,
            reset_kind: ResetKind::BillingRenewal,
            limit_reached: None,
        });
        if used.is_some() || cap.is_some() {
            snap.credits = Some(CreditBalance {
                kind: "grok_credits".into(),
                has_credits: match (used, cap) {
                    (Some(u), Some(c)) => Some(u < c),
                    _ => None,
                },
                unlimited: None,
                remaining: match (used, cap) {
                    (Some(u), Some(c)) => Some((c - u).max(0.0)),
                    _ => None,
                },
                total: cap,
                unit: Some("credits".into()),
                renews_at: reset_at,
            });
        }
    } else {
        snap.note("unrecognized billing schema");
    }
    snap.note("grok billing schema unverified against a live account");
    snap.derive_model_availability();
    Ok(snap)
}

// ── Fetch ───────────────────────────────────────────────────────────────────

/// Options for [`fetch_usage`]. Production callers use `Default`.
#[derive(Debug, Clone)]
pub struct UsageFetchOptions {
    /// Test seam: replace the pinned URL. **Loopback hosts only** —
    /// anything else fails with [`UsageError::InvalidEndpointOverride`]
    /// before any request is built.
    pub endpoint_override: Option<String>,
    /// Test seam for the Codex inventory URL; same loopback rule.
    pub inventory_endpoint_override: Option<String>,
    /// Also `GET` the Codex banked-reset inventory. Failures there are
    /// recorded in `banked_resets.inventory_error`, never fatal.
    pub include_inventory: bool,
    pub timeout: Duration,
    pub max_body_bytes: usize,
}

impl Default for UsageFetchOptions {
    fn default() -> Self {
        Self {
            endpoint_override: None,
            inventory_endpoint_override: None,
            include_inventory: false,
            timeout: USAGE_REQUEST_TIMEOUT,
            max_body_bytes: USAGE_MAX_BODY_BYTES,
        }
    }
}

/// True for `http(s)://127.0.0.1…`, `localhost`, `[::1]`.
fn is_loopback_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn resolve_endpoint(pinned: &str, override_url: Option<&str>) -> Result<String, UsageError> {
    match override_url {
        None => Ok(pinned.to_string()),
        Some(u) if is_loopback_url(u) => Ok(u.to_string()),
        Some(_) => Err(UsageError::InvalidEndpointOverride),
    }
}

/// HTTP client with the policy this module requires: no redirects, bounded
/// connect/total timeouts, built-in roots. Construct once and reuse.
#[derive(Clone)]
pub struct UsageClient {
    http: reqwest::Client,
}

impl std::fmt::Debug for UsageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UsageClient")
    }
}

impl UsageClient {
    pub fn new() -> Result<Self, UsageError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(USAGE_CONNECT_TIMEOUT)
            .timeout(USAGE_REQUEST_TIMEOUT)
            .tls_built_in_webpki_certs(true)
            .tls_built_in_native_certs(true)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| UsageError::Transport {
                detail: "could not build HTTP client".into(),
            })?;
        Ok(Self { http })
    }
}

fn classify_reqwest_error(e: &reqwest::Error) -> UsageError {
    if e.is_timeout() {
        UsageError::Timeout
    } else {
        let detail = if e.is_connect() {
            "connect"
        } else if e.is_request() {
            "request"
        } else if e.is_body() || e.is_decode() {
            "body"
        } else if e.is_builder() {
            "builder"
        } else {
            "unknown"
        };
        UsageError::Transport {
            detail: detail.into(),
        }
    }
}

/// Stream the body into a capped buffer. Fail closed on overflow.
async fn read_body_capped(resp: reqwest::Response, cap: usize) -> Result<String, UsageError> {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| classify_reqwest_error(&e))?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(UsageError::BodyTooLarge { cap });
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| UsageError::Malformed {
        detail: "body is not UTF-8".into(),
    })
}

/// Send one pinned GET; classify status; return the capped body text.
async fn get_json_text(
    client: &UsageClient,
    url: &str,
    headers: &[(&str, String)],
    opts: &UsageFetchOptions,
) -> Result<String, UsageError> {
    let mut req = client
        .http
        .get(url)
        .timeout(opts.timeout)
        .header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.map_err(|e| classify_reqwest_error(&e))?;
    let status = resp.status().as_u16();
    // Non-2xx bodies are upstream-controlled and may echo the request: dropped unread.
    match status {
        200..=299 => {}
        300..=399 => {
            drop(resp);
            return Err(UsageError::Redirected { status });
        }
        401 | 403 => {
            drop(resp);
            return Err(UsageError::Unauthorized { status });
        }
        429 => {
            drop(resp);
            return Err(UsageError::RateLimited);
        }
        _ => {
            drop(resp);
            return Err(UsageError::UpstreamStatus { status });
        }
    }
    read_body_capped(resp, opts.max_body_bytes).await
}

/// Read-only usage fetch for one `(provider, account)` using the caller's
/// already-resolved access token. See the module docs for the guarantees.
pub async fn fetch_usage(
    client: &UsageClient,
    provider: OAuthProviderId,
    account_label: &str,
    access_token: &str,
    opts: &UsageFetchOptions,
) -> Result<UsageSnapshot, UsageError> {
    if !supports_usage(provider) {
        return Err(UsageError::UnsupportedProvider {
            provider: provider.as_str().to_string(),
        });
    }
    let bearer = format!("Bearer {access_token}");
    match provider {
        OAuthProviderId::Anthropic => {
            let url = resolve_endpoint(ANTHROPIC_USAGE_URL, opts.endpoint_override.as_deref())?;
            let headers = [
                ("Authorization", bearer),
                ("anthropic-beta", ANTHROPIC_OAUTH_BETA.to_string()),
            ];
            let body = get_json_text(client, &url, &headers, opts).await?;
            parse_anthropic_usage(&body, account_label, crate::epoch_millis())
        }
        OAuthProviderId::OpenAiCodex => {
            // The account header is derived from THE SAME token that carries
            // the bearer, so header and credential can never disagree.
            let account_id = super::openai_codex::extract_account_id(access_token)
                .ok_or(UsageError::AccountIdUnavailable)?;
            let url = resolve_endpoint(CODEX_USAGE_URL, opts.endpoint_override.as_deref())?;
            let inventory_url = resolve_endpoint(
                CODEX_RESET_CREDITS_URL,
                opts.inventory_endpoint_override.as_deref(),
            )?;
            let headers = [
                ("Authorization", bearer),
                (CODEX_ACCOUNT_HEADER, account_id.clone()),
            ];
            let body = get_json_text(client, &url, &headers, opts).await?;
            let mut snap =
                parse_codex_usage(&body, account_label, crate::epoch_millis(), Some(&account_id))?;
            if opts.include_inventory {
                let inventory = match get_json_text(client, &inventory_url, &headers, opts).await {
                    Ok(text) => parse_codex_reset_credits(&text),
                    Err(e) => Err(e),
                };
                let banked = snap.banked_resets.get_or_insert(BankedResets {
                    available_count: None,
                    credits: Vec::new(),
                    inventory_error: None,
                });
                match inventory {
                    Ok(detail) => {
                        if detail.available_count.is_some() {
                            banked.available_count = detail.available_count;
                        }
                        banked.credits = detail.credits;
                        banked.inventory_error = None;
                    }
                    Err(e) => {
                        // Partial: the usage snapshot stands; only inventory is missing.
                        banked.inventory_error = Some(e.kind().to_string());
                    }
                }
            }
            Ok(snap)
        }
        OAuthProviderId::KimiCode => {
            // The official CLI's usage fetch sends bearer + Accept only; no
            // device-identity headers, so no device-id state is touched.
            let url = resolve_endpoint(KIMI_USAGE_URL, opts.endpoint_override.as_deref())?;
            let headers = [("Authorization", bearer)];
            let body = get_json_text(client, &url, &headers, opts).await?;
            parse_kimi_usage(&body, account_label, crate::epoch_millis())
        }
        OAuthProviderId::Xai => {
            let url = resolve_endpoint(GROK_BILLING_URL, opts.endpoint_override.as_deref())?;
            let headers = [
                ("Authorization", bearer),
                (GROK_TOKEN_AUTH_HEADER, GROK_TOKEN_AUTH_VALUE.to_string()),
            ];
            let body = get_json_text(client, &url, &headers, opts).await?;
            parse_grok_billing(&body, account_label, crate::epoch_millis())
        }
        OAuthProviderId::GitHubCopilot | OAuthProviderId::GoogleGemini => {
            Err(UsageError::UnsupportedProvider {
                provider: provider.as_str().to_string(),
            })
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    const T0: u64 = 1_758_400_000_000; // 2025-09-20T20:26:40Z

    fn secret_free(text: &str, secrets: &[&str]) {
        for s in secrets {
            assert!(!text.contains(s), "secret material leaked: {text}");
        }
    }

    // ── UsedPercent ─────────────────────────────────────────────────────

    #[test]
    fn used_percent_validation() {
        assert_eq!(UsedPercent::from_f64(0.0).valid(), Some(0.0));
        assert_eq!(UsedPercent::from_f64(100.0).valid(), Some(100.0));
        assert_eq!(UsedPercent::from_f64(100.5), UsedPercent::unknown("out of range"));
        assert_eq!(UsedPercent::from_f64(-0.1), UsedPercent::unknown("out of range"));
        assert_eq!(UsedPercent::from_f64(f64::NAN), UsedPercent::unknown("not a number"));
        assert_eq!(UsedPercent::from_f64(f64::INFINITY), UsedPercent::unknown("not a number"));
        assert_eq!(UsedPercent::from_json(None), UsedPercent::unknown("missing"));
        assert_eq!(UsedPercent::from_json(Some(&Value::Null)), UsedPercent::unknown("null"));
        assert_eq!(
            UsedPercent::from_json(Some(&serde_json::json!("abc"))),
            UsedPercent::unknown("not a number")
        );
        assert_eq!(UsedPercent::from_json(Some(&serde_json::json!("42.5"))).valid(), Some(42.5));
        assert_eq!(UsedPercent::from_ratio(Some(5.0), Some(0.0)), UsedPercent::unknown("limit is zero"));
        assert_eq!(UsedPercent::from_ratio(Some(25.0), Some(100.0)).valid(), Some(25.0));
    }

    #[test]
    fn used_percent_json_is_tagged_never_bare() {
        let v = serde_json::to_value(UsedPercent::Valid { percent: 12.5 }).unwrap();
        assert_eq!(v, serde_json::json!({"state":"valid","percent":12.5}));
        let u = serde_json::to_value(UsedPercent::unknown("null")).unwrap();
        assert_eq!(u, serde_json::json!({"state":"unknown","reason":"null"}));
        let back: UsedPercent = serde_json::from_value(v).unwrap();
        assert_eq!(back.valid(), Some(12.5));
    }

    #[test]
    fn epoch_helpers() {
        assert_eq!(epoch_ms_from_rfc3339("2025-09-20T20:26:40Z"), Some(T0));
        assert_eq!(epoch_ms_from_rfc3339("2025-09-20T22:26:40+02:00"), Some(T0));
        assert_eq!(epoch_ms_from_rfc3339("not a date"), None);
        assert_eq!(epoch_ms_from_value(Some(&serde_json::json!(1_758_400_000))), Some(T0));
        assert_eq!(epoch_ms_from_value(Some(&serde_json::json!(1_758_400_000_000u64))), Some(T0));
        assert_eq!(epoch_ms_from_value(Some(&serde_json::json!("1758400000"))), Some(T0));
        assert_eq!(epoch_ms_from_value(Some(&Value::Null)), None);
        assert_eq!(epoch_ms_from_value(Some(&serde_json::json!(-5))), None);
        assert_eq!(epoch_ms_from_value(None), None);
        assert_eq!(duration_label(18000), "5h");
        assert_eq!(duration_label(604800), "7d");
        assert_eq!(duration_label(300), "5m");
        assert_eq!(duration_label(90), "90s");
    }

    // ── Anthropic ───────────────────────────────────────────────────────

    const ANTHROPIC_FIXTURE: &str = r#"{
        "five_hour": {"utilization": 12.5, "resets_at": "2025-09-20T22:00:00Z"},
        "seven_day": {"utilization": 60, "resets_at": "2025-09-24T00:00:00Z"},
        "seven_day_oauth_apps": {"utilization": 3, "resets_at": "2025-09-24T00:00:00Z"},
        "seven_day_opus": null,
        "seven_day_sonnet": {"utilization": 100, "resets_at": "2025-09-24T00:00:00Z"},
        "extra_usage": {"is_enabled": false}
    }"#;

    #[test]
    fn anthropic_valid_fixture() {
        let s = parse_anthropic_usage(ANTHROPIC_FIXTURE, "default", T0).unwrap();
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.account, "default");
        assert_eq!(s.observed_at, T0);
        assert_eq!(s.windows.len(), 4);
        let fh = s.window("five_hour").unwrap();
        assert_eq!(fh.duration_secs, Some(18000));
        assert_eq!(fh.label, "5h");
        assert_eq!(fh.used_percent.valid(), Some(12.5));
        assert_eq!(fh.reset_at, epoch_ms_from_rfc3339("2025-09-20T22:00:00Z"));
        assert_eq!(fh.scope, WindowScope::Account);
        let sd = s.window("seven_day").unwrap();
        assert_eq!(sd.duration_secs, Some(604800));
        assert_eq!(sd.scope, WindowScope::Account);
        assert_eq!(
            s.window("seven_day_oauth_apps").unwrap().scope,
            WindowScope::Feature { feature: "oauth_apps".into() }
        );
        let sonnet = s.window("seven_day_sonnet").unwrap();
        assert_eq!(sonnet.scope, WindowScope::Model { model: "sonnet".into() });
        assert!(sonnet.is_exhausted());
        assert_eq!(s.model_availability.len(), 1);
        assert_eq!(s.model_availability[0].model, "sonnet");
        assert_eq!(s.model_availability[0].availability, Availability::Exhausted);
        assert!(s.notes.iter().any(|n| n == "null windows skipped"));
        assert!(s.notes.iter().any(|n| n == "unrecognized fields ignored"));
        assert_eq!(s.limit_reached, None);
        // Account-level capacity: 12.5% and 60% → proven.
        assert!(s.has_proven_capacity(T0 + 1000, 60_000));
        assert_eq!(s.longest_account_window().unwrap().id, "seven_day");
        assert_eq!(s.earliest_reset_at(), epoch_ms_from_rfc3339("2025-09-20T22:00:00Z"));
    }

    #[test]
    fn anthropic_missing_null_malformed_percent_and_reset() {
        let body = r#"{
            "five_hour": {"resets_at": "2025-09-20T22:00:00Z"},
            "seven_day": {"utilization": null, "resets_at": null},
            "seven_day_sonnet": {"utilization": "lots", "resets_at": "yesterday"},
            "seven_day_opus": {"utilization": 250, "resets_at": 1758400000},
            "one_day": {"utilization": -1}
        }"#;
        let s = parse_anthropic_usage(body, "a", T0).unwrap();
        assert_eq!(s.window("five_hour").unwrap().used_percent, UsedPercent::unknown("missing"));
        assert_eq!(s.window("seven_day").unwrap().used_percent, UsedPercent::unknown("null"));
        assert_eq!(s.window("seven_day").unwrap().reset_at, None);
        let sonnet = s.window("seven_day_sonnet").unwrap();
        assert_eq!(sonnet.used_percent, UsedPercent::unknown("not a number"));
        assert_eq!(sonnet.reset_at, None);
        assert!(s.notes.iter().any(|n| n == "reset timestamp unparseable"));
        let opus = s.window("seven_day_opus").unwrap();
        assert_eq!(opus.used_percent, UsedPercent::unknown("out of range"));
        assert_eq!(opus.reset_at, Some(T0), "unix seconds accepted");
        assert_eq!(s.window("one_day").unwrap().duration_secs, Some(86400));
        assert_eq!(s.window("one_day").unwrap().used_percent, UsedPercent::unknown("out of range"));
        // Unknown is not capacity.
        assert!(!s.has_proven_capacity(T0, 60_000));
        assert!(s
            .model_availability
            .iter()
            .all(|m| m.availability == Availability::Unknown));
        // Notes never carry raw values.
        secret_free(&format!("{:?}", s.notes), &["lots", "yesterday"]);
    }

    #[test]
    fn anthropic_rejects_non_json_and_non_object() {
        assert_eq!(
            parse_anthropic_usage("<html>", "a", T0).unwrap_err(),
            UsageError::Malformed { detail: "body is not JSON".into() }
        );
        assert_eq!(
            parse_anthropic_usage("[1,2]", "a", T0).unwrap_err(),
            UsageError::Malformed { detail: "top-level value is not an object".into() }
        );
        let empty = parse_anthropic_usage("{}", "a", T0).unwrap();
        assert!(empty.windows.is_empty());
        assert!(empty.notes.iter().any(|n| n == "no usage windows in response"));
        assert!(!empty.has_proven_capacity(T0, 60_000));
    }

    // ── Codex ───────────────────────────────────────────────────────────

    fn codex_fixture(secondary: &str) -> String {
        format!(
            r#"{{
            "plan_type": "pro",
            "account_id": "acct_1234567890",
            "rate_limit": {{
                "allowed": true,
                "limit_reached": false,
                "primary_window": {{"used_percent": 37, "limit_window_seconds": 604800,
                                    "reset_after_seconds": 100000, "reset_at": 1758500000}},
                "secondary_window": {secondary}
            }},
            "code_review_rate_limit": {{
                "allowed": true, "limit_reached": true,
                "primary_window": {{"used_percent": 100, "limit_window_seconds": 18000, "reset_after_seconds": 120}}
            }},
            "additional_rate_limits": [
                {{"limit_name": "codex-auto-review", "metered_feature": "auto_review",
                  "rate_limit": {{"allowed": true, "limit_reached": false,
                                 "primary_window": {{"used_percent": 5, "limit_window_seconds": 86400, "reset_at": 1758450000}}}}}},
                {{"limit_name": "gpt-5-mini", "normal_model_slug": "gpt-5-mini",
                  "rate_limit": {{"limit_reached": false,
                                 "primary_window": {{"used_percent": 99.5, "limit_window_seconds": 18000}}}}}}
            ],
            "credits": {{"has_credits": true, "unlimited": false, "balance": "12.50"}},
            "rate_limit_reset_credits": {{"available_count": 2}}
        }}"#
        )
    }

    #[test]
    fn codex_weekly_primary_and_null_secondary() {
        let s = parse_codex_usage(&codex_fixture("null"), "astra2", T0, Some("acct_1234567890")).unwrap();
        assert_eq!(s.provider, "openai-codex");
        assert_eq!(s.account, "astra2");
        assert_eq!(s.plan.as_deref(), Some("pro"));
        assert_eq!(s.identity_prefix.as_deref(), Some("acct_123"));
        assert_eq!(s.seat_fingerprint,
            super::super::seat_fingerprint(OAuthProviderId::OpenAiCodex, "acct_1234567890"));
        let unpaired = parse_codex_usage(&codex_fixture("null"), "astra2", T0, None).unwrap();
        assert!(unpaired.seat_fingerprint.is_none());
        assert_eq!(s.limit_reached, Some(false));
        let p = s.window("primary").unwrap();
        assert_eq!(p.duration_secs, Some(604800), "primary is weekly, not assumed 5h");
        assert_eq!(p.label, "7d");
        assert_eq!(p.used_percent.valid(), Some(37.0));
        assert_eq!(p.reset_at, Some(1_758_500_000_000), "provider reset_at is authoritative");
        assert_eq!(p.limit_reached, Some(false));
        assert!(s.window("secondary").is_none());
        assert!(s.notes.iter().any(|n| n == "null window skipped"));
        // Feature-scoped code review window with reset derived from reset_after_seconds.
        let cr = s.window("code_review.primary").unwrap();
        assert_eq!(cr.scope, WindowScope::Feature { feature: "code_review".into() });
        assert_eq!(cr.reset_at, Some(T0 + 120_000));
        assert_eq!(cr.limit_reached, Some(true));
        assert!(cr.is_exhausted());
        assert!(s.notes.iter().any(|n| n == "reset derived from reset_after_seconds"));
        // Additional limits: feature + model scopes.
        assert_eq!(
            s.window("codex-auto-review.primary").unwrap().scope,
            WindowScope::Feature { feature: "codex-auto-review".into() }
        );
        let mini = s.window("gpt-5-mini.primary").unwrap();
        assert_eq!(mini.scope, WindowScope::Model { model: "gpt-5-mini".into() });
        assert_eq!(s.model_availability.len(), 1);
        assert_eq!(s.model_availability[0].availability, Availability::Available);
        // Credits and banked resets.
        let c = s.credits.as_ref().unwrap();
        assert_eq!(c.kind, "codex_credits");
        assert_eq!(c.remaining, Some(12.5));
        assert_eq!(c.has_credits, Some(true));
        let b = s.banked_resets.as_ref().unwrap();
        assert_eq!(b.available_count, Some(2));
        assert!(b.credits.is_empty());
        assert_eq!(b.inventory_error, None);
        // Only account-scoped windows count toward capacity: the exhausted
        // code-review feature window must not veto.
        assert!(s.has_proven_capacity(T0, 60_000));
        assert_eq!(s.longest_account_window().unwrap().id, "primary");
    }

    #[test]
    fn codex_secondary_present_and_limit_reached_blocks_capacity() {
        let body = codex_fixture(
            r#"{"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1758410000}"#,
        )
        .replace(r#""limit_reached": false,
                "primary_window""#, r#""limit_reached": true,
                "primary_window""#);
        let s = parse_codex_usage(&body, "x", T0, None).unwrap();
        let sec = s.window("secondary").unwrap();
        assert_eq!(sec.duration_secs, Some(18000));
        assert!(sec.is_exhausted());
        assert_eq!(s.limit_reached, Some(true));
        assert!(!s.has_proven_capacity(T0, 60_000));
        assert_eq!(s.earliest_reset_at(), Some(T0 + 120_000));
    }

    #[test]
    fn codex_identity_mismatch_fails_closed_and_absent_id_is_tolerated() {
        let err = parse_codex_usage(&codex_fixture("null"), "x", T0, Some("acct_other")).unwrap_err();
        assert_eq!(err, UsageError::IdentityMismatch);
        assert!(err.is_auth_failure());
        // Response without account_id: prefix comes from the token-derived id.
        let body = codex_fixture("null").replace(r#""account_id": "acct_1234567890","#, "");
        let s = parse_codex_usage(&body, "x", T0, Some("acct_zzzzzzzzz")).unwrap();
        assert_eq!(s.identity_prefix.as_deref(), Some("acct_zzz"));
    }

    #[test]
    fn codex_missing_and_malformed_fields_stay_unknown() {
        let body = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": null, "limit_window_seconds": "week", "reset_at": "soon"},
                "secondary_window": {"used_percent": 101}
            },
            "rate_limit_reset_credits": null,
            "credits": {"has_credits": "yes"}
        }"#;
        let s = parse_codex_usage(body, "x", T0, None).unwrap();
        let p = s.window("primary").unwrap();
        assert_eq!(p.used_percent, UsedPercent::unknown("null"));
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.label, "unknown");
        assert_eq!(p.reset_at, None);
        assert_eq!(s.window("secondary").unwrap().used_percent, UsedPercent::unknown("out of range"));
        assert_eq!(s.limit_reached, None);
        assert!(s.banked_resets.is_none());
        assert_eq!(s.credits.as_ref().unwrap().has_credits, None);
        assert!(!s.has_proven_capacity(T0, 60_000));
        secret_free(&format!("{:?}", s.notes), &["soon", "week"]);

        let none = parse_codex_usage(r#"{"plan_type": "plus"}"#, "x", T0, None).unwrap();
        assert!(none.windows.is_empty());
        assert!(none.notes.iter().any(|n| n == "rate_limit missing"));
        assert!(!none.has_proven_capacity(T0, 60_000));
    }

    /// Shape observed in a real prior `/wham/usage` response: generic quota is
    /// nearly unused, yet the premium model is gated with an `available_at`
    /// and a hint that purchased credits would unlock it.
    const CODEX_ASTRA_FIXTURE: &str = r#"{
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 10, "limit_window_seconds": 604800, "reset_at": 1758900000},
            "secondary_window": {"used_percent": 4, "limit_window_seconds": 18000, "reset_at": 1758410000}
        },
        "model_usage": {
            "gpt-6-astra": {"available": false, "available_at": "2026-01-05T09:00:00Z", "credits_would_enable": true},
            "gpt-5.1-codex": {"available": true},
            "gpt-5-mini": {"available": "maybe", "available_at": "later"},
            "broken": 7
        },
        "spend_control": {"reached": false,
                          "workspace": {"reached": false, "limit": 500, "remaining_percent": 80, "resets_at": 1759276800}},
        "credits": {"has_credits": false, "unlimited": false, "balance": "0"}
    }"#;

    #[test]
    fn codex_model_usage_gates_astra_while_generic_quota_has_room() {
        let s = parse_codex_usage(CODEX_ASTRA_FIXTURE, "astra1", T0, None).unwrap();
        // Generic (account) capacity is proven: 10% weekly, 4% five-hour.
        assert!(s.has_proven_capacity(T0, 60_000));
        // …but the gated premium model is NOT available on the subscription.
        let astra = s.model_availability_for("gpt-6-astra").unwrap();
        assert_eq!(astra.availability, Availability::Exhausted);
        assert_eq!(astra.reset_at, epoch_ms_from_rfc3339("2026-01-05T09:00:00Z"));
        assert_eq!(astra.credits_would_enable, Some(true));
        assert_eq!(astra.source, "model_usage");
        assert_eq!(astra.used_percent, UsedPercent::unknown("missing"));
        assert!(!s.has_proven_capacity_for_model("gpt-6-astra", T0, 60_000));
        assert!(!s.has_proven_capacity_for_model("GPT-6-Astra", T0, 60_000), "case-insensitive");
        // An explicitly available model passes; an unmentioned model falls back to account capacity.
        assert!(s.has_proven_capacity_for_model("gpt-5.1-codex", T0, 60_000));
        assert!(s.has_proven_capacity_for_model("gpt-5.1-codex-max", T0, 60_000), "unmentioned → account capacity");
        // Tri-state: non-boolean `available` is Unknown, and Unknown is not capacity.
        let mini = s.model_availability_for("gpt-5-mini").unwrap();
        assert_eq!(mini.availability, Availability::Unknown);
        assert_eq!(mini.reset_at, None);
        assert!(!s.has_proven_capacity_for_model("gpt-5-mini", T0, 60_000));
        assert!(s.notes.iter().any(|n| n == "reset timestamp unparseable"));
        assert!(s.notes.iter().any(|n| n == "model usage entry is not an object"));
        secret_free(&format!("{:?}", s.notes), &["maybe", "later", "broken"]);
        // Spend control not reached; nested limit becomes a feature window.
        assert_eq!(s.spend_control_reached, Some(false));
        let sc = s.window("spend_control").unwrap();
        assert_eq!(sc.used_percent.valid(), Some(20.0));
        assert_eq!(sc.reset_at, Some(1_759_276_800_000));
        assert_eq!(sc.reset_kind, ResetKind::BillingRenewal);
        // The gated model's available_at participates in earliest_reset_at only
        // if earlier than the windows; here the 5h window is earlier.
        assert_eq!(s.earliest_reset_at(), Some(1_758_410_000_000));
        // JSON carries the gating so keeper/runtime consumers can see it.
        let v = serde_json::to_value(&s).unwrap();
        let astra_json = v["model_availability"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model"] == "gpt-6-astra")
            .expect("astra entry serialized");
        assert_eq!(astra_json["availability"], "exhausted");
        assert_eq!(astra_json["credits_would_enable"], true);
        assert_eq!(astra_json["source"], "model_usage");
    }

    #[test]
    fn codex_spend_control_reached_blocks_capacity() {
        let top = CODEX_ASTRA_FIXTURE.replace(r#""spend_control": {"reached": false,"#, r#""spend_control": {"reached": true,"#);
        let s = parse_codex_usage(&top, "a", T0, None).unwrap();
        assert_eq!(s.spend_control_reached, Some(true));
        assert!(!s.has_proven_capacity(T0, 60_000));
        assert!(!s.has_proven_capacity_for_model("gpt-5.1-codex", T0, 60_000));
        // Nested `reached: true` alone also trips it (fail closed).
        let nested = CODEX_ASTRA_FIXTURE.replace(r#""workspace": {"reached": false,"#, r#""workspace": {"reached": true,"#);
        let s = parse_codex_usage(&nested, "a", T0, None).unwrap();
        assert_eq!(s.spend_control_reached, Some(true));
        assert!(!s.has_proven_capacity(T0, 60_000));
        // Absent spend_control → None (unknown, not asserted) and does not block by itself.
        let s = parse_codex_usage(&codex_fixture("null"), "a", T0, None).unwrap();
        assert_eq!(s.spend_control_reached, None);
        assert!(s.has_proven_capacity(T0, 60_000));
    }

    #[test]
    fn model_availability_merge_never_overwrites_explicit_entry() {
        // Explicit model_usage says available; a model-scoped window says exhausted → Exhausted (fail closed),
        // window percent fills in, explicit available_at is kept.
        let body = r#"{
            "rate_limit": {"limit_reached": false, "primary_window": {"used_percent": 1, "limit_window_seconds": 18000}},
            "model_usage": {"gpt-5-mini": {"available": true, "available_at": "2025-09-21T00:00:00Z"},
                            "gpt-6-astra": {"available": true}},
            "additional_rate_limits": [
                {"limit_name": "mini", "normal_model_slug": "gpt-5-mini",
                 "rate_limit": {"primary_window": {"used_percent": 100, "reset_at": 1758500000}}}
            ]
        }"#;
        let s = parse_codex_usage(body, "a", T0, None).unwrap();
        assert_eq!(s.model_availability.len(), 2, "merged, not duplicated");
        let mini = s.model_availability_for("gpt-5-mini").unwrap();
        assert_eq!(mini.availability, Availability::Exhausted);
        assert_eq!(mini.used_percent.valid(), Some(100.0));
        assert_eq!(mini.reset_at, epoch_ms_from_rfc3339("2025-09-21T00:00:00Z"), "explicit reset kept");
        assert_eq!(mini.source, "merged");
        let astra = s.model_availability_for("gpt-6-astra").unwrap();
        assert_eq!(astra.availability, Availability::Available);
        assert_eq!(astra.source, "model_usage");
        assert!(!s.has_proven_capacity_for_model("gpt-5-mini", T0, 60_000));
        assert!(s.has_proven_capacity_for_model("gpt-6-astra", T0, 60_000));
        // Family-token matching for Anthropic-style entries.
        let a = parse_anthropic_usage(ANTHROPIC_FIXTURE, "d", T0).unwrap();
        assert!(!a.has_proven_capacity_for_model("claude-sonnet-4-5", T0, 60_000), "sonnet window is at 100%");
        assert!(a.has_proven_capacity_for_model("claude-opus-4-1", T0, 60_000), "opus window was null → unmentioned");
        assert!(a.model_availability_for("sonnetx").is_none(), "token match, not substring");
        assert!(a.model_availability_for("claude-3-7-sonnet-latest").is_some());
    }

    #[test]
    fn codex_reset_credit_inventory_parses_details() {
        let body = r#"{"available_count": 1, "credits": [
            {"credit_id": "c1", "reset_type": "weekly", "granted_at": 1758300000, "expires_at": "2025-10-20T00:00:00Z"},
            {"reset_type": null, "expires_at": "garbage"}
        ]}"#;
        let b = parse_codex_reset_credits(body).unwrap();
        assert_eq!(b.available_count, Some(1));
        assert_eq!(b.credits.len(), 2);
        assert_eq!(b.credits[0].reset_type.as_deref(), Some("weekly"));
        assert_eq!(b.credits[0].granted_at, Some(1_758_300_000_000));
        assert_eq!(b.credits[0].expires_at, epoch_ms_from_rfc3339("2025-10-20T00:00:00Z"));
        assert_eq!(b.credits[1].expires_at, None);
        assert!(parse_codex_reset_credits("[]").is_err());
        let unknown = parse_codex_reset_credits("{}").unwrap();
        assert_eq!(unknown.available_count, None, "unknown is not zero");
    }

    // ── Kimi ────────────────────────────────────────────────────────────

    const KIMI_FIXTURE: &str = r#"{
        "usage": {"limit": "1000", "used": "250", "remaining": "750", "resetTime": "2025-09-24T00:00:00Z"},
        "limits": [
            {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
             "detail": {"limit": 100, "used": 100, "resetTime": "2025-09-20T22:00:00Z"}},
            {"name": "weekly", "window": {"duration": 1, "timeUnit": "TIME_UNIT_WEEK"},
             "detail": {"limit": "0", "used": "0"}},
            {"window": {"duration": 1, "timeUnit": "TIME_UNIT_FORTNIGHT"},
             "detail": {"limit": 10, "used": "many", "resetTime": 1758400000}}
        ],
        "boosterWallet": {
            "balance": {"type": "BOOSTER", "amount": 2000000000, "amountLeft": 500000000},
            "monthlyChargeLimit": {"priceInCents": 5000, "currency": "USD"}
        }
    }"#;

    #[test]
    fn kimi_valid_fixture() {
        let s = parse_kimi_usage(KIMI_FIXTURE, "m27", T0).unwrap();
        assert_eq!(s.provider, "kimi-code");
        let summary = s.window("usage").unwrap();
        assert_eq!(summary.duration_secs, Some(604800), "summary implies one week");
        assert_eq!(summary.used_percent.valid(), Some(25.0));
        assert_eq!(summary.used, Some(250.0));
        assert_eq!(summary.limit, Some(1000.0));
        assert_eq!(summary.reset_at, epoch_ms_from_rfc3339("2025-09-24T00:00:00Z"));
        let w0 = s.window("limits[0]").unwrap();
        assert_eq!(w0.duration_secs, Some(18000), "300 minutes = 5h");
        assert_eq!(w0.label, "5h");
        assert!(w0.is_exhausted());
        let w1 = s.window("limits[1]").unwrap();
        assert_eq!(w1.label, "weekly");
        assert_eq!(w1.used_percent, UsedPercent::unknown("limit is zero"));
        let w2 = s.window("limits[2]").unwrap();
        assert_eq!(w2.duration_secs, None, "unknown time unit → unknown duration");
        assert_eq!(w2.used_percent, UsedPercent::unknown("missing"), "non-numeric used");
        assert_eq!(w2.reset_at, Some(T0));
        // Exhausted 5h window + unknown windows → no proven capacity.
        assert!(!s.has_proven_capacity(T0, 60_000));
        let c = s.credits.as_ref().unwrap();
        assert_eq!(c.kind, "kimi_booster");
        assert_eq!(c.total, Some(20.0));
        assert_eq!(c.remaining, Some(5.0));
        assert_eq!(c.unit.as_deref(), Some("USD"));
        assert_eq!(c.has_credits, Some(true));
    }

    #[test]
    fn kimi_missing_and_malformed() {
        let s = parse_kimi_usage(r#"{"usage": {"remaining": "5"}, "limits": "nope"}"#, "x", T0).unwrap();
        assert!(s.windows.is_empty());
        assert!(s.notes.iter().any(|n| n == "summary usage row unparseable"));
        assert!(s.notes.iter().any(|n| n == "limits field is not an array"));
        assert!(s.notes.iter().any(|n| n == "no usage windows in response"));
        assert!(parse_kimi_usage("null", "x", T0).is_err());
        let ok = parse_kimi_usage(
            r#"{"limits": [{"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"}, "detail": {"limit": 100, "used": 10}}]}"#,
            "x",
            T0,
        )
        .unwrap();
        assert!(ok.has_proven_capacity(T0, 60_000));
        assert_eq!(ok.window("limits[0]").unwrap().reset_at, None);
    }

    // ── Grok ────────────────────────────────────────────────────────────

    #[test]
    fn grok_quotakit_shape_with_percent() {
        let body = r#"{"config": {"creditUsagePercent": 42.5, "billingPeriodMinutes": 43200,
                                   "currentPeriod": {"start": "2025-09-01T00:00:00Z", "end": "2025-10-01T00:00:00Z"}},
                       "onDemandUsed": {"val": 425}, "onDemandCap": {"val": 1000}}"#;
        let s = parse_grok_billing(body, "default", T0).unwrap();
        assert_eq!(s.provider, "xai-auth");
        let w = s.window("credits").unwrap();
        assert_eq!(w.used_percent.valid(), Some(42.5));
        assert_eq!(w.duration_secs, Some(43200 * 60));
        assert_eq!(w.label, "30d");
        assert_eq!(w.reset_at, epoch_ms_from_rfc3339("2025-10-01T00:00:00Z"));
        assert_eq!(w.reset_kind, ResetKind::BillingRenewal);
        assert_eq!(w.used, Some(425.0));
        assert_eq!(w.limit, Some(1000.0));
        let c = s.credits.as_ref().unwrap();
        assert_eq!(c.remaining, Some(575.0));
        assert_eq!(c.renews_at, w.reset_at);
        assert!(s.has_proven_capacity(T0, 60_000));
        assert!(s.notes.iter().any(|n| n.contains("unverified")));
    }

    #[test]
    fn grok_fallback_ratio_and_billing_period_end() {
        let body = r#"{"onDemandUsed": {"val": "250"}, "onDemandCap": {"val": 1000},
                       "config": {"billingPeriodEnd": 1759276800}}"#;
        let s = parse_grok_billing(body, "x", T0).unwrap();
        let w = s.window("credits").unwrap();
        assert_eq!(w.used_percent.valid(), Some(25.0));
        assert_eq!(w.reset_at, Some(1_759_276_800_000));
        assert_eq!(w.label, "billing period");
        assert!(s.notes.iter().any(|n| n == "percent derived from on-demand counters"));
    }

    #[test]
    fn grok_unknown_when_no_fraction_or_schema() {
        // Period known but no fraction → window exists, percent unknown.
        let s = parse_grok_billing(r#"{"config": {"billingPeriodMinutes": 1440}}"#, "x", T0).unwrap();
        let w = s.window("credits").unwrap();
        assert_eq!(w.used_percent, UsedPercent::unknown("missing"));
        assert!(!s.has_proven_capacity(T0, 60_000));
        // Out-of-range percent stays unknown.
        let s = parse_grok_billing(r#"{"config": {"creditUsagePercent": 140}}"#, "x", T0).unwrap();
        assert_eq!(s.window("credits").unwrap().used_percent, UsedPercent::unknown("out of range"));
        // Zero cap → limit is zero.
        let s = parse_grok_billing(r#"{"onDemandUsed": {"val": 0}, "onDemandCap": {"val": 0}}"#, "x", T0).unwrap();
        assert_eq!(s.window("credits").unwrap().used_percent, UsedPercent::unknown("limit is zero"));
        // Entirely unrecognized → no windows, fixed-vocabulary note, no key names.
        let s = parse_grok_billing(r#"{"sk_live_secretkey": 1, "mystery": {"x": 2}}"#, "x", T0).unwrap();
        assert!(s.windows.is_empty());
        assert!(s.notes.iter().any(|n| n == "unrecognized billing schema"));
        secret_free(&serde_json::to_string(&s).unwrap(), &["sk_live", "mystery"]);
        assert!(parse_grok_billing("", "x", T0).is_err());
    }

    // ── Snapshot helpers / serialization ────────────────────────────────

    #[test]
    fn staleness_and_json_round_trip() {
        let s = parse_codex_usage(&codex_fixture("null"), "a", T0, None).unwrap();
        assert!(!s.is_stale(T0 + 1000, 5_000));
        assert!(s.is_stale(T0 + 6_000, 5_000));
        assert!(s.is_stale(T0 - 1, 5_000), "future observation is stale");
        assert!(!s.has_proven_capacity(T0 + 10_000, 5_000), "stale is not capacity");
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: UsageSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], SCHEMA_VERSION);
        assert_eq!(v["windows"][0]["used_percent"]["state"], "valid");
        assert_eq!(v["windows"][0]["scope"]["kind"], "account");
        assert_eq!(v["windows"][0]["reset_kind"], "quota_window");
    }

    #[test]
    fn usage_entry_accepts_older_reports_without_identity() {
        let entry: AccountUsageEntry = serde_json::from_value(serde_json::json!({
            "provider": "anthropic", "account": "default", "status": "error",
            "error": {"kind": "credential", "message": "unavailable", "http_status": null}
        })).unwrap();
        assert_eq!(entry.identity, None);
        assert!(serde_json::to_value(entry).unwrap().get("identity").is_none());
    }

    #[test]
    fn report_round_trip_with_partial_errors() {
        let snap = parse_anthropic_usage(ANTHROPIC_FIXTURE, "default", T0).unwrap();
        let mut report = UsageReport::new("local", T0);
        report.accounts.push(AccountUsageEntry {
            provider: "anthropic".into(),
            account: "default".into(),
            identity: None,
            outcome: AccountUsageOutcome::ok(snap),
        });
        report.accounts.push(AccountUsageEntry {
            provider: "openai-codex".into(),
            account: "astra2".into(),
            identity: None,
            outcome: AccountUsageOutcome::error(&UsageError::Unauthorized { status: 401 }),
        });
        assert_eq!(report.ok_count(), 1);
        assert_eq!(report.error_count(), 1);
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["accounts"][0]["status"], "ok");
        assert_eq!(v["accounts"][1]["status"], "error");
        assert_eq!(v["accounts"][1]["error"]["kind"], "unauthorized");
        assert_eq!(v["accounts"][1]["error"]["http_status"], 401);
        let back: UsageReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn error_display_and_broker_mapping_are_secret_free() {
        let e = UsageError::Transport { detail: "connect".into() };
        assert_eq!(e.kind(), "transport");
        assert!(matches!(e.clone().into_broker_error(), BrokerError::Transport(_)));
        assert!(matches!(
            UsageError::Unauthorized { status: 401 }.into_broker_error(),
            BrokerError::Credential(_)
        ));
        assert!(matches!(
            UsageError::UnsupportedProvider { provider: "github-copilot".into() }.into_broker_error(),
            BrokerError::UnsupportedCapability { capability, .. } if capability == "usage"
        ));
        assert!(matches!(
            UsageError::InvalidEndpointOverride.into_broker_error(),
            BrokerError::Denied(_)
        ));
        let v = serde_json::to_value(UsageError::UpstreamStatus { status: 502 }).unwrap();
        assert_eq!(v, serde_json::json!({"kind":"upstream_status","status":502}));
        assert!(!supports_usage(OAuthProviderId::GitHubCopilot));
        assert!(!supports_usage(OAuthProviderId::GoogleGemini));
        assert_eq!(supported_usage_providers().len(), 4);
    }

    #[test]
    fn endpoint_override_is_loopback_only() {
        assert!(is_loopback_url("http://127.0.0.1:8080/usage"));
        assert!(is_loopback_url("http://localhost:1/x"));
        assert!(is_loopback_url("http://[::1]:9/x"));
        assert!(!is_loopback_url("https://api.anthropic.com/api/oauth/usage"));
        assert!(!is_loopback_url("http://127.0.0.1.evil.example/x"));
        assert!(!is_loopback_url("ftp://127.0.0.1/x"));
        assert!(!is_loopback_url("not a url"));
        assert_eq!(
            resolve_endpoint(CODEX_USAGE_URL, Some("https://attacker.example/usage")).unwrap_err(),
            UsageError::InvalidEndpointOverride
        );
        assert_eq!(resolve_endpoint(CODEX_USAGE_URL, None).unwrap(), CODEX_USAGE_URL);
    }

    // ── Fake-server transport tests ─────────────────────────────────────

    fn codex_token(account_id: Option<&str>) -> String {
        let payload = match account_id {
            Some(id) => serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": id}}),
            None => serde_json::json!({"sub": "nobody"}),
        };
        format!(
            "hdr.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    async fn spawn(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn opts(url: &str) -> UsageFetchOptions {
        UsageFetchOptions {
            endpoint_override: Some(url.to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn fetch_unsupported_provider_makes_no_request() {
        let client = UsageClient::new().unwrap();
        let err = fetch_usage(
            &client,
            OAuthProviderId::GitHubCopilot,
            "default",
            "tok",
            &opts("http://127.0.0.1:9/never"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, UsageError::UnsupportedProvider { .. }));
    }

    #[tokio::test]
    async fn fetch_rejects_non_loopback_override_before_sending() {
        let client = UsageClient::new().unwrap();
        let err = fetch_usage(
            &client,
            OAuthProviderId::Anthropic,
            "default",
            "tok",
            &opts("https://evil.example/usage"),
        )
        .await
        .unwrap_err();
        assert_eq!(err, UsageError::InvalidEndpointOverride);
    }

    #[tokio::test]
    async fn fetch_anthropic_sends_bearer_and_beta_header() {
        use axum::{extract::Request, routing::get};
        let app = axum::Router::new().route(
            "/usage",
            get(|req: Request| async move {
                let h = req.headers();
                assert_eq!(h.get("authorization").unwrap(), "Bearer anthropic-access-token");
                assert_eq!(h.get("anthropic-beta").unwrap(), ANTHROPIC_OAUTH_BETA);
                assert_eq!(h.get("accept").unwrap(), "application/json");
                assert!(h.get("user-agent").unwrap().to_str().unwrap().starts_with("SynapsCLI/"));
                ([("content-type", "application/json")], ANTHROPIC_FIXTURE)
            }),
        );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let s = fetch_usage(
            &client,
            OAuthProviderId::Anthropic,
            "default",
            "anthropic-access-token",
            &opts(&format!("{url}/usage")),
        )
        .await
        .unwrap();
        assert_eq!(s.windows.len(), 4);
        assert!(s.observed_at >= T0);
        secret_free(&serde_json::to_string(&s).unwrap(), &["anthropic-access-token"]);
    }

    #[tokio::test]
    async fn fetch_codex_pairs_account_header_with_token_and_reads_inventory() {
        use axum::{extract::Request, routing::get};
        let app = axum::Router::new()
            .route(
                "/wham/usage",
                get(|req: Request| async move {
                    let h = req.headers();
                    assert_eq!(h.get("authorization").unwrap().to_str().unwrap(), format!("Bearer {}", codex_token(Some("acct_1234567890"))));
                    assert_eq!(h.get(CODEX_ACCOUNT_HEADER).unwrap(), "acct_1234567890");
                    codex_fixture("null")
                }),
            )
            .route(
                "/wham/rate-limit-reset-credits",
                get(|req: Request| async move {
                    assert_eq!(req.headers().get(CODEX_ACCOUNT_HEADER).unwrap(), "acct_1234567890");
                    r#"{"available_count": 3, "credits": [{"reset_type": "weekly", "expires_at": 1760000000}]}"#
                }),
            );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let o = UsageFetchOptions {
            endpoint_override: Some(format!("{url}/wham/usage")),
            inventory_endpoint_override: Some(format!("{url}/wham/rate-limit-reset-credits")),
            include_inventory: true,
            ..Default::default()
        };
        let s = fetch_usage(&client, OAuthProviderId::OpenAiCodex, "astra2", &codex_token(Some("acct_1234567890")), &o)
            .await
            .unwrap();
        let b = s.banked_resets.as_ref().unwrap();
        assert_eq!(b.available_count, Some(3), "inventory detail overrides summary count");
        assert_eq!(b.credits.len(), 1);
        assert_eq!(b.credits[0].expires_at, Some(1_760_000_000_000));
        assert_eq!(b.inventory_error, None);
    }

    #[tokio::test]
    async fn fetch_codex_inventory_failure_is_partial_not_total() {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/wham/usage", get(|| async { codex_fixture("null") }))
            .route(
                "/wham/rate-limit-reset-credits",
                get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom-secret-body") }),
            );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let o = UsageFetchOptions {
            endpoint_override: Some(format!("{url}/wham/usage")),
            inventory_endpoint_override: Some(format!("{url}/wham/rate-limit-reset-credits")),
            include_inventory: true,
            ..Default::default()
        };
        let s = fetch_usage(&client, OAuthProviderId::OpenAiCodex, "a", &codex_token(Some("acct_1234567890")), &o)
            .await
            .unwrap();
        assert_eq!(s.window("primary").unwrap().used_percent.valid(), Some(37.0));
        let b = s.banked_resets.as_ref().unwrap();
        assert_eq!(b.available_count, Some(2), "summary count survives");
        assert_eq!(b.inventory_error.as_deref(), Some("upstream_status"));
        secret_free(&serde_json::to_string(&s).unwrap(), &["boom-secret-body"]);
    }

    #[tokio::test]
    async fn fetch_codex_without_account_claim_makes_no_request() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/wham/usage",
            get(|| async {
                if true {
                    panic!("request must not be sent without a paired account id");
                }
                "{}"
            }),
        );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let err = fetch_usage(&client, OAuthProviderId::OpenAiCodex, "a", &codex_token(None), &opts(&format!("{url}/wham/usage")))
            .await
            .unwrap_err();
        assert_eq!(err, UsageError::AccountIdUnavailable);
        assert!(matches!(err.into_broker_error(), BrokerError::Credential(_)));
    }

    #[tokio::test]
    async fn fetch_codex_identity_mismatch_from_server() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/wham/usage",
            get(|| async { codex_fixture("null").replace("acct_1234567890", "acct_someoneelse") }),
        );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let err = fetch_usage(&client, OAuthProviderId::OpenAiCodex, "a", &codex_token(Some("acct_1234567890")), &opts(&format!("{url}/wham/usage")))
            .await
            .unwrap_err();
        assert_eq!(err, UsageError::IdentityMismatch);
    }

    #[tokio::test]
    async fn fetch_kimi_sends_only_bearer_and_accept() {
        use axum::{extract::Request, routing::get};
        let app = axum::Router::new().route(
            "/coding/v1/usages",
            get(|req: Request| async move {
                let h = req.headers();
                assert_eq!(h.get("authorization").unwrap(), "Bearer kimi-token");
                assert!(h.get("x-msh-device-id").is_none(), "no device identity on usage reads");
                KIMI_FIXTURE
            }),
        );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let s = fetch_usage(&client, OAuthProviderId::KimiCode, "m27", "kimi-token", &opts(&format!("{url}/coding/v1/usages")))
            .await
            .unwrap();
        assert_eq!(s.window("usage").unwrap().used_percent.valid(), Some(25.0));
    }

    #[tokio::test]
    async fn fetch_grok_sends_token_auth_header() {
        use axum::{extract::Request, routing::get};
        let app = axum::Router::new().route(
            "/v1/billing",
            get(|req: Request| async move {
                let h = req.headers();
                assert_eq!(h.get("authorization").unwrap(), "Bearer grok-token");
                assert_eq!(h.get(GROK_TOKEN_AUTH_HEADER).unwrap(), GROK_TOKEN_AUTH_VALUE);
                assert_eq!(req.uri().query(), Some("format=credits"));
                r#"{"config": {"creditUsagePercent": 10}}"#
            }),
        );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let s = fetch_usage(&client, OAuthProviderId::Xai, "default", "grok-token", &opts(&format!("{url}/v1/billing?format=credits")))
            .await
            .unwrap();
        assert_eq!(s.window("credits").unwrap().used_percent.valid(), Some(10.0));
    }

    #[tokio::test]
    async fn fetch_status_classification_drops_upstream_bodies() {
        use axum::routing::get;
        let hostile = "{\"error\":\"leak-me-ZZZ\",\"token\":\"sk-ant-secret\"}";
        let app = axum::Router::new()
            .route("/401", get(move || async move { (axum::http::StatusCode::UNAUTHORIZED, hostile) }))
            .route("/403", get(move || async move { (axum::http::StatusCode::FORBIDDEN, hostile) }))
            .route("/429", get(move || async move { (axum::http::StatusCode::TOO_MANY_REQUESTS, hostile) }))
            .route("/500", get(move || async move { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, hostile) }))
            .route(
                "/302",
                get(move || async move {
                    (
                        axum::http::StatusCode::FOUND,
                        [("location", "http://127.0.0.1:9/elsewhere")],
                        hostile,
                    )
                }),
            );
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();
        let cases: [(&str, UsageError); 5] = [
            ("401", UsageError::Unauthorized { status: 401 }),
            ("403", UsageError::Unauthorized { status: 403 }),
            ("429", UsageError::RateLimited),
            ("500", UsageError::UpstreamStatus { status: 500 }),
            ("302", UsageError::Redirected { status: 302 }),
        ];
        for (path, expected) in cases {
            let err = fetch_usage(&client, OAuthProviderId::Anthropic, "d", "tok-ZZ-secret", &opts(&format!("{url}/{path}")))
                .await
                .unwrap_err();
            assert_eq!(err, expected, "path {path}");
            secret_free(&err.to_string(), &["leak-me", "sk-ant", "tok-ZZ"]);
            secret_free(&serde_json::to_string(&err).unwrap(), &["leak-me", "sk-ant"]);
        }
    }

    #[tokio::test]
    async fn fetch_body_cap_and_timeout_and_malformed() {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/big", get(|| async { format!("{{\"five_hour\":{{\"utilization\":1,\"pad\":\"{}\"}}}}", "x".repeat(4096)) }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "{}"
                }),
            )
            .route("/html", get(|| async { "<html>not json</html>" }));
        let url = spawn(app).await;
        let client = UsageClient::new().unwrap();

        let o = UsageFetchOptions {
            endpoint_override: Some(format!("{url}/big")),
            max_body_bytes: 1024,
            ..Default::default()
        };
        let err = fetch_usage(&client, OAuthProviderId::Anthropic, "d", "tok", &o).await.unwrap_err();
        assert_eq!(err, UsageError::BodyTooLarge { cap: 1024 });

        let o = UsageFetchOptions {
            endpoint_override: Some(format!("{url}/slow")),
            timeout: Duration::from_millis(300),
            ..Default::default()
        };
        let err = fetch_usage(&client, OAuthProviderId::Anthropic, "d", "tok", &o).await.unwrap_err();
        assert_eq!(err, UsageError::Timeout);

        let err = fetch_usage(&client, OAuthProviderId::Anthropic, "d", "tok", &opts(&format!("{url}/html")))
            .await
            .unwrap_err();
        assert_eq!(err, UsageError::Malformed { detail: "body is not JSON".into() });
    }
}
