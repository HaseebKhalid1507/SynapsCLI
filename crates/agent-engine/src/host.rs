//! Process-global engine host: the things exactly one of which should exist
//! per process regardless of how many sessions the process serves.
//!
//! `engine::setup::boot` used to construct all of this inside the session's
//! `Runtime`. It now builds (or reuses) the [`EngineHost`] and borrows the
//! process-global parts from it; subagent spawns borrow the same parts
//! instead of rebuilding an HTTP client + registry + token cache per spawn.

use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

use crate::extensions::hooks::HookBus;
use crate::runtime::RuntimeParts;
use crate::session::{SessionActor, SessionConfig, SessionHandle, SessionId, SessionMeta};
use crate::tools::catalog::CatalogGeneration;
use crate::{Result, Runtime, ToolRegistry};

/// Everything a `Runtime` borrows from its host rather than constructing.
/// Cheap to clone (all `Arc`/handle types).
#[derive(Clone)]
pub struct HostParts {
    pub client: reqwest::Client,
    /// THE shared catalog.
    pub tools: Arc<RwLock<ToolRegistry>>,
    pub hook_bus: Arc<HookBus>,
    /// Resolved once from config.
    pub credential_source: crate::auth::CredentialSource,
    /// ONE cache per process.
    pub token_cache: crate::auth::TokenCache,
    pub mcp_runtime: Option<Arc<crate::mcp::McpRuntimeManager>>,
    pub extension_runtime: Option<Arc<crate::extensions::lease::ExtensionRuntimeManager>>,
    pub capture_dir: std::path::PathBuf,
    pub progressive_tool_disclosure: bool,
}

pub struct HostOpts {
    /// Applied via `config::set_profile` by the FIRST boot in a process
    /// only; a later `boot()` with a different profile is not honoured
    /// (the installed host wins — see [`EngineHost::boot_and_install`]).
    pub profile: Option<String>,
    /// Echoed to callers (`EngineBoot::no_extensions`) — extension discovery
    /// is gated by the renderer after boot, not by the host. Not read here.
    pub no_extensions: bool,
}

pub struct EngineHost {
    parts: HostParts,
    config: arc_swap::ArcSwap<crate::SynapsConfig>,
    command_registry: Arc<crate::skills::registry::CommandRegistry>,
    keybind_registry: Arc<std::sync::RwLock<crate::skills::keybinds::KeybindRegistry>>,
    ext_manager: Arc<RwLock<crate::extensions::manager::ExtensionManager>>,
    mcp_server_count: usize,
    /// Worker (subagent) registry template, cached by shared-catalog
    /// generation. `worker_runtime()` clones this instead of rebuilding
    /// `without_subagent_with_extensions` + `rebuild_schema()` per spawn.
    worker_registry: std::sync::Mutex<Option<(CatalogGeneration, ToolRegistry)>>,
    /// File-appender flush guard. Lives for the process (≥ any renderer), so
    /// log lines emitted after `boot()` returns can never be dropped by an
    /// early guard drop (the old `setup::boot` bug). Rust runs no static
    /// destructors, so it is released explicitly by [`Self::flush_logs`] at
    /// process exit — otherwise the tail of the log dies with the writer
    /// thread.
    log_guard: std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>,
    /// Live sessions (Phase 2): id → handle. The map holds one handle per
    /// actor so the actor outlives any single client; the actor task removes
    /// itself here when it finishes.
    sessions: std::sync::Mutex<std::collections::HashMap<SessionId, SessionHandle>>,
    /// Serialises `create_session`'s resolve-then-insert so two concurrent
    /// `--continue X` cannot both miss the live check and build two actors
    /// on one session id.
    create_lock: tokio::sync::Mutex<()>,
    /// C2: flips to `true` when extension discovery on `ext_manager` has
    /// finished (the loader sets it; `extensions_ready()` awaits it). The
    /// paired flag records that a loader was DISPATCHED, so a session
    /// created between `spawn_discover_and_load` and its first lock
    /// acquisition still waits instead of racing the walk.
    extensions_ready: tokio::sync::watch::Sender<bool>,
    extensions_loading: std::sync::atomic::AtomicBool,
}

static HOST: OnceLock<Arc<EngineHost>> = OnceLock::new();

/// See [`EngineHost::extensions_loading_guard`].
pub struct ExtensionsLoadingGuard {
    host: Arc<EngineHost>,
}

