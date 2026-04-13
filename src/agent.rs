//! synaps-agent — Headless autonomous agent worker
//!
//! Boots with a system prompt + handoff state, runs the agentic loop
//! until limits are hit, writes handoff, and exits cleanly.
//!
//! Usage: synaps-agent --config <path/to/config.toml>

use synaps_cli::{Runtime, StreamEvent, AgentConfig, HandoffState};
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Calculate approximate cost for a model
fn estimate_cost(input_tokens: u64, output_tokens: u64, model: &str) -> f64 {
    let (input_rate, output_rate) = if model.contains("opus") {
        (15.0 / 1_000_000.0, 75.0 / 1_000_000.0)
    } else if model.contains("haiku") {
        (0.25 / 1_000_000.0, 1.25 / 1_000_000.0)
    } else {
        // sonnet
        (3.0 / 1_000_000.0, 15.0 / 1_000_000.0)
    };
    input_tokens as f64 * input_rate + output_tokens as f64 * output_rate
}

fn log(agent: &str, msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [{}] {}", ts, agent, msg);
}

fn write_log(log_path: &Path, entry: &serde_json::Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
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
                if let Ok(num) = name_str.trim_start_matches("session-").trim_end_matches(".jsonl").parse::<u64>() {
                    max_session = max_session.max(num);
                }
            }
        }
    }
    max_session + 1
}

