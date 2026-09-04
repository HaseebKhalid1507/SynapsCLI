//! Engine setup — boot sequence shared by TUI and headless modes.
//!
//! Extracts the initialization logic that was previously inlined in
//! chatui/mod.rs so both renderers can use the same boot path.

use crate::skills::keybinds::KeybindRegistry;
use crate::skills::registry::CommandRegistry;
use crate::{latest_session, resolve_session, EngineHost, HostOpts, Result, Runtime, Session};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Options for engine boot.
pub struct EngineOpts {
    pub continue_session: Option<Option<String>>,
    pub system: Option<String>,
    pub prompt_manifest: Option<std::path::PathBuf>,
    /// Honoured by the first `boot` in a process only: the `EngineHost` is
    /// built once and later boots reuse it, profile included.
    pub profile: Option<String>,
    pub no_extensions: bool,
}

/// Background tasks spawned during boot. Aborts on drop.
pub struct BackgroundTasks {
    watcher_shutdown: Arc<std::sync::atomic::AtomicBool>,
    watcher_task: tokio::task::JoinHandle<()>,
    socket_shutdown: Arc<std::sync::atomic::AtomicBool>,
    socket_task: tokio::task::JoinHandle<()>,
    #[allow(dead_code)] // stored for potential future use (e.g. reconnect)
    session_socket_path: String,
    session_id: String,
    /// Hook bus the session's `on_session_start` injection lives on; cleared
    /// at shutdown so a long-lived process does not accumulate stale keys.
    hook_bus: Arc<crate::extensions::hooks::HookBus>,
    /// File-appender flush guard. Holding this for the lifetime of the
    /// renderer keeps the non-blocking log writer's background thread
    /// alive — without it, log lines emitted after `boot()` returns can
    /// be silently dropped before they reach disk. Dropped last when
    /// BackgroundTasks drops.
    #[allow(dead_code)]
    log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl BackgroundTasks {
    /// Signal all tasks to stop and unregister the session.
    pub fn shutdown(&self) {
        self.watcher_shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.socket_shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        crate::events::registry::unregister_session(&self.session_id);
        // Cleanup only — fail-soft when no tokio runtime is current.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let hook_bus = Arc::clone(&self.hook_bus);
            let session_id = self.session_id.clone();
            handle.spawn(async move {
                hook_bus.clear_session_injection(&session_id).await;
            });
        }
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        self.watcher_shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.socket_shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.watcher_task.abort();
        self.socket_task.abort();
    }
}

/// Result of the boot sequence — everything a renderer needs to start.
pub struct EngineBoot {
    pub runtime: Runtime,
    pub config: crate::SynapsConfig,
    /// Echo of EngineOpts.no_extensions — callers gate extension discovery
    /// on this so the flag has one source of truth.
    pub no_extensions: bool,
    pub session: Session,
    pub api_messages: Vec<crate::SharedMessage>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub session_cost: f64,
    pub abort_context: Option<String>,
    pub continued: bool,
    pub continue_info: Option<ContinueInfo>,
    pub registry: Arc<CommandRegistry>,
    /// Keybind registry. Uses std::sync::RwLock (not tokio) because keybind
    /// lookups are synchronous, fast, and called from input handling code
    /// that cannot await. This is safe as long as the lock is never held
    /// across an await point.
    pub keybind_registry: Arc<std::sync::RwLock<KeybindRegistry>>,
    pub mcp_server_count: usize,
    pub system_prompt_path: std::path::PathBuf,
    pub ext_manager: Arc<RwLock<crate::extensions::manager::ExtensionManager>>,
    /// Background tasks — inbox watcher, socket listener. Aborts on drop.
    pub background: BackgroundTasks,
}

/// Info about how a continued session was resolved.
pub struct ContinueInfo {
    pub session_id: String,
    pub resolved_via: Option<String>, // "chain", "name", or None
    pub query: String,
}

