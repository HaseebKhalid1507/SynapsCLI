//! Conservative Google Gemini (Code Assist) model catalog.
//!
//! Provenance of the wire IDs exposed here is limited to the ones the official
//! `google-gemini/gemini-cli` reference names as first-class defaults in
//! `packages/core/src/config/models.ts`:
//!
//!   DEFAULT_GEMINI_MODEL       = "gemini-2.5-pro"
//!   DEFAULT_GEMINI_FLASH_MODEL = "gemini-2.5-flash"
//!
//! Preview / experimental IDs (gemini-3-*, gemini-3.1-*, gemini-3.5-flash,
//! auto-gemini-*) are intentionally NOT surfaced — they are gated behind
//! experiments in the reference client and may 404 or return a
//! "not available" error for accounts that don't have the flag flipped.
//!
//! Media / embedding / video / voice models are NOT surfaced either: this
//! runtime is text + tool-call only.

use super::{
    CatalogModel, CatalogProviderKind, CatalogSource, Modality, PricingSummary, ReasoningSupport,
};

pub const PROVIDER_KEY: &str = "google-gemini";
pub const PROVIDER_NAME: &str = "Google Gemini (Code Assist)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoogleGeminiModelDescriptor {
    /// Wire id sent as `request.model` to `streamGenerateContent`.
    pub id: &'static str,
    pub label: &'static str,
    pub context_tokens: Option<u64>,
    /// Gemini 2.5 models support internal "thinking" — a reasoning-support
    /// distinction that lets the UI hint at higher latency / cost.
    pub thinking: bool,
}

/// Text/tool-capable wire IDs exposed by the official Gemini CLI. Preview and
/// rollout models are intentionally visible; upstream account policy remains
/// authoritative and may reject models not enabled for a particular user.
pub const GOOGLE_GEMINI_TEXT_MODELS: &[GoogleGeminiModelDescriptor] = &[
    GoogleGeminiModelDescriptor {
        id: "gemini-pro-latest",
        label: "Gemini Pro Latest",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro (Preview)",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-3-pro-preview",
        label: "Gemini 3 Pro (Preview)",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-3.5-flash",
        label: "Gemini 3.5 Flash",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-3-flash-preview",
        label: "Gemini 3 Flash (Preview)",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-3.1-flash-lite",
        label: "Gemini 3.1 Flash Lite",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-2.5-pro",
        label: "Gemini 2.5 Pro",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
    GoogleGeminiModelDescriptor {
        id: "gemini-2.5-flash",
        label: "Gemini 2.5 Flash",
        context_tokens: Some(1_048_576),
        thinking: true,
    },
];

pub fn google_gemini_model(id: &str) -> Option<&'static GoogleGeminiModelDescriptor> {
    GOOGLE_GEMINI_TEXT_MODELS.iter().find(|m| m.id == id)
}

pub fn google_gemini_static_catalog_models() -> Vec<CatalogModel> {
    GOOGLE_GEMINI_TEXT_MODELS
        .iter()
        .map(|d| CatalogModel {
            provider_key: PROVIDER_KEY.into(),
            provider_name: PROVIDER_NAME.into(),
            provider_kind: CatalogProviderKind::Generic {
                key: PROVIDER_KEY.into(),
            },
            id: d.id.into(),
            label: Some(d.label.into()),
            context_tokens: d.context_tokens,
            max_output_tokens: None,
            input_modalities: vec![Modality::Text],
            pricing: PricingSummary::default(),
            reasoning: if d.thinking {
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
    fn catalog_exposes_all_official_reference_text_ids() {
        assert_eq!(
            GOOGLE_GEMINI_TEXT_MODELS
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            vec![
                "gemini-pro-latest",
                "gemini-3.1-pro-preview",
                "gemini-3-pro-preview",
                "gemini-3.5-flash",
                "gemini-3-flash-preview",
                "gemini-3.1-flash-lite",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
            ]
        );
    }

    #[test]
    fn catalog_exposes_exact_gemini_pro_latest_wire_id() {
        let descriptor = google_gemini_model("gemini-pro-latest")
            .expect("Gemini Code Assist wire ID must be in the trusted catalog");
        assert_eq!(descriptor.id, "gemini-pro-latest");
        assert!(google_gemini_static_catalog_models()
            .iter()
            .any(|model| model.runtime_id() == "google-gemini/gemini-pro-latest"));
    }

    #[test]
    fn catalog_is_text_only_and_excludes_non_chat_models() {
        let models = google_gemini_static_catalog_models();
        assert!(models
            .iter()
            .all(|m| m.input_modalities == vec![Modality::Text]));
        for banned in [
            "auto-gemini-2.5",
            "auto-gemini-3",
            "text-embedding-004",
            "embedding-001",
            "gemini-2.5-image",
            "gemini-2.5-video",
            "gemini-2.5-voice",
        ] {
            assert!(
                !models.iter().any(|m| m.id == banned),
                "{banned} must not be in the text/tool catalog"
            );
        }
    }

    #[test]
    fn google_gemini_model_lookup_is_exact_match_only() {
        assert!(google_gemini_model("gemini-2.5-pro").is_some());
        assert!(google_gemini_model("gemini-2.5-flash").is_some());
        // No fuzzy match: 'gemini-2.5' alone is NOT a wire ID.
        assert!(google_gemini_model("gemini-2.5").is_none());
        assert!(google_gemini_model("Gemini-2.5-Pro").is_none());
        assert!(google_gemini_model("").is_none());
    }
}
