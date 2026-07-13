use crate::{Result, RuntimeError, ToolRegistry};
use futures::stream::Stream;
use reqwest::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

mod api;
mod api_sync;
mod auth;
#[cfg(test)]
mod body_golden;
pub mod compaction;
pub mod google_gemini;
pub mod google_vertex;
pub(crate) mod helpers;
pub mod openai;
mod request;
mod sse;
mod sse_types;
mod stream;
pub mod subagent;
pub mod telemetry;
mod types;

use api::ApiMethods;
use auth::AuthMethods;
use helpers::HelperMethods;
use stream::StreamMethods;
use types::AuthState;
pub use types::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

/// Result of resolving before_tool_call extension policy.
pub enum BeforeToolCallDecision {
    Continue { input: Value },
    Block { reason: String },
}

/// Emit a `before_tool_call` event and include the runtime tool name when it
/// differs from the API-safe name.
pub async fn emit_before_tool_call(
    hook_bus: &Arc<crate::extensions::hooks::HookBus>,
    tool_name: &str,
    runtime_tool_name: Option<&str>,
    input: Value,
) -> crate::extensions::hooks::events::HookResult {
    let mut event = crate::extensions::hooks::events::HookEvent::before_tool_call(tool_name, input);
    if let Some(runtime_tool_name) = runtime_tool_name {
        event.tool_runtime_name = Some(runtime_tool_name.to_string());
    }
    hook_bus.emit(&event).await
}

/// Resolve a before_tool_call result that may request user confirmation.
///
/// When `auto_approve_confirms` is true, `Confirm` is short-circuited to `Continue`.
/// Headless/non-interactive callers with `auto_approve_confirms = false` fail closed.
pub async fn resolve_before_tool_call_result(
    hook_result: crate::extensions::hooks::events::HookResult,
    secret_prompt: Option<&crate::tools::SecretPromptHandle>,
    auto_approve_confirms: bool,
) -> crate::extensions::hooks::events::HookResult {
    match hook_result {
        crate::extensions::hooks::events::HookResult::Confirm { message } => {
            if auto_approve_confirms {
                tracing::info!(message = %message, "confirm auto-approved (auto_approve_confirms=true)");
                return crate::extensions::hooks::events::HookResult::Continue;
            }

            let Some(prompt) = secret_prompt else {
                return crate::extensions::hooks::events::HookResult::Block {
                    reason: format!(
                        "Tool call requires confirmation but no interactive prompt is available: {}",
                        message
                    ),
                };
            };

            let response = prompt
                .prompt(
                    "Confirm tool call".to_string(),
                    format!("{}\n\nType 'yes' or 'y' to allow.", message),
                )
                .await;

            match response.as_deref().map(str::trim) {
                Some(answer)
                    if answer.eq_ignore_ascii_case("yes") || answer.eq_ignore_ascii_case("y") =>
                {
                    crate::extensions::hooks::events::HookResult::Continue
                }
                _ => crate::extensions::hooks::events::HookResult::Block {
                    reason: format!("Tool call confirmation denied: {}", message),
                },
            }
        }
        other => other,
    }
}

/// Resolve before_tool_call policy into executable input or a block reason.
pub async fn resolve_before_tool_call_decision(
    original_input: Value,
    hook_result: crate::extensions::hooks::events::HookResult,
    secret_prompt: Option<&crate::tools::SecretPromptHandle>,
    auto_approve_confirms: bool,
) -> BeforeToolCallDecision {
    match resolve_before_tool_call_result(hook_result, secret_prompt, auto_approve_confirms).await {
        crate::extensions::hooks::events::HookResult::Block { reason } => {
            BeforeToolCallDecision::Block { reason }
        }
        crate::extensions::hooks::events::HookResult::Modify { input } => {
            BeforeToolCallDecision::Continue { input }
        }
        _ => BeforeToolCallDecision::Continue {
            input: original_input,
        },
    }
}

/// Maximum size (bytes) of a `Replace` output an extension may substitute.
/// A transform returning more than this is clamped (with a warning) to bound
/// memory between deserialization and the downstream tool-output truncation.
/// Generous on purpose — legitimate compression/redaction shrinks output.
const MAX_REPLACE_OUTPUT: usize = 1024 * 1024; // 1 MiB

/// Emit an `after_tool_call` event and return the tool output to record in
/// history — either the original `output`, or a `Replace { output }`
/// substituted by an extension transform hook (compression, redaction,
/// summarization). The runtime tool name is included when it differs from the
/// API-safe name.
///
/// Fail-safe: any non-`Replace` result — `Continue`, a result rejected by the
/// permission matrix, a crashed/timed-out handler, or any future variant —
/// returns the original `output` unchanged. A misbehaving extension can never
/// drop or corrupt a tool's output.
///
/// Note: the event delivered to the extension carries `tool_output` as a
/// size-limited preview for large outputs — a ~256 KB prefix plus a
/// `…[truncated, N total bytes]` marker (see [`HookEvent::after_tool_call`]),
/// matching `max_tool_buffer`. The final returned string is then truncated to
/// `max_tool_output` (the context budget) — compress-then-truncate ordering,
/// so a transform extension sees the full buffered output and decides what to
/// keep before the hard cap is applied.
pub async fn emit_after_tool_call(
    hook_bus: &Arc<crate::extensions::hooks::HookBus>,
    tool_name: &str,
    runtime_tool_name: Option<&str>,
    input: Value,
    output: String,
    max_tool_output: usize,
) -> String {
    use crate::extensions::hooks::events::HookResult;
    // Keep the original to return verbatim if no transform fires.
    let original = output.clone();
    let mut event =
        crate::extensions::hooks::events::HookEvent::after_tool_call(tool_name, input, output);
    if let Some(runtime_tool_name) = runtime_tool_name {
        event.tool_runtime_name = Some(runtime_tool_name.to_string());
    }
    let post_hook = match hook_bus.emit(&event).await {
        HookResult::Replace { mut output } => {
            if output.len() > MAX_REPLACE_OUTPUT {
                tracing::warn!(
                    tool = %tool_name,
                    len = output.len(),
                    cap = MAX_REPLACE_OUTPUT,
                    "Extension Replace output exceeds cap — clamping",
                );
                // Floor to a UTF-8 char boundary before truncating.
                let mut boundary = MAX_REPLACE_OUTPUT;
                while boundary > 0 && !output.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                output.truncate(boundary);
            }
            output
        }
        HookResult::Continue => original,
        other => {
            tracing::warn!(
                tool = %tool_name,
                ?other,
                "Unexpected after_tool_call result — using original output",
            );
            original
        }
    };
    // Compress-then-truncate: apply the context-budget cap AFTER the hook.
    // Mirrors `HelperMethods::truncate_tool_result` byte-for-byte so the
    // no-extension path is behavior-identical to the legacy ordering.
    crate::runtime::helpers::HelperMethods::truncate_tool_result(&post_hook, max_tool_output)
}

/// The core runtime — manages API communication, tool execution, authentication,
/// and streaming for all SynapsCLI binaries (chat, chatui, server, agent, watcher).
#[derive(Clone)]
struct PromptReloadSource {
    manifest: PathBuf,
    context: agent_core::prompt::SelectionContext,
    user_module: Option<agent_core::prompt::PromptModule>,
    delegation_policy_digest: Option<String>,
}

