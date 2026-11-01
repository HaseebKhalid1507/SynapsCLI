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

/// What a create tells the caller that scoped it ([`CREATE_PROBE`]): a
/// daemon's crash restore, whose per-session cap counts from the moment its
/// create may build (it holds its session's reservation, see
/// [`EngineHost::create_session`]) or hands back the live session, never
/// time spent waiting for another create of the same session, and which
/// applies only its resume decision to a session a client's create already
/// brought back.
#[derive(Default)]
pub struct CreateProbe {
    /// Fired once the create holds its session's reservation (the create
    /// lock of that session), or found the session live.
    pub locked: tokio::sync::Notify,
    /// The create found its `--continue` target already live and handed it
    /// back instead of building one.
    pub found_live: std::sync::atomic::AtomicBool,
}

tokio::task_local! {
    /// Set around a crash restore's create (`daemon::crash_record`); unset
    /// for every other create.
    pub static CREATE_PROBE: Arc<CreateProbe>;
}

/// The create running in this task holds its session's reservation now, or
/// found the session live (a no-op outside [`CREATE_PROBE`]'s scope).
/// [`EngineHost::create_session`] calls it; a test session factory that
/// stands in for a real create calls it at the point a real create would
/// hold the reservation.
pub fn note_create_locked() {
    let _ = CREATE_PROBE.try_with(|p| p.locked.notify_one());
}

/// Test seam: what a create awaits just before it builds its actor (never
/// when it hands back a live one). `Err` fails the create with that text; a
/// panic in it unwinds the create.
#[cfg(any(test, feature = "testing"))]
pub type CreateBuildHook = Arc<
    dyn Fn() -> futures::future::BoxFuture<'static, std::result::Result<(), String>> + Send + Sync,
>;

#[cfg(any(test, feature = "testing"))]
tokio::task_local! {
    /// Set by a test around a create; unset everywhere else.
    pub static CREATE_BUILD_HOOK: CreateBuildHook;
}

/// Awaits [`CREATE_BUILD_HOOK`] when a test set one: a no-op otherwise, and
/// in every build without the `testing` feature.
async fn before_build() -> Result<()> {
    #[cfg(any(test, feature = "testing"))]
    if let Ok(hook) = CREATE_BUILD_HOOK.try_with(|hook| hook()) {
        hook.await.map_err(crate::RuntimeError::Session)?;
    }
    Ok(())
}

/// What a create reserves while it builds an actor, so another create of
/// the same session waits for that actor instead of building a second one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CreateKey {
    /// A journal id: the one a `continue_session` resolves to on disk
    /// (after `compacted_into`), or the one the build itself resolved
    /// ([`CreateReservation::confirm`]).
    Journal(String),
    /// A session name: a `continue_session` query (it may name the session
    /// another create is naming right now), the resolved session's own name,
    /// or the `--name` a create sets.
    Name(String),
}

/// The keys of one create: `query` = its `continue_session`, `resolved` =
/// the journal id and name that query resolves to on disk, `name` = its
/// `--name`. A new session without a name has none: its id is new, and the
/// build checks it ([`CreateReservation::confirm`]).
fn create_keys(
    query: Option<&Option<String>>,
    resolved: Option<(&str, Option<&str>)>,
    name: Option<&str>,
) -> Vec<CreateKey> {
    let mut keys = Vec::new();
    let mut add = |k: CreateKey| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    if let Some(Some(q)) = query {
        add(CreateKey::Name(q.clone()));
    }
    if let Some((id, own_name)) = resolved {
        add(CreateKey::Journal(id.to_string()));
        if let Some(n) = own_name {
            add(CreateKey::Name(n.to_string()));
        }
    }
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        add(CreateKey::Name(n.to_string()));
    }
    keys
}

/// Where a create's `continue_session` leads, resolved on disk with no lock
/// held: the journal id it reaches (after `compacted_into`), and the keys
/// the create reserves.
struct CreateTarget {
    resolved: Option<String>,
    keys: Vec<CreateKey>,
}

/// The keys held by creates that are building an actor right now
/// ([`EngineHost::create_session`]). A key is held by at most one create;
/// each entry is a receiver of that create's reservation channel, which
/// closes when the reservation ends.
#[derive(Default)]
pub(crate) struct CreateGate {
    pending: std::sync::Mutex<
        std::collections::HashMap<CreateKey, tokio::sync::watch::Receiver<()>>,
    >,
}

/// What [`CreateGate::claim`] found.
pub(crate) enum Claim<'g, H> {
    /// The target is live: hand it back.
    Live(H),
    /// Another create holds one of the keys: `changed()` returns once its
    /// reservation ends (the receiver is cloned under the gate, so the end
    /// is never missed). Then resolve again.
    Busy(tokio::sync::watch::Receiver<()>),
    /// Every key is this create's until the reservation drops.
    Reserved(CreateReservation<'g>),
}

impl CreateGate {
    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<CreateKey, tokio::sync::watch::Receiver<()>>>
    {
        // Nothing panics while holding it; a poisoned map is still exact.
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Under the gate, in this order: wait when another create holds one of
    /// `keys`; hand back what `live` finds; else reserve every key. Held
    /// keys go first: a key no create holds means every build that held it
    /// has registered its actor already (a build registers, then releases),
    /// so `live` sees that actor.
    pub(crate) fn claim<H>(&self, keys: &[CreateKey], live: impl FnOnce() -> Option<H>) -> Claim<'_, H> {
        let mut pending = self.pending();
        if let Some(held) = keys.iter().find_map(|k| pending.get(k)) {
            return Claim::Busy(held.clone());
        }
        if let Some(h) = live() {
            return Claim::Live(h);
        }
        let (done, rx) = tokio::sync::watch::channel(());
        for k in keys {
            pending.insert(k.clone(), rx.clone());
        }
        Claim::Reserved(CreateReservation {
            gate: self,
            keys: std::sync::Mutex::new(keys.to_vec()),
            rx,
            race: std::sync::Mutex::new(None),
            _done: done,
        })
    }
}

/// Why a build stopped at [`CreateReservation::confirm`].
#[derive(Debug)]
pub(crate) enum CreateRace {
    /// Another create holds the journal id this build resolved: wait for
    /// it, then resolve again.
    Busy(tokio::sync::watch::Receiver<()>),
    /// The journal id this build resolved is live: hand that back.
    Live,
    /// This new session's id is taken (live, held or on disk): build again,
    /// with a new id.
    TakenId,
}

