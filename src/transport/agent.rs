//! AgentHarness — wraps ConversationDriver with autonomous agent lifecycle.
//!
//! Handles: limit enforcement, heartbeat, JSONL logging, handoff protocol,
//! boot message templating, watcher_exit detection, stats persistence.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::watcher_types::{
    AgentConfig, AgentStats, DailyStats, HandoffState,
};
use crate::{Runtime, Session};

use super::driver::{ConversationDriver, DriverConfig};
use super::events::{AgentEvent, MetaEvent, ToolEvent};
use super::Inbound;

// ---------------------------------------------------------------------------
// Helpers (ported from old agent.rs)
// ---------------------------------------------------------------------------

fn log(agent: &str, msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    eprintln!("[{}] [{}] {}", ts, agent, msg);
}

fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_log(log_path: &Path, entry: &Value) {
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
    let mut max = 0u64;
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("session-") && s.ends_with(".jsonl") {
                if let Ok(n) = s
                    .trim_start_matches("session-")
                    .trim_end_matches(".jsonl")
                    .parse::<u64>()
                {
                    max = max.max(n);
                }
            }
        }
    }
    max + 1
}

fn load_stats(agent_dir: &Path) -> AgentStats {
    use std::io::Read;
    let path = agent_dir.join("stats.json");
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).open(&path) else {
        return AgentStats::default();
    };
    #[allow(clippy::incompatible_msrv)]
    {
        #[allow(unused_imports)]
        use fs4::fs_std::FileExt;
        if file.lock_shared().is_err() {
            return AgentStats::default();
        }
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    serde_json::from_str(&buf).unwrap_or_default()
}

fn update_stats(agent_dir: &Path, updater: impl FnOnce(&mut AgentStats)) {
    use std::io::{Read, Seek, Write};
    let path = agent_dir.join("stats.json");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return;
    };
    #[allow(clippy::incompatible_msrv)]
    {
        use fs4::fs_std::FileExt;
        if file.lock_exclusive().is_err() {
            return;
        }
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    let mut stats: AgentStats = serde_json::from_str(&buf).unwrap_or_default();
    updater(&mut stats);
    let _ = file.set_len(0);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = serde_json::to_writer_pretty(&mut file, &stats);
    let _ = file.flush();
}

/// Build boot message from template, substituting variables.
pub fn build_boot_message(
    template: &str,
    handoff: &HandoffState,
    trigger_context: &str,
) -> String {
    let timestamp = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string();
    let handoff_json = serde_json::to_string_pretty(handoff).unwrap_or_default();
    template
        .replace("{timestamp}", &timestamp)
        .replace("{handoff}", &handoff_json)
        .replace("{trigger_context}", trigger_context)
}

// ---------------------------------------------------------------------------
// AgentHarness
// ---------------------------------------------------------------------------

pub struct AgentHarness {
    config: AgentConfig,
    agent_dir: PathBuf,

    // Session tracking
    total_tokens: u64,
    total_cost: f64,
    total_tool_calls: u64,
    session_start: Instant,

    // Heartbeat
    heartbeat_path: PathBuf,
    heartbeat_interval: Duration,

    // JSONL log
    session_log_path: PathBuf,
    session_number: u64,

    // Boot
    boot_message: String,
    trigger_context: String,

    // State flags
    watcher_exit_called: bool,
    exit_reason: String,
}