pub struct Runtime {
    client: Client,
    auth: Arc<RwLock<AuthState>>,
    model: String,
    tools: Arc<RwLock<ToolRegistry>>,
    system_prompt: Option<String>,
    /// Compiled, content-safe effective prompt metadata retained for inspection/reload.
    effective_prompt: Option<agent_core::prompt::PromptStack>,
    prompt_generation: u64,
    /// Inputs retained from boot so callable reloads compile the same manifest selection.
    prompt_reload_source: Option<PromptReloadSource>,
    thinking_budget: u32,
    /// User override for context window size (tokens). When set, takes
    /// precedence over the model's auto-detected window from
    /// `models::context_window_for_model`. Lets users cap context at e.g.
    /// 200k even on models that natively support 1M.
    context_window_override: Option<u64>,
    /// Model used for compaction. Falls back to claude-sonnet-4-6 if not set.
    compaction_model: Option<String>,
    /// Shared registry for reactive subagent handles.
    subagent_registry: Arc<Mutex<crate::runtime::subagent::SubagentRegistry>>,
    /// Session-scoped orchestration enforcement installed during boot.
    orchestration: Option<Arc<crate::orchestration::OrchestrationRuntime>>,
    /// Shared event queue — for Event Bus tooling.
    event_queue: Arc<crate::events::EventQueue>,
    /// Path for watcher_exit tool to write handoff state (agent mode only)
    pub watcher_exit_path: Option<PathBuf>,
    // New configurable fields
    max_tool_output: usize,
    bash_timeout: u64,
    bash_max_timeout: u64,
    subagent_timeout: u64,
    api_retries: u32,
    refusal_retries: u32,
    /// Telemetry level for structured per-request API logging (opt-in).
    telemetry_level: crate::runtime::telemetry::TelemetryLevel,
    /// Opt into the cache-diagnosis beta (`cache-diagnosis-2026-04-07`).
    cache_diagnostics: bool,
    /// Prompt-cache TTL strategy (5m default | 1h | hybrid). Threaded into
    /// every request via `ApiOptions`.
    cache_ttl: crate::core::config::CacheTtl,
    /// One-time-per-session latch for the silent 1h-downgrade notice
    /// (spec §3.4.1). Shared into `ApiOptions` for every request.
    ttl_downgrade_notified: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Session-scoped "1h honored at least once" latch (spec §3.4.1) —
    /// suppresses the downgrade notice on healthy Hybrid turns where the 1h
    /// prefix is already cached. Shared into `ApiOptions` for every request.
    saw_1h_honored: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Last Anthropic message id (`msg_...`) — threaded into the next
    /// request's `diagnostics.previous_message_id` when diagnostics is on.
    /// Reserved for the cache-diagnosis beta wiring (handoff item).
    #[allow(dead_code)]
    last_msg_id: Arc<Mutex<Option<String>>>,
    session_manager: std::sync::Arc<crate::tools::shell::SessionManager>,
    /// Extension hook bus for dispatching events to extensions.
    hook_bus: Arc<crate::extensions::hooks::HookBus>,
    // Held to keep the reaper task alive for the Runtime's lifetime; never read directly.
    #[allow(dead_code)]
    reaper_handle: Option<tokio::task::JoinHandle<()>>,
    #[allow(dead_code)]
    reaper_cancel: Option<tokio_util::sync::CancellationToken>,
    /// How provider credentials are resolved: `Local` (read/refresh auth.json —
    /// the default) or `Remote` (fetch short-lived access tokens from a broker).
    /// Set from config in `apply_config`. See task #157.
    credential_source: crate::auth::CredentialSource,
    /// In-memory cache of broker-fetched access tokens (Remote source only).
    /// Cheap to clone (Arc inside). Never persisted to disk.
    token_cache: crate::auth::TokenCache,
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        // Runtime construction is credential-blind. Credentials are acquired
        // lazily through the broker abstraction after configuration is applied;
        // this layer never opens auth.json or consults a secret environment var.
        let (auth_token, auth_type, refresh_token, token_expires) =
            (String::new(), "oauth".to_string(), None, Some(0));

