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
    pub resolved_via: Option<String>, // "chain", "name", "compacted", or None
    pub query: String,
    /// F24: set when the requested session was compacted into a successor.
    pub compaction_notice: Option<String>,
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
    // merge(112): seed continuation state from the resolved session so a
    // --continue resume picks up the context-continuation high-water mark.
    runtime.reset_context_continuation(&sb.session.id, &sb.api_messages);

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
        kind: crate::events::registry::REGISTRATION_KIND.to_string(),
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
    pub(crate) continued: bool,
    pub(crate) continue_info: Option<ContinueInfo>,
}

/// The session lock for a `boot()` host that drives `Runtime` itself (rpc,
/// `synaps server`). Continuing a session that another process (a TUI, the
/// daemon, another rpc) holds is REFUSED: two writers would fork its history
/// and the older one's saves would overwrite the newer's. So is continuing
/// a session already compacted into a successor (its history is closed). A
/// fresh session, or a lock that cannot be taken for another reason (e.g. a
/// read-only sessions dir), proceeds best-effort, as the session actor does.
/// The lock is held for the returned value's lifetime; a holder that
/// continued a session runs crash recovery (`recover_turn_draft`).
pub fn lock_session(
    id: &str,
    continued: bool,
    kind: &str,
) -> Result<Option<agent_core::session_lock::SessionLock>> {
    let dir = agent_core::session_lock::sessions_dir();
    let holder = agent_core::session_lock::LockHolder {
        pid: std::process::id(),
        kind: kind.to_string(),
    };
    match agent_core::session_lock::SessionLock::try_acquire(&dir, id, holder) {
        Ok(lock) => Ok(Some(lock)),
        Err(
            e @ (agent_core::session_lock::SessionLockError::Held { .. }
            | agent_core::session_lock::SessionLockError::CompactedInto { .. }),
        ) if continued => Err(crate::RuntimeError::Session(e.to_string())),
        Err(e) => {
            tracing::warn!(session = %id, "session lock: {e}");
            Ok(None)
        }
    }
}

