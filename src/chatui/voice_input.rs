use super::app::App;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceTranscriptOutcome {
    Ignored,
    Inserted { submit: bool },
    SlashCommand { command: String, arg: String },
    Submit,
    Escape,
}

#[cfg(test)]
pub(crate) fn insert_voice_transcript(app: &mut App, transcript: &str, max_chars: usize) -> bool {
    matches!(
        handle_voice_transcript(
            app,
            transcript,
            max_chars,
            synaps_cli::VoiceCommandConfig::dictation_only(),
        ),
        VoiceTranscriptOutcome::Inserted { .. }
    )
}

pub(crate) fn handle_voice_transcript(
    app: &mut App,
    transcript: &str,
    max_chars: usize,
    command_config: synaps_cli::VoiceCommandConfig,
) -> VoiceTranscriptOutcome {
    let sanitized = synaps_cli::sanitize_voice_transcript(transcript, max_chars);
    if sanitized.is_empty() {
        return VoiceTranscriptOutcome::Ignored;
    }

    if app.settings.is_none() && app.plugins.is_none() {
        match synaps_cli::map_spoken_phrase(&sanitized, command_config) {
            synaps_cli::VoiceCommandAction::SlashCommand { command, arg } => {
                return VoiceTranscriptOutcome::SlashCommand { command, arg };
            }
            synaps_cli::VoiceCommandAction::Submit => return VoiceTranscriptOutcome::Submit,
            synaps_cli::VoiceCommandAction::Escape => return VoiceTranscriptOutcome::Escape,
            synaps_cli::VoiceCommandAction::NewLine => {
                let byte_pos = app.cursor_byte_pos();
                app.input.insert(byte_pos, '\n');
                app.cursor_pos += 1;
                return VoiceTranscriptOutcome::Inserted { submit: false };
            }
            synaps_cli::VoiceCommandAction::Dictation(_) => {}
        }
    }

    if let Some(settings) = app.settings.as_mut() {
        return if super::settings::insert_voice_text(settings, &sanitized) {
            VoiceTranscriptOutcome::Inserted { submit: false }
        } else {
            VoiceTranscriptOutcome::Ignored
        };
    }

    if let Some(plugins) = app.plugins.as_mut() {
        return if super::plugins::insert_voice_text(plugins, &sanitized) {
            VoiceTranscriptOutcome::Inserted { submit: false }
        } else {
            VoiceTranscriptOutcome::Ignored
        };
    }

    let byte_pos = app.cursor_byte_pos();
    app.input.insert_str(byte_pos, &sanitized);
    app.cursor_pos += sanitized.chars().count();
    VoiceTranscriptOutcome::Inserted { submit: command_config.submit_enabled }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::Session;

    fn app() -> App {
        App::new(Session::new("test-model", "medium", None))
    }

    #[test]
    fn inserts_sanitized_transcript_at_chat_cursor() {
        let mut app = app();
        app.input = "hello world".to_string();
        app.cursor_pos = 5;

        assert!(insert_voice_transcript(&mut app, "\u{1b}[31m voice", 100));

        assert_eq!(app.input, "hello voice world");
        assert_eq!(app.cursor_pos, 11);
    }

    #[test]
    fn ignores_empty_sanitized_transcript() {
        let mut app = app();
        app.input = "hello".to_string();
        app.cursor_pos = 5;

        assert!(!insert_voice_transcript(&mut app, "\u{0000}", 100));

        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor_pos, 5);
    }

    #[test]
    fn inserts_and_marks_plain_dictation_for_auto_submit_when_enabled() {
        let mut app = app();
        let config = synaps_cli::VoiceCommandConfig {
            commands_enabled: true,
            submit_enabled: true,
            escape_enabled: false,
        };

        let outcome = handle_voice_transcript(&mut app, "hello tui", 100, config);

        assert_eq!(outcome, VoiceTranscriptOutcome::Inserted { submit: true });
        assert_eq!(app.input, "hello tui");
        assert_eq!(app.cursor_pos, 9);
    }
}