impl Drop for ExtensionsLoadingGuard {
    fn drop(&mut self) {
        self.host.mark_extensions_ready();
    }
}

impl EngineHost {
    /// Every step of the old `setup::boot()` that does NOT mention a session:
    /// profile, logging, HTTP client, registry, config, skills, MCP, extension
    /// manager. Touches NO process static: the OpenAI routing static is
    /// written by [`Self::install`] for the winning host only, and the global
    /// broker is installed in [`Self::foreground_runtime`] via `apply_config`,
    /// exactly where the old boot did it relative to session resolution.
    pub async fn boot(opts: HostOpts) -> Result<Arc<Self>> {
        if let Some(ref prof) = opts.profile {
            crate::config::set_profile(Some(prof.clone()));
        }
        let log_guard = crate::logging::init_logging();

        let client = crate::runtime::build_host_http_client()?;
        let capture_dir = crate::runtime::trace::default_capture_dir();
        let _ = crate::runtime::trace::sweep_expired_captures(&capture_dir);

        let config = crate::config::load_config();
        let tools = Arc::new(RwLock::new(ToolRegistry::new()));
        // Same point the old boot's `apply_config` disabled tools: on the
        // fresh registry, before skills/MCP register anything.
        if !config.disabled_tools.is_empty() {
            tools.write().await.disable(&config.disabled_tools);
        }

        // Discover plugins/skills, build command registry, register load_skill tool.
        let (command_registry, keybind_registry) = crate::skills::register(&tools, &config).await;

        // MCP loading (if configured). Flag-off keeps the legacy connect
        // gateway; progressive disclosure switches to exact descriptor-backed
        // dormant tools with no gateway.
        let mcp_server_count =
            crate::mcp::setup_lazy_mcp(&tools, config.progressive_tool_disclosure).await;
        let mcp_runtime = if config.progressive_tool_disclosure {
            Some(Arc::new(crate::mcp::McpRuntimeManager::new(
                crate::mcp::lease::config_source_from_disk(),
                crate::mcp::lease::DEFAULT_IDLE_MAX,
            )))
        } else {
            None
        };

        let hook_bus = Arc::new(HookBus::new());
        let mut ext_mgr = crate::extensions::manager::ExtensionManager::new_with_tools(
            Arc::clone(&hook_bus),
            Arc::clone(&tools),
        );
        ext_mgr.set_progressive_deferral(config.progressive_tool_disclosure);
        let extension_runtime = if config.progressive_tool_disclosure {
            Some(ext_mgr.extension_runtime())
        } else {
            None
        };
        let ext_manager = Arc::new(RwLock::new(ext_mgr));

        let parts = HostParts {
            client,
            tools,
            hook_bus,
            credential_source: config.auth.credential_source(),
            token_cache: crate::auth::TokenCache::new(),
            mcp_runtime,
            extension_runtime,
            capture_dir,
            progressive_tool_disclosure: config.progressive_tool_disclosure,
        };

        Ok(Arc::new(Self {
            parts,
            config: arc_swap::ArcSwap::from_pointee(config),
            command_registry,
            keybind_registry,
            ext_manager,
            mcp_server_count,
            worker_registry: std::sync::Mutex::new(None),
            log_guard: std::sync::Mutex::new(log_guard),
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            create_lock: tokio::sync::Mutex::new(()),
            extensions_ready: tokio::sync::watch::channel(false).0,
            extensions_loading: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Idempotent, atomic install of the process host. Exactly one host ever
    /// wins the `OnceLock`; the winner's extension manager becomes the
    /// OpenAI routing static in the same step. A different host arriving
    /// later (or losing a race) is returned as `Err(rejected)` and never
    /// replaces the installed one — nor touches any static.
    pub fn install(host: Arc<Self>) -> std::result::Result<(), Arc<Self>> {
        let installed = HOST.get_or_init(|| {
            crate::runtime::openai::set_extension_manager_for_routing(Arc::clone(
                &host.ext_manager,
            ));
            Arc::clone(&host)
        });
        if Arc::ptr_eq(installed, &host) {
            Ok(())
        } else {
            Err(host)
        }
    }

    /// The process host: the installed one, else boot + install. Under a
    /// race the loser is discarded and the installed host returned, so a
    /// caller never ends up on a host that `current()` does not agree with.
    pub async fn boot_and_install(opts: HostOpts) -> Result<Arc<Self>> {
        if let Some(h) = HOST.get() {
            return Ok(Arc::clone(h));
        }
        let h = Self::boot(opts).await?;
        Ok(match Self::install(h) {
            Ok(()) => HOST.get().cloned().expect("just installed"),
            Err(_rejected) => HOST.get().cloned().expect("a winner exists"),
        })
    }

    pub fn current() -> Option<Arc<Self>> {
        HOST.get().cloned()
    }

    /// Flush the non-blocking log writer: drops the appender guard, which
    /// drains the channel and joins the writer thread (≤ 1 s). Idempotent.
    /// Lines logged AFTER this call are dropped, so it belongs at the very
    /// last exit point (`main` return, panic hook), not at session shutdown.
    pub fn flush_logs(&self) {
        let guard = self.log_guard.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(guard);
    }

    /// [`Self::flush_logs`] on the installed host, if any.
    pub fn flush_installed_logs() {
        if let Some(h) = HOST.get() {
            h.flush_logs();
        }
    }

    pub fn parts(&self) -> &HostParts {
        &self.parts
    }

    pub fn config(&self) -> arc_swap::Guard<Arc<crate::SynapsConfig>> {
        self.config.load()
    }

    pub fn ext_manager(&self) -> &Arc<RwLock<crate::extensions::manager::ExtensionManager>> {
        &self.ext_manager
    }

    pub fn command_registry(&self) -> &Arc<crate::skills::registry::CommandRegistry> {
        &self.command_registry
    }

    pub fn keybind_registry(
        &self,
    ) -> &Arc<std::sync::RwLock<crate::skills::keybinds::KeybindRegistry>> {
        &self.keybind_registry
    }

    pub fn mcp_server_count(&self) -> usize {
        self.mcp_server_count
    }

    /// The foreground runtime for an interactive session: shares client,
    /// tools, hook_bus, credential source, token cache, MCP/extension lease
    /// managers. Everything else is fresh (exactly `Runtime::new()`'s values).
    /// `apply_config` runs here — the ONLY place the broker is (re)installed.
    /// `disabled_tools` is NOT re-applied: `boot()` did it once on the fresh
    /// registry, before skills/MCP registered, exactly where the old boot did.
    pub async fn foreground_runtime(&self) -> Result<Runtime> {
        let mut runtime = Runtime::from_parts(RuntimeParts::with_reaper(self.parts.clone()));
        runtime.apply_config_keep_tools(&self.config());
        Ok(runtime)
    }

    /// A worker (subagent) runtime. Shares credential source and token cache
    /// (so NO broker re-install and NO cache eviction) and takes a CLONE of
    /// the cached worker registry template. Does NOT share: the HTTP client
    /// (fresh — see below), hook_bus (fresh, empty — hooks do not fire for
    /// subagent tool calls), session_manager (fresh + reaper),
    /// mcp_runtime/extension_runtime (`None`, as `Runtime::new()`).
    ///
    /// Own client: workers `block_on` a throwaway `current_thread` runtime
    /// and hyper parks each connection's I/O driver on the runtime that
    /// opened it. A pooled connection born on a worker would be handed to
    /// the foreground (LIFO checkout) and die mid-stream when that worker's
    /// runtime dropped. A per-worker pool dies with its runtime, as before.
    pub async fn worker_runtime(&self) -> Result<Runtime> {
        let mut host = self.parts.clone();
        host.client = crate::runtime::build_host_http_client()?;
        host.hook_bus = Arc::new(HookBus::new());
        host.tools = Arc::new(RwLock::new(self.worker_registry().await));
        host.mcp_runtime = None;
        host.extension_runtime = None;
        // `Runtime::new()` value. Workers never ran `apply_config`, so they
        // never had disclosure on; their registry has no discovery/activation
        // tools, so a projected schema would silently drop every extension
        // tool.
        host.progressive_tool_disclosure = false;
        Ok(Runtime::from_parts(RuntimeParts::with_reaper(host)))
    }

    /// Cached-or-rebuilt worker registry: `without_subagent_with_extensions`
    /// is a pure function of the shared registry's tool set, which is a pure
    /// function of its catalog generation, so caching by generation returns
    /// exactly what a rebuild would.
    pub async fn worker_registry(&self) -> ToolRegistry {
        let shared = self.parts.tools.read().await;
        let gen = shared.catalog().generation();
        if let Some((cached_gen, tpl)) = &*self.worker_registry.lock().unwrap() {
            if *cached_gen == gen {
                return tpl.clone();
            }
        }
        let fresh = ToolRegistry::without_subagent_with_extensions(&shared);
        drop(shared);
        *self.worker_registry.lock().unwrap() = Some((gen, fresh.clone()));
        fresh
    }

    /// Re-read config from disk into the host's snapshot. Phase 1 callers
    /// keep `load_config()` where they use it today.
    pub fn reload_config(&self) {
        self.config
            .store(Arc::new(crate::config::load_config()));
    }

    // ── sessions (Phase 2) ────────────────────────────────────────────────

    /// Create a session on this host: builds the `SessionActor` (the one
    /// `Runtime` + `ConversationState`), spawns its task, registers the
    /// handle. The task removes itself from the map when it ends.
    ///
    /// **Attach-if-live:** when `cfg.continue_session` resolves to a session
    /// that is already running on this host (`--continue X` twice,
    /// `Attach::Create` twice, `--continue` latest while latest is live),
    /// the existing handle is returned and NO second actor is built. Two
    /// actors on one id would share the session file, and the second's
    /// `spawn_session_background` would unlink the first's per-session UDS
    /// and overwrite its registry entry. `cfg`'s other fields (cwd, model
    /// override, …) are ignored in that case — the live session wins.
    pub async fn create_session(self: &Arc<Self>, cfg: SessionConfig) -> Result<SessionHandle> {
        let _serial = self.create_lock.lock().await;
        if let Some(live) = self.live_continue_target(&cfg.continue_session) {
            tracing::info!(session = %live.id, "create_session: target is live — attaching");
            return Ok(live);
        }
        let (handle, task) = SessionActor::create(self, cfg).await?;
        let id = handle.id.clone();
        self.adopt_session(handle.clone());
        let host = Arc::clone(self);
        tokio::spawn(async move {
            // A panicking actor must still leave the map (§6 #12): the
            // handle would otherwise be listed forever and attach → Closed.
            use futures::FutureExt;
            if std::panic::AssertUnwindSafe(task.run())
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::error!(session = %id, "session actor panicked");
            }
            host.remove_session(&id);
        });
        Ok(handle)
    }

    /// Register a handle built elsewhere (daemon test factories). The
    /// caller owns the task; `remove_session` when it ends.
    pub fn adopt_session(&self, handle: SessionHandle) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle.id.clone(), handle);
    }

    /// Every registered handle (B4: THE session map; `DaemonState`
    /// delegates here). Dead handles are dropped defensively: the actor
    /// task drops `cmd_rx` when `run()` returns and `remove_session` runs
    /// *after* — a listing in that window sees `is_alive() == false`. That
    /// is an ordering window, not an invariant violation, so it is logged
    /// at debug, never asserted.
    pub fn session_handles(&self) -> Vec<SessionHandle> {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, h| h.is_alive());
        if before != map.len() {
            tracing::debug!(
                pruned = before - map.len(),
                "session_handles: pruned ended sessions ahead of their self-removal"
            );
        }
        map.values().cloned().collect()
    }

