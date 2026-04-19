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
        }
    }
}

fn default_prompt_patterns() -> Vec<String> {
    vec![
        r"[$#%>»] $".into(),
        r"[$#%>»]\s*$".into(),
        r"\(gdb\)\s*$".into(),
        r">>>\s*$".into(),
        r"\.\.\.\s*$".into(),
        r"In \[\d+\]:\s*$".into(),
        r"irb.*>\s*$".into(),
        r">\s*$".into(),
        r"mysql>\s*$".into(),
        r"postgres[=#]>\s*$".into(),
        r"Password:\s*$".into(),
        r"\[Y/n\]\s*$".into(),
        r"\(yes/no.*\)\?\s*$".into(),
    ]
}