/// Run the full engine boot sequence:
/// config → system prompt → skills → MCP → session → sockets → extensions
pub async fn boot(opts: EngineOpts) -> Result<EngineBoot> {
    // Process-global parts (profile, logging, HTTP client, registry, skills,
    // MCP, extension manager) are built ONCE per process by `EngineHost`
    // and reused by every later boot in the same process. The log-appender
    // guard lives on the host — process lifetime ≥ renderer lifetime — so
    // log lines emitted after boot() returns are never silently dropped.
    let host = EngineHost::boot_and_install(HostOpts {
        profile: opts.profile.clone(),
        no_extensions: opts.no_extensions,
    })
    .await?;
    // `apply_config` is applied inside `foreground_runtime()` — before
    // session resolution, same relative order as before.
    let mut runtime = host.foreground_runtime().await?;
    let config: crate::SynapsConfig = (**host.config()).clone();

    let sb = resolve_session_and_prompt(
        &mut runtime,
        &opts.continue_session,
        opts.system.as_deref(),
        opts.prompt_manifest.as_deref(),
    )?;

    // Skills, command registry, MCP setup and the MCP lease manager are
    // host-owned (see `EngineHost::boot`); the runtime already holds them.
    let registry = Arc::clone(host.command_registry());
    let keybind_registry = Arc::clone(host.keybind_registry());
    let mcp_server_count = host.mcp_server_count();

    let system_prompt_path = crate::config::resolve_read_path("system.md");

    // Session was resolved before policy compilation so its model is the immutable
    // foreground identity used by worker inheritance and authorization.

    let background = spawn_session_background(&runtime, &sb.session)?;

    finish_session_setup(&mut runtime, &config, &sb.session, None, IndexRecord::Start);

    // Extension manager: host-owned.
    let ext_manager = Arc::clone(host.ext_manager());

    if mcp_server_count > 0 {
        tracing::info!(
            "{} MCP servers available (use connect_mcp_server to activate)",
            mcp_server_count
        );
    }

    Ok(EngineBoot {
        runtime,
        config,
        no_extensions: opts.no_extensions,
        session: sb.session,
        api_messages: sb.api_messages,
        total_input_tokens: sb.total_input_tokens,
        total_output_tokens: sb.total_output_tokens,
        session_cost: sb.session_cost,
        abort_context: sb.abort_context,
        continued: sb.continued,
        continue_info: sb.continue_info,
        registry,
        keybind_registry,
        mcp_server_count,
        system_prompt_path,
        ext_manager,
        background,
    })
}

/// Session resolution + prompt/orchestration install (code motion from
/// `boot()`; called per session by `SessionActor::create` too).
pub(crate) fn resolve_session_and_prompt(
    runtime: &mut Runtime,
    continue_session: &Option<Option<String>>,
    system: Option<&str>,
    prompt_manifest: Option<&std::path::Path>,
) -> Result<SessionBootResult> {
    // Resolve the final foreground route before compiling immutable delegation
    // policy. Continuing a session may replace the configured model.
    let sb = resolve_or_create_session(runtime, continue_session)?;
    runtime.set_session_id(Some(sb.session.id.clone()));

    // Validate and compile an opted-in manifest before any session/network work.
    let legacy_prompt = crate::config::resolve_system_prompt(system);
    if let Some(path) = prompt_manifest {
        let raw = std::fs::read_to_string(path)
            .map_err(|_| crate::RuntimeError::Config("prompt manifest is unavailable".into()))?;
        let manifest = agent_core::prompt::PromptManifest::parse(&raw)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        let registry = manifest
            .registry(path.parent())
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        let model = crate::orchestration::canonical_foreground_identity(runtime.model())
            .map_err(|e| crate::RuntimeError::Config(format!("invalid foreground model: {e}")))?;
        let context = agent_core::prompt::SelectionContext::new(model.clone(), None)
            .map_err(|e| crate::RuntimeError::Config(e.to_string()))?;
        let catalog = crate::orchestration::OrchestrationRuntime::trusted_catalog(
            &model,
            manifest.delegation_catalog_candidates(),
        )
        .map_err(|error| crate::RuntimeError::Config(error.into()))?;
        let delegation_policy = manifest
            .delegation_policy(model.clone(), &catalog)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        let delegation_policy_digest = delegation_policy.as_ref().map(|policy| policy.digest());
        if let Some(policy) = delegation_policy {
            runtime.install_orchestration(Arc::new(
                crate::orchestration::OrchestrationRuntime::new(policy),
            ));
        } else {
            runtime.install_orchestration(Arc::new(
                crate::orchestration::OrchestrationRuntime::baseline(model.clone(), 8, 64)
                    .map_err(|error| crate::RuntimeError::Config(error.into()))?,
            ));
        }
        let user = system
            .map(|_| {
                agent_core::prompt::resolved_system_prompt_as_user_module(legacy_prompt.clone())
            })
            .transpose()
            .map_err(|e| crate::RuntimeError::Config(e.to_string()))?;
        let stack =
            agent_core::prompt::compile_prompt_stack(&manifest, &registry, &context, user.clone())
                .map_err(|e| {
                    crate::RuntimeError::Config(format!("invalid prompt manifest: {e}"))
                })?;
        runtime
            .apply_prompt_stack(stack)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        runtime.retain_prompt_reload_source(
            path.to_path_buf(),
            context,
            user,
            delegation_policy_digest,
        );
    } else {
        let foreground = crate::orchestration::canonical_foreground_identity(runtime.model())
            .map_err(|e| crate::RuntimeError::Config(format!("invalid foreground model: {e}")))?;
        runtime.install_orchestration(Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                .map_err(|error| crate::RuntimeError::Config(error.into()))?,
        ));
        runtime.set_system_prompt(legacy_prompt);
    }

    Ok(sb)
}

