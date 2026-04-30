use serde::{Deserialize, Serialize};

pub const VOICE_SIDECAR_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSidecarMode {
    Dictation,
    Command,
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarConfig {
    pub mode: VoiceSidecarMode,
    pub language: Option<String>,
    pub protocol_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    Init { config: SidecarConfig },
    VoiceControlPressed,
    VoiceControlReleased,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarCapability {
    Stt,
    BargeIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarProviderState {
    Ready,
    Listening,
    Transcribing,
    Speaking,
    Stopped,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarEvent {
    Hello {
        protocol_version: u16,
        extension: String,
        capabilities: Vec<SidecarCapability>,
    },
    Status {
        state: SidecarProviderState,
        capabilities: Vec<SidecarCapability>,
    },
    ListeningStarted,
    ListeningStopped,
    TranscribingStarted,
    PartialTranscript { text: String },
    FinalTranscript { text: String },
    VoiceCommand { command: String },
    BargeIn,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_sidecar_protocol_round_trips_shutdown_command() {
        let command = SidecarCommand::Shutdown;
        let json = serde_json::to_string(&command).unwrap();

        assert_eq!(json, r#"{"type":"shutdown"}"#);
        assert_eq!(serde_json::from_str::<SidecarCommand>(&json).unwrap(), command);
    }

    #[test]
    fn voice_sidecar_protocol_round_trips_final_transcript_event() {
        let event = SidecarEvent::FinalTranscript {
            text: "open Cargo.toml".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();

        assert_eq!(json, r#"{"type":"final_transcript","text":"open Cargo.toml"}"#);
        assert_eq!(serde_json::from_str::<SidecarEvent>(&json).unwrap(), event);
    }
}
