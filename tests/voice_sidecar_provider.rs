use synaps_cli::voice::sidecar_protocol::SidecarEvent;
use synaps_cli::voice::sidecar_provider::{sidecar_event_to_voice_event, voice_event_to_sidecar_event};
use synaps_cli::VoiceEvent;

#[test]
fn sidecar_final_transcript_maps_to_internal_voice_event() {
    let mapped = sidecar_event_to_voice_event(SidecarEvent::FinalTranscript {
        text: "open Cargo.toml".to_string(),
    });

    assert_eq!(mapped, Some(VoiceEvent::FinalTranscript("open Cargo.toml".to_string())));
}

#[test]
fn sidecar_status_events_do_not_enter_transcript_pipeline() {
    let mapped = sidecar_event_to_voice_event(SidecarEvent::Hello {
        protocol_version: 1,
        extension: "synaps-voice-mock".to_string(),
        capabilities: vec![],
    });

    assert_eq!(mapped, None);
}

#[test]
fn internal_voice_event_maps_back_to_sidecar_event_for_local_sidecar() {
    let mapped = voice_event_to_sidecar_event(VoiceEvent::FinalTranscript("hello".to_string()));

    assert_eq!(mapped, Some(SidecarEvent::FinalTranscript { text: "hello".to_string() }));
}

