//! sentinel — Autonomous agent supervisor daemon
//!
//! Spawns, monitors, and restarts agent worker processes.
//! Manages agent lifecycles with heartbeat monitoring and crash recovery.
//!
//! Usage:
//!   sentinel run                    — start supervisor daemon (foreground)
//!   sentinel deploy <name>          — start supervising an agent
//!   sentinel stop <name>            — stop an agent
//!   sentinel status                 — show all agent statuses
//!   sentinel list                   — list configured agents
//!   sentinel init <name>            — create agent from template
//!   sentinel once <name>            — run agent once, no supervision
//!   sentinel logs <name>            — show agent logs

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use synaps_cli::AgentConfig;

fn sentinel_dir() -> PathBuf {
    synaps_cli::config::base_dir().join("sentinel")
}

fn agent_binary() -> PathBuf {
    // Find synaps-agent binary next to the sentinel binary
    let current_exe = std::env::current_exe().unwrap_or_default();
    let dir = current_exe.parent().unwrap_or(std::path::Path::new("."));
    dir.join("synaps-agent")
}

fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [sentinel] {}", ts, msg);
}

/// State for a managed agent
struct ManagedAgent {
    name: String,
    config_path: PathBuf,
    config: AgentConfig,
    child: Option<tokio::process::Child>,
    pid: Option<u32>,
    session_count: u64,
    consecutive_crashes: u32,
    last_start: Option<Instant>,
    total_uptime_secs: f64,
    stopped: bool, // manually stopped, don't restart
}

impl ManagedAgent {
    fn new(name: String, config_path: PathBuf, config: AgentConfig) -> Self {
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

    fn is_running(&self) -> bool {
        self.child.is_some()
    }

    fn status_str(&self) -> &str {
        if self.stopped {
            "stopped"
        } else if self.is_running() {
            "running"
        } else {
            "sleeping"
        }
    }

    fn uptime_str(&self) -> String {
        if let Some(start) = self.last_start {
            if self.is_running() {
                let secs = start.elapsed().as_secs();
                if secs < 60 { format!("{}s", secs) }
                else if secs < 3600 { format!("{}m {}s", secs / 60, secs % 60) }
                else { format!("{}h {}m", secs / 3600, (secs % 3600) / 60) }
            } else { "—".to_string() }
        } else { "—".to_string() }
    }
}

/// Discover all configured agents
fn discover_agents() -> Vec<(String, PathBuf)> {
    let dir = sentinel_dir();
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let config_path = entry.path().join("config.toml");
                if config_path.exists() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    agents.push((name, config_path));
                }
            }
        }
    }
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    agents
}

/// Spawn an agent worker process
async fn spawn_agent(agent: &mut ManagedAgent, trigger_context: &str) -> Result<(), String> {
    let bin = agent_binary();
    if !bin.exists() {
        return Err(format!("synaps-agent binary not found at {}", bin.display()));
    }

    log(&format!("[{}] spawning session #{}", agent.name, agent.session_count + 1));

    let child = tokio::process::Command::new(&bin)
        .arg("--config")
        .arg(&agent.config_path)
        .arg("--trigger-context")
        .arg(trigger_context)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn agent: {}", e))?;

    agent.pid = child.id();
    agent.child = Some(child);
    agent.session_count += 1;
    agent.last_start = Some(Instant::now());

    log(&format!("[{}] started (pid: {:?})", agent.name, agent.pid));
    Ok(())
}

/// Check heartbeat freshness
fn check_heartbeat(agent_dir: &std::path::Path, stale_threshold: u64) -> bool {
    let hb_path = agent_dir.join("heartbeat");
    if let Ok(content) = std::fs::read_to_string(&hb_path) {
        if let Ok(ts) = content.trim().parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            return (now - ts).unsigned_abs() < stale_threshold;
        }
    }
    false
}

