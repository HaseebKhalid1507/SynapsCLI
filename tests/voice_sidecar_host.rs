use synaps_cli::voice::sidecar_protocol::{
    SidecarCommand, SidecarConfig, SidecarEvent, SidecarProviderState, VoiceSidecarMode,
    VOICE_SIDECAR_PROTOCOL_VERSION,
};
use synaps_cli::voice::sidecar_host::VoiceSidecarHost;

#[tokio::test]
async fn voice_sidecar_host_spawns_mock_and_receives_transcript_events() {
    let mock = env!("CARGO_BIN_EXE_synaps-voice-mock");
    let mut host = VoiceSidecarHost::spawn(mock, &["--transcript".into(), "host transcript".into()])
        .await
        .unwrap();

    host.send(SidecarCommand::Init {
        config: SidecarConfig {
            mode: VoiceSidecarMode::Dictation,
            language: Some("en".to_string()),
            protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        },
    })
    .await
    .unwrap();

    assert!(matches!(host.recv().await.unwrap(), SidecarEvent::Hello { .. }));
    assert_eq!(
        host.recv().await.unwrap(),
        SidecarEvent::Status {
            state: SidecarProviderState::Ready,
            capabilities: vec![synaps_cli::voice::sidecar_protocol::SidecarCapability::Stt],
        }
    );

    host.send(SidecarCommand::VoiceControlPressed).await.unwrap();
    host.send(SidecarCommand::VoiceControlReleased).await.unwrap();

    assert_eq!(host.recv().await.unwrap(), SidecarEvent::ListeningStarted);
    assert_eq!(host.recv().await.unwrap(), SidecarEvent::ListeningStopped);
    assert_eq!(host.recv().await.unwrap(), SidecarEvent::TranscribingStarted);
    assert_eq!(
        host.recv().await.unwrap(),
        SidecarEvent::FinalTranscript {
            text: "host transcript".to_string(),
        }
    );

    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn voice_sidecar_host_reports_spawn_errors() {
    let err = VoiceSidecarHost::spawn("/definitely/missing/synaps-voice-sidecar", &[])
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("failed to spawn voice sidecar"));
}
