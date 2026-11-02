//! Curated list of Claude models known to work with this CLI.
//! Centralized so the settings dropdown, defaults, and subagent hints agree.

pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-6", "Sonnet 4.6 — balanced (default)"),
    ("claude-fable-5-1", "Fable 5.1"),
    ("claude-fable-5", "Fable 5"),
    ("claude-opus-5-5", "Opus 5.5 — most capable Opus"),
    ("claude-opus-4-7", "Opus 4.7"),
    ("claude-opus-4-6", "Opus 4.6 — previous flagship"),
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
/// Whether an Anthropic model supports tool search, i.e. `defer_loading` tool
/// definitions expanded by `tool_reference` blocks. Anthropic's compatibility
/// table lists Haiku 4.5, Sonnet 4.5+, Opus 4.5+ and every 5.x family; Opus 4.1
/// and earlier (and Sonnet 4 / the 3.x line) do not support it. Unknown future
/// `claude-*` ids are assumed to support it.
pub fn model_supports_tool_search(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let m = m.strip_prefix("anthropic/").unwrap_or(&m);
    if !m.starts_with("claude-") {
        return false;
    }
    const UNSUPPORTED: &[&str] = &[
        "claude-3",
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-2025",
        "claude-sonnet-4-0",
        "claude-sonnet-4-2025",
        "claude-instant",
    ];
    !UNSUPPORTED.iter().any(|p| m.starts_with(p))
}

pub fn model_supports_adaptive_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Only Opus 4.7+ REQUIRES adaptive. Opus 4.6 supports it optionally
    // but works fine with enabled+budget_tokens (and doesn't support the
    // xhigh effort level that adaptive users expect). Keep 4.6 on the
    // legacy path to avoid effort-mapping headaches.
    if m.contains("opus-4-7") || m.contains("opus-4-8") || m.contains("opus-4-9") {
        return true;
    }
    if m.contains("sonnet-4-7") || m.contains("sonnet-4-8") || m.contains("sonnet-4-9") {
        return true;
    }
    // 5.x and beyond — assume adaptive by default.
    if m.contains("opus-5")
        || m.contains("sonnet-5")
        || m.contains("haiku-5")
        || m.contains("fable-5")
    {
        return true;
    }
    false
}

/// Maps a SynapsCLI thinking level to an Anthropic `effort` value for models
/// that use adaptive thinking (Opus 4.6+/Sonnet 4.6+). Effort controls
/// thinking depth when `budget_tokens` is unavailable/deprecated.
///
/// "adaptive" as input means "let the model decide" — returns None so the
/// caller omits `output_config.effort` entirely.
pub fn effort_for_thinking_level(level: &str) -> Option<&'static str> {
    match level {
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "adaptive" => None, // model decides
        _ => Some("high"),  // safe default
    }
}

/// Maps a raw `thinking_budget` value to the user-facing level name.
///
/// `0` is the sentinel for "adaptive" (model decides). Positive values
/// bucket into the four fixed tiers. Single source of truth — consumed by
/// Runtime::thinking_level, the request builders in runtime/api.rs, and
/// the status display.
pub fn thinking_level_for_budget(budget: u32) -> &'static str {
    match budget {
        0 => "adaptive",
        1..=2048 => "low",
        2049..=4096 => "medium",
        4097..=16384 => "high",
        _ => "xhigh",
    }
}

/// Inverse of `thinking_level_for_budget` — maps a level name back to its
/// canonical budget. Returns None for unrecognized labels (callers should
/// leave the current budget untouched). Used by /resume to restore the
/// session's serialized thinking level.
pub fn budget_for_thinking_level(level: &str) -> Option<u32> {
    match level {
        "adaptive" | "off" => Some(0),
        "low" => Some(2048),
        "medium" => Some(4096),
        "high" => Some(16384),
        "xhigh" => Some(32768),
        _ => None,
    }
}

