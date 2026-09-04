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
    pub profile: Option<String>,
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
    /// early guard drop (the old `setup::boot` bug).
    #[allow(dead_code)]
    log_guard: std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>,
}

static HOST: OnceLock<Arc<EngineHost>> = OnceLock::new();

impl EngineHost {
    /// Every step of the old `setup::boot()` that does NOT mention a session:
    /// profile, logging, HTTP client, registry, config, skills, MCP, extension
    /// manager, OpenAI routing static. Does NOT install the global broker —
    /// that happens in [`Self::foreground_runtime`] via `apply_config`, exactly
    /// where the old boot did it relative to session resolution.
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
        crate::runtime::openai::set_extension_manager_for_routing(Arc::clone(&ext_manager));

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
        }))
    }

    /// Idempotent install of the process host. A second call with a
    /// different host is a programming error: returns `Err(rejected)` and
    /// never replaces the installed one.
    pub fn install(host: Arc<Self>) -> std::result::Result<(), Arc<Self>> {
        match HOST.get() {
            Some(existing) if Arc::ptr_eq(existing, &host) => Ok(()),
            Some(_) => Err(host),
            None => HOST.set(host),
        }
    }

    pub fn current() -> Option<Arc<Self>> {
        HOST.get().cloned()
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
    pub async fn foreground_runtime(&self) -> Result<Runtime> {
        let mut runtime = Runtime::from_parts(RuntimeParts::with_reaper(self.parts.clone()));
        runtime.apply_config(&self.config());
        Ok(runtime)
    }

    /// A worker (subagent) runtime. Shares client, credential source and
    /// token cache (so NO broker re-install and NO cache eviction) and takes
    /// a CLONE of the cached worker registry template. Does NOT share:
    /// hook_bus (fresh, empty — hooks do not fire for subagent tool calls),
    /// session_manager (fresh + reaper), mcp_runtime/extension_runtime
    /// (`None`, as `Runtime::new()`).
    pub async fn worker_runtime(&self) -> Result<Runtime> {
        let mut host = self.parts.clone();
        host.hook_bus = Arc::new(HookBus::new());
        host.tools = Arc::new(RwLock::new(self.worker_registry().await));
        host.mcp_runtime = None;
        host.extension_runtime = None;
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
}
