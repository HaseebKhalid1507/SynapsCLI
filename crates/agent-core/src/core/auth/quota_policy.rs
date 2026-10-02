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
//! * **Perishable capacity first.** [`Strategy::SoonestReset`] ranks eligible
//!   seats by tier — urgent anchored, then unanchored (first use starts the
//!   clock), then far anchored, then no reset evidence — soonest budget reset
//!   first, so capacity about to expire is burned before it is lost.
//!   Stickiness keeps the currently pinned seat unless a candidate sits in a
//!   strictly higher tier (or, within tier 1, resets strictly sooner); an
//!   ineligible current seat is never kept and an excluded one never re-picked.

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
/// Default [`SelectionRequest::urgent_horizon_ms`]: an anchored budget window
/// resetting within this horizon is "urgent" — its unused capacity is about
/// to be lost, so it ranks first under [`Strategy::SoonestReset`].
pub const DEFAULT_URGENT_HORIZON_MS: u64 = 24 * 60 * 60 * 1000;
/// Tolerance for [`is_unanchored`]: a window whose reported reset is within
/// this distance of `observed_at + duration` is sliding with the observation
/// rather than fixed on the provider's calendar. Absorbs poll latency and
/// clock skew between the provider and this host.
pub const UNANCHORED_TOLERANCE_MS: u64 = 5 * 60 * 1000;
/// Maximum `used_percent` (inclusive) for a window to count as "≈0 % used"
/// in [`is_unanchored`]. A window with real consumption has, by definition,
/// already been anchored by that first use.
pub const UNANCHORED_MAX_USED_PERCENT: f64 = 0.5;

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
            Some(p) if !p.is_finite() || !(0.0..=100.0).contains(&p) => {
                return WindowVerdict::Unknown
            }
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
            None if self.limit_reached == Some(false) => {
                WindowVerdict::Headroom { used_percent: None }
            }
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

/// Ordering strategy among eligible accounts. All are deterministic.
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
    /// Perishable capacity first. Eligible seats are ranked by
    /// [`Eligible::tier`] (1 = urgent anchored, 2 = unanchored, 3 = anchored
    /// but not urgent, 4 = no reset evidence); tiers 1 and 3 order by soonest
    /// budget reset, tier 2 by preference then storage key, tier 4 by lowest
    /// utilization. Remaining ties: lowest utilization → preference → storage
    /// key. The only strategy to which [`SelectionRequest::sticky`] applies.
    SoonestReset,
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
    /// The seat currently pinned for this provider, if any. Only consulted
    /// when [`sticky`](Self::sticky) is set under [`Strategy::SoonestReset`];
    /// it never bypasses eligibility or exclusion.
    pub current: Option<&'a CredentialRef>,
    /// An anchored budget window resetting within this many ms of `now_ms`
    /// is urgent (tier 1). Default [`DEFAULT_URGENT_HORIZON_MS`].
    pub urgent_horizon_ms: u64,
    /// Keep [`current`](Self::current) when it is still eligible unless a
    /// candidate sits in a strictly higher tier (or both are tier 1 and the
    /// candidate resets strictly sooner). Avoids seat thrash and prompt-cache
    /// loss. Applies to [`Strategy::SoonestReset`] only; `false` in
    /// [`new`](Self::new) so other strategies are unaffected.
    pub sticky: bool,
}

