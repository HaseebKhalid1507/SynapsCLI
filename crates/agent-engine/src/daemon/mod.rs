//! Daemon front-end (`synaps daemon`): UDS listener, per-connection pump,
//! registry file, lifecycle (PLAN-phase2 §2.11). Lives in `agent-engine`
//! (S289 D-1 amended) — `agent-tui` must never appear in this graph.
//!
//! On by default: the first `--attach`/`attach` client auto-spawns the
//! daemon ([`ensure_running`]); `SYNAPS_DAEMON=0` disables every daemon
//! feature (exit 3 with a one-line reason). Safety properties that are
//! NEVER stubbed: the flock liveness oracle, the ready-fd pipe, socket
//! perms 0700/0600, the spawn lock (one spawner per registry dir).

pub mod close;
pub mod conn;
pub mod extension_reload;
pub mod lifecycle;
pub mod listener;
pub mod pin;
pub mod pins;
pub mod registry;
pub mod reload;
pub mod rename;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::session::{SessionConfig, SessionHandle, SessionId, SessionMeta};
use crate::EngineHost;
use registry::{DaemonInfo, DaemonLock, DaemonPaths};

/// Exit code when the daemon refuses to start (flag off / legacy MCP).
pub const EXIT_REFUSED: i32 = 3;
/// Exit code for a protocol/version refusal on the client side.
pub const EXIT_VERSION: i32 = 2;
/// The daemon was lost (SIGKILL / crash) and reconnection failed within the
/// budget (`SYNAPS_TUI_ATTACH_RECONNECT_SECS`).
pub const EXIT_DAEMON_LOST: i32 = 4;
/// Env var carrying the write end of the ready pipe to a spawned daemon.
pub const READY_FD_ENV: &str = "SYNAPS_DAEMON_READY_FD";
/// Upper bound `run_foreground` waits for extension discovery before it
/// starts accepting and signals ready (sidecars spawn once per daemon, and
/// early sessions must see extension tools/hooks).
pub const EXTENSION_DISCOVERY_WAIT: Duration = Duration::from_secs(10);
/// Headroom for the rest of the daemon's pre-ready work (host boot with
/// cached MCP descriptors only, bind, registry files) and a loaded machine.
const SPAWN_READY_MARGIN: Duration = Duration::from_secs(5);
/// How long `spawn_detached` waits for the child's ready byte. Strictly
/// greater than the daemon's bounded pre-ready work: the extension wait plus
/// a margin. MCP discovery never counts here — the daemon runs it in the
/// background after it is accepting ([`crate::McpDiscovery::Background`]).
pub const SPAWN_READY_TIMEOUT: Duration =
    Duration::from_secs(EXTENSION_DISCOVERY_WAIT.as_secs() + SPAWN_READY_MARGIN.as_secs());
// The spawner must outwait the daemon's bounded pre-ready work, or one slow
// extension walk makes every auto-start fail (killed before `R`).
const _: () = assert!(
    SPAWN_READY_TIMEOUT.as_secs() > EXTENSION_DISCOVERY_WAIT.as_secs(),
    "SPAWN_READY_TIMEOUT must exceed EXTENSION_DISCOVERY_WAIT"
);

/// `SYNAPS_DAEMON` feature flag: ON unless set to `0`/`false`/`off`/`no`.
/// `SYNAPS_DAEMON=1` is accepted (no-op, kept for older scripts).
pub fn enabled() -> bool {
    enabled_from(std::env::var("SYNAPS_DAEMON").ok().as_deref())
}

fn env_is_off(v: Option<&str>) -> bool {
    matches!(v.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("0" | "false" | "off" | "no"))
}

fn enabled_from(v: Option<&str>) -> bool {
    !env_is_off(v)
}

/// `SYNAPS_DAEMON_AUTOSPAWN`: ON unless `0`/`false`/`off`/`no`. Off means
/// an attach with no live daemon fails with the "no daemon running" message
/// instead of starting one.
pub fn autospawn_enabled() -> bool {
    !env_is_off(std::env::var("SYNAPS_DAEMON_AUTOSPAWN").ok().as_deref())
}

/// One-line reason used by every attach path when the flag is off.
pub const DISABLED_NOTICE: &str = "daemon disabled by SYNAPS_DAEMON=0";

/// A client running `client` profile must never drive a daemon that serves
/// another one: its sessions, model and config writes would land in the
/// wrong profile. The socket is per profile, but its stem is sanitised
/// (`a.b` and `a_b` share `daemon-a_b.sock`), so the handshake's
/// `Welcome.profile` is the authority. `None` and `Some("")` both mean the
/// default profile. Returns the refusal message on a mismatch.
pub fn profile_mismatch(daemon: Option<&str>, client: Option<&str>) -> Option<String> {
    let norm = |p: Option<&str>| p.filter(|s| !s.is_empty()).map(str::to_owned);
    let (daemon, client) = (norm(daemon), norm(client));
    if daemon == client {
        return None;
    }
    let name = |p: &Option<String>| match p {
        Some(p) => format!("profile `{p}`"),
        None => "the default profile".to_string(),
    };
    let stop = match &daemon {
        Some(p) => format!("synaps --profile {p} daemon stop"),
        None => "synaps daemon stop".to_string(),
    };
    Some(format!(
        "refusing to attach: this daemon serves {}, but this client runs {} — \
         stop it ({stop}) or use the matching --profile",
        name(&daemon),
        name(&client),
    ))
}

