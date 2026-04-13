use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Commands sent from CLI to supervisor
#[derive(Debug, Serialize, Deserialize)]
pub enum SentinelCommand {
    Deploy { name: String },
    Stop { name: String },
    Status,
    AgentStatus { name: String },
}

/// Responses from supervisor to CLI
#[derive(Debug, Serialize, Deserialize)]
pub enum SentinelResponse {
    Ok { message: String },
    Error { message: String },
    Status { agents: Vec<AgentStatusInfo> },
    AgentDetail { info: AgentStatusInfo },
}

/// Runtime info for a single agent (for status display)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusInfo {
    pub name: String,
    pub trigger: String,
    pub status: String,        // "running", "sleeping", "stopped", "crashed"
    pub session_count: u64,
    pub uptime_secs: Option<f64>,
    pub pid: Option<u32>,
    pub consecutive_crashes: u32,
}

/// Agent configuration parsed from config.toml
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub agent: AgentInfo,
    #[serde(default)]
    pub limits: SessionLimits,
    #[serde(default)]
    pub boot: BootConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_thinking")]
    pub thinking: String,
    #[serde(default = "default_trigger")]
    pub trigger: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionLimits {
    #[serde(default = "default_max_tokens")]
    pub max_session_tokens: u64,
    #[serde(default = "default_max_duration")]
    pub max_session_duration_mins: u64,
    #[serde(default = "default_max_cost")]
    pub max_session_cost_usd: f64,
    #[serde(default = "default_max_daily_cost")]
    pub max_daily_cost_usd: f64,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BootConfig {
    #[serde(default = "default_boot_message")]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatConfig {
    #[serde(default = "default_heartbeat_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_heartbeat_stale")]
    pub stale_threshold_secs: u64,
}

/// What the agent writes for its next self
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HandoffState {
    pub summary: String,
    pub pending: Vec<String>,
    pub context: serde_json::Value,
}

/// Why an agent session ended
#[derive(Debug, Clone, Serialize)]
pub enum ExitReason {
    TokenLimit,
    TimeLimit,
    CostLimit,
    ToolCallLimit,
    AgentExit { reason: String },
    Crashed { error: String },
    Interrupted,
}

/// Stats tracked per agent session
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStats {
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub total_tool_calls: u64,
    pub duration_secs: f64,
    pub exit_reason: Option<String>,
}

/// Stats persisted across sessions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStats {
    pub total_sessions: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub total_uptime_secs: f64,
    pub crashes: u64,
    pub last_crash: Option<String>,
    pub today: DailyStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyStats {
    pub date: String,
    pub sessions: u64,
    pub cost_usd: f64,
    pub tokens: u64,
}

impl AgentConfig {
    /// Load config from a TOML file
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config: {}", e))?;
        toml::from_str(&content)
            .map_err(|e| format!("Failed to parse config: {}", e))
    }

    /// Resolve the agent directory (parent of config.toml)
    pub fn agent_dir(path: &std::path::Path) -> PathBuf {
        path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf()
    }

    /// Load soul.md from the agent directory
    pub fn load_soul(agent_dir: &std::path::Path) -> Result<String, String> {
        let soul_path = agent_dir.join("soul.md");
        std::fs::read_to_string(&soul_path)
            .map_err(|e| format!("Failed to read soul.md: {}", e))
    }

    /// Load handoff state from the agent directory
    pub fn load_handoff(agent_dir: &std::path::Path) -> HandoffState {
        let handoff_path = agent_dir.join("handoff.json");
        std::fs::read_to_string(&handoff_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

// -- Defaults ----------------------------------------------------------------

fn default_model() -> String { "claude-sonnet-4-20250514".to_string() }
fn default_thinking() -> String { "medium".to_string() }
fn default_trigger() -> String { "manual".to_string() }
fn default_max_tokens() -> u64 { 100_000 }
fn default_max_duration() -> u64 { 60 }
fn default_max_cost() -> f64 { 0.50 }
fn default_max_daily_cost() -> f64 { 10.0 }
fn default_max_tool_calls() -> u64 { 200 }
fn default_cooldown() -> u64 { 10 }
fn default_max_retries() -> u32 { 3 }
fn default_heartbeat_interval() -> u64 { 30 }
fn default_heartbeat_stale() -> u64 { 120 }

fn default_boot_message() -> String {
    r#"You are waking up for a new session. Current time: {timestamp}

## State from your last session:
{handoff}

## What triggered this session:
{trigger_context}

Review your state, decide what to do, and get to work. When you've completed your work or hit a natural stopping point, call the `sentinel_exit` tool with your handoff state."#.to_string()
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_session_tokens: default_max_tokens(),
            max_session_duration_mins: default_max_duration(),
            max_session_cost_usd: default_max_cost(),
            max_daily_cost_usd: default_max_daily_cost(),
            max_tool_calls: default_max_tool_calls(),
            cooldown_secs: default_cooldown(),
            max_retries: default_max_retries(),
        }
    }
}

impl Default for BootConfig {
    fn default() -> Self {
        Self { message: default_boot_message() }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_heartbeat_interval(),
            stale_threshold_secs: default_heartbeat_stale(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[agent]
name = "dexter"
"#;
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.name, "dexter");
        assert_eq!(config.agent.model, "claude-sonnet-4-20250514");
        assert_eq!(config.agent.trigger, "manual");
        assert_eq!(config.limits.max_session_tokens, 100_000);
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[agent]
name = "dexter"
model = "claude-opus-4-6"
thinking = "high"
trigger = "always"

[limits]
max_session_tokens = 50000
max_session_duration_mins = 30
max_session_cost_usd = 1.0
max_daily_cost_usd = 5.0
cooldown_secs = 30

[boot]
message = "Wake up, {timestamp}. {handoff}"

[heartbeat]
interval_secs = 15
stale_threshold_secs = 60
"#;
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.name, "dexter");
        assert_eq!(config.agent.model, "claude-opus-4-6");
        assert_eq!(config.agent.trigger, "always");
        assert_eq!(config.limits.max_session_tokens, 50000);
        assert_eq!(config.limits.cooldown_secs, 30);
        assert_eq!(config.heartbeat.interval_secs, 15);
        assert!(config.boot.message.contains("{timestamp}"));
    }

    #[test]
    fn test_parse_invalid_toml() {
        let bad = "this is not valid toml [[[";
        let result: Result<AgentConfig, _> = toml::from_str(bad);
        assert!(result.is_err());
    }

    #[test]
    fn test_handoff_default() {
        let h = HandoffState::default();
        assert!(h.summary.is_empty());
        assert!(h.pending.is_empty());
    }

    #[test]
    fn test_handoff_roundtrip() {
        let h = HandoffState {
            summary: "Did market analysis".to_string(),
            pending: vec!["Check BTC".to_string()],
            context: serde_json::json!({"last_price": 42000}),
        };
        let json = serde_json::to_string(&h).unwrap();
        let h2: HandoffState = serde_json::from_str(&json).unwrap();
        assert_eq!(h2.summary, "Did market analysis");
        assert_eq!(h2.pending.len(), 1);
    }

    #[test]
    fn test_session_limits_defaults() {
        let limits = SessionLimits::default();
        assert_eq!(limits.max_session_tokens, 100_000);
        assert_eq!(limits.max_session_duration_mins, 60);
        assert_eq!(limits.cooldown_secs, 10);
        assert_eq!(limits.max_retries, 3);
    }
}