/// Inbox watcher + per-session UDS listener + registry registration (code
/// motion from `boot()`). Fails loudly when registration fails.
pub(crate) fn spawn_session_background(
    runtime: &Runtime,
    session: &Session,
) -> Result<BackgroundTasks> {
    // Start inbox watcher
    let watcher_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_task = {
        let inbox_dir = crate::config::base_dir().join("inbox");
        let event_queue = runtime.event_queue().clone();
        let shutdown = watcher_shutdown.clone();
        tokio::spawn(async move {
            crate::events::watch_inbox(inbox_dir, event_queue, shutdown).await;
        })
    };

    // Helper: abort background tasks on error
    let abort_tasks = |ws: &Arc<std::sync::atomic::AtomicBool>,
                       wt: &tokio::task::JoinHandle<()>| {
        ws.store(true, std::sync::atomic::Ordering::Relaxed);
        wt.abort();
    };

    // Start per-session Unix socket listener + register in session registry
    let socket_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session_socket_path = crate::events::registry::socket_path_for_session(&session.id);
    let socket_task = crate::events::socket::listen_session_socket(
        session_socket_path.clone(),
        runtime.event_queue().clone(),
        socket_shutdown.clone(),
    );
    let session_registration = crate::events::registry::SessionRegistration {
        session_id: session.id.clone(),
        name: session.name.clone(),
        socket_path: session_socket_path.clone(),
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
    };
    if let Err(e) = crate::events::registry::register_session(&session_registration) {
        abort_tasks(&watcher_shutdown, &watcher_task);
        socket_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        socket_task.abort();
        // Fail loudly: returning Ok with already-aborted handles silently
        // poisoned downstream — server inherited dead watcher/socket tasks
        // and a session that wasn't in the registry, so other tools couldn't
        // see it. Better to fail boot than start in a broken state.
        return Err(crate::core::error::RuntimeError::Session(format!(
            "failed to register session {}: {}",
            session_registration.session_id, e
        )));
    }

    Ok(BackgroundTasks {
        watcher_shutdown,
        watcher_task,
        socket_shutdown,
        socket_task,
        session_socket_path,
        session_id: session.id.clone(),
        hook_bus: Arc::clone(runtime.hook_bus()),
        // The appender guard lives on the `EngineHost` now.
        log_guard: None,
    })
}

/// Whether `finish_session_setup` appends the session START index record.
/// `Skip` on unpark (B3) / reload rehydrate (C3): the session already has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexRecord {
    Start,
    Skip,
}

/// Foreground turn budget + session start index record (code motion from
/// `boot()`). `cwd` = the session's configured cwd (`None` → process cwd).
pub(crate) fn finish_session_setup(
    runtime: &mut Runtime,
    config: &crate::SynapsConfig,
    session: &Session,
    cwd: Option<std::path::PathBuf>,
    index_record: IndexRecord,
) {
    // Task 23: the engine's interactive session runs under the FOREGROUND
    // turn budget with typed per-role config overrides applied.
    runtime.set_turn_budget(crate::runtime::budget::TurnBudget::from_config(
        crate::runtime::budget::TurnRole::Foreground,
        &config.turn_budgets,
    ));

    // Session start index record.
    //
    // The `on_session_start` HOOK is deliberately NOT emitted here. Extensions
    // are loaded by the host after boot returns (see
    // `extensions::loader::spawn_discover_and_load`), so emitting at this
    // point delivered the event to an empty bus in every host — the hook had
    // never once reached an extension. It is now emitted by the loader, after
    // subscribers exist.
    if index_record == IndexRecord::Start {
        let mut index_record =
            crate::core::session_index::SessionIndexRecord::start(&session.id);
        index_record.model = Some(session.model.clone());
        index_record.profile = crate::core::config::get_profile();
        index_record.cwd = cwd.or_else(|| std::env::current_dir().ok());
        if let Err(err) = crate::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session start index record: {}", err);
        }
    }

}

