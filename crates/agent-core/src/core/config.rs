use crate::core::shell_config::ShellConfig;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

static PROFILE_NAME: OnceLock<Option<String>> = OnceLock::new();
static PROVIDER_KEYS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
static IDENTITY: OnceLock<String> = OnceLock::new();

pub const DEFAULT_IDENTITY: &str =
    "You are an AI assistant running in SynapsCLI, an open-source agent runtime.";

/// Returns the configured identity string for the system prompt preamble.
/// Falls back to `DEFAULT_IDENTITY` (the SynapsCLI identity above) if not set
/// in config. Initialized by `load_config()` — safe to call anytime after boot.
pub fn get_identity() -> String {
    IDENTITY
        .get()
        .cloned()
        .unwrap_or_else(|| DEFAULT_IDENTITY.to_string())
}

/// Provider API keys parsed from `provider.<name> = ...` lines in config.
/// Empty if `load_config()` hasn't been called. The registry falls back to
/// env vars, so e.g. `GROQ_API_KEY` works even with an empty map.
pub fn get_provider_keys() -> BTreeMap<String, String> {
    PROVIDER_KEYS.get().cloned().unwrap_or_default()
}

/// Returns the active profile name, if any.
/// Reads from `SYNAPS_PROFILE` environment variable if not already set programmatically.
pub fn get_profile() -> Option<String> {
    PROFILE_NAME
        .get_or_init(|| std::env::var("SYNAPS_PROFILE").ok())
        .clone()
}

/// Sets the active profile name. Must be called before any `get_profile()` call
/// (i.e., before config resolution begins). Uses OnceLock — first write wins,
/// subsequent calls are no-ops. No env var mutation (unsafe under tokio).
pub fn set_profile(name: Option<String>) {
    let _ = PROFILE_NAME.set(name);
}

pub fn base_dir() -> PathBuf {
    if let Ok(path) = std::env::var("SYNAPS_BASE_DIR") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".synaps-cli")
}

/// Overrides the Synaps base directory. Intended for tests and embedded harnesses.
#[doc(hidden)]
pub fn set_base_dir_for_tests(path: PathBuf) {
    std::env::set_var("SYNAPS_BASE_DIR", path);
}

/// Resolves a path for reading. Checks the profile folder first, then falls back to the default folder.
pub fn resolve_read_path(filename: &str) -> PathBuf {
    let base = base_dir();

    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(filename);
        if profile_path.exists() {
            return profile_path;
        }
    }

    base.join(filename)
}

/// Resolves a path for reading with an extended arbitrary path tree.
pub fn resolve_read_path_extended(path: &str) -> PathBuf {
    let base = base_dir();

    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(path);
        if profile_path.exists() {
            return profile_path;
        }
    }

    base.join(path)
}

/// Resolves a path for writing. Unconditionally writes to the profile folder if a profile is active.
pub fn resolve_write_path(filename: &str) -> PathBuf {
    let mut base = base_dir();

    if let Some(profile) = get_profile() {
        base.push(profile);
    }

    let _ = std::fs::create_dir_all(&base);
    base.join(filename)
}

/// Gets the absolute directory for the current profile (or root if default).
pub fn get_active_config_dir() -> PathBuf {
    let mut base = base_dir();
    if let Some(profile) = get_profile() {
        base.push(profile);
    }
    base
}

/// Server security configuration parsed from `server.*` keys.
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// Comma-separated list of allowed Origin headers. Empty = allow all (localhost-only protection).
    pub allowed_origins: Vec<String>,
    /// Pre-shared authentication token. If set, clients must provide it on WebSocket upgrade
    /// via `?token=X` query param or `Authorization: Bearer X` header. If None, auto-generated on boot.
    pub token: Option<String>,
    /// When true, `HookResult::Confirm` is auto-approved without prompting (useful for headless/agent mode).
    pub auto_approve_confirms: bool,
    /// Maximum inbound message size in bytes. Defaults to context_window * 4 (rough token→byte estimate).
    /// None means no artificial cap.
    pub max_message_size: Option<usize>,
}

/// Bridge daemon configuration parsed from `bridge.*` keys.
///
/// Controls best-effort mirroring of watcher heartbeats over the bridge
/// daemon's UDS `ControlSocket` (`heartbeat_emit` op). All keys are optional
/// and the feature is OFF by default.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Path to the bridge daemon's UDS control socket.
    /// When `None`, defaults to `base_dir().join("bridge/control.sock")`.
    pub uds_path: Option<PathBuf>,
    /// When true, every watcher heartbeat tick mirrors a `heartbeat_emit`
    /// JSON-RPC call over the UDS socket. Errors are logged at debug only.
    pub heartbeat_mirror: bool,
    /// Per-call timeout in milliseconds (covers connect + write + read).
    pub heartbeat_timeout_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            uds_path: None,
            heartbeat_mirror: false,
            heartbeat_timeout_ms: 250,
        }
    }
}

impl BridgeConfig {
    /// Resolve the UDS path, falling back to the default under `base_dir()`.
    pub fn resolved_uds_path(&self) -> PathBuf {
        self.uds_path
            .clone()
            .unwrap_or_else(|| base_dir().join("bridge/control.sock"))
    }
}

/// Auth configuration parsed from `auth.*` keys. Controls whether this client
/// resolves provider tokens from the local `auth.json` (default) or from a
/// remote credential broker. See task #157 / `auth::credential_source`.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Broker base URL. When set (here, or via `SYNAPS_AUTH_ENDPOINT`), the
    /// client fetches short-lived access tokens from the broker instead of
    /// reading/refreshing the local `auth.json`.
    pub remote_endpoint: Option<String>,
    /// Per-machine bearer presented to the broker (or `SYNAPS_MACHINE_TOKEN`).
    /// This is the machine's own identity, never the provider credential.
    pub machine_token: Option<String>,
}

impl AuthConfig {
    /// Resolve the credential source. Environment variables take precedence over
    /// config-file values: `SYNAPS_AUTH_ENDPOINT` / `SYNAPS_MACHINE_TOKEN`.
    /// Returns `Remote` iff an endpoint is set (env or config), else `Local`.
    pub fn credential_source(&self) -> crate::core::auth::CredentialSource {
        let endpoint = std::env::var("SYNAPS_AUTH_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.remote_endpoint.clone());
        let machine_token = std::env::var("SYNAPS_MACHINE_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.machine_token.clone());
        crate::core::auth::CredentialSource::from_parts(endpoint, machine_token)
    }
}

/// Prompt-cache TTL strategy for Anthropic requests.
///
/// Controls the `cache_control` value emitted at every cache marker site:
/// - `FiveMinutes` (default): bare `{"type": "ephemeral"}` — byte-identical
///   to historical payloads; the default path can never invalidate existing
///   cached prefixes.
/// - `OneHour`: `{"type": "ephemeral", "ttl": "1h"}` on all markers.
/// - `Hybrid`: 1h on the stable prefix (tools + system, written rarely),
///   bare 5m on the message-tail marker (written every turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
    Hybrid,
}

impl CacheTtl {
    /// Parse a config value (case-insensitive). Returns `None` for unknown
    /// values so the caller can warn and fall back to the default.
    pub fn parse(val: &str) -> Option<CacheTtl> {
        match val.to_ascii_lowercase().as_str() {
            "5m" | "5min" | "default" => Some(CacheTtl::FiveMinutes),
            "1h" | "60m" | "1hr" => Some(CacheTtl::OneHour),
            "hybrid" => Some(CacheTtl::Hybrid),
            _ => None,
        }
    }
}

/// Runtime event routing configuration parsed from `events.*` keys.
///
/// Controls how runtime events (from the `EventQueue`) are delivered in
/// server and RPC modes.
#[derive(Debug, Clone)]
pub struct EventsConfig {
    /// When `true` (default), the server/RPC session automatically triggers a
    /// model turn when runtime events arrive while idle.  Set
    /// `events.auto_turn = false` (or `0` / `no` / `off`) to opt out.
    /// Unrecognised values fail safe to `false` with a warning.
    /// The built-in cap (`AUTO_TURN_CAP = 5`) still applies regardless.
    pub auto_turn: bool,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self { auto_turn: true }
    }
}

