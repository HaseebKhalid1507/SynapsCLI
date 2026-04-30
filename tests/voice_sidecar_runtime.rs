use std::path::PathBuf;

use synaps_cli::voice::sidecar_provider::{sidecar_args_from_config, SidecarSttProvider};
use synaps_cli::{SpeechToTextProvider, VoiceConfig, VoiceEvent};

#[tokio::test]
async fn sidecar_stt_provider_emits_mock_transcript_through_voice_events() {
    let mock = env!("CARGO_BIN_EXE_synaps-voice-mock");
    let mut provider = SidecarSttProvider::new(
        mock,
        vec!["--transcript".to_string(), "provider transcript".to_string()],
        "en",
        1,
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    provider.start(tx).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    provider.stop().unwrap();

    let mut seen = Vec::new();
    for _ in 0..4 {
        if let Ok(Some(event)) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
            seen.push(event);
        }
    }

    assert!(seen.contains(&VoiceEvent::ListeningStarted), "seen events: {seen:?}");
    assert!(seen.contains(&VoiceEvent::ListeningStopped), "seen events: {seen:?}");
    assert!(
        seen.contains(&VoiceEvent::FinalTranscript("provider transcript".to_string())),
        "seen events: {seen:?}"
    );
}

#[tokio::test]
async fn sidecar_stt_provider_surfaces_missing_command_as_voice_error() {
    let mut provider = SidecarSttProvider::new(
        "/definitely/missing/synaps-voice-local",
        Vec::new(),
        "en",
        1,
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);

    provider.start(tx).unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();

    match event {
        VoiceEvent::Error(message) => assert!(message.contains("failed to spawn voice sidecar"), "{message}"),
        other => panic!("expected voice error, got {other:?}"),
    }
}

#[test]
fn sidecar_args_from_config_passes_model_path_and_language_to_local_sidecar() {
    let config = VoiceConfig {
        provider: "sidecar".to_string(),
        sidecar_command: "synaps-voice-local".to_string(),
        sidecar_args: Vec::new(),
        stt_model_path: PathBuf::from("/models/ggml-tiny.en.bin"),
        stt_language: "en".to_string(),
        ..VoiceConfig::default()
    };

    assert_eq!(
        sidecar_args_from_config(&config),
        vec![
            "--model-path".to_string(),
            "/models/ggml-tiny.en.bin".to_string(),
            "--language".to_string(),
            "en".to_string(),
        ]
    );
}

#[test]
fn sidecar_args_from_config_preserves_explicit_sidecar_args() {
    let config = VoiceConfig {
        sidecar_command: "custom-voice".to_string(),
        sidecar_args: vec!["--custom".to_string(), "value".to_string()],
        stt_model_path: PathBuf::from("/models/ignored.bin"),
        stt_language: "en".to_string(),
        ..VoiceConfig::default()
    };

    assert_eq!(
        sidecar_args_from_config(&config),
        vec!["--custom".to_string(), "value".to_string()]
    );
}
