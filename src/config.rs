use std::path::PathBuf;
use std::sync::OnceLock;

static PROFILE_NAME: OnceLock<Option<String>> = OnceLock::new();

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
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".synaps-cli")
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
    pub skills: Option<Vec<String>>,
}

impl Default for SynapsConfig {
    fn default() -> Self {
        Self {
            model: None,
            thinking_budget: None,
            skills: None,
        }
    }
}

fn parse_thinking_budget(val: &str) -> Option<u32> {
    match val {
        "low" => Some(2048),
        "medium" => Some(4096),
        "high" => Some(16384),
        "xhigh" => Some(32768),
        _ => val.parse::<u32>().ok(),
    }
}

/// Parse the config file at ~/.synaps-cli/config (or profile variant).
/// Returns default config if file doesn't exist or can't be read.
pub fn load_config() -> SynapsConfig {
    let path = resolve_read_path("config");
    let mut config = SynapsConfig::default();
    
    let Ok(content) = std::fs::read_to_string(&path) else {
        return config;
    };
    
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let Some((key, val)) = line.split_once('=') else { continue };
        let key = key.trim();
        let val = val.trim();
        match key {
            "model" => config.model = Some(val.to_string()),
            "thinking" => config.thinking_budget = parse_thinking_budget(val),
            "skills" => config.skills = Some(crate::skills::parse_skills_config(val)),
            _ => {} // Unknown keys silently ignored
        }
    }
    
    config
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

/// Apply a parsed config to a Runtime instance.
pub fn apply_config(runtime: &mut crate::Runtime, config: &SynapsConfig) {
    if let Some(ref model) = config.model {
        runtime.set_model(model.clone());
    }
    if let Some(budget) = config.thinking_budget {
        runtime.set_thinking_budget(budget);
    }
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

    #[test] 
    fn test_load_config_nonexistent_file() {
        // Test that loading from a completely non-existent directory returns defaults
        // We'll temporarily set HOME to a non-existent directory
        let original_home = std::env::var("HOME").ok();
        
        std::env::set_var("HOME", "/tmp/nonexistent_home_dir_12345");
        
        let config = load_config();
        assert_eq!(config.model, None);
        assert_eq!(config.thinking_budget, None);
        
        // Restore original HOME
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_synaps_config_default() {
        let config = SynapsConfig::default();
        assert_eq!(config.model, None);
        assert_eq!(config.thinking_budget, None);
        assert_eq!(config.skills, None);
    }
}
