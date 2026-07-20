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
use tokio_util::sync::CancellationToken;

mod api;
mod api_sync;
mod auth;
#[cfg(test)]
mod body_golden;
pub mod budget;
pub(crate) mod cloud_invoke;
pub mod compaction;
pub mod context;
pub mod google_gemini;
pub mod google_vertex;
pub(crate) mod helpers;
pub mod memory_context;
pub mod openai;
pub mod relay;
mod request;
mod sse;
mod sse_types;
mod stream;
pub mod subagent;
pub mod telemetry;
pub mod trace;
pub(crate) mod transport;
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
    /// Named reasoning level — canonical for Codex and future providers.
    /// When set, overrides what `thinking_level()` returns. For legacy
    /// Anthropic models this is derived from `thinking_budget` at read time.
    named_level: Option<agent_core::reasoning::ReasoningLevel>,
    /// True when the user has explicitly chosen a reasoning level (via command
    /// or config). False for derived/default values applied by set_model or
    /// session restore. Controls whether set_model overwrites the level with
    /// the new model's default.
    explicit_reasoning: bool,
    /// Foreground by default; central subagent construction marks Worker so
    /// logical Ultra can never recursively activate proactive orchestration.
    codex_request_role: crate::runtime::openai::catalog::CodexRequestRole,
    /// User override for context window size (tokens). When set, takes
    /// precedence over the model's auto-detected window from
    /// `models::context_window_for_model`. Lets users cap context at e.g.
    /// 200k even on models that natively support 1M.
    context_window_override: Option<u64>,
    /// Model used for compaction. Falls back to claude-sonnet-4-6 if not set.
    compaction_model: Option<String>,
    /// Where compaction summarization runs (spec §9.4).
    compaction_mode: agent_core::compaction::CompactionMode,
    /// Content classes excluded from remote compaction disclosure.
    compaction_exclusions: Vec<agent_core::compaction::ContentClass>,
    /// Transport-construction seam (spec §9.4 / CP-12 M3): incremented at
    /// the SINGLE remote-summarization entry point before any preflight,
    /// auth, or HTTP request construction. Shared across clones so the
    /// local-only zero-network proof observes every path.
    remote_summarization_attempts: Arc<std::sync::atomic::AtomicU64>,
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
    /// Session-shared bounded observability writer (Task 11, spec §6.5).
    /// `Some` iff `telemetry_level` is Basic/Full. Cloned runtimes
    /// (subagents) share the same handle — one worker per session, never
    /// per request. Dropping the last handle detaches the worker (it drains
    /// and exits on its own), so `Drop` can never hang.
    telemetry_writer: Option<crate::runtime::telemetry::TelemetryWriter>,
    /// Trace context handed to every `ApiOptions` site. Enabled (writer
    /// sink) iff `telemetry_writer` is `Some`; otherwise the no-op sink.
    /// Shared across clones so all requests of the session carry the same
    /// session-scoped context IDs.
    trace_ctx: trace::TraceContext,
    /// Explicit one-shot trace controls (Task 12): `/trace next` arms
    /// exactly the next outgoing provider request, even when telemetry is
    /// Off, then auto-disarms. Shared across clones.
    trace_controls: std::sync::Arc<trace::TraceControls>,
    /// Session-scoped continuous-memory context state (task A5, spec §7.3):
    /// the single state machine every frontend's `/memory` command and the
    /// task-A4 `memory_context` tool observe. Shared across `Clone`s —
    /// clones are the same session, so all its streams see one truth — but
    /// NEVER inherited by subagents: every subagent spawn path constructs a
    /// brand-new `Runtime::new()` (then `apply_subagent_runtime_policy`,
    /// see `tools/subagent/mod.rs`), and every `Runtime` constructor
    /// initializes this slot to the Off/no-lease default via
    /// [`fresh_memory_context_state`]. Do not add any code path that copies
    /// memory-context state from a parent runtime into a freshly
    /// constructed one.
    memory_context_state: std::sync::Arc<std::sync::Mutex<memory_context::SessionMemoryState>>,
    /// Writer handle backing the most recent armed one-shot ephemeral
    /// trace context (telemetry Off + `/trace next`). Retained here — not
    /// only inside the request's cloned context — so the session exit
    /// epilogue (`shutdown_observability*`) can drain the armed record
    /// even if the process exits right after the request. Replaced on
    /// re-arm; never touched on the request path beyond one mutex store.
    /// Shared across clones.
    one_shot_trace_writer:
        std::sync::Arc<std::sync::Mutex<Option<crate::runtime::telemetry::TelemetryWriter>>>,
    /// Content-capture root, bound ONCE at construction (fix1 I2b): every
    /// sweep — startup, status, sync and async shutdown epilogues — and the
    /// one-shot content capture use THIS path, never a late ambient
    /// `SYNAPS_BASE_DIR` read, so post-construction env churn (parallel
    /// tests, profile switches) can never redirect capture I/O.
    capture_dir: std::path::PathBuf,
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
    /// Exact operator-trusted worker identities loaded from `favorite_models`.
    /// These seed the session policy and are replayed when a manifestless
    /// foreground model change replaces that policy snapshot.
    trusted_worker_models: Vec<agent_core::prompt::QualifiedModelId>,
    /// True when this stream should expose only its session projection rather
    /// than the legacy full tool schema. Opt-in and false by default so the
    /// flag-off request bytes stay unchanged (Task 18).
    progressive_tool_disclosure: bool,
    /// Current worker handle for bounded delegation-tree accounting. `None`
    /// for foreground roots.
    delegation_parent: Option<String>,
    /// Shared exact MCP lease manager (Task 19). Installed at engine boot
    /// when MCP exact mode is active; streams mint per-session capabilities
    /// and RAII guards from it.
    mcp_runtime: Option<std::sync::Arc<crate::mcp::McpRuntimeManager>>,
    /// Durable shared session-scope guard (Task 19 review): terminates this
    /// runtime session's MCP leases only when the LAST owner (runtime clone
    /// or in-flight stream) drops — never per provider turn.
    mcp_session_scope: Option<std::sync::Arc<crate::mcp::McpSessionEndGuard>>,
    /// Shared exact EXTENSION lease manager (Task 20). Installed at engine
    /// boot under progressive disclosure; streams mint per-session
    /// capabilities and hold the durable scope below.
    extension_runtime: Option<std::sync::Arc<crate::extensions::lease::ExtensionRuntimeManager>>,
    /// Durable shared session-scope guard for extension leases (Task 20):
    /// terminates this runtime session's extension leases only when the
    /// LAST owner (runtime clone or in-flight stream) drops.
    extension_session_scope:
        Option<std::sync::Arc<crate::extensions::lease::ExtensionSessionEndGuard>>,
    /// Per-turn budget (Task 23, spec §8.1). Resolved from role + typed
    /// config at boot; every stream turn is metered against it.
    turn_budget: crate::runtime::budget::TurnBudget,
    /// Private runtime-scoped tool-session identity (Task 16). Scopes the
    /// per-stream `SessionToolSet` the execution gate authorizes against.
    /// Minted fresh per constructed `Runtime` — two independently
    /// constructed runtimes can never share session grants — and shared by
    /// `Clone` because clones share the same live tool registry (the
    /// existing shared-session behavior). Never persisted; unrelated to
    /// saved session IDs.
    host_tool_session: crate::tools::activation::SessionId,
}

