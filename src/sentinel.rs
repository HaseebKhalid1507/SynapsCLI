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
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use synaps_cli::{AgentConfig, SentinelCommand, SentinelResponse, AgentStatusInfo};
use tokio::sync::Mutex;
use tokio::net::{UnixListener, UnixStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    fn current_uptime_secs(&self) -> Option<f64> {
        if self.is_running() {
            self.last_start.map(|s| s.elapsed().as_secs_f64())
        } else {
            None
        }
    }

    fn to_status_info(&self) -> AgentStatusInfo {
        AgentStatusInfo {
            name: self.name.clone(),
            trigger: self.config.agent.trigger.clone(),
            status: self.status_str().to_string(),
            session_count: self.session_count,
            uptime_secs: self.current_uptime_secs(),
            pid: self.pid,
            consecutive_crashes: self.consecutive_crashes,
        }
    }
}

/// Handle IPC command from CLI
async fn handle_ipc_command(
    command: SentinelCommand,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
) -> SentinelResponse {
    match command {
        SentinelCommand::Deploy { name } => {
            let mut agents = agents.lock().await;
            
            // Check if agent config exists
            let config_path = sentinel_dir().join(&name).join("config.toml");
            if !config_path.exists() {
                return SentinelResponse::Error {
                    message: format!("Agent '{}' not found. Run: sentinel init {}", name, name)
                };
            }

            // Load config
            let config = match AgentConfig::load(&config_path) {
                Ok(config) => config,
                Err(e) => return SentinelResponse::Error {
                    message: format!("Failed to load agent '{}': {}", name, e)
                }
            };

            // Check if already exists in map
            if let Some(agent) = agents.get_mut(&name) {
                if agent.is_running() {
                    return SentinelResponse::Error {
                        message: format!("Agent '{}' is already running", name)
                    };
                }
                // Un-stop it and restart if needed
                agent.stopped = false;
                if agent.config.agent.trigger == "always" {
                    match spawn_agent(agent, "deploy restart").await {
                        Ok(()) => SentinelResponse::Ok {
                            message: format!("Agent '{}' deployed and started", name)
                        },
                        Err(e) => SentinelResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    SentinelResponse::Ok {
                        message: format!("Agent '{}' deployed", name)
                    }
                }
            } else {
                // Add new agent
                let mut agent = ManagedAgent::new(name.clone(), config_path, config);
                
                if agent.config.agent.trigger == "always" {
                    match spawn_agent(&mut agent, "deploy start").await {
                        Ok(()) => {
                            agents.insert(name.clone(), agent);
                            SentinelResponse::Ok {
                                message: format!("Agent '{}' deployed and started", name)
                            }
                        },
                        Err(e) => SentinelResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    agents.insert(name.clone(), agent);
                    SentinelResponse::Ok {
                        message: format!("Agent '{}' deployed", name)
                    }
                }
            }
        }

        SentinelCommand::Stop { name } => {
            let mut agents = agents.lock().await;
            if let Some(agent) = agents.get_mut(&name) {
                agent.stopped = true;
                if let Some(ref mut child) = agent.child {
                    let _ = child.kill().await;
                }
                SentinelResponse::Ok {
                    message: format!("Agent '{}' stopped", name)
                }
            } else {
                SentinelResponse::Error {
                    message: format!("Agent '{}' not found or not running", name)
                }
            }
        }

        SentinelCommand::Status => {
            let agents = agents.lock().await;
            let agent_info: Vec<AgentStatusInfo> = agents.values()
                .map(|agent| agent.to_status_info())
                .collect();
            SentinelResponse::Status { agents: agent_info }
        }

        SentinelCommand::AgentStatus { name } => {
            let agents = agents.lock().await;
            if let Some(agent) = agents.get(&name) {
                SentinelResponse::AgentDetail {
                    info: agent.to_status_info()
                }
            } else {
                SentinelResponse::Error {
                    message: format!("Agent '{}' not found", name)
                }
            }
        }
    }
}

/// IPC listener task
async fn ipc_listener(agents: Arc<Mutex<HashMap<String, ManagedAgent>>>) {
    let socket_path = sentinel_dir().join("sentinel.sock");
    
    // Clean up any existing socket
    let _ = std::fs::remove_file(&socket_path);
    
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            log(&format!("Failed to bind IPC socket: {}", e));
            return;
        }
    };

    log(&format!("IPC listening on {}", socket_path.display()));

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let agents = agents.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ipc_connection(stream, agents).await {
                        log(&format!("IPC connection error: {}", e));
                    }
                });
            }
            Err(e) => {
                log(&format!("IPC accept error: {}", e));
                break;
            }
        }
    }
}