/// Builds a session for `Attach::Create`: `host_factory` (real actor) or
/// `echo_factory` (tests).
pub type SessionFactory = Arc<
    dyn Fn(SessionConfig) -> Pin<Box<dyn Future<Output = Result<SessionHandle, String>> + Send>>
        + Send
        + Sync,
>;

/// EchoActor-backed factory (B's socket tests before A1).
#[cfg(any(test, feature = "testing"))]
pub fn echo_factory() -> SessionFactory {
    Arc::new(|cfg: SessionConfig| {
        Box::pin(async move {
            let id = SessionId(format!(
                "echo-{}-{}",
                chrono::Utc::now().format("%Y%m%d-%H%M%S%3f"),
                uuid::Uuid::new_v4().simple()
            ));
            let (handle, _task) = SessionHandle::echo_for_test(id);
            let _ = cfg;
            Ok(handle)
        })
    })
}

/// The default factory for a booted host: `EngineHost::create_session` (A1) —
/// the real `SessionActor` owning one `Runtime` + `ConversationState`.
pub fn host_factory(host: &Arc<EngineHost>) -> SessionFactory {
    let host = Arc::clone(host);
    Arc::new(move |cfg: SessionConfig| {
        let host = Arc::clone(&host);
        Box::pin(async move { host.create_session(cfg).await.map_err(|e| e.to_string()) })
    })
}

#[derive(Clone, Default)]
pub struct DaemonOpts {
    /// Override the socket path (the lock/json/pid stay under `registry_dir()`).
    pub socket: Option<PathBuf>,
    pub profile: Option<String>,
    /// Exit after this long with zero connections, no pinned session and no
    /// session running a turn (idle, client-less sessions are saved on exit;
    /// `--continue` brings them back). A turn in flight or a pin always
    /// blocks the exit.
    pub idle_exit: Option<Duration>,
    /// Deprecated, ignored: legacy MCP was removed (every mode uses
    /// per-session MCP leases). Still forwarded so an older client can spawn
    /// a newer daemon.
    pub allow_legacy_mcp: bool,
    /// Test seam: `registry_dir()` replacement.
    pub runtime_dir: Option<PathBuf>,
    /// Test seam / A1 hook: how `Attach::Create` builds a session.
    pub factory: Option<SessionFactory>,
}

impl DaemonOpts {
    pub fn paths(&self) -> DaemonPaths {
        let mut p = match &self.runtime_dir {
            Some(d) => registry::daemon_paths_in(d, self.profile.as_deref()),
            None => registry::daemon_paths(self.profile.as_deref()),
        };
        if let Some(s) = &self.socket {
            p.sock = s.clone();
        }
        p
    }
}

/// Shared by the accept loop and every connection.
pub struct DaemonState {
    pub host: Arc<EngineHost>,
    pub paths: DaemonPaths,
    pub profile: Option<String>,
    pub started: Instant,
    pub factory: SessionFactory,
    pub connections: AtomicUsize,
    pub shutdown: CancellationToken,
    pub force_shutdown: AtomicBool,
    // ── C3 reload ──
    /// Reload counter (`Welcome.generation`; starts at 1).
    pub generation: u64,
    /// Drain: refuse `Attach::Create` / new turns while set.
    pub reloading: AtomicBool,
    /// Generation announced in `Reloading`/`Bye{Reloading}`.
    pub reload_generation: std::sync::atomic::AtomicU64,
    /// Cancelled once per reload attempt: every conn sends `Reloading` +
    /// `Bye`. Replaced with a fresh token when the exec fails (M4) so the
    /// surviving image accepts connections again — read via `announce()`.
    pub reload_announce: Mutex<CancellationToken>,
    /// The flock, held here (not on `Daemon`) so `reload` can hand its fd
    /// to the next image.
    pub lock_fd: Mutex<Option<DaemonLock>>,
    /// `old id → new id` for one generation (ids differ only if a
    /// LinkedSuccessor compaction happened since boot).
    pub reload_aliases: Mutex<HashMap<String, String>>,
    /// The `daemon.json` record this image wrote, and its writes. A Ping
    /// that finds the file missing or naming another pid puts it back
    /// ([`Self::heal_daemon_json`]); reload takes its recorded exe from here.
    pub meta: registry::Metadata,
    /// The pin book: pinned journals that come back on the next cold start.
    pub pins: Arc<pins::PinBook>,
    /// A cold start's pin restore is running (holds the idle exit).
    pub restoring: AtomicBool,
}

impl DaemonState {
    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// The current reload-announce token (clone; select on `.cancelled()`).
    pub fn announce(&self) -> CancellationToken {
        self.reload_announce
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Fire the announce (every conn says `Reloading` + `Bye`).
    pub fn fire_announce(&self) {
        self.announce().cancel();
    }

    /// Exec failed: arm a fresh token so new connections are served again.
    pub fn reset_announce(&self) {
        *self.reload_announce.lock().unwrap_or_else(|e| e.into_inner()) = CancellationToken::new();
    }

    /// The `daemon.json` record this image holds (a copy).
    pub fn daemon_info(&self) -> DaemonInfo {
        self.meta.info()
    }

    /// Ping: if `daemon.json` is missing or names another pid, write this
    /// image's record back. Only while this image holds the flock and has
    /// not begun to shut down; the record comes from memory, never from
    /// disk. One INFO line per [`JSON_HEAL_LOG_EVERY`].
    ///
    /// The shutdown check runs under the `lock_fd` mutex: [`Daemon::wait`]
    /// cancels `shutdown` before it takes that mutex to unlink the files, so
    /// a Ping still in flight at shutdown can't write the file back after
    /// the unlink. During a reload's hand-off the record already names the
    /// new generation and exe, so a heal there writes what the reload wrote.
    pub fn heal_daemon_json(&self) {
        let lock = self.lock_fd.lock().unwrap_or_else(|e| e.into_inner());
        let Some(lock) = lock.as_ref() else {
            return;
        };
        if self.shutdown.is_cancelled() {
            return;
        }
        match self.meta.heal(lock) {
            Ok(registry::Healed::Yes { info, log: Some(heals) }) => tracing::info!(
                path = %self.paths.json.display(),
                pid = info.pid,
                generation = info.generation,
                heals,
                "daemon: daemon.json rewritten (it was missing or named another pid)"
            ),
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "daemon: daemon.json heal failed"),
        }
    }