/// Mint a fresh runtime-scoped tool-session identity. Process id + UUIDv4
/// keeps it unique across runtimes and restarts; the parse cannot fail on
/// this generated shape.
fn fresh_host_tool_session() -> crate::tools::activation::SessionId {
    crate::tools::activation::SessionId::parse(&format!(
        "runtime-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
    .expect("generated runtime session id is always valid")
}

/// Fresh Off/no-lease memory-context state for a newly constructed runtime
/// (task A5). CRITICAL INVARIANT: every `Runtime` constructor calls this,
/// and subagent spawn paths build a brand-new `Runtime::new()` before
/// `apply_subagent_runtime_policy` runs — so subagents always start with no
/// memory lease. Memory-context state must never be copied from a parent
/// runtime into a freshly constructed one.
fn fresh_memory_context_state(
) -> std::sync::Arc<std::sync::Mutex<memory_context::SessionMemoryState>> {
    let session = memory_context::SessionId::parse(&format!(
        "memctx-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
    .expect("generated memory session id is always valid");
    std::sync::Arc::new(std::sync::Mutex::new(
        memory_context::SessionMemoryState::new(session),
    ))
}

/// Host-derived project isolation identity (spec §5.2) recorded on
/// command-granted memory leases: a bounded digest of the current working
/// directory. Exact project-root resolution arrives with provider
/// activation (task A6); the digest keeps the identity bounded and
/// control-character free for any path.
fn memory_project_id() -> memory_context::ProjectId {
    use sha2::{Digest as _, Sha256};
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unresolved".to_string());
    let digest = Sha256::digest(cwd.as_bytes());
    let mut id = String::from("project-cwd-");
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    memory_context::ProjectId::parse(&id).expect("generated project id is always valid")
}

/// The spec-canonical continuous-memory provider identity (spec §17.3,
/// Axel memory-manager). FALLBACK ONLY as of task A6: it is recorded on
/// command-granted leases exclusively when NO extension subsystem is wired
/// into the runtime (legacy/flag-off construction — see
/// [`Runtime::resolve_memory_provider`]). When the extension runtime is
/// installed, enable-time validation resolves the exact declared provider
/// from the loaded catalog instead, failing closed on zero or ambiguous
/// matches.
fn memory_provider_id() -> memory_context::ContextProviderId {
    memory_context::ContextProviderId::parse("axel-memory")
        .expect("static provider id is always valid")
}

/// Idle timeout for the runtime HTTP client: how long a request may go
/// without receiving *any* bytes (headers or body chunks) before it is
/// killed. Resets on every received chunk, so healthy long-running streams
/// are never interrupted.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(90);

/// Build the runtime's shared HTTP client.
///
/// Timeout design (incident: session 20260714-025948-3dab):
/// * `connect_timeout(10s)` — connection-establishment ceiling.
/// * `read_timeout` — **idle** detector; resets after each successful read
///   (reqwest `ReadTimeoutBody`). A hung request surfaces in seconds
///   instead of minutes, while an actively-streaming turn can run
///   indefinitely.
/// * Deliberately **no** total `.timeout(…)`: that was a wall-clock
///   deadline that kept ticking while bytes flowed, killing any healthy
///   stream longer than 300s and taking 300s to notice a dead connection.
///   Turn-level lifecycle is owned by cancellation tokens, not the client.
fn build_http_client(read_timeout: Duration) -> reqwest::Result<Client> {
    Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(read_timeout)
        .build()
}

/// Preserve compatibility with favorite IDs written before Anthropic used its
/// runtime-qualified provider name. Authorization always stores the canonical
/// exact identity; unrelated bare values remain invalid and are ignored.
fn canonical_trusted_worker_model(model: &str) -> String {
    let model = model.trim();
    if let Some(id) = model.strip_prefix("claude/") {
        format!("anthropic/{id}")
    } else if model.starts_with("claude-") {
        format!("anthropic/{model}")
    } else {
        model.to_owned()
    }
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        // Runtime construction is credential-blind. Credentials are acquired
        // lazily through the broker abstraction after configuration is applied;
        // this layer never opens auth.json or consults a secret environment var.
        let (auth_token, auth_type, refresh_token, token_expires) =
            (String::new(), "oauth".to_string(), None, Some(0));

        let client = build_http_client(HTTP_READ_TIMEOUT)
            .map_err(|e| RuntimeError::Config(format!("Failed to build HTTP client: {}", e)))?;

        // Operational retention (Task 12): physically remove expired
        // content-capture bundles at session startup — bounded, fail-soft,
        // confined to the private capture dir. The root resolved here is
        // the SAME value bound into `capture_dir` below (fix1 I2b).
        let capture_dir = trace::default_capture_dir();
        let _ = trace::sweep_expired_captures(&capture_dir);

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
            named_level: None,
            explicit_reasoning: false,
            codex_request_role: crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            context_window_override: None,
            compaction_model: None,
            compaction_mode: agent_core::compaction::CompactionMode::default(),
            compaction_exclusions: Vec::new(),
            remote_summarization_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
            telemetry_writer: None,
            trace_ctx: trace::TraceContext::disabled(),
            trace_controls: std::sync::Arc::new(trace::TraceControls::new()),
            // Off/no-lease default — subagents get a FRESH construction of
            // this state (task A5 invariant), never a copy of the parent's.
            memory_context_state: fresh_memory_context_state(),
            one_shot_trace_writer: std::sync::Arc::new(std::sync::Mutex::new(None)),
            capture_dir,
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
            trusted_worker_models: Vec::new(),
            progressive_tool_disclosure: false,
            delegation_parent: None,
            mcp_runtime: None,
            mcp_session_scope: None,
            extension_runtime: None,
            extension_session_scope: None,
            turn_budget: crate::runtime::budget::TurnBudget::for_role(
                crate::runtime::budget::TurnRole::Foreground,
            ),
            host_tool_session: fresh_host_tool_session(),
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
        let client = build_http_client(HTTP_READ_TIMEOUT)
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
            named_level: None,
            explicit_reasoning: false,
            codex_request_role: crate::runtime::openai::catalog::CodexRequestRole::Foreground,
            context_window_override: None,
            compaction_model: None,
            compaction_mode: agent_core::compaction::CompactionMode::default(),
            compaction_exclusions: Vec::new(),
            remote_summarization_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
            telemetry_writer: None,
            trace_ctx: trace::TraceContext::disabled(),
            trace_controls: std::sync::Arc::new(trace::TraceControls::new()),
            // Off/no-lease default — subagents get a FRESH construction of
            // this state (task A5 invariant), never a copy of the parent's.
            memory_context_state: fresh_memory_context_state(),
            one_shot_trace_writer: std::sync::Arc::new(std::sync::Mutex::new(None)),
            capture_dir: trace::default_capture_dir(),
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
            trusted_worker_models: Vec::new(),
            progressive_tool_disclosure: false,
            delegation_parent: None,
            mcp_runtime: None,
            mcp_session_scope: None,
            extension_runtime: None,
            extension_session_scope: None,
            turn_budget: crate::runtime::budget::TurnBudget::for_role(
                crate::runtime::budget::TurnRole::Foreground,
            ),
            host_tool_session: fresh_host_tool_session(),
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
        let workflow_mode = match self.reasoning_level() {
            agent_core::reasoning::ReasoningLevel::UltraCode => {
                Some(agent_core::prompt::WorkflowMode::UltraCode)
            }
            agent_core::reasoning::ReasoningLevel::Max => {
                Some(agent_core::prompt::WorkflowMode::Max)
            }
            agent_core::reasoning::ReasoningLevel::XHigh => {
                Some(agent_core::prompt::WorkflowMode::XHigh)
            }
            _ => None,
        };
        let Ok(context) = agent_core::prompt::SelectionContext::new(model, None)
            .map(|context| context.with_workflow_mode(workflow_mode))
        else {
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

    /// Install the shared exact MCP lease manager (Task 19, engine boot).
    /// Also mints ONE durable session-scope guard for the runtime's tool
    /// session, shared by every clone and stream; only the LAST owner's
    /// drop terminates leases.
    pub fn install_mcp_runtime(&mut self, manager: std::sync::Arc<crate::mcp::McpRuntimeManager>) {
        self.mcp_session_scope = Some(std::sync::Arc::new(crate::mcp::McpSessionEndGuard::new(
            self.host_tool_session.clone(),
            std::sync::Arc::clone(&manager),
        )));
        self.mcp_runtime = Some(manager);
    }

    /// Set this runtime's per-turn budget (Task 23). The engine resolves
    /// role + typed config at boot; subagent/watcher constructions pass
    /// their role's budget explicitly.
    pub fn set_turn_budget(&mut self, budget: crate::runtime::budget::TurnBudget) {
        self.turn_budget = budget;
    }

    /// The currently configured per-turn budget.
    pub fn turn_budget(&self) -> &crate::runtime::budget::TurnBudget {
        &self.turn_budget
    }

    /// Install the shared exact EXTENSION lease manager (Task 20, engine
    /// boot). Also mints ONE durable session-scope guard for the runtime's
    /// tool session, shared by every clone and stream; only the LAST
    /// owner's drop terminates leases.
    pub fn install_extension_runtime(
        &mut self,
        manager: std::sync::Arc<crate::extensions::lease::ExtensionRuntimeManager>,
    ) {
        // Bind the handler host scope to THIS runtime's durable tool
        // session: a Mixed extension's tool leases and hook/provider/user
        // handler leases share one key — ONE shared child per plugin.
        manager.bind_host_scope(self.host_tool_session.clone());
        self.extension_session_scope = Some(std::sync::Arc::new(
            crate::extensions::lease::ExtensionSessionEndGuard::new(
                self.host_tool_session.clone(),
                std::sync::Arc::clone(&manager),
            ),
        ));
        self.extension_runtime = Some(manager);
    }

    pub fn install_orchestration(
        &mut self,
        runtime: Arc<crate::orchestration::OrchestrationRuntime>,
    ) {
        for model in &self.trusted_worker_models {
            if let Err(error) = runtime.grant_worker_model(model.as_str()) {
                tracing::warn!(
                    model = model.as_str(),
                    error = %error,
                    "failed to apply configured worker-model trust"
                );
            }
        }
        self.orchestration = Some(runtime);
    }

    /// A runtime spawned for a worker shares only this session's bounded
    /// dispatch/tree authority. It must not replay process-global favorite
    /// models: doing so would turn a child construction into a fresh grant
    /// source. The child can dispatch only identities already authorized by
    /// this installed session policy.
    pub fn install_worker_orchestration(
        &mut self,
        runtime: Arc<crate::orchestration::OrchestrationRuntime>,
    ) {
        self.orchestration = Some(runtime);
    }

    pub fn set_delegation_parent(&mut self, parent: Option<String>) {
        self.delegation_parent = parent;
    }

    pub fn orchestration(&self) -> Option<&Arc<crate::orchestration::OrchestrationRuntime>> {
        self.orchestration.as_ref()
    }

    /// Extends the live session delegation policy with one explicitly
    /// user-trusted worker model (e.g. favorited mid-session in the models
    /// picker). Mid-session trust grants were always meant to be honored;
    /// the policy snapshot is not pinned against user decisions.
    pub fn grant_worker_model(&self, model: &str) -> std::result::Result<(), String> {
        self.orchestration
            .as_ref()
            .ok_or_else(|| "delegation policy unavailable".to_string())?
            .grant_worker_model(model)
    }

    pub fn set_model(&mut self, model: String) {
        let _ = self.try_set_model(model);
    }

    /// Apply a model change while preserving orchestration lifecycle invariants.
    /// Returns an error instead of mutating either model or policy when active
    /// workers still require collection/reconciliation or when the replacement
    /// foreground cannot produce a trusted manifestless policy snapshot.
    pub fn try_set_model(&mut self, model: String) -> std::result::Result<(), String> {
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
        // Manifestless orchestration snapshots are tied to the exact foreground
        // identity. Refuse the model mutation while a policy worker remains
        // unreconciled; replacing its registry would erase the completion-gate
        // remediation path, while changing only the runtime model would create a
        // stale authorization snapshot.
        if self.prompt_reload_source.is_none()
            && self
                .orchestration
                .as_ref()
                .is_some_and(|current| !current.unreconciled_runtime_handles().is_empty())
        {
            return Err("model change blocked: workers require collection/reconciliation".into());
        }
        let orchestration_replacement = if self.prompt_reload_source.is_none()
            && self.orchestration.is_some()
        {
            let foreground = crate::orchestration::canonical_foreground_identity(cleaned)
                .map_err(|_| "model change blocked: unresolved foreground model".to_string())?;
            let replacement = crate::orchestration::OrchestrationRuntime::baseline(
                foreground, 8, 64,
            )
            .map_err(|_| "model change blocked: trusted worker catalog unavailable".to_string())?;
            for trusted in &self.trusted_worker_models {
                replacement
                    .grant_worker_model(trusted.as_str())
                    .map_err(|_| {
                        "model change blocked: configured worker trust invalid".to_string()
                    })?;
            }
            Some(Arc::new(replacement))
        } else {
            None
        };
        self.model = cleaned.to_owned();
        // Replace the manifestless policy atomically after a safe model change so
        // inheritance and exact same-provider choices follow the new foreground.
        // Typed manifest policies are immutable and are not rewritten here.
        if let Some(replacement) = orchestration_replacement {
            self.orchestration = Some(replacement);
        }
        // Apply the new model's default reasoning level from exact capability
        // metadata (Codex catalog default, xAI documented default/Adaptive)
        // unless the user has explicitly chosen a level (explicit_reasoning is
        // true — set via command, config, or explicit session restore).
        // Derived/default levels are overwritten by the new model's default.
        if !self.explicit_reasoning {
            if let Some(level) =
                crate::runtime::openai::catalog::validation::default_level_for_model(cleaned)
            {
                self.set_reasoning_level(level);
            }
        }
        Ok(())
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

    /// Runtime-scoped tool-session identity used by the stream execution
    /// gate (Task 16). Shared by clones (which share the tool registry);
    /// fresh per independently constructed `Runtime`.
    pub fn host_tool_session_id(&self) -> &crate::tools::activation::SessionId {
        &self.host_tool_session
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
        // Sync named_level from budget so the two fields stay consistent.
        self.named_level = Some(agent_core::reasoning::ReasoningLevel::from_legacy_budget(
            budget,
        ));
        // Config/restore path — not an explicit user choice.
        self.explicit_reasoning = false;
    }

    /// Set the named reasoning level (config/restore path). Updates
    /// `thinking_budget` from the level's canonical budget when one exists;
    /// for Max/Ultra (no numeric budget), leaves `thinking_budget` unchanged.
    /// Marks `explicit_reasoning = false` — use `set_reasoning_level_explicit`
    /// for user commands and settings.
    pub fn set_reasoning_level(&mut self, level: agent_core::reasoning::ReasoningLevel) {
        self.named_level = Some(level);
        if let Some(budget) = level.to_legacy_budget() {
            self.thinking_budget = budget;
        }
        // Max/Ultra: do NOT overwrite thinking_budget with u32::MAX.
        self.explicit_reasoning = false;
    }

    /// Set the named reasoning level as an **explicit user choice** (slash
    /// commands, settings panel). Identical to `set_reasoning_level` but sets
    /// `explicit_reasoning = true` so that a subsequent `set_model` call does
    /// not overwrite the level with the new model's default.
    pub fn set_reasoning_level_explicit(&mut self, level: agent_core::reasoning::ReasoningLevel) {
        self.set_reasoning_level(level);
        self.explicit_reasoning = true;
    }

    /// Set a custom numeric thinking budget as an **explicit user choice**
    /// (e.g. `/thinking 8192`). Retains the exact budget in `thinking_budget`
    /// while syncing `named_level` to the nearest named level for display.
    /// Sets `explicit_reasoning = true`.
    pub fn set_thinking_budget_explicit(&mut self, budget: u32) {
        self.thinking_budget = budget;
        self.named_level = Some(agent_core::reasoning::ReasoningLevel::from_legacy_budget(
            budget,
        ));
        self.explicit_reasoning = true;
    }

    /// Expose the `explicit_reasoning` provenance flag (test/introspection).
    pub fn is_reasoning_explicit(&self) -> bool {
        self.explicit_reasoning
    }

    /// Raw thinking budget value (for testing and legacy request building).
    pub fn thinking_budget_raw(&self) -> u32 {
        self.thinking_budget
    }

    /// Validate `level` against the current model's capability metadata via
    /// the shared per-provider validator (cache then exact static tables),
    /// then apply it. Returns `Err(user-facing message)` and leaves runtime
    /// state unchanged if the level is unsupported.
    pub fn set_reasoning_level_checked(
        &mut self,
        level: agent_core::reasoning::ReasoningLevel,
    ) -> std::result::Result<(), String> {
        crate::runtime::openai::catalog::validation::validate_reasoning_mutation(
            &self.model,
            level,
        )?;
        if self.model.starts_with("anthropic/")
            && level == agent_core::reasoning::ReasoningLevel::UltraCode
        {
            if self.codex_request_role()
                != crate::runtime::openai::catalog::ExecutionRole::Foreground
            {
                return Err("ultracode_requires_foreground".into());
            }
            if self.effective_prompt.is_some() {
                return Err("typed_prompt_manifest_blocks_required_doctrine".into());
            }
            if self.orchestration.is_none() {
                return Err("ultracode_requires_orchestration".into());
            }
            let tools = self.tools.try_read().map_err(|_| {
                "ultracode prerequisite state is busy; refusing mutation".to_string()
            })?;
            let required = ["subagent_start", "subagent_status", "subagent_collect"];
            if !required.iter().all(|name| {
                tools
                    .get(name)
                    .is_some_and(|tool| tool.extension_id().is_none())
            }) {
                return Err("ultracode_requires_lifecycle_tools".into());
            }
        }
        self.set_reasoning_level_explicit(level);
        Ok(())
    }

    /// Validate then apply a custom numeric thinking budget (`/thinking <N>`).
    /// The budget's derived `ReasoningLevel` runs through the same exact-model
    /// mutation validator as named levels (Anthropic fixed-budget models pass:
    /// derived levels never exceed XHigh and thinking-capable descriptors
    /// accept them). On `Ok` the exact budget is retained — never downgraded
    /// to a named-level canonical budget. On `Err` state is unchanged.
    pub fn set_thinking_budget_checked(&mut self, budget: u32) -> std::result::Result<(), String> {
        let level = agent_core::reasoning::ReasoningLevel::from_legacy_budget(budget);
        crate::runtime::openai::catalog::validation::validate_reasoning_mutation(
            &self.model,
            level,
        )?;
        self.set_thinking_budget_explicit(budget);
        Ok(())
    }

    pub fn reasoning_level(&self) -> agent_core::reasoning::ReasoningLevel {
        self.named_level.unwrap_or_else(|| {
            agent_core::reasoning::ReasoningLevel::from_legacy_budget(self.thinking_budget)
        })
    }

    pub(crate) fn set_codex_request_role(
        &mut self,
        role: crate::runtime::openai::catalog::CodexRequestRole,
    ) {
        self.codex_request_role = role;
    }

    pub(crate) fn codex_request_role(&self) -> crate::runtime::openai::catalog::CodexRequestRole {
        self.codex_request_role
    }

    async fn authorized_anthropic_plan(
        &self,
    ) -> Result<Option<crate::runtime::openai::catalog::AnthropicExecutionPlan>> {
        if !self.model.starts_with("anthropic/") {
            return Ok(None);
        }
        let tools = self.tools.read().await;
        let builtin = |name: &str| {
            tools
                .get(name)
                .is_some_and(|tool| tool.extension_id().is_none())
        };
        let readiness = self
            .orchestration
            .as_ref()
            .and_then(|runtime| runtime.ultracode_readiness(&self.model).ok());
        let prerequisites = crate::runtime::openai::catalog::AnthropicPlanPrerequisites {
            orchestration_policy: self.orchestration.is_some(),
            foreground_worker_authorized: readiness.is_some(),
            concurrent_limit: readiness.map_or(0, |limits| limits.0),
            total_limit: readiness.map_or(0, |limits| limits.1),
            lifecycle_start: builtin("subagent_start"),
            lifecycle_status: builtin("subagent_status"),
            lifecycle_steer: builtin("subagent_steer"),
            lifecycle_collect: builtin("subagent_collect"),
            lifecycle_resume: builtin("subagent_resume"),
        };
        crate::runtime::openai::catalog::plan_anthropic_execution(
            &self.model,
            self.reasoning_level(),
            self.codex_request_role(),
            prerequisites,
            crate::runtime::openai::catalog::capability_cache::get(&self.model).and_then(|entry| {
                match entry.reasoning {
                    crate::runtime::openai::catalog::ReasoningSupport::AnthropicAdaptive {
                        adaptive,
                    } => Some(adaptive),
                    crate::runtime::openai::catalog::ReasoningSupport::None => Some(false),
                    _ => None,
                }
            }),
        )
        .map(Some)
        .map_err(|error| RuntimeError::Config(error.to_string()))
    }

    /// Validate exact-model reasoning and Ultra orchestration prerequisites
    /// before refresh, broker access, or provider network work.
    async fn validate_request_preflight(&self) -> Result<()> {
        self.validate_request_preflight_for(&self.model, self.codex_request_role)
            .await
    }

    async fn validate_request_preflight_for(
        &self,
        model: &str,
        role: crate::runtime::openai::catalog::CodexRequestRole,
    ) -> Result<()> {
        let level = self.reasoning_level();
        if model.starts_with("anthropic/")
            && level == agent_core::reasoning::ReasoningLevel::UltraCode
            && self.effective_prompt.is_some()
        {
            return Err(RuntimeError::Config(
                "Anthropic execution plan rejected: typed_prompt_manifest_blocks_required_doctrine"
                    .into(),
            ));
        }
        if model.starts_with("anthropic/")
            && matches!(
                level,
                agent_core::reasoning::ReasoningLevel::Max
                    | agent_core::reasoning::ReasoningLevel::UltraCode
                    | agent_core::reasoning::ReasoningLevel::Ultra
            )
        {
            let tools = self.tools.read().await;
            let builtin = |name: &str| {
                tools
                    .get(name)
                    .is_some_and(|tool| tool.extension_id().is_none())
            };
            let readiness = self
                .orchestration
                .as_ref()
                .and_then(|runtime| runtime.ultracode_readiness(model).ok());
            let prerequisites = crate::runtime::openai::catalog::AnthropicPlanPrerequisites {
                orchestration_policy: self.orchestration.is_some(),
                foreground_worker_authorized: readiness.is_some(),
                concurrent_limit: readiness.map_or(0, |limits| limits.0),
                total_limit: readiness.map_or(0, |limits| limits.1),
                lifecycle_start: builtin("subagent_start"),
                lifecycle_status: builtin("subagent_status"),
                lifecycle_steer: builtin("subagent_steer"),
                lifecycle_collect: builtin("subagent_collect"),
                lifecycle_resume: builtin("subagent_resume"),
            };
            drop(tools);
            let result = crate::runtime::openai::catalog::plan_anthropic_execution(
                model,
                level,
                role,
                prerequisites,
                crate::runtime::openai::catalog::capability_cache::get(model).and_then(|entry| {
                    match entry.reasoning {
                        crate::runtime::openai::catalog::ReasoningSupport::AnthropicAdaptive {
                            adaptive,
                        } => Some(adaptive),
                        crate::runtime::openai::catalog::ReasoningSupport::None => Some(false),
                        _ => None,
                    }
                }),
            );
            match result {
                Ok(plan) => {
                    tracing::debug!(event = "anthropic_mode_plan", qualified_model = %model, requested_level = %level, runtime_role = role.as_str(), execution_mode = ?plan.mode, wire_effort = plan.wire_effort.map_or("omitted", |effort| effort.as_str()), workflow = ?plan.workflow, credentials_attempted = false, network_attempted = false);
                    return Ok(());
                }
                Err(error) => {
                    tracing::debug!(event = "anthropic_mode_plan", qualified_model = %model, requested_level = %level, runtime_role = role.as_str(), decision = "deny", deny_code = error.code().as_str(), credentials_attempted = false, network_attempted = false);
                    return Err(RuntimeError::Config(error.to_string()));
                }
            }
        }
        if model.starts_with("openai-codex/") {
            let plan =
                crate::runtime::openai::catalog::plan_codex_execution(model, level, role, None)
                    .map_err(|error| {
                        tracing::debug!(
                            event = "codex_mode_plan",
                            qualified_model = %model,
                            requested_level = %level,
                            runtime_role = role.as_str(),
                            decision = "deny",
                            deny_code = error.code().as_str(),
                            network_attempted = false,
                            "Codex request preflight denied"
                        );
                        RuntimeError::Config(error.to_string())
                    })?;

            if plan.automatic_delegation() {
                if self.orchestration.is_none() {
                    tracing::debug!(
                        event = "codex_mode_plan",
                        qualified_model = %model,
                        requested_level = %level,
                        runtime_role = role.as_str(),
                        decision = "deny",
                        deny_code = "ultra_requires_orchestration",
                        network_attempted = false,
                        "Codex request preflight denied"
                    );
                    return Err(RuntimeError::Config(
                        "Ultra requires an installed orchestration policy".to_string(),
                    ));
                }

                let tools = self.tools.read().await;
                let required = ["subagent_start", "subagent_status", "subagent_collect"];
                let tools_ready = required.iter().all(|name| {
                    tools
                        .get(name)
                        .is_some_and(|tool| tool.extension_id().is_none())
                });
                if !tools_ready {
                    tracing::debug!(
                        event = "codex_mode_plan",
                        qualified_model = %model,
                        requested_level = %level,
                        runtime_role = role.as_str(),
                        decision = "deny",
                        deny_code = "ultra_requires_subagent_tools",
                        network_attempted = false,
                        "Codex request preflight denied"
                    );
                    return Err(RuntimeError::Config(
                        "Ultra requires built-in subagent_start, subagent_status, and subagent_collect tools"
                            .to_string(),
                    ));
                }
            }
            return Ok(());
        }

        crate::runtime::openai::catalog::validation::validate_reasoning_mutation(model, level)
            .map_err(|error| {
                tracing::debug!(
                    event = "codex_mode_plan",
                    qualified_model = %model,
                    requested_level = %level,
                    runtime_role = role.as_str(),
                    decision = "deny",
                    deny_code = "unsupported_reasoning_level",
                    network_attempted = false,
                    "request preflight denied"
                );
                RuntimeError::Config(error)
            })
    }

    pub fn set_compaction_model(&mut self, model: Option<String>) {
        self.compaction_model = model;
    }

    /// Set where compaction summarization runs (spec §9.4).
    pub fn set_compaction_mode(&mut self, mode: agent_core::compaction::CompactionMode) {
        self.compaction_mode = mode;
    }

    /// Set the content classes withheld from remote compaction disclosure.
    pub fn set_compaction_exclusions(
        &mut self,
        exclude: Vec<agent_core::compaction::ContentClass>,
    ) {
        self.compaction_exclusions = exclude;
    }

    /// Number of times the remote-summarization transport seam was entered
    /// (CP-12 M3). Local-only compaction must leave this at zero.
    pub fn remote_summarization_attempts(&self) -> u64 {
        self.remote_summarization_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The session's compaction disclosure policy (spec §9.4) — consumed by
    /// `runtime::compaction` for both the preview and the dispatch path.
    pub fn compaction_policy(&self) -> crate::runtime::compaction::DisclosurePolicy {
        crate::runtime::compaction::DisclosurePolicy {
            mode: self.compaction_mode,
            exclude: self.compaction_exclusions.clone(),
        }
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

    /// Task 29 (spec §9.1): the ONE request-aware context assessment every
    /// frontend consumes on its compaction trigger path. Reads the segments
    /// the next request will actually carry — the effective system prompt,
    /// the exposed tool-schema set, the supplied history — and the runtime's
    /// configured reserves (thinking budget, tool-result cap, model output
    /// reserve, provider window with the documented safety margin).
    ///
    /// Skill and memory bodies that reach the request do so through the
    /// system prompt or history today, so they are already accounted there;
    /// the separate breakdown lanes are fed once loaders hand the engine
    /// distinct segments.
    pub async fn assess_context(
        &self,
        messages: &[crate::SharedMessage],
    ) -> context::ContextAssessment {
        let system = self.effective_system_prompt().await;
        let schema = self.tools.read().await.tools_schema();
        context::assess(&context::ContextBudgetInputs {
            model: &self.model,
            provider_window: self.context_window(),
            system_prompt: system.as_deref(),
            tools_schema: &schema,
            messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: self.thinking_budget as u64,
            next_tool_result_bytes: self.max_tool_output as u64,
            output_reserve_tokens: HelperMethods::max_tokens_for_model(&self.model),
        })
    }

    /// Apply a parsed config file to this runtime (model, thinking budget, etc.)
    pub fn apply_config(&mut self, config: &crate::config::SynapsConfig) {
        if let Some(ref model) = config.model {
            self.set_model(model.clone());
        }
        // Named level takes priority over raw budget.
        // Config-specified thinking is an explicit user choice — preserve across model switches.
        if let Some(level) = config.thinking_level {
            self.set_reasoning_level_explicit(level);
        } else if let Some(budget) = config.thinking_budget {
            self.set_thinking_budget_explicit(budget);
        }
        self.context_window_override = config.context_window;
        self.compaction_model = config.compaction_model.clone();
        self.compaction_mode = config.compaction_mode;
        self.compaction_exclusions = config.compaction_exclude.clone();
        self.max_tool_output = config.max_tool_output;
        self.bash_timeout = config.bash_timeout;
        self.bash_max_timeout = config.bash_max_timeout;
        self.subagent_timeout = config.subagent_timeout;
        self.api_retries = config.api_retries;
        self.refusal_retries = config.refusal_retries;
        self.telemetry_level =
            crate::runtime::telemetry::TelemetryLevel::from_str_key(&config.telemetry);
        self.sync_observability();
        self.cache_diagnostics = config.cache_diagnostics;
        self.cache_ttl = config.cache_ttl;
        self.progressive_tool_disclosure = config.progressive_tool_disclosure;
        self.trusted_worker_models = config
            .favorite_models
            .iter()
            .filter_map(|model| {
                let canonical = canonical_trusted_worker_model(model);
                match agent_core::prompt::QualifiedModelId::parse(canonical) {
                    Ok(model) => Some(model),
                    Err(_) => {
                        tracing::warn!(
                            model = model.as_str(),
                            "ignoring invalid favorite worker-model identity"
                        );
                        None
                    }
                }
            })
            .collect();
        self.trusted_worker_models.sort();
        self.trusted_worker_models.dedup();
        if let Some(orchestration) = &self.orchestration {
            for model in &self.trusted_worker_models {
                if let Err(error) = orchestration.grant_worker_model(model.as_str()) {
                    tracing::warn!(
                        model = model.as_str(),
                        error = %error,
                        "failed to apply configured worker-model trust"
                    );
                }
            }
        }
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
        self.sync_observability();
    }

    /// Reconcile the shared observability writer + trace sink with the
    /// telemetry level (Task 11 config rule, documented until Task 12 adds
    /// explicit trace config/UI): `basic`/`full` enables BOTH legacy
    /// telemetry persistence and metadata-only trace persistence through
    /// one bounded session writer; `off` disables both. The trace schema is
    /// structurally metadata-only, so this never persists raw content.
    fn sync_observability(&mut self) {
        if self.telemetry_level.enabled() {
            if self.telemetry_writer.is_none() {
                let writer = crate::runtime::telemetry::TelemetryWriter::new(
                    crate::runtime::telemetry::WriterOptions::default(),
                );
                self.trace_ctx = trace::TraceContext::with_sink(Arc::new(
                    crate::runtime::telemetry::WriterTraceSink::new(writer.clone()),
                ));
                self.telemetry_writer = Some(writer);
            }
        } else {
            // Dropping this handle detaches the worker: it drains whatever
            // is queued and exits on its own — never a hang. In-flight
            // requests holding a cloned handle keep enqueueing harmlessly.
            self.telemetry_writer = None;
            self.trace_ctx = trace::TraceContext::disabled();
        }
    }

    /// The session trace context handed to every request (cheap clone).
    pub fn trace_context(&self) -> trace::TraceContext {
        self.trace_ctx.clone()
    }

    /// Effective trace context for the next outgoing model request:
    /// consumes a pending one-shot arm (`/trace next [content]`, Task 12).
    ///
    /// One-shot semantics (B1): the arm covers exactly one *logical
    /// request* — the returned armed context carries a one-shot request
    /// gate consumed inside `RequestTracer::begin`, so the first request
    /// through it (all retry attempts included) emits records and every
    /// subsequent request sharing the same `ApiOptions` (tool-loop
    /// continuations) is disabled. Normal Basic/Full session contexts are
    /// never gated.
    ///
    /// When the session sink is disabled the one-shot record rides an
    /// ephemeral writer forked from the session context (same session ID,
    /// digest key, degradation counters, and §6.6 cache snapshot store —
    /// so `/context` sees the armed request's diagnostics). The writer
    /// handle is retained in `one_shot_trace_writer` until replaced or
    /// drained by the shutdown epilogue, guaranteeing the armed record
    /// flushes on session exit. With the session sink already enabled
    /// (telemetry Basic/Full) the arm only adds the optional content
    /// capture.
    pub(crate) fn effective_trace_context(&self) -> trace::TraceContext {
        match self.trace_controls.consume() {
            None => self.trace_ctx.clone(),
            Some(with_content) => {
                let base = if self.trace_ctx.enabled() {
                    self.trace_ctx.clone()
                } else {
                    let writer = crate::runtime::telemetry::TelemetryWriter::new(
                        crate::runtime::telemetry::WriterOptions::default(),
                    );
                    // Retain the writer for the exit epilogue (drops any
                    // previously retained one-shot writer, whose worker
                    // keeps draining in the background).
                    *self
                        .one_shot_trace_writer
                        .lock()
                        .expect("one-shot trace writer lock poisoned") = Some(writer.clone());
                    self.trace_ctx
                        .fork_with_sink(Arc::new(crate::runtime::telemetry::WriterTraceSink::new(
                            writer,
                        )))
                        .with_one_shot_request_gate()
                };
                if with_content {
                    base.with_content_capture(Arc::new(trace::ContentCapture::new(
                        self.capture_dir.clone(),
                    )))
                } else {
                    base
                }
            }
        }
    }

    /// Arm tracing for exactly the next outgoing provider request
    /// (`/trace next`; `with_content` adds the one-request redacted
    /// content capture for `/trace next content`).
    pub fn trace_arm_next(&self, with_content: bool) {
        self.trace_controls.arm_next(with_content);
    }

    /// Metadata-only `/trace status` report: mode, persistence path,
    /// counters. Never secrets or content. Also performs the opportunistic
    /// expired-capture sweep (B2): status is a trace interaction, so stale
    /// content-capture bundles are physically removed here too.
    pub fn trace_status(&self) -> trace::TraceStatusReport {
        let _ = trace::sweep_expired_captures(&self.capture_dir);
        trace::TraceStatusReport {
            persistent_enabled: self.telemetry_writer.is_some(),
            arm: self.trace_controls.peek(),
            trace_path: crate::runtime::telemetry::default_trace_log_path(),
            writer_stats: self.telemetry_writer.as_ref().map(|w| w.stats()),
            degraded_records: self.trace_ctx.degraded_records(),
        }
    }

    /// Lock the session-scoped memory-context state (task A5). Poison
    /// recovery is sound for the same reason as
    /// `memory_context::MemoryContextCapability::lock`: every
    /// `SessionMemoryState` transition is check-then-single-assignment with
    /// no panicking code between, so a poisoned lock still holds a
    /// consistent state.
    fn memory_context_lock(&self) -> std::sync::MutexGuard<'_, memory_context::SessionMemoryState> {
        self.memory_context_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Typed `/memory status` snapshot — metadata-only, never spawns a
    /// provider process (spec §7.2 "status does not spawn").
    pub fn memory_context_status(&self) -> memory_context::MemoryContextStatus {
        self.memory_context_lock().status()
    }

    /// Revoke this session's memory context (`/memory off`). Always locally
    /// allowed (spec §7.2); idempotent — disabling an already-off session
    /// reports `Off`.
    ///
    /// Task A6 revocation wiring: after the session state transition, the
    /// extension runtime lease backing each bound provider is defensively
    /// revoked through the SAME [`ExtensionRuntimeManager`] mechanism the
    /// deferred tool/handler lifecycle uses ([`Self::install_extension_runtime`]
    /// / `ExtensionSessionEndGuard`). While Phase A never spawns, this is an
    /// idempotent no-op; once Phase B routes real recall/capture calls under
    /// this runtime's tool session, disable reaps the provider child exactly.
    ///
    /// [`ExtensionRuntimeManager`]: crate::extensions::lease::ExtensionRuntimeManager
    pub fn memory_context_disable(&self) -> memory_context::MemoryContextStatus {
        let (status, bound) = {
            let mut state = self.memory_context_lock();
            let session = state.session_id().clone();
            let bound = state.bound_provider_ids();
            let status = match memory_context::apply_memory_context_action(
                &mut state,
                memory_context::AuthorizedMemoryAction::Disable { session },
            ) {
                Ok(status) => status,
                // Unreachable by construction (the session identity is read
                // from the state itself and `Disable` is total), but never
                // panic on the fallback: report current status.
                Err(_) => state.status(),
            };
            (status, bound)
        };
        // State mutex released before touching the lease manager: the
        // revocation below locks only the lease map (idempotent no-op when
        // nothing was ever spawned).
        self.revoke_memory_provider_leases(&bound);
        status
    }

    /// Task A6: defensively revoke the extension runtime lease backing each
    /// bound memory provider for THIS runtime's tool session. Confirmed
    /// idempotent — `ExtensionRuntimeManager::revoke_plugin_lease` is a map
    /// remove that finds nothing when no lease was ever acquired, so calling
    /// it before Phase B ever spawns is a safe no-op. Provider identities
    /// that are not composed `extension:<plugin>:<id>` runtime addresses
    /// (the legacy canonical fallback) name no plugin child and are skipped.
    fn revoke_memory_provider_leases(&self, bound: &[memory_context::ContextProviderId]) {
        let Some(extension_runtime) = &self.extension_runtime else {
            return;
        };
        for provider in bound {
            let mut parts = provider.as_str().splitn(3, ':');
            if let (Some("extension"), Some(plugin), Some(_local_id)) =
                (parts.next(), parts.next(), parts.next())
            {
                if !plugin.is_empty() {
                    extension_runtime.revoke_plugin_lease(&self.host_tool_session, plugin);
                }
            }
        }
    }

    /// Task A6 enable-time provider validation: resolve the provider a
    /// `/memory` grant binds to. With the extension runtime installed
    /// ([`Self::install_extension_runtime`] — the same wiring point that
    /// backs `extension_leases` capabilities), resolution is MANDATORY and
    /// fail-closed against the loaded context-provider catalog (pure
    /// catalog read, never spawns): exactly one matching declared provider
    /// resolves to its composed `extension:<plugin>:<id>` runtime address;
    /// zero matches fail [`memory_context::MemoryContextError::ProviderNotRegistered`]
    /// and overlapping declarations without an exact requested id fail
    /// [`memory_context::MemoryContextError::ProviderAmbiguous`] — the
    /// exact-scope precedent of `validate_deferred` and
    /// `crate::orchestration::validate_user_authorizable_model`.
    ///
    /// Without an extension subsystem (legacy/flag-off constructions and
    /// headless unit runtimes) the A5 spec-canonical identity is kept:
    /// there is no catalog to validate against, Phase A grants are inert
    /// metadata, and Phase B activation independently re-validates through
    /// `ExtensionLeaseCapability::call_exact` against retained launch
    /// records — an unbacked identity can never start a process.
    fn resolve_memory_provider(
        &self,
        requested: Option<&str>,
    ) -> std::result::Result<memory_context::ContextProviderId, memory_context::MemoryContextError>
    {
        use crate::extensions::context_provider as ext_cp;
        let Some(extension_runtime) = &self.extension_runtime else {
            return Ok(memory_provider_id());
        };
        let requested_id = match requested {
            None => None,
            // An id that cannot even parse as a declared provider identity
            // can never be registered: fail closed as not-registered.
            Some(raw) => Some(
                ext_cp::ContextProviderId::parse(raw)
                    .map_err(|_| memory_context::MemoryContextError::ProviderNotRegistered)?,
            ),
        };
        match extension_runtime.resolve_context_provider(requested_id.as_ref()) {
            Ok(descriptor) => {
                memory_context::ContextProviderId::parse(&descriptor.runtime_address())
            }
            Err(ext_cp::ContextProviderLookupError::NotRegistered) => {
                Err(memory_context::MemoryContextError::ProviderNotRegistered)
            }
            Err(ext_cp::ContextProviderLookupError::Ambiguous) => {
                Err(memory_context::MemoryContextError::ProviderAmbiguous)
            }
        }
    }

    /// Install a durable session-lease memory mode (`/memory on|recall|
    /// capture`, spec §7.3) under the caller-supplied host-owned intent
    /// proof (spec §6.3). Task A6: the target provider is resolved and
    /// validated FIRST ([`Self::resolve_memory_provider`], fail-closed) —
    /// then the lease is minted and the exhaustive
    /// [`memory_context::apply_memory_context_action`] transition applied.
    /// On any typed failure no lease is installed.
    pub(crate) fn memory_context_enable(
        &self,
        mode: memory_context::MemoryContextMode,
        proof: memory_context::UserIntentProof,
    ) -> std::result::Result<memory_context::MemoryContextStatus, memory_context::MemoryContextError>
    {
        self.memory_context_enable_resolved(mode, proof, None)
    }

    /// [`Self::memory_context_enable`] with an explicit requested provider
    /// id (exact declared id; `None` requires a uniquely-declared provider
    /// when the extension subsystem is installed). Crate-internal: the
    /// `/memory` frontend forms currently pass `None`.
    pub(crate) fn memory_context_enable_resolved(
        &self,
        mode: memory_context::MemoryContextMode,
        proof: memory_context::UserIntentProof,
        requested_provider: Option<&str>,
    ) -> std::result::Result<memory_context::MemoryContextStatus, memory_context::MemoryContextError>
    {
        let mut state = self.memory_context_lock();
        let lease = self.grant_command_memory_lease(&state, mode, proof, requested_provider)?;
        memory_context::apply_memory_context_action(
            &mut state,
            memory_context::AuthorizedMemoryAction::Enable { lease },
        )
    }

    /// Install a one-shot recall lease (`/memory once`, spec §7.3) under
    /// the caller-supplied host-owned intent proof. Provider-validated
    /// exactly like [`Self::memory_context_enable`] (task A6). Fails typed
    /// (e.g. a one-shot already pending) without installing anything.
    pub(crate) fn memory_context_recall_once(
        &self,
        proof: memory_context::UserIntentProof,
    ) -> std::result::Result<memory_context::MemoryContextStatus, memory_context::MemoryContextError>
    {
        let mut state = self.memory_context_lock();
        let lease = self.grant_command_memory_lease(
            &state,
            memory_context::MemoryContextMode::RecallOnce,
            proof,
            None,
        )?;
        memory_context::apply_memory_context_action(
            &mut state,
            memory_context::AuthorizedMemoryAction::RecallOnce { lease },
        )
    }

    /// Mint one host-owned lease for a `/memory` command grant: host-minted
    /// lease ID, the state's own session identity, host-derived project
    /// identity, and the task-A6 catalog-validated provider identity
    /// ([`Self::resolve_memory_provider`] — fail-closed; on a resolution
    /// error nothing is minted). Session leases carry no hard expiry (until
    /// revoked or session end); `/memory` has no expiry argument (spec §7.3).
    fn grant_command_memory_lease(
        &self,
        state: &memory_context::SessionMemoryState,
        mode: memory_context::MemoryContextMode,
        proof: memory_context::UserIntentProof,
        requested_provider: Option<&str>,
    ) -> std::result::Result<memory_context::MemoryContextLease, memory_context::MemoryContextError>
    {
        let provider_id = self.resolve_memory_provider(requested_provider)?;
        memory_context::MemoryContextLease::grant(
            memory_context::MemoryLeaseId::parse(&format!("memctx-cmd-{}", uuid::Uuid::new_v4()))?,
            state.session_id().clone(),
            memory_project_id(),
            provider_id,
            mode,
            memory_context::CapturePolicy::default(),
            memory_context::RecallPolicy::default(),
            proof,
            std::time::SystemTime::now(),
            None,
        )
    }

    /// Test-only: intent proof recorded on the current durable lease.
    #[cfg(test)]
    pub(crate) fn memory_durable_proof_for_test(
        &self,
    ) -> Option<memory_context::UserIntentProof> {
        self.memory_context_lock().durable_proof().cloned()
    }

    /// Test-only: intent proof recorded on the pending one-shot lease.
    #[cfg(test)]
    pub(crate) fn memory_one_shot_proof_for_test(
        &self,
    ) -> Option<memory_context::UserIntentProof> {
        self.memory_context_lock().one_shot_pending_proof().cloned()
    }

    /// Test-only: provider identities bound to the currently granted
    /// leases (durable + pending one-shot), in that order.
    #[cfg(test)]
    pub(crate) fn memory_bound_providers_for_test(
        &self,
    ) -> Vec<memory_context::ContextProviderId> {
        self.memory_context_lock().bound_provider_ids()
    }

    /// Structured `/context` report (Task 12): counts, byte lengths, cache
    /// change/reuse estimates, and writer counters — never content.
    ///
    /// `history` is the conversation owned by the calling surface (TUI /
    /// headless); when it is not provided the history lines honestly read
    /// `unavailable` rather than fabricating zeros. Loaded skills/memories
    /// are session-surface state the runtime cannot enumerate, so they are
    /// reported `unavailable` with that provenance.
    pub fn context_report(&self, history: Option<&[crate::SharedMessage]>) -> trace::ContextReport {
        use trace::ReportValue;
        let (history_messages, history_bytes) = match history {
            Some(msgs) => (
                ReportValue::Count(msgs.len() as u64),
                ReportValue::Count(
                    msgs.iter()
                        .map(|m| {
                            serde_json::to_vec(&**m)
                                .map(|v| v.len() as u64)
                                .unwrap_or(0)
                        })
                        .sum(),
                ),
            ),
            None => (ReportValue::Unavailable, ReportValue::Unavailable),
        };
        // Non-blocking view of the tool registry: if it is momentarily
        // write-locked, report honestly instead of blocking or guessing.
        let tool_count = match self.tools.try_read() {
            Ok(tools) => ReportValue::Count(tools.iter_tools_sorted().len() as u64),
            Err(_) => ReportValue::Unavailable,
        };
        trace::ContextReport {
            model: self.model.clone(),
            system_prompt_bytes: ReportValue::Count(
                self.system_prompt().map(|s| s.len() as u64).unwrap_or(0),
            ),
            tool_count,
            history_messages,
            history_bytes,
            loaded_skills: ReportValue::Unavailable,
            loaded_memories: ReportValue::Unavailable,
            cache: self.trace_ctx.cache_snapshots().last_activity(),
            trace_enabled: self.trace_ctx.enabled(),
            writer_stats: self.telemetry_writer.as_ref().map(|w| w.stats()),
            degraded_records: self.trace_ctx.degraded_records(),
        }
    }

    /// The shared observability writer, when telemetry is enabled.
    pub fn telemetry_writer(&self) -> Option<crate::runtime::telemetry::TelemetryWriter> {
        self.telemetry_writer.clone()
    }

    /// Bounded shutdown flush of the observability writer: stop intake,
    /// drain until `timeout`, return typed stats (`None` when telemetry is
    /// off). Sync and bounded — safe from shutdown paths that cannot await;
    /// async callers should use [`Self::shutdown_observability_async`].
    ///
    /// Semantics (Task 11):
    /// - telemetry `off` → no writer exists → `None`, a true no-op;
    /// - "flushed" means every queued record was appended into OS file
    ///   buffers (`write(2)` returned) — deliberately no `fsync`, these are
    ///   best-effort diagnostic logs;
    /// - on timeout the worker stays detached and keeps draining in the
    ///   background; the caller logs metadata-only stats and continues —
    ///   trace loss must never fail or abort a clean exit.
    pub fn shutdown_observability(
        &self,
        timeout: std::time::Duration,
    ) -> Option<crate::runtime::telemetry::ShutdownOutcome> {
        // Operational retention (Task 12): the exit epilogue also removes
        // expired content-capture bundles — bounded, fail-soft, so a stale
        // bundle never has to wait for the next trace interaction. Swept at
        // the CONSTRUCTION-BOUND root (fix1 I2b), immune to env churn.
        let _ = trace::sweep_expired_captures(&self.capture_dir);
        // Drain a retained one-shot ephemeral writer first (Task 12 fix:
        // an armed `/trace next` record written with telemetry Off must
        // survive session exit). Its outcome is returned only when no
        // session writer exists — the session writer's stats remain the
        // primary signal.
        let ephemeral = self
            .one_shot_trace_writer
            .lock()
            .expect("one-shot trace writer lock poisoned")
            .take();
        let ephemeral_outcome = ephemeral.map(|w| w.shutdown(timeout));
        self.telemetry_writer
            .as_ref()
            .map(|w| w.shutdown(timeout))
            .or(ephemeral_outcome)
    }

    /// Async epilogue helper for every clean process/runtime exit path that
    /// owns a `Runtime` (headless chat, TUI teardown, autonomous agent,
    /// RPC/server graceful shutdown): clones the writer handle and runs the
    /// bounded [`Self::shutdown_observability`] on the blocking pool, so an
    /// executor thread is never parked. Same `None`/no-fsync/timeout
    /// semantics as the sync variant. Idempotent — a second call finds the
    /// intake already closed and returns immediately.
    ///
    /// Note: the writer is shared across `Runtime` clones (subagents). Call
    /// this only from the session owner's exit epilogue, after in-flight
    /// work has drained — cloned runtimes still serving requests would see
    /// their subsequent records counted as drops.
    pub async fn shutdown_observability_async(
        &self,
        timeout: std::time::Duration,
    ) -> Option<crate::runtime::telemetry::ShutdownOutcome> {
        // Same operational retention sweep as the sync epilogue, off the
        // executor (bounded filesystem work, fail-soft on a failed spawn).
        // Construction-bound root (fix1 I2b).
        let capture_dir = self.capture_dir.clone();
        let _ =
            tokio::task::spawn_blocking(move || trace::sweep_expired_captures(&capture_dir)).await;
        // Same one-shot ephemeral drain as the sync variant.
        let ephemeral = self
            .one_shot_trace_writer
            .lock()
            .expect("one-shot trace writer lock poisoned")
            .take();
        let ephemeral_outcome = match ephemeral {
            Some(writer) => Some(writer.shutdown_async(timeout).await),
            None => None,
        };
        match self.telemetry_writer.clone() {
            Some(writer) => Some(writer.shutdown_async(timeout).await),
            None => ephemeral_outcome,
        }
    }

    /// Test seam: install a custom writer (e.g. temp paths, artificial
    /// write delay) as the session observability sink, exactly as
    /// `sync_observability` would for a production writer.
    #[cfg(any(test, feature = "testing"))]
    pub fn install_observability_for_tests(
        &mut self,
        writer: crate::runtime::telemetry::TelemetryWriter,
    ) {
        self.trace_ctx = trace::TraceContext::with_sink(Arc::new(
            crate::runtime::telemetry::WriterTraceSink::new(writer.clone()),
        ));
        self.telemetry_writer = Some(writer);
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
        // For Max/Ultra the named_level wins; for legacy budget-only levels
        // we fall through to the legacy bucketing.
        match self.named_level {
            Some(level) => level.as_str(),
            None => crate::core::models::thinking_level_for_budget(self.thinking_budget),
        }
    }

    /// Check if the OAuth token is expired and refresh it if needed.
    pub async fn refresh_if_needed(&self) -> Result<()> {
        self.refresh_if_needed_for_model(&self.model).await
    }

    async fn refresh_if_needed_for_model(&self, model: &str) -> Result<()> {
        // Non-Anthropic models resolve their own provider auth in the OpenAI
        // path (incl. via the broker), so skip the Anthropic pre-fetch. (#158 #7)
        if !crate::runtime::auth::model_is_anthropic(model) {
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
        // Transport-construction seam: counted BEFORE preflight, auth
        // refresh, or any request assembly (CP-12 M3).
        self.remote_summarization_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let model = self.compaction_model();
        self.validate_request_preflight_for(
            model,
            crate::runtime::openai::catalog::CodexRequestRole::Internal,
        )
        .await?;
        self.refresh_if_needed_for_model(model).await?;

        use crate::runtime::compaction::COMPACTION_SYSTEM_PROMPT;

        ApiMethods::call_api_simple(
            &self.auth,
            &self.client,
            model,
            COMPACTION_SYSTEM_PROMPT,
            self.thinking_budget,
            self.reasoning_level(),
            &messages,
            self.api_retries,
            // Default options preserve this path's historical beta gating
            // and endpoint; only the session observability seams are wired
            // (Task 11) so compaction requests trace/persist like any other.
            // Deliberately the BASE context: an internal compaction request
            // must not consume a user's one-shot `/trace next` arm.
            &api::ApiOptions {
                trace: self.trace_ctx.clone(),
                telemetry: self.telemetry_writer.clone(),
                ..api::ApiOptions::default()
            },
        )
        .await
    }

    /// Run a single prompt synchronously (non-streaming). Handles tool execution
    /// internally, looping until the model produces a final text response.
    pub async fn run_single(&self, prompt: &str) -> Result<String> {
        self.validate_request_preflight().await?;
        let anthropic_execution_plan = self.authorized_anthropic_plan().await?;
        // Refresh OAuth token if expired only after capability preflight.
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
                self.reasoning_level(),
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
                    anthropic_execution_plan: anthropic_execution_plan.clone(),
                    codex_request_role: self.codex_request_role(),
                    // Consistent with the streaming path: a pending
                    // `/trace next` arm covers the first request of this
                    // loop too (the one-shot gate inside the armed context
                    // limits it to exactly one logical request).
                    trace: self.effective_trace_context(),
                    request_correlation: None,
                    suppress_stream_deltas: true,
                    telemetry: self.telemetry_writer.clone(),
                    // Non-stream path note (Task 16 review): threading the
                    // host session here gates extension-provider interior
                    // tool loops identically to stream turns. The built-in
                    // tool dispatch below (registry.get) remains the legacy
                    // non-stream path — tracked; the required pass gate is
                    // all STREAM-turn execution paths.
                    tool_session_id: Some(self.host_tool_session.clone()),
                    // Non-stream legacy loop: no retained per-stream set;
                    // the extension route falls back per its documented
                    // policy (fresh default-core, zero activations).
                    session_tool_set: None,
                    request_tools_schema: None,
                    usage_counters: None,
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
                                        delegation_parent: None,
                                        event_queue: Some(self.event_queue.clone()),
                                        secret_prompt: None,
                                        orchestration: self.orchestration.clone(),
                                        tool_activation: None,
                                        mcp_leases: None,
                                        extension_leases: None,
                                        memory_context: None,
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
                                                    delegation_parent: None,
                                                    event_queue: Some(event_queue_inner),
                                                    secret_prompt: None,
                                                    orchestration: orchestration_inner,
                                                    tool_activation: None,
                                                    mcp_leases: None,
                                                    extension_leases: None,
                                                    memory_context: None,
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
        // CP-11 fix-2 (A): the caller-facing boundary is BOUNDED. The
        // internal producer keeps an unbounded sender for API stability;
        // the relay drains it eagerly, enforces the fixed preview-delta
        // retention budget, and cancels the turn when the caller stream
        // is dropped (releasing provider tasks).
        let (tx, internal_rx) = mpsc::unbounded_channel();
        let bounded_rx =
            crate::runtime::relay::spawn_bounded_stream_relay(internal_rx, cancel.clone());

        // One correlation ID per turn: carried by the typed terminal outcome
        // (spec §5.2) so every frontend can tie the failure to trace lines.
        let turn_correlation_id = agent_core::next_turn_correlation_id();

        if let Err(error) = self.validate_request_preflight().await {
            let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
                helpers::turn_error_for(&error, &turn_correlation_id),
            )));
            let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
            return Box::pin(tokio_stream::wrappers::ReceiverStream::new(bounded_rx));
        }

        let anthropic_execution_plan = match self.authorized_anthropic_plan().await {
            Ok(plan) => plan,
            Err(error) => {
                let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
                    helpers::turn_error_for(&error, &turn_correlation_id),
                )));
                let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
                return Box::pin(tokio_stream::wrappers::ReceiverStream::new(bounded_rx));
            }
        };

        // Refresh OAuth token if expired after capability preflight.
        if let Err(e) = self.refresh_if_needed().await {
            let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
                helpers::turn_error_for(&e, &turn_correlation_id),
            )));
            let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
            return Box::pin(tokio_stream::wrappers::ReceiverStream::new(bounded_rx));
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
        let reasoning_level = self.reasoning_level();
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
        let reaper_orchestration = self.orchestration.clone();
        let event_queue = self.event_queue.clone();
        let options = api::ApiOptions {
            use_1m_context: self.context_window_override == Some(1_000_000),
            cache_ttl: self.cache_ttl,
            ttl_downgrade_notified: self.ttl_downgrade_notified.clone(),
            saw_1h_honored: self.saw_1h_honored.clone(),
            credential_source: self.credential_source.clone(),
            token_cache: self.token_cache.clone(),
            anthropic_base_url: None,
            anthropic_execution_plan,
            codex_request_role: self.codex_request_role(),
            trace: self.effective_trace_context(),
            request_correlation: None,
            suppress_stream_deltas: false,
            telemetry: self.telemetry_writer.clone(),
            // Threads the runtime-scoped gate identity into extension-
            // provider interior tool loops (Task 16).
            tool_session_id: Some(self.host_tool_session.clone()),
            // Placeholder: the stream loop installs its RETAINED shared
            // session tool set handle and request schema projection before
            // the first provider round.
            session_tool_set: None,
            request_tools_schema: None,
            usage_counters: None,
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
            reasoning_level,
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
            delegation_parent: self.delegation_parent.clone(),
            turn_correlation_id: turn_correlation_id.clone(),
            progressive_tool_disclosure: self.progressive_tool_disclosure,
            tool_session_id: self.host_tool_session.clone(),
            mcp_runtime: self.mcp_runtime.clone(),
            mcp_session_scope: self.mcp_session_scope.clone(),
            extension_runtime: self.extension_runtime.clone(),
            extension_session_scope: self.extension_session_scope.clone(),
            turn_budget: self.turn_budget.clone(),
        };

        tokio::spawn(async move {
            if let Err(e) = StreamMethods::run_stream_internal(session, messages).await {
                let _ = tx.send(StreamEvent::Session(SessionEvent::Error(
                    helpers::turn_error_for(&e, &turn_correlation_id),
                )));
            }
            // Engine-owned housekeeping: reap finished subagent handles before
            // signalling Done.  Runs on the tokio thread pool — no public sync
            // caller becomes async.  Poison-safe via reap_finished internals.
            crate::runtime::subagent::reap_finished(
                &reaper_registry,
                reaper_orchestration.as_deref(),
            );
            let _ = tx.send(StreamEvent::Session(SessionEvent::Done));
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(bounded_rx))
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
            named_level: self.named_level,
            explicit_reasoning: self.explicit_reasoning,
            codex_request_role: self.codex_request_role,
            context_window_override: self.context_window_override,
            compaction_model: self.compaction_model.clone(),
            compaction_mode: self.compaction_mode,
            compaction_exclusions: self.compaction_exclusions.clone(),
            remote_summarization_attempts: Arc::clone(&self.remote_summarization_attempts),
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
            // Shared session observability: clones (subagents) enqueue into
            // the SAME bounded writer and trace context — one worker and one
            // set of session-scoped context IDs per session.
            telemetry_writer: self.telemetry_writer.clone(),
            trace_ctx: self.trace_ctx.clone(),
            trace_controls: Arc::clone(&self.trace_controls),
            // Clones are the SAME session: `/memory` state is one truth
            // across a session's streams. Subagents are NOT clones — they
            // are constructed via a fresh `Runtime::new()` (see
            // `tools/subagent/mod.rs::apply_subagent_runtime_policy`), so
            // they always start Off/no-lease (task A5 invariant).
            memory_context_state: std::sync::Arc::clone(&self.memory_context_state),
            one_shot_trace_writer: Arc::clone(&self.one_shot_trace_writer),
            capture_dir: self.capture_dir.clone(),
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
            trusted_worker_models: self.trusted_worker_models.clone(),
            progressive_tool_disclosure: self.progressive_tool_disclosure,
            delegation_parent: self.delegation_parent.clone(),
            mcp_runtime: self.mcp_runtime.clone(),
            // Clones SHARE the durable session scope: dropping one clone or
            // one stream can never kill a sibling's leases.
            mcp_session_scope: self.mcp_session_scope.clone(),
            extension_runtime: self.extension_runtime.clone(),
            // Same durable shared-scope rule for extension leases.
            extension_session_scope: self.extension_session_scope.clone(),
            turn_budget: self.turn_budget.clone(),
            // Clones share the live tool registry, so they share the SAME
            // host tool session (matching existing shared-session behavior);
            // independently constructed runtimes mint fresh identities and
            // can never share session grants.
            host_tool_session: self.host_tool_session.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 16 session identity semantics: clones of one Runtime share the
    /// live tool registry, so they share the same host tool session;
    /// independently constructed runtimes mint fresh identities and can
    /// never share session grants.
    #[test]
    fn host_tool_session_shared_by_clones_fresh_per_runtime() {
        let rt = Runtime::new_headless();
        let clone = rt.clone();
        assert_eq!(
            rt.host_tool_session_id(),
            clone.host_tool_session_id(),
            "clones share the live registry and therefore the host session"
        );

        let other = Runtime::new_headless();
        assert_ne!(
            rt.host_tool_session_id(),
            other.host_tool_session_id(),
            "independently constructed runtimes must never share a session"
        );
    }

    /// Task 11 config rule (documented until Task 12 adds explicit trace
    /// config): telemetry `basic`/`full` enables the shared session writer
    /// sink — legacy telemetry AND metadata-only trace persistence; `off`
    /// disables both (trace context reverts to the no-op sink).
    #[test]
    fn telemetry_level_gates_shared_observability_sink() {
        let mut rt = Runtime::new_headless();
        assert!(!rt.trace_context().enabled(), "off by default");
        assert!(rt.telemetry_writer().is_none());

        rt.set_telemetry_level(crate::runtime::telemetry::TelemetryLevel::Basic);
        assert!(rt.trace_context().enabled(), "basic enables the trace sink");
        assert!(rt.telemetry_writer().is_some());

        // Same level again: the writer/session context is not recreated.
        let ctx_before = format!("{:?}", rt.trace_context());
        rt.set_telemetry_level(crate::runtime::telemetry::TelemetryLevel::Full);
        assert_eq!(format!("{:?}", rt.trace_context()), ctx_before);

        // Clones (subagents) share the same writer sink + session context.
        let clone = rt.clone();
        assert!(clone.trace_context().enabled());
        assert_eq!(format!("{:?}", clone.trace_context()), ctx_before);
        assert!(clone.telemetry_writer().is_some());

        rt.set_telemetry_level(crate::runtime::telemetry::TelemetryLevel::Off);
        assert!(!rt.trace_context().enabled(), "off disables both");
        assert!(rt.telemetry_writer().is_none());
        assert!(rt
            .shutdown_observability(std::time::Duration::ZERO)
            .is_none());
    }

    /// Owner-epilogue contract (Task 11): with telemetry Off the async
    /// flush is a `None` no-op and idempotent; with a writer installed, a
    /// chat-like epilogue (final record queued, then bounded flush) must
    /// persist the record; and a slow writer must return within the budget
    /// (TimedOut) instead of blocking exit.
    #[tokio::test]
    async fn shutdown_observability_async_epilogue_contract() {
        // Off → None, twice (idempotent no-op, no writer ever created).
        let rt = Runtime::new_headless();
        let budget = crate::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT;
        assert!(rt.shutdown_observability_async(budget).await.is_none());
        assert!(rt.shutdown_observability_async(budget).await.is_none());

        // Chat-like shutdown: the final queued record is persisted.
        let tmp = tempfile::tempdir().unwrap();
        let telemetry_path = tmp.path().join("synaps/api-log.jsonl");
        let mut rt = Runtime::new_headless();
        let writer = crate::runtime::telemetry::TelemetryWriter::new(
            crate::runtime::telemetry::WriterOptions {
                telemetry_path: Some(telemetry_path.clone()),
                trace_path: Some(tmp.path().join("synaps/request-trace.jsonl")),
                ..Default::default()
            },
        );
        rt.install_observability_for_tests(writer.clone());
        writer.enqueue_telemetry(crate::runtime::telemetry::TelemetryRecord {
            ts: 7,
            model: "claude-sonnet-4-6".to_string(),
            ..Default::default()
        });
        let outcome = rt
            .shutdown_observability_async(budget)
            .await
            .expect("writer installed");
        assert!(outcome.is_flushed());
        assert_eq!(outcome.stats().written, 1);
        assert_eq!(
            std::fs::read_to_string(&telemetry_path)
                .unwrap()
                .lines()
                .count(),
            1,
            "queued final record must be on disk after the epilogue flush"
        );
        // Idempotent second call: intake already closed, still Flushed.
        assert!(rt
            .shutdown_observability_async(budget)
            .await
            .expect("writer still installed")
            .is_flushed());

        // Slow writer: the epilogue returns within its bounded budget.
        let mut rt = Runtime::new_headless();
        let slow = crate::runtime::telemetry::TelemetryWriter::new(
            crate::runtime::telemetry::WriterOptions {
                telemetry_path: Some(tmp.path().join("slow/api-log.jsonl")),
                trace_path: Some(tmp.path().join("slow/request-trace.jsonl")),
                write_delay: Some(std::time::Duration::from_millis(300)),
                ..Default::default()
            },
        );
        rt.install_observability_for_tests(slow.clone());
        for _ in 0..10 {
            slow.enqueue_telemetry(crate::runtime::telemetry::TelemetryRecord::default());
        }
        let start = std::time::Instant::now();
        let outcome = rt
            .shutdown_observability_async(std::time::Duration::from_millis(200))
            .await
            .expect("writer installed");
        assert!(!outcome.is_flushed(), "10×300ms cannot drain in 200ms");
        // Generous ceiling (no timing fragility): well under the 3s a full
        // drain would need, proving the deadline was honored.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(1500),
            "bounded epilogue must not block exit: {:?}",
            start.elapsed()
        );
    }

    /// Task 12 fix: an armed one-shot ephemeral writer (telemetry Off +
    /// `/trace next`) is retained by the runtime and drained by the same
    /// shutdown epilogue as the session writer, so the armed record cannot
    /// be lost to an exit racing the background worker.
    #[tokio::test]
    async fn one_shot_ephemeral_writer_is_retained_and_drained_at_shutdown() {
        let rt = Runtime::new_headless();
        assert!(!rt.trace_context().enabled(), "telemetry off");
        rt.trace_arm_next(false);
        let armed = rt.effective_trace_context();
        assert!(armed.enabled(), "armed ephemeral context traces");
        // No session writer, but the epilogue still finds (and drains) the
        // retained one-shot writer.
        let budget = crate::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT;
        let outcome = rt
            .shutdown_observability_async(budget)
            .await
            .expect("retained one-shot writer must be drained by the epilogue");
        assert!(outcome.is_flushed());
        // Consumed: a second epilogue call finds nothing.
        assert!(rt.shutdown_observability_async(budget).await.is_none());
    }

    /// Task 12 operational expiry: stale content-capture bundles are
    /// physically removed at Runtime startup and by BOTH shutdown
    /// epilogues (sync and async), not only on trace interactions.
    /// Combined into one serialized test: all three paths resolve the
    /// capture dir through `SYNAPS_BASE_DIR`.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn startup_and_shutdown_paths_sweep_expired_captures() {
        struct BaseDirGuard(Option<String>);
        impl Drop for BaseDirGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(old) => std::env::set_var("SYNAPS_BASE_DIR", old),
                    None => std::env::remove_var("SYNAPS_BASE_DIR"),
                }
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let guard = BaseDirGuard(std::env::var("SYNAPS_BASE_DIR").ok());
        std::env::set_var("SYNAPS_BASE_DIR", tmp.path());

        let cap_dir = trace::default_capture_dir();
        assert!(
            cap_dir.starts_with(tmp.path()),
            "capture dir must be private to the test"
        );
        let stale_id = trace::TraceId::new("req-stale").unwrap();
        let plant_stale = || {
            agent_core::core::private_fs::ensure_private_dir(&cap_dir).unwrap();
            let stale = trace::controls::ContentCaptureBundle {
                schema: trace::CONTENT_CAPTURE_SCHEMA.to_string(),
                request_id: stale_id.clone(),
                created_unix_ms: 1_000,
                expires_unix_ms: 2_000, // long past
                redacted: true,
                over_budget: false,
                body: Some(serde_json::json!({"old": true})),
            };
            let path = trace::controls::capture_path(&cap_dir, &stale_id);
            std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
            path
        };

        // 1) Runtime startup sweeps.
        let stale_path = plant_stale();
        let rt = Runtime::new().await.expect("runtime constructs");
        assert!(
            !stale_path.exists(),
            "Runtime startup must physically remove expired capture bundles"
        );

        // 2) Sync shutdown epilogue sweeps (telemetry off → None outcome).
        let stale_path = plant_stale();
        let _ = rt.shutdown_observability(std::time::Duration::ZERO);
        assert!(
            !stale_path.exists(),
            "sync shutdown epilogue must remove expired capture bundles"
        );

        // 3) Async shutdown epilogue sweeps.
        let stale_path = plant_stale();
        let _ = rt
            .shutdown_observability_async(std::time::Duration::from_millis(100))
            .await;
        assert!(
            !stale_path.exists(),
            "async shutdown epilogue must remove expired capture bundles"
        );
        drop(guard);
    }

    /// fix1 I2b: the capture root binds at CONSTRUCTION. Ambient
    /// SYNAPS_BASE_DIR churn after construction must not redirect the
    /// sweep — each runtime keeps sweeping its own root (concurrent
    /// roots), so a parallel test flipping env can never race the epilogue.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn capture_root_binds_at_construction_and_survives_env_churn() {
        let base_a = tempfile::tempdir().unwrap();
        let base_b = tempfile::tempdir().unwrap();
        let old = std::env::var("SYNAPS_BASE_DIR").ok();

        std::env::set_var("SYNAPS_BASE_DIR", base_a.path());
        let rt_a = Runtime::new().await.expect("runtime A");
        std::env::set_var("SYNAPS_BASE_DIR", base_b.path());
        let rt_b = Runtime::new().await.expect("runtime B");

        let cap_a = base_a.path().join("trace").join("capture");
        let cap_b = base_b.path().join("trace").join("capture");
        let plant = |dir: &std::path::Path, name: &str| {
            agent_core::core::private_fs::ensure_private_dir(dir).unwrap();
            let id = trace::TraceId::new(name).unwrap();
            let stale = trace::controls::ContentCaptureBundle {
                schema: trace::CONTENT_CAPTURE_SCHEMA.to_string(),
                request_id: id.clone(),
                created_unix_ms: 1_000,
                expires_unix_ms: 2_000,
                redacted: true,
                over_budget: false,
                body: Some(serde_json::json!({"old": true})),
            };
            let path = trace::controls::capture_path(dir, &id);
            std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
            path
        };

        // Point ambient env somewhere else entirely — the bound roots must win.
        let decoy = tempfile::tempdir().unwrap();
        std::env::set_var("SYNAPS_BASE_DIR", decoy.path());

        let stale_a = plant(&cap_a, "req-stale-a");
        let stale_b = plant(&cap_b, "req-stale-b");
        let _ = rt_a.shutdown_observability(std::time::Duration::ZERO);
        assert!(
            !stale_a.exists(),
            "runtime A must sweep the root bound at ITS construction"
        );
        assert!(
            stale_b.exists(),
            "runtime A must never sweep another runtime's root"
        );
        let _ = rt_b
            .shutdown_observability_async(std::time::Duration::from_millis(100))
            .await;
        assert!(
            !stale_b.exists(),
            "runtime B must sweep the root bound at ITS construction"
        );

        match old {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
    }

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
    fn worker_install_does_not_replay_process_global_favorite_grants() {
        let foreground =
            crate::orchestration::canonical_foreground_identity("anthropic/claude-fable-5")
                .unwrap();
        let session = Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 4, 8).unwrap(),
        );
        let favorite = "openai-codex/gpt-5.6-sol";
        assert!(
            session.preflight(favorite).is_err(),
            "precondition: the parent session did not grant this identity"
        );

        let mut worker = Runtime::new_headless();
        worker.apply_config(&crate::config::SynapsConfig {
            favorite_models: vec![favorite.to_owned()],
            ..Default::default()
        });
        worker.install_worker_orchestration(Arc::clone(&session));

        assert!(
            session.preflight(favorite).is_err(),
            "constructing a child must not mint/replay a favorite-model grant"
        );
        assert!(!session.effective_choices().contains(&favorite.to_owned()));
    }

    #[test]
    fn host_install_may_apply_explicit_operator_favorites() {
        let mut runtime = Runtime::new_headless();
        let config = crate::config::SynapsConfig {
            favorite_models: vec![
                "openai-codex/gpt-5.6-luna".to_owned(),
                "anthropic/claude-opus-4-6".to_owned(),
                // Legacy favorite spelling must retain its existing compatibility.
                "claude/claude-fable-5".to_owned(),
                // Malformed persisted values must fail closed without bricking boot.
                "not-qualified".to_owned(),
            ],
            ..Default::default()
        };
        runtime.apply_config(&config);
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("openai-codex/gpt-5.6-sol").unwrap();
        runtime.install_orchestration(std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 3, 8).unwrap(),
        ));

        let orchestration = runtime.orchestration().unwrap();
        for trusted in [
            "openai-codex/gpt-5.6-sol",
            "openai-codex/gpt-5.6-luna",
            "anthropic/claude-opus-4-6",
            "anthropic/claude-fable-5",
        ] {
            assert!(
                orchestration
                    .effective_choices()
                    .contains(&trusted.to_owned()),
                "missing configured worker choice {trusted}: {:?}",
                orchestration.effective_choices()
            );
        }
        let authorized = orchestration
            .resolve_and_authorize("sa_cross_provider", Some("anthropic/claude-opus-4-6"))
            .expect("an explicitly favorited cross-provider model must be authorized");
        assert_eq!(authorized.model.as_str(), "anthropic/claude-opus-4-6");
        assert!(!orchestration
            .effective_choices()
            .contains(&"not-qualified".to_owned()));
    }

    #[test]
    fn configured_worker_choices_survive_manifestless_foreground_changes() {
        let mut runtime = Runtime::new_headless();
        runtime.apply_config(&crate::config::SynapsConfig {
            favorite_models: vec!["anthropic/claude-opus-4-6".to_owned()],
            ..Default::default()
        });
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("openai-codex/gpt-5.6-sol").unwrap();
        runtime.install_orchestration(std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 3, 8).unwrap(),
        ));

        runtime
            .try_set_model("xai-auth/grok-4.5-latest".to_owned())
            .unwrap();

        let orchestration = runtime.orchestration().unwrap();
        assert_eq!(orchestration.foreground_model(), "xai-auth/grok-4.5-latest");
        assert!(orchestration
            .effective_choices()
            .contains(&"anthropic/claude-opus-4-6".to_owned()));
        orchestration
            .resolve_and_authorize("sa_after_switch", Some("anthropic/claude-opus-4-6"))
            .expect("configured worker trust must survive policy replacement");
    }

    #[test]
    fn set_model_replaces_manifestless_orchestration_foreground_snapshot() {
        let mut runtime = Runtime::new_headless();
        let glm = agent_core::prompt::QualifiedModelId::parse("openrouter/z-ai/glm-5.2").unwrap();
        runtime.install_orchestration(std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(glm, 3, 8).unwrap(),
        ));

        runtime.set_model("openrouter/deepseek/deepseek-v4-pro".to_owned());

        let orchestration = runtime.orchestration().unwrap();
        assert_eq!(
            orchestration.foreground_model(),
            "openrouter/deepseek/deepseek-v4-pro"
        );
        assert!(orchestration
            .effective_choices()
            .contains(&"openrouter/moonshotai/kimi-k2.7-code".to_owned()));
        assert!(orchestration
            .resolve_and_authorize("sa_kimi", Some("openrouter/moonshotai/kimi-k2.7-code"))
            .is_ok());
    }

    #[test]
    fn set_model_refuses_to_orphan_unreconciled_policy_workers() {
        let mut runtime = Runtime::new_headless();
        let glm = agent_core::prompt::QualifiedModelId::parse("openrouter/z-ai/glm-5.2").unwrap();
        let orchestration = std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(glm, 3, 8).unwrap(),
        );
        orchestration
            .resolve_and_authorize("sa_active", None)
            .unwrap();
        runtime.install_orchestration(orchestration.clone());
        runtime.model = "openrouter/z-ai/glm-5.2".to_owned();

        let error = runtime
            .try_set_model("openrouter/deepseek/deepseek-v4-pro".to_owned())
            .unwrap_err();

        assert!(error.contains("collection/reconciliation"));
        assert_eq!(runtime.model(), "openrouter/z-ai/glm-5.2");
        assert!(std::sync::Arc::ptr_eq(
            runtime.orchestration().unwrap(),
            &orchestration
        ));
        assert!(orchestration.is_unreconciled("sa_active"));
    }

    #[test]
    fn set_model_keeps_active_typed_manifest_orchestration_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("prompt.yaml");
        let glm = agent_core::prompt::QualifiedModelId::parse("openrouter/z-ai/glm-5.2").unwrap();
        let mut runtime = Runtime::new_headless();
        let orchestration = std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(glm.clone(), 3, 8).unwrap(),
        );
        runtime.install_orchestration(orchestration.clone());
        runtime.retain_prompt_reload_source(
            manifest_path,
            agent_core::prompt::SelectionContext::new(glm, None).unwrap(),
            None,
            None,
        );

        runtime.set_model("openrouter/deepseek/deepseek-v4-pro".to_owned());

        assert!(std::sync::Arc::ptr_eq(
            runtime.orchestration().unwrap(),
            &orchestration
        ));
        assert_eq!(
            runtime.orchestration().unwrap().foreground_model(),
            "openrouter/z-ai/glm-5.2"
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
        assert!(truncated.contains("[truncated — 30001 total bytes, showing first 30000]"));

        // Should be longer than max (due to notice)
        assert!(truncated.len() > 30000);

        // Test with a much longer string
        let very_long = "a".repeat(50000);
        let truncated_very_long = HelperMethods::truncate_tool_result(&very_long, default_max);
        assert!(
            truncated_very_long.contains("[truncated — 50000 total bytes, showing first 30000]")
        );
        assert!(truncated_very_long.starts_with(&"a".repeat(30000)));

        // Test with custom limit
        let custom_truncated = HelperMethods::truncate_tool_result(&very_long, 100);
        assert!(custom_truncated.starts_with(&"a".repeat(100)));
        assert!(custom_truncated.contains("[truncated — 50000 total bytes, showing first 100]"));
    }

    /// T2 red→green: `truncate_tool_result` must enforce a BYTE budget on
    /// multibyte input (legacy code used `chars().take(n)` — a char budget —
    /// so 100 two-byte chars under a 50-byte budget kept 100 bytes).
    #[test]
    fn truncate_tool_result_enforces_byte_budget() {
        let multibyte = "é".repeat(100); // 200 bytes, 100 chars
        let out = HelperMethods::truncate_tool_result(&multibyte, 50);
        let body = out.split("\n\n[truncated").next().unwrap();
        assert!(
            body.len() <= 50,
            "retained body must fit the byte budget (got {} bytes)",
            body.len()
        );
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
        assert!(
            out.contains("total bytes"),
            "marker must report byte counts, got: {out}"
        );
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
    async fn anthropic_foreground_ultracode_composes_standing_doctrine_once() {
        let mut runtime = headless("anthropic/claude-fable-5", "BASE.");
        runtime.named_level = Some(agent_core::reasoning::ReasoningLevel::UltraCode);
        let prompt = runtime.effective_system_prompt().await.expect("doctrine");
        assert_eq!(prompt.matches("<anthropic-ultracode-workflow>").count(), 1);
        assert!(prompt.contains("subagent_start"));
        assert!(prompt.contains("subagent_resume"));
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

#[cfg(test)]
mod set_reasoning_level_checked_tests {
    use super::*;
    use agent_core::reasoning::ReasoningLevel;

    fn codex_runtime(model_id: &str) -> Runtime {
        let mut rt = Runtime::new_headless();
        rt.set_model(format!("openai-codex/{model_id}"));
        rt.set_reasoning_level(ReasoningLevel::Low);
        rt
    }

    fn provisioned_fable_runtime() -> Runtime {
        let mut rt = Runtime::new_headless();
        rt.set_model("anthropic/claude-fable-5".into());
        rt.set_reasoning_level(ReasoningLevel::Low);
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-fable-5").unwrap();
        let orchestration =
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 3, 8).unwrap();
        rt.install_orchestration(Arc::new(orchestration));
        rt
    }

    fn install_typed_manifest(rt: &mut Runtime) {
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
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-fable-5").unwrap(),
            None,
        )
        .unwrap();
        rt.apply_prompt_stack(agent_core::prompt::PromptStack::new(vec![module], context).unwrap())
            .unwrap();
    }

    #[test]
    fn provisioned_fable_accepts_max_and_ultracode() {
        for level in [ReasoningLevel::Max, ReasoningLevel::UltraCode] {
            let mut rt = provisioned_fable_runtime();
            rt.set_reasoning_level_checked(level).unwrap();
            assert_eq!(rt.reasoning_level(), level);
        }
    }

    #[test]
    fn fable_ultracode_prerequisites_fail_without_mutation() {
        let mut missing_orchestration = Runtime::new_headless();
        missing_orchestration.set_model("anthropic/claude-fable-5".into());
        missing_orchestration.set_reasoning_level(ReasoningLevel::Low);
        assert!(missing_orchestration
            .set_reasoning_level_checked(ReasoningLevel::UltraCode)
            .is_err());
        assert_eq!(missing_orchestration.reasoning_level(), ReasoningLevel::Low);

        let mut missing_tools = provisioned_fable_runtime();
        missing_tools.set_tools(ToolRegistry::without_subagent());
        assert!(missing_tools
            .set_reasoning_level_checked(ReasoningLevel::UltraCode)
            .is_err());
        assert_eq!(missing_tools.reasoning_level(), ReasoningLevel::Low);

        let mut worker = provisioned_fable_runtime();
        worker.set_codex_request_role(crate::runtime::openai::catalog::ExecutionRole::Worker);
        assert!(worker
            .set_reasoning_level_checked(ReasoningLevel::UltraCode)
            .is_err());
        assert_eq!(worker.reasoning_level(), ReasoningLevel::Low);

        let mut internal = provisioned_fable_runtime();
        internal.set_codex_request_role(crate::runtime::openai::catalog::ExecutionRole::Internal);
        assert!(internal
            .set_reasoning_level_checked(ReasoningLevel::UltraCode)
            .is_err());
        assert_eq!(internal.reasoning_level(), ReasoningLevel::Low);

        let mut manifest = provisioned_fable_runtime();
        install_typed_manifest(&mut manifest);
        assert!(manifest
            .set_reasoning_level_checked(ReasoningLevel::UltraCode)
            .is_err());
        assert_eq!(manifest.reasoning_level(), ReasoningLevel::Low);
    }

    #[test]
    fn checked_accepts_client_omission_modes_for_codex() {
        for level in [ReasoningLevel::Off, ReasoningLevel::Adaptive] {
            let mut rt = codex_runtime("gpt-5.6-sol");
            rt.set_reasoning_level_checked(level).unwrap();
            assert_eq!(rt.reasoning_level(), level);
        }
    }

    /// luna does not support Ultra; checked must reject and leave level unchanged.
    #[test]
    fn checked_rejects_ultra_for_luna_no_mutation() {
        let mut rt = codex_runtime("gpt-5.6-luna");
        let before = rt.reasoning_level();
        let err = rt
            .set_reasoning_level_checked(ReasoningLevel::Ultra)
            .unwrap_err();
        assert!(
            err.contains("ultra"),
            "error must name the rejected level; got: {err}"
        );
        assert_eq!(
            rt.reasoning_level(),
            before,
            "runtime must not be mutated when validation fails"
        );
    }

    /// gpt-5.5 does not support Max; checked must reject and not mutate.
    #[test]
    fn checked_rejects_max_for_gpt55_no_mutation() {
        let mut rt = codex_runtime("gpt-5.5");
        rt.set_reasoning_level(ReasoningLevel::Medium);
        let err = rt
            .set_reasoning_level_checked(ReasoningLevel::Max)
            .unwrap_err();
        assert!(
            err.contains("max"),
            "error must name the rejected level; got: {err}"
        );
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Medium);
    }

    /// sol supports Ultra; checked must accept and mutate.
    #[test]
    fn checked_accepts_ultra_for_sol_and_mutates() {
        let mut rt = codex_runtime("gpt-5.6-sol");
        rt.set_reasoning_level_checked(ReasoningLevel::Ultra)
            .expect("sol must accept ultra");
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Ultra);
    }

    /// Providers without exact metadata must not gain Max/Ultra.
    #[test]
    fn checked_rejects_extended_levels_without_metadata() {
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-opus-4-7".to_string());
        rt.set_reasoning_level(ReasoningLevel::Low);
        for level in [ReasoningLevel::Max, ReasoningLevel::Ultra] {
            assert!(rt.set_reasoning_level_checked(level).is_err());
            assert_eq!(rt.reasoning_level(), ReasoningLevel::Low);
        }
    }

    /// Gap fix: unknown Codex ids (no cache, no static metadata) must reject
    /// the extended Max/Ultra modes at mutation time, fail closed.
    #[test]
    fn checked_rejects_max_ultra_for_unknown_codex_model() {
        let mut rt = codex_runtime("gpt-unknown-future");
        rt.set_reasoning_level(ReasoningLevel::Low);
        for level in [ReasoningLevel::Max, ReasoningLevel::Ultra] {
            let err = rt.set_reasoning_level_checked(level).unwrap_err();
            assert!(err.contains("no capability metadata"), "{err}");
            assert_eq!(rt.reasoning_level(), ReasoningLevel::Low);
        }
    }

    /// xAI: Off is rejected (not silently omitted) on models whose reasoning
    /// cannot be disabled; unsupported named efforts are rejected exactly.
    #[test]
    fn checked_enforces_xai_capability_matrix_no_mutation_on_err() {
        let mut rt = Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.5".to_string());
        // Model default (not explicit) applied on switch: documented high.
        assert_eq!(rt.reasoning_level(), ReasoningLevel::High);
        for level in [
            ReasoningLevel::Off,
            ReasoningLevel::XHigh,
            ReasoningLevel::Ultra,
        ] {
            let before = rt.reasoning_level();
            assert!(rt.set_reasoning_level_checked(level).is_err(), "{level}");
            assert_eq!(rt.reasoning_level(), before);
        }
        rt.set_reasoning_level_checked(ReasoningLevel::Low)
            .expect("grok-4.5 supports low");
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Low);
    }

    /// xAI models without documented effort control reject explicit named
    /// levels; model switch applies the Adaptive provider-default.
    #[test]
    fn checked_rejects_named_on_intrinsic_xai_model_and_defaults_adaptive() {
        let mut rt = Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.3".to_string());
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Adaptive);
        assert!(rt
            .set_reasoning_level_checked(ReasoningLevel::Medium)
            .is_err());
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Adaptive);
        rt.set_reasoning_level_checked(ReasoningLevel::Adaptive)
            .unwrap();
    }

    /// Explicit user choice survives an xAI model switch (no default overwrite).
    #[test]
    fn explicit_level_survives_xai_model_switch() {
        let mut rt = Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.5".to_string());
        rt.set_reasoning_level_checked(ReasoningLevel::Low).unwrap();
        rt.set_model("xai-auth/grok-4.5-latest".to_string());
        assert_eq!(
            rt.reasoning_level(),
            ReasoningLevel::Low,
            "explicit level must not be overwritten by the model default"
        );
    }
}

