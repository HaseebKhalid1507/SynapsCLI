//! OpenAI-compatible provider engine.
//!
//! Ported from the `openai-runtime` prototype. Translates between Anthropic-shaped
//! messages/tools/content-blocks (the internal synaps representation) and
//! OpenAI `chat/completions` SSE wire.

use std::collections::BTreeMap;

pub mod types;
pub mod wire;
pub mod registry;
pub mod catalog;
pub mod reasoning;
pub mod translate;
pub mod stream;
pub mod ping;

pub use types::{
    ChatMessage, ChatOptions, ChatRequest, FunctionCall, FunctionDefinition,
    OaiEvent, ProviderConfig, StreamOptions, ToolCall, ToolChoice, ToolDefinition,
};
pub use wire::StreamDecoder;

/// Routing decision for a given model id.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Native Anthropic path (default, backward-compatible).
    Anthropic,
    /// OpenAI-compatible provider with a resolved config.
    OpenAi(ProviderConfig),
    /// ChatGPT subscription-backed Codex responses endpoint.
    Codex(ProviderConfig),
    /// Known provider prefix but no API key configured.
    MissingKey(String),
}

/// Decide which backend a model should route to.
///
/// - `provider/model` shorthand where `provider` matches a known provider key → `OpenAi`
/// - `claude-*` → `Anthropic`
/// - anything else → `Anthropic` (backward compat)
pub fn resolve_route(model: &str, provider_keys: &BTreeMap<String, String>) -> Provider {
    if let Some((prefix, _rest)) = model.split_once('/') {
        if prefix == "openai-codex" {
            if let Some(cfg) = registry::resolve_codex_shorthand(model) {
                return Provider::Codex(cfg);
            }
            return Provider::MissingKey(prefix.to_string());
        }
        if prefix == "local" || registry::providers().iter().any(|s| s.key == prefix) {
            if let Some(cfg) = registry::resolve_shorthand(model, provider_keys) {
                return Provider::OpenAi(cfg);
            }
            // Known provider but no key
            return Provider::MissingKey(prefix.to_string());
        }
    }
    Provider::Anthropic
}

/// Try to route a request through an OpenAI-compatible provider.
///
/// Returns `Some(Ok(value))` if the model resolved to an OpenAI provider and the
/// request completed (successfully or with error). Returns `None` if the model
/// should be handled by the Anthropic path.
///
/// This is the single routing entry point — both streaming and non-streaming
/// callers in `api.rs` use this instead of duplicating the routing logic.
pub async fn try_route(
    model: &str,
    client: &reqwest::Client,
    tools_schema: &std::sync::Arc<Vec<serde_json::Value>>,
    system_prompt: &Option<String>,
    messages: &[serde_json::Value],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    let provider_keys = crate::core::config::get_provider_keys();
    match resolve_route(model, &provider_keys) {
        Provider::OpenAi(cfg) => {
            let result = stream::call_oai_stream_inner(
                &cfg, client, tools_schema, system_prompt, messages, tx,
                temperature, max_tokens, thinking_budget, cancel,
            ).await;
            Some(result)
        }
        Provider::Codex(cfg) => {
            let result = stream::call_codex_stream_inner(
                &cfg, client, tools_schema, system_prompt, messages, tx,
                temperature, max_tokens, cancel,
            ).await;
            Some(result)
        }
        Provider::Anthropic => None,
        Provider::MissingKey(provider) => {
            Some(Err(format!(
                "No API key for '{}'. Set provider.{} in ~/.synaps-cli/config or the corresponding env var.",
                provider, provider
            ).into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_openai_codex_without_requiring_eager_credentials() {
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        match resolve_route("openai-codex/gpt-5.1-codex-mini", &BTreeMap::new()) {
            Provider::Codex(cfg) => {
                assert_eq!(cfg.provider, "openai-codex");
                assert_eq!(cfg.model, "gpt-5.1-codex-mini");
                assert!(cfg.base_url.contains("chatgpt.com/backend-api"));
            }
            other => panic!("expected Codex route, got {other:?}"),
        }
    }
}
