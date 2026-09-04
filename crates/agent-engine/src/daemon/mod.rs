//! Daemon front-end (`synaps daemon`): UDS listener, per-connection pump,
//! registry file, lifecycle (PLAN-phase2 §2.11). Lives in `agent-engine`
//! (S289 D-1 amended) — `agent-tui` must never appear in this graph.
//!
//! Gated: `SYNAPS_DAEMON=1` is required for `run_foreground` (exit 3 with a
//! one-line reason otherwise). Safety properties that are NEVER stubbed:
//! the flock liveness oracle, the ready-fd pipe, socket perms 0700/0600.

pub mod conn;
pub mod lifecycle;
pub mod listener;
pub mod registry;
pub mod reload;

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
/// Env var carrying the write end of the ready pipe to a spawned daemon.
pub const READY_FD_ENV: &str = "SYNAPS_DAEMON_READY_FD";
/// How long `spawn_detached` waits for the child's ready byte.
pub const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// `SYNAPS_DAEMON=1` feature flag (§4.4).
pub fn enabled() -> bool {
    matches!(std::env::var("SYNAPS_DAEMON").as_deref(), Ok("1") | Ok("true"))
}

/// `SYNAPS_DAEMON_ALLOW_LEGACY_MCP=1` (§4.4).
pub fn allow_legacy_mcp_env() -> bool {
    matches!(std::env::var("SYNAPS_DAEMON_ALLOW_LEGACY_MCP").as_deref(), Ok("1") | Ok("true"))
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
    /// Exit after this long with zero connections and no session running a
    /// turn (idle, client-less sessions are saved on exit; `--continue`
    /// brings them back). A turn in flight always blocks the exit.
    pub idle_exit: Option<Duration>,
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
    pub sessions: Mutex<HashMap<SessionId, SessionHandle>>,
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
    /// Cancelled once per process life: every conn sends `Reloading` + `Bye`.
    pub reload_announce: CancellationToken,
    /// The flock, held here (not on `Daemon`) so `reload` can hand its fd
    /// to the next image.
    pub lock_fd: Mutex<Option<DaemonLock>>,
    /// `old id → new id` for one generation (ids differ only if a
    /// LinkedSuccessor compaction happened since boot).
    pub reload_aliases: Mutex<HashMap<String, String>>,
}