#[cfg(test)]
mod codex_execution_preflight_tests {
    use super::*;
    use crate::runtime::openai::catalog::CodexRequestRole;
    use agent_core::prompt::QualifiedModelId;
    use agent_core::reasoning::ReasoningLevel;

    fn ultra_runtime() -> Runtime {
        let mut runtime = Runtime::new_headless();
        runtime.set_model("openai-codex/gpt-5.6-sol".to_string());
        runtime.set_reasoning_level_explicit(ReasoningLevel::Ultra);
        runtime
    }

    fn install_baseline(runtime: &mut Runtime) {
        let foreground =
            QualifiedModelId::parse("openai-codex/gpt-5.6-sol").expect("qualified model");
        let orchestration = crate::orchestration::OrchestrationRuntime::baseline(foreground, 3, 8)
            .expect("baseline orchestration");
        runtime.install_orchestration(Arc::new(orchestration));
    }

    #[tokio::test]
    async fn foreground_ultra_preflight_requires_orchestration() {
        let runtime = ultra_runtime();
        let error = runtime
            .validate_request_preflight()
            .await
            .expect_err("Ultra without orchestration must fail closed");
        assert!(error.to_string().contains("orchestration"), "{error}");
    }

    #[tokio::test]
    async fn foreground_ultra_preflight_requires_actionable_subagent_tools() {
        let mut runtime = ultra_runtime();
        install_baseline(&mut runtime);
        runtime.set_tools(ToolRegistry::without_subagent());

        let error = runtime
            .validate_request_preflight()
            .await
            .expect_err("Ultra without subagent tools must fail closed");
        assert!(error.to_string().contains("subagent"), "{error}");
    }