/// Default legacy-model thinking budget used when the "adaptive" sentinel
/// (0) leaks into the non-adaptive request path. Matches the "high" tier.
pub const DEFAULT_LEGACY_ADAPTIVE_FALLBACK: u32 = 16384;

/// Returns true for models that *support* opting into the 1M context window
/// via the `context-1m-2025-08-07` beta header.
///
/// Without that header, all current Claude models operate at the default
/// 200k window — including those documented as "1M tokens", because the
/// extended window is gated behind the beta opt-in. This matches Anthropic's
/// own claude-code logic (`modelSupports1M` in src/utils/context.ts).
pub fn model_supports_1m(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Opus 4.6+, Sonnet 4 family. Newer models (5.x) assumed supported.
    m.contains("opus-4-6")
        || m.contains("opus-4-7")
        || m.contains("opus-4-8")
        || m.contains("opus-4-9")
        || m.contains("sonnet-4")
        || m.contains("opus-5")
        || m.contains("sonnet-5")
        || m.contains("fable-5")
}

/// The extended context window a 1M-capable model opens ([`model_supports_1m`]).
pub const EXTENDED_CONTEXT_WINDOW: u64 = 1_000_000;

/// Whether the Messages API rejects a request whose input plus `max_tokens`
/// exceeds the model's context window.
///
/// Anthropic's context-window docs ("Context window overflow behavior",
/// <https://platform.claude.com/docs/en/build-with-claude/context-windows#context-window-overflow-behavior>):
/// on Claude 4.5 models and newer such a request is accepted, and generation
/// that reaches the window stops with `stop_reason:
/// "model_context_window_exceeded"`; earlier models return a validation
/// error instead (unless the `model-context-window-exceeded-2025-08-26` beta
/// is sent, which Synaps does not). Input alone over the window is a 400 on
/// every model. So `true` only for the pre-4.5 line: Claude 2/Instant, the
/// 3.x line, Opus 4 / 4.1 and Sonnet 4. Haiku 4.5, Sonnet/Opus 4.5 and every
/// later or unknown future `claude-*` id accept overflow, and so does any
/// non-Claude id: no route but the native Anthropic one sends an output cap.
pub fn model_rejects_output_overflow(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let m = m.strip_prefix("anthropic/").unwrap_or(&m);
    const BEFORE_4_5: &[&str] = &[
        "claude-2",
        "claude-instant",
        "claude-3",
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-2025",
        "claude-sonnet-4-0",
        "claude-sonnet-4-2025",
    ];
    matches!(m, "claude-opus-4" | "claude-sonnet-4") || BEFORE_4_5.iter().any(|p| m.starts_with(p))
}

/// Returns the input context window size for a given model, in tokens.
/// Used as the denominator for the chatui context-usage bar and anywhere
/// else the client needs to know how much prompt the model will accept.
///
/// Codex Astra has a native 1,050,000-token window. Other models keep the
/// conservative 200k fallback. In particular, Anthropic models that support
/// a larger window require explicit opt-in via the `context-1m-2025-08-07`
/// beta header; see [`model_supports_1m`].
///
/// An explicit `context_window` setting replaces this native default, down
/// to a user-selected lower cap and up to the model's ceiling; see
/// [`effective_context_window`].
pub fn context_window_for_model(model: &str) -> u64 {
    match model {
        "gpt-6-astra" | "openai-codex/gpt-6-astra" => 1_050_000,
        _ => 200_000,
    }
}

