//! Engine setup — boot sequence shared by TUI and headless modes.
//!
//! Extracts the initialization logic that was previously inlined in
//! chatui/mod.rs so both renderers can use the same boot path.

use crate::{Runtime, Result, Session, latest_session, resolve_session};
use crate::skills::registry::CommandRegistry;
use crate::skills::keybinds::KeybindRegistry;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Options for engine boot.
pub struct EngineOpts {
    pub continue_session: Option<Option<String>>,
    pub system: Option<String>,
    pub profile: Option<String>,
    pub no_extensions: bool,
}

/// Result of the boot sequence — everything a renderer needs to start.
pub struct EngineBoot {
    pub runtime: Runtime,
    pub config: crate::SynapsConfig,
    pub session: Session,
    pub api_messages: Vec<serde_json::Value>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub session_cost: f64,
    pub abort_context: Option<String>,
    pub continued: bool,
    pub continue_info: Option<ContinueInfo>,
    pub registry: Arc<CommandRegistry>,
    pub keybind_registry: Arc<std::sync::RwLock<KeybindRegistry>>,
    pub mcp_server_count: usize,
    pub system_prompt_path: std::path::PathBuf,
    pub ext_manager: Arc<RwLock<crate::extensions::manager::ExtensionManager>>,
    pub watcher_shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub watcher_task: tokio::task::JoinHandle<()>,
    pub socket_shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub socket_task: tokio::task::JoinHandle<()>,
    pub session_socket_path: String,
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

    let _log_guard = crate::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load config and apply
    let config = crate::config::load_config();
    runtime.apply_config(&config);

    // Load system prompt
    let system_prompt = crate::config::resolve_system_prompt(opts.system.as_deref());
    runtime.set_system_prompt(system_prompt);

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
    let abort_tasks = |ws: &Arc<std::sync::atomic::AtomicBool>, wt: &tokio::task::JoinHandle<()>| {
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
        return Err(crate::error::RuntimeError::Tool(format!("Failed to register session: {}", e)));
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
        let mut index_record = crate::core::session_index::SessionIndexRecord::start(&sb.session.id);
        index_record.model = Some(sb.session.model.clone());
        index_record.profile = crate::core::config::get_profile();
        index_record.cwd = std::env::current_dir().ok();
        if let Err(err) = crate::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session start index record: {}", err);
        }

        let hook_event = crate::extensions::hooks::events::HookEvent::on_session_start(&sb.session.id);
        let _ = runtime.hook_bus().emit(&hook_event).await;
    }

    if mcp_server_count > 0 {
        tracing::info!("{} MCP servers available (use connect_mcp_server to activate)", mcp_server_count);
    }

    Ok(EngineBoot {
        runtime,
        config,
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
        watcher_shutdown,
        watcher_task,
        socket_shutdown,
        socket_task,
        session_socket_path,
    })
}

/// Resolve a session to continue, or create a new one.
/// Result of session resolution.
struct SessionBootResult {
    session: Session,
    api_messages: Vec<serde_json::Value>,
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
                    crate::error::RuntimeError::Tool(format!("Failed to load session '{}': {}", id, e))
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
            let session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
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