impl DaemonState {
    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Live handles only (dead actors are dropped on read).
    pub fn live_sessions(&self) -> Vec<SessionHandle> {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, h| h.is_alive());
        map.values().cloned().collect()
    }

    /// Metas with the handle cells filled in (`lifecycle`, `journal_id`);
    /// `clients`/`input_owner`/`awaiting_input` come with B4's cells.
    pub fn session_metas(&self) -> Vec<SessionMeta> {
        self.live_sessions()
            .iter()
            .map(|h| {
                let mut m = h.meta().clone();
                m.lifecycle = h.lifecycle();
                m.journal_id = h.journal_id();
                m
            })
            .collect()
    }

    pub fn attach(&self, id: &SessionId) -> Option<SessionHandle> {
        let live = self.live_sessions();
        live.iter().find(|h| &h.id == id).cloned().or_else(|| {
            let alias = self.reload_aliases.lock().unwrap_or_else(|e| e.into_inner()).get(id.as_str()).cloned()?;
            live.into_iter().find(|h| h.id.as_str() == alias)
        })
    }

    pub fn insert(&self, handle: SessionHandle) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle.id.clone(), handle);
    }

    pub fn remove(&self, id: &SessionId) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    pub async fn create(&self, cfg: SessionConfig) -> Result<SessionHandle, String> {
        let handle = (self.factory)(cfg).await?;
        self.insert(handle.clone());
        Ok(handle)
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

/// Refuse-to-start check (§2.11): legacy `McpTool` connections would be
/// shared across sessions.
pub fn legacy_mcp_conflict(host: &EngineHost, allow: bool) -> Option<String> {
    if allow || allow_legacy_mcp_env() {
        return None;
    }
    let cfg = host.config();
    if !cfg.progressive_tool_disclosure && host.mcp_server_count() > 0 {
        return Some(format!(
            "progressive_tool_disclosure=false with {} MCP server(s) configured: legacy McpTool connections would be shared across sessions. Set progressive_tool_disclosure=true, or pass --allow-legacy-mcp / SYNAPS_DAEMON_ALLOW_LEGACY_MCP=1",
            host.mcp_server_count()
        ));
    }
    None
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
        if let Some(msg) = legacy_mcp_conflict(&host, opts.allow_legacy_mcp) {
            anyhow::bail!("refusing to start: {msg}");
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

        let factory = opts.factory.clone().unwrap_or_else(|| host_factory(&host));
        let state = Arc::new(DaemonState {
            host,
            paths: paths.clone(),
            profile: opts.profile.clone(),
            started: Instant::now(),
            sessions: Mutex::new(HashMap::new()),
            factory,
            connections: AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
            force_shutdown: AtomicBool::new(false),
            generation,
            reloading: AtomicBool::new(false),
            reload_generation: std::sync::atomic::AtomicU64::new(generation),
            reload_announce: CancellationToken::new(),
            lock_fd: Mutex::new(Some(lock)),
            reload_aliases: Mutex::new(HashMap::new()),
        });

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
        registry::write_daemon_json(&paths, &info)?;

        // C3: rehydrate BEFORE accepting so a reconnecting client finds its session.
        if let Some(rs) = &reload_state {
            reload::rehydrate(&state, rs).await;
        }

        let accept = tokio::spawn(listener::accept_loop(Arc::clone(&state), listener, state.shutdown.clone()));
        if let Some(idle) = opts.idle_exit {
            tokio::spawn(idle_monitor(Arc::clone(&state), idle));
        }
        signal_ready();
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
        for p in [&self.paths.sock, &self.paths.json, &self.paths.pid] {
            crate::events::socket::cleanup_socket(&p.to_string_lossy());
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
    use crate::session::{SessionCommand, SessionEventWire, SessionQuery};
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

/// Idle = zero connections AND every live session between turns, held for
/// `idle` continuously. Clients count first so a probe is never sent while
/// someone is attached.
pub async fn daemon_is_idle(state: &DaemonState) -> bool {
    if state.connections.load(Ordering::SeqCst) > 0 {
        return false;
    }
    for h in state.live_sessions() {
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
/// (`setsid`), stdout → null, stderr → pipe; waits ≤ 5 s for `R` on the
/// ready pipe. EOF before `R` = child died → error with its stderr tail.
#[cfg(unix)]
pub fn spawn_detached(opts: &DaemonOpts) -> anyhow::Result<u32> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()?;
    let mut fds = [0i32; 2];
    // SAFETY: plain pipe(2) on a valid 2-slot array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("--foreground");
    if let Some(p) = &opts.profile {
        cmd.arg("--profile").arg(p);
    }
    if let Some(s) = &opts.socket {
        cmd.arg("--socket").arg(s);
    }
    if let Some(i) = opts.idle_exit {
        cmd.arg("--idle-exit").arg(i.as_secs().to_string());
    }
    if opts.allow_legacy_mcp {
        cmd.arg("--allow-legacy-mcp");
    }
    cmd.env("SYNAPS_DAEMON", "1")
        .env(READY_FD_ENV, write_fd.to_string())
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
    let mut child = cmd.spawn()?;
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
    let mut tail = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut tail);
    }
    let _ = child.kill();
    let _ = child.wait();
    let tail: String = tail.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    anyhow::bail!("daemon did not become ready within {SPAWN_READY_TIMEOUT:?}\n{tail}")
}

/// `synaps daemon --foreground` body: gate → boot host → extension discovery
/// once → start → wait for SIGTERM/SIGINT/Shutdown frame/idle.
pub async fn run_foreground(opts: DaemonOpts) -> anyhow::Result<()> {
    if !enabled() {
        anyhow::bail!("synaps daemon is experimental; set SYNAPS_DAEMON=1 to enable (exit {EXIT_REFUSED})");
    }
    let host = EngineHost::boot_and_install(crate::HostOpts { profile: opts.profile.clone(), no_extensions: false }).await?;
    if let Some(msg) = legacy_mcp_conflict(&host, opts.allow_legacy_mcp) {
        anyhow::bail!("refusing to start: {msg} (exit {EXIT_REFUSED})");
    }
    // Sidecars spawn once per daemon: discover before accepting (bounded 10 s).
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
    if tokio::time::timeout(Duration::from_secs(10), wait).await.is_err() {
        tracing::warn!("daemon: extension discovery still running after 10 s; accepting anyway");
    }
    // C3 router: widget.* frames from every sidecar fan out to every live
    // session (daemon-global widgets today — frames carry no session id).
    let _router = crate::extensions::notify_router::spawn_notification_router(Arc::clone(&host));

    let mut opts = opts;
    if opts.factory.is_none() {
        opts.factory = Some(host_factory(&host));
    }
    let daemon = Daemon::start(host, opts).await?;
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
