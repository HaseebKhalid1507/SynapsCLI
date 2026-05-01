//! Sidecar protocol types — line-JSON over stdio.
//!
//! These are mirrored from `synaps-skills/local-voice-plugin/src/protocol.rs`
//! so Synaps CLI core can talk to any compliant voice sidecar without
//! depending on heavyweight audio/whisper crates.
//!
//! Wire format: one JSON object per line on the sidecar's stdin/stdout.
//! Synaps CLI sends `SidecarCommand` values; the sidecar replies with
//! `SidecarEvent` values (interleaved with internally-generated events).

use serde::{Deserialize, Serialize};

/// Protocol version understood by this build.
pub const VOICE_SIDECAR_PROTOCOL_VERSION: u16 = 1;

/// Modes a sidecar may operate in. Toggle dictation is the only one we
/// ship today; the others reserve namespace for future work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSidecarMode {
    Dictation,
    Command,
    Conversation,
}

/// Configuration sent in the `Init` handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarConfig {
    pub mode: VoiceSidecarMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub protocol_version: u16,
}

/// Commands sent from Synaps CLI to the sidecar (one per line, JSON).
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

/// Events emitted by the sidecar (one per line, JSON).
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
    PartialTranscript {
        text: String,
    },
    FinalTranscript {
        text: String,
    },
    VoiceCommand {
        command: String,
    },
    BargeIn,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trip_init() {
        let cmd = SidecarCommand::Init {
            config: SidecarConfig {
                mode: VoiceSidecarMode::Dictation,
                language: Some("en".into()),
                protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"init\""));
        assert!(json.contains("\"mode\":\"dictation\""));
        let parsed: SidecarCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn command_round_trip_press_release_shutdown() {
        for cmd in [
            SidecarCommand::VoiceControlPressed,
            SidecarCommand::VoiceControlReleased,
            SidecarCommand::Shutdown,
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            let parsed: SidecarCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn event_round_trip_hello() {
        let event = SidecarEvent::Hello {
            protocol_version: 1,
            extension: "synaps-voice-plugin".to_string(),
            capabilities: vec![SidecarCapability::Stt],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"hello\""));
        let parsed: SidecarEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn event_round_trip_transcripts() {
        for event in [
            SidecarEvent::ListeningStarted,
            SidecarEvent::TranscribingStarted,
            SidecarEvent::PartialTranscript {
                text: "hello".into(),
            },
            SidecarEvent::FinalTranscript {
                text: "hello world".into(),
            },
            SidecarEvent::Error {
                message: "model missing".into(),
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: SidecarEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn parses_real_plugin_hello_line() {
        // Wire format example from local-voice-plugin/src/main.rs::emit_ready.
        let line = r#"{"type":"hello","protocol_version":1,"extension":"synaps-voice-plugin","capabilities":["stt"]}"#;
        let parsed: SidecarEvent = serde_json::from_str(line).unwrap();
        match parsed {
            SidecarEvent::Hello {
                protocol_version,
                extension,
                capabilities,
            } => {
                assert_eq!(protocol_version, 1);
                assert_eq!(extension, "synaps-voice-plugin");
                assert_eq!(capabilities, vec![SidecarCapability::Stt]);
            }
            other => panic!("expected Hello, got {:?}", other),
        }
    }

    #[test]
    fn parses_real_plugin_final_transcript_line() {
        let line = r#"{"type":"final_transcript","text":"hello from the plugin"}"#;
        let parsed: SidecarEvent = serde_json::from_str(line).unwrap();
        assert_eq!(
            parsed,
            SidecarEvent::FinalTranscript {
                text: "hello from the plugin".into()
            }
        );
    }
}
