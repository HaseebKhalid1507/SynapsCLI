//! synaps-agent — Headless autonomous agent worker
//!
//! Boots with a system prompt + handoff state, runs the agentic loop
//! until limits are hit, writes handoff, and exits cleanly.
//!
//! Usage: synaps-agent --config <path/to/config.toml>

use fs4::fs_std::FileExt;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use synaps_cli::engine::reactor::{drain_event_queue, idle_should_wait};
use synaps_cli::watcher_session::{
    push_user_prompt, run_handoff_turn, run_session_turn, SessionAllowance, SessionLimit,
    SessionTotals,
};
use synaps_cli::{
    watcher_types::{AgentStats, DailyStats},
    AgentConfig, HandoffState, LlmEvent, Runtime, SessionEvent, StreamEvent,
};

/// Write data to a file atomically (write to .tmp, then rename)
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, data)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Update stats with file locking to prevent race conditions
fn update_stats(agent_dir: &Path, updater: impl FnOnce(&mut AgentStats)) {
    use std::io::{Read, Seek, Write};

    let stats_path = agent_dir.join("stats.json");

    // Open with exclusive lock
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // We'll manually truncate after reading
        .open(&stats_path);

    let Ok(mut file) = file else {
        return;
    };

    // Get exclusive lock - blocking (fs4 crate, not std)
    #[allow(clippy::incompatible_msrv)]
    if file.lock_exclusive().is_err() {
        return; // Failed to lock, skip update
    }

    // Read current stats
    let mut contents = String::new();
    let _ = file.read_to_string(&mut contents);
    let mut stats: AgentStats = serde_json::from_str(&contents).unwrap_or_default();

    // Apply update
    updater(&mut stats);

    // Write back (truncate + write)
    let _ = file.set_len(0);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = serde_json::to_writer_pretty(&mut file, &stats);
    let _ = file.flush();

    // Lock released when file is dropped
}

fn log(agent: &str, msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [{}] {}", ts, agent, msg);
}

fn write_log(log_path: &Path, entry: &serde_json::Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = serde_json::to_writer(&mut f, entry);
        let _ = writeln!(f);
    }
}

fn get_session_number(logs_dir: &Path) -> u64 {
    let mut max_session = 0;
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("session-") && name_str.ends_with(".jsonl") {
                if let Ok(num) = name_str
                    .trim_start_matches("session-")
                    .trim_end_matches(".jsonl")
                    .parse::<u64>()
                {
                    max_session = max_session.max(num);
                }
            }
        }
    }
    max_session + 1
}

fn load_stats(agent_dir: &Path) -> AgentStats {
    use std::io::Read;

    let path = agent_dir.join("stats.json");
    let file = std::fs::OpenOptions::new().read(true).open(&path);

    let Ok(mut file) = file else {
        return AgentStats::default();
    };

    // Try to get shared lock for reading (fs4 crate, not std)
    #[allow(clippy::incompatible_msrv)]
    if file.lock_shared().is_err() {
        // If we can't lock, just return default
        return AgentStats::default();
    }

    let mut contents = String::new();
    let _ = file.read_to_string(&mut contents);
    serde_json::from_str(&contents).unwrap_or_default()
}

