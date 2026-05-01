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

        let manager = VoiceManager::spawn(
            &sidecar.binary,
            &[],
            SidecarConfig {
                mode: VoiceSidecarMode::Dictation,
                language: None,
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
/// V4 only updates the status indicator and surfaces transcripts as
/// system messages — V5 will route final transcripts directly into
/// the input buffer.
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
            // Reserved for V5 — drop for now.
        }
        VoiceManagerEvent::FinalTranscript(text) => {
            // V5 will insert into the input buffer; V4 just shows it.
            v.status = VoiceUiStatus::Idle;
            app.push_msg(ChatMessage::System(format!("🎤 transcript: {}", text)));
        }
        VoiceManagerEvent::Error(message) => {
            v.status = VoiceUiStatus::Error(message.clone());
            app.push_msg(ChatMessage::Error(format!("voice sidecar error: {}", message)));
        }
        VoiceManagerEvent::Exited => {
            app.push_msg(ChatMessage::System("voice sidecar exited".to_string()));
            app.voice = None;
        }
    }
}