        let client = Client::builder()
            .tls_built_in_webpki_certs(true)
            .tls_built_in_native_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| RuntimeError::Config(format!("Failed to build HTTP client: {}", e)))?;

        let session_manager = {
            let config = crate::tools::shell::ShellConfig::default();
            crate::tools::shell::SessionManager::new(config)
        };

        // Start the idle session reaper
        let mgr = session_manager.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let reaper_handle = crate::tools::shell::session::start_reaper(mgr, cancel.clone());

        Ok(Runtime {
            client,
            auth: Arc::new(RwLock::new(AuthState {
                auth_token,
                auth_type,
                refresh_token,
                token_expires,
            })),
            model: crate::models::default_model().to_string(),
            tools: Arc::new(RwLock::new(ToolRegistry::new())),
            system_prompt: None,
            effective_prompt: None,
            prompt_generation: 0,
            prompt_reload_source: None,
            thinking_budget: 4096,
            context_window_override: None,
            compaction_model: None,
            subagent_registry: Arc::new(Mutex::new(
                crate::runtime::subagent::SubagentRegistry::new(),
            )),
            orchestration: None,
            event_queue: Arc::new(crate::events::EventQueue::new(1000)),
            watcher_exit_path: None,
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            api_retries: 3,
            refusal_retries: 2,
            telemetry_level: crate::runtime::telemetry::TelemetryLevel::Off,
            cache_diagnostics: false,
            cache_ttl: crate::core::config::CacheTtl::default(),
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_msg_id: Arc::new(Mutex::new(None)),
            session_manager,
            hook_bus: Arc::new(crate::extensions::hooks::HookBus::new()),
            reaper_handle: Some(reaper_handle),
            reaper_cancel: Some(cancel),
            credential_source: crate::auth::CredentialSource::Local,
            token_cache: crate::auth::TokenCache::new(),
        })
    }

    /// Offline construction seam for headless test harnesses (P4).
    ///
    /// Identical to [`Runtime::new`] except:
    /// - auth is a stub token — no `auth.json` read, no keychain, no network
    /// - the idle shell-session reaper is not spawned — no tokio runtime
    ///   required at construction time
    ///
    /// Every accessor (`model()`, `thinking_level()`, tool registry, …) works
    /// normally; anything that would hit the Anthropic API fails at call time,
    /// which is the correct behavior for a harness that only drives the UI.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_headless() -> Self {
        let client = Client::builder()
            .tls_built_in_webpki_certs(true)
            .tls_built_in_native_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .expect("reqwest client construction is infallible with built-in roots");

        let session_manager = {
            let config = crate::tools::shell::ShellConfig::default();
            crate::tools::shell::SessionManager::new(config)
        };

        Runtime {
            client,
            auth: Arc::new(RwLock::new(AuthState {
                auth_token: "test-token".to_string(),
                auth_type: "api_key".to_string(),
                refresh_token: None,
                token_expires: None,
            })),
            model: crate::models::default_model().to_string(),
            tools: Arc::new(RwLock::new(ToolRegistry::new())),
            system_prompt: None,
            effective_prompt: None,
            prompt_generation: 0,
            prompt_reload_source: None,
            thinking_budget: 4096,
            context_window_override: None,
            compaction_model: None,
            subagent_registry: Arc::new(Mutex::new(
                crate::runtime::subagent::SubagentRegistry::new(),
            )),
            orchestration: None,
            event_queue: Arc::new(crate::events::EventQueue::new(1000)),
            watcher_exit_path: None,
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            api_retries: 3,
            refusal_retries: 2,
            telemetry_level: crate::runtime::telemetry::TelemetryLevel::Off,
            cache_diagnostics: false,
            cache_ttl: crate::core::config::CacheTtl::default(),
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_msg_id: Arc::new(Mutex::new(None)),
            session_manager,
            hook_bus: Arc::new(crate::extensions::hooks::HookBus::new()),
            reaper_handle: None,
            reaper_cancel: None,
            credential_source: crate::auth::CredentialSource::Local,
            token_cache: crate::auth::TokenCache::new(),
        }
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = Some(prompt);
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// The system prompt that actually goes on the wire for the next
    /// request: the configured base plus any builtin orchestration adapter
    /// (see [`agent_core::prompt::builtin_orchestration_adapters`]) selected
    /// by the *current* model identity, so the doctrine follows mid-session
    /// model switches. Composition is skipped when:
    /// - a typed prompt manifest is active — the manifest author owns the
    ///   full stack;
    /// - the runtime exposes no subagent tools (worker runtimes) — the
    ///   doctrine would be unactionable noise;
    /// - the model identity cannot be canonicalized — fail closed to the
    ///   unmodified base.
    pub async fn effective_system_prompt(&self) -> Option<String> {
        if self.effective_prompt.is_some() {
            return self.system_prompt.clone();
        }
        if self.tools.read().await.get("subagent_start").is_none() {
            return self.system_prompt.clone();
        }
        let Ok(model) = crate::orchestration::canonical_foreground_identity(&self.model) else {
            return self.system_prompt.clone();
        };
        let Ok(context) = agent_core::prompt::SelectionContext::new(model, None) else {
            return self.system_prompt.clone();
        };
        agent_core::prompt::compose_orchestration_prompt(self.system_prompt.as_deref(), &context)
    }

    pub fn effective_prompt(&self) -> Option<&agent_core::prompt::PromptStack> {
        self.effective_prompt.as_ref()
    }

    pub fn prompt_generation(&self) -> u64 {
        self.prompt_generation
    }

    pub fn retain_prompt_reload_source(
        &mut self,
        manifest: PathBuf,
        context: agent_core::prompt::SelectionContext,
        user_module: Option<agent_core::prompt::PromptModule>,
        delegation_policy_digest: Option<String>,
    ) {
        self.prompt_reload_source = Some(PromptReloadSource {
            manifest,
            context,
            user_module,
            delegation_policy_digest,
        });
    }

    /// Recompile retained manifest inputs, validate, then atomically install the candidate.
    pub fn reload_prompt(&mut self) -> std::result::Result<u64, agent_core::prompt::PromptError> {
        let source = self.prompt_reload_source.clone().ok_or_else(|| {
            agent_core::prompt::PromptError::Invalid("no prompt manifest is active".into())
        })?;
        let raw = std::fs::read_to_string(&source.manifest).map_err(|_| {
            agent_core::prompt::PromptError::Invalid("prompt manifest is unavailable".into())
        })?;
        let manifest = agent_core::prompt::PromptManifest::parse(&raw)?;
        let reload_catalog = crate::orchestration::OrchestrationRuntime::trusted_catalog(
            source.context.model(),
            manifest.delegation_catalog_candidates(),
        )
        .map_err(|error| agent_core::prompt::PromptError::Invalid(error.into()))?;
        let candidate_policy_digest = manifest
            .delegation_policy(source.context.model().clone(), &reload_catalog)?
            .map(|policy| policy.digest());
        if candidate_policy_digest != source.delegation_policy_digest {
            return Err(agent_core::prompt::PromptError::Invalid(
                "hot reload cannot safely change delegation policy".into(),
            ));
        }
        let registry = manifest.registry(source.manifest.parent())?;
        let candidate = agent_core::prompt::compile_prompt_stack(
            &manifest,
            &registry,
            &source.context,
            source.user_module,
        )?;
        self.apply_prompt_stack(candidate)
    }

    /// Atomically validate and install a compiled stack. Failed validation leaves all state intact.
    pub fn apply_prompt_stack(
        &mut self,
        candidate: agent_core::prompt::PromptStack,
    ) -> std::result::Result<u64, agent_core::prompt::PromptError> {
        if let Some(current) = &self.effective_prompt {
            current.validate_hot_reload(&candidate)?;
        }
        let composed = candidate.composed().to_owned();
        self.effective_prompt = Some(candidate);
        self.system_prompt = Some(composed);
        self.prompt_generation = self.prompt_generation.saturating_add(1);
        Ok(self.prompt_generation)
    }

    pub fn prompt_inspection_json(&self) -> Option<String> {
        let mode = self
            .orchestration
            .as_ref()
            .map(|runtime| runtime.enforcement_mode())
            .unwrap_or(agent_core::orchestration::EnforcementMode::Off);
        self.effective_prompt.as_ref().and_then(|stack| {
            serde_json::to_string(&serde_json::json!({
                "generation": self.prompt_generation,
                "effective": stack.inspect(mode),
                "token_estimate": stack.composed().len().div_ceil(4),
            }))
            .ok()
        })
    }

    pub fn install_orchestration(
        &mut self,
        runtime: Arc<crate::orchestration::OrchestrationRuntime>,
    ) {
        self.orchestration = Some(runtime);
    }

    pub fn orchestration(&self) -> Option<&Arc<crate::orchestration::OrchestrationRuntime>> {
        self.orchestration.as_ref()
    }

    pub fn set_model(&mut self, model: String) {
        // Older model pickers passed the rendered health row back here, e.g.
        // `✅  339ms  groq/llama-3.3-70b`. Remove that exact decoration shape,
        // rather than searching inside the ID: provider-qualified IDs may
        // legitimately contain `claude-` after their slash.
        let trimmed = model.trim();
        let cleaned = trimmed
            .split_once(char::is_whitespace)
            .and_then(|(_, rest)| {
                let rest = rest.trim_start();
                let (latency, candidate) = rest.split_once(char::is_whitespace)?;
                let millis = latency.strip_suffix("ms")?;
                if !millis.is_empty() && millis.chars().all(|c| c.is_ascii_digit()) {
                    let candidate = candidate.trim();
                    if !candidate.is_empty() && !candidate.chars().any(char::is_whitespace) {
                        return Some(candidate);
                    }
                }
                None
            })
            .unwrap_or(trimmed);
        self.model = cleaned.to_owned();
    }

    pub fn set_tools(&mut self, tools: ToolRegistry) {
        self.tools = Arc::new(RwLock::new(tools));
    }

    pub fn subagent_registry(&self) -> &Arc<Mutex<crate::runtime::subagent::SubagentRegistry>> {
        &self.subagent_registry
    }

    pub fn event_queue(&self) -> &Arc<crate::events::EventQueue> {
        &self.event_queue
    }

    /// Get a shared reference to the extension hook bus.
    pub fn hook_bus(&self) -> &Arc<crate::extensions::hooks::HookBus> {
        &self.hook_bus
    }

    /// Get a shared reference to the tool registry (for MCP lazy loading).
    pub fn tools_shared(&self) -> Arc<RwLock<ToolRegistry>> {
        Arc::clone(&self.tools)
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn http_client(&self) -> &Client {
        &self.client
    }
    pub fn set_thinking_budget(&mut self, budget: u32) {
        self.thinking_budget = budget;
    }

    pub fn set_compaction_model(&mut self, model: Option<String>) {
        self.compaction_model = model;
    }

    pub fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window_override = window;
    }

    /// Effective context window for the current model — user override if set,
    /// otherwise the model's native window from `models::context_window_for_model`.
    pub fn compaction_model(&self) -> &str {
        self.compaction_model
            .as_deref()
            .unwrap_or("claude-sonnet-4-6")
    }

    pub fn context_window(&self) -> u64 {
        self.context_window_override
            .unwrap_or_else(|| crate::models::context_window_for_model(&self.model))
    }

    /// Apply a parsed config file to this runtime (model, thinking budget, etc.)
    pub fn apply_config(&mut self, config: &crate::config::SynapsConfig) {
        if let Some(ref model) = config.model {
            self.set_model(model.clone());
        }
        if let Some(budget) = config.thinking_budget {
            self.set_thinking_budget(budget);
        }
        self.context_window_override = config.context_window;
        self.compaction_model = config.compaction_model.clone();
        self.max_tool_output = config.max_tool_output;
        self.bash_timeout = config.bash_timeout;
        self.bash_max_timeout = config.bash_max_timeout;
        self.subagent_timeout = config.subagent_timeout;
        self.api_retries = config.api_retries;
        self.refusal_retries = config.refusal_retries;
        self.telemetry_level =
            crate::runtime::telemetry::TelemetryLevel::from_str_key(&config.telemetry);
        self.cache_diagnostics = config.cache_diagnostics;
        self.cache_ttl = config.cache_ttl;
        self.apply_auth_config(config);

        // Remove any built-in tools the user disabled via `disabled_tools`.
        // try_write is safe here: apply_config runs at boot before the registry
        // is shared with other tasks.
        if !config.disabled_tools.is_empty() {
            if let Ok(mut reg) = self.tools.try_write() {
                reg.disable(&config.disabled_tools);
            }
        }
    }

    /// Apply only the credential-source portion of config. Used by subagent
    /// spawns that build a fresh `Runtime::new()` (which defaults to Local) and
    /// must inherit the user's Remote/broker setup. (#158 A3)
    ///
    /// A6: invalidates the token cache if the source/endpoint changed.
    /// A2: scrubs `AuthState` when Remote so no local `auth.json` credential is
    /// used or held (invariant 1).
    pub fn apply_auth_config(&mut self, config: &crate::config::SynapsConfig) {
        let new_source = config.auth.credential_source();
        if new_source != self.credential_source {
            self.token_cache.invalidate("anthropic");
        }
        self.credential_source = new_source;
        if self.credential_source.is_remote() {
            if let Ok(mut auth) = self.auth.try_write() {
                AuthMethods::scrub_for_remote(&mut auth);
            }
        }
        // Install the process-wide credential broker matching this source so
        // every request path (streams, pings, catalog, TUI status) resolves
        // credentials through the same boundary. Local sources get the
        // in-process broker — no separately launched daemon required.
        crate::auth::set_global_broker(crate::auth::broker_from_source(
            &self.credential_source,
            &self.token_cache,
            self.client.clone(),
        ));
    }

    pub fn thinking_budget(&self) -> u32 {
        self.thinking_budget
    }

    pub fn max_tool_output(&self) -> usize {
        self.max_tool_output
    }

    pub fn bash_timeout(&self) -> u64 {
        self.bash_timeout
    }

    pub fn bash_max_timeout(&self) -> u64 {
        self.bash_max_timeout
    }

    pub fn subagent_timeout(&self) -> u64 {
        self.subagent_timeout
    }

    pub fn api_retries(&self) -> u32 {
        self.api_retries
    }

    pub fn set_max_tool_output(&mut self, v: usize) {
        self.max_tool_output = v;
    }

    pub fn set_bash_timeout(&mut self, v: u64) {
        self.bash_timeout = v;
    }

    pub fn set_bash_max_timeout(&mut self, v: u64) {
        self.bash_max_timeout = v;
    }

    pub fn set_subagent_timeout(&mut self, v: u64) {
        self.subagent_timeout = v;
    }

    pub fn set_api_retries(&mut self, v: u32) {
        self.api_retries = v;
    }

    pub fn telemetry_level(&self) -> crate::runtime::telemetry::TelemetryLevel {
        self.telemetry_level
    }

    pub fn set_telemetry_level(&mut self, level: crate::runtime::telemetry::TelemetryLevel) {
        self.telemetry_level = level;
    }

    pub fn cache_diagnostics(&self) -> bool {
        self.cache_diagnostics
    }

    pub fn set_cache_diagnostics(&mut self, v: bool) {
        self.cache_diagnostics = v;
    }

    pub fn cache_ttl(&self) -> crate::core::config::CacheTtl {
        self.cache_ttl
    }

    /// Change the cache TTL strategy mid-session. The next request re-marks
    /// with the new TTL; the old prefix expires naturally (single-last
    /// strategy never prunes old markers, so no invalidation logic needed).
    pub fn set_cache_ttl(&mut self, ttl: crate::core::config::CacheTtl) {
        self.cache_ttl = ttl;
    }

    pub fn thinking_level(&self) -> &str {
        crate::core::models::thinking_level_for_budget(self.thinking_budget)
    }

    /// Check if the OAuth token is expired and refresh it if needed.
    pub async fn refresh_if_needed(&self) -> Result<()> {
        // Non-Anthropic models resolve their own provider auth in the OpenAI
        // path (incl. via the broker), so skip the Anthropic pre-fetch. (#158 #7)
        if !crate::runtime::auth::model_is_anthropic(&self.model) {
            return Ok(());
        }
        AuthMethods::refresh_if_needed(
            Arc::clone(&self.auth),
            &self.client,
            &self.credential_source,
            &self.token_cache,
        )
        .await
    }

    /// Make a simple non-streaming API call for compaction (no tools).
    ///
    /// Uses a dedicated summarization system prompt (not the user's), omits
    /// all tools, and returns the raw text response. Caller supplies the
    /// full message array including the serialized conversation.
    pub async fn compact_call(&self, messages: Vec<crate::SharedMessage>) -> Result<String> {
        self.refresh_if_needed().await?;

        use crate::runtime::compaction::COMPACTION_SYSTEM_PROMPT;

        ApiMethods::call_api_simple(
            &self.auth,
            &self.client,
            self.compaction_model(),
            COMPACTION_SYSTEM_PROMPT,
            self.thinking_budget,
            &messages,
            self.api_retries,
        )
        .await
    }

    /// Run a single prompt synchronously (non-streaming). Handles tool execution
    /// internally, looping until the model produces a final text response.
    pub async fn run_single(&self, prompt: &str) -> Result<String> {
        // Refresh OAuth token if expired
        self.refresh_if_needed().await?;

        let mut messages: Vec<crate::SharedMessage> = vec![std::sync::Arc::new(
            json!({"role": "user", "content": prompt}),
        )];
        let system_prompt = self.effective_system_prompt().await;

        loop {
            let response = ApiMethods::call_api(
                &self.auth,
                &self.client,
                &self.model,
                &*self.tools.read().await,
                &system_prompt,
                self.thinking_budget,
                &messages,
                self.api_retries,
                &api::ApiOptions {
                    use_1m_context: self.context_window_override == Some(1_000_000),
                    cache_ttl: self.cache_ttl,
                    ttl_downgrade_notified: self.ttl_downgrade_notified.clone(),
                    saw_1h_honored: self.saw_1h_honored.clone(),
                    credential_source: self.credential_source.clone(),
                    token_cache: self.token_cache.clone(),
                    anthropic_base_url: None,
                },
            )
            .await?;

            // Check if Claude wants to use tools
            if let Some(content) = response["content"].as_array() {
                let mut response_text = String::new();
                let mut tool_uses = Vec::new();

                // Process response content
                for item in content {
                    match item["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = item["text"].as_str() {
                                response_text.push_str(text);
                            }
                        }
                        Some("tool_use") => {
                            tool_uses.push(item.clone());
                        }
                        _ => {}
                    }
                }

                // If no tool uses, return the text response
                if tool_uses.is_empty() {
                    if let Some(orchestration) = &self.orchestration {
                        if let agent_core::orchestration::CompletionGate::Blocked { workers } =
                            orchestration.completion_gate()
                        {
                            return Err(crate::RuntimeError::Tool(format!(
                                "completion blocked: {} worker(s) require collection/reconciliation: {} (call subagent_collect with reconciled=true after inspecting each result)",
                                workers.len(),
                                workers.join(", ")
                            )));
                        }
                    }
                    return Ok(response_text);
                }

                // Add assistant's response to conversation (only content, role)
                messages.push(std::sync::Arc::new(json!({
                    "role": "assistant",
                    "content": content
                })));

                // Execute tools — parallel when multiple are requested
                let mut tool_results = Vec::new();

                if tool_uses.len() == 1 {
                    // Single tool — run inline, no spawn overhead
                    let tool_use = &tool_uses[0];
                    if let (Some(tool_name), Some(tool_id)) =
                        (tool_use["name"].as_str(), tool_use["id"].as_str())
                    {
                        let input = &tool_use["input"];
                        let result = match self.tools.read().await.get(tool_name).cloned() {
                            Some(tool) => {
                                let input = self
                                    .tools
                                    .read()
                                    .await
                                    .translate_input_for_api_tool(tool_name, input.clone());
                                let runtime_name = self
                                    .tools
                                    .read()
                                    .await
                                    .runtime_name_for_api(tool_name)
                                    .to_string();
                                let ctx = crate::ToolContext {
                                    channels: crate::tools::ToolChannels {
                                        tx_delta: None,
                                        tx_events: None,
                                    },
                                    capabilities: crate::tools::ToolCapabilities {
                                        watcher_exit_path: self.watcher_exit_path.clone(),
                                        tool_register_tx: None,
                                        session_manager: Some(self.session_manager.clone()),
                                        subagent_registry: Some(self.subagent_registry.clone()),
                                        event_queue: Some(self.event_queue.clone()),
                                        secret_prompt: None,
                                        orchestration: self.orchestration.clone(),
                                    },
                                    limits: crate::tools::ToolLimits {
                                        max_tool_output: self.max_tool_output,
                                        max_tool_buffer: 256 * 1024,
                                        bash_timeout: self.bash_timeout,
                                        bash_max_timeout: self.bash_max_timeout,
                                        subagent_timeout: self.subagent_timeout,
                                    },
                                };
                                let decision = resolve_before_tool_call_decision(
                                    input.clone(),
                                    emit_before_tool_call(
                                        &self.hook_bus,
                                        tool_name,
                                        Some(&runtime_name),
                                        input.clone(),
                                    )
                                    .await,
                                    None,
                                    false,
                                )
                                .await;
                                if let BeforeToolCallDecision::Block { reason } = decision {
                                    format!("Tool call blocked by extension: {}", reason)
                                } else {
                                    let BeforeToolCallDecision::Continue { input } = decision
                                    else {
                                        unreachable!()
                                    };
                                    let input_for_hook = input.clone();
                                    let output = match tool.execute(input, ctx).await {
                                        Ok(output) => output,
                                        Err(e) => e.to_string(),
                                    };
                                    let output = emit_after_tool_call(
                                        &self.hook_bus,
                                        tool_name,
                                        Some(&runtime_name),
                                        input_for_hook,
                                        output,
                                        self.max_tool_output,
                                    )
                                    .await;
                                    output
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": HelperMethods::truncate_tool_result(&result, self.max_tool_output)
                        }));
                    }
                } else {
                    // Multiple tools — run in parallel with JoinSet
                    let mut join_set = tokio::task::JoinSet::new();

                    // Capture config values before spawning (can't borrow &self in 'static spawn)
                    let cfg_max_tool_output = self.max_tool_output;
                    let cfg_bash_timeout = self.bash_timeout;
                    let cfg_bash_max_timeout = self.bash_max_timeout;
                    let cfg_subagent_timeout = self.subagent_timeout;
                    let session_mgr = self.session_manager.clone();
                    let cfg_subagent_registry = self.subagent_registry.clone();
                    let cfg_event_queue = self.event_queue.clone();
                    let cfg_hook_bus = self.hook_bus.clone();
                    let cfg_orchestration = self.orchestration.clone();

                    for tool_use in &tool_uses {
                        if let (Some(tool_name), Some(tool_id)) = (
                            tool_use["name"].as_str().map(|s| s.to_string()),
                            tool_use["id"].as_str().map(|s| s.to_string()),
                        ) {
                            let input = tool_use["input"].clone();
                            let tools_snapshot = self.tools.read().await;
                            let input =
                                tools_snapshot.translate_input_for_api_tool(&tool_name, input);
                            let runtime_name =
                                tools_snapshot.runtime_name_for_api(&tool_name).to_string();
                            let tool = tools_snapshot.get(&tool_name).cloned();
                            drop(tools_snapshot);
                            let exit_path = self.watcher_exit_path.clone();
                            let session_mgr_inner = session_mgr.clone();
                            let registry_inner = cfg_subagent_registry.clone();
                            let event_queue_inner = cfg_event_queue.clone();
                            let hook_bus_inner = cfg_hook_bus.clone();
                            let orchestration_inner = cfg_orchestration.clone();
                            let tool_name_for_hook = tool_name.clone();
                            let runtime_name_for_hook = runtime_name.clone();

                            join_set.spawn(async move {
                                let result = match tool {
                                    Some(t) => {
                                        let decision =
                                            crate::runtime::resolve_before_tool_call_decision(
                                                input.clone(),
                                                crate::runtime::emit_before_tool_call(
                                                    &hook_bus_inner,
                                                    &tool_name_for_hook,
                                                    Some(&runtime_name_for_hook),
                                                    input.clone(),
                                                )
                                                .await,
                                                None,
                                                false,
                                            )
                                            .await;
                                        if let crate::runtime::BeforeToolCallDecision::Block {
                                            reason,
                                        } = decision
                                        {
                                            format!("Tool call blocked by extension: {}", reason)
                                        } else {
                                            let crate::runtime::BeforeToolCallDecision::Continue {
                                                input,
                                            } = decision
                                            else {
                                                unreachable!()
                                            };
                                            let ctx = crate::ToolContext {
                                                channels: crate::tools::ToolChannels {
                                                    tx_delta: None,
                                                    tx_events: None,
                                                },
                                                capabilities: crate::tools::ToolCapabilities {
                                                    watcher_exit_path: exit_path,
                                                    tool_register_tx: None,
                                                    session_manager: Some(session_mgr_inner),
                                                    subagent_registry: Some(registry_inner),
                                                    event_queue: Some(event_queue_inner),
                                                    secret_prompt: None,
                                                    orchestration: orchestration_inner,
                                                },
                                                limits: crate::tools::ToolLimits {
                                                    max_tool_output: cfg_max_tool_output,
                                                    max_tool_buffer: 256 * 1024,
                                                    bash_timeout: cfg_bash_timeout,
                                                    bash_max_timeout: cfg_bash_max_timeout,
                                                    subagent_timeout: cfg_subagent_timeout,
                                                },
                                            };
                                            let input_for_hook = input.clone();
                                            let output = match t.execute(input, ctx).await {
                                                Ok(output) => output,
                                                Err(e) => e.to_string(),
                                            };
                                            let output = crate::runtime::emit_after_tool_call(
                                                &hook_bus_inner,
                                                &tool_name_for_hook,
                                                Some(&runtime_name_for_hook),
                                                input_for_hook,
                                                output,
                                                cfg_max_tool_output,
                                            )
                                            .await;
                                            output
                                        }
                                    }
                                    None => format!("Unknown tool: {}", tool_name),
                                };
                                (tool_id, result)
                            });
                        }
                    }

                    // Collect results, preserving order by tool_id
                    let mut results_map = std::collections::HashMap::new();
                    while let Some(res) = join_set.join_next().await {
                        match res {
                            Ok((tool_id, result)) => {
                                results_map.insert(tool_id, result);
                            }
                            Err(e) => {
                                // Task panicked — log it but don't crash
                                tracing::error!("Parallel tool task panicked: {}", e);
                            }
                        }
                    }

                    // Build tool_results in original order — every tool_use MUST have a result
                    for tool_use in &tool_uses {
                        if let Some(tool_id) = tool_use["id"].as_str() {
                            let result = results_map.remove(tool_id).unwrap_or_else(|| {
                                "Tool execution failed: task panicked".to_string()
                            });
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": HelperMethods::truncate_tool_result(&result, self.max_tool_output)
                            }));
                        }
                    }
                }

                // Add tool results to conversation
                messages.push(std::sync::Arc::new(json!({
                    "role": "user",
                    "content": tool_results
                })));

                // Continue the loop to get Claude's response with tool results
            } else {
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }

    /// Run a prompt as a cancellable stream of [`StreamEvent`]s. Convenience wrapper
    /// around [`run_stream_with_messages`] for single-turn usage.
    pub async fn run_stream(
        &self,
        prompt: String,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.run_stream_with_messages(
            vec![std::sync::Arc::new(
                json!({"role": "user", "content": prompt}),
            )],
            cancel,
            None,
            None,
            false,
        )
        .await
    }

    /// Run a multi-turn conversation as a cancellable stream of [`StreamEvent`]s.
    /// This is the main entry point for chat UIs and agents. Handles tool execution,
    /// API retries, and dynamic tool registration (MCP) internally.
    pub async fn run_stream_with_messages(
        &self,
        messages: Vec<crate::SharedMessage>,
        cancel: CancellationToken,
        steering_rx: Option<mpsc::UnboundedReceiver<String>>,
        secret_prompt: Option<crate::tools::SecretPromptHandle>,
        auto_approve_confirms: bool,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Refresh OAuth token if expired before starting the stream.
        if let Err(e) = self.refresh_if_needed().await {
            let _ = tx.send(StreamEvent::Session(SessionEvent::Error(e.to_string())));
            let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
            return Box::pin(UnboundedReceiverStream::new(rx));
        }

        // Clone the Arc, not the whole Runtime — the spawned task shares the
        // same AuthState so mid-loop token refreshes are visible immediately.
        let auth = Arc::clone(&self.auth);
        let client = self.client.clone();
        let credential_source = self.credential_source.clone();
        let token_cache = self.token_cache.clone();
        let model = self.model.clone();
        let tools = self.tools.clone();
        let system_prompt = self.effective_system_prompt().await;
        let thinking_budget = self.thinking_budget;
        let watcher_exit_path = self.watcher_exit_path.clone();
        let max_tool_output = self.max_tool_output;
        let bash_timeout = self.bash_timeout;
        let bash_max_timeout = self.bash_max_timeout;
        let subagent_timeout = self.subagent_timeout;
        let api_retries = self.api_retries;
        let refusal_retries = self.refusal_retries;
        let session_manager = self.session_manager.clone();
        // Opt into the 1M-context beta header only when the user explicitly
        // requested 1M (via context_window setting). Default 200k matches
        // Anthropic's claude-code default and gives smarter inference.
        let subagent_registry = self.subagent_registry.clone();
        // Extra Arc clone for the reaper hook — session takes ownership of the
        // original clone above; this one is captured separately by the spawn closure.
        let reaper_registry = Arc::clone(&subagent_registry);
        let event_queue = self.event_queue.clone();
        let options = api::ApiOptions {
            use_1m_context: self.context_window_override == Some(1_000_000),
            cache_ttl: self.cache_ttl,
            ttl_downgrade_notified: self.ttl_downgrade_notified.clone(),
            saw_1h_honored: self.saw_1h_honored.clone(),
            credential_source: self.credential_source.clone(),
            token_cache: self.token_cache.clone(),
            anthropic_base_url: None,
        };

        let session = crate::runtime::stream::StreamSession {
            auth,
            client,
            credential_source,
            token_cache,
            options,
            api_retries,
            refusal_retries,
            model,
            tools,
            system_prompt,
            thinking_budget,
            tx: tx.clone(),
            cancel,
            steering_rx,
            watcher_exit_path,
            max_tool_output,
            bash_timeout,
            bash_max_timeout,
            subagent_timeout,
            session_manager,
            subagent_registry,
            event_queue,
            secret_prompt,
            hook_bus: self.hook_bus.clone(),
            auto_approve_confirms,
            telemetry_level: self.telemetry_level,
            orchestration: self.orchestration.clone(),
        };

        tokio::spawn(async move {
            if let Err(e) = StreamMethods::run_stream_internal(session, messages).await {
                let _ = tx.send(StreamEvent::Session(SessionEvent::Error(e.to_string())));
            }
            // Engine-owned housekeeping: reap finished subagent handles before
            // signalling Done.  Runs on the tokio thread pool — no public sync
            // caller becomes async.  Poison-safe via reap_finished internals.
            crate::runtime::subagent::reap_finished(&reaper_registry);
            let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
        });

        Box::pin(UnboundedReceiverStream::new(rx))
    }
}

