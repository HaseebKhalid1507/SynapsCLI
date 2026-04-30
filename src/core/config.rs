use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use crate::tools::shell::config::ShellConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub mode: String,
    pub stt_backend: String,
    pub stt_model_path: PathBuf,
    pub stt_language: String,
    pub stt_show_partials: bool,
    pub stt_auto_submit: bool,
    pub stt_silence_submit_ms: u64,
    pub stt_vad_rms_threshold: f32,
    pub stt_min_speech_ms: u64,
    pub stt_preroll_ms: u64,
    pub stt_max_utterance_ms: u64,
    pub max_transcript_chars: usize,
    pub barge_in_cancel_generation: bool,
    pub commands_enabled: bool,
    pub commands_submit_enabled: bool,
    pub provider: String,
    pub sidecar_command: String,
    pub sidecar_args: Vec<String>,
    pub sidecar_restart_on_crash: bool,
    pub sidecar_protocol_version: u16,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "toggle".to_string(),
            stt_backend: "whisper-rs".to_string(),
            stt_model_path: PathBuf::from("~/.synaps-cli/models/whisper/ggml-base.en.bin"),
            stt_language: "en".to_string(),
            stt_show_partials: true,
            stt_auto_submit: false,
            stt_silence_submit_ms: 1000,
            stt_vad_rms_threshold: 0.01,
            stt_min_speech_ms: 250,
            stt_preroll_ms: 300,
            stt_max_utterance_ms: 30_000,
            max_transcript_chars: 16_000,
            barge_in_cancel_generation: false,
            commands_enabled: true,
            commands_submit_enabled: false,
            provider: "sidecar".to_string(),
            sidecar_command: "synaps-voice-local".to_string(),
            sidecar_args: Vec::new(),
            sidecar_restart_on_crash: false,
            sidecar_protocol_version: 1,
        }
    }
}

static PROFILE_NAME: OnceLock<Option<String>> = OnceLock::new();
static PROVIDER_KEYS: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// Provider API keys parsed from `provider.<name> = ...` lines in config.
/// Empty if `load_config()` hasn't been called. The registry falls back to
/// env vars, so e.g. `GROQ_API_KEY` works even with an empty map.
pub fn get_provider_keys() -> BTreeMap<String, String> {
    PROVIDER_KEYS.get().cloned().unwrap_or_default()
}

