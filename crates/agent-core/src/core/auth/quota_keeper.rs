//! G6 — quota keeper: pure scheduler state machine, private persisted state
//! and singleton/attempt locking.
//!
//! The keeper watches each kept account's **weekly** window (identified by its
//! provider-reported duration — never by the name `primary`) and tracks the
//! provider-reported reset instant as the authoritative *reset generation*.
//! When a generation's reset has passed and fresh usage still shows no newer
//! window, the account is *due for activation*. In read-only mode (the
//! default) the keeper only records and alerts. Only for accounts the
//! operator explicitly opted in, the runner performs **one** bounded,
//! tool-free request per generation; the keeper never declares the window
//! active until a *fresh* usage read shows a strictly later reset.
//!
//! ## Invariants
//!
//! * No I/O, no clock, no network in the state machine: every function takes
//!   `now_ms`. Persistence and locking are separate helpers.
//! * State is keyed by broker source + `provider@label` + provider identity
//!   fingerprint, so a re-login of the same alias with a different seat never
//!   inherits an activation ticket.
//! * An attempt is recorded and must be **persisted before** the request is
//!   sent ([`begin_attempt`]); ambiguous outcomes (timeout, crash between
//!   begin and finish, 5xx) consume the per-generation budget of exactly
//!   one. Only a definitive *not sent* (typed pre-flight stage) or a
//!   whitelisted pre-inference 4xx is refunded, and those are bounded
//!   separately with backoff. A second attempt for the same generation
//!   requires an explicit operator [`rearm`].
//! * A newly observed window is labelled **verified**; correlation with a
//!   keeper attempt is recorded, but never claimed as causal.
//! * Natural rollover is never assumed: with no evidence the phase is
//!   `Unknown`/`AwaitingActivation`, never `Active`.
//! * **Proven capacity before any inference.** An activation attempt is
//!   authorized only when the *latest* poll succeeded, is fresh, carries no
//!   overall `limit_reached` assertion, and every window applicable to the
//!   activation model (5h, weekly, model-scoped) shows headroom. A passed
//!   reset while the provider still asserts 100 % / `limit_reached` is
//!   `Exhausted` (reset passed), never due.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::account::CredentialRef;
use super::quota_policy::{
    self, weekly_window, AccountCapacity, ModelAvailability, QuotaObservation, SelectionRequest,
    Strategy, WindowLimit, WindowVerdict, DEFAULT_EXHAUSTED_AT_PERCENT,
    MAX_FUTURE_OBSERVATION_SKEW_MS,
};

pub const STATE_VERSION: u32 = 1;
/// File name of the persisted state inside the state dir.
pub const STATE_FILE_NAME: &str = "state.json";
/// File name of the singleton lock inside the state dir.
pub const SINGLETON_LOCK_NAME: &str = "keeper.lock";
/// Sub-directory (of the canonical keeper dir) holding per-account locks.
pub const ACCOUNT_LOCK_DIR: &str = "locks";

const SEC: u64 = 1000;
const MIN: u64 = 60 * SEC;
const HOUR: u64 = 60 * MIN;
const DAY: u64 = 24 * HOUR;

// Bounds for operator-supplied intervals (untrusted input; always clamped).
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 5 * MIN;
pub const MIN_POLL_INTERVAL_MS: u64 = MIN;
pub const MAX_POLL_INTERVAL_MS: u64 = HOUR;
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 4 * HOUR;
pub const MAX_MAX_BACKOFF_MS: u64 = 12 * HOUR;
pub const DEFAULT_STALE_AFTER_MS: u64 = 30 * MIN;
pub const MIN_STALE_AFTER_MS: u64 = MIN;
pub const MAX_STALE_AFTER_MS: u64 = DAY;
/// Poll shortly after the reported reset instead of waiting a full interval.
pub const DEFAULT_RESET_GRACE_MS: u64 = 30 * SEC;
pub const MAX_RESET_GRACE_MS: u64 = 10 * MIN;
/// Re-poll cadence while an attempt awaits verification.
pub const DEFAULT_VERIFY_POLL_MS: u64 = 20 * SEC;
/// How long an attempt may stay pending before it is labelled unverified.
pub const DEFAULT_VERIFY_WINDOW_MS: u64 = 10 * MIN;
pub const MAX_VERIFY_WINDOW_MS: u64 = 2 * HOUR;
/// Attempts per reset generation. Fixed at one: a second attempt for the
/// same generation only ever happens through an explicit operator
/// [`rearm`], never automatically.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 1;
pub const MAX_MAX_ATTEMPTS: u32 = 1;
/// Definitive not-sent/rejected outcomes tolerated per generation.
pub const MAX_NOT_SENT_PER_GENERATION: u32 = 5;
pub const DEFAULT_ATTEMPT_TIMEOUT_MS: u64 = 60 * SEC;
pub const MIN_ATTEMPT_TIMEOUT_MS: u64 = 10 * SEC;
pub const MAX_ATTEMPT_TIMEOUT_MS: u64 = 5 * MIN;
pub const DEFAULT_USAGE_TIMEOUT_MS: u64 = 20 * SEC;
pub const DEFAULT_BANKED_EXPIRY_ALERT_MS: u64 = 3 * DAY;
/// Attempt history retained per account.
pub const MAX_HISTORY: usize = 24;

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeeperConfig {
    pub poll_interval_ms: u64,
    pub max_backoff_ms: u64,
    pub stale_after_ms: u64,
    pub reset_grace_ms: u64,
    pub verify_poll_ms: u64,
    pub verify_window_ms: u64,
    pub max_attempts_per_generation: u32,
    pub attempt_timeout_ms: u64,
    pub usage_timeout_ms: u64,
    pub banked_expiry_alert_ms: u64,
    pub exhausted_at_percent: f64,
}

impl Default for KeeperConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            max_backoff_ms: DEFAULT_MAX_BACKOFF_MS,
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
            reset_grace_ms: DEFAULT_RESET_GRACE_MS,
            verify_poll_ms: DEFAULT_VERIFY_POLL_MS,
            verify_window_ms: DEFAULT_VERIFY_WINDOW_MS,
            max_attempts_per_generation: DEFAULT_MAX_ATTEMPTS,
            attempt_timeout_ms: DEFAULT_ATTEMPT_TIMEOUT_MS,
            usage_timeout_ms: DEFAULT_USAGE_TIMEOUT_MS,
            banked_expiry_alert_ms: DEFAULT_BANKED_EXPIRY_ALERT_MS,
            exhausted_at_percent: DEFAULT_EXHAUSTED_AT_PERCENT,
        }
    }
}

impl KeeperConfig {
    /// Clamp every operator-controlled value into its safe range.
    pub fn bounded(mut self) -> Self {
        self.poll_interval_ms = self
            .poll_interval_ms
            .clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
        self.max_backoff_ms = self
            .max_backoff_ms
            .clamp(self.poll_interval_ms, MAX_MAX_BACKOFF_MS);
        self.stale_after_ms = self
            .stale_after_ms
            .clamp(MIN_STALE_AFTER_MS, MAX_STALE_AFTER_MS);
        self.reset_grace_ms = self.reset_grace_ms.min(MAX_RESET_GRACE_MS);
        self.verify_poll_ms = self.verify_poll_ms.clamp(5 * SEC, self.poll_interval_ms);
        self.verify_window_ms = self
            .verify_window_ms
            .clamp(self.verify_poll_ms, MAX_VERIFY_WINDOW_MS);
        self.max_attempts_per_generation = self
            .max_attempts_per_generation
            .clamp(1, MAX_MAX_ATTEMPTS);
        self.attempt_timeout_ms = self
            .attempt_timeout_ms
            .clamp(MIN_ATTEMPT_TIMEOUT_MS, MAX_ATTEMPT_TIMEOUT_MS);
        self.usage_timeout_ms = self.usage_timeout_ms.clamp(5 * SEC, 2 * MIN);
        if !self.exhausted_at_percent.is_finite() || !(1.0..=100.0).contains(&self.exhausted_at_percent) {
            self.exhausted_at_percent = DEFAULT_EXHAUSTED_AT_PERCENT;
        }
        self
    }
}

// ── Identity ─────────────────────────────────────────────────────────────────

/// Namespace of one kept account. Every field is non-secret display text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// Broker source: `local:<auth.json path>` or `remote:<endpoint>`.
    pub source: String,
    /// Storage key: `provider` or `provider@label`.
    pub credential: String,
    /// Provider-side identity fingerprint (see [`AccountIdentity::fingerprint`]).
    pub identity_fp: String,
}

impl AccountIdentity {
    /// Map key. Changing any component yields a fresh state entry.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.source, self.credential, self.identity_fp)
    }

    /// Fingerprint from non-secret provider metadata. Prefers the provider
    /// account-id prefix, then a hash of the identity string, then the
    /// login time. Providers exposing none of these yield `unknown`, which
    /// is reported so the operator knows re-login inheritance is possible.
    pub fn fingerprint(
        account_id_prefix: Option<&str>,
        identity: Option<&str>,
        added_at: Option<u64>,
    ) -> String {
        if let Some(p) = account_id_prefix.map(str::trim).filter(|p| !p.is_empty()) {
            return format!("id:{p}");
        }
        if let Some(i) = identity.map(str::trim).filter(|i| !i.is_empty()) {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(i.as_bytes());
            let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
            return format!("who:{hex}");
        }
        if let Some(t) = added_at {
            return format!("added:{t}");
        }
        "unknown".to_string()
    }
}

// ── Observations (input) ─────────────────────────────────────────────────────

/// Banked reset inventory (Codex `rate_limit_reset_credits`). Alert-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankedResets {
    /// `None` = the provider did not report a count (unknown ≠ zero).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_count: Option<u64>,
    /// Earliest expiry among the inventory, if exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_expiry_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory_error: Option<String>,
}

/// What one usage poll returned. Produced by the runner's usage adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageObservation {
    /// When the body was received (epoch ms).
    pub observed_at_ms: u64,
    pub outcome: ObservationOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationOutcome {
    Ok {
        windows: Vec<WindowLimit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        models: Option<Vec<ModelAvailability>>,
        /// Provider-asserted overall flag (Codex `rate_limit.limit_reached`).
        /// `Some(true)` is an assertion of no capacity regardless of windows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit_reached: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        banked: Option<BankedResets>,
        /// Provider account-id prefix carried by the response, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_prefix: Option<String>,
    },
    AuthError {
        detail: String,
    },
    /// Retryable (network, timeout, 5xx, 429 on the usage endpoint).
    Transport {
        detail: String,
    },
    Malformed {
        detail: String,
    },
    Unsupported {
        detail: String,
    },
}

