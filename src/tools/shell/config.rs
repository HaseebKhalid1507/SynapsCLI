//! Shell session configuration — parsed from ~/.synaps-cli/config shell.* keys.

use std::time::Duration;

/// Configuration for interactive shell sessions.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub max_sessions: usize,
    pub idle_timeout: Duration,
    pub readiness_timeout_ms: u64,
    pub max_readiness_timeout_ms: u64,
    pub default_rows: u16,
    pub default_cols: u16,
    pub prompt_patterns: Vec<String>,
    pub readiness_strategy: String,
    pub max_output: usize,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            max_sessions: 5,
            idle_timeout: Duration::from_secs(600),
            readiness_timeout_ms: 300,
            max_readiness_timeout_ms: 10_000,
            default_rows: 24,
            default_cols: 80,
            prompt_patterns: default_prompt_patterns(),
            readiness_strategy: "hybrid".to_string(),
            max_output: 30000,
        }
    }
}

fn default_prompt_patterns() -> Vec<String> {
    vec![
        r"[$#%»] $".into(),
        r"[$#%»]\s*$".into(),
        r"\(gdb\)\s*$".into(),
        r">>>\s*$".into(),
        r"\.\.\.\:\s*$".into(),
        r"In \[\d+\]:\s*$".into(),
        r"irb.*>\s*$".into(),
        r"mysql>\s*$".into(),
        r"postgres[=#]>\s*$".into(),
        r"Password:\s*$".into(),
        r"\[Y/n\]\s*$".into(),
        r"\(yes/no.*\)\?\s*$".into(),
        r"% $".into(),
        r"% \s*$".into(),
        r"root@[\w.-]+:[/~].*# $".into(),
        r"[\w.-]+@[\w.-]+:[/~].*\$ $".into(),
        r"Enter passphrase.*:\s*$".into(),
        r"Token:\s*$".into(),
        r"Verification code:\s*$".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::load_config;

    #[test]
    fn test_shell_config_default() {
        let config = ShellConfig::default();
        assert_eq!(config.max_sessions, 5);
        assert_eq!(config.idle_timeout, Duration::from_secs(600));
        assert_eq!(config.readiness_timeout_ms, 300);
        assert_eq!(config.max_readiness_timeout_ms, 10_000);
        assert_eq!(config.default_rows, 24);
        assert_eq!(config.default_cols, 80);
        assert_eq!(config.prompt_patterns.len(), 19);
    }

    #[test]
    fn test_shell_config_from_file() {
        // These tests must run sequentially since they mutate HOME env var.
        // Combined into one test to avoid parallel races.

        // --- Subtest 1: Full config parsing ---
        {
            let test_dir = std::path::PathBuf::from("/tmp/synaps-shell-test-1/.synaps-cli");
            let _ = std::fs::create_dir_all(&test_dir);
            let config_path = test_dir.join("config");

            let config_content = "shell.max_sessions = 10\nshell.idle_timeout = 1200\nshell.readiness_timeout_ms = 500\nshell.max_readiness_timeout_ms = 20000\nshell.default_rows = 30\nshell.default_cols = 120\n";
            std::fs::write(&config_path, config_content).unwrap();

            let original_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", "/tmp/synaps-shell-test-1");
            let config = load_config();
            if let Some(home) = original_home {
                std::env::set_var("HOME", home);
            }
            let _ = std::fs::remove_dir_all("/tmp/synaps-shell-test-1");

            assert_eq!(config.shell.max_sessions, 10);
            assert_eq!(config.shell.idle_timeout, Duration::from_secs(1200));
            assert_eq!(config.shell.readiness_timeout_ms, 500);
            assert_eq!(config.shell.max_readiness_timeout_ms, 20000);
            assert_eq!(config.shell.default_rows, 30);
            assert_eq!(config.shell.default_cols, 120);
        }

        // --- Subtest 2: Partial config — missing keys use defaults ---
        {
            let test_dir = std::path::PathBuf::from("/tmp/synaps-shell-test-2/.synaps-cli");
            let _ = std::fs::create_dir_all(&test_dir);
            let config_path = test_dir.join("config");

            let config_content = "shell.max_sessions = 3\nshell.default_rows = 40\n";
            std::fs::write(&config_path, config_content).unwrap();

            let original_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", "/tmp/synaps-shell-test-2");
            let config = load_config();
            if let Some(home) = original_home {
                std::env::set_var("HOME", home);
            }
            let _ = std::fs::remove_dir_all("/tmp/synaps-shell-test-2");

            assert_eq!(config.shell.max_sessions, 3);
            assert_eq!(config.shell.default_rows, 40);
            assert_eq!(config.shell.idle_timeout, Duration::from_secs(600));
            assert_eq!(config.shell.readiness_timeout_ms, 300);
        }

        // --- Subtest 3: Invalid values → defaults ---
        {
            let test_dir = std::path::PathBuf::from("/tmp/synaps-shell-test-3/.synaps-cli");
            let _ = std::fs::create_dir_all(&test_dir);
            let config_path = test_dir.join("config");

            let config_content = "shell.max_sessions = not_a_number\nshell.idle_timeout = invalid\n";
            std::fs::write(&config_path, config_content).unwrap();

            let original_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", "/tmp/synaps-shell-test-3");
            let config = load_config();
            if let Some(home) = original_home {
                std::env::set_var("HOME", home);
            }
            let _ = std::fs::remove_dir_all("/tmp/synaps-shell-test-3");

            assert_eq!(config.shell.max_sessions, 5);
            assert_eq!(config.shell.idle_timeout, Duration::from_secs(600));
        }
    }
}
