//! Engine-level command results — TUI-agnostic outcomes of slash commands.
//!
//! The engine processes a command and returns a `CommandResult`.
//! Renderers (TUI, headless) decide how to display the result.

use serde_json::Value;

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
    Compact,

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
    SidecarToggle { plugin_id: Option<String> },
    SidecarStatus { plugin_id: Option<String> },
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
pub fn handle_engine_command(
    cmd: &str,
    arg: &str,
    runtime: &mut crate::Runtime,
) -> Option<CommandResult> {
    match cmd {
        "model" if !arg.is_empty() => {
            runtime.set_model(arg.to_string());
            Some(CommandResult::ModelChanged {
                model: arg.to_string(),
            })
        }
        "thinking" if !arg.is_empty() => {
            let (level, budget) = match arg {
                "off" | "none" => ("off".to_string(), 0),
                "low" => ("low".to_string(), 2048),
                "medium" | "med" => ("medium".to_string(), 4096),
                "high" => ("high".to_string(), 16384),
                "xhigh" | "max" => ("xhigh".to_string(), 32768),
                other => {
                    if let Ok(n) = other.parse::<u32>() {
                        (format!("custom({})", n), n)
                    } else {
                        return Some(CommandResult::Error(
                            format!("unknown thinking level: {} (use off/low/medium/high/xhigh or a number)", other)
                        ));
                    }
                }
            };
            runtime.set_thinking_budget(budget);
            Some(CommandResult::ThinkingChanged { level, budget })
        }
        "quit" | "exit" => Some(CommandResult::Quit),
        "compact" => Some(CommandResult::Compact),
        _ => None, // Not an engine-level command — delegate to renderer
    }
}
