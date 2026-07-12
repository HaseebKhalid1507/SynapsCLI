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
];

pub fn xai_model(id: &str) -> Option<&'static XaiModelDescriptor> {
    XAI_TEXT_MODELS.iter().find(|model| model.id == id)
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
            ]
        );
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
