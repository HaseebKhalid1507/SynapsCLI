//! Engine-level command results — TUI-agnostic outcomes of slash commands.
//!
//! The engine processes a command and returns a `CommandResult`.
//! Renderers (TUI, headless) decide how to display the result.

/// Result of processing a slash command in the engine.
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// No output, continue.
    None,

    /// Text output to display to the user.
    Output(String),

    /// Error message.
    Error(String),

    /// Model was changed.
    ModelChanged {
        model: String,
    },

    /// Thinking budget was changed.
    ThinkingChanged {
        level: String,
        budget: u32,
    },

    /// System prompt was updated.
    SystemPromptSet {
        source: String, // "inline", "file", "saved"
    },

    /// System prompt displayed.
    SystemPromptShow {
        prompt: String,
    },

    /// Session list.
    SessionList {
        sessions: Vec<SessionSummary>,
    },

    /// Session cleared. New session returned.
    Cleared,

    /// Quit requested.
    Quit,

    /// Compaction requested (engine should trigger it).
    Compact {
        custom_instructions: Option<String>,
    },

    /// Session resumed.
    Resumed {
        session_id: String,
        model: String,
    },

    /// Session named/saved.
    Named {
        name: String,
    },

    /// Chain info.
    ChainInfo(String),

    /// Request to open a TUI-specific modal (TUI handles, headless ignores).
    OpenModal(ModalRequest),

    /// Status/usage info.
    Status {
        text: String,
    },

    /// Ping results.
    PingStarted,

    /// Keybind list.
    KeybindList(String),

    /// Skill loaded — needs to be injected into the conversation.
    SkillLoaded {
        skill: std::sync::Arc<crate::skills::LoadedSkill>,
        arg: String,
    },

    /// Plugin command to execute.
    PluginCommand {
        command: std::sync::Arc<crate::skills::registry::RegisteredPluginCommand>,
        arg: String,
    },

    /// Sidecar toggle/status.
    SidecarToggle {
        plugin_id: Option<String>,
    },
    SidecarStatus {
        plugin_id: Option<String>,
    },
}

/// TUI-specific modals the engine can request.
#[derive(Debug, Clone)]
pub enum ModalRequest {
    Models,
    Settings,
    Plugins,
    HelpFind { query: String },
    Extensions { sub: String },
}

/// Summary of a session for listing.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub model: String,
    pub title: Option<String>,
    pub cost: f64,
    pub message_count: usize,
    pub is_current: bool,
}

/// Parse a slash command into (command, arg).
pub fn parse_command(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let without_slash = &trimmed[1..];
    let (cmd, arg) = match without_slash.find(char::is_whitespace) {
        Some(pos) => (&without_slash[..pos], without_slash[pos..].trim()),
        None => (without_slash, ""),
    };
    Some((cmd, arg))
}

/// Process commands that are pure engine logic — no TUI state needed.
/// Returns None if the command needs TUI-level handling.
///
/// NOTE: this runs BEFORE any renderer-level command arms (the TUI calls it
/// first and returns early on Some — see tui/commands.rs). Renderer arms for
/// commands handled here are unreachable for the matched cases.
pub fn handle_engine_command(
    cmd: &str,
    arg: &str,
    runtime: &mut crate::Runtime,
) -> Option<CommandResult> {
    let result = evaluate_engine_command(cmd, arg)?;
    // Apply the runtime side effects of the (purely computed) result.
    match &result {
        CommandResult::ModelChanged { model } => runtime.set_model(model.clone()),
        CommandResult::ThinkingChanged { budget, .. } => runtime.set_thinking_budget(*budget),
        _ => {}
    }
    Some(result)
}

/// Pure command → result mapping (no runtime mutation). Split out of
/// `handle_engine_command` so dispatch can be unit-tested without a Runtime.
pub fn evaluate_engine_command(cmd: &str, arg: &str) -> Option<CommandResult> {
    match cmd {
        // `models` is the TUI alias — intercept it identically so the
        // non-empty-arg path has a single owner.
        "model" | "models" if !arg.is_empty() => Some(CommandResult::ModelChanged {
            model: arg.to_string(),
        }),
        "thinking" if !arg.is_empty() => match parse_thinking_arg(arg) {
            Ok((level, budget)) => Some(CommandResult::ThinkingChanged { level, budget }),
            Err(e) => Some(CommandResult::Error(e)),
        },
        "quit" | "exit" => Some(CommandResult::Quit),
        "compact" => Some(CommandResult::Compact {
            custom_instructions: if arg.is_empty() {
                None
            } else {
                Some(arg.to_string())
            },
        }),
        _ => None, // Not an engine-level command — delegate to renderer
    }
}

