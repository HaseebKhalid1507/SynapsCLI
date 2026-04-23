//! OpenAI-compatible provider engine.
//!
//! Ported from the `openai-runtime` prototype. Translates between Anthropic-shaped
//! messages/tools/content-blocks (the internal synaps representation) and
//! OpenAI `chat/completions` SSE wire.

use std::collections::BTreeMap;

pub mod types;
pub mod wire;
pub mod registry;
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
}

/// Decide which backend a model should route to.
///
/// - `provider/model` shorthand where `provider` matches a known provider key → `OpenAi`
/// - `claude-*` → `Anthropic`
/// - anything else → `Anthropic` (backward compat)
pub fn resolve_route(model: &str, provider_keys: &BTreeMap<String, String>) -> Provider {
    if let Some((prefix, _rest)) = model.split_once('/') {
        if prefix == "local" || registry::providers().iter().any(|s| s.key == prefix) {
            if let Some(cfg) = registry::resolve_shorthand(model, provider_keys) {
                return Provider::OpenAi(cfg);
            }
        }
    }
    Provider::Anthropic
}