    #[tokio::test]
    async fn foreground_ultra_preflight_accepts_exact_model_policy_and_tools() {
        let mut runtime = ultra_runtime();
        install_baseline(&mut runtime);
        runtime
            .validate_request_preflight()
            .await
            .expect("fully provisioned Ultra must pass preflight");
    }

    #[tokio::test]
    async fn worker_ultra_preflight_never_requires_or_enables_orchestration() {
        let mut runtime = ultra_runtime();
        runtime.set_codex_request_role(CodexRequestRole::Worker);
        runtime.set_tools(ToolRegistry::without_subagent());
        runtime
            .validate_request_preflight()
            .await
            .expect("worker Ultra is underlying max without proactive orchestration");
        assert_eq!(runtime.codex_request_role(), CodexRequestRole::Worker);
    }

    #[tokio::test]
    async fn non_codex_ultra_preflight_fails_before_provider_work() {
        let mut runtime = Runtime::new_headless();
        runtime.set_model("anthropic/claude-opus-4-7".to_string());
        runtime.set_reasoning_level_explicit(ReasoningLevel::Ultra);
        let error = runtime
            .validate_request_preflight()
            .await
            .expect_err("non-Codex Ultra must fail closed");
        assert!(
            error.to_string().contains("unsupported_reasoning_level"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn compaction_preflight_validates_target_before_credentials() {
        let mut runtime = ultra_runtime();
        runtime.set_compaction_model(Some("claude-sonnet-4-6".to_string()));
        {
            let mut auth = runtime.auth.write().await;
            auth.auth_token.clear();
            auth.auth_type = "none".to_string();
            auth.refresh_token = None;
            auth.token_expires = None;
        }

        let error = runtime
            .compact_call(Vec::new())
            .await
            .expect_err("Codex Ultra cannot silently cross into Anthropic compaction");
        assert!(
            error.to_string().contains("authoritative exact-model"),
            "preflight must reject the target before auth access: {error}"
        );
    }
}

#[cfg(test)]
mod http_client_timeout_tests {
    //! Regression tests for the runtime HTTP client's timeout semantics.
    //!
    //! The client used to set `.timeout(300s)` — a **total wall-clock
    //! deadline** that keeps ticking even while a streaming response is
    //! actively delivering bytes (reqwest `TotalTimeoutBody` never resets).
    //! Two failure modes: a dead connection took a full 5 minutes to
    //! surface (incident: session 20260714-025948-3dab), and any healthy
    //! stream longer than 300s was killed mid-flight. The fix is an
    //! idle-based `read_timeout` that resets on every received chunk.
    //!
    //! Servers here are raw TCP writing chunked HTTP/1.1 — full control
    //! over inter-chunk delays with no framework buffering in the way.

    use futures::StreamExt;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    const CHUNKED_HEAD: &[u8] =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";

    fn chunk(data: &str) -> Vec<u8> {
        format!("{:x}\r\n{}\r\n", data.len(), data).into_bytes()
    }

    /// Spawn a raw server: writes headers, then `n_chunks` chunks spaced
    /// `gap` apart, then (optionally) the terminating chunk.
    async fn spawn_chunked_server(n_chunks: usize, gap: Duration, terminate: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request head (we don't care about its contents).
            let mut buf = [0u8; 4096];
            use tokio::io::AsyncReadExt;
            let _ = sock.read(&mut buf).await;
            sock.write_all(CHUNKED_HEAD).await.unwrap();
            for i in 0..n_chunks {
                sock.write_all(&chunk(&format!("data {i}\n")))
                    .await
                    .unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(gap).await;
            }
            if terminate {
                sock.write_all(b"0\r\n\r\n").await.unwrap();
            } else {
                // Stall forever (until the client gives up and drops us).
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        format!("http://{addr}/stream")
    }

    async fn drain(
        client: &reqwest::Client,
        url: &str,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let resp = client.get(url).send().await?;
        let mut stream = resp.bytes_stream();
        let mut total = 0usize;
        while let Some(c) = stream.next().await {
            total += c?.len();
        }
        Ok(total)
    }

    /// A healthy stream that keeps delivering bytes must NOT be killed,
    /// even when its total duration exceeds the read timeout — received
    /// data resets the idle clock. (This is the property the old total
    /// deadline violated at 300s.)
    #[tokio::test]
    async fn active_stream_outliving_read_timeout_survives() {
        // 15 chunks × 100ms ≈ 1.5s total, read_timeout = 400ms.
        let url = spawn_chunked_server(15, Duration::from_millis(100), true).await;
        let client = super::build_http_client(Duration::from_millis(400)).expect("client builds");
        let total = drain(&client, &url)
            .await
            .expect("active stream must never be killed by the idle timeout");
        assert!(total > 0);
    }

    /// A stream that stalls (bytes stop arriving) must be killed within
    /// the read timeout — not after a 300s total deadline.
    #[tokio::test]
    async fn stalled_stream_is_killed_by_read_timeout() {
        // 2 quick chunks then permanent stall, read_timeout = 300ms.
        let url = spawn_chunked_server(2, Duration::from_millis(10), false).await;
        let client = super::build_http_client(Duration::from_millis(300)).expect("client builds");
        let start = std::time::Instant::now();
        let err = drain(&client, &url)
            .await
            .expect_err("stalled stream must be detected");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "stall must be detected promptly, took {:?}",
            start.elapsed()
        );
        let msg = crate::core::error::error_chain_string(err.as_ref());
        assert!(
            msg.contains("timed out") || msg.contains("timeout"),
            "must be a timeout error: {msg}"
        );
    }

    /// A server that accepts but never sends response headers must be
    /// killed by the read timeout too (the incident's failure mode —
    /// previously took the full 300s total deadline to surface).
    #[tokio::test]
    async fn hung_headers_are_killed_by_read_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let client = super::build_http_client(Duration::from_millis(300)).expect("client builds");
        let start = std::time::Instant::now();
        let err = client
            .get(format!("http://{addr}/codex/responses"))
            .send()
            .await
            .expect_err("hung request must be detected");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hung headers must be detected promptly, took {:?}",
            start.elapsed()
        );
        assert!(err.is_timeout(), "must be a timeout: {err:?}");
    }
}

/// Task A6: memory-context provider validation + revocation wiring.
#[cfg(test)]
mod memory_context_provider_tests {
    use super::{memory_context, Runtime};
    use std::sync::Arc;

    // ── task A6: memory-context provider validation + revocation wiring ──

    /// Build a headless Runtime with the extension runtime installed (the
    /// exact `install_extension_runtime` wiring point the engine uses at
    /// boot) and one deferred context-provider-only extension loaded per
    /// `(plugin, provider-id)` pair. Nothing spawns: context-provider
    /// manifests classify as deferred and stay dormant.
    async fn memory_runtime_with_providers(
        plugins: &[(&str, &str)],
    ) -> (
        Runtime,
        std::sync::Arc<crate::extensions::lease::ExtensionRuntimeManager>,
    ) {
        let mut manager = crate::extensions::manager::ExtensionManager::new(Arc::new(
            crate::extensions::hooks::HookBus::new(),
        ));
        manager.set_progressive_deferral(true);
        for (plugin, provider) in plugins {
            let manifest: crate::extensions::manifest::ExtensionManifest =
                serde_json::from_value(serde_json::json!({
                    "runtime": "process",
                    "command": "/bin/false",
                    "permissions": ["context_providers.register"],
                    "deferred": {
                        "context_providers": [{
                            "id": provider,
                            "capability": "project-memory",
                            "description": "test context provider",
                            "schema_version": 1
                        }]
                    }
                }))
                .expect("manifest parses");
            manager
                .load(plugin, &manifest)
                .await
                .expect("deferred context-provider load succeeds without spawning");
        }
        let extension_runtime = manager.extension_runtime();
        let mut runtime = Runtime::new_headless();
        runtime.install_extension_runtime(std::sync::Arc::clone(&extension_runtime));
        (runtime, extension_runtime)
    }

    fn assert_memory_off_no_lease(runtime: &Runtime) {
        let status = runtime.memory_context_status();
        assert_eq!(
            status.durable,
            memory_context::DurableStatus::Off,
            "state must stay Off"
        );
        assert_eq!(
            status.one_shot,
            memory_context::OneShotStatus::Idle,
            "no one-shot lease may exist"
        );
        assert!(
            runtime.memory_bound_providers_for_test().is_empty(),
            "no provider may be bound"
        );
    }

    /// Task A6: enabling against a catalog that does not contain the
    /// requested provider fails closed — typed error, nothing granted,
    /// `SessionMemoryState` unchanged (Off/no-lease) — both for an
    /// explicitly requested nonexistent id and for an id-less request
    /// against an empty catalog.
    #[tokio::test]
    async fn memory_enable_unregistered_provider_fails_closed_grants_nothing() {
        // Explicit nonexistent id against a catalog with one real provider.
        let (runtime, _ext) =
            memory_runtime_with_providers(&[("axel-memory-manager", "project-memory")]).await;
        for requested in ["no-such-provider", "bad:id"] {
            let err = runtime
                .memory_context_enable_resolved(
                    memory_context::MemoryContextMode::CaptureAndRecall,
                    memory_context::mint_explicit_command_proof(),
                    Some(requested),
                )
                .expect_err("unregistered provider must fail closed");
            assert_eq!(
                err,
                memory_context::MemoryContextError::ProviderNotRegistered
            );
            assert_memory_off_no_lease(&runtime);
        }

        // Id-less request against an EMPTY catalog (extension subsystem
        // installed, no context-provider extension loaded).
        let (runtime, _ext) = memory_runtime_with_providers(&[]).await;
        let err = runtime
            .memory_context_enable(
                memory_context::MemoryContextMode::CaptureAndRecall,
                memory_context::mint_explicit_command_proof(),
            )
            .expect_err("empty catalog must fail closed");
        assert_eq!(
            err,
            memory_context::MemoryContextError::ProviderNotRegistered
        );
        assert_memory_off_no_lease(&runtime);
        // One-shot grants run the same validation.
        let err = runtime
            .memory_context_recall_once(memory_context::mint_explicit_command_proof())
            .expect_err("one-shot grant must validate the provider too");
        assert_eq!(
            err,
            memory_context::MemoryContextError::ProviderNotRegistered
        );
        assert_memory_off_no_lease(&runtime);
    }

    /// Task A6: with exactly one matching declared provider the grant
    /// succeeds and records THAT provider's exact composed runtime
    /// address — for the default id-less request and the explicit exact id.
    #[tokio::test]
    async fn memory_enable_exactly_one_declared_provider_records_exact_identity() {
        const ADDRESS: &str = "extension:axel-memory-manager:project-memory";
        // Default (id-less) request.
        let (runtime, _ext) =
            memory_runtime_with_providers(&[("axel-memory-manager", "project-memory")]).await;
        let status = runtime
            .memory_context_enable(
                memory_context::MemoryContextMode::CaptureAndRecall,
                memory_context::mint_explicit_command_proof(),
            )
            .expect("unique declared provider must resolve");
        assert!(matches!(
            status.durable,
            memory_context::DurableStatus::Active { .. }
        ));
        let bound = runtime.memory_bound_providers_for_test();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].as_str(), ADDRESS, "exact runtime address recorded");

        // Explicit exact id resolves to the same identity.
        let (runtime, _ext) =
            memory_runtime_with_providers(&[("axel-memory-manager", "project-memory")]).await;
        runtime
            .memory_context_enable_resolved(
                memory_context::MemoryContextMode::RecallEachPrompt,
                memory_context::mint_explicit_command_proof(),
                Some("project-memory"),
            )
            .expect("explicit exact id must resolve");
        assert_eq!(runtime.memory_bound_providers_for_test()[0].as_str(), ADDRESS);
    }

