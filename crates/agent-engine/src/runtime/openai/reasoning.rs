//! Provider-aware thinking/reasoning request helpers.

use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningProvider {
    OpenRouter,
    Groq,
    NvidiaNim,
    /// Kimi (Moonshot AI): level-driven exact capability wiring
    /// (`catalog::kimi`), not budget-driven inference.
    Kimi,
    Generic,
}

pub fn thinking_level_for_budget(budget: u32) -> &'static str {
    crate::core::models::thinking_level_for_budget(budget)
}

pub fn openai_effort_for_level(level: &str) -> &'static str {
    match level {
        "low" => "low",
        "medium" | "med" => "medium",
        "high" | "xhigh" => "high",
        "adaptive" => "medium",
        _ => "medium",
    }
}

pub fn apply_openai_reasoning_params(
    body: &mut Map<String, Value>,
    provider: OpenAiReasoningProvider,
    model: &str,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
) {
    // Kimi is level-driven, not budget-driven: `Off` must still serialize
    // (`thinking:{"type":"disabled"}` on toggleable models) and `Max` has no
    // numeric budget, so the budget==0 guard below must not apply here.
    // Exact per-model wiring lives in `catalog::kimi`.
    if provider == OpenAiReasoningProvider::Kimi {
        crate::runtime::openai::catalog::apply_kimi_reasoning_params(body, model, reasoning_level);
        return;
    }
    // Don't inject reasoning params when thinking is disabled.
    // Without this guard, non-reasoning models (e.g. llama-3.3) get
    // unsupported fields that cause request failures.
    if thinking_budget == 0 {
        return;
    }
    let level = thinking_level_for_budget(thinking_budget);
    match provider {
        OpenAiReasoningProvider::OpenRouter => {
            let effort = openai_effort_for_level(level);
            body.insert("reasoning".to_string(), json!({ "effort": effort }));
            body.insert("include_reasoning".to_string(), json!(true));
        }
        OpenAiReasoningProvider::Groq => {
            if crate::runtime::openai::catalog::infer_groq_reasoning(model)
                == crate::runtime::openai::catalog::ReasoningSupport::GroqReasoning
            {
                body.insert("reasoning_format".to_string(), json!("parsed"));
                body.insert(
                    "reasoning_effort".to_string(),
                    json!(openai_effort_for_level(level)),
                );
            }
        }
        OpenAiReasoningProvider::Kimi => unreachable!("handled above"),
        OpenAiReasoningProvider::NvidiaNim | OpenAiReasoningProvider::Generic => {}
    }
}

pub fn provider_for_key(provider_key: &str) -> OpenAiReasoningProvider {
    match provider_key {
        "openrouter" => OpenAiReasoningProvider::OpenRouter,
        "groq" => OpenAiReasoningProvider::Groq,
        "nvidia" => OpenAiReasoningProvider::NvidiaNim,
        // Both Kimi routes share the level-driven exact-id wiring in
        // `catalog::kimi` (static Moonshot platform + managed Kimi Code).
        "kimi" | "kimi-code" => OpenAiReasoningProvider::Kimi,
        _ => OpenAiReasoningProvider::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_adds_reasoning_and_include_reasoning() {
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::OpenRouter,
            "deepseek/deepseek-r1",
            4096,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include_reasoning"], true);
    }

    #[test]
    fn groq_adds_reasoning_only_for_reasoning_families() {
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Groq,
            "openai/gpt-oss-120b",
            16_384,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert_eq!(body["reasoning_format"], "parsed");
        assert_eq!(body["reasoning_effort"], "high");

        let mut plain = Map::new();
        apply_openai_reasoning_params(
            &mut plain,
            OpenAiReasoningProvider::Groq,
            "llama-3.3-70b-versatile",
            16_384,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert!(plain.is_empty());
    }

    #[test]
    fn nvidia_and_generic_do_not_emit_unsupported_extra_fields() {
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::NvidiaNim,
            "moonshotai/kimi-k2-thinking",
            4096,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert!(body.is_empty());
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Generic,
            "some/model",
            4096,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert!(body.is_empty());
    }

    #[test]
    fn zero_budget_skips_all_reasoning_params() {
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::OpenRouter,
            "deepseek/deepseek-r1",
            0,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert!(
            body.is_empty(),
            "OpenRouter should not inject reasoning when budget is 0"
        );

        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Groq,
            "openai/gpt-oss-120b",
            0,
            agent_core::reasoning::ReasoningLevel::Medium,
        );
        assert!(
            body.is_empty(),
            "Groq should not inject reasoning when budget is 0"
        );
    }

    #[test]
    fn kimi_key_maps_to_kimi_provider() {
        assert_eq!(provider_for_key("kimi"), OpenAiReasoningProvider::Kimi);
    }

    #[test]
    fn kimi_is_level_driven_not_budget_driven() {
        use agent_core::reasoning::ReasoningLevel;
        // Max has no numeric budget; the level must still reach the wire.
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Kimi,
            "kimi-k3",
            0,
            ReasoningLevel::Max,
        );
        assert_eq!(body["reasoning_effort"], "max");

        // Off carries budget 0 but must still serialize the disable toggle
        // on toggleable models — the zero-budget guard must not swallow it.
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Kimi,
            "kimi-k2.6",
            0,
            ReasoningLevel::Off,
        );
        assert_eq!(body["thinking"]["type"], "disabled");

        // Adaptive omits every reasoning field (provider default).
        let mut body = Map::new();
        apply_openai_reasoning_params(
            &mut body,
            OpenAiReasoningProvider::Kimi,
            "kimi-k3",
            0,
            ReasoningLevel::Adaptive,
        );
        assert!(body.is_empty());
    }
}
