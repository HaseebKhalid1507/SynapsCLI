//! Chatui-side glue for the voice subsystem.
//!
//! Owns the `VoiceUiState` held on `App.voice` and provides helpers
//! the slash-command dispatcher and event loop call into. The actual
//! sidecar lifecycle lives in `crate::voice::manager::VoiceManager`.

use synaps_cli::voice::discovery::{discover, DiscoveredVoiceSidecar};
use synaps_cli::voice::manager::{VoiceManager, VoiceManagerError, VoiceManagerEvent};
use synaps_cli::voice::protocol::{
    SidecarConfig, SidecarProviderState, VoiceSidecarMode, VOICE_SIDECAR_PROTOCOL_VERSION,
};

use super::app::{App, ChatMessage};

/// What the chatui currently shows for the voice indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceUiStatus {
    /// Manager is alive but not currently capturing audio.
    Idle,
    /// Sidecar reported `ListeningStarted`.
    Listening,
    /// Sidecar reported `TranscribingStarted`.
    Transcribing,
    /// Sidecar reported an error; user should `/voice toggle` to retry.
    Error(String),
}

/// State held by the chatui while voice dictation is enabled.
pub(crate) struct VoiceUiState {
    pub manager: VoiceManager,
    pub status: VoiceUiStatus,
    pub sidecar: DiscoveredVoiceSidecar,
}

impl VoiceUiState {
    /// Discover a sidecar from loaded plugins and spawn its manager
    /// with a default dictation-mode handshake.
    ///
    /// Returns `Err` with a user-facing message if no plugin provides
    /// a voice sidecar or the spawn itself fails.
    pub async fn spawn_default() -> Result<Self, String> {
        let sidecar = discover().ok_or_else(|| {
            "no plugin provides a voice sidecar; install the local-voice plugin from synaps-skills"
                .to_string()
        })?;

        if !sidecar.binary.is_file() {
            return Err(format!(
                "voice sidecar binary not found at {} — run the plugin's setup.sh first",
                sidecar.binary.display()
            ));
        }

        let language = synaps_cli::config::read_config_value("voice_language")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let manager = VoiceManager::spawn(
            &sidecar.binary,
            &[],
            SidecarConfig {
                mode: VoiceSidecarMode::Dictation,
                language,
                protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
            },
        )
        .await
        .map_err(|err: VoiceManagerError| format!("failed to start voice sidecar: {}", err))?;

        Ok(Self {
            manager,
            status: VoiceUiStatus::Idle,
            sidecar,
        })
    }

    /// Render a human-readable status line for `/voice status`.
    pub fn status_line(&self) -> String {
        let state = match &self.status {
            VoiceUiStatus::Idle => "idle",
            VoiceUiStatus::Listening => "listening",
            VoiceUiStatus::Transcribing => "transcribing",
            VoiceUiStatus::Error(msg) => return format!("voice: error — {}", msg),
        };
        format!(
            "voice: {} ({}) — sidecar: {}",
            state,
            self.sidecar.plugin_name,
            self.sidecar.binary.display()
        )
    }
}

/// Apply a [`VoiceManagerEvent`] to the chatui state.
///
/// Final transcripts are inserted at the cursor position (with a
/// leading space when the existing input doesn't already end in
/// whitespace), so the user can keep dictating into the same line.
pub(crate) fn handle_event(app: &mut App, event: VoiceManagerEvent) {
    let Some(v) = app.voice.as_mut() else {
        return;
    };
    match event {
        VoiceManagerEvent::Ready { .. } => {
            // Sidecar handshake is informational; we already pressed.
        }
        VoiceManagerEvent::StateChanged(state) => match state {
            SidecarProviderState::Listening => v.status = VoiceUiStatus::Listening,
            SidecarProviderState::Transcribing => v.status = VoiceUiStatus::Transcribing,
            SidecarProviderState::Ready | SidecarProviderState::Stopped => {
                v.status = VoiceUiStatus::Idle
            }
            SidecarProviderState::Error => {
                v.status = VoiceUiStatus::Error("sidecar reported error state".into())
            }
            SidecarProviderState::Speaking => {}
        },
        VoiceManagerEvent::ListeningStarted => {
            v.status = VoiceUiStatus::Listening;
        }
        VoiceManagerEvent::ListeningStopped => {
            v.status = VoiceUiStatus::Idle;
        }
        VoiceManagerEvent::TranscribingStarted => {
            v.status = VoiceUiStatus::Transcribing;
        }
        VoiceManagerEvent::PartialTranscript(_) => {
            // Reserved for V5+ — drop for now.
        }
        VoiceManagerEvent::FinalTranscript(text) => {
            v.status = VoiceUiStatus::Idle;
            insert_transcript_into_input(app, &text);
        }
        VoiceManagerEvent::Error(message) => {
            v.status = VoiceUiStatus::Error(message.clone());
            app.push_msg(ChatMessage::Error(format!(
                "voice sidecar error: {}",
                message
            )));
        }
        VoiceManagerEvent::Exited => {
            app.push_msg(ChatMessage::System("voice sidecar exited".to_string()));
            app.voice = None;
        }
    }
}

/// Insert a transcript at the current cursor position with sensible
/// whitespace handling. Pure function over `App` so it's unit-testable
/// without any sidecar plumbing.
pub(crate) fn insert_transcript_into_input(app: &mut App, transcript: &str) {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return;
    }
    let needs_leading_space = !app.input.is_empty()
        && app.cursor_byte_pos() > 0
        && !app
            .input
            .as_bytes()
            .get(app.cursor_byte_pos().saturating_sub(1))
            .copied()
            .map(|b| (b as char).is_whitespace())
            .unwrap_or(true);
    let to_insert = if needs_leading_space {
        format!(" {}", trimmed)
    } else {
        trimmed.to_string()
    };
    let byte_pos = app.cursor_byte_pos();
    app.input.insert_str(byte_pos, &to_insert);
    app.cursor_pos += to_insert.chars().count();
    app.invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::Session;

    fn fresh_app() -> App {
        App::new(Session::new("test", "medium", None))
    }

    #[test]
    fn insert_transcript_into_empty_input() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "hello world");
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor_pos, "hello world".chars().count());
    }

    #[test]
    fn insert_transcript_appends_with_leading_space() {
        let mut app = fresh_app();
        app.input = "first".to_string();
        app.cursor_pos = "first".chars().count();
        insert_transcript_into_input(&mut app, "second sentence");
        assert_eq!(app.input, "first second sentence");
        assert_eq!(app.cursor_pos, "first second sentence".chars().count());
    }

    #[test]
    fn insert_transcript_no_double_space_when_input_ends_with_space() {
        let mut app = fresh_app();
        app.input = "first ".to_string();
        app.cursor_pos = "first ".chars().count();
        insert_transcript_into_input(&mut app, "second");
        assert_eq!(app.input, "first second");
    }

    #[test]
    fn insert_transcript_trims_whitespace_from_payload() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "  spaced text  ");
        assert_eq!(app.input, "spaced text");
    }

    #[test]
    fn insert_transcript_ignores_empty_or_whitespace_only() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "");
        insert_transcript_into_input(&mut app, "   ");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn insert_transcript_inserts_at_cursor_not_end() {
        let mut app = fresh_app();
        app.input = "hello world".to_string();
        // Place cursor between "hello" and " world" (after "hello")
        app.cursor_pos = 5;
        insert_transcript_into_input(&mut app, "beautiful");
        assert_eq!(app.input, "hello beautiful world");
    }
}