    /// Task A6: two installed extensions declaring overlapping context
    /// providers make both the id-less request AND a request for the
    /// overlapping id ambiguous — fail closed, nothing granted.
    #[tokio::test]
    async fn memory_enable_ambiguous_overlapping_declarations_fails_closed() {
        let (runtime, _ext) = memory_runtime_with_providers(&[
            ("mem-a", "project-memory"),
            ("mem-b", "project-memory"),
        ])
        .await;
        let err = runtime
            .memory_context_enable(
                memory_context::MemoryContextMode::CaptureAndRecall,
                memory_context::mint_explicit_command_proof(),
            )
            .expect_err("overlapping declarations must fail closed");
        assert_eq!(err, memory_context::MemoryContextError::ProviderAmbiguous);
        assert_memory_off_no_lease(&runtime);

        // The overlapping exact id cannot disambiguate two owners either.
        let err = runtime
            .memory_context_enable_resolved(
                memory_context::MemoryContextMode::CaptureOnly,
                memory_context::mint_explicit_command_proof(),
                Some("project-memory"),
            )
            .expect_err("same id declared by two plugins is ambiguous");
        assert_eq!(err, memory_context::MemoryContextError::ProviderAmbiguous);
        assert_memory_off_no_lease(&runtime);
    }

    /// Task A6 revocation wiring: disable revokes the granted lease from
    /// `SessionMemoryState` AND defensively calls
    /// `ExtensionRuntimeManager::revoke_plugin_lease` for the bound
    /// plugin/session pair — an idempotent no-op (no panic) when nothing
    /// was ever spawned.
    #[tokio::test]
    async fn memory_disable_revokes_runtime_lease_without_spawn_no_panic() {
        let (runtime, ext) =
            memory_runtime_with_providers(&[("axel-memory-manager", "project-memory")]).await;
        runtime
            .memory_context_enable(
                memory_context::MemoryContextMode::CaptureAndRecall,
                memory_context::mint_explicit_command_proof(),
            )
            .expect("enable succeeds");
        assert_eq!(ext.lease_count(), 0, "granting a memory lease spawns nothing");

        let status = runtime.memory_context_disable();
        assert_eq!(status.durable, memory_context::DurableStatus::Off);
        assert_memory_off_no_lease(&runtime);
        assert_eq!(ext.lease_count(), 0, "revocation of a never-spawned lease is a no-op");

        // Idempotent: disabling again is safe and stays Off.
        let status = runtime.memory_context_disable();
        assert_eq!(status.durable, memory_context::DurableStatus::Off);
        assert_eq!(ext.lease_count(), 0);
    }

    /// Task A6 session-end plumbing: dropping the LAST runtime owner runs
    /// the shared `ExtensionSessionEndGuard` (terminate_session — the SAME
    /// reap mechanism deferred tools/handlers use), and a fresh
    /// construction against the same extension runtime starts Off with no
    /// lease of any kind.
    #[tokio::test]
    async fn memory_session_end_guard_drop_fresh_construction_is_off_no_lease() {
        let (runtime, ext) =
            memory_runtime_with_providers(&[("axel-memory-manager", "project-memory")]).await;
        runtime
            .memory_context_enable(
                memory_context::MemoryContextMode::CaptureAndRecall,
                memory_context::mint_explicit_command_proof(),
            )
            .expect("enable succeeds");
        // True session end: the last owner drop fires the guard.
        drop(runtime);
        assert_eq!(
            ext.lease_count(),
            0,
            "session end must leave no runtime lease (never-spawned reap is a no-op)"
        );

        // A fresh construction (what any new session does) is Off/no-lease.
        let mut fresh = Runtime::new_headless();
        fresh.install_extension_runtime(std::sync::Arc::clone(&ext));
        assert_memory_off_no_lease(&fresh);
        assert_eq!(ext.lease_count(), 0);
    }
}
