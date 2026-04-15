use super::*;
use synaps_cli::AgentHarness;

/// Spawn an agent worker process
pub(crate) async fn spawn_agent(agent: &mut ManagedAgent, trigger_context: &str) -> Result<(), String> {
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

/// Spawn an agent as an in-process tokio task (task isolation mode).
pub(crate) async fn spawn_agent_inprocess(
    agent: &mut ManagedAgent,
    trigger_context: &str,
) -> Result<(), String> {
    let config_path = agent.config_path.to_string_lossy().to_string();
    let trigger = trigger_context.to_string();
    let name = agent.name.clone();

    log(&format!("[{}] spawning in-process session #{}", name, agent.session_count + 1));

    let mut harness = AgentHarness::from_config(&config_path, Some(&trigger)).await?;
    harness.init().await?;

    let bus_handle = harness.bus_handle()
        .ok_or_else(|| "harness init() did not produce a bus handle".to_string())?;

    let task_handle = tokio::spawn(async move {
        if let Err(e) = harness.run().await {
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            eprintln!("[{}] [{}] session error: {}", ts, name, e);
        }
    });

    agent.bus_handle = Some(bus_handle);
    agent.task_handle = Some(task_handle);
    agent.session_count += 1;
    agent.last_start = Some(Instant::now());
    agent.pid = None; // no PID in task mode

    log(&format!("[{}] started (in-process task)", agent.name));
    Ok(())
}

/// Spawn an agent using the configured isolation mode (process or task).
pub(crate) async fn spawn_agent_auto(agent: &mut ManagedAgent, trigger_context: &str) -> Result<(), String> {
    if agent.config.agent.isolation == "task" {
        spawn_agent_inprocess(agent, trigger_context).await
    } else {
        spawn_agent(agent, trigger_context).await
    }
}

/// Check heartbeat freshness
pub(crate) fn check_heartbeat(agent_dir: &std::path::Path, stale_threshold: u64) -> bool {
    let hb_path = agent_dir.join("heartbeat");
    if let Ok(content) = std::fs::read_to_string(&hb_path) {
        if let Ok(ts) = content.trim().parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            return (now - ts).unsigned_abs() < stale_threshold;
        }
    }
    false
}

/// Expand ~ in a path string to the home directory
pub(crate) fn expand_watch_path(p: &str) -> PathBuf {
    if p.starts_with("~/") {
        if let Some(home) = dirs_next() {
            return home.join(p.strip_prefix("~/").unwrap());
        }
    }
    PathBuf::from(p)
}

pub(crate) fn dirs_next() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Check if a path matches the configured glob patterns (empty patterns = match all)
pub(crate) fn matches_patterns(path: &Path, patterns: &[String]) -> bool {
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
pub(crate) fn spawn_watch_task(
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
                    if let Err(e) = spawn_agent_auto(agent, &trigger_context).await {
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
/// Run the supervisor daemon — manages all agent lifecycles.
pub(crate) async fn run_supervisor() {
    // Check if supervisor already running
    let pid_path = watcher_dir().join("watcher.pid");
    if pid_path.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                let proc_path = format!("/proc/{}", pid);
                if std::path::Path::new(&proc_path).exists() {
                    eprintln!("Error: Supervisor already running (PID {})", pid);
                    std::process::exit(1);
                }
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    log("starting supervisor");

    let socket_path = watcher_dir().join("watcher.sock");
    let pid_path = watcher_dir().join("watcher.pid");

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
                if let Err(e) = spawn_agent_auto(agent, "supervisor start (always-on)").await {
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
                                agent.stopped = true;
                            } else {
                                agent.consecutive_crashes += 1;
                                log(&format!("[{}] session #{} crashed (code: {}, consecutive: {})",
                                    name, agent.session_count, code, agent.consecutive_crashes));
                            }

                            agent.child = None;
                            agent.pid = None;

                            if agent.config.agent.trigger == "always" {
                                if agent.consecutive_crashes >= agent.config.limits.max_retries {
                                    log(&format!("[{}] max retries ({}) exceeded — stopping", name, agent.config.limits.max_retries));
                                    agent.stopped = true;
                                } else {
                                    let backoff = if agent.consecutive_crashes > 0 {
                                        let base = agent.config.limits.cooldown_secs;
                                        let factor = 2u64.pow(agent.consecutive_crashes.saturating_sub(1));
                                        (base * factor).min(300)
                                    } else {
                                        agent.config.limits.cooldown_secs
                                    };
                                    log(&format!("[{}] restarting in {}s", name, backoff));

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
                                                if let Err(e) = spawn_agent_auto(agent, ctx).await {
                                                    log(&format!("[{}] failed to restart: {}", agent_name, e));
                                                }
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        Ok(None) => {
                            let agent_dir = AgentConfig::agent_dir(&agent.config_path);
                            if agent.last_start.map(|s| s.elapsed().as_secs()).unwrap_or(0) > 60
                                && !check_heartbeat(&agent_dir, agent.config.heartbeat.stale_threshold_secs)
                            {
                                log(&format!("[{}] heartbeat stale — killing", name));
                                let _ = child.kill().await;
                                let _ = child.wait().await;
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

    // Graceful shutdown
    log("shutting down — stopping all agents");
    {
        let mut agents_map = agents.lock().await;
        for (name, agent) in agents_map.iter_mut() {
            if let Some(ref mut child) = agent.child {
                log(&format!("[{}] sending SIGTERM", name));
                let _ = child.kill().await;
                let _ = child.wait().await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);

    log("supervisor stopped");
}

/// Create agent from template
pub(crate) fn init_agent(name: &str) -> Result<(), String> {
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