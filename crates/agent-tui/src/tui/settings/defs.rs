//! Single source of truth for every tweakable setting.
//!
//! One macro invocation generates both the UI schema (`ALL_SETTINGS`) and the
//! runtime apply dispatch (`apply_setting_dispatch`). Add a setting here and
//! both sides stay in sync — drift is impossible.

use super::schema::{Category, EditorKind, SettingDef};

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

        /// Apply a setting by key/value and return `Ok(())` on success or
        /// `Err(user-facing message)` on validation failure.  Handlers that
        /// do not perform validation always return `Ok(())`.  On `Err` the
        /// caller must NOT write the value to the config file.
        pub(crate) fn apply_setting_dispatch(
            key: &str,
            value: &str,
            runtime: &mut synaps_cli::Runtime,
            app: &mut crate::tui::app::App,
        ) -> Result<(), String> {
            match key {
                $(
                    stringify!($key) => {
                        let handler: fn(&mut synaps_cli::Runtime, &mut crate::tui::app::App, &str) -> Result<(), String> = $apply;
                        handler(runtime, app, value)
                    }
                )*
                _ => Ok(())
            }
        }
    };
}

define_settings! {
    model, "Model", Model, EditorKind::ModelPicker,
        "Which Claude model to use.",
        |runtime, _app, value| { runtime.set_model(value.to_string()); Ok(()) };

    thinking, "Thinking", Model,
        EditorKind::DynamicCycler,
        "Thinking depth — controls effort on adaptive models, budget on legacy.",
        |runtime, _app, value| {
            use agent_core::reasoning::ReasoningLevel;
            if let Some(level) = ReasoningLevel::parse(value) {
                // Validate against capability cache/static before mutating.
                // On Err: return the error so apply_setting can skip config write.
                runtime.set_reasoning_level_checked(level)
                    .map_err(|msg| msg)
            } else {
                // Unknown string — ignore silently (cycler only emits valid strings).
                Ok(())
            }
        };

    context_window, "Context window", Model,
        EditorKind::Cycler(&["200k", "1m", "auto"]),
        "Override context window limit (auto = model default).",
        |runtime, app, value| {
            let window = match value {
                "200k" | "200K" => Some(200_000u64),
                "1m" | "1M" => Some(1_000_000u64),
                "auto" => None,
                _ => return Ok(()),
            };
            runtime.set_context_window(window);
            // Also update the bar denominator immediately so the UI reflects the change.
            app.last_turn_context_window = runtime.context_window();
            Ok(())
        };

    compaction_model, "Compaction model", Model,
        EditorKind::ModelPicker,
        "Model used for /compact (default: claude-sonnet-4-6).",
        |runtime, _app, value| {
            let model = if value.is_empty() || value == "auto" || value == "default" {
                None
            } else {
                Some(value.to_string())
            };
            runtime.set_compaction_model(model);
            Ok(())
        };

    api_retries, "API retries", Agent, EditorKind::Text { numeric: true },
        "Retries on transient API errors.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u32>() { runtime.set_api_retries(n); }
            Ok(())
        };

    subagent_timeout, "Subagent timeout", Agent, EditorKind::Text { numeric: true },
        "Seconds before a dispatched subagent is canceled.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_subagent_timeout(n); }
            Ok(())
        };

    max_tool_output, "Max tool output", ToolLimits, EditorKind::Text { numeric: true },
        "Bytes to capture from a tool before truncating.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<usize>() { runtime.set_max_tool_output(n); }
            Ok(())
        };

    bash_timeout, "Bash timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Default seconds allowed for a bash command.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_bash_timeout(n); }
            Ok(())
        };

    bash_max_timeout, "Bash max timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Legacy setting retained for config compatibility; requested bash timeouts are no longer clamped.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_bash_max_timeout(n); }
            Ok(())
        };

    theme, "Theme", Appearance, EditorKind::ThemePicker,
        "Color theme (restart required).",
        |_runtime, _app, _value| { /* handled after write_config_value in apply_setting() */ Ok(()) };

    sidecar_toggle_key, "Sidecar toggle key", Sidecar,
        EditorKind::Cycler(&["F8", "F2", "F12", "C-V", "C-G"]),
        "Keybind that toggles the active sidecar plugin. Takes effect immediately.",
        |_runtime, app, value| {
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
            Ok(())
        };
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // B2: apply_setting_dispatch returns Result; rejected thinking must not mutate.
    #[test]
    fn thinking_dispatch_rejects_unsupported_level_returns_err_no_mutation() {
        let mut rt = synaps_cli::Runtime::new_headless();
        rt.set_model("openai-codex/gpt-5.6-luna".to_string()); // luna: no ultra
        let before = rt.reasoning_level();
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("m", "medium", None));

        let result = apply_setting_dispatch("thinking", "ultra", &mut rt, &mut app);
        assert!(result.is_err(), "ultra must be rejected for luna");
        assert_eq!(
            rt.reasoning_level(), before,
            "runtime must not be mutated when dispatch returns Err"
        );
    }

    #[test]
    fn thinking_dispatch_accepts_valid_level_returns_ok_and_mutates() {
        let mut rt = synaps_cli::Runtime::new_headless();
        rt.set_model("openai-codex/gpt-5.6-sol".to_string()); // sol: supports ultra
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("m", "medium", None));

        let result = apply_setting_dispatch("thinking", "ultra", &mut rt, &mut app);
        assert!(result.is_ok(), "ultra must be accepted for sol");
        assert_eq!(
            rt.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::Ultra,
            "runtime must be updated when dispatch returns Ok"
        );
    }

    #[test]
    fn non_thinking_dispatches_always_return_ok() {
        let mut rt = synaps_cli::Runtime::new_headless();
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("m", "medium", None));
        // model, api_retries, etc. return Ok(()) unconditionally.
        assert!(apply_setting_dispatch("model", "claude-opus-4-7", &mut rt, &mut app).is_ok());
        assert!(apply_setting_dispatch("api_retries", "5", &mut rt, &mut app).is_ok());
        assert!(apply_setting_dispatch("bash_timeout", "30", &mut rt, &mut app).is_ok());
        assert!(apply_setting_dispatch("unknown_key_xyz", "val", &mut rt, &mut app).is_ok());
    }
}