/// Crash recovery: if `sessions/<id>.turn` exists, the process that last ran
/// this session died with a turn open. Fold the draft into the history
/// (`engine::interrupt::recover_crashed_turn`), persist that, THEN remove
/// the draft.
///
/// Run ONLY by the holder of the session lock, after taking it (actor
/// create, unpark and `/resume`; rpc and `synaps server` after
/// `lock_session`): a draft under a lock held elsewhere belongs to a turn
/// that is running right now. Legacy chat takes no lock and never recovers.
/// Loading itself (`resolve_or_create_session`) leaves the draft alone.
///
/// The draft is removed only once the recovered history is on disk; if that
/// save fails the draft stays, and the next holder retries (recovery is
/// idempotent through its own marker).
pub async fn recover_turn_draft(conv: &mut crate::engine::session::ConversationState) {
    let dir = agent_core::session_lock::sessions_dir();
    let id = conv.session.id.clone();
    let read = {
        let (dir, id) = (dir.clone(), id.clone());
        tokio::task::spawn_blocking(move || {
            agent_core::core::session_draft::read_turn_draft(&dir, &id)
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r)
    };
    let recovered = match read {
        Ok(None) => return,
        Ok(Some(draft)) => {
            crate::engine::interrupt::recover_crashed_turn(&mut conv.api_messages, &draft)
        }
        Err(e) => {
            // Unreadable draft: never block the load. The history stays
            // exactly as saved; the draft is removed below.
            tracing::warn!(session = %id, "unreadable turn draft, removing it: {e}");
            false
        }
    };
    if recovered {
        tracing::warn!(
            session = %id,
            "session was interrupted mid-turn by an unexpected stop; recovered"
        );
        if !conv.save().await {
            tracing::warn!(
                session = %id,
                "could not save the recovered history; keeping the turn draft"
            );
            return;
        }
    } else {
        // The turn had concluded (or the draft was unreadable): the saved
        // history is already right, only the draft's removal was lost.
        tracing::info!(session = %id, "removing a stale turn draft");
    }
    let removed = tokio::task::spawn_blocking(move || {
        agent_core::core::session_draft::remove_turn_draft(&dir, &id)
    })
    .await;
    if let Ok(Err(e)) | Err(e) = removed.map_err(std::io::Error::other) {
        tracing::warn!(session = %conv.session.id, "failed to remove turn draft: {e}");
    }
}

fn resolve_or_create_session(
    runtime: &mut Runtime,
    continue_session: &Option<Option<String>>,
) -> Result<SessionBootResult> {
    match continue_session {
        Some(ref maybe_id) => {
            let raw_session = match maybe_id {
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
            // F24: follow compacted_into forward so --continue <old> lands
            // on the final successor, never on a pre-compaction fork point.
            let resolved = agent_core::session::follow_compaction_chain(raw_session)
                .map_err(|e| crate::error::RuntimeError::Session(e.to_string()))?;
            let compaction_notice = resolved.compaction_notice;
            let mut session = resolved.session;
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

            let continue_info = {
                let (resolved_via, query) = match maybe_id {
                    Some(ref q) => {
                        let via = if *q != session.id {
                            if crate::chain::load_chain(q).is_ok() {
                                Some("chain".to_string())
                            } else if agent_core::session::find_session_by_name(q).is_ok() {
                                Some("name".to_string())
                            } else if compaction_notice.is_some() {
                                Some("compacted".to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        (via, q.clone())
                    }
                    None => {
                        let via = compaction_notice.as_ref().map(|_| "compacted".to_string());
                        (via, session.id.clone())
                    }
                };
                // Always produce ContinueInfo when we have a compaction notice,
                // even for bare --continue (no explicit id).
                if maybe_id.is_some() || compaction_notice.is_some() {
                    Some(ContinueInfo {
                        session_id: session.id.clone(),
                        resolved_via,
                        query,
                        compaction_notice: compaction_notice.clone(),
                    })
                } else {
                    None
                }
            };

            // Sessions saved before the interruption marker existed carry a
            // recap in `abort_context`, meant to be prepended to the next user
            // message. Migrate here, for actor create, unpark and `boot()`
            // (`/resume` migrates in `ConversationState::from_resumed`), and
            // before the continuation seed, so everything downstream sees the
            // migrated history. The recap is dropped; the marker is appended
            // (append-only: the cached prefix is untouched).
            if crate::engine::interrupt::migrate_legacy_abort_context(
                &mut session.api_messages,
                &mut session.abort_context,
            ) {
                tracing::info!(
                    session = %session.id,
                    "migrated a legacy abort-context recap to an interruption marker"
                );
            }
            // A turn draft (`sessions/<id>.turn`) is NOT folded in here:
            // loading runs before (or without) the session lock. The lock
            // holder does it (`recover_turn_draft`).

            Ok(SessionBootResult {
                api_messages: session.api_messages.clone(),
                total_input_tokens: session.total_input_tokens,
                total_output_tokens: session.total_output_tokens,
                session_cost: session.session_cost,
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

    async fn saved_session(first: &str) -> Session {
        let mut session = Session::new("claude-sonnet-4-5", "low", None);
        session.api_messages = vec![std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": first}),
        )];
        session.save().await.unwrap();
        session
    }

    /// Loading runs before (or without) the session lock, so it must never
    /// fold a turn draft in: `rpc --continue` on a session whose turn is
    /// running in another process would otherwise record a crash that never
    /// happened. (Review finding: `boot()` callers never lock.)
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn loading_never_folds_or_touches_the_draft() {
        use agent_core::core::session_draft::{read_turn_draft, write_turn_draft, TurnDraft};
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let session = saved_session("do X").await;
        let draft = TurnDraft {
            base_len: 1,
            partial_text: "partial".into(),
        };
        write_turn_draft(&dir, &session.id, &draft).unwrap();

        let mut runtime = Runtime::new_headless();
        let sb = resolve_or_create_session(&mut runtime, &Some(Some(session.id.clone()))).unwrap();
        assert_eq!(sb.api_messages.len(), 1, "history exactly as saved");
        assert_eq!(
            read_turn_draft(&dir, &session.id).unwrap(),
            Some(draft),
            "the draft is left for the lock holder"
        );
    }

    /// The lock holder folds the draft in, saves, THEN removes the draft.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn the_lock_holder_recovers_saves_then_removes_the_draft() {
        use agent_core::core::session_draft::{read_turn_draft, write_turn_draft, TurnDraft};
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let session = saved_session("do X").await;
        let id = session.id.clone();
        write_turn_draft(
            &dir,
            &id,
            &TurnDraft {
                base_len: 1,
                partial_text: "partial".into(),
            },
        )
        .unwrap();

        let mut conv = crate::engine::session::ConversationState::from_resumed(session);
        recover_turn_draft(&mut conv).await;
        assert_eq!(conv.api_messages.len(), 3, "partial reply + marker");
        let on_disk = Session::load(&id).unwrap();
        assert_eq!(on_disk.api_messages, conv.api_messages, "saved before removal");
        assert_eq!(read_turn_draft(&dir, &id).unwrap(), None, "draft removed");

        // No draft: nothing to do.
        recover_turn_draft(&mut conv).await;
        assert_eq!(conv.api_messages.len(), 3);
    }

    /// An unreadable draft never blocks the load: history untouched, draft
    /// removed.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn an_unreadable_draft_is_removed_and_history_kept() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let session = saved_session("do X").await;
        let path = dir.join(format!("{}.turn", session.id));
        std::fs::write(&path, b"{not json").unwrap();
        let mut conv = crate::engine::session::ConversationState::from_resumed(session);
        recover_turn_draft(&mut conv).await;
        assert_eq!(conv.api_messages.len(), 1);
        assert!(!path.exists());
    }

    /// If the recovered history cannot be saved, the draft stays: removing
    /// it would lose the only record that the turn was cut off.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_failed_recovery_save_keeps_the_draft() {
        use agent_core::core::session_draft::{read_turn_draft, write_turn_draft, TurnDraft};
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = agent_core::session_lock::sessions_dir();
        let session = saved_session("do X").await;
        let id = session.id.clone();
        let draft = TurnDraft {
            base_len: 1,
            partial_text: "partial".into(),
        };
        write_turn_draft(&dir, &id, &draft).unwrap();
        // Make the snapshot unwritable: a non-empty directory where the
        // `<id>.json` file goes (the atomic rename onto it fails).
        let snapshot = dir.join(format!("{id}.json"));
        std::fs::remove_file(&snapshot).unwrap();
        std::fs::create_dir(&snapshot).unwrap();
        std::fs::write(snapshot.join("occupied"), b"x").unwrap();

        let mut conv = crate::engine::session::ConversationState::from_resumed(session);
        recover_turn_draft(&mut conv).await;
        assert_eq!(read_turn_draft(&dir, &id).unwrap(), Some(draft), "draft kept");
    }

    /// A `boot()` host may not continue a session another holder has; a
    /// fresh one is locked (and the lock is released on drop).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn boot_hosts_refuse_to_continue_a_session_held_elsewhere() {
        let _base = crate::test_env::BaseDirGuard::new();
        let held = lock_session("20260930-000000-held", true, "tui").unwrap();
        assert!(held.is_some());
        let err = lock_session("20260930-000000-held", true, "rpc").unwrap_err();
        assert!(err.to_string().contains("20260930-000000-held"), "{err}");
        // Not continuing (a fresh id) never refuses.
        assert!(lock_session("20260930-000000-held", false, "rpc").unwrap().is_none());
        drop(held);
        assert!(lock_session("20260930-000000-held", true, "rpc").unwrap().is_some());

        // A session compacted into a successor is closed: never continued.
        let mut old = Session::new("claude-sonnet-4-5", "low", None);
        old.api_messages = vec![std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": "before compaction"}),
        )];
        old.compacted_into = Some("20260930-000001-next".into());
        old.save().await.unwrap();
        let err = lock_session(&old.id, true, "rpc").unwrap_err();
        assert!(err.to_string().contains("20260930-000001-next"), "{err}");
    }

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

    /// merge(112) DARK test: default config boots with legacy memory and no
    /// context_checkpoint tool — Axel features are invisible until opted in.
    #[test]
    fn default_config_is_dark_no_checkpoint_tool_legacy_binding() {
        let runtime = Runtime::new_headless();
        // Default memory backend must be legacy (no Axel sidecar spawn).
        assert!(
            !runtime.memory_backend_exclusive(),
            "default config must NOT be exclusive (Axel); it must be legacy"
        );
        // The tool registry must not contain context_checkpoint.
        let tools = runtime.tools_shared();
        let registry = tools.blocking_read();
        let has_checkpoint = registry
            .iter_tools_sorted()
            .iter()
            .any(|t| t.name() == "context_checkpoint");
        assert!(
            !has_checkpoint,
            "context_checkpoint must not appear in the default tool set"
        );
    }
}
