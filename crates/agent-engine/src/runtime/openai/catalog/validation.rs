//! Shared mutation-time reasoning validation and dynamic option derivation.
//!
//! Single source of truth for "may this provider-qualified model accept this
//! `ReasoningLevel`?" — used by `Runtime::set_reasoning_level_checked` (slash
//! commands, settings) and by dynamic TUI option derivation. Capability data
//! comes from the process-local capability cache (live catalogs) first, then
//! the exact static descriptor tables. No model-name substring inference:
//! dispatch keys on the provider-qualified id prefix only (bare `claude-*`
//! ids are Anthropic routing parity with `resolve_route`).

use agent_core::reasoning::ReasoningLevel;

use super::{
    anthropic_static_capability, capability_cache, codex_static_capability, kimi_code_capability,
    kimi_static_capability, plan_codex_execution, xai_static_capability, CatalogProviderKind,
    CodexMultiAgentVersion, CodexRequestRole, KimiReasoningCapability, ReasoningSupport,
    XaiReasoningCapability,
};

/// Conservative option set for providers without authoritative exact-model
/// metadata. Never includes max/ultra.
const CONSERVATIVE_OPTIONS: &[&str] = &["off", "adaptive", "low", "medium", "high", "xhigh"];

/// Capability lookup shared by both Kimi routes: the static Moonshot-platform
/// provider (`kimi/`) and the managed Kimi Code OAuth provider (`kimi-code/`).
/// Outer `None` = not a Kimi route; inner `None` = unknown exact id (fail
/// closed at the caller).
fn kimi_route_capability(model: &str) -> Option<Option<KimiReasoningCapability>> {
    if let Some(model_id) = model.strip_prefix("kimi/") {
        return Some(kimi_static_capability(model_id));
    }
    if let Some(model_id) = model.strip_prefix("kimi-code/") {
        return Some(kimi_code_capability(model_id));
    }
    None
}

fn anthropic_model_id(model: &str) -> Option<&str> {
    model
        .strip_prefix("anthropic/")
        .or_else(|| model.starts_with("claude-").then_some(model))
}

