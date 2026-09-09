//! Single source of truth for every tweakable setting.
//!
//! One macro invocation generates both the UI schema (`ALL_SETTINGS`) and the
//! apply dispatch (`apply_setting_dispatch`). Add a setting here and both
//! sides stay in sync — drift is impossible.
//!
//! Runtime-backed keys produce a [`SettingApply::Session`] (the
//! `SessionSetting` the caller sends as `Set{id, ..}`); local keys apply
//! synchronously and yield [`SettingApply::Local`].

use super::schema::{Category, EditorKind, SettingDef};
use agent_engine::session::SessionSetting;

/// What a settings apply resolves to (PLAN-phase3 §3.2 settings/defs).
pub(crate) enum SettingApply {
    /// Send this to the session; the reply decides the config write.
    Session(SessionSetting),
    /// Applied locally (theme, keybinds, …) or a no-op; `Err` = rejected
    /// (the caller must NOT write the value to the config file).
    Local(Result<(), String>),
}

macro_rules! define_settings {
    ($(
        $key:ident, $label:expr, $category:ident, $editor:expr, $help:expr,
            $apply:expr;
    )*) => {
        pub(crate) const ALL_SETTINGS: &[SettingDef] = &[
            $(
                SettingDef {
                    key: stringify!($key),
                    label: $label,
                    category: Category::$category,
                    editor: $editor,
                    help: $help,
                },
            )*
        ];

        /// Resolve a setting by key/value: a `SessionSetting` to send, or the
        /// local outcome. Handlers that do not perform validation always
        /// yield `Local(Ok(()))`. On `Local(Err)` the caller must NOT write
        /// the value to the config file.
        pub(crate) fn apply_setting_dispatch(
            key: &str,
            value: &str,
            app: &mut crate::tui::app::App,
        ) -> SettingApply {
            match key {
                $(
                    stringify!($key) => {
                        let handler: fn(&mut crate::tui::app::App, &str) -> SettingApply = $apply;
                        handler(app, value)
                    }
                )*
                _ => SettingApply::Local(Ok(()))
            }
        }
    };
}