    /// B4: ONE session map — `EngineHost` owns it; these delegate.
    pub fn live_sessions(&self) -> Vec<SessionHandle> {
        self.host.session_handles()
    }

    /// Listing metas with lifecycle / clients / input_owner / journal_id
    /// (B4: `EngineHost::sessions()` fills the handle cells).
    pub fn session_metas(&self) -> Vec<SessionMeta> {
        self.host.sessions()
    }

    /// Live handle by id; falls back to the C3 reload alias
    /// (`old id → new id`, set when a LinkedSuccessor compaction happened
    /// since boot) so clients reconnecting after `daemon reload` find
    /// their session under the id they last saw.
    pub fn attach(&self, id: &SessionId) -> Option<SessionHandle> {
        self.host.attach(id).filter(|h| h.is_alive()).or_else(|| {
            let alias = self
                .reload_aliases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id.as_str())
                .cloned()?;
            self.host
                .attach(&SessionId::from(alias.as_str()))
                .filter(|h| h.is_alive())
        })
    }

    pub fn insert(&self, handle: SessionHandle) {
        self.host.adopt_session(handle);
    }

    pub fn remove(&self, id: &SessionId) {
        self.host.remove_session(id);
    }

    pub async fn create(&self, cfg: SessionConfig) -> Result<SessionHandle, String> {
        let handle = (self.factory)(cfg).await?;
        self.insert(handle.clone());
        Ok(handle)
    }

    /// F23: a lock-held placeholder (`meta.locked_by.is_some()`) is retried on
    /// every attach. Pull it out of the map first — otherwise the host's
    /// attach-if-live short-circuit hands the placeholder straight back —
    /// and re-run the real `create` from the journal. Success replaces the
    /// placeholder under the same id; still-held puts it back and the caller
    /// attaches to it (→ `AttachRefused` naming the holder).
    pub async fn retry_locked_placeholder(&self, placeholder: SessionHandle) -> SessionHandle {
        let cfg = SessionConfig {
            continue_session: Some(Some(placeholder.id.as_str().to_string())),
            ..placeholder.placeholder_config().cloned().unwrap_or_default()
        };
        self.remove(&placeholder.id);
        match (self.factory)(cfg).await {
            Ok(real) if real.id == placeholder.id || real.journal_id() == placeholder.id.as_str() => {
                tracing::info!(session = %placeholder.id, "daemon: locked journal released — placeholder replaced by the real session");
                self.insert(real.clone());
                real
            }
            Ok(other) => {
                // Journal resolved somewhere else (e.g. compacted_into follow): keep it, drop the placeholder.
                tracing::warn!(session = %placeholder.id, resolved = %other.id, "daemon: placeholder retry resolved to a different id");
                self.insert(other.clone());
                other
            }
            Err(e) => {
                tracing::debug!(session = %placeholder.id, error = %e, "daemon: journal still locked; keeping placeholder");
                self.insert(placeholder.clone());
                placeholder
            }
        }
    }

    pub fn request_shutdown(&self, force: bool) {
        if force {
            self.force_shutdown.store(true, Ordering::SeqCst);
        }
        self.shutdown.cancel();
    }
}

