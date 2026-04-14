use super::*;

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