/// Create agent from template
fn init_agent(name: &str) -> Result<(), String> {
    let dir = sentinel_dir().join(name);
    if dir.exists() {
        return Err(format!("Agent '{}' already exists at {}", name, dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory: {}", e))?;

    let config = format!(r#"[agent]
name = "{name}"
model = "claude-sonnet-4-20250514"
thinking = "medium"
trigger = "always"

[limits]
max_session_tokens = 100000
max_session_duration_mins = 60
max_session_cost_usd = 0.50
max_daily_cost_usd = 10.00
cooldown_secs = 10
max_retries = 3

[boot]
message = """
You are waking up for a new session. Current time: {{timestamp}}

## State from your last session:
{{handoff}}

## What triggered this session:
{{trigger_context}}

Review your state, decide what to do, and get to work. When done, call sentinel_exit.
"""

[heartbeat]
interval_secs = 30
stale_threshold_secs = 120
"#);

    let soul = format!("# {name}\n\nYou are {name}, an autonomous agent.\n\nDescribe your purpose and personality here.\n");

    std::fs::write(dir.join("config.toml"), config).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("soul.md"), soul).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("handoff.json"), "{}").map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir.join("logs")).map_err(|e| e.to_string())?;

    println!("✓ Agent '{}' created at {}", name, dir.display());
    println!("  Edit soul.md to define the agent's identity");
    println!("  Edit config.toml to tune limits and trigger mode");
    println!("  Run: sentinel deploy {}", name);
    Ok(())
}

fn print_status(agents: &HashMap<String, ManagedAgent>) {
    if agents.is_empty() {
        println!("No agents configured. Run: sentinel init <name>");
        return;
    }
    println!("{:<15} {:<10} {:<10} {:<10} {:<10}", "AGENT", "TRIGGER", "STATUS", "SESSION", "UPTIME");
    println!("{}", "─".repeat(55));
    for (_, agent) in agents.iter() {
        println!("{:<15} {:<10} {:<10} {:<10} {:<10}",
            agent.name,
            agent.config.agent.trigger,
            agent.status_str(),
            if agent.session_count > 0 { format!("#{}", agent.session_count) } else { "—".to_string() },
            agent.uptime_str(),
        );
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "init" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: sentinel init <name>");
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
                println!("No agents configured. Run: sentinel init <name>");
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
                eprintln!("Usage: sentinel once <name>");
                std::process::exit(1);
            });
            let config_path = sentinel_dir().join(name).join("config.toml");
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
            // Main supervisor loop
            log("starting supervisor");

            let mut agents: HashMap<String, ManagedAgent> = HashMap::new();

            // Load all agents
            for (name, config_path) in discover_agents() {
                match AgentConfig::load(&config_path) {
                    Ok(config) => {
                        log(&format!("loaded agent: {} (trigger: {})", name, config.agent.trigger));
                        agents.insert(name.clone(), ManagedAgent::new(name, config_path, config));
                    }
                    Err(e) => {
                        log(&format!("WARN: failed to load {}: {}", name, e));
                    }
                }
            }

            if agents.is_empty() {
                log("no agents configured — run 'sentinel init <name>' first");
                std::process::exit(0);
            }

            // Start always-on agents
            for (name, agent) in agents.iter_mut() {
                if agent.config.agent.trigger == "always" {
                    if let Err(e) = spawn_agent(agent, "supervisor start (always-on)").await {
                        log(&format!("[{}] failed to start: {}", name, e));
                    }
                }
            }

            // Setup signal handling
            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                r.store(false, std::sync::atomic::Ordering::Relaxed);
            });

            // Supervisor loop — check agents every 5 seconds
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                for (name, agent) in agents.iter_mut() {
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
                                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                                        
                                        if running.load(std::sync::atomic::Ordering::Relaxed) {
                                            let ctx = if code == 0 { "automatic restart (always-on)" }
                                                      else { "crash recovery restart" };
                                            if let Err(e) = spawn_agent(agent, ctx).await {
                                                log(&format!("[{}] failed to restart: {}", name, e));
                                            }
                                        }
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
                                    }
                                }
                            }
                            Err(e) => {
                                log(&format!("[{}] error checking child: {}", name, e));
                            }
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }

            // Graceful shutdown — kill all running agents
            log("shutting down — stopping all agents");
            for (name, agent) in agents.iter_mut() {
                if let Some(ref mut child) = agent.child {
                    log(&format!("[{}] sending SIGTERM", name));
                    let _ = child.kill().await;
                    // Give it time to write handoff
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            log("supervisor stopped");
        }

        "status" => {
            // Quick status from PID/heartbeat files
            let discovered = discover_agents();
            let mut agents: HashMap<String, ManagedAgent> = HashMap::new();
            for (name, config_path) in discovered {
                if let Ok(config) = AgentConfig::load(&config_path) {
                    agents.insert(name.clone(), ManagedAgent::new(name, config_path, config));
                }
            }
            print_status(&agents);
        }

        "deploy" | "stop" => {
            eprintln!("'sentinel {}' requires the supervisor to be running.", command);
            eprintln!("Start the supervisor first: sentinel run");
            eprintln!("Or use: sentinel once <name>  (run without supervisor)");
            std::process::exit(1);
        }

        "help" | "--help" | "-h" | _ => {
            println!("sentinel — Autonomous agent supervisor");
            println!();
            println!("USAGE:");
            println!("  sentinel run              Start supervisor daemon (foreground)");
            println!("  sentinel once <name>      Run agent once without supervision");
            println!("  sentinel init <name>      Create new agent from template");
            println!("  sentinel list             List configured agents");
            println!("  sentinel status           Show agent statuses");
            println!("  sentinel help             Show this help");
            println!();
            println!("AGENTS DIR: {}", sentinel_dir().display());
        }
    }
}
