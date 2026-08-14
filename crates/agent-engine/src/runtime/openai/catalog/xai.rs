//! Selectable xAI model IDs documented for public text/Responses use.
//!
//! This is a conservative allowlist, not an exhaustive xAI model list. Aliases
//! are first-class selectable IDs when xAI documents them as valid model IDs.
//! Build/code and image, video, and voice models are excluded because the
//! available public documentation does not establish public Responses support
//! for this text runtime.

use super::{
    CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary, ReasoningSupport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XaiModelDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub context_tokens: Option<u64>,
    pub reasoning: bool,
}

pub const XAI_TEXT_MODELS: &[XaiModelDescriptor] = &[
    XaiModelDescriptor {
        id: "grok-4.3",
        label: "Grok 4.3",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.3-latest",
        label: "Grok 4.3 (latest alias)",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-latest",
        label: "Grok (latest alias)",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.20-0309-reasoning",
        label: "Grok 4.20 Reasoning",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.20-0309-non-reasoning",
        label: "Grok 4.20 Non-Reasoning",
        context_tokens: None,
        reasoning: false,
    },
    XaiModelDescriptor {
        id: "grok-4.20-multi-agent-0309",
        label: "Grok 4.20 Multi-Agent",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.5",
        label: "Grok 4.5",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.5-latest",
        label: "Grok 4.5 (latest alias)",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.6",
        label: "Grok 4.6",
        context_tokens: None,
        reasoning: true,
    },
    XaiModelDescriptor {
        id: "grok-4.6-latest",
        label: "Grok 4.6 (latest alias)",
        context_tokens: None,
        reasoning: true,
    },
];

pub fn xai_model(id: &str) -> Option<&'static XaiModelDescriptor> {
    XAI_TEXT_MODELS.iter().find(|model| model.id == id)
}

// ─── Exact-id reasoning capability (spec: anthropic-xai-reasoning-modes) ─────

/// Documented reasoning capability for an exact xAI model id.
///
/// Evidence: official xAI docs. `grok-4.5`/`grok-4.5-latest` and
/// `grok-4.6`/`grok-4.6-latest` support low/medium/high effort, default high,
/// and reasoning cannot be disabled. `grok-4.20-multi-agent-0309` supports
/// low/medium/high/xhigh where effort controls agent count. No other exact id
/// has documented effort support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XaiReasoningCapability {
    /// Documented named-effort control (`reasoning:{effort:"..."}` on the
    /// Responses wire).
    Effort {
        supported: &'static [agent_core::reasoning::ReasoningLevel],
        default_level: Option<agent_core::reasoning::ReasoningLevel>,
        /// Whether reasoning can be disabled. `false` → `Off` must be
        /// rejected, never silently omitted/downgraded.
        can_disable: bool,
    },
    /// Reasoning is intrinsic; no documented effort control or disable switch.
    IntrinsicReasoning,
    /// Model does not reason; named reasoning levels must be rejected.
    NonReasoning,
}

/// Exact-id capability lookup — never substring-based. Unknown ids return
/// `None` and fail closed at validation time.
pub fn xai_static_capability(model_id: &str) -> Option<XaiReasoningCapability> {
    use agent_core::reasoning::ReasoningLevel::*;
    match model_id {
        "grok-4.5" | "grok-4.5-latest" | "grok-4.6" | "grok-4.6-latest" => {
            Some(XaiReasoningCapability::Effort {
                supported: &[Low, Medium, High],
                default_level: Some(High),
                can_disable: false,
            })
        }
        "grok-4.20-multi-agent-0309" => Some(XaiReasoningCapability::Effort {
            supported: &[Low, Medium, High, XHigh],
            // Effort controls agent count; no documented default level.
            default_level: None,
            can_disable: false,
        }),
        "grok-4.3" | "grok-4.3-latest" | "grok-latest" | "grok-4.20-0309-reasoning" => {
            Some(XaiReasoningCapability::IntrinsicReasoning)
        }
        "grok-4.20-0309-non-reasoning" => Some(XaiReasoningCapability::NonReasoning),
        _ => None,
    }
}