/// Parse a `/thinking` argument into a canonical (level, budget) pair.
pub fn parse_thinking_arg(arg: &str) -> Result<(String, u32), String> {
    match arg {
        "off" | "none" => Ok(("off".to_string(), 0)),
        // `adaptive` matches the runtime's own label for budget=0
        // (see core::models::thinking_level_for_budget). Adding
        // it here removes the need for renderers to pre-intercept
        // this case before delegating to the engine.
        "adaptive" => Ok(("adaptive".to_string(), 0)),
        "low" => Ok(("low".to_string(), 2048)),
        "medium" | "med" => Ok(("medium".to_string(), 4096)),
        "high" => Ok(("high".to_string(), 16384)),
        "xhigh" | "max" => Ok(("xhigh".to_string(), 32768)),
        other => {
            if let Ok(n) = other.parse::<u32>() {
                Ok((format!("custom({})", n), n))
            } else {
                Err(format!("unknown thinking level: {} (use off/adaptive/low/medium/high/xhigh or a number)", other))
            }
        }
    }
}

/// Config-file value for a thinking change: the canonical level name when it
/// is one config.rs can parse back, otherwise the raw budget number (which
/// `parse_thinking_budget` also accepts; 0 is the adaptive sentinel).
pub fn thinking_config_value(level: &str, budget: u32) -> String {
    match level {
        "low" | "medium" | "high" | "xhigh" | "adaptive" => level.to_string(),
        _ => budget.to_string(),
    }
}

/// Persist a config key and return an honest, user-visible status suffix —
/// never claims "(saved to config)" unless the write actually succeeded.
pub fn persist_to_config(key: &str, value: &str) -> String {
    match crate::config::write_config_value(key, value) {
        Ok(()) => "(saved to config)".to_string(),
        Err(e) => format!("(session only — failed to persist: {})", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_command_carries_model_name() {
        match evaluate_engine_command("model", "claude-sonnet-4-6") {
            Some(CommandResult::ModelChanged { model }) => assert_eq!(model, "claude-sonnet-4-6"),
            other => panic!("expected ModelChanged, got {:?}", other),
        }
        // `models` alias intercepts identically
        assert!(matches!(
            evaluate_engine_command("models", "claude-opus-4-6"),
            Some(CommandResult::ModelChanged { .. })
        ));
        // empty arg falls through to the renderer (model picker)
        assert!(evaluate_engine_command("model", "").is_none());
    }

    #[test]
    fn thinking_command_normalizes_levels() {
        match evaluate_engine_command("thinking", "high") {
            Some(CommandResult::ThinkingChanged { level, budget }) => {
                assert_eq!(level, "high");
                assert_eq!(budget, 16384);
            }
            other => panic!("expected ThinkingChanged, got {:?}", other),
        }
        assert_eq!(
            parse_thinking_arg("med").unwrap(),
            ("medium".to_string(), 4096)
        );
        assert_eq!(
            parse_thinking_arg("8192").unwrap(),
            ("custom(8192)".to_string(), 8192)
        );
        assert!(parse_thinking_arg("bogus").is_err());
        assert!(evaluate_engine_command("thinking", "").is_none());
    }

    #[test]
    fn compact_carries_custom_instructions() {
        match evaluate_engine_command("compact", "focus on auth") {
            Some(CommandResult::Compact {
                custom_instructions,
            }) => {
                assert_eq!(custom_instructions.as_deref(), Some("focus on auth"));
            }
            other => panic!("expected Compact, got {:?}", other),
        }
        assert!(matches!(
            evaluate_engine_command("compact", ""),
            Some(CommandResult::Compact {
                custom_instructions: None
            })
        ));
    }

    #[test]
    fn thinking_config_value_is_parseable() {
        assert_eq!(thinking_config_value("medium", 4096), "medium");
        assert_eq!(thinking_config_value("adaptive", 0), "adaptive");
        assert_eq!(thinking_config_value("off", 0), "0");
        assert_eq!(thinking_config_value("custom(8192)", 8192), "8192");
    }

    #[test]
    #[serial_test::serial]
    fn persist_to_config_reports_write_result() {
        let home = std::path::PathBuf::from("/tmp/synaps-engine-persist-test");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".synaps-cli")).unwrap();
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        let status = persist_to_config("model", "claude-sonnet-4-6");

        if let Some(h) = original {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        assert_eq!(status, "(saved to config)");
        let contents = std::fs::read_to_string(home.join(".synaps-cli/config")).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