/// A create's keys, held until it drops: after the actor is registered, on
/// an error, in a panic's unwind, or when the create is cancelled. Dropping
/// it wakes every create waiting for one of its keys.
pub(crate) struct CreateReservation<'g> {
    gate: &'g CreateGate,
    keys: std::sync::Mutex<Vec<CreateKey>>,
    /// Its own entries in the gate (`same_channel`).
    rx: tokio::sync::watch::Receiver<()>,
    race: std::sync::Mutex<Option<CreateRace>>,
    /// Dropped after `drop` has removed the keys: closes every waiter's
    /// receiver.
    _done: tokio::sync::watch::Sender<()>,
}

impl CreateReservation<'_> {
    /// Called by the build as soon as it knows its session's id, before it
    /// writes anything (the session file, the per-session socket, the
    /// registry entry): the journal id it continues, or a new session's id
    /// (`new_session`). `taken` = whether that id is live (or, for a new
    /// session, live or on disk). The id becomes one of this reservation's
    /// keys; when it is not free, `Err` stops the build and [`Self::take_race`]
    /// says why. A `continue_session` resolved again by the build can land
    /// elsewhere than the create's own resolution (a journal saved in
    /// between); a new id can repeat one (ids are second-resolution).
    pub(crate) fn confirm(&self, id: &str, new_session: bool, taken: impl FnOnce(&str) -> bool) -> Result<()> {
        let key = CreateKey::Journal(id.to_string());
        let race = {
            let mut pending = self.gate.pending();
            match pending.get(&key) {
                Some(held) if held.same_channel(&self.rx) => return Ok(()),
                Some(_) if new_session => CreateRace::TakenId,
                Some(held) => CreateRace::Busy(held.clone()),
                None if taken(id) => {
                    if new_session {
                        CreateRace::TakenId
                    } else {
                        CreateRace::Live
                    }
                }
                None => {
                    pending.insert(key.clone(), self.rx.clone());
                    self.keys.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(key);
                    return Ok(());
                }
            }
        };
        let why = match &race {
            CreateRace::Busy(_) => format!("session {id} is being created by another create"),
            CreateRace::Live => format!("session {id} is live already"),
            CreateRace::TakenId => format!("new session id {id} is taken"),
        };
        *self.race.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(race);
        Err(crate::RuntimeError::Session(why))
    }

    /// Why [`Self::confirm`] stopped the build, if it did.
    fn take_race(&self) -> Option<CreateRace> {
        self.race.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
    }
}

impl Drop for CreateReservation<'_> {
    fn drop(&mut self) {
        let keys = std::mem::take(self.keys.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner));
        let mut pending = self.gate.pending();
        for k in keys {
            if pending.get(&k).is_some_and(|held| held.same_channel(&self.rx)) {
                pending.remove(&k);
            }
        }
    }
}

/// [`CreateReservation::confirm`] as the build gets it.
pub(crate) type ConfirmId<'a> = &'a (dyn Fn(&str, bool) -> Result<()> + Send + Sync);

/// A create raced at its build ([`CreateRace`]) at most this many times
/// before it fails with the last race's error.
const CREATE_RACES: u32 = 4;

/// A create hands back a live session instead of building one.
fn attach_live(live: SessionHandle) -> SessionHandle {
    note_create_locked();
    tracing::info!(session = %live.id, "create_session: target is live — attaching");
    let _ = CREATE_PROBE.try_with(|p| p.found_live.store(true, std::sync::atomic::Ordering::SeqCst));
    live
}

/// Whether a session file for `id` is on disk (a new session's id must not
/// be one).
fn session_file_exists(id: &str) -> bool {
    !id.contains('/')
        && std::fs::symlink_metadata(agent_core::session_lock::sessions_dir().join(format!("{id}.json"))).is_ok()
}

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
    /// `<base>/spill` (tool output virtualisation), bound once at boot.
    pub spill_root: std::path::PathBuf,
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
    /// When MCP servers without cached descriptors are discovered.
    /// [`McpDiscovery::Inline`] (in-process modes) discovers them during
    /// `boot()`; [`McpDiscovery::Background`] (the daemon) boots with cached
    /// descriptors only and leaves discovery to
    /// [`EngineHost::spawn_background_mcp_discovery`], so a hung MCP server
    /// can never hold up readiness.
    pub mcp_discovery: McpDiscovery,
}

pub use crate::mcp::McpDiscovery;

/// What a cached worker registry template was built from
/// ([`EngineHost::worker_registry`]).
#[derive(PartialEq, Eq)]
struct WorkerRegistryKey {
    generation: CatalogGeneration,
    disabled_tools: Vec<String>,
}

pub struct EngineHost {
    parts: HostParts,
    config: arc_swap::ArcSwap<crate::SynapsConfig>,
    command_registry: Arc<crate::skills::registry::CommandRegistry>,
    keybind_registry: Arc<std::sync::RwLock<crate::skills::keybinds::KeybindRegistry>>,
    /// `skills.status` / `skills.reload` (skills v1) on `command_registry`.
    skills: crate::skills::status::SkillsState,
    ext_manager: Arc<RwLock<crate::extensions::manager::ExtensionManager>>,
    mcp_server_count: usize,
    /// WELCOME's `config_path`: the canonical path of the config file the
    /// boot snapshot was read from (`None`: not valid UTF-8).
    started_config_path: Option<String>,
    /// WELCOME's `memory_backend`: the backend bound at boot.
    started_memory_backend: &'static str,
    /// Worker (subagent) registry template, cached by what it is built from:
    /// the shared catalog generation and the `disabled_tools` list.
    /// `worker_runtime()` clones this instead of rebuilding
    /// `ToolRegistry::worker_set` + `rebuild_schema()` per spawn.
    worker_registry: std::sync::Mutex<Option<(WorkerRegistryKey, ToolRegistry)>>,
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
    /// Per-session create reservations: two concurrent `--continue X` can
    /// never both miss the live check and build two actors on one session
    /// id, while creates of different sessions build at the same time.
    create_gate: CreateGate,
    /// One session endpoint start at a time in this host
    /// ([`Self::endpoint_start_turn`]).
    endpoint_start: tokio::sync::Mutex<()>,
    /// (E-P2) Single-tenancy driver lock: at most one plugin can drive one
    /// session daemon-wide. Grants do NOT survive reload (§3 S2/S5).
    driver_grants: std::sync::Mutex<std::collections::HashMap<String, SessionId>>,
    /// C2: flips to `true` when extension discovery on `ext_manager` has
    /// finished (the loader sets it; `extensions_ready()` awaits it). The
    /// paired flag records that a loader was DISPATCHED, so a session
    /// created between `spawn_discover_and_load` and its first lock
    /// acquisition still waits instead of racing the walk.
    extensions_ready: tokio::sync::watch::Sender<bool>,
    extensions_loading: std::sync::atomic::AtomicBool,
    /// MCP discovery deferred by [`McpDiscovery::Background`]; taken (once)
    /// by [`Self::spawn_background_mcp_discovery`].
    pending_mcp_discovery: std::sync::Mutex<Option<crate::mcp::PendingMcpDiscovery>>,
    /// One `on_session_start` at a time on this host, a hook that a turn
    /// waits for first. A reload restores every session at once; their
    /// hooks, all fired together, would queue in each single-process
    /// extension and the tail would pass the handler timeout and lose its
    /// injection for the whole session.
    session_start_gate: crate::session::session_start::SessionStartGate,
    /// The daemon's pin book (set once by `Daemon::start`): sessions on this
    /// host write their pin changes through it. `None` in-process.
    pin_sink: std::sync::RwLock<Option<Arc<dyn crate::session::PinSink>>>,
    /// This daemon's crash record, as capacity resumes see it
    /// (`session::live::RecordLink`); set by the daemon only while it keeps
    /// a record. `None`: no capacity resume is armed, held or dispatched.
    record_link: arc_swap::ArcSwapOption<crate::session::live::RecordLink>,
}