/// A running daemon (bound socket, lock held, accept loop spawned).
pub struct Daemon {
    pub paths: DaemonPaths,
    pub state: Arc<DaemonState>,
    accept: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// Reap stale files, take the flock, bind (dir 0700 / sock 0600), write
    /// `daemon.json`, spawn the accept loop, signal the ready fd if any.
    pub async fn start(host: Arc<EngineHost>, opts: DaemonOpts) -> anyhow::Result<Self> {
        let paths = opts.paths();
        std::fs::create_dir_all(&paths.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // C3: after `reload` the new image ADOPTS the inherited flock and
        // skips `reap_stale` (the old image's files are ours).
        let adopted = reload::adopt_from_env()?;
        let (lock, reload_state) = match adopted {
            Some((lock, rs, rs_path)) => {
                let _ = std::fs::remove_file(&rs_path);
                (lock, Some(rs))
            }
            None => {
                registry::reap_stale(&paths);
                let lock = match DaemonLock::try_acquire(&paths)? {
                    Some(l) => l,
                    None => {
                        let who = registry::read_daemon_json(&paths).map(|i| i.pid);
                        anyhow::bail!(
                            "another daemon holds {} (pid {})",
                            paths.lock.display(),
                            who.map_or("unknown".to_string(), |p| p.to_string())
                        );
                    }
                };
                (lock, None)
            }
        };
        let generation = reload_state.as_ref().map_or(1, |rs| rs.generation);
        let listener = listener::bind(&paths.sock)?;

        let info = DaemonInfo {
            pid: std::process::id(),
            protocol_version: crate::session::wire::PROTOCOL_VERSION,
            daemon_version: crate::session::wire::binary_version(),
            profile: opts.profile.clone(),
            started_at: chrono::Utc::now(),
            socket: paths.sock.to_string_lossy().into_owned(),
            exe: registry::read_daemon_json(&paths).and_then(|i| i.exe).or_else(|| Some(reload::resolve_exe())),
            generation,
        };
        let meta = registry::Metadata::start(paths.clone(), info.clone())?;

        let factory = opts.factory.clone().unwrap_or_else(|| host_factory(&host));
        let state = Arc::new(DaemonState {
            host,
            paths: paths.clone(),
            profile: opts.profile.clone(),
            started: Instant::now(),
            factory,
            connections: AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
            force_shutdown: AtomicBool::new(false),
            generation,
            reloading: AtomicBool::new(false),
            reload_generation: std::sync::atomic::AtomicU64::new(generation),
            reload_announce: Mutex::new(CancellationToken::new()),
            lock_fd: Mutex::new(Some(lock)),
            reload_aliases: Mutex::new(HashMap::new()),
            meta,
            pins: Arc::new(pins::PinBook::for_active_profile()),
            restoring: AtomicBool::new(false),
        });
        state.host.set_pin_sink(Arc::clone(&state.pins) as Arc<dyn crate::session::PinSink>);

        // Before any session of this image registers: drop registrations no
        // running session owns (dead owner pid). After a reload, also every
        // one naming our own pid: the old image exec'd without ending its
        // sessions, and the ones still live re-register when they rehydrate
        // below, under their current (e.g. post-compaction) id. On a fresh
        // start our pid may already own live registrations (a daemon hosted
        // in a process that runs other sessions), so it is left alone.
        let own_pid = reload_state.as_ref().map(|_| info.pid);
        let swept = crate::events::registry::sweep_stale_registrations(own_pid);
        if swept > 0 {
            tracing::info!(swept, "daemon: removed stale session registrations");
        }

        // C3: rehydrate BEFORE accepting so a reconnecting client finds its session.
        if let Some(rs) = &reload_state {
            reload::rehydrate(&state, rs).await;
        }

        let accept = tokio::spawn(listener::accept_loop(Arc::clone(&state), listener, state.shutdown.clone()));
        if let Some(idle) = opts.idle_exit {
            tokio::spawn(idle_monitor(Arc::clone(&state), idle));
        }
        signal_ready();
        // Cold start only: a reload hands its live sessions over already.
        if reload_state.is_none() && pins::restore_enabled() {
            tokio::spawn(pins::restore_all(Arc::clone(&state)));
        }
        tracing::info!(sock = %paths.sock.display(), pid = info.pid, generation, "daemon: listening");
        Ok(Self { paths, state, accept })
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.state.shutdown.clone()
    }

    /// Block until shutdown is requested (frame, signal, idle), then end
    /// every session inside one budget and unlink the files.
    pub async fn wait(self) {
        self.state.shutdown.cancelled().await;
        let force = self.state.force_shutdown.load(Ordering::SeqCst);
        lifecycle::shutdown_all(&self.state, force).await;
        self.accept.abort();
        let _ = self.accept.await;
        crate::events::socket::cleanup_socket(&self.paths.sock.to_string_lossy());
        {
            let lock = self.state.lock_fd.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lock) = lock.as_ref() {
                self.state.meta.cleanup(lock);
            }
        }
        tracing::info!("daemon: stopped");
        // Release the flock last.
        drop(self.state.lock_fd.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

/// How long a `Status` probe may take before the session counts as busy.
const IDLE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether `handle`'s actor is between turns. Asks `Query{Status}` under the
/// reserved [`crate::session::wire::IDLE_PROBE_QUERY_ID`] (transports swallow
/// it) and reads `streaming`. A no-answer inside [`IDLE_PROBE_TIMEOUT`] —
/// the actor is inside `compact()`/preflight, or its queue is full — counts
/// as busy: the daemon never exits under a running turn (jcode mistake #1).
pub async fn session_is_idle(handle: &SessionHandle) -> bool {
    use crate::session::wire::IDLE_PROBE_QUERY_ID;
    use crate::session::{SessionCommand, SessionEventWire, SessionLifecycle, SessionQuery};
    // B3: a parked session is idle by construction — no probe, no wake.
    if handle.lifecycle() == SessionLifecycle::Parked {
        return true;
    }
    let mut rx = handle.subscribe();
    if handle.send(SessionCommand::Query { id: IDLE_PROBE_QUERY_ID, query: SessionQuery::Status }).await.is_err() {
        return false;
    }
    let probe = async {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let SessionEventWire::QueryResult { id, value } = env.event {
                        if id == IDLE_PROBE_QUERY_ID {
                            // Real actor: {"streaming": bool, "pending_prompts": n}. A backend that
                            // does not implement Status (echo) has no turns to protect.
                            let streaming = value.get("streaming").and_then(|v| v.as_bool()).unwrap_or(false);
                            let prompts = value.get("pending_prompts").and_then(|v| v.as_u64()).unwrap_or(0);
                            return !streaming && prompts == 0;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return true,
            }
        }
    };
    tokio::time::timeout(IDLE_PROBE_TIMEOUT, probe).await.unwrap_or(false)
}

/// Idle = zero connections, no pinned session AND every live session
/// between turns, held for `idle` continuously. Clients and pins count
/// first so a probe is never sent while someone is attached. A pin holds
/// the exit because exiting would end the session the user asked to keep.
pub async fn daemon_is_idle(state: &DaemonState) -> bool {
    if state.connections.load(Ordering::SeqCst) > 0 {
        return false;
    }
    if state.restoring.load(Ordering::SeqCst) {
        return false;
    }
    let sessions = state.live_sessions();
    if sessions.iter().any(|h| h.pinned() && h.is_alive()) {
        return false;
    }
    for h in sessions {
        if !session_is_idle(&h).await {
            return false;
        }
    }
    true
}

async fn idle_monitor(state: Arc<DaemonState>, idle: Duration) {
    let mut idle_since: Option<Instant> = None;
    let tick = Duration::from_secs(5).min(idle.max(Duration::from_millis(100)));
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = tokio::time::sleep(tick) => {}
        }
        if !daemon_is_idle(&state).await {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= idle {
            tracing::info!("daemon: idle for {:?}, exiting", idle);
            state.request_shutdown(false);
            return;
        }
    }
}

/// Child side of the ready pipe: write `R` to `SYNAPS_DAEMON_READY_FD` (if
/// set), close it, and scrub the env so tool subprocesses do not inherit it.
pub fn signal_ready() {
    #[cfg(unix)]
    {
        let Some(fd) = std::env::var(READY_FD_ENV).ok().and_then(|s| s.parse::<i32>().ok()) else {
            return;
        };
        std::env::remove_var(READY_FD_ENV);
        // SAFETY: fd is the write end handed to us by the parent; we own it.
        unsafe {
            let _ = libc::write(fd, b"R".as_ptr() as *const libc::c_void, 1);
            libc::close(fd);
        }
    }
}

/// Parent side: `current_exe daemon --foreground …` in its own session
/// (`setsid`), stdout → null, stderr → pipe; waits ≤ [`SPAWN_READY_TIMEOUT`]
/// (15 s: the daemon's ≤ 10 s extension wait plus margin) for `R` on the
/// ready pipe. EOF before `R` = child died → error with its stderr tail.
#[cfg(unix)]
pub fn spawn_detached(opts: &DaemonOpts) -> anyhow::Result<u32> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()?;


    let mut cmd_args: Vec<std::ffi::OsString> = vec!["daemon".into(), "--foreground".into()];
    if let Some(p) = &opts.profile {
        cmd_args.extend(["--profile".into(), p.into()]);
    }
    if let Some(s) = &opts.socket {
        cmd_args.extend(["--socket".into(), s.into()]);
    }
    if let Some(i) = opts.idle_exit {
        cmd_args.extend(["--idle-exit".into(), i.as_secs().to_string().into()]);
    }
    if opts.allow_legacy_mcp {
        cmd_args.push("--allow-legacy-mcp".into());
    }
    // Scope wrapping execs in place: PID, ready FD and stdio remain intact.
    // Separate scopes do not protect against ancestor/global OOM.
    let mut effective_env = crate::process_scope::inherited_env();
    crate::process_scope::set_env(&mut effective_env, "SYNAPS_DAEMON", "1");
    effective_env.retain(|(k, _)| k != READY_FD_ENV);
    let (program, cmd_args) = match crate::process_scope::launcher_for_effective_env(&effective_env, None) {
        Some(launcher) => launcher.wrap("synaps daemon", exe, cmd_args),
        None => (exe.into_os_string(), cmd_args),
    };
    let mut fds = [0i32; 2];
    // SAFETY: plain pipe(2) on a valid 2-slot array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // Probe has finished before readiness capabilities exist.
    for fd in [read_fd, write_fd] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(read_fd); libc::close(write_fd); }
            return Err(error.into());
        }
    }
    crate::process_scope::set_env(&mut effective_env, READY_FD_ENV, write_fd.to_string());
    let mut cmd = std::process::Command::new(program);
    cmd.args(cmd_args);
    cmd.env_clear().envs(effective_env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // SAFETY: pre_exec runs in the forked child before exec; only async-signal-safe calls.
    unsafe {
        cmd.pre_exec(move || {
            libc::close(read_fd);
            // Clear CLOEXEC on the write end so it survives exec.
            let flags = libc::fcntl(write_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(write_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            unsafe { libc::close(read_fd); libc::close(write_fd); }
            return Err(error.into());
        }
    };
    // SAFETY: parent no longer needs the write end; closing it makes EOF mean "child died".
    unsafe { libc::close(write_fd) };
    let pid = child.id();

    // Wait for 'R' with a deadline (poll(2) on the read end).
    let mut pfd = libc::pollfd { fd: read_fd, events: libc::POLLIN, revents: 0 };
    let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
    let ready = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break false;
        }
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break false;
        }
        if n == 0 {
            break false;
        }
        let mut byte = [0u8; 1];
        // SAFETY: reading one byte into a 1-byte buffer.
        let r = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        break r == 1 && byte[0] == b'R';
    };
    // SAFETY: done with the read end.
    unsafe { libc::close(read_fd) };

    if ready {
        // Keep draining stderr so the child never blocks on a full pipe.
        if let Some(mut err) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = err.read_to_end(&mut sink);
            });
        }
        drop(child);
        return Ok(pid);
    }
    // Stop/reap before reading stderr; descendants may retain its write end.
    let status = child.try_wait().ok().flatten().or_else(|| {
        let _ = child.kill();
        child.wait().ok()
    });
    let mut tail = String::new();
    if let Some(mut err) = child.stderr.take() {
        use std::os::fd::AsRawFd;
        let fd = err.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0 {
            // Bounded bytes and nonblocking reads, even if a descendant holds
            // the pipe open or keeps writing after the wrapper exits.
            let mut bytes = Vec::new();
            let _ = err.by_ref().take(64 * 1024).read_to_end(&mut bytes);
            tail = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    let reason = tail
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("synaps daemon: ").to_string());
    match (status, reason) {
        (Some(st), Some(r)) if st.code().is_some() => {
            anyhow::bail!("daemon exited (code {}): {r}", st.code().unwrap_or(-1))
        }
        (_, Some(r)) => anyhow::bail!("daemon did not become ready within {SPAWN_READY_TIMEOUT:?}: {r}"),
        (_, None) => anyhow::bail!("daemon did not become ready within {SPAWN_READY_TIMEOUT:?}"),
    }
}