define_settings! {
    model, "Model", Model, EditorKind::ModelPicker,
        "Which Claude model to use.",
        |_app, value| SettingApply::Session(SessionSetting::Model { model: value.to_string() });

    thinking, "Thinking", Model,
        EditorKind::DynamicCycler,
        "Thinking depth — controls effort on adaptive models, budget on legacy.",
        |app, value| {
            use agent_core::reasoning::ReasoningLevel;
            if let Some(level) = ReasoningLevel::parse(value) {
                // Validate against capability cache/static before sending
                // (client-local pre-check; the actor re-validates). On Err:
                // return the error so apply_setting can skip config write.
                if let Err(e) = synaps_cli::runtime::openai::catalog::validation::validate_reasoning_mutation(
                    &app.session.model, level,
                ) {
                    return SettingApply::Local(Err(e));
                }
                SettingApply::Session(SessionSetting::ReasoningLevel { level })
            } else {
                // Unknown string — ignore silently (cycler only emits valid strings).
                SettingApply::Local(Ok(()))
            }
        };

    reasoning_type, "Reasoning", Model,
        EditorKind::Display,
        "How the active model expresses reasoning depth (derived from exact model capabilities; read-only).",
        |_app, _value| { /* display-only: no editor emits Apply for this key */ SettingApply::Local(Ok(())) };

    context_window, "Context window", Model,
        EditorKind::Cycler(&["200k", "1m", "auto"]),
        "Override context window limit (auto = model default).",
        |_app, value| {
            let window = match value {
                "200k" | "200K" => Some(200_000u64),
                "1m" | "1M" => Some(1_000_000u64),
                "auto" => None,
                _ => return SettingApply::Local(Ok(())),
            };
            // The bar denominator (`last_turn_context_window`) follows the
            // reply's view in apply_setting().
            SettingApply::Session(SessionSetting::ContextWindow { tokens: window })
        };

    compaction_model, "Compaction model", Model,
        EditorKind::ModelPicker,
        "Model used for /compact (default: claude-sonnet-4-6).",
        |_app, value| {
            let model = if value.is_empty() || value == "auto" || value == "default" {
                None
            } else {
                Some(value.to_string())
            };
            SettingApply::Session(SessionSetting::CompactionModel { model })
        };

    api_retries, "API retries", Agent, EditorKind::Text { numeric: true },
        "Retries on transient API errors.",
        |_app, value| match value.parse::<u32>() {
            Ok(n) => SettingApply::Session(SessionSetting::ApiRetries { n }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    subagent_timeout, "Subagent timeout", Agent, EditorKind::Text { numeric: true },
        "Seconds before a dispatched subagent is canceled.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::SubagentTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    max_tool_output, "Max tool output", ToolLimits, EditorKind::Text { numeric: true },
        "Bytes to capture from a tool before truncating.",
        |_app, value| match value.parse::<usize>() {
            Ok(bytes) => SettingApply::Session(SessionSetting::MaxToolOutput { bytes }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    bash_timeout, "Bash timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Default seconds allowed for a bash command.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::BashTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    bash_max_timeout, "Bash max timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Legacy setting retained for config compatibility; requested bash timeouts are no longer clamped.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::BashMaxTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    theme, "Theme", Appearance, EditorKind::ThemePicker,
        "Color theme (restart required).",
        |_app, _value| { /* handled after write_config_value in apply_setting() */ SettingApply::Local(Ok(())) };

    tui_background_opaque, "Background", Appearance, EditorKind::Cycler(&["opaque", "invisible"]),
        "Opaque paints Synaps' theme background; invisible uses your terminal background.",
        |_app, value| {
            match value {
                "opaque" => super::super::theme::set_background_opaque(true),
                "invisible" => super::super::theme::set_background_opaque(false),
                _ => return SettingApply::Local(Err("expected opaque or invisible".to_string())),
            }
            SettingApply::Local(Ok(()))
        };

    theme_transition, "Theme transition", Appearance, EditorKind::Cycler(&["on", "off"]),
        "Animated cross-fade on theme changes (on = 350ms). Off = instant snap. Integer ms (0-2000) accepted in the config file.",
        |_app, value| {
            match synaps_cli::config::ThemeTransitionMode::parse(value) {
                Some(mode) => {
                    super::super::theme::transition::set_transition_mode(mode);
                    SettingApply::Local(Ok(()))
                }
                None => SettingApply::Local(Err("expected on, off, or milliseconds (0-2000)".to_string())),
            }
        };

    sidecar_toggle_key, "Sidecar toggle key", Sidecar,
        EditorKind::Cycler(&["F8", "F2", "F12", "C-V", "C-G"]),
        "Keybind that toggles the active sidecar plugin. Takes effect immediately.",
        |app, value| {
            if let Some(kb) = app.keybinds.as_ref() {
                match kb.write() {
                    Ok(mut g) => {
                        if let Err(e) = g.set_slash_command_key("sidecar toggle", value) {
                            tracing::warn!("sidecar_toggle_key apply failed: {}", e);
                        }
                    }
                    Err(_) => tracing::warn!("sidecar_toggle_key apply: registry poisoned"),
                }
            }
            SettingApply::Local(Ok(()))
        };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_transition_setting_is_appearance_cycler() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "theme_transition")
            .expect("theme_transition setting should be defined");
        assert_eq!(def.category, Category::Appearance);
        match def.editor {
            EditorKind::Cycler(opts) => assert_eq!(opts, &["on", "off"]),
            _ => panic!("theme_transition editor should be a Cycler"),
        }
    }

    #[test]
    fn sidecar_toggle_key_setting_is_in_sidecar_category() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "sidecar_toggle_key")
            .expect("sidecar_toggle_key setting should be defined");
        assert_eq!(def.category, Category::Sidecar);
    }

    #[test]
    fn sidecar_toggle_key_static_setting_still_defined_for_backward_compat() {
        // Phase 8 slice 8A.4: even after `visible_categories(claims)`
        // hides the global Sidecar page when a plugin claims its own
        // settings_category, the static def stays in ALL_SETTINGS so
        // legacy users without claimed plugins keep a working toggle
        // and config round-trips remain stable.
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "sidecar_toggle_key")
            .expect("sidecar_toggle_key setting must remain in ALL_SETTINGS for back-compat");
        assert_eq!(def.category, Category::Sidecar);
        match def.editor {
            EditorKind::Cycler(opts) => {
                assert!(opts.contains(&"F8"));
                assert!(opts.contains(&"C-V"));
            }
            _ => panic!("expected Cycler editor for sidecar_toggle_key"),
        }
    }

    fn app_with_model(model: &str) -> crate::tui::app::App {
        crate::tui::app::App::new(synaps_cli::Session::new(model, "medium", None))
    }

    // Rejected thinking never becomes a `Set` (the config write is skipped).
    #[test]
    fn thinking_dispatch_rejects_unsupported_level_returns_err_no_set() {
        let mut app = app_with_model("openai-codex/gpt-5.6-luna"); // luna: no ultra
        match apply_setting_dispatch("thinking", "ultra", &mut app) {
            SettingApply::Local(Err(_)) => {}
            _ => panic!("ultra must be rejected for luna before any Set"),
        }
    }

    #[test]
    fn thinking_dispatch_accepts_valid_level_yields_set() {
        let mut app = app_with_model("openai-codex/gpt-5.6-sol"); // sol: supports ultra
        match apply_setting_dispatch("thinking", "ultra", &mut app) {
            SettingApply::Session(SessionSetting::ReasoningLevel { level }) => {
                assert_eq!(level, agent_core::reasoning::ReasoningLevel::Ultra)
            }
            _ => panic!("ultra must be accepted for sol"),
        }
    }

    #[test]
    fn thinking_dispatch_rejects_off_on_xai_45_no_set() {
        let mut app = app_with_model("xai-auth/grok-4.5");
        // Off must be rejected (reasoning cannot be disabled), never omitted.
        assert!(matches!(
            apply_setting_dispatch("thinking", "off", &mut app),
            SettingApply::Local(Err(_))
        ));
        // xhigh is not documented for grok-4.5 either.
        assert!(matches!(
            apply_setting_dispatch("thinking", "xhigh", &mut app),
            SettingApply::Local(Err(_))
        ));
        // A documented effort is accepted.
        assert!(matches!(
            apply_setting_dispatch("thinking", "low", &mut app),
            SettingApply::Session(SessionSetting::ReasoningLevel {
                level: agent_core::reasoning::ReasoningLevel::Low
            })
        ));
    }

    #[test]
    fn runtime_keys_yield_the_expected_session_setting() {
        let mut app = app_with_model("m");
        assert!(matches!(
            apply_setting_dispatch("model", "claude-opus-4-7", &mut app),
            SettingApply::Session(SessionSetting::Model { model }) if model == "claude-opus-4-7"
        ));
        assert!(matches!(
            apply_setting_dispatch("api_retries", "5", &mut app),
            SettingApply::Session(SessionSetting::ApiRetries { n: 5 })
        ));
        assert!(matches!(
            apply_setting_dispatch("bash_timeout", "30", &mut app),
            SettingApply::Session(SessionSetting::BashTimeout { secs: 30 })
        ));
        assert!(matches!(
            apply_setting_dispatch("context_window", "1m", &mut app),
            SettingApply::Session(SessionSetting::ContextWindow { tokens: Some(1_000_000) })
        ));
        assert!(matches!(
            apply_setting_dispatch("compaction_model", "auto", &mut app),
            SettingApply::Session(SessionSetting::CompactionModel { model: None })
        ));
        // Unparseable numerics and unknown keys are local no-ops.
        assert!(matches!(
            apply_setting_dispatch("api_retries", "x", &mut app),
            SettingApply::Local(Ok(()))
        ));
        assert!(matches!(
            apply_setting_dispatch("unknown_key_xyz", "val", &mut app),
            SettingApply::Local(Ok(()))
        ));
    }
}
