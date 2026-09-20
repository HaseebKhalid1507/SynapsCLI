//! G5 — pure capacity-selection policy for multi-account OAuth providers.
//!
//! This module has **no I/O, no clock and no network**. Every decision is a
//! deterministic function of `now_ms` and the caller-supplied per-account
//! capacity views ([`AccountCapacity`]), so it is trivially testable with a
//! synthetic clock and can be shared by the broker (`AccountSelector::Auto`),
//! the Codex stream failover and the quota keeper.
//!
//! ## Invariants
//!
//! * **Fail closed.** Stale, unknown, malformed, unsupported or auth-errored
//!   quota is *not* capacity. An account is only advertised as ready when a
//!   fresh observation shows headroom on every applicable limit.
//! * **All applicable limits count.** Every window that applies to the
//!   requested model (account-wide windows plus model-scoped windows naming
//!   that model) must have headroom. A window with no usable evidence makes
//!   the account ineligible.
//! * **Exclusions are absolute.** Cooling-down, exhausted, excluded and
//!   auth-errored accounts are never selected.
//! * **Explicit never falls back.** When a specific account is requested only
//!   that account is considered; it is selected or the request fails.
//! * **Deterministic preference.** Ties are broken by the operator's
//!   preference order, then by storage key, so equal inputs always yield the
//!   same seat.
//! * **One bounded failover.** [`failover`] permits a single switch to a
//!   different eligible seat, only on a recognized pre-output quota failure,
//!   never after any output or tool activity and never on an ordinary 429.

use serde::{Deserialize, Serialize};

use super::account::CredentialRef;
use super::provider::OAuthProviderId;

/// Serde shim: [`CredentialRef`] round-trips as its storage key
/// (`"openai-codex@astra2"`), which is already validated text.
pub mod cred_serde {
    use super::CredentialRef;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(c: &CredentialRef, s: S) -> Result<S::Ok, S::Error> {
        c.storage_key().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CredentialRef, D::Error> {
        let raw = String::deserialize(d)?;
        CredentialRef::parse_storage_key(&raw)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid credential key '{raw}'")))
    }
}

/// Lower bound (inclusive) for classifying a window as "weekly" from its
/// provider-reported duration. Six days leaves room for providers that report
/// approximate durations; never infer weekliness from a name like `primary`.
pub const WEEKLY_WINDOW_MIN_MS: u64 = 6 * 24 * 60 * 60 * 1000;
/// Upper bound (inclusive) for the weekly classification.
pub const WEEKLY_WINDOW_MAX_MS: u64 = 8 * 24 * 60 * 60 * 1000;
/// Observation timestamps further in the future than this are malformed
/// (a small allowance for clock skew between the provider and this host).
pub const MAX_FUTURE_OBSERVATION_SKEW_MS: u64 = 5 * 60 * 1000;
/// Default utilization percentage at/above which a window is exhausted.
pub const DEFAULT_EXHAUSTED_AT_PERCENT: f64 = 100.0;

// ── Inputs ───────────────────────────────────────────────────────────────────

/// One provider-reported rate-limit window, normalized by the usage adapter.
///
/// `models == None` means the window applies to every model on the account;
/// `Some(list)` scopes it to exactly those model ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowLimit {
    /// Provider-side identifier (e.g. `primary`, `seven_day_sonnet`). Display only.
    pub id: String,
    /// Window length as reported by the provider, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Utilization in percent (`0..=100`), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Provider's explicit "limit reached" flag, if reported. Adapters must
    /// set `Some(false)` ONLY when the provider asserts it (it counts as
    /// headroom evidence when no percentage is available); an absent flag
    /// must be `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    /// Provider-reported reset instant (epoch ms), if reported. Authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    /// Model scope. `None` = account-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

/// Fail-closed verdict for one window.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowVerdict {
    /// Evidence of remaining capacity.
    Headroom { used_percent: Option<f64> },
    /// Evidence the window is used up (or reports contradictory/past data).
    Exhausted { resets_at_ms: Option<u64> },
    /// No usable evidence (missing or malformed fields).
    Unknown,
}

/// Whether a provider availability/window entry (`sonnet`, `gpt-6-astra`)
/// names the requested model id (`claude-sonnet-4-6`). Exact
/// (case-insensitive) match, or the entry equals one `-`/`_`/`/`/`:`/`.`
/// separated token of the request — the same rule the usage adapter uses.
pub fn model_matches(entry: &str, requested: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    let req = requested.trim().to_ascii_lowercase();
    if entry.is_empty() || req.is_empty() {
        return false;
    }
    if entry == req {
        return true;
    }
    req.split(['-', '_', '/', ':', '.']).any(|tok| tok == entry)
}

impl WindowLimit {
    /// Whether this window constrains `model` (`None` = any model).
    pub fn applies_to(&self, model: Option<&str>) -> bool {
        match (&self.models, model) {
            (None, _) => true,
            // A model-scoped window cannot be matched without a model; treat
            // it as applicable so an unknown model never bypasses a limit.
            (Some(_), None) => true,
            (Some(list), Some(m)) => list.iter().any(|x| model_matches(x, m)),
        }
    }