impl Clone for Runtime {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            auth: Arc::clone(&self.auth),
            model: self.model.clone(),
            tools: self.tools.clone(),
            system_prompt: self.system_prompt.clone(),
            effective_prompt: self.effective_prompt.clone(),
            prompt_generation: self.prompt_generation,
            prompt_reload_source: self.prompt_reload_source.clone(),
            thinking_budget: self.thinking_budget,
            context_window_override: self.context_window_override,
            compaction_model: self.compaction_model.clone(),
            subagent_registry: self.subagent_registry.clone(),
            orchestration: self.orchestration.clone(),
            event_queue: self.event_queue.clone(),
            watcher_exit_path: self.watcher_exit_path.clone(),
            max_tool_output: self.max_tool_output,
            bash_timeout: self.bash_timeout,
            bash_max_timeout: self.bash_max_timeout,
            subagent_timeout: self.subagent_timeout,
            api_retries: self.api_retries,
            refusal_retries: self.refusal_retries,
            telemetry_level: self.telemetry_level,
            cache_diagnostics: self.cache_diagnostics,
            cache_ttl: self.cache_ttl,
            // Subagents are their own session — fresh latches so a downgrade
            // in the subagent's chain surfaces its own (single) notice and
            // honored-state isn't inherited from the parent.
            ttl_downgrade_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            saw_1h_honored: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // Subagents start their own request chain — inheriting the parent's
            // last msg_id would produce bogus `messages_changed` diagnostics.
            last_msg_id: Arc::new(Mutex::new(None)),
            session_manager: self.session_manager.clone(),
            hook_bus: self.hook_bus.clone(),
            reaper_handle: None, // Cloned runtimes don't own the reaper
            reaper_cancel: None, // Cloned runtimes don't own the reaper
            credential_source: self.credential_source.clone(),
            token_cache: self.token_cache.clone(), // shares the same cache (Arc inside)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_rejects_policy_change_without_partial_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.yaml");
        let manifest = |total: &str, content: &str| {
            format!(
            "schema: synaps-prompt/1\nkernel: kernel\nmodules:\n  - id: kernel\n    version: v1\n    source: builtin\n    priority: 0\n    selectors: {{}}\n    mutability: mutable_guidance\n    content: {content}\npolicies:\n  delegation:\n    mode: enforced\n    allowed_models: [anthropic/claude-sonnet-4-6]\n    max_concurrent_workers: 1\n    max_total_workers: {total}\n"
        )
        };
        std::fs::write(&path, manifest("1", "before")).unwrap();
        let parsed =
            agent_core::prompt::PromptManifest::parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap();
        let model =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
        let context = agent_core::prompt::SelectionContext::new(model.clone(), None).unwrap();
        let stack = agent_core::prompt::compile_prompt_stack(
            &parsed,
            &parsed.registry(path.parent()).unwrap(),
            &context,
            None,
        )
        .unwrap();
        let catalog = crate::orchestration::OrchestrationRuntime::trusted_catalog(
            &model,
            parsed.delegation_catalog_candidates(),
        )
        .unwrap();
        let digest = parsed
            .delegation_policy(model, &catalog)
            .unwrap()
            .map(|p| p.digest());
        let mut runtime = Runtime::new_headless();
        runtime.apply_prompt_stack(stack).unwrap();
        runtime.retain_prompt_reload_source(path.clone(), context, None, digest.clone());
        let generation = runtime.prompt_generation();
        let composed = runtime.effective_prompt().unwrap().composed().to_owned();