/// Handle a single IPC connection
async fn handle_ipc_connection(
    mut stream: UnixStream,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    
    reader.read_line(&mut line).await?;
    let command: SentinelCommand = serde_json::from_str(line.trim())?;
    
    let response = handle_ipc_command(command, agents).await;
    let response_json = serde_json::to_string(&response)?;
    
    stream.write_all(response_json.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    
    Ok(())
}

/// Send command to supervisor via IPC
async fn send_ipc_command(command: SentinelCommand) -> Result<SentinelResponse, String> {
    let socket_path = sentinel_dir().join("sentinel.sock");
    
    if !socket_path.exists() {
        return Err("Supervisor not running. Start with: sentinel run".to_string());
    }

    let mut stream = UnixStream::connect(&socket_path).await
        .map_err(|e| format!("Failed to connect to supervisor: {}", e))?;
    
    let command_json = serde_json::to_string(&command)
        .map_err(|e| format!("Failed to serialize command: {}", e))?;
    
    stream.write_all(command_json.as_bytes()).await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    stream.write_all(b"\n").await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    stream.flush().await
        .map_err(|e| format!("Failed to send command: {}", e))?;
    
    let mut reader = BufReader::new(&mut stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await
        .map_err(|e| format!("Failed to read response: {}", e))?;
    
    serde_json::from_str(response_line.trim())
        .map_err(|e| format!("Failed to parse response: {}", e))
}

/// Format uptime duration nicely
fn format_uptime(secs: f64) -> String {
    let secs = secs as u64;
    if secs < 60 { format!("{}s", secs) }
    else if secs < 3600 { format!("{}m {}s", secs / 60, secs % 60) }
    else { format!("{}h {}m", secs / 3600, (secs % 3600) / 60) }
}

/// Print status response in table format
fn print_status_table(agents: Vec<AgentStatusInfo>) {
    if agents.is_empty() {
        println!("No agents configured. Run: sentinel init <name>");
        return;
    }
    
    println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<10}", "AGENT", "TRIGGER", "STATUS", "SESSION", "UPTIME", "PID");
    println!("{}", "─".repeat(75));
    
    for agent in agents {
        let uptime = agent.uptime_secs.map(format_uptime).unwrap_or_else(|| "—".to_string());
        let pid = agent.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".to_string());
        let session = if agent.session_count > 0 { 
            format!("#{}", agent.session_count) 
        } else { 
            "—".to_string() 
        };
        
        println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<10}",
            agent.name,
            agent.trigger,
            agent.status,
            session,
            uptime,
            pid
        );
    }
}

/// Print detailed agent status
fn print_agent_detail(info: AgentStatusInfo) {
    println!("Agent: {}", info.name);
    println!("Trigger: {}", info.trigger);
    
    let session_str = if info.session_count > 0 {
        format!("{} (session #{})", info.status, info.session_count)
    } else {
        info.status
    };
    println!("Status: {}", session_str);
    
    if let Some(pid) = info.pid {
        println!("PID: {}", pid);
    }
    if let Some(uptime) = info.uptime_secs {
        println!("Uptime: {}", format_uptime(uptime));
    }
    println!("Sessions: {}", info.session_count);
    println!("Crashes: {}", info.consecutive_crashes);
}
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

