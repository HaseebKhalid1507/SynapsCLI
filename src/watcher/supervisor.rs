use super::*;

/// Route a session-completion notice according to the agent's `[hooks]`.
///
/// * `notify_session = "<id|name>"` → delivered to that one session's socket;
///   if it is not running the notice is dropped (logged), never broadcast.
/// * `notify_inbox = true` → legacy shared-inbox file drop.
/// * neither → nothing (default). Every call site goes through here so the
///   hooks are honoured uniformly.
pub(crate) async fn notify_completion(
    hooks: &synaps_cli::HooksConfig,
    agent_name: &str,
    session_count: u64,
    elapsed_secs: f64,
    exit_code: i32,
) {
    if let Some(ref target) = hooks.notify_session {
        let event = completion_event(agent_name, session_count, elapsed_secs, exit_code);
        let send = crate::cmd::send::send_event_to_session(target, &event);
        match tokio::time::timeout(Duration::from_secs(3), send).await {
            Ok(Ok(sid)) => log(&format!("[{}] completion sent to session {}", agent_name, sid)),
            Ok(Err(e)) => log(&format!("[{}] completion notice dropped: {}", agent_name, e)),
            Err(_) => log(&format!("[{}] completion notice dropped: socket timeout", agent_name)),
        }
    } else if hooks.notify_inbox {
        notify_inbox_completion(agent_name, session_count, elapsed_secs, exit_code);
    }
}

fn completion_event(
    agent_name: &str,
    session_count: u64,
    elapsed_secs: f64,
    exit_code: i32,
) -> synaps_cli::events::types::Event {
    use synaps_cli::events::types::{Event, EventContent, EventSource, Severity};

    Event {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now(),
        source: EventSource {
            source_type: "watcher".to_string(),
            name: agent_name.to_string(),
            callback: None,
        },
        channel: None,
        sender: None,
        content: EventContent {
            text: format!(
                "Agent '{}' completed session #{} ({:.0}s, exit {})",
                agent_name, session_count, elapsed_secs, exit_code
            ),
            content_type: "agent_complete".to_string(),
            severity: if exit_code == 0 {
                Some(Severity::Medium)
            } else {
                Some(Severity::High)
            },
            data: Some(serde_json::json!({
                "agent": agent_name,
                "session": session_count,
                "elapsed_secs": elapsed_secs,
                "exit_code": exit_code,
            })),
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
    }
}

pub(crate) fn notify_inbox_completion(
    agent_name: &str,
    session_count: u64,
    elapsed_secs: f64,
    exit_code: i32,
) {
    let event = completion_event(agent_name, session_count, elapsed_secs, exit_code);
    let inbox_dir = synaps_cli::config::base_dir().join("inbox");
    let _ = std::fs::create_dir_all(&inbox_dir);
    let filename = format!(
        "watcher-{}-{}.json",
        agent_name,
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let path = inbox_dir.join(&filename);
    if let Ok(body) = serde_json::to_string_pretty(&event) {
        let _ = std::fs::write(&path, body);
        log(&format!(
            "[{}] completion event dropped to inbox",
            agent_name
        ));
    }
}

// ── Lightweight long-horizon support ─────────────────────────────────────────

/// Re-read the agent's config.toml so edits to limits / hooks / wake_if take
/// effect at the next spawn without restarting the supervisor. The trigger
/// *mode* (always/watch/manual) still needs a redeploy to rewire tasks.
pub(crate) fn refresh_config(agent: &mut ManagedAgent) {
    match AgentConfig::load(&agent.config_path) {
        Ok(cfg) => agent.config = cfg,
        Err(e) => log(&format!(
            "[{}] config reload failed, keeping previous config: {}",
            agent.name, e
        )),
    }
}

/// Run the agent's `wake_if` gate. No gate configured => always wake.
pub(crate) async fn wake_gate_passes(
    agent_name: &str,
    wake_if: Option<&str>,
    agent_dir: &Path,
) -> bool {
    let Some(cmd) = wake_if else { return true };
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(30), status).await {
        Ok(Ok(st)) => st.success(),
        Ok(Err(e)) => {
            log(&format!("[{}] wake_if failed to run: {}", agent_name, e));
            false
        }
        Err(_) => {
            log(&format!("[{}] wake_if timed out (30s) — treated as not met", agent_name));
            false
        }
    }
}

/// Block (cheaply — a shell check every `wake_poll_secs`, no LLM) until the
/// agent's `wake_if` gate passes. Returns false if the supervisor is shutting
/// down or the agent was stopped/removed meanwhile.
pub(crate) async fn wait_for_wake(
    agent_name: &str,
    agents: &Arc<Mutex<HashMap<String, ManagedAgent>>>,
    running: &Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    let mut sleeping = false;
    loop {
        if !running.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        let (wake_if, poll, dir) = {
            let mut map = agents.lock().await;
            let Some(agent) = map.get_mut(agent_name) else { return false };
            if agent.stopped {
                return false;
            }
            refresh_config(agent);
            (
                agent.config.trigger.wake_if.clone(),
                agent.config.trigger.wake_poll_secs.max(5),
                AgentConfig::agent_dir(&agent.config_path),
            )
        };
        if wake_gate_passes(agent_name, wake_if.as_deref(), &dir).await {
            if sleeping {
                log(&format!("[{}] wake_if met — waking", agent_name));
            }
            return true;
        }
        if !sleeping {
            log(&format!(
                "[{}] wake_if not met — sleeping, re-check every {}s (no LLM session)",
                agent_name, poll
            ));
            sleeping = true;
        }
        tokio::time::sleep(Duration::from_secs(poll)).await;
    }
}

/// Wait `delay`, then for the wake gate, then spawn — all outside the agents
/// lock except for the final spawn.
pub(crate) fn schedule_spawn(
    agent_name: String,
    agents: Arc<Mutex<HashMap<String, ManagedAgent>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    delay: Duration,
    context: &'static str,
) {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if !wait_for_wake(&agent_name, &agents, &running).await {
            return;
        }
        let mut map = agents.lock().await;
        if let Some(agent) = map.get_mut(&agent_name) {
            if agent.stopped || agent.is_running() {
                return;
            }
            if let Err(e) = spawn_agent(agent, context).await {
                log(&format!("[{}] failed to start: {}", agent_name, e));
            }
        }
    });
}

