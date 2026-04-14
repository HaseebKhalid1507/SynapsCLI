//! watcher — Autonomous agent supervisor daemon
//!
//! Spawns, monitors, and restarts agent worker processes.
//! Manages agent lifecycles with heartbeat monitoring and crash recovery.
//!
//! Usage:
//!   watcher run                    — start supervisor daemon (foreground)
//!   watcher deploy <name>          — start supervising an agent
//!   watcher stop <name>            — stop an agent
//!   watcher status                 — show all agent statuses
//!   watcher list                   — list configured agents
//!   watcher init <name>            — create agent from template
//!   watcher once <name>            — run agent once, no supervision
//!   watcher logs <name>            — show agent logs

mod ipc;
mod supervisor;
mod display;

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::os::unix::fs::PermissionsExt;
use synaps_cli::{AgentConfig, WatcherCommand, WatcherResponse, AgentStatusInfo};
use tokio::sync::{Mutex, Semaphore};
use tokio::net::{UnixListener, UnixStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use notify::Watcher;

use ipc::*;
use supervisor::*;
use display::*;

pub(crate) fn watcher_dir() -> PathBuf {
    synaps_cli::config::base_dir().join("watcher")
}

pub(crate) fn agent_binary() -> PathBuf {
    // Find synaps-agent binary next to the watcher binary
    let current_exe = std::env::current_exe().unwrap_or_default();
    let dir = current_exe.parent().unwrap_or(std::path::Path::new("."));
    dir.join("synaps-agent")
}

pub(crate) fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [watcher] {}", ts, msg);
}

pub(crate) fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Agent name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err("Agent name too long (max 64 characters)".to_string());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("Agent name '{}' contains invalid characters (use a-z, 0-9, -, _)", name));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err("Agent name cannot start with - or _".to_string());
    }
    Ok(())
}

pub(crate) fn load_agent_stats(agent_dir: &std::path::Path) -> synaps_cli::watcher_types::AgentStats {
    let path = agent_dir.join("stats.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// State for a managed agent
pub(crate) struct ManagedAgent {
    pub(crate) name: String,
    pub(crate) config_path: PathBuf,
    pub(crate) config: AgentConfig,
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) pid: Option<u32>,
    pub(crate) session_count: u64,
    pub(crate) consecutive_crashes: u32,
    pub(crate) last_start: Option<Instant>,
    pub(crate) total_uptime_secs: f64,
    pub(crate) stopped: bool, // manually stopped, don't restart
}

impl ManagedAgent {
    pub(crate) fn new(name: String, config_path: PathBuf, config: AgentConfig) -> Self {
        Self {
            name,
            config_path,
            config,
            child: None,
            pid: None,
            session_count: 0,
            consecutive_crashes: 0,
            last_start: None,
            total_uptime_secs: 0.0,
            stopped: false,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.child.is_some()
    }

    pub(crate) fn status_str(&self) -> &str {
        if self.stopped {
            "stopped"
        } else if self.is_running() {
            "running"
        } else {
            "sleeping"
        }
    }


    pub(crate) fn current_uptime_secs(&self) -> Option<f64> {
        if self.is_running() {
            self.last_start.map(|s| s.elapsed().as_secs_f64())
        } else {
            None
        }
    }

    pub(crate) fn to_status_info(&self) -> AgentStatusInfo {
        let agent_dir = AgentConfig::agent_dir(&self.config_path);
        let stats = load_agent_stats(&agent_dir);
        
        AgentStatusInfo {
            name: self.name.clone(),
            trigger: self.config.agent.trigger.clone(),
            status: self.status_str().to_string(),
            session_count: self.session_count,
            uptime_secs: self.current_uptime_secs(),
            pid: self.pid,
            consecutive_crashes: self.consecutive_crashes,
            cost_today: stats.today.cost_usd,
            cost_limit: self.config.limits.max_daily_cost_usd,
            tokens_today: stats.today.tokens,
            total_sessions: stats.total_sessions,
            model: self.config.agent.model.clone(),
        }
    }
}

pub(crate) fn discover_agents() -> Vec<(String, PathBuf)> {
    let dir = watcher_dir();
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let config_path = entry.path().join("config.toml");
                if config_path.exists() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Filter out invalid names
                    if validate_agent_name(&name).is_ok() {
                        agents.push((name, config_path));
                    }
                }
            }
        }
    }
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    agents
}