/// Fold a provider-asserted overall `limit_reached: true` into the
/// account-wide windows so every policy consumer honours it. With no
/// account-wide window present a synthetic `overall` window is added.
pub fn windows_with_overall_flag(
    windows: &[WindowLimit],
    limit_reached: Option<bool>,
) -> Vec<WindowLimit> {
    let mut out: Vec<WindowLimit> = windows.to_vec();
    if limit_reached == Some(true) {
        let mut any_account_wide = false;
        for w in out.iter_mut().filter(|w| w.models.is_none()) {
            any_account_wide = true;
            w.limit_reached = Some(true);
        }
        if !any_account_wide {
            out.push(WindowLimit {
                id: "overall".into(),
                duration_ms: None,
                used_percent: None,
                limit_reached: Some(true),
                resets_at_ms: None,
                models: None,
            });
        }
    }
    out
}

impl UsageObservation {
    /// The policy view of this observation (for `quota_policy::select`).
    pub fn to_quota_observation(&self) -> QuotaObservation {
        match &self.outcome {
            ObservationOutcome::Ok {
                windows,
                models,
                limit_reached,
                ..
            } => QuotaObservation::Ok {
                windows: windows_with_overall_flag(windows, *limit_reached),
                models: models.clone(),
            },
            ObservationOutcome::AuthError { .. } => QuotaObservation::AuthError,
            ObservationOutcome::Transport { .. } => QuotaObservation::Unknown,
            ObservationOutcome::Malformed { .. } => QuotaObservation::Malformed,
            ObservationOutcome::Unsupported { .. } => QuotaObservation::Unsupported,
        }
    }
}

// ── State ────────────────────────────────────────────────────────────────────

/// How a verified window relates to keeper activity. Correlation is recorded;
/// causation is never claimed (an external request or a provider-side reset
/// could equally have anchored the window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attribution {
    /// A window was observed without any keeper attempt for the prior generation.
    Observed,
    /// A keeper attempt for the prior generation preceded the new window.
    /// Verified window; **not proven causal**.
    AttemptCorrelated { attempt: u32 },
}

/// Where a request definitively stopped before leaving the process or
/// before reaching the provider. Only these stages may be reported as
/// `NotSent`; anything after the request was written to the socket is
/// `Ambiguous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotSentStage {
    /// The provider/account is not activatable by this runner.
    Unsupported,
    /// Broker refused to vend a token for the tracked credential.
    TokenVend,
    /// The token carries no provider account id to pair with the bearer.
    AccountId,
    /// The request body could not be built.
    RequestBuild,
    /// TCP/TLS connect failed; no bytes were sent.
    Connect,
}

/// HTTP statuses that prove the provider rejected the request before any
/// inference (malformed request, auth, missing route, payload shape).
/// Everything else — notably 429, 402, 408, 409 — is not refunded.
pub const REFUNDABLE_REJECT_STATUSES: &[u16] = &[400, 401, 403, 404, 413, 415, 422];

/// Terminal classification of one activation attempt, as reported by the
/// runner. Only a typed `NotSent` and a whitelisted `Rejected` status are
/// definitive "no inference" outcomes; everything else may have spent quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Provider accepted the request (2xx) and the bounded body was consumed.
    Sent { http_status: u16 },
    /// Provider returned a 4xx. Refunded only for [`REFUNDABLE_REJECT_STATUSES`].
    Rejected { http_status: u16 },
    /// Nothing reached the provider; `stage` names the pre-flight step.
    NotSent { stage: NotSentStage, reason: String },
    /// Timeout, 5xx, stream cut, or crash between begin and finish.
    Ambiguous { reason: String },
}

impl AttemptOutcome {
    /// Whether the attempt is refunded (definitively no inference happened).
    pub fn is_refundable(&self) -> bool {
        match self {
            Self::NotSent { .. } => true,
            Self::Rejected { http_status } => REFUNDABLE_REJECT_STATUSES.contains(http_status),
            Self::Sent { .. } | Self::Ambiguous { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// No usable weekly-window evidence.
    Unknown { reason: String },
    /// Credential rejected by the provider; needs re-login.
    AuthError { detail: String },
    /// No headroom. `reset_passed` = the provider still asserts the limit
    /// although the reported reset instant has passed — never due.
    Exhausted {
        generation: u64,
        #[serde(default)]
        reset_passed: bool,
    },
    /// Live window with headroom. `verified_at_ms` = observation time.
    Active {
        generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_percent: Option<f64>,
        verified_at_ms: u64,
        attribution: Attribution,
    },
    /// Reset passed; fresh usage still shows no newer window.
    AwaitingActivation { generation: u64, due_since_ms: u64 },
    /// An attempt is recorded (persisted before send) and not yet verified.
    ActivationPending {
        generation: u64,
        attempt: u32,
        started_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<AttemptOutcome>,
    },
    /// Attempt budget spent for this generation without verification.
    Unverified { generation: u64, attempts: u32 },
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown { .. } => "unknown",
            Self::AuthError { .. } => "auth_error",
            Self::Exhausted { .. } => "exhausted",
            Self::Active { .. } => "active",
            Self::AwaitingActivation { .. } => "awaiting_activation",
            Self::ActivationPending { .. } => "activation_pending",
            Self::Unverified { .. } => "unverified",
        }
    }

    /// Generation carried by the phase, if any.
    pub fn generation(&self) -> Option<u64> {
        match self {
            Self::Unknown { .. } | Self::AuthError { .. } => None,
            Self::Exhausted { generation, .. }
            | Self::Active { generation, .. }
            | Self::AwaitingActivation { generation, .. }
            | Self::ActivationPending { generation, .. }
            | Self::Unverified { generation, .. } => Some(*generation),
        }
    }
}

/// One activation attempt, for the operator record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt: u32,
    pub generation: u64,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AttemptOutcome>,
    /// Set when a later window was verified after this attempt (correlation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    pub identity: AccountIdentity,
    pub phase: Phase,
    /// Latest provider-reported weekly reset (epoch ms) we track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Attempts that may have spent quota in `generation`.
    #[serde(default)]
    pub attempts_in_generation: u32,
    /// Refunded (definitively not-sent/rejected) attempts in `generation`.
    #[serde(default)]
    pub not_sent_in_generation: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_observation_ms: Option<u64>,
    /// Whether the most recent poll (of any kind) succeeded.
    #[serde(default)]
    pub last_poll_ok: bool,
    /// Windows/models/flag from the last successful poll (capacity evidence).
    #[serde(default)]
    pub last_windows: Vec<WindowLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_models: Option<Vec<ModelAvailability>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_limit_reached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_errors: u32,
    #[serde(default)]
    pub backoff_ms: u64,
    /// Earliest time the runner should act on this account again.
    #[serde(default)]
    pub next_action_at_ms: u64,
    /// Not before this may another activation attempt start (after refunds).
    #[serde(default)]
    pub next_attempt_allowed_ms: u64,
    /// Measured on the last verified new window: anchor − previous reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_idle_delay_ms: Option<u64>,
    /// `true` when the delay is an upper bound (anchor inferred from the
    /// observation, not from a reported duration).
    #[serde(default)]
    pub last_idle_delay_is_upper_bound: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<WindowLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banked: Option<BankedResets>,
    #[serde(default)]
    pub history: Vec<AttemptRecord>,
}

impl AccountState {
    pub fn new(identity: AccountIdentity) -> Self {
        Self {
            identity,
            phase: Phase::Unknown {
                reason: "never polled".into(),
            },
            generation: None,
            attempts_in_generation: 0,
            not_sent_in_generation: 0,
            last_poll_ms: None,
            last_ok_observation_ms: None,
            last_poll_ok: false,
            last_windows: Vec::new(),
            last_models: None,
            last_limit_reached: None,
            last_error: None,
            consecutive_errors: 0,
            backoff_ms: 0,
            next_action_at_ms: 0,
            next_attempt_allowed_ms: 0,
            last_idle_delay_ms: None,
            last_idle_delay_is_upper_bound: false,
            weekly: None,
            banked: None,
            history: Vec::new(),
        }
    }

    fn push_history(&mut self, rec: AttemptRecord) {
        self.history.push(rec);
        if self.history.len() > MAX_HISTORY {
            let drop = self.history.len() - MAX_HISTORY;
            self.history.drain(..drop);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeeperState {
    pub version: u32,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountState>,
}

impl Default for KeeperState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            updated_at_ms: 0,
            accounts: BTreeMap::new(),
        }
    }
}

impl KeeperState {
    /// Fetch or create the entry for `identity`.
    pub fn entry(&mut self, identity: &AccountIdentity) -> &mut AccountState {
        self.accounts
            .entry(identity.key())
            .or_insert_with(|| AccountState::new(identity.clone()))
    }