/// Parse `events.*` configuration keys.
fn parse_events_config_key(cfg: &mut EventsConfig, key: &str, val: &str) {
    if key == "events.auto_turn" {
        let normalised = val.trim().to_lowercase();
        cfg.auto_turn = match normalised.as_str() {
            // Explicit true values.
            "true" | "1" | "yes" | "on" => true,
            // Explicit false values.
            "false" | "0" | "no" | "off" => false,
            // Unrecognised: fail safe to false and warn so the user knows.
            other => {
                eprintln!(
                    "warning: config: unrecognised value for events.auto_turn = {:?}; \
                     expected true/false/yes/no/on/off/1/0 — defaulting to false",
                    other
                );
                false
            }
        };
    } // unknown events.* keys ignored
}

/// Typed per-role turn-budget overrides (Task 23, spec §8.1). Every field
/// is optional: unset fields keep the role's compiled default. Values are
/// parsed from `turn_budget.<role>.<field>` config keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnBudgetOverrides {
    pub max_provider_rounds: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_elapsed_secs: Option<u64>,
    pub max_accumulated_tool_result_bytes: Option<usize>,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
}

/// Per-role turn budgets (foreground, autonomous/watcher, worker).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnBudgetsConfig {
    pub foreground: TurnBudgetOverrides,
    pub autonomous: TurnBudgetOverrides,
    pub worker: TurnBudgetOverrides,
}

/// Parse `turn_budget.<role>.<field>` keys. Invalid values warn and keep
/// the default (parity with shell.* parsing).
fn parse_turn_budget_config_key(budgets: &mut TurnBudgetsConfig, key: &str, val: &str) {
    let Some(rest) = key.strip_prefix("turn_budget.") else {
        return;
    };
    let Some((role, field)) = rest.split_once('.') else {
        eprintln!("Warning: invalid turn_budget key '{key}' (expected turn_budget.<role>.<field>)");
        return;
    };
    let overrides = match role {
        "foreground" => &mut budgets.foreground,
        "autonomous" => &mut budgets.autonomous,
        "worker" => &mut budgets.worker,
        other => {
            eprintln!("Warning: unknown turn_budget role '{other}' (expected foreground|autonomous|worker)");
            return;
        }
    };
    macro_rules! set {
        ($slot:expr, $ty:ty) => {
            match val.parse::<$ty>() {
                Ok(parsed) => $slot = Some(parsed),
                Err(_) => eprintln!("Warning: invalid value for {key}: '{val}', using default"),
            }
        };
    }
    match field {
        "max_provider_rounds" => set!(overrides.max_provider_rounds, u32),
        "max_tool_calls" => set!(overrides.max_tool_calls, u32),
        "max_elapsed_secs" => set!(overrides.max_elapsed_secs, u64),
        "max_accumulated_tool_result_bytes" => {
            set!(overrides.max_accumulated_tool_result_bytes, usize)
        }
        "max_context_tokens" => set!(overrides.max_context_tokens, u64),
        "max_cost_usd" => set!(overrides.max_cost_usd, f64),
        other => eprintln!("Warning: unknown turn_budget field '{other}'"),
    }
}

/// Continuous-memory settings surface (Task A2, spec §12). Every field is
/// fail-closed: unknown or invalid values warn and keep the compiled default.
///
/// By construction this struct is the complete `memory.*` config surface.
/// Secret handling, retention, and project-scope rules are deliberately NOT
/// fields here — they are not configurable at all (spec §12: "Secret,
/// retention, and project rules are not configurable to fail open").
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryConfig {
    /// Durable default memory mode. "off" unless the operator explicitly
    /// consents via `memory.default_mode_confirmed = true` in the same
    /// config parse pass (spec §12 operator-consent rule). Known values:
    /// off | recall_once | recall_each_prompt | capture_only |
    /// capture_and_recall (spec §6.1).
    pub default_mode: String,
    /// Operator-consent latch for `default_mode` (spec §12). Must be set to
    /// `true` in the same parse pass for a non-"off" `default_mode` to take
    /// effect; otherwise `default_mode` reverts to "off" with a warning.
    pub default_mode_confirmed: bool,
    /// Max records surfaced per recall (spec §10.3 budget).
    pub recall_max_records: u32,
    /// Max tokens surfaced per recall (spec §10.3 budget).
    pub recall_max_tokens: u32,
    /// Recall time budget in milliseconds (spec §16).
    pub recall_timeout_ms: u64,
    /// Tool-result capture policy: off | summary_only (spec §11).
    pub capture_tools: String,
    /// Capture assistant turns (spec §8.2).
    pub capture_assistant: bool,
    /// Capture user turns (spec §8.2).
    pub capture_user: bool,
    /// Background consolidation: off | on (spec §11.3, explicit opt-in).
    pub auto_consolidate: String,
    /// Local embedding models: off | on (spec §11.2, never implicit).
    pub local_embeddings: String,
    /// GLiNER entity extraction: off | on (spec §11.2, never implicit).
    pub gliner: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            default_mode: "off".to_string(),
            default_mode_confirmed: false,
            recall_max_records: 8,
            recall_max_tokens: 4096,
            recall_timeout_ms: 150,
            capture_tools: "summary_only".to_string(),
            capture_assistant: true,
            capture_user: true,
            auto_consolidate: "off".to_string(),
            local_embeddings: "off".to_string(),
            gliner: "off".to_string(),
        }
    }
}

/// Memory mode strings accepted by `memory.default_mode` (spec §6.1,
/// snake_case of `MemoryContextMode`). Anything else fails closed to the
/// current value — `default_mode` can never silently become a non-off mode
/// through an unrecognised string.
const KNOWN_MEMORY_MODES: &[&str] = &[
    "off",
    "recall_once",
    "recall_each_prompt",
    "capture_only",
    "capture_and_recall",
];

/// Parse `memory.<field>` keys (Task A2, spec §12). Invalid values warn and
/// keep the default (parity with turn_budget.* parsing) — never fail open.
fn parse_memory_config_key(memory: &mut MemoryConfig, key: &str, val: &str) {
    let Some(field) = key.strip_prefix("memory.") else {
        return;
    };
    /// Strict boolean: recognised forms parse; anything else warns and
    /// keeps the current (default) value.
    fn parse_bool(slot: &mut bool, key: &str, val: &str) {
        match val {
            "true" | "1" | "yes" | "on" => *slot = true,
            "false" | "0" | "no" | "off" => *slot = false,
            other => eprintln!(
                "Warning: invalid value for {key}: '{other}', expected true/false — using default"
            ),
        }
    }
    /// Closed string-enum: value must be in `allowed`, else warn and keep
    /// the current (default) value.
    fn parse_enum(slot: &mut String, key: &str, val: &str, allowed: &[&str]) {
        if allowed.contains(&val) {
            *slot = val.to_string();
        } else {
            eprintln!(
                "Warning: invalid value for {key}: '{val}', expected one of {} — using default",
                allowed.join("|")
            );
        }
    }
    macro_rules! set_num {
        ($slot:expr, $ty:ty) => {
            match val.parse::<$ty>() {
                Ok(parsed) => $slot = parsed,
                Err(_) => eprintln!("Warning: invalid value for {key}: '{val}', using default"),
            }
        };
    }
    match field {
        "default_mode" => parse_enum(&mut memory.default_mode, key, val, KNOWN_MEMORY_MODES),
        "default_mode_confirmed" => parse_bool(&mut memory.default_mode_confirmed, key, val),
        "recall_max_records" => set_num!(memory.recall_max_records, u32),
        "recall_max_tokens" => set_num!(memory.recall_max_tokens, u32),
        "recall_timeout_ms" => set_num!(memory.recall_timeout_ms, u64),
        "capture_tools" => parse_enum(&mut memory.capture_tools, key, val, &["off", "summary_only"]),
        "capture_assistant" => parse_bool(&mut memory.capture_assistant, key, val),
        "capture_user" => parse_bool(&mut memory.capture_user, key, val),
        "auto_consolidate" => parse_enum(&mut memory.auto_consolidate, key, val, &["off", "on"]),
        "local_embeddings" => parse_enum(&mut memory.local_embeddings, key, val, &["off", "on"]),
        "gliner" => parse_enum(&mut memory.gliner, key, val, &["off", "on"]),
        other => eprintln!("Warning: unknown memory field '{other}'"),
    }
}