fn unsupported_msg(level: ReasoningLevel, model: &str, supported: &[ReasoningLevel]) -> String {
    format!(
        "reasoning level '{level}' is not supported by {model}; supported: [{}]",
        supported
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn codex_reasoning_capability(model: &str, model_id: &str) -> Option<ReasoningSupport> {
    // A live exact row is authoritative as a whole. In particular, never
    // borrow static v2 evidence when that live row omitted or contradicted it.
    if let Some(model) = capability_cache::get(model) {
        if model.provider_kind != CatalogProviderKind::OpenAiCodex {
            return None;
        }
        return matches!(model.reasoning, ReasoningSupport::CodexNamed { .. })
            .then_some(model.reasoning);
    }
    codex_static_capability(model_id)
}

fn codex_supported_levels(model: &str, model_id: &str) -> Option<Vec<ReasoningLevel>> {
    match codex_reasoning_capability(model, model_id)? {
        ReasoningSupport::CodexNamed {
            mut supported,
            multi_agent_version,
            ..
        } => {
            if multi_agent_version != Some(CodexMultiAgentVersion::V2) {
                supported.retain(|level| *level != ReasoningLevel::Ultra);
            }
            Some(supported)
        }
        _ => None,
    }
}

fn anthropic_capability(model_id: &str) -> Option<ReasoningSupport> {
    // A live catalog entry whose payload carried no capability evidence
    // (`Unknown`) must not shadow the exact-id static table — absence of
    // live metadata is not evidence against static knowledge.
    capability_cache::get(&format!("anthropic/{model_id}"))
        .map(|m| m.reasoning)
        .filter(|r| !matches!(r, ReasoningSupport::Unknown))
        .or_else(|| anthropic_static_capability(model_id))
}

/// Validate `level` for the provider-qualified `model` at mutation time
/// (command/settings). `Err` carries a user-facing message; callers must not
/// mutate state or persist config on `Err`.
pub fn validate_reasoning_mutation(model: &str, level: ReasoningLevel) -> Result<(), String> {
    if let Some(model_id) = model.strip_prefix("openai-codex/") {
        // Off/Adaptive omit the provider effort field on the Codex wire.
        if matches!(level, ReasoningLevel::Off | ReasoningLevel::Adaptive) {
            return Ok(());
        }
        if matches!(level, ReasoningLevel::Max | ReasoningLevel::Ultra) {
            return plan_codex_execution(model, level, CodexRequestRole::Foreground, None)
                .map(|_| ())
                .map_err(|error| error.to_string());
        }
        return match codex_supported_levels(model, model_id) {
            Some(levels) if levels.contains(&level) => Ok(()),
            Some(levels) => Err(unsupported_msg(level, model, &levels)),
            // Unknown Codex model: metadata absence never authorizes the
            // extended max/ultra modes (fail closed).
            None if level.requires_codex_support() => Err(format!(
                "no capability metadata for {model}; cannot authorize level '{level}'"
            )),
            None => Ok(()),
        };
    }
    if let Some(model_id) = model.strip_prefix("xai-auth/") {
        // Adaptive = provider default (omit `reasoning`) — always expressible.
        if level == ReasoningLevel::Adaptive {
            return Ok(());
        }
        return match xai_static_capability(model_id) {
            Some(XaiReasoningCapability::Effort {
                supported,
                can_disable,
                ..
            }) => match level {
                ReasoningLevel::Off if can_disable => Ok(()),
                ReasoningLevel::Off => Err(format!(
                    "reasoning cannot be disabled on {model}; use adaptive or a supported effort"
                )),
                l if supported.contains(&l) => Ok(()),
                l => Err(unsupported_msg(l, model, supported)),
            },
            Some(XaiReasoningCapability::IntrinsicReasoning) => match level {
                ReasoningLevel::Off => Err(format!(
                    "{model} has no documented way to disable reasoning; use adaptive"
                )),
                l => Err(format!(
                    "{model} has no documented effort control; level '{l}' cannot be sent — use adaptive"
                )),
            },
            Some(XaiReasoningCapability::NonReasoning) => match level {
                ReasoningLevel::Off => Ok(()),
                l => Err(format!(
                    "{model} is a non-reasoning model; level '{l}' is not supported"
                )),
            },
            None => Err(format!(
                "no capability metadata for {model}; cannot authorize level '{level}'"
            )),
        };
    }
    if let Some(capability) = kimi_route_capability(model) {
        // Adaptive = provider default (omit the field) — always expressible.
        if level == ReasoningLevel::Adaptive {
            return Ok(());
        }
        return match capability {
            Some(KimiReasoningCapability::Effort {
                supported,
                can_disable,
                ..
            }) => match level {
                ReasoningLevel::Off if can_disable => Ok(()),
                ReasoningLevel::Off => Err(format!(
                    "reasoning cannot be disabled on {model}; use adaptive or a supported effort"
                )),
                l if supported.contains(&l) => Ok(()),
                l => Err(unsupported_msg(l, model, supported)),
            },
            Some(KimiReasoningCapability::AlwaysThinking) => match level {
                ReasoningLevel::Off => Err(format!(
                    "{model} always thinks and the API rejects disabling; use adaptive"
                )),
                l => Err(format!(
                    "{model} has no documented effort control; level '{l}' cannot be sent — use adaptive"
                )),
            },
            Some(KimiReasoningCapability::ToggleableThinking) => match level {
                ReasoningLevel::Off => Ok(()),
                l => Err(format!(
                    "{model} supports only a thinking on/off toggle; level '{l}' cannot be sent — use off or adaptive"
                )),
            },
            None => Err(format!(
                "no capability metadata for {model}; cannot authorize level '{level}'"
            )),
        };
    }
    if let Some(model_id) = anthropic_model_id(model) {
        if matches!(level, ReasoningLevel::Max | ReasoningLevel::UltraCode) {
            // Max/UltraCode require an exact qualified Anthropic capability row;
            // bare/unknown Anthropic identities must fail closed.
            let capabilities = model
                .starts_with("anthropic/")
                .then(|| super::anthropic_mode_capabilities(model))
                .flatten()
                .map(|caps| {
                    caps.narrow_with_live_effort(capability_cache::get(model).and_then(|entry| {
                        match entry.reasoning {
                            ReasoningSupport::AnthropicAdaptive { adaptive } => Some(adaptive),
                            ReasoningSupport::None => Some(false),
                            _ => None,
                        }
                    }))
                });
            let supported = match level {
                ReasoningLevel::Max => capabilities.is_some_and(|caps| caps.max_supported),
                ReasoningLevel::UltraCode => {
                    capabilities.is_some_and(|caps| caps.ultracode_supported())
                }
                _ => unreachable!(),
            };
            return supported
                .then_some(())
                .ok_or_else(|| format!("reasoning level '{level}' is not supported by {model}"));
        }
        if level == ReasoningLevel::Ultra {
            return Err(format!(
                "reasoning level '{level}' requires authoritative exact-model capability metadata; {model} has none"
            ));
        }
        // Live catalog evidence that thinking is unsupported fails closed for
        // named levels; Off/Adaptive stay expressible (field omission).
        if matches!(anthropic_capability(model_id), Some(ReasoningSupport::None))
            && !matches!(level, ReasoningLevel::Off | ReasoningLevel::Adaptive)
        {
            return Err(format!(
                "{model} does not support extended thinking; level '{level}' is not available"
            ));
        }
        return Ok(());
    }
    // Other providers: only the Codex-extended modes are gated.
    if level.requires_codex_support() {
        return Err(format!(
            "reasoning level '{level}' requires authoritative exact-model capability metadata; {model} has none"
        ));
    }
    Ok(())
}

/// Model-default reasoning level applied on model switch when the user has
/// not explicitly chosen a level. `None` = leave the current level untouched.
pub fn default_level_for_model(model: &str) -> Option<ReasoningLevel> {
    if let Some(model_id) = model.strip_prefix("openai-codex/") {
        return match codex_reasoning_capability(model, model_id)? {
            ReasoningSupport::CodexNamed { default_level, .. } => default_level,
            _ => None,
        };
    }
    if let Some(model_id) = model.strip_prefix("xai-auth/") {
        return Some(match xai_static_capability(model_id) {
            Some(XaiReasoningCapability::Effort {
                default_level: Some(level),
                ..
            }) => level,
            // No documented default / no effort control / unknown id →
            // provider default via field omission.
            _ => ReasoningLevel::Adaptive,
        });
    }
    if let Some(capability) = kimi_route_capability(model) {
        return Some(match capability {
            Some(KimiReasoningCapability::Effort {
                default_level: Some(level),
                ..
            }) => level,
            // Toggle/always-thinking/unknown → provider default via omission.
            _ => ReasoningLevel::Adaptive,
        });
    }
    None
}

/// Human-readable reasoning *type* for the settings UI, derived from the
/// same exact capability sources as `thinking_options_for_model` (capability
/// cache first, then exact static descriptors). Never claims a verified type
/// for providers without authoritative metadata.
pub fn reasoning_type_for_model(model: &str) -> &'static str {
    if model.strip_prefix("openai-codex/").is_some() {
        // Codex always expresses reasoning as named effort on the wire.
        return "effort (named)";
    }
    if let Some(model_id) = model.strip_prefix("xai-auth/") {
        return match xai_static_capability(model_id) {
            Some(XaiReasoningCapability::Effort { .. }) => "effort",
            Some(XaiReasoningCapability::IntrinsicReasoning) => "intrinsic",
            Some(XaiReasoningCapability::NonReasoning) => "none",
            None => "unknown",
        };
    }
    if let Some(capability) = kimi_route_capability(model) {
        return match capability {
            Some(KimiReasoningCapability::Effort { .. }) => "effort",
            Some(KimiReasoningCapability::AlwaysThinking) => "intrinsic",
            Some(KimiReasoningCapability::ToggleableThinking) => "toggle (on/off)",
            None => "unknown",
        };
    }
    if let Some(model_id) = anthropic_model_id(model) {
        return match anthropic_capability(model_id) {
            Some(ReasoningSupport::AnthropicAdaptive { adaptive: true }) => "adaptive",
            Some(ReasoningSupport::None) => "none",
            // Explicit non-adaptive metadata or no metadata: the legacy
            // enabled+budget_tokens request shape.
            _ => "budget (legacy)",
        };
    }
    "unverified"
}

/// Dynamic thinking options for the settings UI, derived from exact
/// catalog/static capabilities for the provider-qualified model id.
pub fn thinking_options_for_model(model: &str) -> Vec<String> {
    let owned = |slice: &[&str]| slice.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Some(model_id) = model.strip_prefix("openai-codex/") {
        if let Some(levels) = codex_supported_levels(model, model_id) {
            let mut opts = vec!["off".to_string(), "adaptive".to_string()];
            opts.extend(levels.iter().map(|l| l.as_str().to_string()));
            return opts;
        }
        return owned(CONSERVATIVE_OPTIONS);
    }
    if let Some(model_id) = model.strip_prefix("xai-auth/") {
        return match xai_static_capability(model_id) {
            Some(XaiReasoningCapability::Effort {
                supported,
                can_disable,
                ..
            }) => {
                let mut opts = Vec::new();
                if can_disable {
                    opts.push("off".to_string());
                }
                opts.push("adaptive".to_string());
                opts.extend(supported.iter().map(|l| l.as_str().to_string()));
                opts
            }
            Some(XaiReasoningCapability::NonReasoning) => owned(&["off", "adaptive"]),
            // Intrinsic reasoning without effort control, or unknown id:
            // only the provider default is expressible.
            _ => owned(&["adaptive"]),
        };
    }
    if let Some(capability) = kimi_route_capability(model) {
        return match capability {
            Some(KimiReasoningCapability::Effort {
                supported,
                can_disable,
                ..
            }) => {
                let mut opts = Vec::new();
                if can_disable {
                    opts.push("off".to_string());
                }
                opts.push("adaptive".to_string());
                opts.extend(supported.iter().map(|l| l.as_str().to_string()));
                opts
            }
            Some(KimiReasoningCapability::ToggleableThinking) => owned(&["off", "adaptive"]),
            // Always-thinking without effort control, or unknown id: only
            // the provider default is expressible.
            _ => owned(&["adaptive"]),
        };
    }
    if let Some(model_id) = anthropic_model_id(model) {
        let live = model
            .starts_with("anthropic/")
            .then(|| capability_cache::get(model))
            .flatten()
            .map(|entry| entry.reasoning);
        let specials_authorized = super::anthropic_mode_capabilities(model).is_some()
            && !matches!(live, Some(ReasoningSupport::None));
        return match anthropic_capability(model_id) {
            // Thinking-capable (adaptive effort or legacy budget tiers). Generic
            // live true preserves exact static authority but cannot invent it.
            Some(ReasoningSupport::AnthropicAdaptive { .. }) if specials_authorized => owned(&[
                "off",
                "adaptive",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode",
            ]),
            Some(ReasoningSupport::AnthropicAdaptive { .. }) => owned(CONSERVATIVE_OPTIONS),
            // Explicit live false revokes both special modes.
            Some(ReasoningSupport::None) => owned(&["off", "adaptive"]),
            _ if specials_authorized => owned(&[
                "off",
                "adaptive",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode",
            ]),
            _ => owned(CONSERVATIVE_OPTIONS),
        };
    }
    owned(CONSERVATIVE_OPTIONS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReasoningLevel::*;

    #[test]
    fn fable_options_expose_distinct_special_modes() {
        let model = "anthropic/claude-fable-5";
        assert_eq!(
            thinking_options_for_model(model),
            [
                "off",
                "adaptive",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode"
            ]
        );
    }

    // ── xAI mutation matrix ──────────────────────────────────────────────────

    #[test]
    fn xai_45_and_46_accept_exact_efforts_and_reject_off_xhigh_max_ultra() {
        for model in [
            "xai-auth/grok-4.5",
            "xai-auth/grok-4.5-latest",
            "xai-auth/grok-4.6",
        ] {
            for level in [Adaptive, Low, Medium, High] {
                assert!(
                    validate_reasoning_mutation(model, level).is_ok(),
                    "{model} {level}"
                );
            }
            for level in [Off, XHigh, Max, Ultra] {
                let err = validate_reasoning_mutation(model, level).unwrap_err();
                assert!(err.contains(model), "{err}");
            }
        }
    }

    #[test]
    fn xai_multi_agent_accepts_xhigh_but_not_off() {
        let model = "xai-auth/grok-4.20-multi-agent-0309";
        for level in [Adaptive, Low, Medium, High, XHigh] {
            assert!(validate_reasoning_mutation(model, level).is_ok(), "{level}");
        }
        for level in [Off, Max, Ultra] {
            assert!(
                validate_reasoning_mutation(model, level).is_err(),
                "{level}"
            );
        }
    }

    #[test]
    fn xai_intrinsic_reasoning_rejects_named_and_off_accepts_adaptive() {
        for id in [
            "grok-4.3",
            "grok-4.3-latest",
            "grok-latest",
            "grok-4.20-0309-reasoning",
        ] {
            let model = format!("xai-auth/{id}");
            assert!(validate_reasoning_mutation(&model, Adaptive).is_ok());
            for level in [Off, Low, Medium, High, XHigh, Max, Ultra] {
                assert!(
                    validate_reasoning_mutation(&model, level).is_err(),
                    "{model} {level}"
                );
            }
        }
    }

    #[test]
    fn xai_non_reasoning_rejects_named_accepts_off_adaptive() {
        let model = "xai-auth/grok-4.20-0309-non-reasoning";
        assert!(validate_reasoning_mutation(model, Off).is_ok());
        assert!(validate_reasoning_mutation(model, Adaptive).is_ok());
        for level in [Low, Medium, High, XHigh, Max, Ultra] {
            assert!(
                validate_reasoning_mutation(model, level).is_err(),
                "{level}"
            );
        }
    }

    #[test]
    fn xai_unknown_id_fails_closed_except_adaptive() {
        let model = "xai-auth/grok-9000";
        assert!(validate_reasoning_mutation(model, Adaptive).is_ok());
        for level in [Off, Low, Medium, High, XHigh, Max, Ultra] {
            assert!(
                validate_reasoning_mutation(model, level).is_err(),
                "{level}"
            );
        }
    }

    // ── Codex gap fix ────────────────────────────────────────────────────────

    #[test]
    fn unknown_codex_model_rejects_max_and_ultra_at_mutation_time() {
        for level in [Max, Ultra] {
            let err =
                validate_reasoning_mutation("openai-codex/gpt-unknown-future", level).unwrap_err();
            assert!(err.contains("no capability metadata"), "{err}");
        }
        // Non-extended named levels remain permissive without metadata.
        for level in [Off, Adaptive, Low, Medium, High, XHigh] {
            assert!(
                validate_reasoning_mutation("openai-codex/gpt-unknown-future", level).is_ok(),
                "{level}"
            );
        }
    }

    #[test]
    fn known_codex_model_membership_still_enforced() {
        assert!(validate_reasoning_mutation("openai-codex/gpt-5.6-sol", Ultra).is_ok());
        assert!(validate_reasoning_mutation("openai-codex/gpt-5.5", Ultra).is_err());
    }

    #[test]
    fn live_codex_ultra_requires_v2_at_mutation_and_in_options() {
        use super::super::{
            CatalogModel, CatalogProviderKind, CatalogSource, CodexMultiAgentVersion,
        };

        for (suffix, version) in [("missing", None), ("v1", Some(CodexMultiAgentVersion::V1))] {
            let id = format!("gpt-ultra-validation-{suffix}");
            let qualified = format!("openai-codex/{id}");
            let mut live = CatalogModel::new("openai-codex", "OpenAI Codex", &id).unwrap();
            live.provider_kind = CatalogProviderKind::OpenAiCodex;
            live.source = CatalogSource::Live;
            live.reasoning = ReasoningSupport::CodexNamed {
                supported: vec![Low, Medium, High, XHigh, Max, Ultra],
                default_level: Some(High),
                multi_agent_version: version,
            };
            capability_cache::insert(live);

            assert!(
                validate_reasoning_mutation(&qualified, Max).is_ok(),
                "Max does not require v2: {qualified}"
            );
            let err = validate_reasoning_mutation(&qualified, Ultra)
                .expect_err("Ultra must fail closed without exact v2 evidence");
            assert!(err.contains("multi-agent v2"), "{err}");

            let options = thinking_options_for_model(&qualified);
            assert!(options.contains(&"max".to_string()), "{options:?}");
            assert!(!options.contains(&"ultra".to_string()), "{options:?}");
        }
    }

    // ── Anthropic ────────────────────────────────────────────────────────────

    #[test]
    fn anthropic_accepts_budget_expressible_levels_and_rejects_extended() {
        for model in ["anthropic/claude-opus-4-7", "claude-sonnet-4-6"] {
            for level in [Off, Adaptive, Low, Medium, High, XHigh] {
                assert!(
                    validate_reasoning_mutation(model, level).is_ok(),
                    "{model} {level}"
                );
            }
            for level in [Max, Ultra, UltraCode] {
                assert!(
                    validate_reasoning_mutation(model, level).is_err(),
                    "{model} {level}"
                );
            }
        }
    }

    #[test]
    fn anthropic_live_no_thinking_evidence_rejects_named() {
        let mut m = super::super::CatalogModel::new(
            "anthropic",
            "Anthropic",
            "claude-test-validation-nothink",
        )
        .unwrap();
        m.provider_kind = super::super::CatalogProviderKind::Anthropic;
        m.reasoning = ReasoningSupport::None;
        m.source = super::super::CatalogSource::Live;
        capability_cache::insert(m);
        let model = "anthropic/claude-test-validation-nothink";
        for level in [Off, Adaptive] {
            assert!(validate_reasoning_mutation(model, level).is_ok(), "{level}");
        }
        for level in [Low, Medium, High, XHigh] {
            assert!(
                validate_reasoning_mutation(model, level).is_err(),
                "{level}"
            );
        }
    }

    // ── Defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn default_levels_come_from_exact_descriptors() {
        assert_eq!(default_level_for_model("xai-auth/grok-4.5"), Some(High));
        assert_eq!(
            default_level_for_model("xai-auth/grok-4.5-latest"),
            Some(High)
        );
        assert_eq!(default_level_for_model("xai-auth/grok-4.6"), Some(High));
        assert_eq!(
            default_level_for_model("xai-auth/grok-4.20-multi-agent-0309"),
            Some(Adaptive)
        );
        assert_eq!(default_level_for_model("xai-auth/grok-4.3"), Some(Adaptive));
        assert_eq!(
            default_level_for_model("xai-auth/grok-4.20-0309-non-reasoning"),
            Some(Adaptive)
        );
        assert_eq!(default_level_for_model("anthropic/claude-opus-4-7"), None);
        assert_eq!(
            default_level_for_model("openai-codex/gpt-5.3-codex-spark"),
            Some(High)
        );
    }

    // ── Dynamic options ──────────────────────────────────────────────────────

    #[test]
    fn xai_options_derive_from_exact_capabilities() {
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.5"),
            vec!["adaptive", "low", "medium", "high"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.6"),
            vec!["adaptive", "low", "medium", "high"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.20-multi-agent-0309"),
            vec!["adaptive", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.3"),
            vec!["adaptive"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-4.20-0309-non-reasoning"),
            vec!["off", "adaptive"]
        );
        assert_eq!(
            thinking_options_for_model("xai-auth/grok-9000"),
            vec!["adaptive"]
        );
    }

    // ── Reasoning type derivation (settings display) ─────────────────────────

    #[test]
    fn reasoning_type_derives_from_exact_capabilities() {
        // Codex: named effort levels on the wire.
        assert_eq!(
            reasoning_type_for_model("openai-codex/gpt-5.6-sol"),
            "effort (named)"
        );
        assert_eq!(
            reasoning_type_for_model("openai-codex/gpt-unknown-future"),
            "effort (named)"
        );
        // xAI: documented effort control / intrinsic / non-reasoning.
        assert_eq!(reasoning_type_for_model("xai-auth/grok-4.5"), "effort");
        assert_eq!(reasoning_type_for_model("xai-auth/grok-4.6"), "effort");
        assert_eq!(reasoning_type_for_model("xai-auth/grok-4.3"), "intrinsic");
        assert_eq!(
            reasoning_type_for_model("xai-auth/grok-4.20-0309-non-reasoning"),
            "none"
        );
        assert_eq!(reasoning_type_for_model("xai-auth/grok-9000"), "unknown");
        // Anthropic: adaptive effort vs legacy numeric budget.
        assert_eq!(
            reasoning_type_for_model("anthropic/claude-opus-4-7"),
            "adaptive"
        );
        assert_eq!(
            reasoning_type_for_model("anthropic/claude-sonnet-4-6"),
            "budget (legacy)"
        );
        assert_eq!(
            reasoning_type_for_model("claude-sonnet-4-6"),
            "budget (legacy)"
        );
        // No authoritative metadata: legacy budget path, honestly labelled.
        assert_eq!(
            reasoning_type_for_model("anthropic/claude-future-x"),
            "budget (legacy)"
        );
        // Other providers without exact metadata never claim a verified type.
        assert_eq!(
            reasoning_type_for_model("groq/llama-3.3-70b-versatile"),
            "unverified"
        );
    }

    /// Regression: a live catalog entry parsed WITHOUT capability metadata
    /// (`ReasoningSupport::Unknown`, e.g. from pagination payloads) must not
    /// shadow the exact-id static table.
    #[test]
    fn cached_unknown_reasoning_does_not_shadow_static_capability() {
        let mut m =
            super::super::CatalogModel::new("anthropic", "Anthropic", "claude-opus-4-7").unwrap();
        m.reasoning = crate::runtime::openai::catalog::ReasoningSupport::Unknown;
        capability_cache::insert(m);
        assert_eq!(
            reasoning_type_for_model("anthropic/claude-opus-4-7"),
            "adaptive",
            "cached Unknown must fall through to the static adaptive entry"
        );
    }

    #[test]
    fn reasoning_type_none_for_live_no_thinking_anthropic_evidence() {
        let mut m = super::super::CatalogModel::new(
            "anthropic",
            "Anthropic",
            "claude-test-reasoning-type-nothink",
        )
        .unwrap();
        m.provider_kind = super::super::CatalogProviderKind::Anthropic;
        m.reasoning = ReasoningSupport::None;
        m.source = super::super::CatalogSource::Live;
        capability_cache::insert(m);
        assert_eq!(
            reasoning_type_for_model("anthropic/claude-test-reasoning-type-nothink"),
            "none"
        );
    }

    #[test]
    fn anthropic_options_derive_from_capabilities() {
        assert_eq!(
            thinking_options_for_model("anthropic/claude-opus-4-7"),
            vec!["off", "adaptive", "low", "medium", "high", "xhigh"]
        );
        // Fixed-budget models keep the budget-tier set.
        assert_eq!(
            thinking_options_for_model("claude-sonnet-4-6"),
            vec!["off", "adaptive", "low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn options_never_advertise_max_ultra_off_codex() {
        for model in [
            "anthropic/claude-opus-4-7",
            "xai-auth/grok-4.5",
            "groq/llama-3.3-70b-versatile",
        ] {
            let opts = thinking_options_for_model(model);
            assert!(!opts.contains(&"max".to_string()), "{model}");
            assert!(!opts.contains(&"ultra".to_string()), "{model}");
        }
    }

    #[test]
    fn kimi_k3_accepts_exact_efforts_and_rejects_off_medium_xhigh_ultra() {
        let model = "kimi/kimi-k3";
        for level in [Adaptive, Low, High, Max] {
            assert!(
                validate_reasoning_mutation(model, level).is_ok(),
                "{model} {level}"
            );
        }
        for level in [Off, Medium, XHigh, Ultra, UltraCode] {
            let err = validate_reasoning_mutation(model, level).unwrap_err();
            assert!(err.contains("kimi"), "{err}");
        }
    }

    #[test]
    fn kimi_always_thinking_models_accept_only_adaptive() {
        for model in ["kimi/kimi-k2.7-code", "kimi/kimi-k2.7-code-highspeed"] {
            assert!(
                validate_reasoning_mutation(model, Adaptive).is_ok(),
                "{model}"
            );
            for level in [Off, Low, Medium, High, Max] {
                assert!(
                    validate_reasoning_mutation(model, level).is_err(),
                    "{model} {level}"
                );
            }
        }
    }

    #[test]
    fn kimi_toggleable_models_accept_off_and_adaptive_only() {
        for model in ["kimi/kimi-k2.6", "kimi/kimi-k2.5"] {
            for level in [Off, Adaptive] {
                assert!(
                    validate_reasoning_mutation(model, level).is_ok(),
                    "{model} {level}"
                );
            }
            for level in [Low, Medium, High, Max] {
                assert!(
                    validate_reasoning_mutation(model, level).is_err(),
                    "{model} {level}"
                );
            }
        }
    }

    #[test]
    fn kimi_unknown_ids_fail_closed_except_adaptive() {
        let model = "kimi/kimi-k9000";
        assert!(validate_reasoning_mutation(model, Adaptive).is_ok());
        for level in [Off, Low, Medium, High, Max, Ultra] {
            assert!(
                validate_reasoning_mutation(model, level).is_err(),
                "{level}"
            );
        }
    }

    #[test]
    fn kimi_options_are_exact_per_model() {
        assert_eq!(
            thinking_options_for_model("kimi/kimi-k3"),
            vec!["adaptive", "low", "high", "max"]
        );
        for model in ["kimi/kimi-k2.7-code", "kimi/kimi-k2.7-code-highspeed"] {
            assert_eq!(
                thinking_options_for_model(model),
                vec!["adaptive"],
                "{model}"
            );
        }
        for model in ["kimi/kimi-k2.6", "kimi/kimi-k2.5"] {
            assert_eq!(
                thinking_options_for_model(model),
                vec!["off", "adaptive"],
                "{model}"
            );
        }
        assert_eq!(
            thinking_options_for_model("kimi/kimi-k9000"),
            vec!["adaptive"]
        );
    }

    #[test]
    fn kimi_reasoning_types_and_default_level() {
        assert_eq!(reasoning_type_for_model("kimi/kimi-k3"), "effort");
        assert_eq!(reasoning_type_for_model("kimi/kimi-k2.7-code"), "intrinsic");
        assert_eq!(
            reasoning_type_for_model("kimi/kimi-k2.6"),
            "toggle (on/off)"
        );
        assert_eq!(reasoning_type_for_model("kimi/kimi-k9000"), "unknown");
        assert_eq!(
            default_level_for_model("kimi/kimi-k3"),
            Some(ReasoningLevel::Max)
        );
        assert_eq!(
            default_level_for_model("kimi/kimi-k2.6"),
            Some(ReasoningLevel::Adaptive)
        );
    }
}
