#[cfg(test)]
mod voice_conversation_mode_tests {
    use synaps_cli::{VoiceCommandConfig, VoiceCommandAction};

    #[test]
    fn conversation_mode_auto_submits_dictation_transcript() {
        let outcome = synaps_cli::voice::transcript_to_conversation_action(
            "what changed in this file",
            VoiceCommandConfig::dictation_only(),
        );

        assert_eq!(outcome, VoiceCommandAction::Submit);
    }

    #[test]
    fn dictation_mode_keeps_send_command_behavior_separate() {
        let config = VoiceCommandConfig { commands_enabled: true, submit_enabled: true, escape_enabled: false };

        assert_eq!(synaps_cli::map_spoken_phrase("send it", config), VoiceCommandAction::Submit);
        assert!(matches!(
            synaps_cli::map_spoken_phrase("what changed in this file", config),
            VoiceCommandAction::Dictation(_)
        ));
    }
}
