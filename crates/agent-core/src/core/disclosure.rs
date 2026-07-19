//! Task 34 — per-record/event disclosure classes (spec §9.7).
//!
//! One typed vocabulary and ONE model-visibility gate. Every boundary that
//! moves content into model context routes through [`gate_for_model`];
//! persistence boundaries consult [`may_persist`]. Consent and redaction
//! fail CLOSED: no consent → withheld, no redactor → withheld.

use serde::{Deserialize, Serialize};

/// Disclosure class of a record or event (spec §9.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureClass {
    /// Baseline: freely model-visible and persistable.
    #[default]
    ModelVisible,
    /// Display locally only — never enters model context.
    LocalOnly,
    /// Model-visible only AFTER a redactor has been applied.
    ModelVisibleAfterRedaction,
    /// Model-visible only after explicit per-item consent.
    ModelVisibleAfterConsent,
    /// May persist to disk but never transmit (model context included —
    /// model context reaches remote providers).
    PersistNeverTransmit,
    /// Never persisted; visibility itself is not restricted.
    NeverPersist,
}

impl DisclosureClass {
    /// Canonical snake_case name (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            DisclosureClass::ModelVisible => "model_visible",
            DisclosureClass::LocalOnly => "local_only",
            DisclosureClass::ModelVisibleAfterRedaction => "model_visible_after_redaction",
            DisclosureClass::ModelVisibleAfterConsent => "model_visible_after_consent",
            DisclosureClass::PersistNeverTransmit => "persist_never_transmit",
            DisclosureClass::NeverPersist => "never_persist",
        }
    }

    /// Parse a canonical name.
    pub fn parse(s: &str) -> Option<Self> {
        [
            DisclosureClass::ModelVisible,
            DisclosureClass::LocalOnly,
            DisclosureClass::ModelVisibleAfterRedaction,
            DisclosureClass::ModelVisibleAfterConsent,
            DisclosureClass::PersistNeverTransmit,
            DisclosureClass::NeverPersist,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }
}

/// Outcome of the model-visibility gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelVisibility {
    /// This text (possibly redacted) may enter model context.
    Visible(String),
    /// Withheld, with the typed reason boundaries surface as a marker.
    Withheld(&'static str),
}

/// THE model-visibility gate (spec §9.7). `consent_granted` is per-item
/// explicit consent; `redactor` transforms text for the after-redaction
/// class. Missing prerequisites fail closed.
pub fn gate_for_model(
    class: DisclosureClass,
    text: &str,
    consent_granted: bool,
    redactor: Option<&dyn Fn(&str) -> String>,
) -> ModelVisibility {
    match class {
        DisclosureClass::ModelVisible | DisclosureClass::NeverPersist => {
            ModelVisibility::Visible(text.to_string())
        }
        DisclosureClass::LocalOnly => ModelVisibility::Withheld("local_only"),
        DisclosureClass::ModelVisibleAfterRedaction => match redactor {
            Some(redact) => ModelVisibility::Visible(redact(text)),
            None => ModelVisibility::Withheld("redaction required but no redactor configured"),
        },
        DisclosureClass::ModelVisibleAfterConsent => {
            if consent_granted {
                ModelVisibility::Visible(text.to_string())
            } else {
                ModelVisibility::Withheld("explicit consent required")
            }
        }
        DisclosureClass::PersistNeverTransmit => {
            ModelVisibility::Withheld("persist_never_transmit")
        }
    }
}

/// Persistence gate: only [`DisclosureClass::NeverPersist`] refuses disk.
pub fn may_persist(class: DisclosureClass) -> bool {
    class != DisclosureClass::NeverPersist
}
