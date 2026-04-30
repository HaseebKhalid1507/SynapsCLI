#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommandAction {
    Dictation(String),
    SlashCommand { command: String, arg: String },
    Submit,
    Escape,
    NewLine,
}

#[derive(Debug, Clone, Copy)]
pub struct VoiceCommandConfig {
    pub commands_enabled: bool,
    pub submit_enabled: bool,
    pub escape_enabled: bool,
}

impl VoiceCommandConfig {
    pub fn dictation_only() -> Self {
        Self { commands_enabled: false, submit_enabled: false, escape_enabled: false }
    }
}

pub fn map_spoken_phrase(phrase: &str, config: VoiceCommandConfig) -> VoiceCommandAction {
    let trimmed = phrase.trim();
    if trimmed.is_empty() || !config.commands_enabled {
        return VoiceCommandAction::Dictation(trimmed.to_string());
    }

    let normalized = normalize_phrase(trimmed);
    if normalized == "new line" || normalized == "newline" {
        return VoiceCommandAction::NewLine;
    }
    if normalized == "submit" || normalized == "send" || normalized == "send it" {
        return if config.submit_enabled || normalized == "send" || normalized == "send it" {
            VoiceCommandAction::Submit
        } else {
            VoiceCommandAction::Dictation(trimmed.to_string())
        };
    }
    if normalized == "escape" || normalized == "cancel" {
        return if config.escape_enabled {
            VoiceCommandAction::Escape
        } else {
            VoiceCommandAction::Dictation(trimmed.to_string())
        };
    }

    if let Some(rest) = normalized.strip_prefix("slash ") {
        return match rest {
            "settings" => VoiceCommandAction::SlashCommand { command: "settings".into(), arg: String::new() },
            "model" | "models" => VoiceCommandAction::SlashCommand { command: "models".into(), arg: String::new() },
            "compact" => VoiceCommandAction::SlashCommand { command: "compact".into(), arg: String::new() },
            "status" => VoiceCommandAction::SlashCommand { command: "status".into(), arg: String::new() },
            "plugins" => VoiceCommandAction::SlashCommand { command: "plugins".into(), arg: String::new() },
            _ => VoiceCommandAction::Dictation(trimmed.to_string()),
        };
    }

    VoiceCommandAction::Dictation(trimmed.to_string())
}

fn normalize_phrase(input: &str) -> String {
    input
        .trim()
        .trim_matches(|c: char| matches!(c, '.' | ',' | '!' | '?' | ':' | ';'))
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(submit_enabled: bool) -> VoiceCommandConfig {
        VoiceCommandConfig { commands_enabled: true, submit_enabled, escape_enabled: false }
    }

    #[test]
    fn slash_settings_maps_to_exact_slash_command() {
        assert_eq!(
            map_spoken_phrase("slash settings", cfg(false)),
            VoiceCommandAction::SlashCommand { command: "settings".into(), arg: String::new() }
        );
    }

    #[test]
    fn slash_model_and_models_open_model_flow() {
        assert_eq!(
            map_spoken_phrase("Slash model.", cfg(false)),
            VoiceCommandAction::SlashCommand { command: "models".into(), arg: String::new() }
        );
        assert_eq!(
            map_spoken_phrase("slash models", cfg(false)),
            VoiceCommandAction::SlashCommand { command: "models".into(), arg: String::new() }
        );
    }

    #[test]
    fn submit_is_dictation_unless_submit_is_enabled() {
        assert_eq!(map_spoken_phrase("submit", cfg(false)), VoiceCommandAction::Dictation("submit".into()));
        assert_eq!(map_spoken_phrase("submit", cfg(true)), VoiceCommandAction::Submit);
    }

    #[test]
    fn send_and_send_it_submit_without_extra_config() {
        assert_eq!(map_spoken_phrase("send", cfg(false)), VoiceCommandAction::Submit);
        assert_eq!(map_spoken_phrase("send it", cfg(false)), VoiceCommandAction::Submit);
        assert_eq!(map_spoken_phrase("Send it.", cfg(false)), VoiceCommandAction::Submit);
    }

    #[test]
    fn near_matches_and_ambiguous_phrases_remain_dictation() {
        assert_eq!(map_spoken_phrase("slash setting", cfg(false)), VoiceCommandAction::Dictation("slash setting".into()));
        assert_eq!(map_spoken_phrase("open settings", cfg(false)), VoiceCommandAction::Dictation("open settings".into()));
    }

    #[test]
    fn commands_disabled_treats_everything_as_dictation() {
        assert_eq!(
            map_spoken_phrase("slash settings", VoiceCommandConfig::dictation_only()),
            VoiceCommandAction::Dictation("slash settings".into())
        );
    }

    #[test]
    fn new_line_maps_to_text_insertion_not_submit() {
        assert_eq!(map_spoken_phrase("new line", cfg(false)), VoiceCommandAction::NewLine);
    }

    #[test]
    fn escape_is_dictation_unless_escape_is_explicitly_enabled() {
        assert_eq!(map_spoken_phrase("escape", cfg(false)), VoiceCommandAction::Dictation("escape".into()));
        let config = VoiceCommandConfig { commands_enabled: true, submit_enabled: false, escape_enabled: true };
        assert_eq!(map_spoken_phrase("cancel", config), VoiceCommandAction::Escape);
    }
}