/// Result of [`ensure_running`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// A daemon already held the flock.
    Running,
    /// We spawned one (pid) and it signalled ready.
    Spawned(u32),
}

/// Why [`ensure_running`] could not hand back a live daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureError {
    /// `SYNAPS_DAEMON_AUTOSPAWN=0` and nobody is running.
    AutospawnDisabled,
    /// The spawn failed or the child refused/died: one-line reason.
    Spawn(String),
}

impl std::fmt::Display for EnsureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AutospawnDisabled => write!(f, "no daemon running (SYNAPS_DAEMON_AUTOSPAWN=0)"),
            Self::Spawn(r) => write!(f, "{r}"),
        }
    }
}

/// How long a second first-client waits for the spawn lock (the winner's
/// `spawn_detached` is bounded by [`SPAWN_READY_TIMEOUT`]).
const SPAWN_LOCK_WAIT: Duration = SPAWN_READY_TIMEOUT.saturating_add(Duration::from_secs(5));

/// Attach-path entry: a live daemon or a freshly spawned one (jcode model —
/// first client spawns via `setsid` + ready-fd; the daemon holds the flock).
/// The **spawn lock** (`<stem>.spawn.lock` next to the flock) serialises
/// concurrent first-clients: the loser blocks on it, then re-probes the
/// flock and finds the winner's daemon. Enabled warm attaches also acquire
/// this lock (extra open/flock overhead, and the lock must be writable).
/// Never touches the socket on ECONNREFUSED alone — `reap_stale` + the
/// flock decide. Daemon startup must NOT acquire the spawn lock: its parent
/// holds it until the child's ready-fd acknowledgement.
#[cfg(unix)]
pub fn ensure_running(opts: &DaemonOpts) -> Result<Ensured, EnsureError> {
    ensure_running_with(opts, autospawn_enabled(), spawn_detached, || {})
}