pub(crate) fn print_status(agents: &HashMap<String, ManagedAgent>) {
    if agents.is_empty() {
        println!("No agents configured. Run: watcher init <name>");
        return;
    }
    let infos: Vec<AgentStatusInfo> = agents.values().map(|a| a.to_status_info()).collect();
    print_status_table(infos);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "init" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher init <name>");
                std::process::exit(1);
            });
            if let Err(e) = init_agent(name) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }

        "list" => {
            let agents = discover_agents();
            if agents.is_empty() {
                println!("No agents configured. Run: watcher init <name>");
            } else {
                println!("{:<15} {:<50}", "AGENT", "CONFIG");
                println!("{}", "─".repeat(65));
                for (name, path) in &agents {
                    println!("{:<15} {}", name, path.display());
                }
            }
        }

        "once" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher once <name>");
                std::process::exit(1);
            });
            let config_path = watcher_dir().join(name).join("config.toml");
            let config = AgentConfig::load(&config_path).unwrap_or_else(|e| {
                eprintln!("Failed to load agent '{}': {}", name, e);
                std::process::exit(1);
            });
            let mut agent = ManagedAgent::new(name.clone(), config_path, config);
            if let Err(e) = spawn_agent(&mut agent, "one-shot run").await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            // Wait for completion
            if let Some(ref mut child) = agent.child {
                let status = child.wait().await.unwrap_or_else(|e| {
                    eprintln!("Error waiting for agent: {}", e);
                    std::process::exit(1);
                });
                let code = status.code().unwrap_or(1);
                log(&format!("[{}] exited with code {}", name, code));
                std::process::exit(code);
            }
        }

        "run" => {
            // Check if supervisor already running
            let pid_path = watcher_dir().join("watcher.pid");
            if pid_path.exists() {
                if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        // Check if process is alive
                        let proc_path = format!("/proc/{}", pid);
                        if std::path::Path::new(&proc_path).exists() {
                            eprintln!("Error: Supervisor already running (PID {})", pid);
                            std::process::exit(1);
                        }
                    }
                }
                // Stale PID file — clean up
                let _ = std::fs::remove_file(&pid_path);
            }
            
            // Main supervisor loop
            log("starting supervisor");

            // Setup socket and PID file paths
            let socket_path = watcher_dir().join("watcher.sock");
            let pid_path = watcher_dir().join("watcher.pid");
            
            // Clean up socket and write PID
            let _ = std::fs::remove_file(&socket_path);
            std::fs::create_dir_all(watcher_dir()).unwrap_or_else(|e| {
                eprintln!("Failed to create watcher directory: {}", e);
                std::process::exit(1);
            });
            std::fs::write(&pid_path, std::process::id().to_string()).unwrap_or_else(|e| {
                eprintln!("Failed to write PID file: {}", e);
                std::process::exit(1);
            });

            let agents: Arc<Mutex<HashMap<String, ManagedAgent>>> = Arc::new(Mutex::new(HashMap::new()));

            // Load all agents
            {
                let mut agents_map = agents.lock().await;
                for (name, config_path) in discover_agents() {
                    match AgentConfig::load(&config_path) {
                        Ok(config) => {
                            log(&format!("loaded agent: {} (trigger: {})", name, config.agent.trigger));
                            agents_map.insert(name.clone(), ManagedAgent::new(name, config_path, config));
                        }
                        Err(e) => {
                            log(&format!("WARN: failed to load {}: {}", name, e));
                        }
                    }
                }

                if agents_map.is_empty() {
                    log("no agents configured — run 'watcher init <name>' first");
                    std::process::exit(0);
                }
            }

            // Start IPC listener
            let ipc_agents = agents.clone();
            tokio::spawn(async move {
                ipc_listener(ipc_agents).await;
            });

            // Setup signal handling (Ctrl+C and SIGTERM)
            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            tokio::spawn(async move {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate()
                ).expect("failed to register SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = sigterm.recv() => {},
                }
                r.store(false, std::sync::atomic::Ordering::Relaxed);
            });

            // Start always-on agents
            {
                let mut agents_map = agents.lock().await;
                for (name, agent) in agents_map.iter_mut() {
                    if agent.config.agent.trigger == "always" {
                        if let Err(e) = spawn_agent(agent, "supervisor start (always-on)").await {
                            log(&format!("[{}] failed to start: {}", name, e));
                        }
                    }
                }
            }

            // Start file watchers for watch-trigger agents
            {
                let agents_map = agents.lock().await;
                for (name, agent) in agents_map.iter() {
                    if agent.config.agent.trigger == "watch" {
                        spawn_watch_task(
                            name.clone(),
                            agent.config.clone(),
                            agents.clone(),
                            running.clone(),
                        );
                    }
                }
            }

            // Supervisor loop — check agents every 5 seconds
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                {
                    let mut agents_map = agents.lock().await;
                    for (name, agent) in agents_map.iter_mut() {
                        if agent.stopped { continue; }

                        // Check if child has exited
                        if let Some(ref mut child) = agent.child {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    let elapsed = agent.last_start.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
                                    agent.total_uptime_secs += elapsed;
                                    let code = status.code().unwrap_or(-1);

                                    if code == 0 {
                                        log(&format!("[{}] session #{} completed cleanly ({:.0}s)", name, agent.session_count, elapsed));
                                        agent.consecutive_crashes = 0;
                                    } else if code == 2 {
                                        log(&format!("[{}] daily cost limit reached — pausing until midnight", name));
                                        agent.stopped = true;  // Don't restart
                                        // TODO: could add a midnight reset timer later
                                    } else {
                                        agent.consecutive_crashes += 1;
                                        log(&format!("[{}] session #{} crashed (code: {}, consecutive: {})",
                                            name, agent.session_count, code, agent.consecutive_crashes));
                                    }

                                    agent.child = None;
                                    agent.pid = None;

                                    // Restart logic for always-on agents
                                    if agent.config.agent.trigger == "always" {
                                        if agent.consecutive_crashes >= agent.config.limits.max_retries {
                                            log(&format!("[{}] max retries ({}) exceeded — stopping", name, agent.config.limits.max_retries));
                                            agent.stopped = true;
                                        } else {
                                            // Backoff: cooldown * 2^crashes (capped at 5 min)
                                            let backoff = if agent.consecutive_crashes > 0 {
                                                let base = agent.config.limits.cooldown_secs;
                                                let factor = 2u64.pow(agent.consecutive_crashes.saturating_sub(1));
                                                (base * factor).min(300)
                                            } else {
                                                agent.config.limits.cooldown_secs
                                            };
                                            log(&format!("[{}] restarting in {}s", name, backoff));
                                            
                                            // Schedule restart after dropping the lock
                                            let agent_name = name.clone();
                                            let agents_clone = agents.clone();
                                            let running_clone = running.clone();
                                            
                                            tokio::spawn(async move {
                                                tokio::time::sleep(Duration::from_secs(backoff)).await;
                                                
                                                if running_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                                    let mut agents_map = agents_clone.lock().await;
                                                    if let Some(agent) = agents_map.get_mut(&agent_name) {
                                                        let ctx = if code == 0 { "automatic restart (always-on)" }
                                                                  else { "crash recovery restart" };
                                                        if let Err(e) = spawn_agent(agent, ctx).await {
                                                            log(&format!("[{}] failed to restart: {}", agent_name, e));
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                                Ok(None) => {
                                    // Still running — check heartbeat
                                    let agent_dir = AgentConfig::agent_dir(&agent.config_path);
                                    if agent.last_start.map(|s| s.elapsed().as_secs()).unwrap_or(0) > 60 {
                                        // Only check heartbeat after first minute
                                        if !check_heartbeat(&agent_dir, agent.config.heartbeat.stale_threshold_secs) {
                                            log(&format!("[{}] heartbeat stale — killing", name));
                                            let _ = child.kill().await;
                                            let _ = child.wait().await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    log(&format!("[{}] error checking child: {}", name, e));
                                }
                            }
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }

            // Graceful shutdown — kill all running agents
            log("shutting down — stopping all agents");
            {
                let mut agents_map = agents.lock().await;
                for (name, agent) in agents_map.iter_mut() {
                    if let Some(ref mut child) = agent.child {
                        log(&format!("[{}] sending SIGTERM", name));
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        // Give it time to write handoff
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }

            // Clean up files
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&pid_path);
            
            log("supervisor stopped");
        }

        "status" => {
            if let Some(agent_name) = args.get(2) {
                // Validate agent name
                if let Err(e) = validate_agent_name(agent_name) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                
                // Detailed status for specific agent
                match send_ipc_command(WatcherCommand::AgentStatus { name: agent_name.clone() }).await {
                    Ok(WatcherResponse::AgentDetail { info }) => {
                        print_agent_detail(info);
                    }
                    Ok(WatcherResponse::Error { message }) => {
                        eprintln!("Error: {}", message);
                        std::process::exit(1);
                    }
                    Err(_e) => {
                        // Fallback to static detailed status
                        let config_path = watcher_dir().join(agent_name).join("config.toml");
                        if let Ok(config) = AgentConfig::load(&config_path) {
                            let agent = ManagedAgent::new(agent_name.clone(), config_path, config);
                            print_agent_detail(agent.to_status_info());
                        } else {
                            eprintln!("Agent '{}' not found", agent_name);
                            std::process::exit(1);
                        }
                    }
                    _ => {
                        eprintln!("Unexpected response from supervisor");
                        std::process::exit(1);
                    }
                }
            } else {
                // Overall status
                match send_ipc_command(WatcherCommand::Status).await {
                    Ok(WatcherResponse::Status { agents }) => {
                        print_status_table(agents);
                    }
                    Ok(WatcherResponse::Error { message }) => {
                        eprintln!("Error: {}", message);
                        std::process::exit(1);
                    }
                    Err(e) => {
                        // Fallback to static status if supervisor not running
                        let discovered = discover_agents();
                        let mut agents: HashMap<String, ManagedAgent> = HashMap::new();
                        for (name, config_path) in discovered {
                            if let Ok(config) = AgentConfig::load(&config_path) {
                                agents.insert(name.clone(), ManagedAgent::new(name, config_path, config));
                            }
                        }
                        print_status(&agents);
                        if !e.contains("Supervisor not running") {
                            eprintln!("Warning: {}", e);
                        }
                    }
                    _ => {
                        eprintln!("Unexpected response from supervisor");
                        std::process::exit(1);
                    }
                }
            }
        }

        "deploy" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher deploy <name>");
                std::process::exit(1);
            });
            
            // Validate agent name
            if let Err(e) = validate_agent_name(name) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            
            match send_ipc_command(WatcherCommand::Deploy { name: name.clone() }).await {
                Ok(WatcherResponse::Ok { message }) => {
                    println!("{}", message);
                }
                Ok(WatcherResponse::Error { message }) => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unexpected response from supervisor");
                    std::process::exit(1);
                }
            }
        }

        "stop" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher stop <name>");
                std::process::exit(1);
            });
            
            // Validate agent name
            if let Err(e) = validate_agent_name(name) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            
            match send_ipc_command(WatcherCommand::Stop { name: name.clone() }).await {
                Ok(WatcherResponse::Ok { message }) => {
                    println!("{}", message);
                }
                Ok(WatcherResponse::Error { message }) => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unexpected response from supervisor");
                    std::process::exit(1);
                }
            }
        }

        "logs" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher logs <name> [--follow | --session N | --last N]");
                std::process::exit(1);
            });

            // Validate agent name
            if let Err(e) = validate_agent_name(name) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }

            // Parse flags
            let follow = args.iter().any(|a| a == "--follow" || a == "-f");
            let session_num = args.iter().position(|a| a == "--session").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<u64>().ok());
            let last_n = args.iter().position(|a| a == "--last").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<usize>().ok());

            if let Err(e) = show_logs(name, follow, session_num, last_n).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }

        "help" | "--help" | "-h" => {
            println!("watcher — Autonomous agent supervisor");
            println!();
            println!("USAGE:");
            println!("  watcher run                 Start supervisor daemon (foreground)");
            println!("  watcher deploy <name>       Deploy/start an agent");
            println!("  watcher stop <name>         Stop an agent");  
            println!("  watcher once <name>         Run agent once without supervision");
            println!("  watcher init <name>         Create new agent from template");
            println!("  watcher list                List configured agents");
            println!("  watcher status              Show all agent statuses");
            println!("  watcher status <name>       Show detailed status for agent");
            println!("  watcher logs <name>         Show latest session log");
            println!("  watcher logs <name> --follow  Tail current session log");
            println!("  watcher logs <name> --session N  Show specific session");
            println!("  watcher help                Show this help");
            println!();
            println!("AGENTS DIR: {}", watcher_dir().display());
        }

        _ => {
            eprintln!("Unknown command: {}", command);
            eprintln!("Run 'watcher help' for usage information");
            std::process::exit(1);
        }
    }
}