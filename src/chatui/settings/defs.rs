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

        pub(crate) fn apply_setting_dispatch(
            key: &str,
            value: &str,
            runtime: &mut synaps_cli::Runtime,
            app: &mut crate::chatui::app::App,
        ) {
            match key {
                $(
                    stringify!($key) => {
                        let handler: fn(&mut synaps_cli::Runtime, &mut crate::chatui::app::App, &str) = $apply;
                        handler(runtime, app, value);
                    }
                )*
                _ => {}
            }
        }
    };
}

define_settings! {
    model, "Model", Model, EditorKind::ModelPicker,
        "Which Claude model to use.",
        |runtime, _app, value| { runtime.set_model(value.to_string()); };

    thinking, "Thinking", Model,
        EditorKind::Cycler(&["low", "medium", "high", "xhigh", "adaptive"]),
        "Thinking depth — controls effort on adaptive models, budget on legacy.",
        |runtime, _app, value| {
            let budget = match value {
                "low" => 2048,
                "medium" => 4096,
                "high" => 16384,
                "xhigh" => 32768,
                "adaptive" => 0,
                _ => return,
            };
            runtime.set_thinking_budget(budget);
        };

    context_window, "Context window", Model,
        EditorKind::Cycler(&["200k", "1m", "auto"]),
        "Override context window limit (auto = model default).",
        |runtime, app, value| {
            let window = match value {
                "200k" | "200K" => Some(200_000u64),
                "1m" | "1M" => Some(1_000_000u64),
                "auto" => None,
                _ => return,
            };
            runtime.set_context_window(window);
            // Also update the bar denominator immediately so the UI reflects the change.
            app.last_turn_context_window = runtime.context_window();
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
        };

    api_retries, "API retries", Agent, EditorKind::Text { numeric: true },
        "Retries on transient API errors.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u32>() { runtime.set_api_retries(n); }
        };

    subagent_timeout, "Subagent timeout", Agent, EditorKind::Text { numeric: true },
        "Seconds before a dispatched subagent is canceled.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_subagent_timeout(n); }
        };

    max_tool_output, "Max tool output", ToolLimits, EditorKind::Text { numeric: true },
        "Bytes to capture from a tool before truncating.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<usize>() { runtime.set_max_tool_output(n); }
        };

    bash_timeout, "Bash timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Default seconds allowed for a bash command.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_bash_timeout(n); }
        };

    bash_max_timeout, "Bash max timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Upper bound on requested bash timeouts.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_bash_max_timeout(n); }
        };

    theme, "Theme", Appearance, EditorKind::ThemePicker,
        "Color theme (restart required).",
        |_runtime, _app, _value| { /* handled after write_config_value in apply_setting() */ };
}