    /// Weekly classification from the reported duration only.
    pub fn is_weekly(&self) -> bool {
        matches!(self.duration_ms, Some(d) if (WEEKLY_WINDOW_MIN_MS..=WEEKLY_WINDOW_MAX_MS).contains(&d))
    }

    /// Capacity verdict. Exhaustion threshold is inclusive. A provider
    /// `limit_reached: true` flag always wins; malformed percentages are
    /// unknown (never capacity). An invalid threshold (non-finite or outside
    /// `0..=100`) fails closed: the verdict is `Unknown`.
    pub fn verdict(&self, exhausted_at_percent: f64) -> WindowVerdict {
        if !valid_threshold(exhausted_at_percent) {
            return WindowVerdict::Unknown;
        }
        let used = match self.used_percent {
            Some(p) if !p.is_finite() || !(0.0..=100.0).contains(&p) => return WindowVerdict::Unknown,
            other => other,
        };
        if self.limit_reached == Some(true) {
            return WindowVerdict::Exhausted {
                resets_at_ms: self.resets_at_ms,
            };
        }
        match used {
            Some(p) if p >= exhausted_at_percent => WindowVerdict::Exhausted {
                resets_at_ms: self.resets_at_ms,
            },
            Some(p) => WindowVerdict::Headroom {
                used_percent: Some(p),
            },
            None if self.limit_reached == Some(false) => WindowVerdict::Headroom { used_percent: None },
            None => WindowVerdict::Unknown,
        }
    }
}

/// Whether an exhaustion threshold is usable (finite, `0..=100`).
pub fn valid_threshold(percent: f64) -> bool {
    percent.is_finite() && (0.0..=100.0).contains(&percent)
}

/// Tri-state model availability. `Unknown` is preserved as unknown — it is
/// never coerced to "available".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Available,
    Exhausted,
    Unknown,
}

/// Provider-reported model availability for an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAvailability {
    pub model: String,
    pub state: ModelState,
}

/// What the usage adapter learned about one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuotaObservation {
    /// A well-formed reading.
    Ok {
        windows: Vec<WindowLimit>,
        /// `None` = the provider exposes no per-model availability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        models: Option<Vec<ModelAvailability>>,
    },
    /// Credential rejected (401/403) — never capacity, needs re-login.
    AuthError,
    /// Provider has no usage endpoint / adapter — unknown, never capacity.
    Unsupported,
    /// Response could not be parsed — never capacity.
    Malformed,
    /// No reading available (transport failure, never fetched, …).
    Unknown,
}

/// Per-account capacity view supplied by the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacity {
    #[serde(with = "cred_serde")]
    pub credential: CredentialRef,
    /// Snapshot time (epoch ms). `None` = never observed → not capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
    pub observation: QuotaObservation,
    /// Client-side cooldown (e.g. after a provider limit response). Epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ms: Option<u64>,
}

impl AccountCapacity {
    /// Convenience constructor for an account with no observation yet.
    pub fn unobserved(credential: CredentialRef) -> Self {
        Self {
            credential,
            observed_at_ms: None,
            observation: QuotaObservation::Unknown,
            cooldown_until_ms: None,
        }
    }
}

/// Ordering strategy among eligible accounts. Both are deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// First eligible account in the operator's preference list; unlisted
    /// accounts follow in storage-key order.
    PreferenceOrder,
    /// Lowest maximum utilization across applicable windows; ties broken by
    /// preference order, then storage key. Accounts whose utilization is
    /// unknown (but proven not limit-reached) sort after known ones.
    LowestUtilization,
}

/// A selection request. All times are epoch milliseconds.
#[derive(Debug, Clone)]
pub struct SelectionRequest<'a> {
    pub provider: OAuthProviderId,
    /// Model the request is for; `None` = any (model-scoped windows still apply).
    pub model: Option<&'a str>,
    pub now_ms: u64,
    /// Observations older than this are stale (not capacity).
    pub max_snapshot_age_ms: u64,
    /// Operator preference (labels, `default` for the bare slot).
    pub preference: &'a [String],
    /// Explicit account label; when set no other account is considered.
    pub explicit_account: Option<&'a str>,
    /// Accounts that must not be selected (e.g. a seat that just failed).
    pub exclude: &'a [CredentialRef],
    pub strategy: Strategy,
    /// Utilization percent at/above which a window counts as exhausted.
    pub exhausted_at_percent: f64,
}

impl<'a> SelectionRequest<'a> {
    /// Minimal request with defaults: any model, no preference/exclusions,
    /// lowest-utilization strategy, 100 % exhaustion threshold.
    pub fn new(provider: OAuthProviderId, now_ms: u64, max_snapshot_age_ms: u64) -> Self {
        Self {
            provider,
            model: None,
            now_ms,
            max_snapshot_age_ms,
            preference: &[],
            explicit_account: None,
            exclude: &[],
            strategy: Strategy::LowestUtilization,
            exhausted_at_percent: DEFAULT_EXHAUSTED_AT_PERCENT,
        }
    }
}

// ── Outputs ──────────────────────────────────────────────────────────────────