        std::fs::write(&path, manifest("2", "after")).unwrap();
        let error = runtime.reload_prompt().unwrap_err().to_string();
        assert!(error.contains("cannot safely change delegation policy"));
        assert_eq!(runtime.prompt_generation(), generation);
        assert_eq!(runtime.effective_prompt().unwrap().composed(), composed);
        assert_eq!(
            runtime
                .prompt_reload_source
                .as_ref()
                .unwrap()
                .delegation_policy_digest,
            digest
        );
    }

    #[test]
    fn set_model_preserves_qualified_provider_ids_and_bare_claude() {
        let cases = [
            (
                "github-copilot/claude-opus-4.8",
                "github-copilot/claude-opus-4.8",
            ),
            ("anthropic/claude-sonnet-4-6", "anthropic/claude-sonnet-4-6"),
            ("google/gemini-2.5-pro", "google/gemini-2.5-pro"),
            ("openai/gpt-5", "openai/gpt-5"),
            ("claude-opus-4-8", "claude-opus-4-8"),
        ];

        for (input, expected) in cases {
            let mut runtime = Runtime::new_headless();
            runtime.set_model(input.to_owned());
            assert_eq!(runtime.model(), expected, "input: {input}");
        }
    }

    #[test]
    fn set_model_strips_only_legacy_health_status_decoration() {
        let cases = [
            ("✅  339ms  groq/llama-3.3-70b", "groq/llama-3.3-70b"),
            (
                "✅  339ms  github-copilot/claude-opus-4.8",
                "github-copilot/claude-opus-4.8",
            ),
            (
                "⚠️  1200ms  anthropic/claude-sonnet-4-6",
                "anthropic/claude-sonnet-4-6",
            ),
        ];

        for (input, expected) in cases {
            let mut runtime = Runtime::new_headless();
            runtime.set_model(input.to_owned());
            assert_eq!(runtime.model(), expected, "input: {input}");
        }
    }

    #[tokio::test]
    async fn confirm_without_prompt_fails_closed() {
        let result = resolve_before_tool_call_result(
            crate::extensions::hooks::events::HookResult::Confirm {
                message: "Run deploy?".into(),
            },
            None,
            false,
        )
        .await;

        assert!(matches!(
            result,
            crate::extensions::hooks::events::HookResult::Block { reason }
                if reason.contains("requires confirmation") && reason.contains("Run deploy?")
        ));
    }

    #[tokio::test]
    async fn modify_result_replaces_tool_input() {
        let result = resolve_before_tool_call_decision(
            serde_json::json!({"command":"rm -rf /"}),
            crate::extensions::hooks::events::HookResult::Modify {
                input: serde_json::json!({"command":"echo safe"}),
            },
            None,
            false,
        )
        .await;

        match result {
            BeforeToolCallDecision::Continue { input } => {
                assert_eq!(input, serde_json::json!({"command":"echo safe"}));
            }
            BeforeToolCallDecision::Block { reason } => panic!("unexpected block: {reason}"),
        }
    }

    #[tokio::test]
    async fn confirm_prompt_yes_continues() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = crate::tools::SecretPromptHandle::new(tx);

        let task = tokio::spawn(async move {
            let request = rx.recv().await.expect("confirm prompt request");
            assert_eq!(request.title, "Confirm tool call");
            assert!(request.prompt.contains("Run deploy?"));
            let _ = request.response_tx.send(Some("yes".to_string()));
        });

        let result = resolve_before_tool_call_result(
            crate::extensions::hooks::events::HookResult::Confirm {
                message: "Run deploy?".into(),
            },
            Some(&handle),
            false,
        )
        .await;

        task.await.unwrap();
        assert!(matches!(
            result,
            crate::extensions::hooks::events::HookResult::Continue
        ));
    }

    #[tokio::test]
    async fn confirm_prompt_non_yes_blocks() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = crate::tools::SecretPromptHandle::new(tx);

        let task = tokio::spawn(async move {
            let request = rx.recv().await.expect("confirm prompt request");
            let _ = request.response_tx.send(Some("no".to_string()));
        });

        let result = resolve_before_tool_call_result(
            crate::extensions::hooks::events::HookResult::Confirm {
                message: "Run deploy?".into(),
            },
            Some(&handle),
            false,
        )
        .await;

        task.await.unwrap();
        assert!(matches!(
            result,
            crate::extensions::hooks::events::HookResult::Block { reason }
                if reason.contains("confirmation denied")
        ));
    }

    #[test]
    fn test_max_tokens_for_model() {
        // Opus models should return 128000
        assert_eq!(
            HelperMethods::max_tokens_for_model("claude-opus-4-6"),
            128000
        );
        assert_eq!(
            HelperMethods::max_tokens_for_model("opus-something"),
            128000
        );

        // Non-opus models should return 64000
        assert_eq!(
            HelperMethods::max_tokens_for_model("claude-sonnet-4-20250514"),
            64000
        );
        assert_eq!(HelperMethods::max_tokens_for_model("haiku"), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("claude-3-haiku"), 64000);
        assert_eq!(
            HelperMethods::max_tokens_for_model("some-other-model"),
            64000
        );

        // Edge cases
        assert_eq!(HelperMethods::max_tokens_for_model(""), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("OPUS"), 64000); // Case sensitive - uppercase doesn't match
        assert_eq!(
            HelperMethods::max_tokens_for_model("model-opus-end"),
            128000
        ); // Contains "opus" anywhere
    }

    #[test]
    fn test_truncate_tool_result() {
        let default_max = 30000;

        // Short string should remain unchanged
        let short = "This is a short string.";
        assert_eq!(
            HelperMethods::truncate_tool_result(short, default_max),
            short
        );

        // Exactly max should remain unchanged
        let exact = "x".repeat(30000);
        assert_eq!(
            HelperMethods::truncate_tool_result(&exact, default_max),
            exact
        );

        // String longer than max should be truncated with notice
        let too_long = "x".repeat(30001);
        let truncated = HelperMethods::truncate_tool_result(&too_long, default_max);

        // Should start with the truncated content
        assert!(truncated.starts_with(&"x".repeat(30000)));

        // Should contain truncation notice with total char count
        assert!(truncated.contains("[truncated — 30001 total chars, showing first 30000]"));

        // Should be longer than max (due to notice)
        assert!(truncated.len() > 30000);

        // Test with a much longer string
        let very_long = "a".repeat(50000);
        let truncated_very_long = HelperMethods::truncate_tool_result(&very_long, default_max);
        assert!(
            truncated_very_long.contains("[truncated — 50000 total chars, showing first 30000]")
        );
        assert!(truncated_very_long.starts_with(&"a".repeat(30000)));

        // Test with custom limit
        let custom_truncated = HelperMethods::truncate_tool_result(&very_long, 100);
        assert!(custom_truncated.starts_with(&"a".repeat(100)));
        assert!(custom_truncated.contains("[truncated — 50000 total chars, showing first 100]"));
    }

    #[test]
    fn test_thinking_level_ranges() {
        use crate::core::models::thinking_level_for_budget;

        // Sentinel 0 = "adaptive" (S172 — model decides)
        assert_eq!(thinking_level_for_budget(0), "adaptive");

        // Low range: 1..=2048
        assert_eq!(thinking_level_for_budget(1), "low");
        assert_eq!(thinking_level_for_budget(1024), "low");
        assert_eq!(thinking_level_for_budget(2048), "low");

        // Medium range: 2049..=4096
        assert_eq!(thinking_level_for_budget(2049), "medium");
        assert_eq!(thinking_level_for_budget(3000), "medium");
        assert_eq!(thinking_level_for_budget(4096), "medium");

        // High range: 4097..=16384
        assert_eq!(thinking_level_for_budget(4097), "high");
        assert_eq!(thinking_level_for_budget(8192), "high");
        assert_eq!(thinking_level_for_budget(16384), "high");

        // XHigh range: _ (everything else)
        assert_eq!(thinking_level_for_budget(16385), "xhigh");
        assert_eq!(thinking_level_for_budget(32768), "xhigh");
        assert_eq!(thinking_level_for_budget(100000), "xhigh");
    }
}

