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
use synaps_cli::{AgentConfig, WatcherCommand, WatcherResponse, AgentStatusInfo, BusHandle};
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
    pub(crate) child: Option<tokio::process::Child>,  // process isolation mode
    pub(crate) bus_handle: Option<BusHandle>,          // task isolation mode
    pub(crate) task_handle: Option<tokio::task::JoinHandle<()>>,  // task isolation mode
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
            bus_handle: None,
            task_handle: None,
            pid: None,
            session_count: 0,
            consecutive_crashes: 0,
            last_start: None,
            total_uptime_secs: 0.0,
            stopped: false,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.child.is_some() || self.task_handle.is_some()
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
            run_supervisor().await;
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

        "attach" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: watcher attach <name> [--readonly]");
                std::process::exit(1);
            });
            if let Err(e) = validate_agent_name(name) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            let mode = if args.iter().any(|a| a == "--readonly" || a == "--ro") { "ro" } else { "rw" };

            let socket_path = watcher_dir().join("watcher.sock");
            if !socket_path.exists() {
                eprintln!("Supervisor not running. Start with: watcher run");
                std::process::exit(1);
            }
            let mut stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
                eprintln!("Failed to connect to supervisor: {}", e);
                std::process::exit(1);
            });

            // Send attach command
            let cmd = serde_json::to_string(&WatcherCommand::Attach {
                name: name.clone(),
                mode: mode.to_string(),
            }).unwrap();
            stream.write_all(cmd.as_bytes()).await.ok();
            stream.write_all(b"\n").await.ok();
            stream.flush().await.ok();

            // Read streaming events
            let mut reader = BufReader::new(stream);
            eprintln!("Attached to '{}' ({}). Ctrl+C to detach.\n", name, mode);

            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }
                        // Try to parse as attach protocol
                        if let Ok(event) = serde_json::from_str::<synaps_cli::AttachEvent>(trimmed) {
                            match event {
                                synaps_cli::AttachEvent::Sync { .. } => {
                                    eprintln!("[sync received]");
                                }
                                synaps_cli::AttachEvent::Event { event } => {
                                    // Simple text-mode display
                                    match &event {
                                        synaps_cli::transport::AgentEvent::Text(t) => print!("{}", t),
                                        synaps_cli::transport::AgentEvent::Thinking(t) => eprint!("\x1b[2m{}\x1b[0m", t),
                                        synaps_cli::transport::AgentEvent::TurnComplete => println!(),
                                        synaps_cli::transport::AgentEvent::Error(e) => eprintln!("\x1b[31mError: {}\x1b[0m", e),
                                        synaps_cli::transport::AgentEvent::Tool(synaps_cli::transport::ToolEvent::Invoke { tool_name, .. }) => {
                                            eprintln!("\x1b[33m⚡ {}\x1b[0m", tool_name);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        } else if let Ok(WatcherResponse::Error { message }) = serde_json::from_str::<WatcherResponse>(trimmed) {
                            eprintln!("Error: {}", message);
                            std::process::exit(1);
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        "help" | "--help" | "-h" => {
            println!("watcher — Autonomous agent supervisor");
            println!();
            println!("USAGE:");
            println!("  watcher run                 Start supervisor daemon (foreground)");
            println!("  watcher deploy <name>       Deploy/start an agent");
            println!("  watcher stop <name>         Stop an agent");  
            println!("  watcher attach <name>       Attach to a running agent's event stream");
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