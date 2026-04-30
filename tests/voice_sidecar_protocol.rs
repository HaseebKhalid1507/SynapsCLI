use synaps_cli::voice::sidecar_protocol::{
    SidecarCapability, SidecarCommand, SidecarConfig, SidecarEvent, SidecarProviderState,
    VoiceSidecarMode, VOICE_SIDECAR_PROTOCOL_VERSION,
};

#[test]
fn voice_sidecar_protocol_round_trips_core_commands() {
    let command = SidecarCommand::Init {
        config: SidecarConfig {
            mode: VoiceSidecarMode::Dictation,
            language: Some("en".to_string()),
            protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        },
    };

    let json = serde_json::to_string(&command).unwrap();
    assert_eq!(
        json,
        r#"{"type":"init","config":{"mode":"dictation","language":"en","protocol_version":1}}"#
    );
    let decoded: SidecarCommand = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, command);
}

#[test]
fn voice_sidecar_protocol_round_trips_sidecar_events() {
    let event = SidecarEvent::Hello {
        protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        extension: "synaps-voice-local".to_string(),
        capabilities: vec![SidecarCapability::Stt],
    };

    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(
        json,
        r#"{"type":"hello","protocol_version":1,"extension":"synaps-voice-local","capabilities":["stt"]}"#
    );
    let decoded: SidecarEvent = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn voice_sidecar_status_event_carries_state_and_capabilities() {
    let event = SidecarEvent::Status {
        state: SidecarProviderState::Ready,
        capabilities: vec![SidecarCapability::Stt, SidecarCapability::BargeIn],
    };

    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(
        json,
        r#"{"type":"status","state":"ready","capabilities":["stt","barge_in"]}"#
    );
    let decoded: SidecarEvent = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, event);
}
