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
    let (session, api_messages, total_input_tokens, total_output_tokens, session_cost, abort_context, continued, continue_info) =
        resolve_or_create_session(&mut runtime, &opts.continue_session)?;

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
        tracing::warn!("Failed to register session: {}", e);
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
        let mut index_record = crate::core::session_index::SessionIndexRecord::start(&session.id);
        index_record.model = Some(session.model.clone());
        index_record.profile = crate::core::config::get_profile();
        index_record.cwd = std::env::current_dir().ok();
        if let Err(err) = crate::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session start index record: {}", err);
        }

        let hook_event = crate::extensions::hooks::events::HookEvent::on_session_start(&session.id);
        let _ = runtime.hook_bus().emit(&hook_event).await;
    }

    if mcp_server_count > 0 {
        tracing::info!("{} MCP servers available (use connect_mcp_server to activate)", mcp_server_count);
    }

    Ok(EngineBoot {
        runtime,
        config,
        session,
        api_messages,
        total_input_tokens,
        total_output_tokens,
        session_cost,
        abort_context,
        continued,
        continue_info,
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
fn resolve_or_create_session(
    runtime: &mut Runtime,
    continue_session: &Option<Option<String>>,
) -> Result<(Session, Vec<serde_json::Value>, u64, u64, f64, Option<String>, bool, Option<ContinueInfo>)> {
    match continue_session {
        Some(ref maybe_id) => {
            let session = match maybe_id {
                Some(ref id) => resolve_session(id).unwrap_or_else(|e| {
                    eprintln!("Failed to load session '{}': {}", id, e);
                    std::process::exit(1);
                }),
                None => latest_session().unwrap_or_else(|e| {
                    eprintln!("No sessions to continue: {}", e);
                    std::process::exit(1);
                }),
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

            Ok((
                session.clone(),
                session.api_messages.clone(),
                session.total_input_tokens,
                session.total_output_tokens,
                session.session_cost,
                session.abort_context.clone(),
                true,
                continue_info,
            ))
        }
        None => {
            let session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
            Ok((session, Vec::new(), 0, 0, 0.0, None, false, None))
        }
    }
}