/// Returns the active profile name, if any.
/// Reads from `SYNAPS_PROFILE` environment variable if not already set programmatically.
pub fn get_profile() -> Option<String> {
    PROFILE_NAME.get_or_init(|| std::env::var("SYNAPS_PROFILE").ok()).clone()
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

/// Parsed configuration from the config file.
#[derive(Debug, Clone)]
pub struct SynapsConfig {
    pub model: Option<String>,
    pub thinking_budget: Option<u32>,
    pub context_window: Option<u64>,   // override auto-detected context window (tokens)
    pub compaction_model: Option<String>, // model used for /compact (default: claude-sonnet-4-6)
    pub max_tool_output: usize,        // default 30000
    pub bash_timeout: u64,             // default 30
    pub bash_max_timeout: u64,         // default 300
    pub subagent_timeout: u64,         // default 300
    pub api_retries: u32,              // default 3
    pub theme: Option<String>,
    pub disabled_plugins: Vec<String>,
    pub favorite_models: Vec<String>,
    pub disabled_skills: Vec<String>,
    pub shell: ShellConfig,
    pub voice: VoiceConfig,
    pub provider_keys: BTreeMap<String, String>,
    pub keybinds: std::collections::HashMap<String, String>,
}

impl Default for SynapsConfig {
    fn default() -> Self {
        Self {
            model: None,
            thinking_budget: None,
            context_window: None,
            compaction_model: None,
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            api_retries: 3,
            theme: None,
            disabled_plugins: Vec::new(),
            favorite_models: Vec::new(),
            disabled_skills: Vec::new(),
            shell: ShellConfig::default(),
            voice: VoiceConfig::default(),
            provider_keys: BTreeMap::new(),
            keybinds: std::collections::HashMap::new(),
        }
    }
}


fn parse_thinking_budget(val: &str) -> Option<u32> {
    match val {
        "low" => Some(2048),
        "medium" => Some(4096),
        "high" => Some(16384),
        "xhigh" => Some(32768),
        "adaptive" => Some(0), // sentinel: model decides depth
        _ => val.parse::<u32>().ok(),
    }
}

fn parse_comma_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn write_comma_list(key: &str, values: &[String]) -> std::io::Result<()> {
    write_config_value(key, &values.join(", "))
}

fn parse_bool(val: &str) -> Option<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn parse_voice_config_key(voice_config: &mut VoiceConfig, key: &str, val: &str) {
    match key {
        "voice.enabled" => {
            if let Some(enabled) = parse_bool(val) {
                voice_config.enabled = enabled;
            } else {
                eprintln!("Warning: invalid value for voice.enabled: '{}', using default", val);
            }
        }
        "voice.mode" => {
            if matches!(val, "push_to_talk" | "toggle" | "conversation") {
                voice_config.mode = val.to_string();
            } else {
                eprintln!("Warning: invalid value for voice.mode: '{}', using default", val);
            }
        }
        "voice.stt_backend" => {
            if val == "whisper-rs" {
                voice_config.stt_backend = val.to_string();
            } else {
                eprintln!("Warning: invalid value for voice.stt_backend: '{}', using default", val);
            }
        }
        "voice.stt_model_path" => voice_config.stt_model_path = PathBuf::from(val),
        "voice.stt_language" => voice_config.stt_language = val.to_string(),
        "voice.stt_show_partials" => {
            if let Some(show_partials) = parse_bool(val) {
                voice_config.stt_show_partials = show_partials;
            } else {
                eprintln!("Warning: invalid value for voice.stt_show_partials: '{}', using default", val);
            }
        }
        "voice.stt_auto_submit" => {
            if let Some(auto_submit) = parse_bool(val) {
                voice_config.stt_auto_submit = auto_submit;
            } else {
                eprintln!("Warning: invalid value for voice.stt_auto_submit: '{}', using default", val);
            }
        }
        "voice.stt_silence_submit_ms" => {
            if let Ok(silence_ms) = val.parse::<u64>() {
                voice_config.stt_silence_submit_ms = silence_ms;
            } else {
                eprintln!("Warning: invalid value for voice.stt_silence_submit_ms: '{}', using default", val);
            }
        }
        "voice.stt_vad_rms_threshold" => {
            if let Ok(threshold) = val.parse::<f32>() {
                voice_config.stt_vad_rms_threshold = threshold;
            } else {
                eprintln!("Warning: invalid value for voice.stt_vad_rms_threshold: '{}', using default", val);
            }
        }
        "voice.stt_min_speech_ms" => {
            if let Ok(min_speech_ms) = val.parse::<u64>() {
                voice_config.stt_min_speech_ms = min_speech_ms;
            } else {
                eprintln!("Warning: invalid value for voice.stt_min_speech_ms: '{}', using default", val);
            }
        }
        "voice.stt_preroll_ms" => {
            if let Ok(preroll_ms) = val.parse::<u64>() {
                voice_config.stt_preroll_ms = preroll_ms;
            } else {
                eprintln!("Warning: invalid value for voice.stt_preroll_ms: '{}', using default", val);
            }
        }
        "voice.stt_max_utterance_ms" => {
            if let Ok(max_utterance_ms) = val.parse::<u64>() {
                voice_config.stt_max_utterance_ms = max_utterance_ms;
            } else {
                eprintln!("Warning: invalid value for voice.stt_max_utterance_ms: '{}', using default", val);
            }
        }
        "voice.max_transcript_chars" => {
            if let Ok(max_chars) = val.parse::<usize>() {
                voice_config.max_transcript_chars = max_chars;
            } else {
                eprintln!("Warning: invalid value for voice.max_transcript_chars: '{}', using default", val);
            }
        }
        "voice.barge_in.cancel_generation" => {
            if let Some(cancel_generation) = parse_bool(val) {
                voice_config.barge_in_cancel_generation = cancel_generation;
            } else {
                eprintln!("Warning: invalid value for voice.barge_in.cancel_generation: '{}', using default", val);
            }
        }
        "voice.commands_enabled" => {
            if let Some(enabled) = parse_bool(val) {
                voice_config.commands_enabled = enabled;
            } else {
                eprintln!("Warning: invalid value for voice.commands_enabled: '{}', using default", val);
            }
        }
        "voice.commands.submit_enabled" => {
            if let Some(enabled) = parse_bool(val) {
                voice_config.commands_submit_enabled = enabled;
            } else {
                eprintln!("Warning: invalid value for voice.commands.submit_enabled: '{}', using default", val);
            }
        }
        "voice.provider" => {
            if matches!(val, "disabled" | "builtin" | "sidecar") {
                voice_config.provider = val.to_string();
            } else {
                eprintln!("Warning: invalid value for voice.provider: '{}', using default", val);
            }
        }
        "voice.sidecar.command" => voice_config.sidecar_command = val.to_string(),
        "voice.sidecar.args" => voice_config.sidecar_args = parse_comma_list(val),
        "voice.sidecar.restart_on_crash" => {
            if let Some(restart) = parse_bool(val) {
                voice_config.sidecar_restart_on_crash = restart;
            } else {
                eprintln!("Warning: invalid value for voice.sidecar.restart_on_crash: '{}', using default", val);
            }
        }
        "voice.sidecar.protocol_version" => {
            if let Ok(version) = val.parse::<u16>() {
                voice_config.sidecar_protocol_version = version;
            } else {
                eprintln!("Warning: invalid value for voice.sidecar.protocol_version: '{}', using default", val);
            }
        }
        _ => {
            // Unknown voice.* keys are preserved (not rejected)
        }
    }
}

/// Parse shell.* configuration keys and update the ShellConfig.
fn parse_shell_config_key(shell_config: &mut ShellConfig, key: &str, val: &str) {
    match key {
        "shell.max_sessions" => {
            if let Ok(sessions) = val.parse::<usize>() {
                shell_config.max_sessions = sessions;
            } else {
                eprintln!("Warning: invalid value for shell.max_sessions: '{}', using default", val);
            }
        }
        "shell.idle_timeout" => {
            if let Ok(timeout) = val.parse::<u64>() {
                shell_config.idle_timeout = std::time::Duration::from_secs(timeout);
            } else {
                eprintln!("Warning: invalid value for shell.idle_timeout: '{}', using default", val);
            }
        }
        "shell.readiness_timeout_ms" => {
            if let Ok(timeout) = val.parse::<u64>() {
                shell_config.readiness_timeout_ms = timeout;
            } else {
                eprintln!("Warning: invalid value for shell.readiness_timeout_ms: '{}', using default", val);
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
                eprintln!("Warning: invalid value for shell.default_rows: '{}', using default", val);
            }
        }
        "shell.default_cols" => {
            if let Ok(cols) = val.parse::<u16>() {
                shell_config.default_cols = cols;
            } else {
                eprintln!("Warning: invalid value for shell.default_cols: '{}', using default", val);
            }
        }
        "shell.readiness_strategy" => {
            let val_lower = val.to_lowercase();
            match val_lower.as_str() {
                "timeout" | "prompt" | "hybrid" => {
                    shell_config.readiness_strategy = val.to_string();
                }
                _ => {
                    eprintln!("Warning: invalid value for shell.readiness_strategy: '{}', using default", val);
                }
            }
        }
        "shell.max_output" => {
            if let Ok(max_output) = val.parse::<usize>() {
                shell_config.max_output = max_output;
            } else {
                eprintln!("Warning: invalid value for shell.max_output: '{}', using default", val);
            }
        }
        _ => {
            // Unknown shell.* keys are preserved (not rejected)
        }
    }
}

fn parse_config_content(content: &str) -> SynapsConfig {
    let mut config = SynapsConfig::default();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let Some((key, val)) = line.split_once('=') else { continue };
        let key = key.trim();
        let val = val.trim();
        match key {
            "model" => config.model = Some(val.to_string()),
            "thinking" => config.thinking_budget = parse_thinking_budget(val),
            "compaction_model" => config.compaction_model = Some(val.to_string()),
            "context_window" => {
                let parsed = match val {
                    "200k" | "200K" => Some(200_000),
                    "1m" | "1M" => Some(1_000_000),
                    _ => val.parse::<u64>().ok(),
                };
                config.context_window = parsed;
            }
            "max_tool_output" => {
                if let Ok(size) = val.parse::<usize>() {
                    config.max_tool_output = size;
                }
            }
            "bash_timeout" => {
                if let Ok(timeout) = val.parse::<u64>() {
                    config.bash_timeout = timeout;
                }
            }
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
            "theme" => config.theme = Some(val.to_string()),
            "disabled_plugins" => {
                config.disabled_plugins = parse_comma_list(val);
            }
            "favorite_models" => {
                config.favorite_models = parse_comma_list(val);
            }
            "disabled_skills" => {
                config.disabled_skills = parse_comma_list(val);
            }
            _ => {
                // Handle shell.* keys
                if key.starts_with("shell.") {
                    parse_shell_config_key(&mut config.shell, key, val);
                } else if key.starts_with("voice.") {
                    parse_voice_config_key(&mut config.voice, key, val);
                } else if let Some(provider_key) = key.strip_prefix("provider.") {
                    config.provider_keys.insert(provider_key.to_string(), val.to_string());
                } else if let Some(keybind_key) = key.strip_prefix("keybind.") {
                    config.keybinds.insert(keybind_key.to_string(), val.to_string());
                }
                // Other unknown keys silently ignored
            }
        }
    }

    config
}