    /// Prune entries that are NOT in `keep`, carry no attempt history, are
    /// not mid-attempt, and have been idle longer than `max_idle_ms`.
    ///
    /// Attempt ledgers are never deleted: an account temporarily excluded by
    /// `--account`/`--provider` (or re-logged-in later with the same seat)
    /// must find its spent generation intact, otherwise a filter switch could
    /// burn a second activation. Returns the pruned keys.
    pub fn prune_idle(&mut self, keep: &[AccountIdentity], now_ms: u64, max_idle_ms: u64) -> Vec<String> {
        let keep: Vec<String> = keep.iter().map(AccountIdentity::key).collect();
        let pruned: Vec<String> = self
            .accounts
            .iter()
            .filter(|(k, a)| {
                !keep.contains(k)
                    && a.history.is_empty()
                    && !matches!(
                        a.phase,
                        Phase::ActivationPending { .. } | Phase::Unverified { .. }
                    )
                    && now_ms.saturating_sub(a.last_poll_ms.unwrap_or(0)) > max_idle_ms
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &pruned {
            self.accounts.remove(k);
        }
        pruned
    }

    /// Accounts whose next action time has arrived.
    pub fn due(&self, now_ms: u64) -> Vec<String> {
        self.accounts
            .iter()
            .filter(|(_, a)| a.next_action_at_ms <= now_ms)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// [`due`](Self::due) restricted to `keys` (the accounts a runner keeps).
    pub fn due_among(&self, now_ms: u64, keys: &[String]) -> Vec<String> {
        self.due(now_ms)
            .into_iter()
            .filter(|k| keys.contains(k))
            .collect()
    }

    /// Earliest next action across all accounts.
    pub fn next_wake_ms(&self) -> Option<u64> {
        self.accounts.values().map(|a| a.next_action_at_ms).min()
    }

    /// Earliest next action among `keys`.
    pub fn next_wake_among(&self, keys: &[String]) -> Option<u64> {
        self.accounts
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, a)| a.next_action_at_ms)
            .min()
    }
}

// ── Events ───────────────────────────────────────────────────────────────────

/// Operator-visible outcome of a state-machine step. Secret-free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum KeeperEvent {
    PhaseChanged {
        key: String,
        from: String,
        to: String,
    },
    PollFailed {
        key: String,
        detail: String,
        backoff_ms: u64,
    },
    /// A new weekly window was verified from fresh usage.
    NewWindow {
        key: String,
        from_generation: Option<u64>,
        to_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle_delay_ms: Option<u64>,
        idle_delay_is_upper_bound: bool,
        attribution: Attribution,
    },
    /// The reset passed but the provider still asserts no capacity.
    LimitAssertedAfterReset {
        key: String,
        generation: u64,
    },
    /// The reset passed and no newer window is reported.
    ActivationDue {
        key: String,
        generation: u64,
        due_since_ms: u64,
    },
    AttemptStarted {
        key: String,
        generation: u64,
        attempt: u32,
    },
    AttemptFinished {
        key: String,
        generation: u64,
        attempt: u32,
        outcome: AttemptOutcome,
        refunded: bool,
    },
    Unverified {
        key: String,
        generation: u64,
        attempts: u32,
    },
    /// Operator explicitly re-armed one more attempt for a generation.
    Rearmed {
        key: String,
        generation: u64,
        previous_attempts: u32,
    },
    AuthError {
        key: String,
        detail: String,
    },
    BankedResets {
        key: String,
        available_count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        earliest_expiry_ms: Option<u64>,
        expiring_soon: bool,
    },
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum KeeperError {
    UnknownAccount(String),
    /// `begin_attempt` called while the account is not awaiting activation.
    NotDue { key: String, phase: String },
    /// Persisted state could not be read/parsed. Fail closed — never activate.
    CorruptState { path: PathBuf, detail: String },
    /// Persisted state has a newer schema than this binary understands.
    UnsupportedVersion { path: PathBuf, version: u32 },
    Io { path: PathBuf, detail: String },
    /// Another process holds the lock.
    Locked { path: PathBuf, holder: Option<String> },
}

impl std::fmt::Display for KeeperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAccount(k) => write!(f, "unknown keeper account '{k}'"),
            Self::NotDue { key, phase } => {
                write!(f, "account '{key}' is not awaiting activation (phase {phase})")
            }
            Self::CorruptState { path, detail } => write!(
                f,
                "keeper state at {} is unreadable ({detail}); refusing to run — move it aside to reset",
                path.display()
            ),
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "keeper state at {} has version {version} (this build supports {STATE_VERSION})",
                path.display()
            ),
            Self::Io { path, detail } => write!(f, "{}: {detail}", path.display()),
            Self::Locked { path, holder } => match holder {
                Some(h) => write!(f, "{} is held by {h}", path.display()),
                None => write!(f, "{} is held by another process", path.display()),
            },
        }
    }
}

impl std::error::Error for KeeperError {}

// ── State machine ────────────────────────────────────────────────────────────

fn set_phase(st: &mut AccountState, events: &mut Vec<KeeperEvent>, to: Phase) {
    if st.phase != to {
        let from = st.phase.label().to_string();
        let to_label = to.label().to_string();
        st.phase = to;
        if from != to_label {
            events.push(KeeperEvent::PhaseChanged {
                key: st.identity.key(),
                from,
                to: to_label,
            });
        }
    }
}

fn grow_backoff(st: &mut AccountState, cfg: &KeeperConfig) -> u64 {
    st.backoff_ms = if st.backoff_ms == 0 {
        cfg.poll_interval_ms
    } else {
        st.backoff_ms.saturating_mul(2).min(cfg.max_backoff_ms)
    };
    st.backoff_ms
}

/// Schedule the next poll: the regular interval, or just after a known
/// future reset — whichever is sooner — so a rollover is never missed by a
/// whole interval.
fn schedule_regular(st: &mut AccountState, cfg: &KeeperConfig, now_ms: u64) {
    let mut next = now_ms + cfg.poll_interval_ms;
    if let Some(g) = st.generation {
        if g > now_ms {
            next = next.min(g + cfg.reset_grace_ms);
        }
    }
    st.next_action_at_ms = next;
}

/// Account-wide capacity evidence in a fresh reading: no overall limit
/// assertion and headroom on every account-wide window (at least one).
fn account_wide_headroom(windows: &[WindowLimit], limit_reached: Option<bool>, exhausted_at: f64) -> bool {
    if limit_reached == Some(true) {
        return false;
    }
    let mut seen = false;
    for w in windows.iter().filter(|w| w.models.is_none()) {
        seen = true;
        if !matches!(w.verdict(exhausted_at), WindowVerdict::Headroom { .. }) {
            return false;
        }
    }
    seen
}

/// Feed one fresh usage observation for `key` into the state machine.
pub fn observe(
    state: &mut KeeperState,
    cfg: &KeeperConfig,
    now_ms: u64,
    key: &str,
    obs: &UsageObservation,
) -> Result<Vec<KeeperEvent>, KeeperError> {
    let st = state
        .accounts
        .get_mut(key)
        .ok_or_else(|| KeeperError::UnknownAccount(key.to_string()))?;
    let mut events = Vec::new();
    st.last_poll_ms = Some(now_ms);
    st.last_poll_ok = false;
    state.updated_at_ms = now_ms;

    // A reading stamped in the future is malformed — never evidence.
    let future_stamped = obs.observed_at_ms > now_ms.saturating_add(MAX_FUTURE_OBSERVATION_SKEW_MS);

    match &obs.outcome {
        ObservationOutcome::Transport { detail } => {
            st.last_error = Some(detail.clone());
            st.consecutive_errors += 1;
            let b = grow_backoff(st, cfg);
            st.next_action_at_ms = now_ms + b;
            events.push(KeeperEvent::PollFailed {
                key: key.to_string(),
                detail: detail.clone(),
                backoff_ms: b,
            });
            // A pending attempt cannot be verified through a failed poll;
            // the verification window keeps running.
            expire_pending(st, cfg, now_ms, &mut events);
            Ok(events)
        }
        ObservationOutcome::AuthError { detail } => {
            st.last_error = Some(detail.clone());
            st.consecutive_errors += 1;
            st.backoff_ms = cfg.max_backoff_ms;
            st.next_action_at_ms = now_ms + cfg.max_backoff_ms;
            set_phase(
                st,
                &mut events,
                Phase::AuthError {
                    detail: detail.clone(),
                },
            );
            events.push(KeeperEvent::AuthError {
                key: key.to_string(),
                detail: detail.clone(),
            });
            Ok(events)
        }
        ObservationOutcome::Malformed { detail } | ObservationOutcome::Unsupported { detail } => {
            st.last_error = Some(detail.clone());
            st.consecutive_errors += 1;
            let b = grow_backoff(st, cfg);
            st.next_action_at_ms = now_ms + b;
            set_phase(
                st,
                &mut events,
                Phase::Unknown {
                    reason: detail.clone(),
                },
            );
            events.push(KeeperEvent::PollFailed {
                key: key.to_string(),
                detail: detail.clone(),
                backoff_ms: b,
            });
            expire_pending(st, cfg, now_ms, &mut events);
            Ok(events)
        }
        ObservationOutcome::Ok { .. } if future_stamped => {
            let detail = "observation timestamp is in the future".to_string();
            st.last_error = Some(detail.clone());
            st.consecutive_errors += 1;
            let b = grow_backoff(st, cfg);
            st.next_action_at_ms = now_ms + b;
            set_phase(
                st,
                &mut events,
                Phase::Unknown {
                    reason: detail.clone(),
                },
            );
            events.push(KeeperEvent::PollFailed {
                key: key.to_string(),
                detail,
                backoff_ms: b,
            });
            expire_pending(st, cfg, now_ms, &mut events);
            Ok(events)
        }
        ObservationOutcome::Ok {
            windows,
            models,
            limit_reached,
            banked,
            ..
        } => {
            st.last_error = None;
            st.consecutive_errors = 0;
            st.backoff_ms = 0;
            st.last_poll_ok = true;
            st.last_ok_observation_ms = Some(obs.observed_at_ms);
            st.last_windows = windows.clone();
            st.last_models = models.clone();
            st.last_limit_reached = *limit_reached;
            if let Some(b) = banked {
                if let Some(count) = b.available_count.filter(|c| *c > 0) {
                    let expiring_soon = b
                        .earliest_expiry_ms
                        .is_some_and(|e| e <= now_ms.saturating_add(cfg.banked_expiry_alert_ms));
                    events.push(KeeperEvent::BankedResets {
                        key: key.to_string(),
                        available_count: count,
                        earliest_expiry_ms: b.earliest_expiry_ms,
                        expiring_soon,
                    });
                }
            }
            st.banked = banked.clone();
            let weekly = weekly_window(windows).cloned();
            st.weekly = weekly.clone();
            let observed = obs.observed_at_ms;
            let headroom_now =
                account_wide_headroom(windows, *limit_reached, cfg.exhausted_at_percent);

            match weekly.as_ref().and_then(|w| w.resets_at_ms.map(|r| (w, r))) {
                None => {
                    // No weekly reset evidence in the fresh reading.
                    let reason = if weekly.is_some() {
                        "weekly window reported without a reset time"
                    } else {
                        "no weekly window evidence"
                    };
                    match st.generation {
                        Some(g) if g <= now_ms && headroom_now => {
                            // Known window ended; nothing newer reported; the
                            // provider asserts capacity → due.
                            window_ended(st, cfg, now_ms, g, &mut events);
                        }
                        Some(g) if g <= now_ms => {
                            // Ended, but no headroom evidence → not due.
                            expire_pending(st, cfg, now_ms, &mut events);
                            if !matches!(st.phase, Phase::ActivationPending { .. }) {
                                set_phase(
                                    st,
                                    &mut events,
                                    Phase::Unknown {
                                        reason: format!("{reason}; no headroom evidence after reset"),
                                    },
                                );
                            }
                            st.generation = Some(g);
                            schedule_regular(st, cfg, now_ms);
                        }
                        _ => {
                            set_phase(
                                st,
                                &mut events,
                                Phase::Unknown {
                                    reason: reason.into(),
                                },
                            );
                            schedule_regular(st, cfg, now_ms);
                        }
                    }
                }
                Some((w, r)) if r > now_ms => {
                    // Live window with a future reset.
                    let prev = st.generation;
                    if let Some(p) = prev {
                        if r < p {
                            // Provider went backwards: contradictory evidence.
                            set_phase(
                                st,
                                &mut events,
                                Phase::Unknown {
                                    reason: "reported reset earlier than the tracked generation".into(),
                                },
                            );
                            schedule_regular(st, cfg, now_ms);
                            return Ok(events);
                        }
                    }
                    let is_new = prev != Some(r);
                    let mut attribution = match &st.phase {
                        Phase::Active { attribution, .. } if !is_new => *attribution,
                        _ => Attribution::Observed,
                    };
                    if is_new {
                        // Correlate with an attempt for the previous generation.
                        let attempt = match &st.phase {
                            Phase::ActivationPending { attempt, .. } => Some(*attempt),
                            Phase::Unverified { attempts, .. } if *attempts > 0 => Some(*attempts),
                            _ => None,
                        };
                        if let Some(a) = attempt {
                            attribution = Attribution::AttemptCorrelated { attempt: a };
                        }
                        // Idle delay: anchor − previous reset.
                        let (idle, upper) = match (prev, w.duration_ms) {
                            (Some(p), Some(d)) if r >= d => {
                                let anchor = r - d;
                                (Some(anchor.saturating_sub(p)), false)
                            }
                            (Some(p), _) => (Some(observed.saturating_sub(p)), true),
                            (None, _) => (None, false),
                        };
                        st.last_idle_delay_ms = idle;
                        st.last_idle_delay_is_upper_bound = upper;
                        for rec in st.history.iter_mut().rev() {
                            if rec.new_generation.is_none() && rec.generation != r {
                                rec.verified_at_ms = Some(observed);
                                rec.new_generation = Some(r);
                                break;
                            }
                        }
                        st.attempts_in_generation = 0;
                        st.not_sent_in_generation = 0;
                        st.next_attempt_allowed_ms = 0;
                        events.push(KeeperEvent::NewWindow {
                            key: key.to_string(),
                            from_generation: prev,
                            to_generation: r,
                            idle_delay_ms: idle,
                            idle_delay_is_upper_bound: upper,
                            attribution,
                        });
                    }
                    st.generation = Some(r);
                    let phase = if *limit_reached == Some(true) {
                        Phase::Exhausted {
                            generation: r,
                            reset_passed: false,
                        }
                    } else {
                        match w.verdict(cfg.exhausted_at_percent) {
                            WindowVerdict::Headroom { used_percent } => Phase::Active {
                                generation: r,
                                used_percent,
                                verified_at_ms: observed,
                                attribution,
                            },
                            WindowVerdict::Exhausted { .. } => Phase::Exhausted {
                                generation: r,
                                reset_passed: false,
                            },
                            WindowVerdict::Unknown => Phase::Unknown {
                                reason: "weekly window has no usage evidence".into(),
                            },
                        }
                    };
                    set_phase(st, &mut events, phase);
                    schedule_regular(st, cfg, now_ms);
                }
                Some((w, r)) => {
                    // Reset already passed and the provider still reports it.
                    if let Some(p) = st.generation {
                        if r < p {
                            set_phase(
                                st,
                                &mut events,
                                Phase::Unknown {
                                    reason: "reported reset earlier than the tracked generation".into(),
                                },
                            );
                            schedule_regular(st, cfg, now_ms);
                            return Ok(events);
                        }
                    }
                    if st.generation != Some(r) {
                        st.generation = Some(r);
                        st.attempts_in_generation = 0;
                        st.not_sent_in_generation = 0;
                        st.next_attempt_allowed_ms = 0;
                    }
                    let weekly_headroom =
                        matches!(w.verdict(cfg.exhausted_at_percent), WindowVerdict::Headroom { .. });
                    if headroom_now && weekly_headroom {
                        // Ended, nothing newer, capacity asserted → due.
                        window_ended(st, cfg, now_ms, r, &mut events);
                    } else {
                        // The provider still asserts no capacity after the
                        // reset: never due. Keep a pending attempt's
                        // verification clock running.
                        expire_pending(st, cfg, now_ms, &mut events);
                        if !matches!(st.phase, Phase::ActivationPending { .. }) {
                            set_phase(
                                st,
                                &mut events,
                                Phase::Exhausted {
                                    generation: r,
                                    reset_passed: true,
                                },
                            );
                        }
                        events.push(KeeperEvent::LimitAssertedAfterReset {
                            key: key.to_string(),
                            generation: r,
                        });
                        st.next_action_at_ms = now_ms + cfg.poll_interval_ms;
                    }
                }
            }
            Ok(events)
        }
    }
}

