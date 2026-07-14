//! Engine-level command results — TUI-agnostic outcomes of slash commands.
//!
//! The engine processes a command and returns a `CommandResult`.
//! Renderers (TUI, headless) decide how to display the result.

use agent_core::reasoning::ReasoningLevel;

/// A thinking specification — either a named level or a custom numeric budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSpec {
    /// A named canonical level (off/adaptive/low/medium/high/xhigh/max/ultra).
    Named(ReasoningLevel),
    /// A custom numeric budget (e.g. `/thinking 8192`).
    /// `level` is the nearest named level for display; `budget` is the exact value.
    Custom { level: ReasoningLevel, budget: u32 },
}

impl ThinkingSpec {
    /// Named level (used for validation and display).
    pub fn level(self) -> ReasoningLevel {
        match self {
            ThinkingSpec::Named(l) => l,
            ThinkingSpec::Custom { level, .. } => level,
        }
    }

    /// Exact budget, if applicable. `None` for Max/Ultra which have no numeric budget.
    pub fn budget(self) -> Option<u32> {
        match self {
            ThinkingSpec::Named(l) => l.to_legacy_budget(),
            ThinkingSpec::Custom { budget, .. } => Some(budget),
        }
    }

    /// Config-file string: exact budget digits for Custom, named string otherwise.
    pub fn config_value(self) -> String {
        match self {
            ThinkingSpec::Named(l) => l.as_str().to_string(),
            ThinkingSpec::Custom { budget, .. } => budget.to_string(),
        }
    }
}

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

    /// Thinking level was changed.
    ThinkingChanged {
        spec: ThinkingSpec,
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
pub fn handle_engine_command(
    cmd: &str,
    arg: &str,
    runtime: &mut crate::Runtime,
) -> Option<CommandResult> {
    let result = evaluate_engine_command(cmd, arg)?;
    match &result {
        CommandResult::ModelChanged { model } => runtime.set_model(model.clone()),
        CommandResult::ThinkingChanged { spec } => {
            // Validate and apply the spec against model capabilities BEFORE
            // mutating runtime. State is unchanged on Err.
            match spec {
                ThinkingSpec::Named(level) => {
                    if let Err(msg) = runtime.set_reasoning_level_checked(*level) {
                        return Some(CommandResult::Error(msg));
                    }
                }
                ThinkingSpec::Custom { budget, .. } => {
                    // Custom budget: validate the derived level against the
                    // exact-model capability tables BEFORE mutating; on Ok the
                    // exact budget is retained (Anthropic wire uses it as-is).
                    if let Err(msg) = runtime.set_thinking_budget_checked(*budget) {
                        return Some(CommandResult::Error(msg));
                    }
                }
            }
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
            Ok(spec) => Some(CommandResult::ThinkingChanged { spec }),
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

/// Parse a `/thinking` argument into a `ThinkingSpec`.
pub fn parse_thinking_arg(arg: &str) -> Result<ThinkingSpec, String> {
    match ReasoningLevel::parse(arg) {
        Some(level) => Ok(ThinkingSpec::Named(level)),
        None => {
            if let Ok(n) = arg.trim().parse::<u32>() {
                // Custom numeric budget: bucketize to nearest named level for display.
                Ok(ThinkingSpec::Custom {
                    level: ReasoningLevel::from_legacy_budget(n),
                    budget: n,
                })
            } else {
                Err(format!(
                    "unknown thinking level: {} \
                     (use off/adaptive/low/medium/high/xhigh/max/ultra/ultracode or a number)",
                    arg
                ))
            }
        }
    }
}

/// Canonical config-file string for a thinking change.
/// Returns the exact budget digits for Custom, named string for Named.
pub fn thinking_config_value(spec: ThinkingSpec) -> String {
    spec.config_value()
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
            Some(CommandResult::ThinkingChanged { spec }) => {
                assert_eq!(spec.level(), ReasoningLevel::High);
                assert_eq!(spec.budget(), Some(16384));
                assert!(matches!(spec, ThinkingSpec::Named(ReasoningLevel::High)));
            }
            other => panic!("expected ThinkingChanged, got {:?}", other),
        }
        let med = parse_thinking_arg("med").unwrap();
        assert_eq!(med.level(), ReasoningLevel::Medium);
        assert_eq!(med.budget(), Some(4096));

        // Custom numeric: produces ThinkingSpec::Custom, not Named.
        let custom = parse_thinking_arg("8192").unwrap();
        assert!(
            matches!(
                custom,
                ThinkingSpec::Custom {
                    level: ReasoningLevel::High,
                    budget: 8192
                }
            ),
            "expected Custom{{High, 8192}}, got {:?}",
            custom
        );
        assert_eq!(custom.budget(), Some(8192));

        assert!(parse_thinking_arg("bogus").is_err());
        assert!(evaluate_engine_command("thinking", "").is_none());
    }

    #[test]
    fn thinking_command_off_and_adaptive_are_distinct() {
        let off = parse_thinking_arg("off").unwrap();
        let adp = parse_thinking_arg("adaptive").unwrap();
        assert_eq!(off.level(), ReasoningLevel::Off);
        assert_eq!(off.budget(), Some(0));
        assert_eq!(adp.level(), ReasoningLevel::Adaptive);
        assert_eq!(adp.budget(), Some(0));
        assert_ne!(off.level(), adp.level());
    }

    #[test]
    fn max_and_ultra_have_no_budget() {
        let max_spec = parse_thinking_arg("max").unwrap();
        assert_eq!(max_spec.level(), ReasoningLevel::Max);
        assert_eq!(max_spec.budget(), None, "max has no numeric budget");

        let ultra_spec = parse_thinking_arg("ultra").unwrap();
        assert_eq!(ultra_spec.level(), ReasoningLevel::Ultra);
        assert_eq!(ultra_spec.budget(), None, "ultra has no numeric budget");

        // xhigh still has a numeric budget
        let xhigh_spec = parse_thinking_arg("xhigh").unwrap();
        assert_eq!(xhigh_spec.level(), ReasoningLevel::XHigh);
        assert_eq!(xhigh_spec.budget(), Some(32768));
    }

    #[test]
    fn thinking_config_value_is_named_for_named_levels() {
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Off)),
            "off"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Adaptive)),
            "adaptive"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Low)),
            "low"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Medium)),
            "medium"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::High)),
            "high"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::XHigh)),
            "xhigh"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Max)),
            "max"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::Ultra)),
            "ultra"
        );
        assert_eq!(
            thinking_config_value(ThinkingSpec::Named(ReasoningLevel::UltraCode)),
            "ultracode"
        );
    }

    #[test]
    fn thinking_config_value_is_exact_digits_for_custom_budget() {
        // B3: /thinking 8192 must persist "8192", not the named level "high".
        let spec = ThinkingSpec::Custom {
            level: ReasoningLevel::High,
            budget: 8192,
        };
        assert_eq!(thinking_config_value(spec), "8192");
        let spec2 = ThinkingSpec::Custom {
            level: ReasoningLevel::Medium,
            budget: 3000,
        };
        assert_eq!(thinking_config_value(spec2), "3000");
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
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("openai-codex/gpt-5.6-sol".to_string());
        assert!(rt
            .set_reasoning_level_checked(ReasoningLevel::Ultra)
            .is_ok());
    }

    #[test]
    fn validate_level_codex_luna_rejects_ultra_leaves_state_unchanged() {
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("openai-codex/gpt-5.6-luna".to_string());
        rt.set_reasoning_level(ReasoningLevel::Low);
        let err = rt
            .set_reasoning_level_checked(ReasoningLevel::Ultra)
            .unwrap_err();
        assert!(err.contains("ultra"));
        assert!(err.contains("gpt-5.6-luna"));
        // State must be unchanged after rejection.
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Low);
    }

    #[test]
    fn validate_level_non_codex_rejects_ultra() {
        for model in [
            "claude-sonnet-4-6",
            "anthropic/claude-opus-4-7",
            "groq/llama-3",
        ] {
            let mut rt = crate::Runtime::new_headless();
            rt.set_model(model.to_string());
            assert!(
                rt.set_reasoning_level_checked(ReasoningLevel::Ultra)
                    .is_err(),
                "non-Codex {model} must not gain ultra without exact metadata"
            );
        }
    }

    // B3: provenance tests -------------------------------------------------------

    #[test]
    fn set_reasoning_level_is_not_explicit() {
        // set_reasoning_level (config/restore path) must NOT set explicit flag.
        let mut rt = crate::Runtime::new_headless();
        rt.set_reasoning_level(ReasoningLevel::High);
        assert!(
            !rt.is_reasoning_explicit(),
            "set_reasoning_level must not mark explicit"
        );
    }

    #[test]
    fn set_reasoning_level_explicit_marks_flag() {
        let mut rt = crate::Runtime::new_headless();
        rt.set_reasoning_level_explicit(ReasoningLevel::High);
        assert!(
            rt.is_reasoning_explicit(),
            "set_reasoning_level_explicit must mark explicit"
        );
    }

    #[test]
    fn set_model_overwrites_non_explicit_codex_default() {
        // No explicit choice → set_model applies model's default.
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("openai-codex/gpt-5.6-luna".to_string());
        // luna default is Medium per static table
        assert_eq!(rt.reasoning_level(), ReasoningLevel::Medium);
        assert!(!rt.is_reasoning_explicit());
    }

    #[test]
    fn set_model_preserves_explicit_user_choice() {
        // Explicit user choice survives model switch.
        let mut rt = crate::Runtime::new_headless();
        rt.set_reasoning_level_explicit(ReasoningLevel::Low);
        rt.set_model("openai-codex/gpt-5.6-sol".to_string());
        // sol default is Low anyway, but let's use a level that differs from default
        assert!(
            rt.is_reasoning_explicit(),
            "explicit flag must survive set_model"
        );
    }

    #[test]
    fn apply_config_thinking_sets_explicit() {
        // Config-specified thinking level is explicit — preserved across model switch.
        use crate::config::SynapsConfig;
        let mut rt = crate::Runtime::new_headless();
        let config = SynapsConfig {
            thinking_level: Some(ReasoningLevel::XHigh),
            ..Default::default()
        };
        rt.apply_config(&config);
        assert!(
            rt.is_reasoning_explicit(),
            "config thinking must be explicit"
        );
        assert_eq!(rt.reasoning_level(), ReasoningLevel::XHigh);
        // Model switch must not overwrite the explicit config level.
        rt.set_model("openai-codex/gpt-5.6-luna".to_string());
        assert_eq!(
            rt.reasoning_level(),
            ReasoningLevel::XHigh,
            "explicit config level must survive set_model"
        );
    }

    #[test]
    fn set_thinking_budget_explicit_retains_exact_budget_and_marks_flag() {
        // B3: /thinking 8192 → set_thinking_budget_explicit(8192) must retain 8192.
        let mut rt = crate::Runtime::new_headless();
        rt.set_thinking_budget_explicit(8192);
        assert_eq!(
            rt.thinking_budget_raw(),
            8192,
            "exact budget must be retained"
        );
        assert!(rt.is_reasoning_explicit());
        // Named level for display is High (4097..=16384 range).
        assert_eq!(rt.reasoning_level(), ReasoningLevel::High);
    }

    // ── Numeric `/thinking <N>` mutation-time validation (review corrective) ──

    #[test]
    fn custom_budget_rejected_on_xai_intrinsic_reasoning_leaves_state_unchanged() {
        // /thinking 8192 derives High; grok-4.3 has no effort control → reject
        // immediately, no mutation, no silent downgrade.
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.3".to_string());
        let before_budget = rt.thinking_budget_raw();
        let before_level = rt.reasoning_level();
        let before_explicit = rt.is_reasoning_explicit();
        match handle_engine_command("thinking", "8192", &mut rt) {
            Some(CommandResult::Error(msg)) => {
                assert!(
                    msg.contains("xai-auth/grok-4.3"),
                    "message names model: {msg}"
                )
            }
            other => panic!("expected Error, got {:?}", other),
        }
        assert_eq!(
            rt.thinking_budget_raw(),
            before_budget,
            "budget must be unchanged"
        );
        assert_eq!(
            rt.reasoning_level(),
            before_level,
            "level must be unchanged"
        );
        assert_eq!(
            rt.is_reasoning_explicit(),
            before_explicit,
            "provenance unchanged"
        );
    }

    #[test]
    fn custom_budget_rejected_on_xai_non_reasoning_model() {
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.20-0309-non-reasoning".to_string());
        let before_budget = rt.thinking_budget_raw();
        match handle_engine_command("thinking", "8192", &mut rt) {
            Some(CommandResult::Error(msg)) => {
                assert!(msg.contains("non-reasoning"), "{msg}")
            }
            other => panic!("expected Error, got {:?}", other),
        }
        assert_eq!(
            rt.thinking_budget_raw(),
            before_budget,
            "state must be unchanged"
        );
    }

    #[test]
    fn custom_budget_maps_to_high_and_is_accepted_on_xai_45() {
        // 8192 → High, which grok-4.5 supports exactly → accepted.
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("xai-auth/grok-4.5".to_string());
        match handle_engine_command("thinking", "8192", &mut rt) {
            Some(CommandResult::ThinkingChanged { spec }) => {
                assert_eq!(spec.level(), ReasoningLevel::High);
                assert_eq!(spec.budget(), Some(8192));
            }
            other => panic!("expected ThinkingChanged, got {:?}", other),
        }
        assert_eq!(rt.reasoning_level(), ReasoningLevel::High);
        assert!(rt.is_reasoning_explicit());
    }

    #[test]
    fn custom_budget_preserved_exactly_on_anthropic_fixed_budget_model() {
        // Anthropic compatibility: numeric budgets stay exact (never bucketized
        // away) and are accepted on fixed-budget thinking models.
        let mut rt = crate::Runtime::new_headless();
        rt.set_model("claude-sonnet-4-6".to_string());
        match handle_engine_command("thinking", "8192", &mut rt) {
            Some(CommandResult::ThinkingChanged { spec }) => {
                assert_eq!(spec.config_value(), "8192", "persists exact digits");
            }
            other => panic!("expected ThinkingChanged, got {:?}", other),
        }
        assert_eq!(
            rt.thinking_budget_raw(),
            8192,
            "exact budget must be retained"
        );
        assert!(rt.is_reasoning_explicit());
    }

    #[test]
    fn custom_budget_config_value_is_digits_not_named() {
        // B3: ThinkingSpec::Custom config_value() must be the digit string.
        let spec = parse_thinking_arg("8192").unwrap();
        assert_eq!(
            spec.config_value(),
            "8192",
            "custom budget must persist as digits, not named level"
        );
    }
}
