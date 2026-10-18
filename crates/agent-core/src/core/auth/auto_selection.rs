//! Automatic subscription choice and its secret-free explanation.
//! Eligibility remains in `quota_policy`; config and the preview use that
//! same policy so the CLI cannot promise a different account than the broker.
use super::quota_policy::{
    self as quota, AccountCapacity, QuotaObservation, RejectReason, Selection, SelectionRequest,
    Strategy,
};
use super::{CredentialRef, OAuthProviderId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoSelectionPolicy {
    pub strategy: Strategy,
    pub overrides: BTreeMap<OAuthProviderId, Strategy>,
    pub urgent_horizon_ms: u64,
    pub sticky: bool,
}
impl Default for AutoSelectionPolicy {
    fn default() -> Self {
        Self {
            strategy: Strategy::SoonestReset,
            overrides: BTreeMap::new(),
            urgent_horizon_ms: quota::DEFAULT_URGENT_HORIZON_MS,
            sticky: true,
        }
    }
}
impl AutoSelectionPolicy {
    pub fn strategy_for(&self, provider: OAuthProviderId) -> Strategy {
        self.overrides
            .get(&provider)
            .copied()
            .unwrap_or(self.strategy)
    }
    /// Invalid values warn and retain the documented default, never an
    /// unvalidated strategy. Keys are relative to `auth.auto.`.
    pub fn from_config_map(map: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut policy = Self::default();
        let mut warnings = Vec::new();
        for (key, value) in map {
            let strategy = || match value.trim() {
                "soonest_reset" => Some(Strategy::SoonestReset),
                "lowest_utilization" => Some(Strategy::LowestUtilization),
                "preference_order" => Some(Strategy::PreferenceOrder),
                _ => None,
            };
            let valid = match key.as_str() {
                "strategy" => strategy().map(|s| policy.strategy = s).is_some(),
                "sticky" => value
                    .trim()
                    .parse::<bool>()
                    .map(|v| policy.sticky = v)
                    .is_ok(),
                "urgent_horizon_hours" => value
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|v| (1..=168).contains(v))
                    .map(|v| policy.urgent_horizon_ms = v * 3_600_000)
                    .is_some(),
                _ => {
                    if let Some(provider) = key
                        .strip_prefix("strategy.")
                        .and_then(|p| p.parse::<OAuthProviderId>().ok())
                    {
                        // A malformed explicit override must not inherit an
                        // unrelated global strategy silently.
                        policy
                            .overrides
                            .insert(provider, strategy().unwrap_or(Strategy::SoonestReset));
                        strategy().is_some()
                    } else {
                        false
                    }
                }
            };
            if !valid {
                warnings.push(format!("invalid auth.auto.{key}; using documented default"));
            }
        }
        (policy, warnings)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanRow {
    pub credential: CredentialRef,
    pub rank: Option<u32>,
    pub tier: Option<u8>,
    pub anchored: Option<bool>,
    pub budget_reset_ms: Option<u64>,
    pub utilization: Option<f64>,
    pub verdict: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectionPlan {
    pub provider: String,
    pub model: Option<String>,
    pub strategy: Strategy,
    pub urgent_horizon_ms: u64,
    pub sticky: bool,
    pub current: Option<CredentialRef>,
    pub now_ms: u64,
    pub rows: Vec<PlanRow>,
    pub selection: Selection,
}
impl SelectionPlan {
    pub fn from_candidates(req: &SelectionRequest<'_>, candidates: &[AccountCapacity]) -> Self {
        let selection = quota::select(req, candidates);
        let rejections = match &selection {
            Selection::Selected { rejections, .. } | Selection::NoCapacity { rejections, .. } => {
                rejections
            }
        };
        let mut rows: Vec<_> = candidates
            .iter()
            .filter(|c| c.credential.provider == req.provider)
            .map(|cap| {
                let eligible = quota::evaluate(req, cap).ok();
                let reason = rejections
                    .iter()
                    .find(|r| r.credential == cap.credential)
                    .map(|r| &r.reason);
                let selected = selection.selected() == Some(&cap.credential);
                let rank = if selected {
                    Some(1)
                } else if let Some(RejectReason::Outranked { rank }) = reason {
                    Some(*rank)
                } else {
                    None
                };
                // Show rejected windows too: the operator needs to see WHY the
                // imminent reset seat cannot serve requests yet.
                let budget = match &cap.observation {
                    QuotaObservation::Ok { windows, .. } => {
                        quota::budget_window(windows, req.model)
                    }
                    _ => None,
                };
                PlanRow {
                    credential: cap.credential.clone(),
                    rank,
                    tier: eligible.as_ref().map(|e| e.tier),
                    anchored: eligible.as_ref().and_then(|e| e.anchored),
                    budget_reset_ms: eligible
                        .as_ref()
                        .and_then(|e| e.budget_reset_ms)
                        .or_else(|| budget.and_then(|w| w.resets_at_ms)),
                    utilization: eligible
                        .as_ref()
                        .and_then(|e| e.utilization)
                        .or_else(|| budget.and_then(|w| w.used_percent)),
                    verdict: if selected {
                        "selected".into()
                    } else {
                        reason
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "not considered".into())
                    },
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            a.rank
                .unwrap_or(u32::MAX)
                .cmp(&b.rank.unwrap_or(u32::MAX))
                .then_with(|| a.credential.cmp(&b.credential))
        });
        Self {
            provider: req.provider.to_string(),
            model: req.model.map(str::to_string),
            strategy: req.strategy,
            urgent_horizon_ms: req.urgent_horizon_ms,
            sticky: req.sticky,
            current: req.current.cloned(),
            now_ms: req.now_ms,
            rows,
            selection,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_loader_retains_auto_keys_and_surfaces_warnings() {
        let config = crate::config::load_config_from_str("auth.auto.strategy = preference_order\nauth.auto.strategy.anthropic = lowest_utilization\nauth.auto.sticky = false\nauth.auto.urgent_horizon_hours = 48\n");
        let (p, warnings) = AutoSelectionPolicy::from_config_map(&config.auth.auto);
        assert!(warnings.is_empty());
        assert_eq!(
            p.strategy_for(OAuthProviderId::Anthropic),
            Strategy::LowestUtilization
        );
        assert_eq!(
            p.strategy_for(OAuthProviderId::OpenAiCodex),
            Strategy::PreferenceOrder
        );
        assert_eq!(p.urgent_horizon_ms, 48 * 3_600_000);
        assert!(!p.sticky);
        let bad = crate::config::load_config_from_str(
            "auth.auto.sticky = typo\nauth.auto.strategy = typo\n",
        );
        assert_eq!(
            bad.warnings
                .iter()
                .filter(|w| w.contains("auth.auto"))
                .count(),
            2
        );
    }

    #[test]
    fn defaults_overrides_and_invalid_values() {
        let p = AutoSelectionPolicy::default();
        assert_eq!(
            p.strategy_for(OAuthProviderId::Anthropic),
            Strategy::SoonestReset
        );
        assert!(p.sticky);
        let input = [
            ("strategy", "lowest_utilization"),
            ("strategy.anthropic", "preference_order"),
            ("urgent_horizon_hours", "48"),
            ("sticky", "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let (p, w) = AutoSelectionPolicy::from_config_map(&input);
        assert!(w.is_empty());
        assert_eq!(
            p.strategy_for(OAuthProviderId::Anthropic),
            Strategy::PreferenceOrder
        );
        assert_eq!(
            p.strategy_for(OAuthProviderId::OpenAiCodex),
            Strategy::LowestUtilization
        );
        assert_eq!(p.urgent_horizon_ms, 48 * 3_600_000);
        assert!(!p.sticky);
        for value in ["0", "169", "-1", "bad"] {
            let (p, w) = AutoSelectionPolicy::from_config_map(&BTreeMap::from([(
                "urgent_horizon_hours".into(),
                value.into(),
            )]));
            assert_eq!(p.urgent_horizon_ms, quota::DEFAULT_URGENT_HORIZON_MS);
            assert_eq!(w.len(), 1);
        }
        let (p, w) = AutoSelectionPolicy::from_config_map(&BTreeMap::from([
            ("strategy.anthropic".into(), "garbage".into()),
            ("strategy".into(), "lowest_utilization".into()),
        ]));
        assert_eq!(
            p.strategy_for(OAuthProviderId::Anthropic),
            Strategy::SoonestReset
        );
        assert_eq!(w.len(), 1);
    }
}
