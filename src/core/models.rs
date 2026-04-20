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

/// Returns true for models that support (and require) adaptive thinking:
/// `thinking: {type: "adaptive"}` with NO `budget_tokens` field.
///
/// Per Anthropic's docs as of 2026-04: Opus 4.6+/Sonnet 4.6+ deprecated
/// the fixed-budget `{type: "enabled", budget_tokens: N}` shape. On those
/// models the deprecated shape is silently accepted but returns no
/// thinking content (observed S172 on Opus 4.7). Older models (Opus 4.5,
/// Sonnet 4.5, Haiku, Opus 3.x) still use the enabled+budget shape.
///
/// Adaptive thinking also auto-enables interleaved thinking — no beta
/// header required.
pub fn model_supports_adaptive_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Parse the version from "claude-<family>-<major>-<minor>"
    // Adaptive required on any Opus 4.6+, Sonnet 4.6+, and assumed on any 5.x+.
    if m.contains("opus-4-6") || m.contains("opus-4-7") || m.contains("opus-4-8") || m.contains("opus-4-9") {
        return true;
    }
    if m.contains("sonnet-4-6") || m.contains("sonnet-4-7") || m.contains("sonnet-4-8") || m.contains("sonnet-4-9") {
        return true;
    }
    // 5.x and beyond — assume adaptive by default.
    if m.contains("opus-5") || m.contains("sonnet-5") || m.contains("haiku-5") {
        return true;
    }
    false
}

/// Returns the input context window size for a given model, in tokens.
/// Used as the denominator for the chatui context-usage bar and anywhere
/// else the client needs to know how much prompt the model will accept.
///
/// Values verified against Anthropic model cards as of 2026-04:
/// - Opus 4.x family: 1M default (S171 limit-test confirmed 270K+ per-turn
///   input succeeded without `CONTEXT_1M_BETA_HEADER` — Anthropic raised the
///   default silently with this generation).
/// - Sonnet 4.x family: 200K default; 1M available via beta header opt-in
///   (we don't send it in api.rs, so the effective limit is 200K).
/// - Haiku (all versions): 200K.
/// - Opus 3.x / unknown models: 200K conservative default.
pub fn context_window_for_model(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("opus-4") || m.contains("opus4") {
        1_000_000
    } else if m.contains("sonnet-4") || m.contains("sonnet4") {
        200_000
    } else if m.contains("haiku") {
        200_000
    } else if m.contains("opus") {
        // Opus 3.x — 200K
        200_000
    } else {
        // Unknown — conservative default.
        200_000
    }
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

    #[test]
    fn context_window_opus4_is_1m() {
        assert_eq!(context_window_for_model("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-5"), 1_000_000);
    }

    #[test]
    fn context_window_sonnet4_is_200k() {
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 200_000);
    }

    #[test]
    fn context_window_haiku_is_200k() {
        assert_eq!(context_window_for_model("claude-haiku-4-5-20251001"), 200_000);
    }

    #[test]
    fn context_window_opus3_is_200k() {
        assert_eq!(context_window_for_model("claude-opus-3-5-20250101"), 200_000);
    }

    #[test]
    fn context_window_unknown_defaults_200k() {
        assert_eq!(context_window_for_model("some-future-model"), 200_000);
        assert_eq!(context_window_for_model(""), 200_000);
    }

    #[test]
    fn context_window_is_case_insensitive() {
        assert_eq!(context_window_for_model("CLAUDE-OPUS-4-7"), 1_000_000);
    }

    #[test]
    fn adaptive_thinking_required_for_opus_4_6_plus() {
        assert!(model_supports_adaptive_thinking("claude-opus-4-6"));
        assert!(model_supports_adaptive_thinking("claude-opus-4-7"));
    }

    #[test]
    fn adaptive_thinking_required_for_sonnet_4_6_plus() {
        assert!(model_supports_adaptive_thinking("claude-sonnet-4-6"));
    }

    #[test]
    fn adaptive_thinking_not_for_older_models() {
        assert!(!model_supports_adaptive_thinking("claude-opus-4-5"));
        assert!(!model_supports_adaptive_thinking("claude-sonnet-4-5"));
        assert!(!model_supports_adaptive_thinking("claude-haiku-4-5-20251001"));
        assert!(!model_supports_adaptive_thinking("claude-opus-3-5"));
    }

    #[test]
    fn adaptive_thinking_assumed_for_5x() {
        assert!(model_supports_adaptive_thinking("claude-opus-5-0"));
        assert!(model_supports_adaptive_thinking("claude-sonnet-5-1"));
    }

    #[test]
    fn adaptive_thinking_case_insensitive() {
        assert!(model_supports_adaptive_thinking("CLAUDE-OPUS-4-7"));
    }
}