/// Why an account was not selected. Secret-free, display-ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RejectReason {
    /// Snapshot missing or older than the allowed age.
    Stale {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        age_ms: Option<u64>,
    },
    /// Observation timestamp is implausibly in the future or fields invalid.
    Malformed,
    Unknown,
    Unsupported,
    AuthError,
    CoolingDown {
        until_ms: u64,
    },
    Exhausted {
        window: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at_ms: Option<u64>,
    },
    /// An applicable window carries no usable evidence.
    WindowUnknown {
        window: String,
    },
    /// The reading has no applicable window at all — no limit evidence.
    NoLimitEvidence,
    ModelUnavailable {
        model: String,
    },
    /// The provider lists the model with unknown availability — not capacity.
    ModelUnknown {
        model: String,
    },
    /// Present in `exclude`.
    Excluded,
    /// The explicitly requested account is not among the candidates.
    NotPresent,
    /// The explicitly requested label fails the label grammar.
    InvalidLabel {
        label: String,
    },
    /// Eligible, but another eligible account ranked higher.
    Outranked,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale { age_ms: Some(a) } => write!(f, "stale snapshot ({}s old)", a / 1000),
            Self::Stale { age_ms: None } => write!(f, "no snapshot"),
            Self::Malformed => write!(f, "malformed snapshot"),
            Self::Unknown => write!(f, "quota unknown"),
            Self::Unsupported => write!(f, "usage unsupported"),
            Self::AuthError => write!(f, "auth error"),
            Self::CoolingDown { until_ms } => write!(f, "cooling down until {until_ms}"),
            Self::Exhausted {
                window,
                resets_at_ms: Some(r),
            } => write!(f, "window '{window}' exhausted, resets at {r}"),
            Self::Exhausted { window, .. } => write!(f, "window '{window}' exhausted"),
            Self::WindowUnknown { window } => write!(f, "window '{window}' has no evidence"),
            Self::NoLimitEvidence => write!(f, "no applicable limit evidence"),
            Self::ModelUnavailable { model } => write!(f, "model '{model}' unavailable"),
            Self::ModelUnknown { model } => write!(f, "model '{model}' availability unknown"),
            Self::Excluded => write!(f, "excluded"),
            Self::NotPresent => write!(f, "account not present"),
            Self::InvalidLabel { label } => write!(f, "invalid account label '{label}'"),
            Self::Outranked => write!(f, "eligible; outranked"),
        }
    }
}

/// One rejected candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    #[serde(with = "cred_serde")]
    pub credential: CredentialRef,
    pub reason: RejectReason,
}

/// Result of [`select`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Selection {
    Selected {
        #[serde(with = "cred_serde")]
        credential: CredentialRef,
        /// Maximum utilization across the applicable windows, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utilization: Option<f64>,
        /// Candidates that lost — informational.
        rejections: Vec<Rejection>,
    },
    NoCapacity {
        rejections: Vec<Rejection>,
        /// Earliest *future* provider-reported reset among exhausted
        /// candidates, if any. Past resets are never reported here: a passed
        /// reset is not evidence of a new window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        earliest_reset_ms: Option<u64>,
    },
}

impl Selection {
    pub fn selected(&self) -> Option<&CredentialRef> {
        match self {
            Self::Selected { credential, .. } => Some(credential),
            Self::NoCapacity { .. } => None,
        }
    }
}

// ── Eligibility ──────────────────────────────────────────────────────────────

/// Eligible-account summary produced by [`evaluate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Eligible {
    /// Max utilization over applicable windows (`None` = proven-not-limited only).
    pub utilization: Option<f64>,
}

/// Evaluate a single account against the request. Pure; ignores
/// `explicit_account` and provider matching (the caller filters those).
pub fn evaluate(req: &SelectionRequest<'_>, cap: &AccountCapacity) -> Result<Eligible, RejectReason> {
    if req.exclude.iter().any(|c| c.storage_key() == cap.credential.storage_key()) {
        return Err(RejectReason::Excluded);
    }
    if let Some(until) = cap.cooldown_until_ms {
        if until > req.now_ms {
            return Err(RejectReason::CoolingDown { until_ms: until });
        }
    }
    let (windows, models) = match &cap.observation {
        QuotaObservation::Ok { windows, models } => (windows, models),
        QuotaObservation::AuthError => return Err(RejectReason::AuthError),
        QuotaObservation::Unsupported => return Err(RejectReason::Unsupported),
        QuotaObservation::Malformed => return Err(RejectReason::Malformed),
        QuotaObservation::Unknown => return Err(RejectReason::Unknown),
    };
    let observed = cap
        .observed_at_ms
        .ok_or(RejectReason::Stale { age_ms: None })?;
    if observed > req.now_ms.saturating_add(MAX_FUTURE_OBSERVATION_SKEW_MS) {
        return Err(RejectReason::Malformed);
    }
    let age = req.now_ms.saturating_sub(observed);
    if age > req.max_snapshot_age_ms {
        return Err(RejectReason::Stale { age_ms: Some(age) });
    }
    if let (Some(model), Some(list)) = (req.model, models) {
        for m in list.iter().filter(|m| model_matches(&m.model, model)) {
            match m.state {
                ModelState::Available => {}
                ModelState::Exhausted => {
                    return Err(RejectReason::ModelUnavailable {
                        model: model.to_string(),
                    })
                }
                ModelState::Unknown => {
                    return Err(RejectReason::ModelUnknown {
                        model: model.to_string(),
                    })
                }
            }
        }
    }
    let mut applicable = 0usize;
    let mut utilization: Option<f64> = None;
    for w in windows.iter().filter(|w| w.applies_to(req.model)) {
        applicable += 1;
        match w.verdict(req.exhausted_at_percent) {
            WindowVerdict::Headroom { used_percent } => {
                if let Some(p) = used_percent {
                    utilization = Some(utilization.map_or(p, |u| u.max(p)));
                }
            }
            WindowVerdict::Exhausted { resets_at_ms } => {
                return Err(RejectReason::Exhausted {
                    window: w.id.clone(),
                    resets_at_ms,
                });
            }
            WindowVerdict::Unknown => {
                return Err(RejectReason::WindowUnknown {
                    window: w.id.clone(),
                });
            }
        }
    }
    if applicable == 0 {
        return Err(RejectReason::NoLimitEvidence);
    }
    Ok(Eligible { utilization })
}