impl AgentHarness {
    /// Build a harness from a config file path + optional trigger context.
    pub async fn from_config(
        config_path: &str,
        trigger_context: Option<&str>,
    ) -> Result<Self, String> {
        let path = PathBuf::from(config_path);
        let config = AgentConfig::load(&path)?;
        let agent_dir = AgentConfig::agent_dir(&path);
        let trigger = trigger_context.unwrap_or("manual start").to_string();

        // Check daily cost limit
        let stats = load_stats(&agent_dir);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if stats.today.date == today
            && stats.today.cost_usd >= config.limits.max_daily_cost_usd
        {
            return Err(format!(
                "daily cost limit reached (${:.2}/${:.2})",
                stats.today.cost_usd, config.limits.max_daily_cost_usd
            ));
        }

        // Setup session log
        let logs_dir = agent_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap_or_default();
        let session_number = get_session_number(&logs_dir);
        let session_log_path = logs_dir.join(format!("session-{:03}.jsonl", session_number));
        let current_log = logs_dir.join("current.log");
        let _ = atomic_write(&current_log, session_log_path.to_string_lossy().as_bytes());

        // Load soul + handoff, build boot message
        let handoff = AgentConfig::load_handoff(&agent_dir);
        let handoff_json = serde_json::to_string_pretty(&handoff).unwrap_or_default();
        if handoff_json.len() > 50 * 1024 {
            log(
                &config.agent.name,
                &format!(
                    "WARNING: handoff state large ({}KB)",
                    handoff_json.len() / 1024
                ),
            );
        }

        let boot_message = build_boot_message(&config.boot.message, &handoff, &trigger);

        let heartbeat_interval =
            Duration::from_secs(config.heartbeat.interval_secs);
        let heartbeat_path = agent_dir.join("heartbeat");

        Ok(Self {
            config,
            agent_dir,
            total_tokens: 0,
            total_cost: 0.0,
            total_tool_calls: 0,
            session_start: Instant::now(),
            heartbeat_path,
            heartbeat_interval,
            session_log_path,
            session_number,
            boot_message,
            trigger_context: trigger,
            watcher_exit_called: false,
            exit_reason: "unknown".to_string(),
        })
    }

    /// Run the autonomous agent session.
    pub async fn run(&mut self) -> Result<(), String> {
        let name = self.config.agent.name.clone();
        log(&name, &format!(
            "booting (model: {}, trigger: {})",
            self.config.agent.model, self.config.agent.trigger
        ));

        // Log boot event
        write_log(&self.session_log_path, &json!({
            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "type": "boot",
            "session": self.session_number,
            "model": self.config.agent.model,
            "trigger": self.trigger_context,
        }));

        // Load soul
        let soul = AgentConfig::load_soul(&self.agent_dir)
            .map_err(|e| format!("FATAL: {}", e))?;

        // Create runtime
        let mut runtime = Runtime::new().await.map_err(|e| format!("{}", e))?;
        runtime.set_model(self.config.agent.model.clone());
        runtime.set_system_prompt(soul);

        let handoff_path = self.agent_dir.join("handoff.json");
        runtime.watcher_exit_path = Some(handoff_path.clone());

        // Register watcher_exit tool
        {
            let tools = runtime.tools_shared();
            let mut tools = tools.write().await;
            tools.register(Arc::new(crate::tools::WatcherExitTool));
        }

        // Apply thinking budget from config
        let thinking_budget = match self.config.agent.thinking.as_str() {
            "low" => 2048,
            "medium" | "med" => 4096,
            "high" => 16384,
            "xhigh" => 32768,
            _ => 4096,
        };
        runtime.set_thinking_budget(thinking_budget);

        // Create session + driver
        let session = Session::new(
            runtime.model(),
            runtime.thinking_level(),
            runtime.system_prompt(),
        );

        let driver_config = DriverConfig {
            agent_name: Some(name.clone()),
            auto_save: false, // Agent manages its own lifecycle
            ..Default::default()
        };

        let mut driver = ConversationDriver::new(runtime, session, driver_config);

        // Get bus handle before we move driver
        let bus_handle = driver.bus().handle();
        let mut event_rx = bus_handle.subscribe();
        let inbound_tx = bus_handle.inbound();

        // Spawn driver loop
        tokio::spawn(async move {
            let _ = driver.run().await;
        });

        // Send boot message
        inbound_tx
            .send(Inbound::Message {
                content: self.boot_message.clone(),
            })
            .map_err(|_| "failed to send boot message".to_string())?;

        self.session_start = Instant::now();
        log(&name, "session started — entering agentic loop");

        // Main monitor loop
        let mut heartbeat_tick =
            tokio::time::interval(self.heartbeat_interval);
        let max_duration = Duration::from_secs(
            self.config.limits.max_session_duration_mins * 60,
        );

        // Signal handling
        let (sig_tx, mut sig_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = sig_tx.send(()).await;
        });