fn format_log_entry(entry: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(entry) {
        let ts_str = json["ts"].as_str().unwrap_or("??:??:??");
        let timestamp = if let Some(time_part) = ts_str.split('T').nth(1) {
            time_part.split('.').next().unwrap_or(time_part)
        } else {
            ts_str
        };

        let log_type = json["type"].as_str().unwrap_or("unknown");
        
        match log_type {
            "boot" => {
                let session = json["session"].as_u64().unwrap_or(0);
                let model = json["model"].as_str().unwrap_or("unknown");
                let trigger = json["trigger"].as_str().unwrap_or("unknown");
                Some(format!("[{}] BOOT session #{} (model: {}, trigger: {})", timestamp, session, model, trigger))
            }
            "tool_start" => {
                let name = json["name"].as_str().unwrap_or("unknown");
                let call_num = json["call_num"].as_u64().unwrap_or(0);
                Some(format!("[{}] TOOL {} (#{}) ", timestamp, name, call_num))
            }
            "tool_result" => {
                let preview = json["preview"].as_str().unwrap_or("").chars().take(80).collect::<String>();
                Some(format!("[{}]   → {}", timestamp, preview))
            }
            "usage" => {
                let input_tokens = json["input_tokens"].as_u64().unwrap_or(0);
                let output_tokens = json["output_tokens"].as_u64().unwrap_or(0);
                let total_tokens = json["total_tokens"].as_u64().unwrap_or(0);
                let cost = json["cost"].as_f64().unwrap_or(0.0);
                Some(format!("[{}] USAGE +{}/+{} tokens (total: {}, cost: ${:.4})", 
                    timestamp, input_tokens, output_tokens, total_tokens, cost))
            }
            "text" => {
                let length = json["length"].as_u64().unwrap_or(0);
                let preview = json["preview"].as_str().unwrap_or("").chars().take(80).collect::<String>();
                Some(format!("[{}] TEXT {} chars: {}", timestamp, length, preview))
            }
            "exit" => {
                let reason = json["reason"].as_str().unwrap_or("unknown");
                let total_tokens = json["total_tokens"].as_u64().unwrap_or(0);
                let total_cost = json["total_cost"].as_f64().unwrap_or(0.0);
                let tool_calls = json["tool_calls"].as_u64().unwrap_or(0);
                let duration_secs = json["duration_secs"].as_u64().unwrap_or(0);
                Some(format!("[{}] EXIT {} ({} tokens, ${:.2}, {} tool calls, {}s)", 
                    timestamp, reason, total_tokens, total_cost, tool_calls, duration_secs))
            }
            _ => Some(format!("[{}] {}: {}", timestamp, log_type.to_uppercase(), entry))
        }
    } else {
        None
    }
}

fn find_latest_session_file(logs_dir: &Path) -> Result<PathBuf, String> {
    let mut max_session = 0;
    let mut found_any = false;
    
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("session-") && name_str.ends_with(".jsonl") {
                found_any = true;
                if let Ok(num) = name_str.trim_start_matches("session-").trim_end_matches(".jsonl").parse::<u64>() {
                    if num > max_session {
                        max_session = num;
                    }
                }
            }
        }
    }
    
    if !found_any {
        return Err("No session logs found".to_string());
    }
    
    Ok(logs_dir.join(format!("session-{:03}.jsonl", max_session)))
}