pub fn xai_static_catalog_models() -> Vec<CatalogModel> {
    XAI_TEXT_MODELS
        .iter()
        .map(|descriptor| CatalogModel {
            provider_key: "xai-auth".into(),
            provider_name: "xAI (Grok)".into(),
            provider_kind: CatalogProviderKind::Generic {
                key: "xai-auth".into(),
            },
            id: descriptor.id.into(),
            label: Some(descriptor.label.into()),
            context_tokens: descriptor.context_tokens,
            max_output_tokens: None,
            input_modalities: vec![Modality::Text],
            pricing: PricingSummary::default(),
            reasoning: if descriptor.reasoning {
                ReasoningSupport::GenericOpenAi
            } else {
                ReasoningSupport::None
            },
            source: CatalogSource::StaticFallback,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_selectable_public_responses_text_ids_are_exposed() {
        assert_eq!(
            XAI_TEXT_MODELS.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![
                "grok-4.3",
                "grok-4.3-latest",
                "grok-latest",
                "grok-4.20-0309-reasoning",
                "grok-4.20-0309-non-reasoning",
                "grok-4.20-multi-agent-0309",
                "grok-4.5",
                "grok-4.5-latest",
                "grok-4.6",
                "grok-4.6-latest",
            ]
        );
    }

    #[test]
    fn reasoning_metadata_is_limited_to_documented_reasoning_models() {
        let reasoning_ids = xai_static_catalog_models()
            .into_iter()
            .filter(|model| model.reasoning != ReasoningSupport::None)
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(
            reasoning_ids,
            vec![
                "grok-4.3",
                "grok-4.3-latest",
                "grok-latest",
                "grok-4.20-0309-reasoning",
                "grok-4.20-multi-agent-0309",
                "grok-4.5",
                "grok-4.5-latest",
                "grok-4.6",
                "grok-4.6-latest",
            ]
        );
    }

    // ── Exact-id reasoning capability table (spec: anthropic-xai-reasoning-modes) ──

    #[test]
    fn grok_45_and_46_family_effort_capability_is_exact() {
        use agent_core::reasoning::ReasoningLevel::*;
        for id in ["grok-4.5", "grok-4.5-latest", "grok-4.6", "grok-4.6-latest"] {
            match xai_static_capability(id) {
                Some(XaiReasoningCapability::Effort {
                    supported,
                    default_level,
                    can_disable,
                }) => {
                    assert_eq!(supported, &[Low, Medium, High], "{id}");
                    assert_eq!(default_level, Some(High), "{id}");
                    assert!(!can_disable, "{id}: reasoning cannot be disabled");
                }
                other => panic!("{id}: expected Effort capability, got {other:?}"),
            }
        }
    }

    #[test]
    fn multi_agent_supports_xhigh_but_45_does_not() {
        use agent_core::reasoning::ReasoningLevel::*;
        match xai_static_capability("grok-4.20-multi-agent-0309") {
            Some(XaiReasoningCapability::Effort {
                supported,
                default_level,
                can_disable,
            }) => {
                assert_eq!(supported, &[Low, Medium, High, XHigh]);
                assert_eq!(default_level, None, "no documented default");
                assert!(!can_disable);
            }
            other => panic!("expected Effort capability, got {other:?}"),
        }
        // 4.5/4.6 have no documented xhigh — must not appear in their sets.
        for id in ["grok-4.5", "grok-4.6"] {
            if let Some(XaiReasoningCapability::Effort { supported, .. }) =
                xai_static_capability(id)
            {
                assert!(!supported.contains(&XHigh), "{id}");
            }
        }
    }

    #[test]
    fn intrinsic_reasoning_models_have_no_named_effort() {
        for id in [
            "grok-4.3",
            "grok-4.3-latest",
            "grok-latest",
            "grok-4.20-0309-reasoning",
        ] {
            assert_eq!(
                xai_static_capability(id),
                Some(XaiReasoningCapability::IntrinsicReasoning),
                "{id}"
            );
        }
    }

    #[test]
    fn non_reasoning_model_is_marked_non_reasoning() {
        assert_eq!(
            xai_static_capability("grok-4.20-0309-non-reasoning"),
            Some(XaiReasoningCapability::NonReasoning)
        );
    }

    #[test]
    fn unknown_exact_ids_fail_closed() {
        for id in ["grok-5", "grok-4.50", "grok-4.5x", "", "gpt-5.5"] {
            assert_eq!(xai_static_capability(id), None, "{id}");
        }
    }

    #[test]
    fn catalog_is_text_only_and_excludes_unconfirmed_code_model() {
        let models = xai_static_catalog_models();
        assert!(models
            .iter()
            .all(|m| m.input_modalities == vec![Modality::Text]));
        assert!(!models.iter().any(|m| m.id == "grok-build-0.1"));
        assert!(!models.iter().any(|m| m.id == "grok-build-latest"));
        assert!(!models.iter().any(|m| ["image", "video", "voice"]
            .iter()
            .any(|kind| m.id.contains(kind))));
    }
}
