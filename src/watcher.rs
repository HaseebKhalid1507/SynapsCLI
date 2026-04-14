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

fn watcher_dir() -> PathBuf {
    synaps_cli::config::base_dir().join("watcher")
}

fn agent_binary() -> PathBuf {
    // Find synaps-agent binary next to the watcher binary
    let current_exe = std::env::current_exe().unwrap_or_default();
    let dir = current_exe.parent().unwrap_or(std::path::Path::new("."));
    dir.join("synaps-agent")
}

fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [watcher] {}", ts, msg);
}

fn validate_agent_name(name: &str) -> Result<(), String> {
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

fn load_agent_stats(agent_dir: &std::path::Path) -> synaps_cli::watcher_types::AgentStats {
    let path = agent_dir.join("stats.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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


    fn current_uptime_secs(&self) -> Option<f64> {
        if self.is_running() {
            self.last_start.map(|s| s.elapsed().as_secs_f64())
        } else {
            None
        }
    }

    fn to_status_info(&self) -> AgentStatusInfo {
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

/// Handle IPC command from CLI
async fn handle_ipc_command(
    command: WatcherCommand,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
) -> WatcherResponse {
    match command {
        WatcherCommand::Deploy { name } => {
            // Validate agent name
            if let Err(e) = validate_agent_name(&name) {
                return WatcherResponse::Error { message: e };
            }

            let mut agents = agents.lock().await;
            
            // Check if agent config exists
            let config_path = watcher_dir().join(&name).join("config.toml");
            if !config_path.exists() {
                return WatcherResponse::Error {
                    message: format!("Agent '{}' not found. Run: watcher init {}", name, name)
                };
            }

            // Load config
            let config = match AgentConfig::load(&config_path) {
                Ok(config) => config,
                Err(e) => return WatcherResponse::Error {
                    message: format!("Failed to load agent '{}': {}", name, e)
                }
            };

            // Check if already exists in map
            if let Some(agent) = agents.get_mut(&name) {
                if agent.is_running() {
                    return WatcherResponse::Error {
                        message: format!("Agent '{}' is already running", name)
                    };
                }
                // Un-stop it and restart if needed
                agent.stopped = false;
                if agent.config.agent.trigger == "always" {
                    match spawn_agent(agent, "deploy restart").await {
                        Ok(()) => WatcherResponse::Ok {
                            message: format!("Agent '{}' deployed and started", name)
                        },
                        Err(e) => WatcherResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    WatcherResponse::Ok {
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
                            WatcherResponse::Ok {
                                message: format!("Agent '{}' deployed and started", name)
                            }
                        },
                        Err(e) => WatcherResponse::Error {
                            message: format!("Failed to start agent '{}': {}", name, e)
                        }
                    }
                } else {
                    agents.insert(name.clone(), agent);
                    WatcherResponse::Ok {
                        message: format!("Agent '{}' deployed", name)
                    }
                }
            }
        }

        WatcherCommand::Stop { name } => {
            let mut agents = agents.lock().await;
            if let Some(agent) = agents.get_mut(&name) {
                agent.stopped = true;
                if let Some(ref mut child) = agent.child {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                WatcherResponse::Ok {
                    message: format!("Agent '{}' stopped", name)
                }
            } else {
                WatcherResponse::Error {
                    message: format!("Agent '{}' not found or not running", name)
                }
            }
        }

        WatcherCommand::Status => {
            let agents = agents.lock().await;
            let agent_info: Vec<AgentStatusInfo> = agents.values()
                .map(|agent| agent.to_status_info())
                .collect();
            WatcherResponse::Status { agents: agent_info }
        }

        WatcherCommand::AgentStatus { name } => {
            let agents = agents.lock().await;
            if let Some(agent) = agents.get(&name) {
                WatcherResponse::AgentDetail {
                    info: agent.to_status_info()
                }
            } else {
                WatcherResponse::Error {
                    message: format!("Agent '{}' not found", name)
                }
            }
        }
    }
}

/// IPC listener task
async fn ipc_listener(agents: Arc<Mutex<HashMap<String, ManagedAgent>>>) {
    let socket_path = watcher_dir().join("watcher.sock");
    
    // Check if socket exists and test if it's alive
    if socket_path.exists() {
        // Try to connect to existing socket
        if tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path)).await.is_ok() {
            log("Another supervisor is already running");
            std::process::exit(1);
        } else {
            // Stale socket - remove it
            log("Removing stale socket");
            let _ = std::fs::remove_file(&socket_path);
        }
    }
    
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            log(&format!("Failed to bind IPC socket: {}", e));
            return;
        }
    };

    // Set socket permissions to owner-only
    if let Err(e) = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)) {
        log(&format!("Failed to set socket permissions: {}", e));
        return;
    }

    log(&format!("IPC listening on {}", socket_path.display()));

    let semaphore = Arc::new(Semaphore::new(10)); // Max 10 concurrent IPC connections

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let agents = agents.clone();
                let permit = semaphore.clone().try_acquire_owned();
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _ = handle_ipc_connection(stream, agents).await;
                            drop(permit); // Release on completion
                        });
                    }
                    Err(_) => {
                        // Too many connections — drop this one
                        log("IPC: too many concurrent connections, dropping");
                    }
                }
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
    let command: WatcherCommand = serde_json::from_str(line.trim())?;
    
    let response = handle_ipc_command(command, agents).await;
    let response_json = serde_json::to_string(&response)?;
    
    stream.write_all(response_json.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    
    Ok(())
}

