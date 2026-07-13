//! Engine setup — boot sequence shared by TUI and headless modes.
//!
//! Extracts the initialization logic that was previously inlined in
//! chatui/mod.rs so both renderers can use the same boot path.

use crate::skills::keybinds::KeybindRegistry;
use crate::skills::registry::CommandRegistry;
use crate::{latest_session, resolve_session, Result, Runtime, Session};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Options for engine boot.
pub struct EngineOpts {
    pub continue_session: Option<Option<String>>,
    pub system: Option<String>,
    pub prompt_manifest: Option<std::path::PathBuf>,
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
    if let Some(ref prof) = opts.profile {
        crate::config::set_profile(Some(prof.clone()));
    }

    // Capture the WorkerGuard from the file appender. tracing-appender's
    // non-blocking writer uses a background flush thread; the guard is
    // an RAII handle that stops that thread on drop. The previous code
    // dropped it at the end of boot() with a comment claiming "this is
    // fine because tracing-subscriber uses a global subscriber" — which
    // is true for the subscriber, but NOT for the file appender's
    // background thread. With the guard dropped, log lines emitted after
    // boot() returned (Extension loaded, hook traces, etc.) could be
    // silently lost. We hand the guard down through EngineBoot so the
    // renderer (TUI / chat / server) keeps it alive for its lifetime.
    let log_guard = crate::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load config and apply
    let config = crate::config::load_config();
    runtime.apply_config(&config);

    // Validate and compile an opted-in manifest before any session/network work.
    let legacy_prompt = crate::config::resolve_system_prompt(opts.system.as_deref());
    if let Some(path) = &opts.prompt_manifest {
        let raw = std::fs::read_to_string(path)
            .map_err(|_| crate::RuntimeError::Config("prompt manifest is unavailable".into()))?;
        let manifest = agent_core::prompt::PromptManifest::parse(&raw)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        let registry = manifest
            .registry(path.parent())
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        let model = agent_core::prompt::QualifiedModelId::parse(runtime.model())
            .map_err(|e| crate::RuntimeError::Config(format!("invalid foreground model: {e}")))?;
        let context = agent_core::prompt::SelectionContext::new(model.clone(), None)
            .map_err(|e| crate::RuntimeError::Config(e.to_string()))?;
        if let Some(policy) = manifest
            .delegation_policy(model)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?
        {
            runtime.install_orchestration(Arc::new(
                crate::orchestration::OrchestrationRuntime::new(policy),
            ));
        }
        let user = opts
            .system
            .as_ref()
            .map(|_| {
                agent_core::prompt::resolved_system_prompt_as_user_module(legacy_prompt.clone())
            })
            .transpose()
            .map_err(|e| crate::RuntimeError::Config(e.to_string()))?;
        let stack = agent_core::prompt::compile_prompt_stack(&manifest, &registry, &context, user)
            .map_err(|e| crate::RuntimeError::Config(format!("invalid prompt manifest: {e}")))?;
        runtime.set_system_prompt(stack.composed().to_owned());
    } else {
        runtime.set_system_prompt(legacy_prompt);
    }

    // Discover plugins/skills, build command registry, register load_skill tool.
    let tools_shared = runtime.tools_shared();
    let (registry, keybind_registry) = crate::skills::register(&tools_shared, &config).await;

    // Set up lazy MCP loading (if configured in ~/.synaps-cli/mcp.json)
    let mcp_server_count = crate::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;

    let system_prompt_path = crate::config::resolve_read_path("system.md");

    // Session: continue existing or create new
    let sb = resolve_or_create_session(&mut runtime, &opts.continue_session)?;

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
    let session_socket_path = crate::events::registry::socket_path_for_session(&sb.session.id);
    let socket_task = crate::events::socket::listen_session_socket(
        session_socket_path.clone(),
        runtime.event_queue().clone(),
        socket_shutdown.clone(),
    );
    let session_registration = crate::events::registry::SessionRegistration {
        session_id: sb.session.id.clone(),
        name: sb.session.name.clone(),
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

    // Extension manager
    let ext_mgr = crate::extensions::manager::ExtensionManager::new_with_tools(
        Arc::clone(runtime.hook_bus()),
        runtime.tools_shared(),
    );
    let ext_manager = Arc::new(RwLock::new(ext_mgr));
    crate::runtime::openai::set_extension_manager_for_routing(Arc::clone(&ext_manager));

    // Session start hook
    {
        let mut index_record =
            crate::core::session_index::SessionIndexRecord::start(&sb.session.id);
        index_record.model = Some(sb.session.model.clone());
        index_record.profile = crate::core::config::get_profile();
        index_record.cwd = std::env::current_dir().ok();
        if let Err(err) = crate::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session start index record: {}", err);
        }

        let hook_event =
            crate::extensions::hooks::events::HookEvent::on_session_start(&sb.session.id);
        let _ = runtime.hook_bus().emit(&hook_event).await;
    }

    if mcp_server_count > 0 {
        tracing::info!(
            "{} MCP servers available (use connect_mcp_server to activate)",
            mcp_server_count
        );
    }

    let session_id = sb.session.id.clone();

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
        background: BackgroundTasks {
            watcher_shutdown,
            watcher_task,
            socket_shutdown,
            socket_task,
            session_socket_path,
            session_id,
            log_guard,
        },
    })
}

/// Resolve a session to continue, or create a new one.
/// Result of session resolution.
struct SessionBootResult {
    session: Session,
    api_messages: Vec<crate::SharedMessage>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    session_cost: f64,
    abort_context: Option<String>,
    continued: bool,
    continue_info: Option<ContinueInfo>,
}

fn resolve_or_create_session(
    runtime: &mut Runtime,
    continue_session: &Option<Option<String>>,
) -> Result<SessionBootResult> {
    match continue_session {
        Some(ref maybe_id) => {
            let session = match maybe_id {
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
            if let Some(ref sp) = session.system_prompt {
                runtime.set_system_prompt(sp.clone());
            }

            let continue_info = maybe_id.as_ref().map(|q| {
                let resolved_via = if *q != session.id {
                    if crate::chain::load_chain(q).is_ok() {
                        Some("chain".to_string())
                    } else if crate::session::find_session_by_name(q).is_ok() {
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