/// (E-P2) Returned by [`EngineHost::claim_driver`] when another live session
/// already holds the driver grant for a plugin.
#[derive(Debug)]
#[allow(dead_code)] // fields read by Display impl + P3 handler
pub struct DriverGrantError {
    pub plugin: String,
    pub held_by: SessionId,
}

impl std::fmt::Display for DriverGrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin '{}' already driving session {}",
            self.plugin, self.held_by
        )
    }
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
        let spill_root = crate::runtime::sweep_spill_root();

        let config = crate::config::load_config();
        // Fixed for this image's lifetime, like the snapshot: what WELCOME
        // reports as the file this daemon read at start.
        let started_config_path = crate::config::canonical_config_read_path()
            .into_os_string()
            .into_string()
            .ok();
        let tools = Arc::new(RwLock::new(ToolRegistry::new()));
        // Same point the old boot's `apply_config` disabled tools: on the
        // fresh registry, before skills/MCP register anything.
        if !config.disabled_tools.is_empty() {
            tools.write().await.disable(&config.disabled_tools);
        }

        // Discover plugins/skills, build command registry, register load_skill tool.
        let crate::skills::Registered {
            commands: command_registry,
            keybinds: keybind_registry,
            state: skills,
        } = crate::skills::register_reported(&tools, &config).await;

        // MCP (if configured): descriptor-backed tools, executed through
        // per-session leases in every mode. With progressive disclosure they
        // stay dormant until activated; without it they are part of every
        // session's core. A configured server with no cached descriptors is
        // discovered once (parallel, bounded, negative-cached): here for
        // `Inline`, later in a background task for `Background` (daemon).
        let (mcp_server_count, pending_mcp_discovery) = match opts.mcp_discovery {
            McpDiscovery::Inline => (crate::mcp::setup_lazy_mcp(&tools).await, None),
            McpDiscovery::Background => crate::mcp::setup_cached_mcp(&tools).await,
        };
        let mcp_runtime = Some(Arc::new(crate::mcp::McpRuntimeManager::new(
            crate::mcp::lease::config_source_from_disk(),
            crate::mcp::lease::DEFAULT_IDLE_MAX,
        )));

        let hook_bus = Arc::new(HookBus::new());
        let mut ext_mgr = crate::extensions::manager::ExtensionManager::new_with_tools(
            Arc::clone(&hook_bus),
            Arc::clone(&tools),
        );
        // Per-process: tell the extension manager whether memory is exclusive
        // (Axel) so extensions spawned later inherit the right policy.
        let mem_binding = crate::memory_backend::MemoryBinding::from_config(&config.memory_backend);
        ext_mgr.bind_memory_backend(mem_binding.exclusive());
        let started_memory_backend = mem_binding.backend_name();
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
            spill_root,
            progressive_tool_disclosure: config.progressive_tool_disclosure,
        };

        Ok(Arc::new(Self {
            parts,
            config: arc_swap::ArcSwap::from_pointee(config),
            command_registry,
            keybind_registry,
            skills,
            ext_manager,
            mcp_server_count,
            started_config_path,
            started_memory_backend,
            worker_registry: std::sync::Mutex::new(None),
            log_guard: std::sync::Mutex::new(log_guard),
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            create_gate: CreateGate::default(),
            endpoint_start: tokio::sync::Mutex::new(()),
            driver_grants: std::sync::Mutex::new(std::collections::HashMap::new()),
            extensions_ready: tokio::sync::watch::channel(false).0,
            extensions_loading: std::sync::atomic::AtomicBool::new(false),
            pending_mcp_discovery: std::sync::Mutex::new(pending_mcp_discovery),
            session_start_gate: crate::session::session_start::SessionStartGate::new(),
            pin_sink: std::sync::RwLock::new(None),
            record_link: arc_swap::ArcSwapOption::empty(),
        }))
    }

    /// [`McpDiscovery::Background`]: start the deferred MCP discovery in a
    /// background task (at most once per host; `None` when nothing is
    /// pending — `Inline` boot, every server cached, or already spawned).
    /// The daemon calls this after it is accepting. On completion ONLY the
    /// newly discovered servers' dormant tools are added to the shared
    /// registry, as one atomic `try_register_batch` (a rejected batch is
    /// logged and fails closed). Like late extension registration this
    /// mutates the shared catalog while sessions may be live: with
    /// progressive disclosure on the tools are dormant (searchable,
    /// activatable); with it off they are core tools that every live session
    /// sends from its NEXT request — one prompt-cache rewrite per live
    /// session, once. The task resolves to the number of tools registered.
    pub fn spawn_background_mcp_discovery(&self) -> Option<tokio::task::JoinHandle<usize>> {
        let pending = self
            .pending_mcp_discovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        let tools = Arc::clone(&self.parts.tools);
        tracing::info!(
            servers = ?pending.server_names(),
            "MCP: discovering uncached servers in the background"
        );
        Some(tokio::spawn(async move { pending.run(&tools).await }))
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

    /// [`Self::flush_installed_logs`] for a panic hook: never waits for the
    /// guard's mutex, since the panicking thread may be the one holding it
    /// (mid-flush). `true` when this call dropped the guard; `false` when
    /// there is no host, it was already flushed, or someone else holds it.
    pub fn try_flush_installed_logs() -> bool {
        HOST.get()
            .and_then(|h| take_without_blocking(&h.log_guard))
            .map(drop)
            .is_some()
    }

    pub fn parts(&self) -> &HostParts {
        &self.parts
    }

    pub fn config(&self) -> arc_swap::Guard<Arc<crate::SynapsConfig>> {
        self.config.load()
    }

    /// Test seam: replace the host's config snapshot without touching the
    /// config file (which the other tests in a binary share).
    #[cfg(any(test, feature = "testing"))]
    pub fn set_config_for_test(&self, config: crate::SynapsConfig) {
        self.config.store(Arc::new(config));
    }

    /// Headless hosts (`synaps chat`, `rpc`, daemon) have no startup notice
    /// area: print config warnings (unknown keys, bad or deprecated values)
    /// to stderr and the log. The TUI shows them as notices instead.
    pub fn report_config_warnings(&self) {
        report_config_warnings(&self.config());
    }

    /// Install the daemon's pin book (each daemon start on this host
    /// replaces it: the host is process-global, a daemon is not).
    pub fn set_pin_sink(&self, sink: Arc<dyn crate::session::PinSink>) {
        *self.pin_sink.write().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    pub(crate) fn pin_sink(&self) -> Option<Arc<dyn crate::session::PinSink>> {
        self.pin_sink.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The crash record's capacity-resume link, while the daemon keeps one.
    pub fn record_link(&self) -> Option<Arc<crate::session::live::RecordLink>> {
        self.record_link.load_full()
    }

    /// The daemon keeps a crash record (`Some`), or not (`None`).
    pub fn set_record_link(&self, link: Option<Arc<crate::session::live::RecordLink>>) {
        self.record_link.store(link);
    }

    /// The host's `on_session_start` gate (one hook at a time).
    pub(crate) fn session_start_gate(&self) -> &crate::session::session_start::SessionStartGate {
        &self.session_start_gate
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

    /// `skills.status`: the last skill discovery pass (boot or reload).
    pub fn skills_status(&self) -> crate::skills::status::SkillsReport {
        self.skills.status()
    }

    /// `skills.reload`: walk the skill roots again with the skill keys read
    /// fresh from the config file (this host's config snapshot is kept) and
    /// swap the new set into the command registry, one reload at a time.
    pub async fn reload_skills(
        &self,
    ) -> std::result::Result<crate::skills::status::SkillsReport, String> {
        self.skills.reload(&self.command_registry).await
    }

    pub fn mcp_server_count(&self) -> usize {
        self.mcp_server_count
    }

    /// The canonical path of the config file read at boot (WELCOME's
    /// `config_path`); `None` when it is not valid UTF-8.
    pub fn started_config_path(&self) -> Option<&str> {
        self.started_config_path.as_deref()
    }

    /// The memory backend bound at boot: "axel", "legacy" or "unavailable"
    /// (WELCOME's `memory_backend`).
    pub fn started_memory_backend(&self) -> &'static str {
        self.started_memory_backend
    }

    /// The foreground runtime for an interactive session: shares client,
    /// tools, hook_bus, credential source, token cache, MCP/extension lease
    /// managers. Everything else is fresh (exactly `Runtime::new()`'s values).
    /// `apply_config` runs here — the ONLY place the broker is (re)installed.
    /// `disabled_tools` is NOT re-applied: `boot()` did it once on the fresh
    /// registry, before skills/MCP registered, exactly where the old boot did.
    pub async fn foreground_runtime(&self) -> Result<Runtime> {
        self.foreground_runtime_for(None).await
    }

    /// `foreground_runtime()` for a session whose cwd is not the process cwd
    /// (daemon-hosted sessions). The cwd is set BEFORE `apply_config` so the
    /// memory binding (`apply_memory_backend_config`, one-shot) scopes to the
    /// session's project, not the daemon's — A-engine-merge §4. `None` =
    /// process cwd, byte-identical to `foreground_runtime()`.
    pub async fn foreground_runtime_for(&self, cwd: Option<std::path::PathBuf>) -> Result<Runtime> {
        self.foreground_runtime_with(cwd, true).await
    }

    /// `foreground_runtime_for`, applying the operator's `memory.default_mode`
    /// only when `memory_default` — a session runtime rebuilt for a session
    /// that made its own `/memory` choice passes `false` and restores that
    /// choice instead (`SessionConfig::memory_mode`).
    pub(crate) async fn foreground_runtime_with(
        &self,
        cwd: Option<std::path::PathBuf>,
        memory_default: bool,
    ) -> Result<Runtime> {
        let mut runtime = Runtime::from_parts(RuntimeParts::with_reaper(self.parts.clone()));
        runtime.set_cwd(cwd);
        let config = self.config();
        runtime.apply_config_keep_tools(&config);
        // Every interactive session starts in the operator's confirmed
        // `memory.default_mode` (after the backend binding above). A failed
        // grant is logged and leaves memory off; it never fails the session.
        if memory_default {
            let _ = runtime.apply_memory_default_mode(&config.memory);
        }
        Ok(runtime)
    }

    /// The model a new session on this host gets right now (no `--model`,
    /// no `--continue`): the runtime default with the host's config snapshot
    /// applied, the same step `foreground_runtime_for` runs for `create`.
    /// The snapshot is the one loaded at boot (a daemon reload re-boots);
    /// session create does not re-read the config file either.
    pub fn new_session_model(&self) -> String {
        let mut runtime = Runtime::from_parts(RuntimeParts::without_reaper(self.parts.clone()));
        runtime.apply_config_model(&self.config());
        runtime.model().to_string()
    }

    /// A worker (subagent) runtime. Shares credential source and token cache
    /// (so NO broker re-install and NO cache eviction) and takes a CLONE of
    /// the cached worker registry template. Does NOT share: the HTTP client
    /// (fresh — see below), hook_bus (a worker view of it instead — see
    /// [`Self::worker_hook_bus`]), session_manager (fresh + reaper),
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
        host.hook_bus = self.worker_hook_bus();
        host.tools = Arc::new(RwLock::new(self.worker_registry().await));
        host.mcp_runtime = None;
        host.extension_runtime = None;
        // `Runtime::new()` value. Workers never ran `apply_config`, so they
        // never had disclosure on; their registry has no discovery/activation
        // tools, so a projected schema would silently drop every extension
        // tool.
        host.progressive_tool_disclosure = false;
        let mut runtime = Runtime::from_parts(RuntimeParts::with_reaper(host));
        runtime.apply_worker_config(&self.config());
        Ok(runtime)
    }

    /// The hook bus every subagent worker runs with, on both spawn paths
    /// (this host's `worker_runtime` and the legacy
    /// `SYNAPS_SUBAGENT_FRESH_RUNTIME=1` runtime): a live view of the host
    /// bus's `after_tool_call` subscriptions from extensions holding
    /// `tools.transform_output` ([`HookBus::worker_transforms`]). Output
    /// transforms (compression, redaction) see a worker's tool results the
    /// way they see the foreground's; `before_tool_call` and every other
    /// hook kind still do not fire for subagent tool calls, so a worker
    /// never raises a confirm prompt. The extension sees `session_id: null`
    /// (a worker has no conversation id). `subagent_transform_hooks = off`
    /// detaches it (`apply_subagent_runtime_policy`).
    pub fn worker_hook_bus(&self) -> Arc<HookBus> {
        HookBus::worker_transforms(&self.parts.hook_bus)
    }

    /// Cached-or-rebuilt worker registry ([`ToolRegistry::worker_set`]).
    /// The worker's built-ins are built from scratch, so the boot-time
    /// disable on the shared registry does not carry over: `worker_set`
    /// re-applies the whole `disabled_tools` list (before, a worker of a
    /// host with `disabled_tools = bash` still had `bash`; only the forum
    /// names were re-applied). The template is a pure function of the shared
    /// registry's tool set (a pure function of its catalog generation) and
    /// the list, so caching by both returns exactly what a rebuild would,
    /// and a `reload_config` that changes the list rebuilds it.
    pub async fn worker_registry(&self) -> ToolRegistry {
        let shared = self.parts.tools.read().await;
        let key = WorkerRegistryKey {
            generation: shared.catalog().generation(),
            disabled_tools: self.config().disabled_tools.clone(),
        };
        if let Some((cached, tpl)) = &*self.worker_registry.lock().unwrap() {
            if *cached == key {
                return tpl.clone();
            }
        }
        let fresh = ToolRegistry::worker_set(&shared, &key.disabled_tools);
        drop(shared);
        *self.worker_registry.lock().unwrap() = Some((key, fresh.clone()));
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
    ///
    /// **One build per session, many sessions at once:** a create reserves
    /// its session's keys ([`CreateKey`]: the journal id its
    /// `continue_session` resolves to, that query, the session's name, its
    /// `--name`) and builds its actor with no lock held. A create that finds
    /// one of its keys reserved waits for that create, then resolves again:
    /// it attaches to the actor that create registered, or (the build
    /// failed, panicked or was cancelled) reserves the keys itself. The
    /// build confirms the id it resolved itself before it writes anything
    /// ([`CreateReservation::confirm`]). Creates of different sessions
    /// build at the same time (a restore's lanes).
    pub async fn create_session(self: &Arc<Self>, cfg: SessionConfig) -> Result<SessionHandle> {
        // A create that waits for extension discovery waits BEFORE it
        // reserves anything, so no other create of its session queues
        // behind a discovery still running. A target already live is handed
        // back without that wait. One that was live when the wait was
        // skipped but ended before the reservation gets the wait after all
        // (the reservation is dropped and taken again), so no create builds
        // an actor without it.
        let mut waited = false;
        let mut races = 0u32;
        loop {
            // Live by its query (memory only): no disk look-up, no wait.
            if let Some(live) = self.live_by_query(&cfg) {
                return Ok(attach_live(live));
            }
            let target = self.create_target(&cfg);
            if !waited && self.live_continue_target(&cfg, &target).is_none() {
                SessionActor::wait_extensions_for_create(self, &cfg).await;
                waited = true;
            }
            let reservation = match self
                .create_gate
                .claim(&target.keys, || self.live_continue_target(&cfg, &target))
            {
                Claim::Live(live) => return Ok(attach_live(live)),
                Claim::Busy(mut done) => {
                    let _ = done.changed().await;
                    continue;
                }
                Claim::Reserved(r) if waited => r,
                Claim::Reserved(_) => continue,
            };
            // A restore lane's cap starts at the first of these. One edge
            // counts inside it: a build that resolves a session its own
            // reservation does not hold and finds that one held by another
            // create (a confirm-time Busy, below) waits for it within the
            // cap. That takes create_target and the build resolving
            // differently (the disk changed between them); a restore lane
            // looks up its exact journal id in both, so it does not arise
            // for one in practice.
            note_create_locked();
            let confirm = |id: &str, new_session: bool| {
                reservation.confirm(id, new_session, |id| {
                    self.live_journal(id).is_some() || (new_session && session_file_exists(id))
                })
            };
            let built = match before_build().await {
                Ok(()) => SessionActor::create(self, cfg.clone(), &confirm).await,
                Err(e) => Err(e),
            };
            let (handle, task) = match built {
                Ok(built) => built,
                Err(e) => {
                    let Some(race) = reservation.take_race() else {
                        return Err(e);
                    };
                    races += 1;
                    tracing::info!(attempt = races, "create_session: {e}; resolving again");
                    if races >= CREATE_RACES {
                        return Err(e);
                    }
                    drop(reservation);
                    if let CreateRace::Busy(mut done) = race {
                        let _ = done.changed().await;
                    }
                    continue;
                }
            };
            let id = handle.id.clone();
            // Registered before the reservation ends, with no await between:
            // a create that finds the keys free finds this actor live.
            self.adopt_session(handle.clone());
            drop(reservation);
            self.spawn_session_task(id, task);
            return Ok(handle);
        }
    }

    /// Run a registered actor's task; it leaves the map when it ends.
    fn spawn_session_task(self: &Arc<Self>, id: SessionId, task: crate::session::SessionTask) {
        let host = Arc::clone(self);
        // Boxed here, outside the task: the actor's future is large, and an
        // unoptimized build would otherwise carry copies of it in the
        // task's poll frame, under the whole actor stack.
        let run = Box::pin(task.run());
        tokio::spawn(async move {
            // A panicking actor must still leave the map (§6 #12): the
            // handle would otherwise be listed forever and attach → Closed.
            use futures::FutureExt;
            if std::panic::AssertUnwindSafe(run)
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::error!(session = %id, "session actor panicked");
            }
            host.remove_session(&id);
        });
    }

    /// Test seam: [`Self::create_session`] without spawning the actor, so a
    /// test can seed session state (e.g. a fake worker in the subagent
    /// registry) before `task.run()`. The handle is registered; the caller
    /// spawns the task.
    #[cfg(any(test, feature = "testing"))]
    pub async fn create_session_unspawned(
        self: &Arc<Self>,
        cfg: SessionConfig,
    ) -> Result<(SessionHandle, crate::session::SessionTask)> {
        SessionActor::wait_extensions_for_create(self, &cfg).await;
        let (handle, task) = SessionActor::create(self, cfg, &|_: &str, _: bool| Ok(())).await?;
        self.adopt_session(handle.clone());
        Ok((handle, task))
    }

    /// Register a handle built elsewhere (daemon test factories). The
    /// caller owns the task; `remove_session` when it ends.
    pub fn adopt_session(&self, handle: SessionHandle) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle.id.clone(), handle);
        // The daemon's crash record lists the registered sessions.
        crate::session::live::mark_dirty(false);
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

    fn alive_handles(&self) -> Vec<SessionHandle> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.values().filter(|h| h.is_alive()).cloned().collect()
    }

    /// This host's turn to start a session's endpoint
    /// (`events::registry::SessionEndpoint::start`): with the registry's file
    /// lock held it scans every session card and binds the session's socket,
    /// and a taker of that lock gives up after 100 ms ("registry lock busy").
    /// The builds of one host run at the same time, so they take turns here
    /// (waiting asynchronously, in order) and never fail on each other; the
    /// file lock and its deadline are left to other processes, as before.
    /// Held for the endpoint start alone, so the rest of each build stays
    /// parallel.
    pub(crate) async fn endpoint_start_turn(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.endpoint_start.lock().await
    }

    /// The live session whose actor id or current journal id is `id`.
    fn live_journal(&self, id: &str) -> Option<SessionHandle> {
        self.alive_handles()
            .into_iter()
            .find(|h| h.id.as_str() == id || h.journal_id() == id)
    }

    /// The live session a `continue_session` query names, from the session
    /// map alone: actor id, current journal id (a LinkedSuccessor compaction
    /// moves it off the actor id), or name (`--name` / `saveas`, which may
    /// not be on disk yet). `None` for a bare `--continue`. A restore's
    /// exact journal id ([`ContinueLookup::ExactJournalId`]) is never
    /// matched against a name.
    ///
    /// [`ContinueLookup::ExactJournalId`]: crate::session::ContinueLookup::ExactJournalId
    fn live_by_query(&self, cfg: &SessionConfig) -> Option<SessionHandle> {
        let Some(Some(q)) = &cfg.continue_session else {
            return None;
        };
        let handles = self.alive_handles();
        if let Some(h) = handles.iter().find(|h| h.id.as_str() == q || h.journal_id() == q.as_str()) {
            return Some(h.clone());
        }
        if cfg.continue_lookup == crate::session::ContinueLookup::ExactJournalId {
            return None;
        }
        handles.into_iter().find(|h| h.name().as_deref() == Some(q.as_str()))
    }

    /// Resolve a create's `continue_session` on disk with the look-up
    /// `setup::resolve_or_create_session` will use
    /// ([`crate::engine::setup::find_continue_session`]: chain → name →
    /// partial id, or a restore's exact journal id; `Some(None)` = latest by
    /// mtime; then F24: follow `compacted_into` so `--continue <old>` finds
    /// the successor), with no lock held. One that does not resolve gets no
    /// journal key: its build fails the same way.
    fn create_target(&self, cfg: &SessionConfig) -> CreateTarget {
        let resolved = cfg.continue_session.as_ref().and_then(|query| {
            let session =
                crate::engine::setup::find_continue_session(query.as_deref(), cfg.continue_lookup);
            let resolved = session.and_then(crate::core::session::follow_compaction_chain).ok()?;
            Some((resolved.session.id, resolved.session.name))
        });
        let keys = create_keys(
            cfg.continue_session.as_ref(),
            resolved.as_ref().map(|(id, name)| (id.as_str(), name.as_deref())),
            cfg.name.as_deref(),
        );
        CreateTarget {
            resolved: resolved.map(|(id, _)| id),
            keys,
        }
    }

    /// Resolve a `continue_session` request against the LIVE sessions
    /// first ([`Self::live_by_query`]), then match the journal id `target`
    /// resolved on disk ([`Self::create_target`]) against id *and* journal
    /// id. Anything that is not running here returns `None` so the real
    /// boot path builds/reports.
    ///
    /// The daemon's `Attach::Create{continue}` goes through here too
    /// (`host_factory` → `create_session`): a live journal is always one
    /// actor, never a second one on the same file / UDS / registry entry.
    fn live_continue_target(
        &self,
        cfg: &SessionConfig,
        target: &CreateTarget,
    ) -> Option<SessionHandle> {
        cfg.continue_session.as_ref()?;
        self.live_by_query(cfg)
            .or_else(|| target.resolved.as_deref().and_then(|id| self.live_journal(id)))
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
        let handle = self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        // E-P2: release any driver grants held by this session.
        if handle.is_some() {
            self.release_all_driver_grants(id);
            crate::session::live::mark_dirty(false);
        }
        handle
    }

    // ── driver single-tenancy (E-P2, §3 S2/S5) ──────────────────────────

    /// Claim the driver grant for `plugin` on behalf of `session`.
    /// Returns `Ok(())` if the grant was acquired (or is idempotent for the
    /// same session). Returns `Err` if another live session holds it.
    #[allow(dead_code)] // consumed by P3 driver_arm
    pub(crate) fn claim_driver(
        &self,
        plugin: &str,
        session: &SessionId,
    ) -> std::result::Result<(), DriverGrantError> {
        let mut grants = self.driver_grants.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = grants.get(plugin) {
            if existing != session {
                return Err(DriverGrantError {
                    plugin: plugin.into(),
                    held_by: existing.clone(),
                });
            }
            return Ok(()); // idempotent
        }
        grants.insert(plugin.into(), session.clone());
        Ok(())
    }

    /// Release the driver grant for `plugin` if held by `session`.
    #[allow(dead_code)] // consumed by P3 driver_revoke
    pub(crate) fn release_driver(&self, plugin: &str, session: &SessionId) {
        let mut grants = self.driver_grants.lock().unwrap_or_else(|e| e.into_inner());
        if grants.get(plugin).is_some_and(|s| s == session) {
            grants.remove(plugin);
        }
    }

    /// Release all driver grants held by `session` (called from `remove_session`).
    fn release_all_driver_grants(&self, session: &SessionId) {
        let mut grants = self.driver_grants.lock().unwrap_or_else(|e| e.into_inner());
        grants.retain(|_, s| s != session);
    }

    /// Clear all driver grants (called on reload — §3 S2/S5: plugin process
    /// died with the old image).
    #[allow(dead_code)] // consumed by reload path in P3+
    pub(crate) fn clear_all_driver_grants(&self) {
        self.driver_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    // ── shared extension host (Phase 2, C) ────────────────────────────────

    /// Loader seam (C2): a discovery pass for this host's manager has been
    /// dispatched. Called synchronously by `spawn_discover_and_load` before
    /// its task runs, so `extensions_ready()` cannot slip through the gap.
    pub fn note_extensions_loading(&self) {
        self.extensions_loading
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// True when no extension discovery is running on this host: it
    /// finished, or none was dispatched (a host without extensions). The
    /// plugin providers listed now are then final. A caller that holds the
    /// manager can also ask `discovery_done()`.
    pub fn extensions_settled(&self) -> bool {
        *self.extensions_ready.borrow()
            || !self
                .extensions_loading
                .load(std::sync::atomic::Ordering::Acquire)
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

/// Print `config.warnings` to stderr and the log (headless hosts).
pub fn report_config_warnings(config: &crate::SynapsConfig) {
    for warning in &config.warnings {
        tracing::warn!(%warning, "config warning");
        // Never `eprintln!`: a detached daemon's stderr may have no reader
        // left, and that panic killed reloaded daemons right here.
        agent_core::eprintln_lossy!("synaps: config warning: {warning}");
    }
}

/// Take what `m` holds without ever blocking: `None` when another thread
/// holds the lock (or it is already empty). A poisoned lock still yields
/// its value — a panic mid-flush must not keep the guard alive.
pub(crate) fn take_without_blocking<T>(m: &std::sync::Mutex<Option<T>>) -> Option<T> {
    match m.try_lock() {
        Ok(mut held) => held.take(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().take(),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod take_without_blocking_tests {
    use super::take_without_blocking;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn takes_the_value_once() {
        let m = Mutex::new(Some(7));
        assert_eq!(take_without_blocking(&m), Some(7));
        assert_eq!(take_without_blocking(&m), None);
    }

    #[test]
    fn returns_at_once_while_another_thread_holds_the_lock() {
        let m = Arc::new(Mutex::new(Some(7)));
        let held = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let holder = {
            let (m, held, release) = (Arc::clone(&m), Arc::clone(&held), Arc::clone(&release));
            std::thread::spawn(move || {
                let _g = m.lock().unwrap();
                held.wait();
                release.wait();
            })
        };
        held.wait();
        let t = Instant::now();
        assert_eq!(take_without_blocking(&m), None, "took a value from a held lock");
        assert!(t.elapsed() < Duration::from_secs(1), "blocked {:?}", t.elapsed());
        release.wait();
        holder.join().unwrap();
        assert_eq!(take_without_blocking(&m), Some(7), "the holder's value is still there");
    }

    #[test]
    fn a_poisoned_lock_still_yields_its_value() {
        let m = Arc::new(Mutex::new(Some(7)));
        let poisoner = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison the guard mutex");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(take_without_blocking(&m), Some(7));
    }
}

#[cfg(test)]
mod driver_grant_tests {
    use super::*;

    // Since EngineHost has many private fields we cannot construct in a unit
    // test without the full boot path, test the grant logic on a standalone
    // map that mirrors the real implementation exactly.

    type Grants = std::sync::Mutex<std::collections::HashMap<String, SessionId>>;

    fn claim(grants: &Grants, plugin: &str, session: &SessionId) -> std::result::Result<(), DriverGrantError> {
        let mut g = grants.lock().unwrap();
        if let Some(existing) = g.get(plugin) {
            if existing != session {
                return Err(DriverGrantError {
                    plugin: plugin.into(),
                    held_by: existing.clone(),
                });
            }
            return Ok(());
        }
        g.insert(plugin.into(), session.clone());
        Ok(())
    }

    fn release(grants: &Grants, plugin: &str, session: &SessionId) {
        let mut g = grants.lock().unwrap();
        if g.get(plugin).is_some_and(|s| s == session) {
            g.remove(plugin);
        }
    }

    fn release_all(grants: &Grants, session: &SessionId) {
        grants.lock().unwrap().retain(|_, s| s != session);
    }

    fn clear_all(grants: &Grants) {
        grants.lock().unwrap().clear();
    }

    fn sid(s: &str) -> SessionId {
        SessionId::from(s)
    }

    #[test]
    fn single_tenancy_second_session_refused() {
        let grants: Grants = Default::default();
        let a = sid("session-a");
        let b = sid("session-b");
        claim(&grants, "autonomous", &a).unwrap();
        let err = claim(&grants, "autonomous", &b).unwrap_err();
        assert_eq!(err.plugin, "autonomous");
        assert_eq!(err.held_by, a);
    }

    #[test]
    fn single_tenancy_released_on_end() {
        let grants: Grants = Default::default();
        let a = sid("session-a");
        let b = sid("session-b");
        claim(&grants, "autonomous", &a).unwrap();
        release(&grants, "autonomous", &a);
        // Now session B can claim it.
        claim(&grants, "autonomous", &b).unwrap();
    }

    #[test]
    fn single_tenancy_same_session_idempotent() {
        let grants: Grants = Default::default();
        let a = sid("session-a");
        claim(&grants, "autonomous", &a).unwrap();
        // Same session, same plugin — idempotent.
        claim(&grants, "autonomous", &a).unwrap();
        assert_eq!(grants.lock().unwrap().len(), 1);
    }

    #[test]
    fn grants_cleared_on_reload_record() {
        let grants: Grants = Default::default();
        let a = sid("session-a");
        let b = sid("session-b");
        claim(&grants, "plugin1", &a).unwrap();
        claim(&grants, "plugin2", &b).unwrap();
        assert_eq!(grants.lock().unwrap().len(), 2);
        // Simulated reload: all grants cleared.
        clear_all(&grants);
        assert!(grants.lock().unwrap().is_empty());
        // Both can re-claim.
        claim(&grants, "plugin1", &a).unwrap();
        claim(&grants, "plugin2", &b).unwrap();
    }

    #[test]
    fn release_all_for_session_leaves_other_sessions() {
        let grants: Grants = Default::default();
        let a = sid("session-a");
        let b = sid("session-b");
        claim(&grants, "plugin1", &a).unwrap();
        claim(&grants, "plugin2", &b).unwrap();
        release_all(&grants, &a);
        assert_eq!(grants.lock().unwrap().len(), 1);
        assert!(grants.lock().unwrap().contains_key("plugin2"));
    }
}

/// The create gate on its own (no host): which create reserves, which
/// waits, which attaches, and that every way a reservation ends frees it.
#[cfg(test)]
mod create_gate_tests {
    use super::*;
    use std::time::Duration;

    fn j(id: &str) -> CreateKey {
        CreateKey::Journal(id.into())
    }

    fn n(name: &str) -> CreateKey {
        CreateKey::Name(name.into())
    }

    fn reserved<'g>(c: Claim<'g, u8>) -> CreateReservation<'g> {
        match c {
            Claim::Reserved(r) => r,
            Claim::Busy(_) => panic!("busy, want reserved"),
            Claim::Live(_) => panic!("live, want reserved"),
        }
    }

    fn busy(c: Claim<'_, u8>) -> tokio::sync::watch::Receiver<()> {
        match c {
            Claim::Busy(done) => done,
            Claim::Reserved(_) => panic!("reserved, want busy"),
            Claim::Live(_) => panic!("live, want busy"),
        }
    }

    fn free() -> Option<u8> {
        None
    }

    #[tokio::test]
    async fn a_held_key_makes_the_next_claim_wait_until_the_reservation_ends() {
        let gate = CreateGate::default();
        let first = reserved(gate.claim(&[j("s1"), n("one")], free));
        // Any one shared key is enough.
        let mut done = busy(gate.claim(&[n("other"), j("s1")], free));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), done.changed()).await.is_err(),
            "woke while the key is still held"
        );
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), done.changed())
            .await
            .expect("woken when the reservation ended")
            .expect_err("the channel closed: no value is ever sent");
        let _again = reserved(gate.claim(&[j("s1")], free));
    }

    #[tokio::test]
    async fn the_end_of_a_reservation_is_not_missed_by_a_waiter_that_looks_late() {
        let gate = CreateGate::default();
        let first = reserved(gate.claim(&[j("s1")], free));
        let mut done = busy(gate.claim(&[j("s1")], free));
        drop(first);
        // The waiter awaits only now, after the end.
        tokio::time::timeout(Duration::from_secs(1), done.changed())
            .await
            .expect("an end before the await is still seen")
            .expect_err("closed");
    }

    #[test]
    fn creates_of_different_sessions_reserve_at_once() {
        let gate = CreateGate::default();
        let _a = reserved(gate.claim(&[j("a"), n("x")], free));
        let _b = reserved(gate.claim(&[j("b"), n("y")], free));
        let _new = reserved(gate.claim(&[], free));
        let _new_too = reserved(gate.claim(&[], free));
    }

    #[test]
    fn a_live_target_is_handed_back_and_nothing_is_reserved() {
        let gate = CreateGate::default();
        assert!(matches!(gate.claim(&[j("s1")], || Some(7u8)), Claim::Live(7)));
        let _r = reserved(gate.claim(&[j("s1")], free));
        // A held key goes first: the holder registers its actor before it
        // lets go, so the waiter then sees it live.
        let _ = busy(gate.claim(&[j("s1")], || Some(7u8)));
    }

    #[tokio::test]
    async fn a_reservation_dropped_by_a_panic_frees_its_keys() {
        let gate = CreateGate::default();
        let mut waiter = None;
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = reserved(gate.claim(&[j("p")], free));
            waiter = Some(busy(gate.claim(&[j("p")], free)));
            panic!("the build panics");
        }));
        assert!(unwound.is_err());
        tokio::time::timeout(Duration::from_secs(1), waiter.unwrap().changed())
            .await
            .expect("the waiter is woken by the unwind")
            .expect_err("closed");
        let _r = reserved(gate.claim(&[j("p")], free));
    }

    #[test]
    fn a_poisoned_gate_still_reserves_and_frees() {
        let gate = std::sync::Arc::new(CreateGate::default());
        let poisoner = std::sync::Arc::clone(&gate);
        let _ = std::thread::spawn(move || {
            let _g = poisoner.pending.lock().unwrap();
            panic!("poison the gate");
        })
        .join();
        assert!(gate.pending.is_poisoned());
        drop(reserved(gate.claim(&[j("s1")], free)));
        let _r = reserved(gate.claim(&[j("s1")], free));
    }

    #[test]
    fn the_build_confirms_the_id_it_resolved() {
        let gate = CreateGate::default();
        let r = reserved(gate.claim(&[j("s1")], free));
        // Its own key.
        r.confirm("s1", false, |_| panic!("not asked: the key is its own")).unwrap();
        assert!(r.take_race().is_none());
        // Resolved elsewhere, and that id is free: it becomes a key.
        r.confirm("s2", false, |_| false).unwrap();
        let _ = busy(gate.claim(&[j("s2")], free));
        // Live already: attach instead.
        assert!(r.confirm("s3", false, |_| true).is_err());
        assert!(matches!(r.take_race(), Some(CreateRace::Live)));
        // Held by another create: wait for it.
        let other = reserved(gate.claim(&[j("s4")], free));
        let err = r.confirm("s4", false, |_| false).unwrap_err();
        assert!(err.to_string().contains("session s4 is being created by another create"), "{err}");
        let Some(CreateRace::Busy(done)) = r.take_race() else { panic!("want Busy") };
        drop(other);
        assert!(done.has_changed().is_err(), "the other create's end closes the channel");
        // The added key is freed with the reservation.
        drop(r);
        let _s2 = reserved(gate.claim(&[j("s1"), j("s2")], free));
    }

    #[test]
    fn a_new_session_id_that_is_taken_is_built_again() {
        let gate = CreateGate::default();
        let r = reserved(gate.claim(&[], free));
        assert!(r.confirm("n1", true, |_| true).is_err(), "live or on disk");
        assert!(matches!(r.take_race(), Some(CreateRace::TakenId)));
        let _held = reserved(gate.claim(&[j("n2")], free));
        assert!(r.confirm("n2", true, |_| false).is_err(), "held by another create");
        assert!(matches!(r.take_race(), Some(CreateRace::TakenId)));
        r.confirm("n3", true, |_| false).unwrap();
        let _ = busy(gate.claim(&[j("n3")], free));
    }

    #[test]
    fn the_keys_of_a_create() {
        let q = Some(Some("q".to_string()));
        assert_eq!(
            create_keys(q.as_ref(), Some(("20261019-1", Some("nm"))), Some(" set ")),
            [n("q"), j("20261019-1"), n("nm"), n("set")]
        );
        // A query that is the session's own name is one key.
        let by_name = Some(Some("nm".to_string()));
        assert_eq!(create_keys(by_name.as_ref(), Some(("20261019-1", Some("nm"))), None), [n("nm"), j("20261019-1")]);
        // Bare `--continue`: the journal it resolves to.
        assert_eq!(create_keys(Some(&None), Some(("20261019-1", None)), None), [j("20261019-1")]);
        // A query that resolves to nothing on disk: still its name (another
        // create may be naming a session so right now).
        assert_eq!(create_keys(q.as_ref(), None, None), [n("q")]);
        // A new session: its `--name`, or nothing.
        assert_eq!(create_keys(None, None, Some("m")), [n("m")]);
        assert!(create_keys(None, None, Some("  ")).is_empty());
        assert!(create_keys(None, None, None).is_empty());
    }
}