#[cfg(test)]
mod effective_prompt_tests {
    use super::*;

    fn headless(model: &str, base: &str) -> Runtime {
        let mut runtime = Runtime::new_headless();
        runtime.set_model(model.to_string());
        runtime.set_system_prompt(base.to_string());
        runtime
    }

    #[tokio::test]
    async fn codex_foreground_with_subagent_tools_composes_doctrine() {
        let runtime = headless("openai-codex/gpt-5.6-sol", "BASE.");
        let prompt = runtime
            .effective_system_prompt()
            .await
            .expect("prompt must compose");
        assert!(prompt.starts_with("BASE."), "base must lead");
        assert!(prompt.contains("## Subagent supervision"));
        assert!(prompt.contains("NEVER end your turn"));
    }

    #[tokio::test]
    async fn worker_runtime_without_subagent_tools_stays_clean() {
        let mut runtime = headless("openai-codex/gpt-5.6-sol", "WORKER.");
        runtime.set_tools(ToolRegistry::without_subagent());
        assert_eq!(
            runtime.effective_system_prompt().await.as_deref(),
            Some("WORKER."),
            "workers have no subagent tools; doctrine would be unactionable noise"
        );
    }

    #[tokio::test]
    async fn non_codex_foreground_stays_clean() {
        let runtime = headless("xai-auth/grok-4.5-latest", "BASE.");
        assert_eq!(
            runtime.effective_system_prompt().await.as_deref(),
            Some("BASE.")
        );
    }