async fn show_logs(name: &str, follow: bool, session_num: Option<u64>, last_n: Option<usize>) -> Result<(), String> {
    let logs_dir = sentinel_dir().join(name).join("logs");
    
    if !logs_dir.exists() {
        return Err(format!("Agent '{}' has no logs directory", name));
    }

    let log_file = if let Some(session) = session_num {
        logs_dir.join(format!("session-{:03}.jsonl", session))
    } else {
        find_latest_session_file(&logs_dir)?
    };

    if !log_file.exists() {
        return Err(format!("Log file {:?} does not exist", log_file));
    }

    if follow {
        // For follow mode, use current.log if available
        let current_log = logs_dir.join("current.log");
        let follow_path = if current_log.exists() {
            if let Ok(contents) = std::fs::read_to_string(&current_log) {
                PathBuf::from(contents.trim())
            } else {
                log_file
            }
        } else {
            log_file
        };

        // Initial read
        if let Ok(contents) = std::fs::read_to_string(&follow_path) {
            for line in contents.lines() {
                if let Some(formatted) = format_log_entry(line) {
                    println!("{}", formatted);
                }
            }
        }

        // Poll for new lines
        let mut last_size = std::fs::metadata(&follow_path).map(|m| m.len()).unwrap_or(0);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            
            if let Ok(metadata) = std::fs::metadata(&follow_path) {
                let current_size = metadata.len();
                if current_size > last_size {
                    if let Ok(contents) = std::fs::read_to_string(&follow_path) {
                        let new_content = &contents[(last_size as usize)..];
                        for line in new_content.lines() {
                            if !line.trim().is_empty() {
                                if let Some(formatted) = format_log_entry(line) {
                                    println!("{}", formatted);
                                }
                            }
                        }
                        last_size = current_size;
                    }
                }
            }
        }
    } else {
        // Read and display log file
        let contents = std::fs::read_to_string(&log_file)
            .map_err(|e| format!("Failed to read log file: {}", e))?;
        
        let mut lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        
        if let Some(n) = last_n {
            if lines.len() > n {
                lines = lines[(lines.len() - n)..].to_vec();
            }
        }

        for line in lines {
            if let Some(formatted) = format_log_entry(line) {
                println!("{}", formatted);
            }
        }
    }

    Ok(())
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

            // Setup socket and PID file paths
            let socket_path = sentinel_dir().join("sentinel.sock");
            let pid_path = sentinel_dir().join("sentinel.pid");
            
            // Clean up socket and write PID
            let _ = std::fs::remove_file(&socket_path);
            std::fs::create_dir_all(sentinel_dir()).unwrap_or_else(|e| {
                eprintln!("Failed to create sentinel directory: {}", e);
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
                    log("no agents configured — run 'sentinel init <name>' first");
                    std::process::exit(0);
                }
            }

            // Start IPC listener
            let ipc_agents = agents.clone();
            tokio::spawn(async move {
                ipc_listener(ipc_agents).await;
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

            // Setup signal handling
            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                r.store(false, std::sync::atomic::Ordering::Relaxed);
            });

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
                // Detailed status for specific agent
                match send_ipc_command(SentinelCommand::AgentStatus { name: agent_name.clone() }).await {
                    Ok(SentinelResponse::AgentDetail { info }) => {
                        print_agent_detail(info);
                    }
                    Ok(SentinelResponse::Error { message }) => {
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
            } else {
                // Overall status
                match send_ipc_command(SentinelCommand::Status).await {
                    Ok(SentinelResponse::Status { agents }) => {
                        print_status_table(agents);
                    }
                    Ok(SentinelResponse::Error { message }) => {
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
                eprintln!("Usage: sentinel deploy <name>");
                std::process::exit(1);
            });
            
            match send_ipc_command(SentinelCommand::Deploy { name: name.clone() }).await {
                Ok(SentinelResponse::Ok { message }) => {
                    println!("{}", message);
                }
                Ok(SentinelResponse::Error { message }) => {
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
                eprintln!("Usage: sentinel stop <name>");
                std::process::exit(1);
            });
            
            match send_ipc_command(SentinelCommand::Stop { name: name.clone() }).await {
                Ok(SentinelResponse::Ok { message }) => {
                    println!("{}", message);
                }
                Ok(SentinelResponse::Error { message }) => {
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
                eprintln!("Usage: sentinel logs <name> [--follow | --session N | --last N]");
                std::process::exit(1);
            });

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
            println!("sentinel — Autonomous agent supervisor");
            println!();
            println!("USAGE:");
            println!("  sentinel run                 Start supervisor daemon (foreground)");
            println!("  sentinel deploy <name>       Deploy/start an agent");
            println!("  sentinel stop <name>         Stop an agent");  
            println!("  sentinel once <name>         Run agent once without supervision");
            println!("  sentinel init <name>         Create new agent from template");
            println!("  sentinel list                List configured agents");
            println!("  sentinel status              Show all agent statuses");
            println!("  sentinel status <name>       Show detailed status for agent");
            println!("  sentinel logs <name>         Show latest session log");
            println!("  sentinel logs <name> --follow  Tail current session log");
            println!("  sentinel logs <name> --session N  Show specific session");
            println!("  sentinel help                Show this help");
            println!();
            println!("AGENTS DIR: {}", sentinel_dir().display());
        }

        _ => {
            eprintln!("Unknown command: {}", command);
            eprintln!("Run 'sentinel help' for usage information");
            std::process::exit(1);
        }
    }
}