fn preference_rank(req: &SelectionRequest<'_>, cred: &CredentialRef) -> usize {
    let label = cred.account.label_str();
    req.preference
        .iter()
        .position(|p| p == label)
        .unwrap_or(usize::MAX)
}

/// Pick the best eligible account for `req` among `candidates`.
///
/// Candidates for other providers are ignored. See module docs for the rules.
pub fn select(req: &SelectionRequest<'_>, candidates: &[AccountCapacity]) -> Selection {
    let mut rejections: Vec<Rejection> = Vec::new();
    let mut eligible: Vec<(&AccountCapacity, Eligible)> = Vec::new();
    let mut explicit_seen = false;

    for cap in candidates
        .iter()
        .filter(|c| c.credential.provider == req.provider)
    {
        if let Some(label) = req.explicit_account {
            if cap.credential.account.label_str() != label {
                continue;
            }
            explicit_seen = true;
        }
        match evaluate(req, cap) {
            Ok(e) => eligible.push((cap, e)),
            Err(reason) => rejections.push(Rejection {
                credential: cap.credential.clone(),
                reason,
            }),
        }
    }

    if let (Some(label), false) = (req.explicit_account, explicit_seen) {
        // An unparseable label cannot be represented as a credential; report
        // it distinctly (against the provider's default slot) so callers never
        // mistake it for a missing valid account.
        let (credential, reason) = match super::account::Account::parse(label) {
            Ok(account) => (
                CredentialRef::new(req.provider, account),
                RejectReason::NotPresent,
            ),
            Err(_) => (
                CredentialRef::default_for(req.provider),
                RejectReason::InvalidLabel {
                    label: label.to_string(),
                },
            ),
        };
        return Selection::NoCapacity {
            rejections: vec![Rejection { credential, reason }],
            earliest_reset_ms: None,
        };
    }

    // Deterministic ordering.
    eligible.sort_by(|(a, ea), (b, eb)| {
        let ka = a.credential.storage_key();
        let kb = b.credential.storage_key();
        let pa = preference_rank(req, &a.credential);
        let pb = preference_rank(req, &b.credential);
        match req.strategy {
            Strategy::PreferenceOrder => pa.cmp(&pb).then_with(|| ka.cmp(&kb)),
            Strategy::LowestUtilization => {
                let ua = ea.utilization;
                let ub = eb.utilization;
                ua.is_none()
                    .cmp(&ub.is_none())
                    .then_with(|| {
                        ua.unwrap_or(0.0)
                            .partial_cmp(&ub.unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| pa.cmp(&pb))
                    .then_with(|| ka.cmp(&kb))
            }
        }
    });

    match eligible.first() {
        Some((cap, e)) => {
            let winner = cap.credential.clone();
            for (other, _) in eligible.iter().skip(1) {
                rejections.push(Rejection {
                    credential: other.credential.clone(),
                    reason: RejectReason::Outranked,
                });
            }
            Selection::Selected {
                credential: winner,
                utilization: e.utilization,
                rejections,
            }
        }
        None => {
            let earliest_reset_ms = rejections
                .iter()
                .filter_map(|r| match &r.reason {
                    RejectReason::Exhausted {
                        resets_at_ms: Some(t),
                        ..
                    } if *t > req.now_ms => Some(*t),
                    _ => None,
                })
                .min();
            Selection::NoCapacity {
                rejections,
                earliest_reset_ms,
            }
        }
    }
}

// ── Bounded failover ─────────────────────────────────────────────────────────

/// Classification of the failure that triggered a failover request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Provider explicitly reported the account's quota as exhausted before
    /// any output was produced (e.g. Codex `usage_limit_reached`).
    QuotaExhaustedPreOutput,
    /// Ordinary rate limiting (429 / overloaded). Not proof of exhaustion.
    RateLimited,
    /// Anything else (network, 5xx, parse, cancel).
    Other,
}

/// Evidence about the failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEvidence {
    pub kind: FailureKind,
    /// Any model output (text/thinking/tool_use) reached the caller.
    pub output_started: bool,
    /// Any tool executed, or tool results were fed back, for this turn.
    pub tool_activity: bool,
    /// Failovers already performed for this request.
    pub failovers_so_far: u32,
}