/// Result of session resolution.
pub(crate) struct SessionBootResult {
    pub(crate) session: Session,
    pub(crate) api_messages: Vec<crate::SharedMessage>,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) session_cost: f64,
    pub(crate) abort_context: Option<String>,
    pub(crate) continued: bool,
    pub(crate) continue_info: Option<ContinueInfo>,
}

fn resolve_or_create_session(
    runtime: &mut Runtime,
    continue_session: &Option<Option<String>>,
) -> Result<SessionBootResult> {
    match continue_session {
        Some(ref maybe_id) => {
            let mut session = match maybe_id {
                Some(ref id) => resolve_session(id).map_err(|e| {
                    crate::error::RuntimeError::Tool(format!(
                        "Failed to load session '{}': {}",
                        id, e
                    ))
                })?,
                None => latest_session().map_err(|e| {
                    crate::error::RuntimeError::Tool(format!("No sessions to continue: {}", e))
                })?,
            };
            runtime.set_model(session.model.clone());
            // Restore the session's named reasoning level so max/ultra/off
            // and custom budgets survive --continue — then clamp against the
            // model so old grok+xhigh session files don't resume into a
            // permanently failing state.
            if let Some(clamp) = runtime.restore_session_reasoning(&session.thinking_level) {
                tracing::warn!(
                    from = %clamp.from,
                    to = %clamp.to,
                    model = %session.model,
                    "saved session thinking level not supported by model; clamped"
                );
                session.thinking_level = runtime.thinking_level().to_string();
            }
            if let Some(ref sp) = session.system_prompt {
                runtime.set_system_prompt(sp.clone());
            }

            let continue_info = maybe_id.as_ref().map(|q| {
                let resolved_via = if *q != session.id {
                    if crate::chain::load_chain(q).is_ok() {
                        Some("chain".to_string())
                    } else if agent_core::session::find_session_by_name(q).is_ok() {
                        Some("name".to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };
                ContinueInfo {
                    session_id: session.id.clone(),
                    resolved_via,
                    query: q.clone(),
                }
            });

            Ok(SessionBootResult {
                api_messages: session.api_messages.clone(),
                total_input_tokens: session.total_input_tokens,
                total_output_tokens: session.total_output_tokens,
                session_cost: session.session_cost,
                abort_context: session.abort_context.clone(),
                continued: true,
                continue_info,
                session,
            })
        }
        None => {
            let session = Session::new(
                runtime.model(),
                runtime.thinking_level(),
                runtime.system_prompt(),
            );
            Ok(SessionBootResult {
                session,
                api_messages: Vec::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                session_cost: 0.0,
                abort_context: None,
                continued: false,
                continue_info: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::reasoning::ReasoningLevel;

    /// B1: --continue path must restore thinking_level from the saved session.
    /// Simulates what resolve_or_create_session does when a session is continued.
    #[test]
    fn continue_path_restores_thinking_level_from_session() {
        let mut runtime = Runtime::new_headless();

        // Simulate what resolve_or_create_session does on --continue.
        let thinking_level_str = "ultra";
        if let Some(level) = ReasoningLevel::parse(thinking_level_str) {
            runtime.set_reasoning_level_explicit(level);
        }

        assert_eq!(
            runtime.reasoning_level(),
            ReasoningLevel::Ultra,
            "thinking level must be restored from session on --continue"
        );
        assert!(
            runtime.is_reasoning_explicit(),
            "restored thinking level must be marked explicit so set_model won't overwrite it"
        );
    }

    #[test]
    fn continue_path_restores_max_level() {
        let mut runtime = Runtime::new_headless();
        let thinking_level_str = "max";
        if let Some(level) = ReasoningLevel::parse(thinking_level_str) {
            runtime.set_reasoning_level_explicit(level);
        }
        assert_eq!(runtime.reasoning_level(), ReasoningLevel::Max);
    }

    #[test]
    fn continue_path_restores_off_level() {
        let mut runtime = Runtime::new_headless();
        let thinking_level_str = "off";
        if let Some(level) = ReasoningLevel::parse(thinking_level_str) {
            runtime.set_reasoning_level_explicit(level);
        }
        assert_eq!(runtime.reasoning_level(), ReasoningLevel::Off);
    }
}
