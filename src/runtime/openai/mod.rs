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

use std::sync::Arc;

use crate::extensions::manager::ExtensionManager;
use crate::extensions::providers::ProviderRegistry;
use crate::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

pub use types::{
    ChatMessage, ChatOptions, ChatRequest, FunctionCall, FunctionDefinition,
    OaiEvent, ProviderConfig, StreamOptions, ToolCall, ToolChoice, ToolDefinition,
};
pub use wire::StreamDecoder;

static EXTENSION_MANAGER: std::sync::RwLock<Option<Arc<tokio::sync::RwLock<ExtensionManager>>>> = std::sync::RwLock::new(None);

pub fn set_extension_manager_for_routing(manager: Arc<tokio::sync::RwLock<ExtensionManager>>) {
    *EXTENSION_MANAGER.write().expect("extension manager routing lock poisoned") = Some(manager);
}

pub fn clear_extension_manager_for_routing() {
    *EXTENSION_MANAGER.write().expect("extension manager routing lock poisoned") = None;
}

pub fn extension_manager_for_routing() -> Option<Arc<tokio::sync::RwLock<ExtensionManager>>> {
    EXTENSION_MANAGER
        .read()
        .expect("extension manager routing lock poisoned")
        .clone()
}

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
    if let Some((plugin_id, provider_id, model_id)) = ProviderRegistry::parse_model_id(model) {
        if let Some(manager) = extension_manager_for_routing() {
            let provider_runtime_id = format!("{}:{}", plugin_id, provider_id);
            let Some((handler, hook_bus, tools_shared)) = ({
                let manager = manager.read().await;
                manager.provider(&provider_runtime_id).and_then(|provider| {
                    provider.handler.as_ref().map(|handler| {
                        (handler.clone(), manager.hook_bus().clone(), manager.tools_shared())
                    })
                })
            }) else {
                return Some(Err(format!("Extension provider model '{}' is not available", model).into()));
            };
            if cancel.is_cancelled() {
                return Some(Err("operation canceled".into()));
            }
            let params = crate::extensions::runtime::process::ProviderCompleteParams {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                model: model.to_string(),
                messages: messages.to_vec(),
                system_prompt: system_prompt.clone(),
                tools: tools_schema.as_ref().clone(),
                temperature,
                max_tokens,
                thinking_budget,
            };
            let result = if let Some(tools) = tools_shared {
                let registry = tools.read().await;
                crate::extensions::runtime::process::complete_provider_with_tools(
                    handler.clone(),
                    params,
                    &registry,
                    &hook_bus,
                    || ToolContext {
                        channels: ToolChannels {
                            tx_delta: None,
                            tx_events: None,
                        },
                        capabilities: ToolCapabilities {
                            watcher_exit_path: None,
                            tool_register_tx: None,
                            session_manager: None,
                            subagent_registry: None,
                            event_queue: None,
                            secret_prompt: None,
                        },
                        limits: ToolLimits {
                            max_tool_output: 30000,
                            bash_timeout: 30,
                            bash_max_timeout: 300,
                            subagent_timeout: 300,
                        },
                    },
                    30000,
                    8,
                ).await
            } else {
                handler.provider_complete(params).await
            };
            if cancel.is_cancelled() {
                return Some(Err("operation canceled".into()));
            }
            return Some(result.map(|complete| {
                let text = complete
                    .content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                        crate::runtime::types::LlmEvent::Text(text)
                    ));
                }
                serde_json::json!({
                    "content": complete.content,
                    "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                    "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                })
            }).map_err(|e| format!("extension provider: {e}").into()));
        }
        return Some(Err(format!("Extension provider model '{}' is not available", model).into()));
    }

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

    #[test]
    fn set_extension_manager_for_routing_overwrites_previous_manager() {
        clear_extension_manager_for_routing();
        let first = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(Arc::new(crate::extensions::hooks::HookBus::new()))));
        let second = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(Arc::new(crate::extensions::hooks::HookBus::new()))));

        set_extension_manager_for_routing(first.clone());
        assert!(Arc::ptr_eq(&extension_manager_for_routing().unwrap(), &first));

        set_extension_manager_for_routing(second.clone());
        assert!(Arc::ptr_eq(&extension_manager_for_routing().unwrap(), &second));

        clear_extension_manager_for_routing();
        assert!(extension_manager_for_routing().is_none());
    }
}
