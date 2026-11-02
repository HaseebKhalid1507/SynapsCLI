//! OpenAI-compatible provider engine.
//!
//! Ported from the `openai-runtime` prototype. Translates between Anthropic-shaped
//! messages/tools/content-blocks (the internal synaps representation) and
//! OpenAI `chat/completions` SSE wire.

pub(crate) mod account_routing;
pub mod catalog;
pub(crate) mod codex_wire;
pub(crate) mod extension_route;
pub mod net;
pub mod ping;
pub mod reasoning;
pub mod registry;
pub mod stream;
pub(crate) mod terminal_diagnostics;
pub mod translate;
pub mod types;
pub mod wire;

use std::sync::Arc;

use crate::extensions::manager::ExtensionManager;
use crate::extensions::providers::ProviderRegistry;

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
/// How many upstream attempts a routed request may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptPolicy {
    /// The configured retries, plus each route's own floors and retry loops.
    Configured,
    /// Exactly one upstream attempt: no retries, no route floors and no
    /// provider-specific retry loops (the rollover summary).
    Single,
}

/// The output limit a route's endpoint documents and enforces, as the field
/// the request carries, or `None` when no ceiling is known to be accepted
/// and enforced there (the bounded rollover summary is then not sent).
///
/// - Anthropic Messages: `max_tokens` (thinking is off on a bounded call).
/// - Responses on xAI and GitHub Copilot: `max_output_tokens`.
/// - Gemini Code Assist: `generationConfig.maxOutputTokens`.
/// - Chat Completions: per endpoint and provider, see
///   [`stream::chat_completion_limit_field`].
/// - ChatGPT-Codex: none — no client code or documentation available shows
///   that backend accepting `max_output_tokens` (the official Codex client
///   does not send it), so a ceiling there cannot be relied on.
pub(crate) fn output_ceiling_field(route: &ResolvedRoute) -> Option<&'static str> {
    match route.wire {
        WireProtocol::AnthropicMessages => Some("max_tokens"),
        WireProtocol::OpenAiResponses => {
            matches!(route.provider.as_str(), "xai-auth" | "github-copilot")
                .then_some("max_output_tokens")
        }
        WireProtocol::GoogleGeminiCodeAssist => Some("generationConfig.maxOutputTokens"),
        WireProtocol::OpenAiChatCompletions => {
            stream::chat_completion_limit_field(&route.provider, &route.endpoint, &route.model)
        }
        WireProtocol::CodexResponses => None,
    }
}

/// Whether `route` has an output ceiling it is known to enforce.
pub(crate) fn output_ceiling_enforceable(route: &ResolvedRoute) -> bool {
    output_ceiling_field(route).is_some()
}

/// A routed request refused before anything was sent upstream.
#[derive(Debug)]
pub(crate) struct NotDispatched(pub String);

impl std::fmt::Display for NotDispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotDispatched {}

impl NotDispatched {
    /// `error`, marked as refused before anything was sent upstream.
    pub(crate) fn boxed(error: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(Self(error.to_string()))
    }
}

