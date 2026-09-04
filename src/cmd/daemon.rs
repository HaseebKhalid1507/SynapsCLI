//! `synaps daemon {start,status,stop,sessions}` (PLAN-phase2 §2.11, B4).

use std::path::PathBuf;
use std::time::Duration;

use agent_engine::daemon::{self, registry, DaemonOpts, EXIT_REFUSED};
use agent_engine::session::socket_transport::SocketTransport;
use clap::{Args, Subcommand};

#[derive(Args, Debug, Clone)]
pub(crate) struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
    #[command(flatten)]
    pub start: StartArgs,
}

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct StartArgs {
    /// Run in this terminal (default).
    #[arg(long, conflicts_with = "detach")]
    pub foreground: bool,
    /// Fork into the background (setsid) and return once the socket is ready.
    #[arg(long)]
    pub detach: bool,
    /// Socket path override (lock/json/pid stay under the run dir).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Exit after SECS with zero clients and zero sessions.
    #[arg(long, value_name = "SECS")]
    pub idle_exit: Option<u64>,
    /// Allow legacy (non-progressive) MCP with servers configured.
    #[arg(long)]
    pub allow_legacy_mcp: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DaemonAction {
    /// Start the daemon (same as no subcommand).
    Start(StartArgs),
    /// Registry + flock probe + Ping.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Graceful Shutdown{force} over the socket; --force escalates to SIGTERM/SIGKILL.
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// List live sessions in the daemon.
    Sessions {
        #[arg(long)]
        json: bool,
    },
}

fn opts_from(profile: Option<String>, a: &StartArgs) -> DaemonOpts {
    DaemonOpts {
        socket: a.socket.clone(),
        profile,
        idle_exit: a.idle_exit.map(Duration::from_secs),
        allow_legacy_mcp: a.allow_legacy_mcp,
        runtime_dir: None,
        factory: None,
    }
}

/// Exit code 3 + one-line reason when the flag is off.
pub(crate) fn require_enabled(what: &str) -> Result<(), i32> {
    if daemon::enabled() {
        return Ok(());
    }
    eprintln!("{what}: experimental; set SYNAPS_DAEMON=1 to enable");
    Err(EXIT_REFUSED)
}

pub(crate) async fn run(profile: Option<String>, args: DaemonArgs) -> anyhow::Result<()> {
    match args.action {
        None => start(profile, args.start).await,
        Some(DaemonAction::Start(a)) => start(profile, a).await,
        Some(DaemonAction::Status { json }) => status(profile, json).await,
        Some(DaemonAction::Stop { force }) => stop(profile, force).await,
        Some(DaemonAction::Sessions { json }) => sessions(profile, json).await,
    }
}

async fn start(profile: Option<String>, a: StartArgs) -> anyhow::Result<()> {
    if let Err(code) = require_enabled("synaps daemon") {
        std::process::exit(code);
    }
    let opts = opts_from(profile, &a);
    if a.detach {
        let paths = opts.paths();
        if registry::is_alive(&paths) {
            let pid = registry::read_daemon_json(&paths).map(|i| i.pid);
            println!("daemon already running (pid {})", pid.map_or("?".into(), |p| p.to_string()));
            return Ok(());
        }
        let pid = daemon::spawn_detached(&opts)?;
        println!("daemon started (pid {pid}, socket {})", opts.paths().sock.display());
        return Ok(());
    }
    match daemon::run_foreground(opts).await {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("synaps daemon: {e}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

async fn status(profile: Option<String>, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    let info = registry::read_daemon_json(&paths);
    let alive = registry::is_alive(&paths);
    let pong = if alive { SocketTransport::ping(&paths.sock).await.ok() } else { None };
    let state = match (info.is_some(), alive, pong.is_some()) {
        (_, true, true) => "running",
        (_, true, false) => "running (not answering)",
        (true, false, _) => "stale (pid dead)",
        (false, false, _) => "stopped",
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "state": state,
                "ok": pong.is_some(),
                "socket": paths.sock,
                "pid": pong.as_ref().map(|p| p.pid).or(info.as_ref().map(|i| i.pid)),
                "uptime_s": pong.as_ref().map(|p| p.uptime_s),
                "sessions": pong.as_ref().map(|p| p.sessions),
                "daemon_version": info.as_ref().map(|i| i.daemon_version.clone()),
                "protocol_version": info.as_ref().map(|i| i.protocol_version),
                "profile": info.as_ref().and_then(|i| i.profile.clone()),
            })
        );
    } else {
        println!("daemon: {state}");
        println!("socket: {}", paths.sock.display());
        if let Some(i) = &info {
            println!("pid: {}  version: {}  protocol: {}  started: {}", i.pid, i.daemon_version, i.protocol_version, i.started_at);
        }
        if let Some(p) = &pong {
            println!("uptime: {}s  sessions: {}", p.uptime_s, p.sessions);
        }
    }
    if pong.is_none() {
        std::process::exit(1);
    }
    Ok(())
}

async fn stop(profile: Option<String>, force: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        println!("daemon not running");
        registry::reap_stale(&paths);
        return Ok(());
    }
    let pid = registry::read_daemon_json(&paths).map(|i| i.pid);
    match SocketTransport::shutdown(&paths.sock, force).await {
        Ok(()) => {}
        Err(e) => eprintln!("shutdown request failed: {e}"),
    }
    // Wait for the flock to be released.
    let grace = Duration::from_secs(if force { 10 } else { 30 });
    let t0 = std::time::Instant::now();
    while registry::is_alive(&paths) {
        if t0.elapsed() > grace {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    #[cfg(unix)]
    if force && registry::is_alive(&paths) {
        if let Some(pid) = pid {
            eprintln!("daemon still alive after {grace:?}; SIGTERM {pid}");
            let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            let t1 = std::time::Instant::now();
            while registry::is_alive(&paths) && t1.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if registry::is_alive(&paths) {
                eprintln!("SIGKILL {pid}");
                let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
            }
        }
    }
    if registry::is_alive(&paths) {
        anyhow::bail!("daemon did not stop");
    }
    registry::reap_stale(&paths);
    println!("daemon stopped{}", pid.map_or(String::new(), |p| format!(" (pid {p})")));
    Ok(())
}

async fn sessions(profile: Option<String>, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        anyhow::bail!("daemon not running");
    }
    let list = SocketTransport::sessions(&paths.sock).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else if list.is_empty() {
        println!("no sessions");
    } else {
        for m in list {
            println!(
                "{}  model={}  cwd={}  created={}{}",
                m.id,
                m.model,
                m.cwd.as_deref().map_or("-".into(), |p| p.display().to_string()),
                m.created_at.format("%H:%M:%S"),
                m.name.as_deref().map_or(String::new(), |n| format!("  name={n}"))
            );
        }
    }
    Ok(())
}
