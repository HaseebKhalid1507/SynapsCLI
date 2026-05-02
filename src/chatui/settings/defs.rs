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
        "Legacy setting retained for config compatibility; requested bash timeouts are no longer clamped.",
        |runtime, _app, value| {
            if let Ok(n) = value.parse::<u64>() { runtime.set_bash_max_timeout(n); }
        };

    theme, "Theme", Appearance, EditorKind::ThemePicker,
        "Color theme (restart required).",
        |_runtime, _app, _value| { /* handled after write_config_value in apply_setting() */ };

    voice_toggle_key, "Voice toggle key", Voice,
        EditorKind::Cycler(&["F8", "F2", "F12", "C-V", "C-G"]),
        "Keybind that toggles voice dictation. Takes effect immediately.",
        |_runtime, app, value| {
            if let Some(kb) = app.keybinds.as_ref() {
                match kb.write() {
                    Ok(mut g) => {
                        if let Err(e) = g.set_slash_command_key("voice toggle", value) {
                            tracing::warn!("voice_toggle_key apply failed: {}", e);
                        }
                    }
                    Err(_) => tracing::warn!("voice_toggle_key apply: registry poisoned"),
                }
            }
        };

    voice_language, "Voice language", Voice,
        EditorKind::Cycler(&[
            "auto", "en", "es", "fr", "de", "it", "pt", "nl", "ja", "zh", "ko", "ar", "hi", "ru",
        ]),
        "Spoken language passed to the voice sidecar. 'auto' lets whisper detect.",
        |_runtime, _app, _value| { /* read by VoiceUiState::spawn_default */ };

    voice_stt_model, "Voice STT model", Voice,
        EditorKind::ModelBrowser,
        "Whisper model used for transcription. Browse the catalog — \
         installed models are marked, uninstalled rows trigger a download.",
        |_runtime, _app, _value| { /* read by VoiceUiState::spawn_default via voice_stt_model_path */ };

    voice_stt_backend, "Voice STT backend", Voice,
        EditorKind::Cycler(&["auto", "cpu", "cuda", "metal", "vulkan", "openblas"]),
        "Whisper compute backend. Selecting a different backend stages a \
         rebuild — run `/voice rebuild` to apply. 'auto' picks based on \
         detected hardware.",
        |_runtime, _app, _value| { /* effect deferred to rebuild action — see voice/rebuild.rs */ };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_backend_setting_in_voice_category() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "voice_stt_backend")
            .expect("voice_stt_backend setting should be defined");
        assert_eq!(def.category, Category::Voice);
        match def.editor {
            EditorKind::Cycler(opts) => {
                assert!(opts.contains(&"auto"));
                assert!(opts.contains(&"cpu"));
                assert!(opts.contains(&"cuda"));
                assert!(opts.contains(&"metal"));
                assert!(opts.contains(&"vulkan"));
                assert!(opts.contains(&"openblas"));
            }
            _ => panic!("expected Cycler editor for voice_stt_backend"),
        }
    }
}