/// A routed request that failed after the provider reported the final usage
/// of its only attempt (a Responses `response.incomplete`, `response.failed`
/// or text-less completion; a Gemini stream error after its final report).
/// The usage is kept as reported, in Anthropic field names with only the
/// counters the provider sent, so settlement can book it although the reply
/// text failed. Displays as the failure it wraps.
#[derive(Debug)]
pub(crate) struct FailedWithUsage {
    pub(crate) usage: serde_json::Value,
    error: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for FailedWithUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for FailedWithUsage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// `error`, carrying `usage` when the provider reported it.
pub(crate) fn failed_with_usage(
    error: Box<dyn std::error::Error + Send + Sync>,
    usage: Option<serde_json::Value>,
) -> Box<dyn std::error::Error + Send + Sync> {
    match usage {
        Some(usage) => Box::new(FailedWithUsage { usage, error }),
        None => error,
    }
}

/// A broker failure surfaced as `message`: marked [`NotDispatched`] only on
/// a route's first attempt and only when the broker reports that no byte of
/// the request reached the provider. Every other failure stays unmarked
/// (its spend is unknown).
pub(crate) fn broker_failure(
    error: &crate::auth::BrokerError,
    first_attempt: bool,
    message: String,
) -> Box<dyn std::error::Error + Send + Sync> {
    if first_attempt && error.request_not_sent() {
        NotDispatched::boxed(message)
    } else {
        message.into()
    }
}

/// Transport retry budget a route receives. Only `Configured` applies the
/// Codex persistent floor.
pub(crate) fn route_retry_budget(
    wire: WireProtocol,
    max_retries: u32,
    attempts: AttemptPolicy,
) -> u32 {
    match (attempts, wire) {
        (AttemptPolicy::Single, _) => 0,
        (AttemptPolicy::Configured, WireProtocol::CodexResponses) => {
            stream::codex_retry_budget(max_retries)
        }
        (AttemptPolicy::Configured, _) => max_retries,
    }
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
        if prefix == "kimi-code" {
            // Broker-proxied managed Kimi Code endpoint: the broker owns the
            // short-lived OAuth access token (and its rotating refresh token)
            // and pins api.kimi.com/coding/v1. Wire ids are limited to the
            // conservative managed catalog — unknown ids fail closed.
            catalog::kimi_code_model(rest)?;
            return Some(ResolvedRoute {
                endpoint: crate::auth::kimi_code::API_BASE_URL.into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::BrokerProxy,
                wire: WireProtocol::OpenAiChatCompletions,
            });
        }
        if prefix == "muse" {
            // Broker-proxied Meta Muse: the broker resolves the Model API key
            // (env override or minted from the Meta login) and pins
            // api.meta.ai. Server-driven catalog → registry precedent: any
            // safe-shaped id routes and Meta decides existence. Routing never
            // depends on the process-local catalog cache (daemon/resume).
            if !catalog::muse_routable_id(rest) {
                return None;
            }
            return Some(ResolvedRoute {
                endpoint: crate::auth::muse::API_BASE_URL.into(),
                model: rest.into(),
                provider: prefix.into(),
                auth: AuthPolicy::BrokerProxy,
                wire: WireProtocol::OpenAiResponses,
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
/// `tool_session_id` scopes the execution gate for extension-provider
/// interior tool loops (Task 16); `None` fails closed inside the route.
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
    tool_session_id: Option<&crate::tools::activation::SessionId>,
    session_tool_set: Option<&crate::tools::activation::SharedSessionToolSet>,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    try_route_with_memory_backend(
        model,
        client,
        tools_schema,
        system_prompt,
        messages,
        tx,
        temperature,
        max_tokens,
        thinking_budget,
        reasoning_level,
        cancel,
        source,
        cache,
        max_retries,
        codex_request_role,
        tool_session_id,
        session_tool_set,
        None,
        None,
        trace,
        suppress_stream_deltas,
        AttemptPolicy::Configured,
        None,
    )
    .await
}

/// The host-key-free history every non-Anthropic route sends, or `None`
/// when there is nothing to strip or the model takes the native Anthropic
/// transport (which strips while serializing its body).
pub(crate) fn routed_wire_messages(
    model: &str,
    messages: &[crate::SharedMessage],
) -> Option<Vec<crate::SharedMessage>> {
    let native_anthropic = ProviderRegistry::parse_model_id(model).is_none()
        && resolve_route(model).is_some_and(|r| r.wire == WireProtocol::AnthropicMessages);
    if native_anthropic {
        return None;
    }
    super::continuation::wire_messages(messages)
}

/// Whether `model`'s transport books a ledgered request's attempts (§6):
/// Anthropic Messages, Chat Completions and Codex Responses. Every other
/// route (the other wires, cloud providers, extension providers) refuses a
/// ledgered request before anything is sent.
pub(crate) fn books_requests(model: &str) -> bool {
    if ProviderRegistry::parse_model_id(model).is_some() {
        return false;
    }
    let (cloud_model, _) = crate::auth::cloud::split_model_route(model);
    if cloud_model
        .split_once('/')
        .is_some_and(|(provider, _)| provider.parse::<crate::auth::CloudProviderId>().is_ok())
    {
        return false;
    }
    resolve_route(model).is_some_and(|route| {
        matches!(
            route.wire,
            WireProtocol::AnthropicMessages
                | WireProtocol::OpenAiChatCompletions
                | WireProtocol::CodexResponses
        )
    })
}

/// The refusal of a ledgered request on a route that does not book it.
pub(crate) fn unbooked_route(model: &str) -> String {
    format!("'{model}' can't book its requests in the run's request ledger; request not sent")
}

/// Runtime route retaining the host's immutable memory binding. The public
/// compatibility facade above has no runtime capability and cannot mint one.
/// `spend` is a worker runtime's spend tap (none for every other caller): a
/// routed transport books each of its paid attempts through it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_route_with_memory_backend(
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
    tool_session_id: Option<&crate::tools::activation::SessionId>,
    session_tool_set: Option<&crate::tools::activation::SharedSessionToolSet>,
    memory_backend: Option<&crate::memory_backend::MemoryBinding>,
    sandbox: Option<&std::sync::Arc<crate::sandbox::SandboxBinding>>,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
    attempts: AttemptPolicy,
    spend: Option<&crate::runtime::child_spend::SpendTap>,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    try_route_with_request_ledger(
        model,
        client,
        tools_schema,
        system_prompt,
        messages,
        tx,
        temperature,
        max_tokens,
        thinking_budget,
        reasoning_level,
        cancel,
        source,
        cache,
        max_retries,
        codex_request_role,
        tool_session_id,
        session_tool_set,
        memory_backend,
        sandbox,
        trace,
        suppress_stream_deltas,
        attempts,
        spend,
        None,
    )
    .await
}

/// [`try_route_with_memory_backend`] for a request that may be ledgered
/// (§6): `request_ledger` reaches the transports that book their attempts;
/// any other route refuses a ledgered request before anything is sent.
/// `spend` is as above: the ledger sits beside a worker's spend tap and
/// changes nothing about it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_route_with_request_ledger(
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
    tool_session_id: Option<&crate::tools::activation::SessionId>,
    session_tool_set: Option<&crate::tools::activation::SharedSessionToolSet>,
    memory_backend: Option<&crate::memory_backend::MemoryBinding>,
    sandbox: Option<&std::sync::Arc<crate::sandbox::SandboxBinding>>,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
    attempts: AttemptPolicy,
    spend: Option<&crate::runtime::child_spend::SpendTap>,
    request_ledger: Option<&crate::tools::RequestLedgerHandle>,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    try_route_with_request_ledger_for_event(
        model,
        client,
        tools_schema,
        system_prompt,
        messages,
        tx,
        temperature,
        max_tokens,
        thinking_budget,
        reasoning_level,
        cancel,
        source,
        cache,
        max_retries,
        codex_request_role,
        tool_session_id,
        session_tool_set,
        memory_backend,
        sandbox,
        trace,
        suppress_stream_deltas,
        attempts,
        spend,
        false,
        request_ledger,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_route_with_request_ledger_for_event(
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
    tool_session_id: Option<&crate::tools::activation::SessionId>,
    session_tool_set: Option<&crate::tools::activation::SharedSessionToolSet>,
    memory_backend: Option<&crate::memory_backend::MemoryBinding>,
    sandbox: Option<&std::sync::Arc<crate::sandbox::SandboxBinding>>,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
    attempts: AttemptPolicy,
    spend: Option<&crate::runtime::child_spend::SpendTap>,
    event_triggered: bool,
    request_ledger: Option<&crate::tools::RequestLedgerHandle>,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    // Muse capability evidence (exact effort set, modalities) lives in the
    // process-local cache; load it here via the turn's broker when this
    // process has none for the model, BEFORE body validation consults it.
    if model.starts_with("muse/") {
        let broker = crate::auth::broker_from_source(source, cache, client.clone());
        catalog::ensure_muse_catalog_for(model, &broker).await;
    }
    if let Err(error) = super::attachments::validate_history_for_request(model, messages) {
        return Some(Err(Box::new(NotDispatched(error))));
    }
    if request_ledger.is_some() && !books_requests(model) {
        return Some(Err(NotDispatched::boxed(unbooked_route(model))));
    }
    // Host `_synaps_*` keys never reach any of these transports (some
    // forward the history verbatim). Only the native Anthropic route, which
    // returns `None` below without reading `messages`, skips the copy.
    let wire = routed_wire_messages(model, messages);
    let messages = wire.as_deref().unwrap_or(messages);
    if let Some((plugin_id, provider_id, model_id)) = ProviderRegistry::parse_model_id(model) {
        if attempts == AttemptPolicy::Single {
            // An extension provider owns its own transport and retries; a
            // single-attempt guarantee cannot be enforced through it.
            return Some(Err(Box::new(NotDispatched(format!(
                "Extension provider model '{model}' cannot serve a single-attempt request"
            )))));
        }
        if let Some(manager) = extension_manager_for_routing() {
            // Extension-hosted provider: routing gates, audits, and the
            // Task 10C transport trace live in `extension_route`.
            return Some(
                extension_route::route_extension_provider(
                    manager,
                    plugin_id,
                    provider_id,
                    model_id,
                    model,
                    tools_schema,
                    system_prompt,
                    messages,
                    tx,
                    temperature,
                    max_tokens,
                    thinking_budget,
                    cancel,
                    tool_session_id,
                    session_tool_set,
                    memory_backend,
                    sandbox,
                    trace,
                    crate::runtime::child_spend::RouteSpend { tap: spend, model },
                )
                .await,
            );
        }
        return Some(Err(NotDispatched::boxed(format!(
            "Extension provider model '{}' is not available",
            model
        ))));
    }

    let Some(route) = resolve_route(model) else {
        return Some(Err(Box::new(NotDispatched(format!(
            "Unknown provider route for model '{model}'"
        )))));
    };
    // A bounded single attempt never follows redirects: a redirect would
    // replay it, and only then is a refused connection proof of not-sent.
    // Without a client that refuses redirects nothing is sent at all.
    let broker = match attempts {
        AttemptPolicy::Single => {
            match crate::auth::bounded_broker_from_source(source, cache, client.clone()) {
                Ok(broker) => broker,
                Err(error) => {
                    return Some(Err(NotDispatched::boxed(format!(
                        "bounded request not sent: {error}"
                    ))))
                }
            }
        }
        AttemptPolicy::Configured => crate::auth::broker_from_source(source, cache, client.clone()),
    };
    // Trace honesty (Task 10A): only a local (in-process) broker sends the
    // exact bytes this process serialized (`ProxyRequest::body_bytes`); a
    // remote broker daemon re-serializes upstream bodies out of process.
    let exact_wire_bytes = !source.is_remote();
    dispatch_route_for_event(
        &route,
        client,
        &broker,
        exact_wire_bytes,
        tools_schema,
        system_prompt,
        messages,
        tx,
        temperature,
        max_tokens,
        thinking_budget,
        reasoning_level,
        cancel,
        max_retries,
        codex_request_role,
        trace,
        suppress_stream_deltas,
        attempts,
        crate::runtime::child_spend::RouteSpend { tap: spend, model },
        event_triggered,
        request_ledger,
    )
    .await
}

/// Send one request on an already resolved route through `broker`. `None`
/// for the Anthropic Messages wire, which the caller serves itself. `spend`
/// guards each paid attempt (inert without a tap).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_route(
    route: &ResolvedRoute,
    client: &reqwest::Client,
    broker: &Arc<dyn crate::auth::CredentialBroker>,
    exact_wire_bytes: bool,
    tools_schema: &std::sync::Arc<Vec<serde_json::Value>>,
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    cancel: &tokio_util::sync::CancellationToken,
    max_retries: u32,
    codex_request_role: catalog::CodexRequestRole,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
    attempts: AttemptPolicy,
    spend: crate::runtime::child_spend::RouteSpend<'_>,
    request_ledger: Option<&crate::tools::RequestLedgerHandle>,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    dispatch_route_for_event(
        route,
        client,
        broker,
        exact_wire_bytes,
        tools_schema,
        system_prompt,
        messages,
        tx,
        temperature,
        max_tokens,
        thinking_budget,
        reasoning_level,
        cancel,
        max_retries,
        codex_request_role,
        trace,
        suppress_stream_deltas,
        attempts,
        spend,
        false,
        request_ledger,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_route_for_event(
    route: &ResolvedRoute,
    client: &reqwest::Client,
    broker: &Arc<dyn crate::auth::CredentialBroker>,
    exact_wire_bytes: bool,
    tools_schema: &std::sync::Arc<Vec<serde_json::Value>>,
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    cancel: &tokio_util::sync::CancellationToken,
    max_retries: u32,
    codex_request_role: catalog::CodexRequestRole,
    trace: &crate::runtime::trace::TraceContext,
    suppress_stream_deltas: bool,
    attempts: AttemptPolicy,
    spend: crate::runtime::child_spend::RouteSpend<'_>,
    event_triggered: bool,
    request_ledger: Option<&crate::tools::RequestLedgerHandle>,
) -> Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    // §6: only the Chat Completions and Codex wires book a ledgered
    // request here (Anthropic Messages is served by the caller).
    if request_ledger.is_some()
        && !matches!(
            route.wire,
            WireProtocol::AnthropicMessages
                | WireProtocol::OpenAiChatCompletions
                | WireProtocol::CodexResponses
        )
    {
        return Some(Err(NotDispatched::boxed(unbooked_route(&format!(
            "{}/{}",
            route.provider, route.model
        )))));
    }
    let cfg = provider_config(route);
    let broker = broker.clone();
    if attempts == AttemptPolicy::Single
        && max_tokens.is_some()
        && !output_ceiling_enforceable(route)
    {
        return Some(Err(Box::new(NotDispatched(format!(
            "'{}/{}' has no enforceable output ceiling; request not sent",
            route.provider, route.model
        )))));
    }
    let max_retries = route_retry_budget(route.wire, max_retries, attempts);
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
                reasoning_level,
                cancel,
                trace,
                exact_wire_bytes,
                suppress_stream_deltas,
                max_retries,
                spend,
                request_ledger,
            )
            .await,
        ),
        WireProtocol::OpenAiResponses => {
            if suppress_stream_deltas {
                return Some(Err(NotDispatched::boxed(
                    "internal sync routing is unavailable for this streaming provider",
                )));
            }
            // Same wire, per-provider body/identity: Meta Muse mirrors the
            // official Muse Code request shape; xAI and GitHub Copilot go
            // out under their own provider identity.
            Some(
                stream::call_responses_route_inner(
                    &route.provider,
                    &cfg,
                    &broker,
                    tools_schema,
                    system_prompt,
                    messages,
                    tx,
                    max_tokens,
                    reasoning_level,
                    cancel,
                    // In-stream capacity errors (HTTP 200 + lone `error`
                    // event) are transient; re-send identical bytes with
                    // the configured retry budget.
                    max_retries,
                    trace,
                    exact_wire_bytes,
                    spend,
                )
                .await,
            )
        }
        WireProtocol::CodexResponses => {
            if suppress_stream_deltas {
                return Some(Err(NotDispatched::boxed(
                    "internal sync routing is unavailable for this streaming provider",
                )));
            }
            Some(
                stream::call_codex_stream_inner_for_event(
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
                    // retries), not the generic three-attempt budget; the floor
                    // is applied by `route_retry_budget` above.
                    max_retries,
                    trace,
                    spend,
                    event_triggered,
                    request_ledger,
                )
                .await,
            )
        }
        WireProtocol::AnthropicMessages => None,
        WireProtocol::GoogleGeminiCodeAssist => {
            if suppress_stream_deltas {
                return Some(Err(NotDispatched::boxed(
                    "internal sync routing is unavailable for this streaming provider",
                )));
            }
            Some(
                crate::runtime::google_gemini::runtime::call_google_gemini_stream_limited(
                    &cfg,
                    &broker,
                    tools_schema,
                    system_prompt,
                    messages,
                    tx,
                    cancel,
                    trace,
                    exact_wire_bytes,
                    crate::runtime::google_gemini::runtime::GeminiLimits {
                        max_output_tokens: max_tokens,
                        max_429_retries: match attempts {
                            AttemptPolicy::Configured => {
                                crate::runtime::google_gemini::runtime::MAX_GEMINI_429_RETRIES
                            }
                            AttemptPolicy::Single => 0,
                        },
                    },
                    spend,
                )
                .await,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §6: only the transports with attempt hooks book a ledgered request.
    #[test]
    fn only_hooked_transports_book_ledgered_requests() {
        for model in [
            "claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-6",
            "local/test-model",
            "openai-codex/gpt-5.5",
        ] {
            assert!(books_requests(model), "{model}");
        }
        for model in [
            "xai-auth/grok-4.5",
            "aws-bedrock/anthropic.claude-sonnet-4-6",
            "some-plugin:provider:model",
            "no-such-provider/model",
        ] {
            assert!(!books_requests(model), "{model}");
        }
    }

    #[test]
    fn the_output_ceiling_follows_the_route() {
        let field = |model: &str| output_ceiling_field(&resolve_route(model).unwrap());
        assert_eq!(field("openai-codex/gpt-5.5"), None, "Codex");
        assert_eq!(field("claude-sonnet-4-6"), Some("max_tokens"));
        assert_eq!(field("anthropic/claude-sonnet-4-6"), Some("max_tokens"));
        assert_eq!(field("xai-auth/grok-4.5"), Some("max_output_tokens"));
        assert_eq!(
            field("github-copilot/gpt-5-mini"),
            Some("max_output_tokens")
        );
        assert_eq!(
            field("github-copilot/claude-sonnet-4.6"),
            Some("max_tokens")
        );
        assert_eq!(
            field("google-gemini/gemini-2.5-pro"),
            Some("generationConfig.maxOutputTokens")
        );
        assert_eq!(
            field("groq/llama-3.3-70b-versatile"),
            Some("max_completion_tokens")
        );
        assert_eq!(field("openrouter/openai/o3"), Some("max_tokens"));
        assert_eq!(field("google/gemini-2.5-flash"), Some("max_tokens"));
        assert_eq!(field("local/qwen3"), Some("max_tokens"));
        assert_eq!(
            field("chutes/some-model"),
            None,
            "no documented ceiling verified"
        );
    }

    #[tokio::test]
    async fn a_bounded_single_attempt_to_codex_is_refused_before_sending() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let messages: Vec<crate::SharedMessage> = vec![std::sync::Arc::new(
            serde_json::json!({"role":"user","content":"hi"}),
        )];
        let result = try_route_with_memory_backend(
            "openai-codex/gpt-5.5",
            &reqwest::Client::new(),
            &std::sync::Arc::new(Vec::new()),
            &None,
            &messages,
            &tx,
            None,
            Some(1_000),
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &tokio_util::sync::CancellationToken::new(),
            &crate::auth::CredentialSource::Local,
            &crate::auth::TokenCache::new(),
            0,
            catalog::CodexRequestRole::Internal,
            None,
            None,
            None,
            None,
            &crate::runtime::trace::TraceContext::disabled(),
            false,
            AttemptPolicy::Single,
            None,
        )
        .await
        .expect("codex is a routed model");
        let error = result.expect_err("refused");
        assert!(error.is::<NotDispatched>(), "{error}");
    }

    /// Records every proxied request and answers it with one Responses
    /// stream (text plus usage).
    #[derive(Default)]
    struct RecordingBroker {
        seen: std::sync::Mutex<Vec<crate::auth::ProxyRequest>>,
        /// Scripted replies, one per request; the default text reply once
        /// the script runs out.
        script: std::sync::Mutex<std::collections::VecDeque<&'static str>>,
    }

    const RESPONSES_TEXT_REPLY: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"SUMMARY\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":9}}}\n\n",
    );

    #[async_trait::async_trait]
    impl crate::auth::CredentialBroker for RecordingBroker {
        async fn access_token(
            &self,
            _provider: crate::auth::OAuthProviderId,
        ) -> Result<crate::auth::AccessToken, crate::auth::BrokerError> {
            Err(crate::auth::BrokerError::Denied("proxy only".into()))
        }
        async fn proxy(
            &self,
            _request: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyResponse, crate::auth::BrokerError> {
            Err(crate::auth::BrokerError::Denied("stream only".into()))
        }
        async fn proxy_stream(
            &self,
            request: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyByteStream, crate::auth::BrokerError> {
            request.validate()?;
            self.seen.lock().unwrap().push(request);
            let sse = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RESPONSES_TEXT_REPLY);
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                bytes::Bytes::from_static(sse.as_bytes()),
            )])))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, crate::auth::BrokerError> {
            Err(crate::auth::BrokerError::Denied("unused".into()))
        }
        async fn capabilities(
            &self,
        ) -> Result<Vec<crate::auth::ProviderStatus>, crate::auth::BrokerError> {
            Ok(vec![])
        }
    }

    /// One bounded single-attempt request on `model`'s resolved route.
    async fn dispatch_recorded(
        model: &str,
        level: agent_core::reasoning::ReasoningLevel,
    ) -> (
        Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>,
        Vec<crate::auth::ProxyRequest>,
    ) {
        let recorder = Arc::new(RecordingBroker::default());
        let broker: Arc<dyn crate::auth::CredentialBroker> = recorder.clone();
        let route = resolve_route(model).expect("a resolved route");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let messages: Vec<crate::SharedMessage> = vec![Arc::new(
            serde_json::json!({"role":"user","content":"summarize"}),
        )];
        let result = dispatch_route(
            &route,
            &reqwest::Client::new(),
            &broker,
            true,
            &Arc::new(Vec::new()),
            &Some("system".into()),
            &messages,
            &tx,
            None,
            Some(1_000),
            0,
            level,
            &tokio_util::sync::CancellationToken::new(),
            0,
            catalog::CodexRequestRole::Internal,
            &crate::runtime::trace::TraceContext::disabled(),
            false,
            AttemptPolicy::Single,
            crate::runtime::child_spend::RouteSpend::NONE,
            None,
        )
        .await
        .expect("a routed wire");
        let seen = std::mem::take(&mut *recorder.seen.lock().unwrap());
        (result, seen)
    }

    #[tokio::test]
    async fn responses_requests_carry_the_resolved_provider_identity() {
        use agent_core::reasoning::ReasoningLevel;
        for (model, provider) in [
            ("github-copilot/gpt-5-mini", "github-copilot"),
            ("github-copilot/gpt-5.4", "github-copilot"),
            ("xai-auth/grok-4.5", "xai-auth"),
        ] {
            for (level, effort) in [
                (ReasoningLevel::Adaptive, None),
                (ReasoningLevel::Low, Some("low")),
            ] {
                let (result, seen) = dispatch_recorded(model, level).await;
                let reply = result.unwrap_or_else(|e| panic!("{model} {level}: {e}"));
                assert_eq!(reply["content"][0]["text"], "SUMMARY", "{model}");
                assert_eq!(seen.len(), 1, "{model} {level}: one attempt");
                let request = &seen[0];
                assert_eq!(request.provider, provider, "{model}: broker identity");
                assert_eq!(request.path, "/responses");
                let body = request.body.as_ref().expect("json body");
                assert_eq!(body["model"], model.split_once('/').unwrap().1);
                assert_eq!(body["max_output_tokens"], 1_000, "{model}: ceiling");
                assert_eq!(
                    body["reasoning"]["effort"].as_str(),
                    effort,
                    "{model} {level}"
                );
            }
        }
    }

    /// One ordinary foreground request (`AttemptPolicy::Configured`) on
    /// `model`'s resolved route, answered by `script` in order.
    async fn dispatch_configured(
        model: &str,
        script: &[&'static str],
        max_retries: u32,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> (
        Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>,
        Vec<crate::auth::ProxyRequest>,
    ) {
        let recorder = Arc::new(RecordingBroker::default());
        recorder
            .script
            .lock()
            .unwrap()
            .extend(script.iter().copied());
        let broker: Arc<dyn crate::auth::CredentialBroker> = recorder.clone();
        let route = resolve_route(model).expect("a resolved route");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let messages: Vec<crate::SharedMessage> = vec![Arc::new(
            serde_json::json!({"role":"user","content":"list the files"}),
        )];
        let tools = Arc::new(vec![serde_json::json!({
            "name": "bash",
            "description": "run a command",
            "input_schema": {"type":"object","properties":{"command":{"type":"string"}}},
        })]);
        let result = dispatch_route(
            &route,
            &reqwest::Client::new(),
            &broker,
            true,
            &tools,
            &Some("system".into()),
            &messages,
            &tx,
            None,
            None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            cancel,
            max_retries,
            catalog::CodexRequestRole::Foreground,
            &crate::runtime::trace::TraceContext::disabled(),
            false,
            AttemptPolicy::Configured,
            crate::runtime::child_spend::RouteSpend::NONE,
            None,
        )
        .await
        .expect("a routed wire");
        let seen = std::mem::take(&mut *recorder.seen.lock().unwrap());
        (result, seen)
    }

    const COPILOT_GPT: &str = "github-copilot/gpt-5-mini";
    const RESPONSES_CAPACITY_REPLY: &str =
        "data: {\"type\":\"error\",\"code\":\"server_overloaded\",\"message\":\"at capacity\"}\n\n";

    #[tokio::test]
    async fn copilot_responses_foreground_text_and_tool_rounds_work() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (result, seen) = dispatch_configured(COPILOT_GPT, &[], 3, &cancel).await;
        let reply = result.expect("text round");
        assert_eq!(reply["content"][0]["text"], "SUMMARY");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].provider, "github-copilot");
        assert!(
            seen[0]
                .body
                .as_ref()
                .unwrap()
                .get("max_output_tokens")
                .is_none(),
            "an ordinary round sends no summary ceiling"
        );

        const TOOL_REPLY: &str = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":50,\"output_tokens\":7}}}\n\n",
        );
        let (result, seen) = dispatch_configured(COPILOT_GPT, &[TOOL_REPLY], 3, &cancel).await;
        let reply = result.expect("tool round");
        let tool = reply["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_use")
            .expect("a tool_use block");
        assert_eq!(tool["name"], "bash");
        assert_eq!(tool["input"]["command"], "ls");
        assert_eq!(seen.len(), 1);
    }

    #[tokio::test]
    async fn copilot_responses_foreground_cancellation_is_canceled() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let (result, seen) = dispatch_configured(COPILOT_GPT, &[], 3, &cancel).await;
        let error = result.expect_err("canceled");
        assert!(matches!(
            net::provider_error_to_runtime_for(COPILOT_GPT, error),
            crate::RuntimeError::Canceled
        ));
        assert!(seen.is_empty(), "nothing sent after cancellation");
    }

    #[tokio::test(start_paused = true)]
    async fn copilot_responses_capacity_retries_then_reports_an_api_status() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (result, seen) =
            dispatch_configured(COPILOT_GPT, &[RESPONSES_CAPACITY_REPLY], 2, &cancel).await;
        assert_eq!(result.expect("retried")["content"][0]["text"], "SUMMARY");
        assert_eq!(seen.len(), 2, "one capacity retry");

        let (result, seen) =
            dispatch_configured(COPILOT_GPT, &[RESPONSES_CAPACITY_REPLY; 3], 2, &cancel).await;
        assert_eq!(seen.len(), 3, "the configured budget is spent");
        let error = net::provider_error_to_runtime_for(COPILOT_GPT, result.expect_err("exhausted"));
        match error {
            crate::RuntimeError::ApiStatus(message) => {
                assert!(message.starts_with("GitHub Copilot reports the model is at capacity"))
            }
            other => panic!("Copilot capacity must be an API status, got {other:?}"),
        }
    }

    #[test]
    fn a_single_attempt_skips_every_route_floor() {
        for wire in [
            WireProtocol::AnthropicMessages,
            WireProtocol::OpenAiChatCompletions,
            WireProtocol::OpenAiResponses,
            WireProtocol::CodexResponses,
            WireProtocol::GoogleGeminiCodeAssist,
        ] {
            for configured in [0, 3, 15] {
                assert_eq!(
                    route_retry_budget(wire, configured, AttemptPolicy::Single),
                    0
                );
            }
        }
        assert_eq!(
            route_retry_budget(WireProtocol::CodexResponses, 0, AttemptPolicy::Configured),
            stream::codex_retry_budget(0),
            "ordinary requests keep the Codex floor"
        );
        assert_eq!(
            route_retry_budget(
                WireProtocol::OpenAiChatCompletions,
                3,
                AttemptPolicy::Configured
            ),
            3
        );
    }

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
    fn muse_routing_is_broker_proxied_responses_and_fail_closed() {
        let route = resolve_route("muse/muse-wonder-1.0").expect("documented seed routes");
        assert_eq!(route.provider, "muse");
        assert_eq!(route.model, "muse-wonder-1.0");
        assert_eq!(route.endpoint, crate::auth::muse::API_BASE_URL);
        // The Model API key never reaches the runtime: broker-proxied.
        assert_eq!(route.auth, AuthPolicy::BrokerProxy);
        assert_eq!(route.wire, WireProtocol::OpenAiResponses);
        // Server-driven catalog (registry precedent): any safe-shaped id
        // routes WITHOUT a live catalog in this process — Meta decides
        // existence. This is what lets a daemon-hosted session switch to a
        // model only the TUI's picker fetched.
        let live_only = resolve_route("muse/muse-spark-9.9").expect("shape-valid id routes");
        assert_eq!(live_only.model, "muse-spark-9.9");
        assert_eq!(live_only.wire, WireProtocol::OpenAiResponses);
        // Malformed ids fail closed: they never reach the request body.
        for banned in ["MUSE-WONDER-1.0", "", "a b", "../x", "-x", "a/b"] {
            assert!(
                resolve_route(&format!("muse/{banned}")).is_none(),
                "{banned} must not resolve"
            );
        }
    }

    #[test]
    fn kimi_code_routing_is_broker_proxied_and_fail_closed() {
        for descriptor in catalog::KIMI_CODE_TEXT_MODELS {
            let route = resolve_route(&format!("kimi-code/{}", descriptor.id)).unwrap();
            assert_eq!(route.provider, "kimi-code");
            assert_eq!(route.model, descriptor.id);
            assert_eq!(route.endpoint, "https://api.kimi.com/coding/v1");
            // Broker-proxied: the short-lived OAuth bearer never reaches the
            // runtime; the broker resolves + attaches it per request.
            assert_eq!(route.auth, AuthPolicy::BrokerProxy);
            assert_eq!(route.wire, WireProtocol::OpenAiChatCompletions);
        }
        // Unknown ids and the static Moonshot-platform ids fail closed on
        // this route (they belong to the API-key `kimi` provider).
        assert!(resolve_route("kimi-code/unknown-model").is_none());
        assert!(resolve_route("kimi-code/kimi-k3").is_none());
        assert!(resolve_route("kimi-code/kimi-k2.7-code").is_none());
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
            None,
            None,
            &trace,
            false,
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
                None,
                None,
                &crate::runtime::trace::TraceContext::disabled(),
                false,
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
    #[serial_test::serial(extension_routing)]
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
