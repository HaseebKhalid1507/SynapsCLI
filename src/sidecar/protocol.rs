//! Sidecar protocol types — line-JSON over stdio.
//!
//! These mirror `synaps-skills/local-voice-plugin/src/protocol.rs` so
//! Synaps CLI core can talk to any compliant sidecar without depending
//! on heavyweight modality-specific crates. The protocol is shared
//! across all sidecar kinds (voice, OCR, agent, etc.) — every kind
//! transports the same handshake / state / payload envelopes.
//!
//! Wire format: one JSON object per line on the sidecar's stdin/stdout.
//! Synaps CLI sends [`SidecarCommand`] values; the sidecar replies with
//! [`SidecarEvent`] values (interleaved with internally-generated
//! events).
//!
//! ## Naming note
//!
//! Several event/command variants still carry voice-shaped wire names
//! (`final_transcript`, `voice_control_pressed`, `voice_command`, the
//! `dictation`/`command`/`conversation` mode values). Those are
//! historical and shared with the plugin; a coordinated cross-repo
//! migration to modality-neutral names (`payload_final`, `trigger_pressed`,
//! `intent`, free-form session strings) is planned for a future phase
//! but is **out of scope for Phase 7**. The Rust enum names are
//! modality-neutral; serde aliases preserve wire compatibility.

use serde::{Deserialize, Serialize};

/// Protocol version understood by this build.
pub const SIDECAR_PROTOCOL_VERSION: u16 = 1;

/// Backwards-compat alias for the protocol version constant. New code
/// should reference [`SIDECAR_PROTOCOL_VERSION`] directly.
#[deprecated(
    since = "0.1.0-phase7",
    note = "use SIDECAR_PROTOCOL_VERSION; the voice-prefixed alias will be removed in a future release"
)]
pub const VOICE_SIDECAR_PROTOCOL_VERSION: u16 = SIDECAR_PROTOCOL_VERSION;

/// Session mode the sidecar should run in.
///
/// The variant *values* are wire-format strings shared with the plugin
/// (`dictation`, `command`, `conversation`). Callers that need a mode
/// not enumerated here should be served by a future free-form-string
/// variant; for now these three cover every kind we ship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarSessionMode {
    Dictation,
    Command,
    Conversation,
}

/// Configuration sent in the [`SidecarCommand::Init`] handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarConfig {
    pub mode: SidecarSessionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub protocol_version: u16,
}

/// Commands sent from Synaps CLI to the sidecar (one per line, JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    Init {
        config: SidecarConfig,
    },
    /// User pressed the activation trigger (key, button, gesture, …).
    /// Wire name kept as `voice_control_pressed` for plugin compat.
    #[serde(rename = "voice_control_pressed")]
    TriggerPressed,
    /// User released the activation trigger.
    /// Wire name kept as `voice_control_released` for plugin compat.
    #[serde(rename = "voice_control_released")]
    TriggerReleased,
    Shutdown,
}

/// Free-form capability tag advertised by a sidecar in its `Hello`
/// frame. Values are plugin-defined strings; core does not enumerate.
///
/// (Pre-Phase-7 this was a closed enum of `stt`/`barge_in`. Kept as an
/// enum-shape with two known variants plus a catch-all for forward
/// compatibility.)
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
///
/// Several variants (`ListeningStarted`, `FinalTranscript`,
/// `VoiceCommand`) carry voice-shaped wire names for backward
/// compatibility with the local-voice plugin. A coordinated cross-repo
/// migration to neutral names (`StateOpen`, `PayloadFinal`, `Intent`)
/// is deferred to a future phase.
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
                mode: SidecarSessionMode::Dictation,
                language: Some("en".into()),
                protocol_version: SIDECAR_PROTOCOL_VERSION,
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
            SidecarCommand::TriggerPressed,
            SidecarCommand::TriggerReleased,
            SidecarCommand::Shutdown,
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            let parsed: SidecarCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn trigger_commands_use_legacy_voice_wire_names() {
        // Wire-format compat: existing local-voice plugin reads
        // "voice_control_pressed"/"voice_control_released". The Rust
        // type names are neutral but the JSON stays bit-identical so
        // we don't break the plugin.
        let pressed = serde_json::to_string(&SidecarCommand::TriggerPressed).unwrap();
        let released = serde_json::to_string(&SidecarCommand::TriggerReleased).unwrap();
        assert!(pressed.contains("\"voice_control_pressed\""), "got {pressed}");
        assert!(released.contains("\"voice_control_released\""), "got {released}");
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