/// Parsed configuration from the config file.
#[derive(Debug, Clone)]
pub struct SynapsConfig {
    pub model: Option<String>,
    pub thinking_budget: Option<u32>,
    /// Named reasoning level, parsed from the same `thinking = …` key.
    /// When `Some`, this is the authoritative level. When `None`, fall back
    /// to `thinking_budget` for legacy numeric-only values.
    pub thinking_level: Option<crate::core::reasoning::ReasoningLevel>,
    pub context_window: Option<u64>, // override auto-detected context window (tokens)
    pub compaction_model: Option<String>, // model used for /compact (default: claude-sonnet-4-6)
    /// Where compaction summarization runs (spec §9.4): remote provider or
    /// local-only (zero network construction).
    pub compaction_mode: crate::core::compaction::CompactionMode,
    /// Content classes excluded from remote compaction disclosure.
    pub compaction_exclude: Vec<crate::core::compaction::ContentClass>,
    pub max_tool_output: usize,  // default 30000
    pub bash_timeout: u64,       // default 30
    pub bash_max_timeout: u64,   // default 300
    pub subagent_timeout: u64,   // default 300
    pub api_retries: u32,        // default 3
    pub refusal_retries: u32,    // default 2 — retries on stop_reason=refusal
    pub telemetry: String,       // off | basic | full (default off)
    pub cache_diagnostics: bool, // opt into cache-diagnosis beta (default false)
    /// Prompt-cache TTL strategy: "5m" (default) | "1h" | "hybrid".
    pub cache_ttl: CacheTtl,
    /// Max TUI redraw rate in frames/sec — caps streaming redraws (e.g. 60,
    /// 144, 240). User input always redraws immediately regardless. Default
    /// 60. Range 1–1000. The frame budget is `1000 / max_fps` ms.
    pub max_fps: u32,
    /// Lines to scroll per mouse-wheel event. Different terminal emulators
    /// emit 1–9+ scroll events per physical notch; set this to compensate.
    /// Default: 3. Range 1–20.
    pub scroll_lines: Option<u16>,
    pub theme: Option<String>,
    pub agent_name: Option<String>,
    pub identity: Option<String>,
    pub disabled_plugins: Vec<String>,
    pub favorite_models: Vec<String>,
    pub disabled_skills: Vec<String>,
    /// Opt-in progressive tool disclosure (Task 18). When false, providers
    /// receive the existing full tool schema byte-for-byte. When true, each
    /// stream starts with the small essential local core plus discovery and
    /// authorization gateways; exact activations are added per session.
    pub progressive_tool_disclosure: bool,
    /// Opt-in session persistence strategy (Task 35, spec §9.8). `Json`
    /// (default) is the unchanged legacy full-rewrite path; `Journal` adds
    /// an append-only delta journal with periodic atomic snapshots. See
    /// docs/decisions/T35-session-journal-opt-in.md.
    pub session_persistence: crate::core::session_journal::SessionPersistence,
    /// Built-in tools to disable by runtime name (e.g. "bash", "ls"). Removed
    /// from the registry at boot so they're never offered to the model.
    pub disabled_tools: Vec<String>,
    pub shell: ShellConfig,
    pub server: ServerConfig,
    pub bridge: BridgeConfig,
    pub auth: AuthConfig,
    pub events: EventsConfig,
    pub provider_keys: BTreeMap<String, String>,
    pub keybinds: std::collections::HashMap<String, String>,
    /// Typed per-role turn budgets (Task 23, spec §8.1).
    pub turn_budgets: TurnBudgetsConfig,
    /// Continuous-memory settings surface (Task A2, spec §12).
    pub memory: MemoryConfig,
    /// Non-fatal problems found while parsing the config file (unknown keys,
    /// unparseable values). Surfaced once at startup — never block boot.
    pub warnings: Vec<String>,
}

impl Default for SynapsConfig {
    fn default() -> Self {
        Self {
            model: None,
            thinking_budget: None,
            thinking_level: None,
            context_window: None,
            compaction_model: None,
            compaction_mode: crate::core::compaction::CompactionMode::default(),
            compaction_exclude: Vec::new(),
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            api_retries: 3,
            refusal_retries: 2,
            telemetry: "off".to_string(),
            cache_diagnostics: false,
            cache_ttl: CacheTtl::default(),
            max_fps: 60,
            scroll_lines: None,
            theme: None,
            agent_name: None,
            identity: None,
            disabled_plugins: Vec::new(),
            favorite_models: Vec::new(),
            disabled_skills: Vec::new(),
            progressive_tool_disclosure: false,
            session_persistence: crate::core::session_journal::SessionPersistence::default(),
            disabled_tools: Vec::new(),
            shell: ShellConfig::default(),
            server: ServerConfig::default(),
            bridge: BridgeConfig::default(),
            auth: AuthConfig::default(),
            events: EventsConfig::default(),
            provider_keys: BTreeMap::new(),
            keybinds: std::collections::HashMap::new(),
            turn_budgets: TurnBudgetsConfig::default(),
            memory: MemoryConfig::default(),
            warnings: Vec::new(),
        }
    }
}

/// Known top-level config keys — used for unknown-key warnings + did-you-mean.
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "model",
    "thinking",
    "compaction_model",
    "context_window",
    "max_tool_output",
    "bash_timeout",
    "bash_max_timeout",
    "subagent_timeout",
    "api_retries",
    "refusal_retries",
    "telemetry",
    "cache_diagnostics",
    "cache_ttl",
    "max_fps",
    "scroll_lines",
    "theme",
    "agent_name",
    "identity",
    "disabled_plugins",
    "favorite_models",
    "disabled_skills",
    "disabled_tools",
    "progressive_tool_disclosure",
    "session_persistence",
];

/// Simple Levenshtein distance for did-you-mean suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Closest known key within edit distance 2, for typo suggestions.
fn did_you_mean(key: &str) -> Option<&'static str> {
    KNOWN_CONFIG_KEYS
        .iter()
        .map(|k| (*k, levenshtein(key, k)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(k, _)| k)
}

// parse_thinking_budget replaced by ThinkingSpec::parse in apply_config_content.

fn parse_comma_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn write_comma_list(key: &str, values: &[String]) -> std::io::Result<()> {
    write_config_value(key, &values.join(", "))
}

/// Parse shell.* configuration keys and update the ShellConfig.
fn parse_shell_config_key(shell_config: &mut ShellConfig, key: &str, val: &str) {
    match key {
        "shell.max_sessions" => {
            if let Ok(sessions) = val.parse::<usize>() {
                shell_config.max_sessions = sessions;
            } else {
                eprintln!(
                    "Warning: invalid value for shell.max_sessions: '{}', using default",
                    val
                );
            }
        }
        "shell.idle_timeout" => {
            if let Ok(timeout) = val.parse::<u64>() {
                shell_config.idle_timeout = std::time::Duration::from_secs(timeout);
            } else {
                eprintln!(
                    "Warning: invalid value for shell.idle_timeout: '{}', using default",
                    val
                );
            }
        }
        "shell.readiness_timeout_ms" => {
            if let Ok(timeout) = val.parse::<u64>() {
                shell_config.readiness_timeout_ms = timeout;
            } else {
                eprintln!(
                    "Warning: invalid value for shell.readiness_timeout_ms: '{}', using default",
                    val
                );
            }
        }
        "shell.max_readiness_timeout_ms" => {
            if let Ok(timeout) = val.parse::<u64>() {
                shell_config.max_readiness_timeout_ms = timeout;
            } else {
                eprintln!("Warning: invalid value for shell.max_readiness_timeout_ms: '{}', using default", val);
            }
        }
        "shell.default_rows" => {
            if let Ok(rows) = val.parse::<u16>() {
                shell_config.default_rows = rows;
            } else {
                eprintln!(
                    "Warning: invalid value for shell.default_rows: '{}', using default",
                    val
                );
            }
        }
        "shell.default_cols" => {
            if let Ok(cols) = val.parse::<u16>() {
                shell_config.default_cols = cols;
            } else {
                eprintln!(
                    "Warning: invalid value for shell.default_cols: '{}', using default",
                    val
                );
            }
        }
        "shell.readiness_strategy" => {
            let val_lower = val.to_lowercase();
            match val_lower.as_str() {
                "timeout" | "prompt" | "hybrid" => {
                    shell_config.readiness_strategy = val.to_string();
                }
                _ => {
                    eprintln!(
                        "Warning: invalid value for shell.readiness_strategy: '{}', using default",
                        val
                    );
                }
            }
        }
        "shell.max_output" => {
            if let Ok(max_output) = val.parse::<usize>() {
                shell_config.max_output = max_output;
            } else {
                eprintln!(
                    "Warning: invalid value for shell.max_output: '{}', using default",
                    val
                );
            }
        }
        _ => {
            // Unknown shell.* keys are preserved (not rejected)
        }
    }
}