/// Maximum number of account switches for one request.
pub const MAX_FAILOVERS: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum FailoverDecision {
    Failover {
        #[serde(with = "cred_serde")]
        credential: CredentialRef,
    },
    NoFailover { reason: String },
}

/// Decide whether the request may be retried on another seat.
///
/// Never replays after output or tool activity, never on an ordinary 429,
/// never for an explicitly requested account, and at most [`MAX_FAILOVERS`]
/// times. The failed seat is excluded from the fresh selection.
pub fn failover(
    req: &SelectionRequest<'_>,
    failed: &CredentialRef,
    evidence: &FailureEvidence,
    candidates: &[AccountCapacity],
) -> FailoverDecision {
    let no = |reason: &str| FailoverDecision::NoFailover {
        reason: reason.to_string(),
    };
    if evidence.output_started {
        return no("output already started; never replay on another account");
    }
    if evidence.tool_activity {
        return no("tool activity occurred; never replay on another account");
    }
    if evidence.failovers_so_far >= MAX_FAILOVERS {
        return no("failover budget exhausted");
    }
    if evidence.kind != FailureKind::QuotaExhaustedPreOutput {
        return no("not a recognized pre-output quota failure");
    }
    if req.explicit_account.is_some() {
        return no("explicit account requested; no silent fallback");
    }
    let mut exclude: Vec<CredentialRef> = req.exclude.to_vec();
    exclude.push(failed.clone());
    let retry = SelectionRequest {
        exclude: &exclude,
        ..req.clone()
    };
    match select(&retry, candidates) {
        Selection::Selected { credential, .. } => FailoverDecision::Failover { credential },
        Selection::NoCapacity { .. } => no("no other eligible account"),
    }
}

// ── Helpers shared with the keeper ───────────────────────────────────────────