pub async fn run(config_path: String, trigger_context: String) {
    let config_path = PathBuf::from(config_path);

    // Load config
    let config = AgentConfig::load(&config_path).unwrap_or_else(|e| {
        eprintln!("Failed to load config: {}", e);
        std::process::exit(1);
    });

    let agent_dir = AgentConfig::agent_dir(&config_path);
    let agent_name = &config.agent.name;

    // Load stats and check daily cost limit
    let stats = load_stats(&agent_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if stats.today.date == today && stats.today.cost_usd >= config.limits.max_daily_cost_usd {
        log(
            agent_name,
            &format!(
                "daily cost limit reached (${:.2}/${:.2}) — exiting",
                stats.today.cost_usd, config.limits.max_daily_cost_usd
            ),
        );
        std::process::exit(2);
    }

    // Setup session logging
    let logs_dir = agent_dir.join("logs");
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("failed to create logs dir {}: {}", logs_dir.display(), e);
    }
    let session_number = get_session_number(&logs_dir);
    let session_log_path = logs_dir.join(format!("session-{:03}.jsonl", session_number));
    let current_log_path = logs_dir.join("current.log");

    // Write current.log file atomically
    let _ = atomic_write(
        &current_log_path,
        session_log_path.to_string_lossy().as_bytes(),
    );

    log(
        agent_name,
        &format!(
            "booting (model: {}, trigger: {})",
            config.agent.model, config.agent.trigger
        ),
    );

    // Log boot event
    write_log(
        &session_log_path,
        &json!({
            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "type": "boot",
            "session": session_number,
            "model": config.agent.model,
            "trigger": trigger_context
        }),
    );

    // Load soul (system prompt)
    let soul = AgentConfig::load_soul(&agent_dir).unwrap_or_else(|e| {
        log(agent_name, &format!("FATAL: {}", e));
        std::process::exit(1);
    });

    // Load handoff state from previous session
    let handoff = AgentConfig::load_handoff(&agent_dir);
    let handoff_json = serde_json::to_string_pretty(&handoff).unwrap_or_default();

    // Validate handoff size to prevent context bloat
    if handoff_json.len() > 50 * 1024 {
        log(
            agent_name,
            &format!(
                "WARNING: handoff state large ({}KB), trimming",
                handoff_json.len() / 1024
            ),
        );
        // Note: using empty handoff to prevent context bloat would require regenerating boot message
        // For now just warn - the agent can decide if this is too much context
    }

    // Build boot message from template
    let timestamp = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string();
    let boot_message = config
        .boot
        .message
        .replace("{timestamp}", &timestamp)
        .replace("{handoff}", &handoff_json)
        .replace("{trigger_context}", &trigger_context);

    // Initialize runtime
    let mut runtime = Runtime::new().await.unwrap_or_else(|e| {
        log(
            agent_name,
            &format!("FATAL: failed to create runtime: {}", e),
        );
        std::process::exit(1);
    });
    let synaps_config = synaps_cli::config::load_config();
    for warning in &synaps_config.warnings {
        log(agent_name, &format!("config warning: {warning}"));
    }
    // Bind notes independently of worker settings and memory-context consent.
    runtime.apply_memory_backend_config(&synaps_config.memory_backend);
    // Task 23: headless autonomous workers run under the AUTONOMOUS turn
    // budget (typed config overrides applied) — the tightest role, finite by
    // default (24 rounds / 96 tool calls / 8 MiB per turn). Each turn narrows
    // it further to what the session's limits have left.
    runtime.set_turn_budget(synaps_cli::runtime::budget::TurnBudget::from_config(
        synaps_cli::runtime::budget::TurnRole::Autonomous,
        &synaps_config.turn_budgets,
    ));
    runtime.set_model(config.agent.model.clone());
    let foreground = agent_engine::orchestration::canonical_foreground_identity(runtime.model())
        .unwrap_or_else(|e| {
            log(
                agent_name,
                &format!("FATAL: invalid foreground model: {}", e),
            );
            std::process::exit(1);
        });
    let orchestration =
        agent_engine::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
            .unwrap_or_else(|_| {
                log(agent_name, "FATAL: foreground model is not routable");
                std::process::exit(1);
            });
    runtime.install_orchestration(Arc::new(orchestration));
    runtime.set_system_prompt(soul);

    // Handoff path for watcher_exit tool
    let handoff_path = agent_dir.join("handoff.json");
    runtime.watcher_exit_path = Some(handoff_path.clone());

    // Register watcher_exit tool
    {
        let tools = runtime.tools_shared();
        let mut tools = tools.write().await;
        tools.register(Arc::new(synaps_cli::tools::WatcherExitTool));
    }

    // Setup heartbeat
    let heartbeat_path = agent_dir.join("heartbeat");
    let heartbeat_interval = config.heartbeat.interval_secs;
    let hb_path = heartbeat_path.clone();
    let hb_running = Arc::new(AtomicBool::new(true));
    let hb_flag = hb_running.clone();
    let heartbeat_task = tokio::spawn(async move {
        while hb_flag.load(Ordering::Acquire) {
            let ts = chrono::Utc::now().timestamp().to_string();
            // Atomic heartbeat write
            let tmp = hb_path.with_extension("tmp");
            let _ = tokio::fs::write(&tmp, &ts).await;
            let _ = tokio::fs::rename(&tmp, &hb_path).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(heartbeat_interval)).await;
        }
    });

    // Per-session event socket + registry (so `synaps send` can reach this agent)
    let agent_session_id = format!(
        "{}-{}-{}",
        agent_name,
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    let endpoint = match synaps_cli::events::registry::SessionEndpoint::start(
        &agent_session_id,
        Some(agent_name.clone()),
        runtime.event_queue().clone(),
    ) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            hb_running.store(false, Ordering::Release);
            heartbeat_task.abort();
            log(
                agent_name,
                &format!("failed to start session endpoint: {e}"),
            );
            return;
        }
    };
    // Also start inbox watcher for file-drop fallback
    let inbox_shutdown = Arc::new(AtomicBool::new(false));
    let inbox_task = {
        let inbox_dir = synaps_cli::config::base_dir().join("inbox");
        let eq = runtime.event_queue().clone();
        let sd = inbox_shutdown.clone();
        tokio::spawn(async move {
            synaps_cli::events::watch_inbox_filtered(
                inbox_dir,
                eq,
                sd,
                synaps_cli::events::not_watcher_notice,
            )
            .await;
        })
    };
    // Setup signal handling for graceful shutdown — catch both SIGINT (Ctrl-C)
    // and SIGTERM (supervisor/systemd stop) so a headless agent is not ignored
    // when its parent process sends SIGTERM.
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let int_flag = interrupted.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            int_flag.store(true, Ordering::Release);
        });
    }
    #[cfg(unix)]
    {
        let int_flag = interrupted.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut stream) = signal(SignalKind::terminate()) {
                stream.recv().await;
                int_flag.store(true, Ordering::Release);
            }
        });
    }

    // Session tracking. The session limits are enforced by the engine
    // inside each turn: every turn runs under the role budget narrowed to
    // what the session has left (tool calls, cost against the session and
    // daily caps, time, tokens).
    let role_budget = runtime.turn_budget().clone();
    let daily_spent_before = if stats.today.date == today {
        stats.today.cost_usd
    } else {
        0.0
    };
    let allowance = SessionAllowance {
        limits: &config.limits,
        daily_spent_before,
    };
    let mut totals = SessionTotals::default();
    let mut tool_call_num: u64 = 0;
    let session_start = Instant::now();
    let mut watcher_exit_called = false;
    let mut handoff_saved = false;
    let mut limit_hit: Option<SessionLimit> = None;
    let mut messages: Vec<synaps_cli::SharedMessage> = vec![std::sync::Arc::new(
        json!({"role": "user", "content": boot_message}),
    )];
    // Pending formatted events buffered while a turn was streaming.
    // Invariant: exactly one waiter on event_queue.notified() exists in agent mode.
    let mut pending_events: Vec<String> = Vec::new();

    // Logs one turn's events (shared by the main turns and the handoff turn).
    let mut log_event = |event: &StreamEvent, totals: &SessionTotals| match event {
        StreamEvent::Llm(LlmEvent::Text(text)) if text.len() > 100 => {
            log(
                agent_name,
                &format!("output: {}...", synaps_cli::truncate_str(text, 100)),
            );
            write_log(
                &session_log_path,
                &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "text",
                    "length": text.len(),
                    "preview": text.chars().take(200).collect::<String>()
                }),
            );
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
            tool_call_num += 1;
            log(agent_name, &format!("tool: {}", tool_name));
            write_log(
                &session_log_path,
                &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "tool_start",
                    "name": tool_name,
                    "call_num": tool_call_num
                }),
            );
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
            let preview: String = result.chars().take(100).collect();
            log(agent_name, &format!("  result: {}", preview));
            write_log(
                &session_log_path,
                &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "tool_result",
                    "name": "unknown", // We don't have tool name here, but it's from the most recent tool
                    "preview": result.chars().take(200).collect::<String>()
                }),
            );
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            ..
        }) => {
            log(
                agent_name,
                &format!(
                    "  tokens: +{}/+{} (cache read {}, write {}; total: {}, cost: ${:.4})",
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    totals.tokens,
                    totals.cost_usd
                ),
            );
            write_log(
                &session_log_path,
                &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "usage",
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_read_input_tokens": cache_read_input_tokens,
                    "cache_creation_input_tokens": cache_creation_input_tokens,
                    "total_tokens": totals.tokens,
                    "cost": totals.cost_usd
                }),
            );
        }
        StreamEvent::Session(SessionEvent::Error(e)) => {
            // Typed spec §5.2 outcome — log the terminal category +
            // correlation ID alongside the message.
            log(
                agent_name,
                &format!("ERROR: {} [{}]", e.message, e.category_label()),
            );
        }
        _ => {} // Thinking, SubagentStart/Update/Done, etc.
    };

    log(agent_name, "session started — entering agentic loop");

    // Main agentic loop
    loop {
        // Check limits before each turn
        if let Some(limit) = allowance.limit_reached(&totals, session_start.elapsed()) {
            log(
                agent_name,
                &format!(
                    "{} reached ({} tokens, ${:.4}, {} tool calls, {}s)",
                    limit.exit_reason(),
                    totals.tokens,
                    totals.cost_usd,
                    totals.tool_calls,
                    session_start.elapsed().as_secs()
                ),
            );
            limit_hit = Some(limit);
            break;
        }
        if interrupted.load(Ordering::Acquire) {
            log(agent_name, "interrupted by signal");
            break;
        }

        // Drain the event queue (idle, no active stream, no steer channel).
        // Each popped event is injected as a role=user message and logged.
        {
            let drained = drain_event_queue(
                runtime.event_queue(),
                &mut messages,
                &mut pending_events,
                false, // idle
                None,  // no steer channel
            );
            for d in &drained {
                log(
                    agent_name,
                    &format!(
                        "event [{}]: {}",
                        d.event.content.content_type, d.event.content.text
                    ),
                );
            }
        }

        // Run one streaming turn under what the session has left; a turn
        // stuck in a long tool round is cancelled at the session deadline.
        let report = run_session_turn(
            &mut runtime,
            &mut messages,
            &allowance,
            &role_budget,
            &mut totals,
            session_start,
            &interrupted,
            &mut log_event,
        )
        .await;
        if report.handoff_saved {
            watcher_exit_called = true;
            handoff_saved = true;
        }

        // If watcher_exit was called, agent is done
        if watcher_exit_called {
            log(agent_name, "agent called watcher_exit — clean shutdown");
            break;
        }

        // If stream ended without tool calls (just text response), agent is idle
        // In a non-always mode this would trigger sleep, but for now just check if
        // the last message was from the assistant with no tool use
        if let Some(last) = messages.last() {
            if last["role"].as_str() == Some("assistant")
                && last["stop_reason"].as_str() == Some("end_turn")
            {
                // Agent stopped on its own without calling watcher_exit.
                // Determine whether to wait on reactive events or nag immediately.
                let (children_running, queue_len) = {
                    // Drop registry lock before any await. Poison-safe: a panicked
                    // subagent thread should not brick the agent idle loop.
                    let reg = match runtime.subagent_registry().lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let running = reg.list_active().len();
                    drop(reg);
                    (running > 0, runtime.event_queue().len())
                };

                if idle_should_wait(children_running, queue_len) {
                    // Park on the queue notifier (bounded 30s) then loop back to
                    // drain at the top.  Exactly one waiter exists in agent mode.
                    log(
                        agent_name,
                        &format!(
                            "idle — waiting for events (children={}, queue={})",
                            children_running as u8, queue_len
                        ),
                    );
                    let _ = tokio::time::timeout(
                        tokio::time::Duration::from_secs(30),
                        runtime.event_queue().notified(),
                    )
                    .await;
                    // Loop back — drain fires at the top.
                    continue;
                }

                // No children and no queued events — give the agent one more chance
                // to write handoff (original nag behaviour).
                if !watcher_exit_called {
                    log(
                        agent_name,
                        "agent ended turn without tool calls — prompting for handoff",
                    );
                    push_user_prompt(
                        &mut messages,
                        "You stopped without calling watcher_exit. If you're done, call watcher_exit now with your handoff state. If you have more work, continue.",
                    );
                    // Let it have one more turn
                    continue;
                }
            }
        }
    }

    // Exit phase — if we didn't get a clean watcher_exit, ask for handoff.
    // The handoff turn is metered like any other (it is real spend).
    if !watcher_exit_called && !interrupted.load(Ordering::Acquire) {
        log(agent_name, "requesting handoff before shutdown...");
        let report = run_handoff_turn(
            &mut runtime,
            &mut messages,
            &role_budget,
            &mut totals,
            &interrupted,
            &mut log_event,
        )
        .await;
        handoff_saved = report.handoff_saved;
        if !handoff_saved {
            log(agent_name, "handoff turn ended without a saved handoff");
        }
    }

    // Stop heartbeat
    hb_running.store(false, Ordering::Release);

    endpoint.shutdown();
    inbox_shutdown.store(true, Ordering::Release);
    let _ = inbox_task.await;

    // If we still don't have a handoff, write a minimal one
    if !handoff_saved {
        log(agent_name, "no handoff from agent — writing minimal state");
        let minimal = HandoffState {
            summary: format!(
                "Session ended without clean handoff. Ran for {:.0}s, {} tokens, ${:.4}",
                session_start.elapsed().as_secs_f64(),
                totals.tokens,
                totals.cost_usd
            ),
            pending: vec!["Review previous session — no clean handoff was written".to_string()],
            context: serde_json::Value::Null,
        };
        let json = serde_json::to_string_pretty(&minimal).unwrap_or_default();
        let _ = atomic_write(&handoff_path, json.as_bytes());
    }

    let elapsed = session_start.elapsed().as_secs_f64();

    // Log exit event
    let exit_reason = if watcher_exit_called {
        "watcher_exit"
    } else if interrupted.load(Ordering::Acquire) {
        "signal"
    } else if let Some(limit) = limit_hit {
        limit.exit_reason()
    } else {
        "unknown"
    };

    write_log(
        &session_log_path,
        &json!({
            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "type": "exit",
            "reason": exit_reason,
            "total_tokens": totals.tokens,
            "total_cost": totals.cost_usd,
            "tool_calls": totals.tool_calls,
            "handoff_saved": handoff_saved,
            "duration_secs": elapsed as u64
        }),
    );

    log(
        agent_name,
        &format!(
            "session complete — {:.0}s, {} tokens, {} tool calls, ${:.4}",
            elapsed, totals.tokens, totals.tool_calls, totals.cost_usd
        ),
    );

    // Update stats before exit using locked update
    update_stats(&agent_dir, |stats| {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Reset daily stats if it's a new day
        if stats.today.date != today {
            stats.today = DailyStats {
                date: today,
                sessions: 0,
                cost_usd: 0.0,
                tokens: 0,
            };
        }

        // Update stats
        stats.total_sessions += 1;
        stats.total_tokens += totals.tokens;
        stats.total_cost_usd += totals.cost_usd;
        stats.total_uptime_secs += session_start.elapsed().as_secs_f64();
        stats.today.sessions += 1;
        stats.today.cost_usd += totals.cost_usd;
        stats.today.tokens += totals.tokens;

        // A crash is an exit that was neither clean, a signal, nor a
        // configured session limit (limits are the designed way to stop).
        if !watcher_exit_called && !interrupted.load(Ordering::Acquire) && limit_hit.is_none() {
            stats.crashes += 1;
            stats.last_crash = Some(format!(
                "{}: {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                exit_reason
            ));
        }
    });

    // Bounded observability flush (Task 11) — MUST run before
    // `process::exit`, which would otherwise bypass every queued
    // telemetry/trace record. This worker builds its Runtime via
    // `Runtime::new()` and never calls `apply_config`, so the telemetry
    // level is Off and this returns `None` today (harmless no-op) — but
    // the epilogue stays correct the day agent config gains telemetry.
    // Bounded: a hung disk can never stall agent shutdown.
    let _ = runtime
        .shutdown_observability_async(
            synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
        )
        .await;

    std::process::exit(0);
}