/// Parse server.* configuration keys and update the ServerConfig.
#[allow(clippy::collapsible_match)]
fn parse_server_config_key(server_config: &mut ServerConfig, key: &str, val: &str) {
    match key {
        "server.allowed_origins" => {
            server_config.allowed_origins = parse_comma_list(val);
        }
        "server.token" => {
            if !val.is_empty() {
                server_config.token = Some(val.to_string());
            }
        }
        "server.auto_approve_confirms" => {
            server_config.auto_approve_confirms = matches!(val, "true" | "1" | "yes");
        }
        "server.max_message_size" => {
            if let Ok(size) = val.parse::<usize>() {
                server_config.max_message_size = Some(size);
            } else {
                eprintln!(
                    "Warning: invalid value for server.max_message_size: '{}', ignored",
                    val
                );
            }
        }
        _ => {
            // Unknown server.* keys preserved (not rejected)
        }
    }
}

/// Parse bridge.* configuration keys and update the BridgeConfig.
fn parse_bridge_config_key(bridge_config: &mut BridgeConfig, key: &str, val: &str) {
    match key {
        "bridge.uds_path" => {
            if val.is_empty() {
                bridge_config.uds_path = None;
            } else {
                bridge_config.uds_path = Some(PathBuf::from(val));
            }
        }
        "bridge.heartbeat_mirror" => {
            bridge_config.heartbeat_mirror = matches!(val, "true" | "1" | "yes");
        }
        "bridge.heartbeat_timeout_ms" => {
            if let Ok(ms) = val.parse::<u64>() {
                bridge_config.heartbeat_timeout_ms = ms;
            } else {
                eprintln!(
                    "Warning: invalid value for bridge.heartbeat_timeout_ms: '{}', using default",
                    val
                );
            }
        }
        _ => {
            // Unknown bridge.* keys preserved (not rejected)
        }
    }
}

/// Parse auth.* configuration keys and update the AuthConfig.
fn parse_auth_config_key(auth_config: &mut AuthConfig, key: &str, val: &str) {
    let v = val.trim();
    match key {
        "auth.remote_endpoint" => {
            auth_config.remote_endpoint = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        "auth.machine_token" => {
            auth_config.machine_token = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        _ => {
            // Unknown auth.* keys preserved (not rejected)
        }
    }
}

/// Parse config from a raw string — useful for tests and embedded harnesses.
/// Does NOT write to `PROVIDER_KEYS` or `IDENTITY` OnceLocks.
pub fn load_config_from_str(content: &str) -> SynapsConfig {
    let mut config = SynapsConfig::default();
    apply_config_content(&mut config, content);
    config
}

/// Parse the config file at ~/.synaps-cli/config (or profile variant).
/// Returns default config if file doesn't exist or can't be read.
pub fn load_config() -> SynapsConfig {
    let path = resolve_read_path("config");
    let mut config = SynapsConfig::default();

    let Ok(content) = std::fs::read_to_string(&path) else {
        return config;
    };

    apply_config_content(&mut config, &content);

    // Publish provider keys to the process-wide cache for the API router.
    // First writer wins (OnceLock) — subsequent load_config calls are no-ops.
    let _ = PROVIDER_KEYS.set(config.provider_keys.clone());

    // Publish identity to the process-wide cache for API system prompt preamble.
    let identity_val = config
        .identity
        .clone()
        .unwrap_or_else(|| DEFAULT_IDENTITY.to_string());
    let _ = IDENTITY.set(identity_val);

    config
}

/// Apply key=value config lines from `content` into `config`.
/// Shared by `load_config` (file path) and `load_config_from_str` (test helper).
fn apply_config_content(config: &mut SynapsConfig, content: &str) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "model" => config.model = Some(val.to_string()),
            "thinking" => {
                use crate::core::reasoning::ThinkingSpec;
                match ThinkingSpec::parse(val) {
                    Some(ThinkingSpec::Named(level)) => {
                        config.thinking_level = Some(level);
                        config.thinking_budget = level.to_legacy_budget();
                    }
                    Some(ThinkingSpec::Budget(budget)) => {
                        // Preserve exact legacy budgets; do not make the derived
                        // bucket authoritative over the user's token count.
                        config.thinking_level = None;
                        config.thinking_budget = Some(budget);
                    }
                    None => {
                        config.warnings.push(format!("thinking = {val} — expected off|adaptive|low|medium|high|xhigh|max|ultra|ultracode or a token count; thinking disabled"));
                    }
                }
            }
            "compaction_model" => config.compaction_model = Some(val.to_string()),
            "compaction_mode" => match val {
                "remote" => {
                    config.compaction_mode = crate::core::compaction::CompactionMode::Remote
                }
                "local" | "local_only" | "local-only" => {
                    config.compaction_mode = crate::core::compaction::CompactionMode::LocalOnly
                }
                _ => {
                    config.warnings.push(format!(
                        "compaction_mode = {val} — expected remote or local; ignored"
                    ));
                }
            },
            "compaction_exclude" => {
                let mut classes = Vec::new();
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    match crate::core::compaction::ContentClass::parse(part) {
                        Some(class) => {
                            if !classes.contains(&class) {
                                classes.push(class);
                            }
                        }
                        None => config.warnings.push(format!(
                            "compaction_exclude — unknown content class '{part}'; \
                             expected one of: user_text, assistant_text, thinking, \
                             tool_calls, tool_results, file_paths, event_data"
                        )),
                    }
                }
                config.compaction_exclude = classes;
            }
            "context_window" => {
                let parsed = match val {
                    "200k" | "200K" => Some(200_000),
                    "1m" | "1M" => Some(1_000_000),
                    _ => val.parse::<u64>().ok(),
                };
                if parsed.is_none() {
                    config.warnings.push(format!(
                        "context_window = {val} — expected 200k, 1m, or a token count; ignored"
                    ));
                }
                config.context_window = parsed;
            }
            "max_tool_output" => match val.parse::<usize>() {
                Ok(size) => config.max_tool_output = size,
                Err(_) => config.warnings.push(format!(
                    "max_tool_output = {val} — not a number; using {}",
                    config.max_tool_output
                )),
            },
            "bash_timeout" => match val.parse::<u64>() {
                Ok(t) if t >= 1 => config.bash_timeout = t,
                Ok(_) => config.warnings.push(format!(
                    "bash_timeout = {val} — below minimum (1s); using {}",
                    config.bash_timeout
                )),
                Err(_) => config.warnings.push(format!(
                    "bash_timeout = {val} — not a number; using {}",
                    config.bash_timeout
                )),
            },
            "bash_max_timeout" => {
                if let Ok(timeout) = val.parse::<u64>() {
                    config.bash_max_timeout = timeout;
                }
            }
            "subagent_timeout" => {
                if let Ok(timeout) = val.parse::<u64>() {
                    config.subagent_timeout = timeout;
                }
            }
            "api_retries" => {
                if let Ok(retries) = val.parse::<u32>() {
                    config.api_retries = retries;
                }
            }
            "refusal_retries" => {
                if let Ok(retries) = val.parse::<u32>() {
                    config.refusal_retries = retries;
                }
            }
            "telemetry" => config.telemetry = val.to_string(),
            "cache_diagnostics" => {
                config.cache_diagnostics = matches!(val, "true" | "1" | "on" | "yes");
            }
            "cache_ttl" => match CacheTtl::parse(val) {
                Some(ttl) => config.cache_ttl = ttl,
                None => config.warnings.push(format!(
                    "cache_ttl = {val} — expected 5m, 1h, or hybrid; using 5m"
                )),
            },
            "max_fps" => match val.parse::<u32>() {
                Ok(fps) if (1..=1000).contains(&fps) => config.max_fps = fps,
                Ok(_) => config.warnings.push(format!(
                    "max_fps = {val} — expected 1–1000; using {}",
                    config.max_fps
                )),
                Err(_) => config.warnings.push(format!(
                    "max_fps = {val} — not a number; using {}",
                    config.max_fps
                )),
            },
            "scroll_lines" => match val.parse::<u16>() {
                Ok(n) if (1..=20).contains(&n) => config.scroll_lines = Some(n),
                Ok(_) => config
                    .warnings
                    .push(format!("scroll_lines = {val} — expected 1–20; ignoring")),
                Err(_) => config
                    .warnings
                    .push(format!("scroll_lines = {val} — not a number; ignoring")),
            },
            "theme" => config.theme = Some(val.to_string()),
            "agent_name" => config.agent_name = Some(val.to_string()),
            "identity" => config.identity = Some(val.to_string()),
            "disabled_plugins" => {
                config.disabled_plugins = parse_comma_list(val);
            }
            "favorite_models" => {
                config.favorite_models = parse_comma_list(val);
            }
            "disabled_skills" => {
                config.disabled_skills = parse_comma_list(val);
            }
            "progressive_tool_disclosure" => {
                config.progressive_tool_disclosure = matches!(val, "true" | "1" | "on" | "yes");
            }
            "session_persistence" => {
                match crate::core::session_journal::SessionPersistence::parse(val) {
                    Some(mode) => config.session_persistence = mode,
                    None => config.warnings.push(format!(
                        "session_persistence = {val} — expected json or journal; \
                         keeping the default (json)"
                    )),
                }
            }
            "disabled_tools" => {
                config.disabled_tools = parse_comma_list(val);
            }
            _ => {
                // Handle namespaced keys
                if key.starts_with("shell.") {
                    parse_shell_config_key(&mut config.shell, key, val);
                } else if key.starts_with("server.") {
                    parse_server_config_key(&mut config.server, key, val);
                } else if key.starts_with("bridge.") {
                    parse_bridge_config_key(&mut config.bridge, key, val);
                } else if key.starts_with("auth.") {
                    parse_auth_config_key(&mut config.auth, key, val);
                } else if key.starts_with("events.") {
                    parse_events_config_key(&mut config.events, key, val);
                } else if key.starts_with("turn_budget.") {
                    parse_turn_budget_config_key(&mut config.turn_budgets, key, val);
                } else if key.starts_with("memory.") {
                    parse_memory_config_key(&mut config.memory, key, val);
                } else if let Some(provider_key) = key.strip_prefix("provider.") {
                    config
                        .provider_keys
                        .insert(provider_key.to_string(), val.to_string());
                } else if let Some(keybind_key) = key.strip_prefix("keybind.") {
                    config
                        .keybinds
                        .insert(keybind_key.to_string(), val.to_string());
                } else if key.contains('.') {
                    // Dotted keys are namespaced (plugin/extension config, e.g.
                    // `knowledge.jawz_notes`). Plugins define their own keys —
                    // not ours to police. Silently preserved.
                } else {
                    // Unknown top-level key — warn with a did-you-mean if close.
                    match did_you_mean(key) {
                        Some(suggestion) => config.warnings.push(format!(
                            "unknown key '{key}' (did you mean '{suggestion}'?)"
                        )),
                        None => config
                            .warnings
                            .push(format!("unknown key '{key}' — ignored")),
                    }
                }
            }
        }
    }

    // Fail-closed operator-consent rule (Task A2, spec §12): a non-"off"
    // `memory.default_mode` takes effect only when the operator also set
    // `memory.default_mode_confirmed = true` in this same parse pass.
    // Checked after the loop so key order never matters.
    if config.memory.default_mode != "off" && !config.memory.default_mode_confirmed {
        config.warnings.push(format!(
            "memory.default_mode = {} requires memory.default_mode_confirmed = true \
             in the same config; reverting to off",
            config.memory.default_mode
        ));
        config.memory.default_mode = "off".to_string();
    }

    // Derive max_message_size from context_window if not explicitly set.
    // Rough estimate: 1 token ≈ 4 bytes. Context window in tokens → bytes.
    if config.server.max_message_size.is_none() {
        if let Some(ctx_tokens) = config.context_window {
            config.server.max_message_size = Some((ctx_tokens as usize) * 4);
        }
    }
}