// Keep the real cleanup/probe/locking path testable without spawning the unit
// test executable. The hook marks entry to the lock attempt, not readiness.
#[cfg(unix)]
fn ensure_running_with(
    opts: &DaemonOpts,
    auto: bool,
    spawn: impl FnOnce(&DaemonOpts) -> anyhow::Result<u32>,
    before_spawn_lock: impl FnOnce(),
) -> Result<Ensured, EnsureError> {
    let paths = opts.paths();
    if !auto {
        // Preserve the disabled path: no spawn-lock dependency, even warm.
        registry::reap_stale(&paths);
        return if registry::is_alive(&paths) {
            Ok(Ensured::Running)
        } else {
            Err(EnsureError::AutospawnDisabled)
        };
    }
    let _ = std::fs::create_dir_all(&paths.dir);
    before_spawn_lock();
    let _guard = registry::SpawnLock::acquire(&paths, SPAWN_LOCK_WAIT)
        .map_err(|e| EnsureError::Spawn(format!("spawn lock {}: {e}", paths.spawn_lock().display())))?;
    // No enabled caller may reap/probe before this lock: another client's
    // transient cleanup/probe DaemonLock is not evidence of a ready daemon.
    // Somebody may have spawned while we waited.
    registry::reap_stale(&paths);
    if registry::is_alive(&paths) {
        return Ok(Ensured::Running);
    }
    // An auto-spawned daemon exits on its own once the last client is gone
    // and no turn is running (parked sessions are journaled; `--continue`
    // brings them back). A daemon started by hand (`synaps daemon`) keeps
    // whatever its caller asked for.
    let mut opts = opts.clone();
    if opts.idle_exit.is_none() {
        opts.idle_exit = autospawn_idle_exit();
    }
    spawn(&opts).map(Ensured::Spawned).map_err(|e| EnsureError::Spawn(e.to_string()))
}

/// `SYNAPS_DAEMON_IDLE_EXIT_SECS` for auto-spawned daemons: default 10 s —
/// when the last client disconnects and no turn is running, the daemon is
/// holding ~100 MB for one reason only: a re-launch inside the window skips
/// extension boot. Ten seconds covers "oops, wrong window"; `0`/`never` →
/// never exit on idle.
pub fn autospawn_idle_exit() -> Option<Duration> {
    // Precedence: env > config (`daemon.idle_exit_secs`) > built-in default.
    let cfg_secs = crate::config::load_config().daemon.idle_exit_secs;
    autospawn_idle_exit_from(
        std::env::var("SYNAPS_DAEMON_IDLE_EXIT_SECS").ok().as_deref(),
        cfg_secs,
    )
}

fn autospawn_idle_exit_from(v: Option<&str>, cfg_secs: u64) -> Option<Duration> {
    // `0` (env or config) disables idle-exit; otherwise config is the default
    // when the env var is absent or unparseable.
    let from_cfg = || (cfg_secs != 0).then(|| Duration::from_secs(cfg_secs));
    match v.map(str::trim) {
        Some("0" | "never" | "off") => None,
        Some(n) => n.parse::<u64>().ok().map(Duration::from_secs).or_else(from_cfg),
        None => from_cfg(),
    }
}

