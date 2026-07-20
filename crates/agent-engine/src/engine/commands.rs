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
    // Runtime-backed commands (Task 12): shared across TUI, headless chat,
    // and server. `/context` here has no conversation history (the surface
    // owns it) — the TUI intercepts `/context` earlier and passes its own
    // history through `context_command`.
    match cmd {
        "context" => return Some(context_command(runtime, None)),
        "trace" => return Some(trace_command(arg, runtime)),
        "memory" => return Some(memory_command(arg, runtime)),
        _ => {}
    }
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

/// `/context` (Task 12, spec §6.6 acceptance): structured metadata-only
/// report — system/tool/history counts and bytes, latest cache component
/// change and reuse estimates, writer counters. Surfaces that own the
/// conversation pass it as `history`; others get honest `unavailable`.
pub fn context_command(
    runtime: &crate::Runtime,
    history: Option<&[crate::SharedMessage]>,
) -> CommandResult {
    CommandResult::Output(runtime.context_report(history).render())
}

/// `/trace next|next content|status` (Task 12): explicit trace controls.
pub fn trace_command(arg: &str, runtime: &crate::Runtime) -> CommandResult {
    match arg.trim() {
        "next" => {
            runtime.trace_arm_next(false);
            CommandResult::Output(
                "trace armed for the next request (metadata only, then auto-disabled)".to_string(),
            )
        }
        "next content" => {
            runtime.trace_arm_next(true);
            CommandResult::Output(
                "trace armed for the next request WITH redacted content capture.\n\
                 The request body (never headers/credentials) is recursively \
                 redacted and kept in a private, short-lived capture bundle; \
                 export it with `synaps trace export <request-id> \
                 --include-content --allow-content-export --output PATH` \
                 before it expires."
                    .to_string(),
            )
        }
        "" | "status" => CommandResult::Output(runtime.trace_status().render()),
        other => CommandResult::Error(format!(
            "unknown /trace subcommand: `{other}` (try: next, next content, status)"
        )),
    }
}

