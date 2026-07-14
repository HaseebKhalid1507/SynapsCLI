//! Engine-level command results — TUI-agnostic outcomes of slash commands.
//!
//! The engine processes a command and returns a `CommandResult`.
//! Renderers (TUI, headless) decide how to display the result.

use agent_core::reasoning::ReasoningLevel;

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

    /// Thinking level was changed. `budget` is `None` for named-only levels
    /// (Max/Ultra) that have no valid numeric representation.
    ThinkingChanged {
        level: ReasoningLevel,
        budget: Option<u32>,
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

/// Validate that `level` is permissible for the runtime's current model.
/// Returns `Err(user-facing message)` if the level is unsupported.
/// Currently enforced for Codex models; all other models accept any level.
fn validate_level_for_model(level: ReasoningLevel, model: &str) -> Result<(), String> {
    use crate::runtime::openai::catalog::{capability_cache, codex_static_capability,
        ReasoningSupport};
    let Some(model_id) = model.strip_prefix("openai-codex/") else {
        return Ok(());
    };
    // Live cache takes priority; static fallback second.
    let supported: Option<Vec<ReasoningLevel>> =
        capability_cache::get(model).and_then(|m| match m.reasoning {
            ReasoningSupport::CodexNamed { supported, .. } => Some(supported),
            _ => None,
        })
        .or_else(|| match codex_static_capability(model_id)? {
            ReasoningSupport::CodexNamed { supported, .. } => Some(supported),
            _ => None,
        });
    let Some(supported) = supported else { return Ok(()); };
    if supported.contains(&level) {
        Ok(())
    } else {
        Err(format!(
            "reasoning level '{}' is not supported by {}; supported: [{}]",
            level, model,
            supported.iter().map(|l| l.as_str()).collect::<Vec<_>>().join(", ")
        ))
    }
}

/// Process commands that are pure engine logic — no TUI state needed.
/// Returns None if the command needs TUI-level handling.
pub fn handle_engine_command(
    cmd: &str,
    arg: &str,
    runtime: &mut crate::Runtime,
) -> Option<CommandResult> {
    let result = evaluate_engine_command(cmd, arg)?;
    match &result {
        CommandResult::ModelChanged { model } => runtime.set_model(model.clone()),
        CommandResult::ThinkingChanged { level, .. } => {
            // Validate against model capabilities BEFORE mutating runtime.
            if let Err(msg) = validate_level_for_model(*level, runtime.model()) {
                return Some(CommandResult::Error(msg));
            }
            runtime.set_reasoning_level(*level);
        }
        _ => {}
    }
    Some(result)
}

/// Pure command → result mapping (no runtime mutation).
pub fn evaluate_engine_command(cmd: &str, arg: &str) -> Option<CommandResult> {
    match cmd {
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
        _ => None,
    }
}

/// Parse a `/thinking` argument into a `(ReasoningLevel, Option<u32>)`.
/// `budget` is `None` for Max/Ultra which have no numeric representation.
pub fn parse_thinking_arg(arg: &str) -> Result<(ReasoningLevel, Option<u32>), String> {
    match ReasoningLevel::parse(arg) {
        Some(level) => Ok((level, level.to_legacy_budget())),
        None => {
            if let Ok(n) = arg.trim().parse::<u32>() {
                // Custom numeric budget: bucketize to nearest named level.
                Ok((ReasoningLevel::from_legacy_budget(n), Some(n)))
            } else {
                Err(format!(
                    "unknown thinking level: {} \
                     (use off/adaptive/low/medium/high/xhigh/max/ultra or a number)",
                    arg
                ))
            }
        }
    }
}

/// Canonical config-file string for a thinking change.
pub fn thinking_config_value(level: ReasoningLevel, _budget: Option<u32>) -> String {
    level.as_str().to_string()
}

/// Persist a config key and return a user-visible status suffix.
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
        assert!(matches!(
            evaluate_engine_command("models", "claude-opus-4-6"),
            Some(CommandResult::ModelChanged { .. })
        ));
        assert!(evaluate_engine_command("model", "").is_none());
    }

    #[test]
    fn thinking_command_normalizes_levels() {
        match evaluate_engine_command("thinking", "high") {
            Some(CommandResult::ThinkingChanged { level, budget }) => {
                assert_eq!(level, ReasoningLevel::High);
                assert_eq!(budget, Some(16384));
            }
            other => panic!("expected ThinkingChanged, got {:?}", other),
        }
        assert_eq!(
            parse_thinking_arg("med").unwrap(),
            (ReasoningLevel::Medium, Some(4096))
        );
        // Custom numeric: bucketized to High (4097..=16384 range).
        let (lvl, budget) = parse_thinking_arg("8192").unwrap();
        assert_eq!(lvl, ReasoningLevel::High);
        assert_eq!(budget, Some(8192));

        assert!(parse_thinking_arg("bogus").is_err());
        assert!(evaluate_engine_command("thinking", "").is_none());
    }

    #[test]
    fn thinking_command_off_and_adaptive_are_distinct() {
        let (off_lvl, off_bud) = parse_thinking_arg("off").unwrap();
        let (adp_lvl, adp_bud) = parse_thinking_arg("adaptive").unwrap();
        assert_eq!(off_lvl, ReasoningLevel::Off);
        assert_eq!(off_bud, Some(0));
        assert_eq!(adp_lvl, ReasoningLevel::Adaptive);
        assert_eq!(adp_bud, Some(0));
        assert_ne!(off_lvl, adp_lvl);
    }

    #[test]
    fn max_and_ultra_have_no_budget() {
        let (lvl, bud) = parse_thinking_arg("max").unwrap();
        assert_eq!(lvl, ReasoningLevel::Max);
        assert_eq!(bud, None, "max has no numeric budget");

        let (lvl, bud) = parse_thinking_arg("ultra").unwrap();
        assert_eq!(lvl, ReasoningLevel::Ultra);
        assert_eq!(bud, None, "ultra has no numeric budget");

        // xhigh still has a numeric budget
        let (lvl, bud) = parse_thinking_arg("xhigh").unwrap();
        assert_eq!(lvl, ReasoningLevel::XHigh);
        assert_eq!(bud, Some(32768));
    }

    #[test]
    fn thinking_config_value_is_named_for_all_levels() {
        assert_eq!(thinking_config_value(ReasoningLevel::Off,      Some(0)),     "off");
        assert_eq!(thinking_config_value(ReasoningLevel::Adaptive, Some(0)),     "adaptive");
        assert_eq!(thinking_config_value(ReasoningLevel::Low,      Some(2048)),  "low");
        assert_eq!(thinking_config_value(ReasoningLevel::Medium,   Some(4096)),  "medium");
        assert_eq!(thinking_config_value(ReasoningLevel::High,     Some(16384)), "high");
        assert_eq!(thinking_config_value(ReasoningLevel::XHigh,    Some(32768)), "xhigh");
        assert_eq!(thinking_config_value(ReasoningLevel::Max,      None),        "max");
        assert_eq!(thinking_config_value(ReasoningLevel::Ultra,    None),        "ultra");
    }

    #[test]
    fn compact_carries_custom_instructions() {
        match evaluate_engine_command("compact", "focus on auth") {
            Some(CommandResult::Compact { custom_instructions }) => {
                assert_eq!(custom_instructions.as_deref(), Some("focus on auth"));
            }
            other => panic!("expected Compact, got {:?}", other),
        }
        assert!(matches!(
            evaluate_engine_command("compact", ""),
            Some(CommandResult::Compact { custom_instructions: None })
        ));
    }

    #[test]
    #[serial_test::serial(synaps_base_dir)]
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

    #[test]
    fn validate_level_codex_sol_accepts_ultra() {
        assert!(validate_level_for_model(
            ReasoningLevel::Ultra,
            "openai-codex/gpt-5.6-sol"
        )
        .is_ok());
    }

    #[test]
    fn validate_level_codex_luna_rejects_ultra_leaves_state_unchanged() {
        let err = validate_level_for_model(
            ReasoningLevel::Ultra,
            "openai-codex/gpt-5.6-luna",
        )
        .unwrap_err();
        assert!(err.contains("ultra"));
        assert!(err.contains("gpt-5.6-luna"));
    }

    #[test]
    fn validate_level_non_codex_always_ok() {
        for model in ["claude-sonnet-4-6", "anthropic/claude-opus-4-7", "groq/llama-3"] {
            assert!(
                validate_level_for_model(ReasoningLevel::Ultra, model).is_ok(),
                "non-Codex {model} should pass validation"
            );
        }
    }
}