/// Default idle grace for auto-spawned daemons.
pub const AUTOSPAWN_IDLE_EXIT: Duration = Duration::from_secs(10);


/// `synaps daemon --foreground` body: gate → boot host (cached MCP
/// descriptors only) → extension discovery once → start → background MCP
/// discovery → wait for SIGTERM/SIGINT/Shutdown frame/idle.
pub async fn run_foreground(opts: DaemonOpts) -> anyhow::Result<()> {
    if !enabled() {
        anyhow::bail!("{DISABLED_NOTICE} (exit {EXIT_REFUSED})");
    }
    // Background MCP discovery: a hung/dead MCP server must never delay the
    // ready byte (the spawner waits only SPAWN_READY_TIMEOUT).
    let host = EngineHost::boot_and_install(crate::HostOpts {
        profile: opts.profile.clone(),
        no_extensions: false,
        mcp_discovery: crate::McpDiscovery::Background,
    })
    .await?;
    host.report_config_warnings();
    // Sidecars spawn once per daemon: discover before accepting (bounded by
    // EXTENSION_DISCOVERY_WAIT, which SPAWN_READY_TIMEOUT outwaits).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _loader = crate::extensions::loader::spawn_discover_and_load(Arc::clone(host.ext_manager()), tx, None);
    let wait = async {
        while let Some(ev) = rx.recv().await {
            if let crate::extensions::loader::ExtensionLoaderEvent::Finished { loaded, failed } = ev {
                tracing::info!(loaded = loaded.len(), failed = failed.len(), "daemon: extensions discovered");
                break;
            }
        }
    };
    if tokio::time::timeout(EXTENSION_DISCOVERY_WAIT, wait).await.is_err() {
        tracing::warn!(
            wait = ?EXTENSION_DISCOVERY_WAIT,
            "daemon: extension discovery still running; accepting anyway"
        );
    }
    // C3 router: widget.* frames from every sidecar fan out to every live
    // session (daemon-global widgets today — frames carry no session id).
    let _router = crate::extensions::notify_router::spawn_notification_router(Arc::clone(&host));

    let mut opts = opts;
    if opts.factory.is_none() {
        opts.factory = Some(host_factory(&host));
    }
    let mcp_host = Arc::clone(&host);
    let daemon = Daemon::start(host, opts).await?;
    // Accepting and ready: now discover uncached MCP servers. New servers'
    // tools join the shared registry when it finishes (see
    // `EngineHost::spawn_background_mcp_discovery` for live-session effects).
    let _mcp_discovery = mcp_host.spawn_background_mcp_discovery();
    drop(mcp_host);
    let state = Arc::clone(&daemon.state);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {}
                _ = state.shutdown.cancelled() => return,
            }
        }
        #[cfg(not(unix))]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = state.shutdown.cancelled() => return,
            }
        }
        tracing::info!("daemon: signal received, shutting down");
        state.request_shutdown(false);
    });
    daemon.wait().await;
    Ok(())
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    #[test]
    fn autospawn_idle_exit_defaults_to_10s_and_honours_never() {
        // cfg_secs = 10 mirrors the DaemonConfig default.
        let d = AUTOSPAWN_IDLE_EXIT.as_secs();
        assert_eq!(autospawn_idle_exit_from(None, d), Some(AUTOSPAWN_IDLE_EXIT));
        assert_eq!(AUTOSPAWN_IDLE_EXIT, Duration::from_secs(10));
        assert_eq!(autospawn_idle_exit_from(Some("15"), d), Some(Duration::from_secs(15)));
        assert_eq!(autospawn_idle_exit_from(Some(" 300 "), d), Some(Duration::from_secs(300)));
        for never in ["0", "never", "off"] {
            assert_eq!(autospawn_idle_exit_from(Some(never), d), None, "{never:?}");
        }
        // garbage env → the config default, never "no exit"
        assert_eq!(autospawn_idle_exit_from(Some("soon"), d), Some(AUTOSPAWN_IDLE_EXIT));
        // env absent → config value is the default (env > config > builtin).
        assert_eq!(autospawn_idle_exit_from(None, 42), Some(Duration::from_secs(42)));
        // config 0 disables idle-exit when env is absent.
        assert_eq!(autospawn_idle_exit_from(None, 0), None);
        // env still wins over config.
        assert_eq!(autospawn_idle_exit_from(Some("5"), 42), Some(Duration::from_secs(5)));
    }

    #[test]
    fn enabled_env_matrix() {
        assert!(enabled_from(None), "unset → on");
        for on in ["1", "true", "yes", "on", "", "anything"] {
            assert!(enabled_from(Some(on)), "{on:?} → on");
        }
        for off in ["0", "false", "off", "no", "FALSE", " 0 ", "Off"] {
            assert!(!enabled_from(Some(off)), "{off:?} → off");
        }
    }

    #[test]
    fn profile_mismatch_refuses_a_daemon_of_another_profile() {
        assert_eq!(profile_mismatch(None, None), None);
        assert_eq!(profile_mismatch(Some("axon"), Some("axon")), None);
        // empty == default
        assert_eq!(profile_mismatch(Some(""), None), None);
        assert_eq!(profile_mismatch(None, Some("")), None);
        // a profile client on the default daemon (the 0.10.1 clobber) and back
        let m = profile_mismatch(None, Some("axon")).expect("mismatch");
        assert!(m.contains("the default profile") && m.contains("profile `axon`"), "{m}");
        assert!(m.contains("synaps daemon stop"), "{m}");
        let m = profile_mismatch(Some("axon"), None).expect("mismatch");
        assert!(m.contains("synaps --profile axon daemon stop"), "{m}");
        // sanitised socket stems collide; the profile names do not
        assert!(profile_mismatch(Some("a.b"), Some("a_b")).is_some());
    }

    #[test]
    fn env_is_off_is_case_and_space_insensitive() {
        assert!(env_is_off(Some("NO")));
        assert!(!env_is_off(Some("1")));
        assert!(!env_is_off(None));
    }
}