#[tokio::main]
async fn main() {
    // Parse args
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("Usage: synaps-agent --config <path/to/config.toml>");
            std::process::exit(1);
        });

    // Optional trigger context passed by supervisor
    let trigger_context = args.iter()
        .position(|a| a == "--trigger-context")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "manual start".to_string());

    // Load config
    let config = AgentConfig::load(&config_path).unwrap_or_else(|e| {
        eprintln!("Failed to load config: {}", e);
        std::process::exit(1);
    });

    let agent_dir = AgentConfig::agent_dir(&config_path);
    let agent_name = &config.agent.name;

    // Setup session logging
    let logs_dir = agent_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap_or_default();
    let session_number = get_session_number(&logs_dir);
    let session_log_path = logs_dir.join(format!("session-{:03}.jsonl", session_number));
    let current_log_path = logs_dir.join("current.log");
    
    // Write current.log file
    let _ = std::fs::write(&current_log_path, session_log_path.to_string_lossy().as_bytes());

    log(agent_name, &format!("booting (model: {}, trigger: {})", config.agent.model, config.agent.trigger));

    // Log boot event
    write_log(&session_log_path, &json!({
        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "type": "boot",
        "session": session_number,
        "model": config.agent.model,
        "trigger": trigger_context
    }));

    // Load soul (system prompt)
    let soul = AgentConfig::load_soul(&agent_dir).unwrap_or_else(|e| {
        log(agent_name, &format!("FATAL: {}", e));
        std::process::exit(1);
    });

    // Load handoff state from previous session
    let handoff = AgentConfig::load_handoff(&agent_dir);
    let handoff_json = serde_json::to_string_pretty(&handoff).unwrap_or_default();

    // Build boot message from template
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string();
    let boot_message = config.boot.message
        .replace("{timestamp}", &timestamp)
        .replace("{handoff}", &handoff_json)
        .replace("{trigger_context}", &trigger_context);

    // Initialize runtime
    let mut runtime = Runtime::new().await.unwrap_or_else(|e| {
        log(agent_name, &format!("FATAL: failed to create runtime: {}", e));
        std::process::exit(1);
    });
    runtime.set_model(config.agent.model.clone());
    runtime.set_system_prompt(soul);

    // Handoff path for sentinel_exit tool
    let handoff_path = agent_dir.join("handoff.json");
    runtime.sentinel_exit_path = Some(handoff_path.clone());

    // Register sentinel_exit tool
    {
        let tools = runtime.tools_shared();
        let mut tools = tools.write().await;
        tools.register(Arc::new(synaps_cli::tools::SentinelExitTool));
    }

    // Setup heartbeat
    let heartbeat_path = agent_dir.join("heartbeat");
    let heartbeat_interval = config.heartbeat.interval_secs;
    let hb_path = heartbeat_path.clone();
    let hb_running = Arc::new(AtomicBool::new(true));
    let hb_flag = hb_running.clone();
    tokio::spawn(async move {
        while hb_flag.load(Ordering::Relaxed) {
            let ts = chrono::Utc::now().timestamp().to_string();
            let _ = tokio::fs::write(&hb_path, &ts).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(heartbeat_interval)).await;
        }
    });

    // Setup signal handling for graceful shutdown
    let interrupted = Arc::new(AtomicBool::new(false));
    let int_flag = interrupted.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        int_flag.store(true, Ordering::Relaxed);
    });

    // Session tracking
    let mut total_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;
    let mut total_tool_calls: u64 = 0;
    let mut tool_call_num: u64 = 0;
    let session_start = Instant::now();
    let max_duration = std::time::Duration::from_secs(config.limits.max_session_duration_mins * 60);
    let mut sentinel_exit_called = false;
    let mut messages: Vec<Value> = vec![json!({"role": "user", "content": boot_message})];

    log(agent_name, "session started — entering agentic loop");

    // Main agentic loop
    loop {
        // Check limits before each turn
        if total_tokens >= config.limits.max_session_tokens {
            log(agent_name, &format!("token limit reached ({}/{})", total_tokens, config.limits.max_session_tokens));
            break;
        }
        if session_start.elapsed() >= max_duration {
            log(agent_name, &format!("time limit reached ({}m)", config.limits.max_session_duration_mins));
            break;
        }
        if total_cost >= config.limits.max_session_cost_usd {
            log(agent_name, &format!("cost limit reached (${:.4}/${:.2})", total_cost, config.limits.max_session_cost_usd));
            break;
        }
        if total_tool_calls >= config.limits.max_tool_calls {
            log(agent_name, &format!("tool call limit reached ({}/{})", total_tool_calls, config.limits.max_tool_calls));
            break;
        }
        if interrupted.load(Ordering::Relaxed) {
            log(agent_name, "interrupted by signal");
            break;
        }

        // Run one streaming turn
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let int_check = interrupted.clone();

        // Monitor for interrupt during streaming
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                if int_check.load(Ordering::Relaxed) {
                    cancel_clone.cancel();
                    break;
                }
            }
        });

        let mut stream = runtime.run_stream_with_messages(
            messages.clone(),
            cancel,
            None, // no steering for autonomous agents
        ).await;

        let mut turn_done = false;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Text(text) => {
                    // Log significant text output
                    if text.len() > 100 {
                        log(agent_name, &format!("output: {}...", &text[..100]));
                        write_log(&session_log_path, &json!({
                            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                            "type": "text",
                            "length": text.len(),
                            "preview": text.chars().take(200).collect::<String>()
                        }));
                    }
                }
                StreamEvent::ToolUseStart(name) => {
                    total_tool_calls += 1;
                    tool_call_num += 1;
                    log(agent_name, &format!("tool: {}", name));

                    write_log(&session_log_path, &json!({
                        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                        "type": "tool_start",
                        "name": name,
                        "call_num": tool_call_num
                    }));

                    // Check if this is a sentinel_exit call
                    if name == "sentinel_exit" {
                        sentinel_exit_called = true;
                    }
                }
                StreamEvent::ToolResult { result, .. } => {
                    let preview: String = result.chars().take(100).collect();
                    log(agent_name, &format!("  result: {}", preview));
                    
                    write_log(&session_log_path, &json!({
                        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                        "type": "tool_result",
                        "name": "unknown", // We don't have tool name here, but it's from the most recent tool
                        "preview": result.chars().take(200).collect::<String>()
                    }));
                }
                StreamEvent::Usage { input_tokens, output_tokens, model, .. } => {
                    total_tokens += input_tokens + output_tokens;
                    total_cost += estimate_cost(input_tokens, output_tokens, model.as_deref().unwrap_or("sonnet"));
                    log(agent_name, &format!("  tokens: +{}/+{} (total: {}, cost: ${:.4})",
                        input_tokens, output_tokens, total_tokens, total_cost));

                    write_log(&session_log_path, &json!({
                        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                        "type": "usage",
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "total_tokens": total_tokens,
                        "cost": total_cost
                    }));

                    // Real-time limit check during streaming
                    if total_tokens >= config.limits.max_session_tokens
                        || total_cost >= config.limits.max_session_cost_usd
                    {
                        log(agent_name, "limit reached during streaming — will exit after this turn");
                    }
                }
                StreamEvent::MessageHistory(history) => {
                    messages = history;
                    turn_done = true;
                }
                StreamEvent::Done => {
                    if !turn_done {
                        // Stream ended without MessageHistory — single turn, no tool use
                        turn_done = true;
                    }
                }
                StreamEvent::Error(e) => {
                    log(agent_name, &format!("ERROR: {}", e));
                    turn_done = true;
                }
                _ => {} // Thinking, SubagentStart/Update/Done, etc.
            }
        }

        // If sentinel_exit was called, agent is done
        if sentinel_exit_called {
            log(agent_name, "agent called sentinel_exit — clean shutdown");
            break;
        }

        // If stream ended without tool calls (just text response), agent is idle
        // In a non-always mode this would trigger sleep, but for now just check if
        // the last message was from the assistant with no tool use
        if let Some(last) = messages.last() {
            if last["role"].as_str() == Some("assistant") && last["stop_reason"].as_str() == Some("end_turn") {
                // Agent stopped on its own without calling sentinel_exit
                // Give it one more chance to write handoff
                if !sentinel_exit_called {
                    log(agent_name, "agent ended turn without tool calls — prompting for handoff");
                    messages.push(json!({
                        "role": "user",
                        "content": "You stopped without calling sentinel_exit. If you're done, call sentinel_exit now with your handoff state. If you have more work, continue."
                    }));
                    // Let it have one more turn
                    continue;
                }
            }
        }
    }

    // Exit phase — if we didn't get a clean sentinel_exit, ask for handoff
    if !sentinel_exit_called && !interrupted.load(Ordering::Relaxed) {
        log(agent_name, "requesting handoff before shutdown...");
        messages.push(json!({
            "role": "user",
            "content": "You're being shut down (resource limit reached). Call sentinel_exit NOW with your handoff state — summarize what you did, what's pending, and any context your next session needs."
        }));

        let cancel = CancellationToken::new();
        let mut stream = runtime.run_stream_with_messages(messages.clone(), cancel, None).await;
        
        // Give it 60 seconds to write handoff
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
        loop {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(StreamEvent::ToolUseStart(name)) if name == "sentinel_exit" => {
                            sentinel_exit_called = true;
                        }
                        Some(StreamEvent::Done) | None => break,
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    log(agent_name, "handoff deadline exceeded — forcing exit");
                    break;
                }
            }
        }
    }

    // Stop heartbeat
    hb_running.store(false, Ordering::Relaxed);

    // If we still don't have a handoff, write a minimal one
    if !sentinel_exit_called {
        log(agent_name, "no handoff from agent — writing minimal state");
        let minimal = HandoffState {
            summary: format!("Session ended without clean handoff. Ran for {:.0}s, {} tokens, ${:.4}",
                session_start.elapsed().as_secs_f64(), total_tokens, total_cost),
            pending: vec!["Review previous session — no clean handoff was written".to_string()],
            context: serde_json::Value::Null,
        };
        let json = serde_json::to_string_pretty(&minimal).unwrap_or_default();
        let _ = std::fs::write(&handoff_path, &json);
    }

    let elapsed = session_start.elapsed().as_secs_f64();
    
    // Log exit event
    let exit_reason = if sentinel_exit_called {
        "sentinel_exit"
    } else if interrupted.load(Ordering::Relaxed) {
        "signal"
    } else if total_tokens >= config.limits.max_session_tokens {
        "token_limit"
    } else if total_cost >= config.limits.max_session_cost_usd {
        "cost_limit"
    } else if session_start.elapsed() >= max_duration {
        "time_limit"
    } else if total_tool_calls >= config.limits.max_tool_calls {
        "tool_limit"
    } else {
        "unknown"
    };
    
    write_log(&session_log_path, &json!({
        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "type": "exit",
        "reason": exit_reason,
        "total_tokens": total_tokens,
        "total_cost": total_cost,
        "tool_calls": total_tool_calls,
        "duration_secs": elapsed as u64
    }));
    
    log(agent_name, &format!(
        "session complete — {:.0}s, {} tokens, {} tool calls, ${:.4}",
        elapsed, total_tokens, total_tool_calls, total_cost
    ));

    std::process::exit(0);
}