/// The window for `g` has ended, fresh usage shows nothing newer and the
/// provider asserts capacity.
fn window_ended(
    st: &mut AccountState,
    cfg: &KeeperConfig,
    now_ms: u64,
    g: u64,
    events: &mut Vec<KeeperEvent>,
) {
    match st.phase.clone() {
        Phase::ActivationPending { generation, .. } if generation == g => {
            // Still awaiting verification.
            expire_pending(st, cfg, now_ms, events);
            if matches!(st.phase, Phase::ActivationPending { .. }) {
                st.next_action_at_ms = now_ms + cfg.verify_poll_ms;
            } else {
                st.next_action_at_ms = now_ms + cfg.poll_interval_ms;
            }
        }
        Phase::Unverified { generation, .. } if generation == g => {
            st.next_action_at_ms = now_ms + cfg.poll_interval_ms;
        }
        Phase::AwaitingActivation {
            generation,
            due_since_ms,
        } if generation == g => {
            events.push(KeeperEvent::ActivationDue {
                key: st.identity.key(),
                generation: g,
                due_since_ms,
            });
            st.next_action_at_ms = now_ms + cfg.poll_interval_ms;
        }
        _ => {
            if st.attempts_in_generation >= cfg.max_attempts_per_generation {
                let attempts = st.attempts_in_generation;
                set_phase(
                    st,
                    events,
                    Phase::Unverified {
                        generation: g,
                        attempts,
                    },
                );
            } else {
                // Due since the reset instant itself: idle delay is measured
                // against the generation, not against poll times.
                set_phase(
                    st,
                    events,
                    Phase::AwaitingActivation {
                        generation: g,
                        due_since_ms: g,
                    },
                );
                events.push(KeeperEvent::ActivationDue {
                    key: st.identity.key(),
                    generation: g,
                    due_since_ms: g,
                });
            }
            st.next_action_at_ms = now_ms + cfg.poll_interval_ms;
        }
    }
}

/// Move a pending attempt whose verification window elapsed to `Unverified`
/// (or back to `AwaitingActivation` when budget remains).
fn expire_pending(
    st: &mut AccountState,
    cfg: &KeeperConfig,
    now_ms: u64,
    events: &mut Vec<KeeperEvent>,
) {
    if let Phase::ActivationPending {
        generation,
        started_ms,
        ..
    } = st.phase.clone()
    {
        if now_ms.saturating_sub(started_ms) >= cfg.verify_window_ms {
            let attempts = st.attempts_in_generation;
            if attempts >= cfg.max_attempts_per_generation {
                set_phase(
                    st,
                    events,
                    Phase::Unverified {
                        generation,
                        attempts,
                    },
                );
                events.push(KeeperEvent::Unverified {
                    key: st.identity.key(),
                    generation,
                    attempts,
                });
            } else {
                set_phase(
                    st,
                    events,
                    Phase::AwaitingActivation {
                        generation,
                        due_since_ms: generation,
                    },
                );
            }
        }
    }
}

/// Runner-facing decision after a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationDecision {
    Activate { generation: u64, attempt: u32 },
    Skip { reason: String },
}

/// Capacity view of the last successful poll, for `quota_policy`.
pub fn capacity_view(st: &AccountState) -> Option<AccountCapacity> {
    let credential = CredentialRef::parse_storage_key(&st.identity.credential)?;
    Some(AccountCapacity {
        credential,
        observed_at_ms: st.last_ok_observation_ms,
        observation: QuotaObservation::Ok {
            windows: windows_with_overall_flag(&st.last_windows, st.last_limit_reached),
            models: st.last_models.clone(),
        },
        cooldown_until_ms: None,
    })
}

/// Whether the runner may start an activation attempt now.
///
/// Read-only unless `opted_in`. Requires: an awaiting phase with budget, the
/// **latest** poll successful and fresh, no overall limit assertion, and
/// headroom on every window applicable to `model` (5h, weekly and
/// model-scoped alike) with the model not listed exhausted/unknown — the
/// same fail-closed evaluation the selection policy uses.
pub fn activation_decision(
    state: &KeeperState,
    cfg: &KeeperConfig,
    now_ms: u64,
    key: &str,
    opted_in: bool,
    model: &str,
) -> ActivationDecision {
    let skip = |r: &str| ActivationDecision::Skip {
        reason: r.to_string(),
    };
    let Some(st) = state.accounts.get(key) else {
        return skip("unknown account");
    };
    if !opted_in {
        return skip("not opted in (read-only)");
    }
    let generation = match &st.phase {
        Phase::AwaitingActivation { generation, .. } => *generation,
        Phase::ActivationPending { .. } => return skip("attempt pending verification"),
        Phase::Unverified { .. } => return skip("attempt budget spent for this generation"),
        other => return skip(&format!("not due (phase {})", other.label())),
    };
    if st.attempts_in_generation >= cfg.max_attempts_per_generation {
        return skip("attempt budget spent for this generation");
    }
    if st.not_sent_in_generation >= MAX_NOT_SENT_PER_GENERATION {
        return skip("too many unsent attempts for this generation");
    }
    if now_ms < st.next_attempt_allowed_ms {
        return skip("backoff after an unsent attempt");
    }
    if !st.last_poll_ok {
        return skip("latest poll failed; no fresh evidence");
    }
    if st.last_limit_reached == Some(true) {
        return skip("provider asserts limit_reached");
    }
    let Some(cap) = capacity_view(st) else {
        return skip("unparseable credential key");
    };
    let req = SelectionRequest {
        model: Some(model),
        strategy: Strategy::PreferenceOrder,
        exhausted_at_percent: cfg.exhausted_at_percent,
        ..SelectionRequest::new(cap.credential.provider, now_ms, cfg.stale_after_ms)
    };
    if let Err(reason) = quota_policy::evaluate(&req, &cap) {
        return skip(&format!("no proven capacity: {reason}"));
    }
    ActivationDecision::Activate {
        generation,
        attempt: st.attempts_in_generation + 1,
    }
}

/// Ticket for one attempt. The caller MUST persist the state between
/// [`begin_attempt`] and the request, so a crash leaves a pending record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptTicket {
    pub key: String,
    pub generation: u64,
    pub attempt: u32,
}

/// Record an attempt as started. Consumes one unit of the generation budget.
pub fn begin_attempt(
    state: &mut KeeperState,
    now_ms: u64,
    key: &str,
) -> Result<(AttemptTicket, Vec<KeeperEvent>), KeeperError> {
    let st = state
        .accounts
        .get_mut(key)
        .ok_or_else(|| KeeperError::UnknownAccount(key.to_string()))?;
    let generation = match &st.phase {
        Phase::AwaitingActivation { generation, .. } => *generation,
        other => {
            return Err(KeeperError::NotDue {
                key: key.to_string(),
                phase: other.label().to_string(),
            })
        }
    };
    st.attempts_in_generation += 1;
    let attempt = st.attempts_in_generation;
    let mut events = Vec::new();
    set_phase(
        st,
        &mut events,
        Phase::ActivationPending {
            generation,
            attempt,
            started_ms: now_ms,
            outcome: None,
        },
    );
    st.push_history(AttemptRecord {
        attempt,
        generation,
        started_ms: now_ms,
        finished_ms: None,
        outcome: None,
        verified_at_ms: None,
        new_generation: None,
    });
    st.next_action_at_ms = now_ms;
    state.updated_at_ms = now_ms;
    events.push(KeeperEvent::AttemptStarted {
        key: key.to_string(),
        generation,
        attempt,
    });
    Ok((
        AttemptTicket {
            key: key.to_string(),
            generation,
            attempt,
        },
        events,
    ))
}