        let mut interrupted = false;

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Ok(ref ev) => {
                            self.log_event(ev);

                            if self.check_watcher_exit(ev) {
                                self.exit_reason = "watcher_exit".to_string();
                                break;
                            }

                            if let Some(reason) = self.check_limits(ev) {
                                self.exit_reason = reason;
                                log(&name, &format!("limit reached: {}", self.exit_reason));
                                // Request shutdown from driver
                                let _ = inbound_tx.send(Inbound::Cancel);
                                break;
                            }

                            // Time limit check on turn complete
                            if matches!(ev, AgentEvent::TurnComplete)
                                && self.session_start.elapsed() >= max_duration
                            {
                                self.exit_reason = "time_limit".to_string();
                                log(&name, &format!(
                                    "time limit reached ({}m)",
                                    self.config.limits.max_session_duration_mins
                                ));
                                let _ = inbound_tx.send(Inbound::Cancel);
                                break;
                            }

                            if matches!(ev, AgentEvent::Meta(MetaEvent::Shutdown { .. })) {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            log(&name, &format!("bus lagged {} events", n));
                        }
                    }
                }

                _ = heartbeat_tick.tick() => {
                    self.write_heartbeat();
                }

                _ = sig_rx.recv() => {
                    interrupted = true;
                    self.exit_reason = "signal".to_string();
                    log(&name, "interrupted by signal");
                    let _ = inbound_tx.send(Inbound::Cancel);
                    break;
                }
            }
        }

        // If no clean watcher_exit and not interrupted, write minimal handoff
        if !self.watcher_exit_called && !interrupted {
            let handoff = HandoffState {
                summary: format!(
                    "Session ended without clean handoff ({}). Ran for {:.0}s, {} tokens, ${:.4}",
                    self.exit_reason,
                    self.session_start.elapsed().as_secs_f64(),
                    self.total_tokens,
                    self.total_cost,
                ),
                pending: vec![
                    "Review previous session — no clean handoff was written"
                        .to_string(),
                ],
                context: Value::Null,
            };
            let json = serde_json::to_string_pretty(&handoff).unwrap_or_default();
            let _ = atomic_write(&handoff_path, json.as_bytes());
        }

        self.finalize(interrupted);
        Ok(())
    }

    // -- Event processing ---------------------------------------------------

    fn log_event(&mut self, event: &AgentEvent) {
        let name = &self.config.agent.name;
        match event {
            AgentEvent::Text(text) => {
                if text.len() > 100 {
                    log(name, &format!("output: {}...", &text[..100]));
                    write_log(&self.session_log_path, &json!({
                        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                        "type": "text",
                        "length": text.len(),
                        "preview": text.chars().take(200).collect::<String>(),
                    }));
                }
            }
            AgentEvent::Tool(ToolEvent::Start { tool_name, .. }) => {
                log(name, &format!("tool: {}", tool_name));
                write_log(&self.session_log_path, &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "tool_start",
                    "name": tool_name,
                    "call_num": self.total_tool_calls + 1,
                }));
            }
            AgentEvent::Tool(ToolEvent::Complete { result, .. }) => {
                let preview: String = result.chars().take(100).collect();
                log(name, &format!("  result: {}", preview));
                write_log(&self.session_log_path, &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "tool_result",
                    "preview": result.chars().take(200).collect::<String>(),
                }));
            }
            AgentEvent::Meta(MetaEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            }) => {
                write_log(&self.session_log_path, &json!({
                    "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "type": "usage",
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "total_tokens": self.total_tokens,
                    "cost": self.total_cost,
                }));
            }
            AgentEvent::Error(e) => {
                log(name, &format!("ERROR: {}", e));
            }
            _ => {}
        }
    }

    fn check_watcher_exit(&mut self, event: &AgentEvent) -> bool {
        if let AgentEvent::Tool(ToolEvent::Invoke { tool_name, .. }) = event {
            if tool_name == "watcher_exit" {
                self.watcher_exit_called = true;
                log(&self.config.agent.name, "agent called watcher_exit — clean shutdown");
                return true;
            }
        }
        false
    }

    /// Check limits. Returns Some(reason) if a limit was breached.
    fn check_limits(&mut self, event: &AgentEvent) -> Option<String> {
        let limits = &self.config.limits;

        // Update accumulators from events
        match event {
            AgentEvent::Meta(MetaEvent::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            }) => {
                self.total_tokens += input_tokens + output_tokens;
                self.total_cost += cost_usd;

                log(
                    &self.config.agent.name,
                    &format!(
                        "  tokens: +{}/+{} (total: {}, cost: ${:.4})",
                        input_tokens, output_tokens, self.total_tokens, self.total_cost
                    ),
                );

                if self.total_tokens >= limits.max_session_tokens {
                    return Some("token_limit".to_string());
                }
                if self.total_cost >= limits.max_session_cost_usd {
                    return Some("cost_limit".to_string());
                }
            }
            AgentEvent::Tool(ToolEvent::Start { .. }) => {
                self.total_tool_calls += 1;
                if self.total_tool_calls >= limits.max_tool_calls {
                    return Some("tool_limit".to_string());
                }
            }
            _ => {}
        }

        None
    }

    fn write_heartbeat(&self) {
        let ts = chrono::Utc::now().timestamp().to_string();
        let _ = atomic_write(&self.heartbeat_path, ts.as_bytes());
    }

    fn finalize(&self, interrupted: bool) {
        let name = &self.config.agent.name;
        let elapsed = self.session_start.elapsed().as_secs_f64();

        write_log(&self.session_log_path, &json!({
            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "type": "exit",
            "reason": self.exit_reason,
            "total_tokens": self.total_tokens,
            "total_cost": self.total_cost,
            "tool_calls": self.total_tool_calls,
            "duration_secs": elapsed as u64,
        }));

        log(name, &format!(
            "session complete — {:.0}s, {} tokens, {} tool calls, ${:.4}",
            elapsed, self.total_tokens, self.total_tool_calls, self.total_cost
        ));

        let watcher_exit_called = self.watcher_exit_called;
        let total_tokens = self.total_tokens;
        let total_cost = self.total_cost;
        let session_elapsed = elapsed;
        let exit_reason = self.exit_reason.clone();

        update_stats(&self.agent_dir, move |stats| {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            if stats.today.date != today {
                stats.today = DailyStats {
                    date: today,
                    sessions: 0,
                    cost_usd: 0.0,
                    tokens: 0,
                };
            }
            stats.total_sessions += 1;
            stats.total_tokens += total_tokens;
            stats.total_cost_usd += total_cost;
            stats.total_uptime_secs += session_elapsed;
            stats.today.sessions += 1;
            stats.today.cost_usd += total_cost;
            stats.today.tokens += total_tokens;

            if !watcher_exit_called && !interrupted {
                stats.crashes += 1;
                stats.last_crash = Some(format!(
                    "{}: {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    exit_reason,
                ));
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_boot_message_basic() {
        let template = "Time: {timestamp}, Handoff: {handoff}, Trigger: {trigger_context}";
        let handoff = HandoffState {
            summary: "did stuff".to_string(),
            pending: vec!["more stuff".to_string()],
            context: Value::Null,
        };
        let msg = build_boot_message(template, &handoff, "cron");
        assert!(msg.contains("did stuff"));
        assert!(msg.contains("more stuff"));
        assert!(msg.contains("cron"));
        assert!(!msg.contains("{timestamp}"));
        assert!(!msg.contains("{handoff}"));
        assert!(!msg.contains("{trigger_context}"));
    }

    #[test]
    fn test_build_boot_message_empty_handoff() {
        let template = "State: {handoff}";
        let handoff = HandoffState::default();
        let msg = build_boot_message(template, &handoff, "manual");
        assert!(msg.contains("State:"));
        assert!(!msg.contains("{handoff}"));
    }

    #[test]
    fn test_get_session_number_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(get_session_number(dir.path()), 1);
    }

    #[test]
    fn test_get_session_number_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("session-001.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("session-003.jsonl"), "").unwrap();
        assert_eq!(get_session_number(dir.path()), 4);
    }

    #[test]
    fn test_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn test_write_log_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        write_log(&path, &json!({"type": "test"}));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"type\":\"test\""));
    }

    #[test]
    fn test_stats_update_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        update_stats(dir.path(), |s| {
            s.total_sessions = 5;
            s.total_cost_usd = 1.23;
        });
        let stats = load_stats(dir.path());
        assert_eq!(stats.total_sessions, 5);
        assert!((stats.total_cost_usd - 1.23).abs() < f64::EPSILON);
    }

    #[test]
    fn test_load_stats_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let stats = load_stats(dir.path());
        assert_eq!(stats.total_sessions, 0);
    }
}