/// Parse the config file at ~/.synaps-cli/config (or profile variant).
/// Returns default config if file doesn't exist or can't be read.
pub fn load_config() -> SynapsConfig {
    let path = resolve_read_path("config");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return SynapsConfig::default();
    };

    let config = parse_config_content(&content);

    // Publish provider keys to the process-wide cache for the API router.
    // First writer wins (OnceLock) — subsequent load_config calls are no-ops.
    let _ = PROVIDER_KEYS.set(config.provider_keys.clone());

    config
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
    let mut new_lines: Vec<String> = existing.lines().map(|line| {
        if found { return line.to_string(); }
        let t = line.trim_start();
        if t.starts_with('#') || t.is_empty() { return line.to_string(); }
        if let Some((k, _)) = t.split_once('=') {
            if k.trim() == key_trimmed {
                found = true;
                return replacement.clone();
            }
        }
        line.to_string()
    }).collect();

    if !found {
        new_lines.push(replacement);
    }

    let mut out = new_lines.join("\n");
    if !out.ends_with('\n') { out.push('\n'); }

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
    use super::*;

    #[test]
    fn test_parse_thinking_budget() {
        assert_eq!(parse_thinking_budget("low"), Some(2048));
        assert_eq!(parse_thinking_budget("medium"), Some(4096));
        assert_eq!(parse_thinking_budget("high"), Some(16384));
        assert_eq!(parse_thinking_budget("xhigh"), Some(32768));
        assert_eq!(parse_thinking_budget("8192"), Some(8192));
        assert_eq!(parse_thinking_budget("invalid"), None);
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
        assert_eq!(config.shell.max_sessions, 5);
        assert_eq!(config.shell.idle_timeout.as_secs(), 600);
        assert!(config.voice.enabled);
        assert_eq!(config.voice.mode, "toggle");
        assert_eq!(config.voice.stt_backend, "whisper-rs");
        assert_eq!(config.voice.stt_model_path, PathBuf::from("~/.synaps-cli/models/whisper/ggml-base.en.bin"));
        assert_eq!(config.voice.stt_language, "en");
        assert!(config.voice.stt_show_partials);
        assert!(!config.voice.stt_auto_submit);
        assert_eq!(config.voice.stt_silence_submit_ms, 1000);
        assert_eq!(config.voice.stt_vad_rms_threshold, 0.01);
        assert_eq!(config.voice.stt_min_speech_ms, 250);
        assert_eq!(config.voice.stt_preroll_ms, 300);
        assert_eq!(config.voice.stt_max_utterance_ms, 30_000);
        assert_eq!(config.voice.max_transcript_chars, 16_000);
        assert!(config.voice.commands_enabled);
        assert!(!config.voice.commands_submit_enabled);
        assert_eq!(config.voice.provider, "sidecar");
        assert_eq!(config.voice.sidecar_command, "synaps-voice-local");
        assert!(config.voice.sidecar_args.is_empty());
        assert!(!config.voice.sidecar_restart_on_crash);
        assert_eq!(config.voice.sidecar_protocol_version, 1);
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
    fn load_config_parses_voice_keys() {
        let config = parse_config_content(r#"
voice.enabled = true
voice.mode = toggle
voice.stt_backend = whisper-rs
voice.stt_model_path = /models/whisper.bin
voice.stt_language = es
voice.stt_show_partials = false
voice.stt_auto_submit = true
voice.stt_silence_submit_ms = 1500
voice.stt_vad_rms_threshold = 0.02
voice.stt_min_speech_ms = 300
voice.stt_preroll_ms = 400
voice.stt_max_utterance_ms = 45000
voice.max_transcript_chars = 8000
voice.commands_enabled = false
voice.commands.submit_enabled = true
voice.provider = sidecar
voice.sidecar.command = synaps-voice-mock
voice.sidecar.args = --transcript, hello from config
voice.sidecar.restart_on_crash = true
voice.sidecar.protocol_version = 1
voice.future_unknown = preserved-by-ignored-parser
"#);
        assert!(config.voice.enabled);
        assert_eq!(config.voice.mode, "toggle");
        assert_eq!(config.voice.stt_backend, "whisper-rs");
        assert_eq!(config.voice.stt_model_path, PathBuf::from("/models/whisper.bin"));
        assert_eq!(config.voice.stt_language, "es");
        assert!(!config.voice.stt_show_partials);
        assert!(config.voice.stt_auto_submit);
        assert_eq!(config.voice.stt_silence_submit_ms, 1500);
        assert_eq!(config.voice.stt_vad_rms_threshold, 0.02);
        assert_eq!(config.voice.stt_min_speech_ms, 300);
        assert_eq!(config.voice.stt_preroll_ms, 400);
        assert_eq!(config.voice.stt_max_utterance_ms, 45_000);
        assert_eq!(config.voice.max_transcript_chars, 8_000);
        assert!(!config.voice.commands_enabled);
        assert!(config.voice.commands_submit_enabled);
        assert_eq!(config.voice.provider, "sidecar");
        assert_eq!(config.voice.sidecar_command, "synaps-voice-mock");
        assert_eq!(config.voice.sidecar_args, vec!["--transcript".to_string(), "hello from config".to_string()]);
        assert!(config.voice.sidecar_restart_on_crash);
        assert_eq!(config.voice.sidecar_protocol_version, 1);
    }

    #[test]
    fn invalid_voice_values_keep_defaults() {
        let config = parse_config_content(r#"
voice.enabled = maybe
voice.stt_show_partials = maybe
voice.stt_auto_submit = maybe
voice.stt_silence_submit_ms = nope
voice.stt_vad_rms_threshold = nope
voice.stt_min_speech_ms = nope
voice.stt_preroll_ms = nope
voice.stt_max_utterance_ms = nope
voice.max_transcript_chars = nope
voice.sidecar.restart_on_crash = maybe
voice.sidecar.protocol_version = nope
"#);
        assert!(config.voice.enabled);
        assert!(config.voice.stt_show_partials);
        assert!(!config.voice.stt_auto_submit);
        assert_eq!(config.voice.stt_silence_submit_ms, 1000);
        assert_eq!(config.voice.stt_vad_rms_threshold, 0.01);
        assert_eq!(config.voice.stt_min_speech_ms, 250);
        assert_eq!(config.voice.stt_preroll_ms, 300);
        assert_eq!(config.voice.stt_max_utterance_ms, 30_000);
        assert_eq!(config.voice.max_transcript_chars, 16_000);
        assert!(!config.voice.sidecar_restart_on_crash);
        assert_eq!(config.voice.sidecar_protocol_version, 1);
    }

    #[test]
    fn invalid_voice_enum_values_keep_defaults() {
        let config = parse_config_content(r#"
voice.mode = continuous
voice.stt_backend = deepgram
voice.provider = remote
"#);
        assert_eq!(config.voice.mode, "toggle");
        assert_eq!(config.voice.stt_backend, "whisper-rs");
        assert_eq!(config.voice.provider, "sidecar");
    }

    #[test]
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
    fn test_load_config_disable_lists() {
        let test_dir = std::path::PathBuf::from("/tmp/synaps-config-test-disable-lists/.synaps-cli");
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

        assert_eq!(config.disabled_plugins, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(config.favorite_models, vec![
            "claude/claude-opus-4-7".to_string(),
            "groq/llama-3.3-70b-versatile".to_string(),
        ]);
        assert_eq!(config.disabled_skills, vec!["baz".to_string(), "plug:qual".to_string()]);
    }

    #[test]
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
}