    #[tokio::test]
    async fn manifest_prompt_is_returned_verbatim() {
        let mut runtime = headless("openai-codex/gpt-5.6-sol", "IGNORED.");
        let module = agent_core::prompt::PromptModule::new(
            agent_core::prompt::PromptModuleId::parse("kernel.test").unwrap(),
            "1.0.0",
            agent_core::prompt::PromptModuleSource::User,
            0,
            agent_core::prompt::PromptSelectors::default(),
            agent_core::prompt::ModuleMutability::MutableGuidance,
            "KERNEL.",
        )
        .unwrap();
        let context = agent_core::prompt::SelectionContext::new(
            agent_core::prompt::QualifiedModelId::parse("openai-codex/gpt-5.6-sol").unwrap(),
            None,
        )
        .unwrap();
        let stack = agent_core::prompt::PromptStack::new(vec![module], context).unwrap();
        runtime.apply_prompt_stack(stack).unwrap();
        assert_eq!(
            runtime.effective_system_prompt().await.as_deref(),
            Some("KERNEL."),
            "manifest authors own the full stack; no builtin injection on top"
        );
    }

    #[tokio::test]
    async fn unresolvable_model_identity_fails_closed_to_base() {
        let runtime = headless("definitely-not-a-model", "BASE.");
        assert_eq!(
            runtime.effective_system_prompt().await.as_deref(),
            Some("BASE.")
        );
    }
}