/// Record the outcome of an attempt. Refundable outcomes return the budget
/// unit and apply backoff; all others stay pending until verified/expired.
pub fn finish_attempt(
    state: &mut KeeperState,
    cfg: &KeeperConfig,
    now_ms: u64,
    ticket: &AttemptTicket,
    outcome: AttemptOutcome,
) -> Result<Vec<KeeperEvent>, KeeperError> {
    let st = state
        .accounts
        .get_mut(&ticket.key)
        .ok_or_else(|| KeeperError::UnknownAccount(ticket.key.clone()))?;
    let mut events = Vec::new();
    if let Some(rec) = st
        .history
        .iter_mut()
        .rev()
        .find(|r| r.generation == ticket.generation && r.attempt == ticket.attempt)
    {
        rec.finished_ms = Some(now_ms);
        rec.outcome = Some(outcome.clone());
    }
    let refunded = outcome.is_refundable();
    match &st.phase {
        Phase::ActivationPending {
            generation,
            attempt,
            ..
        } if *generation == ticket.generation && *attempt == ticket.attempt => {}
        _ => {
            // Phase moved on (e.g. verified through a concurrent poll); keep
            // the record only.
            events.push(KeeperEvent::AttemptFinished {
                key: ticket.key.clone(),
                generation: ticket.generation,
                attempt: ticket.attempt,
                outcome,
                refunded: false,
            });
            state.updated_at_ms = now_ms;
            return Ok(events);
        }
    }
    if refunded {
        st.attempts_in_generation = st.attempts_in_generation.saturating_sub(1);
        st.not_sent_in_generation += 1;
        let b = grow_backoff(st, cfg);
        st.next_attempt_allowed_ms = now_ms + b;
        st.next_action_at_ms = now_ms + b;
        if let AttemptOutcome::Rejected { http_status } = &outcome {
            if matches!(http_status, 401 | 403) {
                set_phase(
                    st,
                    &mut events,
                    Phase::AuthError {
                        detail: format!("activation rejected with HTTP {http_status}"),
                    },
                );
                events.push(KeeperEvent::AuthError {
                    key: ticket.key.clone(),
                    detail: format!("activation rejected with HTTP {http_status}"),
                });
            }
        }
        if !matches!(st.phase, Phase::AuthError { .. }) {
            set_phase(
                st,
                &mut events,
                Phase::AwaitingActivation {
                    generation: ticket.generation,
                    due_since_ms: ticket.generation,
                },
            );
        }
        st.last_error = Some(match &outcome {
            AttemptOutcome::NotSent { stage, reason } => {
                format!("activation not sent ({stage:?}): {reason}")
            }
            AttemptOutcome::Rejected { http_status } => {
                format!("activation rejected: HTTP {http_status}")
            }
            _ => "activation refunded".into(),
        });
    } else {
        st.phase = Phase::ActivationPending {
            generation: ticket.generation,
            attempt: ticket.attempt,
            started_ms: match &st.phase {
                Phase::ActivationPending { started_ms, .. } => *started_ms,
                _ => now_ms,
            },
            outcome: Some(outcome.clone()),
        };
        // Verify from fresh usage as soon as possible.
        st.next_action_at_ms = now_ms;
        if let AttemptOutcome::Ambiguous { reason } = &outcome {
            st.last_error = Some(format!("activation ambiguous: {reason}"));
        }
    }
    state.updated_at_ms = now_ms;
    events.push(KeeperEvent::AttemptFinished {
        key: ticket.key.clone(),
        generation: ticket.generation,
        attempt: ticket.attempt,
        outcome,
        refunded,
    });
    Ok(events)
}

/// Housekeeping on startup: a pending attempt with no recorded outcome means
/// the previous process died between begin and finish — mark it ambiguous.
pub fn recover_after_restart(state: &mut KeeperState, now_ms: u64) -> Vec<KeeperEvent> {
    let mut events = Vec::new();
    for (key, st) in state.accounts.iter_mut() {
        if let Phase::ActivationPending {
            generation,
            attempt,
            started_ms,
            outcome: None,
        } = st.phase.clone()
        {
            let outcome = AttemptOutcome::Ambiguous {
                reason: "process restarted before the attempt outcome was recorded".into(),
            };
            st.phase = Phase::ActivationPending {
                generation,
                attempt,
                started_ms,
                outcome: Some(outcome.clone()),
            };
            if let Some(rec) = st
                .history
                .iter_mut()
                .rev()
                .find(|r| r.generation == generation && r.attempt == attempt)
            {
                rec.finished_ms = Some(now_ms);
                rec.outcome = Some(outcome.clone());
            }
            st.next_action_at_ms = now_ms;
            events.push(KeeperEvent::AttemptFinished {
                key: key.clone(),
                generation,
                attempt,
                outcome,
                refunded: false,
            });
        }
    }
    events
}

/// Explicit operator re-arm: allow exactly one more attempt for the
/// generation currently in `Unverified`. Never automatic. Returns `Ok(None)`
/// when the account is not unverified (nothing to re-arm).
pub fn rearm(
    state: &mut KeeperState,
    now_ms: u64,
    key: &str,
) -> Result<Option<KeeperEvent>, KeeperError> {
    let st = state
        .accounts
        .get_mut(key)
        .ok_or_else(|| KeeperError::UnknownAccount(key.to_string()))?;
    let Phase::Unverified {
        generation,
        attempts,
    } = st.phase.clone()
    else {
        return Ok(None);
    };
    st.attempts_in_generation = 0;
    st.not_sent_in_generation = 0;
    st.next_attempt_allowed_ms = 0;
    st.phase = Phase::AwaitingActivation {
        generation,
        due_since_ms: generation,
    };
    st.next_action_at_ms = now_ms;
    state.updated_at_ms = now_ms;
    Ok(Some(KeeperEvent::Rearmed {
        key: key.to_string(),
        generation,
        previous_attempts: attempts,
    }))
}

// ── Persistence ──────────────────────────────────────────────────────────────

/// Private, atomic JSON state (dir `0700`, file `0600`, tmp+rename).
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE_NAME)
    }

    /// Missing file → default state. Unreadable/unparseable → error (fail closed).
    pub fn load(&self) -> Result<KeeperState, KeeperError> {
        let path = self.path();
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(KeeperState::default())
            }
            Err(e) => {
                return Err(KeeperError::Io {
                    path,
                    detail: e.to_string(),
                })
            }
        };
        let state: KeeperState =
            serde_json::from_slice(&raw).map_err(|e| KeeperError::CorruptState {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        if state.version > STATE_VERSION {
            return Err(KeeperError::UnsupportedVersion {
                path,
                version: state.version,
            });
        }
        Ok(state)
    }

    pub fn save(&self, state: &KeeperState) -> Result<(), KeeperError> {
        crate::private_fs::ensure_private_dir(&self.dir).map_err(|e| KeeperError::Io {
            path: self.dir.clone(),
            detail: e.to_string(),
        })?;
        let mut out = KeeperState {
            version: STATE_VERSION,
            ..state.clone()
        };
        if out.updated_at_ms == 0 {
            out.updated_at_ms = state.updated_at_ms;
        }
        let bytes = serde_json::to_vec_pretty(&out).map_err(|e| KeeperError::Io {
            path: self.path(),
            detail: e.to_string(),
        })?;
        crate::private_fs::write_atomic_private(&self.path(), &bytes).map_err(|e| KeeperError::Io {
            path: self.path(),
            detail: e.to_string(),
        })
    }
}

// ── Locks ────────────────────────────────────────────────────────────────────

/// Exclusive advisory `flock` held for the process lifetime (released on
/// drop or process death). Used for the state-dir singleton and for one lock
/// per kept account in the canonical keeper dir, so a second keeper with a
/// different `--state-dir` still cannot act on the same account.
#[derive(Debug)]
pub struct KeeperLock {
    _file: File,
    path: PathBuf,
}

