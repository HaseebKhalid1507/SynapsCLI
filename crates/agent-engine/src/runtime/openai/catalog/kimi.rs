//! Exact-id reasoning capability for the Kimi (Moonshot AI) static provider.
//!
//! Evidence: official Moonshot platform docs
//! (platform.kimi.ai/docs/guide/use-thinking-effort,
//! platform.kimi.ai/docs/guide/use-kimi-k2-thinking-model), verified live
//! against `api.moonshot.ai`:
//!
//! * `kimi-k3` — always reasons; graduated control via the **top-level**
//!   `reasoning_effort` request field with exactly `"low"` / `"high"` /
//!   `"max"` (provider default `"max"`). Reasoning cannot be disabled.
//! * `kimi-k2.7-code` / `kimi-k2.7-code-highspeed` — thinking is always on;
//!   the K2.x `thinking` field accepts only `{"type":"enabled"}` and the
//!   live API errors on `"disabled"`. No effort control exists.
//! * `kimi-k2.6` / `kimi-k2.5` — thinking on by default, disableable via
//!   `{"thinking":{"type":"disabled"}}`. No graduated effort control.
//!
//! The Moonshot API silently ignores unknown top-level fields and unknown
//! `reasoning_effort` values (verified live), so local validation is the only
//! honest gate: an unsupported level must be rejected here, never sent and
//! silently swallowed upstream.

use agent_core::reasoning::ReasoningLevel;
use serde_json::{json, Map, Value};

use super::{
    CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary, ReasoningSupport,
};

/// Documented reasoning capability for an exact Kimi model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiReasoningCapability {
    /// Documented named-effort control (top-level `reasoning_effort`).
    Effort {
        supported: &'static [ReasoningLevel],
        default_level: Option<ReasoningLevel>,
        /// Whether reasoning can be disabled. `false` → `Off` must be
        /// rejected, never silently omitted/downgraded.
        can_disable: bool,
    },
    /// Thinking is always on; no effort control and no disable switch
    /// (the live API rejects `thinking.type = "disabled"`).
    AlwaysThinking,
    /// Thinking on by default; only a documented on/off toggle
    /// (`thinking.type = "enabled" | "disabled"`), no graduated effort.
    ToggleableThinking,
}

/// Exact-id capability lookup — never substring-based. Unknown ids return
/// `None` and fail closed at validation time.
pub fn kimi_static_capability(model_id: &str) -> Option<KimiReasoningCapability> {
    use ReasoningLevel::*;
    match model_id {
        "kimi-k3" => Some(KimiReasoningCapability::Effort {
            supported: &[Low, High, Max],
            default_level: Some(Max),
            can_disable: false,
        }),
        "kimi-k2.7-code" | "kimi-k2.7-code-highspeed" => {
            Some(KimiReasoningCapability::AlwaysThinking)
        }
        "kimi-k2.6" | "kimi-k2.5" => Some(KimiReasoningCapability::ToggleableThinking),
        _ => None,
    }
}

// ─── Managed Kimi Code (OAuth) catalog ───────────────────────────────────────
//
// Exact ids served by the managed endpoint `https://api.kimi.com/coding/v1`
// (`GET /models`), verified against the official Kimi Code CLI v0.31.1
// provisioning output. This is a conservative allowlist: unknown ids fail
// closed at route resolution, mirroring the xai-auth/google-gemini pattern.

/// One managed Kimi Code model row (id as served by `/models`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KimiCodeModelDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub context_tokens: Option<u64>,
}

pub const KIMI_CODE_TEXT_MODELS: &[KimiCodeModelDescriptor] = &[
    KimiCodeModelDescriptor {
        id: "k3",
        label: "Kimi K3",
        context_tokens: Some(1_048_576),
    },
    KimiCodeModelDescriptor {
        id: "k3-256k",
        label: "Kimi K3 256k",
        context_tokens: Some(262_144),
    },
    KimiCodeModelDescriptor {
        id: "kimi-for-coding",
        label: "Kimi K2.7 Coding",
        context_tokens: Some(262_144),
    },
    KimiCodeModelDescriptor {
        id: "kimi-for-coding-highspeed",
        label: "Kimi K2.7 Coding Highspeed",
        context_tokens: Some(262_144),
    },
];

/// Exact-id lookup — never substring-based. Unknown ids return `None` and
/// fail closed at route resolution.
pub fn kimi_code_model(id: &str) -> Option<&'static KimiCodeModelDescriptor> {
    KIMI_CODE_TEXT_MODELS.iter().find(|model| model.id == id)
}

