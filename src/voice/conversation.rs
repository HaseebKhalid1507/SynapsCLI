use super::commands::{map_spoken_phrase, VoiceCommandAction, VoiceCommandConfig};

pub fn transcript_to_conversation_action(transcript: &str, config: VoiceCommandConfig) -> VoiceCommandAction {
    let mapped = map_spoken_phrase(transcript, config);
    match mapped {
        VoiceCommandAction::SlashCommand { .. } | VoiceCommandAction::Escape | VoiceCommandAction::NewLine => mapped,
        VoiceCommandAction::Submit | VoiceCommandAction::Dictation(_) => VoiceCommandAction::Submit,
    }
}