impl<'a> SelectionRequest<'a> {
    /// Minimal request with defaults: any model, no preference/exclusions,
    /// lowest-utilization strategy, 100 % exhaustion threshold, no current
    /// seat, default urgent horizon, not sticky.
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
            current: None,
            urgent_horizon_ms: DEFAULT_URGENT_HORIZON_MS,
            sticky: false,
        }
    }

    /// Like [`new`](Self::new) but with [`Strategy::SoonestReset`], sticky
    /// selection and the default urgent horizon. Set
    /// [`current`](Self::current) for stickiness to have any effect.
    pub fn soonest_reset(provider: OAuthProviderId, now_ms: u64, max_snapshot_age_ms: u64) -> Self {
        Self {
            strategy: Strategy::SoonestReset,
            sticky: true,
            ..Self::new(provider, now_ms, max_snapshot_age_ms)
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
    /// Eligible, but another eligible account ranked higher. `rank` is the
    /// 1-based position in the final ranking (the winner is rank 1), so
    /// callers can print the full ordered table.
    Outranked {
        #[serde(default)]
        rank: u32,
    },
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
            Self::Outranked { rank } => write!(f, "eligible; outranked (rank {rank})"),
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
        /// The winner's [`Eligible::tier`] (`1..=4`; `0` only when decoded
        /// from a record written before tiers existed). Computed under every
        /// strategy so the pick can always be explained.
        #[serde(default)]
        tier: u8,
        /// The winner's [`Eligible::budget_reset_ms`], if it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_reset_ms: Option<u64>,
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
    /// Reset instant of the budget window (see [`budget_window`]), if the
    /// provider reports one that lies in the future. A reset at or before
    /// `now_ms` is not evidence of a new window and is reported as `None`.
    pub budget_reset_ms: Option<u64>,
    /// Whether the budget window's reset is a fixed instant (`Some(true)`)
    /// or slides with the observation because the window has not been
    /// started by a first use yet (`Some(false)`, see [`is_unanchored`]).
    /// `None` when there is no budget reset to classify.
    pub anchored: Option<bool>,
    /// Priority tier under [`Strategy::SoonestReset`], computed for every
    /// eligible seat regardless of strategy so any ranking can be explained:
    /// 1 = anchored and resetting within the urgent horizon (capacity about
    /// to be lost), 2 = unanchored (using it starts the clock), 3 = anchored
    /// beyond the horizon (earliest deadline first), 4 = no reset evidence
    /// (headroom proven, deadline unknown). Unknown anchoring with a known
    /// reset is treated as anchored.
    pub tier: u8,
}

/// Tier for an eligible seat; see [`Eligible::tier`].
fn tier_for(
    anchored: Option<bool>,
    budget_reset_ms: Option<u64>,
    now_ms: u64,
    urgent_horizon_ms: u64,
) -> u8 {
    match (anchored, budget_reset_ms) {
        (_, None) => 4,
        (Some(false), Some(_)) => 2,
        (_, Some(reset)) if reset.saturating_sub(now_ms) <= urgent_horizon_ms => 1,
        (_, Some(_)) => 3,
    }
}

/// Evaluate a single account against the request. Pure; ignores
/// `explicit_account` and provider matching (the caller filters those).
pub fn evaluate(
    req: &SelectionRequest<'_>,
    cap: &AccountCapacity,
) -> Result<Eligible, RejectReason> {
    if req
        .exclude
        .iter()
        .any(|c| c.storage_key() == cap.credential.storage_key())
    {
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
    // Perishable-capacity evidence. Only a future reset counts: a reset at or
    // before `now_ms` on a still-fresh reading is not evidence of a new
    // window (the same rule `NoCapacity::earliest_reset_ms` applies).
    let budget = budget_window(windows, req.model);
    let budget_reset_ms = budget
        .and_then(|w| w.resets_at_ms)
        .filter(|t| *t > req.now_ms);
    let anchored = match (budget, budget_reset_ms) {
        (Some(w), Some(_)) => is_unanchored(w, observed).map(|u| !u),
        _ => None,
    };
    let tier = tier_for(anchored, budget_reset_ms, req.now_ms, req.urgent_horizon_ms);
    Ok(Eligible {
        utilization,
        budget_reset_ms,
        anchored,
        tier,
    })
}

/// Lowest-utilization ordering: known before unknown, then ascending.
fn utilization_cmp(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    a.is_none().cmp(&b.is_none()).then_with(|| {
        a.unwrap_or(0.0)
            .partial_cmp(&b.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
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
            Strategy::LowestUtilization => utilization_cmp(ea.utilization, eb.utilization)
                .then_with(|| pa.cmp(&pb))
                .then_with(|| ka.cmp(&kb)),
            Strategy::SoonestReset => ea
                .tier
                .cmp(&eb.tier)
                .then_with(|| match ea.tier {
                    // Soonest deadline first; both resets are `Some` here.
                    1 | 3 => ea.budget_reset_ms.cmp(&eb.budget_reset_ms),
                    // Unanchored seats have no deadline yet: operator order.
                    2 => pa.cmp(&pb).then_with(|| ka.cmp(&kb)),
                    // No reset evidence: fall back to headroom.
                    _ => utilization_cmp(ea.utilization, eb.utilization),
                })
                .then_with(|| utilization_cmp(ea.utilization, eb.utilization))
                .then_with(|| pa.cmp(&pb))
                .then_with(|| ka.cmp(&kb)),
        }
    });

    // Stickiness (SoonestReset only): keep the pinned seat unless a candidate
    // is in a strictly higher tier, or both are urgent and the candidate's
    // reset is strictly sooner. An ineligible/excluded current seat is not in
    // `eligible`, so it can never be kept.
    if req.sticky && req.strategy == Strategy::SoonestReset {
        if let Some(current) = req.current {
            let key = current.storage_key();
            let at = eligible
                .iter()
                .position(|(c, _)| c.credential.storage_key() == key);
            if let Some(i) = at.filter(|i| *i > 0) {
                let best = &eligible[0].1;
                let cur = &eligible[i].1;
                let switch = best.tier < cur.tier
                    || (best.tier == 1
                        && cur.tier == 1
                        && best.budget_reset_ms < cur.budget_reset_ms);
                if !switch {
                    eligible[..=i].rotate_right(1);
                }
            }
        }
    }

    match eligible.first() {
        Some((cap, e)) => {
            let winner = cap.credential.clone();
            for (i, (other, _)) in eligible.iter().enumerate().skip(1) {
                rejections.push(Rejection {
                    credential: other.credential.clone(),
                    reason: RejectReason::Outranked {
                        rank: (i + 1) as u32,
                    },
                });
            }
            Selection::Selected {
                credential: winner,
                utilization: e.utilization,
                tier: e.tier,
                budget_reset_ms: e.budget_reset_ms,
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
    NoFailover {
        reason: String,
    },
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

/// Weekly pick over an arbitrary set of windows: account-wide first, then `id`.
fn pick_weekly<'w>(windows: impl Iterator<Item = &'w WindowLimit>) -> Option<&'w WindowLimit> {
    let mut weekly: Vec<&WindowLimit> = windows.filter(|w| w.is_weekly()).collect();
    weekly.sort_by(|a, b| {
        a.models
            .is_some()
            .cmp(&b.models.is_some())
            .then_with(|| a.id.cmp(&b.id))
    });
    weekly.into_iter().next()
}

/// The account-wide weekly window of a reading, if the provider reports one.
///
/// Prefers account-wide (`models == None`) weekly windows; falls back to a
/// model-scoped weekly window only when no account-wide one exists. Order is
/// deterministic (first by scope, then by `id`).
pub fn weekly_window(windows: &[WindowLimit]) -> Option<&WindowLimit> {
    pick_weekly(windows.iter())
}

/// The *budget window* of a reading for `model`: the window whose reset
/// decides when unused capacity is lost.
///
/// Among the windows that [`apply to`](WindowLimit::applies_to) `model`,
/// prefers the weekly window with the same preference as [`weekly_window`]
/// (account-wide before model-scoped, then `id`); otherwise the window with
/// the longest reported `duration_ms` (ties: account-wide first, then `id`).
/// `None` when no applicable window reports a duration — a window of unknown
/// length cannot be a deadline.
pub fn budget_window<'w>(
    windows: &'w [WindowLimit],
    model: Option<&str>,
) -> Option<&'w WindowLimit> {
    let applicable = || windows.iter().filter(|w| w.applies_to(model));
    if let Some(weekly) = pick_weekly(applicable()) {
        return Some(weekly);
    }
    applicable()
        .filter(|w| w.duration_ms.is_some())
        .min_by(|a, b| {
            b.duration_ms
                .cmp(&a.duration_ms)
                .then_with(|| a.models.is_some().cmp(&b.models.is_some()))
                .then_with(|| a.id.cmp(&b.id))
        })
}

/// Whether `window`'s reset slides with the observation instead of being a
/// fixed instant — the signature of a seat whose window has not been started
/// by a first use (Codex anchors a window at first use after a reset; an
/// idle seat therefore keeps reporting `reset ≈ observed_at + duration`).
///
/// * `Some(true)`: `used_percent` is reported and at most
///   [`UNANCHORED_MAX_USED_PERCENT`], **and** both `duration_ms` and
///   `resets_at_ms` are present, **and**
///   `|resets_at_ms − (observed_at_ms + duration_ms)| ≤ UNANCHORED_TOLERANCE_MS`.
/// * `Some(false)`: duration and reset are present but the window has been
///   used, its utilization is unreported, or its reset does not track the
///   observation (e.g. an Anthropic 7-day window at 0 % still reports the
///   provider's fixed calendar reset).
/// * `None`: duration or reset missing — anchoring cannot be judged.
pub fn is_unanchored(window: &WindowLimit, observed_at_ms: u64) -> Option<bool> {
    let (Some(duration), Some(reset)) = (window.duration_ms, window.resets_at_ms) else {
        return None;
    };
    let near_zero = matches!(
        window.used_percent,
        Some(p) if (0.0..=UNANCHORED_MAX_USED_PERCENT).contains(&p)
    );
    if !near_zero {
        return Some(false);
    }
    let expected = observed_at_ms.saturating_add(duration);
    Some(expected.abs_diff(reset) <= UNANCHORED_TOLERANCE_MS)
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
        assert_eq!(
            w.verdict(100.0),
            WindowVerdict::Headroom { used_percent: None }
        );
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
        assert!(select(
            &r,
            &[ok("a", vec![win("primary", 7 * DAY, 1.0, NOW + DAY)])]
        )
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
            select(&r, &[b.clone(), c.clone()])
                .selected()
                .unwrap()
                .storage_key(),
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
        let sel = select(
            &req(),
            &[exhausted, cooling, unknown_window, past_exhausted],
        );
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
        let acct = ok(
            "a",
            vec![
                win("seven_day", 7 * DAY, 10.0, NOW + DAY),
                five_h,
                model_win,
            ],
        );
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
        assert!(select(&req(), std::slice::from_ref(&acct))
            .selected()
            .is_none());
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
        other.credential =
            CredentialRef::new(OAuthProviderId::Anthropic, Account::parse("a").unwrap());
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
            select(&req(), &[proven.clone(), known])
                .selected()
                .unwrap()
                .storage_key(),
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

    // ── SoonestReset ─────────────────────────────────────────────────────────

    /// Request under [`Strategy::SoonestReset`] with the constructor defaults
    /// (sticky, default horizon, no current seat).
    fn sr_req() -> SelectionRequest<'static> {
        SelectionRequest::soonest_reset(OAuthProviderId::OpenAiCodex, NOW, 30 * 60 * 1000)
    }

    /// Observation instant used by [`ok`].
    const OBSERVED: u64 = NOW - 1000;

    /// Anchored weekly window: reset is a fixed instant unrelated to the
    /// observation (Anthropic-like, or a Codex seat already in use).
    fn anchored(used: f64, resets: u64) -> WindowLimit {
        win("primary", 7 * DAY, used, resets)
    }

    /// Unanchored weekly window: idle Codex-like seat reporting 0 % and a
    /// reset exactly one duration after the observation.
    fn unanchored() -> WindowLimit {
        win("primary", 7 * DAY, 0.0, OBSERVED + 7 * DAY)
    }

    /// Headroom proven but no reset instant reported.
    fn no_reset(used: f64) -> WindowLimit {
        WindowLimit {
            resets_at_ms: None,
            ..win("primary", 7 * DAY, used, 0)
        }
    }

    fn label_of(sel: &Selection) -> &str {
        sel.selected().expect("a selection").account.label_str()
    }

    fn ranks(sel: &Selection) -> Vec<(String, u32)> {
        let (Selection::Selected { rejections, .. } | Selection::NoCapacity { rejections, .. }) =
            sel;
        rejections
            .iter()
            .filter_map(|r| match r.reason {
                RejectReason::Outranked { rank } => {
                    Some((r.credential.account.label_str().to_string(), rank))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn soonest_reset_beats_lower_utilization_where_lowest_utilization_does_not() {
        // `a` is busier but its capacity expires sooner.
        let a = ok("a", vec![anchored(60.0, NOW + 2 * DAY)]);
        let b = ok("b", vec![anchored(10.0, NOW + 5 * DAY)]);
        let cands = [b.clone(), a.clone()];
        match select(&sr_req(), &cands) {
            Selection::Selected {
                credential,
                tier,
                budget_reset_ms,
                utilization,
                ..
            } => {
                assert_eq!(credential.storage_key(), "openai-codex@a");
                assert_eq!(tier, 3);
                assert_eq!(budget_reset_ms, Some(NOW + 2 * DAY));
                assert_eq!(utilization, Some(60.0));
            }
            other => panic!("unexpected {other:?}"),
        }
        // Same input, previous strategy: spreads load to the emptier seat.
        assert_eq!(label_of(&select(&req(), &cands)), "b");
        // `new` keeps its defaults byte-for-byte: not sticky, no current.
        let r = req();
        assert_eq!(r.strategy, Strategy::LowestUtilization);
        assert!(!r.sticky && r.current.is_none());
        assert_eq!(r.urgent_horizon_ms, DEFAULT_URGENT_HORIZON_MS);
        let s = sr_req();
        assert_eq!(s.strategy, Strategy::SoonestReset);
        assert!(s.sticky && s.current.is_none());
        assert_eq!(s.urgent_horizon_ms, DEFAULT_URGENT_HORIZON_MS);
    }

    #[test]
    fn tiers_order_urgent_then_unanchored_then_far_then_no_reset() {
        let urgent = ok("u", vec![anchored(50.0, NOW + 6 * HOUR)]);
        let sliding = ok("s", vec![unanchored()]);
        let far = ok("f", vec![anchored(0.0, NOW + 5 * DAY)]);
        let none = ok("n", vec![no_reset(0.0)]);
        let r = sr_req();
        for (cap, tier, anch) in [
            (&urgent, 1u8, Some(true)),
            (&sliding, 2, Some(false)),
            (&far, 3, Some(true)),
            (&none, 4, None),
        ] {
            let e = evaluate(&r, cap).unwrap();
            assert_eq!(e.tier, tier, "{}", cap.credential.storage_key());
            assert_eq!(e.anchored, anch, "{}", cap.credential.storage_key());
        }
        // Tier is computed regardless of strategy (for `auth plan`).
        assert_eq!(evaluate(&req(), &sliding).unwrap().tier, 2);
        // Lower utilization/preference never beats a higher tier.
        let cands = [none.clone(), far.clone(), sliding.clone(), urgent.clone()];
        let sel = select(&r, &cands);
        assert_eq!(label_of(&sel), "u");
        assert_eq!(
            ranks(&sel),
            vec![
                ("s".to_string(), 2),
                ("f".to_string(), 3),
                ("n".to_string(), 4)
            ]
        );
        assert_eq!(
            label_of(&select(&r, &[none.clone(), far.clone(), sliding])),
            "s"
        );
        assert_eq!(label_of(&select(&r, &[none.clone(), far])), "f");
        assert_eq!(label_of(&select(&r, &[none])), "n");
        // Urgency is horizon-relative: with a 1 h horizon the 6 h seat is tier 3
        // and loses to the unanchored seat.
        let short = SelectionRequest {
            urgent_horizon_ms: HOUR,
            ..sr_req()
        };
        assert_eq!(evaluate(&short, &urgent).unwrap().tier, 3);
        assert_eq!(
            label_of(&select(&short, &[urgent, ok("s", vec![unanchored()])])),
            "s"
        );
        // Exactly at the horizon is still urgent (inclusive).
        let edge = ok("e", vec![anchored(0.0, NOW + DEFAULT_URGENT_HORIZON_MS)]);
        assert_eq!(evaluate(&sr_req(), &edge).unwrap().tier, 1);
    }

    #[test]
    fn within_tier_orders_by_reset_preference_or_utilization() {
        // Tier 1: soonest reset wins even with higher utilization.
        let a = ok("a", vec![anchored(90.0, NOW + 2 * HOUR)]);
        let b = ok("b", vec![anchored(1.0, NOW + 3 * HOUR)]);
        assert_eq!(label_of(&select(&sr_req(), &[b, a])), "a");
        // Tier 2: preference, then storage key; utilization is irrelevant.
        let s1 = ok("s1", vec![unanchored()]);
        let s2 = ok("s2", vec![unanchored()]);
        assert_eq!(
            label_of(&select(&sr_req(), &[s2.clone(), s1.clone()])),
            "s1"
        );
        let pref = vec!["s2".to_string()];
        let r = SelectionRequest {
            preference: &pref,
            ..sr_req()
        };
        assert_eq!(label_of(&select(&r, &[s1, s2])), "s2");
        // Tier 4: lowest utilization, unknown last.
        let n1 = ok("n1", vec![no_reset(40.0)]);
        let n2 = ok("n2", vec![no_reset(5.0)]);
        let n3 = ok(
            "n3",
            vec![WindowLimit {
                used_percent: None,
                limit_reached: Some(false),
                ..no_reset(0.0)
            }],
        );
        let sel = select(&sr_req(), &[n3, n1, n2]);
        assert_eq!(label_of(&sel), "n2");
        assert_eq!(
            ranks(&sel),
            vec![("n1".to_string(), 2), ("n3".to_string(), 3)]
        );
        // Equal resets in tier 3: lowest utilization breaks the tie.
        let t1 = ok("t1", vec![anchored(30.0, NOW + 3 * DAY)]);
        let t2 = ok("t2", vec![anchored(20.0, NOW + 3 * DAY)]);
        assert_eq!(label_of(&select(&sr_req(), &[t1, t2])), "t2");
    }

    #[test]
    fn exhausted_soonest_seat_is_skipped_to_next_soonest() {
        let x = ok("x", vec![anchored(100.0, NOW + HOUR)]);
        let a = ok("a", vec![anchored(20.0, NOW + 2 * DAY)]);
        let b = ok("b", vec![anchored(20.0, NOW + 3 * DAY)]);
        let sel = select(&sr_req(), &[b, x.clone(), a]);
        assert_eq!(label_of(&sel), "a");
        let Selection::Selected { rejections, .. } = &sel else {
            panic!("expected selection");
        };
        assert!(rejections
            .iter()
            .any(|r| r.credential.account.label_str() == "x"
                && r.reason
                    == RejectReason::Exhausted {
                        window: "primary".into(),
                        resets_at_ms: Some(NOW + HOUR),
                    }));
        // Everything exhausted: earliest *future* reset is reported.
        let y = ok("y", vec![anchored(100.0, NOW + 2 * DAY)]);
        let past = ok("p", vec![anchored(100.0, NOW - HOUR)]);
        match select(&sr_req(), &[y, past, x]) {
            Selection::NoCapacity {
                earliest_reset_ms, ..
            } => assert_eq!(earliest_reset_ms, Some(NOW + HOUR)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn five_hour_throttle_rejects_then_reenters_after_reset() {
        let throttled = ok(
            "t",
            vec![
                win("five_hour", 5 * HOUR, 100.0, NOW + HOUR),
                win("seven_day", 7 * DAY, 30.0, NOW + 2 * DAY),
            ],
        );
        let other = ok("o", vec![win("seven_day", 7 * DAY, 10.0, NOW + 4 * DAY)]);
        let sel = select(&sr_req(), &[throttled, other.clone()]);
        assert_eq!(label_of(&sel), "o");
        let Selection::Selected { rejections, .. } = &sel else {
            panic!("expected selection");
        };
        assert_eq!(
            rejections[0].reason,
            RejectReason::Exhausted {
                window: "five_hour".into(),
                resets_at_ms: Some(NOW + HOUR),
            }
        );
        // Clock past the 5 h reset, fresh observations: the 5 h window is
        // empty again and the seat wins on its sooner 7 d deadline.
        let now2 = NOW + HOUR + 1;
        let mut throttled2 = ok(
            "t",
            vec![
                win("five_hour", 5 * HOUR, 0.0, now2 + 5 * HOUR),
                win("seven_day", 7 * DAY, 30.0, NOW + 2 * DAY),
            ],
        );
        throttled2.observed_at_ms = Some(now2 - 1000);
        let mut other2 = other;
        other2.observed_at_ms = Some(now2 - 1000);
        let r = SelectionRequest {
            now_ms: now2,
            ..sr_req()
        };
        match select(&r, &[other2, throttled2]) {
            Selection::Selected {
                credential,
                tier,
                budget_reset_ms,
                ..
            } => {
                assert_eq!(credential.storage_key(), "openai-codex@t");
                assert_eq!(tier, 3);
                assert_eq!(budget_reset_ms, Some(NOW + 2 * DAY));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn is_unanchored_classifies_sliding_resets_only() {
        let obs = NOW - 90_000;
        // Codex-like idle seat: reset == observed + duration.
        let codex_idle = win("primary", 7 * DAY, 0.0, obs + 7 * DAY);
        assert_eq!(is_unanchored(&codex_idle, obs), Some(true));
        // One minute of skew either way is within tolerance.
        let skewed = win("primary", 7 * DAY, 0.0, obs + 7 * DAY + 60_000);
        assert_eq!(is_unanchored(&skewed, obs), Some(true));
        let skewed = win("primary", 7 * DAY, 0.0, obs + 7 * DAY - 60_000);
        assert_eq!(is_unanchored(&skewed, obs), Some(true));
        // Just beyond tolerance → anchored.
        let beyond = win(
            "primary",
            7 * DAY,
            0.0,
            obs + 7 * DAY + UNANCHORED_TOLERANCE_MS + 1,
        );
        assert_eq!(is_unanchored(&beyond, obs), Some(false));
        // Anthropic-like: 0 % but the calendar reset is 5 days out.
        let anthropic = win("seven_day", 7 * DAY, 0.0, obs + 5 * DAY);
        assert_eq!(is_unanchored(&anthropic, obs), Some(false));
        // Real consumption means the window has already been anchored.
        let used = win("primary", 7 * DAY, 40.0, obs + 7 * DAY);
        assert_eq!(is_unanchored(&used, obs), Some(false));
        // Threshold is inclusive.
        let half = win(
            "primary",
            7 * DAY,
            UNANCHORED_MAX_USED_PERCENT,
            obs + 7 * DAY,
        );
        assert_eq!(is_unanchored(&half, obs), Some(true));
        // Unreported utilization is never evidence of idleness.
        let unreported = WindowLimit {
            used_percent: None,
            limit_reached: Some(false),
            ..codex_idle.clone()
        };
        assert_eq!(is_unanchored(&unreported, obs), Some(false));
        // Missing duration or reset → unknown.
        let no_dur = WindowLimit {
            duration_ms: None,
            ..codex_idle.clone()
        };
        assert_eq!(is_unanchored(&no_dur, obs), None);
        let no_reset = WindowLimit {
            resets_at_ms: None,
            ..codex_idle
        };
        assert_eq!(is_unanchored(&no_reset, obs), None);
    }

    #[test]
    fn budget_window_prefers_weekly_then_longest() {
        let five_h = win("five_hour", 5 * HOUR, 0.0, NOW + HOUR);
        let one_h = win("one_hour", HOUR, 0.0, NOW + HOUR);
        let thirty_d = win("thirty_day", 30 * DAY, 0.0, NOW + 20 * DAY);
        let weekly_wide = win("weekly", 7 * DAY, 0.0, NOW + 3 * DAY);
        let weekly_scoped = WindowLimit {
            id: "aaa_weekly_scoped".into(),
            models: Some(vec!["m".into()]),
            ..weekly_wide.clone()
        };
        // Account-wide weekly beats model-scoped weekly beats longest other.
        let all = [
            thirty_d.clone(),
            weekly_scoped.clone(),
            five_h.clone(),
            weekly_wide.clone(),
        ];
        assert_eq!(
            budget_window(&all, Some("m")).map(|w| w.id.as_str()),
            Some("weekly")
        );
        assert_eq!(
            budget_window(&all, None).map(|w| w.id.as_str()),
            Some("weekly")
        );
        let no_wide = [thirty_d.clone(), weekly_scoped.clone(), five_h.clone()];
        assert_eq!(
            budget_window(&no_wide, Some("m")).map(|w| w.id.as_str()),
            Some("aaa_weekly_scoped")
        );
        // Scoped weekly does not apply to another model → longest applicable.
        assert_eq!(
            budget_window(&no_wide, Some("other")).map(|w| w.id.as_str()),
            Some("thirty_day")
        );
        // No weekly at all → longest duration.
        assert_eq!(
            budget_window(&[five_h.clone(), one_h.clone()], None).map(|w| w.id.as_str()),
            Some("five_hour")
        );
        // Equal durations: account-wide first, then id.
        let b5 = WindowLimit {
            id: "b5".into(),
            ..five_h.clone()
        };
        let a5_scoped = WindowLimit {
            id: "a5".into(),
            models: Some(vec!["m".into()]),
            ..five_h.clone()
        };
        assert_eq!(
            budget_window(&[b5.clone(), a5_scoped.clone()], Some("m")).map(|w| w.id.as_str()),
            Some("b5")
        );
        let a5 = WindowLimit {
            id: "a5".into(),
            ..five_h.clone()
        };
        assert_eq!(
            budget_window(&[b5, a5], None).map(|w| w.id.as_str()),
            Some("a5")
        );
        // No durations → no budget window.
        let no_dur = WindowLimit {
            duration_ms: None,
            ..five_h
        };
        assert!(budget_window(&[no_dur.clone(), no_dur], None).is_none());
        assert!(budget_window(&[], None).is_none());
        // `weekly_window` is unchanged by the shared picker.
        assert_eq!(weekly_window(&all).map(|w| w.id.as_str()), Some("weekly"));
    }

    #[test]
    fn past_reset_with_headroom_is_not_reset_evidence() {
        // Fresh reading, headroom, but the reported reset already passed: no
        // deadline can be inferred → tier 4, not tier 1 with a zero horizon.
        let stale_reset = ok("p", vec![anchored(20.0, NOW - 1)]);
        let e = evaluate(&sr_req(), &stale_reset).unwrap();
        assert_eq!(e.budget_reset_ms, None);
        assert_eq!(e.anchored, None);
        assert_eq!(e.tier, 4);
        let far = ok("f", vec![anchored(80.0, NOW + 6 * DAY)]);
        assert_eq!(label_of(&select(&sr_req(), &[stale_reset, far])), "f");
    }

    #[test]
    fn sticky_keeps_current_within_tier_3() {
        let cur = cred("cur");
        let current = ok("cur", vec![anchored(30.0, NOW + 5 * DAY)]);
        let sooner = ok("soon", vec![anchored(10.0, NOW + 2 * DAY)]);
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        let sel = select(&r, &[sooner.clone(), current.clone()]);
        assert_eq!(label_of(&sel), "cur");
        assert_eq!(ranks(&sel), vec![("soon".to_string(), 2)]);
        // Without a current seat the sooner deadline wins.
        assert_eq!(
            label_of(&select(&sr_req(), &[sooner.clone(), current.clone()])),
            "soon"
        );
        // sticky = false ignores `current`.
        let r = SelectionRequest { sticky: false, ..r };
        assert_eq!(label_of(&select(&r, &[sooner, current])), "soon");
    }

    #[test]
    fn sticky_yields_to_strictly_higher_tier() {
        let cur = cred("cur");
        let current = ok("cur", vec![anchored(10.0, NOW + 5 * DAY)]);
        let urgent = ok("urg", vec![anchored(90.0, NOW + 6 * HOUR)]);
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        assert_eq!(label_of(&select(&r, &[current.clone(), urgent])), "urg");
        // Tier 2 also outranks a tier-3 current seat.
        let sliding = ok("sld", vec![unanchored()]);
        assert_eq!(label_of(&select(&r, &[current, sliding])), "sld");
        // But a tier-2 current seat is kept over another tier-2 seat that
        // would otherwise sort first, and over any tier-3 seat.
        let cur_sliding = ok("cur", vec![unanchored()]);
        let a_sliding = ok("a", vec![unanchored()]);
        let far = ok("far", vec![anchored(0.0, NOW + 2 * DAY)]);
        assert_eq!(label_of(&select(&r, &[a_sliding, far, cur_sliding])), "cur");
    }

    #[test]
    fn sticky_tier1_yields_only_to_strictly_sooner_reset() {
        let cur = cred("cur");
        let current = ok("cur", vec![anchored(50.0, NOW + 10 * HOUR)]);
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        let sooner = ok("soon", vec![anchored(80.0, NOW + 6 * HOUR)]);
        assert_eq!(label_of(&select(&r, &[current.clone(), sooner])), "soon");
        // Equal reset with lower utilization would sort first, but it is not
        // strictly sooner → current kept.
        let equal = ok("eq", vec![anchored(1.0, NOW + 10 * HOUR)]);
        let sel = select(&r, &[equal.clone(), current.clone()]);
        assert_eq!(label_of(&sel), "cur");
        assert_eq!(ranks(&sel), vec![("eq".to_string(), 2)]);
        // Later reset in tier 1 → current kept.
        let later = ok("late", vec![anchored(0.0, NOW + 12 * HOUR)]);
        assert_eq!(label_of(&select(&r, &[later, current])), "cur");
    }

    #[test]
    fn sticky_never_keeps_ineligible_current_and_only_applies_to_soonest_reset() {
        let cur = cred("cur");
        let other = ok("o", vec![anchored(50.0, NOW + 5 * DAY)]);
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        // Exhausted current → switch.
        let exhausted = ok("cur", vec![anchored(100.0, NOW + DAY)]);
        assert_eq!(label_of(&select(&r, &[exhausted, other.clone()])), "o");
        // Cooling-down current → switch.
        let mut cooling = ok("cur", vec![anchored(0.0, NOW + DAY)]);
        cooling.cooldown_until_ms = Some(NOW + HOUR);
        assert_eq!(label_of(&select(&r, &[cooling, other.clone()])), "o");
        // Stale current → switch.
        let mut stale = ok("cur", vec![anchored(0.0, NOW + DAY)]);
        stale.observed_at_ms = Some(NOW - 2 * HOUR);
        assert_eq!(label_of(&select(&r, &[stale, other.clone()])), "o");
        // Excluded current → switch (exclusion wins over stickiness).
        let excl = vec![cur.clone()];
        let r_excl = SelectionRequest {
            exclude: &excl,
            ..r.clone()
        };
        let healthy = ok("cur", vec![anchored(0.0, NOW + DAY)]);
        assert_eq!(
            label_of(&select(&r_excl, &[healthy.clone(), other.clone()])),
            "o"
        );
        // Current not among candidates at all → normal ranking, no panic.
        assert_eq!(label_of(&select(&r, std::slice::from_ref(&other))), "o");
        // Stickiness never applies to the other strategies, even if asked.
        let busy_current = ok("cur", vec![anchored(90.0, NOW + 5 * DAY)]);
        let r_lu = SelectionRequest {
            strategy: Strategy::LowestUtilization,
            sticky: true,
            current: Some(&cur),
            ..req()
        };
        assert_eq!(
            label_of(&select(&r_lu, &[busy_current.clone(), other.clone()])),
            "o"
        );
        let pref = vec!["o".to_string()];
        let r_po = SelectionRequest {
            strategy: Strategy::PreferenceOrder,
            sticky: true,
            current: Some(&cur),
            preference: &pref,
            ..req()
        };
        assert_eq!(label_of(&select(&r_po, &[busy_current, other])), "o");
    }

    #[test]
    fn failover_under_soonest_reset_never_repicks_sticky_failed_seat() {
        let cur = cred("a");
        // `a` still claims headroom and would be kept by stickiness.
        let a = ok("a", vec![anchored(0.0, NOW + 2 * DAY)]);
        let b = ok("b", vec![anchored(50.0, NOW + 5 * DAY)]);
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        assert_eq!(label_of(&select(&r, &[a.clone(), b.clone()])), "a");
        let ev = FailureEvidence {
            kind: FailureKind::QuotaExhaustedPreOutput,
            output_started: false,
            tool_activity: false,
            failovers_so_far: 0,
        };
        match failover(&r, &cur, &ev, &[a.clone(), b]) {
            FailoverDecision::Failover { credential } => {
                assert_eq!(credential.storage_key(), "openai-codex@b")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            failover(&r, &cur, &ev, &[a]),
            FailoverDecision::NoFailover { .. }
        ));
    }

    #[test]
    fn explicit_account_under_soonest_reset_never_falls_back() {
        let a = ok("a", vec![anchored(100.0, NOW + HOUR)]);
        let b = ok("b", vec![anchored(0.0, NOW + 6 * HOUR)]);
        let cur = cred("b");
        // Explicit exhausted seat: fail, even though `b` is urgent and pinned.
        let r = SelectionRequest {
            explicit_account: Some("a"),
            current: Some(&cur),
            ..sr_req()
        };
        match select(&r, &[a.clone(), b.clone()]) {
            Selection::NoCapacity { rejections, .. } => {
                assert_eq!(rejections.len(), 1);
                assert_eq!(rejections[0].credential.storage_key(), "openai-codex@a");
            }
            other => panic!("unexpected {other:?}"),
        }
        // Explicit healthy seat: other seats are never even evaluated.
        let r = SelectionRequest {
            explicit_account: Some("b"),
            ..sr_req()
        };
        match select(&r, &[a, b]) {
            Selection::Selected {
                credential,
                rejections,
                tier,
                ..
            } => {
                assert_eq!(credential.storage_key(), "openai-codex@b");
                assert!(rejections.is_empty());
                assert_eq!(tier, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// All permutations of `items` (Heap's algorithm), for order-independence checks.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        fn go<T: Clone>(k: usize, a: &mut Vec<T>, out: &mut Vec<Vec<T>>) {
            if k <= 1 {
                out.push(a.clone());
                return;
            }
            go(k - 1, a, out);
            for i in 0..k - 1 {
                if k % 2 == 0 {
                    a.swap(i, k - 1);
                } else {
                    a.swap(0, k - 1);
                }
                go(k - 1, a, out);
            }
        }
        let mut a = items.to_vec();
        let mut out = Vec::new();
        go(a.len(), &mut a, &mut out);
        out
    }

    #[test]
    fn soonest_reset_is_deterministic_under_candidate_shuffle() {
        let cur = cred("t3b");
        let seats = vec![
            ok("t1", vec![anchored(70.0, NOW + 3 * HOUR)]),
            ok("t2", vec![unanchored()]),
            ok("t3a", vec![anchored(5.0, NOW + 2 * DAY)]),
            ok("t3b", vec![anchored(40.0, NOW + 4 * DAY)]),
            ok("x", vec![anchored(100.0, NOW + DAY)]),
        ];
        let r = SelectionRequest {
            current: Some(&cur),
            ..sr_req()
        };
        let perms = permutations(&seats);
        assert_eq!(perms.len(), 120);
        let baseline = select(&r, &seats);
        assert_eq!(label_of(&baseline), "t1");
        let normalize = |sel: &Selection| {
            let Selection::Selected {
                credential,
                tier,
                budget_reset_ms,
                utilization,
                rejections,
            } = sel
            else {
                panic!("expected selection");
            };
            let mut rej: Vec<(String, RejectReason)> = rejections
                .iter()
                .map(|r| (r.credential.storage_key(), r.reason.clone()))
                .collect();
            rej.sort_by(|a, b| a.0.cmp(&b.0));
            (
                credential.storage_key(),
                *tier,
                *budget_reset_ms,
                *utilization,
                rej,
            )
        };
        let expected = normalize(&baseline);
        assert_eq!(
            ranks(&baseline),
            vec![
                ("t2".to_string(), 2),
                ("t3a".to_string(), 3),
                ("t3b".to_string(), 4)
            ]
        );
        for p in perms {
            assert_eq!(normalize(&select(&r, &p)), expected);
        }
    }

    #[test]
    fn outranked_ranks_are_two_to_n_in_ranking_order() {
        let seats = [
            ok("c", vec![anchored(0.0, NOW + 4 * DAY)]),
            ok("a", vec![anchored(0.0, NOW + 2 * DAY)]),
            ok("d", vec![anchored(0.0, NOW + 5 * DAY)]),
            ok("b", vec![anchored(0.0, NOW + 3 * DAY)]),
        ];
        let sel = select(&sr_req(), &seats);
        assert_eq!(label_of(&sel), "a");
        assert_eq!(
            ranks(&sel),
            vec![
                ("b".to_string(), 2),
                ("c".to_string(), 3),
                ("d".to_string(), 4)
            ]
        );
        // Ranks are produced under the existing strategies too.
        let lu = [
            ok("hi", vec![anchored(80.0, NOW + DAY)]),
            ok("lo", vec![anchored(10.0, NOW + DAY)]),
            ok("mid", vec![anchored(50.0, NOW + DAY)]),
        ];
        let sel = select(&req(), &lu);
        assert_eq!(label_of(&sel), "lo");
        assert_eq!(
            ranks(&sel),
            vec![("mid".to_string(), 2), ("hi".to_string(), 3)]
        );
        assert_eq!(
            RejectReason::Outranked { rank: 3 }.to_string(),
            "eligible; outranked (rank 3)"
        );
    }

    #[test]
    fn soonest_reset_serde_is_backward_compatible() {
        assert_eq!(
            serde_json::to_string(&Strategy::SoonestReset).unwrap(),
            "\"soonest_reset\""
        );
        assert_eq!(
            serde_json::from_str::<Strategy>("\"soonest_reset\"").unwrap(),
            Strategy::SoonestReset
        );
        let a = ok("a", vec![anchored(10.0, NOW + 2 * HOUR)]);
        let b = ok("b", vec![anchored(10.0, NOW + 3 * HOUR)]);
        let sel = select(&sr_req(), &[a, b]);
        let json = serde_json::to_string(&sel).unwrap();
        assert!(json.contains("\"tier\":1"));
        assert!(json.contains("\"budget_reset_ms\""));
        assert!(json.contains("\"rank\":2"));
        assert_eq!(serde_json::from_str::<Selection>(&json).unwrap(), sel);
        // Records written before tiers/ranks existed still decode.
        let old = r#"{"outcome":"selected","credential":"openai-codex@a","utilization":10.0,"rejections":[{"credential":"openai-codex@b","reason":{"reason":"outranked"}}]}"#;
        match serde_json::from_str::<Selection>(old).unwrap() {
            Selection::Selected {
                tier,
                budget_reset_ms,
                rejections,
                ..
            } => {
                assert_eq!(tier, 0);
                assert_eq!(budget_reset_ms, None);
                assert_eq!(rejections[0].reason, RejectReason::Outranked { rank: 0 });
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
