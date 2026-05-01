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
            let Some((handler, hook_bus, tools_shared, streaming, model_tool_use)) = ({
                let manager = manager.read().await;
                manager.provider(&provider_runtime_id).and_then(|provider| {
                    provider.handler.as_ref().map(|handler| {
                        let model_spec = provider.spec.models.iter().find(|m| m.id == model_id);
                        let streaming = model_spec
                            .and_then(|m| m.capabilities.get("streaming"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let model_tool_use = model_spec
                            .and_then(|m| m.capabilities.get("tool_use"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        (handler.clone(), manager.hook_bus().clone(), manager.tools_shared(), streaming, model_tool_use)
                    })
                })
            }) else {
                return Some(Err(format!("Extension provider model '{}' is not available", model).into()));
            };
            // Per-provider trust gate: a disabled provider must not be invoked.
            // The check runs before any IPC and we DO NOT silently fall back to
            // built-in routing — instead return a clear routing error.
            let trust = crate::extensions::trust::load_trust_state().unwrap_or_default();
            if !crate::extensions::trust::is_provider_enabled(&trust, &provider_runtime_id) {
                let _ = crate::extensions::audit::append_audit_entry(
                    &crate::extensions::audit::new_audit_entry(
                        plugin_id,
                        provider_id,
                        model_id,
                        false,
                        0,
                        false,
                        "blocked",
                        Some("trust_disabled".to_string()),
                    ),
                );
                return Some(Err(format!(
                    "Provider '{}' is disabled by user trust settings",
                    provider_runtime_id
                ).into()));
            }
            // Audit metadata captured up-front so each terminal branch can record an entry.
            let audit_plugin = plugin_id.to_string();
            let audit_provider = provider_id.to_string();
            let audit_model = model_id.to_string();
            let tools_exposed = !tools_schema.is_empty();
            let emit_audit = |streamed: bool, outcome: &str, error_class: Option<&str>, tools_requested: u32| {
                let _ = crate::extensions::audit::append_audit_entry(
                    &crate::extensions::audit::new_audit_entry(
                        audit_plugin.clone(),
                        audit_provider.clone(),
                        audit_model.clone(),
                        tools_exposed,
                        tools_requested,
                        streamed,
                        outcome,
                        error_class.map(|s| s.to_string()),
                    ),
                );
            };
            if cancel.is_cancelled() {
                emit_audit(false, "error", Some("canceled"), 0);
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
            let has_active_tools = model_tool_use && !tools_schema.is_empty();
            // Streaming path: forward TextDelta events as LlmEvent::Text deltas in real time.
            if streaming && !has_active_tools {
                let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<crate::extensions::runtime::process::ProviderStreamEvent>();
                let tx_clone = tx.clone();
                let forwarder = tokio::spawn(async move {
                    use crate::extensions::runtime::process::ProviderStreamEvent;
                    while let Some(event) = sink_rx.recv().await {
                        match event {
                            ProviderStreamEvent::TextDelta { text } => {
                                let _ = tx_clone.send(crate::runtime::types::StreamEvent::Llm(
                                    crate::runtime::types::LlmEvent::Text(text)
                                ));
                            }
                            ProviderStreamEvent::ToolUse { .. } => {
                                tracing::warn!("provider.stream tool_use event ignored (streaming tool-use not yet wired)");
                            }
                            // Usage / Done / ThinkingDelta / Error are absorbed; final result aggregates them.
                            _ => {}
                        }
                    }
                });
                let stream_fut = handler.provider_stream(params, sink_tx);
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        forwarder.abort();
                        emit_audit(true, "error", Some("canceled"), 0);
                        return Some(Err("operation canceled".into()));
                    }
                    res = stream_fut => res,
                };
                let _ = forwarder.await;
                if cancel.is_cancelled() {
                    emit_audit(true, "error", Some("canceled"), 0);
                    return Some(Err("operation canceled".into()));
                }
                // TODO(audit): tools_requested is reported as 0 for the streaming
                // path until ProviderStreamEvent::ToolUse is wired through the
                // forwarder; tool-use over streaming is not yet routed.
                match result {
                    Ok(complete) => {
                        emit_audit(true, "ok", None, 0);
                        return Some(Ok(serde_json::json!({
                            "content": complete.content,
                            "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                            "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                        })));
                    }
                    Err(e) => {
                        emit_audit(true, "error", Some("provider_error"), 0);
                        return Some(Err(format!("extension provider: {e}").into()));
                    }
                }
            }
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
                emit_audit(false, "error", Some("canceled"), 0);
                return Some(Err("operation canceled".into()));
            }
            // TODO(audit): tools_requested is reported as 0 here; the
            // complete_provider_with_tools helper does not yet expose its
            // observed tool-use iteration count. Wire that through when the
            // helper grows a return-tuple or counter argument.
            match result {
                Ok(complete) => {
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
                    emit_audit(false, "ok", None, 0);
                    return Some(Ok(serde_json::json!({
                        "content": complete.content,
                        "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                        "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                    })));
                }
                Err(e) => {
                    emit_audit(false, "error", Some("provider_error"), 0);
                    return Some(Err(format!("extension provider: {e}").into()));
                }
            }
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