/// Reasoning capability for an exact managed Kimi Code model id.
///
/// Evidence: the managed `/models` payload provisioned by the official CLI —
/// `k3`/`k3-256k` carry `think_efforts {low, high, max}` with default `high`
/// and `always_thinking`; the `kimi-for-coding*` pair are `always_thinking`
/// with no effort control. Reasoning is not disableable on any managed id.
pub fn kimi_code_capability(model_id: &str) -> Option<KimiReasoningCapability> {
    use ReasoningLevel::*;
    match model_id {
        "k3" | "k3-256k" => Some(KimiReasoningCapability::Effort {
            supported: &[Low, High, Max],
            default_level: Some(High),
            can_disable: false,
        }),
        "kimi-for-coding" | "kimi-for-coding-highspeed" => {
            Some(KimiReasoningCapability::AlwaysThinking)
        }
        _ => None,
    }
}

/// Static catalog rows for the managed Kimi Code OAuth provider (TUI/model
/// pickers). All managed ids reason, so `GenericOpenAi` is honest here.
pub fn kimi_code_static_catalog_models() -> Vec<CatalogModel> {
    KIMI_CODE_TEXT_MODELS
        .iter()
        .map(|descriptor| CatalogModel {
            provider_key: "kimi-code".into(),
            provider_name: "Kimi Code".into(),
            provider_kind: CatalogProviderKind::Generic {
                key: "kimi-code".into(),
            },
            id: descriptor.id.into(),
            label: Some(descriptor.label.into()),
            context_tokens: descriptor.context_tokens,
            max_output_tokens: None,
            input_modalities: vec![Modality::Text],
            pricing: PricingSummary::default(),
            reasoning: ReasoningSupport::GenericOpenAi,
            source: CatalogSource::StaticFallback,
        })
        .collect()
}

/// Exact wire label for a validated Kimi effort level.
///
/// Only levels present in a capability row's `supported` slice ever reach
/// this; anything else returns `None` and the field is omitted (provider
/// default), never guessed.
fn kimi_effort_label(level: ReasoningLevel) -> Option<&'static str> {
    match level {
        ReasoningLevel::Low => Some("low"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::Max => Some("max"),
        _ => None,
    }
}