impl KeeperLock {
    pub fn acquire(dir: &Path, name: &str, holder: &str) -> Result<Self, KeeperError> {
        use fs4::fs_std::FileExt;
        crate::private_fs::ensure_private_dir(dir).map_err(|e| KeeperError::Io {
            path: dir.to_path_buf(),
            detail: e.to_string(),
        })?;
        let path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| KeeperError::Io {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        let locked = FileExt::try_lock_exclusive(&file).map_err(|e| KeeperError::Io {
            path: path.clone(),
            detail: e.to_string(),
        })?;
        if !locked {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            let holder = body.lines().next().map(str::to_string).filter(|s| !s.is_empty());
            return Err(KeeperError::Locked { path, holder });
        }
        use std::io::Write;
        let _ = file.set_len(0);
        let _ = (&file).write_all(format!("{holder}\n").as_bytes());
        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// File-name-safe form of a storage key for per-account locks.
pub fn account_lock_name(storage_key: &str) -> String {
    let safe: String = storage_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::quota_policy::ModelState;

    const NOW: u64 = 1_800_000_000_000;
    const WEEK: u64 = 7 * DAY;
    const MODEL: &str = "gpt-5.4-mini";

    fn ident(label: &str) -> AccountIdentity {
        AccountIdentity {
            source: "local:/tmp/auth.json".into(),
            credential: format!("openai-codex@{label}"),
            identity_fp: "id:2b2f1234".into(),
        }
    }

    fn cfg() -> KeeperConfig {
        KeeperConfig::default().bounded()
    }

    fn five_hour(observed: u64, used: f64) -> WindowLimit {
        WindowLimit {
            id: "primary".into(),
            duration_ms: Some(5 * HOUR),
            used_percent: Some(used),
            limit_reached: Some(used >= 100.0),
            resets_at_ms: Some(observed + HOUR),
            models: None,
        }
    }

    fn weekly(used: f64, resets: Option<u64>) -> WindowLimit {
        WindowLimit {
            id: "secondary".into(),
            duration_ms: Some(WEEK),
            used_percent: Some(used),
            limit_reached: Some(used >= 100.0),
            resets_at_ms: resets,
            models: None,
        }
    }

    /// Reading with a 5h window at 1 % and a weekly window at `used`.
    fn ok_obs(observed: u64, used: f64, resets: Option<u64>) -> UsageObservation {
        UsageObservation {
            observed_at_ms: observed,
            outcome: ObservationOutcome::Ok {
                windows: vec![five_hour(observed, 1.0), weekly(used, resets)],
                models: None,
                limit_reached: Some(used >= 100.0),
                banked: None,
                identity_prefix: Some("2b2f1234".into()),
            },
        }
    }

    fn no_weekly_obs(observed: u64) -> UsageObservation {
        UsageObservation {
            observed_at_ms: observed,
            outcome: ObservationOutcome::Ok {
                windows: vec![five_hour(observed, 0.0)],
                models: None,
                limit_reached: Some(false),
                banked: None,
                identity_prefix: None,
            },
        }
    }

    fn transport(t: u64) -> UsageObservation {
        UsageObservation {
            observed_at_ms: t,
            outcome: ObservationOutcome::Transport {
                detail: "connect".into(),
            },
        }
    }

    fn fresh(label: &str) -> (KeeperState, String) {
        let mut s = KeeperState::default();
        let id = ident(label);
        s.entry(&id);
        (s, id.key())
    }

    /// Drive an account to "due": exhausted live window, reset passes, fresh
    /// reading shows the old (past) reset with headroom (provider asserts
    /// capacity again but has not anchored a new window).
    fn make_due(label: &str) -> (KeeperState, String, u64, u64) {
        let c = cfg();
        let (mut s, k) = fresh(label);
        let g1 = NOW + HOUR;
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { reset_passed: false, .. }));
        let t = g1 + MIN;
        observe(&mut s, &c, t, &k, &ok_obs(t, 0.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { generation, due_since_ms } if generation == g1 && due_since_ms == g1));
        (s, k, g1, t)
    }

    fn decide(s: &KeeperState, t: u64, k: &str, opted_in: bool) -> ActivationDecision {
        activation_decision(s, &cfg(), t, k, opted_in, MODEL)
    }

    #[test]
    fn config_bounds_untrusted_intervals() {
        let c = KeeperConfig {
            poll_interval_ms: 1,
            max_backoff_ms: 0,
            stale_after_ms: u64::MAX,
            reset_grace_ms: u64::MAX,
            verify_poll_ms: 0,
            verify_window_ms: u64::MAX,
            max_attempts_per_generation: 99,
            attempt_timeout_ms: 0,
            usage_timeout_ms: u64::MAX,
            banked_expiry_alert_ms: 0,
            exhausted_at_percent: f64::NAN,
        }
        .bounded();
        assert_eq!(c.poll_interval_ms, MIN_POLL_INTERVAL_MS);
        assert_eq!(c.max_backoff_ms, MIN_POLL_INTERVAL_MS);
        assert_eq!(c.stale_after_ms, MAX_STALE_AFTER_MS);
        assert_eq!(c.reset_grace_ms, MAX_RESET_GRACE_MS);
        assert_eq!(c.max_attempts_per_generation, 1);
        assert_eq!(c.attempt_timeout_ms, MIN_ATTEMPT_TIMEOUT_MS);
        assert_eq!(c.exhausted_at_percent, DEFAULT_EXHAUSTED_AT_PERCENT);
        assert!(c.verify_window_ms <= MAX_VERIFY_WINDOW_MS);
        assert!(c.verify_poll_ms >= 5 * SEC);
        assert_eq!(KeeperConfig::default().bounded(), KeeperConfig::default());
        assert_eq!(KeeperConfig::default().poll_interval_ms, 5 * MIN);
    }

    #[test]
    fn identity_fingerprint_prefers_account_id_and_changes_on_relogin() {
        assert_eq!(
            AccountIdentity::fingerprint(Some("2b2f1234"), Some("a@b"), Some(1)),
            "id:2b2f1234"
        );
        let a = AccountIdentity::fingerprint(None, Some("a@b"), Some(1));
        let b = AccountIdentity::fingerprint(None, Some("c@d"), Some(1));
        assert!(a.starts_with("who:") && b.starts_with("who:") && a != b);
        assert_eq!(AccountIdentity::fingerprint(None, None, Some(7)), "added:7");
        assert_eq!(AccountIdentity::fingerprint(None, None, None), "unknown");
        let mut i1 = ident("x");
        let mut i2 = ident("x");
        i1.identity_fp = "id:aaaa".into();
        i2.identity_fp = "id:bbbb".into();
        assert_ne!(i1.key(), i2.key());
        // Re-login under the same alias with another seat → a due ticket
        // does not carry over.
        let (mut s, k, _g, _t) = make_due("x");
        let relogged = AccountIdentity {
            identity_fp: "id:other".into(),
            ..ident("x")
        };
        s.entry(&relogged);
        assert!(matches!(s.accounts[&relogged.key()].phase, Phase::Unknown { .. }));
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { .. }));
        assert_eq!(s.accounts.len(), 2);
    }

    #[test]
    fn live_window_tracks_generation_and_schedules_reset_grace() {
        let (mut s, k) = fresh("a");
        let reset = NOW + 2 * MIN;
        let ev = observe(&mut s, &cfg(), NOW, &k, &ok_obs(NOW, 10.0, Some(reset))).unwrap();
        let st = &s.accounts[&k];
        assert!(matches!(st.phase, Phase::Active { generation, .. } if generation == reset));
        assert_eq!(st.generation, Some(reset));
        assert!(st.last_poll_ok);
        // reset sooner than the poll interval → poll at reset + grace
        assert_eq!(st.next_action_at_ms, reset + cfg().reset_grace_ms);
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::NewWindow { from_generation: None, .. })));
        // exhausted live window
        observe(&mut s, &cfg(), NOW + 1, &k, &ok_obs(NOW + 1, 100.0, Some(reset))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { generation, reset_passed: false } if generation == reset));
        // overall limit flag alone makes it exhausted even with weekly headroom
        let mut obs = ok_obs(NOW + 1, 10.0, Some(reset));
        if let ObservationOutcome::Ok { limit_reached, .. } = &mut obs.outcome {
            *limit_reached = Some(true);
        }
        observe(&mut s, &cfg(), NOW + 1, &k, &obs).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { .. }));
        // far reset → regular interval
        let far = NOW + 3 * DAY;
        observe(&mut s, &cfg(), NOW + 2, &k, &ok_obs(NOW + 2, 50.0, Some(far))).unwrap();
        assert_eq!(s.accounts[&k].next_action_at_ms, NOW + 2 + cfg().poll_interval_ms);
    }

    #[test]
    fn weekly_from_duration_not_primary_name() {
        // "primary" is weekly here, "secondary" is 5h.
        let obs = UsageObservation {
            observed_at_ms: NOW,
            outcome: ObservationOutcome::Ok {
                windows: vec![
                    WindowLimit {
                        id: "primary".into(),
                        duration_ms: Some(WEEK),
                        used_percent: Some(30.0),
                        limit_reached: Some(false),
                        resets_at_ms: Some(NOW + 3 * DAY),
                        models: None,
                    },
                    WindowLimit {
                        id: "secondary".into(),
                        duration_ms: Some(5 * HOUR),
                        used_percent: Some(100.0),
                        limit_reached: Some(true),
                        resets_at_ms: Some(NOW + HOUR),
                        models: None,
                    },
                ],
                models: None,
                limit_reached: Some(false),
                banked: None,
                identity_prefix: None,
            },
        };
        let (mut s, k) = fresh("a");
        observe(&mut s, &cfg(), NOW, &k, &obs).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Active { generation, .. } if generation == NOW + 3 * DAY));
        assert_eq!(s.accounts[&k].weekly.as_ref().unwrap().id, "primary");
    }

    #[test]
    fn passed_reset_with_asserted_limit_is_exhausted_never_due() {
        let c = cfg();
        let (mut s, k) = fresh("a");
        let g1 = NOW + HOUR;
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        // Reset passed; provider still says weekly 100 % / limit_reached.
        let t = g1 + MIN;
        let ev = observe(&mut s, &c, t, &k, &ok_obs(t, 100.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { generation, reset_passed: true } if generation == g1));
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::LimitAssertedAfterReset { .. })));
        assert!(!ev.iter().any(|e| matches!(e, KeeperEvent::ActivationDue { .. })));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        assert!(matches!(begin_attempt(&mut s, t, &k), Err(KeeperError::NotDue { .. })));
        // Weekly 0 % but overall limit_reached true (some other limit) → still not due.
        let mut obs = ok_obs(t, 0.0, Some(g1));
        if let ObservationOutcome::Ok { limit_reached, .. } = &mut obs.outcome {
            *limit_reached = Some(true);
        }
        observe(&mut s, &c, t, &k, &obs).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { reset_passed: true, .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        // Weekly 0 % but the 5h window is exhausted → not due either.
        let mut obs = ok_obs(t, 0.0, Some(g1));
        if let ObservationOutcome::Ok { windows, .. } = &mut obs.outcome {
            windows[0] = five_hour(t, 100.0);
        }
        observe(&mut s, &c, t, &k, &obs).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Exhausted { reset_passed: true, .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn passed_reset_with_headroom_is_due_and_read_only_never_activates() {
        let (mut s, k, g1, t) = make_due("a");
        for _ in 0..5 {
            assert!(matches!(
                decide(&s, t, &k, false),
                ActivationDecision::Skip { reason } if reason.contains("read-only")
            ));
        }
        // due_since stays pinned to the generation across cycles
        let t2 = t + 3 * cfg().poll_interval_ms;
        let ev = observe(&mut s, &cfg(), t2, &k, &ok_obs(t2, 0.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { due_since_ms, .. } if due_since_ms == g1));
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::ActivationDue { due_since_ms, .. } if *due_since_ms == g1)));
        // opted in → exactly one activate decision
        assert_eq!(
            decide(&s, t2, &k, true),
            ActivationDecision::Activate {
                generation: g1,
                attempt: 1
            }
        );
    }

    #[test]
    fn missing_weekly_window_needs_headroom_evidence() {
        let c = cfg();
        let (mut s, k) = fresh("a");
        let g1 = NOW + HOUR;
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        let t = g1 + MIN;
        // Weekly dropped, 5h window headroom, overall false → due.
        observe(&mut s, &c, t, &k, &no_weekly_obs(t)).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { generation, .. } if generation == g1));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Activate { .. }));
        // Weekly dropped and NO windows at all → no evidence → not due.
        let (mut s, k) = fresh("b");
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        let empty = UsageObservation {
            observed_at_ms: t,
            outcome: ObservationOutcome::Ok {
                windows: vec![],
                models: None,
                limit_reached: Some(false),
                banked: None,
                identity_prefix: None,
            },
        };
        observe(&mut s, &c, t, &k, &empty).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        // Weekly dropped, 5h headroom but overall limit_reached true → not due.
        let (mut s, k) = fresh("c");
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        let mut obs = no_weekly_obs(t);
        if let ObservationOutcome::Ok { limit_reached, .. } = &mut obs.outcome {
            *limit_reached = Some(true);
        }
        observe(&mut s, &c, t, &k, &obs).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn never_observed_account_with_no_reset_evidence_stays_unknown() {
        let (mut s, k) = fresh("a");
        observe(&mut s, &cfg(), NOW, &k, &no_weekly_obs(NOW)).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, NOW, &k, true), ActivationDecision::Skip { .. }));
        // weekly window without reset time → unknown too
        observe(&mut s, &cfg(), NOW, &k, &ok_obs(NOW, 0.0, None)).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, NOW, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn activation_requires_latest_poll_ok_fresh_and_all_windows_for_model() {
        let c = cfg();
        // Latest poll failed → blocked even though the last OK reading is fresh.
        let (mut s, k, g1, t) = make_due("a");
        observe(&mut s, &c, t + SEC, &k, &transport(t + SEC)).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { .. }));
        assert!(!s.accounts[&k].last_poll_ok);
        assert!(matches!(
            decide(&s, t + SEC, &k, true),
            ActivationDecision::Skip { reason } if reason.contains("latest poll failed")
        ));
        // A fresh OK poll restores eligibility.
        observe(&mut s, &c, t + 2 * SEC, &k, &ok_obs(t + 2 * SEC, 0.0, Some(g1))).unwrap();
        assert!(matches!(decide(&s, t + 2 * SEC, &k, true), ActivationDecision::Activate { .. }));
        // Stale: time passes without a poll.
        let late = t + 2 * SEC + c.stale_after_ms + SEC;
        assert!(matches!(
            decide(&s, late, &k, true),
            ActivationDecision::Skip { reason } if reason.contains("stale")
        ));
        // Future-stamped reading is malformed evidence, not capacity.
        let (mut s, k, g1, t) = make_due("b");
        let fut = ok_obs(t + HOUR, 0.0, Some(g1));
        observe(&mut s, &c, t, &k, &fut).unwrap();
        assert!(!s.accounts[&k].last_poll_ok);
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        // Model-scoped window for the activation model exhausted → blocked.
        let (mut s, k, g1, t) = make_due("c");
        let mut obs = ok_obs(t, 0.0, Some(g1));
        if let ObservationOutcome::Ok { windows, .. } = &mut obs.outcome {
            windows.push(WindowLimit {
                id: "model_weekly".into(),
                duration_ms: Some(WEEK),
                used_percent: Some(100.0),
                limit_reached: Some(true),
                resets_at_ms: Some(g1 + DAY),
                models: Some(vec![MODEL.into()]),
            });
        }
        observe(&mut s, &c, t, &k, &obs).unwrap();
        assert!(matches!(
            decide(&s, t, &k, true),
            ActivationDecision::Skip { reason } if reason.contains("exhausted")
        ));
        // Model listed exhausted / unknown → blocked; available → ok.
        let (mut s, k, g1, t) = make_due("d");
        for (state, ok) in [
            (ModelState::Exhausted, false),
            (ModelState::Unknown, false),
            (ModelState::Available, true),
        ] {
            let mut obs = ok_obs(t, 0.0, Some(g1));
            if let ObservationOutcome::Ok { models, .. } = &mut obs.outcome {
                *models = Some(vec![ModelAvailability {
                    model: MODEL.into(),
                    state,
                }]);
            }
            observe(&mut s, &c, t, &k, &obs).unwrap();
            assert_eq!(
                matches!(decide(&s, t, &k, true), ActivationDecision::Activate { .. }),
                ok,
                "{state:?}"
            );
        }
        // Short (5h) window with an unknown percentage → no evidence → blocked.
        let (mut s, k, g1, t) = make_due("e");
        let mut obs = ok_obs(t, 0.0, Some(g1));
        if let ObservationOutcome::Ok { windows, .. } = &mut obs.outcome {
            windows[0].used_percent = None;
            windows[0].limit_reached = None;
        }
        observe(&mut s, &c, t, &k, &obs).unwrap();
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn one_activation_per_generation_verified_only_from_fresh_usage() {
        let c = cfg();
        let (mut s, k, g1, t) = make_due("a");
        assert_eq!(
            decide(&s, t, &k, true),
            ActivationDecision::Activate {
                generation: g1,
                attempt: 1
            }
        );
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::ActivationPending { attempt: 1, .. }));
        assert_eq!(s.accounts[&k].attempts_in_generation, 1);
        // while pending nothing else may start
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        assert!(matches!(begin_attempt(&mut s, t, &k), Err(KeeperError::NotDue { .. })));
        finish_attempt(&mut s, &c, t + SEC, &ticket, AttemptOutcome::Sent { http_status: 200 }).unwrap();
        // NOT active yet: the request succeeded but no fresh evidence.
        assert!(matches!(s.accounts[&k].phase, Phase::ActivationPending { .. }));
        // Poll still shows the old reset → still pending, verify cadence.
        observe(&mut s, &c, t + 2 * SEC, &k, &ok_obs(t + 2 * SEC, 0.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::ActivationPending { .. }));
        assert_eq!(s.accounts[&k].next_action_at_ms, t + 2 * SEC + c.verify_poll_ms);
        // Provider now asserts a limit after the reset → still pending (clock runs), never due.
        observe(&mut s, &c, t + 3 * SEC, &k, &ok_obs(t + 3 * SEC, 100.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::ActivationPending { .. }));
        // fresh usage shows a strictly later reset → active, attempt-correlated (not causal)
        let anchor = t + 30 * SEC;
        let g2 = anchor + WEEK;
        let ev = observe(&mut s, &c, t + MIN, &k, &ok_obs(t + MIN, 0.0, Some(g2))).unwrap();
        let st = &s.accounts[&k];
        assert!(matches!(
            st.phase,
            Phase::Active {
                generation,
                attribution: Attribution::AttemptCorrelated { attempt: 1 },
                ..
            } if generation == g2
        ));
        assert_eq!(st.generation, Some(g2));
        assert_eq!(st.attempts_in_generation, 0);
        assert_eq!(st.last_idle_delay_ms, Some(anchor - g1));
        assert!(!st.last_idle_delay_is_upper_bound);
        assert!(ev.iter().any(|e| matches!(
            e,
            KeeperEvent::NewWindow {
                from_generation: Some(f),
                to_generation,
                attribution: Attribution::AttemptCorrelated { attempt: 1 },
                ..
            } if *f == g1 && *to_generation == g2
        )));
        let rec = st.history.last().unwrap();
        assert_eq!(rec.new_generation, Some(g2));
        assert!(rec.verified_at_ms.is_some());
        assert!(matches!(decide(&s, t + MIN, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn ambiguous_attempt_and_crash_never_burn_twice() {
        let c = cfg();
        let (mut s, k, g1, t) = make_due("a");
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        // Simulate crash: persist, reload, recover.
        let json = serde_json::to_vec(&s).unwrap();
        let mut s2: KeeperState = serde_json::from_slice(&json).unwrap();
        let ev = recover_after_restart(&mut s2, t + 5 * MIN);
        assert!(ev.iter().any(|e| matches!(
            e,
            KeeperEvent::AttemptFinished {
                outcome: AttemptOutcome::Ambiguous { .. },
                refunded: false,
                ..
            }
        )));
        assert!(matches!(
            s2.accounts[&k].phase,
            Phase::ActivationPending {
                outcome: Some(AttemptOutcome::Ambiguous { .. }),
                ..
            }
        ));
        // Duplicate restart: recovery is idempotent.
        assert!(recover_after_restart(&mut s2, t + 6 * MIN).is_empty());
        // Polls keep showing the old generation with headroom past the verify window.
        let t2 = t + c.verify_window_ms + SEC;
        let ev = observe(&mut s2, &c, t2, &k, &ok_obs(t2, 0.0, Some(g1))).unwrap();
        assert!(matches!(s2.accounts[&k].phase, Phase::Unverified { attempts: 1, .. }));
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::Unverified { .. })));
        // No further activation for this generation, ever.
        for i in 0..10u64 {
            let tt = t2 + i * c.poll_interval_ms;
            observe(&mut s2, &c, tt, &k, &ok_obs(tt, 0.0, Some(g1))).unwrap();
            assert!(matches!(decide(&s2, tt, &k, true), ActivationDecision::Skip { .. }));
            assert!(begin_attempt(&mut s2, tt, &k).is_err());
        }
        // The late-finishing original process (ticket) cannot re-arm anything.
        let ev = finish_attempt(&mut s2, &c, t2 + 1, &ticket, AttemptOutcome::Sent { http_status: 200 }).unwrap();
        assert!(matches!(s2.accounts[&k].phase, Phase::Unverified { .. }));
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::AttemptFinished { refunded: false, .. })));
        // A later external window is still recognized (and correlated).
        let g2 = t2 + DAY + WEEK;
        observe(&mut s2, &c, t2 + DAY, &k, &ok_obs(t2 + DAY, 0.0, Some(g2))).unwrap();
        assert!(matches!(
            s2.accounts[&k].phase,
            Phase::Active {
                attribution: Attribution::AttemptCorrelated { attempt: 1 },
                ..
            }
        ));
    }

    #[test]
    fn not_sent_is_refunded_with_backoff_but_bounded() {
        let c = cfg();
        let (mut s, k, g1, mut t) = make_due("a");
        for i in 0..MAX_NOT_SENT_PER_GENERATION {
            assert_eq!(
                decide(&s, t, &k, true),
                ActivationDecision::Activate {
                    generation: g1,
                    attempt: 1
                },
                "iteration {i}"
            );
            let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
            finish_attempt(
                &mut s,
                &c,
                t,
                &ticket,
                AttemptOutcome::NotSent {
                    stage: NotSentStage::TokenVend,
                    reason: "token vend failed".into(),
                },
            )
            .unwrap();
            let st = &s.accounts[&k];
            assert_eq!(st.attempts_in_generation, 0, "refunded");
            assert!(matches!(st.phase, Phase::AwaitingActivation { due_since_ms, .. } if due_since_ms == g1));
            // backoff blocks an immediate retry
            assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
            t = st.next_attempt_allowed_ms;
            observe(&mut s, &c, t, &k, &ok_obs(t, 0.0, Some(g1))).unwrap();
        }
        assert!(matches!(
            decide(&s, t, &k, true),
            ActivationDecision::Skip { reason } if reason.contains("unsent")
        ));
        assert!(s.accounts[&k].backoff_ms <= c.max_backoff_ms);
    }

    #[test]
    fn refund_whitelist_is_explicit() {
        for s in REFUNDABLE_REJECT_STATUSES {
            assert!(AttemptOutcome::Rejected { http_status: *s }.is_refundable());
        }
        for s in [402u16, 408, 409, 418, 429, 500, 502] {
            assert!(!AttemptOutcome::Rejected { http_status: s }.is_refundable(), "{s}");
        }
        assert!(AttemptOutcome::NotSent {
            stage: NotSentStage::Connect,
            reason: "x".into()
        }
        .is_refundable());
        assert!(!AttemptOutcome::Ambiguous { reason: "x".into() }.is_refundable());
        assert!(!AttemptOutcome::Sent { http_status: 200 }.is_refundable());
    }

    #[test]
    fn rearm_is_explicit_and_only_from_unverified() {
        let c = cfg();
        let (mut s, k, g1, t) = make_due("a");
        // nothing to re-arm while due
        assert_eq!(rearm(&mut s, t, &k).unwrap(), None);
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        finish_attempt(&mut s, &c, t, &ticket, AttemptOutcome::Ambiguous { reason: "timeout".into() }).unwrap();
        let t2 = t + c.verify_window_ms + SEC;
        observe(&mut s, &c, t2, &k, &ok_obs(t2, 0.0, Some(g1))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unverified { attempts: 1, .. }));
        assert!(matches!(decide(&s, t2, &k, true), ActivationDecision::Skip { .. }));
        let ev = rearm(&mut s, t2, &k).unwrap();
        assert!(matches!(ev, Some(KeeperEvent::Rearmed { previous_attempts: 1, .. })));
        assert!(matches!(s.accounts[&k].phase, Phase::AwaitingActivation { generation, due_since_ms } if generation == g1 && due_since_ms == g1));
        assert_eq!(decide(&s, t2, &k, true), ActivationDecision::Activate { generation: g1, attempt: 1 });
        // history retains the first attempt
        assert_eq!(s.accounts[&k].history.len(), 1);
        // a second rearm without a new unverified state is a no-op
        assert_eq!(rearm(&mut s, t2, &k).unwrap(), None);
    }

    #[test]
    fn rejected_401_becomes_auth_error_and_429_is_not_refunded() {
        let c = cfg();
        let (mut s, k, _g1, t) = make_due("a");
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        finish_attempt(&mut s, &c, t, &ticket, AttemptOutcome::Rejected { http_status: 401 }).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AuthError { .. }));
        assert_eq!(s.accounts[&k].attempts_in_generation, 0);

        let (mut s, k, _g1, t) = make_due("b");
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        finish_attempt(&mut s, &c, t, &ticket, AttemptOutcome::Rejected { http_status: 429 }).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::ActivationPending { .. }));
        assert_eq!(s.accounts[&k].attempts_in_generation, 1);
    }

    #[test]
    fn transport_errors_back_off_exponentially_and_recover() {
        let c = cfg();
        let (mut s, k) = fresh("a");
        let mut t = NOW;
        let mut last = 0;
        for _ in 0..8 {
            observe(&mut s, &c, t, &k, &transport(t)).unwrap();
            let b = s.accounts[&k].backoff_ms;
            assert!(b >= last && b <= c.max_backoff_ms);
            assert_eq!(s.accounts[&k].next_action_at_ms, t + b);
            last = b;
            t += b;
        }
        assert_eq!(last, c.max_backoff_ms);
        observe(&mut s, &c, t, &k, &ok_obs(t, 1.0, Some(t + DAY))).unwrap();
        assert_eq!(s.accounts[&k].backoff_ms, 0);
        assert_eq!(s.accounts[&k].consecutive_errors, 0);
    }

    #[test]
    fn auth_error_unsupported_and_malformed_are_labelled_not_activated() {
        let c = cfg();
        let (mut s, k, _g1, t) = make_due("a");
        observe(
            &mut s,
            &c,
            t,
            &k,
            &UsageObservation {
                observed_at_ms: t,
                outcome: ObservationOutcome::AuthError {
                    detail: "401".into(),
                },
            },
        )
        .unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::AuthError { .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
        observe(
            &mut s,
            &c,
            t,
            &k,
            &UsageObservation {
                observed_at_ms: t,
                outcome: ObservationOutcome::Malformed {
                    detail: "rate_limit missing".into(),
                },
            },
        )
        .unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        observe(
            &mut s,
            &c,
            t,
            &k,
            &UsageObservation {
                observed_at_ms: t,
                outcome: ObservationOutcome::Unsupported {
                    detail: "no adapter".into(),
                },
            },
        )
        .unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn external_new_window_is_observed_not_attributed() {
        let c = cfg();
        let (mut s, k) = fresh("a");
        let g1 = NOW + HOUR;
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        let t = g1 + 2 * HOUR;
        let g2 = t + WEEK - 10 * MIN; // anchored 10 min ago by someone else
        let ev = observe(&mut s, &c, t, &k, &ok_obs(t, 1.0, Some(g2))).unwrap();
        assert!(matches!(
            s.accounts[&k].phase,
            Phase::Active {
                attribution: Attribution::Observed,
                ..
            }
        ));
        assert_eq!(s.accounts[&k].last_idle_delay_ms, Some(2 * HOUR - 10 * MIN));
        assert!(ev.iter().any(|e| matches!(e, KeeperEvent::NewWindow { attribution: Attribution::Observed, .. })));
        // Reset earlier than the tracked generation → contradictory → unknown.
        let (mut s, k) = fresh("b");
        observe(&mut s, &c, NOW, &k, &ok_obs(NOW, 100.0, Some(g1))).unwrap();
        observe(&mut s, &c, t, &k, &ok_obs(t, 1.0, Some(t + WEEK))).unwrap();
        observe(&mut s, &c, t + 1, &k, &ok_obs(t + 1, 1.0, Some(t + DAY))).unwrap();
        assert!(matches!(s.accounts[&k].phase, Phase::Unknown { .. }));
    }

    #[test]
    fn banked_resets_alert_only() {
        let c = cfg();
        let (mut s, k) = fresh("a");
        let mut obs = ok_obs(NOW, 10.0, Some(NOW + DAY));
        if let ObservationOutcome::Ok { banked, .. } = &mut obs.outcome {
            *banked = Some(BankedResets {
                available_count: Some(2),
                earliest_expiry_ms: Some(NOW + DAY),
                inventory_error: None,
            });
        }
        let ev = observe(&mut s, &c, NOW, &k, &obs).unwrap();
        assert!(ev.iter().any(|e| matches!(
            e,
            KeeperEvent::BankedResets {
                available_count: 2,
                expiring_soon: true,
                ..
            }
        )));
        assert_eq!(s.accounts[&k].banked.as_ref().unwrap().available_count, Some(2));
        // zero or unreported inventory → no alert
        for count in [Some(0), None] {
            if let ObservationOutcome::Ok { banked, .. } = &mut obs.outcome {
                *banked = Some(BankedResets {
                    available_count: count,
                    earliest_expiry_ms: None,
                    inventory_error: None,
                });
            }
            let ev = observe(&mut s, &c, NOW, &k, &obs).unwrap();
            assert!(!ev.iter().any(|e| matches!(e, KeeperEvent::BankedResets { .. })));
        }
    }

    #[test]
    fn overall_limit_flag_folds_into_policy_view() {
        let w = vec![five_hour(NOW, 1.0)];
        let folded = windows_with_overall_flag(&w, Some(true));
        assert_eq!(folded[0].limit_reached, Some(true));
        let folded = windows_with_overall_flag(&[], Some(true));
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].id, "overall");
        let folded = windows_with_overall_flag(&w, Some(false));
        assert_eq!(folded, w);
    }

    #[test]
    fn prune_never_deletes_attempt_ledgers_and_scheduling_is_scoped() {
        let mut s = KeeperState::default();
        let a = ident("a");
        let mut b = ident("a");
        b.identity_fp = "id:other".into();
        s.entry(&a);
        s.entry(&b);
        s.accounts.get_mut(&a.key()).unwrap().next_action_at_ms = NOW + 10;
        s.accounts.get_mut(&b.key()).unwrap().next_action_at_ms = NOW + 5;
        assert_eq!(s.next_wake_ms(), Some(NOW + 5));
        assert_eq!(s.due(NOW + 5), vec![b.key()]);
        assert_eq!(s.next_wake_among(&[a.key()]), Some(NOW + 10));
        assert!(s.due_among(NOW + 5, &[a.key()]).is_empty());
        // Not kept, no ledger → prunable only after the idle window.
        s.accounts.get_mut(&a.key()).unwrap().last_poll_ms = Some(NOW);
        assert!(s.prune_idle(&[b.clone()], NOW + DAY, 30 * DAY).is_empty());
        assert_eq!(s.prune_idle(&[b.clone()], NOW + 31 * DAY, 30 * DAY), vec![a.key()]);
        // An excluded account with a spent attempt is never pruned.
        let (mut s, k, _g, t) = make_due("c");
        let (ticket, _) = begin_attempt(&mut s, t, &k).unwrap();
        finish_attempt(&mut s, &cfg(), t, &ticket, AttemptOutcome::Sent { http_status: 200 }).unwrap();
        assert!(s.prune_idle(&[], t + 365 * DAY, 30 * DAY).is_empty());
        assert_eq!(s.accounts[&k].attempts_in_generation, 1);
        // Excluded then restored: the spent generation is still spent.
        s.entry(&ident("c"));
        assert!(matches!(decide(&s, t, &k, true), ActivationDecision::Skip { .. }));
    }

    #[test]
    fn state_store_is_private_atomic_and_fails_closed_on_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(&dir.path().join("keeper"));
        assert_eq!(store.load().unwrap(), KeeperState::default());
        let (mut s, k) = fresh("a");
        observe(&mut s, &cfg(), NOW, &k, &ok_obs(NOW, 1.0, Some(NOW + DAY))).unwrap();
        store.save(&s).unwrap();
        let back = store.load().unwrap();
        assert_eq!(back.accounts[&k].phase, s.accounts[&k].phase);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fm = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
            let dm = std::fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
            assert_eq!(fm, 0o600);
            assert_eq!(dm, 0o700);
        }
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("Bearer") && !raw.contains("refresh"));
        std::fs::write(store.path(), b"{ not json").unwrap();
        assert!(matches!(store.load(), Err(KeeperError::CorruptState { .. })));
        std::fs::write(store.path(), b"{\"version\": 99, \"accounts\": {}}").unwrap();
        assert!(matches!(store.load(), Err(KeeperError::UnsupportedVersion { .. })));
    }

    #[test]
    fn locks_are_exclusive_per_name_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = KeeperLock::acquire(dir.path(), SINGLETON_LOCK_NAME, "pid 1").unwrap();
        match KeeperLock::acquire(dir.path(), SINGLETON_LOCK_NAME, "pid 2") {
            Err(KeeperError::Locked { holder, .. }) => assert_eq!(holder.as_deref(), Some("pid 1")),
            other => panic!("expected lock conflict, got {other:?}"),
        }
        let name = account_lock_name("openai-codex@astra2");
        assert_eq!(name, "openai-codex@astra2.lock");
        assert_eq!(account_lock_name("weird/../key"), "weird_.._key.lock");
        let _l2 = KeeperLock::acquire(dir.path(), &name, "pid 1").unwrap();
        drop(l1);
        let _l3 = KeeperLock::acquire(dir.path(), SINGLETON_LOCK_NAME, "pid 3").unwrap();
    }
}