/// The largest input window `model` is known to accept, in tokens, or
/// `None` when it is not known.
///
/// - Codex Astra (the exact ids [`context_window_for_model`] matches):
///   its native 1,050,000.
/// - Anthropic models (`claude-*`, bare or `anthropic/`-qualified):
///   1,000,000 when [`model_supports_1m`], else 200,000.
/// - Every other model (other Codex models, OpenRouter, Groq, NVIDIA,
///   extension and local routes, unknown ids): `None`. The static Codex
///   catalog publishes no window for them, and a route may well accept more
///   than synaps's 200k native default, so an explicit setting is trusted.
pub fn model_context_ceiling(model: &str) -> Option<u64> {
    if matches!(model, "gpt-6-astra" | "openai-codex/gpt-6-astra") {
        return Some(1_050_000);
    }
    let m = model.to_ascii_lowercase();
    let m = m.strip_prefix("anthropic/").unwrap_or(&m);
    if !m.starts_with("claude-") {
        return None;
    }
    Some(if model_supports_1m(m) {
        1_000_000
    } else {
        200_000
    })
}

/// The context window a session on `model` budgets with, in tokens: the
/// model's native window ([`context_window_for_model`]) when nothing is
/// configured, else the configured window clamped to the model's ceiling
/// ([`model_context_ceiling`]). A configured window below the ceiling is
/// kept (a deliberate lower cap); a model with no known ceiling keeps the
/// configured window unchanged. Every reader of a session's window goes
/// through this, so `context_window = 1m` never budgets a 200k model for
/// requests its provider will refuse.
pub fn effective_context_window(model: &str, configured: Option<u64>) -> u64 {
    let Some(tokens) = configured else {
        return context_window_for_model(model);
    };
    match model_context_ceiling(model) {
        Some(ceiling) => tokens.min(ceiling),
        None => tokens,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn astra_context_capacity_is_exact_and_other_models_keep_the_fallback() {
        for model in ["gpt-6-astra", "openai-codex/gpt-6-astra"] {
            assert_eq!(super::context_window_for_model(model), 1_050_000, "{model}");
        }
        for model in [
            "",
            "unknown",
            "gpt-6-astra-wm",
            "gpt-6-astra-preview",
            "other/gpt-6-astra",
            "openai-codex/gpt-6.1-sol",
            "claude-opus-4-7",
        ] {
            assert_eq!(super::context_window_for_model(model), 200_000, "{model}");
        }
    }

    #[test]
    fn tool_search_support_follows_anthropic_compatibility_table() {
        for m in [
            "claude-opus-5-5",
            "anthropic/claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "claude-opus-4-8",
        ] {
            assert!(super::model_supports_tool_search(m), "{m}");
        }
        for m in [
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
            "claude-sonnet-4-20250514",
            "claude-3-7-sonnet-latest",
            "gpt-5.6-sol",
            "groq/llama-3.3-70b-versatile",
        ] {
            assert!(!super::model_supports_tool_search(m), "{m}");
        }
    }

    use super::*;

    #[test]
    fn default_model_is_first_entry() {
        assert_eq!(default_model(), KNOWN_MODELS[0].0);
    }

    #[test]
    fn known_models_has_expected_ids() {
        let ids: Vec<&str> = KNOWN_MODELS.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"claude-fable-5-1"));
        assert!(ids.contains(&"claude-fable-5"));
        assert!(ids.contains(&"claude-opus-5-5"));
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
    fn budget_for_thinking_level_round_trips() {
        for budget in [0u32, 2048, 4096, 16384, 32768] {
            let level = thinking_level_for_budget(budget);
            assert_eq!(
                budget_for_thinking_level(level),
                Some(budget),
                "level {}",
                level
            );
        }
        assert_eq!(budget_for_thinking_level("off"), Some(0));
        assert_eq!(budget_for_thinking_level("custom(8192)"), None);
    }

    #[test]
    fn only_models_before_claude_4_5_reject_output_overflow() {
        for m in [
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
            "claude-opus-4-0",
            "claude-sonnet-4-20250514",
            "anthropic/claude-sonnet-4-0",
            "claude-sonnet-4",
            "claude-3-7-sonnet-latest",
            "claude-3-5-haiku-20241022",
            "CLAUDE-3-OPUS-20240229",
            "claude-2.1",
            "claude-instant-1.2",
        ] {
            assert!(model_rejects_output_overflow(m), "{m}");
        }
        for m in [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-6",
            "claude-opus-4-7",
            "anthropic/claude-opus-5-5",
            "claude-fable-5-1",
            "claude-sonnet-9-0",
            "gpt-5.5",
            "google-gemini/gemini-2.5-pro",
        ] {
            assert!(!model_rejects_output_overflow(m), "{m}");
        }
    }

    #[test]
    fn context_window_default_is_200k_for_all_models() {
        // Without explicit 1M opt-in (context-1m beta), every model is 200k.
        assert_eq!(context_window_for_model("claude-opus-4-7"), 200_000);
        assert_eq!(context_window_for_model("claude-opus-4-6"), 200_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 200_000);
        assert_eq!(
            context_window_for_model("claude-opus-4-5-20251101"),
            200_000
        );
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001"),
            200_000
        );
    }

    #[test]
    fn model_supports_1m_for_opus_46_and_sonnet_4() {
        assert!(model_supports_1m("claude-opus-4-6"));
        assert!(model_supports_1m("claude-opus-4-7"));
        assert!(model_supports_1m("claude-sonnet-4-6"));
        assert!(model_supports_1m("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn model_supports_1m_rejects_older_and_haiku() {
        assert!(!model_supports_1m("claude-opus-4-5"));
        assert!(!model_supports_1m("claude-haiku-4-5-20251001"));
        assert!(!model_supports_1m("claude-opus-3-5"));
    }

    #[test]
    fn model_supports_1m_assumed_for_5x() {
        assert!(model_supports_1m("claude-opus-5-0"));
        assert!(model_supports_1m("claude-sonnet-5-1"));
        assert!(model_supports_1m("claude-fable-5"));
    }

    #[test]
    fn model_supports_1m_case_insensitive() {
        assert!(model_supports_1m("CLAUDE-OPUS-4-6"));
    }

    #[test]
    fn context_window_haiku_is_200k() {
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001"),
            200_000
        );
    }

    #[test]
    fn context_window_opus3_is_200k() {
        assert_eq!(
            context_window_for_model("claude-opus-3-5-20250101"),
            200_000
        );
    }

    #[test]
    fn context_window_unknown_defaults_200k() {
        assert_eq!(context_window_for_model("some-future-model"), 200_000);
        assert_eq!(context_window_for_model(""), 200_000);
    }

    #[test]
    fn context_window_clamp_caps_an_explicit_window_at_the_model_ceiling() {
        // 200k-only Anthropic models: `1m` budgets their real 200k.
        for model in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "anthropic/claude-haiku-4-5-20251001",
            "claude-opus-4-5-20251101",
        ] {
            assert_eq!(model_context_ceiling(model), Some(200_000), "{model}");
            assert_eq!(
                effective_context_window(model, Some(1_000_000)),
                200_000,
                "{model}"
            );
        }
        // 1M-capable Anthropic models keep `1m`; more is capped at 1M.
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-6",
            "CLAUDE-OPUS-4-7",
            "claude-fable-5-1",
        ] {
            assert_eq!(model_context_ceiling(model), Some(1_000_000), "{model}");
            assert_eq!(
                effective_context_window(model, Some(1_000_000)),
                1_000_000,
                "{model}"
            );
            assert_eq!(
                effective_context_window(model, Some(2_000_000)),
                1_000_000,
                "{model}"
            );
        }
        // Astra: `1m` is below its 1.05M ceiling; more is capped at it.
        for model in ["gpt-6-astra", "openai-codex/gpt-6-astra"] {
            assert_eq!(model_context_ceiling(model), Some(1_050_000), "{model}");
            assert_eq!(
                effective_context_window(model, Some(1_000_000)),
                1_000_000,
                "{model}"
            );
            assert_eq!(
                effective_context_window(model, Some(2_000_000)),
                1_050_000,
                "{model}"
            );
        }
        // A deliberate lower cap is kept.
        for model in ["claude-haiku-4-5", "claude-sonnet-4-6", "gpt-6-astra"] {
            assert_eq!(
                effective_context_window(model, Some(100_000)),
                100_000,
                "{model}"
            );
        }
    }

    #[test]
    fn context_window_clamp_keeps_unknown_models_and_the_native_default() {
        // No known ceiling: the configured window is trusted as-is.
        for model in [
            "openrouter/google/gemini-2.5-pro",
            "groq/llama-3.3-70b-versatile",
            "nvidia/llama-3.1-nemotron-ultra",
            "plugin:provider:claude-sonnet-4-6",
            "github-copilot/claude-opus-4.8",
            "local/test-model",
            "openai-codex/gpt-6.1-sol",
            "gpt-6-astra-preview",
            "some-future-model",
            "",
        ] {
            assert_eq!(model_context_ceiling(model), None, "{model}");
            assert_eq!(
                effective_context_window(model, Some(2_000_000)),
                2_000_000,
                "{model}"
            );
            assert_eq!(
                effective_context_window(model, Some(1_000_000)),
                1_000_000,
                "{model}"
            );
        }
        // Nothing configured: the native default, exactly as before.
        for model in [
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
            "gpt-6-astra",
            "openai-codex/gpt-6-astra",
            "openai-codex/gpt-6.1-sol",
            "openrouter/google/gemini-2.5-pro",
        ] {
            assert_eq!(
                effective_context_window(model, None),
                context_window_for_model(model),
                "{model}"
            );
        }
        assert_eq!(effective_context_window("claude-opus-4-7", None), 200_000);
        assert_eq!(effective_context_window("gpt-6-astra", None), 1_050_000);
    }

    #[test]
    fn adaptive_thinking_required_for_opus_4_7_plus() {
        assert!(model_supports_adaptive_thinking("claude-opus-4-7"));
        assert!(model_supports_adaptive_thinking("claude-opus-4-8"));
    }

    #[test]
    fn adaptive_thinking_not_for_opus_4_6() {
        // 4.6 uses enabled+budget_tokens (deprecated but functional)
        assert!(!model_supports_adaptive_thinking("claude-opus-4-6"));
    }

    #[test]
    fn adaptive_thinking_not_for_sonnet_4_6() {
        assert!(!model_supports_adaptive_thinking("claude-sonnet-4-6"));
    }

    #[test]
    fn adaptive_thinking_required_for_sonnet_4_7_plus() {
        assert!(model_supports_adaptive_thinking("claude-sonnet-4-7"));
    }

    #[test]
    fn adaptive_thinking_not_for_older_models() {
        assert!(!model_supports_adaptive_thinking("claude-opus-4-5"));
        assert!(!model_supports_adaptive_thinking("claude-sonnet-4-5"));
        assert!(!model_supports_adaptive_thinking(
            "claude-haiku-4-5-20251001"
        ));
        assert!(!model_supports_adaptive_thinking("claude-opus-3-5"));
    }

    #[test]
    fn adaptive_thinking_assumed_for_5x() {
        assert!(model_supports_adaptive_thinking("claude-opus-5-0"));
        assert!(model_supports_adaptive_thinking("claude-sonnet-5-1"));
    }

    #[test]
    fn opus_5_5_is_a_known_adaptive_1m_capable_model() {
        // Exact wire id is dash-separated, like every other Anthropic id.
        assert!(KNOWN_MODELS.iter().any(|(id, _)| *id == "claude-opus-5-5"));
        assert!(!KNOWN_MODELS.iter().any(|(id, _)| *id == "claude-opus-5.5"));
        assert!(model_supports_adaptive_thinking("claude-opus-5-5"));
        assert!(model_supports_1m("claude-opus-5-5"));
        // Not the default: adding a model must not silently change it.
        assert_ne!(default_model(), "claude-opus-5-5");
    }

    #[test]
    fn adaptive_thinking_case_insensitive() {
        assert!(model_supports_adaptive_thinking("CLAUDE-OPUS-4-7"));
    }
}