    /// Resolve a `continue_session` request against the LIVE sessions
    /// first — actor id, current journal id (a LinkedSuccessor compaction
    /// moves it off the actor id), or name (`--name` / `saveas`, which may
    /// not be on disk yet) — then the way `setup::resolve_or_create_session`
    /// will (chain → name → partial id; `Some(None)` = latest by mtime) and
    /// match the resolved id against id *and* journal id. Anything that is
    /// not running here returns `None` so the real boot path builds/reports.
    ///
    /// The daemon's `Attach::Create{continue}` goes through here too
    /// (`host_factory` → `create_session`): a live journal is always one
    /// actor, never a second one on the same file / UDS / registry entry.
    fn live_continue_target(
        &self,
        continue_session: &Option<Option<String>>,
    ) -> Option<SessionHandle> {
        let query = continue_session.as_ref()?;
        let handles: Vec<SessionHandle> = {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            sessions.values().filter(|h| h.is_alive()).cloned().collect()
        };
        if handles.is_empty() {
            return None;
        }
        let by_id = |id: &str| {
            handles
                .iter()
                .find(|h| h.id.as_str() == id || h.journal_id() == id)
                .cloned()
        };
        let id = match query {
            Some(q) => {
                if let Some(h) = by_id(q) {
                    return Some(h);
                }
                if let Some(h) = handles.iter().find(|h| h.name().as_deref() == Some(q.as_str())) {
                    return Some(h.clone());
                }
                crate::core::session::resolve_session(q).ok()?.id
            }
            None => crate::core::session::latest_session().ok()?.id,
        };
        by_id(&id)
    }