fn systemd_run_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn valid_memory_max(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 16
        && (v == "infinity"
            || v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'K' | 'M' | 'G' | 'T' | '%')))
}

/// Build the command that launches one agent session, wrapping it in a
/// systemd user scope (MemoryMax / CPUQuota) and `nice` when configured.
/// systemd-run --scope execs in place, so the child PID is the agent itself
/// and stop/kill semantics are unchanged.
pub(crate) fn build_agent_command(
    agent_name: &str,
    bin: &Path,
    limits: &synaps_cli::SessionLimits,
) -> tokio::process::Command {
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    let mem = limits.memory_max.as_deref().filter(|m| {
        let ok = valid_memory_max(m);
        if !ok {
            log(&format!("[{}] ignoring invalid memory_max {:?}", agent_name, m));
        }
        ok
    });
    let cpu = limits.cpu_quota_pct.filter(|c| *c > 0);
    if mem.is_some() || cpu.is_some() {
        if systemd_run_available() {
            argv.extend(["systemd-run", "--user", "--scope", "--quiet", "--collect"].map(Into::into));
            if let Some(m) = mem {
                argv.push("-p".into());
                argv.push(format!("MemoryMax={}", m).into());
            }
            if let Some(c) = cpu {
                argv.push("-p".into());
                argv.push(format!("CPUQuota={}%", c).into());
            }
            argv.push("--".into());
        } else {
            log(&format!(
                "[{}] memory_max/cpu_quota_pct set but `systemd-run --user --scope` unavailable — running unconfined",
                agent_name
            ));
        }
    }
    if let Some(n) = limits.nice {
        argv.extend(["nice".into(), "-n".into(), n.clamp(0, 19).to_string().into()]);
    }
    argv.push(bin.as_os_str().to_owned());
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Spawn an agent worker process
pub(crate) async fn spawn_agent(
    agent: &mut ManagedAgent,
    trigger_context: &str,
) -> Result<(), String> {
    let bin = agent_binary();
    if !bin.exists() {
        return Err(format!(
            "synaps-agent binary not found at {}",
            bin.display()
        ));
    }

    refresh_config(agent);

    log(&format!(
        "[{}] spawning session #{}",
        agent.name,
        agent.session_count + 1
    ));

    let child = build_agent_command(&agent.name, &bin, &agent.config.limits)
        .arg("agent")
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
pub(crate) fn check_heartbeat(agent_dir: &std::path::Path, stale_threshold: u64) -> bool {
    let hb_path = agent_dir.join("heartbeat");
    if let Ok(content) = std::fs::read_to_string(&hb_path) {
        if let Ok(ts) = content.trim().parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            return now.saturating_sub(ts) < stale_threshold as i64;
        }
    }
    false
}

/// Expand ~ in a path string to the home directory
pub(crate) fn expand_watch_path(p: &str) -> PathBuf {
    if p.starts_with("~/") {
        if let Some(home) = dirs_next() {
            return home.join(p.trim_start_matches("~/"));
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
        let watch_paths: Vec<PathBuf> = trigger_config
            .paths
            .iter()
            .map(|p| expand_watch_path(p))
            .collect();

        if watch_paths.is_empty() {
            log(&format!(
                "[{}] watch trigger has no paths configured — skipping",
                agent_name
            ));
            return;
        }

        // Validate that paths exist
        for p in &watch_paths {
            if !p.exists() {
                log(&format!(
                    "[{}] creating watched directory: {}",
                    agent_name,
                    p.display()
                ));
                let _ = std::fs::create_dir_all(p);
            }
        }

        log(&format!(
            "[{}] watching {} path(s): {}",
            agent_name,
            watch_paths.len(),
            watch_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        let patterns = trigger_config.patterns.clone();
        let debounce_secs = trigger_config.debounce_secs;

        // Main watch loop — restarts the watcher after each agent session
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            // Set up notify watcher with a crossbeam channel
            let (tx, rx) = std::sync::mpsc::channel();
            let mut notify_watcher: notify::RecommendedWatcher =
                match notify::RecommendedWatcher::new(tx, notify::Config::default()) {
                    Ok(w) => w,
                    Err(e) => {
                        log(&format!(
                            "[{}] failed to create file watcher: {}",
                            agent_name, e
                        ));
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                };

            // Watch all configured paths
            for path in &watch_paths {
                if let Err(e) = notify_watcher.watch(path, notify::RecursiveMode::Recursive) {
                    log(&format!(
                        "[{}] failed to watch {}: {}",
                        agent_name,
                        path.display(),
                        e
                    ));
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
            })
            .await
            .unwrap_or_default();

            // Drop the watcher to release inotify watches while agent runs
            drop(notify_watcher);

            if changed_paths.is_empty() || !running.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            // Build trigger context with changed file paths
            let paths_str: Vec<String> = changed_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let trigger_context = format!("files changed:\n{}", paths_str.join("\n"));

            log(&format!(
                "[{}] triggered by {} file(s)",
                agent_name,
                paths_str.len()
            ));

            // Honour the wake_if gate before paying for an LLM session
            if !wait_for_wake(&agent_name, &agents, &running).await {
                continue;
            }

            // Spawn the agent
            {
                let mut agents_map = agents.lock().await;
                if let Some(agent) = agents_map.get_mut(&agent_name) {
                    if agent.stopped {
                        log(&format!(
                            "[{}] agent is stopped — ignoring trigger",
                            agent_name
                        ));
                        continue;
                    }
                    if agent.is_running() {
                        log(&format!(
                            "[{}] agent already running — ignoring trigger",
                            agent_name
                        ));
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
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let mut agents_map = agents.lock().await;
                if let Some(agent) = agents_map.get_mut(&agent_name) {
                    if let Some(ref mut child) = agent.child {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                let elapsed = agent
                                    .last_start
                                    .map(|s| s.elapsed().as_secs_f64())
                                    .unwrap_or(0.0);
                                agent.total_uptime_secs += elapsed;
                                let code = status.code().unwrap_or(-1);

                                if code == 0 {
                                    log(&format!(
                                        "[{}] session #{} completed cleanly ({:.0}s)",
                                        agent_name, agent.session_count, elapsed
                                    ));
                                    agent.consecutive_crashes = 0;
                                    notify_completion(
                                        &agent.config.hooks,
                                        &agent_name,
                                        agent.session_count,
                                        elapsed,
                                        code,
                                    )
                                    .await;
                                } else {
                                    agent.consecutive_crashes += 1;
                                    log(&format!(
                                        "[{}] session #{} crashed (code: {})",
                                        agent_name, agent.session_count, code
                                    ));
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
                agents_map
                    .get(&agent_name)
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

    // Load global SynapsConfig once for bridge mirror settings; failures here
    // are non-fatal — we just default the BridgeConfig.
    let synaps_cfg = synaps_cli::load_config();
    let bridge_cfg = std::sync::Arc::new(synaps_cfg.bridge.clone());
    if bridge_cfg.heartbeat_mirror {
        log(&format!(
            "bridge heartbeat mirror ENABLED (uds={}, timeout={}ms)",
            bridge_cfg.resolved_uds_path().display(),
            bridge_cfg.heartbeat_timeout_ms
        ));
    }

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
                    log(&format!(
                        "loaded agent: {} (trigger: {})",
                        name, config.agent.trigger
                    ));
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
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        r.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    // Start always-on agents (each waits on its wake_if gate, if any)
    {
        let always: Vec<String> = {
            let agents_map = agents.lock().await;
            agents_map
                .iter()
                .filter(|(_, a)| a.config.agent.trigger == "always")
                .map(|(n, _)| n.clone())
                .collect()
        };
        for name in always {
            schedule_spawn(
                name,
                agents.clone(),
                running.clone(),
                Duration::ZERO,
                "supervisor start (always-on)",
            );
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
                if agent.stopped {
                    continue;
                }

                if let Some(ref mut child) = agent.child {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let elapsed = agent
                                .last_start
                                .map(|s| s.elapsed().as_secs_f64())
                                .unwrap_or(0.0);
                            agent.total_uptime_secs += elapsed;
                            let code = status.code().unwrap_or(-1);

                            if code == 0 {
                                log(&format!(
                                    "[{}] session #{} completed cleanly ({:.0}s)",
                                    name, agent.session_count, elapsed
                                ));
                                agent.consecutive_crashes = 0;
                                notify_completion(
                                    &agent.config.hooks,
                                    name,
                                    agent.session_count,
                                    elapsed,
                                    code,
                                )
                                .await;
                            } else if code == 2 {
                                log(&format!(
                                    "[{}] daily cost limit reached — pausing until midnight",
                                    name
                                ));
                                agent.stopped = true;
                            } else {
                                agent.consecutive_crashes += 1;
                                log(&format!(
                                    "[{}] session #{} crashed (code: {}, consecutive: {})",
                                    name, agent.session_count, code, agent.consecutive_crashes
                                ));
                            }

                            agent.child = None;
                            agent.pid = None;

                            if agent.config.agent.trigger == "always" {
                                if agent.consecutive_crashes >= agent.config.limits.max_retries {
                                    log(&format!(
                                        "[{}] max retries ({}) exceeded — stopping",
                                        name, agent.config.limits.max_retries
                                    ));
                                    agent.stopped = true;
                                } else {
                                    let backoff = if agent.consecutive_crashes > 0 {
                                        let base = agent.config.limits.cooldown_secs;
                                        let factor =
                                            2u64.pow(agent.consecutive_crashes.saturating_sub(1));
                                        (base * factor).min(300)
                                    } else {
                                        agent.config.limits.cooldown_secs
                                    };
                                    log(&format!("[{}] restarting in {}s", name, backoff));
                                    let ctx: &'static str = if code == 0 {
                                        "automatic restart (always-on)"
                                    } else {
                                        "crash recovery restart"
                                    };
                                    schedule_spawn(
                                        name.clone(),
                                        agents.clone(),
                                        running.clone(),
                                        Duration::from_secs(backoff),
                                        ctx,
                                    );
                                }
                            }
                        }
                        Ok(None) => {
                            let agent_dir = AgentConfig::agent_dir(&agent.config_path);
                            let stale_threshold = agent.config.heartbeat.stale_threshold_secs;
                            let fresh = check_heartbeat(&agent_dir, stale_threshold);

                            // Best-effort: mirror heartbeat to bridge UDS. Never blocks
                            // the supervisor loop or fails the agent.
                            if bridge_cfg.heartbeat_mirror {
                                let bridge_cfg_for_task = bridge_cfg.clone();
                                let agent_name = name.clone();
                                let session_count = agent.session_count;
                                let pid = agent.pid;
                                tokio::spawn(async move {
                                    let details = serde_json::json!({
                                        "session_count": session_count,
                                        "pid": pid,
                                    });
                                    if let Err(e) = bridge_client::mirror_heartbeat(
                                        &bridge_cfg_for_task,
                                        &agent_name,
                                        fresh,
                                        details,
                                    )
                                    .await
                                    {
                                        tracing::debug!(
                                            target: "watcher::bridge",
                                            agent = %agent_name,
                                            error = %e,
                                            "bridge heartbeat mirror failed (non-fatal)"
                                        );
                                    }
                                });
                            }

                            if agent.last_start.map(|s| s.elapsed().as_secs()).unwrap_or(0) > 60
                                && !fresh
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
        return Err(format!(
            "Agent '{}' already exists at {}",
            name,
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory: {}", e))?;

    let config = format!(
        r#"[agent]
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
"#
    );

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

#[cfg(test)]
mod light_tests {
    use super::*;

    fn argv(cmd: &tokio::process::Command) -> Vec<String> {
        let c = cmd.as_std();
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn memory_max_validation() {
        for ok in ["1G", "1.5G", "512M", "80%", "infinity"] {
            assert!(valid_memory_max(ok), "{ok}");
        }
        for bad in ["", "1G; rm", "-p", "1 G", "12345678901234567"] {
            assert!(!valid_memory_max(bad), "{bad}");
        }
    }

    #[test]
    fn command_unconfined_by_default() {
        let limits = synaps_cli::SessionLimits::default();
        let cmd = build_agent_command("t", Path::new("/bin/synaps"), &limits);
        assert_eq!(argv(&cmd), vec!["/bin/synaps"]);
    }

    #[test]
    fn command_nice_is_clamped() {
        let mut limits = synaps_cli::SessionLimits::default();
        limits.nice = Some(99);
        let cmd = build_agent_command("t", Path::new("/bin/synaps"), &limits);
        assert_eq!(argv(&cmd), vec!["nice", "-n", "19", "/bin/synaps"]);
    }

    #[test]
    fn command_scope_when_available() {
        let mut limits = synaps_cli::SessionLimits::default();
        limits.memory_max = Some("1.5G".into());
        limits.cpu_quota_pct = Some(200);
        let a = argv(&build_agent_command("t", Path::new("/bin/synaps"), &limits));
        assert_eq!(a.last().unwrap(), "/bin/synaps");
        if systemd_run_available() {
            assert_eq!(a[0], "systemd-run");
            assert!(a.contains(&"MemoryMax=1.5G".to_string()));
            assert!(a.contains(&"CPUQuota=200%".to_string()));
        } else {
            assert_eq!(a, vec!["/bin/synaps"]);
        }
    }

    #[tokio::test]
    async fn wake_gate_semantics() {
        let dir = std::env::temp_dir();
        assert!(wake_gate_passes("t", None, &dir).await);
        assert!(wake_gate_passes("t", Some("true"), &dir).await);
        assert!(!wake_gate_passes("t", Some("false"), &dir).await);
        assert!(!wake_gate_passes("t", Some("exit 3"), &dir).await);
    }
}