/// The account-wide weekly window of a reading, if the provider reports one.
///
/// Prefers account-wide (`models == None`) weekly windows; falls back to a
/// model-scoped weekly window only when no account-wide one exists. Order is
/// deterministic (first by scope, then by `id`).
pub fn weekly_window(windows: &[WindowLimit]) -> Option<&WindowLimit> {
    let mut weekly: Vec<&WindowLimit> = windows.iter().filter(|w| w.is_weekly()).collect();
    weekly.sort_by(|a, b| {
        a.models
            .is_some()
            .cmp(&b.models.is_some())
            .then_with(|| a.id.cmp(&b.id))
    });
    weekly.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::account::Account;

    const HOUR: u64 = 3_600_000;
    const DAY: u64 = 24 * HOUR;
    const NOW: u64 = 1_800_000_000_000;

    fn cred(label: &str) -> CredentialRef {
        CredentialRef::new(OAuthProviderId::OpenAiCodex, Account::parse(label).unwrap())
    }

    fn win(id: &str, dur: u64, used: f64, resets: u64) -> WindowLimit {
        WindowLimit {
            id: id.into(),
            duration_ms: Some(dur),
            used_percent: Some(used),
            limit_reached: Some(used >= 100.0),
            resets_at_ms: Some(resets),
            models: None,
        }
    }

    fn ok(label: &str, windows: Vec<WindowLimit>) -> AccountCapacity {
        AccountCapacity {
            credential: cred(label),
            observed_at_ms: Some(NOW - 1000),
            observation: QuotaObservation::Ok {
                windows,
                models: None,
            },
            cooldown_until_ms: None,
        }
    }

    fn req() -> SelectionRequest<'static> {
        SelectionRequest::new(OAuthProviderId::OpenAiCodex, NOW, 30 * 60 * 1000)
    }

    #[test]
    fn window_verdicts_fail_closed() {
        let mut w = win("primary", 7 * DAY, 20.0, NOW + DAY);
        assert_eq!(
            w.verdict(100.0),
            WindowVerdict::Headroom {
                used_percent: Some(20.0)
            }
        );
        w.used_percent = Some(100.0);
        w.limit_reached = Some(false);
        assert!(matches!(w.verdict(100.0), WindowVerdict::Exhausted { .. }));
        w.used_percent = Some(50.0);
        w.limit_reached = Some(true);
        assert!(matches!(w.verdict(100.0), WindowVerdict::Exhausted { .. }));
        w.used_percent = None;
        w.limit_reached = None;
        assert_eq!(w.verdict(100.0), WindowVerdict::Unknown);
        w.limit_reached = Some(false);
        assert_eq!(w.verdict(100.0), WindowVerdict::Headroom { used_percent: None });
        w.used_percent = Some(f64::NAN);
        assert_eq!(w.verdict(100.0), WindowVerdict::Unknown);
        w.used_percent = Some(140.0);
        assert_eq!(w.verdict(100.0), WindowVerdict::Unknown);
        w.used_percent = Some(-1.0);
        assert_eq!(w.verdict(100.0), WindowVerdict::Unknown);
        // invalid thresholds fail closed
        w.used_percent = Some(1.0);
        assert_eq!(w.verdict(f64::NAN), WindowVerdict::Unknown);
        assert_eq!(w.verdict(101.0), WindowVerdict::Unknown);
        assert_eq!(w.verdict(-0.5), WindowVerdict::Unknown);
        assert_eq!(w.verdict(f64::INFINITY), WindowVerdict::Unknown);
        let r = SelectionRequest {
            exhausted_at_percent: f64::NAN,
            ..SelectionRequest::new(OAuthProviderId::OpenAiCodex, NOW, HOUR)
        };
        assert!(select(&r, &[ok("a", vec![win("primary", 7 * DAY, 1.0, NOW + DAY)])])
            .selected()
            .is_none());
    }

    #[test]
    fn weekly_is_from_duration_not_name() {
        let primary_5h = win("primary", 5 * HOUR, 0.0, NOW + HOUR);
        let primary_week = win("primary", 7 * DAY, 0.0, NOW + DAY);
        let no_dur = WindowLimit {
            duration_ms: None,
            ..win("seven_day", 0, 0.0, NOW)
        };
        assert!(!primary_5h.is_weekly());
        assert!(primary_week.is_weekly());
        assert!(!no_dur.is_weekly());
        assert_eq!(
            weekly_window(&[primary_5h.clone(), primary_week.clone()]).map(|w| w.duration_ms),
            Some(Some(7 * DAY))
        );
        assert!(weekly_window(&[primary_5h, no_dur]).is_none());
        // account-wide weekly preferred over model-scoped weekly
        let scoped = WindowLimit {
            id: "aaa_scoped".into(),
            models: Some(vec!["m".into()]),
            ..primary_week.clone()
        };
        assert_eq!(
            weekly_window(&[scoped, primary_week]).map(|w| w.id.as_str()),
            Some("primary")
        );
    }

    #[test]
    fn selects_lowest_utilization_deterministically() {
        let a = ok("a", vec![win("primary", 7 * DAY, 60.0, NOW + DAY)]);
        let b = ok("b", vec![win("primary", 7 * DAY, 10.0, NOW + DAY)]);
        let c = ok("c", vec![win("primary", 7 * DAY, 10.0, NOW + DAY)]);
        let sel = select(&req(), &[a.clone(), c.clone(), b.clone()]);
        assert_eq!(sel.selected().unwrap().storage_key(), "openai-codex@b");
        // identical inputs in another order → same answer
        let sel2 = select(&req(), &[c, b, a]);
        assert_eq!(sel2.selected().unwrap().storage_key(), "openai-codex@b");
    }

    #[test]
    fn preference_breaks_ties_and_orders() {
        let b = ok("b", vec![win("primary", 7 * DAY, 10.0, NOW + DAY)]);
        let c = ok("c", vec![win("primary", 7 * DAY, 10.0, NOW + DAY)]);
        let pref = vec!["c".to_string(), "b".to_string()];
        let r = SelectionRequest {
            preference: &pref,
            ..req()
        };
        assert_eq!(
            select(&r, &[b.clone(), c.clone()]).selected().unwrap().storage_key(),
            "openai-codex@c"
        );
        let a = ok("a", vec![win("primary", 7 * DAY, 90.0, NOW + DAY)]);
        let pref2 = vec!["a".to_string()];
        let r2 = SelectionRequest {
            preference: &pref2,
            strategy: Strategy::PreferenceOrder,
            ..req()
        };
        assert_eq!(
            select(&r2, &[b, c, a]).selected().unwrap().storage_key(),
            "openai-codex@a"
        );
    }

    #[test]
    fn stale_unknown_malformed_auth_are_not_capacity() {
        let mut stale = ok("s", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        stale.observed_at_ms = Some(NOW - 2 * HOUR);
        let mut never = ok("n", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        never.observed_at_ms = None;
        let mut future = ok("f", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        future.observed_at_ms = Some(NOW + HOUR);
        let mut auth = ok("x", vec![]);
        auth.observation = QuotaObservation::AuthError;
        let mut unk = ok("u", vec![]);
        unk.observation = QuotaObservation::Unknown;
        let mut mal = ok("m", vec![]);
        mal.observation = QuotaObservation::Malformed;
        let mut uns = ok("v", vec![]);
        uns.observation = QuotaObservation::Unsupported;
        let none = ok("e", vec![]);
        let sel = select(&req(), &[stale, never, future, auth, unk, mal, uns, none]);
        let Selection::NoCapacity {
            rejections,
            earliest_reset_ms,
        } = sel
        else {
            panic!("expected no capacity");
        };
        assert_eq!(earliest_reset_ms, None);
        let reasons: Vec<(String, RejectReason)> = rejections
            .into_iter()
            .map(|r| (r.credential.account.label_str().to_string(), r.reason))
            .collect();
        assert!(reasons.contains(&(
            "s".into(),
            RejectReason::Stale {
                age_ms: Some(2 * HOUR)
            }
        )));
        assert!(reasons.contains(&("n".into(), RejectReason::Stale { age_ms: None })));
        assert!(reasons.contains(&("f".into(), RejectReason::Malformed)));
        assert!(reasons.contains(&("x".into(), RejectReason::AuthError)));
        assert!(reasons.contains(&("u".into(), RejectReason::Unknown)));
        assert!(reasons.contains(&("m".into(), RejectReason::Malformed)));
        assert!(reasons.contains(&("v".into(), RejectReason::Unsupported)));
        assert!(reasons.contains(&("e".into(), RejectReason::NoLimitEvidence)));
    }

    #[test]
    fn exhausted_cooldown_and_unknown_window_excluded() {
        let exhausted = ok("x", vec![win("primary", 7 * DAY, 100.0, NOW + 2 * DAY)]);
        let mut cooling = ok("c", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        cooling.cooldown_until_ms = Some(NOW + HOUR);
        let mut cooled = ok("d", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        cooled.cooldown_until_ms = Some(NOW - 1);
        let unknown_window = ok(
            "w",
            vec![WindowLimit {
                used_percent: None,
                limit_reached: None,
                ..win("primary", 7 * DAY, 0.0, NOW + DAY)
            }],
        );
        let past_exhausted = ok("p", vec![win("primary", 7 * DAY, 100.0, NOW - DAY)]);
        let sel = select(&req(), &[exhausted, cooling, unknown_window, past_exhausted]);
        let Selection::NoCapacity {
            earliest_reset_ms, ..
        } = &sel
        else {
            panic!("no capacity expected");
        };
        // past reset never reported as the earliest reset
        assert_eq!(*earliest_reset_ms, Some(NOW + 2 * DAY));
        let sel = select(&req(), &[cooled]);
        assert_eq!(sel.selected().unwrap().storage_key(), "openai-codex@d");
    }

    #[test]
    fn all_applicable_limits_including_model_scoped_count() {
        let model_win = WindowLimit {
            id: "weekly_sonnet".into(),
            models: Some(vec!["sonnet".into()]),
            ..win("weekly_sonnet", 7 * DAY, 100.0, NOW + DAY)
        };
        let five_h = win("five_hour", 5 * HOUR, 99.0, NOW + HOUR);
        let acct = ok("a", vec![win("seven_day", 7 * DAY, 10.0, NOW + DAY), five_h, model_win]);
        // sonnet request hits the exhausted model window
        let r = SelectionRequest {
            model: Some("sonnet"),
            ..req()
        };
        assert!(select(&r, std::slice::from_ref(&acct)).selected().is_none());
        // opus request ignores the sonnet window and takes max utilization (99)
        let r = SelectionRequest {
            model: Some("opus"),
            ..req()
        };
        match select(&r, std::slice::from_ref(&acct)) {
            Selection::Selected { utilization, .. } => assert_eq!(utilization, Some(99.0)),
            other => panic!("unexpected {other:?}"),
        }
        // no model given: scoped windows still apply (never bypass a limit)
        assert!(select(&req(), std::slice::from_ref(&acct)).selected().is_none());
        // model listed as unavailable
        let mut unavailable = acct;
        if let QuotaObservation::Ok { models, .. } = &mut unavailable.observation {
            *models = Some(vec![ModelAvailability {
                model: "opus".into(),
                state: ModelState::Exhausted,
            }]);
        }
        let r = SelectionRequest {
            model: Some("opus"),
            ..req()
        };
        let sel = select(&r, &[unavailable.clone()]);
        match sel {
            Selection::NoCapacity { rejections, .. } => assert_eq!(
                rejections[0].reason,
                RejectReason::ModelUnavailable {
                    model: "opus".into()
                }
            ),
            other => panic!("unexpected {other:?}"),
        }
        // unknown availability is preserved as unknown → not capacity
        let mut unknown = unavailable;
        if let QuotaObservation::Ok { models, .. } = &mut unknown.observation {
            *models = Some(vec![ModelAvailability {
                model: "opus".into(),
                state: ModelState::Unknown,
            }]);
        }
        match select(&r, &[unknown]) {
            Selection::NoCapacity { rejections, .. } => assert_eq!(
                rejections[0].reason,
                RejectReason::ModelUnknown {
                    model: "opus".into()
                }
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn explicit_account_never_falls_back() {
        let a = ok("a", vec![win("primary", 7 * DAY, 100.0, NOW + DAY)]);
        let b = ok("b", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        let r = SelectionRequest {
            explicit_account: Some("a"),
            ..req()
        };
        let sel = select(&r, &[a.clone(), b.clone()]);
        assert!(sel.selected().is_none());
        let r = SelectionRequest {
            explicit_account: Some("zzz"),
            ..req()
        };
        match select(&r, &[a.clone(), b.clone()]) {
            Selection::NoCapacity { rejections, .. } => {
                assert_eq!(rejections.len(), 1);
                assert_eq!(rejections[0].reason, RejectReason::NotPresent);
                assert_eq!(rejections[0].credential.storage_key(), "openai-codex@zzz");
            }
            other => panic!("unexpected {other:?}"),
        }
        let r = SelectionRequest {
            explicit_account: Some("Bad Label"),
            ..req()
        };
        match select(&r, &[a, b]) {
            Selection::NoCapacity { rejections, .. } => assert_eq!(
                rejections[0].reason,
                RejectReason::InvalidLabel {
                    label: "Bad Label".into()
                }
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn other_providers_and_exclusions_are_ignored() {
        let mut other = ok("a", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        other.credential = CredentialRef::new(OAuthProviderId::Anthropic, Account::parse("a").unwrap());
        let b = ok("b", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        let excl = vec![cred("b")];
        let r = SelectionRequest {
            exclude: &excl,
            ..req()
        };
        let sel = select(&r, &[other, b]);
        match sel {
            Selection::NoCapacity { rejections, .. } => {
                assert_eq!(rejections.len(), 1);
                assert_eq!(rejections[0].reason, RejectReason::Excluded);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn failover_is_bounded_and_never_after_output() {
        let a = ok("a", vec![win("primary", 7 * DAY, 100.0, NOW + DAY)]);
        let b = ok("b", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)]);
        let c = ok("c", vec![win("primary", 7 * DAY, 100.0, NOW + DAY)]);
        let cands = [a, b, c];
        let ev = FailureEvidence {
            kind: FailureKind::QuotaExhaustedPreOutput,
            output_started: false,
            tool_activity: false,
            failovers_so_far: 0,
        };
        match failover(&req(), &cred("a"), &ev, &cands) {
            FailoverDecision::Failover { credential } => {
                assert_eq!(credential.storage_key(), "openai-codex@b")
            }
            other => panic!("unexpected {other:?}"),
        }
        // second failover denied
        let ev2 = FailureEvidence {
            failovers_so_far: 1,
            ..ev.clone()
        };
        assert!(matches!(
            failover(&req(), &cred("b"), &ev2, &cands),
            FailoverDecision::NoFailover { .. }
        ));
        // output started → never
        let ev3 = FailureEvidence {
            output_started: true,
            ..ev.clone()
        };
        assert!(matches!(
            failover(&req(), &cred("a"), &ev3, &cands),
            FailoverDecision::NoFailover { .. }
        ));
        // tool activity → never
        let ev4 = FailureEvidence {
            tool_activity: true,
            ..ev.clone()
        };
        assert!(matches!(
            failover(&req(), &cred("a"), &ev4, &cands),
            FailoverDecision::NoFailover { .. }
        ));
        // ordinary 429 → never
        let ev5 = FailureEvidence {
            kind: FailureKind::RateLimited,
            ..ev.clone()
        };
        assert!(matches!(
            failover(&req(), &cred("a"), &ev5, &cands),
            FailoverDecision::NoFailover { .. }
        ));
        // explicit account → never
        let r = SelectionRequest {
            explicit_account: Some("a"),
            ..req()
        };
        assert!(matches!(
            failover(&r, &cred("a"), &ev, &cands),
            FailoverDecision::NoFailover { .. }
        ));
        // no other eligible seat → NoFailover, and the failed seat is excluded
        // even if its snapshot still claims headroom.
        let only = [ok("a", vec![win("primary", 7 * DAY, 0.0, NOW + DAY)])];
        assert!(matches!(
            failover(&req(), &cred("a"), &ev, &only),
            FailoverDecision::NoFailover { .. }
        ));
    }

    #[test]
    fn unknown_utilization_sorts_after_known() {
        let proven = ok(
            "p",
            vec![WindowLimit {
                used_percent: None,
                limit_reached: Some(false),
                ..win("primary", 7 * DAY, 0.0, NOW + DAY)
            }],
        );
        let known = ok("k", vec![win("primary", 7 * DAY, 95.0, NOW + DAY)]);
        assert_eq!(
            select(&req(), &[proven.clone(), known]).selected().unwrap().storage_key(),
            "openai-codex@k"
        );
        assert_eq!(
            select(&req(), &[proven]).selected().unwrap().storage_key(),
            "openai-codex@p"
        );
    }

    #[test]
    fn model_matching_uses_family_tokens() {
        assert!(model_matches("sonnet", "claude-sonnet-4-6"));
        assert!(model_matches("Sonnet", "CLAUDE-SONNET-4-6"));
        assert!(model_matches("gpt-6-astra", "gpt-6-astra"));
        assert!(!model_matches("opus", "claude-sonnet-4-6"));
        assert!(!model_matches("", "x") && !model_matches("x", ""));
        let w = WindowLimit {
            models: Some(vec!["sonnet".into()]),
            ..win("seven_day_sonnet", 7 * DAY, 100.0, NOW + DAY)
        };
        assert!(w.applies_to(Some("claude-sonnet-4-6")));
        assert!(!w.applies_to(Some("claude-opus-4-6")));
        assert!(w.applies_to(None));
        let acct = ok("a", vec![win("seven_day", 7 * DAY, 1.0, NOW + DAY), w]);
        let r = SelectionRequest {
            model: Some("claude-sonnet-4-6"),
            ..req()
        };
        assert!(select(&r, std::slice::from_ref(&acct)).selected().is_none());
        let r = SelectionRequest {
            model: Some("claude-opus-4-6"),
            ..req()
        };
        assert!(select(&r, std::slice::from_ref(&acct)).selected().is_some());
    }

    #[test]
    fn serde_round_trip_is_secret_free() {
        let a = ok("a", vec![win("primary", 7 * DAY, 100.0, NOW + DAY)]);
        let sel = select(&req(), &[a]);
        let json = serde_json::to_string(&sel).unwrap();
        assert!(json.contains("no_capacity"));
        let back: Selection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }
}
