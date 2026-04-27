//! Provider-aware thinking/reasoning request helpers.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningProvider {
    OpenRouter,
    Groq,
    NvidiaNim,
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
) {
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
                body.insert("reasoning_effort".to_string(), json!(openai_effort_for_level(level)));
            }
        }
        OpenAiReasoningProvider::NvidiaNim | OpenAiReasoningProvider::Generic => {}
    }
}

pub fn provider_for_key(provider_key: &str) -> OpenAiReasoningProvider {
    match provider_key {
        "openrouter" => OpenAiReasoningProvider::OpenRouter,
        "groq" => OpenAiReasoningProvider::Groq,
        "nvidia" => OpenAiReasoningProvider::NvidiaNim,
        _ => OpenAiReasoningProvider::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_adds_reasoning_and_include_reasoning() {
        let mut body = Map::new();
        apply_openai_reasoning_params(&mut body, OpenAiReasoningProvider::OpenRouter, "deepseek/deepseek-r1", 4096);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include_reasoning"], true);
    }

    #[test]
    fn groq_adds_reasoning_only_for_reasoning_families() {
        let mut body = Map::new();
        apply_openai_reasoning_params(&mut body, OpenAiReasoningProvider::Groq, "openai/gpt-oss-120b", 16_384);
        assert_eq!(body["reasoning_format"], "parsed");
        assert_eq!(body["reasoning_effort"], "high");

        let mut plain = Map::new();
        apply_openai_reasoning_params(&mut plain, OpenAiReasoningProvider::Groq, "llama-3.3-70b-versatile", 16_384);
        assert!(plain.is_empty());
    }

    #[test]
    fn nvidia_and_generic_do_not_emit_unsupported_extra_fields() {
        let mut body = Map::new();
        apply_openai_reasoning_params(&mut body, OpenAiReasoningProvider::NvidiaNim, "moonshotai/kimi-k2-thinking", 4096);
        assert!(body.is_empty());
        apply_openai_reasoning_params(&mut body, OpenAiReasoningProvider::Generic, "some/model", 4096);
        assert!(body.is_empty());
    }
}
