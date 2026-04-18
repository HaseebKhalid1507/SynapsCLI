//! Curated list of Claude models known to work with this CLI.
//! Centralized so the settings dropdown, defaults, and subagent hints agree.

pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-7",           "Opus 4.7 — most capable"),
    ("claude-sonnet-4-6",         "Sonnet 4.6 — balanced"),
    ("claude-haiku-4-5-20251001", "Haiku 4.5 — fast"),
];

pub fn default_model() -> &'static str {
    KNOWN_MODELS[0].0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_first_entry() {
        assert_eq!(default_model(), KNOWN_MODELS[0].0);
    }

    #[test]
    fn known_models_has_expected_ids() {
        let ids: Vec<&str> = KNOWN_MODELS.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-sonnet-4-6"));
        assert!(ids.contains(&"claude-haiku-4-5-20251001"));
    }

    #[test]
    fn descriptions_are_non_empty() {
        for (_, desc) in KNOWN_MODELS {
            assert!(!desc.is_empty(), "empty description");
        }
    }
}
