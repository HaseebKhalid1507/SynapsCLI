//! OpenAI-compatible provider engine.
//!
//! Ported from the `openai-runtime` prototype. Translates between Anthropic-shaped
//! messages/tools/content-blocks (the internal synaps representation) and
//! OpenAI `chat/completions` SSE wire.

pub mod catalog;
pub mod net;
pub mod ping;
pub mod reasoning;
pub mod registry;
pub mod stream;
pub mod translate;
pub mod types;
pub mod wire;

use std::sync::Arc;

use crate::extensions::manager::ExtensionManager;
use crate::extensions::providers::ProviderRegistry;
use crate::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

pub use types::{
    ChatMessage, ChatOptions, ChatRequest, FunctionCall, FunctionDefinition, OaiEvent,
    ProviderConfig, StreamOptions, ToolCall, ToolChoice, ToolDefinition,
};
pub use wire::StreamDecoder;

static EXTENSION_MANAGER: std::sync::RwLock<Option<Arc<tokio::sync::RwLock<ExtensionManager>>>> =
    std::sync::RwLock::new(None);

pub fn set_extension_manager_for_routing(manager: Arc<tokio::sync::RwLock<ExtensionManager>>) {
    *EXTENSION_MANAGER
        .write()
        .expect("extension manager routing lock poisoned") = Some(manager);
}

pub fn clear_extension_manager_for_routing() {
    *EXTENSION_MANAGER
        .write()
        .expect("extension manager routing lock poisoned") = None;
}