/// Send command to supervisor via IPC
async fn send_ipc_command(command: WatcherCommand) -> Result<WatcherResponse, String> {
    let socket_path = watcher_dir().join("watcher.sock");
    if !socket_path.exists() {
        return Err("Supervisor not running. Start with: watcher run".to_string());
    }
    
    // Add timeout to avoid hanging on stale socket
    let connect_result = tokio::time::timeout(
        Duration::from_secs(5),
        UnixStream::connect(&socket_path)
    ).await;
    
    let mut stream = match connect_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => {
            // Socket exists but can't connect — stale
            return Err("Supervisor socket is stale. Remove it and restart: watcher run".to_string());
        }
        Err(_) => {
            return Err("Supervisor not responding (timeout). Try: watcher run".to_string());
        }
    };
    
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
        println!("No agents configured. Run: watcher init <name>");
        return;
    }
    
    println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<12}", "AGENT", "TRIGGER", "STATUS", "SESSION", "UPTIME", "COST TODAY");
    println!("{}", "─".repeat(80));
    
    for agent in agents {
        let uptime = agent.uptime_secs.map(format_uptime).unwrap_or_else(|| "—".to_string());
        let session = if agent.session_count > 0 { 
            format!("#{}", agent.session_count) 
        } else { 
            "—".to_string() 
        };
        let cost = format!("${:.2}/${:.2}", agent.cost_today, agent.cost_limit);
        
        println!("{:<15} {:<10} {:<10} {:<10} {:<10} {:<12}",
            agent.name,
            agent.trigger,
            agent.status,
            session,
            uptime,
            cost
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
    println!("Model: {}", info.model);
    
    if let Some(pid) = info.pid {
        println!("PID: {}", pid);
    }
    if let Some(uptime) = info.uptime_secs {
        println!("Uptime: {}", format_uptime(uptime));
    }
    
    println!("Sessions: {} (total) / {} (today)", info.total_sessions, 
        if info.session_count > 0 { info.session_count } else { 0 });
    println!("Cost: ${:.2} today / ${:.2} limit", info.cost_today, info.cost_limit);
    
    // Format tokens with commas
    let tokens_formatted = format_number_with_commas(info.tokens_today);
    println!("Tokens: {} today", tokens_formatted);
    
    println!("Crashes: {}", info.consecutive_crashes);
}

/// Format numbers with commas for readability
fn format_number_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.insert(0, ',');
        }
        result.insert(0, c);
    }
    result
}
fn discover_agents() -> Vec<(String, PathBuf)> {
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
    let logs_dir = watcher_dir().join(name).join("logs");
    
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
    // Validate agent name
    validate_agent_name(name)?;
    
    let dir = watcher_dir().join(name);
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

Review your state, decide what to do, and get to work. When done, call watcher_exit.
"""

[heartbeat]
interval_secs = 30
stale_threshold_secs = 120
"#);

    let soul = format!("# {name}\n\nYou are {name}, an autonomous agent.\n\nDescribe your purpose and personality here.\n");

    std::fs::write(dir.join("config.toml"), config).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("soul.md"), soul).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("handoff.json"), "{}").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("stats.json"), "{}").map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir.join("logs")).map_err(|e| e.to_string())?;

    println!("✓ Agent '{}' created at {}", name, dir.display());
    println!("  Edit soul.md to define the agent's identity");
    println!("  Edit config.toml to tune limits and trigger mode");
    println!("  Run: watcher deploy {}", name);
    Ok(())
}

fn print_status(agents: &HashMap<String, ManagedAgent>) {
    if agents.is_empty() {
        println!("No agents configured. Run: watcher init <name>");
        return;
    }
    let infos: Vec<AgentStatusInfo> = agents.values().map(|a| a.to_status_info()).collect();
    print_status_table(infos);
}

/// Expand ~ in a path string to the home directory
fn expand_watch_path(p: &str) -> PathBuf {
    if p.starts_with("~/") {
        if let Some(home) = dirs_next() {
            return home.join(p.strip_prefix("~/").unwrap());
        }
    }
    PathBuf::from(p)
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Check if a path matches the configured glob patterns (empty patterns = match all)
fn matches_patterns(path: &Path, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    for pattern in patterns {
        if let Ok(glob) = globset::Glob::new(pattern) {
            let matcher = glob.compile_matcher();
            if matcher.is_match(file_name) {
                return true;
            }
        }
    }
    false
}

/// Spawn a file-watching task for a watch-trigger agent.
/// Runs in its own tokio task, watches directories, debounces events,
/// and spawns the agent when files change.
fn spawn_watch_task(
    agent_name: String,
    config: AgentConfig,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
) {
    let trigger_config = config.trigger.clone();

    tokio::spawn(async move {
        // Validate paths
        let watch_paths: Vec<PathBuf> = trigger_config.paths.iter()
            .map(|p| expand_watch_path(p))
            .collect();

        if watch_paths.is_empty() {
            log(&format!("[{}] watch trigger has no paths configured — skipping", agent_name));
            return;
        }

        // Validate that paths exist
        for p in &watch_paths {
            if !p.exists() {
                log(&format!("[{}] creating watched directory: {}", agent_name, p.display()));
                let _ = std::fs::create_dir_all(p);
            }
        }

        log(&format!("[{}] watching {} path(s): {}",
            agent_name,
            watch_paths.len(),
            watch_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ));

        let patterns = trigger_config.patterns.clone();
        let debounce_secs = trigger_config.debounce_secs;

        // Main watch loop — restarts the watcher after each agent session
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            // Set up notify watcher with a crossbeam channel
            let (tx, rx) = std::sync::mpsc::channel();
            let mut notify_watcher: notify::RecommendedWatcher = match notify::RecommendedWatcher::new(
                tx,
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    log(&format!("[{}] failed to create file watcher: {}", agent_name, e));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };

            // Watch all configured paths
            for path in &watch_paths {
                if let Err(e) = notify_watcher.watch(path, notify::RecursiveMode::Recursive) {
                    log(&format!("[{}] failed to watch {}: {}", agent_name, path.display(), e));
                }
            }

            // Wait for events with debounce
            let changed_paths = tokio::task::spawn_blocking({
                let patterns = patterns.clone();
                let agent_name = agent_name.clone();
                let running = running.clone();
                let debounce = Duration::from_secs(debounce_secs);

                move || -> HashSet<PathBuf> {
                    let mut changed: HashSet<PathBuf> = HashSet::new();

                    // Block until first event
                    loop {
                        if !running.load(std::sync::atomic::Ordering::Relaxed) {
                            return changed;
                        }
                        // Use recv_timeout so we can check the running flag periodically
                        match rx.recv_timeout(Duration::from_secs(2)) {
                            Ok(Ok(event)) => {
                                for path in &event.paths {
                                    if matches_patterns(path, &patterns) {
                                        changed.insert(path.to_path_buf());
                                    }
                                }
                                if !changed.is_empty() {
                                    break; // Got first matching event, start debounce
                                }
                            }
                            Ok(Err(e)) => {
                                eprintln!("[watcher] [{}] notify error: {}", agent_name, e);
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return changed,
                        }
                    }

                    // Debounce: keep collecting events until quiet for debounce_secs
                    loop {
                        match rx.recv_timeout(debounce) {
                            Ok(Ok(event)) => {
                                for path in &event.paths {
                                    if matches_patterns(path, &patterns) {
                                        changed.insert(path.to_path_buf());
                                    }
                                }
                                // Reset debounce timer by continuing
                            }
                            Ok(Err(_)) => continue,
                            Err(_) => break, // Timeout = debounce complete
                        }
                    }

                    changed
                }
            }).await.unwrap_or_default();

            // Drop the watcher to release inotify watches while agent runs
            drop(notify_watcher);

            if changed_paths.is_empty() || !running.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            // Build trigger context with changed file paths
            let paths_str: Vec<String> = changed_paths.iter()
                .map(|p| p.display().to_string())
                .collect();
            let trigger_context = format!("files changed:\n{}", paths_str.join("\n"));

            log(&format!("[{}] triggered by {} file(s)", agent_name, paths_str.len()));

            // Spawn the agent
            {
                let mut agents_map = agents.lock().await;
                if let Some(agent) = agents_map.get_mut(&agent_name) {
                    if agent.stopped {
                        log(&format!("[{}] agent is stopped — ignoring trigger", agent_name));
                        continue;
                    }
                    if agent.is_running() {
                        log(&format!("[{}] agent already running — ignoring trigger", agent_name));
                        continue;
                    }
                    if let Err(e) = spawn_agent(agent, &trigger_context).await {
                        log(&format!("[{}] failed to start: {}", agent_name, e));
                        continue;
                    }
                }
            }

            // Wait for agent to finish before watching again
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if !running.load(std::sync::atomic::Ordering::Relaxed) { break; }

                let mut agents_map = agents.lock().await;
                if let Some(agent) = agents_map.get_mut(&agent_name) {
                    if let Some(ref mut child) = agent.child {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                let elapsed = agent.last_start.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
                                agent.total_uptime_secs += elapsed;
                                let code = status.code().unwrap_or(-1);

                                if code == 0 {
                                    log(&format!("[{}] session #{} completed cleanly ({:.0}s)", agent_name, agent.session_count, elapsed));
                                    agent.consecutive_crashes = 0;
                                } else {
                                    agent.consecutive_crashes += 1;
                                    log(&format!("[{}] session #{} crashed (code: {})", agent_name, agent.session_count, code));
                                }

                                agent.child = None;
                                agent.pid = None;
                                break; // Back to watching
                            }
                            Ok(None) => {} // Still running
                            Err(e) => {
                                log(&format!("[{}] error checking child: {}", agent_name, e));
                            }
                        }
                    } else {
                        break; // No child = already exited
                    }
                } else {
                    break;
                }
            }

            // Small cooldown before re-watching
            let cooldown = {
                let agents_map = agents.lock().await;
                agents_map.get(&agent_name)
                    .map(|a| a.config.limits.cooldown_secs)
                    .unwrap_or(5)
            };
            if cooldown > 0 {
                tokio::time::sleep(Duration::from_secs(cooldown)).await;
            }
        }
    });
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