/// Apply the documented Kimi reasoning fields to a Chat Completions body.
///
/// Levels are validated upstream by `validation::validate_reasoning_mutation`
/// against the same capability table; this function is deliberately
/// omission-biased for anything unexpected (omitted field = provider
/// default), so it can never send an undocumented value.
pub fn apply_kimi_reasoning_params(
    body: &mut Map<String, Value>,
    model_id: &str,
    level: ReasoningLevel,
) {
    // Static (Moonshot platform) and managed (Kimi Code OAuth) tables are
    // disjoint id namespaces (`kimi-k3` vs `k3`), so this chain can never
    // pick the wrong capability row.
    match kimi_static_capability(model_id).or_else(|| kimi_code_capability(model_id)) {
        Some(KimiReasoningCapability::Effort { supported, .. }) => {
            // Adaptive = provider default via field omission.
            if supported.contains(&level) {
                if let Some(label) = kimi_effort_label(level) {
                    body.insert("reasoning_effort".to_string(), json!(label));
                }
            }
        }
        Some(KimiReasoningCapability::ToggleableThinking) => {
            if level == ReasoningLevel::Off {
                body.insert("thinking".to_string(), json!({ "type": "disabled" }));
            }
            // Everything else: omit → provider default (thinking enabled).
        }
        // Always-thinking models and unknown ids: nothing is expressible on
        // the wire; send no reasoning fields at all.
        Some(KimiReasoningCapability::AlwaysThinking) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReasoningLevel::*;

    #[test]
    fn k3_capability_is_exact_low_high_max_default_max_no_disable() {
        let Some(KimiReasoningCapability::Effort {
            supported,
            default_level,
            can_disable,
        }) = kimi_static_capability("kimi-k3")
        else {
            panic!("kimi-k3 must have documented effort capability");
        };
        assert_eq!(supported, &[Low, High, Max]);
        assert_eq!(default_level, Some(Max));
        assert!(!can_disable);
    }

    #[test]
    fn k27_code_family_is_always_thinking() {
        for id in ["kimi-k2.7-code", "kimi-k2.7-code-highspeed"] {
            assert_eq!(
                kimi_static_capability(id),
                Some(KimiReasoningCapability::AlwaysThinking),
                "{id}"
            );
        }
    }

    #[test]
    fn k26_and_k25_are_toggleable() {
        for id in ["kimi-k2.6", "kimi-k2.5"] {
            assert_eq!(
                kimi_static_capability(id),
                Some(KimiReasoningCapability::ToggleableThinking),
                "{id}"
            );
        }
    }

    #[test]
    fn unknown_ids_fail_closed() {
        assert_eq!(kimi_static_capability("kimi-k9"), None);
        assert_eq!(kimi_static_capability(""), None);
        // Never substring-based.
        assert_eq!(kimi_static_capability("kimi-k3-preview"), None);
    }

    #[test]
    fn k3_body_carries_exact_reasoning_effort() {
        for (level, label) in [(Low, "low"), (High, "high"), (Max, "max")] {
            let mut body = Map::new();
            apply_kimi_reasoning_params(&mut body, "kimi-k3", level);
            assert_eq!(body.get("reasoning_effort"), Some(&json!(label)));
            assert!(!body.contains_key("thinking"));
        }
    }

    #[test]
    fn k3_adaptive_omits_the_field() {
        let mut body = Map::new();
        apply_kimi_reasoning_params(&mut body, "kimi-k3", Adaptive);
        assert!(body.is_empty());
    }

    #[test]
    fn k3_unsupported_levels_are_omitted_never_guessed() {
        // Validation rejects these upstream; the wire layer must still never
        // invent a value for them.
        for level in [Off, Medium, XHigh, Ultra, UltraCode] {
            let mut body = Map::new();
            apply_kimi_reasoning_params(&mut body, "kimi-k3", level);
            assert!(body.is_empty(), "{level:?} must not serialize");
        }
    }

    #[test]
    fn k26_off_disables_thinking_and_other_levels_omit() {
        let mut body = Map::new();
        apply_kimi_reasoning_params(&mut body, "kimi-k2.6", Off);
        assert_eq!(body.get("thinking"), Some(&json!({"type": "disabled"})));
        assert!(!body.contains_key("reasoning_effort"));
        for level in [Adaptive, Low, Medium, High, Max] {
            let mut body = Map::new();
            apply_kimi_reasoning_params(&mut body, "kimi-k2.6", level);
            assert!(body.is_empty(), "{level:?} must omit all fields");
        }
    }

    #[test]
    fn always_thinking_and_unknown_send_nothing() {
        for id in ["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k9"] {
            for level in [Off, Adaptive, Low, High, Max] {
                let mut body = Map::new();
                apply_kimi_reasoning_params(&mut body, id, level);
                assert!(body.is_empty(), "{id} {level:?} must send nothing");
            }
        }
    }

    // ─── Managed Kimi Code (OAuth) catalog ───────────────────────────────────

    #[test]
    fn kimi_code_k3_family_has_effort_low_high_max_default_high_no_disable() {
        for id in ["k3", "k3-256k"] {
            let Some(KimiReasoningCapability::Effort {
                supported,
                default_level,
                can_disable,
            }) = kimi_code_capability(id)
            else {
                panic!("{id} must have managed effort capability");
            };
            assert_eq!(supported, &[Low, High, Max]);
            assert_eq!(default_level, Some(High));
            assert!(!can_disable, "{id} reasoning must not be disableable");
        }
    }

    #[test]
    fn kimi_code_coding_family_is_always_thinking_and_unknown_fails_closed() {
        for id in ["kimi-for-coding", "kimi-for-coding-highspeed"] {
            assert_eq!(
                kimi_code_capability(id),
                Some(KimiReasoningCapability::AlwaysThinking),
                "{id}"
            );
        }
        assert_eq!(kimi_code_capability("k3-preview"), None);
        // Static-platform ids never leak into the managed table.
        assert_eq!(kimi_code_capability("kimi-k3"), None);
        assert_eq!(kimi_code_capability(""), None);
    }

    #[test]
    fn managed_and_static_id_namespaces_are_disjoint() {
        for descriptor in KIMI_CODE_TEXT_MODELS {
            assert!(
                kimi_static_capability(descriptor.id).is_none(),
                "{} must not collide with the static Moonshot table",
                descriptor.id
            );
            assert!(kimi_code_model(descriptor.id).is_some());
        }
    }

    #[test]
    fn managed_k3_body_carries_exact_reasoning_effort_via_shared_apply() {
        for (level, label) in [(Low, "low"), (High, "high"), (Max, "max")] {
            let mut body = Map::new();
            apply_kimi_reasoning_params(&mut body, "k3", level);
            assert_eq!(body.get("reasoning_effort"), Some(&json!(label)));
            assert!(!body.contains_key("thinking"));
        }
        // Adaptive/Off/unknown-levels omit — provider default, never guessed.
        for level in [Adaptive, Off, Medium, XHigh] {
            let mut body = Map::new();
            apply_kimi_reasoning_params(&mut body, "k3-256k", level);
            assert!(body.is_empty(), "{level:?} must not serialize");
        }
    }

    #[test]
    fn managed_always_thinking_sends_nothing() {
        for id in ["kimi-for-coding", "kimi-for-coding-highspeed"] {
            for level in [Off, Adaptive, Low, High, Max] {
                let mut body = Map::new();
                apply_kimi_reasoning_params(&mut body, id, level);
                assert!(body.is_empty(), "{id} {level:?} must send nothing");
            }
        }
    }

    #[test]
    fn kimi_code_catalog_rows_are_managed_and_prefixed_consistently() {
        let models = kimi_code_static_catalog_models();
        assert_eq!(models.len(), KIMI_CODE_TEXT_MODELS.len());
        for model in &models {
            assert_eq!(model.provider_key, "kimi-code");
            assert!(model.context_tokens.is_some());
            assert!(model.label.is_some());
        }
    }
}