/// Read a single config value by exact key from the active config file.
pub fn read_config_value(key: &str) -> Option<String> {
    let path = resolve_read_path("config");
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key.trim() {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Write a single `key = value` pair to `~/.synaps-cli/config` (or profile config).
/// Replaces the first existing line that matches the key, or appends if absent.
/// Preserves comments and unknown keys. Writes atomically via temp file + rename.
pub fn write_config_value(key: &str, value: &str) -> std::io::Result<()> {
    let path = resolve_write_path("config");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let key_trimmed = key.trim();
    let replacement = format!("{} = {}", key_trimmed, value);

    let mut found = false;
    let mut new_lines: Vec<String> = existing
        .lines()
        .map(|line| {
            if found {
                return line.to_string();
            }
            let t = line.trim_start();
            if t.starts_with('#') || t.is_empty() {
                return line.to_string();
            }
            if let Some((k, _)) = t.split_once('=') {
                if k.trim() == key_trimmed {
                    found = true;
                    return replacement.clone();
                }
            }
            line.to_string()
        })
        .collect();

    if !found {
        new_lines.push(replacement);
    }

    let mut out = new_lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    // Config may contain API keys — restrict to owner-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Add a favorite model id (`provider/model`) to config, preserving sort/dedup.
pub fn add_favorite_model(id: &str) -> std::io::Result<()> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let mut values = load_config().favorite_models;
    if !values.iter().any(|v| v == trimmed) {
        values.push(trimmed.to_string());
        values.sort();
    }
    write_comma_list("favorite_models", &values)
}

/// Remove a favorite model id (`provider/model`) from config.
pub fn remove_favorite_model(id: &str) -> std::io::Result<()> {
    let mut values = load_config().favorite_models;
    values.retain(|v| v != id.trim());
    write_comma_list("favorite_models", &values)
}

/// Return whether a model id is marked as favorite.
pub fn is_favorite_model(id: &str) -> bool {
    load_config().favorite_models.iter().any(|v| v == id.trim())
}

/// Resolve the system prompt from CLI flag, config file, or default.
/// Priority: explicit value > ~/.synaps-cli/system.md > built-in default.
pub fn resolve_system_prompt(explicit: Option<&str>) -> String {
    const DEFAULT_PROMPT: &str = "You are a helpful AI agent running in a terminal. \
        You have access to bash, read, and write tools. \
        Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands.";

    if let Some(val) = explicit {
        let path = std::path::Path::new(val);
        if path.exists() && path.is_file() {
            return std::fs::read_to_string(path).unwrap_or_else(|_| val.to_string());
        }
        return val.to_string();
    }

    let system_path = resolve_read_path("system.md");
    if system_path.exists() {
        return std::fs::read_to_string(&system_path).unwrap_or_default();
    }

    DEFAULT_PROMPT.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn turn_budget_keys_parse_typed_per_role() {
        let config = super::load_config_from_str(
            "turn_budget.worker.max_provider_rounds = 3\n\
             turn_budget.worker.max_cost_usd = 0.25\n\
             turn_budget.autonomous.max_elapsed_secs = 90\n\
             turn_budget.foreground.max_tool_calls = 7\n\
             turn_budget.worker.max_provider_rounds_bogus = 1\n\
             turn_budget.nosuchrole.max_tool_calls = 1\n",
        );
        assert_eq!(config.turn_budgets.worker.max_provider_rounds, Some(3));
        assert_eq!(config.turn_budgets.worker.max_cost_usd, Some(0.25));
        assert_eq!(config.turn_budgets.autonomous.max_elapsed_secs, Some(90));
        assert_eq!(config.turn_budgets.foreground.max_tool_calls, Some(7));
        // Unknown fields/roles warn and change nothing.
        assert_eq!(config.turn_budgets.worker.max_tool_calls, None);
        // Invalid values keep the default (None).
        let bad = super::load_config_from_str("turn_budget.worker.max_provider_rounds = nope\n");
        assert_eq!(bad.turn_budgets.worker.max_provider_rounds, None);
    }

    #[test]
    fn memory_keys_all_parse_with_consent() {
        let config = super::load_config_from_str(
            "memory.default_mode = capture_and_recall\n\
             memory.default_mode_confirmed = true\n\
             memory.recall_max_records = 12\n\
             memory.recall_max_tokens = 2048\n\
             memory.recall_timeout_ms = 250\n\
             memory.capture_tools = off\n\
             memory.capture_assistant = false\n\
             memory.capture_user = false\n\
             memory.auto_consolidate = on\n\
             memory.local_embeddings = on\n\
             memory.gliner = on\n",
        );
        assert_eq!(config.memory.default_mode, "capture_and_recall");
        assert!(config.memory.default_mode_confirmed);
        assert_eq!(config.memory.recall_max_records, 12);
        assert_eq!(config.memory.recall_max_tokens, 2048);
        assert_eq!(config.memory.recall_timeout_ms, 250);
        assert_eq!(config.memory.capture_tools, "off");
        assert!(!config.memory.capture_assistant);
        assert!(!config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "on");
        assert_eq!(config.memory.local_embeddings, "on");
        assert_eq!(config.memory.gliner, "on");
    }

    #[test]
    fn memory_defaults_match_spec_12() {
        let config = super::load_config_from_str("");
        assert_eq!(config.memory, super::MemoryConfig::default());
        assert_eq!(config.memory.default_mode, "off");
        assert!(!config.memory.default_mode_confirmed);
        assert_eq!(config.memory.recall_max_records, 8);
        assert_eq!(config.memory.recall_max_tokens, 4096);
        assert_eq!(config.memory.recall_timeout_ms, 150);
        assert_eq!(config.memory.capture_tools, "summary_only");
        assert!(config.memory.capture_assistant);
        assert!(config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "off");
        assert_eq!(config.memory.local_embeddings, "off");
        assert_eq!(config.memory.gliner, "off");
    }

    #[test]
    fn memory_unknown_values_fail_closed_to_defaults() {
        let config = super::load_config_from_str(
            "memory.default_mode = recall_everything_forever\n\
             memory.default_mode_confirmed = true\n\
             memory.recall_max_records = lots\n\
             memory.recall_max_tokens = -5\n\
             memory.recall_timeout_ms = fast\n\
             memory.capture_tools = full\n\
             memory.capture_assistant = maybe\n\
             memory.capture_user = sometimes\n\
             memory.auto_consolidate = aggressive\n\
             memory.local_embeddings = auto\n\
             memory.gliner = download\n\
             memory.nosuchfield = 1\n",
        );
        // Unrecognised mode string must stay "off" — never a non-off mode.
        assert_eq!(config.memory.default_mode, "off");
        assert_eq!(config.memory.recall_max_records, 8);
        assert_eq!(config.memory.recall_max_tokens, 4096);
        assert_eq!(config.memory.recall_timeout_ms, 150);
        assert_eq!(config.memory.capture_tools, "summary_only");
        assert!(config.memory.capture_assistant);
        assert!(config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "off");
        assert_eq!(config.memory.local_embeddings, "off");
        assert_eq!(config.memory.gliner, "off");
    }

    #[test]
    fn memory_default_mode_without_consent_reverts_to_off() {
        let config =
            super::load_config_from_str("memory.default_mode = recall_each_prompt\n");
        assert_eq!(config.memory.default_mode, "off");
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("memory.default_mode_confirmed")),
            "reversion must surface a warning; got: {:?}",
            config.warnings
        );

        // Explicit false consent is not consent.
        let denied = super::load_config_from_str(
            "memory.default_mode = capture_only\n\
             memory.default_mode_confirmed = false\n",
        );
        assert_eq!(denied.memory.default_mode, "off");

        // Garbage consent value fails closed — not consent.
        let garbage = super::load_config_from_str(
            "memory.default_mode = capture_only\n\
             memory.default_mode_confirmed = definitely\n",
        );
        assert_eq!(garbage.memory.default_mode, "off");
    }

    #[test]
    fn memory_consent_is_order_independent() {
        // Consent key before the mode key.
        let before = super::load_config_from_str(
            "memory.default_mode_confirmed = true\n\
             memory.default_mode = recall_once\n",
        );
        assert_eq!(before.memory.default_mode, "recall_once");
        // Consent key after the mode key.
        let after = super::load_config_from_str(
            "memory.default_mode = recall_once\n\
             memory.default_mode_confirmed = true\n",
        );
        assert_eq!(after.memory.default_mode, "recall_once");
    }

    #[test]
    fn memory_config_surface_is_closed_by_construction() {
        // Exhaustive struct literal — no `..Default::default()` spread. This
        // fails to COMPILE if any field is added to or removed from
        // MemoryConfig, pinning the exact memory.* config surface. Secret,
        // retention, and project-scope semantics have no field here, so no
        // `memory.secret*` / `memory.retention*` / `memory.project*` config
        // key can exist (spec §12: those rules are not configurable to fail
        // open). Asserted by construction, not by test disproof.
        let _closed_surface = super::MemoryConfig {
            default_mode: "off".to_string(),
            default_mode_confirmed: false,
            recall_max_records: 8,
            recall_max_tokens: 4096,
            recall_timeout_ms: 150,
            capture_tools: "summary_only".to_string(),
            capture_assistant: true,
            capture_user: true,
            auto_consolidate: "off".to_string(),
            local_embeddings: "off".to_string(),
            gliner: "off".to_string(),
        };
    }

    use super::*;
    use serial_test::serial;

    #[test]
    fn test_levenshtein_basics() {
        assert_eq!(levenshtein("model", "model"), 0);
        assert_eq!(levenshtein("modle", "model"), 2);
        assert_eq!(levenshtein("them", "theme"), 1);
    }

    #[test]
    fn test_did_you_mean_close_typos() {
        assert_eq!(did_you_mean("modle"), Some("model"));
        assert_eq!(did_you_mean("them"), Some("theme"));
        assert_eq!(did_you_mean("thinkng"), Some("thinking"));
        assert_eq!(did_you_mean("completely_unrelated_key"), None);
    }

    #[test]
    #[serial]
    fn test_config_warnings_unknown_key_and_bad_values() {
        let home = std::env::temp_dir().join(format!("synaps-warn-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "modle = claude-opus-4-6\nthinking = hgih\nbash_timeout = 0\nknowledge.jawz_notes = ~/Jawz/notes\ncustom.plugin.key = 42\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            // Dotted (namespaced) keys must NOT warn — plugins own those.
            assert_eq!(config.warnings.len(), 3, "warnings: {:?}", config.warnings);
            assert!(
                !config.warnings.iter().any(|w| w.contains("knowledge")),
                "{:?}",
                config.warnings
            );
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.contains("did you mean 'model'")),
                "{:?}",
                config.warnings
            );
            assert!(
                config.warnings.iter().any(|w| w.contains("thinking")),
                "{:?}",
                config.warnings
            );
            assert!(
                config.warnings.iter().any(|w| w.contains("below minimum")),
                "{:?}",
                config.warnings
            );
            // Bad values fall back to defaults
            assert_eq!(config.bash_timeout, 30);
            assert_eq!(config.thinking_budget, None);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── cache_ttl parse table (spec §3.1) ──

    #[test]
    fn test_cache_ttl_parse_table() {
        // 5m aliases
        assert_eq!(CacheTtl::parse("5m"), Some(CacheTtl::FiveMinutes));
        assert_eq!(CacheTtl::parse("5min"), Some(CacheTtl::FiveMinutes));
        assert_eq!(CacheTtl::parse("default"), Some(CacheTtl::FiveMinutes));
        // 1h aliases
        assert_eq!(CacheTtl::parse("1h"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("60m"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("1hr"), Some(CacheTtl::OneHour));
        // hybrid
        assert_eq!(CacheTtl::parse("hybrid"), Some(CacheTtl::Hybrid));
        // case-insensitive
        assert_eq!(CacheTtl::parse("1H"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("HYBRID"), Some(CacheTtl::Hybrid));
        assert_eq!(CacheTtl::parse("Default"), Some(CacheTtl::FiveMinutes));
        // garbage → None (caller warns + defaults)
        assert_eq!(CacheTtl::parse("2h"), None);
        assert_eq!(CacheTtl::parse(""), None);
        assert_eq!(CacheTtl::parse("forever"), None);
    }

    #[test]
    fn test_cache_ttl_default_is_five_minutes() {
        assert_eq!(CacheTtl::default(), CacheTtl::FiveMinutes);
        assert_eq!(SynapsConfig::default().cache_ttl, CacheTtl::FiveMinutes);
    }

    #[test]
    fn test_max_fps_default_is_60() {
        assert_eq!(SynapsConfig::default().max_fps, 60);
    }

    #[test]
    #[serial]
    fn test_max_fps_config_parse_and_validation() {
        let home = std::env::temp_dir().join(format!("synaps-maxfps-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();

        // Valid high-refresh value parses, no warning.
        std::fs::write(dir.join("config"), "max_fps = 144\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 144);
            assert!(
                config.warnings.is_empty(),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Out-of-range (0) → default 60 + boot warning (never blocks boot).
        std::fs::write(dir.join("config"), "max_fps = 0\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 60);
            assert!(
                config.warnings.iter().any(|w| w.contains("max_fps")),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Non-numeric → default 60 + warning.
        std::fs::write(dir.join("config"), "max_fps = fast\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 60);
            assert!(
                config.warnings.iter().any(|w| w.contains("max_fps")),
                "warnings: {:?}",
                config.warnings
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_cache_ttl_config_parse_and_garbage_warning() {
        let home =
            std::env::temp_dir().join(format!("synaps-cachettl-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();

        // Valid value parses, no warning.
        std::fs::write(dir.join("config"), "cache_ttl = hybrid\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.cache_ttl, CacheTtl::Hybrid);
            assert!(
                config.warnings.is_empty(),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Garbage value → 5m default + boot warning (never blocks boot).
        std::fs::write(dir.join("config"), "cache_ttl = 2h\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.cache_ttl, CacheTtl::FiveMinutes);
            assert!(
                config.warnings.iter().any(|w| w.contains("cache_ttl")),
                "warnings: {:?}",
                config.warnings
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn test_parse_thinking_budget() {
        use crate::core::reasoning::ThinkingSpec;
        // ThinkingSpec::parse replaced parse_thinking_budget; verify budget values.
        assert_eq!(ThinkingSpec::parse("low").unwrap().to_budget(), Some(2048));
        assert_eq!(
            ThinkingSpec::parse("medium").unwrap().to_budget(),
            Some(4096)
        );
        assert_eq!(
            ThinkingSpec::parse("high").unwrap().to_budget(),
            Some(16384)
        );
        assert_eq!(
            ThinkingSpec::parse("xhigh").unwrap().to_budget(),
            Some(32768)
        );
        assert_eq!(ThinkingSpec::parse("8192").unwrap().to_budget(), Some(8192));
        let config = load_config_from_str("thinking = 8192\n");
        assert_eq!(config.thinking_level, None);
        assert_eq!(config.thinking_budget, Some(8192));
        assert_eq!(ThinkingSpec::parse("invalid"), None);
    }

    #[test]
    fn test_base_dir() {
        let path = base_dir();
        assert!(path.to_string_lossy().ends_with(".synaps-cli"));
    }

    #[test]
    fn test_resolve_system_prompt_explicit() {
        let result = resolve_system_prompt(Some("test prompt"));
        assert_eq!(result, "test prompt");
    }

    #[test]
    fn test_resolve_system_prompt_none() {
        let result = resolve_system_prompt(None);
        assert!(result.contains("helpful AI agent"));
    }

    // Note: test_load_config_nonexistent_file removed — HOME env var mutation
    // is not thread-safe and races with shell config tests. Coverage provided
    // by shell::config::tests::test_shell_config_from_file.

    #[test]
    fn test_synaps_config_default() {
        let config = SynapsConfig::default();
        assert_eq!(config.model, None);
        assert_eq!(config.thinking_budget, None);
        assert_eq!(config.max_tool_output, 30000);
        assert_eq!(config.bash_timeout, 30);
        assert_eq!(config.bash_max_timeout, 300);
        assert_eq!(config.subagent_timeout, 300);
        assert_eq!(config.api_retries, 3);
        assert_eq!(config.theme, None);
        assert!(config.disabled_plugins.is_empty());
        assert!(config.favorite_models.is_empty());
        assert!(config.disabled_skills.is_empty());
        assert!(!config.progressive_tool_disclosure);
        assert_eq!(config.shell.max_sessions, 5);
        assert_eq!(config.shell.idle_timeout.as_secs(), 600);
        // Server config defaults
        assert!(config.server.allowed_origins.is_empty());
        assert_eq!(config.server.token, None);
        assert!(!config.server.auto_approve_confirms);
        assert_eq!(config.server.max_message_size, None);
        // Bridge config defaults
        assert!(config.bridge.uds_path.is_none());
        assert!(!config.bridge.heartbeat_mirror);
        assert_eq!(config.bridge.heartbeat_timeout_ms, 250);
    }

    #[test]
    #[serial]
    fn test_load_config_progressive_tool_disclosure_is_opt_in() {
        let home = make_test_home("progressive-tool-disclosure");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "progressive_tool_disclosure = true\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            assert!(config.progressive_tool_disclosure);
            assert!(config.warnings.is_empty());
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_load_config_bridge_keys() {
        let home = make_test_home("bridge-keys");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(
            &cfg,
            "\
bridge.uds_path = /tmp/some/control.sock\n\
bridge.heartbeat_mirror = true\n\
bridge.heartbeat_timeout_ms = 750\n\
",
        )
        .unwrap();

        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.bridge.uds_path,
                Some(std::path::PathBuf::from("/tmp/some/control.sock")),
            );
            assert!(config.bridge.heartbeat_mirror);
            assert_eq!(config.bridge.heartbeat_timeout_ms, 750);
            assert_eq!(
                config.bridge.resolved_uds_path(),
                std::path::PathBuf::from("/tmp/some/control.sock"),
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn test_bridge_config_defaults() {
        let cfg = BridgeConfig::default();
        assert!(cfg.uds_path.is_none());
        assert!(!cfg.heartbeat_mirror);
        assert_eq!(cfg.heartbeat_timeout_ms, 250);
        // resolved path falls under base_dir()/bridge/control.sock
        let resolved = cfg.resolved_uds_path();
        assert!(resolved.ends_with("bridge/control.sock"));
    }

    #[test]
    #[serial]
    fn test_bridge_heartbeat_mirror_defaults_off_when_unset() {
        let home = make_test_home("bridge-default-off");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-sonnet-4-6\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            assert!(!config.bridge.heartbeat_mirror);
            assert!(config.bridge.uds_path.is_none());
            assert_eq!(config.bridge.heartbeat_timeout_ms, 250);
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn compaction_disclosure_keys_parse_with_typed_warnings() {
        let config = load_config_from_str(
            "compaction_mode = local\ncompaction_exclude = thinking, tool_results, bogus\n",
        );
        assert_eq!(
            config.compaction_mode,
            crate::core::compaction::CompactionMode::LocalOnly
        );
        assert_eq!(
            config.compaction_exclude,
            vec![
                crate::core::compaction::ContentClass::Thinking,
                crate::core::compaction::ContentClass::ToolResults,
            ]
        );
        assert!(
            config.warnings.iter().any(|w| w.contains("bogus")),
            "unknown class must warn: {:?}",
            config.warnings
        );

        let defaults = load_config_from_str("");
        assert_eq!(
            defaults.compaction_mode,
            crate::core::compaction::CompactionMode::Remote
        );
        assert!(defaults.compaction_exclude.is_empty());

        let bad_mode = load_config_from_str("compaction_mode = cloud\n");
        assert_eq!(
            bad_mode.compaction_mode,
            crate::core::compaction::CompactionMode::Remote
        );
        assert!(bad_mode
            .warnings
            .iter()
            .any(|w| w.contains("compaction_mode")));
    }

    #[test]
    #[serial]
    fn test_load_config_server_keys() {
        let home = make_test_home("server-keys");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(
            &cfg,
            "\
server.allowed_origins = http://localhost:3000, http://localhost:5193\n\
server.token = my-secret-token\n\
server.auto_approve_confirms = true\n\
server.max_message_size = 65536\n\
context_window = 200k\n\
",
        )
        .unwrap();

        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.server.allowed_origins,
                vec![
                    "http://localhost:3000".to_string(),
                    "http://localhost:5193".to_string(),
                ]
            );
            assert_eq!(config.server.token, Some("my-secret-token".to_string()));
            assert!(config.server.auto_approve_confirms);
            // Explicit max_message_size takes precedence over context_window derivation
            assert_eq!(config.server.max_message_size, Some(65536));
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_server_max_message_size_derived_from_context_window() {
        let home = make_test_home("server-derive");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "context_window = 200k\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            // 200_000 tokens * 4 bytes/token = 800_000 bytes
            assert_eq!(config.server.max_message_size, Some(800_000));
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    fn make_test_home(subdir: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/synaps-write-test-{}", subdir));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".synaps-cli")).unwrap();
        dir
    }

    fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", home);
        f();
        if let Some(h) = original {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    #[serial]
    fn write_config_value_replaces_existing_key() {
        let home = make_test_home("replace");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\nthinking = low\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        assert!(contents.contains("thinking = low"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_appends_when_missing() {
        let home = make_test_home("append");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\n").unwrap();

        with_home(&home, || {
            write_config_value("theme", "dracula").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-opus-4-6"));
        assert!(contents.contains("theme = dracula"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_preserves_comments() {
        let home = make_test_home("comments");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "# user comment\nmodel = claude-opus-4-6\n# another\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("# user comment"));
        assert!(contents.contains("# another"));
        assert!(contents.contains("model = claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_preserves_unknown_keys() {
        let home = make_test_home("unknown");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "custom_thing = 42\nmodel = claude-opus-4-6\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("custom_thing = 42"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_creates_file_if_absent() {
        let home = make_test_home("create");
        let cfg = home.join(".synaps-cli/config");
        assert!(!cfg.exists());

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn load_config_parses_theme_key() {
        let dir = std::path::PathBuf::from("/tmp/synaps-config-test-theme/.synaps-cli");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("config"), "theme = dracula\n").unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-theme");

        let config = load_config();

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-theme");

        assert_eq!(config.theme.as_deref(), Some("dracula"));
    }

    #[test]
    #[serial]
    fn test_load_config_disable_lists() {
        let test_dir =
            std::path::PathBuf::from("/tmp/synaps-config-test-disable-lists/.synaps-cli");
        let _ = std::fs::create_dir_all(&test_dir);
        let config_path = test_dir.join("config");

        let config_content = r#"
# Test config with disable lists
favorite_models = claude/claude-opus-4-7, groq/llama-3.3-70b-versatile

disabled_plugins = foo, bar
disabled_skills = baz, plug:qual
"#;
        std::fs::write(&config_path, config_content).unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-disable-lists");

        let config = load_config();

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }

        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-disable-lists");

        assert_eq!(
            config.disabled_plugins,
            vec!["foo".to_string(), "bar".to_string()]
        );
        assert_eq!(
            config.favorite_models,
            vec![
                "claude/claude-opus-4-7".to_string(),
                "groq/llama-3.3-70b-versatile".to_string(),
            ]
        );
        assert_eq!(
            config.disabled_skills,
            vec!["baz".to_string(), "plug:qual".to_string()]
        );
    }

    #[test]
    #[serial]
    fn favorite_model_helpers_round_trip_through_config_file() {
        let home = make_test_home("favorite-models");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-7\n").unwrap();

        with_home(&home, || {
            add_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            add_favorite_model("claude/claude-opus-4-7").unwrap();
            add_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            assert!(is_favorite_model("groq/llama-3.3-70b-versatile"));
            remove_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            assert!(!is_favorite_model("groq/llama-3.3-70b-versatile"));
            assert!(is_favorite_model("claude/claude-opus-4-7"));
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-opus-4-7"));
        assert!(contents.contains("favorite_models = claude/claude-opus-4-7"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_load_config_new_keys() {
        // Create a temporary config directory with the new keys
        let test_dir = std::path::PathBuf::from("/tmp/synaps-config-test-new-keys/.synaps-cli");
        let _ = std::fs::create_dir_all(&test_dir);
        let config_path = test_dir.join("config");

        let config_content = r#"
# Test config with new keys
model = claude-haiku
thinking = medium
max_tool_output = 50000
bash_timeout = 45
bash_max_timeout = 600
subagent_timeout = 120
api_retries = 5
"#;
        std::fs::write(&config_path, config_content).unwrap();

        // Temporarily override the config path for this test
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-new-keys");

        let config = load_config();

        // Restore original HOME
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }

        // Cleanup
        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-new-keys");

        assert_eq!(config.model, Some("claude-haiku".to_string()));
        assert_eq!(config.thinking_budget, Some(4096)); // medium = 4096
        assert_eq!(config.max_tool_output, 50000);
        assert_eq!(config.bash_timeout, 45);
        assert_eq!(config.bash_max_timeout, 600);
        assert_eq!(config.subagent_timeout, 120);
        assert_eq!(config.api_retries, 5);
    }

    // ── auth.* config + credential source (#157) ──

    // ── events.auto_turn parser ──────────────────────────────────────────────

    #[test]
    fn events_auto_turn_explicit_true_values() {
        for val in &["true", "TRUE", "1", "yes", "YES", "on", "ON"] {
            let cfg = load_config_from_str(&format!("events.auto_turn = {val}"));
            assert!(
                cfg.events.auto_turn,
                "expected true for events.auto_turn = {val}"
            );
        }
    }

    #[test]
    fn events_auto_turn_explicit_false_values() {
        for val in &["false", "FALSE", "0", "no", "NO", "off", "OFF"] {
            let cfg = load_config_from_str(&format!("events.auto_turn = {val}"));
            assert!(
                !cfg.events.auto_turn,
                "expected false for events.auto_turn = {val}"
            );
        }
    }

    #[test]
    fn events_auto_turn_typo_fails_safe_false() {
        // Unrecognised value should fail safe to false (with a warning on stderr).
        let cfg = load_config_from_str("events.auto_turn = fales");
        assert!(
            !cfg.events.auto_turn,
            "typo 'fales' must fail safe to false"
        );
    }

    #[test]
    fn events_auto_turn_default_is_true() {
        let cfg = load_config_from_str("model = claude-haiku");
        assert!(cfg.events.auto_turn, "default must be true when key absent");
    }

    #[test]
    #[serial]
    fn test_auth_config_parses_endpoint_and_token() {
        let home = std::env::temp_dir().join(format!("synaps-auth-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config"),
            "auth.remote_endpoint = https://jade.jade:8181\nauth.machine_token = machine-abc\n",
        )
        .unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.auth.remote_endpoint.as_deref(),
                Some("https://jade.jade:8181")
            );
            assert_eq!(config.auth.machine_token.as_deref(), Some("machine-abc"));
            // auth.* are namespaced -> must NOT warn as unknown keys
            assert!(
                !config.warnings.iter().any(|w| w.contains("auth.")),
                "{:?}",
                config.warnings
            );
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_credential_source_local_by_default() {
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        assert!(!AuthConfig::default().credential_source().is_remote());
    }

    #[test]
    #[serial]
    fn test_credential_source_remote_from_config() {
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        let auth = AuthConfig {
            remote_endpoint: Some("https://b".into()),
            machine_token: Some("m".into()),
        };
        assert_eq!(
            auth.credential_source(),
            crate::core::auth::CredentialSource::Remote {
                endpoint: "https://b".into(),
                machine_token: "m".into()
            }
        );
    }

    #[test]
    #[serial]
    fn test_credential_source_env_overrides_config() {
        let auth = AuthConfig {
            remote_endpoint: Some("https://config-host".into()),
            machine_token: Some("config-tok".into()),
        };
        std::env::set_var("SYNAPS_AUTH_ENDPOINT", "https://env-host");
        std::env::set_var("SYNAPS_MACHINE_TOKEN", "env-tok");
        let src = auth.credential_source();
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        assert_eq!(
            src,
            crate::core::auth::CredentialSource::Remote {
                endpoint: "https://env-host".into(),
                machine_token: "env-tok".into()
            }
        );
    }
    #[test]
    fn config_parses_ultracode_as_distinct_canonical_level() {
        let cfg = load_config_from_str("thinking = ultracode\n");
        assert_eq!(
            cfg.thinking_level,
            Some(crate::reasoning::ReasoningLevel::UltraCode)
        );
        assert_eq!(cfg.thinking_budget, None);
        for other in [
            crate::reasoning::ReasoningLevel::Ultra,
            crate::reasoning::ReasoningLevel::Max,
            crate::reasoning::ReasoningLevel::XHigh,
        ] {
            assert_ne!(cfg.thinking_level, Some(other));
        }
    }
}