/// `/memory on|recall|capture|once|status|off|index-history|why` (task A5,
/// spec §7.3): the ONE deterministic engine entry point every frontend —
/// TUI, headless chat, RPC, server, watcher, agent — shares via
/// [`handle_engine_command`]. Frontends must not duplicate lease or budget
/// logic.
///
/// Every enabling arm mints host-owned
/// [`crate::runtime::memory_context::UserIntentProof::ExplicitCommand`]
/// proof right here: a deterministic slash command is authoritative (spec
/// assumption 5), and proof never derives from model text.
pub fn memory_command(arg: &str, runtime: &crate::Runtime) -> CommandResult {
    use crate::runtime::memory_context::{mint_explicit_command_proof, MemoryContextMode};
    let enable = |mode: MemoryContextMode| {
        match runtime.memory_context_enable(mode, mint_explicit_command_proof()) {
            Ok(status) => CommandResult::Output(status.render()),
            Err(e) => CommandResult::Error(e.to_string()),
        }
    };
    match arg.trim() {
        "on" => enable(MemoryContextMode::CaptureAndRecall),
        "recall" => enable(MemoryContextMode::RecallEachPrompt),
        "capture" => enable(MemoryContextMode::CaptureOnly),
        "once" => match runtime.memory_context_recall_once(mint_explicit_command_proof()) {
            Ok(status) => CommandResult::Output(status.render()),
            Err(e) => CommandResult::Error(e.to_string()),
        },
        "" | "status" => CommandResult::Output(runtime.memory_context_status().render()),
        // Disable is applied to session state BEFORE this returns: `render`
        // runs on the post-revocation status snapshot.
        "off" => CommandResult::Output(runtime.memory_context_disable().render()),
        "index-history" => CommandResult::Error(
            "index-history requires a separate disclosure/consent flow, \
             not yet implemented (task D1)"
                .to_string(),
        ),
        "why" => CommandResult::Error(
            "/memory why requires per-turn recall metadata, not yet implemented (task B5)"
                .to_string(),
        ),
        other => CommandResult::Error(format!(
            "unknown /memory subcommand: `{other}` \
             (try: on, recall, capture, once, status, off, index-history, why)"
        )),
    }
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

    // ── Task 12: /trace and /context surfaces ──

    #[test]
    fn trace_next_enables_exactly_the_next_request_then_auto_disables() {
        let runtime = crate::Runtime::new_headless();
        // Telemetry off: the base session context is disabled.
        assert!(!runtime.trace_context().enabled());

        match trace_command("next", &runtime) {
            CommandResult::Output(text) => assert!(text.contains("next request")),
            other => panic!("expected Output, got {other:?}"),
        }
        let status = runtime.trace_status();
        assert!(!status.persistent_enabled);
        assert_eq!(status.arm, crate::runtime::trace::TraceArm::NextMetadata);

        // Exactly one effective context is enabled, even with telemetry Off…
        let armed = runtime.effective_trace_context();
        assert!(armed.enabled(), "armed context must trace the next request");
        // …then the arm auto-disables: the following request is untraced.
        let after = runtime.effective_trace_context();
        assert!(!after.enabled(), "arm must cover exactly one request");
        assert_eq!(
            runtime.trace_status().arm,
            crate::runtime::trace::TraceArm::Off
        );
    }

    #[test]
    fn trace_status_reports_mode_path_and_counters_without_secrets() {
        let runtime = crate::Runtime::new_headless();
        match trace_command("status", &runtime) {
            CommandResult::Output(text) => {
                assert!(text.contains("trace: disabled"), "got: {text}");
                assert!(text.contains("degraded"));
            }
            other => panic!("expected Output, got {other:?}"),
        }
        runtime.trace_arm_next(true);
        match trace_command("", &runtime) {
            CommandResult::Output(text) => {
                assert!(text.contains("armed for next request"), "got: {text}");
                assert!(text.contains("content capture"));
            }
            other => panic!("expected Output, got {other:?}"),
        }
        assert!(matches!(
            trace_command("bogus", &runtime),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn context_command_reports_metadata_never_content() {
        let mut runtime = crate::Runtime::new_headless();
        let secret = "CONTEXT-SENTINEL-5a5a";
        runtime.set_system_prompt(format!("you are {secret}"));
        let history = vec![std::sync::Arc::new(serde_json::json!({
            "role": "user", "content": secret
        }))];
        match context_command(&runtime, Some(&history)) {
            CommandResult::Output(text) => {
                assert!(!text.contains(secret), "sentinel leaked: {text}");
                assert!(text.contains("history: 1 messages"));
                assert!(text.contains("system prompt:"));
                assert!(text.contains("skills: unavailable"));
            }
            other => panic!("expected Output, got {other:?}"),
        }
        // The engine-shared path (no history) reports honestly.
        match handle_engine_command("context", "", &mut runtime) {
            Some(CommandResult::Output(text)) => {
                assert!(
                    text.contains("history: unavailable messages"),
                    "got: {text}"
                );
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }

    // ── /memory (task A5, spec §7.3) ────────────────────────────────────────

    use crate::runtime::memory_context::{
        DurableStatus, MemoryContextMode, OneShotStatus, UserIntentProof,
    };

    fn output_text(result: CommandResult) -> String {
        match result {
            CommandResult::Output(text) => text,
            other => panic!("expected Output, got {other:?}"),
        }
    }

    /// Spec §7.3: every enabling arg form transitions the session state to
    /// its exact mode — `on` → CaptureAndRecall, `recall` →
    /// RecallEachPrompt, `capture` → CaptureOnly.
    #[test]
    fn memory_enable_args_transition_to_exact_modes() {
        for (arg, mode) in [
            ("on", MemoryContextMode::CaptureAndRecall),
            ("recall", MemoryContextMode::RecallEachPrompt),
            ("capture", MemoryContextMode::CaptureOnly),
        ] {
            let runtime = crate::Runtime::new_headless();
            let text = output_text(memory_command(arg, &runtime));
            assert!(
                text.contains(&format!("mode {}", mode.as_str())),
                "/memory {arg} output must name the mode; got: {text}"
            );
            match runtime.memory_context_status().durable {
                DurableStatus::Active { mode: active, .. } => assert_eq!(
                    active, mode,
                    "/memory {arg} must install exactly {mode:?}"
                ),
                DurableStatus::Off => panic!("/memory {arg} must install a session lease"),
            }
        }
    }

    /// `/memory once` installs a pending one-shot recall and leaves the
    /// durable slot untouched.
    #[test]
    fn memory_once_installs_pending_one_shot_only() {
        let runtime = crate::Runtime::new_headless();
        let text = output_text(memory_command("once", &runtime));
        assert!(text.contains("one-shot recall: pending"), "got: {text}");
        let status = runtime.memory_context_status();
        assert_eq!(status.durable, DurableStatus::Off, "durable slot untouched");
        assert!(matches!(status.one_shot, OneShotStatus::Pending { .. }));
        // A second grant while one is pending fails typed, not silently.
        assert!(matches!(
            memory_command("once", &runtime),
            CommandResult::Error(e) if e.contains("already pending")
        ));
    }

    /// Rendering: `/memory status` (and the empty default) mentions the
    /// mode and the lease expiry.
    #[test]
    fn memory_status_render_mentions_mode_and_lease_expiry() {
        let runtime = crate::Runtime::new_headless();
        // Off default (spec §21): status names the off mode.
        let text = output_text(memory_command("status", &runtime));
        assert!(text.contains("mode off"), "got: {text}");

        output_text(memory_command("on", &runtime));
        for arg in ["status", ""] {
            let text = output_text(memory_command(arg, &runtime));
            assert!(
                text.contains("mode capture_and_recall"),
                "status must mention the mode; got: {text}"
            );
            assert!(
                text.contains("expiry"),
                "status must mention the lease expiry; got: {text}"
            );
        }
    }

    /// `/memory off` revokes session state BEFORE returning: the runtime
    /// state check and the returned render both show Off.
    #[test]
    fn memory_off_takes_effect_before_returning() {
        let runtime = crate::Runtime::new_headless();
        output_text(memory_command("on", &runtime));
        assert!(matches!(
            runtime.memory_context_status().durable,
            DurableStatus::Active { .. }
        ));
        let text = output_text(memory_command("off", &runtime));
        // State check: revocation already applied when the command returned.
        assert_eq!(runtime.memory_context_status().durable, DurableStatus::Off);
        // The returned render is the post-revocation snapshot.
        assert!(text.contains("mode off"), "got: {text}");
    }

    /// Spec §6.3 / assumption 5: every enable path grants under host-minted
    /// `ExplicitCommand` proof — never model-supplied text.
    #[test]
    fn memory_enable_always_uses_explicit_command_proof() {
        for arg in ["on", "recall", "capture"] {
            let runtime = crate::Runtime::new_headless();
            output_text(memory_command(arg, &runtime));
            assert!(
                matches!(
                    runtime.memory_durable_proof_for_test(),
                    Some(UserIntentProof::ExplicitCommand { .. })
                ),
                "/memory {arg} must grant under ExplicitCommand proof"
            );
        }
        let runtime = crate::Runtime::new_headless();
        output_text(memory_command("once", &runtime));
        assert!(
            matches!(
                runtime.memory_one_shot_proof_for_test(),
                Some(UserIntentProof::ExplicitCommand { .. })
            ),
            "/memory once must grant under ExplicitCommand proof"
        );
    }

    /// Unknown args are a typed error naming the valid forms.
    #[test]
    fn memory_unknown_arg_is_typed_error() {
        let runtime = crate::Runtime::new_headless();
        match memory_command("bogus", &runtime) {
            CommandResult::Error(e) => {
                assert!(e.contains("unknown /memory subcommand"), "got: {e}");
                assert!(e.contains("index-history"), "hint lists forms; got: {e}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(runtime.memory_context_status().durable, DurableStatus::Off);
    }

    /// `index-history` (task D1) and `why` (task B5) are typed
    /// not-yet-implemented errors — never silent no-ops — and mutate no
    /// session state.
    #[test]
    fn memory_index_history_and_why_are_typed_not_yet_errors() {
        let runtime = crate::Runtime::new_headless();
        match memory_command("index-history", &runtime) {
            CommandResult::Error(e) => {
                assert!(e.contains("disclosure/consent"), "got: {e}");
                assert!(e.contains("task D1"), "got: {e}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match memory_command("why", &runtime) {
            CommandResult::Error(e) => {
                assert!(e.contains("per-turn recall metadata"), "got: {e}");
                assert!(e.contains("task B5"), "got: {e}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let status = runtime.memory_context_status();
        assert_eq!(status.durable, DurableStatus::Off);
        assert_eq!(status.one_shot, OneShotStatus::Idle);
    }

    /// All frontends route `/memory` through the single
    /// `handle_engine_command` dispatch point (spec §7.3: one engine API).
    #[test]
    fn memory_dispatches_through_handle_engine_command() {
        let mut runtime = crate::Runtime::new_headless();
        match handle_engine_command("memory", "on", &mut runtime) {
            Some(CommandResult::Output(text)) => {
                assert!(text.contains("mode capture_and_recall"), "got: {text}");
            }
            other => panic!("expected Some(Output), got {other:?}"),
        }
        match handle_engine_command("memory", "", &mut runtime) {
            Some(CommandResult::Output(text)) => {
                assert!(text.contains("mode capture_and_recall"), "got: {text}");
                assert!(text.contains("expiry"), "got: {text}");
            }
            other => panic!("expected Some(Output), got {other:?}"),
        }
    }
}