#[cfg(all(test, unix))]
mod ensure_tests {
    use super::*;
    use std::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(3);
    const BLOCKED: Duration = Duration::from_millis(100);

    fn serialization_case(stale_metadata: bool) {
        let dir = tempfile::tempdir().unwrap();
        let opts = DaemonOpts {
            runtime_dir: Some(dir.path().to_path_buf()),
            profile: Some("ensure-serialization".into()),
            // Avoid reading ambient idle-exit configuration in this test.
            idle_exit: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        let paths = opts.paths();
        let spawn_lock = registry::SpawnLock::acquire(&paths, WAIT).unwrap();
        let cleanup_lock = DaemonLock::try_acquire(&paths).unwrap().unwrap();
        if stale_metadata {
            std::fs::write(&paths.json, b"stale metadata").unwrap();
            std::fs::write(&paths.pid, b"0\n").unwrap();
        }
        let count = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let first_opts = opts.clone();
        let first_count = Arc::clone(&count);
        let first_ready = Arc::clone(&ready);
        let first = std::thread::spawn(move || {
            let result = ensure_running_with(&first_opts, true, |opts| {
                first_count.fetch_add(1, Ordering::SeqCst);
                let paths = opts.paths();
                // The real ensure path must have reaped the old metadata.
                assert!(!paths.json.exists());
                assert!(!paths.pid.exists());
                let lock = DaemonLock::try_acquire(&paths)?.expect("daemon lock free after reap");
                let pid = std::process::id();
                registry::write_daemon_json(&paths, &DaemonInfo {
                    pid,
                    protocol_version: 1,
                    daemon_version: "test".into(),
                    profile: opts.profile.clone(),
                    started_at: chrono::Utc::now(),
                    socket: paths.sock.display().to_string(),
                    exe: None,
                    generation: 1,
                })?;
                // Main retains daemon ownership past both callers' returns.
                daemon_tx.send(lock).unwrap();
                ready_rx.recv_timeout(WAIT).unwrap();
                first_ready.store(true, Ordering::SeqCst);
                Ok(pid)
            }, || attempt_tx.send(()).unwrap());
            done_tx.send(result).unwrap();
        });
        // Trace entry to the actual lock attempt before asserting absence.
        attempt_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(done_rx.recv_timeout(BLOCKED), Err(mpsc::RecvTimeoutError::Timeout));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(paths.json.exists(), stale_metadata);
        assert_eq!(paths.pid.exists(), stale_metadata);
        drop(cleanup_lock);
        drop(spawn_lock);
        let daemon_lock = daemon_rx.recv_timeout(WAIT).unwrap();

        // A second real ensure caller sees daemon ownership and metadata, but
        // must still wait for the spawning caller's ready acknowledgement.
        let (second_attempt_tx, second_attempt_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second_count = Arc::clone(&count);
        let second_ready = Arc::clone(&ready);
        let second = std::thread::spawn(move || {
            let result = ensure_running_with(&opts, true, |_| {
                second_count.fetch_add(1, Ordering::SeqCst);
                panic!("warm attach must not spawn");
            }, || second_attempt_tx.send(()).unwrap());
            second_done_tx.send((result, second_ready.load(Ordering::SeqCst))).unwrap();
        });
        second_attempt_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(second_done_rx.recv_timeout(BLOCKED), Err(mpsc::RecvTimeoutError::Timeout));
        assert!(!ready.load(Ordering::SeqCst));
        ready_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(WAIT).unwrap(), Ok(Ensured::Spawned(std::process::id())));
        assert_eq!(second_done_rx.recv_timeout(WAIT).unwrap(), (Ok(Ensured::Running), true));
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(registry::read_daemon_json(&paths).unwrap().pid, std::process::id());
        drop(daemon_lock);
    }

    #[test]
    fn ensure_serializes_before_probe_with_missing_metadata() {
        serialization_case(false);
    }

    #[test]
    fn ensure_serializes_before_probe_with_stale_metadata() {
        serialization_case(true);
    }

    #[test]
    fn disabled_ensure_keeps_warm_attach_without_spawn_lock() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DaemonOpts {
            runtime_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let paths = opts.paths();
        let _spawn_lock = registry::SpawnLock::acquire(&paths, WAIT).unwrap();
        let daemon_lock = DaemonLock::try_acquire(&paths).unwrap().unwrap();
        let ensure = || ensure_running_with(&opts, false,
            |_| panic!("disabled must not spawn"),
            || panic!("disabled must not attempt spawn lock"));
        assert_eq!(ensure(), Ok(Ensured::Running));
        drop(daemon_lock);
        std::fs::write(&paths.json, b"stale").unwrap();
        std::fs::write(&paths.pid, b"0\n").unwrap();
        assert_eq!(ensure(), Err(EnsureError::AutospawnDisabled));
        assert!(!paths.json.exists());
        assert!(!paths.pid.exists());
    }
}