pub fn extension_manager_for_routing() -> Option<Arc<tokio::sync::RwLock<ExtensionManager>>> {
    EXTENSION_MANAGER
        .read()
        .expect("extension manager routing lock poisoned")
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPolicy {
    OAuthAccessToken(crate::auth::OAuthProviderId),
    BrokerProxy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    CodexResponses,
    /// Google Gemini Code Assist v1internal:streamGenerateContent envelope.
    /// Broker-proxied only.
    GoogleGeminiCodeAssist,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub endpoint: String,
    pub model: String,
    pub provider: String,
    pub auth: AuthPolicy,
    pub wire: WireProtocol,
}

/// Resolve typed credential and wire dimensions. Unknown explicit prefixes fail closed.
pub fn resolve_route(model: &str) -> Option<ResolvedRoute> {
    if let Some((prefix, rest)) = model.split_once('/') {
        if prefix == "anthropic" && rest.starts_with("claude-") {
            return Some(ResolvedRoute {
                endpoint: "https://api.anthropic.com".into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::OAuthAccessToken(crate::auth::OAuthProviderId::Anthropic),
                wire: WireProtocol::AnthropicMessages,
            });
        }
        if prefix == "xai-auth" && catalog::xai_model(rest).is_some() {
            return Some(ResolvedRoute {
                endpoint: "https://api.x.ai/v1".into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::OAuthAccessToken(crate::auth::OAuthProviderId::Xai),
                wire: WireProtocol::OpenAiResponses,
            });
        }
        if prefix == "github-copilot" {
            let descriptor = catalog::github_copilot_runtime_model(rest)?;
            return Some(ResolvedRoute {
                endpoint: catalog::MODELS_BASE_URL.into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::BrokerProxy,
                wire: descriptor,
            });
        }
        if prefix == "google-gemini" {
            // Broker-proxied only: broker owns the Google OAuth access token
            // and pins cloudcode-pa.googleapis.com. Wire IDs are limited to
            // the conservative catalog.
            catalog::google_gemini_model(rest)?;
            return Some(ResolvedRoute {
                endpoint: crate::auth::broker::GOOGLE_GEMINI_CODE_ASSIST_BASE_URL.into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::BrokerProxy,
                wire: WireProtocol::GoogleGeminiCodeAssist,
            });
        }
        if prefix == "openai-codex" {
            let c = registry::resolve_codex_shorthand(model)?;
            return Some(ResolvedRoute {
                endpoint: c.base_url,
                model: c.model,
                provider: c.provider,
                auth: AuthPolicy::OAuthAccessToken(crate::auth::OAuthProviderId::OpenAiCodex),
                wire: WireProtocol::CodexResponses,
            });
        }
        if prefix == "local" {
            let c = registry::resolve_provider_model(prefix, rest)?;
            return Some(ResolvedRoute {
                endpoint: c.base_url,
                model: c.model,
                provider: c.provider,
                auth: AuthPolicy::BrokerProxy,
                wire: WireProtocol::OpenAiChatCompletions,
            });
        }
        if let Some(spec) = registry::providers().iter().find(|s| s.key == prefix) {
            return Some(ResolvedRoute {
                endpoint: spec.base_url.into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::BrokerProxy,
                wire: WireProtocol::OpenAiChatCompletions,
            });
        }
        return None;
    }
    Some(ResolvedRoute {
        endpoint: "https://api.anthropic.com".into(),
        model: model.into(),
        provider: "anthropic".into(),
        auth: AuthPolicy::OAuthAccessToken(crate::auth::OAuthProviderId::Anthropic),
        wire: WireProtocol::AnthropicMessages,
    })
}
fn provider_config(r: &ResolvedRoute) -> ProviderConfig {
    ProviderConfig {
        base_url: r.endpoint.clone(),
        model: r.model.clone(),
        provider: r.provider.clone(),
    }
}

/// Try to route a request through an OpenAI-compatible provider.
///
/// Returns `Some(Ok(value))` if the model resolved to an OpenAI provider and the
/// request completed (successfully or with error). Returns `None` if the model
/// should be handled by the Anthropic path.
///
/// This is the single routing entry point — both streaming and non-streaming
/// callers in `api.rs` use this instead of duplicating the routing logic.
#[allow(clippy::too_many_arguments)]
pub async fn try_route(
    model: &str,
    client: &reqwest::Client,
    tools_schema: &std::sync::Arc<Vec<serde_json::Value>>,
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    cancel: &tokio_util::sync::CancellationToken,
    source: &crate::auth::CredentialSource,
    cache: &crate::auth::TokenCache,
    max_retries: u32,
    codex_request_role: catalog::CodexRequestRole,
    trace: &crate::runtime::trace::TraceContext,
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
                        (
                            handler.clone(),
                            manager.hook_bus().clone(),
                            manager.tools_shared(),
                            streaming,
                            model_tool_use,
                        )
                    })
                })
            }) else {
                return Some(Err(format!(
                    "Extension provider model '{}' is not available",
                    model
                )
                .into()));
            };
            // Per-provider trust gate: a disabled provider must not be invoked.
            // The check runs before any IPC and we DO NOT silently fall back to
            // built-in routing — instead return a clear routing error.
            let trust = match crate::extensions::trust::load_trust_state() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("trust.json corrupt or unreadable, failing closed: {e}");
                    return Some(Err(format!(
                        "Cannot route to provider: trust state unreadable: {e}"
                    )
                    .into()));
                }
            };
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
                )
                .into()));
            }
            // Audit metadata captured up-front so each terminal branch can record an entry.
            let audit_plugin = plugin_id.to_string();
            let audit_provider = provider_id.to_string();
            let audit_model = model_id.to_string();
            let tools_exposed = !tools_schema.is_empty();
            let emit_audit =
                |streamed: bool, outcome: &str, error_class: Option<&str>, tools_requested: u32| {
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
                // Arc-shared (#128): refcount bumps, not a deep copy. Serde's
                // `rc` feature serializes Arc<Value> transparently, so the
                // wire shape crossing the extension boundary is unchanged.
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
                let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<
                    crate::extensions::runtime::process::ProviderStreamEvent,
                >();
                let tx_clone = tx.clone();
                let forwarder = tokio::spawn(async move {
                    use crate::extensions::runtime::process::ProviderStreamEvent;
                    while let Some(event) = sink_rx.recv().await {
                        match event {
                            ProviderStreamEvent::TextDelta { text } => {
                                let _ = tx_clone.send(crate::runtime::types::StreamEvent::Llm(
                                    crate::runtime::types::LlmEvent::Text(text),
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
                            orchestration: None,
                        },
                        limits: ToolLimits {
                            max_tool_output: 30000,
                            max_tool_buffer: 256 * 1024,
                            bash_timeout: 30,
                            bash_max_timeout: 300,
                            subagent_timeout: 300,
                        },
                    },
                    30000,
                    8,
                )
                .await
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
                            crate::runtime::types::LlmEvent::Text(text),
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
        return Some(Err(format!(
            "Extension provider model '{}' is not available",
            model
        )
        .into()));
    }

    let Some(route) = resolve_route(model) else {
        return Some(Err(
            format!("Unknown provider route for model '{model}'").into()
        ));
    };
    let cfg = provider_config(&route);
    let broker = crate::auth::broker_from_source(source, cache, client.clone());
    // Trace honesty (Task 10A): only a local (in-process) broker sends the
    // exact bytes this process serialized (`ProxyRequest::body_bytes`); a
    // remote broker daemon re-serializes upstream bodies out of process.
    let exact_wire_bytes = !source.is_remote();
    match route.wire {
        WireProtocol::OpenAiChatCompletions => Some(
            stream::call_oai_stream_inner(
                &cfg,
                &broker,
                tools_schema,
                system_prompt,
                messages,
                tx,
                temperature,
                max_tokens,
                thinking_budget,
                cancel,
                trace,
                exact_wire_bytes,
            )
            .await,
        ),
        WireProtocol::OpenAiResponses => Some(
            stream::call_xai_responses_stream_inner(
                &cfg,
                &broker,
                tools_schema,
                system_prompt,
                messages,
                tx,
                max_tokens,
                reasoning_level,
                cancel,
                trace,
                exact_wire_bytes,
            )
            .await,
        ),
        WireProtocol::CodexResponses => Some(
            stream::call_codex_stream_inner(
                &cfg,
                client,
                &broker,
                tools_schema,
                system_prompt,
                messages,
                tx,
                temperature,
                max_tokens,
                reasoning_level,
                codex_request_role,
                cancel,
                // Codex transport rides out chatgpt.com edge bursts with the
                // same persistent posture as Anthropic OAuth overloads (10
                // retries), not the generic three-attempt budget.
                stream::codex_retry_budget(max_retries),
                trace,
            )
            .await,
        ),
        WireProtocol::AnthropicMessages => None,
        WireProtocol::GoogleGeminiCodeAssist => Some(
            crate::runtime::google_gemini::runtime::call_google_gemini_stream_inner(
                &cfg,
                &broker,
                tools_schema,
                system_prompt,
                messages,
                tx,
                cancel,
            )
            .await,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_identity_is_explicit_while_legacy_bare_ids_still_route() {
        for id in ["claude-sonnet-4-6", "claude-opus-4-7"] {
            let qualified = resolve_route(&format!("anthropic/{id}"))
                .expect("qualified Anthropic model must route");
            assert_eq!(qualified.provider, "anthropic");
            assert_eq!(qualified.model, id);
            assert_eq!(qualified.wire, WireProtocol::AnthropicMessages);

            let legacy = resolve_route(id).expect("legacy bare Claude model must route");
            assert_eq!(legacy.provider, "anthropic");
            assert_eq!(legacy.model, id);
        }
        assert_eq!(
            resolve_route("github-copilot/claude-sonnet-4.6")
                .unwrap()
                .provider,
            "github-copilot"
        );
    }

    #[test]
    fn brokered_provider_routing_is_typed_and_fail_closed() {
        for descriptor in catalog::XAI_TEXT_MODELS {
            let runtime_id = format!("xai-auth/{}", descriptor.id);
            let xai = resolve_route(&runtime_id).unwrap();
            assert_eq!(xai.model, descriptor.id);
            assert_eq!(xai.provider, "xai-auth");
            assert_eq!(xai.endpoint, "https://api.x.ai/v1");
            assert_eq!(
                xai.auth,
                AuthPolicy::OAuthAccessToken(crate::auth::OAuthProviderId::Xai)
            );
            assert_eq!(xai.wire, WireProtocol::OpenAiResponses);
        }
        let static_route = resolve_route("groq/llama-3.3-70b-versatile").unwrap();
        assert_eq!(static_route.auth, AuthPolicy::BrokerProxy);
        assert_eq!(static_route.wire, WireProtocol::OpenAiChatCompletions);

        let copilot_chat = resolve_route("github-copilot/claude-sonnet-4.6").unwrap();
        assert_eq!(copilot_chat.auth, AuthPolicy::BrokerProxy);
        assert_eq!(copilot_chat.wire, WireProtocol::OpenAiChatCompletions);
        let copilot_responses = resolve_route("github-copilot/gpt-5.3-codex").unwrap();
        assert_eq!(copilot_responses.auth, AuthPolicy::BrokerProxy);
        assert_eq!(copilot_responses.wire, WireProtocol::OpenAiResponses);
        assert!(resolve_route("github-copilot/unverified-model").is_none());
        assert!(resolve_route("unknown-provider/model").is_none());
        assert!(resolve_route("xai-auth/not-a-real-model").is_none());
        assert!(resolve_route("xai-auth/grok-build-0.1").is_none());
        assert!(resolve_route("xai-auth/grok-build-latest").is_none());
        assert!(resolve_route("xai-auth/grok-imagine-image").is_none());

        // Every verified concrete text/tool wire ID in the catalog must resolve.
        for descriptor in catalog::GOOGLE_GEMINI_TEXT_MODELS {
            let route = resolve_route(&format!("google-gemini/{}", descriptor.id)).unwrap();
            assert_eq!(route.provider, "google-gemini");
            assert_eq!(route.model, descriptor.id);
            assert_eq!(route.endpoint, "https://cloudcode-pa.googleapis.com");
            assert_eq!(route.auth, AuthPolicy::BrokerProxy);
            assert_eq!(route.wire, WireProtocol::GoogleGeminiCodeAssist);
        }
        // Aliases, unsupported media/embedding models, and family prefixes fail closed.
        for banned in [
            "auto-gemini-2.5",
            "auto-gemini-3",
            "text-embedding-004",
            "gemini-2.5-image",
            "gemini-2.5", // family prefix is not a wire id
        ] {
            assert!(
                resolve_route(&format!("google-gemini/{banned}")).is_none(),
                "{banned} must not resolve"
            );
        }
    }

    /// Regression: `gemini-pro-latest` must NOT resolve — it is a public
    /// Gemini API alias that Code Assist rejects with 404 NOT_FOUND. Routing
    /// fails closed instead of emitting a doomed upstream request.
    #[test]
    fn does_not_resolve_public_api_alias_gemini_pro_latest() {
        assert!(
            resolve_route("google-gemini/gemini-pro-latest").is_none(),
            "gemini-pro-latest is not a Code Assist wire ID (404 upstream)"
        );
    }

    #[test]
    fn resolves_openai_codex_without_requiring_eager_credentials() {
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        let route = resolve_route("openai-codex/gpt-5.1-codex-mini").unwrap();
        assert_eq!(route.provider, "openai-codex");
        assert_eq!(route.wire, WireProtocol::CodexResponses);
    }

    /// Regression: models under the `google-gemini/*` prefix must route through
    /// `try_route` (i.e. the OpenAI-runtime dispatcher must return `Some(_)` so
    /// the caller invokes the Gemini stream rather than falling through to the
    /// Anthropic path and reporting a misleading 401. Before this fix,
    /// `WireProtocol::GoogleGeminiCodeAssist => None` made `try_route` return
    /// `None`.
    ///
    /// We drive `try_route` through the `Remote` credential source pointed at a
    /// closed loopback port so the broker request errors out fast without
    /// touching the developer's real auth.json or the network. The outcome we
    /// assert is only that the routing dispatcher returned `Some(_)` — the
    /// specific broker error is irrelevant.
    #[tokio::test]
    async fn try_route_google_gemini_is_not_none_and_does_not_fall_through_to_anthropic() {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        // 127.0.0.1:1 is a reserved port; on Linux connect(2) returns
        // ECONNREFUSED immediately so the broker call fails fast.
        let source = crate::auth::CredentialSource::Remote {
            endpoint: "http://127.0.0.1:1".into(),
            machine_token: "not-a-real-token".into(),
        };
        let cache = crate::auth::TokenCache::new();
        let tools_schema: std::sync::Arc<Vec<serde_json::Value>> = std::sync::Arc::new(Vec::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();

        let messages: Vec<crate::SharedMessage> = vec![std::sync::Arc::new(serde_json::json!({
            "role": "user",
            "content": "hi"
        }))];

        let trace = crate::runtime::trace::TraceContext::disabled();
        let fut = try_route(
            "google-gemini/gemini-2.5-pro",
            &client,
            &tools_schema,
            &None,
            &messages,
            &tx,
            None,
            None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &cancel,
            &source,
            &cache,
            0,
            catalog::CodexRequestRole::Foreground,
            &trace,
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("try_route completed within budget");

        assert!(
            result.is_some(),
            "google-gemini must route through try_route (was None → fell through to Anthropic)"
        );
    }

    /// Headless harness (no live credentials): rejected xAI reasoning
    /// combinations must fail BEFORE any credential or network access on the
    /// Responses path. The Remote credential source points at a closed
    /// loopback port — if validation ran after broker access this would
    /// surface a connection error instead of the capability message.
    #[tokio::test]
    async fn xai_off_is_rejected_pre_network_through_try_route() {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let source = crate::auth::CredentialSource::Remote {
            endpoint: "http://127.0.0.1:1".into(),
            machine_token: "not-a-real-token".into(),
        };
        let cache = crate::auth::TokenCache::new();
        let tools_schema: std::sync::Arc<Vec<serde_json::Value>> = std::sync::Arc::new(Vec::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let messages: Vec<crate::SharedMessage> = vec![std::sync::Arc::new(serde_json::json!({
            "role": "user",
            "content": "hi"
        }))];

        for (level, needle) in [
            (
                agent_core::reasoning::ReasoningLevel::Off,
                "cannot be disabled",
            ),
            (
                agent_core::reasoning::ReasoningLevel::XHigh,
                "not supported",
            ),
            (
                agent_core::reasoning::ReasoningLevel::Ultra,
                "not supported",
            ),
        ] {
            let result = try_route(
                "xai-auth/grok-4.5",
                &client,
                &tools_schema,
                &None,
                &messages,
                &tx,
                None,
                None,
                0,
                level,
                &cancel,
                &source,
                &cache,
                0,
                catalog::CodexRequestRole::Foreground,
                &crate::runtime::trace::TraceContext::disabled(),
            )
            .await
            .expect("xai-auth must route through try_route");
            let err = result.expect_err("rejected level must error");
            assert!(
                err.to_string().contains(needle),
                "{level}: pre-network capability rejection expected, got: {err}"
            );
        }
    }

    #[test]
    fn set_extension_manager_for_routing_overwrites_previous_manager() {
        clear_extension_manager_for_routing();
        let first = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(Arc::new(
            crate::extensions::hooks::HookBus::new(),
        ))));
        let second = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(Arc::new(
            crate::extensions::hooks::HookBus::new(),
        ))));

        set_extension_manager_for_routing(first.clone());
        assert!(Arc::ptr_eq(
            &extension_manager_for_routing().unwrap(),
            &first
        ));

        set_extension_manager_for_routing(second.clone());
        assert!(Arc::ptr_eq(
            &extension_manager_for_routing().unwrap(),
            &second
        ));

        clear_extension_manager_for_routing();
        assert!(extension_manager_for_routing().is_none());
    }
}