    /// Handle for a live session, if any.
    pub fn attach(&self, id: &SessionId) -> Option<SessionHandle> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// Metadata of every live session, with the live cells filled
    /// (lifecycle, clients, input_owner, awaiting_input, journal_id).
    pub fn sessions(&self) -> Vec<SessionMeta> {
        self.session_handles()
            .iter()
            .map(|h| h.meta_live())
            .collect()
    }

    /// Drop the host's handle for a session (the actor keeps running until
    /// its command queue closes or it receives `End`).
    pub fn remove_session(&self, id: &SessionId) -> Option<SessionHandle> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    // ── shared extension host (Phase 2, C) ────────────────────────────────

    /// Loader seam (C2): a discovery pass for this host's manager has been
    /// dispatched. Called synchronously by `spawn_discover_and_load` before
    /// its task runs, so `extensions_ready()` cannot slip through the gap.
    pub fn note_extensions_loading(&self) {
        self.extensions_loading
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Loader seam (C2): discovery finished — every `extensions_ready()`
    /// waiter proceeds. Idempotent.
    pub fn mark_extensions_ready(&self) {
        self.extensions_ready.send_replace(true);
    }

    /// Loader seam (C2): `note_extensions_loading()` now, and
    /// `mark_extensions_ready()` when the guard drops — however the loader
    /// task ends (return, cancel, panic). Hold it inside the task so a
    /// panicking walk cannot strand `extensions_ready()` waiters.
    pub fn extensions_loading_guard(self: &Arc<Self>) -> ExtensionsLoadingGuard {
        self.note_extensions_loading();
        ExtensionsLoadingGuard {
            host: Arc::clone(self),
        }
    }

    /// Resolve once extension discovery on this host is known-finished, so a
    /// session's `on_session_start` lands on subscribed extensions. Resolves
    /// IMMEDIATELY when no loader was dispatched (hosts that never load
    /// extensions, `--no-extensions`, tests) or discovery already completed
    /// on the manager (in-process `discover_and_load()` callers such as
    /// `synaps chat`); otherwise awaits `mark_extensions_ready()`.
    ///
    /// The watch sender is a field of the host, so "sender dropped" is NOT a
    /// wake-up path while the host lives. Liveness comes from
    /// [`ExtensionsLoadingGuard`] (the loader task marks ready on any exit,
    /// including panic); callers that must not trust the loader at all
    /// (`SessionActor::create`) additionally bound the await with
    /// `budgets::EXTENSIONS_READY_TIMEOUT`.
    pub async fn extensions_ready(&self) {
        let mut rx = self.extensions_ready.subscribe();
        if *rx.borrow() {
            return;
        }
        // A running walk holds the manager write lock; this read waits it out.
        if self.ext_manager.read().await.discovery_done().is_some() {
            return;
        }
        if !self
            .extensions_loading
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// C3: push one extension notification frame to every live session
    /// (non-blocking; a session whose command queue is full drops it with a
    /// warning — widget upserts are idempotent last-writer-wins). Frames
    /// carry no session id today, hence broadcast. Returns how many
    /// sessions accepted it.
    pub async fn broadcast_extension_notification(
        &self,
        ext_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> usize {
        let handles: Vec<SessionHandle> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut delivered = 0;
        for handle in handles {
            let cmd = crate::session::SessionCommand::HostEvent(
                crate::session::HostEvent::ExtensionNotification {
                    extension_id: ext_id.to_string(),
                    method: method.to_string(),
                    params: params.clone(),
                },
            );
            match handle.send(cmd).await {
                Ok(()) => delivered += 1,
                Err(err) => tracing::warn!(
                    session = %handle.id,
                    extension = %ext_id,
                    method = %method,
                    error = %err,
                    "dropping extension notification for session"
                ),
            }
        }
        delivered
    }
}
